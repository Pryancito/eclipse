//! Asistente Virtual Inteligente con IA
//!
//! Este módulo utiliza los 7 modelos de IA para proporcionar asistencia
//! contextual, respuestas inteligentes y automatización de tareas.

#![no_std]

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::time::Duration;

use crate::ai_inference::{AIInferenceEngine, InferenceResult, SystemContext};
use crate::ai_models_global::{GlobalAIModelManager, ModelType};

/// Asistente Virtual Inteligente
pub struct IntelligentAssistant {
    /// Motor de inferencia de IA
    inference_engine: AIInferenceEngine,
    /// Configuración del asistente
    config: AssistantConfig,
    /// Estado del asistente
    state: AssistantState,
    /// Historial de conversaciones
    conversation_history: Vec<ConversationEntry>,
    /// Contexto del usuario
    user_context: UserContext,
    /// Perfil del usuario
    user_profile: UserProfile,
    /// Tareas automatizadas
    automated_tasks: Vec<AutomatedTask>,
    /// Comandos disponibles
    available_commands: BTreeMap<String, CommandInfo>,
    /// Estado del sistema
    enabled: bool,
    /// Frame actual
    current_frame: u32,
}

/// Configuración del asistente
#[derive(Debug, Clone)]
pub struct AssistantConfig {
    /// Intervalo de procesamiento en frames
    pub processing_interval: u32,
    /// Habilitar respuestas automáticas
    pub enable_auto_responses: bool,
    /// Habilitar aprendizaje del usuario
    pub enable_user_learning: bool,
    /// Habilitar automatización de tareas
    pub enable_task_automation: bool,
    /// Habilitar análisis de contexto
    pub enable_context_analysis: bool,
    /// Habilitar predicción de necesidades
    pub enable_need_prediction: bool,
    /// Nivel de verbosidad
    pub verbosity_level: VerbosityLevel,
    /// Tiempo máximo de respuesta
    pub max_response_time_ms: u32,
}

/// Estado del asistente
#[derive(Debug, Default)]
pub struct AssistantState {
    /// Estado actual
    pub current_state: AssistantMode,
    /// Actividad reciente
    pub recent_activity: Vec<String>,
    /// Tareas activas
    pub active_tasks: Vec<String>,
    /// Alertas pendientes
    pub pending_alerts: Vec<String>,
    /// Última interacción
    pub last_interaction: u32,
    /// Nivel de confianza
    pub confidence_level: f32,
    /// Estado de aprendizaje
    pub learning_state: LearningState,
}

/// Modos del asistente
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantMode {
    /// Modo inactivo
    Idle,
    /// Modo de escucha
    Listening,
    /// Modo de procesamiento
    Processing,
    /// Modo de respuesta
    Responding,
    /// Modo de aprendizaje
    Learning,
    /// Modo de automatización
    Automating,
    /// Modo de análisis
    Analyzing,
}

impl Default for AssistantMode {
    fn default() -> Self {
        AssistantMode::Idle
    }
}

/// Niveles de verbosidad
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerbosityLevel {
    /// Mínimo
    Minimal,
    /// Bajo
    Low,
    /// Medio
    Medium,
    /// Alto
    High,
    /// Máximo
    Maximum,
}

/// Estados de aprendizaje
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningState {
    /// No aprendiendo
    NotLearning,
    /// Aprendiendo patrones
    LearningPatterns,
    /// Aprendiendo preferencias
    LearningPreferences,
    /// Aprendiendo comandos
    LearningCommands,
    /// Aprendiendo contexto
    LearningContext,
}

impl Default for LearningState {
    fn default() -> Self {
        LearningState::NotLearning
    }
}

/// Entrada de conversación
#[derive(Debug, Clone)]
pub struct ConversationEntry {
    /// ID de la entrada
    pub id: String,
    /// Timestamp de la entrada
    pub timestamp: u32,
    /// Tipo de entrada
    pub entry_type: ConversationType,
    /// Contenido del usuario
    pub user_input: String,
    /// Respuesta del asistente
    pub assistant_response: String,
    /// Confianza de la respuesta
    pub response_confidence: f32,
    /// Contexto de la conversación
    pub context: ConversationContext,
    /// Satisfacción del usuario
    pub user_satisfaction: Option<f32>,
}

/// Tipos de conversación
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationType {
    /// Pregunta
    Question,
    /// Comando
    Command,
    /// Solicitud de ayuda
    HelpRequest,
    /// Comentario
    Comment,
    /// Queja
    Complaint,
    /// Sugerencia
    Suggestion,
    /// Conversación casual
    Casual,
}

/// Contexto de conversación
#[derive(Debug, Clone, Default)]
pub struct ConversationContext {
    /// Aplicaciones activas
    pub active_applications: Vec<String>,
    /// Tareas en ejecución
    pub running_tasks: Vec<String>,
    /// Estado del sistema
    pub system_state: String,
    /// Hora del día
    pub time_of_day: String,
    /// Día de la semana
    pub day_of_week: String,
    /// Ubicación del usuario
    pub user_location: String,
}

/// Contexto del usuario
#[derive(Debug, Default)]
pub struct UserContext {
    /// Preferencias del usuario
    pub preferences: BTreeMap<String, String>,
    /// Historial de comandos
    pub command_history: Vec<String>,
    /// Patrones de uso
    pub usage_patterns: BTreeMap<String, f32>,
    /// Nivel de experiencia
    pub experience_level: ExperienceLevel,
    /// Idiomas preferidos
    pub preferred_languages: Vec<String>,
    /// Zona horaria
    pub timezone: String,
}

/// Niveles de experiencia
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExperienceLevel {
    /// Principiante
    Beginner,
    /// Intermedio
    Intermediate,
    /// Avanzado
    Advanced,
    /// Experto
    Expert,
}

impl Default for ExperienceLevel {
    fn default() -> Self {
        ExperienceLevel::Intermediate
    }
}

/// Perfil del usuario
#[derive(Debug, Default)]
pub struct UserProfile {
    /// Nombre del usuario
    pub name: String,
    /// Edad estimada
    pub estimated_age: Option<u8>,
    /// Ocupación
    pub occupation: String,
    /// Intereses
    pub interests: Vec<String>,
    /// Habilidades técnicas
    pub technical_skills: Vec<String>,
    /// Patrones de trabajo
    pub work_patterns: BTreeMap<String, f32>,
    /// Preferencias de interfaz
    pub interface_preferences: BTreeMap<String, String>,
    /// Nivel de experiencia
    pub experience_level: ExperienceLevel,
    /// Idiomas preferidos
    pub preferred_languages: Vec<String>,
    /// Zona horaria
    pub timezone: String,
}

/// Tarea automatizada
#[derive(Debug, Clone)]
pub struct AutomatedTask {
    /// ID de la tarea
    pub id: String,
    /// Nombre de la tarea
    pub name: String,
    /// Descripción de la tarea
    pub description: String,
    /// Tipo de tarea
    pub task_type: TaskType,
    /// Estado de la tarea
    pub status: TaskStatus,
    /// Prioridad de la tarea
    pub priority: u32,
    /// Tiempo de creación
    pub created_at: u32,
    /// Tiempo de ejecución
    pub execution_time: Option<u32>,
    /// Resultado de la tarea
    pub result: Option<TaskResult>,
    /// Contexto de la tarea
    pub context: TaskContext,
}

/// Tipos de tareas
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskType {
    /// Tarea del sistema
    System,
    /// Tarea de aplicación
    Application,
    /// Tarea de mantenimiento
    Maintenance,
    /// Tarea de optimización
    Optimization,
    /// Tarea de seguridad
    Security,
    /// Tarea de respaldo
    Backup,
    /// Tarea de limpieza
    Cleanup,
}

/// Estados de tareas
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Pendiente
    Pending,
    /// En ejecución
    Running,
    /// Completada
    Completed,
    /// Fallida
    Failed,
    /// Cancelada
    Cancelled,
    /// Pausada
    Paused,
}

/// Resultado de tarea
#[derive(Debug, Clone)]
pub struct TaskResult {
    /// Éxito de la tarea
    pub success: bool,
    /// Mensaje de resultado
    pub message: String,
    /// Datos de resultado
    pub data: BTreeMap<String, String>,
    /// Tiempo de ejecución
    pub execution_time_ms: u32,
    /// Recursos utilizados
    pub resources_used: BTreeMap<String, f32>,
}

/// Contexto de tarea
#[derive(Debug, Clone, Default)]
pub struct TaskContext {
    /// Aplicaciones relacionadas
    pub related_applications: Vec<String>,
    /// Archivos involucrados
    pub involved_files: Vec<String>,
    /// Configuraciones afectadas
    pub affected_configurations: Vec<String>,
    /// Dependencias
    pub dependencies: Vec<String>,
}

/// Información de comando
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// Nombre del comando
    pub name: String,
    /// Descripción del comando
    pub description: String,
    /// Sintaxis del comando
    pub syntax: String,
    /// Parámetros del comando
    pub parameters: Vec<ParameterInfo>,
    /// Ejemplos de uso
    pub examples: Vec<String>,
    /// Categoría del comando
    pub category: CommandCategory,
    /// Nivel de acceso requerido
    pub access_level: AccessLevel,
}

/// Información de parámetro
#[derive(Debug, Clone)]
pub struct ParameterInfo {
    /// Nombre del parámetro
    pub name: String,
    /// Tipo del parámetro
    pub parameter_type: ParameterType,
    /// Descripción del parámetro
    pub description: String,
    /// Si es requerido
    pub required: bool,
    /// Valor por defecto
    pub default_value: Option<String>,
}

/// Tipos de parámetros
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterType {
    /// String
    String,
    /// Número entero
    Integer,
    /// Número flotante
    Float,
    /// Booleano
    Boolean,
    /// Archivo
    File,
    /// Directorio
    Directory,
    /// URL
    Url,
    /// Email
    Email,
}

/// Categorías de comandos
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandCategory {
    /// Sistema
    System,
    /// Archivos
    Files,
    /// Red
    Network,
    /// Aplicaciones
    Applications,
    /// Configuración
    Configuration,
    /// Utilidades
    Utilities,
    /// Desarrollo
    Development,
    /// Multimedia
    Multimedia,
}

/// Niveles de acceso
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessLevel {
    /// Público
    Public,
    /// Usuario
    User,
    /// Administrador
    Administrator,
    /// Sistema
    System,
}

impl IntelligentAssistant {
    /// Crear nuevo asistente inteligente
    pub fn new() -> Self {
        Self {
            inference_engine: AIInferenceEngine::new(),
            config: AssistantConfig::default(),
            state: AssistantState::default(),
            conversation_history: Vec::new(),
            user_context: UserContext::default(),
            user_profile: UserProfile::default(),
            automated_tasks: Vec::new(),
            available_commands: BTreeMap::new(),
            enabled: true,
            current_frame: 0,
        }
    }

    /// Crear asistente con configuración personalizada
    pub fn with_config(config: AssistantConfig) -> Self {
        Self {
            inference_engine: AIInferenceEngine::new(),
            config,
            state: AssistantState::default(),
            conversation_history: Vec::new(),
            user_context: UserContext::default(),
            user_profile: UserProfile::default(),
            automated_tasks: Vec::new(),
            available_commands: BTreeMap::new(),
            enabled: true,
            current_frame: 0,
        }
    }

    /// Inicializar el asistente
    pub fn initialize(&mut self) -> Result<(), String> {
        // Inicializar motor de inferencia
        self.inference_engine = AIInferenceEngine::new();

        // Configurar estado inicial
        self.state.current_state = AssistantMode::Idle;
        self.state.confidence_level = 0.8;
        self.state.learning_state = LearningState::NotLearning;

        // Inicializar comandos disponibles
        self.initialize_commands();

        // Inicializar perfil del usuario
        self.initialize_user_profile();

        Ok(())
    }

    /// Actualizar el asistente
    pub fn update(&mut self, frame: u32, system_context: &SystemContext) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        self.current_frame = frame;
        self.state.last_interaction = frame;

        // Actualizar contexto del usuario
        self.update_user_context(system_context);

        // Procesar conversaciones pendientes
        if frame % self.config.processing_interval == 0 {
            self.process_conversations(frame)?;
        }

        // Ejecutar tareas automatizadas
        if self.config.enable_task_automation && frame % 60 == 0 {
            // Cada segundo
            self.execute_automated_tasks(frame)?;
        }

        // Aprender del usuario
        if self.config.enable_user_learning && frame % 300 == 0 {
            // Cada 5 segundos
            self.learn_from_user_behavior(frame)?;
        }

        // Analizar contexto
        if self.config.enable_context_analysis && frame % 180 == 0 {
            // Cada 3 segundos
            self.analyze_context(frame)?;
        }

        // Predecir necesidades del usuario
        if self.config.enable_need_prediction && frame % 600 == 0 {
            // Cada 10 segundos
            self.predict_user_needs(frame)?;
        }

        Ok(())
    }

    /// Procesar entrada del usuario
    pub fn process_user_input(
        &mut self,
        input: String,
        context: ConversationContext,
    ) -> Result<String, String> {
        if !self.enabled {
            return Err("Asistente deshabilitado".to_string());
        }

        // Cambiar a modo de procesamiento
        self.state.current_state = AssistantMode::Processing;

        // Determinar tipo de entrada
        let entry_type = self.classify_user_input(&input);

        // Generar respuesta usando IA
        let response = self.generate_response(&input, &context, entry_type)?;

        // Crear entrada de conversación
        let conversation_entry = ConversationEntry {
            id: format!("conv_{}", self.current_frame),
            timestamp: self.current_frame,
            entry_type,
            user_input: input.clone(),
            assistant_response: response.clone(),
            response_confidence: self.state.confidence_level,
            context: context.clone(),
            user_satisfaction: None,
        };

        // Agregar a historial
        self.conversation_history.push(conversation_entry);

        // Cambiar a modo de respuesta
        self.state.current_state = AssistantMode::Responding;

        Ok(response)
    }

    /// Obtener estado del asistente
    pub fn get_state(&self) -> &AssistantState {
        &self.state
    }

    /// Obtener historial de conversaciones
    pub fn get_conversation_history(&self) -> &Vec<ConversationEntry> {
        &self.conversation_history
    }

    /// Obtener perfil del usuario
    pub fn get_user_profile(&self) -> &UserProfile {
        &self.user_profile
    }

    /// Obtener tareas automatizadas
    pub fn get_automated_tasks(&self) -> &Vec<AutomatedTask> {
        &self.automated_tasks
    }

    /// Obtener comandos disponibles
    pub fn get_available_commands(&self) -> &BTreeMap<String, CommandInfo> {
        &self.available_commands
    }

    /// Configurar el asistente
    pub fn configure(&mut self, config: AssistantConfig) {
        self.config = config;
    }

    /// Habilitar/deshabilitar el asistente
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Obtener recomendaciones para el usuario
    pub fn get_recommendations(&self) -> Vec<String> {
        let mut recommendations = Vec::new();

        // Recomendaciones basadas en el contexto del usuario
        if self.user_context.experience_level == ExperienceLevel::Beginner {
            recommendations.push("Usa 'ayuda' para ver comandos disponibles".to_string());
            recommendations.push("Usa 'tutorial' para aprender funciones básicas".to_string());
        }

        // Recomendaciones basadas en patrones de uso
        if self.user_context.command_history.len() > 10 {
            recommendations.push("Considera crear alias para comandos frecuentes".to_string());
        }

        // Recomendaciones basadas en el estado del sistema
        if self.state.confidence_level < 0.5 {
            recommendations
                .push("Proporciona más contexto para respuestas más precisas".to_string());
        }

        recommendations
    }

    /// Obtener estadísticas del asistente
    pub fn get_assistant_stats(&self) -> AssistantStats {
        AssistantStats {
            total_conversations: self.conversation_history.len() as u32,
            average_confidence: self.state.confidence_level,
            active_tasks: self
                .automated_tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Running)
                .count() as u32,
            completed_tasks: self
                .automated_tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Completed)
                .count() as u32,
            available_commands: self.available_commands.len() as u32,
            learning_state: self.state.learning_state,
            last_interaction: self.state.last_interaction,
        }
    }

    // Métodos privados de implementación

    fn initialize_commands(&mut self) {
        // Comandos del sistema
        self.add_command(
            "ayuda",
            "Mostrar ayuda del asistente",
            "ayuda [comando]",
            CommandCategory::System,
            AccessLevel::Public,
        );
        self.add_command(
            "estado",
            "Mostrar estado del sistema",
            "estado",
            CommandCategory::System,
            AccessLevel::Public,
        );
        self.add_command(
            "configurar",
            "Configurar el asistente",
            "configurar [opción] [valor]",
            CommandCategory::Configuration,
            AccessLevel::User,
        );

        // Comandos de archivos
        self.add_command(
            "listar",
            "Listar archivos y directorios",
            "listar [ruta]",
            CommandCategory::Files,
            AccessLevel::Public,
        );
        self.add_command(
            "crear",
            "Crear archivo o directorio",
            "crear [tipo] [nombre]",
            CommandCategory::Files,
            AccessLevel::User,
        );
        self.add_command(
            "eliminar",
            "Eliminar archivo o directorio",
            "eliminar [ruta]",
            CommandCategory::Files,
            AccessLevel::User,
        );

        // Comandos de aplicaciones
        self.add_command(
            "abrir",
            "Abrir aplicación",
            "abrir [aplicación]",
            CommandCategory::Applications,
            AccessLevel::Public,
        );
        self.add_command(
            "cerrar",
            "Cerrar aplicación",
            "cerrar [aplicación]",
            CommandCategory::Applications,
            AccessLevel::Public,
        );
        self.add_command(
            "listar_apps",
            "Listar aplicaciones disponibles",
            "listar_apps",
            CommandCategory::Applications,
            AccessLevel::Public,
        );

        // Comandos de red
        self.add_command(
            "conectar",
            "Conectar a red",
            "conectar [red]",
            CommandCategory::Network,
            AccessLevel::User,
        );
        self.add_command(
            "desconectar",
            "Desconectar de red",
            "desconectar",
            CommandCategory::Network,
            AccessLevel::User,
        );
        self.add_command(
            "estado_red",
            "Mostrar estado de la red",
            "estado_red",
            CommandCategory::Network,
            AccessLevel::Public,
        );
    }

    fn add_command(
        &mut self,
        name: &str,
        description: &str,
        syntax: &str,
        category: CommandCategory,
        access_level: AccessLevel,
    ) {
        let command_info = CommandInfo {
            name: name.to_string(),
            description: description.to_string(),
            syntax: syntax.to_string(),
            parameters: Vec::new(),
            examples: Vec::new(),
            category,
            access_level,
        };

        self.available_commands
            .insert(name.to_string(), command_info);
    }

    fn initialize_user_profile(&mut self) {
        self.user_profile.name = "Usuario".to_string();
        self.user_profile.experience_level = ExperienceLevel::Intermediate;
        self.user_profile.preferred_languages.push("es".to_string());
        self.user_profile.timezone = "UTC".to_string();
    }

    fn update_user_context(&mut self, system_context: &SystemContext) {
        // Actualizar contexto basándose en el estado del sistema
        self.user_context.preferences.insert(
            "cpu_usage".to_string(),
            system_context.cpu_usage.to_string(),
        );
        self.user_context.preferences.insert(
            "memory_usage".to_string(),
            system_context.memory_usage.to_string(),
        );
        self.user_context.preferences.insert(
            "active_processes".to_string(),
            system_context.active_processes.to_string(),
        );
    }

    fn process_conversations(&mut self, frame: u32) -> Result<(), String> {
        // Procesar conversaciones pendientes
        // Simular procesamiento de conversaciones
        Ok(())
    }

    fn execute_automated_tasks(&mut self, frame: u32) -> Result<(), String> {
        // Ejecutar tareas automatizadas
        for task in &mut self.automated_tasks {
            if task.status == TaskStatus::Pending && task.priority > 0 {
                task.status = TaskStatus::Running;
                task.execution_time = Some(frame);

                // Simular ejecución de tarea
                let result = TaskResult {
                    success: true,
                    message: format!("Tarea {} completada", task.name),
                    data: BTreeMap::new(),
                    execution_time_ms: 100,
                    resources_used: BTreeMap::new(),
                };

                task.result = Some(result);
                task.status = TaskStatus::Completed;
            }
        }

        Ok(())
    }

    fn learn_from_user_behavior(&mut self, frame: u32) -> Result<(), String> {
        // Aprender del comportamiento del usuario
        self.state.learning_state = LearningState::LearningPatterns;

        // Simular aprendizaje de patrones
        if self.conversation_history.len() > 0 {
            let recent_entries =
                &self.conversation_history[self.conversation_history.len().saturating_sub(5)..];
            for entry in recent_entries {
                // Aprender de patrones de conversación
                self.user_context.usage_patterns.insert(
                    entry.entry_type.to_string(),
                    self.user_context
                        .usage_patterns
                        .get(&entry.entry_type.to_string())
                        .unwrap_or(&0.0)
                        + 1.0,
                );
            }
        }

        self.state.learning_state = LearningState::NotLearning;
        Ok(())
    }

    fn analyze_context(&mut self, frame: u32) -> Result<(), String> {
        // Analizar contexto del usuario
        self.state.current_state = AssistantMode::Analyzing;

        // Simular análisis de contexto
        self.state.confidence_level = 0.8 + (frame % 100) as f32 / 500.0;

        self.state.current_state = AssistantMode::Idle;
        Ok(())
    }

    fn predict_user_needs(&mut self, frame: u32) -> Result<(), String> {
        // Predecir necesidades del usuario
        // Simular predicción de necesidades
        Ok(())
    }

    fn classify_user_input(&self, input: &str) -> ConversationType {
        let input_lower = input.to_lowercase();

        if input_lower.contains("?") || input_lower.contains("qué") || input_lower.contains("cómo")
        {
            ConversationType::Question
        } else if input_lower.starts_with("ayuda") || input_lower.contains("help") {
            ConversationType::HelpRequest
        } else if input_lower.starts_with("hacer") || input_lower.starts_with("ejecutar") {
            ConversationType::Command
        } else if input_lower.contains("problema") || input_lower.contains("error") {
            ConversationType::Complaint
        } else if input_lower.contains("sugerir") || input_lower.contains("recomendar") {
            ConversationType::Suggestion
        } else {
            ConversationType::Casual
        }
    }

    fn generate_response(
        &mut self,
        input: &str,
        context: &ConversationContext,
        entry_type: ConversationType,
    ) -> Result<String, String> {
        // Generar respuesta usando el motor de inferencia
        match self.inference_engine.generate_conversation(input, None) {
            Ok(result) => {
                // Ajustar respuesta basándose en el tipo de entrada
                let response = match entry_type {
                    ConversationType::Question => {
                        format!("Basándome en tu pregunta: {}", result.output_text)
                    }
                    ConversationType::Command => {
                        format!("Ejecutando comando: {}", result.output_text)
                    }
                    ConversationType::HelpRequest => {
                        format!("Aquí tienes ayuda: {}", result.output_text)
                    }
                    ConversationType::Complaint => {
                        format!("Entiendo tu preocupación: {}", result.output_text)
                    }
                    ConversationType::Suggestion => {
                        format!("Gracias por tu sugerencia: {}", result.output_text)
                    }
                    _ => result.output_text.to_string(),
                };

                Ok(response)
            }
            Err(e) => {
                // Respuesta de fallback
                Ok(format!("Lo siento, no pude procesar tu solicitud: {}", e))
            }
        }
    }
}

/// Estadísticas del asistente
#[derive(Debug)]
pub struct AssistantStats {
    /// Total de conversaciones
    pub total_conversations: u32,
    /// Confianza promedio
    pub average_confidence: f32,
    /// Tareas activas
    pub active_tasks: u32,
    /// Tareas completadas
    pub completed_tasks: u32,
    /// Comandos disponibles
    pub available_commands: u32,
    /// Estado de aprendizaje
    pub learning_state: LearningState,
    /// Última interacción
    pub last_interaction: u32,
}

impl Default for AssistantConfig {
    fn default() -> Self {
        Self {
            processing_interval: 60, // Cada segundo
            enable_auto_responses: true,
            enable_user_learning: true,
            enable_task_automation: true,
            enable_context_analysis: true,
            enable_need_prediction: true,
            verbosity_level: VerbosityLevel::Medium,
            max_response_time_ms: 1000,
        }
    }
}

impl ToString for ConversationType {
    fn to_string(&self) -> String {
        match self {
            ConversationType::Question => "Pregunta".to_string(),
            ConversationType::Command => "Comando".to_string(),
            ConversationType::HelpRequest => "Solicitud de ayuda".to_string(),
            ConversationType::Comment => "Comentario".to_string(),
            ConversationType::Complaint => "Queja".to_string(),
            ConversationType::Suggestion => "Sugerencia".to_string(),
            ConversationType::Casual => "Conversación casual".to_string(),
        }
    }
}

impl ToString for AssistantMode {
    fn to_string(&self) -> String {
        match self {
            AssistantMode::Idle => "Inactivo".to_string(),
            AssistantMode::Listening => "Escuchando".to_string(),
            AssistantMode::Processing => "Procesando".to_string(),
            AssistantMode::Responding => "Respondiendo".to_string(),
            AssistantMode::Learning => "Aprendiendo".to_string(),
            AssistantMode::Automating => "Automatizando".to_string(),
            AssistantMode::Analyzing => "Analizando".to_string(),
        }
    }
}

impl ToString for LearningState {
    fn to_string(&self) -> String {
        match self {
            LearningState::NotLearning => "No aprendiendo".to_string(),
            LearningState::LearningPatterns => "Aprendiendo patrones".to_string(),
            LearningState::LearningPreferences => "Aprendiendo preferencias".to_string(),
            LearningState::LearningCommands => "Aprendiendo comandos".to_string(),
            LearningState::LearningContext => "Aprendiendo contexto".to_string(),
        }
    }
}
