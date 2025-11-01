//! Driver de stdin para Eclipse OS
//!
//! Este módulo implementa un buffer de entrada estándar (stdin) que
//! convierte eventos de teclado en caracteres ASCII para ser leídos
//! por la syscall read().

use alloc::collections::VecDeque;
use spin::Mutex;
use crate::debug::serial_write_str;
use super::keyboard::KeyCode;

/// Tamaño del buffer de stdin (4KB)
const STDIN_BUFFER_SIZE: usize = 4096;

/// Buffer circular de stdin
pub struct StdinBuffer {
    /// Buffer de caracteres
    buffer: VecDeque<u8>,
    /// Capacidad máxima
    capacity: usize,
    /// Echo habilitado
    echo_enabled: bool,
    /// Line discipline habilitado
    line_discipline: bool,
    /// Buffer de línea actual (antes de Enter)
    line_buffer: VecDeque<u8>,
}

impl StdinBuffer {
    /// Crear nuevo buffer de stdin
    pub const fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            capacity: STDIN_BUFFER_SIZE,
            echo_enabled: true,
            line_discipline: true,
            line_buffer: VecDeque::new(),
        }
    }

    /// Añadir carácter al buffer
    pub fn push(&mut self, ch: u8) {
        // Line discipline: procesar caracteres especiales
        if self.line_discipline {
            match ch {
                b'\n' | b'\r' => {
                    // Enter: transferir line buffer al main buffer
                    if self.echo_enabled {
                        serial_write_str("\n");
                    }
                    
                    // Copiar line_buffer al buffer principal
                    while let Some(byte) = self.line_buffer.pop_front() {
                        if self.buffer.len() < self.capacity {
                            self.buffer.push_back(byte);
                        }
                    }
                    
                    // Añadir newline
                    if self.buffer.len() < self.capacity {
                        self.buffer.push_back(b'\n');
                    }
                }
                0x08 | 0x7F => {
                    // Backspace: eliminar último carácter
                    if self.line_buffer.pop_back().is_some() {
                        if self.echo_enabled {
                            // Enviar secuencia de backspace (BS + space + BS)
                            serial_write_str("\x08 \x08");
                        }
                    }
                }
                0x03 => {
                    // Ctrl+C: limpiar línea
                    self.line_buffer.clear();
                    if self.echo_enabled {
                        serial_write_str("^C\n");
                    }
                }
                0x04 => {
                    // Ctrl+D: EOF
                    // TODO: Enviar señal EOF al proceso
                }
                _ => {
                    // Carácter normal: añadir a line buffer
                    if self.line_buffer.len() < self.capacity && ch >= 0x20 && ch < 0x7F {
                        self.line_buffer.push_back(ch);
                        if self.echo_enabled {
                            serial_write_str(core::str::from_utf8(&[ch]).unwrap_or("?"));
                        }
                    }
                }
            }
        } else {
            // Sin line discipline: añadir directamente
            if self.buffer.len() < self.capacity {
                self.buffer.push_back(ch);
                if self.echo_enabled {
                    serial_write_str(core::str::from_utf8(&[ch]).unwrap_or("?"));
                }
            }
        }
    }

    /// Leer caracteres del buffer
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut count = 0;
        
        for i in 0..buf.len() {
            if let Some(ch) = self.buffer.pop_front() {
                buf[i] = ch;
                count += 1;
            } else {
                break;
            }
        }
        
        count
    }

    /// Verificar si hay datos disponibles
    pub fn has_data(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Obtener número de bytes disponibles
    pub fn available(&self) -> usize {
        self.buffer.len()
    }

    /// Limpiar buffer
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.line_buffer.clear();
    }

    /// Habilitar/deshabilitar echo
    pub fn set_echo(&mut self, enabled: bool) {
        self.echo_enabled = enabled;
    }

    /// Habilitar/deshabilitar line discipline
    pub fn set_line_discipline(&mut self, enabled: bool) {
        self.line_discipline = enabled;
    }
}

/// Buffer global de stdin
static STDIN_BUFFER: Mutex<StdinBuffer> = Mutex::new(StdinBuffer::new());

/// Obtener el buffer de stdin
pub fn get_stdin_buffer() -> &'static Mutex<StdinBuffer> {
    &STDIN_BUFFER
}

/// Convertir KeyCode a carácter ASCII
pub fn keycode_to_ascii(key: KeyCode, shift: bool) -> Option<u8> {
    match key {
        // Letras
        KeyCode::A => Some(if shift { b'A' } else { b'a' }),
        KeyCode::B => Some(if shift { b'B' } else { b'b' }),
        KeyCode::C => Some(if shift { b'C' } else { b'c' }),
        KeyCode::D => Some(if shift { b'D' } else { b'd' }),
        KeyCode::E => Some(if shift { b'E' } else { b'e' }),
        KeyCode::F => Some(if shift { b'F' } else { b'f' }),
        KeyCode::G => Some(if shift { b'G' } else { b'g' }),
        KeyCode::H => Some(if shift { b'H' } else { b'h' }),
        KeyCode::I => Some(if shift { b'I' } else { b'i' }),
        KeyCode::J => Some(if shift { b'J' } else { b'j' }),
        KeyCode::K => Some(if shift { b'K' } else { b'k' }),
        KeyCode::L => Some(if shift { b'L' } else { b'l' }),
        KeyCode::M => Some(if shift { b'M' } else { b'm' }),
        KeyCode::N => Some(if shift { b'N' } else { b'n' }),
        KeyCode::O => Some(if shift { b'O' } else { b'o' }),
        KeyCode::P => Some(if shift { b'P' } else { b'p' }),
        KeyCode::Q => Some(if shift { b'Q' } else { b'q' }),
        KeyCode::R => Some(if shift { b'R' } else { b'r' }),
        KeyCode::S => Some(if shift { b'S' } else { b's' }),
        KeyCode::T => Some(if shift { b'T' } else { b't' }),
        KeyCode::U => Some(if shift { b'U' } else { b'u' }),
        KeyCode::V => Some(if shift { b'V' } else { b'v' }),
        KeyCode::W => Some(if shift { b'W' } else { b'w' }),
        KeyCode::X => Some(if shift { b'X' } else { b'x' }),
        KeyCode::Y => Some(if shift { b'Y' } else { b'y' }),
        KeyCode::Z => Some(if shift { b'Z' } else { b'z' }),
        
        // Números
        KeyCode::Key0 => Some(if shift { b')' } else { b'0' }),
        KeyCode::Key1 => Some(if shift { b'!' } else { b'1' }),
        KeyCode::Key2 => Some(if shift { b'@' } else { b'2' }),
        KeyCode::Key3 => Some(if shift { b'#' } else { b'3' }),
        KeyCode::Key4 => Some(if shift { b'$' } else { b'4' }),
        KeyCode::Key5 => Some(if shift { b'%' } else { b'5' }),
        KeyCode::Key6 => Some(if shift { b'^' } else { b'6' }),
        KeyCode::Key7 => Some(if shift { b'&' } else { b'7' }),
        KeyCode::Key8 => Some(if shift { b'*' } else { b'8' }),
        KeyCode::Key9 => Some(if shift { b'(' } else { b'9' }),
        
        // Espacios y control
        KeyCode::Space => Some(b' '),
        KeyCode::Enter => Some(b'\n'),
        KeyCode::Tab => Some(b'\t'),
        KeyCode::Backspace => Some(0x08),
        
        // Símbolos
        KeyCode::Minus => Some(if shift { b'_' } else { b'-' }),
        KeyCode::Equals | KeyCode::Equal => Some(if shift { b'+' } else { b'=' }),
        KeyCode::LeftBracket => Some(if shift { b'{' } else { b'[' }),
        KeyCode::RightBracket => Some(if shift { b'}' } else { b']' }),
        KeyCode::Backslash => Some(if shift { b'|' } else { b'\\' }),
        KeyCode::Semicolon => Some(if shift { b':' } else { b';' }),
        KeyCode::Quote | KeyCode::Apostrophe => Some(if shift { b'"' } else { b'\'' }),
        KeyCode::Grave => Some(if shift { b'~' } else { b'`' }),
        KeyCode::Comma => Some(if shift { b'<' } else { b',' }),
        KeyCode::Period => Some(if shift { b'>' } else { b'.' }),
        KeyCode::Slash => Some(if shift { b'?' } else { b'/' }),
        
        // Otras teclas no producen ASCII
        _ => None,
    }
}

/// Procesar evento de teclado y añadir al buffer de stdin
pub fn process_key_event(key: KeyCode, pressed: bool, shift: bool) {
    // Solo procesar teclas presionadas
    if !pressed {
        return;
    }
    
    // Convertir a ASCII
    if let Some(ascii) = keycode_to_ascii(key, shift) {
        let mut stdin = STDIN_BUFFER.lock();
        stdin.push(ascii);
    }
}

/// Leer datos de stdin (syscall read)
pub fn read_stdin(buf: &mut [u8]) -> Result<usize, &'static str> {
    let mut stdin = STDIN_BUFFER.lock();
    
    // Si no hay datos disponibles, retornar 0 (no bloqueante por ahora)
    // TODO: Implementar bloqueo real cuando no hay datos
    if !stdin.has_data() {
        return Ok(0);
    }
    
    // Leer datos disponibles
    let bytes_read = stdin.read(buf);
    
    serial_write_str(&alloc::format!(
        "STDIN: Leídos {} bytes\n",
        bytes_read
    ));
    
    Ok(bytes_read)
}

/// Inicializar el sistema de stdin
pub fn init_stdin() -> Result<(), &'static str> {
    serial_write_str("STDIN: Inicializando buffer de entrada estándar\n");
    
    // Limpiar buffer
    let mut stdin = STDIN_BUFFER.lock();
    stdin.clear();
    stdin.set_echo(true);
    stdin.set_line_discipline(true);
    
    drop(stdin);
    
    serial_write_str("STDIN: Buffer de stdin inicializado (4KB, echo + line discipline)\n");
    Ok(())
}

/// Obtener estadísticas de stdin
pub fn get_stdin_stats() -> StdinStats {
    let stdin = STDIN_BUFFER.lock();
    StdinStats {
        available_bytes: stdin.available(),
        buffer_size: STDIN_BUFFER_SIZE,
        echo_enabled: stdin.echo_enabled,
        line_discipline: stdin.line_discipline,
    }
}

/// Estadísticas de stdin
#[derive(Debug, Clone, Copy)]
pub struct StdinStats {
    pub available_bytes: usize,
    pub buffer_size: usize,
    pub echo_enabled: bool,
    pub line_discipline: bool,
}

