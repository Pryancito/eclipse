//! The driver's `IoMapper`: in the kernel it maps device memory into the
//! kernel's address space and answers where; here it maps a BAR of the
//! GPU this process holds into the process, whole, the first time any of
//! it is asked for, and registers it with `platform`, so that the
//! driver's own `phys_to_virt` of a BAR address (its registers, the
//! framebuffer in BAR 1) finds it too.
use std::sync::{Arc, Mutex};

use zcore_drivers::builder::IoMapper;

use crate::platform::{self, Range};

/// Map BAR `index` (`len` bytes), write-combining when asked: the address
/// it is mapped at, or `None`.
pub type MapBar = Box<dyn Fn(usize, usize, bool) -> Option<usize> + Send + Sync>;

/// A memory BAR of the GPU: its number, where the bus put it, how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    pub index: usize,
    pub phys: usize,
    pub len: usize,
}

pub struct BarMapper {
    bars: Vec<Bar>,
    map: MapBar,
    /// The BARs mapped so far, by number, and where.
    mapped: Mutex<Vec<(usize, usize)>>,
}

impl BarMapper {
    pub fn new(bars: Vec<Bar>, map: MapBar) -> Arc<BarMapper> {
        Arc::new(BarMapper {
            bars,
            map,
            mapped: Mutex::new(Vec::new()),
        })
    }
}

impl IoMapper for BarMapper {
    fn query_or_map(&self, paddr: usize, size: usize) -> Option<usize> {
        let bar = *self
            .bars
            .iter()
            .find(|b| paddr >= b.phys && paddr - b.phys + size.max(1) <= b.len)?;
        let mut mapped = self.mapped.lock().unwrap();
        let virt = match mapped.iter().find(|(i, _)| *i == bar.index) {
            Some(&(_, virt)) => virt,
            None => {
                // BAR 0 is registers; the rest are apertures of video
                // memory, which a CPU writes faster combined.
                let virt = (self.map)(bar.index, bar.len, bar.index != 0)?;
                platform::register(Range {
                    phys: bar.phys,
                    virt,
                    len: bar.len,
                });
                mapped.push((bar.index, virt));
                virt
            }
        };
        Some(virt + (paddr - bar.phys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_bar_is_mapped_once_whole_and_found_by_phys_to_virt() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let mapper = BarMapper::new(
            vec![
                Bar {
                    index: 0,
                    phys: 0xa000_0000,
                    len: 16 << 20,
                },
                Bar {
                    index: 1,
                    phys: 0x40_0000_0000,
                    len: 256 << 20,
                },
            ],
            Box::new(|index, len, combine| {
                CALLS.fetch_add(1, Ordering::Relaxed);
                assert_eq!(
                    combine,
                    index != 0,
                    "registers uncached, apertures combined"
                );
                assert!(len == 16 << 20 || len == 256 << 20);
                Some(0x7000_0000_0000 + index * 0x1_0000_0000)
            }),
        );
        assert_eq!(
            mapper.query_or_map(0xa000_0000, 16 << 20),
            Some(0x7000_0000_0000)
        );
        assert_eq!(
            mapper.query_or_map(0xa010_0000, 0x1000),
            Some(0x7000_0010_0000)
        );
        assert_eq!(
            CALLS.load(Ordering::Relaxed),
            1,
            "the same BAR, mapped once"
        );
        assert_eq!(
            mapper.query_or_map(0x40_0000_2000, 0x1000),
            Some(0x7001_0000_2000)
        );
        // What the driver does next with a BAR address: its own
        // phys_to_virt, which finds the mapping.
        extern "C" {
            fn drivers_phys_to_virt(paddr: usize) -> usize;
        }
        // SAFETY: platform's lookup, no memory touched.
        assert_eq!(
            unsafe { drivers_phys_to_virt(0xa000_0040) },
            0x7000_0000_0040
        );
        // Outside every BAR, or running past one: nothing.
        assert_eq!(mapper.query_or_map(0x9000_0000, 0x1000), None);
        assert_eq!(mapper.query_or_map(0xa0ff_f000, 0x2000), None);
        assert_eq!(CALLS.load(Ordering::Relaxed), 2);
    }
}
