//! Process-related syscalls implementation
//!
//! Implementation of process lifecycle, scheduling affinity, and threading.

use alloc::vec::Vec;
use crate::process::{ProcessId, exit_process, current_process_id};
use crate::scheduler::yield_cpu;
use super::{copy_from_user, copy_to_user, is_user_pointer, linux_abi_error};

pub fn sys_getpid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(p) = crate::process::get_process(pid) {
            return p.tgid as u64;
        }
    }
    0
}

pub fn sys_gettid() -> u64 {
    current_process_id().unwrap_or(0) as u64
}

pub fn sys_getppid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(process) = crate::process::get_process(pid) {
            return process.proc.lock().parent_pid.unwrap_or(0) as u64;
        }
    }
    0
}

pub fn sys_fork(context: &crate::interrupts::SyscallContext) -> u64 {
    let ctx = crate::process::Context {
        rsp: context.rsp, rip: context.rip, rflags: context.rflags,
        rbp: context.rbp, rax: 0, rbx: context.rbx, rcx: context.rcx,
        rdx: context.rdx, rsi: context.rsi, rdi: context.rdi,
        r8: context.r8, r9: context.r9, r10: context.r10, r11: context.r11,
        r12: context.r12, r13: context.r13, r14: context.r14, r15: context.r15,
        fs_base: context.fs_base, gs_base: context.gs_base,
    };
    match crate::process::fork_process(&ctx) {
        Some(child_pid) => {
            crate::scheduler::enqueue_process(child_pid);
            child_pid as u64
        }
        None => linux_abi_error(11), // EAGAIN
    }
}

pub fn sys_exit(exit_code: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        process.proc.lock().exit_code = exit_code as i32;
    }
    exit_process();
    yield_cpu();
    0
}

pub fn sys_wait_pid(status_ptr: u64, wait_pid: u64, flags: u64) -> u64 {
    sys_wait_impl(status_ptr, wait_pid, flags)
}

pub fn sys_waitid(idtype: u64, id: u64, infop: u64, options: u64, _rusage: u64) -> u64 {
    let wait_pid = if idtype == 1 { id } else { 0 };
    let res = sys_wait_impl(0, wait_pid, options);
    if (res as i64) < 0 { return res; }
    if infop != 0 && is_user_pointer(infop, 128) {
        unsafe {
            let ptr = infop as *mut i32;
            ptr.write_unaligned(17); // SIGCHLD
            ptr.add(1).write_unaligned(0);
            ptr.add(2).write_unaligned(1); // CLD_EXITED
            ((infop + 16) as *mut i32).write_unaligned(res as i32);
        }
    }
    0
}

fn sys_wait_impl(status_ptr: u64, wait_pid: u64, flags: u64) -> u64 {
    let wnohang = (flags & 1) != 0;
    let wuntraced = (flags & 2) != 0;
    let current_pid = match current_process_id() { Some(pid) => pid, None => return u64::MAX };

    loop {
        let mut found_eligible_child = false;
        let mut terminated_child = None;

        {
            let mut table = crate::process::PROCESS_TABLE.lock();
            for slot in table.iter_mut() {
                if let Some(p) = slot {
                    let mut proc = p.proc.lock();
                    if proc.parent_pid == Some(current_pid) {
                        if wait_pid != 0 && p.id != wait_pid as u32 { continue; }
                        found_eligible_child = true;
                        if p.state == crate::process::ProcessState::Terminated {
                            terminated_child = Some((p.id, proc.exit_code));
                            break;
                        }
                        if wuntraced && p.state == crate::process::ProcessState::Stopped && !proc.notified_stopped {
                            terminated_child = Some((p.id, (0x7F | ((proc.exit_signal as u64) << 8)) as i32));
                            proc.notified_stopped = true;
                            break;
                        }
                    }
                }
            }
        }

        if let Some((child_pid, status)) = terminated_child {
            if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                let b = (status as u32).to_le_bytes();
                if !copy_to_user(status_ptr, &b) {
                    return linux_abi_error(14); // EFAULT
                }
            }
            if (status & 0x7F) == 0 { crate::process::remove_process(child_pid); }
            crate::process::unregister_child_waiter(current_pid);
            return child_pid as u64;
        }

        if !found_eligible_child { return linux_abi_error(10); } // ECHILD
        if wnohang { return 0; }

        if let Some(mut proc) = crate::process::get_process(current_pid) {
            proc.state = crate::process::ProcessState::WaitingForChild;
            crate::process::update_process(current_pid, proc);
            crate::process::register_child_waiter(current_pid);
        }
        yield_cpu();
    }
}

pub fn sys_exec(elf_ptr: u64, elf_size: u64) -> u64 {
    if elf_ptr == 0 || elf_size == 0 || elf_size > 128 * 1024 * 1024 { return u64::MAX; }
    if !is_user_pointer(elf_ptr, elf_size) { return u64::MAX; }
    
    let src = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
    let mut elf_data = Vec::with_capacity(elf_size as usize);
    elf_data.extend_from_slice(src);

    let current_pid = current_process_id().expect("exec without process");
    let _ = crate::process::vfork_detach_mm_for_exec_if_needed(current_pid);
    
    match crate::elf_loader::replace_process_image(current_pid, &elf_data) {
        Ok(res) => {
            if let Some(mut process) = crate::process::get_process(current_pid) {
                {
                    let proc = process.proc.lock();
                    let mut r = proc.resources.lock();
                    r.vmas.clear();
                    r.brk_current = res.max_vaddr;
                }
                
                {
                    let mut proc = process.proc.lock();
                    proc.mem_frames = (0x100000 / 4096) + res.segment_frames;
                    proc.dynamic_linker_aux = res.dynamic_linker;
                }
                
                process.fs_base = if res.dynamic_linker.is_some() { 0 } else { res.tls_base };
                crate::process::update_process(current_pid, process);
            }
            crate::process::clear_pending_process_args(current_pid);
            
            const STACK_BASE: u64 = 0x2000_0000;
            const STACK_SIZE: usize = 0x10_0000;
            let cr3 = crate::memory::get_cr3();
            let _ = crate::elf_loader::setup_user_stack(cr3, STACK_BASE, STACK_SIZE);
            crate::process::register_post_exec_vm_as(current_pid, &res, STACK_BASE, STACK_SIZE as u64);
            crate::fd::fd_ensure_stdio(current_pid);
            
            unsafe {
                let stack_top = STACK_BASE + STACK_SIZE as u64;
                if res.dynamic_linker.is_some() {
                    crate::elf_loader::jump_to_userspace_dynamic_linker(res.entry_point, stack_top, res.phdr_va, res.phnum, res.phentsize);
                } else {
                    crate::elf_loader::jump_to_userspace(res.entry_point, stack_top, res.phdr_va, res.phnum, res.phentsize);
                }
            }
        }
        Err(_) => u64::MAX,
    }
}

pub fn sys_spawn(elf_ptr: u64, elf_size: u64, name_ptr: u64) -> u64 {
    if !is_user_pointer(elf_ptr, elf_size) { return u64::MAX; }
    let mut name_buf = [0u8; 16];
    if name_ptr != 0 {
        let len = super::strlen_user_unique(name_ptr, 15);
        let _ = super::copy_from_user(name_ptr, &mut name_buf[..len]);
    }
    let name = core::str::from_utf8(&name_buf).unwrap_or("unknown").trim_matches('\0');
    let elf_data = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
    
    match crate::process::spawn_process(elf_data, name) {
        Ok(pid) => {
            let parent = current_process_id().unwrap_or(1);
            if let Some(process) = crate::process::get_process(pid) {
                process.proc.lock().parent_pid = Some(parent);
            }
            pid as u64
        }
        Err(_) => u64::MAX,
    }
}

pub fn sys_spawn_service(service_id: u64, name_ptr: u64, name_len: u64) -> u64 {
    use eclipse_program_codes::spawn_service as svc;
    let path = match service_id {
        x if x == svc::LOG as u64 => svc::PATH_LOG,
        x if x == svc::DEVFS as u64 => svc::PATH_DEVFS,
        x if x == svc::FILESYSTEM as u64 => svc::PATH_FILESYSTEM,
        x if x == svc::INPUT as u64 => svc::PATH_INPUT,
        x if x == svc::DISPLAY as u64 => svc::PATH_DISPLAY,
        x if x == svc::AUDIO as u64 => svc::PATH_AUDIO,
        x if x == svc::NETWORK as u64 => svc::PATH_NETWORK,
        x if x == svc::GUI as u64 => svc::PATH_GUI,
        x if x == svc::SEATD as u64 => svc::PATH_SEATD,
        _ => return u64::MAX,
    };

    let elf_data = match crate::filesystem::read_file_alloc(path) {
        Ok(buf) => buf,
        Err(_) => return u64::MAX,
    };

    let mut name_buf = [0u8; 16];
    if name_ptr != 0 {
        let copy_len = (name_len as usize).min(15);
        let _ = super::copy_from_user(name_ptr, &mut name_buf[..copy_len]);
    }
    let name_str = core::str::from_utf8(&name_buf).unwrap_or("");
    let name = if name_str.trim_matches('\0').is_empty() {
        eclipse_program_codes::spawn_service_short_name(service_id as u32)
    } else {
        name_str.trim_matches('\0')
    };

    match crate::process::spawn_process(&elf_data, name) {
        Ok(pid) => {
            let parent = current_process_id().unwrap_or(1);
            if let Some(process) = crate::process::get_process(pid) {
                process.proc.lock().parent_pid = Some(parent);
            }
            crate::scheduler::enqueue_process(pid);
            pid as u64
        }
        Err(_) => u64::MAX,
    }
}

pub fn sys_spawn_with_stdio(elf_ptr: u64, elf_size: u64, name_ptr: u64, fd_in: u64, fd_out: u64, fd_err: u64) -> u64 {
    if !is_user_pointer(elf_ptr, elf_size) { return u64::MAX; }
    let mut elf_data = Vec::with_capacity(elf_size as usize);
    elf_data.extend_from_slice(unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) });
    
    let mut name_buf = [0u8; 16];
    if name_ptr != 0 {
        let len = super::strlen_user_unique(name_ptr, 15);
        let _ = super::copy_from_user(name_ptr, &mut name_buf[..len]);
    }
    let name = core::str::from_utf8(&name_buf).unwrap_or("unknown").trim_matches('\0');

    match crate::process::spawn_process(&elf_data, name) {
        Ok(pid) => setup_stdio(pid, fd_in, fd_out, fd_err),
        Err(_) => u64::MAX,
    }
}

fn setup_stdio(pid: crate::process::ProcessId, fd_in: u64, fd_out: u64, fd_err: u64) -> u64 {
    let parent = current_process_id().unwrap_or(1);
    if let Some(process) = crate::process::get_process(pid) {
        process.proc.lock().parent_pid = Some(parent);
    }
    
    if let Some(child_idx) = crate::fd::pid_to_fd_idx(pid as u32) {
        let mut tables = crate::fd::FD_TABLES.lock();
        for (i, &fd) in [fd_in, fd_out, fd_err].iter().enumerate() {
            if let Some(p_fd) = crate::fd::fd_get(parent, fd as usize) {
                tables[child_idx].fds[i] = p_fd;
                if p_fd.in_use { let _ = crate::scheme::dup(p_fd.scheme_id, p_fd.resource_id); }
            }
        }
    }
    pid as u64
}

pub fn sys_spawn_with_stdio_args(pid: u64, args_ptr: u64, args_len: u64, _a: u64, _b: u64, _c: u64, _ctx: &mut crate::interrupts::SyscallContext) -> u64 {
    if !is_user_pointer(args_ptr, args_len) || args_len > 4096 { return u64::MAX; }
    let args = unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }.to_vec();
    crate::process::set_pending_process_args(pid as u32, args);
    crate::scheduler::enqueue_process(pid as u32);
    0
}

pub fn sys_get_process_args(buf_ptr: u64, buf_size: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_size as usize) };
    crate::process::copy_pending_process_args(pid, buf) as u64
}

pub fn sys_spawn_with_stdio_path(path_ptr: u64, name_ptr: u64, fd_in: u64, fd_out: u64, fd_err: u64, _a: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return u64::MAX; }
    let mut path_buf = [0u8; 1024];
    let _ = super::copy_from_user(path_ptr, &mut path_buf[..len]);
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");
    
    if let Some(pid) = crate::elf_loader::load_elf_path(path) {
        if name_ptr != 0 {
            let n_len = super::strlen_user_unique(name_ptr, 15);
            let mut n_buf = [0u8; 16];
            let _ = super::copy_from_user(name_ptr, &mut n_buf[..n_len]);
            if let Some(process) = crate::process::get_process(pid) {
                process.proc.lock().name[..16].copy_from_slice(&n_buf);
            }
        }
        return setup_stdio(pid, fd_in, fd_out, fd_err);
    }
    u64::MAX
}

pub fn sys_get_process_list(buf_ptr: u64, max_count: u64) -> u64 {
    use super::ProcessInfo;
    
    if buf_ptr == 0 || max_count == 0 { return 0; }
    if !is_user_pointer(buf_ptr, max_count * core::mem::size_of::<ProcessInfo>() as u64) {
        return u64::MAX;
    }

    let mut count = 0;
    let table = crate::process::PROCESS_TABLE.lock();
    for slot in table.iter() {
        if let Some(p) = slot {
            if count >= max_count as usize { break; }
            
            let mut name = [0u8; 32];
            let proc = p.proc.lock();
            name[..16].copy_from_slice(&proc.name);
            
            let info = ProcessInfo {
                pid: p.id,
                ppid: proc.parent_pid.unwrap_or(0),
                state: p.state as u32,
                cpu_usage: 0,
                mem_usage_kb: proc.mem_frames * 4,
                name,
                thread_count: 1,
                priority: 5,
            };
            
            unsafe {
                core::ptr::write_unaligned((buf_ptr as *mut ProcessInfo).add(count), info);
            }
            count += 1;
        }
    }
    count as u64
}

pub fn sys_set_process_name(name_ptr: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let mut buf = [0u8; 16];
    let l = (len as usize).min(15);
    let _ = super::copy_from_user(name_ptr, &mut buf[..l]);
    if let Some(process) = crate::process::get_process(pid) {
        process.proc.lock().name = buf;
    }
    0
}

pub fn sys_sched_setaffinity(pid: u64, cpu_id: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    let affinity = if cpu_id == u64::MAX { None } else { Some(cpu_id as u32) };
    let _ = crate::process::modify_process(target, |p| p.cpu_affinity = affinity);
    0
}

pub fn sys_sched_getaffinity(pid: u64, cpusetsize: u64, mask_ptr: u64) -> u64 {
    if cpusetsize < 8 || !is_user_pointer(mask_ptr, 8) { return linux_abi_error(22); }
    let mask: u64 = (1 << crate::cpu::get_active_cpu_count()) - 1;
    unsafe { *(mask_ptr as *mut u64) = mask; }
    0
}

pub fn sys_set_tid_address(tidptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let _ = crate::process::modify_process(pid, |p| p.clear_child_tid = tidptr);
    pid as u64
}

pub fn sys_clone(flags: u64, stack: u64, _parent_tid_ptr: u64, child_tid_ptr: u64, tls: u64, ctx: &crate::interrupts::SyscallContext) -> u64 {
    let mut child_ctx = crate::process::Context {
        rsp: if stack != 0 { stack } else { ctx.rsp },
        rip: ctx.rip,
        rflags: ctx.rflags,
        rbp: ctx.rbp,
        rax: 0,
        rbx: ctx.rbx,
        rcx: ctx.rcx,
        rdx: ctx.rdx,
        rsi: ctx.rsi,
        rdi: ctx.rdi,
        r8: ctx.r8,
        r9: ctx.r9,
        r10: ctx.r10,
        r11: ctx.r11,
        r12: ctx.r12,
        r13: ctx.r13,
        r14: ctx.r14,
        r15: ctx.r15,
        fs_base: if (flags & 0x80000) != 0 { tls } else { ctx.fs_base }, // CLONE_SETTLS
        gs_base: ctx.gs_base,
    };
    
    match crate::process::fork_process(&child_ctx) {
        Some(child_pid) => {
            if (flags & 0x200000) != 0 { // CLONE_CHILD_CLEARTID
                let _ = crate::process::modify_process(child_pid, |p| p.clear_child_tid = child_tid_ptr);
            }
            crate::scheduler::enqueue_process(child_pid);
            child_pid as u64
        }
        None => linux_abi_error(11),
    }
}

pub fn sys_execve(path_ptr: u64, _argv: u64, _envp: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    match crate::scheme::open(path, 0, 0) {
        Ok((sid, rid)) => {
            let mut stat = crate::scheme::Stat::default();
            let _ = crate::scheme::fstat(sid, rid, &mut stat);
            let mut elf_data = Vec::with_capacity(stat.size as usize);
            unsafe { elf_data.set_len(stat.size as usize); }
            let _ = crate::scheme::read(sid, rid, &mut elf_data, 0);
            let _ = crate::scheme::close(sid, rid);
            
            let current_pid = current_process_id().expect("exec without process");
            match crate::elf_loader::replace_process_image(current_pid, &elf_data) {
                Ok(res) => {
                    if let Some(mut process) = crate::process::get_process(current_pid) {
                        process.proc.lock().resources.lock().brk_current = res.max_vaddr;
                        process.fs_base = res.tls_base;
                    }
                    0
                }
                Err(_) => linux_abi_error(8), // ENOEXEC
            }
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_wait4_linux(pid: u64, status: u64, options: u64, _rusage: u64) -> u64 {
    sys_wait_impl(status, pid, options)
}

pub fn sys_ptrace(request: u64, pid: u64, addr: u64, data: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_prctl(option: u64, arg2: u64, _a: u64, _b: u64, _c: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    match option {
        15 => { // PR_SET_NAME
            let len = super::strlen_user_unique(arg2, 15);
            let mut name = [0u8; 16];
            if super::copy_from_user(arg2, &mut name[..len]) {
                if let Some(process) = crate::process::get_process(pid) {
                    process.proc.lock().name = name;
                }
            }
            0
        }
        _ => 0,
    }
}

pub fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    match code {
        0x1002 => { // SET_FS
            if let Some(mut process) = crate::process::get_process(pid) {
                process.fs_base = addr;
            }
            super::set_fs_base(addr);
            0
        }
        _ => linux_abi_error(22),
    }
}

pub fn sys_sched_set_deadline(pid: u32, runtime: u64, deadline: u64, period: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid };
    let _ = crate::process::modify_process(target, |p| {
        p.rt_params = Some(crate::process::RTParams {
            runtime,
            deadline,
            period,
            next_deadline: crate::interrupts::ticks() + deadline,
        });
    });
    0
}

pub fn sys_getuid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_uid(pid) as u64
}

pub fn sys_getgid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_gid(pid) as u64
}

pub fn sys_setuid(uid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        proc.uid = uid as u32;
        proc.euid = uid as u32;
        proc.suid = uid as u32;
    }
    0
}

pub fn sys_setgid(gid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        proc.gid = gid as u32;
        proc.egid = gid as u32;
        proc.sgid = gid as u32;
    }
    0
}

pub fn sys_geteuid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_euid(pid) as u64
}

pub fn sys_getegid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_egid(pid) as u64
}

pub fn sys_setpgid(pid: u64, pgid: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    let new_pgid = if pgid == 0 { target } else { pgid as u32 };
    if let Some(process) = crate::process::get_process(target) {
        process.proc.lock().pgid = new_pgid;
    }
    0
}

pub fn sys_getpgrp() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_pgid(pid) as u64
}

pub fn sys_setsid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        proc.sid = pid;
        proc.pgid = pid;
    }
    pid as u64
}

pub fn sys_setreuid(ruid: u64, euid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if ruid != u64::MAX { proc.uid = ruid as u32; }
        if euid != u64::MAX { proc.euid = euid as u32; }
    }
    0
}

pub fn sys_setregid(rgid: u64, egid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if rgid != u64::MAX { proc.gid = rgid as u32; }
        if egid != u64::MAX { proc.egid = egid as u32; }
    }
    0
}

pub fn sys_setresuid(ruid: u64, euid: u64, suid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if ruid != u64::MAX { proc.uid = ruid as u32; }
        if euid != u64::MAX { proc.euid = euid as u32; }
        if suid != u64::MAX { proc.suid = suid as u32; }
    }
    0
}

pub fn sys_setresgid(rgid: u64, egid: u64, sgid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if rgid != u64::MAX { proc.gid = rgid as u32; }
        if egid != u64::MAX { proc.egid = egid as u32; }
        if sgid != u64::MAX { proc.sgid = sgid as u32; }
    }
    0
}

pub fn sys_getresuid(ruid_ptr: u64, euid_ptr: u64, suid_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let proc = process.proc.lock();
        if ruid_ptr != 0 && super::is_user_pointer(ruid_ptr, 4) {
            let b = proc.uid.to_le_bytes();
            if !copy_to_user(ruid_ptr, &b) { return linux_abi_error(14); }
        }
        if euid_ptr != 0 && super::is_user_pointer(euid_ptr, 4) {
            let b = proc.euid.to_le_bytes();
            if !copy_to_user(euid_ptr, &b) { return linux_abi_error(14); }
        }
        if suid_ptr != 0 && super::is_user_pointer(suid_ptr, 4) {
            let b = proc.suid.to_le_bytes();
            if !copy_to_user(suid_ptr, &b) { return linux_abi_error(14); }
        }
        0
    } else { linux_abi_error(3) }
}

pub fn sys_getresgid(rgid_ptr: u64, egid_ptr: u64, sgid_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let proc = process.proc.lock();
        if rgid_ptr != 0 && super::is_user_pointer(rgid_ptr, 4) {
            let b = proc.gid.to_le_bytes();
            if !copy_to_user(rgid_ptr, &b) { return linux_abi_error(14); }
        }
        if egid_ptr != 0 && super::is_user_pointer(egid_ptr, 4) {
            let b = proc.egid.to_le_bytes();
            if !copy_to_user(egid_ptr, &b) { return linux_abi_error(14); }
        }
        if sgid_ptr != 0 && super::is_user_pointer(sgid_ptr, 4) {
            let b = proc.sgid.to_le_bytes();
            if !copy_to_user(sgid_ptr, &b) { return linux_abi_error(14); }
        }
        0
    } else { linux_abi_error(3) }
}

pub fn sys_getpgid(pid: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    crate::process::get_pgid(target) as u64
}

pub fn sys_get_last_exec_error(_buf: u64, _len: u64) -> u64 { 0 }

pub fn sys_thread_create(entry: u64, stack: u64, arg: u64, ctx: &crate::interrupts::SyscallContext) -> u64 {
    // Thread creation logic (simplified)
    sys_fork(ctx)
}

pub fn sys_strace(pid: u64, enable: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    if let Some(process) = crate::process::get_process(target) {
        process.proc.lock().syscall_trace = enable != 0;
    }
    0
}
