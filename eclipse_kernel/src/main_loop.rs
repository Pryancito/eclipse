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
use crate::drivers::usb_xhci_interrupts::{process_xhci_events, XhciEvent};
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

/// Procesa eventos XHCI
fn process_xhci_task(fb: &mut FramebufferDriver) -> usize {
    let mut event_count = 0;
    
    if MAIN_LOOP_STATE.xhci_initialized.load(Ordering::Relaxed) {
        event_count = process_xhci_events(|event| {
            // Mostrar solo eventos importantes
            match event {
                XhciEvent::PortStatusChange(port) => {
                    fb.write_text_kernel(
                        &format!("🔌 USB: Puerto {} cambió de estado", port),
                        Color::MAGENTA,
                    );
                }
                XhciEvent::TransferComplete(comp) if !comp.is_success() => {
                    fb.write_text_kernel(
                        &format!("⚠ USB: Error en transferencia (código {})", comp.completion_code),
                        Color::YELLOW,
                    );
                }
                XhciEvent::DeviceNotification(slot, data) => {
                    fb.write_text_kernel(
                        &format!("📱 USB: Dispositivo {} notificación 0x{:04X}", slot, data),
                        Color::CYAN,
                    );
                }
                _ => {
                    // No mostrar otros eventos para no saturar la pantalla
                }
            }
        });
        
        if event_count > 0 {
            crate::debug::serial_write_str(&format!(
                "MAIN_LOOP: Procesados {} eventos XHCI\n",
                event_count
            ));
        }
    }
    
    event_count
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

/// Muestra estadísticas de procesos
fn show_process_stats(fb: &mut FramebufferDriver) {
    let (running, ready, blocked) = crate::process::get_process_stats();
    let proc_info = crate::process::get_process_system_info();
    
    fb.write_text_kernel(
        &format!(
            "⚙️  Procesos: Run={}, Ready={}, Block={}, Total={}",
            running, ready, blocked, proc_info.total_processes
        ),
        Color::GREEN,
    );
}

/// Muestra estadísticas de módulos
fn show_module_stats(fb: &mut FramebufferDriver) {
    let modules = crate::modules::api::list_modules();
    
    fb.write_text_kernel(
        &format!("📦 Módulos cargados: {}", modules.len()),
        Color::BLUE,
    );
}

/// Muestra estadísticas del loop principal
fn show_main_loop_stats(fb: &mut FramebufferDriver) {
    let iterations = MAIN_LOOP_STATE.stats_iterations.load(Ordering::Relaxed);
    let tasks = MAIN_LOOP_STATE.stats_tasks_executed.load(Ordering::Relaxed);
    let events = MAIN_LOOP_STATE.stats_events_processed.load(Ordering::Relaxed);
    
    let avg_tasks = if iterations > 0 {
        tasks as f32 / iterations as f32
    } else {
        0.0
    };
    
    fb.write_text_kernel(
        &format!(
            "🔄 Loop: Iter={}, Tasks={}, Events={}, Avg={:.2} tasks/iter",
            iterations, tasks, events, avg_tasks
        ),
        Color::LIGHT_GRAY,
    );
    
    // Mostrar estado de cada tarea
    crate::debug::serial_write_str(&format!(
        "MAIN_LOOP_STATS: KBD={}, Mouse={}, XHCI={}, IRQ={}, Proc={}, Mod={}, Stats={}, PM={}\n",
        TASK_KEYBOARD_INPUT.run_count.load(Ordering::Relaxed),
        TASK_MOUSE_INPUT.run_count.load(Ordering::Relaxed),
        TASK_PROCESS_XHCI.run_count.load(Ordering::Relaxed),
        TASK_INTERRUPT_STATS.run_count.load(Ordering::Relaxed),
        TASK_PROCESS_STATS.run_count.load(Ordering::Relaxed),
        TASK_MODULE_STATS.run_count.load(Ordering::Relaxed),
        TASK_MAIN_LOOP_STATS.run_count.load(Ordering::Relaxed),
        TASK_POWER_MANAGEMENT.run_count.load(Ordering::Relaxed),
    ));
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
                    if kbd_event.pressed {
                        fb.write_text_kernel(
                            &alloc::format!(
                                "⌨️  Tecla: {:?} (char: {:?})",
                                kbd_event.key_code,
                                kbd_event.character
                            ),
                            Color::YELLOW,
                        );
                    } else {
                        crate::debug::serial_write_str(&alloc::format!(
                            "KBD: Key release {:?}\n", kbd_event.key_code
                        ));
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
                        if buttons.left || buttons.right || buttons.middle {
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
                        fb.write_text_kernel(
                            &alloc::format!("🖱️  Click {:?} en ({}, {})", button, position.x, position.y),
                            Color::GREEN,
                        );
                        count += 1;
                    }
                    MouseEvent::ButtonRelease { button, position: _ } => {
                        crate::debug::serial_write_str(&alloc::format!(
                            "MOUSE: Button release {:?}\n", button
                        ));
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
        crate::debug::serial_write_str("POWER: Sistema en idle, considerando reducir energía\n");
    }
}

/// Ejecuta todas las tareas programadas
fn run_scheduled_tasks(fb: &mut FramebufferDriver) -> usize {
    let current_tick = MAIN_LOOP_STATE.get_tick();
    let mut tasks_run = 0;
    let mut events_processed = 0;
    
    // Procesar tareas por prioridad
    let tasks = [
        // Tareas críticas y de alta prioridad primero
        (&TASK_KEYBOARD_INPUT, process_keyboard_task as fn(&mut FramebufferDriver) -> usize),
        (&TASK_MOUSE_INPUT, process_mouse_task as fn(&mut FramebufferDriver) -> usize),
        (&TASK_PROCESS_XHCI, process_xhci_task as fn(&mut FramebufferDriver) -> usize),
        // Tareas normales y de baja prioridad
        (&TASK_INTERRUPT_STATS, |fb| { show_interrupt_stats(fb); 0 }),
        (&TASK_PROCESS_STATS, |fb| { show_process_stats(fb); 0 }),
        (&TASK_MODULE_STATS, |fb| { show_module_stats(fb); 0 }),
        (&TASK_MAIN_LOOP_STATS, |fb| { show_main_loop_stats(fb); 0 }),
        (&TASK_POWER_MANAGEMENT, |_| { power_management_task(); 0 }),
    ];
    
    for (task, handler) in &tasks {
        if task.should_run(current_tick) {
            let events = handler(fb);
            events_processed += events;
            task.mark_run(current_tick);
            tasks_run += 1;
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
    crate::debug::serial_write_str("MAIN_LOOP: Iniciando loop principal mejorado\n");
    
    // Configurar estado inicial
    MAIN_LOOP_STATE.xhci_initialized.store(xhci_initialized, Ordering::Relaxed);
    MAIN_LOOP_STATE.running.store(true, Ordering::Relaxed);
    
    fb.write_text_kernel("🚀 Loop principal mejorado iniciado", Color::GREEN);
    fb.write_text_kernel("Sistema listo. Procesando eventos...", Color::WHITE);
    
    loop {
        // Incrementar contador de ticks
        MAIN_LOOP_STATE.increment_tick();
        
        // Actualizar estadísticas
        MAIN_LOOP_STATE.stats_iterations.fetch_add(1, Ordering::Relaxed);
        
        // Ejecutar tareas programadas
        let tasks_run = run_scheduled_tasks(fb);
        
        // Gestión de energía: si no hay tareas, usar HLT
        if tasks_run == 0 {
            if MAIN_LOOP_STATE.enable_power_management.load(Ordering::Relaxed) {
                unsafe {
                    core::arch::asm!("hlt");
                }
            }
        }
        
        // Pequeña pausa para evitar saturar la CPU
        for _ in 0..10 {
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

