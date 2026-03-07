//! sys/eclipse.rs - Eclipse OS specific extensions
pub use eclipse_syscall::{SystemStats, InputEvent};
use crate::types::*;

#[cfg(not(any(test, feature = "host-testing")))]
pub use eclipse_syscall::ProcessInfo;

#[cfg(any(test, feature = "host-testing"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessInfo {
    pub pid: u32,
    pub state: u32,
    pub name: [u8; 16],
    pub cpu_ticks: u64,
    pub mem_frames: u64,
}

#[cfg(any(test, feature = "host-testing"))]
impl ProcessInfo {
    pub const fn new() -> Self {
        Self {
            pid: 0,
            state: 0,
            name: [0; 16],
            cpu_ticks: 0,
            mem_frames: 0,
        }
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn get_system_stats(stats: *mut SystemStats) -> c_int {
    if stats.is_null() { return -1; }
    let res = eclipse_syscall::syscall1(eclipse_syscall::number::SYS_GET_SYSTEM_STATS, stats as usize);
    if res == 0 { 0 } else { -1 }
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn get_system_stats(_stats: *mut SystemStats) -> c_int { -1 }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn get_process_list(buf: *mut ProcessInfo, max_count: usize) -> isize {
    if buf.is_null() { return -1; }
    let res = eclipse_syscall::syscall2(eclipse_syscall::number::SYS_GET_PROCESS_LIST, buf as usize, max_count);
    res as isize
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn get_process_list(_buf: *mut ProcessInfo, _max_count: usize) -> isize { -1 }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn eclipse_kill(pid: u32) -> c_int {
    let res = eclipse_syscall::syscall1(eclipse_syscall::number::SYS_KILL, pid as usize);
    if res == 0 { 0 } else { -1 }
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn eclipse_kill(_pid: u32) -> c_int { -1 }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn set_process_name(name: *const c_char) -> c_int {
    if name.is_null() { return -1; }
    let res = eclipse_syscall::syscall1(eclipse_syscall::number::SYS_SET_PROCESS_NAME, name as usize);
    if res == 0 { 0 } else { -1 }
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn set_process_name(_name: *const c_char) -> c_int { -1 }

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default)]
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

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn get_framebuffer_info() -> core::option::Option<FramebufferInfo> {
    let mut fb_info = FramebufferInfo::default();
    let res = eclipse_syscall::syscall1(eclipse_syscall::number::SYS_GET_FRAMEBUFFER_INFO, &mut fb_info as *mut _ as usize);
    if res == 0 { core::option::Option::Some(fb_info) } else { core::option::Option::None }
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn get_framebuffer_info() -> core::option::Option<FramebufferInfo> { core::option::Option::None }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn map_framebuffer() -> core::option::Option<usize> {
    let res = eclipse_syscall::syscall0(eclipse_syscall::number::SYS_MAP_FRAMEBUFFER);
    if res != 0 { core::option::Option::Some(res as usize) } else { core::option::Option::None }
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn map_framebuffer() -> core::option::Option<usize> { core::option::Option::None }

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct GpuDisplayBufferInfo {
    pub vaddr: u64,
    pub resource_id: u32,
    pub pitch: u32,
    pub size: u64,
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn get_gpu_display_info(out: *mut [u32; 2]) -> bool {
    let res = eclipse_syscall::syscall1(eclipse_syscall::number::SYS_GET_GPU_DISPLAY_INFO, out as usize);
    res != usize::MAX
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn get_gpu_display_info(_out: *mut [u32; 2]) -> bool { false }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn gpu_alloc_display_buffer(width: u32, height: u32) -> core::option::Option<GpuDisplayBufferInfo> {
    let mut out = GpuDisplayBufferInfo::default();
    let res = eclipse_syscall::syscall3(eclipse_syscall::number::SYS_GPU_ALLOC_DISPLAY_BUFFER, width as usize, height as usize, &mut out as *mut _ as usize);
    if res == 0 { core::option::Option::Some(out) } else { core::option::Option::None }
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn gpu_alloc_display_buffer(_width: u32, _height: u32) -> core::option::Option<GpuDisplayBufferInfo> { core::option::Option::None }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn gpu_present(resource_id: u32, x: u32, y: u32, w: u32, h: u32) -> bool {
    let res = eclipse_syscall::syscall5(eclipse_syscall::number::SYS_GPU_PRESENT, resource_id as usize, x as usize, y as usize, w as usize, h as usize);
    res != usize::MAX
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn gpu_present(_resource_id: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> bool { false }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn eclipse_send(target: u32, msg_type: u32, data: *const c_void, len: size_t, _flags: i32) -> isize {
    let res = eclipse_syscall::syscall4(eclipse_syscall::number::SYS_SEND, target as usize, msg_type as usize, data as usize, len);
    res as isize
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn eclipse_send(_target: u32, _msg_type: u32, _data: *const c_void, _len: size_t, _flags: i32) -> isize { -1 }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn receive(buffer: *mut u8, len: size_t, sender_pid: *mut u32) -> usize {
    let mut pid_temp: u64 = 0;
    let res = eclipse_syscall::syscall3(eclipse_syscall::number::SYS_RECEIVE, buffer as usize, len, &mut pid_temp as *mut _ as usize);
    if !sender_pid.is_null() { *sender_pid = pid_temp as u32; }
    res
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn receive(_buffer: *mut u8, _len: size_t, _sender_pid: *mut u32) -> usize { 0 }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn get_logs(buf: *mut u8, len: size_t) -> usize {
    let res = eclipse_syscall::syscall2(eclipse_syscall::number::SYS_GET_LOGS, buf as usize, len);
    res
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn get_logs(_buf: *mut u8, _len: size_t) -> usize { 0 }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub fn receive_fast() -> core::option::Option<([u8; 24], u32, usize)> {
    let size: usize;
    let w0: usize; let w1: usize; let w2: usize; let from: usize;
    unsafe {
        core::arch::asm!("int 0x80", inout("rax") eclipse_syscall::number::SYS_RECEIVE_FAST => size, lateout("rdi") w0, lateout("rsi") w1, lateout("rdx") w2, lateout("rcx") from, out("r8") _, out("r9") _, out("r10") _, out("r11") _, options(nostack));
    }
    if size > 0 {
        let mut data = [0u8; 24];
        data[0..8].copy_from_slice(&w0.to_le_bytes());
        data[8..16].copy_from_slice(&w1.to_le_bytes());
        data[16..24].copy_from_slice(&w2.to_le_bytes());
        core::option::Option::Some((data, from as u32, size))
    } else { core::option::Option::None }
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub fn receive_fast() -> core::option::Option<([u8; 24], u32, usize)> { core::option::Option::None }

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn yield_cpu() {
    let _ = eclipse_syscall::syscall0(eclipse_syscall::number::SYS_YIELD);
}

#[cfg(any(test, feature = "host-testing"))]
#[no_mangle]
pub unsafe extern "C" fn yield_cpu() {}

#[inline]
pub fn receive_ipc(buffer: &mut [u8]) -> (usize, u32) {
    let mut sender_pid: u32 = 0;
    let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender_pid) };
    (len, sender_pid)
}

#[inline]
pub fn send_ipc(server_id: u32, msg_type: u32, data: &[u8]) -> isize {
    unsafe { eclipse_send(server_id, msg_type, data.as_ptr() as *const _, data.len(), 0) }
}

#[inline]
pub fn get_last_exec_error(buf: &mut [u8]) -> usize {
    if buf.is_empty() { return 0; }
    let n = unsafe {
        eclipse_syscall::syscall2(
            eclipse_syscall::number::SYS_GET_LAST_EXEC_ERROR,
            buf.as_mut_ptr() as usize,
            buf.len()
        )
    };
    if n == core::usize::MAX { 0 } else { n }
}

#[inline]
pub fn eclipse_spawn(buf: &[u8], name: Option<&str>) -> i32 {
    let name_ptr = name.map(|s| s.as_ptr() as usize).unwrap_or(0);
    let res = unsafe {
        eclipse_syscall::syscall3(
            eclipse_syscall::number::SYS_SPAWN,
            buf.as_ptr() as usize,
            buf.len(),
            name_ptr
        )
    };
    res as i32
}

#[inline]
pub fn spawn_service(service_id: u32, name_ptr: *const u8, name_len: usize) -> u64 {
    let res = unsafe {
        eclipse_syscall::syscall3(
            55, // SYS_SPAWN_SERVICE
            service_id as usize,
            name_ptr as usize,
            name_len
        )
    };
    res as u64
}

use core::sync::atomic::{AtomicBool, Ordering};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: core::marker::Send> Sync for Spinlock<T> {}
unsafe impl<T: core::marker::Send> core::marker::Send for Spinlock<T> {}

impl<T> Spinlock<T> {
    pub const fn new(data: T) -> Self {
        Spinlock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        SpinlockGuard { lock: self }
    }
}

pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
}

impl<T> Deref for SpinlockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

#[inline]
pub fn eclipse_open(path: &str, flags: i32, _mode: i32) -> i32 {
    let res = unsafe {
        eclipse_syscall::syscall3(
            eclipse_syscall::number::SYS_OPEN,
            path.as_ptr() as usize,
            path.len(),
            flags as usize
        )
    };
    if res == core::usize::MAX { -1 } else { res as i32 }
}

#[inline]
pub fn eclipse_write(fd: u32, buf: &[u8]) -> isize {
    unsafe { eclipse_syscall::syscall3(eclipse_syscall::number::SYS_WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) as isize }
}

#[inline]
pub fn eclipse_read(fd: u32, buf: &mut [u8]) -> isize {
    unsafe { eclipse_syscall::syscall3(eclipse_syscall::number::SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize }
}

#[inline]
pub fn eclipse_close(fd: i32) -> i32 {
    unsafe { eclipse_syscall::syscall1(eclipse_syscall::number::SYS_CLOSE, fd as usize) as i32 }
}

/// Map a file-descriptor resource into process address space via SYS_FMAP (28).
/// Returns `Some(virtual_address)` on success or `None` on failure.
#[inline]
pub fn eclipse_fmap(fd: i32, offset: usize, len: usize) -> core::option::Option<usize> {
    let res = unsafe {
        eclipse_syscall::syscall3(
            eclipse_syscall::number::SYS_FMAP,
            fd as usize,
            offset,
            len,
        )
    };
    if res == 0 || res == core::usize::MAX {
        core::option::Option::None
    } else {
        core::option::Option::Some(res)
    }
}



pub fn get_service_binary(service_id: u32) -> (*const u8, usize) {
    let mut ptr: u64 = 0;
    let mut size: u64 = 0;
    let res = unsafe {
        eclipse_syscall::syscall3(
            eclipse_syscall::number::SYS_GET_SERVICE_BINARY,
            service_id as usize,
            &mut ptr as *mut u64 as usize,
            &mut size as *mut u64 as usize
        )
    };
    if res == 0 {
        (ptr as *const u8, size as usize)
    } else {
        (core::ptr::null(), 0)
    }
}

pub fn exec(binary: &[u8]) -> i32 {
    unsafe {
        eclipse_syscall::syscall2(
            eclipse_syscall::number::SYS_EXEC,
            binary.as_ptr() as usize,
            binary.len()
        ) as i32
    }
}

pub fn gpu_command(kind: usize, command: usize, payload: &[u8]) -> isize {
    unsafe {
        eclipse_syscall::syscall4(
            eclipse_syscall::number::SYS_GPU_COMMAND,
            kind,
            command,
            payload.as_ptr() as usize,
            payload.len()
        ) as isize
    }
}

/// Mount a filesystem device as root.
/// device_name: "disk:0p2" or "disk:0@offset_blocks"
pub fn mount(device_name: &str) -> i32 {
    unsafe {
        eclipse_syscall::syscall2(
            eclipse_syscall::number::SYS_MOUNT,
            device_name.as_ptr() as usize,
            device_name.len()
        ) as i32
    }
}

/// Return number of registered block-storage devices.
pub fn get_storage_device_count() -> usize {
    unsafe { eclipse_syscall::syscall0(eclipse_syscall::number::SYS_GET_STORAGE_DEVICE_COUNT) }
}

pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

/// Safe lseek wrapper.
pub fn eclipse_lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    unsafe {
        eclipse_syscall::syscall3(
            eclipse_syscall::number::SYS_LSEEK,
            fd as usize,
            offset as usize,
            whence as usize
        ) as i64
    }
}

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

pub fn pci_enum_devices(class_code: u8, devices: &mut [PciDeviceInfo]) -> usize {
    let max = devices.len().min(16);
    if max == 0 { return 0; }
    let mut tmp_buffer = [0usize; 16 * 8];
    let count = unsafe {
        eclipse_syscall::syscall3(
            eclipse_syscall::number::SYS_PCI_ENUM_DEVICES,
            class_code as usize,
            tmp_buffer.as_mut_ptr() as usize,
            max
        )
    };
    if count == usize::MAX || count == 0 { return 0; }
    let actual = count.min(max);
    for i in 0..actual {
        let o = i * 8;
        devices[i] = PciDeviceInfo {
            bus:        tmp_buffer[o + 0] as u8,
            device:     tmp_buffer[o + 1] as u8,
            function:   tmp_buffer[o + 2] as u8,
            vendor_id:  tmp_buffer[o + 3] as u16,
            device_id:  tmp_buffer[o + 4] as u16,
            class_code: tmp_buffer[o + 5] as u8,
            subclass:   tmp_buffer[o + 6] as u8,
            bar0:       tmp_buffer[o + 7] as u32,
        };
    }
    actual
}

pub fn pci_read_config_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let loc = ((bus as usize) << 16) | ((device as usize) << 8) | (function as usize);
    let res = unsafe { eclipse_syscall::syscall3(eclipse_syscall::number::SYS_PCI_READ_CONFIG, loc, offset as usize, 4) };
    if res == usize::MAX { 0 } else { res as u32 }
}

/// Read a keyboard scancode. Returns Some(scancode) or None if no key available.
pub fn read_key_scancode() -> Option<u32> {
    let sc = unsafe { eclipse_syscall::syscall0(eclipse_syscall::number::SYS_READ_KEY) };
    if sc == 0 || sc == usize::MAX { None } else { Some(sc as u32) }
}

/// Mouse packet returned by read_mouse_packet() as a packed u64:
///   bits  0..7  = buttons
///   bits  8..15 = dx (i8)
///   bits 16..23 = dy (i8)
///   bits 24..31 = scroll (i8)
pub fn read_mouse_packet() -> Option<u64> {
    let res = unsafe { eclipse_syscall::syscall0(eclipse_syscall::number::SYS_READ_MOUSE_PACKET) };
    if res == 0 || res == usize::MAX { None } else { Some(res as u64) }
}

/// Set hardware cursor position.
pub fn set_cursor_position(x: u32, y: u32) {
    unsafe {
        eclipse_syscall::syscall2(
            eclipse_syscall::number::SYS_SET_CURSOR_POSITION,
            x as usize,
            y as usize
        );
    }
}

#[no_mangle]
pub unsafe extern "C" fn sleep_ms(ms: u32) {
    let ts = timespec {
        tv_sec: (ms / 1000) as time_t,
        tv_nsec: ((ms % 1000) * 1_000_000) as c_long,
    };
    crate::header::time::nanosleep(&ts, core::ptr::null_mut());
}
