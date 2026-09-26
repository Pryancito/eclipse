//! Linux Process

use crate::{
    error::{LxError, LxResult},
    fs::{File, FileDesc, FileLike, OpenFlags},
    ipc::*,
    loader::AuxIdentity,
    net::SOCKET_FD,
    signal::{SigInfo, Signal as LinuxSignal, SignalAction, Sigset},
};
use alloc::{
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::convert::TryFrom;
use core::sync::atomic::{AtomicI32, Ordering};
use hashbrown::{HashMap, HashSet};
use kernel_hal::sync::{Mutex, MutexGuard};
use kernel_hal::VirtAddr;
use rcore_fs::vfs::{FileSystem, FileType, INode, Metadata};

use zircon_object::{
    object::{KernelObject, KoID, Signal},
    signal::{Futex, FutexTable},
    task::{
        Job, Process, Status, Thread, ROOT_JOB, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO,
        SCHED_IDLE, SCHED_NORMAL, SCHED_RR,
    },
    ZxError, ZxResult,
};

pub use rcore_fs::vfs::FsInfo;

/// Process extension for linux
pub trait ProcessExt {
    /// create Linux process with a fixed Linux PID (`pid`): 1 for init, or the
    /// reserved 101.. range for the per-terminal shells.
    fn create_linux(
        job: &Arc<Job>,
        rootfs: Arc<dyn FileSystem>,
        vt: usize,
        shared_root: Option<Arc<dyn INode>>,
        pid: KoID,
    ) -> ZxResult<Arc<Self>>;
    /// get linux process
    fn linux(&self) -> &LinuxProcess;
    /// Like [`linux`](Self::linux) but returns `None` instead of panicking when
    /// the extension is not a [`LinuxProcess`]. Use this on the lock-free
    /// process-walk / reaper / signal-callback paths: racing against a process
    /// that is going away (or whose extension cannot be resolved during SMP
    /// teardown churn) must degrade to "skip it", not bring down the kernel.
    fn try_linux(&self) -> Option<&LinuxProcess>;
    /// fork from current linux process
    fn fork_from(parent: &Arc<Self>) -> ZxResult<Arc<Self>>;
}

const ROOT_UID: u32 = 0;

// The capability numbers this kernel names, from `include/uapi/linux/
// capability.h`. Only the ones a gate actually asks about are here: a number
// nobody asks about is a number nobody can get wrong, and is exactly the kind
// of write-only constant this change exists to remove.

/// `CAP_KILL`: signal a process none of whose ids you hold.
pub const CAP_KILL: u32 = 5;
/// `CAP_SETGID`: change group ids, and set the supplementary group list.
pub const CAP_SETGID: u32 = 6;
/// `CAP_SETUID`: change user ids, `setfsuid(2)` included.
pub const CAP_SETUID: u32 = 7;
/// `CAP_SYS_PTRACE`: trace, inspect or reach into the innards of a process
/// that is not yours -- `pidfd_getfd(2)` included.
pub const CAP_SYS_PTRACE: u32 = 19;
/// `CAP_SYS_ADMIN`: the catch-all -- mount, `sethostname`, and much else.
pub const CAP_SYS_ADMIN: u32 = 21;
/// `CAP_SYS_BOOT`: `reboot(2)` and `kexec_load(2)`.
pub const CAP_SYS_BOOT: u32 = 22;
/// `CAP_SYS_NICE`: raise a task's priority, and set the scheduling
/// parameters or CPU affinity of a task that is not yours.
pub const CAP_SYS_NICE: u32 = 23;
/// `CAP_SYS_RESOURCE`: raise a hard resource limit, and reach into another
/// process's limits.
pub const CAP_SYS_RESOURCE: u32 = 24;
/// `CAP_SYS_TIME`: set the system clock and discipline it.
pub const CAP_SYS_TIME: u32 = 25;

/// The highest capability number this kernel reports. 40 is Linux 5.15's
/// `CAP_LAST_CAP` (`CAP_CHECKPOINT_RESTORE`); `capget`, `prctl`'s bounding
/// set and [`LinuxProcess::capable`] all measure against this one number.
pub const CAP_LAST_CAP: u32 = 40;

/// Whether one id pair holds **every** id of `target` -- all three uids and all
/// three gids.
///
/// The six comparisons of `check_prlimit_permission()` and
/// `__ptrace_may_access()`, which are the same six lines written twice in
/// Linux and once here. They are not "same user": they are "that process holds
/// no id I do not already have", so a target part-way through a set-user-ID
/// dance is out of reach even for the user who started it.
///
/// What the three callers disagree about is **which pair of the caller's ids**
/// they hand in, and that one argument is the whole difference between them:
/// the real pair for `prlimit64` and `pidfd_getfd`
/// (`PTRACE_MODE_*_REALCREDS`), the filesystem pair for the gated files of
/// `/proc/<pid>/` (`PTRACE_MODE_READ_FSCREDS`). Passing the wrong one reads as
/// correct code, so each caller says which it means in one line and there is a
/// test per caller that moves the two apart.
fn holds_every_id_of(caller_uid: u32, caller_gid: u32, target: &Credentials) -> bool {
    caller_uid == target.ruid
        && caller_uid == target.euid
        && caller_uid == target.suid
        && caller_gid == target.rgid
        && caller_gid == target.egid
        && caller_gid == target.sgid
}

/// Whether a process with effective uid `euid` holds capability `cap`.
///
/// Split from the process so the rule can be read on its own: it is the
/// whole of this kernel's privilege model, and both [`LinuxProcess::capable`]
/// and [`published_capabilities`] are it.
pub fn has_capability(euid: u32, cap: u32) -> bool {
    cap <= CAP_LAST_CAP && euid == ROOT_UID
}

/// The capability set `capget(2)` reports for a process with this effective
/// uid, as the bitmap userspace reads.
///
/// Built out of [`has_capability`] one bit at a time rather than written down
/// as a constant, so the set the kernel *publishes* cannot drift from the one
/// it *honours*.
pub fn published_capabilities(euid: u32) -> u64 {
    (0..=CAP_LAST_CAP)
        .filter(|&cap| has_capability(euid, cap))
        .fold(0u64, |set, cap| set | 1u64 << cap)
}

const NO_ID: u32 = u32::MAX;
const ACCESS_WRITE: u16 = 0o2;
const ACCESS_EXEC: u16 = 0o1;
const MODE_PERM_MASK: u16 = 0o7777;
const MODE_SET_UID: u16 = 0o4000;
const MODE_SET_GID: u16 = 0o2000;
/// The group-execute bit. `S_ISGID` means set-group-ID only when it is set
/// too; on a file without it the bit is the mandatory-locking convention.
const MODE_EXEC_GRP: u16 = 0o0010;
const MODE_STICKY: u16 = 0o1000;

#[derive(Clone, Debug)]
pub struct Credentials {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    /// The id a FILESYSTEM access is checked against. Linux keeps it apart
    /// from `euid` on purpose -- `generic_permission`'s own comment says why:
    /// "We use `fsuid` for this, letting us set arbitrary permissions for
    /// filesystem access without changing the 'normal' uids which are used
    /// for other things." Every `set*id` call drags it along, so it equals
    /// `euid` unless `setfsuid(2)` has moved it on its own.
    pub fsuid: u32,
    /// The group half of the same story, and what `in_group_p()` asks.
    pub fsgid: u32,
    pub groups: Vec<u32>,
    pub umask: u16,
}

impl Credentials {
    /// Move the effective uid, and the filesystem uid with it.
    ///
    /// The two fields are one decision in Linux -- every `set*id` path ends
    /// `new->fsuid = new->euid;` -- so writing `euid` on its own is the bug
    /// this pair exists to make impossible. `setfsuid(2)` is the only caller
    /// that moves `fsuid` alone, and it says so by name.
    pub fn set_euid(&mut self, uid: u32) {
        self.euid = uid;
        self.fsuid = uid;
    }

    /// Move the effective gid, and the filesystem gid with it.
    pub fn set_egid(&mut self, gid: u32) {
        self.egid = gid;
        self.fsgid = gid;
    }
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            ruid: ROOT_UID,
            euid: ROOT_UID,
            suid: ROOT_UID,
            rgid: ROOT_UID,
            egid: ROOT_UID,
            sgid: ROOT_UID,
            fsuid: ROOT_UID,
            fsgid: ROOT_UID,
            groups: vec![ROOT_UID],
            umask: 0o022,
        }
    }
}

impl ProcessExt for Process {
    fn create_linux(
        job: &Arc<Job>,
        rootfs: Arc<dyn FileSystem>,
        vt: usize,
        shared_root: Option<Arc<dyn INode>>,
        pid: KoID,
    ) -> ZxResult<Arc<Self>> {
        let linux_proc = match shared_root {
            Some(root) => LinuxProcess::with_root(root, vt),
            None => LinuxProcess::new(rootfs, vt),
        };
        // Each process is given an explicit, stable Linux PID by the boot code:
        // 1 for init and the reserved 101.. range for the per-terminal shells.
        // (Reusing PID 1 for every VT shell once made `top`/`ps` list PID 1 N
        // times and made `find_process(1)`, signals, `kill` and `/proc/1` all
        // resolve to whichever process happened to be enumerated first.)
        let proc = Process::create_with_fixed_id_ext(job, pid, "root", linux_proc)?;
        let weak_proc = Arc::downgrade(&proc);
        proc.add_signal_callback(Box::new(move |signal| {
            if signal.contains(Signal::PROCESS_TERMINATED) {
                if let Some(proc) = weak_proc.upgrade() {
                    // Reparent any still-live children to INIT before tearing
                    // this process down, so they are not stranded on a dead
                    // parent that will never `wait` for them.
                    reparent_live_children_to_init(&proc);
                    // Record locks die with the process (same as the
                    // fork path below; see `record_lock::release_owner`).
                    crate::fs::record_lock::release_owner(proc.id());
                    // try_linux (not linux): this callback runs from the
                    // object layer on PROCESS_TERMINATED, concurrently with
                    // SMP teardown churn. If the extension can no longer be
                    // resolved, skip the cleanup rather than panic the kernel.
                    if let Some(lp) = proc.try_linux() {
                        // Take the file table out and drop it AFTER the lock is
                        // released — file teardown can re-enter this process's
                        // accessors (see close_file).
                        let dropped_files = {
                            let mut inner = lp.inner.lock();
                            let files = core::mem::take(&mut inner.files);
                            inner.cloexec_fds.clear();
                            inner.futexes.clear();
                            inner.semaphores = Default::default();
                            inner.shm_identifiers = Default::default();
                            files
                        };
                        drop(dropped_files);
                    }
                }
                return true;
            }
            false
        }));
        Ok(proc)
    }

    fn linux(&self) -> &LinuxProcess {
        // A failed downcast here is "impossible" (every process the Linux layer
        // creates carries the LinuxProcess ext, and ext is immutable) — so if
        // it ever fires it means either a kernel-internal Zircon process leaked
        // into a Linux-only path, or the ext Box was corrupted. Name the
        // process so the report is actionable instead of a bare unwrap panic.
        self.ext()
            .downcast_ref::<LinuxProcess>()
            .unwrap_or_else(|| {
                // Enumeration says this is UNREACHABLE by construction in a
                // `linux` build: process.rs:109 (create_with_fixed_id_ext) and
                // :212 (create_with_ext, the fork path) are the only creators
                // and both pass a LinuxProcess by value; `ext` is written once
                // in the constructor, never replaced, and Process has no Drop.
                // `Process::create` (ext = ()) is `zircon`-feature only. So a
                // failure here is NOT "a kernel process wandered in" -- it is
                // the ext fat pointer having been CORRUPTED. Dump both of its
                // words: a plausible-looking vtable pointer means a structured
                // overwrite by another allocation, zeros or garbage mean a
                // spray. That distinction is what makes the next occurrence
                // actionable, so print it before dying.
                let fat: [usize; 2] = unsafe {
                    core::mem::transmute::<&dyn core::any::Any, [usize; 2]>(
                        &**self.ext() as &dyn core::any::Any
                    )
                };
                // What the field held when the process was built, snapshotted
                // before it was published to its job. Comparing against it says
                // which word moved -- and a match would mean the field was
                // never a LinuxProcess, disproving corruption entirely.
                let (born_data, born_vtable) = self.ext_born();
                // The vtable's own header identifies the concrete type without
                // a symbol table from a matching build: `size` and `align`
                // belong to the type, not to the link.
                let vt = zircon_object::task::vtable_info(fat[1]);
                // The canonical `LinuxProcess` vtable IN THIS BUILD, built from
                // a null thin pointer (never dereferenced -- only the vtable
                // word is read). If this equals the observed vtable then the
                // ext IS a LinuxProcess and the DOWNCAST is what is wrong, not
                // the field; the two TypeIds below then say why.
                let want_vtable: usize = {
                    let p: *const LinuxProcess = core::ptr::null();
                    let d: *const dyn core::any::Any = p;
                    unsafe { core::mem::transmute::<*const dyn core::any::Any, [usize; 2]>(d)[1] }
                };
                let want_id = core::any::TypeId::of::<LinuxProcess>();
                let got_id = core::any::Any::type_id(&**self.ext());
                // Ask again. `ext` is immutable, so a downcast that fails once
                // and succeeds now would mean the first read was torn -- i.e.
                // the object is being written concurrently, which is a
                // different bug from a wrong type.
                let retry = self.ext().downcast_ref::<LinuxProcess>();
                let retry_ok = retry.is_some();
                if let Some(lp) = retry {
                    // The field is written once at construction and never
                    // again, so a downcast that fails and then immediately
                    // succeeds did not see a different TYPE -- it saw an
                    // inconsistent READ of a 16-byte field. Killing the kernel
                    // over a transient read of a value we can now see is
                    // correct is strictly worse than continuing with it. Log
                    // loudly: this is a mitigation, and every line it prints is
                    // still the bug.
                    error!(
                        "[ext-glitch] Process::linux(): pid={} name={:?} downcast failed then \
                         SUCCEEDED on retry -- ext read inconsistently, not a wrong type. \
                         fat data={:#x} vtable={:#x}, at birth data={:#x} vtable={:#x}, \
                         canaries lo={:#x} hi={:#x}",
                        self.id(),
                        // try_name: this diagnostic can be reached from inside
                        // the object layer's own signal callback (file teardown
                        // re-enters process accessors), where `name()` would
                        // wait on the lock that callback holds -- a deadlock
                        // that prints nothing, which is strictly worse than a
                        // corruption report missing one name.
                        trace_name(self),
                        fat[0],
                        fat[1],
                        born_data,
                        born_vtable,
                        self.ext_canary_values().0,
                        self.ext_canary_values().1,
                    );
                    return lp;
                }
                panic!(
                    "Process::linux(): pid={} name={:?} status={:?} has no LinuxProcess ext \
                     (ext fat pointer: data={:#x} vtable={:#x} -> {:x?} (drop, size, align), \
                     LinuxProcess would be size={} align={} vtable={:#x} -> {}; \
                     TypeId want={:?} got={:?} -> {}; downcast retry={}; actual type: {}; \
                     canaries lo={:#x} hi={:#x} -> {}; \
                     at birth: data={:#x} vtable={:#x} -> {}) -- \
                     ext is immutable and always installed for Linux processes, so \
                     this means the ext was CORRUPTED, not that a kernel-internal \
                     process leaked in",
                    self.id(),
                    trace_name(self),
                    self.status(),
                    fat[0],
                    fat[1],
                    vt,
                    core::mem::size_of::<LinuxProcess>(),
                    core::mem::align_of::<LinuxProcess>(),
                    want_vtable,
                    if want_vtable == fat[1] {
                        "SAME vtable: the ext IS a LinuxProcess and the downcast is what fails"
                    } else {
                        "DIFFERENT vtable: the ext really is another type"
                    },
                    want_id,
                    got_id,
                    if want_id == got_id {
                        "EQUAL: downcast_ref should have succeeded"
                    } else {
                        "DIFFERENT: two type identities for one type"
                    },
                    retry_ok,
                    // Name the type that is actually there. Across boots the
                    // vtable word has been CONSTANT (0xffffff0000a655e8) while
                    // `data` varied and stayed page-aligned -- so this is one
                    // specific type being written over the field, not a spray.
                    // A `Mutex<LinuxThread>` here would mean a Thread's ext
                    // landed on a Process's, i.e. the two objects overlap.
                    if self
                        .ext()
                        .downcast_ref::<Mutex<crate::thread::LinuxThread>>()
                        .is_some()
                    {
                        "Mutex<LinuxThread> -- a THREAD's ext on a PROCESS"
                    } else if self.ext().downcast_ref::<()>().is_some() {
                        "() -- a zircon-created process"
                    } else {
                        "unrecognised"
                    },
                    self.ext_canary_values().0,
                    self.ext_canary_values().1,
                    // Intact guards => the writer hit `ext` EXACTLY, so hunt
                    // something computing that field's address. Broken guards
                    // => a wider overrun, and the side names its direction.
                    match self.ext_canaries() {
                        (true, true) => "both INTACT: a precise write to ext alone",
                        (false, true) => "LOW broken: overrun growing upward from below",
                        (true, false) => "HIGH broken: overrun growing downward from above",
                        (false, false) => "BOTH broken: wide overrun across the field",
                    },
                    born_data,
                    born_vtable,
                    match (born_data == fat[0], born_vtable == fat[1]) {
                        (true, true) =>
                            "UNCHANGED: the ext was never a LinuxProcess -- not corruption, \
                             a construction path installs the wrong type",
                        (true, false) =>
                            "VTABLE ONLY: one 8-byte store over the vtable word, data untouched",
                        (false, true) =>
                            "DATA ONLY: one 8-byte store over the data word, vtable untouched",
                        (false, false) =>
                            "BOTH words replaced: a whole fat pointer was assigned over ext",
                    },
                )
            })
    }

    fn try_linux(&self) -> Option<&LinuxProcess> {
        self.ext().downcast_ref::<LinuxProcess>()
    }

    /// [Fork] the process.
    ///
    /// [Fork]: http://man7.org/linux/man-pages/man2/fork.2.html
    fn fork_from(parent: &Arc<Self>) -> ZxResult<Arc<Self>> {
        let linux_parent = parent.linux();
        // mmap_lock, WRITE side: freeze the parent's address-space LAYOUT for
        // the entire fork — snapshot of the mapping list AND the whole copy
        // loop below. Without this, another thread of a multithreaded parent
        // (llvmpipe's JIT mprotect/mmap storm in labwc) mutates the VMAR while
        // `fork_from` walks its stale snapshot, and the child materializes an
        // address space that never existed (the Xwayland fork-child dying in
        // its first mallocs). Taken BEFORE `inner` — that is the global order
        // (see the `aspace_lock` field doc).
        let _aspace = linux_parent.aspace_lock().lock();
        let mut linux_parent_inner = linux_parent.inner.lock();
        // Child joins the parent's process group: copy the parent's *effective*
        // pgid so the inherited value is concrete even if the parent never
        // called setpgid (raw 0 → own pid). This is what makes a Ctrl-C reach a
        // child like `ping` that the shell spawned without job control.
        let parent_pgid = if linux_parent_inner.pgid == 0 {
            parent.id()
        } else {
            linux_parent_inner.pgid
        };
        // Same resolution for the session: the child joins the parent's
        // session, and an unset (`0`) parent sid means "the parent's own pid".
        let parent_sid = if linux_parent_inner.sid == 0 {
            parent.id()
        } else {
            linux_parent_inner.sid
        };
        let new_linux_proc = LinuxProcess {
            root_inode: linux_parent.root_inode.clone(),
            parent: Mutex::new(Arc::downgrade(parent)),
            vt: linux_parent.vt,
            perf: crate::perf::ProcPerf::new(),
            itimers: Default::default(),
            aspace_lock: Mutex::new(()),
            inner: Mutex::new(linux_parent_inner.forked_child(parent_pgid, parent_sid)),
        };
        let new_proc = Process::create_with_ext(&parent.job(), "", new_linux_proc)?;
        // Batch the fork's cross-CPU TLB shootdowns into one, but only when
        // the parent has a single thread. That is the condition under which no
        // other CPU can be executing in the parent's address space, and so the
        // one under which the widened write-protect window cannot be observed —
        // see `VmAddressRegion::fork_from`. It is also the overwhelmingly common
        // case: a shell, or anything that forks to exec, forks single-threaded.
        // A multi-threaded parent keeps the per-mapping shootdown exactly as
        // before.
        let single_threaded = parent.thread_count() == 1;
        new_proc.vmar().fork_from(&parent.vmar(), single_threaded)?;
        // `create_with_ext` publishes the child into ROOT_JOB before its address
        // space exists and before it has any thread, so a concurrent kill can
        // terminate it inside this window. `set_status_running` refuses to
        // resurrect such a child; abandon it here rather than returning a
        // process that is "running" but already torn down and out of its job.
        // The child never ran a single user instruction, so ECANCELED-shaped
        // failure of the fork is the truthful outcome.
        if !new_proc.set_status_running() {
            return Err(ZxError::BAD_STATE);
        }
        linux_parent_inner
            .children
            .insert(new_proc.id(), new_proc.clone());

        // On termination: reparent this process's own still-live children to
        // INIT, then notify whoever reaps *this* process — its real parent
        // while alive, otherwise INIT (orphan reparenting). See `reaper_for` /
        // `reparent_live_children_to_init`.
        let parent = parent.clone();
        let weak_proc = Arc::downgrade(&new_proc);
        new_proc.add_signal_callback(Box::new(move |signal| {
            if signal.contains(Signal::PROCESS_TERMINATED) {
                if let Some(child) = weak_proc.upgrade() {
                    let exit_code = match child.status() {
                        Status::Exited(code) => code,
                        _ => 0,
                    };
                    reparent_live_children_to_init(&child);
                    // POSIX record locks die with the process, eagerly: the
                    // lazy liveness prune in `record_lock` cannot tell a dead
                    // owner from a recycled pid (see `release_owner`).
                    crate::fs::record_lock::release_owner(child.id());
                    // try_linux (not linux): this callback fires from the object
                    // layer on PROCESS_TERMINATED, concurrently with SMP teardown
                    // churn. A process whose extension can no longer be resolved
                    // must be skipped, not panic the kernel.
                    if let Some(lp) = child.try_linux() {
                        // Drop the file table AFTER releasing the lock — file
                        // teardown can re-enter process accessors (see
                        // close_file).
                        let dropped_files = {
                            let mut inner = lp.inner.lock();
                            let files = core::mem::take(&mut inner.files);
                            inner.cloexec_fds.clear();
                            inner.futexes.clear();
                            inner.semaphores = Default::default();
                            inner.shm_identifiers = Default::default();
                            files
                        };
                        drop(dropped_files);
                    }
                    if let Some(reaper) = reaper_for(&parent) {
                        if let Some(reaper_lp) = reaper.try_linux() {
                            reaper.signal_set(Signal::SIGCHLD);
                            reaper_lp.record_child_exit(child.id(), exit_code, child_cpu(&child));
                            // The zircon bit above wakes a blocked `wait*`;
                            // the LINUX signal is what a SIGCHLD handler, a
                            // signalfd or a `sigwait` is waiting for
                            // (`do_notify_parent`). Without it a shell's
                            // `trap CHLD`, a compositor reaping its autostart
                            // children, or a `pause()`-and-wait loop never
                            // hear that the child is gone.
                            let info = SigInfo::child_state_change(
                                child.id() as i32,
                                child
                                    .try_linux()
                                    .map(|lp| lp.credentials().ruid)
                                    .unwrap_or(0),
                                wait_status_exited(exit_code),
                            );
                            let _ = send_signal_to_process_with_info(
                                reaper.id() as usize,
                                LinuxSignal::SIGCHLD,
                                Some(info),
                            );
                        }
                    }
                }
                return true;
            }
            false
        }));
        Ok(new_proc)
    }
}

/// Wait for state changes in a child of the calling process, and obtain information about
/// the child whose state has changed.
///
/// A state change is considered to be:
/// - the child terminated.
/// - the child was stopped by a signal (`WSTOPPED` / `WUNTRACED`).
/// - the child was resumed by a signal (`WCONTINUED`).
///
/// CPU usage a child had accumulated by the time it exited: what `wait4(2)`
/// reports through its rusage out-parameter and what the parent adds to its
/// `RUSAGE_CHILDREN` totals when it reaps the child.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChildCpu {
    /// User-mode nanoseconds (every thread is dead by exit, so the process's
    /// dead-thread accumulator holds the complete figure).
    pub utime_ns: u64,
    /// Kernel nanoseconds from the per-process syscall accounting.
    pub stime_ns: u64,
}

/// Which child state changes a `wait*` call is interested in.
#[derive(Debug, Clone, Copy)]
pub struct WaitInterest {
    /// Child terminated (always true for classic `wait4`).
    pub exited: bool,
    /// Child stopped by a signal.
    pub stopped: bool,
    /// Stopped child resumed by `SIGCONT`.
    pub continued: bool,
}

impl WaitInterest {
    /// Classic `wait4` / `waitpid` without `WUNTRACED`/`WCONTINUED`.
    pub const EXITED_ONLY: Self = Self {
        exited: true,
        stopped: false,
        continued: false,
    };
}

/// `wait` status word for a stopped child: `WIFSTOPPED` / `WSTOPSIG`.
pub fn wait_status_stopped(sig: u8) -> i32 {
    ((sig as i32) << 8) | 0x7f
}

/// The exit code a process object carries when a SIGNAL killed it.
///
/// Negative, because the `i64` has to say WHICH of the two ways a process can
/// finish this was, and every ordinary exit code is a byte. The default-action
/// kill path used to store `128 + signo` -- the number a SHELL prints, which
/// is the shell's own arithmetic over `WIFSIGNALED`, not a status word -- so
/// the kernel reported every killed process as one that had called
/// `exit(128 + n)`, and nothing downstream could tell the two apart (a program
/// that really does `exit(137)` is not a process killed by SIGKILL).
pub const fn exit_code_killed_by(sig: u8) -> i64 {
    -(sig as i64)
}

/// The `wait(2)` status word for a child that has finished: `WIFEXITED` with
/// `WEXITSTATUS`, or `WIFSIGNALED` with `WTERMSIG`.
///
/// `sys/wait.h`: a status whose low seven bits are zero is an exit, and the
/// code is the SECOND byte; a status whose low seven bits are a signal number
/// is a death by that signal. The two are read apart by those seven bits, so
/// a kernel that only ever writes the first shape leaves `WIFSIGNALED` false
/// for every process it killed -- `system()` cannot tell a failed command from
/// an interrupted one, and a shell never prints "Killed".
pub fn wait_status_exited(raw: i64) -> i32 {
    if raw < 0 {
        // Killed by `-raw`. No core-dump bit: this kernel dumps no cores.
        (-raw as i32) & 0x7f
    } else {
        // exit(2) takes an int and the parent sees only its low byte, which
        // is why `exit(256)` is `exit(0)`.
        ((raw as i32) & 0xff) << 8
    }
}

/// `wait` status word for a continued child: `WIFCONTINUED`.
pub const WAIT_STATUS_CONTINUED: i32 = 0xffff;

/// Zircon user signal used to wake threads parked in a job-control stop.
const JOB_CONTINUE_SIGNAL: Signal = Signal::USER_SIGNAL_1;

/// A reaped child's zombie record as carried through reparenting: its pid and
/// the `(exit_code, cpu_usage)` pair kept until the reaper collects it.
type ReapedChild = (KoID, (i64, ChildCpu));

/// Read an exited child's final CPU usage off its still-live object.
fn child_cpu(child: &Arc<Process>) -> ChildCpu {
    ChildCpu {
        utime_ns: child.dead_threads_time(),
        stime_ns: child
            .try_linux()
            .map(|lp| lp.perf().totals().1)
            .unwrap_or(0),
    }
}

pub async fn wait_child(
    proc: &Arc<Process>,
    pid: KoID,
    nonblock: bool,
    reap: bool,
) -> LxResult<(ExitCode, ChildCpu)> {
    wait_child_interest(proc, pid, nonblock, reap, WaitInterest::EXITED_ONLY).await
}

pub async fn wait_child_interest(
    proc: &Arc<Process>,
    pid: KoID,
    nonblock: bool,
    reap: bool,
    interest: WaitInterest,
) -> LxResult<(ExitCode, ChildCpu)> {
    loop {
        check_signals()?;
        {
            let mut inner = proc.linux().inner.lock();
            if interest.exited {
                if let Some(&(code, cpu)) = inner.reaped_children.get(&pid) {
                    if reap {
                        inner.reaped_children.remove(&pid);
                        inner.add_children_cpu(cpu);
                    }
                    return Ok((wait_status_exited(code), cpu));
                }
            }
        }
        let child = {
            let inner = proc.linux().inner.lock();
            inner.children.get(&pid).cloned().ok_or(LxError::ECHILD)?
        };
        if interest.exited {
            if let Status::Exited(code) = child.status() {
                let cpu = child_cpu(&child);
                if reap {
                    let mut inner = proc.linux().inner.lock();
                    inner.children.remove(&pid);
                    inner.reaped_children.remove(&pid);
                    inner.add_children_cpu(cpu);
                }
                return Ok((wait_status_exited(code), cpu));
            }
        }
        if let Some(status) = child
            .try_linux()
            .and_then(|lp| lp.take_wait_notification(interest))
        {
            return Ok((status, ChildCpu::default()));
        }
        if nonblock {
            return Err(LxError::EAGAIN);
        }
        // Exit, stop and continue all pulse SIGCHLD on the parent.
        let proc_obj: Arc<dyn KernelObject> = proc.clone();
        proc_obj.signal_clear(Signal::SIGCHLD);
        if interest.exited {
            if let Status::Exited(code) = child.status() {
                let cpu = child_cpu(&child);
                if reap {
                    let mut inner = proc.linux().inner.lock();
                    inner.children.remove(&pid);
                    inner.reaped_children.remove(&pid);
                    inner.add_children_cpu(cpu);
                }
                return Ok((wait_status_exited(code), cpu));
            }
        }
        if let Some(status) = child
            .try_linux()
            .and_then(|lp| lp.take_wait_notification(interest))
        {
            return Ok((status, ChildCpu::default()));
        }
        check_signals()?;
        proc_obj.wait_signal(Signal::SIGCHLD).await;
    }
}

/// Wait for state changes in a child of the calling process.
pub async fn wait_child_any(
    proc: &Arc<Process>,
    nonblock: bool,
    reap: bool,
) -> LxResult<(KoID, ExitCode, ChildCpu)> {
    wait_child_any_interest(proc, nonblock, reap, WaitInterest::EXITED_ONLY, None).await
}

pub async fn wait_child_any_interest(
    proc: &Arc<Process>,
    nonblock: bool,
    reap: bool,
    interest: WaitInterest,
    pgid: Option<KoID>,
) -> LxResult<(KoID, ExitCode, ChildCpu)> {
    loop {
        check_signals()?;
        if let Some(result) = scan_waitable_children(proc, reap, interest, pgid) {
            return result;
        }
        {
            let inner = proc.linux().inner.lock();
            let has_candidate = if let Some(want) = pgid {
                inner
                    .children
                    .iter()
                    .any(|(_, c)| effective_pgid(c) == want)
            } else {
                !inner.children.is_empty() || !inner.reaped_children.is_empty()
            };
            if !has_candidate {
                return Err(LxError::ECHILD);
            }
        }
        if nonblock {
            return Err(LxError::EAGAIN);
        }
        let proc_obj: Arc<dyn KernelObject> = proc.clone();
        proc_obj.signal_clear(Signal::SIGCHLD);
        if let Some(result) = scan_waitable_children(proc, reap, interest, pgid) {
            return result;
        }
        check_signals()?;
        trace!("wait_child_any: waiting for SIGCHLD");
        proc_obj.wait_signal(Signal::SIGCHLD).await;
        trace!("wait_child_any: woke up from SIGCHLD");
    }
}

fn scan_waitable_children(
    proc: &Arc<Process>,
    reap: bool,
    interest: WaitInterest,
    pgid: Option<KoID>,
) -> Option<LxResult<(KoID, ExitCode, ChildCpu)>> {
    let mut inner = proc.linux().inner.lock();
    if interest.exited && pgid.is_none() {
        if let Some((pid, (code, cpu))) = inner.reaped_children.iter().next().map(|(&p, &c)| (p, c))
        {
            if reap {
                inner.reaped_children.remove(&pid);
                inner.add_children_cpu(cpu);
            }
            return Some(Ok((pid, wait_status_exited(code), cpu)));
        }
    }
    let kids: Vec<(KoID, Arc<Process>)> = inner
        .children
        .iter()
        .filter(|(_, c)| pgid.map(|want| effective_pgid(c) == want).unwrap_or(true))
        .map(|(&pid, c)| (pid, c.clone()))
        .collect();
    drop(inner);

    for (pid, child) in kids {
        if interest.exited {
            if let Status::Exited(code) = child.status() {
                let cpu = child_cpu(&child);
                if reap {
                    let mut inner = proc.linux().inner.lock();
                    inner.children.remove(&pid);
                    inner.reaped_children.remove(&pid);
                    inner.add_children_cpu(cpu);
                }
                return Some(Ok((pid, wait_status_exited(code), cpu)));
            }
        }
        if let Some(status) = child
            .try_linux()
            .and_then(|lp| lp.take_wait_notification(interest))
        {
            return Some(Ok((pid, status, ChildCpu::default())));
        }
    }
    None
}

/// System-call personality of a process: which operating system's ABI its
/// binary speaks. Detected from the ELF header at load time (see
/// `crate::loader`) and consulted by the trap handler to route each syscall to
/// the right translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Abi {
    /// Native Linux ABI (the default for every binary this kernel runs).
    #[default]
    Linux,
    /// FreeBSD/amd64 ABI (`ELFOSABI_FREEBSD` or a FreeBSD ABI note). Only
    /// meaningful on x86_64; other architectures always run as [`Abi::Linux`].
    Freebsd,
}

/// The `FD_CLOEXEC` a descriptor starts with when it is created by OPENING
/// something: whatever `O_CLOEXEC` the caller asked for.
///
/// It is a separate function, and the fd-table inserts take the answer as an
/// argument, because this rule holds ONLY for a descriptor that is born with
/// its description. `FD_CLOEXEC` lives in the descriptor (`fs/file.c`'s fd
/// table), not in the description (`struct file`), and `dup(2)`, `dup2(2)`,
/// `fcntl(F_DUPFD)` and `pidfd_getfd(2)` all install a description that is
/// ALREADY open under a second descriptor -- one whose close-on-exec state
/// has nothing to do with the `O_CLOEXEC` the first one was opened with, and
/// which must not be written back onto the shared object. Those four call
/// [`LinuxProcess::add_file_cloexec`] / [`LinuxProcess::replace_file`]
/// and say what they mean.
pub fn opened_cloexec(file: &Arc<dyn FileLike>) -> bool {
    file.flags().close_on_exec()
}

/// Linux specific process information.
pub struct LinuxProcess {
    /// The root INode of file system
    root_inode: Arc<dyn INode>,
    /// Parent process.
    ///
    /// MUTABLE, because a process can be re-parented: when its parent dies,
    /// [`reparent_live_children_to_init`] hands it to the nearest subreaper
    /// or to init, and from that moment THAT is its parent -- `getppid`
    /// returning 1 is how the daemonize idiom knows the intermediate process
    /// is gone, and it is the adopter that `wait`s for it and that a
    /// stop/continue must notify.
    parent: Mutex<Weak<Process>>,
    /// Virtual terminal this process is attached to (its stdin/stdout VT).
    /// Used to keep background (inactive-VT) shells from busy-polling stdin for
    /// input that can only arrive on the active terminal.
    vt: usize,
    /// Inner
    inner: Mutex<LinuxProcessInner>,
    /// Per-process syscall accounting (surfaced at `/proc/<pid>/perf`).
    perf: crate::perf::ProcPerf,
    /// Interval timers (`setitimer(2)`), indexed by ITIMER_REAL/VIRTUAL/PROF.
    /// Outside `inner` so timer-wheel callbacks never contend the big lock.
    /// A fresh process starts disarmed, and `fork` deliberately does not copy
    /// this field — fork(2): "timers are not inherited by the child".
    itimers: Mutex<[crate::time::ItimerSlot; 3]>,
    /// This kernel's `mmap_lock`: serializes every MUTATION of the process's
    /// address-space LAYOUT (mmap / munmap / mprotect / mremap / brk / shmat /
    /// shmdt / execve's teardown) against `fork`.
    ///
    /// Why it exists: `VmAddressRegion::fork_from` snapshots the mapping list
    /// ONCE and then clones each mapping with no VMAR lock held (deliberately
    /// — the copy of a big process takes tens of ms and the inner locks are
    /// IRQ-off spinlocks). Its "a point-in-time snapshot is sound" argument
    /// holds only for the forking THREAD; any OTHER thread of a multithreaded
    /// parent could still mmap/mprotect/munmap DURING the copy loop, and the
    /// child then materialized an address space that never existed: split-off
    /// pieces missing (SIGSEGV on a mapping fork should have provided — the
    /// historical openrc-init mis-replication), stale geometry from `cut()`,
    /// or content torn across the loop. labwc is exactly that parent: llvmpipe
    /// JIT threads mprotect/mmap continuously (the hunter W^X storm) while
    /// wlroots forks its Xwayland server — whose child died in musl mallocng
    /// on reshaped heap mappings before ever reaching execve, so Xwayland
    /// never came up. Linux forbids the race wholesale with `mmap_lock`; this
    /// is the same rule.
    ///
    /// Lock ORDER: `aspace_lock` is taken BEFORE `inner` everywhere (fork
    /// takes it first thing; syscall paths take it before any `get_file_like`
    /// / inner access). Page FAULTS do not touch it — they never change the
    /// layout — so a fault on another thread cannot deadlock against a fork
    /// holding this.
    aspace_lock: Mutex<()>,
}

/// Linux process mut inner data
#[derive(Default)]
struct LinuxProcessInner {
    /// Execute path
    execute_path: String,
    /// argv as seen by userland (`/proc/<pid>/cmdline`)
    cmdline: Vec<String>,
    /// Environment as of the last `execve` (`/proc/<pid>/environ`)
    environ: Vec<String>,
    /// Current Working Directory
    ///
    /// Omit leading '/'.
    current_working_directory: String,
    /// The process's sixteen resource limits, `tsk->signal->rlim` indexed by
    /// resource number. Only [`RLIMIT_NOFILE`] is enforced (right below, on
    /// every descriptor this table grows by); the rest are the soft budgets
    /// `getrlimit`/`setrlimit` remember for the program that set them. Each
    /// of the two facts is checkable: nothing else in this crate reads the
    /// array, and the hard limits are what [`LinuxProcess::rlimit`] refuses
    /// to raise without `CAP_SYS_RESOURCE`.
    limits: RLimits,
    /// Opened files
    files: HashMap<FileDesc, Arc<dyn FileLike>>,
    /// Per-descriptor `FD_CLOEXEC` state — the set of fds the next `execve`
    /// must close. POSIX makes this a property of the DESCRIPTOR, not the open
    /// file description: `fork` copies it per-process and plain `dup` clears it
    /// on the new fd. It therefore CANNOT live in the `File` object, which is
    /// shared via `Arc` between parent and child after `fork` — storing it
    /// there let one process's `fcntl(F_SETFD)` retag another process's fd,
    /// and the execve sweep then closed fds that should have survived
    /// (observed: dbus-daemon's `--print-address` pipe dying with EBADF).
    /// Creation-time registration happens in `insert_file`/`replace_file` from
    /// the newly built object's flags; after that, only `set_fd_cloexec`
    /// (fcntl) mutates membership.
    cloexec_fds: HashSet<FileDesc>,
    /// Semaphore
    semaphores: SemProc,
    /// Share Memory
    shm_identifiers: ShmProc,
    /// Futexes
    futexes: FutexTable,
    /// Child processes
    children: HashMap<KoID, Arc<Process>>,
    /// Exit codes and final CPU usage for children already detached (freed
    /// `Arc<Process>` at exit).
    reaped_children: HashMap<KoID, (i64, ChildCpu)>,
    /// CPU totals of children this process has reaped (`wait*` with reap):
    /// what getrusage(RUSAGE_CHILDREN) and times() cutime/cstime report.
    children_utime_ns: u64,
    /// Kernel-side counterpart of `children_utime_ns`.
    children_stime_ns: u64,
    /// Process group id (job control). `0` means "unset" and resolves to the
    /// process's own pid, so a fresh session/group leader is its own group.
    /// `fork` copies the parent's *effective* pgid (children join the parent's
    /// group); `setpgid` overrides it. Terminal-generated signals (Ctrl-C/\\/Z)
    /// go to every process whose effective pgid matches the tty's foreground
    /// group — see [`send_signal_to_pgrp`].
    pgid: u64,
    /// Session id (`setsid`/`getsid`). Same convention as `pgid`: `0` means
    /// "unset" and resolves to the process's own pid. `fork` copies the
    /// parent's *effective* sid (children stay in the parent's session);
    /// `setsid` starts a fresh session with `sid == pgid == pid`.
    sid: u64,
    /// Whether this process has already `execve`'d since the `fork` that
    /// created it -- the INVERSE of Linux's `PF_FORKNOEXEC`, which
    /// `copy_process` sets on every new task and `begin_new_exec` clears.
    /// Its one reader is [`setpgid_verdict`]: a parent may move a child
    /// between process groups only while the child is still running the
    /// parent's own code, because once the child has exec'd, the image now
    /// running never agreed to be job-controlled by whoever forked it
    /// (`kernel/sys.c`: `if (!(p->flags & PF_FORKNOEXEC)) return -EACCES`).
    has_execed: bool,
    /// Job-control stop: the process is stopped (SIGSTOP/SIGTSTP/…).
    /// Cleared by SIGCONT. Threads park in `run_user` while this is set.
    job_stopped: bool,
    /// Signal that caused the current stop (for `WIFSTOPPED` status).
    job_stop_sig: u8,
    /// Stop notification not yet collected by a `wait*` with `WSTOPPED`.
    job_stop_pending: bool,
    /// Continue notification not yet collected by a `wait*` with `WCONTINUED`.
    job_continued_pending: bool,
    /// Signal delivered to this process when its parent terminates
    /// (`prctl(PR_SET_PDEATHSIG)`); `0` = none. Deliberately NOT copied on
    /// `fork` — prctl(2): "the value is cleared for the child of a fork".
    pdeathsig: u8,
    /// `prctl(PR_SET_CHILD_SUBREAPER)`: orphaned descendants are reparented to
    /// the nearest live subreaper ancestor instead of init. Like Linux, the
    /// attribute itself is not inherited across `fork`.
    child_subreaper: bool,
    /// `prctl(PR_SET_NO_NEW_PRIVS)` (Documentation/userspace-api/no_new_privs.rst):
    /// once set it can never be cleared, is inherited across fork and execve,
    /// and stops `execve` from granting setuid/setgid privilege elevation.
    no_new_privs: bool,
    /// FreeBSD's `P_SUGID`, which is what `issetugid(2)` reports: this
    /// process's ids are not the ones it was started with, so neither its
    /// environment nor its address space can be assumed to be its own.
    ///
    /// Sticky on purpose, and `kern_prot.c` says why: "This is significant
    /// for procs that start as root and 'become' a user without an exec --
    /// programs cannot know *everything* that libc *might* have put in their
    /// data segment." So every `set*id` that moves an id latches it, it is
    /// inherited across `fork` (`p2->p_flag |= p1->p_flag & P_SUGID`), and
    /// only `execve` can clear it -- by recomputing it, which is the same
    /// question Linux answers as `bprm->secureexec` ([`Self::apply_exec_ids`]).
    sugid: bool,
    /// `prctl(PR_SET_DUMPABLE)`: `None` means "never set" and reads as the
    /// Linux default `SUID_DUMP_USER` (1).
    dumpable: Option<u8>,
    /// Execution domain (`personality(2)`). `0` is `PER_LINUX`; bits like
    /// `ADDR_NO_RANDOMIZE` are stored so a later query reads back what was
    /// set. Inherited across fork and execve.
    personality: u32,
    /// `prctl(PR_SET_THP_DISABLE)`: recorded and read back; there is no
    /// transparent-hugepage machinery for it to steer.
    thp_disable: bool,
    /// Signal actions
    signal_actions: SignalActions,
    /// Program break (top of heap).
    ///
    /// Initialized to 0; set to the end of the loaded ELF image by the loader
    /// via [`LinuxProcess::set_brk`] before the first user instruction runs.
    /// Updated by `sys_brk` as the heap grows or shrinks.
    brk: usize,
    /// Upper bound of the address range actually mapped to back the heap.
    ///
    /// `sys_brk` reserves heap pages in [`BRK_CHUNK`]-sized strides instead of
    /// one VMO per user-visible grow, so the user-visible `brk` typically
    /// trails `mapped_brk`. Lazily initialised on first `sys_brk`: a value of
    /// 0 means "matches `brk`".
    mapped_brk: usize,
    /// Process credentials.
    credentials: Credentials,
    /// System-call personality (Linux vs FreeBSD). Set by the loader from the
    /// ELF header and inherited across `fork`; re-evaluated at `execve`.
    abi: Abi,
}

#[derive(Clone)]
struct SignalActions {
    table: [SignalAction; LinuxSignal::RTMAX + 1],
}

impl Default for SignalActions {
    fn default() -> Self {
        Self {
            table: [SignalAction::default(); LinuxSignal::RTMAX + 1],
        }
    }
}

/// resource limit
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RLimit {
    /// soft limit
    pub cur: u64,
    /// hard limit
    pub max: u64,
}

impl Default for RLimit {
    fn default() -> Self {
        RLimit {
            cur: 1024,
            max: 1024,
        }
    }
}

/// `RLIM_INFINITY`: no limit at all. Not a very large limit -- code that
/// enforces a limit has to recognise it and not compare against it.
pub const RLIM_INFINITY: u64 = u64::MAX;

/// The sixteen resources of `asm-generic/resource.h`, in its order. A number
/// at or above [`RLIM_NLIMITS`] is `EINVAL`, not a resource this kernel has
/// not got round to.
pub const RLIMIT_CPU: usize = 0;
/// Largest file the process may create, in bytes.
pub const RLIMIT_FSIZE: usize = 1;
/// Size of the data segment.
pub const RLIMIT_DATA: usize = 2;
/// Size of the main thread's stack.
pub const RLIMIT_STACK: usize = 3;
/// Largest core dump, in bytes.
pub const RLIMIT_CORE: usize = 4;
/// Resident set size.
pub const RLIMIT_RSS: usize = 5;
/// Processes the real uid may have.
pub const RLIMIT_NPROC: usize = 6;
/// **One more than** the highest descriptor the process may open. The one
/// limit this kernel actually enforces.
pub const RLIMIT_NOFILE: usize = 7;
/// Memory that may be locked down.
pub const RLIMIT_MEMLOCK: usize = 8;
/// Size of the address space.
pub const RLIMIT_AS: usize = 9;
/// File locks the process may hold.
pub const RLIMIT_LOCKS: usize = 10;
/// Signals that may be queued.
pub const RLIMIT_SIGPENDING: usize = 11;
/// Bytes in POSIX message queues.
pub const RLIMIT_MSGQUEUE: usize = 12;
/// Ceiling on the nice value, as `20 - nice`.
pub const RLIMIT_NICE: usize = 13;
/// Ceiling on the real-time priority.
pub const RLIMIT_RTPRIO: usize = 14;
/// Microseconds of CPU a real-time thread may take without blocking.
pub const RLIMIT_RTTIME: usize = 15;
/// How many there are.
pub const RLIM_NLIMITS: usize = 16;

/// Linux's `sysctl_nr_open` default: the ceiling `prlimit64` puts on
/// `RLIMIT_NOFILE`'s HARD limit, over which it answers `EPERM`. It applies to
/// that one resource and no other.
pub const NR_OPEN: u64 = 1024 * 1024;

/// 8 MiB, Linux's `_STK_LIM` and the size this kernel gives a user stack.
pub const USER_STACK_SIZE: u64 = 8 * 1024 * 1024;

/// `setpriority`/`getpriority` `which`: one task, named by pid.
pub const PRIO_PROCESS: usize = 0;
/// `setpriority`/`getpriority` `which`: a process group, named by pgid.
pub const PRIO_PGRP: usize = 1;
/// `setpriority`/`getpriority` `which`: everything a user is running, named
/// by uid.
pub const PRIO_USER: usize = 2;

/// `fair_policy()`: the two policies that share out what is left over.
/// `SCHED_IDLE` is NOT one of them -- Linux counts it separately, and the
/// difference is what makes leaving it a privileged step.
pub fn is_fair_policy(policy: u8) -> bool {
    policy == SCHED_NORMAL || policy == SCHED_BATCH
}

/// `rt_policy()`: the two policies that run ahead of everything.
pub fn is_rt_policy(policy: u8) -> bool {
    policy == SCHED_FIFO || policy == SCHED_RR
}

/// What `user_check_sched_setscheduler()` needs to know about the task whose
/// scheduling is being set. All five belong to the TARGET, limits included:
/// the budget spent is the one of the task being moved, not of whoever asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedFacts {
    /// The policy it runs under now.
    pub policy: u8,
    /// Its nice value now.
    pub nice: i8,
    /// Its real-time priority now.
    pub rt_priority: u8,
    /// Its own soft `RLIMIT_NICE`.
    pub rlimit_nice: u64,
    /// Its own soft `RLIMIT_RTPRIO`.
    pub rlimit_rtprio: u64,
}

/// What a `sched_setscheduler`/`sched_setparam`/`sched_setattr` call is
/// asking for, once the parameters have been validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedRequest {
    /// The policy asked for.
    pub policy: u8,
    /// The nice value asked for.
    pub nice: i8,
    /// The real-time priority asked for.
    pub rt_priority: u8,
}

/// What a `setpriority`/`getpriority` `which`/`who` pair names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrioTarget {
    /// `PRIO_PROCESS`: the one task with this id.
    Thread(KoID),
    /// `PRIO_PGRP`: every task of every process in this group.
    Group(KoID),
    /// `PRIO_USER`: every task of every process whose REAL uid is this one.
    User(u32),
}

/// Resolve a `which`/`who` pair against the caller's own ids.
///
/// `who == 0` means "mine" in all three flavours, and each flavour means a
/// different "mine": this task, this task's process group, the uid this
/// process runs as. `who != 0` means exactly what it says, and that is the
/// whole point -- a `who` that is only ever read for `PRIO_PROCESS` turns
/// `renice -g <otro>` into a renice of the caller, which is indistinguishable
/// from doing the job right until someone checks.
///
/// The third arm carries Linux's trap: it falls back to the caller's REAL
/// uid (`cred->uid`), not the effective one, so a set-user-ID program asking
/// `getpriority(PRIO_USER, 0)` asks about the user who ran it.
pub fn prio_target(
    which: usize,
    who: usize,
    own_tid: KoID,
    own_pgid: KoID,
    own_ruid: u32,
) -> LxResult<PrioTarget> {
    match which {
        PRIO_PROCESS if who == 0 => Ok(PrioTarget::Thread(own_tid)),
        PRIO_PROCESS => Ok(PrioTarget::Thread(who as KoID)),
        PRIO_PGRP if who == 0 => Ok(PrioTarget::Group(own_pgid)),
        PRIO_PGRP => Ok(PrioTarget::Group(who as KoID)),
        PRIO_USER if who == 0 => Ok(PrioTarget::User(own_ruid)),
        PRIO_USER => Ok(PrioTarget::User(who as u32)),
        _ => Err(LxError::EINVAL),
    }
}

/// A process's sixteen limits, indexed by resource number.
///
/// A newtype and not a bare array because `LinuxProcessInner` derives
/// `Default`, and a derived array default is sixteen copies of one row --
/// which is how every resource would quietly come out as whatever
/// [`RLimit::default`] happens to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RLimits([RLimit; RLIM_NLIMITS]);

impl Default for RLimits {
    fn default() -> Self {
        RLimits(INIT_RLIMITS)
    }
}

impl RLimits {
    /// The limit for `resource`, or `EINVAL` for a number that is not one.
    pub fn get(&self, resource: usize) -> LxResult<RLimit> {
        self.0.get(resource).copied().ok_or(LxError::EINVAL)
    }

    /// Replace one row. The rules live in [`LinuxProcess::rlimit_check`];
    /// this only stores.
    pub fn set(&mut self, resource: usize, limit: RLimit) -> LxResult {
        *self.0.get_mut(resource).ok_or(LxError::EINVAL)? = limit;
        Ok(())
    }
}

/// `INIT_RLIMITS` (`include/asm-generic/resource.h`), one row per resource in
/// the order above.
///
/// One deliberate departure: `RLIMIT_NPROC` and `RLIMIT_SIGPENDING` are
/// `{0, 0}` in Linux because `init` overwrites them from `max_threads` before
/// any userspace runs. Nothing counts either here, so `RLIM_INFINITY` is the
/// truth about this kernel where a literal zero would not be.
///
/// What a process used to get was [`RLimit::default`] -- 1024/1024 -- for the
/// one limit that existed. The soft limit is unchanged; the hard one becomes
/// Linux's `INR_OPEN_MAX`, so a process can raise its own descriptor budget
/// to 4096 the way it can anywhere else.
const INIT_RLIMITS: [RLimit; RLIM_NLIMITS] = {
    const INF: RLimit = RLimit {
        cur: RLIM_INFINITY,
        max: RLIM_INFINITY,
    };
    let mut table = [INF; RLIM_NLIMITS];
    table[RLIMIT_STACK] = RLimit {
        cur: USER_STACK_SIZE,
        max: RLIM_INFINITY,
    };
    // No core is ever written, so a soft limit of zero is not a policy
    // this kernel is choosing: it is the size of the dump.
    table[RLIMIT_CORE] = RLimit {
        cur: 0,
        max: RLIM_INFINITY,
    };
    // `INR_OPEN_CUR` / `INR_OPEN_MAX`.
    table[RLIMIT_NOFILE] = RLimit {
        cur: 1024,
        max: 4096,
    };
    // `MLOCK_LIMIT`.
    table[RLIMIT_MEMLOCK] = RLimit {
        cur: USER_STACK_SIZE,
        max: USER_STACK_SIZE,
    };
    // `MQ_BYTES_MAX`.
    table[RLIMIT_MSGQUEUE] = RLimit {
        cur: 819_200,
        max: 819_200,
    };
    // A process may not raise its own priority here at all, which is what
    // Linux's zero means.
    table[RLIMIT_NICE] = RLimit { cur: 0, max: 0 };
    table[RLIMIT_RTPRIO] = RLimit { cur: 0, max: 0 };
    table
};

/// The type of process exit code.
pub type ExitCode = i32;

impl LinuxProcess {
    /// Whether `pid` is a child of this process that has exited and has not
    /// been collected by `wait*` yet -- a zombie. Its `Process` has already
    /// left the job (`ROOT_JOB.find_process` misses it), but `kill(2)` must
    /// still succeed: Linux accepts a signal to a zombie and drops it, and
    /// callers rely on `kill(pid, 0) == 0` / `kill(pid, SIGKILL) == 0` to
    /// mean "that pid is still ours to wait for".
    pub fn is_zombie_child(&self, pid: KoID) -> bool {
        self.inner.lock().reaped_children.contains_key(&pid)
    }

    /// Drop the live child handle and keep only the exit code (plus the
    /// child's final CPU usage, for the reaper's rusage) for a future `wait`.
    pub fn record_child_exit(&self, child_id: KoID, exit_code: i64, cpu: ChildCpu) {
        let mut inner = self.inner.lock();
        inner.children.remove(&child_id);
        inner.reaped_children.insert(child_id, (exit_code, cpu));
    }

    /// CPU totals of already-reaped children, in nanoseconds (utime, stime).
    pub fn children_cpu_ns(&self) -> (u64, u64) {
        let inner = self.inner.lock();
        (inner.children_utime_ns, inner.children_stime_ns)
    }

    /// Create a new process bound to virtual terminal `vt`, building a fresh
    /// root filesystem.
    pub fn new(rootfs: Arc<dyn FileSystem>, vt: usize) -> Self {
        Self::with_root(crate::fs::create_root_fs(rootfs), vt)
    }

    /// Create a new process reusing an already-built root filesystem (shared by
    /// `Arc`, like `fork`), bound to virtual terminal `vt`. Used to spawn the
    /// extra per-VT shells without re-scanning disks / re-mounting.
    pub fn with_root(root_inode: Arc<dyn INode>, vt: usize) -> Self {
        // fd 0/1/2 are the process's controlling terminal: the per-VT console
        // device node `/dev/tty{vt+1}`. Naming the path after the real device
        // node — rather than `/dev/stdin`, which has no devfs entry — lets
        // `/proc/self/fd/N` resolve to a node `stat()` can find. musl's
        // `ttyname()` reads that symlink and then requires `stat(path)` and
        // `fstat(fd)` to report the same (st_dev, st_ino); both resolve to the
        // same `Stdin`/`Stdout` inode, so they match and `isatty()`/`ttyname()`
        // (and the `tty` command) report the terminal instead of failing with
        // ENOTTY ("not a tty").
        let tty_path = alloc::format!("/dev/tty{}", vt + 1);
        let stdin = File::new(
            crate::fs::stdio::vt_stdin(vt),
            OpenFlags::RDONLY,
            tty_path.clone(),
        ) as Arc<dyn FileLike>;
        let stdout_dev = crate::fs::stdio::vt_stdout(vt);
        let stdout =
            File::new(stdout_dev.clone(), OpenFlags::WRONLY, tty_path.clone()) as Arc<dyn FileLike>;
        let stderr = File::new(stdout_dev, OpenFlags::WRONLY, tty_path) as Arc<dyn FileLike>;
        let mut files = HashMap::new();
        files.insert(0.into(), stdin);
        files.insert(1.into(), stdout);
        files.insert(2.into(), stderr);

        LinuxProcess {
            root_inode,
            parent: Mutex::new(Weak::default()),
            vt,
            perf: crate::perf::ProcPerf::new(),
            itimers: Default::default(),
            aspace_lock: Mutex::new(()),
            inner: Mutex::new(LinuxProcessInner {
                files,
                ..Default::default()
            }),
        }
    }

    /// The process's `mmap_lock` (see the field doc): hold it across any
    /// address-space LAYOUT mutation, and across the whole of `fork`'s
    /// address-space copy. Taken BEFORE `inner` wherever both are needed.
    pub fn aspace_lock(&self) -> &Mutex<()> {
        &self.aspace_lock
    }

    /// Interval-timer slots (`setitimer(2)`), indexed by
    /// ITIMER_REAL (0) / ITIMER_VIRTUAL (1) / ITIMER_PROF (2).
    pub fn itimers(&self) -> &Mutex<[crate::time::ItimerSlot; 3]> {
        &self.itimers
    }

    /// Per-process syscall accounting (see [`crate::perf`]).
    pub fn perf(&self) -> &crate::perf::ProcPerf {
        &self.perf
    }

    /// The virtual terminal this process is attached to.
    pub fn vt(&self) -> usize {
        self.vt
    }

    /// Raw process-group id (`0` = unset → resolves to the process's own pid).
    pub fn pgid_raw(&self) -> u64 {
        self.inner.lock().pgid
    }

    /// Set this process's group id (`setpgid`). A `0` argument is stored as-is
    /// and resolves to the own pid; callers normally pass a concrete pgid.
    pub fn set_pgid_raw(&self, pgid: u64) {
        self.inner.lock().pgid = pgid;
    }

    /// Whether this process has `execve`'d since the fork that created it
    /// (the inverse of Linux's `PF_FORKNOEXEC`). See the field.
    pub fn has_execed(&self) -> bool {
        self.inner.lock().has_execed
    }

    /// Raw session id (`0` = unset → resolves to the process's own pid).
    pub fn sid_raw(&self) -> u64 {
        self.inner.lock().sid
    }

    /// `setsid`: make this process a session (and group) leader in one step,
    /// so both ids resolve to `pid` and job control sees a fresh group.
    pub fn become_session_leader(&self, pid: u64) {
        let mut inner = self.inner.lock();
        inner.sid = pid;
        inner.pgid = pid;
    }

    /// True while this process is job-control stopped.
    pub fn is_job_stopped(&self) -> bool {
        self.inner.lock().job_stopped
    }

    /// Enter a job-control stop caused by `sig`. Notifies the parent with
    /// `SIGCHLD` so a `wait*` with `WSTOPPED` can collect it. Idempotent if
    /// already stopped.
    pub fn job_stop(&self, proc: &Arc<Process>, sig: u8) {
        {
            let mut inner = self.inner.lock();
            if inner.job_stopped {
                return;
            }
            inner.job_stopped = true;
            inner.job_stop_sig = sig;
            inner.job_stop_pending = true;
            inner.job_continued_pending = false;
        }
        notify_parent_child_state(proc);
    }

    /// Leave a job-control stop (`SIGCONT`). Wakes parked threads and notifies
    /// the parent for `WCONTINUED`. Returns whether the process was stopped.
    pub fn job_continue(&self, proc: &Arc<Process>) -> bool {
        let was_stopped = self.inner.lock().leave_stop(true);
        proc.signal_set(JOB_CONTINUE_SIGNAL);
        if was_stopped {
            notify_parent_child_state(proc);
        }
        was_stopped
    }

    /// Break a job-control stop because the process is being KILLED.
    ///
    /// Like [`Self::job_continue`] it clears the stop and wakes the parked
    /// threads -- a thread sitting in [`wait_while_job_stopped`] is the one
    /// that has to run the default SIGKILL action, and it cannot run it while
    /// it is parked -- but it leaves NO continue notification behind: the
    /// process is not continuing, and a parent in `wait(WCONTINUED)` must not
    /// be told that it did.
    pub fn job_wake_to_die(&self, proc: &Arc<Process>) {
        self.inner.lock().leave_stop(false);
        proc.signal_set(JOB_CONTINUE_SIGNAL);
    }

    /// Consume a pending stop/continue notification for `wait*` if `interest`
    /// asks for it. Does not change `job_stopped` itself.
    pub fn take_wait_notification(&self, interest: WaitInterest) -> Option<ExitCode> {
        let mut inner = self.inner.lock();
        if interest.stopped && inner.job_stop_pending {
            inner.job_stop_pending = false;
            return Some(wait_status_stopped(inner.job_stop_sig));
        }
        if interest.continued && inner.job_continued_pending {
            inner.job_continued_pending = false;
            return Some(WAIT_STATUS_CONTINUED);
        }
        None
    }

    /// Parent-death signal (`prctl(PR_SET_PDEATHSIG)`); `0` = none.
    pub fn pdeathsig(&self) -> u8 {
        self.inner.lock().pdeathsig
    }

    /// Set the parent-death signal; `0` clears it.
    pub fn set_pdeathsig(&self, sig: u8) {
        self.inner.lock().pdeathsig = sig;
    }

    /// Whether this process volunteered as a child subreaper
    /// (`prctl(PR_SET_CHILD_SUBREAPER)`).
    pub fn is_child_subreaper(&self) -> bool {
        self.inner.lock().child_subreaper
    }

    /// Mark/unmark this process as a child subreaper.
    pub fn set_child_subreaper(&self, on: bool) {
        self.inner.lock().child_subreaper = on;
    }

    /// `no_new_privs` flag (see Documentation/userspace-api/no_new_privs.rst).
    pub fn no_new_privs(&self) -> bool {
        self.inner.lock().no_new_privs
    }

    /// Set `no_new_privs`. One-way: the kernel never clears it once set.
    pub fn set_no_new_privs(&self) {
        self.inner.lock().no_new_privs = true;
    }

    /// Dumpable attribute (`prctl(PR_GET_DUMPABLE)`); defaults to
    /// `SUID_DUMP_USER` (1) like Linux.
    pub fn dumpable(&self) -> u8 {
        self.inner.lock().dumpable.unwrap_or(1)
    }

    /// Set the dumpable attribute (0, 1 or 2).
    pub fn set_dumpable(&self, value: u8) {
        self.inner.lock().dumpable = Some(value);
    }

    /// Current execution domain (`personality(2)`).
    pub fn personality(&self) -> u32 {
        self.inner.lock().personality
    }

    /// Replace the execution domain, returning the previous one.
    pub fn set_personality(&self, persona: u32) -> u32 {
        let mut inner = self.inner.lock();
        core::mem::replace(&mut inner.personality, persona)
    }

    /// `PR_GET_THP_DISABLE` state.
    pub fn thp_disable(&self) -> bool {
        self.inner.lock().thp_disable
    }

    /// Record `PR_SET_THP_DISABLE`.
    pub fn set_thp_disable(&self, on: bool) {
        self.inner.lock().thp_disable = on;
    }

    /// Get futex object.
    ///
    /// Returns `None` if `uaddr` is null or not aligned for an `AtomicI32`;
    /// dereferencing such an address would otherwise fault or be undefined
    /// behaviour.
    #[allow(unsafe_code)]
    pub fn get_futex(&self, uaddr: VirtAddr) -> Option<Arc<Futex>> {
        if uaddr == 0 || !uaddr.is_multiple_of(core::mem::align_of::<AtomicI32>()) {
            return None;
        }
        Some(self.inner.lock().futexes.get_or_create(uaddr, || {
            let value = unsafe { &*(uaddr as *const AtomicI32) };
            Futex::new(value)
        }))
    }

    /// Get lowest free fd
    pub fn get_free_fd(&self) -> FileDesc {
        self.inner.lock().get_free_fd()
    }

    /// get the lowest available fd great than or equal to `start`.
    pub fn get_free_fd_from(&self, start: usize) -> FileDesc {
        self.inner.lock().get_free_fd_from(start)
    }

    /// Add a file to the file descriptor table.
    pub fn add_file(&self, file: Arc<dyn FileLike>) -> LxResult<FileDesc> {
        let cloexec = opened_cloexec(&file);
        self.add_file_cloexec(file, cloexec)
    }

    /// [`Self::add_file`] with the new descriptor's `FD_CLOEXEC` stated
    /// outright, for the callers that install an ALREADY-OPEN description
    /// under a second descriptor: `dup`, `fcntl(F_DUPFD)` and `pidfd_getfd`.
    /// See [`opened_cloexec`] for why those cannot let the flag be read off
    /// the object.
    pub fn add_file_cloexec(&self, file: Arc<dyn FileLike>, cloexec: bool) -> LxResult<FileDesc> {
        let inner = self.inner.lock();
        let fd = inner.get_free_fd();
        self.insert_file(inner, fd, file, cloexec)
    }

    /// Add a socket to the fd table.
    pub fn add_socket(&self, file: Arc<dyn FileLike>) -> LxResult<FileDesc> {
        let cloexec = opened_cloexec(&file);
        let inner = self.inner.lock();
        let fd = inner.get_free_fd_from(SOCKET_FD);
        self.insert_file(inner, fd, file, cloexec)
    }

    /// Add a file to the file descriptor table at given `fd`.
    pub fn add_file_at(&self, fd: FileDesc, file: Arc<dyn FileLike>) -> LxResult<FileDesc> {
        let cloexec = opened_cloexec(&file);
        let inner = self.inner.lock();
        self.insert_file(inner, fd, file, cloexec)
    }

    /// Atomically replace the file at `fd` (Linux dup2 semantics): the old
    /// entry (if any) is removed and the new one inserted under a SINGLE lock
    /// acquisition. The previous close-then-insert sequence left a window in
    /// which `fd` was absent from the table, so a concurrent thread's syscall
    /// on that fd got a spurious EBADF. Returns the previously installed file,
    /// if any, so the caller can log/inspect it.
    ///
    /// `cloexec` is the new descriptor's `FD_CLOEXEC`, given outright rather
    /// than read off `file`: the only caller is `dup2`/`dup3`, which installs
    /// an ALREADY-OPEN description and must not take its close-on-exec state
    /// from the `O_CLOEXEC` that description was opened with. See
    /// [`opened_cloexec`].
    pub fn replace_file(
        &self,
        fd: FileDesc,
        file: Arc<dyn FileLike>,
        cloexec: bool,
    ) -> LxResult<Option<Arc<dyn FileLike>>> {
        let mut inner = self.inner.lock();
        let old = inner.files.remove(&fd);
        // Net table size is unchanged (replace) or +1 (plain insert); apply the
        // same limit check as insert_file for the growth case.
        if old.is_none() && inner.files.len() >= inner.nofile() {
            return Err(LxError::EMFILE);
        }
        if cloexec {
            inner.cloexec_fds.insert(fd);
        } else {
            inner.cloexec_fds.remove(&fd);
        }
        inner.files.insert(fd, file);
        Ok(old)
    }

    /// insert a file and fd into the file descriptor table
    fn insert_file(
        &self,
        mut inner: MutexGuard<LinuxProcessInner>,
        fd: FileDesc,
        file: Arc<dyn FileLike>,
        cloexec: bool,
    ) -> LxResult<FileDesc> {
        if inner.files.len() < inner.nofile() {
            if cloexec {
                inner.cloexec_fds.insert(fd);
            } else {
                inner.cloexec_fds.remove(&fd);
            }
            inner.files.insert(fd, file);
            Ok(fd)
        } else {
            Err(LxError::EMFILE)
        }
    }

    /// Set or clear this descriptor's `FD_CLOEXEC` flag (`fcntl(F_SETFD)`).
    /// Per-descriptor by design — never touches the shared `File` object.
    pub fn set_fd_cloexec(&self, fd: FileDesc, on: bool) -> LxResult {
        let mut inner = self.inner.lock();
        if !inner.files.contains_key(&fd) {
            return Err(LxError::EBADF);
        }
        if on {
            inner.cloexec_fds.insert(fd);
        } else {
            inner.cloexec_fds.remove(&fd);
        }
        Ok(())
    }

    /// This descriptor's `FD_CLOEXEC` flag (`fcntl(F_GETFD)`).
    pub fn fd_cloexec(&self, fd: FileDesc) -> LxResult<bool> {
        let inner = self.inner.lock();
        if !inner.files.contains_key(&fd) {
            return Err(LxError::EBADF);
        }
        Ok(inner.cloexec_fds.contains(&fd))
    }

    /// The soft `RLIMIT_NOFILE`, the one limit this kernel enforces.
    ///
    /// **Read only.** The one way to move a limit is [`Self::rlimit`], which
    /// is where `do_prlimit`'s rules live; a second way in would be a second
    /// place for them not to be applied.
    pub fn file_limit(&self) -> RLimit {
        self.inner
            .lock()
            .limits
            .get(RLIMIT_NOFILE)
            .unwrap_or_default()
    }

    /// `do_prlimit()`: read `resource`, and replace it when `new` is given.
    /// Returns the limit as it was, which is what `prlimit64` reports.
    ///
    /// `may_raise_hard` is the CALLER's `capable(CAP_SYS_RESOURCE)`, not this
    /// process's: `prlimit64(pid, ...)` asks about whoever is calling.
    pub fn rlimit(
        &self,
        resource: usize,
        new: Option<RLimit>,
        may_raise_hard: bool,
    ) -> LxResult<RLimit> {
        let mut inner = self.inner.lock();
        let old = inner.limits.get(resource)?;
        if let Some(new) = new {
            Self::rlimit_check(resource, old, new, may_raise_hard)?;
            inner.limits.set(resource, new)?;
        }
        Ok(old)
    }

    /// Whether `caller` may read or move **another** process's limits.
    ///
    /// `check_prlimit_permission()`:
    ///
    /// ```c
    /// id_match = (uid_eq(cred->uid, tcred->euid) &&
    ///             uid_eq(cred->uid, tcred->suid) &&
    ///             uid_eq(cred->uid, tcred->uid)  &&
    ///             gid_eq(cred->gid, tcred->egid) &&
    ///             gid_eq(cred->gid, tcred->sgid) &&
    ///             gid_eq(cred->gid, tcred->gid));
    /// if (!id_match && !ns_capable(tcred->user_ns, CAP_SYS_RESOURCE))
    ///         return -EPERM;
    /// ```
    ///
    /// The six comparisons are [`holds_every_id_of`]; what this rule chooses is
    /// the caller's **REAL** pair, and `CAP_SYS_RESOURCE` as the way past them.
    pub fn may_touch_limits_of(caller: &Credentials, target: &Credentials) -> bool {
        holds_every_id_of(caller.ruid, caller.rgid, target)
            || has_capability(caller.euid, CAP_SYS_RESOURCE)
    }

    /// Whether `caller` may reach INTO `target`: read its memory, trace it, or
    /// take one of its open files.
    ///
    /// `__ptrace_may_access()` under `PTRACE_MODE_ATTACH_REALCREDS`, which is
    /// the mode `pidfd_getfd(2)` asks for:
    ///
    /// ```c
    /// if (same_thread_group(task, current))
    ///         return 0;
    /// caller_uid = cred->uid;  /* REALCREDS: the real pair, not the effective one */
    /// caller_gid = cred->gid;
    /// if (uid_eq(caller_uid, tcred->euid) && uid_eq(caller_uid, tcred->suid) &&
    ///     uid_eq(caller_uid, tcred->uid)  && gid_eq(caller_gid, tcred->egid) &&
    ///     gid_eq(caller_gid, tcred->sgid) && gid_eq(caller_gid, tcred->gid))
    ///         goto ok;
    /// if (ptrace_has_cap(tcred->user_ns, mode))
    ///         goto ok;
    /// return -EPERM;
    /// ```
    ///
    /// The six comparisons are [`holds_every_id_of`], the same ones
    /// [`Self::may_touch_limits_of`] makes -- Linux writes them out twice, and
    /// they are written once here precisely so they cannot drift apart. What
    /// this rule chooses is the caller's **REAL** pair (`REALCREDS`),
    /// `CAP_SYS_PTRACE` as the way past them, and an escape hatch `prlimit64`
    /// does not have at all: taking an fd out of your OWN process is a `dup`,
    /// so the thread group goes through before any id is read, where
    /// `check_prlimit_permission()` compares `current == task` and leaves a
    /// sibling thread to the ids.
    ///
    /// Linux has a seventh test after these: a target that dropped privileges
    /// and became non-dumpable is out of reach even for an id match. There is
    /// no `dumpable` flag in this kernel, so it is not written here -- and
    /// that omission is the permissive direction, which is why it is written
    /// down.
    ///
    /// Which capability is named cannot be tested: [`has_capability`] asks only
    /// whether the effective uid is root, so `CAP_SYS_PTRACE` and
    /// `CAP_SYS_RESOURCE` are the same word here. It is spelled correctly
    /// anyway, because the day this kernel keeps a real capability set the two
    /// rules part company and nothing will point at this line.
    pub fn may_attach_to(
        caller: &Credentials,
        target: &Credentials,
        same_thread_group: bool,
    ) -> bool {
        if same_thread_group {
            return true;
        }
        holds_every_id_of(caller.ruid, caller.rgid, target)
            || has_capability(caller.euid, CAP_SYS_PTRACE)
    }

    /// Whether `caller` may READ what `target` is doing: its environment, its
    /// address map, the targets of its open file descriptors.
    ///
    /// The same `__ptrace_may_access()`, under `PTRACE_MODE_READ_FSCREDS`,
    /// which is the mode every gated file of `/proc/<pid>/` asks for. The one
    /// line that changes is the pair: `FSCREDS` compares the caller's
    /// **filesystem** ids, and Linux says why in the comment on the branch
    /// this does not take -- "using the euid would make more sense here, but
    /// something in userland might rely on the old behavior [...]
    /// PTRACE_MODE_REALCREDS implies that the caller explicitly used a syscall
    /// that requests access to another process (and not a filesystem syscall
    /// to procfs)". So a path through the filesystem is judged on the identity
    /// the filesystem uses, and a syscall that names a process is judged on
    /// the one the caller cannot lay down.
    ///
    /// The two are only ever different for a process that moved its `fsuid`
    /// with `setfsuid(2)`, which is why the pair is the whole test:
    /// [`Self::may_attach_to`] and this one answer differently for the same
    /// credentials, and reading somebody's environment is not taking their
    /// open files.
    pub fn may_read_process_innards(
        caller: &Credentials,
        target: &Credentials,
        same_thread_group: bool,
    ) -> bool {
        if same_thread_group {
            return true;
        }
        holds_every_id_of(caller.fsuid, caller.fsgid, target)
            || has_capability(caller.euid, CAP_SYS_PTRACE)
    }

    /// The three rules `do_prlimit` applies to a new limit, and the fourth
    /// that `prlimit64` applies before it:
    ///
    /// ```c
    /// if (resource >= RLIM_NLIMITS)                                   return -EINVAL;
    /// if (new_rlim->rlim_cur > new_rlim->rlim_max)                    return -EINVAL;
    /// if (resource == RLIMIT_NOFILE &&
    ///     new_rlim->rlim_max > sysctl_nr_open)                        return -EPERM;
    /// if (new_rlim->rlim_max > rlim->rlim_max && !capable(CAP_SYS_RESOURCE))
    ///                                                                 return -EPERM;
    /// ```
    ///
    /// The last one is what makes a hard limit a limit. Without it a process
    /// that wanted a bigger soft limit than its hard one simply set both at
    /// once, and the hard limit meant nothing: a self-imposed budget with a
    /// lid the process could lift.
    fn rlimit_check(
        resource: usize,
        old: RLimit,
        new: RLimit,
        may_raise_hard: bool,
    ) -> LxResult<()> {
        if new.cur > new.max {
            return Err(LxError::EINVAL);
        }
        if resource == RLIMIT_NOFILE && new.max > NR_OPEN {
            return Err(LxError::EPERM);
        }
        if new.max > old.max && !may_raise_hard {
            return Err(LxError::EPERM);
        }
        Ok(())
    }

    /// `nice_to_rlimit()`: a nice value (`19..=-20`) as the rlimit-style
    /// number (`1..=40`) that `RLIMIT_NICE` is written in, and that
    /// `getpriority` returns so a raw syscall result stays non-negative.
    ///
    /// It runs the other way round: a SMALLER nice (a more favourable task)
    /// is a BIGGER number here.
    pub fn nice_to_rlimit(nice: i8) -> u64 {
        (20 - nice as i64) as u64
    }

    /// `is_nice_reduction()`: whether the target's own `RLIMIT_NICE` budget
    /// reaches as far down as `nice`.
    ///
    /// The budget belongs to the task being reniced, not to whoever asks: it
    /// says how far up the queue *that* task may go. Its boot value is `0`
    /// and [`Self::nice_to_rlimit`] never returns less than 1, so with the
    /// default limits the answer is always no. That is Linux's answer too,
    /// and it is why lowering a nice value is a privileged act unless
    /// somebody raised `RLIMIT_NICE` first.
    pub fn is_nice_reduction(target_rlimit_nice: u64, nice: i8) -> bool {
        Self::nice_to_rlimit(nice) <= target_rlimit_nice
    }

    /// `can_nice()`: the target's budget, or the caller's capability.
    pub fn can_nice(caller: &Credentials, target_rlimit_nice: u64, nice: i8) -> bool {
        Self::is_nice_reduction(target_rlimit_nice, nice)
            || has_capability(caller.euid, CAP_SYS_NICE)
    }

    /// `check_same_owner()`: whether the task is `caller`'s to schedule.
    ///
    /// ```c
    /// match = (uid_eq(cred->euid, pcred->euid) ||
    ///          uid_eq(cred->euid, pcred->uid));
    /// ```
    ///
    /// One id of the caller's -- the effective one -- against two of the
    /// target's. Mind the direction: the target's REAL uid counts, so a
    /// set-user-ID program stays reniceable by the user who started it, and
    /// a caller who dropped its effective uid loses the tasks it left behind.
    /// This is a looser test than the one `prlimit64` makes
    /// ([`Self::may_touch_limits_of`]), which wants every id to match.
    pub fn is_same_owner(caller: &Credentials, target: &Credentials) -> bool {
        target.ruid == caller.euid || target.euid == caller.euid
    }

    /// `set_one_prio_perm()`, which is also the question `sched_setaffinity`
    /// asks: whether `caller` may touch this task's scheduling at all.
    ///
    /// ```c
    /// if (uid_eq(pcred->uid,  cred->euid) ||
    ///     uid_eq(pcred->euid, cred->euid))
    ///         return true;
    /// if (ns_capable(pcred->user_ns, CAP_SYS_NICE))
    ///         return true;
    /// ```
    ///
    /// Linux spells the same rule out in two places -- `set_one_prio_perm()`
    /// and the `check_same_owner()` in `__sched_setaffinity()` -- and they
    /// are one rule, so it is written once here.
    pub fn may_set_priority_of(caller: &Credentials, target: &Credentials) -> bool {
        Self::is_same_owner(caller, target) || has_capability(caller.euid, CAP_SYS_NICE)
    }

    /// `set_one_prio()`'s two gates, in order: whether the task is yours,
    /// then how far down you are asking to push it.
    ///
    /// ```c
    /// if (!set_one_prio_perm(p))                           error = -EPERM;
    /// if (niceval < task_nice(p) && !can_nice(p, niceval))  error = -EACCES;
    /// ```
    ///
    /// The second gate only fires when the nice value goes DOWN: giving CPU
    /// away is free, taking it back is not. The two errors say different
    /// things and userspace can tell them apart -- `EPERM` is "not your
    /// task", `EACCES` is "your task, and you still may not".
    pub fn set_priority_verdict(
        caller: &Credentials,
        target: &Credentials,
        target_nice: i8,
        target_rlimit_nice: u64,
        nice: i8,
    ) -> LxResult<()> {
        if !Self::may_set_priority_of(caller, target) {
            return Err(LxError::EPERM);
        }
        if nice < target_nice && !Self::can_nice(caller, target_rlimit_nice, nice) {
            return Err(LxError::EACCES);
        }
        Ok(())
    }

    /// How `setpriority` folds one target's verdict into the result of a
    /// call that may name a whole group.
    ///
    /// `set_one_prio(p, niceval, error)` takes the running result in and
    /// hands it back, and the only thing a success does to it is
    ///
    /// ```c
    /// if (error == -ESRCH)
    ///         error = 0;
    /// ```
    ///
    /// while a failure overwrites it outright. So: a set with one unreachable
    /// member reports a failure even though every other member was reniced,
    /// the LAST failure is the one reported, and a success never clears a
    /// failure already recorded. Starting at `ESRCH` is what makes an empty
    /// set come out as `ESRCH`.
    pub fn fold_priority_verdict(so_far: LxResult<()>, one: LxResult<()>) -> LxResult<()> {
        match one {
            Err(e) => Err(e),
            Ok(()) if so_far == Err(LxError::ESRCH) => Ok(()),
            Ok(()) => so_far,
        }
    }

    /// `kill_ok_by_cred()`: whether `caller` may signal a process running
    /// under `target`'s credentials.
    ///
    /// ```c
    /// return uid_eq(cred->euid, tcred->suid) ||
    ///        uid_eq(cred->euid, tcred->uid)  ||
    ///        uid_eq(cred->uid,  tcred->suid) ||
    ///        uid_eq(cred->uid,  tcred->uid)  ||
    ///        ns_capable(tcred->user_ns, CAP_KILL);
    /// ```
    ///
    /// BOTH of the caller's uids against TWO of the target's -- and the
    /// target's EFFECTIVE uid is not one of them. That is the whole point: a
    /// set-user-ID program keeps the uid that started it in `suid`, so its
    /// owner can still kill it, and the user it turned INTO cannot. Compare
    /// [`Self::may_set_priority_of`], which asks the opposite pair, because
    /// reniceing something is not killing it.
    pub fn may_signal_cred(caller: &Credentials, target: &Credentials) -> bool {
        caller.euid == target.suid
            || caller.euid == target.ruid
            || caller.ruid == target.suid
            || caller.ruid == target.ruid
            || has_capability(caller.euid, CAP_KILL)
    }

    /// `check_kill_permission()`: the verdict on one target.
    ///
    /// Two ways past the credentials. A thread always reaches its own thread
    /// group, whatever the ids say. And `SIGCONT` always reaches your own
    /// SESSION: the shell that resumes a job is not always the job's owner,
    /// and a session that could not be continued would be a session that
    /// could be wedged from inside.
    ///
    /// `signal` is `None` for `kill(pid, 0)`, which asks the same question
    /// and delivers nothing -- and is refused just the same, so a probe
    /// cannot tell a live process you may not signal from a dead one.
    pub fn may_signal(
        caller: &Credentials,
        target: &Credentials,
        same_thread_group: bool,
        same_session: bool,
        signal: Option<LinuxSignal>,
    ) -> LxResult<()> {
        if same_thread_group || Self::may_signal_cred(caller, target) {
            return Ok(());
        }
        if signal == Some(LinuxSignal::SIGCONT) && same_session {
            return Ok(());
        }
        Err(LxError::EPERM)
    }

    /// `__kill_pgrp_info()`: how a send to a process group folds.
    ///
    /// ```c
    /// success |= !err;
    /// retval = err;
    /// ...
    /// return success ? 0 : retval;
    /// ```
    ///
    /// ONE member reached makes the whole call a success, and an error comes
    /// back only when every member refused -- the LAST one. This is the
    /// opposite of [`Self::fold_priority_verdict`], where a single refusal
    /// spoils the call: a group signal asks "did it land anywhere", a group
    /// renice asks "did all of it go through". Starting at `ESRCH` is what
    /// makes an empty group `ESRCH`.
    pub fn fold_group_signal(so_far: LxResult<()>, one: LxResult<()>) -> LxResult<()> {
        match (so_far, one) {
            (Ok(()), _) | (_, Ok(())) => Ok(()),
            (_, Err(e)) => Err(e),
        }
    }

    /// `kill(-1, sig)`'s fold, which is a third rule again:
    ///
    /// ```c
    /// int err = group_send_sig_info(...);
    /// ++count;
    /// if (err != -EPERM)
    ///         retval = err;
    /// ```
    ///
    /// `retval` starts at 0 and `EPERM` never reaches it, so **a broadcast
    /// reports success even when every process refused it**. That is
    /// deliberate in Linux: `kill(-1, SIGTERM)` is "everything I am allowed
    /// to", and being allowed nothing is not an error. `ESRCH` is left to the
    /// caller, for the walk that found nobody at all to count.
    pub fn fold_broadcast_signal(so_far: LxResult<()>, one: LxResult<()>) -> LxResult<()> {
        match one {
            Err(LxError::EPERM) => so_far,
            _ => one,
        }
    }

    /// `user_check_sched_setscheduler()`: whether an unprivileged caller may
    /// have this request, and `EPERM` when only `CAP_SYS_NICE` could.
    ///
    /// Linux reads as a list of `goto req_priv`, every one of them a reason
    /// the request is privileged, with the capability asked once at the
    /// bottom. [`Self::sched_request_is_unprivileged`] is that list; this is
    /// the bottom.
    ///
    /// `EINVAL` comes first in Linux and comes first here too: the request is
    /// validated before anyone asks who is making it, so a nonsense priority
    /// is `EINVAL` even from a caller who would have been refused.
    pub fn may_set_scheduler(
        caller: &Credentials,
        target: &Credentials,
        now: &SchedFacts,
        want: &SchedRequest,
    ) -> LxResult<()> {
        if Self::sched_request_is_unprivileged(caller, target, now, want)
            || has_capability(caller.euid, CAP_SYS_NICE)
        {
            return Ok(());
        }
        Err(LxError::EPERM)
    }

    /// Every `goto req_priv` of `user_check_sched_setscheduler()`, read the
    /// other way round: the request needs no privilege when none of them
    /// fires.
    ///
    /// Not modelled: the `sched_reset_on_fork` clause, because this kernel
    /// accepts that flag and does not keep it, so there is no flag to clear.
    fn sched_request_is_unprivileged(
        caller: &Credentials,
        target: &Credentials,
        now: &SchedFacts,
        want: &SchedRequest,
    ) -> bool {
        // A fair policy going DOWN the nice scale spends the task's own
        // RLIMIT_NICE, the same budget `setpriority` spends.
        if is_fair_policy(want.policy)
            && want.nice < now.nice
            && !Self::is_nice_reduction(now.rlimit_nice, want.nice)
        {
            return false;
        }
        if is_rt_policy(want.policy) {
            // A zero RLIMIT_RTPRIO means no real time at all, so entering a
            // real-time policy is privileged outright. Note it only stops a
            // CHANGE of policy: a task already running real-time may keep its
            // priority.
            if want.policy != now.policy && now.rlimit_rtprio == 0 {
                return false;
            }
            // And going up is capped by the budget, which is why a task can
            // always lower its own real-time priority.
            if want.rt_priority > now.rt_priority && want.rt_priority as u64 > now.rlimit_rtprio {
                return false;
            }
        }
        // SCHED_DEADLINE is privileged outright, "safest behavior for now".
        // Today it never reaches this far: the parameter check refuses it
        // with EINVAL first, since there is no deadline runqueue to put it on.
        if want.policy == SCHED_DEADLINE {
            return false;
        }
        // Leaving SCHED_IDLE is a nice reduction of the value the task
        // already has: idle sits below nice 19, so anything else is a step up
        // the queue and is paid for out of the same budget.
        if now.policy == SCHED_IDLE
            && want.policy != SCHED_IDLE
            && !Self::is_nice_reduction(now.rlimit_nice, now.nice)
        {
            return false;
        }
        // And, last, it has to be your task.
        Self::is_same_owner(caller, target)
    }

    /// Get the `File` with given `fd`.
    pub fn get_file(&self, fd: FileDesc) -> LxResult<Arc<File>> {
        let file = self
            .get_file_like(fd)?
            .downcast_arc::<File>()
            .map_err(|_| LxError::EBADF)?;
        Ok(file)
    }

    /*
        /// Get the `Socket` with given `fd`.
        pub fn get_socket(&self, fd: FileDesc) -> LxResult<Arc<dyn Socket>> {
            let socket = self
                .get_file_like(fd)?
                .as_socket()
            .map_err(|_| LxError::EBADF)?;
            Ok(Arc::new(socket))
        }
    */

    /// Get the `FileLike` with given `fd`.
    pub fn get_file_like(&self, fd: FileDesc) -> LxResult<Arc<dyn FileLike>> {
        let inner = self.inner.lock();
        trace!("get_file_like: {:x?}", inner.files);
        inner.files.get(&fd).cloned().ok_or(LxError::EBADF)
    }

    /// get all files
    pub fn get_files(&self) -> LxResult<HashMap<FileDesc, Arc<dyn FileLike>>> {
        let inner = self.inner.lock();
        Ok(inner.files.clone())
    }

    /// Close file descriptor `fd`.
    ///
    /// The removed file is dropped AFTER `inner` is released. Dropping the
    /// last reference runs the file's teardown, which can re-enter this very
    /// process's accessors — e.g. closing a controlling TTY delivers SIGHUP to
    /// the foreground process group, which reads `pgid_raw()` and takes the
    /// same spinlock. Seen live as a hard SMP deadlock:
    ///   [DEADLOCK cpu=1 at pgid_raw, HOLDER cpu=1 at close_file].
    pub fn close_file(&self, fd: FileDesc) -> LxResult {
        let removed = {
            let mut inner = self.inner.lock();
            inner.cloexec_fds.remove(&fd);
            inner.files.remove(&fd)
        };
        removed.map(drop).ok_or(LxError::EBADF)
    }

    /// Whether `pid` is a tracked child of this process (live or not yet reaped).
    pub fn has_child(&self, pid: KoID) -> bool {
        let inner = self.inner.lock();
        inner.children.contains_key(&pid) || inner.reaped_children.contains_key(&pid)
    }

    /// Mark every open descriptor in `[first, last]` close-on-exec.
    ///
    /// This is `close_range(2)`'s `CLOSE_RANGE_CLOEXEC` mode: the descriptors
    /// stay open and usable and only disappear at the next `execve`.
    pub fn set_range_cloexec(&self, first: FileDesc, last: FileDesc) {
        let mut inner = self.inner.lock();
        let fds: Vec<_> = inner
            .files
            .keys()
            .filter(|&&fd| fd >= first && fd <= last)
            .cloned()
            .collect();
        for fd in fds {
            inner.cloexec_fds.insert(fd);
        }
    }

    /// Close all file descriptors between `first` and `last`.
    pub fn close_range(&self, first: FileDesc, last: FileDesc) {
        // Collect the removed files and drop them only after `inner` is
        // released — see `close_file` for the re-entrancy deadlock this avoids.
        let removed: Vec<(FileDesc, Arc<dyn FileLike>)> = {
            let mut inner = self.inner.lock();
            let fds: Vec<_> = inner
                .files
                .keys()
                .filter(|&&fd| fd >= first && fd <= last)
                .cloned()
                .collect();
            fds.into_iter()
                .filter_map(|fd| {
                    inner.cloexec_fds.remove(&fd);
                    inner.files.remove(&fd).map(|f| (fd, f))
                })
                .collect()
        };
        for (fd, f) in removed {
            // DRM diagnostics: see fs::drm_fd_desc.
            if let Some(desc) = crate::fs::drm_fd_desc(&f) {
                debug!("[drm] fd {:?} ({}) closed by close_range", fd, desc);
            }
        }
    }

    /// Get root INode of the process.
    pub fn root_inode(&self) -> &Arc<dyn INode> {
        &self.root_inode
    }

    /// Get a snapshot of current credentials.
    pub fn credentials(&self) -> Credentials {
        self.inner.lock().credentials.clone()
    }

    /// Get real uid.
    pub fn uid(&self) -> u32 {
        self.inner.lock().credentials.ruid
    }

    /// Get effective uid.
    pub fn euid(&self) -> u32 {
        self.inner.lock().credentials.euid
    }

    /// Get saved uid.
    pub fn suid(&self) -> u32 {
        self.inner.lock().credentials.suid
    }

    /// Get real gid.
    pub fn gid(&self) -> u32 {
        self.inner.lock().credentials.rgid
    }

    /// Get effective gid.
    pub fn egid(&self) -> u32 {
        self.inner.lock().credentials.egid
    }

    /// Get saved gid.
    pub fn sgid(&self) -> u32 {
        self.inner.lock().credentials.sgid
    }

    /// Get the filesystem uid, the id every file access is checked against.
    pub fn fsuid(&self) -> u32 {
        self.inner.lock().credentials.fsuid
    }

    /// Get the filesystem gid.
    pub fn fsgid(&self) -> u32 {
        self.inner.lock().credentials.fsgid
    }

    /// Get supplementary groups.
    pub fn groups(&self) -> Vec<u32> {
        self.inner.lock().credentials.groups.clone()
    }

    /// Get umask.
    pub fn umask(&self) -> u16 {
        self.inner.lock().credentials.umask
    }

    /// Set umask and return the previous one.
    pub fn set_umask(&self, mask: u16) -> u16 {
        let mut inner = self.inner.lock();
        let old = inner.credentials.umask;
        inner.credentials.umask = mask & 0o777;
        old
    }

    /// Whether the current effective uid is root.
    pub fn is_superuser(&self) -> bool {
        self.euid() == ROOT_UID
    }

    /// Whether this process holds `cap`, one of the [`CAP_SYS_ADMIN`]-family
    /// numbers from `linux/capability.h`.
    ///
    /// **One answer for two jobs.** `capget(2)` hands userspace a capability
    /// set, and a program reads it to decide whether to even try a privileged
    /// call; the kernel then has to honour exactly that set when the call
    /// arrives. Answering the two separately is how a kernel ends up telling
    /// `ping` it has no capabilities and rebooting for it anyway. So this is
    /// the single answer: `sys_capget` builds the set it publishes out of it,
    /// and every gate asks it.
    ///
    /// The model is the one this kernel has everywhere else -- root, or not --
    /// so there are no per-process sets to store, and a capability number
    /// above [`CAP_LAST_CAP`] is held by nobody.
    pub fn capable(&self, cap: u32) -> bool {
        has_capability(self.euid(), cap)
    }

    /// Apply umask to file creation mode.
    pub fn apply_umask(&self, mode: u16) -> u16 {
        mode & !self.umask()
    }

    // Which of a caller's three ids an unprivileged id switch may name is NOT
    // one set: Linux draws a different one per syscall argument, and collapsing
    // them into "any of the three" quietly accepts switches Linux refuses.
    // Each rule below cites the `kernel/sys.c` test it comes from.

    /// `setuid(2)`/`setgid(2)`: the real or the SAVED id.
    /// `sys_setuid`: `!uid_eq(kuid, old->uid) && !uid_eq(kuid, new->suid)`
    /// -> `-EPERM`. The EFFECTIVE id is deliberately not in this set.
    fn setid_allowed(real: u32, saved: u32, id: u32) -> bool {
        id == real || id == saved
    }

    /// The REAL id argument of `setreuid(2)`/`setregid(2)`: the real or the
    /// effective id, never the saved one. `sys_setreuid`:
    /// `ruid != -1 && !uid_eq(kruid, old->uid) && !uid_eq(kruid, old->euid)`
    /// -> `-EPERM`. This is the narrowest of the three rules, and the one that
    /// keeps a saved id from being laundered into the real id.
    fn set_real_allowed(real: u32, effective: u32, id: u32) -> bool {
        id == real || id == effective
    }

    /// Any of the three: the EFFECTIVE argument of `setreuid(2)`, and every
    /// argument of `setresuid(2)`/`setresgid(2)`.
    fn set_any_allowed(real: u32, effective: u32, saved: u32, id: u32) -> bool {
        id == real || id == effective || id == saved
    }

    /// Whether `setreuid(real, eff)`/`setregid` also rewrites the SAVED id (to
    /// the new effective one). Linux: `if (ruid != -1 || (euid != -1 &&
    /// !uid_eq(keuid, old->uid))) new->suid = new->euid;` -- and nothing
    /// else. There used to be a `privileged ||` term in front, so a root
    /// `setreuid(-1, -1)`, which asks for nothing at all, still overwrote the
    /// saved uid and threw away an id the caller had deliberately kept in
    /// order to come back to it.
    fn setreid_updates_saved(real_arg: u32, eff_arg: u32, old_real: u32) -> bool {
        real_arg != NO_ID || (eff_arg != NO_ID && eff_arg != old_real)
    }

    /// `setfsuid(2)`/`setfsgid(2)`: the real, the effective, the saved, or
    /// **the one already in force**. `__sys_setfsuid`:
    /// `uid_eq(kuid, old->uid) || uid_eq(kuid, old->euid) ||
    /// uid_eq(kuid, old->suid) || uid_eq(kuid, old->fsuid)`. The acting id is
    /// in the set and in none of the other three rules, which is the whole
    /// point of the call: a program that has already dropped to `nobody` for
    /// file work can keep doing so after its effective id moves on.
    fn setfsid_allowed(real: u32, effective: u32, saved: u32, acting: u32, id: u32) -> bool {
        Self::set_any_allowed(real, effective, saved, id) || id == acting
    }

    /// Which ids a permission check acts as.
    ///
    /// `use_effective` selects the FILESYSTEM ids -- `current_fsuid()` /
    /// `current_fsgid()`, what `generic_permission` asks on every normal
    /// path -- and the real ones otherwise, which is `access(2)` asking the
    /// question as the real user.
    ///
    /// Written once because three places used to pick the pair themselves,
    /// and three places picking it is three places to update the day the
    /// kernel grows an id they should pick instead. That day was this one.
    fn acting_fs_ids(creds: &Credentials, use_effective: bool) -> (u32, u32) {
        if use_effective {
            (creds.fsuid, creds.fsgid)
        } else {
            (creds.ruid, creds.rgid)
        }
    }

    fn allowed_uid(creds: &Credentials, uid: u32) -> bool {
        Self::set_any_allowed(creds.ruid, creds.euid, creds.suid, uid)
    }

    fn allowed_gid(creds: &Credentials, gid: u32) -> bool {
        Self::set_any_allowed(creds.rgid, creds.egid, creds.sgid, gid)
    }

    fn check_requested_access(mode: u16, requested: u16) -> bool {
        requested == 0 || (mode & requested) == requested
    }

    /// Whether the credentials belong to group `gid`, the way Linux's
    /// `in_group_p()` decides it: the ACTING group (`fsgid`) plus the
    /// supplementary list -- and nothing else.
    ///
    /// The effective path used to also accept `rgid` (through a helper that
    /// mixed this question up with which gids a caller may switch TO, a
    /// different set). That handed a process the file's GROUP
    /// permission bits on the strength of a group it no longer acts as, which
    /// is the one thing `setegid`/`setregid` exists to take away: a program
    /// that drops its effective gid to give up access keeps it. Group bits are
    /// normally wider than other bits, so the divergence only ever granted
    /// more than Linux would.
    fn acts_as_group(creds: &Credentials, gid: u32, use_effective: bool) -> bool {
        let (_, acting) = Self::acting_fs_ids(creds, use_effective);
        acting == gid || creds.groups.contains(&gid)
    }

    /// The permission bits `creds` gets on a file owned by `owner_uid:owner_gid`
    /// with mode `mode`. Linux's `acl_permission_check`: owner, else group,
    /// else other -- and the owner arm is EXCLUSIVE, never falling back to the
    /// wider group or other bits.
    fn access_bits(
        creds: &Credentials,
        owner_uid: u32,
        owner_gid: u32,
        mode: u16,
        use_effective: bool,
    ) -> u16 {
        let (uid, _) = Self::acting_fs_ids(creds, use_effective);
        if uid == ROOT_UID {
            return mode & 0o777;
        }
        if uid == owner_uid {
            return (mode >> 6) & 0o7;
        }
        if Self::acts_as_group(creds, owner_gid, use_effective) {
            return (mode >> 3) & 0o7;
        }
        mode & 0o7
    }

    /// The whole DAC decision (Linux's `generic_permission`) on pure inputs:
    /// may `creds` do `requested` (an `0o7` mask) to a file owned by
    /// `owner_uid:owner_gid` with mode `mode`? `use_effective` selects the
    /// effective ids (every normal path) or the real ones (`access(2)`).
    fn access_verdict(
        creds: &Credentials,
        owner_uid: u32,
        owner_gid: u32,
        mode: u16,
        is_dir: bool,
        requested: u16,
        use_effective: bool,
    ) -> LxResult {
        let (selected_uid, _) = Self::acting_fs_ids(creds, use_effective);
        if selected_uid == ROOT_UID {
            // CAP_DAC_OVERRIDE semantics: root bypasses permission checks
            // except executing a non-directory with no exec bit set anywhere
            // (mode & 0o111 == 0). Directories are always searchable by root;
            // testing only the others-exec bit here used to lock root out of
            // 0700 directories (e.g. apk's /lib/apk/exec, breaking triggers).
            if requested & ACCESS_EXEC != 0 && !is_dir && mode & 0o111 == 0 {
                return Err(LxError::EACCES);
            }
            return Ok(());
        }
        let granted = Self::access_bits(creds, owner_uid, owner_gid, mode, use_effective);
        if Self::check_requested_access(granted, requested) {
            Ok(())
        } else {
            Err(LxError::EACCES)
        }
    }

    /// Check inode access against current credentials.
    pub fn check_access(
        &self,
        metadata: &Metadata,
        requested: u16,
        use_effective: bool,
    ) -> LxResult {
        Self::access_verdict(
            &self.credentials(),
            metadata.uid as u32,
            metadata.gid as u32,
            metadata.mode,
            metadata.type_ == FileType::Dir,
            requested,
            use_effective,
        )
    }

    /// Check inode access by fetching metadata first.
    pub fn check_inode_access(
        &self,
        inode: &Arc<dyn INode>,
        requested: u16,
        use_effective: bool,
    ) -> LxResult {
        let metadata = inode.metadata()?;
        self.check_access(&metadata, requested, use_effective)
    }

    /// Check parent directory mutation rights.
    pub fn check_directory_write(&self, inode: &Arc<dyn INode>) -> LxResult {
        self.check_inode_access(inode, ACCESS_WRITE | ACCESS_EXEC, true)
    }

    /// Check if sticky-directory removal/rename is allowed.
    pub fn check_sticky(&self, dir_metadata: &Metadata, target_metadata: &Metadata) -> LxResult {
        if (dir_metadata.mode & MODE_STICKY) == 0 {
            return Ok(());
        }
        let creds = self.credentials();
        // `__check_sticky()` opens with `kuid_t fsuid = current_fsuid();` and
        // compares that one id against both inodes.
        if creds.fsuid == ROOT_UID
            || creds.fsuid == dir_metadata.uid as u32
            || creds.fsuid == target_metadata.uid as u32
        {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// The mode a `chmod` by `creds` actually lands on a file owned by
    /// `owner_uid:owner_gid` whose current mode is `cur_mode`, or `EPERM` when
    /// the caller may not chmod it at all (Linux's `inode_owner_or_capable`:
    /// the owner, or root).
    ///
    /// Linux (`setattr_prepare`) strips exactly ONE bit, and only sometimes:
    /// `S_ISGID` is cleared when the caller is not in the file's group, "so
    /// that a chmod cannot give away group-execute privilege it does not
    /// hold". `S_ISUID` is never stripped from the owner's own chmod -- that
    /// is how an unprivileged user makes a setuid binary of a file they own.
    ///
    /// Both bits used to be stripped from every non-root chmod, and silently:
    /// the call still returned success, so `chmod 4755 prog` left 0755 behind
    /// and a program that checked the mode it had just set saw a different
    /// one. Being stricter than Linux is not the safe direction -- it breaks
    /// programs that work on Linux, and it breaks them without an error.
    fn chmod_bits(
        creds: &Credentials,
        owner_uid: u32,
        owner_gid: u32,
        cur_mode: u16,
        mode: u16,
    ) -> LxResult<u16> {
        // `inode_owner_or_capable()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
        if creds.fsuid != ROOT_UID && creds.fsuid != owner_uid {
            return Err(LxError::EPERM);
        }
        let mut out = cur_mode & !MODE_PERM_MASK | (mode & MODE_PERM_MASK);
        if creds.fsuid != ROOT_UID && !Self::acts_as_group(creds, owner_gid, true) {
            out &= !MODE_SET_GID;
        }
        Ok(out)
    }

    /// Change mode if current process is owner or root.
    pub fn chmod_metadata(&self, metadata: &mut Metadata, mode: u16) -> LxResult {
        let creds = self.credentials();
        metadata.mode = Self::chmod_bits(
            &creds,
            metadata.uid as u32,
            metadata.gid as u32,
            metadata.mode,
            mode,
        )?;
        Ok(())
    }

    /// Change owner/group following a conservative POSIX-compatible policy.
    pub fn chown_metadata(&self, metadata: &mut Metadata, uid: u32, gid: u32) -> LxResult {
        let creds = self.credentials();
        // `chown_ok()`/`chgrp_ok()` (`fs/attr.c`) both open on
        // `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
        let privileged = creds.fsuid == ROOT_UID;
        if !privileged {
            if uid != NO_ID && uid != metadata.uid as u32 {
                return Err(LxError::EPERM);
            }
            if creds.fsuid != metadata.uid as u32 {
                return Err(LxError::EPERM);
            }
            // Linux `chgrp_ok`: `in_group_p(gid)` -- the acting gid plus the
            // supplementary list, the same set `acts_as_group` answers for
            // the permission check. The real gid is not in it.
            if gid != NO_ID && !Self::acts_as_group(&creds, gid, true) {
                return Err(LxError::EPERM);
            }
        }
        if uid != NO_ID {
            metadata.uid = uid as _;
        }
        if gid != NO_ID {
            metadata.gid = gid as _;
        }
        metadata.mode &= !(MODE_SET_UID | MODE_SET_GID);
        Ok(())
    }

    /// Set owner/group for a newly created inode.
    pub fn initialize_created_metadata(
        &self,
        inode: &Arc<dyn INode>,
        parent_metadata: Option<&Metadata>,
        mode: u16,
        is_dir: bool,
    ) -> LxResult {
        let creds = self.credentials();
        let mut metadata = inode.metadata()?;
        // `inode_init_owner()`: `inode_fsuid_set()` / `inode_fsgid_set()`,
        // which are `current_fsuid()` / `current_fsgid()`. A new file belongs
        // to the id its creator was acting as, not to the one it kept for
        // everything else.
        metadata.uid = creds.fsuid as _;
        metadata.gid = parent_metadata
            .filter(|meta| (meta.mode & MODE_SET_GID) != 0)
            .map(|meta| meta.gid)
            .unwrap_or(creds.fsgid as _);
        let mut final_mode = mode & MODE_PERM_MASK;
        if let Some(parent) = parent_metadata {
            if (parent.mode & MODE_SET_GID) != 0 && is_dir {
                final_mode |= MODE_SET_GID;
            }
        }
        metadata.mode = (metadata.mode & !MODE_PERM_MASK | final_mode) as _;
        inode.set_metadata(&metadata)?;
        Ok(())
    }

    /// Apply setuid/setgid exec transitions.
    ///
    /// Returns whether this exec actually RAISED privileges -- Linux's
    /// `bprm->secureexec`, which is what turns on the extra forgetting in
    /// [`Self::reset_for_exec`].
    /// `may_suid` is Linux's `mnt_may_suid()`: false when the image sits on a
    /// mount that was mounted `nosuid`, which is the whole of what that option
    /// buys and the reason `/tmp` and `/dev/shm` are mounted with it.
    pub fn apply_exec_metadata(&self, metadata: &Metadata, may_suid: bool) -> bool {
        self.inner.lock().apply_exec_ids(
            metadata.mode,
            metadata.uid as u32,
            metadata.gid as u32,
            may_suid,
        )
    }

    /// FreeBSD's `issetugid(2)`: whether this process's ids have moved since
    /// it was started, so that its environment and its data segment were
    /// arranged by someone who is not who it is now.
    pub fn is_sugid(&self) -> bool {
        self.inner.lock().sugid
    }

    /// The aux-vector identity block to hand the image `execve` is about to
    /// load: who this process runs as now that the set-user-ID bits have been
    /// honoured, and `privileged`, which is what
    /// [`Self::apply_exec_metadata`] just returned.
    ///
    /// Built here, under the one lock, so that no caller has to remember
    /// which of the three user ids `AT_UID` means (the real one -- `AT_EUID`
    /// is the effective one) or that the block has to agree with itself.
    pub fn aux_identity(&self, privileged: bool) -> AuxIdentity {
        self.inner.lock().aux_identity(privileged)
    }

    /// What `execve` must make the PROCESS forget, once the old address space
    /// is gone. `privileged` is Linux's `bprm->secureexec`, which
    /// [`Self::apply_exec_metadata`] returns.
    ///
    /// The other halves of the exec reset are [`Self::remove_cloexec_files`],
    /// [`Self::reset_signal_actions_for_exec`] and, per thread,
    /// [`LinuxThread::reset_for_exec`](crate::thread::LinuxThread::reset_for_exec).
    pub fn reset_for_exec(&self, privileged: bool) {
        self.inner.lock().reset_for_exec(privileged)
    }

    /// Set supplementary groups.
    pub fn set_groups(&self, groups: Vec<u32>) {
        let mut inner = self.inner.lock();
        inner.credentials.groups = groups;
        // `kern_setgroups()` calls `setsugid(p)` unconditionally -- it does
        // not compare the lists first, and neither does this.
        inner.sugid = true;
    }

    /// Set uid according to current privileges.
    pub fn set_uid(&self, uid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let before = inner.ids();
        let privileged = inner.credentials.euid == ROOT_UID;
        if privileged {
            inner.credentials.ruid = uid;
            inner.credentials.euid = uid;
            inner.credentials.suid = uid;
            inner.note_id_change(before);
            return Ok(());
        }
        if Self::setid_allowed(inner.credentials.ruid, inner.credentials.suid, uid) {
            inner.credentials.euid = uid;
            inner.note_id_change(before);
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// Set gid according to current privileges.
    pub fn set_gid(&self, gid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let before = inner.ids();
        let privileged = inner.credentials.euid == ROOT_UID;
        if privileged {
            inner.credentials.rgid = gid;
            inner.credentials.egid = gid;
            inner.credentials.sgid = gid;
            inner.note_id_change(before);
            return Ok(());
        }
        if Self::setid_allowed(inner.credentials.rgid, inner.credentials.sgid, gid) {
            inner.credentials.egid = gid;
            inner.note_id_change(before);
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// Set real/effective uid.
    pub fn set_reuid(&self, ruid: u32, euid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let before = inner.ids();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            if ruid != NO_ID
                && !Self::set_real_allowed(inner.credentials.ruid, inner.credentials.euid, ruid)
            {
                return Err(LxError::EPERM);
            }
            if euid != NO_ID && !Self::allowed_uid(&inner.credentials, euid) {
                return Err(LxError::EPERM);
            }
        }
        let old_ruid = inner.credentials.ruid;
        if ruid != NO_ID {
            inner.credentials.ruid = ruid;
        }
        if euid != NO_ID {
            inner.credentials.euid = euid;
        }
        if Self::setreid_updates_saved(ruid, euid, old_ruid) {
            inner.credentials.suid = inner.credentials.euid;
        }
        inner.note_id_change(before);
        Ok(())
    }

    /// Set real/effective gid.
    pub fn set_regid(&self, rgid: u32, egid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let before = inner.ids();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            if rgid != NO_ID
                && !Self::set_real_allowed(inner.credentials.rgid, inner.credentials.egid, rgid)
            {
                return Err(LxError::EPERM);
            }
            if egid != NO_ID && !Self::allowed_gid(&inner.credentials, egid) {
                return Err(LxError::EPERM);
            }
        }
        let old_rgid = inner.credentials.rgid;
        if rgid != NO_ID {
            inner.credentials.rgid = rgid;
        }
        if egid != NO_ID {
            inner.credentials.egid = egid;
        }
        if Self::setreid_updates_saved(rgid, egid, old_rgid) {
            inner.credentials.sgid = inner.credentials.egid;
        }
        inner.note_id_change(before);
        Ok(())
    }

    /// Set real/effective/saved uid.
    pub fn set_resuid(&self, ruid: u32, euid: u32, suid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let before = inner.ids();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            for uid in [ruid, euid, suid] {
                if uid != NO_ID && !Self::allowed_uid(&inner.credentials, uid) {
                    return Err(LxError::EPERM);
                }
            }
        }
        if ruid != NO_ID {
            inner.credentials.ruid = ruid;
        }
        if euid != NO_ID {
            inner.credentials.euid = euid;
        }
        if suid != NO_ID {
            inner.credentials.suid = suid;
        }
        inner.note_id_change(before);
        Ok(())
    }

    /// Set real/effective/saved gid.
    pub fn set_resgid(&self, rgid: u32, egid: u32, sgid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let before = inner.ids();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            for gid in [rgid, egid, sgid] {
                if gid != NO_ID && !Self::allowed_gid(&inner.credentials, gid) {
                    return Err(LxError::EPERM);
                }
            }
        }
        if rgid != NO_ID {
            inner.credentials.rgid = rgid;
        }
        if egid != NO_ID {
            inner.credentials.egid = egid;
        }
        if sgid != NO_ID {
            inner.credentials.sgid = sgid;
        }
        inner.note_id_change(before);
        Ok(())
    }

    /// `setfsuid(2)`: move the id file accesses are checked against, on its
    /// own, and return the id that was in force before the call.
    ///
    /// **It cannot fail, and that is the whole difficulty.** `setfsuid`
    /// returns the OLD id whether or not it changed anything, so its return
    /// value says nothing about whether it worked; the man page tells a
    /// program to call `setfsuid(-1)` afterwards and compare. A kernel that
    /// accepts the id, does nothing and hands back something that looks like
    /// success is therefore not caught by the caller's error handling --
    /// there is none to catch it -- and the program goes on touching files
    /// with the privilege it believes it just put down. That is what this
    /// used to do.
    pub fn set_fsuid(&self, uid: u32) -> u32 {
        let mut inner = self.inner.lock();
        let c = inner.credentials.clone();
        // `if (!uid_valid(kuid)) return old_fsuid;` -- `(uid_t)-1` is not a
        // user id, so `setfsuid(-1)` is the pure query, and an id that is
        // already in force is `if (!uid_eq(kuid, old->fsuid))` declining to
        // build new credentials at all. Neither taints the process.
        if uid == NO_ID || uid == c.fsuid {
            return c.fsuid;
        }
        if Self::setfsid_allowed(c.ruid, c.euid, c.suid, c.fsuid, uid)
            || has_capability(c.euid, CAP_SETUID)
        {
            inner.credentials.fsuid = uid;
            // `commit_creds()` treats a move of `fsuid` alone exactly like a
            // `set*id`: `if (!uid_eq(new->fsuid, old->fsuid) || ...)
            // set_dumpable(task->mm, suid_dumpable);`.
            inner.sugid = true;
        }
        c.fsuid
    }

    /// `setfsgid(2)`: the group half of [`Self::set_fsuid`], with
    /// `CAP_SETGID` in place of `CAP_SETUID`.
    pub fn set_fsgid(&self, gid: u32) -> u32 {
        let mut inner = self.inner.lock();
        let c = inner.credentials.clone();
        if gid == NO_ID || gid == c.fsgid {
            return c.fsgid;
        }
        if Self::setfsid_allowed(c.rgid, c.egid, c.sgid, c.fsgid, gid)
            || has_capability(c.euid, CAP_SETGID)
        {
            inner.credentials.fsgid = gid;
            inner.sugid = true;
        }
        c.fsgid
    }

    /// Get parent process.
    pub fn parent(&self) -> Option<Arc<Process>> {
        self.parent.lock().upgrade()
    }

    /// Hand this process to a new parent (adoption on the old one's death).
    pub fn set_parent(&self, parent: &Arc<Process>) {
        *self.parent.lock() = Arc::downgrade(parent);
    }

    /// Get current working directory.
    pub fn current_working_directory(&self) -> String {
        String::from("/") + &self.inner.lock().current_working_directory
    }

    /// Get absolute path from dirfd and relative path.
    pub fn get_absolute_path(&self, dirfd: FileDesc, path: &str) -> LxResult<String> {
        if path.is_empty() {
            return Ok(String::from("/"));
        }
        let base_path = if path.starts_with('/') {
            String::new()
        } else if dirfd == FileDesc::CWD {
            self.inner.lock().current_working_directory.clone()
        } else {
            let file = self.get_file(dirfd)?;
            let file_path = file.path().clone();
            if let Some(stripped) = file_path.strip_prefix('/') {
                String::from(stripped)
            } else {
                file_path
            }
        };
        let mut cwd_vec: Vec<_> = base_path.split('/').filter(|x| !x.is_empty()).collect();
        for seg in path.split('/') {
            match seg {
                ".." => {
                    cwd_vec.pop();
                }
                "." | "" => {}
                _ => cwd_vec.push(seg),
            }
        }
        Ok(String::from("/") + &cwd_vec.join("/"))
    }

    /// Change working directory.
    pub fn change_directory(&self, path: &str) {
        if path.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        let cwd = match path.as_bytes()[0] {
            b'/' => String::new(),
            _ => inner.current_working_directory.clone(),
        };
        let mut cwd_vec: Vec<_> = cwd.split('/').filter(|x| !x.is_empty()).collect();
        for seg in path.split('/') {
            match seg {
                ".." => {
                    cwd_vec.pop();
                }
                "." | "" => {} // nothing to do here.
                _ => cwd_vec.push(seg),
            }
        }
        inner.current_working_directory = cwd_vec.join("/");
    }

    /// Get execute path.
    pub fn execute_path(&self) -> String {
        self.inner.lock().execute_path.clone()
    }

    /// Set execute path.
    ///
    /// Re-exec via `/proc/self/exe` or `/proc/<own_pid>/exe` must not clobber a
    /// previously recorded real path. Own pid is taken from the current thread
    /// when available; `LinuxProcess` only holds a `Weak` to its *parent*
    /// process, so there is no self-Process back-pointer to upgrade here
    /// (doing so used to `unwrap` a default `Weak` and panic on init/shell).
    pub fn set_execute_path(&self, path: &str) {
        let mut inner = self.inner.lock();
        if !inner.execute_path.is_empty() && is_proc_exe_magic(path) {
            return;
        }
        inner.execute_path = String::from(path);
    }

    /// The process's system-call personality (Linux or FreeBSD).
    pub fn abi(&self) -> Abi {
        self.inner.lock().abi
    }

    /// Set the system-call personality. The loader calls this after detecting
    /// the ABI of the ELF it just mapped (at initial load and at `execve`).
    pub fn set_abi(&self, abi: Abi) {
        self.inner.lock().abi = abi;
    }

    /// Set argv for `/proc/<pid>/cmdline`.
    pub fn set_cmdline(&self, args: Vec<String>) {
        self.inner.lock().cmdline = args;
    }

    /// Get argv.
    pub fn cmdline(&self) -> Vec<String> {
        self.inner.lock().cmdline.clone()
    }

    /// Set the environment for `/proc/<pid>/environ` (captured at `execve`).
    pub fn set_environ(&self, envs: Vec<String>) {
        self.inner.lock().environ = envs;
    }

    /// Get the environment as captured at the last `execve`.
    pub fn environ(&self) -> Vec<String> {
        self.inner.lock().environ.clone()
    }

    /// Get the current program break (top of heap).
    pub fn brk(&self) -> usize {
        self.inner.lock().brk
    }

    /// Set the current program break.
    pub fn set_brk(&self, brk: usize) {
        self.inner.lock().brk = brk;
    }

    /// Get the heap address actually mapped by `sys_brk` (>= the user-visible
    /// `brk` whenever the heap has been grown in chunks). Zero before the
    /// first `sys_brk`.
    pub fn mapped_brk(&self) -> usize {
        self.inner.lock().mapped_brk
    }

    /// Record the new upper bound of the heap's actual mapping.
    pub fn set_mapped_brk(&self, mapped_brk: usize) {
        self.inner.lock().mapped_brk = mapped_brk;
    }

    /// Get signal action.
    pub fn signal_action(&self, signal: LinuxSignal) -> SignalAction {
        self.inner.lock().signal_actions.table[signal as u8 as usize]
    }

    /// Set signal action.
    pub fn set_signal_action(&self, signal: LinuxSignal, action: SignalAction) {
        self.inner.lock().signal_actions.table[signal as u8 as usize] = action;
    }

    /// Reset signal dispositions across `execve`, as POSIX requires: every
    /// signal that was being *caught* (a custom handler) is restored to
    /// `SIG_DFL`; signals set to `SIG_IGN` or already `SIG_DFL` are left
    /// untouched. Without this, a `fork`+`exec`'d child keeps the parent
    /// shell's handler addresses; since busybox is one static binary, those
    /// addresses are still valid code in the new image, so a delivered signal
    /// (e.g. SIGINT) jumps into the shell's handler with the new applet's
    /// uninitialised globals (`ptr_to_globals == NULL`) and crashes.
    pub fn reset_signal_actions_for_exec(&self) {
        use crate::signal::{SIG_DFL, SIG_IGN};
        let mut inner = self.inner.lock();
        for action in inner.signal_actions.table.iter_mut() {
            if action.handler != SIG_DFL && action.handler != SIG_IGN {
                *action = SignalAction::default();
            }
        }
    }

    /// Close file that FD_CLOEXEC is set
    pub fn remove_cloexec_files(&self) {
        // Remove under the lock, DROP outside it — see `close_file` for the
        // re-entrancy deadlock this avoids.
        type RemovedFds = Vec<(FileDesc, Arc<dyn FileLike>)>;
        let (removed, exec_path): (RemovedFds, String) = {
            let mut inner = self.inner.lock();
            // Per-fd state is authoritative — NOT the flag inside the (possibly
            // fork-shared) `File` objects, which is only a creation-time record.
            let close_fds = inner.cloexec_fds.drain().collect::<Vec<_>>();
            let removed = close_fds
                .into_iter()
                .filter_map(|fd| inner.files.remove(&fd).map(|f| (fd, f)))
                .collect();
            (removed, inner.execute_path.clone())
        };
        for (fd, f) in removed {
            // DRM diagnostics: removal of a DRM/dmabuf fd — see
            // fs::drm_fd_desc. debug level: this is NORMAL CLOEXEC behavior
            // (it fires on every execve of a process holding DRM fds); it
            // earned its keep during the stale-fd hunts and stays available
            // under LOG=debug without spamming ordinary boots.
            if let Some(desc) = crate::fs::drm_fd_desc(&f) {
                debug!(
                    "[drm] fd {:?} ({}) closed by execve CLOEXEC sweep",
                    fd, desc
                );
            }
            // Pipe diagnostics: a pipe end swept at exec is exactly the
            // dbus-launch --print-address failure shape (child inherits a
            // pipe fd across exec and the daemon writes the bus address
            // into it). Any hit here names the culprit immediately.
            // debug level: sweeping a CLOEXEC pipe end at exec is normal
            // (every shell pipeline does it); the dbus-launch bug it was
            // added for is fixed, and under LOG=debug the trace remains.
            if let Some(file) = f.downcast_ref::<crate::fs::File>() {
                let p = file.path();
                if p.starts_with("pipe_") {
                    debug!(
                        "[cloexec] {} fd={:?} ({}) closed by execve CLOEXEC sweep",
                        exec_path, fd, p
                    );
                }
            }
        }
    }

    /// Insert a `SemArray` and return its ID
    pub fn semaphores_add(&self, array: Arc<SemArray>) -> usize {
        self.inner.lock().semaphores.add(array)
    }

    /// Get an semaphore set by `id`
    pub fn semaphores_get(&self, id: usize) -> Option<Arc<SemArray>> {
        self.inner.lock().semaphores.get(id)
    }

    /// Add an undo operation
    pub fn semaphores_add_undo(&self, id: usize, num: u16, op: i16) {
        self.inner.lock().semaphores.add_undo(id, num, op)
    }

    /// Remove an `SemArray` by ID
    pub fn semaphores_remove(&self, id: usize) {
        self.inner.lock().semaphores.remove(id)
    }

    /// get ShmId from Virtual Addr
    pub fn shm_get_id(&self, id: usize) -> Option<usize> {
        self.inner.lock().shm_identifiers.get_id(id)
    }

    /// get the ShmIdentifier from shm_identifiers
    pub fn shm_get(&self, id: usize) -> Option<ShmIdentifier> {
        self.inner.lock().shm_identifiers.get(id)
    }

    /// Delete the ShmIdentifier from shm_identifiers
    pub fn shm_pop(&self, id: usize) {
        self.inner.lock().shm_identifiers.pop(id)
    }

    /// Record that this process is using the segment `id` names.
    pub fn shm_add(&self, id: usize, shared_guard: Arc<Mutex<ShmGuard>>) {
        self.inner.lock().shm_identifiers.add(id, shared_guard)
    }

    /// Set Virtual Addr for shared memory
    pub fn shm_set(&self, id: usize, shm_id: ShmIdentifier) {
        self.inner.lock().shm_identifiers.set(id, shm_id)
    }
}

impl LinuxProcessInner {
    /// Everything a `fork(2)` child starts life with, decided field by field.
    ///
    /// Written out in full, with no `..Default::default()`, on purpose: a
    /// field that falls through to its default silently gives the child a
    /// *fresh* value where Linux gives it the parent's, and nothing says so
    /// at the fork site. Four fields were getting exactly that. Spelling
    /// every field out makes the compiler ask the question again each time
    /// one is added.
    /// Honour the set-user-ID / set-group-ID bits of the image `execve` is
    /// loading, and report Linux's `bprm->secureexec`: whether the image about
    /// to run is more privileged than whoever asked for it.
    ///
    /// That answer decides two things -- the parent-death signal
    /// [`Self::reset_for_exec`] drops, and the `AT_SECURE` the C library reads
    /// before `main` ([`crate::loader::AuxIdentity`]) -- so it has to be
    /// the whole rule, which `cap_bprm_creds_from_file()` (security/commoncap.c)
    /// spells out as three questions, any one of which is enough:
    ///
    /// ```text
    /// if (id_changed ||                        // this image granted an id
    ///     !uid_eq(new->euid, old->uid) ||      // effective != REAL uid
    ///     !gid_eq(new->egid, old->gid) || ...) // effective != REAL gid
    ///         bprm->secureexec = 1;
    /// ```
    ///
    /// The first question is about this exec; the other two are about the
    /// process, and they are the ones that catch the case with no set-user-ID
    /// bit in sight: a program that is *already* running set-user-ID root
    /// keeps its effective id across `execve`, so the ordinary shell it execs
    /// is every bit as privileged -- and every bit as unable to trust the
    /// environment it was handed -- as the image that raised it.
    fn apply_exec_ids(&mut self, mode: u16, uid: u32, gid: u32, may_suid: bool) -> bool {
        // The ids this exec starts from. `execve` never moves the real ones,
        // so `ruid`/`rgid` below are `old->uid`/`old->gid` as well.
        let old_euid = self.credentials.euid;
        let old_egid = self.credentials.egid;

        // `bprm_fill_uid()` asks two questions before it honours a bit, and
        // leaves without honouring either if the answer to one of them is no:
        // `mnt_may_suid(file->f_path.mnt)` -- was the filesystem mounted
        // `nosuid`? -- and `task_no_new_privs(current)`. It then goes on to
        // compute `secureexec` regardless, which is the point of doing them
        // here and not at the return: refusing to raise a process says nothing
        // about how privileged it already was.
        if may_suid && !self.no_new_privs {
            if (mode & MODE_SET_UID) != 0 {
                self.credentials.euid = uid;
                self.credentials.suid = uid;
            }
            // `(mode & (S_ISGID | S_IXGRP)) == (S_ISGID | S_IXGRP)`: a
            // set-group-ID bit on a file the group cannot execute is not a
            // set-group-ID program at all.
            if (mode & (MODE_SET_GID | MODE_EXEC_GRP)) == (MODE_SET_GID | MODE_EXEC_GRP) {
                self.credentials.egid = gid;
                self.credentials.sgid = gid;
            }
        }

        // `id_changed = !uid_eq(new->euid, old->euid) || !in_group_p(new->egid)`.
        // The group half is deliberately not a comparison: joining a group the
        // caller was already a member of grants nothing, so Linux asks whether
        // the new effective group is one the caller already had.
        let egid = self.credentials.egid;
        let kept_group = egid == old_egid || self.credentials.groups.contains(&egid);
        let id_changed = self.credentials.euid != old_euid || !kept_group;

        let secure = id_changed
            || self.credentials.euid != self.credentials.ruid
            || self.credentials.egid != self.credentials.rgid;

        // `cap_bprm_creds_from_file()` ends with
        // `new->suid = new->fsuid = new->euid; new->sgid = new->fsgid =
        // new->egid;`. This path latches its own taint below instead of going
        // through `note_id_change`, so it asks for the line itself.
        self.follow_effective_ids();

        // `do_execve()` asks the same three questions to decide FreeBSD's
        // `P_SUGID`: `setsugid(p)` when the image granted an id, and
        // `p->p_flag &= ~P_SUGID` only when it did not AND the effective ids
        // already match the real ones. That is this `secure`, so an exec is
        // the one event that can clear the taint -- by answering it again.
        self.sugid = secure;

        secure
    }

    /// The six ids `issetugid(2)` watches.
    /// How many descriptors this process may have open: the soft
    /// `RLIMIT_NOFILE`, and the only limit of the sixteen anything enforces.
    fn nofile(&self) -> usize {
        self.limits
            .get(RLIMIT_NOFILE)
            .map(|l| l.cur as usize)
            .unwrap_or(usize::MAX)
    }

    fn ids(&self) -> [u32; 6] {
        let c = &self.credentials;
        [c.ruid, c.euid, c.suid, c.rgid, c.egid, c.sgid]
    }

    /// FreeBSD's `setsugid()`: a `set*id` call that actually MOVED an id
    /// taints the process for good. Written as a before/after comparison
    /// rather than a line in each setter, because nine setters each
    /// remembering to latch a flag is nine chances to forget one -- and the
    /// one that forgets is a program that asks whether it is tainted and is
    /// told no.
    fn note_id_change(&mut self, before: [u32; 6]) {
        self.follow_effective_ids();
        if self.ids() != before {
            self.sugid = true;
        }
    }

    /// `new->fsuid = new->euid;` -- the last line of every `set*id` path in
    /// `kernel/sys.c`, and of `cap_bprm_creds_from_file()` on the exec path.
    /// Re-applies the effective ids through [`Credentials::set_euid`], the
    /// one place in this kernel that writes a filesystem id from an
    /// effective one. At the END of the path, not beside the write, which is
    /// where Linux puts it: `setreuid(ruid, -1)` names no effective id and
    /// still brings a wandered filesystem id home.
    ///
    /// One deliberate divergence: `kernel/sys.c` opens `__sys_setresuid`
    /// with a "check for no-op" that returns before touching credentials, so
    /// a `setresuid(-1, -1, -1)` there leaves a moved filesystem id where it
    /// is. Here every path reaches this line, so it comes home. The
    /// direction is the safe one -- it can only put back the id the caller
    /// is already acting as everywhere else -- and the alternative is a
    /// second copy of "did this call ask for anything?" in three setters.
    ///
    /// Written once for the same reason [`Self::note_id_change`] is: nine
    /// setters each remembering a line is nine chances to forget one, and the
    /// one that forgets leaves a process whose file accesses are still
    /// checked against an id it just gave away. `setfsuid(2)` is the only
    /// call that moves a filesystem id on its own, and it is the only one
    /// that does not come through here.
    fn follow_effective_ids(&mut self) {
        let (euid, egid) = (self.credentials.euid, self.credentials.egid);
        self.credentials.set_euid(euid);
        self.credentials.set_egid(egid);
    }

    /// `apply_exec_ids` for an image on an ordinary mount, which is what every
    /// test that is not about `nosuid` means.
    #[cfg(test)]
    fn apply_exec_ids_from_a_normal_mount(&mut self, mode: u16, uid: u32, gid: u32) -> bool {
        self.apply_exec_ids(mode, uid, gid, true)
    }

    /// The aux-vector identity block for the image about to be loaded.
    ///
    /// The mapping is written once, here, because it is the kind that reads
    /// correct either way round: `AT_UID` is the REAL user id and `AT_EUID`
    /// the effective one, and a swap leaves a program that is told the
    /// opposite of the truth about which id its accesses are checked against.
    fn aux_identity(&self, privileged: bool) -> AuxIdentity {
        AuxIdentity {
            uid: self.credentials.ruid,
            euid: self.credentials.euid,
            gid: self.credentials.rgid,
            egid: self.credentials.egid,
            secure: privileged,
        }
    }

    /// What `execve` must make the process forget, once the old address space
    /// is gone. `privileged` is `bprm->secureexec`.
    fn reset_for_exec(&mut self, privileged: bool) {
        // Every System V segment this process had attached was mapped in the
        // address space `execve` just cleared; Linux unmaps them with the
        // rest of the old mm and each `shm_close` accounts its detach. Kept,
        // these entries claim attachments at addresses that now belong to the
        // NEW image, and `shmdt` trusts them: it looks the address up in this
        // very map and unmaps that many bytes there (`sys_shmdt`), so a
        // detach of a segment the process no longer has punches a hole in the
        // new program. The segment's attach count never drops either, so an
        // `IPC_RMID` on it frees nothing.
        self.shm_identifiers = Default::default();

        // `begin_new_exec()`: `me->flags &= ~PF_FORKNOEXEC`. From here on the
        // process runs an image of its own choosing, and a `setpgid` from the
        // parent must be refused with `EACCES`. See `has_execed`.
        self.has_execed = true;

        // `begin_new_exec()`: `if (bprm->secureexec) me->pdeath_signal = 0;`
        // The parent picked this signal while the child was still running
        // code the parent controlled. Keeping it across a privilege-raising
        // exec would let an unprivileged parent arrange for the now-
        // privileged process to be signalled the moment the parent exits --
        // which is precisely the parent's own choice of moment.
        if privileged {
            self.pdeathsig = 0;
        }

        // Left alone on purpose, because execve(2) and friends say so: the
        // file table (minus close-on-exec, done by `remove_cloexec_files`),
        // the credentials, the working directory, the process group and
        // session, `no_new_privs`, the resource limits, and the `setitimer`
        // interval timers, which setitimer(2) preserves across an exec.
        // `brk`/`mapped_brk`, `environ`, `cmdline`, `execute_path` and `abi`
        // are all overwritten by the caller from the new image.
    }

    fn forked_child(&self, pgid: KoID, sid: KoID) -> Self {
        LinuxProcessInner {
            // --- copied from the parent -------------------------------------
            execute_path: self.execute_path.clone(),
            cmdline: self.cmdline.clone(),
            // `/proc/<pid>/environ` reads the process's own memory in Linux,
            // and a fork copies that memory, so a child that has not exec'd
            // still reports the parent's environment.
            environ: self.environ.clone(),
            current_working_directory: self.current_working_directory.clone(),
            files: self.files.clone(),
            // POSIX fork(2): the child gets its own COPY of each fd's
            // FD_CLOEXEC flag — later fcntl(F_SETFD) in either process
            // must not affect the other.
            cloexec_fds: self.cloexec_fds.clone(),
            // RLIMIT_NOFILE survives fork, and this is the field that really
            // caps the fd table. Resetting it undid every `ulimit -n` the
            // moment the shell forked -- which is the only way a program ever
            // gets a raised limit.
            limits: self.limits,
            signal_actions: self.signal_actions.clone(),
            credentials: self.credentials.clone(),
            pgid,
            sid,
            // fork(2)/prctl(2) inheritance: no_new_privs, dumpable, the
            // execution domain and THP setting carry over.
            no_new_privs: self.no_new_privs,
            // `p2->p_flag |= p1->p_flag & P_SUGID` (kern_fork.c). The child
            // is a copy of an address space someone else's libc filled in,
            // so it inherits the doubt along with the memory.
            sugid: self.sugid,
            dumpable: self.dumpable,
            personality: self.personality,
            abi: self.abi,
            thp_disable: self.thp_disable,
            // The heap. `fork` copies the address space, so the heap is there
            // in the child -- but the bookkeeping that says where it ends was
            // starting from zero, and `sys_brk` answers every call with the
            // old break when the break is below the heap base. So `brk` in a
            // forked child could not move at all, and `sbrk(0)` reported 0.
            // A child that never execs -- a subshell, a zygote -- had its
            // allocator silently pushed onto mmap for the rest of its life.
            brk: self.brk,
            mapped_brk: self.mapped_brk,
            // shmat(2): the attachments come along with the copied address
            // space, so the child must be able to `shmdt` them. Without the
            // record it cannot, and the mapping stays for the child's life.
            shm_identifiers: self.shm_identifiers.clone(),

            // --- deliberately fresh -----------------------------------------
            // A child has no children of its own, and no accumulated times
            // for them: Linux zeroes `cutime`/`cstime` in `copy_process`.
            children: Default::default(),
            reaped_children: Default::default(),
            children_utime_ns: 0,
            children_stime_ns: 0,
            // `p->pdeath_signal = 0` in `copy_process`, and the subreaper
            // attribute is the parent's own role, not a child's.
            pdeathsig: 0,
            child_subreaper: false,
            // `copy_process`: `p->flags |= PF_FORKNOEXEC`. A fresh child is
            // still running the forking program, so its parent may still put
            // it in a process group (the shell's job-control idiom).
            has_execed: false,
            // Not stopped, and nothing pending to report to a waiter.
            job_stopped: false,
            job_stop_sig: 0,
            job_stop_pending: false,
            job_continued_pending: false,
            // Kernel-side futex objects are keyed by address in *this*
            // address space; the child gets its own.
            futexes: Default::default(),
            // `SemProc`'s own `Clone` says what a fork needs -- "Fork the
            // semaphore table. Clear undo info." -- and fork was not using
            // it. Through `..Default::default()` the child lost the sets its
            // parent had open as well, and the ids are per-process indices,
            // so an id the parent passed down named nothing in the child
            // until it did its own `semget`. SEM_UNDO really is not
            // inherited by a plain fork (only `CLONE_SYSVSEM` shares it), and
            // that is exactly what the `Clone` drops.
            semaphores: self.semaphores.clone(),
        }
    }

    /// Leave a job-control stop. `continued` says whether this is a real
    /// `SIGCONT` -- which owes the parent a `WCONTINUED` notification -- or a
    /// wake-up to die under `SIGKILL`, which owes it nothing: the process is
    /// not continuing, and telling `wait` that it did would have the parent
    /// report a job as resumed at the moment it was killed.
    ///
    /// Either way the pending STOP notification goes: a stop the parent never
    /// collected is not news any more once the process is out of the stop,
    /// and left behind it would be reported as a current state.
    ///
    /// Returns whether the process was in fact stopped.
    fn leave_stop(&mut self, continued: bool) -> bool {
        let was = self.job_stopped;
        self.job_stopped = false;
        if was {
            self.job_stop_pending = false;
            self.job_continued_pending = continued;
        }
        was
    }

    /// Fold a reaped child's CPU usage into the RUSAGE_CHILDREN totals.
    fn add_children_cpu(&mut self, cpu: ChildCpu) {
        self.children_utime_ns += cpu.utime_ns;
        self.children_stime_ns += cpu.stime_ns;
    }

    fn get_free_fd(&self) -> FileDesc {
        self.get_free_fd_from(0)
    }

    fn get_free_fd_from(&self, start: usize) -> FileDesc {
        (start..)
            .map(|i| i.into())
            .find(|fd| !self.files.contains_key(fd))
            .unwrap()
    }
}
/// Deliver SIGINT to the foreground terminal process group (job control).
pub fn deliver_sigint_to_foreground() {
    let pgid = crate::fs::stdio::get_foreground_pgrp();
    if pgid > 0 {
        let _ = send_signal_to_pgrp(pgid as usize, LinuxSignal::SIGINT);
        return;
    }
    if let Some(arc) = kernel_hal::thread::get_current_thread() {
        if let Ok(thread) = arc.downcast::<Thread>() {
            let _ = send_signal_to_process(thread.proc().id() as usize, LinuxSignal::SIGINT);
        }
    }
}

/// First Ctrl-C for a foreground group delivers `SIGINT` (graceful, as before).
/// A **second** Ctrl-C for the same `pgid` while the arm is still set escalates
/// to `SIGKILL`, so a process that ignores or hangs on the interrupt can be
/// torn down without closing the terminal. Changing the tty's foreground
/// group ([`clear_interrupt_arm`]) clears the arm so a later job starts fresh.
///
/// `armed_pgid` is per-tty state (console VT or pty pair): two terminals must
/// not share it, or a Ctrl-C in one would escalate the other.
pub fn interrupt_or_force_pgrp(pgid: i32, armed_pgid: &AtomicI32) -> LinuxSignal {
    if pgid <= 0 {
        return LinuxSignal::SIGINT;
    }
    let prev = armed_pgid.load(Ordering::Relaxed);
    if prev == pgid {
        armed_pgid.store(0, Ordering::Relaxed);
        let _ = send_signal_to_pgrp(pgid as usize, LinuxSignal::SIGKILL);
        zcore_drivers::klog_warn!("[tty] second Ctrl-C on pgrp {} -> SIGKILL (forced)", pgid);
        LinuxSignal::SIGKILL
    } else {
        armed_pgid.store(pgid, Ordering::Relaxed);
        let _ = send_signal_to_pgrp(pgid as usize, LinuxSignal::SIGINT);
        LinuxSignal::SIGINT
    }
}

/// Drop the double-Ctrl-C arm (new foreground job, ctty change, etc.).
pub fn clear_interrupt_arm(armed_pgid: &AtomicI32) {
    armed_pgid.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod interrupt_escalate_tests {
    use super::*;
    use core::sync::atomic::AtomicI32;

    #[test]
    fn first_ctrl_c_arms_sigint_second_escalates_to_sigkill() {
        let armed = AtomicI32::new(0);
        // No live members → ESRCH inside send, but the arm / escalate choice
        // is independent of that.
        assert_eq!(
            interrupt_or_force_pgrp(4242, &armed),
            LinuxSignal::SIGINT,
            "first press is graceful"
        );
        assert_eq!(armed.load(Ordering::Relaxed), 4242);
        assert_eq!(
            interrupt_or_force_pgrp(4242, &armed),
            LinuxSignal::SIGKILL,
            "second press for the same pgrp is forced"
        );
        assert_eq!(armed.load(Ordering::Relaxed), 0, "arm clears after force");
        assert_eq!(
            interrupt_or_force_pgrp(4242, &armed),
            LinuxSignal::SIGINT,
            "a third press starts over with SIGINT"
        );
    }

    #[test]
    fn a_different_pgrp_does_not_inherit_the_force_arm() {
        let armed = AtomicI32::new(0);
        assert_eq!(interrupt_or_force_pgrp(100, &armed), LinuxSignal::SIGINT);
        assert_eq!(
            interrupt_or_force_pgrp(200, &armed),
            LinuxSignal::SIGINT,
            "a new job gets a fresh SIGINT, not an inherited SIGKILL"
        );
        assert_eq!(armed.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn clear_interrupt_arm_resets_escalation() {
        let armed = AtomicI32::new(0);
        assert_eq!(interrupt_or_force_pgrp(55, &armed), LinuxSignal::SIGINT);
        clear_interrupt_arm(&armed);
        assert_eq!(
            interrupt_or_force_pgrp(55, &armed),
            LinuxSignal::SIGINT,
            "after clear, the next Ctrl-C is SIGINT again"
        );
    }
}

/// This process's effective process-group id: its raw pgid, or its own pid when
/// the raw value is unset (`0`).
pub fn effective_pgid(proc: &Arc<Process>) -> KoID {
    // try_linux: called while walking all_live_processes() (e.g. send_signal_to_pgrp
    // on Ctrl-C), so `proc` may be tearing down concurrently under SMP churn.
    // Fall back to the pid when the extension can no longer be resolved.
    let raw = proc.try_linux().map(|lp| lp.pgid_raw()).unwrap_or(0);
    if raw == 0 {
        proc.id()
    } else {
        raw
    }
}

/// Deliver `signal` to every live process in process group `pgid`. This is the
/// POSIX behaviour for terminal-generated signals (Ctrl-C → SIGINT, Ctrl-\\ →
/// SIGQUIT, Ctrl-Z → SIGTSTP): the whole foreground group is signalled, not
/// just the group leader, so a shell's foreground child (e.g. `ping`) actually
/// receives it. `ESRCH` if the group has no members.
pub fn send_signal_to_pgrp(pgid: usize, signal: LinuxSignal) -> LxResult<()> {
    let pgid = pgid as KoID;
    let mut any = false;
    let members: Vec<Arc<Process>> = all_live_processes()
        .into_iter()
        .filter(|p| effective_pgid(p) == pgid)
        .collect();
    if signal_trace_worthy(signal) {
        // The group model here is "pgid == some ancestor's pid" and pids are
        // recycled fast, so a stale pgrp number can name a process that never
        // belonged to the job. Say exactly who a group signal fans out to.
        let (spid, sname) = current_process_pid_name();
        let mut list = String::new();
        for p in &members {
            if !list.is_empty() {
                list.push_str(", ");
            }
            list.push_str(&alloc::format!("{} ({})", p.id(), trace_name(p)));
        }
        zcore_drivers::klog_warn!(
            "[signal] {:?} to pgrp {} from pid {} ({}) -> [{}]",
            signal,
            pgid,
            spid,
            sname,
            list
        );
    }
    for proc in members {
        if send_signal_to_process(proc.id() as usize, signal).is_ok() {
            any = true;
        }
    }
    if any {
        Ok(())
    } else {
        Err(LxError::ESRCH)
    }
}

/// Pulse the parent's zircon `SIGCHLD` so a blocking `wait*` wakes for a
/// stop/continue (exit already does the same from the terminate callback),
/// and send it the Linux `SIGCHLD` a handler or signalfd waits for, unless
/// its `sigaction` said `SA_NOCLDSTOP` (`do_notify_parent_cldstop`).
fn notify_parent_child_state(child: &Arc<Process>) {
    let parent = child.try_linux().and_then(|lp| lp.parent());
    let parent = match parent {
        Some(p) => p,
        None => match ROOT_JOB.find_process(INIT_PID) {
            Some(p) => p,
            None => return,
        },
    };
    parent.signal_set(Signal::SIGCHLD);
    if parent_wants_sigchld_for_stops(&parent) {
        let info = child.try_linux().map(|lp| {
            let (uid, status) = {
                let inner = lp.inner.lock();
                let status = if inner.job_stopped {
                    wait_status_stopped(inner.job_stop_sig)
                } else {
                    WAIT_STATUS_CONTINUED
                };
                (inner.credentials.ruid, status)
            };
            SigInfo::child_state_change(child.id() as i32, uid, status)
        });
        let _ = send_signal_to_process_with_info(parent.id() as usize, LinuxSignal::SIGCHLD, info);
    }
}

/// Whether a stop or continue of a child is reported to `parent` as a Linux
/// `SIGCHLD`: it is unless the parent's `SIGCHLD` action carries
/// `SA_NOCLDSTOP`, the flag whose whole meaning is "only tell me when they
/// die".
fn parent_wants_sigchld_for_stops(parent: &Arc<Process>) -> bool {
    parent
        .try_linux()
        .map(|lp| {
            !lp.signal_action(LinuxSignal::SIGCHLD)
                .flags
                .contains(crate::signal::SignalActionFlags::NOCLDSTOP)
        })
        .unwrap_or(false)
}

/// Park the current task until this process leaves a job-control stop (or dies).
pub async fn wait_while_job_stopped(proc: &Arc<Process>) {
    loop {
        let stopped = proc
            .try_linux()
            .map(|lp| lp.is_job_stopped())
            .unwrap_or(false);
        if !stopped || matches!(proc.status(), Status::Exited(_)) {
            return;
        }
        proc.signal_clear(JOB_CONTINUE_SIGNAL);
        let stopped = proc
            .try_linux()
            .map(|lp| lp.is_job_stopped())
            .unwrap_or(false);
        if !stopped || matches!(proc.status(), Status::Exited(_)) {
            return;
        }
        let obj: Arc<dyn KernelObject> = proc.clone();
        obj.wait_signal(JOB_CONTINUE_SIGNAL).await;
    }
}

/// Everything `setpgid(2)` decides on, read off the two processes involved
/// before any of them is touched.
///
/// A process group is a KILL LIST: [`send_signal_to_pgrp`] walks every live
/// process whose effective pgid matches and signals it, and the terminal's
/// Ctrl-C/Ctrl-\\/Ctrl-Z do exactly that to the foreground group. So "which
/// process may be put into which group" is an access-control decision, and
/// `kernel/sys.c:do_setpgid` spells out four rules for it. This kernel had
/// none of them -- any process could move ANY other process, in any session,
/// into its own group, and then have the tty signal it.
#[derive(Debug, Clone, Copy)]
pub struct SetpgidFacts {
    /// Pid of the process making the call.
    pub caller_pid: KoID,
    /// Effective session id of the caller.
    pub caller_sid: KoID,
    /// Pid of the process being moved.
    pub target_pid: KoID,
    /// Effective session id of the target.
    pub target_sid: KoID,
    /// The target is a child of the caller.
    pub target_is_child: bool,
    /// The target has already `execve`'d (`!PF_FORKNOEXEC`).
    pub target_has_execed: bool,
    /// The target is a session leader (its effective sid is its own pid).
    pub target_is_session_leader: bool,
    /// The group the target is being moved into.
    pub new_pgid: KoID,
    /// Some live process in the CALLER's session already has `new_pgid` as its
    /// effective process group.
    pub group_exists_in_caller_session: bool,
}

/// `kernel/sys.c:do_setpgid`, rule for rule and in its order.
pub fn setpgid_verdict(f: &SetpgidFacts) -> LxResult<()> {
    if f.target_is_child {
        // A child may be moved only within the caller's own session: a
        // process that has left for a session of its own (a daemon, another
        // terminal's shell) is no longer this shell's to job-control.
        if f.target_sid != f.caller_sid {
            return Err(LxError::EPERM);
        }
        // `if (!(p->flags & PF_FORKNOEXEC)) return -EACCES`. The window in
        // which a parent may group its child closes at the child's `execve`:
        // after it the child runs a program that never agreed to this.
        if f.target_has_execed {
            return Err(LxError::EACCES);
        }
    } else if f.target_pid != f.caller_pid {
        // Neither the caller nor a child of it. Linux reports this as "no
        // such process" rather than EPERM, so a caller cannot use `setpgid`
        // to probe which pids exist outside its own family.
        return Err(LxError::ESRCH);
    }
    // A session leader is pinned to the group that carries its own pid: its
    // pid IS the session id, and letting it wander would leave the session
    // named after a group it is not in.
    if f.target_is_session_leader {
        return Err(LxError::EPERM);
    }
    // Joining an EXISTING group (`pgid != pid`) requires that group to exist
    // in the caller's session. Creating one (`pgid == pid`) always may.
    if f.new_pgid != f.target_pid && !f.group_exists_in_caller_session {
        return Err(LxError::EPERM);
    }
    Ok(())
}

/// `setpgid`: `caller` puts process `pid` into process group `pgid`.
///
/// The rules are [`setpgid_verdict`]'s; everything here only reads the facts
/// it decides on off the live process table.
pub fn set_process_pgid(caller: &Arc<Process>, pid: KoID, pgid: KoID) -> LxResult<()> {
    let live = all_live_processes();
    let proc = live
        .iter()
        .find(|p| p.id() == pid)
        .cloned()
        .ok_or(LxError::ESRCH)?;
    let target_linux = proc.try_linux().ok_or(LxError::ESRCH)?;
    let caller_sid = effective_sid(caller);
    let target_sid = effective_sid(&proc);
    let facts = SetpgidFacts {
        caller_pid: caller.id(),
        caller_sid,
        target_pid: pid,
        target_sid,
        target_is_child: caller
            .try_linux()
            .map(|lp| lp.has_child(pid))
            .unwrap_or(false),
        target_has_execed: target_linux.has_execed(),
        target_is_session_leader: target_sid == pid,
        new_pgid: pgid,
        group_exists_in_caller_session: live
            .iter()
            .any(|p| effective_pgid(p) == pgid && effective_sid(p) == caller_sid),
    };
    setpgid_verdict(&facts)?;
    target_linux.set_pgid_raw(pgid);
    Ok(())
}

/// `setsid(2)`: may `caller_pid` start a session of its own?
///
/// `kernel/sys.c:ksys_setsid` refuses when the caller's pid already NAMES a
/// process group (`pid_task(pid, PIDTYPE_PGID)`), because the new session
/// would be given that same number for its own group and two unrelated groups
/// would end up sharing an id. The usual case is the caller's own group --
/// which is why the daemonize idiom forks first -- but a child the caller put
/// into a group named after the caller counts just the same.
pub fn setsid_verdict(caller_pid: KoID, live_pgids: &[KoID]) -> LxResult<()> {
    if live_pgids.contains(&caller_pid) {
        debug!("setsid: pid {} already names a process group", caller_pid);
        return Err(LxError::EPERM);
    }
    Ok(())
}

/// The effective process group of every live process, for [`setsid_verdict`].
pub fn live_effective_pgids() -> Vec<KoID> {
    all_live_processes().iter().map(effective_pgid).collect()
}

/// `getpgid`: the effective process-group id of process `pid`.
pub fn get_process_pgid(pid: KoID) -> LxResult<KoID> {
    let proc = all_live_processes()
        .into_iter()
        .find(|p| p.id() == pid)
        .ok_or(LxError::ESRCH)?;
    Ok(effective_pgid(&proc))
}

/// This process's effective session id: its raw sid, or its own pid when the
/// raw value is unset (`0`).
pub fn effective_sid(proc: &Arc<Process>) -> KoID {
    // try_linux: same teardown-race tolerance as `effective_pgid`.
    let raw = proc.try_linux().map(|lp| lp.sid_raw()).unwrap_or(0);
    if raw == 0 {
        proc.id()
    } else {
        raw
    }
}

/// `getsid`: the effective session id of process `pid`.
pub fn get_process_sid(pid: KoID) -> LxResult<KoID> {
    let proc = all_live_processes()
        .into_iter()
        .find(|p| p.id() == pid)
        .ok_or(LxError::ESRCH)?;
    Ok(effective_sid(&proc))
}

pub fn check_and_deliver_tty_interrupt() -> LxResult<()> {
    if crate::fs::stdio::ctrl_c_pending_take() {
        deliver_sigint_to_foreground();
        return Err(LxError::EINTR);
    }
    check_signals()
}

fn collect_live_processes(job: &Arc<Job>, out: &mut Vec<Arc<Process>>) {
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
                collect_live_processes(&child_job, out);
            }
        }
    }
}

/// All non-exited processes in the root job tree.
pub fn all_live_processes() -> Vec<Arc<Process>> {
    let mut processes = Vec::new();
    collect_live_processes(&ROOT_JOB, &mut processes);
    processes
}

/// The process `pid` names, exited or not.
///
/// `find_task_by_vpid()`: the lookup a syscall that takes a pid does before
/// it can ask anything else about it.
pub fn find_process(pid: KoID) -> Option<Arc<Process>> {
    ROOT_JOB.find_process(pid)
}

/// The real uid of the process `pid` names, exited or not; 0 when there is
/// no such process. What `si_uid` carries in a `SIGCHLD` (`task_uid`).
pub fn real_uid_of(pid: KoID) -> u32 {
    find_process(pid)
        .and_then(|p| p.try_linux().map(|lp| lp.credentials().ruid))
        .unwrap_or(0)
}

/// Whether `pid` names a process that has not exited.
///
/// The same lookup `send_signal_to_process` does, minus the processes that
/// have already exited and are only waiting to be reaped: a zombie cannot act
/// on a signal, so for anything that asks "is somebody still there to do
/// this?" it is not there.
pub fn process_exists(pid: KoID) -> bool {
    ROOT_JOB
        .find_process(pid)
        .map(|p| !matches!(p.status(), Status::Exited(_)))
        .unwrap_or(false)
}

/// Linux PID of the `init` process (the base program). The system's reaper of
/// last resort for orphaned children, and the one process a `kill(-1, sig)`
/// broadcast must never reach.
pub const INIT_PID: KoID = 1;

/// Live INIT (PID 1) process, or `None` if there is no running init.
/// True for the Linux magic exe links that must not replace a real
/// `execute_path`. Prefer matching `/proc/self/exe` or `/proc/<own_pid>/exe`
/// when the calling thread is known; otherwise accept any `/proc/<digits>/exe`
/// so early `spawn` (no current thread yet) still preserves a prior path.
fn is_proc_exe_magic(path: &str) -> bool {
    if path == "/proc/self/exe" {
        return true;
    }
    let Some(rest) = path.strip_prefix("/proc/") else {
        return false;
    };
    let Some(digits) = rest.strip_suffix("/exe") else {
        return false;
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(link_pid) = digits.parse::<u64>() else {
        return false;
    };
    match kernel_hal::thread::get_current_thread()
        .and_then(|t| t.downcast::<Thread>().ok())
        .map(|t| t.proc().id())
    {
        Some(own_pid) => link_pid == own_pid,
        // Loader spawn has no "current" thread yet; treat numeric /proc/N/exe
        // as magic only when execute_path is already set (caller checks that).
        None => true,
    }
}

fn live_init() -> Option<Arc<Process>> {
    let init = ROOT_JOB.find_process(INIT_PID)?;
    if matches!(init.status(), Status::Exited(_)) {
        None
    } else {
        Some(init)
    }
}

/// Nearest live ancestor of `proc` that volunteered as a child subreaper via
/// `prctl(PR_SET_CHILD_SUBREAPER)` — prctl(2): orphans are reparented to it
/// instead of init (how session managers like tmux or `systemd --user` adopt
/// their descendants). The walk is capped so a corrupted parent chain can
/// never wedge the teardown path.
fn nearest_live_subreaper(proc: &Arc<Process>) -> Option<Arc<Process>> {
    let mut cur = proc.try_linux()?.parent();
    for _ in 0..64 {
        let p = cur?;
        if matches!(p.status(), Status::Exited(_)) {
            cur = p.try_linux()?.parent();
            continue;
        }
        match p.try_linux() {
            Some(lp) if lp.is_child_subreaper() => return Some(p),
            Some(lp) => cur = lp.parent(),
            None => return None,
        }
    }
    None
}

/// Choose who reaps a terminating child: its real parent while that parent is
/// still alive, otherwise the nearest live subreaper ancestor, otherwise INIT
/// (PID 1) — orphan reparenting. Returns `None` when nobody can take it (no
/// living parent, subreaper or init), so the exit status is dropped instead of
/// being leaked onto a dead process that will never `wait` for it.
fn reaper_for(parent: &Arc<Process>) -> Option<Arc<Process>> {
    if !matches!(parent.status(), Status::Exited(_)) {
        return Some(parent.clone());
    }
    if let Some(subreaper) = nearest_live_subreaper(parent) {
        return Some(subreaper);
    }
    // Parent already gone: hand the child to PID 1 (init), unless the parent
    // *is* init (it is exiting → the system is going down anyway).
    let init = live_init()?;
    if init.id() == parent.id() {
        None
    } else {
        Some(init)
    }
}

/// Reparent a terminating process's children so they are not stranded on a
/// dead parent that will never `wait` for them. The adopter is the nearest
/// live subreaper ancestor (`prctl(PR_SET_CHILD_SUBREAPER)`, prctl(2)) when
/// one exists, otherwise INIT (PID 1). Both still-live children and any
/// already-collected (zombie) exit statuses the dying process never reaped are
/// moved over, and the adopter is woken so a blocked `wait(-1)` observes the
/// adopted zombies at once. Live children that requested a parent-death signal
/// (`prctl(PR_SET_PDEATHSIG)`) receive it here — this is the moment their
/// parent dies. No-op when the dying process is INIT itself or nobody can
/// adopt (the orphans' exits then auto-reap via [`reaper_for`]).
fn reparent_live_children_to_init(dying: &Arc<Process>) {
    if dying.id() == INIT_PID {
        return;
    }
    let adopter = match nearest_live_subreaper(dying).or_else(live_init) {
        Some(adopter) => adopter,
        None => return,
    };
    let dying_linux = match dying.try_linux() {
        Some(lp) => lp,
        None => return,
    };
    let (orphans, zombies): (Vec<Arc<Process>>, Vec<ReapedChild>) = {
        let mut inner = dying_linux.inner.lock();
        let live_ids: Vec<KoID> = inner
            .children
            .iter()
            .filter(|(_, c)| !matches!(c.status(), Status::Exited(_)))
            .map(|(&id, _)| id)
            .collect();
        let orphans = live_ids
            .iter()
            .filter_map(|id| inner.children.remove(id))
            .collect();
        let zombies = inner.reaped_children.drain().collect();
        (orphans, zombies)
    };
    // Parent-death signals go out before the handover: the child asked to be
    // told when *this* parent dies, whoever adopts it afterwards.
    for orphan in &orphans {
        let sig = match orphan.try_linux() {
            Some(lp) => lp.pdeathsig(),
            None => 0,
        };
        if sig != 0 {
            if let Ok(signal) = LinuxSignal::try_from(sig) {
                let _ = send_signal_to_process(orphan.id() as usize, signal);
            }
        }
    }
    if orphans.is_empty() && zombies.is_empty() {
        return;
    }
    {
        let adopter_linux = match adopter.try_linux() {
            Some(lp) => lp,
            None => return,
        };
        let mut adopter_inner = adopter_linux.inner.lock();
        for orphan in orphans {
            // The adopter is the orphan's PARENT now, and the pointer has to
            // say so. It never did: `parent` was written once at fork and
            // never again, so after an adoption `getppid()` still named the
            // dead process (or 0 once its object went away) where Linux says
            // 1 -- which is the very thing the daemonize idiom waits for --
            // `notify_parent_child_state` pulsed SIGCHLD at the corpse
            // instead of at the adopter blocked in `wait`, and
            // `nearest_live_subreaper` walked a chain through the dead.
            if let Some(lp) = orphan.try_linux() {
                lp.set_parent(&adopter);
            }
            adopter_inner.children.insert(orphan.id(), orphan);
        }
        for (pid, entry) in zombies {
            adopter_inner.reaped_children.insert(pid, entry);
        }
    }
    adopter.signal_set(Signal::SIGCHLD);
}

/// Whether a signal with its *default* disposition interrupts a blocking
/// syscall.
fn signal_default_action_interrupts(sig: LinuxSignal) -> bool {
    !matches!(
        sig,
        LinuxSignal::SIGCHLD
            | LinuxSignal::SIGURG
            | LinuxSignal::SIGWINCH
            | LinuxSignal::SIGCONT
            | LinuxSignal::SIGSTOP
            | LinuxSignal::SIGTSTP
            | LinuxSignal::SIGTTIN
            | LinuxSignal::SIGTTOU
    )
}

/// Whether a pending signal that did NOT interrupt the syscall may also be
/// thrown away.
///
/// This is a **different question** from [`signal_interrupts_syscall`], and
/// the two were being answered with one predicate. Linux keeps them apart:
///
/// - `sig_kernel_ignore()` -- SIGCONT, SIGCHLD, SIGWINCH, SIGURG -- is the
///   set whose default action is to do nothing at all, and those are the ones
///   `prepare_signal()` drops on the floor;
/// - `sig_kernel_stop()` -- SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU -- does not
///   raise `EINTR` either, but it most certainly is not a no-op: it is the
///   whole of job control.
///
/// Answering both with "does it interrupt?" made the four stop signals
/// discardable, so a thread parked in `ppoll`/`epoll_wait`/`read` -- which is
/// every idle desktop process -- had its pending SIGSTOP removed by the very
/// loop that was waiting, and `kill -STOP` (or Ctrl-Z, or the SIGTTIN a
/// background job gets for reading the terminal) did nothing at all. The bit
/// now survives, so the stop happens when the syscall next returns.
///
/// A signal explicitly set to `SIG_IGN` is discardable whatever it is -- that
/// covers SIGTSTP and friends when a shell has told the kernel to ignore
/// them. SIGKILL cannot reach here: it interrupts.
fn signal_is_discarded_when_pending(proc_linux: &LinuxProcess, sig: LinuxSignal) -> bool {
    discards_when_pending(proc_linux.signal_action(sig).handler, sig)
}

/// [`signal_is_discarded_when_pending`] over a disposition rather than a
/// process, so the rule can be compared against
/// [`interrupts_syscall`] without one.
fn discards_when_pending(handler: usize, sig: LinuxSignal) -> bool {
    use crate::signal::{SIG_DFL, SIG_IGN};
    if handler == SIG_IGN {
        return true;
    }
    handler == SIG_DFL && signal_default_action_ignores(sig)
}

/// `sig_kernel_ignore()`: the signals whose *default* action is to do nothing.
///
/// Deliberately not the complement of [`signal_default_action_interrupts`]:
/// that list also holds the four stop signals, which do not interrupt and are
/// not ignored either.
fn signal_default_action_ignores(sig: LinuxSignal) -> bool {
    matches!(
        sig,
        LinuxSignal::SIGCHLD | LinuxSignal::SIGURG | LinuxSignal::SIGWINCH | LinuxSignal::SIGCONT
    )
}

/// Whether a pending signal actually interrupts a blocking syscall.
///
/// Linux only interrupts a syscall for a signal that will run a handler or
/// terminate the process. A signal whose disposition is *ignore* — `SIG_IGN`,
/// or `SIG_DFL` for a signal whose default action is ignore/stop (SIGCHLD,
/// SIGURG, SIGWINCH, SIGCONT, and the job-control stops) — is discarded (or
/// stops the process without EINTR) and must NOT wake a blocking syscall with
/// `EINTR`.
///
/// Returning `EINTR` for these was the bug behind a compositor's libinput
/// dispatch failing with "Interrupted system call": every time an autostart
/// child exited (or crashed) and raised SIGCHLD, the parent's blocking
/// `ppoll`/`epoll_pwait` returned `EINTR`, which wlroots treats as a fatal
/// dispatch error. The disposition list here mirrors the default-ignore set in
/// `handle_signal` (loader/src/linux.rs) so the two agree on what is a no-op.
fn signal_interrupts_syscall(proc_linux: &LinuxProcess, sig: LinuxSignal) -> bool {
    interrupts_syscall(proc_linux.signal_action(sig).handler, sig)
}

/// [`signal_interrupts_syscall`] over a disposition rather than a process.
fn interrupts_syscall(handler: usize, sig: LinuxSignal) -> bool {
    use crate::signal::{SIG_DFL, SIG_IGN};
    if handler == SIG_IGN {
        return false;
    }
    if handler == SIG_DFL {
        return signal_default_action_interrupts(sig);
    }
    // A caught signal (custom handler) interrupts the syscall.
    true
}

/// Check for pending signals and return EINTR if any *deliverable* signal
/// would interrupt the syscall. Signals whose disposition is ignore (SIG_IGN /
/// default-ignore, e.g. SIGCHLD) are skipped — matching Linux, where an ignored
/// signal is discarded and never returns EINTR from a blocking syscall.
pub fn check_signals() -> LxResult<()> {
    if let Some(arc) = kernel_hal::thread::get_current_thread() {
        if let Ok(thread) = arc.downcast::<Thread>() {
            use crate::thread::ThreadExt;
            use zircon_object::task::ThreadState;
            if thread.state() == ThreadState::Dying {
                return Err(LxError::EINTR);
            }
            if matches!(thread.proc().status(), Status::Exited(_)) {
                return Err(LxError::EINTR);
            }
            // Snapshot the deliverable (unblocked) pending signals, then drop the
            // per-thread lock before consulting the per-process disposition table
            // (a different lock) to avoid nesting the two.
            //
            // try_lock_linux (not lock_linux): the current thread may lack a
            // LinuxThread extension — most notably PID 1 / init, which running X
            // reparents exited grandchildren onto, so it takes their SIGCHLD and
            // ends up here. A thread with no Linux ext has no Linux signal state,
            // so nothing is pending and nothing can interrupt: return Ok rather
            // than unwrap-panicking ("init has no LinuxThread ext").
            let pending = match thread.try_lock_linux() {
                Some(linux_thread) => linux_thread.signals.mask_with(&linux_thread.signal_mask()),
                None => return Ok(()),
            };
            if pending.is_not_empty() {
                let proc = thread.proc();
                // try_linux (not linux), mirroring the try_lock_linux above.
                // DEFENCE IN DEPTH ONLY, NOT a fix: every Linux process provably
                // HAS the ext -- process.rs:109 and :212 are the only creators
                // and both pass a LinuxProcess by value, `ext` is written once
                // in the constructor and never replaced, and Process has no
                // Drop. So a None here still means the ext fat pointer was
                // CORRUPTED and must be investigated (see the dump in
                // `Process::linux()`). But a process with no resolvable Linux
                // extension has no signal disposition table, so nothing is
                // deliverable and nothing can interrupt: answer "no pending
                // signal" rather than panicking the whole kernel from an
                // arbitrary blocking syscall at session teardown.
                let proc_linux = match proc.try_linux() {
                    Some(lp) => lp,
                    None => return Ok(()),
                };
                let mut rest = pending;
                let mut discard = Sigset::empty();
                while let Some(sig) = rest.find_first_signal() {
                    rest.remove(sig);
                    if signal_interrupts_syscall(proc_linux, sig) {
                        return Err(LxError::EINTR);
                    }
                    // Linux discards an unblocked signal whose disposition is
                    // ignore (or whose default action this kernel maps to
                    // ignore) instead of leaving it pending forever. A thread
                    // parked in poll/epoll_wait may stay inside blocking
                    // syscalls for minutes; if we merely "skip" such signals
                    // here, every unrelated wake re-scans the same stale
                    // pending bit.
                    //
                    // But only those: see
                    // [`signal_is_discarded_when_pending`] for the four that
                    // were being thrown away with them.
                    if signal_is_discarded_when_pending(proc_linux, sig) {
                        discard.insert(sig);
                    }
                }
                if discard.is_not_empty() {
                    if let Some(mut linux_thread) = thread.try_lock_linux() {
                        linux_thread.signals.remove_set(&discard);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Send a signal to a process by its KoID.
/// Signals worth a kernel log line when delivered: the ones that end or
/// stop a process. The periodic ones (SIGCHLD, SIGALRM, SIGWINCH, SIGIO, ...)
/// would only drown the ring.
fn signal_trace_worthy(signal: LinuxSignal) -> bool {
    matches!(
        signal,
        LinuxSignal::SIGHUP
            | LinuxSignal::SIGINT
            | LinuxSignal::SIGQUIT
            | LinuxSignal::SIGILL
            | LinuxSignal::SIGABRT
            | LinuxSignal::SIGBUS
            | LinuxSignal::SIGFPE
            | LinuxSignal::SIGKILL
            | LinuxSignal::SIGUSR1
            | LinuxSignal::SIGUSR2
            | LinuxSignal::SIGSEGV
            | LinuxSignal::SIGPIPE
            | LinuxSignal::SIGTERM
            | LinuxSignal::SIGSTOP
            | LinuxSignal::SIGTSTP
    )
}

/// The calling process's pid and name for signal traces; `(0, "kernel")`
/// when there is no user thread on this CPU (a pty master dropped from a
/// kernel context, a timer).
pub fn current_process_pid_name() -> (u64, String) {
    if let Some(arc) = kernel_hal::thread::get_current_thread() {
        if let Ok(thread) = arc.downcast::<Thread>() {
            let proc = thread.proc();
            return (proc.id(), trace_name(proc));
        }
    }
    (0, String::from("kernel"))
}

/// A process name for the `[signal]`/`[wait]` traces that never blocks.
///
/// These traces run from arbitrary contexts, including INSIDE the object
/// layer's PROCESS_TERMINATED callback: `Process::exit` fires it with the
/// process's own `KObjectBase` lock held, the callback drops the file table,
/// dropping a pty master sends SIGHUP to the foreground group, and the trace
/// then asked the exiting process for its `name()` -- the same lock, on the
/// same CPU. That was a hard deadlock at every exit of a terminal that still
/// owned its pty (`[DEADLOCK] cpu=N at object/mod.rs name() / HOLDER
/// signal_change()`). `try_name` yields `?` instead of waiting.
fn trace_name(proc: &Process) -> String {
    proc.try_name().unwrap_or_else(|| String::from("?"))
}

/// Trace for `kill(pid, SIGKILL)`, which ends the target directly instead of
/// going through [`send_signal_to_process`] and would otherwise leave no
/// record of who sent it.
pub fn trace_direct_kill(target: &Arc<Process>, sender: &Arc<Process>) {
    zcore_drivers::klog_warn!(
        "[signal] SIGKILL -> pid {} ({}) from pid {} ({}) [kill()]",
        target.id(),
        trace_name(target),
        sender.id(),
        trace_name(sender)
    );
}

/// Trace an error returned by a blocking wait (`poll`, `ppoll`, `epoll_wait`).
/// libwayland treats ANY `epoll_wait` failure, EINTR included, as fatal and
/// a compositor then leaves `wl_display_run` without a word; foot logs
/// "failed to poll" and exits the same way. Knowing the errno and the victim
/// is the only way to tell such an exit from a deliberate one.
pub fn trace_wait_error(call: &str, err: crate::error::LxError) {
    let (pid, name) = current_process_pid_name();
    zcore_drivers::klog_warn!("[wait] {}() -> {:?} for pid {} ({})", call, err, pid, name);
}

#[cfg(test)]
mod tests {
    use super::signal_default_action_interrupts;
    use crate::signal::Signal as LinuxSignal;

    #[test]
    fn default_ignored_and_stop_signals_do_not_interrupt_waits() {
        for sig in [
            LinuxSignal::SIGCHLD,
            LinuxSignal::SIGURG,
            LinuxSignal::SIGWINCH,
            LinuxSignal::SIGCONT,
            LinuxSignal::SIGSTOP,
            LinuxSignal::SIGTSTP,
            LinuxSignal::SIGTTIN,
            LinuxSignal::SIGTTOU,
        ] {
            assert!(
                !signal_default_action_interrupts(sig),
                "{:?} should not interrupt",
                sig
            );
        }
    }

    #[test]
    fn default_terminating_signals_interrupt_waits() {
        for sig in [
            LinuxSignal::SIGHUP,
            LinuxSignal::SIGINT,
            LinuxSignal::SIGTERM,
        ] {
            assert!(
                signal_default_action_interrupts(sig),
                "{:?} should interrupt",
                sig
            );
        }
    }
}

/// The four signals whose default action is a job-control STOP.
pub const STOP_SIGNALS: [LinuxSignal; 4] = [
    LinuxSignal::SIGSTOP,
    LinuxSignal::SIGTSTP,
    LinuxSignal::SIGTTIN,
    LinuxSignal::SIGTTOU,
];

/// What sending a signal does to the target's job-control state AT THE MOMENT
/// OF SENDING -- before any mask, handler or `wait` is consulted.
///
/// Linux decides this in `prepare_signal()`, in the sender's own context, and
/// it has to: the work belongs to signals whose whole point is to act on a
/// process that is NOT running, so the target cannot be the one to do it. This
/// kernel had nothing here, and left both halves to the target thread's own
/// `handle_signal` loop -- which a stopped process's threads never reach,
/// because they are parked in [`wait_while_job_stopped`] waiting for a
/// continue that only that same loop could have issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendEffect {
    /// `SIGCONT`: resume the process now, whatever it has this signal set to,
    /// and drop any stop that had not been acted on yet. Without it,
    /// `kill -CONT` and a shell's `fg` moved nothing.
    Resume,
    /// A stop signal: drop any `SIGCONT` that had not been acted on yet, so
    /// the last one sent is the one that decides.
    Stop,
    /// `SIGKILL`: the process is not continuing, but a parked thread has to
    /// wake up to die. Without it, `kill -9` on a Ctrl-Z'd process did
    /// nothing at all and the pid stayed forever.
    WakeToDie,
    /// Everything else waits to be received, which is what a signal normally
    /// does.
    None,
}

/// `prepare_signal()`: what [`send_signal_to_process`] must do before the
/// signal is so much as queued.
pub fn send_effect(signal: LinuxSignal) -> SendEffect {
    match signal {
        LinuxSignal::SIGCONT => SendEffect::Resume,
        LinuxSignal::SIGKILL => SendEffect::WakeToDie,
        s if STOP_SIGNALS.contains(&s) => SendEffect::Stop,
        _ => SendEffect::None,
    }
}

/// The pending signals a target is left with once `signal` is sent to it:
/// `SIGCONT` and the stop signals cancel each other out, so that a process
/// cannot end up carrying both and stopping or resuming depending on which
/// its threads happen to dequeue first.
pub fn pending_after_send(mut pending: Sigset, signal: LinuxSignal) -> Sigset {
    match send_effect(signal) {
        SendEffect::Resume => {
            for stop in STOP_SIGNALS {
                pending.remove(stop);
            }
        }
        SendEffect::Stop => pending.remove(LinuxSignal::SIGCONT),
        _ => {}
    }
    pending
}

pub fn send_signal_to_process(pid: usize, signal: LinuxSignal) -> LxResult<()> {
    send_signal_to_process_with_info(pid, signal, None)
}

/// [`send_signal_to_process`], with the `siginfo_t` the handler will be
/// handed: who sent a `kill`, which child a `SIGCHLD` is about. `None` leaves
/// [`SigInfo::bare`].
pub fn send_signal_to_process_with_info(
    pid: usize,
    signal: LinuxSignal,
    info: Option<SigInfo>,
) -> LxResult<()> {
    use crate::thread::ThreadExt;
    if let Some(process) = ROOT_JOB.find_process(pid as KoID) {
        if signal_trace_worthy(signal) {
            // Who signals whom: a process that exits on a handled SIGTERM/
            // SIGINT/SIGHUP leaves no other trace (the `[exit]` line only
            // covers default-disposition deaths), and "labwc/foot vanished
            // 25 ms after a job started" was otherwise unexplainable.
            let (spid, sname) = current_process_pid_name();
            zcore_drivers::klog_warn!(
                "[signal] {:?} -> pid {} ({}) from pid {} ({})",
                signal,
                process.id(),
                trace_name(&process),
                spid,
                sname
            );
        }
        let tids = process.thread_ids();
        // `prepare_signal()`, before the signal is queued: a stop and a
        // continue cancel each other where they are waiting to be received,
        // so a process never carries both and the last one sent is the one
        // that decides.
        for tid in process.thread_ids() {
            if let Ok(thread_obj) = process.get_child(tid) {
                if let Ok(thread) = thread_obj.downcast_arc::<Thread>() {
                    if let Some(mut lt) = thread.try_lock_linux() {
                        lt.signals = pending_after_send(lt.signals, signal);
                    }
                }
            }
        }
        // Prefer a thread that has the signal *unblocked* — it can act on it
        // right away — and deliver there.
        let mut first: Option<Arc<Thread>> = None;
        for tid in tids {
            if let Ok(thread_obj) = process.get_child(tid) {
                if let Ok(thread) = thread_obj.downcast_arc::<Thread>() {
                    // Peek without holding the guard across a move of `thread`.
                    let delivered = if let Some(mut lt) = thread.try_lock_linux() {
                        if lt.signal_mask().contains(signal) {
                            false
                        } else {
                            lt.queue_signal(signal, info);
                            true
                        }
                    } else {
                        // Lock held (e.g. PID 1 in waitpid): queue below rather
                        // than dropping the signal — that drop is why lunarbar's
                        // `kill 1` did nothing.
                        false
                    };
                    if delivered {
                        // Wake waitpid(-1): PID 1 blocks on this zircon bit.
                        process.signal_set(Signal::SIGCHLD);
                        wake_for_job_control(&process, signal);
                        return Ok(());
                    }
                    if first.is_none() {
                        first = Some(thread);
                    }
                }
            }
        }
        // Every thread has the signal blocked: it must still become *pending*
        // (POSIX — masking only delays delivery, it does not discard the
        // signal), so it is delivered once unblocked or consumed via signalfd.
        // The old code dropped it here, which is why a Wayland compositor that
        // blocks SIGINT for its signalfd never saw Ctrl-C.
        if let Some(thread) = first {
            thread.lock_linux().queue_signal(signal, info);
        }
        // Pulse even when every thread had the Linux signal blocked: waitpid
        // still needs to return so the waiter can notice the pending set.
        process.signal_set(Signal::SIGCHLD);
        wake_for_job_control(&process, signal);
        Ok(())
    } else {
        Err(LxError::ESRCH)
    }
}

/// `complete_signal()`: the wake-up half, once the signal is queued.
///
/// It goes here, in the SENDER, because the work is only ever needed when the
/// target is job-control stopped -- and a stopped process's threads are
/// parked in [`wait_while_job_stopped`], so they cannot do it for themselves.
/// After the queueing, as Linux does it, so the thread that wakes already has
/// the signal to act on.
fn wake_for_job_control(process: &Arc<Process>, signal: LinuxSignal) {
    let lp = match process.try_linux() {
        Some(lp) => lp,
        None => return,
    };
    match send_effect(signal) {
        SendEffect::Resume => {
            lp.job_continue(process);
        }
        SendEffect::WakeToDie => lp.job_wake_to_die(process),
        SendEffect::Stop | SendEffect::None => {}
    }
}

#[cfg(test)]
mod fork_inheritance_tests {
    //! What a `fork(2)` child starts life with. Every one of these is a
    //! one-line decision that is invisible at the call site and wrong in only
    //! one direction: a field that quietly falls back to its default gives
    //! the child a fresh value where Linux gives it the parent's, and nothing
    //! fails -- the child just behaves as if the parent had never configured
    //! anything. Four were doing exactly that.

    use super::*;

    /// A parent with every field set to something that is *not* its default,
    /// so a field the fork forgets shows up as the default and a field it
    /// copies shows up as this.
    fn a_configured_parent() -> LinuxProcessInner {
        let mut p = LinuxProcessInner {
            execute_path: String::from("/usr/bin/labwc"),
            cmdline: alloc::vec![String::from("labwc"), String::from("-s")],
            environ: alloc::vec![String::from("WAYLAND_DISPLAY=wayland-0")],
            current_working_directory: String::from("/home/moebius"),
            limits: {
                let mut limits = RLimits::default();
                limits
                    .set(
                        RLIMIT_NOFILE,
                        RLimit {
                            cur: 65536,
                            max: 65536,
                        },
                    )
                    .unwrap();
                limits
            },
            brk: 0x5555_0010_0000,
            mapped_brk: 0x5555_0020_0000,
            pgid: 41,
            sid: 42,
            no_new_privs: true,
            dumpable: Some(0),
            personality: 0x0004_0000,
            thp_disable: true,
            children_utime_ns: 111,
            children_stime_ns: 222,
            pdeathsig: 15,
            child_subreaper: true,
            job_stopped: true,
            job_stop_sig: 19,
            job_stop_pending: true,
            job_continued_pending: true,
            has_execed: true,
            sugid: true,
            ..Default::default()
        };
        p.cloexec_fds.insert(7.into());
        p.files.insert(
            7.into(),
            crate::fs::Inotify::new(crate::fs::OpenFlags::empty()) as Arc<dyn FileLike>,
        );
        p
    }

    fn fork_of(parent: &LinuxProcessInner) -> LinuxProcessInner {
        parent.forked_child(41, 42)
    }

    #[test]
    fn the_credential_taint_survives_the_fork() {
        // `p2->p_flag |= p1->p_flag & P_SUGID` (kern_fork.c). The child is a
        // copy of an address space that a more privileged program filled in,
        // so it inherits the doubt along with the memory -- and a child that
        // forgot it would answer `issetugid()` with 0 while holding exactly
        // the data segment the flag exists to warn about.
        let child = fork_of(&a_configured_parent());
        assert!(child.sugid);
    }

    #[test]
    fn the_file_descriptor_limit_survives_the_fork() {
        // `ulimit -n 65536` only ever reaches a program through a fork: the
        // shell raises its own limit and then forks. Resetting it here undid
        // every raise in the system, silently, and the program hit EMFILE at
        // the default -- the failure a raised limit exists to prevent.
        let child = fork_of(&a_configured_parent());
        assert_eq!(child.limits.get(RLIMIT_NOFILE).unwrap().cur, 65536);
        assert_eq!(child.limits.get(RLIMIT_NOFILE).unwrap().max, 65536);
    }

    #[test]
    fn the_heap_bookkeeping_survives_the_fork() {
        // `fork` copies the address space, so the heap is in the child. The
        // numbers that say where it ends were starting from zero, and
        // `sys_brk` returns the old break unchanged for anything below the
        // heap base -- so in a forked child `brk` could not move at all and
        // `sbrk(0)` answered 0. A child that never execs (a subshell, a
        // zygote) had its allocator pushed onto mmap for good.
        let child = fork_of(&a_configured_parent());
        assert_eq!(child.brk, 0x5555_0010_0000);
        assert_eq!(child.mapped_brk, 0x5555_0020_0000);
    }

    #[test]
    fn the_environment_survives_the_fork() {
        // `/proc/<pid>/environ` reads the process's own memory in Linux, and
        // a fork copies that memory. A child that has not exec'd reported an
        // empty environment.
        let child = fork_of(&a_configured_parent());
        assert_eq!(
            child.environ,
            alloc::vec![String::from("WAYLAND_DISPLAY=wayland-0")]
        );
    }

    #[test]
    fn the_working_directory_and_command_line_survive_the_fork() {
        let child = fork_of(&a_configured_parent());
        assert_eq!(child.current_working_directory, "/home/moebius");
        assert_eq!(child.execute_path, "/usr/bin/labwc");
        assert_eq!(child.cmdline.len(), 2);
    }

    #[test]
    fn the_close_on_exec_set_is_copied_and_not_shared() {
        // POSIX: the child gets its own copy of each fd's FD_CLOEXEC flag, so
        // a later `fcntl(F_SETFD)` in either process must not reach the other.
        let parent = a_configured_parent();
        let mut child = fork_of(&parent);
        assert!(child.cloexec_fds.contains(&7.into()));
        child.cloexec_fds.remove(&7.into());
        assert!(
            parent.cloexec_fds.contains(&7.into()),
            "the child's copy must be its own"
        );
    }

    #[test]
    fn the_prctl_settings_that_linux_inherits_do() {
        // A `no_new_privs` that did not survive fork would hand a child back
        // the setuid behaviour its parent gave up -- the one thing the flag
        // exists to make irreversible.
        let child = fork_of(&a_configured_parent());
        assert!(child.no_new_privs);
        assert_eq!(child.dumpable, Some(0));
        assert_eq!(child.personality, 0x0004_0000);
        assert!(child.thp_disable);
    }

    #[test]
    fn the_process_group_and_session_are_the_resolved_ones_passed_in() {
        // Not the parent's raw fields: an unset (0) pgid means "the parent's
        // own pid", and the child needs the concrete value or a Ctrl-C never
        // reaches it.
        let mut parent = a_configured_parent();
        parent.pgid = 0;
        parent.sid = 0;
        let child = parent.forked_child(1234, 5678);
        assert_eq!(child.pgid, 1234);
        assert_eq!(child.sid, 5678);
    }

    #[test]
    fn the_open_files_survive_the_fork() {
        // The one thing everybody knows a fork does. It is here so the
        // exhaustive list above cannot lose it while nobody is looking.
        let child = fork_of(&a_configured_parent());
        assert!(child.files.contains_key(&7.into()));
    }

    #[test]
    fn a_child_starts_with_no_children_of_its_own() {
        let mut parent = a_configured_parent();
        parent.reaped_children.insert(99, (0, Default::default()));
        let child = fork_of(&parent);
        assert!(child.children.is_empty());
        assert!(
            child.reaped_children.is_empty(),
            "a newborn child has reaped nobody"
        );
        // `copy_process` zeroes `cutime`/`cstime`: a child must not be born
        // already credited with the CPU time of its parent's other children,
        // or `times(2)` double-counts it up the whole tree.
        assert_eq!(child.children_utime_ns, 0);
        assert_eq!(child.children_stime_ns, 0);
    }

    /// `copy_process`: `p->flags |= PF_FORKNOEXEC`. The parent has exec'd --
    /// every process has -- but the child has not, and that flag is what
    /// gives the shell its window to put the child in a job's process group
    /// (see [`setpgid_verdict`]). Inherited, the window would never open and
    /// every `setpgid(child, ...)` would answer EACCES.
    #[test]
    fn a_child_has_not_execed_however_long_its_parent_has_been_running() {
        let parent = a_configured_parent();
        assert!(parent.has_execed, "the fixture must have exec'd");
        assert!(!fork_of(&parent).has_execed);
    }

    #[test]
    fn a_child_is_not_born_stopped_or_owing_a_notification() {
        // `job_stopped` carried over would leave the child parked before its
        // first instruction, waiting for a SIGCONT nobody will send it; the
        // pending flags carried over would make its first `waitpid` report a
        // stop that happened to its parent.
        let child = fork_of(&a_configured_parent());
        assert!(!child.job_stopped);
        assert_eq!(child.job_stop_sig, 0);
        assert!(!child.job_stop_pending);
        assert!(!child.job_continued_pending);
    }

    #[test]
    fn the_parents_own_roles_are_not_handed_down() {
        // `p->pdeath_signal = 0` in `copy_process`: the signal is "tell me
        // when MY parent dies", so inheriting it would have the child killed
        // when its grandparent exits. The subreaper attribute is likewise the
        // parent's role, not something a child is born holding.
        let child = fork_of(&a_configured_parent());
        assert_eq!(child.pdeathsig, 0);
        assert!(!child.child_subreaper);
    }

    #[test]
    fn the_semaphore_undo_state_is_not_inherited() {
        // A plain `fork` does NOT share SEM_UNDO state -- only
        // `CLONE_SYSVSEM` does. Copying it would have the child undo, on its
        // own exit, semaphore operations that its parent performed and that
        // the parent will undo again.
        let mut parent = a_configured_parent();
        let id = parent
            .semaphores
            .add(crate::ipc::SemArray::get_or_create(0, 1, 0o666, 0, 0).unwrap());
        parent.semaphores.add_undo(id, 0, -1);

        let child = fork_of(&parent);
        assert!(
            child.semaphores.owes_no_undo(),
            "a forked child owes no semaphore undo"
        );
        // ...but it keeps the sets the parent had open. The ids are
        // per-process indices, so dropping the table left an id the parent
        // passed down naming nothing in the child.
        assert!(
            child.semaphores.get(id).is_some(),
            "the child must still find the set its parent had open"
        );
    }

    #[test]
    fn the_shared_memory_attachments_survive_the_fork() {
        // `fork` copies the address space, so the segments the parent had
        // attached are mapped in the child too -- it is holding them whether
        // the kernel remembers or not. Without the record the child cannot
        // `shmdt` them, so the mapping stays for its whole life, and the
        // segment's use count is wrong. This is the same bookkeeping whose
        // loss on `IPC_RMID` leaked an address range per X11 frame.
        use crate::ipc::ShmGuard;
        use zircon_object::vm::VmObject;
        let mut parent = a_configured_parent();
        let guard = Arc::new(kernel_hal::sync::Mutex::new(ShmGuard {
            shared_guard: VmObject::new_paged(1),
            shmid_ds: kernel_hal::sync::Mutex::new(Default::default()),
        }));
        parent.shm_identifiers.add(9, guard);
        let mut ident = parent.shm_identifiers.get(9).unwrap();
        ident.addr = 0x7f00_0000;
        parent.shm_identifiers.set(9, ident);

        let child = fork_of(&parent);
        assert_eq!(
            child.shm_identifiers.get_id(0x7f00_0000),
            Some(9),
            "the child must be able to find the segment it inherited"
        );
    }

    #[test]
    fn the_kernel_side_futex_objects_are_not_inherited() {
        // They are keyed by address in the parent's address space and hold
        // its waiters. The child's memory is a copy: same addresses,
        // different pages, and nobody waiting. Handing the child the
        // parent's objects would have a `futex_wake` in the child reach
        // threads of the parent that are waiting on their own memory.
        static WORD: AtomicI32 = AtomicI32::new(0);
        let mut parent = a_configured_parent();
        parent.futexes.get_or_create(0x1000, || Futex::new(&WORD));

        let child = fork_of(&parent);
        assert!(child.futexes.is_empty());
    }
}

#[cfg(test)]
mod exec_reset_tests {
    //! What the PROCESS must forget when it calls `execve`: its attachments
    //! to an address space that no longer exists, and the parent's choice of
    //! death signal once the exec has made the program privileged.

    use super::*;

    /// A process holding a shared segment at a known address, a parent-death
    /// signal and an ordinary user's ids, so the tests have something real to
    /// lose. Nothing here is left at its default: a fixture that is already
    /// empty where the test asserts emptiness asserts nothing.
    fn a_process_about_to_exec() -> LinuxProcessInner {
        use crate::ipc::ShmGuard;
        use zircon_object::vm::VmObject;
        let mut inner = LinuxProcessInner::default();
        let guard = Arc::new(kernel_hal::sync::Mutex::new(ShmGuard {
            shared_guard: VmObject::new_paged(1),
            shmid_ds: kernel_hal::sync::Mutex::new(Default::default()),
        }));
        inner.shm_identifiers.add(9, guard);
        let mut ident = inner.shm_identifiers.get(9).unwrap();
        ident.addr = 0x7f00_0000;
        inner.shm_identifiers.set(9, ident);
        inner.pdeathsig = crate::signal::Signal::SIGTERM as u8;
        inner.credentials.ruid = 1000;
        inner.credentials.set_euid(1000);
        inner.credentials.suid = 1000;
        inner.credentials.rgid = 1000;
        inner.credentials.set_egid(1000);
        inner.credentials.sgid = 1000;
        // Its own group, not root's: `secureexec` asks whether the effective
        // group is one this process already held, and a fixture left in group
        // 0 answers that question for a user who is not in group 0.
        inner.credentials.groups = vec![1000];
        inner
    }

    #[test]
    fn an_exec_forgets_the_segments_the_old_address_space_had_attached() {
        // Those mappings died with `vmar.clear()`. The record of where they
        // were is what `shmdt` trusts: it looks an address up in this very
        // map and unmaps that many bytes there, so kept across an exec it
        // punches a hole in the program that is running now.
        let mut inner = a_process_about_to_exec();
        assert_eq!(
            inner.shm_identifiers.get_id(0x7f00_0000),
            Some(9),
            "the fixture must be holding a segment to lose"
        );
        inner.reset_for_exec(false);
        assert_eq!(inner.shm_identifiers.get_id(0x7f00_0000), None);
        assert!(inner.shm_identifiers.get(9).is_none());
    }

    /// `begin_new_exec`: `me->flags &= ~PF_FORKNOEXEC`. The exec is what
    /// closes the parent's `setpgid` window, so it is the exec that has to
    /// record it -- and it does so whether or not the new image is
    /// privileged, unlike the death signal below.
    #[test]
    fn an_exec_is_what_ends_the_window_its_parent_had_to_group_it() {
        let mut inner = a_process_about_to_exec();
        assert!(!inner.has_execed, "a forked child has not exec'd yet");
        inner.reset_for_exec(false);
        assert!(inner.has_execed);

        let mut privileged = a_process_about_to_exec();
        privileged.reset_for_exec(true);
        assert!(privileged.has_execed);
    }

    #[test]
    fn an_ordinary_exec_keeps_the_parent_death_signal() {
        // prctl(2) puts PR_SET_PDEATHSIG among the settings an `execve`
        // preserves; Linux clears it only for a privilege-raising one. A
        // supervisor that sets it and then execs its real payload is relying
        // on exactly this.
        let mut inner = a_process_about_to_exec();
        inner.reset_for_exec(false);
        assert_eq!(inner.pdeathsig, crate::signal::Signal::SIGTERM as u8);
    }

    #[test]
    fn an_exec_that_raises_privileges_drops_the_parent_death_signal() {
        // `begin_new_exec()`: `if (bprm->secureexec) me->pdeath_signal = 0;`
        // The parent chose both the signal and, by exiting, the moment --
        // while the child was still running code the parent controlled. It
        // must not keep that lever over a program that is now privileged.
        let mut inner = a_process_about_to_exec();
        inner.reset_for_exec(true);
        assert_eq!(inner.pdeathsig, 0);
    }

    #[test]
    fn a_setuid_image_owned_by_another_user_raises_privileges() {
        let mut inner = a_process_about_to_exec();
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o4755, ROOT_UID, 1000));
        assert_eq!(inner.credentials.euid, ROOT_UID);
        // The saved id follows, which is what lets the program drop and
        // regain the privilege later.
        assert_eq!(inner.credentials.suid, ROOT_UID);
        // And the real id does not move: that is the whole point of setuid.
        assert_eq!(inner.credentials.ruid, 1000);
    }

    #[test]
    fn a_setgid_image_alone_raises_privileges_too() {
        // Its own test because the group half is a second, separate decision
        // -- and a `raised` that only ever looked at the user half would let
        // a set-group-ID program keep the lever.
        let mut inner = a_process_about_to_exec();
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o2755, 1000, 0));
        assert_eq!(inner.credentials.egid, 0);
        assert_eq!(inner.credentials.sgid, 0);
        assert_eq!(inner.credentials.rgid, 1000);
    }

    #[test]
    fn a_setuid_bit_naming_the_id_we_already_run_as_raises_nothing() {
        // Linux's `secureexec` asks whether the ids CHANGED, not whether the
        // bits were set. A user's own set-user-ID binary grants that user
        // nothing, so nothing about the process needs hardening.
        let mut inner = a_process_about_to_exec();
        assert!(!inner.apply_exec_ids_from_a_normal_mount(0o6755, 1000, 1000));
        assert_eq!(inner.credentials.euid, 1000);
        assert_eq!(inner.credentials.egid, 1000);
    }

    #[test]
    fn an_ordinary_image_raises_nothing() {
        let mut inner = a_process_about_to_exec();
        assert!(!inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
        assert_eq!(inner.credentials.euid, 1000);
        assert_eq!(inner.credentials.egid, 1000);
    }

    #[test]
    fn no_new_privs_means_the_setuid_bits_are_not_honoured_at_all() {
        // Documentation/userspace-api/no_new_privs.rst. And since nothing was
        // granted, nothing was raised: reporting a raise here would have the
        // process hardened against a privilege it never got.
        let mut inner = a_process_about_to_exec();
        inner.no_new_privs = true;
        assert!(!inner.apply_exec_ids_from_a_normal_mount(0o6755, ROOT_UID, ROOT_UID));
        assert_eq!(inner.credentials.euid, 1000);
        assert_eq!(inner.credentials.egid, 1000);
        assert_eq!(inner.credentials.suid, 1000);
        assert_eq!(inner.credentials.sgid, 1000);
    }

    #[test]
    fn a_setuid_image_on_a_nosuid_mount_grants_nothing() {
        // `mnt_may_suid(bprm->file->f_path.mnt)`: this is the whole of what
        // mounting `/tmp` with `nosuid` buys, and it was bought and never
        // delivered -- the option reached `/proc/mounts` and stopped there.
        let mut inner = a_process_about_to_exec();
        assert!(!inner.apply_exec_ids(0o6755, ROOT_UID, ROOT_UID, false));
        assert_eq!(inner.credentials.euid, 1000);
        assert_eq!(inner.credentials.egid, 1000);
        assert_eq!(inner.credentials.suid, 1000);
        assert_eq!(inner.credentials.sgid, 1000);
    }

    #[test]
    fn nosuid_does_not_hide_a_process_that_was_already_privileged() {
        // The mount decides what this exec may GRANT. It says nothing about
        // what the process is already carrying, and a program running as root
        // has to distrust its environment wherever its image happens to live.
        let mut inner = a_process_already_setuid_root();
        assert!(inner.apply_exec_ids(0o0755, ROOT_UID, ROOT_UID, false));
        assert_eq!(inner.credentials.euid, ROOT_UID);
    }

    #[test]
    fn the_mount_and_no_new_privs_are_two_separate_gates() {
        // Either one refusing is enough, and neither is the other: a table,
        // so that a rewrite collapsing them into one condition fails here by
        // name rather than in whichever of the two cases it got wrong.
        for (may_suid, no_new_privs, honoured) in [
            (true, false, true),
            (false, false, false),
            (true, true, false),
            (false, true, false),
        ] {
            let mut inner = a_process_about_to_exec();
            inner.no_new_privs = no_new_privs;
            inner.apply_exec_ids(0o4755, ROOT_UID, ROOT_UID, may_suid);
            let got = inner.credentials.euid == ROOT_UID;
            assert_eq!(
                got, honoured,
                "may_suid={} no_new_privs={}: euid ended {}",
                may_suid, no_new_privs, inner.credentials.euid
            );
        }
    }

    #[test]
    fn the_bits_are_the_ones_linux_uses() {
        // Pinned against the octal literals, not against each other: a test
        // that checks a constant with the same constant moves with it.
        assert_eq!(MODE_SET_UID, 0o4000);
        assert_eq!(MODE_SET_GID, 0o2000);
        assert_eq!(MODE_EXEC_GRP, 0o0010);
    }

    // --- `bprm->secureexec` -------------------------------------------------
    //
    // `cap_bprm_creds_from_file()` asks three questions and takes any one of
    // them as a yes. The first is about the image being loaded; the other two
    // are about the process, and they are the ones a rule written only around
    // the set-user-ID bits cannot see.

    /// A process already running set-user-ID root: its real id is an ordinary
    /// user's and its effective id is not. Whoever started it chose its
    /// environment; whoever owns the id it runs as did not.
    fn a_process_already_setuid_root() -> LinuxProcessInner {
        let mut inner = a_process_about_to_exec();
        inner.credentials.set_euid(ROOT_UID);
        inner.credentials.suid = ROOT_UID;
        inner
    }

    #[test]
    fn an_already_privileged_process_is_secure_even_exec_ing_an_ordinary_image() {
        // `!uid_eq(new->euid, old->uid)`. There is no set-user-ID bit in
        // sight here: the effective id came from the PREVIOUS exec and
        // survives this one, so the ordinary shell being loaded runs with
        // exactly the same privilege -- and can trust the caller's
        // LD_PRELOAD exactly as little.
        let mut inner = a_process_already_setuid_root();
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
        assert_eq!(inner.credentials.euid, ROOT_UID);
        assert_eq!(inner.credentials.ruid, 1000);
    }

    #[test]
    fn the_group_half_is_asked_on_its_own() {
        // `!gid_eq(new->egid, old->gid)`. A set-group-ID program that reads
        // the mail spool is privileged over its caller just as surely as a
        // set-user-ID one, and a rule that only ever looked at the user half
        // would hand it the caller's environment.
        let mut inner = a_process_about_to_exec();
        inner.credentials.set_egid(0);
        inner.credentials.sgid = 0;
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
        assert_eq!(inner.credentials.egid, 0);
        assert_eq!(inner.credentials.rgid, 1000);
    }

    #[test]
    fn no_new_privs_does_not_make_an_already_privileged_process_look_safe() {
        // Linux returns early from `bprm_fill_uid()` and computes
        // `secureexec` afterwards regardless. Refusing to RAISE a process
        // says nothing about how privileged it already was -- and answering
        // "not secure" here would hand the caller's LD_PRELOAD to a process
        // running as root, in the one mode whose whole purpose is that a
        // sandbox can exec without granting anything.
        let mut inner = a_process_already_setuid_root();
        inner.no_new_privs = true;
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o6755, 1000, 1000));
        // The bits were still not honoured: the image asked for uid 1000 and
        // got nothing, which is the whole of what no_new_privs promises.
        assert_eq!(inner.credentials.euid, ROOT_UID);
        assert_eq!(inner.credentials.egid, 1000);
    }

    #[test]
    fn an_exec_that_moves_an_id_is_a_change_even_when_the_ids_end_up_even() {
        // `id_changed = !uid_eq(new->euid, old->euid)`, and it is a separate
        // question from the two that compare against the real ids -- this is
        // the case where only it can answer. A process running set-user-ID
        // root (ruid 1000, euid 0) execs an image whose set-user-ID bit names
        // 1000: the ids come out even, so both "effective != real" questions
        // say no, and the exec still performed an id transition that the new
        // image must be hardened against.
        let mut inner = a_process_about_to_exec();
        inner.credentials.set_euid(ROOT_UID);
        inner.credentials.suid = ROOT_UID;
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o4755, 1000, ROOT_UID));
        assert_eq!(inner.credentials.euid, 1000);
        assert_eq!(inner.credentials.ruid, 1000);
        assert_eq!(inner.credentials.egid, inner.credentials.rgid);
    }

    #[test]
    fn a_setgid_bit_on_a_file_the_group_cannot_execute_is_not_a_setgid_program() {
        // `(mode & (S_ISGID | S_IXGRP)) == (S_ISGID | S_IXGRP)`. Without the
        // group-execute bit, S_ISGID is the mandatory-locking convention --
        // an old, unrelated use of the same bit -- and honouring it would
        // move a process's effective group for a file that never claimed to
        // be a set-group-ID program at all.
        let mut inner = a_process_about_to_exec();
        assert!(!inner.apply_exec_ids_from_a_normal_mount(0o2744, ROOT_UID, ROOT_UID));
        assert_eq!(inner.credentials.egid, 1000);
        assert_eq!(inner.credentials.sgid, 1000);
    }

    #[test]
    fn the_same_bit_with_group_execute_is_one() {
        // The other side of the line above, so that the test pair pins the
        // rule and not just one of its answers.
        let mut inner = a_process_about_to_exec();
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o2754, ROOT_UID, ROOT_UID));
        assert_eq!(inner.credentials.egid, ROOT_UID);
        assert_eq!(inner.credentials.sgid, ROOT_UID);
    }

    #[test]
    fn falling_back_to_a_group_the_caller_already_held_grants_nothing() {
        // `in_group_p(new->egid)`: the group half of `id_changed` is a
        // membership test, not a comparison. A process running set-group-ID
        // root that execs a set-group-ID image naming a group it is already
        // in has gained nothing to be hardened against -- it has given
        // something up.
        let mut inner = a_process_about_to_exec();
        inner.credentials.set_egid(ROOT_UID);
        inner.credentials.sgid = ROOT_UID;
        inner.credentials.groups = vec![1000];
        assert!(!inner.apply_exec_ids_from_a_normal_mount(0o2754, ROOT_UID, 1000));
        assert_eq!(inner.credentials.egid, 1000);
    }

    #[test]
    fn a_group_the_caller_did_not_hold_is_a_raise_even_back_to_its_own() {
        // The same shape, minus the membership: Linux asks `in_group_p`,
        // which knows nothing about the real group id, so a process whose
        // supplementary list does not carry its own rgid is hardened when it
        // returns to it. Written down because it looks like a mistake and is
        // the rule as Linux states it.
        let mut inner = a_process_about_to_exec();
        inner.credentials.set_egid(ROOT_UID);
        inner.credentials.sgid = ROOT_UID;
        inner.credentials.groups = vec![ROOT_UID];
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o2754, ROOT_UID, 1000));
        assert_eq!(inner.credentials.egid, 1000);
    }

    #[test]
    fn an_ordinary_process_exec_ing_an_ordinary_image_is_not_secure() {
        // The case every process on this machine is in, and the one that
        // matters most to get right in THIS direction: answering "secure"
        // here puts the whole system in secure mode, which drops LD_PRELOAD
        // everywhere and makes GLib refuse to autolaunch a session bus.
        let mut inner = LinuxProcessInner::default();
        assert!(!inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    }

    // --- The aux-vector identity block --------------------------------------

    #[test]
    fn the_identity_block_carries_the_ids_the_new_image_will_run_with() {
        // Built after `apply_exec_ids`, because that is when the ids are
        // final -- and mapped here, once, so no caller has to remember that
        // `AT_UID` is the REAL id while `AT_EUID` is the effective one.
        let mut inner = a_process_already_setuid_root();
        inner.credentials.rgid = 1001;
        let id = inner.aux_identity(true);
        assert_eq!(id.uid, 1000, "AT_UID is the REAL user id");
        assert_eq!(id.euid, ROOT_UID, "AT_EUID is the EFFECTIVE user id");
        assert_eq!(id.gid, 1001, "AT_GID is the REAL group id");
        assert_eq!(id.egid, 1000, "AT_EGID is the EFFECTIVE group id");
        assert!(id.secure);
    }

    #[test]
    fn the_identity_block_reports_an_unprivileged_exec_as_such() {
        let id = LinuxProcessInner::default().aux_identity(false);
        assert!(!id.secure);
        assert_eq!((id.uid, id.euid, id.gid, id.egid), (0, 0, 0, 0));
    }
}

#[cfg(test)]
mod sugid_tests {
    //! `issetugid(2)`, the question a FreeBSD program asks before it decides
    //! that the environment and the data segment it woke up with are its own:
    //! "are my ids the ones I was started with?" It was answered with a
    //! constant 0, which is the answer that makes a set-user-ID program trust
    //! whatever its caller left in `MALLOC_OPTIONS` or `LD_*`.
    //!
    //! FreeBSD keeps it as `P_SUGID` on the process. It is deliberately
    //! sticky -- `kern_prot.c` explains that a program that started as root
    //! and *became* a user without an exec "cannot know everything that libc
    //! might have put in their data segment" -- so these tests are mostly
    //! about the two ways it must NOT be forgotten.

    use super::dup_fd_tests::a_process;
    use super::*;

    #[test]
    fn a_process_that_has_not_touched_its_ids_is_not_tainted() {
        // The case every process on this machine is in. Answering 1 here
        // would put the whole system in the hardened mode, which is the
        // mirror of the bug and just as wrong.
        assert!(!a_process().is_sugid());
    }

    #[test]
    fn dropping_privilege_taints_the_process_even_though_the_ids_end_up_even() {
        // Root calling `setuid(1000)` lands on ruid == euid == suid == 1000,
        // which looks exactly like a process that was started as that user.
        // It is not one: everything in its memory was put there by root. This
        // is the case the flag exists for, and the one a rule derived from
        // the ids alone cannot see.
        let proc = a_process();
        proc.set_uid(1000).unwrap();
        let creds = proc.credentials();
        assert_eq!((creds.ruid, creds.euid, creds.suid), (1000, 1000, 1000));
        assert!(proc.is_sugid());
    }

    #[test]
    fn a_call_that_moves_no_id_does_not_taint() {
        // `sys_setuid` latches inside `if (change)`. Root setting its own id
        // changes nothing about who arranged this process's memory.
        let proc = a_process();
        proc.set_uid(ROOT_UID).unwrap();
        assert!(!proc.is_sugid());
    }

    #[test]
    fn a_refused_switch_does_not_taint() {
        // An unprivileged process asking to become root is told EPERM; it
        // must not come away marked as though it had succeeded.
        let proc = a_process();
        {
            let mut inner = proc.inner.lock();
            inner.credentials.ruid = 1000;
            inner.credentials.set_euid(1000);
            inner.credentials.suid = 1000;
        }
        assert!(proc.set_uid(ROOT_UID).is_err());
        assert!(!proc.is_sugid());
    }

    #[test]
    fn setting_the_supplementary_groups_taints_whatever_the_list_says() {
        // `kern_setgroups()` calls `setsugid(p)` unconditionally -- it never
        // compares the new list with the old one. A process that has called
        // `setgroups` has been rearranged by whoever called it.
        let proc = a_process();
        proc.set_groups(proc.groups());
        assert!(proc.is_sugid());
    }

    #[test]
    fn every_setter_that_moves_an_id_latches_it() {
        // Six setters, six chances to forget the latch, and forgetting is
        // silent: the program asks whether it is tainted and is told no. So
        // the table is the test -- a seventh setter added without it fails
        // here by name.
        type Setter = (&'static str, fn(&LinuxProcess) -> LxResult);
        let setters: [Setter; 6] = [
            ("setuid", |p| p.set_uid(1000)),
            ("setgid", |p| p.set_gid(1000)),
            ("setreuid", |p| p.set_reuid(1000, 1000)),
            ("setregid", |p| p.set_regid(1000, 1000)),
            ("setresuid", |p| p.set_resuid(1000, 1000, 1000)),
            ("setresgid", |p| p.set_resgid(1000, 1000, 1000)),
        ];
        for (name, call) in setters {
            let proc = a_process();
            call(&proc).unwrap_or_else(|e| panic!("{} was refused: {:?}", name, e));
            assert!(proc.is_sugid(), "{} moved an id without tainting", name);
        }
    }

    #[test]
    fn moving_only_the_saved_id_taints_too() {
        // `setresuid(-1, -1, 1000)` moves nothing a `getuid`/`geteuid` pair
        // would show, and it is still a change of who this process may
        // become -- FreeBSD's `sys_setresuid` latches on the saved id like on
        // the other two. A watch list that only held the real and effective
        // ids would call this a no-op.
        let proc = a_process();
        proc.set_resuid(NO_ID, NO_ID, 1000).unwrap();
        assert_eq!(proc.credentials().suid, 1000);
        assert!(proc.is_sugid());
    }

    #[test]
    fn an_exec_that_grants_nothing_clears_the_taint() {
        // `do_execve()`: `p->p_flag &= ~P_SUGID` when the image granted no id
        // AND the effective ids already match the real ones. The new image
        // did not inherit the old one's data segment -- `execve` threw the
        // address space away -- so the doubt goes with it.
        let mut inner = LinuxProcessInner::default();
        inner.sugid = true;
        assert!(!inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
        assert!(!inner.sugid);
    }

    #[test]
    fn an_exec_cannot_clear_it_while_the_effective_ids_are_uneven() {
        // The other half of the same `else` branch. A process running
        // set-user-ID root keeps its effective id across the exec, so the
        // program it just became is privileged over whoever asked for it --
        // whether or not this particular image had a set-user-ID bit.
        let mut inner = LinuxProcessInner::default();
        inner.credentials.ruid = 1000;
        inner.sugid = false;
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
        assert!(inner.sugid);
    }

    #[test]
    fn a_setuid_exec_taints_a_process_that_was_clean() {
        let mut inner = LinuxProcessInner::default();
        inner.credentials.ruid = 1000;
        inner.credentials.set_euid(1000);
        inner.credentials.suid = 1000;
        inner.credentials.rgid = 1000;
        inner.credentials.set_egid(1000);
        inner.credentials.sgid = 1000;
        inner.credentials.groups = vec![1000];
        assert!(!inner.sugid);
        assert!(inner.apply_exec_ids_from_a_normal_mount(0o4755, ROOT_UID, 1000));
        assert!(inner.sugid);
    }
}

#[cfg(test)]
mod capability_tests {
    //! The privilege model, asked directly: what `capget(2)` publishes and
    //! what every gate honours are the same answer, computed once.

    use super::*;
    use alloc::vec::Vec;

    /// Every capability number named in this file, so a new one added to the
    /// list is automatically measured by the tests below.
    const NAMED: &[(u32, &str)] = &[
        (CAP_SETGID, "CAP_SETGID"),
        (CAP_SYS_ADMIN, "CAP_SYS_ADMIN"),
        (CAP_SYS_BOOT, "CAP_SYS_BOOT"),
        (CAP_SYS_TIME, "CAP_SYS_TIME"),
    ];

    #[test]
    fn the_capability_numbers_are_the_ones_capability_h_names() {
        // Wrong by one and a gate asks about somebody else's privilege.
        assert_eq!(CAP_SETGID, 6);
        assert_eq!(CAP_SYS_ADMIN, 21);
        assert_eq!(CAP_SYS_BOOT, 22);
        assert_eq!(CAP_SYS_TIME, 25);
        // CAP_CHECKPOINT_RESTORE, the last one Linux 5.15 defines.
        assert_eq!(CAP_LAST_CAP, 40);
    }

    #[test]
    fn what_the_kernel_publishes_is_exactly_what_it_honours() {
        // The whole point of the change. A program reads its capability set
        // with `capget(2)` and decides from it whether to even try; the
        // kernel then has to honour exactly that set when the call arrives.
        for euid in [0u32, 1, 1000, u32::MAX] {
            let published = published_capabilities(euid);
            for cap in 0..64u32 {
                let bit = published & (1u64 << cap) != 0;
                assert_eq!(
                    bit,
                    has_capability(euid, cap),
                    "euid {}, cap {}: published {}",
                    euid,
                    cap,
                    bit
                );
            }
        }
    }

    #[test]
    fn root_holds_every_capability_this_kernel_names() {
        for &(cap, name) in NAMED {
            assert!(has_capability(0, cap), "root lacks {}", name);
        }
    }

    #[test]
    fn nobody_else_holds_any_of_them() {
        for euid in [1u32, 100, 1000, u32::MAX] {
            for &(cap, name) in NAMED {
                assert!(!has_capability(euid, cap), "euid {} holds {}", euid, name);
            }
            assert_eq!(published_capabilities(euid), 0);
        }
    }

    #[test]
    fn a_capability_number_this_kernel_does_not_reach_is_held_by_nobody() {
        // Not even by root: `capget` reports bits 0..=CAP_LAST_CAP, so a gate
        // asking about anything above it would be asking about a privilege
        // the kernel never told anyone they had.
        for cap in [CAP_LAST_CAP + 1, 41, 63, 64, u32::MAX] {
            assert!(!has_capability(0, cap), "cap {}", cap);
        }
    }

    #[test]
    fn the_published_set_is_the_bottom_forty_one_bits_and_no_more() {
        // What `capget` used to write down as a constant, derived instead.
        assert_eq!(published_capabilities(0), (1u64 << 41) - 1);
        assert_eq!(published_capabilities(0).count_ones(), CAP_LAST_CAP + 1);
    }

    #[test]
    fn every_named_capability_is_inside_the_range_the_kernel_reports() {
        let over: Vec<&str> = NAMED
            .iter()
            .filter(|&&(cap, _)| cap > CAP_LAST_CAP)
            .map(|&(_, name)| name)
            .collect();
        assert!(over.is_empty(), "past CAP_LAST_CAP: {:?}", over);
    }

    #[test]
    fn the_named_capabilities_are_all_different() {
        for (i, &(a, na)) in NAMED.iter().enumerate() {
            for &(b, nb) in &NAMED[i + 1..] {
                assert_ne!(a, b, "{} and {} are the same number", na, nb);
            }
        }
    }
}

#[cfg(test)]
mod rlimit_tests {
    //! The sixteen resource limits, and who may move them.
    //!
    //! Twelve of the sixteen used to answer `ENOSYS` and three more answered
    //! with a constant, whatever the caller had set. And the hard limit was
    //! not a limit: an earlier batch left a test saying so on the record --
    //! *"this kernel has no capability model to ask"* -- because Linux's
    //! fourth rule needs `CAP_SYS_RESOURCE`. It has one now, so the rule is
    //! here and that test has become its opposite.

    use super::dup_fd_tests::a_process;
    use super::*;

    const USER: u32 = 1000;
    const OTHER: u32 = 2000;

    fn limit(cur: u64, max: u64) -> RLimit {
        RLimit { cur, max }
    }

    /// Ordinary credentials: every id the same, so two processes built this
    /// way with the same numbers can reach each other.
    fn creds(ruid: u32, euid: u32, suid: u32) -> Credentials {
        Credentials {
            ruid,
            euid,
            suid,
            rgid: ruid,
            egid: euid,
            sgid: suid,
            fsuid: euid,
            fsgid: euid,
            groups: Vec::new(),
            umask: 0o022,
        }
    }

    fn check(resource: usize, old: RLimit, new: RLimit, may_raise: bool) -> LxResult {
        LinuxProcess::rlimit_check(resource, old, new, may_raise)
    }

    // ---- the four rules ----------------------------------------------

    #[test]
    fn a_soft_limit_above_the_hard_one_is_einval() {
        // The rule that makes the hard limit mean anything at all.
        assert_eq!(
            check(RLIMIT_NOFILE, limit(1024, 1024), limit(4096, 1024), true),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            check(RLIMIT_CORE, limit(0, RLIM_INFINITY), limit(1, 0), true),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_soft_limit_equal_to_the_hard_one_is_fine() {
        // The boundary is `>`, not `>=`: setting both to the same value is
        // what a process does when it raises itself to its hard limit, the
        // single most common `setrlimit` call there is.
        assert_eq!(
            check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 4096), false),
            Ok(())
        );
    }

    #[test]
    fn the_descriptor_ceiling_is_eperm_and_applies_to_that_resource_alone() {
        // `if (resource == RLIMIT_NOFILE && new_rlim->rlim_max >
        // sysctl_nr_open) return -EPERM;`. EPERM and not EINVAL, and the
        // difference is load-bearing: a caller that sees EPERM retries with a
        // smaller number, one that sees EINVAL concludes it built the struct
        // wrong.
        assert_eq!(
            check(
                RLIMIT_NOFILE,
                limit(0, RLIM_INFINITY),
                limit(NR_OPEN + 1, NR_OPEN + 1),
                true
            ),
            Err(LxError::EPERM)
        );
        assert_eq!(
            check(
                RLIMIT_MEMLOCK,
                limit(0, RLIM_INFINITY),
                limit(NR_OPEN + 1, NR_OPEN + 1),
                true
            ),
            Ok(()),
            "a million is not a lot of bytes, and this rule is about descriptors"
        );
    }

    #[test]
    fn the_descriptor_ceiling_itself_is_accepted() {
        assert_eq!(
            check(
                RLIMIT_NOFILE,
                limit(0, RLIM_INFINITY),
                limit(NR_OPEN, NR_OPEN),
                true
            ),
            Ok(())
        );
    }

    #[test]
    fn the_order_of_the_rules_is_linuxs() {
        // Both wrong at once: `cur > max` is checked first, so this is EINVAL
        // and not EPERM. A caller that retries on EPERM would otherwise spin
        // on a struct that is never going to be accepted.
        assert_eq!(
            check(
                RLIMIT_NOFILE,
                limit(0, RLIM_INFINITY),
                limit(RLIM_INFINITY, NR_OPEN + 1),
                true
            ),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn the_hard_limit_is_a_boundary_now() {
        // `if (new_rlim->rlim_max > rlim->rlim_max && !capable(CAP_SYS_RESOURCE))
        //         retval = -EPERM;`
        //
        // Without it a process that wanted a soft limit past its hard one set
        // both at once and got it, so the hard limit was a lid the process
        // could lift. This is the test that used to assert the opposite.
        assert_eq!(
            check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 8192), false),
            Err(LxError::EPERM)
        );
        assert_eq!(
            check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 8192), true),
            Ok(()),
            "and CAP_SYS_RESOURCE is what lifts it"
        );
    }

    #[test]
    fn lowering_the_hard_limit_needs_nothing_and_does_not_come_back() {
        // A one-way ratchet is the point of the thing: a program drops its
        // own ceiling before running something it does not trust, and that
        // something cannot undo it.
        assert_eq!(
            check(RLIMIT_NOFILE, limit(1024, 4096), limit(64, 64), false),
            Ok(())
        );
        assert_eq!(
            check(RLIMIT_NOFILE, limit(64, 64), limit(64, 4096), false),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn a_hard_limit_that_does_not_move_is_not_a_raise() {
        // The comparison is `>`, so re-setting the same hard limit while
        // moving the soft one is the ordinary unprivileged call.
        assert_eq!(
            check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 4096), false),
            Ok(())
        );
    }

    // ---- the sixteen resources ---------------------------------------

    #[test]
    fn the_resource_numbers_are_the_ones_resource_h_names() {
        // The order IS the numbering, so a row out of place is a program
        // asking about its stack and being told about its core dumps.
        const IN_ORDER: [(usize, &str); RLIM_NLIMITS] = [
            (RLIMIT_CPU, "RLIMIT_CPU"),
            (RLIMIT_FSIZE, "RLIMIT_FSIZE"),
            (RLIMIT_DATA, "RLIMIT_DATA"),
            (RLIMIT_STACK, "RLIMIT_STACK"),
            (RLIMIT_CORE, "RLIMIT_CORE"),
            (RLIMIT_RSS, "RLIMIT_RSS"),
            (RLIMIT_NPROC, "RLIMIT_NPROC"),
            (RLIMIT_NOFILE, "RLIMIT_NOFILE"),
            (RLIMIT_MEMLOCK, "RLIMIT_MEMLOCK"),
            (RLIMIT_AS, "RLIMIT_AS"),
            (RLIMIT_LOCKS, "RLIMIT_LOCKS"),
            (RLIMIT_SIGPENDING, "RLIMIT_SIGPENDING"),
            (RLIMIT_MSGQUEUE, "RLIMIT_MSGQUEUE"),
            (RLIMIT_NICE, "RLIMIT_NICE"),
            (RLIMIT_RTPRIO, "RLIMIT_RTPRIO"),
            (RLIMIT_RTTIME, "RLIMIT_RTTIME"),
        ];
        for (index, (number, name)) in IN_ORDER.iter().enumerate() {
            assert_eq!(*number, index, "{} is not number {}", name, index);
        }
        assert_eq!(RLIM_NLIMITS, 16);
    }

    #[test]
    fn every_resource_answers_and_the_seventeenth_is_einval() {
        // `ulimit -a` walks all sixteen. Twelve of them used to answer
        // `ENOSYS`, which a program reads as "this kernel has no such call"
        // rather than "no such resource".
        let proc = a_process();
        for resource in 0..RLIM_NLIMITS {
            assert!(
                proc.rlimit(resource, None, false).is_ok(),
                "resource {} has no answer",
                resource
            );
        }
        assert_eq!(
            proc.rlimit(RLIM_NLIMITS, None, false),
            Err(LxError::EINVAL),
            "past the end is EINVAL, which is what `do_prlimit` says"
        );
        assert_eq!(proc.rlimit(usize::MAX, None, false), Err(LxError::EINVAL));
    }

    #[test]
    fn what_a_process_starts_with_is_the_table_linux_starts_with() {
        let proc = a_process();
        let at = |r| proc.rlimit(r, None, false).unwrap();
        assert_eq!(at(RLIMIT_NOFILE), limit(1024, 4096), "INR_OPEN_CUR/MAX");
        assert_eq!(at(RLIMIT_STACK), limit(USER_STACK_SIZE, RLIM_INFINITY));
        assert_eq!(
            at(RLIMIT_CORE),
            limit(0, RLIM_INFINITY),
            "no core is ever written, so zero is the size and not a policy"
        );
        assert_eq!(at(RLIMIT_NICE), limit(0, 0));
        assert_eq!(at(RLIMIT_RTPRIO), limit(0, 0));
        assert_eq!(at(RLIMIT_AS), limit(RLIM_INFINITY, RLIM_INFINITY));
        assert_eq!(
            at(RLIMIT_NPROC),
            limit(RLIM_INFINITY, RLIM_INFINITY),
            "Linux's zero is a placeholder init overwrites; nothing counts here"
        );
    }

    #[test]
    fn a_limit_that_is_set_is_the_limit_that_is_read_back() {
        // What three of the four answered resources did not do: they reported
        // a constant, so a program that lowered `RLIMIT_AS` and read it back
        // was told its own request had not happened -- and no error said so.
        let proc = a_process();
        let wanted = limit(64 * 1024 * 1024, 128 * 1024 * 1024);
        assert_eq!(
            proc.rlimit(RLIMIT_AS, Some(wanted), false),
            Ok(limit(RLIM_INFINITY, RLIM_INFINITY)),
            "and the answer is the limit as it WAS"
        );
        assert_eq!(proc.rlimit(RLIMIT_AS, None, false), Ok(wanted));
    }

    #[test]
    fn a_refused_change_leaves_the_limit_where_it_was() {
        let proc = a_process();
        let before = proc.rlimit(RLIMIT_NOFILE, None, false).unwrap();
        assert_eq!(
            proc.rlimit(RLIMIT_NOFILE, Some(limit(8192, 8192)), false),
            Err(LxError::EPERM)
        );
        assert_eq!(proc.rlimit(RLIMIT_NOFILE, None, false), Ok(before));
    }

    #[test]
    fn the_soft_limit_is_what_closes_the_descriptor_table() {
        // The hard limit is a ceiling on what may be ASKED for; the soft one
        // is the budget in force. A table that checked the hard limit would
        // hand out descriptors a program had deliberately stopped itself
        // taking, and `EMFILE` would arrive four thousand files later than
        // the program arranged.
        let proc = a_process();
        proc.rlimit(RLIMIT_NOFILE, Some(limit(2, 4096)), false)
            .unwrap();
        let open =
            || super::dup_fd_tests::an_open_log(super::dup_fd_tests::Log::new(), OpenFlags::WRONLY);
        assert!(proc.add_file(open()).is_ok());
        assert!(proc.add_file(open()).is_ok());
        assert_eq!(proc.add_file(open()), Err(LxError::EMFILE));
    }

    #[test]
    fn the_descriptor_limit_that_is_stored_is_the_one_that_is_enforced() {
        // One table, one row: there is no second place for the number the fd
        // table checks to drift away from the number `getrlimit` reports.
        let proc = a_process();
        proc.rlimit(RLIMIT_NOFILE, Some(limit(64, 4096)), false)
            .unwrap();
        assert_eq!(proc.file_limit(), limit(64, 4096));
    }

    // ---- whose limits they are ---------------------------------------

    #[test]
    fn a_process_with_the_very_same_ids_is_reachable() {
        let caller = creds(USER, USER, USER);
        let target = creds(USER, USER, USER);
        assert!(LinuxProcess::may_touch_limits_of(&caller, &target));
    }

    #[test]
    fn another_users_process_is_not() {
        let caller = creds(USER, USER, USER);
        let target = creds(OTHER, OTHER, OTHER);
        assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
    }

    #[test]
    fn a_target_part_way_through_a_set_user_id_dance_is_out_of_reach() {
        // `id_match` is all six comparisons, not "same user": a target that
        // still holds root in its saved uid is beyond the user who started
        // it, because raising its limits would raise the limits of whatever
        // it is about to become.
        let caller = creds(USER, USER, USER);
        let target = creds(USER, USER, ROOT_UID);
        assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
        let target = creds(USER, ROOT_UID, USER);
        assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
    }

    #[test]
    fn it_is_the_callers_real_ids_that_are_compared() {
        // `cred->uid`, not `cred->euid`. A set-user-ID program does not get
        // to reach further than the user who ran it just by holding an
        // effective id; what it gets instead is `CAP_SYS_RESOURCE`, and only
        // if that effective id is root.
        //
        // The caller here differs from the target in its real UID and in
        // NOTHING else, so the effective ids alone would say yes.
        let mut caller = creds(USER, USER, USER);
        caller.ruid = OTHER;
        let target = creds(USER, USER, USER);
        assert!(
            !LinuxProcess::may_touch_limits_of(&caller, &target),
            "the effective id matches and the real one does not"
        );
        let mut caller = creds(USER, USER, USER);
        caller.rgid = OTHER;
        assert!(
            !LinuxProcess::may_touch_limits_of(&caller, &target),
            "and the same on the group side"
        );
    }

    #[test]
    fn the_group_half_is_compared_too() {
        let caller = creds(USER, USER, USER);
        let mut target = creds(USER, USER, USER);
        target.sgid = OTHER;
        assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
    }

    #[test]
    fn cap_sys_resource_reaches_anybody() {
        let root = creds(ROOT_UID, ROOT_UID, ROOT_UID);
        let target = creds(OTHER, USER, ROOT_UID);
        assert!(LinuxProcess::may_touch_limits_of(&root, &target));
    }

    #[test]
    fn a_root_real_uid_without_a_root_effective_one_does_not() {
        // The capability comes from the EFFECTIVE id, so a root-owned program
        // that has dropped to a user is on the id-match path like anyone
        // else.
        let dropped = creds(ROOT_UID, USER, USER);
        let target = creds(OTHER, OTHER, OTHER);
        assert!(!LinuxProcess::may_touch_limits_of(&dropped, &target));
    }
}

#[cfg(test)]
mod renice_tests {
    //! `setpriority(2)`/`getpriority(2)`: who a `which`/`who` pair names, and
    //! who is allowed to move the nice value once it has been named.
    //!
    //! Both halves were missing. `who` was read for `PRIO_PROCESS` only, so
    //! `renice -g` and `renice -u` aimed at whoever called them, and no gate
    //! asked anything at all, so any task could put itself at nice -20 and
    //! renice anybody else's.

    use super::*;

    const USER: u32 = 1000;
    const OTHER: u32 = 1001;
    /// The nice range Linux allows, `MIN_NICE..=MAX_NICE`.
    const NICE_RANGE: core::ops::RangeInclusive<i8> = -20..=19;

    fn creds(ruid: u32, euid: u32) -> Credentials {
        Credentials {
            ruid,
            euid,
            suid: euid,
            rgid: ruid,
            egid: euid,
            sgid: euid,
            fsuid: euid,
            fsgid: euid,
            groups: Vec::new(),
            umask: 0o022,
        }
    }

    // ---- the encoding -----------------------------------------------------

    #[test]
    fn the_nice_encoding_runs_backwards_so_the_most_favoured_task_is_the_biggest_number() {
        assert_eq!(LinuxProcess::nice_to_rlimit(19), 1, "the meekest task");
        assert_eq!(LinuxProcess::nice_to_rlimit(0), 20, "the default");
        assert_eq!(LinuxProcess::nice_to_rlimit(-20), 40, "the greediest");
        // Which is why getpriority takes a MAXIMUM over a group: the largest
        // number is the smallest nice.
        let group = [5i8, -3, 11];
        assert_eq!(
            group.iter().map(|&n| LinuxProcess::nice_to_rlimit(n)).max(),
            Some(LinuxProcess::nice_to_rlimit(-3)),
            "the best-off member is the one reported"
        );
    }

    #[test]
    fn every_nice_value_encodes_to_at_least_one() {
        for nice in NICE_RANGE {
            assert!(
                LinuxProcess::nice_to_rlimit(nice) >= 1,
                "nice {} encodes to 0 or less",
                nice
            );
        }
    }

    // ---- the budget -------------------------------------------------------

    #[test]
    fn the_default_nice_budget_of_zero_reaches_no_nice_value_at_all() {
        // INIT_RLIMITS gives RLIMIT_NICE {0, 0}, and the encoding never goes
        // below 1, so an unprivileged task cannot lower its own nice by even
        // one step. This is Linux's behaviour and the whole reason
        // CAP_SYS_NICE exists.
        for nice in NICE_RANGE {
            assert!(
                !LinuxProcess::is_nice_reduction(0, nice),
                "budget 0 should reach nothing, but it reached nice {}",
                nice
            );
        }
    }

    #[test]
    fn a_budget_reaches_exactly_down_to_nice_twenty_minus_itself() {
        // Budget 21 == nice_to_rlimit(-1): it reaches -1 and stops there.
        assert!(LinuxProcess::is_nice_reduction(21, -1));
        assert!(!LinuxProcess::is_nice_reduction(21, -2));
        // And the far end: 40 reaches everything.
        assert!(LinuxProcess::is_nice_reduction(40, -20));
    }

    #[test]
    fn raising_the_budget_never_withdraws_a_nice_value_it_already_allowed() {
        for nice in NICE_RANGE {
            for budget in 0..=41u64 {
                if LinuxProcess::is_nice_reduction(budget, nice) {
                    assert!(
                        LinuxProcess::is_nice_reduction(budget + 1, nice),
                        "budget {} allowed nice {} and {} did not",
                        budget,
                        nice,
                        budget + 1
                    );
                }
            }
        }
    }

    #[test]
    fn the_capability_reaches_where_no_budget_does() {
        let root = creds(ROOT_UID, ROOT_UID);
        let user = creds(USER, USER);
        assert!(
            LinuxProcess::can_nice(&root, 0, -20),
            "root with no budget still reaches the bottom"
        );
        assert!(
            !LinuxProcess::can_nice(&user, 0, -20),
            "a user with no budget reaches nothing"
        );
    }

    #[test]
    fn the_budget_works_without_the_capability() {
        let user = creds(USER, USER);
        assert!(
            LinuxProcess::can_nice(&user, 40, -20),
            "a raised RLIMIT_NICE is the unprivileged way down"
        );
    }

    #[test]
    fn it_is_the_callers_effective_uid_that_carries_the_capability() {
        // A root program that dropped its effective uid has stopped being
        // privileged, exactly as everywhere else in this kernel.
        let dropped = creds(ROOT_UID, USER);
        assert!(!LinuxProcess::can_nice(&dropped, 0, -1));
    }

    // ---- whose task is it -------------------------------------------------

    #[test]
    fn a_task_running_as_you_is_yours_to_renice() {
        let caller = creds(USER, USER);
        let target = creds(USER, USER);
        assert!(LinuxProcess::may_set_priority_of(&caller, &target));
    }

    #[test]
    fn a_task_that_merely_turned_into_you_is_yours_too() {
        // target.euid == caller.euid is the second half of set_one_prio_perm.
        let caller = creds(USER, USER);
        let target = creds(OTHER, USER);
        assert!(LinuxProcess::may_set_priority_of(&caller, &target));
    }

    #[test]
    fn a_setuid_program_stays_reniceable_by_the_user_who_started_it() {
        // Its real uid is still yours even though it is running as root: this
        // is why set_one_prio_perm compares the target's REAL uid and not only
        // its effective one.
        let caller = creds(USER, USER);
        let setuid_root = creds(USER, ROOT_UID);
        assert!(LinuxProcess::may_set_priority_of(&caller, &setuid_root));
    }

    #[test]
    fn a_strangers_task_needs_the_capability() {
        let caller = creds(USER, USER);
        let stranger = creds(OTHER, OTHER);
        assert!(!LinuxProcess::may_set_priority_of(&caller, &stranger));
        let root = creds(ROOT_UID, ROOT_UID);
        assert!(LinuxProcess::may_set_priority_of(&root, &stranger));
    }

    #[test]
    fn it_is_the_callers_effective_uid_that_is_compared_not_its_real_one() {
        // A caller that switched to OTHER reaches OTHER's tasks and loses its
        // own, which is the point of comparing cred->euid.
        let switched = creds(USER, OTHER);
        assert!(LinuxProcess::may_set_priority_of(
            &switched,
            &creds(OTHER, OTHER)
        ));
        assert!(!LinuxProcess::may_set_priority_of(
            &switched,
            &creds(USER, USER)
        ));
    }

    #[test]
    fn reniceing_is_a_looser_test_than_touching_limits() {
        // prlimit64 wants every one of the target's ids to be the caller's
        // real id; setpriority wants one of two against the effective one. A
        // target part-way through a set-user-ID dance shows the difference.
        let caller = creds(USER, USER);
        let halfway = creds(USER, ROOT_UID);
        assert!(LinuxProcess::may_set_priority_of(&caller, &halfway));
        assert!(!LinuxProcess::may_touch_limits_of(&caller, &halfway));
    }

    // ---- the verdict ------------------------------------------------------

    fn verdict(
        caller: &Credentials,
        target: &Credentials,
        target_nice: i8,
        budget: u64,
        nice: i8,
    ) -> LxResult<()> {
        LinuxProcess::set_priority_verdict(caller, target, target_nice, budget, nice)
    }

    #[test]
    fn pushing_a_task_further_down_the_queue_is_free() {
        let user = creds(USER, USER);
        assert_eq!(verdict(&user, &user, 0, 0, 10), Ok(()));
    }

    #[test]
    fn standing_still_is_free_too() {
        // niceval < task_nice(p) is strict: setting the value it already has
        // never asks for the budget.
        let user = creds(USER, USER);
        assert_eq!(verdict(&user, &user, 5, 0, 5), Ok(()));
    }

    #[test]
    fn taking_it_back_is_not_free_and_says_eacces() {
        let user = creds(USER, USER);
        assert_eq!(verdict(&user, &user, 5, 0, 4), Err(LxError::EACCES));
    }

    #[test]
    fn a_task_that_is_not_yours_is_eperm_before_the_budget_is_even_asked() {
        // Both gates would fire; EPERM is the one Linux reports, because
        // "that task is not yours" answers the question first.
        let caller = creds(USER, USER);
        let stranger = creds(OTHER, OTHER);
        assert_eq!(verdict(&caller, &stranger, 0, 0, -20), Err(LxError::EPERM));
    }

    #[test]
    fn a_stranger_is_eperm_even_when_the_nice_value_goes_up() {
        // The first gate does not care which way the value moves.
        let caller = creds(USER, USER);
        let stranger = creds(OTHER, OTHER);
        assert_eq!(verdict(&caller, &stranger, 0, 40, 19), Err(LxError::EPERM));
    }

    #[test]
    fn the_budget_that_opens_the_gate_is_the_targets_and_not_the_callers() {
        // can_nice(p, niceval) reads p's RLIMIT_NICE. The caller here has
        // nothing of its own; the task it is reniceing has room.
        let user = creds(USER, USER);
        assert_eq!(verdict(&user, &user, 0, 40, -20), Ok(()));
    }

    #[test]
    fn root_may_take_any_task_all_the_way_back_to_minus_twenty() {
        let root = creds(ROOT_UID, ROOT_UID);
        let stranger = creds(OTHER, OTHER);
        assert_eq!(verdict(&root, &stranger, 19, 0, -20), Ok(()));
    }

    // ---- folding a group's verdicts --------------------------------------

    fn fold_all(members: &[LxResult<()>]) -> LxResult<()> {
        members
            .iter()
            .copied()
            .fold(Err(LxError::ESRCH), LinuxProcess::fold_priority_verdict)
    }

    #[test]
    fn a_set_nobody_could_be_found_in_stays_esrch() {
        // How a pgid that names no live process comes out as ESRCH: nothing
        // ever clears the value the walk starts with.
        assert_eq!(fold_all(&[]), Err(LxError::ESRCH));
        assert_eq!(
            fold_all(&[Err(LxError::ESRCH), Err(LxError::ESRCH)]),
            Err(LxError::ESRCH),
            "members that vanished mid-walk are no different"
        );
    }

    #[test]
    fn one_success_turns_the_initial_esrch_into_success() {
        let r = LinuxProcess::fold_priority_verdict(Err(LxError::ESRCH), Ok(()));
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn a_failure_survives_every_later_success() {
        // The `if (error == -ESRCH) error = 0;` in set_one_prio only clears
        // the INITIAL value, so a group with one member out of reach reports
        // the failure however many members went through.
        let mut r: LxResult<()> = Err(LxError::ESRCH);
        r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EPERM));
        r = LinuxProcess::fold_priority_verdict(r, Ok(()));
        r = LinuxProcess::fold_priority_verdict(r, Ok(()));
        assert_eq!(r, Err(LxError::EPERM));
    }

    #[test]
    fn a_failure_after_a_success_is_reported_too() {
        let mut r: LxResult<()> = Err(LxError::ESRCH);
        r = LinuxProcess::fold_priority_verdict(r, Ok(()));
        assert_eq!(r, Ok(()), "the first member went through");
        r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EPERM));
        assert_eq!(r, Err(LxError::EPERM));
    }

    #[test]
    fn the_last_failure_is_the_one_reported() {
        // set_one_prio overwrites `error` outright on a failure, so of two
        // different failures it is the later one that reaches userspace.
        let mut r: LxResult<()> = Err(LxError::ESRCH);
        r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EACCES));
        r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EPERM));
        assert_eq!(r, Err(LxError::EPERM));
    }

    // ---- who the pair names ----------------------------------------------

    const OWN_TID: KoID = 3;
    const OWN_PGID: KoID = 7;

    #[test]
    fn zero_means_mine_in_each_of_the_three_flavours() {
        assert_eq!(
            prio_target(PRIO_PROCESS, 0, OWN_TID, OWN_PGID, USER),
            Ok(PrioTarget::Thread(OWN_TID))
        );
        assert_eq!(
            prio_target(PRIO_PGRP, 0, OWN_TID, OWN_PGID, USER),
            Ok(PrioTarget::Group(OWN_PGID))
        );
        assert_eq!(
            prio_target(PRIO_USER, 0, OWN_TID, OWN_PGID, USER),
            Ok(PrioTarget::User(USER))
        );
    }

    #[test]
    fn a_named_who_is_the_one_that_gets_used() {
        // This is the bug: `who` used to be read for PRIO_PROCESS only, so
        // `renice -g 99` and `renice -u 1001` aimed at the caller instead.
        assert_eq!(
            prio_target(PRIO_PROCESS, 42, OWN_TID, OWN_PGID, USER),
            Ok(PrioTarget::Thread(42))
        );
        assert_eq!(
            prio_target(PRIO_PGRP, 99, OWN_TID, OWN_PGID, USER),
            Ok(PrioTarget::Group(99)),
            "a named group is not the caller's group"
        );
        assert_eq!(
            prio_target(PRIO_USER, OTHER as usize, OWN_TID, OWN_PGID, USER),
            Ok(PrioTarget::User(OTHER)),
            "a named user is not the caller"
        );
    }

    #[test]
    fn the_three_flavours_do_not_borrow_each_others_ids() {
        // Three different own-ids, so a flavour reaching for the wrong one
        // shows up instead of coinciding.
        for (which, want) in [
            (PRIO_PROCESS, PrioTarget::Thread(OWN_TID)),
            (PRIO_PGRP, PrioTarget::Group(OWN_PGID)),
            (PRIO_USER, PrioTarget::User(USER)),
        ] {
            assert_eq!(
                prio_target(which, 0, OWN_TID, OWN_PGID, USER),
                Ok(want),
                "which={} took the wrong id",
                which
            );
        }
    }

    #[test]
    fn a_which_that_is_not_one_of_the_three_is_einval() {
        assert_eq!(
            prio_target(3, 0, OWN_TID, OWN_PGID, USER),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            prio_target(usize::MAX, 0, OWN_TID, OWN_PGID, USER),
            Err(LxError::EINVAL)
        );
    }
}

#[cfg(test)]
mod sched_permission_tests {
    //! `user_check_sched_setscheduler()`: who may change a task's scheduling
    //! policy, its real-time priority, or its nice value through the
    //! `sched_set*` family.
    //!
    //! Nothing asked before this. Any process could put itself on `SCHED_FIFO`
    //! at priority 99 and keep the machine to itself, and could do it to
    //! another user's threads too.

    use super::*;

    const USER: u32 = 1000;
    const OTHER: u32 = 1001;

    fn creds(ruid: u32, euid: u32) -> Credentials {
        Credentials {
            ruid,
            euid,
            suid: euid,
            rgid: ruid,
            egid: euid,
            sgid: euid,
            fsuid: euid,
            fsgid: euid,
            groups: Vec::new(),
            umask: 0o022,
        }
    }

    /// A task of USER's, on the default policy, with the boot limits: no nice
    /// budget and no real-time budget at all.
    fn on_the_default_policy() -> SchedFacts {
        SchedFacts {
            policy: SCHED_NORMAL,
            nice: 0,
            rt_priority: 0,
            rlimit_nice: 0,
            rlimit_rtprio: 0,
        }
    }

    fn want(policy: u8, nice: i8, rt_priority: u8) -> SchedRequest {
        SchedRequest {
            policy,
            nice,
            rt_priority,
        }
    }

    /// USER asking about a task of USER's.
    fn verdict(now: &SchedFacts, want: &SchedRequest) -> LxResult<()> {
        let user = creds(USER, USER);
        LinuxProcess::may_set_scheduler(&user, &user, now, want)
    }

    // ---- the policy classes ----------------------------------------------

    #[test]
    fn sched_idle_is_not_one_of_the_fair_policies() {
        // Linux counts it apart, and that is what makes LEAVING it cost
        // something: idle sits below nice 19, so everything else is a step up.
        assert!(is_fair_policy(SCHED_NORMAL));
        assert!(is_fair_policy(SCHED_BATCH));
        assert!(!is_fair_policy(SCHED_IDLE));
        assert!(!is_fair_policy(SCHED_FIFO));
    }

    #[test]
    fn only_fifo_and_rr_are_real_time() {
        assert!(is_rt_policy(SCHED_FIFO));
        assert!(is_rt_policy(SCHED_RR));
        assert!(!is_rt_policy(SCHED_NORMAL));
        assert!(!is_rt_policy(SCHED_BATCH));
        assert!(!is_rt_policy(SCHED_IDLE));
        assert!(!is_rt_policy(SCHED_DEADLINE));
    }

    // ---- the fair half ---------------------------------------------------

    #[test]
    fn asking_for_what_the_task_already_has_needs_nothing() {
        assert_eq!(
            verdict(&on_the_default_policy(), &want(SCHED_NORMAL, 0, 0)),
            Ok(())
        );
    }

    #[test]
    fn a_task_may_always_make_itself_meeker() {
        assert_eq!(
            verdict(&on_the_default_policy(), &want(SCHED_NORMAL, 19, 0)),
            Ok(())
        );
    }

    #[test]
    fn lowering_a_nice_value_this_way_says_eperm_where_setpriority_says_eacces() {
        // The same budget, spent through a different syscall, and Linux
        // reports it differently: `sched_setattr` has one errno for every one
        // of its reasons.
        let now = on_the_default_policy();
        assert_eq!(
            verdict(&now, &want(SCHED_NORMAL, -1, 0)),
            Err(LxError::EPERM)
        );
        let user = creds(USER, USER);
        assert_eq!(
            LinuxProcess::set_priority_verdict(&user, &user, now.nice, now.rlimit_nice, -1),
            Err(LxError::EACCES),
            "setpriority's own answer, for the same move"
        );
    }

    #[test]
    fn a_nice_budget_pays_for_the_fair_half() {
        let generous = SchedFacts {
            rlimit_nice: LinuxProcess::nice_to_rlimit(-5),
            ..on_the_default_policy()
        };
        assert_eq!(verdict(&generous, &want(SCHED_NORMAL, -5, 0)), Ok(()));
        assert_eq!(
            verdict(&generous, &want(SCHED_NORMAL, -6, 0)),
            Err(LxError::EPERM),
            "one step past what the budget covers"
        );
    }

    // ---- the real-time half ----------------------------------------------

    #[test]
    fn entering_a_real_time_policy_with_no_budget_is_privileged() {
        assert_eq!(
            verdict(&on_the_default_policy(), &want(SCHED_FIFO, 0, 1)),
            Err(LxError::EPERM),
            "the lowest real-time priority there is, and still refused"
        );
    }

    #[test]
    fn a_task_already_running_real_time_may_keep_its_priority_without_a_budget() {
        // The zero-budget rule only stops a CHANGE of policy. A task that is
        // already on SCHED_FIFO -- put there by root -- can call
        // sched_setparam with the priority it has.
        let running = SchedFacts {
            policy: SCHED_FIFO,
            rt_priority: 30,
            ..on_the_default_policy()
        };
        assert_eq!(verdict(&running, &want(SCHED_FIFO, 0, 30)), Ok(()));
    }

    #[test]
    fn switching_between_the_two_real_time_policies_still_needs_a_budget() {
        // The zero-budget rule bars a CHANGE of policy, and FIFO to RR is one
        // even though the priority does not move -- so this is refused where
        // asking for the very same policy and priority would go through.
        let running = SchedFacts {
            policy: SCHED_FIFO,
            rt_priority: 30,
            ..on_the_default_policy()
        };
        assert_eq!(
            verdict(&running, &want(SCHED_RR, 0, 30)),
            Err(LxError::EPERM)
        );
        assert_eq!(
            verdict(&running, &want(SCHED_FIFO, 0, 30)),
            Ok(()),
            "staying put is the case this one is being told apart from"
        );
    }

    #[test]
    fn a_real_time_task_may_always_give_priority_back() {
        let running = SchedFacts {
            policy: SCHED_FIFO,
            rt_priority: 50,
            ..on_the_default_policy()
        };
        assert_eq!(verdict(&running, &want(SCHED_FIFO, 0, 1)), Ok(()));
    }

    #[test]
    fn raising_a_real_time_priority_is_capped_by_the_budget() {
        let running = SchedFacts {
            policy: SCHED_FIFO,
            rt_priority: 10,
            rlimit_rtprio: 20,
            ..on_the_default_policy()
        };
        assert_eq!(verdict(&running, &want(SCHED_FIFO, 0, 20)), Ok(()));
        assert_eq!(
            verdict(&running, &want(SCHED_FIFO, 0, 21)),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn a_real_time_budget_lets_a_task_in_without_the_capability() {
        let ready = SchedFacts {
            rlimit_rtprio: 10,
            ..on_the_default_policy()
        };
        assert_eq!(verdict(&ready, &want(SCHED_RR, 0, 10)), Ok(()));
        assert_eq!(
            verdict(&ready, &want(SCHED_RR, 0, 11)),
            Err(LxError::EPERM),
            "above the budget, even on the way in"
        );
    }

    #[test]
    fn the_nice_value_carried_along_is_not_checked_under_a_real_time_policy() {
        // fair_policy(policy) is false for FIFO, so the nice clause does not
        // run; the value is stored and means nothing until the task goes back
        // to a fair policy. This is Linux's behaviour, not an oversight here.
        let ready = SchedFacts {
            rlimit_rtprio: 10,
            ..on_the_default_policy()
        };
        assert_eq!(verdict(&ready, &want(SCHED_FIFO, -20, 10)), Ok(()));
    }

    // ---- leaving SCHED_IDLE ----------------------------------------------

    #[test]
    fn staying_in_sched_idle_is_free() {
        let idle = SchedFacts {
            policy: SCHED_IDLE,
            ..on_the_default_policy()
        };
        assert_eq!(verdict(&idle, &want(SCHED_IDLE, 0, 0)), Ok(()));
    }

    #[test]
    fn leaving_sched_idle_costs_the_nice_value_the_task_already_has() {
        // Not the nice value asked for: the one it has. Coming off idle is
        // itself the step up, so the budget has to cover where the task
        // lands.
        let idle = SchedFacts {
            policy: SCHED_IDLE,
            ..on_the_default_policy()
        };
        assert_eq!(
            verdict(&idle, &want(SCHED_NORMAL, 0, 0)),
            Err(LxError::EPERM)
        );
        let idle_with_budget = SchedFacts {
            rlimit_nice: LinuxProcess::nice_to_rlimit(0),
            ..idle
        };
        assert_eq!(
            verdict(&idle_with_budget, &want(SCHED_NORMAL, 0, 0)),
            Ok(())
        );
    }

    #[test]
    fn leaving_sched_idle_is_judged_on_the_nice_it_has_and_not_the_one_asked_for() {
        // A task parked on SCHED_IDLE at nice -5, asking for SCHED_NORMAL at
        // nice 19, is asking to be MEEKER -- and is still refused. Coming off
        // idle lands it at -5 whatever it says, and -5 is what the budget has
        // to cover.
        let idle = SchedFacts {
            policy: SCHED_IDLE,
            nice: -5,
            rlimit_nice: LinuxProcess::nice_to_rlimit(19),
            ..on_the_default_policy()
        };
        assert_eq!(
            verdict(&idle, &want(SCHED_NORMAL, 19, 0)),
            Err(LxError::EPERM)
        );
        let enough = SchedFacts {
            rlimit_nice: LinuxProcess::nice_to_rlimit(-5),
            ..idle
        };
        assert_eq!(verdict(&enough, &want(SCHED_NORMAL, 19, 0)), Ok(()));
    }

    // ---- whose task, and the way past ------------------------------------

    #[test]
    fn another_users_task_is_eperm_however_modest_the_request() {
        // Same policy, same nice, nothing asked for: it is still not yours.
        let caller = creds(USER, USER);
        let stranger = creds(OTHER, OTHER);
        assert_eq!(
            LinuxProcess::may_set_scheduler(
                &caller,
                &stranger,
                &on_the_default_policy(),
                &want(SCHED_NORMAL, 0, 0)
            ),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn a_setuid_program_stays_schedulable_by_the_user_who_started_it() {
        let caller = creds(USER, USER);
        let setuid_root = creds(USER, ROOT_UID);
        assert_eq!(
            LinuxProcess::may_set_scheduler(
                &caller,
                &setuid_root,
                &on_the_default_policy(),
                &want(SCHED_NORMAL, 0, 0)
            ),
            Ok(())
        );
    }

    #[test]
    fn the_capability_is_the_one_way_past_every_one_of_these() {
        let root = creds(ROOT_UID, ROOT_UID);
        let stranger = creds(OTHER, OTHER);
        let idle = SchedFacts {
            policy: SCHED_IDLE,
            ..on_the_default_policy()
        };
        // Another user's task, coming off idle, straight to the top of the
        // real-time range, with every budget at zero.
        assert_eq!(
            LinuxProcess::may_set_scheduler(&root, &stranger, &idle, &want(SCHED_FIFO, -20, 99)),
            Ok(())
        );
    }

    #[test]
    fn sched_deadline_is_privileged_outright() {
        // Unreachable through the syscalls today -- the parameter check
        // refuses it with EINVAL first, there being no deadline runqueue --
        // but the rule belongs with the others.
        assert_eq!(
            verdict(&on_the_default_policy(), &want(SCHED_DEADLINE, 0, 0)),
            Err(LxError::EPERM)
        );
        let root = creds(ROOT_UID, ROOT_UID);
        assert_eq!(
            LinuxProcess::may_set_scheduler(
                &root,
                &root,
                &on_the_default_policy(),
                &want(SCHED_DEADLINE, 0, 0)
            ),
            Ok(())
        );
    }
}

#[cfg(test)]
mod ptrace_access_tests {
    //! `__ptrace_may_access(PTRACE_MODE_ATTACH_REALCREDS)`: who may reach INTO
    //! another process rather than merely signal it.
    //!
    //! `pidfd_getfd(2)` asked nothing, so any process could take any open file
    //! out of any other. It is the fifth syscall in a row to touch another
    //! process with no gate, and the fourth DIFFERENT set of ids: `kill` reads
    //! two of the target's, the renice two others, `prlimit64` all six, and
    //! this one all six again but for another capability. Copying the
    //! neighbour's rule is the bug this module exists to catch.

    use super::*;

    const USER: u32 = 1000;
    const GROUP: u32 = 1000;
    const OTHER: u32 = 1001;
    const OTHER_GROUP: u32 = 1001;

    fn ids(ruid: u32, euid: u32, suid: u32, rgid: u32, egid: u32, sgid: u32) -> Credentials {
        Credentials {
            ruid,
            euid,
            suid,
            rgid,
            egid,
            sgid,
            fsuid: euid,
            fsgid: egid,
            groups: Vec::new(),
            umask: 0o022,
        }
    }

    /// A process that has done nothing clever: one uid and one gid, three times
    /// over.
    fn plain(uid: u32, gid: u32) -> Credentials {
        ids(uid, uid, uid, gid, gid, gid)
    }

    /// The same, after a `setfsuid`/`setfsgid` that moved the filesystem pair
    /// off the effective one.
    fn with_fs(mut creds: Credentials, fsuid: u32, fsgid: u32) -> Credentials {
        creds.fsuid = fsuid;
        creds.fsgid = fsgid;
        creds
    }

    // ---- the six comparisons ----------------------------------------------

    #[test]
    fn a_process_of_yours_is_yours_to_reach_into() {
        assert!(LinuxProcess::may_attach_to(
            &plain(USER, GROUP),
            &plain(USER, GROUP),
            false
        ));
    }

    #[test]
    fn another_users_process_is_not() {
        assert!(!LinuxProcess::may_attach_to(
            &plain(USER, GROUP),
            &plain(OTHER, OTHER_GROUP),
            false
        ));
    }

    #[test]
    fn every_one_of_the_targets_three_uids_has_to_match() {
        // Not "same user": "that process holds no id I do not already hold".
        // One id out of six is enough to put it out of reach, so each is named
        // on its own rather than swept in a loop -- a loop would pass if the
        // predicate read one field six times.
        let caller = plain(USER, GROUP);
        assert!(!LinuxProcess::may_attach_to(
            &caller,
            &ids(OTHER, USER, USER, GROUP, GROUP, GROUP),
            false
        ));
        assert!(!LinuxProcess::may_attach_to(
            &caller,
            &ids(USER, OTHER, USER, GROUP, GROUP, GROUP),
            false
        ));
        assert!(!LinuxProcess::may_attach_to(
            &caller,
            &ids(USER, USER, OTHER, GROUP, GROUP, GROUP),
            false
        ));
    }

    #[test]
    fn and_every_one_of_its_three_gids() {
        // The groups are half the rule and the half a signal check does not
        // have at all: `may_signal_cred` never looks at a gid. A caller in
        // another group may kill the process and may not read its files.
        let caller = plain(USER, GROUP);
        assert!(!LinuxProcess::may_attach_to(
            &caller,
            &ids(USER, USER, USER, OTHER_GROUP, GROUP, GROUP),
            false
        ));
        assert!(!LinuxProcess::may_attach_to(
            &caller,
            &ids(USER, USER, USER, GROUP, OTHER_GROUP, GROUP),
            false
        ));
        assert!(!LinuxProcess::may_attach_to(
            &caller,
            &ids(USER, USER, USER, GROUP, GROUP, OTHER_GROUP),
            false
        ));
        assert!(LinuxProcess::may_signal_cred(
            &caller,
            &ids(USER, USER, USER, OTHER_GROUP, OTHER_GROUP, OTHER_GROUP)
        ));
    }

    // ---- which of the CALLER's ids ----------------------------------------

    #[test]
    fn it_is_the_callers_real_pair_that_is_compared() {
        // `PTRACE_MODE_REALCREDS`, and Linux's own comment says using the
        // effective uid "would make more sense here" -- but userland relies on
        // the old behaviour, so the real one it is. A process whose effective
        // uid has moved keeps the reach its real one gives it.
        let moved_euid = ids(USER, OTHER, USER, GROUP, OTHER_GROUP, GROUP);
        assert!(LinuxProcess::may_attach_to(
            &moved_euid,
            &plain(USER, GROUP),
            false
        ));
    }

    #[test]
    fn an_effective_uid_you_borrowed_does_not_reach_into_anything() {
        // The mirror image, and the one that matters: `kill` accepts the
        // caller's EFFECTIVE uid against the target's real one, so this same
        // pair may be killed. Two rules over one pair of credentials, two
        // answers.
        let borrowed = ids(OTHER, USER, OTHER, OTHER_GROUP, GROUP, OTHER_GROUP);
        let target = plain(USER, GROUP);
        assert!(LinuxProcess::may_signal_cred(&borrowed, &target));
        assert!(!LinuxProcess::may_attach_to(&borrowed, &target, false));
    }

    #[test]
    fn a_setuid_program_is_out_of_reach_for_the_user_who_started_it() {
        // The headline case, and why the rule is this strict: the user's own
        // shell started a set-user-ID root program, so the user may still KILL
        // it -- its real and saved uids are theirs. Taking its open files would
        // be taking root's files, so the reach stops at the effective uid it
        // holds.
        let user = plain(USER, GROUP);
        let setuid_root = ids(USER, ROOT_UID, ROOT_UID, GROUP, ROOT_UID, ROOT_UID);
        assert!(LinuxProcess::may_signal_cred(&user, &setuid_root));
        assert!(!LinuxProcess::may_attach_to(&user, &setuid_root, false));
    }

    // ---- the capability, and the escape -----------------------------------

    #[test]
    fn cap_sys_ptrace_is_nineteen() {
        // From the ABI (`include/uapi/linux/capability.h`), so asserted
        // against the literal and not against another name for it.
        assert_eq!(CAP_SYS_PTRACE, 19);
    }

    #[test]
    fn the_capability_lifts_the_six_comparisons() {
        assert!(LinuxProcess::may_attach_to(
            &plain(ROOT_UID, ROOT_UID),
            &plain(OTHER, OTHER_GROUP),
            false
        ));
    }

    #[test]
    fn the_capability_is_read_from_the_callers_effective_uid() {
        // Not the real one the comparisons use: the ids ask who you ARE and
        // the capability asks what you may DO, and in this kernel the second
        // is the effective uid. A root program that dropped to a user keeps
        // neither.
        let root_by_real_uid_only = ids(ROOT_UID, USER, USER, ROOT_UID, GROUP, GROUP);
        assert!(!LinuxProcess::may_attach_to(
            &root_by_real_uid_only,
            &plain(OTHER, OTHER_GROUP),
            false
        ));
        let root_by_effective_uid = ids(USER, ROOT_UID, USER, GROUP, ROOT_UID, GROUP);
        assert!(LinuxProcess::may_attach_to(
            &root_by_effective_uid,
            &plain(OTHER, OTHER_GROUP),
            false
        ));
    }

    #[test]
    fn your_own_thread_group_is_reached_without_reading_an_id() {
        // Linux short-circuits the whole check for your own thread group, and
        // it is kept because a pidfd on yourself is how `pidfd_getfd` spells
        // `dup`. With one credential set per thread group the ids would say
        // yes anyway, so this cannot change an answer here -- it is Linux's
        // seventh test, the non-dumpable one this kernel does not have, that
        // the escape exists to get past.
        let me = ids(USER, ROOT_UID, ROOT_UID, GROUP, ROOT_UID, ROOT_UID);
        assert!(LinuxProcess::may_attach_to(&me, &me, true));
        assert!(LinuxProcess::may_attach_to(&me, &me, false));
    }

    // ---- READ_FSCREDS: the same six comparisons, the other pair -----------

    /// A caller whose filesystem pair has moved off its real one: a
    /// set-user-ID-`OTHER` program, started by `USER`, that called
    /// `setfsuid(OTHER)`. That is the only way the two rules can disagree, and
    /// `setfsuid(2)` only ever moves the pair to an id the process already
    /// holds.
    fn moved_fs_pair() -> Credentials {
        with_fs(
            ids(USER, OTHER, OTHER, GROUP, OTHER_GROUP, OTHER_GROUP),
            OTHER,
            OTHER_GROUP,
        )
    }

    #[test]
    fn reading_a_process_goes_by_the_filesystem_pair() {
        // `PTRACE_MODE_READ_FSCREDS`, which every gated file of `/proc/<pid>/`
        // asks for: a path through the filesystem is judged on the identity the
        // filesystem uses.
        assert!(LinuxProcess::may_read_process_innards(
            &moved_fs_pair(),
            &plain(OTHER, OTHER_GROUP),
            false
        ));
    }

    #[test]
    fn taking_its_files_goes_by_the_real_pair() {
        // The same caller, the same target, the other answer. `REALCREDS`
        // exists for exactly this: a syscall that names a process is judged on
        // the identity the caller cannot lay down with `setfsuid`.
        assert!(!LinuxProcess::may_attach_to(
            &moved_fs_pair(),
            &plain(OTHER, OTHER_GROUP),
            false
        ));
    }

    #[test]
    fn so_does_moving_its_limits() {
        assert!(!LinuxProcess::may_touch_limits_of(
            &moved_fs_pair(),
            &plain(OTHER, OTHER_GROUP)
        ));
    }

    #[test]
    fn and_the_mirror_image_answers_the_other_way_round() {
        // A caller whose real pair is the target's and whose filesystem pair
        // has moved away: now the procfs rule refuses and the other two allow.
        // Without this half, reading `fsuid` where `ruid` was meant would still
        // pass the three tests above.
        let moved_off = with_fs(plain(OTHER, OTHER_GROUP), USER, GROUP);
        let target = plain(OTHER, OTHER_GROUP);
        assert!(!LinuxProcess::may_read_process_innards(
            &moved_off, &target, false
        ));
        assert!(LinuxProcess::may_attach_to(&moved_off, &target, false));
        assert!(LinuxProcess::may_touch_limits_of(&moved_off, &target));
    }

    #[test]
    fn a_process_that_never_called_setfsuid_gets_one_answer() {
        // The ordinary case, and why a wrong pair hides: `fsuid` follows `euid`
        // everywhere else, so for almost every process in the machine the two
        // rules agree and the wrong argument reads as correct code.
        for (caller, target) in [
            (plain(USER, GROUP), plain(USER, GROUP)),
            (plain(USER, GROUP), plain(OTHER, OTHER_GROUP)),
            (
                plain(USER, GROUP),
                ids(USER, OTHER, USER, GROUP, GROUP, GROUP),
            ),
            (plain(ROOT_UID, ROOT_UID), plain(OTHER, OTHER_GROUP)),
        ] {
            assert_eq!(
                LinuxProcess::may_read_process_innards(&caller, &target, false),
                LinuxProcess::may_attach_to(&caller, &target, false),
                "{:?} against {:?}",
                caller,
                target
            );
        }
    }

    #[test]
    fn your_own_process_is_readable_whatever_its_ids() {
        // `/proc/self/environ` is how a program reads its own environment and
        // `/proc/self/maps` how it reads its own map: the thread group goes
        // through first, as in Linux.
        let me = with_fs(
            ids(USER, ROOT_UID, ROOT_UID, GROUP, ROOT_UID, ROOT_UID),
            OTHER,
            OTHER_GROUP,
        );
        assert!(LinuxProcess::may_read_process_innards(&me, &me, true));
    }

    #[test]
    fn the_capability_lifts_the_procfs_rule_from_the_effective_uid() {
        // Still the effective uid, not the filesystem one: `setfsuid` moves
        // what you are for a file, never what you may do.
        let dropped_fsuid = with_fs(plain(ROOT_UID, ROOT_UID), USER, GROUP);
        assert!(LinuxProcess::may_read_process_innards(
            &dropped_fsuid,
            &plain(OTHER, OTHER_GROUP),
            false
        ));
        let root_by_fsuid_only = with_fs(plain(USER, GROUP), ROOT_UID, ROOT_UID);
        assert!(!LinuxProcess::may_read_process_innards(
            &root_by_fsuid_only,
            &plain(OTHER, OTHER_GROUP),
            false
        ));
    }
}

#[cfg(test)]
mod kill_permission_tests {
    //! `check_kill_permission()`: who may signal whom, and how the verdicts
    //! of a whole group or a whole machine fold into one answer.
    //!
    //! `sys_kill` asked nothing at all, so any process could `SIGKILL` any
    //! other. It is the same shape of hole as the scheduler's, with a
    //! different set of ids -- and the difference between the two sets is
    //! the interesting part.

    use super::*;

    const USER: u32 = 1000;
    const OTHER: u32 = 1001;

    fn creds(ruid: u32, euid: u32, suid: u32) -> Credentials {
        Credentials {
            ruid,
            euid,
            suid,
            rgid: ruid,
            egid: euid,
            sgid: suid,
            fsuid: euid,
            fsgid: euid,
            groups: Vec::new(),
            umask: 0o022,
        }
    }

    fn plain(uid: u32) -> Credentials {
        creds(uid, uid, uid)
    }

    // ---- which four ids ---------------------------------------------------

    #[test]
    fn a_process_running_as_you_is_yours_to_kill() {
        assert!(LinuxProcess::may_signal_cred(&plain(USER), &plain(USER)));
    }

    #[test]
    fn a_setuid_program_stays_killable_by_the_user_who_started_it() {
        // Its real and saved uids are still yours while it runs as root.
        let started_by_user = creds(USER, ROOT_UID, USER);
        assert!(LinuxProcess::may_signal_cred(
            &plain(USER),
            &started_by_user
        ));
    }

    #[test]
    fn the_user_a_program_turned_into_cannot_kill_it() {
        // The one id that does NOT count is the target's effective uid. A
        // program that dropped from root to OTHER keeps ruid/suid = root, so
        // OTHER -- whom it is now running as -- still cannot touch it.
        let dropped_to_other = creds(ROOT_UID, OTHER, ROOT_UID);
        assert!(!LinuxProcess::may_signal_cred(
            &plain(OTHER),
            &dropped_to_other
        ));
    }

    #[test]
    fn the_targets_effective_uid_is_the_one_id_that_does_not_count() {
        // The same pair, read by the two rules: reniceing looks at the
        // target's effective uid and killing does not, so a process that has
        // turned into you is yours to renice and not yours to kill.
        let caller = plain(USER);
        let turned_into_you = creds(OTHER, USER, OTHER);
        assert!(LinuxProcess::may_set_priority_of(&caller, &turned_into_you));
        assert!(!LinuxProcess::may_signal_cred(&caller, &turned_into_you));
    }

    #[test]
    fn both_of_the_callers_uids_count() {
        // A caller half-way through a set-user-ID dance reaches the processes
        // of BOTH users -- `cred->euid` and `cred->uid` each get a turn.
        let half_way = creds(USER, OTHER, OTHER);
        assert!(LinuxProcess::may_signal_cred(&half_way, &plain(USER)));
        assert!(LinuxProcess::may_signal_cred(&half_way, &plain(OTHER)));
    }

    #[test]
    fn each_of_the_four_pairs_reaches_on_its_own() {
        // Four ids compared and no coincidences: each case matches exactly
        // one of the four clauses, so dropping any one of them shows up.
        // The caller is `ruid A, euid B`; none of these uids is root.
        const A: u32 = 10;
        const B: u32 = 11;
        const C: u32 = 12;
        let caller = creds(A, B, A);
        assert!(
            LinuxProcess::may_signal_cred(&caller, &creds(C, C, B)),
            "cred->euid against tcred->suid"
        );
        assert!(
            LinuxProcess::may_signal_cred(&caller, &creds(B, C, C)),
            "cred->euid against tcred->uid"
        );
        assert!(
            LinuxProcess::may_signal_cred(&caller, &creds(C, C, A)),
            "cred->uid against tcred->suid"
        );
        assert!(
            LinuxProcess::may_signal_cred(&caller, &creds(A, C, C)),
            "cred->uid against tcred->uid"
        );
        assert!(
            !LinuxProcess::may_signal_cred(&caller, &creds(C, C, C)),
            "and nothing in common reaches nothing"
        );
    }

    #[test]
    fn a_strangers_process_needs_the_capability() {
        assert!(!LinuxProcess::may_signal_cred(&plain(USER), &plain(OTHER)));
        assert!(LinuxProcess::may_signal_cred(
            &plain(ROOT_UID),
            &plain(OTHER)
        ));
    }

    #[test]
    fn the_capability_is_read_off_the_callers_effective_uid() {
        let dropped_root = creds(ROOT_UID, USER, ROOT_UID);
        assert!(!LinuxProcess::may_signal_cred(&dropped_root, &plain(OTHER)));
    }

    // ---- the two ways past ------------------------------------------------

    fn verdict(
        caller: &Credentials,
        target: &Credentials,
        same_group: bool,
        same_session: bool,
        signal: Option<LinuxSignal>,
    ) -> LxResult<()> {
        LinuxProcess::may_signal(caller, target, same_group, same_session, signal)
    }

    #[test]
    fn a_thread_always_reaches_its_own_thread_group() {
        // Whatever the ids say: a process signalling itself is never a
        // permission question.
        assert_eq!(
            verdict(
                &plain(USER),
                &plain(OTHER),
                true,
                false,
                Some(LinuxSignal::SIGKILL)
            ),
            Ok(())
        );
    }

    #[test]
    fn sigcont_reaches_your_own_session_whoever_owns_it() {
        // The shell that resumes a job is not always the job's owner, and a
        // session nobody could continue would be a session that can be wedged
        // from the inside.
        assert_eq!(
            verdict(
                &plain(USER),
                &plain(OTHER),
                false,
                true,
                Some(LinuxSignal::SIGCONT)
            ),
            Ok(())
        );
    }

    #[test]
    fn sigcont_to_another_session_is_eperm_like_anything_else() {
        assert_eq!(
            verdict(
                &plain(USER),
                &plain(OTHER),
                false,
                false,
                Some(LinuxSignal::SIGCONT)
            ),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn no_signal_but_sigcont_gets_the_session_exception() {
        for sig in [
            LinuxSignal::SIGTERM,
            LinuxSignal::SIGKILL,
            LinuxSignal::SIGHUP,
            LinuxSignal::SIGSTOP,
        ] {
            assert_eq!(
                verdict(&plain(USER), &plain(OTHER), false, true, Some(sig)),
                Err(LxError::EPERM),
                "{:?} should not ride the SIGCONT exception",
                sig
            );
        }
    }

    #[test]
    fn a_probe_is_refused_exactly_like_a_signal() {
        // kill(pid, 0) delivers nothing and asks the same question. If it
        // did not, it would be a way to ask whether a process you may not
        // signal exists.
        assert_eq!(
            verdict(&plain(USER), &plain(OTHER), false, false, None),
            Err(LxError::EPERM)
        );
        assert_eq!(
            verdict(&plain(USER), &plain(OTHER), false, true, None),
            Err(LxError::EPERM),
            "and it is not SIGCONT, so the session does not help it"
        );
    }

    #[test]
    fn the_capability_reaches_any_owner_in_any_session() {
        assert_eq!(
            verdict(
                &plain(ROOT_UID),
                &plain(OTHER),
                false,
                false,
                Some(LinuxSignal::SIGKILL)
            ),
            Ok(())
        );
    }

    // ---- folding a group --------------------------------------------------

    fn fold_group(members: &[LxResult<()>]) -> LxResult<()> {
        members
            .iter()
            .copied()
            .fold(Err(LxError::ESRCH), LinuxProcess::fold_group_signal)
    }

    #[test]
    fn a_group_with_no_members_is_esrch() {
        assert_eq!(fold_group(&[]), Err(LxError::ESRCH));
    }

    #[test]
    fn one_member_reached_makes_the_whole_group_send_a_success() {
        assert_eq!(fold_group(&[Err(LxError::EPERM), Ok(())]), Ok(()));
        assert_eq!(
            fold_group(&[Ok(()), Err(LxError::EPERM)]),
            Ok(()),
            "and a later refusal cannot take the success back"
        );
    }

    #[test]
    fn a_group_that_refused_every_member_reports_the_last_refusal() {
        assert_eq!(
            fold_group(&[Err(LxError::ESRCH), Err(LxError::EPERM)]),
            Err(LxError::EPERM)
        );
        assert_eq!(
            fold_group(&[Err(LxError::EPERM), Err(LxError::EINVAL)]),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_group_signal_and_a_group_renice_fold_the_opposite_way() {
        // Same two members, one refused and one reached. The signal landed
        // somewhere, so the call worked; the renice did not go through
        // everywhere, so it did not.
        let members = [Err(LxError::EPERM), Ok(())];
        assert_eq!(fold_group(&members), Ok(()));
        assert_eq!(
            members
                .iter()
                .copied()
                .fold(Err(LxError::ESRCH), LinuxProcess::fold_priority_verdict),
            Err(LxError::EPERM)
        );
    }

    // ---- folding a broadcast ----------------------------------------------

    fn fold_broadcast(seen: &[LxResult<()>]) -> LxResult<()> {
        seen.iter()
            .copied()
            .fold(Ok(()), LinuxProcess::fold_broadcast_signal)
    }

    #[test]
    fn a_broadcast_that_every_process_refused_still_reports_success() {
        // Deliberate in Linux: kill(-1, SIGTERM) means "everything I am
        // allowed to", and being allowed nothing is not an error. `retval`
        // starts at 0 and EPERM is the one error that never reaches it.
        assert_eq!(
            fold_broadcast(&[Err(LxError::EPERM), Err(LxError::EPERM)]),
            Ok(())
        );
    }

    #[test]
    fn a_broadcast_reports_an_error_that_is_not_eperm() {
        assert_eq!(
            fold_broadcast(&[Err(LxError::EPERM), Err(LxError::EINVAL)]),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_later_process_overwrites_an_earlier_error_in_a_broadcast() {
        // `retval = err` with no guard, so the last non-EPERM answer is the
        // one that comes back -- a success included.
        assert_eq!(fold_broadcast(&[Err(LxError::EINVAL), Ok(())]), Ok(()));
    }
}

#[cfg(test)]
mod fsid_tests {
    //! `setfsuid(2)`/`setfsgid(2)`, and the id a file access is really
    //! checked against.
    //!
    //! Linux keeps `fsuid` apart from `euid` on purpose;
    //! `generic_permission()` says why in its own comment: "We use `fsuid`
    //! for this, letting us set arbitrary permissions for filesystem access
    //! without changing the 'normal' uids which are used for other things."
    //! A file server takes an id from the wire, wears it while it touches the
    //! file, and puts it back -- without giving up the privileges it needs
    //! for its own sockets and its own log.
    //!
    //! This kernel had no `fsuid` at all. `setfsuid` took the argument,
    //! ignored it and returned the caller's `euid`, which is **exactly what a
    //! call that worked looks like**: the syscall cannot fail, so it returns
    //! the previous id either way and there is no error for the caller to
    //! check. The server went on reading the file as root, with nothing
    //! anywhere saying so.
    //!
    //! Three parts, then: who may move the id, who drags it along (every
    //! `set*id` path and `execve`, where a forgotten line is a privilege the
    //! caller believes it put down), and what actually asks it.

    use super::dup_fd_tests::a_process;
    use super::*;
    use rcore_fs::vfs::{FileType, FsError, PollStatus, Timespec};

    const REAL: u32 = 1000;
    const SAVED: u32 = 1500;
    const OTHER: u32 = 2000;
    const SPOOL: u32 = 4242;

    /// The credentials of an ordinary, untainted process.
    fn creds(ruid: u32, euid: u32, suid: u32) -> Credentials {
        Credentials {
            ruid,
            euid,
            suid,
            rgid: ruid,
            egid: euid,
            sgid: suid,
            fsuid: euid,
            fsgid: euid,
            groups: Vec::new(),
            umask: 0o022,
        }
    }

    /// A live process wearing those ids.
    fn process_as(ruid: u32, euid: u32, suid: u32) -> LinuxProcess {
        let proc = a_process();
        proc.inner.lock().credentials = creds(ruid, euid, suid);
        proc
    }

    fn a_metadata(mode: u16, uid: u32, gid: u32) -> Metadata {
        Metadata {
            dev: 1,
            inode: 7,
            size: 0,
            blk_size: 4096,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode,
            nlinks: 1,
            uid: uid as _,
            gid: gid as _,
            rdev: 0,
        }
    }

    /// An inode that remembers who it ended up belonging to.
    struct Owned(Mutex<Metadata>);

    impl Owned {
        fn new() -> Arc<Self> {
            Arc::new(Owned(Mutex::new(a_metadata(0o644, NO_ID, NO_ID))))
        }
    }

    impl INode for Owned {
        fn read_at(&self, _: usize, _: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::NotSupported)
        }
        fn write_at(&self, _: usize, _: &[u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::NotSupported)
        }
        fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
            Err(FsError::NotSupported)
        }
        fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
            Ok(self.0.lock().clone())
        }
        fn set_metadata(&self, metadata: &Metadata) -> rcore_fs::vfs::Result<()> {
            *self.0.lock() = metadata.clone();
            Ok(())
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    // ---- who may move it ---------------------------------------------

    #[test]
    fn the_id_already_in_force_is_in_the_set_and_in_no_other_rule() {
        // The one thing that tells `setfsid_allowed` apart from the rule
        // every other `set*id` call uses. Without it a process whose
        // filesystem id sits somewhere the rest of its ids never were could
        // not name that id again -- not even to ask for it back.
        assert!(
            LinuxProcess::setfsid_allowed(REAL, REAL, REAL, SPOOL, SPOOL),
            "the acting id must be nameable"
        );
        assert!(
            !LinuxProcess::set_any_allowed(REAL, REAL, REAL, SPOOL),
            "and no other rule accepts it, which is why this one exists"
        );
    }

    #[test]
    fn the_three_ordinary_ids_are_in_the_set_too() {
        for id in [REAL, OTHER, SAVED] {
            assert!(
                LinuxProcess::setfsid_allowed(REAL, OTHER, SAVED, REAL, id),
                "{} is one of the caller's own ids",
                id
            );
        }
    }

    #[test]
    fn an_id_the_caller_never_held_is_refused() {
        assert!(!LinuxProcess::setfsid_allowed(
            REAL, OTHER, SAVED, REAL, SPOOL
        ));
    }

    #[test]
    fn setfsuid_answers_with_the_id_that_was_in_force_not_the_new_one() {
        // `old_fsuid` is read before anything is decided and is what every
        // return path hands back; a program keeps it to put the id back.
        let proc = process_as(REAL, ROOT_UID, ROOT_UID);
        assert_eq!(proc.set_fsuid(REAL), ROOT_UID);
        assert_eq!(proc.fsuid(), REAL);
        assert_eq!(proc.set_fsuid(ROOT_UID), REAL, "and again on the way back");
        assert_eq!(proc.fsuid(), ROOT_UID);
    }

    #[test]
    fn dropping_to_another_user_for_file_work_leaves_the_other_ids_alone() {
        // The whole point of the call: still root for everything that is not
        // a file.
        let proc = process_as(REAL, ROOT_UID, ROOT_UID);
        proc.set_fsuid(REAL);
        assert_eq!(proc.fsuid(), REAL);
        assert_eq!(proc.euid(), ROOT_UID);
        assert_eq!(proc.uid(), REAL);
        assert_eq!(proc.suid(), ROOT_UID);
    }

    #[test]
    fn minus_one_is_the_query_the_man_page_tells_you_to_make() {
        // `if (!uid_valid(kuid)) return old_fsuid;`. Since the call cannot
        // report an error, `setfsuid(-1)` is how a careful program finds out
        // whether the previous one took.
        let proc = process_as(REAL, ROOT_UID, ROOT_UID);
        proc.set_fsuid(REAL);
        assert_eq!(proc.set_fsuid(NO_ID), REAL);
        assert_eq!(proc.fsuid(), REAL, "and it changed nothing");
    }

    #[test]
    fn an_id_the_caller_never_held_leaves_the_filesystem_id_where_it_was() {
        let proc = process_as(REAL, REAL, REAL);
        assert_eq!(proc.set_fsuid(SPOOL), REAL);
        assert_eq!(
            proc.fsuid(),
            REAL,
            "refused, and the only sign of it is that the id did not move"
        );
    }

    #[test]
    fn root_may_move_it_anywhere_because_of_cap_setuid() {
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        assert_eq!(proc.set_fsuid(SPOOL), ROOT_UID);
        assert_eq!(proc.fsuid(), SPOOL);
    }

    #[test]
    fn the_group_half_runs_the_same_rule_with_cap_setgid() {
        let privileged = process_as(REAL, ROOT_UID, ROOT_UID);
        assert_eq!(privileged.set_fsgid(SPOOL), ROOT_UID);
        assert_eq!(privileged.fsgid(), SPOOL, "root, so the capability decides");

        let user = process_as(REAL, REAL, SAVED);
        assert_eq!(user.set_fsgid(SPOOL), REAL);
        assert_eq!(user.fsgid(), REAL, "no capability, and not one of its ids");
        assert_eq!(user.set_fsgid(SAVED), REAL);
        assert_eq!(user.fsgid(), SAVED, "the saved gid is in the set");
    }

    #[test]
    fn a_move_taints_the_process_and_a_refusal_does_not() {
        // `commit_creds()` runs the same dumpability check on `fsuid` as on
        // the other ids, and `abort_creds()` never reaches it.
        let moved = process_as(REAL, ROOT_UID, ROOT_UID);
        moved.set_fsuid(REAL);
        assert!(moved.is_sugid());

        let refused = process_as(REAL, REAL, REAL);
        refused.set_fsuid(SPOOL);
        assert!(!refused.is_sugid());

        let noop = process_as(REAL, REAL, REAL);
        noop.set_fsuid(REAL);
        assert!(
            !noop.is_sugid(),
            "asking for the id already in force builds no credentials at all"
        );
    }

    // ---- who drags it along ------------------------------------------
    //
    // Every `set*id` path in `kernel/sys.c` ends `new->fsuid = new->euid;`.
    // A path that forgets it leaves a process that dropped to `nobody` still
    // reading files as root: the same hole from the other side.

    #[test]
    fn setuid_drags_the_filesystem_id_along() {
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsuid(SPOOL);
        proc.set_uid(REAL).unwrap();
        assert_eq!(proc.fsuid(), REAL);
    }

    #[test]
    fn setreuid_drags_it_too() {
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsuid(SPOOL);
        proc.set_reuid(NO_ID, REAL).unwrap();
        assert_eq!(proc.fsuid(), REAL);
    }

    #[test]
    fn setresuid_drags_it_too() {
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsuid(SPOOL);
        proc.set_resuid(NO_ID, REAL, NO_ID).unwrap();
        assert_eq!(proc.fsuid(), REAL);
    }

    #[test]
    fn setgid_drags_the_filesystem_group_along() {
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsgid(SPOOL);
        proc.set_gid(REAL).unwrap();
        assert_eq!(proc.fsgid(), REAL);
    }

    #[test]
    fn setregid_drags_it_too() {
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsgid(SPOOL);
        proc.set_regid(NO_ID, REAL).unwrap();
        assert_eq!(proc.fsgid(), REAL);
    }

    #[test]
    fn setresgid_drags_it_too() {
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsgid(SPOOL);
        proc.set_resgid(NO_ID, REAL, NO_ID).unwrap();
        assert_eq!(proc.fsgid(), REAL);
    }

    #[test]
    fn a_switch_that_names_no_effective_id_still_brings_the_pair_in_step() {
        // `setreuid(ruid, -1)` moves the REAL uid and nothing else, and
        // `__sys_setreuid` still ends `new->fsuid = new->euid;`. So the line
        // belongs at the end of the path and not beside the write to `euid`:
        // a filesystem id that had wandered comes home even though no
        // effective id was named.
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsuid(SPOOL);
        proc.set_reuid(REAL, NO_ID).unwrap();
        assert_eq!(proc.euid(), ROOT_UID, "the effective id did not move");
        assert_eq!(proc.fsuid(), ROOT_UID, "and the filesystem id came back");
    }

    #[test]
    fn an_exec_puts_the_filesystem_id_back_on_the_effective_one() {
        // `cap_bprm_creds_from_file()`: `new->suid = new->fsuid = new->euid;`
        // `new->sgid = new->fsgid = new->egid;`. An image inherits the id its
        // accesses are checked against; it does not inherit a stray one the
        // caller happened to be wearing.
        let mut inner = LinuxProcessInner::default();
        inner.credentials = creds(REAL, REAL, REAL);
        inner.credentials.fsuid = SPOOL;
        inner.credentials.fsgid = SPOOL;
        inner.apply_exec_ids_from_a_normal_mount(0o755, OTHER, OTHER);
        assert_eq!(inner.credentials.fsuid, REAL);
        assert_eq!(inner.credentials.fsgid, REAL);
    }

    #[test]
    fn a_setuid_image_hands_its_own_id_to_the_filesystem_too() {
        let mut inner = LinuxProcessInner::default();
        inner.credentials = creds(REAL, REAL, REAL);
        inner.apply_exec_ids_from_a_normal_mount(0o4755, ROOT_UID, OTHER);
        assert_eq!(inner.credentials.euid, ROOT_UID);
        assert_eq!(inner.credentials.fsuid, ROOT_UID);
    }

    // ---- what asks it ------------------------------------------------

    #[test]
    fn a_files_permission_bits_are_weighed_against_the_filesystem_id() {
        // Root that has dropped its filesystem id gets the OTHER bits of a
        // file it does not own, exactly like the user it stands in for.
        // Before this change the same process was still root here: a server
        // asked to read `/root/.ssh/id_rsa` on behalf of user 1000 read it.
        let mut root = creds(ROOT_UID, ROOT_UID, ROOT_UID);
        root.fsuid = REAL;
        root.fsgid = REAL;
        assert_eq!(
            LinuxProcess::access_verdict(&root, OTHER, OTHER, 0o600, false, ACCESS_WRITE, true),
            Err(LxError::EACCES)
        );
        assert_eq!(
            LinuxProcess::access_verdict(
                &creds(ROOT_UID, ROOT_UID, ROOT_UID),
                OTHER,
                OTHER,
                0o600,
                false,
                ACCESS_WRITE,
                true
            ),
            Ok(()),
            "and with the id left alone it is still root"
        );
    }

    #[test]
    fn the_owner_bits_go_to_whoever_the_filesystem_id_names() {
        let mut c = creds(REAL, REAL, REAL);
        c.fsuid = OTHER;
        assert_eq!(
            LinuxProcess::access_verdict(&c, OTHER, SPOOL, 0o600, false, ACCESS_WRITE, true),
            Ok(()),
            "the file's owner is the id being acted as"
        );
    }

    #[test]
    fn the_group_bits_follow_the_filesystem_group() {
        let mut c = creds(REAL, REAL, REAL);
        c.fsgid = SPOOL;
        assert_eq!(
            LinuxProcess::access_verdict(&c, OTHER, SPOOL, 0o060, false, ACCESS_WRITE, true),
            Ok(())
        );
        c.fsgid = REAL;
        assert_eq!(
            LinuxProcess::access_verdict(&c, OTHER, SPOOL, 0o060, false, ACCESS_WRITE, true),
            Err(LxError::EACCES)
        );
    }

    #[test]
    fn access_2_still_asks_the_real_ids_and_not_the_filesystem_ones() {
        // `access(2)` is the one caller that deliberately asks as the real
        // user; a wandering filesystem id must not answer for it.
        let mut c = creds(REAL, ROOT_UID, ROOT_UID);
        c.fsuid = OTHER;
        c.fsgid = OTHER;
        assert_eq!(
            LinuxProcess::access_verdict(&c, OTHER, OTHER, 0o600, false, ACCESS_WRITE, false),
            Err(LxError::EACCES),
            "the real uid owns nothing here"
        );
    }

    #[test]
    fn a_chmod_is_refused_to_a_filesystem_id_that_does_not_own_the_file() {
        // `inode_owner_or_capable()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
        let mut root = creds(ROOT_UID, ROOT_UID, ROOT_UID);
        root.fsuid = REAL;
        assert_eq!(
            LinuxProcess::chmod_bits(&root, OTHER, OTHER, 0o644, 0o600),
            Err(LxError::EPERM)
        );
        root.fsuid = OTHER;
        assert_eq!(
            LinuxProcess::chmod_bits(&root, OTHER, OTHER, 0o644, 0o600),
            Ok(0o600),
            "and allowed once the acting id is the owner"
        );
    }

    #[test]
    fn a_new_file_belongs_to_the_id_its_creator_was_acting_as() {
        // `inode_init_owner()`: `inode_fsuid_set()` / `inode_fsgid_set()`. A
        // server that creates a file on a user's behalf must leave it owned
        // by the user, not by the server.
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsuid(REAL);
        proc.set_fsgid(SPOOL);
        let inode: Arc<dyn INode> = Owned::new();
        proc.initialize_created_metadata(&inode, None, 0o644, false)
            .unwrap();
        let meta = inode.metadata().unwrap();
        assert_eq!(meta.uid as u32, REAL);
        assert_eq!(meta.gid as u32, SPOOL);
    }

    #[test]
    fn a_chown_is_weighed_against_the_filesystem_id() {
        // `chown_ok()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsuid(REAL);
        let mut meta = a_metadata(0o644, OTHER, OTHER);
        assert_eq!(
            proc.chown_metadata(&mut meta, NO_ID, SPOOL),
            Err(LxError::EPERM),
            "not root here, and not the owner either"
        );
    }

    #[test]
    fn the_sticky_bit_asks_the_filesystem_id_too() {
        // `__check_sticky()` opens with `kuid_t fsuid = current_fsuid();`.
        let dir = a_metadata(0o1777, OTHER, OTHER);
        let victim = a_metadata(0o644, OTHER, OTHER);
        let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
        proc.set_fsuid(REAL);
        assert_eq!(proc.check_sticky(&dir, &victim), Err(LxError::EPERM));
        proc.set_fsuid(ROOT_UID);
        assert_eq!(proc.check_sticky(&dir, &victim), Ok(()));
    }

    #[test]
    fn the_pair_cannot_be_moved_one_at_a_time_by_hand() {
        // `Credentials::set_euid` is the only line in this kernel that writes
        // a filesystem id from an effective one, so a caller holding bare
        // credentials cannot separate them by accident.
        let mut c = creds(REAL, REAL, REAL);
        c.set_euid(OTHER);
        assert_eq!(c.fsuid, OTHER);
        c.set_egid(SPOOL);
        assert_eq!(c.fsgid, SPOOL);
    }
}

#[cfg(test)]
mod dac_tests {
    //! The discretionary-access decisions of `LinuxProcess`, on pure inputs:
    //! who gets which permission bits, what a `chmod` really lands, and which
    //! of a caller's three ids an unprivileged id switch may name. Each test
    //! cites the Linux rule it pins (`fs/namei.c`, `fs/attr.c`, `kernel/sys.c`).

    use super::*;

    const OWNER: u32 = 1000;
    const GROUP: u32 = 100;
    const OTHER_GROUP: u32 = 200;

    /// An ordinary user: every id the same, no supplementary groups.
    fn user(uid: u32, gid: u32) -> Credentials {
        Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsuid: uid,
            fsgid: gid,
            groups: Vec::new(),
            umask: 0o022,
        }
    }

    fn may(creds: &Credentials, mode: u16, requested: u16, use_effective: bool) -> bool {
        LinuxProcess::access_verdict(creds, OWNER, GROUP, mode, false, requested, use_effective)
            .is_ok()
    }

    fn may_dir(creds: &Credentials, mode: u16, requested: u16) -> bool {
        LinuxProcess::access_verdict(creds, OWNER, GROUP, mode, true, requested, true).is_ok()
    }

    /// `acl_permission_check`: "Are we the owner? If so, ACL's don't matter"
    /// -- and neither do the group or other bits. `0o077` gives the owner
    /// nothing even though everybody else can do everything.
    #[test]
    fn the_owner_arm_is_exclusive() {
        let owner = user(OWNER, GROUP);
        assert!(
            !may(&owner, 0o077, 0o4, true),
            "owner denied read by the owner bits"
        );
        assert!(!may(&owner, 0o077, 0o2, true));
        assert!(!may(&owner, 0o077, 0o1, true));
        let stranger = user(4242, 4242);
        assert!(
            may(&stranger, 0o077, 0o7, true),
            "other bits still apply to others"
        );
    }

    /// `in_group_p()` consults the ACTING gid and the supplementary list. A
    /// process that dropped its effective gid must lose the group bits with
    /// it: keeping them is keeping the very access the drop gave up.
    #[test]
    fn a_dropped_effective_gid_loses_group_access() {
        // rgid is still the file's group, egid is not.
        let mut dropped = user(4242, GROUP);
        dropped.set_egid(OTHER_GROUP);
        dropped.sgid = OTHER_GROUP;

        assert!(
            !may(&dropped, 0o060, 0o4, true),
            "the effective path must not read group bits through the REAL gid"
        );
        // `access(2)` asks about the real ids, and there the real gid counts.
        assert!(
            may(&dropped, 0o060, 0o4, false),
            "the real path is the one place the real gid belongs"
        );
    }

    /// The supplementary list counts on both paths, as `in_group_p` walks it
    /// regardless of which primary gid is being asked about.
    #[test]
    fn a_supplementary_group_counts_on_both_paths() {
        let mut member = user(4242, OTHER_GROUP);
        member.groups = vec![7, GROUP, 9];
        assert!(may(&member, 0o060, 0o6, true));
        assert!(may(&member, 0o060, 0o6, false));
        let outsider = user(4242, OTHER_GROUP);
        assert!(!may(&outsider, 0o060, 0o4, true));
        assert!(
            may(&outsider, 0o004, 0o4, true),
            "falls through to the other bits"
        );
    }

    /// `generic_permission`: read/write DACs are always overridable by
    /// CAP_DAC_OVERRIDE; executing a file needs at least one x bit somewhere;
    /// a directory is always searchable.
    #[test]
    fn root_reads_and_writes_anything_but_executes_only_what_has_an_x_bit() {
        let root = user(ROOT_UID, ROOT_UID);
        assert!(may(&root, 0o000, 0o6, true));
        assert!(
            !may(&root, 0o000, 0o1, true),
            "no x bit anywhere: even root may not exec"
        );
        assert!(
            may(&root, 0o001, 0o1, true),
            "one x bit, any column, is enough"
        );
        assert!(
            may_dir(&root, 0o000, 0o1),
            "a 0700 (or 0000) directory is searchable by root"
        );
        // The override follows the SELECTED uid: a setuid-root program asked
        // about its real ids is not root for `access(2)`.
        let mut setuid_root = user(OWNER, GROUP);
        setuid_root.set_euid(ROOT_UID);
        assert!(may(&setuid_root, 0o000, 0o2, true));
        assert!(!may(&setuid_root, 0o000, 0o2, false));
    }

    /// `mask & ~mode` with an empty mask is zero: `F_OK` is existence only.
    #[test]
    fn nothing_requested_is_always_granted() {
        let stranger = user(4242, 4242);
        assert!(may(&stranger, 0o000, 0, true));
        assert!(may(&stranger, 0o000, 0, false));
    }

    // ---- chmod -------------------------------------------------------------

    fn chmod(creds: &Credentials, cur: u16, mode: u16) -> LxResult<u16> {
        LinuxProcess::chmod_bits(creds, OWNER, GROUP, cur, mode)
    }

    /// `setattr_prepare` never touches `S_ISUID`: this is how a user makes a
    /// setuid binary of a file they own. It used to be stripped from every
    /// non-root chmod, and the call still returned success, so `chmod 4755`
    /// silently left `0755` behind.
    #[test]
    fn the_owner_keeps_setuid_on_chmod() {
        let owner = user(OWNER, GROUP);
        assert_eq!(chmod(&owner, 0o100_644, 0o4755), Ok(0o104_755));
        // File-type bits above the permission mask are never the caller's to
        // change.
        assert_eq!(chmod(&owner, 0o100_644, 0o7777), Ok(0o107_777));
    }

    /// "Normal users cannot set the setgid bit if they are not in the group"
    /// -- and that is the only bit `setattr_prepare` strips, and only then.
    #[test]
    fn setgid_is_stripped_only_from_a_caller_outside_the_file_group() {
        let owner_in_group = user(OWNER, GROUP);
        assert_eq!(chmod(&owner_in_group, 0o644, 0o2755), Ok(0o2755));

        let mut owner_outside = user(OWNER, OTHER_GROUP);
        assert_eq!(
            chmod(&owner_outside, 0o644, 0o6755),
            Ok(0o4755),
            "setgid stripped, setuid kept"
        );
        // A supplementary membership is membership.
        owner_outside.groups = vec![GROUP];
        assert_eq!(chmod(&owner_outside, 0o644, 0o2755), Ok(0o2755));
        // And, per `in_group_p`, the REAL gid is not.
        let mut owner_real_only = user(OWNER, GROUP);
        owner_real_only.set_egid(OTHER_GROUP);
        owner_real_only.sgid = OTHER_GROUP;
        assert_eq!(chmod(&owner_real_only, 0o644, 0o2755), Ok(0o755));
    }

    /// `inode_owner_or_capable`: the owner or root, nobody else; and root is
    /// exempt from the setgid rule.
    #[test]
    fn a_stranger_cannot_chmod_and_root_keeps_every_bit() {
        let stranger = user(4242, GROUP);
        assert_eq!(chmod(&stranger, 0o644, 0o600), Err(LxError::EPERM));
        let root = user(ROOT_UID, ROOT_UID);
        assert_eq!(chmod(&root, 0o644, 0o6755), Ok(0o6755));
    }

    // ---- which id an unprivileged switch may name --------------------------

    /// `sys_setuid`: `!uid_eq(kuid, old->uid) && !uid_eq(kuid, new->suid)`
    /// -> EPERM. The effective id is not in the set: with (r=1000, e=2000,
    /// s=3000), `setuid(2000)` is refused by Linux.
    #[test]
    fn setuid_draws_from_real_and_saved_not_effective() {
        assert!(LinuxProcess::setid_allowed(1000, 3000, 1000));
        assert!(LinuxProcess::setid_allowed(1000, 3000, 3000));
        assert!(!LinuxProcess::setid_allowed(1000, 3000, 2000));
        assert!(!LinuxProcess::setid_allowed(1000, 3000, ROOT_UID));
    }

    /// `sys_setreuid`: the real argument may be the old real or effective id,
    /// never the saved one. With (r=1000, e=1000, s=0) -- a daemon that
    /// dropped root and kept it in the saved slot -- `setreuid(0, -1)` is
    /// EPERM on Linux; letting it through moved the saved id into the REAL
    /// slot, where every later rule accepts it. The effective argument, and
    /// every `setresuid` argument, may name any of the three.
    #[test]
    fn the_real_argument_of_setreuid_never_takes_the_saved_id() {
        assert!(!LinuxProcess::set_real_allowed(1000, 1000, ROOT_UID));
        assert!(LinuxProcess::set_real_allowed(1000, 2000, 2000));
        assert!(LinuxProcess::set_real_allowed(1000, 2000, 1000));
        assert!(LinuxProcess::set_any_allowed(
            1000, 1000, ROOT_UID, ROOT_UID
        ));
        assert!(!LinuxProcess::set_any_allowed(1000, 2000, 3000, 4000));
    }

    /// `if (ruid != -1 || (euid != -1 && !uid_eq(keuid, old->uid))) new->suid
    /// = new->euid;` -- there is no privileged term, so a `setreuid(-1, -1)`
    /// that asks for nothing leaves the saved id alone, root or not.
    #[test]
    fn setreuid_with_nothing_to_do_leaves_the_saved_id_alone() {
        assert!(!LinuxProcess::setreid_updates_saved(NO_ID, NO_ID, 1000));
        assert!(
            LinuxProcess::setreid_updates_saved(1000, NO_ID, 1000),
            "a real id is set"
        );
        assert!(
            LinuxProcess::setreid_updates_saved(NO_ID, 2000, 1000),
            "an effective id other than the old real one is set"
        );
        assert!(
            !LinuxProcess::setreid_updates_saved(NO_ID, 1000, 1000),
            "setting the effective id back to the real one is not a new identity"
        );
    }
}

#[cfg(test)]
mod dup_fd_tests {
    //! What a second descriptor for an already-open file shares with the
    //! first, and what it does not.
    //!
    //! `dup(2)`, `dup2(2)`, `dup3(2)`, `fcntl(F_DUPFD)` and `pidfd_getfd(2)`
    //! all install the SAME open file description under another descriptor --
    //! `fs/file.c` does `get_file(file)` and stores that one pointer -- so the
    //! two descriptors share the file offset and the status flags. The only
    //! thing the new descriptor owns is `FD_CLOEXEC`, which lives in the fd
    //! table and not in the description.
    //!
    //! This kernel had it the other way round: every `FileLike` carried a
    //! hand-written `dup` that built a NEW object, and the syscalls wrote the
    //! close-on-exec flag back onto it. A shell's `prog >log 2>&1` therefore
    //! gave stdout and stderr an offset each, both starting at zero, and the
    //! second stream wrote over the first from the beginning of the file.

    use super::*;
    use crate::fs::SeekFrom;
    use rcore_fs::vfs::{FsError, PollStatus, Timespec};

    /// An inode that remembers where each write landed, so a test can see the
    /// offset a second descriptor actually used.
    pub(super) struct Log {
        writes: Mutex<Vec<(usize, usize)>>,
    }

    impl Log {
        pub(super) fn new() -> Arc<Self> {
            Arc::new(Log {
                writes: Mutex::new(Vec::new()),
            })
        }
        fn offsets(&self) -> Vec<usize> {
            self.writes.lock().iter().map(|(off, _)| *off).collect()
        }
        fn end(&self) -> usize {
            self.writes
                .lock()
                .iter()
                .map(|(off, len)| off + len)
                .max()
                .unwrap_or(0)
        }
    }

    impl INode for Log {
        fn read_at(&self, _: usize, _: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::NotSupported)
        }
        fn write_at(&self, offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
            self.writes.lock().push((offset, buf.len()));
            Ok(buf.len())
        }
        fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
            Err(FsError::NotSupported)
        }
        fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
            Ok(Metadata {
                dev: 1,
                inode: 11,
                size: self.end(),
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
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    pub(super) fn a_process() -> LinuxProcess {
        LinuxProcess {
            root_inode: Log::new(),
            parent: Mutex::new(Weak::default()),
            vt: 0,
            perf: crate::perf::ProcPerf::new(),
            itimers: Default::default(),
            aspace_lock: Mutex::new(()),
            inner: Mutex::new(LinuxProcessInner::default()),
        }
    }

    /// `sh -c 'prog >log'`: one open file description, opened for writing.
    pub(super) fn an_open_log(inode: Arc<Log>, flags: OpenFlags) -> Arc<dyn FileLike> {
        File::new(inode, flags, String::from("/var/log/prog.log"))
    }

    fn file_at(proc: &LinuxProcess, fd: FileDesc) -> Arc<File> {
        proc.get_file_like(fd)
            .unwrap()
            .downcast_arc::<File>()
            .ok()
            .unwrap()
    }

    /// The `2>&1` bug, at the fd table: `dup2` must not give the second
    /// descriptor an offset of its own. With a copy, both start at zero and
    /// the second stream writes over the first from the top of the file.
    #[test]
    fn two_descriptors_for_one_file_share_the_offset() {
        let proc = a_process();
        let log = Log::new();
        let stdout = proc
            .add_file(an_open_log(log.clone(), OpenFlags::WRONLY))
            .unwrap();
        // `dup2(stdout, stderr)`.
        let stderr = FileDesc::from(2);
        proc.replace_file(stderr, proc.get_file_like(stdout).unwrap(), false)
            .unwrap();

        proc.get_file_like(stdout)
            .unwrap()
            .write(b"hello\n")
            .unwrap();
        proc.get_file_like(stderr)
            .unwrap()
            .write(b"world\n")
            .unwrap();

        assert_eq!(
            log.offsets(),
            vec![0, 6],
            "the second stream must continue the file, not restart it"
        );
    }

    /// The same rule the other way: a seek through one descriptor moves the
    /// other. `dup(2)`: "the two file descriptors ... share file offset and
    /// file status flags".
    #[test]
    fn a_seek_through_one_descriptor_moves_the_other() {
        let proc = a_process();
        let log = Log::new();
        let one = proc.add_file(an_open_log(log, OpenFlags::RDWR)).unwrap();
        let two = proc
            .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
            .unwrap();

        file_at(&proc, one).seek(SeekFrom::Start(4096)).unwrap();
        assert_eq!(
            file_at(&proc, two).seek(SeekFrom::Current(0)).unwrap(),
            4096
        );
    }

    /// And the two descriptors are literally one object, which is the reason
    /// for both of the above.
    #[test]
    fn the_table_hands_out_the_same_description_for_both_descriptors() {
        let proc = a_process();
        let one = proc
            .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
            .unwrap();
        let two = proc
            .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
            .unwrap();
        assert!(Arc::ptr_eq(
            &proc.get_file_like(one).unwrap(),
            &proc.get_file_like(two).unwrap()
        ));
    }

    /// The status flags are part of the description, so `fcntl(F_SETFL)`
    /// through either descriptor is in force on both.
    #[test]
    fn the_status_flags_are_shared() {
        let proc = a_process();
        let one = proc
            .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
            .unwrap();
        let two = proc
            .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
            .unwrap();

        let flags = proc.get_file_like(two).unwrap().flags();
        proc.get_file_like(two)
            .unwrap()
            .set_flags(flags | OpenFlags::NON_BLOCK)
            .unwrap();
        assert!(proc.get_file_like(one).unwrap().flags().non_block());
    }

    /// `FD_CLOEXEC` is the one thing that is NOT shared. An `O_CLOEXEC` file
    /// gives its first descriptor the flag, and a `dup` of it must not carry
    /// the flag over -- POSIX says the copy is created with `FD_CLOEXEC`
    /// clear, which is how `sh` installs an `O_CLOEXEC` fd as stdout and
    /// expects it to survive the exec.
    #[test]
    fn a_dup_starts_close_on_exec_clear_even_from_an_o_cloexec_file() {
        let proc = a_process();
        let one = proc
            .add_file(an_open_log(
                Log::new(),
                OpenFlags::WRONLY | OpenFlags::CLOEXEC,
            ))
            .unwrap();
        assert!(proc.fd_cloexec(one).unwrap(), "opened with O_CLOEXEC");

        let two = proc
            .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
            .unwrap();
        assert!(!proc.fd_cloexec(two).unwrap(), "the dup is not");
        assert!(
            proc.fd_cloexec(one).unwrap(),
            "and the original keeps its own flag"
        );
    }

    /// Per-descriptor in both directions: marking one does not mark the other,
    /// and neither shows up in the shared object's flags.
    #[test]
    fn marking_one_descriptor_close_on_exec_leaves_the_other_alone() {
        let proc = a_process();
        let one = proc
            .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
            .unwrap();
        let two = proc
            .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
            .unwrap();

        proc.set_fd_cloexec(two, true).unwrap();
        assert!(proc.fd_cloexec(two).unwrap());
        assert!(!proc.fd_cloexec(one).unwrap());
        assert!(
            !proc.get_file_like(one).unwrap().flags().close_on_exec(),
            "the description is not where this flag lives"
        );
    }

    /// `dup2(2)` leaves the target descriptor close-on-exec CLEAR, whatever
    /// either side had: the old flag goes with the old entry, and the new
    /// one does not come from the description being installed. Both halves
    /// matter -- a copying `dup` got the first by writing the flag onto its
    /// copy, and the second only by accident.
    #[test]
    fn dup2_leaves_the_target_close_on_exec_clear_whatever_the_two_sides_had() {
        let proc = a_process();
        let marked_target = proc
            .add_file(an_open_log(
                Log::new(),
                OpenFlags::WRONLY | OpenFlags::CLOEXEC,
            ))
            .unwrap();
        assert!(proc.fd_cloexec(marked_target).unwrap());
        let plain_source = proc
            .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
            .unwrap();
        proc.replace_file(
            marked_target,
            proc.get_file_like(plain_source).unwrap(),
            false,
        )
        .unwrap();
        assert!(
            !proc.fd_cloexec(marked_target).unwrap(),
            "the target's own flag went with the entry it replaced"
        );

        // And the other way: an `O_CLOEXEC` description installed over a
        // plain descriptor must not bring its flag along.
        let plain_target = proc
            .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
            .unwrap();
        let marked_source = proc
            .add_file(an_open_log(
                Log::new(),
                OpenFlags::WRONLY | OpenFlags::CLOEXEC,
            ))
            .unwrap();
        proc.replace_file(
            plain_target,
            proc.get_file_like(marked_source).unwrap(),
            false,
        )
        .unwrap();
        assert!(
            !proc.fd_cloexec(plain_target).unwrap(),
            "O_CLOEXEC is not a property of the description a dup2 installs"
        );
        assert!(
            proc.fd_cloexec(marked_source).unwrap(),
            "and the source descriptor keeps its own"
        );
    }

    /// `pidfd_getfd(2)`: "the close-on-exec flag is set on the file
    /// descriptor" -- on the new one, and it must not follow the description
    /// back to the descriptor the target process is still using.
    #[test]
    fn pidfd_getfd_marks_only_the_descriptor_it_creates() {
        let target = a_process();
        let theirs = target
            .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
            .unwrap();
        let caller = a_process();
        let ours = caller
            .add_file_cloexec(target.get_file_like(theirs).unwrap(), true)
            .unwrap();

        assert!(caller.fd_cloexec(ours).unwrap());
        assert!(!target.fd_cloexec(theirs).unwrap());
    }

    /// A descriptor created by OPENING something does take its close-on-exec
    /// from the `O_CLOEXEC` that opened it -- the rule `opened_cloexec`
    /// states, and the reason the dup paths have to say otherwise explicitly.
    #[test]
    fn opening_registers_the_o_cloexec_that_was_asked_for() {
        let proc = a_process();
        let plain = proc
            .add_file(an_open_log(Log::new(), OpenFlags::RDONLY))
            .unwrap();
        let tagged = proc
            .add_file(an_open_log(
                Log::new(),
                OpenFlags::RDONLY | OpenFlags::CLOEXEC,
            ))
            .unwrap();
        assert!(!proc.fd_cloexec(plain).unwrap());
        assert!(proc.fd_cloexec(tagged).unwrap());
    }

    /// The same rule for a descriptor that is not a file: an eventfd's
    /// counter and its status flags both belong to the description, so a dup
    /// of a `EFD_NONBLOCK` eventfd reads the same counter and loses
    /// `O_NONBLOCK` when the other descriptor clears it. Seventeen
    /// hand-written `dup`s got the first half right and the second wrong.
    #[test]
    fn a_dup_of_an_eventfd_shares_the_counter_and_the_flags() {
        use crate::fs::EventFd;
        let proc = a_process();
        let one = proc
            .add_file(EventFd::new(0, OpenFlags::NON_BLOCK))
            .unwrap();
        let two = proc
            .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
            .unwrap();

        proc.get_file_like(one)
            .unwrap()
            .write(&7u64.to_ne_bytes())
            .unwrap();
        let mut buf = [0u8; 8];
        let n = async_std::task::block_on(proc.get_file_like(two).unwrap().read(&mut buf)).unwrap();
        assert_eq!((n, u64::from_ne_bytes(buf)), (8, 7), "one counter");

        proc.get_file_like(two)
            .unwrap()
            .set_flags(OpenFlags::empty())
            .unwrap();
        assert!(
            !proc.get_file_like(one).unwrap().flags().non_block(),
            "one set of status flags"
        );
    }

    /// And the whole point of the flag: the exec sweep closes the marked
    /// descriptor and leaves its twin, even though both name one description.
    #[test]
    fn the_exec_sweep_closes_the_marked_descriptor_and_keeps_its_twin() {
        let proc = a_process();
        let kept = proc
            .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
            .unwrap();
        let swept = proc
            .add_file_cloexec(proc.get_file_like(kept).unwrap(), true)
            .unwrap();

        proc.remove_cloexec_files();
        assert!(proc.get_file_like(kept).is_ok());
        assert_eq!(proc.get_file_like(swept).err(), Some(LxError::EBADF));
    }
}

#[cfg(test)]
mod job_control_membership_tests {
    //! Who may move whom between process groups and sessions.
    //!
    //! A process group is the unit a terminal signals: [`send_signal_to_pgrp`]
    //! walks every live process whose effective pgid matches the foreground
    //! group and delivers there, which is how one Ctrl-C reaches a pipeline.
    //! Membership of that list was writable by anybody: `set_process_pgid`
    //! said so in its own doc comment ("Permissive (no session/leader
    //! checks)"), took no caller at all, and set the field. `kernel/sys.c`'s
    //! `do_setpgid` has four rules and `ksys_setsid` a fifth; these are them.

    use super::*;

    const SHELL: KoID = 100;
    const CHILD: KoID = 101;
    const STRANGER: KoID = 900;
    const OTHER_SESSION: KoID = 800;

    /// A shell that leads its own session, about to group a child of its own
    /// that has forked but not yet exec'd -- the job-control idiom, which
    /// must keep working.
    fn shell_grouping_its_fresh_child() -> SetpgidFacts {
        SetpgidFacts {
            caller_pid: SHELL,
            caller_sid: SHELL,
            target_pid: CHILD,
            target_sid: SHELL,
            target_is_child: true,
            target_has_execed: false,
            target_is_session_leader: false,
            new_pgid: CHILD,
            group_exists_in_caller_session: false,
        }
    }

    #[test]
    fn a_shell_may_put_its_own_child_in_a_group_of_its_own() {
        assert_eq!(setpgid_verdict(&shell_grouping_its_fresh_child()), Ok(()));
    }

    /// The rule this kernel is missing, and the one that matters: a process
    /// group is a kill list, so filing somebody else's process under your own
    /// group number hands your terminal's Ctrl-C to a process that never
    /// agreed to it. Linux answers ESRCH -- not EPERM -- so the call cannot
    /// double as a probe for which pids exist.
    #[test]
    fn a_stranger_is_not_the_callers_to_move() {
        let mut f = shell_grouping_its_fresh_child();
        f.target_pid = STRANGER;
        f.target_is_child = false;
        f.new_pgid = STRANGER;
        assert_eq!(setpgid_verdict(&f), Err(LxError::ESRCH));
    }

    /// ...including into the caller's OWN group, which is the shape that
    /// actually steals a process: the target then receives every
    /// terminal-generated signal aimed at the caller's job.
    #[test]
    fn a_stranger_cannot_be_dragged_into_the_callers_group() {
        let mut f = shell_grouping_its_fresh_child();
        f.target_pid = STRANGER;
        f.target_is_child = false;
        f.new_pgid = SHELL;
        f.group_exists_in_caller_session = true;
        assert_eq!(setpgid_verdict(&f), Err(LxError::ESRCH));
    }

    /// `if (!(p->flags & PF_FORKNOEXEC)) return -EACCES`. The parent's window
    /// closes at the child's `execve`: after it, the program running in that
    /// child is one the parent did not write.
    #[test]
    fn a_child_that_has_already_execed_is_no_longer_groupable() {
        let mut f = shell_grouping_its_fresh_child();
        f.target_has_execed = true;
        assert_eq!(setpgid_verdict(&f), Err(LxError::EACCES));
    }

    /// That rule is about a CHILD, not about the caller. A shell has exec'd
    /// itself, obviously, and `setpgid(0, 0)` is how it puts itself into a
    /// group -- so reading the flag before asking whose process it is would
    /// break the ordinary self-call.
    #[test]
    fn a_process_that_has_execed_may_still_move_itself() {
        let f = SetpgidFacts {
            caller_pid: CHILD,
            caller_sid: SHELL,
            target_pid: CHILD,
            target_sid: SHELL,
            target_is_child: false,
            target_has_execed: true,
            target_is_session_leader: false,
            new_pgid: CHILD,
            group_exists_in_caller_session: false,
        };
        assert_eq!(setpgid_verdict(&f), Ok(()));
    }

    /// A child that has left for a session of its own (a daemon that called
    /// `setsid`) is out of this shell's reach, even though it is still its
    /// child. EPERM, and it is checked BEFORE the exec rule: a child that is
    /// both gone and exec'd answers for the session, which is the reason it
    /// can never come back.
    #[test]
    fn a_child_in_another_session_is_out_of_reach() {
        let mut f = shell_grouping_its_fresh_child();
        f.target_sid = OTHER_SESSION;
        assert_eq!(setpgid_verdict(&f), Err(LxError::EPERM));
        f.target_has_execed = true;
        assert_eq!(
            setpgid_verdict(&f),
            Err(LxError::EPERM),
            "the session rule is the one that answers"
        );
    }

    /// A session leader's pid IS its session id. Letting it wander into
    /// another group would leave the session named after a group its leader
    /// is not in -- and `getsid` and `getpgid` would stop agreeing about it.
    #[test]
    fn a_session_leader_is_pinned_to_its_own_group() {
        let f = SetpgidFacts {
            caller_pid: SHELL,
            caller_sid: SHELL,
            target_pid: SHELL,
            target_sid: SHELL,
            target_is_child: false,
            target_has_execed: true,
            target_is_session_leader: true,
            new_pgid: SHELL,
            group_exists_in_caller_session: true,
        };
        assert_eq!(setpgid_verdict(&f), Err(LxError::EPERM));
    }

    /// Joining an EXISTING group means the group has to exist, and in the
    /// caller's session. This is what stops a second job from being filed
    /// under a number that belongs to another terminal's pipeline.
    #[test]
    fn joining_a_group_that_exists_nowhere_is_refused() {
        let mut f = shell_grouping_its_fresh_child();
        f.new_pgid = 555;
        f.group_exists_in_caller_session = false;
        assert_eq!(setpgid_verdict(&f), Err(LxError::EPERM));
    }

    #[test]
    fn joining_a_group_of_the_callers_own_session_is_allowed() {
        let mut f = shell_grouping_its_fresh_child();
        f.new_pgid = 555;
        f.group_exists_in_caller_session = true;
        assert_eq!(setpgid_verdict(&f), Ok(()));
    }

    /// Creating one is always allowed, precisely because `pgid == pid` cannot
    /// collide with a group that is already there: the pid is the target's
    /// own. This is the second half of the same rule and the first half is
    /// useless without it -- a shell's first job would have nowhere to go.
    #[test]
    fn a_group_named_after_the_target_needs_no_group_to_exist() {
        let mut f = shell_grouping_its_fresh_child();
        f.new_pgid = f.target_pid;
        f.group_exists_in_caller_session = false;
        assert_eq!(setpgid_verdict(&f), Ok(()));
    }

    /// Order, again: a stranger with an impossible group answers for WHOSE
    /// process it is, not for the group. Otherwise the error tells an
    /// unrelated caller whether that group exists in its session.
    #[test]
    fn whose_process_it_is_is_answered_before_which_group() {
        let mut f = shell_grouping_its_fresh_child();
        f.target_pid = STRANGER;
        f.target_is_child = false;
        f.new_pgid = 555;
        f.group_exists_in_caller_session = false;
        assert_eq!(setpgid_verdict(&f), Err(LxError::ESRCH));
    }

    /// `ksys_setsid`: refused while the caller's pid already names a group,
    /// because the new session would claim that same number for its group.
    /// The everyday case is the caller's own group, which is exactly why the
    /// daemonize idiom `fork`s before calling `setsid`.
    #[test]
    fn a_group_leader_may_not_start_a_session() {
        assert_eq!(
            setsid_verdict(SHELL, &[SHELL, CHILD]),
            Err(LxError::EPERM),
            "the caller's own group carries its pid"
        );
    }

    #[test]
    fn a_process_that_leads_no_group_may() {
        assert_eq!(setsid_verdict(CHILD, &[SHELL, SHELL]), Ok(()));
    }

    /// And the case the old check could not see: it read only the CALLER's
    /// own pgid, so a caller that had moved itself elsewhere while a child of
    /// its own still carried its pid as a group number passed -- and the new
    /// session's group would then have had two unrelated members.
    #[test]
    fn a_pid_that_names_a_group_somebody_else_is_in_counts_too() {
        // The caller sits in group 555; only its child still carries SHELL.
        assert_eq!(setsid_verdict(SHELL, &[555, SHELL]), Err(LxError::EPERM));
    }
}

#[cfg(test)]
mod signal_send_effect_tests {
    //! What a signal does at the moment it is SENT, before anybody receives
    //! it.
    //!
    //! `prepare_signal()` does this work in the sender's context because the
    //! signals it covers exist to act on a process that is not running. Here
    //! there was nothing: every bit of it was left to the target thread's own
    //! `handle_signal` loop in `loader/src/linux.rs`. A job-control-stopped
    //! process's threads are parked in `wait_while_job_stopped`, waiting for
    //! a zircon signal that only `job_continue` raises -- and `job_continue`
    //! was called from that same loop. So a stopped process could not be
    //! resumed by `SIGCONT` and could not be killed by `SIGKILL`: both waited
    //! for the one thread that was waiting for them.

    use super::*;

    #[test]
    fn a_continue_resumes_at_the_moment_it_is_sent() {
        assert_eq!(send_effect(LinuxSignal::SIGCONT), SendEffect::Resume);
    }

    /// `kill -9` on a Ctrl-Z'd process. The default action for SIGKILL runs
    /// in the target's own loop, so the target has to be out of the park
    /// before it can die -- otherwise the pid stays stopped forever and no
    /// signal can ever remove it.
    #[test]
    fn a_kill_wakes_a_stopped_process_so_it_can_die() {
        assert_eq!(send_effect(LinuxSignal::SIGKILL), SendEffect::WakeToDie);
    }

    #[test]
    fn the_four_stop_signals_are_the_four() {
        for sig in STOP_SIGNALS {
            assert_eq!(send_effect(sig), SendEffect::Stop, "{:?}", sig);
        }
        assert_eq!(
            STOP_SIGNALS,
            [
                LinuxSignal::SIGSTOP,
                LinuxSignal::SIGTSTP,
                LinuxSignal::SIGTTIN,
                LinuxSignal::SIGTTOU
            ]
        );
    }

    /// Everything else is an ordinary signal: it waits to be received. A
    /// SIGTERM must NOT resume a stopped process, or a `kill` of a stopped
    /// job would restart it just long enough to run a handler.
    #[test]
    fn an_ordinary_signal_does_nothing_until_it_is_received() {
        for sig in [
            LinuxSignal::SIGTERM,
            LinuxSignal::SIGINT,
            LinuxSignal::SIGHUP,
            LinuxSignal::SIGCHLD,
            LinuxSignal::SIGUSR1,
            LinuxSignal::SIGWINCH,
        ] {
            assert_eq!(send_effect(sig), SendEffect::None, "{:?}", sig);
        }
    }

    /// A stop and a continue cancel each other where they are waiting, so the
    /// last one sent decides. Carrying both, the outcome depended on which of
    /// them the target's loop happened to dequeue first.
    #[test]
    fn a_continue_cancels_a_stop_that_was_still_waiting() {
        let mut pending = Sigset::default();
        for stop in STOP_SIGNALS {
            pending.insert(stop);
        }
        pending.insert(LinuxSignal::SIGTERM);
        let after = pending_after_send(pending, LinuxSignal::SIGCONT);
        for stop in STOP_SIGNALS {
            assert!(!after.contains(stop), "{:?} survived a SIGCONT", stop);
        }
        assert!(
            after.contains(LinuxSignal::SIGTERM),
            "only the stops are cancelled"
        );
    }

    #[test]
    fn a_stop_cancels_a_continue_that_was_still_waiting() {
        for stop in STOP_SIGNALS {
            let mut pending = Sigset::default();
            pending.insert(LinuxSignal::SIGCONT);
            pending.insert(LinuxSignal::SIGUSR2);
            let after = pending_after_send(pending, stop);
            assert!(
                !after.contains(LinuxSignal::SIGCONT),
                "a pending SIGCONT survived {:?}",
                stop
            );
            assert!(after.contains(LinuxSignal::SIGUSR2));
        }
    }

    #[test]
    fn an_ordinary_signal_cancels_nothing() {
        let mut pending = Sigset::default();
        pending.insert(LinuxSignal::SIGCONT);
        pending.insert(LinuxSignal::SIGTSTP);
        let after = pending_after_send(pending, LinuxSignal::SIGTERM);
        assert!(after.contains(LinuxSignal::SIGCONT));
        assert!(after.contains(LinuxSignal::SIGTSTP));
    }

    /// The wake-to-die is not a continue. A parent in `wait(WCONTINUED)` must
    /// not be told the job resumed at the very moment it was killed.
    #[test]
    fn a_process_woken_to_die_owes_its_parent_no_continue_notification() {
        let mut inner = LinuxProcessInner {
            job_stopped: true,
            job_stop_sig: LinuxSignal::SIGTSTP as u8,
            job_stop_pending: true,
            ..Default::default()
        };
        assert!(inner.leave_stop(false));
        assert!(!inner.job_stopped);
        assert!(!inner.job_continued_pending, "it is not continuing");
        assert!(
            !inner.job_stop_pending,
            "and the stop it never collected is no longer current"
        );
    }

    #[test]
    fn a_real_continue_does_owe_one() {
        let mut inner = LinuxProcessInner {
            job_stopped: true,
            job_stop_pending: true,
            ..Default::default()
        };
        assert!(inner.leave_stop(true));
        assert!(inner.job_continued_pending);
        assert!(!inner.job_stop_pending);
    }

    /// A `SIGCONT` to a process that was not stopped is not a state change,
    /// so it owes nothing either -- `wait(WCONTINUED)` would otherwise return
    /// for a job that never went anywhere.
    #[test]
    fn a_continue_to_a_running_process_is_not_a_state_change() {
        let mut inner = LinuxProcessInner::default();
        assert!(!inner.leave_stop(true));
        assert!(!inner.job_continued_pending);
    }
}

#[cfg(test)]
mod pending_signal_disposition_tests {
    //! What `check_signals` does with a pending signal that does NOT wake the
    //! syscall.
    //!
    //! One predicate was answering two questions. "Does this interrupt the
    //! syscall with EINTR?" and "may this be thrown away?" have the same
    //! answer for most of the enum and a different one for exactly four
    //! signals -- the job-control stops -- which is why the difference went
    //! unnoticed: everything the tests looked at agreed.
    //!
    //! The consequence was not subtle. A thread parked in `ppoll`/
    //! `epoll_wait`/`read` is every idle desktop process, and `check_signals`
    //! runs on each turn of that loop: a `kill -STOP` made the bit pending
    //! and the next turn of the wait removed it. `kill -STOP` on anything
    //! idle did nothing whatsoever, and so did Ctrl-Z on a program blocked in
    //! a read, and so did the SIGTTIN a background job gets for reading the
    //! terminal.
    //!
    //! Both predicates are checked over a disposition rather than a process,
    //! which is what lets the two be laid side by side here.

    use super::*;
    use crate::signal::{SIG_DFL, SIG_IGN};
    use alloc::vec::Vec;
    use core::convert::TryFrom;

    /// Some address that is neither `SIG_DFL` (0) nor `SIG_IGN` (1): a
    /// handler the program installed.
    const CAUGHT: usize = 0x4000_1000;

    fn every_signal() -> Vec<LinuxSignal> {
        (1u8..=64)
            .filter_map(|n| LinuxSignal::try_from(n).ok())
            .collect()
    }

    /// The whole finding, in one assertion: the set of signals that neither
    /// interrupt nor may be discarded is exactly the four stop signals. Empty
    /// before the split, because one predicate answered both questions.
    #[test]
    fn the_two_questions_differ_on_exactly_the_four_stop_signals() {
        let mut neither: Vec<LinuxSignal> = every_signal()
            .into_iter()
            .filter(|&s| !interrupts_syscall(SIG_DFL, s) && !discards_when_pending(SIG_DFL, s))
            .collect();
        neither.sort_unstable_by_key(|s| *s as u8);
        assert_eq!(neither, STOP_SIGNALS.to_vec());
    }

    /// `kill -STOP` on a process sitting in `poll`. The stop itself happens
    /// when the syscall returns, in `handle_signal`; what this fixes is the
    /// bit being gone by then.
    #[test]
    fn a_pending_stop_signal_is_not_a_signal_to_throw_away() {
        for sig in STOP_SIGNALS {
            assert!(
                !discards_when_pending(SIG_DFL, sig),
                "{:?} was discarded while the thread waited",
                sig
            );
        }
    }

    /// And it still does not raise EINTR, which is the half that was right:
    /// Linux restarts the syscall around a stop rather than failing it, and
    /// this kernel has no restart machinery to do that with.
    #[test]
    fn a_stop_signal_still_does_not_wake_the_syscall_with_eintr() {
        for sig in STOP_SIGNALS {
            assert!(!interrupts_syscall(SIG_DFL, sig), "{:?}", sig);
        }
    }

    /// `sig_kernel_ignore()`: these four really are no-ops by default, and
    /// dropping them is what keeps a blocking wait from re-scanning the same
    /// stale bit on every wake. SIGCHLD in particular: returning EINTR for it
    /// is what made a compositor's libinput dispatch fail with "Interrupted
    /// system call" every time an autostart child exited.
    #[test]
    fn the_four_whose_default_is_to_do_nothing_are_still_discarded() {
        for sig in [
            LinuxSignal::SIGCHLD,
            LinuxSignal::SIGURG,
            LinuxSignal::SIGWINCH,
            LinuxSignal::SIGCONT,
        ] {
            assert!(discards_when_pending(SIG_DFL, sig), "{:?}", sig);
            assert!(!interrupts_syscall(SIG_DFL, sig), "{:?}", sig);
        }
    }

    /// A shell that sets SIGTSTP to `SIG_IGN` so it cannot be suspended means
    /// it: an explicitly ignored signal is discardable whatever it is.
    #[test]
    fn a_signal_the_program_ignores_is_discarded_whatever_it_is() {
        for sig in every_signal() {
            assert!(discards_when_pending(SIG_IGN, sig), "{:?}", sig);
            assert!(!interrupts_syscall(SIG_IGN, sig), "{:?}", sig);
        }
    }

    /// A signal with a handler has somewhere to go, so it wakes the syscall
    /// and is never dropped on the way.
    #[test]
    fn a_caught_signal_interrupts_and_is_never_discarded() {
        for sig in every_signal() {
            assert!(interrupts_syscall(CAUGHT, sig), "{:?}", sig);
            assert!(!discards_when_pending(CAUGHT, sig), "{:?}", sig);
        }
    }

    /// SIGKILL never reaches the discard at all: it interrupts first, and
    /// `check_signals` returns EINTR before it gets that far. Its disposition
    /// cannot be changed (`rt_sigaction` refuses), so `SIG_DFL` is the only
    /// case there is.
    #[test]
    fn a_kill_leaves_the_wait_before_anything_can_drop_it() {
        assert!(interrupts_syscall(SIG_DFL, LinuxSignal::SIGKILL));
        assert!(!discards_when_pending(SIG_DFL, LinuxSignal::SIGKILL));
    }

    /// The two lists are near-complements and were treated as exact ones.
    #[test]
    fn the_ignore_list_and_the_interrupt_list_are_not_complements() {
        for sig in every_signal() {
            if signal_default_action_ignores(sig) {
                assert!(!signal_default_action_interrupts(sig), "{:?}", sig);
            }
        }
        // ... and four signals are in neither.
        assert!(!signal_default_action_ignores(LinuxSignal::SIGSTOP));
        assert!(!signal_default_action_interrupts(LinuxSignal::SIGSTOP));
    }
}

#[cfg(test)]
mod exit_status_tests {
    //! The status word a `wait(2)` hands back, which is the ONLY thing a
    //! parent learns about how its child finished.
    //!
    //! `sys/wait.h` packs two different endings into one int and tells them
    //! apart by the low seven bits. This kernel only ever built one of the
    //! two shapes: the default-action kill path stored `128 + signo` -- the
    //! number a SHELL prints, which the shell computes ITSELF from
    //! `WIFSIGNALED` -- and `wait` shifted it up eight bits like any exit
    //! code. So every process the kernel killed was reported as one that had
    //! called `exit(128 + n)`.

    use super::*;

    /// The macros from `sys/wait.h`, spelled as glibc spells them, so the
    /// tests below ask the questions userspace asks.
    fn wifexited(status: i32) -> bool {
        status & 0x7f == 0
    }
    fn wexitstatus(status: i32) -> i32 {
        (status >> 8) & 0xff
    }
    fn wifsignaled(status: i32) -> bool {
        // `((signed char) (((status) & 0x7f) + 1) >> 1) > 0`
        ((((status & 0x7f) + 1) as i8) >> 1) > 0
    }
    fn wtermsig(status: i32) -> i32 {
        status & 0x7f
    }

    #[test]
    fn an_ordinary_exit_is_an_exit_with_its_code() {
        for code in [0i64, 1, 2, 42, 127, 255] {
            let status = wait_status_exited(code);
            assert!(wifexited(status), "exit({})", code);
            assert!(!wifsignaled(status), "exit({})", code);
            assert_eq!(wexitstatus(status), code as i32);
        }
    }

    /// `exit(2)` takes an `int` and the parent sees only its low byte -- which
    /// is why every shell script that ends in `exit(256)` reports success.
    #[test]
    fn only_the_low_byte_of_an_exit_code_reaches_the_parent() {
        assert_eq!(wexitstatus(wait_status_exited(256)), 0);
        assert_eq!(wexitstatus(wait_status_exited(257)), 1);
        // And the whole word, not just what WEXITSTATUS masks back out: a
        // status carrying bits above the second byte is not the status Linux
        // hands over, and userspace is free to compare the int itself
        // (`status == 0` is the idiom for "the command worked").
        assert_eq!(wait_status_exited(256), 0);
        assert_eq!(wait_status_exited(0x1234_5601), 1 << 8);
    }

    /// The shape that did not exist. `system()` and every supervisor use
    /// `WIFSIGNALED` to tell a command that failed from one that was
    /// interrupted, and a shell prints "Killed" off it.
    #[test]
    fn a_death_by_signal_says_so_and_names_the_signal() {
        for sig in [
            LinuxSignal::SIGHUP,
            LinuxSignal::SIGINT,
            LinuxSignal::SIGKILL,
            LinuxSignal::SIGSEGV,
            LinuxSignal::SIGPIPE,
            LinuxSignal::SIGTERM,
        ] {
            let status = wait_status_exited(exit_code_killed_by(sig as u8));
            assert!(wifsignaled(status), "killed by {:?}", sig);
            assert!(!wifexited(status), "killed by {:?}", sig);
            assert_eq!(wtermsig(status), sig as i32);
        }
    }

    /// And the two are distinguishable, which is the whole point: storing
    /// `128 + signo` made a SIGKILL indistinguishable from a program that
    /// really does `exit(137)` -- and both then read as an ordinary exit.
    #[test]
    fn a_program_that_exits_with_137_is_not_a_process_killed_by_sigkill() {
        let exited = wait_status_exited(128 + LinuxSignal::SIGKILL as i64);
        let killed = wait_status_exited(exit_code_killed_by(LinuxSignal::SIGKILL as u8));
        assert_ne!(exited, killed);
        assert!(wifexited(exited) && !wifsignaled(exited));
        assert!(wifsignaled(killed) && !wifexited(killed));
        assert_eq!(wexitstatus(exited), 137);
        assert_eq!(wtermsig(killed), 9);
    }

    /// A stopped child is the third shape, and it must not collide with the
    /// other two: `0x7f` in the low byte is what `WIFSTOPPED` looks for.
    #[test]
    fn a_stop_is_neither_of_the_two() {
        let status = wait_status_stopped(LinuxSignal::SIGTSTP as u8);
        assert!(!wifexited(status));
        assert!(
            !wifsignaled(status),
            "0x7f is the stop marker, not a signal"
        );
        assert_eq!(status & 0xff, 0x7f);
    }
}

#[cfg(test)]
mod sigchld_tests {
    //! A child that exits, stops or resumes pulsed the zircon `SIGCHLD` bit
    //! at its parent, which wakes a blocked `wait*`, and nothing else: no
    //! Linux `SIGCHLD` was ever queued, so a handler, a signalfd or a
    //! `sigwait` on it never ran. `do_notify_parent` and
    //! `do_notify_parent_cldstop` are the two places Linux sends it.

    use super::*;
    use crate::signal::{SignalAction, SignalActionFlags, SignalCode};
    use crate::thread::ThreadExt;
    use rcore_fs_ramfs::RamFS;

    fn a_parent(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "parent",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let thread = Thread::create_linux(&proc).unwrap();
        (proc, thread)
    }

    fn pending_sigchld(thread: &Arc<Thread>) -> bool {
        thread.lock_linux().signals.contains(LinuxSignal::SIGCHLD)
    }

    fn clear_pending(thread: &Arc<Thread>) {
        thread.lock_linux().signals = Sigset::default();
    }

    /// The `siginfo_t` of the pending SIGCHLD as `(si_code, si_pid, si_uid,
    /// si_status)`, read where glibc reads them.
    fn sigchld_info(thread: &Arc<Thread>) -> (SignalCode, i32, i32, i32) {
        let info = thread.lock_linux().take_siginfo(LinuxSignal::SIGCHLD);
        let b = info.as_bytes();
        let word = |at: usize| i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        (info.code, word(16), word(20), word(24))
    }

    #[test]
    fn the_sigchld_says_which_child_whose_and_how_it_ended() {
        let (parent, thread) = a_parent(43_004);
        let child = Process::fork_from(&parent).unwrap();
        child.linux().set_resuid(1000, 1000, 1000).unwrap();
        child.exit(7);
        assert_eq!(
            sigchld_info(&thread),
            (SignalCode::CLD_EXITED, child.id() as i32, 1000, 7)
        );
    }

    #[test]
    fn a_child_killed_by_a_signal_is_reported_as_killed_by_that_signal() {
        let (parent, thread) = a_parent(43_005);
        let child = Process::fork_from(&parent).unwrap();
        child.exit(exit_code_killed_by(LinuxSignal::SIGKILL as u8));
        assert_eq!(
            sigchld_info(&thread),
            (
                SignalCode::CLD_KILLED,
                child.id() as i32,
                0,
                LinuxSignal::SIGKILL as i32
            )
        );
    }

    #[test]
    fn a_stop_and_a_continue_say_so_in_the_sigchld() {
        let (parent, thread) = a_parent(43_006);
        let child = Process::fork_from(&parent).unwrap();
        child.linux().job_stop(&child, LinuxSignal::SIGTSTP as u8);
        assert_eq!(
            sigchld_info(&thread),
            (
                SignalCode::CLD_STOPPED,
                child.id() as i32,
                0,
                LinuxSignal::SIGTSTP as i32
            )
        );
        child.linux().job_continue(&child);
        let (code, pid, _, _) = sigchld_info(&thread);
        assert_eq!((code, pid), (SignalCode::CLD_CONTINUED, child.id() as i32));
    }

    #[test]
    fn a_child_that_exits_sends_its_parent_a_linux_sigchld() {
        let (parent, thread) = a_parent(43_001);
        let child = Process::fork_from(&parent).unwrap();
        assert!(!pending_sigchld(&thread));
        child.exit(0);
        assert!(
            pending_sigchld(&thread),
            "the parent never heard the child die"
        );
    }

    #[test]
    fn a_child_that_stops_and_resumes_sends_sigchld_both_times() {
        let (parent, thread) = a_parent(43_002);
        let child = Process::fork_from(&parent).unwrap();
        child.linux().job_stop(&child, LinuxSignal::SIGSTOP as u8);
        assert!(pending_sigchld(&thread), "no SIGCHLD for the stop");
        clear_pending(&thread);
        assert!(child.linux().job_continue(&child));
        assert!(pending_sigchld(&thread), "no SIGCHLD for the continue");
    }

    #[test]
    fn sa_nocldstop_keeps_stops_quiet_but_not_deaths() {
        let (parent, thread) = a_parent(43_003);
        parent.linux().set_signal_action(
            LinuxSignal::SIGCHLD,
            SignalAction {
                handler: 0x1000,
                flags: SignalActionFlags::NOCLDSTOP,
                restorer: 0,
                mask: Sigset::default(),
            },
        );
        let child = Process::fork_from(&parent).unwrap();
        child.linux().job_stop(&child, LinuxSignal::SIGSTOP as u8);
        assert!(!pending_sigchld(&thread), "SA_NOCLDSTOP was ignored");
        child.linux().job_continue(&child);
        assert!(
            !pending_sigchld(&thread),
            "SA_NOCLDSTOP was ignored on continue"
        );
        child.exit(0);
        assert!(
            pending_sigchld(&thread),
            "a death is never covered by SA_NOCLDSTOP"
        );
    }
}
