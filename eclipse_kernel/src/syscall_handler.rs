//! Syscall entry point and MSR configuration
//! 
//! This module sets up the SYSCALL/SYSRET mechanism for fast system calls

use crate::debug::serial_write_str;
use crate::syscalls::{SyscallArgs, SyscallResult};
use core::arch::asm;
use spin::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    /// Global process manager for syscalls
    static ref SYSCALL_PROCESS_MANAGER: Mutex<Option<crate::process::manager::ProcessManager>> = {
        Mutex::new(None)
    };
    
    /// Current process ID (simulated for now)
    static ref CURRENT_PID: Mutex<u32> = Mutex::new(1);
}

/// Initialize the process manager for syscalls
fn init_process_manager_if_needed() {
    let mut manager_guard = SYSCALL_PROCESS_MANAGER.lock();
    if manager_guard.is_none() {
        serial_write_str("SYSCALL: Initializing process manager\n");
        let mut manager = crate::process::manager::ProcessManager::new();
        if let Err(e) = manager.init() {
            serial_write_str(&alloc::format!("SYSCALL: Failed to init manager: {}\n", e));
        } else {
            serial_write_str("SYSCALL: Process manager initialized\n");
        }
        *manager_guard = Some(manager);
    }
}

/// Get current process ID
fn get_current_pid() -> u32 {
    *CURRENT_PID.lock()
}

/// Set current process ID
fn set_current_pid(pid: u32) {
    *CURRENT_PID.lock() = pid;
}

/// MSR addresses for syscall setup
const IA32_STAR: u32 = 0xC0000081;
const IA32_LSTAR: u32 = 0xC0000082;
const IA32_FMASK: u32 = 0xC0000084;
const IA32_EFER: u32 = 0xC0000080;

/// EFER flags
const EFER_SCE: u64 = 1 << 0;  // System Call Extensions

/// Kernel/user segment selectors
const KERNEL_CS: u64 = 0x08;
const KERNEL_DS: u64 = 0x10;
const USER_CS: u64 = 0x2B;      // RPL=3, index=5 (0x28 + 3)
const USER_DS: u64 = 0x23;      // RPL=3, index=4 (0x20 + 3)

/// Per-CPU kernel data
#[repr(C)]
pub struct KernelCpuData {
    pub kernel_rsp: u64,        // Offset 0: Kernel stack pointer
    pub user_rsp: u64,          // Offset 8: User stack pointer
}

/// Global kernel CPU data
static mut KERNEL_CPU_DATA: KernelCpuData = KernelCpuData {
    kernel_rsp: 0,
    user_rsp: 0,
};

/// Write to MSR
unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") low,
        in("edx") high,
        options(nomem, nostack),
    );
}

/// Read from MSR
unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") low,
        out("edx") high,
        options(nomem, nostack),
    );
    ((high as u64) << 32) | (low as u64)
}

/// External assembly syscall entry point
extern "C" {
    fn syscall_entry();
}

/// Initialize syscall mechanism
pub fn init_syscall() -> Result<(), &'static str> {
    serial_write_str("SYSCALL: Initializing syscall mechanism\n");
    
    unsafe {
        // Set up kernel stack for syscalls
        // Allocate a dedicated kernel stack for syscalls (8KB)
        let kernel_stack = alloc::vec![0u8; 8192];
        let kernel_stack_top = kernel_stack.as_ptr() as u64 + 8192;
        core::mem::forget(kernel_stack); // Don't drop the stack
        
        KERNEL_CPU_DATA.kernel_rsp = kernel_stack_top;
        
        // Set GS base to point to kernel CPU data
        // We'll use SWAPGS to access this
        let kernel_data_ptr = &KERNEL_CPU_DATA as *const _ as u64;
        
        // Set KERNEL_GS_BASE MSR for SWAPGS
        const IA32_KERNEL_GS_BASE: u32 = 0xC0000102;
        wrmsr(IA32_KERNEL_GS_BASE, kernel_data_ptr);
        
        // Enable SYSCALL/SYSRET by setting SCE bit in EFER
        let mut efer = rdmsr(IA32_EFER);
        efer |= EFER_SCE;
        wrmsr(IA32_EFER, efer);
        
        // Set up STAR register (kernel/user code segments)
        // Bits 63:48 = User CS (will be +16 for SS)
        // Bits 47:32 = Kernel CS (will be +8 for SS)
        let star = (USER_CS << 48) | (KERNEL_CS << 32);
        wrmsr(IA32_STAR, star);
        
        // Set up LSTAR register (syscall entry point)
        let entry_point = syscall_entry as u64;
        wrmsr(IA32_LSTAR, entry_point);
        
        // Set up FMASK register (RFLAGS mask)
        // Clear IF (interrupts), DF, TF, AC during syscall
        const FMASK: u64 = (1 << 9) | (1 << 10) | (1 << 8) | (1 << 18);  // IF | DF | TF | AC
        wrmsr(IA32_FMASK, FMASK);
    }
    
    serial_write_str(&alloc::format!(
        "SYSCALL: Entry point at 0x{:x}\n",
        syscall_entry as u64
    ));
    serial_write_str("SYSCALL: Syscall mechanism initialized\n");
    
    Ok(())
}

/// Rust syscall handler called from assembly
#[no_mangle]
pub extern "C" fn rust_syscall_handler(syscall_num: u64, regs: *const u64) -> u64 {
    // Extract arguments from saved registers
    unsafe {
        let rdi = *regs.add(1);  // arg1
        let rsi = *regs.add(2);  // arg2
        let rdx = *regs.add(3);  // arg3
        let r10 = *regs.add(6);  // arg4 (note: x86_64 uses r10 instead of rcx)
        let r8 = *regs.add(4);   // arg5
        let r9 = *regs.add(5);   // arg6
        
        let args = SyscallArgs::from_registers(rdi, rsi, rdx, r10, r8, r9);
        
        serial_write_str(&alloc::format!(
            "SYSCALL: Handler called - num={} rdi=0x{:x} rsi=0x{:x} rdx=0x{:x}\n",
            syscall_num, rdi, rsi, rdx
        ));
        
        // Dispatch to appropriate syscall handler
        let result = handle_syscall(syscall_num, &args);
        
        match result {
            SyscallResult::Success(val) => {
                serial_write_str(&alloc::format!("SYSCALL: Returning success: {}\n", val));
                val
            }
            SyscallResult::Error(err) => {
                let errno = err.to_errno();
                serial_write_str(&alloc::format!("SYSCALL: Returning error: {:?} ({})\n", err, errno));
                // Return negative errno for error (Linux convention)
                (-(errno as i64)) as u64
            }
        }
    }
}

/// Handle individual syscalls
fn handle_syscall(num: u64, args: &SyscallArgs) -> SyscallResult {
    match num {
        1 => sys_write(args.arg0 as i32, args.arg1 as *const u8, args.arg2 as usize),
        57 => sys_fork(),
        59 => sys_execve(args.arg0 as *const u8, args.arg1 as *const *const u8, args.arg2 as *const *const u8),
        60 => sys_exit(args.arg0 as i32),
        61 => sys_wait4(args.arg0 as i32, args.arg1 as *mut i32, args.arg2 as i32, args.arg3 as *mut u8),
        _ => {
            serial_write_str(&alloc::format!("SYSCALL: Unimplemented syscall {}\n", num));
            SyscallResult::Error(crate::syscalls::SyscallError::NotImplemented)
        }
    }
}

/// sys_write - Write to file descriptor
fn sys_write(fd: i32, buf: *const u8, count: usize) -> SyscallResult {
    serial_write_str(&alloc::format!("SYSCALL: write(fd={}, count={})\n", fd, count));
    
    if buf.is_null() || count == 0 {
        return SyscallResult::Error(crate::syscalls::SyscallError::InvalidArgument);
    }
    
    // Safety: We're in kernel mode, but userland pointer could be invalid
    // TODO: Verify the pointer is in userland memory range
    let data = unsafe {
        core::slice::from_raw_parts(buf, count)
    };
    
    // For now, always write to serial output
    match fd {
        1 | 2 => {  // stdout or stderr
            // Try to convert to UTF-8 and write
            if let Ok(s) = core::str::from_utf8(data) {
                serial_write_str(&alloc::format!("USERLAND: {}", s));
            } else {
                // Write raw bytes in hex if not UTF-8
                serial_write_str("USERLAND: [binary data]\n");
            }
            SyscallResult::Success(count as u64)
        }
        _ => {
            serial_write_str(&alloc::format!("SYSCALL: Invalid fd {}\n", fd));
            SyscallResult::Error(crate::syscalls::SyscallError::InvalidArgument)
        }
    }
}

/// sys_exit - Exit process
fn sys_exit(code: i32) -> SyscallResult {
    serial_write_str(&alloc::format!("SYSCALL: exit(code={})\n", code));
    
    // Initialize process manager if needed
    init_process_manager_if_needed();
    
    let current_pid = get_current_pid();
    
    // Mark process as zombie and store exit code
    let mut manager_guard = SYSCALL_PROCESS_MANAGER.lock();
    if let Some(ref mut manager) = *manager_guard {
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            process.set_state(crate::process::process::ProcessState::Zombie);
            process.exit_code = Some(code as u32);
            
            serial_write_str(&alloc::format!(
                "SYSCALL: Process {} marked as zombie with code {}\n",
                current_pid, code
            ));
            
            // Get parent PID for SIGCHLD
            if let Some(parent_pid) = process.parent_pid {
                serial_write_str(&alloc::format!(
                    "SYSCALL: Sending SIGCHLD to parent PID {}\n",
                    parent_pid
                ));
                
                // Set SIGCHLD pending for parent
                if let Some(ref mut parent) = manager.processes[parent_pid as usize] {
                    parent.pending_signals |= 1 << 17; // SIGCHLD = 17
                    serial_write_str("SYSCALL: SIGCHLD set for parent\n");
                }
            }
        } else {
            serial_write_str(&alloc::format!(
                "SYSCALL: Process {} not found in table\n",
                current_pid
            ));
        }
    }
    
    // In real implementation, this would switch to another process
    // For now, just halt
    serial_write_str("SYSCALL: Process exited, halting system\n");
    
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}

/// sys_fork - Create child process
fn sys_fork() -> SyscallResult {
    serial_write_str("SYSCALL: fork() - creating child process\n");
    
    // Initialize process manager if needed
    init_process_manager_if_needed();
    
    // Get current process ID
    let parent_pid = get_current_pid();
    
    // Create child process using process manager
    let mut manager_guard = SYSCALL_PROCESS_MANAGER.lock();
    if let Some(ref mut manager) = *manager_guard {
        match manager.create_process("child", crate::process::process::ProcessPriority::Normal) {
            Ok(child_pid) => {
                serial_write_str(&alloc::format!(
                    "SYSCALL: fork() - parent PID {}, created child PID {}\n",
                    parent_pid, child_pid
                ));
                
                // Set parent-child relationship
                if let Some(ref mut child) = manager.processes[child_pid as usize] {
                    child.parent_pid = Some(parent_pid);
                }
                
                // In real fork:
                // 1. Copy parent's memory space (COW)
                // 2. Duplicate page tables
                // 3. Copy file descriptors
                // 4. Return 0 to child, child PID to parent
                
                // For now, we're always the parent
                SyscallResult::Success(child_pid as u64)
            }
            Err(e) => {
                serial_write_str(&alloc::format!("SYSCALL: fork() failed: {}\n", e));
                SyscallResult::Error(crate::syscalls::SyscallError::OutOfMemory)
            }
        }
    } else {
        serial_write_str("SYSCALL: fork() - process manager not initialized\n");
        SyscallResult::Error(crate::syscalls::SyscallError::InvalidOperation)
    }
}

/// sys_execve - Execute program
fn sys_execve(pathname: *const u8, argv: *const *const u8, envp: *const *const u8) -> SyscallResult {
    serial_write_str("SYSCALL: execve() - executing program\n");
    
    if pathname.is_null() {
        return SyscallResult::Error(crate::syscalls::SyscallError::InvalidArgument);
    }
    
    // Read pathname from userland
    // TODO: Validate pointer is in userland memory
    let path_str = unsafe {
        // Read until null terminator (max 256 bytes)
        let mut len = 0;
        while len < 256 && *pathname.add(len) != 0 {
            len += 1;
        }
        
        let path_slice = core::slice::from_raw_parts(pathname, len);
        core::str::from_utf8(path_slice).unwrap_or("<invalid>")
    };
    
    serial_write_str(&alloc::format!("SYSCALL: execve('{}', argv, envp)\n", path_str));
    
    // For minimal implementation, just log and return error
    // In real implementation, this would:
    // 1. Load ELF binary from VFS
    // 2. Replace current process memory
    // 3. Set up new stack with arguments
    // 4. Jump to new entry point
    
    serial_write_str("SYSCALL: execve() not fully implemented\n");
    SyscallResult::Error(crate::syscalls::SyscallError::NotImplemented)
}

/// sys_wait4 - Wait for process to change state
fn sys_wait4(pid: i32, wstatus: *mut i32, options: i32, rusage: *mut u8) -> SyscallResult {
    serial_write_str(&alloc::format!("SYSCALL: wait4(pid={}, options=0x{:x})\n", pid, options));
    
    // Initialize process manager if needed
    init_process_manager_if_needed();
    
    let current_pid = get_current_pid();
    
    // Check for zombie children
    let mut manager_guard = SYSCALL_PROCESS_MANAGER.lock();
    if let Some(ref mut manager) = *manager_guard {
        // Find zombie child
        for i in 0..crate::process::MAX_PROCESSES {
            if let Some(ref mut child) = manager.processes[i] {
                // Check if this is a zombie child of current process
                let is_zombie = child.get_state() == crate::process::process::ProcessState::Zombie;
                let is_child = child.parent_pid == Some(current_pid);
                let matches_pid = pid == -1 || pid == child.pid as i32;
                
                if is_zombie && is_child && matches_pid {
                    let child_pid = child.pid;
                    let exit_code = child.exit_code.unwrap_or(0) as i32;
                    
                    serial_write_str(&alloc::format!(
                        "SYSCALL: wait4() - found zombie child PID {}, exit code {}\n",
                        child_pid, exit_code
                    ));
                    
                    // Write exit status to userland if pointer provided
                    if !wstatus.is_null() {
                        unsafe {
                            // Status encoding: exit code in high byte
                            *wstatus = (exit_code & 0xFF) << 8;
                        }
                    }
                    
                    // Reap the zombie - remove from process table
                    manager.processes[i] = None;
                    manager.active_processes = manager.active_processes.saturating_sub(1);
                    
                    serial_write_str(&alloc::format!(
                        "SYSCALL: Reaped zombie child {}\n",
                        child_pid
                    ));
                    
                    // Return child PID
                    return SyscallResult::Success(child_pid as u64);
                }
            }
        }
        
        // No zombie children found
        serial_write_str("SYSCALL: wait4() - no zombie children\n");
        
        // Check if we have any children at all
        let mut has_children = false;
        for i in 0..crate::process::MAX_PROCESSES {
            if let Some(ref child) = manager.processes[i] {
                if child.parent_pid == Some(current_pid) {
                    has_children = true;
                    break;
                }
            }
        }
        
        if has_children {
            // Have children but none are zombies yet
            serial_write_str("SYSCALL: wait4() - children exist but not zombies yet\n");
            // In real implementation, would block here
            // For now, return error
            SyscallResult::Error(crate::syscalls::SyscallError::InvalidOperation)
        } else {
            // No children at all
            serial_write_str("SYSCALL: wait4() - no children (ECHILD)\n");
            SyscallResult::Error(crate::syscalls::SyscallError::InvalidOperation)
        }
    } else {
        serial_write_str("SYSCALL: wait4() - process manager not initialized\n");
        SyscallResult::Error(crate::syscalls::SyscallError::InvalidOperation)
    }
}
