//! `PIDFD_GET_INFO`, the one ioctl a pidfd answers: the pid/tgid/ppid it
//! names and the target's credentials, into a `struct pidfd_info`.
//!
//! What Linux 6.18 does, measured with glibc and musl (`info-probe`): the
//! `mask` the caller passes in is ignored — the kernel fills every field it
//! can and sets the output `mask` to what it provided — so a `mask` of 0 in
//! still comes back filled. A reply names `PIDFD_INFO_PID | PIDFD_INFO_CREDS`
//! here (`0x3`); Linux also sets `PIDFD_INFO_CGROUPID`, which Eclipse cannot
//! (it has no cgroups), and neither sets `PIDFD_INFO_EXIT` for a task that
//! has not been reaped. A pid that is gone — a non-leader thread that has
//! exited, a process that has been reaped — is `ESRCH`, and a call with a
//! struct of another size, or on an fd that is not a pidfd, is `ENOTTY`.
//!
//! The call goes through `sys_ioctl`, so these also check that a pidfd is
//! not caught by any of the special ioctl paths before its own handler.
//!
//! Fixed pids live in 49_36x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::convert::TryInto;
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `_IOWR(0xff, 11, struct pidfd_info)`, the 72-byte struct making the size
/// field `0x48`.
const PIDFD_GET_INFO: usize = 0xc048_ff0b;
/// The same command as a musl caller passes it: bit 31 is set, and musl types
/// `ioctl`'s request `int`, so it sign-extends to 64 bits. The ioctl matches
/// the low 32 bits, so this reaches the handler exactly as the clean value
/// does.
const PIDFD_GET_INFO_MUSL: usize = 0xffff_ffff_c048_ff0b;
/// The same command with a 24-byte struct (`0x18`): a different number, which
/// is why a short struct is `ENOTTY`, not a truncated answer.
const PIDFD_GET_INFO_SHORT: usize = 0xc018_ff0b;
/// `PIDFD_INFO_PID | PIDFD_INFO_CREDS`, and neither `CGROUPID` (`1 << 2`) nor
/// `EXIT` (`1 << 3`).
const MASK_PID_CREDS: u64 = 0b11;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "info", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}
fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

/// A leader pidfd reports the process's pid as both pid and tgid, its
/// parent's pid as ppid, and every one of the eight credential ids in its
/// own field. The `mask` is `PID | CREDS`, and the cgroup-id and exit fields
/// are zero.
#[test]
fn get_info_fills_pid_tgid_ppid_and_every_credential() {
    let parent = a_process(49_360);
    let target = a_process(49_361);
    target.linux().set_parent(&parent);
    // Eight distinct ids, so a field written from the wrong one is caught.
    // `euid` stays root (`-1` is "leave it") so the two `setfs*id` calls below
    // still hold `CAP_SET{U,G}ID` and take an arbitrary id; a `set*uid` that
    // left root would drop those caps and recouple fsuid to euid. 0 is the
    // value of `euid` alone, so no two fields share a value.
    const LEAVE: u32 = u32::MAX;
    target.linux().set_resgid(21, 22, 23).unwrap();
    target.linux().set_resuid(11, LEAVE, 13).unwrap();
    target.linux().set_fsuid(14);
    target.linux().set_fsgid(24);
    let (tpid, ppid) = (target.id(), parent.id());
    let t = target.clone();
    on_a_thread(&a_process(49_362), move |sys| {
        Box::pin(async move {
            let fd = sys
                .linux_process()
                .add_file(PidFd::new(t, OpenFlags::empty()))
                .unwrap();
            let mut buf = [0u8; 72];
            let rc = sys
                .sys_ioctl(fd, PIDFD_GET_INFO, buf.as_mut_ptr() as usize, 0, 0)
                .await;
            assert_eq!(rc, Ok(0));
            assert_eq!(u64_at(&buf, 0), MASK_PID_CREDS, "mask");
            assert_eq!(u64_at(&buf, 8), 0, "cgroupid");
            assert_eq!(u32_at(&buf, 16) as u64, tpid, "pid");
            assert_eq!(u32_at(&buf, 20) as u64, tpid, "tgid");
            assert_eq!(u32_at(&buf, 24) as u64, ppid, "ppid");
            assert_eq!(u32_at(&buf, 28), 11, "ruid");
            assert_eq!(u32_at(&buf, 32), 21, "rgid");
            assert_eq!(u32_at(&buf, 36), 0, "euid");
            assert_eq!(u32_at(&buf, 40), 22, "egid");
            assert_eq!(u32_at(&buf, 44), 13, "suid");
            assert_eq!(u32_at(&buf, 48), 23, "sgid");
            assert_eq!(u32_at(&buf, 52), 14, "fsuid");
            assert_eq!(u32_at(&buf, 56), 24, "fsgid");
            assert_eq!(i32_at(&buf, 60), 0, "exit_code");
            assert_eq!(u32_at(&buf, 64), 0, "coredump_mask");
            assert_eq!(u32_at(&buf, 68), 0, "spare1");
        })
    });
}

/// With no parent, `ppid` is 0, as `getppid` answers it.
#[test]
fn get_info_ppid_is_zero_with_no_parent() {
    let target = a_process(49_363);
    let tpid = target.id();
    let t = target.clone();
    on_a_thread(&a_process(49_364), move |sys| {
        Box::pin(async move {
            let fd = sys
                .linux_process()
                .add_file(PidFd::new(t, OpenFlags::empty()))
                .unwrap();
            let mut buf = [0u8; 72];
            assert_eq!(
                sys.sys_ioctl(fd, PIDFD_GET_INFO, buf.as_mut_ptr() as usize, 0, 0)
                    .await,
                Ok(0)
            );
            assert_eq!(u32_at(&buf, 16) as u64, tpid, "pid");
            assert_eq!(u32_at(&buf, 24), 0, "ppid with no parent");
        })
    });
}

/// A thread pidfd names the thread: its tid as pid, and the process as tgid.
#[test]
fn a_thread_pidfd_reports_the_thread_as_pid_and_the_process_as_tgid() {
    let target = a_process(49_365);
    // The first thread is the leader, whose tid is the pid; the second is a
    // plain thread, whose tid is its own.
    let _leader = Thread::create_linux(&target).unwrap();
    let thread = Thread::create_linux(&target).unwrap();
    let (tid, tgid) = (thread.id(), target.id());
    let (t, th) = (target.clone(), thread.clone());
    on_a_thread(&a_process(49_366), move |sys| {
        Box::pin(async move {
            let fd = sys
                .linux_process()
                .add_file(PidFd::of_thread(t, th, OpenFlags::empty()))
                .unwrap();
            let mut buf = [0u8; 72];
            assert_eq!(
                sys.sys_ioctl(fd, PIDFD_GET_INFO, buf.as_mut_ptr() as usize, 0, 0)
                    .await,
                Ok(0)
            );
            assert_eq!(u64_at(&buf, 0), MASK_PID_CREDS, "mask");
            assert_eq!(u32_at(&buf, 16) as u64, tid, "pid is the thread's tid");
            assert_eq!(u32_at(&buf, 20) as u64, tgid, "tgid is the process's");
            assert_ne!(tid, tgid, "the fixture needs a non-leader thread");
        })
    });
}

/// A pid that is gone is `ESRCH`, and nothing is written.
#[test]
fn a_gone_pid_is_esrch_and_writes_nothing() {
    let target = a_process(49_367);
    // A non-leader thread (the second), so terminating it does not exit the
    // process: its pidfd is gone once the thread is dead.
    let _leader = Thread::create_linux(&target).unwrap();
    let thread = Thread::create_linux(&target).unwrap();
    let (t, th) = (target.clone(), thread.clone());
    on_a_thread(&a_process(49_368), move |sys| {
        Box::pin(async move {
            let fd = sys
                .linux_process()
                .add_file(PidFd::of_thread(t, th.clone(), OpenFlags::empty()))
                .unwrap();
            th.terminate_abandoned();
            let mut buf = [0u8; 72];
            assert_eq!(
                sys.sys_ioctl(fd, PIDFD_GET_INFO, buf.as_mut_ptr() as usize, 0, 0)
                    .await,
                Err(LxError::ESRCH)
            );
            assert_eq!(buf, [0u8; 72], "a refused call writes nothing");
        })
    });
}

/// Another request, or `PIDFD_GET_INFO` with a struct of another size, is
/// `ENOTTY` — the command a pidfd does not answer — and writes nothing.
#[test]
fn an_unknown_request_is_enotty() {
    let target = a_process(49_369);
    let t = target.clone();
    on_a_thread(&a_process(49_370), move |sys| {
        Box::pin(async move {
            let fd = sys
                .linux_process()
                .add_file(PidFd::new(t, OpenFlags::empty()))
                .unwrap();
            assert_eq!(
                sys.sys_ioctl(fd, 0x1234, 0, 0, 0).await,
                Err(LxError::ENOTTY),
                "a command a pidfd does not know"
            );
            let mut buf = [0u8; 72];
            assert_eq!(
                sys.sys_ioctl(fd, PIDFD_GET_INFO_SHORT, buf.as_mut_ptr() as usize, 0, 0)
                    .await,
                Err(LxError::ENOTTY),
                "a struct of the wrong size is a different number"
            );
            assert_eq!(buf, [0u8; 72], "and writes nothing");
        })
    });
}

/// The command as a musl caller passes it (sign-extended to 64 bits) reaches
/// the handler just as the clean value does: the ioctl matches the low 32
/// bits, which is what Linux does and what lets a musl program use this at
/// all. Measured: on a musl userspace the full request arrives as
/// `0xffff_ffff_c048_ff0b`.
#[test]
fn a_sign_extended_command_still_fills_the_struct() {
    let target = a_process(49_371);
    let tpid = target.id();
    let t = target.clone();
    on_a_thread(&a_process(49_372), move |sys| {
        Box::pin(async move {
            let fd = sys
                .linux_process()
                .add_file(PidFd::new(t, OpenFlags::empty()))
                .unwrap();
            let mut buf = [0u8; 72];
            assert_eq!(
                sys.sys_ioctl(fd, PIDFD_GET_INFO_MUSL, buf.as_mut_ptr() as usize, 0, 0)
                    .await,
                Ok(0),
                "the sign-extended command is the same command"
            );
            assert_eq!(u64_at(&buf, 0), MASK_PID_CREDS, "mask");
            assert_eq!(u32_at(&buf, 16) as u64, tpid, "pid");
            assert_eq!(u32_at(&buf, 20) as u64, tpid, "tgid");
        })
    });
}
