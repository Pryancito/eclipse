//! Integration tests for EclipseFS improvements

use eclipsefs_lib::{
    EclipseFS, Journal, JournalConfig, TransactionType,
    JournalEntry, NodeKind, constants,
};

#[test]
fn test_basic_filesystem_operations() {
    let mut fs = EclipseFS::new();
    
    // Create a file
    let file_inode = fs.create_file(constants::ROOT_INODE, "test.txt").unwrap();
    assert!(file_inode > 0);
    
    // Write data to file
    fs.write_file(file_inode, b"Hello, EclipseFS!").unwrap();
    
    // Read data from file
    let data = fs.read_file(file_inode).unwrap();
    assert_eq!(data, b"Hello, EclipseFS!");
    
    // Get stats
    let (total, files, dirs) = fs.get_stats();
    assert!(total >= 2); // root + test.txt
    assert_eq!(files, 1);
    assert_eq!(dirs, 1);
}

#[test]
fn test_directory_operations() {
    let mut fs = EclipseFS::new();
    
    // Create a directory
    let dir_inode = fs.create_directory(constants::ROOT_INODE, "testdir").unwrap();
    assert!(dir_inode > 0);
    
    // Create file in directory
    let file_inode = fs.create_file(dir_inode, "file.txt").unwrap();
    assert!(file_inode > 0);
    
    // List directory
    let entries = fs.list_directory(dir_inode).unwrap();
    assert!(entries.contains(&"file.txt".to_string()));
}

#[test]
fn test_journaling_system() {
    let mut fs = EclipseFS::new();
    
    // Enable journaling
    fs.enable_journaling(JournalConfig::default()).unwrap();
    
    // Create a file (should be journaled)
    let file_inode = fs.create_file(constants::ROOT_INODE, "journaled.txt").unwrap();
    
    // Write data (should be journaled)
    fs.write_file(file_inode, b"This is journaled").unwrap();
    
    // Commit journal
    fs.commit_journal().unwrap();
    
    // Verify file exists and has correct data
    let data = fs.read_file(file_inode).unwrap();
    assert_eq!(data, b"This is journaled");
}

#[test]
fn test_journal_recovery() {
    let mut fs = EclipseFS::new();
    fs.enable_journaling(JournalConfig::default()).unwrap();
    
    // Create some files
    let file1 = fs.create_file(constants::ROOT_INODE, "file1.txt").unwrap();
    let file2 = fs.create_file(constants::ROOT_INODE, "file2.txt").unwrap();
    
    // Write data
    fs.write_file(file1, b"Data 1").unwrap();
    fs.write_file(file2, b"Data 2").unwrap();
    
    // Simulate crash recovery
    let recovered = fs.recover_from_journal().unwrap();
    assert!(recovered >= 0);
}

#[test]
fn test_copy_on_write() {
    let mut fs = EclipseFS::new();
    fs.enable_copy_on_write();
    
    // Create a file
    let file_inode = fs.create_file(constants::ROOT_INODE, "cow.txt").unwrap();
    fs.write_file(file_inode, b"Version 1").unwrap();
    
    // Modify file (should create new version with CoW)
    fs.write_file(file_inode, b"Version 2").unwrap();
    
    // Verify new data
    let data = fs.read_file(file_inode).unwrap();
    assert_eq!(data, b"Version 2");
    
    // Check version history
    let history = fs.get_version_history(file_inode);
    assert!(history.is_some());
}

#[test]
fn test_path_lookup() {
    let mut fs = EclipseFS::new();
    
    // Create directory structure
    let home = fs.create_directory(constants::ROOT_INODE, "home").unwrap();
    let user = fs.create_directory(home, "user").unwrap();
    let docs = fs.create_directory(user, "documents").unwrap();
    let file = fs.create_file(docs, "readme.txt").unwrap();
    
    // Lookup by path
    let found_inode = fs.lookup_path("/home/user/documents/readme.txt").unwrap();
    assert_eq!(found_inode, file);
}

#[test]
fn test_journal_transaction_types() {
    let mut journal = Journal::new(JournalConfig::default());
    
    // Test different transaction types
    let tx1 = JournalEntry::new(TransactionType::CreateFile, 1, 0);
    let tx2 = JournalEntry::new(TransactionType::CreateDirectory, 2, 0);
    let tx3 = JournalEntry::new(TransactionType::WriteData, 3, 0)
        .with_data(b"test data").unwrap();
    
    journal.log_transaction(tx1).unwrap();
    journal.log_transaction(tx2).unwrap();
    journal.log_transaction(tx3).unwrap();
    
    let stats = journal.get_stats();
    assert_eq!(stats.total_entries, 3);
    assert_eq!(stats.uncommitted_count, 3);
}

#[test]
fn test_journal_commit_rollback() {
    let mut journal = Journal::new(JournalConfig::default());
    
    // Add transaction
    let tx = JournalEntry::new(TransactionType::CreateFile, 1, 0);
    journal.log_transaction(tx).unwrap();
    
    assert_eq!(journal.get_stats().uncommitted_count, 1);
    
    // Test commit
    journal.commit().unwrap();
    assert_eq!(journal.get_stats().uncommitted_count, 0);
    assert_eq!(journal.get_stats().committed_count, 1);
    
    // Add another transaction
    let tx2 = JournalEntry::new(TransactionType::DeleteFile, 2, 0);
    journal.log_transaction(tx2).unwrap();
    
    // Test rollback
    journal.rollback().unwrap();
    assert_eq!(journal.get_stats().uncommitted_count, 0);
    assert_eq!(journal.get_stats().total_entries, 1); // Only committed one
}

#[test]
fn test_checksum_verification() {
    let entry = JournalEntry::new(TransactionType::WriteData, 1, 0)
        .with_data(b"Test data for checksum").unwrap();
    
    // Verify checksum is valid
    assert!(entry.verify_checksum());
}

#[test]
fn test_node_checksum() {
    use eclipsefs_lib::EclipseFSNode;
    
    let mut node = EclipseFSNode::new_file();
    // set_data already calls update_checksum internally
    node.set_data(b"Test data").unwrap();
    
    // Manual checksum update
    node.update_checksum();
    
    // Verify integrity
    assert!(node.verify_integrity().is_ok());
}

#[test]
fn test_encryption_configuration() {
    use eclipsefs_lib::{EncryptionInfo, EncryptionType};
    
    let enc_info = EncryptionInfo::new_transparent(EncryptionType::AES256, 1);
    
    // Verify key integrity check
    assert!(enc_info.verify_key_integrity());
}

#[test]
fn test_snapshot_creation() {
    let mut fs = EclipseFS::new();
    
    // Create a file
    let file_inode = fs.create_file(constants::ROOT_INODE, "snapshot_test.txt").unwrap();
    fs.write_file(file_inode, b"Initial data").unwrap();
    
    // Create a filesystem snapshot
    fs.create_filesystem_snapshot(1, "Test snapshot").unwrap();
    
    // Verify snapshot exists
    let snapshots = fs.list_snapshots().unwrap();
    assert_eq!(snapshots.len(), 1);
}

#[test]
fn test_system_stats() {
    let mut fs = EclipseFS::new();
    
    // Create some content
    fs.create_file(constants::ROOT_INODE, "file.txt").unwrap();
    fs.create_directory(constants::ROOT_INODE, "dir").unwrap();
    
    // Get system stats
    let stats = fs.get_system_stats();
    assert!(stats.total_nodes >= 3); // root + file + dir
    assert!(!stats.cow_enabled); // Not enabled by default
    assert!(!stats.cache_enabled); // Not enabled by default
}

