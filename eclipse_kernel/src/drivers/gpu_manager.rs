//! Gestor de Drivers de GPU para Eclipse OS
//! 
//! Coordina la carga y gestión de drivers específicos de GPU
//! basados en la detección PCI.

use crate::drivers::pci::{PciDevice, GpuInfo, GpuType};
use crate::drivers::intel_graphics::{IntelGraphicsDriver, IntelDriverState, create_intel_driver};
use crate::drivers::nvidia_graphics::{NvidiaGraphicsDriver, NvidiaDriverState, create_nvidia_driver};
use crate::drivers::amd_graphics::{AmdGraphicsDriver, AmdDriverState, create_amd_driver};
use crate::drivers::framebuffer::FramebufferDriver;
use alloc::format;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;

/// Resultado de la carga de driver
#[derive(Debug, Clone)]
pub enum DriverLoadResult {
    Success,
    Unsupported,
    Error(String),
}

/// Información del driver cargado
#[derive(Debug, Clone)]
pub struct LoadedDriver {
    pub gpu_type: GpuType,
    pub driver_name: &'static str,
    pub is_ready: bool,
    pub supports_2d: bool,
    pub supports_3d: bool,
    pub memory_size: u64,
}

/// Gestor de drivers de GPU
#[derive(Debug, Clone)]
pub struct GpuDriverManager {
    intel_drivers: [Option<IntelGraphicsDriver>; 4],
    intel_count: usize,
    nvidia_drivers: [Option<NvidiaGraphicsDriver>; 4],
    nvidia_count: usize,
    amd_drivers: [Option<AmdGraphicsDriver>; 4],
    amd_count: usize,
    loaded_drivers: [Option<LoadedDriver>; 8],
    driver_count: usize,
}

impl GpuDriverManager {
    pub fn new() -> Self {
        Self {
            intel_drivers: [(); 4].map(|_| None),
            intel_count: 0,
            nvidia_drivers: [(); 4].map(|_| None),
            nvidia_count: 0,
            amd_drivers: [(); 4].map(|_| None),
            amd_count: 0,
            loaded_drivers: [(); 8].map(|_| None),
            driver_count: 0,
        }
    }
    
    /// Cargar drivers para GPUs detectadas
    pub fn load_drivers_for_gpus(&mut self, gpus: &[GpuInfo]) -> Result<usize, &'static str> {
        let mut loaded_count = 0;
        
        for gpu in gpus {
            match gpu.gpu_type {
                GpuType::Intel => {
                    if let Ok(()) = self.load_intel_driver(gpu) {
                        loaded_count += 1;
                    }
                },
                GpuType::Nvidia => {
                    if let Ok(()) = self.load_nvidia_driver(gpu) {
                        loaded_count += 1;
                    }
                },
                GpuType::Amd => {
                    if let Ok(()) = self.load_amd_driver(gpu) {
                        loaded_count += 1;
                    }
                },
                _ => {
                    // Driver genérico
                    self.add_loaded_driver(LoadedDriver {
                        gpu_type: gpu.gpu_type,
                        driver_name: "Generic Driver",
                        is_ready: false,
                        supports_2d: gpu.supports_2d,
                        supports_3d: gpu.supports_3d,
                        memory_size: gpu.memory_size,
                    });
                }
            }
        }
        
        Ok(loaded_count)
    }
    
    /// Cargar driver Intel real
    fn load_intel_driver(&mut self, gpu: &GpuInfo) -> Result<(), &'static str> {
        if self.intel_count >= self.intel_drivers.len() {
            return Err("Demasiados drivers Intel");
        }
        
        // Verificar que es una GPU Intel real
        if !self.is_real_intel_gpu(gpu) {
            return Err("GPU Intel no válida");
        }
        
        // Crear driver Intel real
        let mut driver = self.create_real_intel_driver(gpu)?;
        
        // Inicializar driver real
        driver.initialize()?;
        
        // Agregar a la lista
        self.intel_drivers[self.intel_count] = Some(driver);
        self.intel_count += 1;
        
        // Agregar información del driver cargado
        self.add_loaded_driver(LoadedDriver {
            gpu_type: GpuType::Intel,
            driver_name: "Intel Graphics Driver",
            is_ready: true,
            supports_2d: gpu.supports_2d,
            supports_3d: gpu.supports_3d,
            memory_size: gpu.memory_size,
        });
        
        Ok(())
    }
    
    /// Cargar driver NVIDIA
    fn load_nvidia_driver(&mut self, gpu: &GpuInfo) -> Result<(), &'static str> {
        if self.nvidia_count >= self.nvidia_drivers.len() {
            return Err("Demasiados drivers NVIDIA");
        }
        
        // Crear driver NVIDIA
        let mut driver = create_nvidia_driver(gpu.pci_device, gpu.clone());
        
        // Inicializar driver
        driver.init(None)?;
        
        // Agregar a la lista
        self.nvidia_drivers[self.nvidia_count] = Some(driver);
        self.nvidia_count += 1;
        
        // Agregar información del driver cargado
        self.add_loaded_driver(LoadedDriver {
            gpu_type: GpuType::Nvidia,
            driver_name: "NVIDIA Graphics Driver",
            is_ready: true,
            supports_2d: gpu.supports_2d,
            supports_3d: gpu.supports_3d,
            memory_size: gpu.memory_size,
        });
        
        Ok(())
    }

    /// Cargar driver AMD
    fn load_amd_driver(&mut self, gpu: &GpuInfo) -> Result<(), &'static str> {
        if self.amd_count >= self.amd_drivers.len() {
            return Err("Demasiados drivers AMD");
        }
        
        // Crear driver AMD
        let mut driver = create_amd_driver(gpu.pci_device, gpu.clone());
        
        // Inicializar driver
        driver.init(None)?;
        
        // Agregar a la lista
        self.amd_drivers[self.amd_count] = Some(driver);
        self.amd_count += 1;
        
        // Agregar información del driver cargado
        self.add_loaded_driver(LoadedDriver {
            gpu_type: GpuType::Amd,
            driver_name: "AMD Graphics Driver",
            is_ready: true,
            supports_2d: gpu.supports_2d,
            supports_3d: gpu.supports_3d,
            memory_size: gpu.memory_size,
        });
        
        Ok(())
    }
    
    /// Agregar driver cargado
    fn add_loaded_driver(&mut self, driver: LoadedDriver) {
        if self.driver_count < self.loaded_drivers.len() {
            self.loaded_drivers[self.driver_count] = Some(driver);
            self.driver_count += 1;
        }
    }
    
    /// Obtener driver Intel por índice
    pub fn get_intel_driver(&mut self, index: usize) -> Option<&mut IntelGraphicsDriver> {
        self.intel_drivers.get_mut(index)?.as_mut()
    }
    
    /// Obtener todos los drivers Intel
    pub fn get_intel_drivers(&mut self) -> &mut [Option<IntelGraphicsDriver>] {
        &mut self.intel_drivers[..self.intel_count]
    }
    
    /// Obtener driver NVIDIA por índice
    pub fn get_nvidia_driver(&mut self, index: usize) -> Option<&mut NvidiaGraphicsDriver> {
        self.nvidia_drivers.get_mut(index)?.as_mut()
    }
    
    /// Obtener todos los drivers NVIDIA
    pub fn get_nvidia_drivers(&mut self) -> &mut [Option<NvidiaGraphicsDriver>] {
        &mut self.nvidia_drivers[..self.nvidia_count]
    }

    /// Obtener driver AMD por índice
    pub fn get_amd_driver(&mut self, index: usize) -> Option<&mut AmdGraphicsDriver> {
        self.amd_drivers.get_mut(index)?.as_mut()
    }

    /// Obtener todos los drivers AMD
    pub fn get_amd_drivers(&mut self) -> &mut [Option<AmdGraphicsDriver>] {
        &mut self.amd_drivers[..self.amd_count]
    }
    
    /// Obtener drivers cargados
    pub fn get_loaded_drivers(&self) -> &[Option<LoadedDriver>] {
        &self.loaded_drivers[..self.driver_count]
    }
    
    /// Obtener número de drivers cargados
    pub fn get_driver_count(&self) -> usize {
        self.driver_count
    }
    
    /// Obtener número de drivers Intel
    pub fn get_intel_count(&self) -> usize {
        self.intel_count
    }

    /// Obtener número de drivers NVIDIA
    pub fn get_nvidia_count(&self) -> usize {
        self.nvidia_count
    }

    /// Obtener número de drivers AMD
    pub fn get_amd_count(&self) -> usize {
        self.amd_count
    }
    
    /// Obtener driver primario (primera GPU)
    pub fn get_primary_driver(&mut self) -> Option<&mut IntelGraphicsDriver> {
        self.get_intel_driver(0)
    }
    
    /// Obtener framebuffer del driver primario
    pub fn get_primary_framebuffer(&mut self) -> Option<&mut FramebufferDriver> {
        self.get_primary_driver()?.get_framebuffer()
    }
    
    /// Verificar si hay drivers listos
    pub fn has_ready_drivers(&self) -> bool {
        self.loaded_drivers.iter()
            .filter_map(|d| d.as_ref())
            .any(|d| d.is_ready)
    }
    
    /// Obtener información de todos los drivers
    pub fn get_driver_info(&self) -> Vec<String> {
        let mut info = Vec::new();
        
        // Drivers Intel
        for (i, driver) in self.intel_drivers.iter().enumerate() {
            if let Some(driver) = driver {
                let state_str = match driver.get_state() {
                    IntelDriverState::Ready => "Listo",
                    IntelDriverState::Initializing => "Inicializando",
                    IntelDriverState::Error => "Error",
                    IntelDriverState::Suspended => "Suspendido",
                    IntelDriverState::Uninitialized => "No inicializado",
                };
                
                let gen_info = driver.get_info();
                info.push(format!(
                    "Intel GPU {}: {} {} - {} - Memoria: {}MB - Estado: {}",
                    i + 1,
                    gen_info.generation.as_str(),
                    format!("{:04X}", gen_info.device_id),
                    if gen_info.supports_2d { "2D" } else { "" },
                    gen_info.memory_size / (1024 * 1024),
                    state_str
                ));
            }
        }
        
        // Drivers NVIDIA
        for (i, driver) in self.nvidia_drivers.iter().enumerate() {
            if let Some(driver) = driver {
                let state_str = match driver.state {
                    NvidiaDriverState::Ready => "Listo",
                    NvidiaDriverState::Initializing => "Inicializando",
                    NvidiaDriverState::Error => "Error",
                    NvidiaDriverState::Suspended => "Suspendido",
                    NvidiaDriverState::Uninitialized => "No inicializado",
                };
                
                let nvidia_info = driver.get_info();
                info.push(format!(
                    "NVIDIA GPU {}: {} {} - {} - Memoria: {}MB - Estado: {}",
                    i + 1,
                    nvidia_info.generation.as_str(),
                    format!("{:04X}", nvidia_info.device_id),
                    if nvidia_info.supports_2d { "2D" } else { "" },
                    nvidia_info.memory_size / (1024 * 1024),
                    state_str
                ));
            }
        }

        // Drivers AMD
        for (i, driver) in self.amd_drivers.iter().enumerate() {
            if let Some(driver) = driver {
                let state_str = match driver.state {
                    AmdDriverState::Ready => "Listo",
                    AmdDriverState::Initializing => "Inicializando",
                    AmdDriverState::Error => "Error",
                    AmdDriverState::Suspended => "Suspendido",
                    AmdDriverState::Uninitialized => "No inicializado",
                };
                
                let amd_info = driver.get_info();
                info.push(format!(
                    "AMD GPU {}: {} {} - {} - Memoria: {}MB - Estado: {}",
                    i + 1,
                    amd_info.generation.as_str(),
                    format!("{:04X}", amd_info.device_id),
                    if amd_info.supports_2d { "2D" } else { "" },
                    amd_info.memory_size / (1024 * 1024),
                    state_str
                ));
            }
        }
        
        // Otros drivers
        for driver in &self.loaded_drivers[..self.driver_count] {
            if let Some(driver) = driver {
                if driver.gpu_type != GpuType::Intel && driver.gpu_type != GpuType::Nvidia && driver.gpu_type != GpuType::Amd {
                    info.push(format!(
                        "{}: {} - {} - Memoria: {}MB - Estado: {}",
                        driver.gpu_type.as_str(),
                        driver.driver_name,
                        if driver.supports_2d { "2D" } else { "" },
                        driver.memory_size / (1024 * 1024),
                        if driver.is_ready { "Listo" } else { "No implementado" }
                    ));
                }
            }
        }
        
        if info.is_empty() {
            info.push("No se cargaron drivers de GPU".to_string());
        }
        
        info
    }
    
    /// Inicializar todos los drivers
    pub fn initialize_all_drivers(&mut self) -> Result<usize, &'static str> {
        let mut initialized_count = 0;
        
        // Inicializar drivers Intel
        for driver in &mut self.intel_drivers[..self.intel_count] {
            if let Some(driver) = driver {
                if driver.initialize().is_ok() {
                    initialized_count += 1;
                }
            }
        }

        // Inicializar drivers NVIDIA
        for driver in &mut self.nvidia_drivers[..self.nvidia_count] {
            if let Some(driver) = driver {
                if driver.init(None).is_ok() {
                    initialized_count += 1;
                }
            }
        }

        // Inicializar drivers AMD
        for driver in &mut self.amd_drivers[..self.amd_count] {
            if let Some(driver) = driver {
                if driver.init(None).is_ok() {
                    initialized_count += 1;
                }
            }
        }
        
        Ok(initialized_count)
    }
    
    /// Obtener estadísticas de drivers
    pub fn get_driver_stats(&self) -> (usize, usize, usize) {
        let total = self.driver_count;
        let ready = self.loaded_drivers.iter()
            .filter_map(|d| d.as_ref())
            .filter(|d| d.is_ready)
            .count();
        let intel_ready = self.intel_drivers.iter()
            .filter_map(|d| d.as_ref())
            .filter(|d| d.is_ready())
            .count();
        let nvidia_ready = self.nvidia_drivers.iter()
            .filter_map(|d| d.as_ref())
            .filter(|d| d.is_ready())
            .count();
        let amd_ready = self.amd_drivers.iter()
            .filter_map(|d| d.as_ref())
            .filter(|d| d.is_ready())
            .count();
        
        (total, ready, intel_ready + nvidia_ready + amd_ready)
    }
    
    /// Verificar si es una GPU Intel real
    fn is_real_intel_gpu(&self, gpu: &GpuInfo) -> bool {
        // Verificar vendor ID de Intel (0x8086)
        gpu.pci_device.vendor_id == 0x8086 && gpu.pci_device.class_code == 0x03
    }
    
    /// Verificar si es una GPU NVIDIA real
    fn is_real_nvidia_gpu(&self, gpu: &GpuInfo) -> bool {
        // Verificar vendor ID de NVIDIA (0x10DE)
        gpu.pci_device.vendor_id == 0x10DE && gpu.pci_device.class_code == 0x03
    }
    
    /// Verificar si es una GPU AMD real
    fn is_real_amd_gpu(&self, gpu: &GpuInfo) -> bool {
        // Verificar vendor ID de AMD (0x1002)
        gpu.pci_device.vendor_id == 0x1002 && gpu.pci_device.class_code == 0x03
    }
    
    /// Verificar si es una GPU real (no simulada)
    fn is_real_gpu(&self, gpu: &GpuInfo) -> bool {
        // Verificar que tiene vendor ID válido y clase VGA
        gpu.pci_device.class_code == 0x03 && gpu.pci_device.vendor_id != 0x1234
    }
    
    /// Crear driver Intel real
    fn create_real_intel_driver(&self, gpu: &GpuInfo) -> Result<IntelGraphicsDriver, &'static str> {
        // Crear driver Intel con hardware real
        let driver = IntelGraphicsDriver::new(gpu.pci_device);
        
        // Verificar que el hardware está disponible (simulado)
        if gpu.pci_device.vendor_id != 0x8086 {
            return Err("Hardware Intel no disponible");
        }
        
        Ok(driver)
    }
    
    /// Crear driver NVIDIA real
    fn create_real_nvidia_driver(&self, gpu: &GpuInfo) -> Result<NvidiaGraphicsDriver, &'static str> {
        // Crear driver NVIDIA con hardware real
        let driver = NvidiaGraphicsDriver::new(gpu.pci_device, gpu.clone());
        
        // Verificar que el hardware está disponible (simulado)
        if gpu.pci_device.vendor_id != 0x10DE {
            return Err("Hardware NVIDIA no disponible");
        }
        
        Ok(driver)
    }
    
    /// Crear driver AMD real
    fn create_real_amd_driver(&self, gpu: &GpuInfo) -> Result<AmdGraphicsDriver, &'static str> {
        // Crear driver AMD con hardware real
        let driver = AmdGraphicsDriver::new(gpu.pci_device, gpu.clone());
        
        // Verificar que el hardware está disponible (simulado)
        if gpu.pci_device.vendor_id != 0x1002 {
            return Err("Hardware AMD no disponible");
        }
        
        Ok(driver)
    }
}

/// Función de conveniencia para crear gestor de drivers
pub fn create_gpu_driver_manager() -> GpuDriverManager {
    GpuDriverManager::new()
}
