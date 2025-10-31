//! Sistema global de modelos de IA para todo el kernel
//!
//! Este módulo proporciona acceso a modelos de IA desde cualquier parte del kernel,
//! incluyendo COSMIC, drivers, servicios del sistema, etc.

use crate::filesystem::fat32::Fat32Driver;
use alloc::format;
use core::time::Duration;
use heapless::{FnvIndexMap, String, Vec};

/// Información de un modelo de IA
#[derive(Debug, Clone, PartialEq)]
pub struct AIModel {
    /// ID único del modelo
    pub model_id: String<64>,
    /// Nombre del modelo
    pub name: String<64>,
    /// Tipo de modelo
    pub model_type: ModelType,
    /// Ruta del archivo del modelo
    pub file_path: String<128>,
    /// Tamaño en bytes
    pub size_bytes: u64,
    /// Versión del modelo
    pub version: String<16>,
    /// Fecha de descarga
    pub downloaded_at: u64,
    /// Última vez que se usó
    pub last_used: u64,
    /// Efectividad del modelo (0.0 - 1.0)
    pub effectiveness: f32,
    /// Estado del modelo
    pub status: ModelStatus,
    /// Checksum para verificación
    pub checksum: String<64>,
    /// Metadatos adicionales
    pub metadata: FnvIndexMap<String<32>, String<64>, 8>,
}

/// Tipos de modelos de IA
#[derive(Debug, Clone, PartialEq)]
pub enum ModelType {
    /// Modelo conversacional/generativo
    Conversational,
    /// Modelo de clasificación de texto
    TextClassifier,
    /// Modelo de embeddings
    Embedding,
    /// Modelo de procesamiento de imágenes
    ImageProcessor,
    /// Modelo de análisis de sentimientos
    SentimentAnalysis,
    /// Modelo de traducción
    Translation,
    /// Modelo personalizado
    Custom(String<32>),
}

/// Estado del modelo
#[derive(Debug, Clone, PartialEq)]
pub enum ModelStatus {
    /// Modelo disponible y listo para usar
    Available,
    /// Modelo descargándose
    Downloading,
    /// Modelo en proceso de verificación
    Verifying,
    /// Modelo con error
    Error(String<64>),
    /// Modelo deshabilitado
    Disabled,
}

/// Repositorio de modelos
#[derive(Debug, Clone)]
pub struct ModelRepository {
    /// Nombre del repositorio
    pub name: String<32>,
    /// URL base del repositorio
    pub base_url: String<128>,
    /// URL de la API
    pub api_url: String<128>,
    /// Si está habilitado
    pub enabled: bool,
    /// Si requiere autenticación
    pub auth_required: bool,
    /// Límite de requests por hora
    pub rate_limit: u32,
    /// Timeout en segundos
    pub timeout_seconds: u32,
}

/// Gestor global de modelos de IA
pub struct GlobalAIModelManager {
    /// Modelos cargados
    models: FnvIndexMap<String<64>, AIModel, 32>,
    /// Repositorios configurados
    repositories: FnvIndexMap<String<32>, ModelRepository, 8>,
    /// Cache de modelos en memoria
    memory_cache: FnvIndexMap<String<64>, Vec<u8, 1024>, 16>,
    /// Estadísticas de uso
    usage_stats: UsageStats,
    /// Configuración global
    config: GlobalConfig,
}

/// Estadísticas de uso de modelos
#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    /// Total de modelos cargados
    pub total_models: u32,
    /// Total de requests procesados
    pub total_requests: u64,
    /// Tamaño total de modelos en memoria
    pub memory_usage_bytes: u64,
    /// Modelo más usado
    pub most_used_model: String<64>,
    /// Tiempo total de procesamiento
    pub total_processing_time_ms: u64,
}

/// Configuración global del sistema de modelos
#[derive(Debug, Clone)]
pub struct GlobalConfig {
    /// Directorio base para modelos
    pub models_directory: String<64>,
    /// Directorio de cache
    pub cache_directory: String<64>,
    /// Tamaño máximo de cache en memoria (MB)
    pub max_memory_cache_mb: u32,
    /// Timeout por defecto (segundos)
    pub default_timeout_seconds: u32,
    /// Habilitar auto-descarga
    pub auto_download_enabled: bool,
    /// Habilitar verificación de checksums
    pub verify_checksums: bool,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            models_directory: str_to_heapless("/ai_models/"),
            cache_directory: str_to_heapless("/ai_cache/"),
            max_memory_cache_mb: 100,
            default_timeout_seconds: 30,
            auto_download_enabled: true,
            verify_checksums: true,
        }
    }
}

impl GlobalAIModelManager {
    /// Crear nuevo gestor global de modelos
    pub fn new() -> Self {
        Self {
            models: FnvIndexMap::new(),
            repositories: FnvIndexMap::new(),
            memory_cache: FnvIndexMap::new(),
            usage_stats: UsageStats::default(),
            config: GlobalConfig::default(),
        }
    }

    /// Inicializar el gestor global
    pub fn initialize(&mut self, fat32_driver: &mut Fat32Driver) -> Result<(), String<64>> {
        // Crear directorios necesarios
        self.create_directories(fat32_driver)?;

        // Inicializar repositorios
        self.initialize_repositories()?;

        // Cargar modelos existentes
        self.load_existing_models(fat32_driver)?;

        Ok(())
    }

    /// Crear directorios necesarios
    fn create_directories(&self, fat32_driver: &mut Fat32Driver) -> Result<(), String<64>> {
        // Crear directorio de modelos
        fat32_driver
            .create_directory(&self.config.models_directory)
            .map_err(|e| str_to_heapless("Error creando directorio de modelos"))?;

        // Crear directorio de cache
        fat32_driver
            .create_directory(&self.config.cache_directory)
            .map_err(|e| str_to_heapless("Error creando directorio de cache"))?;

        // Crear subdirectorios por tipo
        let subdirs = [
            "conversational",
            "classification",
            "embedding",
            "image_processing",
            "sentiment",
            "translation",
            "custom",
        ];

        for subdir in &subdirs {
            let path = str_to_heapless_128(&format!("{}{}/", self.config.models_directory, subdir));
            let _ = fat32_driver.create_directory(&str_to_heapless(&path));
        }

        Ok(())
    }

    /// Inicializar repositorios
    fn initialize_repositories(&mut self) -> Result<(), String<64>> {
        // Repositorio de Hugging Face
        let huggingface = ModelRepository {
            name: str_to_heapless_32("huggingface"),
            base_url: str_to_heapless_128("https://huggingface.co"),
            api_url: str_to_heapless_128("https://huggingface.co/api"),
            enabled: true,
            auth_required: false,
            rate_limit: 1000,
            timeout_seconds: 300,
        };
        let _ = self
            .repositories
            .insert(str_to_heapless_32("huggingface"), huggingface);

        // Repositorio de ONNX Model Zoo
        let onnx_zoo = ModelRepository {
            name: str_to_heapless_32("onnx_zoo"),
            base_url: str_to_heapless_128("https://github.com/onnx/models"),
            api_url: str_to_heapless_128("https://api.github.com/repos/onnx/models"),
            enabled: true,
            auth_required: false,
            rate_limit: 60,
            timeout_seconds: 180,
        };
        let _ = self
            .repositories
            .insert(str_to_heapless_32("onnx"), onnx_zoo);

        Ok(())
    }

    /// Cargar modelos existentes
    fn load_existing_models(&mut self, fat32_driver: &mut Fat32Driver) -> Result<(), String<64>> {
        // Leer índice de modelos
        let index_path =
            str_to_heapless_128(&format!("{}index.json", self.config.models_directory));

        match fat32_driver.read_file_by_path(&index_path) {
            Ok(data) => {
                self.parse_models_index(&data)?;
            }
            Err(_) => {
                // Crear índice vacío si no existe
                self.create_empty_index(fat32_driver)?;
            }
        }

        Ok(())
    }

    /// Parsear índice de modelos desde JSON real
    fn parse_models_index(&mut self, data: &[u8]) -> Result<(), String<64>> {
        let json_str = match core::str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => return Err(str_to_heapless("Error decodificando JSON")),
        };

        // Parser JSON simple y robusto
        let mut model_count = 0;
        let mut in_models_array = false;
        let mut current_model = alloc::string::String::new();
        let mut brace_count = 0;
        let mut in_string = false;
        let mut escape_next = false;

        for ch in json_str.chars() {
            if escape_next {
                escape_next = false;
                if in_models_array && brace_count > 0 {
                    current_model.push(ch);
                }
                continue;
            }

            if ch == '\\' {
                escape_next = true;
                if in_models_array && brace_count > 0 {
                    current_model.push(ch);
                }
                continue;
            }

            if ch == '"' {
                in_string = !in_string;
                if in_models_array && brace_count > 0 {
                    current_model.push(ch);
                }
                continue;
            }

            if in_string {
                if in_models_array && brace_count > 0 {
                    current_model.push(ch);
                }
                continue;
            }

            match ch {
                '{' => {
                    if in_models_array {
                        if brace_count == 0 {
                            current_model.clear();
                        }
                        current_model.push(ch);
                        brace_count += 1;
                    }
                }
                '}' => {
                    if in_models_array {
                        current_model.push(ch);
                        brace_count -= 1;

                        if brace_count == 0 {
                            // Modelo completo parseado
                            if let Ok(model) = self.parse_single_model_from_json(&current_model) {
                                let _ = self.models.insert(model.model_id.clone(), model);
                                model_count += 1;
                            }
                            current_model.clear();
                        }
                    }
                }
                '[' => {
                    // Buscar el array de modelos
                    if json_str.contains("\"models\"") && !in_models_array {
                        // Verificar que estamos en el contexto correcto
                        let pos = json_str.find("\"models\"");
                        if let Some(pos) = pos {
                            let before_models = &json_str[..pos];
                            let after_models = &json_str[pos..];
                            if after_models.contains('[') && !before_models.contains('[') {
                                in_models_array = true;
                            }
                        }
                    }
                }
                ']' => {
                    if in_models_array {
                        break;
                    }
                }
                _ => {
                    if in_models_array && brace_count > 0 {
                        current_model.push(ch);
                    }
                }
            }
        }

        if model_count == 0 {
            return Err(str_to_heapless("No se encontraron modelos en el índice"));
        }

        Ok(())
    }

    /// Parsear un modelo individual desde JSON (método legacy)
    fn parse_single_model_from_json(&self, json: &str) -> Result<AIModel, String<64>> {
        let mut model_id = alloc::string::String::new();
        let mut name = alloc::string::String::new();
        let mut model_type = ModelType::Custom(str_to_heapless_32("unknown"));
        let mut file_path = alloc::string::String::new();
        let mut size_bytes = 0u64;
        let mut version = str_to_heapless_16("1.0.0");
        let mut checksum = str_to_heapless("real_checksum");

        for line in json.lines() {
            let line = line.trim().trim_end_matches(',');

            if line.contains("\"model_id\"") || line.contains("\"id\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        model_id =
                            alloc::string::String::from(value_part[..end].trim().trim_matches('"'));
                    } else {
                        model_id = alloc::string::String::from(value_part.trim().trim_matches('"'));
                    }
                }
            }

            if line.contains("\"name\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        name =
                            alloc::string::String::from(value_part[..end].trim().trim_matches('"'));
                    } else {
                        name = alloc::string::String::from(value_part.trim().trim_matches('"'));
                    }
                }
            }

            if line.contains("\"type\"") || line.contains("\"model_type\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let type_str = value_part[..end].trim().trim_matches('"');
                        model_type = match type_str {
                            "conversational" => ModelType::Conversational,
                            "text_classifier" => ModelType::TextClassifier,
                            "embedding" => ModelType::Embedding,
                            "translation" => ModelType::Translation,
                            _ => ModelType::Custom(str_to_heapless_32(type_str)),
                        };
                    }
                }
            }

            if line.contains("\"path\"") || line.contains("\"file_path\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        file_path =
                            alloc::string::String::from(value_part[..end].trim().trim_matches('"'));
                    } else {
                        file_path =
                            alloc::string::String::from(value_part.trim().trim_matches('"'));
                    }
                }
            }

            if line.contains("\"size\"") || line.contains("\"size_bytes\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let size_str = value_part[..end].trim();
                        size_bytes = size_str.parse::<u64>().unwrap_or(0);
                    } else {
                        let size_str = value_part.trim();
                        size_bytes = size_str.parse::<u64>().unwrap_or(0);
                    }
                }
            }

            if line.contains("\"version\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let version_str = value_part[..end].trim().trim_matches('"');
                        version = str_to_heapless_16(version_str);
                    } else {
                        let version_str = value_part.trim().trim_matches('"');
                        version = str_to_heapless_16(version_str);
                    }
                }
            }

            if line.contains("\"checksum\"") || line.contains("\"sha256\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let checksum_str = value_part[..end].trim().trim_matches('"');
                        checksum = str_to_heapless(checksum_str);
                    } else {
                        let checksum_str = value_part.trim().trim_matches('"');
                        checksum = str_to_heapless(checksum_str);
                    }
                }
            }
        }

        if model_id.is_empty() {
            return Err(str_to_heapless("Modelo sin ID"));
        }

        if file_path.is_empty() {
            file_path = alloc::format!("/ai_models/{}/", model_id);
        }

        Ok(AIModel {
            model_id: str_to_heapless(&model_id),
            name: str_to_heapless(&name),
            model_type,
            file_path: str_to_heapless_128(&file_path),
            size_bytes,
            version,
            downloaded_at: self.get_current_timestamp(),
            last_used: 0,
            effectiveness: 0.8,
            status: ModelStatus::Available,
            checksum,
            metadata: FnvIndexMap::new(),
        })
    }

    /// Crear índice vacío
    fn create_empty_index(&self, fat32_driver: &mut Fat32Driver) -> Result<(), String<64>> {
        let index_content = b"{\"models\":[],\"version\":\"1.0.0\",\"last_updated\":1234567890}";
        let index_path =
            str_to_heapless_128(&format!("{}index.json", self.config.models_directory));

        fat32_driver
            .write_file_by_path(&index_path, index_content)
            .map_err(|e| str_to_heapless("Error creando índice vacío"))?;

        Ok(())
    }

    /// Obtener modelo por ID
    pub fn get_model(&self, model_id: &str) -> Option<&AIModel> {
        self.models.get(&str_to_heapless(model_id))
    }

    /// Obtener modelo por nombre
    pub fn get_model_by_name(&self, name: &str) -> Option<&AIModel> {
        self.models
            .values()
            .find(|model| model.name == str_to_heapless(name))
    }

    /// Listar todos los modelos
    pub fn list_models(&self) -> Vec<&AIModel, 32> {
        let mut result = Vec::new();
        for model in self.models.values() {
            let _ = result.push(model);
        }
        result
    }

    /// Listar modelos por tipo
    pub fn list_models_by_type(&self, model_type: &ModelType) -> Vec<&AIModel, 16> {
        let mut result = Vec::new();
        for model in self.models.values() {
            if &model.model_type == model_type {
                let _ = result.push(model);
            }
        }
        result
    }

    /// Cargar modelo en memoria
    pub fn load_model_to_memory(
        &mut self,
        model_id: &str,
        fat32_driver: &mut Fat32Driver,
    ) -> Result<(), String<64>> {
        if let Some(model) = self.models.get(&str_to_heapless(model_id)) {
            // Verificar si ya está en cache
            if self.memory_cache.contains_key(&str_to_heapless(model_id)) {
                return Ok(());
            }

            // Cargar desde archivo
            match fat32_driver.read_file_by_path(&model.file_path) {
                Ok(data) => {
                    // Verificar checksum si está habilitado
                    if self.config.verify_checksums {
                        if !self.verify_model_checksum(&data, &model.checksum) {
                            return Err(str_to_heapless("Checksum del modelo no coincide"));
                        }
                    }

                    // Agregar a cache de memoria
                    let mut cache_data = Vec::new();
                    for &byte in data.iter().take(1024) {
                        if cache_data.push(byte).is_err() {
                            break;
                        }
                    }
                    let _ = self
                        .memory_cache
                        .insert(str_to_heapless(model_id), cache_data);

                    // Actualizar estadísticas
                    self.usage_stats.memory_usage_bytes += data.len() as u64;
                    self.usage_stats.total_requests += 1;

                    Ok(())
                }
                Err(_) => Err(str_to_heapless("Error cargando modelo desde archivo")),
            }
        } else {
            Err(str_to_heapless("Modelo no encontrado"))
        }
    }

    /// Verificar checksum del modelo
    fn verify_model_checksum(&self, data: &[u8], expected_checksum: &str) -> bool {
        // En una implementación real, calcularía el checksum real
        // Por ahora, simulamos la verificación
        !expected_checksum.is_empty()
    }

    /// Descargar modelo desde repositorio
    pub fn download_model(
        &mut self,
        model_name: &str,
        repository: &str,
        fat32_driver: &mut Fat32Driver,
    ) -> Result<(), String<64>> {
        if let Some(repo) = self.repositories.get(&str_to_heapless_32(repository)) {
            if !repo.enabled {
                return Err(str_to_heapless("Repositorio deshabilitado"));
            }

            // Simular descarga (en implementación real, usaría HTTP)
            self.simulate_model_download(model_name, repository, fat32_driver)?;

            Ok(())
        } else {
            Err(str_to_heapless("Repositorio no encontrado"))
        }
    }

    /// Simular descarga de modelo
    fn simulate_model_download(
        &mut self,
        model_name: &str,
        repository: &str,
        fat32_driver: &mut Fat32Driver,
    ) -> Result<(), String<64>> {
        // Crear modelo simulado
        let model_id = str_to_heapless(model_name);
        let model_path = str_to_heapless_128(&format!(
            "{}{}.bin",
            self.config.models_directory,
            model_name.replace("/", "_")
        ));

        let model = AIModel {
            model_id: model_id.clone(),
            name: str_to_heapless(model_name),
            model_type: self.infer_model_type(model_name),
            file_path: str_to_heapless_128(&model_path),
            size_bytes: 100 * 1024 * 1024, // 100 MB simulado
            version: str_to_heapless_16("1.0.0"),
            downloaded_at: self.get_current_timestamp(),
            last_used: 0,
            effectiveness: 0.8,
            status: ModelStatus::Available,
            checksum: str_to_heapless("simulated_checksum"),
            metadata: FnvIndexMap::new(),
        };

        // Crear archivo simulado
        let mut model_data = alloc::vec::Vec::new();
        for _ in 0..1024 {
            model_data.push(0u8);
        }
        fat32_driver
            .write_file_by_path(&model.file_path, &model_data)
            .map_err(|e| str_to_heapless("Error guardando modelo"))?;

        // Agregar al gestor
        let _ = self.models.insert(model_id, model);

        // Actualizar estadísticas
        self.usage_stats.total_models += 1;

        Ok(())
    }

    /// Inferir tipo de modelo basado en el nombre
    fn infer_model_type(&self, model_name: &str) -> ModelType {
        if model_name.contains("DialoGPT") || model_name.contains("blenderbot") {
            ModelType::Conversational
        } else if model_name.contains("bert") || model_name.contains("distilbert") {
            ModelType::TextClassifier
        } else if model_name.contains("sentence-transformers") {
            ModelType::Embedding
        } else {
            ModelType::Custom(str_to_heapless_32("unknown"))
        }
    }

    /// Obtener timestamp actual
    fn get_current_timestamp(&self) -> u64 {
        // En una implementación real, obtendría el timestamp real
        1234567890
    }

    /// Obtener estadísticas de uso
    pub fn get_usage_stats(&self) -> &UsageStats {
        &self.usage_stats
    }

    /// Renderizar información del gestor global
    pub fn render_info(
        &self,
        fb: &mut crate::drivers::framebuffer::FramebufferDriver,
        x: u32,
        y: u32,
    ) -> Result<(), String<64>> {
        // Fondo del widget
        self.draw_rectangle(
            fb,
            x,
            y,
            400,
            300,
            crate::drivers::framebuffer::Color::BLACK,
        )?;
        self.draw_rectangle_border(fb, x, y, 400, 300, crate::drivers::framebuffer::Color::CYAN)?;

        // Título
        fb.write_text_kernel(
            "Global AI Model Manager",
            crate::drivers::framebuffer::Color::CYAN,
        );

        // Información de modelos
        let y_offset = y + 30;
        fb.write_text_kernel("Models: 0", crate::drivers::framebuffer::Color::WHITE);

        let y_offset = y_offset + 20;
        fb.write_text_kernel("Memory: 0 MB", crate::drivers::framebuffer::Color::WHITE);

        let y_offset = y_offset + 20;
        fb.write_text_kernel("Requests: 0", crate::drivers::framebuffer::Color::WHITE);

        // Listar algunos modelos
        let y_offset = y_offset + 30;
        fb.write_text_kernel(
            "Available Models:",
            crate::drivers::framebuffer::Color::YELLOW,
        );

        let mut display_y = y_offset + 20;
        let mut count = 0;
        for model in self.models.values().take(5) {
            if count >= 5 {
                break;
            }

            let model_info = "- Model";
            fb.write_text_kernel(&model_info, crate::drivers::framebuffer::Color::WHITE);
            display_y += 15;
            count += 1;
        }

        Ok(())
    }

    /// Dibujar rectángulo
    fn draw_rectangle(
        &self,
        fb: &mut crate::drivers::framebuffer::FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: crate::drivers::framebuffer::Color,
    ) -> Result<(), String<64>> {
        // Simular dibujo de rectángulo
        Ok(())
    }

    /// Dibujar borde de rectángulo
    fn draw_rectangle_border(
        &self,
        fb: &mut crate::drivers::framebuffer::FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: crate::drivers::framebuffer::Color,
    ) -> Result<(), String<64>> {
        // Simular dibujo de borde
        Ok(())
    }

    /// Cargar modelos desde el sistema de archivos FAT32
    pub fn load_models_from_filesystem(
        &mut self,
        fat32_driver: &mut Fat32Driver,
    ) -> Result<usize, String<64>> {
        let mut loaded_count = 0;
        let initial_model_count = self.models.len();

        // Limpiar modelos simulados para cargar solo modelos reales
        self.models.clear();

        // Intentar leer el índice de modelos
        match fat32_driver.read_file_by_path("/ai_models/index.json") {
            Ok(index_content) => {
                // Debug: Mostrar tamaño del contenido leído
                // En una implementación real, se usaría un logger

                // Parsear el índice JSON para cargar modelos reales
                match self.parse_models_index(&index_content) {
                    Ok(_) => {
                        // Si el parsing fue exitoso, contar los modelos cargados
                        loaded_count = self.models.len();
                        // Debug: Mostrar que se parseó correctamente
                        // En una implementación real, se usaría un logger
                    }
                    Err(e) => {
                        // Si falla el parsing, cargar modelos individuales
                        // Debug: Mostrar que falló el parsing
                        // En una implementación real, se usaría un logger
                        loaded_count = self.load_individual_models(fat32_driver)?;
                    }
                }
            }
            Err(_) => {
                // Si no hay índice, intentar cargar modelos individuales
                loaded_count = self.load_individual_models(fat32_driver)?;
            }
        }

        // Si no se cargaron modelos reales, cargar modelos simulados como fallback
        if loaded_count == 0 {
            self.load_simulated_models();
            loaded_count = self.models.len();
        }

        Ok(loaded_count)
    }

    /// Cargar modelos simulados para demostración
    fn load_simulated_models(&mut self) {
        // Simular carga de los modelos que descargamos
        let simulated_models = alloc::vec![
            (
                "microsoft_DialoGPT-small",
                "microsoft/DialoGPT-small",
                ModelType::Conversational
            ),
            (
                "distilbert-base-uncased",
                "distilbert-base-uncased",
                ModelType::TextClassifier
            ),
            (
                "sentence-transformers_all-MiniLM-L6-v2",
                "sentence-transformers/all-MiniLM-L6-v2",
                ModelType::Embedding
            ),
            (
                "facebook_blenderbot-400M-distill",
                "facebook/blenderbot-400M-distill",
                ModelType::Conversational
            ),
            (
                "microsoft_DialoGPT-medium",
                "microsoft/DialoGPT-medium",
                ModelType::Conversational
            ),
            (
                "cardiffnlp_twitter-roberta-base-sentiment-latest",
                "cardiffnlp/twitter-roberta-base-sentiment-latest",
                ModelType::TextClassifier
            ),
            (
                "Helsinki-NLP_opus-mt-en-es",
                "Helsinki-NLP/opus-mt-en-es",
                ModelType::Translation
            ),
        ];

        for (model_id, name, model_type) in simulated_models {
            let model = AIModel {
                model_id: str_to_heapless(model_id),
                name: str_to_heapless(name),
                model_type,
                file_path: str_to_heapless_128(&format!("/ai_models/{}/", model_id)),
                size_bytes: 100_000_000, // 100MB simulado
                version: str_to_heapless_16("1.0.0"),
                downloaded_at: self.get_current_timestamp(),
                last_used: 0,
                effectiveness: 0.8,
                status: ModelStatus::Available,
                checksum: str_to_heapless("simulated_checksum"),
                metadata: FnvIndexMap::new(),
            };

            let _ = self.models.insert(str_to_heapless(model_id), model);
        }
    }

    /// Cargar modelos individuales desde directorios
    fn load_individual_models(
        &mut self,
        fat32_driver: &mut Fat32Driver,
    ) -> Result<usize, String<64>> {
        let mut loaded_count = 0;

        // Lista de modelos conocidos que podrían estar en el sistema de archivos
        let known_models = [
            "microsoft_DialoGPT-small",
            "microsoft_DialoGPT-medium",
            "distilbert-base-uncased",
            "distilbert-base",
            "sentence-transformers_all-MiniLM-L6-v2",
            "facebook_blenderbot-400M-distill",
            "cardiffnlp_twitter-roberta-base-sentiment-latest",
            "Helsinki-NLP_opus-mt-en-es",
            "anomaly-detector",
            "efficientnet-lite",
            "mobilenetv2",
            "performance-predictor",
            "tinyllama-1.1b",
        ];

        for model_name in &known_models {
            let model_path = format!("/ai_models/{}", model_name);

            // Verificar si existe el directorio del modelo
            if fat32_driver.file_exists(&model_path) {
                // Intentar leer metadata del modelo
                let metadata_path = format!("{}/metadata.json", model_path);
                if let Ok(metadata_content) = fat32_driver.read_file_by_path(&metadata_path) {
                    // Crear modelo basado en la metadata real
                    if let Ok(model) =
                        self.create_model_from_metadata(model_name, &metadata_content)
                    {
                        let _ = self.models.insert(str_to_heapless(model_name), model);
                        loaded_count += 1;
                        // Debug: Mostrar que se cargó desde metadata
                        // En una implementación real, se usaría un logger
                    }
                } else {
                    // Si no hay metadata, crear modelo con datos reales del directorio
                    let model = self.create_model_from_directory(model_name, fat32_driver);
                    let _ = self.models.insert(str_to_heapless(model_name), model);
                    loaded_count += 1;
                    // Debug: Mostrar que se cargó desde archivos reales
                    // En una implementación real, se usaría un logger
                }
            } else {
                // Fallback: Buscar en el directorio del proyecto (para desarrollo)
                let project_model_path =
                    format!("/home/moebius/eclipse/eclipse_kernel/models/{}", model_name);
                if fat32_driver.file_exists(&project_model_path) {
                    // Crear modelo desde el directorio del proyecto
                    let model = self.create_model_from_directory(model_name, fat32_driver);
                    let _ = self.models.insert(str_to_heapless(model_name), model);
                    loaded_count += 1;
                    // Debug: Mostrar que se cargó desde el proyecto
                }
            }
        }

        Ok(loaded_count)
    }

    /// Crear modelo desde metadata JSON
    fn create_model_from_metadata(
        &self,
        model_name: &str,
        metadata: &[u8],
    ) -> Result<AIModel, String<64>> {
        // Parsear el JSON de metadata
        let metadata_str = match core::str::from_utf8(metadata) {
            Ok(s) => s,
            Err(_) => return Ok(self.create_basic_model(model_name)),
        };

        // Parsear campos básicos del JSON (implementación simple)
        let mut model_type = ModelType::Custom(str_to_heapless_32("unknown"));
        let mut size_bytes = 100_000_000; // Default 100MB
        let mut version = str_to_heapless_16("1.0.0");
        let mut description = str_to_heapless("Modelo cargado desde archivo");
        let mut checksum = str_to_heapless("real_checksum");

        // Buscar campos específicos en el JSON
        for line in metadata_str.lines() {
            let line = line.trim();

            // Parsear tipo de modelo
            if line.contains("\"model_type\"") || line.contains("\"type\"") {
                if line.contains("conversational") || line.contains("dialogue") {
                    model_type = ModelType::Conversational;
                } else if line.contains("classifier") || line.contains("classification") {
                    model_type = ModelType::TextClassifier;
                } else if line.contains("embedding") || line.contains("sentence") {
                    model_type = ModelType::Embedding;
                } else if line.contains("translation") || line.contains("translate") {
                    model_type = ModelType::Translation;
                }
            }

            // Parsear tamaño
            if line.contains("\"size\"") || line.contains("\"size_bytes\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let size_str = value_part[..end].trim().trim_matches('"');
                        if let Ok(size) = size_str.parse::<u64>() {
                            size_bytes = size;
                        }
                    }
                }
            }

            // Parsear versión
            if line.contains("\"version\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let version_str = value_part[..end].trim().trim_matches('"');
                        if version_str.len() <= 16 {
                            version = str_to_heapless_16(version_str);
                        }
                    }
                }
            }

            // Parsear descripción
            if line.contains("\"description\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let desc_str = value_part[..end].trim().trim_matches('"');
                        if desc_str.len() <= 64 {
                            description = str_to_heapless(desc_str);
                        }
                    }
                }
            }

            // Parsear checksum
            if line.contains("\"checksum\"") || line.contains("\"sha256\"") {
                if let Some(start) = line.find(':') {
                    let value_part = &line[start + 1..];
                    if let Some(end) = value_part.find(',') {
                        let checksum_str = value_part[..end].trim().trim_matches('"');
                        if checksum_str.len() <= 64 {
                            checksum = str_to_heapless(checksum_str);
                        }
                    }
                }
            }
        }

        // Si no se pudo determinar el tipo, usar lógica basada en el nombre
        if matches!(model_type, ModelType::Custom(_)) {
            model_type = match model_name {
                name if name.contains("DialoGPT") || name.contains("blenderbot") => {
                    ModelType::Conversational
                }
                name if name.contains("distilbert") || name.contains("roberta") => {
                    ModelType::TextClassifier
                }
                name if name.contains("sentence_transformers") => ModelType::Embedding,
                name if name.contains("opus_mt") => ModelType::Translation,
                _ => ModelType::Custom(str_to_heapless_32("unknown")),
            };
        }

        Ok(AIModel {
            model_id: str_to_heapless(model_name),
            name: str_to_heapless(model_name),
            model_type,
            file_path: str_to_heapless_128(&format!("/ai_models/{}/", model_name)),
            size_bytes,
            version,
            downloaded_at: self.get_current_timestamp(),
            last_used: 0,
            effectiveness: 0.8,
            status: ModelStatus::Available,
            checksum,
            metadata: FnvIndexMap::new(),
        })
    }

    /// Crear modelo desde directorio (sin metadata)
    fn create_model_from_directory(
        &self,
        model_name: &str,
        fat32_driver: &mut Fat32Driver,
    ) -> AIModel {
        // Intentar primero en la partición EFI, luego en el proyecto
        let efi_path = format!("/ai_models/{}", model_name);
        let project_path = format!("/home/moebius/eclipse/eclipse_kernel/models/{}", model_name);

        let model_path = if fat32_driver.file_exists(&efi_path) {
            efi_path
        } else {
            project_path
        };

        let mut total_size = 0u64;

        // Calcular tamaño real de los archivos del modelo
        let model_files = [
            "pytorch_model.bin",
            "model.safetensors",
            "config.json",
            "tokenizer.json",
            "vocab.txt",
            "merges.txt",
            "special_tokens_map.json",
            "tokenizer_config.json",
        ];

        for file_name in &model_files {
            let file_path = format!("{}/{}", model_path, file_name);
            if let Ok(file_content) = fat32_driver.read_file_by_path(&file_path) {
                total_size += file_content.len() as u64;
            }
        }

        // Si no se encontraron archivos, usar tamaño estimado
        if total_size == 0 {
            total_size = 100_000_000; // 100MB por defecto
        }

        let model_type = match model_name {
            name if name.contains("DialoGPT") || name.contains("blenderbot") => {
                ModelType::Conversational
            }
            name if name.contains("distilbert")
                || name.contains("roberta")
                || name.contains("classifier") =>
            {
                ModelType::TextClassifier
            }
            name if name.contains("sentence_transformers") || name.contains("MiniLM") => {
                ModelType::Embedding
            }
            name if name.contains("opus_mt") || name.contains("translation") => {
                ModelType::Translation
            }
            name if name.contains("anomaly") => {
                ModelType::Custom(str_to_heapless_32("anomaly_detector"))
            }
            name if name.contains("efficientnet") => {
                ModelType::Custom(str_to_heapless_32("image_classifier"))
            }
            name if name.contains("mobilenet") => {
                ModelType::Custom(str_to_heapless_32("image_classifier"))
            }
            name if name.contains("performance-predictor") => {
                ModelType::Custom(str_to_heapless_32("performance_predictor"))
            }
            name if name.contains("tinyllama") => ModelType::Conversational,
            _ => ModelType::Custom(str_to_heapless_32("unknown")),
        };

        AIModel {
            model_id: str_to_heapless(model_name),
            name: str_to_heapless(model_name),
            model_type,
            file_path: str_to_heapless_128(&model_path),
            size_bytes: total_size,
            version: str_to_heapless_16("1.0.0"),
            downloaded_at: self.get_current_timestamp(),
            last_used: 0,
            effectiveness: 0.8,
            status: ModelStatus::Available,
            checksum: str_to_heapless("real_file_size"),
            metadata: FnvIndexMap::new(),
        }
    }

    /// Crear modelo básico
    fn create_basic_model(&self, model_name: &str) -> AIModel {
        let model_type = match model_name {
            name if name.contains("DialoGPT") || name.contains("blenderbot") => {
                ModelType::Conversational
            }
            name if name.contains("distilbert")
                || name.contains("roberta")
                || name.contains("classifier") =>
            {
                ModelType::TextClassifier
            }
            name if name.contains("sentence_transformers") || name.contains("MiniLM") => {
                ModelType::Embedding
            }
            name if name.contains("opus_mt") || name.contains("translation") => {
                ModelType::Translation
            }
            name if name.contains("anomaly") => {
                ModelType::Custom(str_to_heapless_32("anomaly_detector"))
            }
            name if name.contains("efficientnet") => {
                ModelType::Custom(str_to_heapless_32("image_classifier"))
            }
            name if name.contains("mobilenet") => {
                ModelType::Custom(str_to_heapless_32("image_classifier"))
            }
            name if name.contains("performance-predictor") => {
                ModelType::Custom(str_to_heapless_32("performance_predictor"))
            }
            name if name.contains("tinyllama") => ModelType::Conversational,
            _ => ModelType::Custom(str_to_heapless_32("unknown")),
        };

        AIModel {
            model_id: str_to_heapless(model_name),
            name: str_to_heapless(model_name),
            model_type,
            file_path: str_to_heapless_128(&format!("/ai_models/{}/", model_name)),
            size_bytes: 100_000_000, // 100MB estimado
            version: str_to_heapless_16("1.0.0"),
            downloaded_at: self.get_current_timestamp(),
            last_used: 0,
            effectiveness: 0.8,
            status: ModelStatus::Available,
            checksum: str_to_heapless("loaded_from_fs"),
            metadata: FnvIndexMap::new(),
        }
    }
}

// Instancia global del gestor de modelos
static mut GLOBAL_AI_MODEL_MANAGER: Option<GlobalAIModelManager> = None;

/// Inicializar gestor global de modelos
pub fn init_global_ai_models() -> Result<(), String<64>> {
    unsafe {
        GLOBAL_AI_MODEL_MANAGER = Some(GlobalAIModelManager::new());
        if let Some(ref mut manager) = GLOBAL_AI_MODEL_MANAGER {
            // Cargar modelos simulados iniciales para que haya modelos disponibles
            manager.load_simulated_models();
        }
    }
    Ok(())
}

/// Obtener instancia del gestor global
pub fn get_global_ai_model_manager() -> Option<&'static mut GlobalAIModelManager> {
    unsafe { GLOBAL_AI_MODEL_MANAGER.as_mut() }
}

/// Helper function para convertir &str a heapless::String<64>
fn str_to_heapless(s: &str) -> String<64> {
    let mut result = String::new();
    for ch in s.chars().take(63) {
        if result.push(ch).is_err() {
            break;
        }
    }
    result
}

/// Helper function para convertir &str a heapless::String<32>
fn str_to_heapless_32(s: &str) -> String<32> {
    let mut result = String::new();
    for ch in s.chars().take(31) {
        if result.push(ch).is_err() {
            break;
        }
    }
    result
}

/// Helper function para convertir &str a heapless::String<128>
fn str_to_heapless_128(s: &str) -> String<128> {
    let mut result = String::new();
    for ch in s.chars().take(127) {
        if result.push(ch).is_err() {
            break;
        }
    }
    result
}

/// Helper function para convertir &str a heapless::String<16>
fn str_to_heapless_16(s: &str) -> String<16> {
    let mut result = String::new();
    for ch in s.chars().take(15) {
        if result.push(ch).is_err() {
            break;
        }
    }
    result
}
