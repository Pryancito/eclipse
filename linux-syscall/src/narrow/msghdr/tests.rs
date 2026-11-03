//! The two layouts, and the five places a wrong one shows up.
//!
//! Three kinds of test, because the defect has three kinds of consequence:
//!
//! 1. **The layouts themselves**, which is where the numbers are pinned ---
//!    including the native ones, taken from the struct rather than from
//!    arithmetic, so a field added to [`MsgHdr`] fails here instead of
//!    silently moving `recvmsg`'s answers into the wrong fields.
//! 2. **The read**, through a real `sendmsg` on a real socket. The same
//!    twenty-eight bytes are handed to a 32-bit process and to a native one,
//!    and the two answers differ: read narrow, the envelope names one
//!    `iovec` of three bytes and the call sends three; read natively, the
//!    halves pair into `msg_iov == NULL` and `msg_iovlen == 0`, and the call
//!    answers `EFAULT` for a list the caller never passed.
//!
//!    The `EFAULT` is the *luck* of this envelope, not the mechanism: these
//!    fields are zero, so the pointer the misread invents is null and
//!    something notices. An envelope whose `msg_control` and `msg_controllen`
//!    were anything else would invent a pointer that is merely **wrong**, and
//!    `sendmsg` would gather from it and report a count. That is why the
//!    narrow half of the pair asserts the byte count and not just "not an
//!    error".
//! 3. **The write-back**, which is the half a misread cannot show: `recvmsg`
//!    reports `msg_namelen`, `msg_controllen` and `msg_flags` through the
//!    caller's own structure, and at the native offsets `msg_flags` lands
//!    twenty bytes past the end of a 32-bit caller's. The test poisons the
//!    bytes past the structure and requires them untouched.
//!
//! And one round trip of a real descriptor through a real `SCM_RIGHTS`, at
//! the narrow layout, with **two** descriptors: one is the count where the
//! two alignments happen to agree (16 bytes either way), so a single-fd
//! round trip passes with the wrong `CMSG_ALIGN`.
//!
//! Fixed pids live in 48_42x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

// ---------------------------------------------------------------------------
// The layouts
// ---------------------------------------------------------------------------

/// The narrow envelope, written out field by field.
///
/// Not a loop over the fields: the point of the table is that each name sits
/// at one number, and a test that computed `4 * i` would pass for a layout
/// whose fields had been reordered.
#[test]
fn the_narrow_envelope_is_seven_four_byte_fields_and_no_padding() {
    let l = msghdr_layout(Abi::Linux32);
    assert_eq!(l.size, 28, "sizeof(struct msghdr) for a 32-bit caller");
    assert_eq!(l.name, 0);
    assert_eq!(l.namelen, 4);
    assert_eq!(l.iov, 8);
    assert_eq!(l.iovlen, 12);
    assert_eq!(l.control, 16);
    assert_eq!(l.controllen, 20);
    assert_eq!(l.flags, 24);
    assert_eq!(l.word, 4, "a 32-bit size_t");
    // And the size really is the end of the last field, which is what says
    // there is no padding anywhere in it.
    assert_eq!(l.flags + 4, l.size);
}

/// The native envelope, which has to come out of the struct.
///
/// `msg_flags` at 48 is the number that matters: it is where the write-back
/// of a 32-bit caller's answers used to go, twenty bytes past the end of its
/// twenty-eight.
#[test]
fn the_native_envelope_is_fifty_six_bytes_with_padding_in_two_places() {
    let l = msghdr_layout(Abi::Linux);
    assert_eq!(l.size, 56);
    assert_eq!(l.name, 0);
    assert_eq!(l.namelen, 8);
    assert_eq!(l.iov, 16, "4 bytes of padding after msg_namelen");
    assert_eq!(l.iovlen, 24);
    assert_eq!(l.control, 32);
    assert_eq!(l.controllen, 40);
    assert_eq!(l.flags, 48);
    assert_eq!(l.word, 8);
    // Twice the narrow one, which is the headline and is also a coincidence
    // worth pinning: it is true of this structure and not of `mmsghdr`.
    assert_eq!(l.size, 2 * msghdr_layout(Abi::Linux32).size);
}

/// A control header halves, and its `CMSG_ALIGN` halves with it.
///
/// Both at once: `CMSG_ALIGN` is `sizeof(long)` in Linux's macro, so a
/// 32-bit program pads to 4. Keeping one and changing the other is the
/// failure that looks like it works, because at one descriptor the two
/// agree.
#[test]
fn a_control_header_and_its_alignment_both_halve() {
    let n = cmsg_layout(Abi::Linux);
    assert_eq!(n.hdr_len, 16);
    assert_eq!(n.align, 8);
    assert_eq!(n.len_width, 8);

    let w = cmsg_layout(Abi::Linux32);
    assert_eq!(w.hdr_len, 12, "a 4-byte size_t and two 4-byte int");
    assert_eq!(w.align, 4, "CMSG_ALIGN is sizeof(long)");
    assert_eq!(w.len_width, 4);
}

/// Where the two alignments part, which is at **two** descriptors.
#[test]
fn one_descriptor_fits_both_alignments_and_two_do_not() {
    let n = cmsg_layout(Abi::Linux);
    let w = cmsg_layout(Abi::Linux32);

    // One: 16 + 4 padded to 8 is 24 native; 12 + 4 is 16 narrow. The
    // *message* sizes differ, but each is already aligned, which is why a
    // single round trip says nothing about the padding rule.
    assert_eq!(n.align_up(n.hdr_len + 4), 24);
    assert_eq!(w.align_up(w.hdr_len + 4), 16);

    // Two: 16 + 8 is 24 and already aligned; 12 + 8 is 20, and 20 is a
    // multiple of 4 and not of 8. A walk that kept the native rule would
    // step a narrow two-descriptor message to 24 and land four bytes inside
    // the message after it.
    assert_eq!(n.align_up(n.hdr_len + 8), 24);
    assert_eq!(w.align_up(w.hdr_len + 8), 20);
    assert_ne!(w.align_up(w.hdr_len + 8), n.align_up(w.hdr_len + 8));
}

/// What a narrow `cmsg_len` read at the native width comes back as.
///
/// Not a failure: `cmsg_level` becomes the top half of the length, so a
/// `SOL_SOCKET` message (level 1) reads as a length of 4 GiB and 20 bytes.
/// Documented as a test because it is the reason the walk takes a layout:
/// the number it gets wrong is the one that bounds every read after it.
#[test]
fn a_narrow_control_length_read_natively_is_four_gigabytes() {
    let w = cmsg_layout(Abi::Linux32);
    let n = cmsg_layout(Abi::Linux);

    // `{ cmsg_len = 20, cmsg_level = 1 (SOL_SOCKET), cmsg_type = 1
    // (SCM_RIGHTS) }` as a 32-bit program writes it, then two descriptors.
    let mut bytes = Vec::new();
    w.push_len(&mut bytes, 20);
    bytes.extend_from_slice(&1i32.to_ne_bytes());
    bytes.extend_from_slice(&1i32.to_ne_bytes());
    bytes.extend_from_slice(&7i32.to_ne_bytes());
    bytes.extend_from_slice(&9i32.to_ne_bytes());
    assert_eq!(bytes.len(), 20);

    assert_eq!(w.read_len(&bytes), Some(20), "at the width it was written");
    assert_eq!(
        n.read_len(&bytes),
        Some(20 + (1u64 << 32) as usize),
        "SOL_SOCKET became the top half of the length"
    );
}

/// A `cmsg_len` big enough to wrap the rounding, which is the case the
/// checked rounding exists for.
///
/// `cmsg_len` is entirely the caller's, so the walk reads whatever is in the
/// buffer. Rounded up unchecked, a length within `align - 1` of `usize::MAX`
/// wraps to a small number or to zero --- and a **step of zero** is a walk
/// that reads the same header for ever. The walk's own `> 0` catches the zero;
/// this is the half that catches the wrap before it becomes a plausible small
/// step.
#[test]
fn a_control_length_that_wraps_the_rounding_has_no_step() {
    for abi in [Abi::Linux, Abi::Linux32] {
        let l = cmsg_layout(abi);
        // The largest length that still rounds, and the smallest that does
        // not: the boundary, not a round number near it.
        let last = usize::MAX - (l.align - 1);
        assert_eq!(l.align_up_checked(last), Some(last), "{:?}", abi);
        assert_eq!(l.align_up_checked(last + 1), None, "{:?}", abi);
        assert_eq!(l.align_up_checked(usize::MAX), None, "{:?}", abi);
        // And the ordinary case is the same answer as the infallible one, so
        // the two are one rule and not two.
        for len in [0, 1, l.hdr_len, l.hdr_len + 4, l.hdr_len + 8, 1024] {
            assert_eq!(l.align_up_checked(len), Some(l.align_up(len)));
        }
    }
}

/// `cmsg_len` written and read back at each width, which is the pair that
/// says `push_len` and `read_len` agree about more than one number.
#[test]
fn a_control_length_survives_its_own_width() {
    for abi in [Abi::Linux, Abi::Linux32] {
        let l = cmsg_layout(abi);
        for len in [l.hdr_len, l.hdr_len + 4, l.hdr_len + 8, 1024] {
            let mut bytes = Vec::new();
            l.push_len(&mut bytes, len);
            bytes.resize(l.hdr_len, 0);
            assert_eq!(l.read_len(&bytes), Some(len), "{:?} len {}", abi, len);
        }
    }
}

// ---------------------------------------------------------------------------
// The read and the write-back, through a real socket
// ---------------------------------------------------------------------------

/// Where the process's own memory lives: an address a four-byte pointer can
/// hold, which this test's stack is not.
const USER_AT: usize = 0x2000_0000;
const USER_PAGES: usize = 4;
/// The page size the mapping below is measured in.
const PAGE_SIZE: usize = 0x1000;

const IOV_AT: usize = USER_AT;
const BUF_AT: usize = USER_AT + 0x100;
const HDR_AT: usize = USER_AT + 0x200;
const CTRL_AT: usize = USER_AT + 0x300;
const NAME_AT: usize = USER_AT + 0x400;

/// AF_UNIX, `SOCK_STREAM`, `SOCK_NONBLOCK`: no network stack, and a read
/// with nothing to read answers rather than waiting for ever.
const AF_UNIX: usize = 1;
const A_STREAM: usize = 1;
const NONBLOCK: usize = 0o4000;

/// The three bytes every one of these sends.
const MSG: &[u8] = b"abc";

/// What a null gather list answers.
const EFAULT: isize = -(LxError::EFAULT as isize);

/// A process of `abi` with four pages of its own at [`USER_AT`], mapped
/// eagerly so the kernel's writes land on pages that are really there.
fn a_process(pid: KoID, abi: Abi) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "msghdr32", lp).unwrap();
    proc.vmar()
        .map_at(
            USER_AT,
            VmObject::new_paged(USER_PAGES),
            0,
            USER_PAGES * PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .expect("pages of the process's own, at an address a u32 can hold");
    proc
}

/// Write bytes into the process's pages, the way the kernel writes to any
/// process.
fn put(at: usize, bytes: &[u8]) {
    let mut p: UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

/// Read them back.
fn get(at: usize, len: usize) -> Vec<u8> {
    let p: UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped above")
}

/// The twenty-eight bytes of a 32-bit `struct msghdr` naming one `iovec`.
///
/// Read at the native width these same bytes are a different envelope
/// entirely: `msg_name` takes `msg_namelen` as its top half (both zero, so
/// still null), `msg_iov` comes out of `msg_control` and `msg_controllen`
/// (both zero, so NULL) and `msg_iovlen` out of `msg_flags` and the four
/// bytes after the structure (zero). So the native read invents a gather list
/// that is null and empty, and `sendmsg` answers `EFAULT` for a list the
/// caller never passed --- which is this envelope's luck rather than the
/// mechanism's doing: with any other values in those fields the invented
/// pointer is wrong instead of null, and the call gathers from it and reports
/// a count.
fn a_narrow_msghdr(iov_at: usize, iovlen: u32, control_at: usize, controllen: u32) -> [u8; 28] {
    let mut hdr = [0u8; 28];
    let words = [
        0u32,
        0,
        iov_at as u32,
        iovlen,
        control_at as u32,
        controllen,
        0,
    ];
    for (i, w) in words.iter().enumerate() {
        hdr[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
    }
    hdr
}

/// A 32-bit `struct iovec`: a four-byte base and a four-byte length.
fn a_narrow_iovec(base: usize, len: usize) -> [u8; 8] {
    let mut iov = [0u8; 8];
    iov[..4].copy_from_slice(&(base as u32).to_ne_bytes());
    iov[4..].copy_from_slice(&(len as u32).to_ne_bytes());
    iov
}

/// A connected `AF_UNIX` pair, both ends nonblocking, by the native numbers
/// --- `socketpair`'s arguments are scalars and an `int[2]`, so it is the
/// same call in both ABIs and is not what any of this is testing.
async fn a_pair(sys: &Syscall<'_>) -> (i32, i32) {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let mut sv = [0i32; 2];
    let ret = own
        .syscall(
            Sys::SOCKETPAIR as u32,
            [
                AF_UNIX,
                A_STREAM | NONBLOCK,
                0,
                sv.as_mut_ptr() as usize,
                0,
                0,
            ],
        )
        .await;
    assert_eq!(ret, 0, "socketpair failed");
    (sv[0], sv[1])
}

/// `sendmsg` of the twenty-eight-byte envelope, by the native number.
async fn sendmsg_the_narrow_envelope(sys: &Syscall<'_>) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let (a, _b) = a_pair(sys).await;
    put(BUF_AT, MSG);
    put(IOV_AT, &a_narrow_iovec(BUF_AT, MSG.len()));
    put(HDR_AT, &a_narrow_msghdr(IOV_AT, 1, 0, 0));
    own.syscall(Sys::SENDMSG as u32, [a as usize, HDR_AT, 0, 0, 0, 0])
        .await
}

#[test]
fn a_32_bit_process_envelope_is_read_twenty_eight_bytes_at_a_time() {
    let proc = a_process(48_420, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { sendmsg_the_narrow_envelope(sys).await })
    });
    assert_eq!(
        got,
        MSG.len() as isize,
        "the envelope was read at the wrong width: read 56 bytes at a time \
         these same bytes name no iovec at all, and the call sends nothing"
    );
    proc.exit(0);
}

#[test]
fn the_same_envelope_read_natively_names_no_iovec() {
    // The other half of the pair, and the reason the test above measures
    // something: these are the SAME twenty-eight bytes. A kernel that read
    // every envelope at one width would answer both tests alike.
    let proc = a_process(48_421, Abi::Linux);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { sendmsg_the_narrow_envelope(sys).await })
    });
    assert_eq!(
        got, EFAULT,
        "read natively the halves pair into msg_iov = NULL and \
         msg_iovlen = 0, and a null gather list is EFAULT"
    );
    proc.exit(0);
}

#[test]
fn recvmsg_writes_its_answers_inside_a_32_bit_callers_structure() {
    // `msg_controllen` and `msg_flags` are reported through the caller's own
    // envelope. Natively they sit at 40 and 48, which for a 28-byte
    // structure is twelve and twenty bytes PAST its end. So the bytes past
    // it are poisoned and required untouched --- and the two fields inside
    // it are required written, so that a kernel which wrote nowhere at all
    // would fail too.
    let proc = a_process(48_422, Abi::Linux32);
    let (recvd, hdr, poison) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let (a, b) = a_pair(sys).await;

            // Three bytes into the pipe, by `sendto`, whose arguments are a
            // descriptor, a byte buffer and scalars in both ABIs.
            put(BUF_AT, MSG);
            let sent = own
                .syscall(Sys::SENDTO as u32, [a as usize, BUF_AT, MSG.len(), 0, 0, 0])
                .await;
            assert_eq!(sent, MSG.len() as isize, "sendto failed");

            // 0xaa over fifty-six bytes from the envelope's start --- as far
            // as the native `msg_flags` reaches and four bytes further --- and
            // then the twenty-eight-byte envelope over the front of it, so
            // what is left poisoned is exactly what lies past its end.
            put(HDR_AT, &[0xaau8; 56]);
            put(IOV_AT, &a_narrow_iovec(BUF_AT + 0x40, MSG.len()));
            put(HDR_AT, &a_narrow_msghdr(IOV_AT, 1, CTRL_AT, 64));
            let recvd = own
                .syscall(Sys::RECVMSG as u32, [b as usize, HDR_AT, 0, 0, 0, 0])
                .await;
            (recvd, get(HDR_AT, 28), get(HDR_AT + 28, 28))
        })
    });

    assert_eq!(recvd, MSG.len() as isize, "recvmsg failed");
    // `msg_controllen` at 20: four bytes, and zero because nothing was sent
    // with the message. Written, not left as it was: the caller put 64 there.
    assert_eq!(
        u32::from_ne_bytes(hdr[20..24].try_into().unwrap()),
        0,
        "msg_controllen was not reported at offset 20"
    );
    // `msg_flags` at 24.
    assert_eq!(
        i32::from_ne_bytes(hdr[24..28].try_into().unwrap()),
        0,
        "msg_flags was not reported at offset 24"
    );
    assert!(
        poison.iter().all(|b| *b == 0xaa),
        "recvmsg wrote past the end of a 32-bit caller's envelope: {:x?}",
        poison
    );
    proc.exit(0);
}

// ---------------------------------------------------------------------------
// A real descriptor through a real SCM_RIGHTS, at the narrow layout
// ---------------------------------------------------------------------------

/// `SOL_SOCKET` and `SCM_RIGHTS`, as they appear in a control header.
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;

/// A 32-bit `SCM_RIGHTS` control message carrying `fds`: a four-byte
/// `cmsg_len`, two four-byte `int`, then the descriptors.
fn a_narrow_scm_rights(fds: &[i32]) -> Vec<u8> {
    let mut buf = Vec::new();
    let len = 12 + 4 * fds.len();
    buf.extend_from_slice(&(len as u32).to_ne_bytes());
    buf.extend_from_slice(&SOL_SOCKET.to_ne_bytes());
    buf.extend_from_slice(&SCM_RIGHTS.to_ne_bytes());
    for fd in fds {
        buf.extend_from_slice(&fd.to_ne_bytes());
    }
    buf
}

#[test]
fn two_descriptors_ride_a_narrow_control_message_both_ways() {
    // The end-to-end one, and the only test here that exercises the walk and
    // the builder against each other rather than each against a number.
    //
    // TWO descriptors on purpose. At one the native and the narrow message
    // are both sixteen bytes, so a round trip of one passes with the header
    // length right and the alignment wrong, or with both wrong in the way
    // that cancels. At two they are 24 and 20.
    //
    // Read with the native layout the message that goes in has a `cmsg_len`
    // of four gigabytes (`SOL_SOCKET` as its top half), which is longer than
    // the control buffer, so the walk stops before it and NO descriptor is
    // passed: `recvmsg` then hands back a message with no control data, and
    // the descriptor count is what says so.
    let proc = a_process(48_423, Abi::Linux32);
    let (sent, recvd, ctrl, works) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let (a, b) = a_pair(sys).await;
            // The two descriptors to pass are the pair's own ends, which are
            // the two this process is sure to have.
            let passing = a_narrow_scm_rights(&[a, b]);
            assert_eq!(passing.len(), 20, "12 + two 4-byte descriptors");

            put(BUF_AT, MSG);
            put(IOV_AT, &a_narrow_iovec(BUF_AT, MSG.len()));
            put(CTRL_AT, &passing);
            put(
                HDR_AT,
                &a_narrow_msghdr(IOV_AT, 1, CTRL_AT, passing.len() as u32),
            );
            let sent = own
                .syscall(Sys::SENDMSG as u32, [a as usize, HDR_AT, 0, 0, 0, 0])
                .await;

            // And back, into a control buffer of its own.
            put(NAME_AT, &[0u8; 64]);
            put(BUF_AT + 0x40, &[0u8; 8]);
            put(IOV_AT, &a_narrow_iovec(BUF_AT + 0x40, MSG.len()));
            put(HDR_AT, &a_narrow_msghdr(IOV_AT, 1, NAME_AT, 64));
            let recvd = own
                .syscall(Sys::RECVMSG as u32, [b as usize, HDR_AT, 0, 0, 0, 0])
                .await;
            let written = u32::from_ne_bytes(get(HDR_AT + 20, 4).try_into().unwrap()) as usize;
            let ctrl = get(NAME_AT, written.min(64));
            // Whether the descriptors that arrived are usable, which is the
            // only question that matters in the end: `dup` of each.
            let mut works = Vec::new();
            if ctrl.len() >= 12 {
                for chunk in ctrl[12..].as_chunks::<4>().0 {
                    let fd = i32::from_ne_bytes(*chunk);
                    works.push(
                        own.syscall(Sys::DUP as u32, [fd as usize, 0, 0, 0, 0, 0])
                            .await,
                    );
                }
            }
            (sent, recvd, ctrl, works)
        })
    });

    assert_eq!(sent, MSG.len() as isize, "sendmsg with SCM_RIGHTS failed");
    assert_eq!(recvd, MSG.len() as isize, "recvmsg failed");
    assert_eq!(
        ctrl.len(),
        20,
        "the control message that came back is not a 12-byte header and two          descriptors: {:x?}",
        ctrl
    );
    assert_eq!(
        u32::from_ne_bytes(ctrl[..4].try_into().unwrap()),
        20,
        "cmsg_len was not written at the narrow width"
    );
    assert_eq!(
        i32::from_ne_bytes(ctrl[4..8].try_into().unwrap()),
        SOL_SOCKET
    );
    assert_eq!(
        i32::from_ne_bytes(ctrl[8..12].try_into().unwrap()),
        SCM_RIGHTS
    );
    assert_eq!(works.len(), 2, "two descriptors did not arrive");
    for (i, got) in works.iter().enumerate() {
        assert!(
            *got >= 0,
            "descriptor {} arrived but does not work: {}",
            i,
            got
        );
    }
    proc.exit(0);
}

#[test]
fn the_walk_steps_from_one_narrow_control_message_to_the_next() {
    // The alignment, end to end, which the test above only checks as
    // arithmetic. Two `SCM_RIGHTS` messages in one control buffer: the first
    // carries two descriptors and is twenty bytes, which is a multiple of
    // four and NOT of eight. So a walk that kept the native `CMSG_ALIGN`
    // steps twenty-four bytes from the start of the first and lands four
    // bytes inside the second, reads its `cmsg_level` as a length and gives
    // up --- and the second message's descriptor never arrives.
    //
    // Three descriptors go in and three have to come out. Two would mean the
    // first message was read and the second was lost, which is exactly the
    // failure a one-message round trip cannot see.
    let proc = a_process(48_424, Abi::Linux32);
    let (sent, recvd, ctrl) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let (a, b) = a_pair(sys).await;

            let mut passing = a_narrow_scm_rights(&[a, b]);
            assert_eq!(passing.len(), 20, "a multiple of 4 and not of 8");
            passing.extend(a_narrow_scm_rights(&[a]));
            assert_eq!(passing.len(), 36);

            put(BUF_AT, MSG);
            put(IOV_AT, &a_narrow_iovec(BUF_AT, MSG.len()));
            put(CTRL_AT, &passing);
            put(
                HDR_AT,
                &a_narrow_msghdr(IOV_AT, 1, CTRL_AT, passing.len() as u32),
            );
            let sent = own
                .syscall(Sys::SENDMSG as u32, [a as usize, HDR_AT, 0, 0, 0, 0])
                .await;

            put(NAME_AT, &[0u8; 64]);
            put(IOV_AT, &a_narrow_iovec(BUF_AT + 0x40, MSG.len()));
            put(HDR_AT, &a_narrow_msghdr(IOV_AT, 1, NAME_AT, 64));
            let recvd = own
                .syscall(Sys::RECVMSG as u32, [b as usize, HDR_AT, 0, 0, 0, 0])
                .await;
            let written = u32::from_ne_bytes(get(HDR_AT + 20, 4).try_into().unwrap()) as usize;
            (sent, recvd, get(NAME_AT, written.min(64)))
        })
    });

    assert_eq!(sent, MSG.len() as isize, "sendmsg failed");
    assert_eq!(recvd, MSG.len() as isize, "recvmsg failed");
    // One message back, carrying all three: `recvmsg` coalesces the
    // descriptors it hands over into a single `SCM_RIGHTS`, as Linux does.
    assert_eq!(
        ctrl.len(),
        12 + 3 * 4,
        "three descriptors went in; the control buffer that came back holds \
         {} of them: {:x?}",
        ctrl.len().saturating_sub(12) / 4,
        ctrl
    );
    assert_eq!(
        u32::from_ne_bytes(ctrl[..4].try_into().unwrap()),
        24,
        "cmsg_len of a 12-byte header and three descriptors"
    );
    proc.exit(0);
}

/// `socketcall`'s i386 number, and the two calls inside it that this batch
/// wires. A 32-bit program has no `sendmsg` syscall of its own: it reaches
/// both through 102, with the call number in the first register and the
/// arguments in a block of four-byte words.
const I386_SOCKETCALL: u32 = 102;
const SYS_SENDMSG: usize = 16;
const SYS_RECVMSG: usize = 17;

/// Where the three-word argument block of a `socketcall` goes.
const SARGS_AT: usize = USER_AT + 0x500;

#[test]
fn a_32_bit_program_reaches_sendmsg_and_recvmsg_through_socketcall() {
    // The door, which no test above opens: every one of them enters by the
    // native number, so with `socketcall`'s 16 and 17 left unwired they all
    // still pass and a 32-bit program's `sendmsg` answers `ENOSYS`. Which is
    // what it did until this batch.
    //
    // The block is three words --- `{ fd, msg, flags }` --- of four bytes,
    // which is the width `socketcall` reads whatever is in it.
    let proc = a_process(48_425, Abi::Linux32);
    let (sent, recvd, got) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let (a, b) = a_pair(sys).await;

            let block = |fd: i32| {
                let mut w = [0u8; 12];
                w[..4].copy_from_slice(&(fd as u32).to_ne_bytes());
                w[4..8].copy_from_slice(&(HDR_AT as u32).to_ne_bytes());
                w
            };

            put(BUF_AT, MSG);
            put(IOV_AT, &a_narrow_iovec(BUF_AT, MSG.len()));
            put(HDR_AT, &a_narrow_msghdr(IOV_AT, 1, 0, 0));
            put(SARGS_AT, &block(a));
            let sent = own
                .compat32_syscall(I386_SOCKETCALL, [SYS_SENDMSG, SARGS_AT, 0, 0, 0, 0])
                .await;

            put(BUF_AT + 0x40, b"...");
            put(IOV_AT, &a_narrow_iovec(BUF_AT + 0x40, MSG.len()));
            put(HDR_AT, &a_narrow_msghdr(IOV_AT, 1, 0, 0));
            put(SARGS_AT, &block(b));
            let recvd = own
                .compat32_syscall(I386_SOCKETCALL, [SYS_RECVMSG, SARGS_AT, 0, 0, 0, 0])
                .await;
            (sent, recvd, get(BUF_AT + 0x40, MSG.len()))
        })
    });

    assert_eq!(
        sent,
        MSG.len() as isize,
        "socketcall(16) did not reach `sendmsg`: a 32-bit program cannot \
         send a message at all"
    );
    assert_eq!(recvd, MSG.len() as isize, "socketcall(17) did not receive");
    assert_eq!(&got, MSG, "the bytes did not arrive at the other end");
    proc.exit(0);
}

// ---------------------------------------------------------------------------
// The batch array's stride
// ---------------------------------------------------------------------------

/// The stride and the count's offset, and the trap that they are **not** in
/// the same ratio.
///
/// A `struct mmsghdr` is an envelope plus an `unsigned int`. Natively that is
/// 56 + 4 rounded up to the structure's own 8-byte alignment, which is 64;
/// narrow it is 28 + 4, which is 32 and needs no rounding at all because an
/// i386 `msghdr` aligns to 4. So the strides happen to be in a ratio of two,
/// like the envelope --- and the offsets, 56 and 28, are too.
///
/// What is **not** derivable is the stride from the envelope: 2 * 28 is 56,
/// which is the narrow *offset*, not the narrow stride. A walk that reached
/// for the obvious arithmetic would step 56 through an array of 32-byte
/// entries and read every other entry's second half as an envelope.
#[test]
fn a_batch_entry_is_an_envelope_and_a_count_and_not_twice_an_envelope() {
    let n = mmsghdr_layout(Abi::Linux);
    assert_eq!(n.stride, 64);
    assert_eq!(n.len, 56, "the count sits right after the envelope");
    assert_eq!(n.len, msghdr_layout(Abi::Linux).size);

    let w = mmsghdr_layout(Abi::Linux32);
    assert_eq!(w.stride, 32);
    assert_eq!(w.len, 28);
    assert_eq!(w.len, msghdr_layout(Abi::Linux32).size);

    // The count is four bytes in both, so it fits inside the entry in both:
    // `len + 4 <= stride`. Natively there are four bytes of padding after
    // it; narrow there are none, and that is the whole difference.
    assert_eq!(n.len + 4 + 4, n.stride, "padding after the native count");
    assert_eq!(w.len + 4, w.stride, "no padding after the narrow one");

    // And the trap, written out: the narrow stride is NOT twice the narrow
    // envelope, which is the number an arithmetic shortcut would reach for.
    assert_ne!(w.stride, 2 * msghdr_layout(Abi::Linux32).size);
}

/// Where the second entry of a narrow array starts, and what the native
/// stride would read instead.
#[test]
fn the_native_stride_walks_past_the_second_narrow_entry() {
    let w = mmsghdr_layout(Abi::Linux32);
    let n = mmsghdr_layout(Abi::Linux);
    // Entry 1 of a narrow array starts at 32. With the native stride the
    // walk looks for it at 64, which is where entry **2** starts --- so a
    // batch of three has its middle message skipped and its count written
    // into the array's own tail.
    assert_eq!(w.stride, 32);
    assert_eq!(n.stride, 2 * w.stride);
    assert_eq!(2 * w.stride, n.stride, "entry 2, not entry 1");
}

/// The i386 numbers of the two batch calls, inside `socketcall`.
const SYS_SENDMMSG: usize = 20;
const SYS_RECVMMSG: usize = 19;

/// Where a two-entry narrow `mmsghdr` array goes.
const MMSG_AT: usize = USER_AT + 0x600;
/// And the two `iovec`, and the two buffers they name.
const MIOV_AT: usize = USER_AT + 0x700;
const MBUF_AT: usize = USER_AT + 0x800;

/// Two bytes to send, one per message, so the two counts written back differ
/// from each other and from the message length.
const TWO: [&[u8]; 2] = [b"a", b"bb"];

#[test]
fn a_32_bit_program_sends_and_receives_a_batch_through_socketcall() {
    // Two messages in one call, which is the smallest batch that uses the
    // stride at all: the first entry is at offset 0 whatever the stride is,
    // so a one-entry batch says nothing.
    //
    // With the native stride the second entry is looked for at 64, which in
    // a 32-bit array is past the end of the two entries written there --- so
    // the second message's envelope is read out of whatever follows and the
    // call either answers 1 instead of 2 or faults.
    let proc = a_process(48_426, Abi::Linux32);
    let (sent, recvd, counts, got) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let (a, b) = a_pair(sys).await;

            // Two entries of 32 bytes: a 28-byte envelope and a four-byte
            // count, with the count poisoned so that a kernel writing it
            // anywhere else is caught by what is left behind.
            let mut vec = [0xaau8; 64];
            for i in 0..2 {
                put(MBUF_AT + i * 0x40, TWO[i]);
                put(
                    MIOV_AT + i * 8,
                    &a_narrow_iovec(MBUF_AT + i * 0x40, TWO[i].len()),
                );
                let hdr = a_narrow_msghdr(MIOV_AT + i * 8, 1, 0, 0);
                vec[i * 32..i * 32 + 28].copy_from_slice(&hdr);
            }
            put(MMSG_AT, &vec);

            let mut block = [0u8; 16];
            block[..4].copy_from_slice(&(a as u32).to_ne_bytes());
            block[4..8].copy_from_slice(&(MMSG_AT as u32).to_ne_bytes());
            block[8..12].copy_from_slice(&2u32.to_ne_bytes());
            put(SARGS_AT, &block);
            let sent = own
                .compat32_syscall(I386_SOCKETCALL, [SYS_SENDMMSG, SARGS_AT, 0, 0, 0, 0])
                .await;
            // The two counts the kernel wrote, at offsets 28 and 60.
            let counts = [
                u32::from_ne_bytes(get(MMSG_AT + 28, 4).try_into().unwrap()),
                u32::from_ne_bytes(get(MMSG_AT + 32 + 28, 4).try_into().unwrap()),
            ];

            // And back, into two destinations of their own.
            let mut vec = [0xaau8; 64];
            for i in 0..2 {
                put(MBUF_AT + 0x100 + i * 0x40, &[0u8; 8]);
                put(
                    MIOV_AT + 0x40 + i * 8,
                    &a_narrow_iovec(MBUF_AT + 0x100 + i * 0x40, 8),
                );
                let hdr = a_narrow_msghdr(MIOV_AT + 0x40 + i * 8, 1, 0, 0);
                vec[i * 32..i * 32 + 28].copy_from_slice(&hdr);
            }
            put(MMSG_AT, &vec);
            block[..4].copy_from_slice(&(b as u32).to_ne_bytes());
            put(SARGS_AT, &block);
            let recvd = own
                .compat32_syscall(I386_SOCKETCALL, [SYS_RECVMMSG, SARGS_AT, 0, 0, 0, 0])
                .await;
            let got = [get(MBUF_AT + 0x100, 1), get(MBUF_AT + 0x100 + 0x40, 2)];
            (sent, recvd, counts, got)
        })
    });

    assert_eq!(
        sent, 2,
        "socketcall(20) did not walk two entries of a 32-byte stride"
    );
    assert_eq!(
        counts,
        [1, 2],
        "the per-message counts were not written at offset 28 of each entry"
    );
    // A stream socket coalesces, so the two messages arrive as one run of
    // bytes; what the batch receive has to show is that it walked two
    // entries and wrote two counts, which `recvd` says.
    assert!(
        recvd >= 1,
        "socketcall(19) did not receive the batch: {}",
        recvd
    );
    assert_eq!(&got[0], b"a", "the first message's byte did not arrive");
    proc.exit(0);
}

use core::mem::{align_of, offset_of};
#[test]
fn the_batch_entry_is_the_mmsghdr_of_the_uapi() {
    // `struct mmsghdr { struct msghdr msg_hdr; unsigned int msg_len; }`,
    // which the compiler pads to 8 because `msghdr` holds pointers. Both
    // batch calls walk the user's array by this size and write the count
    // at this offset, so these two numbers ARE where `sendmmsg(2)` and
    // `recvmmsg(2)` reach into user memory.
    assert_eq!(size_of::<MMsgHdr>(), 64);
    assert_eq!(align_of::<MMsgHdr>(), 8);
    assert_eq!(offset_of!(MMsgHdr, msg_hdr), 0);
    assert_eq!(offset_of!(MMsgHdr, msg_len), 56);
    assert_eq!(offset_of!(MMsgHdr, _pad), 60);
    // And the one `msghdr` inside it, whose size is what the entry is
    // built around: a field added to it moves `msg_len`, and the two
    // constants below are the only reason the walk follows.
    assert_eq!(size_of::<MsgHdr>(), 56);
    assert_eq!(NATIVE_MMSG.stride, 64);
    assert_eq!(NATIVE_MMSG.len, 56);
}

#[test]
fn the_stride_is_the_whole_entry_and_not_the_header_plus_a_guess() {
    // What this replaces, written out: `size_of::<MsgHdr>() + 8` for the
    // stride and `size_of::<MsgHdr>()` for the offset. The two agree with
    // the entry today, which is why nothing caught the arithmetic -- and
    // the day a `msghdr` field is added they stop agreeing, the walk
    // falls one stride short per entry, and each transfer count lands
    // inside the next entry, on top of its `msg_name` pointer.
    assert_eq!(NATIVE_MMSG.stride, size_of::<MsgHdr>() + 8);
    assert_eq!(NATIVE_MMSG.len, size_of::<MsgHdr>());
    // The count has to fit inside the entry it belongs to, which is the
    // property the arithmetic never stated.
    assert!(NATIVE_MMSG.len + size_of::<u32>() <= NATIVE_MMSG.stride);
}
