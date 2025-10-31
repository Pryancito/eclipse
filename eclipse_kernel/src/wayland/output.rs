//! Outputs Wayland para Eclipse OS
//!
//! Implementa la gestión de outputs (pantallas) en Wayland.

use super::protocol::*;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

/// Output Wayland (pantalla)
pub struct WaylandOutput {
    pub id: ObjectId,
    pub name: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub scale: i32,
    pub subpixel: SubpixelLayout,
    pub transform: Transform,
    pub mode: OutputMode,
    pub position: (i32, i32),
    pub physical_size: (i32, i32), // mm
    pub make: String,
    pub model: String,
}

impl WaylandOutput {
    pub fn new(width: u32, height: u32, refresh_rate: u32) -> Self {
        Self {
            id: 0, // Se asignará cuando se registre
            name: "eclipse-display".to_string(),
            description: "Eclipse OS Display".to_string(),
            width,
            height,
            refresh_rate,
            scale: 1,
            subpixel: SubpixelLayout::Unknown,
            transform: Transform::Normal,
            mode: OutputMode::Current,
            position: (0, 0),
            physical_size: (508, 286), // 24" monitor
            make: "Eclipse".to_string(),
            model: "Eclipse Display".to_string(),
        }
    }

    /// Obtener resolución
    pub fn get_resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Establecer resolución
    pub fn set_resolution(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    /// Obtener tasa de refresco
    pub fn get_refresh_rate(&self) -> u32 {
        self.refresh_rate
    }

    /// Establecer tasa de refresco
    pub fn set_refresh_rate(&mut self, rate: u32) {
        self.refresh_rate = rate;
    }

    /// Obtener escala
    pub fn get_scale(&self) -> i32 {
        self.scale
    }

    /// Establecer escala
    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale;
    }

    /// Obtener posición
    pub fn get_position(&self) -> (i32, i32) {
        self.position
    }

    /// Establecer posición
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.position = (x, y);
    }

    /// Obtener información del modo
    pub fn get_mode_info(&self) -> ModeInfo {
        ModeInfo {
            width: self.width,
            height: self.height,
            refresh: self.refresh_rate,
            flags: ModeFlags::CURRENT,
        }
    }

    /// Enviar evento de geometría
    pub fn send_geometry(&self, client: &WaylandClient) -> Result<(), &'static str> {
        let mut message = Message::new(self.id, 0); // wl_output::geometry
        message.add_argument(Argument::Int(self.position.0));
        message.add_argument(Argument::Int(self.position.1));
        message.add_argument(Argument::Int(self.physical_size.0));
        message.add_argument(Argument::Int(self.physical_size.1));
        message.add_argument(Argument::Int(self.subpixel as i32));
        message.add_argument(Argument::String(self.make.clone()));
        message.add_argument(Argument::String(self.model.clone()));
        message.add_argument(Argument::Int(self.transform as i32));
        message.calculate_size();

        client.send_message(&message)
    }

    /// Enviar evento de modo
    pub fn send_mode(&self, client: &WaylandClient, flags: ModeFlags) -> Result<(), &'static str> {
        let mut message = Message::new(self.id, 1); // wl_output::mode
        message.add_argument(Argument::Uint(flags as u32));
        message.add_argument(Argument::Int(self.width as i32));
        message.add_argument(Argument::Int(self.height as i32));
        message.add_argument(Argument::Int(self.refresh_rate as i32));
        message.calculate_size();

        client.send_message(&message)
    }

    /// Enviar evento de escala
    pub fn send_scale(&self, client: &WaylandClient) -> Result<(), &'static str> {
        let mut message = Message::new(self.id, 2); // wl_output::scale
        message.add_argument(Argument::Int(self.scale));
        message.calculate_size();

        client.send_message(&message)
    }

    /// Enviar evento de done
    pub fn send_done(&self, client: &WaylandClient) -> Result<(), &'static str> {
        let mut message = Message::new(self.id, 3); // wl_output::done
        message.calculate_size();

        client.send_message(&message)
    }
}

/// Layout de subpíxeles
#[derive(Debug, Clone, Copy)]
pub enum SubpixelLayout {
    Unknown = 0,
    None = 1,
    HorizontalRGB = 2,
    HorizontalBGR = 3,
    VerticalRGB = 4,
    VerticalBGR = 5,
}

/// Transformación de output
#[derive(Debug, Clone, Copy)]
pub enum Transform {
    Normal = 0,
    Rotate90 = 1,
    Rotate180 = 2,
    Rotate270 = 3,
    Flipped = 4,
    Flipped90 = 5,
    Flipped180 = 6,
    Flipped270 = 7,
}

/// Modo de output
#[derive(Debug, Clone, Copy)]
pub enum OutputMode {
    Current = 0,
    Preferred = 1,
}

/// Información de modo
#[derive(Debug, Clone)]
pub struct ModeInfo {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
    pub flags: ModeFlags,
}

/// Flags de modo
#[derive(Debug, Clone, Copy)]
pub enum ModeFlags {
    CURRENT = 0x1,
    PREFERRED = 0x2,
}

/// Gestor de outputs
pub struct OutputManager {
    pub outputs: Vec<WaylandOutput>,
    pub next_output_id: ObjectId,
}

impl OutputManager {
    pub fn new() -> Self {
        Self {
            outputs: Vec::new(),
            next_output_id: 1,
        }
    }

    /// Agregar output
    pub fn add_output(&mut self, mut output: WaylandOutput) -> ObjectId {
        let id = self.next_output_id;
        self.next_output_id += 1;

        output.id = id;
        self.outputs.push(output);
        id
    }

    /// Remover output
    pub fn remove_output(&mut self, output_id: ObjectId) -> bool {
        if let Some(pos) = self.outputs.iter().position(|o| o.id == output_id) {
            self.outputs.remove(pos);
            true
        } else {
            false
        }
    }

    /// Obtener output por ID
    pub fn get_output(&self, output_id: ObjectId) -> Option<&WaylandOutput> {
        self.outputs.iter().find(|o| o.id == output_id)
    }

    /// Obtener output por ID (mutable)
    pub fn get_output_mut(&mut self, output_id: ObjectId) -> Option<&mut WaylandOutput> {
        self.outputs.iter_mut().find(|o| o.id == output_id)
    }

    /// Obtener output principal
    pub fn get_primary_output(&self) -> Option<&WaylandOutput> {
        self.outputs.first()
    }

    /// Obtener todos los outputs
    pub fn get_all_outputs(&self) -> &[WaylandOutput] {
        &self.outputs
    }
}
