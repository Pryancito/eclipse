//! Scheduler Adaptativo para Eclipse OS
//!
//! Este módulo implementa un scheduler que se adapta automáticamente
//! a las condiciones del sistema para optimizar el rendimiento

use crate::math_utils::{max_f64, sqrt};
use crate::process::{ProcessId, ProcessPriority, ThreadId};
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Algoritmo de scheduling actual
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CurrentAlgorithm {
    RoundRobin,
    Priority,
    FCFS,
    SJF,
    MLFQ,
    Adaptive,
}

/// Métricas de rendimiento del scheduler
#[derive(Debug, Clone)]
pub struct SchedulerMetrics {
    pub throughput: f64,              // tareas completadas por segundo
    pub latency: f64,                 // latencia promedio
    pub fairness: f64,                // score de equidad (0-100)
    pub cpu_utilization: f64,         // utilización de CPU
    pub context_switch_overhead: f64, // overhead de context switching
    pub algorithm_effectiveness: f64, // efectividad del algoritmo actual
}

/// Configuración del scheduler adaptativo
#[derive(Debug, Clone)]
pub struct AdaptiveSchedulerConfig {
    pub adaptation_interval: u64, // intervalo de adaptación en nanosegundos
    pub performance_window: u64,  // ventana de medición de rendimiento
    pub threshold_improvement: f64, // umbral de mejora para cambiar algoritmo
    pub enable_learning: bool,    // habilitar aprendizaje automático
    pub enable_prediction: bool,  // habilitar predicción de carga
    pub enable_workload_analysis: bool, // habilitar análisis de carga de trabajo
}

impl Default for AdaptiveSchedulerConfig {
    fn default() -> Self {
        Self {
            adaptation_interval: 1000000000, // 1 segundo
            performance_window: 5000000000,  // 5 segundos
            threshold_improvement: 0.1,      // 10% de mejora
            enable_learning: true,
            enable_prediction: true,
            enable_workload_analysis: true,
        }
    }
}

/// Información de un proceso para el scheduler
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub process_id: ProcessId,
    pub thread_id: ThreadId,
    pub priority: ProcessPriority,
    pub cpu_time: u64,
    pub memory_usage: u64,
    pub io_wait_time: u64,
    pub context_switches: u64,
    pub last_run_time: u64,
    pub estimated_remaining_time: u64,
    pub workload_type: WorkloadType,
}

/// Tipo de carga de trabajo
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkloadType {
    CpuIntensive,
    IoIntensive,
    MemoryIntensive,
    Mixed,
    Interactive,
    Batch,
}

/// Scheduler adaptativo
pub struct AdaptiveScheduler {
    config: AdaptiveSchedulerConfig,
    current_algorithm: CurrentAlgorithm,
    metrics: SchedulerMetrics,
    process_info: BTreeMap<ProcessId, ProcessInfo>,
    algorithm_performance: BTreeMap<CurrentAlgorithm, f64>,
    last_adaptation_time: AtomicU64,
    adaptation_count: AtomicUsize,
    learning_data: Vec<LearningSample>,
    prediction_model: Option<PredictionModel>,
}

/// Muestra de aprendizaje
#[derive(Debug, Clone)]
pub struct LearningSample {
    pub workload_characteristics: WorkloadCharacteristics,
    pub algorithm_used: CurrentAlgorithm,
    pub performance_achieved: f64,
    pub timestamp: u64,
}

/// Características de la carga de trabajo
#[derive(Debug, Clone)]
pub struct WorkloadCharacteristics {
    pub cpu_intensity: f64,
    pub io_intensity: f64,
    pub memory_intensity: f64,
    pub process_count: usize,
    pub average_priority: u8,
    pub variance_priority: f64,
}

/// Modelo de predicción
#[derive(Debug, Clone)]
pub struct PredictionModel {
    pub weights: Vec<f64>,
    pub bias: f64,
    pub accuracy: f64,
}

impl AdaptiveScheduler {
    /// Crear un nuevo scheduler adaptativo
    pub fn new() -> Self {
        Self {
            config: AdaptiveSchedulerConfig::default(),
            current_algorithm: CurrentAlgorithm::Adaptive,
            metrics: SchedulerMetrics {
                throughput: 0.0,
                latency: 0.0,
                fairness: 0.0,
                cpu_utilization: 0.0,
                context_switch_overhead: 0.0,
                algorithm_effectiveness: 0.0,
            },
            process_info: BTreeMap::new(),
            algorithm_performance: BTreeMap::new(),
            last_adaptation_time: AtomicU64::new(0),
            adaptation_count: AtomicUsize::new(0),
            learning_data: Vec::new(),
            prediction_model: None,
        }
    }

    /// Inicializar el scheduler
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Limpiar datos
        self.process_info.clear();
        self.algorithm_performance.clear();
        self.learning_data.clear();

        // Inicializar métricas
        self.metrics = SchedulerMetrics {
            throughput: 0.0,
            latency: 0.0,
            fairness: 0.0,
            cpu_utilization: 0.0,
            context_switch_overhead: 0.0,
            algorithm_effectiveness: 0.0,
        };

        // Inicializar algoritmo por defecto
        self.current_algorithm = CurrentAlgorithm::RoundRobin;

        Ok(())
    }

    /// Adaptar el scheduling
    pub fn adapt_scheduling(&mut self) -> Result<(), &'static str> {
        let current_time = self.get_current_time();
        let last_adaptation = self.last_adaptation_time.load(Ordering::Acquire);

        // Verificar si es tiempo de adaptar
        if current_time - last_adaptation < self.config.adaptation_interval {
            return Ok(());
        }

        // Actualizar timestamp
        self.last_adaptation_time
            .store(current_time, Ordering::Release);

        // Analizar carga de trabajo actual
        let workload_characteristics = self.analyze_workload();

        // Predecir mejor algoritmo si está habilitado
        let predicted_algorithm = if self.config.enable_prediction {
            self.predict_best_algorithm(&workload_characteristics)
        } else {
            self.current_algorithm
        };

        // Evaluar rendimiento del algoritmo actual
        let current_performance = self.evaluate_current_performance();

        // Decidir si cambiar algoritmo
        if self.should_change_algorithm(predicted_algorithm, current_performance) {
            self.change_algorithm(predicted_algorithm);
        }

        // Actualizar métricas
        self.update_metrics();

        // Aprender del resultado si está habilitado
        if self.config.enable_learning {
            self.learn_from_performance(current_performance);
        }

        self.adaptation_count.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Analizar carga de trabajo actual
    fn analyze_workload(&self) -> WorkloadCharacteristics {
        if self.process_info.is_empty() {
            return WorkloadCharacteristics {
                cpu_intensity: 0.0,
                io_intensity: 0.0,
                memory_intensity: 0.0,
                process_count: 0,
                average_priority: 5,
                variance_priority: 0.0,
            };
        }

        let process_count = self.process_info.len();
        let total_cpu_time: u64 = self.process_info.values().map(|p| p.cpu_time).sum();
        let total_io_time: u64 = self.process_info.values().map(|p| p.io_wait_time).sum();
        let total_memory: u64 = self.process_info.values().map(|p| p.memory_usage).sum();

        let cpu_intensity = if total_cpu_time > 0 {
            total_cpu_time as f64 / (total_cpu_time + total_io_time) as f64
        } else {
            0.0
        };

        let io_intensity = if total_io_time > 0 {
            total_io_time as f64 / (total_cpu_time + total_io_time) as f64
        } else {
            0.0
        };

        let memory_intensity = if total_memory > 0 {
            (total_memory as f64 / (1024.0 * 1024.0)) / process_count as f64 // MB por proceso
        } else {
            0.0
        };

        let priorities: Vec<u8> = self
            .process_info
            .values()
            .map(|p| p.priority as u8)
            .collect();
        let average_priority = if !priorities.is_empty() {
            priorities.iter().sum::<u8>() as f64 / priorities.len() as f64
        } else {
            5.0
        };

        let variance_priority = if priorities.len() > 1 {
            let mean = average_priority;
            let variance = priorities
                .iter()
                .map(|&p| (p as f64 - mean) * (p as f64 - mean))
                .sum::<f64>()
                / priorities.len() as f64;
            sqrt(variance)
        } else {
            0.0
        };

        WorkloadCharacteristics {
            cpu_intensity,
            io_intensity,
            memory_intensity,
            process_count,
            average_priority: average_priority as u8,
            variance_priority,
        }
    }

    /// Predecir mejor algoritmo
    fn predict_best_algorithm(&self, workload: &WorkloadCharacteristics) -> CurrentAlgorithm {
        if let Some(model) = &self.prediction_model {
            // Usar modelo de predicción
            let score = self.predict_with_model(model, workload);
            self.algorithm_with_highest_score(score)
        } else {
            // Usar heurísticas simples
            self.heuristic_algorithm_selection(workload)
        }
    }

    /// Predecir con modelo
    fn predict_with_model(
        &self,
        model: &PredictionModel,
        workload: &WorkloadCharacteristics,
    ) -> BTreeMap<CurrentAlgorithm, f64> {
        let mut scores = BTreeMap::new();

        // Simulación de predicción con modelo
        // En un sistema real, esto usaría un modelo de ML entrenado
        for algorithm in &[
            CurrentAlgorithm::RoundRobin,
            CurrentAlgorithm::Priority,
            CurrentAlgorithm::FCFS,
            CurrentAlgorithm::SJF,
            CurrentAlgorithm::MLFQ,
        ] {
            let score = self.calculate_algorithm_score(*algorithm, workload);
            scores.insert(*algorithm, score);
        }

        scores
    }

    /// Calcular score de algoritmo
    fn calculate_algorithm_score(
        &self,
        algorithm: CurrentAlgorithm,
        workload: &WorkloadCharacteristics,
    ) -> f64 {
        match algorithm {
            CurrentAlgorithm::RoundRobin => {
                // Round Robin es bueno para cargas equilibradas
                if workload.variance_priority < 1.0 {
                    0.8
                } else {
                    0.6
                }
            }
            CurrentAlgorithm::Priority => {
                // Priority es bueno cuando hay mucha variación en prioridades
                if workload.variance_priority > 2.0 {
                    0.9
                } else {
                    0.5
                }
            }
            CurrentAlgorithm::FCFS => {
                // FCFS es bueno para cargas ligeras
                if workload.process_count < 10 {
                    0.7
                } else {
                    0.4
                }
            }
            CurrentAlgorithm::SJF => {
                // SJF es bueno para cargas con tiempos de ejecución conocidos
                if workload.cpu_intensity > 0.8 {
                    0.8
                } else {
                    0.5
                }
            }
            CurrentAlgorithm::MLFQ => {
                // MLFQ es bueno para cargas mixtas
                if workload.cpu_intensity > 0.3 && workload.io_intensity > 0.3 {
                    0.9
                } else {
                    0.6
                }
            }
            CurrentAlgorithm::Adaptive => 0.5, // Placeholder
        }
    }

    /// Selección heurística de algoritmo
    fn heuristic_algorithm_selection(
        &self,
        workload: &WorkloadCharacteristics,
    ) -> CurrentAlgorithm {
        if workload.process_count < 5 {
            CurrentAlgorithm::FCFS
        } else if workload.variance_priority > 2.0 {
            CurrentAlgorithm::Priority
        } else if workload.cpu_intensity > 0.8 {
            CurrentAlgorithm::SJF
        } else if workload.io_intensity > 0.5 {
            CurrentAlgorithm::MLFQ
        } else {
            CurrentAlgorithm::RoundRobin
        }
    }

    /// Algoritmo con mayor score
    fn algorithm_with_highest_score(
        &self,
        scores: BTreeMap<CurrentAlgorithm, f64>,
    ) -> CurrentAlgorithm {
        scores
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(algorithm, _)| *algorithm)
            .unwrap_or(CurrentAlgorithm::RoundRobin)
    }

    /// Evaluar rendimiento actual
    fn evaluate_current_performance(&self) -> f64 {
        // Combinar métricas en un score de rendimiento
        let throughput_score = (self.metrics.throughput / 100.0).min(1.0);
        let latency_score = (1.0 - self.metrics.latency / 1000.0).max(0.0);
        let fairness_score = self.metrics.fairness / 100.0;
        let utilization_score = self.metrics.cpu_utilization / 100.0;

        (throughput_score + latency_score + fairness_score + utilization_score) / 4.0
    }

    /// Decidir si cambiar algoritmo
    fn should_change_algorithm(
        &self,
        predicted_algorithm: CurrentAlgorithm,
        current_performance: f64,
    ) -> bool {
        if predicted_algorithm == self.current_algorithm {
            return false;
        }

        // Obtener rendimiento histórico del algoritmo predicho
        let predicted_performance = self
            .algorithm_performance
            .get(&predicted_algorithm)
            .copied()
            .unwrap_or(0.5);

        // Cambiar si el algoritmo predicho es significativamente mejor
        predicted_performance > current_performance + self.config.threshold_improvement
    }

    /// Cambiar algoritmo
    fn change_algorithm(&mut self, new_algorithm: CurrentAlgorithm) {
        self.current_algorithm = new_algorithm;

        // Registrar el cambio
        self.record_algorithm_change(new_algorithm);
    }

    /// Registrar cambio de algoritmo
    fn record_algorithm_change(&mut self, algorithm: CurrentAlgorithm) {
        // En un sistema real, esto notificaría al scheduler principal
        // Por ahora, solo actualizamos el estado interno
    }

    /// Actualizar métricas
    fn update_metrics(&mut self) {
        // Simular actualización de métricas
        // En un sistema real, esto calcularía métricas reales

        self.metrics.throughput = self.calculate_throughput();
        self.metrics.latency = self.calculate_latency();
        self.metrics.fairness = self.calculate_fairness();
        self.metrics.cpu_utilization = self.calculate_cpu_utilization();
        self.metrics.context_switch_overhead = self.calculate_context_switch_overhead();
        self.metrics.algorithm_effectiveness = self.evaluate_current_performance();
    }

    /// Calcular throughput
    fn calculate_throughput(&self) -> f64 {
        // Simulación de cálculo de throughput
        let process_count = self.process_info.len();
        if process_count > 0 {
            (process_count as f64 * 10.0).min(100.0)
        } else {
            0.0
        }
    }

    /// Calcular latencia
    fn calculate_latency(&self) -> f64 {
        // Simulación de cálculo de latencia
        let average_cpu_time = if !self.process_info.is_empty() {
            let total_cpu: u64 = self.process_info.values().map(|p| p.cpu_time).sum();
            total_cpu / self.process_info.len() as u64
        } else {
            0
        };

        average_cpu_time as f64 / 1000.0 // Convertir a ms
    }

    /// Calcular equidad
    fn calculate_fairness(&self) -> f64 {
        // Simulación de cálculo de equidad
        if self.process_info.is_empty() {
            return 100.0;
        }

        let cpu_times: Vec<u64> = self.process_info.values().map(|p| p.cpu_time).collect();
        let mean = cpu_times.iter().sum::<u64>() as f64 / cpu_times.len() as f64;

        let variance = cpu_times
            .iter()
            .map(|&time| (time as f64 - mean) * (time as f64 - mean))
            .sum::<f64>()
            / cpu_times.len() as f64;

        let std_dev = sqrt(variance);
        let coefficient_of_variation = if mean > 0.0 { std_dev / mean } else { 0.0 };

        // Convertir a score de equidad (menor variación = mayor equidad)
        max_f64((1.0 - coefficient_of_variation) * 100.0, 0.0)
    }

    /// Calcular utilización de CPU
    fn calculate_cpu_utilization(&self) -> f64 {
        // Simulación de utilización de CPU
        let total_cpu_time: u64 = self.process_info.values().map(|p| p.cpu_time).sum();
        let time_window = 1000000000; // 1 segundo en nanosegundos

        (total_cpu_time as f64 / time_window as f64 * 100.0).min(100.0)
    }

    /// Calcular overhead de context switching
    fn calculate_context_switch_overhead(&self) -> f64 {
        // Simulación de overhead de context switching
        let total_switches: u64 = self.process_info.values().map(|p| p.context_switches).sum();
        let switch_overhead = 1000; // 1 microsegundo por switch

        total_switches as f64 * switch_overhead as f64 / 1000.0 // Convertir a ms
    }

    /// Aprender del rendimiento
    fn learn_from_performance(&mut self, performance: f64) {
        let current_time = self.get_current_time();
        let workload = self.analyze_workload();

        let sample = LearningSample {
            workload_characteristics: workload,
            algorithm_used: self.current_algorithm,
            performance_achieved: performance,
            timestamp: current_time,
        };

        self.learning_data.push(sample);

        // Mantener solo los últimos 1000 samples
        if self.learning_data.len() > 1000 {
            self.learning_data.remove(0);
        }

        // Actualizar rendimiento del algoritmo
        self.algorithm_performance
            .insert(self.current_algorithm, performance);

        // Entrenar modelo si hay suficientes datos
        if self.learning_data.len() >= 100 {
            self.train_prediction_model();
        }
    }

    /// Entrenar modelo de predicción
    fn train_prediction_model(&mut self) {
        // Simulación de entrenamiento de modelo
        // En un sistema real, esto usaría algoritmos de ML como regresión lineal o redes neuronales

        let model = PredictionModel {
            weights: vec![0.3, 0.2, 0.2, 0.1, 0.1, 0.1], // Pesos para diferentes características
            bias: 0.1,
            accuracy: 0.85, // 85% de precisión simulada
        };

        self.prediction_model = Some(model);
    }

    /// Obtener score actual
    pub fn get_score(&self) -> f64 {
        self.metrics.algorithm_effectiveness
    }

    /// Obtener algoritmo actual
    pub fn get_current_algorithm(&self) -> CurrentAlgorithm {
        self.current_algorithm
    }

    /// Obtener métricas
    pub fn get_metrics(&self) -> &SchedulerMetrics {
        &self.metrics
    }

    /// Actualizar información de proceso
    pub fn update_process_info(&mut self, process_info: ProcessInfo) {
        self.process_info
            .insert(process_info.process_id, process_info);
    }

    /// Obtener tiempo actual (simulado)
    fn get_current_time(&self) -> u64 {
        static mut COUNTER: u64 = 0;
        unsafe {
            COUNTER += 1;
            COUNTER
        }
    }

    /// Obtener estadísticas detalladas
    pub fn get_detailed_stats(&self) -> AdaptiveSchedulerStats {
        AdaptiveSchedulerStats {
            current_algorithm: self.current_algorithm,
            throughput: self.metrics.throughput,
            latency: self.metrics.latency,
            fairness: self.metrics.fairness,
            cpu_utilization: self.metrics.cpu_utilization,
            context_switch_overhead: self.metrics.context_switch_overhead,
            algorithm_effectiveness: self.metrics.algorithm_effectiveness,
            adaptation_count: self.adaptation_count.load(Ordering::Relaxed),
            process_count: self.process_info.len(),
            learning_samples: self.learning_data.len(),
            prediction_model_available: self.prediction_model.is_some(),
        }
    }
}

/// Estadísticas detalladas del scheduler adaptativo
#[derive(Debug, Clone)]
pub struct AdaptiveSchedulerStats {
    pub current_algorithm: CurrentAlgorithm,
    pub throughput: f64,
    pub latency: f64,
    pub fairness: f64,
    pub cpu_utilization: f64,
    pub context_switch_overhead: f64,
    pub algorithm_effectiveness: f64,
    pub adaptation_count: usize,
    pub process_count: usize,
    pub learning_samples: usize,
    pub prediction_model_available: bool,
}
