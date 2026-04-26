//! Signal handling syscalls and infrastructure
//!
//! Implementation of Linux-compatible signal delivery and management.

use crate::process::{exit_process, current_process_id};
use crate::scheduler::yield_cpu;
use super::{is_user_pointer, copy_to_user, copy_from_user};

/// Build and push an `rt_sigframe` onto the user stack, then redirect `ctx`
/// (the syscall return context) to the signal handler.
pub fn push_rt_signal_frame(
    ctx: &mut crate::interrupts::SyscallContext,
    pid: crate::process::ProcessId,
    sig: u8,
    action: &crate::process::SignalAction,
    old_mask: u64,
    fault_addr: u64,
    trap_num: u64,
) -> bool {
    use crate::process::{SA_ONSTACK, SA_NODEFER, SA_RESETHAND, SS_DISABLE};
    use super::RtSigframe;
    use super::UContext;
    use super::SigContext;
    use super::StackT;
    use super::SigInfo;
    
    // Require a valid restorer (musl always sets SA_RESTORER + __restore_rt).
    if action.restorer == 0 {
        return false;
    }
    let (alt_sp, alt_sz, alt_flags) = crate::process::get_process_altstack(pid);
    let mut using_altstack = false;
    let user_rsp = if (action.flags & SA_ONSTACK) != 0 {
        if (alt_flags & crate::process::SS_DISABLE) == 0 {
            if (alt_flags & crate::process::SS_ONSTACK) == 0 {
                using_altstack = true;
                alt_sp + alt_sz
            } else {
                ctx.rsp
            }
        } else {
            ctx.rsp
        }
    } else {
        ctx.rsp
    };

    // Allocate frame: reserve frame, align to 16 B.
    const FRAME_SZ: u64 = core::mem::size_of::<RtSigframe>() as u64;
    let frame_addr = (user_rsp.wrapping_sub(FRAME_SZ)) & !15u64;

    // Build the frame.
    let mut frame = RtSigframe {
        pretcode: action.restorer,
        uc: UContext {
            uc_flags:    0,
            uc_link:     0,
            uc_stack:    StackT { 
                ss_sp:    alt_sp, 
                ss_flags: alt_flags, 
                _pad:     0, 
                ss_size:  alt_sz 
            },
            uc_mcontext: SigContext {
                r8:      ctx.r8,
                r9:      ctx.r9,
                r10:     ctx.r10,
                r11:     ctx.r11,
                r12:     ctx.r12,
                r13:     ctx.r13,
                r14:     ctx.r14,
                r15:     ctx.r15,
                rdi:     ctx.rdi,
                rsi:     ctx.rsi,
                rbp:     ctx.rbp,
                rbx:     ctx.rbx,
                rdx:     ctx.rdx,
                rax:     ctx.rax,
                rcx:     ctx.rcx,
                rsp:     ctx.rsp,
                rip:     ctx.rip,
                eflags:  ctx.rflags,
                cs:      ctx.cs as u16,
                gs:      0,
                fs:      0,
                ss:      ctx.ss as u16,
                err:     0,
                trapno:  trap_num,
                oldmask: old_mask,
                cr2:     fault_addr,
                fpstate: 0,
                _reserved1: [0u64; 8],
            },
            uc_sigmask: old_mask,
        },
        info: SigInfo {
            si_signo: sig as i32,
            si_errno: 0,
            si_code:  0, // Set later if needed
            _rest:    [0u8; 116],
        },
        _pad: 0,
        fpstate: [0; 512],
    };

    // Save FPU state to the frame.
    unsafe {
        core::arch::asm!("fxsave [{}]", in(reg) &mut frame.fpstate[0]);
    }

    // Point uc_mcontext.fpstate to the fpstate buffer ON THE USER STACK.
    frame.uc.uc_mcontext.fpstate = frame_addr + 448;

    // Write the frame to user memory safely.
    let frame_bytes = unsafe {
        core::slice::from_raw_parts(&frame as *const RtSigframe as *const u8, FRAME_SZ as usize)
    };
    
    if !copy_to_user(frame_addr, frame_bytes) {
        crate::serial::serial_printf(format_args!("[SIG] Failed to push frame to {:#018x}\n", frame_addr));
        return false;
    }

    // Block this signal during the handler (and any additional signals from sa_mask),
    // unless SA_NODEFER is set.
    let _ = crate::process::modify_process(pid, |p| {
        if (action.flags & SA_NODEFER) == 0 {
            p.signal_mask |= 1u64 << sig;
        }
        p.signal_mask |= action.mask;
        
        // SIGKILL and SIGSTOP are unblockable.
        p.signal_mask &= !((1u64 << 8) | (1u64 << 18));

        // Set SS_ONSTACK if we moved to the altstack.
        if using_altstack {
            p.sigaltstack.ss_flags |= crate::process::SS_ONSTACK;
        }
    });

    // SA_RESETHAND: reset handler to SIG_DFL after first delivery.
    if (action.flags & SA_RESETHAND) != 0 {
        let _ = crate::process::modify_process(pid, |p| {
            let mut proc = p.proc.lock();
            if (sig as usize) < proc.signal_actions.len() {
                proc.signal_actions[sig as usize].handler = 0;
            }
        });
    }

    // Set up context for handler.
    ctx.rip = action.handler;
    ctx.rsp = frame_addr;
    ctx.rdi = sig as u64;
    ctx.rsi = frame_addr + 312; // Offset of 'info'
    ctx.rdx = frame_addr + 8;   // Offset of 'uc'
    ctx.rax = 0;
    
    // Clear RFLAGS.TF to prevent single-stepping into handler.
    ctx.rflags &= !0x100;
    
    true
}

/// Deliver pending signals that have userspace handlers by pushing signal frames.
pub fn deliver_pending_signals_for_current(ctx: &mut crate::interrupts::SyscallContext) {
    let Some(pid) = current_process_id() else { return };
    if crate::process::get_process(pid)
        .map_or(true, |p| p.state == crate::process::ProcessState::Terminated)
    {
        return;
    }

    loop {
        let (sig, action, old_mask) = {
            let p = match crate::process::get_process(pid) {
                Some(p) => p,
                None => break,
            };
            let old_mask = p.signal_mask;
            let Some((sig, action)) = crate::process::pop_lowest_pending_signal(pid) else {
                break;
            };
            (sig, action, old_mask)
        };

        if action.handler == 1 {
            // SIG_IGN — discard.
            continue;
        }

        if action.handler != 0 {
            // Userspace handler: try to push a signal frame.
            if push_rt_signal_frame(ctx, pid, sig, &action, old_mask, 0, 0) {
                // Successfully set up; handler will run on iretq.
                // Deliver one signal at a time per syscall return.
                break;
            }
            // Frame build failed (bad stack): fall through to fatal handling.
        }

        // SIG_DFL or frame-build failure.
        let is_fatal = sig == 9 // SIGKILL is always fatal
            || !crate::process::signal_default_is_ignore(sig);

        if !is_fatal {
            continue;
        }

        if let Some(mut proc) = crate::process::get_process(pid) {
            proc.proc.lock().exit_code = (128 + sig as u64) as i32;
            crate::process::update_process(pid, proc);
        }
        exit_process();
        yield_cpu();
        return;
    }
}

/// Deliver a signal to a userspace process directly from the exception handler.
pub fn deliver_signal_from_exception(
    exc:        &mut crate::interrupts::ExceptionContext,
    pid:        crate::process::ProcessId,
    signum:     u8,
    si_code:    i32,
    fault_addr: u64,
) -> bool {
    use crate::process::{SA_ONSTACK, SA_NODEFER, SA_RESETHAND, SS_DISABLE};
    use super::RtSigframe;
    use super::UContext;
    use super::SigContext;
    use super::StackT;
    use super::SigInfo;

    let (action, old_mask, user_rsp, alt_sp, alt_sz, alt_flags, using_altstack) = {
        let p = match crate::process::get_process(pid) {
            Some(p) => p,
            None    => return false,
        };
        let action = p.proc.lock().signal_actions[signum as usize];
        if action.handler == 0 || action.handler == 1 || action.restorer == 0 {
            return false;
        }
        let old_mask = p.signal_mask;
        let ss = p.sigaltstack;
        let rsp = exc.rsp;
        let mut using_altstack = false;
        let rsp = if (action.flags & SA_ONSTACK) != 0
            && (ss.ss_flags & SS_DISABLE) == 0
        {
            if (ss.ss_flags & crate::process::SS_ONSTACK) == 0 {
                using_altstack = true;
                ss.ss_sp.wrapping_add(ss.ss_size)
            } else {
                rsp
            }
        } else {
            rsp
        };
        (action, old_mask, rsp, ss.ss_sp, ss.ss_size, ss.ss_flags, using_altstack)
    };

    const FRAME_SZ: u64 = core::mem::size_of::<RtSigframe>() as u64;
    let frame_addr = (user_rsp.wrapping_sub(FRAME_SZ)) & !15u64;

    let mut frame = RtSigframe {
        pretcode: action.restorer,
        uc: UContext {
            uc_flags:    0,
            uc_link:     0,
            uc_stack:    StackT { 
                ss_sp:    alt_sp, 
                ss_flags: alt_flags, 
                _pad:     0, 
                ss_size:  alt_sz 
            },
            uc_mcontext: SigContext {
                r8:      exc.r8,
                r9:      exc.r9,
                r10:     exc.r10,
                r11:     exc.r11,
                r12:     exc.r12,
                r13:     exc.r13,
                r14:     exc.r14,
                r15:     exc.r15,
                rdi:     exc.rdi,
                rsi:     exc.rsi,
                rbp:     exc.rbp,
                rbx:     exc.rbx,
                rdx:     exc.rdx,
                rax:     exc.rax,
                rcx:     exc.rcx,
                rsp:     exc.rsp,
                rip:     exc.rip,
                eflags:  exc.rflags,
                cs:      exc.cs as u16,
                gs:      0,
                fs:      0,
                ss:      exc.ss as u16,
                err:     exc.error_code,
                trapno:  exc.num,
                oldmask: old_mask,
                cr2:     fault_addr,
                fpstate: 0,
                _reserved1: [0u64; 8],
            },
            uc_sigmask: old_mask,
        },
        info: SigInfo {
            si_signo: signum as i32,
            si_errno: 0,
            si_code:  si_code,
            _rest:    {
                let mut r = [0u8; 116];
                let addr_bytes = fault_addr.to_ne_bytes();
                for i in 0..8 {
                    r[4 + i] = addr_bytes[i];
                }
                r
            },
        },
        _pad: 0,
        fpstate: [0; 512],
    };

    // Save FPU state to the frame.
    unsafe {
        core::arch::asm!("fxsave [{}]", in(reg) &mut frame.fpstate[0]);
    }

    // Point uc_mcontext.fpstate to the fpstate buffer ON THE USER STACK.
    frame.uc.uc_mcontext.fpstate = frame_addr + 448;

    // Write the frame to user memory safely.
    let frame_bytes = unsafe {
        core::slice::from_raw_parts(&frame as *const RtSigframe as *const u8, FRAME_SZ as usize)
    };
    
    if !copy_to_user(frame_addr, frame_bytes) {
        return false;
    }

    // Block this signal during the handler (and any additional signals from sa_mask),
    // unless SA_NODEFER is set.
    let _ = crate::process::modify_process(pid, |p| {
        if (action.flags & SA_NODEFER) == 0 {
            p.signal_mask |= 1u64 << signum;
        }
        p.signal_mask |= action.mask;
        
        // Set SS_ONSTACK if we moved to the altstack.
        if using_altstack {
            p.sigaltstack.ss_flags |= crate::process::SS_ONSTACK;
        }
    });

    // SA_RESETHAND: reset handler to SIG_DFL after first delivery.
    if (action.flags & SA_RESETHAND) != 0 {
        let _ = crate::process::modify_process(pid, |p| {
            p.proc.lock().signal_actions[signum as usize].handler = 0;
        });
    }

    // Redirect the iretq to the signal handler.
    exc.rsp = frame_addr;
    exc.rip = action.handler;
    exc.rdi = signum as u64;
    exc.rsi = frame_addr + 312; // Offset of 'info'
    exc.rdx = frame_addr + 8;   // Offset of 'uc'
    exc.rax = 0;
    
    // Clear RFLAGS.TF to prevent single-stepping into handler.
    exc.rflags &= !0x100;
    exc.rflags |= 0x200; // Ensure IF=1 on return

    true
}

pub fn sys_rt_sigaction(sig: u64, act_ptr: u64, old_ptr: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 { return super::linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    
    if old_ptr != 0 {
        if !is_user_pointer(old_ptr, core::mem::size_of::<crate::process::SignalAction>() as u64) {
            return super::linux_abi_error(14);
        }
        if let Some(p) = crate::process::get_process(pid) {
            let proc = p.proc.lock();
            if (sig as usize) < proc.signal_actions.len() {
                let old = proc.signal_actions[sig as usize];
                let out = unsafe {
                    core::slice::from_raw_parts(&old as *const crate::process::SignalAction as *const u8, core::mem::size_of::<crate::process::SignalAction>())
                };
                if !copy_to_user(old_ptr, out) { return super::linux_abi_error(14); }
            }
        }
    }
    
    if act_ptr != 0 {
        if !is_user_pointer(act_ptr, core::mem::size_of::<crate::process::SignalAction>() as u64) {
            return super::linux_abi_error(14);
        }
        let mut act = core::mem::MaybeUninit::<crate::process::SignalAction>::uninit();
        let act_bytes = unsafe {
            core::slice::from_raw_parts_mut(act.as_mut_ptr() as *mut u8, core::mem::size_of::<crate::process::SignalAction>())
        };
        if !copy_from_user(act_ptr, act_bytes) { return super::linux_abi_error(14); }
        let act = unsafe { act.assume_init() };
        let _ = crate::process::modify_process(pid, |p| {
            let mut proc = p.proc.lock();
            if (sig as usize) < proc.signal_actions.len() {
                proc.signal_actions[sig as usize] = act;
            }
        });
    }
    0
}

pub fn sys_rt_sigprocmask(how: u64, set_ptr: u64, old_ptr: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 { return super::linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    
    if old_ptr != 0 {
        if !is_user_pointer(old_ptr, 8) { return super::linux_abi_error(14); }
        if let Some(p) = crate::process::get_process(pid) {
            if !copy_to_user(old_ptr, &p.signal_mask.to_le_bytes()) { return super::linux_abi_error(14); }
        }
    }
    
    if set_ptr != 0 {
        if !is_user_pointer(set_ptr, 8) { return super::linux_abi_error(14); }
        let mut b = [0u8; 8];
        if !copy_from_user(set_ptr, &mut b) { return super::linux_abi_error(14); }
        let set = u64::from_le_bytes(b);
        let _ = crate::process::modify_process(pid, |p| {
            match how {
                0 => p.signal_mask |= set, // SIG_BLOCK
                1 => p.signal_mask &= !set, // SIG_UNBLOCK
                2 => p.signal_mask = set,   // SIG_SETMASK
                _ => {}
            }
            // SIGKILL and SIGSTOP are unblockable.
            p.signal_mask &= !((1u64 << 8) | (1u64 << 18));
        });
    }
    0
}

pub fn sys_rt_sigreturn(ctx: &mut crate::interrupts::SyscallContext) -> u64 {
    use super::RtSigframe;
    let pid = current_process_id().unwrap_or(0);
    let frame_ptr = ctx.rsp; // RIP is at frame_ptr + pretcode(8) + uc(304)... no, rsp points to frame
    
    if !is_user_pointer(frame_ptr, core::mem::size_of::<RtSigframe>() as u64) {
        exit_process();
        return 0;
    }
    
    let mut frame = unsafe { core::mem::MaybeUninit::<RtSigframe>::uninit().assume_init() };
    if !copy_from_user(frame_ptr, unsafe { core::slice::from_raw_parts_mut(&mut frame as *mut _ as *mut u8, core::mem::size_of::<RtSigframe>()) }) {
        exit_process();
        return 0;
    }
    
    // Restore registers from uc_mcontext
    let m = &frame.uc.uc_mcontext;
    ctx.r8 = m.r8; ctx.r9 = m.r9; ctx.r10 = m.r10; ctx.r11 = m.r11;
    ctx.r12 = m.r12; ctx.r13 = m.r13; ctx.r14 = m.r14; ctx.r15 = m.r15;
    ctx.rdi = m.rdi; ctx.rsi = m.rsi; ctx.rbp = m.rbp; ctx.rbx = m.rbx;
    ctx.rdx = m.rdx; ctx.rax = m.rax; ctx.rcx = m.rcx; ctx.rsp = m.rsp;
    ctx.rip = m.rip; ctx.rflags = m.eflags;
    
    // Restore signal mask
    let _ = crate::process::modify_process(pid, |p| {
        p.signal_mask = frame.uc.uc_sigmask;
        // SIGKILL and SIGSTOP are unblockable.
        p.signal_mask &= !((1u64 << 8) | (1u64 << 18));
        
        // Clear SS_ONSTACK if we were using it.
        if (frame.uc.uc_stack.ss_flags & crate::process::SS_ONSTACK) != 0 {
            p.sigaltstack.ss_flags &= !crate::process::SS_ONSTACK;
        }
    });

    // Restore FPU state
    unsafe {
        core::arch::asm!("fxrstor [{}]", in(reg) &frame.fpstate[0]);
    }
    
    ctx.rax
}

pub fn sys_kill(pid: u64, sig: u64) -> u64 {
    if pid == 0 || pid == 1 {
        return super::linux_abi_error(1); // EPERM
    }

    let target_pid = pid as crate::process::ProcessId;

    if sig == 0 {
        return if crate::process::get_process(target_pid).is_some() {
            0
        } else {
            super::linux_abi_error(3) // ESRCH
        };
    }

    if sig == 9 {
        let parent_pid = match crate::process::terminate_other_process_by_signal(target_pid, 9) {
            None => return super::linux_abi_error(3),
            Some(pp) => pp,
        };

        if let Some(ppid) = parent_pid {
            crate::process::wake_parent_from_wait(ppid);
        }
        return 0;
    }

    crate::process::set_pending_signal(target_pid, sig as u8);
    0
}

pub fn sys_tkill(tid: u64, sig: u64) -> u64 {
    // In our model TID == PID for now (single thread per process)
    sys_kill(tid, sig)
}

pub fn sys_rt_sigpending(set_ptr: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 { return super::linux_abi_error(22); }
    if !is_user_pointer(set_ptr, 8) { return super::linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(p) = crate::process::get_process(pid) {
        if !copy_to_user(set_ptr, &p.pending_signals.to_le_bytes()) { return super::linux_abi_error(14); }
        return 0;
    }
    super::linux_abi_error(3)
}

pub fn sys_sigaltstack(ss_ptr: u64, old_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    
    if old_ptr != 0 {
        if !is_user_pointer(old_ptr, core::mem::size_of::<crate::process::Sigaltstack>() as u64) {
            return super::linux_abi_error(14);
        }
        if let Some(p) = crate::process::get_process(pid) {
            let out = unsafe {
                core::slice::from_raw_parts(&p.sigaltstack as *const crate::process::Sigaltstack as *const u8, core::mem::size_of::<crate::process::Sigaltstack>())
            };
            if !copy_to_user(old_ptr, out) { return super::linux_abi_error(14); }
        }
    }
    
    if ss_ptr != 0 {
        if !is_user_pointer(ss_ptr, core::mem::size_of::<crate::process::Sigaltstack>() as u64) {
            return super::linux_abi_error(14);
        }
        let mut ss = core::mem::MaybeUninit::<crate::process::Sigaltstack>::uninit();
        let ss_bytes = unsafe {
            core::slice::from_raw_parts_mut(ss.as_mut_ptr() as *mut u8, core::mem::size_of::<crate::process::Sigaltstack>())
        };
        if !copy_from_user(ss_ptr, ss_bytes) { return super::linux_abi_error(14); }
        let ss = unsafe { ss.assume_init() };
        
        // Cannot change altstack if currently using it
        if let Some(p) = crate::process::get_process(pid) {
            if (p.sigaltstack.ss_flags & crate::process::SS_ONSTACK) != 0 {
                return super::linux_abi_error(16); // EBUSY
            }
        }
        
        let _ = crate::process::modify_process(pid, |p| {
            p.sigaltstack = ss;
        });
    }
    0
}

pub fn sys_signalfd4(fd: u64, mask_ptr: u64, sigsetsize: u64, flags: u64) -> u64 {
    // Basic stub for now, would return a file descriptor that can be read to get signals
    if sigsetsize != 8 { return super::linux_abi_error(22); }
    super::linux_abi_error(38) // ENOSYS
}
