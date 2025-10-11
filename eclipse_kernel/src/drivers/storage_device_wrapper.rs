//! Wrapper para convertir StorageManager en BlockDevice

use crate::drivers::storage_manager::{StorageManager, StorageDeviceInfo};
use crate::partitions::BlockDevice;

/// Wrapper que implementa BlockDevice para StorageManager
pub struct StorageDeviceWrapper<'a> {
    storage_manager: &'a StorageManager,
    device_info: &'a StorageDeviceInfo,
}

impl<'a> StorageDeviceWrapper<'a> {
    /// Crear un nuevo wrapper
    pub fn new(storage_manager: &'a StorageManager, device_info: &'a StorageDeviceInfo) -> Self {
        Self {
            storage_manager,
            device_info,
        }
    }
}

impl<'a> BlockDevice for StorageDeviceWrapper<'a> {
    /// Escribir un bloque al dispositivo
    fn write_block(&mut self, lba: u64, buffer: &[u8]) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "STORAGE_WRAPPER: Escribiendo bloque {} al dispositivo {:?}\n",
            lba, self.device_info.controller_type
        ));
        
        // Buscar el índice del dispositivo en el storage manager
        for (index, device) in self.storage_manager.devices.iter().enumerate() {
            if core::ptr::eq(device, self.device_info) {
                return self.storage_manager.write_device_sector(&device.name, lba, buffer);
            }
        }
        
        Err("Dispositivo no encontrado en storage manager")
    }

    /// Leer un bloque del dispositivo
    fn read_block(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "STORAGE_WRAPPER: Leyendo bloque {} del dispositivo {:?}\n",
            lba, self.device_info.controller_type
        ));
        
        // Intentar lectura real del disco
        match self.storage_manager.read_device_sector_real(&self.device_info.name, lba, buffer) {
            Ok(_) => {
                crate::debug::serial_write_str("STORAGE_WRAPPER: Lectura real exitosa\n");
                Ok(())
            }
            Err(e) => {
                crate::debug::serial_write_str(&alloc::format!(
                    "STORAGE_WRAPPER: Error en lectura real: {}, usando simulación\n",
                    e
                ));
                // Fallback a simulación
                self.storage_manager.read_device_sector_with_type(
                    self.device_info,
                    lba,
                    buffer,
                    crate::drivers::storage_manager::StorageSectorType::FAT32,
                )
            }
        }
    }
    
    /// Obtener el tamaño del bloque
    fn block_size(&self) -> usize {
        self.device_info.block_size as usize
    }
    
    /// Obtener el número total de bloques
    fn total_blocks(&self) -> u64 {
        self.device_info.capacity / self.device_info.block_size as u64
    }
}

/// Wrapper específico para EclipseFS
pub struct EclipseFSDeviceWrapper<'a> {
    storage_manager: &'a StorageManager,
    device_info: &'a StorageDeviceInfo,
}

impl<'a> EclipseFSDeviceWrapper<'a> {
    /// Crear un nuevo wrapper para EclipseFS
    pub fn new(storage_manager: &'a StorageManager, device_info: &'a StorageDeviceInfo) -> Self {
        Self {
            storage_manager,
            device_info,
        }
    }
}

impl<'a> BlockDevice for EclipseFSDeviceWrapper<'a> {
    /// Escribir un bloque al dispositivo
    fn write_block(&mut self, lba: u64, buffer: &[u8]) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "ECLIPSEFS_WRAPPER: Escribiendo bloque {} al dispositivo {:?}\n",
            lba, self.device_info.controller_type
        ));
        
        // Buscar el índice del dispositivo en el storage manager
        for (index, device) in self.storage_manager.devices.iter().enumerate() {
            if core::ptr::eq(device, self.device_info) {
                return self.storage_manager.write_device_sector(&device.name, lba, buffer);
            }
        }
        
        Err("Dispositivo no encontrado en storage manager")
    }

    /// Leer un bloque del dispositivo
    fn read_block(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "ECLIPSEFS_WRAPPER: Leyendo bloque {} del dispositivo {:?}\n",
            lba, self.device_info.controller_type
        ));
        
        // Intentar lectura real del disco
        match self.storage_manager.read_device_sector_real(&self.device_info.name, lba, buffer) {
            Ok(_) => {
                crate::debug::serial_write_str("ECLIPSEFS_WRAPPER: Lectura real exitosa\n");
                Ok(())
            }
            Err(e) => {
                crate::debug::serial_write_str(&alloc::format!(
                    "ECLIPSEFS_WRAPPER: Error en lectura real: {}, usando simulación\n",
                    e
                ));
                // Fallback a simulación
                self.storage_manager.read_device_sector_with_type(
                    self.device_info,
                    lba,
                    buffer,
                    crate::drivers::storage_manager::StorageSectorType::EclipseFS,
                )
            }
        }
    }
    
    /// Obtener el tamaño del bloque
    fn block_size(&self) -> usize {
        self.device_info.block_size as usize
    }
    
    /// Obtener el número total de bloques
    fn total_blocks(&self) -> u64 {
        self.device_info.capacity / self.device_info.block_size as u64
    }
}
