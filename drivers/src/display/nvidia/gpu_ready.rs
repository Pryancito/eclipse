//! Is the console GPU usable by a GL/Vulkan client yet?
//!
//! Own file for the size ceiling of `nvidia.rs`; the whole answer lives here.

use super::NvidiaGpu;
use alloc::string::String;

/// `None` when this GPU does not drive the boot display, so `/proc/gpuready`
/// can treat "nobody answered" as "nothing to wait for". Otherwise whether a
/// client could use the card, and one line saying why not.
pub(super) fn report(gpu: &NvidiaGpu) -> Option<(bool, String)> {
    if !gpu.drives_boot_display() {
        return None;
    }
    // Read-only on purpose: a boot polls this in a loop. `gr_ready` is a
    // read of a C global and takes no RM lock -- it is one of the handful
    // of entry points `nvidia-rm-sys/tests/rm_gate.rs` names as ungated,
    // and that is exactly why it can be asked this often.
    let (rm, gr) = match *gpu.rm_device_instance.lock() {
        Some(d) => (true, nvidia_rm_sys::rm_init::gr_ready(d)),
        None => (false, false),
    };
    Some((
        rm && gr,
        alloc::format!(
            "{:02x}:{:02x}.0 {}: rm={} gr={}",
            gpu.pci_bus,
            gpu.pci_device,
            gpu.gpu_model,
            if rm { "yes" } else { "no" },
            if gr { "yes" } else { "no" },
        ),
    ))
}
