//! Núcleo de IA del Kernel
//! 
//! Proporciona un motor de inferencia ligero para optimizar el kernel
//! basado en heurísticas y modelos estadísticos online.

use crate::serial;
use spin::Mutex;

/// Tipo de predicción solicitada
#[derive(Debug, Clone, Copy)]
pub enum PredictionType {
    CpuBurst,
    IoWait,
    AnomalyDetection,
    ThermalSpike,
    GpuLoadBalance,
}

/// Tarea de renderizado para afinidad de GPU
#[derive(Debug, Clone, Copy)]
pub enum RenderTask {
    PrimarySurface,     // Ventanas principales, shell
    SecondaryEffects,   // Fractals, ripples, moss
    BackgroundFlux,     // Nebula flux, background logic
    ComputeAI,          // Inferencia de IA pesada
}

/// Estructura de perfilado de comportamiento para procesos
#[derive(Debug, Clone, Copy)]
pub struct ProcessProfile {
    pub cpu_history: [u64; 4],
    pub io_wait_history: [u64; 4],
    pub page_fault_history: [u64; 4],
    pub syscall_history: [u64; 8], // Frecuencia de syscalls por tick
    pub last_tick_syscalls: u64,
    pub syscall_count: u64,
    pub ewma_burst: u64,           // Promedio móvil pesado (escalado x1024)
    pub last_prediction: u64,
    pub working_set_size: u64,     // Estimación de páginas activas
    pub recent_blocks: [u64; 8],   // Historial de bloques de disco accedidos
    pub is_foreground: bool,       // Hint de la GUI: ¿Este proceso tiene el foco?
    pub last_cpu: u32,             // Última CPU donde corrió (afinidad suave)
}

/// Per-CPU AI Profile for Idle Management and Thermal Prediction
#[derive(Debug, Clone, Copy)]
pub struct CpuProfile {
    pub idle_ewma: u64,           // Proverbio de duración de idle (MICROSEGUNDOS escalados x1024)
    pub last_idle_start: u64,      // TSC del inicio del último periodo idle
    pub last_idle_duration: u64,   // Duración en µs del último idle
    pub interrupt_frequency: u64,  // Ticks entre interrupciones
}

impl CpuProfile {
    pub const fn new() -> Self {
        Self {
            idle_ewma: 100_000 << 10, // Default a 100ms (long idle)
            last_idle_start: 0,
            last_idle_duration: 100_000,
            interrupt_frequency: 10,
        }
    }

    /// Predicts the duration of the next idle period in MICROSECONDS
    pub fn predict_idle_duration(&self) -> u64 {
        self.idle_ewma >> 10
    }

    /// Updates the idle EWMA with a new real duration (in µs)
    pub fn update_idle_duration(&mut self, duration_us: u64) {
        // alpha = 0.5 for fast adaptation
        let duration_scaled = duration_us << 10;
        self.idle_ewma = (self.idle_ewma + duration_scaled) / 2;
        self.last_idle_duration = duration_us;
    }
}

impl ProcessProfile {
    pub const fn new() -> Self {
        Self {
            cpu_history: [10; 4], // Default a 10 ticks
            io_wait_history: [0; 4],
            page_fault_history: [0; 4],
            syscall_history: [0; 8],
            last_tick_syscalls: 0,
            syscall_count: 0,
            ewma_burst: 10 << 10,  // Inicializar en 10 con escala x1024
            last_prediction: 10,
            working_set_size: 0,
            recent_blocks: [0; 8],
            is_foreground: false,
            last_cpu: 0,
        }
    }

    /// Predice la duración de la próxima ráfaga usando EWMA
    /// P_{n+1} = alpha * B_n + (1 - alpha) * P_n
    /// Usamos alpha = 0.5 (peso equilibrado)
    pub fn predict_burst(&self) -> u64 {
        self.ewma_burst >> 10
    }

    /// Registra el fin de una ráfaga de CPU y actualiza EWMA
    pub fn update_burst(&mut self, duration: u64) {
        // Actualizar EWMA: alpha = 1/2
        // P = (P + B) / 2
        let duration_scaled = duration << 10;
        self.ewma_burst = (self.ewma_burst + duration_scaled) / 2;

        // Desplazar historial CPU
        for i in 0..3 {
            self.cpu_history[i] = self.cpu_history[i+1];
        }
        self.cpu_history[3] = duration;

        // Desplazar historial de syscalls
        for i in 0..7 {
            self.syscall_history[i] = self.syscall_history[i+1];
        }
        self.syscall_history[7] = self.last_tick_syscalls;
        self.last_tick_syscalls = 0;
    }

    /// Registra tiempo de espera de I/O
    pub fn update_io_wait(&mut self, wait_ticks: u64) {
        for i in 0..3 {
            self.io_wait_history[i] = self.io_wait_history[i+1];
        }
        self.io_wait_history[3] = wait_ticks;
    }

    /// Registra fallos de página
    pub fn update_page_fault(&mut self) {
        self.page_fault_history[3] += 1;
    }

    /// Predice si el proceso va a bloquearse por I/O pronto
    pub fn predict_io_bound(&self) -> bool {
        let avg_io: u64 = self.io_wait_history.iter().sum::<u64>() / 4;
        let avg_cpu: u64 = self.cpu_history.iter().sum::<u64>() / 4;
        
        // Si el tiempo de I/O supera al de CPU, es un proceso IO-bound
        avg_io > avg_cpu
    }

    /// Decide el nivel de prioridad sugerido [0-9]
    /// 0-2: Alta (IO-bound / Interactivo)
    /// 3-6: Normal
    /// 7-9: Fondo (CPU-bound)
    pub fn decide_priority_level(&self) -> u8 {
        if self.is_foreground {
            return 0; // Prioridad Máxima (Turbo) para el proceso enfocado por el usuario
        }

        let avg_burst = self.predict_burst();
        let is_io = self.predict_io_bound();

        if is_io {
            return 1; // Alta prioridad para procesos que liberan CPU rápido
        }

        if avg_burst > 40 {
            return 8; // Baja prioridad para procesos que acaparan CPU
        }

        if avg_burst < 15 {
            return 2; // Alta/Media para procesos ligeros
        }

        4 // Por defecto Normal
    }

    /// Registra acceso a un bloque de disco para detectar patrones
    pub fn record_block_access(&mut self, block_num: u64) {
        // Desplazar historial
        for i in 0..7 {
            self.recent_blocks[i] = self.recent_blocks[i+1];
        }
        self.recent_blocks[7] = block_num;
    }

    /// Predice los próximos bloques probables (pre-fetching)
    pub fn predict_next_blocks(&self) -> alloc::vec::Vec<u64> {
        let mut predictions = alloc::vec::Vec::new();
        
        // Patrón 1: Lectura secuencial simple (N, N+1, N+2...)
        let b7 = self.recent_blocks[7];
        let b6 = self.recent_blocks[6];
        
        if b7 == b6 + 1 && b7 != 0 {
            predictions.push(b7 + 1);
            predictions.push(b7 + 2);
        }
        
        // Patrón 2: Lectura con zancada (stride) fija (N, N+2, N+4...)
        let stride = b7.wrapping_sub(b6);
        if stride > 1 && stride < 64 && b7 == b6 + stride {
             predictions.push(b7 + stride);
        }

        predictions
    }

    pub fn update_memory_metrics(&mut self, page_table_phys: u64) {
        let pages = crate::memory::update_working_set(page_table_phys);
        self.working_set_size = pages * 4096; // En bytes
    }

    /// Sugiere una migración de CPU para balanceo de carga
    pub fn suggest_migration(&self, current_cpu: u32) -> Option<u32> {
        let load = CPU_LOAD.lock();
        let current_load = load[current_cpu as usize];
        
        // Solo sugerir migración si la CPU actual está muy cargada (>80%)
        if current_load > 80 {
            let mut target_cpu = current_cpu;
            let mut min_load = current_load;
            
            // Buscar la CPU más libre
            for i in 0..16 {
                if load[i] < min_load {
                    min_load = load[i];
                    target_cpu = i as u32;
                }
            }
            
            // Diferencial de carga significativo (>30%) para justificar pérdida de caché
            if current_load > min_load + 30 {
                return Some(target_cpu);
            }
        }
        None
    }

    /// Sugiere una migración si la CPU actual está en peligro térmico
    pub fn suggest_thermal_migration(&self, current_cpu: u32) -> Option<u32> {
        let temp = CPU_TEMP.lock();
        let current_temp = temp[current_cpu as usize];

        // Umbral crítico: 85°C (escalado x10 = 850)
        if current_temp > 850 {
            let mut target_cpu = current_cpu;
            let mut min_temp = current_temp;

            for i in 0..16 {
                if temp[i] < min_temp {
                    min_temp = temp[i];
                    target_cpu = i as u32;
                }
            }

            // Migrar a una CPU que esté al menos 15°C más fría
            if current_temp > min_temp + 150 {
                return Some(target_cpu);
            }
        }
        None
    }

    /// Sugiere un factor de penalización (0.0 a 1.0) si el proceso es anómalo
    pub fn suggest_anomaly_throttle(&self) -> f32 {
        let history_sum: u64 = self.syscall_history.iter().sum();
        let avg_syscalls = history_sum / 8;
        
        // Si hay una ráfaga masiva comparada con la media
        if self.last_tick_syscalls > (avg_syscalls * 10).max(150) {
            return 0.5; // Reducir tiempo de CPU a la mitad
        }

        if self.last_tick_syscalls > (avg_syscalls * 20).max(300) {
            return 0.1; // Reducir tiempo de CPU al 10% (Throttling pesado)
        }

        1.0 // Sin penalización
    }
}

/// Carga estimada por CPU (0-100)
pub static CPU_LOAD: Mutex<[u32; 16]> = Mutex::new([0; 16]);

/// Temperatura estimada por CPU (Celsius * 10)
pub static CPU_TEMP: Mutex<[u32; 16]> = Mutex::new([400; 16]); // Inicia a 40°C

/// Per-CPU Profile storage (Sync via Mutex)
pub static CPU_PROFILES: Mutex<[CpuProfile; 16]> = Mutex::new([CpuProfile::new(); 16]);

/// Historial de ticks para calcular carga delta
static LAST_CPU_TICKS: Mutex<[(u64, u64); 16]> = Mutex::new([(0, 0); 16]);

/// Actualiza la carga de CPU basada en ticks reales del scheduler
pub fn update_cpu_load_metrics() {
    let mut load = CPU_LOAD.lock();
    let mut last = LAST_CPU_TICKS.lock();
    
    for i in 0..16 {
        let (total, idle) = crate::scheduler::get_cpu_ticks(i);
        let (prev_total, prev_idle) = last[i];
        
        let delta_total = total.saturating_sub(prev_total);
        let delta_idle = idle.saturating_sub(prev_idle);
        
        if delta_total > 0 {
            let busy = delta_total.saturating_sub(delta_idle);
            load[i] = (busy * 100 / delta_total) as u32;
        }
        
        last[i] = (total, idle);
    }
}

/// Estado de energía por CPU (0-100, 100=Max Performance)
pub static POWER_STATE: Mutex<[u8; 16]> = Mutex::new([100; 16]);

/// Historial de marcos de página libres para predicción de OOM
pub static FREE_FRAMES_HISTORY: Mutex<[u64; 8]> = Mutex::new([100000; 8]);

/// Contador total de anomalías detectadas por IA
pub static ANOMALY_COUNT: Mutex<u32> = Mutex::new(0);

/// Actualiza el modelo térmico y de energía del sistema
pub fn update_thermal_model() {
    // 1. Actualizar carga de CPU real (delta de ticks)
    update_cpu_load_metrics();

    let load = CPU_LOAD.lock();
    let mut temp = CPU_TEMP.lock();
    let mut power = POWER_STATE.lock();

    let has_thermal = crate::cpu::has_thermal_msrs();

    for i in 0..16 {
        let mut tcc = 100;
        let mut digital_readout = 0;
        let mut msr_ok = false;

        if has_thermal {
            // Tcc Activation Temperature (Intel default is 100C, but we read it from MSR 0x1A2)
            // IA32_TEMPERATURE_TARGET [23:16]
            let target_msr = unsafe { crate::cpu::rdmsr(0x1A2) };
            let tcc_activation = ((target_msr >> 16) & 0xFF) as u32;
            tcc = if tcc_activation == 0 { 100 } else { tcc_activation };

            // IA32_THERM_STATUS [22:16] is the digital readout (offset from Tcc)
            let status_msr = unsafe { crate::cpu::rdmsr(0x19C) };
            digital_readout = ((status_msr >> 16) & 0x7F) as u32;
            msr_ok = true;
        }
        
        // Real Temp in Celsius
        let mut real_temp = if msr_ok { tcc.saturating_sub(digital_readout) } else { 0 };
        
        // Fallback to simulation if MSR reading is invalid or unsupported
        if !msr_ok || real_temp == 0 || real_temp == tcc {
            // Simulación Térmica:
            // 1. Ganancia: Proporcional a la carga y al estado de energía actual
            let heat_gain = (load[i] * power[i] as u32) / 20;

            // 2. Disipación (Enfriamiento): Ley de enfriamiento de Newton
            let ambient_temp = 35; // 35°C ambiente
            let current_sim_temp = temp[i] / 10;
            let heat_loss = if current_sim_temp > ambient_temp {
                (current_sim_temp - ambient_temp) / 3
            } else { 0 };

            real_temp = (current_sim_temp as i32 + heat_gain as i32 - heat_loss as i32).max(ambient_temp as i32) as u32;
        }

        // Report in Tenths of Celsius (450 = 45.0 C)
        temp[i] = real_temp * 10;
        
        // Power/Load simulation for other internal logic if needed
        let cpu_load = load[i];
        
        // Gestión Inteligente de Energía (DVFS AI Hints):
        // Si la CPU está muy caliente (>80°C), bajar potencia preventivamente
        if temp[i] > 800 {
            power[i] = (power[i] as i16 - 5).max(30) as u8;
        } else if cpu_load < 20 {
            // Si la carga es baja, ahorrar energía
            power[i] = (power[i] as i16 - 2).max(50) as u8;
        } else if temp[i] < 600 {
            // Recuperar potencia si está fresca
            power[i] = (power[i] as i16 + 5).min(100) as u8;
        }
    }
}

/// Predice si habrá un pico térmico inminente en un núcleo
pub fn predict_thermal_spike(cpu_id: usize) -> bool {
    // Unify lock order: CPU_LOAD then CPU_TEMP
    let load = CPU_LOAD.lock();
    let temp = CPU_TEMP.lock();
    
    // Si la temperatura es > 75°C y la carga es > 90%, predecir pico
    temp[cpu_id] > 750 && load[cpu_id] > 90
}

/// Registra la duración del último periodo idle (llamado por el scheduler)
pub fn record_idle_duration(cpu_id: usize, duration: u64) {
    if cpu_id < 16 {
        let mut profiles = CPU_PROFILES.lock();
        profiles[cpu_id].update_idle_duration(duration);
    }
}

/// Idle Strategy decided by AI
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IdleMode {
    Poll,        // Adaptive polling (active wait with pause)
    Mwait(u32),  // MWAIT with specific hint (C-state)
    Hlt,         // Traditional HLT
}

/// Sugiere un modo de idle basado en la predicción de la IA y el tiempo hasta el próximo evento.
pub fn suggest_idle_mode(cpu_id: usize) -> IdleMode {
    let profiles = CPU_PROFILES.lock();
    let ai_prediction = if cpu_id < 16 {
         profiles[cpu_id].predict_idle_duration()
    } else {
         100_000 // Safe default
    };
    drop(profiles);

    // Consciencia de Timers: Obtener tiempo real hasta la próxima interrupción del hardware
    let timer_remaining = crate::apic::get_timer_remaining_us();

    // Heurística de decisión:

    // 1. Adaptive Polling: SOLO si el hardware timer está a punto de dispararse (< 30µs).
    //
    // IMPORTANTE: usamos `timer_remaining` directamente en lugar de
    // `ai_prediction.min(timer_remaining)`. Si usáramos el mínimo, un instante en
    // que `timer_remaining == 0` (deadline TSC ya expirado, interrupción aún no servida)
    // forzaría el modo Poll, registrando ~20 µs de idle. Eso corrompe la EWMA de
    // `ai_prediction`, que converge a < 30 µs en solo ~7 iteraciones y bloquea el
    // modo Poll de forma permanente, causando un uso de CPU del 100%.
    // Usando `timer_remaining` como único criterio, el modo Poll solo se activa cuando
    // el temporizador va a dispararse en < 30 µs, y la EWMA refleja los periodos de
    // reposo reales (≈ 1 ms) sin contaminación.
    if timer_remaining < 30 {
        return IdleMode::Poll;
    }

    // 2. MWAIT/HLT: el periodo idle esperado es el mínimo entre la predicción
    //    estadística y el tiempo restante del timer de hardware.
    //    Seleccionar el C-state óptimo según la latencia esperada (Microsegundos):
    //    < 1ms:  C1 (Latencia mínima)
    //    1-10ms: C2 (Balanceado)
    //    10-50ms: C3 (Ahorro moderado)
    //    > 50ms: C6 (Máximo ahorro de energía)
    let final_prediction = ai_prediction.min(timer_remaining);

    if final_prediction < 1_000 {
        IdleMode::Mwait(0x00) // C1
    } else if final_prediction < 10_000 {
        IdleMode::Mwait(0x10) // C2
    } else if final_prediction < 50_000 {
        IdleMode::Mwait(0x20) // C3
    } else {
        IdleMode::Mwait(0x30) // C6
    }
}

/// Sugiere un hint para MWAIT basado en la predicción de idle (legacy wrapper)
pub fn suggest_mwait_hint(cpu_id: usize) -> u32 {
    match suggest_idle_mode(cpu_id) {
        IdleMode::Mwait(hint) => hint,
        _ => 0x00,
    }
}

/// Predice la probabilidad de una amenaza de OOM (0.0 a 1.0)
pub fn predict_oom_threat() -> f32 {
    let history = FREE_FRAMES_HISTORY.lock();
    predict_oom_threat_internal(&*history)
}

/// Versión interna que no bloquea para evitar deadlocks recursivos
fn predict_oom_threat_internal(history: &[u64; 8]) -> f32 {
    // Si los marcos libres son críticamente bajos (<1000, aprox 4MB)
    let current = history[7];
    if current < 1000 { return 1.0; }
    
    // Analizar tendencia: si la tasa de consumo es alta
    let h0 = history[0];
    let h7 = history[7];
    
    if h7 < h0 {
        let consumption = h0 - h7;
        let consumption_rate = consumption / 8; // por tick
        
        // Si al ritmo actual nos quedamos sin frames en menos de 50 ticks
        if consumption_rate > 0 && h7 / consumption_rate < 50 {
            return 0.8;
        }
    }
    
    if current < 5000 { return 0.3; } // Amenaza moderada
    0.0
}

/// Actualiza el historial de memoria (llamado por el gestor de memoria)
pub fn update_memory_stats(free_frames: u64) {
    let mut history = FREE_FRAMES_HISTORY.lock();
    for i in 0..7 {
        history[i] = history[i+1];
    }
    history[7] = free_frames;
}

/// Estadísticas de fragmentación del heap
#[derive(Debug, Clone, Copy)]
pub struct HeapStats {
    pub total_allocated: usize,
    pub largest_free_block: usize,
    pub fragmentation_p: u32, // 0-100
}

pub static HEAP_METRICS: Mutex<HeapStats> = Mutex::new(HeapStats {
    total_allocated: 0,
    largest_free_block: 64 * 1024 * 1024,
    fragmentation_p: 0,
});

/// Sugiere si el kernel debe intentar una compactación/reorganización del heap
pub fn suggest_compaction() -> bool {
    let stats = HEAP_METRICS.lock();
    // Sugerir compactación si la fragmentación es > 60% o el bloque más grande es muy pequeño
    stats.fragmentation_p > 60 || stats.largest_free_block < 64 * 1024
}

pub fn update_heap_metrics(allocated: usize, largest_free: usize) {
    let mut stats = HEAP_METRICS.lock();
    stats.total_allocated = allocated;
    stats.largest_free_block = largest_free;
    
    // Fragmentación simple: ratio de (total - mayor) / total
    let total_capacity: usize = 64 * 1024 * 1024;
    let free_space = total_capacity.saturating_sub(allocated);
    if free_space > 0 {
        stats.fragmentation_p = ((free_space.saturating_sub(largest_free)) * 100 / free_space) as u32;
    }
}

/// Inicializa el núcleo de IA
pub fn init() {
    serial::serial_print("[AI-CORE] Kernel-Native AI Engine initialized.\n");
    serial::serial_print("[AI-CORE] Mode: High-Hybrid Multi-GPU Orchestration\n");
    
    // Descubrir GPUs iniciales
    let gpus = crate::pci::find_all_gpus();
    let mut perf = GPU_PERF.lock();
    for gpu in gpus {
        let is_discrete = gpu.vendor_id == 0x10DE || gpu.vendor_id == 0x1002; // NVIDIA or AMD
        serial::serial_print("[AI-CORE] GPU DISCOVERED: ");
        serial::serial_print(gpu.device_type());
        if is_discrete { serial::serial_print(" [Discrete/Performance]"); }
        serial::serial_print("\n");
        
        perf.push(GPUPerfProfile::new(gpu.bus, gpu.vendor_id, is_discrete));
    }
}

/// Analiza una syscall para detectar anomalías (DoS, ráfagas sospechosas)
pub fn audit_syscall(pid: u32, _syscall_num: u64) -> bool {
    let mut blocked = false;
    let _ = crate::process::modify_process(pid, |proc| {
        proc.ai_profile.last_tick_syscalls += 1;
        proc.ai_profile.syscall_count += 1;

        // Umbral estadístico: promediar syscalls históricas
        let history_sum: u64 = proc.ai_profile.syscall_history.iter().sum();
        let avg_syscalls = history_sum / 8;
        
        // Aumentamos el límite base y permitimos más ráfaga para PIDs del sistema (< 16)
        let mut limit = (avg_syscalls * 20).max(500);
        if pid < 16 {
            limit *= 4; // Los procesos del sistema pueden ser muy ruidosos legítimamente (E/S, Render)
        }
        
        if proc.ai_profile.last_tick_syscalls > limit {
            blocked = true;
        }
    });

    if blocked {
        {
            let mut count = ANOMALY_COUNT.lock();
            *count += 1;
        }
        serial::serial_print("[AI-CORE] ANOMALY DETECTED: PID ");
        serial::serial_print_dec(pid as u64);
        serial::serial_print(" blocked.\n");
        return false;
    }
    true
}

/// Incrementa el contador de syscalls de forma eficiente (sin clonar todo el proceso)
pub fn increment_syscall_count(pid: u32) {
    let _ = crate::process::modify_process(pid, |proc| {
        proc.ai_profile.last_tick_syscalls += 1;
        proc.ai_profile.syscall_count += 1;
    });
}

/// Establece el hint de foreground para un proceso
pub fn set_foreground_hint(pid: u32, is_fg: bool) {
    let _ = crate::process::modify_process(pid, |proc| {
        proc.ai_profile.is_foreground = is_fg;
    });
}

/// Analiza una asignación de memoria para detectar anomalías
pub fn audit_memory_allocation(pid: u32, size: u64) -> bool {
    if let Some(proc) = crate::process::get_process(pid) {
        // Umbral: No permitir asignaciones individuales > 16MB para procesos normales
        // O si ya tiene demasiados fallos de página recientemente (thrashing/leak)
        let pf_sum: u64 = proc.ai_profile.page_fault_history.iter().sum();
        
        if size > 16 * 1024 * 1024 {
             {
                 let mut count = ANOMALY_COUNT.lock();
                 *count += 1;
             }
             serial::serial_print("[AI-CORE] MEMORY ANOMALY: PID ");
             serial::serial_print_dec(pid as u64);
             serial::serial_print(" requesting massive allocation (");
             serial::serial_print_dec(size / 1024);
             serial::serial_print(" KB)\n");
             // Podríamos retornar false para denegar, pero por ahora solo logueamos
        }

        if pf_sum > 100 {
             serial::serial_print("[AI-CORE] MEMORY PRESSURE: PID ");
             serial::serial_print_dec(pid as u64);
             serial::serial_print(" is thrashing (high PF rate)\n");
        }
    }
    true
}

/// Historial de rendimiento de una GPU
#[derive(Debug, Clone, Copy)]
pub struct GPUPerfProfile {
    pub bus_id: u8,
    pub vendor_id: u16,
    pub load: u32,             // 0-100
    pub memory_used: u64,      // en bytes
    pub temperature: u32,      // Celsius * 10
    pub is_discrete: bool,     // ¿Es una GPU potente o integrada?
}

impl GPUPerfProfile {
    pub const fn new(bus_id: u8, vendor_id: u16, is_discrete: bool) -> Self {
        Self {
            bus_id,
            vendor_id,
            load: 0,
            memory_used: 0,
            temperature: 450, // 45°C idle
            is_discrete,
        }
    }
}

/// Estado de hasta 4 GPUs en el sistema
pub static GPU_PERF: Mutex<alloc::vec::Vec<GPUPerfProfile>> = Mutex::new(alloc::vec::Vec::new());

// Cached VRAM stats for the system dashboard. These are filled by the GPU drivers.
static GPU_VRAM_TOTAL_BYTES: Mutex<u64> = Mutex::new(0);
static GPU_VRAM_USED_BYTES: Mutex<u64> = Mutex::new(0);

/// Called by the GPU driver to refresh system-wide VRAM totals for the dashboard.
pub fn set_gpu_vram_stats(total_bytes: u64, used_bytes: u64) {
    *GPU_VRAM_TOTAL_BYTES.lock() = total_bytes;
    *GPU_VRAM_USED_BYTES.lock() = used_bytes;
}

/// Sugiere qué GPU debe manejar una tarea específica
pub fn suggest_gpu_affinity(task: RenderTask) -> usize {
    let perf = GPU_PERF.lock();
    if perf.is_empty() { return 0; }
    if perf.len() == 1 { return 0; }

    // Heurística de afinidad:
    // 1. Tareas pesadas (FX, Compute) -> GPU Discreta (si está fresca)
    // 2. Tareas base (Shell) -> GPU Interna/Integrada (para ahorrar energía)
    
    match task {
        RenderTask::SecondaryEffects | RenderTask::ComputeAI => {
            // Buscar la GPU discreta más potente/fresca
            let mut best_idx = 0;
            let mut min_load = 101;
            
            for (i, gpu) in perf.iter().enumerate() {
                if gpu.is_discrete && gpu.temperature < 800 {
                    if gpu.load < min_load {
                        min_load = gpu.load;
                        best_idx = i;
                    }
                }
            }
            best_idx
        },
        RenderTask::PrimarySurface | RenderTask::BackgroundFlux => {
            // Usar GPU integrada (o la menos cargada si son iguales)
            let mut best_idx = 0;
            let mut min_load = 101;
            
            for (i, gpu) in perf.iter().enumerate() {
                if !gpu.is_discrete {
                     if gpu.load < min_load {
                        min_load = gpu.load;
                        best_idx = i;
                    }
                }
            }
            best_idx
        }
    }
}

/// Actualiza estadísticos de una GPU (llamado por los drivers)
pub fn update_gpu_metrics(idx: usize, load: u32, mem: u64, temp: u32) {
    let mut perf = GPU_PERF.lock();
    if let Some(gpu) = perf.get_mut(idx) {
        gpu.load = load;
        gpu.memory_used = mem;
        gpu.temperature = temp;
    }
}

/// Sugiere una prioridad para el bloque en el bcache (0=Baja, 255=Alta)
pub fn get_bcache_priority(block_num: u64) -> u8 {
    // Heurística simple: 
    // - Bloques < 100 suelen ser superbloques, tablas de inodos, etc. (Meta)
    // - Bloques muy altos suelen ser datos de usuario.
    if block_num < 100 {
        return 200; // Alta prioridad para metadatos
    }
    if block_num < 1000 {
        return 128; // Media para ejecutables/librerías
    }
    50 // Baja para datos generales
}

fn update_process(pid: u32, process: crate::process::Process) {
    // Only update if the process still exists in table
    crate::process::update_process(pid, process);
}

/// Estadísticas completas del sistema para el Dashboard
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemVitals {
    pub cpu_load: [u32; 16],
    pub cpu_temp: [u32; 16],
    pub gpu_load: [u32; 4],
    pub gpu_temp: [u32; 4],
    pub gpu_vram_total_bytes: u64,
    pub gpu_vram_used_bytes: u64,
    pub free_memory_kb: u64,
    pub oom_threat: f32,
    pub anomaly_count: u32,
    pub heap_fragmentation: u32,
}

pub fn get_vitals() -> SystemVitals {
    let load = CPU_LOAD.lock();
    let temp = CPU_TEMP.lock();
    let gpu_perf = GPU_PERF.lock();
    let memory_history = FREE_FRAMES_HISTORY.lock();
    let heap = HEAP_METRICS.lock();

    let mut gpu_l = [0; 4];
    let mut gpu_t = [0; 4];
    for (i, gpu) in gpu_perf.iter().enumerate().take(4) {
        gpu_l[i] = gpu.load;
        gpu_t[i] = gpu.temperature;
    }

    let gpu_vram_total_bytes = *GPU_VRAM_TOTAL_BYTES.lock();
    let gpu_vram_used_bytes = *GPU_VRAM_USED_BYTES.lock();

    SystemVitals {
        cpu_load: *load,
        cpu_temp: *temp,
        gpu_load: gpu_l,
        gpu_temp: gpu_t,
        gpu_vram_total_bytes,
        gpu_vram_used_bytes,
        free_memory_kb: (memory_history[7] * 4096) / 1024,
        oom_threat: predict_oom_threat_internal(&*memory_history),
        anomaly_count: *ANOMALY_COUNT.lock(), 
        heap_fragmentation: heap.fragmentation_p,
    }
}
