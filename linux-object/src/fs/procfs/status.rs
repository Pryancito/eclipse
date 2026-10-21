//! `/proc/<pid>/status` and `/proc/<pid>/task/<tid>/status`: the lines
//! `proc_pid_status()` writes in `fs/proc/array.c`, in its order. Split out of
//! `procfs.rs` for the size ratchet.
//!
//! The file used to stop at eleven lines. `Umask:`, `TracerPid:`, `FDSize:`,
//! `NSpid:`, the signal masks, the capability sets, `NoNewPrivs:` and
//! `Cpus_allowed_list:` were missing, so a program asking whether it is being
//! traced (`grep TracerPid`) found nothing to read, `ps s` had no masks to
//! show, and a check of `CapEff:` before trying something privileged read an
//! empty string. The memory lines Linux writes after `NSsid:` need numbers
//! this kernel does not keep yet.

use super::*;
use crate::fs::FileDesc;
use crate::process::LinuxProcess;
use crate::signal::{Signal, Sigset, SIG_DFL, SIG_IGN};
use crate::thread::ThreadExt;
use core::convert::TryFrom;
use core::fmt::Write;

/// The `State:` line of `/proc/<pid>/status` for a `/proc/<pid>/stat` state
/// letter.
///
/// The letter and the word have to agree, because different readers take
/// different halves: `procps` matches the word, `htop` the letter. Reaching
/// this table through [`proc_pid_status`] needs a process whose leader thread
/// is in the state in question, and a thread only reads `Suspended` once it
/// has a context, which no test can give it -- so `T` was the one row nothing
/// could ever ask for. As an argument it is four rows of a table.
fn status_state_text(letter: char) -> &'static str {
    match letter {
        'R' => "R (running)",
        'T' => "T (stopped)",
        'Z' => "Z (zombie)",
        _ => "S (sleeping)",
    }
}

pub(super) fn proc_pid_status(proc: &Process) -> String {
    proc_task_status(proc, None)
}

/// `/proc/<pid>/status`, and `/proc/<pid>/task/<tid>/status` when `thread`
/// names one.
///
/// `Name:` and `State:` are the task's own, `Tgid:` is always the process and
/// `Pid:` the task -- which is the whole point of those two lines being
/// separate, and is how a reader holding a tid tells which group it is in.
/// The pending and blocked signals and the allowed CPUs are the task's too,
/// the leader's for the process's file. Everything else belongs to the
/// process, so it is the same for every thread, as in Linux.
pub(super) fn proc_task_status(proc: &Process, thread: Option<&Arc<Thread>>) -> String {
    let pid = proc.id();
    let tid = thread.map_or(pid, |t| t.id());
    let task = thread.cloned().or_else(|| proc_first_thread(proc));
    let name = escaped_comm(&match thread {
        Some(t) => thread_comm(proc, t),
        None => proc_comm(proc),
    });
    let state = status_state_text(match thread {
        Some(t) => task_state_char(proc, t.state()),
        None => proc_state_char(proc),
    });
    // A zombie has given up its `fs_struct`, its descriptor table and its
    // memory (`exit_fs`, `exit_files`, `exit_mm`): Linux writes no `Umask:`
    // for it, an `FDSize:` of 0 and no `Vm` lines.
    let alive = !matches!(proc.status(), Status::Exited(_));
    let lp = proc.try_linux();
    let creds = lp.map(|lp| lp.credentials());

    let mut out = format!("Name:\t{}\n", name);
    if let Some(creds) = creds.as_ref().filter(|_| alive) {
        // `%#04o`: 022 is written `0022`.
        let _ = writeln!(out, "Umask:\t{:04o}", creds.umask);
    }
    // No NUMA balancing, so no `Ngid:` other than 0; no `ptrace`, so nobody
    // is ever tracing anyone.
    let _ = write!(
        out,
        "State:\t{}\nTgid:\t{}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\n",
        state,
        pid,
        tid,
        proc_ppid(proc)
    );
    if let (Some(lp), Some(creds)) = (lp, creds.as_ref()) {
        let fd_size = match alive {
            true => fd_table_size(lp.get_files().ok().and_then(|f| f.keys().max().copied())),
            false => 0,
        };
        id_lines(&mut out, creds, fd_size);
    }
    // One pid namespace: each of these is one number, the same one `getpid`,
    // `gettid`, `getpgid` and `getsid` answer.
    let pgid = crate::process::get_process_pgid(pid).unwrap_or(pid);
    let sid = crate::process::get_process_sid(pid).unwrap_or(pid);
    let _ = write!(
        out,
        "NStgid:\t{}\nNSpid:\t{}\nNSpgid:\t{}\nNSsid:\t{}\nKthread:\t0\n",
        pid, tid, pgid, sid
    );
    if alive {
        // VmSize = total mapped address space; VmRSS = committed (resident)
        // bytes, private + shared — the fields `ps`/`top`/OOM-watchers read.
        let stats = proc.vmar().get_task_stats();
        let vm_size_kb = stats.mapped_bytes() / 1024;
        let vm_rss_kb = (stats.private_bytes() + stats.shared_bytes()) / 1024;
        let _ = write!(
            out,
            "VmSize:\t{:8} kB\nVmRSS:\t{:8} kB\n",
            vm_size_kb, vm_rss_kb
        );
    }
    let _ = writeln!(out, "Threads:\t{}", proc.thread_ids().len().max(1));
    if let (Some(lp), Some(creds)) = (lp, creds.as_ref()) {
        signal_lines(&mut out, lp, task.as_ref(), creds.ruid);
        capability_lines(&mut out, creds.euid);
        let _ = writeln!(out, "NoNewPrivs:\t{}", lp.no_new_privs() as u8);
    }
    // No `Seccomp:` lines: `prctl(PR_GET_SECCOMP)` answers as a kernel built
    // without `CONFIG_SECCOMP`, and that kernel does not write them. The
    // speculation lines are what Linux writes when `PR_GET_SPECULATION_CTRL`
    // is `EINVAL`, as it is here.
    out.push_str("Speculation_Store_Bypass:\tunknown\nSpeculationIndirectBranch:\tunknown\n");
    let online = kernel_hal::cpu_online_mask();
    let allowed = task.as_ref().map_or(online, |t| t.affinity());
    cpu_lines(&mut out, allowed, online);
    // `task_context_switch_counts()`: the task's own, the leader's for the
    // process's file, since Linux counts them per task.
    let (voluntary, involuntary) = task.as_ref().map_or((0, 0), |t| t.context_switches());
    let _ = write!(
        out,
        "voluntary_ctxt_switches:\t{}\nnonvoluntary_ctxt_switches:\t{}\n",
        voluntary, involuntary
    );
    out
}

/// The `Uid:`, `Gid:`, `FDSize:` and `Groups:` lines, in the order
/// `task_state()` writes them. The ids go real, effective, saved,
/// filesystem, as proc(5) gives them. `Groups:` separates the ids with a
/// space and ends in one, with no groups too: `Groups:\t \n`.
fn id_lines(out: &mut String, creds: &crate::process::Credentials, fd_size: usize) {
    let _ = write!(
        out,
        "Uid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nGroups:\t",
        creds.ruid,
        creds.euid,
        creds.suid,
        creds.fsuid,
        creds.rgid,
        creds.egid,
        creds.sgid,
        creds.fsgid,
        fd_size
    );
    for (i, group) in creds.groups.iter().enumerate() {
        let _ = write!(out, "{}{}", if i == 0 { "" } else { " " }, group);
    }
    out.push_str(" \n");
}

/// `FDSize:`, the number of slots in the descriptor table, given the highest
/// descriptor open.
///
/// Linux's table starts with 64 (`NR_OPEN_DEFAULT`) and `alloc_fdtable()`
/// grows it to a power of two of 128 that holds the descriptor asked for: a
/// shell, which keeps its terminal on fd 255, reads 256. This table has no
/// slots to count, so the size is the one Linux's would have for the
/// descriptors open now; Linux's never shrinks.
fn fd_table_size(highest_fd: Option<FileDesc>) -> usize {
    match highest_fd.map(usize::from) {
        Some(fd) if fd >= 64 => (fd / 128 + 1).next_power_of_two() * 128,
        _ => 64,
    }
}

/// `SigQ:` to `SigCgt:`, as `task_sig()` writes them.
///
/// `SigQ:` is the signals pending for every task of the process's REAL
/// user, over that user's `RLIMIT_SIGPENDING` -- proc(5) says the user's,
/// not the process's. `SigPnd:` and `SigBlk:` are `task`'s own; `ShdPnd:` is
/// the process's, the signals sent to it that no thread has taken yet.
fn signal_lines(out: &mut String, lp: &LinuxProcess, task: Option<&Arc<Thread>>, ruid: u32) {
    let (pending, blocked) = task
        .and_then(|t| t.try_lock_linux().map(|lt| lt.get_signal_info()))
        .map_or((Sigset::default(), Sigset::default()), |(p, b, _)| (p, b));
    let (mut ignored, mut caught) = (Sigset::default(), Sigset::default());
    for sig in (1..=Signal::RTMAX as u8).filter_map(|n| Signal::try_from(n).ok()) {
        match lp.signal_action(sig).handler {
            SIG_IGN => ignored.insert(sig),
            SIG_DFL => {}
            _ => caught.insert(sig),
        }
    }
    let limit = lp
        .rlimit(crate::process::RLIMIT_SIGPENDING, None, false)
        .map_or(0, |l| l.cur);
    let _ = writeln!(out, "SigQ:\t{}/{}", signals_pending_for_user(ruid), limit);
    for (key, set) in [
        ("SigPnd", pending),
        ("ShdPnd", lp.shared_pending()),
        ("SigBlk", blocked),
        ("SigIgn", ignored),
        ("SigCgt", caught),
    ] {
        // `render_sigset_t()`: the 64 bits as 16 hex digits, bit `n - 1` for
        // signal `n` -- the same word `sigset_t` holds.
        let _ = writeln!(out, "{}:\t{:016x}", key, set.val());
    }
}

/// The first half of `SigQ:`: signals pending on every live task of real
/// user `uid`, thread-directed and process-directed alike. Linux keeps the
/// count as it queues and dequeues; this kernel counts when asked.
fn signals_pending_for_user(uid: u32) -> u64 {
    signals_pending_for_user_in(&ROOT_JOB, uid)
}

/// [`signals_pending_for_user`] over one job's subtree, for the tests.
fn signals_pending_for_user_in(job: &Arc<Job>, uid: u32) -> u64 {
    crate::process::all_live_processes_in(job)
        .iter()
        .filter_map(|proc| {
            let lp = proc.try_linux().filter(|lp| lp.uid() == uid)?;
            let threads: u64 = proc
                .threads()
                .iter()
                .filter_map(|t| t.try_lock_linux().map(|lt| lt.get_signal_info().0))
                .map(|set| u64::from(set.val().count_ones()))
                .sum();
            Some(threads + u64::from(lp.shared_pending().val().count_ones()))
        })
        .sum()
}

/// `CapInh:` to `CapAmb:`. The sets are the ones `capget(2)` publishes
/// (root holds every capability, anybody else none, nothing is inheritable),
/// and the bounding set is every capability: `prctl(PR_CAPBSET_READ)` answers
/// 1 for each one there is.
fn capability_lines(out: &mut String, euid: u32) {
    let held = crate::process::published_capabilities(euid);
    let bounding = (1u64 << (crate::process::CAP_LAST_CAP + 1)) - 1;
    let _ = write!(
        out,
        "CapInh:\t{:016x}\nCapPrm:\t{:016x}\nCapEff:\t{:016x}\nCapBnd:\t{:016x}\nCapAmb:\t{:016x}\n",
        0, held, held, bounding, 0
    );
}

/// `Cpus_allowed:` to `Mems_allowed_list:`. The CPU mask is the one
/// `sched_getaffinity` answers, over as many CPUs as there are; memory is a
/// single node, as on a kernel built without NUMA.
fn cpu_lines(out: &mut String, allowed: u64, online: u64) {
    let cpus = (u64::BITS - online.leading_zeros()).max(1);
    let allowed = allowed & online;
    let _ = write!(
        out,
        "Cpus_allowed:\t{}\nCpus_allowed_list:\t{}\nMems_allowed:\t1\nMems_allowed_list:\t0\n",
        bitmap_hex(allowed, cpus),
        bitmap_list(allowed, cpus)
    );
}

/// `%*pb`, `bitmap_string()` in `lib/vsprintf.c`: the bitmap in hex, in
/// 32-bit groups from the top, separated by commas, the first group only as
/// many digits wide as the bits it holds. Eight CPUs are `ff`, forty are
/// `ff,ffffffff`.
fn bitmap_hex(mask: u64, bits: u32) -> String {
    let groups = bits.div_ceil(32);
    let mut out = String::new();
    for group in (0..groups).rev() {
        let width = (bits - 32 * group).min(32);
        let value = (mask >> (32 * group)) & ((1u64 << width) - 1);
        if group + 1 != groups {
            out.push(',');
        }
        let _ = write!(out, "{:0w$x}", value, w = width.div_ceil(4) as usize);
    }
    out
}

/// `%*pbl`, `bitmap_list_string()`: the set bits as ranges, `0-3,6`.
fn bitmap_list(mask: u64, bits: u32) -> String {
    let mut out = String::new();
    let mut bit = 0;
    while bit < bits {
        if mask >> bit & 1 == 0 {
            bit += 1;
            continue;
        }
        let first = bit;
        while bit + 1 < bits && mask >> (bit + 1) & 1 == 1 {
            bit += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        let _ = match first == bit {
            true => write!(out, "{}", first),
            false => write!(out, "{}-{}", first, bit),
        };
        bit += 1;
    }
    out
}

#[cfg(test)]
mod tests;
