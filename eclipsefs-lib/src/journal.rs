//! Journaling system for EclipseFS (crash recovery)
//! Inspired by ext4's journal and RedoxFS transaction log

#[cfg(feature = "std")]
use std::collections::VecDeque;
#[cfg(feature = "std")]
use std::io::{Read, Write};
#[cfg(feature = "std")]
use std::fs::File;

#[cfg(not(feature = "std"))]
use heapless::{Vec as HeaplessVec, Deque};

use crate::{EclipseFSError, EclipseFSResult};

/// Transaction types in the journal
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransactionType {
    CreateFile,
    CreateDirectory,
    DeleteFile,
    DeleteDirectory,
    WriteData,
    UpdateMetadata,
    CreateSnapshot,
    DeleteSnapshot,
}

/// Journal entry representing a filesystem operation
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub transaction_id: u64,
    pub transaction_type: TransactionType,
    pub inode: u32,
    pub parent_inode: u32,
    pub timestamp: u64,
    pub data_size: usize,
    #[cfg(feature = "std")]
    pub data: Vec<u8>,
    #[cfg(not(feature = "std"))]
    pub data: HeaplessVec<u8, 256>,
    pub checksum: u32,
}

impl JournalEntry {
    /// Create a new journal entry
    pub fn new(transaction_type: TransactionType, inode: u32, parent_inode: u32) -> Self {
        let mut entry = Self {
            transaction_id: 0, // Will be assigned by journal
            transaction_type,
            inode,
            parent_inode,
            timestamp: Self::current_time(),
            data_size: 0,
            #[cfg(feature = "std")]
            data: Vec::new(),
            #[cfg(not(feature = "std"))]
            data: HeaplessVec::new(),
            checksum: 0,
        };
        // Set correct checksum for empty data
        entry.checksum = Self::calculate_checksum(&[]);
        entry
    }

    /// Add data to the journal entry
    pub fn with_data(mut self, data: &[u8]) -> EclipseFSResult<Self> {
        #[cfg(feature = "std")]
        {
            self.data = data.to_vec();
            self.data_size = data.len();
        }
        
        #[cfg(not(feature = "std"))]
        {
            if data.len() > 256 {
                return Err(EclipseFSError::FileTooLarge);
            }
            self.data.extend_from_slice(data)
                .map_err(|_| EclipseFSError::OutOfMemory)?;
            self.data_size = data.len();
        }
        
        self.checksum = Self::calculate_checksum(&self.data);
        Ok(self)
    }

    /// Calculate CRC32 checksum
    fn calculate_checksum(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFFFFFF;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xEDB88320;
                } else {
                    crc >>= 1;
                }
            }
        }
        crc ^ 0xFFFFFFFF
    }

    /// Verify checksum
    pub fn verify_checksum(&self) -> bool {
        Self::calculate_checksum(&self.data) == self.checksum
    }

    /// Get current timestamp (stub for now)
    fn current_time() -> u64 {
        1640995200 // 2022-01-01 00:00:00 UTC
    }
}

/// Configuration for the journal system
#[derive(Debug, Clone)]
pub struct JournalConfig {
    pub max_entries: usize,
    pub auto_commit: bool,
    pub commit_interval_ms: u64,
    pub recovery_enabled: bool,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            auto_commit: true,
            commit_interval_ms: 5000,
            recovery_enabled: true,
        }
    }
}

/// Journal system for crash recovery
#[cfg(feature = "std")]
pub struct Journal {
    entries: VecDeque<JournalEntry>,
    next_transaction_id: u64,
    config: JournalConfig,
    committed_count: u64,
    uncommitted_count: u64,
}

#[cfg(not(feature = "std"))]
pub struct Journal {
    entries: Deque<JournalEntry, 64>,
    next_transaction_id: u64,
    config: JournalConfig,
    committed_count: u64,
    uncommitted_count: u64,
}

impl Journal {
    /// Create a new journal
    pub fn new(config: JournalConfig) -> Self {
        Self {
            #[cfg(feature = "std")]
            entries: VecDeque::with_capacity(config.max_entries),
            #[cfg(not(feature = "std"))]
            entries: Deque::new(),
            next_transaction_id: 1,
            config,
            committed_count: 0,
            uncommitted_count: 0,
        }
    }

    /// Add a new entry to the journal
    pub fn log_transaction(&mut self, mut entry: JournalEntry) -> EclipseFSResult<u64> {
        entry.transaction_id = self.next_transaction_id;
        let tx_id = entry.transaction_id;
        self.next_transaction_id += 1;
        
        #[cfg(feature = "std")]
        {
            // Remove oldest entries if we exceed max_entries
            while self.entries.len() >= self.config.max_entries {
                self.entries.pop_front();
            }
            self.entries.push_back(entry);
        }
        
        #[cfg(not(feature = "std"))]
        {
            if self.entries.is_full() {
                self.entries.pop_front();
            }
            self.entries.push_back(entry)
                .map_err(|_| EclipseFSError::DeviceFull)?;
        }
        
        self.uncommitted_count += 1;
        
        Ok(tx_id)
    }

    /// Commit all uncommitted transactions
    pub fn commit(&mut self) -> EclipseFSResult<()> {
        self.committed_count += self.uncommitted_count;
        self.uncommitted_count = 0;
        Ok(())
    }

    /// Rollback uncommitted transactions
    pub fn rollback(&mut self) -> EclipseFSResult<()> {
        let uncommitted = self.uncommitted_count as usize;
        
        #[cfg(feature = "std")]
        {
            for _ in 0..uncommitted {
                self.entries.pop_back();
            }
        }
        
        #[cfg(not(feature = "std"))]
        {
            for _ in 0..uncommitted {
                self.entries.pop_back();
            }
        }
        
        self.uncommitted_count = 0;
        Ok(())
    }

    /// Get journal statistics
    pub fn get_stats(&self) -> JournalStats {
        JournalStats {
            total_entries: self.entries.len() as u64,
            committed_count: self.committed_count,
            uncommitted_count: self.uncommitted_count,
            next_transaction_id: self.next_transaction_id,
        }
    }

    /// Replay journal for crash recovery
    pub fn replay(&self) -> EclipseFSResult<Vec<JournalEntry>> {
        #[cfg(feature = "std")]
        {
            let entries: Vec<JournalEntry> = self.entries.iter()
                .filter(|e| e.verify_checksum())
                .cloned()
                .collect();
            Ok(entries)
        }
        
        #[cfg(not(feature = "std"))]
        {
            let mut entries = HeaplessVec::new();
            for entry in self.entries.iter() {
                if entry.verify_checksum() {
                    entries.push(entry.clone())
                        .map_err(|_| EclipseFSError::OutOfMemory)?;
                }
            }
            Ok(entries)
        }
    }

    /// Clear the journal
    pub fn clear(&mut self) {
        self.entries.clear();
        self.committed_count = 0;
        self.uncommitted_count = 0;
    }

    /// Write journal to file (only available with std)
    #[cfg(feature = "std")]
    pub fn write_to_file(&self, path: &str) -> EclipseFSResult<()> {
        let mut file = File::create(path)?;
        
        // Write header
        file.write_all(b"ECLIPSEFS_JOURNAL")?;
        file.write_all(&self.next_transaction_id.to_le_bytes())?;
        file.write_all(&(self.entries.len() as u64).to_le_bytes())?;
        
        // Write entries
        for entry in &self.entries {
            self.write_entry(&mut file, entry)?;
        }
        
        Ok(())
    }

    /// Read journal from file (only available with std)
    #[cfg(feature = "std")]
    pub fn read_from_file(&mut self, path: &str) -> EclipseFSResult<()> {
        let mut file = File::open(path)?;
        
        // Read header
        let mut magic = [0u8; 17];
        file.read_exact(&mut magic)?;
        if &magic != b"ECLIPSEFS_JOURNAL" {
            return Err(EclipseFSError::InvalidFormat);
        }
        
        let mut transaction_id_bytes = [0u8; 8];
        file.read_exact(&mut transaction_id_bytes)?;
        self.next_transaction_id = u64::from_le_bytes(transaction_id_bytes);
        
        let mut count_bytes = [0u8; 8];
        file.read_exact(&mut count_bytes)?;
        let entry_count = u64::from_le_bytes(count_bytes);
        
        // Read entries
        self.entries.clear();
        for _ in 0..entry_count {
            let entry = self.read_entry(&mut file)?;
            self.entries.push_back(entry);
        }
        
        Ok(())
    }

    #[cfg(feature = "std")]
    fn write_entry(&self, file: &mut File, entry: &JournalEntry) -> EclipseFSResult<()> {
        file.write_all(&entry.transaction_id.to_le_bytes())?;
        file.write_all(&(entry.transaction_type as u8).to_le_bytes())?;
        file.write_all(&entry.inode.to_le_bytes())?;
        file.write_all(&entry.parent_inode.to_le_bytes())?;
        file.write_all(&entry.timestamp.to_le_bytes())?;
        file.write_all(&(entry.data_size as u32).to_le_bytes())?;
        file.write_all(&entry.data)?;
        file.write_all(&entry.checksum.to_le_bytes())?;
        Ok(())
    }

    #[cfg(feature = "std")]
    fn read_entry(&self, file: &mut File) -> EclipseFSResult<JournalEntry> {
        let mut buf8 = [0u8; 8];
        let mut buf4 = [0u8; 4];
        let mut buf1 = [0u8; 1];
        
        file.read_exact(&mut buf8)?;
        let transaction_id = u64::from_le_bytes(buf8);
        
        file.read_exact(&mut buf1)?;
        let transaction_type = match buf1[0] {
            0 => TransactionType::CreateFile,
            1 => TransactionType::CreateDirectory,
            2 => TransactionType::DeleteFile,
            3 => TransactionType::DeleteDirectory,
            4 => TransactionType::WriteData,
            5 => TransactionType::UpdateMetadata,
            6 => TransactionType::CreateSnapshot,
            7 => TransactionType::DeleteSnapshot,
            _ => return Err(EclipseFSError::InvalidFormat),
        };
        
        file.read_exact(&mut buf4)?;
        let inode = u32::from_le_bytes(buf4);
        
        file.read_exact(&mut buf4)?;
        let parent_inode = u32::from_le_bytes(buf4);
        
        file.read_exact(&mut buf8)?;
        let timestamp = u64::from_le_bytes(buf8);
        
        file.read_exact(&mut buf4)?;
        let data_size = u32::from_le_bytes(buf4) as usize;
        
        let mut data = vec![0u8; data_size];
        file.read_exact(&mut data)?;
        
        file.read_exact(&mut buf4)?;
        let checksum = u32::from_le_bytes(buf4);
        
        Ok(JournalEntry {
            transaction_id,
            transaction_type,
            inode,
            parent_inode,
            timestamp,
            data_size,
            data,
            checksum,
        })
    }
}

/// Statistics about the journal
#[derive(Debug, Clone)]
pub struct JournalStats {
    pub total_entries: u64,
    pub committed_count: u64,
    pub uncommitted_count: u64,
    pub next_transaction_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_journal_creation() {
        let journal = Journal::new(JournalConfig::default());
        let stats = journal.get_stats();
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.committed_count, 0);
    }

    #[test]
    fn test_log_transaction() {
        let mut journal = Journal::new(JournalConfig::default());
        let entry = JournalEntry::new(TransactionType::CreateFile, 1, 0);
        let tx_id = journal.log_transaction(entry).unwrap();
        assert_eq!(tx_id, 1);
        assert_eq!(journal.get_stats().total_entries, 1);
    }

    #[test]
    fn test_commit_rollback() {
        let mut journal = Journal::new(JournalConfig::default());
        let entry = JournalEntry::new(TransactionType::WriteData, 1, 0);
        journal.log_transaction(entry).unwrap();
        
        assert_eq!(journal.get_stats().uncommitted_count, 1);
        journal.commit().unwrap();
        assert_eq!(journal.get_stats().committed_count, 1);
        assert_eq!(journal.get_stats().uncommitted_count, 0);
    }

    #[test]
    fn test_checksum_verification() {
        let entry = JournalEntry::new(TransactionType::CreateFile, 1, 0)
            .with_data(b"test data").unwrap();
        assert!(entry.verify_checksum());
    }
}
