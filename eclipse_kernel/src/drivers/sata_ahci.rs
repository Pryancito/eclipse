//! Driver SATA/AHCI para controladoras SATA
//! 
//! Implementa el protocolo AHCI (Advanced Host Controller Interface)
//! para controladoras SATA modernas.

use crate::drivers::block::BlockDevice;
use crate::drivers::pci::PciDevice;
use crate::drivers::framebuffer::FramebufferDriver;
use crate::drivers::framebuffer::Color;
use crate::debug::serial_write_str;
use alloc::string::String;
use alloc::format;

/// Driver SATA/AHCI
pub struct SataAhciDriver {
    pci_device: PciDevice,
    initialized: bool,
    ahci_base: u64,
}

impl SataAhciDriver {
    pub fn new(pci_device: PciDevice) -> Self {
        Self {
            pci_device,
            initialized: false,
            ahci_base: 0,
        }
    }

    pub fn initialize(&mut self) -> Result<(), String> {
        serial_write_str("SATA_AHCI: Inicializando driver SATA/AHCI\n");
        
        serial_write_str(&format!("SATA_AHCI: Dispositivo PCI - Vendor: 0x{:04X}, Device: 0x{:04X}\n",
            self.pci_device.vendor_id, self.pci_device.device_id));
        
        serial_write_str(&format!("SATA_AHCI: Ubicación PCI - Bus: {}, Device: {}, Function: {}\n",
            self.pci_device.bus, self.pci_device.device, self.pci_device.function));
        
        // Leer BAR5 (AHCI Base Address Register)
        let bar5 = self.read_pci_config_u32(0x24);
        serial_write_str(&format!("SATA_AHCI: BAR5 = 0x{:08X}\n", bar5));
        
        // Si BAR5 no está configurado, intentar con BAR0
        if bar5 == 0 {
            serial_write_str("SATA_AHCI: BAR5 no configurado, intentando con BAR0\n");
            let bar0 = self.read_pci_config_u32(0x10);
            serial_write_str(&format!("SATA_AHCI: BAR0 = 0x{:08X}\n", bar0));
            
            if bar0 == 0 {
                return Err(String::from("Ni BAR5 ni BAR0 están configurados"));
            }
            
            // Extraer la dirección base de BAR0 (limpiar bits de control)
            self.ahci_base = (bar0 & 0xFFFFFFF0) as u64;
            serial_write_str(&format!("SATA_AHCI: AHCI Base Address desde BAR0 = 0x{:08X}\n", self.ahci_base));
        } else {
            // Extraer la dirección base de BAR5 (limpiar bits de control)
            self.ahci_base = (bar5 & 0xFFFFFFF0) as u64;
            serial_write_str(&format!("SATA_AHCI: AHCI Base Address desde BAR5 = 0x{:08X}\n", self.ahci_base));
        }
        
        // Leer el registro CAP (Capabilities)
        let cap = self.read_ahci_register(0x00);
        serial_write_str(&format!("SATA_AHCI: CAP = 0x{:08X}\n", cap));
        
        // Verificar que CAP sea válido (no todo ceros o todo unos)
        if cap == 0 || cap == 0xFFFFFFFF {
            return Err(String::from("Registro CAP inválido - controladora AHCI no responde"));
        }
        
        // Leer el registro GHC (Global Host Control)
        let ghc = self.read_ahci_register(0x04);
        serial_write_str(&format!("SATA_AHCI: GHC = 0x{:08X}\n", ghc));
        
        // Leer el registro IS (Interrupt Status)
        let is = self.read_ahci_register(0x08);
        serial_write_str(&format!("SATA_AHCI: IS = 0x{:08X}\n", is));
        
        // Leer el registro PI (Ports Implemented)
        let pi = self.read_ahci_register(0x0C);
        serial_write_str(&format!("SATA_AHCI: PI = 0x{:08X}\n", pi));
        
        // Contar puertos implementados
        let port_count = pi.count_ones() as u32;
        serial_write_str(&format!("SATA_AHCI: {} puertos implementados\n", port_count));
        
        // Verificar puertos activos
        for port in 0..32 {
            if pi & (1 << port) != 0 {
                serial_write_str(&format!("SATA_AHCI: Puerto {} implementado\n", port));
                
                // Leer información del puerto
                let port_ssts = self.read_ahci_register(0x10 + (port * 4));
                let port_sctl = self.read_ahci_register(0x14 + (port * 4));
                let port_serr = self.read_ahci_register(0x18 + (port * 4));
                let port_sact = self.read_ahci_register(0x1C + (port * 4));
                
                serial_write_str(&format!("SATA_AHCI: Puerto {} - SSTS=0x{:08X}, SCTL=0x{:08X}, SERR=0x{:08X}, SACT=0x{:08X}\n",
                    port, port_ssts, port_sctl, port_serr, port_sact));
                
                // Verificar si hay dispositivo conectado
                let device_detected = (port_ssts & 0xF) != 0;
                if device_detected {
                    serial_write_str(&format!("SATA_AHCI: Dispositivo detectado en puerto {}\n", port));
                }
            }
        }
        
        self.initialized = true;
        serial_write_str("SATA_AHCI: Driver SATA/AHCI inicializado exitosamente\n");
        Ok(())
    }

    fn read_pci_config_u32(&self, offset: u8) -> u32 {
        // Implementar lectura directa de configuración PCI
        let address = 0x80000000u32 | 
                     ((self.pci_device.bus as u32) << 16) | 
                     ((self.pci_device.device as u32) << 11) | 
                     ((self.pci_device.function as u32) << 8) | 
                     ((offset as u32) & 0xFC);
        
        unsafe {
            core::arch::asm!("out dx, eax", in("eax") address, in("dx") 0xCF8u16);
            let result: u32;
            core::arch::asm!("in eax, dx", out("eax") result, in("dx") 0xCFCu16);
            result
        }
    }

    fn read_ahci_register(&self, offset: u32) -> u32 {
        // Leer registro AHCI desde la memoria mapeada
        unsafe {
            let ptr = (self.ahci_base + offset as u64) as *const u32;
            core::ptr::read_volatile(ptr)
        }
    }

    fn write_ahci_register(&mut self, offset: u32, value: u32) {
        // Escribir registro AHCI a la memoria mapeada
        unsafe {
            let ptr = (self.ahci_base + offset as u64) as *mut u32;
            core::ptr::write_volatile(ptr, value);
        }
    }

    pub fn is_ready(&self) -> bool {
        self.initialized
    }

    pub fn get_device_info(&self) -> &PciDevice {
        &self.pci_device
    }
}

impl BlockDevice for SataAhciDriver {
    fn read_blocks(&self, start_block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Driver SATA/AHCI no inicializado");
        }

        serial_write_str(&format!("SATA_AHCI: Leyendo {} bytes desde sector {}\n", 
            buffer.len(), start_block));

        // Verificar que el buffer sea múltiplo del tamaño de bloque
        if buffer.len() % 512 != 0 {
            return Err("El tamaño del buffer debe ser múltiplo de 512 bytes");
        }

        let num_blocks = buffer.len() / 512;
        
        // Simular lectura de bloques con datos más realistas
        // TODO: Implementar lectura real usando comandos AHCI DMA
        // Esto requeriría:
        // 1. Configurar Command List y Command Table
        // 2. Construir un FIS de tipo Register H2D con comando READ DMA EXT
        // 3. Configurar PRD (Physical Region Descriptor) apuntando al buffer
        // 4. Escribir en PxCI para iniciar el comando
        // 5. Esperar completación verificando PxCI y PxIS
        
        for block_idx in 0..num_blocks {
            let current_block = start_block + block_idx as u64;
            let block_offset = block_idx * 512;
            let block_buffer = &mut buffer[block_offset..block_offset + 512];
            
            // Simular datos de bloque basados en el número de bloque
            // Sector 0: Boot sector simulado
            if current_block == 0 {
                // Simular MBR o boot sector
                block_buffer[510] = 0x55;
                block_buffer[511] = 0xAA;
                serial_write_str("SATA_AHCI: Sector 0 - Boot sector con firma MBR\n");
            } else {
                // Simular datos con patrón basado en el bloque
                for (i, byte) in block_buffer.iter_mut().enumerate() {
                    *byte = ((current_block as u8).wrapping_add(i as u8)).wrapping_mul(11);
                }
            }
        }

        serial_write_str(&format!("SATA_AHCI: Lectura de {} bloques completada (simulado)\n", num_blocks));
        Ok(())
    }

    fn write_blocks(&mut self, block_address: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Driver SATA/AHCI no inicializado");
        }

        serial_write_str(&format!("SATA_AHCI: Escribiendo {} bytes en sector {}\n", 
            buffer.len(), block_address));

        // Verificar que el buffer sea múltiplo del tamaño de bloque
        if buffer.len() % 512 != 0 {
            return Err("El tamaño del buffer debe ser múltiplo de 512 bytes");
        }

        let num_blocks = buffer.len() / 512;
        
        // TODO: Implementar escritura real usando comandos AHCI DMA
        // Similar a read_blocks pero usando WRITE DMA EXT
        
        serial_write_str(&format!("SATA_AHCI: Escritura de {} bloques completada (simulado)\n", num_blocks));
        Ok(())
    }

    fn block_size(&self) -> u32 {
        512
    }

    fn block_count(&self) -> u64 {
        // TODO: Obtener capacidad real del dispositivo usando IDENTIFY DEVICE
        1000000 // Simular 1M bloques (512 MB)
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
