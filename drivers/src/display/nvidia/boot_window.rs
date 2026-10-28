//! Handing the card over to its own firmware, the way Linux does it.
//!
//! Linux's NVIDIA probe prepares the device before `kgspInitRm` and then lets
//! the firmware boot into a quiet machine:
//!
//! * `drm_aperture_remove_conflicting_pci_framebuffers` — the firmware
//!   framebuffer driver that owns the card's aperture is unbound and
//!   destroyed, so nothing is scanning out of this GPU any more;
//! * `vga_tryget` + `vga_set_legacy_decoding(VGA_RSRC_NONE)` (nv-pci.c:855)
//!   — legacy VGA decode is renounced for the function and the bridges above
//!   it.
//!
//! Eclipse had the second and not the first. "Pausing scanout" is a software
//! flag: it stops the CPU drawing and stops flips, and the display engine goes
//! on fetching the same framebuffer out of BAR1 at 60 Hz for the whole GSP
//! boot. That isochronous fetch across the SEC2-RTOS HS resume is the wedge,
//! and every parche layered over it — console silence, PBUS unit clear, VGA
//! decode off, MPS normalization, Linux byte-parity — left it in place and
//! failed. What shipped instead was an empirical register bracket that
//! survived 2 boots out of 3, which is what losing the monitor on some boots
//! and not others looks like from the outside.
//!
//! Taking the display away (PDISP into reset across the SEC2 window) was
//! tried as an opt-in and cost the monitor for the whole session: an
//! engine-level reset wipes the scanout state the VBIOS programmed, and
//! nothing here reprograms it. The boot survives without it, so it is gone.
//!
//! So this module is the window in one place: enter it, boot the firmware,
//! drop it. [`BootWindow::enter`] renounces VGA and I/O decode; [`Drop`]
//! gives both back.

use super::NvidiaGpu;
use alloc::string::String;
use alloc::vec::Vec;

/// The card prepared for a firmware boot. Restores everything when dropped,
/// including on the panic path — the previous shape of this code restored by
/// falling off the end of a long function, so an early return would have left
/// the GPU with its I/O decode off and no display.
pub(super) struct BootWindow<'a> {
    gpu: &'a NvidiaGpu,
    tag: &'static str,
    /// PCI COMMAND as found, restored on drop. `None` once given back, so
    /// `leave` followed by `Drop` does not write it a second time.
    io_cmd_old: Option<u16>,
    /// Bridges whose VGA routing this window cleared.
    bridges_changed: Vec<(u8, u8, u8, u16)>,
}

impl<'a> BootWindow<'a> {
    /// Prepare `gpu` for its own GSP boot and narrate what was done.
    pub(super) fn enter(gpu: &'a NvidiaGpu, tag: &'static str, log: &mut String) -> Self {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        use pci::Location;
        let loc = Location {
            bus: gpu.pci_bus,
            device: gpu.pci_device,
            function: 0,
        };
        let ops = &PortOpsImpl;
        let io_cmd_old = unsafe { PCI_ACCESS.read16(ops, loc, 0x04) };
        unsafe { PCI_ACCESS.write16(ops, loc, 0x04, io_cmd_old & !0x0001) };
        log.push_str(&alloc::format!(
            "[{}] PCI I/O decode disabled for the boot (COMMAND {:#06x} -> {:#06x})\n",
            tag,
            io_cmd_old,
            io_cmd_old & !0x0001
        ));
        let (bridge_log, bridges_changed) = gpu.set_path_vga_routing(true, &[]);
        log.push_str(&bridge_log);

        Self {
            gpu,
            tag,
            io_cmd_old: Some(io_cmd_old),
            bridges_changed,
        }
    }

    /// Give the card back and narrate it. Separate from `Drop` so the report
    /// carries the restore lines; `Drop` alone cannot write into the caller's
    /// `String`, and this chain's whole output is that `String`.
    pub(super) fn leave(mut self, log: &mut String) {
        log.push_str(&self.restore());
    }

    fn restore(&mut self) -> String {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        use pci::Location;
        let mut s = String::new();
        let (restore_log, _) = self.gpu.set_path_vga_routing(false, &self.bridges_changed);
        s.push_str(&restore_log);
        self.bridges_changed.clear();
        if let Some(io_cmd_old) = self.io_cmd_old.take() {
            let loc = Location {
                bus: self.gpu.pci_bus,
                device: self.gpu.pci_device,
                function: 0,
            };
            // The original COMMAND, except INTx stays masked: `gsp_boot_run`
            // masked it and Eclipse is fully polled.
            unsafe {
                PCI_ACCESS.write16(&PortOpsImpl, loc, 0x04, io_cmd_old | (1 << 10));
            }
            s.push_str(&alloc::format!(
                "[{}] PCI I/O decode restored after boot\n",
                self.tag
            ));
        }
        s
    }
}

impl Drop for BootWindow<'_> {
    fn drop(&mut self) {
        // Already done by `leave` on the normal path; this is the unwind and
        // early-return path, where losing the restore costs the console.
        let _ = self.restore();
    }
}
