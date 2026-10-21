//! System V message queues through the syscall: a message sent in one
//! process and taken in another, which message a receiver gets, and the two
//! parks a queue has.
//!
//! The four calls here are the second of `ipc.rs`'s three families, and like
//! the semaphores before them **no test named any of them**. What is covered
//! next door is pure: `msgrcv_copy_flags`, `msgrcv_bufsz`, `select_message`
//! and `may_access` all have their own tests, and not one of them can see
//! whether the syscall asks, in what order, or what it does with the answer.
//!
//! What this file is for:
//!
//! **An id means the same queue in every process**, and a message put in one
//! end comes out the other with its type and its bytes intact. A queue is
//! the one System V object whose whole purpose is to carry a payload across,
//! so the test that matters sends from one process and receives in another
//! and compares the bytes.
//!
//! **Order.** `msgsnd` judges the size before it reads the message and reads
//! the message before it looks the id up, so a size past `MSGMAX` is
//! `EINVAL` with a null pointer while the same pointer with a legal size is
//! `EFAULT`, whatever id it was aimed at. `msgrcv` is not the same shape:
//! it judges the size, looks the id up, checks the permission, and only
//! then looks at `MSG_COPY` -- so a queue the caller may not read answers
//! `EACCES` to a `MSG_COPY` that a readable queue answers `ENOSYS`.
//!
//! **Which message a receiver gets**, which is `msgtyp`'s whole job: zero is
//! the first in the queue, a positive type is that type (inverted by
//! `MSG_EXCEPT`), and a negative one is the LOWEST type at or below its
//! magnitude -- not the first that fits. `isize::MIN` is its own case,
//! because negating it does not fit in an `isize`; Linux turns it into
//! `LONG_MAX`, and the test sends a message that only that reading picks up.
//!
//! **That a message too big for the buffer is still in the queue
//! afterwards.** `E2BIG` is a refusal, not a delivery, and `MSG_NOERROR` is
//! the flag that turns it into a truncated delivery. The pair is the only
//! way to tell a kernel that refuses from one that drops.
//!
//! **The two parks.** A full queue parks the sender and a receive makes
//! room; an empty queue parks the receiver and a send fills it. Both need
//! two threads, and both are done here between two processes that share
//! nothing but the id.
//!
//! **The six numbers a queue keeps** -- how many messages, how many bytes,
//! who sent last, who received last, and when each happened -- which only
//! `IPC_STAT` can read, and which two processes make say something.
//!
//! Fixed pids live in 47_32x and 47_33x, keys in 47_320_0xx.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use core::time::Duration;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `IPC_CREAT`, `IPC_EXCL`, and the mode a queue is made with when the test
/// is not about modes.
const CREAT: usize = 0o1000;
const EXCL: usize = 0o2000;
const EVERYBODY: usize = 0o666;
/// The access a probe asks for. Zero would ask for nothing.
const READ_AND_WRITE: usize = 0o600;

/// An id the registry can never answer: ids are handed out counting up.
const NO_SUCH_QUEUE: usize = usize::MAX;
/// A `msgctl` command no `<linux/msg.h>` defines.
const A_COMMAND_OF_NOBODY: usize = 999;

/// Keys of this file's own, one queue each.
const KEY: usize = 47_320_000;

/// How long a probe keeps asking before it gives up.
const TRIES: usize = 300;
const A_MOMENT: Duration = Duration::from_millis(10);
/// How long a parked caller is watched getting nowhere.
const WHILE_IT_PARKS: usize = 60;

/// The payload every message below carries.
const A_PAYLOAD: usize = 8;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// What this process acts as from here on. The group goes first: dropping
/// the effective uid takes away the privilege `set_resgid` needs.
fn acting_as(sys: &Syscall<'_>, euid: u32, egid: u32, groups: &[u32]) {
    let lp = sys.linux_process();
    lp.set_groups(Vec::from(groups));
    lp.set_resgid(egid, egid, egid).unwrap();
    lp.set_resuid(euid, euid, euid).unwrap();
}

/// `struct msgbuf { long mtype; char mtext[]; }`, as userspace lays it out.
#[repr(C)]
struct AMessage {
    mtype: isize,
    text: [u8; A_PAYLOAD],
}

impl AMessage {
    fn new(mtype: isize, text: [u8; A_PAYLOAD]) -> Self {
        AMessage { mtype, text }
    }
    /// A buffer to receive into, with a pattern in it that no message
    /// carries, so a byte the kernel did not write is recognisable.
    fn empty() -> Self {
        AMessage::new(0, [0xee; A_PAYLOAD])
    }
    /// Where it is, as userspace names it.
    fn at(&self) -> usize {
        self as *const AMessage as usize
    }
    /// The type tag, which for a receive is what the kernel wrote here.
    fn mtype(&self) -> isize {
        self.mtype
    }
    /// The payload, which for a receive is what the kernel wrote here.
    fn text(&self) -> &[u8] {
        &self.text
    }
}

fn msgget(sys: &Syscall<'_>, key: usize, msgflg: usize) -> SysResult {
    sys.sys_msgget(key, msgflg)
}

async fn msgsnd(
    sys: &Syscall<'_>,
    id: usize,
    msg: &AMessage,
    msgsz: usize,
    msgflg: usize,
) -> SysResult {
    sys.sys_msgsnd(id, msg.at(), msgsz, msgflg).await
}

async fn msgrcv(
    sys: &Syscall<'_>,
    id: usize,
    into: &mut AMessage,
    msgsz: usize,
    msgtyp: isize,
    msgflg: usize,
) -> SysResult {
    let at = into.at();
    sys.sys_msgrcv(id, at, msgsz, msgtyp, msgflg).await
}

fn msgctl(sys: &Syscall<'_>, id: usize, cmd: usize, buf: usize) -> SysResult {
    sys.sys_msgctl(id, cmd, buf)
}

/// The `Syscall` the harness lends, as a `&mut` one. Every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A call reached through the syscall table instead of by name.
async fn by_number(sys: &Syscall<'_>, which: crate::Sys, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

/// A `msqid_ds` to hand the kernel as a buffer.
///
/// The struct has padding a test cannot name, so one cannot be built here;
/// this takes a copy of ANOTHER queue's, which also makes a command that
/// writes nothing leave recognisably the wrong queue's numbers behind.
fn a_msqid_ds_of_another_queue(id: usize) -> MsqidDs {
    msg_queue(id).unwrap().stat()
}

/// Send one message of `A_PAYLOAD` bytes.
async fn send(sys: &Syscall<'_>, id: usize, mtype: isize, text: [u8; A_PAYLOAD]) -> SysResult {
    let msg = AMessage::new(mtype, text);
    msgsnd(sys, id, &msg, A_PAYLOAD, 0).await
}

/// Take one message, answering its type and its bytes.
async fn take(
    sys: &Syscall<'_>,
    id: usize,
    msgtyp: isize,
    msgflg: usize,
) -> Result<(isize, [u8; A_PAYLOAD]), LxError> {
    let mut into = AMessage::empty();
    msgrcv(sys, id, &mut into, A_PAYLOAD, msgtyp, msgflg).await?;
    let mut text = [0u8; A_PAYLOAD];
    text.copy_from_slice(into.text());
    Ok((into.mtype(), text))
}

/// A thread of `proc` parked in a send, on its own host thread.
fn a_parked_sender(
    proc: &Arc<Process>,
    id: usize,
    mtype: isize,
    text: [u8; A_PAYLOAD],
) -> std::thread::JoinHandle<SysResult> {
    let proc = proc.clone();
    std::thread::spawn(move || {
        on_a_thread(&proc, move |sys| {
            Box::pin(async move { send(sys, id, mtype, text).await })
        })
    })
}

/// A thread of `proc` parked in a receive, on its own host thread.
fn a_parked_receiver(
    proc: &Arc<Process>,
    id: usize,
    msgtyp: isize,
) -> std::thread::JoinHandle<Result<(isize, [u8; A_PAYLOAD]), LxError>> {
    let proc = proc.clone();
    std::thread::spawn(move || {
        on_a_thread(&proc, move |sys| {
            Box::pin(async move { take(sys, id, msgtyp, 0).await })
        })
    })
}

/// Watch the queue hold exactly `qnum` messages for a while, which is both
/// the time a parked caller needs and the proof that it got nowhere.
async fn nothing_moves_while_it_parks(id: usize, qnum: usize) {
    for _ in 0..WHILE_IT_PARKS {
        assert_eq!(
            msg_queue(id).unwrap().stat().qnum,
            qnum,
            "the queue moved while nobody had acted on it"
        );
        async_std::task::sleep(A_MOMENT).await;
    }
}

/// Wait for the queue to hold `qnum` messages, which is how the test sees
/// that the parked caller woke and did its work.
async fn the_queue_settles_at(id: usize, qnum: usize) -> bool {
    for _ in 0..TRIES {
        if msg_queue(id).unwrap().stat().qnum == qnum {
            return true;
        }
        async_std::task::sleep(A_MOMENT).await;
    }
    false
}

/// A queue made by one process is the same queue in another, and a message
/// crosses between them whole.
///
/// The id is system-wide and the registry holds the queue, so neither
/// process needs to be related to the other -- which is the case a message
/// queue exists for. The test is the only one here that proves what a queue
/// is FOR: the type tag and all eight bytes come out the other end as they
/// went in.
#[test]
fn a_queue_made_by_one_process_carries_a_message_to_another() {
    const MINE: usize = KEY + 1;
    const FREE: usize = KEY + 2;
    const A_TYPE: isize = 42;
    const A_TEXT: [u8; A_PAYLOAD] = [1, 2, 3, 4, 250, 251, 252, 253];

    let sender = a_process(47_320, "msg-across-sender");
    let id = on_a_thread(&sender, move |sys| {
        Box::pin(async move {
            assert_eq!(
                msgget(sys, FREE, 0),
                Err(LxError::ENOENT),
                "a key that names no queue was not ENOENT"
            );
            let id = msgget(sys, MINE, CREAT | EVERYBODY).unwrap();
            assert_eq!(
                msgget(sys, MINE, CREAT | EVERYBODY),
                Ok(id),
                "the same key asked for again answered with another id"
            );
            assert_eq!(
                msgget(sys, MINE, CREAT | EXCL | EVERYBODY),
                Err(LxError::EEXIST),
                "IPC_EXCL on a key that already names a queue was not EEXIST"
            );
            assert_eq!(send(sys, id, A_TYPE, A_TEXT).await, Ok(0));
            id
        })
    });

    let receiver = a_process(47_321, "msg-across-receiver");
    on_a_thread(&receiver, move |sys| {
        Box::pin(async move {
            assert_eq!(
                msgget(sys, MINE, READ_AND_WRITE),
                Ok(id),
                "the key named a different queue in another process"
            );
            assert_eq!(
                take(sys, id, 0, IPC_NOWAIT).await,
                Ok((A_TYPE, A_TEXT)),
                "the message did not cross whole"
            );
            // And it was the only one.
            assert_eq!(
                take(sys, id, 0, IPC_NOWAIT).await,
                Err(LxError::ENOMSG),
                "the queue gave out a message twice"
            );
            // An id nobody has been given names nothing.
            assert_eq!(
                take(sys, NO_SUCH_QUEUE, 0, IPC_NOWAIT).await,
                Err(LxError::EINVAL),
                "an id that names no queue was received from"
            );
        })
    });
}

/// `msgsnd` judges the size before it reads the message, and reads the
/// message before it looks the id up.
///
/// Both of the first two refusals are answered with a null pointer, and the
/// third call shows that pointer would be `EFAULT` if it were read first. The
/// id coming last is the other half: a message too big is refused whatever
/// queue it was aimed at, and a bad pointer is `EFAULT` even for an id that
/// names nothing.
#[test]
fn the_size_of_a_message_is_judged_before_the_message_is_read() {
    const MINE: usize = KEY + 3;
    let proc = a_process(47_322, "msgsnd-order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = msgget(sys, MINE, CREAT | EVERYBODY).unwrap();
            let nothing = AMessage::new(0, [0; A_PAYLOAD]);
            // A message at address zero, which is the one address `UserInPtr`
            // refuses on its own.
            let at_nothing = |sz, flg| sys.sys_msgsnd(id, 0, sz, flg);

            assert_eq!(
                at_nothing(linux_object::ipc::MSGMAX + 1, 0).await,
                Err(LxError::EINVAL),
                "a message past MSGMAX was not EINVAL"
            );
            assert_eq!(
                sys.sys_msgsnd(NO_SUCH_QUEUE, 0, linux_object::ipc::MSGMAX + 1, 0)
                    .await,
                Err(LxError::EINVAL),
                "the size is judged before the id, so this is the same answer"
            );
            // The same pointer with a size the kernel will take: now it IS
            // read, and there is nothing at it.
            assert_eq!(
                at_nothing(1, 0).await,
                Err(LxError::EFAULT),
                "a null message was read anyway"
            );
            assert_eq!(
                sys.sys_msgsnd(NO_SUCH_QUEUE, 0, 1, 0).await,
                Err(LxError::EFAULT),
                "the id was looked up before the message was read"
            );

            // The type is read out of the message and judged: msgsnd(2)
            // wants it strictly positive, and a message with a type below
            // one is refused rather than queued under a type no `msgrcv`
            // can name.
            for bad in [0isize, -1, isize::MIN] {
                let msg = AMessage::new(bad, [7; A_PAYLOAD]);
                assert_eq!(
                    msgsnd(sys, id, &msg, A_PAYLOAD, 0).await,
                    Err(LxError::EINVAL),
                    "a message of type {} was queued",
                    bad
                );
            }
            assert_eq!(
                msg_queue(id).unwrap().stat().qnum,
                0,
                "a refused message was queued anyway"
            );
            // And one is the smallest type that works.
            let good = AMessage::new(1, [7; A_PAYLOAD]);
            assert_eq!(
                msgsnd(sys, id, &good, A_PAYLOAD, 0).await,
                Ok(0),
                "type one was refused"
            );
            // A zero-length message is legal and carries no bytes.
            assert_eq!(
                msgsnd(sys, id, &nothing_typed(), 0, 0).await,
                Ok(0),
                "a zero-length message was refused"
            );
            let _ = &nothing;
        })
    });
}

/// A message of type one and no payload, for the zero-length case.
fn nothing_typed() -> AMessage {
    AMessage::new(1, [0; A_PAYLOAD])
}

/// `msgrcv` looks the queue up and checks the permission before it looks at
/// `MSG_COPY`, which is not the shape `msgsnd` has.
///
/// `MSG_COPY` is the flag this kernel does not serve, and what it answers
/// says where in the call it was noticed: `EACCES` from a queue the caller
/// may not read, `ENOSYS` from one it may, and `EINVAL` for the two
/// combinations Linux refuses ahead of the `ENOSYS`. Leaving the bit
/// unchecked used to dequeue the message and answer success, which is a lost
/// message rather than an error.
#[test]
fn a_copy_this_kernel_does_not_serve_is_noticed_after_the_permission_is_checked() {
    const READABLE: usize = KEY + 4;
    const UNREADABLE: usize = KEY + 5;
    const A_USER: u32 = 1_000;
    const MSG_COPY: usize = 0o40000;
    const MSG_EXCEPT: usize = 0o20000;
    let proc = a_process(47_323, "msgrcv-copy");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            let readable = msgget(sys, READABLE, CREAT | 0o444).unwrap();
            let unreadable = msgget(sys, UNREADABLE, CREAT | 0o222).unwrap();
            let mut into = AMessage::empty();

            // The two illegal combinations come first, and they are the
            // caller's mistake rather than the kernel's gap.
            assert_eq!(
                msgrcv(sys, readable, &mut into, A_PAYLOAD, 0, MSG_COPY).await,
                Err(LxError::EINVAL),
                "MSG_COPY without IPC_NOWAIT was not EINVAL"
            );
            assert_eq!(
                msgrcv(
                    sys,
                    readable,
                    &mut into,
                    A_PAYLOAD,
                    0,
                    MSG_COPY | IPC_NOWAIT | MSG_EXCEPT
                )
                .await,
                Err(LxError::EINVAL),
                "MSG_COPY with MSG_EXCEPT was not EINVAL"
            );
            // Legal, and this kernel has no such thing.
            assert_eq!(
                msgrcv(
                    sys,
                    readable,
                    &mut into,
                    A_PAYLOAD,
                    0,
                    MSG_COPY | IPC_NOWAIT
                )
                .await,
                Err(LxError::ENOSYS),
                "MSG_COPY was served"
            );
            // The same flags on a queue this caller may not read: the
            // permission is checked first, so the answer is EACCES.
            assert_eq!(
                msgrcv(
                    sys,
                    unreadable,
                    &mut into,
                    A_PAYLOAD,
                    0,
                    MSG_COPY | IPC_NOWAIT
                )
                .await,
                Err(LxError::EACCES),
                "MSG_COPY was noticed before the permission was checked"
            );
            // A buffer size that is negative as a long is refused before any
            // of that, which is what stops it becoming an unlimited buffer
            // in which every message fits.
            assert_eq!(
                msgrcv(sys, readable, &mut into, usize::MAX, 0, IPC_NOWAIT).await,
                Err(LxError::EINVAL),
                "a buffer size with the high bit set was taken"
            );
            assert_eq!(
                msgrcv(sys, readable, &mut into, 0, 0, IPC_NOWAIT).await,
                Err(LxError::ENOMSG),
                "a buffer size of zero is legal and the queue is empty"
            );
        })
    });
}

/// Which message a receiver gets is what `msgtyp` says, and the three
/// readings of it are three different answers to the same queue.
///
/// Zero is the first message in the queue, a positive type is that type
/// (inverted by `MSG_EXCEPT`), and a NEGATIVE one is the lowest type at or
/// below its magnitude -- not the first that fits, which is the easy thing
/// to write and the wrong answer. `isize::MIN` is its own case: negating it
/// does not fit in an `isize`, so without a line of its own it either
/// panics the kernel from an unprivileged syscall or wraps back and matches
/// nothing, leaving the caller blocked for ever on a queue full of messages
/// for it. Linux turns it into `LONG_MAX`, which means every message.
#[test]
fn which_message_a_receiver_gets_is_what_msgtyp_says() {
    const MINE: usize = KEY + 6;
    let proc = a_process(47_324, "msgrcv-select");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = msgget(sys, MINE, CREAT | EVERYBODY).unwrap();
            // Deliberately not in order, so "the first" and "the lowest" are
            // different messages.
            assert_eq!(send(sys, id, 3, [3; A_PAYLOAD]).await, Ok(0));
            assert_eq!(send(sys, id, 1, [1; A_PAYLOAD]).await, Ok(0));
            assert_eq!(send(sys, id, 2, [2; A_PAYLOAD]).await, Ok(0));

            // Zero: the first one in, which is not the lowest type.
            assert_eq!(
                take(sys, id, 0, IPC_NOWAIT).await,
                Ok((3, [3; A_PAYLOAD])),
                "a msgtyp of zero did not take the first message"
            );
            // A positive type: that type, and not the first one left.
            assert_eq!(
                take(sys, id, 2, IPC_NOWAIT).await,
                Ok((2, [2; A_PAYLOAD])),
                "a positive msgtyp did not take its own type"
            );
            assert_eq!(
                take(sys, id, 9, IPC_NOWAIT).await,
                Err(LxError::ENOMSG),
                "a type nothing in the queue carries was answered"
            );
            assert_eq!(take(sys, id, 0, IPC_NOWAIT).await, Ok((1, [1; A_PAYLOAD])));

            // MSG_EXCEPT inverts a positive type.
            const MSG_EXCEPT: usize = 0o20000;
            assert_eq!(send(sys, id, 5, [5; A_PAYLOAD]).await, Ok(0));
            assert_eq!(send(sys, id, 6, [6; A_PAYLOAD]).await, Ok(0));
            assert_eq!(
                take(sys, id, 5, IPC_NOWAIT | MSG_EXCEPT).await,
                Ok((6, [6; A_PAYLOAD])),
                "MSG_EXCEPT took the type it was told to skip"
            );
            assert_eq!(take(sys, id, 0, IPC_NOWAIT).await, Ok((5, [5; A_PAYLOAD])));

            // A negative type: the LOWEST at or below its magnitude. The
            // queue is loaded highest first, so "the first that fits" and
            // "the lowest" are different messages again.
            assert_eq!(send(sys, id, 8, [8; A_PAYLOAD]).await, Ok(0));
            assert_eq!(send(sys, id, 4, [4; A_PAYLOAD]).await, Ok(0));
            assert_eq!(
                take(sys, id, -9, IPC_NOWAIT).await,
                Ok((4, [4; A_PAYLOAD])),
                "a negative msgtyp took the first message that fit, not the lowest"
            );
            assert_eq!(
                take(sys, id, -7, IPC_NOWAIT).await,
                Err(LxError::ENOMSG),
                "a negative msgtyp took a type above its magnitude"
            );

            // And the one value whose negation does not exist.
            assert_eq!(
                take(sys, id, isize::MIN, IPC_NOWAIT).await,
                Ok((8, [8; A_PAYLOAD])),
                "the lowest possible msgtyp did not mean every message"
            );
        })
    });
}

/// A message too big for the buffer is refused and STAYS in the queue, and
/// `MSG_NOERROR` is what turns the refusal into a truncated delivery.
///
/// The two halves are the only way to tell a kernel that refuses from one
/// that drops: both answer the caller, and only the queue afterwards says
/// which happened.
#[test]
fn a_message_too_big_for_the_buffer_stays_in_the_queue() {
    const MINE: usize = KEY + 7;
    const MSG_NOERROR: usize = 0o10000;
    const HALF: usize = A_PAYLOAD / 2;
    const A_TEXT: [u8; A_PAYLOAD] = [10, 11, 12, 13, 14, 15, 16, 17];
    let proc = a_process(47_325, "msgrcv-toobig");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = msgget(sys, MINE, CREAT | EVERYBODY).unwrap();
            assert_eq!(send(sys, id, 1, A_TEXT).await, Ok(0));

            let mut into = AMessage::empty();
            assert_eq!(
                msgrcv(sys, id, &mut into, HALF, 0, IPC_NOWAIT).await,
                Err(LxError::E2BIG),
                "an oversized message was delivered without MSG_NOERROR"
            );
            assert_eq!(
                msg_queue(id).unwrap().stat().qnum,
                1,
                "a message refused with E2BIG was taken out of the queue"
            );
            assert_eq!(
                into.text(),
                &[0xee; A_PAYLOAD],
                "a refused receive wrote into the caller's buffer"
            );

            // The whole buffer takes it, bytes and all.
            assert_eq!(
                take(sys, id, 0, IPC_NOWAIT).await,
                Ok((1, A_TEXT)),
                "the message left behind by the E2BIG was not the one sent"
            );

            // And with MSG_NOERROR the same short buffer gets the front of
            // the message, the message goes, and the rest of the buffer is
            // left alone.
            assert_eq!(send(sys, id, 1, A_TEXT).await, Ok(0));
            let mut short = AMessage::empty();
            assert_eq!(
                msgrcv(sys, id, &mut short, HALF, 0, IPC_NOWAIT | MSG_NOERROR).await,
                Ok(HALF),
                "MSG_NOERROR did not truncate"
            );
            assert_eq!(
                &short.text()[..HALF],
                &A_TEXT[..HALF],
                "the truncated message is not the front of what was sent"
            );
            assert_eq!(
                &short.text()[HALF..],
                &[0xee; HALF],
                "a truncated receive wrote past the size it was given"
            );
            assert_eq!(short.mtype(), 1, "the type did not come back");
            assert_eq!(
                msg_queue(id).unwrap().stat().qnum,
                0,
                "a truncated delivery left the message in the queue"
            );
        })
    });
}

/// Sending needs write and receiving needs read, and a queue that grants
/// one and not the other is what makes the question visible.
///
/// Nothing asked at all before: `msgsnd`, `msgrcv` and every `IPC_STAT` went
/// straight through, so knowing the id was the whole check. Each refusal
/// below is a call that would otherwise have had an answer -- the sends are
/// legal messages and the receives land on a queue with something in it.
#[test]
fn sending_needs_write_and_receiving_needs_read() {
    const READABLE: usize = KEY + 8;
    const WRITABLE: usize = KEY + 9;
    const A_USER: u32 = 1_000;
    let proc = a_process(47_326, "msg-access");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            let readable = msgget(sys, READABLE, CREAT | 0o444).unwrap();
            let writable = msgget(sys, WRITABLE, CREAT | 0o222).unwrap();

            // Read only: a receive gets as far as finding the queue empty,
            // and a send does not get that far.
            assert_eq!(
                take(sys, readable, 0, IPC_NOWAIT).await,
                Err(LxError::ENOMSG),
                "receiving needed more than read"
            );
            assert_eq!(
                send(sys, readable, 1, [1; A_PAYLOAD]).await,
                Err(LxError::EACCES),
                "a read-only queue took a message"
            );
            assert_eq!(
                msg_queue(readable).unwrap().stat().qnum,
                0,
                "the refused message was queued anyway"
            );

            // Write only: the mirror, and the receive that is refused would
            // have had a message waiting for it.
            assert_eq!(
                send(sys, writable, 1, [1; A_PAYLOAD]).await,
                Ok(0),
                "a write-only queue refused a message"
            );
            assert_eq!(
                msg_queue(writable).unwrap().stat().qnum,
                1,
                "the message did not reach the write-only queue"
            );
            assert_eq!(
                take(sys, writable, 0, IPC_NOWAIT).await,
                Err(LxError::EACCES),
                "a write-only queue gave out a message"
            );
            // IPC_STAT reads, so it needs read too.
            let mut ds = a_msqid_ds_of_another_queue(readable);
            assert_eq!(
                msgctl(sys, writable, IPC_STAT, &mut ds as *mut MsqidDs as usize),
                Err(LxError::EACCES),
                "a write-only queue answered IPC_STAT"
            );
        })
    });
}

/// A full queue parks the sender until a receive makes room, and an empty
/// one parks the receiver until a send fills it.
///
/// Both pairs are made between two processes that share nothing but the id,
/// and in both the parked caller is watched getting nowhere first, so what
/// ends the wait is really the other process's call. The capacity is lowered
/// to eight bytes with `IPC_SET` rather than filled with two megabytes.
#[test]
fn a_full_queue_parks_the_sender_and_an_empty_one_parks_the_receiver() {
    const MINE: usize = KEY + 10;
    const FIRST: [u8; A_PAYLOAD] = [21; A_PAYLOAD];
    const SECOND: [u8; A_PAYLOAD] = [22; A_PAYLOAD];

    let maker = a_process(47_327, "msg-park-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move {
            let id = msgget(sys, MINE, CREAT | EVERYBODY).unwrap();
            // Room for exactly one of these messages.
            let mut ds = msg_queue(id).unwrap().stat();
            ds.qbytes = A_PAYLOAD;
            assert_eq!(
                msgctl(sys, id, IPC_SET, &mut ds as *mut MsqidDs as usize),
                Ok(0),
                "the capacity could not be lowered"
            );
            assert_eq!(send(sys, id, 1, FIRST).await, Ok(0));
            // And now it is full, which IPC_NOWAIT says outright.
            let full = AMessage::new(1, SECOND);
            assert_eq!(
                msgsnd(sys, id, &full, A_PAYLOAD, IPC_NOWAIT).await,
                Err(LxError::EAGAIN),
                "a full queue took a message with IPC_NOWAIT"
            );
            assert_eq!(
                msg_queue(id).unwrap().stat().qnum,
                1,
                "the refused message was queued anyway"
            );
            id
        })
    });

    // The sender parks, because the queue has no room.
    let sending = a_process(47_328, "msg-park-sender");
    let sender = a_parked_sender(&sending, id, 1, SECOND);

    let receiving = a_process(47_329, "msg-park-receiver");
    on_a_thread(&receiving, move |sys| {
        Box::pin(async move {
            nothing_moves_while_it_parks(id, 1).await;
            // Taking the first message makes room, and the parked send goes
            // in behind it.
            assert_eq!(
                take(sys, id, 0, IPC_NOWAIT).await,
                Ok((1, FIRST)),
                "the message in the full queue was not the first one sent"
            );
            assert!(
                the_queue_settles_at(id, 1).await,
                "the parked sender never got its message in"
            );
            assert_eq!(
                take(sys, id, 0, IPC_NOWAIT).await,
                Ok((1, SECOND)),
                "what the parked sender put in is not what it was given"
            );
        })
    });
    assert_eq!(
        sender.join().unwrap(),
        Ok(0),
        "the send did not come back once there was room"
    );

    // And the other way round: the queue is empty now, so a receiver parks.
    let waiting = a_process(47_330, "msg-park-waiter");
    let waiter = a_parked_receiver(&waiting, id, 0);

    let posting = a_process(47_331, "msg-park-poster");
    on_a_thread(&posting, move |sys| {
        Box::pin(async move {
            nothing_moves_while_it_parks(id, 0).await;
            assert_eq!(send(sys, id, 1, FIRST).await, Ok(0));
            assert!(
                the_queue_settles_at(id, 0).await,
                "the parked receiver never took the message"
            );
        })
    });
    assert_eq!(
        waiter.join().unwrap(),
        Ok((1, FIRST)),
        "the parked receiver did not get what was sent"
    );
}

/// The six numbers a queue keeps, which only `IPC_STAT` can read.
///
/// How many messages, how many bytes, who sent last, who received last, and
/// when each of the two happened. The two processes are what make the two
/// pids say something: a kernel that stamped the sender where the receiver
/// goes, or the same pid in both, reads the same with one process.
#[test]
fn the_six_numbers_a_queue_keeps_say_who_did_what() {
    const MINE: usize = KEY + 11;
    const ANOTHER: usize = KEY + 12;
    const A_TEXT: [u8; A_PAYLOAD] = [31; A_PAYLOAD];

    let sending = a_process(47_332, "msg-stat-sender");
    let (id, other, sender_pid) = on_a_thread(&sending, move |sys| {
        Box::pin(async move {
            let id = msgget(sys, MINE, CREAT | EVERYBODY).unwrap();
            let other = msgget(sys, ANOTHER, CREAT | EVERYBODY).unwrap();
            let us = sys.zircon_process().id() as usize;

            let mut fresh = a_msqid_ds_of_another_queue(other);
            assert_eq!(
                msgctl(sys, id, IPC_STAT, &mut fresh as *mut MsqidDs as usize),
                Ok(0),
                "IPC_STAT by id did not answer zero"
            );
            assert_eq!(fresh.qnum, 0, "a fresh queue counted a message");
            assert_eq!(fresh.cbytes, 0, "a fresh queue counted bytes");
            assert_eq!(fresh.stime, 0, "a fresh queue had been sent to");
            assert_eq!(fresh.rtime, 0, "a fresh queue had been received from");
            assert_eq!(
                fresh.qbytes,
                linux_object::ipc::MSGMNB,
                "a fresh queue did not get the default capacity"
            );

            assert_eq!(send(sys, id, 1, A_TEXT).await, Ok(0));
            let mut sent = a_msqid_ds_of_another_queue(other);
            msgctl(sys, id, IPC_STAT, &mut sent as *mut MsqidDs as usize).unwrap();
            assert_eq!(sent.qnum, 1, "the message was not counted");
            assert_eq!(
                sent.cbytes, A_PAYLOAD,
                "the bytes of the message were not counted"
            );
            assert_eq!(
                sent.lspid as usize, us,
                "the queue did not name the process that sent"
            );
            assert_ne!(sent.stime, 0, "the send was not timed");
            assert_eq!(sent.rtime, 0, "a queue nobody read from was timed");
            assert_eq!(sent.lrpid, 0, "a queue nobody read from named a reader");
            (id, other, us)
        })
    });

    let receiving = a_process(47_333, "msg-stat-receiver");
    on_a_thread(&receiving, move |sys| {
        Box::pin(async move {
            let us = sys.zircon_process().id() as usize;
            assert_ne!(us, sender_pid, "the two processes have one pid");
            assert_eq!(take(sys, id, 0, IPC_NOWAIT).await, Ok((1, A_TEXT)));

            let mut got = a_msqid_ds_of_another_queue(other);
            msgctl(sys, id, IPC_STAT, &mut got as *mut MsqidDs as usize).unwrap();
            assert_eq!(got.qnum, 0, "the message was not uncounted");
            assert_eq!(got.cbytes, 0, "the bytes were not uncounted");
            assert_eq!(
                got.lrpid as usize, us,
                "the queue did not name the process that received"
            );
            assert_eq!(
                got.lspid as usize, sender_pid,
                "receiving overwrote who had sent"
            );
            assert_ne!(got.rtime, 0, "the receive was not timed");
        })
    });
}

/// What the first argument of `msgctl` names depends on the command, and so
/// does what comes back.
///
/// The same three readings of one register as `semctl`: the `*_INFO`s ignore
/// it, the two `MSG_STAT`s read it as an INDEX and answer the id -- which is
/// how `ipcs -q` walks the table, and neither of them existed -- and
/// everything else reads it as an id. `MSG_STAT_ANY` is the one with no
/// permission check, which on a queue with no mode bits is the whole
/// difference.
#[test]
fn what_the_first_argument_of_msgctl_names_depends_on_the_command() {
    const MINE: usize = KEY + 13;
    const UNREADABLE: usize = KEY + 14;
    const A_USER: u32 = 1_000;
    const A_STRANGER: u32 = 1_001;
    const MSG_STAT: usize = 11;
    const MSG_INFO: usize = 12;
    const MSG_STAT_ANY: usize = 13;

    let owner = a_process(47_334, "msgctl-subject-owner");
    let (id, unreadable) = on_a_thread(&owner, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            (
                msgget(sys, MINE, CREAT | EVERYBODY).unwrap(),
                msgget(sys, UNREADABLE, CREAT).unwrap(),
            )
        })
    });

    let proc = a_process(47_335, "msgctl-subject");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_STRANGER, A_STRANGER, &[]);
            let mut info = MsgInfo::default();
            let at_info = &mut info as *mut MsgInfo as usize;

            // Unknown command, before anything else.
            assert_eq!(
                msgctl(sys, id, A_COMMAND_OF_NOBODY, 0),
                Err(LxError::EINVAL),
                "a command number nobody defines was accepted"
            );

            // The first argument is ignored, and the answer is the highest
            // index in use.
            let highest = msgctl(sys, 0, MSG_INFO, at_info).unwrap();
            assert!(
                highest >= id,
                "MSG_INFO answered {}, below this queue's own index {}",
                highest,
                id
            );
            assert_eq!(
                msgctl(sys, 0, IPC_INFO, at_info),
                Ok(highest),
                "IPC_INFO and MSG_INFO did not agree on the highest index"
            );

            // An index, and the id comes back. Queue ids are indices from
            // zero here, so this one is its own index.
            let mut ds = a_msqid_ds_of_another_queue(unreadable);
            let at_ds = &mut ds as *mut MsqidDs as usize;
            assert_eq!(
                msgctl(sys, id, MSG_STAT, at_ds),
                Ok(id),
                "MSG_STAT did not answer the id it found"
            );
            assert_eq!(
                ds.perm.key as usize, MINE,
                "MSG_STAT filled in another queue"
            );
            // An id, and zero comes back.
            assert_eq!(
                msgctl(sys, id, IPC_STAT, at_ds),
                Ok(0),
                "IPC_STAT did not answer zero"
            );

            // A queue with no mode bits: the stat with the check is refused
            // and the one defined not to check is not.
            assert_eq!(
                msgctl(sys, unreadable, MSG_STAT, at_ds),
                Err(LxError::EACCES),
                "a queue with no mode bits was stat-ed by a stranger"
            );
            assert_eq!(
                msgctl(sys, unreadable, MSG_STAT_ANY, at_ds),
                Ok(unreadable),
                "MSG_STAT_ANY asked the question it is defined not to ask"
            );

            // And the two that change the queue ask who owns it, not who may
            // use it: this one is 0666, so every other command said yes.
            assert_eq!(
                msgctl(sys, id, IPC_SET, at_ds),
                Err(LxError::EPERM),
                "a stranger took a queue over with IPC_SET"
            );
            assert_eq!(
                msgctl(sys, id, IPC_RMID, 0),
                Err(LxError::EPERM),
                "a stranger removed somebody else's queue"
            );
        })
    });
}

/// `MSG_INFO` is `IPC_INFO` with three of its fields replaced by what is
/// actually queued.
///
/// The three limits come from the crate that ENFORCES them -- `msgmni`
/// bounds `msgget`, `msgmax` bounds one `msgsnd` and `msgmnb` is a queue's
/// capacity -- because a reader told one number and refused at another has
/// no way to size itself.
#[test]
fn msg_info_replaces_three_of_the_limits_with_what_is_queued() {
    const MINE: usize = KEY + 15;
    const MSG_INFO: usize = 12;
    let proc = a_process(47_336, "msgctl-info");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = msgget(sys, MINE, CREAT | EVERYBODY).unwrap();
            assert_eq!(send(sys, id, 1, [41; A_PAYLOAD]).await, Ok(0));

            let mut limits = MsgInfo::default();
            msgctl(sys, 0, IPC_INFO, &mut limits as *mut MsgInfo as usize).unwrap();
            let mut counts = MsgInfo::default();
            msgctl(sys, 0, MSG_INFO, &mut counts as *mut MsgInfo as usize).unwrap();

            // IPC_INFO reports the limits and nothing else.
            assert_eq!(
                limits.msgmap as usize,
                linux_object::ipc::MSGMNB,
                "IPC_INFO moved msgmap off its limit"
            );
            assert_eq!(
                limits.msgtot as usize,
                linux_object::ipc::MSGMNB,
                "IPC_INFO moved msgtot off its limit"
            );
            // MSG_INFO puts what exists in those places.
            assert!(
                counts.msgpool >= 1,
                "MSG_INFO counted no queues with one in front of it"
            );
            assert!(
                counts.msgtot >= 1,
                "MSG_INFO counted no messages with one queued"
            );
            assert!(
                counts.msgmap as usize >= A_PAYLOAD,
                "MSG_INFO counted {} bytes with {} queued",
                counts.msgmap,
                A_PAYLOAD
            );
            assert_ne!(
                counts.msgmap as usize,
                linux_object::ipc::MSGMNB,
                "MSG_INFO left msgmap on the limit instead of counting"
            );

            // The three limits either way, from the crate that enforces them.
            assert_eq!(limits.msgmax, counts.msgmax);
            assert_eq!(limits.msgmnb, counts.msgmnb);
            assert_eq!(limits.msgmni, counts.msgmni);
            assert_eq!(
                limits.msgmax as usize,
                linux_object::ipc::MSGMAX,
                "the reported MSGMAX is not the one msgsnd enforces"
            );
            assert_eq!(
                limits.msgmnb as usize,
                linux_object::ipc::MSGMNB,
                "the reported MSGMNB is not a queue's capacity"
            );
            assert_eq!(
                limits.msgmni as usize,
                linux_object::ipc::MSGMNI,
                "the reported MSGMNI is not the one msgget enforces"
            );
        })
    });
}

/// Removing a queue wakes what was parked on it with `EIDRM`, and leaves its
/// id and its key naming nothing.
#[test]
fn removing_a_queue_wakes_what_was_parked_on_it_and_leaves_nothing_behind() {
    const MINE: usize = KEY + 16;
    let maker = a_process(47_337, "msg-rmid-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move { msgget(sys, MINE, CREAT | EVERYBODY).unwrap() })
    });

    let waiting = a_process(47_338, "msg-rmid-waiter");
    let waiter = a_parked_receiver(&waiting, id, 0);

    let removing = a_process(47_339, "msg-rmid-remover");
    on_a_thread(&removing, move |sys| {
        Box::pin(async move {
            nothing_moves_while_it_parks(id, 0).await;
            assert_eq!(
                msgctl(sys, id, IPC_RMID, 0),
                Ok(0),
                "the queue could not be removed"
            );
            assert_eq!(
                take(sys, id, 0, IPC_NOWAIT).await,
                Err(LxError::EINVAL),
                "a removed queue still answered by id"
            );
            assert_eq!(
                send(sys, id, 1, [1; A_PAYLOAD]).await,
                Err(LxError::EINVAL),
                "a removed queue still took messages"
            );
            assert_eq!(
                msgget(sys, MINE, 0),
                Err(LxError::ENOENT),
                "the key of a removed queue still named it"
            );
            assert_eq!(
                msgctl(sys, id, IPC_RMID, 0),
                Err(LxError::EINVAL),
                "a queue was removed twice"
            );
        })
    });

    assert_eq!(
        waiter.join().unwrap(),
        Err(LxError::EIDRM),
        "the process parked on the queue was not woken into EIDRM"
    );
}

/// The arguments arrive in the places the syscall table names.
///
/// `msgrcv` is the one worth asking: five registers, of which two are sizes,
/// one is a pointer and one is a SIGNED type that the entry has to cast.
/// A message sent by number and taken by number, with its type and its bytes
/// compared, is the only thing that pins all five.
#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    const MINE: usize = KEY + 17;
    const A_TYPE: isize = 77;
    const A_TEXT: [u8; A_PAYLOAD] = [51, 52, 53, 54, 55, 56, 57, 58];
    let proc = a_process(47_340, "msg-by-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let made = by_number(
                sys,
                crate::Sys::MSGGET,
                [MINE, CREAT | EVERYBODY, 0, 0, 0, 0],
            )
            .await;
            assert!(made >= 0, "msgget by number did not make a queue: {}", made);
            let id = made as usize;

            let msg = AMessage::new(A_TYPE, A_TEXT);
            assert_eq!(
                by_number(sys, crate::Sys::MSGSND, [id, msg.at(), A_PAYLOAD, 0, 0, 0]).await,
                0,
                "msgsnd by number did not go through"
            );

            // The type goes in as a signed value, so a kernel that read the
            // register unsigned would be asking for a type of 2^64 - 1.
            let mut into = AMessage::empty();
            let at = &mut into as *mut AMessage as usize;
            let got = by_number(
                sys,
                crate::Sys::MSGRCV,
                [id, at, A_PAYLOAD, (-99isize) as usize, IPC_NOWAIT, 0],
            )
            .await;
            assert_eq!(
                got, A_PAYLOAD as isize,
                "msgrcv by number did not answer the payload length"
            );
            assert_eq!(into.mtype(), A_TYPE, "the type did not come back");
            assert_eq!(into.text(), &A_TEXT, "the bytes did not come back");
        })
    });
}
