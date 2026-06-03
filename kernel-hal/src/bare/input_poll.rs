//! Input device poll hook for interactive I/O wait loops (poll/epoll/select).

/// Drain USB HID transfer rings before network / deferred NIC work.
/// PS/2 keyboard and mouse remain IRQ-driven; xHCI needs periodic backup poll.
pub fn poll_input_devices() {
    #[cfg(all(target_arch = "x86_64", not(feature = "no-pci")))]
    {
        #[cfg(feature = "xhci-usb-hid")]
        zcore_drivers::usb::xhci_hid::poll();
    }
}
