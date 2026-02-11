//! Sistema de defragmentación para EclipseFS (inspirado en RedoxFS)

#[cfg(feature = "std")]
use std::collections::HashMap;

#[cfg(not(feature = "std"))]
use heapless::FnvIndexMap;

use crate::EclipseFSResult;

/// Configuración de defragmentación
#[derive(Debug, Clone)]
pub struct DefragmentationConfig {
    pub enabled: bool,
    pub threshold_percentage: f32,    // Porcentaje de fragmentación para activar
    pub max_operations_per_cycle: usize,
    pub background_mode: bool,
    pub optimize_small_files: bool,
    pub optimize_large_files: bool,
    pub preserve_access_patterns: bool,
}

impl Default for DefragmentationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_percentage: 30.0,
            max_operations_per_cycle: 100,
            background_mode: true,
            optimize_small_files: true,
            optimize_large_files: false,
            preserve_access_patterns: true,
        }
    }
}

/// Información de fragmentación de un archivo
#[derive(Debug, Clone)]
pub struct FileFragmentationInfo {
    pub inode: u32,
    pub total_size: u64,
    pub fragment_count: usize,
    pub fragments: Vec<Fragment>,
    pub fragmentation_percentage: f32,
    pub access_frequency: u32,
    pub last_access: u64,
}

/// Fragmento de archivo
#[derive(Debug, Clone)]
pub struct Fragment {
    pub offset: u64,
    pub size: u64,
    pub block_id: u64,
    pub is_contiguous: bool,
}

/// Sistema de defragmentación inteligente (inspirado en RedoxFS)
#[cfg(feature = "std")]
pub struct IntelligentDefragmenter {
    config: DefragmentationConfig,
    fragmentation_map: HashMap<u32, FileFragmentationInfo>,
    free_blocks: Vec<u64>,
    allocated_blocks: HashMap<u64, u32>,
    operation_count: usize,
    last_defrag_time: u64,
}

#[cfg(not(feature = "std"))]
pub struct IntelligentDefragmenter {
    config: DefragmentationConfig,
    fragmentation_map: FnvIndexMap<u32, FileFragmentationInfo, 512>,
    free_blocks: heapless::Vec<u64, 1024>,
    allocated_blocks: FnvIndexMap<u64, u32, 1024>,
    operation_count: usize,
    last_defrag_time: u64,
}

impl IntelligentDefragmenter {
    pub fn new(config: DefragmentationConfig) -> Self {
        Self {
            config,
            #[cfg(feature = "std")]
            fragmentation_map: HashMap::new(),
            #[cfg(not(feature = "std"))]
            fragmentation_map: FnvIndexMap::new(),
            #[cfg(feature = "std")]
            free_blocks: Vec::new(),
            #[cfg(not(feature = "std"))]
            free_blocks: heapless::Vec::new(),
            #[cfg(feature = "std")]
            allocated_blocks: HashMap::new(),
            #[cfg(not(feature = "std"))]
            allocated_blocks: FnvIndexMap::new(),
            operation_count: 0,
            last_defrag_time: Self::current_timestamp(),
        }
    }
    
    /// Analizar fragmentación del sistema de archivos
    pub fn analyze_fragmentation(&mut self, files: &[(u32, u64, Vec<Fragment>)]) -> EclipseFSResult<FragmentationReport> {
        let mut total_files = 0;
        let mut fragmented_files = 0;
        let mut total_fragments = 0;
        let mut total_size = 0u64;
        
        for (inode, size, fragments) in files {
            total_files += 1;
            total_size += size;
            
            if fragments.len() > 1 {
                fragmented_files += 1;
                total_fragments += fragments.len();
            }
            
            let fragmentation_percentage = if fragments.len() > 1 {
                ((fragments.len() - 1) as f32 / fragments.len() as f32) * 100.0
            } else {
                0.0
            };
            
            let info = FileFragmentationInfo {
                inode: *inode,
                total_size: *size,
                fragment_count: fragments.len(),
                fragments: fragments.clone(),
                fragmentation_percentage,
                access_frequency: 1, // Simulado
                last_access: Self::current_timestamp(),
            };
            
            #[cfg(feature = "std")]
            {
                self.fragmentation_map.insert(*inode, info);
            }
            
            #[cfg(not(feature = "std"))]
            {
                let _ = self.fragmentation_map.insert(*inode, info);
            }
        }
        
        let overall_fragmentation = if total_files > 0 {
            (fragmented_files as f32 / total_files as f32) * 100.0
        } else {
            0.0
        };
        
        Ok(FragmentationReport {
            total_files,
            fragmented_files,
            total_fragments,
            total_size,
            overall_fragmentation,
            average_fragments_per_file: if total_files > 0 {
                total_fragments as f32 / total_files as f32
            } else {
                0.0
            },
        })
    }
    
    /// Ejecutar defragmentación (inspirado en RedoxFS)
    pub fn defragment(&mut self) -> EclipseFSResult<DefragmentationResult> {
        if !self.config.enabled {
            return Ok(DefragmentationResult {
                files_processed: 0,
                fragments_consolidated: 0,
                space_freed: 0,
                time_taken_ms: 0,
                errors: Vec::new(),
            });
        }
        
        let start_time = Self::current_timestamp();
        let mut files_processed = 0;
        let mut fragments_consolidated = 0;
        let mut space_freed = 0u64;
        let mut errors = Vec::new();
        
        // Obtener archivos más fragmentados primero
        let mut candidates: Vec<_> = self.fragmentation_map.iter().map(|(k, v)| (*k, v.clone())).collect();
        candidates.sort_by(|a, b| {
            b.1.fragmentation_percentage.partial_cmp(&a.1.fragmentation_percentage).unwrap()
        });
        
        for (inode, info) in candidates {
            if self.operation_count >= self.config.max_operations_per_cycle {
                break;
            }
            
            // Verificar si el archivo cumple criterios de optimización
            if self.should_defragment(&info) {
                match self.defragment_file(inode, &info) {
                    Ok(result) => {
                        files_processed += 1;
                        fragments_consolidated += result.fragments_consolidated;
                        space_freed += result.space_freed;
                        self.operation_count += 1;
                    }
                    Err(e) => {
                        errors.push(format!("Error defragmentando inode {}: {:?}", inode, e));
                    }
                }
            }
        }
        
        let end_time = Self::current_timestamp();
        self.last_defrag_time = end_time;
        
        Ok(DefragmentationResult {
            files_processed,
            fragments_consolidated,
            space_freed,
            time_taken_ms: end_time - start_time,
            errors,
        })
    }
    
    /// Determinar si un archivo debe ser defragmentado
    fn should_defragment(&self, info: &FileFragmentationInfo) -> bool {
        // Criterios de defragmentación
        let is_fragmented = info.fragmentation_percentage > self.config.threshold_percentage;
        let is_small_file = info.total_size < 1024 * 1024; // < 1MB
        let is_large_file = info.total_size > 10 * 1024 * 1024; // > 10MB
        
        let size_criteria = match (is_small_file, is_large_file) {
            (true, false) => self.config.optimize_small_files,
            (false, true) => self.config.optimize_large_files,
            _ => true,
        };
        
        is_fragmented && size_criteria
    }
    
    /// Defragmentar un archivo específico
    fn defragment_file(&mut self, inode: u32, info: &FileFragmentationInfo) -> EclipseFSResult<FileDefragmentationResult> {
        if info.fragment_count <= 1 {
            return Ok(FileDefragmentationResult {
                fragments_consolidated: 0,
                space_freed: 0,
                new_fragments: info.fragments.clone(),
            });
        }
        
        // Encontrar bloque contiguo libre
        let contiguous_size = info.total_size;
        let start_block = self.find_contiguous_free_blocks(contiguous_size)?;
        
        // Simular consolidación de fragmentos
        let new_fragment = Fragment {
            offset: 0,
            size: contiguous_size,
            block_id: start_block,
            is_contiguous: true,
        };
        
        // Liberar bloques antiguos
        let mut space_freed = 0u64;
        for fragment in &info.fragments {
            if let Some(old_inode) = self.allocated_blocks.remove(&fragment.block_id) {
                if old_inode == inode {
                    space_freed += fragment.size;
                    self.free_blocks.push(fragment.block_id);
                }
            }
        }
        
        // Asignar nuevo bloque
        self.allocated_blocks.insert(start_block, inode);
        
        Ok(FileDefragmentationResult {
            fragments_consolidated: info.fragment_count - 1,
            space_freed,
            new_fragments: vec![new_fragment],
        })
    }
    
    /// Encontrar bloques contiguos libres
    fn find_contiguous_free_blocks(&self, _size: u64) -> EclipseFSResult<u64> {
        // Simular búsqueda de bloques contiguos
        // En un sistema real, esto buscaría en el mapa de bloques libres
        Ok(1000) // Bloque simulado
    }
    
    /// Obtener estadísticas de defragmentación
    pub fn get_stats(&self) -> DefragmentationStats {
        let total_files = self.fragmentation_map.len();
        let fragmented_files = self.fragmentation_map.values()
            .filter(|info| info.fragment_count > 1)
            .count();
        
        DefragmentationStats {
            total_files,
            fragmented_files,
            operation_count: self.operation_count,
            last_defrag_time: self.last_defrag_time,
            config: self.config.clone(),
        }
    }
    
    fn current_timestamp() -> u64 {
        // En un sistema real, esto vendría del kernel o RTC
        1640995200 // 2022-01-01 00:00:00 UTC
    }
}

/// Reporte de fragmentación
#[derive(Debug, Clone)]
pub struct FragmentationReport {
    pub total_files: usize,
    pub fragmented_files: usize,
    pub total_fragments: usize,
    pub total_size: u64,
    pub overall_fragmentation: f32,
    pub average_fragments_per_file: f32,
}

/// Resultado de defragmentación
#[derive(Debug, Clone)]
pub struct DefragmentationResult {
    pub files_processed: usize,
    pub fragments_consolidated: usize,
    pub space_freed: u64,
    pub time_taken_ms: u64,
    pub errors: Vec<String>,
}

/// Resultado de defragmentación de archivo
#[derive(Debug, Clone)]
pub struct FileDefragmentationResult {
    pub fragments_consolidated: usize,
    pub space_freed: u64,
    pub new_fragments: Vec<Fragment>,
}

/// Estadísticas de defragmentación
#[derive(Debug, Clone)]
pub struct DefragmentationStats {
    pub total_files: usize,
    pub fragmented_files: usize,
    pub operation_count: usize,
    pub last_defrag_time: u64,
    pub config: DefragmentationConfig,
}

impl DefragmentationStats {
    pub fn print_summary(&self) {
        println!("Defragmentation Stats:");
        println!("  Total Files: {}", self.total_files);
        println!("  Fragmented Files: {}", self.fragmented_files);
        println!("  Operations: {}", self.operation_count);
        println!("  Last Defrag: {}", self.last_defrag_time);
        println!("  Config: {:?}", self.config);
    }
}
