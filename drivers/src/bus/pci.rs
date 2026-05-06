use super::{phys_to_virt, PAGE_SIZE};
use crate::builder::IoMapper;
use crate::scheme::SchemeUpcast;
use crate::{Device, DeviceError, DeviceResult};
use alloc::{format, sync::Arc, vec::Vec};
use pci::*;

const PCI_COMMAND: u16 = 0x04;
const BAR0: u16 = 0x10;
const BAR5_REG: u16 = 0x24;
const PCI_CAP_PTR: u16 = 0x34;
const _PCI_INTERRUPT_LINE: u16 = 0x3c;
const _PCI_INTERRUPT_PIN: u16 = 0x3d;

const PCI_MSI_CTRL_CAP: u16 = 0x00;
const PCI_MSI_ADDR: u16 = 0x04;
const _PCI_MSI_UPPER_ADDR: u16 = 0x08;
const PCI_MSI_DATA_32: u16 = 0x08;
const PCI_MSI_DATA_64: u16 = 0x0C;

// const PCI_COMMAND_INTX_DISABLE:u16 = 0x400;

const PCI_CAP_ID_MSI: u8 = 0x05;

struct PortOpsImpl;

#[cfg(target_arch = "x86_64")]
use x86_64::instructions::port::Port;

#[cfg(target_arch = "x86_64")]
impl PortOps for PortOpsImpl {
    unsafe fn read8(&self, port: u16) -> u8 {
        Port::new(port).read()
    }
    unsafe fn read16(&self, port: u16) -> u16 {
        Port::new(port).read()
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        Port::new(port as u16).read()
    }
    unsafe fn write8(&self, port: u16, val: u8) {
        Port::new(port).write(val);
    }
    unsafe fn write16(&self, port: u16, val: u16) {
        Port::new(port).write(val);
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        Port::new(port as u16).write(val);
    }
}

#[cfg(target_arch = "x86_64")]
const PCI_BASE: usize = 0; //Fix me

#[cfg(any(target_arch = "mips", target_arch = "riscv64"))]
use super::{read, write};

#[cfg(feature = "board_malta")]
const PCI_BASE: usize = 0xbbe00000;

#[cfg(target_arch = "riscv64")]
const PCI_BASE: usize = 0x30000000;
#[cfg(target_arch = "riscv64")]
const E1000_BASE: usize = 0x40000000;
// riscv64 Qemu

#[cfg(target_arch = "x86_64")]
const PCI_ACCESS: CSpaceAccessMethod = CSpaceAccessMethod::IO;
#[cfg(not(target_arch = "x86_64"))]
const PCI_ACCESS: CSpaceAccessMethod = CSpaceAccessMethod::MemoryMapped(PCI_BASE as *mut u8);

#[cfg(any(target_arch = "mips", target_arch = "riscv64"))]
impl PortOps for PortOpsImpl {
    unsafe fn read8(&self, port: u16) -> u8 {
        read(phys_to_virt(PCI_BASE) + port as usize)
    }
    unsafe fn read16(&self, port: u16) -> u16 {
        read(phys_to_virt(PCI_BASE) + port as usize)
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        read(phys_to_virt(PCI_BASE) + port as usize)
    }
    unsafe fn write8(&self, port: u16, val: u8) {
        write(phys_to_virt(PCI_BASE) + port as usize, val);
    }
    unsafe fn write16(&self, port: u16, val: u16) {
        write(phys_to_virt(PCI_BASE) + port as usize, val);
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        write(phys_to_virt(PCI_BASE) + port as usize, val);
    }
}

/// Enable the pci device and its interrupt
/// Return assigned MSI interrupt number when applicable
unsafe fn enable(loc: Location, paddr: u64) -> Option<usize> {
    let ops = &PortOpsImpl;
    //let am = CSpaceAccessMethod::IO;
    let am = PCI_ACCESS;

    if paddr != 0 {
        // reveal PCI regs by setting paddr
        let bar0_raw = am.read32(ops, loc, BAR0);
        am.write32(ops, loc, BAR0, (paddr & !0xfff) as u32); //Only for 32-bit decoding
        warn!(
            "BAR0 set from {:#x} to {:#x}",
            bar0_raw,
            am.read32(ops, loc, BAR0)
        );
    }

    // 23 and lower are used
    static mut MSI_IRQ: u32 = 23;

    let orig = am.read16(ops, loc, PCI_COMMAND);
    // Always enable MEM space + Bus Mastering so DMA devices (e.g. AHCI) work
    // regardless of whether MSI is available.
    am.write16(ops, loc, PCI_COMMAND, orig | 0x6);

    // find MSI cap
    let mut msi_found = false;
    let mut cap_ptr = am.read8(ops, loc, PCI_CAP_PTR) as u16;
    let mut assigned_irq = None;
    while cap_ptr > 0 {
        let cap_id = am.read8(ops, loc, cap_ptr);
        if cap_id == PCI_CAP_ID_MSI {
            let orig_ctrl = am.read32(ops, loc, cap_ptr + PCI_MSI_CTRL_CAP);
            // The manual Volume 3 Chapter 10.11 Message Signalled Interrupts
            // 0 is (usually) the apic id of the bsp.
            //am.write32(ops, loc, cap_ptr + PCI_MSI_ADDR, 0xfee00000 | (0 << 12));
            am.write32(ops, loc, cap_ptr + PCI_MSI_ADDR, 0xfee00000);
            MSI_IRQ += 1;
            let irq = MSI_IRQ;
            assigned_irq = Some(irq as usize);
            // we offset all our irq numbers by 32
            if (orig_ctrl >> 16) & (1 << 7) != 0 {
                // 64bit
                am.write32(ops, loc, cap_ptr + PCI_MSI_DATA_64, irq + 32);
            } else {
                // 32bit
                am.write32(ops, loc, cap_ptr + PCI_MSI_DATA_32, irq + 32);
            }

            // enable MSI interrupt, assuming 64bit for now
            am.write32(ops, loc, cap_ptr + PCI_MSI_CTRL_CAP, orig_ctrl | 0x10000);
            debug!(
                "MSI control {:#b}, enabling MSI interrupt {}",
                orig_ctrl >> 16,
                irq
            );
            msi_found = true;
        }
        debug!("PCI device has cap id {} at {:#X}", cap_id, cap_ptr);
        cap_ptr = am.read8(ops, loc, cap_ptr + 1) as u16;
    }

    if !msi_found {
        am.write32(ops, loc, _PCI_INTERRUPT_LINE, 33);
        debug!("MSI not found, using PCI interrupt");
    }

    warn!("pci device enable done");

    assigned_irq
}

pub fn init_driver(dev: &PCIDevice, mapper: &Option<Arc<dyn IoMapper>>) -> DeviceResult<Device> {
    let name = format!("enp{}s{}f{}", dev.loc.bus, dev.loc.device, dev.loc.function);
    match (dev.id.vendor_id, dev.id.device_id) {
        (0x8086, 0x100e) | (0x8086, 0x100f) | (0x8086, 0x10d3) => {
            if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[0] {
                #[cfg(target_arch = "riscv64")]
                let addr = if addr == 0 { E1000_BASE as u64 } else { addr };

                if let Some(m) = mapper {
                    m.query_or_map(addr as usize, PAGE_SIZE * 8);
                }
                let irq = unsafe { enable(dev.loc, addr) };
                let vaddr = phys_to_virt(addr as usize);
                let dev = Device::Net(Arc::new(crate::net::e1000::init(
                    name,
                    irq.unwrap_or(0),
                    vaddr,
                    len as usize,
                    0,
                )?));
                return Ok(dev);
            }
        }

        (0x1b36, 0x10) => {
            if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[0] {
                #[cfg(target_arch = "riscv64")]
                let addr = if addr == 0 { E1000_BASE as u64 } else { addr };

                if let Some(m) = mapper {
                    m.query_or_map(addr as usize, PAGE_SIZE * 8);
                }

                let irq = unsafe { enable(dev.loc, addr) };
                let vaddr = phys_to_virt(addr as usize);

                let blk = Arc::new(crate::nvme::NvmeInterface::new(vaddr, irq.unwrap_or(33))?);

                let dev = Device::Block(blk);
                return Ok(dev);
            }
        }
        (0x8086, 0x10fb) => {
            // 82599ES 10-Gigabit SFI/SFP+ Network Connection
            if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[0] {
                let irq = unsafe { enable(dev.loc, 0) };
                let vaddr = phys_to_virt(addr as usize);
                info!("Found ixgbe dev {:#x}, irq: {:?}", vaddr, irq);
                /*
                let index = NET_DRIVERS.read().len();
                PCI_DRIVERS.lock().insert(
                    dev.loc,
                    ixgbe::ixgbe_init(name, irq, vaddr, len as usize, index),
                );
                */
                return Err(DeviceError::NotSupported);
            }
        }
        (0x8086, 0x1533) => {
            if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[0] {
                info!("Intel Corporation I210 Gigabit Network Connection");
                info!("DEV: {:?}, BAR0: {:#x}", dev, addr);
                return Err(DeviceError::NotSupported);
            }
        }
        (0x8086, 0x1539) => {
            if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[0] {
                info!(
                    "Found Intel I211 ethernet controller dev {:?}, addr: {:x?}",
                    dev, addr
                );
                return Err(DeviceError::NotSupported);
            }
        }
        (0x10de, _) if dev.id.class == 0x03 => {
            // NVIDIA GPU
            if let Some(BAR::Memory(bar0_addr, _bar0_len, _, _)) = dev.bars[0] {
                // Map BAR0 first so we can probe the display registers for resolution
                if let Some(m) = mapper {
                    m.query_or_map(bar0_addr as usize, PAGE_SIZE * 1024); // 4 MiB for regs
                }
                let bar0_vaddr = phys_to_virt(bar0_addr as usize);

                // For modern NVIDIA GPUs, BAR0 is a 64-bit BAR and occupies PCI
                // BARs 0+1, so bars[1] is None.  The framebuffer (BAR2) is at
                // bars[2].  Older GPUs with a 32-bit BAR0 have the FB at bars[1].
                // Scan bars[1..6] and pick the first large (≥16 MiB) memory BAR.
                let fb_bar = (1..6usize).find_map(|i| {
                    if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[i] {
                        if addr != 0 && (len as u64) >= (16 * 1024 * 1024) {
                            Some((addr, len))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

                if let Some((fb_addr, fb_len)) = fb_bar {
                    if let Some(m) = mapper {
                        m.query_or_map(fb_addr as usize, fb_len as usize);
                    }
                    let fb_vaddr = phys_to_virt(fb_addr as usize);

                    // Unique name includes PCI bus:device.function
                    let gpu_name = format!(
                        "nvidia-gpu-{}:{}.{}",
                        dev.loc.bus, dev.loc.device, dev.loc.function
                    );
                    warn!(
                        "[NVIDIA] GPU at {} bar0={:#x} fb={:#x} fb_len={:#x}",
                        gpu_name, bar0_addr, fb_addr, fb_len
                    );
                    let gpu = Arc::new(crate::display::NvidiaGpu::new(
                        gpu_name,
                        bar0_vaddr,
                        fb_vaddr,
                        fb_len as usize,
                        1920,
                        1080,
                    )?);
                    return Ok(Device::Drm(gpu));
                }
            }
        }
        (0x1af4, 0x1050) => {
            // VirtIO GPU
            warn!("VirtIO GPU found!");
            
            #[cfg(feature = "virtio")]
            {
                let ops = &PortOpsImpl;
                let am = PCI_ACCESS;
                let mut cap_ptr = unsafe { am.read8(ops, dev.loc, 0x34) } as u16;
                
                let mut common_cfg = None;
                let mut device_cfg = None;
                let mut notify_cfg = None;
                
                while cap_ptr > 0 {
                    let cap_id = unsafe { am.read8(ops, dev.loc, cap_ptr) };
                    if cap_id == 0x09 { // Vendor Specific
                        let cfg_type = unsafe { am.read8(ops, dev.loc, cap_ptr + 3) };
                        let bar = unsafe { am.read8(ops, dev.loc, cap_ptr + 4) };
                        let offset = unsafe { am.read32(ops, dev.loc, cap_ptr + 8) };
                        let length = unsafe { am.read32(ops, dev.loc, cap_ptr + 12) };
                        
                        match cfg_type {
                            1 => common_cfg = Some((bar, offset, length)),
                            2 => notify_cfg = Some((bar, offset, length)),
                            4 => device_cfg = Some((bar, offset, length)),
                            _ => {}
                        }
                        warn!("VirtIO Cap: type={}, bar={}, offset={:#x}, len={}", cfg_type, bar, offset, length);
                    }
                    cap_ptr = unsafe { am.read8(ops, dev.loc, cap_ptr + 1) } as u16;
                }

                if let Some((bar, offset, _len)) = common_cfg {
                    if let Some(BAR::Memory(addr, bar_len, _, _)) = dev.bars[bar as usize] {
                        // Map the entire BAR to avoid overlapping mappings for different capabilities
                        if let Some(m) = mapper {
                            m.query_or_map(addr as usize, bar_len as usize);
                        }
                        let common_vaddr = phys_to_virt(addr as usize + offset as usize);
                        
                        let device_vaddr = if let Some((d_bar, d_offset, _)) = device_cfg {
                            if let Some(BAR::Memory(d_addr, d_len, _, _)) = dev.bars[d_bar as usize] {
                                if d_bar != bar {
                                    if let Some(m) = mapper {
                                        m.query_or_map(d_addr as usize, d_len as usize);
                                    }
                                }
                                phys_to_virt(d_addr as usize + d_offset as usize)
                            } else { 0 }
                        } else { 0 };
                        
                        let notify_vaddr = if let Some((n_bar, n_offset, _)) = notify_cfg {
                            if let Some(BAR::Memory(n_addr, n_len, _, _)) = dev.bars[n_bar as usize] {
                                if n_bar != bar && n_bar != (device_cfg.map(|(b, _, _)| b).unwrap_or(255)) {
                                    if let Some(m) = mapper {
                                        m.query_or_map(n_addr as usize, n_len as usize);
                                    }
                                }
                                phys_to_virt(n_addr as usize + n_offset as usize)
                            } else { 0 }
                        } else { 0 };

                        let (fb_vaddr, fb_size) = if let Some(BAR::Memory(fb_addr, fb_len, _, _)) = dev.bars[0] {
                            if let Some(m) = mapper {
                                m.query_or_map(fb_addr as usize, fb_len as usize);
                            }
                            (phys_to_virt(fb_addr as usize), fb_len as usize)
                        } else { (0, 0) };

                        match crate::virtio::VirtIoGpu::new_modern(common_vaddr, device_vaddr, notify_vaddr, fb_vaddr, fb_size) {
                            Ok(gpu) => {
                                warn!("VirtIO Modern GPU initialized successfully!");
                                return Ok(Device::Drm(Arc::new(gpu)));
                            },
                            Err(e) => warn!("VirtIO Modern GPU init failed: {:?}", e),
                        }
                    }
                }
                
                // Fallback to legacy if no modern caps found or failed
                if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[0] {
                    if let Some(m) = mapper {
                        m.query_or_map(addr as usize, len as usize);
                    }
                    let vaddr = phys_to_virt(addr as usize);
                    let header = unsafe { &mut *(vaddr as *mut crate::virtio::VirtIOHeader) };
                    if let Ok(gpu) = crate::virtio::VirtIoGpu::new(header) {
                        return Ok(Device::Drm(Arc::new(gpu)));
                    }
                }
            }
        }
        _ => {}
    }

    #[cfg(feature = "xhci-usb-hid")]
    {
        if dev.id.class == 0x0c && dev.id.subclass == 0x03 && dev.id.prog_if == 0x30 {
            if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[0] {
                if addr == 0 {
                    warn!("xHCI: BAR0 es 0 (recursos no asignados por el firmware); omitido");
                    return Err(DeviceError::NotSupported);
                }
                
                warn!("[xhci] PCI BAR0 detectado en Phys:{:#x}, Len:{:#x}", addr, len);
                
                // Alineación a página (4KB)
                let base_addr = (addr as usize) & !0xfff;
                let offset = (addr as usize) & 0xfff;
                // Forzamos un mapeo de al menos 128KB para asegurar que CAP, OP y RT estén cubiertos
                let map_len = ((len as usize + offset + 0xfff) & !0xfff).max(128 * 1024);

                if let Some(m) = mapper {
                    warn!("[xhci] Solicitando mapeo kernel: [{:#x} - {:#x}]", base_addr, base_addr + map_len);
                    m.query_or_map(base_addr, map_len);
                } else {
                    warn!("[xhci] CRÍTICO: No hay IoMapper disponible. El acceso a {:#x} causará un Page Fault si no está pre-mapeado.", base_addr);
                }
                
                let vaddr = phys_to_virt(addr as usize);
                warn!("[xhci] VAddr mapeado: {:#x}. Intentando acceder a registros...", vaddr);

                let msi_idx = unsafe { enable(dev.loc, 0) };
                if let Some(idx) = msi_idx {
                    let vector = idx + 32;
                    warn!("[xhci] Usando MSI (vector {})", vector);
                    match crate::usb::xhci_hid::XhciUsbHid::probe(dev, vaddr, len as usize, vector) {
                        Ok(input) => {
                            crate::usb::xhci_hid::pci_note_pending_msi(vector, input.clone().upcast());
                            return Ok(Device::Input(input));
                        }
                        Err(e) => warn!("xHCI HID probe error: {:?}", e),
                    }
                } else {
                    warn!("[xhci] MSI no disponible; iniciando modo polling (vector 0)");
                    match crate::usb::xhci_hid::XhciUsbHid::probe(dev, vaddr, len as usize, 0) {
                        Ok(input) => {
                            crate::usb::xhci_hid::set_poll_instance(Some(input.clone()));
                            return Ok(Device::Input(input));
                        }
                        Err(e) => warn!("xHCI HID probe (poll) error: {:?}", e),
                    }
                }
            }
        }
    }

    if dev.id.class == 0x01 && dev.id.subclass == 0x06 {
        // Mass storage class - SATA AHCI
        // Ignore the potentially stale BAR info from PCIDevice and read RAW BAR5
        let ops = &PortOpsImpl;
        let am = PCI_ACCESS;
        let raw_bar5 = unsafe { am.read32(ops, dev.loc, BAR5_REG) };
        if raw_bar5 != 0 && raw_bar5 != 0xFFFF_FFFF {
            let addr = (raw_bar5 & !0xF) as u64; // Mask out flags
            warn!("[AHCI] Using RAW BAR5 address: {:#x}", addr);

            // Map the ABAR registers (at least one full page)
            let base_addr = (addr as usize) & !0xfff;
            let map_len = 4096; // 4KB is enough for AHCI registers

            if let Some(m) = mapper {
                warn!("[AHCI] Solicitando mapeo kernel: [{:#x} - {:#x}]", base_addr, base_addr + map_len);
                m.query_or_map(base_addr, map_len);
            }

            let irq = unsafe { enable(dev.loc, 0) };
            let vaddr = phys_to_virt(addr as usize);
            let blk = Arc::new(crate::ata::ahci::AhciInterface::new(vaddr, irq.unwrap_or(33))?);
            return Ok(Device::Block(blk));
        } else {
            warn!("AHCI dev found but RAW BAR5 is invalid: {:#x}", raw_bar5);
        }
    }

    Err(DeviceError::NoResources)
}

pub fn detach_driver(_loc: &Location) -> bool {
    false
}

pub fn init(mapper: Option<Arc<dyn IoMapper>>) -> DeviceResult<Vec<Device>> {
    let _mapper_driver = if let Some(m) = mapper.clone() {
        m.query_or_map(PCI_BASE, PAGE_SIZE * 256 * 32 * 8);
        Some(m)
    } else {
        None
    };

    let mut dev_list = Vec::new();
    let pci_iter = unsafe { scan_bus(&PortOpsImpl, PCI_ACCESS) };
    info!("");
    info!("--------- PCI bus:device:function ---------");
    for dev in pci_iter {
        info!(
            "pci: {}:{}:{} {:04x}:{:04x} ({} {}) irq: {}:{:?}",
            dev.loc.bus,
            dev.loc.device,
            dev.loc.function,
            dev.id.vendor_id,
            dev.id.device_id,
            dev.id.class,
            dev.id.subclass,
            dev.pic_interrupt_line,
            dev.interrupt_pin,
        );
        let res = init_driver(&dev, &mapper);
        match res {
            Ok(d) => dev_list.push(d),
            Err(e) => warn!(
                "{:?}, failed to initialize PCI device: {:04x}:{:04x}",
                e, dev.id.vendor_id, dev.id.device_id
            ),
        }
    }
    info!("---------");
    info!("");

    Ok(dev_list)
}

pub fn find_device(vendor: u16, product: u16) -> Option<Location> {
    let pci_iter = unsafe { scan_bus(&PortOpsImpl, PCI_ACCESS) };
    for dev in pci_iter {
        if dev.id.vendor_id == vendor && dev.id.device_id == product {
            return Some(dev.loc);
        }
    }
    None
}

pub fn get_bar0_mem(loc: Location) -> Option<(usize, usize)> {
    unsafe { probe_function(&PortOpsImpl, loc, PCI_ACCESS) }
        .and_then(|dev| dev.bars[0])
        .map(|bar| match bar {
            BAR::Memory(addr, len, _, _ ) => (addr as usize, len as usize),
            _ => unimplemented!(),
        })
}
