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
    serial_write_str(&format!("VIRTIO: map_mmio_region identidad {:#x} tamaño {:#x}\n", paddr, size));
    Ok(paddr as usize)
}

pub struct PciMmioBridge {
    base: NonNull<u8>,
    cam: Cam,
}

impl PciMmioBridge {
    pub fn new() -> Result<Self, String> {
        serial_write_str("PCI_BRIDGE: Probando diferentes direcciones ECAM\n");
        
        // Probar diferentes direcciones ECAM comunes en QEMU
        // Excluir 0xC000_0000 que es el framebuffer
        let ecam_addresses = [
            0xE000_0000, // ECAM estándar
            0xF000_0000, // ECAM alternativo
            0xE800_0000, // ECAM alternativo 2
            0xF800_0000, // ECAM alternativo 3
            0xD000_0000, // ECAM alternativo 4
            0xD800_0000, // ECAM alternativo 5
            0xE400_0000, // ECAM alternativo 6
            0xF400_0000, // ECAM alternativo 7
            0x9000_0000, // ECAM alternativo 8
            0xA000_0000, // ECAM alternativo 9
            0xB000_0000, // ECAM alternativo 10
            0x8000_0000, // ECAM alternativo 11
            0x8800_0000, // ECAM alternativo 12
            0x9800_0000, // ECAM alternativo 13
            0xA800_0000, // ECAM alternativo 14
            0xB800_0000, // ECAM alternativo 15
            0x7000_0000, // ECAM alternativo 16
            0x7800_0000, // ECAM alternativo 17
            0x6000_0000, // ECAM alternativo 18
            0x6800_0000, // ECAM alternativo 19
            0x5000_0000, // ECAM alternativo 20
        ];
        const ECAM_SIZE: u64 = 0x0100_0000; // 16 MiB cubren bus 0 completo

        for &ecam_base in &ecam_addresses {
            serial_write_str(&format!("PCI_BRIDGE: Probando ECAM en 0x{:08x}\n", ecam_base));
            
            match map_mmio_region(ecam_base, ECAM_SIZE) {
                Ok(base) => {
                    let base_ptr = NonNull::new(base as *mut u8).ok_or_else(|| "MMIO base null".to_string())?;
                    serial_write_str(&format!("PCI_BRIDGE: ECAM mapeado exitosamente en 0x{:08x}\n", base));
                    
                    // Probar una lectura para verificar que funciona
                    let test_value = unsafe { (base_ptr.as_ptr() as *const u32).read_volatile() };
                    serial_write_str(&format!("PCI_BRIDGE: Lectura de prueba en 0x{:08x}: 0x{:08x}\n", base, test_value));
                    
                    // Si la lectura de prueba devuelve ceros, continuar probando otras direcciones
                    if test_value == 0x00000000 {
                        serial_write_str(&format!("PCI_BRIDGE: ECAM en 0x{:08x} devuelve ceros, probando siguiente dirección...\n", ecam_base));
                        continue;
                    }
                    
                    // Si devuelve 0xff000000, probablemente es el framebuffer
                    if test_value == 0xff000000 {
                        serial_write_str(&format!("PCI_BRIDGE: ECAM en 0x{:08x} devuelve 0xff000000 (posible framebuffer), probando siguiente dirección...\n", ecam_base));
                        continue;
                    }
                    
                    // Validar que el Vendor ID sea razonable (no 0xFFFF ni valores sospechosos)
                    let vendor_id = test_value & 0xFFFF;
                    if vendor_id == 0xFFFF || vendor_id == 0x0000 || vendor_id == 0xFF00 {
                        serial_write_str(&format!("PCI_BRIDGE: ECAM en 0x{:08x} devuelve Vendor ID sospechoso (0x{:04x}), probando siguiente dirección...\n", ecam_base, vendor_id));
                        continue;
                    }
                    
                    serial_write_str(&format!("PCI_BRIDGE: ECAM válido encontrado en 0x{:08x} con Vendor ID 0x{:04x}\n", ecam_base, vendor_id));
                    return Ok(Self {
                        base: base_ptr,
                        cam: Cam::Ecam,
                    });
                },
                Err(e) => {
                    serial_write_str(&format!("PCI_BRIDGE: Falló mapeo ECAM en 0x{:08x}: {}\n", ecam_base, e));
                }
            }
        }
        
        Err("No se pudo mapear ninguna dirección ECAM".to_string())
    }
}

impl ConfigurationAccess for PciMmioBridge {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        // Calcular manualmente el offset ECAM
        let manual_offset = (device_function.bus as u32 * 0x100000) + 
                           (device_function.device as u32 * 0x8000) + 
                           (device_function.function as u32 * 0x1000) + 
                           (register_offset as u32 & 0xFC);
        
        let value = unsafe { (self.base.as_ptr().add(manual_offset as usize) as *const u32).read_volatile() };
        
        // Log detallado para diagnosticar lecturas PCI
        serial_write_str(&format!("PCI_ECAM_READ: bus:{} dev:{} func:{} offset:0x{:02x} manual_offset:0x{:08x} base:0x{:016x} final_addr:0x{:016x} value:0x{:08x}\n", 
            device_function.bus, device_function.device, device_function.function, register_offset, 
            manual_offset, self.base.as_ptr() as usize, self.base.as_ptr() as usize + manual_offset as usize, value));
        
        value
    }

    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        // Calcular manualmente el offset ECAM
        let manual_offset = (device_function.bus as u32 * 0x100000) + 
                           (device_function.device as u32 * 0x8000) + 
                           (device_function.function as u32 * 0x1000) + 
                           (register_offset as u32 & 0xFC);
        
        unsafe { (self.base.as_ptr().add(manual_offset as usize) as *mut u32).write_volatile(data) }
    }

    unsafe fn unsafe_clone(&self) -> Self {
        Self {
            base: self.base,
            cam: self.cam,
        }
    }
}
