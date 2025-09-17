//! Sistema de árbol de dispositivos para Eclipse OS
//! 
//! Implementa enumeración y gestión de dispositivos del sistema

use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;

/// ID de dispositivo
pub type DeviceId = u32;

/// Tipo de dispositivo
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceType {
    Cpu,
    Memory,
    Storage,
    Network,
    Graphics,
    Audio,
    Input,
    Usb,
    Pci,
    Serial,
    Parallel,
    Unknown,
}

/// Estado del dispositivo
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceState {
    Unknown,
    Present,
    Initialized,
    Active,
    Suspended,
    Error,
    Removed,
}

/// Información de dispositivo
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device_id: DeviceId,
    pub name: String,
    pub device_type: DeviceType,
    pub state: DeviceState,
    pub parent_id: Option<DeviceId>,
    pub children: Vec<DeviceId>,
    pub properties: BTreeMap<String, String>,
    pub driver_name: Option<String>,
    pub is_plug_and_play: bool,
}

/// Nodo del árbol de dispositivos
#[derive(Debug, Clone)]
pub struct DeviceNode {
    pub device: DeviceInfo,
    pub level: u32,
}

/// Gestor del árbol de dispositivos
pub struct DeviceTreeManager {
    devices: BTreeMap<DeviceId, DeviceNode>,
    next_device_id: DeviceId,
    root_device_id: DeviceId,
    initialized: bool,
}

impl DeviceTreeManager {
    pub fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
            next_device_id: 0,
            root_device_id: 0,
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Device tree manager already initialized");
        }

        // Crear dispositivo raíz
        let root_device = DeviceInfo {
            device_id: 0,
            name: "root"String::from(.to_string(),
            device_type: DeviceType::Unknown,
            state: DeviceState::Active,
            parent_id: None,
            children: Vec::new(),
            properties: BTreeMap::new(),
            driver_name: None,
            is_plug_and_play: false,
        };

        let root_node = DeviceNode {
            device: root_device,
            level: 0,
        };

        self.devices.insert(0, root_node);
        self.root_device_id = 0;
        self.next_device_id = 1;

        // Enumerar dispositivos del sistema
        self.enumerate_system_devices()?;

        self.initialized = true;
        Ok(())
    }

    fn enumerate_system_devices(&mut self) -> Result<(), &'static str> {
        // Simular enumeración de dispositivos del sistema
        
        // CPU
        let cpu_id = self.add_device(
            "cpu0"String::from(.to_string(),
            DeviceType::Cpu,
            Some(0),
            BTreeMap::new(),
            None,
            false,
        )?;

        // Memoria
        let memory_id = self.add_device(
            "memory0"String::from(.to_string(),
            DeviceType::Memory,
            Some(0),
            BTreeMap::new(),
            None,
            false,
        )?;

        // GPU
        let gpu_id = self.add_device(
            "gpu0"String::from(.to_string(),
            DeviceType::Graphics,
            Some(0),
            BTreeMap::new(),
            Some("nvidia_driver"String::from(.to_string()),
            false,
        )?;

        // Red
        let network_id = self.add_device(
            "eth0"String::from(.to_string(),
            DeviceType::Network,
            Some(0),
            BTreeMap::new(),
            Some("e1000_driver"String::from(.to_string()),
            false,
        )?;

        // Audio
        let audio_id = self.add_device(
            "audio0"String::from(.to_string(),
            DeviceType::Audio,
            Some(0),
            BTreeMap::new(),
            Some("hda_driver"String::from(.to_string()),
            false,
        )?;

        // USB
        let usb_id = self.add_device(
            "usb0"String::from(.to_string(),
            DeviceType::Usb,
            Some(0),
            BTreeMap::new(),
            Some("xhci_driver"String::from(.to_string()),
            true,
        )?;

        // Ratón USB
        let mouse_id = self.add_device(
            "mouse0"String::from(.to_string(),
            DeviceType::Input,
            Some(usb_id),
            BTreeMap::new(),
            Some("usb_mouse_driver"String::from(.to_string()),
            true,
        )?;

        // Teclado USB
        let keyboard_id = self.add_device(
            "keyboard0"String::from(.to_string(),
            DeviceType::Input,
            Some(usb_id),
            BTreeMap::new(),
            Some("usb_keyboard_driver"String::from(.to_string()),
            true,
        )?;

        Ok(())
    }

    pub fn add_device(
        &mut self,
        name: String,
        device_type: DeviceType,
        parent_id: Option<DeviceId>,
        properties: BTreeMap<String, String>,
        driver_name: Option<String>,
        is_plug_and_play: bool,
    ) -> Result<DeviceId, &'static str> {
        if !self.initialized {
            return Err("Device tree manager not initialized");
        }

        let device_id = self.next_device_id;
        self.next_device_id += 1;

        let device = DeviceInfo {
            device_id,
            name,
            device_type,
            state: DeviceState::Present,
            parent_id,
            children: Vec::new(),
            properties,
            driver_name,
            is_plug_and_play,
        };

        let level = if let Some(parent_id) = parent_id {
            if let Some(parent) = self.devices.get(&parent_id) {
                parent.level + 1
            } else {
                return Err("Parent device not found");
            }
        } else {
            0
        };

        let device_node = DeviceNode {
            device,
            level,
        };

        self.devices.insert(device_id, device_node);

        // Agregar como hijo del padre
        if let Some(parent_id) = parent_id {
            if let Some(parent) = self.devices.get_mut(&parent_id) {
                parent.device.children.push(device_id);
            }
        }

        Ok(device_id)
    }

    pub fn remove_device(&mut self, device_id: DeviceId) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Device tree manager not initialized");
        }

        if device_id == self.root_device_id {
            return Err("Cannot remove root device");
        }

        if let Some(device) = self.devices.get(&device_id) {
            // Obtener información necesaria antes de hacer préstamos mutables
            let parent_id = device.device.parent_id;
            let children = device.device.children.clone();

            // Remover de la lista de hijos del padre
            if let Some(parent_id) = parent_id {
                if let Some(parent) = self.devices.get_mut(&parent_id) {
                    parent.device.children.retain(|&id| id != device_id);
                }
            }

            // Remover todos los hijos
            for &child_id in &children {
                self.remove_device(child_id)?;
            }

            self.devices.remove(&device_id);
            Ok(())
        } else {
            Err("Device not found")
        }
    }

    pub fn get_device(&self, device_id: DeviceId) -> Option<&DeviceNode> {
        self.devices.get(&device_id)
    }

    pub fn get_device_mut(&mut self, device_id: DeviceId) -> Option<&mut DeviceNode> {
        self.devices.get_mut(&device_id)
    }

    pub fn get_devices_by_type(&self, device_type: DeviceType) -> Vec<&DeviceNode> {
        self.devices.values()
            .filter(|node| node.device.device_type == device_type)
            .collect()
    }

    pub fn get_devices_by_state(&self, state: DeviceState) -> Vec<&DeviceNode> {
        self.devices.values()
            .filter(|node| node.device.state == state)
            .collect()
    }

    pub fn get_children(&self, device_id: DeviceId) -> Vec<&DeviceNode> {
        if let Some(device) = self.devices.get(&device_id) {
            device.device.children.iter()
                .filter_map(|&child_id| self.devices.get(&child_id))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_parent(&self, device_id: DeviceId) -> Option<&DeviceNode> {
        if let Some(device) = self.devices.get(&device_id) {
            if let Some(parent_id) = device.device.parent_id {
                self.devices.get(&parent_id)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn get_device_count(&self) -> usize {
        self.devices.len()
    }

    pub fn get_device_count_by_type(&self, device_type: DeviceType) -> usize {
        self.devices.values()
            .filter(|node| node.device.device_type == device_type)
            .count()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}
