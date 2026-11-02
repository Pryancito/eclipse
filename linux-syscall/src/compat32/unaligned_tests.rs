//! The third shape of the i386 structure trap: the **same** layout, written
//! to a destination this kernel would otherwise refuse.
//!
//! `struct rlimit64` and `struct statx` are byte for byte the native ones in
//! both ABIs --- `statx` was designed that way on purpose --- so neither
//! needs converting. What changes is the *alignment* a caller's pointer is
//! allowed to have: on i386 a `long long` aligns to 4, so a 32-bit program's
//! `struct statx *` or `struct rlimit64 *` can sit at any multiple of four,
//! and Linux writes it. Here `UserPtr::write` asks for `align_of::<T>()`,
//! which for both of these is 8, and answers `EFAULT` for a pointer that is
//! 4 mod 8.
//!
//! That is a bug nothing else in this crate can see: the structure is right,
//! the fields are right, the conversion tables have nothing to say, and the
//! call simply fails for half the buffers a 32-bit program might hand it.
//! The i386 probe caught it in QEMU at step 50; these two tests catch it
//! here, by asking each call at a destination that is deliberately 4 mod 8
//! and reading what came back.
//!
//! Each test asks the SAME address twice: once from a 32-bit process, where
//! it must be written, and once from a native one, where the refusal must
//! stay. The second half is what keeps the fix from becoming "stop checking
//! alignment", which would drop a check Linux does make --- `futex` wants
//! its word 4-aligned and says `EINVAL` otherwise.
//!
//! Fixed pids live in 47_45x. 47_3xx was taken: `ipc/semctl_wiring_tests`
//! already holds 47_300 to 47_307, and two processes created with the same
//! fixed id both leave the shared `ROOT_JOB` when either one exits, so a
//! collision unregisters a process that is still running.

extern crate std;

use super::*;
use crate::file::Statx;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use core::mem::{align_of, offset_of, size_of};
use linux_object::process::{Abi, LinuxProcess, RLimit, RLIMIT_NOFILE};
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// The mask `statx` is asked for. Advisory; what comes back field by field
/// is `stat_conversion_tests`' business, not this file's.
const BASIC_STATS: u32 = 0x07ff;

/// A process with `/<top>/f` in it, running `abi`.
fn a_process(pid: KoID, top: &str, abi: Abi) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    top_dir.create("f", FileType::File, 0o644).unwrap();
    lp.set_abi(abi);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "unaligned", lp).unwrap()
}

/// A destination that is four bytes past an eight-byte boundary, which is
/// what an i386 program's pointer to either of these structures can be and
/// what `UserPtr::write` refuses.
///
/// The offset is searched for rather than assumed: a `[u64]` is 8-aligned
/// and a `[u32]` may or may not be, and a test that quietly got an 8-aligned
/// address would pass for the wrong reason --- which is exactly how the
/// probe nearly missed this, when its buffer grew and moved onto a boundary.
fn four_mod_eight(buf: &mut [u32]) -> usize {
    let at = buf.as_mut_ptr() as usize;
    let addr = if at % 8 == 4 { at } else { at + 4 };
    assert_eq!(addr % 8, 4, "no 4-mod-8 address in this buffer");
    assert_eq!(align_of::<Statx>(), 8, "the refusal is about this number");
    assert_eq!(align_of::<RLimit>(), 8);
    addr
}

/// `statx` into a 4-mod-8 buffer: written for a 32-bit caller, refused for a
/// native one.
///
/// The inode number is read back out of the image, so reverting the handler
/// to `statxbuf.write(x)` fails here with `EFAULT` and not merely with a
/// buffer left untouched.
#[test]
fn a_32_bit_statx_writes_a_four_aligned_buffer_that_the_native_one_refuses() {
    let path = String::from("/unal_x/f\0");
    let at = path.as_ptr() as usize;

    let answer = |pid: KoID, top: &'static str, abi: Abi, path_at: usize| {
        let proc = a_process(pid, top, abi);
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let mut buf = [0u32; 80];
                let dst = four_mod_eight(&mut buf);
                let got = sys.sys_statx(
                    FileDesc::from(0),
                    UserInPtr::from(path_at),
                    0,
                    BASIC_STATS,
                    UserOutPtr::from(dst),
                );
                // The image is read back through the user pointer the same
                // way the kernel wrote it, byte by byte: a `&Statx` made out
                // of a 4-mod-8 address would be an unaligned reference, and
                // this crate denies `unsafe_code` anyway.
                let ino = got.ok().map(|_| {
                    let ptr: kernel_hal::user::UserInPtr<u8> =
                        (dst + offset_of!(Statx, stx_ino)).into();
                    let b = ptr.read_array(8).unwrap();
                    u64::from_ne_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                });
                (got.map(|_| ()), ino)
            })
        })
    };

    // `/unal_x/f` has to exist for the 32-bit half to be able to answer
    // anything, and the path is the same for both so the only difference
    // between the two calls is the ABI.
    let (ok32, ino32) = answer(47_450, "unal_x", Abi::Linux32, at);
    assert_eq!(ok32, Ok(()), "a 32-bit statx was refused its own buffer");
    assert!(
        ino32.map(|i| i != 0).unwrap_or(false),
        "the image was not written: stx_ino is {:?}",
        ino32
    );

    let (ok64, _) = answer(47_451, "unal_x", Abi::Linux, at);
    assert_eq!(
        ok64,
        Err(LxError::EFAULT),
        "the native alignment check is gone, which is not the fix"
    );

    assert_eq!(size_of::<Statx>(), 256, "the same 256 bytes in both ABIs");
}

/// `prlimit64`'s old-limit pointer, the one the i386 probe failed on. Its
/// structure is `struct rlimit64` in both ABIs, so nothing about it is
/// narrow --- only where it may be put.
#[test]
fn a_32_bit_prlimit64_writes_a_four_aligned_buffer_that_the_native_one_refuses() {
    let answer = |pid: KoID, top: &'static str, abi: Abi| {
        let proc = a_process(pid, top, abi);
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let mut buf = [0u32; 8];
                let dst = four_mod_eight(&mut buf);
                let got = Syscall {
                    thread: sys.thread,
                    thread_fn: sys.thread_fn,
                    syscall_entry: sys.syscall_entry,
                }
                .sys_prlimit64(0, RLIMIT_NOFILE, 0.into(), dst.into());
                let cur = got.ok().map(|_| {
                    let ptr: kernel_hal::user::UserInPtr<u8> = dst.into();
                    let b = ptr.read_array(8).unwrap();
                    u64::from_ne_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                });
                (got.map(|_| ()), cur)
            })
        })
    };

    let (ok32, cur32) = answer(47_452, "unal_r", Abi::Linux32);
    assert_eq!(
        ok32,
        Ok(()),
        "a 32-bit prlimit64 was refused its own buffer"
    );
    assert!(
        cur32.map(|c| c != 0).unwrap_or(false),
        "the limit was not written: rlim_cur is {:?}",
        cur32
    );

    let (ok64, _) = answer(47_453, "unal_r", Abi::Linux);
    assert_eq!(
        ok64,
        Err(LxError::EFAULT),
        "the native alignment check is gone, which is not the fix"
    );

    assert_eq!(size_of::<RLimit>(), 16, "the same sixteen bytes in both");
}

/// `ppoll`'s signal mask, the fourth of this shape --- and the one that was
/// already reachable before the two above were fixed, because `ppoll` went
/// into `i386_abi_narrow.in` in the fifth batch for its `timespec`.
///
/// `Sigset` is a `u64`, so the typed read wants 8. An i386 `sigset_t` is
/// `unsigned long sig[2]`, which aligns to **4**. Measured on this
/// container's Linux: an i386 `ppoll(NULL, 0, {0,0}, mask, 8)` with its mask
/// at `0x0804a02c` --- 4 mod 8 --- answers 0.
///
/// This is a read and not a write, so the mutation it guards against is the
/// opposite one: the mask would be refused with `EFAULT` and `ppoll` would
/// fail for a program whose mask Linux accepts.
#[test]
fn a_32_bit_ppoll_reads_a_four_aligned_sigmask_that_the_native_one_refuses() {
    let answer = |pid: KoID, top: &'static str, abi: Abi| {
        let proc = a_process(pid, top, abi);
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let mut room = [0u32; 8];
                let at = four_mod_eight(&mut room);
                // A mask that blocks nothing, so what is being asked is only
                // whether the eight bytes could be read at all.
                let mut mask: kernel_hal::user::UserOutPtr<u8> = at.into();
                mask.write_array(&[0u8; 8]).unwrap();
                // The timeout is `{0, 0}` and goes in a buffer of its own,
                // 8-aligned: a `timespec` is sixteen bytes here and eight for
                // a 32-bit caller, and all of them are zero either way, so
                // the same bytes serve both widths. Keeping it off the
                // 4-mod-8 address matters --- otherwise the native half would
                // fail on the TIMEOUT's alignment and the test would pass
                // without ever reaching the mask.
                let mut when = [0u64; 2];
                let ts_at = when.as_mut_ptr() as usize;
                assert_eq!(ts_at % 8, 0);
                let mut own = Syscall {
                    thread: sys.thread,
                    thread_fn: sys.thread_fn,
                    syscall_entry: sys.syscall_entry,
                };
                own.sys_ppoll(0.into(), 0, ts_at.into(), at.into(), 8).await
            })
        })
    };

    assert_eq!(
        answer(47_454, "unal_p", Abi::Linux32),
        Ok(0),
        "a 32-bit ppoll was refused a mask Linux accepts"
    );
    assert_eq!(
        answer(47_455, "unal_p", Abi::Linux),
        Err(LxError::EFAULT),
        "the native alignment check is gone, which is not the fix"
    );
}
