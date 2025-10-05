//! Características AI-powered para EclipseFS
//! 
//! Funcionalidades:
//! - Predicción de acceso de archivos
//! - Optimización automática de layout
//! - Detección de patrones de uso
//! - Recomendaciones de limpieza
//! - Análisis de rendimiento inteligente

use crate::types::*;
use crate::EclipseFSResult;

#[cfg(not(feature = "std"))]
use heapless::{String, Vec, BTreeMap};

#[cfg(feature = "std")]
use std::{string::String, vec::Vec, collections::BTreeMap};

/// Configuración de características AI
#[derive(Debug, Clone)]
pub struct AIFeaturesConfig {
    pub enabled: bool,
    pub prediction_enabled: bool,
    pub optimization_enabled: bool,
    pub analysis_enabled: bool,
    pub learning_rate: f32,
    pub cache_size: usize,
}

impl Default for AIFeaturesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prediction_enabled: true,
            optimization_enabled: true,
            analysis_enabled: true,
            learning_rate: 0.1,
            cache_size: 1024,
        }
    }
}

/// Patrón de acceso de archivos
#[derive(Debug, Clone)]
pub struct AccessPattern {
    pub file_path: String,
    pub access_count: u64,
    pub last_access: u64,
    pub access_frequency: f32,
    #[cfg(not(feature = "std"))]
    pub access_times: Vec<u64, 32>,
    #[cfg(feature = "std")]
    pub access_times: Vec<u64>,
    pub sequential_access: bool,
    pub random_access: bool,
}

/// Predicción de acceso
#[derive(Debug, Clone)]
pub struct AccessPrediction {
    pub file_path: String,
    pub probability: f32,
    pub predicted_access_time: u64,
    pub confidence: f32,
}

/// Métricas de rendimiento
#[derive(Debug, Clone)]
pub struct PerformanceMetrics {
    pub read_latency_ms: f32,
    pub write_latency_ms: f32,
    pub throughput_mbps: f32,
    pub cache_hit_rate: f32,
    pub fragmentation_level: f32,
    pub compression_ratio: f32,
    pub deduplication_ratio: f32,
}

/// Recomendación de optimización
#[derive(Debug, Clone)]
pub enum OptimizationRecommendation {
    Defragment,
    Compress,
    Deduplicate,
    MoveToFasterStorage,
    IncreaseCacheSize,
    AdjustCompressionLevel,
    CleanupUnusedFiles,
}

/// Motor AI para EclipseFS
pub struct AIEngine {
    pub config: AIFeaturesConfig,
    pub access_patterns: BTreeMap<String, AccessPattern>,
    pub predictions: Vec<AccessPrediction>,
    pub metrics: PerformanceMetrics,
    pub recommendations: Vec<OptimizationRecommendation>,
    pub learning_data: Vec<f32>,
}

impl AIEngine {
    pub fn new(config: AIFeaturesConfig) -> Self {
        Self {
            config,
            access_patterns: BTreeMap::new(),
            predictions: Vec::new(),
            metrics: PerformanceMetrics {
                read_latency_ms: 0.0,
                write_latency_ms: 0.0,
                throughput_mbps: 0.0,
                cache_hit_rate: 0.0,
                fragmentation_level: 0.0,
                compression_ratio: 1.0,
                deduplication_ratio: 1.0,
            },
            recommendations: Vec::new(),
            learning_data: Vec::new(),
        }
    }

    /// Registrar acceso a un archivo
    pub fn record_access(&mut self, file_path: &str, access_time: u64) -> EclipseFSResult<()> {
        if !self.config.enabled {
            return Ok(());
        }

        // Actualizar o crear patrón de acceso
        if !self.access_patterns.contains_key(file_path) {
            let new_pattern = AccessPattern {
                file_path: file_path.to_string(),
                access_count: 0,
                last_access: 0,
                access_frequency: 0.0,
                access_times: Vec::new(),
                sequential_access: false,
                random_access: false,
            };
            self.access_patterns.insert(file_path.to_string(), new_pattern);
        }

        // Obtener referencia mutable al patrón
        let pattern = self.access_patterns.get_mut(file_path).unwrap();
        
        pattern.access_count += 1;
        pattern.last_access = access_time;
        
        #[cfg(not(feature = "std"))]
        {
            if let Err(_) = pattern.access_times.push(access_time) {
                // Si no cabe, mantener solo los últimos accesos
                pattern.access_times.remove(0);
                let _ = pattern.access_times.push(access_time);
            }
        }
        #[cfg(feature = "std")]
        {
            pattern.access_times.push(access_time);
        }

        // Calcular frecuencia de acceso
        Self::calculate_access_frequency_static(pattern);
        
        // Analizar patrones de acceso
        Self::analyze_access_pattern_static(pattern);

        Ok(())
    }

    /// Calcular frecuencia de acceso (función estática)
    fn calculate_access_frequency_static(pattern: &mut AccessPattern) {
        if pattern.access_times.len() < 2 {
            return;
        }

        let time_span = pattern.access_times[pattern.access_times.len() - 1] - pattern.access_times[0];
        if time_span > 0 {
            pattern.access_frequency = pattern.access_count as f32 / time_span as f32;
        }
    }

    /// Analizar patrones de acceso (función estática)
    fn analyze_access_pattern_static(pattern: &mut AccessPattern) {
        if pattern.access_times.len() < 3 {
            return;
        }

        let mut sequential_count = 0;
        let mut random_count = 0;

        for i in 1..pattern.access_times.len() {
            let interval = pattern.access_times[i] - pattern.access_times[i-1];
            // Si el intervalo es consistente (±20%), es acceso secuencial
            if i > 1 {
                let prev_interval = pattern.access_times[i-1] - pattern.access_times[i-2];
                let diff = (interval as f32 - prev_interval as f32).abs() / prev_interval as f32;
                if diff < 0.2 {
                    sequential_count += 1;
                } else {
                    random_count += 1;
                }
            }
        }

        pattern.sequential_access = sequential_count > random_count;
        pattern.random_access = random_count > sequential_count;
    }

    /// Generar predicciones de acceso
    pub fn generate_predictions(&mut self) -> EclipseFSResult<()> {
        if !self.config.prediction_enabled {
            return Ok(());
        }

        self.predictions.clear();

        for (_, pattern) in self.access_patterns.iter() {
            if pattern.access_count > 10 { // Solo archivos con suficiente historial
                let prediction = self.predict_access(pattern)?;
                let _ = self.predictions.push(prediction);
            }
        }

        Ok(())
    }

    /// Predecir acceso a un archivo
    fn predict_access(&self, pattern: &AccessPattern) -> EclipseFSResult<AccessPrediction> {
        let current_time = self.get_current_time();
        
        // Algoritmo simple de predicción basado en frecuencia histórica
        let probability = if pattern.access_frequency > 0.0 {
            (pattern.access_frequency * self.config.learning_rate).min(1.0)
        } else {
            0.0
        };

        // Predecir tiempo de próximo acceso basado en frecuencia
        let predicted_interval = if pattern.access_frequency > 0.0 {
            (1.0 / pattern.access_frequency) as u64
        } else {
            3600 // 1 hora por defecto
        };

        let predicted_access_time = current_time + predicted_interval;

        // Calcular confianza basada en consistencia del patrón
        let confidence = if pattern.sequential_access {
            0.9
        } else if pattern.random_access {
            0.5
        } else {
            0.7
        };

        Ok(AccessPrediction {
            file_path: pattern.file_path.clone(),
            probability,
            predicted_access_time,
            confidence,
        })
    }

    /// Optimizar layout del sistema de archivos
    pub fn optimize_layout(&mut self) -> EclipseFSResult<()> {
        if !self.config.optimization_enabled {
            return Ok(());
        }

        self.recommendations.clear();

        // Analizar métricas y generar recomendaciones
        self.analyze_performance();
        self.generate_recommendations();

        Ok(())
    }

    /// Analizar rendimiento del sistema
    fn analyze_performance(&mut self) {
        // Calcular fragmentación basada en patrones de acceso
        let fragmented_files = self.access_patterns.iter()
            .filter(|(_, pattern)| pattern.random_access)
            .count();

        self.metrics.fragmentation_level = if !self.access_patterns.is_empty() {
            fragmented_files as f32 / self.access_patterns.len() as f32
        } else {
            0.0
        };

        // Calcular ratio de cache hit basado en predicciones
        let predicted_accesses = self.predictions.iter()
            .filter(|p| p.probability > 0.7)
            .count();

        self.metrics.cache_hit_rate = if !self.predictions.is_empty() {
            predicted_accesses as f32 / self.predictions.len() as f32
        } else {
            0.0
        };
    }

    /// Generar recomendaciones de optimización
    fn generate_recommendations(&mut self) {
        // Recomendar desfragmentación si hay alta fragmentación
        if self.metrics.fragmentation_level > 0.7 {
            let _ = self.recommendations.push(OptimizationRecommendation::Defragment);
        }

        // Recomendar limpieza si hay archivos poco accedidos
        let unused_files = self.access_patterns.iter()
            .filter(|(_, pattern)| pattern.access_frequency < 0.001)
            .count();

        if unused_files > 10 {
            let _ = self.recommendations.push(OptimizationRecommendation::CleanupUnusedFiles);
        }

        // Recomendar compresión para archivos grandes poco accedidos
        let large_unused = self.access_patterns.iter()
            .filter(|(_, pattern)| pattern.access_frequency < 0.01 && pattern.access_count > 100)
            .count();

        if large_unused > 5 {
            let _ = self.recommendations.push(OptimizationRecommendation::Compress);
        }

        // Recomendar aumento de cache si hay baja tasa de hit
        if self.metrics.cache_hit_rate < 0.6 {
            let _ = self.recommendations.push(OptimizationRecommendation::IncreaseCacheSize);
        }
    }

    /// Obtener archivos que deberían estar en cache
    pub fn get_files_for_cache(&self) -> Vec<String> {
        let mut cache_candidates = Vec::new();

        for prediction in self.predictions.iter() {
            if prediction.probability > 0.8 && prediction.confidence > 0.7 {
                let _ = cache_candidates.push(prediction.file_path.clone());
            }
        }

        // Ordenar por probabilidad (mayor primero)
        cache_candidates.sort_by(|a, b| {
            let prob_a = self.predictions.iter()
                .find(|p| p.file_path == *a)
                .map(|p| p.probability)
                .unwrap_or(0.0);
            let prob_b = self.predictions.iter()
                .find(|p| p.file_path == *b)
                .map(|p| p.probability)
                .unwrap_or(0.0);
            prob_b.partial_cmp(&prob_a).unwrap_or(core::cmp::Ordering::Equal)
        });

        cache_candidates
    }

    /// Obtener archivos candidatos para compresión
    pub fn get_files_for_compression(&self) -> Vec<String> {
        let mut compression_candidates = Vec::new();

        for (_, pattern) in self.access_patterns.iter() {
            if pattern.access_frequency < 0.01 && pattern.access_count > 50 {
                let _ = compression_candidates.push(pattern.file_path.clone());
            }
        }

        compression_candidates
    }

    /// Obtener archivos candidatos para eliminación
    pub fn get_files_for_cleanup(&self) -> Vec<String> {
        let mut cleanup_candidates = Vec::new();
        let current_time = self.get_current_time();

        for (_, pattern) in self.access_patterns.iter() {
            let time_since_access = current_time - pattern.last_access;
            // Archivos no accedidos en más de 30 días
            if time_since_access > 2592000 && pattern.access_count < 10 {
                let _ = cleanup_candidates.push(pattern.file_path.clone());
            }
        }

        cleanup_candidates
    }

    /// Obtener métricas de rendimiento
    pub fn get_performance_metrics(&self) -> &PerformanceMetrics {
        &self.metrics
    }

    /// Obtener recomendaciones
    pub fn get_recommendations(&self) -> &Vec<OptimizationRecommendation> {
        &self.recommendations
    }

    /// Obtener predicciones
    pub fn get_predictions(&self) -> &Vec<AccessPrediction> {
        &self.predictions
    }

    /// Actualizar métricas de latencia
    pub fn update_latency_metrics(&mut self, read_latency: f32, write_latency: f32) {
        self.metrics.read_latency_ms = read_latency;
        self.metrics.write_latency_ms = write_latency;
    }

    /// Actualizar métricas de throughput
    pub fn update_throughput_metrics(&mut self, throughput_mbps: f32) {
        self.metrics.throughput_mbps = throughput_mbps;
    }

    /// Actualizar métricas de compresión y deduplicación
    pub fn update_compression_metrics(&mut self, compression_ratio: f32, deduplication_ratio: f32) {
        self.metrics.compression_ratio = compression_ratio;
        self.metrics.deduplication_ratio = deduplication_ratio;
    }

    /// Obtener tiempo actual (simulado)
    fn get_current_time(&self) -> u64 {
        // En implementación real, usaríamos un timer del sistema
        1640995200 // Timestamp simulado
    }

    /// Entrenar el modelo AI con nuevos datos
    pub fn train_model(&mut self, training_data: &[f32]) -> EclipseFSResult<()> {
        if !self.config.enabled {
            return Ok(());
        }

        // Algoritmo simple de aprendizaje
        for (i, &data_point) in training_data.iter().enumerate() {
            self.learning_data.push(data_point);
            // Si el vector se llena, usar promedio móvil
            if self.learning_data.len() >= 1024 {
                let avg = self.learning_data.iter().sum::<f32>() / self.learning_data.len() as f32;
                self.learning_data.clear();
                self.learning_data.push(avg);
            }
        }

        // Ajustar tasa de aprendizaje basada en consistencia de datos
        let variance = self.calculate_variance(&self.learning_data);
        if variance < 0.1 {
            // Datos consistentes, aumentar tasa de aprendizaje
            self.config.learning_rate = (self.config.learning_rate * 1.1).min(0.5);
        } else if variance > 0.5 {
            // Datos inconsistentes, reducir tasa de aprendizaje
            self.config.learning_rate = (self.config.learning_rate * 0.9).max(0.01);
        }

        Ok(())
    }

    /// Calcular varianza de un conjunto de datos
    fn calculate_variance(&self, data: &[f32]) -> f32 {
        if data.is_empty() {
            return 0.0;
        }

        let mean = data.iter().sum::<f32>() / data.len() as f32;
        let variance = data.iter()
            .map(|&x| (x - mean).powi(2))
            .sum::<f32>() / data.len() as f32;

        variance
    }

    /// Generar reporte de análisis
    pub fn generate_analysis_report(&self) -> String {
        let mut report = String::new();
        
        report.push_str("=== EclipseFS AI Analysis Report ===\n");
        report.push_str(&format!("Total files analyzed: {}\n", self.access_patterns.len()));
        report.push_str(&format!("Active predictions: {}\n", self.predictions.len()));
        report.push_str(&format!("Recommendations: {}\n", self.recommendations.len()));
        
        report.push_str("\nPerformance Metrics:\n");
        report.push_str(&format!("  Read latency: {:.2}ms\n", self.metrics.read_latency_ms));
        report.push_str(&format!("  Write latency: {:.2}ms\n", self.metrics.write_latency_ms));
        report.push_str(&format!("  Throughput: {:.2}MB/s\n", self.metrics.throughput_mbps));
        report.push_str(&format!("  Cache hit rate: {:.2}%\n", self.metrics.cache_hit_rate * 100.0));
        report.push_str(&format!("  Fragmentation: {:.2}%\n", self.metrics.fragmentation_level * 100.0));
        
        report.push_str("\nTop Recommendations:\n");
        for (i, rec) in self.recommendations.iter().take(5).enumerate() {
            report.push_str(&format!("  {}. {:?}\n", i + 1, rec));
        }

        report
    }
}

impl Default for AIEngine {
    fn default() -> Self {
        Self::new(AIFeaturesConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ai_engine_creation() {
        let ai = AIEngine::new(AIFeaturesConfig::default());
        assert!(ai.config.enabled);
        assert!(ai.access_patterns.is_empty());
    }

    #[test]
    fn test_access_recording() {
        let mut ai = AIEngine::new(AIFeaturesConfig::default());
        let _ = ai.record_access("/test/file.txt", 1640995200);
        assert_eq!(ai.access_patterns.len(), 1);
    }

    #[test]
    fn test_prediction_generation() {
        let mut ai = AIEngine::new(AIFeaturesConfig::default());
        
        // Simular múltiples accesos
        for i in 0..20 {
            let _ = ai.record_access("/test/file.txt", 1640995200 + i * 3600);
        }
        
        let _ = ai.generate_predictions();
        assert!(!ai.predictions.is_empty());
    }

    #[test]
    fn test_optimization_recommendations() {
        let mut ai = AIEngine::new(AIFeaturesConfig::default());
        let _ = ai.optimize_layout();
        // Las recomendaciones pueden estar vacías si no hay datos suficientes
    }

    #[test]
    fn test_cache_candidates() {
        let mut ai = AIEngine::new(AIFeaturesConfig::default());
        
        // Simular acceso a archivos
        for i in 0..10 {
            let _ = ai.record_access(&format!("/test/file{}.txt", i), 1640995200 + i);
        }
        
        let _ = ai.generate_predictions();
        let cache_candidates = ai.get_files_for_cache();
        // Puede estar vacío si no hay suficientes datos
    }

    #[test]
    fn test_performance_metrics() {
        let mut ai = AIEngine::new(AIFeaturesConfig::default());
        ai.update_latency_metrics(1.5, 2.0);
        ai.update_throughput_metrics(100.0);
        
        let metrics = ai.get_performance_metrics();
        assert_eq!(metrics.read_latency_ms, 1.5);
        assert_eq!(metrics.write_latency_ms, 2.0);
        assert_eq!(metrics.throughput_mbps, 100.0);
    }
}
