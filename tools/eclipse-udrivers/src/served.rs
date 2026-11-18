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
pub const OP_READY: u32 = 16;

pub const POLLIN: u64 = 0x1;
pub const POLLOUT: u64 = 0x4;
pub const POLLERR: u64 = 0x8;
pub const POLLHUP: u64 = 0x10;

/// A slot of the window, and how many there are.
pub const SLOT_LEN: usize = 64 * 1024;
pub const SLOTS: usize = 16;

/// The `errno`s a device answers with.
pub const EAGAIN: i32 = 11;
pub const EINVAL: i32 = 22;
pub const ENOTTY: i32 = 25;

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
        let (fifo, theirs) =
            Fifo::<Msg>::create(SLOTS * 4).map_err(|e| format!("fifo_create: {e}"))?;
        let (window, vmo) = zx::shared_memory(SLOTS * SLOT_LEN)?;
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
        .map_err(|e| format!("dev_serve /dev/{name}: {e}"))?;
        Ok(Node {
            fifo,
            window,
            held: std::sync::Mutex::new(None),
        })
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
                let (_, out) = ioctl_carries(req.a as u32);
                let data = &mut self.slot(req.reqid)[..len.max(out)];
                dev.ioctl(self, req.fh, req.a as u32, req.b, data)
                    .map(|(ret, n)| {
                        reply.len = n.min(out) as u32;
                        ret
                    })
            }
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
                let reply = self.answer(&mut *dev, req);
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
    fn an_ioctl_number_says_what_it_carries() {
        assert_eq!(ioctl_carries(0x8060_4521), (0, 96));
        assert_eq!(ioctl_carries(0x4004_4590), (4, 0));
        assert_eq!(ioctl_carries(0xc010_4100), (16, 16));
        assert_eq!(ioctl_carries(0x5401), (0, 0));
    }
}
