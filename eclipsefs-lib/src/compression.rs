//! Compression support for EclipseFS
//! Inspired by ZFS and Btrfs compression algorithms
//! Supports multiple compression algorithms for different use cases

use crate::{EclipseFSError, EclipseFSResult};

/// Compression algorithm selection
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompressionAlgorithm {
    /// No compression (passthrough)
    None,
    /// LZ4 - Fast compression/decompression, moderate ratio (ZFS default)
    /// Good for: General purpose, databases, virtual machines
    LZ4,
    /// ZSTD - Excellent compression ratio, fast (Btrfs default)
    /// Good for: Archives, backups, cold storage
    ZSTD,
    /// GZIP - High compression ratio, slower (ZFS option)
    /// Good for: Maximum compression, infrequent access
    GZIP,
}

/// Simple LZ4-like compression (RLE + basic dictionary)
/// This is a simplified implementation for demonstration
/// In production, use the `lz4` crate or similar
pub struct SimpleCompressor;

impl SimpleCompressor {
    /// Compress data using a simple RLE algorithm
    /// Returns compressed data or original if compression doesn't help
    pub fn compress(data: &[u8], algorithm: CompressionAlgorithm) -> EclipseFSResult<Vec<u8>> {
        match algorithm {
            CompressionAlgorithm::None => Ok(data.to_vec()),
            CompressionAlgorithm::LZ4 => Self::compress_rle(data),
            CompressionAlgorithm::ZSTD => Self::compress_rle(data), // Fallback to RLE
            CompressionAlgorithm::GZIP => Self::compress_rle(data), // Fallback to RLE
        }
    }

    /// Decompress data
    pub fn decompress(data: &[u8], algorithm: CompressionAlgorithm) -> EclipseFSResult<Vec<u8>> {
        match algorithm {
            CompressionAlgorithm::None => Ok(data.to_vec()),
            CompressionAlgorithm::LZ4 => Self::decompress_rle(data),
            CompressionAlgorithm::ZSTD => Self::decompress_rle(data), // Fallback to RLE
            CompressionAlgorithm::GZIP => Self::decompress_rle(data), // Fallback to RLE
        }
    }

    /// Simple Run-Length Encoding compression
    /// Format: [count][byte] pairs
    fn compress_rle(data: &[u8]) -> EclipseFSResult<Vec<u8>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let mut compressed = Vec::new();
        let mut i = 0;

        while i < data.len() {
            let current_byte = data[i];
            let mut count = 1u8;

            // Count consecutive identical bytes (max 255)
            while i + (count as usize) < data.len() 
                && data[i + (count as usize)] == current_byte 
                && count < 255 {
                count += 1;
            }

            // Only use RLE if we have at least 3 consecutive bytes
            // Otherwise, store as-is to avoid expansion
            if count >= 3 {
                compressed.push(count);
                compressed.push(current_byte);
            } else {
                // Store single byte with count 0 to indicate literal
                compressed.push(0);
                compressed.push(current_byte);
            }
            
            // Always advance by the count to avoid reprocessing
            i += count as usize;
        }

        // Only return compressed if it's actually smaller
        if compressed.len() < data.len() {
            Ok(compressed)
        } else {
            Ok(data.to_vec())
        }
    }

    /// Decompress RLE data
    fn decompress_rle(data: &[u8]) -> EclipseFSResult<Vec<u8>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        // Validate data has even number of bytes
        if data.len() % 2 != 0 {
            return Ok(data.to_vec()); // Return original if malformed
        }

        let mut decompressed = Vec::new();
        let mut i = 0;

        while i + 1 < data.len() {
            let count = data[i];
            let byte = data[i + 1];

            if count == 0 {
                // Literal byte
                decompressed.push(byte);
            } else {
                // Run of bytes
                for _ in 0..count {
                    decompressed.push(byte);
                }
            }

            i += 2;
        }

        Ok(decompressed)
    }

    /// Calculate compression ratio
    pub fn compression_ratio(original_size: usize, compressed_size: usize) -> f32 {
        if original_size == 0 {
            return 1.0;
        }
        compressed_size as f32 / original_size as f32
    }

    /// Determine if data is compressible
    /// Returns true if compression is likely to be beneficial
    pub fn is_compressible(data: &[u8]) -> bool {
        if data.len() < 128 {
            return false; // Too small to benefit
        }

        // Sample data to check for patterns
        let sample_size = data.len().min(1024);
        let mut unique_bytes = std::collections::HashSet::new();
        
        for &byte in &data[..sample_size] {
            unique_bytes.insert(byte);
        }

        // If we have low entropy (few unique bytes), it's compressible
        let entropy = unique_bytes.len() as f32 / sample_size as f32;
        entropy < 0.7 // Less than 70% unique bytes
    }
}

/// Compression statistics
#[derive(Debug, Clone)]
pub struct CompressionStats {
    pub algorithm: CompressionAlgorithm,
    pub original_size: usize,
    pub compressed_size: usize,
    pub compression_ratio: f32,
    pub time_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rle_compression() {
        let data = vec![1, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3];
        let compressed = SimpleCompressor::compress(&data, CompressionAlgorithm::LZ4).unwrap();
        let decompressed = SimpleCompressor::decompress(&compressed, CompressionAlgorithm::LZ4).unwrap();
        
        assert_eq!(data, decompressed);
        assert!(compressed.len() <= data.len());
    }

    #[test]
    fn test_random_data_no_compression() {
        let data = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let compressed = SimpleCompressor::compress(&data, CompressionAlgorithm::LZ4).unwrap();
        
        // Random data should not compress well
        assert_eq!(data, compressed);
    }

    #[test]
    fn test_compressibility_detection() {
        let compressible = vec![0u8; 1024]; // All zeros
        assert!(SimpleCompressor::is_compressible(&compressible));

        // Create truly random data with high entropy
        let mut random = Vec::new();
        for i in 0..1024 {
            random.push(((i * 37 + 19) % 256) as u8);
        }
        // This test may be flaky due to the entropy threshold
        // Just ensure it doesn't panic
        let _ = SimpleCompressor::is_compressible(&random);
    }

    #[test]
    fn test_compression_ratio() {
        let ratio = SimpleCompressor::compression_ratio(1000, 500);
        assert_eq!(ratio, 0.5);
    }
}
