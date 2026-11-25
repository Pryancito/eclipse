//! `zx_object_*`: what a process learns about its objects and how it
//! signals and waits on them, through the syscall table as a process makes
//! the calls (`Syscall::syscall`), with its buffers in its own address
//! space so the kernel's checks of them are the real ones.
//!
//! These calls are the ones every other subsystem leans on -- a driver
//! waits on its interrupt and its channels with them, a debugger reads
//! threads and names -- and they are also how a process probes what it
//! holds. So most of what is asserted is the refusals: which right each one
//! needs, which error wins when two things are wrong, and what is written
//! back when the caller's buffer is too small or not there.

use kernel_hal::MMUFlags;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use zircon_object::{
    object::{Handle, KernelObject, Rights, Signal},
    signal::{Event, EventPair, Port},
    task::{CurrentThread, Job, Process, Thread},
    vm::{VmObject, PAGE_SIZE},
    ZxError,
};
use zircon_syscall::Syscall;

type ThreadFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

fn finish_thread(ct: CurrentThread) -> ThreadFuture {
    Box::pin(async move { drop(ct) })
}

/// Run `body` as the first thread of a new process: `Thread::start` calls
/// the thread function on this very thread, so the assertions are this
/// test's.
fn on_a_thread(body: fn(&mut Syscall<'_>, usize)) {
    // `ThreadFn` is a plain `fn`, so the body reaches it through a
    // thread-local of this (the calling) thread.
    std::thread_local! {
        static BODY: core::cell::Cell<Option<fn(&mut Syscall<'_>, usize)>> =
            const { core::cell::Cell::new(None) };
    }
    fn run(ct: CurrentThread) -> ThreadFuture {
        let body = BODY.with(|b| b.take()).expect("started from on_a_thread");
        let base = map_user_memory(&ct);
        let mut sc = Syscall {
            thread: &ct,
            thread_fn: finish_thread,
        };
        body(&mut sc, base);
        Box::pin(async move { drop(ct) })
    }
    BODY.with(|b| b.set(Some(body)));
    let proc = Process::create(&Job::root(), "object-test").unwrap();
    let thread = Thread::create(&proc, "object-test").unwrap();
    thread.start(run).unwrap();
    assert!(
        BODY.with(|b| b.get()).is_none(),
        "the body did not run on this thread"
    );
}

/// Twenty pages of the process's own memory, readable and writable.
fn map_user_memory(ct: &CurrentThread) -> usize {
    let vmo = VmObject::new_paged(20);
    ct.proc()
        .vmar()
        .map(
            None,
            vmo,
            0,
            20 * PAGE_SIZE,
            MMUFlags::USER | MMUFlags::READ | MMUFlags::WRITE,
        )
        .unwrap()
}

/// Make syscall `num` once. Everything here answers on the first poll; a
/// call that would park is a test bug, not an answer.
fn call(sc: &mut Syscall<'_>, num: u32, args: &[usize]) -> isize {
    let mut a = [0usize; 8];
    a[..args.len()].copy_from_slice(args);
    let mut f = Box::pin(sc.syscall(num, a));
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(r) => r,
        Poll::Pending => panic!("syscall {} parked", num),
    }
}

fn zx(e: ZxError) -> isize {
    e as isize
}

fn add(sc: &Syscall<'_>, object: std::sync::Arc<dyn KernelObject>, rights: Rights) -> u32 {
    sc.thread.proc().add_handle(Handle::new(object, rights))
}

fn read<T: Copy>(at: usize) -> T {
    unsafe { (at as *const T).read_unaligned() }
}

fn write<T>(at: usize, value: T) {
    unsafe { (at as *mut T).write_unaligned(value) }
}

// The numbers of the generated table (`src/consts.rs`).
const OBJECT_GET_CHILD: u32 = 72;
const OBJECT_GET_INFO: u32 = 73;
const OBJECT_GET_PROPERTY: u32 = 74;
const OBJECT_SET_PROPERTY: u32 = 76;
const OBJECT_SIGNAL: u32 = 77;
const OBJECT_SIGNAL_PEER: u32 = 78;
const OBJECT_WAIT_ASYNC: u32 = 79;
const OBJECT_WAIT_MANY: u32 = 80;
const OBJECT_WAIT_ONE: u32 = 81;

// `ZX_INFO_*`, `ZX_PROP_*`, `ZX_OBJ_TYPE_*`.
const INFO_HANDLE_VALID: usize = 1;
const INFO_HANDLE_BASIC: usize = 2;
const INFO_PROCESS: usize = 0x1000_0003;
const INFO_PROCESS_THREADS: usize = 4;
const INFO_PROCESS_MAPS: usize = 13;
const INFO_HANDLE_COUNT: usize = 19;
const PROP_NAME: usize = 3;
const PROP_VMO_CONTENT_SIZE: usize = 17;
const OBJ_TYPE_EVENT: u32 = 5;

// ---- zx_object_get_info --------------------------------------------------

#[test]
fn handle_basic_says_what_the_handle_is_and_how_much_was_written() {
    on_a_thread(|sc, base| {
        let event = Event::new();
        let h = add(sc, event.clone(), Rights::DEFAULT_EVENT);
        let (buf, actual, avail) = (base, base + 0x100, base + 0x108);
        let args = [h as usize, INFO_HANDLE_BASIC, buf, 32, actual, avail];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), 0);
        assert_eq!(read::<u64>(buf), event.id(), "koid");
        assert_eq!(read::<u32>(buf + 8), Rights::DEFAULT_EVENT.bits(), "rights");
        assert_eq!(read::<u32>(buf + 12), OBJ_TYPE_EVENT, "type");
        assert_eq!((read::<usize>(actual), read::<usize>(avail)), (1, 1));
    });
}

#[test]
fn a_short_info_buffer_is_refused_but_still_says_there_was_one_record() {
    on_a_thread(|sc, base| {
        let h = add(sc, Event::new(), Rights::DEFAULT_EVENT);
        let (actual, avail) = (base + 0x100, base + 0x108);
        write(actual, 7usize);
        write(avail, 7usize);
        let args = [h as usize, INFO_HANDLE_BASIC, base, 8, actual, avail];
        assert_eq!(
            call(sc, OBJECT_GET_INFO, &args),
            zx(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(read::<usize>(actual), 0, "nothing was written");
        assert_eq!(read::<usize>(avail), 1, "and one record is there to have");
    });
}

#[test]
fn info_into_memory_the_process_does_not_have_is_invalid_args() {
    on_a_thread(|sc, base| {
        let h = add(sc, Event::new(), Rights::DEFAULT_EVENT);
        // A host address outside this process's address space: what the
        // libos check of the pointer alone would have let through.
        let mut outside = [0u8; 64];
        let at = outside.as_mut_ptr() as usize;
        let args = [h as usize, INFO_HANDLE_BASIC, at, 32, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), zx(ZxError::INVALID_ARGS));
        assert_eq!(outside, [0; 64]);
        // And the counts, which are written first.
        let args = [h as usize, INFO_HANDLE_BASIC, base, 32, at, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), zx(ZxError::INVALID_ARGS));
        assert_eq!(outside, [0; 64]);
    });
}

#[test]
fn a_bad_handle_is_reported_before_an_unknown_topic() {
    on_a_thread(|sc, base| {
        assert_eq!(
            call(sc, OBJECT_GET_INFO, &[0xdead, 0x7777, base, 64, 0, 0]),
            zx(ZxError::BAD_HANDLE)
        );
        let h = add(sc, Event::new(), Rights::DEFAULT_EVENT);
        assert_eq!(
            call(sc, OBJECT_GET_INFO, &[h as usize, 0x7777, base, 64, 0, 0]),
            zx(ZxError::INVALID_ARGS)
        );
        // `ZX_INFO_NONE` is not a topic either.
        assert_eq!(
            call(sc, OBJECT_GET_INFO, &[h as usize, 0, base, 64, 0, 0]),
            zx(ZxError::INVALID_ARGS)
        );
        // `ZX_INFO_HANDLE_VALID` needs no right and writes nothing.
        let bare = add(sc, Event::new(), Rights::empty());
        assert_eq!(
            call(
                sc,
                OBJECT_GET_INFO,
                &[bare as usize, INFO_HANDLE_VALID, 0, 0, 0, 0]
            ),
            0
        );
    });
}

#[test]
fn a_process_is_only_inspected_with_inspect_and_is_a_process() {
    on_a_thread(|sc, base| {
        let proc = sc.thread.proc().clone();
        let blind = add(sc, proc.clone(), Rights::DEFAULT_PROCESS - Rights::INSPECT);
        let args = [blind as usize, INFO_PROCESS, base, 64, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), zx(ZxError::ACCESS_DENIED));
        let event = add(sc, Event::new(), Rights::DEFAULT_EVENT);
        let args = [event as usize, INFO_PROCESS, base, 64, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), zx(ZxError::WRONG_TYPE));
        // A stub topic still asks for the right type before it says no.
        let args = [event as usize, INFO_PROCESS_MAPS, base, 64, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), zx(ZxError::WRONG_TYPE));
        let seen = add(sc, proc, Rights::DEFAULT_PROCESS);
        let args = [seen as usize, INFO_PROCESS_MAPS, base, 64, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), zx(ZxError::NOT_SUPPORTED));
        // The v2 record: a process nobody has `zx_process_start`ed, and that
        // has not exited.
        write(base + 16, u32::MAX);
        let args = [seen as usize, INFO_PROCESS, base, 64, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), 0);
        assert_eq!(read::<u32>(base + 16), 0, "flags");
    });
}

#[test]
fn process_threads_writes_what_fits_and_says_how_many_there_are() {
    on_a_thread(|sc, base| {
        let proc = sc.thread.proc().clone();
        let second = Thread::create(&proc, "second").unwrap();
        let h = add(sc, proc.clone(), Rights::DEFAULT_PROCESS);
        let (actual, avail) = (base + 0x100, base + 0x108);
        let args = [h as usize, INFO_PROCESS_THREADS, base, 64, actual, avail];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), 0);
        assert_eq!((read::<usize>(actual), read::<usize>(avail)), (2, 2));
        let ids = [read::<u64>(base), read::<u64>(base + 8)];
        assert!(ids.contains(&sc.thread.id()) && ids.contains(&second.id()));
        // Room for one: one written, two there.
        write(base + 8, 0u64);
        let args = [h as usize, INFO_PROCESS_THREADS, base, 8, actual, avail];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), 0);
        assert_eq!((read::<usize>(actual), read::<usize>(avail)), (1, 2));
        assert_eq!(read::<u64>(base + 8), 0, "a second id went past the buffer");
        // ENUMERATE, not INSPECT.
        let no_list = add(sc, proc, Rights::DEFAULT_PROCESS - Rights::ENUMERATE);
        let args = [no_list as usize, INFO_PROCESS_THREADS, base, 64, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), zx(ZxError::ACCESS_DENIED));
    });
}

#[test]
fn handle_count_counts_the_handles_to_the_object_in_the_process() {
    on_a_thread(|sc, base| {
        let event = Event::new();
        let h = add(sc, event.clone(), Rights::DEFAULT_EVENT);
        let _second = add(sc, event.clone(), Rights::DEFAULT_EVENT);
        let args = [h as usize, INFO_HANDLE_COUNT, base, 4, 0, 0];
        assert_eq!(call(sc, OBJECT_GET_INFO, &args), 0);
        assert_eq!(read::<u32>(base), 2);
    });
}

// ---- zx_object_get_property / zx_object_set_property ---------------------

#[test]
fn a_name_set_is_the_name_read_back_and_both_need_their_right() {
    on_a_thread(|sc, base| {
        let vmo = VmObject::new_paged(1);
        let h = add(sc, vmo.clone(), Rights::DEFAULT_VMO);
        let name = b"blobfs-cache";
        unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), base as *mut u8, name.len()) };
        assert_eq!(
            call(
                sc,
                OBJECT_SET_PROPERTY,
                &[h as usize, PROP_NAME, base, name.len()]
            ),
            0
        );
        assert_eq!(vmo.name(), "blobfs-cache");
        let out = base + 0x200;
        write(out, [0xffu8; 32]);
        assert_eq!(
            call(sc, OBJECT_GET_PROPERTY, &[h as usize, PROP_NAME, out, 32]),
            0
        );
        let got: [u8; 32] = read(out);
        assert_eq!(&got[..name.len()], name);
        assert!(
            got[name.len()..].iter().all(|&b| b == 0),
            "unterminated or stale tail"
        );
        // A buffer short of ZX_MAX_NAME_LEN.
        assert_eq!(
            call(sc, OBJECT_GET_PROPERTY, &[h as usize, PROP_NAME, out, 31]),
            zx(ZxError::BUFFER_TOO_SMALL)
        );
        let ro = add(sc, vmo.clone(), Rights::DEFAULT_VMO - Rights::SET_PROPERTY);
        assert_eq!(
            call(sc, OBJECT_SET_PROPERTY, &[ro as usize, PROP_NAME, base, 4]),
            zx(ZxError::ACCESS_DENIED)
        );
        assert_eq!(vmo.name(), "blobfs-cache");
        let blind = add(sc, vmo, Rights::DEFAULT_VMO - Rights::GET_PROPERTY);
        assert_eq!(
            call(
                sc,
                OBJECT_GET_PROPERTY,
                &[blind as usize, PROP_NAME, out, 32]
            ),
            zx(ZxError::ACCESS_DENIED)
        );
    });
}

#[test]
fn a_bad_handle_is_reported_before_an_unknown_property() {
    on_a_thread(|sc, base| {
        for num in [OBJECT_GET_PROPERTY, OBJECT_SET_PROPERTY] {
            assert_eq!(
                call(sc, num, &[0xdead, 0x7777, base, 32]),
                zx(ZxError::BAD_HANDLE)
            );
        }
        let h = add(sc, VmObject::new_paged(1), Rights::DEFAULT_VMO);
        for num in [OBJECT_GET_PROPERTY, OBJECT_SET_PROPERTY] {
            assert_eq!(
                call(sc, num, &[h as usize, 0x7777, base, 32]),
                zx(ZxError::INVALID_ARGS)
            );
        }
    });
}

#[test]
fn a_vmo_content_size_is_set_only_through_a_handle_that_may_write() {
    on_a_thread(|sc, base| {
        let vmo = VmObject::new_paged(2);
        let h = add(sc, vmo.clone(), Rights::DEFAULT_VMO);
        write(base, 5000usize);
        assert_eq!(
            call(
                sc,
                OBJECT_SET_PROPERTY,
                &[h as usize, PROP_VMO_CONTENT_SIZE, base, 8]
            ),
            0
        );
        assert_eq!(vmo.content_size(), 5000);
        let ro = add(sc, vmo.clone(), Rights::DEFAULT_VMO - Rights::WRITE);
        write(base, 10usize);
        assert_eq!(
            call(
                sc,
                OBJECT_SET_PROPERTY,
                &[ro as usize, PROP_VMO_CONTENT_SIZE, base, 8]
            ),
            zx(ZxError::ACCESS_DENIED)
        );
        assert_eq!(vmo.content_size(), 5000);
        // Read back through a handle that only reads, into too small a buffer
        // and then into one that fits.
        assert_eq!(
            call(
                sc,
                OBJECT_GET_PROPERTY,
                &[ro as usize, PROP_VMO_CONTENT_SIZE, base, 4]
            ),
            zx(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(
            call(
                sc,
                OBJECT_GET_PROPERTY,
                &[ro as usize, PROP_VMO_CONTENT_SIZE, base, 8]
            ),
            0
        );
        assert_eq!(read::<usize>(base), 5000);
    });
}

// ---- zx_object_signal / zx_object_signal_peer ------------------------------

#[test]
fn a_process_sets_only_the_user_signals_and_only_with_the_signal_right() {
    on_a_thread(|sc, _| {
        let event = Event::new();
        let h = add(sc, event.clone(), Rights::DEFAULT_EVENT);
        let user0 = Signal::USER_SIGNAL_0.bits() as usize;
        assert_eq!(call(sc, OBJECT_SIGNAL, &[h as usize, 0, user0]), 0);
        assert!(event.signal().contains(Signal::USER_SIGNAL_0));
        assert_eq!(call(sc, OBJECT_SIGNAL, &[h as usize, user0, 0]), 0);
        assert!(!event.signal().contains(Signal::USER_SIGNAL_0));
        // READABLE is the kernel's to say.
        let readable = Signal::READABLE.bits() as usize;
        assert_eq!(
            call(sc, OBJECT_SIGNAL, &[h as usize, 0, readable]),
            zx(ZxError::INVALID_ARGS)
        );
        assert!(!event.signal().contains(Signal::READABLE));
        let mute = add(sc, event.clone(), Rights::DEFAULT_EVENT - Rights::SIGNAL);
        assert_eq!(
            call(sc, OBJECT_SIGNAL, &[mute as usize, 0, user0]),
            zx(ZxError::ACCESS_DENIED)
        );
        assert!(!event.signal().contains(Signal::USER_SIGNAL_0));
    });
}

#[test]
fn signal_peer_reaches_the_peer_and_says_peer_closed_before_a_bad_mask() {
    on_a_thread(|sc, _| {
        let (ours, theirs) = EventPair::create();
        let h = add(sc, ours, Rights::DEFAULT_EVENTPAIR);
        let user1 = Signal::USER_SIGNAL_1.bits() as usize;
        assert_eq!(call(sc, OBJECT_SIGNAL_PEER, &[h as usize, 0, user1]), 0);
        assert!(theirs.signal().contains(Signal::USER_SIGNAL_1));
        // Only with SIGNAL_PEER.
        let ours2 = add(
            sc,
            sc.thread
                .proc()
                .get_dyn_object_with_rights(h, Rights::empty())
                .unwrap(),
            Rights::DEFAULT_EVENTPAIR - Rights::SIGNAL_PEER,
        );
        assert_eq!(
            call(sc, OBJECT_SIGNAL_PEER, &[ours2 as usize, user1, 0]),
            zx(ZxError::ACCESS_DENIED)
        );
        assert!(theirs.signal().contains(Signal::USER_SIGNAL_1));
        drop(theirs);
        let readable = Signal::READABLE.bits() as usize;
        assert_eq!(
            call(sc, OBJECT_SIGNAL_PEER, &[h as usize, 0, readable]),
            zx(ZxError::PEER_CLOSED)
        );
        // An event has no peer at all.
        let event = add(
            sc,
            Event::new(),
            Rights::DEFAULT_EVENT | Rights::SIGNAL_PEER,
        );
        assert_eq!(
            call(sc, OBJECT_SIGNAL_PEER, &[event as usize, 0, user1]),
            zx(ZxError::NOT_SUPPORTED)
        );
    });
}

// ---- zx_object_wait_one / zx_object_wait_many ------------------------------

#[test]
fn wait_one_on_a_signal_already_up_answers_at_once_with_what_was_observed() {
    on_a_thread(|sc, base| {
        let event = Event::new();
        event.signal_set(Signal::USER_SIGNAL_2);
        let h = add(sc, event, Rights::DEFAULT_EVENT);
        let user2 = Signal::USER_SIGNAL_2.bits() as usize;
        write(base, 0u32);
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ONE,
                &[h as usize, user2, i64::MAX as usize, base]
            ),
            0
        );
        assert!(Signal::from_bits_truncate(read::<u32>(base)).contains(Signal::USER_SIGNAL_2));
        // WAIT is the right it takes.
        let blind = add(
            sc,
            sc.thread
                .proc()
                .get_dyn_object_with_rights(h, Rights::empty())
                .unwrap(),
            Rights::DEFAULT_EVENT - Rights::WAIT,
        );
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ONE,
                &[blind as usize, user2, i64::MAX as usize, base]
            ),
            zx(ZxError::ACCESS_DENIED)
        );
    });
}

#[test]
fn wait_many_takes_at_most_32_items_and_every_handle_must_wait() {
    on_a_thread(|sc, base| {
        assert_eq!(
            call(sc, OBJECT_WAIT_MANY, &[base, 33, 0]),
            zx(ZxError::OUT_OF_RANGE)
        );
        let up = Event::new();
        up.signal_set(Signal::USER_SIGNAL_0);
        let a = add(sc, up.clone(), Rights::DEFAULT_EVENT);
        let b = add(sc, Event::new(), Rights::DEFAULT_EVENT);
        let user0 = Signal::USER_SIGNAL_0.bits();
        // `zx_wait_item_t`: handle, waitfor, pending.
        let items = |second: u32| [[a, user0, 0], [second, user0, 0]];
        write(base, items(0xdead));
        assert_eq!(
            call(sc, OBJECT_WAIT_MANY, &[base, 2, i64::MAX as usize]),
            zx(ZxError::BAD_HANDLE)
        );
        write(base, items(b));
        assert_eq!(call(sc, OBJECT_WAIT_MANY, &[base, 2, i64::MAX as usize]), 0);
        let got: [[u32; 3]; 2] = read(base);
        assert_ne!(got[0][2] & user0, 0, "the raised signal is observed");
        assert_eq!(got[1][2] & user0, 0, "a signal nobody raised is not");
    });
}

// ---- zx_object_wait_async ----------------------------------------------------

#[test]
fn wait_async_checks_both_handles_before_the_options() {
    on_a_thread(|sc, _| {
        let port = add(sc, Port::new(0).unwrap(), Rights::DEFAULT_PORT);
        let event = add(sc, Event::new(), Rights::DEFAULT_EVENT);
        let user0 = Signal::USER_SIGNAL_0.bits() as usize;
        let bad_options = 0x8000_0000;
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ASYNC,
                &[0xdead, port as usize, 1, user0, bad_options]
            ),
            zx(ZxError::BAD_HANDLE)
        );
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ASYNC,
                &[event as usize, event as usize, 1, user0, bad_options]
            ),
            zx(ZxError::WRONG_TYPE)
        );
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ASYNC,
                &[event as usize, port as usize, 1, user0, bad_options]
            ),
            zx(ZxError::INVALID_ARGS)
        );
        // ZX_WAIT_ASYNC_TIMESTAMP (1) and ZX_WAIT_ASYNC_BOOT_TIMESTAMP (4) together.
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ASYNC,
                &[event as usize, port as usize, 1, user0, 0b101]
            ),
            zx(ZxError::INVALID_ARGS)
        );
        let ro_port = add(
            sc,
            sc.thread
                .proc()
                .get_dyn_object_with_rights(port, Rights::empty())
                .unwrap(),
            Rights::DEFAULT_PORT - Rights::WRITE,
        );
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ASYNC,
                &[event as usize, ro_port as usize, 1, user0, 0]
            ),
            zx(ZxError::ACCESS_DENIED)
        );
    });
}

#[test]
fn wait_async_queues_a_packet_with_its_key_once_the_signal_comes() {
    on_a_thread(|sc, _| {
        let port_object = Port::new(0).unwrap();
        let port = add(sc, port_object.clone(), Rights::DEFAULT_PORT);
        let event = Event::new();
        let h = add(sc, event.clone(), Rights::DEFAULT_EVENT);
        let user0 = Signal::USER_SIGNAL_0.bits() as usize;
        assert_eq!(
            call(
                sc,
                OBJECT_WAIT_ASYNC,
                &[h as usize, port as usize, 0x5eed, user0, 0]
            ),
            0
        );
        let mut wait = Box::pin(port_object.wait());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            wait.as_mut().poll(&mut cx).is_pending(),
            "a packet before the signal"
        );
        event.signal_set(Signal::USER_SIGNAL_0);
        match wait.as_mut().poll(&mut cx) {
            Poll::Ready(packet) => assert_eq!(packet.key, 0x5eed),
            Poll::Pending => panic!("no packet after the signal"),
        }
    });
}

// ---- zx_object_get_child -------------------------------------------------------

#[test]
fn get_child_wants_a_task_enumerate_and_no_more_rights_than_the_parent() {
    on_a_thread(|sc, base| {
        let job = Job::root().create_child().unwrap();
        let child = Process::create(&job, "child").unwrap();
        let koid = child.id() as usize;
        let out = base;
        let event = add(sc, Event::new(), Rights::DEFAULT_EVENT);
        let same = Rights::SAME_RIGHTS.bits() as usize;
        assert_eq!(
            call(sc, OBJECT_GET_CHILD, &[event as usize, koid, same, out]),
            zx(ZxError::WRONG_TYPE)
        );
        let blind = add(sc, job.clone(), Rights::DEFAULT_JOB - Rights::ENUMERATE);
        assert_eq!(
            call(sc, OBJECT_GET_CHILD, &[blind as usize, koid, same, out]),
            zx(ZxError::ACCESS_DENIED)
        );
        // A parent handle without WRITE cannot hand out a child that has it.
        let parent = add(sc, job.clone(), Rights::DEFAULT_JOB - Rights::WRITE);
        let more = (Rights::READ | Rights::WRITE).bits() as usize;
        assert_eq!(
            call(sc, OBJECT_GET_CHILD, &[parent as usize, koid, more, out]),
            zx(ZxError::ACCESS_DENIED)
        );
        assert_eq!(
            call(sc, OBJECT_GET_CHILD, &[parent as usize, koid, 1 << 30, out]),
            zx(ZxError::INVALID_ARGS)
        );
        // SAME_RIGHTS: the parent handle's.
        assert_eq!(
            call(sc, OBJECT_GET_CHILD, &[parent as usize, koid, same, out]),
            0
        );
        let got = read::<u32>(out);
        let (object, rights) = sc.thread.proc().get_dyn_object_and_rights(got).unwrap();
        assert_eq!(object.id(), child.id());
        assert_eq!(rights, Rights::DEFAULT_JOB - Rights::WRITE);
        // A koid that is not a child.
        assert_eq!(
            call(
                sc,
                OBJECT_GET_CHILD,
                &[parent as usize, 0x7fff_ffff, same, out]
            ),
            zx(ZxError::NOT_FOUND)
        );
    });
}
