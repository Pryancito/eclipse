//! Syscall entry point and MSR configuration
//! 
//! This module sets up the SYSCALL/SYSRET mechanism for fast system calls

use crate::debug::serial_write_str;
use crate::syscalls::{SyscallArgs, SyscallResult};
use core::arch::asm;

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
        60 => sys_exit(args.arg0 as i32),
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
    
    // TODO: Actually terminate the process
    // For now, just halt the system
    serial_write_str("SYSCALL: Process exited, halting system\n");
    
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}
