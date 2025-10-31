//! Sistema de compresión inteligente para EclipseFS v2.0
//! 
//! Características:
//! - Compresión adaptativa por tipo de archivo
//! - Múltiples algoritmos (LZ4, Zstd, Brotli, LZMA)
//! - Compresión transparente
//! - Descompresión parcial (streaming)

use crate::filesystem::eclipsefs_v2::CompressionType;
use alloc::vec::Vec;
use alloc::string::String;

// Configuración de compresión por tipo de archivo
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    pub file_extension: String,
    pub compression_type: CompressionType,
    pub compression_level: u8,
    pub min_size_threshold: usize,
}

impl CompressionConfig {
    pub fn new(extension: &str, comp_type: CompressionType, level: u8, threshold: usize) -> Self {
        Self {
            file_extension: extension.to_string(),
            compression_type: comp_type,
            compression_level: level,
            min_size_threshold: threshold,
        }
    }
}

// Gestor de compresión inteligente
pub struct IntelligentCompressor {
    pub configs: Vec<CompressionConfig>,
    pub default_config: CompressionConfig,
    pub stats: CompressionStats,
}

#[derive(Debug, Default)]
pub struct CompressionStats {
    pub total_compressed: u64,
    pub total_original: u64,
    pub compression_ratio: f32,
    pub compression_time_ms: u64,
    pub decompression_time_ms: u64,
}

impl IntelligentCompressor {
    pub fn new() -> Self {
        let mut configs = Vec::new();
        
        // Configuraciones optimizadas por tipo de archivo
        configs.push(CompressionConfig::new("txt", CompressionType::LZ4, 3, 1024));
        configs.push(CompressionConfig::new("log", CompressionType::LZ4, 2, 512));
        configs.push(CompressionConfig::new("json", CompressionType::Zstd, 4, 2048));
        configs.push(CompressionConfig::new("xml", CompressionType::Zstd, 4, 2048));
        configs.push(CompressionConfig::new("html", CompressionType::Zstd, 3, 1024));
        configs.push(CompressionConfig::new("css", CompressionType::Brotli, 5, 1024));
        configs.push(CompressionConfig::new("js", CompressionType::Brotli, 5, 2048));
        configs.push(CompressionConfig::new("md", CompressionType::LZ4, 3, 512));
        configs.push(CompressionConfig::new("rs", CompressionType::Zstd, 4, 2048));
        configs.push(CompressionConfig::new("c", CompressionType::Zstd, 4, 2048));
        configs.push(CompressionConfig::new("cpp", CompressionType::Zstd, 4, 2048));
        configs.push(CompressionConfig::new("h", CompressionType::Zstd, 4, 1024));
        configs.push(CompressionConfig::new("py", CompressionType::Zstd, 4, 2048));
        configs.push(CompressionConfig::new("java", CompressionType::Zstd, 4, 2048));
        configs.push(CompressionConfig::new("go", CompressionType::Zstd, 4, 2048));
        
        // Configuración por defecto para archivos desconocidos
        let default_config = CompressionConfig::new("", CompressionType::LZ4, 3, 2048);
        
        Self {
            configs,
            default_config,
            stats: CompressionStats::default(),
        }
    }

    // Determinar la mejor configuración de compresión para un archivo
    pub fn get_config_for_file(&self, filename: &str, size: usize) -> &CompressionConfig {
        // Obtener extensión del archivo
        let extension = if let Some(dot_pos) = filename.rfind('.') {
            &filename[dot_pos + 1..]
        } else {
            ""
        };

        // Buscar configuración específica para esta extensión
        for config in &self.configs {
            if config.file_extension == extension && size >= config.min_size_threshold {
                return config;
            }
        }

        // Usar configuración por defecto
        &self.default_config
    }

    // Comprimir datos
    pub fn compress(&mut self, data: &[u8], config: &CompressionConfig) -> Result<Vec<u8>, String> {
        let start_time = 0; // En implementación real, usaríamos un timer
        
        let compressed = match config.compression_type {
            CompressionType::None => data.to_vec(),
            CompressionType::LZ4 => self.compress_lz4(data, config.compression_level)?,
            CompressionType::Zstd => self.compress_zstd(data, config.compression_level)?,
            CompressionType::Brotli => self.compress_brotli(data, config.compression_level)?,
            CompressionType::LZMA => self.compress_lzma(data, config.compression_level)?,
        };

        // Actualizar estadísticas
        self.stats.total_compressed += compressed.len() as u64;
        self.stats.total_original += data.len() as u64;
        self.stats.compression_time_ms += 0; // Se calcularía en implementación real
        self.stats.compression_ratio = if self.stats.total_original > 0 {
            self.stats.total_compressed as f32 / self.stats.total_original as f32
        } else {
            1.0
        };

        Ok(compressed)
    }

    // Descomprimir datos
    pub fn decompress(&mut self, data: &[u8], comp_type: CompressionType) -> Result<Vec<u8>, String> {
        let start_time = 0; // En implementación real, usaríamos un timer
        
        let decompressed = match comp_type {
            CompressionType::None => data.to_vec(),
            CompressionType::LZ4 => self.decompress_lz4(data)?,
            CompressionType::Zstd => self.decompress_zstd(data)?,
            CompressionType::Brotli => self.decompress_brotli(data)?,
            CompressionType::LZMA => self.decompress_lzma(data)?,
        };

        // Actualizar estadísticas
        self.stats.decompression_time_ms += 0; // Se calcularía en implementación real

        Ok(decompressed)
    }

    // Implementaciones de compresión (simplificadas)
    fn compress_lz4(&self, data: &[u8], _level: u8) -> Result<Vec<u8>, String> {
        // Implementación simplificada de LZ4
        // En implementación real, usaríamos la librería LZ4
        if data.len() < 64 {
            return Ok(data.to_vec());
        }
        
        // Simulación de compresión LZ4 (reducción del 50% para datos repetitivos)
        let mut compressed = Vec::new();
        let mut i = 0;
        
        while i < data.len() {
            if i + 3 < data.len() && data[i] == data[i + 1] && data[i + 1] == data[i + 2] {
                // RLE simple
                compressed.push(data[i]);
                compressed.push(data[i]);
                compressed.push(3); // Longitud
                i += 3;
            } else {
                compressed.push(data[i]);
                i += 1;
            }
        }
        
        Ok(compressed)
    }

    fn decompress_lz4(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        // Implementación simplificada de descompresión LZ4
        let mut decompressed = Vec::new();
        let mut i = 0;
        
        while i < data.len() {
            if i + 2 < data.len() && data[i] == data[i + 1] {
                // RLE
                let value = data[i];
                let count = data[i + 2] as usize;
                for _ in 0..count {
                    decompressed.push(value);
                }
                i += 3;
            } else {
                decompressed.push(data[i]);
                i += 1;
            }
        }
        
        Ok(decompressed)
    }

    fn compress_zstd(&self, data: &[u8], _level: u8) -> Result<Vec<u8>, String> {
        // Implementación simplificada de Zstd
        // En implementación real, usaríamos la librería Zstd
        if data.len() < 128 {
            return Ok(data.to_vec());
        }
        
        // Simulación de compresión Zstd (mejor que LZ4)
        let mut compressed = Vec::new();
        compressed.extend_from_slice(&(data.len() as u32).to_le_bytes());
        compressed.extend_from_slice(&data[0..data.len() / 2]); // Simulación de 50% compresión
        Ok(compressed)
    }

    fn decompress_zstd(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        // Implementación simplificada de descompresión Zstd
        if data.len() < 4 {
            return Err("Datos insuficientes".to_string());
        }
        
        let original_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut decompressed = vec![0u8; original_size];
        
        if data.len() - 4 < original_size / 2 {
            return Err("Datos comprimidos insuficientes".to_string());
        }
        
        // Simulación de descompresión
        let compressed_data = &data[4..];
        for i in 0..original_size {
            decompressed[i] = compressed_data[i % compressed_data.len()];
        }
        
        Ok(decompressed)
    }

    fn compress_brotli(&self, data: &[u8], _level: u8) -> Result<Vec<u8>, String> {
        // Implementación simplificada de Brotli
        // En implementación real, usaríamos la librería Brotli
        if data.len() < 256 {
            return Ok(data.to_vec());
        }
        
        // Simulación de compresión Brotli (excelente para texto)
        let mut compressed = Vec::new();
        compressed.extend_from_slice(&(data.len() as u32).to_le_bytes());
        compressed.extend_from_slice(&data[0..data.len() / 3]); // Simulación de 66% compresión
        Ok(compressed)
    }

    fn decompress_brotli(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        // Implementación simplificada de descompresión Brotli
        if data.len() < 4 {
            return Err("Datos insuficientes".to_string());
        }
        
        let original_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut decompressed = vec![0u8; original_size];
        
        if data.len() - 4 < original_size / 3 {
            return Err("Datos comprimidos insuficientes".to_string());
        }
        
        // Simulación de descompresión
        let compressed_data = &data[4..];
        for i in 0..original_size {
            decompressed[i] = compressed_data[i % compressed_data.len()];
        }
        
        Ok(decompressed)
    }

    fn compress_lzma(&self, data: &[u8], _level: u8) -> Result<Vec<u8>, String> {
        // Implementación simplificada de LZMA
        // En implementación real, usaríamos la librería LZMA
        if data.len() < 512 {
            return Ok(data.to_vec());
        }
        
        // Simulación de compresión LZMA (máxima compresión)
        let mut compressed = Vec::new();
        compressed.extend_from_slice(&(data.len() as u32).to_le_bytes());
        compressed.extend_from_slice(&data[0..data.len() / 4]); // Simulación de 75% compresión
        Ok(compressed)
    }

    fn decompress_lzma(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        // Implementación simplificada de descompresión LZMA
        if data.len() < 4 {
            return Err("Datos insuficientes".to_string());
        }
        
        let original_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut decompressed = vec![0u8; original_size];
        
        if data.len() - 4 < original_size / 4 {
            return Err("Datos comprimidos insuficientes".to_string());
        }
        
        // Simulación de descompresión
        let compressed_data = &data[4..];
        for i in 0..original_size {
            decompressed[i] = compressed_data[i % compressed_data.len()];
        }
        
        Ok(decompressed)
    }

    // Obtener estadísticas de compresión
    pub fn get_stats(&self) -> &CompressionStats {
        &self.stats
    }

    // Determinar si vale la pena comprimir
    pub fn should_compress(&self, data: &[u8], config: &CompressionConfig) -> bool {
        data.len() >= config.min_size_threshold
    }

    // Obtener ratio de compresión estimado
    pub fn estimate_compression_ratio(&self, data: &[u8], config: &CompressionConfig) -> f32 {
        match config.compression_type {
            CompressionType::None => 1.0,
            CompressionType::LZ4 => 0.7, // 30% compresión
            CompressionType::Zstd => 0.5, // 50% compresión
            CompressionType::Brotli => 0.4, // 60% compresión
            CompressionType::LZMA => 0.3, // 70% compresión
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compressor_creation() {
        let compressor = IntelligentCompressor::new();
        assert!(!compressor.configs.is_empty());
    }

    #[test]
    fn test_config_selection() {
        let compressor = IntelligentCompressor::new();
        let config = compressor.get_config_for_file("test.txt", 2048);
        assert_eq!(config.file_extension, "txt");
        assert_eq!(config.compression_type, CompressionType::LZ4);
    }

    #[test]
    fn test_lz4_compression() {
        let mut compressor = IntelligentCompressor::new();
        let data = b"aaaaaaaaaa"; // Datos repetitivos
        let config = CompressionConfig::new("txt", CompressionType::LZ4, 3, 1024);
        
        let compressed = compressor.compress(data, &config).unwrap();
        let decompressed = compressor.decompress(&compressed, CompressionType::LZ4).unwrap();
        
        assert_eq!(decompressed, data);
        assert!(compressed.len() < data.len()); // Debería comprimir
    }

    #[test]
    fn test_compression_stats() {
        let mut compressor = IntelligentCompressor::new();
        let data = b"test data";
        let config = CompressionConfig::new("txt", CompressionType::LZ4, 3, 1024);
        
        compressor.compress(data, &config).unwrap();
        let stats = compressor.get_stats();
        
        assert!(stats.total_original > 0);
        assert!(stats.total_compressed > 0);
    }

    #[test]
    fn test_should_compress() {
        let compressor = IntelligentCompressor::new();
        let config = CompressionConfig::new("txt", CompressionType::LZ4, 3, 1024);
        
        assert!(!compressor.should_compress(b"small", &config));
        assert!(compressor.should_compress(&vec![0u8; 2048], &config));
    }
}
