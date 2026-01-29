//! Sistema de Búsqueda Global Inteligente para COSMIC
//!
//! Este módulo implementa un sistema de búsqueda avanzado que permite
//! encontrar archivos, aplicaciones, configuraciones y contenido en todo el sistema.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

/// Tipos de contenido que se pueden buscar
#[derive(Debug, Clone, PartialEq)]
pub enum SearchContentType {
    File,
    Directory,
    Application,
    Configuration,
    Document,
    Image,
    Video,
    Audio,
    Code,
    Database,
    Archive,
    System,
    Process,
    Network,
    Custom(String),
}

/// Prioridad de los resultados de búsqueda
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub enum SearchResultPriority {
    Low = 1,
    Normal = 2,
    High = 3,
    Critical = 4,
    Exact = 5,
}

/// Estado del resultado de búsqueda
#[derive(Debug, Clone, PartialEq)]
pub enum SearchResultStatus {
    Available,
    Restricted,
    Hidden,
    Deleted,
    Error,
}

/// Tipo de coincidencia en la búsqueda
#[derive(Debug, Clone, PartialEq)]
pub enum MatchType {
    Exact,
    Prefix,
    Suffix,
    Contains,
    Fuzzy,
    Regex,
    Semantic,
}

/// Configuración de búsqueda
#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub max_results: u32,
    pub timeout_ms: u32,
    pub case_sensitive: bool,
    pub use_fuzzy_search: bool,
    pub use_semantic_search: bool,
    pub search_hidden: bool,
    pub search_system: bool,
    pub content_types: Vec<SearchContentType>,
    pub directories: Vec<String>,
    pub excluded_directories: Vec<String>,
    pub min_score: f32,
    pub auto_complete: bool,
    pub cache_enabled: bool,
    pub cache_duration_ms: u32,
}

/// Resultado de búsqueda individual
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub description: String,
    pub path: String,
    pub content_type: SearchContentType,
    pub priority: SearchResultPriority,
    pub status: SearchResultStatus,
    pub match_type: MatchType,
    pub score: f32,
    pub metadata: BTreeMap<String, String>,
    pub preview: Option<String>,
    pub icon: Option<String>,
    pub size: Option<u64>,
    pub modified_time: Option<u64>,
    pub created_time: Option<u64>,
    pub tags: Vec<String>,
    pub keywords: Vec<String>,
    pub action: SearchAction,
    pub is_bookmarked: bool,
    pub access_count: u32,
    pub last_accessed: Option<u64>,
}

/// Acción que se puede realizar con el resultado
#[derive(Debug, Clone)]
pub enum SearchAction {
    Open,
    OpenWith(String),
    Execute,
    Edit,
    Copy,
    Move,
    Delete,
    Bookmark,
    Share,
    Custom(String),
}

/// Filtro de búsqueda
#[derive(Debug, Clone)]
pub struct SearchFilter {
    pub content_types: Vec<SearchContentType>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub date_from: Option<u64>,
    pub date_to: Option<u64>,
    pub tags: Vec<String>,
    pub extensions: Vec<String>,
    pub owners: Vec<String>,
    pub permissions: Option<u32>,
}

/// Estadísticas de búsqueda
#[derive(Debug, Clone)]
pub struct SearchStats {
    pub total_searches: u32,
    pub average_search_time: f32,
    pub most_searched_terms: Vec<String>,
    pub popular_results: Vec<String>,
    pub cache_hit_rate: f32,
    pub error_rate: f32,
    pub total_indexed_items: u32,
    pub last_index_update: u64,
}

/// Índice de búsqueda
#[derive(Debug, Clone)]
pub struct SearchIndex {
    pub items: BTreeMap<String, SearchResult>,
    pub content_index: BTreeMap<String, Vec<String>>,
    pub tag_index: BTreeMap<String, Vec<String>>,
    pub keyword_index: BTreeMap<String, Vec<String>>,
    pub last_updated: u64,
    pub version: u32,
}

/// Motor de búsqueda semántica
#[derive(Debug, Clone)]
pub struct SemanticSearchEngine {
    pub word_embeddings: BTreeMap<String, Vec<f32>>,
    pub concept_map: BTreeMap<String, Vec<String>>,
    pub synonym_map: BTreeMap<String, Vec<String>>,
    pub context_weights: BTreeMap<String, f32>,
}

/// Motor de búsqueda difusa
#[derive(Debug, Clone)]
pub struct FuzzySearchEngine {
    pub edit_distance_threshold: u32,
    pub ngram_size: u32,
    pub similarity_threshold: f32,
    pub ngram_index: BTreeMap<String, Vec<String>>,
}

/// Sugerencias de autocompletado
#[derive(Debug, Clone)]
pub struct SearchSuggestion {
    pub text: String,
    pub type_: SearchContentType,
    pub confidence: f32,
    pub usage_count: u32,
    pub last_used: u64,
}

/// Historial de búsquedas
#[derive(Debug, Clone)]
pub struct SearchHistory {
    pub query: String,
    pub timestamp: u64,
    pub result_count: u32,
    pub selected_result: Option<String>,
    pub filters_applied: Vec<SearchFilter>,
}

/// Gestor del sistema de búsqueda global
pub struct GlobalSearchSystem {
    pub config: SearchConfig,
    pub index: SearchIndex,
    pub semantic_engine: SemanticSearchEngine,
    pub fuzzy_engine: FuzzySearchEngine,
    pub stats: SearchStats,
    pub suggestions: Vec<SearchSuggestion>,
    pub history: VecDeque<SearchHistory>,
    pub cache: BTreeMap<String, Vec<SearchResult>>,
    pub bookmarks: BTreeSet<String>,
    pub next_id: AtomicU32,
    pub current_search_id: Option<String>,
    pub search_in_progress: bool,
}

impl GlobalSearchSystem {
    /// Crear nuevo sistema de búsqueda global
    pub fn new() -> Self {
        Self {
            config: SearchConfig {
                max_results: 100,
                timeout_ms: 5000,
                case_sensitive: false,
                use_fuzzy_search: true,
                use_semantic_search: true,
                search_hidden: false,
                search_system: false,
                content_types: Vec::new(),
                directories: Vec::new(),
                excluded_directories: Vec::from([
                    "/proc".to_string(),
                    "/sys".to_string(),
                    "/dev".to_string(),
                ]),
                min_score: 0.1,
                auto_complete: true,
                cache_enabled: true,
                cache_duration_ms: 300000, // 5 minutos
            },
            index: SearchIndex {
                items: BTreeMap::new(),
                content_index: BTreeMap::new(),
                tag_index: BTreeMap::new(),
                keyword_index: BTreeMap::new(),
                last_updated: 0,
                version: 1,
            },
            semantic_engine: SemanticSearchEngine {
                word_embeddings: BTreeMap::new(),
                concept_map: BTreeMap::new(),
                synonym_map: BTreeMap::new(),
                context_weights: BTreeMap::new(),
            },
            fuzzy_engine: FuzzySearchEngine {
                edit_distance_threshold: 3,
                ngram_size: 2,
                similarity_threshold: 0.6,
                ngram_index: BTreeMap::new(),
            },
            stats: SearchStats {
                total_searches: 0,
                average_search_time: 0.0,
                most_searched_terms: Vec::new(),
                popular_results: Vec::new(),
                cache_hit_rate: 0.0,
                error_rate: 0.0,
                total_indexed_items: 0,
                last_index_update: 0,
            },
            suggestions: Vec::new(),
            history: VecDeque::new(),
            cache: BTreeMap::new(),
            bookmarks: BTreeSet::new(),
            next_id: AtomicU32::new(1),
            current_search_id: None,
            search_in_progress: false,
        }
    }

    /// Realizar búsqueda global
    pub fn search(
        &mut self,
        query: String,
        filters: Option<SearchFilter>,
    ) -> Result<Vec<SearchResult>, &'static str> {
        if query.is_empty() {
            return Err("Query cannot be empty");
        }

        if self.search_in_progress {
            return Err("Search already in progress");
        }

        self.search_in_progress = true;
        self.stats.total_searches += 1;

        // Verificar caché primero
        if self.config.cache_enabled {
            if let Some(cached_results) = self.cache.get(&query) {
                self.stats.cache_hit_rate = (self.stats.cache_hit_rate + 1.0) / 2.0;
                self.search_in_progress = false;
                return Ok(cached_results.clone());
            }
        }

        let search_start = self.get_current_timestamp();
        let mut results = Vec::new();

        // Búsqueda exacta
        results.extend(self.exact_search(&query)?);

        // Búsqueda difusa si está habilitada
        if self.config.use_fuzzy_search {
            results.extend(self.fuzzy_search(&query)?);
        }

        // Búsqueda semántica si está habilitada
        if self.config.use_semantic_search {
            results.extend(self.semantic_search(&query)?);
        }

        // Aplicar filtros si se proporcionan
        if let Some(filter) = &filters {
            results = self.apply_filters(results, filter);
        }

        // Ordenar por score y prioridad
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(core::cmp::Ordering::Equal)
                .then(b.priority.cmp(&a.priority))
        });

        // Limitar resultados
        if results.len() > self.config.max_results as usize {
            results.truncate(self.config.max_results as usize);
        }

        // Actualizar estadísticas
        let search_time = self.get_current_timestamp() - search_start;
        self.stats.average_search_time =
            (self.stats.average_search_time + search_time as f32) / 2.0;

        // Agregar al historial
        self.add_to_history(query.clone(), results.len() as u32, filters);

        // Actualizar sugerencias
        self.update_suggestions(&query);

        // Guardar en caché
        if self.config.cache_enabled {
            self.cache.insert(query, results.clone());
        }

        self.search_in_progress = false;
        Ok(results)
    }

    /// Búsqueda exacta
    fn exact_search(&self, query: &str) -> Result<Vec<SearchResult>, &'static str> {
        let mut results = Vec::new();
        let query_lower = if self.config.case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };

        for (id, item) in &self.index.items {
            let mut score = 0.0;

            // Coincidencia en título
            let title_lower = if self.config.case_sensitive {
                item.title.clone()
            } else {
                item.title.to_lowercase()
            };

            if title_lower == query_lower {
                score += 1.0;
            } else if title_lower.starts_with(&query_lower) {
                score += 0.9;
            } else if title_lower.contains(&query_lower) {
                score += 0.7;
            }

            // Coincidencia en descripción
            let desc_lower = if self.config.case_sensitive {
                item.description.clone()
            } else {
                item.description.to_lowercase()
            };

            if desc_lower.contains(&query_lower) {
                score += 0.5;
            }

            // Coincidencia en keywords
            for keyword in &item.keywords {
                let keyword_lower = if self.config.case_sensitive {
                    keyword.clone()
                } else {
                    keyword.to_lowercase()
                };

                if keyword_lower.contains(&query_lower) {
                    score += 0.3;
                }
            }

            if score > self.config.min_score {
                let mut result = item.clone();
                result.score = score;
                result.match_type = if score == 1.0 {
                    MatchType::Exact
                } else {
                    MatchType::Contains
                };
                results.push(result);
            }
        }

        Ok(results)
    }

    /// Búsqueda difusa
    fn fuzzy_search(&self, query: &str) -> Result<Vec<SearchResult>, &'static str> {
        let mut results = Vec::new();
        let query_lower = query.to_lowercase();

        for (id, item) in &self.index.items {
            let similarity = self.calculate_similarity(&query_lower, &item.title.to_lowercase());

            if similarity >= self.fuzzy_engine.similarity_threshold {
                let mut result = item.clone();
                result.score = similarity * 0.8; // Penalizar búsqueda difusa
                result.match_type = MatchType::Fuzzy;
                results.push(result);
            }
        }

        Ok(results)
    }

    /// Búsqueda semántica
    fn semantic_search(&self, query: &str) -> Result<Vec<SearchResult>, &'static str> {
        let mut results = Vec::new();
        let query_concepts = self.extract_concepts(query);

        for (id, item) in &self.index.items {
            let mut semantic_score = 0.0;

            // Buscar conceptos relacionados
            for concept in &query_concepts {
                if let Some(related_concepts) = self.semantic_engine.concept_map.get(concept) {
                    for related_concept in related_concepts {
                        if item.keywords.contains(related_concept) {
                            semantic_score += 0.4;
                        }
                    }
                }

                // Buscar sinónimos
                if let Some(synonyms) = self.semantic_engine.synonym_map.get(concept) {
                    for synonym in synonyms {
                        if item.title.to_lowercase().contains(&synonym.to_lowercase()) {
                            semantic_score += 0.3;
                        }
                    }
                }
            }

            if semantic_score > self.config.min_score {
                let mut result = item.clone();
                result.score = semantic_score * 0.6; // Penalizar búsqueda semántica
                result.match_type = MatchType::Semantic;
                results.push(result);
            }
        }

        Ok(results)
    }

    /// Calcular similitud entre dos strings
    fn calculate_similarity(&self, s1: &str, s2: &str) -> f32 {
        let distance = self.levenshtein_distance(s1, s2);
        let max_len = s1.len().max(s2.len()) as f32;

        if max_len == 0.0 {
            1.0
        } else {
            1.0 - (distance as f32 / max_len)
        }
    }

    /// Calcular distancia de Levenshtein
    fn levenshtein_distance(&self, s1: &str, s2: &str) -> usize {
        let s1_chars: Vec<char> = s1.chars().collect();
        let s2_chars: Vec<char> = s2.chars().collect();
        let s1_len = s1_chars.len();
        let s2_len = s2_chars.len();

        if s1_len == 0 {
            return s2_len;
        }
        if s2_len == 0 {
            return s1_len;
        }

        let mut matrix = Vec::new();
        for _ in 0..=s1_len {
            let mut row = Vec::new();
            for _ in 0..=s2_len {
                row.push(0);
            }
            matrix.push(row);
        }

        for i in 0..=s1_len {
            matrix[i][0] = i;
        }

        for j in 0..=s2_len {
            matrix[0][j] = j;
        }

        for i in 1..=s1_len {
            for j in 1..=s2_len {
                let cost = if s1_chars[i - 1] == s2_chars[j - 1] {
                    0
                } else {
                    1
                };
                matrix[i][j] = (matrix[i - 1][j] + 1)
                    .min(matrix[i][j - 1] + 1)
                    .min(matrix[i - 1][j - 1] + cost);
            }
        }

        matrix[s1_len][s2_len]
    }

    /// Extraer conceptos de una consulta
    fn extract_concepts(&self, query: &str) -> Vec<String> {
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut concepts = Vec::new();

        for word in words {
            let word_lower = word.to_lowercase();
            concepts.push(word_lower.clone());

            // Buscar conceptos relacionados
            if let Some(related) = self.semantic_engine.concept_map.get(&word_lower) {
                concepts.extend(related.clone());
            }
        }

        concepts
    }

    /// Aplicar filtros a los resultados
    fn apply_filters(
        &self,
        mut results: Vec<SearchResult>,
        filter: &SearchFilter,
    ) -> Vec<SearchResult> {
        results.retain(|result| {
            // Filtrar por tipo de contenido
            if !filter.content_types.is_empty()
                && !filter.content_types.contains(&result.content_type)
            {
                return false;
            }

            // Filtrar por tamaño
            if let Some(size) = result.size {
                if let Some(min_size) = filter.min_size {
                    if size < min_size {
                        return false;
                    }
                }
                if let Some(max_size) = filter.max_size {
                    if size > max_size {
                        return false;
                    }
                }
            }

            // Filtrar por fecha
            if let Some(modified_time) = result.modified_time {
                if let Some(date_from) = filter.date_from {
                    if modified_time < date_from {
                        return false;
                    }
                }
                if let Some(date_to) = filter.date_to {
                    if modified_time > date_to {
                        return false;
                    }
                }
            }

            // Filtrar por tags
            if !filter.tags.is_empty() {
                let has_matching_tag = filter.tags.iter().any(|tag| result.tags.contains(tag));
                if !has_matching_tag {
                    return false;
                }
            }

            // Filtrar por extensión
            if !filter.extensions.is_empty() {
                let extension = self.get_file_extension(&result.path);
                if !filter.extensions.contains(&extension) {
                    return false;
                }
            }

            true
        });

        results
    }

    /// Obtener extensión de archivo
    fn get_file_extension(&self, path: &str) -> String {
        if let Some(dot_pos) = path.rfind('.') {
            path[dot_pos + 1..].to_lowercase()
        } else {
            String::new()
        }
    }

    /// Agregar elemento al índice
    pub fn index_item(&mut self, item: SearchResult) -> Result<(), &'static str> {
        let id = item.id.clone();

        // Agregar al índice principal
        self.index.items.insert(id.clone(), item.clone());

        // Agregar al índice de contenido
        let content_key = item.title.to_lowercase();
        self.index
            .content_index
            .entry(content_key)
            .or_insert_with(Vec::new)
            .push(id.clone());

        // Agregar al índice de tags
        for tag in &item.tags {
            self.index
                .tag_index
                .entry(tag.clone())
                .or_insert_with(Vec::new)
                .push(id.clone());
        }

        // Agregar al índice de keywords
        for keyword in &item.keywords {
            self.index
                .keyword_index
                .entry(keyword.clone())
                .or_insert_with(Vec::new)
                .push(id.clone());
        }

        self.stats.total_indexed_items += 1;
        self.index.last_updated = self.get_current_timestamp();

        Ok(())
    }

    /// Eliminar elemento del índice
    pub fn remove_from_index(&mut self, id: &str) -> Result<(), &'static str> {
        if let Some(item) = self.index.items.remove(id) {
            // Remover de índices secundarios
            let content_key = item.title.to_lowercase();
            if let Some(ids) = self.index.content_index.get_mut(&content_key) {
                ids.retain(|item_id| item_id != id);
                if ids.is_empty() {
                    self.index.content_index.remove(&content_key);
                }
            }

            for tag in &item.tags {
                if let Some(ids) = self.index.tag_index.get_mut(tag) {
                    ids.retain(|item_id| item_id != id);
                    if ids.is_empty() {
                        self.index.tag_index.remove(tag);
                    }
                }
            }

            for keyword in &item.keywords {
                if let Some(ids) = self.index.keyword_index.get_mut(keyword) {
                    ids.retain(|item_id| item_id != id);
                    if ids.is_empty() {
                        self.index.keyword_index.remove(keyword);
                    }
                }
            }

            self.stats.total_indexed_items = self.stats.total_indexed_items.saturating_sub(1);
            self.index.last_updated = self.get_current_timestamp();
        }

        Ok(())
    }

    /// Obtener sugerencias de autocompletado
    pub fn get_suggestions(&self, query: &str, limit: u32) -> Vec<SearchSuggestion> {
        let mut suggestions = Vec::new();
        let query_lower = query.to_lowercase();

        // Buscar en el historial
        for history_item in &self.history {
            if history_item.query.to_lowercase().starts_with(&query_lower) {
                suggestions.push(SearchSuggestion {
                    text: history_item.query.clone(),
                    type_: SearchContentType::File,
                    confidence: 0.9,
                    usage_count: 1,
                    last_used: history_item.timestamp,
                });
            }
        }

        // Buscar en el índice
        for (id, item) in &self.index.items {
            if item.title.to_lowercase().starts_with(&query_lower) {
                suggestions.push(SearchSuggestion {
                    text: item.title.clone(),
                    type_: item.content_type.clone(),
                    confidence: 0.8,
                    usage_count: item.access_count,
                    last_used: item.last_accessed.unwrap_or(0),
                });
            }
        }

        // Ordenar por confianza y uso
        suggestions.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(core::cmp::Ordering::Equal)
                .then(b.usage_count.cmp(&a.usage_count))
        });

        // Limitar resultados
        if suggestions.len() > limit as usize {
            suggestions.truncate(limit as usize);
        }

        suggestions
    }

    /// Agregar al historial de búsquedas
    fn add_to_history(&mut self, query: String, result_count: u32, filters: Option<SearchFilter>) {
        let history_item = SearchHistory {
            query,
            timestamp: self.get_current_timestamp(),
            result_count,
            selected_result: None,
            filters_applied: filters.map(|f| Vec::from([f])).unwrap_or_default(),
        };

        self.history.push_back(history_item);

        // Limitar tamaño del historial
        if self.history.len() > 100 {
            self.history.pop_front();
        }
    }

    /// Actualizar sugerencias
    fn update_suggestions(&mut self, query: &str) {
        let suggestions = self.get_suggestions(query, 10);
        self.suggestions = suggestions;
    }

    /// Marcar resultado como favorito
    pub fn bookmark_result(&mut self, result_id: &str) -> Result<(), &'static str> {
        self.bookmarks.insert(result_id.to_string());

        if let Some(item) = self.index.items.get_mut(result_id) {
            item.is_bookmarked = true;
        }

        Ok(())
    }

    /// Desmarcar resultado como favorito
    pub fn unbookmark_result(&mut self, result_id: &str) -> Result<(), &'static str> {
        self.bookmarks.remove(result_id);

        if let Some(item) = self.index.items.get_mut(result_id) {
            item.is_bookmarked = false;
        }

        Ok(())
    }

    /// Obtener resultados favoritos
    pub fn get_bookmarked_results(&self) -> Vec<SearchResult> {
        let mut results = Vec::new();

        for id in &self.bookmarks {
            if let Some(item) = self.index.items.get(id) {
                results.push(item.clone());
            }
        }

        results.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));
        results
    }

    /// Renderizar interfaz de búsqueda
    pub fn render_search_interface(
        &self,
        fb: &mut FramebufferDriver,
        query: &str,
        results: &[SearchResult],
        selected_index: usize,
    ) -> Result<(), &'static str> {
        let search_box_x = 100;
        let search_box_y = 100;
        let search_box_width = 800;
        let search_box_height = 40;

        // Renderizar caja de búsqueda
        fb.draw_rect(
            search_box_x,
            search_box_y,
            search_box_width,
            search_box_height,
            Color::WHITE,
        );
        fb.draw_line(
            search_box_x as i32,
            search_box_y as i32,
            (search_box_x + search_box_width) as i32,
            search_box_y as i32,
            Color::BLACK,
        );
        fb.draw_line(
            search_box_x as i32,
            search_box_y as i32,
            search_box_x as i32,
            (search_box_y + search_box_height) as i32,
            Color::BLACK,
        );
        fb.draw_line(
            (search_box_x + search_box_width) as i32,
            search_box_y as i32,
            (search_box_x + search_box_width) as i32,
            (search_box_y + search_box_height) as i32,
            Color::BLACK,
        );
        fb.draw_line(
            search_box_x as i32,
            (search_box_y + search_box_height) as i32,
            (search_box_x + search_box_width) as i32,
            (search_box_y + search_box_height) as i32,
            Color::BLACK,
        );

        // Renderizar query
        fb.write_text_kernel_typing(search_box_x + 10, search_box_y + 10, query, Color::BLACK);

        // Renderizar resultados
        let mut result_y = search_box_y + search_box_height + 20;
        for (index, result) in results.iter().enumerate() {
            if index >= 10 {
                break;
            } // Limitar a 10 resultados visibles

            let bg_color = if index == selected_index {
                Color::BLUE
            } else {
                Color::GRAY
            };

            fb.draw_rect(search_box_x, result_y, search_box_width, 60, bg_color);

            // Renderizar título
            fb.write_text_kernel_typing(
                search_box_x + 10,
                result_y + 10,
                &result.title,
                Color::BLACK,
            );

            // Renderizar descripción
            fb.write_text_kernel_typing(
                search_box_x + 10,
                result_y + 25,
                &result.description,
                Color::GRAY,
            );

            // Renderizar tipo
            let type_text = format!("{:?}", result.content_type);
            fb.write_text_kernel_typing(search_box_x + 10, result_y + 40, &type_text, Color::BLUE);

            result_y += 70;
        }

        Ok(())
    }

    /// Obtener timestamp actual (simulado)
    fn get_current_timestamp(&self) -> u64 {
        1640995200 // Timestamp simulado
    }

    /// Obtener estadísticas del sistema
    pub fn get_stats(&self) -> &SearchStats {
        &self.stats
    }

    /// Configurar el sistema
    pub fn configure(&mut self, config: SearchConfig) {
        self.config = config;
    }

    /// Limpiar caché
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Limpiar historial
    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

impl Default for GlobalSearchSystem {
    fn default() -> Self {
        Self::new()
    }
}
