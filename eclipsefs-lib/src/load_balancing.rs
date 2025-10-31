//! Sistema de balanceo de carga para EclipseFS (inspirado en RedoxFS)

#[cfg(feature = "std")]
use std::collections::HashMap;

#[cfg(not(feature = "std"))]
use heapless::FnvIndexMap;

use crate::{EclipseFSError, EclipseFSResult};

/// Configuración de balanceo de carga
#[derive(Debug, Clone)]
pub struct LoadBalancingConfig {
    pub enabled: bool,
    pub rebalance_threshold: f32,      // Umbral para rebalancear
    pub max_operations_per_cycle: usize,
    pub background_mode: bool,
    pub consider_access_patterns: bool,
    pub consider_file_sizes: bool,
    pub consider_fragmentation: bool,
    pub load_balancing_algorithm: LoadBalancingAlgorithm,
}

#[derive(Debug, Clone, Copy)]
pub enum LoadBalancingAlgorithm {
    RoundRobin,
    LeastLoaded,
    WeightedRoundRobin,
    ConsistentHashing,
    Adaptive,
}

impl Default for LoadBalancingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rebalance_threshold: 20.0,
            max_operations_per_cycle: 50,
            background_mode: true,
            consider_access_patterns: true,
            consider_file_sizes: true,
            consider_fragmentation: true,
            load_balancing_algorithm: LoadBalancingAlgorithm::Adaptive,
        }
    }
}

/// Información de carga de un nodo/storage
#[derive(Debug, Clone)]
pub struct NodeLoadInfo {
    pub node_id: u32,
    pub total_capacity: u64,
    pub used_capacity: u64,
    pub file_count: usize,
    pub access_count: u64,
    pub fragmentation_level: f32,
    pub response_time_ms: u64,
    pub is_healthy: bool,
    pub last_update: u64,
}

impl NodeLoadInfo {
    pub fn new(node_id: u32, total_capacity: u64) -> Self {
        Self {
            node_id,
            total_capacity,
            used_capacity: 0,
            file_count: 0,
            access_count: 0,
            fragmentation_level: 0.0,
            response_time_ms: 0,
            is_healthy: true,
            last_update: Self::current_timestamp(),
        }
    }
    
    pub fn utilization_percentage(&self) -> f32 {
        if self.total_capacity > 0 {
            (self.used_capacity as f32 / self.total_capacity as f32) * 100.0
        } else {
            0.0
        }
    }
    
    pub fn calculate_load_score(&self) -> f64 {
        let utilization = self.utilization_percentage() as f64;
        let fragmentation_penalty = self.fragmentation_level as f64 * 0.1;
        let response_penalty = (self.response_time_ms as f64) / 1000.0;
        let health_bonus = if self.is_healthy { 0.0 } else { 100.0 };
        
        utilization + fragmentation_penalty + response_penalty + health_bonus
    }
    
    fn current_timestamp() -> u64 {
        // En un sistema real, esto vendría del kernel o RTC
        1640995200 // 2022-01-01 00:00:00 UTC
    }
}

/// Sistema de balanceo de carga inteligente (inspirado en RedoxFS)
#[cfg(feature = "std")]
pub struct IntelligentLoadBalancer {
    config: LoadBalancingConfig,
    nodes: HashMap<u32, NodeLoadInfo>,
    file_assignments: HashMap<u32, u32>, // inode -> node_id
    operation_count: usize,
    last_rebalance_time: u64,
    round_robin_counter: usize,
}

#[cfg(not(feature = "std"))]
pub struct IntelligentLoadBalancer {
    config: LoadBalancingConfig,
    nodes: FnvIndexMap<u32, NodeLoadInfo, 16>,
    file_assignments: FnvIndexMap<u32, u32, 1024>, // inode -> node_id
    operation_count: usize,
    last_rebalance_time: u64,
    round_robin_counter: usize,
}

impl IntelligentLoadBalancer {
    pub fn new(config: LoadBalancingConfig) -> Self {
        Self {
            config,
            #[cfg(feature = "std")]
            nodes: HashMap::new(),
            #[cfg(not(feature = "std"))]
            nodes: FnvIndexMap::new(),
            #[cfg(feature = "std")]
            file_assignments: HashMap::new(),
            #[cfg(not(feature = "std"))]
            file_assignments: FnvIndexMap::new(),
            operation_count: 0,
            last_rebalance_time: Self::current_timestamp(),
            round_robin_counter: 0,
        }
    }
    
    /// Agregar nodo al sistema
    pub fn add_node(&mut self, node_id: u32, total_capacity: u64) -> EclipseFSResult<()> {
        let node_info = NodeLoadInfo::new(node_id, total_capacity);
        
        #[cfg(feature = "std")]
        {
            self.nodes.insert(node_id, node_info);
        }
        
        #[cfg(not(feature = "std"))]
        {
            self.nodes.insert(node_id, node_info).map_err(|_| EclipseFSError::InvalidOperation)?;
        }
        
        Ok(())
    }
    
    /// Asignar archivo a nodo usando algoritmo de balanceo
    pub fn assign_file(&mut self, inode: u32, file_size: u64) -> EclipseFSResult<u32> {
        if self.nodes.is_empty() {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        let node_id = match self.config.load_balancing_algorithm {
            LoadBalancingAlgorithm::RoundRobin => self.round_robin_assign(),
            LoadBalancingAlgorithm::LeastLoaded => self.least_loaded_assign(),
            LoadBalancingAlgorithm::WeightedRoundRobin => self.weighted_round_robin_assign(file_size),
            LoadBalancingAlgorithm::ConsistentHashing => self.consistent_hashing_assign(inode),
            LoadBalancingAlgorithm::Adaptive => self.adaptive_assign(inode, file_size),
        };
        
        // Actualizar información del nodo
        if let Some(node) = self.nodes.get_mut(&node_id) {
            node.used_capacity += file_size;
            node.file_count += 1;
            node.last_update = Self::current_timestamp();
        }
        
        // Registrar asignación
        #[cfg(feature = "std")]
        {
            self.file_assignments.insert(inode, node_id);
        }
        
        #[cfg(not(feature = "std"))]
        {
            let _ = self.file_assignments.insert(inode, node_id);
        }
        
        Ok(node_id)
    }
    
    /// Rebalancear carga entre nodos
    pub fn rebalance(&mut self) -> EclipseFSResult<RebalancingResult> {
        if !self.config.enabled || self.nodes.len() < 2 {
            return Ok(RebalancingResult {
                files_moved: 0,
                nodes_affected: 0,
                load_improvement: 0.0,
                time_taken_ms: 0,
                errors: Vec::new(),
            });
        }
        
        let start_time = Self::current_timestamp();
        let mut files_moved = 0;
        let mut nodes_affected = 0;
        let mut errors = Vec::new();
        
        // Calcular carga actual
        let current_load = self.calculate_system_load();
        
        // Identificar nodos sobrecargados y subcargados
        let (overloaded_nodes, underloaded_nodes) = self.identify_load_imbalance();
        
        // Mover archivos de nodos sobrecargados a subcargados
        for (overloaded_node, underloaded_node) in overloaded_nodes.iter().zip(underloaded_nodes.iter()) {
            if self.operation_count >= self.config.max_operations_per_cycle {
                break;
            }
            
            match self.move_files_between_nodes(*overloaded_node, *underloaded_node) {
                Ok(moved_count) => {
                    files_moved += moved_count;
                    nodes_affected += 2;
                    self.operation_count += 1;
                }
                Err(e) => {
                    errors.push(format!("Error moviendo archivos: {:?}", e));
                }
            }
        }
        
        let end_time = Self::current_timestamp();
        self.last_rebalance_time = end_time;
        
        // Calcular mejora en la carga
        let new_load = self.calculate_system_load();
        let load_improvement = current_load - new_load;
        
        Ok(RebalancingResult {
            files_moved,
            nodes_affected,
            load_improvement,
            time_taken_ms: (end_time - start_time) as u64,
            errors,
        })
    }
    
    /// Algoritmo Round Robin
    fn round_robin_assign(&mut self) -> u32 {
        let node_ids: Vec<u32> = self.nodes.keys().copied().collect();
        if node_ids.is_empty() {
            return 0;
        }
        
        let node_id = node_ids[self.round_robin_counter % node_ids.len()];
        self.round_robin_counter += 1;
        node_id
    }
    
    /// Algoritmo Least Loaded
    fn least_loaded_assign(&self) -> u32 {
        self.nodes
            .values()
            .min_by(|a, b| a.calculate_load_score().partial_cmp(&b.calculate_load_score()).unwrap())
            .map(|node| node.node_id)
            .unwrap_or(0)
    }
    
    /// Algoritmo Weighted Round Robin
    fn weighted_round_robin_assign(&self, file_size: u64) -> u32 {
        let mut best_node = 0u32;
        let mut best_score = f64::MAX;
        
        for node in self.nodes.values() {
            let capacity_remaining = node.total_capacity - node.used_capacity;
            if capacity_remaining >= file_size {
                let score = node.calculate_load_score() + (file_size as f64 / capacity_remaining as f64);
                if score < best_score {
                    best_score = score;
                    best_node = node.node_id;
                }
            }
        }
        
        best_node
    }
    
    /// Algoritmo Consistent Hashing
    fn consistent_hashing_assign(&self, inode: u32) -> u32 {
        let hash = self.hash_inode(inode);
        let node_count = self.nodes.len();
        
        if node_count == 0 {
            return 0;
        }
        
        let node_index = (hash % node_count as u64) as usize;
        let node_ids: Vec<u32> = self.nodes.keys().copied().collect();
        node_ids[node_index]
    }
    
    /// Algoritmo Adaptativo
    fn adaptive_assign(&self, _inode: u32, file_size: u64) -> u32 {
        // Combinar múltiples factores
        let mut best_node = 0u32;
        let mut best_score = f64::MAX;
        
        for node in self.nodes.values() {
            if !node.is_healthy {
                continue;
            }
            
            let capacity_remaining = node.total_capacity - node.used_capacity;
            if capacity_remaining < file_size {
                continue;
            }
            
            let mut score = node.calculate_load_score();
            
            // Factor de tamaño de archivo
            if self.config.consider_file_sizes {
                score += (file_size as f64 / capacity_remaining as f64) * 10.0;
            }
            
            // Factor de fragmentación
            if self.config.consider_fragmentation {
                score += node.fragmentation_level as f64 * 5.0;
            }
            
            // Factor de patrón de acceso
            if self.config.consider_access_patterns {
                score += (node.access_count as f64).ln() * 2.0;
            }
            
            if score < best_score {
                best_score = score;
                best_node = node.node_id;
            }
        }
        
        best_node
    }
    
    /// Calcular carga del sistema
    fn calculate_system_load(&self) -> f64 {
        if self.nodes.is_empty() {
            return 0.0;
        }
        
        let total_load: f64 = self.nodes.values()
            .map(|node| node.calculate_load_score())
            .sum();
        
        total_load / self.nodes.len() as f64
    }
    
    /// Identificar desequilibrio de carga
    fn identify_load_imbalance(&self) -> (Vec<u32>, Vec<u32>) {
        let mut overloaded = Vec::new();
        let mut underloaded = Vec::new();
        
        let avg_load = self.calculate_system_load();
        let threshold = avg_load * (self.config.rebalance_threshold / 100.0) as f64;
        
        for node in self.nodes.values() {
            let load = node.calculate_load_score();
            if load > avg_load + threshold {
                overloaded.push(node.node_id);
            } else if load < avg_load - threshold {
                underloaded.push(node.node_id);
            }
        }
        
        (overloaded, underloaded)
    }
    
    /// Mover archivos entre nodos
    fn move_files_between_nodes(&mut self, from_node: u32, to_node: u32) -> EclipseFSResult<usize> {
        let mut moved_count = 0;
        
        // Encontrar archivos asignados al nodo origen
        let files_to_move: Vec<u32> = self.file_assignments
            .iter()
            .filter(|(_, &node_id)| node_id == from_node)
            .map(|(&inode, _)| inode)
            .collect();
        
        // Mover archivos (simulado)
        for inode in files_to_move {
            if let Some(node_from) = self.nodes.get_mut(&from_node) {
                node_from.file_count = node_from.file_count.saturating_sub(1);
                node_from.last_update = Self::current_timestamp();
            }
            
            if let Some(node_to) = self.nodes.get_mut(&to_node) {
                node_to.file_count += 1;
                node_to.last_update = Self::current_timestamp();
            }
            
            // Actualizar asignación
            if let Some(assignment) = self.file_assignments.get_mut(&inode) {
                *assignment = to_node;
            }
            
            moved_count += 1;
        }
        
        Ok(moved_count)
    }
    
    /// Hash de inode para consistent hashing
    fn hash_inode(&self, inode: u32) -> u64 {
        // Hash simple para demostración
        (inode as u64 * 2654435761) % (2u64.pow(32))
    }
    
    /// Obtener estadísticas de balanceo
    pub fn get_stats(&self) -> LoadBalancingStats {
        LoadBalancingStats {
            total_nodes: self.nodes.len(),
            total_files: self.file_assignments.len(),
            system_load: self.calculate_system_load(),
            operation_count: self.operation_count,
            last_rebalance_time: self.last_rebalance_time,
            config: self.config.clone(),
        }
    }
    
    fn current_timestamp() -> u64 {
        // En un sistema real, esto vendría del kernel o RTC
        1640995200 // 2022-01-01 00:00:00 UTC
    }
}

/// Resultado de rebalanceo
#[derive(Debug, Clone)]
pub struct RebalancingResult {
    pub files_moved: usize,
    pub nodes_affected: usize,
    pub load_improvement: f64,
    pub time_taken_ms: u64,
    pub errors: Vec<String>,
}

/// Estadísticas de balanceo de carga
#[derive(Debug, Clone)]
pub struct LoadBalancingStats {
    pub total_nodes: usize,
    pub total_files: usize,
    pub system_load: f64,
    pub operation_count: usize,
    pub last_rebalance_time: u64,
    pub config: LoadBalancingConfig,
}

impl LoadBalancingStats {
    pub fn print_summary(&self) {
        println!("Load Balancing Stats:");
        println!("  Total Nodes: {}", self.total_nodes);
        println!("  Total Files: {}", self.total_files);
        println!("  System Load: {:.2}", self.system_load);
        println!("  Operations: {}", self.operation_count);
        println!("  Last Rebalance: {}", self.last_rebalance_time);
        println!("  Config: {:?}", self.config);
    }
}
