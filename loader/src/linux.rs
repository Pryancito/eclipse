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
use zircon_object::{object::KernelObject, vm::USER_STACK_PAGES, ZxError, ZxResult};

fn comm_from_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Create and run the main Linux process (virtual terminal 0).
pub fn run(args: Vec<String>, envs: Vec<String>, rootfs: Arc<dyn FileSystem>) -> Arc<Process> {
    run_on_vt(args, envs, rootfs, 0, None)
}

/// Create and run a Linux process bound to virtual terminal `vt`.
///
/// `shared_root` lets extra per-VT shells reuse the primary process's mounted
/// root filesystem (by `Arc`) instead of re-scanning disks. One-time boot work
/// (network init, fstab mount, console clear) only runs for `vt == 0`.
pub fn run_on_vt(
    args: Vec<String>,
    envs: Vec<String>,
    rootfs: Arc<dyn FileSystem>,
    vt: usize,
    shared_root: Option<Arc<dyn INode>>,
) -> Arc<Process> {
    info!("Run Linux process on vt{}: args={:?}, envs={:?}", vt, args, envs);
    if vt == 0 {
        linux_object::net::init();
    }
    let job = zircon_object::task::ROOT_JOB.clone();
    let proc = Process::create_linux(&job, rootfs.clone(), vt, shared_root).expect("create_linux");
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
        Err(e) => {
            warn!(
                "root proc {:?} missing on installed root ({:?}); trying initramfs",
                args[0], e
            );
            rootfs
                .root_inode()
                .lookup(&args[0])
                .expect("lookup root process on initramfs")
        }
    };
    let vmo = inode
        .read_as_vmo()
        .unwrap_or_else(|e| panic!("failed to read root process {:?}: {:?}", args[0], e));
    let path = args[0].clone();

    // Boot UX: clear to black right before the first graphic-console output (prompt).
    // This call is a no-op when graphic mode is disabled. Only for the primary VT.
    if vt == 0 {
        kernel_hal::console::request_clear_graphic_on_next_write();
    }

    let pg_token = kernel_hal::vm::current_vmtoken();
    debug!("current pgt = {:#x}", pg_token);
    //调用zircon-object/src/task/thread.start设置好要执行的thread
    let (entry, sp, initial_brk, execute_path) = loader
        .load(&proc.vmar(), &vmo, args.clone(), envs, path)
        .unwrap_or_else(|e| panic!("failed to load root process {:?}: {:?}", args[0], e));
    proc.linux().set_execute_path(&execute_path);
    proc.linux().set_cmdline(args);
    proc.linux().set_brk(initial_brk);
    proc.set_name(comm_from_path(&execute_path));

    thread
        .start_with_entry(entry, sp, 0, 0, thread_fn)
        .expect("failed to start main thread");

    // Now that init is started, mount the non-root entries from /etc/fstab
    // (/boot/efi vfat, /home, …) as a deferred kernel task. This is done off
    // the synchronous boot path on purpose: the blocking block-device I/O it
    // performs would otherwise risk stalling disk boot before the shell shows.
    if vt == 0 {
        kernel_hal::thread::spawn(async {
            linux_object::fs::mount_fstab_deferred();
        });
    }

    proc
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
        // Minimal default dispositions: for Ctrl+C (SIGINT) terminate the process.
        // TODO: implement per-signal default table.
        let code = 128 + signal as i32;
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
            if vector == kernel_hal::context::TIMER_INTERRUPT_VEC
                && kernel_hal::percpu::tick_should_preempt()
            {
                // Preempt on a multiple-of-tick quantum (≈ 20 ms at 250 Hz)
                // instead of every raw tick. Cuts executor churn on
                // CPU-bound workloads where the same task would otherwise
                // be re-selected immediately.
                kernel_hal::thread::yield_now().await;
            }
            Ok(())
        }
        TrapReason::PageFault(vaddr, flags) => {
            // A user page fault is the normal demand-paging / CoW / stack-growth
            // path and happens constantly; logging each one at `warn!` forces a
            // synchronous serial write per fault and dominates fault-heavy
            // workloads (sysbench `memory`, fresh `mmap`, stack growth). Only the
            // failure case below (a real SIGSEGV) is worth a warning.
            trace!(
                "page fault from user mode @ {:#x}({:?}), pid={}",
                vaddr, flags, pid
            );
            let vmar = thread.proc().vmar();
            if let Err(err) = vmar.handle_page_fault(vaddr, flags) {
                let pc = thread
                    .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
                    .unwrap_or(0);
                warn!(
                    "unhandled page fault @ {:#x}({:?}): {:?}, pid={} proc={} pc={:#x} -> SIGSEGV",
                    vaddr, flags, err, pid, thread.proc().name(), pc,
                );
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
