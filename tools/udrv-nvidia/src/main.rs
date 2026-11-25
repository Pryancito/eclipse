//! `udrv-nvidia`: the kernel's NVIDIA driver (`zcore-drivers`' display
//! module and `nvidia-rm-sys` with NVIDIA's C), linked into a process.
//! What the driver needs from the system it gets through the symbols in
//! `platform`; claiming the GPU and serving `/dev/dri` come in later steps,
//! so for now the process only proves that the driver links and starts.
mod platform;

use zcore_drivers::bus::pci_drivers::PciDriver;

fn main() {
    let drv = zcore_drivers::display::NvidiaGpuDriverPci;
    // Never true; it keeps the whole bring-up path in the link.
    if std::env::args().count() > 99 {
        // SAFETY: never runs.
        let dev: pci::PCIDevice = unsafe { core::mem::zeroed() };
        let _ = drv.init(&dev, &None, None);
    }
    println!("udrv-nvidia: {}", drv.name());
}
