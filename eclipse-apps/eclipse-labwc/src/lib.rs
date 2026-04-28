//! eclipse-labwc — labwc 0.8 clone usando **Smithay nativo** sobre Eclipse OS.
//!
//! Arquitectura (idéntica a labwc upstream, pero en Rust + Smithay):
//!
//! ```text
//!  ┌────────────────────────────────────────────────────────────────────┐
//!  │ State (state.rs)                                                   │
//!  │   ┌─────────────────────────────────────────────────────────────┐  │
//!  │   │ Smithay handlers (handlers/*.rs)                            │  │
//!  │   │   CompositorHandler, XdgShellHandler, ShmHandler,           │  │
//!  │   │   SeatHandler, OutputHandler, LayerShellHandler,            │  │
//!  │   │   XdgDecorationHandler, XwaylandHandler, DataDeviceHandler  │  │
//!  │   └─────────────────────────────────────────────────────────────┘  │
//!  │   ┌─────────────────────────────────────────────────────────────┐  │
//!  │   │ Stack/Space    (view.rs)        ← stacking WM (no tiling)   │  │
//!  │   │ SSD            (ssd.rs)         ← titlebar + 3 botones      │  │
//!  │   │ rc.xml/themerc (config.rs/theme.rs/menu.rs)                 │  │
//!  │   │ Keybindings    (key.rs/actions.rs)                          │  │
//!  │   └─────────────────────────────────────────────────────────────┘  │
//!  └────────────┬───────────────────────────────────────────────────────┘
//!               │
//!   ┌───────────┴────────────┐  calloop event loop
//!   │ backend/drm.rs         │  smithay::backend::drm + gbm + libinput
//!   │ backend/winit.rs       │  smithay::backend::winit (host Linux)
//!   │ xwayland.rs            │  smithay::xwayland
//!   └────────────────────────┘
//! ```

extern crate alloc;
pub use libc;

pub mod actions;
pub mod backend;
pub mod config;
pub mod grabs;
pub mod handlers;
pub mod key;
pub mod menu;
pub mod render;
pub mod ssd;
pub mod state;
pub mod theme;
pub mod view;
pub mod xwayland_mgr;

pub mod server;

pub const LABWC_COMPAT: &str = "eclipse-labwc 0.1.0 (compatible labwc 0.8)";
