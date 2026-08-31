//! Functions only available on x86 platforms.

pub use zcore_drivers::io::{Io, Pmio};

/// Get physical address of `acpi_rsdp` and `smbios` on x86_64.
pub fn pc_firmware_tables() -> (u64, u64) {
    (crate::KCONFIG.acpi_rsdp, crate::KCONFIG.smbios)
}

/// Describe how the CURRENT context (this CPU, this CR3) maps the boot
/// framebuffer: leaf PTE, selected PAT index, effective memory type. See
/// `pat::fb_mapping_diag` — this is the present-blit "is it really WC?"
/// ground truth, klogged once at first present.
pub fn fb_mapping_diag() -> alloc::string::String {
    super::pat::fb_mapping_diag()
}

/// Retype the boot framebuffer's physmap PTEs in the CURRENT page-table tree
/// to write-combining (idempotent; no-op when they already are). The boot-time
/// passes edit the tree current at *that* moment; if a process tree ends up
/// with its own UC copies of those PTEs, calling this from that process's
/// context converts them in place.
pub fn ensure_framebuffer_wc() {
    super::pat::enable_framebuffer_wc()
}
