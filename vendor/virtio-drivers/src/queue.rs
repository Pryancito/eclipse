use core::mem::size_of;
use core::slice;
use core::sync::atomic::{fence, Ordering};

use super::*;
use crate::header::VirtIOHeader;
use bitflags::*;

use volatile::Volatile;

/// The mechanism for bulk data transport on virtio devices.
///
/// Each device can have zero or more virtqueues.
#[repr(C)]
pub struct VirtQueue<'a> {
    /// DMA guard
    dma: DMA,
    /// Descriptor table
    desc: &'a mut [Descriptor],
    /// Available ring
    avail: &'a mut AvailRing,
    /// Used ring
    used: &'a mut UsedRing,

    /// The index of queue
    queue_idx: u32,
    /// The size of queue
    queue_size: u16,
    /// The number of used queues.
    num_used: u16,
    /// The head desc index of the free list.
    free_head: u16,
    avail_idx: u16,
    last_used_idx: u16,
}

/// The end of the free list. Not a descriptor index.
///
/// The last descriptor's `next` used to be left at whatever the DMA block
/// happened to hold. Filling the queue completely walks the free list to that
/// descriptor, so `free_head` became that value, and the next recycle wrote it
/// into the tail of the free list -- from where a later `add` dereferenced it.
const NO_DESC: u16 = u16::MAX;

/// How many entries [`AvailRing`] and [`UsedRing`] have room for.
///
/// Both carry a fixed-size array, so the queue cannot be bigger than this no
/// matter what the device says `QueueNumMax` is. `VirtQueue::new` used to take
/// the caller's word for it and publish a queue of, say, 64 to the device; the
/// panic then landed on the 33rd request, far from the call that caused it.
const RING_CAPACITY: u16 = 32;

impl VirtQueue<'_> {
    /// Create a new VirtQueue.
    pub fn new(header: &mut VirtIOHeader, idx: usize, size: u16) -> Result<Self> {
        if header.queue_used(idx as u32) {
            return Err(Error::AlreadyUsed);
        }
        if !size.is_power_of_two() || header.max_queue_size() < size as u32 {
            return Err(Error::InvalidParam);
        }
        if size > RING_CAPACITY {
            return Err(Error::InvalidParam);
        }
        let layout = VirtQueueLayout::new(size);
        // alloc continuous pages
        let dma = DMA::new(layout.size / PAGE_SIZE)?;

        // The block comes from the kernel frame allocator, which recycles
        // frames and hands them back with the last owner's bytes still in
        // them. Everything below -- `avail.idx`, `used.idx`, both `flags`, the
        // whole descriptor table -- is read before it is ever written, so a
        // ring built on a recycled block starts with `can_pop()` already true
        // and the first `pop_used` walks a descriptor index the device never
        // wrote. On a fresh boot the frames happen to be zero and none of this
        // shows.
        unsafe { core::ptr::write_bytes(dma.vaddr() as *mut u8, 0, layout.size) };

        let desc =
            unsafe { slice::from_raw_parts_mut(dma.vaddr() as *mut Descriptor, size as usize) };
        let avail = unsafe { &mut *((dma.vaddr() + layout.avail_offset) as *mut AvailRing) };
        let used = unsafe { &mut *((dma.vaddr() + layout.used_offset) as *mut UsedRing) };

        // link descriptors together, and mark the end of the list
        for i in 0..(size - 1) {
            desc[i as usize].next.write(i + 1);
        }
        desc[size as usize - 1].next.write(NO_DESC);

        // Only now tell the device where the queue is: writing `QueuePFN` is
        // what puts it in service, and until the rings above are cleared there
        // is nothing there it could safely read.
        header.queue_set(idx as u32, size as u32, PAGE_SIZE as u32, dma.pfn());

        Ok(VirtQueue {
            dma,
            desc,
            avail,
            used,
            queue_size: size,
            queue_idx: idx as u32,
            num_used: 0,
            free_head: 0,
            avail_idx: 0,
            last_used_idx: 0,
        })
    }

    /// Add buffers to the virtqueue, return a token.
    ///
    /// Ref: linux virtio_ring.c virtqueue_add
    pub fn add(&mut self, inputs: &[&[u8]], outputs: &[&mut [u8]]) -> Result<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(Error::InvalidParam);
        }
        if inputs.len() + outputs.len() + self.num_used as usize > self.queue_size as usize {
            return Err(Error::BufferTooSmall);
        }

        // allocate descriptors from free list
        let head = self.free_head;
        let mut last = self.free_head;
        for input in inputs.iter() {
            let slot = self.take_free()?;
            let desc = &mut self.desc[slot as usize];
            desc.set_buf(input);
            desc.flags.write(DescFlags::NEXT);
            last = slot;
        }
        for output in outputs.iter() {
            let slot = self.take_free()?;
            let desc = &mut self.desc[slot as usize];
            desc.set_buf(output);
            desc.flags.write(DescFlags::NEXT | DescFlags::WRITE);
            last = slot;
        }
        // set last_elem.next = NULL
        {
            let desc = &mut self.desc[last as usize];
            let mut flags = desc.flags.read();
            flags.remove(DescFlags::NEXT);
            desc.flags.write(flags);
        }
        let avail_slot = self.avail_idx & (self.queue_size - 1);
        self.avail.ring[avail_slot as usize].write(head);

        // write barrier
        fence(Ordering::SeqCst);

        // increase head of avail ring
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.avail.idx.write(self.avail_idx);
        Ok(head)
    }

    /// Whether there is a used element that can pop.
    pub fn can_pop(&self) -> bool {
        self.last_used_idx != self.used.idx.read()
    }

    /// The number of free descriptors.
    pub fn available_desc(&self) -> usize {
        (self.queue_size - self.num_used) as usize
    }

    /// Take the next descriptor off the free list.
    ///
    /// The capacity check in [`add`](Self::add) means this never runs out, but
    /// the free list lives in memory the device can reach, so a descriptor's
    /// `next` is not a fact -- it is a value to be checked before it is used
    /// as an index.
    fn take_free(&mut self) -> Result<u16> {
        let slot = self.free_head;
        if slot >= self.queue_size {
            return Err(Error::InvalidParam);
        }
        self.free_head = self.desc[slot as usize].next.read();
        self.num_used += 1;
        Ok(slot)
    }

    /// Walk the chain from `head`, checking it before anything is changed.
    ///
    /// Returns its tail. The chain is the device's word: `pop_used` gets
    /// `head` out of the used ring, which the device writes. An index past the
    /// table used to be an out-of-bounds panic, a chain that loops used to
    /// spin forever while `num_used` underflowed, and a chain longer than the
    /// outstanding descriptors used to underflow it too. All three are the
    /// device saying something impossible, which is an I/O error, not a reason
    /// to bring the kernel down.
    fn check_chain(&self, head: u16) -> Result<u16> {
        let mut index = head;
        for length in 1..=self.num_used {
            if index >= self.queue_size {
                return Err(Error::InvalidParam);
            }
            let desc = &self.desc[index as usize];
            if !desc.flags.read().contains(DescFlags::NEXT) {
                let _ = length;
                return Ok(index);
            }
            index = desc.next.read();
        }
        Err(Error::InvalidParam)
    }

    /// Recycle descriptors in the list specified by head.
    ///
    /// This will push all linked descriptors at the front of the free list.
    fn recycle_descriptors(&mut self, head: u16) -> Result {
        let tail = self.check_chain(head)?;
        let origin_free_head = self.free_head;
        self.free_head = head;
        let mut index = head;
        loop {
            self.num_used -= 1;
            if index == tail {
                self.desc[index as usize].next.write(origin_free_head);
                return Ok(());
            }
            index = self.desc[index as usize].next.read();
        }
    }

    /// Get a token from device used buffers, return (token, len).
    ///
    /// Ref: linux virtio_ring.c virtqueue_get_buf_ctx
    pub fn pop_used(&mut self) -> Result<(u16, u32)> {
        if !self.can_pop() {
            return Err(Error::NotReady);
        }
        // read barrier
        fence(Ordering::SeqCst);

        let last_used_slot = self.last_used_idx & (self.queue_size - 1);
        let id = self.used.ring[last_used_slot as usize].id.read();
        let len = self.used.ring[last_used_slot as usize].len.read();
        // `id` is a `u32` the device wrote. Narrowing it to `u16` first turned
        // a wild value into a plausible one.
        if id >= self.queue_size as u32 {
            return Err(Error::InvalidParam);
        }
        let index = id as u16;

        self.recycle_descriptors(index)?;
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        Ok((index, len))
    }
}

/// The inner layout of a VirtQueue.
///
/// Ref: 2.6.2 Legacy Interfaces: A Note on Virtqueue Layout
struct VirtQueueLayout {
    avail_offset: usize,
    used_offset: usize,
    size: usize,
}

impl VirtQueueLayout {
    fn new(queue_size: u16) -> Self {
        assert!(
            queue_size.is_power_of_two(),
            "queue size should be a power of 2"
        );
        let queue_size = queue_size as usize;
        let desc = size_of::<Descriptor>() * queue_size;
        let avail = size_of::<u16>() * (3 + queue_size);
        let used = size_of::<u16>() * 3 + size_of::<UsedElem>() * queue_size;
        VirtQueueLayout {
            avail_offset: desc,
            used_offset: align_up(desc + avail),
            size: align_up(desc + avail) + align_up(used),
        }
    }
}

#[repr(C, align(16))]
#[derive(Debug)]
struct Descriptor {
    addr: Volatile<u64>,
    len: Volatile<u32>,
    flags: Volatile<DescFlags>,
    next: Volatile<u16>,
}

impl Descriptor {
    fn set_buf(&mut self, buf: &[u8]) {
        self.addr.write(virt_to_phys(buf.as_ptr() as usize) as u64);
        self.len.write(buf.len() as u32);
    }
}

bitflags! {
    /// Descriptor flags
    struct DescFlags: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
    }
}

/// The driver uses the available ring to offer buffers to the device:
/// each ring entry refers to the head of a descriptor chain.
/// It is only written by the driver and read by the device.
#[repr(C)]
#[derive(Debug)]
struct AvailRing {
    flags: Volatile<u16>,
    /// A driver MUST NOT decrement the idx.
    idx: Volatile<u16>,
    ring: [Volatile<u16>; 32], // actual size: queue_size
    used_event: Volatile<u16>, // unused
}

/// The used ring is where the device returns buffers once it is done with them:
/// it is only written to by the device, and read by the driver.
#[repr(C)]
#[derive(Debug)]
struct UsedRing {
    flags: Volatile<u16>,
    idx: Volatile<u16>,
    ring: [UsedElem; 32],       // actual size: queue_size
    avail_event: Volatile<u16>, // unused
}

#[repr(C)]
#[derive(Debug)]
struct UsedElem {
    id: Volatile<u32>,
    len: Volatile<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dev::{fake_header, Ring};

    /// Run `body` with a deadline, so a test that reaches a loop nobody is
    /// going to end fails with its own name instead of hanging the job.
    fn within(what: &'static str, secs: u64, body: impl FnOnce() + Send + 'static) {
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
                panic!(
                    "{} never returned: it is going round a chain that does not end",
                    what
                )
            }
        }
    }

    /// A queue of `size` on a device that allows 256, plus the device's view of
    /// its rings.
    fn queue(size: u16) -> (VirtQueue<'static>, Ring) {
        let header = fake_header(2, 256);
        let q = VirtQueue::new(header, 0, size).expect("the queue was refused");
        let ring = Ring::at(q.dma.paddr(), size);
        (q, ring)
    }

    // ---- what the device is handed -------------------------------------

    #[test]
    fn a_fresh_queue_has_nothing_to_pop() {
        // The DMA block arrives full of the last owner's bytes. Before this
        // was fixed, `used.idx` read as poison, `can_pop` was immediately true
        // and the first request popped a completion that never happened.
        let (q, _ring) = queue(16);
        assert!(!q.can_pop(), "a queue nobody used says the device answered");
    }

    #[test]
    fn a_fresh_queue_hands_the_device_cleared_rings() {
        let (_q, ring) = queue(16);
        assert_eq!(ring.avail_idx(), 0, "the available ring starts part-used");
        assert_eq!(ring.used_idx(), 0, "the used ring starts part-used");
        assert_eq!(ring.avail_flags(), 0);
        assert_eq!(ring.used_flags(), 0);
    }

    #[test]
    fn the_free_list_ends_at_the_last_descriptor() {
        // It used to end at whatever the recycled block held there, which a
        // full queue then wrote into the tail of the free list.
        let (q, _ring) = queue(16);
        for i in 0..15u16 {
            assert_eq!(q.desc[i as usize].next.read(), i + 1);
        }
        assert_eq!(q.desc[15].next.read(), NO_DESC);
    }

    #[test]
    fn the_queue_is_published_with_the_size_and_alignment_it_was_built_with() {
        let header = fake_header(2, 256);
        let q = VirtQueue::new(header, 0, 16).unwrap();
        assert_eq!(header.fake_queue_size(), (16, PAGE_SIZE as u32));
        assert_eq!(
            header.queue_physical_page_number(0) as usize,
            q.dma.paddr() >> 12
        );
    }

    #[test]
    fn the_guest_page_size_is_what_the_layout_assumes() {
        let header = fake_header(2, 256);
        header.begin_init(|_| 0);
        assert_eq!(header.fake_guest_page_size(), PAGE_SIZE as u32);
    }

    // ---- what `new` refuses ---------------------------------------------

    #[test]
    fn a_queue_bigger_than_the_ring_is_refused() {
        // `AvailRing`/`UsedRing` carry a fixed 32-entry array. A queue of 64
        // was accepted and published to the device, and then the 33rd request
        // indexed past the array: "index out of bounds: the len is 32 but the
        // index is 32", inside the kernel, long after the call that caused it.
        let header = fake_header(2, 256);
        assert_eq!(
            VirtQueue::new(header, 0, 64).err(),
            Some(Error::InvalidParam)
        );
    }

    #[test]
    fn a_queue_exactly_the_size_of_the_ring_is_allowed() {
        // 32 is what `VirtIOInput` asks for, so the boundary has to be open.
        let header = fake_header(18, 256);
        assert!(VirtQueue::new(header, 0, RING_CAPACITY).is_ok());
    }

    #[test]
    fn a_queue_size_that_is_not_a_power_of_two_is_refused() {
        let header = fake_header(2, 256);
        assert_eq!(
            VirtQueue::new(header, 0, 12).err(),
            Some(Error::InvalidParam)
        );
    }

    #[test]
    fn a_queue_bigger_than_the_device_allows_is_refused() {
        let header = fake_header(2, 8);
        assert_eq!(
            VirtQueue::new(header, 0, 16).err(),
            Some(Error::InvalidParam)
        );
    }

    #[test]
    fn a_queue_that_is_already_in_service_is_refused() {
        let header = fake_header(2, 256);
        let _first = VirtQueue::new(header, 0, 16).unwrap();
        assert_eq!(
            VirtQueue::new(header, 0, 16).err(),
            Some(Error::AlreadyUsed)
        );
    }

    #[test]
    fn a_second_queue_on_another_index_is_allowed() {
        let header = fake_header(3, 256);
        let _rx = VirtQueue::new(header, 0, 2).unwrap();
        header.fake_forget_queue_pfn();
        assert!(VirtQueue::new(header, 1, 2).is_ok());
    }

    // ---- offering buffers ------------------------------------------------

    #[test]
    fn add_with_nothing_to_offer_is_refused() {
        let (mut q, _ring) = queue(16);
        assert_eq!(q.add(&[], &[]).err(), Some(Error::InvalidParam));
    }

    #[test]
    fn add_beyond_the_queue_size_is_refused_before_anything_is_taken() {
        let (mut q, _ring) = queue(2);
        let mut a = [0u8; 4];
        let mut b = [0u8; 4];
        assert_eq!(
            q.add(&[b"x"], &[&mut a[..], &mut b[..]]).err(),
            Some(Error::BufferTooSmall)
        );
        assert_eq!(q.available_desc(), 2, "a refused add took descriptors");
    }

    #[test]
    fn the_device_sees_the_buffers_the_driver_offered() {
        let (mut q, ring) = queue(16);
        let request = b"read sector 7";
        let mut answer = [0u8; 512];
        let head = q.add(&[&request[..]], &[&mut answer[..]]).unwrap();
        assert_eq!(ring.avail_idx(), 1);
        assert_eq!(ring.avail_entry(0), head);
        let chain = ring.chain(head);
        assert_eq!(chain.len(), 2, "the chain is not two descriptors long");
        assert_eq!(chain[0], (request.as_ptr() as usize, request.len(), false));
        assert_eq!(chain[1], (answer.as_ptr() as usize, answer.len(), true));
    }

    #[test]
    fn only_the_outputs_are_marked_writable() {
        let (mut q, ring) = queue(16);
        let mut out = [0u8; 8];
        let head = q.add(&[b"a", b"b"], &[&mut out[..]]).unwrap();
        let writable: Vec<bool> = ring.chain(head).iter().map(|d| d.2).collect();
        assert_eq!(writable, vec![false, false, true]);
    }

    #[test]
    fn the_chain_ends_at_its_last_descriptor() {
        let (mut q, ring) = queue(16);
        let mut out = [0u8; 8];
        let head = q.add(&[b"a"], &[&mut out[..]]).unwrap();
        // `chain` walks the NEXT flag and refuses to go round for ever; if the
        // last descriptor still had NEXT set it would walk into the free list.
        assert_eq!(ring.chain(head).len(), 2);
    }

    #[test]
    fn available_desc_counts_what_is_left() {
        let (mut q, _ring) = queue(16);
        assert_eq!(q.available_desc(), 16);
        let mut out = [0u8; 8];
        let head = q.add(&[b"a"], &[&mut out[..]]).unwrap();
        assert_eq!(q.available_desc(), 14);
        q.recycle_descriptors(head).unwrap();
        assert_eq!(q.available_desc(), 16);
    }

    #[test]
    fn the_available_ring_wraps_at_the_queue_size() {
        let (mut q, ring) = queue(2);
        let mut out = [0u8; 4];
        for expected in 1..=5u16 {
            let head = q.add(&[], &[&mut out[..]]).unwrap();
            assert_eq!(ring.avail_idx(), expected);
            assert_eq!(ring.avail_entry(expected - 1), head);
            q.recycle_descriptors(head).unwrap();
        }
    }

    // ---- taking them back ------------------------------------------------

    #[test]
    fn pop_used_on_a_queue_the_device_has_not_answered_is_not_ready() {
        let (mut q, _ring) = queue(16);
        assert_eq!(q.pop_used().err(), Some(Error::NotReady));
    }

    #[test]
    fn a_round_trip_gives_back_the_token_and_the_length() {
        let (mut q, ring) = queue(16);
        let mut answer = [0u8; 8];
        let head = q.add(&[b"req"], &[&mut answer[..]]).unwrap();
        let written = ring.fill(head, b"hello!!!");
        ring.complete(head, written);
        assert!(q.can_pop());
        assert_eq!(q.pop_used().unwrap(), (head, 8));
        assert_eq!(&answer, b"hello!!!");
        assert_eq!(q.available_desc(), 16, "the chain was not recycled");
    }

    #[test]
    fn two_chains_come_back_in_the_order_the_device_chose() {
        let (mut q, ring) = queue(16);
        let mut first = [0u8; 4];
        let mut second = [0u8; 4];
        let a = q.add(&[b"1"], &[&mut first[..]]).unwrap();
        let b = q.add(&[b"2"], &[&mut second[..]]).unwrap();
        assert_ne!(a, b);
        ring.complete(b, 0);
        ring.complete(a, 0);
        assert_eq!(q.pop_used().unwrap().0, b);
        assert_eq!(q.pop_used().unwrap().0, a);
    }

    #[test]
    fn a_full_queue_is_refused_and_works_again_once_something_comes_back() {
        // This is the one that walks the free list to its end. With the end
        // left uninitialised, `free_head` became the recycled block's bytes,
        // the recycle wrote them into the tail of the free list, and the add
        // after that indexed the descriptor table with them.
        let (mut q, ring) = queue(16);
        let mut bufs: Vec<[u8; 4]> = vec![[0u8; 4]; 16];
        let mut heads = Vec::new();
        for buf in bufs.iter_mut() {
            heads.push(q.add(&[], &[&mut buf[..]]).unwrap());
        }
        assert_eq!(q.available_desc(), 0);
        let mut one_more = [0u8; 4];
        assert_eq!(
            q.add(&[], &[&mut one_more[..]]).err(),
            Some(Error::BufferTooSmall)
        );
        for head in &heads {
            ring.complete(*head, 0);
        }
        for _ in 0..16 {
            q.pop_used().unwrap();
        }
        assert_eq!(q.available_desc(), 16);
        // and round again, which is where the corrupt tail used to land
        for buf in bufs.iter_mut() {
            q.add(&[], &[&mut buf[..]]).unwrap();
        }
        assert_eq!(q.available_desc(), 0);
    }

    // ---- a device that says something impossible -------------------------

    #[test]
    fn an_id_past_the_descriptor_table_is_an_error_not_a_panic() {
        // "index out of bounds: the len is 16 but the index is 9999", inside
        // the kernel, on the word of the device.
        let (mut q, ring) = queue(16);
        let mut out = [0u8; 4];
        let _head = q.add(&[b"req"], &[&mut out[..]]).unwrap();
        ring.complete_raw(9999, 0);
        assert_eq!(q.pop_used().err(), Some(Error::InvalidParam));
    }

    #[test]
    fn an_id_that_only_looks_small_after_narrowing_is_an_error() {
        // The used ring's `id` is 32 bits and was narrowed to `u16` before
        // anything looked at it, so 0x1_0003 arrived as a perfectly ordinary 3.
        let (mut q, ring) = queue(16);
        let mut out = [0u8; 4];
        let _head = q.add(&[b"req"], &[&mut out[..]]).unwrap();
        ring.complete_raw(0x1_0003, 0);
        assert_eq!(q.pop_used().err(), Some(Error::InvalidParam));
    }

    #[test]
    fn a_chain_the_device_pointed_at_itself_is_an_error_not_a_hang() {
        within("a chain that loops", 20, || {
            let (mut q, ring) = queue(16);
            let mut out = [0u8; 4];
            let head = q.add(&[b"req"], &[&mut out[..]]).unwrap();
            // the first descriptor now points back at itself, still with NEXT
            q.desc[head as usize].flags.write(DescFlags::NEXT);
            q.desc[head as usize].next.write(head);
            ring.complete(head, 0);
            assert_eq!(q.pop_used().err(), Some(Error::InvalidParam));
        });
    }

    #[test]
    fn a_chain_longer_than_what_is_outstanding_is_an_error() {
        // Two descriptors are out, but the chain the device names runs through
        // three. That used to take `num_used` below zero.
        let (mut q, ring) = queue(16);
        let mut out = [0u8; 4];
        let head = q.add(&[b"req"], &[&mut out[..]]).unwrap();
        let tail = q.desc[head as usize].next.read();
        q.desc[tail as usize].flags.write(DescFlags::NEXT);
        q.desc[tail as usize].next.write(5);
        q.desc[5].flags.write(DescFlags::NEXT);
        q.desc[5].next.write(6);
        q.desc[6].flags.write(DescFlags::NEXT);
        q.desc[6].next.write(7);
        ring.complete(head, 0);
        assert_eq!(q.pop_used().err(), Some(Error::InvalidParam));
    }

    #[test]
    fn a_descriptor_the_device_scribbled_is_an_error_not_a_panic() {
        // The free list lives in the DMA block, which the device can reach. A
        // `next` that leaves the table used to be indexed straight into the
        // descriptor slice: "index out of bounds", inside the kernel, on the
        // word of the device.
        let (mut q, _ring) = queue(16);
        let mut out = [0u8; 4];
        q.desc[0].next.write(500);
        // the first descriptor is taken, then `free_head` is 500
        assert_eq!(
            q.add(&[b"a"], &[&mut out[..]]).err(),
            Some(Error::InvalidParam)
        );
    }

    #[test]
    fn a_chain_that_leaves_the_table_mid_way_is_an_error() {
        // Same memory, the other direction: the head the device names is in
        // range and a `next` further along is not. Three descriptors are
        // outstanding, so the walk still has budget when it reaches the bad
        // link -- which is what makes this about the bound and not about
        // running out of descriptors.
        let (mut q, ring) = queue(16);
        let mut out = [0u8; 4];
        let head = q.add(&[b"a", b"b"], &[&mut out[..]]).unwrap();
        let second = q.desc[head as usize].next.read();
        q.desc[second as usize].next.write(900);
        ring.complete(head, 0);
        assert_eq!(q.pop_used().err(), Some(Error::InvalidParam));
    }

    #[test]
    fn a_completion_with_nothing_outstanding_is_an_error() {
        let (mut q, ring) = queue(16);
        ring.complete_raw(0, 0);
        assert_eq!(q.pop_used().err(), Some(Error::InvalidParam));
        assert_eq!(q.available_desc(), 16);
    }

    #[test]
    fn a_refused_completion_frees_nothing_and_stops_the_queue() {
        // The bad entry stays at the head of the used ring on purpose, so the
        // queue stops instead of guessing which chain the device meant. Linux
        // does the same (`BAD_RING`, and the queue is marked broken): a device
        // that says something impossible has stopped being a device, and
        // carrying on with its next answer is how one bad completion turns
        // into a freed descriptor that is still in flight.
        let (mut q, ring) = queue(16);
        let mut out = [0u8; 4];
        let _head = q.add(&[b"req"], &[&mut out[..]]).unwrap();
        ring.complete_raw(9999, 0);
        assert_eq!(q.pop_used().err(), Some(Error::InvalidParam));
        assert_eq!(q.available_desc(), 14, "a refused pop freed descriptors");
        assert_eq!(
            q.pop_used().err(),
            Some(Error::InvalidParam),
            "the queue went on as if nothing had happened"
        );
        assert_eq!(q.available_desc(), 14);
    }

    // ---- the layout ------------------------------------------------------

    #[test]
    fn the_layout_keeps_the_rings_apart() {
        for size in [2u16, 4, 8, 16, 32] {
            let layout = VirtQueueLayout::new(size);
            let desc_bytes = 16 * size as usize;
            let avail_bytes = 6 + 2 * size as usize;
            assert_eq!(layout.avail_offset, desc_bytes);
            assert!(
                layout.used_offset >= desc_bytes + avail_bytes,
                "size {}: the used ring starts inside the available ring",
                size
            );
            let used_bytes = 6 + 8 * size as usize;
            assert!(
                layout.size >= layout.used_offset + used_bytes,
                "size {}: the used ring runs off the end of the block",
                size
            );
        }
    }

    #[test]
    fn the_layout_is_whole_pages() {
        for size in [2u16, 4, 8, 16, 32] {
            let layout = VirtQueueLayout::new(size);
            assert_eq!(
                layout.size % PAGE_SIZE,
                0,
                "size {} is not whole pages",
                size
            );
            assert_eq!(layout.used_offset % PAGE_SIZE, 0);
        }
    }
}
