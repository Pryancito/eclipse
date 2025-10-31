//! Protocolo Wayland para Eclipse OS
//!
//! Implementa los mensajes y estructuras básicas del protocolo Wayland.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// ID de objeto Wayland
pub type ObjectId = u32;

/// ID de interfaz Wayland
pub type InterfaceId = u32;

/// Versión de interfaz
pub type Version = u32;

/// Opcode de mensaje
pub type Opcode = u16;

/// Tamaño de mensaje
pub type Size = u16;

/// Tipos de argumentos Wayland
#[derive(Debug, Clone)]
pub enum Argument {
    Int(i32),
    Uint(u32),
    Fixed(i32), // Fixed point number
    String(String),
    Object(ObjectId),
    NewId(ObjectId),
    Array(Vec<u8>),
    Fd(i32),
}

/// Mensaje Wayland
#[derive(Debug, Clone)]
pub struct Message {
    pub sender_id: ObjectId,
    pub opcode: Opcode,
    pub size: Size,
    pub arguments: Vec<Argument>,
}

impl Message {
    pub fn new(sender_id: ObjectId, opcode: Opcode) -> Self {
        Self {
            sender_id,
            opcode,
            size: 0,
            arguments: Vec::new(),
        }
    }

    pub fn add_argument(&mut self, arg: Argument) {
        self.arguments.push(arg);
    }

    pub fn calculate_size(&mut self) {
        // Calcular tamaño total del mensaje
        let mut size = 8; // Header básico

        for arg in &self.arguments {
            size += match arg {
                Argument::Int(_)
                | Argument::Uint(_)
                | Argument::Fixed(_)
                | Argument::Object(_)
                | Argument::NewId(_) => 4,
                Argument::String(s) => 4 + s.len() + (4 - (s.len() % 4)) % 4, // String + padding
                Argument::Array(a) => 4 + a.len() + (4 - (a.len() % 4)) % 4,  // Array + padding
                Argument::Fd(_) => 4,
            };
        }

        self.size = size as Size;
    }
}

/// Interfaz Wayland
pub trait WaylandInterface {
    fn get_interface_name() -> &'static str;
    fn get_version() -> Version;
    fn handle_request(&mut self, message: &Message) -> Result<(), &'static str>;
}

/// Cliente Wayland
pub struct WaylandClient {
    pub id: ObjectId,
    pub display_fd: i32,
    pub objects: Vec<ObjectId>,
}

impl WaylandClient {
    pub fn new(id: ObjectId, display_fd: i32) -> Self {
        Self {
            id,
            display_fd,
            objects: Vec::new(),
        }
    }

    pub fn send_message(&self, message: &Message) -> Result<(), &'static str> {
        // En un sistema real, aquí se enviaría el mensaje por el socket
        // Por ahora, simulamos el envío
        Ok(())
    }

    pub fn add_object(&mut self, object_id: ObjectId) {
        self.objects.push(object_id);
    }

    pub fn remove_object(&mut self, object_id: ObjectId) {
        self.objects.retain(|&id| id != object_id);
    }
}

/// Servidor Wayland
pub struct WaylandServer {
    pub clients: Vec<WaylandClient>,
    pub next_client_id: ObjectId,
    pub next_object_id: ObjectId,
}

impl WaylandServer {
    pub fn new() -> Self {
        Self {
            clients: Vec::new(),
            next_client_id: 1,
            next_object_id: 1,
        }
    }

    pub fn add_client(&mut self, display_fd: i32) -> ObjectId {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        let client = WaylandClient::new(client_id, display_fd);
        self.clients.push(client);

        client_id
    }

    pub fn remove_client(&mut self, client_id: ObjectId) {
        self.clients.retain(|client| client.id != client_id);
    }

    pub fn get_next_object_id(&mut self) -> ObjectId {
        let id = self.next_object_id;
        self.next_object_id += 1;
        id
    }

    pub fn broadcast_message(&self, message: &Message) -> Result<(), &'static str> {
        for client in &self.clients {
            client.send_message(message)?;
        }
        Ok(())
    }
}

/// Errores de Wayland
#[derive(Debug, Clone)]
pub enum WaylandError {
    InvalidObject,
    InvalidMethod,
    InvalidArgument,
    NoMemory,
    ImplementationError,
}

impl fmt::Display for WaylandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaylandError::InvalidObject => write!(f, "Invalid object"),
            WaylandError::InvalidMethod => write!(f, "Invalid method"),
            WaylandError::InvalidArgument => write!(f, "Invalid argument"),
            WaylandError::NoMemory => write!(f, "No memory"),
            WaylandError::ImplementationError => write!(f, "Implementation error"),
        }
    }
}

/// Resultado de operaciones Wayland
pub type WaylandResult<T> = Result<T, WaylandError>;
