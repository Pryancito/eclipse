//! Invariantes que deben coincidir con el kernel (`eclipse_kernel/src/storage.rs`
//! `DiskScheme::read` / `write` y con la política LRU de `DirCacheState` en
//! `filesystem.rs`). Se ejecutan en host con `cargo test -p eclipsefs-lib`.

const BLOCK: u64 = 4096;

/// Primer trozo de una lectura `DiskScheme::read` (equivale a una iteración del bucle).
fn disk_scheme_read_first_chunk(
    buffer_len: usize,
    disk_offset_in_view: u64,
    partition_size: u64,
) -> usize {
    let avail_in_partition = if partition_size == u64::MAX {
        buffer_len as u64
    } else {
        partition_size.saturating_sub(disk_offset_in_view)
    };
    if avail_in_partition == 0 {
        return 0;
    }
    let read_len = std::cmp::min(buffer_len as u64, avail_in_partition) as usize;
    let abs_offset = disk_offset_in_view; // sin partition_offset en estos tests
    let offset_in_block = (abs_offset % BLOCK) as usize;
    let available = 4096usize - offset_in_block;
    std::cmp::min(read_len, available)
}

/// Réplica del bucle completo de `DiskScheme::read`: bytes devueltos (sin fallos de I/O).
fn disk_scheme_read_total_returned(
    buffer_len: usize,
    disk_offset_in_view: u64,
    partition_size: u64,
) -> usize {
    let avail_in_partition = if partition_size == u64::MAX {
        buffer_len as u64
    } else {
        partition_size.saturating_sub(disk_offset_in_view)
    };
    if avail_in_partition == 0 {
        return 0;
    }
    std::cmp::min(buffer_len as u64, avail_in_partition) as usize
}

/// Primer trozo de un `DiskScheme::write` (misma geometría que la primera iteración del `read`).
fn disk_scheme_write_first_chunk(
    buffer_len: usize,
    disk_offset_in_view: u64,
    partition_size: u64,
) -> usize {
    let avail_in_partition = if partition_size == u64::MAX {
        buffer_len as u64
    } else {
        partition_size.saturating_sub(disk_offset_in_view)
    };
    if avail_in_partition == 0 {
        return 0;
    }
    let write_len = std::cmp::min(buffer_len as u64, avail_in_partition) as usize;
    let offset_in_block = (disk_offset_in_view % BLOCK) as usize;
    std::cmp::min(write_len, 4096usize - offset_in_block)
}

/// Bytes transferidos en una llamada completa `read` o `write` (misma fórmula).
fn disk_scheme_rw_total_returned(
    buffer_len: usize,
    disk_offset_in_view: u64,
    partition_size: u64,
) -> usize {
    disk_scheme_read_total_returned(buffer_len, disk_offset_in_view, partition_size)
}

/// Suma de los `to_copy` del bucle read/write hasta agotar `cap` (debe ser `cap`).
fn sum_rw_chunk_sizes(cap: usize, mut disk_off: u64) -> usize {
    let mut sum = 0usize;
    while sum < cap {
        let ob = (disk_off % BLOCK) as usize;
        let avail = 4096usize - ob;
        let chunk = std::cmp::min(cap - sum, avail);
        sum += chunk;
        disk_off += chunk as u64;
    }
    sum
}

#[test]
fn disk_read_first_chunk_never_exceeds_block_remainder() {
    assert_eq!(disk_scheme_read_first_chunk(8192, 0, u64::MAX), 4096);
    assert_eq!(disk_scheme_read_first_chunk(100, 0, u64::MAX), 100);
    assert_eq!(disk_scheme_read_first_chunk(8192, 100, u64::MAX), 4096 - 100);
    assert_eq!(disk_scheme_read_first_chunk(8192, 4095, u64::MAX), 1);
    assert_eq!(disk_scheme_read_first_chunk(8192, 4096, u64::MAX), 4096);
}

#[test]
fn disk_read_full_call_spans_multiple_blocks() {
    assert_eq!(disk_scheme_read_total_returned(8192, 0, u64::MAX), 8192);
    assert_eq!(disk_scheme_read_total_returned(10_000, 100, u64::MAX), 10_000);
    assert_eq!(disk_scheme_read_total_returned(1, 2048, u64::MAX), 1);
}

#[test]
fn disk_read_first_chunk_respects_partition_end() {
    let part = 500u64;
    assert_eq!(disk_scheme_read_first_chunk(4096, 0, part), 500);
    assert_eq!(disk_scheme_read_first_chunk(4096, 400, part), 100);
    assert_eq!(disk_scheme_read_first_chunk(4096, 500, part), 0);
}

#[test]
fn disk_read_total_respects_partition_end() {
    let part = 500u64;
    assert_eq!(disk_scheme_read_total_returned(4096, 0, part), 500);
    assert_eq!(disk_scheme_read_total_returned(100, 400, part), 100);
    assert_eq!(disk_scheme_read_total_returned(4096, 500, part), 0);
}

#[test]
fn disk_write_same_chunk_rule_as_read_first_chunk() {
    for off in [0u64, 1, 2048, 4095, 4096, 10_000] {
        for buf in [1usize, 100, 4096, 8192] {
            assert_eq!(
                disk_scheme_read_first_chunk(buf, off, u64::MAX),
                disk_scheme_write_first_chunk(buf, off, u64::MAX),
                "off={} buf={}",
                off,
                buf
            );
        }
    }
}

#[test]
fn disk_write_total_matches_read_total() {
    for off in [0u64, 7, 4095, 4096, 9000] {
        for buf in [0usize, 1, 50, 4096, 8193, 20_000] {
            assert_eq!(
                disk_scheme_read_total_returned(buf, off, u64::MAX),
                disk_scheme_rw_total_returned(buf, off, u64::MAX)
            );
        }
    }
    let part = 12_345u64;
    assert_eq!(
        disk_scheme_read_total_returned(50_000, 100, part),
        disk_scheme_rw_total_returned(50_000, 100, part)
    );
}

#[test]
fn disk_rw_chunk_partitioning_sums_to_total() {
    for off in [0u64, 3, 4095, 5000] {
        for buf in [1usize, 100, 4097, 12_000] {
            let cap = disk_scheme_rw_total_returned(buf, off, u64::MAX);
            assert_eq!(sum_rw_chunk_sizes(cap, off), cap);
        }
    }
    let cap = disk_scheme_rw_total_returned(8000, 0, 600);
    assert_eq!(sum_rw_chunk_sizes(cap, 0), cap);
}

// --- Dir cache LRU (mirror de filesystem.rs DirCacheState::insert) ---

struct TestDirCache {
    entries: Vec<(u32, u64, u32)>, // (inode_id, last_access, dummy payload id)
    access_counter: u64,
    max: usize,
}

impl TestDirCache {
    fn new(max: usize) -> Self {
        Self {
            entries: Vec::new(),
            access_counter: 0,
            max,
        }
    }

    fn insert(&mut self, inode_id: u32, payload: u32) {
        if self.entries.iter().any(|e| e.0 == inode_id) {
            return;
        }
        self.access_counter = self.access_counter.wrapping_add(1);
        let ac = self.access_counter;
        if self.entries.len() < self.max {
            self.entries.push((inode_id, ac, payload));
        } else {
            let victim = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.1)
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.entries[victim] = (inode_id, ac, payload);
        }
    }

    fn payload(&self, inode: u32) -> Option<u32> {
        self.entries
            .iter()
            .find(|e| e.0 == inode)
            .map(|e| e.2)
    }
}

#[test]
fn dir_cache_lru_evicts_oldest_access() {
    let mut c = TestDirCache::new(3);
    c.insert(1, 10);
    c.insert(2, 20);
    c.insert(3, 30);
    assert_eq!(c.payload(1), Some(10));
    c.insert(4, 40);
    assert_eq!(c.payload(1), None, "inode 1 debe ser víctima LRU");
    assert_eq!(c.payload(4), Some(40));
}

#[test]
fn dir_cache_skip_duplicate_inode() {
    let mut c = TestDirCache::new(2);
    c.insert(1, 10);
    c.insert(1, 99);
    assert_eq!(c.payload(1), Some(10));
    assert_eq!(c.entries.len(), 1);
}
