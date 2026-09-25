//! Virtual memory operations.

use core::slice;
use core::sync::atomic::{AtomicUsize, Ordering};

static KERNEL_VMTOKEN: AtomicUsize = AtomicUsize::new(0);

use x86_64::{
    instructions::tlb,
    registers::control::{Cr3, Cr3Flags},
};

use crate::utils::page_table::{PageTableImpl, PageTableLevel4};
// The entry format itself lives in `utils::pte`, which every build
// compiles -- including the host's, so it can be tested.
pub use crate::utils::pte::x86_64::X86PTE;
use crate::{mem::phys_to_virt, PhysAddr, VirtAddr};

hal_fn_impl! {
    impl mod crate::hal_fn::vm {
        fn activate_paging(vmtoken: PhysAddr) {
            use x86_64::structures::paging::PhysFrame;
            let frame = PhysFrame::containing_address(x86_64::PhysAddr::new(vmtoken as _));
            if Cr3::read().0 != frame {
                // Publish BEFORE the hardware switch: a TLB-shootdown initiator
                // that reads the old token and skips this CPU races a CR3 write
                // that flushes every non-global entry anyway; reading the NEW
                // token early merely costs one spurious IPI. The reverse order
                // would let an initiator skip a CPU that already runs the new
                // tables — a missed invalidation. See `remote_flush_tlb_aspace`.
                crate::common::ipi::note_active_vmtoken(frame.start_address().as_u64() as usize);
                unsafe { Cr3::write(frame, Cr3Flags::empty()) };
                debug!("set page_table @ {:#x}", vmtoken);
            }
        }

        fn current_vmtoken() -> PhysAddr {
            Cr3::read().0.start_address().as_u64() as _
        }

        fn pin_kernel_vmtoken() {
            let token = current_vmtoken();
            let prev = KERNEL_VMTOKEN.swap(token, Ordering::Release);
            if prev != 0 && prev != token {
                crate::klog_warn!(
                    "pin_kernel_vmtoken: retoken {:#x} -> {:#x}",
                    prev,
                    token
                );
            }
        }

        fn kernel_vmtoken() -> PhysAddr {
            KERNEL_VMTOKEN.load(Ordering::Acquire)
        }

        fn activate_kernel_paging() {
            let token = KERNEL_VMTOKEN.load(Ordering::Acquire);
            // Skip the CR3 write when the kernel table is already active: the
            // executor's idle callback invokes this on EVERY idle iteration,
            // and a redundant CR3 reload is a full non-global TLB flush. The
            // write is only needed to drop a lingering user CR3 (lazy TLB);
            // when CR3 already points at the kernel tree there is nothing
            // stale to release.
            //
            // One CR3 holds both roots, so the user root this CPU has loaded
            // is simply the one it is running on. aarch64 has two registers
            // and asked the wrong one: hence the shared predicate.
            if crate::common::vm::should_restore_kernel_table(current_vmtoken(), token) {
                activate_paging(token);
            }
        }

        fn flush_tlb(vaddr: Option<VirtAddr>) {
            if let Some(vaddr) = vaddr {
                let v = vaddr as u64;
                if v <= 0x0000_7fff_ffff_ffff || v >= 0xffff_8000_0000_0000 {
                    tlb::flush(x86_64::VirtAddr::new(v));
                } else {
                    warn!("flush_tlb: non-canonical vaddr {:#x}", vaddr);
                }
            } else {
                tlb::flush_all()
            }
        }

        fn pt_clone_kernel_space(dst_pt_root: PhysAddr, src_pt_root: PhysAddr) {
            let entry_range = 0x100..0x200; // 0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF
            let dst_table = unsafe { slice::from_raw_parts_mut(phys_to_virt(dst_pt_root) as *mut X86PTE, 512) };
            let src_table = unsafe { slice::from_raw_parts(phys_to_virt(src_pt_root) as *const X86PTE, 512) };
            for i in entry_range {
                dst_table[i] = src_table[i];
                // Do NOT set PTF::GLOBAL here. Bit 8 (the G bit of *leaf*
                // entries) is IGNORED in a PML4E on Intel but RESERVED
                // (must-be-zero) on AMD: with it set, the first hardware page
                // walk through this entry raises #PF with the RSVD error bit.
                // Every kernel address in the new user address space resolves
                // through these entries — including the fault handler itself —
                // so on an AMD CPU (QEMU/KVM or VirtualBox on an AMD host, or
                // bare metal) activating the first user CR3 escalated to a
                // triple fault and rebooted the machine right when boot
                // reached 100%. Intel silently ignored the bit, which is why
                // this only ever crashed on AMD. (See AMD APM Vol. 2 §5.3.3,
                // and KVM's `nonleaf_bit8_rsvd` in arch/x86/kvm/mmu.c.)
                // Global-TLB retention for kernel mappings, if ever wanted,
                // must be done via the G bit on leaf PTEs/PDEs instead.
            }
        }
    }
}

/// The 4-level page table on x86.
pub type PageTable = PageTableImpl<PageTableLevel4, X86PTE>;
