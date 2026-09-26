//! Minimal procfs implementation for Linux userland compatibility.

use alloc::{fmt::Write as _, string::String, sync::Arc, vec::Vec};
use core::any::Any;
use core::time::Duration;
use lazy_static::lazy_static;

use kernel_hal::drivers;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zircon_object::object::KernelObject;
use zircon_object::task::{Job, Process, Status, Thread, ThreadState, ROOT_JOB};

use crate::process::ProcessExt;
use smoltcp::wire::{IpAddress, IpCidr};

const PROC_ROOT_STATIC: [&str; 55] = [
    "net",
    "oops",
    "memhogs",
    "kheap",
    "sysvipc",
    "meminfo",
    "syscalls",
    "cmdline",
    "cpuinfo",
    "swaps",
    "uptime",
    "mounts",
    "self",
    "stat",
    "loadavg",
    "sys",
    "version",
    "perf",
    "hunter",
    "filesystems",
    "gpudbg",
    "gpustep2",
    "gpustep3",
    "gpustep4",
    "gpustep5",
    "gpustep6",
    "gpustep7",
    "gpustep8",
    "gpustep9",
    "gpustep10",
    "gpustep11",
    "gpustep12",
    "gpustep13",
    "gpustep14",
    "gpustep15",
    "gpustep16",
    "gpustep17",
    "gpustep18",
    "gpustep19",
    "gpustep20",
    "gpustep21",
    "gpustep22",
    "gpustep23",
    "gpudump",
    "gpuinit",
    "gpubench",
    "gpuedid",
    "gpusnd",
    "gpusurvive",
    "gpucefill",
    "gpucefillp2p",
    "gpuroles",
    "usbhid",
    "bootprofile",
    "kbd",
];

fn collect_processes(job: &Arc<Job>, out: &mut Vec<Arc<Process>>) {
    for id in job.process_ids() {
        if let Some(proc) = job.find_process(id) {
            if !matches!(proc.status(), Status::Exited(_)) {
                out.push(proc);
            }
        }
    }
    for child_id in job.children_ids() {
        if let Ok(child) = job.get_child(child_id) {
            if let Ok(child_job) = child.downcast_arc::<Job>() {
                collect_processes(&child_job, out);
            }
        }
    }
}

fn all_processes() -> Vec<Arc<Process>> {
    let mut out = Vec::new();
    collect_processes(&ROOT_JOB, &mut out);
    out
}

fn current_process_id() -> Option<u64> {
    let arc = kernel_hal::thread::get_current_thread()?;
    let thread = arc.downcast::<Thread>().ok()?;
    Some(thread.proc().id() as u64)
}

fn current_process() -> Option<Arc<Process>> {
    let arc = kernel_hal::thread::get_current_thread()?;
    let thread = arc.downcast::<Thread>().ok()?;
    Some(thread.proc().clone())
}

/// Whether the process making this call may read what `target` is doing.
///
/// `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)`, which in Linux guards
/// `/proc/<pid>/environ`, `maps`, `fd/` and `exe` -- everything that says what
/// another process is holding rather than merely that it exists. None of them
/// asked anything here: every process could read every other process's
/// environment (where programs are still handed passwords and tokens), its
/// address map (which is what ASLR exists to hide) and the name of every file
/// it has open.
///
/// `stat`, `status`, `statm`, `cmdline` and `comm` stay open to everybody,
/// because `ps` reads them and Linux leaves them world-readable.
///
/// With no current thread there is no caller to judge: that is the kernel
/// reading its own procfs, never userspace, so it goes through. Refusing
/// instead would make a kernel-side read of `/proc/self/exe` fail.
fn may_read_innards_of(target: &Arc<Process>) -> bool {
    let Some(caller) = current_process() else {
        return true;
    };
    if caller.id() == target.id() {
        return true;
    }
    let (Some(caller_linux), Some(target_linux)) = (caller.try_linux(), target.try_linux()) else {
        // One of the two is past teardown and has no credentials left to
        // compare. `effective_pgid` tolerates the same race.
        return true;
    };
    crate::process::LinuxProcess::may_read_process_innards(
        &caller_linux.credentials(),
        &target_linux.credentials(),
        false,
    )
}

fn sanitize_comm(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let mut s = String::new();
    for c in base.chars().take(15) {
        let ch = match c {
            '(' | ')' | '\0' => '_',
            _ => c,
        };
        s.push(ch);
    }
    if s.is_empty() {
        s.push_str("process");
    }
    s
}

/// The state letter for `/proc/<pid>/stat` and `/proc/<pid>/status`.
///
/// Linux reports the thread-group leader's scheduling state, and the
/// distinction that matters is running versus asleep. The process-level
/// [`Status`] cannot express it — it is `Running` for every live process — so
/// this asks the leader thread, which the run loop now marks blocked whenever
/// its syscall future parks.
fn proc_state_char(proc: &Process) -> char {
    if let Status::Exited(_) = proc.status() {
        return 'Z';
    }
    match proc_first_thread(proc).map(|t| t.state()) {
        // Suspended by `zx_task_suspend` — Linux's "stopped by a signal".
        Some(ThreadState::Suspended) => 'T',
        Some(ThreadState::Dying) | Some(ThreadState::Dead) => 'Z',
        // Every blocked flavour (syscall, sleep, futex, port, channel,
        // exception) is an interruptible sleep here; nothing in this kernel
        // waits uninterruptibly, so none of them is Linux's `D`.
        Some(s) if (s as u32) & 0xff == ThreadState::Blocked as u32 => 'S',
        Some(ThreadState::New) | Some(ThreadState::Running) => 'R',
        // No leader thread yet: created, not started.
        _ => 'S',
    }
}

/// `/proc/<pid>/threads`: one line per thread — tid, state, name, and the
/// syscall it is in.
///
/// Linux answers this with `/proc/<pid>/task/<tid>/{comm,status,syscall}`, a
/// directory per thread; this is the same information in one read, which is
/// what a hang needs. A process asleep as a whole says nothing about WHY: a
/// browser parked in `epoll_wait` on its IPC thread and one stuck in a futex
/// held by a dead peer look identical from `/proc/<pid>/status`. Per thread,
/// with the name userspace gave it through `prctl(PR_SET_NAME)`, they do not.
///
/// Syscall numbers are the raw x86_64 ones (`asm/unistd_64.h`); `-` means the
/// thread is in user code.
fn proc_pid_threads(proc: &Process) -> String {
    use crate::thread::ThreadExt;
    use core::fmt::Write;
    let mut out = String::from("# tid state name syscall\n");
    for tid in proc.thread_ids() {
        let Ok(obj) = proc.get_child(tid) else {
            continue;
        };
        let Ok(thread) = obj.downcast_arc::<Thread>() else {
            continue;
        };
        let state = match thread.state() {
            ThreadState::Suspended => 'T',
            ThreadState::Dying | ThreadState::Dead => 'Z',
            s if (s as u32) & 0xff == ThreadState::Blocked as u32 => 'S',
            _ => 'R',
        };
        // try_lock: a thread tearing down may hold its own lock, and a /proc
        // read must never block on it.
        let name = thread
            .try_lock_linux()
            .map(|lt| lt.comm.clone())
            .filter(|c| !c.is_empty())
            .map(|c| sanitize_comm(&c))
            .unwrap_or_else(|| String::from("-"));
        match thread.current_syscall() {
            Some(num) => {
                let _ = writeln!(out, "{} {} {} {}", tid, state, name, num);
            }
            None => {
                let _ = writeln!(out, "{} {} {} -", tid, state, name);
            }
        }
    }
    out
}

fn proc_comm(proc: &Process) -> String {
    // A name set through `prctl(PR_SET_NAME)` on the leader thread wins — that
    // is what Linux reports in `/proc/<pid>/comm` — and threads that never set
    // one fall back to the executable's basename below.
    if let Some(thread) = proc_first_thread(proc) {
        use crate::thread::ThreadExt;
        if let Some(lt) = thread.try_lock_linux() {
            if !lt.comm.is_empty() {
                let comm = lt.comm.clone();
                drop(lt);
                return sanitize_comm(&comm);
            }
        }
    }
    // try_linux: /proc readers run on processes looked up by pid, which may be
    // tearing down concurrently. Fall back to the kobject name on a miss rather
    // than panicking the reader.
    let path = proc
        .try_linux()
        .map(|lp| lp.execute_path())
        .unwrap_or_default();
    if !path.is_empty() {
        let base = path.rsplit('/').next().unwrap_or(&path);
        return sanitize_comm(base);
    }
    sanitize_comm(&proc.name())
}

fn proc_ppid(proc: &Process) -> u64 {
    proc.try_linux()
        .and_then(|lp| lp.parent())
        .map(|p| p.id())
        .unwrap_or(0)
}

/// The first (leader) thread of a process, if any, for reporting its
/// scheduling attributes.
fn proc_first_thread(proc: &Process) -> Option<Arc<Thread>> {
    let id = *proc.thread_ids().first()?;
    proc.get_child(id).ok()?.downcast_arc::<Thread>().ok()
}

fn proc_pid_stat(proc: &Process) -> String {
    let pid = proc.id();
    let comm = proc_comm(proc);
    let state = proc_state_char(proc);
    let ppid = proc_ppid(proc);

    let nthreads = proc.thread_ids().len().max(1) as i64;
    // priority(18)/nice(19)/rt_priority(40)/policy(41) come from the leader
    // thread. Per proc(5): for real-time policies the priority field is
    // `-1 - rt_priority`; for the fair policies it is `20 + nice`.
    let first_thread = proc_first_thread(proc);
    let (priority, nice, rt_priority, policy) = match first_thread.as_ref() {
        Some(t) => {
            let rt = t.sched_rt_priority() as i64;
            let n = t.sched_nice() as i64;
            let prio = if t.sched_is_realtime() {
                -1 - rt
            } else {
                20 + n
            };
            (prio, n, rt, t.sched_policy() as i64)
        }
        None => (20, 0, 0, 0),
    };

    // Fields 14/15 (utime/stime): USER_HZ (100 Hz) ticks of user-/kernel-mode
    // CPU time, live threads plus whatever `dead_threads_(sys_)time` already
    // credited from threads that already exited. Both accumulators are real
    // measured CPU time (see `ThreadSwitchFuture::poll` in zircon-object) —
    // not wall-clock time a blocked/waiting thread happened to sit for.
    const NS_PER_TICK: u64 = 10_000_000; // 1e7 ns = 10 ms = 1 / USER_HZ(100)
    let mut utime_ns = proc.dead_threads_time();
    let mut stime_ns = proc.dead_threads_sys_time();
    for tid in proc.thread_ids() {
        if let Ok(child) = proc.get_child(tid) {
            if let Ok(t) = child.downcast_arc::<Thread>() {
                utime_ns += t.get_time();
                stime_ns += t.get_sys_time();
            }
        }
    }
    let utime = (utime_ns / NS_PER_TICK) as i64;
    let stime = (stime_ns / NS_PER_TICK) as i64;

    // Fields 16/17 (cutime/cstime), 22 (starttime), 23 (vsize) and 24 (rss):
    // `do_task_stat` publishes the reaped children's times, the task's
    // `start_boottime` in clock ticks, `mm->total_vm` in BYTES and
    // `get_mm_rss` in PAGES. All five read 0 here while `status`, `statm`,
    // `getrusage(RUSAGE_CHILDREN)` and `times()` had the numbers, so `ps
    // aux` showed VSZ 0 and RSS 0 for every process, `ps -o etime` the age
    // of the machine, and `top`'s TIME+ never counted a finished child.
    let (cutime_ns, cstime_ns) = proc
        .try_linux()
        .map(|lp| lp.children_cpu_ns())
        .unwrap_or((0, 0));
    let cutime = (cutime_ns / NS_PER_TICK) as i64;
    let cstime = (cstime_ns / NS_PER_TICK) as i64;
    let starttime = proc
        .try_linux()
        .map(|lp| (lp.start_time_ns() / NS_PER_TICK) as i64)
        .unwrap_or(0);
    let stats = proc.vmar().get_task_stats();
    let (vsize, rss) = stat_memory_fields(
        stats.mapped_bytes(),
        stats.private_bytes() + stats.shared_bytes(),
    );

    // Field 39 (processor): the CPU the leader thread last ran on.
    let processor = first_thread
        .as_ref()
        .map(|t| t.last_cpu() as i64)
        .unwrap_or(0);

    // Job-control ids (proc(5) fields 5-8): the effective pgid/sid resolve the
    // "0 = own pid" convention, tty_nr encodes the per-process VT console as
    // major 4 (TTY_MAJOR) + minor, and tpgid is the tty's foreground group.
    let pgrp = crate::process::get_process_pgid(pid).unwrap_or(pid) as i64;
    let session = crate::process::get_process_sid(pid).unwrap_or(pid) as i64;
    let tty_nr = proc
        .try_linux()
        .map(|lp| (4 << 8) | (lp.vt() as i64 + 1))
        .unwrap_or(0);
    let tpgid = crate::fs::stdio::get_foreground_pgrp() as i64;

    // Fields 5..=52 of /proc/[pid]/stat (proc(5)); 0 where not tracked. Indexed
    // by `field - 5` to keep the field numbers obvious.
    let mut rest = [0i64; 48];
    rest[0] = pgrp; // field 5 (pgrp): first entry of the 5-indexed tail
    rest[6 - 5] = session;
    rest[7 - 5] = tty_nr;
    rest[8 - 5] = tpgid;
    rest[14 - 5] = utime;
    rest[15 - 5] = stime;
    rest[16 - 5] = cutime;
    rest[17 - 5] = cstime;
    rest[18 - 5] = priority;
    rest[19 - 5] = nice;
    rest[20 - 5] = nthreads;
    rest[22 - 5] = starttime;
    rest[23 - 5] = vsize;
    rest[24 - 5] = rss;
    rest[39 - 5] = processor;
    rest[40 - 5] = rt_priority;
    rest[41 - 5] = policy;

    let mut out = format!("{} ({}) {} {}", pid, comm, state, ppid);
    for v in rest.iter() {
        let _ = write!(out, " {}", v);
    }
    out.push('\n');
    out
}

/// Fields 23 and 24 of `/proc/<pid>/stat` from the task's memory totals:
/// `vsize` is `mm->total_vm << PAGE_SHIFT`, in BYTES, and `rss` is
/// `get_mm_rss(mm)`, in PAGES (`do_task_stat`, `fs/proc/array.c`). The two
/// units differ, and `ps` divides the first by 1024 and multiplies the
/// second by the page size, so a byte count in `rss` would show every
/// process at four thousand times its size.
fn stat_memory_fields(mapped_bytes: u64, resident_bytes: u64) -> (i64, i64) {
    const PAGE: u64 = 4096;
    (mapped_bytes as i64, (resident_bytes / PAGE) as i64)
}

fn proc_pid_status(proc: &Process) -> String {
    let pid = proc.id();
    let name = proc_comm(proc);
    let ppid = proc_ppid(proc);
    let state = match proc_state_char(proc) {
        'R' => "R (running)",
        'T' => "T (stopped)",
        'Z' => "Z (zombie)",
        _ => "S (sleeping)",
    };
    // VmSize = total mapped address space; VmRSS = committed (resident)
    // bytes, private + shared — the fields `ps`/`top`/OOM-watchers read.
    let stats = proc.vmar().get_task_stats();
    let vm_size_kb = stats.mapped_bytes() / 1024;
    let vm_rss_kb = (stats.private_bytes() + stats.shared_bytes()) / 1024;
    let threads = proc.thread_ids().len().max(1);
    // The ids are the process's own, not a constant: `polkit`, `sudo`,
    // `ps -u` and `pkexec` read the `Uid:` line to decide who is asking.
    let ids = proc
        .try_linux()
        .map(|lp| credential_lines(&lp.credentials()))
        .unwrap_or_default();
    format!(
        "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\n{}VmSize:\t{:8} kB\nVmRSS:\t{:8} kB\nThreads:\t{}\n",
        name, state, pid, pid, ppid, ids, vm_size_kb, vm_rss_kb, threads
    )
}

/// The `Uid:`, `Gid:` and `Groups:` lines of `/proc/<pid>/status`, in the
/// order proc(5) gives them: real, effective, saved, filesystem. `Groups:`
/// follows `task_state()` in `fs/proc/array.c`: each id followed by a space,
/// then the newline.
fn credential_lines(creds: &crate::process::Credentials) -> String {
    let mut out = format!(
        "Uid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nGroups:\t",
        creds.ruid,
        creds.euid,
        creds.suid,
        creds.fsuid,
        creds.rgid,
        creds.egid,
        creds.sgid,
        creds.fsgid
    );
    for group in &creds.groups {
        let _ = write!(out, "{} ", group);
    }
    out.push('\n');
    out
}

/// `/proc/<pid>/statm`: memory usage in PAGES — "size resident shared text
/// lib data dt" per proc.rst. text/lib/data/dt read zero (Linux itself zeroes
/// lib and dt); `ps` computes RSS from field 2.
fn proc_pid_statm(proc: &Process) -> String {
    const PAGE_KB: u64 = 4096;
    let stats = proc.vmar().get_task_stats();
    let size = stats.mapped_bytes() / PAGE_KB;
    let shared = stats.shared_bytes() / PAGE_KB;
    let resident = stats.private_bytes() / PAGE_KB + shared;
    alloc::format!("{} {} {} 0 0 0 0\n", size, resident, shared)
}

fn proc_pid_cmdline(proc: &Process) -> Vec<u8> {
    let lp = match proc.try_linux() {
        Some(lp) => lp,
        None => return Vec::new(),
    };
    let args = lp.cmdline();
    if !args.is_empty() {
        let mut out = Vec::new();
        for arg in args {
            out.extend_from_slice(arg.as_bytes());
            out.push(0);
        }
        return out;
    }
    let path = lp.execute_path();
    let mut out = path.into_bytes();
    out.push(0);
    out
}

/// A minimal `procfs` with a few common files.
pub struct ProcFS;

impl ProcFS {
    /// Create a new procfs instance.
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for ProcFS {
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        PROC_ROOT.clone()
    }

    fn info(&self) -> FsInfo {
        // Virtual FS: report conservative, non-zero values.
        FsInfo {
            bsize: 4096,
            frsize: 4096,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 255,
        }
    }
}

struct ProcRootINode;

impl ProcRootINode {
    fn entry_name(id: usize) -> Result<String> {
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i if i - 2 < PROC_ROOT_STATIC.len() => Ok(PROC_ROOT_STATIC[i - 2].into()),
            i => {
                let idx = i - 2 - PROC_ROOT_STATIC.len();
                let procs = all_processes();
                if idx >= procs.len() {
                    return Err(FsError::EntryNotFound);
                }
                Ok(alloc::format!("{}", procs[idx].id()))
            }
        }
    }
}

impl INode for ProcRootINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        // `net`, `sysvipc`, `sys` and `perf`, plus a `/proc/<pid>` per live
        // process -- which is why Linux's `/proc` nlink moves as processes come
        // and go (`proc_root_getattr`: `nlink + nr_processes()`).
        Ok(dir_metadata(10, 4 + all_processes().len()))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." | ".." => Ok(PROC_ROOT.clone()),
            "net" => Ok(PROC_NET_DIR.clone()),
            "sysvipc" => Ok(PROC_SYSVIPC_DIR.clone()),
            "meminfo" => Ok(PROC_MEMINFO.clone()),
            "syscalls" => Ok(PROC_SYSCALLS.clone()),
            "cmdline" => Ok(PROC_CMDLINE.clone()),
            "memhogs" => Ok(PROC_MEMHOGS.clone()),
            "kheap" => Ok(PROC_KHEAP.clone()),
            "cpuinfo" => Ok(PROC_CPUINFO.clone()),
            "swaps" => Ok(PROC_SWAPS.clone()),
            "uptime" => Ok(PROC_UPTIME.clone()),
            "mounts" => Ok(PROC_MOUNTS.clone()),
            "stat" => Ok(PROC_STAT.clone()),
            "loadavg" => Ok(PROC_LOADAVG.clone()),
            "sys" => Ok(PROC_SYS_DIR.clone()),
            "version" => Ok(PROC_VERSION.clone()),
            "perf" => Ok(PROC_PERF_DIR.clone()),
            "hunter" => Ok(PROC_HUNTER.clone()),
            "filesystems" => Ok(PROC_FILESYSTEMS.clone()),
            "gpudbg" => Ok(PROC_GPUDBG.clone()),
            "oops" => Ok(PROC_OOPS.clone()),
            "gpustep2" => Ok(PROC_GPUSTEP2.clone()),
            "gpustep3" => Ok(PROC_GPUSTEP3.clone()),
            "gpustep4" => Ok(PROC_GPUSTEP4.clone()),
            "gpustep5" => Ok(PROC_GPUSTEP5.clone()),
            "gpustep6" => Ok(PROC_GPUSTEP6.clone()),
            "gpustep7" => Ok(PROC_GPUSTEP7.clone()),
            "gpustep8" => Ok(PROC_GPUSTEP8.clone()),
            "gpustep9" => Ok(PROC_GPUSTEP9.clone()),
            "gpustep10" => Ok(PROC_GPUSTEP10.clone()),
            "gpustep11" => Ok(PROC_GPUSTEP11.clone()),
            "gpustep12" => Ok(PROC_GPUSTEP12.clone()),
            "gpustep13" => Ok(PROC_GPUSTEP13.clone()),
            "gpustep14" => Ok(PROC_GPUSTEP14.clone()),
            "gpustep15" => Ok(PROC_GPUSTEP15.clone()),
            "gpustep16" => Ok(PROC_GPUSTEP16.clone()),
            "gpustep17" => Ok(PROC_GPUSTEP17.clone()),
            "gpustep18" => Ok(PROC_GPUSTEP18.clone()),
            "gpustep19" => Ok(PROC_GPUSTEP19.clone()),
            "gpustep20" => Ok(PROC_GPUSTEP20.clone()),
            "gpustep21" => Ok(PROC_GPUSTEP21.clone()),
            "gpustep22" => Ok(PROC_GPUSTEP22.clone()),
            "gpustep23" => Ok(PROC_GPUSTEP23.clone()),
            "gpuinit" => Ok(PROC_GPUINIT.clone()),
            "gpubench" => Ok(PROC_GPUBENCH.clone()),
            "gpuedid" => Ok(PROC_GPUEDID.clone()),
            "gpusnd" => Ok(PROC_GPUSND.clone()),
            "gpusurvive" => Ok(PROC_GPUSURVIVE.clone()),
            "gpucefill" => Ok(PROC_GPUCEFILL.clone()),
            "gpucefillp2p" => Ok(PROC_GPUCEFILLP2P.clone()),
            "gpuroles" => Ok(PROC_GPUROLES.clone()),
            "usbhid" => Ok(PROC_USBHID.clone()),
            "gpudump" => Ok(PROC_GPUDUMP.clone()),
            "bootprofile" => Ok(PROC_BOOTPROFILE.clone()),
            "kbd" => Ok(PROC_KBD.clone()),
            "self" => Ok(PROC_SELF_SYM.clone()),
            name => {
                if let Ok(pid) = name.parse::<u64>() {
                    if ROOT_JOB.find_process(pid as _).is_some() {
                        return Ok(Arc::new(ProcPidDirINode { pid }));
                    }
                }
                Err(FsError::EntryNotFound)
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        Self::entry_name(id)
    }
}

/// Inode numbers of `/proc/<pid>` and everything under it.
///
/// Linux gives every one of those a number of its own (`proc_pid_make_inode`
/// takes a fresh one per entry). Here the directory was `100 + pid`, every
/// file under it `200 + pid` whatever the file, and `fd` `40 + pid`, all on
/// device 0 with the fixed files of `/proc` living in 10..=999: `/proc/1`
/// was the same (dev, ino) as a fixed file, `/proc/61/fd` as `/proc/1`, and
/// `/proc/<pid>/stat`, `status`, `cmdline`, `environ`... were one inode to
/// every tool that compares files by identity (`cmp`, `diff`, `cp`, `tar`,
/// `[ a -ef b ]`), which then takes them for the same file without reading.
///
/// The numbers start above every fixed one and leave `PID_INODE_SLOTS` per
/// process: slot 0 is the directory, the rest its entries.
pub(crate) const PID_INODE_BASE: usize = 0x1_0000;
pub(crate) const PID_INODE_SLOTS: usize = 16;
/// Slot of `/proc/<pid>/fd` (`proc_self.rs`); the files use their kind's.
pub(crate) const PID_FD_DIR_SLOT: usize = 10;

pub(crate) fn pid_inode(pid: u64, slot: usize) -> usize {
    debug_assert!(slot < PID_INODE_SLOTS);
    PID_INODE_BASE + (pid as usize) * PID_INODE_SLOTS + slot
}

/// `/proc/<pid>/` — `stat`, `cmdline`, `status` for BusyBox `ps`.
struct ProcPidDirINode {
    pid: u64,
}

impl ProcPidDirINode {
    fn process(&self) -> Option<Arc<Process>> {
        ROOT_JOB.find_process(self.pid as _)
    }

    /// `EACCES` unless the caller may read this process's innards.
    ///
    /// Answered at lookup, which is where Linux answers it too (`open` on the
    /// file, `readlink` on the symlink): the name stays in the directory
    /// listing, so this is "you may not read that", not "there is no such
    /// file".
    fn check_read_innards(&self) -> Result<()> {
        let proc = self.process().ok_or(FsError::EntryNotFound)?;
        if may_read_innards_of(&proc) {
            Ok(())
        } else {
            Err(FsError::NoPermission)
        }
    }

    fn entries() -> [&'static str; 13] {
        [
            ".", "..", "stat", "cmdline", "status", "perf", "maps", "fd", "comm", "environ",
            "statm", "exe", "threads",
        ]
    }
}

impl INode for ProcPidDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: pid_inode(self.pid, 0),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o555,
            // `.`, the name in `/proc`, and `fd`'s `..`.
            nlinks: 3,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        if self.process().is_none() {
            return Err(FsError::EntryNotFound);
        }
        match name {
            "." => Ok(Arc::new(ProcPidDirINode { pid: self.pid })),
            ".." => Ok(PROC_ROOT.clone()),
            "stat" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Stat,
            })),
            "cmdline" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Cmdline,
            })),
            "status" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Status,
            })),
            "perf" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Perf,
            })),
            "maps" => {
                self.check_read_innards()?;
                Ok(Arc::new(ProcPidFileINode {
                    pid: self.pid,
                    kind: ProcPidFileKind::Maps,
                }))
            }
            // Reuse the /proc/self/fd directory for any pid: it resolves the
            // fd table of the process it is handed at lookup time.
            "fd" => {
                self.check_read_innards()?;
                Ok(Arc::new(super::proc_self::ProcSelfFdDir {
                    process: self.process().ok_or(FsError::EntryNotFound)?,
                }))
            }
            "comm" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Comm,
            })),
            "environ" => {
                self.check_read_innards()?;
                Ok(Arc::new(ProcPidFileINode {
                    pid: self.pid,
                    kind: ProcPidFileKind::Environ,
                }))
            }
            "statm" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Statm,
            })),
            "threads" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Threads,
            })),
            "exe" => {
                self.check_read_innards()?;
                let proc = self.process().ok_or(FsError::EntryNotFound)?;
                let path = proc
                    .try_linux()
                    .map(|lp| lp.execute_path())
                    .unwrap_or_default();
                Ok(Arc::new(super::pseudo::Pseudo::new(
                    &path,
                    FileType::SymLink,
                )))
            }
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
    }
}

struct ProcNetDirINode;

impl ProcNetDirINode {
    /// `.` and `..` first, as `readdir` gives them and as every other
    /// directory here lists them. This was the one that left them out, so
    /// `ls -a /proc/net` showed neither and a walker that counts on finding
    /// them -- `fts(3)` uses `..` to climb back out without re-resolving the
    /// path -- got a directory shaped like nothing else in the tree. `find`
    /// answered them from `find()`, which is why a lookup by name worked and
    /// only the listing was short.
    fn entries() -> [&'static str; 9] {
        [
            ".", "..", "dev", "route", "arp", "if_inet6", "tcp", "udp", "unix",
        ]
    }
}

impl INode for ProcNetDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(20, 0))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_NET_DIR.clone()),
            ".." => Ok(PROC_ROOT.clone()),
            "dev" => Ok(PROC_NET_DEV.clone()),
            "route" => Ok(PROC_NET_ROUTE.clone()),
            "arp" => Ok(PROC_NET_ARP.clone()),
            "if_inet6" => Ok(PROC_NET_IF_INET6.clone()),
            "tcp" => Ok(PROC_NET_TCP.clone()),
            "udp" => Ok(PROC_NET_UDP.clone()),
            "unix" => Ok(PROC_NET_UNIX.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
    }
}

/// `/proc/sysvipc` — per-mechanism System V IPC tables
/// (Documentation/filesystems/proc.rst): `msg`, `sem` and `shm` are what
/// `ipcs -q`, `ipcs -s` and `ipcs -m` read. Only `msg` was here, so `ipcs`
/// (and `ipcrm -a`) saw no semaphore set and no shared segment at all.
struct ProcSysvipcDirINode;

impl ProcSysvipcDirINode {
    fn entries() -> [&'static str; 5] {
        [".", "..", "msg", "sem", "shm"]
    }
}

impl INode for ProcSysvipcDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(21, 0))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYSVIPC_DIR.clone()),
            ".." => Ok(PROC_ROOT.clone()),
            "msg" => Ok(PROC_SYSVIPC_MSG.clone()),
            "sem" => Ok(PROC_SYSVIPC_SEM.clone()),
            "shm" => Ok(PROC_SYSVIPC_SHM.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
    }
}

/// `/proc/sys` — the sysctl tree: `kernel/`, `vm/` and `fs/` subtrees carrying
/// the knobs real userspace reads (Documentation/admin-guide/sysctl/).
struct ProcSysDirINode;

impl INode for ProcSysDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(40, 3))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYS_DIR.clone()),
            ".." => Ok(PROC_ROOT.clone()),
            "kernel" => Ok(PROC_SYS_KERNEL_DIR.clone()),
            "vm" => Ok(PROC_SYS_VM_DIR.clone()),
            "fs" => Ok(PROC_SYS_FS_DIR.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("kernel".into()),
            3 => Ok("vm".into()),
            4 => Ok("fs".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

/// `/proc/sys/kernel` — identity strings (shared with `uname(2)` via
/// `crate::uname` so the views agree), process-table limits, the `random/`
/// subtree, and the knobs `perf` probes before profiling.
struct ProcSysKernelDirINode;

impl INode for ProcSysKernelDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(41, 1))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYS_KERNEL_DIR.clone()),
            ".." => Ok(PROC_SYS_DIR.clone()),
            // -1 = no restrictions: let `perf` open kernel/CPU-wide events.
            "perf_event_paranoid" => Ok(PROC_PERF_PARANOID.clone()),
            "kptr_restrict" => Ok(PROC_KPTR_RESTRICT.clone()),
            "hostname" => Ok(PROC_SYS_HOSTNAME.clone()),
            "domainname" => Ok(PROC_SYS_DOMAINNAME.clone()),
            "ostype" => Ok(PROC_SYS_OSTYPE.clone()),
            "osrelease" => Ok(PROC_SYS_OSRELEASE.clone()),
            "version" => Ok(PROC_SYS_VERSION.clone()),
            "pid_max" => Ok(PROC_SYS_PID_MAX.clone()),
            "ngroups_max" => Ok(PROC_SYS_NGROUPS_MAX.clone()),
            "threads-max" => Ok(PROC_SYS_THREADS_MAX.clone()),
            "overflowuid" => Ok(PROC_SYS_OVERFLOWUID.clone()),
            "overflowgid" => Ok(PROC_SYS_OVERFLOWGID.clone()),
            "random" => Ok(PROC_SYS_KERNEL_RANDOM_DIR.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("perf_event_paranoid".into()),
            3 => Ok("kptr_restrict".into()),
            4 => Ok("hostname".into()),
            5 => Ok("domainname".into()),
            6 => Ok("ostype".into()),
            7 => Ok("osrelease".into()),
            8 => Ok("version".into()),
            9 => Ok("pid_max".into()),
            10 => Ok("ngroups_max".into()),
            11 => Ok("threads-max".into()),
            12 => Ok("overflowuid".into()),
            13 => Ok("overflowgid".into()),
            14 => Ok("random".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

/// `/proc/sys/kernel/random` — `boot_id` (stable for this boot; D-Bus and
/// systemd-alikes use it to tell boots apart) and `uuid` (fresh per read).
struct ProcSysKernelRandomDirINode;

impl INode for ProcSysKernelRandomDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(59, 0))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYS_KERNEL_RANDOM_DIR.clone()),
            ".." => Ok(PROC_SYS_KERNEL_DIR.clone()),
            "boot_id" => Ok(PROC_SYS_BOOT_ID.clone()),
            "uuid" => Ok(PROC_SYS_UUID.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("boot_id".into()),
            3 => Ok("uuid".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

/// `/proc/sys/vm` — memory-management sysctls allocators and daemons consult.
struct ProcSysVmDirINode;

impl INode for ProcSysVmDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(62, 0))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYS_VM_DIR.clone()),
            ".." => Ok(PROC_SYS_DIR.clone()),
            "overcommit_memory" => Ok(PROC_SYS_OVERCOMMIT.clone()),
            "max_map_count" => Ok(PROC_SYS_MAX_MAP_COUNT.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("overcommit_memory".into()),
            3 => Ok("max_map_count".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

/// `/proc/sys/fs` — file-table limits (`ulimit`, daemons sizing fd pools).
struct ProcSysFsDirINode;

impl INode for ProcSysFsDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(65, 0))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYS_FS_DIR.clone()),
            ".." => Ok(PROC_SYS_DIR.clone()),
            "file-max" => Ok(PROC_SYS_FILE_MAX.clone()),
            "nr_open" => Ok(PROC_SYS_NR_OPEN.clone()),
            "pipe-max-size" => Ok(PROC_SYS_PIPE_MAX_SIZE.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("file-max".into()),
            3 => Ok("nr_open".into()),
            4 => Ok("pipe-max-size".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

/// The metadata of a `/proc` directory that holds `subdirs` directories of its
/// own.
///
/// `nlinks` was `0`, and a live directory with no links is not a thing a
/// filesystem may report. Two tools act on the number rather than showing it:
/// `stat` prints "Links: 0", which reads as a directory that has been removed;
/// and everything built on `fts(3)` -- `find`, `du`, `chmod -R`, `cp -r`,
/// `rm -r`, `rsync` -- subtracts 2 from it to learn how many subdirectories are
/// left to visit, and stops descending when the count runs out. That is what
/// `find -noleaf` exists to switch off, and its man page names "filesystems
/// that do not follow the Unix link convention" as the reason. So a directory
/// has to report `2 + subdirs`: one link for its own `.`, one for its name in
/// the parent, and one per child directory's `..`.
fn dir_metadata(inode: usize, subdirs: usize) -> Metadata {
    Metadata {
        dev: 0,
        inode,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::Dir,
        mode: 0o555,
        nlinks: 2 + subdirs,
        uid: 0,
        gid: 0,
        rdev: 0,
    }
}

fn proc_perf_event_paranoid_content() -> String {
    String::from("-1\n")
}

fn proc_perf_syscalls_content() -> String {
    crate::perf::global_report()
}

fn proc_perf_top_content() -> String {
    crate::perf::top_report()
}

fn proc_perf_kernel_content() -> String {
    crate::perf::kernel_report()
}

fn proc_perf_tasks_content() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "eclipse perf — tasks (processes / kernel threads)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {:>5} {:>4} {:<16} {:<2} {:>10} {:>12}",
        "PID", "THR", "NAME", "ST", "SYSCALLS", "TIME ms"
    );
    let mut procs = all_processes();
    procs.sort_by_key(|p| p.id());
    for proc in procs {
        let pid = proc.id();
        let comm = proc_comm(&proc);
        let state = proc_state_char(&proc);
        let nthr = proc.thread_ids().len().max(1);
        let (calls, ns) = proc
            .try_linux()
            .map(|lp| lp.perf().totals())
            .unwrap_or((0, 0));
        let _ = writeln!(
            out,
            "  {:>5} {:>4} {:<16} {:<2} {:>10} {:>12.3}",
            pid,
            nthr,
            comm,
            state,
            calls,
            ns as f64 / 1_000_000.0
        );
    }
    out
}

/// `/proc/perf` — a directory holding Eclipse's own observability views.
struct ProcPerfDirINode;

impl INode for ProcPerfDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(45, 0))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_PERF_DIR.clone()),
            ".." => Ok(PROC_ROOT.clone()),
            "syscalls" => Ok(PROC_PERF_SYSCALLS.clone()),
            "tasks" => Ok(PROC_PERF_TASKS.clone()),
            "top" => Ok(PROC_PERF_TOP.clone()),
            "kernel" => Ok(PROC_PERF_KERNEL.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("syscalls".into()),
            3 => Ok("tasks".into()),
            4 => Ok("top".into()),
            5 => Ok("kernel".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

/// `/proc/bootprofile` — the boot-time file-access recorder. Empty unless
/// `BOOTTRACE=<comm>` was on the kernel command line; then it holds the traced
/// process's open timeline plus a deduplicated preload list. See
/// `crate::boot_trace`.
fn proc_bootprofile_content() -> String {
    crate::boot_trace::render()
}

fn proc_kptr_restrict_content() -> String {
    String::from("0\n")
}

fn proc_version_content() -> String {
    crate::uname::proc_version()
}

/// `/proc/cmdline` — the kernel boot command line, newline-terminated like
/// Linux. This is how userspace (e.g. eclipse-init picking the desktop session
/// from a `desktop=xorg`/`desktop=labwc` boot argument) reads boot options.
fn proc_cmdline_content() -> String {
    let mut s = kernel_hal::boot::cmdline();
    s.push('\n');
    s
}

fn proc_sys_hostname_content() -> String {
    alloc::format!("{}\n", crate::uname::hostname())
}

fn proc_sys_domainname_content() -> String {
    alloc::format!("{}\n", crate::uname::domainname())
}

fn proc_sys_ostype_content() -> String {
    alloc::format!("{}\n", crate::uname::OS_TYPE)
}

fn proc_sys_osrelease_content() -> String {
    alloc::format!("{}\n", crate::uname::OS_RELEASE)
}

fn proc_sys_version_content() -> String {
    alloc::format!("{}\n", crate::uname::os_version())
}

/// Highest pid value + 1; 64-bit Linux default ceiling. `ps`, container
/// runtimes and some allocators size tables from it.
fn proc_sys_pid_max_content() -> String {
    String::from("4194304\n")
}

/// NGROUPS_MAX, constant on Linux since 2.6.4.
fn proc_sys_ngroups_max_content() -> String {
    String::from("65536\n")
}

fn proc_sys_threads_max_content() -> String {
    String::from("65536\n")
}

/// The UID/GID substituted for one that does not fit the caller's view of a
/// filesystem or a user namespace. Linux has had these since 2.4 and never
/// varies the default; `nobody` (65534) is the value every distro ships.
///
/// Not cosmetic: **bubblewrap reads `/proc/sys/kernel/overflowuid` before it
/// does anything else** and dies outright if it cannot:
///
///   bwrap: Can't read /proc/sys/kernel/overflowuid: No such file or directory
///
/// That killed the whole XFCE session by a long chain — Alpine's gdk-pixbuf
/// decodes every image format through out-of-process glycin loaders, glycin
/// runs each loader under bwrap, so no bwrap meant no PNG decoding, and
/// libwnck `g_assert`s on the NULL pixbuf instead of degrading.
fn proc_sys_overflowuid_content() -> String {
    String::from("65534\n")
}

fn proc_sys_overflowgid_content() -> String {
    String::from("65534\n")
}

/// Format 16 random bytes as an RFC 4122 version-4 UUID string.
fn format_uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 1
    alloc::format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

lazy_static! {
    /// One UUID for this boot, generated on first read. D-Bus (and everything
    /// modelled on it) reads `/proc/sys/kernel/random/boot_id` to build its
    /// per-boot machine identity, and expects the value to be stable until
    /// reboot.
    static ref BOOT_ID: String = {
        let mut bytes = [0u8; 16];
        kernel_hal::rand::fill_random(&mut bytes);
        format_uuid_v4(bytes)
    };
}

fn proc_sys_boot_id_content() -> String {
    alloc::format!("{}\n", *BOOT_ID)
}

fn proc_sys_uuid_content() -> String {
    let mut bytes = [0u8; 16];
    kernel_hal::rand::fill_random(&mut bytes);
    alloc::format!("{}\n", format_uuid_v4(bytes))
}

/// 0 = heuristic overcommit, the Linux default. Anonymous mappings here are
/// demand-paged and never charged up front, which is what 0 describes.
fn proc_sys_overcommit_content() -> String {
    String::from("0\n")
}

/// `/proc/sysvipc/msg`: the live System V message-queue table.
fn proc_sysvipc_msg_content() -> String {
    crate::ipc::msg_proc_table()
}

/// `/proc/sysvipc/sem`: the live System V semaphore-set table.
fn proc_sysvipc_sem_content() -> String {
    crate::ipc::sem_proc_table()
}

/// `/proc/sysvipc/shm`: the live System V shared-memory table.
fn proc_sysvipc_shm_content() -> String {
    crate::ipc::shm_proc_table()
}

/// Linux's default vm.max_map_count. Address-space-hungry runtimes (JVMs,
/// wasm engines) read it to size their reservation strategy.
fn proc_sys_max_map_count_content() -> String {
    String::from("65530\n")
}

fn proc_sys_file_max_content() -> String {
    String::from("1048576\n")
}

fn proc_sys_nr_open_content() -> String {
    String::from("1048576\n")
}

/// fs.pipe-max-size: upper bound for fcntl(F_SETPIPE_SZ), Linux default 1 MiB.
fn proc_sys_pipe_max_size_content() -> String {
    String::from("1048576\n")
}

/// Proc file that regenerates text on each read (no snapshot in `find()`).
struct ProcSeqINode {
    inode: usize,
    generate: fn() -> String,
}

fn seq_read_at(generate: fn() -> String, offset: usize, buf: &mut [u8]) -> Result<usize> {
    let content = generate();
    let bytes = content.as_bytes();
    if offset >= bytes.len() {
        return Ok(0);
    }
    let len = (bytes.len() - offset).min(buf.len());
    buf[..len].copy_from_slice(&bytes[offset..offset + len]);
    Ok(len)
}

impl INode for ProcSeqINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        seq_read_at(self.generate, offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        // Linux reports size 0 for seq_file pseudo entries; content is generated on read.
        Ok(Metadata {
            dev: 0,
            inode: self.inode,
            size: 0,
            blk_size: 4096,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o444,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

/// A writable sysctl entry: reads like [`ProcSeqINode`], and each `write(2)`
/// hands the whole payload to `store` — per-write-not-per-byte, which is how
/// `/proc/sys` files behave on Linux (`sysctl(8)` and `echo x > file` both
/// issue one write of the complete new value).
struct ProcSysWritableINode {
    inode: usize,
    generate: fn() -> String,
    store: fn(&str) -> Result<()>,
}

impl INode for ProcSysWritableINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        seq_read_at(self.generate, offset, buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        // The value is the written bytes minus the customary trailing newline
        // (`echo` appends one; `sysctl` does not).
        let text = core::str::from_utf8(buf).map_err(|_| FsError::InvalidParam)?;
        (self.store)(text.trim_end_matches('\n'))?;
        Ok(buf.len())
    }

    fn resize(&self, _len: usize) -> Result<()> {
        // Linux `/proc` and `/proc/sys` ignore truncate. Shell redirects
        // (`echo us > /proc/kbd`) open O_TRUNC, which would otherwise fail
        // with the default `NotSupported` resize and never reach write_at —
        // lunarbar's ES/US pill then flipped back on the next metrics tick.
        Ok(())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: self.inode,
            size: 0,
            blk_size: 4096,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

fn store_hostname(value: &str) -> Result<()> {
    if value.len() > crate::uname::HOST_NAME_MAX {
        return Err(FsError::InvalidParam);
    }
    crate::uname::set_hostname(value);
    Ok(())
}

fn store_domainname(value: &str) -> Result<()> {
    if value.len() > crate::uname::HOST_NAME_MAX {
        return Err(FsError::InvalidParam);
    }
    crate::uname::set_domainname(value);
    Ok(())
}

fn proc_kbd_content() -> String {
    super::kbd_layout::proc_content()
}

fn store_kbd(value: &str) -> Result<()> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(());
    }
    if v.eq_ignore_ascii_case("toggle") {
        super::kbd_layout::toggle();
        return Ok(());
    }
    super::kbd_layout::set(v)
}

/// `/proc/self` — target pid is resolved on read, not at `find()` time.
struct ProcSelfSymINode;

impl INode for ProcSelfSymINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let target = current_process_id()
            .map(|id| alloc::format!("{}", id))
            .unwrap_or_else(|| "1".into());
        let bytes = target.as_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let target = current_process_id()
            .map(|id| alloc::format!("{}", id))
            .unwrap_or_else(|| "1".into());
        Ok(Metadata {
            dev: 0,
            // Its own: 12 is `/proc/cpuinfo`'s.
            inode: 19,
            size: target.len(),
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::SymLink,
            mode: 0o777,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

#[derive(Clone, Copy)]
enum ProcPidFileKind {
    Stat,
    Cmdline,
    Status,
    Perf,
    Maps,
    Comm,
    Environ,
    Statm,
    Threads,
}

impl ProcPidFileKind {
    /// The file's slot in its process's inode numbers (see `pid_inode`):
    /// one per kind, none of them the directory's 0 or `PID_FD_DIR_SLOT`.
    fn slot(self) -> usize {
        match self {
            Self::Stat => 1,
            Self::Cmdline => 2,
            Self::Status => 3,
            Self::Perf => 4,
            Self::Maps => 5,
            Self::Comm => 6,
            Self::Environ => 7,
            Self::Statm => 8,
            Self::Threads => 9,
        }
    }
}

/// `/proc/<pid>/maps` in the format of Documentation/filesystems/proc.rst:
/// `address perms offset dev inode pathname`. Consumers (gdb, GC runtimes,
/// address-sanitizer, `pmap`) split on whitespace, so the columns matter more
/// than their exact widths. The backing VMO's koid stands in for the inode and
/// its kernel-object name for the pathname (empty for anonymous memory, like
/// Linux prints anonymous VMAs).
fn proc_pid_maps(proc: &Arc<Process>) -> String {
    use zircon_object::vm::MMUFlags;
    let mut s = String::new();
    for m in proc.vmar().mappings_dump() {
        let r = if m.flags.contains(MMUFlags::READ) {
            'r'
        } else {
            '-'
        };
        let w = if m.flags.contains(MMUFlags::WRITE) {
            'w'
        } else {
            '-'
        };
        let x = if m.flags.contains(MMUFlags::EXECUTE) {
            'x'
        } else {
            '-'
        };
        let p = if m.shared { 's' } else { 'p' };
        let _ = writeln!(
            s,
            "{:08x}-{:08x} {}{}{}{} {:08x} 00:00 {:<10} {}",
            m.start, m.end, r, w, x, p, m.file_offset, m.vmo_id, m.name
        );
    }
    s
}

/// `/proc/<pid>/{stat,cmdline,status}` without snapshotting at lookup time.
struct ProcPidFileINode {
    pid: u64,
    kind: ProcPidFileKind,
}

impl ProcPidFileINode {
    fn bytes(&self) -> Result<Vec<u8>> {
        let proc = ROOT_JOB
            .find_process(self.pid as _)
            .ok_or(FsError::EntryNotFound)?;
        Ok(match self.kind {
            ProcPidFileKind::Stat => proc_pid_stat(&proc).into_bytes(),
            ProcPidFileKind::Cmdline => proc_pid_cmdline(&proc),
            ProcPidFileKind::Status => proc_pid_status(&proc).into_bytes(),
            ProcPidFileKind::Perf => match proc.try_linux() {
                Some(lp) => crate::perf::proc_report(lp, self.pid).into_bytes(),
                None => Vec::new(),
            },
            ProcPidFileKind::Maps => proc_pid_maps(&proc).into_bytes(),
            ProcPidFileKind::Comm => alloc::format!("{}\n", proc_comm(&proc)).into_bytes(),
            // NUL-separated KEY=VALUE list, exactly like cmdline's encoding.
            ProcPidFileKind::Environ => match proc.try_linux() {
                Some(lp) => {
                    let mut out = Vec::new();
                    for env in lp.environ() {
                        out.extend_from_slice(env.as_bytes());
                        out.push(0);
                    }
                    out
                }
                None => Vec::new(),
            },
            ProcPidFileKind::Statm => proc_pid_statm(&proc).into_bytes(),
            ProcPidFileKind::Threads => proc_pid_threads(&proc).into_bytes(),
        })
    }
}

impl INode for ProcPidFileINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let bytes = self.bytes()?;
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let size = self.bytes()?.len();
        Ok(Metadata {
            dev: 0,
            inode: pid_inode(self.pid, self.kind.slot()),
            size,
            blk_size: 4096,
            blocks: size.div_ceil(4096),
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o444,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

fn proc_net_tcp_content() -> String {
    crate::net::proc_net_tcp_content()
}

fn proc_net_udp_content() -> String {
    crate::net::proc_net_udp_content()
}

fn proc_net_unix_content() -> String {
    crate::net::proc_net_unix_content()
}

fn proc_net_dev_content() -> String {
    // Linux-like procfs content used by BusyBox `ifconfig`.
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Inter-|   Receive                                                |  Transmit"
    );
    let _ = writeln!(
        s,
        " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed"
    );

    let ifaces = kernel_hal::net::get_net_device();
    if ifaces.is_empty() {
        let _ = writeln!(s, "{:>6}: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0", "lo");
        return s;
    }

    for iface in ifaces.iter() {
        let name = iface.get_ifname();
        let stats = iface.get_stats();
        let _ = writeln!(
            s,
            "{:>6}: {:>7} {:>7} {:>4} {:>4}    0     0          0         0 {:>8} {:>8} {:>4} {:>4}    0     0       0          0",
            name,
            stats.rx_bytes,
            stats.rx_packets,
            stats.rx_errors,
            stats.rx_dropped,
            stats.tx_bytes,
            stats.tx_packets,
            stats.tx_errors,
            stats.tx_dropped,
        );
    }
    s
}

fn proc_net_route_content() -> String {
    use crate::net::ipv4_netmask;

    let mut s = String::new();
    let _ = writeln!(
        s,
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT"
    );

    let ifaces = drivers::all_net().as_vec();
    for iface in ifaces.iter() {
        let name = iface.get_ifname();
        for route in iface.get_routes() {
            if let IpCidr::Ipv4(dst_cidr) = route.dst {
                let dst = u32::from_ne_bytes(dst_cidr.address().0);
                let gateway = match route.gateway {
                    Some(IpAddress::Ipv4(gw)) => u32::from_ne_bytes(gw.0),
                    _ => 0,
                };
                let mask = u32::from_ne_bytes(ipv4_netmask(dst_cidr.prefix_len()).0);
                let flags = if route.gateway.is_some() {
                    0x0003 // RTF_UP | RTF_GATEWAY
                } else {
                    0x0001 // RTF_UP
                };

                let _ = writeln!(
                    s,
                    "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
                    name, dst, gateway, flags, mask
                );
            }
        }
    }
    s
}

fn proc_uptime_content() -> String {
    // Format: "<uptime_seconds> <idle_seconds>\n". `idle_seconds` is summed
    // across all online CPUs then averaged back down to one CPU's worth, the
    // same convention `/proc/perf/kernel`'s idle% uses — and it is real halt
    // time (`kernel_hal::kstats::note_idle`, driven from the actual `hlt`/
    // `mwait` idle path), not a placeholder.
    let now = kernel_hal::timer::timer_now();
    let uptime = now.as_secs_f64();
    let idle_ns = kernel_hal::kstats::snapshot().idle_ns;
    let ncpus = kernel_hal::online_cpu_count().max(1) as f64;
    let idle = idle_ns as f64 / 1_000_000_000.0 / ncpus;
    format!("{:.2} {:.2}\n", uptime, idle)
}

/// `/proc/stat` — aggregate and per-CPU counters in USER_HZ jiffies
/// (BusyBox `top` reads this after chdir to `/proc` and diffs consecutive
/// reads to compute %usr/%sys/%idle). `user`/`system` come from real CPU time
/// measured around each thread's `Future::poll` call — never wall-clock time
/// spent blocked in a syscall — and `idle` from the same halt-time counter
/// `/proc/perf/kernel` and `/proc/uptime` already use, so all three views
/// agree with each other and with reality.
///
/// `btime` is the boot time in seconds since the epoch, `intr` the interrupts
/// handled since boot and `processes` the processes created since boot
/// (`total_forks`). All three read 0 (`processes` read the number ALIVE): `ps
/// -o lstart` adds the process's start ticks to `btime`, so every process
/// started on 1 January 1970, and `vmstat` differentiated `processes` and
/// `intr` into forks and interrupts per second, both 0 or negative.
fn proc_stat_content() -> String {
    let running = crate::loadavg::runnable_count();
    let ncpus = kernel_hal::online_cpu_count().max(1);
    let btime = boot_time_secs(
        kernel_hal::timer::timer_now_realtime(),
        kernel_hal::timer::timer_now(),
    );
    let intr = kernel_hal::kstats::snapshot().irq_total;
    let forks = crate::process::processes_created();

    let (mut total_user, mut total_sys, mut total_idle) = (0u64, 0u64, 0u64);
    let mut per_cpu = String::new();
    for cpu in 0..ncpus {
        let (user, sys, idle) = kernel_hal::kstats::cpu_times_jiffies(cpu);
        total_user += user;
        total_sys += sys;
        total_idle += idle;
        // "cpuN user nice system idle iowait irq softirq steal guest guest_nice"
        let _ = writeln!(per_cpu, "cpu{cpu} {user} 0 {sys} {idle} 0 0 0 0 0 0");
    }

    format!(
        "cpu  {total_user} 0 {total_sys} {total_idle} 0 0 0 0 0 0\n\
         {per_cpu}\
         intr {intr}\n\
         ctxt 0\n\
         btime {btime}\n\
         processes {forks}\n\
         procs_running {}\n\
         procs_blocked 0\n",
        running
    )
}

/// The `btime` of `/proc/stat`: when the machine booted, in seconds since
/// the epoch, which is the wall clock now minus how long it has been up
/// (`getboottime64`). `ps` adds a process's start ticks to it to print
/// `lstart`/`start_time`, so a `btime` of 0 dates every process to 1970.
/// A wall clock behind the uptime (unset RTC) gives 0 rather than a wrap.
fn boot_time_secs(realtime: Duration, monotonic: Duration) -> u64 {
    realtime.saturating_sub(monotonic).as_secs()
}

/// `/proc/loadavg` — one-line load averages for `top` header.
fn proc_loadavg_content() -> String {
    let procs = all_processes();
    let total = procs.len().max(1);
    // Runnable count (excludes idle/blocked tasks), not the live-process count —
    // see `loadavg::runnable_count`. `+1` so the field is never below 1: the
    // process reading `/proc/loadavg` is itself runnable but is excluded by the
    // sampler's self-subtraction, and Linux always reports at least 1 here.
    let running = crate::loadavg::runnable_count() + 1;
    let last_pid = procs.last().map(|p| p.id()).unwrap_or(1);
    let [l1, l5, l15] = crate::loadavg::loadavg_f64();
    format!("{l1:.2} {l5:.2} {l15:.2} {running}/{total} {last_pid}\n")
}

/// `/proc/hunter` — security subsystem status and recent intrusion-detection
/// event ring, rendered on each read by the `hunter` crate.
fn proc_hunter_content() -> String {
    hunter::render_report()
}

fn proc_meminfo_content() -> String {
    let (used, total) = kernel_hal::mem::memory_usage();
    let free = total.saturating_sub(used);
    let mut s = String::with_capacity(128);
    let _ = writeln!(s, "MemTotal:     {:>10} kB", total / 1024);
    let _ = writeln!(s, "MemFree:      {:>10} kB", free / 1024);
    let _ = writeln!(s, "MemAvailable: {:>10} kB", free / 1024);
    let _ = writeln!(s, "Buffers:               0 kB");
    let _ = writeln!(s, "Cached:                0 kB");
    // The kernel heap is a FIXED arena, separate from the frames above, and
    // running it out kills the machine (`alloc_error` -> panic). It appeared
    // in no /proc file, so its growth could only be seen as the crash. Not a
    // Linux meminfo field name: KernelHeap* says what it is.
    let (kheap_used, kheap_total) = kernel_hal::mem::kernel_heap_usage();
    if kheap_total > 0 {
        let _ = writeln!(s, "KernelHeapTotal: {:>10} kB", kheap_total / 1024);
        let _ = writeln!(s, "KernelHeapUsed:  {:>10} kB", kheap_used / 1024);
        let _ = writeln!(
            s,
            "KernelHeapFree:  {:>10} kB",
            kheap_total.saturating_sub(kheap_used) / 1024
        );
    }
    s
}

/// `/proc/kheap` — where the kernel heap went, live, by size class.
///
/// The counterpart of `/proc/memhogs` for the fixed kernel arena. Read it
/// twice while the desktop runs: the size class that grew between the reads is
/// the leak, and seeing that beats reading it off the OOM panic afterwards.
fn proc_kheap_content() -> String {
    kernel_hal::mem::kernel_heap_report()
}

/// `/proc/memhogs` — where the physical RAM actually went.
///
/// `frame_alloc FAILED: 2561 MiB used / 2561 MiB managed` says memory is gone
/// but not to whom, and the two possible answers need opposite fixes: if the
/// live processes account for it, the desktop simply does not fit; if they do
/// not, the kernel is leaking and no amount of trimming userspace will help.
///
/// So this reports both sides and, most importantly, the DIFFERENCE:
///
///   * per-process private/shared/mapped bytes (the `/proc/<pid>/status`
///     numbers), biggest first,
///   * the MAP_SHARED file-VMO registry, which holds committed pages that
///     belong to NO process once the last mapper exits,
///   * `unattributed` = used - (private + shared + registry). Anything large
///     there is kernel-side: ramfs file contents, the kernel heap, or frames
///     that were allocated and never freed.
///
/// Read it while the desktop is up, not only after the failure: the shape of
/// the growth over two reads is what distinguishes a leak from a working set.
///
/// Deliberately NOT printed from the `frame_alloc` failure path: that runs
/// inside page-fault handling with the faulting VMAR's lock held, and walking
/// every process takes those same locks — the report would deadlock exactly
/// when it is needed.
/// `/proc/syscalls`: how many times each syscall has been made since boot,
/// busiest first. Numbers are the raw x86_64 ones (`asm/unistd_64.h`); read
/// this twice around a suspicious interval and subtract to get a profile of
/// what a process is actually doing when it burns system time in silence.
fn proc_syscalls_content() -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::from("# syscall calls (x86_64 numbers, busiest first)\n");
    for (num, calls) in crate::syscall_stats::snapshot() {
        let _ = writeln!(out, "{:>4} {}", num, calls);
    }
    // The last calls made, oldest first. With every process asleep this names
    // the syscall each one went to sleep in -- the thing a histogram cannot
    // say and a log will never print.
    out.push_str("# recent: pid/syscall, oldest first\n");
    let mut line = alloc::string::String::new();
    for (i, (pid, num)) in crate::syscall_stats::trace().into_iter().enumerate() {
        let _ = write!(line, "{}/{} ", pid, num);
        if i % 12 == 11 {
            let _ = writeln!(out, "{}", line.trim_end());
            line.clear();
        }
    }
    if !line.is_empty() {
        let _ = writeln!(out, "{}", line.trim_end());
    }
    out
}

fn proc_memhogs_content() -> String {
    let (used, total) = kernel_hal::mem::memory_usage();
    let procs = crate::process::all_live_processes();
    let mut rows: Vec<(u64, u64, u64, zircon_object::object::KoID, String)> = procs
        .iter()
        .map(|p| {
            let st = p.vmar().get_task_stats();
            (
                st.private_bytes(),
                st.shared_bytes(),
                st.mapped_bytes(),
                p.id(),
                p.name(),
            )
        })
        .collect();
    rows.sort_by_key(|r| core::cmp::Reverse(r.0));

    let sum_priv: u64 = rows.iter().map(|r| r.0).sum();
    let sum_shared: u64 = rows.iter().map(|r| r.1).sum();
    let (reg_entries, reg_bytes) = super::file::shared_file_vmo_stats();

    let mib = |b: u64| b / (1024 * 1024);
    let mut s = String::with_capacity(2048);
    let _ = writeln!(
        s,
        "physical:   {:>6} MiB used / {:>6} MiB managed",
        mib(used as u64),
        mib(total as u64)
    );
    let _ = writeln!(
        s,
        "processes:  {} live, {} MiB private, {} MiB shared",
        rows.len(),
        mib(sum_priv),
        mib(sum_shared)
    );
    let _ = writeln!(
        s,
        "shared-vmo registry: {} entries, {} MiB committed (owned by no process)",
        reg_entries,
        mib(reg_bytes)
    );
    // Per-kind VMO accounting. When the per-process totals do not add up to
    // physical use, this says WHICH kind of object holds the rest, and the
    // answers need opposite fixes: a growing `Contiguous` count is the DRM
    // buffer pool, a growing `PagedSource` count is orphaned file mappings, a
    // growing `Paged` count with no process behind it is a plain VMO leak.
    let _ = writeln!(s, "vmo by kind (live / declared MiB):");
    for (kind, live, bytes) in zircon_object::vm::vmo_stats() {
        let _ = writeln!(
            s,
            "  {:<12} {:>6} / {:>6}",
            alloc::format!("{:?}", kind),
            live,
            mib(bytes as u64)
        );
    }
    let (_, sole, inode_lo, inode_hi) = super::file::shared_file_vmo_refs();
    let _ = writeln!(
        s,
        "shared-vmo refs: {} entries with no mapper left, inode strong refs {}..{}",
        sole, inode_lo, inode_hi
    );
    let attributed = sum_priv
        .saturating_add(sum_shared)
        .saturating_add(reg_bytes);
    let _ = writeln!(
        s,
        "unattributed: {} MiB (kernel heap, ramfs contents, or leaked frames)",
        mib((used as u64).saturating_sub(attributed))
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "     PID    PRIV_KB  SHARED_KB  MAPPED_KB  NAME");
    for (priv_b, shared_b, mapped_b, pid, name) in rows.iter().take(40) {
        let _ = writeln!(
            s,
            "{:>8} {:>10} {:>10} {:>10}  {}",
            pid,
            priv_b / 1024,
            shared_b / 1024,
            mapped_b / 1024,
            name
        );
    }
    s
}

/// Minimal `/proc/cpuinfo` for fastfetch CPU detection on x86_64.
/// `/proc/oops` — the contained-fault record kept in RAM by `kernel_hal::oops_log`.
///
/// The fault path cannot write a file (interrupts off, heap possibly smashed,
/// no locks may be taken), so it appends here instead and userspace persists it
/// to `/var/log/oops.log`. Empty output means no kernel fault has been contained
/// since boot — the healthy case.
fn proc_oops_content() -> String {
    let bytes = kernel_hal::oops_log::snapshot();
    if bytes.is_empty() {
        return String::from("# no contained kernel faults since boot\n");
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn proc_gpudbg_content() -> String {
    // GPUs register as DRM devices (Device::Drm), not displays, and there may be
    // more than one — dump every one.
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.debug_dump());
    }
    if s.is_empty() {
        s.push_str("[gpudbg] no DRM driver with debug support\n");
    }
    s
}

/// `/proc/gpustep2` — opt-in GPU copy-engine bring-up Step 2 (instance block +
/// GMMU flush). NOT read-only: each `cat` issues the real GPU writes, but only
/// on the GPU that does not drive the console. Kept separate from `/proc/gpudbg`
/// so the latter stays safe to poll.
fn proc_gpustep2_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step2());
    }
    if s.is_empty() {
        s.push_str("[gpustep2] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep3` — opt-in bring-up Step 3 (doorbell + runlist commit) on the
/// non-console GPU.
fn proc_gpustep3_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step3());
    }
    if s.is_empty() {
        s.push_str("[gpustep3] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep4` — opt-in bring-up Step 4 (ring doorbell + SET_OBJECT) on the
/// non-console GPU.
fn proc_gpustep4_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step4());
    }
    if s.is_empty() {
        s.push_str("[gpustep4] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep5` — opt-in: the real vendored RM core's own attach path
/// (real HAL bind/attach work). Moved out of `/proc/gpudbg` after it hung
/// real hardware on a plain `cat` -- deliberately separate so `gpudbg`
/// stays safe to poll.
fn proc_gpustep5_content() -> String {
    // TEMPORARY: bracket the generic driver-enumeration dispatch itself,
    // to isolate whether a real-hardware hang is inside NvidiaGpu's own
    // bringup_step5 or already stuck in all_drm()/the per-driver loop
    // before it -- two prior real-hardware tests (confirmed-fresh
    // binaries) showed zero trace output even from bringup_step5's own
    // first line, so this checkpoint runs strictly before that call.
    log::warn!("[gpustep5] proc_gpustep5_content: entered, about to enumerate drm drivers");
    let drivers = kernel_hal::drivers::all_drm();
    log::warn!(
        "[gpustep5] proc_gpustep5_content: got driver list, count={}",
        drivers.as_vec().len()
    );
    let mut s = String::new();
    for d in drivers.as_vec().iter() {
        log::warn!("[gpustep5] proc_gpustep5_content: calling bringup_step5 on next driver");
        s.push_str(&d.bringup_step5());
        log::warn!("[gpustep5] proc_gpustep5_content: bringup_step5 returned");
    }
    if s.is_empty() {
        s.push_str("[gpustep5] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep6` — opt-in: real `kgspInitRm` GSP-RM boot. Requires
/// `/proc/gpustep5` to have succeeded first. The deepest, riskiest bring-up
/// step yet (VBIOS/FWSEC extraction, Booter secure boot, WPR2 setup).
fn proc_gpustep6_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step6());
    }
    if s.is_empty() {
        s.push_str("[gpustep6] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep7` â readback of the firmware-provided GspStaticConfigInfo
/// (GPU name, VRAM geometry, VBIOS IDs) fetched from the live GSP-RM during
/// `/proc/gpustep6`. Pure readback: safe to cat repeatedly.
fn proc_gpustep7_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step7());
    }
    if s.is_empty() {
        s.push_str("[gpustep7] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep8` â RM API controls served by the live GSP-RM (name,
/// UUID, FB heap total/free). Read-only; safe to cat repeatedly.
fn proc_gpustep8_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step8());
    }
    if s.is_empty() {
        s.push_str("[gpustep8] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep9` â gpuStatePreInit/StateInit/StateLoad (rest of the real
/// RmInitAdapter) against the live GSP. One-shot per boot; result cached.
fn proc_gpustep9_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step9());
    }
    if s.is_empty() {
        s.push_str("[gpustep9] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep10` — first real copy-engine data movement (CE memset +
/// copy between VRAM buffers, CPU readback verify). Cached per boot.
fn proc_gpustep10_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step10());
    }
    if s.is_empty() {
        s.push_str("[gpustep10] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep11` — GSP-RM boot on the CONSOLE GPU, with the graphic
/// console frozen around it. The wedge that made step 6 skip this GPU is
/// (per every experiment so far) CPU pixel writes landing in its BAR1 —
/// the console framebuffer — during the SEC2 GSP-RM resume window; step 6's
/// own narration draws dozens of lines right into that window. Freezing =
/// KD_GRAPHICS on the active VT: pixel presentation stops (this is the
/// battle-tested Xorg handover path), while the VT shadow buffer keeps
/// accumulating and serial/dmesg stay live. Returning to KD_TEXT repaints
/// the whole backlog, so nothing is visually lost — the screen just stands
/// still for the few seconds the boot takes. NVIDIA's own driver does the
/// same thing around init via os_disable_console_access() (osinit.c).
fn proc_gpustep11_content() -> String {
    // NOTE: an earlier revision froze the graphic console (KD_GRAPHICS)
    // around this call, mirroring Linux's os_disable_console_access(). The
    // console GPU wedged identically with zero CPU pixel writes -- so the
    // console is exonerated and this now runs UNFROZEN, with the driver
    // narrating every post-STARTCPU register access live (see
    // bringup_step11): on a wedge, the last line on screen names the exact
    // register access that never completed.
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step11());
    }
    if s.is_empty() {
        s.push_str("[gpustep11] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep12` — EXP 1: console-GPU GSP boot with the display engine
/// held in reset (scanout stopped). BLANKS THE SCREEN; run blind and capture
/// to a file (`cat /proc/gpustep12 > /r12.txt; sync`), then hard-reset.
fn proc_gpustep12_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step12());
    }
    if s.is_empty() {
        s.push_str("[gpustep12] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep13` — EXP 2: console-GPU GSP boot with a pre-STARTCPU
/// interrupt-drain "pseudo-ISR service loop" (no display touch). Snapshots +
/// W1C-drains the CPU-facing interrupt tree right before the SEC2 STARTCPU
/// store. Screen is untouched, but if STARTCPU still wedges the snapshot only
/// survives on the framebuffer/serial -- capture with
/// `cat /proc/gpustep13 > /r13.txt; sync`, then read /r13.txt.
fn proc_gpustep13_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step13());
    }
    if s.is_empty() {
        s.push_str("[gpustep13] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep14` — CONSOLE GPU full bring-up chained in one shot (attach ->
/// GSP boot with console SEC2 drain -> RM controls -> state-load -> CE),
/// bringing the primary to the same state as the secondary. Capture with
/// `cat /proc/gpustep14 > /r14.txt; sync`.
fn proc_gpustep14_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step14());
    }
    if s.is_empty() {
        s.push_str("[gpustep14] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep15` — GR (graphics/compute) engine GPC/TPC/SM config probe on
/// a state-loaded GPU, via the live GSP-RM. Read-only, repeatable.
fn proc_gpustep15_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step15());
    }
    if s.is_empty() {
        s.push_str("[gpustep15] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep16` — GR allocation ladder (client/device/subdevice/VAS/
/// TSG(GR)/ctxshare) on a state-loaded GPU via the vendored resource server.
/// Idempotent (the ladder stays alive for step17).
fn proc_gpustep16_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step16());
    }
    if s.is_empty() {
        s.push_str("[gpustep16] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep17` — compute channel on the step-16 ladder (USERD + GPFIFO
/// memory + channel-in-TSG + TURING_COMPUTE_A + schedule). Idempotent.
fn proc_gpustep17_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step17());
    }
    if s.is_empty() {
        s.push_str("[gpustep17] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep18` — first Eclipse-authored submission on the step-17
/// channel (semaphore method stream + GP entry + GPPut + doorbell + CPU
/// poll). Idempotent once fully successful.
fn proc_gpustep18_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step18());
    }
    if s.is_empty() {
        s.push_str("[gpustep18] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep19` — first real compute launch (Turing QMD + minimal SM75
/// kernel via SEND_PCAS, verified by the QMD RELEASE0 semaphore). Idempotent
/// once the semaphore lands.
fn proc_gpustep19_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step19());
    }
    if s.is_empty() {
        s.push_str("[gpustep19] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep20` — kernel store: patched-immediate MOV+STG+EXIT on the
/// step-19 harness with triple verification. Idempotent once verified.
fn proc_gpustep20_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step20());
    }
    if s.is_empty() {
        s.push_str("[gpustep20] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep21` — 32-thread kernel with per-thread verification.
fn proc_gpustep21_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step21());
    }
    if s.is_empty() {
        s.push_str("[gpustep21] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep22` — chip-scale grid (68 CTAs / 2176 threads), verified.
fn proc_gpustep22_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step22());
    }
    if s.is_empty() {
        s.push_str("[gpustep22] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpustep23` — integer SAXPY (load-compute-store), per-element verified.
fn proc_gpustep23_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_step23());
    }
    if s.is_empty() {
        s.push_str("[gpustep23] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpuinit` — the whole compute bring-up ladder in one cat:
/// core RM attach (5) → GSP-RM boot (6) → state load (9) → GR alloc
/// ladder (16) → GPFIFO channel (17). Each internal step is idempotent
/// (guarded by its own done-flag), so re-catting is safe and a stage that
/// already ran is a fast no-op. After this, /proc/gpustep23 (or the
/// benchmark) can launch compute directly.
fn proc_gpuinit_content() -> String {
    let mut s = String::new();
    let drivers = kernel_hal::drivers::all_drm();
    if drivers.as_vec().is_empty() {
        return String::from("[gpuinit] no DRM driver with bring-up support\n");
    }
    for d in drivers.as_vec().iter() {
        s.push_str("[gpuinit] ===== stage 1/5: RM core attach (step5) =====\n");
        s.push_str(&d.bringup_step5());
        s.push_str("[gpuinit] ===== stage 2/5: GSP-RM boot (step6) =====\n");
        s.push_str(&d.bringup_step6());
        s.push_str("[gpuinit] ===== stage 3/5: state pre-init/init/load (step9) =====\n");
        s.push_str(&d.bringup_step9());
        s.push_str("[gpuinit] ===== stage 4/5: GR alloc ladder (step16) =====\n");
        s.push_str(&d.bringup_step16());
        s.push_str("[gpuinit] ===== stage 5/5: GPFIFO + TURING_COMPUTE_A channel (step17) =====\n");
        s.push_str(&d.bringup_step17());
    }
    s.push_str(
        "[gpuinit] ===== chain complete -- GPU ready; cat /proc/gpustep23 to run SAXPY =====\n",
    );
    s
}

/// `/proc/gpubench` — integer-ALU GIOPS benchmark (needs /proc/gpuinit first).
fn proc_gpubench_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_bench());
    }
    if s.is_empty() {
        s.push_str("[gpubench] no DRM driver with bench support\n");
    }
    s
}

/// `/proc/gpuedid` — real display query (connectors + EDID) via the RM's
/// NV04_DISPLAY_COMMON. Read-only; needs /proc/gpuinit first.
/// `/proc/gpusnd` -- audio pipeline state, read back from the hardware.
///
/// Playing to a GPU HDA function can fail in a way that reports success at
/// every layer: the ring drains, the stream descriptor says RUN, LPIB
/// advances, and no call returns an error -- yet the cable carries nothing,
/// because on a GPU the codec is only half the path and the display engine
/// owns the other half. This dump shows both halves.
fn proc_gpusnd_content() -> String {
    let mut s = String::new();
    // ALSA card order (the same order /dev/snd and /dev/dsp<n> use), so
    // "card N" here is hw:N -- audio-probe reads its verdict from this block.
    let devices = super::audio_cards_alsa_order();
    if devices.is_empty() {
        s.push_str("[gpusnd] no audio devices registered\n");
        return s;
    }
    for (card, d) in devices.iter().enumerate() {
        s.push_str(&format!("[gpusnd] --- card {} ---\n", card));
        s.push_str(&d.diagnostics());
    }
    // The display side, which no amount of codec state can reveal.
    s.push_str(&kernel_hal::drivers::hdmi_audio_status());
    s
}

fn proc_gpuedid_content() -> String {
    let mut s = String::new();
    // UEFI-captured EDID of the active console panel first: this is the real
    // monitor (on the GOP-driving GPU), read by the firmware at power-on with
    // no GPU display bring-up. Available even when the console GPU's GSP
    // display cannot be brought up.
    s.push_str(&format_uefi_edid());
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_edid());
    }
    if s.is_empty() {
        s.push_str("[gpuedid] no DRM driver with display support\n");
    }
    s
}

/// `/proc/gpucefill` — CE-offload visual test: CE-memset the console GPU's
/// scanout framebuffer to a solid colour (white) via the persistent CeUtils
/// channel. Requires `/proc/gpustep14` (state-load) first. If the screen turns
/// white, the BAR1->VRAM offset is correct and the CE drives the display —
/// green light for the full `ce_blit` present path.
fn proc_gpucefill_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_ce_fill_fb());
    }
    if s.is_empty() {
        s.push_str("[gpucefill] no DRM driver with CE-offload support\n");
    }
    s
}

/// `/proc/gpucefillp2p` — P2P CE-offload visual test: from the COMPUTE GPU,
/// CE-memset the CONSOLE GPU's scanout framebuffer white over PCIe
/// peer-to-peer. Confirms P2P works (screen turns white) before relying on it
/// for the present path. Requires the compute GPU state-loaded.
fn proc_gpucefillp2p_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.bringup_ce_fill_fb_p2p());
    }
    if s.is_empty() {
        s.push_str("[gpucefillp2p] no DRM driver with P2P CE-offload support\n");
    }
    s
}

/// `/proc/gpusurvive` — read + clear the CMOS survival breadcrumb from the
/// previous console-GPU GSP-boot attempt. On a serial-less box this is the only
/// thing that outlives a SEC2-window wedge (the CPU hangs; the CMOS NVRAM keeps
/// the last milestone + RM narration count across the reboot). Safe/instant:
/// two port I/O reads, no GPU, no bring-up. Reading it clears the breadcrumb.
fn proc_gpusurvive_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.survival_report());
    }
    if s.is_empty() {
        s.push_str("[gpusurvive] no console DRM driver present to read the breadcrumb\n");
    }
    s
}

/// Decode the bootloader-captured UEFI EDID into a human block.
fn format_uefi_edid() -> String {
    use core::fmt::Write;
    let mut s = String::new();
    let Some((e, len)) = zcore_drivers::display::boot_edid() else {
        s.push_str("[gpuedid] === UEFI active-panel EDID: none captured by firmware ===\n");
        return s;
    };
    let valid = len >= 128 && e[0] == 0x00 && e[7] == 0x00 && e[1..=6].iter().all(|&b| b == 0xFF);
    if !valid {
        let _ = writeln!(
            s,
            "[gpuedid] === UEFI active-panel EDID: {} bytes, header INVALID ===",
            len
        );
        // Dump what we captured so a bad pointer (garbage) is distinguishable
        // from an empty buffer (zeros) or a real-but-nonstandard block.
        let _ = write!(s, "[gpuedid] raw head:");
        for b in e[..32].iter() {
            let _ = write!(s, " {:02x}", b);
        }
        let _ = writeln!(s);
        return s;
    }
    // Manufacturer PNP id: bytes 8-9, big-endian, 5-bit packed letters.
    let m = ((e[8] as u16) << 8) | e[9] as u16;
    let l1 = (b'A' - 1 + ((m >> 10) & 0x1f) as u8) as char;
    let l2 = (b'A' - 1 + ((m >> 5) & 0x1f) as u8) as char;
    let l3 = (b'A' - 1 + (m & 0x1f) as u8) as char;
    let product = ((e[11] as u16) << 8) | e[10] as u16;
    let serial =
        (e[12] as u32) | ((e[13] as u32) << 8) | ((e[14] as u32) << 16) | ((e[15] as u32) << 24);
    let year = 1990u32 + e[17] as u32;
    let (cm_w, cm_h) = (e[21] as u32, e[22] as u32);
    let _ = writeln!(
        s,
        "[gpuedid] === UEFI active-panel EDID: {}{}{} product={:#06x} serial={:#010x} year={} (EDID v{}.{}) ===",
        l1, l2, l3, product, serial, year, e[18], e[19]
    );
    let _ = writeln!(
        s,
        "[gpuedid] MONITOR: {}{}{} product={:#06x} year={} -- {}x{} mm",
        l1,
        l2,
        l3,
        product,
        year,
        cm_w * 10,
        cm_h * 10
    );
    // First detailed timing descriptor (byte 54) = preferred/native mode.
    let d = &e[54..72];
    let pclk_khz = (((d[1] as u32) << 8) | d[0] as u32) * 10;
    if pclk_khz != 0 {
        let h_active = (d[2] as u32) | (((d[4] as u32) & 0xF0) << 4);
        let v_active = (d[5] as u32) | (((d[7] as u32) & 0xF0) << 4);
        let _ = writeln!(
            s,
            "[gpuedid] native mode: {}x{} (pixel clock {} kHz)",
            h_active, v_active, pclk_khz
        );
    }
    let _ = write!(s, "[gpuedid] EDID head:");
    for b in e[..32].iter() {
        let _ = write!(s, " {:02x}", b);
    }
    let _ = writeln!(s);
    s
}

/// `/proc/gpudump` — read-only discriminating hardware dump for every NVIDIA
/// GPU (console + secondary): display head liveness, VGA workspace base, PMC,
/// BSI scratch, sysmem flush. NO GSP boot -> ZERO wedge risk. Read this first
/// and diff primary vs secondary to pre-decide the display experiments.
fn proc_gpudump_content() -> String {
    let mut s = String::new();
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        s.push_str(&d.hw_dump());
    }
    if s.is_empty() {
        s.push_str("[gpudump] no DRM driver with bring-up support\n");
    }
    s
}

/// `/proc/gpuroles` — console vs compute NVIDIA GPUs, the DRM nodes each
/// owns, and whether GSP-RM is attached. Pin with `nvidia.compute=BB.DD.F`.
fn proc_gpuroles_content() -> String {
    let mut s = String::new();
    let pin = kernel_hal::boot::cmdline();
    let pinned = pin
        .split([':', ' ', '\t', '\n'])
        .find(|t| t.starts_with("nvidia.compute="));
    if let Some(p) = pinned {
        let _ = writeln!(s, "[gpuroles] cmdline pin: {p}");
    } else {
        let _ = writeln!(
            s,
            "[gpuroles] cmdline pin: (auto — first non-console NVIDIA GPU)"
        );
    }
    let mut n = 0u32;
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        let line = d.gpu_role_line();
        if !line.is_empty() {
            n += 1;
            s.push_str(&line);
        }
    }
    if n == 0 {
        s.push_str("[gpuroles] no NVIDIA DRM GPUs\n");
    }
    // Which GPU actually answers each /dev/dri node. This is the map that
    // decides where an ioctl lands, so it is the thing to read when a client
    // has opened a node and is talking to a card it did not expect.
    let nodes = crate::fs::devfs::drm::gpu_nodes();
    if nodes.is_empty() {
        s.push_str("[gpuroles] no /dev/dri nodes\n");
    } else {
        for node in nodes {
            let _ = writeln!(
                s,
                "[gpuroles] /dev/dri/{} + /dev/dri/{} -> {} pci_bdf={:x?} console={}",
                crate::fs::devfs::drm::node_name(node.card_minor()),
                crate::fs::devfs::drm::node_name(node.render_minor()),
                node.driver.name(),
                node.driver.pci_bdf(),
                node.driver.is_console_gpu(),
            );
        }
    }
    s
}

/// `/proc/usbhid`: per-interface USB HID diagnostics — bInterfaceProtocol,
/// subclass, VID:PID, the role we bound it as, and the bytes of the last
/// report seen. On real hardware where no kernel log is reachable, this is how
/// a "cursor drawn but frozen, keyboard fine" mouse is diagnosed: `cat` it from
/// a text VT, move the mouse, `cat` it again. `reports=0` on the pointer means
/// no reports arrive (endpoint/enumeration); a changing `last=[..]` whose bytes
/// don't look like `[buttons, dx, dy, ...]` means a report-ID/non-boot layout.
fn proc_usbhid_content() -> String {
    let mut s = String::new();
    let mut any = false;
    for d in kernel_hal::drivers::all_input().as_vec().iter() {
        let line = d.debug_report();
        if !line.is_empty() {
            any = true;
            s.push_str(&line);
        }
    }
    if !any {
        s.push_str("[usbhid] no USB HID input devices reporting diagnostics\n");
    }
    s
}

fn proc_cpuinfo_content() -> String {
    let mut brand = kernel_hal::cpu::cpu_brand();
    if brand.is_empty() {
        brand = "Eclipse CPU".into();
    }
    let cpu_count = kernel_hal::cpu::cpu_count() as usize;
    let mut s = String::new();
    for i in 0..cpu_count {
        #[cfg(target_arch = "x86_64")]
        {
            use core::fmt::Write;
            let _ = writeln!(
                s,
                "processor\t: {}\n\
                 vendor_id\t: GenuineIntel\n\
                 model name\t: {}\n\
                 stepping\t: 0\n\
                 cpu MHz\t\t: {:.3}\n\
                 cache size\t: 4096 KB\n\
                 physical id\t: {}\n\
                 core id\t\t: {}\n\
                 cpu cores\t: {}",
                i,
                brand,
                kernel_hal::cpu::cpu_frequency() as f64,
                i,
                i,
                cpu_count
            );
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            use core::fmt::Write;
            let _ = writeln!(
                s,
                "processor\t: {}\n\
                 model name\t: {}\n\
                 cpu cores\t: {}",
                i, brand, cpu_count
            );
        }
    }
    s
}

/// Empty swap table (header only) — fastfetch falls back to meminfo for swap stats.
fn proc_swaps_content() -> String {
    "Filename\t\tType\t\tSize\t\tUsed\t\tPriority\n".into()
}

fn proc_mounts_content() -> String {
    super::proc_mounts_content()
}

/// `/proc/filesystems` — the filesystem types the kernel can mount. Each line is
/// an optional `nodev` (the fs needs no backing block device) followed by a TAB
/// and the type name. Userland (`mount`, `grep`, init scripts) probes this
/// before mounting; a missing file makes tools log
/// `grep: /proc/filesystems: No such file or directory`.
fn proc_filesystems_content() -> String {
    "nodev\tsysfs\n\
     nodev\tproc\n\
     nodev\ttmpfs\n\
     nodev\tdevtmpfs\n\
     nodev\tramfs\n\
     nodev\tdevpts\n\
     \tbtrfs\n\
     \tvfat\n"
        .into()
}

fn proc_net_arp_content() -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "IP address       HW type     Flags       HW address            Mask     Device"
    );
    let entries = crate::net::arp_cache::get_entries();
    for (ip, mac) in entries {
        let dev_name = if let Ok(dev) = crate::net::netdev_for_ipv4(ip) {
            dev.get_ifname()
        } else {
            kernel_hal::net::get_net_device()
                .iter()
                .find(|d| d.get_ifname() != "loopback")
                .map(|d| d.get_ifname())
                .unwrap_or_else(|| "eth0".into())
        };
        let mac_bytes = mac.as_bytes();
        let _ = writeln!(
            s,
            "{:<15}  0x1         0x2         {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}     *        {}",
            ip,
            mac_bytes[0],
            mac_bytes[1],
            mac_bytes[2],
            mac_bytes[3],
            mac_bytes[4],
            mac_bytes[5],
            dev_name
        );
    }
    s
}

fn proc_net_if_inet6_content() -> String {
    let mut s = String::new();
    let ifaces = kernel_hal::net::get_net_device();
    for (idx, iface) in ifaces.iter().enumerate() {
        crate::net::ensure_ipv6_link_local(iface.as_ref());
        let name = iface.get_ifname();
        let ifindex = idx + 1;
        for ip in iface.get_ip_address() {
            if let IpCidr::Ipv6(cidr) = ip {
                let addr = cidr.address();
                if addr.is_unspecified() {
                    continue;
                }
                let mut addr_hex = String::new();
                for &byte in addr.as_bytes() {
                    let _ = write!(addr_hex, "{:02x}", byte);
                }
                let ifindex_hex = format!("{:08x}", ifindex);
                let prefix_hex = format!("{:02x}", cidr.prefix_len());
                let scope_hex = if addr.is_loopback() {
                    "10"
                } else if addr.is_link_local() {
                    "20"
                } else {
                    "00"
                };
                let flags_hex = if addr.is_loopback() { "80" } else { "00" };
                let _ = writeln!(
                    s,
                    "{} {} {} {} {} {}",
                    addr_hex, ifindex_hex, prefix_hex, scope_hex, flags_hex, name
                );
            }
        }
    }
    s
}

/// Resolve an absolute `/proc/...` path without walking the ext2 backing store.
pub(crate) fn lookup_path(path: &str, follow_times: usize) -> Result<Arc<dyn INode>> {
    let path = path.trim_end_matches('/');
    if path == "/proc" {
        return Ok(PROC_ROOT.clone());
    }
    let rest = path.strip_prefix("/proc/").ok_or(FsError::EntryNotFound)?;
    if rest.is_empty() {
        return Ok(PROC_ROOT.clone());
    }
    PROC_ROOT.lookup_follow(rest, follow_times)
}

lazy_static! {
    static ref PROC_ROOT: Arc<dyn INode> = Arc::new(ProcRootINode);
    static ref PROC_NET_DIR: Arc<dyn INode> = Arc::new(ProcNetDirINode);
    static ref PROC_SYS_DIR: Arc<dyn INode> = Arc::new(ProcSysDirINode);
    static ref PROC_SYS_KERNEL_DIR: Arc<dyn INode> = Arc::new(ProcSysKernelDirINode);
    static ref PROC_PERF_PARANOID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 42,
        generate: proc_perf_event_paranoid_content,
    });
    static ref PROC_KPTR_RESTRICT: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 43,
        generate: proc_kptr_restrict_content,
    });
    static ref PROC_PERF_DIR: Arc<dyn INode> = Arc::new(ProcPerfDirINode);
    static ref PROC_PERF_SYSCALLS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 44,
        generate: proc_perf_syscalls_content,
    });
    static ref PROC_PERF_TASKS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 46,
        generate: proc_perf_tasks_content,
    });
    static ref PROC_PERF_TOP: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 47,
        generate: proc_perf_top_content,
    });
    static ref PROC_PERF_KERNEL: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 48,
        generate: proc_perf_kernel_content,
    });
    static ref PROC_SELF_SYM: Arc<dyn INode> = Arc::new(ProcSelfSymINode);
    static ref PROC_MEMINFO: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 11,
        generate: proc_meminfo_content,
    });
    static ref PROC_SYSCALLS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 137,
        generate: proc_syscalls_content,
    });
    static ref PROC_MEMHOGS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 109,
        generate: proc_memhogs_content,
    });
    static ref PROC_KHEAP: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 111,
        generate: proc_kheap_content,
    });
    static ref PROC_CPUINFO: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 12,
        generate: proc_cpuinfo_content,
    });
    /// `/proc/gpudbg` — on-demand, read-only GPU register/state dump for the GPU
    /// copy-engine bring-up. Re-reads live each `cat`, so it doubles as the dev
    /// loop: change the driver's `debug_dump`, rebuild, `cat /proc/gpudbg`.
    /// `/proc/oops` — contained kernel faults, recorded in RAM by the fault
    /// path (which cannot touch a filesystem) and drained to
    /// `/var/log/oops.log` by userspace. See `kernel_hal::oops_log`.
    static ref PROC_OOPS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 121,
        generate: proc_oops_content,
    });
    static ref PROC_GPUDBG: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 99,
        generate: proc_gpudbg_content,
    });
    /// `/proc/bootprofile` — boot-time file-access trace + desktop preload list.
    static ref PROC_BOOTPROFILE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 131,
        generate: proc_bootprofile_content,
    });
    /// `/proc/gpustep2` — opt-in: each read performs Step 2 (instance block +
    /// GMMU flush) on the non-console GPU and reports the result.
    static ref PROC_GPUSTEP2: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 98,
        generate: proc_gpustep2_content,
    });
    /// `/proc/gpustep3` — opt-in: doorbell-enable + runlist commit on the
    /// non-console GPU. Inodes 97/98 are deliberately below the `100 + pid`
    /// per-process inode range to avoid colliding with process directories.
    static ref PROC_GPUSTEP3: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 97,
        generate: proc_gpustep3_content,
    });
    /// `/proc/gpustep4` — opt-in: ring doorbell + SET_OBJECT on the non-console GPU.
    static ref PROC_GPUSTEP4: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 96,
        generate: proc_gpustep4_content,
    });
    /// `/proc/gpustep5` — opt-in: real vendored RM core attach
    /// (`nvidia_rm_sys::rm_init::attach_gpu`). Moved out of `/proc/gpudbg`
    /// after it hung real hardware on a plain `cat`.
    static ref PROC_GPUSTEP5: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 95,
        generate: proc_gpustep5_content,
    });
    /// `/proc/gpustep6` — opt-in: real `kgspInitRm` GSP-RM boot. Requires
    /// `/proc/gpustep5` to have succeeded first.
    static ref PROC_GPUSTEP6: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 94,
        generate: proc_gpustep6_content,
    });
    /// `/proc/gpustep7` â GSP static-info readback (see proc_gpustep7_content).
    static ref PROC_GPUSTEP7: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 93,
        generate: proc_gpustep7_content,
    });
    /// `/proc/gpustep8` â live-GSP RM API control demo (see proc_gpustep8_content).
    static ref PROC_GPUSTEP8: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 92,
        generate: proc_gpustep8_content,
    });
    /// `/proc/gpustep9` â full device state bring-up (see proc_gpustep9_content).
    static ref PROC_GPUSTEP9: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 91,
        generate: proc_gpustep9_content,
    });
    /// `/proc/gpustep10` -- CE data-movement verify (see proc_gpustep10_content).
    static ref PROC_GPUSTEP10: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 90,
        generate: proc_gpustep10_content,
    });
    /// `/proc/gpustep11` -- console-GPU GSP boot, console frozen (see
    /// proc_gpustep11_content).
    static ref PROC_GPUSTEP11: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 89,
        generate: proc_gpustep11_content,
    });
    /// `/proc/gpustep12` -- EXP1 console-GPU GSP boot, PDISP held in reset
    /// (see proc_gpustep12_content).
    static ref PROC_GPUSTEP12: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 88,
        generate: proc_gpustep12_content,
    });
    /// `/proc/gpustep13` -- EXP2 console-GPU GSP boot, pre-STARTCPU interrupt
    /// drain (see proc_gpustep13_content).
    static ref PROC_GPUSTEP13: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 86,
        generate: proc_gpustep13_content,
    });
    /// `/proc/gpustep14` -- console GPU full bring-up chain (see
    /// proc_gpustep14_content).
    static ref PROC_GPUSTEP14: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 85,
        generate: proc_gpustep14_content,
    });
    /// `/proc/gpustep15` -- GR engine GPC/TPC/SM config probe (see
    /// proc_gpustep15_content).
    static ref PROC_GPUSTEP15: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 84,
        generate: proc_gpustep15_content,
    });
    /// `/proc/gpustep16` -- GR allocation ladder (see proc_gpustep16_content).
    static ref PROC_GPUSTEP16: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 83,
        generate: proc_gpustep16_content,
    });
    /// `/proc/gpustep17` -- compute channel bring-up (see proc_gpustep17_content).
    static ref PROC_GPUSTEP17: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 81,
        generate: proc_gpustep17_content,
    });
    /// `/proc/gpustep18` -- first Eclipse-authored GPU submission (see
    /// proc_gpustep18_content).
    static ref PROC_GPUSTEP18: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 80,
        generate: proc_gpustep18_content,
    });
    /// `/proc/gpustep19` -- first real compute launch (see
    /// proc_gpustep19_content).
    static ref PROC_GPUSTEP19: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 79,
        generate: proc_gpustep19_content,
    });
    /// `/proc/gpustep20` -- kernel store, triple verified (see
    /// proc_gpustep20_content).
    static ref PROC_GPUSTEP20: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 78,
        generate: proc_gpustep20_content,
    });
    /// `/proc/gpustep21` -- 32-thread kernel, per-thread verified (see
    /// proc_gpustep21_content).
    static ref PROC_GPUSTEP21: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 77,
        generate: proc_gpustep21_content,
    });
    /// `/proc/gpustep22` -- chip-scale grid, verified (see
    /// proc_gpustep22_content).
    static ref PROC_GPUSTEP22: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 76,
        generate: proc_gpustep22_content,
    });
    /// `/proc/gpustep23` -- integer SAXPY (see proc_gpustep23_content).
    static ref PROC_GPUSTEP23: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 75,
        generate: proc_gpustep23_content,
    });
    /// `/proc/gpuinit` -- one-cat compute bring-up chain (steps 5,6,9,16,17).
    static ref PROC_GPUINIT: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 101,
        generate: proc_gpuinit_content,
    });
    /// `/proc/gpubench` -- integer-ALU GIOPS benchmark.
    static ref PROC_GPUBENCH: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 102,
        generate: proc_gpubench_content,
    });
    /// `/proc/gpuedid` -- real display query (connectors + EDID).
    static ref PROC_GPUEDID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 103,
        generate: proc_gpuedid_content,
    });
    /// `/proc/gpusnd` -- what every audio device and its codec are actually
    /// doing, plus whether the GPU display engine was ever told to transmit
    /// audio. The dump that tells silence-with-no-error apart from a genuinely
    /// dead stream.
    static ref PROC_GPUSND: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 141,
        generate: proc_gpusnd_content,
    });
    /// `/proc/gpusurvive` -- CMOS survival breadcrumb from the previous
    /// console-GPU boot attempt (see proc_gpusurvive_content).
    static ref PROC_GPUSURVIVE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 104,
        generate: proc_gpusurvive_content,
    });
    /// `/proc/gpucefill` -- CE-offload visual test (see proc_gpucefill_content).
    static ref PROC_GPUCEFILL: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 105,
        generate: proc_gpucefill_content,
    });
    /// `/proc/gpucefillp2p` -- P2P CE-offload visual test (see
    /// proc_gpucefillp2p_content).
    static ref PROC_GPUCEFILLP2P: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 106,
        generate: proc_gpucefillp2p_content,
    });
    /// `/proc/gpuroles` -- console vs compute NVIDIA GPU map.
    static ref PROC_GPUROLES: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 107,
        generate: proc_gpuroles_content,
    });
    /// `/proc/usbhid` -- USB HID pointer/keyboard diagnostics (see
    /// proc_usbhid_content). `cat` it from a text VT to debug a dead mouse.
    static ref PROC_USBHID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 142,
        generate: proc_usbhid_content,
    });
    /// `/proc/gpudump` -- read-only discriminating HW dump, both GPUs (see
    /// proc_gpudump_content).
    static ref PROC_GPUDUMP: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 87,
        generate: proc_gpudump_content,
    });
    static ref PROC_SWAPS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 18,
        generate: proc_swaps_content,
    });
    static ref PROC_UPTIME: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 13,
        generate: proc_uptime_content,
    });
    static ref PROC_MOUNTS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 14,
        generate: proc_mounts_content,
    });
    static ref PROC_STAT: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 15,
        generate: proc_stat_content,
    });
    static ref PROC_LOADAVG: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 16,
        generate: proc_loadavg_content,
    });
    static ref PROC_FILESYSTEMS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 49,
        generate: proc_filesystems_content,
    });
    static ref PROC_HUNTER: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 17,
        generate: proc_hunter_content,
    });
    static ref PROC_NET_DEV: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 30,
        generate: proc_net_dev_content,
    });
    static ref PROC_NET_ROUTE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 31,
        generate: proc_net_route_content,
    });
    static ref PROC_NET_ARP: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 32,
        generate: proc_net_arp_content,
    });
    static ref PROC_NET_IF_INET6: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 33,
        generate: proc_net_if_inet6_content,
    });
}

// Second block: lazy_static! is recursive over its items and the main block
// above is already at the macro recursion limit.
lazy_static! {
    static ref PROC_VERSION: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 50,
        generate: proc_version_content,
    });
    static ref PROC_CMDLINE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 110,
        generate: proc_cmdline_content,
    });
    static ref PROC_SYS_HOSTNAME: Arc<dyn INode> = Arc::new(ProcSysWritableINode {
        inode: 51,
        generate: proc_sys_hostname_content,
        store: store_hostname,
    });
    static ref PROC_SYS_DOMAINNAME: Arc<dyn INode> = Arc::new(ProcSysWritableINode {
        inode: 52,
        generate: proc_sys_domainname_content,
        store: store_domainname,
    });
    static ref PROC_SYS_OSTYPE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 53,
        generate: proc_sys_ostype_content,
    });
    static ref PROC_SYS_OSRELEASE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 54,
        generate: proc_sys_osrelease_content,
    });
    static ref PROC_SYS_VERSION: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 55,
        generate: proc_sys_version_content,
    });
    static ref PROC_SYS_PID_MAX: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 56,
        generate: proc_sys_pid_max_content,
    });
    static ref PROC_SYS_NGROUPS_MAX: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 57,
        generate: proc_sys_ngroups_max_content,
    });
    static ref PROC_SYS_THREADS_MAX: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 58,
        generate: proc_sys_threads_max_content,
    });
    static ref PROC_SYS_OVERFLOWUID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        // Its own: 107 is `/proc/gpuroles`'s.
        inode: 112,
        generate: proc_sys_overflowuid_content,
    });
    static ref PROC_SYS_OVERFLOWGID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 108,
        generate: proc_sys_overflowgid_content,
    });
    static ref PROC_SYS_KERNEL_RANDOM_DIR: Arc<dyn INode> = Arc::new(ProcSysKernelRandomDirINode);
    static ref PROC_SYS_BOOT_ID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 60,
        generate: proc_sys_boot_id_content,
    });
    static ref PROC_SYS_UUID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 61,
        generate: proc_sys_uuid_content,
    });
    static ref PROC_SYSVIPC_DIR: Arc<dyn INode> = Arc::new(ProcSysvipcDirINode);
    static ref PROC_SYSVIPC_MSG: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 22,
        generate: proc_sysvipc_msg_content,
    });
    static ref PROC_SYSVIPC_SEM: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 23,
        generate: proc_sysvipc_sem_content,
    });
    static ref PROC_SYSVIPC_SHM: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 24,
        generate: proc_sysvipc_shm_content,
    });
    static ref PROC_SYS_VM_DIR: Arc<dyn INode> = Arc::new(ProcSysVmDirINode);
    static ref PROC_SYS_OVERCOMMIT: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 63,
        generate: proc_sys_overcommit_content,
    });
    static ref PROC_SYS_MAX_MAP_COUNT: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 64,
        generate: proc_sys_max_map_count_content,
    });
    static ref PROC_SYS_FS_DIR: Arc<dyn INode> = Arc::new(ProcSysFsDirINode);
    static ref PROC_SYS_FILE_MAX: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 66,
        generate: proc_sys_file_max_content,
    });
    static ref PROC_SYS_NR_OPEN: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 67,
        generate: proc_sys_nr_open_content,
    });
    static ref PROC_SYS_PIPE_MAX_SIZE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 68,
        generate: proc_sys_pipe_max_size_content,
    });
    static ref PROC_NET_TCP: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 70,
        generate: proc_net_tcp_content,
    });
    static ref PROC_NET_UDP: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 71,
        generate: proc_net_udp_content,
    });
    static ref PROC_NET_UNIX: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 72,
        generate: proc_net_unix_content,
    });
    static ref PROC_KBD: Arc<dyn INode> = Arc::new(ProcSysWritableINode {
        inode: 73,
        generate: proc_kbd_content,
        store: store_kbd,
    });
}

#[cfg(test)]
mod pid_status_tests {
    //! `/proc/<pid>/status` said `Uid:\t0\t0\t0\t0` and `Gid:\t0\t0\t0\t0`
    //! for every process on the machine, whoever it belonged to. That line is
    //! what `polkit`, `pkexec`, `sudo` and `ps -u` read to learn who is
    //! asking, so every unprivileged process looked like root to them.

    use super::*;
    use crate::process::{Credentials, LinuxProcess};
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

    #[test]
    fn the_uid_and_gid_lines_are_the_process_s_own_ids_in_proc_5_order() {
        // Real, effective, saved, filesystem: the order proc(5) gives, and
        // the one `ps` and `polkit` parse by position.
        let lines = credential_lines(&creds());
        assert!(lines.starts_with("Uid:\t1000\t1001\t1002\t1003\nGid:\t2000\t2001\t2002\t2003\n"));
    }

    #[test]
    fn the_groups_line_lists_every_supplementary_group() {
        let lines = credential_lines(&creds());
        assert!(lines.ends_with("Groups:\t1000 4 24 \n"), "{:?}", lines);
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
            status.contains("PPid:\t0\nUid:\t1000\t1000\t1000\t1000\nGid:\t100\t100\t100\t100\nGroups:\t100 27 \nVmSize:"),
            "{:?}",
            status
        );
    }
}

#[cfg(test)]
mod pid_stat_tests {
    //! `/proc/<pid>/stat` fields 16/17 (cutime/cstime), 22 (starttime), 23
    //! (vsize) and 24 (rss) were written as 0 for every process, while
    //! `status`, `statm`, `getrusage` and `times()` already had the numbers:
    //! `ps aux` showed VSZ 0 and RSS 0, `ps -o etime` the uptime of the
    //! machine for everything, and `top`'s TIME+ never counted a reaped child.

    use super::*;
    use crate::process::{ChildCpu, LinuxProcess};
    use rcore_fs_ramfs::RamFS;
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
        proc.vmar()
            .map(None, vmo.clone(), 0, 3 * PAGE_SIZE, flags)
            .unwrap();
        let line = proc_pid_stat(&proc);
        assert_eq!(field(&line, 23), (3 * PAGE_SIZE) as i64, "{:?}", line);
        assert_eq!(field(&line, 24), 3, "{:?}", line);
        // And they agree with `statm`, which `top` reads: size and resident
        // there are both in pages.
        assert_eq!(proc_pid_statm(&proc), "3 3 0 0 0 0 0\n");
        // Mapped a second time the pages are shared, and shared pages are
        // resident too: `get_mm_rss` is file + anon + shmem, and `ps` would
        // otherwise show RSS 0 for a process that lives on shared memory.
        proc.vmar().map(None, vmo, 0, 3 * PAGE_SIZE, flags).unwrap();
        let line = proc_pid_stat(&proc);
        assert_eq!(field(&line, 23), (6 * PAGE_SIZE) as i64, "{:?}", line);
        assert_eq!(field(&line, 24), 6, "{:?}", line);
        assert_eq!(proc_pid_statm(&proc), "6 6 6 0 0 0 0\n");
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
}

#[cfg(test)]
mod proc_inode_tests {
    //! Every entry of `/proc` had to have an inode number of its own, and
    //! did not: `/proc/self` was `/proc/cpuinfo`'s, `/proc/sys/kernel/
    //! overflowuid` was `/proc/gpuroles`'s, `/proc/<pid>` sat on the fixed
    //! files' numbers, and the nine files under a process were one inode.

    use super::*;
    use crate::process::LinuxProcess;
    use alloc::collections::BTreeMap;
    use rcore_fs_ramfs::RamFS;

    /// Walk the fixed part of `/proc` (no process directories, no `self`),
    /// collecting `(path, inode)`.
    fn fixed_entries(dir: &Arc<dyn INode>, path: &str, out: &mut Vec<(String, usize)>) {
        let mut i = 0;
        while let Ok(name) = dir.get_entry(i) {
            i += 1;
            if name == "." || name == ".." || name == "self" || name.parse::<u64>().is_ok() {
                continue;
            }
            let child = dir
                .find(&name)
                .unwrap_or_else(|e| panic!("{}/{}: {:?}", path, name, e));
            let md = child.metadata().unwrap();
            let child_path = alloc::format!("{}/{}", path, name);
            out.push((child_path.clone(), md.inode));
            if md.type_ == FileType::Dir {
                fixed_entries(&child, &child_path, out);
            }
        }
    }

    fn duplicates(entries: &[(String, usize)]) -> Vec<(usize, Vec<String>)> {
        let mut by_inode: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (path, inode) in entries {
            by_inode.entry(*inode).or_default().push(path.clone());
        }
        by_inode
            .into_iter()
            .filter(|(_, paths)| paths.len() > 1)
            .collect()
    }

    #[test]
    fn every_fixed_file_of_proc_has_an_inode_of_its_own_below_the_pids() {
        let root: Arc<dyn INode> = PROC_ROOT.clone();
        let mut entries = alloc::vec![(String::from("/proc"), root.metadata().unwrap().inode)];
        fixed_entries(&root, "/proc", &mut entries);
        assert!(
            entries.len() > 50,
            "the walk saw the tree: {}",
            entries.len()
        );
        assert_eq!(duplicates(&entries), Vec::new());
        for (path, inode) in &entries {
            assert!(
                *inode < PID_INODE_BASE,
                "{} at {} is in the pid range",
                path,
                inode
            );
        }
        // The two that shared a number with another file.
        let inode_of = |name: &str| root.find(name).unwrap().metadata().unwrap().inode;
        assert_ne!(inode_of("self"), inode_of("cpuinfo"));
        assert_ne!(
            PROC_SYS_DIR
                .find("kernel")
                .unwrap()
                .find("overflowuid")
                .unwrap()
                .metadata()
                .unwrap()
                .inode,
            inode_of("gpuroles")
        );
    }

    #[test]
    fn a_process_s_directory_and_each_file_under_it_are_distinct_inodes() {
        let kinds = [
            ProcPidFileKind::Stat,
            ProcPidFileKind::Cmdline,
            ProcPidFileKind::Status,
            ProcPidFileKind::Perf,
            ProcPidFileKind::Maps,
            ProcPidFileKind::Comm,
            ProcPidFileKind::Environ,
            ProcPidFileKind::Statm,
            ProcPidFileKind::Threads,
        ];
        let inodes_of = |pid: u64| -> Vec<usize> {
            let mut v = alloc::vec![ProcPidDirINode { pid }.metadata().unwrap().inode];
            v.extend(
                kinds
                    .iter()
                    .map(|&kind| ProcPidFileINode { pid, kind }.metadata().unwrap().inode),
            );
            v.push(pid_inode(pid, PID_FD_DIR_SLOT));
            v
        };
        // The files answer `metadata` from the process, so it must exist.
        let pids = [4601u64, 4607, 4661];
        let _alive: Vec<Arc<Process>> = pids
            .iter()
            .map(|&pid| {
                // Under `ROOT_JOB`, where `/proc` looks processes up.
                Process::create_with_fixed_id_ext(
                    &ROOT_JOB,
                    pid,
                    "ino",
                    LinuxProcess::new(RamFS::new(), 0),
                )
                .unwrap()
            })
            .collect();
        let mut all: Vec<(String, usize)> = Vec::new();
        for pid in pids {
            for (i, inode) in inodes_of(pid).into_iter().enumerate() {
                assert!(
                    inode >= PID_INODE_BASE,
                    "pid {} slot {} at {}",
                    pid,
                    i,
                    inode
                );
                all.push((alloc::format!("{}/{}", pid, i), inode));
            }
        }
        assert_eq!(duplicates(&all), Vec::new());
    }

    #[test]
    fn the_fd_directory_of_a_process_is_its_own_inode_too() {
        // `/proc/61/fd` was `40 + 61 = 101`, the inode of `/proc/1`.
        let proc = Process::create_with_fixed_id_ext(
            &Job::root(),
            4501,
            "fd",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let fd_dir = super::super::proc_self::ProcSelfFdDir {
            process: proc.clone(),
        };
        let inode = fd_dir.metadata().unwrap().inode;
        assert_eq!(inode, pid_inode(4501, PID_FD_DIR_SLOT));
        assert_ne!(
            inode,
            ProcPidDirINode { pid: 4501 }.metadata().unwrap().inode
        );
        assert_ne!(
            inode,
            ProcPidDirINode { pid: 4461 }.metadata().unwrap().inode
        );
    }
}

#[cfg(test)]
mod stat_file_tests {
    //! `/proc/stat` said `btime 0`, `intr 0` and gave the number of live
    //! processes as `processes`: `ps -o lstart` dated every process to 1970
    //! and `vmstat` had no forks or interrupts per second to show.

    use super::*;
    use crate::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;

    fn line_value(text: &str, key: &str) -> u64 {
        text.lines()
            .find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|rest| rest.strip_prefix(' '))
            })
            .unwrap_or_else(|| panic!("{} in {:?}", key, text))
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn btime_is_the_wall_clock_minus_the_uptime() {
        assert_eq!(
            boot_time_secs(Duration::from_secs(1_700_000_100), Duration::from_secs(100)),
            1_700_000_000
        );
        // Seconds, whole: `ps` reads an integer.
        assert_eq!(
            boot_time_secs(Duration::from_millis(10_900), Duration::from_millis(400)),
            10
        );
        // A wall clock behind the uptime is a clock nobody set, not a wrap.
        assert_eq!(
            boot_time_secs(Duration::from_secs(5), Duration::from_secs(50)),
            0
        );
        // And the file carries it: what the two clocks say around the read.
        let before = boot_time_secs(
            kernel_hal::timer::timer_now_realtime(),
            kernel_hal::timer::timer_now(),
        );
        let btime = line_value(&proc_stat_content(), "btime");
        let after = boot_time_secs(
            kernel_hal::timer::timer_now_realtime(),
            kernel_hal::timer::timer_now(),
        );
        assert!(
            before.saturating_sub(1) <= btime && btime <= after + 1,
            "{}",
            btime
        );
        assert!(btime > 0);
    }

    #[test]
    fn processes_counts_the_forks_since_boot_not_the_processes_alive() {
        let before = line_value(&proc_stat_content(), "processes");
        let created = Process::create_with_fixed_id_ext(
            &Job::root(),
            4401,
            "stat",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let alive = all_processes().len() as u64;
        let with_one_more = line_value(&proc_stat_content(), "processes");
        assert!(
            with_one_more >= before + 1,
            "{} then {}",
            before,
            with_one_more
        );
        // It is a counter of births, so it does not track how many live.
        drop(created);
        assert!(line_value(&proc_stat_content(), "processes") >= with_one_more);
        assert!(
            with_one_more >= alive,
            "{} forks, {} alive",
            with_one_more,
            alive
        );
    }

    #[test]
    fn intr_is_the_interrupts_handled_since_boot() {
        let before = line_value(&proc_stat_content(), "intr");
        for _ in 0..3 {
            kernel_hal::kstats::note_irq(0xF2);
        }
        assert!(line_value(&proc_stat_content(), "intr") >= before + 3);
    }
}

/// `ipcs` walks `/proc/sysvipc/{msg,sem,shm}`; a table that is not there is
/// a mechanism whose objects nobody can list or clean up.
#[cfg(test)]
mod sysvipc_dir_tests {
    use super::*;

    fn read_all(inode: &Arc<dyn INode>) -> String {
        let mut buf = alloc::vec![0u8; 4096];
        let n = inode.read_at(0, &mut buf).unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    #[test]
    fn the_directory_lists_and_resolves_all_three_tables() {
        let dir = PROC_SYSVIPC_DIR.clone();
        let mut names = alloc::vec::Vec::new();
        let mut i = 0;
        while let Ok(name) = dir.get_entry(i) {
            names.push(name);
            i += 1;
        }
        for table in ["msg", "sem", "shm"] {
            assert!(names.iter().any(|n| n == table), "{} is listed", table);
            let inode = dir
                .find(table)
                .unwrap_or_else(|_| panic!("{} resolves", table));
            let text = read_all(&inode);
            assert!(
                text.starts_with("       key "),
                "{} reads as a kernel table:\n{}",
                table,
                text
            );
        }
        assert!(dir.find("nope").is_err());
    }

    #[test]
    fn each_table_has_its_own_inode() {
        let sem = PROC_SYSVIPC_DIR
            .find("sem")
            .unwrap()
            .metadata()
            .unwrap()
            .inode;
        let shm = PROC_SYSVIPC_DIR
            .find("shm")
            .unwrap()
            .metadata()
            .unwrap()
            .inode;
        let msg = PROC_SYSVIPC_DIR
            .find("msg")
            .unwrap()
            .metadata()
            .unwrap()
            .inode;
        assert!(sem != shm && shm != msg && sem != msg);
        assert!(
            read_all(&PROC_SYSVIPC_DIR.find("sem").unwrap()).contains("semid"),
            "sem is the semaphore table"
        );
        assert!(
            read_all(&PROC_SYSVIPC_DIR.find("shm").unwrap()).contains("shmid"),
            "shm is the shared-memory table"
        );
    }
}

#[cfg(test)]
mod dir_shape_tests {
    //! What a directory of `/proc` looks like to a walker: its entries and its
    //! link count. Both were wrong in ways only a tool notices.

    use super::*;

    fn names(dir: &Arc<dyn INode>) -> alloc::vec::Vec<String> {
        let mut out = alloc::vec::Vec::new();
        let mut i = 0;
        while let Ok(name) = dir.get_entry(i) {
            out.push(name);
            i += 1;
        }
        out
    }

    /// `/proc/net` was the one directory whose listing had no `.` and no `..`,
    /// so `ls -a` showed neither although `find()` resolved both.
    #[test]
    fn proc_net_lists_dot_and_dotdot_like_every_other_directory() {
        let dir = PROC_NET_DIR.clone();
        let names = names(&dir);
        assert_eq!(names.first().map(|s| s.as_str()), Some("."));
        assert_eq!(names.get(1).map(|s| s.as_str()), Some(".."));
        // And the tables are still all there, after the two.
        for table in ["dev", "route", "arp", "if_inet6", "tcp", "udp", "unix"] {
            assert!(names.iter().any(|n| n == table), "{} is listed", table);
        }
        assert_eq!(names.len(), 9);
    }

    /// Every listed name resolves, which is the property that makes a listing
    /// walkable: `fts(3)` looks up what `readdir` handed it.
    #[test]
    fn every_name_proc_net_lists_resolves() {
        let dir = PROC_NET_DIR.clone();
        for name in names(&dir) {
            assert!(dir.find(&name).is_ok(), "{} resolves", name);
        }
    }

    /// A live directory reports at least two links. Zero reads as removed, and
    /// `fts(3)` subtracts 2 from it to decide how far to descend.
    #[test]
    fn no_proc_directory_claims_to_have_no_links() {
        let dirs: alloc::vec::Vec<(&str, Arc<dyn INode>)> = alloc::vec![
            ("/proc", PROC_ROOT.clone()),
            ("/proc/net", PROC_NET_DIR.clone()),
            ("/proc/sysvipc", PROC_SYSVIPC_DIR.clone()),
            ("/proc/sys", PROC_SYS_DIR.clone()),
            ("/proc/perf", PROC_PERF_DIR.clone()),
        ];
        for (path, dir) in dirs {
            let m = dir.metadata().unwrap();
            assert_eq!(m.type_, FileType::Dir, "{} is a directory", path);
            assert!(m.nlinks >= 2, "{} has {} links", path, m.nlinks);
        }
    }

    /// And a directory that holds directories says so: `/proc/sys` has
    /// `kernel`, `vm` and `fs` under it, so 2 + 3.
    #[test]
    fn a_directory_counts_its_subdirectories_in_its_links() {
        assert_eq!(PROC_SYS_DIR.metadata().unwrap().nlinks, 5);
        assert_eq!(
            PROC_SYS_DIR
                .find("kernel")
                .unwrap()
                .metadata()
                .unwrap()
                .nlinks,
            3,
            "/proc/sys/kernel holds random"
        );
        assert_eq!(
            PROC_SYS_DIR.find("vm").unwrap().metadata().unwrap().nlinks,
            2,
            "/proc/sys/vm holds only files"
        );
    }
}

#[cfg(test)]
mod sysctl_tests {
    use super::*;

    #[test]
    fn writable_hostname_trims_newline_and_roundtrips() {
        let inode = ProcSysWritableINode {
            inode: 999,
            generate: proc_sys_hostname_content,
            store: store_hostname,
        };
        // `echo testbox > /proc/sys/kernel/hostname` sends the newline too.
        let written = inode.write_at(0, b"testbox\n").unwrap();
        assert_eq!(written, 8);
        assert_eq!(crate::uname::hostname(), "testbox");
        let mut buf = [0u8; 32];
        let read = inode.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..read], b"testbox\n");
        // Oversized names are rejected like sethostname(2) does.
        assert!(inode.write_at(0, &[b'a'; 65]).is_err());
    }

    #[test]
    fn uuid_v4_has_rfc4122_shape() {
        let s = format_uuid_v4([0u8; 16]);
        assert_eq!(s.len(), 36);
        let parts: alloc::vec::Vec<usize> = s.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        // Version nibble is 4; variant nibble is one of 8/9/a/b.
        assert_eq!(s.as_bytes()[14], b'4');
        assert!(matches!(s.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn writable_kbd_rejects_unknown_layout() {
        let inode = ProcSysWritableINode {
            inode: 998,
            generate: proc_kbd_content,
            store: store_kbd,
        };
        assert!(inode.write_at(0, b"de\n").is_err());
    }

    #[test]
    fn writable_kbd_truncate_then_echo_us() {
        let inode = ProcSysWritableINode {
            inode: 997,
            generate: proc_kbd_content,
            store: store_kbd,
        };
        assert!(inode.resize(0).is_ok());
        assert_eq!(inode.write_at(0, b"us\n").unwrap(), 3);
        assert_eq!(crate::fs::kbd_layout::current_name(), "us");
        assert!(inode.write_at(0, b"es\n").is_ok());
        assert_eq!(crate::fs::kbd_layout::current_name(), "es");
    }
}
