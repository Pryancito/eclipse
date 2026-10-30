use x2apic::lapic::{
    xapic_base, IpiAllShorthand, LocalApic as LocalApicInner, LocalApicBuilder, TimerDivide,
    TimerMode,
};

use super::{consts, Phys2VirtFn};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

// APIC MMIO addresses are CPU-local, but the driver's mutable configuration
// must also be private to each CPU.
//
// Slots are indexed by the kernel's DENSE logical cpu id (`lock::current_cpu_id`,
// read from this CPU's GS-based per-CPU block), not by the CPUID initial APIC
// id the upstream driver used. That lookup ran `CPUID` twice on EVERY
// interrupt (the EOI path goes through `get()`): a serialising instruction
// on bare metal and a VM exit per IRQ under a hypervisor. It also aliased
// CPUs whose x2APIC ids differ in more than the low 8 bits. The logical id
// is registered for the BSP and for each AP before their Local APIC is
// initialised (kernel-hal `boot.rs`), so it is valid at every access here.
static mut LOCAL_APICS: [Option<LocalApic>; lock::MAX_CORE_NUM] =
    [const { None }; lock::MAX_CORE_NUM];
static APIC_BASE: AtomicUsize = AtomicUsize::new(0);
static BSP_ID: AtomicU8 = AtomicU8::new(0);

/// The mode every Local APIC ends up in once its slot exists, published by the
/// BSP. `LocalApicBuilder::build` picks it from CPUID, which is the same on
/// every core, so an AP that has been through `init_ap` agrees with it.
static X2APIC_MODE: AtomicBool = AtomicBool::new(false);

/// (x2APIC MSR, xAPIC MMIO offset) of the two registers written on every
/// interrupt. Same values as the private constants in `x2apic::lapic`.
const REG_EOI: (u32, u32) = (0x80B, 0x0B0);
const REG_TICR: (u32, u32) = (0x838, 0x380);

// A copy of each slot as it was last left by this driver, and a count of the
// times a slot was found to differ from it.
//
// A real machine died with `#GP` on the `wrmsr` of `set_timer_initial` on
// CPU6, 375 s after boot, with `ecx = 0xffffff00`: the TICR register in that
// CPU's slot no longer said MSR 0x838. Nothing in this driver writes a
// register descriptor after `init_*`, so something else wrote over this
// static. The two registers written on every interrupt (EOI and the timer
// count) no longer go through the slot at all, see `write_reg`. The copy is
// for the rest of it and for finding the writer: the tick compares the slot
// against it, logs what changed and where, and puts it back.
const SLOT_WORDS: usize = core::mem::size_of::<LocalApic>() / 8;
static mut SLOT_SHADOWS: [[u64; SLOT_WORDS]; lock::MAX_CORE_NUM] =
    [[0; SLOT_WORDS]; lock::MAX_CORE_NUM];
static SLOT_CORRUPTIONS: AtomicUsize = AtomicUsize::new(0);
/// Lines `check_slot` may log in all; the counter keeps counting past it.
const SLOT_CORRUPTION_LOG_BUDGET: usize = 8;

/// Write one Local APIC register of the calling CPU without reading anything
/// from its slot. Only after this CPU's `init_*` (the callers reach it through
/// `get()`, which refuses an empty slot), so the mode is the published one.
unsafe fn write_reg(reg: (u32, u32), value: u32) {
    unsafe {
        if X2APIC_MODE.load(Ordering::Relaxed) {
            x86_64::registers::model_specific::Msr::new(reg.0).write(value as u64);
        } else {
            let base = APIC_BASE.load(Ordering::Relaxed);
            core::ptr::write_volatile((base + reg.1 as usize) as *mut u32, value);
        }
    }
}

/// Index of this CPU's slot in [`LOCAL_APICS`].
fn slot_index() -> usize {
    let id = lock::current_cpu_id() as usize;
    assert!(
        id < lock::MAX_CORE_NUM,
        "logical cpu id {} exceeds MAX_CORE_NUM {}",
        id,
        lock::MAX_CORE_NUM
    );
    id
}

/// `IA32_APIC_BASE` bit 10: the Local APIC is in x2APIC mode.
///
/// [`LocalApicBuilder::build`] selects x2APIC whenever the CPU *supports* it
/// (`CPUID.01H:ECX[21]`) — which is every x86 since roughly 2008 — and
/// `enable()` then sets this bit. Once it is set the LAPIC no longer decodes
/// its MMIO page and every register moves to the MSR interface, which changes
/// both the ICR destination encoding and the layout of the ID register.
/// QEMU's default TCG CPU does not advertise x2APIC, so emulated boots stay on
/// the xAPIC path and never exercise the difference.
fn x2apic_active() -> bool {
    const IA32_APIC_BASE: u32 = 0x1B;
    const EXTD: u64 = 1 << 10;
    let apic_base = unsafe { x86_64::registers::model_specific::Msr::new(IA32_APIC_BASE).read() };
    apic_base & EXTD != 0
}

pub struct LocalApic {
    inner: LocalApicInner,
}

impl LocalApic {
    pub fn is_initialized() -> bool {
        APIC_BASE.load(Ordering::Acquire) != 0
    }

    pub unsafe fn get<'a>() -> &'a mut LocalApic {
        unsafe {
            let local_apic = (&raw mut LOCAL_APICS)
                .cast::<Option<LocalApic>>()
                .add(slot_index());
            (*local_apic)
                .as_mut()
                .expect("Local APIC is not initialized for this CPU")
        }
    }

    pub unsafe fn init_bsp(phys_to_virt: Phys2VirtFn) {
        unsafe {
            let base_vaddr = phys_to_virt(xapic_base() as usize);
            // Publish `APIC_BASE` (which is what `is_initialized()` reports)
            // only once the BSP's Local APIC object actually exists below.
            // Storing it first meant a failed `build()` left the driver
            // claiming to be initialised with an empty BSP slot, so the very
            // next interrupt's EOI hit `get()`'s expect -- a panic behind the
            // "continuing without LAPIC" log.
            let mut inner = match LocalApicBuilder::new()
                .timer_vector(consts::X86_INT_APIC_TIMER)
                .error_vector(consts::X86_INT_APIC_ERROR)
                .spurious_vector(consts::X86_INT_APIC_SPURIOUS)
                .set_xapic_base(base_vaddr as u64)
                .build()
            {
                Ok(lapic) => lapic,
                Err(e) => {
                    crate::klog_err!(
                        "[lapic] LocalApicBuilder::build() failed: {} — continuing without LAPIC",
                        e
                    );
                    return;
                }
            };
            inner.enable();

            if !inner.is_bsp() {
                crate::klog_warn!(
                    "[lapic] init_bsp() on non-BSP core (id={:#x}); APIC routing may be incorrect",
                    Self::decode_id(inner.id())
                );
            }
            let bsp_id = Self::decode_id(inner.id());
            crate::klog_info!(
                "[lapic] BSP APIC id {:#x}, mode {}",
                bsp_id,
                if x2apic_active() { "x2APIC" } else { "xAPIC" }
            );
            BSP_ID.store(bsp_id as u8, Ordering::Release);
            let slot = (&raw mut LOCAL_APICS)
                .cast::<Option<LocalApic>>()
                .add(slot_index());
            slot.write(Some(LocalApic { inner }));
            X2APIC_MODE.store(x2apic_active(), Ordering::Release);
            APIC_BASE.store(base_vaddr, Ordering::Release);
            Self::get().snapshot();
        }
    }

    pub unsafe fn init_ap() {
        unsafe {
            let base_vaddr = APIC_BASE.load(Ordering::Acquire);
            let mut inner = match LocalApicBuilder::new()
                .timer_vector(consts::X86_INT_APIC_TIMER)
                .error_vector(consts::X86_INT_APIC_ERROR)
                .spurious_vector(consts::X86_INT_APIC_SPURIOUS)
                .set_xapic_base(base_vaddr as u64)
                .build()
            {
                Ok(lapic) => lapic,
                Err(e) => {
                    crate::klog_err!(
                        "[lapic] LocalApicBuilder::build() failed: {} — continuing without LAPIC",
                        e
                    );
                    return;
                }
            };
            inner.enable();
            let slot = (&raw mut LOCAL_APICS)
                .cast::<Option<LocalApic>>()
                .add(slot_index());
            slot.write(Some(LocalApic { inner }));
            Self::get().snapshot();
        }
    }

    fn words(&self) -> *const u64 {
        (self as *const Self).cast()
    }

    /// Record this slot as the driver left it. After `init_*` and after every
    /// setter that changes a field of the inner object.
    fn snapshot(&mut self) {
        let idx = slot_index();
        unsafe {
            let shadow = &mut *(&raw mut SLOT_SHADOWS).cast::<[u64; SLOT_WORDS]>().add(idx);
            for (i, w) in shadow.iter_mut().enumerate() {
                *w = core::ptr::read_volatile(self.words().add(i));
            }
        }
    }

    /// Compare this slot with its copy; on a difference, log it and restore
    /// the copy. Runs from the tick, which is interrupt context or under an
    /// IRQ-disabling lock (see kernel-hal `arm_deadline`), so nothing on this
    /// CPU is half way through a setter.
    fn check_slot(&mut self) {
        let idx = slot_index();
        let words = (self as *mut Self).cast::<u64>();
        unsafe {
            let shadow = &*(&raw const SLOT_SHADOWS)
                .cast::<[u64; SLOT_WORDS]>()
                .add(idx);
            let mut first = None;
            let mut last = 0;
            for (i, &want) in shadow.iter().enumerate() {
                if core::ptr::read_volatile(words.add(i)) != want {
                    first.get_or_insert(i);
                    last = i;
                }
            }
            let Some(first) = first else {
                return;
            };
            let found = core::ptr::read_volatile(words.add(first));
            let n = SLOT_CORRUPTIONS.fetch_add(1, Ordering::Relaxed);
            for (i, &want) in shadow.iter().enumerate() {
                core::ptr::write_volatile(words.add(i), want);
            }
            if n < SLOT_CORRUPTION_LOG_BUDGET {
                crate::klog_err!(
                    "[lapic] cpu{} slot overwritten at {:p} +{:#x}..+{:#x}: \
                     {:#018x} where {:#018x} was; restored",
                    idx,
                    words,
                    first * 8,
                    last * 8 + 8,
                    found,
                    shadow[first],
                );
            }
        }
    }

    fn decode_id(raw: u32) -> u32 {
        if x2apic_active() {
            raw
        } else {
            raw >> 24
        }
    }

    pub fn bsp_id() -> u8 {
        BSP_ID.load(Ordering::Acquire)
    }

    pub fn id(&mut self) -> u32 {
        unsafe { Self::decode_id(self.inner.id()) }
    }

    fn icr_dest(dest: u32) -> u32 {
        if x2apic_active() {
            dest
        } else {
            (dest & 0xFF) << 24
        }
    }

    pub fn send_init_ipi(&mut self, dest: u32) {
        unsafe { self.inner.send_init_ipi(Self::icr_dest(dest)) }
    }

    pub fn send_sipi(&mut self, vector: u8, dest: u32) {
        unsafe { self.inner.send_sipi(vector, Self::icr_dest(dest)) }
    }

    pub fn send_ipi_to(&mut self, vector: u8, dest: u32) {
        unsafe { self.inner.send_ipi(vector, Self::icr_dest(dest)) }
    }

    pub fn send_nmi_all_others(&mut self) {
        unsafe { self.inner.send_nmi_all(IpiAllShorthand::AllExcludingSelf) }
    }

    pub fn eoi(&mut self) {
        unsafe { write_reg(REG_EOI, 0) }
    }

    pub fn disable_timer(&mut self) {
        unsafe { self.inner.disable_timer() }
    }

    pub fn enable_timer(&mut self) {
        unsafe { self.inner.enable_timer() }
    }

    pub fn set_timer_mode(&mut self, mode: TimerMode) {
        unsafe { self.inner.set_timer_mode(mode) }
        self.snapshot();
    }

    pub fn set_timer_divide(&mut self, divide: TimerDivide) {
        unsafe { self.inner.set_timer_divide(divide) }
        self.snapshot();
    }

    /// The timer's initial count. Written straight to the register: the
    /// inner object keeps its own copy only for `enable()`, which has run by
    /// now. Every tick comes through here, so it is also where the slot is
    /// checked.
    pub fn set_timer_initial(&mut self, initial: u32) {
        self.check_slot();
        unsafe { write_reg(REG_TICR, initial) }
    }

    /// The timer's current count (TMCCT): how far the countdown loaded by
    /// [`set_timer_initial`](Self::set_timer_initial) has got. Read against
    /// the TSC at boot to measure the rate the timer actually counts at.
    pub fn timer_current(&self) -> u32 {
        unsafe { self.inner.timer_current() }
    }
}
