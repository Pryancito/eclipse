//! Configure Global Descriptor Table (GDT)

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use x86_64::instructions::tables::{lgdt, load_tss};
use x86_64::registers::control::{Cr4, Cr4Flags};
use x86_64::registers::model_specific::{GsBase, KernelGsBase, Star};
use x86_64::structures::gdt::{Descriptor, SegmentSelector};
use x86_64::structures::DescriptorTablePointer;
use x86_64::{PrivilegeLevel, VirtAddr};

/// Base address and entry count of the GDT created by the BSP's [`init`].
/// APs read this instead of their own `sgdt()` (which points at the trampoline's
/// 3-entry mini-GDT and is therefore missing all user-space segments).
static BSP_GDT_BASE: AtomicU64 = AtomicU64::new(0);
static BSP_GDT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Index of the first of this GDT's three 32-bit TLS descriptors, and of
/// `KCODE64`, as [`init`] laid them out. Both are the same on every CPU: an AP
/// copies the BSP's whole table and appends only its own TSS, so everything the
/// BSP placed keeps its index. That is the whole reason the TLS slots are the
/// BSP's and not each CPU's own appendix --- a selector is a number ring 3
/// holds on to, and it has to name the same descriptor on whichever CPU the
/// thread wakes up on.
static BSP_TLS_INDEX: AtomicUsize = AtomicUsize::new(0);
static BSP_KCODE64_INDEX: AtomicUsize = AtomicUsize::new(0);

/// How many TLS descriptors a 32-bit thread gets, and the first `entry_number`
/// `set_thread_area(2)` hands out. Linux's are `GDT_ENTRY_TLS_MIN..=MAX`,
/// three of them, and its `entry_number` is the GDT index itself; here the
/// index depends on how many descriptors the firmware left in the table, so
/// the numbers ring 3 sees are 0, 1 and 2 and [`compat_tls_selector`] turns
/// one into a selector.
pub const COMPAT_TLS_ENTRIES: usize = 3;

/// The `STAR` MSR value written by the BSP.  `STAR` is a per-CPU MSR, so each
/// AP must write it independently with the same value.
/// Packed as `(u_cs_raw << 16) | k_cs_raw` into a single AtomicU32.
static BSP_STAR: AtomicU32 = AtomicU32::new(0);

#[cfg(not(feature = "ioport_bitmap"))]
#[allow(clippy::upper_case_acronyms)]
type TSS = x86_64::structures::tss::TaskStateSegment;
#[cfg(feature = "ioport_bitmap")]
#[allow(clippy::upper_case_acronyms)]
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
    /// Non-zero once this CPU runs with `CR4.FSGSBASE` set, i.e. once ring 3
    /// can move its own `FS`/`GS` base without going through the kernel. The
    /// entry stubs read it to decide whether the two base slots in the saved
    /// context still hold what `syscall_return` loaded. See `syscall.S`.
    user_fsgsbase: u8,
    _pad: [u8; 5],
    /// DEBUG: dirección base donde `trap_syscall_entry` guardó el último
    /// `GeneralRegs` en ESTA CPU (escrita por el asm vía `gs:`).
    dbg_save: usize,
    /// The user `FSBASE` this CPU's MSR holds, as `syscall_return` last wrote
    /// it; `UNKNOWN_BASE` until the first write. See `syscall.S`.
    user_fsbase: u64,
    /// The same for the user `GSBASE`, which sits in `KERNEL_GS_BASE` while
    /// the CPU is in the kernel.
    user_gsbase: u64,
}

/// A base no user context can hold -- it is not canonical, and
/// `refuse_noncanonical_entry` turns such a context away before
/// `syscall_return` -- so the first return on a CPU always writes both MSRs.
const UNKNOWN_BASE: u64 = u64::MAX;

/// Offsets of the base caches in [`CpuLocalRegion`], for the asm.
pub const USER_FSBASE_GS_OFFSET: usize = core::mem::offset_of!(CpuLocalRegion, user_fsbase);
pub const USER_GSBASE_GS_OFFSET: usize = core::mem::offset_of!(CpuLocalRegion, user_gsbase);
/// Offset of [`CpuLocalRegion::user_fsgsbase`], for the asm.
pub const USER_FSGSBASE_GS_OFFSET: usize = core::mem::offset_of!(CpuLocalRegion, user_fsgsbase);

/// Let ring 3 move its own `FS`/`GS` base, as Linux does on a CPU that has the
/// feature. Per-CPU: `CR4` is per-CPU, so every AP calls this for itself.
///
/// The two halves must not be separated. `CR4.FSGSBASE` alone stops
/// `RDFSBASE`/`WRFSBASE` raising `#UD` in ring 3 -- which is what a userspace
/// that read `CPUID.(EAX=7,ECX=0):EBX[0]` already expects, and the kernel does
/// not intercept `CPUID`, so on a CPU that reports the feature the choice is
/// between honouring it and lying by omission -- but it also breaks the
/// invariant the entry stubs were written on: that nothing but a syscall
/// writing the saved context can move a base, so the context's two base slots
/// can be left alone on the way in. A thread that called `WRFSBASE` would come
/// back from its next trap with whatever base the kernel last loaded, i.e.
/// silently without its TLS pointer. The per-CPU flag that tells the stubs to
/// read the live bases back is therefore set here, in the same call.
///
/// Returns false and changes nothing when the CPU does not report the feature.
pub unsafe fn enable_user_fsgsbase() -> bool {
    if !raw_cpuid::CpuId::new()
        .get_extended_feature_info()
        .is_some_and(|info| info.has_fsgsbase())
    {
        return false;
    }
    // `CR4` first, then the flag, and the order matters in exactly one
    // direction: the flag is what makes the entry stub execute `RDFSBASE`, and
    // that instruction raises `#UD` while `CR4.FSGSBASE` is clear -- in ring 0,
    // on the entry path, with no way to report it. The reverse window is
    // vacuous: this runs during per-CPU bring-up, before this CPU has ever
    // entered ring 3, so there is no user base yet that could have moved and
    // nothing for the stub to miss.
    Cr4::update(|f| f.insert(Cr4Flags::FSGSBASE));
    asm!(
        "mov byte ptr gs:[{off}], 1",
        off = const core::mem::offset_of!(CpuLocalRegion, user_fsgsbase),
        options(nostack, preserves_flags),
    );
    true
}

/// Whether [`enable_user_fsgsbase`] has run on this CPU.
#[inline]
pub fn user_fsgsbase_enabled() -> bool {
    let flag: u8;
    unsafe {
        asm!(
            "mov {flag}, byte ptr gs:[{off}]",
            flag = out(reg_byte) flag,
            off = const core::mem::offset_of!(CpuLocalRegion, user_fsgsbase),
            options(nostack, preserves_flags, readonly),
        );
    }
    flag != 0
}

/// The one field of [`CpuLocalRegion`] whose offset is hardcoded elsewhere.
///
/// `GSBASE` and the TSS descriptor both point at this struct, and the entry
/// stubs read `gs:4` and `gs:12` as offsets *into the TSS* -- the ring-0 stack
/// pointer and the slot where the user stack is stashed. That only works while
/// `tss` is the first field: move it and every `syscall` and every trap loads
/// its kernel stack from whatever word happens to be at `gs:4`, which is the
/// first push of the entry path and therefore a fault with no stack to report
/// it on.
///
/// The other fields need no guard, and that is worth writing down rather than
/// leaving to the next reader: the assembly reaches `cpu_local`,
/// `logical_cpu_id`, `logical_cpu_valid` and `dbg_save` through
/// `const offset_of!(..)` (see [`DBG_SAVE_GS_OFFSET`] and the `gs:[{off}]`
/// operands below), so reordering them moves the asm's operand with the field.
/// A mutation that swaps two of them survives because it is *equivalent*, not
/// because it is untested.
///
/// A compile-time assertion rather than a unit test because this module only
/// exists under `target_os = "none"` / `"uefi"`, so no host `cargo test`
/// compiles it.
const _: () = {
    assert!(
        core::mem::offset_of!(CpuLocalRegion, tss) == 0,
        "the entry stubs read gs:4 and gs:12 as offsets into the TSS, so it has to come first"
    );
    // The three words the entry stubs reach by `gs:[const]` must be three
    // distinct words. They are only distinct because they are distinct fields,
    // which is exactly the kind of thing a later `#[repr(packed)]` or a
    // hand-written offset would break silently: an aliased FS/GS cache makes
    // `syscall_return` skip a WRMSR it owed, so a thread returns to ring 3
    // with another thread's TLS base.
    assert!(USER_FSBASE_GS_OFFSET != USER_GSBASE_GS_OFFSET);
    assert!(USER_FSGSBASE_GS_OFFSET != USER_FSBASE_GS_OFFSET);
    assert!(USER_FSGSBASE_GS_OFFSET != USER_GSBASE_GS_OFFSET);
    // And the flag byte must not land inside the TSS, which the CPU writes
    // behind the kernel's back on every ring transition.
    assert!(USER_FSGSBASE_GS_OFFSET >= size_of::<TSS>());
};

/// DEBUG: offset de `dbg_save` dentro de [`CpuLocalRegion`], para el asm.
pub const DBG_SAVE_GS_OFFSET: usize = core::mem::offset_of!(CpuLocalRegion, dbg_save);

/// DEBUG: leer la dirección de save per-CPU registrada por el asm.
#[inline]
pub fn read_dbg_save() -> usize {
    let ret: usize;
    unsafe {
        asm!(
            "mov {ret}, gs:[{off}]",
            ret = out(reg) ret,
            off = const core::mem::offset_of!(CpuLocalRegion, dbg_save),
            options(nostack, preserves_flags, readonly),
        );
    }
    ret
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
        user_fsgsbase: 0,
        _pad: [0; 5],
        dbg_save: 0,
        user_fsbase: UNKNOWN_BASE,
        user_gsbase: UNKNOWN_BASE,
    });
    // Trap/exception stack used on ring3->ring0 transitions (TSS sp0). 4 KiB was
    // dangerously small for deep user-trap handling (page-fault -> VMO commit ->
    // smoltcp ...): an overflow silently corrupts the adjacent heap (observed as
    // wild UserContext corruption -> iret to ring-0 junk -> #UD). Use 64 KiB.
    let trap_stack_top = Box::leak(Box::new([0u8; 0x10000])).as_ptr() as u64 + 0x10000;
    region.tss.privilege_stack_table[0] = VirtAddr::new(trap_stack_top);
    // Dedicated #DF stack (IST1; see idt.rs vector 8). A double fault caused
    // by a kernel stack overflow cannot be handled on the faulting stack —
    // the CPU must switch to a known-good one or the machine triple-faults
    // silently. 16 KiB is ample: the #DF handler only prints and panics.
    let df_stack_top = Box::leak(Box::new([0u8; 0x4000])).as_ptr() as u64 + 0x4000;
    region.tss.interrupt_stack_table[0] = VirtAddr::new(df_stack_top);
    // Dedicated #GP stack (IST2; see idt.rs vector 13). A #GP raised because an
    // interrupt/`ret` landed on a corrupted kernel stack cannot push its
    // exception frame onto that same stack — it faults again and escalates
    // #GP -> #DF -> triple fault (the machine dies silently). Giving #GP its own
    // known-good stack lets the handler run: it repairs a mangled saved RIP and
    // resumes, or panics with a backtrace instead of vanishing. 16 KiB is ample.
    let gp_stack_top = Box::leak(Box::new([0u8; 0x4000])).as_ptr() as u64 + 0x4000;
    region.tss.interrupt_stack_table[1] = VirtAddr::new(gp_stack_top);
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

        // allocate new GDT with 7 more entries, and the three TLS slots after
        // them
        //
        // NOTICE: for fast syscall:
        //   STAR[47:32] = K_CS   = K_SS - 8
        //   STAR[63:48] = U_CS32 = U_SS32 - 8 = U_CS - 16
        //
        // The TLS descriptors start out empty (a not-present descriptor, which
        // is what index 0 of any GDT is too): a thread that has not called
        // `set_thread_area` has no thread area, and loading its selector has
        // to fault rather than reach whatever the last thread put there.
        let mut gdt = Vec::from(old_gdt);
        gdt.extend([tss0, tss1, KCODE64, KDATA64, UCODE32, UDATA32, UCODE64].iter());
        let tls_index = gdt.len();
        gdt.extend([0u64; COMPAT_TLS_ENTRIES].iter());
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
        //
        // `swapgs` (used by every trap/syscall entry and exit in trap.S /
        // syscall.S to reach `gs:4`/`gs:12`, i.e. this CpuLocalRegion's TSS
        // slots) EXCHANGES GsBase (IA32_GS_BASE, MSR 0xC0000101 -- the
        // currently active one) with KernelGsBase (IA32_KERNEL_GS_BASE, MSR
        // 0xC0000102). Writing only GsBase leaves KernelGsBase at its
        // power-up value (0, never otherwise written anywhere in this
        // codebase) -- so the very first `swapgs` executed on this cpu (on
        // its first trap taken from ring 3) swaps a *valid* GsBase for a
        // *zero* one, and every `gs:`-relative access in that entry's
        // trampoline prologue (`__from_user` in trap.S, `syscall_entry` in
        // syscall.S) resolves against linear address ~0 instead of this
        // region -- under whatever page table was active for the
        // interrupted user thread, that is normally the deliberately
        // unmapped null-guard page, so the very read/write that is supposed
        // to fetch this cpu's kernel stack (TSS.sp0) instead takes a second,
        // nested page fault while the first trap is still being delivered:
        // exactly x86's double-fault trigger condition. Writing the same
        // pointer to both MSRs makes `swapgs` idempotent with respect to the
        // value `gs:` accesses see, regardless of how many times it has
        // fired -- the standard pattern for any kernel that uses swapgs.
        #[allow(const_item_mutation)]
        GsBase::MSR.write(tss as *const _ as u64);
        #[allow(const_item_mutation)]
        KernelGsBase::MSR.write(tss as *const _ as u64);

        let star_k_cs = SegmentSelector::new(entry_count as u16 + 2, PrivilegeLevel::Ring0).0;
        let star_u_cs = SegmentSelector::new(entry_count as u16 + 4, PrivilegeLevel::Ring3).0;
        Star::write_raw(star_u_cs, star_k_cs);

        // Persist the BSP GDT info for APs.  APs cannot use their own sgdt()
        // after the trampoline because it points at a 3-entry mini-GDT that
        // lacks KCODE64/UCODE64/UDATA32 etc.  They must extend THIS GDT.
        BSP_GDT_BASE.store(gdt.as_ptr() as u64, Ordering::Release);
        BSP_GDT_COUNT.store(gdt.len(), Ordering::Release);
        // Published before `BSP_GDT_BASE`'s release store is read by any AP,
        // and read by `init_ap` and `load_compat_tls` instead of being
        // recomputed: the one arithmetic copy of this layout that existed
        // ("KCODE64 index = entry_count - 5") went stale the moment the table
        // grew, and an index into a descriptor table that is quietly off by
        // three is a #GP on a far return with no clue in it.
        BSP_TLS_INDEX.store(tls_index, Ordering::Release);
        BSP_KCODE64_INDEX.store(entry_count + 2, Ordering::Release);
        // Pack STAR fields so APs can replicate the write.
        BSP_STAR.store(
            ((star_u_cs as u32) << 16) | star_k_cs as u32,
            Ordering::Release,
        );
    }
}

/// Per-CPU GDT/TSS/GSBASE setup for application processors.
///
/// The BSP must call [`init`] first.  Extends the BSP's GDT with this AP's
/// TSS entry, loads it, and points `GSBASE` at the AP's [`CpuLocalRegion`].
/// Also replicates the `STAR` MSR (segment selectors for syscall/sysret),
/// which is a per-CPU register that the BSP set during [`init`].
pub fn init_ap() {
    let mut region = Box::new(CpuLocalRegion {
        tss: TSS::new(),
        cpu_local: 0,
        logical_cpu_id: 0,
        logical_cpu_valid: 0,
        user_fsgsbase: 0,
        _pad: [0; 5],
        dbg_save: 0,
        user_fsbase: UNKNOWN_BASE,
        user_gsbase: UNKNOWN_BASE,
    });
    // Trap/exception stack used on ring3->ring0 transitions (TSS sp0). 4 KiB was
    // dangerously small for deep user-trap handling (page-fault -> VMO commit ->
    // smoltcp ...): an overflow silently corrupts the adjacent heap (observed as
    // wild UserContext corruption -> iret to ring-0 junk -> #UD). Use 64 KiB.
    let trap_stack_top = Box::leak(Box::new([0u8; 0x10000])).as_ptr() as u64 + 0x10000;
    region.tss.privilege_stack_table[0] = VirtAddr::new(trap_stack_top);
    // Dedicated #DF stack (IST1; see idt.rs vector 8). A double fault caused
    // by a kernel stack overflow cannot be handled on the faulting stack —
    // the CPU must switch to a known-good one or the machine triple-faults
    // silently. 16 KiB is ample: the #DF handler only prints and panics.
    let df_stack_top = Box::leak(Box::new([0u8; 0x4000])).as_ptr() as u64 + 0x4000;
    region.tss.interrupt_stack_table[0] = VirtAddr::new(df_stack_top);
    // Dedicated #GP stack (IST2; see idt.rs vector 13). A #GP raised because an
    // interrupt/`ret` landed on a corrupted kernel stack cannot push its
    // exception frame onto that same stack — it faults again and escalates
    // #GP -> #DF -> triple fault (the machine dies silently). Giving #GP its own
    // known-good stack lets the handler run: it repairs a mangled saved RIP and
    // resumes, or panics with a backtrace instead of vanishing. 16 KiB is ample.
    let gp_stack_top = Box::leak(Box::new([0u8; 0x4000])).as_ptr() as u64 + 0x4000;
    region.tss.interrupt_stack_table[1] = VirtAddr::new(gp_stack_top);
    let region: &'static CpuLocalRegion = Box::leak(region);
    let tss: &'static TSS = &region.tss;
    let (tss0, tss1) = match Descriptor::tss_segment(tss) {
        Descriptor::SystemSegment(tss0, tss1) => (tss0, tss1),
        _ => unreachable!(),
    };
    #[cfg(feature = "ioport_bitmap")]
    let tss0 = (tss0 & !0xFFFF) | (size_of::<TSS>() as u64);

    static GDT_EXTEND_LOCK: AtomicBool = AtomicBool::new(false);
    while GDT_EXTEND_LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }

    unsafe {
        // Read the BSP's full GDT (not sgdt() which would give the AP's own
        // 3-entry trampoline GDT, missing all user-space segment descriptors).
        let base = BSP_GDT_BASE.load(Ordering::Acquire);
        let entry_count = BSP_GDT_COUNT.load(Ordering::Acquire);
        assert!(base != 0, "BSP GDT not initialized before AP init_ap");
        let old_gdt = core::slice::from_raw_parts(base as *const u64, entry_count);
        let mut gdt = Vec::from(old_gdt);
        gdt.extend([tss0, tss1].iter());
        let gdt = Vec::leak(gdt);
        lgdt(&DescriptorTablePointer {
            limit: gdt.len() as u16 * 8 - 1,
            base: VirtAddr::new(gdt.as_ptr() as _),
        });

        // The AP entered via the trampoline with CS=0x8 (mini-GDT code segment).
        // After lgdt() the mini-GDT is no longer the active GDT, so CS=0x8 now
        // points at BSP's GDT[1] (a UEFI firmware descriptor, typically a 32-bit
        // code or data segment).  Any interrupt that fires before a far branch
        // will save CS=0x8 to the stack; on iretq the CPU reloads CS=0x8 from
        // the new GDT and faults with #GP error_code=0x8.  Fix: far-return to
        // immediately reload CS with KCODE64.
        //
        // `init` published the index rather than leaving it to be derived
        // from `entry_count`: it appends a variable number of descriptors
        // (seven, plus the TLS slots), and the arithmetic that used to live
        // here was wrong by exactly as many as the table had grown.
        let kcode64_idx = BSP_KCODE64_INDEX.load(Ordering::Acquire) as u16;
        assert!(
            kcode64_idx != 0,
            "BSP GDT layout not published before init_ap"
        );
        let kcode64_sel = SegmentSelector::new(kcode64_idx, PrivilegeLevel::Ring0).0 as u64;
        asm!(
            // Push new CS, then the return address, then 64-bit far return.
            // 0x48 0xcb = REX.W RETF — the 64-bit far return (lretq).
            // This atomically reloads CS with kcode64_sel and continues at "2:".
            "pushq {sel}",
            "leaq 2f(%rip), {tmp}",
            "pushq {tmp}",
            ".byte 0x48, 0xcb",
            "2:",
            sel = in(reg) kcode64_sel,
            tmp = lateout(reg) _,
            options(att_syntax),
        );

        load_tss(SegmentSelector::new(
            entry_count as u16,
            PrivilegeLevel::Ring0,
        ));
        // See the matching comment in `init()`: both MSRs must hold this AP's
        // own CpuLocalRegion pointer, or this AP's first `swapgs` (on its
        // first trap from ring 3) hands `gs:`-relative accesses a zeroed
        // base instead of it.
        #[allow(const_item_mutation)]
        GsBase::MSR.write(tss as *const _ as u64);
        #[allow(const_item_mutation)]
        KernelGsBase::MSR.write(tss as *const _ as u64);

        // Replicate the STAR MSR (per-CPU) that the BSP set during init().
        let packed = BSP_STAR.load(Ordering::Acquire);
        let star_k_cs = (packed & 0xFFFF) as u16;
        let star_u_cs = (packed >> 16) as u16;
        Star::write_raw(star_u_cs, star_k_cs);
    }

    GDT_EXTEND_LOCK.store(false, Ordering::Release);
}

/// The selector ring 3 loads into `%gs` (or `%fs`) to reach TLS entry
/// `entry`, with RPL 3.
///
/// `entry` is 0, 1 or 2; anything else is a caller that did not range-check
/// what came from ring 3, and would hand out a selector naming some other
/// descriptor entirely.
pub fn compat_tls_selector(entry: usize) -> u16 {
    assert!(entry < COMPAT_TLS_ENTRIES, "TLS entry out of range");
    let index = BSP_TLS_INDEX.load(Ordering::Acquire);
    assert!(index != 0, "GDT not initialized");
    SegmentSelector::new((index + entry) as u16, PrivilegeLevel::Ring3).0
}

/// The GDT index of the first TLS descriptor, which is the `entry_number`
/// `set_thread_area(2)` speaks in.
///
/// Linux's is the constant `GDT_ENTRY_TLS_MIN`, 12, because Linux lays out its
/// own GDT from the first entry. This one cannot be a constant: the table
/// starts as the firmware's and this kernel appends to it, so where the TLS
/// slots land depends on how many descriptors were already there. The number
/// has to be the real index all the same, and not a slot number of our own
/// choosing, because ring 3 builds the selector itself --- musl does
/// `(entry_number << 3) | 3` --- and a number that is not the index names some
/// other descriptor.
pub fn compat_tls_index() -> usize {
    let index = BSP_TLS_INDEX.load(Ordering::Acquire);
    assert!(index != 0, "GDT not initialized");
    index
}

/// Write a thread's three 32-bit TLS descriptors into THIS CPU's GDT.
///
/// The counterpart of Linux's `load_TLS`, and called in the same place: on the
/// way to user mode, every time, because the thread can wake up on a CPU that
/// last held somebody else's descriptors. The descriptors live with the thread
/// and the GDT slots are scratch space for whoever is running now.
///
/// Writing a descriptor table that is currently loaded is allowed: the CPU
/// reads a descriptor when a selector is loaded, not before, so ring 3's next
/// `mov %gs, sel` is what picks this up. The window between this call and the
/// return to user is not a hole either --- a preemption in it ends in another
/// trip through the same path, which loads the right thread's descriptors
/// again.
///
/// Takes `&self`-free module state only: there is no lock, because the slots
/// it writes belong to the CPU it runs on, and the caller is that CPU.
pub fn load_compat_tls(desc: &[u64; COMPAT_TLS_ENTRIES]) {
    let index = BSP_TLS_INDEX.load(Ordering::Acquire);
    assert!(index != 0, "GDT not initialized");
    unsafe {
        let gdtp = sgdt();
        let count = (gdtp.limit as usize + 1) / size_of::<u64>();
        assert!(
            index + COMPAT_TLS_ENTRIES <= count,
            "this CPU's GDT has no TLS slots"
        );
        let slots = gdtp.base.as_mut_ptr::<u64>().add(index);
        core::ptr::copy_nonoverlapping(desc.as_ptr(), slots, COMPAT_TLS_ENTRIES);
    }
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
