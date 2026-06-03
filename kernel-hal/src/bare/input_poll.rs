//! Input device poll hook — only used when Eclipse Pulse requests HID tier-C backup.
pub fn poll_input_devices() {
    #[cfg(all(target_arch = "x86_64", not(feature = "no-pci")))]
    {
        #[cfg(feature = "xhci-usb-hid")]
        zcore_drivers::usb::xhci_hid::poll();
    }
}
