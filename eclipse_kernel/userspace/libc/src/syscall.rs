//! Syscall wrappers para Eclipse OS
use core::arch::asm;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_READ: u64 = 2;
pub const SYS_SEND: u64 = 3;
pub const SYS_RECEIVE: u64 = 4;
pub const SYS_YIELD: u64 = 5;
pub const SYS_GETPID: u64 = 6;
pub const SYS_FORK: u64 = 7;
pub const SYS_EXEC: u64 = 8;
pub const SYS_WAIT: u64 = 9;
pub const SYS_GET_SERVICE_BINARY: u64 = 10;
pub const SYS_OPEN: u64 = 11;
pub const SYS_CLOSE: u64 = 12;
pub const SYS_GETPPID: u64 = 13;
pub const SYS_LSEEK: u64 = 14;
pub const SYS_GET_FRAMEBUFFER_INFO: u64 = 15;
pub const SYS_MAP_FRAMEBUFFER: u64 = 16;
pub const SYS_PCI_ENUM_DEVICES: u64 = 17;
pub const SYS_PCI_READ_CONFIG: u64 = 18;
pub const SYS_PCI_WRITE_CONFIG: u64 = 19;
pub const SYS_MOUNT: u64 = 29;
pub const SYS_FSTAT: u64 = 30;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
}

pub fn fstat(fd: i32, stat: &mut Stat) -> i32 {
    unsafe { syscall2(SYS_FSTAT, fd as u64, stat as *mut Stat as u64) as i32 }
}

pub fn brk(addr: u64) -> u64 {
    unsafe { syscall1(26, addr) }
}

// File open flags
pub const O_RDONLY: i32 = 0x0000;
pub const O_WRONLY: i32 = 0x0001;
pub const O_RDWR: i32 = 0x0002;
pub const O_CREAT: i32 = 0x0040;
pub const O_TRUNC: i32 = 0x0200;
pub const O_APPEND: i32 = 0x0400;

// Seek flags
pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

#[inline(always)]
unsafe fn syscall0(n: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, lateout("rax") ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall1(n: u64, arg1: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, in("rdi") arg1, lateout("rax") ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, in("rdi") arg1, in("rsi") arg2, lateout("rax") ret, options(nostack));
    ret
}

#[inline(always)]
pub unsafe fn syscall3(n: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, in("rdi") arg1, in("rsi") arg2, in("rdx") arg3, lateout("rax") ret, options(nostack));
    ret
}

pub fn exit(code: i32) -> ! {
    unsafe { syscall1(SYS_EXIT, code as u64); }
    loop {}
}

pub fn write(fd: u32, buf: &[u8]) -> isize {
    unsafe { syscall3(SYS_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64) as isize }
}

pub fn read(fd: u32, buf: &mut [u8]) -> isize {
    unsafe { syscall3(SYS_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) as isize }
}

pub fn yield_cpu() {
    unsafe { syscall0(SYS_YIELD); }
}

pub fn getpid() -> u32 {
    unsafe { syscall0(SYS_GETPID) as u32 }
}

pub fn getppid() -> u32 {
    unsafe { syscall0(SYS_GETPPID) as u32 }
}

pub fn fork() -> i32 {
    let pid = unsafe { syscall0(SYS_FORK) as i32 };
    // DEBUG: Print what fork() returned
    unsafe {
        let msg = if pid == 0 {
            "[LIBC] fork() returned 0 (child)\n"
        } else if pid > 0 {
            "[LIBC] fork() returned positive (parent)\n"
        } else {
            "[LIBC] fork() returned negative (error)\n"
        };
        syscall3(SYS_WRITE, 1, msg.as_ptr() as u64, msg.len() as u64);
        
        // Also print the actual PID value
        let prefix = "[LIBC] fork() return value: ";
        syscall3(SYS_WRITE, 1, prefix.as_ptr() as u64, prefix.len() as u64);
        
        // Convert PID to string and print (simple approach)
        let mut buf = [0u8; 20];
        let mut n = if pid < 0 { -pid } else { pid };
        let mut i = 0;
        if n == 0 {
            buf[0] = b'0';
            i = 1;
        } else {
            while n > 0 {
                buf[i] = b'0' + (n % 10) as u8;
                n /= 10;
                i += 1;
            }
        }
        // Reverse
        for j in 0..i/2 {
            buf.swap(j, i-1-j);
        }
        if pid < 0 {
            syscall3(SYS_WRITE, 1, b"-".as_ptr() as u64, 1);
        }
        syscall3(SYS_WRITE, 1, buf.as_ptr() as u64, i as u64);
        syscall3(SYS_WRITE, 1, b"\n".as_ptr() as u64, 1);
    }
    pid
}

pub fn exec(elf_buffer: &[u8]) -> i32 {
    unsafe { syscall2(SYS_EXEC, elf_buffer.as_ptr() as u64, elf_buffer.len() as u64) as i32 }
}

pub fn wait(status: Option<&mut i32>) -> i32 {
    let status_ptr = match status {
        Some(s) => s as *mut i32 as u64,
        None => 0,
    };
    unsafe { syscall1(SYS_WAIT, status_ptr) as i32 }
}

/// Get service binary by ID
/// Returns (pointer, size) or (0, 0) on error
pub fn get_service_binary(service_id: u32) -> (*const u8, usize) {
    let mut ptr: u64 = 0;
    let mut size: u64 = 0;
    
    let result = unsafe {
        syscall3(
            SYS_GET_SERVICE_BINARY,
            service_id as u64,
            &mut ptr as *mut u64 as u64,
            &mut size as *mut u64 as u64
        )
    };
    
    if result == 0 {
        (ptr as *const u8, size as usize)
    } else {
        (core::ptr::null(), 0)
    }
}

/// Open a file
/// Returns file descriptor on success, -1 on error
pub fn open(path: &str, flags: i32, _mode: i32) -> i32 {
    unsafe {
        syscall3(
            SYS_OPEN,
            path.as_ptr() as u64,
            path.len() as u64,
            flags as u64
        ) as i32
    }
}

/// Close a file descriptor
/// Returns 0 on success, -1 on error
pub fn close(fd: i32) -> i32 {
    unsafe {
        syscall1(SYS_CLOSE, fd as u64) as i32
    }
}

/// Reposition read/write file offset
/// Returns new offset on success, -1 on error
pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    unsafe {
        syscall3(SYS_LSEEK, fd as u64, offset as u64, whence as u64) as i64
    }
}
/// Send a message to a server
/// Returns 0 on success, -1 on error
pub fn send(server_id: u32, msg_type: u32, data: &[u8]) -> i32 {
    unsafe {
        syscall3(
            SYS_SEND,
            server_id as u64,
            msg_type as u64,
            data.as_ptr() as u64
        ) as i32
    }
}

/// Receive a message
/// Returns (length, sender_pid) or (0, 0) if no message
pub fn receive(buffer: &mut [u8]) -> (usize, u32) {
    let mut sender_pid: u64 = 0;
    
    let result = unsafe {
        syscall3(
            SYS_RECEIVE,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
            &mut sender_pid as *mut u64 as u64
        )
    };
    
    if result > 0 {
        (result as usize, sender_pid as u32)
    } else {
        (0, 0)
    }
}

/// PCI device information structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PciDeviceInfo {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub bar0: u32,
}

/// Enumerate PCI devices by class
/// class_code: 0x04 for audio devices, 0xFF for all
/// Returns number of devices found
pub fn pci_enum_devices(class_code: u8, devices: &mut [PciDeviceInfo]) -> usize {
    let max_devices = devices.len();
    if max_devices == 0 { return 0; }
    
    // El kernel escribe 8 u64 por dispositivo.
    // Usamos un buffer temporal para evitar solapamientos y corrupción de memoria.
    let count_cap = core::cmp::min(max_devices, 16);
    let mut tmp_buffer = [0u64; 16 * 8]; 
    
    let count = unsafe {
        syscall3(
            SYS_PCI_ENUM_DEVICES,
            class_code as u64,
            tmp_buffer.as_mut_ptr() as u64,
            count_cap as u64
        )
    };
    
    if count == u64::MAX || count == 0 {
        return 0;
    }
    
    let actual_count = core::cmp::min(count as usize, count_cap);
    
    // Parsear el buffer temporal a PciDeviceInfo
    for i in 0..actual_count {
        let offset = i * 8;
        devices[i] = PciDeviceInfo {
            bus: tmp_buffer[offset + 0] as u8,
            device: tmp_buffer[offset + 1] as u8,
            function: tmp_buffer[offset + 2] as u8,
            vendor_id: tmp_buffer[offset + 3] as u16,
            device_id: tmp_buffer[offset + 4] as u16,
            class_code: tmp_buffer[offset + 5] as u8,
            subclass: tmp_buffer[offset + 6] as u8,
            bar0: tmp_buffer[offset + 7] as u32,
        };
    }
    
    actual_count
}


/// Read PCI configuration space (32-bit)
pub fn pci_read_config_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let device_location = ((bus as u64) << 16) | ((device as u64) << 8) | (function as u64);
    
    let result = unsafe {
        syscall3(
            SYS_PCI_READ_CONFIG,
            device_location,
            offset as u64,
            4  // 4 bytes
        )
    };
    
    if result == u64::MAX {
        0
    } else {
        result as u32
    }
}

/// Read PCI configuration space (16-bit)
pub fn pci_read_config_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let device_location = ((bus as u64) << 16) | ((device as u64) << 8) | (function as u64);
    
    let result = unsafe {
        syscall3(
            SYS_PCI_READ_CONFIG,
            device_location,
            offset as u64,
            2  // 2 bytes
        )
    };
    
    if result == u64::MAX {
        0
    } else {
        result as u16
    }
}

/// Read PCI configuration space (8-bit)
pub fn pci_read_config_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let device_location = ((bus as u64) << 16) | ((device as u64) << 8) | (function as u64);
    
    let result = unsafe {
        syscall3(
            SYS_PCI_READ_CONFIG,
            device_location,
            offset as u64,
            1  // 1 byte
        )
    };
    
    if result == u64::MAX {
        0
    } else {
        result as u8
    }
}

/// Framebuffer information structure (matches kernel's FramebufferInfo in servers.rs)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub address: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u16,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

/// Get framebuffer information from the kernel
/// Returns Some(FramebufferInfo) on success, None on failure
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    let mut fb_info = FramebufferInfo {
        address: 0,
        width: 0,
        height: 0,
        pitch: 0,
        bpp: 0,
        red_mask_size: 0,
        red_mask_shift: 0,
        green_mask_size: 0,
        green_mask_shift: 0,
        blue_mask_size: 0,
        blue_mask_shift: 0,
    };
    
    let result = unsafe {
        syscall1(SYS_GET_FRAMEBUFFER_INFO, &mut fb_info as *mut _ as u64)
    };
    
    if result == 0 {
        Some(fb_info)
    } else {
        None
    }
}

/// Map framebuffer into process address space
/// Returns the virtual address of the mapped framebuffer on success, None on failure
pub fn map_framebuffer() -> Option<usize> {
    let result = unsafe { syscall0(SYS_MAP_FRAMEBUFFER) };
    
    if result != 0 {
        Some(result as usize)
    } else {
        None
    }
}

/// Mount the root filesystem
pub fn mount() -> i32 {
    unsafe { syscall0(SYS_MOUNT) as i32 }
}

pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;

/// Map memory
/// Returns: Address of mapped memory, or u64::MAX on error
pub fn mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: i32, offset: u64) -> u64 {
    // Note: offset is currently ignored by kernel sys_mmap signature but we should pass it if we update kernel
    // For now, kernel only takes 5 args, and our syscall5 helper doesn't exist yet either.
    // Wait, sys_mmap defined in kernel takes 5 args: addr, length, prot, flags, fd. Offset is missing!
    // We should probably update kernel to take 6 args, but we only have registers for 6 args (rdi, rsi, rdx, r10, r8, r9)
    // syscall_handler in kernel takes 6 args.
    
    // Let's check syscall_handler signature in kernel:
    // fn syscall_handler(syscall_num, arg1, arg2, arg3, _arg4, _arg5, context)
    // It only captures up to 5 args currently in the signature?
    // "arg1: u64, arg2: u64, arg3: u64, _arg4: u64, _arg5: u64" -> 5 arguments.
    // So for now we can't pass offset easily unless we extend syscall handler.
    // The kernel sys_mmap ignores offset anyway.
    
    // We need a syscall5 function
    unsafe { 
        let ret: u64;
        let n = 20; // SYS_MMAP
        let arg1 = addr;
        let arg2 = length;
        let arg3 = prot;
        let arg4 = flags;
        let arg5 = fd as u64;
        
        // Inline asm for 5 arguments
        // System V AMD64 ABI: rdi, rsi, rdx, rcx, r8, r9
        // Eclipse OS Syscall ABI (from syscall_handler):
        // rdi=arg1, rsi=arg2, rdx=arg3, r10=arg4, r8=arg5
        // (rcx is destroyed by syscall instruction, so Linux uses r10 for 4th arg)
        
        // Let's verify our syscall dispatcher in syscall.rs
        // syscall3 uses rdi, rsi, rdx.
        // We need to implement syscall5.
        
        asm!(
            "int 0x80",
            in("rax") n,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            lateout("rax") ret,
            options(nostack)
        );
        ret
    }
}

/// Unmap memory
pub fn munmap(addr: u64, length: u64) -> i32 {
    let n = 21; // SYS_MUNMAP
    unsafe { syscall2(n, addr, length) as i32 }
}
