use crate::hal_fn::mem::phys_to_virt;
use crate::imp::config::*;
use crate::sync::Mutex;
use crate::utils::page_table::{PageTableImpl, PageTableLevel4};
// The descriptor format itself lives in `utils::pte`, which every
// build compiles -- including the host's, so it can be tested.
pub use crate::utils::pte::aarch64::AARCH64PTE;
use crate::MMUFlags;
use crate::{PhysAddr, VirtAddr, KCONFIG};
use core::sync::atomic::{AtomicUsize, Ordering};
use cortex_a::registers::*;
use tock_registers::interfaces::{Readable, Writeable};
use zcore_drivers::irq::gic_400::{GICC_SIZE, GICD_SIZE};

lazy_static! {
    static ref KERNEL_PT: Mutex<PageTable> = Mutex::new(init_kernel_page_table().unwrap());
}

static KERNEL_VMTOKEN: AtomicUsize = AtomicUsize::new(0);

/// remap kernel ELF segments with 4K page
fn init_kernel_page_table() -> PagingResult<PageTable> {
    extern "C" {
        fn stext();
        fn etext();
        fn srodata();
        fn erodata();
        fn sdata();
        fn sksyms();
        fn eksyms();
        fn edata();
        fn sbss();
        fn ebss();
        fn boot_stack();
        fn boot_stack_top();
    }

    let mut pt = PageTable::new();
    let mut map_range = |start: VirtAddr, end: VirtAddr, flags: MMUFlags| -> PagingResult {
        pt.map_cont(
            crate::addr::align_down(start),
            crate::addr::align_up(end - start),
            start - KCONFIG.phys_to_virt_offset,
            flags,
        )
    };

    map_range(
        stext as *const () as usize,
        etext as *const () as usize,
        MMUFlags::READ | MMUFlags::EXECUTE,
    )?;
    map_range(
        srodata as *const () as usize,
        erodata as *const () as usize,
        MMUFlags::READ,
    )?;
    map_range(
        sdata as *const () as usize,
        edata as *const () as usize,
        MMUFlags::READ | MMUFlags::WRITE,
    )?;
    // The symbol table the crash reporters read; its own section, outside
    // every range above. See `.ksyms` in zCore/src/platform/aarch64/linker.ld.
    map_range(
        sksyms as *const () as usize,
        eksyms as *const () as usize,
        MMUFlags::READ,
    )?;
    map_range(
        sbss as *const () as usize,
        ebss as *const () as usize,
        MMUFlags::READ | MMUFlags::WRITE,
    )?;
    // stack
    map_range(
        boot_stack as *const () as usize,
        boot_stack_top as *const () as usize,
        MMUFlags::READ | MMUFlags::WRITE,
    )?;
    // uart
    map_range(
        phys_to_virt(KCONFIG.uart_base),
        phys_to_virt(KCONFIG.uart_base) + UART_SIZE,
        MMUFlags::READ | MMUFlags::WRITE | MMUFlags::DEVICE,
    )?;
    // gic
    map_range(
        phys_to_virt(KCONFIG.gic_base + 0x1_0000),
        phys_to_virt(KCONFIG.gic_base + 0x1_0000) + GICC_SIZE,
        MMUFlags::READ | MMUFlags::WRITE | MMUFlags::DEVICE,
    )?;
    map_range(
        phys_to_virt(KCONFIG.gic_base),
        phys_to_virt(KCONFIG.gic_base) + GICD_SIZE,
        MMUFlags::READ | MMUFlags::WRITE | MMUFlags::DEVICE,
    )?;
    // virtio_drivers
    map_range(
        phys_to_virt(VIRTIO_BASE),
        phys_to_virt(VIRTIO_BASE) + VIRTIO_SIZE,
        MMUFlags::READ | MMUFlags::WRITE | MMUFlags::DEVICE,
    )?;
    // physical frames
    for r in crate::mem::free_pmem_regions() {
        map_range(
            phys_to_virt(r.start),
            phys_to_virt(r.end),
            MMUFlags::READ | MMUFlags::WRITE,
        )?;
    }

    // Force the tables under the kernel VMAR window into existence, by mapping
    // one 4 KiB page there and taking it straight back out. Unmapping clears
    // the leaf; it does not free the tables above it.
    //
    // `zircon_object::vm::KERNEL_ASPACE` is a VMAR with its own page table,
    // made by `PageTable::from_current().clone_kernel()`, and
    // `pt_clone_kernel_space` copies the TOP-LEVEL entries by value. That
    // shares something only if the entry is already there: an empty one gets
    // filled in the clone alone, so the mapping is invisible from the table
    // the CPU is running on. The Linux ELF loader maps an executable's VMO
    // into that VMAR and reads it at boot, off whatever table is loaded, so
    // the first byte faulted every time: `[KERNEL PAGE FAULT]
    // vaddr=0xffffff0200000002 flags=READ | WRITE | USER rip=0x0`, on every
    // case of `Linux Other Test Baremetal (aarch64)`.
    //
    // Nothing else here reaches this far up: the physmap sits just above
    // `phys_to_virt_offset` and 0xffff_ff02_.. is 255 TiB past it. riscv64
    // needs the same block for the same reason, at its own base.
    //
    // Hard-coded, and it must keep agreeing with
    // `zircon_object::vm::KERNEL_ASPACE_BASE`: kernel-hal cannot depend on
    // zircon-object, which depends on it.
    {
        const KERNEL_ASPACE_BASE: VirtAddr = 0xffff_ff02_0000_0000;
        const ONE_PAGE: usize = 0x1000;
        map_range(
            KERNEL_ASPACE_BASE,
            KERNEL_ASPACE_BASE + ONE_PAGE,
            MMUFlags::READ,
        )?;
        pt.unmap_cont(KERNEL_ASPACE_BASE, ONE_PAGE)?;
    }

    Ok(pt)
}

pub(super) fn kernel_page_table() -> &'static Mutex<PageTable> {
    &KERNEL_PT
}

pub fn init() {
    let mut pt = KERNEL_PT.lock();
    info!("initialized kernel page table @ {:#x}", pt.table_phys());
    unsafe {
        pt.activate();
        TTBR0_EL1.set(0);
        flush_tlb_all();
    }
}

pub fn flush_tlb_all() {
    unsafe {
        core::arch::asm!(
            "dsb ishst
             tlbi vmalle1is
             dsb ish
             isb"
        );
    }
}

hal_fn_impl! {
    impl mod crate::hal_fn::vm {
        fn activate_paging(vmtoken: PhysAddr) {
            let flagged_user = (vmtoken & USER_TABLE_FLAG) != 0;
            let vmtoken = vmtoken & PHYS_ADDR_MASK;
            // Which base register this root belongs in is a property of the
            // root, not of a flag every caller has to remember to set: see
            // `is_user_table_root`, and the caller that does not set it.
            let check_if_user = crate::common::vm::is_user_table_root(
                vmtoken,
                KERNEL_VMTOKEN.load(Ordering::Acquire),
                flagged_user,
            );
            if check_if_user != flagged_user {
                crate::klog_warn!(
                    "[vm] page_table {:#x} activated as {} although the caller said {}",
                    vmtoken,
                    if check_if_user { "user" } else { "kernel" },
                    if flagged_user { "user" } else { "kernel" },
                );
            }
            info!("set {} page_table @ {:#x}", if check_if_user { "user" } else { "kernel" }, vmtoken);
            if check_if_user {
                // Publish BEFORE the hardware switch, for the reason spelled
                // out in `remote_flush_tlb_aspace`: an initiator that reads
                // the new token early pays one spurious IPI, one that reads
                // the old token after the switch skips a CPU that already
                // runs these tables.
                //
                // Only for the user table, and that is not a detail. On
                // aarch64 the two roots live in two registers: writing TTBR1
                // leaves whatever user table TTBR0 holds loaded and its
                // entries valid. Noting the kernel root here would claim this
                // CPU had left its user address space while it still holds
                // every one of its translations, and the next shootdown for
                // that address space would filter it out -- a missed
                // invalidation, which is the one failure this whole mechanism
                // is arranged to never have.
                // Already the one loaded: nothing to switch, and nothing on
                // this core to retire (what goes stale while a table stays
                // loaded is the shootdowns' to invalidate, as on x86_64 and
                // riscv64, which have always skipped a reload of the root
                // they hold). This used to flush the whole TLB on every poll
                // of a user thread, resumed on the core it ran on or not.
                if TTBR0_EL1.get() as usize & PHYS_ADDR_MASK == vmtoken {
                    return;
                }
                crate::common::ipi::note_active_vmtoken(vmtoken);
                TTBR0_EL1.set(vmtoken as _);
            } else {
                TTBR1_EL1.set(vmtoken as _);
            }
            // LOCAL, not inner-shareable, for the reason `activate_kernel_paging`
            // gives: the entries that go stale on a switch are this core's
            // own (user leaves are not tagged by ASID, so the TTBR0 write
            // alone does not retire them), and the other cores' entries for
            // the table being left are theirs to retire on their own switch,
            // or the shootdown's when the table itself changes. This was
            // `flush_tlb_all()`, i.e. `tlbi vmalle1is`: EVERY address-space
            // switch on any core wiped the TLB of ALL cores. Under QEMU that
            // is `tlb_flush_all_cpus_synced`, which stops every vCPU; with
            // six vCPUs, six shells and init's boot scripts switching, the
            // aarch64 libc-test image spent 99.9% of its CPU time in the
            // kernel and init took longer than the harness's 300 s to start
            // its services (CI 42197041420). riscv's `activate_paging` has
            // always flushed locally (`sfence.vma` with no broadcast).
            unsafe {
                core::arch::asm!(
                    "dsb nshst
                     tlbi vmalle1
                     dsb nsh
                     isb"
                );
            }
        }

        fn current_vmtoken() -> PhysAddr {
            TTBR1_EL1.get() as _
        }

        fn loaded_user_vmtoken() -> PhysAddr {
            TTBR0_EL1.get() as usize & PHYS_ADDR_MASK
        }

        fn pin_kernel_vmtoken() {
            let token = KERNEL_PT.lock().table_phys();
            KERNEL_VMTOKEN.store(token, Ordering::Release);
        }

        fn kernel_vmtoken() -> PhysAddr {
            KERNEL_VMTOKEN.load(Ordering::Acquire)
        }

        fn activate_kernel_paging() {
            // NOT `current_vmtoken()`, which reads TTBR1_EL1: the kernel's own
            // root is the only thing ever written there, so asking it whether
            // this CPU still runs userspace answered "no" on every call after
            // boot and this function did nothing at all. The user half is
            // translated through the OTHER register, and dropping it is the
            // whole job -- see `should_restore_kernel_table`.
            let token = KERNEL_VMTOKEN.load(Ordering::Acquire);
            let loaded = TTBR0_EL1.get() as usize & PHYS_ADDR_MASK;
            if !crate::common::vm::should_restore_kernel_table(loaded, token) {
                // Nothing loaded, or nowhere to go back to. The idle callback
                // runs on every idle iteration, so this is the common case and
                // it must not flush.
                return;
            }
            // TTBR1_EL1 already holds `token` and is left alone: unlike CR3 and
            // satp, it never held anything else.
            TTBR0_EL1.set(0);
            // Local, not inner-shareable: the entries that go stale here are
            // this core's own. And it has to happen -- user leaves are not
            // marked non-global (`PTF::NG` is declared in `utils::pte` and
            // never set by `From<MMUFlags>`), so they are not tagged by the
            // base register that walked them and the TTBR0 write alone does
            // not stop the TLB answering from them. That is also why
            // `tlbi aside1` would not do.
            unsafe {
                core::arch::asm!(
                    "dsb nshst
                     tlbi vmalle1
                     dsb nsh
                     isb"
                );
            }
            // AFTER the flush, and that is the opposite of `activate_paging`'s
            // order on purpose. There, publishing early costs a spurious IPI;
            // here, publishing early would let a shootdown initiator skip a
            // CPU that still holds the entries it is invalidating.
            crate::common::ipi::note_active_vmtoken(token);
        }

        /// THIS core's TLB, as on x86_64 (`invlpg`/CR3 reload) and riscv
        /// (`sfence.vma`): the HAL contract is a local flush, and the
        /// cross-core half is `common::ipi`'s shootdown protocol, whose
        /// acknowledgers call this very function. Both arms used the
        /// inner-shareable forms (`vaae1is`, `vmalle1is`), so every
        /// acknowledger of a shootdown re-broadcast the flush to every
        /// other core, and every page unmapped cost the whole machine N
        /// flushes instead of one each. Under QEMU an `is` flush stops all
        /// vCPUs (`tlb_flush_all_cpus_synced`); see `activate_paging`.
        fn flush_tlb(vaddr: Option<VirtAddr>) {
            // Translations used at EL1 for the specified address, for all
            // ASID values, on this core.
            if let Some(vaddr) = vaddr {
                unsafe {
                    core::arch::asm!(
                        "dsb nshst
                        tlbi vaae1, {0}
                        dsb nsh
                        isb",
                        in(reg) vaddr >> 12
                    );
                }
            } else {
                unsafe {
                    core::arch::asm!(
                        "dsb nshst
                         tlbi vmalle1
                         dsb nsh
                         isb"
                    );
                }
            }
        }

        fn pt_clone_kernel_space(dst_pt_root: PhysAddr, src_pt_root: PhysAddr) {
            let entry_range = 0x100..0x200;  // 0xffff_0000_8000_0000..0xffff_0000_c000_0000
            let dst_table = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(dst_pt_root) as *mut AARCH64PTE, 512) };
            let src_table = unsafe { core::slice::from_raw_parts(phys_to_virt(src_pt_root) as *const AARCH64PTE, 512) };
            for i in entry_range {
                // Copied as they are, empty ones included. An empty entry used
                // to get PTF::NG stamped onto it here, which no code reads and
                // which the architecture ignores on a table descriptor anyway
                // -- but which left the entry non-zero and not valid. That is
                // the one shape `next_table_mut_or_create` cannot handle: not
                // unused, so it does not build a table, and not present, so it
                // returns NotMapped. The first mapping made in this address
                // space under a top-level entry that happened to be empty when
                // it was cloned therefore died in `VmMapping::map`'s
                // `.expect("failed to map")`, which is what every case of
                // `Linux Other Test Baremetal (aarch64)` hit right after
                // `create_root_fs` finished.
                dst_table[i] = src_table[i];
            }
        }
    }
}

/// Sv48: Page-Based 48-bit Virtual-Memory System.
pub type PageTable = PageTableImpl<PageTableLevel4, AARCH64PTE>;
