#![no_std]

//! Aplicaciones interactivas para Eclipse OS
//! Incluye aplicaciones que utilizan el sistema de entrada y la aceleración 2D

pub mod interactive_apps;

// Re-exportar componentes principales
pub use interactive_apps::{InteractiveApp, InteractiveAppManager, create_app_manager};