//! Objects for Kernel Debuglog.
use {
    super::*,
    crate::object::*,
    alloc::{collections::VecDeque, sync::Arc},
    core::convert::TryInto,
    kernel_hal::sync::Mutex,
    lazy_static::lazy_static,
};

lazy_static! {
    static ref DLOG: Mutex<DlogBuffer> = Mutex::new(DlogBuffer {
        buf: VecDeque::with_capacity(0x1000),
        base: 0,
    });
}

/// Debuglog - Kernel debuglog
///
/// ## SYNOPSIS
///
/// Debuglog objects allow userspace to read and write to kernel debug logs.
pub struct DebugLog {
    base: KObjectBase,
    flags: u32,
    /// Where this reader is in the log, as an offset that keeps counting
    /// from the first byte ever written, so it stays meaningful after the
    /// buffer has dropped records in front of it.
    read_offset: Mutex<usize>,
}

/// The kernel's one log: the newest `DLOG_SIZE` bytes of records.
///
/// It used to be an append-only `Vec` that kept every record ever written
/// for the life of the kernel, so any process with a writable debuglog
/// handle could grow the kernel heap without bound.
struct DlogBuffer {
    buf: VecDeque<u8>,
    /// The offset, in the same count readers use, of `buf[0]`.
    base: usize,
}

impl_kobject!(DebugLog);

impl DebugLog {
    /// Create a new `DebugLog`.
    pub fn create(flags: u32) -> Arc<Self> {
        Arc::new(DebugLog {
            base: KObjectBase::new(),
            flags,
            read_offset: Default::default(),
        })
    }

    /// Read a log, return the actual read size.
    ///
    /// A reader that fell behind the records the buffer has since dropped
    /// resumes at the oldest one still kept.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let mut offset = self.read_offset.lock();
        let (next, len) = DLOG.lock().read_at(*offset, buf);
        *offset = next;
        len
    }

    /// Write a log. Data past `DLOG_MAX_DATA` bytes is dropped, as Zircon
    /// does, so a record always fits one `DLOG_MAX_LEN` read.
    pub fn write(&self, severity: Severity, flags: u32, tid: u64, pid: u64, data: &[u8]) {
        DLOG.lock()
            .write(severity, flags | self.flags, tid, pid, data);
    }
}

/// The record header, in the layout `zx_log_record_t` gives it: `rollout`,
/// `datalen`, `severity`, `flags`, `timestamp`, `pid`, `tid`.
#[derive(Debug, PartialEq, Eq)]
struct DlogHeader {
    /// Bits 12..: `HEADER_SIZE + datalen`; bits ..12: that rounded up to 4,
    /// which is the distance to the next record.
    rollout: u32,
    datalen: u16,
    severity: u8,
    flags: u8,
    timestamp: u64,
    pid: u64,
    tid: u64,
}

impl DlogHeader {
    /// The header as its wire bytes: every record starts on a multiple of
    /// four, not eight, so a header is not read as a `#[repr(C)]` struct in
    /// place (the reference was misaligned, which panics with debug
    /// assertions and is undefined behaviour without them).
    fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0..4].copy_from_slice(&self.rollout.to_ne_bytes());
        out[4..6].copy_from_slice(&self.datalen.to_ne_bytes());
        out[6] = self.severity;
        out[7] = self.flags;
        out[8..16].copy_from_slice(&self.timestamp.to_ne_bytes());
        out[16..24].copy_from_slice(&self.pid.to_ne_bytes());
        out[24..32].copy_from_slice(&self.tid.to_ne_bytes());
        out
    }

    fn from_bytes(b: &[u8; HEADER_SIZE]) -> Self {
        let u64_at = |i: usize| u64::from_ne_bytes(b[i..i + 8].try_into().unwrap());
        DlogHeader {
            rollout: u32::from_ne_bytes(b[0..4].try_into().unwrap()),
            datalen: u16::from_ne_bytes(b[4..6].try_into().unwrap()),
            severity: b[6],
            flags: b[7],
            timestamp: u64_at(8),
            pid: u64_at(16),
            tid: u64_at(24),
        }
    }

    /// The distance from this record's first byte to the next record's.
    fn wire_size(&self) -> usize {
        (self.rollout & 0xFFF) as usize
    }
}

/// Log entry severity. Used for coarse filtering of log messages.
#[allow(missing_docs)]
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Trace = 0x10,
    Debug = 0x20,
    Info = 0x30,
    Warning = 0x40,
    Error = 0x50,
    Fatal = 0x60,
}

const HEADER_SIZE: usize = 32;
/// Max length of Dlog read buffer.
pub const DLOG_MAX_LEN: usize = 256;
/// Max length of one record's data; what does not fit is dropped.
pub const DLOG_MAX_DATA: usize = DLOG_MAX_LEN - HEADER_SIZE;
/// How much of the log the kernel keeps: the newest records that fit.
pub const DLOG_SIZE: usize = 128 * 1024;

impl DlogBuffer {
    /// The offset one past the newest record.
    fn end(&self) -> usize {
        self.base + self.buf.len()
    }

    /// The header of the record at `offset`, which must be a record start
    /// at or after `base`.
    fn header_at(&self, offset: usize) -> DlogHeader {
        let start = offset - self.base;
        let mut header = [0u8; HEADER_SIZE];
        for (dst, src) in header
            .iter_mut()
            .zip(self.buf.range(start..start + HEADER_SIZE))
        {
            *dst = *src;
        }
        DlogHeader::from_bytes(&header)
    }

    /// Read one record at `offset`, answering where the reader continues and
    /// how many bytes it got (0 at the end of the log).
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> (usize, usize) {
        assert!(buf.len() >= DLOG_MAX_LEN);
        // Everything before `base` has been dropped: the oldest record kept
        // is the next one this reader can have.
        let offset = offset.max(self.base);
        if offset == self.end() {
            return (offset, 0);
        }
        let len = self.header_at(offset).wire_size();
        let start = offset - self.base;
        for (dst, src) in buf[..len]
            .iter_mut()
            .zip(self.buf.range(start..start + len))
        {
            *dst = *src;
        }
        (offset + len, len)
    }

    fn write(&mut self, severity: Severity, flags: u32, tid: u64, pid: u64, data: &[u8]) {
        let data = &data[..data.len().min(DLOG_MAX_DATA)];
        let wire_size = HEADER_SIZE + align_up_4(data.len());
        let size = HEADER_SIZE + data.len();
        let header = DlogHeader {
            rollout: ((size as u32) << 12) | (wire_size as u32),
            datalen: data.len() as u16,
            severity: severity as u8,
            flags: flags as u8,
            timestamp: kernel_hal::timer::timer_now().as_nanos() as u64,
            pid,
            tid,
        };
        self.make_room(wire_size);
        self.buf.extend(header.to_bytes());
        self.buf.extend(data);
        self.buf.extend(&[0u8; 4][..wire_size - size]);
    }

    /// Drop the oldest records until `needed` more bytes fit in `DLOG_SIZE`.
    fn make_room(&mut self, needed: usize) {
        while !self.buf.is_empty() && self.buf.len() + needed > DLOG_SIZE {
            let oldest = self.header_at(self.base).wire_size();
            self.buf.drain(..oldest);
            self.base += oldest;
        }
    }
}

fn align_up_4(x: usize) -> usize {
    (x + 3) & !3
}

#[cfg(test)]
mod tests {
    //! The log is one kernel-wide buffer, so these tests take turns on it
    //! and tag their records with a pid nobody else uses.
    use super::*;
    use alloc::vec::Vec;

    lazy_static! {
        static ref TURN: Mutex<()> = Mutex::new(());
    }

    /// The next record for `pid`, as its header and data, skipping records
    /// other tests wrote.
    fn next_record(log: &DebugLog, pid: u64) -> Option<(DlogHeader, Vec<u8>)> {
        let mut buf = [0u8; DLOG_MAX_LEN];
        loop {
            let len = log.read(&mut buf);
            if len == 0 {
                return None;
            }
            let header = DlogHeader::from_bytes(buf[..HEADER_SIZE].try_into().unwrap());
            assert_eq!(len, header.wire_size());
            if header.pid == pid {
                let data = buf[HEADER_SIZE..HEADER_SIZE + header.datalen as usize].to_vec();
                return Some((header, data));
            }
        }
    }

    /// A one-byte record ends on a multiple of four, not eight, so reading
    /// the record after it dereferenced a misaligned `DlogHeader`: a panic
    /// with debug assertions on, undefined behaviour with them off.
    #[test]
    fn the_record_after_an_odd_length_one_reads_back() {
        let _turn = TURN.lock();
        let log = DebugLog::create(0x1);
        log.write(Severity::Info, 0, 7, 0x0dd1, b"a");
        log.write(Severity::Warning, 0x10, 8, 0x0dd1, b"second");
        let (first, data) = next_record(&log, 0x0dd1).unwrap();
        assert_eq!((first.datalen, data.as_slice()), (1, &b"a"[..]));
        assert_eq!(first.wire_size(), HEADER_SIZE + 4, "padded to four bytes");
        let (second, data) = next_record(&log, 0x0dd1).unwrap();
        assert_eq!(data, b"second");
        assert_eq!(
            (second.severity, second.flags, second.tid, second.pid),
            (Severity::Warning as u8, 0x11, 8, 0x0dd1),
            "the log's own flags join the write's"
        );
        assert_eq!(second.rollout >> 12, (HEADER_SIZE + 6) as u32);
        assert_eq!(second.wire_size(), HEADER_SIZE + 8);
    }

    /// The header goes out and comes back field by field, in the layout
    /// userspace reads it in.
    #[test]
    fn a_header_survives_its_wire_bytes() {
        let header = DlogHeader {
            rollout: (36 << 12) | 36,
            datalen: 4,
            severity: Severity::Error as u8,
            flags: 0x10,
            timestamp: 0x0102_0304_0506_0708,
            pid: 0x1122_3344_5566_7788,
            tid: 0x99aa_bbcc_ddee_ff00,
        };
        let bytes = header.to_bytes();
        assert_eq!(&bytes[0..4], &(((36u32) << 12) | 36).to_ne_bytes());
        assert_eq!(&bytes[4..6], &4u16.to_ne_bytes());
        assert_eq!(bytes[6], 0x50);
        assert_eq!(bytes[7], 0x10);
        assert_eq!(DlogHeader::from_bytes(&bytes), header);
    }

    /// Only the newest `DLOG_SIZE` bytes are kept, and what a fresh reader
    /// sees first is the oldest record still there, with nothing skipped
    /// after it.
    #[test]
    fn the_log_keeps_the_newest_records_and_drops_the_oldest_whole() {
        let _turn = TURN.lock();
        let log = DebugLog::create(0);
        let data = [b'x'; DLOG_MAX_DATA];
        let records = 2 * DLOG_SIZE / DLOG_MAX_LEN;
        for i in 0..records {
            log.write(Severity::Info, 0, i as u64, 0x0dd2, &data);
        }
        {
            let dlog = DLOG.lock();
            assert!(dlog.buf.len() <= DLOG_SIZE);
            assert!(
                dlog.buf.len() > DLOG_SIZE - DLOG_MAX_LEN,
                "as full as it gets"
            );
            assert_eq!(dlog.end() - dlog.base, dlog.buf.len());
        }
        let (first, _) = next_record(&log, 0x0dd2).unwrap();
        assert!(
            first.tid >= records as u64 / 2,
            "the first half was dropped, this is record {}",
            first.tid
        );
        let mut expect = first.tid + 1;
        while let Some((h, d)) = next_record(&log, 0x0dd2) {
            assert_eq!(h.tid, expect, "records come in order with no gap");
            assert_eq!(d.len(), DLOG_MAX_DATA);
            expect += 1;
        }
        assert_eq!(expect, records as u64, "the newest record is the last one");
    }

    /// A reader that stopped, then had the log wrap past it, resumes at the
    /// oldest record kept instead of reading from an offset the buffer no
    /// longer holds.
    #[test]
    fn a_reader_left_behind_resumes_at_the_oldest_record_kept() {
        let _turn = TURN.lock();
        let log = DebugLog::create(0);
        log.write(Severity::Info, 0, 0, 0x0dd3, b"before");
        let (h, _) = next_record(&log, 0x0dd3).unwrap();
        assert_eq!(h.tid, 0);
        let data = [b'y'; DLOG_MAX_DATA];
        for i in 1..=(DLOG_SIZE / DLOG_MAX_LEN + 8) as u64 {
            log.write(Severity::Info, 0, i, 0x0dd3, &data);
        }
        let base = DLOG.lock().base;
        assert!(
            *log.read_offset.lock() < base,
            "the reader is behind the buffer"
        );
        let (h, _) = next_record(&log, 0x0dd3).unwrap();
        assert!(h.tid > 1, "the first records are gone");
        assert!(*log.read_offset.lock() > base);
    }

    /// Data past `DLOG_MAX_DATA` is dropped: the record must fit a
    /// `DLOG_MAX_LEN` read, and `rollout` has twelve bits for its size.
    #[test]
    fn a_write_longer_than_a_record_keeps_the_first_dlog_max_data_bytes() {
        let _turn = TURN.lock();
        let log = DebugLog::create(0);
        let long: Vec<u8> = (0..DLOG_MAX_LEN + 100).map(|i| i as u8).collect();
        log.write(Severity::Info, 0, 0, 0x0dd4, &long);
        let (h, data) = next_record(&log, 0x0dd4).unwrap();
        assert_eq!(h.datalen as usize, DLOG_MAX_DATA);
        assert_eq!(data, &long[..DLOG_MAX_DATA]);
        assert_eq!(h.wire_size(), DLOG_MAX_LEN);
    }
}
