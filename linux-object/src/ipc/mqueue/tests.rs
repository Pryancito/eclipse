//! Host tests for the POSIX message queues.
//!
//! The table of names and the `RLIMIT_MSGQUEUE` accounts are global, as they
//! are in the kernel, and the tests run in parallel: each test uses names
//! and real users of its own, and every one but
//! [`queues_max_is_enospc_without_cap_sys_resource`] holds
//! `CAP_SYS_RESOURCE`, so that the one test that fills `queues_max` stops
//! nobody else.
//!
//! The waits are polled one at a time with a counting waker rather than
//! `block_on`, as in `msgqueue.rs`: a wait that never resolves would make
//! `block_on` hang, and a hang is not a failure anyone reads.

use super::*;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

extern crate std;
use std::cell::RefCell;
use std::sync::Arc as StdArc;
use std::task::Wake;
use std::thread_local;

thread_local! {
    /// Owners `owner_alive` says have exited.
    pub(super) static DEAD: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    /// Signals `deliver` sent: owner, signo, sender pid, sender uid, value.
    pub(super) static SENT: RefCell<Vec<Sent>> =
        const { RefCell::new(Vec::new()) };
    /// Cookies `deliver` and `deliver_removed` sent.
    pub(super) static COOKIES: RefCell<Vec<[u8; NOTIFY_COOKIE_LEN]>> =
        const { RefCell::new(Vec::new()) };
}

/// A signal `deliver` sent: owner, signo, sender pid, sender uid, value.
pub(super) type Sent = (u64, usize, u32, u32, usize);

const O_RDONLY: usize = 0;

/// Root, with every capability a queue asks about, as real user `ruid`.
fn root(ruid: u32) -> MqCred {
    MqCred {
        euid: 0,
        egid: 0,
        groups: Vec::new(),
        ruid,
        dac_override: true,
        fowner: true,
        sys_resource: true,
        umask: 0o022,
        rlimit_msgqueue: 819_200,
    }
}

/// An ordinary user `uid`, in group `uid`, with no capabilities. Still
/// `sys_resource`: see the module documentation.
fn user(uid: u32) -> MqCred {
    MqCred {
        euid: uid,
        egid: uid,
        groups: Vec::new(),
        ruid: uid,
        dac_override: false,
        fowner: false,
        sys_resource: true,
        umask: 0,
        rlimit_msgqueue: 819_200,
    }
}

fn attr(maxmsg: isize, msgsize: isize) -> Option<MqAttr> {
    Some(MqAttr {
        maxmsg,
        msgsize,
        ..MqAttr::default()
    })
}

/// A new queue under `name`, for root as real user `ruid`.
fn make(name: &str, ruid: u32, maxmsg: isize, msgsize: isize) -> Arc<PosixMq> {
    let (q, created) = mq_open(
        name,
        O_CREAT | O_EXCL | O_RDWR,
        0o600,
        attr(maxmsg, msgsize),
        &root(ruid),
    )
    .expect("a fresh name makes a queue");
    assert!(created);
    q
}

fn send(q: &PosixMq, data: &[u8], prio: u32) -> Result<Option<MqNotify>, ()> {
    q.try_send(data.to_vec(), prio, Charge::none())
        .map_err(|_| ())
}

struct CountWaker(core::sync::atomic::AtomicUsize);

impl Wake for CountWaker {
    fn wake(self: StdArc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker() -> (StdArc<CountWaker>, Waker) {
    let count = StdArc::new(CountWaker(core::sync::atomic::AtomicUsize::new(0)));
    (count.clone(), Waker::from(count))
}

// ------------------------------------------------------------------ names

#[test]
fn names_are_checked_in_linux_order() {
    assert_eq!(check_name(""), Err(LxError::ENOENT));
    assert_eq!(check_name("a/b"), Err(LxError::EACCES));
    assert_eq!(check_name("/a"), Err(LxError::EACCES));
    assert_eq!(check_name("."), Err(LxError::EACCES));
    assert_eq!(check_name(".."), Err(LxError::EACCES));
    assert_eq!(check_name("..."), Ok(()));
    let long = "a".repeat(MQ_NAME_MAX);
    assert_eq!(check_name(&long), Ok(()));
    let longer = "a".repeat(MQ_NAME_MAX + 1);
    assert_eq!(check_name(&longer), Err(LxError::ENAMETOOLONG));
    // `getname` comes first: a slash in a name too long for it is not seen.
    let path = "/".repeat(MQ_PATH_MAX);
    assert_eq!(check_name(&path), Err(LxError::ENAMETOOLONG));
}

// ------------------------------------------------------------------- open

#[test]
fn open_without_creat_of_nothing_is_enoent() {
    assert_eq!(
        mq_open("t-enoent", O_RDWR, 0o600, None, &root(5001)).err(),
        Some(LxError::ENOENT)
    );
}

#[test]
fn creat_excl_of_an_existing_queue_is_eexist_and_creat_alone_opens_it() {
    let q = make("t-excl", 5002, 4, 16);
    assert_eq!(
        mq_open(
            "t-excl",
            O_CREAT | O_EXCL | O_RDWR,
            0o600,
            None,
            &root(5002)
        )
        .err(),
        Some(LxError::EEXIST)
    );
    // O_CREAT on a name in use opens what is there, attributes and all.
    let (again, created) =
        mq_open("t-excl", O_CREAT | O_RDWR, 0o600, attr(9, 9), &root(5002)).unwrap();
    assert!(!created);
    assert!(Arc::ptr_eq(&q, &again));
    assert_eq!((again.maxmsg(), again.msgsize()), (4, 16));
    mq_unlink("t-excl", &root(5002)).unwrap();
}

#[test]
fn defaults_without_attributes() {
    let (q, _) = mq_open("t-dflt", O_CREAT | O_RDWR, 0o600, None, &root(5003)).unwrap();
    assert_eq!((q.maxmsg(), q.msgsize()), (DFLT_MAXMSG, DFLT_MSGSIZE));
    mq_unlink("t-dflt", &root(5003)).unwrap();
}

#[test]
fn attributes_are_checked_only_when_a_queue_is_made() {
    let r = root(5004);
    for (m, s) in [(0, 8), (8, 0), (-1, 8), (8, -1)] {
        assert_eq!(
            mq_open("t-attr", O_CREAT | O_RDWR, 0o600, attr(m, s), &r).err(),
            Some(LxError::EINVAL),
            "maxmsg={m} msgsize={s}"
        );
    }
    assert_eq!(
        mq_open(
            "t-attr",
            O_CREAT | O_RDWR,
            0o600,
            attr(HARD_MSGMAX as isize + 1, 8),
            &r
        )
        .err(),
        Some(LxError::EINVAL)
    );
    let mut plain = user(5004);
    plain.sys_resource = false;
    assert_eq!(
        attr_ok(&attr(MSG_MAX as isize + 1, 8).unwrap(), false).err(),
        Some(LxError::EINVAL)
    );
    assert_eq!(
        attr_ok(&attr(8, MSGSIZE_MAX as isize + 1).unwrap(), false).err(),
        Some(LxError::EINVAL)
    );
    assert_eq!(
        attr_ok(
            &attr(MSG_MAX as isize, MSGSIZE_MAX as isize).unwrap(),
            false
        ),
        Ok((10, 8192))
    );
    drop(plain);
}

#[test]
fn an_existing_queue_is_opened_for_the_access_its_bits_allow() {
    let owner = user(5005);
    let (q, _) = mq_open("t-perm", O_CREAT | O_RDWR, 0o640, None, &owner).unwrap();
    let mut member = user(5006);
    member.groups.push(5005);
    assert!(mq_open("t-perm", O_RDONLY, 0, None, &member).is_ok());
    assert_eq!(
        mq_open("t-perm", O_WRONLY, 0, None, &member).err(),
        Some(LxError::EACCES)
    );
    assert_eq!(
        mq_open("t-perm", O_RDONLY, 0, None, &user(5007)).err(),
        Some(LxError::EACCES)
    );
    assert!(mq_open("t-perm", O_RDWR, 0, None, &root(5007)).is_ok());
    // An access mode of 3 is EINVAL on an existing queue.
    assert_eq!(
        mq_open("t-perm", 3, 0, None, &owner).err(),
        Some(LxError::EINVAL)
    );
    drop(q);
    mq_unlink("t-perm", &owner).unwrap();
}

#[test]
fn the_umask_comes_off_a_new_queue() {
    let mut cred = user(5008);
    cred.umask = 0o077;
    let (q, _) = mq_open("t-umask", O_CREAT | O_RDWR, 0o666, None, &cred).unwrap();
    let file = MqFile::new(q, O_RDWR);
    assert_eq!(file.metadata().unwrap().mode, 0o600);
    assert_eq!(file.metadata().unwrap().uid, 5008);
    mq_unlink("t-umask", &cred).unwrap();
}

#[test]
fn a_descriptor_has_the_access_it_was_opened_with_and_is_cloexec() {
    let q = make("t-acc", 5009, 2, 8);
    let r = MqFile::new(q.clone(), O_RDONLY);
    let w = MqFile::new(q.clone(), O_WRONLY | O_NONBLOCK);
    let rw = MqFile::new(q.clone(), O_RDWR);
    let none = MqFile::new(q, 3);
    assert_eq!(
        (r.readable(), r.writable(), r.nonblock()),
        (true, false, false)
    );
    assert_eq!(
        (w.readable(), w.writable(), w.nonblock()),
        (false, true, true)
    );
    assert_eq!((rw.readable(), rw.writable()), (true, true));
    assert_eq!((none.readable(), none.writable()), (false, false));
    assert!(r.flags().close_on_exec());
    mq_unlink("t-acc", &root(5009)).unwrap();
}

// ----------------------------------------------------------------- unlink

#[test]
fn unlink_takes_the_name_and_the_queue_lives_on_while_open() {
    let q = make("t-unlink", 5010, 4, 8);
    let file = MqFile::new(q.clone(), O_RDWR);
    assert_eq!(file.link_name(), "/t-unlink");
    mq_unlink("t-unlink", &root(5010)).unwrap();
    assert_eq!(mq_unlink("t-unlink", &root(5010)), Err(LxError::ENOENT));
    assert_eq!(
        mq_open("t-unlink", O_RDWR, 0, None, &root(5010)).err(),
        Some(LxError::ENOENT)
    );
    send(&q, b"still", 0).unwrap();
    assert_eq!(q.try_recv().unwrap().0, b"still");
    assert_eq!(file.link_name(), "/t-unlink (deleted)");
    // A new queue under the old name is another queue.
    let other = make("t-unlink", 5010, 4, 8);
    assert!(!Arc::ptr_eq(&q, &other));
    mq_unlink("t-unlink", &root(5010)).unwrap();
}

#[test]
fn only_the_owner_or_cap_fowner_unlinks_a_queue() {
    let owner = user(5011);
    mq_open("t-sticky", O_CREAT | O_RDWR, 0o666, None, &owner).unwrap();
    assert_eq!(mq_unlink("t-sticky", &user(5012)), Err(LxError::EPERM));
    let mut fowner = user(5012);
    fowner.fowner = true;
    assert_eq!(mq_unlink("t-sticky", &fowner), Ok(()));
}

#[test]
fn a_created_queue_whose_descriptor_failed_is_forgotten() {
    let q = make("t-forget", 5013, 1, 1);
    mq_forget_created(&q);
    assert_eq!(
        mq_open("t-forget", O_RDWR, 0, None, &root(5013)).err(),
        Some(LxError::ENOENT)
    );
}

// --------------------------------------------------------------- messages

#[test]
fn highest_priority_first_and_oldest_first_within_one() {
    let q = make("t-prio", 5014, 8, 8);
    for (data, prio) in [
        (&b"a0"[..], 0),
        (b"c5", 5),
        (b"b1", 1),
        (b"d5", 5),
        (b"e31", 32767),
    ] {
        send(&q, data, prio).unwrap();
    }
    assert_eq!(q.attr().curmsgs, 5);
    let got: Vec<(Vec<u8>, u32)> = core::iter::from_fn(|| q.try_recv()).collect();
    let want: Vec<(Vec<u8>, u32)> = [
        (&b"e31"[..], 32767),
        (b"c5", 5),
        (b"d5", 5),
        (b"b1", 1),
        (b"a0", 0),
    ]
    .iter()
    .map(|(d, p)| (d.to_vec(), *p))
    .collect();
    assert_eq!(got, want);
    assert_eq!(q.attr().curmsgs, 0);
    mq_unlink("t-prio", &root(5014)).unwrap();
}

#[test]
fn a_full_queue_hands_the_message_back() {
    let q = make("t-full", 5015, 2, 8);
    send(&q, b"1", 0).unwrap();
    send(&q, b"2", 0).unwrap();
    match q.try_send(b"3".to_vec(), 0, Charge::none()) {
        Err((data, _)) => assert_eq!(data, b"3"),
        Ok(_) => panic!("a full queue took a third message"),
    }
    assert_eq!(q.attr().curmsgs, 2);
    q.try_recv().unwrap();
    send(&q, b"3", 0).unwrap();
    mq_unlink("t-full", &root(5015)).unwrap();
}

#[test]
fn poll_says_what_the_queue_has() {
    let q = make("t-poll", 5016, 1, 8);
    let f = MqFile::new(q.clone(), O_RDWR);
    let st = f.poll(PollEvents::IN | PollEvents::OUT).unwrap();
    assert_eq!((st.read, st.write), (false, true));
    send(&q, b"x", 0).unwrap();
    let st = f.poll(PollEvents::IN | PollEvents::OUT).unwrap();
    assert_eq!((st.read, st.write), (true, false));
    assert_eq!(
        q.bus.lock().events() & (Event::READABLE | Event::WRITABLE),
        Event::READABLE
    );
    q.try_recv().unwrap();
    assert_eq!(
        q.bus.lock().events() & (Event::READABLE | Event::WRITABLE),
        Event::WRITABLE
    );
    mq_unlink("t-poll", &root(5016)).unwrap();
}

#[test]
fn getsetattr_reports_the_descriptor_flags_and_sets_only_nonblock() {
    let q = make("t-attrs", 5017, 3, 64);
    send(&q, b"x", 0).unwrap();
    let f = MqFile::new(q, O_RDWR);
    let old = f.getsetattr(Some(&MqAttr {
        flags: O_NONBLOCK as isize,
        maxmsg: 99,
        msgsize: 99,
        curmsgs: 99,
        reserved: [0; 4],
    }));
    assert_eq!(
        (old.flags, old.maxmsg, old.msgsize, old.curmsgs),
        (0, 3, 64, 1)
    );
    assert!(f.nonblock());
    let now = f.getsetattr(Some(&MqAttr::default()));
    assert_eq!(
        (now.flags, now.maxmsg, now.msgsize),
        (O_NONBLOCK as isize, 3, 64)
    );
    assert!(!f.nonblock());
    mq_unlink("t-attrs", &root(5017)).unwrap();
}

#[test]
fn a_read_is_the_status_line() {
    let q = make("t-read", 5018, 3, 64);
    send(&q, b"hello", 0).unwrap();
    q.notify_register(MqNotify {
        owner: 42,
        how: MqNotifyHow::Signal {
            signo: 10,
            value: 0,
        },
    })
    .unwrap();
    let f = MqFile::new(q, O_RDWR);
    let mut buf = [0u8; 128];
    let n = async_std::task::block_on(f.read(&mut buf)).unwrap();
    assert_eq!(
        core::str::from_utf8(&buf[..n]).unwrap(),
        "QSIZE:5          NOTIFY:0     SIGNO:10    NOTIFY_PID:42    \n"
    );
    assert_eq!(async_std::task::block_on(f.read(&mut buf)).unwrap(), 0);
    assert_eq!(f.write(b"x"), Err(LxError::EINVAL));
    mq_unlink("t-read", &root(5018)).unwrap();
}

// ----------------------------------------------------------------- limits

#[test]
fn rlimit_msgqueue_counts_room_not_messages_and_comes_back_with_the_queue() {
    // 10 x 8192 plus Linux's 96 bytes a message: nine default queues fit
    // the default 819200, the tenth does not.
    assert_eq!(rlimit_bytes(10, 8192), 82_880);
    let cred = root(6001);
    let mut held = Vec::new();
    for i in 0..9 {
        let name = alloc::format!("t-rlim-{i}");
        held.push(
            mq_open(&name, O_CREAT | O_RDWR, 0o600, None, &cred)
                .unwrap()
                .0,
        );
    }
    assert_eq!(
        mq_open("t-rlim-9", O_CREAT | O_RDWR, 0o600, None, &cred).err(),
        Some(LxError::EMFILE),
        "root too: RLIMIT_MSGQUEUE has no capability that passes it"
    );
    // Another user has an account of their own.
    let (other, _) = mq_open("t-rlim-other", O_CREAT | O_RDWR, 0o600, None, &root(6002)).unwrap();
    // Unlinked but open still counts; closed as well, it does not.
    mq_unlink("t-rlim-0", &cred).unwrap();
    assert_eq!(
        mq_open("t-rlim-9", O_CREAT | O_RDWR, 0o600, None, &cred).err(),
        Some(LxError::EMFILE)
    );
    held.remove(0);
    let (ninth, _) = mq_open("t-rlim-9", O_CREAT | O_RDWR, 0o600, None, &cred).unwrap();
    drop((ninth, other));
    for i in 1..10 {
        mq_unlink(&alloc::format!("t-rlim-{i}"), &cred).unwrap();
    }
    mq_unlink("t-rlim-other", &root(6002)).unwrap();
}

#[test]
fn queues_max_is_enospc_without_cap_sys_resource() {
    let mut cred = user(6003);
    cred.sys_resource = false;
    cred.rlimit_msgqueue = usize::MAX;
    let mut made = 0;
    let mut refused = None;
    for i in 0..=QUEUES_MAX {
        let name = alloc::format!("t-qmax-{i}");
        match mq_open(&name, O_CREAT | O_RDWR, 0o600, attr(1, 1), &cred) {
            Ok(_) => made += 1,
            Err(err) => {
                refused = Some(err);
                break;
            }
        }
    }
    assert_eq!(refused, Some(LxError::ENOSPC));
    assert!(made <= QUEUES_MAX);
    // CAP_SYS_RESOURCE goes past it.
    let mut capable = cred.clone();
    capable.sys_resource = true;
    assert!(mq_open("t-qmax-cap", O_CREAT | O_RDWR, 0o600, attr(1, 1), &capable).is_ok());
    mq_unlink("t-qmax-cap", &capable).unwrap();
    for i in 0..made {
        mq_unlink(&alloc::format!("t-qmax-{i}"), &cred).unwrap();
    }
    assert!(mq_open("t-qmax-again", O_CREAT | O_RDWR, 0o600, attr(1, 1), &cred).is_ok());
    mq_unlink("t-qmax-again", &cred).unwrap();
}

// ----------------------------------------------------------- notification

fn signal_to(owner: u64, signo: usize, value: usize) -> MqNotify {
    MqNotify {
        owner,
        how: MqNotifyHow::Signal { signo, value },
    }
}

#[test]
fn a_notification_fires_once_on_a_message_into_an_empty_queue() {
    let q = make("t-notify", 6004, 4, 8);
    q.notify_register(signal_to(100, 10, 0xbeef)).unwrap();
    // Taken: only one process at a time.
    assert_eq!(
        q.notify_register(signal_to(101, 10, 0)),
        Err(LxError::EBUSY)
    );
    assert_eq!(
        q.notify_register(signal_to(100, 10, 0)),
        Err(LxError::EBUSY)
    );
    let fired = send(&q, b"1", 0)
        .unwrap()
        .expect("the first message fires it");
    assert_eq!(fired.owner, 100);
    SENT.with(|s| s.borrow_mut().clear());
    deliver(fired, 7, 1000);
    assert_eq!(
        SENT.with(|s| s.borrow().clone()),
        [(100, 10, 7, 1000, 0xbeef)]
    );
    // Spent: the next registration is free, and a message into a queue that
    // is not empty fires nothing.
    q.notify_register(signal_to(101, 12, 0)).unwrap();
    assert!(send(&q, b"2", 0).unwrap().is_none());
    q.try_recv().unwrap();
    q.try_recv().unwrap();
    assert_eq!(send(&q, b"3", 0).unwrap().map(|n| n.owner), Some(101));
    mq_unlink("t-notify", &root(6004)).unwrap();
}

#[test]
fn only_its_owner_takes_a_registration_away() {
    let q = make("t-unreg", 6005, 4, 8);
    q.notify_register(signal_to(200, 10, 0)).unwrap();
    assert!(q.notify_unregister(201).is_none());
    assert!(q.notify_unregister(200).is_some());
    assert!(q.notify_unregister(200).is_none());
    assert!(send(&q, b"x", 0).unwrap().is_none());
    q.notify_register(signal_to(201, 10, 0)).unwrap();
    mq_unlink("t-unreg", &root(6005)).unwrap();
}

#[test]
fn a_registration_of_an_exited_process_is_none() {
    let q = make("t-dead", 6006, 4, 8);
    q.notify_register(signal_to(300, 10, 0)).unwrap();
    DEAD.with(|d| d.borrow_mut().push(300));
    assert_eq!(q.notify_register(signal_to(301, 10, 0)), Ok(()));
    DEAD.with(|d| d.borrow_mut().clear());
    mq_unlink("t-dead", &root(6006)).unwrap();
}

#[test]
fn a_thread_notification_sends_its_cookie_woken_or_removed() {
    let q = make("t-cookie", 6007, 4, 8);
    let sock: Arc<dyn FileLike> = MqFile::new(q.clone(), O_RDWR);
    let mut cookie = [0u8; NOTIFY_COOKIE_LEN];
    cookie[0] = 0x5a;
    COOKIES.with(|c| c.borrow_mut().clear());
    q.notify_register(MqNotify {
        owner: 400,
        how: MqNotifyHow::Thread {
            sock: sock.clone(),
            cookie,
        },
    })
    .unwrap();
    deliver(send(&q, b"x", 0).unwrap().unwrap(), 1, 2);
    q.try_recv().unwrap();
    q.notify_register(MqNotify {
        owner: 400,
        how: MqNotifyHow::Thread { sock, cookie },
    })
    .unwrap();
    deliver_removed(q.notify_unregister(400).unwrap());
    let sent = COOKIES.with(|c| c.borrow().clone());
    assert_eq!(sent.len(), 2);
    assert_eq!(
        (sent[0][0], sent[0][NOTIFY_COOKIE_LEN - 1]),
        (0x5a, NOTIFY_WOKENUP)
    );
    assert_eq!(
        (sent[1][0], sent[1][NOTIFY_COOKIE_LEN - 1]),
        (0x5a, NOTIFY_REMOVED)
    );
    mq_unlink("t-cookie", &root(6007)).unwrap();
}

// ------------------------------------------------------------------ waits

#[test]
fn a_message_for_a_receiver_asleep_is_not_a_notification() {
    let q = make("t-asleep", 6008, 4, 8);
    q.notify_register(signal_to(500, 10, 0)).unwrap();
    let (count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    {
        let mut wait = pin!(q.wait_message(None));
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        assert!(
            send(&q, b"x", 0).unwrap().is_none(),
            "the receiver asleep takes it"
        );
        assert!(
            count.0.load(Ordering::SeqCst) >= 1,
            "the message wakes the receiver"
        );
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    }
    assert_eq!(
        q.inner.lock().receivers,
        0,
        "a wait that ended is no receiver"
    );
    // Still registered: it did not fire.
    q.try_recv().unwrap();
    assert!(send(&q, b"y", 0).unwrap().is_some());
    mq_unlink("t-asleep", &root(6008)).unwrap();
}

#[test]
fn a_full_queue_wakes_its_sender_when_a_message_goes() {
    let q = make("t-room", 6009, 1, 8);
    send(&q, b"x", 0).unwrap();
    let (count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    let mut wait = pin!(q.wait_room(None));
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    q.try_recv().unwrap();
    assert!(count.0.load(Ordering::SeqCst) >= 1);
    assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    mq_unlink("t-room", &root(6009)).unwrap();
}

#[test]
fn a_deadline_already_past_is_etimedout_unless_the_queue_is_ready() {
    let q = make("t-timeout", 6010, 1, 8);
    let (_, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    let past = Some(Duration::ZERO);
    {
        let mut wait = pin!(q.wait_message(past));
        assert_eq!(
            wait.as_mut().poll(&mut cx),
            Poll::Ready(Err(LxError::ETIMEDOUT))
        );
    }
    {
        let mut wait = pin!(q.wait_room(past));
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    }
    send(&q, b"x", 0).unwrap();
    {
        let mut wait = pin!(q.wait_room(past));
        assert_eq!(
            wait.as_mut().poll(&mut cx),
            Poll::Ready(Err(LxError::ETIMEDOUT))
        );
    }
    mq_unlink("t-timeout", &root(6010)).unwrap();
}

#[test]
fn a_realtime_deadline_is_moved_onto_the_monotonic_clock() {
    let s = Duration::from_secs;
    assert_eq!(realtime_deadline(s(1_000_010), s(1_000_000), s(50)), s(60));
    // Already past: now.
    assert_eq!(realtime_deadline(s(5), s(1_000_000), s(50)), s(50));
}
