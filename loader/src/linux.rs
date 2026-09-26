//! Run Linux process and manage trap/interrupt/syscall.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{future::Future, pin::Pin};
use linux_object::signal::{
    MachineContext, SigInfo, Signal, SignalAction, SignalCode, SignalStack, SignalUserContext,
    Sigset, SIG_DFL, SIG_IGN,
};

use kernel_hal::context::{TrapReason, UserContext, UserContextField};
use kernel_hal::interrupt::intr_on;
use linux_object::fs::{
    vfs::{FileSystem, INode},
    INodeExt,
};
use linux_object::thread::{CurrentThreadExt, ThreadExt};
// `Abi` is only consumed by the x86_64-gated FreeBSD-personality paths; the
// unconditional import broke riscv64/aarch64 builds (`deny(warnings)`).
#[cfg(target_arch = "x86_64")]
use linux_object::process::Abi;
use linux_object::signal::SignalActionFlags;
use linux_object::{loader::LinuxElfLoader, process::ProcessExt};
use zircon_object::task::{CurrentThread, Process, Thread, ThreadState};
use zircon_object::{
    object::{KernelObject, KoID},
    vm::{VmAddressRegion, USER_STACK_PAGES},
    ZxError, ZxResult,
};

fn comm_from_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The x86-64 SysV red zone: 128 bytes below `rsp` that a leaf function may be
/// using right now, so a signal frame must start below them. Doubled here for
/// margin, and kept on every architecture because it only costs stack.
const RED_ZONE_MAX_SIZE: usize = 0x100;

/// Where the signal frame goes: the address the first object is pushed below.
///
/// Linux's `get_sigframe()`. `SA_ONSTACK` moves the frame to the top of the
/// alternate signal stack, which is the whole point of `sigaltstack(2)`: a
/// `SIGSEGV` handler for a blown stack cannot run on the stack that just blew.
/// The red zone is skipped only when we stay on the interrupted stack -- the
/// alternate stack has no live frames to preserve -- and `None` means the
/// frame has nowhere to go (`sp` so low that stepping past the red zone would
/// wrap), which Linux answers with a forced `SIGSEGV`.
fn sigframe_top(user_sp: usize, alt: &SignalStack, want_alt_stack: bool) -> Option<usize> {
    if want_alt_stack && alt.usable_from(user_sp) {
        return alt.sp.checked_add(alt.size);
    }
    user_sp.checked_sub(RED_ZONE_MAX_SIZE)
}

/// Does `[base, base + len)` lie inside one writable user mapping?
///
/// The kernel writes the signal frame through a raw pointer, so this is the
/// `access_ok()` Linux does before `copy_to_user`: `sp` is whatever the
/// interrupted program left in the stack register, and a program is free to
/// leave anything there. `ranges` is `(start, end, writable)` per mapping.
/// One mapping, not several adjacent ones: a frame split across a boundary is
/// not something a real stack does, and treating it as fine would mean
/// trusting the gap between them.
fn frame_fits_writable(ranges: &[(usize, usize, bool)], base: usize, len: usize) -> bool {
    let end = match base.checked_add(len) {
        Some(end) => end,
        None => return false,
    };
    ranges
        .iter()
        .any(|&(start, stop, writable)| writable && base >= start && end <= stop)
}

/// Create and run a single Linux process as PID 1 on virtual terminal 0.
///
/// Used by the libos example/tests, where one program is the whole system.
pub fn run(args: Vec<String>, envs: Vec<String>, rootfs: Arc<dyn FileSystem>) -> Arc<Process> {
    const INIT_PID: KoID = 1;
    spawn(args, envs, rootfs, 0, None, INIT_PID, true, true).expect("run: program not found")
}

/// Create and run the configured per-terminal SHELL on virtual terminal `vt`
/// with the fixed Linux `pid` (the reserved 101.. range). Returns `None` when
/// the shell cannot be started so boot can continue with other VTs and INIT.
///
/// `shared_root` lets extra per-VT shells reuse the primary shell's mounted
/// root filesystem (by `Arc`) instead of re-scanning disks. The one-time boot
/// work (network init, fstab mount, console clear) runs only for `vt == 0`.
pub fn run_shell_on_vt(
    args: Vec<String>,
    envs: Vec<String>,
    rootfs: Arc<dyn FileSystem>,
    vt: usize,
    shared_root: Option<Arc<dyn INode>>,
    pid: KoID,
) -> Option<Arc<Process>> {
    spawn(
        args,
        envs,
        rootfs,
        vt,
        shared_root,
        pid,
        /* boot_work */ vt == 0,
        /* foreground */ true,
    )
}

/// Spawn the INIT process as PID 1 if its binary exists, returning `None`
/// otherwise so the machine can boot without a PID 1 init (e.g. when
/// `/bin/busybox` is not installed). Runs on the primary console (vt 0).
///
/// `boot_work` should be set only when no shell has already performed the
/// one-time boot setup (network init, fstab mount).
pub fn run_init_if_present(
    args: Vec<String>,
    envs: Vec<String>,
    rootfs: Arc<dyn FileSystem>,
    shared_root: Option<Arc<dyn INode>>,
    boot_work: bool,
) -> Option<Arc<Process>> {
    if args.is_empty() || args[0].is_empty() {
        return None;
    }
    if !program_exists(&args[0], &shared_root, &rootfs) {
        warn!(
            "INIT {:?} not present on root or initramfs; booting WITHOUT a PID 1 init (only the per-terminal shells run). Expected /sbin/init (default INIT=/sbin/init -> eclipse-init, busybox init fallback); rebuild the rootfs or set INIT= in rboot.conf.",
            args[0]
        );
        return None;
    }
    const INIT_PID: KoID = 1;
    let init = args[0].clone();
    // `foreground = false`: init shares vt 0 with the primary shell and must not
    // seize its foreground process group (that would wedge the shell on SIGTTIN).
    let proc = spawn(
        args,
        envs,
        rootfs,
        0,
        shared_root,
        INIT_PID,
        boot_work,
        false,
    );
    if proc.is_none() {
        // The binary exists but could not be started (unreadable, malformed
        // ELF, or blocked by the hunter exec policy — see the preceding
        // `spawn:`/`hunter:` console line for which). Don't silently end up
        // with no PID 1: make the fallback to the terminal shell explicit.
        warn!(
            "INIT {:?} present but FAILED to start (see the spawn/hunter line above); falling back to the shell as the lifetime process",
            init
        );
    } else {
        info!("INIT {:?} started as PID {}", init, INIT_PID);
    }
    proc
}

/// True if `path` resolves on the primary root (if any) or the initramfs.
/// Symlinks are followed (e.g. `/sbin/init` -> `/bin/busybox`), matching how
/// `spawn` loads the binary.
fn program_exists(
    path: &str,
    primary_root: &Option<Arc<dyn INode>>,
    rootfs: &Arc<dyn FileSystem>,
) -> bool {
    if let Some(root) = primary_root {
        if root.lookup_follow(path, FOLLOW_LINK_DEPTH).is_ok() {
            return true;
        }
    }
    rootfs
        .root_inode()
        .lookup_follow(path, FOLLOW_LINK_DEPTH)
        .is_ok()
}

/// Max symlink hops to follow when resolving an exec target (e.g.
/// `/sbin/init` -> `/bin/busybox`). A small bound prevents loops.
const FOLLOW_LINK_DEPTH: usize = 8;

/// Core process spawn. Returns `None` if `args[0]` cannot be found on the
/// process's root or the initramfs (instead of panicking) so optional
/// processes can be skipped gracefully. `boot_work` runs the one-time boot
/// setup (network init, fstab mount, console clear) and must happen once.
///
/// `foreground` seeds the VT's foreground process group with this process so an
/// interactive shell does not conclude it is backgrounded. It must be `false`
/// for a non-interactive PID 1 init that shares vt 0 with the primary shell —
/// otherwise init would steal vt 0's foreground group and wedge that shell on
/// `SIGTTIN`.
// Process bring-up genuinely needs all of these inputs (identity, filesystem,
// controlling terminal, job-control role); grouping them into a struct would
// only move the argument list, not reduce it.
#[allow(clippy::too_many_arguments)]
fn spawn(
    args: Vec<String>,
    envs: Vec<String>,
    rootfs: Arc<dyn FileSystem>,
    vt: usize,
    shared_root: Option<Arc<dyn INode>>,
    pid: KoID,
    boot_work: bool,
    foreground: bool,
) -> Option<Arc<Process>> {
    // Tell the process which VT it runs on. /etc/profile uses this to run the
    // serial terminal-size probe only where a reply can arrive (the serial
    // mirror follows the ACTIVE VT, i.e. VT 0 at boot) — on VTs 1..N the probe
    // could never be answered and blocked each shell ~0.3 s at boot.
    let mut envs = envs;
    envs.push(alloc::format!("ECLIPSE_VT={}", vt));
    info!(
        "spawn pid={} vt={}: args={:?}, envs={:?}",
        pid, vt, args, envs
    );
    if boot_work {
        linux_object::net::init();
        hunter::init();
    }
    let job = zircon_object::task::ROOT_JOB.clone();
    let proc =
        Process::create_linux(&job, rootfs.clone(), vt, shared_root, pid).expect("create_linux");
    let thread = Thread::create_linux(&proc).expect("create_linux thread");
    // Use the pivoted root (e.g. installed btrfs/ext2), not the initramfs SFS passed in.
    let root_inode = proc.linux().root_inode().clone();
    let loader = LinuxElfLoader {
        syscall_entry: kernel_hal::context::syscall_entry as *const () as usize,
        stack_pages: USER_STACK_PAGES,
        root_inode: root_inode.clone(),
        // A process the kernel starts itself was asked for by nobody, so
        // there is no less-privileged caller whose environment it has to
        // distrust: `AT_SECURE` is 0 and the four ids are the ones the
        // freshly created process actually has.
        identity: proc.linux().aux_identity(false),
    };

    // Follow symlinks so a symlinked entry point (e.g. /sbin/init ->
    // /bin/busybox) loads the real ELF instead of the symlink's path text.
    let inode = match root_inode.lookup_follow(&args[0], FOLLOW_LINK_DEPTH) {
        Ok(inode) => inode,
        Err(e) => match rootfs
            .root_inode()
            .lookup_follow(&args[0], FOLLOW_LINK_DEPTH)
        {
            Ok(inode) => inode,
            Err(e2) => {
                warn!(
                    "process {:?} not found on root ({:?}) or initramfs ({:?}); skipping",
                    args[0], e, e2
                );
                return None;
            }
        },
    };
    let vmo = inode
        .read_as_vmo_cached()
        .unwrap_or_else(|e| panic!("failed to read process {:?}: {:?}", args[0], e));
    let path = args[0].clone();

    // hunter P8: verify binary integrity + path policy using a full 64-byte
    // header (e_ident/e_type/e_machine), matching the runtime execve gate
    // rather than the old 4-byte magic-only check.
    let mut header = [0u8; 64];
    let _ = vmo.read(0, &mut header);
    if !hunter::check_elf_binary(&path, &header) {
        warn!("spawn: binary {:?} blocked by hunter security policy", path);
        return None;
    }

    // Boot UX: clear to black right before the first graphic-console output
    // (prompt). No-op when graphic mode is disabled. Only when this process
    // performs the one-time boot setup (the primary terminal).
    if boot_work {
        kernel_hal::console::request_clear_graphic_on_next_write();
    }

    let pg_token = kernel_hal::vm::current_vmtoken();
    debug!("current pgt = {:#x}", pg_token);
    //调用zircon-object/src/task/thread.start设置好要执行的thread
    // Likewise, a malformed/incompatible ELF for the base program must fall
    // back rather than panic the kernel.
    let (entry, sp, initial_brk, execute_path, abi) =
        match loader.load(&proc.vmar(), &vmo, args.clone(), envs, path) {
            Ok(loaded) => loaded,
            Err(e) => {
                warn!("spawn: failed to load {:?}: {:?}; skipping", args[0], e);
                return None;
            }
        };
    // Record the ABI personality the loader detected so the trap handler routes
    // this process's syscalls (and the FreeBSD carry-flag return convention)
    // correctly.
    proc.linux().set_abi(abi);
    proc.linux().set_execute_path(&execute_path);
    proc.linux().set_cmdline(args);
    proc.linux().set_brk(initial_brk);
    proc.set_name(comm_from_path(&execute_path));

    // Make this process the foreground process group of its own tty before it
    // ever runs in user mode. `getpgid` reports each process's pgrp as its own
    // pid, so seeding the VT's foreground pgrp to `pid` makes
    // `tcgetpgrp == getpgrp`; otherwise it starts at 0, the shell concludes it
    // is a background job and spins on `kill(0, SIGTTIN)` (a tight, CPU-burning
    // enter_uspace loop that was previously hidden behind the handle_signal
    // self-deadlock). Seed it only here — after every fallible step has
    // succeeded — so a process that fails to load never leaves the VT pointing
    // at a pid that never started. Skipped for a non-interactive init that
    // shares vt 0 with the primary shell (see `foreground` above).
    if foreground {
        linux_object::fs::stdio::set_vt_foreground_pgrp(vt, pid as i32);
    }

    // Entry register convention differs by ABI. FreeBSD/amd64 passes a pointer
    // to argc in %rdi and starts on an 8-mod-16 stack (`exec_setregs`,
    // sys/amd64/amd64/exec_machdep.c); Linux simply points %rsp at argc and
    // leaves the argument registers zero. `start_with_entry(entry, stack, arg1,
    // ..)` maps `stack` -> %rsp and `arg1` -> %rdi.
    #[cfg(target_arch = "x86_64")]
    let (start_sp, arg1) = if abi == Abi::Freebsd {
        (((sp - 8) & !0xf) + 8, sp)
    } else {
        (sp, 0)
    };
    #[cfg(not(target_arch = "x86_64"))]
    let (start_sp, arg1) = {
        let _ = abi;
        (sp, 0)
    };
    thread
        .start_with_entry(entry, start_sp, arg1, 0, thread_fn)
        .expect("failed to start main thread");

    // Mount the non-root /etc/fstab entries (/boot/efi vfat, /home, …) as a
    // deferred kernel task, off the synchronous boot path: the blocking
    // block-device I/O would otherwise risk stalling boot before the shell
    // shows. Done once, by whoever performs the boot work.
    if boot_work {
        kernel_hal::thread::spawn(async {
            linux_object::fs::mount_fstab_deferred();
        });
        // Periodic load-average sampler (Linux samples from the scheduler
        // tick): keeps /proc/loadavg honest by sampling the run queue at
        // instants uncorrelated with whoever reads it.
        kernel_hal::thread::spawn(linux_object::loadavg::sampler_task());
    }

    Some(proc)
}

fn thread_fn(thread: CurrentThread) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
    Box::pin(run_user(thread))
}

/// Runs a syscall future, marking the thread [`ThreadState::Blocked`] for as
/// long as it stays parked.
///
/// The state is what `/proc/<pid>/status` and `ZX_INFO_THREAD` report, and
/// without this every thread looks `Running` — including one asleep in `read`,
/// which makes a deadlock indistinguishable from a spin.
///
/// The fast path costs nothing. A syscall that completes on its first poll —
/// nearly all of them — never touches the thread's lock; only one that
/// genuinely parks pays for the two transitions, and it is about to sleep
/// anyway.
struct MarkBlocked<'a, F> {
    fut: F,
    thread: &'a CurrentThread,
    /// Whether we have marked the thread blocked and owe it an unmark.
    marked: bool,
}

impl<'a, F> MarkBlocked<'a, F> {
    fn new(thread: &'a CurrentThread, fut: F) -> Self {
        Self {
            fut,
            thread,
            marked: false,
        }
    }
}

impl<F: Future> Future for MarkBlocked<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> core::task::Poll<F::Output> {
        // SAFETY: structural pinning of `fut`. `self` is pinned, `fut` is
        // never moved out or replaced, and neither `thread` (a shared
        // reference) nor `marked` (a bool) requires pinning.
        let this = unsafe { self.get_unchecked_mut() };
        if this.marked {
            // Woken: running again before the inner future observes anything.
            // A no-op if something more specific (a futex wait, say) owns the
            // state now — `set_blocked` defers to it.
            this.thread.set_blocked(false);
            this.marked = false;
        }
        // SAFETY: as above — `fut` lives in a pinned `Self` and stays put.
        let fut = unsafe { Pin::new_unchecked(&mut this.fut) };
        let out = fut.poll(cx);
        if out.is_pending() {
            // Only own the state if nothing more specific already does.
            this.marked = this.thread.set_blocked(true);
        }
        out
    }
}

impl<F> Drop for MarkBlocked<'_, F> {
    fn drop(&mut self) {
        // Dropped while parked (a kill, say): do not leave the thread marked
        // blocked forever. `set_blocked` refuses to disturb a dying thread.
        if self.marked {
            self.thread.set_blocked(false);
        }
        // `set_blocked` is a no-op unless we still own the generic state, so
        // a thread torn down mid-wait is never dragged back to `Running`.
    }
}

/// Where an address lives, for a fault report: the name of the object backing
/// it, the offset *within its mapping*, and the mapping's bounds.
///
/// A bare `pc` says nothing when every library is mapped at a runtime-chosen
/// base -- a fault in a stripped `libxul.so` looked exactly like a fault in
/// the kernel's own idea of nowhere.
///
/// Read the offset for what it is: a file mapping here is backed by a VMO that
/// already starts at the mapped file offset (see `get_vmo` in
/// `linux-object/src/fs/file.rs`), so this counts from the start of THAT
/// mapping, not from the start of the file. A shared library is mapped one
/// PT_LOAD at a time, so for a fault in library code it is an offset into that
/// segment. Turning it into an address `addr2line -e` understands means adding
/// the segment's own base: match the mapping's size against `readelf -lW` to
/// find which PT_LOAD it is, then add that header's page-aligned `VirtAddr`.
/// Claiming otherwise sent a first attempt at this to an address 0x263d000
/// short, where `addr2line` and `readelf -r` both found nothing.
///
/// This is only built on the fatal path, where the process is already being
/// killed, so walking the mapping list costs nothing that matters.
fn describe_addr(vmar: &Arc<VmAddressRegion>, addr: usize) -> String {
    if addr == 0 {
        return String::from("null");
    }
    for m in vmar.mappings_dump() {
        if (m.start..m.end).contains(&addr) {
            let offset = m.file_offset + (addr - m.start);
            let name: &str = if m.name.is_empty() { "anon" } else { &m.name };
            return alloc::format!("{name}+{offset:#x} in map {:#x}-{:#x}", m.start, m.end);
        }
    }
    String::from("unmapped")
}

/// The permissions the kernel has on RECORD for the faulting page, and the
/// ones actually in the hardware page table.
///
/// An `ACCESS_DENIED` page fault has two very different causes and the message
/// could not tell them apart. Either the mapping never carried the access —
/// `perm` is missing the bit, which is a question about the `mmap`/`mprotect`
/// that built it — or the record allows it and the page table does not, which
/// is ours: a fault path that did not install what the record promised. The
/// two have opposite fixes, and guessing between them costs a boot on hardware
/// each time. Printing both settles it in the line that already reports the
/// fault.
///
/// Per PAGE, like `addr_is_executable` and for the same reason: a partial
/// `mprotect` leaves mixed permissions inside one mapping.
fn describe_fault_perms(vmar: &Arc<VmAddressRegion>, addr: usize) -> String {
    let perm = vmar
        .get_mapping_flags(addr)
        .map(|f| alloc::format!("{:?}", f))
        .unwrap_or_else(|e| alloc::format!("{:?}", e));
    let pte = vmar
        .get_vaddr_flags(addr)
        .map(|f| alloc::format!("{:?}", f))
        .unwrap_or_else(|e| alloc::format!("{:?}", e));
    alloc::format!("perm={perm} pte={pte}")
}

/// Whether `addr` lies on a mapped, executable user page.
///
/// Per PAGE, not per mapping row: a partial `mprotect` leaves mixed
/// permissions inside one mapping and `MappingDump` only carries the first
/// page's flags.
fn addr_is_executable(vmar: &Arc<VmAddressRegion>, addr: usize) -> bool {
    vmar.find_mapping(addr)
        .and_then(|m| m.get_flags(addr).ok())
        .is_some_and(|f| f.contains(kernel_hal::MMUFlags::EXECUTE))
}

/// Scan the user stack from `sp` upwards for words that point into an
/// executable page and render each as `stack[+off] = addr [lib+offset]`,
/// `offset` being the FILE offset (`MappingDump::file_offset`) so it feeds
/// `addr2line` directly.
///
/// Reads go through the address space's VMOs, so an unmapped `sp` (stack
/// overflow, corrupted register) yields an empty trace instead of a kernel
/// fault. An unaligned `sp` is a legal state on x86-64 (any `sub rsp` before
/// the fault) and is aligned down rather than rejected. Executability is
/// tested on the candidate's own page: a partial `mprotect` leaves mixed
/// per-page permissions inside one mapping, and the dump row only carries
/// the first page's. The scan is bounded both in bytes looked at and in
/// lines printed so a report stays a screenful on the serial console.
fn user_stack_backtrace(vmar: &Arc<VmAddressRegion>, sp: usize) -> Vec<String> {
    /// Bytes of stack to look at (128 words on a 64-bit target).
    const SCAN_BYTES: usize = 1024;
    /// Cap on rendered frames.
    const MAX_FRAMES: usize = 24;
    let mut out = Vec::new();
    if sp == 0 {
        return out;
    }
    let word = core::mem::size_of::<usize>();
    let sp = sp & !(word - 1);
    let mut buf = [0u8; SCAN_BYTES];
    let got = match vmar.read_memory(sp, &mut buf) {
        Ok(n) => n,
        Err(_) => return out,
    };
    let maps = vmar.mappings_dump();
    let executable = |addr: usize| addr_is_executable(vmar, addr);
    for (i, chunk) in buf[..got].chunks_exact(word).enumerate() {
        let mut raw = [0u8; core::mem::size_of::<usize>()];
        raw.copy_from_slice(chunk);
        let addr = usize::from_ne_bytes(raw);
        if addr == 0 {
            continue;
        }
        let Some(m) = maps.iter().find(|m| (m.start..m.end).contains(&addr)) else {
            continue;
        };
        if !executable(addr) {
            continue;
        }
        let offset = m.file_offset + (addr - m.start);
        let name: &str = if m.name.is_empty() { "anon" } else { &m.name };
        out.push(alloc::format!(
            "stack[+{:#x}] = {addr:#x} [{name}+{offset:#x}]",
            i * word
        ));
        if out.len() >= MAX_FRAMES {
            break;
        }
    }
    out
}

/// The function of a new thread.
///
/// loop:
/// - wait for the thread to be ready
/// - get user thread context
/// - enter user mode
/// - handle trap/interrupt/syscall according to the return value
/// - return the context to the user thread
async fn run_user(thread: CurrentThread) {
    kernel_hal::thread::set_current_thread(Some(thread.inner()));
    loop {
        // wait
        let mut ctx = thread.wait_for_run().await;
        if thread.state() == ThreadState::Dying {
            break;
        }

        // check the signal and handle
        //
        // Bind the result to a local FIRST so the `lock_linux()` temporary guard
        // is dropped at the end of this statement. Inlining it into the `if let`
        // scrutinee keeps the guard alive for the whole `if let` body (Rust
        // temporary scoping), and `handle_signal` re-locks the same per-thread
        // `LinuxThread` mutex — a self-deadlock on the non-reentrant TicketMutex.
        // It fires whenever a thread has a pending signal (e.g. the job-control
        // SIGTTIN a shell sends itself), wedging that core in an interrupts-off
        // spin forever: the silent multi-core busy/heat (and a hang risk).
        let pending_signal = thread.inner().lock_linux().handle_signal();
        if let Some((signal, sigmask)) = pending_signal {
            ctx = handle_signal(&thread, ctx, signal, sigmask);
        }
        if thread.state() == ThreadState::Dying {
            break;
        }
        // Job-control stop: do not re-enter uspace until SIGCONT (or death).
        if thread
            .proc()
            .try_linux()
            .map(|lp| lp.is_job_stopped())
            .unwrap_or(false)
        {
            linux_object::process::wait_while_job_stopped(thread.proc()).await;
            if thread.state() == ThreadState::Dying {
                break;
            }
            // Drop into the next loop iteration so a pending SIGCONT handler
            // (or another signal) is considered before enter_uspace.
            continue;
        }

        // run
        trace!(
            "go to user: tid = {} pc = {:x} sp = {:x}",
            thread.id(),
            ctx.get_field(UserContextField::InstrPointer),
            ctx.get_field(UserContextField::StackPointer)
        );
        trace!("ctx before enter: {:#x?}", ctx);
        // Time the user-mode slice and attribute it to the thread: this is what
        // getrusage(2)/times(2) report as utime (kernel-side time is accounted
        // separately, per syscall, by linux_object::perf). checked_sub because a
        // cross-CPU migration over the entry can see unsynchronised TSCs.
        let uspace_start = kernel_hal::timer::timer_now();
        ctx.enter_uspace();
        let user_ns = kernel_hal::timer::timer_now()
            .checked_sub(uspace_start)
            .unwrap_or_default()
            .as_nanos();
        thread.time_add(user_ns);
        debug!(
            "back from user: tid = {} pc = {:x} trap reason = {:?}",
            thread.id(),
            ctx.get_field(UserContextField::InstrPointer),
            ctx.trap_reason(),
        );
        trace!("ctx = {:#x?}", ctx);
        // handle trap/interrupt/syscall
        if let Err(err) = MarkBlocked::new(&thread, handle_user_trap(&thread, ctx)).await {
            thread.exit_linux(err as i32);
        }
        if thread.state() == ThreadState::Dying {
            break;
        }
    }
    kernel_hal::thread::set_current_thread(None);
}

fn handle_signal(
    thread: &CurrentThread,
    mut ctx: Box<UserContext>,
    signal: Signal,
    sigmask: Sigset,
) -> Box<UserContext> {
    let user_sp = ctx.get_field(UserContextField::StackPointer);
    let user_pc = ctx.get_field(UserContextField::InstrPointer);
    let action = thread.proc().linux().signal_action(signal);
    // Handle default/ignore actions without entering a handler.
    if action.handler == SIG_IGN {
        // SIGCONT still resumes a stopped process even when ignored.
        if signal == Signal::SIGCONT {
            let proc = thread.proc();
            if let Some(lp) = proc.try_linux() {
                lp.job_continue(proc);
            }
        }
        thread.inner().lock_linux().handling_signal = None;
        return ctx;
    }
    if signal == Signal::SIGSTOP
        || (action.handler == SIG_DFL
            && matches!(signal, Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU))
    {
        let proc = thread.proc();
        if let Some(lp) = proc.try_linux() {
            lp.job_stop(proc, signal as u8);
        }
        thread.inner().lock_linux().handling_signal = None;
        return ctx;
    }
    if signal == Signal::SIGCONT {
        let proc = thread.proc();
        if let Some(lp) = proc.try_linux() {
            lp.job_continue(proc);
        }
        if action.handler == SIG_DFL {
            thread.inner().lock_linux().handling_signal = None;
            return ctx;
        }
        // Custom handler: fall through after resuming.
    }
    if action.handler == SIG_DFL {
        // Per-signal default disposition. Linux's default for the job-control and
        // a few status signals is NOT to terminate: SIGCHLD/SIGURG/SIGWINCH are
        // ignored. Stop/continue are handled above. Everything else still
        // terminates, as before.
        match signal {
            Signal::SIGCHLD | Signal::SIGURG | Signal::SIGWINCH => {
                trace!(
                    "default-ignore signal {:?} for pid={}",
                    signal,
                    thread.proc().id()
                );
                thread.inner().lock_linux().handling_signal = None;
                return ctx;
            }
            _ => {}
        }
        // Linux never applies a default terminate/core action to init: a
        // signal PID 1 has no handler for is simply discarded. Here it killed
        // the supervisor (`kill -HUP 1` from a root shell, or any signal that
        // races init's handler installation at boot), leaving the system
        // running with no PID 1 — no service restarts, no shutdown path.
        if thread.proc().id() == linux_object::process::INIT_PID {
            warn!(
                "signal {:?} for init (pid 1) has no handler; discarded (Linux semantics)",
                signal
            );
            thread.inner().lock_linux().handling_signal = None;
            return ctx;
        }
        // Resolve addresses back to "<file>+<offset>" through the process's own
        // mappings. A bare `pc=0x499f8c` is unusable — the same number means a
        // different function in every process — while `libglib-2.0.so.0+0x1f8c`
        // names the guilty binary and can be fed straight to `addr2line` or
        // `objdump -d` on the host's copy of that file. File-backed VMOs carry
        // their path as the kernel-object name (see `File::get_vmo`).
        let maps = thread.proc().vmar().mappings_dump();
        let resolve = |addr: usize| -> Option<String> {
            let m = maps.iter().find(|m| addr >= m.start && addr < m.end)?;
            let off = m.file_offset + (addr - m.start);
            Some(if m.name.is_empty() {
                alloc::format!("<anon:{}>+{:#x}", m.vmo_id, off)
            } else {
                alloc::format!("{}+{:#x}", m.name, off)
            })
        };
        // Record the death in the dmesg ring at error! (survives LOG=error): a
        // process that dies on a default-disposition signal — apk killed by
        // SIGPIPE when a fetch connection resets, Xorg aborting in early init —
        // otherwise vanishes with no trace at all, and a `Done(139)`/silent exit
        // gives no clue which signal took it down.
        error!(
            "[exit] pid={} ({}) killed by signal {:?} ({}, shells print {}) at pc={:#x} \
             [{}] (default disposition)",
            thread.proc().id(),
            thread.proc().name(),
            signal,
            signal as i32,
            128 + signal as i32,
            user_pc,
            resolve(user_pc).unwrap_or_else(|| String::from("unmapped")),
        );
        // For a crash/abort signal, dump the top of the user stack: any word
        // that lands in the process's own code is a return address, so the
        // abort()/assert() CALLER chain can be read off by mapping these back
        // against the binary — turning a bare 'killed by SIGABRT' into "which
        // function aborted" without a debugger. Read through the process page
        // table's physmap image, exactly like the page-fault code-bytes dump.
        #[cfg(not(feature = "libos"))]
        if matches!(
            signal,
            Signal::SIGABRT
                | Signal::SIGSEGV
                | Signal::SIGILL
                | Signal::SIGBUS
                | Signal::SIGFPE
                | Signal::SIGTRAP
        ) {
            use kernel_hal::vm::{GenericPageTable, PageTable};
            let rsp = ctx.get_field(UserContextField::StackPointer);
            let pt = PageTable::from_current();
            // 8-aligned u64s never cross a page boundary (4096 % 8 == 0), so a
            // single physmap read per word is safe.
            let rd = |va: usize| -> Option<u64> {
                pt.query(va & !0xfff).ok().map(|(pa, _, _)| {
                    let kv = 0xffff_8000_0000_0000usize + (pa & !0xfff) + (va & 0xfff);
                    unsafe { core::ptr::read_volatile(kv as *const u64) }
                })
            };
            let base = rsp & !0x7;
            let mut words = alloc::vec::Vec::new();
            for i in 0..96usize {
                match rd(base + i * 8) {
                    Some(w) => words.push(w),
                    None => break,
                }
            }
            error!(
                "[crash] pid={} SIG={:?} rsp={:#x} stack[0..{}]={:#x?}",
                thread.proc().id(),
                signal,
                rsp,
                words.len(),
                words
            );
            // Every stack word that points into an EXECUTABLE mapping is a
            // return address, so this is the abort()/assert() caller chain,
            // in order, already resolved to file+offset. Duplicates are kept:
            // repeats are how a recursive/looping frame shows itself.
            let mut frames = alloc::vec::Vec::new();
            for w in words.iter() {
                let a = *w as usize;
                if let Some(m) = maps.iter().find(|m| a >= m.start && a < m.end) {
                    if m.flags.contains(zircon_object::vm::MMUFlags::EXECUTE) {
                        frames.push(resolve(a).unwrap_or_else(|| alloc::format!("{:#x}", a)));
                    }
                }
            }
            error!(
                "[crash-bt] pid={} {} candidate return addresses (innermost first): {:?}",
                thread.proc().id(),
                frames.len(),
                frames
            );
        }
        // `code` above is the number a SHELL prints for a signal death
        // (128 + n); the STATUS a `wait` reads is a different shape entirely.
        // Store "killed by this signal", so `WIFSIGNALED` can be true.
        thread
            .proc()
            .exit(linux_object::process::exit_code_killed_by(signal as u8));
        return ctx;
    }
    // The set the handler runs under: what the thread already had blocked,
    // plus the `sa_mask` this action asked for, plus the signal itself unless
    // `SA_NODEFER`. `sa_mask` is the whole reason a handler can safely touch
    // data the signal also touches, and it was stored by the syscall and read
    // by nobody -- so a handler that asked for SIGTERM to be held off while it
    // ran was not given that, and a `sigprocmask(SIG_BLOCK, NULL, &old)`
    // inside it reported the mask from before the signal. `sigmask` below is
    // the one the frame carries back on `sigreturn`, and stays what it was.
    {
        // `thread.inner()` is a temporary, so the guard needs a binding that
        // outlives it; the one-liners elsewhere get away with it only because
        // the whole thing is a single statement.
        let inner = thread.inner();
        let mut linux = inner.lock_linux();
        let live = linux.signal_mask();
        linux.set_signal_mask(action.handler_mask(live, signal));
    }
    // `SA_RESETHAND` puts the disposition back to `SIG_DFL` BEFORE the handler
    // runs: that is what makes a one-shot handler one-shot, and what lets a
    // handler for a fault re-raise it and die the way it would have. Unread,
    // the handler stayed installed for good.
    if action.resets_to_default() {
        thread.proc().linux().set_signal_action(
            signal,
            SignalAction {
                handler: SIG_DFL,
                ..action
            },
        );
    }
    // What the sender left for the handler: `si_pid`/`si_uid` of a `kill`,
    // the child and its status of a `SIGCHLD`. Bare (number only) otherwise.
    let signal_info = thread.inner().lock_linux().take_siginfo(signal);
    let signal_context = SignalUserContext {
        sig_mask: sigmask,
        context: MachineContext::new(user_pc),
        ..Default::default()
    };
    // Where the frame goes. `SA_ONSTACK` is the reason `sigaltstack(2)` exists:
    // a handler for the signal a blown stack raises cannot run on that stack.
    // It was stored by the syscall and read by nobody, so every handler ran on
    // the interrupted stack -- including the SIGSEGV handler Rust's runtime and
    // glibc install precisely to survive one.
    let alt = thread.inner().lock_linux().signal_alternate_stack;
    let want_alt_stack = action.flags.contains(SignalActionFlags::ONSTACK);
    let frame_top = sigframe_top(user_sp, &alt, want_alt_stack);
    // And whether it can go there at all. `sp` is whatever the interrupted
    // program left in the stack register; the pushes below go through a raw
    // pointer with the user page table live, so an unmapped or kernel address
    // here is the kernel writing where userspace told it to. Linux checks the
    // same range with `access_ok()` and answers a bad one with a forced
    // SIGSEGV, which is what a process gets for handing the kernel a stack
    // pointer it cannot use.
    let frame_size = RED_ZONE_MAX_SIZE
        + core::mem::size_of::<SigInfo>()
        + core::mem::size_of::<SignalUserContext>()
        + core::mem::size_of::<usize>();
    let usable = frame_top.filter(|top| {
        let ranges: Vec<(usize, usize, bool)> = thread
            .proc()
            .vmar()
            .mappings_dump()
            .iter()
            .map(|m| {
                (
                    m.start,
                    m.end,
                    m.flags.contains(zircon_object::vm::MMUFlags::WRITE),
                )
            })
            .collect();
        frame_fits_writable(&ranges, top.saturating_sub(frame_size), frame_size)
    });
    let mut sp = match usable {
        Some(sp) => sp,
        None => {
            error!(
                "[exit] pid={} nowhere to put the {:?} frame: sp={:#x} is not writable user memory (forcing SIGSEGV)",
                thread.proc().id(),
                signal,
                user_sp,
            );
            thread
                .proc()
                .exit(linux_object::process::exit_code_killed_by(
                    Signal::SIGSEGV as u8,
                ));
            return ctx;
        }
    };
    // Always use the 3-argument SA_SIGINFO calling convention; extra args are harmless
    // for 1-argument handlers on SysV ABIs, and avoids crashing when flags are unset.
    sp = push_stack(sp & !0xF, signal_info); // & !0xF for 16 bytes aligned
    let siginfo_ptr = sp;
    sp = push_stack(sp & !0xF, signal_context);
    let uctx_ptr = sp;
    // backup current context
    thread.backup_context(*ctx, siginfo_ptr, uctx_ptr);
    // set user return address as `action.restorer`
    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            sp = push_stack::<usize>(sp & !0xF, action.restorer);
        } else {
            ctx.set_ra(action.restorer);
        }
    }
    // set trapframe
    ctx.setup_uspace(
        action.handler,
        sp,
        &[signal as usize, siginfo_ptr, uctx_ptr],
    );
    ctx
}

/// The part of a user-fault report that does not depend on the fault kind:
/// registers, the faulting instruction's bytes and a user-stack backtrace.
/// Shared by the page-fault (SIGSEGV) and CPU-exception (#GP, #DE, ...)
/// paths so a `movaps` on a misaligned stack or a non-canonical pointer --
/// which raise #GP, not a page fault -- get the same forensics instead of a
/// bare `[exit] killed by signal SIGSEGV`. `pc_is_data_fault` is false when
/// `pc` may itself be the bad address (an instruction-fetch fault, or a #GP
/// raised after a jump to a non-canonical RIP).
fn dump_user_fault_context(
    thread: &CurrentThread,
    vmar: &Arc<VmAddressRegion>,
    pc: usize,
    pc_is_data_fault: bool,
) {
    // The faulting address says WHAT was touched; the registers say
    // WHICH operand carried it. A write to 0 is a null pointer, and
    // the register holding 0 here is the one whose value never got
    // filled in -- knowing it is `rdi` (an argument, a `this`) or
    // `rax` (a return value that should have been checked) is the
    // difference between a guess and a direction.
    if let Ok(regs) = thread.with_context(|ctx| alloc::format!("{:x?}", ctx.general())) {
        error!("  regs: {}", regs);
    }
    // And the instruction itself. Several registers are usually
    // zero at a fault and only the opcode says which one was being
    // dereferenced -- with `rax`, `rdx`, `rbp` and `r9` all zero
    // there is no reading the faulting operand off the dump alone.
    //
    // Only safe when `pc` is known-good memory: reading it is an
    // unchecked `copy_from_nonoverlapping` with NO fault fixup, so a
    // `pc` that is itself the bad address turns a recoverable user
    // SIGSEGV into an unresolved KERNEL page fault and the
    // isolate/kill cascade. Every desktop process jumping to one bad
    // address once took the kernel down through this read.
    //
    // Two independent guards, because either alone is insufficient:
    //  * `pc_is_data_fault` -- on an INSTRUCTION-FETCH page fault
    //    (`EXECUTE` flag) `pc` IS the unmapped address that faulted.
    //  * the mapping test -- a CPU exception carries no fault address
    //    and no access flags at all, so a #GP raised after a jump to a
    //    non-canonical or unmapped RIP reaches here with the caller
    //    unable to tell. `UserInPtr`'s own check is a bounds check, not
    //    a mapping check (and under `libos` it accepts every address),
    //    so the page must be confirmed mapped and executable here.
    // The address is in the report above (`pc=... [unmapped]`) either
    // way, so skipping the bytes loses nothing.
    if pc_is_data_fault && addr_is_executable(vmar, pc) {
        if let Ok(bytes) = kernel_hal::user::UserInPtr::<u8>::from(pc).read_array(16) {
            let mut hex = String::new();
            for b in &bytes {
                hex.push_str(&alloc::format!("{b:02x} "));
            }
            error!("  code: {}", hex.trim_end());
        }
    }
    // Finally, WHO got here. `pc` names the function that faulted
    // (`wl_list_remove`, `memcpy`, ...) but those are leaves called
    // from hundreds of places; the caller is what points at the
    // bug. Scan the live stack above `sp` for words that land in an
    // executable mapping and print them as `lib+offset`: a poor
    // man's backtrace that needs no frame pointers and no DWARF
    // (`addr2line -e <lib> <offset>` / `nm -D` on the user side
    // turns each into a function). Stale return addresses from
    // earlier calls show up too, so read it as a set of candidate
    // callers, innermost first, not as an exact chain.
    //
    // Read through the VMO (`read_memory`), never by dereferencing
    // the user pointer: an unmapped or not-yet-committed stack page
    // would otherwise re-fault from kernel mode with no fixup, the
    // same cascade the `code:` read above guards against.
    let sp = thread
        .with_context(|ctx| ctx.get_field(UserContextField::StackPointer))
        .unwrap_or(0);
    for line in user_stack_backtrace(vmar, sp) {
        error!("  {}", line);
    }
}

/// Deliver a *synchronous* fault signal (SIGSEGV / SIGBUS / SIGILL / SIGFPE).
///
/// A faulting instruction is re-executed when the thread returns to user mode.
/// If the fault signal is merely queued but cannot actually be delivered — it is
/// blocked by the signal mask, already pending (we are re-faulting on the same
/// instruction), or a handler is already mid-flight — the thread loops on the
/// same fault forever and starves every CPU (observed as a 0x9b SIGSEGV storm,
/// ~71k faults, that hung the whole system under network load and stalled
/// downloads like `apk update`).
///
/// Mirror Linux `force_sig`: in any of those un-deliverable cases, or when the
/// disposition is the default/ignore action, terminate the process. Only a
/// not-yet-faulted custom handler gets to run, exactly once — a fault inside it
/// sets `handling_signal`, so the next fault terminates instead of looping.
/// The exit code of a process a fault kills: a death by `signal`, which
/// `wait` reports as `WIFSIGNALED`. `128 + signo` here was a shell's number,
/// not a status word: `bash` printed nothing for a segfault and `system()`
/// saw a program that had exited with 139.
fn undeliverable_fault_exit_code(signal: Signal) -> i64 {
    linux_object::process::exit_code_killed_by(signal as u8)
}

/// What a page fault the process could not have caused on purpose is told
/// about itself (`force_sig_fault`): `SEGV_MAPERR` when nothing is mapped at
/// the address, `SEGV_ACCERR` when something is but not for this access, and
/// `si_addr` in both cases.
fn page_fault_info(vaddr: usize, err: ZxError) -> SigInfo {
    let code = match err {
        ZxError::NOT_FOUND => SignalCode::SEGV_MAPERR,
        _ => SignalCode::SEGV_ACCERR,
    };
    SigInfo::fault(Signal::SIGSEGV, code, vaddr)
}

fn force_fault_signal(thread: &CurrentThread, signal: Signal, info: SigInfo) {
    let action = thread.proc().linux().signal_action(signal);
    let inner = thread.inner();
    let undeliverable = {
        let linux = inner.lock_linux();
        linux.signal_mask().contains(signal)
            || linux.handling_signal.is_some()
            || linux.signals.contains(signal)
            || action.handler == SIG_DFL
            || action.handler == SIG_IGN
    };
    if undeliverable {
        // error!, not warn!, and for the same reason the sibling path above
        // spells out: this is a process DYING, and at `LOG=error` — what a
        // desktop actually boots with — a warn! line is not printed at all.
        // So the loudest case of all, a fault the process had no handler for,
        // was the one that vanished without a trace: the program's window
        // disappears, nothing reaches the console, and the only evidence left
        // is a shell exit status nobody is watching. That is exactly how
        // "glxgears opens for under a second and then closes with no apparent
        // errors" looks from the outside.
        //
        // The process name goes in too, as it does above: a bare pid says
        // nothing when the question is *which* program just died.
        error!(
            "[exit] pid={} ({}) killed by fault signal {:?} ({}) — undeliverable, terminating",
            thread.proc().id(),
            thread.proc().name(),
            signal,
            signal as i32,
        );
        thread.proc().exit(undeliverable_fault_exit_code(signal));
    } else {
        // Deliverable custom handler: unblock so it cannot be deferred and queue
        // it for the next pass of the run loop, with where and why.
        let mut linux = inner.lock_linux();
        let mut unblock = Sigset::empty();
        unblock.insert(signal);
        linux.unblock_signals(&unblock);
        linux.queue_signal(signal, Some(info));
    }
}

/// Push a object onto stack
/// # Safety
///
/// This function is handling a raw pointer to the top of the stack .
pub fn push_stack<T>(stack_top: usize, val: T) -> usize {
    unsafe {
        let stack_top = (stack_top as *mut T).sub(1);
        *stack_top = val;
        stack_top as usize
    }
}

macro_rules! run_with_irq_enable {
    ($($body:tt)*) => {
        {
            intr_on();
            let ret = { $($body)* };
            kernel_hal::interrupt::intr_off();
            ret
        }
    };
}

async fn handle_user_trap(thread: &CurrentThread, mut ctx: Box<UserContext>) -> ZxResult {
    let reason = ctx.trap_reason();
    if let TrapReason::Syscall = reason {
        let num = syscall_num(&ctx);
        let args = syscall_args(&ctx);
        ctx.advance_pc(reason);
        thread.put_context(ctx);
        let mut syscall = linux_syscall::Syscall {
            thread,
            thread_fn,
            syscall_entry: kernel_hal::context::syscall_entry as *const () as usize,
        };
        // FreeBSD-personality processes speak the FreeBSD/amd64 ABI: different
        // syscall numbers and a carry-flag return convention (result in %rax,
        // secondary in %rdx, error signalled by CF with the errno in %rax). The
        // whole path is amd64-only, matching the ABI it implements.
        #[cfg(target_arch = "x86_64")]
        if thread.proc().linux().abi() == Abi::Freebsd {
            trace!("FreeBSD syscall: {} {:x?}", num, args);
            let ret = run_with_irq_enable! {
                syscall.bsd_syscall(num, args).await
            };
            thread.with_context(|ctx| {
                let g = ctx.general_mut();
                g.rax = ret.rax;
                g.rdx = ret.rdx;
                if ret.error {
                    g.rflags |= 1; // set carry: error
                } else {
                    g.rflags &= !1; // clear carry: success
                }
            })?;
            return Ok(());
        }
        trace!("Syscall: {} {:x?}", num as u32, args);
        let ret = run_with_irq_enable! {
            syscall.syscall(num as u32, args).await as usize
        };
        trace!("Syscall ret: {} -> {:x}", num as u32, ret);
        thread.with_context(|ctx| ctx.set_field(UserContextField::ReturnValue, ret))?;
        return Ok(());
    }

    thread.put_context(ctx);

    let pid = thread.proc().id();
    match reason {
        TrapReason::Interrupt(vector) => {
            kernel_hal::interrupt::handle_irq(vector);
            #[cfg(not(feature = "libos"))]
            if vector == kernel_hal::context::TIMER_INTERRUPT_VEC {
                // perf software sampling: the timer fired while this thread was
                // in user mode, so its saved PC is the interrupted user
                // instruction. Feed it to any enabled perf event (cheap no-op
                // when nothing is profiling). This gives `perf top` a live
                // user-space profile without a hardware PMU.
                let pc = thread
                    .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
                    .unwrap_or(0);
                if pc != 0 {
                    linux_object::perf::tick(
                        thread.proc().id() as i32,
                        thread.id() as i32,
                        kernel_hal::cpu::cpu_id() as u32,
                        pc as u64,
                    );
                }
            }
            #[cfg(not(feature = "libos"))]
            {
                // Two independent reasons to give the CPU up here.
                //
                // 1. The timeslice elapsed. The slice length comes from the
                //    thread's Linux scheduling policy / nice value (see
                //    `Thread::tick_should_preempt`), so `nice` and `SCHED_*`
                //    policies give a real, observable bias in CPU share while
                //    still cutting executor churn on CPU-bound workloads.
                //
                // 2. **Wake-up preemption.** Something became runnable on this
                //    CPU while we were holding it. Without this the woken task
                //    waited for the *whole* remaining slice (up to 20 ms), because
                //    the executor only reconsiders its run queue when the polled
                //    future returns `Pending` — and a CPU-bound user thread does
                //    that only at slice expiry. That latency is invisible to any
                //    single-threaded benchmark (nothing else is ever runnable)
                //    yet it is precisely what makes an interactive session feel
                //    slow next to Linux, which preempts on wake-up via
                //    `check_preempt_curr` + a reschedule IPI. `take_need_resched`
                //    is checked on *every* interrupt vector, not just the timer,
                //    so the reschedule IPI the waker sends turns into a yield
                //    immediately instead of at the next 4 ms tick.
                let slice_expired = vector == kernel_hal::context::TIMER_INTERRUPT_VEC
                    && thread.tick_should_preempt();
                if slice_expired || kernel_hal::thread::take_need_resched() {
                    kernel_hal::thread::yield_now().await;
                }
            }
            Ok(())
        }
        TrapReason::PageFault(vaddr, flags) => {
            trace!(
                "page fault from user mode @ {:#x}({:?}), pid={}",
                vaddr,
                flags,
                pid
            );
            let vmar = thread.proc().vmar();
            let pc = thread
                .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
                .unwrap_or(0);
            let fault_result = vmar.handle_page_fault(vaddr, flags);
            if let Err(err) = fault_result {
                error!(
                    "unhandled page fault @ {:#x}({:?}) [{}] {}: {:?}, pid={} proc={} pc={:#x} [{}] -> SIGSEGV",
                    vaddr,
                    flags,
                    describe_addr(&vmar, vaddr),
                    describe_fault_perms(&vmar, vaddr),
                    err,
                    pid,
                    thread.proc().name(),
                    pc,
                    describe_addr(&vmar, pc),
                );
                dump_user_fault_context(
                    thread,
                    &vmar,
                    pc,
                    !flags.contains(kernel_hal::MMUFlags::EXECUTE),
                );
                force_fault_signal(thread, Signal::SIGSEGV, page_fault_info(vaddr, err));
            }
            Ok(())
        }
        TrapReason::UndefinedInstruction => {
            warn!("undefined instruction from user mode, pid={}", pid);
            let pc = thread
                .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
                .unwrap_or(0);
            force_fault_signal(
                thread,
                Signal::SIGILL,
                SigInfo::fault(Signal::SIGILL, SignalCode::ILL_ILLOPC, pc),
            );
            Ok(())
        }
        TrapReason::SoftwareBreakpoint | TrapReason::HardwareBreakpoint => {
            warn!("breakpoint from user mode, pid={}", pid);
            thread.inner().lock_linux().signals.insert(Signal::SIGTRAP);
            Ok(())
        }
        TrapReason::UnalignedAccess => {
            warn!("unaligned access from user mode, pid={}", pid);
            // The data address is not in the trap frame here; the code is.
            force_fault_signal(
                thread,
                Signal::SIGBUS,
                SigInfo::fault(Signal::SIGBUS, SignalCode::BUS_ADRALN, 0),
            );
            Ok(())
        }
        TrapReason::GernelFault(trap_num) => {
            let signal = cpu_fault_signal(trap_num);
            let pc = thread
                .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
                .unwrap_or(0);
            // error!, not warn!: at the default LOG=error a #GP (misaligned
            // SSE access, non-canonical pointer) or #DE left only the `[exit]
            // killed by signal SIGSEGV` line, with no fault address, no
            // registers and no caller -- indistinguishable from a page fault
            // and unresolvable without the report the page-fault path prints.
            let vmar = thread.proc().vmar();
            error!(
                "cpu fault from user mode: trap={:#x} -> {:?}, pid={} proc={} tid={} pc={:#x} [{}]",
                trap_num,
                signal,
                pid,
                thread.proc().name(),
                thread.id(),
                pc,
                describe_addr(&vmar, pc),
            );
            dump_user_fault_context(thread, &vmar, pc, true);
            // A #DE is `FPE_INTDIV` at the instruction; a #GP and the rest
            // are what Linux sends as `SI_KERNEL` with no address.
            let info = match signal {
                Signal::SIGFPE => SigInfo::fault(signal, SignalCode::FPE_INTDIV, pc),
                _ => SigInfo::fault(signal, SignalCode::KERNEL, 0),
            };
            force_fault_signal(thread, signal, info);
            Ok(())
        }
        _ => {
            error!(
                "unsupported trap from user mode: {:x?}, pid={}, {:#x?}",
                reason,
                pid,
                thread.context_cloned(),
            );
            Err(ZxError::NOT_SUPPORTED)
        }
    }
}

/// Map a CPU exception (trap) number to the appropriate Linux signal.
///
/// On x86_64 the mapping follows Linux kernel conventions from
/// `arch/x86/kernel/traps.c`.  On other architectures a conservative
/// default of SIGSEGV is used.
fn cpu_fault_signal(trap_num: usize) -> Signal {
    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            match trap_num as u8 {
                0x00 => Signal::SIGFPE,   // #DE  Divide Error
                0x04 => Signal::SIGSEGV,  // #OF  Overflow
                0x05 => Signal::SIGSEGV,  // #BR  Bound-Range Exceeded
                0x07 => Signal::SIGFPE,   // #NM  Device Not Available (no FPU)
                0x08 => Signal::SIGKILL,  // #DF  Double Fault
                0x09 => Signal::SIGFPE,   // Coprocessor Segment Overrun
                0x0a => Signal::SIGSEGV,  // #TS  Invalid TSS
                0x0b => Signal::SIGBUS,   // #NP  Segment Not Present
                // #SS is SIGBUS, not SIGSEGV: `DO_ERROR(X86_TRAP_SS, SIGBUS,
                // 0, NULL, "stack segment", stack_segment)` in traps.c, and it
                // sits between two neighbours that really are SIGSEGV.
                0x0c => Signal::SIGBUS,   // #SS  Stack-Segment Fault
                0x0d => Signal::SIGSEGV,  // #GP  General Protection Fault
                0x10 => Signal::SIGFPE,   // #MF  x87 FP Exception
                0x13 => Signal::SIGFPE,   // #XF  SIMD FP Exception
                _    => Signal::SIGSEGV,
            }
        } else {
            let _ = trap_num;
            Signal::SIGSEGV
        }
    }
}

fn syscall_num(ctx: &UserContext) -> usize {
    let regs = ctx.general();
    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            regs.rax
        } else if #[cfg(target_arch = "aarch64")] {
            regs.x8
        } else if #[cfg(target_arch = "riscv64")] {
            regs.a7
        } else {
            unimplemented!()
        }
    }
}

fn syscall_args(ctx: &UserContext) -> [usize; 6] {
    let regs = ctx.general();
    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9]
        } else if #[cfg(target_arch = "aarch64")] {
            [regs.x0, regs.x1, regs.x2, regs.x3, regs.x4, regs.x5]
        } else if #[cfg(target_arch = "riscv64")] {
            [regs.a0, regs.a1, regs.a2, regs.a3, regs.a4, regs.a5]
        } else {
            unimplemented!()
        }
    }
}

#[cfg(test)]
mod fault_signal_tests {
    //! What a process is told, or its parent, when the CPU faults it.

    use super::*;
    use core::convert::TryInto;
    use linux_object::process::wait_status_exited;

    #[test]
    fn a_fault_nobody_handles_is_a_death_by_that_signal_not_an_exit_with_139() {
        let status = wait_status_exited(undeliverable_fault_exit_code(Signal::SIGSEGV));
        // WIFSIGNALED: the low seven bits are the signal, and not 0.
        assert_eq!(status & 0x7f, Signal::SIGSEGV as i32);
        assert_eq!(status & 0xff00, 0, "no exit code on a kill");
    }

    #[test]
    fn a_page_fault_says_whether_the_address_was_mapped_and_where_it_was() {
        let info = page_fault_info(0x1234_5000, ZxError::NOT_FOUND);
        assert_eq!(info.code, SignalCode::SEGV_MAPERR);
        let addr = usize::from_ne_bytes(info.as_bytes()[16..24].try_into().unwrap());
        assert_eq!(addr, 0x1234_5000);
        assert_eq!(
            page_fault_info(0x1234_5000, ZxError::ACCESS_DENIED).code,
            SignalCode::SEGV_ACCERR
        );
    }
}

#[cfg(test)]
mod loader_tests {
    //! The loader is the code around user mode: it builds the signal frame a
    //! handler returns from, turns a CPU exception into a signal, and names
    //! the process in `ps`. None of it needs a process or a scheduler once the
    //! decisions are lifted out, and none of it had a test before.

    use super::*;
    use linux_object::signal::SignalStackFlags;

    const STACK_BASE: usize = 0x7fff_0000_0000;
    const STACK_TOP: usize = 0x7fff_0001_0000;
    const ALT_BASE: usize = 0x7000_0000;
    const ALT_SIZE: usize = 0x4000;

    fn alt_stack() -> SignalStack {
        SignalStack {
            sp: ALT_BASE,
            flags: SignalStackFlags::empty(),
            size: ALT_SIZE,
        }
    }

    // ---- where the signal frame goes -------------------------------------

    #[test]
    fn without_sa_onstack_the_frame_stays_below_the_red_zone() {
        // The x86-64 SysV red zone is memory a leaf function may be using
        // right now without having moved `rsp`. Building the frame at `sp`
        // itself would write over the interrupted function's live locals,
        // and the damage only shows after the handler returns.
        let sp = STACK_TOP - 0x1000;
        let top = sigframe_top(sp, &alt_stack(), false).unwrap();
        assert!(top < sp, "the frame must start below the stack pointer");
        assert_eq!(sp - top, RED_ZONE_MAX_SIZE);
    }

    #[test]
    fn sa_onstack_moves_the_frame_to_the_top_of_the_alternate_stack() {
        // This is the whole of `sigaltstack(2)`. The alternate stack was
        // stored by the syscall and read by nobody, so every handler ran on
        // the interrupted stack -- including the SIGSEGV handler that Rust's
        // runtime and glibc install precisely to survive a blown one.
        let sp = STACK_TOP - 0x1000;
        let top = sigframe_top(sp, &alt_stack(), true).unwrap();
        assert_eq!(top, ALT_BASE + ALT_SIZE);
        assert!(
            !(STACK_BASE..=STACK_TOP).contains(&top),
            "the frame must leave the stack that raised the signal"
        );
    }

    #[test]
    fn the_alternate_stack_keeps_its_whole_size_because_it_has_no_red_zone() {
        // The red zone protects frames that are already there. The alternate
        // stack has none -- nothing is running on it yet -- so subtracting it
        // would just waste the top 256 bytes of a stack a program may have
        // sized at exactly MINSIGSTKSZ.
        let top = sigframe_top(STACK_TOP, &alt_stack(), true).unwrap();
        assert_eq!(top, ALT_BASE + ALT_SIZE, "no red zone on the alt stack");
    }

    #[test]
    fn sa_onstack_without_an_installed_stack_falls_back_to_the_current_one() {
        // A handler may carry SA_ONSTACK while the thread never called
        // `sigaltstack`. Linux runs it on the ordinary stack rather than
        // refusing, so a library that sets the flag unconditionally works.
        let sp = STACK_TOP - 0x1000;
        let top = sigframe_top(sp, &SignalStack::default(), true).unwrap();
        assert_eq!(top, sp - RED_ZONE_MAX_SIZE);
    }

    #[test]
    fn a_handler_already_on_the_alternate_stack_is_not_moved_to_its_top() {
        // The nested case: a signal arrives while a handler is running on the
        // alternate stack. Restarting at the top would drop the new frame on
        // top of the live one, so the second handler would return into
        // rubble. Linux keeps the frame where the running handler left `sp`.
        let inside = ALT_BASE + ALT_SIZE / 2;
        let top = sigframe_top(inside, &alt_stack(), true).unwrap();
        assert_eq!(top, inside - RED_ZONE_MAX_SIZE);
    }

    #[test]
    fn a_stack_pointer_too_low_to_step_past_the_red_zone_has_nowhere_to_go() {
        // `sp` is whatever the interrupted program left in the stack
        // register, and a program may leave zero there. The subtraction used
        // to be unchecked: in a debug kernel that is a panic reachable from
        // any process that installs a handler and wrecks its own `sp`, and in
        // a release kernel it wraps to the top of the address space and the
        // frame is written there.
        assert_eq!(sigframe_top(0, &alt_stack(), false), None);
        assert_eq!(
            sigframe_top(RED_ZONE_MAX_SIZE - 1, &alt_stack(), false),
            None
        );
        assert_eq!(
            sigframe_top(RED_ZONE_MAX_SIZE, &alt_stack(), false),
            Some(0)
        );
    }

    #[test]
    fn an_alternate_stack_that_ends_past_the_address_space_has_nowhere_to_go() {
        // `sigaltstack` takes the base and the size separately, so a program
        // can name a stack whose top does not exist.
        let overflowing = SignalStack {
            sp: usize::MAX - 0x10,
            flags: SignalStackFlags::empty(),
            size: 0x1000,
        };
        assert_eq!(sigframe_top(STACK_TOP, &overflowing, true), None);
    }

    // ---- and whether it can go there at all ------------------------------

    #[test]
    fn a_frame_inside_one_writable_mapping_is_accepted() {
        let maps = [(STACK_BASE, STACK_TOP, true)];
        assert!(frame_fits_writable(&maps, STACK_TOP - 0x1000, 0x400));
        // Flush against both ends of the mapping is still inside it.
        assert!(frame_fits_writable(&maps, STACK_BASE, 0x400));
        assert!(frame_fits_writable(&maps, STACK_TOP - 0x400, 0x400));
    }

    #[test]
    fn a_frame_that_runs_off_the_end_of_its_mapping_is_refused() {
        // The guard page at the bottom of a thread stack is exactly this: the
        // frame starts on a real page and ends on one that is not there.
        let maps = [(STACK_BASE, STACK_TOP, true)];
        assert!(!frame_fits_writable(&maps, STACK_TOP - 0x10, 0x400));
        assert!(!frame_fits_writable(&maps, STACK_BASE - 0x10, 0x400));
    }

    #[test]
    fn a_frame_in_a_read_only_mapping_is_refused() {
        // A program that points `sp` at its own text segment. The kernel
        // writes the frame through a raw pointer with the user page table
        // live, so without this the write either faults in kernel mode or,
        // where the mapping is writable to the kernel, succeeds.
        let maps = [(0x40_0000, 0x41_0000, false), (STACK_BASE, STACK_TOP, true)];
        assert!(!frame_fits_writable(&maps, 0x40_1000, 0x400));
        assert!(frame_fits_writable(&maps, STACK_BASE + 0x1000, 0x400));
    }

    #[test]
    fn a_frame_in_no_mapping_at_all_is_refused() {
        let maps = [(STACK_BASE, STACK_TOP, true)];
        // An address the process never mapped, and the kernel half of the
        // address space, which is what a hostile `sp` aims at.
        assert!(!frame_fits_writable(&maps, 0x1_0000, 0x400));
        assert!(!frame_fits_writable(&maps, 0xffff_8000_0000_0000, 0x400));
        assert!(!frame_fits_writable(&[], STACK_BASE, 0x400));
    }

    #[test]
    fn a_frame_spanning_two_adjacent_mappings_is_refused() {
        // Treating touching mappings as one would mean trusting that they
        // really do touch, and a real stack never straddles the boundary
        // anyway: the conservative answer costs a process nothing.
        let maps = [
            (STACK_BASE, STACK_BASE + 0x1000, true),
            (STACK_BASE + 0x1000, STACK_TOP, true),
        ];
        assert!(!frame_fits_writable(&maps, STACK_BASE + 0xf00, 0x400));
    }

    #[test]
    fn a_frame_whose_length_wraps_the_address_space_is_refused() {
        let maps = [(0, usize::MAX, true)];
        assert!(!frame_fits_writable(&maps, usize::MAX - 0x10, 0x400));
    }

    // ---- the pushes themselves -------------------------------------------

    #[test]
    fn push_stack_writes_below_the_pointer_it_is_given() {
        // A stack grows down, so the value must land *under* `stack_top` and
        // the returned address must be where it landed -- that address is
        // what the handler is handed as its `siginfo`/`ucontext` argument.
        let mut buf = [0u64; 8];
        let top = buf.as_mut_ptr() as usize + core::mem::size_of_val(&buf);
        let at = push_stack(top, 0xdead_beef_u64);
        assert_eq!(at, top - 8, "one object below the top");
        assert_eq!(buf[7], 0xdead_beef);
        assert_eq!(buf[6], 0, "nothing else was touched");

        let again = push_stack(at, 0x1234_u64);
        assert_eq!(again, at - 8);
        assert_eq!(buf[6], 0x1234);
        assert_eq!(buf[7], 0xdead_beef, "the first push survived the second");
    }

    #[test]
    fn the_frame_budget_covers_everything_the_handler_path_pushes() {
        // `frame_fits_writable` is asked about a fixed budget, so the budget
        // has to be at least what the pushes below it actually use. Each push
        // first rounds the pointer down to 16, which can cost 15 bytes, and
        // on x86-64 a return address goes on top of the two structs.
        let actual = core::mem::size_of::<SigInfo>()
            + core::mem::size_of::<SignalUserContext>()
            + core::mem::size_of::<usize>()
            + 3 * 15;
        let budget = RED_ZONE_MAX_SIZE
            + core::mem::size_of::<SigInfo>()
            + core::mem::size_of::<SignalUserContext>()
            + core::mem::size_of::<usize>();
        assert!(
            budget >= actual,
            "budget {} does not cover the {} bytes pushed",
            budget,
            actual
        );
    }

    // ---- CPU exceptions --------------------------------------------------

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn cpu_exceptions_map_to_the_signals_linux_raises() {
        // `arch/x86/kernel/traps.c`. Three of these are SIGBUS or SIGFPE
        // where the neighbours are SIGSEGV, and a program that installs a
        // per-signal handler -- a JIT catching #DE, a runtime catching
        // #GP -- sees the difference directly.
        assert_eq!(cpu_fault_signal(0x00), Signal::SIGFPE, "#DE divide error");
        assert_eq!(cpu_fault_signal(0x04), Signal::SIGSEGV, "#OF overflow");
        assert_eq!(cpu_fault_signal(0x05), Signal::SIGSEGV, "#BR bound range");
        assert_eq!(cpu_fault_signal(0x07), Signal::SIGFPE, "#NM no FPU");
        assert_eq!(cpu_fault_signal(0x08), Signal::SIGKILL, "#DF double fault");
        assert_eq!(cpu_fault_signal(0x09), Signal::SIGFPE, "coproc overrun");
        assert_eq!(cpu_fault_signal(0x0a), Signal::SIGSEGV, "#TS invalid TSS");
        assert_eq!(cpu_fault_signal(0x0b), Signal::SIGBUS, "#NP not present");
        assert_eq!(cpu_fault_signal(0x0c), Signal::SIGBUS, "#SS stack segment");
        assert_eq!(cpu_fault_signal(0x0d), Signal::SIGSEGV, "#GP protection");
        assert_eq!(cpu_fault_signal(0x10), Signal::SIGFPE, "#MF x87");
        assert_eq!(cpu_fault_signal(0x13), Signal::SIGFPE, "#XF SIMD");
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn an_unknown_exception_number_is_a_segmentation_fault() {
        // The table is indexed by `trap_num as u8`, so a number above 255
        // aliases onto a real vector. Anything unrecognised has to end at a
        // signal rather than at a `match` that cannot answer.
        assert_eq!(cpu_fault_signal(0xff), Signal::SIGSEGV);
        assert_eq!(cpu_fault_signal(usize::MAX), Signal::SIGSEGV);
        // 0x100 truncates to 0x00, the divide error: the truncation is
        // deliberate (a vector is one byte) but worth pinning, because a
        // reader who expects the default arm here would be wrong.
        assert_eq!(cpu_fault_signal(0x100), Signal::SIGFPE);
    }

    // ---- the process name ------------------------------------------------

    #[test]
    fn the_process_name_is_the_last_path_component() {
        // This is what `ps` and `/proc/<pid>/comm` show, and what a user
        // greps for to kill something.
        assert_eq!(comm_from_path("/usr/bin/firefox"), "firefox");
        assert_eq!(comm_from_path("firefox"), "firefox");
        assert_eq!(comm_from_path("./a.out"), "a.out");
        assert_eq!(comm_from_path("/usr/lib/x86_64/ld.so"), "ld.so");
    }

    #[test]
    fn a_path_with_no_last_component_does_not_lose_its_name() {
        // A trailing slash and the root itself both leave an empty component.
        // Empty is what the caller gets; it must not panic, because the path
        // comes from `execve` and a program may pass anything.
        assert_eq!(comm_from_path("/usr/bin/"), "");
        assert_eq!(comm_from_path("/"), "");
        assert_eq!(comm_from_path(""), "");
    }
}
