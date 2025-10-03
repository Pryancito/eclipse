use super::ipc::{
    Driver, DriverCapability, DriverInfo, DriverMessage, DriverResponse, DriverState,
};
use super::pci::{GpuInfo, GpuType, PciDevice, PciManager};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Driver PCI base que implementa el sistema IPC
pub struct PciDriver {
    info: DriverInfo,
    pci_manager: PciManager,
    devices: BTreeMap<u32, PciDevice>,
    gpus: Vec<GpuInfo>,
}

impl PciDriver {
    pub fn new() -> Self {
        let info = DriverInfo {
            id: 0, // Se asignará al registrar
            name: String::from("PCI Driver"),
            version: String::from("1.0.0"),
            author: String::from("Eclipse OS Team"),
            description: String::from("Driver base para dispositivos PCI/PCIe"),
            state: DriverState::Unloaded,
            dependencies: Vec::new(),
            capabilities: {
                let mut caps = Vec::new();
                caps.push(DriverCapability::Graphics);
                caps.push(DriverCapability::Network);
                caps.push(DriverCapability::Storage);
                caps.push(DriverCapability::Audio);
                caps.push(DriverCapability::Input);
                caps
            },
        };

        Self {
            info,
            pci_manager: PciManager::new(),
            devices: BTreeMap::new(),
            gpus: Vec::new(),
        }
    }

    /// Escanear todos los dispositivos PCI
    fn scan_devices(&mut self) -> Result<(), String> {
        self.devices.clear();
        self.gpus.clear();

        // Escanear dispositivos PCI (temporalmente deshabilitado para evitar cuelgues en entornos sin HW)
        // self.pci_manager.scan_devices();
        // Los dispositivos se obtienen después del escaneo
        // for device in devices {
        //     let key = (device.bus as u32) << 16 | (device.device as u32) << 8 | (device.function as u32);
        //     self.devices.insert(key, device);
        // }

        // Detectar GPUs
        self.gpus = self
            .pci_manager
            .get_gpus()
            .iter()
            .filter_map(|gpu| gpu.clone())
            .collect();

        // PCI Driver: {} dispositivos detectados, {} GPUs encontradas

        Ok(())
    }

    /// Obtener información de un dispositivo específico
    fn get_device_info(&self, bus: u8, device: u8, function: u8) -> Option<&PciDevice> {
        let key = (bus as u32) << 16 | (device as u32) << 8 | (function as u32);
        self.devices.get(&key)
    }

    /// Obtener información de GPUs
    fn get_gpu_info(&self) -> &Vec<GpuInfo> {
        &self.gpus
    }

    /// Ejecutar comando específico del driver PCI
    fn execute_command(&self, command: &str, args: &[u8]) -> DriverResponse {
        match command {
            "scan_devices" => {
                // Comando para re-escanear dispositivos
                DriverResponse::Success
            }
            "get_device_count" => {
                let count = self.devices.len() as u32;
                DriverResponse::SuccessWithData(count.to_le_bytes().to_vec())
            }
            "get_gpu_count" => {
                let count = self.gpus.len() as u32;
                DriverResponse::SuccessWithData(count.to_le_bytes().to_vec())
            }
            "get_device_info" => {
                if args.len() >= 3 {
                    let bus = args[0];
                    let device = args[1];
                    let function = args[2];

                    if let Some(pci_device) = self.get_device_info(bus, device, function) {
                        // Serializar información del dispositivo
                        let mut data = Vec::new();
                        data.extend_from_slice(&pci_device.vendor_id.to_le_bytes());
                        data.extend_from_slice(&pci_device.device_id.to_le_bytes());
                        data.extend_from_slice(&pci_device.class_code.to_le_bytes());
                        data.extend_from_slice(&pci_device.subclass_code.to_le_bytes());
                        data.extend_from_slice(&pci_device.prog_if.to_le_bytes());
                        data.extend_from_slice(&pci_device.revision_id.to_le_bytes());

                        DriverResponse::SuccessWithData(data)
                    } else {
                        DriverResponse::Error(String::from("Dispositivo no encontrado"))
                    }
                } else {
                    DriverResponse::Error(String::from("Argumentos insuficientes"))
                }
            }
            "get_gpu_info" => {
                if args.len() >= 1 {
                    let gpu_index = args[0] as usize;
                    if gpu_index < self.gpus.len() {
                        let gpu = &self.gpus[gpu_index];
                        let mut data = Vec::new();

                        // Serializar información de la GPU
                        data.extend_from_slice(&gpu.pci_device.vendor_id.to_le_bytes());
                        data.extend_from_slice(&gpu.pci_device.device_id.to_le_bytes());
                        data.extend_from_slice(&gpu.memory_size.to_le_bytes());
                        data.extend_from_slice(&gpu.pci_device.bus.to_le_bytes());
                        data.extend_from_slice(&gpu.pci_device.device.to_le_bytes());
                        data.extend_from_slice(&gpu.pci_device.function.to_le_bytes());

                        // Agregar tipo de GPU como string
                        let gpu_type_str = gpu.gpu_type.as_str();
                        let gpu_type_bytes = gpu_type_str.as_bytes();
                        data.extend_from_slice(&(gpu_type_bytes.len() as u32).to_le_bytes());
                        data.extend_from_slice(gpu_type_bytes);

                        DriverResponse::SuccessWithData(data)
                    } else {
                        DriverResponse::Error(String::from("Índice de GPU inválido"))
                    }
                } else {
                    DriverResponse::Error(String::from("Argumentos insuficientes"))
                }
            }
            "enable_device" => {
                if args.len() >= 3 {
                    let bus = args[0];
                    let device = args[1];
                    let function = args[2];

                    if let Some(pci_device) = self.get_device_info(bus, device, function) {
                        // Habilitar MMIO y Bus Master
                        // MMIO habilitado temporalmente - método no implementado
                        DriverResponse::Success
                    } else {
                        DriverResponse::Error(String::from("Dispositivo no encontrado"))
                    }
                } else {
                    DriverResponse::Error(String::from("Argumentos insuficientes"))
                }
            }
            _ => DriverResponse::NotSupported,
        }
    }
}

impl Driver for PciDriver {
    fn initialize(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Initializing;

        // Inicializar el manager PCI
        // self.pci_manager.initialize()?; // TEMPORALMENTE DESHABILITADO

        // Escanear dispositivos
        self.scan_devices()?;

        self.info.state = DriverState::Ready;
        // PCI Driver inicializado correctamente
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Unloading;

        // Limpiar recursos
        self.devices.clear();
        self.gpus.clear();

        self.info.state = DriverState::Unloaded;
        // PCI Driver cerrado correctamente
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Unloaded;
        // PCI Driver suspendido
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        self.info.state = DriverState::Initializing;

        // Re-escanear dispositivos al reanudar
        self.scan_devices()?;

        self.info.state = DriverState::Ready;
        // PCI Driver reanudado
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
            DriverMessage::GetStatus => DriverResponse::SuccessWithData(
                format!("{:?}", self.info.state).as_bytes().to_vec(),
            ),
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
        // El driver PCI puede manejar cualquier dispositivo PCI
        // Pero solo reporta dispositivos de clase específica
        match class_code {
            0x03 => true, // Display Controller
            0x02 => true, // Network Controller
            0x01 => true, // Mass Storage Controller
            0x04 => true, // Multimedia Controller
            0x0C => true, // Serial Bus Controller
            _ => false,
        }
    }
}

impl Default for PciDriver {
    fn default() -> Self {
        Self::new()
    }
}
