//! [`ECLIPSE_BLOCK_SERVE`](super::ECLIPSE_BLOCK_SERVE): a disk that a process
//! drives, given to the kernel as a block device of its own.
//!
//! The process (a userspace driver such as `tools/zx-virtio-blk`) keeps one
//! end of a FIFO of 32-byte block messages and hands the kernel the other,
//! with a data window: a VMO both sides reach, which the driver's device
//! reads and writes directly. The kernel sees a [`BlockScheme`] like any
//! other and puts it under `/dev` through the hot-plug path, so everything
//! above it -- the page cache, the filesystems, `pread` on the node -- works
//! unchanged.
//!
//! A request is one message out and the same message back with its status:
//! the data is copied between the caller's buffer and the window on this
//! side, and the driver never sees kernel memory.
//!
//! [`BlockScheme`] is synchronous and bare metal has no `block_on`, so the
//! wait for the reply parks the calling executor with
//! [`yield_now_sync`](kernel_hal::thread::yield_now_sync) between looks,
//! as the NFS transport does, and opens a window for interrupts each time
//! (see `between_looks`). One request is outstanding at a time.
//!
//! What this trusts: the driver is a process the system started for the
//! purpose. It can answer wrongly (the data is whatever it put in the window)
//! or slowly (a request gives up after [`REPLY_TIMEOUT_MS`]); it cannot make
//! the kernel read or write anything but the window.

use alloc::sync::Arc;
use core::convert::TryInto;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use kernel_hal::drivers::prelude::{Device, DeviceError, DeviceResult};
use kernel_hal::drivers::scheme::{BlockScheme, IoSnapshot, IoStats, Scheme};
use zircon_object::ipc::Fifo;
use zircon_object::object::Rights;
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::{ZxError, ZxResult};

/// One FIFO element: `op, reqid, sectors, buf_off` (u32 each), `sector`
/// (u64), `status` (i64, the driver's answer). The driver's own layout.
pub(super) const BLOCK_MSG: usize = 32;
const OP_READ: u32 = 1;
const OP_WRITE: u32 = 2;
const SECTOR: usize = 512;
/// How long a request waits for its reply before it is an I/O error.
pub(super) const REPLY_TIMEOUT_MS: u64 = 30_000;
/// The smallest window worth serving through: one page.
const MIN_WINDOW: usize = 4096;

/// A disk behind a FIFO.
pub(super) struct ServedDisk {
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    /// Bytes of the window a request may use, a whole number of sectors.
    window_len: usize,
    sectors: u64,
    /// Held for the whole of a request: one outstanding at a time.
    busy: AtomicBool,
    next_id: AtomicU32,
    stats: IoStats,
}

/// A message, as bytes.
pub(super) fn encode(op: u32, reqid: u32, sectors: u32, sector: u64) -> [u8; BLOCK_MSG] {
    let mut m = [0u8; BLOCK_MSG];
    m[0..4].copy_from_slice(&op.to_ne_bytes());
    m[4..8].copy_from_slice(&reqid.to_ne_bytes());
    m[8..12].copy_from_slice(&sectors.to_ne_bytes());
    // buf_off stays 0: the kernel uses the window from its start.
    m[16..24].copy_from_slice(&sector.to_ne_bytes());
    m
}

/// The reply's `(reqid, status)`.
pub(super) fn decode(m: &[u8; BLOCK_MSG]) -> (u32, i64) {
    let reqid = u32::from_ne_bytes(m[4..8].try_into().unwrap());
    let status = i64::from_ne_bytes(m[24..32].try_into().unwrap());
    (reqid, status)
}

/// Whether `len` bytes at `sector` are a request this disk can take: whole
/// sectors, at least one, and inside the disk.
pub(super) fn in_range(sector: u64, len: usize, sectors: u64) -> bool {
    let n = (len / SECTOR) as u64;
    len.is_multiple_of(SECTOR) && n != 0 && sector <= sectors && n <= sectors - sector
}

/// What a request does while it waits: let the CPU go, and let it take
/// interrupts.
///
/// Kernel tasks run with interrupts masked. While this loop has the CPU, the
/// interrupt that would wake the driver -- its device's -- has nowhere to
/// land on this CPU, and the driver sleeps through the request until the
/// deadline. So each look opens a window, the same one the idle loop's poll
/// opens, and closes it again: the caller's state is what it was.
fn between_looks() {
    use kernel_hal::interrupt::{intr_get, intr_off, intr_on};
    if !intr_get() {
        intr_on();
        core::hint::spin_loop();
        intr_off();
        // And a TLB shootdown another CPU is waiting on: what every spin
        // with interrupts off does.
        kernel_hal::tlb_shootdown_pump();
    }
    kernel_hal::thread::yield_now_sync();
}

fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

impl ServedDisk {
    pub(super) fn new(fifo: Arc<Fifo>, window: Arc<VmObject>, sectors: u64) -> ZxResult<Self> {
        let window_len = window.len() / SECTOR * SECTOR;
        if window_len < MIN_WINDOW || sectors == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        // The FIFO must carry the driver's messages, or every write fails.
        fifo.read_buffer_elems(BLOCK_MSG, 1)?;
        Ok(Self {
            fifo,
            window,
            window_len,
            sectors,
            busy: AtomicBool::new(false),
            next_id: AtomicU32::new(0),
            stats: IoStats::new(),
        })
    }

    /// One request of at most `window_len` bytes, start to end: the window
    /// already holds what a write writes.
    fn call(&self, op: u32, sector: u64, len: usize) -> DeviceResult {
        let reqid = self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let msg = encode(op, reqid, (len / SECTOR) as u32, sector);
        let deadline = now_ms() + REPLY_TIMEOUT_MS;
        loop {
            match self.fifo.write(BLOCK_MSG, &msg, 1) {
                Ok(1) => break,
                Ok(_) | Err(ZxError::SHOULD_WAIT) => {}
                Err(e) => {
                    warn!("served disk: sending request {reqid}: {e:?}");
                    return Err(DeviceError::IoError);
                }
            }
            if now_ms() > deadline {
                warn!("served disk: request {reqid} could not be sent in time");
                return Err(DeviceError::IoError);
            }
            between_looks();
        }
        let mut back = [0u8; BLOCK_MSG];
        loop {
            match self.fifo.read(BLOCK_MSG, &mut back, 1) {
                Ok(1) => {
                    let (id, status) = decode(&back);
                    // One request at a time, so anything else is a driver
                    // answering a request it was never sent.
                    if id != reqid || status != 0 {
                        warn!(
                            "served disk: request {reqid} (op {op}, sector {sector}, {len} bytes): \
                             answered as {id}, status {status}"
                        );
                        return Err(DeviceError::IoError);
                    }
                    return Ok(());
                }
                Ok(_) | Err(ZxError::SHOULD_WAIT) => {}
                Err(e) => {
                    warn!("served disk: waiting for request {reqid}: {e:?}");
                    return Err(DeviceError::IoError);
                }
            }
            if now_ms() > deadline {
                warn!("served disk: request {reqid} was not answered in time");
                return Err(DeviceError::IoError);
            }
            between_looks();
        }
    }

    /// Take the disk for one request. A loop that yields, not a lock: the
    /// holder may itself be parked in [`ServedDisk::call`], and a waiter
    /// spinning on the same CPU would never let it back.
    fn take(&self) {
        while self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            between_looks();
        }
    }

    fn give_back(&self) {
        self.busy.store(false, Ordering::Release);
    }

    fn transfer(
        &self,
        block_id: usize,
        len: usize,
        mut each: impl FnMut(u64, usize, usize) -> DeviceResult,
    ) -> DeviceResult {
        if !in_range(block_id as u64, len, self.sectors) {
            debug!("served disk: {len} bytes at sector {block_id} refused");
            return Err(DeviceError::InvalidParam);
        }
        self.take();
        let mut done = 0;
        let mut r = Ok(());
        while done < len && r.is_ok() {
            let n = (len - done).min(self.window_len);
            r = each(block_id as u64 + (done / SECTOR) as u64, done, n);
            done += n;
        }
        self.give_back();
        r
    }
}

impl Scheme for ServedDisk {
    fn name(&self) -> &str {
        // The `virtio` family, so the node is a `vdX`: what is behind it is
        // a virtio disk, driven from userspace.
        "virtio-blk-served"
    }
}

impl BlockScheme for ServedDisk {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        let len = buf.len();
        let r = self.transfer(block_id, len, |sector, at, n| {
            self.call(OP_READ, sector, n)?;
            self.window.read(0, &mut buf[at..at + n]).map_err(|e| {
                warn!("served disk: reading the window: {e:?}");
                DeviceError::IoError
            })
        });
        self.stats.note_read(&r, len);
        r
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
        let len = buf.len();
        let r = self.transfer(block_id, len, |sector, at, n| {
            self.window.write(0, &buf[at..at + n]).map_err(|e| {
                warn!("served disk: writing the window: {e:?}");
                DeviceError::IoError
            })?;
            self.call(OP_WRITE, sector, n)
        });
        self.stats.note_write(&r, len);
        r
    }

    fn flush(&self) -> DeviceResult {
        // The driver answers a write once the device has it.
        Ok(())
    }

    fn block_count(&self) -> usize {
        self.sectors as usize
    }

    fn io_stats(&self) -> Option<IoSnapshot> {
        Some(self.stats.snapshot())
    }
}

/// [`ECLIPSE_BLOCK_SERVE`](super::ECLIPSE_BLOCK_SERVE): `args[0]` the FIFO end
/// the kernel keeps, `args[1]` the data window, `args[2]` the disk's size in
/// sectors. Both handles are the kernel's from here on, whatever the answer.
pub(super) fn block_serve(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    let (fifo, window) = (fifo?, window?);
    let _ = proc.remove_handle(args[0] as u32);
    let _ = proc.remove_handle(args[1] as u32);
    let disk = ServedDisk::new(fifo, window, args[2] as u64)?;
    if kernel_hal::drivers::hotplug::add(Device::Block(Arc::new(disk))) {
        Ok(())
    } else {
        Err(ZxError::NOT_SUPPORTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_the_drivers_layout() {
        let m = encode(OP_READ, 7, 8, 0x1_0000_0002);
        assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()), OP_READ);
        assert_eq!(u32::from_ne_bytes(m[8..12].try_into().unwrap()), 8);
        assert_eq!(u32::from_ne_bytes(m[12..16].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_ne_bytes(m[16..24].try_into().unwrap()),
            0x1_0000_0002
        );
        let mut back = m;
        back[24..32].copy_from_slice(&(-40i64).to_ne_bytes());
        assert_eq!(decode(&back), (7, -40));
    }

    #[test]
    fn a_request_is_whole_sectors_inside_the_disk() {
        assert!(in_range(0, 512, 1));
        assert!(in_range(8, 4096, 16));
        assert!(!in_range(9, 4096, 16), "one sector past the end");
        assert!(!in_range(0, 0, 16), "nothing to transfer");
        assert!(!in_range(0, 100, 16), "part of a sector");
        assert!(!in_range(17, 512, 16), "starts past the end");
        assert!(!in_range(u64::MAX, 512, 16), "and does not wrap");
    }
}
