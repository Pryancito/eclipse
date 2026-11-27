//! Top-down placement of a Linux process's `mmap`s, as Linux places them,
//! for the address spaces that ask for it.
//!
//! This kernel places an `mmap` with no address bottom-up: the first hole
//! from `mmap_min_addr` that fits (`find_free_area`). Linux places it
//! top-down, in the highest hole below `mmap_base`, just under the stack, so
//! a 64-bit program's buffers sit near `0x7f...`.
//!
//! CUDA depends on that. With unified addressing a GPU maps host memory at
//! the address the CPU sees it at: `cuMemHostRegister` hands UVM the
//! buffer's own address, and the RM must place a virtual allocation of the
//! GPU there. The low part of a GPU VA space is the RM's: the GSP's split
//! range at 4 GiB, the RM's internal range, everything the RM places itself,
//! which it places bottom-up. A buffer placed bottom-up lands among them,
//! the RM cannot put the GPU's allocation there
//! (NV_ERR_INSUFFICIENT_RESOURCES), and `cuMemHostRegister` answers
//! CUDA_ERROR_OUT_OF_MEMORY. Placed as Linux places it, it is far above.
//!
//! So an address space is switched to top-down when its process becomes a
//! CUDA program (its first `/dev/nvidia-uvm` call, which `cuInit` makes),
//! from then on and for every `mmap` that names no address. Anything else
//! keeps the placement it always had.

use super::*;
use lock::Mutex;

/// The address spaces placed top-down, by koid (never reused), with the
/// highest address a placement may end at.
static TOP_DOWN: Mutex<BTreeMap<KoID, VirtAddr>> = Mutex::new(BTreeMap::new());

/// How far below the end of the address space top-down placement starts:
/// Linux's smallest stack gap (`MIN_GAP`, 128 MiB) on top of the stack's own
/// 8 MiB, rounded up to 256 MiB.
pub const TOP_DOWN_GAP: usize = 256 << 20;

impl VmAddressRegion {
    /// Place this address space's `mmap`s that name no address top-down from
    /// [`TOP_DOWN_GAP`] below its end, as Linux does, from now on. Asking
    /// again changes nothing.
    pub fn set_mmap_top_down(&self) {
        let Some(ceiling) = self.end_addr().checked_sub(TOP_DOWN_GAP) else {
            return;
        };
        let ceiling = ceiling & !(PAGE_SIZE - 1);
        TOP_DOWN.lock().entry(self.id()).or_insert(ceiling);
    }

    /// Whether [`VmAddressRegion::set_mmap_top_down`] was asked of this one.
    pub fn mmap_top_down(&self) -> bool {
        TOP_DOWN.lock().contains_key(&self.id())
    }

    /// Forget this one: its koid is not reused, but the table should not
    /// keep every CUDA program that ever ran.
    pub(super) fn forget_top_down(&self) {
        TOP_DOWN.lock().remove(&self.id());
    }
}

/// The top-down placement of `len` bytes aligned to `align`, as an offset
/// into `vmar`, in the highest hole that ends at or below the address
/// space's ceiling and starts at or above `floor` (an offset); `None` when
/// `vmar` is not placed top-down, or no hole fits, and the bottom-up search
/// goes on as before.
pub(super) fn place(
    vmar: &VmAddressRegion,
    inner: &VmarInner,
    floor: VirtAddr,
    len: usize,
    align: usize,
    max_offset: usize,
) -> Option<usize> {
    // Only the Linux mmap path passes a floor (`mmap_min_addr`); every other
    // caller places at offset 0 upward and depends on it (the ELF loader's
    // image region, see `map_ext`), whatever the address space asked for.
    if floor == 0 {
        return None;
    }
    let ceiling = *TOP_DOWN.lock().get(&vmar.id())?;
    let base = vmar.addr;
    let align = align.max(PAGE_SIZE);
    let floor = base + floor;
    let mut top = ceiling.min(base + max_offset.min(vmar.size));
    loop {
        let cand = top.checked_sub(len)? & !(align - 1);
        if cand < floor {
            return None;
        }
        let end = cand + len;
        // The mapping that starts highest below `end`, if it reaches `cand`.
        if let Some((_, m)) = inner.mappings.range(..end).next_back() {
            if m.end_addr() > cand {
                top = m.addr();
                continue;
            }
        }
        // The lowest-starting child in the way.
        if let Some(start) = inner
            .children
            .iter()
            .filter(|ch| ch.addr() < end && ch.end_addr() > cand)
            .map(|ch| ch.addr())
            .min()
        {
            top = start;
            continue;
        }
        return Some(cand - base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anon(vmar: &Arc<VmAddressRegion>, len: usize) -> usize {
        vmar.map_ext_within(
            None,
            VmObject::new_paged(len / PAGE_SIZE),
            0,
            len,
            MMUFlags::RXW | MMUFlags::USER,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
            false,
            false,
            false,
            0x1_0000,
            usize::MAX,
        )
        .expect("mapped")
    }

    #[test]
    fn a_top_down_address_space_places_from_its_ceiling_down() {
        let root = VmAddressRegion::new_root();
        let low = anon(&root, 16 * PAGE_SIZE);
        assert!(low < root.addr() + (1 << 32), "bottom-up before");
        root.set_mmap_top_down();
        assert!(root.mmap_top_down());
        let ceiling = (root.end_addr() - TOP_DOWN_GAP) & !(PAGE_SIZE - 1);
        let first = anon(&root, 16 << 20);
        assert_eq!(first + (16 << 20), ceiling, "the highest hole");
        let second = anon(&root, 4 * PAGE_SIZE);
        assert_eq!(second + 4 * PAGE_SIZE, first, "just below the first");
        root.unmap(first, 16 << 20).expect("unmapped");
        let third = anon(&root, 8 * PAGE_SIZE);
        assert_eq!(third + 8 * PAGE_SIZE, ceiling, "the hole the first left");
        root.forget_top_down();
        assert!(!root.mmap_top_down());
    }

    #[test]
    fn asking_again_keeps_the_first_ceiling() {
        let root = VmAddressRegion::new_root();
        root.set_mmap_top_down();
        root.set_mmap_top_down();
        assert_eq!(
            TOP_DOWN
                .lock()
                .get(&root.id())
                .copied()
                .map(|c| c % PAGE_SIZE),
            Some(0)
        );
        root.forget_top_down();
    }
}
