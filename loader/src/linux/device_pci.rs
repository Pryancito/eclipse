//! [`ECLIPSE_DEVICE_PCI_FN`]: the rest of a PCI function left to a driver
//! in userspace, beyond the window its claim hands over. A GPU has more
//! than one BAR that matters (an NVIDIA's registers in BAR 0, its video
//! memory aperture in BAR 1, its instance memory in BAR 2/3), and its
//! driver reads and writes the function's configuration space (power
//! states, MSI, PCI Express capabilities). A process cannot reach that
//! space itself (on x86 it sits behind ports 0xCF8/0xCFC, shared by every
//! device on the machine), so the kernel does each access for it, for that
//! function only, and never on the registers that place the function: its
//! identity, BARs and ROM (`kernel_hal::drivers::user::PciPlace`).

use alloc::sync::Arc;
use kernel_hal::drivers::user::ConfigRefused;
use kernel_hal::{CachePolicy, PAGE_SIZE};
use zircon_object::object::{Handle, Rights};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::ZxError;

use super::device_claim::{held_device, kind_of};

/// `eclipse_device_pci_fn(kind, index, op, a, b, c)`, on device `index` of
/// claim `kind` (with the function's identity, `vendor << 16 | device`, in
/// the high 32 bits of `kind` for one named by its identity), which this
/// process must hold:
///
/// - [`OP_BAR`]: a handle to a VMO of BAR `a`'s memory, uncached, or
///   write-combining with `b` = 1 (an aperture of video memory). Its
///   rights are a VMO's.
/// - [`OP_CONFIG_READ`]: `b` bytes (1, 2 or 4) of the configuration space
///   at `a`, aligned to them, in the first 256 bytes.
/// - [`OP_CONFIG_WRITE`]: the low `b` bytes of `c` there; zero.
/// - [`OP_PLACE`]: where the function is, `bus << 8 | device << 3 |
///   function`, as a driver names it to the code it runs.
/// - [`OP_BAR_LEN`]: the length of memory BAR `a`, 0 for a BAR that is
///   not one (an I/O BAR, the upper half of a 64-bit one, none). A driver
///   cannot size a BAR itself, by writing all ones to it, as the kernel
///   did when it scanned the bus: the BARs are the kernel's to write.
///
/// `ACCESS_DENIED` when this process does not hold that device, or for a
/// write to a register only the kernel sets; `NOT_SUPPORTED` for a device
/// that is not a PCI function the kernel recorded the place of, or a
/// configuration space this machine does not reach; `OUT_OF_RANGE` for a
/// BAR that is not memory or an offset outside the space; `INVALID_ARGS`
/// for anything else.
pub const ECLIPSE_DEVICE_PCI_FN: u32 = super::ECLIPSE_GATEWAY_EXT + 21;

/// [`ECLIPSE_DEVICE_PCI_FN`] operations.
pub const OP_BAR: usize = 0;
pub const OP_CONFIG_READ: usize = 1;
pub const OP_CONFIG_WRITE: usize = 2;
pub const OP_PLACE: usize = 3;
pub const OP_BAR_LEN: usize = 4;

/// [`ECLIPSE_DEVICE_PCI_FN`].
pub(super) fn device_pci_fn(proc: &Arc<Process>, args: &[usize; 6]) -> isize {
    match pci_fn(proc, args) {
        Ok(value) => value as isize,
        Err(e) => e as isize,
    }
}

fn pci_fn(proc: &Arc<Process>, args: &[usize; 6]) -> Result<usize, ZxError> {
    let [kind, index, op, a, b, c] = *args;
    let (number, pci_id) = (kind & 0xffff_ffff, (kind >> 32) as u32);
    let kind = kind_of(number).ok_or(ZxError::INVALID_ARGS)?;
    let device = held_device(proc, kind, index, pci_id).ok_or(ZxError::ACCESS_DENIED)?;
    let place = device.pci.ok_or(ZxError::NOT_SUPPORTED)?;
    match op {
        OP_BAR => {
            let &(paddr, len) = place.bars.get(a).ok_or(ZxError::OUT_OF_RANGE)?;
            if paddr == 0 || len == 0 || !paddr.is_multiple_of(PAGE_SIZE) || b > 1 {
                return Err(if b > 1 {
                    ZxError::INVALID_ARGS
                } else {
                    ZxError::OUT_OF_RANGE
                });
            }
            let vmo = VmObject::new_physical(paddr, len.div_ceil(PAGE_SIZE));
            vmo.set_cache_policy(if b == 1 {
                CachePolicy::WriteCombining
            } else {
                CachePolicy::UncachedDevice
            })?;
            Ok(proc.add_handle(Handle::new(vmo, Rights::DEFAULT_VMO)) as usize)
        }
        OP_CONFIG_READ => {
            place
                .config_read(a, b)
                .map(|v| v as usize)
                .ok_or(if cfg!(target_arch = "x86_64") {
                    ZxError::OUT_OF_RANGE
                } else {
                    ZxError::NOT_SUPPORTED
                })
        }
        OP_CONFIG_WRITE => {
            let value: u32 =
                core::convert::TryFrom::try_from(c).map_err(|_| ZxError::INVALID_ARGS)?;
            match place.config_write(a, b, value) {
                Ok(()) => Ok(0),
                Err(ConfigRefused::Range) => Err(ZxError::OUT_OF_RANGE),
                Err(ConfigRefused::Kernel) => Err(ZxError::ACCESS_DENIED),
                Err(ConfigRefused::NotHere) => Err(ZxError::NOT_SUPPORTED),
            }
        }
        OP_PLACE => {
            Ok((place.bus as usize) << 8 | (place.device as usize) << 3 | place.function as usize)
        }
        OP_BAR_LEN => place
            .bars
            .get(a)
            .map(|&(paddr, len)| if paddr == 0 { 0 } else { len })
            .ok_or(ZxError::OUT_OF_RANGE),
        _ => Err(ZxError::INVALID_ARGS),
    }
}
