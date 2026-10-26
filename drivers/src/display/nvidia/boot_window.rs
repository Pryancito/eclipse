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
//! So this module is the whole window in one place: enter it, boot the
//! firmware, drop it. [`BootWindow::enter`] takes the display away and
//! renounces VGA; [`Drop`] gives both back.

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
    /// Whether the display engine was handed over too.
    display_off: bool,
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

        // The piece Linux has and we did not -- and the piece that is NOT
        // equivalent to it, which is why this is off unless asked for:
        // Linux unbinds the firmware framebuffer and later does a full modeset
        // of its own, while all we can undo is the `NV_PMC_ENABLE` write.
        // PDISP (`NV_PMC_ENABLE` bit 30)
        // drops into engine-level reset the moment `kgspCalculateFbLayout` has
        // consumed `NV_PDISP_VGA_WORKSPACE_BASE` — the last thing in the boot
        // that needs the display — and comes back on the RM's own "RISCV
        // started" line, past the wedge window and before GSP-RM's init wants
        // the display engine alive (EXP1/EXP1b timed out in
        // `kgspWaitForRmInitDone` precisely because they left it in reset for
        // the whole run). The monitor loses signal for that stretch. A frozen
        // picture is what it showed before, and that was not safer, only
        // quieter.
        let display_off = super::super::nouveau_uapi::display_off_for_gsp();
        if display_off {
            nvidia_rm_sys::os_boundary::pdisp_kill_arm();
            log.push_str(&alloc::format!(
                "[{}] display engine OFF for the SEC2 window (PDISP reset, restored at 'RISCV started') \
                 -- como el remove_conflicting_framebuffers de Linux; nvidia.nodispoff lo desactiva\n",
                tag
            ));
        }
        Self {
            gpu,
            tag,
            io_cmd_old: Some(io_cmd_old),
            bridges_changed,
            display_off,
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
        if self.display_off {
            // Gives the display back AND cancels an arm that never fired, in
            // that order -- `pdisp_kill_disarm` does both, because doing only
            // the second (which is what this call used to mean) left PDISP in
            // engine reset until reboot on any boot that did not reach its own
            // restore anchor. Leaving this window is the deadline for getting
            // the monitor back; nothing later is going to do it -- and a boot
            // that wedges inside `gsp_boot_run` never leaves it at all, which
            // is one of the reasons the hand-over is opt-in.
            nvidia_rm_sys::os_boundary::pdisp_kill_disarm();
            self.display_off = false;
        }
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
