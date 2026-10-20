//! `/proc/<pid>/status`: its lines, in Linux's order, with the process's own
//! numbers.
//!
//! Fixed pids live in 45_37x and 45_38x here; they are a global table, so every file
//! that pins one has its own range.

use super::*;
use crate::process::{Credentials, RLimit};
use crate::signal::{SignalAction, SignalActionFlags};
use rcore_fs_ramfs::RamFS;

fn creds() -> Credentials {
    Credentials {
        ruid: 1000,
        euid: 1001,
        suid: 1002,
        rgid: 2000,
        egid: 2001,
        sgid: 2002,
        fsuid: 1003,
        fsgid: 2003,
        groups: alloc::vec![1000, 4, 24],
        umask: 0o022,
    }
}

/// A process with one thread, in a job of its own under the root, so the
/// census that `SigQ:` takes can see it.
fn a_process(pid: u64) -> (Arc<Job>, Arc<Process>, Arc<Thread>) {
    let job = ROOT_JOB.create_child().unwrap();
    let proc =
        Process::create_with_fixed_id_ext(&job, pid, "status", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (job, proc, thread)
}

/// The value of line `key`, or `None` when the file has no such line.
fn line<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(":\t"))
}

fn handler(handler: usize) -> SignalAction {
    SignalAction {
        handler,
        flags: SignalActionFlags::empty(),
        restorer: 0,
        mask: Sigset::default(),
    }
}

/// `/proc/<pid>/status` said `Uid:\t0\t0\t0\t0` and `Gid:\t0\t0\t0\t0` for
/// every process on the machine, whoever it belonged to. That line is what
/// `polkit`, `pkexec`, `sudo` and `ps -u` read to learn who is asking, so
/// every unprivileged process looked like root to them.
#[test]
fn the_uid_and_gid_lines_are_the_process_s_own_ids_in_proc_5_order() {
    // Real, effective, saved, filesystem: the order proc(5) gives, and the
    // one `ps` and `polkit` parse by position. `FDSize:` goes between `Gid:`
    // and `Groups:`, as `task_state()` writes it.
    let mut lines = String::new();
    id_lines(&mut lines, &creds(), 256);
    assert_eq!(
        lines,
        "Uid:\t1000\t1001\t1002\t1003\nGid:\t2000\t2001\t2002\t2003\nFDSize:\t256\nGroups:\t1000 4 24 \n"
    );
    // With no groups the space is still there: `task_state()` writes it
    // after the list whatever the list holds.
    let mut lines = String::new();
    let no_groups = Credentials {
        groups: alloc::vec![],
        ..creds()
    };
    id_lines(&mut lines, &no_groups, 64);
    assert!(lines.ends_with("FDSize:\t64\nGroups:\t \n"), "{:?}", lines);
}

#[test]
fn a_process_that_dropped_to_a_user_reports_that_user() {
    // The whole file, on a real process: root that became uid 1000 via
    // setresuid, the way `login` and `su` end up.
    let proc = Process::create_with_fixed_id_ext(
        &Job::root(),
        4242,
        "root",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let lp = proc.linux();
    lp.set_resgid(100, 100, 100).unwrap();
    lp.set_groups(alloc::vec![100, 27]);
    lp.set_resuid(1000, 1000, 1000).unwrap();
    let status = proc_pid_status(&proc);
    assert!(
        status.contains("PPid:\t0\nTracerPid:\t0\nUid:\t1000\t1000\t1000\t1000\nGid:\t100\t100\t100\t100\nFDSize:\t64\nGroups:\t100 27 \nNStgid:"),
        "{:?}",
        status
    );
}

/// The `State:` line carries the letter and the word, and different readers
/// take different halves: `procps` matches the word, `htop` the letter. The
/// four rows as a table, because reaching `T` through a process needs a
/// leader thread with a context, which no test can give it.
#[test]
fn the_state_line_of_status_carries_the_letter_and_the_word() {
    assert_eq!(status_state_text('R'), "R (running)");
    assert_eq!(status_state_text('T'), "T (stopped)");
    assert_eq!(status_state_text('Z'), "Z (zombie)");
    assert_eq!(status_state_text('S'), "S (sleeping)");
}

/// The file stopped at eleven lines. These are the keys `proc_pid_status()`
/// writes, in its order, but for the memory lines this kernel does not keep
/// numbers for yet.
#[test]
fn the_file_has_linux_s_lines_in_linux_s_order() {
    let (_job, proc, _thread) = a_process(45_371);
    let status = proc_pid_status(&proc);
    let keys: Vec<&str> = status
        .lines()
        .filter_map(|l| l.split(":\t").next())
        .collect();
    assert_eq!(
        keys,
        [
            "Name",
            "Umask",
            "State",
            "Tgid",
            "Ngid",
            "Pid",
            "PPid",
            "TracerPid",
            "Uid",
            "Gid",
            "FDSize",
            "Groups",
            "NStgid",
            "NSpid",
            "NSpgid",
            "NSsid",
            "Kthread",
            "VmSize",
            "VmRSS",
            "Threads",
            "SigQ",
            "SigPnd",
            "ShdPnd",
            "SigBlk",
            "SigIgn",
            "SigCgt",
            "CapInh",
            "CapPrm",
            "CapEff",
            "CapBnd",
            "CapAmb",
            "NoNewPrivs",
            "Speculation_Store_Bypass",
            "SpeculationIndirectBranch",
            "Cpus_allowed",
            "Cpus_allowed_list",
            "Mems_allowed",
            "Mems_allowed_list",
            "voluntary_ctxt_switches",
            "nonvoluntary_ctxt_switches",
        ],
        "{:?}",
        status
    );
    assert!(status.ends_with('\n'), "{:?}", status);
}

/// `Umask:` is the process's, written as `%#04o`; `NoNewPrivs:` follows
/// `prctl(PR_SET_NO_NEW_PRIVS)`; the namespace lines are the ids `getpid`,
/// `getpgid` and `getsid` answer.
#[test]
fn the_process_s_own_umask_flag_and_ids() {
    let (_job, proc, _thread) = a_process(45_372);
    let lp = proc.linux();
    lp.set_umask(0o027);
    lp.become_session_leader(45_389);
    lp.set_pgid_raw(45_379);
    let status = proc_pid_status(&proc);
    assert_eq!(line(&status, "Umask"), Some("0027"), "{:?}", status);
    assert_eq!(line(&status, "NoNewPrivs"), Some("0"));
    assert_eq!(line(&status, "NStgid"), Some("45372"));
    assert_eq!(line(&status, "NSpid"), Some("45372"));
    assert_eq!(line(&status, "NSpgid"), Some("45379"));
    assert_eq!(line(&status, "NSsid"), Some("45389"));
    lp.set_umask(0);
    lp.set_no_new_privs();
    let status = proc_pid_status(&proc);
    assert_eq!(line(&status, "Umask"), Some("0000"));
    assert_eq!(line(&status, "NoNewPrivs"), Some("1"));
}

/// A zombie has given up its `fs_struct`, its descriptors and its memory:
/// Linux writes no `Umask:`, an `FDSize:` of 0 and no `Vm` lines.
#[test]
fn a_zombie_has_no_umask_no_descriptor_table_and_no_memory() {
    let (_job, proc, _thread) = a_process(45_373);
    proc.exit(0);
    let status = proc_pid_status(&proc);
    assert_eq!(line(&status, "Umask"), None, "{:?}", status);
    assert_eq!(line(&status, "FDSize"), Some("0"), "{:?}", status);
    assert_eq!(line(&status, "VmSize"), None, "{:?}", status);
    assert_eq!(line(&status, "VmRSS"), None, "{:?}", status);
    assert_eq!(line(&status, "Threads"), Some("1"), "{:?}", status);
}

/// `alloc_fdtable()`: 64 slots to start, then a power of two of 128 that
/// holds the descriptor. A shell keeps its terminal on 255 and reads 256.
#[test]
fn the_descriptor_table_grows_as_linux_s_does() {
    let size = |fd: Option<i32>| fd_table_size(fd.map(FileDesc::from));
    assert_eq!(size(None), 64);
    assert_eq!(size(Some(0)), 64);
    assert_eq!(size(Some(63)), 64);
    assert_eq!(size(Some(64)), 128);
    assert_eq!(size(Some(127)), 128);
    assert_eq!(size(Some(128)), 256);
    assert_eq!(size(Some(255)), 256);
    assert_eq!(size(Some(256)), 512);
    assert_eq!(size(Some(1023)), 1024);
}

/// `FDSize:` is measured on the highest descriptor open, not the lowest.
#[test]
fn the_highest_descriptor_sizes_the_table() {
    use crate::fs::OpenFlags;
    use crate::process::dup_fd_tests::{an_open_log, Log};
    let (_job, proc, _thread) = a_process(45_381);
    let lp = proc.linux();
    let log = || an_open_log(Log::new(), OpenFlags::WRONLY);
    lp.add_file_at(FileDesc::from(0), log()).unwrap();
    assert_eq!(line(&proc_pid_status(&proc), "FDSize"), Some("64"));
    lp.add_file_at(FileDesc::from(255), log()).unwrap();
    let status = proc_pid_status(&proc);
    assert_eq!(line(&status, "FDSize"), Some("256"), "{:?}", status);
}

/// `SigIgn:` and `SigCgt:` are the dispositions; `SigBlk:` and `SigPnd:` the
/// task's own mask and signals, `ShdPnd:` the process's. `ps s` and anyone
/// asking why a signal does nothing read these, bit `n - 1` for signal `n`.
#[test]
fn the_signal_masks_are_the_process_s_and_the_task_s() {
    let (_job, proc, thread) = a_process(45_374);
    let lp = proc.linux();
    lp.set_signal_action(Signal::SIGINT, handler(SIG_IGN));
    lp.set_signal_action(Signal::SIGTERM, handler(0x1234));
    lp.queue_shared_signal(Signal::SIGUSR2, None);
    {
        let mut lt = thread.lock_linux();
        let mut mask = Sigset::default();
        mask.insert(Signal::SIGUSR1);
        lt.set_signal_mask(mask);
        lt.signals.insert(Signal::SIGUSR1);
    }
    let status = proc_pid_status(&proc);
    assert_eq!(
        line(&status, "SigPnd"),
        Some("0000000000000200"),
        "{:?}",
        status
    );
    assert_eq!(line(&status, "ShdPnd"), Some("0000000000000800"));
    assert_eq!(line(&status, "SigBlk"), Some("0000000000000200"));
    assert_eq!(line(&status, "SigIgn"), Some("0000000000000002"));
    assert_eq!(line(&status, "SigCgt"), Some("0000000000004000"));
}

/// `SigQ:` counts what is pending for every task of the real user, and the
/// limit is that user's `RLIMIT_SIGPENDING`, unlimited until somebody sets it.
#[test]
fn the_signal_queue_is_the_user_s() {
    let uid = 45_375;
    let (job, proc, thread) = a_process(45_375);
    let (_other_job, other, _) = a_process(45_376);
    for p in [&proc, &other] {
        p.linux().set_resuid(uid, uid, uid).unwrap();
    }
    // Somebody else's, in the same job: not this user's queue.
    let stranger =
        Process::create_with_fixed_id_ext(&job, 45_380, "x", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    stranger
        .linux()
        .set_resuid(uid + 1, uid + 1, uid + 1)
        .unwrap();
    stranger.linux().queue_shared_signal(Signal::SIGUSR1, None);
    {
        let mut lt = thread.lock_linux();
        lt.signals.insert(Signal::SIGUSR1);
        lt.signals.insert(Signal::SIGUSR2);
    }
    other.linux().queue_shared_signal(Signal::SIGUSR2, None);
    assert_eq!(signals_pending_for_user_in(&job, uid), 2, "one job's");
    let status = proc_pid_status(&proc);
    assert_eq!(
        line(&status, "SigQ"),
        Some("3/18446744073709551615"),
        "{:?}",
        status
    );
    let limit = RLimit { cur: 100, max: 200 };
    proc.linux()
        .rlimit(crate::process::RLIMIT_SIGPENDING, Some(limit), false)
        .unwrap();
    let status = proc_pid_status(&proc);
    assert_eq!(line(&status, "SigQ"), Some("3/100"), "{:?}", status);
}

/// The capability sets are what `capget` publishes and the bounding set
/// is every capability there is.
#[test]
fn the_capability_sets_are_the_ones_capget_publishes() {
    let (_job, proc, _thread) = a_process(45_377);
    let status = proc_pid_status(&proc);
    assert_eq!(line(&status, "CapInh"), Some("0000000000000000"));
    assert_eq!(line(&status, "CapPrm"), Some("000001ffffffffff"), "root");
    assert_eq!(line(&status, "CapEff"), Some("000001ffffffffff"), "root");
    assert_eq!(line(&status, "CapBnd"), Some("000001ffffffffff"));
    assert_eq!(line(&status, "CapAmb"), Some("0000000000000000"));
    proc.linux().set_resuid(1000, 1000, 1000).unwrap();
    let status = proc_pid_status(&proc);
    assert_eq!(line(&status, "CapPrm"), Some("0000000000000000"));
    assert_eq!(line(&status, "CapEff"), Some("0000000000000000"));
    assert_eq!(line(&status, "CapBnd"), Some("000001ffffffffff"));
}

/// `%*pb` and `%*pbl`, over as many bits as there are CPUs.
#[test]
fn a_cpu_mask_is_written_as_linux_writes_a_bitmap() {
    assert_eq!(bitmap_hex(0b11, 2), "3");
    assert_eq!(bitmap_list(0b11, 2), "0-1");
    assert_eq!(bitmap_hex(0xff, 8), "ff");
    assert_eq!(bitmap_hex(0x0f, 8), "0f");
    assert_eq!(bitmap_list(0b1011_0001, 8), "0,4-5,7");
    assert_eq!(bitmap_hex(0xff_ffff_ffff, 40), "ff,ffffffff");
    assert_eq!(bitmap_list(0xff_ffff_ffff, 40), "0-39");
    assert_eq!(bitmap_hex(0x1_0000_0001, 33), "1,00000001");
    assert_eq!(bitmap_list(0x1_0000_0001, 33), "0,32");
    assert_eq!(bitmap_hex(u64::MAX, 64), "ffffffff,ffffffff");
    assert_eq!(bitmap_list(u64::MAX, 64), "0-63");
    assert_eq!(bitmap_list(0, 4), "");
    // Bits past the last CPU are not written.
    assert_eq!(bitmap_hex(0xff, 2), "3");
}

/// `Cpus_allowed:` is the task's affinity, as `sched_getaffinity` answers it:
/// only the CPUs there are.
#[test]
fn the_allowed_cpus_are_the_task_s_affinity() {
    let mut out = String::new();
    cpu_lines(&mut out, u64::MAX, 0b1111);
    assert_eq!(
        out,
        "Cpus_allowed:\tf\nCpus_allowed_list:\t0-3\nMems_allowed:\t1\nMems_allowed_list:\t0\n"
    );
    let mut out = String::new();
    cpu_lines(&mut out, 0b0110, 0b1111);
    assert!(
        out.starts_with("Cpus_allowed:\t6\nCpus_allowed_list:\t1-2\n"),
        "{:?}",
        out
    );
    // A CPU that never came up is not one a task may run on, and the mask
    // is still as wide as the highest one that did.
    let mut out = String::new();
    cpu_lines(&mut out, u64::MAX, 0b1011);
    assert!(
        out.starts_with("Cpus_allowed:\tb\nCpus_allowed_list:\t0-1,3\n"),
        "{:?}",
        out
    );
}

/// `/proc/<pid>/task/<tid>/status` is that thread's: its own id in `Pid:` and
/// `NSpid:`, its own mask, its own CPUs. The process's file is the leader's.
#[test]
fn a_thread_s_file_has_the_thread_s_mask_and_cpus() {
    let (_job, proc, leader) = a_process(45_378);
    let worker = Thread::create_linux(&proc).unwrap();
    let mut mask = Sigset::default();
    mask.insert(Signal::SIGHUP);
    worker.lock_linux().set_signal_mask(mask);
    let online = kernel_hal::cpu_online_mask();
    let first = 1u64 << online.trailing_zeros();
    worker.set_affinity(first).unwrap();

    let status = proc_task_status(&proc, Some(&worker));
    assert_eq!(line(&status, "Threads"), Some("2"), "{:?}", status);
    let tid = alloc::format!("{}", worker.id());
    assert_eq!(line(&status, "Pid"), Some(tid.as_str()), "{:?}", status);
    assert_eq!(line(&status, "NSpid"), Some(tid.as_str()));
    assert_eq!(line(&status, "NStgid"), Some("45378"));
    assert_eq!(line(&status, "SigBlk"), Some("0000000000000001"));
    let cpus = (u64::BITS - online.leading_zeros()).max(1);
    assert_eq!(
        line(&status, "Cpus_allowed_list"),
        Some(bitmap_list(first, cpus).as_str())
    );

    let status = proc_pid_status(&proc);
    assert_eq!(
        line(&status, "SigBlk"),
        Some("0000000000000000"),
        "the leader's"
    );
    assert_eq!(
        line(&status, "Cpus_allowed_list"),
        Some(bitmap_list(leader.affinity() & online, cpus).as_str())
    );
}

/// The context switch counts are the task's own, as Linux keeps them per
/// task: the thread's in its file, the leader's in the process's.
#[test]
fn the_context_switches_are_the_task_s_own() {
    let (_job, proc, leader) = a_process(45_382);
    let worker = Thread::create_linux(&proc).unwrap();
    for _ in 0..3 {
        leader.note_context_switch(false);
    }
    leader.note_context_switch(true);
    worker.note_context_switch(false);
    for _ in 0..5 {
        worker.note_context_switch(true);
    }

    let status = proc_pid_status(&proc);
    assert_eq!(
        line(&status, "voluntary_ctxt_switches"),
        Some("3"),
        "{:?}",
        status
    );
    assert_eq!(line(&status, "nonvoluntary_ctxt_switches"), Some("1"));
    let status = proc_task_status(&proc, Some(&worker));
    assert_eq!(
        line(&status, "voluntary_ctxt_switches"),
        Some("1"),
        "{:?}",
        status
    );
    assert_eq!(line(&status, "nonvoluntary_ctxt_switches"), Some("5"));
}
