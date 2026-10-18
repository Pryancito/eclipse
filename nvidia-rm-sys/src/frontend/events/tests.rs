extern crate std;

use super::*;
use std::sync::{Mutex as StdMutex, MutexGuard};
use std::vec::Vec;

// Most of these work on a `Registrations` of their own. The ones that go
// through the RM's callbacks share `REGISTRATIONS` with every other test in
// the process, so each names a client and an open no other test uses, and
// none counts what is listed there.

const OPEN: usize = 0x1000;
const OTHER_OPEN: usize = 0x2000;

#[test]
fn a_client_registers_an_fd_once() {
    let mut r = Registrations::new();
    assert_eq!(r.alloc(1, OPEN, 7), NV_OK);
    assert_eq!(r.alloc(1, OPEN, 7), NV_ERR_INSUFFICIENT_RESOURCES);
    assert_eq!(
        r.alloc(1, OTHER_OPEN, 7),
        NV_ERR_INSUFFICIENT_RESOURCES,
        "the pair is (client, fd), whichever open asks"
    );
    assert_eq!(r.alloc(1, OPEN, 8), NV_OK, "another fd");
    assert_eq!(r.alloc(2, OPEN, 7), NV_OK, "another client");
    assert_eq!(r.len(), 3);
}

#[test]
fn the_rm_finds_a_registration_by_client_and_fd_and_references_it() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 7);
    r.alloc(1, OPEN, 8);
    let id = r.reference_by_fd(1, 7).expect("registered");
    assert_ne!(id, 0, "an id is never NULL to the RM");
    assert_eq!(r.list[0].refcount, 1);
    assert_ne!(r.reference_by_fd(1, 8), Some(id), "each its own id");
    assert_eq!(r.reference_by_fd(2, 7), None, "another client's fd 7");
    assert_eq!(r.reference_by_fd(1, 9), None);
}

#[test]
fn freeing_answers_whether_there_was_a_registration() {
    let mut r = Registrations::new();
    assert_eq!(r.free(1, 7), NV_ERR_INVALID_EVENT);
    r.alloc(1, OPEN, 7);
    assert_eq!(r.free(1, 7), NV_OK);
    assert_eq!(r.len(), 0, "nothing held it");
    assert_eq!(r.free(1, 7), NV_ERR_INVALID_EVENT, "not twice");
}

/// `free_os_event_under_lock` and `osDereferenceObjectCount`: the client
/// can free a registration the RM still holds. Nothing reaches it after
/// that, and it goes with the RM's last reference.
#[test]
fn a_freed_registration_stays_while_the_rm_holds_it() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 7);
    let id = r.reference_by_fd(1, 7).unwrap();
    assert_eq!(r.free(1, 7), NV_OK);
    assert_eq!(r.len(), 1, "the RM's event object still holds it");
    assert_eq!(
        r.reference_for_post(id),
        None,
        "but nothing is posted to it"
    );
    assert_eq!(r.reference_by_fd(1, 7), None, "nor found by its fd");
    assert_eq!(r.free(1, 7), NV_ERR_INVALID_EVENT, "nor freed again");
    assert_eq!(
        r.alloc(1, OPEN, 7),
        NV_OK,
        "and the fd can be registered anew"
    );
    assert_ne!(r.reference_by_fd(1, 7), Some(id), "as a new registration");
    assert!(r.unreference(id));
    assert_eq!(r.len(), 1, "the old one is gone, the new one stays");
    assert!(!r.unreference(id), "and its id finds nothing");
}

/// A registration nobody freed outlives the RM object that held it: the
/// client can bind another one to the same fd.
#[test]
fn an_active_registration_stays_when_the_rm_lets_go() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 7);
    let id = r.reference_by_fd(1, 7).unwrap();
    assert!(r.unreference(id));
    assert_eq!(r.len(), 1);
    assert_eq!(r.reference_by_fd(1, 7), Some(id));
}

/// `referenceEventForPost`: a post holds the registration, so a free in the
/// middle of one does not take it from under the post.
#[test]
fn a_post_holds_the_registration_until_it_is_done() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 7);
    let id = r.reference_by_fd(1, 7).unwrap();
    assert_eq!(r.reference_for_post(id), Some(OPEN));
    assert_eq!(r.free(1, 7), NV_OK);
    assert!(r.unreference(id), "the RM's object goes");
    assert_eq!(r.len(), 1, "the post still holds it");
    assert!(r.unreference(id), "the post is done");
    assert_eq!(r.len(), 0);
}

#[test]
fn a_post_finds_the_open_it_was_registered_on() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 7);
    r.alloc(1, OTHER_OPEN, 8);
    let first = r.reference_by_fd(1, 7).unwrap();
    let second = r.reference_by_fd(1, 8).unwrap();
    assert_eq!(r.reference_for_post(second), Some(OTHER_OPEN));
    assert_eq!(r.reference_for_post(first), Some(OPEN));
    assert_eq!(r.reference_for_post(0), None, "an id never handed out");
}

/// `RmFreeUnusedClients`: a close frees what was registered on that open,
/// whoever's client it was for, and nothing registered on another.
#[test]
fn a_close_frees_the_registrations_made_on_that_open() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 3);
    r.alloc(1, OTHER_OPEN, 4);
    r.alloc(2, OPEN, 5);
    r.alloc(2, OTHER_OPEN, 6);
    r.alloc(3, OPEN, 9);
    r.free_open(OPEN);
    assert_eq!(r.reference_by_fd(1, 3), None);
    assert_eq!(r.reference_by_fd(2, 5), None);
    assert_eq!(r.reference_by_fd(3, 9), None);
    assert!(r.reference_by_fd(1, 4).is_some());
    assert!(r.reference_by_fd(2, 6).is_some());
    assert_eq!(r.len(), 2);
}

/// `rm_client_free_os_events`: an RM_FREE of a whole client frees its
/// registrations, on every open.
#[test]
fn freeing_a_client_frees_its_registrations_on_every_open() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 3);
    r.alloc(2, OPEN, 4);
    r.alloc(1, OTHER_OPEN, 5);
    r.alloc(1, OPEN, 6);
    r.free_client(1);
    assert_eq!(r.reference_by_fd(1, 3), None);
    assert_eq!(r.reference_by_fd(1, 5), None);
    assert_eq!(r.reference_by_fd(1, 6), None);
    assert!(r.reference_by_fd(2, 4).is_some());
    assert_eq!(r.len(), 1);
}

/// Linux's single walk matches either key, so a close (client 0) also took
/// every registration made for client 0 on any open. Here a close is a close.
#[test]
fn a_close_does_not_take_another_opens_client_zero() {
    let mut r = Registrations::new();
    r.alloc(0, OTHER_OPEN, 3);
    r.free_open(OPEN);
    assert!(r.reference_by_fd(0, 3).is_some());
}

/// One the RM still holds is retired by a close, not dropped.
#[test]
fn a_close_leaves_what_the_rm_holds_until_it_lets_go() {
    let mut r = Registrations::new();
    r.alloc(1, OPEN, 3);
    let id = r.reference_by_fd(1, 3).unwrap();
    r.free_open(OPEN);
    assert_eq!(r.len(), 1);
    assert_eq!(r.reference_for_post(id), None);
    assert!(r.unreference(id));
    assert_eq!(r.len(), 0);
}

#[test]
fn there_is_a_most_registrations() {
    let mut r = Registrations::new();
    for fd in 0..MAX_REGISTRATIONS as u32 {
        assert_eq!(r.alloc(1, OPEN, fd), NV_OK);
    }
    assert_eq!(
        r.alloc(1, OPEN, MAX_REGISTRATIONS as u32),
        NV_ERR_INSUFFICIENT_RESOURCES
    );
    assert_eq!(r.free(1, 0), NV_OK);
    assert_eq!(r.alloc(1, OPEN, MAX_REGISTRATIONS as u32), NV_OK);
}

/// `NvUnixEvent` as `get_os_event_data` fills it.
#[test]
fn an_event_has_the_nvunixevent_layout() {
    let event = UnixEvent {
        h_object: 0x1122_3344,
        notify_index: 0x0000_0005,
        info32: 0xa1b2_c3d4,
        info16: 0xbeef,
    };
    assert_eq!(
        event.to_bytes(),
        [0x44, 0x33, 0x22, 0x11, 0x05, 0, 0, 0, 0xd4, 0xc3, 0xb2, 0xa1, 0xef, 0xbe, 0, 0]
    );
}

// ---- Through the RM's callbacks, on the process-wide registrations ----

/// `SINK` is process-wide: the tests that set it take turns.
fn turnstile() -> MutexGuard<'static, ()> {
    static T: StdMutex<()> = StdMutex::new(());
    T.lock().unwrap_or_else(|e| e.into_inner())
}

static POSTED: StdMutex<Vec<(usize, Option<UnixEvent>)>> = StdMutex::new(Vec::new());

fn record(os_info: usize, event: Option<UnixEvent>) {
    POSTED.lock().unwrap().push((os_info, event));
}

/// What was posted to `os_info` since the last call, which forgets it.
fn posted_to(os_info: usize) -> Vec<Option<UnixEvent>> {
    let mut posted = POSTED.lock().unwrap();
    let mine = posted
        .iter()
        .filter(|&&(o, _)| o == os_info)
        .map(|&(_, e)| e)
        .collect();
    posted.retain(|&(o, _)| o != os_info);
    mine
}

/// What `osUserHandleToKernelPtr` hands the RM for `fd`, or its status.
fn kernel_ptr(client: u32, fd: u32) -> Result<*mut c_void, NV_STATUS> {
    let mut out = core::ptr::null_mut();
    match osUserHandleToKernelPtr(client, fd as usize as *mut c_void, &mut out) {
        NV_OK => Ok(out),
        status => Err(status),
    }
}

#[test]
fn the_rm_trades_the_fd_number_for_the_registration() {
    const CLIENT: u32 = 0xc1d0_0001;
    // Registers nothing in any test: `CLIENT + 1` is the next test's client,
    // which holds fd 7 while it runs.
    const STRANGER: u32 = 0xc1d0_00ff;
    const MINE: usize = 0x00e1_0001;
    assert_eq!(alloc_os_event(CLIENT, MINE, 7), NV_OK);
    let event = kernel_ptr(CLIENT, 7).unwrap();
    assert!(!event.is_null());
    assert_eq!(kernel_ptr(CLIENT, 8), Err(NV_ERR_OBJECT_NOT_FOUND));
    assert_eq!(kernel_ptr(STRANGER, 7), Err(NV_ERR_OBJECT_NOT_FOUND));
    assert_eq!(
        osUserHandleToKernelPtr(CLIENT, 7 as *mut c_void, core::ptr::null_mut()),
        NV_ERR_INVALID_ARGUMENT
    );
    // The upper half of the 64-bit handle is not part of the fd.
    let wide = (1usize << 32) | 7;
    let mut out = core::ptr::null_mut();
    assert_eq!(
        osUserHandleToKernelPtr(CLIENT, wide as *mut c_void, &mut out),
        NV_OK
    );
    assert_eq!(out, event);
    assert_eq!(osDereferenceObjectCount(event), NV_OK);
    assert_eq!(osDereferenceObjectCount(event), NV_OK);
    assert_eq!(free_os_event(CLIENT, 7), NV_OK);
    assert_eq!(
        osDereferenceObjectCount(event),
        NV_ERR_INVALID_EVENT,
        "gone once freed and let go"
    );
}

#[test]
fn an_event_reaches_the_open_it_was_registered_on() {
    const CLIENT: u32 = 0xc1d0_0002;
    const MINE: usize = 0x00e1_0002;
    let _t = turnstile();
    set_event_sink(record);
    posted_to(MINE);
    assert_eq!(alloc_os_event(CLIENT, MINE, 7), NV_OK);
    let event = kernel_ptr(CLIENT, 7).unwrap();

    eclipse_rm_frontend_post_event(event, 0xbeef_0001, 3, 0x55, 0x66, 1);
    let with_data = UnixEvent {
        h_object: 0xbeef_0001,
        notify_index: 3,
        info32: 0x55,
        info16: 0x66,
    };
    assert_eq!(posted_to(MINE), [Some(with_data)]);

    eclipse_rm_frontend_post_event(event, 0xbeef_0001, 3, 0x55, 0x66, 0);
    assert_eq!(posted_to(MINE), [None], "an event without data");

    assert_eq!(osSetEvent(core::ptr::null_mut(), event), 1);
    assert_eq!(posted_to(MINE), [None], "osSetEvent's has none");

    assert_eq!(free_os_event(CLIENT, 7), NV_OK);
    eclipse_rm_frontend_post_event(event, 0xbeef_0001, 3, 0, 0, 1);
    assert_eq!(osSetEvent(core::ptr::null_mut(), event), 1);
    assert_eq!(posted_to(MINE), [], "nothing once the client freed it");
    assert_eq!(osDereferenceObjectCount(event), NV_OK);
}

/// A post lets go of the registration it held: once the client and the RM
/// are done with it, it is gone.
#[test]
fn a_post_lets_go_of_the_registration() {
    const CLIENT: u32 = 0xc1d0_0003;
    const MINE: usize = 0x00e1_0003;
    let _t = turnstile();
    set_event_sink(record);
    assert_eq!(alloc_os_event(CLIENT, MINE, 7), NV_OK);
    let event = kernel_ptr(CLIENT, 7).unwrap();
    for _ in 0..3 {
        osSetEvent(core::ptr::null_mut(), event);
    }
    assert_eq!(posted_to(MINE).len(), 3);
    assert_eq!(free_os_event(CLIENT, 7), NV_OK);
    assert_eq!(osDereferenceObjectCount(event), NV_OK);
    assert_eq!(osDereferenceObjectCount(event), NV_ERR_INVALID_EVENT);
}

/// An id the RM holds after it was let go, or never had, posts nothing.
#[test]
fn a_stale_id_posts_nothing() {
    const CLIENT: u32 = 0xc1d0_0004;
    const MINE: usize = 0x00e1_0004;
    let _t = turnstile();
    set_event_sink(record);
    assert_eq!(alloc_os_event(CLIENT, MINE, 7), NV_OK);
    let event = kernel_ptr(CLIENT, 7).unwrap();
    assert_eq!(free_os_event(CLIENT, 7), NV_OK);
    assert_eq!(osDereferenceObjectCount(event), NV_OK);
    osSetEvent(core::ptr::null_mut(), event);
    eclipse_rm_frontend_post_event(event, 1, 2, 3, 4, 1);
    assert_eq!(posted_to(MINE), []);
}

/// escape.c's RM_FREE of a whole client.
#[test]
fn freeing_the_client_frees_its_registrations() {
    const CLIENT: u32 = 0xc1d0_0005;
    const MINE: usize = 0x00e1_0005;
    assert_eq!(alloc_os_event(CLIENT, MINE, 7), NV_OK);
    assert_eq!(alloc_os_event(CLIENT, MINE + 1, 8), NV_OK);
    rm_client_free_os_events(CLIENT);
    assert_eq!(kernel_ptr(CLIENT, 7), Err(NV_ERR_OBJECT_NOT_FOUND));
    assert_eq!(kernel_ptr(CLIENT, 8), Err(NV_ERR_OBJECT_NOT_FOUND));
    assert_eq!(free_os_event(CLIENT, 7), NV_ERR_INVALID_EVENT);
}
