extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use lock::Mutex;

lazy_static::lazy_static! {
    /// Global ring buffer of security incidents
    pub static ref GLOBAL_LOG: Mutex<IntrusionLog> = Mutex::new(IntrusionLog::new(256));
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub pid: u64,
    pub action: &'static str, // e.g., "BLOCKED", "WARN"
    pub description: String,
}

pub struct IntrusionLog {
    entries: Vec<LogEntry>,
    max_size: usize,
}

impl IntrusionLog {
    pub const fn new(max_size: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_size,
        }
    }

    pub fn push(&mut self, entry: LogEntry) {
        if self.entries.len() >= self.max_size {
            // Remove the oldest entry if buffer is full
            if !self.entries.is_empty() {
                self.entries.remove(0);
            }
        }
        self.entries.push(entry);
    }

    pub fn get_entries(&self) -> Vec<LogEntry> {
        self.entries.clone()
    }
}

/// Appends a security event to the global log.
pub fn log_event(pid: u64, action: &'static str, description: String) {
    let mut log = GLOBAL_LOG.lock();
    log.push(LogEntry {
        pid,
        action,
        description,
    });
}
