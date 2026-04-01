//! nano — Editor de texto minimalista para Eclipse OS.
//!
//! Teclas:
//!   Flechas          → mover cursor
//!   Inicio / Fin     → inicio / fin de línea  (^A / ^E)
//!   RePág / AvPág    → desplazar vista
//!   Ctrl+S           → guardar
//!   Ctrl+Q / Ctrl+X  → salir
//!   Ctrl+K           → cortar línea
//!   Ctrl+U           → pegar línea
//!   Ctrl+G           → ir a línea (prompt)
//!   Enter            → nueva línea
//!   Backspace / Del  → borrar carácter

#![cfg_attr(target_vendor = "eclipse", no_std)]
#![cfg_attr(not(target_vendor = "eclipse"), no_main)]

#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
use std::prelude::v1::*;

// ============================================================================
// Ancho máximo de línea y tamaño de pantalla por defecto
// ============================================================================
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;
const TIOCGWINSZ:  usize  = 4;
const STATUS_ROWS: usize  = 2; // barra de título + barra de ayuda

// ============================================================================
// Estado del editor
// ============================================================================

struct Editor {
    lines:    Vec<String>,   // contenido del archivo como líneas
    cx:       usize,         // columna del cursor (0-based, en chars)
    cy:       usize,         // fila del cursor (0-based, en líneas del archivo)
    row_off:  usize,         // primera línea visible (scroll vertical)
    col_off:  usize,         // primera columna visible (scroll horizontal)
    rows:     usize,         // filas de la terminal
    cols:     usize,         // columnas de la terminal
    filename: String,
    modified: bool,
    cut_buf:  Option<String>,// buffer de Ctrl+K
    message:  String,        // mensaje de estado temporal
}

impl Editor {
    fn new(filename: &str, content: &str, rows: usize, cols: usize) -> Self {
        let lines: Vec<String> = if content.is_empty() {
            { let mut v = Vec::new(); v.push(String::new()); v }
        } else {
            content.lines().map(String::from).collect()
        };
        // Aseguramos al menos una línea
        let lines = if lines.is_empty() {
            let mut v = Vec::new(); v.push(String::new()); v
        } else { lines };
        Self {
            lines,
            cx: 0, cy: 0,
            row_off: 0, col_off: 0,
            rows, cols,
            filename: String::from(filename),
            modified: false,
            cut_buf: None,
            message: String::new(),
        }
    }

    // -------------------------------------------------------------------------
    // Número de líneas visibles en el área de edición
    // -------------------------------------------------------------------------
    fn edit_rows(&self) -> usize {
        self.rows.saturating_sub(STATUS_ROWS)
    }

    // -------------------------------------------------------------------------
    // Renderizar la pantalla completa
    // -------------------------------------------------------------------------
    fn render(&self) {
        let mut out = String::new();

        // Ocultar cursor, ir a inicio
        out.push_str("\x1b[?25l\x1b[H");

        // ── Barra de título ──────────────────────────────────────────────────
        out.push_str("\x1b[7m"); // modo inverso
        let title = if self.modified {
            format!(" NANO Eclipse OS │ {} [modificado] ", self.filename)
        } else {
            format!(" NANO Eclipse OS │ {} ", self.filename)
        };
        let title_pad = self.cols.saturating_sub(title.len());
        out.push_str(&title);
        for _ in 0..title_pad { out.push(' '); }
        out.push_str("\x1b[0m\r\n");

        // ── Área de edición ──────────────────────────────────────────────────
        let edit_rows = self.edit_rows();
        for screen_row in 0..edit_rows {
            let file_row = screen_row + self.row_off;

            out.push_str("\x1b[K"); // borrar línea

            if file_row < self.lines.len() {
                let line = &self.lines[file_row];
                // Recorte horizontal
                let chars: Vec<char> = line.chars().collect();
                let start = self.col_off.min(chars.len());
                let visible: String = chars[start..]
                    .iter()
                    .take(self.cols)
                    .collect();
                out.push_str(&visible);
            } else {
                out.push('~');
            }
            out.push_str("\r\n");
        }

        // ── Barra de estado ──────────────────────────────────────────────────
        out.push_str("\x1b[7m");
        let pos_str = format!(" L{}:C{} ", self.cy + 1, self.cx + 1);
        let msg_trunc: String = self.message.chars().take(self.cols.saturating_sub(pos_str.len() + 2)).collect();
        let pad = self.cols.saturating_sub(msg_trunc.len() + pos_str.len());
        out.push_str(&format!(" {}", msg_trunc));
        for _ in 0..pad { out.push(' '); }
        out.push_str(&pos_str);
        out.push_str("\x1b[0m\r\n");

        // ── Barra de ayuda ───────────────────────────────────────────────────
        out.push_str("\x1b[K");
        out.push_str("^S Guardar  ^Q Salir  ^K Cortar  ^U Pegar  ^G Ir a línea");

        // ── Posicionar cursor ────────────────────────────────────────────────
        let screen_cy = self.cy.saturating_sub(self.row_off);
        let screen_cx = self.cx.saturating_sub(self.col_off);
        out.push_str(&format!("\x1b[{};{}H", screen_cy + 2, screen_cx + 1));
        out.push_str("\x1b[?25h"); // mostrar cursor

        let _ = eclipse_syscall::call::write(1, out.as_bytes());
    }

    // -------------------------------------------------------------------------
    // Ajustar scroll para que el cursor sea visible
    // -------------------------------------------------------------------------
    fn scroll(&mut self) {
        let edit_rows = self.edit_rows();
        if self.cy < self.row_off {
            self.row_off = self.cy;
        }
        if self.cy >= self.row_off + edit_rows {
            self.row_off = self.cy - edit_rows + 1;
        }
        if self.cx < self.col_off {
            self.col_off = self.cx;
        }
        if self.cx >= self.col_off + self.cols {
            self.col_off = self.cx - self.cols + 1;
        }
    }

    // -------------------------------------------------------------------------
    // Guardar al disco
    // -------------------------------------------------------------------------
    fn save(&mut self) {
        let mut content = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            content.push_str(line);
            if i + 1 < self.lines.len() { content.push('\n'); }
        }
        let flags = eclipse_syscall::flag::O_WRONLY
            | eclipse_syscall::flag::O_CREAT
            | eclipse_syscall::flag::O_TRUNC;
        match eclipse_syscall::call::open(&self.filename, flags) {
            Ok(fd) => {
                let _ = eclipse_syscall::call::write(fd, content.as_bytes());
                let _ = eclipse_syscall::call::close(fd);
                self.modified = false;
                self.message = format!("Guardado: {} bytes", content.len());
            }
            Err(_) => {
                self.message = format!("ERROR: no se puede guardar '{}'", self.filename);
            }
        }
    }

    // -------------------------------------------------------------------------
    // Clamp cx al final de la línea actual
    // -------------------------------------------------------------------------
    fn clamp_cx(&mut self) {
        let line_len = self.lines.get(self.cy).map(|l| l.chars().count()).unwrap_or(0);
        if self.cx > line_len { self.cx = line_len; }
    }

    // -------------------------------------------------------------------------
    // Insertar un carácter en la posición del cursor
    // -------------------------------------------------------------------------
    fn insert_char(&mut self, c: char) {
        if self.cy >= self.lines.len() {
            self.lines.push(String::new());
        }
        let line = &mut self.lines[self.cy];
        let byte_idx = char_to_byte_idx(line, self.cx);
        line.insert(byte_idx, c);
        self.cx += 1;
        self.modified = true;
    }

    // -------------------------------------------------------------------------
    // Insertar nueva línea (Enter)
    // -------------------------------------------------------------------------
    fn insert_newline(&mut self) {
        if self.cy >= self.lines.len() {
            self.lines.push(String::new());
            self.cy += 1;
            self.cx = 0;
            self.modified = true;
            return;
        }
        let line = &mut self.lines[self.cy];
        let byte_idx = char_to_byte_idx(line, self.cx);
        let rest = line[byte_idx..].to_owned();
        line.truncate(byte_idx);
        self.cy += 1;
        self.lines.insert(self.cy, rest);
        self.cx = 0;
        self.modified = true;
    }

    // -------------------------------------------------------------------------
    // Borrar carácter antes del cursor (Backspace)
    // -------------------------------------------------------------------------
    fn delete_char_before(&mut self) {
        if self.cx == 0 {
            if self.cy == 0 { return; }
            // Unir con la línea anterior
            let line = self.lines.remove(self.cy);
            self.cy -= 1;
            let prev_len = self.lines[self.cy].chars().count();
            self.lines[self.cy].push_str(&line);
            self.cx = prev_len;
        } else {
            let line = &mut self.lines[self.cy];
            let byte_idx = char_to_byte_idx(line, self.cx);
            // Encontrar el byte del carácter anterior
            let prev_idx = line[..byte_idx].char_indices().last().map(|(i, _)| i).unwrap_or(0);
            let _ = line.drain(prev_idx..byte_idx);
            self.cx -= 1;
        }
        self.modified = true;
    }

    // -------------------------------------------------------------------------
    // Borrar carácter en la posición del cursor (Delete)
    // -------------------------------------------------------------------------
    fn delete_char_at(&mut self) {
        if self.cy >= self.lines.len() { return; }
        let line_len = self.lines[self.cy].chars().count();
        if self.cx >= line_len {
            if self.cy + 1 < self.lines.len() {
                let next = self.lines.remove(self.cy + 1);
                self.lines[self.cy].push_str(&next);
                self.modified = true;
            }
        } else {
            let line = &mut self.lines[self.cy];
            let byte_idx = char_to_byte_idx(line, self.cx);
            let next_char = line[byte_idx..].char_indices().nth(1).map(|(i, _)| byte_idx + i).unwrap_or(line.len());
            let _ = line.drain(byte_idx..next_char);
            self.modified = true;
        }
    }

    // -------------------------------------------------------------------------
    // Cortar línea (Ctrl+K)
    // -------------------------------------------------------------------------
    fn cut_line(&mut self) {
        if self.cy < self.lines.len() {
            self.cut_buf = Some(self.lines.remove(self.cy));
            if self.lines.is_empty() { self.lines.push(String::new()); }
            if self.cy >= self.lines.len() { self.cy = self.lines.len() - 1; }
            self.clamp_cx();
            self.modified = true;
            self.message = String::from("Línea cortada");
        }
    }

    // -------------------------------------------------------------------------
    // Pegar línea (Ctrl+U)
    // -------------------------------------------------------------------------
    fn paste_line(&mut self) {
        if let Some(ref buf) = self.cut_buf.clone() {
            self.lines.insert(self.cy, buf.clone());
            self.modified = true;
            self.message = String::from("Línea pegada");
        }
    }

    // -------------------------------------------------------------------------
    // Ir a línea con prompt (Ctrl+G)
    // -------------------------------------------------------------------------
    fn goto_line_prompt(&mut self) {
        let target = self.prompt("Ir a línea: ");
        if let Ok(n) = target.trim().parse::<usize>() {
            if n > 0 && n <= self.lines.len() {
                self.cy = n - 1;
                self.cx = 0;
                self.message = format!("Línea {}", n);
            } else {
                self.message = format!("Línea {} fuera de rango (1-{})", n, self.lines.len());
            }
        }
    }

    // -------------------------------------------------------------------------
    // Mini-prompt en la barra de estado
    // -------------------------------------------------------------------------
    fn prompt(&self, prompt_text: &str) -> String {
        // Dibujar prompt en última línea
        let bottom = self.rows;
        let _ = eclipse_syscall::call::write(1,
            format!("\x1b[{};1H\x1b[K{}", bottom, prompt_text).as_bytes());

        let mut input = String::new();
        let mut buf = [0u8; 1];
        loop {
            if eclipse_syscall::call::read(0, &mut buf).is_err() { break; }
            match buf[0] {
                b'\n' | b'\r' => break,
                8 | 127 => {
                    if !input.is_empty() {
                        let _ = input.pop();
                        let _ = eclipse_syscall::call::write(1, b"\x08 \x08");
                    }
                }
                b if b >= 0x20 => {
                    input.push(b as char);
                    let _ = eclipse_syscall::call::write(1, &[b]);
                }
                _ => {}
            }
        }
        input
    }

    // -------------------------------------------------------------------------
    // Bucle principal de eventos
    // -------------------------------------------------------------------------
    fn run(&mut self) -> bool {
        let mut buf = [0u8; 1];
        loop {
            self.scroll();
            self.render();

            if eclipse_syscall::call::read(0, &mut buf).is_err() {
                let _ = eclipse_syscall::call::sched_yield();
                continue;
            }

            match buf[0] {
                // Ctrl+Q / Ctrl+X — salir
                17 | 24 => {
                    if self.modified {
                        let resp = self.prompt("¿Salir sin guardar? (s/N): ");
                        if resp.trim().to_ascii_lowercase() != "s" {
                            self.message = String::from("Cancelado. Usa ^S para guardar.");
                            continue;
                        }
                    }
                    return false;
                }
                // Ctrl+S — guardar
                19 => { self.save(); }
                // Ctrl+K — cortar línea
                11 => { self.cut_line(); }
                // Ctrl+U — pegar línea
                21 => { self.paste_line(); }
                // Ctrl+G — ir a línea
                7 => { self.goto_line_prompt(); }
                // Ctrl+A — inicio de línea
                1 => { self.cx = 0; }
                // Ctrl+E — fin de línea
                5 => {
                    self.cx = self.lines.get(self.cy).map(|l| l.chars().count()).unwrap_or(0);
                }
                // Enter
                b'\n' | b'\r' => { self.insert_newline(); }
                // Backspace
                8 | 127 => { self.delete_char_before(); }
                // ESC — inicio de secuencia de escape (flechas, etc.)
                27 => {
                    self.handle_escape();
                }
                // Carácter imprimible
                b if b >= 0x20 => {
                    self.insert_char(b as char);
                }
                _ => {}
            }
        }
    }

    fn handle_escape(&mut self) {
        let mut seq = [0u8; 3];
        if eclipse_syscall::call::read(0, &mut seq[..1]).is_err() { return; }
        if seq[0] != b'[' {
            // ESC O (app cursor mode — flechas alternativas)
            if seq[0] == b'O' {
                if eclipse_syscall::call::read(0, &mut seq[..1]).is_ok() {
                    match seq[0] {
                        b'A' => self.move_cursor_up(),
                        b'B' => self.move_cursor_down(),
                        b'C' => self.move_cursor_right(),
                        b'D' => self.move_cursor_left(),
                        _ => {}
                    }
                }
            }
            return;
        }
        if eclipse_syscall::call::read(0, &mut seq[..1]).is_err() { return; }
        match seq[0] {
            b'A' => self.move_cursor_up(),
            b'B' => self.move_cursor_down(),
            b'C' => self.move_cursor_right(),
            b'D' => self.move_cursor_left(),
            b'H' => { self.cx = 0; } // Home
            b'F' => {                 // End
                self.cx = self.lines.get(self.cy).map(|l| l.chars().count()).unwrap_or(0);
            }
            b'1'..=b'9' => {
                // Secuencias extendidas: ESC [ N ~
                let first = seq[0];
                if eclipse_syscall::call::read(0, &mut seq[..1]).is_ok() && seq[0] == b'~' {
                    match first {
                        b'1' | b'7' => { self.cx = 0; } // Home
                        b'4' | b'8' => {                 // End
                            self.cx = self.lines.get(self.cy).map(|l| l.chars().count()).unwrap_or(0);
                        }
                        b'3' => { self.delete_char_at(); } // Delete
                        b'5' => {                            // RePág
                            let jump = self.edit_rows();
                            self.cy = self.cy.saturating_sub(jump);
                            self.clamp_cx();
                        }
                        b'6' => {                            // AvPág
                            let jump = self.edit_rows();
                            self.cy = (self.cy + jump).min(self.lines.len().saturating_sub(1));
                            self.clamp_cx();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn move_cursor_up(&mut self) {
        if self.cy > 0 { self.cy -= 1; self.clamp_cx(); }
    }
    fn move_cursor_down(&mut self) {
        if self.cy + 1 < self.lines.len() { self.cy += 1; self.clamp_cx(); }
    }
    fn move_cursor_right(&mut self) {
        let line_len = self.lines.get(self.cy).map(|l| l.chars().count()).unwrap_or(0);
        if self.cx < line_len {
            self.cx += 1;
        } else if self.cy + 1 < self.lines.len() {
            self.cy += 1;
            self.cx = 0;
        }
    }
    fn move_cursor_left(&mut self) {
        if self.cx > 0 {
            self.cx -= 1;
        } else if self.cy > 0 {
            self.cy -= 1;
            self.cx = self.lines.get(self.cy).map(|l| l.chars().count()).unwrap_or(0);
        }
    }
}

// ============================================================================
// Utilidades
// ============================================================================

/// Convierte un índice de carácter UTF-8 a índice de byte en la cadena.
fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Lee el tamaño de la terminal vía ioctl TIOCGWINSZ.
fn get_terminal_size() -> (usize, usize) {
    let mut winsz = [0u16; 4];
    if eclipse_syscall::call::ioctl(0, TIOCGWINSZ, winsz.as_mut_ptr() as usize).is_ok() {
        let rows = winsz[0] as usize;
        let cols = winsz[1] as usize;
        if rows > 4 && cols > 10 { return (rows, cols); }
    }
    (DEFAULT_ROWS, DEFAULT_COLS)
}

/// Lee todos los bytes de stdin/fd en un Vec<u8>.
fn read_file(path: &str) -> Vec<u8> {
    match eclipse_syscall::call::open(path, 0) {
        Ok(fd) => {
            let mut content = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match eclipse_syscall::call::read(fd, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => content.extend_from_slice(&buf[..n]),
                }
            }
            let _ = eclipse_syscall::call::close(fd);
            content
        }
        Err(_) => Vec::new(), // Nuevo archivo
    }
}

// ============================================================================
// Entrada principal
// ============================================================================

#[cfg(target_vendor = "eclipse")]
fn main() {
    // Leer argumento (nombre del archivo) desde argv del proceso
    let args = std::env::args();
    let filename_owned: String;
    let filename: &str = if let Some(f) = args.get(1) {
        filename_owned = f.clone();
        &filename_owned
    } else {
        "/tmp/noname.txt"
    };

    // Leer contenido del archivo (vacío si no existe)
    let raw = read_file(filename);
    let content = String::from_utf8_lossy(&raw);

    // Tamaño de la terminal
    let (rows, cols) = get_terminal_size();

    // Limpiar pantalla
    let _ = eclipse_syscall::call::write(1, b"\x1b[2J\x1b[H");

    // Crear y ejecutar el editor
    let mut ed = Editor::new(filename, &content, rows, cols);
    ed.message = format!("nano Eclipse OS │ {} líneas │ ^S guardar ^Q salir", ed.lines.len());
    let _ = ed.run();

    // Restaurar terminal al salir
    let _ = eclipse_syscall::call::write(1, b"\x1b[2J\x1b[H\x1b[?25h");
    eclipse_syscall::call::exit(0);
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    println!("Solo soportado en Eclipse OS");
}
