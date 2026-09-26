//! System V message queues (msgget/msgsnd/msgrcv/msgctl).
//!
//! Queues live in a global table and — unlike the `Weak`-held semaphore keys —
//! are kept alive until `IPC_RMID`, matching sysvipc(7): a producer may write
//! and exit before the consumer ever attaches. Ids are global, like Linux's.

use super::{IpcGetFlag, IpcPerm};
use crate::error::LxError;
use crate::time::TimeSpec;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use lazy_static::lazy_static;
use lock::{Mutex, RwLock};

/// Largest single message, bytes (`MSGMAX`, Documentation/admin-guide/sysctl/kernel.rst).
pub const MSGMAX: usize = 8192;
/// Default queue capacity, bytes (`MSGMNB`).
pub const MSGMNB: usize = 16384;
/// How many queues may exist at once (`MSGMNI`, same file).
///
/// Linux answers `ENOSPC` past it, and the bound has to be there: a queue is
/// held by a **strong** `Arc` until `IPC_RMID`, so a loop of
/// `msgget(IPC_PRIVATE, IPC_CREAT)` from an unprivileged process pins kernel
/// memory that nothing will ever free. `shared_mem.rs` bounds its own table
/// at `SHMMNI` and its test says exactly why; this table had no bound at all.
pub const MSGMNI: usize = 32000;

/// How often a blocked `msgsnd`/`msgrcv` wakes up just to ask whether it
/// should still be waiting. The queue wakes its waiters itself the moment a
/// message lands, room appears or `IPC_RMID` runs; this tick is only a
/// ceiling on how late a `kill` can be noticed, the same figure (and the
/// same reasoning) as the semaphore and io-multiplex backstops.
const MSG_INTERRUPT_CHECK_TICK_MS: u64 = 100;

/// `struct msqid_ds` as 64-bit userland (musl/glibc `msqid64_ds`) lays it out.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MsqidDs {
    /// Ownership and permissions
    pub perm: IpcPerm,
    /// Time of last msgsnd
    pub stime: usize,
    /// Time of last msgrcv
    pub rtime: usize,
    /// Time of last change
    pub ctime: usize,
    /// Bytes currently queued
    pub cbytes: usize,
    /// Messages currently queued
    pub qnum: usize,
    /// Queue capacity in bytes
    pub qbytes: usize,
    /// pid of last msgsnd
    pub lspid: u32,
    /// pid of last msgrcv
    pub lrpid: u32,
    __unused: [usize; 2],
}

/// One queued message: its type tag and payload.
struct MsgItem {
    mtype: isize,
    data: Vec<u8>,
}

/// Outcome of a non-blocking send attempt.
pub enum MsgSendError {
    /// Queue was removed (`EIDRM` to the caller).
    Removed,
    /// Not enough room; a blocking caller parks on
    /// [`MsgQueue::wait_for_change`] with the generation carried here and
    /// retries.
    Full(u64),
}

/// Outcome of a non-blocking receive attempt.
pub enum MsgRecvError {
    /// Queue was removed (`EIDRM` to the caller).
    Removed,
    /// No message of the requested type; a blocking caller parks on
    /// [`MsgQueue::wait_for_change`] with the generation carried here.
    NoMsg(u64),
    /// First matching message is larger than the caller's buffer and
    /// `MSG_NOERROR` was not given (`E2BIG`).
    TooBig,
}

struct MsgQueueInner {
    ds: MsqidDs,
    messages: VecDeque<MsgItem>,
    /// Set by `IPC_RMID`: blocked senders/receivers wake into `EIDRM`.
    removed: bool,
    /// Bumped on every change a blocked caller could care about: a message
    /// in, a message out, the queue removed. A waiter snapshots it with the
    /// `Full`/`NoMsg` it got and parks until it moves, so a change that lands
    /// between the failed attempt and the park is seen, not slept through.
    generation: u64,
    /// The wakers of everybody parked in [`MsgQueue::wait_for_change`], by
    /// subscription id.
    waiters: Vec<(u64, Waker)>,
    /// Next subscription id.
    next_waiter: u64,
}

impl MsgQueueInner {
    /// Something changed: move the generation on and wake every waiter.
    ///
    /// Every write to `messages` or to `removed` ends here. `msgsnd` and
    /// `msgrcv` used to sleep 5 ms and look again, so a receiver saw its
    /// message up to 5 ms late and a daemon parked in `msgrcv` woke two
    /// hundred times a second for nothing.
    fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        for (_, waker) in self.waiters.drain(..) {
            waker.wake();
        }
    }
}

/// A System V message queue.
pub struct MsgQueue {
    inner: Mutex<MsgQueueInner>,
}

lazy_static! {
    /// Global id → queue table. Strong references: queues survive their
    /// creator until `IPC_RMID`, per sysvipc(7).
    static ref MSG_QUEUES: RwLock<BTreeMap<usize, Arc<MsgQueue>>> =
        RwLock::new(BTreeMap::new());
}

/// The next id `msgget` hands out. Ids are **never** reused.
///
/// `(0..).find(|i| !queues.contains_key(i))` handed back the lowest free
/// number, so the id of a queue `IPC_RMID` had just retired came straight back
/// out of the next `msgget` -- and a process still holding the old number then
/// sent into a stranger's queue instead of getting `EIDRM`. Linux carries a
/// sequence number inside the id for exactly this (`ipc_buildid`), and
/// [`IpcPerm::__seq`] is the field it lives in; nothing here ever incremented
/// it. Counting up hands the same number out twice only after 2^64 queues.
///
/// It also ends the search. That `find` walked the table on every call, under
/// the table's own write lock, so filling the table was quadratic in the
/// number of queues -- which an unprivileged process chooses.
static NEXT_MSG_ID: AtomicUsize = AtomicUsize::new(0);

impl MsgQueue {
    fn new(key: u32, mode: u32, uid: u32, gid: u32) -> Self {
        MsgQueue {
            inner: Mutex::new(MsgQueueInner {
                ds: MsqidDs {
                    perm: IpcPerm {
                        key,
                        uid,
                        gid,
                        cuid: uid,
                        cgid: gid,
                        mode: mode & 0o777,
                        __seq: 0,
                        __pad1: 0,
                        __pad2: 0,
                    },
                    stime: 0,
                    rtime: 0,
                    ctime: TimeSpec::now().sec,
                    cbytes: 0,
                    qnum: 0,
                    qbytes: MSGMNB,
                    lspid: 0,
                    lrpid: 0,
                    __unused: [0; 2],
                },
                messages: VecDeque::new(),
                removed: false,
                generation: 0,
                waiters: Vec::new(),
                next_waiter: 0,
            }),
        }
    }

    /// Try to append a message. `Full` asks the caller to block and retry
    /// (the payload is only copied in on success); bookkeeping
    /// (`cbytes`/`qnum`/`stime`/`lspid`) matches msgsnd(2).
    pub fn try_send(&self, mtype: isize, data: &[u8], sender: u32) -> Result<(), MsgSendError> {
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(MsgSendError::Removed);
        }
        // msgsnd(2) blocks when adding the message would exceed msg_qbytes.
        // Checked: `qbytes` is a number a privileged `IPC_SET` chooses and
        // `cbytes` climbs to meet it, so this sum is not the kernel's to
        // assume fits -- a queue at the top of the range answers "full".
        let cbytes = match inner.ds.cbytes.checked_add(data.len()) {
            Some(total) if total <= inner.ds.qbytes => total,
            _ => return Err(MsgSendError::Full(inner.generation)),
        };
        // ...and Linux bounds the NUMBER of messages by `msg_qbytes` too
        // (`1 + q_qnum <= q_qbytes`, do_msgsnd in ipc/msg.c). Only the byte
        // count was measured here, and a zero-length message adds no bytes:
        // an unprivileged loop of `msgsnd(id, &buf, 0, 0)` grew the queue
        // without limit, a heap allocation per message, pinned until
        // `IPC_RMID`.
        if inner.ds.qnum >= inner.ds.qbytes {
            return Err(MsgSendError::Full(inner.generation));
        }
        inner.ds.cbytes = cbytes;
        inner.ds.qnum += 1;
        inner.ds.stime = TimeSpec::now().sec;
        inner.ds.lspid = sender;
        inner.messages.push_back(MsgItem {
            mtype,
            data: data.to_vec(),
        });
        inner.touch();
        Ok(())
    }

    /// Try to take the first message matching `msgtyp` under msgrcv(2)'s
    /// selection rules; on success returns `(mtype, payload)` already
    /// truncated to `max_size` when `noerror` allows it.
    pub fn try_recv(
        &self,
        msgtyp: isize,
        max_size: usize,
        noerror: bool,
        except: bool,
        receiver: u32,
    ) -> Result<(isize, Vec<u8>), MsgRecvError> {
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(MsgRecvError::Removed);
        }
        let idx = select_message(inner.messages.iter().map(|m| m.mtype), msgtyp, except)
            .ok_or(MsgRecvError::NoMsg(inner.generation))?;
        if inner.messages[idx].data.len() > max_size && !noerror {
            return Err(MsgRecvError::TooBig);
        }
        let mut msg = inner.messages.remove(idx).unwrap();
        inner.ds.cbytes -= msg.data.len();
        inner.ds.qnum -= 1;
        inner.ds.rtime = TimeSpec::now().sec;
        inner.ds.lrpid = receiver;
        inner.touch();
        msg.data.truncate(max_size);
        Ok((msg.mtype, msg.data))
    }

    /// Park until this queue may have changed since `since`, the generation a
    /// failed [`try_send`](Self::try_send) or [`try_recv`](Self::try_recv)
    /// carried: a message landed, one left, or the queue was removed
    /// (`EIDRM`). A caught signal ends the wait with its error (`EINTR`),
    /// which msgsnd(2) and msgrcv(2) both list. Resolves at once when the
    /// generation already moved, so the change that lands between the failed
    /// attempt and this call is a no-op rather than a missed wakeup.
    pub fn wait_for_change(&self, since: u64) -> impl Future<Output = Result<(), LxError>> + '_ {
        ChangeFuture {
            queue: self,
            since,
            sub_id: None,
            timer: None,
        }
    }

    /// How many waiters are parked on this queue (tests).
    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.inner.lock().waiters.len()
    }

    /// `IPC_STAT`: snapshot the queue's `msqid_ds`.
    pub fn stat(&self) -> MsqidDs {
        self.inner.lock().ds
    }

    /// `IPC_SET`: owner, permission bits and `msg_qbytes`, per msgctl(2).
    ///
    /// Only the owner, the creator or a privileged caller may do this; anyone
    /// else gets `EPERM`. Without that check the `msqid_ds` userspace handed
    /// in rewrote `uid` and `gid`, so naming the id was enough to own the
    /// queue -- see [`IpcPerm::may_control`].
    ///
    /// `qbytes` is what `try_send` measures the queue against, so an
    /// unprivileged caller may only lower it. Raising `msg_qbytes` above
    /// `MSGMNB` is `CAP_SYS_RESOURCE` on Linux, and without that a `qbytes` of
    /// `usize::MAX` turns the queue into unbounded pinned kernel memory, eight
    /// kilobytes per `msgsnd`.
    pub fn set(&self, new: &MsqidDs, euid: u32) -> Result<(), LxError> {
        let mut inner = self.inner.lock();
        if !inner.ds.perm.may_control(euid) {
            return Err(LxError::EPERM);
        }
        inner.ds.perm.uid = new.perm.uid;
        inner.ds.perm.gid = new.perm.gid;
        inner.ds.perm.mode = new.perm.mode & 0o777;
        inner.ds.qbytes = if euid == 0 {
            new.qbytes
        } else {
            new.qbytes.min(MSGMNB)
        };
        inner.ds.ctime = TimeSpec::now().sec;
        Ok(())
    }

    /// Whether `euid` may `IPC_SET` or `IPC_RMID` this queue.
    fn may_control(&self, euid: u32) -> bool {
        self.inner.lock().ds.perm.may_control(euid)
    }

    /// Whether `euid`/`egid` may use this queue for `want`: `IPC_W` to send,
    /// `IPC_R` to receive or to `IPC_STAT`. See [`IpcPerm::may_access`].
    pub fn may_access(&self, euid: u32, egid: u32, want: u32) -> bool {
        self.inner.lock().ds.perm.may_access(euid, egid, want)
    }

    fn mark_removed(&self) {
        let mut inner = self.inner.lock();
        inner.removed = true;
        inner.touch();
    }

    fn key(&self) -> u32 {
        self.inner.lock().ds.perm.key
    }
}

/// The future behind [`MsgQueue::wait_for_change`].
#[must_use = "futures do nothing unless polled/`await`-ed"]
struct ChangeFuture<'a> {
    queue: &'a MsgQueue,
    since: u64,
    sub_id: Option<u64>,
    /// Backstop: the queue wakes this for what happens to the QUEUE, and for
    /// nothing that happens to the waiter. Without a tick nothing re-polls
    /// it, so nothing checks for a signal, and a `msgrcv` on a queue nobody
    /// writes could not be killed either.
    timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
}

impl ChangeFuture<'_> {
    fn done(&mut self, out: Result<(), LxError>) -> Poll<Result<(), LxError>> {
        if let Some(id) = self.sub_id.take() {
            self.queue.inner.lock().waiters.retain(|(i, _)| *i != id);
        }
        kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
        Poll::Ready(out)
    }
}

impl Drop for ChangeFuture<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            self.queue.inner.lock().waiters.retain(|(i, _)| *i != id);
        }
        kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
    }
}

impl Future for ChangeFuture<'_> {
    type Output = Result<(), LxError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        {
            let mut inner = this.queue.inner.lock();
            if inner.removed {
                drop(inner);
                return this.done(Err(LxError::EIDRM));
            }
            if inner.generation != this.since {
                drop(inner);
                return this.done(Ok(()));
            }
            // Park: register (or refresh) our waker before letting go of the
            // lock, so a `touch` that runs right after cannot miss us.
            match this.sub_id {
                Some(id) => {
                    if let Some(slot) = inner.waiters.iter_mut().find(|(i, _)| *i == id) {
                        slot.1 = cx.waker().clone();
                    } else {
                        inner.waiters.push((id, cx.waker().clone()));
                    }
                }
                None => {
                    let id = inner.next_waiter;
                    inner.next_waiter += 1;
                    inner.waiters.push((id, cx.waker().clone()));
                    this.sub_id = Some(id);
                }
            }
        }
        // The queue reports only what the QUEUE does, so this is the only
        // thing here that can answer a signal or a kill.
        if let Err(err) = crate::process::check_signals() {
            return this.done(Err(err));
        }
        let deadline =
            kernel_hal::timer::deadline_after(Duration::from_millis(MSG_INTERRUPT_CHECK_TICK_MS));
        kernel_hal::timer_waker::ensure_timer_waker(&mut this.timer, deadline, cx);
        Poll::Pending
    }
}

/// msgrcv(2) message selection, factored out for unit testing:
/// - `msgtyp == 0`: first message;
/// - `msgtyp > 0`: first with that exact type — or, with `MSG_EXCEPT`, the
///   first with a *different* type;
/// - `msgtyp < 0`: the message with the lowest type ≤ `|msgtyp|`.
fn select_message(
    mut types: impl Iterator<Item = isize>,
    msgtyp: isize,
    except: bool,
) -> Option<usize> {
    if msgtyp == 0 {
        return types.next().map(|_| 0);
    }
    if msgtyp > 0 {
        if except {
            types
                .enumerate()
                .find(|&(_, t)| t != msgtyp)
                .map(|(i, _)| i)
        } else {
            types
                .enumerate()
                .find(|&(_, t)| t == msgtyp)
                .map(|(i, _)| i)
        }
    } else {
        // `-isize::MIN` does not fit in an `isize`, and the sign here is
        // userspace's to choose: `msgrcv(id, buf, sz, LONG_MIN, 0)` arrives as
        // this argument, unexamined. With `overflow-checks` that negation is a
        // kernel panic from an unprivileged syscall; without them it wraps
        // back to `isize::MIN`, and then nothing can match -- `msgsnd` refuses
        // a type below 1 -- so the caller blocks for ever on a queue that is
        // full of messages for it. Linux writes the case out on its own line,
        // and lands on the opposite answer, *every* message (`convert_mode`,
        // ipc/msg.c):
        //
        //     if (*msgtyp == LONG_MIN) *msgtyp = LONG_MAX;
        let bound = msgtyp.checked_neg().unwrap_or(isize::MAX);
        types
            .enumerate()
            .filter(|&(_, t)| t <= bound)
            .min_by_key(|&(_, t)| t)
            .map(|(i, _)| i)
    }
}

/// `msgget(2)`: resolve `key` to a queue id, creating the queue as the flags
/// demand. `key == 0` is `IPC_PRIVATE` (always a fresh queue).
pub fn msg_get(key: u32, flags: usize, uid: u32, gid: u32) -> Result<usize, LxError> {
    let flag = IpcGetFlag::from_bits_truncate(flags);
    let mut queues = MSG_QUEUES.write();
    if key != 0 {
        if let Some((&id, q)) = queues.iter().find(|(_, q)| q.key() == key) {
            if flag.contains(IpcGetFlag::CREAT) && flag.contains(IpcGetFlag::EXCLUSIVE) {
                return Err(LxError::EEXIST);
            }
            // The key resolved to somebody else's queue. Asking for a mode it
            // will not grant is EACCES, not a working id (`ipc_check_perms`).
            if !q.may_access(uid, gid, IpcPerm::requested_mode(flags)) {
                return Err(LxError::EACCES);
            }
            return Ok(id);
        }
        if !flag.contains(IpcGetFlag::CREAT) {
            return Err(LxError::ENOENT);
        }
    }
    if queues.len() >= MSGMNI {
        return Err(LxError::ENOSPC);
    }
    let id = NEXT_MSG_ID.fetch_add(1, Ordering::Relaxed);
    queues.insert(id, Arc::new(MsgQueue::new(key, flags as u32, uid, gid)));
    Ok(id)
}

/// Look up a queue by id.
pub fn msg_queue(id: usize) -> Option<Arc<MsgQueue>> {
    MSG_QUEUES.read().get(&id).cloned()
}

/// `ipc_get_maxidx` for the queue table: the highest INDEX in use, `None`
/// when there is no queue. Ids are handed out from 0 and never reused, so
/// the slot IS the id; `MSG_INFO` and `IPC_INFO` return this and `ipcs -q`
/// walks `MSG_STAT` up to it.
pub fn msg_max_index() -> Option<usize> {
    MSG_QUEUES.read().keys().next_back().copied()
}

/// `msgctl(idx, MSG_STAT, ..)`: the queue in slot `idx`, with the id the
/// call returns for it; `None` is `EINVAL`.
pub fn msg_at_index(idx: usize) -> Option<(usize, Arc<MsgQueue>)> {
    msg_queue(idx).map(|queue| (idx, queue))
}

/// For `MSG_INFO`: how many queues exist (`msgpool`), the bytes queued in
/// all of them (`msgmap`) and the messages (`msgtot`).
pub fn msg_totals() -> (usize, usize, usize) {
    let table = MSG_QUEUES.read();
    let (mut bytes, mut messages) = (0, 0);
    for queue in table.values() {
        let ds = queue.stat();
        bytes += ds.cbytes;
        messages += ds.qnum;
    }
    (table.len(), bytes, messages)
}

/// `IPC_RMID`: drop the queue from the table and wake blocked callers into
/// `EIDRM` via the `removed` latch (their `Arc` keeps the object alive until
/// they notice).
pub fn msg_remove(id: usize, euid: u32) -> Result<(), LxError> {
    let mut queues = MSG_QUEUES.write();
    let queue = queues.get(&id).cloned().ok_or(LxError::EINVAL)?;
    // Same rule as `IPC_SET`: destroying someone else's queue is `EPERM`, not
    // a thing any process that can guess an id gets to do (msgctl(2)).
    if !queue.may_control(euid) {
        return Err(LxError::EPERM);
    }
    queues.remove(&id);
    drop(queues);
    queue.mark_removed();
    Ok(())
}

/// `/proc/sysvipc/msg` (Documentation/filesystems/proc.rst): one line per
/// queue in the kernel's column layout, consumed by `ipcs -q`.
pub fn msg_proc_table() -> alloc::string::String {
    use core::fmt::Write as _;
    let mut out = alloc::string::String::from(
        "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n",
    );
    for (id, queue) in MSG_QUEUES.read().iter() {
        let ds = queue.stat();
        let _ = writeln!(
            out,
            "{:>10} {:>10} {:>5o} {:>11} {:>10} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10} {:>10}",
            ds.perm.key as i32,
            id,
            ds.perm.mode,
            ds.cbytes,
            ds.qnum,
            ds.lspid,
            ds.lrpid,
            ds.perm.uid,
            ds.perm.gid,
            ds.perm.cuid,
            ds.perm.cgid,
            ds.stime,
            ds.rtime,
            ds.ctime,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_first_message_for_type_zero() {
        assert_eq!(select_message([3, 1, 2].iter().copied(), 0, false), Some(0));
        assert_eq!(select_message(core::iter::empty(), 0, false), None);
    }

    #[test]
    fn select_exact_type_and_except() {
        let types = || [3isize, 1, 2].iter().copied();
        assert_eq!(select_message(types(), 2, false), Some(2));
        assert_eq!(select_message(types(), 9, false), None);
        // MSG_EXCEPT: first message whose type differs.
        assert_eq!(select_message(types(), 3, true), Some(1));
    }

    #[test]
    fn select_negative_takes_lowest_type_within_bound() {
        let types = || [5isize, 3, 4, 1].iter().copied();
        // |msgtyp| = 4: candidates {3,4,1}, lowest type wins (1, index 3).
        assert_eq!(select_message(types(), -4, false), Some(3));
        assert_eq!(select_message(types(), -1, false), Some(3));
        assert_eq!(select_message([5isize].iter().copied(), -4, false), None);
    }
}

/// Who may change a System V object, and what bounds the values `IPC_SET`
/// takes from userspace.
///
/// `msgctl(IPC_SET)` and `msgctl(IPC_RMID)` are the two operations Linux gates
/// on ownership (`ipcctl_obtain_check`, ipc/util.c). Neither was gated here,
/// in any of the three IPC classes, and the `msqid_ds` came in unexamined, so
/// a process that could name an id took the queue over, destroyed it, or gave
/// itself unbounded pinned kernel memory by raising `msg_qbytes`.
#[cfg(test)]
mod msg_control_tests {
    use super::*;
    use crate::ipc::{IPC_R, IPC_W};

    extern crate std;

    /// `IPC_CREAT`, as userspace spells it.
    const CREAT: usize = 0o1000;
    /// A pid; the field is only bookkeeping here.
    const PID: u32 = 7;
    /// The owner in these tests.
    const OWNER: u32 = 1000;
    /// Somebody else.
    const STRANGER: u32 = 1001;
    /// `euid == 0`.
    const ROOT: u32 = 0;

    /// `MSG_QUEUES` is process-wide and cargo runs a crate's tests in threads.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_queues() {
        MSG_QUEUES.write().clear();
    }

    /// busybox `ipcs -q`: `maxid = msgctl(0, MSG_INFO, &info)`, then
    /// `MSG_STAT` over `0..=maxid`. Neither command existed.
    #[test]
    fn the_index_walk_of_ipcs_finds_every_queue() {
        let _guard = test_lock();
        clear_queues();
        assert_eq!(msg_max_index(), None);
        assert_eq!(msg_totals(), (0, 0, 0));
        let a = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        let b = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        assert_eq!(msg_max_index(), Some(b));
        assert_eq!(msg_at_index(a).unwrap().0, a);
        assert_eq!(msg_at_index(b).unwrap().0, b);
        assert!(msg_at_index(b + 1).is_none(), "one past the last slot");
        assert_eq!(msg_totals(), (2, 0, 0));
        assert!(msg_queue(a).unwrap().try_send(1, &[0u8; 10], OWNER).is_ok());
        assert_eq!(msg_totals(), (2, 10, 1), "queues, bytes, messages");
        msg_remove(a, OWNER).unwrap();
        assert!(msg_at_index(a).is_none());
        assert_eq!(msg_max_index(), Some(b));
        assert_eq!(msg_totals(), (1, 0, 0));
    }

    /// A queue owned by `uid`, off the global table.
    fn owned_by(uid: u32) -> MsgQueue {
        MsgQueue::new(0, 0o600, uid, uid)
    }

    // ---------------------------------------------------------------- msgrcv

    /// `msgrcv(id, buf, sz, LONG_MIN, 0)`. The negative branch used to negate
    /// `msgtyp` outright: `isize::MIN` has no positive counterpart, so that is
    /// a kernel panic under `overflow-checks` and, without them, a bound of
    /// `isize::MIN` that no message can meet -- `msgsnd` refuses a type below
    /// 1 -- leaving the caller blocked for ever on a queue full of messages
    /// for it. Linux lands on the opposite answer: every message matches.
    #[test]
    fn select_negative_min_matches_every_message() {
        let types = || [5isize, 3, 4, 1].iter().copied();
        assert_eq!(select_message(types(), isize::MIN, false), Some(3));
        // Even a type as large as the bound would have been.
        assert_eq!(
            select_message([isize::MAX].iter().copied(), isize::MIN, false),
            Some(0)
        );
        assert_eq!(select_message(core::iter::empty(), isize::MIN, false), None);
    }

    /// `isize::MIN + 1` negates fine and is the first value that does, so the
    /// two branches meet here rather than at some arbitrary place.
    #[test]
    fn select_negative_just_above_min_still_bounds() {
        let types = || [isize::MAX, 3].iter().copied();
        assert_eq!(select_message(types(), isize::MIN + 1, false), Some(1));
        assert_eq!(
            select_message([isize::MAX].iter().copied(), isize::MIN + 1, false),
            Some(0)
        );
    }

    // -------------------------------------------------------------- may_control

    #[test]
    fn only_the_owner_the_creator_and_root_may_control() {
        let mut perm = IpcPerm {
            uid: OWNER,
            cuid: OWNER,
            ..IpcPerm::default()
        };
        assert!(perm.may_control(OWNER));
        assert!(perm.may_control(ROOT));
        assert!(!perm.may_control(STRANGER));
        // Handing ownership over keeps the creator's rights, per ipc/util.c.
        perm.uid = STRANGER;
        assert!(perm.may_control(OWNER));
        assert!(perm.may_control(STRANGER));
    }

    // ------------------------------------------------------------- IPC_SET

    /// The whole of the old `set`: uid, gid and mode straight out of the
    /// buffer userspace handed in, with nobody asked.
    #[test]
    fn ipc_set_from_a_stranger_is_eperm_and_changes_nothing() {
        let queue = owned_by(OWNER);
        let mut ds = queue.stat();
        ds.perm.uid = STRANGER;
        ds.perm.gid = STRANGER;
        ds.perm.mode = 0o666;
        assert_eq!(queue.set(&ds, STRANGER), Err(LxError::EPERM));
        let after = queue.stat();
        assert_eq!(after.perm.uid, OWNER);
        assert_eq!(after.perm.gid, OWNER);
        assert_eq!(after.perm.mode, 0o600);
    }

    #[test]
    fn ipc_set_from_the_owner_rewrites_owner_and_mode() {
        let queue = owned_by(OWNER);
        let mut ds = queue.stat();
        ds.perm.uid = STRANGER;
        ds.perm.gid = STRANGER;
        // Only the low nine bits are permission bits; the rest is userspace's
        // to send and the kernel's to drop.
        ds.perm.mode = 0o7654;
        assert_eq!(queue.set(&ds, OWNER), Ok(()));
        let after = queue.stat();
        assert_eq!(after.perm.uid, STRANGER);
        assert_eq!(after.perm.gid, STRANGER);
        assert_eq!(after.perm.mode, 0o654);
    }

    /// Having given the queue away, the creator may still take it back: that
    /// is what `cuid` is for, and it is a different arm of the check.
    #[test]
    fn the_creator_may_still_control_a_queue_it_gave_away() {
        let queue = owned_by(OWNER);
        let mut ds = queue.stat();
        ds.perm.uid = STRANGER;
        assert_eq!(queue.set(&ds, OWNER), Ok(()));
        assert_eq!(queue.stat().perm.uid, STRANGER);

        let mut back = queue.stat();
        back.perm.uid = OWNER;
        assert_eq!(queue.set(&back, OWNER), Ok(()));
        assert_eq!(queue.stat().perm.uid, OWNER);
        // And the stranger it was handed to is now the owner's business too.
        assert_eq!(queue.set(&back, STRANGER), Err(LxError::EPERM));
    }

    // -------------------------------------------------------------- qbytes

    /// `try_send` measures the queue against `qbytes`, so an `IPC_SET` with
    /// `qbytes = usize::MAX` used to turn a queue into unbounded pinned kernel
    /// memory, `MSGMAX` bytes per `msgsnd`. Raising `msg_qbytes` past `MSGMNB`
    /// is `CAP_SYS_RESOURCE` on Linux.
    #[test]
    fn an_unprivileged_ipc_set_cannot_raise_qbytes() {
        let queue = owned_by(OWNER);
        let mut ds = queue.stat();
        ds.qbytes = usize::MAX;
        assert_eq!(queue.set(&ds, OWNER), Ok(()));
        assert_eq!(queue.stat().qbytes, MSGMNB);
    }

    #[test]
    fn root_may_raise_qbytes_and_anyone_may_lower_it() {
        let queue = owned_by(OWNER);
        let mut up = queue.stat();
        up.qbytes = MSGMNB * 4;
        assert_eq!(queue.set(&up, ROOT), Ok(()));
        assert_eq!(queue.stat().qbytes, MSGMNB * 4);

        let mut down = queue.stat();
        down.qbytes = 100;
        assert_eq!(queue.set(&down, OWNER), Ok(()));
        assert_eq!(queue.stat().qbytes, 100);
    }

    /// The bound has to reach `try_send`, or it is bookkeeping.
    #[test]
    fn a_lowered_qbytes_fills_the_queue() {
        let queue = owned_by(OWNER);
        let mut ds = queue.stat();
        ds.qbytes = 4;
        assert_eq!(queue.set(&ds, OWNER), Ok(()));
        assert!(queue.try_send(1, &[0u8; 4], PID).is_ok());
        assert!(matches!(
            queue.try_send(1, &[0u8; 1], PID),
            Err(MsgSendError::Full(_))
        ));
    }

    /// With `qbytes` raised by root, `cbytes + data.len()` is an addition on
    /// numbers userspace chose: a queue near the top of the address space must
    /// answer "full", not overflow.
    #[test]
    fn a_send_that_would_overflow_the_byte_count_is_full() {
        let queue = owned_by(OWNER);
        {
            let mut inner = queue.inner.lock();
            inner.ds.qbytes = usize::MAX;
            inner.ds.cbytes = usize::MAX;
        }
        assert!(matches!(
            queue.try_send(1, &[0u8; 1], PID),
            Err(MsgSendError::Full(_))
        ));
    }

    // ------------------------------------------------------------- IPC_RMID

    #[test]
    fn ipc_rmid_from_a_stranger_is_eperm_and_leaves_the_queue_alive() {
        let _guard = test_lock();
        clear_queues();
        let id = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        assert_eq!(msg_remove(id, STRANGER), Err(LxError::EPERM));
        let queue = msg_queue(id).expect("queue destroyed by a stranger");
        // Not even latched as removed: a blocked sender would have woken into
        // EIDRM on a queue that is still there.
        assert!(queue.try_send(1, b"still here", PID).is_ok());
    }

    #[test]
    fn ipc_rmid_from_the_owner_removes_and_wakes_blocked_callers() {
        let _guard = test_lock();
        clear_queues();
        let id = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        let queue = msg_queue(id).unwrap();
        assert_eq!(msg_remove(id, OWNER), Ok(()));
        assert!(msg_queue(id).is_none());
        assert!(matches!(
            queue.try_send(1, b"gone", PID),
            Err(MsgSendError::Removed)
        ));
    }

    #[test]
    fn root_may_remove_a_queue_it_does_not_own() {
        let _guard = test_lock();
        clear_queues();
        let id = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        assert_eq!(msg_remove(id, ROOT), Ok(()));
        assert!(msg_queue(id).is_none());
    }

    #[test]
    fn ipc_rmid_of_an_id_that_names_nothing_is_einval() {
        let _guard = test_lock();
        clear_queues();
        assert_eq!(msg_remove(4242, ROOT), Err(LxError::EINVAL));
    }

    // ------------------------------------------------------------------ ids

    /// `(0..).find(|i| !queues.contains_key(i))` handed the retired id
    /// straight back out, so a process still holding the old number sent into
    /// a stranger's queue instead of getting `EIDRM`.
    #[test]
    fn an_id_is_never_handed_out_twice() {
        let _guard = test_lock();
        clear_queues();
        let first = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        assert_eq!(msg_remove(first, OWNER), Ok(()));
        let second = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        assert_ne!(second, first);
        assert!(msg_queue(first).is_none());
    }

    #[test]
    fn ipc_private_always_makes_a_fresh_queue() {
        let _guard = test_lock();
        clear_queues();
        let a = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        let b = msg_get(0, CREAT, OWNER, OWNER).unwrap();
        assert_ne!(a, b);
        assert_eq!(MSG_QUEUES.read().len(), 2);
    }

    // --------------------------------------------------------------- MSGMNI

    /// A queue is held by a strong `Arc` until `IPC_RMID`, so an unbounded
    /// table is pinned kernel memory an unprivileged loop chooses the size of.
    /// Filled by hand: going through `msg_get` 32000 times is the quadratic
    /// key scan, which is the other half of the finding.
    #[test]
    fn msgget_stops_at_msgmni() {
        let _guard = test_lock();
        clear_queues();
        {
            let mut queues = MSG_QUEUES.write();
            for i in 0..MSGMNI {
                queues.insert(i, Arc::new(MsgQueue::new(0, 0o600, OWNER, OWNER)));
            }
        }
        assert_eq!(msg_get(0, CREAT, OWNER, OWNER), Err(LxError::ENOSPC));
        clear_queues();
    }

    /// A full table still answers a lookup: `msgget(key, 0)` on a queue that
    /// already exists is not a create, and Linux does not refuse it.
    #[test]
    fn a_full_table_still_resolves_a_key_that_exists() {
        let _guard = test_lock();
        clear_queues();
        let id = {
            let mut queues = MSG_QUEUES.write();
            for i in 1..MSGMNI {
                queues.insert(i, Arc::new(MsgQueue::new(0, 0o600, OWNER, OWNER)));
            }
            queues.insert(0, Arc::new(MsgQueue::new(77, 0o600, OWNER, OWNER)));
            0
        };
        assert_eq!(msg_get(77, 0, OWNER, OWNER), Ok(id));
        clear_queues();
    }

    // ---------------------------------------------- who may USE the queue

    /// `msgget` resolving an existing key is `ipc_check_perms`: the queue may
    /// be somebody else's, and asking it for a mode it will not grant is
    /// EACCES. Nothing asked before, so naming the key was the whole check.
    #[test]
    fn msgget_refuses_a_key_that_belongs_to_somebody_else() {
        let _guard = test_lock();
        clear_queues();
        let id = msg_get(9901, CREAT | 0o600, OWNER, OWNER).unwrap();
        assert_eq!(msg_get(9901, 0o600, OWNER, OWNER), Ok(id), "its owner");
        assert_eq!(
            msg_get(9901, 0o600, STRANGER, STRANGER),
            Err(LxError::EACCES),
            "a stranger gets EACCES, not the id"
        );
        assert_eq!(
            msg_get(9901, 0o400, STRANGER, STRANGER),
            Err(LxError::EACCES)
        );
        assert_eq!(msg_get(9901, 0, ROOT, ROOT), Ok(id), "root passes");
        // A bare existence probe asks for no access at all, and Linux grants
        // it: `msgget(key, 0)` is how a program asks whether a queue is there.
        assert_eq!(msg_get(9901, 0, STRANGER, STRANGER), Ok(id));
        clear_queues();
    }

    /// ...and grants exactly what the mode grants.
    #[test]
    fn msgget_grants_a_stranger_what_the_mode_grants() {
        let _guard = test_lock();
        clear_queues();
        let id = msg_get(9902, CREAT | 0o644, OWNER, OWNER).unwrap();
        assert_eq!(msg_get(9902, 0o400, STRANGER, STRANGER), Ok(id));
        assert_eq!(
            msg_get(9902, 0o600, STRANGER, STRANGER),
            Err(LxError::EACCES),
            "0644 does not grant a stranger a write"
        );
        clear_queues();
    }

    /// The same question, asked of a queue this time rather than of a bare
    /// `IpcPerm`: `msgsnd` needs write, `msgrcv` and `IPC_STAT` need read.
    #[test]
    fn a_queue_answers_for_itself_who_may_send_and_who_may_receive() {
        let q = MsgQueue::new(0, 0o640, OWNER, OWNER);
        assert!(q.may_access(OWNER, OWNER, IPC_R | IPC_W));
        assert!(q.may_access(STRANGER, OWNER, IPC_R));
        assert!(!q.may_access(STRANGER, OWNER, IPC_W));
        assert!(!q.may_access(STRANGER, STRANGER, IPC_R));
    }
}

/// A blocked `msgsnd`/`msgrcv` used to sleep 5 ms and look again: a
/// receiver saw its message up to 5 ms late, and a daemon parked in `msgrcv`
/// woke two hundred times a second for nothing. Now the queue wakes its
/// waiters itself. And `try_send` measured the queue in bytes only, so a
/// zero-length message never filled it: an unprivileged loop of
/// `msgsnd(id, &buf, 0, 0)` grew the queue without limit.
///
/// The waits are driven one poll at a time with a counting waker rather than
/// `block_on`, ON PURPOSE: a mutation that stops the wait from resolving
/// would make `block_on` hang, and a hang is not a detected failure.
#[cfg(test)]
mod blocking_wait_tests {
    use super::*;
    use core::pin::pin;
    use core::sync::atomic::AtomicUsize;

    extern crate std;
    use std::sync::Arc as StdArc;
    use std::task::Wake;

    const PID: u32 = 7;
    const OWNER: u32 = 1000;
    const ROOT: u32 = 0;

    /// A waker that counts how many times it was woken.
    struct CountWaker(AtomicUsize);

    impl Wake for CountWaker {
        fn wake(self: StdArc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (StdArc<CountWaker>, Waker) {
        let count = StdArc::new(CountWaker(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        (count, waker)
    }

    fn wakes(count: &StdArc<CountWaker>) -> usize {
        count.0.load(Ordering::SeqCst)
    }

    /// A queue that takes `qbytes` bytes, and `qbytes` messages.
    fn queue_of(qbytes: usize) -> MsgQueue {
        let queue = MsgQueue::new(0, 0o600, OWNER, OWNER);
        let mut ds = queue.stat();
        ds.qbytes = qbytes;
        assert_eq!(queue.set(&ds, ROOT), Ok(()));
        queue
    }

    /// The generation a receive on an empty queue is told to wait from.
    fn empty_generation(queue: &MsgQueue) -> u64 {
        match queue.try_recv(0, 64, false, false, PID) {
            Err(MsgRecvError::NoMsg(since)) => since,
            _ => panic!("an empty queue must answer NoMsg"),
        }
    }

    // ------------------------------------------------------------- the bound

    /// Linux: `1 + q_qnum <= q_qbytes` (do_msgsnd). Three zero-byte messages
    /// fit a queue of three bytes; the fourth does not, whatever the byte
    /// count says.
    #[test]
    fn zero_length_messages_fill_a_queue_too() {
        let queue = queue_of(3);
        for _ in 0..3 {
            assert!(queue.try_send(1, &[], PID).is_ok());
        }
        assert!(
            matches!(queue.try_send(1, &[], PID), Err(MsgSendError::Full(_))),
            "the fourth zero-length message must find the queue full"
        );
        let ds = queue.stat();
        assert_eq!(ds.qnum, 3);
        assert_eq!(ds.cbytes, 0);
        // Taking one out makes room for one.
        assert!(queue.try_recv(0, 64, false, false, PID).is_ok());
        assert!(queue.try_send(1, &[], PID).is_ok());
    }

    // -------------------------------------------------------------- the wait

    /// A message that lands between the failed receive and the park must be
    /// seen on the first poll, not slept through: the generation the failed
    /// receive carried is older than the queue's.
    #[test]
    fn a_wait_on_a_stale_generation_resolves_at_once() {
        let queue = queue_of(MSGMNB);
        let since = empty_generation(&queue);
        assert!(queue.try_send(1, b"hi", PID).is_ok());
        let (_, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(queue.wait_for_change(since));
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "a change that already happened must be seen on the very first poll"
        );
    }

    /// The receiver's wait: parks while the queue is empty, is WOKEN (not
    /// polled by luck) when a message lands, and resolves on the next poll.
    #[test]
    fn a_receiver_parks_until_a_message_lands_and_is_woken() {
        let queue = queue_of(MSGMNB);
        let since = empty_generation(&queue);
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(queue.wait_for_change(since));

        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "nothing has changed yet"
        );
        assert_eq!(wakes(&count), 0);
        assert_eq!(queue.waiter_count(), 1, "the wait must be registered");

        assert!(queue.try_send(1, b"hi", PID).is_ok());
        assert_eq!(wakes(&count), 1, "a send must wake the parked receiver");
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "and the next poll must see the change"
        );
        // Whoever re-plans first finds the message still there.
        assert_eq!(queue.stat().qnum, 1);
    }

    /// The sender's wait: a full queue parks it, and a receive that makes
    /// room wakes it.
    #[test]
    fn a_sender_parks_on_a_full_queue_until_a_receive_makes_room() {
        let queue = queue_of(4);
        assert!(queue.try_send(1, &[0u8; 4], PID).is_ok());
        let since = match queue.try_send(1, &[0u8; 1], PID) {
            Err(MsgSendError::Full(since)) => since,
            _ => panic!("the queue must be full"),
        };
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(queue.wait_for_change(since));

        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(queue.try_recv(0, 64, false, false, PID).is_ok());
        assert_eq!(wakes(&count), 1, "a receive must wake the parked sender");
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        assert!(queue.try_send(1, &[0u8; 1], PID).is_ok(), "and now it fits");
    }

    /// `IPC_RMID` is the other way a blocked caller ends: woken, into
    /// `EIDRM`.
    #[test]
    fn removing_the_queue_wakes_a_waiter_into_eidrm() {
        let queue = queue_of(MSGMNB);
        let since = empty_generation(&queue);
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(queue.wait_for_change(since));

        assert!(fut.as_mut().poll(&mut cx).is_pending());
        queue.mark_removed();
        assert_eq!(wakes(&count), 1, "a removal must wake the waiter");
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Err(LxError::EIDRM))
        ));
        assert_eq!(
            queue.waiter_count(),
            0,
            "a resolved wait leaves nothing behind"
        );
    }

    /// A wait that is dropped while parked (the syscall was cancelled, the
    /// task torn down) takes its waker with it; otherwise every abandoned
    /// `msgrcv` leaves a dead entry the queue wakes for ever.
    #[test]
    fn a_dropped_wait_leaves_no_waker_behind() {
        let queue = queue_of(MSGMNB);
        let since = empty_generation(&queue);
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        {
            let mut fut = pin!(queue.wait_for_change(since));
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            assert_eq!(queue.waiter_count(), 1);
        }
        assert_eq!(
            queue.waiter_count(),
            0,
            "dropping the wait must unregister it"
        );
        assert!(queue.try_send(1, b"hi", PID).is_ok());
        assert_eq!(wakes(&count), 0, "nothing left to wake");
    }

    /// Two receivers parked on one queue: a send wakes both (each re-plans;
    /// one wins the message, the other parks again on the new generation).
    #[test]
    fn a_send_wakes_every_parked_receiver() {
        let queue = queue_of(MSGMNB);
        let since = empty_generation(&queue);
        let (count_a, waker_a) = counting_waker();
        let (count_b, waker_b) = counting_waker();
        let mut cx_a = Context::from_waker(&waker_a);
        let mut cx_b = Context::from_waker(&waker_b);
        let mut fut_a = pin!(queue.wait_for_change(since));
        let mut fut_b = pin!(queue.wait_for_change(since));
        assert!(fut_a.as_mut().poll(&mut cx_a).is_pending());
        assert!(fut_b.as_mut().poll(&mut cx_b).is_pending());
        assert_eq!(queue.waiter_count(), 2);

        assert!(queue.try_send(1, b"hi", PID).is_ok());
        assert_eq!((wakes(&count_a), wakes(&count_b)), (1, 1));
        assert!(matches!(
            fut_a.as_mut().poll(&mut cx_a),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            fut_b.as_mut().poll(&mut cx_b),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(queue.waiter_count(), 0);
    }
}
