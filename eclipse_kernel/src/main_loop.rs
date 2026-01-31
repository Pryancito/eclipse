//! Loop principal mejorado del kernel
//! 
//! Este módulo implementa un loop principal eficiente con:
//! - Scheduler de tareas por prioridad
//! - Gestión de energía inteligente
//! - Procesamiento ordenado de eventos
//! - Estadísticas en tiempo real
//! - Hot-reload de configuración

use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use core::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use spin::Mutex;

use crate::drivers::framebuffer::{FramebufferDriver, Color};
// use crate::drivers::usb_xhci_interrupts::{process_xhci_events, XhciEvent}; // ELIMINADO
use crate::idt::get_interrupt_stats;

/// Prioridad de tarea
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum TaskPriority {
    Critical = 0,   // Interrupciones críticas
    High = 1,       // Eventos USB, red
    Normal = 2,     // Procesamiento de procesos
    Low = 3,        // Estadísticas, UI updates
    Idle = 4,       // Tareas de fondo
}

/// Tarea programable
pub struct ScheduledTask {
    pub name: &'static str,
    pub priority: TaskPriority,
    pub interval_ms: u64,
    pub last_run: AtomicU64,
    pub enabled: AtomicBool,
    pub run_count: AtomicU64,
}

impl ScheduledTask {
    pub const fn new(name: &'static str, priority: TaskPriority, interval_ms: u64) -> Self {
        Self {
            name,
            priority,
            interval_ms,
            last_run: AtomicU64::new(0),
            enabled: AtomicBool::new(true),
            run_count: AtomicU64::new(0),
        }
    }

    pub fn should_run(&self, current_tick: u64) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        
        let last = self.last_run.load(Ordering::Relaxed);
        current_tick.saturating_sub(last) >= self.interval_ms
    }

    pub fn mark_run(&self, current_tick: u64) {
        self.last_run.store(current_tick, Ordering::Relaxed);
        self.run_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }
}

/// Estadísticas del loop principal
#[derive(Debug, Clone, Copy)]
pub struct MainLoopStats {
    pub total_iterations: u64,
    pub total_ticks: u64,
    pub idle_iterations: u64,
    pub tasks_executed: u64,
    pub events_processed: u64,
    pub avg_tasks_per_iteration: f32,
}

impl MainLoopStats {
    pub fn new() -> Self {
        Self {
            total_iterations: 0,
            total_ticks: 0,
            idle_iterations: 0,
            tasks_executed: 0,
            events_processed: 0,
            avg_tasks_per_iteration: 0.0,
        }
    }

    pub fn update_averages(&mut self) {
        if self.total_iterations > 0 {
            self.avg_tasks_per_iteration = 
                self.tasks_executed as f32 / self.total_iterations as f32;
        }
    }
}

/// Configuración del loop principal
pub struct MainLoopConfig {
    pub enable_power_management: bool,
    pub enable_statistics: bool,
    pub statistics_interval_ms: u64,
    pub idle_threshold_iterations: u64,
    pub max_events_per_iteration: usize,
}

impl Default for MainLoopConfig {
    fn default() -> Self {
        Self {
            enable_power_management: true,
            enable_statistics: true,
            statistics_interval_ms: 5000,  // 5 segundos
            idle_threshold_iterations: 100,
            max_events_per_iteration: 50,
        }
    }
}

/// Estado del loop principal
pub struct MainLoopState {
    pub tick_counter: AtomicU64,
    pub stats_iterations: AtomicU64,
    pub stats_tasks_executed: AtomicU64,
    pub stats_events_processed: AtomicU64,
    pub stats_idle_iterations: AtomicU64,
    pub running: AtomicBool,
    pub xhci_initialized: AtomicBool,
    pub enable_power_management: AtomicBool,
    pub enable_statistics: AtomicBool,
}

impl MainLoopState {
    pub const fn new() -> Self {
        Self {
            tick_counter: AtomicU64::new(0),
            stats_iterations: AtomicU64::new(0),
            stats_tasks_executed: AtomicU64::new(0),
            stats_events_processed: AtomicU64::new(0),
            stats_idle_iterations: AtomicU64::new(0),
            running: AtomicBool::new(true),
            xhci_initialized: AtomicBool::new(false),
            enable_power_management: AtomicBool::new(true),
            enable_statistics: AtomicBool::new(true),
        }
    }

    pub fn get_tick(&self) -> u64 {
        self.tick_counter.load(Ordering::Relaxed)
    }

    pub fn increment_tick(&self) {
        self.tick_counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Estado global del loop principal
pub static MAIN_LOOP_STATE: MainLoopState = MainLoopState::new();

/// Tareas programadas
static TASK_PROCESS_XHCI: ScheduledTask = ScheduledTask::new(
    "process_xhci_events",
    TaskPriority::High,
    10,  // cada 10 ticks
);

static TASK_INTERRUPT_STATS: ScheduledTask = ScheduledTask::new(
    "interrupt_statistics",
    TaskPriority::Low,
    1000,  // cada 1000 ticks
);

static TASK_PROCESS_STATS: ScheduledTask = ScheduledTask::new(
    "process_statistics",
    TaskPriority::Normal,
    500,  // cada 500 ticks
);

static TASK_MODULE_STATS: ScheduledTask = ScheduledTask::new(
    "module_statistics",
    TaskPriority::Low,
    1000,  // cada 1000 ticks
);

static TASK_MAIN_LOOP_STATS: ScheduledTask = ScheduledTask::new(
    "main_loop_statistics",
    TaskPriority::Idle,
    5000,  // cada 5000 ticks
);

static TASK_POWER_MANAGEMENT: ScheduledTask = ScheduledTask::new(
    "power_management",
    TaskPriority::Normal,
    100,  // cada 100 ticks
);

static TASK_KEYBOARD_INPUT: ScheduledTask = ScheduledTask::new(
    "keyboard_input",
    TaskPriority::High,
    5,  // cada 5 ticks (muy frecuente para input responsivo)
);

static TASK_MOUSE_INPUT: ScheduledTask = ScheduledTask::new(
    "mouse_input",
    TaskPriority::High,

    5,  // cada 5 ticks (muy frecuente para input responsivo)
);

static TASK_TERMINAL_UPDATE: ScheduledTask = ScheduledTask::new(
    "terminal_update",
    TaskPriority::Normal,
    30, // 30ms (~30 FPS)
);

/// Procesa eventos XHCI mediante polling simple (sin interrupciones)
fn process_xhci_task(_fb: &mut FramebufferDriver) -> usize {
    // XHCI deshabilitado temporalmente.
    // El código usb_xhci_interrupts.rs fue eliminado porque causaba kernel panics
    // debido a problemas de concurrencia (deadlocks y race conditions).
    //
    // Los dispositivos USB (teclado y ratón) funcionan mediante el InputSystem
    // que ya está operativo y probado.
    //
    // TODO: Re-implementar soporte XHCI usando polling simple o un ring buffer lock-free.
    0
}

/// Muestra estadísticas de interrupciones
fn show_interrupt_stats(fb: &mut FramebufferDriver) {
    let stats = get_interrupt_stats();
    
    if stats.total_interrupts > 0 {
        fb.write_text_kernel(
            &format!(
                "📊 IRQ: Total={}, Timer={}, KB={}, Syscalls={}",
                stats.total_interrupts,
                stats.timer_interrupts,
                stats.keyboard_interrupts,
                stats.syscalls
            ),
            Color::CYAN,
        );
    }
}

/// Procesa eventos de teclado
fn process_keyboard_task(fb: &mut FramebufferDriver) -> usize {
    use crate::drivers::input_system::{with_input_system, InputEventType};
    use crate::drivers::usb_keyboard::KeyboardEvent;
    
    let mut events_processed = 0;
    
    // Procesar eventos del InputSystem global
    if let Some(count) = with_input_system(|input_sys| {
        // Procesar todos los eventos de entrada
        if input_sys.process_events().is_ok() {
            let mut count = 0;
            
            // Obtener eventos de teclado del buffer
            while let Some(mut event) = input_sys.get_next_event() {
                if let InputEventType::Keyboard(kbd_event) = &event.event_type {
                    // Procesar el evento de teclado
                    if crate::window_system::is_window_system_initialized() {
                        // Enviar al sistema de ventanas
                        let _ = crate::window_system::event_system::simulate_global_key_press(
                             kbd_event.key_code as u8 as u32,
                             kbd_event.pressed
                        );
                    } else if kbd_event.pressed {
                        fb.write_text_kernel(
                            &alloc::format!(
                                "⌨️  Tecla: {:?} (char: {:?})",
                                kbd_event.key_code,
                                kbd_event.character
                            ),
                            Color::YELLOW,
                        );
                    }
                    count += 1;
                    event.mark_processed();
                }
                
                // Limitar eventos procesados para no saturar
                if count >= 10 {
                    break;
                }
            }
            count
        } else {
            0
        }
    }) {
        events_processed = count;
    }
    
    events_processed
}

/// Procesa eventos de ratón
fn process_mouse_task(fb: &mut FramebufferDriver) -> usize {
    use crate::drivers::input_system::{with_input_system, InputEventType};
    use crate::drivers::usb_mouse::MouseEvent;
    
    let mut events_processed = 0;
    
    // Procesar eventos del InputSystem global
    if let Some(count) = with_input_system(|input_sys| {
        let mut count = 0;
        
        // Obtener eventos de ratón del buffer (ya fueron procesados por InputSystem)
        let events: Vec<_> = input_sys.event_buffer
            .iter()
            .filter(|e| e.is_mouse() && !e.processed)
            .take(10)
            .cloned()
            .collect();
        
        for event in events {
            if let InputEventType::Mouse(mouse_event) = &event.event_type {
                match mouse_event {
                    MouseEvent::Move { position, buttons } => {
                        // Solo mostrar si hay botones presionados (para no saturar)
                        if crate::window_system::is_window_system_initialized() {
                             let _ = crate::window_system::event_system::simulate_global_mouse_move(position.x, position.y);
                        } else if buttons.left || buttons.right || buttons.middle {
                            fb.write_text_kernel(
                                &alloc::format!(
                                    "🖱️  Ratón: ({}, {}) [L:{} R:{} M:{}]",
                                    position.x, position.y,
                                    if buttons.left { "✓" } else { " " },
                                    if buttons.right { "✓" } else { " " },
                                    if buttons.middle { "✓" } else { " " }
                                ),
                                Color::CYAN,
                            );
                        }
                        count += 1;
                    }
                    MouseEvent::ButtonPress { button, position } => {
                        if crate::window_system::is_window_system_initialized() {
                             let _ = crate::window_system::event_system::simulate_global_mouse_click(*button as u8, true);
                        } else {
                            fb.write_text_kernel(
                                &alloc::format!("🖱️  Click {:?} en ({}, {})", button, position.x, position.y),
                                Color::GREEN,
                            );
                        }
                        count += 1;
                    }
                    MouseEvent::ButtonRelease { button, position: _ } => {
                        if crate::window_system::is_window_system_initialized() {
                             let _ = crate::window_system::event_system::simulate_global_mouse_click(*button as u8, false);
                        }
                        count += 1;
                    }
                    MouseEvent::Scroll { delta, position: _ } => {
                        fb.write_text_kernel(
                            &alloc::format!("🖱️  Scroll: {}", delta),
                            Color::BLUE,
                        );
                        count += 1;
                    }
                }
            }
        }
        count
    }) {
        events_processed = count;
    }
    
    events_processed
}

/// Gestión de energía
fn power_management_task() {
    let idle_iters = MAIN_LOOP_STATE.stats_idle_iterations.load(Ordering::Relaxed);
    
    // Si hemos estado idle por mucho tiempo, reducir frecuencia de polling
    if idle_iters > 1000 {
        // Aquí se podría implementar reducción de frecuencia de CPU, etc.
        // Por ahora solo lo registramos
    }
}

fn process_terminal_task(_fb: &mut FramebufferDriver) -> usize {
    if crate::window_system::is_window_system_initialized() {
        crate::apps::terminal::update_terminal();
    }
    0
}

/// Ejecuta todas las tareas programadas
fn run_scheduled_tasks(fb: &mut FramebufferDriver) -> usize {
    let current_tick = MAIN_LOOP_STATE.get_tick();
    let mut tasks_run: usize = 0;
    let mut events_processed: usize = 0;
    
    // Tareas críticas y de alta prioridad
    let tasks = [
        (&TASK_KEYBOARD_INPUT, process_keyboard_task as fn(&mut FramebufferDriver) -> usize),
        (&TASK_MOUSE_INPUT, process_mouse_task as fn(&mut FramebufferDriver) -> usize),
        (&TASK_PROCESS_XHCI, process_xhci_task as fn(&mut FramebufferDriver) -> usize),
        (&TASK_TERMINAL_UPDATE, process_terminal_task as fn(&mut FramebufferDriver) -> usize),
        (&TASK_POWER_MANAGEMENT, power_management_task_wrapper as fn(&mut FramebufferDriver) -> usize),
    ];

    // 1. El patrón es (task, handler), como sugiere el compilador
    for (task, handler) in tasks.iter() {
        if task.should_run(current_tick) {
            
            // 2. Desreferencia 'handler' con (*) para poder llamarlo
            let events = (*handler)(fb);
            
            events_processed = events_processed.saturating_add(events);
            task.mark_run(current_tick);
            tasks_run = tasks_run.saturating_add(1);
        }
    }
    // Actualizar estadísticas
    MAIN_LOOP_STATE.stats_tasks_executed.fetch_add(tasks_run as u64, Ordering::Relaxed);
    MAIN_LOOP_STATE.stats_events_processed.fetch_add(events_processed as u64, Ordering::Relaxed);
    
    if tasks_run == 0 {
        MAIN_LOOP_STATE.stats_idle_iterations.fetch_add(1, Ordering::Relaxed);
    } else {
        MAIN_LOOP_STATE.stats_idle_iterations.store(0, Ordering::Relaxed);
    }
    
    tasks_run
}

/// Loop principal mejorado
pub fn main_loop(fb: &mut FramebufferDriver, xhci_initialized: bool) -> ! {
    use crate::debug::serial_write_str;
    
    serial_write_str("MAIN_LOOP: ========================================\n");
    serial_write_str("MAIN_LOOP: INICIADO - Con soporte USB HID polling\n");
    serial_write_str("MAIN_LOOP: ========================================\n");
    
    fb.write_text_kernel("", Color::WHITE);
    fb.write_text_kernel("========================================", Color::CYAN);
    fb.write_text_kernel("   ECLIPSE OS - MAIN LOOP ACTIVO", Color::GREEN);
    fb.write_text_kernel("   Teclado/Raton USB listo (polling)", Color::WHITE);
    fb.write_text_kernel("========================================", Color::CYAN);
    fb.write_text_kernel("", Color::WHITE);
    
    let mut counter = 0u64;
    let mut last_hid_poll = 0u64;
    let mut last_stats = 0u64;
    
    loop {
        counter = counter.wrapping_add(1);

        // Ejecutar tareas programadas (input, terminal, procesos)
        run_scheduled_tasks(fb);
        
        // Procesar mensajes del microkernel cada iteración
        crate::microkernel::process_messages();
        
        // Polling USB HID cada 100,000 iteraciones (~cada ~10ms dependiendo de la CPU)
        // Solo si XHCI está inicializado
        if xhci_initialized && counter.wrapping_sub(last_hid_poll) > 100_000 {
            let events = crate::drivers::usb_hid::poll_usb_hid_devices();
            if events > 0 {
                serial_write_str(&alloc::format!("USB_HID: {} eventos procesados\n", events));
            }
            last_hid_poll = counter;
        }


        
        // Mostrar estadísticas cada ~200 millones de iteraciones
        if counter.wrapping_sub(last_stats) > 200_000_000 {
            let (total, kbd, mouse) = crate::drivers::usb_hid::get_hid_stats();
            serial_write_str(&alloc::format!(
                "MAIN_LOOP: Activo - {} HID devices ({} kbd, {} mouse)\n", 
                total, kbd, mouse
            ));
            fb.write_text_kernel("Sistema estable - loop activo", Color::GREEN);
            last_stats = counter;
        }
        
        // Renderizar sistema de ventanas si está activo
        if crate::window_system::is_window_system_initialized() {
            if let Err(e) = crate::window_system::compositor::render_global_to_framebuffer(fb) {
                 // Si falla, loguear en pantalla
                 fb.write_text_kernel(
                     &alloc::format!("RENDER ERROR: {}", e),
                     Color::RED,
                 );
            }
        }

        // Pausa breve para no saturar la CPU
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }
}

/// Habilita una tarea específica
pub fn enable_task(task_name: &str) {
    match task_name {
        "keyboard" => TASK_KEYBOARD_INPUT.enable(),
        "mouse" => TASK_MOUSE_INPUT.enable(),
        "xhci" => TASK_PROCESS_XHCI.enable(),
        "interrupt_stats" => TASK_INTERRUPT_STATS.enable(),
        "process_stats" => TASK_PROCESS_STATS.enable(),
        "module_stats" => TASK_MODULE_STATS.enable(),
        "loop_stats" => TASK_MAIN_LOOP_STATS.enable(),
        "power" => TASK_POWER_MANAGEMENT.enable(),
        _ => {},
    }
}

/// Deshabilita una tarea específica
pub fn disable_task(task_name: &str) {
    match task_name {
        "keyboard" => TASK_KEYBOARD_INPUT.disable(),
        "mouse" => TASK_MOUSE_INPUT.disable(),
        "xhci" => TASK_PROCESS_XHCI.disable(),
        "interrupt_stats" => TASK_INTERRUPT_STATS.disable(),
        "process_stats" => TASK_PROCESS_STATS.disable(),
        "module_stats" => TASK_MODULE_STATS.disable(),
        "loop_stats" => TASK_MAIN_LOOP_STATS.disable(),
        "power" => TASK_POWER_MANAGEMENT.disable(),
        _ => {},
    }
}

/// Obtiene estadísticas actuales del loop
pub fn get_stats() -> MainLoopStats {
    let iterations = MAIN_LOOP_STATE.stats_iterations.load(Ordering::Relaxed);
    let tasks = MAIN_LOOP_STATE.stats_tasks_executed.load(Ordering::Relaxed);
    let events = MAIN_LOOP_STATE.stats_events_processed.load(Ordering::Relaxed);
    let idle = MAIN_LOOP_STATE.stats_idle_iterations.load(Ordering::Relaxed);
    
    let mut stats = MainLoopStats::new();
    stats.total_iterations = iterations;
    stats.total_ticks = MAIN_LOOP_STATE.get_tick();
    stats.tasks_executed = tasks;
    stats.events_processed = events;
    stats.idle_iterations = idle;
    stats.update_averages();
    
    stats
}

/// Reinicia estadísticas
pub fn reset_stats() {
    MAIN_LOOP_STATE.stats_iterations.store(0, Ordering::Relaxed);
    MAIN_LOOP_STATE.stats_tasks_executed.store(0, Ordering::Relaxed);
    MAIN_LOOP_STATE.stats_events_processed.store(0, Ordering::Relaxed);
    MAIN_LOOP_STATE.stats_idle_iterations.store(0, Ordering::Relaxed);
}

/// Habilita/deshabilita gestión de energía
pub fn set_power_management(enabled: bool) {
    MAIN_LOOP_STATE.enable_power_management.store(enabled, Ordering::Relaxed);
}

/// Habilita/deshabilita estadísticas
pub fn set_statistics(enabled: bool) {
    MAIN_LOOP_STATE.enable_statistics.store(enabled, Ordering::Relaxed);
}

/// Muestra estadísticas de procesos
fn show_process_stats(_fb: &mut FramebufferDriver) {
    crate::debug::serial_write_str("MAIN_LOOP: show_process_stats ejecutado\n");
}

/// Muestra estadísticas de módulos
fn show_module_stats(_fb: &mut FramebufferDriver) {
    crate::debug::serial_write_str("MAIN_LOOP: show_module_stats ejecutado\n");
}

/// Muestra estadísticas del loop principal
fn show_main_loop_stats(_fb: &mut FramebufferDriver) {
    crate::debug::serial_write_str("MAIN_LOOP: show_main_loop_stats ejecutado\n");
}

fn show_interrupt_stats_wrapper(fb: &mut FramebufferDriver) -> usize {
    show_interrupt_stats(fb);
    0
}

fn show_process_stats_wrapper(fb: &mut FramebufferDriver) -> usize {
    crate::debug::serial_write_str("MAIN_LOOP: show_process_stats ejecutado\n");
    0
}

fn show_module_stats_wrapper(fb: &mut FramebufferDriver) -> usize {
    crate::debug::serial_write_str("MAIN_LOOP: show_module_stats ejecutado\n");
    0
}

fn show_main_loop_stats_wrapper(fb: &mut FramebufferDriver) -> usize {
    crate::debug::serial_write_str("MAIN_LOOP: show_main_loop_stats ejecutado\n");
    0
}

fn power_management_task_wrapper(_fb: &mut FramebufferDriver) -> usize {
    power_management_task();
    0
}

