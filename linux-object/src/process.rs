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
use core::future::Future;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};
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
/// `CAP_SETPCAP`: drop capabilities from the bounding set
/// (`prctl(PR_CAPBSET_DROP)`), among other things.
pub const CAP_SETPCAP: u32 = 8;
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
/// `CAP_MKNOD`: make a character or block device node with `mknod(2)`.
pub const CAP_MKNOD: u32 = 27;
/// `CAP_SYSLOG`: the `syslog(2)` actions that change the kernel log or the
/// console (clear, read-and-clear, console on/off/level) -- and, under
/// `dmesg_restrict`, reading it at all.
pub const CAP_SYSLOG: u32 = 34;

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
const ACCESS_READ: u16 = 0o4;
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

lazy_static::lazy_static! {
    /// What else dies with a process, registered by the layers above this
    /// crate for the state they keep by pid and this crate cannot name: the
    /// POSIX timers of `timer_create(2)` live in `linux-syscall`. Each hook
    /// runs once per process death, with the dead process's pid, from the
    /// `PROCESS_TERMINATED` callbacks below, after the process's own tables
    /// have been torn down.
    static ref PROCESS_EXIT_HOOKS: Mutex<Vec<fn(KoID)>> = Mutex::new(Vec::new());
}

/// Run `hook(pid)` whenever a process dies. Registration is not deduplicated:
/// a layer that may register more than once keeps its own once-flag.
pub fn register_process_exit_hook(hook: fn(KoID)) {
    PROCESS_EXIT_HOOKS.lock().push(hook);
}

/// How many hooks are registered, for a layer to check that its own once-flag
/// holds.
pub fn process_exit_hook_count() -> usize {
    PROCESS_EXIT_HOOKS.lock().len()
}

/// The death of `pid`, told to every registered hook. The list is copied out
/// first: a hook may take locks of its own, and must not run under this one.
fn run_process_exit_hooks(pid: KoID) {
    let hooks: Vec<fn(KoID)> = PROCESS_EXIT_HOOKS.lock().clone();
    for hook in hooks {
        hook(pid);
    }
}

/// The `ext` fat pointer's DATA word has been overwritten, and a
/// `downcast_ref` just handed out a reference built on it.
///
/// This is the half of the `ext` corruption that the report inside
/// [`ProcessExt::linux`] enumerates (`DATA ONLY: one 8-byte store over the
/// data word, vtable untouched`) and could never actually print. A downcast
/// checks the `TypeId`, which lives in the VTABLE word; with the vtable
/// untouched the downcast SUCCEEDS, so the failure arm never runs. The
/// reference it returns points at whatever was written over the field, and the
/// first field access through it faults there plus an offset -- seen on
/// hardware as
///
/// ```text
/// KERNEL NULL-RANGE PAGE FAULT cpu=6 vaddr=0x10 flags=WRITE
/// rip=... (<lock::ticket::TicketMutex<[linux_object::time::ItimerSlot; 3]>>::lock+0x67)
/// ```
///
/// i.e. a data word of 0 and the `itimers` mutex at offset 0x10 of a
/// `LinuxProcess` that is not there. That fault halts the machine from inside
/// a lock acquisition, naming the mutex and nothing about the process, the
/// writer or the field. Comparing against the birth snapshot before the
/// reference escapes turns it into a report that names all three.
///
/// Nothing here dereferences the ext: every value read belongs to the
/// `Process` itself.
#[cold]
#[inline(never)]
fn ext_data_word_overwritten(whose: &str, proc: &Process) -> ! {
    let (data, vtable) = proc.ext_fat();
    let (born_data, born_vtable) = proc.ext_born();
    panic!(
        "{}: pid={} name={:?} status={:?} -- the ext DATA word was overwritten and the \
         downcast could not see it (the TypeId lives in the vtable word, which is \
         {}). fat data={:#x} vtable={:#x}, at birth data={:#x} vtable={:#x} -> {}; \
         canaries lo={:#x} hi={:#x} -> {}. Refusing to hand out &LinuxProcess at {:#x}: \
         the first field access through it would fault at {:#x}+offset inside whatever \
         lock it touches first, which is a halt that names the lock and not this process. \
         ext is written once in the constructor and never again, so this is a wild write \
         that landed on the field, not a construction path installing the wrong type",
        whose,
        proc.id(),
        trace_name(proc),
        proc.status(),
        if vtable == born_vtable {
            "intact"
        } else {
            "also moved"
        },
        data,
        vtable,
        born_data,
        born_vtable,
        proc.ext_drift().describe(),
        proc.ext_canary_values().0,
        proc.ext_canary_values().1,
        match proc.ext_canaries() {
            (true, true) => "both INTACT: a precise write to ext alone",
            (false, true) => "LOW broken: overrun growing upward from below",
            (true, false) => "HIGH broken: overrun growing downward from above",
            (false, false) => "BOTH broken: wide overrun across the field",
        },
        data,
        data,
    )
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
                    // And so does what the layers above keep by pid (the
                    // POSIX timers); see `register_process_exit_hook`.
                    run_process_exit_hooks(proc.id());
                    // try_linux (not linux): this callback runs from the
                    // object layer on PROCESS_TERMINATED, concurrently with
                    // SMP teardown churn. If the extension can no longer be
                    // resolved, skip the cleanup rather than panic the kernel.
                    if let Some(lp) = proc.try_linux() {
                        // Take the file table out and drop it AFTER the lock is
                        // released — file teardown can re-enter this process's
                        // accessors (see close_file).
                        // The shared-memory table goes the same way: its
                        // drop is the detach of every attachment, which
                        // locks each segment, and `fork` locks the segments
                        // under this process's lock.
                        // The futex table is the third: `clear()` dropped
                        // every `Arc<Futex>` in place, and the last one takes
                        // its waiter queue and its owner `Thread` -- whose
                        // `proc` is a strong `Arc` back to this very process --
                        // with it. It has its own lock now (see
                        // `LinuxProcess::futexes`), so it comes out the same
                        // way and goes with every lock released.
                        let dropped = {
                            let futexes = core::mem::take(&mut *lp.futexes.lock());
                            let mut inner = lp.inner.lock();
                            let files = core::mem::take(&mut inner.files);
                            inner.cloexec_fds.clear();
                            inner.semaphores = Default::default();
                            let shm = core::mem::take(&mut inner.shm_identifiers);
                            (files, shm, futexes)
                        };
                        drop(dropped);
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
        let lp = self
            .ext()
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
            });
        // The downcast vouches for the VTABLE word only -- see
        // `ext_data_word_overwritten`. Ask about the data word too, before the
        // reference escapes and something dereferences it.
        if self.ext_drift().data_moved() {
            ext_data_word_overwritten("Process::linux()", self);
        }
        lp
    }

    fn try_linux(&self) -> Option<&LinuxProcess> {
        let lp = self.ext().downcast_ref::<LinuxProcess>()?;
        // A `try_` that answers "not a Linux process" would be wrong here: the
        // ext IS a LinuxProcess, its address is what moved. Returning `None`
        // would make every caller skip quietly -- the itimer wheel among them,
        // where this was first seen -- and leave the wild write unreported.
        if self.ext_drift().data_moved() {
            ext_data_word_overwritten("Process::try_linux()", self);
        }
        Some(lp)
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
        let linux_parent_inner = linux_parent.inner.lock();
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
            futexes: Default::default(),
            itimers: Default::default(),
            aspace_lock: Mutex::new(()),
            inner: Mutex::new(linux_parent_inner.forked_child(
                parent_pgid,
                parent_sid,
                monotonic_now_ns(),
            )),
        };
        // `inner` goes here, and not at the end of the function. Everything
        // above needed the parent's big lock -- the pgid/sid resolution and
        // the `forked_child` snapshot -- and nothing below it does until the
        // child is inserted into the parent's child list, which takes it again
        // for the three stores that need it.
        //
        // Held to the end of the function, as it used to be, it covered the
        // whole address-space copy. `VmAddressRegion::fork_from` is the one
        // part of this kernel whose own documentation calls itself slow: "the
        // copy of a big process takes tens of ms and the inner locks are
        // IRQ-off spinlocks". `inner` is the parent's big lock, taken by every
        // descriptor lookup -- so every `read`, `write` and `ioctl` of every
        // other thread of the parent queued behind the copy, with interrupts
        // off, for as long as the copy ran. That is the shape of the capture
        // this came from:
        //
        // ```text
        // DEADLOCK: spinlock(s) stuck >8s
        // cpu=11 at linux-object/src/process.rs:2483   (get_file_like)
        // HOLDER cpu=4 at linux-object/src/process.rs:581   (this lock)
        // cpu=0 at linux-object/src/process.rs:2483
        // cpu=2 at linux-object/src/process.rs:2483
        // cpu=6 at linux-object/src/process.rs:2483
        // ```
        //
        // four CPUs stacked on the descriptor lookup behind one fork. And a
        // long hold is only the mild failure: a page fault or a panic taken
        // anywhere inside the copy never releases the lock at all, which turns
        // the fault into a freeze of the whole parent process.
        //
        // The parent's address-space LAYOUT stays frozen across the copy
        // regardless: that is `aspace_lock`'s job, and it is still held.
        drop(linux_parent_inner);
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
        {
            let mut linux_parent_inner = linux_parent.inner.lock();
            // The parent could have exited while the copy ran. Its death
            // already drained `children` (`reparent_live_children_to_init`),
            // so inserting now would strand the child in a dead process's map
            // where no `wait` will ever look. Skipping the insert is the
            // correct outcome and not a loss: the child's own termination
            // callback resolves its reaper with `reaper_for`, which walks past
            // a dead parent to the nearest subreaper, or to init.
            if !matches!(parent.status(), Status::Exited(_)) {
                linux_parent_inner
                    .children
                    .insert(new_proc.id(), new_proc.clone());
            }
        }

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
                    // And what the layers above keep by pid (the POSIX
                    // timers); see `register_process_exit_hook`.
                    run_process_exit_hooks(child.id());
                    // try_linux (not linux): this callback fires from the object
                    // layer on PROCESS_TERMINATED, concurrently with SMP teardown
                    // churn. A process whose extension can no longer be resolved
                    // must be skipped, not panic the kernel.
                    if let Some(lp) = child.try_linux() {
                        // Drop the file table AFTER releasing the lock — file
                        // teardown can re-enter process accessors (see
                        // close_file).
                        // The shared-memory table goes the same way: its
                        // drop is the detach of every attachment, which
                        // locks each segment, and `fork` locks the segments
                        // under this process's lock.
                        // The futex table is the third: `clear()` dropped
                        // every `Arc<Futex>` in place, and the last one takes
                        // its waiter queue and its owner `Thread` -- whose
                        // `proc` is a strong `Arc` back to this very process --
                        // with it. It has its own lock now (see
                        // `LinuxProcess::futexes`), so it comes out the same
                        // way and goes with every lock released.
                        let dropped = {
                            let futexes = core::mem::take(&mut *lp.futexes.lock());
                            let mut inner = lp.inner.lock();
                            let files = core::mem::take(&mut inner.files);
                            inner.cloexec_fds.clear();
                            inner.semaphores = Default::default();
                            let shm = core::mem::take(&mut inner.shm_identifiers);
                            (files, shm, futexes)
                        };
                        drop(dropped);
                    }
                    if let Some(reaper) = reaper_for(&parent) {
                        if let Some(reaper_lp) = reaper.try_linux() {
                            let policy = child_death_policy(reaper_lp);
                            // The zircon bit wakes a blocked `wait*` either
                            // way: with nothing left to collect it comes back
                            // ECHILD, which is how POSIX says a `wait` ends
                            // when SIGCHLD is ignored.
                            reaper.signal_set(Signal::SIGCHLD);
                            if policy.autoreap {
                                reaper_lp.forget_child(child.id());
                            } else {
                                reaper_lp.record_child_exit(
                                    child.id(),
                                    exit_code,
                                    child_cpu(&child),
                                );
                            }
                            if !policy.notify {
                                return true;
                            }
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
    /// The process's futex objects, keyed by the address of their word.
    ///
    /// Outside `inner` ON PURPOSE, like `itimers` right below. `inner` is this
    /// process's big lock: the file-descriptor table, the credentials, the
    /// job-control state and the child list all live under it, and well over a
    /// hundred call sites in this file take it -- among them every descriptor
    /// lookup, so every `read`, `write` and `ioctl`. The futex table is on an entirely
    /// different hot path -- every contended `pthread_mutex_lock`, every
    /// `pthread_cond_wait` and every `pthread_cond_signal` of a threaded
    /// program looks one up here -- and nothing correlates the two. These are
    /// IRQ-off spin locks, so sharing one made each path burn another CPU's
    /// cycles waiting on the other for no reason: a thread handing off a
    /// condition variable queued behind a `fork` cloning the whole file table
    /// under that same lock.
    ///
    /// A fresh `LinuxProcess` always starts with an empty table, `fork`
    /// included: the objects are keyed by address in *this* address space and
    /// hold *this* process's waiters. The child's memory is a copy -- same
    /// addresses, different pages, nobody waiting -- so handing it the parent's
    /// objects would let a `futex_wake` in the child reach threads of the
    /// parent waiting on their own memory. Living outside `inner` makes that
    /// structural: there is no inheritance to forget.
    futexes: Mutex<FutexTable>,
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
    /// When this process was created, in nanoseconds of the monotonic clock
    /// (`task->start_boottime`): what field 22 of `/proc/<pid>/stat` reports
    /// in clock ticks, and what `ps -o etime,start` and `top`'s TIME+ column
    /// derive from. A fork stamps the child with its own birth; an exec
    /// keeps it, since Linux's `start_time` is the task's and not the
    /// image's.
    start_ns: u64,
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
    /// `prctl(PR_SET_KEEPCAPS)`, `SECBIT_KEEP_CAPS`: recorded, read back by
    /// `PR_GET_KEEPCAPS`, inherited by `fork` and cleared by `execve`
    /// (`cap_bprm_creds_from_file`). Capabilities here follow the effective
    /// uid alone, so there is nothing for it to keep, but a daemon that
    /// drops privilege asks for it before `setuid` and treats a refusal as
    /// fatal: `prctl(PR_SET_KEEPCAPS, 1)` was EINVAL and ntpd, chrony and
    /// dumpcap (and libcap's `cap_setuid`) stopped at startup.
    keep_caps: bool,
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

    /// Drop a dead child without keeping anything for `wait*`: the reaper
    /// ignores `SIGCHLD` (or set `SA_NOCLDWAIT`), so Linux releases the
    /// task in `exit_notify` instead of leaving a zombie, and its CPU never
    /// reaches the reaper's `RUSAGE_CHILDREN` (that only counts children
    /// that were waited for).
    pub fn forget_child(&self, child_id: KoID) {
        let mut inner = self.inner.lock();
        inner.children.remove(&child_id);
        inner.reaped_children.remove(&child_id);
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

        note_process_created();
        LinuxProcess {
            root_inode,
            parent: Mutex::new(Weak::default()),
            vt,
            perf: crate::perf::ProcPerf::new(),
            futexes: Default::default(),
            itimers: Default::default(),
            aspace_lock: Mutex::new(()),
            inner: Mutex::new(LinuxProcessInner {
                files,
                start_ns: monotonic_now_ns(),
                ..Default::default()
            }),
        }
    }

    /// When this process was created, in nanoseconds of the monotonic clock
    /// (`task->start_boottime`). See `LinuxProcessInner::start_ns`.
    pub fn start_time_ns(&self) -> u64 {
        self.inner.lock().start_ns
    }

    /// Credit the CPU time of a reaped child, as `wait*` does when it reaps
    /// one: what `RUSAGE_CHILDREN`, `times()` and fields 16/17 of
    /// `/proc/<pid>/stat` add up.
    #[cfg(test)]
    pub(crate) fn credit_children_cpu(&self, cpu: ChildCpu) {
        self.inner.lock().add_children_cpu(cpu);
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

    /// `prctl(PR_GET_KEEPCAPS)`.
    pub fn keep_caps(&self) -> bool {
        self.inner.lock().keep_caps
    }

    /// Record `PR_SET_KEEPCAPS`.
    pub fn set_keep_caps(&self, on: bool) {
        self.inner.lock().keep_caps = on;
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
        // The table's sweep hands its victims out rather than dropping them in
        // place, so they go with the futex lock released: see
        // `FutexTable::get_or_create`.
        let mut swept = Vec::new();
        let futex = self.futexes.lock().get_or_create(
            uaddr,
            || {
                let value = unsafe { &*(uaddr as *const AtomicI32) };
                Futex::new(value)
            },
            &mut swept,
        );
        drop(swept);
        Some(futex)
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
        let forget;
        let old = {
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
            // `dup2` onto an open descriptor closes it, and closing it tells
            // the epolls, like `close` does. Planned after the insert so the
            // new file at `fd` counts as a holder if it is the same
            // description (`dup2(fd, fd)` is a no-op in Linux too).
            forget = old
                .as_ref()
                .map(|f| inner.epoll_forget_plan(vec![(fd, f.clone())]));
            old
        };
        if let Some(forget) = forget {
            forget.run();
        }
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

    /// `set_task_ioprio()` (`block/ioprio.c`): whether `caller` may change
    /// the I/O priority of a task running under `target`'s credentials.
    ///
    /// ```c
    /// if (!uid_eq(tcred->uid, cred->euid) &&
    ///     !uid_eq(tcred->uid, cred->uid) && !capable(CAP_SYS_NICE)) {
    ///         err = -EPERM;
    /// ```
    ///
    /// Not the same rule as [`Self::may_set_priority_of`]: it is the target's
    /// REAL uid that is compared, against either of the caller's, so a task
    /// running set-uid as somebody else is still its real owner's to renice
    /// for I/O, and a caller's real uid counts where `setpriority` only
    /// looks at the effective one.
    pub fn may_set_ioprio_of(caller: &Credentials, target: &Credentials) -> bool {
        target.ruid == caller.euid
            || target.ruid == caller.ruid
            || has_capability(caller.euid, CAP_SYS_NICE)
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
        let (removed, forget) = {
            let mut inner = self.inner.lock();
            inner.cloexec_fds.remove(&fd);
            let removed = inner.files.remove(&fd);
            let forget = removed
                .as_ref()
                .map(|f| inner.epoll_forget_plan(vec![(fd, f.clone())]));
            (removed, forget)
        };
        if let Some(forget) = forget {
            forget.run();
        }
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
        let (removed, forget): (Vec<(FileDesc, Arc<dyn FileLike>)>, _) = {
            let mut inner = self.inner.lock();
            let fds: Vec<_> = inner
                .files
                .keys()
                .filter(|&&fd| fd >= first && fd <= last)
                .cloned()
                .collect();
            let removed: Vec<_> = fds
                .into_iter()
                .filter_map(|fd| {
                    inner.cloexec_fds.remove(&fd);
                    inner.files.remove(&fd).map(|f| (fd, f))
                })
                .collect();
            let forget = inner.epoll_forget_plan(removed.clone());
            (removed, forget)
        };
        forget.run();
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
        Self::sticky_verdict(&self.credentials(), dir_metadata, target_metadata)
    }

    /// Linux's `check_sticky()`: in a sticky directory only root, the
    /// directory's owner and the entry's owner may remove or replace the
    /// entry. Pure, on the metadata of both.
    fn sticky_verdict(
        creds: &Credentials,
        dir_metadata: &Metadata,
        target_metadata: &Metadata,
    ) -> LxResult {
        if (dir_metadata.mode & MODE_STICKY) == 0 {
            return Ok(());
        }
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

    /// What `vfs_rename` and its two `may_delete` calls decide about a
    /// rename of `old` (in `old_dir`) onto `new` (in `new_dir`, `None` when
    /// no such entry exists), once both parents are known writable and
    /// searchable. Pure, so the matrix is unit-testable.
    ///
    /// Only the first of these was ever asked before, which left the other
    /// three to whoever called: in a sticky `/tmp`, `mv mine yours` replaced
    /// another user's file (the very thing the sticky bit forbids `rm` from
    /// doing, and which `unlinkat` here already refused); a file could be
    /// renamed onto a directory and a directory onto a file, and a directory
    /// could be moved out of one parent into another without the caller
    /// holding write permission on it -- Linux asks for it because the move
    /// rewrites the directory's own `..`.
    ///
    /// The order is Linux's: `may_delete(old_dir, old)` (the sticky bit), then
    /// `may_delete(new_dir, new, is_dir)` on the target, whose sticky `EPERM`
    /// comes before its type mismatch (`ENOTDIR` for a directory onto a
    /// non-directory, `EISDIR` for the reverse), then the `MAY_WRITE` on a
    /// directory changing parents (`EACCES`).
    fn rename_verdict(
        creds: &Credentials,
        old_dir: &Metadata,
        old: &Metadata,
        new_dir: &Metadata,
        new: Option<&Metadata>,
    ) -> LxResult {
        Self::sticky_verdict(creds, old_dir, old)?;
        let is_dir = old.type_ == FileType::Dir;
        if let Some(new) = new {
            Self::sticky_verdict(creds, new_dir, new)?;
            let new_is_dir = new.type_ == FileType::Dir;
            if is_dir && !new_is_dir {
                return Err(LxError::ENOTDIR);
            }
            if !is_dir && new_is_dir {
                return Err(LxError::EISDIR);
            }
        }
        let same_parent = old_dir.dev == new_dir.dev && old_dir.inode == new_dir.inode;
        if is_dir && !same_parent {
            Self::access_verdict(
                creds,
                old.uid as u32,
                old.gid as u32,
                old.mode,
                true,
                ACCESS_WRITE,
                true,
            )?;
        }
        Ok(())
    }

    /// [`rename_verdict`](Self::rename_verdict) for this process.
    pub fn check_rename(
        &self,
        old_dir: &Metadata,
        old: &Metadata,
        new_dir: &Metadata,
        new: Option<&Metadata>,
    ) -> LxResult {
        Self::rename_verdict(&self.credentials(), old_dir, old, new_dir, new)
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

    /// Whether `creds` may set the times of a file owned by `owner_uid` with
    /// mode `mode` (Linux's `utimes_common` / `setattr_prepare`).
    ///
    /// Explicit times (`ATTR_ATIME_SET` / `ATTR_MTIME_SET`) are the owner's
    /// and root's alone: `EPERM` for anyone else, write access or not. A
    /// touch (a null `times`, or both `UTIME_NOW`) is `ATTR_TOUCH`: the owner
    /// and root, or anyone `inode_permission(MAY_WRITE)` lets write the file
    /// (`EACCES` otherwise). Ownership is judged by the FILESYSTEM uid, as
    /// `inode_owner_or_capable` does.
    ///
    /// Nothing checked this before: `utimensat`, `utimes` and `futimens` set
    /// whatever times they were handed on whatever file they named, so any
    /// process could backdate `/etc/passwd`, hide a modification from `make`
    /// or a backup's mtime comparison, or forge the timestamps of another
    /// user's files.
    fn utimes_verdict(
        creds: &Credentials,
        owner_uid: u32,
        owner_gid: u32,
        mode: u16,
        is_dir: bool,
        explicit: bool,
    ) -> LxResult {
        // `inode_owner_or_capable()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
        if creds.fsuid == ROOT_UID || creds.fsuid == owner_uid {
            return Ok(());
        }
        if explicit {
            return Err(LxError::EPERM);
        }
        Self::access_verdict(
            creds,
            owner_uid,
            owner_gid,
            mode,
            is_dir,
            ACCESS_WRITE,
            true,
        )
    }

    /// [`utimes_verdict`](Self::utimes_verdict) for this process on
    /// `metadata`: `explicit` when any of the times is a value the caller
    /// chose rather than "now".
    pub fn check_utimes(&self, metadata: &Metadata, explicit: bool) -> LxResult {
        Self::utimes_verdict(
            &self.credentials(),
            metadata.uid as u32,
            metadata.gid as u32,
            metadata.mode,
            metadata.type_ == FileType::Dir,
            explicit,
        )
    }

    /// Whether `creds` may make a new hard link to a file owned by
    /// `owner_uid:owner_gid` with mode `mode` and type `type_`.
    ///
    /// `vfs_link`: a directory is never linkable, by anyone (`EPERM`). Then
    /// `may_linkat` with `fs.protected_hardlinks` on, the default since Linux
    /// 3.6: the owner and root link what they like; anyone else only a "safe
    /// source" (`safe_hardlink_source`): a regular file, not setuid, not
    /// setgid-and-group-executable, that they could open for reading AND
    /// writing. Everything else is `EPERM`, the way Linux answers it, not the
    /// `EACCES` of the access check underneath.
    ///
    /// Nothing checked this before: any process could pin `/etc/shadow`, a
    /// setuid binary or another user's private file under a name of its own
    /// choosing, and keep its content across the owner's replace-and-unlink
    /// (which is exactly the attack `protected_hardlinks` exists to stop).
    fn link_verdict(
        creds: &Credentials,
        owner_uid: u32,
        owner_gid: u32,
        mode: u16,
        type_: FileType,
    ) -> LxResult {
        if type_ == FileType::Dir {
            return Err(LxError::EPERM);
        }
        // `inode_owner_or_capable()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
        if creds.fsuid == ROOT_UID || creds.fsuid == owner_uid {
            return Ok(());
        }
        if type_ != FileType::File {
            return Err(LxError::EPERM);
        }
        if mode & MODE_SET_UID != 0 {
            return Err(LxError::EPERM);
        }
        if mode & (MODE_SET_GID | MODE_EXEC_GRP) == (MODE_SET_GID | MODE_EXEC_GRP) {
            return Err(LxError::EPERM);
        }
        Self::access_verdict(
            creds,
            owner_uid,
            owner_gid,
            mode,
            false,
            ACCESS_READ | ACCESS_WRITE,
            true,
        )
        .map_err(|_| LxError::EPERM)
    }

    /// `inode_owner_or_capable`: whether `creds` own the file (by the
    /// FILESYSTEM uid, `vfsuid_eq_kuid(vfsuid, current_fsuid())`) or are
    /// root. The question behind chmod, chown of a group, explicit utimes
    /// and `O_NOATIME`.
    fn owner_or_capable(creds: &Credentials, owner_uid: u32) -> bool {
        creds.fsuid == ROOT_UID || creds.fsuid == owner_uid
    }

    /// `may_open`: "O_NOATIME can only be set by the owner or superuser",
    /// `EPERM` for anyone else. Nothing asked this before: any process could
    /// read another user's file without leaving an access time behind.
    pub fn check_owner_or_capable(&self, metadata: &Metadata) -> LxResult {
        if Self::owner_or_capable(&self.credentials(), metadata.uid as u32) {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// [`link_verdict`](Self::link_verdict) for this process on the file
    /// `metadata` describes, the one `linkat(2)` was asked to link.
    pub fn check_link(&self, metadata: &Metadata) -> LxResult {
        Self::link_verdict(
            &self.credentials(),
            metadata.uid as u32,
            metadata.gid as u32,
            metadata.mode,
            metadata.type_,
        )
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
        // The old attachments come out under the lock and are dropped after
        // it: dropping them locks each segment (see `ShmProc`).
        let old_attachments = self.inner.lock().reset_for_exec(privileged);
        drop(old_attachments);
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
        flush_signal_handlers(&mut self.inner.lock().signal_actions.table);
    }

    /// Close file that FD_CLOEXEC is set
    pub fn remove_cloexec_files(&self) {
        // Remove under the lock, DROP outside it — see `close_file` for the
        // re-entrancy deadlock this avoids.
        type RemovedFds = Vec<(FileDesc, Arc<dyn FileLike>)>;
        let (removed, forget, exec_path): (RemovedFds, _, String) = {
            let mut inner = self.inner.lock();
            // Per-fd state is authoritative — NOT the flag inside the (possibly
            // fork-shared) `File` objects, which is only a creation-time record.
            let close_fds = inner.cloexec_fds.drain().collect::<Vec<_>>();
            let removed: RemovedFds = close_fds
                .into_iter()
                .filter_map(|fd| inner.files.remove(&fd).map(|f| (fd, f)))
                .collect();
            let forget = inner.epoll_forget_plan(removed.clone());
            (removed, forget, inner.execute_path.clone())
        };
        forget.run();
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
    pub fn semaphores_add(&self, id: usize, array: Arc<SemArray>) {
        self.inner.lock().semaphores.add(id, array)
    }

    /// The set `id` names for a `semop` or `semctl` of this process, or
    /// `None` (`EINVAL`) when it names none.
    ///
    /// `sem_obtain_object_check`: the id must name a set in the system
    /// table now. One removed with `IPC_RMID` names nothing, to the process
    /// that created it as much as to anyone else (a sleeper in `semop` is
    /// woken into `EIDRM` by [`SemArray::remove`]; the next call is
    /// `EINVAL`). The per-process table is for the `SEM_UNDO` replay at
    /// exit, not a second namespace: it used to be consulted first, so a
    /// process kept operating by id on a set another had removed (`ipcrm
    /// -s`, a daemon's own `IPC_RMID` on restart), with values nobody else
    /// could see, and a lock built on it held nothing.
    ///
    /// The id may have been created by another program and passed here,
    /// which is what a system-wide id is for; it is recorded in the table
    /// so the `SEM_UNDO` records `semop` leaves have a set to replay
    /// against.
    pub fn semaphores_get(&self, id: usize) -> Option<Arc<SemArray>> {
        let array = crate::ipc::sem_lookup(id)?;
        let mut inner = self.inner.lock();
        if inner.semaphores.get(id).is_none() {
            inner.semaphores.add(id, array.clone());
        }
        Some(array)
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

    /// Record an attachment of segment `id` at `addr`: what `shmat` mapped.
    pub fn shm_attach(&self, id: usize, shared_guard: Arc<Mutex<ShmGuard>>, addr: usize) {
        self.inner
            .lock()
            .shm_identifiers
            .attach(id, shared_guard, addr)
    }

    /// Forget the attachment at `addr`, and say what was there; `None` when
    /// nothing was, which `shmdt` answers with `EINVAL`.
    pub fn shm_detach(&self, addr: usize) -> Option<ShmIdentifier> {
        self.inner.lock().shm_identifiers.detach(addr)
    }
}

/// What a close has to tell this process's epolls, decided under the table
/// lock and delivered after it: [`LinuxProcessInner::epoll_forget_plan`].
pub(crate) struct EpollForgetPlan {
    epolls: Vec<Arc<crate::fs::Epoll>>,
    gone: Vec<(FileDesc, Arc<dyn FileLike>)>,
}

impl EpollForgetPlan {
    /// Remove every `gone` entry from every epoll. Called with the table lock
    /// released: an epoll takes its own lock, and the `Arc`s dropped here may
    /// be the description's last, whose teardown re-enters the process (see
    /// [`LinuxProcess::close_file`]).
    pub(crate) fn run(self) {
        for epoll in &self.epolls {
            for (_, file) in &self.gone {
                epoll.forget_closed(file);
            }
        }
    }
}

impl LinuxProcessInner {
    /// The closed descriptors whose open file description no descriptor of
    /// this table holds any more, with the epolls of this table that may
    /// watch them.
    ///
    /// Linux drops an epoll entry from `__fput`, when the LAST reference to
    /// the description goes: a `dup` of a watched descriptor keeps its entry
    /// alive through the close of the original, and epoll(7) tells the
    /// program to `EPOLL_CTL_DEL` explicitly in that case. The references
    /// this table can see are its own, which is where every dup made by
    /// `dup`, `dup2` and `fcntl(F_DUPFD)` lives; a fork's copy of the same
    /// description in another process is not counted, and its own close
    /// tells its own epolls.
    ///
    /// `closed` must already be out of `files`.
    fn epoll_forget_plan(&self, closed: Vec<(FileDesc, Arc<dyn FileLike>)>) -> EpollForgetPlan {
        let epolls: Vec<Arc<crate::fs::Epoll>> = self
            .files
            .values()
            .filter_map(|f| f.clone().downcast_arc::<crate::fs::Epoll>().ok())
            .collect();
        let gone = if epolls.is_empty() {
            Vec::new()
        } else {
            closed
                .into_iter()
                .filter(|(_, file)| !self.files.values().any(|f| Arc::ptr_eq(f, file)))
                .collect()
        };
        EpollForgetPlan { epolls, gone }
    }

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
    ///
    /// Returns the shared-memory table the old image had, for the caller to
    /// drop once the process lock is released: its drop accounts a detach on
    /// every segment it had attached, and that locks each segment.
    fn reset_for_exec(&mut self, privileged: bool) -> ShmProc {
        // Every System V segment this process had attached was mapped in the
        // address space `execve` just cleared; Linux unmaps them with the
        // rest of the old mm and each `shm_close` accounts its detach. Kept,
        // these entries claim attachments at addresses that now belong to the
        // NEW image, and `shmdt` trusts them: it looks the address up in this
        // very map and unmaps that many bytes there (`sys_shmdt`), so a
        // detach of a segment the process no longer has punches a hole in the
        // new program. The detach itself is the table's drop.
        let old_attachments = core::mem::take(&mut self.shm_identifiers);

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
        // `cap_bprm_creds_from_file`: `SECBIT_KEEP_CAPS` does not survive an
        // exec, privileged or not.
        self.keep_caps = false;

        // Left alone on purpose, because execve(2) and friends say so: the
        // file table (minus close-on-exec, done by `remove_cloexec_files`),
        // the credentials, the working directory, the process group and
        // session, `no_new_privs`, the resource limits, and the `setitimer`
        // interval timers, which setitimer(2) preserves across an exec.
        // `brk`/`mapped_brk`, `environ`, `cmdline`, `execute_path` and `abi`
        // are all overwritten by the caller from the new image.
        old_attachments
    }

    fn forked_child(&self, pgid: KoID, sid: KoID, start_ns: u64) -> Self {
        note_process_created();
        LinuxProcessInner {
            // `copy_process`: `p->start_time = ktime_get_ns()`. The child is
            // born now, whenever its parent was; carried over, every child
            // of a long-lived shell would claim the shell's start time and
            // `ps -o etime` would show the shell's age for all of them.
            start_ns,
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
            keep_caps: self.keep_caps,
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
            shm_identifiers: self.shm_identifiers.inherited(),

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
    deliver_sigint_for_vt(None)
}

/// `SIGINT` to the foreground group of `vt`, or of the VT on screen when no VT
/// is named, falling back to the calling thread's own process.
///
/// A latched Ctrl-C names its VT ([`CtrlCInterrupt`](crate::fs::stdio::CtrlCInterrupt)):
/// the keystroke may have arrived on the serial console while the desktop
/// holds the graphics VT, and answering it with the VT on screen signalled the
/// desktop's process group instead of the one that was typed at.
pub fn deliver_sigint_for_vt(vt: Option<usize>) {
    let pgid = match vt {
        Some(vt) => crate::fs::stdio::vt_foreground_pgrp(vt),
        None => crate::fs::stdio::get_foreground_pgrp(),
    };
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
    let signal = interrupt_escalation(pgid, armed_pgid);
    if pgid > 0 {
        let _ = send_signal_to_pgrp(pgid as usize, signal);
    }
    signal
}

/// Which of the two [`interrupt_or_force_pgrp`] would send, arming the tty for
/// the next Ctrl-C, but WITHOUT sending it.
///
/// For the caller that is holding a lock the signalled process may itself
/// need: a signal send walks the job tree and takes each target's own locks,
/// and doing that from under a terminal's lock is a cycle waiting for the
/// right pair of CPUs. The pty's Ctrl-C did exactly that while its Ctrl-\ and
/// Ctrl-Z, three lines below, already queued the signal and sent it after the
/// lock was dropped. Deciding here and sending there makes the three the same
/// shape, and the label the echo needs (`^C` or `^C (killed)`) is known
/// before anything goes out.
pub fn interrupt_escalation(pgid: i32, armed_pgid: &AtomicI32) -> LinuxSignal {
    if pgid <= 0 {
        return LinuxSignal::SIGINT;
    }
    if armed_pgid.load(Ordering::Relaxed) == pgid {
        armed_pgid.store(0, Ordering::Relaxed);
        zcore_drivers::klog_warn!("[tty] second Ctrl-C on pgrp {} -> SIGKILL (forced)", pgid);
        LinuxSignal::SIGKILL
    } else {
        armed_pgid.store(pgid, Ordering::Relaxed);
        LinuxSignal::SIGINT
    }
}

/// Drop the double-Ctrl-C arm (new foreground job, ctty change, etc.).
pub fn clear_interrupt_arm(armed_pgid: &AtomicI32) {
    armed_pgid.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod interrupt_escalate_tests;

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

/// What a reaper's `SIGCHLD` disposition says to do with a child that just
/// died: `do_notify_parent`'s `autoreap`, and whether the signal is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChildDeathPolicy {
    /// Release the child at once instead of leaving a zombie for `wait*`.
    autoreap: bool,
    /// Queue the Linux `SIGCHLD` at all.
    notify: bool,
}

/// `SIG_IGN` on `SIGCHLD` means "I will never wait for them": the child is
/// released without a zombie and no signal is sent. `SA_NOCLDWAIT` with a
/// handler means the same for the zombie, but the handler still runs
/// (sigaction(2), and the `psig->action[SIGCHLD-1]` test in
/// `do_notify_parent`). Anything else leaves the zombie and sends the signal.
fn child_death_policy(reaper: &LinuxProcess) -> ChildDeathPolicy {
    let action = reaper.signal_action(LinuxSignal::SIGCHLD);
    if action.handler == crate::signal::SIG_IGN {
        ChildDeathPolicy {
            autoreap: true,
            notify: false,
        }
    } else if action
        .flags
        .contains(crate::signal::SignalActionFlags::NOCLDWAIT)
    {
        ChildDeathPolicy {
            autoreap: true,
            notify: true,
        }
    } else {
        ChildDeathPolicy {
            autoreap: false,
            notify: true,
        }
    }
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
    if let Some(intr) = crate::fs::stdio::ctrl_c_pending_take() {
        // Only when the keystroke handler could not signal the group itself:
        // it already did whenever the VT had one, and delivering again is how
        // a single Ctrl-C came to raise two `SIGINT`s.
        if intr.signal_owed {
            deliver_sigint_for_vt(Some(intr.vt));
        }
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

/// The monotonic clock now, in nanoseconds: what a process's birth is
/// stamped with (`ktime_get_ns()` in `copy_process`).
fn monotonic_now_ns() -> u64 {
    kernel_hal::timer::timer_now().as_nanos() as u64
}

/// Processes created since boot, exited or not: Linux's `total_forks`,
/// bumped once per `copy_process`, which the `processes` line of
/// `/proc/stat` publishes and `vmstat` differentiates into forks per second.
static PROCESSES_CREATED: AtomicU64 = AtomicU64::new(0);

fn note_process_created() {
    PROCESSES_CREATED.fetch_add(1, Ordering::Relaxed);
}

/// How many processes have been created since boot (`total_forks`): a
/// counter that only grows, not the number alive now.
pub fn processes_created() -> u64 {
    PROCESSES_CREATED.load(Ordering::Relaxed)
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

/// `flush_signal_handlers(t, force_default = 0)` (kernel/signal.c), what an
/// `execve` does to the disposition table: a caught signal goes back to
/// `SIG_DFL`, an ignored one stays ignored, and EVERY entry loses its
/// `sa_flags`, `sa_mask` and restorer, the ignored and default ones too.
///
/// The flags of a `SIG_DFL`/`SIG_IGN` entry used to survive the exec. A
/// parent that set SIGCHLD to `SIG_DFL` with `SA_NOCLDWAIT` and then
/// exec'd a shell handed that shell a table in which its children are
/// reaped on their own, so its `waitpid` got ECHILD for every job; and a
/// `sigaction(sig, NULL, &old)` after the exec reported the stale flags.
pub fn flush_signal_handlers(table: &mut [SignalAction]) {
    use crate::signal::SIG_IGN;
    for action in table.iter_mut() {
        *action = if action.handler == SIG_IGN {
            SignalAction {
                handler: SIG_IGN,
                ..SignalAction::default()
            }
        } else {
            SignalAction::default()
        };
    }
}

/// `do_sigaction` after storing a disposition: when the new one ignores the
/// signal (`SIG_IGN`, or `SIG_DFL` for a signal whose default is to ignore),
/// "any pending instances of the signal are discarded" (sigaction(2)), from
/// every thread of the process, blocked or not
/// (`flush_sigqueue_mask` over `shared_pending` and each `t->pending`).
///
/// The table store used to be all there was: block SIGUSR1, raise it, set
/// it to `SIG_IGN`, and `sigpending()` still listed it; a handler installed
/// again before unblocking then got the stale signal.
pub fn set_signal_action_in(process: &Arc<Process>, signal: LinuxSignal, action: SignalAction) {
    use crate::thread::ThreadExt;
    process.linux().set_signal_action(signal, action);
    if !discards_when_pending(action.handler, signal) {
        return;
    }
    for tid in process.thread_ids() {
        if let Ok(thread) = process.get_child(tid) {
            if let Ok(thread) = thread.downcast_arc::<Thread>() {
                if let Some(mut lt) = thread.try_lock_linux() {
                    if lt.signals.contains(signal) {
                        lt.take_siginfo(signal);
                    }
                }
            }
        }
    }
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
            return check_signals_of(&thread);
        }
    }
    Ok(())
}

/// [`check_signals`] for a named thread rather than the current one: what an
/// interruptible sleep asks about the thread it is putting to sleep.
pub fn check_signals_of(thread: &Arc<Thread>) -> LxResult<()> {
    {
        {
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

/// The zircon bit on a Linux THREAD object that says "a signal was just
/// queued to you": what [`interruptible_sleep_until`] parks on. Set by every
/// path that makes a signal pending on a thread ([`wake_signal_sleeper`]).
pub const SIGNAL_WAKE: Signal = Signal::USER_SIGNAL_0;

/// The zircon bit the sleep's own deadline timer sets on the thread.
const SLEEP_DEADLINE: Signal = Signal::USER_SIGNAL_2;

/// Tell `thread` a signal was queued to it, so a sleep it is in ends now.
pub fn wake_signal_sleeper(thread: &Arc<Thread>) {
    thread.signal_set(SIGNAL_WAKE);
}

/// Sleep until `deadline`, or until a signal that would interrupt a syscall
/// is pending on `thread`, whichever comes first: `TASK_INTERRUPTIBLE`, the
/// sleep of `nanosleep(2)`, `clock_nanosleep(2)` and `pause(2)`. `Ok` at the
/// deadline; `EINTR` with the signal still pending otherwise.
///
/// `nanosleep` used to be one uninterruptible `sleep_until(deadline)` with a
/// signal check after it: a daemon in `sleep(60)` took up to a minute to see
/// the `SIGTERM` its handler was waiting for, and `alarm(1)` did nothing to a
/// `sleep(10)` until the ten seconds were up.
///
/// The bit is cleared BEFORE the pending set is looked at, so a signal that
/// arrives after the look sets it again and the wait returns at once; the
/// deadline is a timer that sets a second bit, and a stale one from an
/// earlier sleep only costs a spurious pass through the loop.
pub async fn interruptible_sleep_until(
    thread: &Arc<Thread>,
    deadline: core::time::Duration,
) -> LxResult<()> {
    let mut park = SignalPark::new(thread);
    loop {
        park.prepare();
        check_signals_of(thread)?;
        if kernel_hal::timer::timer_now() >= deadline {
            return Ok(());
        }
        park.park(Some(deadline)).await;
    }
}

/// Park until a signal that would interrupt a syscall is pending on
/// `thread`: `pause(2)` and `rt_sigsuspend(2)`, which return only through
/// that signal, so this returns the error to hand back (`EINTR`, or what
/// [`check_signals_of`] says about a thread being torn down).
///
/// Both used to spin on a 10 ms `sleep_until`: a shell parked in `pause`
/// woke a hundred times a second for nothing, and a hundred idle daemons
/// were ten thousand wakeups a second on a machine doing nothing.
pub async fn wait_for_signal(thread: &Arc<Thread>) -> LxError {
    let mut park = SignalPark::new(thread);
    loop {
        park.prepare();
        if let Err(e) = check_signals_of(thread) {
            return e;
        }
        park.park(None).await;
    }
}

/// Run `future`, but give up as soon as a signal that would interrupt a
/// syscall is pending on the calling thread: `EINTR`, with the signal still
/// pending so `run_user` delivers it on the way out.
///
/// This is the generic form of what [`interruptible_sleep_until`] and
/// [`wait_for_signal`] do by hand, for a wait whose own future knows nothing
/// about signals. `INode::async_poll` is the case that matters: a blocking
/// `read` of a pipe parks on the pipe's event bus, which only a writer or a
/// close ever fires, so the thread took no signal at all however long it sat
/// there. `__synccall` -- what musl turns `setuid`/`seteuid`/`setgid` into --
/// stops every OTHER thread with SIGRT34 and waits for each to check in, so
/// one thread parked in `read` and the whole process is stuck for good, and
/// deaf to ^C because `__synccall` has blocked every application signal.
///
/// The future is dropped when a signal wins, so it must be one that can be
/// abandoned: a readiness wait, not a wait that consumes the thing it waits
/// for. `INode::async_poll` qualifies -- dropping it unsubscribes and leaves
/// the data where it is.
///
/// With no Linux thread on this CPU (a kernel context, a timer callback)
/// there is nothing to signal, and the future is simply awaited.
pub async fn interruptible<F: Future>(future: F) -> LxResult<F::Output> {
    match kernel_hal::thread::get_current_thread().and_then(|arc| arc.downcast::<Thread>().ok()) {
        Some(thread) => interruptible_on(&thread, future).await,
        None => Ok(future.await),
    }
}

/// [`interruptible`] for a named thread rather than the current one, the way
/// [`check_signals_of`] stands next to [`check_signals`].
pub async fn interruptible_on<F: Future>(thread: &Arc<Thread>, future: F) -> LxResult<F::Output> {
    futures::pin_mut!(future);
    let interrupted = async {
        let mut park = SignalPark::new(thread);
        loop {
            // Clear the wake bits BEFORE looking, so a signal queued after
            // the look wakes the park at once instead of being missed.
            park.prepare();
            if let Err(e) = check_signals_of(thread) {
                return e;
            }
            park.park(None).await;
        }
    };
    futures::pin_mut!(interrupted);
    match futures::future::select(future, interrupted).await {
        futures::future::Either::Left((out, _)) => Ok(out),
        futures::future::Either::Right((err, _)) => Err(err),
    }
}

/// The parking half of an interruptible wait on a Linux thread: wake when a
/// signal is queued to `thread` -- blocked or not, which is what a
/// `sigtimedwait` on a blocked set needs -- or when a deadline passes.
///
/// The protocol is two calls per pass, [`Self::prepare`] then
/// [`Self::park`], with the caller's own look at its condition in between:
/// `prepare` clears the wake bits BEFORE the look, so a signal queued after
/// the look sets a bit again and the park returns at once. A stale deadline
/// bit from an earlier pass only costs one spurious pass.
pub struct SignalPark {
    thread: Arc<Thread>,
    object: Arc<dyn KernelObject>,
    armed: bool,
}

impl SignalPark {
    /// A park on `thread`.
    pub fn new(thread: &Arc<Thread>) -> Self {
        SignalPark {
            thread: thread.clone(),
            object: thread.clone(),
            armed: false,
        }
    }

    /// Clear the wake bits. Call this before looking at the condition the
    /// park waits for, never after.
    pub fn prepare(&self) {
        self.object.signal_clear(SIGNAL_WAKE | SLEEP_DEADLINE);
    }

    /// Wait for a signal queued to the thread, or for `deadline` (`None`:
    /// only a signal ends the wait). The deadline timer is armed once per
    /// park, on the first call that passes one.
    pub async fn park(&mut self, deadline: Option<core::time::Duration>) {
        if let Some(deadline) = deadline {
            if !self.armed {
                self.armed = true;
                // Weak: the timer must not keep a dead thread's object alive
                // for the length of a long sleep.
                let weak = Arc::downgrade(&self.thread);
                kernel_hal::timer::timer_set(
                    deadline,
                    Box::new(move |_now| {
                        if let Some(thread) = weak.upgrade() {
                            thread.signal_set(SLEEP_DEADLINE);
                        }
                    }),
                );
            }
        }
        self.object.wait_signal(SIGNAL_WAKE | SLEEP_DEADLINE).await;
    }
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

/// The calling process's `struct ucred` -- pid, EFFECTIVE uid, EFFECTIVE gid.
///
/// This is what Linux stamps onto a message at SEND time (`scm_send`), and the
/// reason it has to be sampled there rather than read back later: by the time
/// a receiver asks, the sender may be gone, may have changed its ids, or -- the
/// case that matters -- may never have been the process that created the
/// socket. chromium's zygote passes an inherited `socketpair` end to a forked
/// child, and it is the CHILD's pid the browser is waiting to read.
///
/// `None` when no user thread is on this CPU, i.e. when the kernel itself is
/// the writer: there is no process to speak for, and the caller leaves the
/// message unstamped rather than inventing pid 0.
pub fn current_ucred() -> Option<(i32, u32, u32)> {
    let arc = kernel_hal::thread::get_current_thread()?;
    let thread = arc.downcast::<Thread>().ok()?;
    let proc = thread.proc();
    let lp = proc.try_linux()?;
    let c = lp.credentials();
    Some((proc.id() as i32, c.euid, c.egid))
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
mod tests;

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

/// `prepare_signal()`: a stop and a continue cancel each other where they are
/// waiting to be received, so a process never carries both and the last one
/// sent is the one that decides.
///
/// Process-wide whoever the signal was aimed at, because the state it touches
/// is: Linux's `prepare_signal` reaches the shared `task->signal`, so a
/// `SIGCONT` aimed at one thread flushes the pending stop of the whole group.
/// The thread-directed path did not call this at all -- see
/// [`complete_signal_for_job_control`].
pub fn prepare_signal(process: &Arc<Process>, signal: LinuxSignal) {
    use crate::thread::ThreadExt;
    for tid in process.thread_ids() {
        if let Ok(thread_obj) = process.get_child(tid) {
            if let Ok(thread) = thread_obj.downcast_arc::<Thread>() {
                if let Some(mut lt) = thread.try_lock_linux() {
                    lt.signals = pending_after_send(lt.signals, signal);
                }
            }
        }
    }
}

/// The job-control half of `complete_signal()`, for a signal aimed at ONE
/// thread (`tkill`, `tgkill`, `rt_tgsigqueueinfo`, and so `pthread_kill`).
///
/// Queueing the signal on the thread is not enough, and this is the half that
/// was missing: a thread of a job-control-stopped process is parked in
/// `wait_while_job_stopped`, which nothing but `job_continue` clears, and a
/// stopped thread does not run to notice a pending `SIGKILL` either. So
/// `tgkill(pid, tid, SIGCONT)` on a Ctrl-Z'd process never resumed it and
/// `tgkill(pid, tid, SIGKILL)` never woke it to die -- and Go, the JVM and
/// glibc route nearly every signal they send through `tgkill`.
pub fn complete_signal_for_job_control(process: &Arc<Process>, signal: LinuxSignal) {
    wake_for_job_control(process, signal)
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
        prepare_signal(&process, signal);
        // Prefer a thread that has the signal *unblocked* — it can act on it
        // right away — and deliver there.
        let mut first: Option<Arc<Thread>> = None;
        for tid in tids {
            if let Ok(thread_obj) = process.get_child(tid) {
                if let Ok(thread) = thread_obj.downcast_arc::<Thread>() {
                    // Peek without holding the guard across a move of `thread`.
                    let delivered = if let Some(mut lt) = thread.try_lock_linux() {
                        // `wants_signal()`: unblocked, or parked in
                        // `rt_sigtimedwait` for exactly this signal.
                        if lt.wants_signal(signal) {
                            lt.queue_signal(signal, info);
                            true
                        } else {
                            false
                        }
                    } else {
                        // Lock held (e.g. PID 1 in waitpid): queue below rather
                        // than dropping the signal — that drop is why lunarbar's
                        // `kill 1` did nothing.
                        false
                    };
                    if delivered {
                        wake_signal_sleeper(&thread);
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
            wake_signal_sleeper(&thread);
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
mod fork_inheritance_tests;

#[cfg(test)]
mod exec_reset_tests;

#[cfg(test)]
mod sugid_tests;

#[cfg(test)]
mod capability_tests;

#[cfg(test)]
mod rlimit_tests;

#[cfg(test)]
mod renice_tests;

#[cfg(test)]
mod sched_permission_tests;

#[cfg(test)]
mod ptrace_access_tests;

#[cfg(test)]
mod kill_permission_tests;

#[cfg(test)]
mod fsid_tests;

#[cfg(test)]
mod utimes_permission_tests;

#[cfg(test)]
mod link_permission_tests;

/// `owner_or_capable`, the gate `O_NOATIME` goes through.
#[cfg(test)]
mod noatime_permission_tests;

/// `may_set_ioprio_of`: which of the caller's ids and the target's ids
/// `set_task_ioprio` compares.
#[cfg(test)]
mod ioprio_permission_tests;

/// `rename_verdict`: the three questions `vfs_rename` asks that `renameat2`
/// never asked. Pure inputs, so every cell of the matrix runs on the host.
#[cfg(test)]
mod rename_permission_tests;

#[cfg(test)]
mod dac_tests;

#[cfg(test)]
mod dup_fd_tests;

#[cfg(test)]
mod job_control_membership_tests;

#[cfg(test)]
mod signal_send_effect_tests;

#[cfg(test)]
mod pending_signal_disposition_tests;

#[cfg(test)]
mod exit_status_tests;

#[cfg(test)]
mod sigchld_tests;

#[cfg(test)]
mod disposition_change_tests;

#[cfg(test)]
mod signal_park_tests;

#[cfg(test)]
mod interruptible_sleep_tests;

/// A semaphore id is system-wide: a process handed one it never `semget`-ed
/// (by a parent through a file, by `ipcrm -s`) must reach the set through it.
#[cfg(test)]
mod sem_id_from_elsewhere_tests;

#[cfg(test)]
mod exit_hook_tests;

#[cfg(test)]
mod reparenting_tests;

#[cfg(test)]
mod init_adoption_tests;

#[cfg(test)]
mod session_and_group_tests;

#[cfg(test)]
mod dying_process_tests;
