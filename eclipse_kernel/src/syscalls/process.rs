//! Syscalls de gestión de procesos para Eclipse OS
//! Implementa el ciclo de vida de los procesos, hilos, señales y permisos.

use super::*;
use crate::process::{self, ProcessId, exit_process, current_process_id};
use alloc::vec::Vec;
use alloc::string::String;

pub fn sys_exit(exit_code: u64) -> u64 {
    SYSCALL_STATS.exit_calls.fetch_add(1, Ordering::Relaxed);
    let pid = current_process_id().unwrap_or(0);
    if let Some(mut proc) = crate::process::get_process(pid) {
        proc.exit_code = exit_code;
        crate::process::update_process(pid, proc);
    }
    exit_process();
    yield_cpu();
    0
}

pub fn sys_getpid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(p) = crate::process::get_process(pid) {
            return p.tgid as u64;
        }
    }
    0
}

pub fn sys_getppid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(proc) = crate::process::get_process(pid) {
            if let Some(ppid) = proc.parent_pid {
                return ppid as u64;
            }
        }
    }
    0
}

pub fn sys_gettid() -> u64 {
    current_process_id().unwrap_or(0) as u64
}

pub fn sys_fork(context: &crate::process::Context) -> u64 {
    SYSCALL_STATS.fork_calls.fetch_add(1, Ordering::Relaxed);
    let mut child_context = *context;
    child_context.rax = 0;
    match process::fork_process(&child_context) {
        Some(child_pid) => {
            crate::scheduler::enqueue_process(child_pid);
            child_pid as u64
        }
        None => super::linux_abi_error(11) // EAGAIN
    }
}

pub fn sys_clone(flags: u64, stack: u64, _ptid: u64, context: &crate::process::Context) -> u64 {
    // Basic clone implementation
    let mut child_context = *context;
    child_context.rax = 0;
    if stack != 0 { child_context.rsp = stack; }
    
    match process::fork_process(&child_context) {
        Some(child_pid) => {
            let _ = flags;
            crate::scheduler::enqueue_process(child_pid);
            child_pid as u64
        }
        None => super::linux_abi_error(11)
    }
}

pub fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    const MAX_PATH: usize = 1024;
    let path_len = strlen_user_unique(path_ptr, MAX_PATH);
    if path_ptr == 0 || path_len == 0 { return super::linux_abi_error(14); }
    
    let mut path_buf = Vec::with_capacity(path_len as usize);
    unsafe {
        path_buf.set_len(path_len as usize);
        super::copy_from_user(path_ptr, &mut path_buf);
    }
    let path_str = core::str::from_utf8(&path_buf).unwrap_or("");
    
    // Resolve path
    let current_pid = current_process_id().unwrap_or(0);
    let resolved_path = if path_str.starts_with('/') { String::from(path_str) }
                        else { crate::process::resolve_path_cwd(current_pid, path_str) };

    // Read argv
    let mut argv_strings = Vec::new();
    if argv_ptr != 0 {
        let mut off = 0;
        loop {
            let mut arg_ptr = 0u64;
            if !super::copy_from_user(argv_ptr + off, unsafe { core::slice::from_raw_parts_mut(&mut arg_ptr as *mut _ as *mut u8, 8) }) { break; }
            if arg_ptr == 0 { break; }
            let arg_len = strlen_user_unique(arg_ptr, 4096);
            let mut arg_data = Vec::with_capacity(arg_len as usize + 1);
            unsafe {
                arg_data.set_len(arg_len as usize);
                super::copy_from_user(arg_ptr, &mut arg_data);
            }
            if let Ok(s) = String::from_utf8(arg_data) {
                argv_strings.push(s);
            }
            off += 8;
        }
    }

    // Read envp
    let mut envp_strings = Vec::new();
    if envp_ptr != 0 {
        let mut off = 0;
        loop {
            let mut env_ptr = 0u64;
            if !super::copy_from_user(envp_ptr + off, unsafe { core::slice::from_raw_parts_mut(&mut env_ptr as *mut _ as *mut u8, 8) }) { break; }
            if env_ptr == 0 { break; }
            let env_len = strlen_user_unique(env_ptr, 4096);
            let mut env_data = Vec::with_capacity(env_len as usize + 1);
            unsafe {
                env_data.set_len(env_len as usize);
                super::copy_from_user(env_ptr, &mut env_data);
            }
            if let Ok(s) = String::from_utf8(env_data) {
                envp_strings.push(s);
            }
            off += 8;
        }
    }

    // Exec via elf_loader
    let res = crate::elf_loader::replace_process_image_path(current_pid, resolved_path.as_str());
    match res {
        Ok(_) => 0,
        Err(_) => super::linux_abi_error(2), // ENOENT or EIO
    }
}

pub fn sys_getuid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(p) = crate::process::get_process(pid) { return p.uid as u64; }
    }
    0
}

pub fn sys_getgid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(p) = crate::process::get_process(pid) { return p.gid as u64; }
    }
    0
}

pub fn sys_geteuid() -> u64 { sys_getuid() }
pub fn sys_getegid() -> u64 { sys_getgid() }

pub fn sys_setpgid(_pid: u64, _pgid: u64) -> u64 {
    0
}

pub fn sys_setsid() -> u64 {
    if let Some(pid) = current_process_id() {
        return pid as u64;
    }
    0
}

pub fn sys_kill(_pid: u64, _sig: u64) -> u64 {
    0
}

pub fn sys_rt_sigaction(signum: u64, act_ptr: u64, oldact_ptr: u64, _sigsetsize: u64) -> u64 {
    if signum >= 64 { return super::linux_abi_error(22); }
    if let Some(pid) = current_process_id() {
        if oldact_ptr != 0 {
            if let Some(old) = crate::process::get_signal_handler(pid, signum as u8) {
                let old_bytes = unsafe { core::slice::from_raw_parts(&old as *const _ as *const u8, 32) };
                super::copy_to_user(oldact_ptr, old_bytes);
            }
        }
        if act_ptr != 0 {
            let mut act = crate::process::SignalAction::new();
            let act_bytes = unsafe { core::slice::from_raw_parts_mut(&mut act as *mut _ as *mut u8, 32) };
            super::copy_from_user(act_ptr, act_bytes);
            crate::process::set_signal_handler(pid, signum as u8, act);
        }
        return 0;
    }
    super::linux_abi_error(1)
}

pub fn sys_rt_sigprocmask(_how: u64, _set_ptr: u64, _oldset_ptr: u64, _sigsetsize: u64) -> u64 {
    0
}

pub fn sys_rt_sigreturn(_context: &mut crate::interrupts::SyscallContext) -> u64 {
    0
}

pub fn sys_sigaltstack(_ss_ptr: u64, _oss_ptr: u64) -> u64 {
    0
}

pub fn sys_arch_prctl(code: u64, addr: u64, context: &mut crate::interrupts::SyscallContext) -> u64 {
    match code {
        0x1001 => { // ARCH_SET_GS
            context.gs_base = addr;
            unsafe { x86_64::registers::model_specific::Msr::new(0xC0000101).write(addr); }
            0
        }
        0x1002 => { // ARCH_SET_FS
            context.fs_base = addr;
            unsafe { x86_64::registers::model_specific::Msr::new(0xC0000100).write(addr); }
            0
        }
        0x1003 => { // ARCH_GET_FS
            super::copy_to_user(addr, &context.fs_base.to_le_bytes());
            0
        }
        0x1004 => { // ARCH_GET_GS
            super::copy_to_user(addr, &context.gs_base.to_le_bytes());
            0
        }
        _ => super::linux_abi_error(22),
    }
}

pub fn sys_futex(uaddr: u64, op: u64, val: u64, _timeout: u64, _uaddr2: u64, _val3: u32) -> u64 {
    let op_type = op & 0x7F;
    match op_type {
        0 => { // FUTEX_WAIT
            let mut mem_val = 0u32;
            super::copy_from_user(uaddr, unsafe { core::slice::from_raw_parts_mut(&mut mem_val as *mut _ as *mut u8, 4) });
            if mem_val != val as u32 { return super::linux_abi_error(11); }
            yield_cpu();
            0
        }
        1 => { // FUTEX_WAKE
            0
        }
        _ => super::linux_abi_error(38),
    }
}

pub fn sys_set_tid_address(ptr: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(mut p) = crate::process::get_process(pid) {
            p.clear_child_tid = ptr;
            crate::process::update_process(pid, p);
        }
    }
    current_process_id().unwrap_or(0) as u64
}

pub fn sys_spawn(path_ptr: u64, argv_ptr: u64) -> u64 {
    sys_execve(path_ptr, argv_ptr, 0)
}

pub fn deliver_signal_from_exception(_context: &mut crate::interrupts::ExceptionContext, _pid: u32, _signum: u8, _si_code: i32, _cr2: u64) -> bool {
    // Stub
    false
}

pub fn futex_wake_all_atomic(_uaddr: u64) {
    // Stub
}

pub fn sys_wait4_linux(_pid: u64, _status_ptr: u64, _options: u64, _rusage_ptr: u64) -> u64 {
    super::linux_abi_error(10)
}
