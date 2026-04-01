#![cfg_attr(target_vendor = "eclipse", no_std)]
#![cfg_attr(not(target_vendor = "eclipse"), no_main)]

#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
use std::prelude::v1::*;

// ============================================================================
// nano — editor de texto de línea de comandos para Eclipse OS
// ============================================================================

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::call::{exit, read, write};

/// Escribe una cadena en stdout (fd 1).
fn nano_print(s: &str) {
    #[cfg(target_vendor = "eclipse")]
    let _ = write(1, s.as_bytes());
    #[cfg(not(target_vendor = "eclipse"))]
    print!("{}", s);
}

/// Escribe una cadena en stderr (fd 2) seguida de nueva línea.
fn nano_eprintln(s: &str) {
    #[cfg(target_vendor = "eclipse")]
    {
        let _ = write(2, s.as_bytes());
        let _ = write(2, b"\n");
    }
    #[cfg(not(target_vendor = "eclipse"))]
    eprintln!("{}", s);
}

/// Lee un byte de stdin (fd 0). Devuelve None en EOF o error.
#[cfg(target_vendor = "eclipse")]
fn read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    match read(0, &mut buf) {
        Ok(1) => Some(buf[0]),
        _ => None,
    }
}

#[cfg(target_vendor = "eclipse")]
fn nano_main() {
    // Obtener argumentos del proceso (argv).
    let args: Vec<String> = std::env::args().collect();
    let filename: Option<&str> = args.get(1).map(|s| s.as_str());

    // Cargar el archivo si se proporcionó un nombre.
    let mut lines: Vec<String> = Vec::new();
    if let Some(path) = filename {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                for line in content.lines() {
                    lines.push(String::from(line));
                }
            }
            Err(_) => {
                // Archivo nuevo o error — empezar con un buffer vacío.
                nano_eprintln(&format!("nano: {} (archivo nuevo)", path));
            }
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }

    // Mostrar interfaz mínima.
    nano_print("\x1b[2J\x1b[H"); // limpiar pantalla
    nano_print("  Eclipse nano — editor de texto\r\n");
    nano_print("  ^X Salir  ^O Guardar  ^G Ayuda\r\n");
    nano_print("─────────────────────────────────\r\n");
    for line in &lines {
        nano_print(line);
        nano_print("\r\n");
    }
    nano_print("\r\n[nano]: modo lectura. Pulsa ^X para salir.\r\n");

    // Bucle de entrada mínimo: solo ^X para salir.
    let mut current_line: String = String::new();
    loop {
        let Some(byte) = read_byte() else { break };
        match byte {
            // Ctrl+X → salir
            24 => break,
            // Enter
            b'\n' | b'\r' => {
                lines.push(current_line.clone());
                current_line.clear();
                nano_print("\r\n");
            }
            // Backspace / DEL
            8 | 127 => {
                if !current_line.is_empty() {
                    let _ = current_line.pop();
                    nano_print("\x08 \x08");
                }
            }
            // Caracteres imprimibles
            c if c >= 0x20 => {
                // Only ASCII printable characters are safe to cast to char directly.
                // Bytes >= 0x80 are multi-byte UTF-8 sequences; ignore them for now
                // since PTY input on this platform is expected to be ASCII.
                if c.is_ascii() {
                    current_line.push(c as char);
                }
                let _ = write(1, &[c]);
            }
            _ => {}
        }
    }

    // Preguntar si guardar al salir.
    if let Some(path) = filename {
        nano_print("\r\n¿Guardar cambios? (s/n): ");
        if let Some(b) = read_byte() {
            if b == b's' || b == b'S' {
                let mut content = String::new();
                for (i, line) in lines.iter().enumerate() {
                    content.push_str(line);
                    if i + 1 < lines.len() {
                        content.push('\n');
                    }
                }
                if std::fs::write(path, content.as_bytes()).is_ok() {
                    nano_print("\r\nGuardado.\r\n");
                } else {
                    nano_eprintln("nano: error al guardar el archivo");
                }
            }
        }
    }

    exit(0);
}

#[cfg(target_vendor = "eclipse")]
fn main() {
    std::init_runtime();
    nano_main();
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    nano_eprintln("nano: solo disponible en Eclipse OS");
    std::process::exit(1);
}
