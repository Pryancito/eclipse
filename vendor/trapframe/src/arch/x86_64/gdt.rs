//! Configure Global Descriptor Table (GDT)

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::asm;
use core::mem::size_of;

use x86_64::instructions::tables::{lgdt, load_tss};
use x86_64::registers::model_specific::{GsBase, Star};
use x86_64::structures::gdt::{Descriptor, SegmentSelector};
use x86_64::structures::DescriptorTablePointer;
use x86_64::{PrivilegeLevel, VirtAddr};

#[cfg(not(feature = "ioport_bitmap"))]
type TSS = x86_64::structures::tss::TaskStateSegment;
#[cfg(feature = "ioport_bitmap")]
type TSS = super::ioport::TSSWithPortBitmap;

/// The region pointed to by `GSBASE` on each CPU.
///
/// `GSBASE` (and the TSS descriptor) point at the `tss` field, which therefore
/// MUST stay first: the syscall/trap entry stubs read `gs:4` / `gs:12` as
/// offsets *into the TSS*. The trailing `cpu_local` word is appended storage for
/// a kernel per-CPU pointer (à la Redox's ProcessorControlRegion), read/written
/// via `gs:[offset_of!(.., cpu_local)]`. Reads through GSBASE are not bounded by
/// the TSS segment limit, so the extra word is always reachable.
#[repr(C)]
struct CpuLocalRegion {
    tss: TSS,
    cpu_local: usize,
    /// Dense logical CPU id (0..NCPU), valid once [`write_logical_cpu_id`] runs.
    logical_cpu_id: u8,
    logical_cpu_valid: u8,
    _pad: [u8; 6],
}

/// Read the kernel per-CPU pointer stored in this CPU's GS region.
///
/// Returns 0 if [`init`] has not run yet on this CPU.
#[inline]
pub fn read_cpu_local() -> usize {
    let ret: usize;
    unsafe {
        asm!(
            "mov {ret}, gs:[{off}]",
            ret = out(reg) ret,
            off = const core::mem::offset_of!(CpuLocalRegion, cpu_local),
            options(nostack, preserves_flags, readonly),
        );
    }
    ret
}

/// Store a kernel per-CPU pointer in this CPU's GS region.
///
/// # Safety
///
/// `GSBASE` must point to a [`CpuLocalRegion`], i.e. [`init`] must have run on
/// the current CPU.
#[inline]
pub unsafe fn write_cpu_local(val: usize) {
    asm!(
        "mov gs:[{off}], {val}",
        val = in(reg) val,
        off = const core::mem::offset_of!(CpuLocalRegion, cpu_local),
        options(nostack, preserves_flags),
    );
}

/// Whether this CPU's dense logical id has been stored in the GS region.
#[inline]
pub fn logical_cpu_id_valid() -> bool {
    let valid: u8;
    unsafe {
        asm!(
            "mov {valid}, gs:[{off}]",
            valid = out(reg_byte) valid,
            off = const core::mem::offset_of!(CpuLocalRegion, logical_cpu_valid),
            options(nostack, preserves_flags, readonly),
        );
    }
    valid != 0
}

/// Read the dense logical CPU id from the GS region.
///
/// # Panics
///
/// If [`logical_cpu_id_valid`] is false.
#[inline]
pub fn read_logical_cpu_id() -> u8 {
    let id: u8;
    unsafe {
        asm!(
            "mov {id}, gs:[{off}]",
            id = out(reg_byte) id,
            off = const core::mem::offset_of!(CpuLocalRegion, logical_cpu_id),
            options(nostack, preserves_flags, readonly),
        );
    }
    id
}

/// Store the dense logical CPU id for this CPU in the GS region.
///
/// # Safety
///
/// [`init`] must have run on the current CPU.
#[inline]
pub unsafe fn write_logical_cpu_id(id: u8) {
    asm!(
        "mov gs:[{id_off}], {id}",
        "mov byte ptr gs:[{valid_off}], 1",
        id = in(reg_byte) id,
        id_off = const core::mem::offset_of!(CpuLocalRegion, logical_cpu_id),
        valid_off = const core::mem::offset_of!(CpuLocalRegion, logical_cpu_valid),
        options(nostack, preserves_flags),
    );
}

/// Init TSS & GDT.
pub fn init() {
    // allocate stack for trap from user
    // set the stack top to TSS
    // so that when trap from ring3 to ring0, CPU can switch stack correctly
    let mut region = Box::new(CpuLocalRegion {
        tss: TSS::new(),
        cpu_local: 0,
        logical_cpu_id: 0,
        logical_cpu_valid: 0,
        _pad: [0; 6],
    });
    let trap_stack_top = Box::leak(Box::new([0u8; 0x1000])).as_ptr() as u64 + 0x1000;
    region.tss.privilege_stack_table[0] = VirtAddr::new(trap_stack_top);
    let region: &'static CpuLocalRegion = Box::leak(region);
    let tss: &'static TSS = &region.tss;
    let (tss0, tss1) = match Descriptor::tss_segment(tss) {
        Descriptor::SystemSegment(tss0, tss1) => (tss0, tss1),
        _ => unreachable!(),
    };
    // Extreme hack: the segment limit assumed by x86_64 does not include the port bitmap.
    #[cfg(feature = "ioport_bitmap")]
    let tss0 = (tss0 & !0xFFFF) | (size_of::<TSS>() as u64);

    unsafe {
        // get current GDT
        let gdtp = sgdt();
        let entry_count = (gdtp.limit + 1) as usize / size_of::<u64>();
        let old_gdt = core::slice::from_raw_parts(gdtp.base.as_ptr::<u64>(), entry_count);

        // allocate new GDT with 7 more entries
        //
        // NOTICE: for fast syscall:
        //   STAR[47:32] = K_CS   = K_SS - 8
        //   STAR[63:48] = U_CS32 = U_SS32 - 8 = U_CS - 16
        let mut gdt = Vec::from(old_gdt);
        gdt.extend([tss0, tss1, KCODE64, KDATA64, UCODE32, UDATA32, UCODE64].iter());
        let gdt = Vec::leak(gdt);

        // load new GDT and TSS
        lgdt(&DescriptorTablePointer {
            limit: gdt.len() as u16 * 8 - 1,
            base: VirtAddr::new(gdt.as_ptr() as _),
        });
        load_tss(SegmentSelector::new(
            entry_count as u16,
            PrivilegeLevel::Ring0,
        ));

        // for fast syscall:
        // store address of TSS to kernel_gsbase
        #[allow(const_item_mutation)]
        GsBase::MSR.write(tss as *const _ as u64);

        Star::write_raw(
            SegmentSelector::new(entry_count as u16 + 4, PrivilegeLevel::Ring3).0,
            SegmentSelector::new(entry_count as u16 + 2, PrivilegeLevel::Ring0).0,
        );
    }
}

/// Per-CPU setup for application processors (BSP must call [`init`] once first).
///
/// Adds this CPU's TSS to the global GDT and points `GSBASE` at its
/// [`CpuLocalRegion`]. Does not touch IDT or syscall MSRs (already configured on BSP).
pub fn init_ap() {
    let mut region = Box::new(CpuLocalRegion {
        tss: TSS::new(),
        cpu_local: 0,
        logical_cpu_id: 0,
        logical_cpu_valid: 0,
        _pad: [0; 6],
    });
    let trap_stack_top = Box::leak(Box::new([0u8; 0x1000])).as_ptr() as u64 + 0x1000;
    region.tss.privilege_stack_table[0] = VirtAddr::new(trap_stack_top);
    let region: &'static CpuLocalRegion = Box::leak(region);
    let tss: &'static TSS = &region.tss;
    let (tss0, tss1) = match Descriptor::tss_segment(tss) {
        Descriptor::SystemSegment(tss0, tss1) => (tss0, tss1),
        _ => unreachable!(),
    };
    #[cfg(feature = "ioport_bitmap")]
    let tss0 = (tss0 & !0xFFFF) | (size_of::<TSS>() as u64);

    use core::sync::atomic::{AtomicBool, Ordering};
    static GDT_EXTEND_LOCK: AtomicBool = AtomicBool::new(false);
    while GDT_EXTEND_LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }

    unsafe {
        let gdtp = sgdt();
        let entry_count = (gdtp.limit + 1) as usize / size_of::<u64>();
        let old_gdt = core::slice::from_raw_parts(gdtp.base.as_ptr::<u64>(), entry_count);
        let mut gdt = Vec::from(old_gdt);
        gdt.extend([tss0, tss1].iter());
        let gdt = Vec::leak(gdt);
        lgdt(&DescriptorTablePointer {
            limit: gdt.len() as u16 * 8 - 1,
            base: VirtAddr::new(gdt.as_ptr() as _),
        });
        load_tss(SegmentSelector::new(
            entry_count as u16,
            PrivilegeLevel::Ring0,
        ));
        #[allow(const_item_mutation)]
        GsBase::MSR.write(tss as *const _ as u64);
    }

    GDT_EXTEND_LOCK.store(false, Ordering::Release);
}

/// Get current GDT register
#[inline]
unsafe fn sgdt() -> DescriptorTablePointer {
    let mut gdt = DescriptorTablePointer {
        limit: 0,
        base: VirtAddr::zero(),
    };
    asm!("sgdt [{}]", in(reg) &mut gdt);
    gdt
}

const KCODE64: u64 = 0x00209800_00000000; // EXECUTABLE | USER_SEGMENT | PRESENT | LONG_MODE
const UCODE64: u64 = 0x0020F800_00000000; // EXECUTABLE | USER_SEGMENT | USER_MODE | PRESENT | LONG_MODE
const KDATA64: u64 = 0x00009200_00000000; // DATA_WRITABLE | USER_SEGMENT | PRESENT
#[allow(dead_code)]
const UDATA64: u64 = 0x0000F200_00000000; // DATA_WRITABLE | USER_SEGMENT | USER_MODE | PRESENT
const UCODE32: u64 = 0x00cffa00_0000ffff; // EXECUTABLE | USER_SEGMENT | USER_MODE | PRESENT
const UDATA32: u64 = 0x00cff200_0000ffff; // EXECUTABLE | USER_SEGMENT | USER_MODE | PRESENT
