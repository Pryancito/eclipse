use lock::Mutex;
use virtio_drivers::{VirtIOBlk as InnerDriver, VirtIOHeader};

use crate::scheme::{BlockScheme, Scheme};
use crate::DeviceResult;

pub struct VirtIoBlk<'a> {
    inner: Mutex<InnerDriver<'a>>,
    capacity: usize,
}

impl<'a> VirtIoBlk<'a> {
    pub fn new(header: &'static mut VirtIOHeader) -> DeviceResult<Self> {
        let capacity = unsafe {
            let config = &*(header.config_space() as *const u64);
            *config as usize
        };
        Ok(Self {
            inner: Mutex::new(InnerDriver::new(header)?),
            capacity,
        })
    }
}

impl<'a> Scheme for VirtIoBlk<'a> {
    fn name(&self) -> &str {
        "virtio-blk"
    }

    fn handle_irq(&self, _irq_num: usize) {
        self.inner.lock().ack_interrupt();
    }
}

impl<'a> BlockScheme for VirtIoBlk<'a> {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        self.inner.lock().read_block(block_id, buf)?;
        Ok(())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
        self.inner.lock().write_block(block_id, buf)?;
        Ok(())
    }

    fn flush(&self) -> DeviceResult {
        Ok(())
    }

    fn block_count(&self) -> usize {
        self.capacity
    }
}
