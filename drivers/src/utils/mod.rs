//! Event handler and device tree.

pub mod deferred_job;
mod event_listener;
mod id_allocator;
mod irq_manager;

#[cfg(feature = "graphic")]
mod graphic_console;
#[cfg(feature = "graphic")]
mod shadow_fb;

pub mod devicetree;
pub mod dma;
pub mod dma_sync;

pub(super) use id_allocator::IdAllocator;
pub(super) use irq_manager::IrqManager;

pub use event_listener::{EventHandler, EventListener};

#[cfg(feature = "graphic")]
pub use graphic_console::GraphicConsole;
#[cfg(feature = "graphic")]
pub use shadow_fb::ShadowFramebuffer;
