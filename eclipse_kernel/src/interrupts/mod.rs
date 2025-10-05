//! Sistema de interrupciones avanzado para Eclipse OS
//!
//! Este módulo proporciona un sistema completo de manejo de interrupciones
//! incluyendo PIC, APIC, manejo de IRQs y gestión de interrupciones de hardware.

pub mod pic;
pub mod apic;
pub mod irq;
pub mod handlers;
pub mod manager;

pub use manager::InterruptManager;
pub use irq::IrqManager;
pub use pic::PicManager;
pub use apic::ApicManager;
