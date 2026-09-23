//! Virtual memory operations.

use core::slice;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::Mutex;
use riscv::{asm, register::satp};

use crate::addr::{align_down, align_up};
use crate::utils::page_table::{GenericPTE, PageTableImpl, PageTableLevel3};
// The entry format itself lives in `utils::pte`, which every build
// compiles -- including the host's, so it can be tested.
pub use crate::utils::pte::riscv64::Rv64PTE;
use crate::{mem::phys_to_virt, MMUFlags, PhysAddr, VirtAddr, KCONFIG};

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
        fn edata();
        fn sbss();
        fn ebss();

        fn bootstack();
        fn bootstacktop();
    }

    let mut pt = PageTable::new();
    let mut map_range = |start: VirtAddr, end: VirtAddr, flags: MMUFlags| -> PagingResult {
        pt.map_cont(
            crate::addr::align_down(start),
            crate::addr::align_up(end - start),
            start - KCONFIG.phys_to_virt_offset,
            flags | MMUFlags::HUGE_PAGE,
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
    map_range(
        sbss as *const () as usize,
        ebss as *const () as usize,
        MMUFlags::READ | MMUFlags::WRITE,
    )?;
    // stack
    map_range(
        bootstack as *const () as usize,
        bootstacktop as *const () as usize,
        MMUFlags::READ | MMUFlags::WRITE,
    )?;
    // initrd
    if let Some(initrd) = super::INITRD_REGION.as_ref() {
        map_range(
            phys_to_virt(initrd.start),
            phys_to_virt(initrd.end),
            MMUFlags::READ | MMUFlags::WRITE,
        )?;
    }
    cfg_if! {
    if #[cfg(any(feature = "fu740-drivers", feature = "board-c910light"))] {
        extern "C" {
            fn boot_stack();
            fn boot_stack_top();
        }
        map_range(
            boot_stack as *const () as usize,
            boot_stack_top as *const () as usize,
            MMUFlags::READ | MMUFlags::WRITE,
            )?;
    }}
    // device tree
    map_range(
        phys_to_virt(align_down(KCONFIG.dtb_paddr)),
        phys_to_virt(align_up(KCONFIG.dtb_paddr + KCONFIG.dtb_size)),
        MMUFlags::READ,
    )?;
    // physical frames
    //
    // Capped at 2 MiB pages, where every other range above takes whatever
    // `map_cont` can fit. Sv39 has three levels, so a 1 GiB mapping *is* a
    // top-level entry — and `pt_clone_kernel_space` copies the top-level
    // entries BY VALUE into every address space it makes. A top-level entry
    // that points at a table is genuinely shared (the copy is the same
    // pointer); a top-level entry that is a leaf is not. This window is the
    // kernel heap, which is where the scheduler's coroutine stacks come from,
    // and `stack_guard` has to punch a 4 KiB hole in it to give a stack an
    // unmapped guard band. Splitting a 1 GiB leaf to do that would be
    // invisible to every address space cloned before the split — the guard
    // would exist on the kernel's own table and nowhere else. Keeping the
    // top-level entries tables makes the split happen one level down, in a
    // table all of them already share.
    //
    // The price is one extra table per GiB of RAM (4 KiB) and 2 MiB TLB
    // entries instead of 1 GiB ones.
    const MAX_KERNEL_HEAP_PAGE: usize = 0x20_0000;
    for r in crate::mem::free_pmem_regions() {
        info!("FREE PHY MEM: {:x?}", r);
        let start = align_down(phys_to_virt(r.start));
        let end = align_up(phys_to_virt(r.end));
        let mut vaddr = start;
        while vaddr < end {
            // Chunks stop on 2 MiB boundaries, so `map_cont` sees a run that
            // is never big enough for a 1 GiB page and is 2 MiB-aligned as
            // soon as the region is.
            let chunk =
                (MAX_KERNEL_HEAP_PAGE - (vaddr & (MAX_KERNEL_HEAP_PAGE - 1))).min(end - vaddr);
            map_range(vaddr, vaddr + chunk, MMUFlags::READ | MMUFlags::WRITE)?;
            vaddr += chunk;
        }
    }

    // Force the level-1 table of the kernel VMAR window into existence, by
    // mapping one 4 KiB page there and taking it straight back out. Unmapping
    // clears the leaf; it does not free the tables above it.
    //
    // `zircon_object::vm::KERNEL_ASPACE` is a VMAR with its own page table,
    // made by `PageTable::from_current().clone_kernel()`, and
    // `pt_clone_kernel_space` copies the TOP-LEVEL entries by value. That
    // shares something only if the entry is already there: an empty one gets
    // filled in the clone alone, so the mapping is invisible from the table
    // the CPU is running on. The Linux ELF loader maps an executable's VMO
    // into that VMAR and reads it at boot, off the kernel's own table, so the
    // first byte faulted every time: `[KERNEL PAGE FAULT]
    // vaddr=0xffffffff80000001 flags=READ rip=0x0`, then `[KERNEL BUG]
    // halting`, on every case of `Linux Other Test Baremetal (riscv64)`.
    // x86_64 gets this for free, because the bootloader's table already maps
    // through the top-level entry its `KERNEL_ASPACE_BASE` falls in; aarch64
    // does not, and carries the same block in its own `init_kernel_page_table`.
    //
    // Hard-coded, and it must keep agreeing with
    // `zircon_object::vm::KERNEL_ASPACE_BASE` for riscv64: kernel-hal cannot
    // depend on zircon-object, which depends on it.
    {
        const KERNEL_ASPACE_BASE: VirtAddr = 0xffff_ffff_8000_0000;
        const ONE_PAGE: usize = 0x1000;
        map_range(
            KERNEL_ASPACE_BASE,
            KERNEL_ASPACE_BASE + ONE_PAGE,
            MMUFlags::READ,
        )?;
        pt.unmap_cont(KERNEL_ASPACE_BASE, ONE_PAGE)?;
    }

    info!("initialized kernel page table @ {:#x}", pt.table_phys());
    Ok(pt)
}

pub(super) fn kernel_page_table() -> &'static Mutex<PageTable> {
    &KERNEL_PT
}

pub(super) fn init() {
    unsafe { KERNEL_PT.lock().activate() };
}

hal_fn_impl! {
    impl mod crate::hal_fn::vm {
        fn activate_paging(vmtoken: PhysAddr) {
            let old_token = current_vmtoken();
            if old_token != vmtoken {
                #[cfg(target_arch = "riscv64")]
                let mode = satp::Mode::Sv39;
                unsafe {
                    satp::set(mode, 0, vmtoken >> 12);
                    asm::sfence_vma_all();
                }
                debug!("cpu {} switch table {:x?} -> {:x?}", crate::cpu::cpu_id(), old_token, vmtoken);
            }
        }

        fn current_vmtoken() -> PhysAddr {
            satp::read().ppn() << 12
        }

        fn pin_kernel_vmtoken() {
            let token = KERNEL_PT.lock().table_phys();
            KERNEL_VMTOKEN.store(token, Ordering::Release);
        }

        fn activate_kernel_paging() {
            let token = KERNEL_VMTOKEN.load(Ordering::Acquire);
            // Already on the kernel table: skip the satp write + fence (the
            // idle callback calls this every idle iteration).
            if token != 0 && current_vmtoken() != token {
                activate_paging(token);
            }
        }

        fn flush_tlb(vaddr: Option<VirtAddr>) {
            unsafe {
                if let Some(vaddr) = vaddr {
                    asm::sfence_vma(0, vaddr)
                } else {
                    asm::sfence_vma_all();
                }
            }
        }

        fn pt_clone_kernel_space(dst_pt_root: PhysAddr, src_pt_root: PhysAddr) {
            let entry_range = 0x100..0x200; // 0xFFFF_FFC0_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF
            let dst_table = unsafe { slice::from_raw_parts_mut(phys_to_virt(dst_pt_root) as *mut Rv64PTE, 512) };
            let src_table = unsafe { slice::from_raw_parts(phys_to_virt(src_pt_root) as *const Rv64PTE, 512) };
            for i in entry_range {
                dst_table[i] = src_table[i];
                if !dst_table[i].is_unused() {
                    dst_table[i].set_global();
                }
            }
        }
    }
}

/// Sv39: Page-Based 39-bit Virtual-Memory System.
pub type PageTable = PageTableImpl<PageTableLevel3, Rv64PTE>;
