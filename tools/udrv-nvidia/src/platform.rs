//! What `zcore-drivers` asks of the system, through the `extern "C"`
//! symbols that `kernel-hal/src/drivers.rs` defines inside the kernel.
//! Here a process defines them: the clock is the process's own clock,
//! the log goes to stderr, and DMA memory and device addresses come from
//! whatever the GPU's claim provides (`install_dma`), once there is one.
//!
//! The kernel turns a physical address into a pointer with one addition
//! (its linear map). A process has no linear map: every range it can
//! reach was mapped by it, so `drivers_phys_to_virt` looks the address up
//! in the ranges registered here (DMA buffers and BARs).
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

type PhysAddr = usize;
type VirtAddr = usize;

/// One range the process can reach: device address `phys` is the
/// process's `virt`, for `len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub phys: PhysAddr,
    pub virt: VirtAddr,
    pub len: usize,
}

/// Where DMA memory comes from: pages in, the mapped range out (or None).
pub type DmaAlloc = Box<dyn Fn(usize) -> Option<Range> + Send + Sync>;
/// And where it goes back: the range `DmaAlloc` gave.
pub type DmaFree = Box<dyn Fn(Range) + Send + Sync>;

struct Dma {
    alloc: DmaAlloc,
    free: DmaFree,
}

static DMA: OnceLock<Dma> = OnceLock::new();
static RANGES: Mutex<Vec<Range>> = Mutex::new(Vec::new());
const PAGE: usize = 4096;

/// Set where DMA memory comes from. Only once: a second call is refused.
/// Its caller is the GPU claim (`gpu`).
pub fn install_dma(alloc: DmaAlloc, free: DmaFree) -> Result<(), ()> {
    DMA.set(Dma { alloc, free }).map_err(|_| ())
}

/// Make `phys..phys+len` reachable through `drivers_phys_to_virt` (a BAR
/// the process mapped, for instance).
pub fn register(range: Range) {
    RANGES.lock().unwrap().push(range);
}

/// Forget the range that starts at `phys`, and return it.
pub fn unregister(phys: PhysAddr) -> Option<Range> {
    let mut ranges = RANGES.lock().unwrap();
    let i = ranges.iter().position(|r| r.phys == phys)?;
    Some(ranges.swap_remove(i))
}

fn lookup(phys: PhysAddr) -> Option<VirtAddr> {
    RANGES
        .lock()
        .unwrap()
        .iter()
        .find(|r| phys >= r.phys && phys - r.phys < r.len)
        .map(|r| r.virt + (phys - r.phys))
}

fn reverse(virt: VirtAddr) -> Option<PhysAddr> {
    RANGES
        .lock()
        .unwrap()
        .iter()
        .find(|r| virt >= r.virt && virt - r.virt < r.len)
        .map(|r| r.phys + (virt - r.virt))
}

fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

#[no_mangle]
extern "C" fn drivers_timer_now_as_micros() -> u64 {
    // The driver only measures intervals with it (timeouts, polls), so a
    // clock that starts with the process serves as well as the kernel's.
    start().elapsed().as_micros() as u64
}

#[no_mangle]
extern "C" fn drivers_klog_emit(priority: u8, msg: *const u8, len: usize) {
    if msg.is_null() {
        return;
    }
    // SAFETY: the caller hands a byte slice it owns for the call.
    let bytes = unsafe { core::slice::from_raw_parts(msg, len) };
    // Not written here: the driver makes these inside its timed stretches.
    crate::stderr_log::klog(priority, &String::from_utf8_lossy(bytes));
}

#[no_mangle]
extern "C" fn drivers_dma_alloc(pages: usize) -> PhysAddr {
    let Some(dma) = DMA.get() else { return 0 };
    match (dma.alloc)(pages) {
        Some(range) if range.len >= pages * PAGE => {
            register(range);
            range.phys
        }
        Some(range) => {
            (dma.free)(range);
            0
        }
        None => 0,
    }
}

#[no_mangle]
extern "C" fn drivers_dma_dealloc(paddr: PhysAddr, _pages: usize) -> i32 {
    match (DMA.get(), unregister(paddr)) {
        (Some(dma), Some(range)) => {
            (dma.free)(range);
            0
        }
        _ => -1,
    }
}

#[no_mangle]
extern "C" fn drivers_dma_mark_uncached(_paddr: PhysAddr, _pages: usize) -> i32 {
    // The mapping's cache policy is set when the claim maps the memory;
    // there is no kernel page table here to change afterwards.
    0
}

#[no_mangle]
extern "C" fn drivers_phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    // 0 for an address the process never mapped: the driver faults in its
    // own process, where the kernel would have read someone else's RAM.
    lookup(paddr).unwrap_or(0)
}

#[no_mangle]
extern "C" fn drivers_virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    reverse(vaddr).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_translate_both_ways() {
        register(Range {
            phys: 0x10_0000,
            virt: 0x7f00_0000,
            len: 0x2000,
        });
        assert_eq!(drivers_phys_to_virt(0x10_1234), 0x7f00_1234);
        assert_eq!(drivers_virt_to_phys(0x7f00_1234), 0x10_1234);
        assert_eq!(drivers_phys_to_virt(0x10_2000), 0);
        assert_eq!(unregister(0x10_0000).map(|r| r.len), Some(0x2000));
        assert_eq!(drivers_phys_to_virt(0x10_1234), 0);
    }

    #[test]
    fn dma_goes_through_the_installed_allocator() {
        // Before install: no memory, as a process without a claim has none.
        assert_eq!(drivers_dma_alloc(1), 0);
        let backing: &'static mut [u8] = Box::leak(vec![0u8; 3 * PAGE].into_boxed_slice());
        let base = backing.as_mut_ptr() as usize;
        install_dma(
            Box::new(move |pages| {
                (pages <= 3).then_some(Range {
                    phys: 0x4000_0000,
                    virt: base,
                    len: pages * PAGE,
                })
            }),
            Box::new(|_| {}),
        )
        .unwrap();
        assert!(install_dma(Box::new(|_| None), Box::new(|_| {})).is_err());
        assert_eq!(drivers_dma_alloc(4), 0);
        let p = drivers_dma_alloc(2);
        assert_eq!(p, 0x4000_0000);
        assert_eq!(drivers_phys_to_virt(p + 5), base + 5);
        assert_eq!(drivers_dma_dealloc(p, 2), 0);
        assert_eq!(drivers_dma_dealloc(p, 2), -1);
    }

    #[test]
    fn clock_moves_forward() {
        let a = drivers_timer_now_as_micros();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(drivers_timer_now_as_micros() >= a + 2000);
    }
}
