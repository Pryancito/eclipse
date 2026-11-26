//! A device node in `/dev` that this process serves (`eclipse_dev_serve`):
//! what a program does to the node -- open it, read, write, `ioctl`, close
//! -- comes here as a message on a FIFO, and what [`Device`] answers is
//! what the program is told. The kernel's side is
//! `linux-object/src/fs/devfs/served.rs`, with the same messages.
//!
//! A read never waits here: with nothing to give, [`Device::read`] answers
//! `EAGAIN`, and the program waits in the kernel until this process says the
//! open is readable ([`Node::ready`]). A device says that from wherever its
//! data comes, another thread included, and before it answers the request
//! that changed it: the kernel reads the FIFO in order.
//!
//! An `ioctl` whose argument points further (a structure holding the
//! address of more) asks the kernel for that memory ([`Device::fetch`]), and
//! is answered with it ([`Device::ioctl_iov`]). One that cannot be done yet
//! answers `EAGAIN`; on an open that waits, the kernel asks again once this
//! process has said something of the open.

use core::mem::size_of;

use crate::fifo::{Fifo, Message};
use crate::zx::{self, nr, Status};

/// A message of the FIFO: `op, reqid, fh, len`, `a`, and `b` in a request
/// or the status in a reply.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Msg {
    pub op: u32,
    pub reqid: u32,
    pub fh: u32,
    pub len: u32,
    pub a: u64,
    pub b: u64,
}

// SAFETY: `#[repr(C)]`, four `u32`s then two `u64`s, no padding; any bits
// are a value.
unsafe impl Message for Msg {}

const _: () = assert!(size_of::<Msg>() == 32);

pub const OP_OPEN: u32 = 1;
pub const OP_RELEASE: u32 = 2;
pub const OP_READ: u32 = 3;
pub const OP_WRITE: u32 = 4;
pub const OP_IOCTL: u32 = 5;
pub const OP_IOCTL_IOV: u32 = 6;
/// `mmap` of an open: answered with a VMO handle of this process's own
/// table, and where the window starts in that VMO ([`Device::mmap`]).
pub const OP_MMAP: u32 = 7;
pub const OP_READY: u32 = 16;

/// In an `ioctl`'s reply, in `a`: the slot holds an [`Iov`], what more of
/// the program's memory it needs.
pub const IOCTL_FETCH: u64 = 1;
/// In an `ioctl`'s request, above the command in `a`: the open waits, and
/// an `EAGAIN` is asked again once the node says something of it.
pub const IOCTL_WAITS: u64 = 1 << 32;
/// How many ranges an [`Iov`] names, at most, each way.
pub const IOV_MAX: usize = 8;

pub const POLLIN: u64 = 0x1;
pub const POLLOUT: u64 = 0x4;
pub const POLLERR: u64 = 0x8;
pub const POLLHUP: u64 = 0x10;

/// A slot of the window, and how many there are.
pub const SLOT_LEN: usize = 64 * 1024;
pub const SLOTS: usize = 16;

/// The `errno`s a device answers with.
pub const EAGAIN: i32 = 11;
pub const EFAULT: i32 = 14;
pub const EBUSY: i32 = 16;
pub const EINVAL: i32 = 22;
pub const ENOTTY: i32 = 25;
pub const EIO: i32 = 5;
pub const ENODEV: i32 = 19;

/// In [`Device::mmap`]'s `how`: the program maps `MAP_SHARED`.
pub const MMAP_SHARED: u32 = 1;
/// In [`Device::mmap`]'s `how`: the program asked for `PROT_WRITE`.
pub const MMAP_WRITE: u32 = 2;

/// The ranges of the program's memory an `ioctl` reads (`ins`) and writes
/// (`outs`): address and length. Laid out in the slot as the kernel's
/// `wire::Iov`: the two counts (`u32`), then the ranges (`u64` pairs),
/// those to read first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Iov {
    pub ins: Vec<(u64, u64)>,
    pub outs: Vec<(u64, u64)>,
}

impl Iov {
    pub fn len_of(ins: usize, outs: usize) -> usize {
        8 + 16 * (ins + outs)
    }

    /// How many bytes it reads, and writes.
    pub fn totals(&self) -> (usize, usize) {
        let sum = |r: &[(u64, u64)]| r.iter().map(|(_, l)| *l as usize).sum();
        (sum(&self.ins), sum(&self.outs))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::len_of(self.ins.len(), self.outs.len()));
        b.extend_from_slice(&(self.ins.len() as u32).to_ne_bytes());
        b.extend_from_slice(&(self.outs.len() as u32).to_ne_bytes());
        for (addr, len) in self.ins.iter().chain(self.outs.iter()) {
            b.extend_from_slice(&addr.to_ne_bytes());
            b.extend_from_slice(&len.to_ne_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Iov> {
        let u32_at =
            |i: usize| Some(u32::from_ne_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize);
        let u64_at = |i: usize| Some(u64::from_ne_bytes(b.get(i..i + 8)?.try_into().ok()?));
        let (ins, outs) = (u32_at(0)?, u32_at(4)?);
        if ins > IOV_MAX || outs > IOV_MAX {
            return None;
        }
        let mut ranges = Vec::with_capacity(ins + outs);
        for i in 0..ins + outs {
            ranges.push((u64_at(8 + 16 * i)?, u64_at(16 + 16 * i)?));
        }
        let outs = ranges.split_off(ins);
        Some(Iov { ins: ranges, outs })
    }

    /// Whether the kernel can carry it: what it reads, after the ranges
    /// themselves, and what it writes each fit a slot.
    pub fn fits(&self) -> bool {
        let (i, o) = self.totals();
        self.ins.len() <= IOV_MAX
            && self.outs.len() <= IOV_MAX
            && i + Self::len_of(self.ins.len(), self.outs.len()) <= SLOT_LEN
            && o <= SLOT_LEN
    }
}

/// What an `ioctl` command's number says it carries in and out (the
/// generic encoding: direction in bits 30-31, size in bits 16-29).
pub fn ioctl_carries(cmd: u32) -> (usize, usize) {
    let size = ((cmd >> 16) & 0x3fff) as usize;
    let dir = cmd >> 30;
    (
        if dir & 1 != 0 { size } else { 0 },
        if dir & 2 != 0 { size } else { 0 },
    )
}

/// What a node does. Each method answers a request; an `Err` is an
/// `errno`.
pub trait Device {
    /// A new open, with its flags (`O_ACCMODE`): the name it goes by, and
    /// its readiness ([`POLLIN`] and the rest).
    fn open(&mut self, flags: u64) -> Result<(u32, u64), i32>;
    /// The open is closed.
    fn release(&mut self, fh: u32);
    /// Up to `buf.len()` bytes, or `EAGAIN`.
    fn read(&mut self, node: &Node, fh: u32, offset: u64, buf: &mut [u8]) -> Result<usize, i32>;
    /// How many of `data` were taken, or `EAGAIN`.
    fn write(&mut self, node: &Node, fh: u32, offset: u64, data: &[u8]) -> Result<usize, i32>;
    /// `ioctl(cmd, arg)`. `data` holds what the command carries in, and
    /// is where what it carries out goes: the call's return value and how
    /// many bytes of `data` go back.
    fn ioctl(
        &mut self,
        node: &Node,
        fh: u32,
        cmd: u32,
        arg: u64,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        let _ = (node, fh, cmd, arg, data);
        Err(ENOTTY)
    }
    /// Whether `cmd`, which says it carries something in, takes `arg` by
    /// value instead (evdev's `EVIOCGRAB` says `int` and means one). The
    /// kernel sends such a command with nothing in the slot when `arg` is
    /// not memory it can read; every other command is then answered
    /// `EFAULT` without reaching [`Device::ioctl`].
    fn by_value(&self, cmd: u32) -> bool {
        let _ = cmd;
        false
    }
    /// Asked before [`Device::ioctl`], with what `cmd` carried in: what
    /// more of the program's memory it needs, when its argument points
    /// further. `Some` has the kernel read the [`Iov`]'s ranges and ask
    /// again ([`Device::ioctl_iov`]); `None` goes on to [`Device::ioctl`].
    /// An `Err` answers the call.
    /// `waits` is whether the open waits ([`IOCTL_WAITS`]).
    fn fetch(
        &mut self,
        fh: u32,
        cmd: u32,
        arg: u64,
        waits: bool,
        data: &[u8],
    ) -> Result<Option<Iov>, i32> {
        let _ = (fh, cmd, arg, waits, data);
        Ok(None)
    }
    /// `mmap(offset, len)` of the open, `how` it is mapped ([`MMAP_SHARED`],
    /// [`MMAP_WRITE`]): a handle in this process's table to a VMO that holds
    /// the window, and where the window starts in it (whole pages). The
    /// handle stays this process's: keep it while the device lasts, since
    /// the kernel looks it up as it answers. Its rights are the mapping's
    /// ceiling: `MAP` and `READ` are needed, and a handle without `WRITE`
    /// is mapped read-only. `ENODEV`, the default, for a device that maps
    /// nothing.
    fn mmap(
        &mut self,
        node: &Node,
        fh: u32,
        offset: u64,
        len: u64,
        how: u32,
    ) -> Result<(u32, u64), i32> {
        let _ = (node, fh, offset, len, how);
        Err(ENODEV)
    }
    /// `ioctl(cmd, arg)` with what [`Device::fetch`] asked for: `ins` is
    /// the bytes of its ranges to read, one after the other, and what goes
    /// in `out` is written to its ranges to write, in order. The call's
    /// return value and how many bytes of `out` go back.
    fn ioctl_iov(
        &mut self,
        node: &Node,
        fh: u32,
        cmd: u32,
        arg: u64,
        waits: bool,
        iov: &Iov,
        ins: &[u8],
        out: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        let _ = (node, fh, cmd, arg, waits, iov, ins, out);
        Err(EINVAL)
    }
}

std::thread_local! {
    /// The request this thread is answering whose program's memory can be
    /// reached ([`caller_read`]): the FIFO it came on, and its id.
    static ASKER: core::cell::Cell<Option<(zx::Handle, u32)>> =
        const { core::cell::Cell::new(None) };
}

/// Sets [`ASKER`] while an `ioctl` is answered, and empties it after.
struct Asking;

impl Asking {
    fn new(fifo: zx::Handle, reqid: u32) -> Asking {
        ASKER.with(|a| a.set(Some((fifo, reqid))));
        Asking
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        ASKER.with(|a| a.set(None));
    }
}

/// Copy `buf.len()` bytes of the memory of the program whose `ioctl` this
/// thread is answering, from its `addr`: how many were copied, fewer where
/// a page on the way is one the program could not read itself. Only from
/// inside [`Device::ioctl`] or [`Device::ioctl_iov`]: anywhere else, or once
/// the answer is gone, it is [`Status::NOT_FOUND`]. For a command whose
/// argument holds pointers, to follow them as the driver goes.
pub fn caller_read(addr: u64, buf: &mut [u8]) -> Result<usize, Status> {
    caller_memory(addr, buf.as_mut_ptr() as usize, buf.len(), 0)
}

/// [`caller_read`]'s other way: `data` into the program's memory at `addr`.
pub fn caller_write(addr: u64, data: &[u8]) -> Result<usize, Status> {
    caller_memory(addr, data.as_ptr() as usize, data.len(), 1)
}

fn caller_memory(addr: u64, buf: usize, len: usize, flags: usize) -> Result<usize, Status> {
    let (fifo, reqid) = ASKER.with(|a| a.get()).ok_or(Status::NOT_FOUND)?;
    if len == 0 {
        return Ok(0);
    }
    // SAFETY: the kernel reads, or writes, `len` bytes at `buf`, which the
    // caller lends for the call.
    unsafe {
        zx::call(
            nr::ECLIPSE_DEV_MEMORY,
            [
                fifo as usize,
                reqid as usize,
                addr as usize,
                buf,
                len,
                flags,
            ],
        )
    }
}

/// A node this process serves.
pub struct Node {
    fifo: Fifo<Msg>,
    window: *mut u8,
    /// While [`Node::run`] answers a batch: the readiness said meanwhile,
    /// which goes out with the replies, ahead of the one it came before.
    held: std::sync::Mutex<Option<Vec<Msg>>>,
}

// SAFETY: the window is memory shared with the kernel, and a slot is only
// touched by whoever answers its request; `ready` touches no slot.
unsafe impl Send for Node {}
unsafe impl Sync for Node {}

impl Node {
    /// Put `/dev/<name>` there, a character device `major:minor` with
    /// permissions `mode`, answered by this process.
    pub fn serve(name: &str, major: u32, minor: u32, mode: u32) -> Result<Node, String> {
        Node::serve_status(name, major, minor, mode).map_err(|(_, e)| e)
    }

    /// [`Node::serve`], with the kernel's status on a refusal:
    /// [`Status::ALREADY_EXISTS`] when the name is someone else's (the
    /// kernel's own driver of another card, or another server).
    pub fn serve_status(
        name: &str,
        major: u32,
        minor: u32,
        mode: u32,
    ) -> Result<Node, (Option<crate::zx::Status>, String)> {
        let (fifo, theirs) =
            Fifo::<Msg>::create(SLOTS * 4).map_err(|e| (None, format!("fifo_create: {e}")))?;
        let (window, vmo) = zx::shared_memory(SLOTS * SLOT_LEN).map_err(|e| (None, e))?;
        // SAFETY: the kernel reads `name.len()` bytes of `name`; both
        // handles go to it.
        unsafe {
            zx::call(
                nr::ECLIPSE_DEV_SERVE,
                [
                    theirs as usize,
                    vmo as usize,
                    name.as_ptr() as usize,
                    name.len(),
                    ((major as usize) << 32) | minor as usize,
                    mode as usize,
                ],
            )
        }
        .map_err(|e| (Some(e), format!("dev_serve /dev/{name}: {e}")))?;
        Ok(Node {
            fifo,
            window,
            held: std::sync::Mutex::new(None),
        })
    }

    /// Serve a DRM card (`ECLIPSE_DRM_SERVE`): the kernel's DRM core
    /// counts it with the others and puts a node in `/dev/dri` for it, and
    /// each call it makes of the card is a request here. `hello` is what
    /// the card says of itself (`zcore_drivers::scheme::drm_remote::hello`).
    pub fn serve_drm(hello: &[u8]) -> Result<Node, String> {
        let (fifo, theirs) =
            Fifo::<Msg>::create(SLOTS * 4).map_err(|e| format!("fifo_create: {e}"))?;
        let (window, vmo) = zx::shared_memory(SLOTS * SLOT_LEN)?;
        // SAFETY: the kernel reads `hello.len()` bytes of `hello`; both
        // handles go to it.
        unsafe {
            zx::call(
                nr::ECLIPSE_DRM_SERVE,
                [
                    theirs as usize,
                    vmo as usize,
                    hello.as_ptr() as usize,
                    hello.len(),
                    0,
                    0,
                ],
            )
        }
        .map_err(|e| format!("drm_serve: {e}"))?;
        Ok(Node {
            fifo,
            window,
            held: std::sync::Mutex::new(None),
        })
    }

    /// Serve the NVIDIA RM this process runs (`ECLIPSE_NVCTL_SERVE`): the
    /// kernel's `/dev/nvidiactl` and `/dev/nvidia<N>` pass what they ask of
    /// the RM here. `hello` is what the RM says of itself
    /// (`zcore_drivers::display::nvctl::remote::hello`).
    pub fn serve_nvctl(hello: &[u8]) -> Result<Node, (Status, String)> {
        let (fifo, theirs) = Fifo::<Msg>::create(SLOTS * 4)
            .map_err(|e| (Status::NO_MEMORY, format!("fifo_create: {e}")))?;
        let (window, vmo) =
            zx::shared_memory(SLOTS * SLOT_LEN).map_err(|e| (Status::NO_MEMORY, e))?;
        // SAFETY: the kernel reads `hello.len()` bytes of `hello`; both
        // handles go to it.
        unsafe {
            zx::call(
                nr::ECLIPSE_NVCTL_SERVE,
                [
                    theirs as usize,
                    vmo as usize,
                    hello.as_ptr() as usize,
                    hello.len(),
                    0,
                    0,
                ],
            )
        }
        .map_err(|e| (e, format!("nvctl_serve: {e}")))?;
        Ok(Node {
            fifo,
            window,
            held: std::sync::Mutex::new(None),
        })
    }

    /// While the value lives, [`caller_read`] and [`caller_write`] reach
    /// the memory of the program whose request `req` is: for an answer
    /// not made through [`Node::answer`] whose request lends it (the
    /// kernel says which do).
    pub fn reaching(&self, req: &Msg) -> impl Drop {
        Asking::new(self.fifo.handle(), req.reqid)
    }

    /// The bytes of request `req`'s slot: what it carries in, and where its
    /// answer goes out. Only while it is being answered.
    #[allow(clippy::mut_from_ref)]
    pub fn slot_of(&self, req: &Msg) -> &mut [u8] {
        self.slot(req.reqid)
    }

    /// Say that open `fh` is now as `mask` says. From any thread. Said
    /// while a batch is being answered, it goes out with the replies, in
    /// the same write: a message apart wakes the kernel for nothing but
    /// costs a turn of its scheduler, measured at 3.7 ms a request on
    /// QEMU against 0.3 ms batched.
    pub fn ready(&self, fh: u32, mask: u64) {
        let m = Msg {
            op: OP_READY,
            fh,
            a: mask,
            ..Msg::default()
        };
        if let Some(held) = self.held.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            held.push(m);
            return;
        }
        // A FIFO the kernel does not read is a kernel that is not there.
        let _ = self.fifo.write(&[m]);
    }

    // The window is the kernel's memory as much as this process's: a slot
    // is lent out by the request that names it, not by `&mut self`.
    #[allow(clippy::mut_from_ref)]
    fn slot(&self, reqid: u32) -> &mut [u8] {
        let at = (reqid as usize & (SLOTS - 1)) * SLOT_LEN;
        // SAFETY: the window is `SLOTS * SLOT_LEN` bytes, mapped for the
        // life of the process, and this slot is this request's until it is
        // answered.
        unsafe { core::slice::from_raw_parts_mut(self.window.add(at), SLOT_LEN) }
    }

    /// Answer one request: the reply, or none for a close.
    pub fn answer(&self, dev: &mut impl Device, req: &Msg) -> Option<Msg> {
        let mut reply = Msg {
            op: req.op,
            reqid: req.reqid,
            fh: req.fh,
            ..Msg::default()
        };
        let len = (req.len as usize).min(SLOT_LEN);
        let status: Result<i64, i32> = match req.op {
            OP_OPEN => dev.open(req.a).map(|(fh, mask)| {
                reply.fh = fh;
                reply.a = mask;
                0
            }),
            OP_RELEASE => {
                dev.release(req.fh);
                return None;
            }
            OP_READ => {
                let buf = &mut self.slot(req.reqid)[..len];
                dev.read(self, req.fh, req.a, buf).map(|n| {
                    let n = n.min(len);
                    reply.len = n as u32;
                    n as i64
                })
            }
            OP_WRITE => {
                let data = &self.slot(req.reqid)[..len];
                dev.write(self, req.fh, req.a, data).map(|n| n as i64)
            }
            OP_IOCTL => {
                let _asking = Asking::new(self.fifo.handle(), req.reqid);
                let cmd = req.a as u32;
                let (into, out) = ioctl_carries(cmd);
                let data = &mut self.slot(req.reqid)[..len.max(out)];
                if len < into && !dev.by_value(cmd) {
                    Err(EFAULT)
                } else {
                    let waits = req.a & IOCTL_WAITS != 0;
                    match dev.fetch(req.fh, cmd, req.b, waits, &data[..len.min(into)]) {
                        Err(e) => Err(e),
                        Ok(Some(iov)) if !iov.fits() => Err(EIO),
                        Ok(Some(iov)) => {
                            let b = iov.encode();
                            // Longer than what the command carries.
                            self.slot(req.reqid)[..b.len()].copy_from_slice(&b);
                            reply.len = b.len() as u32;
                            reply.a = IOCTL_FETCH;
                            Ok(0)
                        }
                        Ok(None) => dev.ioctl(self, req.fh, cmd, req.b, data).map(|(ret, n)| {
                            reply.len = n.min(out) as u32;
                            ret
                        }),
                    }
                }
            }
            OP_IOCTL_IOV => {
                let _asking = Asking::new(self.fifo.handle(), req.reqid);
                let cmd = req.a as u32;
                let slot = self.slot(req.reqid);
                match Iov::decode(&slot[..len]) {
                    None => Err(EIO),
                    Some(iov) => {
                        let (into, out) = iov.totals();
                        let at = Iov::len_of(iov.ins.len(), iov.outs.len());
                        if at + into > len || out > SLOT_LEN {
                            Err(EIO)
                        } else {
                            let ins = slot[at..at + into].to_vec();
                            let back = &mut slot[..out];
                            let waits = req.a & IOCTL_WAITS != 0;
                            dev.ioctl_iov(self, req.fh, cmd, req.b, waits, &iov, &ins, back)
                                .map(|(ret, n)| {
                                    reply.len = n.min(out) as u32;
                                    ret
                                })
                        }
                    }
                }
            }
            OP_MMAP => dev
                .mmap(self, req.fh, req.a, req.b, req.len)
                .map(|(handle, at)| {
                    reply.a = handle as u64;
                    if at != 0 {
                        self.slot(req.reqid)[..8].copy_from_slice(&at.to_ne_bytes());
                        reply.len = 8;
                    }
                    0
                }),
            _ => Err(EINVAL),
        };
        reply.b = match status {
            Ok(v) => v as u64,
            Err(e) => (-(e as i64)) as u64,
        };
        Some(reply)
    }

    /// Answer requests until the kernel lets go of the node: a batch at a
    /// time, the replies of one going out in the same call that waits for
    /// the next. `lock` is where the device is, shared with whoever else
    /// says it is ready.
    pub fn run<D: Device>(&self, lock: &std::sync::Mutex<D>) -> Status {
        self.run_with(lock, |dev, node, req| node.answer(dev, req))
    }

    /// [`Node::run`] for a node that is one of several over the same
    /// device: `answer` answers a request with what is behind `lock`
    /// ([`Node::answer`] on the part of it this node is).
    pub fn run_with<T>(
        &self,
        lock: &std::sync::Mutex<T>,
        mut answer: impl FnMut(&mut T, &Node, &Msg) -> Option<Msg>,
    ) -> Status {
        let mut buf = [Msg::default(); SLOTS * 4];
        let mut replies = 0;
        let mut out: Vec<Msg> = Vec::with_capacity(SLOTS * 4);
        loop {
            let got = match self.fifo.reply_wait(&mut buf[..], replies) {
                Ok(n) => n,
                Err(e) => return e,
            };
            let mut dev = lock.lock().unwrap_or_else(|p| p.into_inner());
            *self.held.lock().unwrap_or_else(|p| p.into_inner()) = Some(Vec::new());
            out.clear();
            for req in buf.iter().take(got) {
                let reply = answer(&mut *dev, self, req);
                if let Some(held) = self.held.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                    out.append(held);
                }
                out.extend(reply);
            }
            *self.held.lock().unwrap_or_else(|p| p.into_inner()) = None;
            drop(dev);
            // What does not fit the next call goes first, in order.
            let extra = out.len().saturating_sub(buf.len());
            if extra > 0 {
                let _ = self.fifo.write(&out[..extra]);
            }
            replies = out.len() - extra;
            buf[..replies].copy_from_slice(&out[extra..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_go_as_the_kernel_reads_them() {
        let iov = Iov {
            ins: vec![(0x1000, 4096)],
            outs: vec![(0x2000, 24), (0x3000, 64)],
        };
        let b = iov.encode();
        assert_eq!(b.len(), Iov::len_of(1, 2));
        assert_eq!(Iov::decode(&b), Some(iov.clone()));
        assert_eq!(iov.totals(), (4096, 88));
        assert!(iov.fits());
        let big = Iov {
            ins: vec![(0, SLOT_LEN as u64)],
            outs: vec![],
        };
        assert!(!big.fits());
    }

    #[test]
    fn an_ioctl_number_says_what_it_carries() {
        assert_eq!(ioctl_carries(0x8060_4521), (0, 96));
        assert_eq!(ioctl_carries(0x4004_4590), (4, 0));
        assert_eq!(ioctl_carries(0xc010_4100), (16, 16));
        assert_eq!(ioctl_carries(0x5401), (0, 0));
    }
}
