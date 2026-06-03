/// Poll input devices once for wait-loop responsiveness.
#[inline]
pub fn poll_input_devices() {
    #[cfg(all(target_arch = "x86_64", not(feature = "no-pci")))]
    {
        zcore_drivers::usb::xhci_hid::poll();
    }
}
