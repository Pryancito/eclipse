//! Run Linux process and manage trap/interrupt/syscall.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{future::Future, pin::Pin};
use linux_object::signal::{
    MachineContext, SigInfo, Signal, SignalUserContext, Sigset, SIG_DFL, SIG_IGN,
};

use kernel_hal::context::{TrapReason, UserContext, UserContextField};
use kernel_hal::interrupt::intr_on;
use linux_object::fs::{
    vfs::{FileSystem, INode},
    INodeExt,
};
use linux_object::thread::{CurrentThreadExt, ThreadExt};
use linux_object::{loader::LinuxElfLoader, process::ProcessExt};
use zircon_object::task::{CurrentThread, Process, Thread, ThreadState};
use zircon_object::{
    object::{KernelObject, KoID},
    vm::USER_STACK_PAGES,
    ZxError, ZxResult,
};

fn comm_from_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// [bootdiag] TEMPORARY: emit a diagnostic line on the EARLY framebuffer console
/// (the splash-logo surface), which paints reliably even when the native graphic
/// console does not yet. Used to trace the real-hardware boot hang where busybox
/// `sh` is spawned but never prints a prompt. Remove once diagnosed.
#[allow(dead_code)]
fn bootdiag(s: &str) {
    // Early framebuffer console (visible on real hardware where the native
    // console does not paint) AND serial (so a QEMU run captures the trace in
    // /tmp/serial.out without needing a screen photo).
    kernel_hal::console::early_console_write_str(s);
    kernel_hal::console::early_console_write_str("\n");
    kernel_hal::console::serial_write_str(s);
    kernel_hal::console::serial_write_str("\n");
}

/// [bootdiag] TEMPORARY: short name for common x86_64 syscall numbers, so the
/// blocking syscall is recognizable in a (possibly mirrored) boot photo.
#[allow(dead_code)]
fn sc_name(num: u32) -> &'static str {
    match num {
        0 => "read",
        1 => "write",
        2 => "open",
        3 => "close",
        4 => "stat",
        5 => "fstat",
        7 => "poll",
        9 => "mmap",
        10 => "mprot",
        11 => "munmap",
        12 => "brk",
        13 => "rt_sigaction",
        14 => "rt_sigprocmask",
        16 => "ioctl",
        20 => "writev",
        21 => "access",
        35 => "nanosleep",
        39 => "getpid",
        59 => "execve",
        60 => "exit",
        61 => "wait4",
        63 => "uname",
        72 => "fcntl",
        79 => "getcwd",
        89 => "readlink",
        97 => "getrlimit",
        102 => "getuid",
        104 => "getgid",
        107 => "geteuid",
        108 => "getegid",
        110 => "getppid",
        111 => "getpgrp",
        157 => "prctl",
        158 => "arch_prctl",
        202 => "futex",
        217 => "getdents64",
        218 => "set_tid_address",
        228 => "clock_gettime",
        231 => "exit_group",
        257 => "openat",
        262 => "newfstatat",
        270 => "pselect6",
        271 => "ppoll",
        273 => "set_robust_list",
        290 => "eventfd2",
        302 => "prlimit64",
        318 => "getrandom",
        332 => "statx",
        _ => "?",
    }
}

/// Create and run a single Linux process as PID 1 on virtual terminal 0.
///
/// Used by the libos example/tests, where one program is the whole system.
pub fn run(args: Vec<String>, envs: Vec<String>, rootfs: Arc<dyn FileSystem>) -> Arc<Process> {
    const INIT_PID: KoID = 1;
    spawn(args, envs, rootfs, 0, None, INIT_PID, true).expect("run: program not found")
}

/// Create and run the configured per-terminal SHELL on virtual terminal `vt`
/// with the fixed Linux `pid` (the reserved 101.. range). The shell binary is
/// the system default and must exist; a missing shell is fatal.
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
) -> Arc<Process> {
    spawn(
        args,
        envs,
        rootfs,
        vt,
        shared_root,
        pid,
        /* boot_work */ vt == 0,
    )
    .expect("configured SHELL not found")
}

/// Spawn the INIT process as PID 1 if its binary exists, returning `None`
/// otherwise so the machine can boot without a PID 1 init (e.g. when
/// `/sbin/openrc-init` is not installed). Runs on the primary console (vt 0).
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
            "INIT {:?} not present; booting without a PID 1 init",
            args[0]
        );
        return None;
    }
    const INIT_PID: KoID = 1;
    spawn(args, envs, rootfs, 0, shared_root, INIT_PID, boot_work)
}

/// True if `path` resolves on the primary root (if any) or the initramfs.
fn program_exists(
    path: &str,
    primary_root: &Option<Arc<dyn INode>>,
    rootfs: &Arc<dyn FileSystem>,
) -> bool {
    if let Some(root) = primary_root {
        if root.lookup(path).is_ok() {
            return true;
        }
    }
    rootfs.root_inode().lookup(path).is_ok()
}

/// Core process spawn. Returns `None` if `args[0]` cannot be found on the
/// process's root or the initramfs (instead of panicking) so optional
/// processes can be skipped gracefully. `boot_work` runs the one-time boot
/// setup (network init, fstab mount, console clear) and must happen once.
fn spawn(
    args: Vec<String>,
    envs: Vec<String>,
    rootfs: Arc<dyn FileSystem>,
    vt: usize,
    shared_root: Option<Arc<dyn INode>>,
    pid: KoID,
    boot_work: bool,
) -> Option<Arc<Process>> {
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
    };

    let inode = match root_inode.lookup(&args[0]) {
        Ok(inode) => inode,
        Err(e) => match rootfs.root_inode().lookup(&args[0]) {
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
        .read_as_vmo()
        .unwrap_or_else(|e| panic!("failed to read process {:?}: {:?}", args[0], e));
    let path = args[0].clone();

    // Verify binary integrity with hunter
    let mut header = [0u8; 4];
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
    // [bootdiag] TEMPORARY: print to the EARLY framebuffer console (the surface
    // showing the splash logo), not warn!: on real hardware the native graphic
    // console does not paint at this stage, so warn! markers were invisible. The
    // early console always paints (it is drawing the logo right now). Pinpoints
    // whether boot reaches/clears the ELF load. Remove once the hang is found.
    bootdiag(&alloc::format!("[bd] vt={} pid={} loading ELF {:?}", vt, pid, args.get(0)));
    //调用zircon-object/src/task/thread.start设置好要执行的thread
    let (entry, sp, initial_brk, execute_path) = loader
        .load(&proc.vmar(), &vmo, args.clone(), envs, path)
        .unwrap_or_else(|e| panic!("failed to load process {:?}: {:?}", args[0], e));
    bootdiag(&alloc::format!(
        "[bd] vt={} pid={} ELF loaded entry={:#x} sp={:#x}",
        vt, pid, entry, sp
    ));
    proc.linux().set_execute_path(&execute_path);
    proc.linux().set_cmdline(args);
    proc.linux().set_brk(initial_brk);
    proc.set_name(comm_from_path(&execute_path));

    thread
        .start_with_entry(entry, sp, 0, 0, thread_fn)
        .expect("failed to start main thread");

    // [bootdiag] TEMPORARY: shell main thread created and queued on the executor.
    bootdiag(&alloc::format!("[bd] vt={} pid={} thread queued", vt, pid));

    // Mount the non-root /etc/fstab entries (/boot/efi vfat, /home, …) as a
    // deferred kernel task, off the synchronous boot path: the blocking
    // block-device I/O would otherwise risk stalling boot before the shell
    // shows. Done once, by whoever performs the boot work.
    if boot_work {
        kernel_hal::thread::spawn(async {
            linux_object::fs::mount_fstab_deferred();
        });
    }

    Some(proc)
}

fn thread_fn(thread: CurrentThread) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
    Box::pin(run_user(thread))
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
    // [bootdiag] TEMPORARY: confirm the executor actually polled this thread.
    #[cfg(not(feature = "libos"))]
    bootdiag(&alloc::format!(
        "[bd] run_user entered pid={} tid={}",
        thread.proc().id(),
        thread.id()
    ));
    loop {
        // wait
        let mut ctx = thread.wait_for_run().await;
        if thread.state() == ThreadState::Dying {
            break;
        }

        // check the signal and handle
        if let Some((signal, sigmask)) = thread.inner().lock_linux().handle_signal() {
            ctx = handle_signal(&thread, ctx, signal, sigmask);
        }
        if thread.state() == ThreadState::Dying {
            break;
        }

        // run
        trace!(
            "go to user: tid = {} pc = {:x} sp = {:x}",
            thread.id(),
            ctx.get_field(UserContextField::InstrPointer),
            ctx.get_field(UserContextField::StackPointer)
        );
        trace!("ctx before enter: {:#x?}", ctx);
        ctx.enter_uspace();
        debug!(
            "back from user: tid = {} pc = {:x} trap reason = {:?}",
            thread.id(),
            ctx.get_field(UserContextField::InstrPointer),
            ctx.trap_reason(),
        );
        trace!("ctx = {:#x?}", ctx);
        // handle trap/interrupt/syscall
        if let Err(err) = handle_user_trap(&thread, ctx).await {
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
        thread.inner().lock_linux().handling_signal = None;
        return ctx;
    }
    if action.handler == SIG_DFL {
        // Per-signal default disposition. Linux's default for the job-control and
        // a few status signals is NOT to terminate: SIGCHLD/SIGURG/SIGWINCH are
        // ignored, and SIGTSTP/SIGTTIN/SIGTTOU/SIGSTOP stop the process while
        // SIGCONT resumes it. This kernel has no job-control stop state, so it
        // approximates all of those as "ignore" — crucially this stops an
        // interactive `sh` from being *killed* by the SIGTTIN it sends itself
        // during job-control setup (the cause of the per-VT shells dying and the
        // terminal never reaching a usable prompt). Everything else still
        // terminates, as before.
        match signal {
            Signal::SIGCHLD
            | Signal::SIGURG
            | Signal::SIGWINCH
            | Signal::SIGCONT
            | Signal::SIGSTOP
            | Signal::SIGTSTP
            | Signal::SIGTTIN
            | Signal::SIGTTOU => {
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
        let code = 128 + signal as i32;
        // Record the death in the dmesg ring (warn! reaches it at the default
        // level). A process that dies on a default-disposition signal — Xorg
        // aborting in early init, say — otherwise vanishes with no trace at all.
        warn!(
            "[exit] pid={} killed by signal {:?} ({}) at pc={:#x} (default disposition)",
            thread.proc().id(),
            signal,
            signal as i32,
            user_pc,
        );
        thread.proc().exit(code as i64);
        return ctx;
    }
    let signal_info = SigInfo::default();
    let signal_context = SignalUserContext {
        sig_mask: sigmask,
        context: MachineContext::new(user_pc),
        ..Default::default()
    };
    // push `siginfo` `uctx` into user stack
    const RED_ZONE_MAX_SIZE: usize = 0x100; // 256Bytes
    let mut sp = user_sp - RED_ZONE_MAX_SIZE;
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
fn force_fault_signal(thread: &CurrentThread, signal: Signal) {
    let action = thread.proc().linux().signal_action(signal);
    let inner = thread.inner();
    let undeliverable = {
        let linux = inner.lock_linux();
        linux.signal_mask.contains(signal)
            || linux.handling_signal.is_some()
            || linux.signals.contains(signal)
            || action.handler == SIG_DFL
            || action.handler == SIG_IGN
    };
    if undeliverable {
        warn!(
            "[exit] pid={} killed by fault signal {:?} ({}) — undeliverable, terminating",
            thread.proc().id(),
            signal,
            signal as i32,
        );
        thread.proc().exit(128 + signal as i64);
    } else {
        // Deliverable custom handler: unblock so it cannot be deferred and queue
        // it for the next pass of the run loop.
        let mut linux = inner.lock_linux();
        linux.signal_mask.remove(signal);
        linux.signals.insert(signal);
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
        // [bootdiag] TEMPORARY: trace the primary shell's (pid 101) first syscalls
        // so a real-hardware boot photo shows exactly where busybox `sh` blocks
        // before printing its prompt. Prints an "enter" before the call and a
        // "ret" after, so the last "enter" with no matching "ret" is the syscall
        // that never returned (the hang). Bounded so it can't flood. Remove once
        // the hang is diagnosed.
        #[cfg(not(feature = "libos"))]
        let diag_n: Option<usize> = if thread.proc().id() == 101 {
            use core::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 120 {
                bootdiag(&alloc::format!(
                    "[bd] sc#{} {} a0={:x} a1={:x} ENTER",
                    n, sc_name(num as u32), args[0], args[1]
                ));
                Some(n)
            } else {
                None
            }
        } else {
            None
        };
        ctx.advance_pc(reason);
        thread.put_context(ctx);
        let mut syscall = linux_syscall::Syscall {
            thread,
            thread_fn,
            syscall_entry: kernel_hal::context::syscall_entry as *const () as usize,
        };
        trace!("Syscall: {} {:x?}", num as u32, args);
        let ret = run_with_irq_enable! {
            syscall.syscall(num as u32, args).await as usize
        };
        // [bootdiag] TEMPORARY: matching "ret" for the enter above.
        #[cfg(not(feature = "libos"))]
        if let Some(n) = diag_n {
            bootdiag(&alloc::format!("[bd] sc#{} ret={:x}", n, ret));
        }
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
            if vector == kernel_hal::context::TIMER_INTERRUPT_VEC && thread.tick_should_preempt() {
                // Preempt once the running thread's timeslice elapses rather
                // than on every raw tick. The slice length comes from the
                // thread's Linux scheduling policy / nice value (see
                // `Thread::tick_should_preempt`), so `nice` and `SCHED_*`
                // policies give a real, observable bias in CPU share while
                // still cutting executor churn on CPU-bound workloads.
                kernel_hal::thread::yield_now().await;
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
            let pgf_res = vmar.handle_page_fault(vaddr, flags);
            if let Err(err) = pgf_res {
                let pc = thread
                    .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
                    .unwrap_or(0);
                warn!(
                    "unhandled page fault @ {:#x}({:?}): {:?}, pid={} proc={} pc={:#x} -> SIGSEGV",
                    vaddr,
                    flags,
                    err,
                    pid,
                    thread.proc().name(),
                    pc,
                );
                // Make a userspace crash self-diagnosing from dmesg: dump the
                // registers and the code bytes around the faulting PC. With the
                // faulting instruction *and* the instructions that computed the
                // bad pointer, plus the register values, the root cause (botched
                // relocation/GOT, bad TLS access, AVX op, …) can be read off
                // directly instead of guessed.
                #[cfg(not(feature = "libos"))]
                {
                    use kernel_hal::vm::{GenericPageTable, PageTable};
                    let pt = PageTable::from_current();
                    // Read one user byte through the process page table (physmap).
                    let rd = |va: usize| -> Option<u8> {
                        pt.query(va & !0xfff).ok().map(|(pa, _, _)| {
                            let kv = 0xffff_8000_0000_0000usize + (pa & !0xfff) + (va & 0xfff);
                            unsafe { core::ptr::read_volatile(kv as *const u8) }
                        })
                    };
                    if let Some((rax, rbx, rcx, rdx, rsi, rdi, rbp, r8to11)) = thread
                        .with_context(|ctx| {
                            let g = ctx.general();
                            (g.rax, g.rbx, g.rcx, g.rdx, g.rsi, g.rdi, g.rbp, g.r8)
                        })
                        .ok()
                    {
                        warn!(
                            "[crash] pid={} pc={:#x} rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x} rbp={:#x} r8={:#x}",
                            pid, pc, rax, rbx, rcx, rdx, rsi, rdi, rbp, r8to11,
                        );
                    }
                    // 16 bytes before + 32 after the PC: shows how the faulting
                    // pointer register was loaded, and the faulting instruction.
                    let mut code = [0u8; 48];
                    let start = pc.wrapping_sub(16);
                    let mut any = false;
                    for (i, b) in code.iter_mut().enumerate() {
                        if let Some(byte) = rd(start.wrapping_add(i)) {
                            *b = byte;
                            any = true;
                        }
                    }
                    if any {
                        warn!(
                            "[crash] pid={} code@{:#x} (pc-16): {:02x?}",
                            pid, start, code
                        );
                    } else {
                        warn!(
                            "[crash] pid={} pc={:#x} UNMAPPED (jumped through a bad pointer)",
                            pid, pc
                        );
                    }
                }
                // DEBUG: si el fault apunta a la mitad-kernel (rango physmap
                // 0xffff8000_xxxx), apk dereferenció un puntero de kernel. Volcar
                // el contenido del frame físico destino (el kernel SÍ lo tiene
                // mapeado por physmap) para identificar qué estructura lee, y los
                // registros de usuario para rastrear el origen del puntero.
                #[cfg(not(feature = "libos"))]
                if (0xffff_8000_0000_0000..0xffff_8000_4000_0000).contains(&vaddr) {
                    let base = vaddr & !0x3f;
                    let mut w = [0u64; 8];
                    for i in 0..8 {
                        w[i] = unsafe { core::ptr::read_volatile((base + i * 8) as *const u64) };
                    }
                    let (sp, saved_rbp, ctx_addr) = thread
                        .with_context(|ctx| {
                            (
                                ctx.get_field(UserContextField::StackPointer),
                                ctx.dbg_general_rbp(),
                                ctx.dbg_ctx_addr(),
                            )
                        })
                        .unwrap_or((0, 0, 0));
                    let (rsi, rdi, r8, r9, r10, r11) = thread
                        .with_context(|ctx| ctx.dbg_loop_regs())
                        .unwrap_or((0, 0, 0, 0, 0, 0));
                    warn!(
                        "[faultdump] regs: rsi={:#x} rdi={:#x} r8={:#x} r9={:#x} r10={:#x} r11={:#x}",
                        rsi, rdi, r8, r9, r10, r11
                    );
                    // Volcar el buffer fuente de apk (rsi) para caracterizar la
                    // corrupción de datos: traducir la vaddr de usuario a física vía
                    // la vmar del proceso y leer por physmap (0xffff8000... base).
                    {
                        use kernel_hal::vm::{GenericPageTable, PageTable};
                        // Volcar 256 B del buffer de apk para ver la granularidad de
                        // la corrupción (trozos buenos/malos). Se interpreta cada
                        // byte como ASCII imprimible ('.' si no) para ver texto vs
                        // basura de un vistazo.
                        let src = (rsi & !0x3f).saturating_sub(0x40);
                        let pt = PageTable::from_current();
                        if let Ok((pa, _, _)) = pt.query(src & !0xfff) {
                            let byte_pa = (pa & !0xfff) + (src & 0xfff);
                            let kv = 0xffff_8000_0000_0000usize + byte_pa;
                            for row in 0..16 {
                                let mut s = [b'.'; 16];
                                for j in 0..16 {
                                    let b = unsafe {
                                        core::ptr::read_volatile((kv + row * 16 + j) as *const u8)
                                    };
                                    if (0x20..0x7f).contains(&b) {
                                        s[j] = b;
                                    }
                                }
                                warn!(
                                    "[bufdump] @{:#x} {}",
                                    src + row * 16,
                                    core::str::from_utf8(&s).unwrap_or("?")
                                );
                            }
                        }
                        core::mem::forget(pt);
                    }
                    let asm_save = kernel_hal::context::dbg_asm_save_addr();
                    warn!(
                        "[faultdump] target physmap {:#x} (phys {:#x}) content: {:#018x?}",
                        base,
                        base - 0xffff_8000_0000_0000,
                        w
                    );
                    // CLAVE: ¿el rbp GUARDADO en el contexto coincide con la dirección
                    // que falló (~vaddr)? Si sí -> el contexto tiene el rbp corrupto
                    // (y ctxcheck debería haber disparado). Si el rbp guardado es
                    // VÁLIDO mientras el registro real estaba corrupto -> el save del
                    // asm escribió un rbp distinto al real.
                    warn!(
                        "[faultdump] saved_rbp={:#x} vaddr={:#x} user_rsp={:#x} pc={:#x} {}",
                        saved_rbp,
                        vaddr,
                        sp,
                        pc,
                        if saved_rbp >= 0xffff_8000_0000_0000 {
                            "(saved rbp YA corrupto en contexto)"
                        } else {
                            "(saved rbp VALIDO; registro real difiere)"
                        }
                    );
                    warn!(
                        "[faultdump] ctx_addr={:#x} asm_save_addr={:#x} {}",
                        ctx_addr,
                        asm_save,
                        if asm_save != ctx_addr {
                            "<<< MISMATCH: el asm guardó en OTRA direccion que el UserContext del kernel"
                        } else {
                            "(coinciden: save y contexto misma memoria)"
                        }
                    );
                }
                force_fault_signal(thread, Signal::SIGSEGV);
            } else {
                // DEBUG: detector de frame compartido ESCRIBIBLE entre procesos
                // vivos (COW-break fallido). En faults de ESCRITURA, registrar el
                // pid dueño del frame mapeado; si otro pid vivo ya lo tenía -> bug.
                #[cfg(not(feature = "libos"))]
                if flags.contains(kernel_hal::MMUFlags::WRITE) {
                    use kernel_hal::vm::{GenericPageTable, PageTable};
                    let pt = PageTable::from_current();
                    if let Ok((pa, fl, _)) = pt.query(vaddr & !0xfff) {
                        if fl.contains(kernel_hal::MMUFlags::WRITE) {
                            if let Some(prev) = kernel_hal::dbg_frameowner::set_check(
                                (pa & !0xfff) >> 12,
                                pid as u32,
                            ) {
                                warn!(
                                    "[shared-w] !!! frame {:#x} WRITABLE por pid {} Y pid {} (vaddr={:#x}) -> COW-break FALLIDO",
                                    pa & !0xfff,
                                    prev,
                                    pid,
                                    vaddr
                                );
                            }
                        }
                    }
                    core::mem::forget(pt);
                }
            }
            Ok(())
        }
        TrapReason::UndefinedInstruction => {
            warn!("undefined instruction from user mode, pid={}", pid);
            force_fault_signal(thread, Signal::SIGILL);
            Ok(())
        }
        TrapReason::SoftwareBreakpoint | TrapReason::HardwareBreakpoint => {
            warn!("breakpoint from user mode, pid={}", pid);
            thread.inner().lock_linux().signals.insert(Signal::SIGTRAP);
            Ok(())
        }
        TrapReason::UnalignedAccess => {
            warn!("unaligned access from user mode, pid={}", pid);
            force_fault_signal(thread, Signal::SIGBUS);
            Ok(())
        }
        TrapReason::GernelFault(trap_num) => {
            let signal = cpu_fault_signal(trap_num);
            let pc = thread
                .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
                .unwrap_or(0);
            warn!(
                "cpu fault from user mode: trap={:#x} -> {:?}, pid={}, tid={}, pc={:#x}",
                trap_num,
                signal,
                pid,
                thread.id(),
                pc
            );
            force_fault_signal(thread, signal);
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
                0x0c => Signal::SIGSEGV,  // #SS  Stack-Segment Fault
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
