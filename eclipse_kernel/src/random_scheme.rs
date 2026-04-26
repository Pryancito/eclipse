use crate::scheme::{Scheme, Stat};

/// Scheme providing random numbers (similar to /dev/urandom).
/// Uses the hardware random number generator (RDRAND) via crate::cpu::get_random_u64.
pub struct RandomScheme;

impl RandomScheme {
    pub fn new() -> Self {
        Self
    }
}

impl Scheme for RandomScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0) // Single resource ID for all opens
    }

    fn read(&self, _id: usize, buf: &mut [u8], _offset: u64) -> Result<usize, usize> {
        let mut i = 0;
        while i < buf.len() {
            let rnd = crate::cpu::get_random_u64();
            let bytes = rnd.to_ne_bytes();
            let to_copy = core::cmp::min(bytes.len(), buf.len() - i);
            buf[i..i + to_copy].copy_from_slice(&bytes[..to_copy]);
            i += to_copy;
        }
        Ok(buf.len())
    }

    fn write(&self, _id: usize, buf: &[u8], _offset: u64) -> Result<usize, usize> {
        // Writing to random device is allowed but doesn't change entropy pool in this simple impl
        Ok(buf.len())
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Ok(0) // Seeking has no effect
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o666 | 0x2000; // S_IFCHR | rw-rw-rw-
        stat.size = 0;
        Ok(0)
    }
}
