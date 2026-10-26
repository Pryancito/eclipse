//! Which card a `PRIME_FD_TO_HANDLE` resolves against. Out of `drm.rs` for
//! its size ceiling.
//!
//! `drm_gem_prime_fd_to_handle` is a per-DEVICE entry point: it asks the
//! file's own prime table (`drm_prime_lookup_buf_handle`), and a dma-buf
//! that table does not have goes to **that device's** importer
//! (`nouveau_gem_prime_import_sg_table`), which builds a GEM object of its
//! own over the pages. A card never hands back another card's handle,
//! because a handle names an object of one device's driver.
//!
//! This kernel resolves the dma-buf back to a GEM object by its physical
//! address, through [`gem_mmap::lookup_by_phys`] -- a table with no card in
//! the key, because for a long time there was only one card that mattered.
//! On one card that reverse lookup IS the prime table and is what makes the
//! desktop work: the compositor exports a swapchain buffer and imports it
//! again on the same device, and it must get the original nouveau handle
//! back or every driver-private ioctl on it answers ENOENT.
//!
//! On two cards the same lookup is wrong, and silently so. Importing a
//! card0 buffer through `card1` found the memory, which is right, and
//! answered with **card0's handle**, which is not: the importer then holds
//! an id out of card0's slice ([`gem_mmap::alloc_handle_slice`]) through
//! card1's node, so its `GEM_CLOSE` drops a reference on card0's object,
//! its `GEM_INFO`/`VM_BIND` on card1 name an object of card0's RM, and the
//! reference `nouveau_gem_import_ref` takes is taken on the wrong card's
//! buffer. That is the hole under "one labwc desktop over
//! `WLR_DRM_DEVICES=/dev/dri/card0:/dev/dri/card1`": wlroots renders on the
//! primary and imports the composed frame on each secondary card, which is
//! exactly this call.
//!
//! So the reverse lookup answers which of the three it is, and the card
//! behind the importing node decides. The card is asked
//! [`DrmScheme::gem_aperture`] **as the kernel** (pid 0), the way the export
//! side asks it: the question here is which card holds the object, not
//! whether the caller owns it. An importer's authority is the dma-buf fd it
//! was handed -- Linux imports a buffer for whoever holds the fd, so
//! Xwayland importing a client's buffer on the same card still resolves to
//! the original handle, as it does today.

use super::*;

/// What the importing node's card should do with a dma-buf, once the
/// physical address has been looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimeImport {
    /// The object is this card's: hand back the original driver-private
    /// handle, as Linux's prime table does for a device's own export.
    SameCard(u32),
    /// The object is ANOTHER card's (the handle is that card's, named only
    /// so the log can say which). This node must import it as an object of
    /// its own; handing the id back would alias the other card's object
    /// through this node.
    OtherCard(u32),
    /// No driver-private object has this physical address: a dumb buffer, or
    /// a nouveau object already freed under the dma-buf. Nothing to resolve.
    Untracked,
}

/// [`PrimeImport`] for the dma-buf at `phys_addr` imported through `minor`.
///
/// With no node table every minor is the primary's (the pre-table rule the
/// whole DRM core keeps), so a single-card box answers [`PrimeImport::SameCard`]
/// for every object it tracks, exactly as before.
pub fn prime_import_for(phys_addr: u64, minor: u32) -> PrimeImport {
    let Some((handle, _)) = zcore_drivers::scheme::gem_mmap::lookup_by_phys(phys_addr) else {
        return PrimeImport::Untracked;
    };
    // As the kernel, not as the caller: see [`card_knows_gem_as`].
    if card_knows_gem_as(minor, handle, KERNEL_PID) {
        PrimeImport::SameCard(handle)
    } else {
        PrimeImport::OtherCard(handle)
    }
}

/// The pid every "which card is this?" question is asked under: the kernel,
/// which may touch every object (`gem_mmap::holds`, `gem_usable_by`).
const KERNEL_PID: u64 = 0;

/// Whether the card behind `minor` has `handle` as one of its GEM objects,
/// as far as `pid` can tell.
///
/// `pid` has to be [`KERNEL_PID`] for the import question, and that is a
/// decision rather than a detail. [`DrmScheme::gem_aperture`] answers two
/// things at once: whether the card has the object, and whether the asker
/// may touch it (an object of another process is `Unknown`). An import is
/// authorised by the dma-buf fd the caller was handed, not by prior
/// ownership -- `drm_gem_prime_fd_to_handle` takes the fd
/// (`dma_buf_get`) and gives the holder a handle on the object, which is
/// how Xwayland imports a client's buffer. Asking as the CALLER would turn
/// every cross-process import on one card into a foreign-card import and
/// mint a second handle over the same memory, which is the chain
/// `prime_import_ref_tests` exists to keep intact.
///
/// A node with no table entry is the primary's, and with no driver at all
/// there is no card to be wrong about, so the reverse lookup alone decides
/// -- which is what the single-card path has always done.
fn card_knows_gem_as(minor: u32, handle: u32, pid: u64) -> bool {
    driver_for_minor(minor)
        .is_none_or(|d| !matches!(d.gem_aperture(handle, pid), GemAperture::Unknown))
}

/// [`card_knows_gem_as`] for the test that shows why the pid is the
/// kernel's.
#[cfg(test)]
pub(super) fn card_knows_gem_as_for_test(minor: u32, handle: u32, pid: u64) -> bool {
    card_knows_gem_as(minor, handle, pid)
}

/// The `/dev/dri` index of the card that holds `handle`, for the line that
/// reports a cross-card import. `None` when no node's card claims it (the
/// object was freed under us, or there is no node table).
pub fn card_index_holding_gem(handle: u32) -> Option<u32> {
    gpu_nodes()
        .iter()
        .find(|n| !matches!(n.driver.gem_aperture(handle, 0), GemAperture::Unknown))
        .map(|n| n.index)
}

/// The `/dev/dri` index of the node `minor` belongs to, for the same line.
pub fn card_index_of_minor(minor: u32) -> Option<u32> {
    gpu_nodes()
        .iter()
        .find(|n| n.owns_minor(minor))
        .map(|n| n.index)
}
