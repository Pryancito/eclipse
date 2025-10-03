//! API para aplicaciones cliente del sistema de ventanas
//!
//! Proporciona una interfaz similar a X11/Wayland para que las aplicaciones
//! puedan crear ventanas, manejar eventos y renderizar contenido.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::event_system::{InputEvent, WindowEvent, WindowEventType};
use super::geometry::{Point, Rectangle, Size};
use super::protocol::{
    InputEventData, InputEventType, MessageBuilder, MessageData, MessageType, ProtocolError,
    ProtocolMessage, WindowFlags,
};
use super::{ClientId, WindowId};

/// Información de un cliente conectado
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub client_id: ClientId,
    pub name: String,
    pub windows: Vec<WindowId>,
    pub connected: bool,
}

/// Información de una ventana del cliente
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub window_id: WindowId,
    pub client_id: ClientId,
    pub title: String,
    pub geometry: Rectangle,
    pub flags: WindowFlags,
    pub mapped: bool,
    pub visible: bool,
}

/// API para clientes del sistema de ventanas
pub struct ClientAPI {
    /// Clientes conectados
    clients: BTreeMap<ClientId, ClientInfo>,
    /// Ventanas de los clientes
    windows: BTreeMap<WindowId, WindowInfo>,
    /// Cola de mensajes entrantes
    incoming_messages: VecDeque<ProtocolMessage>,
    /// Cola de mensajes salientes
    outgoing_messages: VecDeque<ProtocolMessage>,
    /// Próximo ID de cliente
    next_client_id: AtomicU32,
    /// API inicializada
    initialized: AtomicBool,
}

impl ClientAPI {
    pub fn new() -> Result<Self, &'static str> {
        Ok(Self {
            clients: BTreeMap::new(),
            windows: BTreeMap::new(),
            incoming_messages: VecDeque::new(),
            outgoing_messages: VecDeque::new(),
            next_client_id: AtomicU32::new(1),
            initialized: AtomicBool::new(false),
        })
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Conectar un nuevo cliente
    pub fn connect_client(&mut self, name: String) -> Result<ClientId, &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("API de cliente no inicializada");
        }

        let client_id = self.next_client_id.fetch_add(1, Ordering::SeqCst);

        let client_info = ClientInfo {
            client_id,
            name,
            windows: Vec::new(),
            connected: true,
        };

        self.clients.insert(client_id, client_info);

        // Enviar mensaje de bienvenida
        let welcome_message = MessageBuilder::new(MessageType::WindowCreated, client_id)
            .sequence(0)
            .build();

        self.outgoing_messages.push_back(welcome_message);

        Ok(client_id)
    }

    /// Desconectar un cliente
    pub fn disconnect_client(&mut self, client_id: ClientId) -> Result<(), &'static str> {
        if let Some(client_info) = self.clients.get(&client_id) {
            // Destruir todas las ventanas del cliente
            let window_ids = client_info.windows.clone();
            for window_id in window_ids {
                self.destroy_window(window_id)?;
            }

            self.clients.remove(&client_id);
        }

        Ok(())
    }

    /// Crear una ventana para un cliente
    pub fn create_window(
        &mut self,
        client_id: ClientId,
        title: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        flags: WindowFlags,
    ) -> Result<WindowId, &'static str> {
        if !self.clients.contains_key(&client_id) {
            return Err("Cliente no encontrado");
        }

        // Generar ID de ventana único
        let window_id = self.generate_window_id();

        let window_info = WindowInfo {
            window_id,
            client_id,
            title,
            geometry: Rectangle::new(x, y, width, height),
            flags,
            mapped: false,
            visible: false,
        };

        self.windows.insert(window_id, window_info);

        // Agregar ventana al cliente
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.windows.push(window_id);
        }

        // Enviar confirmación al cliente
        let response = MessageBuilder::new(MessageType::WindowCreated, client_id)
            .window_id(window_id)
            .sequence(0)
            .window_created(window_id)
            .build();

        self.outgoing_messages.push_back(response);

        Ok(window_id)
    }

    /// Destruir una ventana
    pub fn destroy_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window_info) = self.windows.remove(&window_id) {
            // Remover ventana del cliente
            if let Some(client) = self.clients.get_mut(&window_info.client_id) {
                client.windows.retain(|&id| id != window_id);
            }

            // Enviar confirmación al cliente
            let response = MessageBuilder::new(MessageType::WindowDestroyed, window_info.client_id)
                .window_id(window_id)
                .sequence(0)
                .build();

            self.outgoing_messages.push_back(response);
        }

        Ok(())
    }

    /// Mover una ventana
    pub fn move_window(&mut self, window_id: WindowId, x: i32, y: i32) -> Result<(), &'static str> {
        if let Some(window_info) = self.windows.get_mut(&window_id) {
            window_info.geometry.x = x;
            window_info.geometry.y = y;

            // Enviar confirmación al cliente
            let response = MessageBuilder::new(MessageType::WindowMoved, window_info.client_id)
                .window_id(window_id)
                .sequence(0)
                .move_window(x, y)
                .build();

            self.outgoing_messages.push_back(response);
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Redimensionar una ventana
    pub fn resize_window(
        &mut self,
        window_id: WindowId,
        width: u32,
        height: u32,
    ) -> Result<(), &'static str> {
        if let Some(window_info) = self.windows.get_mut(&window_id) {
            window_info.geometry.width = width;
            window_info.geometry.height = height;

            // Enviar confirmación al cliente
            let response = MessageBuilder::new(MessageType::WindowResized, window_info.client_id)
                .window_id(window_id)
                .sequence(0)
                .resize_window(width, height)
                .build();

            self.outgoing_messages.push_back(response);
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Establecer título de ventana
    pub fn set_window_title(
        &mut self,
        window_id: WindowId,
        title: String,
    ) -> Result<(), &'static str> {
        if let Some(window_info) = self.windows.get_mut(&window_id) {
            window_info.title = title.clone();

            // Enviar confirmación al cliente
            let response = MessageBuilder::new(MessageType::SetWindowTitle, window_info.client_id)
                .window_id(window_id)
                .sequence(0)
                .set_window_title(title)
                .build();

            self.outgoing_messages.push_back(response);
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Mapear una ventana (hacerla visible)
    pub fn map_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window_info) = self.windows.get_mut(&window_id) {
            window_info.mapped = true;
            window_info.visible = true;

            // Enviar confirmación al cliente
            let response = MessageBuilder::new(MessageType::WindowMapped, window_info.client_id)
                .window_id(window_id)
                .sequence(0)
                .build();

            self.outgoing_messages.push_back(response);
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Desmapear una ventana (ocultarla)
    pub fn unmap_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window_info) = self.windows.get_mut(&window_id) {
            window_info.mapped = false;
            window_info.visible = false;

            // Enviar confirmación al cliente
            let response = MessageBuilder::new(MessageType::WindowUnmapped, window_info.client_id)
                .window_id(window_id)
                .sequence(0)
                .build();

            self.outgoing_messages.push_back(response);
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Dar foco a una ventana
    pub fn focus_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window_info) = self.windows.get(&window_id) {
            // Enviar evento de foco al cliente
            let response = MessageBuilder::new(MessageType::WindowFocused, window_info.client_id)
                .window_id(window_id)
                .sequence(0)
                .build();

            self.outgoing_messages.push_back(response);
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Procesar mensaje de un cliente
    pub fn process_client_message(&mut self, message: ProtocolMessage) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("API de cliente no inicializada");
        }

        match message.data {
            MessageData::CreateWindow {
                title,
                x,
                y,
                width,
                height,
                flags,
            } => {
                self.create_window(message.client_id, title, x, y, width, height, flags)?;
            }
            MessageData::DestroyWindow => {
                if let Some(window_id) = message.window_id {
                    self.destroy_window(window_id)?;
                }
            }
            MessageData::MoveWindow { x, y } => {
                if let Some(window_id) = message.window_id {
                    self.move_window(window_id, x, y)?;
                }
            }
            MessageData::ResizeWindow { width, height } => {
                if let Some(window_id) = message.window_id {
                    self.resize_window(window_id, width, height)?;
                }
            }
            MessageData::SetWindowTitle { title } => {
                if let Some(window_id) = message.window_id {
                    self.set_window_title(window_id, title)?;
                }
            }
            _ => {
                // Otros tipos de mensaje (simplificado)
            }
        }

        Ok(())
    }

    /// Enviar evento a un cliente
    pub fn send_event_to_client(
        &mut self,
        client_id: ClientId,
        event: InputEvent,
    ) -> Result<(), &'static str> {
        let window_id = event.window_id.unwrap_or(0);

        let message = MessageBuilder::new(MessageType::EventReceived, client_id)
            .window_id(window_id)
            .sequence(0)
            .input_event(event.event_type, event.data)
            .build();

        self.outgoing_messages.push_back(message);
        Ok(())
    }

    /// Obtener mensaje saliente
    pub fn get_outgoing_message(&mut self) -> Option<ProtocolMessage> {
        self.outgoing_messages.pop_front()
    }

    /// Agregar mensaje entrante
    pub fn add_incoming_message(&mut self, message: ProtocolMessage) {
        self.incoming_messages.push_back(message);
    }

    /// Procesar mensajes entrantes
    pub fn process_incoming_messages(&mut self) -> Result<(), &'static str> {
        while let Some(message) = self.incoming_messages.pop_front() {
            self.process_client_message(message)?;
        }
        Ok(())
    }

    /// Obtener información de un cliente
    pub fn get_client_info(&self, client_id: ClientId) -> Option<&ClientInfo> {
        self.clients.get(&client_id)
    }

    /// Obtener información de una ventana
    pub fn get_window_info(&self, window_id: WindowId) -> Option<&WindowInfo> {
        self.windows.get(&window_id)
    }

    /// Obtener todas las ventanas de un cliente
    pub fn get_client_windows(&self, client_id: ClientId) -> Option<&Vec<WindowId>> {
        self.clients.get(&client_id).map(|client| &client.windows)
    }

    /// Obtener ventana bajo un punto
    pub fn get_window_at(&self, point: Point) -> Option<WindowId> {
        // Buscar ventanas desde la más alta (última en el orden de dibujo)
        // hasta la más baja
        for (window_id, window_info) in self.windows.iter().rev() {
            if window_info.visible && window_info.geometry.contains_point(&point) {
                return Some(*window_id);
            }
        }
        None
    }

    /// Obtener ventanas visibles en un área
    pub fn get_windows_in_area(&self, area: Rectangle) -> Vec<WindowId> {
        let mut windows = Vec::new();

        for (window_id, window_info) in &self.windows {
            if window_info.visible && window_info.geometry.intersects(&area) {
                windows.push(*window_id);
            }
        }

        windows
    }

    /// Generar nuevo ID de ventana
    fn generate_window_id(&self) -> WindowId {
        // En una implementación real, esto usaría un generador de IDs único
        // Por simplicidad, usamos un contador basado en el timestamp
        core::time::Duration::from_millis(0).as_nanos() as u32 // Placeholder
    }

    /// Obtener número de clientes conectados
    pub fn get_client_count(&self) -> u32 {
        self.clients.len() as u32
    }

    /// Obtener número de ventanas
    pub fn get_window_count(&self) -> u32 {
        self.windows.len() as u32
    }

    /// Obtener estadísticas de la API
    pub fn get_stats(&self) -> ClientAPIStats {
        ClientAPIStats {
            client_count: self.clients.len(),
            window_count: self.windows.len(),
            incoming_queue_size: self.incoming_messages.len(),
            outgoing_queue_size: self.outgoing_messages.len(),
        }
    }
}

/// Estadísticas de la API de cliente
#[derive(Debug, Clone)]
pub struct ClientAPIStats {
    pub client_count: usize,
    pub window_count: usize,
    pub incoming_queue_size: usize,
    pub outgoing_queue_size: usize,
}

/// Instancia global de la API de cliente
static mut CLIENT_API: Option<ClientAPI> = None;

/// Inicializar la API de cliente global
pub fn init_client_api() -> Result<(), &'static str> {
    unsafe {
        if CLIENT_API.is_some() {
            return Err("API de cliente ya inicializada");
        }

        let mut api = ClientAPI::new()?;
        api.initialize()?;
        CLIENT_API = Some(api);
    }
    Ok(())
}

/// Obtener referencia a la API de cliente
pub fn get_client_api() -> Result<&'static mut ClientAPI, &'static str> {
    unsafe { CLIENT_API.as_mut().ok_or("API de cliente no inicializada") }
}

/// Verificar si la API de cliente está inicializada
pub fn is_client_api_initialized() -> bool {
    unsafe { CLIENT_API.is_some() }
}

/// Conectar un cliente globalmente
pub fn connect_global_client(name: String) -> Result<ClientId, &'static str> {
    let api = get_client_api()?;
    api.connect_client(name)
}

/// Desconectar un cliente globalmente
pub fn disconnect_global_client(client_id: ClientId) -> Result<(), &'static str> {
    let api = get_client_api()?;
    api.disconnect_client(client_id)
}

/// Obtener ventana bajo un punto globalmente
pub fn get_global_window_at(point: Point) -> Result<Option<WindowId>, &'static str> {
    let api = get_client_api()?;
    Ok(api.get_window_at(point))
}
