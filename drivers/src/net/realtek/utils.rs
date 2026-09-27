// c906

/// The D1's C906 cache maintenance and the ordering fence the GMAC's DMA needs.
///
/// The bodies are RISC-V machine code emitted by hand -- `dcache.cpa`,
/// `dcache.ipa` and `sync.is` are T-Head extensions LLVM does not know -- so on
/// any other target they cannot be assembled at all. That is what kept this
/// whole driver behind `#[cfg(target_arch = "riscv64")]`, and therefore out of
/// every `cargo test`: see [`host`] below for the arm that lets the host build
/// compile it, and count what the driver asked the cache to do.
#[cfg(target_arch = "riscv64")]
mod riscv {
    use core::arch::asm;

    const L1_CACHE_BYTES: u64 = 64;
    const CACHE_LINE_SIZE: u64 = 64;

    // 注意start输入物理地址
    pub fn flush_dcache_range(start: u64, end: u64) {
        // CACHE_LINE 64对齐
        let end = (end + (CACHE_LINE_SIZE - 1)) & !(CACHE_LINE_SIZE - 1);

        // 地址对齐到L1 Cache的节
        let mut i: u64 = start & !(L1_CACHE_BYTES - 1);
        while i < end {
            unsafe {
                // 老风格的llvm asm
                // DCACHE 指定物理地址清脏表项
                // llvm_asm!("dcache.cpa $0"::"r"(i));

                // 新asm
                asm!(".long 0x0295000b", in("a0") i); // dcache.cpa a0, 因编译器无法识别该指令
            }

            i += L1_CACHE_BYTES;
        }

        unsafe {
            //llvm_asm!("sync.is");

            asm!(".long 0x01b0000b"); // sync.is
        }
    }

    // start 物理地址
    pub fn invalidate_dcache_range(start: u64, end: u64) {
        let end = (end + (CACHE_LINE_SIZE - 1)) & !(CACHE_LINE_SIZE - 1);
        let mut i: u64 = start & !(L1_CACHE_BYTES - 1);
        while i < end {
            unsafe {
                //llvm_asm!("dcache.ipa $0"::"r"(i)); // DCACHE 指定物理地址无效表项
                asm!(".long 0x02a5000b", in("a0") i); // dcache.ipa a0
            }

            i += L1_CACHE_BYTES;
        }

        unsafe {
            //llvm_asm!("sync.is");
            asm!(".long 0x01b0000b"); // sync.is
        }
    }

    pub fn fence_w() {
        unsafe {
            //llvm_asm!("fence ow, ow" ::: "memory");
            asm!("fence ow, ow");
        }
    }
}

/// The same three operations on a host build, recorded instead of performed.
///
/// A cache flush and a fence leave no trace a test could read, so the host arm
/// keeps a log of them. That is not decoration: the ordering the GMAC's DMA
/// needs is *which* operation came after which, and the only way to assert it
/// off a C906 is to look at the sequence the driver asked for.
#[cfg(not(target_arch = "riscv64"))]
pub mod host {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// One entry per cache/fence operation the driver performed, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CacheOp {
        /// `flush_cache(addr, size)`: clean dirty lines out to RAM.
        Flush(u64, u64),
        /// `invalidate_dcache(addr, size)`: drop stale lines so a DMA write is seen.
        Invalidate(u64, u64),
        /// `fence_w()`: order the stores before it against the stores after it.
        Fence,
    }

    /// The log is a process-global and this crate's tests run in parallel, so it
    /// is only ever switched on under the register-file turnstile that
    /// `rtl8211f::fake` holds -- one lock for the whole fake device, so there is
    /// no pair of locks to take in the wrong order.
    static LOG: spin::Mutex<alloc::vec::Vec<CacheOp>> = spin::Mutex::new(alloc::vec::Vec::new());
    static RECORDING: AtomicUsize = AtomicUsize::new(0);

    fn record(op: CacheOp) {
        if RECORDING.load(Ordering::SeqCst) != 0 {
            LOG.lock().push(op);
        }
    }

    /// Start recording from empty.
    #[cfg(test)]
    pub fn begin() {
        LOG.lock().clear();
        RECORDING.store(1, Ordering::SeqCst);
    }

    /// What has been recorded so far, in order.
    #[cfg(test)]
    pub fn ops() -> alloc::vec::Vec<CacheOp> {
        LOG.lock().clone()
    }

    /// Stop recording and let the log go.
    #[cfg(test)]
    pub fn end() {
        RECORDING.store(0, Ordering::SeqCst);
        LOG.lock().clear();
    }

    pub fn flush_dcache_range(start: u64, end: u64) {
        record(CacheOp::Flush(start, end.saturating_sub(start)));
    }

    pub fn invalidate_dcache_range(start: u64, end: u64) {
        record(CacheOp::Invalidate(start, end.saturating_sub(start)));
    }

    pub fn fence_w() {
        record(CacheOp::Fence);
    }
}

#[cfg(not(target_arch = "riscv64"))]
use host as arch;
#[cfg(target_arch = "riscv64")]
use riscv as arch;

pub fn flush_cache(addr: u64, size: u64) {
    flush_dcache_range(addr, addr + size);
}

pub fn invalidate_dcache(addr: u64, size: u64) {
    invalidate_dcache_range(addr, addr + size);
}

pub fn flush_dcache_range(start: u64, end: u64) {
    arch::flush_dcache_range(start, end);
}

pub fn invalidate_dcache_range(start: u64, end: u64) {
    arch::invalidate_dcache_range(start, end);
}

/// Order every store issued before this point against every store after it.
///
/// The GMAC reads a descriptor's OWN bit and the payload it points at with two
/// independent DMA reads, so on a weakly-ordered core the payload write must be
/// fenced against the descriptor write that publishes it.
pub fn fence_w() {
    arch::fence_w();
}

#[cfg(target_arch = "riscv64")]
mod timer {
    const MMIO_MTIME: *const u64 = 0x0200_BFF8 as *const u64;

    pub fn get_cycle() -> u64 {
        unsafe { MMIO_MTIME.read_volatile() }
    }

    // Timer, Freq = 24000000Hz
    // TIMER_CLOCK = (24 * 1000 * 1000)
    // 微秒(us)
    pub fn usdelay(us: u64) {
        let mut t1: u64 = get_cycle();
        let t2 = t1 + us * 24;

        while t2 >= t1 {
            t1 = get_cycle();
        }
    }

    // 毫秒(ms)
    #[allow(unused)]
    pub fn msdelay(ms: u64) {
        usdelay(ms * 1000);
    }
}

#[cfg(target_arch = "riscv64")]
#[allow(unused)]
pub use timer::{get_cycle, msdelay, usdelay};
