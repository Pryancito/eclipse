//! `/proc/<pid>/stat` fields 16/17 (cutime/cstime), 22 (starttime), 23
//! (vsize) and 24 (rss) were written as 0 for every process, while
//! `status`, `statm`, `getrusage` and `times()` already had the numbers:
//! `ps aux` showed VSZ 0 and RSS 0, `ps -o etime` the uptime of the
//! machine for everything, and `top`'s TIME+ never counted a reaped child.

use super::*;
use crate::process::{ChildCpu, LinuxProcess};
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{SCHED_FIFO, SCHED_NORMAL};
use zircon_object::vm::{MMUFlags, VmObject, PAGE_SIZE};

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(
        &Job::root(),
        pid,
        "stat",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap()
}

/// Field `n` (1-based, as proc(5) numbers them) of the stat line. The
/// comm is in parentheses and may hold spaces, so the split starts
/// after the closing one.
fn field(line: &str, n: usize) -> i64 {
    assert!(n >= 3);
    let after_comm = &line[line.rfind(')').unwrap() + 2..];
    after_comm
        .split(' ')
        .nth(n - 3)
        .unwrap_or_else(|| panic!("field {} of {:?}", n, line))
        .trim()
        .parse()
        .unwrap()
}

/// Field 8 (`tpgid`) is the foreground group of THIS process's terminal.
/// It came from the VT that was on screen, while field 7 (`tty_nr`) on the
/// line above already came from the process's own: the two described two
/// different terminals, so a shell on a VT that was not on screen looked
/// like a background job to `ps`, which prints `+` exactly when a process's
/// pgrp equals its tty's tpgid.
#[test]
fn tpgid_is_the_foreground_group_of_the_processs_own_terminal() {
    let proc = Process::create_with_fixed_id_ext(
        &Job::root(),
        4321,
        "stat",
        LinuxProcess::new(RamFS::new(), 3),
    )
    .unwrap();
    crate::fs::stdio::set_vt_foreground_pgrp(3, 777);
    // Another VT, with another foreground group, and it is the one on
    // screen in a default boot (VT 0).
    crate::fs::stdio::set_vt_foreground_pgrp(0, 999);
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 8), 777, "{:?}", line);
    // And the tty it names is that same VT, so the two fields agree.
    assert_eq!(field(&line, 7), (4 << 8) | 4, "{:?}", line);
}

/// Field 38 (`exit_signal`) is the signal the parent gets when the task
/// dies, and -1 on the `stat` of a thread that is not the leader. It
/// read 0, which is a `clone` that asked for no signal at all.
#[test]
fn field_38_is_the_signal_the_parent_gets_when_it_dies() {
    let proc = a_process(45_361);
    let leader = Thread::create_linux(&proc).unwrap();
    let other = Thread::create_linux(&proc).unwrap();
    let sigchld = crate::signal::Signal::SIGCHLD as i64;
    assert_eq!(field(&proc_pid_stat(&proc), 38), sigchld);
    assert_eq!(field(&proc_task_stat(&proc, Some(&leader)), 38), sigchld);
    assert_eq!(field(&proc_task_stat(&proc, Some(&other)), 38), -1);
    // `clone(CLONE_VM | SIGUSR1)`, say.
    proc.linux()
        .set_exit_signal(crate::signal::Signal::SIGUSR1 as u8);
    assert_eq!(field(&proc_pid_stat(&proc), 38), 10);
}

#[test]
fn the_children_times_are_the_reaped_children_s_cpu_in_ticks() {
    let proc = a_process(4301);
    let line = proc_pid_stat(&proc);
    assert_eq!((field(&line, 16), field(&line, 17)), (0, 0));
    // The child's CPU is what `wait4` credited: 2.5 s user and 30 ms
    // system, in USER_HZ ticks.
    proc.linux().credit_children_cpu(ChildCpu {
        utime_ns: 2_500_000_000,
        stime_ns: 30_000_000,
        ..Default::default()
    });
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 16), 250, "{:?}", line);
    assert_eq!(field(&line, 17), 3, "{:?}", line);
}

#[test]
fn starttime_is_the_process_s_birth_on_the_monotonic_clock_in_ticks() {
    let before = kernel_hal::timer::timer_now().as_nanos() as u64;
    let proc = a_process(4302);
    let after = kernel_hal::timer::timer_now().as_nanos() as u64;
    // Born between the two readings, not at 0 (the boot) and not on the
    // wall clock.
    let born = proc.linux().start_time_ns();
    assert!(
        before <= born && born <= after,
        "{} <= {} <= {}",
        before,
        born,
        after
    );
    let expected = (born / 10_000_000) as i64;
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 22), expected, "{:?}", line);
    // A process born later is born later: the value is per process, not
    // the boot or a constant.
    let later = a_process(4303);
    assert!(later.linux().start_time_ns() >= proc.linux().start_time_ns());
}

#[test]
fn vsize_is_in_bytes_and_rss_in_pages_of_what_is_mapped_and_resident() {
    let proc = a_process(4304);
    let line = proc_pid_stat(&proc);
    assert_eq!((field(&line, 23), field(&line, 24)), (0, 0), "{:?}", line);
    // Three pages mapped and written, so all three are resident.
    let vmo = VmObject::new_paged(3);
    vmo.write(0, &[1u8; 3 * PAGE_SIZE]).unwrap();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    // Explicit offsets, and not `None`: a Linux root VMAR starts at
    // `USER_ASPACE_BASE`, which is **zero** for this personality (a non-PIE
    // image has to land on the absolute vaddrs baked into its code), and
    // `find_free_area` hands out the base of an empty VMAR. So `None` here
    // means guest vaddr 0, which under libos is a host `MAP_FIXED` at 0 --
    // allowed only to a process holding `CAP_SYS_RAWIO`. The test passed
    // for anyone running it as root and failed in CI with
    // `failed to mmap: ... vaddr=0x0: EPERM`. What it measures is the size
    // and residency of what is mapped, so where it is mapped is incidental.
    const AT: usize = 0x1000_0000;
    proc.vmar()
        .map(Some(AT), vmo.clone(), 0, 3 * PAGE_SIZE, flags)
        .unwrap();
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 23), (3 * PAGE_SIZE) as i64, "{:?}", line);
    assert_eq!(field(&line, 24), 3, "{:?}", line);
    // And they agree with `statm`, which `top` reads: size and resident
    // there are both in pages.
    // Private and writable, so data too (`data_vm`).
    assert_eq!(proc_pid_statm(&proc), "3 3 0 0 0 3 0\n");
    // Mapped a second time the pages are shared, and shared pages are
    // resident too: `get_mm_rss` is file + anon + shmem, and `ps` would
    // otherwise show RSS 0 for a process that lives on shared memory.
    proc.vmar()
        .map(Some(AT + 3 * PAGE_SIZE), vmo, 0, 3 * PAGE_SIZE, flags)
        .unwrap();
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 23), (6 * PAGE_SIZE) as i64, "{:?}", line);
    assert_eq!(field(&line, 24), 6, "{:?}", line);
    assert_eq!(proc_pid_statm(&proc), "6 6 6 0 0 6 0\n");
}

#[test]
fn the_two_memory_fields_have_different_units() {
    // `do_task_stat`: `vsize` is `total_vm << PAGE_SHIFT` (bytes), `rss`
    // is `get_mm_rss` (pages). `ps` divides the one by 1024 and
    // multiplies the other by the page size.
    assert_eq!(stat_memory_fields(3 * 4096, 2 * 4096), (3 * 4096, 2));
    assert_eq!(stat_memory_fields(0, 0), (0, 0));
    assert_eq!(stat_memory_fields(4096, 4095), (4096, 0));
}

/// Field 20 is `num_threads`, and Linux never writes 0 there: a task with
/// no threads is not a task. It also has to be field 20 and not 21, which
/// is `itrealvalue` and has read 0 since 2.6 -- one column off and every
/// reader from `ps -o nlwp` down takes the wrong number.
#[test]
fn a_process_reports_at_least_one_thread_and_in_the_right_column() {
    let proc = a_process(4310);
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 20), 1, "{:?}", line);
    assert_eq!(field(&line, 21), 0, "{:?}", line);
    // With a thread of its own the count is the same one, from the list.
    let _thread = Thread::create_linux(&proc).unwrap();
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 20), 1, "{:?}", line);
    assert_eq!(field(&line, 21), 0, "{:?}", line);
    // And `status` carries it too: the two files describe the same task.
    let status = proc_pid_status(&proc);
    assert!(status.contains("\nThreads:\t1\n"), "{:?}", status);
}

/// proc(5) field 18: `20 + nice` for the fair policies and `-1 -
/// rt_priority` for the real-time ones. It is what `top` prints raw in its
/// PR column, so the offset and the sign are the whole meaning: with the
/// nice added the wrong way round the most-favoured task in the machine
/// sorts where the least-favoured should be.
#[test]
fn the_priority_field_is_twenty_plus_nice_and_minus_one_minus_rt() {
    let proc = a_process(4311);
    let thread = Thread::create_linux(&proc).unwrap();

    thread.set_sched(SCHED_NORMAL, 19, 0);
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 18), 39, "{:?}", line);
    assert_eq!(field(&line, 19), 19, "{:?}", line);

    thread.set_sched(SCHED_NORMAL, -20, 0);
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 18), 0, "{:?}", line);
    assert_eq!(field(&line, 19), -20, "{:?}", line);

    // Real time: 18 is -1 - rt, and 40/41 carry the priority and policy.
    thread.set_sched(SCHED_FIFO, 0, 30);
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 18), -31, "{:?}", line);
    assert_eq!(field(&line, 40), 30, "{:?}", line);
    assert_eq!(field(&line, 41), i64::from(SCHED_FIFO), "{:?}", line);
}

/// Fields 14 and 15 are the process's user and kernel CPU, and `terminate`
/// credits a thread's own to the process on the way out precisely so the
/// total does not drop when a worker finishes. `times(2)`, `top`'s TIME+
/// and `ps -o cputime` all read them, and CPU time that goes **backwards**
/// is what a thread pool looks like without that credit -- while the
/// process is the same process and has used every one of those cycles.
#[test]
fn the_cpu_of_a_thread_that_already_exited_is_still_the_process_s() {
    let proc = a_process(4312);
    let thread = Thread::create_linux(&proc).unwrap();
    // 2,5 s of user and 30 ms of kernel, in USER_HZ ticks.
    thread.time_add(2_500_000_000);
    thread.sys_time_add(30_000_000);
    let line = proc_pid_stat(&proc);
    assert_eq!((field(&line, 14), field(&line, 15)), (250, 3), "{:?}", line);

    // Off the list, and the numbers stay where they were.
    thread.terminate_abandoned();
    assert!(proc.thread_ids().is_empty());
    let line = proc_pid_stat(&proc);
    assert_eq!((field(&line, 14), field(&line, 15)), (250, 3), "{:?}", line);
}

/// proc(5) fields 5 and 6. Linux has no pgid or sid 0: a process that
/// never called `setpgid` leads its own group, and `getpgid` answers its
/// pid. Zero is the swapper's, so a shell reported that way looks to
/// `ps -o pgid,sid` -- and to every job-control check -- like it belongs
/// to a session that no terminal can ever own.
#[test]
fn a_process_that_never_joined_a_group_leads_its_own_group_and_session() {
    let proc = a_process(4313);
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 5), 4313, "{:?}", line);
    assert_eq!(field(&line, 6), 4313, "{:?}", line);
}

/// A process that exited and nobody has reaped is a zombie in both files.
/// `ps` shows `Z` and `<defunct>` for it; reported as running it is a
/// process that never leaves any monitor's list.
#[test]
fn a_process_that_exited_is_a_zombie_in_both_files() {
    let proc = a_process(4314);
    assert_ne!(proc_state_char(&proc), 'Z');
    proc.exit(0);
    let line = proc_pid_stat(&proc);
    let letter = line.split(") ").nth(1).and_then(|t| t.chars().next());
    assert_eq!(letter, Some('Z'), "{:?}", line);
    let status = proc_pid_status(&proc);
    assert!(status.contains("State:\tZ (zombie)\n"), "{:?}", status);
}

/// `VmSize` and `VmRSS` are in kB, and the resident one counts the shared
/// pages: `get_mm_rss` is anon + file + shmem. In pages instead of kB
/// every process looks four thousand times smaller than it is, and
/// without the shared half a process that lives on shared memory reports
/// RSS 0 -- which is what every OOM watcher and `ps --sort=-rss` reads.
#[test]
fn the_memory_lines_of_status_are_kilobytes_and_count_the_shared_pages() {
    let proc = a_process(4315);
    let vmo = VmObject::new_paged(3);
    vmo.write(0, &[1u8; 3 * PAGE_SIZE]).unwrap();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    // Explicit vaddrs: guest 0 is a host `MAP_FIXED` at 0 under libos.
    const AT: usize = 0x2000_0000;
    proc.vmar()
        .map(Some(AT), vmo.clone(), 0, 3 * PAGE_SIZE, flags)
        .unwrap();
    let status = proc_pid_status(&proc);
    assert!(status.contains("VmSize:\t      12 kB\n"), "{:?}", status);
    assert!(status.contains("VmRSS:\t      12 kB\n"), "{:?}", status);
    // Mapped a second time the pages are shared, and shared is resident.
    proc.vmar()
        .map(Some(AT + 3 * PAGE_SIZE), vmo, 0, 3 * PAGE_SIZE, flags)
        .unwrap();
    let status = proc_pid_status(&proc);
    assert!(status.contains("VmSize:\t      24 kB\n"), "{:?}", status);
    assert!(status.contains("VmRSS:\t      24 kB\n"), "{:?}", status);
}

/// `/proc/<pid>/cmdline` is the argument vector as it was handed to
/// `execve`: every argument ends in a NUL, including the last. That is
/// the whole format -- `ps`, `pgrep -f` and `/proc/<pid>/cmdline | tr
/// '\0' ' '` split on it -- and without the separators the whole vector
/// reads as one argument, so `pgrep -f "rm -rf"` matches a command line
/// that never had those two words next to each other.
#[test]
fn every_argument_of_cmdline_ends_in_a_nul_including_the_last() {
    let proc = a_process(4316);
    proc.linux().set_cmdline(alloc::vec![
        alloc::string::String::from("sh"),
        alloc::string::String::from("-c"),
        alloc::string::String::from("echo hola"),
    ]);
    assert_eq!(proc_pid_cmdline(&proc), b"sh\0-c\0echo hola\0");
    // With no argv the path is the one argument, and it is NUL-terminated
    // too: a reader that stops at the first NUL would read past the end.
    let bare = a_process(4317);
    bare.linux().set_execute_path("/bin/true");
    assert_eq!(proc_pid_cmdline(&bare), b"/bin/true\0");
}

/// The page size `/proc` divides by has to be the kernel's own: `ps`
/// multiplies field 24 of `stat` and the first two fields of `statm` back
/// up by the page size `sysconf` handed it, so a procfs dividing by
/// anything else reports every process at the wrong size. The number was
/// written twice in this file, as `PAGE` and as `PAGE_KB`, each inside
/// its own function.
#[test]
fn the_page_size_proc_reports_in_is_the_kernels_own() {
    assert_eq!(PROC_PAGE_SIZE as usize, PAGE_SIZE);
    assert_eq!(PROC_PAGE_SIZE, 4096);
}
