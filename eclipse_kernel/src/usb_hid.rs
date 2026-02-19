//! USB HID (Human Interface Device) - Stub / Plan
//!
//! ## Objetivo
//! Soporte para ratones y teclados USB además de PS/2.
//!
//! ## Plan de implementación
//! 1. **Driver USB host**: EHCI o XHCI para enumerar buses USB
//! 2. **PCI**: Detectar controladores USB (class 0x0C, subclass 0x03)
//! 3. **Enumeración**: Reset, address assignment, descriptor reading
//! 4. **HID boot protocol**: Teclado (interface 1) y ratón (interface 2)
//! 5. **Polling/Interrupt**: Leer reportes y convertir a InputEvents
//! 6. **Integración**: input_service drena eventos USB además de PS/2
//!
//! ## Dependencias
//! - PCI (ya existe)
//! - IRQ para USB (MSI o legacy)
//! - Memoria para buffers DMA (descriptores)
//!
//! ## Estado actual
//! Stub: sin implementación. El input actual usa solo PS/2.

/// Inicializar soporte USB HID (stub).
pub fn init() {
    // TODO: detectar controladores USB vía PCI, init EHCI/XHCI, enumerar dispositivos HID
}
