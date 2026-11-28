//! A device node that a process serves: the kernel's side of it.
//!
//! A driver that runs as a process (`tools/eclipse-udrivers`) puts a node
//! in `/dev` ([`serve`]) and answers what is done to it: each open, read,
//! write and `ioctl` is a request on the FIFO the driver gave
//! ([`link`]), and the driver's answer is what the program is told. The
//! kernel keeps no state of the device, only of each open: its name on the
//! server's side and its readiness, which the server says when it changes.
//! It is the same idea as Linux's CUSE.
//!
//! A read never waits in the server. A server with nothing to give says
//! `EAGAIN`, and a reader that may wait waits in the kernel, on the open's
//! readiness ([`ServedOpen::bus`]), until the server says the open can be
//! read; `poll`, `select` and `epoll` wait on the same. So a quiet input
//! device holds no request of the server's, and a reader can be killed
//! while it waits.
//!
//! When the server goes the node goes with it, as an unplugged device's
//! does: what is open of it reads as hung up (`POLLERR | POLLHUP`) and
//! every request on it is `ENODEV`. The next server puts the node back, and
//! programs open it again, as they would a device plugged in again.
//!
//! What this trusts: the server is a process root started for the purpose.
//! Its answers are whatever it says, and it sees no kernel memory, only the
//! window it gave; what an `ioctl` carries is copied through that window as
//! the command's number says ([`wire::ioctl_carries`]). An argument that
//! points further (a structure holding pointers) is followed only as far as
//! the server asks ([`wire::IOCTL_FETCH`]), and only in the memory of the
//! program that made the call, as its own `read` and `write` would. The
//! server must not open its own node: the open would wait on itself.

pub mod card;
pub mod input;
mod link;
/// The RM of `/dev/nvidiactl` in a process. x86_64 only, like the device
/// files.
#[cfg(target_arch = "x86_64")]
pub mod nvctl;
#[cfg(test)]
mod tests;
pub mod wire;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use kernel_hal::thread::SyncWait;
use lock::Mutex;
use rcore_fs::vfs::{FileType, FsError, INode, Metadata, PollStatus, Result, Timespec};
use zircon_object::ipc::Fifo;
use zircon_object::object::KernelObject;
use zircon_object::vm::{VmAddressRegion, VmObject};

pub use self::link::LEFT as SERVER_LEFT;
use self::link::{Fail, Link};
use self::wire::{
    MMAP_SHARED, MMAP_WRITE, OP_IOCTL, OP_IOCTL_IOV, OP_MMAP, OP_OPEN, OP_READ, OP_RELEASE,
    OP_WRITE, POLLERR, POLLHUP, POLLIN, POLLOUT, SLOT_LEN,
};
use crate::error::{LxError, LxResult};
use crate::process::LinuxProcess;
use crate::sync::{Event, EventBus};

/// The readiness bits an open's bus carries.
const READINESS: Event = Event::from_bits_truncate(
    Event::READABLE.bits() | Event::WRITABLE.bits() | Event::ERROR.bits() | Event::CLOSED.bits(),
);

/// How many readiness messages for opens not yet known are kept: a server
/// may say an open is readable before the kernel has read its answer to
/// the open.
const EARLY_KEPT: usize = 64;

/// A node in `/dev` that a process serves.
pub struct ServedNode(Arc<Core>);

/// How a handle the server names in its answer to an `mmap`
/// ([`wire::OP_MMAP`]) is found in its table: the VMO, and whether the
/// handle lets it be written. `None` for a handle that is not a VMO the
/// server may map. The loader gives it, knowing the server's process.
pub type VmoOf = alloc::boxed::Box<dyn Fn(u64) -> Option<(Arc<VmObject>, bool)> + Send + Sync>;

/// How long a node whose server died waits for the next one, with what
/// is open of it, before it goes ([`Core::keeps`]): as long as a served
/// disk or mount waits, longer than init's longest pause between two
/// starts of a service that keeps dying (8 s).
const SERVER_GRACE_MS: u64 = 10_000;

/// What the node and its opens share.
struct Core {
    /// The link to the server of the moment.
    link: Mutex<Arc<Link>>,
    /// Whether the node outlives its server: it stays in `/dev`, what is
    /// open of it waits for the next server for [`SERVER_GRACE_MS`], and
    /// the next server, serving the same name and number, takes it over
    /// and is asked to open again each open of it. An evdev node only: an
    /// open of one is a stream of events and nothing more, so another open
    /// of the next server is the same open to its reader.
    keeps: bool,
    /// How many servers it has had: the turn each open's `fh` is of.
    turn: AtomicU64,
    /// While it waits for its next server: until when (`link::now_ms`).
    until: AtomicU64,
    /// Its path under `/dev`.
    name: String,
    inode_id: usize,
    rdev: usize,
    mode: u32,
    /// The opens of it, by the server's name for each.
    opens: Mutex<BTreeMap<u32, Weak<Open>>>,
    /// Readiness said of opens not in `opens` yet.
    early: Mutex<Vec<(u32, u64)>>,
    /// The server's VMOs, for its answers to an `mmap`: none, and the node
    /// cannot be mapped.
    vmos: Mutex<Option<Arc<VmoOf>>>,
}

/// What a request's failure is to the program.
fn lx_of(fail: Fail) -> LxError {
    match fail {
        Fail::Gone => LxError::ENODEV,
        Fail::Said(errno) => match errno {
            1 => LxError::EPERM,
            2 => LxError::ENOENT,
            6 => LxError::ENXIO,
            11 => LxError::EAGAIN,
            12 => LxError::ENOMEM,
            13 => LxError::EACCES,
            14 => LxError::EFAULT,
            16 => LxError::EBUSY,
            19 => LxError::ENODEV,
            22 => LxError::EINVAL,
            25 => LxError::ENOTTY,
            28 => LxError::ENOSPC,
            32 => LxError::EPIPE,
            38 => LxError::ENOSYS,
            75 => LxError::EOVERFLOW,
            95 => LxError::EOPNOTSUPP,
            110 => LxError::ETIMEDOUT,
            _ => LxError::EIO,
        },
    }
}

/// The same, where the VFS wants its own error.
fn fs_of(fail: Fail) -> FsError {
    match fail {
        Fail::Gone => FsError::NoDevice,
        Fail::Said(errno) => match errno {
            1 => FsError::NotPermitted,
            2 => FsError::EntryNotFound,
            6 => FsError::NoSuchDeviceOrAddress,
            11 => FsError::Again,
            12 => FsError::NoMemory,
            13 => FsError::NoPermission,
            14 => FsError::BadAddress,
            16 => FsError::Busy,
            19 => FsError::NoDevice,
            22 => FsError::InvalidParam,
            28 => FsError::NoDeviceSpace,
            32 => FsError::Broken,
            38 => FsError::NotSupported,
            95 => FsError::OpNotSupported,
            110 => FsError::TimedOut,
            _ => FsError::DeviceError,
        },
    }
}

/// A server's readiness mask as the bus's events.
fn events_of(mask: u64) -> Event {
    let mut e = Event::empty();
    if mask & POLLIN != 0 {
        e |= Event::READABLE;
    }
    if mask & POLLOUT != 0 {
        e |= Event::WRITABLE;
    }
    if mask & POLLERR != 0 {
        e |= Event::ERROR;
    }
    if mask & POLLHUP != 0 {
        e |= Event::CLOSED;
    }
    e
}

impl ServedNode {
    fn new(link: Arc<Link>, name: &str, rdev: usize, mode: u32) -> Arc<Self> {
        let node = Arc::new(Core {
            link: Mutex::new(link.clone()),
            keeps: input::event_id(name, rdev).is_some(),
            turn: AtomicU64::new(1),
            until: AtomicU64::new(0),
            name: String::from(name),
            inode_id: rcore_fs_devfs::DevFS::new_inode_id(),
            rdev,
            mode,
            opens: Mutex::new(BTreeMap::new()),
            early: Mutex::new(Vec::new()),
            vmos: Mutex::new(None),
        });
        node.hear(&link);
        Arc::new(ServedNode(node))
    }

    /// Its path under `/dev`.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// A new open: the server is asked, and the open is its answer.
    pub fn open_client(&self, flags: usize) -> LxResult<Arc<dyn INode>> {
        self.0.open_client(flags)
    }

    /// The server is gone, or is to be let go of: every open hangs up.
    pub fn server_left(&self) {
        self.0.server_left()
    }

    /// The server is gone. A node that [`keeps`](Core::keeps) waits for the
    /// next: its turn now, which [`ServedNode::taken_over_since`] is asked
    /// about once the wait is over. `None` for one that does not wait, and
    /// whose opens have hung up.
    pub fn server_went(&self) -> Option<u64> {
        let core = &self.0;
        if !core.keeps {
            core.server_left();
            return None;
        }
        core.link().close();
        core.until
            .store(link::now_ms() + SERVER_GRACE_MS, Ordering::Release);
        info!("served dev: /dev/{} waits for its next server", core.name);
        Some(core.turn.load(Ordering::Acquire))
    }

    /// How long a node waits for its next server.
    pub const GRACE_MS: u64 = SERVER_GRACE_MS;

    /// Whether a next server took the node over since turn `turn`. When
    /// none did, the wait is over: its opens hang up.
    pub fn taken_over_since(&self, turn: u64) -> bool {
        let core = &self.0;
        if core.turn.load(Ordering::Acquire) != turn {
            return true;
        }
        core.until.store(0, Ordering::Release);
        core.server_left();
        false
    }

    /// Read whatever the server has written, while no request does: its
    /// readiness, or its going. Whether the server is still there.
    pub fn look(&self) -> bool {
        let link = self.0.link();
        link.pump();
        !link.is_dead()
    }

    /// The watch's look, once a second, at a server that may have frozen:
    /// see [`Link::look_quiet`]. Only while something is open of the node:
    /// a server nobody waits on is nobody's to find hung.
    pub fn look_quiet(&self) {
        let open = self.0.opens.lock().values().any(|o| o.strong_count() > 0);
        self.0.link().look_quiet(link::now_ms(), open);
    }

    /// The process that serves the node (`server`, its id), and how it is
    /// ended when it hangs.
    pub fn killed_with(&self, server: u64, kill: alloc::boxed::Box<dyn Fn() + Send + Sync>) {
        self.0.link().killed_with(server, kill);
    }

    /// Where the VMOs the server answers an `mmap` with are found.
    pub fn vmos_from(&self, vmos: VmoOf) {
        *self.0.vmos.lock() = Some(Arc::new(vmos));
    }
}

impl Core {
    /// The link to the server of the moment.
    fn link(&self) -> Arc<Link> {
        self.link.lock().clone()
    }

    /// Where the readiness `link`'s server says goes: to this node's opens.
    fn hear(self: &Arc<Self>, link: &Link) {
        let weak = Arc::downgrade(self);
        link.ready_to(alloc::boxed::Box::new(move |fh, mask| {
            if let Some(node) = weak.upgrade() {
                node.ready(fh, mask);
            }
        }));
    }

    /// Whether a request that found the server gone waits for the next:
    /// the node [`keeps`](Self::keeps), and its wait is not over.
    fn waits(&self) -> bool {
        self.keeps && link::now_ms() <= self.until.load(Ordering::Acquire)
    }

    /// Ask with `ask` on the link of the moment and the name the server has
    /// for `open` there (none for a new open), and that server's turn. A request that finds the
    /// server gone, on a node that waits for the next, is asked again of the
    /// next once it has opened `open` again; one whose open it could not
    /// open again is `Gone`.
    fn asking<T>(
        &self,
        open: Option<&Open>,
        mut ask: impl FnMut(&Arc<Link>, u32, u64) -> core::result::Result<T, Fail>,
    ) -> core::result::Result<T, Fail> {
        let mut away = SyncWait::new();
        loop {
            if open.is_some_and(|o| o.lost.load(Ordering::Acquire)) {
                return Err(Fail::Gone);
            }
            // The two together: the turn changes with the link, under its lock.
            let (link, turn) = {
                let link = self.link.lock();
                (link.clone(), self.turn.load(Ordering::Acquire))
            };
            // An open not opened again on this server yet has no name there.
            let fh = match open {
                Some(open) if open.turn.load(Ordering::Acquire) != turn => None,
                Some(open) => Some(open.fh.load(Ordering::Acquire)),
                None => Some(0),
            };
            if let Some(fh) = fh {
                match ask(&link, fh, turn) {
                    Err(Fail::Gone) if self.waits() => {}
                    done => return done,
                }
            } else if !self.waits() {
                return Err(Fail::Gone);
            }
            crate::fs::served::between_looks(&mut away);
        }
    }

    /// The next server took the node over, on `link`: each open of it is
    /// opened again there, and has the readiness the server says of it.
    /// One the server will not open again hangs up. Not from the call that
    /// serves the node: the server answers nothing until it is back.
    fn take_over(self: &Arc<Self>, link: Arc<Link>) {
        // What waits, waits until the opens are opened again.
        self.until
            .fetch_max(link::now_ms() + SERVER_GRACE_MS, Ordering::AcqRel);
        self.hear(&link);
        let opens: Vec<Arc<Open>> = {
            let mut held = self.link.lock();
            *held = link.clone();
            // The names were the dead server's: the next one's may be the
            // same numbers for other opens.
            let opens = core::mem::take(&mut *self.opens.lock())
                .into_values()
                .filter_map(|o| o.upgrade())
                .collect();
            self.turn.fetch_add(1, Ordering::AcqRel);
            opens
        };
        let turn = self.turn.load(Ordering::Acquire);
        let core = self.clone();
        kernel_hal::thread::spawn(async move {
            for open in opens {
                match link.call(OP_OPEN, 0, 0, open.flags as u64, 0, &[]) {
                    Ok(answer) => {
                        let (fh, mut mask) = (answer.fh, answer.a);
                        drop(answer);
                        core.opens.lock().insert(fh, Arc::downgrade(&open));
                        if let Some(at) = core.early.lock().iter().position(|(f, _)| *f == fh) {
                            mask = core.early.lock().remove(at).1;
                        }
                        open.fh.store(fh, Ordering::Release);
                        open.turn.store(turn, Ordering::Release);
                        open.bus.lock().change(READINESS, events_of(mask));
                    }
                    Err(_) => {
                        open.lost.store(true, Ordering::Release);
                        open.bus
                            .lock()
                            .change(READINESS, Event::ERROR | Event::CLOSED);
                    }
                }
            }
            core.until.store(0, Ordering::Release);
            info!("served dev: /dev/{} has a server again", core.name);
        });
    }

    /// The server said open `fh` is now as `mask` says.
    fn ready(&self, fh: u32, mask: u64) {
        let bus = self
            .opens
            .lock()
            .get(&fh)
            .and_then(Weak::upgrade)
            .map(|open| open.bus.clone());
        match bus {
            Some(bus) => bus.lock().change(READINESS, events_of(mask)),
            None => {
                let mut early = self.early.lock();
                early.retain(|(f, _)| *f != fh);
                if early.len() >= EARLY_KEPT {
                    early.remove(0);
                }
                early.push((fh, mask));
            }
        }
    }

    fn open_client(self: &Arc<Self>, flags: usize) -> LxResult<Arc<dyn INode>> {
        let (answer, turn) = self
            .asking(None, |link, _, turn| {
                link.call(OP_OPEN, 0, 0, flags as u64, 0, &[])
                    .map(|answer| (answer, turn))
            })
            .map_err(lx_of)?;
        let (fh, mut mask) = (answer.fh, answer.a);
        drop(answer);
        let bus = EventBus::new();
        let open = Arc::new(Open {
            fh: AtomicU32::new(fh),
            turn: AtomicU64::new(turn),
            flags,
            bus: bus.clone(),
            lost: AtomicBool::new(false),
        });
        {
            let mut opens = self.opens.lock();
            opens.retain(|_, o| o.strong_count() > 0);
            opens.insert(fh, Arc::downgrade(&open));
        }
        {
            let mut early = self.early.lock();
            if let Some(at) = early.iter().position(|(f, _)| *f == fh) {
                mask = early.remove(at).1;
            }
        }
        let gone = self.link().is_dead() && !self.waits();
        bus.lock().change(
            READINESS,
            if gone {
                Event::ERROR | Event::CLOSED
            } else {
                events_of(mask)
            },
        );
        Ok(Arc::new(ServedOpen {
            node: self.clone(),
            open,
        }))
    }

    fn server_left(&self) {
        self.link().close();
        let opens: Vec<_> = self
            .opens
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for open in opens {
            open.bus
                .lock()
                .change(READINESS, Event::ERROR | Event::CLOSED);
        }
    }

    fn metadata_of(&self) -> Metadata {
        Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: self.mode as u16,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: self.rdev,
        }
    }
}

/// The node itself, before an open: what `stat` sees. Opening it goes
/// through [`ServedNode::open_client`] (`prepare_open_inode`), so nothing
/// reads or writes this.
impl INode for ServedNode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NoDevice)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NoDevice)
    }

    fn poll(&self) -> Result<PollStatus> {
        Err(FsError::NoDevice)
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(self.0.metadata_of())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// What an open of a served node is to its node.
struct Open {
    /// The server's name for it, on the server of turn `turn`.
    fh: AtomicU32,
    turn: AtomicU64,
    /// What it was opened with: what the next server is asked to open it
    /// with again.
    flags: usize,
    /// Its readiness, as the server last said.
    bus: Arc<Mutex<EventBus>>,
    /// The next server would not open it again: it has hung up.
    lost: AtomicBool,
}

/// One open of a served node.
pub struct ServedOpen {
    node: Arc<Core>,
    open: Arc<Open>,
}

impl ServedOpen {
    /// Where its readiness is: what a reader, `poll` or `epoll` waits on.
    pub fn bus(&self) -> Arc<Mutex<EventBus>> {
        self.open.bus.clone()
    }

    /// Ask the server of the moment with `ask`, given the link and its
    /// name for this open: [`Core::asking`].
    fn ask<T>(
        &self,
        ask: impl FnMut(&Arc<Link>, u32, u64) -> core::result::Result<T, Fail>,
    ) -> core::result::Result<T, Fail> {
        self.node.asking(Some(&self.open), ask)
    }

    /// `mmap(offset, len)` of this open, `shared` or not, with `write`
    /// asked for: the VMO the server answered with, where the window starts
    /// in it, and whether the mapping is read-only (the server's handle
    /// does not let the VMO be written). `ENODEV` when the server has
    /// nothing to map, or the node no way to find its VMOs; `EIO` for an
    /// answer that names no VMO the server may map; `ENXIO` for a VMO too
    /// short for the window, or a window that does not start on a page;
    /// `EACCES` for a shared, writable mapping of a read-only one.
    pub fn map(
        &self,
        offset: usize,
        len: usize,
        shared: bool,
        write: bool,
    ) -> LxResult<(Arc<VmObject>, usize, bool)> {
        let vmos = self.node.vmos.lock().clone().ok_or(LxError::ENODEV)?;
        let how = if shared { MMAP_SHARED } else { 0 } | if write { MMAP_WRITE } else { 0 };
        let answer = self
            .ask(|link, fh, _| link.call(OP_MMAP, fh, how as usize, offset as u64, len as u64, &[]))
            .map_err(lx_of)?;
        let at = if answer.len >= 8 {
            let mut b = [0u8; 8];
            answer.data(&mut b).map_err(lx_of)?;
            u64::from_ne_bytes(b)
        } else {
            0
        };
        let handle = answer.a;
        drop(answer);
        let (vmo, writable) = vmos(handle).ok_or(LxError::EIO)?;
        let at: usize = core::convert::TryFrom::try_from(at).map_err(|_| LxError::ENXIO)?;
        let end = at
            .checked_add(len.div_ceil(zircon_object::vm::PAGE_SIZE) * zircon_object::vm::PAGE_SIZE)
            .ok_or(LxError::ENXIO)?;
        if !at.is_multiple_of(zircon_object::vm::PAGE_SIZE) || end > vmo.len() {
            return Err(LxError::ENXIO);
        }
        if shared && write && !writable {
            return Err(LxError::EACCES);
        }
        Ok((vmo, at, !writable))
    }

    /// `ioctl(cmd, arg)`, with what the command carries copied in from the
    /// program's `arg` and out to it, and what else of the program's memory
    /// the server asks for ([`wire::IOCTL_FETCH`]). What the server answers
    /// is what the call returns.
    pub fn ioctl(&self, cmd: u32, arg: usize) -> LxResult<usize> {
        self.ioctl_as(cmd, arg, false)
    }

    /// [`ServedOpen::ioctl`], said to the server to be on an open that
    /// waits, or not ([`wire::IOCTL_WAITS`]).
    fn ioctl_as(&self, cmd: u32, arg: usize, waits: bool) -> LxResult<usize> {
        let a = cmd as u64 | if waits { wire::IOCTL_WAITS } else { 0 };
        let (into, out) = wire::ioctl_carries(cmd);
        // An argument that is not memory to read goes over with nothing in
        // the slot: some commands say they carry an `int` in and take it by
        // value (evdev's `EVIOCGRAB`), and only the server knows which. It
        // answers `EFAULT` for any other (`udrivers::served`).
        let data = if into > 0 {
            kernel_hal::user::UserInPtr::<u8>::from(arg)
                .read_array(into)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // The caller's memory, which the server may reach while it answers
        // ([`asker_of`]): what NVIDIA's commands hold is pointers to more.
        let asker = current_vmar();
        // The rounds of one call go to the one server that answered its
        // first.
        let (link, fh, mut answer) = self
            .ask(|link, fh, _| {
                link.call_for(asker.clone(), OP_IOCTL, fh, 0, a, arg as u64, &data)
                    .map(|answer| (link.clone(), fh, answer))
            })
            .map_err(lx_of)?;
        if answer.a != wire::IOCTL_FETCH {
            if out > 0 {
                let n = answer.len.min(out);
                let mut back = vec![0u8; n];
                answer.data(&mut back).map_err(lx_of)?;
                kernel_hal::user::UserOutPtr::<u8>::from(arg)
                    .write_array(&back)
                    .map_err(|_| LxError::EFAULT)?;
                // ALSA's HW_PARAMS: the period the served card settled on,
                // for its `/dev/snd/timer` ticks (`snd::note_served_hw_params`).
                if cmd & 0xffff == 0x4111 {
                    super::snd::note_served_hw_params(&self.node.name, &back);
                }
            }
            return Ok(answer.count);
        }
        for _ in 0..wire::FETCH_ROUNDS {
            let iov = Self::asked(&answer)?;
            drop(answer);
            let mut req = iov.encode();
            for &(addr, len) in &iov.ins {
                let bytes = kernel_hal::user::UserInPtr::<u8>::from(addr as usize)
                    .read_array(len as usize)
                    .map_err(|_| LxError::EFAULT)?;
                req.extend_from_slice(&bytes);
            }
            answer = link
                .call_for(asker.clone(), OP_IOCTL_IOV, fh, 0, a, arg as u64, &req)
                .map_err(lx_of)?;
            if answer.a == wire::IOCTL_FETCH {
                continue;
            }
            let total = iov.outs.iter().map(|(_, l)| *l as usize).sum::<usize>();
            let mut back = vec![0u8; answer.len.min(total)];
            answer.data(&mut back).map_err(lx_of)?;
            let mut at = 0;
            for &(addr, len) in &iov.outs {
                let n = (len as usize).min(back.len() - at);
                if n == 0 {
                    break;
                }
                kernel_hal::user::UserOutPtr::<u8>::from(addr as usize)
                    .write_array(&back[at..at + n])
                    .map_err(|_| LxError::EFAULT)?;
                at += n;
            }
            return Ok(answer.count);
        }
        // A server that keeps asking is failing the call.
        Err(LxError::EIO)
    }

    /// The ranges a [`wire::IOCTL_FETCH`] asks for: `EIO` when the server
    /// asked for more than a slot carries.
    fn asked(answer: &link::Answer) -> LxResult<wire::Iov> {
        let mut head = vec![
            0u8;
            answer
                .len
                .min(wire::Iov::len_of(wire::IOV_MAX, wire::IOV_MAX))
        ];
        answer.data(&mut head).map_err(lx_of)?;
        wire::Iov::decode(&head).ok_or(LxError::EIO)
    }

    /// `ioctl(cmd)` for the kernel's own use: a command that only gives
    /// back (`_IOR`), into `out`. How many bytes it gave.
    pub fn ioctl_read(&self, cmd: u32, out: &mut [u8]) -> LxResult<usize> {
        let answer = self
            .ask(|link, fh, _| link.call(OP_IOCTL, fh, 0, cmd as u64, 0, &[]))
            .map_err(lx_of)?;
        if answer.a == wire::IOCTL_FETCH {
            return Err(LxError::EIO);
        }
        let n = answer.len.min(out.len());
        answer.data(&mut out[..n]).map_err(lx_of)?;
        Ok(n)
    }

    /// [`ServedOpen::ioctl`] on an open that waits: a server never waits,
    /// and answers `EAGAIN` for what cannot be done yet (ALSA's
    /// `WRITEI_FRAMES` with no room, its `DRAIN` with sound still to
    /// play). The call then waits for the server to say something of the
    /// open again ([`OP_READY`]), and asks again. A signal ends the wait
    /// with `EINTR`.
    pub async fn ioctl_waiting(&self, cmd: u32, arg: usize) -> LxResult<usize> {
        loop {
            // Read before the call: what the server says on its way to the
            // answer, or right after it, counts.
            let seen = self.open.bus.lock().seq();
            match self.ioctl_as(cmd, arg, true) {
                Err(LxError::EAGAIN) => {}
                done => return done,
            }
            let bus = self.open.bus.clone();
            crate::process::interruptible(SaidSince {
                bus,
                seen,
                sub: None,
            })
            .await?;
        }
    }
}

impl Drop for ServedOpen {
    fn drop(&mut self) {
        let fh = self.open.fh.load(Ordering::Acquire);
        {
            // Only its own entry: an open not opened again on the next
            // server has none, and its old name may be another's there.
            let mut opens = self.node.opens.lock();
            if opens
                .get(&fh)
                .is_some_and(|o| core::ptr::eq(o.as_ptr(), Arc::as_ptr(&self.open)))
            {
                opens.remove(&fh);
            }
        }
        // Only the server it was opened on knows it.
        if self.open.turn.load(Ordering::Acquire) == self.node.turn.load(Ordering::Acquire) {
            self.node.link().tell(OP_RELEASE, fh);
        }
    }
}

impl INode for ServedOpen {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let room = buf.len().min(SLOT_LEN);
        let answer = self
            .ask(|link, fh, _| link.call(OP_READ, fh, room, offset as u64, 0, &[]))
            .map_err(fs_of)?;
        let n = answer.count.min(room);
        answer.data(&mut buf[..n]).map_err(fs_of)?;
        Ok(n)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let mut done = 0;
        for chunk in buf.chunks(SLOT_LEN) {
            let taken = match self
                .ask(|link, fh, _| link.call(OP_WRITE, fh, 0, (offset + done) as u64, 0, chunk))
            {
                Ok(answer) => answer.count.min(chunk.len()),
                Err(fail) if done == 0 => return Err(fs_of(fail)),
                Err(_) => break,
            };
            done += taken;
            if taken < chunk.len() {
                break;
            }
        }
        Ok(done)
    }

    fn poll(&self) -> Result<PollStatus> {
        let events = self.open.bus.lock().events();
        Ok(PollStatus {
            read: events.contains(Event::READABLE),
            write: events.contains(Event::WRITABLE),
            error: events.contains(Event::ERROR),
            hangup: events.contains(Event::CLOSED),
        })
    }

    /// Waits to read, as every node's does: until the server says the open
    /// can be read, or has hung up.
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<alloc::boxed::Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        alloc::boxed::Box::pin(async move {
            let mask = Event::READABLE | Event::ERROR | Event::CLOSED;
            crate::sync::wait_for_event(self.open.bus.clone(), mask)
                .await
                .map_err(|_| FsError::Interrupted)?;
            self.poll()
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.ioctl(cmd, data).map_err(|_| FsError::IOCTLError)
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(self.node.metadata_of())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// Until the server says something of an open again: its bus has been
/// published to since `seen` ([`EventBus::seq`]), even with the readiness
/// as it was.
struct SaidSince {
    bus: Arc<Mutex<EventBus>>,
    seen: u64,
    sub: Option<crate::sync::ReadinessSub>,
}

impl Future for SaidSince {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> core::task::Poll<()> {
        // The last poll's parking goes first: it takes the bus's lock.
        drop(self.sub.take());
        let bus = self.bus.clone();
        let mut b = bus.lock();
        if b.seq() != self.seen {
            return core::task::Poll::Ready(());
        }
        // Parked for the next publish of any readiness. `subscribe_edge`
        // compares against the counter of the bits asked about, which for
        // all four is the sum of theirs, not `seq`: compare as it does.
        let seen = b.seq_for(READINESS);
        let sub = b
            .subscribe_edge(READINESS, cx.waker(), seen)
            .map(|id| crate::sync::ReadinessSub::on_bus(bus.clone(), id));
        drop(b);
        self.sub = sub;
        core::task::Poll::Pending
    }
}

/// The node's path under `/dev`, when it is one a server may take: one to
/// four names of letters, digits, `-`, `_` and `.`, a `/` between each two,
/// none of them `.` or `..`.
fn valid_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('/').collect();
    (1..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 64
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        })
}

/// What [`serve`] made: the node, for whoever watches the server.
pub struct Served {
    pub node: Arc<ServedNode>,
}

/// Put `/dev/<name>` there, a character device `major:minor` with
/// permissions `mode`, served by the process at the other end of `fifo`
/// through `window`. The directories on the way are made when they are not
/// there. `EPERM` for a process that may not; `EEXIST` when the name is
/// taken; `EINVAL` for a name [`valid_name`] refuses, a window that does
/// not hold a slot, or a FIFO of other messages; `ENODEV` before `/dev` is
/// there.
pub fn serve(
    proc: &LinuxProcess,
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    name: &str,
    major: usize,
    minor: usize,
    mode: u32,
) -> LxResult<Served> {
    if !proc.capable(crate::process::CAP_SYS_ADMIN) {
        return Err(LxError::EPERM);
    }
    if !valid_name(name) || mode & !0o777 != 0 || major > 0xfff || minor > 0xff {
        return Err(LxError::EINVAL);
    }
    let koid = fifo.id();
    let link = Arc::new(Link::new(fifo, window).map_err(|_| LxError::EINVAL)?);
    let rdev = rcore_fs::vfs::make_rdev(major, minor);
    let root = crate::fs::DEVFS_ROOT.lock().clone();
    let root: Arc<dyn INode> = root.ok_or(LxError::ENODEV)?;
    // The node of a server that died, waiting for the next: this one's, as
    // it is, with what is open of it.
    if let Some(core) = waiting_at(&root, name, rdev, mode) {
        core.take_over(link.clone());
        remember(koid, &link);
        info!("served dev: /dev/{name} ({major}:{minor}) is taken over");
        return Ok(Served {
            node: Arc::new(ServedNode(core)),
        });
    }
    let node = ServedNode::new(link.clone(), name, rdev, mode);
    add(&root, name, node.clone())?;
    remember(koid, &link);
    info!("served dev: /dev/{name} ({major}:{minor})");
    input::attach(&node);
    Ok(Served { node })
}

/// The node at `path` under `dir` when it is a served one with the device
/// number `rdev` and permissions `mode` whose server died and that waits
/// for the next ([`Core::keeps`]).
fn waiting_at(dir: &Arc<dyn INode>, path: &str, rdev: usize, mode: u32) -> Option<Arc<Core>> {
    let mut at = dir.clone();
    for part in path.split('/') {
        at = at.find(part).ok()?;
    }
    let core = &at.downcast_ref::<ServedNode>()?.0;
    (core.keeps && core.rdev == rdev && core.mode == mode && core.link().is_dead() && core.waits())
        .then(|| core.clone())
}

/// `node` at `path` under `dir`, with the directories on the way.
fn add(dir: &Arc<dyn INode>, path: &str, node: Arc<dyn INode>) -> LxResult {
    use rcore_fs_devfs::DevINode;
    let devdir = dir.downcast_ref::<DevINode>().ok_or(LxError::ENOTDIR)?;
    match path.split_once('/') {
        None => devdir.add(path, node).map_err(|_| LxError::EEXIST),
        Some((first, rest)) => {
            let next: Arc<dyn INode> = match dir.find(first) {
                Ok(next) => next,
                Err(_) => match devdir.add_dir(first) {
                    Ok(made) => made,
                    // Made by another server meanwhile.
                    Err(_) => dir.find(first).map_err(|_| LxError::EEXIST)?,
                },
            };
            add(&next, rest, node)
        }
    }
}

/// Take the node away from `/dev`: its server is gone. The directories on
/// the way stay, for the next server.
pub fn unserve(node: &ServedNode) {
    let Some(root) = crate::fs::DEVFS_ROOT.lock().clone() else {
        return;
    };
    let mut dir: Arc<dyn INode> = root;
    let node = &node.0;
    let mut parts: Vec<&str> = node.name.split('/').collect();
    let Some(last) = parts.pop() else {
        return;
    };
    for part in parts {
        match dir.find(part) {
            Ok(next) => dir = next,
            Err(_) => return,
        }
    }
    // Only this node: the name may be another server's by now.
    let ours = dir
        .find(last)
        .ok()
        .and_then(|n| {
            n.downcast_ref::<ServedNode>()
                .map(|n| n.0.inode_id == node.inode_id)
        })
        .unwrap_or(false);
    if let (true, Some(d)) = (ours, dir.downcast_ref::<rcore_fs_devfs::DevINode>()) {
        let _ = d.remove(last);
        info!("served dev: /dev/{} is gone", node.name);
    }
}

/// The links of the nodes being served, by the kernel's end of each FIFO:
/// how [`asker_of`] finds a request from the server's end.
static LINKS: Mutex<BTreeMap<u64, Weak<Link>>> = Mutex::new(BTreeMap::new());

fn remember(koid: u64, link: &Arc<Link>) {
    let mut links = LINKS.lock();
    // The links of servers that are gone go as new ones come.
    links.retain(|_, l| l.strong_count() > 0);
    links.insert(koid, Arc::downgrade(link));
}

/// The requests known to take long that are waiting now ([`long_request`]):
/// each with the link it was asked on, and until when (`link::now_ms`) its
/// silence is no hang.
static LONG_REQUESTS: Mutex<Vec<(u64, Arc<Link>, u64)>> = Mutex::new(Vec::new());

/// A request known to take long, waiting on a link until this is dropped.
struct LongRequest {
    id: u64,
    link: Arc<Link>,
}

/// A request that may take `ms` is about to be asked on `link` (a GPU's
/// firmware boot, nvidia-modeset's first modeset). While it waits, and no
/// longer than `ms`, silence is not a hang on any link its server answers.
///
/// Such a request holds the driver's RM for as long, and what the driver's
/// other threads were asked meanwhile, on the links of its other cards and
/// of `/dev/nvidiactl`, waits for the RM unanswered. Each link counts its
/// own silence: ten seconds of that on any of them had the driver killed
/// halfway through the boot of the console GPU, which leaves its monitor
/// black for the rest of the boot.
///
/// The patience is the request's, and ends with it: when what this returns
/// is dropped, every link of the server counts its silence from then, and
/// a driver that hangs afterwards is found as soon as any other.
fn long_request(link: &Arc<Link>, ms: u64) -> LongRequest {
    static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let until = link::now_ms().saturating_add(ms);
    LONG_REQUESTS.lock().push((id, link.clone(), until));
    LongRequest {
        id,
        link: link.clone(),
    }
}

impl Drop for LongRequest {
    fn drop(&mut self) {
        // Before the request stops waiting, so that no link finds neither
        // the request nor the time it ended: the server was busy until now,
        // not silent.
        self.link.heard_now();
        let server = self.link.server();
        if server != 0 {
            // Looked at outside the lock: one of these may be a link's last
            // reference.
            let links: Vec<Arc<Link>> = LINKS.lock().values().filter_map(Weak::upgrade).collect();
            for other in links.iter().filter(|other| other.server() == server) {
                other.heard_now();
            }
        }
        let over = {
            let mut waiting = LONG_REQUESTS.lock();
            let at = waiting.iter().position(|(id, ..)| *id == self.id);
            at.map(|at| waiting.swap_remove(at))
        };
        // Outside the lock, for the same reason.
        drop(over);
    }
}

/// Whether a request known to take long is waiting at `now`
/// ([`long_request`]), on `link` or on another link of its server: the
/// silence is that request's.
///
/// Each link's server is read now, not when the request was asked: a link
/// is covered from the moment its server is said, whenever it was made.
fn long_request_waits(link: &Link, now: u64) -> bool {
    let server = link.server();
    LONG_REQUESTS.lock().iter().any(|(_, asked, until)| {
        now < *until
            && (core::ptr::eq(Arc::as_ptr(asked), link)
                || (server != 0 && asked.server() == server))
    })
}

/// The address space of the program whose `ioctl` is request `reqid` of
/// the node served at the other end of `server_end`, while it waits for
/// the answer. `None` for a FIFO that serves no node, or a request that is
/// not waiting or lent no memory. Holding the server's end of the FIFO is
/// the authority, as answering on it is.
pub fn asker_of(server_end: &dyn KernelObject, reqid: u32) -> Option<Arc<VmAddressRegion>> {
    let link = LINKS.lock().get(&server_end.related_koid())?.upgrade()?;
    link.asker(reqid)
}

/// The address space of the process making this system call.
fn current_vmar() -> Option<Arc<VmAddressRegion>> {
    let thread = kernel_hal::thread::get_current_thread()?
        .downcast::<zircon_object::task::Thread>()
        .ok()?;
    Some(thread.proc().vmar())
}
