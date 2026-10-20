extern crate std;

use super::*;
use crate::frontend::tests::{answer_with, turnstile};
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::AtomicBool;
use std::cell::RefCell;
use std::rc::Rc;

/// Sets `gone` when the pages it keeps are unpinned, and `under_rm` if that
/// was inside an RM call.
struct Unpinned {
    gone: Arc<AtomicBool>,
    under_rm: Arc<AtomicBool>,
}

impl Drop for Unpinned {
    fn drop(&mut self) {
        self.under_rm
            .store(IN_CALL.load(Ordering::SeqCst), Ordering::SeqCst);
        self.gone.store(true, Ordering::SeqCst);
    }
}

/// Three pages at 0x7000_0000, and the flags their unpinning sets.
fn three_pages(writable: bool) -> (UserPages, Arc<AtomicBool>, Arc<AtomicBool>) {
    let gone = Arc::new(AtomicBool::new(false));
    let under_rm = Arc::new(AtomicBool::new(false));
    let pages = UserPages {
        va: 0x7000_0000,
        phys: vec![0x1_0000, 0x5_0000, 0x2_0000],
        writable,
        keep: Box::new(Unpinned {
            gone: gone.clone(),
            under_rm: under_rm.clone(),
        }),
    };
    (pages, gone, under_rm)
}

fn caller() -> Caller {
    Caller {
        os_info: 0x1000,
        ctl: false,
        admin: false,
        gpu_mask: 0,
    }
}

fn lock_at(va: usize, count: u64, flags: NvU32) -> (NV_STATUS, *mut c_void) {
    let mut array = core::ptr::null_mut();
    let status = lock(va as *mut c_void, count, &mut array, flags);
    (status, array)
}

#[test]
fn only_the_range_that_was_pinned_is_taken() {
    let (pages, _, _) = three_pages(false);
    let mut lent = Some(pages);
    for (va, count, write) in [
        (0x7000_1000, 3, false),
        (0x7000_0000, 2, false),
        (0x7000_0000, 4, false),
        // Pinned for reading only: nothing the RM may write through.
        (0x7000_0000, 3, true),
    ] {
        assert!(take_if_fits(&mut lent, va, count, write).is_none());
        assert!(lent.is_some(), "a refusal leaves the pages lent");
    }
    let taken = take_if_fits(&mut lent, 0x7000_0000, 3, false).expect("the very range");
    assert_eq!(taken.phys, [0x1_0000, 0x5_0000, 0x2_0000]);
    assert!(lent.is_none());
    assert!(
        take_if_fits(&mut lent, 0x7000_0000, 3, false).is_none(),
        "and once"
    );
}

#[test]
fn pages_pinned_for_writing_can_be_read() {
    let (pages, _, _) = three_pages(true);
    let mut lent = Some(pages);
    assert!(take_if_fits(&mut lent, 0x7000_0000, 3, false).is_some());
}

#[test]
fn nothing_is_locked_outside_a_call() {
    let _t = turnstile();
    let (status, array) = lock_at(0x7000_0000, 3, 0);
    assert_eq!(status, NV_ERR_NOT_SUPPORTED);
    assert!(array.is_null());
}

#[test]
fn the_rm_takes_the_pages_it_was_lent_and_unpins_them_when_done() {
    let _t = turnstile();
    let (pages, gone, _) = three_pages(true);
    let array = Rc::new(RefCell::new(core::ptr::null_mut()));
    let seen = array.clone();
    answer_with(move |cmd| {
        assert_eq!(cmd, 0x27);
        // Through the entry point the RM calls.
        let mut array = core::ptr::null_mut();
        let status = crate::os_interface::os_lock_user_pages(
            0x7000_0000 as *mut c_void,
            3,
            &mut array,
            LOCK_USER_PAGES_FLAGS_WRITE,
        );
        *seen.borrow_mut() = array;
        status
    });
    let status = rm_escape_lending(0x27, &mut [0u8; 56], &caller(), Some(pages));
    assert_eq!(status, NV_OK);
    let array = *array.borrow();
    assert!(!array.is_null());
    assert!(
        !gone.load(Ordering::SeqCst),
        "the RM's memory still holds them after the call"
    );
    assert_eq!(eclipse_user_pages_count(array), 3);
    assert_eq!(eclipse_user_pages_phys(array, 0), 0x1_0000);
    assert_eq!(eclipse_user_pages_phys(array, 1), 0x5_0000);
    assert_eq!(eclipse_user_pages_phys(array, 2), 0x2_0000);
    assert_eq!(eclipse_user_pages_phys(array, 3), 0, "past the end");
    assert_eq!(
        crate::os_interface::os_unlock_user_pages(3, array, LOCK_USER_PAGES_FLAGS_WRITE),
        NV_OK
    );
    assert!(gone.load(Ordering::SeqCst));
}

#[test]
fn pages_pinned_for_reading_are_not_lent_to_a_writer() {
    let _t = turnstile();
    let (pages, gone, _) = three_pages(false);
    answer_with(|_| {
        let (status, array) = lock_at(0x7000_0000, 3, LOCK_USER_PAGES_FLAGS_WRITE);
        assert_eq!(status, NV_ERR_INVALID_ADDRESS);
        assert!(array.is_null());
        let (status, array) = lock_at(0x7000_0000, 3, 0);
        assert_eq!(status, NV_OK, "but they are to a reader");
        unlock(array)
    });
    assert_eq!(
        rm_escape_lending(0x27, &mut [0u8; 56], &caller(), Some(pages)),
        NV_OK
    );
    assert!(gone.load(Ordering::SeqCst));
}

#[test]
fn no_place_to_put_the_array_takes_nothing() {
    let _t = turnstile();
    let (pages, gone, _) = three_pages(true);
    answer_with(|_| {
        let null = core::ptr::null_mut();
        assert_eq!(
            lock(0x7000_0000 as *mut c_void, 3, null, 0),
            NV_ERR_INVALID_ARGUMENT
        );
        let (status, array) = lock_at(0x7000_0000, 3, 0);
        assert!(!array.is_null(), "the pages were still there to take");
        assert_eq!(unlock(array), NV_OK);
        status
    });
    let status = rm_escape_lending(0x27, &mut [0u8; 56], &caller(), Some(pages));
    assert_eq!(status, NV_OK);
    assert!(gone.load(Ordering::SeqCst));
}

#[test]
fn pages_the_rm_does_not_take_are_unpinned_after_the_call() {
    let _t = turnstile();
    let (pages, gone, under_rm) = three_pages(true);
    answer_with(|_| {
        // Not the range that was pinned: refused, and the pages stay lent.
        let (status, array) = lock_at(0x7000_0000, 2, 0);
        assert_eq!(status, NV_ERR_INVALID_ADDRESS);
        assert!(array.is_null());
        NV_OK
    });
    assert_eq!(
        rm_escape_lending(0x27, &mut [0u8; 56], &caller(), Some(pages)),
        NV_OK
    );
    assert!(
        gone.load(Ordering::SeqCst),
        "unpinned once the call is over"
    );
    assert!(!under_rm.load(Ordering::SeqCst), "and not under the RM");

    // And nothing of them is left for the next call to take.
    answer_with(|_| lock_at(0x7000_0000, 3, 0).0);
    assert_eq!(
        rm_escape_lending(0x27, &mut [0u8; 56], &caller(), None),
        NV_ERR_INVALID_ADDRESS
    );
}

#[test]
fn a_caller_with_no_open_lends_nothing() {
    let _t = turnstile();
    let (pages, gone, _) = three_pages(false);
    let nobody = Caller {
        os_info: 0,
        ..caller()
    };
    assert_eq!(
        rm_escape_lending(0x27, &mut [0u8; 56], &nobody, Some(pages)),
        NV_ERR_INVALID_ARGUMENT
    );
    assert!(gone.load(Ordering::SeqCst));
}

#[test]
fn no_page_array_answers_for_nothing() {
    let null = core::ptr::null_mut();
    assert_eq!(eclipse_user_pages_count(null), 0);
    assert_eq!(eclipse_user_pages_phys(null, 0), 0);
    assert_eq!(unlock(null), NV_OK);
}
