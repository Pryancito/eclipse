//! What the driver of a PCI function may do to the bridges above it, when
//! that driver is a process (`udrv-nvidia`) and cannot reach them itself.
//!
//! The kernel's NVIDIA driver walks the bus to its GPU's root port for
//! three things, each before or around the GPU's firmware boot:
//!
//! * it reads the port's PCI Express capability and clamps the GPU's Max
//!   Payload Size to the port's (`NvidiaGpu::normalize_mps`): UEFI leaves
//!   both RTX cards of the reference machine at 256 bytes under ports at
//!   128, and the first oversized packet the GPU sends is a Malformed TLP
//!   that stalls the port;
//! * it clears VGA Enable in the Bridge Control of every bridge on the
//!   path while the firmware boots, and puts it back
//!   (`NvidiaGpu::set_path_vga_routing`);
//! * it pulses Secondary Bus Reset on the port when the link wedges anyway
//!   (`NvidiaGpu::sbr_recover`).
//!
//! In a process every function but the GPU's read as absent, so the walk
//! found no port and all three were skipped without a word. Reading is
//! harmless, so the kernel reads any function for a driver that holds one
//! (`PciPlace::config_read_at`). Writing is not, so this module is the whole
//! of what a driver may write above its function: those two bits of Bridge
//! Control, the reset only on a bridge that leads to nothing but its device,
//! and as a pulse the kernel ends itself ([`bridge_control_pulsed`]).

use super::cfgwrite::cfg_write16;
use pci::{CSpaceAccessMethod, Location, PortOps};

/// Header Type (0x0E), without its multi-function bit: a PCI-PCI bridge.
const HEADER_BRIDGE: u8 = 0x01;
/// Bridge Control, in a PCI-PCI bridge's header.
pub const BRIDGE_CONTROL: u16 = 0x3E;
/// Bridge Control bit 3, VGA Enable: the legacy VGA ranges are forwarded
/// to the secondary bus.
pub const VGA_ENABLE: u16 = 1 << 3;
/// Bridge Control bit 6, Secondary Bus Reset.
pub const BUS_RESET: u16 = 1 << 6;

/// Why a write to a bridge was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Nothing there, or not a PCI-PCI bridge.
    NotABridge,
    /// A bridge whose buses do not lead to the driver's function.
    NotAbove,
    /// A bus reset asked of a bridge that leads to more than the driver's
    /// device: it would reset somebody else's.
    Shared,
}

/// The buses behind the bridge at `bridge`: its secondary and subordinate
/// bus numbers. `None` for a slot with nothing in it, or a function that is
/// not a PCI-PCI bridge.
///
/// # Safety
///
/// `ops` has to reach the configuration space.
pub unsafe fn buses_behind<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    bridge: Location,
) -> Option<(u8, u8)> {
    if unsafe { am.read16(ops, bridge, 0x00) } == 0xFFFF {
        return None;
    }
    if unsafe { am.read8(ops, bridge, 0x0E) } & 0x7F != HEADER_BRIDGE {
        return None;
    }
    let buses = unsafe { am.read32(ops, bridge, 0x18) };
    Some(((buses >> 8) as u8, (buses >> 16) as u8))
}

/// Whether the bridge with `(secondary, subordinate)` leads to the device
/// of `held` and to nothing else: `held`'s bus is the one right behind it,
/// and no other device answers on any bus behind it (firmware gives some
/// ports spare bus numbers, with nothing on them). The device's other
/// functions (a GPU's audio) are reset with it either way, as they are by
/// the kernel's own driver.
///
/// # Safety
///
/// `ops` has to reach the configuration space.
unsafe fn leads_only_to<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    (secondary, subordinate): (u8, u8),
    held: Location,
) -> bool {
    if secondary != held.bus || subordinate < secondary {
        return false;
    }
    (secondary..=subordinate).all(|bus| {
        (0..32u8)
            .filter(|&device| (bus, device) != (held.bus, held.device))
            .all(|device| {
                let loc = Location {
                    bus,
                    device,
                    function: 0,
                };
                unsafe { am.read16(ops, loc, 0x00) == 0xFFFF }
            })
    })
}

/// Write Bridge Control of `bridge` for the driver of `held`: of `value`,
/// only [`VGA_ENABLE`] and [`BUS_RESET`] are taken, every other bit stays
/// as the bridge has it. What is there afterwards.
///
/// Refused for anything that is not a bridge above `held`, and a change of
/// [`BUS_RESET`] for a bridge that leads to more than `held`'s device.
///
/// # Safety
///
/// `ops` has to reach the configuration space.
pub unsafe fn bridge_control_write<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    bridge: Location,
    held: Location,
    value: u16,
) -> Result<u16, Refused> {
    let buses = unsafe { buses_behind(ops, am, bridge) }.ok_or(Refused::NotABridge)?;
    if !(buses.0 <= held.bus && held.bus <= buses.1) {
        return Err(Refused::NotAbove);
    }
    let old = unsafe { am.read16(ops, bridge, BRIDGE_CONTROL) };
    if (old ^ value) & BUS_RESET != 0 && !unsafe { leads_only_to(ops, am, buses, held) } {
        return Err(Refused::Shared);
    }
    let taken = VGA_ENABLE | BUS_RESET;
    let new = (old & !taken) | (value & taken);
    if new != old {
        unsafe { cfg_write16(ops, am, bridge, BRIDGE_CONTROL, new) };
    }
    Ok(new)
}

/// [`bridge_control_write`] for a driver that is a process, which makes
/// each write with a call of its own: a Secondary Bus Reset it asks for is
/// a pulse the kernel ends itself, after `hold`, and the write the driver
/// then takes the bit off with finds it off. A driver that died between
/// its two writes would otherwise leave its device's bus in reset until
/// the next boot, with nobody left who may write that bit. What the
/// register holds afterwards: never the reset.
///
/// `one` is taken around each access to the bridge, and not kept across
/// `hold`.
///
/// # Safety
///
/// `ops` has to reach the configuration space.
pub unsafe fn bridge_control_pulsed<T: PortOps, G>(
    ops: &T,
    am: CSpaceAccessMethod,
    bridge: Location,
    held: Location,
    value: u16,
    one: impl Fn() -> G,
    hold: impl FnOnce(),
) -> Result<u16, Refused> {
    let now = {
        let _one = one();
        unsafe { bridge_control_write(ops, am, bridge, held, value) }?
    };
    if now & BUS_RESET == 0 {
        return Ok(now);
    }
    hold();
    let _one = one();
    unsafe { bridge_control_write(ops, am, bridge, held, now & !BUS_RESET) }
}

/// Write the memory BARs of `loc` again, from what the kernel recorded of
/// them when it scanned the bus (`bars`: each BAR's address, 0 for one that
/// is not a memory BAR). A bus reset clears them, and the BARs are the
/// kernel's to write, so the driver that asked for the reset asks for this
/// once the function answers again. Writing a BAR the value it already
/// holds changes nothing.
///
/// # Safety
///
/// `ops` has to reach the configuration space of `loc`.
pub unsafe fn restore_bars<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    bars: &[usize; 6],
) {
    for (i, &paddr) in bars.iter().enumerate() {
        if paddr == 0 {
            continue;
        }
        let reg = 0x10 + 4 * i as u16;
        unsafe { am.write32(ops, loc, reg, paddr as u32) };
        // The type bits are the device's, read only: a 64-bit BAR takes
        // the upper half of its address in the next register.
        let wide = unsafe { am.read32(ops, loc, reg) } & 0b111 == 0b100;
        if wide && i < 5 {
            unsafe { am.write32(ops, loc, reg + 4, (paddr as u64 >> 32) as u32) };
        }
    }
}

#[cfg(test)]
mod tests;
