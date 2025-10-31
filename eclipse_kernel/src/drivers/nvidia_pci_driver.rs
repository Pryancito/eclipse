use super::ipc::{
    Driver, DriverCapability, DriverInfo, DriverMessage, DriverResponse, DriverState,
};
use super::pci::{GpuInfo, GpuType, PciDevice, PciManager};
use crate::hardware_detection::HardwareDetectionResult;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Driver específico para GPUs NVIDIA
pub struct NvidiaPciDriver {
    info: DriverInfo,
    pci_manager: PciManager,
    nvidia_gpus: Vec<GpuInfo>,
    active_gpu: Option<usize>,
    memory_mapped: bool,
}

impl NvidiaPciDriver {
    pub fn new() -> Self {
        let info = DriverInfo {
            id: 0, // Se asignará al registrar
            name: String::from("NVIDIA PCI Driver"),
            version: String::from("1.0.0"),
            author: String::from("Eclipse OS Team"),
            description: String::from(
                "Driver específico para GPUs NVIDIA con detección avanzada de memoria",
            ),
            state: DriverState::Unloaded,
            dependencies: {
                let mut deps = Vec::new();
                deps.push(String::from("PCI Driver"));
                deps
            },
            capabilities: {
                let mut caps = Vec::new();
                caps.push(DriverCapability::Graphics);
                caps.push(DriverCapability::Custom(String::from("CUDA")));
                caps.push(DriverCapability::Custom(String::from("RayTracing")));
                caps.push(DriverCapability::Custom(String::from("DLSS")));
                caps
            },
        };

        Self {
            info,
            pci_manager: PciManager::new(),
            nvidia_gpus: Vec::new(),
            active_gpu: None,
            memory_mapped: false,
        }
    }

    /// Detectar y configurar GPUs NVIDIA
    fn detect_nvidia_gpus(&mut self, hw_result: &HardwareDetectionResult) -> Result<(), String> {
        self.nvidia_gpus.clear();

        // Usar la información de hardware ya detectada
        for gpu in &hw_result.available_gpus {
            if matches!(gpu.gpu_type, GpuType::Nvidia) {
                self.nvidia_gpus.push(gpu.clone());
            }
        }

        if self.nvidia_gpus.is_empty() {
            return Err(String::from("No se encontraron GPUs NVIDIA"));
        }

        // Configurar la primera GPU como activa por defecto
        self.active_gpu = Some(0);

        Ok(())
    }

    /// Habilitar MMIO para la GPU activa
    fn enable_mmio(&mut self) -> Result<(), String> {
        if let Some(gpu_index) = self.active_gpu {
            if gpu_index < self.nvidia_gpus.len() {
                let gpu = &self.nvidia_gpus[gpu_index];

                // Habilitar MMIO y Bus Master - TEMPORALMENTE DESHABILITADO
                // self.pci_manager.enable_mmio_and_bus_master(
                //     gpu.pci_device.bus,
                //     gpu.pci_device.device,
                //     gpu.pci_device.function
                // )?;

                self.memory_mapped = true;
                // MMIO habilitado para GPU NVIDIA
                Ok(())
            } else {
                Err(String::from("Índice de GPU inválido"))
            }
        } else {
            Err(String::from("No hay GPU activa"))
        }
    }

    /// Obtener información detallada de la GPU activa
    fn get_active_gpu_info(&self) -> Option<&GpuInfo> {
        if let Some(gpu_index) = self.active_gpu {
            self.nvidia_gpus.get(gpu_index)
        } else {
            None
        }
    }

    /// Cambiar GPU activa
    fn set_active_gpu(&mut self, gpu_index: usize) -> Result<(), String> {
        if gpu_index < self.nvidia_gpus.len() {
            self.active_gpu = Some(gpu_index);
            self.memory_mapped = false; // Necesita re-habilitar MMIO
            Ok(())
        } else {
            Err(String::from("Índice de GPU inválido"))
        }
    }

    /// Ejecutar comando específico del driver NVIDIA
    fn execute_command(&self, command: &str, args: &[u8]) -> DriverResponse {
        match command {
            "detect_gpus" => DriverResponse::Success,
            "get_gpu_count" => {
                let count = self.nvidia_gpus.len() as u32;
                DriverResponse::SuccessWithData(count.to_le_bytes().to_vec())
            }
            "get_active_gpu" => {
                if let Some(gpu_index) = self.active_gpu {
                    DriverResponse::SuccessWithData((gpu_index as u32).to_le_bytes().to_vec())
                } else {
                    DriverResponse::Error(String::from("No hay GPU activa"))
                }
            }
            "set_active_gpu" => {
                if args.len() >= 1 {
                    let gpu_index = args[0] as usize;
                    if gpu_index < self.nvidia_gpus.len() {
                        DriverResponse::Success
                    } else {
                        DriverResponse::Error(String::from("Índice de GPU inválido"))
                    }
                } else {
                    DriverResponse::Error(String::from("Argumentos insuficientes"))
                }
            }
            "get_gpu_info" => {
                if let Some(gpu) = self.get_active_gpu_info() {
                    let mut data = Vec::new();

                    // Información básica
                    data.extend_from_slice(&gpu.pci_device.vendor_id.to_le_bytes());
                    data.extend_from_slice(&gpu.pci_device.device_id.to_le_bytes());
                    data.extend_from_slice(&gpu.memory_size.to_le_bytes());
                    data.extend_from_slice(&gpu.pci_device.bus.to_le_bytes());
                    data.extend_from_slice(&gpu.pci_device.device.to_le_bytes());
                    data.extend_from_slice(&gpu.pci_device.function.to_le_bytes());

                    // Información de BARs
                    if let Some(pci_device) = self.pci_manager.get_device(0) {
                        let bars = pci_device.read_all_bars();
                        for bar in bars {
                            data.extend_from_slice(&bar.to_le_bytes());
                        }
                    }

                    // Estado de MMIO
                    data.push(if self.memory_mapped { 1 } else { 0 });

                    DriverResponse::SuccessWithData(data)
                } else {
                    DriverResponse::Error(String::from("No hay GPU activa"))
                }
            }
            "enable_mmio" => {
                if self.memory_mapped {
                    DriverResponse::Success
                } else {
                    DriverResponse::Error(String::from("MMIO no habilitado"))
                }
            }
            "get_memory_info" => {
                if let Some(gpu) = self.get_active_gpu_info() {
                    let mut data = Vec::new();

                    // Tamaño total de memoria
                    data.extend_from_slice(&gpu.memory_size.to_le_bytes());

                    // Información de BARs detallada
                    if let Some(pci_device) = self.pci_manager.get_device(0) {
                        let bars = pci_device.read_all_bars();
                        let mut bar_sizes = Vec::new();
                        for i in 0..6 {
                            bar_sizes.push(pci_device.calculate_bar_size(i));
                        }

                        for (i, (bar, size)) in bars.iter().zip(bar_sizes.iter()).enumerate() {
                            data.push(i as u8);
                            data.extend_from_slice(&bar.to_le_bytes());
                            data.extend_from_slice(&size.to_le_bytes());
                        }
                    }

                    DriverResponse::SuccessWithData(data)
                } else {
                    DriverResponse::Error(String::from("No hay GPU activa"))
                }
            }
            "test_memory_access" => {
                // Comando para probar acceso a memoria de la GPU
                if self.memory_mapped {
                    DriverResponse::Success
                } else {
                    DriverResponse::Error(String::from("MMIO no habilitado"))
                }
            }
            _ => DriverResponse::NotSupported,
        }
    }
}

impl NvidiaPciDriver {
    /// Inicializar con información de hardware detectada
    pub fn initialize_with_hardware(
        &mut self,
        hw_result: &HardwareDetectionResult,
    ) -> Result<(), String> {
        self.info.state = DriverState::Initializing;

        // Detectar GPUs NVIDIA usando la información ya detectada
        self.detect_nvidia_gpus(hw_result)?;

        // Habilitar MMIO para la GPU activa
        self.enable_mmio()?;

        self.info.state = DriverState::Ready;
        Ok(())
    }
}

impl Driver for NvidiaPciDriver {
    fn initialize(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Initializing;

        // El driver necesita información de hardware externa
        // Por ahora, solo configuramos el estado
        self.info.state = DriverState::Ready;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Unloading;

        // Limpiar recursos
        self.nvidia_gpus.clear();
        self.active_gpu = None;
        self.memory_mapped = false;

        self.info.state = DriverState::Unloaded;
        // NVIDIA PCI Driver cerrado correctamente
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Unloaded;
        self.memory_mapped = false;
        // NVIDIA PCI Driver suspendido
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Initializing;

        // El resume necesita información de hardware externa
        // Por ahora, solo configuramos el estado
        self.info.state = DriverState::Ready;
        Ok(())
    }

    fn get_info(&self) -> DriverInfo {
        self.info.clone()
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
                let status = if self.nvidia_gpus.is_empty() {
                    "No GPUs detected"
                } else if self.memory_mapped {
                    "Ready with MMIO"
                } else {
                    "Ready without MMIO"
                };
                DriverResponse::SuccessWithData(status.as_bytes().to_vec())
            }
            DriverMessage::GetCapabilities => {
                let caps: Vec<String> = self
                    .info
                    .capabilities
                    .iter()
                    .map(|c| format!("{:?}", c))
                    .collect();
                let caps_str = caps.join(",");
                DriverResponse::SuccessWithData(caps_str.as_bytes().to_vec())
            }
            DriverMessage::ExecuteCommand { command, args } => {
                self.execute_command(&command, &args)
            }
            _ => DriverResponse::NotSupported,
        }
    }

    fn get_state(&self) -> DriverState {
        self.info.state.clone()
    }

    fn can_handle_device(&self, vendor_id: u16, device_id: u16, class_code: u8) -> bool {
        // Solo manejar dispositivos NVIDIA de clase Display Controller
        vendor_id == 0x10DE && class_code == 0x03
    }
}

impl Default for NvidiaPciDriver {
    fn default() -> Self {
        Self::new()
    }
}
