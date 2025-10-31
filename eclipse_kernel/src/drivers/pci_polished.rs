//! Driver PCI usando el crate polished_pci
//! 
//! Este driver usa la implementación oficial de polished_pci para Rust,
//! que es más robusta y confiable que nuestra implementación personalizada.

use crate::debug::serial_write_str;
use crate::drivers::block::BlockDevice;
use alloc::{format, vec::Vec, string::String};
use polished_pci::{PciDevice, scan_bus0_devices, pci_enumeration_demo};
use core::ptr::NonNull;

#[derive(Debug, Clone)]
pub struct PolishedPciDriver {
    devices: Vec<PciDevice>,
    initialized: bool,
}

impl PolishedPciDriver {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), String> {
        serial_write_str("POLISHED_PCI: Inicializando driver PCI con polished_pci...\n");
        
        // Ejecutar demo de enumeración PCI (imprime a serial)
        serial_write_str("POLISHED_PCI: Ejecutando demo de enumeración PCI...\n");
        // Ejecutar demo de enumeración PCI
        pci_enumeration_demo();
        serial_write_str("POLISHED_PCI: Demo ejecutado exitosamente\n");
        
        // Escanear dispositivos en bus 0
        serial_write_str("POLISHED_PCI: Escaneando dispositivos en bus 0...\n");
        let devices_result = scan_bus0_devices();
        
        match devices_result {
            Ok(devices) => {
                serial_write_str(&format!("POLISHED_PCI: {} dispositivos PCI encontrados en bus 0\n", devices.len()));
                
                for device in &devices {
                    serial_write_str(&format!("POLISHED_PCI: Dispositivo encontrado - Vendor: 0x{:04X}, Device: 0x{:04X}, Class: 0x{:02X}, Subclass: 0x{:02X}\n",
                        device.vendor_id, device.device_id, device.class, device.subclass));
                    
                    // Buscar dispositivos VirtIO
                    if device.vendor_id == 0x1AF4 {
                        serial_write_str(&format!("POLISHED_PCI: Dispositivo VirtIO encontrado - Device: 0x{:04X}\n", device.device_id));
                        
                        // VirtIO Block Device
                        if device.device_id == 0x1001 {
                            serial_write_str("POLISHED_PCI: VirtIO Block Device detectado\n");
                        }
                    }
                }
                
                self.devices = devices;
                serial_write_str(&format!("POLISHED_PCI: {} dispositivos PCI almacenados\n", self.devices.len()));
                self.initialized = true;
                Ok(())
            }
            Err(e) => {
                let error_msg = format!("Error escaneando dispositivos PCI: {:?}", e);
                serial_write_str(&error_msg);
                Err(error_msg)
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.initialized
    }

    pub fn get_virtio_devices(&self) -> Vec<&PciDevice> {
        self.devices.iter()
            .filter(|device| device.vendor_id == 0x1AF4)
            .collect()
    }

    pub fn get_device_count(&self) -> usize {
        self.devices.len()
    }

    pub fn get_device(&self, index: usize) -> Option<&PciDevice> {
        self.devices.get(index)
    }
}

impl BlockDevice for PolishedPciDriver {
    fn read_blocks(&self, start_block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Driver PCI no inicializado");
        }

        serial_write_str(&format!("POLISHED_PCI: Leyendo {} bytes desde sector {} (simulado)\n", 
            buffer.len(), start_block));
        
        // Por ahora, simular datos válidos
        // TODO: Implementar lectura real cuando tengamos acceso a los dispositivos
        for (i, byte) in buffer.iter_mut().enumerate() {
            *byte = ((start_block as u8).wrapping_add(i as u8)).wrapping_mul(3);
        }

        Ok(())
    }

    fn write_blocks(&mut self, _start_block: u64, _buffer: &[u8]) -> Result<(), &'static str> {
        Err("Escritura no implementada en driver PCI")
    }

    fn block_size(&self) -> u32 {
        512
    }

    fn block_count(&self) -> u64 {
        // Simular un disco de 1GB
        1024 * 1024 * 1024 / 512
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
