use x2apic::lapic::{
    xapic_base, IpiAllShorthand, LocalApic as LocalApicInner, LocalApicBuilder, TimerDivide,
    TimerMode,
};

use super::{consts, Phys2VirtFn};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

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
            APIC_BASE.store(base_vaddr, Ordering::Release);
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
        unsafe { self.inner.end_of_interrupt() }
    }

    pub fn disable_timer(&mut self) {
        unsafe { self.inner.disable_timer() }
    }

    pub fn enable_timer(&mut self) {
        unsafe { self.inner.enable_timer() }
    }

    pub fn set_timer_mode(&mut self, mode: TimerMode) {
        unsafe { self.inner.set_timer_mode(mode) }
    }

    pub fn set_timer_divide(&mut self, divide: TimerDivide) {
        unsafe { self.inner.set_timer_divide(divide) }
    }

    pub fn set_timer_initial(&mut self, initial: u32) {
        unsafe { self.inner.set_timer_initial(initial) }
    }
}
