//! [`ECLIPSE_DEVICE_PORT`]: the I/O ports of a device left to a driver in
//! userspace, read and written by the kernel for it. A process on x86 has
//! no way of its own to reach a port (it runs with IOPL 0 and no I/O
//! bitmap), and a device like the PC's i8042 has nothing else: so the
//! driver names the port and the kernel does the `in` or the `out`, one
//! system call each. That is fast enough for a keyboard and a mouse, which
//! move a few bytes an interrupt.

use alloc::sync::Arc;
use kernel_hal::drivers::user::{self, Transport};
use zircon_object::task::Process;
use zircon_object::ZxError;

use super::device_claim::{held_by, ECLIPSE_DEVICE_I8042, ECLIPSE_DEVICE_I8042_AUX};

/// `eclipse_device_port(kind, index, offset, width, value, write)`: read
/// (`write` 0) or write (`write` 1) `width` bytes (1, 2 or 4) at port
/// `offset` of the window of device `index` of claim `kind`, which this
/// process must hold (`ECLIPSE_DEVICE_CLAIM`). A read answers the value, a
/// write zero.
///
/// `ACCESS_DENIED` when this process does not hold that device,
/// `NOT_SUPPORTED` for a device that has no ports or a machine with none,
/// `OUT_OF_RANGE` for a port outside its window, `INVALID_ARGS` for
/// anything else.
pub const ECLIPSE_DEVICE_PORT: u32 = super::ECLIPSE_GATEWAY_EXT + 17;

/// [`ECLIPSE_DEVICE_PORT`].
pub(super) fn device_port(proc: &Arc<Process>, args: &[usize; 6]) -> isize {
    match port(proc, args) {
        Ok(value) => value as isize,
        Err(e) => e as isize,
    }
}

fn port(proc: &Arc<Process>, args: &[usize; 6]) -> Result<u32, ZxError> {
    let [kind, index, offset, width, value, write] = *args;
    let kind = match kind {
        ECLIPSE_DEVICE_I8042 => user::I8042,
        ECLIPSE_DEVICE_I8042_AUX => user::I8042_AUX,
        _ => return Err(ZxError::INVALID_ARGS),
    };
    if !matches!(width, 1 | 2 | 4) || write > 1 {
        return Err(ZxError::INVALID_ARGS);
    }
    if held_by(proc, kind) != Some(index) {
        return Err(ZxError::ACCESS_DENIED);
    }
    let device = user::find(kind, index).ok_or(ZxError::NOT_FOUND)?;
    if device.transport != Transport::Ports || !cfg!(target_arch = "x86_64") {
        return Err(ZxError::NOT_SUPPORTED);
    }
    if write == 1 {
        if device.port_out(offset, width, value as u32) {
            Ok(0)
        } else {
            Err(ZxError::OUT_OF_RANGE)
        }
    } else {
        device.port_in(offset, width).ok_or(ZxError::OUT_OF_RANGE)
    }
}
