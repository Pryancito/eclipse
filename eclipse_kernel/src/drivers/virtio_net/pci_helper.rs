use crate::drivers::pci::PciDevice;
use crate::drivers::pci::{inl, outl};
use crate::debug::serial_write_str;
use alloc::format;
use alloc::string::{String, ToString};
use core::ptr::NonNull;
use virtio_drivers_and_devices::transport::pci::bus::{self, Cam, ConfigurationAccess, DeviceFunction, PciRoot};

pub struct PciRootAdapter {
    root: PciRoot<PciMmioBridge>,
}

impl PciRootAdapter {
    pub fn new(_device: PciDevice) -> Result<Self, String> {
        let bridge = PciMmioBridge::new()?;
        Ok(Self {
            root: PciRoot::new(bridge),
        })
    }

    pub fn root_mut(&mut self) -> &mut PciRoot<PciMmioBridge> {
        &mut self.root
    }
}

pub fn device_function(device: PciDevice) -> DeviceFunction {
    DeviceFunction {
        bus: device.bus,
        device: device.device,
        function: device.function,
    }
}

pub fn map_mmio_region(paddr: u64, size: u64) -> Result<usize, String> {
    serial_write_str(&format!("VIRTIO_NET: map_mmio_region identidad {:#x} tamaño {:#x}\n", paddr, size));
    
    // Usar mapeo de identidad directo - el kernel tiene mapeo de identidad habilitado
    // Esto significa que la dirección física es la misma que la virtual
    
    // Verificar que la región esté en el rango mapeado (0-64 GiB)
    if paddr >= 0x1000000000 { // 64 GiB
        serial_write_str("VIRTIO_NET: ADVERTENCIA - región fuera del rango mapeado (64 GiB)\n");
        return Err("Región fuera del rango mapeado".to_string());
    }
    
    Ok(paddr as usize)
}

pub struct PciMmioBridge {
    base: NonNull<u8>,
    cam: Cam,
}

impl PciMmioBridge {
    pub fn new() -> Result<Self, String> {
        serial_write_str("PCI_BRIDGE_NET: Usando PCI I/O ports (legacy) para compatibilidad con QEMU\n");
        
        let dummy_base = NonNull::new(0x1000 as *mut u8).ok_or_else(|| "Dummy base null".to_string())?;
        
        Ok(Self {
            base: dummy_base,
            cam: Cam::Ecam, // Usar ECAM (pero implementaremos I/O ports)
        })
    }
}

impl ConfigurationAccess for PciMmioBridge {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        // Usar PCI I/O ports (método legacy) en lugar de ECAM
        
        let config_address = 0x80000000 | 
                           ((device_function.bus as u32) << 16) |
                           ((device_function.device as u32) << 11) |
                           ((device_function.function as u32) << 8) |
                           ((register_offset as u32) & 0xFC);
        
        unsafe {
            outl(0xCF8, config_address);
        }
        
        let value = unsafe { inl(0xCFC) };
        
        // Reducir log para evitar spam en red
        if register_offset != 0 {
             // serial_write_str(&format!("PCI_NET_READ: bus:{} dev:{} func:{} offset:0x{:02x} val:0x{:08x}\n", 
             //   device_function.bus, device_function.device, device_function.function, register_offset, value));
        }
        
        value
    }

    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        let config_address = 0x80000000 | 
                           ((device_function.bus as u32) << 16) |
                           ((device_function.device as u32) << 11) |
                           ((device_function.function as u32) << 8) |
                           ((register_offset as u32) & 0xFC);
        
        unsafe {
            outl(0xCF8, config_address);
            outl(0xCFC, data);
        }
        
        // serial_write_str(&format!("PCI_NET_WRITE: bus:{} dev:{} func:{} offset:0x{:02x} val:0x{:08x}\n", 
        //    device_function.bus, device_function.device, device_function.function, register_offset, data));
    }

    unsafe fn unsafe_clone(&self) -> Self {
        Self {
            base: self.base,
            cam: self.cam,
        }
    }
}
