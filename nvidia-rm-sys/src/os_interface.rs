//! Implementation of NVIDIA's `os-interface.h` ABI for Eclipse. Signatures
//! transcribed verbatim from src/nvidia/arch/nvalloc/unix/include/os-interface.h
//! (MIT, NVIDIA/open-gpu-kernel-modules) -- this is the contract real vendored
//! RM source will link against, once vendored.
//!
//! Functions are grouped exactly like the real header. Each is one of:
//!  - REAL: fully implemented against Eclipse/this crate's own primitives.
//!  - HOOK: implemented via `crate::hooks` (needs `drivers` to call
//!    `register_hooks`); returns a safe default until then.
//!  - STUB: deliberately not supported (vGPU/NUMA/cgroups/Tegra/etc. do not
//!    apply to a single desktop GPU) -- returns the appropriate "no" value.
//!  - TODO: needs work this pass didn't do (mainly the 3 variadic
//!    functions, which stable Rust cannot export as `extern "C" fn(...)`).
#![allow(non_snake_case)]

extern crate alloc;

use crate::hooks::with_hooks;
use crate::types::*;
use alloc::alloc::{alloc, dealloc, Layout};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

// ---------------------------------------------------------------------
// Globals RM reads directly (not functions) -- from the bottom of
// os-interface.h. 4 KiB pages, no huge pages, no confidential computing,
// no dma-buf/imex support yet.
// ---------------------------------------------------------------------
#[no_mangle]
pub static os_page_size: NvU64 = 4096;
#[no_mangle]
pub static os_max_page_size: NvU64 = 4096;
#[no_mangle]
pub static os_page_mask: NvU64 = 0xFFF;
#[no_mangle]
pub static os_page_shift: NvU8 = 12;
#[no_mangle]
pub static os_cc_enabled: NvBool = NV_FALSE;
#[no_mangle]
pub static os_cc_sev_snp_enabled: NvBool = NV_FALSE;
#[no_mangle]
pub static os_cc_sme_enabled: NvBool = NV_FALSE;
#[no_mangle]
pub static os_cc_snp_vtom_enabled: NvBool = NV_FALSE;
#[no_mangle]
pub static os_cc_tdx_enabled: NvBool = NV_FALSE;
#[no_mangle]
pub static os_dma_buf_enabled: NvBool = NV_FALSE;
#[no_mangle]
pub static os_imex_channel_is_supported: NvBool = NV_FALSE;

// ---------------------------------------------------------------------
// Memory (REAL). os_free_mem gets no size, so os_alloc_mem stores one in
// a small header just before the pointer it hands back -- the standard
// trick for bridging a sized allocator (Rust's GlobalAlloc) to a
// free-without-size C API.
// ---------------------------------------------------------------------
const ALLOC_ALIGN: usize = 16;
const HEADER_PAD: usize = ALLOC_ALIGN; // one aligned slot is plenty for a usize

#[no_mangle]
pub extern "C" fn os_alloc_mem(p_address: *mut *mut c_void, size: NvU64) -> NV_STATUS {
    if p_address.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let total = match HEADER_PAD.checked_add(size as usize) {
        Some(t) => t,
        None => return NV_ERR_INVALID_ARGUMENT,
    };
    let layout = match Layout::from_size_align(total, ALLOC_ALIGN) {
        Ok(l) => l,
        Err(_) => return NV_ERR_INVALID_ARGUMENT,
    };
    let raw = unsafe { alloc(layout) };
    if raw.is_null() {
        return NV_ERR_NO_MEMORY;
    }
    unsafe {
        (raw as *mut usize).write(total);
        *p_address = raw.add(HEADER_PAD) as *mut c_void;
    }
    NV_OK
}

#[no_mangle]
pub extern "C" fn os_free_mem(p_address: *mut c_void) {
    if p_address.is_null() {
        return;
    }
    unsafe {
        let raw = (p_address as *mut u8).sub(HEADER_PAD);
        let total = (raw as *const usize).read();
        let layout = Layout::from_size_align_unchecked(total, ALLOC_ALIGN);
        dealloc(raw, layout);
    }
}

// ---------------------------------------------------------------------
// Time (HOOK) -- Eclipse's real timer lives in kernel-hal, out of reach
// from this crate; `drivers` supplies it via register_hooks.
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_get_monotonic_time_ns() -> NvU64 {
    with_hooks(0, |h| h.monotonic_time_ns())
}
#[no_mangle]
pub extern "C" fn os_get_monotonic_time_ns_hr() -> NvU64 {
    with_hooks(0, |h| h.monotonic_time_ns())
}
#[no_mangle]
pub extern "C" fn os_get_monotonic_tick_resolution_ns() -> NvU64 {
    1
}
#[no_mangle]
pub extern "C" fn os_delay(milliseconds: NvU32) -> NV_STATUS {
    with_hooks((), |h| h.delay_us(milliseconds.saturating_mul(1000)));
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_delay_us(microseconds: NvU32) -> NV_STATUS {
    with_hooks((), |h| h.delay_us(microseconds));
    NV_OK
}
/// Real: TSC frequency in Hz, calibrated once against the hook-provided
/// microsecond delay and cached. Feeds `osGetCpuFrequency` (os_init.c,
/// Hz -> MHz) and from there `pSys->cpuInfo.clock` (cpu.c) -- a 0 here is
/// not a divisor anywhere at construction time (checked), but a 0 MHz
/// CPU clock would flow into later consumers (e.g. GSP boot arguments),
/// so report the real value.
#[no_mangle]
pub extern "C" fn os_get_cpu_frequency() -> NvU64 {
    static CACHED_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let cached = CACHED_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    log::warn!("[nvidia-rm] os_get_cpu_frequency: calibrating TSC (10ms)...");
    // 10 ms calibration window: long enough to make delay_us's own
    // resolution error negligible, short enough to be a one-off blip.
    let t0 = unsafe { core::arch::x86_64::_rdtsc() };
    with_hooks((), |h| h.delay_us(10_000));
    let t1 = unsafe { core::arch::x86_64::_rdtsc() };
    let hz = t1.wrapping_sub(t0).saturating_mul(100);
    log::warn!(
        "[nvidia-rm] os_get_cpu_frequency: calibrated {} MHz",
        hz / 1_000_000
    );
    if hz != 0 {
        CACHED_HZ.store(hz, Ordering::Relaxed);
    }
    hz
}

// ---------------------------------------------------------------------
// Process/thread queries (STUB) -- this driver runs entirely in kernel
// context; there is no per-call "current userspace process" to report.
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_get_current_process() -> NvU32 {
    0
}
#[no_mangle]
pub extern "C" fn os_get_current_process_name(buffer: *mut c_char, length: NvU32) {
    if buffer.is_null() || length == 0 {
        return;
    }
    let name = b"eclipse-kernel\0";
    let n = core::cmp::min(name.len(), length as usize);
    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), buffer as *mut u8, n) };
}
#[no_mangle]
pub extern "C" fn os_get_current_thread(thread_id: *mut NvU64) -> NV_STATUS {
    // TEMPORARY one-shot bring-up marker: portThreadGetCurrentThreadId
    // funnels here; in the sysConstruct stretch under investigation its
    // callers are threadStateGlobalAlloc / rmapiInitialize's lock setup /
    // rmapiLockAcquire. Remove with the other trace checkpoints.
    {
        use core::sync::atomic::AtomicBool;
        static SEEN: AtomicBool = AtomicBool::new(false);
        if !SEEN.swap(true, Ordering::Relaxed) {
            log::warn!("[nvidia-rm] first os_get_current_thread call (threadState/rmapi lock path reached)");
        }
    }
    if thread_id.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *thread_id = 0 };
    NV_OK
}

// ---------------------------------------------------------------------
// String / memory utilities (REAL) -- freestanding, no libc.
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_string_copy(dst: *mut c_char, src: *const c_char) -> *mut c_char {
    unsafe {
        let (mut d, mut s) = (dst, src);
        loop {
            *d = *s;
            if *s == 0 {
                break;
            }
            d = d.add(1);
            s = s.add(1);
        }
    }
    dst
}

#[no_mangle]
pub extern "C" fn os_string_length(str_: *const c_char) -> NvU32 {
    let mut len = 0u32;
    unsafe {
        let mut p = str_;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
    }
    len
}

#[no_mangle]
pub extern "C" fn os_strtoul(str_: *const c_char, endp: *mut *mut c_char, base: NvU32) -> NvU32 {
    unsafe {
        let mut p = str_ as *const u8;
        let mut base = base;
        if (base == 0 || base == 16) && *p == b'0' && (*p.add(1) | 0x20) == b'x' {
            base = 16;
            p = p.add(2);
        } else if base == 0 {
            base = 10;
        }
        let mut value: NvU32 = 0;
        loop {
            let c = *p;
            let digit = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => break,
            };
            if digit >= base {
                break;
            }
            value = value.wrapping_mul(base).wrapping_add(digit);
            p = p.add(1);
        }
        if !endp.is_null() {
            *endp = p as *mut c_char;
        }
        value
    }
}

#[no_mangle]
pub extern "C" fn os_string_compare(str1: *const c_char, str2: *const c_char) -> NvS32 {
    unsafe {
        let (mut a, mut b) = (str1 as *const u8, str2 as *const u8);
        loop {
            let (ca, cb) = (*a, *b);
            if ca != cb {
                return ca as NvS32 - cb as NvS32;
            }
            if ca == 0 {
                return 0;
            }
            a = a.add(1);
            b = b.add(1);
        }
    }
}

// TODO(variadic): os_snprintf/os_vsnprintf/os_log_error/nv_printf take
// `...`/`va_list`. Stable Rust cannot define an exported `extern "C"`
// variadic function -- only declare (import) one. These need a tiny
// hand-written C shim (fixed-arity Rust callback behind vsnprintf-style
// C code), not something this crate can do in pure Rust. Left
// unimplemented deliberately rather than faked.

#[no_mangle]
pub extern "C" fn os_mem_copy(dst: *mut c_void, src: *const c_void, length: NvU32) -> *mut c_void {
    unsafe { core::ptr::copy(src as *const u8, dst as *mut u8, length as usize) };
    dst
}
/// STUB: no userspace address space to copy from/to yet.
#[no_mangle]
pub extern "C" fn os_memcpy_from_user(_to: *mut c_void, _from: *const c_void, _n: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_memcpy_to_user(_to: *mut c_void, _from: *const c_void, _n: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_mem_set(dst: *mut c_void, value: NvU8, length: NvU32) -> *mut c_void {
    unsafe { core::ptr::write_bytes(dst as *mut u8, value, length as usize) };
    dst
}
#[no_mangle]
pub extern "C" fn os_mem_cmp(buf0: *const NvU8, buf1: *const NvU8, length: NvU32) -> NvS32 {
    unsafe {
        for i in 0..length as isize {
            let (a, b) = (*buf0.offset(i), *buf1.offset(i));
            if a != b {
                return a as NvS32 - b as NvS32;
            }
        }
    }
    0
}

// ---------------------------------------------------------------------
// PCI config space (HOOK). `handle` is whatever `drivers` chooses to hand
// back from pci_config_read/write's `pci_handle` (we pass it through as a
// bare usize -- `drivers` decides what it points to).
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_pci_init_handle(
    _domain: NvU32,
    bus: NvU8,
    slot: NvU8,
    function: NvU8,
    vendor: *mut NvU16,
    device: *mut NvU16,
) -> *mut c_void {
    // Pack (bus, device, function) into the usize handle every other
    // os_pci_* function already passes through verbatim to
    // `KernelHooks::pci_config_read/write` (see those functions just
    // below). Top bit is a "valid handle" tag so the packed value is
    // never 0/null even for bus=device=function=0 (a real, valid
    // location -- e.g. this GPU's own function 0).
    let handle = 0x8000_0000usize
        | ((bus as usize) << 16)
        | ((slot as usize) << 8)
        | (function as usize);

    // Vendor/device ID live in the first PCI config dword (offset 0),
    // vendor in the low 16 bits, device in the high 16 bits -- standard
    // PCI config space layout, not NVIDIA-specific.
    let id_dword = with_hooks(0xFFFF_FFFF, |h| h.pci_config_read(handle, 0, 4));
    if !vendor.is_null() {
        unsafe { *vendor = (id_dword & 0xFFFF) as NvU16 };
    }
    if !device.is_null() {
        unsafe { *device = ((id_dword >> 16) & 0xFFFF) as NvU16 };
    }

    handle as *mut c_void
}
#[no_mangle]
pub extern "C" fn os_pci_read_byte(handle: *mut c_void, offset: NvU32, value: *mut NvU8) -> NV_STATUS {
    if value.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *value = with_hooks(0xFF, |h| h.pci_config_read(handle as usize, offset, 1)) as NvU8 };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_read_word(handle: *mut c_void, offset: NvU32, value: *mut NvU16) -> NV_STATUS {
    if value.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *value = with_hooks(0xFFFF, |h| h.pci_config_read(handle as usize, offset, 2)) as NvU16 };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_read_dword(handle: *mut c_void, offset: NvU32, value: *mut NvU32) -> NV_STATUS {
    if value.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *value = with_hooks(0xFFFF_FFFF, |h| h.pci_config_read(handle as usize, offset, 4)) };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_write_byte(handle: *mut c_void, offset: NvU32, value: NvU8) -> NV_STATUS {
    with_hooks((), |h| h.pci_config_write(handle as usize, offset, 1, value as u32));
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_write_word(handle: *mut c_void, offset: NvU32, value: NvU16) -> NV_STATUS {
    with_hooks((), |h| h.pci_config_write(handle as usize, offset, 2, value as u32));
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_write_dword(handle: *mut c_void, offset: NvU32, value: NvU32) -> NV_STATUS {
    with_hooks((), |h| h.pci_config_write(handle as usize, offset, 4, value));
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_remove_supported() -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_pci_remove(_handle: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_enable_pci_req_atomics(_handle: *mut c_void, _kind: u32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_pci_trigger_flr(_handle: *mut c_void) {}

// ---------------------------------------------------------------------
// MMIO / I/O ports (HOOK).
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_map_kernel_space(start: NvU64, size: NvU64, _mode: NvU32) -> *mut c_void {
    with_hooks(0, |h| h.map_kernel_space(start, size)) as *mut c_void
}
#[no_mangle]
pub extern "C" fn os_unmap_kernel_space(addr: *mut c_void, size: NvU64) {
    with_hooks((), |h| h.unmap_kernel_space(addr as u64, size));
}
#[no_mangle]
pub extern "C" fn os_flush_cpu_cache_all() -> NV_STATUS {
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_flush_user_cache() -> NV_STATUS {
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_flush_cpu_write_combine_buffer() {
    unsafe { core::arch::asm!("sfence") };
}
#[no_mangle]
pub extern "C" fn os_io_read_byte(port: NvU32) -> NvU8 {
    with_hooks(0xFF, |h| h.io_read(port, 1)) as NvU8
}
#[no_mangle]
pub extern "C" fn os_io_read_word(port: NvU32) -> NvU16 {
    with_hooks(0xFFFF, |h| h.io_read(port, 2)) as NvU16
}
#[no_mangle]
pub extern "C" fn os_io_read_dword(port: NvU32) -> NvU32 {
    with_hooks(0xFFFF_FFFF, |h| h.io_read(port, 4))
}
#[no_mangle]
pub extern "C" fn os_io_write_byte(port: NvU32, value: NvU8) {
    with_hooks((), |h| h.io_write(port, 1, value as u32));
}
#[no_mangle]
pub extern "C" fn os_io_write_word(port: NvU32, value: NvU16) {
    with_hooks((), |h| h.io_write(port, 2, value as u32));
}
#[no_mangle]
pub extern "C" fn os_io_write_dword(port: NvU32, value: NvU32) {
    with_hooks((), |h| h.io_write(port, 4, value));
}

// ---------------------------------------------------------------------
// Permissions (REAL, trivially) -- everything in this driver already runs
// fully privileged in kernel context.
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_is_administrator() -> NvBool {
    NV_TRUE
}
#[no_mangle]
pub extern "C" fn os_check_access(_access_right: u32) -> NvBool {
    NV_TRUE
}
#[no_mangle]
pub extern "C" fn os_get_euid(euid: *mut NvU32) -> NV_STATUS {
    if euid.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *euid = 0 };
    NV_OK
}

// ---------------------------------------------------------------------
// Debug (REAL where trivial).
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_dbg_init() {}
#[no_mangle]
pub extern "C" fn os_dbg_breakpoint() {
    log::error!("[nvidia-rm] os_dbg_breakpoint()");
}
#[no_mangle]
pub extern "C" fn os_dbg_set_level(_level: NvU32) {}
#[no_mangle]
pub extern "C" fn os_dump_stack() {
    log::warn!("[nvidia-rm] os_dump_stack() -- no unwinder wired up, nothing to print");
}
fn log_raw_cstr(str_: *const c_char) {
    // TEMPORARY: stamp every RM diagnostic line with the current stack
    // pointer. Two things fall out of one photo: (a) actual stack
    // consumption across the init sequence (deltas between lines), and
    // (b) WHICH stack this is running on -- the BSP boot stack (2 MiB,
    // ~0xffffff80_xxxxxxxx) vs a heap-allocated 256 KiB AP stack
    // (~0xffff8000_xxxxxxxx, and crucially NO guard page below it, so an
    // overflow is silent heap corruption, not a clean fault). Remove with
    // the other bring-up checkpoints.
    let rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp) };
    unsafe {
        let mut p = str_;
        let mut len = 0usize;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
        let slice = core::slice::from_raw_parts(str_ as *const u8, len);
        if let Ok(s) = core::str::from_utf8(slice) {
            // WARN, not info: the kernel's default max log level is WARN
            // (zCore/src/logging.rs) -- `log::info!` is silently dropped by
            // the `log` crate's own level check before SimpleLogger ever
            // runs, so every real NV_PRINTF (which routes here via
            // nvDbg_Printf -> nv_printf -> nvrm_shim_log_raw) and every
            // ECLIPSE_TRACE checkpoint (eclipse_rm_init.c) was silently
            // going nowhere -- confirmed empirically: a real-hardware test
            // with trace checkpoints already deployed produced zero output
            // at all, not even the very first checkpoint, which is only
            // consistent with the print calls never executing their body.
            log::warn!("[nvidia-rm] {} [rsp={:#x}]", s, rsp);
        }
    }
}
#[no_mangle]
pub extern "C" fn out_string(str_: *const c_char) {
    log_raw_cstr(str_);
}
// TODO(variadic): nv_printf -- see note above os_snprintf.

/// Called from vendor/glue.c's `nvDbg_Printf` -- NVIDIA's real printf
/// backend (NVRM_PRINTF_FUNCTION) is variadic, which stable Rust can't
/// export directly (see the TODO above os_snprintf). glue.c forwards the
/// unexpanded format string here, dropping the variadic args for now.
#[no_mangle]
pub extern "C" fn nvrm_shim_log_raw(str_: *const c_char) {
    log_raw_cstr(str_);
}

// ---------------------------------------------------------------------
// CPU topology (HOOK, conservative single-CPU fallback).
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_get_cpu_count() -> NvU32 {
    1
}
#[no_mangle]
pub extern "C" fn os_get_cpu_number() -> NvU32 {
    0
}
#[no_mangle]
pub extern "C" fn os_disable_console_access() {}
#[no_mangle]
pub extern "C" fn os_enable_console_access() {}
#[no_mangle]
pub extern "C" fn os_registry_init() -> NV_STATUS {
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_get_max_user_va() -> NvU64 {
    0x0000_7FFF_FFFF_FFFF
}
#[no_mangle]
pub extern "C" fn os_schedule() -> NV_STATUS {
    core::hint::spin_loop();
    NV_OK
}

// ---------------------------------------------------------------------
// Spinlock / mutex / semaphore / rwlock (REAL). RM's C API is
// acquire-here / release-there across a bare `void*` handle, which does
// not fit Rust's RAII guards -- these are minimal hand-rolled primitives
// built for exactly that shape, not wrappers around `lock`'s guard-based
// Mutex/RwLock. All of them busy-wait (no real thread blocking is wired
// up yet), which is correct but potentially wasteful under contention.
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_alloc_spinlock(handle: *mut *mut c_void) -> NV_STATUS {
    if handle.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let lock = Box::new(AtomicBool::new(false));
    unsafe { *handle = Box::into_raw(lock) as *mut c_void };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_free_spinlock(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle as *mut AtomicBool)) };
    }
}
#[no_mangle]
pub extern "C" fn os_acquire_spinlock(handle: *mut c_void) -> NvU64 {
    let lock = unsafe { &*(handle as *const AtomicBool) };
    while lock
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    0 // no IRQL concept to restore
}
#[no_mangle]
pub extern "C" fn os_release_spinlock(handle: *mut c_void, _old_irql: NvU64) {
    let lock = unsafe { &*(handle as *const AtomicBool) };
    lock.store(false, Ordering::Release);
}

// Mutex: same busy-wait primitive as the spinlock above (no sleeping
// available at this layer yet); `os_cond_acquire_mutex` is the one
// non-blocking variant, using try-lock semantics.
#[no_mangle]
pub extern "C" fn os_alloc_mutex(handle: *mut *mut c_void) -> NV_STATUS {
    os_alloc_spinlock(handle)
}
#[no_mangle]
pub extern "C" fn os_free_mutex(handle: *mut c_void) {
    os_free_spinlock(handle)
}
#[no_mangle]
pub extern "C" fn os_acquire_mutex(handle: *mut c_void) -> NV_STATUS {
    os_acquire_spinlock(handle);
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_cond_acquire_mutex(handle: *mut c_void) -> NV_STATUS {
    let lock = unsafe { &*(handle as *const AtomicBool) };
    match lock.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
        Ok(_) => NV_OK,
        Err(_) => NV_ERR_TIMEOUT,
    }
}
#[no_mangle]
pub extern "C" fn os_release_mutex(handle: *mut c_void) {
    os_release_spinlock(handle, 0)
}

// Semaphore: atomic counter, spin-wait on acquire.
#[no_mangle]
pub extern "C" fn os_alloc_semaphore(initial_value: NvU32) -> *mut c_void {
    Box::into_raw(Box::new(AtomicU32::new(initial_value))) as *mut c_void
}
#[no_mangle]
pub extern "C" fn os_free_semaphore(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle as *mut AtomicU32)) };
    }
}
#[no_mangle]
pub extern "C" fn os_acquire_semaphore(handle: *mut c_void) -> NV_STATUS {
    let sem = unsafe { &*(handle as *const AtomicU32) };
    loop {
        let cur = sem.load(Ordering::Acquire);
        if cur > 0
            && sem
                .compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return NV_OK;
        }
        core::hint::spin_loop();
    }
}
#[no_mangle]
pub extern "C" fn os_cond_acquire_semaphore(handle: *mut c_void) -> NV_STATUS {
    let sem = unsafe { &*(handle as *const AtomicU32) };
    let cur = sem.load(Ordering::Acquire);
    if cur > 0
        && sem
            .compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    {
        NV_OK
    } else {
        NV_ERR_TIMEOUT
    }
}
#[no_mangle]
pub extern "C" fn os_release_semaphore(handle: *mut c_void) -> NV_STATUS {
    let sem = unsafe { &*(handle as *const AtomicU32) };
    sem.fetch_add(1, Ordering::AcqRel);
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_semaphore_may_sleep() -> NvBool {
    NV_FALSE
}

// RwLock: isize state (0 = free, -1 = writer, N>0 = N readers).
#[no_mangle]
pub extern "C" fn os_alloc_rwlock() -> *mut c_void {
    Box::into_raw(Box::new(AtomicIsize::new(0))) as *mut c_void
}
#[no_mangle]
pub extern "C" fn os_free_rwlock(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle as *mut AtomicIsize)) };
    }
}
#[no_mangle]
pub extern "C" fn os_acquire_rwlock_read(handle: *mut c_void) -> NV_STATUS {
    let lock = unsafe { &*(handle as *const AtomicIsize) };
    loop {
        let cur = lock.load(Ordering::Acquire);
        if cur >= 0
            && lock
                .compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return NV_OK;
        }
        core::hint::spin_loop();
    }
}
#[no_mangle]
pub extern "C" fn os_acquire_rwlock_write(handle: *mut c_void) -> NV_STATUS {
    let lock = unsafe { &*(handle as *const AtomicIsize) };
    while lock
        .compare_exchange_weak(0, -1, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_cond_acquire_rwlock_read(handle: *mut c_void) -> NV_STATUS {
    let lock = unsafe { &*(handle as *const AtomicIsize) };
    let cur = lock.load(Ordering::Acquire);
    if cur >= 0
        && lock
            .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    {
        NV_OK
    } else {
        NV_ERR_TIMEOUT
    }
}
#[no_mangle]
pub extern "C" fn os_cond_acquire_rwlock_write(handle: *mut c_void) -> NV_STATUS {
    let lock = unsafe { &*(handle as *const AtomicIsize) };
    if lock
        .compare_exchange(0, -1, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        NV_OK
    } else {
        NV_ERR_TIMEOUT
    }
}
#[no_mangle]
pub extern "C" fn os_release_rwlock_read(handle: *mut c_void) {
    let lock = unsafe { &*(handle as *const AtomicIsize) };
    lock.fetch_sub(1, Ordering::Release);
}
#[no_mangle]
pub extern "C" fn os_release_rwlock_write(handle: *mut c_void) {
    let lock = unsafe { &*(handle as *const AtomicIsize) };
    lock.store(0, Ordering::Release);
}

// ---------------------------------------------------------------------
// Work queues / wait queues (STUB) -- Eclipse's async/task infra isn't
// wired into this crate yet; returning "not supported" is honest and
// safe (RM falls back to synchronous paths for most callers of these).
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_queue_work_item(_queue: *mut c_void, _data: *mut c_void) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_flush_work_queue(_queue: *mut c_void, _b: NvBool) -> NV_STATUS {
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_is_queue_flush_ongoing(_queue: *mut c_void) -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_alloc_wait_queue(handle: *mut *mut c_void) -> NV_STATUS {
    if handle.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *handle = core::ptr::null_mut() };
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_free_wait_queue(_queue: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_wait_uninterruptible(_queue: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_wait_interruptible(_queue: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_wake_up(_queue: *mut c_void) {}

// ---------------------------------------------------------------------
// Misc capability/environment queries (STUB) -- none of these apply to a
// bare-metal single desktop GPU (no hypervisor, no vGPU, no Tegra, no
// EFI-runtime concept the way Linux has one, no NUMA, no cgroups).
// ---------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn os_get_version_info(info: *mut os_version_info) -> NV_STATUS {
    if info.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe {
        (*info).os_major_version = 0;
        (*info).os_minor_version = 1;
        (*info).os_build_number = 0;
        (*info).os_build_version_str = b"eclipse\0".as_ptr() as *const c_char;
        (*info).os_build_date_plus_str = b"\0".as_ptr() as *const c_char;
    }
    NV_OK
}
#[repr(C)]
pub struct os_version_info {
    pub os_major_version: NvU32,
    pub os_minor_version: NvU32,
    pub os_build_number: NvU32,
    pub os_build_version_str: *const c_char,
    pub os_build_date_plus_str: *const c_char,
}
#[no_mangle]
pub extern "C" fn os_get_is_openrm(is_openrm: *mut NvBool) -> NV_STATUS {
    if is_openrm.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *is_openrm = NV_TRUE };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_is_bif_reset_supported(_handle: *mut c_void) -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_is_isr() -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_is_efi_enabled() -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_is_xen_dom0() -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_is_vgx_hyper() -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_inject_vgx_msi(_domain: NvU16, _addr: NvU64, _data: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_is_grid_supported() -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_get_grid_csp_support() -> NvU32 {
    0
}
#[no_mangle]
pub extern "C" fn os_bug_check(code: NvU32, message: *const c_char) -> ! {
    let msg = if message.is_null() {
        "(no message)"
    } else {
        unsafe {
            let mut len = 0usize;
            let mut p = message;
            while *p != 0 {
                len += 1;
                p = p.add(1);
            }
            core::str::from_utf8(core::slice::from_raw_parts(message as *const u8, len))
                .unwrap_or("(invalid utf8)")
        }
    };
    panic!("[nvidia-rm] os_bug_check({:#x}): {}", code, msg);
}
#[no_mangle]
pub extern "C" fn os_lock_user_pages(_a: *mut c_void, _b: NvU64, _c: *mut *mut c_void, _d: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_lookup_user_io_memory(_a: *mut c_void, _b: NvU64, _c: *mut *mut NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_unlock_user_pages(_a: NvU64, _b: *mut c_void, _c: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_match_mmap_offset(_a: *mut c_void, _b: NvU64, _c: *mut NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_get_smbios_header(_p_smbs_addr: *mut NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_get_acpi_rsdp_from_uefi(_a: *mut NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_add_record_for_crashLog(_a: *mut c_void, _b: NvU32) {}
#[no_mangle]
pub extern "C" fn os_delete_record_for_crashLog(_a: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_call_vgpu_vfio(_a: *mut c_void, _b: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_device_vm_present() -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_numa_memblock_size(_a: *mut NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_alloc_pages_node(_a: NvS32, _b: NvU32, _c: NvU32, _d: *mut NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_get_page(_address: NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_put_page(_address: NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_get_page_refcount(_address: NvU64) -> NvU32 {
    0
}
#[no_mangle]
pub extern "C" fn os_count_tail_pages(_address: NvU64) -> NvU32 {
    0
}
#[no_mangle]
pub extern "C" fn os_free_pages_phys(_a: NvU64, _b: NvU32) {}
#[no_mangle]
pub extern "C" fn os_open_temporary_file(handle: *mut *mut c_void) -> NV_STATUS {
    if !handle.is_null() {
        unsafe { *handle = core::ptr::null_mut() };
    }
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_close_file(_handle: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_write_file(_a: *mut c_void, _b: *mut NvU8, _c: NvU64, _d: NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_read_file(_a: *mut c_void, _b: *mut NvU8, _c: NvU64, _d: NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_open_readonly_file(_path: *const c_char, handle: *mut *mut c_void) -> NV_STATUS {
    if !handle.is_null() {
        unsafe { *handle = core::ptr::null_mut() };
    }
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_open_and_read_file(_path: *const c_char, _buf: *mut NvU8, _len: NvU64) -> NV_STATUS {
    // NOTE: the GSP/booter firmware blobs will most likely be loaded by a
    // path we control directly (Eclipse's own filesystem access before
    // handing buffers to RM), not through this call -- revisit if RM
    // actually exercises this for something we need.
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_is_nvswitch_present() -> NvBool {
    NV_FALSE
}
/// The RDRAND intrinsic requires the `rdrand` target feature enabled on
/// the calling function itself (not just present at compile time), and
/// `#[target_feature]` functions must be `unsafe fn` -- kept as a small
/// private helper so the exported `os_get_random_bytes` can stay a plain
/// `extern "C" fn` matching NVIDIA's real signature exactly.
#[target_feature(enable = "rdrand")]
unsafe fn rdrand64_step(val: &mut u64) -> i32 {
    core::arch::x86_64::_rdrand64_step(val)
}

/// REAL: x86 RDRAND, no OS entropy pool needed.
#[no_mangle]
pub extern "C" fn os_get_random_bytes(bytes: *mut NvU8, length: NvU16) -> NV_STATUS {
    if bytes.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let mut remaining = length as usize;
    let mut out = bytes;
    while remaining > 0 {
        let mut val: u64 = 0;
        let ok = unsafe { rdrand64_step(&mut val) };
        if ok == 0 {
            return NV_ERR_GENERIC;
        }
        let chunk = core::cmp::min(remaining, 8);
        unsafe {
            core::ptr::copy_nonoverlapping(&val as *const u64 as *const u8, out, chunk);
            out = out.add(chunk);
        }
        remaining -= chunk;
    }
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_get_current_process_flags() -> NvU32 {
    0 // OS_CURRENT_PROCESS_FLAG_NONE
}
#[no_mangle]
pub extern "C" fn os_nv_cap_init(_path: *const c_char) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_nv_cap_create_dir_entry(_a: *mut c_void, _b: *const c_char, _c: i32) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_nv_cap_create_file_entry(_a: *mut c_void, _b: *const c_char, _c: i32) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_nv_cap_destroy_entry(_a: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_nv_cap_validate_and_dup_fd(_a: *const c_void, fd: i32) -> i32 {
    fd
}
#[no_mangle]
pub extern "C" fn os_nv_cap_close_fd(_fd: i32) {}
#[no_mangle]
pub extern "C" fn os_imex_channel_get(_a: NvU64) -> NvS32 {
    -1
}
#[no_mangle]
pub extern "C" fn os_imex_channel_count() -> NvS32 {
    0
}
#[no_mangle]
pub extern "C" fn os_tegra_igpu_perf_boost(_a: *mut c_void, _b: NvBool, _c: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_get_tegra_platform(_a: *mut NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_get_numa_node_memory_usage(_a: NvS32, _b: *mut NvU64, _c: *mut NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_numa_add_gpu_memory(_a: *mut c_void, _b: NvU64, _c: NvU64, _d: *mut NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_numa_remove_gpu_memory(_a: *mut c_void, _b: NvU64, _c: NvU64, _d: NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_offline_page_at_address(_address: NvU64) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_get_pid_info() -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_put_pid_info(_pid_info: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_find_ns_pid(_pid_info: *mut c_void, ns_pid: *mut NvU32) -> NV_STATUS {
    if !ns_pid.is_null() {
        unsafe { *ns_pid = 0 };
    }
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_is_init_ns() -> NvBool {
    NV_TRUE // no namespaces at all -- vacuously "the" (only) namespace
}
#[no_mangle]
pub extern "C" fn os_iommu_sva_bind(_a: *mut c_void, _b: *mut *mut c_void, _c: *mut NvU32) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_iommu_sva_unbind(_handle: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_supports_kernel_suspend_notifiers() -> NvBool {
    NV_FALSE
}
#[no_mangle]
pub extern "C" fn os_cgroup_implementation() -> NvU32 {
    0 // OS_CGROUP_IMPL_NONE
}
#[no_mangle]
pub extern "C" fn os_dmem_cgroup_register_region(_size: NvU64, _name: *const c_char) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_dmem_cgroup_unregister_region(_region: *mut c_void) {}
#[no_mangle]
pub extern "C" fn os_dmem_cgroup_try_charge(
    _region: *mut c_void,
    _size: NvU64,
    _ret_pool: *mut *mut c_void,
    _ret_limit_pool: *mut *mut c_void,
) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_dmem_cgroup_uncharge(_pool: *mut c_void, _size: NvU64) {}
#[no_mangle]
pub extern "C" fn os_cgroup_for_pid(_pid: i32, _pid_info: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_cgroup_get_from_fd(_fd: NvU32) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_cgroup_put(_cgroup: *mut c_void) {}
