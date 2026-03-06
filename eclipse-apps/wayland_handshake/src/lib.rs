//! Crate para probar el handshake Wayland con el compositor Eclipse.
//! El binario real está en `examples/wayland_handshake.rs` (no se ejecuta con `cargo test`).

#![no_std]

#[cfg(test)]
mod tests {
    #[test]
    fn wayland_handshake_placeholder() {
        // El test real es el example; aquí solo evitamos que Cargo ejecute el binario como test.
    }
}
