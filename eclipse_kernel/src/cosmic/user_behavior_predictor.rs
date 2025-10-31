//! Sistema de Predicción de Comportamiento del Usuario
//!
//! Este módulo utiliza los 6 modelos de IA existentes para predecir
//! el comportamiento del usuario, incluyendo aplicaciones que va a abrir,
//! acciones que va a realizar, y sugerencias proactivas.

#![no_std]

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use crate::ai::model_loader::{ModelLoader, ModelType};

/// Sistema de Predicción de Comportamiento del Usuario
pub struct UserBehaviorPredictor {
    /// Configuración del sistema
    config: UserBehaviorConfig,
    /// Estadísticas del sistema
    stats: UserBehaviorStats,
    /// Estado del sistema
    enabled: bool,
    /// Cargador de modelos de IA
    model_loader: ModelLoader,
    /// Patrones de comportamiento del usuario
    user_patterns: UserBehaviorPatterns,
    /// Historial de acciones
    action_history: Vec<UserAction>,
    /// Predicciones actuales
    current_predictions: BTreeMap<String, BehaviorPrediction>,
    /// Contexto del usuario
    user_context: UserContext,
    /// Sugerencias generadas
    generated_suggestions: Vec<BehaviorSuggestion>,
    /// Modelos de comportamiento
    behavior_models: BehaviorModels,
}

/// Configuración del sistema de predicción de comportamiento
#[derive(Debug, Clone)]
pub struct UserBehaviorConfig {
    /// Intervalo de análisis en frames
    pub analysis_interval: u32,
    /// Habilitar predicción de aplicaciones
    pub enable_app_prediction: bool,
    /// Habilitar predicción de acciones
    pub enable_action_prediction: bool,
    /// Habilitar sugerencias proactivas
    pub enable_proactive_suggestions: bool,
    /// Habilitar pre-carga de recursos
    pub enable_resource_preloading: bool,
    /// Habilitar análisis de contexto
    pub enable_context_analysis: bool,
    /// Habilitar aprendizaje de patrones
    pub enable_pattern_learning: bool,
    /// Sensibilidad de predicción
    pub prediction_sensitivity: f32,
    /// Tiempo máximo de análisis por frame
    pub max_analysis_time_ms: u32,
}

/// Estadísticas del sistema de predicción de comportamiento
#[derive(Debug, Default)]
pub struct UserBehaviorStats {
    /// Total de predicciones realizadas
    pub total_predictions: u32,
    /// Total de predicciones correctas
    pub total_correct_predictions: u32,
    /// Total de sugerencias generadas
    pub total_suggestions: u32,
    /// Total de sugerencias aceptadas
    pub total_accepted_suggestions: u32,
    /// Precisión promedio de predicciones
    pub average_prediction_accuracy: f32,
    /// Tasa de aceptación de sugerencias
    pub suggestion_acceptance_rate: f32,
    /// Tiempo promedio de análisis
    pub average_analysis_time: f32,
    /// Patrones aprendidos
    pub patterns_learned: u32,
    /// Última actualización
    pub last_update_frame: u32,
}

/// Patrones de comportamiento del usuario
#[derive(Debug, Default)]
pub struct UserBehaviorPatterns {
    /// Patrones por hora del día
    pub hourly_patterns: BTreeMap<u8, HourlyPattern>,
    /// Patrones por día de la semana
    pub daily_patterns: BTreeMap<u8, DailyPattern>,
    /// Patrones por aplicación
    pub application_patterns: BTreeMap<String, ApplicationPattern>,
    /// Patrones de secuencia de acciones
    pub action_sequences: Vec<ActionSequence>,
    /// Patrones de contexto
    pub context_patterns: BTreeMap<String, ContextPattern>,
    /// Patrones de tiempo de uso
    pub usage_time_patterns: BTreeMap<String, UsageTimePattern>,
}

/// Patrón por hora
#[derive(Debug, Default)]
pub struct HourlyPattern {
    /// Hora del día
    pub hour: u8,
    /// Aplicaciones más usadas
    pub frequent_applications: Vec<String>,
    /// Acciones más comunes
    pub common_actions: Vec<UserActionType>,
    /// Nivel de actividad
    pub activity_level: f32,
    /// Contexto típico
    pub typical_context: String,
}

/// Patrón por día
#[derive(Debug, Default)]
pub struct DailyPattern {
    /// Día de la semana (0-6)
    pub day_of_week: u8,
    /// Rutinas típicas
    pub typical_routines: Vec<String>,
    /// Aplicaciones del día
    pub daily_applications: Vec<String>,
    /// Horas de mayor actividad
    pub peak_hours: Vec<u8>,
    /// Contexto del día
    pub day_context: String,
}

/// Patrón de aplicación
#[derive(Debug, Default)]
pub struct ApplicationPattern {
    /// Nombre de la aplicación
    pub application_name: String,
    /// Frecuencia de uso
    pub usage_frequency: f32,
    /// Duración promedio de uso
    pub average_duration: f32,
    /// Horas de uso típicas
    pub typical_usage_hours: Vec<u8>,
    /// Contextos de uso
    pub usage_contexts: Vec<String>,
    /// Acciones típicas en la aplicación
    pub typical_actions: Vec<UserActionType>,
}

/// Secuencia de acciones
#[derive(Debug, Clone)]
pub struct ActionSequence {
    /// ID de la secuencia
    pub id: String,
    /// Secuencia de acciones
    pub actions: Vec<UserActionType>,
    /// Frecuencia de la secuencia
    pub frequency: f32,
    /// Contexto de la secuencia
    pub context: String,
    /// Probabilidad de ocurrencia
    pub probability: f32,
}

/// Patrón de contexto
#[derive(Debug, Default)]
pub struct ContextPattern {
    /// Tipo de contexto
    pub context_type: String,
    /// Aplicaciones asociadas
    pub associated_applications: Vec<String>,
    /// Acciones típicas
    pub typical_actions: Vec<UserActionType>,
    /// Duración típica
    pub typical_duration: f32,
    /// Transiciones comunes
    pub common_transitions: Vec<String>,
}

/// Patrón de tiempo de uso
#[derive(Debug, Default)]
pub struct UsageTimePattern {
    /// Identificador del patrón
    pub pattern_id: String,
    /// Duración típica de sesiones
    pub typical_session_duration: f32,
    /// Intervalos entre sesiones
    pub session_intervals: Vec<f32>,
    /// Horas de inicio típicas
    pub typical_start_times: Vec<u8>,
    /// Patrón de intensidad
    pub intensity_pattern: String,
}

/// Acción del usuario
#[derive(Debug, Clone)]
pub struct UserAction {
    /// ID de la acción
    pub id: String,
    /// Tipo de acción
    pub action_type: UserActionType,
    /// Aplicación relacionada
    pub application: Option<String>,
    /// Timestamp de la acción
    pub timestamp: u32,
    /// Contexto de la acción
    pub context: String,
    /// Duración de la acción
    pub duration: f32,
    /// Resultado de la acción
    pub result: ActionResult,
}

/// Tipos de acción del usuario
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UserActionType {
    OpenApplication,
    CloseApplication,
    SwitchApplication,
    CreateFile,
    EditFile,
    SaveFile,
    DeleteFile,
    NavigateFolder,
    SearchContent,
    CopyContent,
    PasteContent,
    UndoAction,
    RedoAction,
    MinimizeWindow,
    MaximizeWindow,
    ResizeWindow,
    MoveWindow,
    ChangeTheme,
    OpenSettings,
    InstallApplication,
    UpdateApplication,
    NetworkAccess,
    PrintDocument,
    ShareFile,
    BackupData,
    SystemRestart,
    SystemShutdown,
    LockScreen,
    UnlockScreen,
}

/// Resultado de acción
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    Success,
    Failure,
    Cancelled,
    Timeout,
    Unknown,
}

/// Predicción de comportamiento
#[derive(Debug, Clone)]
pub struct BehaviorPrediction {
    /// ID de la predicción
    pub id: String,
    /// Tipo de predicción
    pub prediction_type: PredictionType,
    /// Acción predicha
    pub predicted_action: UserActionType,
    /// Aplicación predicha
    pub predicted_application: Option<String>,
    /// Confianza de la predicción
    pub confidence: f32,
    /// Modelo usado
    pub model_used: ModelType,
    /// Timestamp de la predicción
    pub timestamp: u32,
    /// Contexto de la predicción
    pub context: String,
}

/// Tipos de predicción
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PredictionType {
    NextApplication,
    NextAction,
    SessionDuration,
    WorkPattern,
    BreakTime,
    ResourceNeed,
    ContextSwitch,
    ErrorPrediction,
}

/// Contexto del usuario
#[derive(Debug, Default, Clone)]
pub struct UserContext {
    /// Hora actual
    pub current_hour: u8,
    /// Día de la semana
    pub day_of_week: u8,
    /// Contexto de trabajo
    pub work_context: String,
    /// Nivel de fatiga
    pub fatigue_level: f32,
    /// Nivel de concentración
    pub concentration_level: f32,
    /// Aplicaciones activas
    pub active_applications: Vec<String>,
    /// Última acción
    pub last_action: Option<UserActionType>,
    /// Estado del sistema
    pub system_state: String,
    /// Disponibilidad de recursos
    pub resource_availability: f32,
}

/// Sugerencia de comportamiento
#[derive(Debug, Clone)]
pub struct BehaviorSuggestion {
    /// ID de la sugerencia
    pub id: String,
    /// Tipo de sugerencia
    pub suggestion_type: SuggestionType,
    /// Acción sugerida
    pub suggested_action: UserActionType,
    /// Aplicación sugerida
    pub suggested_application: Option<String>,
    /// Razón de la sugerencia
    pub reason: String,
    /// Prioridad de la sugerencia
    pub priority: u8,
    /// Timestamp de la sugerencia
    pub timestamp: u32,
    /// Estado de la sugerencia
    pub status: SuggestionStatus,
}

/// Tipos de sugerencia
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SuggestionType {
    ApplicationRecommendation,
    ActionRecommendation,
    WorkflowOptimization,
    ResourceOptimization,
    TimeManagement,
    ProductivityTip,
    SystemOptimization,
    SecurityRecommendation,
}

/// Estado de sugerencia
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionStatus {
    Pending,
    Accepted,
    Rejected,
    Implemented,
    Expired,
}

/// Modelos de comportamiento
#[derive(Debug)]
pub struct BehaviorModels {
    /// Modelo de predicción de aplicaciones
    pub application_prediction_model: ModelType,
    /// Modelo de predicción de acciones
    pub action_prediction_model: ModelType,
    /// Modelo de análisis de contexto
    pub context_analysis_model: ModelType,
    /// Modelo de optimización de recursos
    pub resource_optimization_model: ModelType,
    /// Modelo de detección de patrones
    pub pattern_detection_model: ModelType,
}

impl UserBehaviorPredictor {
    /// Crear nuevo predictor de comportamiento
    pub fn new() -> Self {
        Self {
            config: UserBehaviorConfig::default(),
            stats: UserBehaviorStats::default(),
            enabled: true,
            model_loader: ModelLoader::new(),
            user_patterns: UserBehaviorPatterns::default(),
            action_history: Vec::new(),
            current_predictions: BTreeMap::new(),
            user_context: UserContext::default(),
            generated_suggestions: Vec::new(),
            behavior_models: BehaviorModels::default(),
        }
    }

    /// Crear predictor con ModelLoader existente
    pub fn with_model_loader(model_loader: ModelLoader) -> Self {
        Self {
            config: UserBehaviorConfig::default(),
            stats: UserBehaviorStats::default(),
            enabled: true,
            model_loader,
            user_patterns: UserBehaviorPatterns::default(),
            action_history: Vec::new(),
            current_predictions: BTreeMap::new(),
            user_context: UserContext::default(),
            generated_suggestions: Vec::new(),
            behavior_models: BehaviorModels::default(),
        }
    }

    /// Inicializar el sistema
    pub fn initialize(&mut self) -> Result<(), String> {
        self.stats.last_update_frame = 0;

        // Cargar modelos de IA
        match self.model_loader.load_all_models() {
            Ok(_) => {
                let loaded_count = self
                    .model_loader
                    .list_models()
                    .iter()
                    .filter(|m| m.loaded)
                    .count();
                if loaded_count > 0 {
                    // Inicializar contexto
                    self.user_context.initialize();
                    // Configurar modelos de comportamiento
                    self.setup_behavior_models();
                    // Crear patrones por defecto
                    self.create_default_patterns();
                    Ok(())
                } else {
                    Err(
                        "No se pudieron cargar modelos de IA para predicción de comportamiento"
                            .to_string(),
                    )
                }
            }
            Err(e) => Err(format!("Error cargando modelos de IA: {:?}", e)),
        }
    }

    /// Actualizar el sistema
    pub fn update(&mut self, frame: u32) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        self.stats.last_update_frame = frame;

        // Actualizar contexto del usuario
        self.update_user_context(frame);

        // Analizar comportamiento si está habilitado
        if frame % self.config.analysis_interval == 0 {
            self.analyze_user_behavior(frame)?;
        }

        // Predecir aplicaciones si está habilitado
        if self.config.enable_app_prediction && frame % 180 == 0 {
            // Cada 3 segundos
            self.predict_next_applications(frame)?;
        }

        // Predecir acciones si está habilitado
        if self.config.enable_action_prediction && frame % 120 == 0 {
            // Cada 2 segundos
            self.predict_next_actions(frame)?;
        }

        // Generar sugerencias si está habilitado
        if self.config.enable_proactive_suggestions && frame % 240 == 0 {
            // Cada 4 segundos
            self.generate_proactive_suggestions(frame)?;
        }

        // Aprender patrones si está habilitado
        if self.config.enable_pattern_learning && frame % 300 == 0 {
            // Cada 5 segundos
            self.learn_user_patterns(frame)?;
        }

        // Analizar contexto si está habilitado
        if self.config.enable_context_analysis && frame % 150 == 0 {
            // Cada 2.5 segundos
            self.analyze_user_context(frame)?;
        }

        Ok(())
    }

    /// Registrar acción del usuario
    pub fn register_user_action(&mut self, action: UserAction) -> Result<(), String> {
        self.action_history.push(action.clone());

        // Mantener solo los últimos 1000 registros
        if self.action_history.len() > 1000 {
            self.action_history.remove(0);
        }

        // Actualizar patrones inmediatamente
        self.update_patterns_from_action(&action);

        Ok(())
    }

    /// Obtener estadísticas del sistema
    pub fn get_stats(&self) -> &UserBehaviorStats {
        &self.stats
    }

    /// Configurar el sistema
    pub fn configure(&mut self, config: UserBehaviorConfig) {
        self.config = config;
    }

    /// Obtener predicciones actuales
    pub fn get_current_predictions(&self) -> &BTreeMap<String, BehaviorPrediction> {
        &self.current_predictions
    }

    /// Obtener sugerencias generadas
    pub fn get_generated_suggestions(&self) -> &Vec<BehaviorSuggestion> {
        &self.generated_suggestions
    }

    /// Obtener patrones del usuario
    pub fn get_user_patterns(&self) -> &UserBehaviorPatterns {
        &self.user_patterns
    }

    /// Obtener contexto del usuario
    pub fn get_user_context(&self) -> &UserContext {
        &self.user_context
    }

    /// Aceptar sugerencia
    pub fn accept_suggestion(&mut self, suggestion_id: &str) -> Result<(), String> {
        if let Some(suggestion) = self
            .generated_suggestions
            .iter_mut()
            .find(|s| s.id == suggestion_id)
        {
            suggestion.status = SuggestionStatus::Accepted;
            self.stats.total_accepted_suggestions += 1;
        }
        Ok(())
    }

    /// Rechazar sugerencia
    pub fn reject_suggestion(&mut self, suggestion_id: &str) -> Result<(), String> {
        if let Some(suggestion) = self
            .generated_suggestions
            .iter_mut()
            .find(|s| s.id == suggestion_id)
        {
            suggestion.status = SuggestionStatus::Rejected;
        }
        Ok(())
    }

    /// Habilitar/deshabilitar el sistema
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Simular comportamiento para testing
    pub fn simulate_behavior(&mut self, frame: u32) -> Result<(), String> {
        if frame % 180 == 0 {
            // Cada 3 segundos
            let action_types = [
                UserActionType::OpenApplication,
                UserActionType::CreateFile,
                UserActionType::EditFile,
                UserActionType::SaveFile,
            ];

            let action_type = action_types[(frame / 180) as usize % action_types.len()];
            let action = UserAction {
                id: format!("sim_action_{:?}_{}", action_type, frame),
                action_type,
                application: Some(format!("sim_app_{}", frame % 5)),
                timestamp: frame,
                context: "simulation".to_string(),
                duration: 1.0 + (frame % 10) as f32,
                result: ActionResult::Success,
            };

            self.register_user_action(action)?;
        }

        Ok(())
    }

    // Métodos privados de implementación

    fn setup_behavior_models(&mut self) {
        self.behavior_models.application_prediction_model = ModelType::LinearRegression;
        self.behavior_models.action_prediction_model = ModelType::Llama;
        self.behavior_models.context_analysis_model = ModelType::TinyLlama;
        self.behavior_models.resource_optimization_model = ModelType::EfficientNet;
        self.behavior_models.pattern_detection_model = ModelType::IsolationForest;
    }

    fn create_default_patterns(&mut self) {
        // Crear patrones por defecto para diferentes horas
        for hour in 0..24 {
            let mut hourly_pattern = HourlyPattern::default();
            hourly_pattern.hour = hour;
            hourly_pattern.activity_level = self.calculate_activity_level_for_hour(hour);
            hourly_pattern.typical_context = self.get_typical_context_for_hour(hour);
            self.user_patterns
                .hourly_patterns
                .insert(hour, hourly_pattern);
        }
    }

    fn update_user_context(&mut self, frame: u32) {
        // Simular actualización del contexto
        self.user_context.current_hour = ((frame / 3600) % 24) as u8;
        self.user_context.day_of_week = ((frame / 86400) % 7) as u8;
        self.user_context.fatigue_level = 0.3 + (frame % 100) as f32 / 200.0;
        self.user_context.concentration_level = 0.7 - (frame % 100) as f32 / 300.0;
        self.user_context.resource_availability = 0.8 - (self.action_history.len() as f32 / 1000.0);
    }

    fn analyze_user_behavior(&mut self, frame: u32) -> Result<(), String> {
        // Analizar comportamiento usando IA
        if self.action_history.len() > 10 {
            let action_count = self.action_history.len();

            // Usar diferentes modelos según el tipo de análisis
            self.analyze_with_behavior_model_by_count(action_count)?;
        }

        Ok(())
    }

    fn predict_next_applications(&mut self, frame: u32) -> Result<(), String> {
        if !self.config.enable_app_prediction {
            return Ok(());
        }

        // Predecir próximas aplicaciones usando IA
        let prediction = self.create_application_prediction(frame)?;
        self.current_predictions
            .insert(prediction.id.clone(), prediction);
        self.stats.total_predictions += 1;

        Ok(())
    }

    fn predict_next_actions(&mut self, frame: u32) -> Result<(), String> {
        if !self.config.enable_action_prediction {
            return Ok(());
        }

        // Predecir próximas acciones usando IA
        let prediction = self.create_action_prediction(frame)?;
        self.current_predictions
            .insert(prediction.id.clone(), prediction);
        self.stats.total_predictions += 1;

        Ok(())
    }

    fn generate_proactive_suggestions(&mut self, frame: u32) -> Result<(), String> {
        if !self.config.enable_proactive_suggestions {
            return Ok(());
        }

        // Generar sugerencias proactivas
        let suggestion = self.create_proactive_suggestion(frame)?;
        self.generated_suggestions.push(suggestion);
        self.stats.total_suggestions += 1;

        Ok(())
    }

    fn learn_user_patterns(&mut self, frame: u32) -> Result<(), String> {
        if !self.config.enable_pattern_learning {
            return Ok(());
        }

        // Aprender patrones del usuario
        self.update_patterns_from_history();
        self.stats.patterns_learned += 1;

        Ok(())
    }

    fn analyze_user_context(&mut self, frame: u32) -> Result<(), String> {
        if !self.config.enable_context_analysis {
            return Ok(());
        }

        // Analizar contexto del usuario
        self.update_context_analysis();

        Ok(())
    }

    fn analyze_with_behavior_model(&mut self, actions: &[&UserAction]) -> Result<(), String> {
        // Simular análisis con modelo de comportamiento
        // Usar Llama para análisis de secuencias de acciones
        Ok(())
    }

    fn analyze_with_behavior_model_by_count(&mut self, action_count: usize) -> Result<(), String> {
        // Simular análisis con modelo de comportamiento basado en cantidad
        // Usar Llama para análisis de secuencias de acciones
        Ok(())
    }

    fn create_application_prediction(&self, frame: u32) -> Result<BehaviorPrediction, String> {
        let prediction_id = format!("app_pred_{}", frame);

        Ok(BehaviorPrediction {
            id: prediction_id,
            prediction_type: PredictionType::NextApplication,
            predicted_action: UserActionType::OpenApplication,
            predicted_application: Some(format!("predicted_app_{}", frame % 5)),
            confidence: 0.8,
            model_used: self.behavior_models.application_prediction_model,
            timestamp: frame,
            context: "behavior_analysis".to_string(),
        })
    }

    fn create_action_prediction(&self, frame: u32) -> Result<BehaviorPrediction, String> {
        let prediction_id = format!("action_pred_{}", frame);

        Ok(BehaviorPrediction {
            id: prediction_id,
            prediction_type: PredictionType::NextAction,
            predicted_action: UserActionType::EditFile,
            predicted_application: None,
            confidence: 0.7,
            model_used: self.behavior_models.action_prediction_model,
            timestamp: frame,
            context: "action_sequence".to_string(),
        })
    }

    fn create_proactive_suggestion(&self, frame: u32) -> Result<BehaviorSuggestion, String> {
        let suggestion_id = format!("suggestion_{}", frame);

        Ok(BehaviorSuggestion {
            id: suggestion_id,
            suggestion_type: SuggestionType::ProductivityTip,
            suggested_action: UserActionType::OpenApplication,
            suggested_application: Some("productivity_app".to_string()),
            reason: "Basado en patrones de uso".to_string(),
            priority: 5,
            timestamp: frame,
            status: SuggestionStatus::Pending,
        })
    }

    fn update_patterns_from_action(&mut self, action: &UserAction) {
        // Actualizar patrones basándose en la acción
        if let Some(hourly_pattern) = self
            .user_patterns
            .hourly_patterns
            .get_mut(&self.user_context.current_hour)
        {
            if !hourly_pattern
                .frequent_applications
                .contains(&action.application.clone().unwrap_or_default())
            {
                hourly_pattern
                    .frequent_applications
                    .push(action.application.clone().unwrap_or_default());
            }
        }
    }

    fn update_patterns_from_history(&mut self) {
        // Actualizar patrones basándose en el historial
        // Implementar lógica de aprendizaje de patrones
    }

    fn update_context_analysis(&mut self) {
        // Actualizar análisis de contexto
        // Implementar lógica de análisis de contexto
    }

    fn calculate_activity_level_for_hour(&self, hour: u8) -> f32 {
        // Calcular nivel de actividad para una hora específica
        match hour {
            9..=17 => 0.8,  // Horario laboral
            18..=22 => 0.6, // Tarde/noche
            _ => 0.3,       // Madrugada/mañana temprano
        }
    }

    fn get_typical_context_for_hour(&self, hour: u8) -> String {
        // Obtener contexto típico para una hora específica
        match hour {
            9..=17 => "work".to_string(),
            18..=22 => "leisure".to_string(),
            _ => "rest".to_string(),
        }
    }
}

impl Default for UserBehaviorConfig {
    fn default() -> Self {
        Self {
            analysis_interval: 60,
            enable_app_prediction: true,
            enable_action_prediction: true,
            enable_proactive_suggestions: true,
            enable_resource_preloading: true,
            enable_context_analysis: true,
            enable_pattern_learning: true,
            prediction_sensitivity: 0.7,
            max_analysis_time_ms: 15,
        }
    }
}

impl Default for BehaviorModels {
    fn default() -> Self {
        Self {
            application_prediction_model: ModelType::LinearRegression,
            action_prediction_model: ModelType::Llama,
            context_analysis_model: ModelType::TinyLlama,
            resource_optimization_model: ModelType::EfficientNet,
            pattern_detection_model: ModelType::IsolationForest,
        }
    }
}

impl UserContext {
    fn initialize(&mut self) {
        self.current_hour = 9; // 9 AM por defecto
        self.day_of_week = 1; // Lunes por defecto
        self.work_context = "general".to_string();
        self.fatigue_level = 0.0;
        self.concentration_level = 1.0;
        self.active_applications = Vec::new();
        self.last_action = None;
        self.system_state = "running".to_string();
        self.resource_availability = 1.0;
    }
}
