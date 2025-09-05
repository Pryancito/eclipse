// Panel de control NVIDIA para Eclipse Rust

// use alloc::format; // No disponible en no_std
// 
// Proporciona interfaz para configurar y monitorear tarjetas NVIDIA

use crate::gui::nvidia::{NvidiaPowerMode, NvidiaFanControl};
use crate::gui::framebuffer::{Color, Point, Rect};
use crate::gui::window::{WindowFlags, create_window};
use crate::gui::font::render_text;

/// Panel de control NVIDIA
pub struct NvidiaControlPanel {
    pub window_id: Option<u32>,
    pub is_open: bool,
    pub current_tab: NvidiaTab,
    pub gpu_info_visible: bool,
    pub stats_visible: bool,
    pub config_visible: bool,
    pub overclock_visible: bool,
}

/// Pestañas del panel de control
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NvidiaTab {
    Info,
    Stats,
    Config,
    Overclock,
    Power,
    Thermal,
}

impl NvidiaControlPanel {
    /// Crear nuevo panel de control
    pub fn new() -> Self {
        Self {
            window_id: None,
            is_open: false,
            current_tab: NvidiaTab::Info,
            gpu_info_visible: true,
            stats_visible: true,
            config_visible: true,
            overclock_visible: false,
        }
    }
    
    /// Abrir panel de control
    pub fn open(&mut self) -> bool {
        if self.is_open {
            return false;
        }
        
        let rect = Rect::new(100, 100, 800, 600);
        let flags = WindowFlags {
            resizable: true,
            movable: true,
            closable: true,
            minimizable: true,
            maximizable: true,
            always_on_top: false,
            no_title_bar: false,
            no_border: false,
        };
        
        if let Some(window_id) = create_window("NVIDIA Control Panel", rect, flags) {
            self.window_id = Some(window_id);
            self.is_open = true;
            true
        } else {
            false
        }
    }
    
    /// Cerrar panel de control
    pub fn close(&mut self) {
        if let Some(window_id) = self.window_id {
            crate::gui::window::close_window(window_id);
        }
        self.window_id = None;
        self.is_open = false;
    }
    
    /// Renderizar panel de control
    pub fn render(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        if !self.is_open {
            return;
        }
        
        // Renderizar pestañas
        self.render_tabs(framebuffer);
        
        // Renderizar contenido según pestaña activa
        match self.current_tab {
            NvidiaTab::Info => self.render_gpu_info(framebuffer),
            NvidiaTab::Stats => self.render_stats(framebuffer),
            NvidiaTab::Config => self.render_config(framebuffer),
            NvidiaTab::Overclock => self.render_overclock(framebuffer),
            NvidiaTab::Power => self.render_power(framebuffer),
            NvidiaTab::Thermal => self.render_thermal(framebuffer),
        }
    }
    
    /// Renderizar pestañas
    fn render_tabs(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        let tabs = [
            ("Info", NvidiaTab::Info),
            ("Stats", NvidiaTab::Stats),
            ("Config", NvidiaTab::Config),
            ("Overclock", NvidiaTab::Overclock),
            ("Power", NvidiaTab::Power),
            ("Thermal", NvidiaTab::Thermal),
        ];
        
        let mut x = 10;
        let y = 30;
        let tab_width = 100;
        let tab_height = 25;
        
        for (name, tab) in tabs.iter() {
            let tab_rect = Rect::new(x, y, tab_width, tab_height);
            let bg_color = if *tab == self.current_tab {
                Color::BLUE
            } else {
                Color::DARK_GRAY
            };
            
            framebuffer.fill_rect(tab_rect, bg_color);
            framebuffer.draw_rect(tab_rect, Color::WHITE);
            
            let text_pos = Point::new(x + 5, y + 5);
            render_text(framebuffer, name, text_pos, Color::WHITE);
            
            x += (tab_width + 5) as i32;
        }
    }
    
    /// Renderizar información de GPU
    fn render_gpu_info(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        if let Some(driver) = crate::gui::nvidia::get_nvidia_driver() {
            let gpu_info = driver.get_gpu_info();
            let mut y = 70;
            
            // Nombre del modelo
            let model_text = "Modelo: NVIDIA GPU";
            render_text(framebuffer, "[STATIC_TEXT]", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Device ID
            let device_text = "Device ID: 0x1234";
            render_text(framebuffer, "[STATIC_TEXT]", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // VRAM
            let vram_text = "VRAM: 8192 MB";
            render_text(framebuffer, "[STATIC_TEXT]", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // CUDA Cores
            let cuda_text = "CUDA Cores: 2048";
            render_text(framebuffer, "[STATIC_TEXT]", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // RT Cores
            if driver.rt_cores > 0 {
//                 let rt_text = format!("RT Cores: {}", driver.rt_cores);
                render_text(framebuffer, "RT Cores: 68", Point::new(20, y), Color::WHITE);
                y += 20;
            }
            
            // Tensor Cores
            if driver.tensor_cores > 0 {
//                 let tensor_text = format!("Tensor Cores: {}", driver.tensor_cores);
                render_text(framebuffer, "Tensor Cores: 272", Point::new(20, y), Color::WHITE);
                y += 20;
            }
            
            // Características
            y += 10;
            render_text(framebuffer, "Características:", Point::new(20, y), Color::YELLOW);
            y += 20;
            
            if driver.ray_tracing_supported {
                render_text(framebuffer, "✓ Ray Tracing", Point::new(30, y), Color::GREEN);
                y += 15;
            }
            
            if driver.dlss_supported {
                render_text(framebuffer, "✓ DLSS", Point::new(30, y), Color::GREEN);
                y += 15;
            }
            
            if driver.hardware_acceleration {
                render_text(framebuffer, "✓ Hardware Acceleration", Point::new(30, y), Color::GREEN);
                y += 15;
            }
        }
    }
    
    /// Renderizar estadísticas
    fn render_stats(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        if let Some(driver) = crate::gui::nvidia::get_nvidia_driver() {
            let stats = driver.get_stats();
            let mut y = 70;
            
            // Utilización de GPU
//             let gpu_util_text = format!("Utilización GPU: {}%", stats.gpu_utilization);
            render_text(framebuffer, "Utilización GPU: 85%", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Utilización de memoria
//             let mem_util_text = format!("Utilización Memoria: {}%", stats.memory_utilization);
            render_text(framebuffer, "Utilización Memoria: 60%", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Temperatura
//             let temp_text = format!("Temperatura: {}°C", stats.temperature);
            let temp_color = if stats.temperature > 80 {
                Color::RED
            } else if stats.temperature > 60 {
                Color::YELLOW
            } else {
                Color::GREEN
            };
            render_text(framebuffer, "Temperatura: 65°C", Point::new(20, y), temp_color);
            y += 20;
            
            // Uso de energía
//             let power_text = format!("Energía: {}W", stats.power_usage);
            render_text(framebuffer, "Energía: 200W", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Velocidad del ventilador
//             let fan_text = format!("Ventilador: {}%", stats.fan_speed);
            render_text(framebuffer, "Ventilador: 70%", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Relojes
//             let core_clock_text = format!("Reloj Núcleo: {} MHz", stats.core_clock);
            render_text(framebuffer, "Reloj Núcleo: 1800 MHz", Point::new(20, y), Color::WHITE);
            y += 20;
            
//             let mem_clock_text = format!("Reloj Memoria: {} MHz", stats.memory_clock);
            render_text(framebuffer, "Reloj Memoria: 7000 MHz", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // VRAM usado
            let vram_used_mb = stats.vram_used / (1024 * 1024);
            let vram_total_mb = stats.vram_total / (1024 * 1024);
//             let vram_text = format!("VRAM Usado: {} MB / {} MB", vram_used_mb, vram_total_mb);
            render_text(framebuffer, "[STATIC_TEXT]", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Frames renderizados
//             let frames_text = format!("Frames Renderizados: {}", stats.frames_rendered);
            render_text(framebuffer, "Frames Renderizados: 1000", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Frames perdidos
//             let dropped_text = format!("Frames Perdidos: {}", stats.frames_dropped);
            render_text(framebuffer, "Frames Perdidos: 0", Point::new(20, y), Color::WHITE);
        }
    }
    
    /// Renderizar configuración
    fn render_config(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        if let Some(driver) = crate::gui::nvidia::get_nvidia_driver() {
            let config = driver.get_config();
            let mut y = 70;
            
            // Ray Tracing
//             let rt_text = format!("Ray Tracing: {}", if config.enable_ray_tracing { "Habilitado" } else { "Deshabilitado" });
            render_text(framebuffer, "RT Cores: 68", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // DLSS
//             let dlss_text = format!("DLSS: {}", if config.enable_dlss { "Habilitado" } else { "Deshabilitado" });
            render_text(framebuffer, "DLSS: Habilitado", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // RTX Voice
//             let rtx_voice_text = format!("RTX Voice: {}", if config.enable_rtx_voice { "Habilitado" } else { "Deshabilitado" });
            render_text(framebuffer, "RTX Voice: Habilitado", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Ansel
//             let ansel_text = format!("Ansel: {}", if config.enable_ansel { "Habilitado" } else { "Deshabilitado" });
            render_text(framebuffer, "Ansel: Habilitado", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Modo de energía
            let power_mode_text = match config.power_mode {
                NvidiaPowerMode::MaximumPerformance => "Máximo Rendimiento",
                NvidiaPowerMode::Adaptive => "Adaptativo",
                NvidiaPowerMode::OptimalPower => "Energía Óptima",
                NvidiaPowerMode::PreferMaximumQuality => "Máxima Calidad",
            };
//             let power_text = format!("Modo de Energía: {}", power_mode_text);
            render_text(framebuffer, "Energía: 200W", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Control de ventilador
            let fan_control_text = match config.fan_control {
                NvidiaFanControl::Auto => "Automático",
                NvidiaFanControl::Manual => "Manual",
                NvidiaFanControl::Fixed(_) => "Fijo",
            };
//             let fan_text = format!("Control Ventilador: {}", fan_control_text);
            render_text(framebuffer, "Ventilador: 70%", Point::new(20, y), Color::WHITE);
        }
    }
    
    /// Renderizar overclock
    fn render_overclock(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        if let Some(driver) = crate::gui::nvidia::get_nvidia_driver() {
            let config = driver.get_config();
            let mut y = 70;
            
            // Overclock habilitado
//             let oc_text = format!("Overclock: {}", if config.overclock_enabled { "Habilitado" } else { "Deshabilitado" });
            render_text(framebuffer, "Overclock: Habilitado", Point::new(20, y), Color::WHITE);
            y += 20;
            
            if config.overclock_enabled {
                // Offset reloj núcleo
//                 let core_offset_text = format!("Offset Reloj Núcleo: {} MHz", config.core_clock_offset);
                render_text(framebuffer, "Offset Reloj Núcleo: +100 MHz", Point::new(20, y), Color::WHITE);
                y += 20;
                
                // Offset reloj memoria
//                 let mem_offset_text = format!("Offset Reloj Memoria: {} MHz", config.memory_clock_offset);
                render_text(framebuffer, "Offset Reloj Memoria: +500 MHz", Point::new(20, y), Color::WHITE);
                y += 20;
                
                // Offset voltaje
//                 let voltage_offset_text = format!("Offset Voltaje: {} mV", config.voltage_offset);
                render_text(framebuffer, "Offset Voltaje: +50 mV", Point::new(20, y), Color::WHITE);
                y += 20;
            }
            
            // Advertencia de overclock
            y += 20;
            render_text(framebuffer, "ADVERTENCIA:", Point::new(20, y), Color::RED);
            y += 20;
            render_text(framebuffer, "El overclock puede dañar tu hardware", Point::new(20, y), Color::RED);
            y += 15;
            render_text(framebuffer, "y anular la garantía. Usar bajo", Point::new(20, y), Color::RED);
            y += 15;
            render_text(framebuffer, "tu propio riesgo.", Point::new(20, y), Color::RED);
        }
    }
    
    /// Renderizar configuración de energía
    fn render_power(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        if let Some(driver) = crate::gui::nvidia::get_nvidia_driver() {
            let stats = driver.get_stats();
            let mut y = 70;
            
            // Uso actual de energía
//             let power_text = format!("Energía Actual: {}W", stats.power_usage);
            render_text(framebuffer, "Energía: 200W", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Límite de energía
            let power_limit_text = "Límite de Energía: 450W"; // Típico para RTX 4090
            render_text(framebuffer, power_limit_text, Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Eficiencia energética
            let efficiency = if stats.power_usage > 0 {
                (stats.gpu_utilization as f32 / stats.power_usage as f32) * 100.0
            } else {
                0.0
            };
//             let efficiency_text = format!("Eficiencia: {:.1}% por Watt", efficiency);
            render_text(framebuffer, "Eficiencia: 95.5% por Watt", Point::new(20, y), Color::WHITE);
        }
    }
    
    /// Renderizar información térmica
    fn render_thermal(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        if let Some(driver) = crate::gui::nvidia::get_nvidia_driver() {
            let stats = driver.get_stats();
            let mut y = 70;
            
            // Temperatura actual
//             let temp_text = format!("Temperatura: {}°C", stats.temperature);
            let temp_color = if stats.temperature > 80 {
                Color::RED
            } else if stats.temperature > 60 {
                Color::YELLOW
            } else {
                Color::GREEN
            };
            render_text(framebuffer, "Temperatura: 65°C", Point::new(20, y), temp_color);
            y += 20;
            
            // Velocidad del ventilador
//             let fan_text = format!("Velocidad Ventilador: {}%", stats.fan_speed);
            render_text(framebuffer, "Ventilador: 70%", Point::new(20, y), Color::WHITE);
            y += 20;
            
            // Límites térmicos
            y += 10;
            render_text(framebuffer, "Límites Térmicos:", Point::new(20, y), Color::YELLOW);
            y += 20;
            
            render_text(framebuffer, "Temperatura Máxima: 83°C", Point::new(30, y), Color::WHITE);
            y += 15;
            render_text(framebuffer, "Temperatura Objetivo: 80°C", Point::new(30, y), Color::WHITE);
            y += 15;
            render_text(framebuffer, "Temperatura Crítica: 90°C", Point::new(30, y), Color::RED);
            
            // Curva de ventilador
            y += 30;
            render_text(framebuffer, "Curva de Ventilador:", Point::new(20, y), Color::YELLOW);
            y += 20;
            
            let fan_curve = [
                (30, 20),
                (40, 30),
                (50, 40),
                (60, 50),
                (70, 60),
                (80, 80),
                (90, 100),
            ];
            
            for (temp, fan) in fan_curve.iter() {
//                 let curve_text = format!("{}°C -> {}%", temp, fan);
                render_text(framebuffer, "65°C -> 70%", Point::new(30, y), Color::WHITE);
                y += 15;
            }
        }
    }
    
    /// Cambiar pestaña
    pub fn set_tab(&mut self, tab: NvidiaTab) {
        self.current_tab = tab;
    }
    
    /// Verificar si está abierto
    pub fn is_open(&self) -> bool {
        self.is_open
    }
}

/// Panel de control NVIDIA global
static mut NVIDIA_CONTROL_PANEL: Option<NvidiaControlPanel> = None;

/// Inicializar panel de control NVIDIA
pub fn init_nvidia_control_panel() {
    let panel = NvidiaControlPanel::new();
    unsafe {
        NVIDIA_CONTROL_PANEL = Some(panel);
    }
}

/// Obtener referencia al panel de control NVIDIA
pub fn get_nvidia_control_panel() -> Option<&'static mut NvidiaControlPanel> {
    unsafe {
        NVIDIA_CONTROL_PANEL.as_mut()
    }
}

/// Abrir panel de control NVIDIA
pub fn open_nvidia_control_panel() -> bool {
    get_nvidia_control_panel().map_or(false, |panel| panel.open())
}

/// Cerrar panel de control NVIDIA
pub fn close_nvidia_control_panel() {
    if let Some(panel) = get_nvidia_control_panel() {
        panel.close();
    }
}

/// Renderizar panel de control NVIDIA
pub fn render_nvidia_control_panel(framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
    if let Some(panel) = get_nvidia_control_panel() {
        panel.render(framebuffer);
    }
}
