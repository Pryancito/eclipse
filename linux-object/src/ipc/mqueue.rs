//! POSIX message queues: `mq_open(3)` and the rest of mq_overview(7).
//!
//! Linux keeps them in `ipc/mqueue.c` as the inodes of an internal `mqueue`
//! filesystem. Here they are what the System V queues next door are: a table
//! of names, one [`PosixMq`] per queue, and a descriptor ([`MqFile`]) onto it.
//! A queue lives while it has a name or a descriptor: `mq_unlink` takes the
//! name away and the queue goes with its last descriptor, the way an unlinked
//! inode goes with its last open file.
//!
//! What is Linux's and kept as it is, because programs see it:
//!
//! - the limits of `/proc/sys/fs/mqueue` at their defaults ([`MSG_MAX`],
//!   [`MSGSIZE_MAX`], [`QUEUES_MAX`]) and the `CAP_SYS_RESOURCE` ceilings
//!   above them ([`HARD_MSGMAX`], [`HARD_MSGSIZEMAX`]);
//! - `RLIMIT_MSGQUEUE`, counted per real user as `mqueue_get_inode` counts it
//!   (`EMFILE` past it, root too): a queue is room for `maxmsg` messages of
//!   `msgsize` bytes whether they are there or not, and the limit is what
//!   keeps a user from naming a thousand queues of 16 MiB;
//! - the highest priority first, and the oldest first within a priority;
//! - one process registered for a notification at a time, the registration
//!   gone once it fires, and fired only when a message lands in an EMPTY
//!   queue that nobody is already waiting on (`__do_notify`, and the
//!   `pipelined_send` that takes its place when a receiver is asleep);
//! - the registration dropped on any `close` of a descriptor of the queue by
//!   the process that holds it (`mqueue_flush_file`).

use crate::error::{LxError, LxResult};
use crate::fs::{anon_metadata, FileLike, OpenFlags, PollEvents, PollStatus};
use crate::kmem::Charge;
use crate::sync::{Event, EventBus};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use async_trait::async_trait;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::FileType;
use zircon_object::object::*;

/// `MQ_PRIO_MAX`: priorities run from 0 to 32767.
pub const MQ_PRIO_MAX: usize = 32768;
/// `NAME_MAX`: the longest name `simple_lookup` takes.
pub const MQ_NAME_MAX: usize = 255;
/// `PATH_MAX`: `getname` refuses a name this long or longer.
pub const MQ_PATH_MAX: usize = 4096;
/// `/proc/sys/fs/mqueue/msg_default`: `maxmsg` of a queue made without
/// attributes.
pub const DFLT_MAXMSG: usize = 10;
/// `/proc/sys/fs/mqueue/msgsize_default`.
pub const DFLT_MSGSIZE: usize = 8192;
/// `/proc/sys/fs/mqueue/msg_max`: the most `maxmsg` without
/// `CAP_SYS_RESOURCE`.
pub const MSG_MAX: usize = 10;
/// `/proc/sys/fs/mqueue/msgsize_max`.
pub const MSGSIZE_MAX: usize = 8192;
/// `HARD_MSGMAX`: the most `maxmsg` for anybody.
pub const HARD_MSGMAX: usize = 65536;
/// `HARD_MSGSIZEMAX`: the most `msgsize` for anybody.
pub const HARD_MSGSIZEMAX: usize = 16 * 1024 * 1024;
/// `/proc/sys/fs/mqueue/queues_max`: how many queues may exist at once
/// without `CAP_SYS_RESOURCE` (`ENOSPC` past it).
pub const QUEUES_MAX: usize = 256;
/// `NOTIFY_COOKIE_LEN`: what a `SIGEV_THREAD` registration hands the kernel
/// to send back on its netlink socket.
pub const NOTIFY_COOKIE_LEN: usize = 32;
/// The cookie's last byte when the notification fired (`NOTIFY_WOKENUP`)...
const NOTIFY_WOKENUP: u8 = 1;
/// ...and when the registration was taken away instead (`NOTIFY_REMOVED`).
const NOTIFY_REMOVED: u8 = 2;

/// `sizeof(struct msg_msg)` and `sizeof(struct posix_msg_tree_node)` on a
/// 64-bit Linux: the per-message overhead `RLIMIT_MSGQUEUE` counts. Linux's
/// figures and not this kernel's, so that a program sized to fit the limit
/// on Linux fits here.
const LINUX_MSG_MSG: usize = 48;
const LINUX_TREE_NODE: usize = 48;
/// What one queued message costs the kernel besides its bytes, charged to its
/// sender ([`crate::kmem`]) as `msg_msg` is with `GFP_KERNEL_ACCOUNT`.
pub const MQ_MSG_COST: usize = core::mem::size_of::<MqMsg>() + 16;

/// `struct mq_attr`: four `long`s and four reserved. The same eight words at
/// either width, so a 32-bit caller's is this with each word halved.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MqAttr {
    /// `O_NONBLOCK` or 0: the descriptor's, not the queue's.
    pub flags: isize,
    /// The most messages the queue holds.
    pub maxmsg: isize,
    /// The largest message, in bytes.
    pub msgsize: isize,
    /// Messages on the queue now.
    pub curmsgs: isize,
    /// `__reserved`.
    pub reserved: [isize; 4],
}

/// Who is asking, as far as a queue cares.
#[derive(Clone, Debug, Default)]
pub struct MqCred {
    /// Effective user and group, and the supplementary groups: what
    /// `inode_permission` decides on.
    pub euid: u32,
    /// Effective group.
    pub egid: u32,
    /// Supplementary groups.
    pub groups: Vec<u32>,
    /// Real user: whose `RLIMIT_MSGQUEUE` a new queue counts against.
    pub ruid: u32,
    /// `CAP_DAC_OVERRIDE`: past the permission bits.
    pub dac_override: bool,
    /// `CAP_FOWNER`: may unlink anybody's queue.
    pub fowner: bool,
    /// `CAP_SYS_RESOURCE`: past `msg_max`, `msgsize_max` and `queues_max`.
    pub sys_resource: bool,
    /// The caller's umask, taken off a new queue's mode.
    pub umask: u32,
    /// The caller's soft `RLIMIT_MSGQUEUE`.
    pub rlimit_msgqueue: usize,
}

/// What a registered process asked to be told with (`struct sigevent`).
pub enum MqNotifyHow {
    /// `SIGEV_NONE`: nothing is sent, but the registration is held and spent.
    None,
    /// `SIGEV_SIGNAL`: this signal (0: none), with this `si_value`.
    Signal {
        /// `sigev_signo`.
        signo: usize,
        /// `sigev_value`.
        value: usize,
    },
    /// `SIGEV_THREAD` as libc speaks it to the kernel: send `cookie` on this
    /// netlink socket, and libc's helper thread runs the function.
    Thread {
        /// The socket `sigev_signo` named.
        sock: Arc<dyn FileLike>,
        /// `sigev_value.sival_ptr`'s 32 bytes.
        cookie: [u8; NOTIFY_COOKIE_LEN],
    },
}

/// A registration for a queue's notification (`mq_notify`).
pub struct MqNotify {
    /// The process registered (`notify_owner`, a thread group).
    pub owner: u64,
    /// How it is told.
    pub how: MqNotifyHow,
}

/// One message on a queue.
struct MqMsg {
    data: Vec<u8>,
    /// Charged to its sender until it is received or the queue goes.
    _charge: Charge,
}

struct MqInner {
    /// By priority; each priority oldest first.
    queue: BTreeMap<u32, VecDeque<MqMsg>>,
    curmsgs: usize,
    /// Bytes on the queue: `QSIZE` of a `read`.
    qsize: usize,
    notify: Option<MqNotify>,
    /// Receivers asleep in [`PosixMq::wait_message`]: a message that comes
    /// while one is is theirs, not a notification's.
    receivers: usize,
}

/// One POSIX message queue.
pub struct PosixMq {
    /// Its inode number, for `fstat` and for telling queues apart.
    ino: usize,
    /// The name it was made with, without the slash: what `/proc/<pid>/fd`
    /// shows.
    name: String,
    /// Whether `mq_unlink` took the name away.
    unlinked: core::sync::atomic::AtomicBool,
    maxmsg: usize,
    msgsize: usize,
    uid: u32,
    gid: u32,
    mode: u32,
    /// `RLIMIT_MSGQUEUE` bytes counted against `ruid`, given back with the
    /// queue.
    ruid: u32,
    charged: usize,
    inner: Mutex<MqInner>,
    /// READABLE while a message is there, WRITABLE while there is room.
    bus: Arc<Mutex<EventBus>>,
}

/// The names, and what the queues cost.
///
/// Two locks, taken in this order and never the other way: a queue's `Drop`
/// gives back its share of [`ACCOUNT`], and a queue can be dropped with
/// [`NAMES`] held.
struct Account {
    /// Queues that exist, named or not (`mq_queues_count`).
    queues: usize,
    /// `RLIMIT_MSGQUEUE` bytes in use, by real user (`user_struct.mq_bytes`).
    bytes: BTreeMap<u32, usize>,
}

lazy_static! {
    static ref NAMES: Mutex<BTreeMap<String, Arc<PosixMq>>> = Mutex::new(BTreeMap::new());
    static ref ACCOUNT: Mutex<Account> = Mutex::new(Account {
        queues: 0,
        bytes: BTreeMap::new(),
    });
}

static NEXT_INO: AtomicUsize = AtomicUsize::new(1);

/// The name check of `mq_open` and `mq_unlink`, in Linux's order: `getname`
/// (empty: `ENOENT`; `PATH_MAX` or longer: `ENAMETOOLONG`), then
/// `lookup_one_len` (a `/` anywhere, `.` or `..`: `EACCES`), then
/// `simple_lookup` (past `NAME_MAX`: `ENAMETOOLONG`).
///
/// The name is what libc passes, which is what the program wrote with its
/// leading slash taken off: `"/q"` reaches the kernel as `"q"`, and a `"/"`
/// the kernel sees is a second one.
pub fn check_name(name: &str) -> LxResult<()> {
    let b = name.as_bytes();
    if b.is_empty() {
        return Err(LxError::ENOENT);
    }
    if b.len() >= MQ_PATH_MAX {
        return Err(LxError::ENAMETOOLONG);
    }
    if b == b"." || b == b".." || b.contains(&b'/') {
        return Err(LxError::EACCES);
    }
    if b.len() > MQ_NAME_MAX {
        return Err(LxError::ENAMETOOLONG);
    }
    Ok(())
}

/// `mq_attr_ok`: what a new queue may be made with.
fn attr_ok(attr: &MqAttr, sys_resource: bool) -> LxResult<(usize, usize)> {
    if attr.maxmsg <= 0 || attr.msgsize <= 0 {
        return Err(LxError::EINVAL);
    }
    let (maxmsg, msgsize) = (attr.maxmsg as usize, attr.msgsize as usize);
    let (most_msgs, most_size) = if sys_resource {
        (HARD_MSGMAX, HARD_MSGSIZEMAX)
    } else {
        (MSG_MAX, MSGSIZE_MAX)
    };
    if maxmsg > most_msgs || msgsize > most_size {
        return Err(LxError::EINVAL);
    }
    Ok((maxmsg, msgsize))
}

/// What a queue of this shape counts against `RLIMIT_MSGQUEUE`
/// (`mqueue_get_inode`): room for every message, and Linux's bookkeeping for
/// each.
pub fn rlimit_bytes(maxmsg: usize, msgsize: usize) -> usize {
    maxmsg * LINUX_MSG_MSG + maxmsg.min(MQ_PRIO_MAX) * LINUX_TREE_NODE + maxmsg * msgsize
}

/// Whether the permission bits `mode` of an object owned by `uid`/`gid` let
/// `cred` read (`want & 4`) and write (`want & 2`): `generic_permission`.
fn permits(mode: u32, uid: u32, gid: u32, cred: &MqCred, want: u32) -> bool {
    if cred.dac_override {
        return true;
    }
    let granted = if cred.euid == uid {
        mode >> 6
    } else if cred.egid == gid || cred.groups.contains(&gid) {
        mode >> 3
    } else {
        mode
    };
    want & !granted & 0o7 == 0
}

/// `O_ACCMODE` and the bits of `mq_open`'s `oflag` that matter here.
const O_ACCMODE: usize = 0o3;
const O_WRONLY: usize = 0o1;
const O_RDWR: usize = 0o2;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_NONBLOCK: usize = 0o4000;

/// `do_mq_open`: the queue `name` names, made first when `oflag` has
/// `O_CREAT` and there is none, and whether it was made.
///
/// `attr` is only looked at when a queue is made, as in Linux; `None` is the
/// defaults. An existing queue is checked for the access `oflag` asks
/// (`EACCES`), and an access mode of 3 is `EINVAL` there and nowhere else.
pub fn mq_open(
    name: &str,
    oflag: usize,
    mode: u32,
    attr: Option<MqAttr>,
    cred: &MqCred,
) -> LxResult<(Arc<PosixMq>, bool)> {
    check_name(name)?;
    let mut names = NAMES.lock();
    if let Some(queue) = names.get(name) {
        if oflag & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
            return Err(LxError::EEXIST);
        }
        let want = match oflag & O_ACCMODE {
            0 => 4,
            O_WRONLY => 2,
            O_RDWR => 6,
            _ => return Err(LxError::EINVAL),
        };
        if !permits(queue.mode, queue.uid, queue.gid, cred, want) {
            return Err(LxError::EACCES);
        }
        return Ok((queue.clone(), false));
    }
    if oflag & O_CREAT == 0 {
        return Err(LxError::ENOENT);
    }
    let queue = PosixMq::create(name, mode & !cred.umask & 0o7777, attr, cred)?;
    names.insert(String::from(name), queue.clone());
    Ok((queue, true))
}

/// Take back the name a [`mq_open`] just made, when the descriptor for it
/// could not be had (`EMFILE`): Linux takes the descriptor first and never
/// makes the queue.
pub fn mq_forget_created(queue: &Arc<PosixMq>) {
    let removed = {
        let mut names = NAMES.lock();
        match names.get(queue.name.as_str()) {
            Some(q) if Arc::ptr_eq(q, queue) => names.remove(queue.name.as_str()),
            _ => None,
        }
    };
    drop(removed);
}

/// `mq_unlink`: take the name away. The queue goes when its last descriptor
/// does. In a sticky directory owned by root, which is what the mqueue root
/// is, only the queue's owner or `CAP_FOWNER` may (`EPERM`).
pub fn mq_unlink(name: &str, cred: &MqCred) -> LxResult<()> {
    check_name(name)?;
    let removed = {
        let mut names = NAMES.lock();
        let queue = names.get(name).ok_or(LxError::ENOENT)?;
        if cred.euid != queue.uid && cred.euid != 0 && !cred.fowner {
            return Err(LxError::EPERM);
        }
        names.remove(name)
    };
    if let Some(queue) = &removed {
        queue.unlinked.store(true, Ordering::Relaxed);
    }
    // Dropped here, outside `NAMES`: see [`Account`].
    drop(removed);
    Ok(())
}

impl PosixMq {
    fn create(name: &str, mode: u32, attr: Option<MqAttr>, cred: &MqCred) -> LxResult<Arc<Self>> {
        let (maxmsg, msgsize) = match attr {
            Some(attr) => attr_ok(&attr, cred.sys_resource)?,
            None => (DFLT_MAXMSG, DFLT_MSGSIZE),
        };
        let charged = rlimit_bytes(maxmsg, msgsize);
        {
            let mut acct = ACCOUNT.lock();
            if acct.queues >= QUEUES_MAX && !cred.sys_resource {
                return Err(LxError::ENOSPC);
            }
            let used = acct.bytes.get(&cred.ruid).copied().unwrap_or(0);
            match used.checked_add(charged) {
                Some(total) if total <= cred.rlimit_msgqueue => {
                    acct.bytes.insert(cred.ruid, total);
                }
                _ => return Err(LxError::EMFILE),
            }
            acct.queues += 1;
        }
        let queue = PosixMq {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            name: String::from(name),
            unlinked: core::sync::atomic::AtomicBool::new(false),
            maxmsg,
            msgsize,
            uid: cred.euid,
            gid: cred.egid,
            mode,
            ruid: cred.ruid,
            charged,
            inner: Mutex::new(MqInner {
                queue: BTreeMap::new(),
                curmsgs: 0,
                qsize: 0,
                notify: None,
                receivers: 0,
            }),
            bus: EventBus::new(),
        };
        queue.bus.lock().set(Event::WRITABLE);
        Ok(Arc::new(queue))
    }

    /// The most messages it holds.
    pub fn maxmsg(&self) -> usize {
        self.maxmsg
    }

    /// The largest message it takes.
    pub fn msgsize(&self) -> usize {
        self.msgsize
    }

    /// Its `struct mq_attr`, with `flags` left for the descriptor to fill.
    pub fn attr(&self) -> MqAttr {
        MqAttr {
            flags: 0,
            maxmsg: self.maxmsg as isize,
            msgsize: self.msgsize as isize,
            curmsgs: self.inner.lock().curmsgs as isize,
            reserved: [0; 4],
        }
    }

    /// READABLE and WRITABLE from the counts. Called with `inner` held, after
    /// every change, so that a waiter that looks at the bus sees what the
    /// queue is.
    fn publish(&self, inner: &MqInner) {
        let mut bus = self.bus.lock();
        let (reset, set) = match (inner.curmsgs > 0, inner.curmsgs < self.maxmsg) {
            (true, true) => (Event::empty(), Event::READABLE | Event::WRITABLE),
            (true, false) => (Event::WRITABLE, Event::READABLE),
            (false, true) => (Event::READABLE, Event::WRITABLE),
            (false, false) => (Event::READABLE | Event::WRITABLE, Event::empty()),
        };
        bus.change(reset, set);
    }

    /// Put a message on the queue, or hand it back when the queue is full.
    ///
    /// The notification it fires, when it fires one, comes back to be sent
    /// by the caller with nothing held ([`deliver`]): the queue is no longer
    /// registered to anybody by then.
    pub fn try_send(
        &self,
        data: Vec<u8>,
        prio: u32,
        charge: Charge,
    ) -> Result<Option<MqNotify>, (Vec<u8>, Charge)> {
        let mut inner = self.inner.lock();
        if inner.curmsgs >= self.maxmsg {
            return Err((data, charge));
        }
        let len = data.len();
        inner.queue.entry(prio).or_default().push_back(MqMsg {
            data,
            _charge: charge,
        });
        inner.curmsgs += 1;
        inner.qsize += len;
        let fired = if inner.curmsgs == 1 && inner.receivers == 0 {
            inner.notify.take()
        } else {
            None
        };
        self.publish(&inner);
        Ok(fired)
    }

    /// Take the oldest message of the highest priority there is, with its
    /// priority.
    pub fn try_recv(&self) -> Option<(Vec<u8>, u32)> {
        let mut inner = self.inner.lock();
        let (&prio, msgs) = inner.queue.iter_mut().next_back()?;
        let msg = msgs.pop_front()?;
        if msgs.is_empty() {
            inner.queue.remove(&prio);
        }
        inner.curmsgs -= 1;
        inner.qsize -= msg.data.len();
        self.publish(&inner);
        Some((msg.data, prio))
    }

    /// Sleep until there is room, a signal comes (`EINTR`) or `deadline`
    /// passes (`ETIMEDOUT`). `deadline` is a `timer_now` instant.
    pub async fn wait_room(&self, deadline: Option<Duration>) -> LxResult<()> {
        wait_until(self.bus.clone(), Event::WRITABLE, deadline).await
    }

    /// Sleep until there is a message, a signal comes or `deadline` passes;
    /// counted as a receiver asleep while it sleeps.
    pub async fn wait_message(&self, deadline: Option<Duration>) -> LxResult<()> {
        struct Asleep<'a>(&'a PosixMq);
        impl Drop for Asleep<'_> {
            fn drop(&mut self) {
                self.0.inner.lock().receivers -= 1;
            }
        }
        self.inner.lock().receivers += 1;
        let _asleep = Asleep(self);
        wait_until(self.bus.clone(), Event::READABLE, deadline).await
    }

    /// `mq_notify` with a `sigevent`: register `owner`, or `EBUSY` when some
    /// process is registered already. A registration left by a process that
    /// has exited is no registration: its exit closed its descriptors.
    pub fn notify_register(&self, notify: MqNotify) -> LxResult<()> {
        let mut inner = self.inner.lock();
        if let Some(held) = &inner.notify {
            if owner_alive(held.owner) {
                return Err(LxError::EBUSY);
            }
        }
        inner.notify = Some(notify);
        Ok(())
    }

    /// `mq_notify(fd, NULL)` and `mqueue_flush_file`: take away `owner`'s
    /// registration, if it holds the queue's. What comes back is to be told
    /// it was removed ([`deliver_removed`]).
    pub fn notify_unregister(&self, owner: u64) -> Option<MqNotify> {
        let mut inner = self.inner.lock();
        if inner.notify.as_ref().map(|n| n.owner) == Some(owner) {
            inner.notify.take()
        } else {
            None
        }
    }

    /// What a `read` of the descriptor says (`mqueue_read_file`).
    pub fn status_line(&self) -> String {
        let inner = self.inner.lock();
        let (notify, signo, pid) = match &inner.notify {
            None => (0, 0, 0),
            Some(n) => match &n.how {
                MqNotifyHow::None => (1, 0, n.owner),
                MqNotifyHow::Signal { signo, .. } => (0, *signo, n.owner),
                MqNotifyHow::Thread { .. } => (2, 0, n.owner),
            },
        };
        alloc::format!(
            "QSIZE:{:<10} NOTIFY:{:<5} SIGNO:{:<5} NOTIFY_PID:{:<6}\n",
            inner.qsize,
            notify,
            signo,
            pid
        )
    }
}

impl Drop for PosixMq {
    fn drop(&mut self) {
        let mut acct = ACCOUNT.lock();
        acct.queues = acct.queues.saturating_sub(1);
        if let Some(used) = acct.bytes.get_mut(&self.ruid) {
            *used = used.saturating_sub(self.charged);
            if *used == 0 {
                acct.bytes.remove(&self.ruid);
            }
        }
    }
}

/// [`crate::sync::wait_for_event_until`], whose `EAGAIN` at the deadline is
/// a queue's `ETIMEDOUT`.
async fn wait_until(
    bus: Arc<Mutex<EventBus>>,
    mask: Event,
    deadline: Option<Duration>,
) -> LxResult<()> {
    match crate::sync::wait_for_event_until(bus, mask, deadline).await {
        Ok(_) => Ok(()),
        Err(LxError::EAGAIN) if deadline.is_some() => Err(LxError::ETIMEDOUT),
        Err(err) => Err(err),
    }
}

/// The `timer_now` instant an absolute `CLOCK_REALTIME` time `abs` falls
/// on, given the two clocks now: a time already past is now.
pub fn realtime_deadline(abs: Duration, wall_now: Duration, mono_now: Duration) -> Duration {
    mono_now + abs.saturating_sub(wall_now)
}

#[cfg(not(test))]
fn owner_alive(pid: u64) -> bool {
    crate::process::process_exists(pid as KoID)
}

#[cfg(test)]
fn owner_alive(pid: u64) -> bool {
    !tests::DEAD.with(|d| d.borrow().contains(&pid))
}

/// Tell the process a fired registration names, as `__do_notify` does: the
/// signal with `si_code` `SI_MESGQ`, the sender's pid and real uid and the
/// registration's `sigev_value`; or the cookie on the netlink socket.
pub fn deliver(notify: MqNotify, sender_pid: u32, sender_uid: u32) {
    match notify.how {
        MqNotifyHow::None => {}
        MqNotifyHow::Signal { signo, value } => {
            send_mesgq_signal(notify.owner, signo, sender_pid, sender_uid, value)
        }
        MqNotifyHow::Thread { sock, mut cookie } => {
            cookie[NOTIFY_COOKIE_LEN - 1] = NOTIFY_WOKENUP;
            send_cookie(&sock, cookie);
        }
    }
}

/// Tell a `SIGEV_THREAD` registration it was taken away
/// (`remove_notification`), so that libc's helper thread ends without
/// running the function. A signal registration is told nothing.
pub fn deliver_removed(notify: MqNotify) {
    if let MqNotifyHow::Thread { sock, mut cookie } = notify.how {
        cookie[NOTIFY_COOKIE_LEN - 1] = NOTIFY_REMOVED;
        send_cookie(&sock, cookie);
    }
}

#[cfg(not(test))]
fn send_mesgq_signal(owner: u64, signo: usize, pid: u32, uid: u32, value: usize) {
    use crate::signal::{SigInfo, Signal, SignalCode};
    let Ok(Some(signal)) = Signal::from_syscall_arg(signo) else {
        return;
    };
    let mut info = SigInfo::queued(signal, pid as i32, uid, value);
    info.code = SignalCode::MESGQ;
    // Nobody to tell is nothing to do: the queue is no longer registered
    // either way, as in Linux.
    let _ = crate::process::send_signal_to_process_with_info(owner as usize, signal, Some(info));
}

#[cfg(not(test))]
fn send_cookie(sock: &Arc<dyn FileLike>, cookie: [u8; NOTIFY_COOKIE_LEN]) {
    if let Some(nl) = sock.downcast_ref::<crate::net::NetlinkSocketState>() {
        nl.push_datagram(cookie.to_vec());
    }
}

#[cfg(test)]
fn send_mesgq_signal(owner: u64, signo: usize, pid: u32, uid: u32, value: usize) {
    tests::SENT.with(|s| s.borrow_mut().push((owner, signo, pid, uid, value)));
}

#[cfg(test)]
fn send_cookie(_sock: &Arc<dyn FileLike>, cookie: [u8; NOTIFY_COOKIE_LEN]) {
    tests::COOKIES.with(|c| c.borrow_mut().push(cookie));
}

/// An open message queue description: what `mq_open` puts in the
/// descriptor table.
pub struct MqFile {
    base: KObjectBase,
    queue: Arc<PosixMq>,
    /// `O_NONBLOCK` is the one that matters; `mq_setattr` and `F_SETFL` both
    /// change it.
    flags: Mutex<OpenFlags>,
    readable: bool,
    writable: bool,
    /// Where a `read` of the status line is.
    pos: Mutex<usize>,
}

impl_kobject!(MqFile);

impl MqFile {
    /// The description `mq_open` makes with `oflag`. An access mode of 3 is
    /// neither read nor write, as `OPEN_FMODE` makes it: Linux lets a queue
    /// be MADE that way.
    pub fn new(queue: Arc<PosixMq>, oflag: usize) -> Arc<Self> {
        let acc = oflag & O_ACCMODE;
        let mut flags = OpenFlags::from_bits_truncate(oflag & (O_ACCMODE | O_NONBLOCK));
        flags |= OpenFlags::CLOEXEC;
        Arc::new(MqFile {
            base: KObjectBase::new(),
            queue,
            flags: Mutex::new(flags),
            readable: acc == 0 || acc == O_RDWR,
            writable: acc == O_WRONLY || acc == O_RDWR,
            pos: Mutex::new(0),
        })
    }

    /// The queue.
    pub fn queue(&self) -> &Arc<PosixMq> {
        &self.queue
    }

    /// Whether it was opened for receiving (`FMODE_READ`).
    pub fn readable(&self) -> bool {
        self.readable
    }

    /// Whether it was opened for sending (`FMODE_WRITE`).
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Whether `O_NONBLOCK` is set on it.
    pub fn nonblock(&self) -> bool {
        self.flags.lock().non_block()
    }

    /// `mq_getsetattr`: the attributes, with `O_NONBLOCK` of this
    /// description, and then `O_NONBLOCK` set from `new` if there is one.
    pub fn getsetattr(&self, new: Option<&MqAttr>) -> MqAttr {
        let mut flags = self.flags.lock();
        let mut old = self.queue.attr();
        old.flags = if flags.non_block() {
            O_NONBLOCK as isize
        } else {
            0
        };
        if let Some(new) = new {
            flags.set(OpenFlags::NON_BLOCK, new.flags as usize & O_NONBLOCK != 0);
        }
        old
    }
}

#[async_trait]
impl FileLike for MqFile {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.flags.lock().take_settable(f);
        Ok(())
    }

    /// A regular file of the mqueue filesystem, with the queue's owner and
    /// mode, and `FILENT_SIZE` for a size.
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        let mut meta = anon_metadata(FileType::File, self.queue.ino);
        meta.mode = self.queue.mode as u16;
        meta.uid = self.queue.uid as usize;
        meta.gid = self.queue.gid as usize;
        meta.size = 80;
        Ok(meta)
    }

    /// `d_path` of a file on the internal mount: `/name`.
    fn link_name(&self) -> String {
        if self.queue.unlinked.load(Ordering::Relaxed) {
            alloc::format!("/{} (deleted)", self.queue.name)
        } else {
            alloc::format!("/{}", self.queue.name)
        }
    }

    /// `mqueue_read_file`: the status line, read like a file.
    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        let mut pos = self.pos.lock();
        let n = self.read_status(*pos, buf);
        *pos += n;
        Ok(n)
    }

    /// No `write` in `mqueue_file_operations`.
    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        Ok(self.read_status(offset as usize, buf))
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let curmsgs = self.queue.inner.lock().curmsgs;
        Ok(PollStatus {
            read: curmsgs > 0,
            write: curmsgs < self.queue.maxmsg,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        loop {
            let status = self.poll(events)?;
            let (want_read, want_write) = (events.wants_read(), events.wants_write());
            if (want_read && status.read)
                || (want_write && status.write)
                || (!want_read && !want_write)
            {
                return Ok(status);
            }
            let mut mask = Event::empty();
            if want_read {
                mask |= Event::READABLE;
            }
            if want_write {
                mask |= Event::WRITABLE;
            }
            crate::sync::wait_for_event(self.queue.bus.clone(), mask).await?;
        }
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(
            &self.queue.bus,
            mask,
            waker,
        ))
    }

    fn readiness_seq(&self, events: PollEvents) -> Option<u64> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(self.queue.bus.lock().seq_for(mask))
    }

    fn subscribe_edge(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
        seen: u64,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_edge_on(
            &self.queue.bus,
            mask,
            waker,
            seen,
        ))
    }
}

impl MqFile {
    fn read_status(&self, at: usize, buf: &mut [u8]) -> usize {
        let line = self.queue.status_line();
        let bytes = line.as_bytes();
        if at >= bytes.len() {
            return 0;
        }
        let n = buf.len().min(bytes.len() - at);
        buf[..n].copy_from_slice(&bytes[at..at + n]);
        n
    }
}

#[cfg(test)]
mod tests;
