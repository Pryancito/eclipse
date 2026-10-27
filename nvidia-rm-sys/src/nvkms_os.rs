//! The kernel half of nvidia-modeset's OS interface: what Linux's
//! `kernel-open/nvidia-modeset/nvidia-modeset-linux.c` gives nvkms out of the
//! kernel -- memory, time, semaphores, refcounted pointers, timers, the global
//! `nvkms_lock` and the way into the RM. PR 2 of `docs/README-nvkms-port.md`.
//!
//! The half that needs nothing from the kernel (module parameters, string and
//! printf helpers, the kapi open/ioctl glue) stays in C, in
//! `vendor/eclipse_nvkms_os.c`, next to the headers it is written against.
//!
//! Two things differ from Linux on purpose, both because this kernel has no
//! sleeping primitives at this layer (see the semaphores in `os_interface.rs`):
//!
//! * **Semaphores spin**, preemptible and with interrupts on, pumping TLB
//!   shootdowns like every other RM wait here.
//! * **Timers are serviced, not fired.** Linux runs each `nvkms_alloc_timer`
//!   callback from a kthread with `nvkms_lock` held. Here they wait in a queue
//!   until somebody calls [`service`], which runs the due ones under the same
//!   lock -- never from the timer interrupt, where taking `nvkms_lock` could
//!   deadlock against the thread that holds it on the same CPU. The driver
//!   that loads nvkms (PR 3) is the one that calls it.

#[cfg(target_os = "none")]
use crate::rm_init::RmGate;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use lock::Mutex;

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// `nvkms_alloc`: `kmalloc`/`vmalloc` in Linux, the RM's own allocator here
/// (`os_alloc_mem`), so nvkms and the RM share one accounting of the heap.
#[no_mangle]
pub extern "C" fn nvkms_alloc(size: usize, zero: u8) -> *mut c_void {
    let mut p: *mut c_void = core::ptr::null_mut();
    if crate::os_interface::os_alloc_mem(&mut p, size as u64) != crate::types::NV_OK {
        return core::ptr::null_mut();
    }
    if zero != 0 {
        unsafe { core::ptr::write_bytes(p as *mut u8, 0, size) };
    }
    p
}

#[no_mangle]
pub extern "C" fn nvkms_free(ptr: *mut c_void, _size: usize) {
    crate::os_interface::os_free_mem(ptr);
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Microseconds on the RM's own monotonic clock, so a deadline nvkms computes
/// and one the RM computes are on the same timeline.
#[no_mangle]
pub extern "C" fn nvkms_get_usec() -> u64 {
    crate::hooks::clock_ns() / 1_000
}

/// `usleep_range`/`mdelay` in Linux. A busy wait here, through the same path
/// as the RM's `osDelayUs`.
#[no_mangle]
pub extern "C" fn nvkms_usleep(usec: u64) {
    let mut left = usec;
    while left > 0 {
        let step = left.min(u32::MAX as u64) as u32;
        crate::hooks::delay_us_or_spin(step);
        left -= step as u64;
    }
}

#[no_mangle]
pub extern "C" fn nvkms_yield() {
    crate::os_interface::os_schedule();
}

// ---------------------------------------------------------------------------
// Semaphores, and nvkms_lock
// ---------------------------------------------------------------------------

/// A counting semaphore that spins to acquire. Linux's `struct semaphore`
/// sleeps; nothing here can.
pub struct Sema(AtomicU32);

impl Sema {
    pub const fn new(count: u32) -> Self {
        Self(AtomicU32::new(count))
    }

    pub fn down(&self) {
        let mut spins = 0u32;
        loop {
            let cur = self.0.load(Ordering::Acquire);
            if cur > 0
                && self
                    .0
                    .compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                return;
            }
            spins = spins.wrapping_add(1);
            if spins & 511 == 0 {
                lock::pump();
            }
            core::hint::spin_loop();
        }
    }

    /// `down_trylock`, inverted: `true` when it was taken.
    pub fn try_down(&self) -> bool {
        let cur = self.0.load(Ordering::Acquire);
        cur > 0
            && self
                .0
                .compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
    }

    pub fn up(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

/// `nvkms_lock` of nvidia-modeset-linux.c: every entry into nvkms -- open,
/// ioctl, close, a timer -- runs with it held, which is what makes nvkms
/// single-threaded. The C glue takes it through [`eclipse_nvkms_lock`].
static NVKMS_LOCK: Sema = Sema::new(1);

#[no_mangle]
pub extern "C" fn eclipse_nvkms_lock() {
    NVKMS_LOCK.down();
}

#[no_mangle]
pub extern "C" fn eclipse_nvkms_unlock() {
    NVKMS_LOCK.up();
}

#[no_mangle]
pub extern "C" fn nvkms_sema_alloc() -> *mut c_void {
    Box::into_raw(Box::new(Sema::new(1))) as *mut c_void
}

#[no_mangle]
pub extern "C" fn nvkms_sema_free(sema: *mut c_void) {
    if !sema.is_null() {
        drop(unsafe { Box::from_raw(sema as *mut Sema) });
    }
}

#[no_mangle]
pub extern "C" fn nvkms_sema_down(sema: *mut c_void) {
    unsafe { &*(sema as *const Sema) }.down();
}

#[no_mangle]
pub extern "C" fn nvkms_sema_up(sema: *mut c_void) {
    unsafe { &*(sema as *const Sema) }.up();
}

// ---------------------------------------------------------------------------
// Refcounted pointers
// ---------------------------------------------------------------------------

/// `struct nvkms_ref_ptr`: a pointer nvkms can clear when its object dies
/// while timers that captured it are still queued. Each queued timer holds a
/// reference; the timer finds the pointer cleared and does nothing.
pub struct RefPtr {
    refs: AtomicUsize,
    ptr: core::sync::atomic::AtomicPtr<c_void>,
}

#[no_mangle]
pub extern "C" fn nvkms_alloc_ref_ptr(ptr: *mut c_void) -> *mut RefPtr {
    Box::into_raw(Box::new(RefPtr {
        refs: AtomicUsize::new(1),
        ptr: core::sync::atomic::AtomicPtr::new(ptr),
    }))
}

/// The owner is done with the object: the pointer reads NULL from now on and
/// the owner's reference goes.
#[no_mangle]
pub extern "C" fn nvkms_free_ref_ptr(ref_ptr: *mut RefPtr) {
    if ref_ptr.is_null() {
        return;
    }
    unsafe { &*ref_ptr }
        .ptr
        .store(core::ptr::null_mut(), Ordering::Release);
    let _ = nvkms_dec_ref(ref_ptr);
}

#[no_mangle]
pub extern "C" fn nvkms_inc_ref(ref_ptr: *mut RefPtr) {
    unsafe { &*ref_ptr }.refs.fetch_add(1, Ordering::AcqRel);
}

/// Drops a reference and returns the pointer as it was before, NULL if the
/// owner freed it. The last reference frees the `RefPtr` itself.
#[no_mangle]
pub extern "C" fn nvkms_dec_ref(ref_ptr: *mut RefPtr) -> *mut c_void {
    let r = unsafe { &*ref_ptr };
    let ptr = r.ptr.load(Ordering::Acquire);
    if r.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
        drop(unsafe { Box::from_raw(ref_ptr) });
    }
    ptr
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// `nvkms_timer_proc_t`.
pub type TimerProc = extern "C" fn(data_ptr: *mut c_void, data_u32: u32);

/// What a queued timer hands its callback: a plain pointer, or a `RefPtr`
/// that may have been cleared since.
#[derive(Clone, Copy)]
enum TimerData {
    Ptr(usize),
    Ref(usize),
}

struct Timer {
    id: usize,
    deadline_us: u64,
    proc_: TimerProc,
    data: TimerData,
    data_u32: u32,
    /// Run under `nvkms_lock`: every nvkms timer. Kapi's event work is the
    /// exception (see [`eclipse_nvkms_schedule_work`]).
    locked: bool,
}

struct Timers {
    next_id: usize,
    queued: Vec<Timer>,
}

static TIMERS: Mutex<Timers> = Mutex::new(Timers {
    next_id: 1,
    queued: Vec::new(),
});

/// The handle nvkms keeps for a timer it may cancel: the queue entry's id,
/// never a pointer into the queue.
#[repr(C)]
pub struct TimerHandle {
    id: usize,
}

fn queue_timer(proc_: TimerProc, data: TimerData, data_u32: u32, usec: u64) -> usize {
    let deadline_us = nvkms_get_usec().saturating_add(usec);
    let mut t = TIMERS.lock();
    let id = t.next_id;
    t.next_id += 1;
    t.queued.push(Timer {
        id,
        deadline_us,
        proc_,
        data,
        data_u32,
        locked: true,
    });
    id
}

/// `nv_kthread_q_schedule_q_item` for kapi's event work: run `proc_(data)` at
/// the next [`service`], **without** `nvkms_lock` -- the work calls back into
/// nvkms through ioctls, which take the lock themselves. Scheduling work that
/// is already queued does nothing, as with a kthread queue item.
#[no_mangle]
pub extern "C" fn eclipse_nvkms_schedule_work(proc_: TimerProc, data: *mut c_void) {
    let now = nvkms_get_usec();
    let mut t = TIMERS.lock();
    let queued = t.queued.iter().any(|x| {
        !x.locked
            && x.proc_ as usize == proc_ as usize
            && matches!(x.data, TimerData::Ptr(p) if p == data as usize)
    });
    if queued {
        return;
    }
    let id = t.next_id;
    t.next_id += 1;
    t.queued.push(Timer {
        id,
        deadline_us: now,
        proc_,
        data: TimerData::Ptr(data as usize),
        data_u32: 0,
        locked: false,
    });
}

/// Drops the queued work for `data`, so it can be freed: what Linux gets from
/// flushing the kthread queue after `nvKmsClose`.
#[no_mangle]
pub extern "C" fn eclipse_nvkms_cancel_work(data: *mut c_void) {
    TIMERS
        .lock()
        .queued
        .retain(|x| x.locked || !matches!(x.data, TimerData::Ptr(p) if p == data as usize));
}

#[no_mangle]
pub extern "C" fn nvkms_alloc_timer(
    proc_: TimerProc,
    data_ptr: *mut c_void,
    data_u32: u32,
    usec: u64,
) -> *mut TimerHandle {
    let id = queue_timer(proc_, TimerData::Ptr(data_ptr as usize), data_u32, usec);
    Box::into_raw(Box::new(TimerHandle { id }))
}

/// A timer nobody cancels: it holds a reference on `ref_ptr` until it runs,
/// and runs with whatever the pointer reads then (NULL once freed). Linux
/// takes the reference the same way, in `nvkms_alloc_timer_with_ref_ptr`.
#[no_mangle]
pub extern "C" fn nvkms_alloc_timer_with_ref_ptr(
    proc_: TimerProc,
    ref_ptr: *mut RefPtr,
    data_u32: u32,
    usec: u64,
) -> u8 {
    nvkms_inc_ref(ref_ptr);
    queue_timer(proc_, TimerData::Ref(ref_ptr as usize), data_u32, usec);
    1
}

/// Cancels the timer if it has not run, and frees the handle either way --
/// `nvkms_free_timer` is also how nvkms releases a timer that already fired.
#[no_mangle]
pub extern "C" fn nvkms_free_timer(handle: *mut TimerHandle) {
    if handle.is_null() {
        return;
    }
    let handle = unsafe { Box::from_raw(handle) };
    TIMERS.lock().queued.retain(|t| t.id != handle.id);
}

/// Runs every timer due at `now_us`, oldest deadline first, each under
/// `nvkms_lock` as in Linux. Returns the next deadline still queued, for the
/// caller to come back at.
///
/// The queue lock is not held while a callback runs: callbacks queue and
/// cancel timers of their own.
pub fn service(now_us: u64) -> Option<u64> {
    loop {
        let due = {
            let mut t = TIMERS.lock();
            let next = t
                .queued
                .iter()
                .enumerate()
                .filter(|(_, x)| x.deadline_us <= now_us)
                .min_by_key(|(_, x)| (x.deadline_us, x.id))
                .map(|(i, _)| i);
            next.map(|i| t.queued.remove(i))
        };
        let Some(timer) = due else {
            break;
        };
        let data_ptr = match timer.data {
            TimerData::Ptr(p) => p as *mut c_void,
            TimerData::Ref(r) => nvkms_dec_ref(r as *mut RefPtr),
        };
        // A ref-ptr timer whose object is gone has nothing to run on.
        if matches!(timer.data, TimerData::Ref(_)) && data_ptr.is_null() {
            continue;
        }
        if timer.locked {
            NVKMS_LOCK.down();
            (timer.proc_)(data_ptr, timer.data_u32);
            NVKMS_LOCK.up();
        } else {
            (timer.proc_)(data_ptr, timer.data_u32);
        }
    }
    TIMERS.lock().queued.iter().map(|t| t.deadline_us).min()
}

/// [`service`] at the current time.
pub fn service_now() -> Option<u64> {
    service(nvkms_get_usec())
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

unsafe fn c_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    let bytes = core::ffi::CStr::from_ptr(p).to_bytes();
    core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
}

/// `NVKMS_LOG_LEVEL_{INFO,WARN,ERROR}` = 0/1/2.
#[no_mangle]
pub extern "C" fn nvkms_log(level: c_int, gpu_prefix: *const c_char, msg: *const c_char) {
    let (prefix, msg) = unsafe { (c_str(gpu_prefix), c_str(msg)) };
    match level {
        2 => log::error!("[nvkms] {}{}", prefix, msg),
        1 => log::warn!("[nvkms] {}{}", prefix, msg),
        _ => log::info!("[nvkms] {}{}", prefix, msg),
    }
}

// ---------------------------------------------------------------------------
// The RM
// ---------------------------------------------------------------------------

#[cfg(target_os = "none")]
extern "C" {
    fn eclipse_rm_kernel_rmapi_op(ops: *mut c_void);
    fn eclipse_rm_enumerate_gpus(gpu_info: *mut c_void) -> u32;
    fn eclipse_rm_gpu_id_attached(gpu_id: u32) -> u8;
}

/// `nvkms_call_rm`: one `nvidia_kernel_rmapi_ops_t` into the RM, through
/// `rm_kernel_rmapi_op`. Waits for the RM gate like any ioctl does: nvkms is
/// one more client of the RM, and the RM here takes one caller at a time.
#[no_mangle]
pub extern "C" fn nvkms_call_rm(ops: *mut c_void) {
    #[cfg(target_os = "none")]
    {
        let _gate = RmGate::lock();
        unsafe { eclipse_rm_kernel_rmapi_op(ops) };
    }
    #[cfg(not(target_os = "none"))]
    {
        // Hosted builds compile no RM and never load nvkms.
        let _ = ops;
    }
}

/// `nvkms_enumerate_gpus`: every GPU the RM has attached, with the RM's own
/// gpu id. `gpu_info` has room for `NV_MAX_GPUS` entries.
#[no_mangle]
pub extern "C" fn nvkms_enumerate_gpus(gpu_info: *mut c_void) -> u32 {
    #[cfg(target_os = "none")]
    {
        let _gate = RmGate::lock();
        unsafe { eclipse_rm_enumerate_gpus(gpu_info) }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = gpu_info;
        0
    }
}

/// `nvkms_open_gpu`. In Linux the first open brings the GPU up
/// (`nvidia_dev_get`); here the driver has already attached it during probe,
/// so opening one only has to find it.
#[no_mangle]
pub extern "C" fn nvkms_open_gpu(gpu_id: u32) -> u8 {
    #[cfg(target_os = "none")]
    {
        let _gate = RmGate::lock();
        unsafe { eclipse_rm_gpu_id_attached(gpu_id) }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = gpu_id;
        0
    }
}

/// The GPU stays attached for the life of the kernel, as it was before nvkms.
#[no_mangle]
pub extern "C" fn nvkms_close_gpu(_gpu_id: u32) {}

/// One entry of `nv_gpu_info_t` (`nv-gpu-info.h`), the table
/// [`nvkms_enumerate_gpus`] fills.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GpuInfo {
    pub gpu_id: u32,
    pub domain: u32,
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub needs_numa_setup: u8,
    pub os_device_ptr: *mut c_void,
}

/// `NV_MAX_GPUS`: the room `nvkms_enumerate_gpus` is given.
pub const MAX_GPUS: usize = 32;

/// The RM's gpu id for the GPU at `domain:bus:slot`, which is what kapi's
/// `allocateDevice` takes. `None` if the RM has not attached it.
pub fn gpu_id_at(domain: u32, bus: u8, slot: u8) -> Option<u32> {
    let mut table = [GpuInfo {
        gpu_id: 0,
        domain: 0,
        bus: 0,
        slot: 0,
        function: 0,
        needs_numa_setup: 0,
        os_device_ptr: core::ptr::null_mut(),
    }; MAX_GPUS];
    let n = nvkms_enumerate_gpus(table.as_mut_ptr() as *mut c_void) as usize;
    find_gpu_id(&table[..n.min(MAX_GPUS)], domain, bus, slot)
}

fn find_gpu_id(table: &[GpuInfo], domain: u32, bus: u8, slot: u8) -> Option<u32> {
    table
        .iter()
        .find(|g| g.domain == domain && g.bus == bus && g.slot == slot)
        .map(|g| g.gpu_id)
}

// ---------------------------------------------------------------------------
// The boot console
// ---------------------------------------------------------------------------

/// The firmware (UEFI GOP) framebuffer: Linux's `screen_info`. The RM's
/// `NV0080_CTRL_CMD_OS_UNIX_VT_GET_FB_INFO` answers with it on the GPU whose
/// BAR1 it starts at (`vendor/eclipse_nvkms_rm.c`), and nvkms imports that
/// memory as the console surface it can scan out and restore.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootConsole {
    pub phys: u64,
    pub width: u32,
    pub height: u32,
    /// Bits per pixel, as `screen_info.lfb_depth`.
    pub depth: u32,
    pub pitch: u32,
}

static BOOT_CONSOLE: Mutex<Option<BootConsole>> = Mutex::new(None);

/// Called once at boot by the driver, with what the bootloader handed over.
pub fn set_boot_console(console: BootConsole) {
    *BOOT_CONSOLE.lock() = Some(console);
}

#[no_mangle]
pub extern "C" fn eclipse_boot_console(out: *mut BootConsole) -> u8 {
    match *BOOT_CONSOLE.lock() {
        Some(c) if c.phys != 0 && !out.is_null() => {
            unsafe { out.write(c) };
            1
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Loading nvkms
// ---------------------------------------------------------------------------

#[cfg(target_os = "none")]
extern "C" {
    fn nvKmsModuleLoad() -> u8;
}

/// What the one `nvKmsModuleLoad` answered: 0 before it has run, then
/// [`LOADED`] or [`LOAD_FAILED`]. Read and written under `nvkms_lock`, which
/// is also what serialises the load against anything else entering nvkms --
/// not a `lock::Mutex`, which would keep interrupts off for the whole load.
static MODULE_STATE: AtomicU32 = AtomicU32::new(0);
const LOADED: u32 = 1;
const LOAD_FAILED: u32 = 2;

/// `nvkms_init` of nvidia-modeset-linux.c, after its kthreads and chrdev:
/// `nvKmsModuleLoad` under `nvkms_lock`, once per boot. It allocates nvkms's
/// own RM client, so the RM must be up. Later calls answer the first one.
pub fn module_load() -> bool {
    NVKMS_LOCK.down();
    let mut state = MODULE_STATE.load(Ordering::Relaxed);
    if state == 0 {
        #[cfg(target_os = "none")]
        let ok = unsafe { nvKmsModuleLoad() } != 0;
        // Hosted builds compile no nvkms.
        #[cfg(not(target_os = "none"))]
        let ok = false;
        state = if ok { LOADED } else { LOAD_FAILED };
        MODULE_STATE.store(state, Ordering::Relaxed);
    }
    NVKMS_LOCK.up();
    state == LOADED
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use alloc::vec;
    use std::sync::Mutex as StdMutex;

    /// The timer queue is a process global; tests that look at it take turns.
    static TURN: StdMutex<()> = StdMutex::new(());

    static RAN: StdMutex<Vec<(usize, u32)>> = StdMutex::new(Vec::new());

    extern "C" fn record(p: *mut c_void, v: u32) {
        RAN.lock().unwrap().push((p as usize, v));
    }

    fn reset() {
        TIMERS.lock().queued.clear();
        RAN.lock().unwrap().clear();
    }

    #[test]
    fn a_timer_runs_once_when_due_and_not_before() {
        let _t = TURN.lock().unwrap();
        reset();
        let now = nvkms_get_usec();
        let h = nvkms_alloc_timer(record, 0x10 as *mut c_void, 7, 1_000_000);
        assert!(
            service(now).is_some(),
            "the queued timer's deadline went missing"
        );
        assert!(RAN.lock().unwrap().is_empty(), "ran before its deadline");
        assert_eq!(service(u64::MAX), None);
        assert_eq!(*RAN.lock().unwrap(), vec![(0x10, 7)]);
        assert_eq!(service(u64::MAX), None);
        assert_eq!(RAN.lock().unwrap().len(), 1, "ran twice");
        nvkms_free_timer(h);
    }

    #[test]
    fn a_cancelled_timer_never_runs() {
        let _t = TURN.lock().unwrap();
        reset();
        let h = nvkms_alloc_timer(record, core::ptr::null_mut(), 1, 0);
        nvkms_free_timer(h);
        service(u64::MAX);
        assert!(RAN.lock().unwrap().is_empty());
    }

    #[test]
    fn due_timers_run_in_deadline_order() {
        let _t = TURN.lock().unwrap();
        reset();
        let a = nvkms_alloc_timer(record, core::ptr::null_mut(), 2, 20);
        let b = nvkms_alloc_timer(record, core::ptr::null_mut(), 1, 10);
        service(u64::MAX);
        let order: Vec<u32> = RAN.lock().unwrap().iter().map(|r| r.1).collect();
        assert_eq!(order, vec![1, 2]);
        nvkms_free_timer(a);
        nvkms_free_timer(b);
    }

    #[test]
    fn a_ref_ptr_timer_sees_its_object_or_skips() {
        let _t = TURN.lock().unwrap();
        reset();
        let live = nvkms_alloc_ref_ptr(0x20 as *mut c_void);
        let dead = nvkms_alloc_ref_ptr(0x30 as *mut c_void);
        nvkms_alloc_timer_with_ref_ptr(record, live, 1, 0);
        nvkms_alloc_timer_with_ref_ptr(record, dead, 2, 0);
        nvkms_free_ref_ptr(dead);
        service(u64::MAX);
        assert_eq!(*RAN.lock().unwrap(), vec![(0x20, 1)]);
        // The owner's reference is the last one now.
        nvkms_free_ref_ptr(live);
    }

    #[test]
    fn dec_ref_reports_the_pointer_until_it_is_freed() {
        let r = nvkms_alloc_ref_ptr(0x40 as *mut c_void);
        nvkms_inc_ref(r);
        assert_eq!(nvkms_dec_ref(r) as usize, 0x40);
        nvkms_inc_ref(r);
        nvkms_free_ref_ptr(r);
        assert!(nvkms_dec_ref(r).is_null(), "freed, but still handed out");
    }

    #[test]
    fn work_is_queued_once_and_cancellable() {
        let _t = TURN.lock().unwrap();
        reset();
        eclipse_nvkms_schedule_work(record, 0x50 as *mut c_void);
        eclipse_nvkms_schedule_work(record, 0x50 as *mut c_void);
        eclipse_nvkms_schedule_work(record, 0x60 as *mut c_void);
        eclipse_nvkms_cancel_work(0x60 as *mut c_void);
        service(u64::MAX);
        assert_eq!(*RAN.lock().unwrap(), vec![(0x50, 0)]);
    }

    /// Work runs without `nvkms_lock`; a timer runs with it.
    #[test]
    fn work_runs_unlocked_and_timers_locked() {
        static HELD: StdMutex<Vec<bool>> = StdMutex::new(Vec::new());
        extern "C" fn probe(_: *mut c_void, _: u32) {
            let free = NVKMS_LOCK.try_down();
            if free {
                NVKMS_LOCK.up();
            }
            HELD.lock().unwrap().push(!free);
        }
        let _t = TURN.lock().unwrap();
        reset();
        eclipse_nvkms_schedule_work(probe, core::ptr::null_mut());
        service(u64::MAX);
        let h = nvkms_alloc_timer(probe, core::ptr::null_mut(), 0, 0);
        service(u64::MAX);
        nvkms_free_timer(h);
        assert_eq!(*HELD.lock().unwrap(), vec![false, true]);
    }

    #[test]
    fn a_semaphore_counts() {
        let s = Sema::new(1);
        assert!(s.try_down());
        assert!(!s.try_down(), "taken twice");
        s.up();
        s.down();
        s.up();
    }
    fn gpu(gpu_id: u32, bus: u8, slot: u8) -> GpuInfo {
        GpuInfo {
            gpu_id,
            domain: 0,
            bus,
            slot,
            function: 0,
            needs_numa_setup: 0,
            os_device_ptr: core::ptr::null_mut(),
        }
    }

    #[test]
    fn gpu_info_is_laid_out_like_nv_gpu_info_t() {
        // u32 gpu_id, {u32 domain; u8 bus, slot, function}, NvBool, void *.
        assert_eq!(core::mem::size_of::<GpuInfo>(), 24);
        assert_eq!(core::mem::offset_of!(GpuInfo, bus), 8);
        assert_eq!(core::mem::offset_of!(GpuInfo, os_device_ptr), 16);
    }

    #[test]
    fn each_gpu_is_found_by_its_own_pci_address() {
        let table = [gpu(0x100, 0x01, 0), gpu(0x6500, 0x65, 0)];
        assert_eq!(find_gpu_id(&table, 0, 0x65, 0), Some(0x6500));
        assert_eq!(find_gpu_id(&table, 0, 0x01, 0), Some(0x100));
        assert_eq!(find_gpu_id(&table, 0, 0x02, 0), None);
        assert_eq!(find_gpu_id(&table, 1, 0x01, 0), None, "other domain");
    }

    #[test]
    fn the_boot_console_is_handed_to_the_rm_once_registered() {
        let mut out = BootConsole::default();
        let console = BootConsole {
            phys: 0xe000_0000,
            width: 1920,
            height: 1080,
            depth: 32,
            pitch: 7680,
        };
        set_boot_console(console);
        assert_eq!(eclipse_boot_console(&mut out), 1);
        assert_eq!(out, console);
        assert_eq!(eclipse_boot_console(core::ptr::null_mut()), 0);
        set_boot_console(BootConsole::default());
        assert_eq!(eclipse_boot_console(&mut out), 0, "no framebuffer at 0");
    }

    #[test]
    fn a_hosted_build_never_loads_nvkms_and_says_so_every_time() {
        assert!(!module_load());
        assert!(!module_load());
    }
}
