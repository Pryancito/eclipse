//! Gestor de Framebuffer con Referencias Compartidas
//!
//! Implementa un sistema de referencias compartidas para el framebuffer
//! que permite actualizar todas las referencias cuando el framebuffer cambia.

use crate::drivers::framebuffer::{FramebufferDriver, FramebufferInfo};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Puntero atómico al framebuffer actual
static CURRENT_FRAMEBUFFER: AtomicPtr<FramebufferDriver> = AtomicPtr::new(core::ptr::null_mut());

/// Gestor de Framebuffer con Referencias Compartidas
pub struct FramebufferManager {
    current_fb: Option<Arc<FramebufferDriver>>,
}

impl FramebufferManager {
    /// Crear nuevo gestor de framebuffer
    pub fn new() -> Self {
        Self { current_fb: None }
    }

    /// Establecer el framebuffer actual
    pub fn set_framebuffer(&mut self, fb: FramebufferDriver) {
        // Crear una copia en el heap
        let fb_box = Box::new(fb);
        let fb_ptr = Box::into_raw(fb_box);

        // Actualizar el puntero atómico
        CURRENT_FRAMEBUFFER.store(fb_ptr, Ordering::SeqCst);

        // Crear Arc para referencias compartidas
        let fb_arc = unsafe { Arc::from_raw(fb_ptr) };
        self.current_fb = Some(fb_arc);
    }

    /// Obtener referencia compartida al framebuffer actual
    pub fn get_framebuffer(&self) -> Option<Arc<FramebufferDriver>> {
        self.current_fb.clone()
    }

    /// Obtener referencia compartida al framebuffer global
    pub fn get_global_framebuffer() -> Option<Arc<FramebufferDriver>> {
        let fb_ptr = CURRENT_FRAMEBUFFER.load(Ordering::SeqCst);
        if fb_ptr.is_null() {
            None
        } else {
            // Crear Arc desde el puntero (sin tomar ownership)
            Some(unsafe { Arc::from_raw(fb_ptr) })
        }
    }

    /// Actualizar información del framebuffer sin cambiar la instancia
    pub fn update_framebuffer_info(&mut self, new_info: FramebufferInfo) -> Result<(), String> {
        if let Some(fb_arc) = &self.current_fb {
            // Crear nuevo framebuffer con la información actualizada
            let mut new_fb = (**fb_arc).clone();
            new_fb.info = new_info;

            // Reemplazar el framebuffer actual (desempaquetar del Arc)
            self.set_framebuffer(new_fb);
            Ok(())
        } else {
            Err(String::from("No hay framebuffer para actualizar"))
        }
    }

    /// Verificar si el framebuffer está inicializado
    pub fn is_initialized(&self) -> bool {
        self.current_fb.is_some()
    }

    /// Obtener información del framebuffer actual
    pub fn get_framebuffer_info(&self) -> Option<FramebufferInfo> {
        self.current_fb.as_ref().map(|fb| fb.info.clone())
    }

    /// Limpiar el framebuffer actual
    pub fn clear_framebuffer(&mut self) {
        self.current_fb = None;
        CURRENT_FRAMEBUFFER.store(core::ptr::null_mut(), Ordering::SeqCst);
    }
}

impl Default for FramebufferManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Función de conveniencia para obtener el framebuffer global
pub fn get_global_framebuffer() -> Option<Arc<FramebufferDriver>> {
    FramebufferManager::get_global_framebuffer()
}

/// Función de conveniencia para establecer el framebuffer global
pub fn set_global_framebuffer(fb: FramebufferDriver) {
    let mut manager = FramebufferManager::new();
    manager.set_framebuffer(fb);
    // El manager se descarta, pero el framebuffer queda en el puntero global
}

/// Instancia global del gestor de framebuffer
static mut FRAMEBUFFER_MANAGER: Option<FramebufferManager> = None;

/// Obtener el gestor de framebuffer global
pub fn get_framebuffer_manager() -> &'static mut FramebufferManager {
    unsafe {
        if FRAMEBUFFER_MANAGER.is_none() {
            FRAMEBUFFER_MANAGER = Some(FramebufferManager::new());
        }
        FRAMEBUFFER_MANAGER.as_mut().unwrap()
    }
}
