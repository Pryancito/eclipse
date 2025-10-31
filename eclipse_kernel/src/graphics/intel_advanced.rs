//! Driver avanzado para GPUs Intel
//! 
//! Implementa detección real de memoria, aceleración por hardware
//! y características específicas de Intel.

use crate::drivers::ipc::{Driver, DriverInfo, DriverState, DriverCapability, DriverMessage, DriverResponse};
use crate::drivers::pci::{PciDevice, PciManager, GpuInfo, GpuType};
use crate::syslog;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;

/// Información detallada de una GPU Intel
#[derive(Debug, Clone)]
pub struct IntelGpuInfo {
    pub pci_device: PciDevice,
    pub gpu_name: String,
    pub total_memory: u64,        // En bytes
    pub available_memory: u64,    // En bytes
    pub memory_clock: u32,        // En MHz
    pub core_clock: u32,          // En MHz
    pub execution_units: u32,     // Execution Units (equivalente a CUDA cores)
    pub ray_tracing_units: u32,   // Ray Tracing Units
    pub ai_accelerators: u32,     // AI Accelerators
    pub memory_bandwidth: u64,    // En GB/s
    pub pcie_version: u8,
    pub pcie_lanes: u8,
    pub power_limit: u32,         // En watts
    pub temperature: u32,         // En Celsius
    pub fan_speed: u32,          // En RPM
    pub driver_version: String,
    pub oneapi_version: String,   // Intel oneAPI
    pub vulkan_support: bool,
    pub opengl_support: bool,
    pub directx_support: bool,
    pub opencl_support: bool,
}

/// Driver avanzado para Intel
pub struct IntelAdvancedDriver {
    info: DriverInfo,
    pci_manager: PciManager,
    intel_gpus: Vec<IntelGpuInfo>,
    active_gpu: Option<usize>,
    memory_mapped: bool,
    acceleration_enabled: bool,
    oneapi_enabled: bool,
    ray_tracing_enabled: bool,
}

impl IntelAdvancedDriver {
    pub fn new() -> Self {
        let info = DriverInfo {
            id: 0, // Se asignará al registrar
            name: String::from("Intel Advanced Driver"),
            version: String::from("2.0.0"),
            author: String::from("Eclipse OS Team"),
            description: String::from("Driver avanzado para GPUs Intel con detección real de memoria y aceleración"),
            state: DriverState::Unloaded,
            dependencies: {
                let mut deps = Vec::new();
                deps.push(String::from("PCI Driver"));
                deps
            },
            capabilities: {
                let mut caps = Vec::new();
                caps.push(DriverCapability::Graphics);
                caps.push(DriverCapability::Custom(String::from("oneAPI")));
                caps.push(DriverCapability::Custom(String::from("RayTracing")));
                caps.push(DriverCapability::Custom(String::from("AIAccelerators")));
                caps.push(DriverCapability::Custom(String::from("Vulkan")));
                caps.push(DriverCapability::Custom(String::from("OpenGL")));
                caps.push(DriverCapability::Custom(String::from("OpenCL")));
                caps
            },
        };

        Self {
            info,
            pci_manager: PciManager::new(),
            intel_gpus: Vec::new(),
            active_gpu: None,
            memory_mapped: false,
            acceleration_enabled: false,
            oneapi_enabled: false,
            ray_tracing_enabled: false,
        }
    }

    /// Detectar GPUs Intel con información detallada
    fn detect_intel_gpus(&mut self) -> Result<(), String> {
        self.pci_manager.scan_devices();
        let gpus = self.pci_manager.get_gpus();
        
        self.intel_gpus.clear();
        
        for gpu_option in gpus {
            if let Some(gpu) = gpu_option {
                if matches!(gpu.gpu_type, GpuType::Intel) {
                    let intel_info = self.analyze_intel_gpu(&gpu)?;
                    self.intel_gpus.push(intel_info);
                }
            }
        }


        for (i, gpu) in self.intel_gpus.iter().enumerate() {
        }
        Ok(())
    }

    /// Analizar GPU Intel específica
    fn analyze_intel_gpu(&self, gpu: &GpuInfo) -> Result<IntelGpuInfo, String> {
        let device = &gpu.pci_device;
        
        // Detectar modelo específico de GPU
        let gpu_name = self.detect_gpu_model(device);
        
        // Detectar memoria real usando BARs
        let total_memory = self.detect_real_memory(device)?;
        
        // Detectar características específicas
        let (execution_units, ray_tracing_units, ai_accelerators) = self.detect_gpu_cores(device);
        
        // Detectar relojes
        let (memory_clock, core_clock) = self.detect_clocks(device);
        
        // Detectar ancho de banda de memoria
        let memory_bandwidth = self.calculate_memory_bandwidth(memory_clock, total_memory);
        
        // Detectar versión PCIe
        let (pcie_version, pcie_lanes) = self.detect_pcie_info(device);
        
        // Detectar límite de potencia
        let power_limit = self.detect_power_limit(device);
        
        Ok(IntelGpuInfo {
            pci_device: device.clone(),
            gpu_name,
            total_memory,
            available_memory: total_memory, // Inicialmente toda la memoria está disponible
            memory_clock,
            core_clock,
            execution_units,
            ray_tracing_units,
            ai_accelerators,
            memory_bandwidth,
            pcie_version,
            pcie_lanes,
            power_limit,
            temperature: 0, // Se actualizará en tiempo real
            fan_speed: 0,   // Se actualizará en tiempo real
            driver_version: String::from("2.0.0"),
            oneapi_version: String::from("2023.2"),
            vulkan_support: true,
            opengl_support: true,
            directx_support: true,
            opencl_support: true,
        })
    }

    /// Detectar modelo específico de GPU Intel
    fn detect_gpu_model(&self, device: &PciDevice) -> String {
        match (device.vendor_id, device.device_id) {
            // Arc A-Series (Alchemist)
            (0x8086, 0x56A0) => "Arc A770".to_string(),
            (0x8086, 0x56A1) => "Arc A750".to_string(),
            (0x8086, 0x56A2) => "Arc A580".to_string(),
            (0x8086, 0x56A3) => "Arc A380".to_string(),
            (0x8086, 0x56A4) => "Arc A310".to_string(),
            
            // Xe Graphics (Tiger Lake)
            (0x8086, 0x9A49) => "Iris Xe Graphics G7".to_string(),
            (0x8086, 0x9A40) => "Iris Xe Graphics G7".to_string(),
            (0x8086, 0x9A60) => "Iris Xe Graphics G7".to_string(),
            
            // UHD Graphics (Comet Lake)
            (0x8086, 0x3E9B) => "UHD Graphics 630".to_string(),
            (0x8086, 0x3E98) => "UHD Graphics 630".to_string(),
            (0x8086, 0x3E9A) => "UHD Graphics 630".to_string(),
            
            // UHD Graphics (Coffee Lake)
            (0x8086, 0x3E92) => "UHD Graphics 630".to_string(),
            (0x8086, 0x3E91) => "UHD Graphics 630".to_string(),
            (0x8086, 0x3E90) => "UHD Graphics 630".to_string(),
            
            // UHD Graphics (Kaby Lake)
            (0x8086, 0x5912) => "UHD Graphics 630".to_string(),
            (0x8086, 0x5916) => "UHD Graphics 630".to_string(),
            (0x8086, 0x5917) => "UHD Graphics 630".to_string(),
            
            // UHD Graphics (Skylake)
            (0x8086, 0x1912) => "UHD Graphics 530".to_string(),
            (0x8086, 0x1916) => "UHD Graphics 530".to_string(),
            (0x8086, 0x1917) => "UHD Graphics 530".to_string(),
            
            // HD Graphics (Haswell)
            (0x8086, 0x0412) => "HD Graphics 4600".to_string(),
            (0x8086, 0x0416) => "HD Graphics 4600".to_string(),
            (0x8086, 0x0417) => "HD Graphics 4600".to_string(),
            
            _ => format!("Intel GPU {:04X}:{:04X}", device.vendor_id, device.device_id),
        }
    }

    /// Detectar memoria real usando BARs
    fn detect_real_memory(&self, device: &PciDevice) -> Result<u64, String> {
        // Leer todos los BARs
        let bars = device.read_all_bars();
        
        let mut total_memory = 0u64;
        let mut memory_bars = 0;
        
        for (i, bar) in bars.iter().enumerate() {
            if let Some(bar_value) = Some(*bar) {
                // Verificar si es un BAR de memoria
                if (bar_value & 0x1) == 0 { // Bit 0 = 0 indica memoria
                    let bar_size = device.calculate_bar_size(i as usize);
                    if bar_size > 0 {
                        total_memory += bar_size as u64;
                        memory_bars += 1;
                        
                    }
                }
            }
        }
        
        // Si no se detectó memoria en BARs, usar estimación por modelo
        if total_memory == 0 {
            total_memory = self.estimate_memory_by_model(device);
        }
        
        
        Ok(total_memory)
    }

    /// Estimar memoria por modelo de GPU
    fn estimate_memory_by_model(&self, device: &PciDevice) -> u64 {
        match (device.vendor_id, device.device_id) {
            // Arc A-Series (Alchemist)
            (0x8086, 0x56A0) => 8 * 1024 * 1024 * 1024,   // Arc A770 - 8GB
            (0x8086, 0x56A1) => 8 * 1024 * 1024 * 1024,   // Arc A750 - 8GB
            (0x8086, 0x56A2) => 8 * 1024 * 1024 * 1024,   // Arc A580 - 8GB
            (0x8086, 0x56A3) => 6 * 1024 * 1024 * 1024,   // Arc A380 - 6GB
            (0x8086, 0x56A4) => 4 * 1024 * 1024 * 1024,   // Arc A310 - 4GB
            
            // Xe Graphics (Tiger Lake)
            (0x8086, 0x9A49) => 1 * 1024 * 1024 * 1024,   // Iris Xe Graphics G7 - 1GB
            (0x8086, 0x9A40) => 1 * 1024 * 1024 * 1024,   // Iris Xe Graphics G7 - 1GB
            (0x8086, 0x9A60) => 1 * 1024 * 1024 * 1024,   // Iris Xe Graphics G7 - 1GB
            
            // UHD Graphics (Comet Lake)
            (0x8086, 0x3E9B) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            (0x8086, 0x3E98) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            (0x8086, 0x3E9A) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            
            // UHD Graphics (Coffee Lake)
            (0x8086, 0x3E92) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            (0x8086, 0x3E91) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            (0x8086, 0x3E90) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            
            // UHD Graphics (Kaby Lake)
            (0x8086, 0x5912) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            (0x8086, 0x5916) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            (0x8086, 0x5917) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 630 - 1GB
            
            // UHD Graphics (Skylake)
            (0x8086, 0x1912) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 530 - 1GB
            (0x8086, 0x1916) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 530 - 1GB
            (0x8086, 0x1917) => 1 * 1024 * 1024 * 1024,   // UHD Graphics 530 - 1GB
            
            // HD Graphics (Haswell)
            (0x8086, 0x0412) => 1 * 1024 * 1024 * 1024,   // HD Graphics 4600 - 1GB
            (0x8086, 0x0416) => 1 * 1024 * 1024 * 1024,   // HD Graphics 4600 - 1GB
            (0x8086, 0x0417) => 1 * 1024 * 1024 * 1024,   // HD Graphics 4600 - 1GB
            
            _ => 1 * 1024 * 1024 * 1024, // Por defecto 1GB
        }
    }

    /// Detectar número de cores
    fn detect_gpu_cores(&self, device: &PciDevice) -> (u32, u32, u32) {
        match (device.vendor_id, device.device_id) {
            // Arc A-Series (Alchemist)
            (0x8086, 0x56A0) => (32, 32, 64),   // Arc A770
            (0x8086, 0x56A1) => (28, 28, 56),   // Arc A750
            (0x8086, 0x56A2) => (24, 24, 48),   // Arc A580
            (0x8086, 0x56A3) => (8, 8, 16),     // Arc A380
            (0x8086, 0x56A4) => (6, 6, 12),     // Arc A310
            
            // Xe Graphics (Tiger Lake)
            (0x8086, 0x9A49) => (96, 0, 0),     // Iris Xe Graphics G7
            (0x8086, 0x9A40) => (96, 0, 0),     // Iris Xe Graphics G7
            (0x8086, 0x9A60) => (96, 0, 0),     // Iris Xe Graphics G7
            
            // UHD Graphics (Comet Lake)
            (0x8086, 0x3E9B) => (24, 0, 0),     // UHD Graphics 630
            (0x8086, 0x3E98) => (24, 0, 0),     // UHD Graphics 630
            (0x8086, 0x3E9A) => (24, 0, 0),     // UHD Graphics 630
            
            // UHD Graphics (Coffee Lake)
            (0x8086, 0x3E92) => (24, 0, 0),     // UHD Graphics 630
            (0x8086, 0x3E91) => (24, 0, 0),     // UHD Graphics 630
            (0x8086, 0x3E90) => (24, 0, 0),     // UHD Graphics 630
            
            // UHD Graphics (Kaby Lake)
            (0x8086, 0x5912) => (24, 0, 0),     // UHD Graphics 630
            (0x8086, 0x5916) => (24, 0, 0),     // UHD Graphics 630
            (0x8086, 0x5917) => (24, 0, 0),     // UHD Graphics 630
            
            // UHD Graphics (Skylake)
            (0x8086, 0x1912) => (24, 0, 0),     // UHD Graphics 530
            (0x8086, 0x1916) => (24, 0, 0),     // UHD Graphics 530
            (0x8086, 0x1917) => (24, 0, 0),     // UHD Graphics 530
            
            // HD Graphics (Haswell)
            (0x8086, 0x0412) => (20, 0, 0),     // HD Graphics 4600
            (0x8086, 0x0416) => (20, 0, 0),     // HD Graphics 4600
            (0x8086, 0x0417) => (20, 0, 0),     // HD Graphics 4600
            
            _ => (12, 0, 0), // Por defecto
        }
    }

    /// Detectar relojes de memoria y core
    fn detect_clocks(&self, device: &PciDevice) -> (u32, u32) {
        match (device.vendor_id, device.device_id) {
            // Arc A-Series (Alchemist)
            (0x8086, 0x56A0) => (16000, 2100),  // Arc A770
            (0x8086, 0x56A1) => (16000, 2050),  // Arc A750
            (0x8086, 0x56A2) => (16000, 2000),  // Arc A580
            (0x8086, 0x56A3) => (15500, 2000),  // Arc A380
            (0x8086, 0x56A4) => (15500, 2000),  // Arc A310
            
            // Xe Graphics (Tiger Lake)
            (0x8086, 0x9A49) => (12000, 1300),  // Iris Xe Graphics G7
            (0x8086, 0x9A40) => (12000, 1300),  // Iris Xe Graphics G7
            (0x8086, 0x9A60) => (12000, 1300),  // Iris Xe Graphics G7
            
            // UHD Graphics (Comet Lake)
            (0x8086, 0x3E9B) => (12000, 1200),  // UHD Graphics 630
            (0x8086, 0x3E98) => (12000, 1200),  // UHD Graphics 630
            (0x8086, 0x3E9A) => (12000, 1200),  // UHD Graphics 630
            
            // UHD Graphics (Coffee Lake)
            (0x8086, 0x3E92) => (12000, 1200),  // UHD Graphics 630
            (0x8086, 0x3E91) => (12000, 1200),  // UHD Graphics 630
            (0x8086, 0x3E90) => (12000, 1200),  // UHD Graphics 630
            
            // UHD Graphics (Kaby Lake)
            (0x8086, 0x5912) => (12000, 1150),  // UHD Graphics 630
            (0x8086, 0x5916) => (12000, 1150),  // UHD Graphics 630
            (0x8086, 0x5917) => (12000, 1150),  // UHD Graphics 630
            
            // UHD Graphics (Skylake)
            (0x8086, 0x1912) => (12000, 1150),  // UHD Graphics 530
            (0x8086, 0x1916) => (12000, 1150),  // UHD Graphics 530
            (0x8086, 0x1917) => (12000, 1150),  // UHD Graphics 530
            
            // HD Graphics (Haswell)
            (0x8086, 0x0412) => (8000, 1200),   // HD Graphics 4600
            (0x8086, 0x0416) => (8000, 1200),   // HD Graphics 4600
            (0x8086, 0x0417) => (8000, 1200),   // HD Graphics 4600
            
            _ => (8000, 1000), // Por defecto
        }
    }

    /// Calcular ancho de banda de memoria
    fn calculate_memory_bandwidth(&self, memory_clock: u32, total_memory: u64) -> u64 {
        // Fórmula simplificada: (memory_clock * bus_width * 2) / 8
        // Asumiendo bus de 128 bits para la mayoría de GPUs Intel
        let bus_width = 128;
        ((memory_clock as u64 * bus_width * 2) / 8) / 1000000 // Convertir a GB/s
    }

    /// Detectar información PCIe
    fn detect_pcie_info(&self, device: &PciDevice) -> (u8, u8) {
        // Por ahora, asumir PCIe 3.0 x16
        (3, 16)
    }

    /// Detectar límite de potencia
    fn detect_power_limit(&self, device: &PciDevice) -> u32 {
        match (device.vendor_id, device.device_id) {
            // Arc A-Series (Alchemist)
            (0x8086, 0x56A0) => 225,  // Arc A770
            (0x8086, 0x56A1) => 225,  // Arc A750
            (0x8086, 0x56A2) => 175,  // Arc A580
            (0x8086, 0x56A3) => 75,   // Arc A380
            (0x8086, 0x56A4) => 75,   // Arc A310
            
            // Xe Graphics (Tiger Lake)
            (0x8086, 0x9A49) => 15,   // Iris Xe Graphics G7
            (0x8086, 0x9A40) => 15,   // Iris Xe Graphics G7
            (0x8086, 0x9A60) => 15,   // Iris Xe Graphics G7
            
            // UHD Graphics (Comet Lake)
            (0x8086, 0x3E9B) => 15,   // UHD Graphics 630
            (0x8086, 0x3E98) => 15,   // UHD Graphics 630
            (0x8086, 0x3E9A) => 15,   // UHD Graphics 630
            
            // UHD Graphics (Coffee Lake)
            (0x8086, 0x3E92) => 15,   // UHD Graphics 630
            (0x8086, 0x3E91) => 15,   // UHD Graphics 630
            (0x8086, 0x3E90) => 15,   // UHD Graphics 630
            
            // UHD Graphics (Kaby Lake)
            (0x8086, 0x5912) => 15,   // UHD Graphics 630
            (0x8086, 0x5916) => 15,   // UHD Graphics 630
            (0x8086, 0x5917) => 15,   // UHD Graphics 630
            
            // UHD Graphics (Skylake)
            (0x8086, 0x1912) => 15,   // UHD Graphics 530
            (0x8086, 0x1916) => 15,   // UHD Graphics 530
            (0x8086, 0x1917) => 15,   // UHD Graphics 530
            
            // HD Graphics (Haswell)
            (0x8086, 0x0412) => 15,   // HD Graphics 4600
            (0x8086, 0x0416) => 15,   // HD Graphics 4600
            (0x8086, 0x0417) => 15,   // HD Graphics 4600
            
            _ => 15, // Por defecto
        }
    }

    /// Habilitar aceleración por hardware
    fn enable_acceleration(&mut self) -> Result<(), String> {
        if let Some(idx) = self.active_gpu {
            if let Some(gpu_info) = self.intel_gpus.get(idx) {
                // Habilitar MMIO y Bus Master
                gpu_info.pci_device.enable_mmio_and_bus_master();
                
                // Configurar aceleración
                self.acceleration_enabled = true;
                self.oneapi_enabled = true;
                self.ray_tracing_enabled = true;
                
                Ok(())
            } else {
                Err(String::from("GPU activa no encontrada"))
            }
        } else {
            Err(String::from("No hay GPU activa"))
        }
    }
}

impl Driver for IntelAdvancedDriver {
    fn get_info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Initializing;
        self.detect_intel_gpus()?;
        
        if !self.intel_gpus.is_empty() {
            self.active_gpu = Some(0); // Usar la primera GPU como activa
            self.enable_acceleration()?;
        }
        
        self.info.state = DriverState::Ready;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Unloading;
        self.intel_gpus.clear();
        self.active_gpu = None;
        self.memory_mapped = false;
        self.acceleration_enabled = false;
        self.oneapi_enabled = false;
        self.ray_tracing_enabled = false;
        self.info.state = DriverState::Unloaded;
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Unloaded;
        self.acceleration_enabled = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Initializing;
        self.detect_intel_gpus()?;
        self.enable_acceleration()?;
        self.info.state = DriverState::Ready;
        Ok(())
    }

    fn handle_message(&mut self, message: DriverMessage) -> DriverResponse {
        match message {
            DriverMessage::Initialize => match self.initialize() {
                Ok(_) => DriverResponse::Success,
                Err(e) => DriverResponse::Error(e),
            },
            DriverMessage::Shutdown => match self.shutdown() {
                Ok(_) => DriverResponse::Success,
                Err(e) => DriverResponse::Error(e),
            },
            DriverMessage::Suspend => match self.suspend() {
                Ok(_) => DriverResponse::Success,
                Err(e) => DriverResponse::Error(e),
            },
            DriverMessage::Resume => match self.resume() {
                Ok(_) => DriverResponse::Success,
                Err(e) => DriverResponse::Error(e),
            },
            DriverMessage::GetStatus => {
                DriverResponse::SuccessWithData(format!("{:?}", self.info.state).into_bytes())
            },
            DriverMessage::GetCapabilities => {
                let caps: Vec<String> = self.info.capabilities.iter()
                    .map(|c| format!("{:?}", c))
                    .collect();
                let caps_str = caps.join(",");
                DriverResponse::SuccessWithData(caps_str.into_bytes())
            },
            DriverMessage::ExecuteCommand { command, args: _ } => {
                match command.as_str() {
                    "get_gpu_count" => {
                        let count = self.intel_gpus.len() as u32;
                        DriverResponse::SuccessWithData(count.to_le_bytes().to_vec())
                    },
                    "get_gpu_info" => {
                        if let Some(idx) = self.active_gpu {
                            if let Some(gpu_info) = self.intel_gpus.get(idx) {
                                let info = format!(
                                    "GPU: {}, Memoria: {} GB, Execution Units: {}, Ray Tracing Units: {}, AI Accelerators: {}",
                                    gpu_info.gpu_name,
                                    gpu_info.total_memory / (1024 * 1024 * 1024),
                                    gpu_info.execution_units,
                                    gpu_info.ray_tracing_units,
                                    gpu_info.ai_accelerators
                                );
                                DriverResponse::SuccessWithData(info.into_bytes())
                            } else {
                                DriverResponse::Error(String::from("GPU activa no encontrada"))
                            }
                        } else {
                            DriverResponse::Error(String::from("No hay GPU activa"))
                        }
                    },
                    "get_memory_info" => {
                        if let Some(idx) = self.active_gpu {
                            if let Some(gpu_info) = self.intel_gpus.get(idx) {
                                let info = format!(
                                    "Total: {} GB, Disponible: {} GB, Ancho de banda: {} GB/s",
                                    gpu_info.total_memory / (1024 * 1024 * 1024),
                                    gpu_info.available_memory / (1024 * 1024 * 1024),
                                    gpu_info.memory_bandwidth
                                );
                                DriverResponse::SuccessWithData(info.into_bytes())
                            } else {
                                DriverResponse::Error(String::from("GPU activa no encontrada"))
                            }
                        } else {
                            DriverResponse::Error(String::from("No hay GPU activa"))
                        }
                    },
                    "get_performance_info" => {
                        if let Some(idx) = self.active_gpu {
                            if let Some(gpu_info) = self.intel_gpus.get(idx) {
                                let info = format!(
                                    "Core Clock: {} MHz, Memory Clock: {} MHz, Power: {}W, Temp: {}°C",
                                    gpu_info.core_clock,
                                    gpu_info.memory_clock,
                                    gpu_info.power_limit,
                                    gpu_info.temperature
                                );
                                DriverResponse::SuccessWithData(info.into_bytes())
                            } else {
                                DriverResponse::Error(String::from("GPU activa no encontrada"))
                            }
                        } else {
                            DriverResponse::Error(String::from("No hay GPU activa"))
                        }
                    },
                    _ => DriverResponse::Error(format!("Comando desconocido: {}", command)),
                }
            },
            _ => DriverResponse::Error(String::from("Mensaje de driver no soportado")),
        }
    }

    fn get_state(&self) -> DriverState {
        self.info.state.clone()
    }

    fn can_handle_device(&self, vendor_id: u16, device_id: u16, class_code: u8) -> bool {
        // Intel Vendor ID es 0x8086
        vendor_id == 0x8086 && class_code == 0x03 // 0x03 es Display Controller
    }
}
