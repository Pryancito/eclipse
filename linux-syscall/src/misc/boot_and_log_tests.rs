//! The four calls a program makes about the machine as a whole rather than
//! about itself: `reboot`, `syslog`, `getrandom` and `membarrier`, plus
//! `arch_prctl`.
//!
//! None of the five was named by a test. The pure halves -- `syslog_plan`,
//! `membarrier_verdict`, `getrandom_args`, `user_base_settable` -- are
//! covered next door and are the best kind of gate: total functions over
//! small argument spaces. What none of them can see is whether the syscall
//! asks them, in what order, and what it does with the answer.
//!
//! **`reboot` is the one that matters.** Its capability test sits ABOVE the
//! magic numbers, which is not an accident of style: the magic numbers are
//! public constants, there to catch a wild call rather than to keep anybody
//! out, so a kernel that checks them first and the capability second lets
//! any process at all power the machine off by quoting two numbers out of a
//! header. The test for it asks with the RIGHT magic and no capability.
//!
//! What this file deliberately never calls is `LINUX_REBOOT_CMD_POWER_OFF`,
//! `_HALT`, `_RESTART` and `_RESTART2`. Those four reach
//! `kernel_hal::cpu::power_off` and `reset`, and from a host test binary
//! that is the test runner going down mid-suite. Every other command and
//! every refusal is reachable without stopping anything, and the four that
//! are not reachable are the four whose arm is one call long.
//!
//! The log needs a ring buffer to read, and in a test binary nothing has
//! registered one: `klog_read` and `klog_buf_size` answer through a slot
//! `zcore` fills at boot, so unregistered they are both 0 and the whole
//! read path answers 0 whatever it does. `klog_register` is public, so the
//! tests here register a small fixed log once and read THAT. It is a
//! binary-wide registration with no way back, so the log below is what any
//! later test in this crate will see too, and `a_kernel_log` is what every
//! test here calls before it asks.
//!
//! Fixed pids live in 47_25x.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use std::sync::Once;
use zircon_object::task::ROOT_JOB;

/// An ordinary user, who holds no capability at all.
const A_USER: u32 = 1000;

/// `LINUX_REBOOT_MAGIC1`, the one value `magic1` may have.
const MAGIC1: u32 = 0xfee1_dead;

/// The four values `magic2` may have: `LINUX_REBOOT_MAGIC2` and the three
/// alternates, which are dates and have been accepted since 2.6.
const MAGIC2: [u32; 4] = [0x2812_1969, 0x0512_1996, 0x1604_1998, 0x2011_2000];

/// `LINUX_REBOOT_CMD_CAD_ON` and `_CAD_OFF`: the two commands that change
/// nothing here and answer 0.
const CAD_ON: u32 = 0x89ab_cdef;
const CAD_OFF: u32 = 0x0000_0000;

/// `LINUX_REBOOT_CMD_SW_SUSPEND`, which this kernel does not implement.
const SW_SUSPEND: u32 = 0x4567_89ab;

/// What a buffer holds before a call, so a byte nobody wrote shows up as
/// one.
const A_SEED: u8 = 0xa5;

/// The fixed kernel log these tests read. Short enough to ask for less than
/// all of it and long enough that "all of it" is not one byte.
const A_LOG: &[u8] = b"<6>eclipse: one line of log\n";

fn a_log_read(dst: &mut [u8]) -> usize {
    let n = dst.len().min(A_LOG.len());
    dst[..n].copy_from_slice(&A_LOG[..n]);
    n
}

fn a_log_size() -> usize {
    A_LOG.len()
}

/// Emitting goes nowhere: every `klog_info!` in this crate's tests arrives
/// here once the registration below has happened, and a test's log line is
/// not what any of this is about.
fn a_log_emit(_level: u8, _line: &str) {}

/// Register [`A_LOG`] as this binary's kernel log, once.
///
/// There is no unregistering, so this is deliberately idempotent and
/// deliberately called by every test here before it asks anything of the
/// log: whichever test runs first, the log is the same one.
fn a_kernel_log() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        kernel_hal::console::klog_register(a_log_read, a_log_size, a_log_emit);
    });
}

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A process whose three user ids are all `uid`.
fn a_process_of(pid: KoID, name: &str, uid: u32) -> Arc<Process> {
    let proc = a_process(pid, name);
    proc.linux().set_resuid(uid, uid, uid).unwrap();
    proc
}

/// The `Syscall` the harness lends, as a `&mut` one. Every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// `reboot(2)`. The `arg` pointer is null: no command this kernel answers
/// reads it.
fn reboot(sys: &Syscall<'_>, magic1: u32, magic2: u32, cmd: u32) -> SysResult {
    as_mut(sys).sys_reboot(magic1, magic2, cmd, 0usize.into())
}

/// `syslog(2)`, with a buffer.
fn syslog(sys: &Syscall<'_>, type_: i32, buf: &mut [u8], len: i32) -> SysResult {
    sys.sys_syslog(type_, (buf.as_mut_ptr() as usize).into(), len)
}

/// `syslog(2)` with no buffer at all.
fn syslog_bare(sys: &Syscall<'_>, type_: i32, len: i32) -> SysResult {
    sys.sys_syslog(type_, 0usize.into(), len)
}

fn getrandom(sys: &Syscall<'_>, buf: &mut [u8], len: usize, flags: u32) -> SysResult {
    as_mut(sys).sys_getrandom((buf.as_mut_ptr() as usize).into(), len, flags)
}

#[test]
fn a_reboot_asked_without_the_capability_is_refused_before_the_magic_is_read() {
    let them = a_process_of(47_250, "reboot-stranger", A_USER);
    on_a_thread(&them, move |sys| {
        Box::pin(async move {
            // The magic numbers are RIGHT and the command is one that
            // changes nothing: what refuses this call is the capability,
            // and nothing else can.
            assert_eq!(
                reboot(sys, MAGIC1, MAGIC2[0], CAD_OFF),
                Err(LxError::EPERM),
                "an ordinary user was allowed to reboot"
            );

            // And with the magic wrong it is STILL `EPERM`, not `EINVAL`:
            // the order of the two refusals is the whole point. A kernel
            // that read the magic first would answer `EINVAL` here and
            // `Ok` to the call above.
            assert_eq!(
                reboot(sys, 0, 0, CAD_OFF),
                Err(LxError::EPERM),
                "the magic was judged before the capability"
            );
        })
    });
}

#[test]
fn root_is_refused_a_wrong_magic_and_obeyed_on_each_of_the_four_right_ones() {
    let root = a_process(47_251, "reboot-root");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // The four values `magic2` may have, each with the one `magic1`
            // may have. A kernel that knows only `LINUX_REBOOT_MAGIC2`
            // refuses three of these, and the three it refuses are the ones
            // libc has used by date since 2.6.
            for (i, magic2) in MAGIC2.iter().enumerate() {
                assert_eq!(
                    reboot(sys, MAGIC1, *magic2, CAD_ON),
                    Ok(0),
                    "magic2 number {} was refused",
                    i
                );
            }

            // A wrong `magic1`, with a right `magic2`.
            assert_eq!(
                reboot(sys, 0xdead_beef, MAGIC2[0], CAD_ON),
                Err(LxError::EINVAL),
                "a wrong magic1 was accepted"
            );
            // A wrong `magic2`, with the right `magic1`. One off from a
            // right one, so a comparison that only looks at some of the
            // word shows up.
            assert_eq!(
                reboot(sys, MAGIC1, MAGIC2[0] + 1, CAD_ON),
                Err(LxError::EINVAL),
                "a wrong magic2 was accepted"
            );
            // The two the other way round: `magic1` is not interchangeable
            // with `magic2`.
            assert_eq!(
                reboot(sys, MAGIC2[0], MAGIC1, CAD_ON),
                Err(LxError::EINVAL),
                "the two magic numbers were read the other way round"
            );
        })
    });
}

#[test]
fn the_commands_this_kernel_answers_without_stopping_the_machine() {
    let root = a_process(47_252, "reboot-cmds");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // The keystroke switches: nothing to do here, and 0 is the
            // answer `ctrlaltdel(8)` wants.
            assert_eq!(reboot(sys, MAGIC1, MAGIC2[0], CAD_ON), Ok(0), "cad_on");
            assert_eq!(reboot(sys, MAGIC1, MAGIC2[0], CAD_OFF), Ok(0), "cad_off");

            // Suspend-to-disk is not implemented, and says so rather than
            // pretending. `CAD_OFF` is zero, so a kernel that fell through
            // to a default arm would answer this one 0.
            assert_eq!(
                reboot(sys, MAGIC1, MAGIC2[0], SW_SUSPEND),
                Err(LxError::EINVAL),
                "sw_suspend answered as if it had worked"
            );

            // A command out of no header at all.
            assert_eq!(
                reboot(sys, MAGIC1, MAGIC2[0], 0x1234_5678),
                Err(LxError::EINVAL),
                "an unknown command was accepted"
            );
        })
    });
}

#[test]
fn reading_the_log_copies_the_bytes_out_and_answers_how_many() {
    a_kernel_log();
    let root = a_process(47_253, "syslog-read");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // What the ring holds, which is what `SIZE_BUFFER` answers.
            let size = kernel_hal::console::klog_buf_size();
            assert_eq!(size, A_LOG.len(), "the registered log is not the one read");
            assert_eq!(
                syslog_bare(sys, SYSLOG_ACTION_SIZE_BUFFER, 0),
                Ok(size),
                "size_buffer did not answer the size of the ring"
            );
            assert_eq!(
                syslog_bare(sys, SYSLOG_ACTION_SIZE_UNREAD, 0),
                Ok(size),
                "size_unread did not answer the size of the ring"
            );

            // A read of less than all of it gets that much, from the
            // front, and touches nothing past it.
            let mut buf = [A_SEED; 64];
            assert_eq!(
                syslog(sys, SYSLOG_ACTION_READ_ALL, &mut buf, 8),
                Ok(8),
                "a short read"
            );
            assert_eq!(&buf[..8], &A_LOG[..8], "the bytes read are not the log's");
            assert!(
                buf[8..].iter().all(|&b| b == A_SEED),
                "the read wrote past the length it was given"
            );

            // A read of more than all of it gets all of it, and the answer
            // is how many bytes CAME, not how many were asked for.
            let mut buf = [A_SEED; 64];
            assert_eq!(
                syslog(sys, SYSLOG_ACTION_READ_ALL, &mut buf, 64),
                Ok(A_LOG.len()),
                "a long read did not answer the length of the log"
            );
            assert_eq!(&buf[..A_LOG.len()], A_LOG, "the whole log");
            assert_eq!(
                buf[A_LOG.len()],
                A_SEED,
                "the read wrote past the end of the log"
            );

            // A length no allocation could serve, which is the reason the
            // read is clamped to the size of the ring before anything is
            // allocated: without the clamp this is `ENOMEM` for a log of
            // twenty-eight bytes.
            let mut buf = [A_SEED; 64];
            assert_eq!(
                syslog(sys, SYSLOG_ACTION_READ_ALL, &mut buf, i32::MAX),
                Ok(A_LOG.len()),
                "a huge length was allocated instead of clamped"
            );
            assert_eq!(&buf[..A_LOG.len()], A_LOG, "the whole log, again");

            // A read of nothing is not an error and writes nothing.
            let mut buf = [A_SEED; 64];
            assert_eq!(
                syslog(sys, SYSLOG_ACTION_READ_ALL, &mut buf, 0),
                Ok(0),
                "a zero-length read"
            );
            assert!(
                buf.iter().all(|&b| b == A_SEED),
                "a zero-length read wrote something"
            );

            // A read with nowhere to put it, and a negative length, which
            // would sign-extend into a huge `usize`.
            assert_eq!(
                syslog_bare(sys, SYSLOG_ACTION_READ_ALL, 8),
                Err(LxError::EINVAL),
                "a read into a null buffer"
            );
            let mut buf = [A_SEED; 64];
            assert_eq!(
                syslog(sys, SYSLOG_ACTION_READ_ALL, &mut buf, -1),
                Err(LxError::EINVAL),
                "a negative length"
            );
        })
    });
}

#[test]
fn only_the_two_open_log_actions_are_open_to_anybody() {
    a_kernel_log();
    let them = a_process_of(47_254, "syslog-stranger", A_USER);
    let root = a_process(47_255, "syslog-root");
    on_a_thread(&them, move |sys| {
        Box::pin(async move {
            // `dmesg` with no privilege at all reads the whole log and asks
            // how big the ring is. Those two are the whole unprivileged
            // surface.
            let mut buf = [A_SEED; 64];
            assert_eq!(
                syslog(sys, SYSLOG_ACTION_READ_ALL, &mut buf, 8),
                Ok(8),
                "read_all needs no privilege"
            );
            assert_eq!(
                syslog_bare(sys, SYSLOG_ACTION_SIZE_BUFFER, 0),
                Ok(A_LOG.len()),
                "size_buffer needs no privilege"
            );

            // Everything else does, including the two reads that look just
            // like the open one and the size that looks just like the open
            // size.
            for (name, type_) in [
                ("read", SYSLOG_ACTION_READ),
                ("read_clear", SYSLOG_ACTION_READ_CLEAR),
            ] {
                let mut buf = [A_SEED; 64];
                assert_eq!(
                    syslog(sys, type_, &mut buf, 8),
                    Err(LxError::EPERM),
                    "{} was open to anybody",
                    name
                );
            }
            for (name, type_) in [
                ("close", SYSLOG_ACTION_CLOSE),
                ("open", SYSLOG_ACTION_OPEN),
                ("clear", SYSLOG_ACTION_CLEAR),
                ("console_off", SYSLOG_ACTION_CONSOLE_OFF),
                ("console_on", SYSLOG_ACTION_CONSOLE_ON),
                ("size_unread", SYSLOG_ACTION_SIZE_UNREAD),
            ] {
                assert_eq!(
                    syslog_bare(sys, type_, 0),
                    Err(LxError::EPERM),
                    "{} was open to anybody",
                    name
                );
            }
            // The level change is privileged too, and that is asked BEFORE
            // the level is judged: an out-of-range level from a stranger
            // is `EPERM` and not `EINVAL`.
            assert_eq!(
                syslog_bare(sys, SYSLOG_ACTION_CONSOLE_LEVEL, 99),
                Err(LxError::EPERM),
                "the level was judged before the privilege"
            );
        })
    });
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // For root the level is judged, and the range is 1 to 8.
            assert_eq!(
                syslog_bare(sys, SYSLOG_ACTION_CONSOLE_LEVEL, 0),
                Err(LxError::EINVAL),
                "level 0"
            );
            for level in 1..=8 {
                assert_eq!(
                    syslog_bare(sys, SYSLOG_ACTION_CONSOLE_LEVEL, level),
                    Ok(0),
                    "level {}",
                    level
                );
            }
            assert_eq!(
                syslog_bare(sys, SYSLOG_ACTION_CONSOLE_LEVEL, 9),
                Err(LxError::EINVAL),
                "level 9"
            );
            // And the actions that do nothing answer 0 rather than
            // refusing, which is what an `openlog` at startup needs.
            assert_eq!(syslog_bare(sys, SYSLOG_ACTION_OPEN, 0), Ok(0), "open");
            assert_eq!(syslog_bare(sys, SYSLOG_ACTION_CLOSE, 0), Ok(0), "close");
            assert_eq!(syslog_bare(sys, SYSLOG_ACTION_CLEAR, 0), Ok(0), "clear");
            // An action out of no header at all.
            assert_eq!(
                syslog_bare(sys, 11, 0),
                Err(LxError::EINVAL),
                "an unknown action was answered"
            );
        })
    });
}

#[test]
fn getrandom_fills_every_byte_it_was_asked_for_past_the_chunk_it_copies_through() {
    let root = a_process(47_256, "getrandom");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // Twice the 1 KiB chunk the copy goes through, so a loop that
            // writes every chunk over the first one leaves the whole
            // second half untouched. The odds of 1024 random bytes all
            // being the seed are not worth writing down.
            let mut buf = [A_SEED; 2048];
            assert_eq!(getrandom(sys, &mut buf, 2048, 0), Ok(2048), "a long read");
            assert!(
                buf[1024..].iter().any(|&b| b != A_SEED),
                "the second chunk was never written"
            );
            assert!(
                buf[..1024].iter().any(|&b| b != A_SEED),
                "the first chunk was never written"
            );

            // Not one byte past the length: the tail of the allocation is
            // the caller's.
            let mut buf = [A_SEED; 2048];
            assert_eq!(getrandom(sys, &mut buf, 1500, 0), Ok(1500), "an odd length");
            assert!(
                buf[1500..].iter().all(|&b| b == A_SEED),
                "getrandom wrote past the length it was given"
            );

            // Nothing asked for is nothing written, and not an error.
            let mut buf = [A_SEED; 8];
            assert_eq!(getrandom(sys, &mut buf, 0, 0), Ok(0), "a zero-length read");
            assert!(
                buf.iter().all(|&b| b == A_SEED),
                "a zero-length getrandom wrote something"
            );
        })
    });
}

#[test]
fn the_three_random_flags_this_kernel_knows_are_accepted_and_anything_else_is_einval() {
    let root = a_process(47_257, "getrandom-flags");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // None of the three changes what comes back here -- the
            // entropy source is always ready -- but which bits EXIST is
            // the caller's question, and a program probing for
            // `GRND_INSECURE` has to be told whether this kernel knows it.
            for flags in [
                0,
                GRND_NONBLOCK,
                GRND_RANDOM,
                GRND_INSECURE,
                GRND_NONBLOCK | GRND_RANDOM,
                GRND_NONBLOCK | GRND_INSECURE,
            ] {
                let mut buf = [A_SEED; 16];
                assert_eq!(
                    getrandom(sys, &mut buf, 16, flags),
                    Ok(16),
                    "flags {:#x} were refused",
                    flags
                );
            }

            // The one combination the kernel calls nonsense: insecure and
            // blocking-on-the-good-pool at once.
            let mut buf = [A_SEED; 16];
            assert_eq!(
                getrandom(sys, &mut buf, 16, GRND_INSECURE | GRND_RANDOM),
                Err(LxError::EINVAL),
                "insecure and random together"
            );

            // A bit out of no header.
            let mut buf = [A_SEED; 16];
            assert_eq!(
                getrandom(sys, &mut buf, 16, 0x8000),
                Err(LxError::EINVAL),
                "an undefined flag was accepted"
            );
        })
    });
}

#[test]
fn membarrier_answers_every_command_it_advertises_and_no_others() {
    let root = a_process(47_258, "membarrier");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // The query is the whole protocol: a program asks what this
            // kernel can do and then uses only that.
            let advertised = sys.sys_membarrier(MEMBARRIER_QUERY, 0, 0);
            assert_eq!(
                advertised,
                Ok(MEMBARRIER_SUPPORTED as usize),
                "the query did not answer the advertised set"
            );
            let advertised = advertised.unwrap() as i32;
            assert_ne!(advertised, 0, "this kernel advertises nothing");

            // Every bit the query set must be a command that works, and
            // every command that works must be a bit the query set. The
            // two halves are written in different places and nothing made
            // them agree.
            for bit in 0..31 {
                let cmd = 1i32 << bit;
                let answer = sys.sys_membarrier(cmd, 0, 0);
                if advertised & cmd != 0 {
                    assert_eq!(answer, Ok(0), "an advertised command {:#x} failed", cmd);
                } else {
                    assert_eq!(
                        answer,
                        Err(LxError::EINVAL),
                        "an unadvertised command {:#x} was accepted",
                        cmd
                    );
                }
            }

            // A flag: the only one defined belongs to a command this
            // kernel does not advertise, so any flag at all is refused --
            // and refused before the command is looked at, which is why
            // the query itself is refused with one.
            assert_eq!(
                sys.sys_membarrier(MEMBARRIER_QUERY, 1, 0),
                Err(LxError::EINVAL),
                "a flag on the query was accepted"
            );
            assert_eq!(
                sys.sys_membarrier(MEMBARRIER_GLOBAL_EXPEDITED, 1, 0),
                Err(LxError::EINVAL),
                "a flag on a barrier was accepted"
            );

            // The cpu id argument is not read at all, so it cannot refuse
            // anything.
            assert_eq!(
                sys.sys_membarrier(MEMBARRIER_QUERY, 0, -1),
                Ok(MEMBARRIER_SUPPORTED as usize),
                "the cpu id changed the answer"
            );
        })
    });
}

#[cfg(target_arch = "x86_64")]
#[test]
fn the_execution_base_a_hosted_kernel_cannot_honour_is_refused_and_the_other_round_trips() {
    let root = a_process(47_259, "arch-prctl");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            // A base inside the user address space, which is the only
            // thing `wrmsr` on the way out would accept.
            let base = 0x1234_5000usize;
            assert_eq!(
                as_mut(sys).sys_arch_prctl(ARCH_SET_FS, base),
                Ok(0),
                "a user thread pointer was refused"
            );
            let mut got = 0usize;
            assert_eq!(
                as_mut(sys).sys_arch_prctl(ARCH_GET_FS, (&mut got) as *mut usize as usize),
                Ok(0),
                "arch_prctl could not read the base back"
            );
            assert_eq!(got, base, "the base read back is not the one set");

            // Outside it, `EPERM`: the value would fault on the exit path,
            // in kernel mode, which is a kernel fault and not a user one.
            assert_eq!(
                as_mut(sys).sys_arch_prctl(ARCH_SET_FS, usize::MAX),
                Err(LxError::EPERM),
                "a non-canonical thread pointer was accepted"
            );

            // The GS base is refused outright on a hosted kernel, because
            // the host runtime owns `gs` and the value would never reach
            // the CPU: accepting it would promise user code a base it will
            // never see.
            if kernel_hal::LIBOS {
                assert_eq!(
                    as_mut(sys).sys_arch_prctl(ARCH_SET_GS, base),
                    Err(LxError::EINVAL),
                    "a hosted kernel accepted a gs base it cannot honour"
                );
            }

            // A code out of no header.
            assert_eq!(
                as_mut(sys).sys_arch_prctl(0x9999, base),
                Err(LxError::EINVAL),
                "an unknown code was accepted"
            );
        })
    });
}
