//! Interrupt remapping, as the buses and interrupt controllers see it.
//!
//! With an IOMMU that remaps interrupts, a device's interrupt is no longer
//! a message naming a CPU and a vector: it names an entry of the IOMMU's
//! table, and the IOMMU decides which vector reaches which CPU, and whether
//! the device that sent it may send it at all. So whoever programs an MSI
//! address or an I/O APIC redirection entry asks here for the remappable
//! form. Nothing is installed on a machine without one, or before the
//! IOMMU is up, and then the compatibility format is written as always.

use alloc::sync::Arc;
use core::convert::TryFrom;
use spin::Once;

/// The IOMMU half (`kernel-hal`'s VT-d).
pub trait Remapper: Send + Sync {
    /// The MSI address and data the PCI function at requester id `rid`
    /// must send to raise `vector`. `None` when it cannot be remapped, and
    /// then the caller writes the compatibility format.
    fn msi(&self, rid: u16, vector: u8) -> Option<(u64, u32)>;

    /// The redirection entry (low, high) of a line of the I/O APIC whose
    /// MADT id is `ioapic`, that raises `vector`, unmasked. `None` as for
    /// [`msi`](Self::msi).
    fn ioapic(&self, ioapic: u8, vector: u8, level: bool, active_low: bool) -> Option<(u32, u32)>;
}

static REMAPPER: Once<Arc<dyn Remapper>> = Once::new();

/// Remap every interrupt programmed from now on through `r`. Once, before
/// any line or MSI is programmed.
pub fn install(r: Arc<dyn Remapper>) {
    REMAPPER.call_once(|| r);
}

/// The remapper, when interrupts are remapped.
pub fn remapper() -> Option<&'static Arc<dyn Remapper>> {
    REMAPPER.get()
}

/// The MSI (address, data) a PCI function at `rid` sends to raise `vector`
/// on the APIC `dest`: remappable when interrupts are remapped, the LAPIC
/// window with the vector as data otherwise.
pub fn msi_message(rid: u16, dest: u32, vector: u32) -> (u64, u32) {
    let remapped = remapper()
        .zip(u8::try_from(vector).ok())
        .and_then(|(r, v)| r.msi(rid, v));
    remapped.unwrap_or((0xfee0_0000u64 | ((dest as u64) << 12), vector))
}

/// A requester id: `bus << 8 | device << 3 | function`.
pub fn rid(bus: u8, device: u8, function: u8) -> u16 {
    (bus as u16) << 8 | ((device as u16) & 0x1f) << 3 | (function as u16 & 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_remapper_a_message_is_the_lapic_window_and_the_vector() {
        assert_eq!(msi_message(0x0010, 2, 0x41), (0xfee0_2000, 0x41));
        assert_eq!(rid(1, 2, 3), 0x0113);
    }
}
