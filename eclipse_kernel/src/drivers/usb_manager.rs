//! Gestor de drivers USB reales
//! 
//! Integra todos los drivers USB reales (teclado, ratón, etc.) y proporciona
//! una interfaz unificada para el sistema.

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType, DeviceOperations},
    manager::{Driver, DriverInfo, DriverResult, DriverError},
    usb::{RealUsbController, UsbControllerType, UsbDeviceClass},
    usb_keyboard_real::UsbKeyboardReal,
    usb_mouse_real::UsbMouseReal,
    keyboard::{KeyEvent, KeyCode},
    mouse::{MouseEvent, MouseButton},
};
use alloc::vec::Vec;
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

/// Gestor principal de drivers USB reales
pub struct UsbManager {
    pub info: DriverInfo,
    pub controllers: Vec<RealUsbController>,
    pub keyboard_driver: Option<UsbKeyboardReal>,
    pub mouse_driver: Option<UsbMouseReal>,
    pub is_initialized: bool,
    pub device_count: u32,
}

impl UsbManager {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("usb_manager");
        info.device_type = DeviceType::Usb;
        info.version = 1;

        Self {
            info,
            controllers: Vec::new(),
            keyboard_driver: None,
            mouse_driver: None,
            is_initialized: false,
            device_count: 0,
        }
    }

    /// Agregar controlador USB
    pub fn add_controller(&mut self, controller: RealUsbController) -> DriverResult<()> {
        self.controllers.push(controller);
        Ok(())
    }

    /// Detectar controladores USB del sistema
    pub fn detect_controllers(&mut self) -> DriverResult<()> {
        // Detectar controladores XHCI (USB 3.0)
        if let Some(xhci_base) = self.detect_xhci_controller() {
            let controller = RealUsbController::new(
                UsbControllerType::XHCI,
                xhci_base,
                11 // IRQ típico para XHCI
            );
            self.add_controller(controller)?;
        }

        // Detectar controladores EHCI (USB 2.0)
        if let Some(ehci_base) = self.detect_ehci_controller() {
            let controller = RealUsbController::new(
                UsbControllerType::EHCI,
                ehci_base,
                12 // IRQ típico para EHCI
            );
            self.add_controller(controller)?;
        }

        // Detectar controladores OHCI (USB 1.1)
        if let Some(ohci_base) = self.detect_ohci_controller() {
            let controller = RealUsbController::new(
                UsbControllerType::OHCI,
                ohci_base,
                9 // IRQ típico para OHCI
            );
            self.add_controller(controller)?;
        }

        // Detectar controladores UHCI (USB 1.1)
        if let Some(uhci_base) = self.detect_uhci_controller() {
            let controller = RealUsbController::new(
                UsbControllerType::UHCI,
                uhci_base,
                9 // IRQ típico para UHCI
            );
            self.add_controller(controller)?;
        }

        Ok(())
    }

    /// Detectar controlador XHCI
    fn detect_xhci_controller(&self) -> Option<u64> {
        // Buscar en PCI para controladores XHCI
        // Direcciones típicas de controladores XHCI
        let xhci_addresses = [
            0xFED00000, // Dirección típica de XHCI
            0xFED80000, // Alternativa
        ];

        for &addr in &xhci_addresses {
            if self.is_valid_xhci_controller(addr) {
                return Some(addr as u64);
            }
        }
        None
    }

    /// Verificar si es un controlador XHCI válido
    fn is_valid_xhci_controller(&self, addr: u32) -> bool {
        unsafe {
            let regs = addr as *const u32;
            // Leer CAPLENGTH y verificar que sea válido
            let caplength = core::ptr::read_volatile(regs) & 0xFF;
            caplength >= 0x20 && caplength <= 0x40 // Rango válido para CAPLENGTH
        }
    }

    /// Detectar controlador EHCI
    fn detect_ehci_controller(&self) -> Option<u64> {
        let ehci_addresses = [
            0xFED00000, // Dirección típica de EHCI
            0xFED80000, // Alternativa
        ];

        for &addr in &ehci_addresses {
            if self.is_valid_ehci_controller(addr) {
                return Some(addr as u64);
            }
        }
        None
    }

    /// Verificar si es un controlador EHCI válido
    fn is_valid_ehci_controller(&self, addr: u32) -> bool {
        unsafe {
            let regs = addr as *const u32;
            // Leer CAPLENGTH y verificar que sea válido
            let caplength = core::ptr::read_volatile(regs) & 0xFF;
            caplength >= 0x20 && caplength <= 0x40
        }
    }

    /// Detectar controlador OHCI
    fn detect_ohci_controller(&self) -> Option<u64> {
        let ohci_addresses = [
            0xFED00000, // Dirección típica de OHCI
            0xFED80000, // Alternativa
        ];

        for &addr in &ohci_addresses {
            if self.is_valid_ohci_controller(addr) {
                return Some(addr as u64);
            }
        }
        None
    }

    /// Verificar si es un controlador OHCI válido
    fn is_valid_ohci_controller(&self, addr: u32) -> bool {
        unsafe {
            let regs = addr as *const u32;
            // Leer HcRevision y verificar que sea válido
            let revision = core::ptr::read_volatile(regs) & 0xFF;
            revision >= 0x10 && revision <= 0x20 // Rango válido para OHCI
        }
    }

    /// Detectar controlador UHCI
    fn detect_uhci_controller(&self) -> Option<u64> {
        let uhci_addresses = [
            0xFED00000, // Dirección típica de UHCI
            0xFED80000, // Alternativa
        ];

        for &addr in &uhci_addresses {
            if self.is_valid_uhci_controller(addr) {
                return Some(addr as u64);
            }
        }
        None
    }

    /// Verificar si es un controlador UHCI válido
    fn is_valid_uhci_controller(&self, addr: u32) -> bool {
        unsafe {
            let regs = addr as *const u16;
            // Leer USBCMD y verificar que sea válido
            let cmd = core::ptr::read_volatile(regs);
            (cmd & 0x01) == 0 // Debe estar deshabilitado inicialmente
        }
    }

    /// Inicializar todos los controladores
    pub fn initialize_controllers(&mut self) -> DriverResult<()> {
        for controller in &mut self.controllers {
            controller.enable()?;
            controller.detect_devices()?;
        }
        Ok(())
    }

    /// Inicializar drivers de dispositivos
    pub fn initialize_device_drivers(&mut self) -> DriverResult<()> {
        // Inicializar driver de teclado
        let mut keyboard_driver = UsbKeyboardReal::new();
        if let Some(controller) = self.controllers.first() {
            keyboard_driver.set_usb_controller(controller.clone());
            if keyboard_driver.detect_keyboard().is_ok() {
                keyboard_driver.initialize()?;
                self.keyboard_driver = Some(keyboard_driver);
                self.device_count += 1;
            }
        }

        // Inicializar driver de ratón
        let mut mouse_driver = UsbMouseReal::new();
        if let Some(controller) = self.controllers.first() {
            mouse_driver.set_usb_controller(controller.clone());
            if mouse_driver.detect_mouse().is_ok() {
                mouse_driver.initialize()?;
                self.mouse_driver = Some(mouse_driver);
                self.device_count += 1;
            }
        }

        Ok(())
    }

    /// Obtener siguiente evento de teclado
    pub fn get_next_key_event(&mut self) -> Option<KeyEvent> {
        if let Some(ref mut keyboard) = self.keyboard_driver {
            keyboard.get_next_key_event()
        } else {
            None
        }
    }

    /// Obtener siguiente evento de ratón
    pub fn get_next_mouse_event(&mut self) -> Option<MouseEvent> {
        if let Some(ref mut mouse) = self.mouse_driver {
            mouse.get_next_mouse_event()
        } else {
            None
        }
    }

    /// Verificar si hay eventos de teclado
    pub fn has_keyboard_events(&self) -> bool {
        if let Some(ref keyboard) = self.keyboard_driver {
            keyboard.has_key_events()
        } else {
            false
        }
    }

    /// Verificar si hay eventos de ratón
    pub fn has_mouse_events(&self) -> bool {
        if let Some(ref mouse) = self.mouse_driver {
            mouse.has_mouse_events()
        } else {
            false
        }
    }

    /// Verificar si una tecla está presionada
    pub fn is_key_pressed(&self, key: KeyCode) -> bool {
        if let Some(ref keyboard) = self.keyboard_driver {
            keyboard.is_key_pressed(key)
        } else {
            false
        }
    }

    /// Verificar si un botón del ratón está presionado
    pub fn is_mouse_button_pressed(&self, button: MouseButton) -> bool {
        if let Some(ref mouse) = self.mouse_driver {
            mouse.is_button_pressed(button)
        } else {
            false
        }
    }

    /// Obtener posición del ratón
    pub fn get_mouse_position(&self) -> (i32, i32) {
        if let Some(ref mouse) = self.mouse_driver {
            mouse.get_position()
        } else {
            (0, 0)
        }
    }

    /// Establecer posición del ratón
    pub fn set_mouse_position(&mut self, x: i32, y: i32) {
        if let Some(ref mut mouse) = self.mouse_driver {
            mouse.set_position(x, y);
        }
    }

    /// Obtener estadísticas completas
    pub fn get_complete_stats(&self) -> String {
        let mut stats = String::new();
        stats.push_str("=== GESTOR USB REAL ===\n");
        stats.push_str(&format!("Controladores USB: {}\n", self.controllers.len()));
        stats.push_str(&format!("Dispositivos detectados: {}\n", self.device_count));
        
        if let Some(ref keyboard) = self.keyboard_driver {
            stats.push_str("\n");
            stats.push_str(&keyboard.get_keyboard_stats());
        }
        
        if let Some(ref mouse) = self.mouse_driver {
            stats.push_str("\n");
            stats.push_str(&mouse.get_mouse_stats());
        }
        
        // Estadísticas de controladores
        for (i, controller) in self.controllers.iter().enumerate() {
            stats.push_str(&format!("\nControlador {}: {:?} en 0x{:08X}\n", 
                i + 1, 
                controller.controller_type,
                controller.base_address
            ));
            stats.push_str(&format!("  Habilitado: {}\n", controller.is_enabled));
            stats.push_str(&format!("  Dispositivos: {}\n", controller.devices.len()));
        }
        
        stats
    }

    /// Procesar interrupciones USB
    pub fn handle_usb_interrupts(&mut self) -> DriverResult<()> {
        for controller in &mut self.controllers {
            if controller.is_enabled {
                // Procesar interrupciones del controlador
                // En una implementación real, esto verificaría el estado de interrupción
            }
        }

        // Procesar eventos de dispositivos
        if let Some(ref mut keyboard) = self.keyboard_driver {
            keyboard.handle_interrupt(0)?;
        }

        if let Some(ref mut mouse) = self.mouse_driver {
            mouse.handle_interrupt(0)?;
        }

        Ok(())
    }

    /// Verificar si el teclado está conectado
    pub fn is_keyboard_connected(&self) -> bool {
        self.keyboard_driver.is_some()
    }

    /// Verificar si el ratón está conectado
    pub fn is_mouse_connected(&self) -> bool {
        self.mouse_driver.is_some()
    }

    /// Obtener número de dispositivos conectados
    pub fn get_connected_device_count(&self) -> u32 {
        self.device_count
    }

    /// Reinicializar todos los dispositivos
    pub fn reinitialize_devices(&mut self) -> DriverResult<()> {
        // Limpiar drivers existentes
        self.keyboard_driver = None;
        self.mouse_driver = None;
        self.device_count = 0;

        // Reinicializar controladores
        self.initialize_controllers()?;

        // Reinicializar drivers de dispositivos
        self.initialize_device_drivers()?;

        Ok(())
    }
}

impl Driver for UsbManager {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        if self.is_initialized {
            return Ok(());
        }

        // Detectar controladores USB
        self.detect_controllers()?;

        // Inicializar controladores
        self.initialize_controllers()?;

        // Inicializar drivers de dispositivos
        self.initialize_device_drivers()?;

        self.info.is_loaded = true;
        self.is_initialized = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        self.controllers.clear();
        self.keyboard_driver = None;
        self.mouse_driver = None;
        self.device_count = 0;
        self.is_initialized = false;
        self.info.is_loaded = false;
        Ok(())
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        device_info.device_type == DeviceType::Usb
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        device.driver_id = Some(self.info.id);
        Ok(())
    }

    fn detach_device(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }

    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> {
        self.handle_usb_interrupts()
    }
}
