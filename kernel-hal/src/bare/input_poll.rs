//! Input device poll hook for I/O wait loops and stdin read().
pub fn poll_input_devices() {
    #[cfg(all(target_arch = "x86_64", not(feature = "no-pci")))]
    {
        #[cfg(feature = "xhci-usb-hid")]
        {
            crate::kstats::note_hid_poll_iowait(); // [diag]
            zcore_drivers::usb::xhci_hid::poll();
        }
    }
}
