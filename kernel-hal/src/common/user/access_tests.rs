//! A copy to or from user memory asks whether the pages allow it, not only
//! whether they are mapped. The copy faults like the user would, and a fault
//! the address space refuses has no fixup here: a `write(2)` from a
//! `PROT_NONE` page or a `clock_gettime` into a `PROT_READ` one was a kernel
//! page fault, and with a lock held, a halted machine.
//!
//! The test handler refuses one access to a host buffer that stays readable
//! and writable underneath, so what is asserted is that the question is
//! asked, and that nothing is copied when the answer is no.

use super::*;
use crate::utils::test_frames::{allow_user_access, install, refuse_user_access};

/// A host buffer of `pages` pages starting on a page boundary, filled with
/// `fill`. Leaked: `UserPtr` hands out `&'static` slices.
fn pages(pages: usize, fill: u8) -> usize {
    let buf = alloc::vec![fill; (pages + 1) * PAGE_SIZE].leak();
    (buf.as_ptr() as usize + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

fn bytes_at(addr: usize, len: usize) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(addr as *const u8, len) }
}

#[test]
fn a_write_into_a_page_that_may_only_be_read_is_refused_and_stores_nothing() {
    install();
    let at = pages(1, 0x5a);
    refuse_user_access(at, PAGE_SIZE, MMUFlags::WRITE);
    let mut word = UserOutPtr::<u32>::from(at);
    assert_eq!(word.write(7), Err(Error::InvalidPointer), "write");
    assert_eq!(word.write_if_not_null(7), Err(Error::InvalidPointer));
    let mut bytes = UserOutPtr::<u8>::from(at);
    assert_eq!(
        bytes.write_array(&[1, 2, 3]),
        Err(Error::InvalidPointer),
        "write_array"
    );
    assert_eq!(
        bytes.write_cstring("ab"),
        Err(Error::InvalidPointer),
        "write_cstring"
    );
    assert!(
        bytes_at(at, 8).iter().all(|&b| b == 0x5a),
        "a byte was stored"
    );
    // Reading it is still fine.
    assert_eq!(UserInPtr::<u32>::from(at).read(), Ok(0x5a5a_5a5a));
    assert_eq!(UserInOutPtr::<u32>::from(at).read(), Ok(0x5a5a_5a5a));
    allow_user_access(at, PAGE_SIZE, MMUFlags::WRITE);
    word.write(7).unwrap();
}

#[test]
fn a_read_from_a_page_that_may_not_be_read_is_refused() {
    install();
    let at = pages(1, b'x');
    refuse_user_access(at, PAGE_SIZE, MMUFlags::READ);
    let ptr = UserInPtr::<u8>::from(at);
    assert_eq!(ptr.read(), Err(Error::InvalidPointer), "read");
    assert_eq!(ptr.read_if_not_null(), Err(Error::InvalidPointer));
    assert!(ptr.as_slice(4).is_err(), "as_slice");
    assert!(ptr.read_array(4).is_err(), "read_array");
    assert!(ptr.as_str(4).is_err(), "as_str");
    assert!(ptr.as_c_str().is_err(), "as_c_str");
    assert!(
        UserInPtr::<UserInPtr<u8>>::from(at)
            .read_cstring_array()
            .is_err(),
        "read_cstring_array"
    );
    // Writing it is still fine.
    UserOutPtr::<u8>::from(at).write(b'y').unwrap();
    allow_user_access(at, PAGE_SIZE, MMUFlags::READ);
    assert_eq!(ptr.read(), Ok(b'y'));
}

/// An in-out word is read and then written, and each asks its own access.
#[test]
fn an_in_out_word_is_written_only_if_it_may_be_written() {
    install();
    let at = pages(1, 0);
    refuse_user_access(at, PAGE_SIZE, MMUFlags::WRITE);
    let mut word = UserInOutPtr::<u32>::from(at);
    assert_eq!(word.read(), Ok(0));
    assert_eq!(word.write(1), Err(Error::InvalidPointer));
    allow_user_access(at, PAGE_SIZE, MMUFlags::WRITE);
}

/// A write is refused when any page of it may not be written, and a page
/// it does not reach does not matter.
#[test]
fn a_write_running_into_a_page_that_may_not_be_written_is_refused_whole() {
    install();
    let at = pages(2, 0);
    refuse_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::WRITE);
    let mut tail = UserOutPtr::<u8>::from(at + PAGE_SIZE - 2);
    assert_eq!(tail.write_array(&[9; 4]), Err(Error::InvalidPointer));
    assert_eq!(
        bytes_at(at + PAGE_SIZE - 2, 2),
        &[0, 0],
        "the first page was written"
    );
    tail.write_array(&[9; 2]).unwrap();
    allow_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::WRITE);
}

/// A C string has no length to check up front. The scan checks each page
/// as it reaches it: checking the first byte alone, it read on past the end
/// of the mapping into whatever came next.
#[test]
fn a_c_string_that_runs_into_a_page_it_may_not_read_is_refused() {
    install();
    let at = pages(2, b'a');
    refuse_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::READ);
    let name = UserInPtr::<u8>::from(at);
    assert_eq!(
        name.as_c_str(),
        Err(Error::InvalidPointer),
        "the scan read the page it may not"
    );
    // Not even for its NUL alone. The scan reads the NUL before it knows the
    // length, and the length it checks afterwards stops short of the NUL, so
    // only the check at the page boundary stands between the scan and the
    // fault.
    unsafe { *((at + PAGE_SIZE) as *mut u8) = 0 };
    assert_eq!(
        name.as_c_str(),
        Err(Error::InvalidPointer),
        "the scan read the NUL on the page it may not"
    );
    allow_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::READ);
    // Allowed, the same string is read whole, across the boundary.
    assert_eq!(name.as_c_str().map(str::len), Ok(PAGE_SIZE));
}

/// Nor does a string that ends before the page it may not read need it.
#[test]
fn a_c_string_that_ends_before_that_page_is_read() {
    install();
    let at = pages(2, b'a');
    refuse_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::READ);
    unsafe { *((at + PAGE_SIZE - 1) as *mut u8) = 0 };
    assert_eq!(
        UserInPtr::<u8>::from(at).as_c_str().map(str::len),
        Ok(PAGE_SIZE - 1)
    );
    allow_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::READ);
}

/// An `argv` with no `NULL` before a page it may not read: each page of the
/// array is checked as the walk reaches it.
#[test]
fn a_string_array_that_runs_into_a_page_it_may_not_read_is_refused() {
    install();
    let name = pages(1, 0);
    unsafe { *(name as *mut u8) = b'n' };
    unsafe { *((name + 1) as *mut u8) = 0 };
    let at = pages(2, 0);
    let entries = PAGE_SIZE / core::mem::size_of::<usize>();
    let array = unsafe { core::slice::from_raw_parts_mut(at as *mut usize, 2 * entries) };
    array[..entries].fill(name);
    refuse_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::READ);
    let argv = UserInPtr::<UserInPtr<u8>>::from(at);
    assert!(
        argv.read_cstring_array().is_err(),
        "the walk read the page it may not"
    );
    // The NULL on the last entry of the first page ends it before that page.
    array[entries - 1] = 0;
    assert_eq!(argv.read_cstring_array().map(|v| v.len()), Ok(entries - 1));
    allow_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::READ);
}

/// The NUL `write_cstring` puts after the string is part of what it writes:
/// a string that fits before a page it may not write, but whose NUL would
/// land on it, is refused, and nothing of it is stored.
#[test]
fn a_c_string_whose_nul_would_land_on_a_page_it_may_not_write_is_refused() {
    install();
    let at = pages(2, 0x5a);
    refuse_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::WRITE);
    let mut name = UserOutPtr::<u8>::from(at + PAGE_SIZE - 2);
    assert_eq!(name.write_cstring("ab"), Err(Error::InvalidPointer));
    assert_eq!(
        bytes_at(at + PAGE_SIZE - 2, 2),
        &[0x5a, 0x5a],
        "the string was stored without its NUL"
    );
    name.write_cstring("a").unwrap();
    assert_eq!(bytes_at(at + PAGE_SIZE - 2, 2), b"a\0");
    allow_user_access(at + PAGE_SIZE, PAGE_SIZE, MMUFlags::WRITE);
}
