//! What `execve(2)` and `execveat(2)` answer BEFORE the point of no return,
//! called by their own numbers, and the proof that a caller refused there
//! still has every thread it came with.
//!
//! `exec_at` -- the whole of both calls -- was 127 of 127 lines at zero: it
//! had never run. Its halves were covered one by one (`check_exec_args`,
//! `exec_target`, `execve_preflight`, `de_thread`, `kill_siblings`, each
//! with its own module), which is the usual shape: a gate that is proved on
//! its own proves nothing about the order it is called in, nor about which
//! errno reaches the caller.
//!
//! The order is the whole point here. Everything these tests ask for is a
//! refusal on the near side of `de_thread`, so the process survives the
//! call; a successful `execve` would replace this test's own address space,
//! which is why there is not one here.
//!
//! The `/proc/self/exe` rewrite is the reason this file exists. It is the
//! root-cause fix for the "intermittent kernel corruption" -- busybox
//! re-executes its applets through that path, and storing the literal string
//! as the new image's `execute_path` made the magic link self-referential and
//! sprayed the heap with return addresses. The fix had no test of any kind.
//!
//! Fixed pids live in 48_4xx.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::ProcessExt;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{ThreadState, ROOT_JOB};

const E2BIG: isize = -(LxError::E2BIG as isize);
const EACCES: isize = -(LxError::EACCES as isize);
const EFAULT: isize = -(LxError::EFAULT as isize);
const ENOENT: isize = -(LxError::ENOENT as isize);
const ELOOP: isize = -(LxError::ELOOP as isize);
const ENOEXEC: isize = -(LxError::ENOEXEC as isize);

/// `<linux/fcntl.h>`, written out rather than read back from this kernel's
/// own `AtFlags`: a bit that drifted would still match itself.
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
/// See [`AT_SYMLINK_NOFOLLOW`].
const AT_EMPTY_PATH: usize = 0x1000;
/// `AT_FDCWD` as the caller sends it.
const AT_FDCWD: usize = -100isize as usize;

/// `MAX_ARG_STRLEN`: 32 pages, its NUL included, so a string of exactly this
/// many bytes is one too many.
const MAX_ARG_STRLEN: usize = 32 * 4096;

/// `e_machine` of this host, which is the machine the preflight asks for.
#[cfg(target_arch = "x86_64")]
const EM_HOST: u16 = 62;
/// See [`EM_HOST`].
#[cfg(target_arch = "aarch64")]
const EM_HOST: u16 = 183;
/// See [`EM_HOST`].
#[cfg(target_arch = "riscv64")]
const EM_HOST: u16 = 243;

/// A smallest ELF64 for this machine, with one `PT_LOAD` over the whole
/// file: what the preflight lets through.
fn an_elf() -> Vec<u8> {
    let mut v = alloc::vec![0u8; 64 + 56];
    v[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    v[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    v[18..20].copy_from_slice(&EM_HOST.to_le_bytes());
    v[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    v[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    v[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    v[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
    let len = v.len() as u64;
    v[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    v[96..104].copy_from_slice(&len.to_le_bytes()); // p_filesz
    v
}

/// A process with `threads` threads besides the one `on_a_thread` adds, and
/// in its root:
/// - `dir`, a directory;
/// - `prog`, an ELF anybody may run;
/// - `unreadable`, an ELF with no execute bit;
/// - `junk`, a file that is no program at all;
/// - `script`, a `#!` naming an interpreter that is not installed.
fn a_process(pid: KoID, threads: usize) -> Arc<Process> {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "execs",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let root = proc.linux().root_inode().clone();
    root.create("dir", FileType::Dir, 0o755).unwrap();
    for (name, mode, bytes) in [
        ("prog", 0o755, an_elf()),
        ("unreadable", 0o644, an_elf()),
        ("junk", 0o755, alloc::vec![0x42u8; 64]),
        ("script", 0o755, b"#!/nonexistent/interpreter\n".to_vec()),
    ] {
        let file = root.create(name, FileType::File, mode).unwrap();
        file.write_at(0, &bytes).unwrap();
        let mut meta = file.metadata().unwrap();
        meta.mode = mode as u16;
        file.set_metadata(&meta).unwrap();
    }
    for _ in 0..threads {
        Thread::create_linux(&proc).unwrap();
    }
    proc
}

/// A NUL-terminated C string in this test's own memory, and the address a
/// user pointer would carry.
struct CStr(Vec<u8>);

impl CStr {
    fn new(s: &str) -> Self {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        CStr(v)
    }

    fn at(&self) -> usize {
        self.0.as_ptr() as usize
    }
}

/// An `argv` of one argument: the array of pointers, NULL-terminated, as
/// `read_cstring_array` walks it.
struct Argv {
    _arg: CStr,
    ptrs: Vec<usize>,
}

impl Argv {
    fn of(arg: &str) -> Self {
        let arg = CStr::new(arg);
        let ptrs = alloc::vec![arg.at(), 0];
        Argv { _arg: arg, ptrs }
    }

    fn at(&self) -> usize {
        self.ptrs.as_ptr() as usize
    }
}

async fn by_number(sys: &Syscall<'_>, num: Sys, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num as u32, args).await
}

async fn execve(sys: &Syscall<'_>, path: usize, argv: usize, envp: usize) -> isize {
    by_number(sys, Sys::EXECVE, [path, argv, envp, 0, 0, 0]).await
}

async fn execveat(
    sys: &Syscall<'_>,
    dirfd: usize,
    path: usize,
    argv: usize,
    flags: usize,
) -> isize {
    by_number(sys, Sys::EXECVEAT, [dirfd, path, argv, 0, flags, 0]).await
}

/// Whether every thread of `proc` is still alive, which is what a refusal on
/// this side of the line promises.
fn all_alive(proc: &Arc<Process>) -> bool {
    proc.threads()
        .iter()
        .all(|t| t.state() != ThreadState::Dying)
}

/// An `execve` that is refused answers its errno and leaves the thread group
/// whole: the kill of the siblings used to be the FIRST thing the function
/// did, above every check, so `execve("/does/not/exist")` from a threaded
/// program answered `ENOENT` to a program that no longer had its threads.
///
/// `exec_order_tests` holds this for the preflight on its own. Only the
/// syscall can say it of the checks that come BEFORE the preflight, which is
/// where every errno below comes from.
#[test]
fn an_execve_refused_before_the_line_leaves_every_thread_alive() {
    let proc = a_process(48_400, 3);
    let before = proc.thread_count();
    let (got, alive_inside, count_inside) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = CStr::new("/does/not/exist");
            let argv = Argv::of("prog");
            let got = execve(sys, path.at(), argv.at(), 0).await;
            (
                got,
                all_alive(sys.zircon_process()),
                sys.zircon_process().thread_count(),
            )
        })
    });
    assert_eq!(got, ENOENT, "there is no such program");
    assert!(alive_inside, "and every thread is still alive");
    assert_eq!(
        count_inside,
        before + 1,
        "the three siblings and the one this call runs on"
    );
    proc.exit(0);
}

/// The three ways an image is not a program, each with the errno Linux gives
/// it: a directory and a file without the execute bit are `EACCES`, a file
/// that is not an ELF at all is `ENOEXEC`, and a `#!` line naming an
/// interpreter that is not installed is `ENOENT` -- the interpreter's own
/// errno, not the script's.
#[test]
fn what_is_not_a_program_is_refused_with_its_own_errno() {
    let proc = a_process(48_401, 1);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let argv = Argv::of("prog");
            let mut got = Vec::new();
            for name in ["/dir", "/unreadable", "/junk", "/script"] {
                let path = CStr::new(name);
                got.push(execve(sys, path.at(), argv.at(), 0).await);
            }
            got
        })
    });
    assert_eq!(
        got,
        [EACCES, EACCES, ENOEXEC, ENOENT],
        "a directory, a file with no execute bit, junk, and a missing interpreter"
    );
    assert!(all_alive(&proc), "none of them cost the caller a thread");
    proc.exit(0);
}

/// `execve("/proc/self/exe")` is rewritten to the image the process is
/// actually running BEFORE the path is resolved, so the errno it answers is
/// the rewritten path's and not the magic link's. With `AT_SYMLINK_NOFOLLOW`
/// it is not rewritten at all: the walk then reaches the magic link itself,
/// and a symlink is no program, so it is `ELOOP`. Two different errnos out of
/// the same path is what says the rewrite happened.
///
/// This is the root-cause fix for the "intermittent kernel corruption":
/// busybox re-executes its applets through `/proc/self/exe`, and storing
/// that literal string as the new image's `execute_path` made the next
/// lookup of it self-referential and recursed without bound over a stack
/// with no guard page. It had no test.
#[test]
fn proc_self_exe_is_rewritten_before_the_path_is_resolved() {
    let proc = a_process(48_402, 1);
    proc.linux().set_execute_path("/junk");
    let (rewritten, literal, when_there_is_none) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = CStr::new("/proc/self/exe");
            let argv = Argv::of("busybox");
            let rewritten = execve(sys, path.at(), argv.at(), 0).await;
            // Not rewritten: the walk gets to the magic link itself.
            let literal = execveat(sys, AT_FDCWD, path.at(), argv.at(), AT_SYMLINK_NOFOLLOW).await;
            // The same call once the process has no image to name.
            sys.linux_process().set_execute_path("");
            let when_there_is_none = execve(sys, path.at(), argv.at(), 0).await;
            (rewritten, literal, when_there_is_none)
        })
    });
    assert_eq!(
        rewritten, ENOEXEC,
        "the errno belongs to /junk, which is what this process is running"
    );
    assert_eq!(
        literal, ELOOP,
        "AT_SYMLINK_NOFOLLOW asks for the link itself, and a symlink is no program"
    );
    assert_eq!(
        when_there_is_none, ENOENT,
        "a process with no image to name cannot re-execute itself"
    );
    assert!(all_alive(&proc));
    proc.exit(0);
}

/// The arguments are measured before the path is resolved, so one string
/// one byte over `MAX_ARG_STRLEN` is `E2BIG` for a path that does not even
/// exist, while the same path with a short argument gets as far as its own
/// `ENOENT`.
#[test]
fn the_arguments_are_measured_before_the_path_is_resolved() {
    let proc = a_process(48_403, 1);
    let (too_long, just_short_enough) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = CStr::new("/does/not/exist");
            let over = "a".repeat(MAX_ARG_STRLEN);
            let under = "a".repeat(MAX_ARG_STRLEN - 1);
            let too_long = execve(sys, path.at(), Argv::of(&over).at(), 0).await;
            let just_short_enough = execve(sys, path.at(), Argv::of(&under).at(), 0).await;
            (too_long, just_short_enough)
        })
    });
    assert_eq!(
        too_long, E2BIG,
        "one string of MAX_ARG_STRLEN bytes needs MAX_ARG_STRLEN + 1 with its NUL"
    );
    assert_eq!(
        just_short_enough, ENOENT,
        "one byte less passes the measure and reaches the path's own errno"
    );
    assert!(all_alive(&proc));
    proc.exit(0);
}

/// A NULL `path` is `EFAULT`, while a NULL `argv` and a NULL `envp` are
/// allowed: `execve(2)` says they mean one empty argument and no
/// environment, so the call goes on and fails on the path instead.
///
/// And an empty path without `AT_EMPTY_PATH` is `ENOENT`, while with it and
/// `AT_FDCWD` it names the working directory, which is no program.
#[test]
fn a_null_path_is_efault_and_a_null_argv_is_one_empty_argument() {
    let proc = a_process(48_404, 1);
    let (no_path, no_argv, empty_path, the_cwd_itself) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = CStr::new("/does/not/exist");
            let empty = CStr::new("");
            let argv = Argv::of("prog");
            let no_path = execve(sys, 0, argv.at(), 0).await;
            let no_argv = execve(sys, path.at(), 0, 0).await;
            let empty_path = execveat(sys, AT_FDCWD, empty.at(), argv.at(), 0).await;
            let the_cwd_itself =
                execveat(sys, AT_FDCWD, empty.at(), argv.at(), AT_EMPTY_PATH).await;
            (no_path, no_argv, empty_path, the_cwd_itself)
        })
    });
    assert_eq!(no_path, EFAULT, "there is no path to read");
    assert_eq!(
        no_argv, ENOENT,
        "a NULL argv is one empty argument, so the path answers"
    );
    assert_eq!(
        empty_path, ENOENT,
        "an empty path is a path, and it resolves to nothing"
    );
    assert_eq!(
        the_cwd_itself, EACCES,
        "with AT_EMPTY_PATH and AT_FDCWD it is the working directory, which is no program"
    );
    assert!(all_alive(&proc));
    proc.exit(0);
}
