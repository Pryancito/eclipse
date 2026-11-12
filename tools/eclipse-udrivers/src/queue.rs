//! A split virtqueue (virtio 1.1 §2.6) in two pages of DMA memory.
//!
//! The layout is the one legacy virtio fixes and modern virtio is simply
//! told about: the descriptor table and the available ring in the first
//! page, the used ring in the second.

use core::ptr;
use core::sync::atomic::{fence, Ordering};

use crate::PAGE;

/// The buffer continues in the descriptor `next` names.
pub const DESC_F_NEXT: u16 = 1;
/// The device writes into the buffer (it reads from it otherwise).
pub const DESC_F_WRITE: u16 = 2;

/// One descriptor: a buffer, by its device address.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// How many bytes of a queue's two pages a queue of `size` uses in each:
/// the descriptors and the available ring, and the used ring.
pub const fn footprint(size: u16) -> (usize, usize) {
    let size = size as usize;
    (16 * size + 4 + 2 * size + 2, 4 + 8 * size + 2)
}

/// Where the available ring starts, from the start of the queue.
pub const fn avail_offset(size: u16) -> usize {
    16 * size as usize
}

/// Where the used ring starts, from the start of the queue.
pub const USED_OFFSET: usize = PAGE;

/// The driver's side of one queue.
pub struct Queue {
    mem: *mut u8,
    size: u16,
    /// The used ring up to here has been taken.
    last_used: u16,
}

impl Queue {
    /// The queue of `size` descriptors in the two pages at `mem`.
    ///
    /// # Safety
    /// `mem` is the start of two zeroed pages, aligned to a page, that stay
    /// mapped and are this queue's alone for as long as it lives.
    pub unsafe fn new(mem: *mut u8, size: u16) -> Queue {
        let (first, second) = footprint(size);
        assert!(size.is_power_of_two() && first <= PAGE && second <= PAGE);
        Queue {
            mem,
            size,
            last_used: 0,
        }
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    /// Write descriptor `i`.
    pub fn set_desc(&mut self, i: u16, desc: Desc) {
        assert!(i < self.size);
        // SAFETY: inside the descriptor table (`new`), which is aligned.
        unsafe { ptr::write_volatile((self.mem as *mut Desc).add(i as usize), desc) }
    }

    /// Descriptor `i`, as it was written.
    pub fn desc(&self, i: u16) -> Desc {
        assert!(i < self.size);
        // SAFETY: as `set_desc`.
        unsafe { ptr::read_volatile((self.mem as *const Desc).add(i as usize)) }
    }

    fn avail_word(&self, word: usize) -> *mut u16 {
        // SAFETY: the available ring is `2 + size + 1` words in the first page.
        unsafe { (self.mem.add(avail_offset(self.size)) as *mut u16).add(word) }
    }

    fn used_word(&self, word: usize) -> *const u16 {
        // SAFETY: the used ring's two leading words, in the second page.
        unsafe { (self.mem.add(USED_OFFSET) as *const u16).add(word) }
    }

    /// How far the driver has offered: the available ring's index.
    pub fn avail_idx(&self) -> u16 {
        // SAFETY: word 1 of the available ring.
        unsafe { ptr::read_volatile(self.avail_word(1)) }
    }

    /// Put the chain that starts at descriptor `head` in the available
    /// ring, `ahead` places past what the device has been told of. It is
    /// the device's once [`Self::publish`] covers it.
    pub fn offer(&mut self, ahead: u16, head: u16) {
        let at = self.avail_idx().wrapping_add(ahead) % self.size;
        // SAFETY: a ring entry, `2 + at` words in.
        unsafe { ptr::write_volatile(self.avail_word(2 + at as usize), head) }
    }

    /// Tell the device of `n` more chains in the available ring.
    pub fn publish(&mut self, n: u16) {
        fence(Ordering::SeqCst);
        let idx = self.avail_idx().wrapping_add(n);
        // SAFETY: word 1 of the available ring.
        unsafe { ptr::write_volatile(self.avail_word(1), idx) };
        fence(Ordering::SeqCst);
    }

    /// How far the device has answered: the used ring's index.
    pub fn used_idx(&self) -> u16 {
        // SAFETY: word 1 of the used ring.
        unsafe { ptr::read_volatile(self.used_word(1)) }
    }

    /// The used ring up to here has been taken by the driver.
    pub fn last_used(&self) -> u16 {
        self.last_used
    }

    /// The oldest answer not yet taken: the head of its chain and how many
    /// bytes the device wrote. `None` when the device has answered nothing
    /// new.
    pub fn take_used(&mut self) -> Option<(u32, u32)> {
        if self.used_idx() == self.last_used {
            return None;
        }
        fence(Ordering::SeqCst);
        let at = (self.last_used % self.size) as usize;
        // SAFETY: element `at` of the used ring, two `u32`s past its header.
        let elem = unsafe {
            let ring = self.mem.add(USED_OFFSET + 4) as *const u32;
            (
                ptr::read_volatile(ring.add(2 * at)),
                ptr::read_volatile(ring.add(2 * at + 1)),
            )
        };
        self.last_used = self.last_used.wrapping_add(1);
        Some(elem)
    }

    /// Take everything up to `upto` at once, for a driver that knows what it
    /// offered and only waits for the count.
    pub fn take_upto(&mut self, upto: u16) {
        fence(Ordering::SeqCst);
        self.last_used = upto;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Two zeroed pages, aligned as DMA memory is.
    pub(crate) struct Pages(Vec<u64>);
    impl Pages {
        pub(crate) fn new(pages: usize) -> Pages {
            Pages(vec![0u64; pages * PAGE / 8])
        }
        pub(crate) fn ptr(&mut self) -> *mut u8 {
            self.0.as_mut_ptr() as *mut u8
        }
        pub(crate) fn bytes(&self) -> &[u8] {
            // SAFETY: the vector's own memory, as bytes.
            unsafe { core::slice::from_raw_parts(self.0.as_ptr() as *const u8, self.0.len() * 8) }
        }
        pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
            // SAFETY: as `bytes`.
            unsafe {
                core::slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, self.0.len() * 8)
            }
        }
    }

    fn word(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    #[test]
    fn a_descriptor_is_sixteen_bytes_laid_out_as_the_device_reads_it() {
        assert_eq!(core::mem::size_of::<Desc>(), 16);
        let mut pages = Pages::new(2);
        let mut q = unsafe { Queue::new(pages.ptr(), 64) };
        q.set_desc(
            3,
            Desc {
                addr: 0x1122_3344_5566_7788,
                len: 0x0a0b_0c0d,
                flags: DESC_F_NEXT | DESC_F_WRITE,
                next: 9,
            },
        );
        assert_eq!(q.desc(3).next, 9);
        let b = &pages.bytes()[48..64];
        assert_eq!(&b[..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&b[8..12], &0x0a0b_0c0du32.to_le_bytes());
        assert_eq!(&b[12..14], &3u16.to_le_bytes());
        assert_eq!(&b[14..16], &9u16.to_le_bytes());
    }

    #[test]
    fn the_rings_of_the_queues_the_drivers_use_fit_their_pages() {
        for size in [64u16, 128] {
            let (first, second) = footprint(size);
            assert!(first <= PAGE && second <= PAGE, "queue of {size}");
        }
        // The available ring follows the descriptors with no gap, which is
        // where legacy virtio looks for it.
        assert_eq!(avail_offset(64), 1024);
    }

    #[test]
    fn chains_are_the_devices_only_once_published() {
        let mut pages = Pages::new(2);
        let mut q = unsafe { Queue::new(pages.ptr(), 64) };
        q.offer(0, 6);
        q.offer(1, 9);
        let avail = avail_offset(64);
        assert_eq!(word(pages.bytes(), avail + 2), 0, "nothing published yet");
        assert_eq!(word(pages.bytes(), avail + 4), 6);
        assert_eq!(word(pages.bytes(), avail + 6), 9);
        q.publish(2);
        assert_eq!(word(pages.bytes(), avail + 2), 2);
        // The next offers go after those.
        q.offer(0, 12);
        assert_eq!(word(pages.bytes(), avail + 8), 12);
    }

    #[test]
    fn the_available_ring_wraps_at_the_queue_size_and_its_index_at_sixteen_bits() {
        let mut pages = Pages::new(2);
        let avail = avail_offset(64);
        // As after 65535 chains.
        pages.bytes_mut()[avail + 2..avail + 4].copy_from_slice(&0xffffu16.to_le_bytes());
        let mut q = unsafe { Queue::new(pages.ptr(), 64) };
        q.offer(0, 5); // place 0xffff % 64 = 63
        q.offer(1, 7); // place 0
        assert_eq!(word(pages.bytes(), avail + 4 + 2 * 63), 5);
        assert_eq!(word(pages.bytes(), avail + 4), 7);
        q.publish(2);
        assert_eq!(q.avail_idx(), 1);
    }

    #[test]
    fn answers_are_taken_in_order_and_once() {
        let mut pages = Pages::new(2);
        let mut q = unsafe { Queue::new(pages.ptr(), 64) };
        assert_eq!(q.take_used(), None);
        // The device answers two chains.
        let used = USED_OFFSET;
        let b = pages.bytes_mut();
        b[used + 4..used + 8].copy_from_slice(&3u32.to_le_bytes());
        b[used + 8..used + 12].copy_from_slice(&1514u32.to_le_bytes());
        b[used + 12..used + 16].copy_from_slice(&0u32.to_le_bytes());
        b[used + 16..used + 20].copy_from_slice(&60u32.to_le_bytes());
        b[used + 2..used + 4].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(q.used_idx(), 2);
        assert_eq!(q.take_used(), Some((3, 1514)));
        assert_eq!(q.take_used(), Some((0, 60)));
        assert_eq!(q.take_used(), None);
        assert_eq!(q.last_used(), 2);
    }
}
