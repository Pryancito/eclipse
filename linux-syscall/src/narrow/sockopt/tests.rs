//! The table, and the two directions it governs.
//!
//! Three kinds of test, because this defect's two halves fail differently:
//!
//! 1. **The table and the two conversions**, where the numbers are pinned.
//! 2. **The way in**, which fails loudly: a 32-bit `setsockopt(SO_RCVTIMEO)`
//!    with the eight bytes it owns is `EINVAL` from
//!    `check_setsockopt_len` --- which is right at the native width and is
//!    why the widening belongs **before** it, in this layer, so that
//!    `linux-object` keeps one width and one length table.
//! 3. **The way out**, which fails quietly, and this is the one worth
//!    reading. A sixteen-byte answer clamped to an eight-byte buffer puts the
//!    native `tv_sec` where the caller's whole structure goes: little-endian,
//!    so the **seconds survive** and `tv_usec` comes out of the seconds' high
//!    half, which is zero. So a round trip of a whole-second timeout passes
//!    with no conversion at all, and only a timeout with microseconds in it
//!    shows the difference. Every test here that cares uses 500 000 µs.
//!
//! Fixed pids live in 48_43x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// `SOL_SOCKET`, spelled the way the handlers spell it.
const SOL_SOCKET: usize = Level::SOL_SOCKET as usize;
const RCVTIMEO: usize = SolOptname::RCVTIMEO as usize;
const SNDTIMEO: usize = SolOptname::SNDTIMEO as usize;

#[test]
fn only_the_two_timeout_options_carry_a_structure() {
    assert_eq!(opt_shape(SOL_SOCKET, RCVTIMEO), OptShape::Timeval);
    assert_eq!(opt_shape(SOL_SOCKET, SNDTIMEO), OptShape::Timeval);

    // Named one by one rather than in a loop over a range, because a loop
    // over "everything else" is also what a table of all `SameWidth` passes.
    for opt in [
        SolOptname::REUSEADDR as usize,
        SolOptname::TYPE as usize,
        SolOptname::ERROR as usize,
        SolOptname::SNDBUF as usize,
        SolOptname::RCVBUF as usize,
        // `SO_LINGER` is the trap of this list: it IS a structure, and it is
        // the same bytes in both ABIs, because it is two `int`s and not two
        // `long`s. A table that said "a structure is halved" would convert
        // it and halve a caller's eight bytes to four.
        SolOptname::LINGER as usize,
        SolOptname::ACCEPTCONN as usize,
    ] {
        assert_eq!(
            opt_shape(SOL_SOCKET, opt),
            OptShape::SameWidth,
            "optname {} was converted; only 20 and 21 are a timeval",
            opt
        );
    }
}

#[test]
fn the_level_is_part_of_the_option_and_not_decoration() {
    // 20 and 21 under the TCP level are not timeouts at all, and converting
    // them would halve a value that is not a structure. Linux's
    // `do_tcp_setsockopt` is a separate switch for exactly this reason.
    const IPPROTO_TCP: usize = Level::IPPROTO_TCP as usize;
    const IPPROTO_IP: usize = Level::IPPROTO_IP as usize;
    assert_eq!(opt_shape(IPPROTO_TCP, RCVTIMEO), OptShape::SameWidth);
    assert_eq!(opt_shape(IPPROTO_IP, SNDTIMEO), OptShape::SameWidth);
}

#[test]
fn a_32_bit_timeval_is_eight_bytes_and_a_native_one_sixteen() {
    assert_eq!(NARROW_TIMEVAL_LEN, 8);
    assert_eq!(TimeVal::WORDS, 2);
    assert_eq!(
        TimeVal::WORDS * size_of::<usize>(),
        size_of::<TimeVal>(),
        "the native image is the structure itself, with no padding"
    );
}

/// Five seconds and half a second, as a 32-bit caller writes it.
fn narrow_tv(sec: i32, usec: i32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&sec.to_ne_bytes());
    b[4..].copy_from_slice(&usec.to_ne_bytes());
    b
}

/// The same, as the families store it.
fn native_tv(sec: i64, usec: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&sec.to_ne_bytes());
    b[8..].copy_from_slice(&usec.to_ne_bytes());
    b
}

#[test]
fn widening_a_timeout_puts_each_field_in_its_own_word() {
    let tv: TimeVal = from_narrow_bytes(&narrow_tv(5, 500_000));
    let (bytes, len) = to_native_bytes(tv);
    assert_eq!(len, 16);
    assert_eq!(
        &bytes[..16],
        &native_tv(5, 500_000),
        "read at the native width the caller's eight bytes are ONE field"
    );
}

#[test]
fn widening_a_negative_timeout_sign_extends_it() {
    // `old_timeval32`'s fields are `__kernel_long_t`, signed. Zero-extending
    // turns a `-1` into four billion, which passes every range check there
    // is. The rule is the parent module's; this is it holding for the one
    // structure that reaches it through a byte buffer.
    let tv: TimeVal = from_narrow_bytes(&narrow_tv(-1, -1));
    let (bytes, _) = to_native_bytes(tv);
    assert_eq!(&bytes[..16], &native_tv(-1, -1));
}

#[test]
fn narrowing_a_timeout_keeps_both_fields() {
    let out = OptOut {
        shape: OptShape::Timeval,
        narrow: true,
    };
    assert_eq!(
        out.apply(&native_tv(5, 500_000)).as_ref(),
        &narrow_tv(5, 500_000),
        "the microseconds are the field a clamp loses"
    );
}

#[test]
fn a_clamped_native_timeout_loses_its_microseconds() {
    // What the kernel did before this batch, written out so the test above
    // is not measuring nothing: with no conversion, a 32-bit caller's
    // eight-byte buffer takes the first eight bytes of a native `timeval`,
    // which is `tv_sec` alone. Little-endian, so the seconds come out right
    // and `tv_usec` comes out of the seconds' high half.
    let native = native_tv(5, 500_000);
    assert_eq!(
        &native[..8],
        &narrow_tv(5, 0),
        "a whole-second timeout round-trips correctly with NO conversion, \
         which is why every test that matters here sets microseconds"
    );
}

#[test]
fn a_native_caller_gets_the_structure_untouched() {
    let out = OptOut {
        shape: OptShape::Timeval,
        narrow: false,
    };
    let native = native_tv(5, 500_000);
    assert_eq!(out.apply(&native).as_ref(), &native);
    // And an option of the same width is untouched whoever asks.
    for narrow in [true, false] {
        let out = OptOut {
            shape: OptShape::SameWidth,
            narrow,
        };
        assert_eq!(out.apply(&native).as_ref(), &native);
    }
}

// ---------------------------------------------------------------------------
// Both directions, through a real socket
// ---------------------------------------------------------------------------

const USER_AT: usize = 0x2000_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// The option value, and the `socklen_t` beside it.
const VAL_AT: usize = USER_AT;
const LEN_AT: usize = USER_AT + 0x100;
/// The argument block of a `socketcall`.
const SARGS_AT: usize = USER_AT + 0x200;

/// `AF_INET`, `SOCK_DGRAM`: a UDP socket, which is one of the two families
/// that really store a timeout, so a round trip reads back what it set
/// rather than the trait default of zero.
const AF_INET: usize = 2;
const A_DGRAM: usize = 2;
/// `AF_UNIX`, `SOCK_STREAM`: the family that does not store one, used for the
/// refusal, where what matters is the length check and not the storage.
const AF_UNIX: usize = 1;
const A_STREAM: usize = 1;

const EINVAL: isize = -(LxError::EINVAL as isize);

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "sockopt32", lp).unwrap();
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

fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

fn get(at: usize, len: usize) -> Vec<u8> {
    let p: UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped above")
}

fn put_len(n: u32) {
    put(LEN_AT, &n.to_ne_bytes());
}

fn got_len() -> u32 {
    let b = get(LEN_AT, 4);
    u32::from_ne_bytes([b[0], b[1], b[2], b[3]])
}

/// A fresh `Syscall` on the same thread, the way every test in this family
/// borrows one.
fn own<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

async fn a_socket(sys: &Syscall<'_>, family: usize, ty: usize) -> usize {
    let fd = own(sys)
        .syscall(crate::Sys::SOCKET as u32, [family, ty, 0, 0, 0, 0])
        .await;
    assert!(fd >= 0, "socket({}, {}) failed: {}", family, ty, fd);
    fd as usize
}

/// `setsockopt(fd, SOL_SOCKET, optname, VAL_AT, optlen)` by the native
/// number: the number is not what any of this is about.
async fn set_timeout(sys: &Syscall<'_>, fd: usize, value: &[u8]) -> isize {
    put(VAL_AT, value);
    own(sys)
        .syscall(
            crate::Sys::SETSOCKOPT as u32,
            [fd, SOL_SOCKET, RCVTIMEO, VAL_AT, value.len(), 0],
        )
        .await
}

/// `getsockopt` of the same option into a buffer of `room` bytes, poisoned
/// first so that a write past the caller's length is visible.
async fn get_timeout(sys: &Syscall<'_>, fd: usize, room: u32) -> (isize, Vec<u8>) {
    put(VAL_AT, &[0xAAu8; 32]);
    put_len(room);
    let ret = own(sys)
        .syscall(
            crate::Sys::GETSOCKOPT as u32,
            [fd, SOL_SOCKET, RCVTIMEO, VAL_AT, LEN_AT, 0],
        )
        .await;
    (ret, get(VAL_AT, 32))
}

#[test]
fn a_32_bit_process_sets_a_timeout_with_the_eight_bytes_it_owns() {
    let proc = a_process(48_430, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_UNIX, A_STREAM).await;
            set_timeout(sys, fd, &narrow_tv(5, 500_000)).await
        })
    });
    assert_eq!(
        got, 0,
        "a 32-bit SO_RCVTIMEO was refused: its eight bytes were read as half \
         of a native timeval, and `check_setsockopt_len` wants sixteen"
    );
    proc.exit(0);
}

#[test]
fn the_same_eight_bytes_from_a_native_process_are_refused() {
    // The other half of the pair, and the reason the test above is not
    // measuring nothing: the SAME eight bytes, and a kernel that widened
    // every caller's would answer both alike. Eight bytes really is half a
    // native `struct timeval`, and `EINVAL` is the right answer to it.
    let proc = a_process(48_431, Abi::Linux);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_UNIX, A_STREAM).await;
            set_timeout(sys, fd, &narrow_tv(5, 500_000)).await
        })
    });
    assert_eq!(got, EINVAL);
    proc.exit(0);
}

#[test]
fn a_32_bit_caller_that_passes_four_bytes_is_still_refused() {
    // The widening is not a licence to read bytes the caller does not own:
    // `sock_set_timeout` refuses a buffer shorter than the structure at
    // whichever width that is, so four bytes is `EINVAL` and not a read of
    // eight.
    let proc = a_process(48_432, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_UNIX, A_STREAM).await;
            set_timeout(sys, fd, &5i32.to_ne_bytes()).await
        })
    });
    assert_eq!(got, EINVAL);
    proc.exit(0);
}

#[test]
fn a_32_bit_caller_reads_back_the_microseconds_it_set() {
    // The round trip, on a family that really stores the value. The
    // microseconds are the whole point: with no conversion out, the eight
    // bytes the caller gets are a native `tv_sec` and `tv_usec` comes back
    // zero.
    let proc = a_process(48_433, Abi::Linux32);
    let (set, ret, buf, len) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_INET, A_DGRAM).await;
            let set = set_timeout(sys, fd, &narrow_tv(5, 500_000)).await;
            let (ret, buf) = get_timeout(sys, fd, 8).await;
            (set, ret, buf, got_len())
        })
    });
    assert_eq!(set, 0, "the set half failed, so the read says nothing");
    assert_eq!(ret, 0);
    assert_eq!(len, 8, "a 32-bit caller was told a length it cannot hold");
    assert_eq!(
        &buf[..8],
        &narrow_tv(5, 500_000),
        "the microseconds were lost: this is a native timeval clamped to eight \
         bytes rather than a narrow one"
    );
    assert_eq!(
        &buf[8..],
        &[0xAAu8; 24],
        "wrote past the eight bytes the caller asked for"
    );
    proc.exit(0);
}

#[test]
fn a_native_process_round_trips_sixteen_bytes_as_it_always_did() {
    let proc = a_process(48_434, Abi::Linux);
    let (set, ret, buf, len) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_INET, A_DGRAM).await;
            let set = set_timeout(sys, fd, &native_tv(5, 500_000)).await;
            let (ret, buf) = get_timeout(sys, fd, 16).await;
            (set, ret, buf, got_len())
        })
    });
    assert_eq!(set, 0);
    assert_eq!(ret, 0);
    assert_eq!(len, 16);
    assert_eq!(&buf[..16], &native_tv(5, 500_000));
    proc.exit(0);
}

/// The i386 number of `socketcall`, and the two calls inside it.
const I386_SOCKETCALL: u32 = 102;
const CALL_SETSOCKOPT: usize = 14;
const CALL_GETSOCKOPT: usize = 15;

/// Five words of a `socketcall` block, four bytes each.
fn put_sargs(words: &[u32]) {
    let mut bytes = Vec::new();
    for w in words {
        bytes.extend_from_slice(&w.to_ne_bytes());
    }
    put(SARGS_AT, &bytes);
}

#[test]
fn a_32_bit_program_reaches_both_through_socketcall() {
    // The mutation nobody else kills: every test above enters by the native
    // number, so with 14 and 15 out of the `socketcall` table they all pass
    // and a 32-bit program still gets `ENOSYS` for the only door it has.
    // The same lesson as batches 11, 12 and 13.
    let proc = a_process(48_435, Abi::Linux32);
    let (set, get_ret, buf, len) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_INET, A_DGRAM).await;
            put(VAL_AT, &narrow_tv(5, 500_000));
            put_sargs(&[
                fd as u32,
                SOL_SOCKET as u32,
                RCVTIMEO as u32,
                VAL_AT as u32,
                NARROW_TIMEVAL_LEN as u32,
            ]);
            let set = own(sys)
                .compat32_syscall(I386_SOCKETCALL, [CALL_SETSOCKOPT, SARGS_AT, 0, 0, 0, 0])
                .await;

            put(VAL_AT, &[0xAAu8; 32]);
            put_len(8);
            put_sargs(&[
                fd as u32,
                SOL_SOCKET as u32,
                RCVTIMEO as u32,
                VAL_AT as u32,
                LEN_AT as u32,
            ]);
            let get_ret = own(sys)
                .compat32_syscall(I386_SOCKETCALL, [CALL_GETSOCKOPT, SARGS_AT, 0, 0, 0, 0])
                .await;
            (set, get_ret, get(VAL_AT, 8), got_len())
        })
    });
    assert_eq!(set, 0, "i386 socketcall 14 did not reach `setsockopt`");
    assert_eq!(get_ret, 0, "i386 socketcall 15 did not reach `getsockopt`");
    assert_eq!(len, 8);
    assert_eq!(&buf[..], &narrow_tv(5, 500_000));
    proc.exit(0);
}

#[test]
fn a_negative_optlen_through_socketcall_is_refused() {
    // Copilot's finding on this batch, and it is right. `optlen` is an
    // `int`, and a 32-bit caller's `socketcall` block carries it as a word:
    // `0xffffffff` is -1 to the program and arrives here zero-extended as
    // four billion. Four billion is more than eight, so the widening's lower
    // bound was satisfied, it read the eight bytes it needed, ignored the
    // rest of `optlen` and the call SUCCEEDED --- changing a timeout on a
    // length `__sys_setsockopt` refuses outright.
    //
    // Through `socketcall`, because that is the only door that can carry the
    // value: a native caller passes `optlen` in a register, where -1 is
    // `usize::MAX` and not `0xffffffff`.
    let proc = a_process(48_436, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_INET, A_DGRAM).await;
            put(VAL_AT, &narrow_tv(5, 500_000));
            put_sargs(&[
                fd as u32,
                SOL_SOCKET as u32,
                RCVTIMEO as u32,
                VAL_AT as u32,
                0xffff_ffff,
            ]);
            own(sys)
                .compat32_syscall(I386_SOCKETCALL, [CALL_SETSOCKOPT, SARGS_AT, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(
        got, EINVAL,
        "a negative optlen set a timeout: the length the caller passed was \
         never looked at past the eight bytes the conversion needed"
    );
    proc.exit(0);
}

#[test]
fn the_largest_optlen_an_int_can_hold_is_not_refused() {
    // The other side of the bound, so that the check is a check on the sign
    // and not a ceiling pulled out of the air: `i32::MAX` is a positive
    // length, and `__sys_setsockopt` takes it. It fails further in, on the
    // buffer, which is the native path's business and not this one's --- what
    // matters here is that it is not EINVAL from the length.
    let proc = a_process(48_437, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = a_socket(sys, AF_INET, A_DGRAM).await;
            put(VAL_AT, &narrow_tv(5, 500_000));
            put_sargs(&[
                fd as u32,
                SOL_SOCKET as u32,
                RCVTIMEO as u32,
                VAL_AT as u32,
                i32::MAX as u32,
            ]);
            own(sys)
                .compat32_syscall(I386_SOCKETCALL, [CALL_SETSOCKOPT, SARGS_AT, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(
        got, 0,
        "i32::MAX is a positive length, and the eight bytes it covers are there"
    );
    proc.exit(0);
}
