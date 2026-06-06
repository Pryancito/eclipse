//! Eclipse Pulse signals from device drivers into kernel-hal.

pub const PULSE_HID: u32 = 1 << 0;
pub const PULSE_NET_RX: u32 = 1 << 1;
pub const PULSE_LINK: u32 = 1 << 2;

extern "C" {
    fn drivers_pulse_signal(bits: u32);
}

/// Notify the Pulse reactor that HID, RX, or link work is pending.
#[inline]
pub fn pulse_signal(bits: u32) {
    if bits != 0 {
        unsafe { drivers_pulse_signal(bits) };
    }
}
