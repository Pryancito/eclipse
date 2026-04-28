//! Handlers de Smithay — un módulo por protocolo, exactamente como hace
//! cosmic-comp / anvil. Cada uno implementa el trait `*Handler` para `LabwcState`.
//!
//! El cuerpo concreto de cada handler es esencialmente *boilerplate* de Smithay
//! (delegar a `*State`); la lógica labwc-específica (focus model, raise on
//! focus, window rules, SSD) se inyecta aquí cuando hace falta.

pub mod compositor;
pub mod xdg_shell;
pub mod xdg_decoration;
pub mod layer_shell;
pub mod shm;
pub mod seat;
pub mod output;
pub mod data_device;
pub mod selection;
pub mod viewporter;
pub mod xwayland;
