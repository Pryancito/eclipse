//! Security utilities for EclipseFS
//! 
//! This module provides security-related functions including input validation,
//! sanitization, and cryptographic operations.

use crate::{EclipseFSError, EclipseFSResult};

/// Maximum allowed filename length
pub const MAX_FILENAME_LENGTH: usize = 255;

/// Maximum allowed path length
pub const MAX_PATH_LENGTH: usize = 4096;

/// Maximum allowed file size (16 TB)
pub const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024 * 1024 * 1024;

/// Validate and sanitize a filename
/// 
/// # Security
/// 
/// This function prevents path traversal attacks by rejecting:
/// - Names containing path separators (/, \)
/// - Names containing null bytes
/// - Relative path components (., ..)
/// - Empty names
/// - Names exceeding MAX_FILENAME_LENGTH
/// 
/// # Examples
/// 
/// ```
/// use eclipsefs_lib::security::validate_filename;
/// 
/// assert!(validate_filename("valid_file.txt").is_ok());
/// assert!(validate_filename("../etc/passwd").is_err()); // Path traversal attempt
/// assert!(validate_filename("").is_err()); // Empty name
/// ```
pub fn validate_filename(name: &str) -> EclipseFSResult<()> {
    // Check if empty
    if name.is_empty() {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    // Check length
    if name.len() > MAX_FILENAME_LENGTH {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    // Check for null bytes
    if name.contains('\0') {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    // Check for path separators (prevent path traversal)
    if name.contains('/') || name.contains('\\') {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    // Check for relative path components
    if name == "." || name == ".." {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    // Check for control characters
    if name.chars().any(|c| c.is_control()) {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    Ok(())
}

/// Validate a path
/// 
/// # Security
/// 
/// This function validates a full path by:
/// - Checking total path length
/// - Validating each component separately
/// - Ensuring no path traversal attempts
pub fn validate_path(path: &str) -> EclipseFSResult<()> {
    // Check if empty
    if path.is_empty() {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    // Check total path length
    if path.len() > MAX_PATH_LENGTH {
        return Err(EclipseFSError::InvalidFileName);
    }
    
    // Split path and validate each component
    for component in path.split('/') {
        // Skip empty components (from leading/trailing slashes or //)
        if component.is_empty() {
            continue;
        }
        
        validate_filename(component)?;
    }
    
    Ok(())
}

/// Validate an inode number
/// 
/// # Security
/// 
/// Ensures inode numbers are within valid range to prevent:
/// - Integer overflow
/// - Out-of-bounds access
pub fn validate_inode(inode: u32, max_inode: u32) -> EclipseFSResult<()> {
    if inode == 0 || inode > max_inode {
        return Err(EclipseFSError::NotFound);
    }
    Ok(())
}

/// Validate file size
/// 
/// # Security
/// 
/// Prevents integer overflow in size calculations
pub fn validate_file_size(size: u64) -> EclipseFSResult<()> {
    if size > MAX_FILE_SIZE {
        return Err(EclipseFSError::FileTooLarge);
    }
    Ok(())
}

/// Validate block number
/// 
/// # Security
/// 
/// Ensures block numbers are within filesystem bounds
pub fn validate_block_number(block: u64, total_blocks: u64) -> EclipseFSResult<()> {
    if block >= total_blocks {
        return Err(EclipseFSError::InvalidOperation);
    }
    Ok(())
}

/// Constant-time comparison for security-sensitive data
/// 
/// # Security
/// 
/// Uses constant-time comparison to prevent timing attacks
/// when comparing checksums or cryptographic values
pub fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    
    result == 0
}

/// Validate checksum with constant-time comparison
/// 
/// # Security
/// 
/// Uses constant-time comparison to prevent timing attacks
pub fn validate_checksum(computed: u32, expected: u32) -> bool {
    let a = computed.to_le_bytes();
    let b = expected.to_le_bytes();
    constant_time_compare(&a, &b)
}

/// Check for integer overflow in size calculations
/// 
/// # Security
/// 
/// Safely performs addition with overflow check
pub fn checked_add_size(a: u64, b: u64) -> EclipseFSResult<u64> {
    a.checked_add(b).ok_or(EclipseFSError::FileTooLarge)
}

/// Check for integer overflow in multiplication
/// 
/// # Security
/// 
/// Safely performs multiplication with overflow check
pub fn checked_mul_size(a: u64, b: u64) -> EclipseFSResult<u64> {
    a.checked_mul(b).ok_or(EclipseFSError::FileTooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_filename_valid() {
        assert!(validate_filename("file.txt").is_ok());
        assert!(validate_filename("my_document").is_ok());
        assert!(validate_filename("test-123").is_ok());
    }

    #[test]
    fn test_validate_filename_empty() {
        assert!(validate_filename("").is_err());
    }

    #[test]
    fn test_validate_filename_path_traversal() {
        assert!(validate_filename("..").is_err());
        assert!(validate_filename(".").is_err());
        assert!(validate_filename("../etc/passwd").is_err());
        assert!(validate_filename("dir/file").is_err());
        assert!(validate_filename("dir\\file").is_err());
    }

    #[test]
    fn test_validate_filename_null_byte() {
        assert!(validate_filename("file\0name").is_err());
    }

    #[test]
    fn test_validate_filename_too_long() {
        let long_name = "a".repeat(300);
        assert!(validate_filename(&long_name).is_err());
    }

    #[test]
    fn test_validate_filename_control_chars() {
        assert!(validate_filename("file\nname").is_err());
        assert!(validate_filename("file\rname").is_err());
    }

    #[test]
    fn test_validate_path_valid() {
        assert!(validate_path("/home/user/file.txt").is_ok());
        assert!(validate_path("home/user/file.txt").is_ok());
        assert!(validate_path("/").is_ok());
    }

    #[test]
    fn test_validate_path_traversal() {
        assert!(validate_path("/home/../etc/passwd").is_err());
        assert!(validate_path("home/./file").is_err());
    }

    #[test]
    fn test_constant_time_compare() {
        assert!(constant_time_compare(b"hello", b"hello"));
        assert!(!constant_time_compare(b"hello", b"world"));
        assert!(!constant_time_compare(b"hello", b"hel"));
    }

    #[test]
    fn test_validate_checksum() {
        assert!(validate_checksum(0x12345678, 0x12345678));
        assert!(!validate_checksum(0x12345678, 0x12345679));
    }

    #[test]
    fn test_checked_add_size() {
        assert_eq!(checked_add_size(100, 200).unwrap(), 300);
        assert!(checked_add_size(u64::MAX, 1).is_err());
    }

    #[test]
    fn test_checked_mul_size() {
        assert_eq!(checked_mul_size(10, 20).unwrap(), 200);
        assert!(checked_mul_size(u64::MAX, 2).is_err());
    }

    #[test]
    fn test_validate_file_size() {
        assert!(validate_file_size(1024).is_ok());
        assert!(validate_file_size(MAX_FILE_SIZE).is_ok());
        assert!(validate_file_size(MAX_FILE_SIZE + 1).is_err());
    }
}
