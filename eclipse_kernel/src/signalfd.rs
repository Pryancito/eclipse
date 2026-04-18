use crate::scheme::*;
use spin::Mutex;

pub struct SignalfdScheme {
    next_id: Mutex<usize>,
}

impl SignalfdScheme {
    pub fn new() -> Self {
        Self { next_id: Mutex::new(1) }
    }
}

impl Scheme for SignalfdScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let mut id = self.next_id.lock();
        let res = *id;
        *id += 1;
        Ok(res)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        // For now, return EAGAIN to indicate no signals are pending.
        // This satisfies the event loop's non-blocking check.
        Err(error::EAGAIN)
    }

    fn write(&self, _id: usize, _buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        Err(error::EBADF)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o140000; // Socket-like behavior for signalfd
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    fn poll(&self, _id: usize, _events: usize) -> Result<usize, usize> {
        // Signalfd is currently a placeholder that never has signals pending
        Ok(0)
    }
}
