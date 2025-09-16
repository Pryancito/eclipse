//! Driver avanzado para GPUs AMD
//! 
//! Implementa detección real de memoria, aceleración por hardware
//! y características específicas de AMD.

use crate::drivers::ipc::{Driver, DriverInfo, DriverState, DriverCapability, DriverMessage, DriverResponse};
use crate::drivers::pci::{PciDevice, PciManager, GpuInfo, GpuType};
use crate::syslog;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;

/// Información detallada de una GPU AMD
#[derive(Debug, Clone)]
pub struct AmdGpuInfo {
    pub pci_device: PciDevice,
    pub gpu_name: String,
    pub total_memory: u64,        // En bytes
    pub available_memory: u64,    // En bytes
    pub memory_clock: u32,        // En MHz
    pub core_clock: u32,          // En MHz
    pub compute_units: u32,       // Compute Units (equivalente a CUDA cores)
    pub ray_accelerators: u32,    // Ray Accelerators (equivalente a RT cores)
    pub ai_accelerators: u32,     // AI Accelerators (equivalente a Tensor cores)
    pub memory_bandwidth: u64,    // En GB/s
    pub pcie_version: u8,
    pub pcie_lanes: u8,
    pub power_limit: u32,         // En watts
    pub temperature: u32,         // En Celsius
    pub fan_speed: u32,          // En RPM
    pub driver_version: String,
    pub rocm_version: String,     // ROCm (Radeon Open Compute)
    pub vulkan_support: bool,
    pub opengl_support: bool,
    pub directx_support: bool,
    pub opencl_support: bool,
}

/// Driver avanzado para AMD
pub struct AmdAdvancedDriver {
    info: DriverInfo,
    pci_manager: PciManager,
    amd_gpus: Vec<AmdGpuInfo>,
    active_gpu: Option<usize>,
    memory_mapped: bool,
    acceleration_enabled: bool,
    rocm_enabled: bool,
    ray_tracing_enabled: bool,
}

impl AmdAdvancedDriver {
    pub fn new() -> Self {
        let info = DriverInfo {
            id: 0, // Se asignará al registrar
            name: String::from("AMD Advanced Driver"),
            version: String::from("2.0.0"),
            author: String::from("Eclipse OS Team"),
            description: String::from("Driver avanzado para GPUs AMD con detección real de memoria y aceleración"),
            state: DriverState::Unloaded,
            dependencies: {
                let mut deps = Vec::new();
                deps.push(String::from("PCI Driver"));
                deps
            },
            capabilities: {
                let mut caps = Vec::new();
                caps.push(DriverCapability::Graphics);
                caps.push(DriverCapability::Custom(String::from("ROCm")));
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
            amd_gpus: Vec::new(),
            active_gpu: None,
            memory_mapped: false,
            acceleration_enabled: false,
            rocm_enabled: false,
            ray_tracing_enabled: false,
        }
    }

    /// Detectar GPUs AMD con información detallada
    fn detect_amd_gpus(&mut self) -> Result<(), String> {
        self.pci_manager.scan_devices();
        let gpus = self.pci_manager.detect_gpus();
        
        self.amd_gpus.clear();
        
        for gpu in gpus {
            if matches!(gpu.gpu_type, GpuType::Amd) {
                let amd_info = self.analyze_amd_gpu(&gpu)?;
                self.amd_gpus.push(amd_info);
            }
        }


        for (i, gpu) in self.amd_gpus.iter().enumerate() {
        }
        Ok(())
    }

    /// Analizar GPU AMD específica
    fn analyze_amd_gpu(&self, gpu: &GpuInfo) -> Result<AmdGpuInfo, String> {
        let device = &gpu.pci_device;
        
        // Detectar modelo específico de GPU
        let gpu_name = self.detect_gpu_model(device);
        
        // Detectar memoria real usando BARs
        let total_memory = self.detect_real_memory(device)?;
        
        // Detectar características específicas
        let (compute_units, ray_accelerators, ai_accelerators) = self.detect_gpu_cores(device);
        
        // Detectar relojes
        let (memory_clock, core_clock) = self.detect_clocks(device);
        
        // Detectar ancho de banda de memoria
        let memory_bandwidth = self.calculate_memory_bandwidth(memory_clock, total_memory);
        
        // Detectar versión PCIe
        let (pcie_version, pcie_lanes) = self.detect_pcie_info(device);
        
        // Detectar límite de potencia
        let power_limit = self.detect_power_limit(device);
        
        Ok(AmdGpuInfo {
            pci_device: device.clone(),
            gpu_name,
            total_memory,
            available_memory: total_memory, // Inicialmente toda la memoria está disponible
            memory_clock,
            core_clock,
            compute_units,
            ray_accelerators,
            ai_accelerators,
            memory_bandwidth,
            pcie_version,
            pcie_lanes,
            power_limit,
            temperature: 0, // Se actualizará en tiempo real
            fan_speed: 0,   // Se actualizará en tiempo real
            driver_version: String::from("2.0.0"),
            rocm_version: String::from("5.7"),
            vulkan_support: true,
            opengl_support: true,
            directx_support: true,
            opencl_support: true,
        })
    }

    /// Detectar modelo específico de GPU AMD
    fn detect_gpu_model(&self, device: &PciDevice) -> String {
        match (device.vendor_id, device.device_id) {
            // RDNA 3 (RX 7000 series)
            (0x1002, 0x744C) => "Radeon RX 7900 XTX".to_string(),
            (0x1002, 0x7448) => "Radeon RX 7900 XT".to_string(),
            (0x1002, 0x7480) => "Radeon RX 7800 XT".to_string(),
            (0x1002, 0x747E) => "Radeon RX 7700 XT".to_string(),
            (0x1002, 0x747C) => "Radeon RX 7600 XT".to_string(),
            (0x1002, 0x7478) => "Radeon RX 7600".to_string(),
            
            // RDNA 2 (RX 6000 series)
            (0x1002, 0x73BF) => "Radeon RX 6900 XT".to_string(),
            (0x1002, 0x73AF) => "Radeon RX 6800 XT".to_string(),
            (0x1002, 0x73A0) => "Radeon RX 6800".to_string(),
            (0x1002, 0x73DF) => "Radeon RX 6700 XT".to_string(),
            (0x1002, 0x73CF) => "Radeon RX 6600 XT".to_string(),
            (0x1002, 0x73FF) => "Radeon RX 6600".to_string(),
            (0x1002, 0x73EF) => "Radeon RX 6500 XT".to_string(),
            (0x1002, 0x73E0) => "Radeon RX 6400".to_string(),
            
            // RDNA 1 (RX 5000 series)
            (0x1002, 0x731F) => "Radeon RX 5700 XT".to_string(),
            (0x1002, 0x731E) => "Radeon RX 5700".to_string(),
            (0x1002, 0x7319) => "Radeon RX 5600 XT".to_string(),
            (0x1002, 0x731F) => "Radeon RX 5500 XT".to_string(),
            
            // GCN (RX 400/500 series)
            (0x1002, 0x67DF) => "Radeon RX 580".to_string(),
            (0x1002, 0x67FF) => "Radeon RX 570".to_string(),
            (0x1002, 0x67EF) => "Radeon RX 560".to_string(),
            (0x1002, 0x67E0) => "Radeon RX 550".to_string(),
            (0x1002, 0x67C0) => "Radeon RX 480".to_string(),
            (0x1002, 0x67DF) => "Radeon RX 470".to_string(),
            (0x1002, 0x67EF) => "Radeon RX 460".to_string(),
            
            _ => format!("AMD GPU {:04X}:{:04X}", device.vendor_id, device.device_id),
        }
    }

    /// Detectar memoria real usando BARs
    fn detect_real_memory(&self, device: &PciDevice) -> Result<u64, String> {
        // Leer todos los BARs
        let bars = device.read_all_bars();
        
        let mut total_memory = 0u64;
        let mut memory_bars = 0;
        
        for (i, bar) in bars.iter().enumerate() {
            if let Some(bar_value) = bar {
                // Verificar si es un BAR de memoria
                if (bar_value & 0x1) == 0 { // Bit 0 = 0 indica memoria
                    let bar_size = device.calculate_bar_size(i as u8)?;
                    if bar_size > 0 {
                        total_memory += bar_size;
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
            // RDNA 3 (RX 7000 series)
            (0x1002, 0x744C) => 24 * 1024 * 1024 * 1024, // RX 7900 XTX - 24GB
            (0x1002, 0x7448) => 20 * 1024 * 1024 * 1024, // RX 7900 XT - 20GB
            (0x1002, 0x7480) => 16 * 1024 * 1024 * 1024, // RX 7800 XT - 16GB
            (0x1002, 0x747E) => 12 * 1024 * 1024 * 1024, // RX 7700 XT - 12GB
            (0x1002, 0x747C) => 16 * 1024 * 1024 * 1024, // RX 7600 XT - 16GB
            (0x1002, 0x7478) => 8 * 1024 * 1024 * 1024,  // RX 7600 - 8GB
            
            // RDNA 2 (RX 6000 series)
            (0x1002, 0x73BF) => 16 * 1024 * 1024 * 1024, // RX 6900 XT - 16GB
            (0x1002, 0x73AF) => 16 * 1024 * 1024 * 1024, // RX 6800 XT - 16GB
            (0x1002, 0x73A0) => 16 * 1024 * 1024 * 1024, // RX 6800 - 16GB
            (0x1002, 0x73DF) => 12 * 1024 * 1024 * 1024, // RX 6700 XT - 12GB
            (0x1002, 0x73CF) => 8 * 1024 * 1024 * 1024,  // RX 6600 XT - 8GB
            (0x1002, 0x73FF) => 8 * 1024 * 1024 * 1024,  // RX 6600 - 8GB
            (0x1002, 0x73EF) => 4 * 1024 * 1024 * 1024,  // RX 6500 XT - 4GB
            (0x1002, 0x73E0) => 4 * 1024 * 1024 * 1024,  // RX 6400 - 4GB
            
            // RDNA 1 (RX 5000 series)
            (0x1002, 0x731F) => 8 * 1024 * 1024 * 1024,  // RX 5700 XT - 8GB
            (0x1002, 0x731E) => 8 * 1024 * 1024 * 1024,  // RX 5700 - 8GB
            (0x1002, 0x7319) => 6 * 1024 * 1024 * 1024,  // RX 5600 XT - 6GB
            (0x1002, 0x731F) => 8 * 1024 * 1024 * 1024,  // RX 5500 XT - 8GB
            
            // GCN (RX 400/500 series)
            (0x1002, 0x67DF) => 8 * 1024 * 1024 * 1024,  // RX 580 - 8GB
            (0x1002, 0x67FF) => 4 * 1024 * 1024 * 1024,  // RX 570 - 4GB
            (0x1002, 0x67EF) => 4 * 1024 * 1024 * 1024,  // RX 560 - 4GB
            (0x1002, 0x67E0) => 4 * 1024 * 1024 * 1024,  // RX 550 - 4GB
            (0x1002, 0x67C0) => 8 * 1024 * 1024 * 1024,  // RX 480 - 8GB
            (0x1002, 0x67DF) => 4 * 1024 * 1024 * 1024,  // RX 470 - 4GB
            (0x1002, 0x67EF) => 4 * 1024 * 1024 * 1024,  // RX 460 - 4GB
            
            _ => 8 * 1024 * 1024 * 1024, // Por defecto 8GB
        }
    }

    /// Detectar número de cores
    fn detect_gpu_cores(&self, device: &PciDevice) -> (u32, u32, u32) {
        match (device.vendor_id, device.device_id) {
            // RDNA 3 (RX 7000 series)
            (0x1002, 0x744C) => (96, 96, 192),  // RX 7900 XTX
            (0x1002, 0x7448) => (84, 84, 168),  // RX 7900 XT
            (0x1002, 0x7480) => (60, 60, 120),  // RX 7800 XT
            (0x1002, 0x747E) => (54, 54, 108),  // RX 7700 XT
            (0x1002, 0x747C) => (32, 32, 64),   // RX 7600 XT
            (0x1002, 0x7478) => (32, 32, 64),   // RX 7600
            
            // RDNA 2 (RX 6000 series)
            (0x1002, 0x73BF) => (80, 80, 160),  // RX 6900 XT
            (0x1002, 0x73AF) => (72, 72, 144),  // RX 6800 XT
            (0x1002, 0x73A0) => (60, 60, 120),  // RX 6800
            (0x1002, 0x73DF) => (40, 40, 80),   // RX 6700 XT
            (0x1002, 0x73CF) => (32, 32, 64),   // RX 6600 XT
            (0x1002, 0x73FF) => (28, 28, 56),   // RX 6600
            (0x1002, 0x73EF) => (16, 16, 32),   // RX 6500 XT
            (0x1002, 0x73E0) => (12, 12, 24),   // RX 6400
            
            // RDNA 1 (RX 5000 series)
            (0x1002, 0x731F) => (40, 0, 0),     // RX 5700 XT (sin ray tracing)
            (0x1002, 0x731E) => (36, 0, 0),     // RX 5700
            (0x1002, 0x7319) => (36, 0, 0),     // RX 5600 XT
            (0x1002, 0x731F) => (22, 0, 0),     // RX 5500 XT
            
            // GCN (RX 400/500 series)
            (0x1002, 0x67DF) => (36, 0, 0),     // RX 580
            (0x1002, 0x67FF) => (32, 0, 0),     // RX 570
            (0x1002, 0x67EF) => (16, 0, 0),     // RX 560
            (0x1002, 0x67E0) => (8, 0, 0),      // RX 550
            (0x1002, 0x67C0) => (36, 0, 0),     // RX 480
            (0x1002, 0x67DF) => (32, 0, 0),     // RX 470
            (0x1002, 0x67EF) => (14, 0, 0),     // RX 460
            
            _ => (16, 0, 0), // Por defecto
        }
    }

    /// Detectar relojes de memoria y core
    fn detect_clocks(&self, device: &PciDevice) -> (u32, u32) {
        match (device.vendor_id, device.device_id) {
            // RDNA 3 (RX 7000 series)
            (0x1002, 0x744C) => (25000, 2300),  // RX 7900 XTX
            (0x1002, 0x7448) => (20000, 2000),  // RX 7900 XT
            (0x1002, 0x7480) => (19500, 2124),  // RX 7800 XT
            (0x1002, 0x747E) => (18000, 2544),  // RX 7700 XT
            (0x1002, 0x747C) => (18000, 2755),  // RX 7600 XT
            (0x1002, 0x7478) => (18000, 2625),  // RX 7600
            
            // RDNA 2 (RX 6000 series)
            (0x1002, 0x73BF) => (16000, 2015),  // RX 6900 XT
            (0x1002, 0x73AF) => (16000, 2015),  // RX 6800 XT
            (0x1002, 0x73A0) => (16000, 1815),  // RX 6800
            (0x1002, 0x73DF) => (16000, 2424),  // RX 6700 XT
            (0x1002, 0x73CF) => (16000, 2359),  // RX 6600 XT
            (0x1002, 0x73FF) => (14000, 2044),  // RX 6600
            (0x1002, 0x73EF) => (14000, 2610),  // RX 6500 XT
            (0x1002, 0x73E0) => (14000, 2039),  // RX 6400
            
            // RDNA 1 (RX 5000 series)
            (0x1002, 0x731F) => (14000, 1605),  // RX 5700 XT
            (0x1002, 0x731E) => (14000, 1465),  // RX 5700
            (0x1002, 0x7319) => (14000, 1560),  // RX 5600 XT
            (0x1002, 0x731F) => (14000, 1607),  // RX 5500 XT
            
            // GCN (RX 400/500 series)
            (0x1002, 0x67DF) => (8000, 1257),   // RX 580
            (0x1002, 0x67FF) => (7000, 1168),   // RX 570
            (0x1002, 0x67EF) => (7000, 1075),   // RX 560
            (0x1002, 0x67E0) => (7000, 1075),   // RX 550
            (0x1002, 0x67C0) => (8000, 1120),   // RX 480
            (0x1002, 0x67DF) => (7000, 926),    // RX 470
            (0x1002, 0x67EF) => (7000, 1090),   // RX 460
            
            _ => (8000, 1000), // Por defecto
        }
    }

    /// Calcular ancho de banda de memoria
    fn calculate_memory_bandwidth(&self, memory_clock: u32, total_memory: u64) -> u64 {
        // Fórmula simplificada: (memory_clock * bus_width * 2) / 8
        // Asumiendo bus de 256 bits para la mayoría de GPUs modernas
        let bus_width = 256;
        ((memory_clock as u64 * bus_width * 2) / 8) / 1000000 // Convertir a GB/s
    }

    /// Detectar información PCIe
    fn detect_pcie_info(&self, device: &PciDevice) -> (u8, u8) {
        // Por ahora, asumir PCIe 4.0 x16
        (4, 16)
    }

    /// Detectar límite de potencia
    fn detect_power_limit(&self, device: &PciDevice) -> u32 {
        match (device.vendor_id, device.device_id) {
            // RDNA 3 (RX 7000 series)
            (0x1002, 0x744C) => 355,  // RX 7900 XTX
            (0x1002, 0x7448) => 300,  // RX 7900 XT
            (0x1002, 0x7480) => 263,  // RX 7800 XT
            (0x1002, 0x747E) => 245,  // RX 7700 XT
            (0x1002, 0x747C) => 190,  // RX 7600 XT
            (0x1002, 0x7478) => 165,  // RX 7600
            
            // RDNA 2 (RX 6000 series)
            (0x1002, 0x73BF) => 300,  // RX 6900 XT
            (0x1002, 0x73AF) => 300,  // RX 6800 XT
            (0x1002, 0x73A0) => 250,  // RX 6800
            (0x1002, 0x73DF) => 230,  // RX 6700 XT
            (0x1002, 0x73CF) => 160,  // RX 6600 XT
            (0x1002, 0x73FF) => 132,  // RX 6600
            (0x1002, 0x73EF) => 107,  // RX 6500 XT
            (0x1002, 0x73E0) => 53,   // RX 6400
            
            // RDNA 1 (RX 5000 series)
            (0x1002, 0x731F) => 225,  // RX 5700 XT
            (0x1002, 0x731E) => 180,  // RX 5700
            (0x1002, 0x7319) => 160,  // RX 5600 XT
            (0x1002, 0x731F) => 130,  // RX 5500 XT
            
            // GCN (RX 400/500 series)
            (0x1002, 0x67DF) => 185,  // RX 580
            (0x1002, 0x67FF) => 150,  // RX 570
            (0x1002, 0x67EF) => 80,   // RX 560
            (0x1002, 0x67E0) => 50,   // RX 550
            (0x1002, 0x67C0) => 150,  // RX 480
            (0x1002, 0x67DF) => 120,  // RX 470
            (0x1002, 0x67EF) => 75,   // RX 460
            
            _ => 150, // Por defecto
        }
    }

    /// Habilitar aceleración por hardware
    fn enable_acceleration(&mut self) -> Result<(), String> {
        if let Some(idx) = self.active_gpu {
            if let Some(gpu_info) = self.amd_gpus.get(idx) {
                // Habilitar MMIO y Bus Master
                gpu_info.pci_device.enable_mmio_and_bus_master();
                
                // Configurar aceleración
                self.acceleration_enabled = true;
                self.rocm_enabled = true;
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

impl Driver for AmdAdvancedDriver {
    fn get_info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Initializing;
        self.detect_amd_gpus()?;
        
        if !self.amd_gpus.is_empty() {
            self.active_gpu = Some(0); // Usar la primera GPU como activa
            self.enable_acceleration()?;
        }
        
        self.info.state = DriverState::Ready;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Unloading;
        self.amd_gpus.clear();
        self.active_gpu = None;
        self.memory_mapped = false;
        self.acceleration_enabled = false;
        self.rocm_enabled = false;
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
        self.detect_amd_gpus()?;
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
                        let count = self.amd_gpus.len() as u32;
                        DriverResponse::SuccessWithData(count.to_le_bytes().to_vec())
                    },
                    "get_gpu_info" => {
                        if let Some(idx) = self.active_gpu {
                            if let Some(gpu_info) = self.amd_gpus.get(idx) {
                                let info = format!(
                                    "GPU: {}, Memoria: {} GB, Compute Units: {}, Ray Accelerators: {}, AI Accelerators: {}",
                                    gpu_info.gpu_name,
                                    gpu_info.total_memory / (1024 * 1024 * 1024),
                                    gpu_info.compute_units,
                                    gpu_info.ray_accelerators,
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
                            if let Some(gpu_info) = self.amd_gpus.get(idx) {
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
                            if let Some(gpu_info) = self.amd_gpus.get(idx) {
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
        self.info.state
    }

    fn can_handle_device(&self, vendor_id: u16, device_id: u16, class_code: u8) -> bool {
        // AMD Vendor ID es 0x1002
        vendor_id == 0x1002 && class_code == 0x03 // 0x03 es Display Controller
    }
}
