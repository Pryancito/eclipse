use alloc::boxed::Box;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::idt::*;
use x86_64::structures::DescriptorTablePointer;
use x86_64::{PrivilegeLevel, VirtAddr};

include!(concat!(env!("OUT_DIR"), "/vectors.rs"));

/// Pointer to the shared IDT allocated by the BSP (stored as raw address).
/// APs must call `lidt` with this same IDT — they cannot reuse the BSP's IDTR
/// because it lives in a per-CPU register, and after the trampoline each AP's
/// IDTR is at the reset-default (base = 0, limit = 0), making any interrupt or
/// exception cause an immediate triple-fault.
static IDT_PTR: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let idt = Box::leak(Box::new(InterruptDescriptorTable::new()));
    let entries: &'static mut [Entry<HandlerFunc>; 256] =
        unsafe { core::mem::transmute_copy(&idt) };
    for i in 0..256 {
        let opt = entries[i].set_handler_fn(unsafe { core::mem::transmute(VECTORS[i]) });
        // Enable user space `int3` and `into`
        if i == 3 || i == 4 {
            opt.set_privilege_level(PrivilegeLevel::Ring3);
        }
    }
    idt.load();
    // Store pointer for APs.
    IDT_PTR.store(idt as *const _ as u64, Ordering::Release);
}

/// Load the BSP's IDT on this AP.
///
/// Must be called on every application processor before interrupts are
/// enabled.  The IDT is a read-only structure shared by all CPUs; only the
/// `IDTR` register is per-CPU, so each CPU must call `lidt` independently.
pub fn init_ap() {
    let ptr = IDT_PTR.load(Ordering::Acquire);
    assert!(ptr != 0, "IDT not initialized by BSP before init_ap");
    unsafe {
        let idt = &*(ptr as *const InterruptDescriptorTable);
        idt.load();
    }
}

/// Get current IDT register
#[allow(dead_code)]
#[inline]
fn sidt() -> DescriptorTablePointer {
    let mut dtp = DescriptorTablePointer {
        limit: 0,
        base: VirtAddr::zero(),
    };
    unsafe {
        asm!("sidt [{}]", in(reg) &mut dtp);
    }
    dtp
}
