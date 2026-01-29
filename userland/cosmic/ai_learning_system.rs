// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use core::cmp::Ordering;
use heapless::{String, Vec};

/// Helper function to convert &str to heapless::String<64>
fn str_to_heapless(s: &str) -> String<64> {
    let mut result = String::new();
    for ch in s.chars().take(63) {
        if result.push(ch).is_err() {
            break;
        }
    }
    result
}

/// Sistema de aprendizaje adaptativo para la IA de COSMIC
/// Respeta la arquitectura existente y mejora el comportamiento basándose en patrones del usuario
pub struct AILearningSystem {
    /// Patrones de comportamiento del usuario
    user_patterns: Vec<UserPattern, 100>,
    /// Preferencias aprendidas
    learned_preferences: Vec<LearnedPreference, 50>,
    /// Métricas de rendimiento
    performance_metrics: PerformanceMetrics,
    /// Configuraciones adaptativas
    adaptive_configs: Vec<AdaptiveConfig, 30>,
    /// Historial de decisiones
    decision_history: Vec<DecisionRecord, 200>,
    /// Nivel de confianza en las predicciones
    confidence_level: f32,
}

/// Patrón de comportamiento del usuario
#[derive(Clone, Debug)]
pub struct UserPattern {
    pub pattern_id: u32,
    pub pattern_type: PatternType,
    pub frequency: u32,
    pub success_rate: f32,
    pub context: String<64>,
    pub timestamp: u64,
    pub weight: f32,
}

/// Tipo de patrón de comportamiento
#[derive(Clone, Debug, PartialEq)]
pub enum PatternType {
    /// Patrón de uso de ventanas
    WindowUsage,
    /// Patrón de navegación
    Navigation,
    /// Patrón de preferencias visuales
    VisualPreference,
    /// Patrón de interacción con applets
    AppletInteraction,
    /// Patrón de uso del portal de escritorio
    PortalUsage,
    /// Patrón de notificaciones
    NotificationPreference,
}

/// Preferencia aprendida del usuario
#[derive(Clone, Debug)]
pub struct LearnedPreference {
    pub preference_id: u32,
    pub category: PreferenceCategory,
    pub value: String<64>,
    pub confidence: f32,
    pub usage_count: u32,
    pub last_used: u64,
}

/// Categoría de preferencia
#[derive(Clone, Debug, PartialEq)]
pub enum PreferenceCategory {
    /// Preferencias de ventanas
    WindowManagement,
    /// Preferencias de applets
    AppletConfiguration,
    /// Preferencias de notificaciones
    NotificationSettings,
    /// Preferencias de renderizado
    RenderingOptions,
    /// Preferencias de portal
    PortalConfiguration,
}

/// Métricas de rendimiento del sistema
#[derive(Clone, Debug)]
pub struct PerformanceMetrics {
    pub total_interactions: u32,
    pub successful_predictions: u32,
    pub failed_predictions: u32,
    pub user_satisfaction: f32,
    pub system_efficiency: f32,
    pub learning_rate: f32,
    pub adaptation_speed: f32,
}

/// Configuración adaptativa
#[derive(Clone, Debug)]
pub struct AdaptiveConfig {
    pub config_id: u32,
    pub component: String<64>,
    pub parameter: String<64>,
    pub current_value: String<64>,
    pub optimal_value: String<64>,
    pub confidence: f32,
    pub last_updated: u64,
}

/// Registro de decisión de la IA
#[derive(Clone, Debug)]
pub struct DecisionRecord {
    pub decision_id: u32,
    pub context: String<64>,
    pub action_taken: String<64>,
    pub user_feedback: Option<UserFeedback>,
    pub success: bool,
    pub timestamp: u64,
    pub learning_applied: bool,
}

/// Feedback del usuario
#[derive(Clone, Debug)]
pub enum UserFeedback {
    Positive,
    Negative,
    Neutral,
    Explicit(String<64>), // Feedback explícito del usuario
}

impl AILearningSystem {
    /// Crear nuevo sistema de aprendizaje
    pub fn new() -> Self {
        Self {
            user_patterns: Vec::new(),
            learned_preferences: Vec::new(),
            performance_metrics: PerformanceMetrics {
                total_interactions: 0,
                successful_predictions: 0,
                failed_predictions: 0,
                user_satisfaction: 0.5,
                system_efficiency: 0.5,
                learning_rate: 0.1,
                adaptation_speed: 0.05,
            },
            adaptive_configs: Vec::new(),
            decision_history: Vec::new(),
            confidence_level: 0.5,
        }
    }

    /// Aprender de una interacción del usuario
    pub fn learn_from_interaction(
        &mut self,
        interaction: &UserInteraction,
    ) -> Result<(), String<64>> {
        // Analizar el patrón de la interacción
        let pattern = self.analyze_interaction_pattern(interaction)?;

        // Actualizar o crear patrón de usuario
        self.update_user_pattern(pattern)?;

        // Aprender preferencias implícitas
        self.learn_preferences_from_interaction(interaction)?;

        // Actualizar métricas de rendimiento
        self.update_performance_metrics(interaction);

        // Ajustar configuraciones adaptativas
        self.adjust_adaptive_configs(interaction);

        Ok(())
    }

    /// Analizar patrón de interacción
    fn analyze_interaction_pattern(
        &self,
        interaction: &UserInteraction,
    ) -> Result<UserPattern, String<64>> {
        let pattern_type = match interaction.action_type {
            ActionType::WindowOperation => PatternType::WindowUsage,
            ActionType::AppletInteraction => PatternType::AppletInteraction,
            ActionType::NotificationAction => PatternType::NotificationPreference,
            ActionType::PortalRequest => PatternType::PortalUsage,
            ActionType::VisualChange => PatternType::VisualPreference,
            ActionType::Navigation => PatternType::Navigation,
        };

        Ok(UserPattern {
            pattern_id: self.generate_pattern_id(),
            pattern_type,
            frequency: 1,
            success_rate: if interaction.success { 1.0 } else { 0.0 },
            context: interaction.context.clone(),
            timestamp: interaction.timestamp,
            weight: 1.0,
        })
    }

    /// Actualizar patrón de usuario existente o crear uno nuevo
    fn update_user_pattern(&mut self, new_pattern: UserPattern) -> Result<(), String<64>> {
        // Buscar patrón similar existente
        if let Some(existing_index) = self.find_similar_pattern(&new_pattern) {
            // Actualizar patrón existente
            let existing = &mut self.user_patterns[existing_index];
            existing.frequency += 1;
            existing.success_rate = (existing.success_rate + new_pattern.success_rate) / 2.0;
            // Calcular peso directamente sin llamar a self
            let frequency_weight = (existing.frequency as f32).min(10.0) / 10.0;
            let recency_weight = 1.0; // En una implementación real, calcular basándose en timestamp
            let success_weight = existing.success_rate;
            existing.weight = (frequency_weight + recency_weight + success_weight) / 3.0;
        } else {
            // Agregar nuevo patrón
            if self.user_patterns.len() >= self.user_patterns.capacity() {
                // Remover patrón menos relevante si está lleno
                self.remove_least_relevant_pattern()?;
            }
            self.user_patterns
                .push(new_pattern)
                .map_err(|_| str_to_heapless("No se pudo agregar patrón"))?;
        }

        Ok(())
    }

    /// Aprender preferencias de la interacción
    fn learn_preferences_from_interaction(
        &mut self,
        interaction: &UserInteraction,
    ) -> Result<(), String<64>> {
        let preferences = self.extract_preferences_from_interaction(interaction);

        for preference in preferences {
            if let Some(existing_index) = self.find_similar_preference(&preference) {
                // Actualizar preferencia existente
                let existing = &mut self.learned_preferences[existing_index];
                existing.confidence = (existing.confidence + preference.confidence) / 2.0;
                existing.usage_count += 1;
                existing.last_used = interaction.timestamp;
            } else {
                // Agregar nueva preferencia
                if self.learned_preferences.len() >= self.learned_preferences.capacity() {
                    self.remove_least_used_preference()?;
                }
                self.learned_preferences
                    .push(preference)
                    .map_err(|_| str_to_heapless("No se pudo agregar preferencia"))?;
            }
        }

        Ok(())
    }

    /// Extraer preferencias de una interacción
    fn extract_preferences_from_interaction(
        &self,
        interaction: &UserInteraction,
    ) -> Vec<LearnedPreference, 10> {
        let mut preferences = Vec::new();

        match interaction.action_type {
            ActionType::WindowOperation => {
                if let Some(window_pref) = self.extract_window_preference(interaction) {
                    let _ = preferences.push(window_pref);
                }
            }
            ActionType::AppletInteraction => {
                if let Some(applet_pref) = self.extract_applet_preference(interaction) {
                    let _ = preferences.push(applet_pref);
                }
            }
            ActionType::NotificationAction => {
                if let Some(notif_pref) = self.extract_notification_preference(interaction) {
                    let _ = preferences.push(notif_pref);
                }
            }
            ActionType::PortalRequest => {
                if let Some(portal_pref) = self.extract_portal_preference(interaction) {
                    let _ = preferences.push(portal_pref);
                }
            }
            ActionType::VisualChange => {
                if let Some(visual_pref) = self.extract_visual_preference(interaction) {
                    let _ = preferences.push(visual_pref);
                }
            }
            ActionType::Navigation => {
                if let Some(nav_pref) = self.extract_navigation_preference(interaction) {
                    let _ = preferences.push(nav_pref);
                }
            }
        }

        preferences
    }

    /// Extraer preferencia de ventana
    fn extract_window_preference(
        &self,
        interaction: &UserInteraction,
    ) -> Option<LearnedPreference> {
        // Analizar el contexto para extraer preferencias de ventana
        if interaction.context.contains("maximize") {
            Some(LearnedPreference {
                preference_id: self.generate_preference_id(),
                category: PreferenceCategory::WindowManagement,
                value: str_to_heapless("prefer_maximize"),
                confidence: 0.7,
                usage_count: 1,
                last_used: interaction.timestamp,
            })
        } else if interaction.context.contains("minimize") {
            Some(LearnedPreference {
                preference_id: self.generate_preference_id(),
                category: PreferenceCategory::WindowManagement,
                value: str_to_heapless("prefer_minimize"),
                confidence: 0.7,
                usage_count: 1,
                last_used: interaction.timestamp,
            })
        } else {
            None
        }
    }

    /// Extraer preferencia de applet
    fn extract_applet_preference(
        &self,
        interaction: &UserInteraction,
    ) -> Option<LearnedPreference> {
        if interaction.context.contains("clock") {
            Some(LearnedPreference {
                preference_id: self.generate_preference_id(),
                category: PreferenceCategory::AppletConfiguration,
                value: str_to_heapless("prefer_clock_applet"),
                confidence: 0.8,
                usage_count: 1,
                last_used: interaction.timestamp,
            })
        } else {
            None
        }
    }

    /// Extraer preferencia de notificación
    fn extract_notification_preference(
        &self,
        interaction: &UserInteraction,
    ) -> Option<LearnedPreference> {
        if interaction.context.contains("dismiss") {
            Some(LearnedPreference {
                preference_id: self.generate_preference_id(),
                category: PreferenceCategory::NotificationSettings,
                value: str_to_heapless("prefer_quick_dismiss"),
                confidence: 0.6,
                usage_count: 1,
                last_used: interaction.timestamp,
            })
        } else {
            None
        }
    }

    /// Extraer preferencia de portal
    fn extract_portal_preference(
        &self,
        interaction: &UserInteraction,
    ) -> Option<LearnedPreference> {
        if interaction.context.contains("screenshot") {
            Some(LearnedPreference {
                preference_id: self.generate_preference_id(),
                category: PreferenceCategory::PortalConfiguration,
                value: str_to_heapless("frequent_screenshot"),
                confidence: 0.7,
                usage_count: 1,
                last_used: interaction.timestamp,
            })
        } else {
            None
        }
    }

    /// Extraer preferencia visual
    fn extract_visual_preference(
        &self,
        interaction: &UserInteraction,
    ) -> Option<LearnedPreference> {
        if interaction.context.contains("dark_theme") {
            Some(LearnedPreference {
                preference_id: self.generate_preference_id(),
                category: PreferenceCategory::RenderingOptions,
                value: str_to_heapless("prefer_dark_theme"),
                confidence: 0.8,
                usage_count: 1,
                last_used: interaction.timestamp,
            })
        } else {
            None
        }
    }

    /// Extraer preferencia de navegación
    fn extract_navigation_preference(
        &self,
        interaction: &UserInteraction,
    ) -> Option<LearnedPreference> {
        if interaction.context.contains("keyboard_shortcut") {
            Some(LearnedPreference {
                preference_id: self.generate_preference_id(),
                category: PreferenceCategory::WindowManagement,
                value: str_to_heapless("prefer_keyboard_navigation"),
                confidence: 0.6,
                usage_count: 1,
                last_used: interaction.timestamp,
            })
        } else {
            None
        }
    }

    /// Actualizar métricas de rendimiento
    fn update_performance_metrics(&mut self, interaction: &UserInteraction) {
        self.performance_metrics.total_interactions += 1;

        if interaction.success {
            self.performance_metrics.successful_predictions += 1;
        } else {
            self.performance_metrics.failed_predictions += 1;
        }

        // Calcular satisfacción del usuario basada en el feedback
        if let Some(feedback) = &interaction.feedback {
            match feedback {
                UserFeedback::Positive => {
                    self.performance_metrics.user_satisfaction =
                        (self.performance_metrics.user_satisfaction + 0.1).min(1.0);
                }
                UserFeedback::Negative => {
                    self.performance_metrics.user_satisfaction =
                        (self.performance_metrics.user_satisfaction - 0.1).max(0.0);
                }
                _ => {} // Neutral no cambia la satisfacción
            }
        }

        // Calcular eficiencia del sistema
        let success_rate = self.performance_metrics.successful_predictions as f32
            / self.performance_metrics.total_interactions as f32;
        self.performance_metrics.system_efficiency = success_rate;
    }

    /// Ajustar configuraciones adaptativas
    fn adjust_adaptive_configs(&mut self, interaction: &UserInteraction) -> Result<(), String<64>> {
        // Ajustar configuraciones basándose en patrones aprendidos
        let pattern_indices: Vec<usize, 10> = self
            .user_patterns
            .iter()
            .enumerate()
            .filter(|(_, p)| p.frequency > 3 && p.success_rate > 0.7)
            .map(|(i, _)| i)
            .collect();

        for pattern_index in pattern_indices {
            self.apply_pattern_to_config_by_index(pattern_index)?;
        }

        Ok(())
    }

    /// Aplicar patrón a configuración por índice
    fn apply_pattern_to_config_by_index(&mut self, pattern_index: usize) -> Result<(), String<64>> {
        if pattern_index >= self.user_patterns.len() {
            return Ok(());
        }

        // Clonar el patrón para evitar problemas de borrowing
        let pattern = self.user_patterns[pattern_index].clone();
        self.apply_pattern_to_config(&pattern)
    }

    /// Aplicar patrón a configuración
    fn apply_pattern_to_config(&mut self, pattern: &UserPattern) -> Result<(), String<64>> {
        let config = match pattern.pattern_type {
            PatternType::WindowUsage => AdaptiveConfig {
                config_id: self.generate_config_id(),
                component: str_to_heapless("window_manager"),
                parameter: str_to_heapless("default_behavior"),
                current_value: str_to_heapless("standard"),
                optimal_value: str_to_heapless("user_preferred"),
                confidence: pattern.success_rate,
                last_updated: pattern.timestamp,
            },
            PatternType::AppletInteraction => AdaptiveConfig {
                config_id: self.generate_config_id(),
                component: str_to_heapless("applet_system"),
                parameter: str_to_heapless("auto_arrange"),
                current_value: str_to_heapless("false"),
                optimal_value: str_to_heapless("true"),
                confidence: pattern.success_rate,
                last_updated: pattern.timestamp,
            },
            PatternType::NotificationPreference => AdaptiveConfig {
                config_id: self.generate_config_id(),
                component: str_to_heapless("notification_system"),
                parameter: str_to_heapless("auto_dismiss_delay"),
                current_value: str_to_heapless("5000"),
                optimal_value: str_to_heapless("3000"),
                confidence: pattern.success_rate,
                last_updated: pattern.timestamp,
            },
            _ => return Ok(()), // No aplicar para otros tipos
        };

        // Agregar o actualizar configuración
        if let Some(existing_index) =
            self.find_config_by_component(&config.component, &config.parameter)
        {
            self.adaptive_configs[existing_index] = config;
        } else {
            if self.adaptive_configs.len() >= self.adaptive_configs.capacity() {
                self.remove_oldest_config()?;
            }
            self.adaptive_configs
                .push(config)
                .map_err(|_| str_to_heapless("No se pudo agregar configuración"))?;
        }

        Ok(())
    }

    /// Predecir acción del usuario basándose en patrones aprendidos
    pub fn predict_user_action(&self, context: &str) -> Option<PredictedAction> {
        // Buscar patrones similares
        let similar_patterns: Vec<&UserPattern, 10> = self
            .user_patterns
            .iter()
            .filter(|p| self.is_context_similar(context, &p.context))
            .collect();

        if similar_patterns.is_empty() {
            return None;
        }

        // Calcular predicción basada en patrones similares
        let best_pattern = similar_patterns.iter().max_by(|a, b| {
            let score_a = a.weight * a.success_rate;
            let score_b = b.weight * b.success_rate;
            score_a.partial_cmp(&score_b).unwrap_or(Ordering::Equal)
        })?;

        Some(PredictedAction {
            action: self.extract_action_from_pattern(best_pattern),
            confidence: best_pattern.weight * best_pattern.success_rate,
            reasoning: str_to_heapless("Basado en patrón: user_pattern"),
        })
    }

    /// Obtener recomendaciones para mejorar la experiencia
    pub fn get_recommendations(&self) -> Vec<Recommendation, 10> {
        let mut recommendations = Vec::new();

        // Recomendaciones basadas en patrones frecuentes
        for pattern in &self.user_patterns {
            if pattern.frequency > 5 && pattern.success_rate > 0.8 {
                let recommendation = Recommendation {
                    recommendation_id: self.generate_recommendation_id(),
                    category: pattern.pattern_type.clone(),
                    description: self.generate_recommendation_description(pattern),
                    priority: pattern.weight,
                    action: self.generate_recommendation_action(pattern),
                };
                let _ = recommendations.push(recommendation);
            }
        }

        // Recomendaciones basadas en preferencias
        for preference in &self.learned_preferences {
            if preference.confidence > 0.8 && preference.usage_count > 3 {
                let recommendation = Recommendation {
                    recommendation_id: self.generate_recommendation_id(),
                    category: self.preference_to_pattern_type(&preference.category),
                    description: str_to_heapless("Aplicar preferencia: user_pref"),
                    priority: preference.confidence,
                    action: str_to_heapless("configure_user_preference"),
                };
                let _ = recommendations.push(recommendation);
            }
        }

        recommendations
    }

    /// Renderizar información del sistema de aprendizaje
    pub fn render_learning_info(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
    ) -> Result<(), String<64>> {
        let mut y_offset = y;

        // Título
        fb.write_text_kernel("=== AI LEARNING SYSTEM ===", Color::CYAN);
        y_offset += 25;

        // Métricas de rendimiento
        let metrics_text = "Interacciones: 0 | Éxito: 0 | Fallos: 0";
        fb.write_text_kernel(metrics_text, Color::WHITE);
        y_offset += 20;

        // Satisfacción del usuario
        let satisfaction_text = "Satisfacción: 0.0%";
        fb.write_text_kernel(satisfaction_text, Color::GREEN);
        y_offset += 20;

        // Eficiencia del sistema
        let efficiency_text = "Eficiencia: 0.0%";
        fb.write_text_kernel(efficiency_text, Color::YELLOW);
        y_offset += 20;

        // Patrones aprendidos
        let patterns_text = "Patrones: 0 | Preferencias: 0";
        fb.write_text_kernel(patterns_text, Color::MAGENTA);
        y_offset += 20;

        // Configuraciones adaptativas
        let configs_text = "Configuraciones adaptativas: 0";
        fb.write_text_kernel(configs_text, Color::CYAN);
        y_offset += 20;

        // Nivel de confianza
        let confidence_text = "Confianza: 0.0%";
        fb.write_text_kernel(confidence_text, Color::WHITE);

        Ok(())
    }

    // Métodos auxiliares
    fn find_similar_pattern(&self, pattern: &UserPattern) -> Option<usize> {
        self.user_patterns.iter().position(|p| {
            p.pattern_type == pattern.pattern_type
                && self.is_context_similar(&p.context, &pattern.context)
        })
    }

    fn find_similar_preference(&self, preference: &LearnedPreference) -> Option<usize> {
        self.learned_preferences
            .iter()
            .position(|p| p.category == preference.category && p.value == preference.value)
    }

    fn find_config_by_component(&self, component: &str, parameter: &str) -> Option<usize> {
        self.adaptive_configs
            .iter()
            .position(|c| c.component == component && c.parameter == parameter)
    }

    fn is_context_similar(&self, context1: &str, context2: &str) -> bool {
        // Implementación simple de similitud de contexto
        let words1: Vec<&str, 20> = context1.split_whitespace().collect();
        let words2: Vec<&str, 20> = context2.split_whitespace().collect();

        let common_words = words1.iter().filter(|w| words2.contains(w)).count();
        let total_words = words1.len().max(words2.len());

        if total_words == 0 {
            return false;
        }

        (common_words as f32 / total_words as f32) > 0.5
    }

    fn calculate_pattern_weight(&self, pattern: &UserPattern) -> f32 {
        let frequency_weight = (pattern.frequency as f32).min(10.0) / 10.0;
        let recency_weight = 1.0; // En una implementación real, calcular basándose en timestamp
        let success_weight = pattern.success_rate;

        (frequency_weight + recency_weight + success_weight) / 3.0
    }

    /// Calcular peso de un patrón (versión estática)
    fn calculate_pattern_weight_static(&self, pattern: &UserPattern) -> f32 {
        let frequency_weight = (pattern.frequency as f32).min(10.0) / 10.0;
        let recency_weight = 1.0; // En una implementación real, calcular basándose en timestamp
        let success_weight = pattern.success_rate;

        (frequency_weight + recency_weight + success_weight) / 3.0
    }

    fn remove_least_relevant_pattern(&mut self) -> Result<(), String<64>> {
        if let Some(index) = self
            .user_patterns
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.weight.partial_cmp(&b.weight).unwrap_or(Ordering::Equal))
            .map(|(i, _)| i)
        {
            self.user_patterns.swap_remove(index);
        }
        Ok(())
    }

    fn remove_least_used_preference(&mut self) -> Result<(), String<64>> {
        if let Some(index) = self
            .learned_preferences
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.usage_count.cmp(&b.usage_count))
            .map(|(i, _)| i)
        {
            self.learned_preferences.swap_remove(index);
        }
        Ok(())
    }

    fn remove_oldest_config(&mut self) -> Result<(), String<64>> {
        if let Some(index) = self
            .adaptive_configs
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.last_updated.cmp(&b.last_updated))
            .map(|(i, _)| i)
        {
            self.adaptive_configs.swap_remove(index);
        }
        Ok(())
    }

    fn extract_action_from_pattern(&self, pattern: &UserPattern) -> String<64> {
        match pattern.pattern_type {
            PatternType::WindowUsage => str_to_heapless("optimize_window_management"),
            PatternType::AppletInteraction => str_to_heapless("adjust_applet_layout"),
            PatternType::NotificationPreference => str_to_heapless("customize_notifications"),
            PatternType::PortalUsage => str_to_heapless("optimize_portal_access"),
            PatternType::VisualPreference => str_to_heapless("adjust_visual_settings"),
            PatternType::Navigation => str_to_heapless("improve_navigation"),
        }
    }

    fn generate_recommendation_description(&self, pattern: &UserPattern) -> String<64> {
        match pattern.pattern_type {
            PatternType::WindowUsage => {
                str_to_heapless("Optimizar gestión de ventanas basándose en tu uso frecuente")
            }
            PatternType::AppletInteraction => {
                str_to_heapless("Ajustar disposición de applets para mejor acceso")
            }
            PatternType::NotificationPreference => {
                str_to_heapless("Personalizar configuración de notificaciones")
            }
            PatternType::PortalUsage => {
                str_to_heapless("Mejorar acceso a funciones del portal de escritorio")
            }
            PatternType::VisualPreference => {
                str_to_heapless("Ajustar configuración visual según tus preferencias")
            }
            PatternType::Navigation => str_to_heapless("Mejorar sistema de navegación"),
        }
    }

    fn generate_recommendation_action(&self, pattern: &UserPattern) -> String<64> {
        match pattern.pattern_type {
            PatternType::WindowUsage => str_to_heapless("apply_window_optimization"),
            PatternType::AppletInteraction => str_to_heapless("rearrange_applets"),
            PatternType::NotificationPreference => {
                str_to_heapless("customize_notification_settings")
            }
            PatternType::PortalUsage => str_to_heapless("optimize_portal_interface"),
            PatternType::VisualPreference => str_to_heapless("adjust_theme_settings"),
            PatternType::Navigation => str_to_heapless("improve_navigation_shortcuts"),
        }
    }

    fn preference_to_pattern_type(&self, category: &PreferenceCategory) -> PatternType {
        match category {
            PreferenceCategory::WindowManagement => PatternType::WindowUsage,
            PreferenceCategory::AppletConfiguration => PatternType::AppletInteraction,
            PreferenceCategory::NotificationSettings => PatternType::NotificationPreference,
            PreferenceCategory::RenderingOptions => PatternType::VisualPreference,
            PreferenceCategory::PortalConfiguration => PatternType::PortalUsage,
        }
    }

    fn generate_pattern_id(&self) -> u32 {
        // En una implementación real, usar un generador de IDs único
        self.user_patterns.len() as u32 + 1
    }

    fn generate_preference_id(&self) -> u32 {
        self.learned_preferences.len() as u32 + 1
    }

    fn generate_config_id(&self) -> u32 {
        self.adaptive_configs.len() as u32 + 1
    }

    fn generate_recommendation_id(&self) -> u32 {
        // ID único para recomendaciones
        1000 + self.user_patterns.len() as u32
    }
}

/// Interacción del usuario para aprendizaje
#[derive(Clone, Debug)]
pub struct UserInteraction {
    pub interaction_id: u32,
    pub action_type: ActionType,
    pub context: String<64>,
    pub success: bool,
    pub feedback: Option<UserFeedback>,
    pub timestamp: u64,
    pub duration_ms: u32,
}

/// Tipo de acción del usuario
#[derive(Clone, Debug, PartialEq)]
pub enum ActionType {
    WindowOperation,
    AppletInteraction,
    NotificationAction,
    PortalRequest,
    VisualChange,
    Navigation,
}

/// Acción predicha por la IA
#[derive(Clone, Debug)]
pub struct PredictedAction {
    pub action: String<64>,
    pub confidence: f32,
    pub reasoning: String<64>,
}

/// Recomendación del sistema
#[derive(Clone, Debug)]
pub struct Recommendation {
    pub recommendation_id: u32,
    pub category: PatternType,
    pub description: String<64>,
    pub priority: f32,
    pub action: String<64>,
}

impl LearnedPreference {
    fn parameter(&self) -> String<64> {
        match self.category {
            PreferenceCategory::WindowManagement => str_to_heapless("window_behavior"),
            PreferenceCategory::AppletConfiguration => str_to_heapless("applet_layout"),
            PreferenceCategory::NotificationSettings => str_to_heapless("notification_timing"),
            PreferenceCategory::RenderingOptions => str_to_heapless("visual_theme"),
            PreferenceCategory::PortalConfiguration => str_to_heapless("portal_access"),
        }
    }
}
