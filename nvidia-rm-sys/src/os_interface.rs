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
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use lock::Mutex;

// ---------------------------------------------------------------------
// Globals RM reads directly (not functions) -- from the bottom of
// os-interface.h. 4 KiB pages, no huge pages, no confidential computing,
// no dma-buf/imex support yet.
// ---------------------------------------------------------------------
#[no_mangle]
pub static os_page_size: NvU64 = 4096;
#[no_mangle]
pub static os_max_page_size: NvU64 = 4096;
// NV_PAGE_MASK semantics (kernel-open/nvidia/os-interface.c:53 + usage at
// :1523-1525): the page-ALIGNMENT mask -- `start & os_page_mask` keeps the
// page BASE and `start & ~os_page_mask` the in-page offset. The old value
// 0xFFF was the bitwise COMPLEMENT of the contract: any consumer would have
// collapsed addresses to their low 12 bits. Latent today (the arch/nvalloc
// files that read it are not compiled in), but a wrong-typed constant on a
// live export is a trap for every future vendoring step.
#[no_mangle]
pub static os_page_mask: NvU64 = !0xFFFu64;
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
/// The widest `HEADER_PAD + size` `os_free_mem` will accept back from the
/// header before calling it corruption. `os_alloc_mem` refuses anything larger
/// up front, because an allocation this pair cannot free is a silent permanent
/// leak reported as heap corruption -- and the x86_64 kernel heap is 512 MiB
/// (`KERNEL_HEAP_SIZE` in zCore/src/memory_x86_64.rs), so a request above this
/// ceiling is one the heap could otherwise have served.
const MAX_SANE_TOTAL: usize = 256 * 1024 * 1024;

#[no_mangle]
pub extern "C" fn os_alloc_mem(p_address: *mut *mut c_void, size: NvU64) -> NV_STATUS {
    if p_address.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    // Cleared before anything can fail, which is what the real os_alloc_mem
    // does on entry (`*address = NULL;`) and what at least one caller relies on
    // instead of the status: `_portMemAllocNonPagedUntracked`
    // (memory_unix_kernel_os.c:61) calls this and returns the pointer without
    // ever looking at what came back. That caller zeroes its own local first,
    // so it survives either way -- but the contract is the contract, and the
    // next caller may not.
    unsafe { *p_address = core::ptr::null_mut() };
    let total = match HEADER_PAD.checked_add(size as usize) {
        Some(t) => t,
        None => return NV_ERR_INVALID_ARGUMENT,
    };
    if total > MAX_SANE_TOTAL {
        // Not INVALID_ARGUMENT: the size is well-formed, we just will not be
        // able to hand it back. NV_ERR_NO_MEMORY is what every caller already
        // handles for "this allocator cannot serve that".
        return NV_ERR_NO_MEMORY;
    }
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
        // Sanity-gate the size recovered from the header before handing it to
        // the buddy allocator. os_alloc_mem always stores `HEADER_PAD + size`,
        // so a legitimate `total` is >= HEADER_PAD and comfortably bounded. A
        // wild value here means the header was clobbered: a double free (the
        // block already went back to LockedHeap, which wrote free-list pointers
        // over the size word), an RM buffer underrun (a store just before
        // p_address), or a stale/corrupt pointer. dealloc()ing with a bogus
        // size corrupts the buddy metadata, and a corrupt buddy hands out
        // OVERLAPPING blocks — one of which lands on a live coroutine stack or a
        // page table and gets sprayed with zeros. That is exactly the multi-core
        // signature we are chasing: [null-exec] / heap_smash=true, a 24-byte zero
        // run over a return slot, and "page table entry has reserved bits set".
        // Refuse the free: leak this one block (bounded, recoverable, and now
        // DIAGNOSED) rather than let a single bad free corrupt the whole heap.
        const MIN_SANE: usize = HEADER_PAD;
        if !(MIN_SANE..=MAX_SANE_TOTAL).contains(&total) {
            log::error!(
                "[nvidia-rm] os_free_mem: REFUSING free of {:p} — header size \
                 {:#x} is insane (raw {:p}); double-free or header corruption. \
                 Leaking the block to protect the kernel heap (prevents the \
                 [null-exec]/heap_smash stack+pagetable corruption).",
                p_address,
                total,
                raw,
            );
            return;
        }
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
/// Twin of `os_services::osGetMonotonicTickResolutionNs`, which said 1_000
/// where this said 1. `gpu_timeout.c` adds one of these to every GPU timeout,
/// so the two padded every timeout differently by a factor of a thousand.
#[no_mangle]
pub extern "C" fn os_get_monotonic_tick_resolution_ns() -> NvU64 {
    crate::os_services::osGetMonotonicTickResolutionNs()
}
#[no_mangle]
pub extern "C" fn os_delay(milliseconds: NvU32) -> NV_STATUS {
    crate::hooks::delay_us_or_spin(milliseconds.saturating_mul(1000));
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_delay_us(microseconds: NvU32) -> NV_STATUS {
    crate::hooks::delay_us_or_spin(microseconds);
    NV_OK
}
/// Real: TSC frequency in Hz, calibrated once against the hook-provided
/// microsecond delay and cached. Feeds `osGetCpuFrequency` (os_init.c,
/// Hz -> MHz) and from there `pSys->cpuInfo.clock` (cpu.c) -- a 0 here is
/// not a divisor anywhere at construction time (checked), but a 0 MHz
/// CPU clock would flow into later consumers (e.g. GSP boot arguments),
/// so report the real value.
/// Calibrated once and kept, because the calibration costs 10 ms of wall time.
/// At module scope rather than inside the function so a test can put a known
/// frequency in it: on the host the calibration measures a `delay_us` that does
/// not delay, which comes out under a megahertz and therefore zero megahertz --
/// the same answer as the bug, which would make the test that catches the bug
/// unable to fail.
static CACHED_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Test-only: put `hz` in the cache and hand back what was there.
#[cfg(test)]
pub(crate) fn swap_cached_cpu_hz(hz: NvU64) -> NvU64 {
    CACHED_HZ.swap(hz, Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn os_get_cpu_frequency() -> NvU64 {
    let cached = CACHED_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    log::debug!("[nvidia-rm] os_get_cpu_frequency: calibrating TSC (10ms)...");
    // 10 ms calibration window: long enough to make delay_us's own
    // resolution error negligible, short enough to be a one-off blip.
    #[cfg(target_arch = "x86_64")]
    let hz = {
        let t0 = unsafe { core::arch::x86_64::_rdtsc() };
        with_hooks((), |h| h.delay_us(10_000));
        let t1 = unsafe { core::arch::x86_64::_rdtsc() };
        t1.wrapping_sub(t0).saturating_mul(100)
    };
    // On non-x86_64 architectures the TSC intrinsic is unavailable; fall
    // back to 1 GHz as a conservative placeholder (RM uses this only for
    // timeout calculations, not performance measurement).
    #[cfg(not(target_arch = "x86_64"))]
    let hz = 1_000_000_000u64;
    log::debug!(
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
/// The name this kernel answers with when the RM asks who is running.
pub(crate) const PROCESS_NAME: &[u8] = b"eclipse-kernel";

/// Copy [`PROCESS_NAME`] into a caller's buffer, **always NUL-terminated**.
///
/// The terminator is the whole contract, because the RM prints the result with
/// `%s` out of a buffer it never zeroed: `kernel_rc.c:349` hands the Xid path a
/// fresh `portMemAllocNonPaged(NV_PROC_NAME_MAX_LENGTH)` and prints it as
/// `name=%s`, so a copy that fills the buffer edge to edge leaves `%s` reading
/// whatever follows the allocation. Linux truncates the same way -- `strscpy`
/// returns `-E2BIG` and its caller deliberately ignores it -- but never leaves
/// the buffer unterminated.
pub(crate) fn write_process_name(buffer: *mut c_char, length: NvU32) {
    if buffer.is_null() || length == 0 {
        return;
    }
    // One byte of the caller's room belongs to the terminator, always.
    let room = length as usize - 1;
    let n = core::cmp::min(PROCESS_NAME.len(), room);
    unsafe {
        core::ptr::copy_nonoverlapping(PROCESS_NAME.as_ptr(), buffer as *mut u8, n);
        *(buffer as *mut u8).add(n) = 0;
    }
}

#[no_mangle]
pub extern "C" fn os_get_current_process_name(buffer: *mut c_char, length: NvU32) {
    write_process_name(buffer, length);
}
/// Provider of per-thread identity for the RM (`set_thread_id_provider`).
/// Stored as a raw fn pointer in an atomic so this leaf crate needs no
/// dependency on the kernel's thread machinery.
///
/// Deliberately LEFT UNREGISTERED today, after hardware tried both regimes.
/// The RM keys its API-lock reentrancy guard (`rmapiLockAcquire` begins
/// with `NV_ASSERT_OR_RETURN(!rmapiLockIsOwner(), NV_ERR_INVALID_LOCK_STATE)`)
/// and its threadState tracking on this id:
///
/// * Constant 0, unserialized: an entire bring-up (enumeration, channels,
///   VM_BIND, EXEC with confirmed fence) ran flawlessly, but two userspace
///   threads in CONCURRENT ioctls looked like one thread and the second was
///   refused with 0x2f (`GEM_NEW ... NV_STATUS=0x2f` killing the
///   compositor's swapchain mid-submission).
/// * Real per-thread ids: the RM entered blocking paths this port never
///   validated. One boot froze at the first GPU probe; adding RM_CALL_GATE
///   only turned the freeze into every RM-backed ioctl failing fast
///   (vkCreateDevice -13 before anything drew).
///
/// The resolution is the gate, not the ids: RM_CALL_GATE (rm_init.rs)
/// serializes every ioctl-time RM call, so RM-level concurrency -- the only
/// thing real ids were for -- cannot happen. A serialized RM is exactly
/// what one constant thread id describes. If the gate is ever lifted, the
/// provider is the first thing to bring back, together with real
/// os-layer wait primitives for the RM's blocking paths.
static THREAD_ID_PROVIDER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

pub fn set_thread_id_provider(f: fn() -> u64) {
    THREAD_ID_PROVIDER.store(f as usize, core::sync::atomic::Ordering::Release);
}

#[no_mangle]
pub extern "C" fn os_get_current_thread(thread_id: *mut NvU64) -> NV_STATUS {
    if thread_id.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let f = THREAD_ID_PROVIDER.load(core::sync::atomic::Ordering::Acquire);
    let id = if f != 0 {
        // SAFETY: only ever stored from `set_thread_id_provider(fn() -> u64)`.
        let f: fn() -> u64 = unsafe { core::mem::transmute(f) };
        f()
    } else {
        // Boot-time RM calls before the provider is registered are
        // serialized, so a constant id is safe there.
        0
    };
    unsafe { *thread_id = id };
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
pub extern "C" fn os_memcpy_from_user(
    _to: *mut c_void,
    _from: *const c_void,
    _n: NvU32,
) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_memcpy_to_user(
    _to: *mut c_void,
    _from: *const c_void,
    _n: NvU32,
) -> NV_STATUS {
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
    let handle =
        0x8000_0000usize | ((bus as usize) << 16) | ((slot as usize) << 8) | (function as usize);

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
pub extern "C" fn os_pci_read_byte(
    handle: *mut c_void,
    offset: NvU32,
    value: *mut NvU8,
) -> NV_STATUS {
    if value.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *value = with_hooks(0xFF, |h| h.pci_config_read(handle as usize, offset, 1)) as NvU8 };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_read_word(
    handle: *mut c_void,
    offset: NvU32,
    value: *mut NvU16,
) -> NV_STATUS {
    if value.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe {
        *value = with_hooks(0xFFFF, |h| h.pci_config_read(handle as usize, offset, 2)) as NvU16
    };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_read_dword(
    handle: *mut c_void,
    offset: NvU32,
    value: *mut NvU32,
) -> NV_STATUS {
    if value.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe {
        *value = with_hooks(0xFFFF_FFFF, |h| {
            h.pci_config_read(handle as usize, offset, 4)
        })
    };
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_write_byte(handle: *mut c_void, offset: NvU32, value: NvU8) -> NV_STATUS {
    with_hooks((), |h| {
        h.pci_config_write(handle as usize, offset, 1, value as u32)
    });
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_write_word(handle: *mut c_void, offset: NvU32, value: NvU16) -> NV_STATUS {
    with_hooks((), |h| {
        h.pci_config_write(handle as usize, offset, 2, value as u32)
    });
    NV_OK
}
#[no_mangle]
pub extern "C" fn os_pci_write_dword(
    handle: *mut c_void,
    offset: NvU32,
    value: NvU32,
) -> NV_STATUS {
    with_hooks((), |h| {
        h.pci_config_write(handle as usize, offset, 4, value)
    });
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
/// Test-only tally of how many times `write_combine_fence` ran, so a test can
/// prove that BOTH exported spellings of the flush reach it. The fence itself
/// leaves no trace a test could see.
#[cfg(test)]
pub(crate) static WC_FENCES: AtomicU32 = AtomicU32::new(0);

/// The one store fence behind every write-combine flush in this crate.
///
/// The RM writes into write-combined mappings (BAR1 pushbuffers, notifiers,
/// semaphores) and then tells the GPU to go read them; this is the only thing
/// between the two. It exists as a shared helper because the ABI spells the
/// same flush twice -- `os_flush_cpu_write_combine_buffer` here and
/// `osFlushCpuWriteCombineBuffer` in `os_boundary.rs` -- and one of the two
/// used to be an empty body, which is a fence that silently isn't one.
pub(crate) fn write_combine_fence() {
    #[cfg(test)]
    WC_FENCES.fetch_add(1, Ordering::Relaxed);
    // x86_64: SFENCE is what orders write-combining stores; a compiler fence
    // would not. Other architectures have no write-combining buffer needing an
    // explicit flush, so a full fence is both sufficient and available.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("sfence")
    };
    #[cfg(not(target_arch = "x86_64"))]
    core::sync::atomic::fence(Ordering::SeqCst);
}

#[no_mangle]
pub extern "C" fn os_flush_cpu_write_combine_buffer() {
    write_combine_fence();
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
/// Both of these are the OS half of a pair the real driver ties together:
/// `os.c` defines `osIsAdministrator()` as `return os_is_administrator();` and
/// `osCheckAccess()` as `return os_check_access(accessRight);`. build.rs
/// excludes `arch/nvalloc/unix/src/`, so this crate supplies both halves, and
/// these two used to answer the opposite of their twins -- a blanket yes here
/// against a considered no there. Unified on the twin, which is the half the
/// compiled RM actually calls, so the answer cannot depend on which spelling a
/// caller happens to use. The privilege POLICY lives in one place now
/// (`os_boundary::access_granted`); this is only the plumbing.
#[no_mangle]
pub extern "C" fn os_is_administrator() -> NvBool {
    crate::os_boundary::osIsAdministrator()
}
/// `RsAccessRight` is `NvU16` (src/common/sdk/nvidia/inc/rs_access.h:73), not
/// `u32`; the width is fixed here along with the answer.
#[no_mangle]
pub extern "C" fn os_check_access(access_right: NvU16) -> NvBool {
    crate::os_boundary::osCheckAccess(access_right)
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
// ---------------------------------------------------------------------
// Log capture. On the real bring-up box the kernel `log::warn!` stream
// (where every RM nv_printf / assert / ECLIPSE_TRACE line lands) does NOT
// reach the monitor the user watches -- only the `cat /proc/gpustepN`
// stdout does. So a diagnostic that only `log::warn!`s is invisible in
// practice. This lets the driver bracket a bring-up call
// (capture_begin/capture_take) and fold the RM's own narration into the
// String `cat` returns, which the user can actually read. Bounded so a
// chatty RmMsg rule can't grow it without limit.
// ---------------------------------------------------------------------
static LOG_CAPTURE: Mutex<Option<String>> = Mutex::new(None);

// When set, RM narration is echoed at ERROR level (not WARN) so it survives
// the kernel's default `LOG=error` max-level filter and reaches the live
// console. This is the ONLY way to see how far a *crashing* RM path got: the
// capture buffer (folded into the /proc read) is only emitted on a clean
// return, so a page fault mid-step loses it entirely. Scoped by the caller
// (live_echo_begin/end) around a risky step (e.g. gpuStateInit) so the far
// chattier, non-crashing GSP-boot narration isn't dumped live every boot.
static LIVE_ECHO: AtomicBool = AtomicBool::new(false);

/// Echo subsequent RM narration at ERROR level so it passes `LOG=error` and
/// appears live on the console. Pair with `live_echo_end`.
pub fn live_echo_begin() {
    LIVE_ECHO.store(true, Ordering::Relaxed);
}

/// Stop live-echoing RM narration.
pub fn live_echo_end() {
    LIVE_ECHO.store(false, Ordering::Relaxed);
}

/// Saved `log::max_level` across a console-quiet window (encoded as usize via
/// `LevelFilter as usize`; usize::MAX = no window active).
static QUIET_SAVED_LEVEL: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

/// Enter the console-quiet window: suppress ALL kernel log rendering (the
/// console framebuffer lives in the console GPU's own BAR1, so every log line
/// is a burst of posted CPU writes into the GPU under bring-up). This is
/// Eclipse's equivalent of what the Linux driver does around kgspInitRm on a
/// client-managed-console GPU: os_disable_console_access() == console_lock()
/// (osinit.c:1841-1845, NVIDIA's own comment: "to ensure no console writes
/// through BAR1 can interfere"). Every prior console-GPU boot here rendered
/// the live seq-trace INTO BAR1 interleaved with the sequencer MMIO -- the
/// single biggest divergence from Linux in the exact wedge window. Capture
/// paths (capture_push / seq trace) are unaffected: they run before the log
/// macros, so the narration still lands in the /proc output afterwards.
/// Pair with `console_quiet_end`. A nested begin is safe: only the outermost
/// one records a level to go back to.
pub fn console_quiet_begin() {
    let cur = log::max_level();
    // Record the level from OUTSIDE the outermost window, and only that one.
    // A nested begin used to overwrite it with `Off` -- the level the outer
    // begin had just set -- so the next end "restored" silence and the console
    // never came back. That nesting is real: the wedge watch latches rendering
    // off from inside the GSP-boot window (os_boundary's dead-fabric branch),
    // and a RECOVERED wedge then left the machine dark for the rest of the
    // boot, starting with the very line announcing the recovery.
    let _ = QUIET_SAVED_LEVEL.compare_exchange(
        usize::MAX,
        cur as usize,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    log::set_max_level(log::LevelFilter::Off);
}

/// Set while rendering must stay off no matter how the windows around it
/// close: the console framebuffer lives in a wedged GPU's BAR1 and the next
/// rendered line would kill the machine.
static QUIET_LATCHED: AtomicBool = AtomicBool::new(false);

/// Latch console rendering off until something says the framebuffer is safe
/// again. Unlike [`console_quiet_begin`] this outlives a
/// [`console_quiet_end`]; only [`console_quiet_unlatch`] lifts it, and the
/// level to go back to is kept for whoever does.
pub fn console_quiet_latch() {
    QUIET_LATCHED.store(true, Ordering::Relaxed);
    log::set_max_level(log::LevelFilter::Off);
}

/// Declare the console framebuffer safe to render into again. Called from
/// `os_boundary::wedge_fake_mmio_clear`, which is what the recovery path runs
/// once the device answers config space: it does not restore the level by
/// itself, it lets the next [`console_quiet_end`] do it.
pub fn console_quiet_unlatch() {
    QUIET_LATCHED.store(false, Ordering::Relaxed);
}

/// Whether console rendering is latched off (for the /proc report).
pub fn console_quiet_latched() -> bool {
    QUIET_LATCHED.load(Ordering::Relaxed)
}

/// Leave the console-quiet window, restoring the level from before the
/// outermost begin. An unpaired call (no matching begin) is a no-op; so is one
/// made while rendering is latched off, which keeps the saved level for the
/// call that follows the unlatch.
pub fn console_quiet_end() {
    if QUIET_LATCHED.load(Ordering::Relaxed) {
        return;
    }
    let saved = QUIET_SAVED_LEVEL.swap(usize::MAX, Ordering::Relaxed);
    log::set_max_level(match level_from_usize(saved) {
        Some(level) => level,
        None => return, // usize::MAX sentinel: no window was active
    });
}

/// The `log::LevelFilter` a `LevelFilter as usize` came from.
///
/// [`console_quiet_begin`] stores the discriminant and this reads it back, so
/// the two halves have to agree about all six of them; a test walks every
/// level through a window rather than trusting that they do.
fn level_from_usize(saved: usize) -> Option<log::LevelFilter> {
    Some(match saved {
        0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Error,
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        5 => log::LevelFilter::Trace,
        _ => return None,
    })
}

/// Probe-diagnostic line: ALWAYS lands in the capture buffer (folded into the
/// /proc output afterwards) and ALSO renders at ERROR level when rendering is
/// on. Inside the console-quiet window the log macro short-circuits at
/// `max_level` (Off) but the capture still records -- use this instead of a
/// bare `log::error!` for any diagnostic that runs inside the GSP boot
/// window, or quiet mode silently discards it (review finding).
pub fn probe_line(s: &str) {
    capture_push(s);
    log::error!("{}", s);
}
// Generous cap: the GSP-RM boot path (gpustep6) narrates far more than the
// attach path, and the whole buffer is folded into the /proc read (which is
// offset-chunked, so size is not a problem). Bounded only so a runaway loop
// can't grow it without limit.
const LOG_CAPTURE_CAP: usize = 256 * 1024;

/// Start (or restart) capturing RM log lines into an in-memory buffer, in
/// addition to the normal `log::warn!` sink.
pub fn capture_begin() {
    CAPTURE_DROPPED.store(0, Ordering::Relaxed);
    *LOG_CAPTURE.lock() = Some(String::new());
}

/// Narration lines the cap turned away since the last `capture_begin`.
static CAPTURE_DROPPED: AtomicU32 = AtomicU32::new(0);

/// Stop capturing and return everything captured since `capture_begin`, with a
/// final line when the cap turned any narration away.
///
/// The buffer is the only place the RM's narration reaches the person reading
/// it -- the kernel log stream does not reach the monitor on the bring-up box,
/// only `cat /proc/gpustepN` does -- and what the cap drops is the TAIL, which
/// is the part nearest whatever went wrong. Dropping it quietly made a
/// truncated report look like a complete one that simply stopped.
pub fn capture_take() -> Option<String> {
    let mut buf = LOG_CAPTURE.lock().take()?;
    let dropped = CAPTURE_DROPPED.swap(0, Ordering::Relaxed);
    if dropped != 0 {
        let _ = core::fmt::Write::write_fmt(
            &mut buf,
            format_args!(
                "[nvidia-rm] ...TRUNCATED: {} further narration line(s) dropped after the {} KiB capture cap; the end of this boot's narration is NOT here.\n",
                dropped,
                LOG_CAPTURE_CAP / 1024
            ),
        );
    }
    Some(buf)
}

fn capture_push(s: &str) {
    let mut guard = LOG_CAPTURE.lock();
    if let Some(buf) = guard.as_mut() {
        if buf.len() < LOG_CAPTURE_CAP {
            buf.push_str(s);
            buf.push('\n');
        } else {
            CAPTURE_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Append a line to the active capture buffer from outside this module
/// (used by os_boundary's SEC2-resume register trace so the traced register
/// sequence lands in the /proc output block too, not just the live screen).
pub fn capture_line(s: &str) {
    capture_push(s);
}

/// Whether live-echo (ERROR-level narration) is currently armed.
pub fn live_echo_on() -> bool {
    LIVE_ECHO.load(Ordering::Relaxed)
}

fn log_raw_cstr(str_: *const c_char) {
    // Every RM nv_printf / assert line lands here (nvassert.c -> nv_printf).
    // The console GPU's GSP bring-up narrates hundreds of these per boot; now
    // that bring-up is done and stable, the routine narration is demoted to
    // DEBUG so it is dropped at the default LOG level and no longer floods the
    // desktop-boot console. It is NOT lost: `capture_push` still records every
    // line into the /proc/gpustep* buffers, `osAssertFailed` still fires its
    // own ERROR line on a real assert, and arming live-echo re-promotes this
    // path to ERROR for step-by-step debugging.
    unsafe {
        let mut p = str_;
        let mut len = 0usize;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
        let slice = core::slice::from_raw_parts(str_ as *const u8, len);
        // GPU-independent survival breadcrumb: bump the CMOS narration counter on
        // every RM line so a wedge's surviving count says how far the RM's own
        // narration got (see crate::survival / /proc/gpusurvive).
        crate::survival::narration_tick();
        // A single byte the RM got from the GPU (a monitor name out of an EDID,
        // a VBIOS string) used to drop the WHOLE line: no log, no capture, and
        // with it the two things this routine latches on -- arming the
        // sequencer trace and restoring PDISP. Replace the bad bytes instead;
        // `from_utf8_lossy` borrows and allocates nothing for the normal line.
        let lossy = alloc::string::String::from_utf8_lossy(slice);
        {
            let s: &str = &lossy;
            // ERROR level when live-echo is armed (opt-in step debugging);
            // DEBUG otherwise, so the routine RM narration is filtered at the
            // default LOG level.
            //
            // EXCEPTION, always at ERROR: the RM's robust-channel / Xid
            // narration. When the GPU kills a channel (e.g. an MMU fault),
            // the RM prints exactly one burst naming the engine, the fault
            // type and -- crucially -- the FAULTING ADDRESS. That burst
            // happens asynchronously (GSP event processing), far from any
            // capture_begin window, so demoting it to DEBUG made the single
            // most important line of a dead-channel boot invisible. Xid
            // bursts are rare and bounded (the channel is dead afterwards),
            // so there is no flood risk.
            let xid = s.contains("Xid") || s.contains("MMU Fault") || s.contains("MMU fault");
            // Also always at ERROR: the assert TEXT itself. nvassert.c's
            // NoLog helpers print "Assertion failed: <expr> @ <file>:<line>"
            // through this very path -- at DEBUG that left `osAssertFailed()`
            // pointing at an "adjacent message" the console never carried
            // (hardware showed the marker, alone, while vkCreateDevice died
            // with -13 and nothing named the assert). Promote it, but
            // suppress consecutive identical lines: the marker is deduped
            // per call site by the RM, the text is NOT, and a per-ioctl
            // assert would otherwise own the UART (~15 ms/line).
            let assert_text = s.contains("Assertion failed");
            let assert_fresh = assert_text && {
                let mut h = 0xcbf2_9ce4_8422_2325u64; // FNV-1a
                for &b in s.as_bytes() {
                    h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
                }
                static LAST_ASSERT_HASH: core::sync::atomic::AtomicU64 =
                    core::sync::atomic::AtomicU64::new(0);
                LAST_ASSERT_HASH.swap(h, Ordering::Relaxed) != h
            };
            // Also always at ERROR: Eclipse's GR bring-up diagnostics, tagged
            // "ECLIPSE-GR" (golden-image creation in kernel_graphics.c, global
            // ctx-buffer mapping in kernel_graphics_context.c). Those fire during
            // GR StateLoad / the first client's PROMOTE_CTX -- outside any
            // live_echo or capture window -- so at the default LOG level they
            // were demoted to DEBUG and dropped, right along with the routine
            // GSP narration, leaving the FECS-RESTORE-hang investigation blind
            // (the single line that says whether the GRAPHICS golden image was
            // created, and whether the global ctx buffers were mapped to the
            // channel VAS, never reached dmesg). Promote exactly these tagged
            // lines: they are a handful per boot -- one golden verdict plus four
            // ctx-buffer map results per client -- not a flood.
            let eclipse_diag = s.contains("ECLIPSE-GR");
            if LIVE_ECHO.load(Ordering::Relaxed) || xid || assert_fresh || eclipse_diag {
                if assert_fresh {
                    log::error!("[nvidia-rm] {} (identical repeats suppressed)", s);
                } else {
                    log::error!("[nvidia-rm] {}", s);
                }
            } else {
                log::debug!("[nvidia-rm] {}", s);
            }
            capture_push(s);
            // SEC2-resume register trace goes live the moment the RM narrates
            // receiving the GSP's RUN_CPU_SEQUENCER RPC -- that captures the
            // WHOLE sequencer buffer execution (dozens of pre-STARTCPU
            // register ops), not just the post-STARTCPU tail. No-op unless
            // the trace was armed for this boot.
            if s.contains("RUN_CPU_SEQUENCER") {
                crate::os_boundary::seq_trace_go_live();
            }
            // EXP1c: the SEC2 HS-resume window (which wedges on live scanout)
            // is over once GSP's RISC-V core is up -- restore PDISP now so
            // GSP-RM finds the display engine alive during its own init.
            if s.contains("RISCV started") {
                crate::os_boundary::pdisp_restore();
            }
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
/// Twin of `os_boundary::osGetMaxUserVa`. This said one byte less than that
/// one; the shift the RM derives (`osGetCpuVaAddrShift`) came out 48 either
/// way, but two answers to "where does user space end" is one too many.
#[no_mangle]
pub extern "C" fn os_get_max_user_va() -> NvU64 {
    crate::os_boundary::osGetMaxUserVa()
}
/// Twin of `os_services::osSchedule`, which drains this CPU's TLB-shootdown
/// queue. A bare `spin_loop()` yields nothing at all, so the two disagreed
/// about what "schedule" means; unified on the one the RM calls.
#[no_mangle]
pub extern "C" fn os_schedule() -> NV_STATUS {
    crate::os_services::osSchedule()
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
    let l = unsafe { &*(handle as *const AtomicBool) };
    let mut spins: u64 = 0;
    while l
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
        spins += 1;
        // A CPU spinning here with interrupts disabled cannot ack a peer's TLB
        // shootdown; drain our own queue at a coarse cadence so an unrelated
        // munmap on another CPU (spin-waiting for our ack while it holds the
        // VMAR lock) cannot wedge behind this RM lock. Cheap: one relaxed load
        // when nothing is queued. See `lock::pump` / the kernel ticket lock.
        if spins & 511 == 0 {
            lock::pump();
        }
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
pub extern "C" fn os_lock_user_pages(
    _a: *mut c_void,
    _b: NvU64,
    _c: *mut *mut c_void,
    _d: NvU32,
) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_lookup_user_io_memory(
    _a: *mut c_void,
    _b: NvU64,
    _c: *mut *mut NvU64,
) -> NV_STATUS {
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
pub extern "C" fn os_alloc_pages_node(
    _a: NvS32,
    _b: NvU32,
    _c: NvU32,
    _d: *mut NvU64,
) -> NV_STATUS {
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
pub extern "C" fn os_open_readonly_file(
    _path: *const c_char,
    handle: *mut *mut c_void,
) -> NV_STATUS {
    if !handle.is_null() {
        unsafe { *handle = core::ptr::null_mut() };
    }
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_open_and_read_file(
    _path: *const c_char,
    _buf: *mut NvU8,
    _len: NvU64,
) -> NV_STATUS {
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
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "rdrand")]
unsafe fn rdrand64_step(val: &mut u64) -> i32 {
    core::arch::x86_64::_rdrand64_step(val)
}

/// REAL: x86 RDRAND, no OS entropy pool needed.
/// On non-x86_64 architectures RDRAND is unavailable; fill the output
/// buffer with a simple deterministic sequence (RM uses this only for
/// internal resource-server handle generation, not security-critical
/// purposes on the bring-up path).
#[no_mangle]
pub extern "C" fn os_get_random_bytes(bytes: *mut NvU8, length: NvU16) -> NV_STATUS {
    if bytes.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let mut remaining = length as usize;
    let mut out = bytes;
    #[cfg(target_arch = "x86_64")]
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
    // On non-x86_64 architectures: fill with a simple counter-based
    // sequence as a best-effort placeholder.
    #[cfg(not(target_arch = "x86_64"))]
    {
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0xdeadbeefcafe1234);
        while remaining > 0 {
            let val = COUNTER.fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed);
            let chunk = core::cmp::min(remaining, 8);
            unsafe {
                core::ptr::copy_nonoverlapping(&val as *const u64 as *const u8, out, chunk);
                out = out.add(chunk);
            }
            remaining -= chunk;
        }
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
pub extern "C" fn os_nv_cap_create_dir_entry(
    _a: *mut c_void,
    _b: *const c_char,
    _c: i32,
) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn os_nv_cap_create_file_entry(
    _a: *mut c_void,
    _b: *const c_char,
    _c: i32,
) -> *mut c_void {
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
pub extern "C" fn os_get_numa_node_memory_usage(
    _a: NvS32,
    _b: *mut NvU64,
    _c: *mut NvU64,
) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_numa_add_gpu_memory(
    _a: *mut c_void,
    _b: NvU64,
    _c: NvU64,
    _d: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
pub extern "C" fn os_numa_remove_gpu_memory(
    _a: *mut c_void,
    _b: NvU64,
    _c: NvU64,
    _d: NvU32,
) -> NV_STATUS {
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
pub extern "C" fn os_iommu_sva_bind(
    _a: *mut c_void,
    _b: *mut *mut c_void,
    _c: *mut NvU32,
) -> NV_STATUS {
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
pub extern "C" fn os_dmem_cgroup_register_region(
    _size: NvU64,
    _name: *const c_char,
) -> *mut c_void {
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

#[cfg(test)]
mod interface_tests {
    use super::*;
    extern crate std;
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
    use std::sync::Mutex as StdMutex;

    /// Everything this file exposes to a test lives in a process global -- the
    /// capture buffer, `log::max_level`, the quiet latch, the last-assert hash
    /// -- so the tests take turns and put back what they found, whether the
    /// body passes or panics.
    static TURNSTILE: StdMutex<()> = StdMutex::new(());

    fn with_globals<F: FnOnce()>(f: F) {
        let _guard = TURNSTILE.lock().unwrap_or_else(|e| e.into_inner());
        let saved_level = log::max_level();
        let saved_capture = LOG_CAPTURE.lock().take();
        let saved_dropped = CAPTURE_DROPPED.swap(0, Ordering::Relaxed);
        let saved_echo = LIVE_ECHO.swap(false, Ordering::Relaxed);
        QUIET_SAVED_LEVEL.store(usize::MAX, Ordering::Relaxed);
        QUIET_LATCHED.store(false, Ordering::Relaxed);
        let outcome = catch_unwind(AssertUnwindSafe(f));
        QUIET_LATCHED.store(false, Ordering::Relaxed);
        QUIET_SAVED_LEVEL.store(usize::MAX, Ordering::Relaxed);
        LIVE_ECHO.store(saved_echo, Ordering::Relaxed);
        CAPTURE_DROPPED.store(saved_dropped, Ordering::Relaxed);
        *LOG_CAPTURE.lock() = saved_capture;
        log::set_max_level(saved_level);
        if let Err(payload) = outcome {
            resume_unwind(payload);
        }
    }

    // -----------------------------------------------------------------
    // The name the RM prints for the process that owns a dead channel.
    // -----------------------------------------------------------------

    /// A buffer with a byte of poison on either side of the room the callee is
    /// allowed to touch, so "wrote past the end" is a visible failure rather
    /// than someone else's crash.
    const POISON: u8 = 0xA5;

    fn name_into(room: usize) -> (std::vec::Vec<u8>, usize) {
        let mut buf = std::vec![POISON; room + 8];
        write_process_name(buf.as_mut_ptr() as *mut c_char, room as NvU32);
        let canary = room;
        (buf, canary)
    }

    fn as_cstr(buf: &[u8]) -> &str {
        let end = buf.iter().position(|&b| b == 0).expect("no terminator");
        core::str::from_utf8(&buf[..end]).expect("not utf8")
    }

    #[test]
    fn the_process_name_is_the_one_this_kernel_answers_with() {
        let (buf, _) = name_into(64);
        assert_eq!(as_cstr(&buf), "eclipse-kernel");
    }

    #[test]
    fn a_process_name_that_does_not_fit_is_still_terminated() {
        // `kernel_rc.c` prints this with `%s` out of memory it never zeroed, so
        // a copy that fills the buffer edge to edge sends `%s` off the end.
        for room in 1..=("eclipse-kernel".len() + 2) {
            let (buf, _) = name_into(room);
            assert!(
                buf[..room].contains(&0),
                "a {}-byte buffer came back with no terminator",
                room
            );
            let text = as_cstr(&buf);
            assert!(
                "eclipse-kernel".starts_with(text),
                "a {}-byte buffer came back with {:?}, which is not a prefix of the name",
                room,
                text
            );
        }
    }

    #[test]
    fn the_process_name_never_writes_past_the_room_it_was_given() {
        for room in 1..=20 {
            let (buf, canary) = name_into(room);
            assert!(
                buf[canary..].iter().all(|&b| b == POISON),
                "a {}-byte buffer wrote past its end: {:?}",
                room,
                &buf[canary..]
            );
        }
    }

    #[test]
    fn a_single_byte_of_room_holds_the_terminator_and_nothing_else() {
        let (buf, _) = name_into(1);
        assert_eq!(
            buf[0], 0,
            "one byte of room must be spent on the terminator"
        );
    }

    #[test]
    fn no_room_and_no_buffer_are_both_left_alone() {
        let mut buf = std::vec![POISON; 8];
        write_process_name(buf.as_mut_ptr() as *mut c_char, 0);
        assert!(
            buf.iter().all(|&b| b == POISON),
            "zero length wrote something: {:?}",
            buf
        );
        write_process_name(core::ptr::null_mut(), 64);
    }

    #[test]
    fn both_spellings_of_the_process_name_write_the_same_bytes() {
        // The half the RM actually calls is the CamelCase one; it used to
        // return without writing anything at all.
        let mut theirs = std::vec![POISON; 40];
        let mut ours = std::vec![POISON; 40];
        crate::os_boundary::osGetCurrentProcessName(theirs.as_mut_ptr() as *mut c_char, 32);
        os_get_current_process_name(ours.as_mut_ptr() as *mut c_char, 32);
        assert_eq!(theirs, ours, "the two spellings disagree");
        assert_eq!(as_cstr(&theirs), "eclipse-kernel");
    }

    // -----------------------------------------------------------------
    // The console-quiet window around the GSP boot.
    // -----------------------------------------------------------------

    const EVERY_LEVEL: [log::LevelFilter; 6] = [
        log::LevelFilter::Off,
        log::LevelFilter::Error,
        log::LevelFilter::Warn,
        log::LevelFilter::Info,
        log::LevelFilter::Debug,
        log::LevelFilter::Trace,
    ];

    #[test]
    fn every_log_level_survives_a_quiet_window() {
        with_globals(|| {
            // The level is stored as `LevelFilter as usize` and read back by a
            // table written by hand; walk all six rather than trust that the
            // two halves agree.
            for &level in EVERY_LEVEL.iter() {
                log::set_max_level(level);
                console_quiet_begin();
                assert_eq!(
                    log::max_level(),
                    log::LevelFilter::Off,
                    "the window did not silence {}",
                    level
                );
                console_quiet_end();
                assert_eq!(log::max_level(), level, "{} did not come back", level);
            }
        });
    }

    #[test]
    fn a_window_nested_inside_another_does_not_bury_the_level() {
        with_globals(|| {
            log::set_max_level(log::LevelFilter::Warn);
            console_quiet_begin();
            console_quiet_begin();
            console_quiet_end();
            assert_eq!(
                log::max_level(),
                log::LevelFilter::Warn,
                "the inner begin saved the silence the outer one had just set"
            );
        });
    }

    #[test]
    fn a_recovered_wedge_gives_the_console_back() {
        with_globals(|| {
            // The real sequence: the GSP boot opens a quiet window, the wedge
            // watch finds the fabric dead and latches rendering off, the
            // recovery finds the device answering config space again, and the
            // window closes. The line that announces the recovery is the first
            // one that has to render.
            log::set_max_level(log::LevelFilter::Warn);
            console_quiet_begin();
            crate::os_boundary::wedge_console_suppress_for_test();
            crate::os_boundary::wedge_fake_mmio_clear();
            console_quiet_end();
            assert_eq!(
                log::max_level(),
                log::LevelFilter::Warn,
                "the console stayed dark after the wedge was recovered"
            );
        });
    }

    #[test]
    fn an_unrecovered_wedge_keeps_the_console_dark() {
        with_globals(|| {
            // The framebuffer lives in the wedged GPU's BAR1, so the next
            // rendered line would take the machine with it. Closing the window
            // must not undo the latch.
            log::set_max_level(log::LevelFilter::Warn);
            console_quiet_begin();
            crate::os_boundary::wedge_console_suppress_for_test();
            console_quiet_end();
            assert_eq!(
                log::max_level(),
                log::LevelFilter::Off,
                "closing the window rendered into a dead GPU's BAR1"
            );
            assert!(console_quiet_latched(), "the latch did not survive the end");
            // ...and the level is still there for the attempt that recovers.
            console_quiet_unlatch();
            console_quiet_end();
            assert_eq!(
                log::max_level(),
                log::LevelFilter::Warn,
                "the level was consumed by the end that had to refuse"
            );
        });
    }

    #[test]
    fn an_end_with_no_window_open_changes_nothing() {
        with_globals(|| {
            log::set_max_level(log::LevelFilter::Info);
            console_quiet_end();
            assert_eq!(log::max_level(), log::LevelFilter::Info);
        });
    }

    #[test]
    fn a_level_that_no_filter_encodes_is_not_restored_as_one() {
        assert!(level_from_usize(usize::MAX).is_none());
        assert!(level_from_usize(6).is_none());
        for (n, &level) in EVERY_LEVEL.iter().enumerate() {
            assert_eq!(level_from_usize(n), Some(level), "level {}", n);
            assert_eq!(level as usize, n, "discriminant of {}", level);
        }
    }

    // -----------------------------------------------------------------
    // The capture buffer, which is the only place this narration is read.
    // -----------------------------------------------------------------

    fn fill_capture_to_the_cap() {
        let big: String = core::iter::repeat('x').take(LOG_CAPTURE_CAP).collect();
        capture_push(&big);
    }

    #[test]
    fn a_capture_that_fit_says_nothing_about_truncation() {
        with_globals(|| {
            capture_begin();
            capture_push("one");
            capture_push("two");
            let got = capture_take().expect("no buffer");
            assert_eq!(got, "one\ntwo\n");
        });
    }

    #[test]
    fn the_capture_says_when_it_dropped_the_end_of_the_narration() {
        with_globals(|| {
            // What the cap turns away is the TAIL, which is the part nearest
            // whatever went wrong; dropping it quietly made a truncated report
            // read like a complete one.
            capture_begin();
            fill_capture_to_the_cap();
            for _ in 0..7 {
                capture_push("a line nobody will ever see");
            }
            let got = capture_take().expect("no buffer");
            assert!(
                got.contains("TRUNCATED"),
                "the report did not say it was cut short"
            );
            assert!(
                got.contains("7 further narration line(s) dropped"),
                "the report did not say how much it lost: {:?}",
                &got[got.len() - 200..]
            );
        });
    }

    #[test]
    fn a_dropped_count_does_not_leak_into_the_next_capture() {
        with_globals(|| {
            // Not via `capture_take`, which clears the count on its way out: a
            // window that is abandoned and reopened is the case that used to
            // carry the previous boot's losses into the next report.
            capture_begin();
            fill_capture_to_the_cap();
            capture_push("dropped");
            capture_begin();
            capture_push("this one fit");
            let got = capture_take().expect("no buffer");
            assert_eq!(got, "this one fit\n", "a stale drop count came along");
        });
    }

    #[test]
    fn narration_outside_a_capture_window_is_not_counted_as_dropped() {
        with_globals(|| {
            capture_push("nobody asked for this");
            capture_begin();
            capture_push("kept");
            let got = capture_take().expect("no buffer");
            assert_eq!(got, "kept\n");
        });
    }

    // -----------------------------------------------------------------
    // The RM narration path itself.
    // -----------------------------------------------------------------

    fn narrate(bytes: &[u8]) {
        let mut owned = std::vec::Vec::from(bytes);
        owned.push(0);
        log_raw_cstr(owned.as_ptr() as *const c_char);
    }

    #[test]
    fn a_narration_line_costs_nothing_that_needs_a_kernel() {
        // The breadcrumb this used to bump was four port accesses to 0x70/0x71
        // per line, with NMI masked; `out dx, al` outside the kernel is a fault,
        // so this call is the whole proof that the path is free of it.
        for _ in 0..1000 {
            crate::survival::narration_tick();
        }
    }

    #[test]
    fn a_narration_line_reaches_the_capture() {
        with_globals(|| {
            capture_begin();
            narrate(b"NVRM: ordinary narration");
            let got = capture_take().expect("no buffer");
            assert_eq!(got, "NVRM: ordinary narration\n");
        });
    }

    #[test]
    fn a_narration_line_with_a_byte_that_is_not_utf8_is_not_thrown_away() {
        with_globals(|| {
            // The RM prints strings it got from the GPU -- a monitor name out of
            // an EDID, a VBIOS string -- and one stray byte used to drop the
            // whole line: no log, no capture, nothing.
            capture_begin();
            narrate(b"NVRM: monitor name \xFF\xFE here");
            let got = capture_take().expect("no buffer");
            assert!(
                got.contains("NVRM: monitor name "),
                "the line was thrown away over a byte: {:?}",
                got
            );
            assert!(
                got.contains(" here"),
                "the text after the bad byte was lost: {:?}",
                got
            );
        });
    }

    #[test]
    fn a_bad_byte_does_not_swallow_what_the_line_was_going_to_trigger() {
        with_globals(|| {
            // This routine latches on two lines: the sequencer RPC arms the
            // register trace, and "RISCV started" restores PDISP. Dropping a
            // line over one byte dropped its side effect with it.
            crate::os_boundary::seq_trace_arm();
            capture_begin();
            narrate(b"NVRM: RPC \xFF RUN_CPU_SEQUENCER received");
            let got = capture_take().expect("no buffer");
            crate::os_boundary::seq_trace_disarm();
            assert!(
                got.contains("SEQ trace LIVE"),
                "the sequencer trace never went live: {:?}",
                got
            );
        });
    }
}
