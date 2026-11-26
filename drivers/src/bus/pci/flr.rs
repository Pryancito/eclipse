//! PCIe Function Level Reset, by the configuration space alone: for a
//! function whose driver is not the kernel's (a GPU `udrv-nvidia` drives)
//! and that has to be left cold before a warm reboot. A GPU with a live
//! GSP-RM keeps its WPR2 locked across a warm reset, which does not
//! power-cycle PCIe, and the next firmware POST stalls on it.
//!
//! The same steps the NVIDIA driver takes on its own GPUs
//! (`NvidiaGpu::pcie_flr`): the function says it can, its pending
//! transactions drain (bounded), Initiate FLR, and the 100 ms the spec
//! gives the function before it may be touched again. Its configuration
//! is not put back: the machine resets right after.

use super::cfgwrite::cfg_write16;
use pci::{CSpaceAccessMethod, Location, PortOps};

const CAP_ID_PCIE: u8 = 0x10;
/// Status register bit 4: there is a capabilities list.
const STATUS_CAP_LIST: u16 = 1 << 4;
/// Device Capabilities (PCIe cap + 4) bit 28: FLR capable.
const DEVCAP_FLR: u32 = 1 << 28;
/// Device Control (PCIe cap + 8) bit 15: Initiate FLR.
const DEVCTL_FLR: u16 = 1 << 15;
/// Device Status (PCIe cap + 0xA) bit 5: Transactions Pending.
const DEVSTA_PENDING: u16 = 1 << 5;
/// How long pending transactions are waited for, and how long the
/// function is left alone after the reset, in microseconds.
const DRAIN_US: u64 = 100_000;
const SETTLE_US: u64 = 100_000;
/// A broken list is not walked for ever: 48 is every capability that fits.
const MAX_CAP_STEPS: usize = 48;

/// Where the PCI Express capability of `loc` is.
///
/// # Safety
///
/// `ops` has to reach the configuration space of `loc`.
pub unsafe fn pcie_cap<T: PortOps>(ops: &T, am: CSpaceAccessMethod, loc: Location) -> Option<u16> {
    if unsafe { am.read16(ops, loc, 0x06) } & STATUS_CAP_LIST == 0 {
        return None;
    }
    let mut ptr = (unsafe { am.read8(ops, loc, 0x34) } & 0xFC) as u16;
    for _ in 0..MAX_CAP_STEPS {
        if ptr < 0x40 {
            return None;
        }
        let header = unsafe { am.read16(ops, loc, ptr) };
        if header as u8 == CAP_ID_PCIE {
            return Some(ptr);
        }
        ptr = (header >> 8) & 0xFC;
    }
    None
}

/// Reset `loc` (FLR). `now_us` is the clock the waits are timed by.
/// Whether it was reset: `false` for a function with no PCI Express
/// capability, one that cannot FLR, or one that is gone (all ones).
///
/// # Safety
///
/// `ops` has to reach the configuration space of `loc`, and nothing may be
/// using the function: it forgets everything.
pub unsafe fn flr<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    now_us: impl Fn() -> u64,
) -> bool {
    if unsafe { am.read32(ops, loc, 0) } == u32::MAX {
        return false;
    }
    let Some(cap) = (unsafe { pcie_cap(ops, am, loc) }) else {
        return false;
    };
    if unsafe { am.read32(ops, loc, cap + 0x04) } & DEVCAP_FLR == 0 {
        return false;
    }
    let t0 = now_us();
    while unsafe { am.read16(ops, loc, cap + 0x0A) } & DEVSTA_PENDING != 0 {
        if now_us().wrapping_sub(t0) > DRAIN_US {
            break;
        }
        core::hint::spin_loop();
    }
    let devctl = unsafe { am.read16(ops, loc, cap + 0x08) };
    unsafe { cfg_write16(ops, am, loc, cap + 0x08, devctl | DEVCTL_FLR) };
    let t1 = now_us();
    while now_us().wrapping_sub(t1) < SETTLE_US {
        core::hint::spin_loop();
    }
    true
}

#[cfg(test)]
mod tests;
