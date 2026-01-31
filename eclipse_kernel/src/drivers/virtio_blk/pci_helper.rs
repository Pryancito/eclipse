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
    serial_write_str(&format!("VIRTIO: map_mmio_region paddr={:#x} tamaño={:#x}\n", paddr, size));
    
    // Validar que paddr no sea 0 (dirección inválida)
    if paddr == 0 {
        serial_write_str("VIRTIO: ERROR - paddr es 0 (dirección inválida)\n");
        return Err("paddr inválido (0)".to_string());
    }
    
    // Validar que size no sea 0
    if size == 0 {
        serial_write_str("VIRTIO: ERROR - size es 0\n");
        return Err("size inválido (0)".to_string());
    }
    
    // Usar mapeo de identidad directo - el kernel tiene mapeo de identidad habilitado
    // Esto significa que la dirección física es la misma que la virtual
    // En Eclipse OS, las primeras 64 GiB de RAM física están mapeadas 1:1
    serial_write_str(&format!("VIRTIO: Usando mapeo de identidad {:#x}\n", paddr));
    
    // Verificar que la región esté en el rango mapeado (0-64 GiB)
    // QEMU típicamente asigna BARs de dispositivos en rangos bajos (< 4 GiB)
    if paddr >= 0x1000000000 { // 64 GiB
        serial_write_str(&format!("VIRTIO: ADVERTENCIA - región fuera del rango mapeado (64 GiB): {:#x}\n", paddr));
        return Err(format!("Región fuera del rango mapeado: {:#x}", paddr));
    }
    
    // Verificar que la región completa esté en el rango
    let end_addr = paddr.checked_add(size).ok_or_else(|| {
        serial_write_str("VIRTIO: ERROR - overflow en cálculo de end_addr\n");
        "Overflow en cálculo de dirección final".to_string()
    })?;
    
    if end_addr > 0x1000000000 {
        serial_write_str(&format!("VIRTIO: ADVERTENCIA - región se extiende fuera del rango: {:#x}-{:#x}\n", paddr, end_addr));
        return Err(format!("Región se extiende fuera del rango: {:#x}-{:#x}", paddr, end_addr));
    }
    
    serial_write_str(&format!("VIRTIO: map_mmio_region exitoso - retornando {:#x}\n", paddr));
    Ok(paddr as usize)
}

pub struct PciMmioBridge {
    base: NonNull<u8>,
    cam: Cam,
}

impl PciMmioBridge {
    pub fn new() -> Result<Self, String> {
        serial_write_str("PCI_BRIDGE: Usando PCI I/O ports (legacy) para compatibilidad con QEMU\n");
        
        // En lugar de usar ECAM, vamos a usar PCI I/O ports que son más compatibles
        // Esto es lo que usa el kernel de Linux cuando ECAM no está disponible
        serial_write_str("PCI_BRIDGE: Configurando PCI I/O bridge legacy\n");
        
        // Crear un bridge "falso" que usa I/O ports
        // Usaremos una dirección dummy para el base pointer
        let dummy_base = NonNull::new(0x1000 as *mut u8).ok_or_else(|| "Dummy base null".to_string())?;
        
        serial_write_str("PCI_BRIDGE: PCI I/O bridge configurado exitosamente\n");
        
        Ok(Self {
            base: dummy_base,
            cam: Cam::Ecam, // Usar ECAM (pero implementaremos I/O ports)
        })
    }
}

impl ConfigurationAccess for PciMmioBridge {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        // Usar PCI I/O ports (método legacy) en lugar de ECAM
        // Esto es más compatible con QEMU
        
        // Configurar la dirección PCI en el registro CONFIG_ADDRESS (0xCF8)
        let config_address = 0x80000000 | 
                           ((device_function.bus as u32) << 16) |
                           ((device_function.device as u32) << 11) |
                           ((device_function.function as u32) << 8) |
                           ((register_offset as u32) & 0xFC);
        
        // Escribir la dirección en CONFIG_ADDRESS
        unsafe {
            outl(0xCF8, config_address);
        }
        
        // Leer el valor desde CONFIG_DATA (0xCFC)
        let value = unsafe { inl(0xCFC) };
        
        // Log detallado para diagnosticar lecturas PCI
        serial_write_str(&format!("PCI_IOPORT_READ: bus:{} dev:{} func:{} offset:0x{:02x} config_addr:0x{:08x} value:0x{:08x}\n", 
            device_function.bus, device_function.device, device_function.function, register_offset, 
            config_address, value));
        
        value
    }

    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        // Usar PCI I/O ports (método legacy) en lugar de ECAM
        
        // Configurar la dirección PCI en el registro CONFIG_ADDRESS (0xCF8)
        let config_address = 0x80000000 | 
                           ((device_function.bus as u32) << 16) |
                           ((device_function.device as u32) << 11) |
                           ((device_function.function as u32) << 8) |
                           ((register_offset as u32) & 0xFC);
        
        // Escribir la dirección en CONFIG_ADDRESS
        unsafe {
            outl(0xCF8, config_address);
        }
        
        // Escribir el valor en CONFIG_DATA (0xCFC)
        unsafe {
            outl(0xCFC, data);
        }
        
        serial_write_str(&format!("PCI_IOPORT_WRITE: bus:{} dev:{} func:{} offset:0x{:02x} config_addr:0x{:08x} data:0x{:08x}\n", 
            device_function.bus, device_function.device, device_function.function, register_offset, 
            config_address, data));
    }

    unsafe fn unsafe_clone(&self) -> Self {
        Self {
            base: self.base,
            cam: self.cam,
        }
    }
}
