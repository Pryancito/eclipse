//! Protocolo de comunicación del sistema de ventanas
//!
//! Define mensajes y protocolos para comunicación entre clientes y servidor,
//! similar a X11 y Wayland.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use super::geometry::{Point, Rectangle, Size};
use super::{ClientId, WindowId};

/// Tipo de mensaje del protocolo
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageType {
    // Mensajes del cliente al servidor
    CreateWindow,
    DestroyWindow,
    MoveWindow,
    ResizeWindow,
    SetWindowTitle,
    MapWindow,
    UnmapWindow,
    FocusWindow,
    SendEvent,

    // Mensajes del servidor al cliente
    WindowCreated,
    WindowDestroyed,
    WindowMoved,
    WindowResized,
    WindowMapped,
    WindowUnmapped,
    WindowFocused,
    EventReceived,
    Error,

    // Mensajes de ping/pong
    Ping,
    Pong,
}

/// Códigos de error del protocolo
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProtocolError {
    InvalidMessage,
    InvalidWindow,
    InvalidClient,
    PermissionDenied,
    ResourceNotFound,
    InvalidParameter,
    OutOfMemory,
    Unknown,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::InvalidMessage => write!(f, "Invalid message"),
            ProtocolError::InvalidWindow => write!(f, "Invalid window"),
            ProtocolError::InvalidClient => write!(f, "Invalid client"),
            ProtocolError::PermissionDenied => write!(f, "Permission denied"),
            ProtocolError::ResourceNotFound => write!(f, "Resource not found"),
            ProtocolError::InvalidParameter => write!(f, "Invalid parameter"),
            ProtocolError::OutOfMemory => write!(f, "Out of memory"),
            ProtocolError::Unknown => write!(f, "Unknown error"),
        }
    }
}

/// Mensaje del protocolo
#[derive(Debug, Clone)]
pub struct ProtocolMessage {
    pub message_type: MessageType,
    pub client_id: ClientId,
    pub window_id: Option<WindowId>,
    pub data: MessageData,
    pub sequence: u32,
}

/// Datos del mensaje
#[derive(Debug, Clone)]
pub enum MessageData {
    // Crear ventana
    CreateWindow {
        title: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        flags: WindowFlags,
    },

    // Destruir ventana
    DestroyWindow,

    // Mover ventana
    MoveWindow {
        x: i32,
        y: i32,
    },

    // Redimensionar ventana
    ResizeWindow {
        width: u32,
        height: u32,
    },

    // Establecer título
    SetWindowTitle {
        title: String,
    },

    // Evento de entrada
    InputEvent {
        event_type: InputEventType,
        data: InputEventData,
    },

    // Respuesta de ventana creada
    WindowCreated {
        window_id: WindowId,
    },

    // Error
    Error {
        error_code: ProtocolError,
        message: String,
    },

    // Ping/Pong
    Ping,
    Pong,
}

/// Banderas de ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowFlags {
    pub resizable: bool,
    pub movable: bool,
    pub minimizable: bool,
    pub maximizable: bool,
    pub closeable: bool,
    pub always_on_top: bool,
    pub transparent: bool,
}

impl Default for WindowFlags {
    fn default() -> Self {
        Self {
            resizable: true,
            movable: true,
            minimizable: true,
            maximizable: true,
            closeable: true,
            always_on_top: false,
            transparent: false,
        }
    }
}

/// Tipos de eventos de entrada
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEventType {
    MouseMove,
    MousePress,
    MouseRelease,
    MouseWheel,
    KeyPress,
    KeyRelease,
    FocusIn,
    FocusOut,
}

/// Datos de eventos de entrada
#[derive(Debug, Clone)]
pub enum InputEventData {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: u8,
        x: i32,
        y: i32,
    },
    MouseWheel {
        delta_x: i32,
        delta_y: i32,
    },
    Keyboard {
        key_code: u32,
        modifiers: KeyModifiers,
    },
    Focus,
}

/// Modificadores de teclado
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl Default for KeyModifiers {
    fn default() -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        }
    }
}

/// Constructor de mensajes del protocolo
pub struct MessageBuilder {
    message: ProtocolMessage,
}

impl MessageBuilder {
    pub fn new(message_type: MessageType, client_id: ClientId) -> Self {
        Self {
            message: ProtocolMessage {
                message_type,
                client_id,
                window_id: None,
                data: MessageData::Ping, // Placeholder
                sequence: 0,
            },
        }
    }

    pub fn window_id(mut self, window_id: WindowId) -> Self {
        self.message.window_id = Some(window_id);
        self
    }

    pub fn sequence(mut self, sequence: u32) -> Self {
        self.message.sequence = sequence;
        self
    }

    pub fn create_window(
        mut self,
        title: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        flags: WindowFlags,
    ) -> Self {
        self.message.data = MessageData::CreateWindow {
            title,
            x,
            y,
            width,
            height,
            flags,
        };
        self
    }

    pub fn destroy_window(mut self) -> Self {
        self.message.data = MessageData::DestroyWindow;
        self
    }

    pub fn move_window(mut self, x: i32, y: i32) -> Self {
        self.message.data = MessageData::MoveWindow { x, y };
        self
    }

    pub fn resize_window(mut self, width: u32, height: u32) -> Self {
        self.message.data = MessageData::ResizeWindow { width, height };
        self
    }

    pub fn set_window_title(mut self, title: String) -> Self {
        self.message.data = MessageData::SetWindowTitle { title };
        self
    }

    pub fn input_event(mut self, event_type: InputEventType, data: InputEventData) -> Self {
        self.message.data = MessageData::InputEvent { event_type, data };
        self
    }

    pub fn window_created(mut self, window_id: WindowId) -> Self {
        self.message.data = MessageData::WindowCreated { window_id };
        self
    }

    pub fn error(mut self, error_code: ProtocolError, message: String) -> Self {
        self.message.data = MessageData::Error {
            error_code,
            message,
        };
        self
    }

    pub fn ping(mut self) -> Self {
        self.message.data = MessageData::Ping;
        self
    }

    pub fn pong(mut self) -> Self {
        self.message.data = MessageData::Pong;
        self
    }

    pub fn build(self) -> ProtocolMessage {
        self.message
    }
}

/// Serializador de mensajes (simplificado)
pub struct MessageSerializer;

impl MessageSerializer {
    /// Serializar mensaje a bytes
    pub fn serialize(message: &ProtocolMessage) -> Result<Vec<u8>, ProtocolError> {
        // Implementación simplificada usando un formato binario básico
        let mut data = Vec::new();

        // Header del mensaje
        data.extend_from_slice(&(message.message_type.clone() as u32).to_le_bytes());
        data.extend_from_slice(&message.client_id.to_le_bytes());
        data.extend_from_slice(&message.window_id.unwrap_or(0).to_le_bytes());
        data.extend_from_slice(&message.sequence.to_le_bytes());

        // Datos específicos del mensaje
        match &message.data {
            MessageData::CreateWindow {
                title,
                x,
                y,
                width,
                height,
                flags,
            } => {
                data.push(1); // Tipo de datos
                data.extend_from_slice(&title.len().to_le_bytes());
                data.extend_from_slice(title.as_bytes());
                data.extend_from_slice(&x.to_le_bytes());
                data.extend_from_slice(&y.to_le_bytes());
                data.extend_from_slice(&width.to_le_bytes());
                data.extend_from_slice(&height.to_le_bytes());
                data.push(if flags.resizable { 1 } else { 0 });
                data.push(if flags.movable { 1 } else { 0 });
                data.push(if flags.minimizable { 1 } else { 0 });
                data.push(if flags.maximizable { 1 } else { 0 });
                data.push(if flags.closeable { 1 } else { 0 });
                data.push(if flags.always_on_top { 1 } else { 0 });
                data.push(if flags.transparent { 1 } else { 0 });
            }
            MessageData::DestroyWindow => {
                data.push(2);
            }
            MessageData::MoveWindow { x, y } => {
                data.push(3);
                data.extend_from_slice(&x.to_le_bytes());
                data.extend_from_slice(&y.to_le_bytes());
            }
            MessageData::ResizeWindow { width, height } => {
                data.push(4);
                data.extend_from_slice(&width.to_le_bytes());
                data.extend_from_slice(&height.to_le_bytes());
            }
            MessageData::SetWindowTitle { title } => {
                data.push(5);
                data.extend_from_slice(&title.len().to_le_bytes());
                data.extend_from_slice(title.as_bytes());
            }
            MessageData::InputEvent {
                event_type,
                data: event_data,
            } => {
                data.push(6);
                data.push(event_type.clone() as u8);
                // Serializar datos del evento (simplificado)
                match event_data {
                    InputEventData::MouseMove { x, y } => {
                        data.push(1);
                        data.extend_from_slice(&x.to_le_bytes());
                        data.extend_from_slice(&y.to_le_bytes());
                    }
                    InputEventData::MouseButton { button, x, y } => {
                        data.push(2);
                        data.push(*button);
                        data.extend_from_slice(&x.to_le_bytes());
                        data.extend_from_slice(&y.to_le_bytes());
                    }
                    _ => {
                        // Otros tipos de eventos (simplificado)
                        data.push(0);
                    }
                }
            }
            MessageData::WindowCreated { window_id } => {
                data.push(7);
                data.extend_from_slice(&window_id.to_le_bytes());
            }
            MessageData::Error {
                error_code,
                message,
            } => {
                data.push(8);
                data.push(error_code.clone() as u8);
                data.extend_from_slice(&message.len().to_le_bytes());
                data.extend_from_slice(message.as_bytes());
            }
            MessageData::Ping => {
                data.push(9);
            }
            MessageData::Pong => {
                data.push(10);
            }
        }

        Ok(data)
    }

    /// Deserializar mensaje desde bytes
    pub fn deserialize(data: &[u8]) -> Result<ProtocolMessage, ProtocolError> {
        if data.len() < 16 {
            return Err(ProtocolError::InvalidMessage);
        }

        let mut offset = 0;

        // Deserializar header
        let message_type_u32 = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 4;

        let message_type = match message_type_u32 {
            0 => MessageType::CreateWindow,
            1 => MessageType::DestroyWindow,
            2 => MessageType::MoveWindow,
            3 => MessageType::ResizeWindow,
            4 => MessageType::SetWindowTitle,
            5 => MessageType::SendEvent,
            6 => MessageType::WindowCreated,
            7 => MessageType::WindowDestroyed,
            8 => MessageType::WindowMoved,
            9 => MessageType::WindowResized,
            10 => MessageType::WindowMapped,
            11 => MessageType::WindowUnmapped,
            12 => MessageType::WindowFocused,
            13 => MessageType::EventReceived,
            14 => MessageType::Error,
            15 => MessageType::Ping,
            16 => MessageType::Pong,
            _ => return Err(ProtocolError::InvalidMessage),
        };

        let client_id = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 4;

        let window_id_raw = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 4;
        let window_id = if window_id_raw != 0 {
            Some(window_id_raw)
        } else {
            None
        };

        let sequence = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 4;

        // Deserializar datos específicos (simplificado)
        let message_data = if offset < data.len() {
            let data_type = data[offset];
            offset += 1;

            match data_type {
                1 => {
                    // CreateWindow
                    if offset + 4 > data.len() {
                        return Err(ProtocolError::InvalidMessage);
                    }
                    let title_len = u32::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]) as usize;
                    offset += 4;

                    if offset + title_len > data.len() {
                        return Err(ProtocolError::InvalidMessage);
                    }
                    let title =
                        String::from_utf8_lossy(&data[offset..offset + title_len]).into_owned();
                    offset += title_len;

                    if offset + 20 > data.len() {
                        return Err(ProtocolError::InvalidMessage);
                    }

                    let x = i32::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]);
                    offset += 4;
                    let y = i32::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]);
                    offset += 4;
                    let width = u32::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]);
                    offset += 4;
                    let height = u32::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]);
                    offset += 4;

                    let flags = WindowFlags {
                        resizable: data[offset] != 0,
                        movable: data[offset + 1] != 0,
                        minimizable: data[offset + 2] != 0,
                        maximizable: data[offset + 3] != 0,
                        closeable: data[offset + 4] != 0,
                        always_on_top: data[offset + 5] != 0,
                        transparent: data[offset + 6] != 0,
                    };

                    MessageData::CreateWindow {
                        title,
                        x,
                        y,
                        width,
                        height,
                        flags,
                    }
                }
                2 => MessageData::DestroyWindow,
                7 => {
                    // WindowCreated
                    if offset + 4 > data.len() {
                        return Err(ProtocolError::InvalidMessage);
                    }
                    let window_id = u32::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]);
                    MessageData::WindowCreated { window_id }
                }
                9 => MessageData::Ping,
                10 => MessageData::Pong,
                _ => MessageData::Ping, // Fallback
            }
        } else {
            MessageData::Ping
        };

        Ok(ProtocolMessage {
            message_type,
            client_id,
            window_id,
            data: message_data,
            sequence,
        })
    }
}

/// Canal de comunicación para el protocolo
pub struct ProtocolChannel {
    incoming: alloc::collections::VecDeque<ProtocolMessage>,
    outgoing: alloc::collections::VecDeque<ProtocolMessage>,
}

impl ProtocolChannel {
    pub fn new() -> Self {
        Self {
            incoming: alloc::collections::VecDeque::new(),
            outgoing: alloc::collections::VecDeque::new(),
        }
    }

    pub fn send_message(&mut self, message: ProtocolMessage) {
        self.outgoing.push_back(message);
    }

    pub fn receive_message(&mut self) -> Option<ProtocolMessage> {
        self.incoming.pop_front()
    }

    pub fn has_incoming(&self) -> bool {
        !self.incoming.is_empty()
    }

    pub fn has_outgoing(&self) -> bool {
        !self.outgoing.is_empty()
    }

    pub fn get_outgoing(&mut self) -> Option<ProtocolMessage> {
        self.outgoing.pop_front()
    }

    pub fn queue_incoming(&mut self, message: ProtocolMessage) {
        self.incoming.push_back(message);
    }
}
