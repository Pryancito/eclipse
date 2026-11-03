//! `argv` and `envp` at the **caller's** pointer width, through the call.
//!
//! A 32-bit program's `argv` is an array of four-byte pointers ended by a
//! four-byte `NULL`. Reading it at the native width does not fail --- which
//! is what makes it the same kind of defect as the narrow `iovec` --- so
//! what a test has to show is not that the narrow read works but that the
//! two reads of the **same bytes** differ, and that the kernel does the one
//! the process asked for.
//!
//! These bytes are four words, `[arg0, 0, arg1, 0]`:
//!
//! ```text
//! narrow, four bytes an entry:  arg0, then the terminator  -> ONE argument
//! wide,  eight bytes an entry:  arg0 | 0<<32, arg1 | 0<<32 -> TWO arguments
//! ```
//!
//! Both reads succeed, and neither faults, which is the point: nothing in
//! the mechanism reports the difference. What reports it here is `E2BIG`.
//! Each argument is 70 000 bytes, so one of them fits under
//! `MAX_ARG_STRLEN` and under the floor `arg_limit` puts on `RLIMIT_STACK`
//! (131 072 bytes, Linux's `ARG_FLOOR`), and two do not. So the narrow read
//! gets as far as the image and answers `ENOEXEC` --- `/junk` is a file that
//! is not an ELF --- and the wide read of the same array answers `E2BIG`
//! before it ever opens anything.
//!
//! The bytes live at `0x2000_0000`, mapped into the process itself, because
//! a four-byte pointer has to name an address below 4 GiB and this test's
//! own memory does not. They are written through `UserOutPtr`, the way the
//! kernel writes to any process.
//!
//! What this cannot ask for is a **successful** exec: that replaces the
//! address space the test is running in. Step 61 of `tools/i386-hello` is
//! the other half, where a real 32-bit program really does exec, and the
//! new image is what reports it.
//!
//! Fixed pids live in 48_41x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use kernel_hal::user::UserOutPtr;
use linux_object::process::{Abi, RLimit, RLIMIT_STACK};
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;
use zircon_object::vm::{MMUFlags, VmObject};

const E2BIG: isize = -(LxError::E2BIG as isize);
const ENOEXEC: isize = -(LxError::ENOEXEC as isize);

/// Where the caller's own `argv` lives: an address a four-byte pointer can
/// hold, which this test's heap is not.
const USER_AT: usize = 0x2000_0000;
/// Forty pages, which is room for the two arguments and the array.
const USER_PAGES: usize = 40;

/// One argument's length without its NUL. Two of these pass
/// `MAX_ARG_STRLEN` one at a time and fail `arg_limit`'s floor together.
const ARG_LEN: usize = 70_000;

const ARG0_AT: usize = USER_AT;
const ARG1_AT: usize = USER_AT + 0x11_800;
const ARGV_AT: usize = USER_AT + 0x23_000;

/// A process of `abi` with `/junk` in it --- a file that is not a program,
/// so the exec is refused after the arguments have been read --- and forty
/// pages of its own memory at [`USER_AT`], mapped eagerly so the kernel's
/// writes below land on pages that are really there.
fn a_process(pid: KoID, abi: Abi) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    // `arg_limit` is a quarter of `RLIMIT_STACK` within Linux's own bounds,
    // and the default stack here is large enough that the quarter is
    // megabytes. Put it where the quarter lands exactly on the floor
    // (131 072 bytes, `ARG_FLOOR`), which is the lowest limit Linux allows
    // and the one the two arguments straddle.
    let stack = (4 * 131_072) as u64;
    lp.rlimit(
        RLIMIT_STACK,
        Some(RLimit {
            cur: stack,
            max: stack,
        }),
        true,
    )
    .unwrap();
    let junk = lp
        .root_inode()
        .create("junk", FileType::File, 0o755)
        .unwrap();
    junk.write_at(0, &[0x42u8; 64]).unwrap();
    let mut meta = junk.metadata().unwrap();
    meta.mode = 0o755;
    junk.set_metadata(&meta).unwrap();
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "argv32", lp).unwrap();
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

/// Write the two arguments and the four-word array into the process's pages.
fn write_the_array() -> LxResult<()> {
    let arg = alloc::vec![b'a'; ARG_LEN + 1];
    let mut arg0: UserOutPtr<u8> = ARG0_AT.into();
    arg0.write_array(&arg)?;
    let mut arg1: UserOutPtr<u8> = ARG1_AT.into();
    arg1.write_array(&arg)?;
    // `[arg0, 0, arg1, 0]`, four bytes a word. The pages are zero already,
    // so only the two addresses have to be written --- but they are written
    // in full, with their zero words, because a test that depended on the
    // page being zero would pass for the wrong reason.
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&(ARG0_AT as u32).to_le_bytes());
    bytes[8..12].copy_from_slice(&(ARG1_AT as u32).to_le_bytes());
    let mut argv: UserOutPtr<u8> = ARGV_AT.into();
    argv.write_array(&bytes)?;
    Ok(())
}

/// The i386 number of `execve`, spelled here as a 32-bit program spells it:
/// 11, where 11 natively means `fork`. See `crate::compat32`.
const I386_EXECVE: u32 = 11;

/// `execve("/junk", argv, NULL)` by number, with that array.
async fn execve_with_the_array(sys: &Syscall<'_>) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    write_the_array().expect("the pages are mapped above");
    let path = alloc::string::String::from("/junk\0");
    own.syscall(
        Sys::EXECVE as u32,
        [path.as_ptr() as usize, ARGV_AT, 0, 0, 0, 0],
    )
    .await
}

#[test]
fn a_32_bit_process_argv_is_read_four_bytes_at_a_time() {
    let proc = a_process(48_410, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { execve_with_the_array(sys).await })
    });
    assert_eq!(
        got, ENOEXEC,
        "a 32-bit process's argv was read at the wrong width: read eight \
         bytes at a time this array is two arguments, which is E2BIG"
    );
    proc.exit(0);
}

#[test]
fn the_same_array_read_at_the_native_width_is_two_arguments() {
    // The other half of the pair, and the reason the test above is not
    // measuring nothing: these are the SAME bytes, and a kernel that read
    // every `argv` at one width would answer both tests alike.
    let proc = a_process(48_411, Abi::Linux);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { execve_with_the_array(sys).await })
    });
    assert_eq!(
        got, E2BIG,
        "read at the native width the array is two 70 000-byte arguments, \
         which is past the floor `arg_limit` puts on RLIMIT_STACK"
    );
    proc.exit(0);
}

#[test]
fn execve_by_its_i386_number_reaches_the_call_and_reads_argv_narrow() {
    // And by the number a 32-bit program really sends, through the 32-bit
    // dispatcher. Without `execve` in an allowlist this answers `ENOSYS`
    // and a 32-bit program cannot start another program at all --- which is
    // what it did until this batch, and which no test above would notice,
    // because they enter by the native number.
    let proc = a_process(48_412, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            write_the_array().expect("the pages are mapped above");
            let path = alloc::string::String::from("/junk\0");
            own.compat32_syscall(I386_EXECVE, [path.as_ptr() as usize, ARGV_AT, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(
        got, ENOEXEC,
        "i386 11 did not reach `execve`: a 32-bit program cannot exec at all"
    );
    proc.exit(0);
}
