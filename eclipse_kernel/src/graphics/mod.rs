//! Sistema de gráficos avanzado para Eclipse OS
//! 
//! Implementa drivers de gráficos reales, sistema de ventanas,
//! widgets y aceleración por hardware.

pub mod window_system;
pub mod widgets;
pub mod nvidia_advanced;
pub mod amd_advanced;
pub mod intel_advanced;
pub mod multi_gpu_manager;
pub mod graphics_manager;

// Re-exportar componentes principales
pub use window_system::{WindowCompositor, Window, WindowId, Position, Size, Rectangle, WindowState, WindowType};
pub use widgets::{WidgetManager, Widget, WidgetId, WidgetType, WidgetState, WidgetEvent};
pub use nvidia_advanced::NvidiaAdvancedDriver;
pub use amd_advanced::AmdAdvancedDriver;
pub use intel_advanced::IntelAdvancedDriver;
pub use multi_gpu_manager::{MultiGpuManager, UnifiedGpuInfo, SupportedGpuType, MultiGpuStats};
pub use graphics_manager::GraphicsManager;
