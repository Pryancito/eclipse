//! A naive LRU cache layer for `BlockDevice`
use super::*;
use alloc::{vec, vec::Vec};
use spin::{Mutex, MutexGuard};

pub struct BlockCache<T: BlockDevice> {
    device: T,
    bufs: Vec<Mutex<Buf>>,
    lru: Mutex<LRU>,
    /// Held while deciding which buffer a block lives in.
    ///
    /// Without it two threads can each conclude that a block is in no buffer
    /// and each take a fresh one for it, and then the cache holds two buffers
    /// for one block: a read can be answered from the stale one, and on
    /// write-back whichever is evicted last wins, which is not necessarily the
    /// newer. Only the decision is serialised, not the I/O: the buffer lock is
    /// what the caller holds across the device access, and this one is already
    /// released by then.
    install: Mutex<()>,
}

struct Buf {
    status: BufStatus,
    data: Vec<u8>,
}

enum BufStatus {
    /// buffer is unused
    Unused,
    /// buffer has been read from disk
    Valid(BlockId),
    /// buffer needs to be written to disk
    Dirty(BlockId),
}

impl<T: BlockDevice> BlockCache<T> {
    /// # Panics
    ///
    /// If `capacity` is zero. A cache with no buffers cannot answer a single
    /// read, and the alternative was worse: `LRU::new(0)` computed `size - 1`,
    /// which is an overflow panic in a debug build and, in a release one, the
    /// range `0..usize::MAX` collected into a `Vec`.
    pub fn new(device: T, capacity: usize) -> Self {
        assert!(capacity > 0, "a block cache needs at least one buffer");
        let mut bufs = Vec::new();
        bufs.resize_with(capacity, || {
            Mutex::new(Buf {
                status: BufStatus::Unused,
                data: vec![0; 1 << T::BLOCK_SIZE_LOG2 as usize],
            })
        });
        let lru = Mutex::new(LRU::new(capacity));
        BlockCache {
            device,
            bufs,
            lru,
            install: Mutex::new(()),
        }
    }

    /// Get a buffer for `block_id` with any status
    fn get_buf(&self, block_id: BlockId) -> Result<MutexGuard<'_, Buf>> {
        let (i, buf) = self._get_buf(block_id)?;
        self.lru.lock().visit(i);
        Ok(buf)
    }

    fn _get_buf(&self, block_id: BlockId) -> Result<(usize, MutexGuard<'_, Buf>)> {
        // Fast path: a buffer we can lock and that already holds the block is
        // the answer, whoever else is busy. Nobody can take it away, because
        // eviction needs this same lock.
        if let Some(found) = self.find(block_id, Mutex::try_lock) {
            return Ok(found);
        }
        // Slow path: a `try_lock` that failed says nothing about what that
        // buffer holds, so the scan above cannot conclude the block is absent.
        // Under `install` nobody else is installing, so a blocking scan can:
        // wait for each buffer in turn, and only then take a fresh one.
        let _installing = self.install.lock();
        if let Some(found) = self.find(block_id, |buf| Some(buf.lock())) {
            return Ok(found);
        }
        self.get_unused()
    }

    /// The buffer holding `block_id`, if `lock` can get at it.
    fn find<'a, F>(&'a self, block_id: BlockId, lock: F) -> Option<(usize, MutexGuard<'a, Buf>)>
    where
        F: Fn(&'a Mutex<Buf>) -> Option<MutexGuard<'a, Buf>>,
    {
        for (i, buf) in self.bufs.iter().enumerate() {
            if let Some(guard) = lock(buf) {
                match guard.status {
                    BufStatus::Valid(id) if id == block_id => return Some((i, guard)),
                    BufStatus::Dirty(id) if id == block_id => return Some((i, guard)),
                    _ => {}
                }
            }
        }
        None
    }

    /// Get an unused buffer
    fn get_unused(&self) -> Result<(usize, MutexGuard<'_, Buf>)> {
        for (i, buf) in self.bufs.iter().enumerate() {
            if let Some(lock) = buf.try_lock() {
                if let BufStatus::Unused = lock.status {
                    return Ok((i, lock));
                }
            }
        }
        let victim_id = self.lru.lock().victim();
        let mut victim = self.bufs[victim_id].lock();
        // A disk that refuses the write-back used to panic the kernel here,
        // and then panic a second time in `Drop` while the first was still
        // unwinding -- which is not a panic at all but an abort, with no
        // message. The caller asked for a block; a disk error is its answer.
        self.write_back(&mut victim)?;
        victim.status = BufStatus::Unused;
        Ok((victim_id, victim))
    }

    /// Write back data if buffer is dirty
    fn write_back(&self, buf: &mut Buf) -> Result<()> {
        if let BufStatus::Dirty(block_id) = buf.status {
            self.device.write_at(block_id, &buf.data)?;
            buf.status = BufStatus::Valid(block_id);
        }
        Ok(())
    }
}

impl<T: BlockDevice> Drop for BlockCache<T> {
    /// A last resort, and it cannot report anything: a destructor has nobody
    /// to report to, and a panic here is an abort whenever it lands during
    /// another unwind. Whoever cares whether the data reached the disk calls
    /// [`BlockDevice::sync`] and reads the answer before dropping the cache.
    fn drop(&mut self) {
        let _ = BlockDevice::sync(self);
    }
}

impl<T: BlockDevice> BlockDevice for BlockCache<T> {
    const BLOCK_SIZE_LOG2: u8 = T::BLOCK_SIZE_LOG2;

    fn read_at(&self, block_id: BlockId, buffer: &mut [u8]) -> Result<()> {
        let len = 1usize << Self::BLOCK_SIZE_LOG2;
        // A buffer that cannot hold a block is a caller error, and used to be
        // a panic from `copy_from_slice` -- inside the kernel, on behalf of
        // whoever asked for the read.
        if buffer.len() < len {
            return Err(DevError);
        }
        let mut buf = self.get_buf(block_id)?;
        if let BufStatus::Unused = buf.status {
            // read from device
            self.device.read_at(block_id, &mut buf.data)?;
            buf.status = BufStatus::Valid(block_id);
        }
        buffer[..len].copy_from_slice(&buf.data);
        Ok(())
    }

    fn write_at(&self, block_id: BlockId, buffer: &[u8]) -> Result<()> {
        let len = 1usize << Self::BLOCK_SIZE_LOG2;
        if buffer.len() < len {
            return Err(DevError);
        }
        let mut buf = self.get_buf(block_id)?;
        // The data first and the mark second reads better, but the two orders
        // are the same program: the check above makes `copy_from_slice`
        // infallible -- both sides are exactly one block -- and the buffer lock
        // is held across both, so no other thread can see between them.
        buf.data.copy_from_slice(&buffer[..len]);
        buf.status = BufStatus::Dirty(block_id);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        for buf in self.bufs.iter() {
            self.write_back(&mut buf.lock())?;
        }
        self.device.sync()?;
        Ok(())
    }
}

/// Doubly circular linked list LRU manager
///
/// Node `0` doubles as the list head: the most recently used buffer is
/// `next[0]` and the victim is `prev[0]`. So buffer 0 is never moved and never
/// chosen as a victim -- a cache of `n` buffers evicts among `n - 1` of them,
/// and buffer 0 keeps whatever block landed in it until an explicit
/// [`BlockDevice::sync`]. That costs one buffer and is not a correctness
/// problem; treating node 0 as an ordinary element would be one, because
/// `_list_remove(0)` would unlink the head and leave the ring pointing at
/// itself.
#[allow(clippy::upper_case_acronyms)]
struct LRU {
    prev: Vec<usize>,
    next: Vec<usize>,
}

impl LRU {
    /// # Panics
    ///
    /// If `size` is zero: there is no ring to build and no head to hang it
    /// from. [`BlockCache::new`] rejects that before getting here, which is
    /// where the message a caller can act on lives.
    fn new(size: usize) -> Self {
        assert!(size > 0, "an LRU ring needs at least one element");
        LRU {
            prev: (size - 1..size).chain(0..size - 1).collect(),
            next: (1..size).chain(0..1).collect(),
        }
    }
    /// Visit element `id`, move it to head.
    fn visit(&mut self, id: usize) {
        // `0` is the head itself, see the type's note.
        if id == 0 || id >= self.prev.len() {
            return;
        }
        self._list_remove(id);
        self._list_insert_head(id);
    }
    /// Get a victim at tail.
    fn victim(&self) -> usize {
        self.prev[0]
    }
    fn _list_remove(&mut self, id: usize) {
        let prev = self.prev[id];
        let next = self.next[id];
        self.prev[next] = prev;
        self.next[prev] = next;
    }
    fn _list_insert_head(&mut self, id: usize) {
        let head = self.next[0];
        self.prev[id] = 0;
        self.next[id] = head;
        self.next[0] = id;
        self.prev[head] = id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const BS: usize = 512;
    const BLOCKS: usize = 16;

    /// A RAM disk of [`BLOCKS`] blocks that can be told to fail, and that
    /// counts what reached it. `Arc<Ram>` is what implements [`BlockDevice`],
    /// so a test can keep looking at the disk after handing it to the cache.
    struct Ram {
        data: Mutex<Vec<u8>>,
        fail_writes: AtomicBool,
        fail_reads: AtomicBool,
        fail_syncs: AtomicBool,
        writes: AtomicUsize,
        reads: AtomicUsize,
        syncs: AtomicUsize,
        /// One counter per block, so a test can ask how many times the cache
        /// went to the disk for a particular one.
        reads_of: Vec<AtomicUsize>,
    }

    impl Ram {
        fn new() -> Arc<Self> {
            Self::of(BLOCKS)
        }
        fn of(blocks: usize) -> Arc<Self> {
            Arc::new(Ram {
                data: Mutex::new(vec![0u8; blocks * BS]),
                fail_writes: AtomicBool::new(false),
                fail_reads: AtomicBool::new(false),
                fail_syncs: AtomicBool::new(false),
                writes: AtomicUsize::new(0),
                reads: AtomicUsize::new(0),
                syncs: AtomicUsize::new(0),
                reads_of: (0..blocks).map(|_| AtomicUsize::new(0)).collect(),
            })
        }
        /// The first byte of a block, which is what every test stamps.
        fn at(&self, block: BlockId) -> u8 {
            self.data.lock()[block * BS]
        }
        fn put(&self, block: BlockId, byte: u8) {
            let mut data = self.data.lock();
            let begin = block * BS;
            data[begin..begin + BS].fill(byte);
        }
    }

    impl BlockDevice for Arc<Ram> {
        const BLOCK_SIZE_LOG2: u8 = 9;

        fn read_at(&self, block_id: BlockId, buf: &mut [u8]) -> Result<()> {
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(DevError);
            }
            let data = self.data.lock();
            let begin = block_id * BS;
            if begin + BS > data.len() || buf.len() < BS {
                return Err(DevError);
            }
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.reads_of[block_id].fetch_add(1, Ordering::SeqCst);
            buf[..BS].copy_from_slice(&data[begin..begin + BS]);
            Ok(())
        }

        fn write_at(&self, block_id: BlockId, buf: &[u8]) -> Result<()> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(DevError);
            }
            let mut data = self.data.lock();
            let begin = block_id * BS;
            if begin + BS > data.len() || buf.len() < BS {
                return Err(DevError);
            }
            // The whole point of one of these tests: a device is entitled to
            // write everything it is handed, so a caller that hands it more
            // than one block has asked for more than one block.
            let n = buf.len().min(data.len() - begin);
            self.writes.fetch_add(1, Ordering::SeqCst);
            data[begin..begin + n].copy_from_slice(&buf[..n]);
            Ok(())
        }

        fn sync(&self) -> Result<()> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            if self.fail_syncs.load(Ordering::SeqCst) {
                return Err(DevError);
            }
            Ok(())
        }
    }

    fn cache(capacity: usize) -> (Arc<Ram>, BlockCache<Arc<Ram>>) {
        let dev = Ram::new();
        (dev.clone(), BlockCache::new(dev, capacity))
    }

    /// How many buffers name `block`, which the cache's own invariant says is
    /// never more than one.
    ///
    /// Taken under the cache's own install lock, because a scan buffer by
    /// buffer is not one observation: without it, the count can add a buffer
    /// seen before an eviction to the buffer the block moved into after it, and
    /// report two where there was only ever one.
    fn holders<T: BlockDevice>(cache: &BlockCache<T>, block: BlockId) -> usize {
        let _installing = cache.install.lock();
        cache
            .bufs
            .iter()
            .filter(|b| match b.lock().status {
                BufStatus::Valid(id) | BufStatus::Dirty(id) => id == block,
                BufStatus::Unused => false,
            })
            .count()
    }

    fn index_of<T: BlockDevice>(cache: &BlockCache<T>, block: BlockId) -> Option<usize> {
        let _installing = cache.install.lock();
        cache.bufs.iter().position(|b| match b.lock().status {
            BufStatus::Valid(id) | BufStatus::Dirty(id) => id == block,
            BufStatus::Unused => false,
        })
    }

    /// Run `body` with a deadline, so a test that reaches a lock nobody will
    /// release fails by name instead of hanging the job with no hint inside it.
    ///
    /// The thread is **not** scoped on purpose: a scope re-joins its threads
    /// when it ends, so a scoped version waits for the very thread it has just
    /// given up on and hangs anyway. On a timeout the body is left where it is
    /// and the process exits with the rest of the suite.
    fn within(what: &str, secs: u64, body: impl FnOnce() + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            body();
            let _ = tx.send(());
        });
        match rx.recv_timeout(core::time::Duration::from_secs(secs)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                handle.join().expect("the body of the test failed");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("{} never returned: it is waiting on a lock", what)
            }
        }
    }

    /// A cache several threads can hold at once, for the tests that hand it to
    /// a thread with a deadline: those bodies have to own what they touch.
    fn shared(capacity: usize) -> (Arc<Ram>, Arc<BlockCache<Arc<Ram>>>) {
        let dev = Ram::new();
        (dev.clone(), Arc::new(BlockCache::new(dev, capacity)))
    }

    /// What a caught panic actually said, so a test can tell a deliberate
    /// refusal from the arithmetic falling over on its own.
    fn panic_message(err: &Box<dyn core::any::Any + Send>) -> String {
        err.downcast_ref::<&str>()
            .map(|s| String::from(*s))
            .or_else(|| err.downcast_ref::<String>().cloned())
            .unwrap_or_default()
    }

    /// Spin until `f` answers true, or fail after `secs`.
    fn until(what: &str, secs: u64, f: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + core::time::Duration::from_secs(secs);
        while !f() {
            assert!(std::time::Instant::now() < deadline, "{}", what);
            std::thread::yield_now();
        }
    }

    /// How long to wait before believing a thread is blocked and not merely
    /// slow to start. It only has to be long enough that a thread which was
    /// going to run has run.
    fn settle() {
        for _ in 0..2000 {
            std::thread::yield_now();
        }
    }

    fn read_block<T: BlockDevice>(cache: &BlockCache<T>, block: BlockId) -> Result<u8> {
        let mut out = vec![0u8; BS];
        BlockDevice::read_at(cache, block, &mut out)?;
        Ok(out[0])
    }

    fn write_block<T: BlockDevice>(cache: &BlockCache<T>, block: BlockId, byte: u8) -> Result<()> {
        BlockDevice::write_at(cache, block, &vec![byte; BS])
    }

    /// A counter in the first four bytes of a block, so a test can tell one
    /// version of a block from another more than 255 times.
    fn write_word<T: BlockDevice>(cache: &BlockCache<T>, block: BlockId, v: u32) {
        let mut out = vec![0u8; BS];
        out[..4].copy_from_slice(&v.to_le_bytes());
        BlockDevice::write_at(cache, block, &out).unwrap();
    }

    fn read_word<T: BlockDevice>(cache: &BlockCache<T>, block: BlockId) -> u32 {
        let mut out = vec![0u8; BS];
        BlockDevice::read_at(cache, block, &mut out).unwrap();
        u32::from_le_bytes([out[0], out[1], out[2], out[3]])
    }

    // ---- the cache holding one block in two buffers ----

    #[test]
    fn a_block_whose_buffer_is_busy_does_not_get_a_second_buffer() {
        let (_dev, cache) = shared(4);
        write_block(&cache, 7, 0xaa).unwrap();
        let i = index_of(&cache, 7).expect("block 7 is in no buffer");

        // Another thread is using block 7's buffer. The cache must wait for it,
        // not start a second buffer for the same block: two buffers for one
        // block means one of the two writes is lost.
        let done = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let held = cache.bufs[i].lock();
        {
            let (cache, started, done) = (cache.clone(), started.clone(), done.clone());
            std::thread::spawn(move || {
                started.store(true, Ordering::SeqCst);
                write_block(&cache, 7, 0xbb).unwrap();
                done.store(true, Ordering::SeqCst);
            });
        }
        until("the writer never started", 20, || {
            started.load(Ordering::SeqCst)
        });
        settle();
        // This is the whole test: the writer cannot have got anywhere, because
        // the only buffer that may hold block 7 is in our hand.
        assert!(
            !done.load(Ordering::SeqCst),
            "the write went to a second buffer for block 7"
        );
        drop(held);
        until("the write never finished", 20, || {
            done.load(Ordering::SeqCst)
        });

        assert_eq!(holders(&cache, 7), 1, "block 7 is in two buffers at once");
        assert_eq!(read_block(&cache, 7).unwrap(), 0xbb, "the write was lost");
    }

    #[test]
    fn the_newer_write_is_the_one_that_reaches_the_disk() {
        let (dev, cache) = shared(4);
        write_block(&cache, 3, 0x11).unwrap();
        let i = index_of(&cache, 3).unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let held = cache.bufs[i].lock();
        {
            let (cache, started, done) = (cache.clone(), started.clone(), done.clone());
            std::thread::spawn(move || {
                started.store(true, Ordering::SeqCst);
                write_block(&cache, 3, 0x22).unwrap();
                done.store(true, Ordering::SeqCst);
            });
        }
        until("the writer never started", 20, || {
            started.load(Ordering::SeqCst)
        });
        settle();
        drop(held);
        until("the write never finished", 20, || {
            done.load(Ordering::SeqCst)
        });
        BlockDevice::sync(&*cache).unwrap();
        assert_eq!(dev.at(3), 0x22, "an older copy of the block won");
    }

    #[test]
    fn threads_reading_the_same_block_at_once_read_the_disk_once() {
        // The ordinary case on a real disk with more than one CPU: the block is
        // in no buffer, several CPUs want it. Taking a buffer for a block and
        // filling it are two steps, and in between the buffer's own state says
        // nothing about which block it is being filled for -- so two threads
        // that both get past the search take two buffers for the same block,
        // and go to the disk twice for it. From then on a read of that block
        // can be answered from either, and a write reaches only one.
        let blocks = 96usize;
        let dev = Ram::of(blocks);
        let cache = Arc::new(BlockCache::new(dev.clone(), 4));
        within("the readers", 60, move || {
            for (b, counter) in dev.reads_of.iter().enumerate() {
                let start = Arc::new(std::sync::Barrier::new(8));
                let mut handles = Vec::new();
                for _ in 0..8 {
                    let (cache, start) = (cache.clone(), start.clone());
                    handles.push(std::thread::spawn(move || {
                        start.wait();
                        read_block(&cache, b).unwrap();
                    }));
                }
                for h in handles {
                    h.join().unwrap();
                }
                let n = counter.load(Ordering::SeqCst);
                assert_eq!(n, 1, "block {} was read from the disk {} times", b, n);
            }
        });
    }

    #[test]
    fn a_block_never_goes_backwards_under_readers_and_a_writer() {
        // One writer per block, so a block's value only ever goes up; four
        // readers watching all of them. A block held in two buffers shows here
        // as a read answered from the older one -- a value the reader had
        // already seen go past.
        let (_dev, cache) = shared(3);
        let blocks = 4usize;
        within("the contention run", 60, move || {
            let mut handles = Vec::new();
            for b in 0..blocks {
                let cache = cache.clone();
                handles.push(std::thread::spawn(move || {
                    for v in 1..=500u32 {
                        write_word(&cache, b, v);
                    }
                }));
            }
            for _ in 0..4 {
                let cache = cache.clone();
                handles.push(std::thread::spawn(move || {
                    let mut last = vec![0u32; blocks];
                    for i in 0..1500usize {
                        let b = i % blocks;
                        let got = read_word(&cache, b);
                        assert!(
                            got >= last[b],
                            "block {} went back from {} to {}",
                            b,
                            last[b],
                            got
                        );
                        last[b] = got;
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
    }

    #[test]
    fn many_threads_each_on_its_own_block_never_lose_a_write() {
        // Every thread owns one block and keeps stamping its own byte on it and
        // reading it straight back. There are more blocks than buffers, so the
        // threads are fighting over eviction the whole time: a write that gets
        // lost in the churn shows up as a read that answers with somebody
        // else's byte, or as the disk keeping an older one.
        let (dev, cache) = shared(3);
        let blocks = 6usize;
        let (seen, kept) = (dev.clone(), cache.clone());
        within("the stress run", 60, move || {
            let mut handles = Vec::new();
            for b in 0..blocks {
                let cache = cache.clone();
                handles.push(std::thread::spawn(move || {
                    let mine = 0x10 + b as u8;
                    for _ in 0..300 {
                        write_block(&cache, b, mine).unwrap();
                        let got = read_block(&cache, b).unwrap();
                        assert_eq!(got, mine, "block {} read back {:#x}", b, got);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
        BlockDevice::sync(&*kept).unwrap();
        for b in 0..blocks {
            assert_eq!(
                seen.at(b),
                0x10 + b as u8,
                "block {} kept an older value",
                b
            );
        }
    }

    #[test]
    fn a_hit_does_not_wait_for_an_unrelated_buffer() {
        let (_dev, cache) = shared(4);
        write_block(&cache, 1, 0xa1).unwrap();
        write_block(&cache, 2, 0xa2).unwrap();
        // Buffer for block 1 is busy; a read of block 2 must not block on it.
        let busy = index_of(&cache, 1).unwrap();
        let held = cache.bufs[busy].lock();
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let cache = cache.clone();
            std::thread::spawn(move || {
                let _ = tx.send(read_block(&cache, 2));
            });
        }
        let got = rx
            .recv_timeout(core::time::Duration::from_secs(20))
            .expect("the read of block 2 waited for a buffer holding block 1");
        drop(held);
        assert_eq!(got.unwrap(), 0xa2);
    }

    // ---- a disk that says no ----

    #[test]
    fn a_write_back_that_fails_is_an_error_and_not_a_panic() {
        let (dev, cache) = cache(2);
        write_block(&cache, 0, 1).unwrap();
        write_block(&cache, 1, 2).unwrap();
        dev.fail_writes.store(true, Ordering::SeqCst);
        // A third block: one of the two dirty buffers has to go to the disk.
        assert_eq!(
            write_block(&cache, 2, 3),
            Err(DevError),
            "a failed write-back was reported as a successful write"
        );
        // And the cache is still usable: the block that failed is still dirty.
        dev.fail_writes.store(false, Ordering::SeqCst);
        BlockDevice::sync(&cache).unwrap();
        assert_eq!(dev.at(0), 1);
        assert_eq!(dev.at(1), 2);
    }

    #[test]
    fn a_read_that_fails_is_an_error_and_leaves_no_buffer_claiming_the_block() {
        let (dev, cache) = cache(2);
        dev.fail_reads.store(true, Ordering::SeqCst);
        assert_eq!(read_block(&cache, 5), Err(DevError));
        assert_eq!(
            holders(&cache, 5),
            0,
            "a block the disk never gave us is in a buffer"
        );
        dev.fail_reads.store(false, Ordering::SeqCst);
        dev.put(5, 0x55);
        assert_eq!(read_block(&cache, 5).unwrap(), 0x55);
    }

    #[test]
    fn dropping_the_cache_over_a_disk_that_refuses_to_sync_does_not_panic() {
        let (dev, cache) = cache(2);
        write_block(&cache, 0, 1).unwrap();
        dev.fail_syncs.store(true, Ordering::SeqCst);
        drop(cache);
        assert_eq!(dev.syncs.load(Ordering::SeqCst), 1, "drop did not try");
    }

    #[test]
    fn dropping_the_cache_writes_the_dirty_blocks_back() {
        let (dev, cache) = cache(2);
        write_block(&cache, 4, 0x44).unwrap();
        assert_eq!(dev.at(4), 0, "the write reached the disk too early");
        drop(cache);
        assert_eq!(dev.at(4), 0x44, "a dirty block never reached the disk");
    }

    #[test]
    fn a_failed_write_back_during_eviction_does_not_abort_the_kernel() {
        // The two panics used to land one inside the other: the `expect` in
        // eviction, then `Drop`'s own `expect` while that one was unwinding,
        // which is an abort with no message at all.
        let (dev, cache) = cache(2);
        write_block(&cache, 0, 1).unwrap();
        write_block(&cache, 1, 2).unwrap();
        dev.fail_writes.store(true, Ordering::SeqCst);
        dev.fail_syncs.store(true, Ordering::SeqCst);
        assert_eq!(write_block(&cache, 2, 3), Err(DevError));
        drop(cache);
    }

    // ---- sizes the caller got wrong ----

    #[test]
    fn a_cache_with_no_buffers_is_refused_at_construction() {
        let err = std::panic::catch_unwind(|| {
            let _ = BlockCache::new(Ram::new(), 0);
        })
        .expect_err("a cache with no buffers was built");
        assert!(
            panic_message(&err).contains("at least one buffer"),
            "unhelpful message: {:?}",
            panic_message(&err)
        );
    }

    #[test]
    fn a_cache_of_exactly_one_buffer_works() {
        let (dev, cache) = cache(1);
        write_block(&cache, 0, 0xf0).unwrap();
        // The only buffer has to be evicted to make room for another block.
        write_block(&cache, 1, 0xf1).unwrap();
        assert_eq!(dev.at(0), 0xf0, "the only buffer was not written back");
        assert_eq!(read_block(&cache, 0).unwrap(), 0xf0);
    }

    #[test]
    fn a_buffer_too_short_for_a_block_is_an_error_and_not_a_panic() {
        let (_dev, cache) = cache(2);
        let mut small = [0u8; 8];
        assert_eq!(BlockDevice::read_at(&cache, 0, &mut small), Err(DevError));
        assert_eq!(BlockDevice::write_at(&cache, 0, &small), Err(DevError));
    }

    #[test]
    fn a_buffer_longer_than_a_block_reads_and_writes_exactly_one_block() {
        let (dev, cache) = cache(4);
        let mut long = vec![0xeeu8; 4 * BS];
        long[..BS].fill(0x77);
        BlockDevice::write_at(&cache, 1, &long).unwrap();
        BlockDevice::sync(&cache).unwrap();
        assert_eq!(dev.at(1), 0x77);
        assert_eq!(dev.at(2), 0, "the block after the target was written too");

        let mut out = vec![0x33u8; 4 * BS];
        BlockDevice::read_at(&cache, 1, &mut out).unwrap();
        assert_eq!(out[0], 0x77);
        assert_eq!(out[BS], 0x33, "the read spilled past one block");
    }

    // ---- what the cache is for ----

    #[test]
    fn a_second_read_of_the_same_block_does_not_touch_the_disk() {
        let (dev, cache) = cache(4);
        dev.put(6, 0x66);
        assert_eq!(read_block(&cache, 6).unwrap(), 0x66);
        let after_first = dev.reads.load(Ordering::SeqCst);
        assert_eq!(read_block(&cache, 6).unwrap(), 0x66);
        assert_eq!(dev.reads.load(Ordering::SeqCst), after_first);
    }

    #[test]
    fn a_write_is_visible_to_a_read_before_it_reaches_the_disk() {
        let (dev, cache) = cache(4);
        write_block(&cache, 8, 0x88).unwrap();
        assert_eq!(read_block(&cache, 8).unwrap(), 0x88);
        assert_eq!(dev.at(8), 0, "the write went straight through");
    }

    #[test]
    fn a_write_of_a_block_already_in_the_cache_does_not_read_it_again() {
        let (dev, cache) = cache(4);
        dev.put(9, 0x99);
        assert_eq!(read_block(&cache, 9).unwrap(), 0x99);
        let reads = dev.reads.load(Ordering::SeqCst);
        write_block(&cache, 9, 0x9a).unwrap();
        assert_eq!(dev.reads.load(Ordering::SeqCst), reads);
        assert_eq!(read_block(&cache, 9).unwrap(), 0x9a);
    }

    #[test]
    fn sync_leaves_every_buffer_clean_and_still_cached() {
        let (dev, cache) = cache(4);
        write_block(&cache, 0, 1).unwrap();
        write_block(&cache, 1, 2).unwrap();
        BlockDevice::sync(&cache).unwrap();
        assert_eq!((dev.at(0), dev.at(1)), (1, 2));
        for b in cache.bufs.iter() {
            assert!(
                !matches!(b.lock().status, BufStatus::Dirty(_)),
                "a buffer is still dirty after sync"
            );
        }
        // Still cached, so a read after the sync does not go to the disk.
        let reads = dev.reads.load(Ordering::SeqCst);
        assert_eq!(read_block(&cache, 0).unwrap(), 1);
        assert_eq!(dev.reads.load(Ordering::SeqCst), reads);
    }

    #[test]
    fn a_failed_sync_reports_the_error_and_keeps_the_block_dirty() {
        let (dev, cache) = cache(4);
        write_block(&cache, 0, 7).unwrap();
        dev.fail_writes.store(true, Ordering::SeqCst);
        assert_eq!(BlockDevice::sync(&cache), Err(DevError));
        dev.fail_writes.store(false, Ordering::SeqCst);
        BlockDevice::sync(&cache).unwrap();
        assert_eq!(dev.at(0), 7, "the block the failed sync owed was forgotten");
    }

    #[test]
    fn more_blocks_than_buffers_still_all_reach_the_disk() {
        let (dev, cache) = cache(3);
        for b in 0..BLOCKS {
            write_block(&cache, b, b as u8 + 1).unwrap();
        }
        BlockDevice::sync(&cache).unwrap();
        for b in 0..BLOCKS {
            assert_eq!(dev.at(b), b as u8 + 1, "block {} was lost", b);
        }
        // Three buffers cannot still be holding sixteen blocks, and no block
        // is in two of them.
        let still_cached = (0..BLOCKS).filter(|b| holders(&cache, *b) > 0).count();
        assert_eq!(still_cached, 3, "{} blocks in 3 buffers", still_cached);
    }

    #[test]
    fn every_block_reads_back_what_was_written_through_the_cache() {
        let (_dev, cache) = cache(3);
        for b in 0..BLOCKS {
            write_block(&cache, b, (b * 3) as u8).unwrap();
        }
        for b in 0..BLOCKS {
            assert_eq!(read_block(&cache, b).unwrap(), (b * 3) as u8);
        }
    }

    // ---- the LRU ring ----

    #[test]
    fn the_least_recently_used_buffer_is_the_one_evicted() {
        let (dev, cache) = cache(4);
        // Fill every buffer. Buffer 0 is the ring's head and never a victim,
        // so blocks 1, 2 and 3 are the candidates.
        for b in 0..4 {
            write_block(&cache, b, b as u8).unwrap();
        }
        // Touch 1 and 3, leaving 2 as the oldest of the three.
        assert_eq!(read_block(&cache, 1).unwrap(), 1);
        assert_eq!(read_block(&cache, 3).unwrap(), 3);
        // One more block has to push somebody out.
        write_block(&cache, 10, 10).unwrap();
        assert_eq!(holders(&cache, 2), 0, "the wrong buffer was evicted");
        assert_eq!(holders(&cache, 1), 1);
        assert_eq!(holders(&cache, 3), 1);
        assert_eq!(dev.at(2), 2, "the evicted block was not written back");
    }

    #[test]
    fn the_head_buffer_keeps_its_block_and_sync_is_what_writes_it_back() {
        let (dev, cache) = cache(3);
        write_block(&cache, 0, 0xc0).unwrap();
        assert_eq!(index_of(&cache, 0), Some(0), "block 0 is not in buffer 0");
        // Churn far more blocks than there are buffers.
        for b in 1..BLOCKS {
            write_block(&cache, b, b as u8).unwrap();
        }
        assert_eq!(index_of(&cache, 0), Some(0), "buffer 0 was evicted");
        assert_eq!(dev.at(0), 0, "buffer 0 was written back without a sync");
        BlockDevice::sync(&cache).unwrap();
        assert_eq!(dev.at(0), 0xc0);
    }

    #[test]
    fn visiting_the_head_or_a_buffer_that_does_not_exist_leaves_the_ring_whole() {
        let mut lru = LRU::new(4);
        let before = (lru.prev.clone(), lru.next.clone());
        lru.visit(0);
        lru.visit(4);
        lru.visit(usize::MAX);
        assert_eq!((lru.prev.clone(), lru.next.clone()), before);
        // And the ring still walks all the way round.
        let mut seen = vec![0usize];
        let mut at = lru.next[0];
        while at != 0 {
            seen.push(at);
            at = lru.next[at];
            assert!(seen.len() <= 4, "the ring does not close: {:?}", seen);
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }

    #[test]
    fn an_lru_ring_of_one_names_itself_as_the_victim() {
        let lru = LRU::new(1);
        assert_eq!(lru.victim(), 0);
    }

    #[test]
    fn an_lru_ring_of_none_is_refused_by_name() {
        // It has to be the ring's own refusal and not the arithmetic falling
        // over: `size - 1` is an overflow panic in a debug build, but in a
        // release one it wraps and `0..usize::MAX` gets collected into a `Vec`.
        // Only the message tells the two apart.
        let err = std::panic::catch_unwind(|| {
            let _ = LRU::new(0);
        })
        .expect_err("a ring of none was built");
        assert!(
            panic_message(&err).contains("at least one element"),
            "the arithmetic fell over instead of refusing: {:?}",
            panic_message(&err)
        );
    }

    #[test]
    fn visiting_a_buffer_makes_it_the_last_to_be_evicted() {
        let mut lru = LRU::new(4);
        assert_eq!(lru.victim(), 3);
        lru.visit(3);
        assert_eq!(lru.victim(), 2);
        lru.visit(2);
        assert_eq!(lru.victim(), 1);
        lru.visit(1);
        assert_eq!(lru.victim(), 3, "the ring did not come back round");
    }
}
