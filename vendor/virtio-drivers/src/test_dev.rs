//! A virtio device made of ordinary memory, so the drivers can be driven from
//! a hosted test process.
//!
//! Nothing in this tree could test these drivers before. They reach the
//! outside world through four `extern "C"` symbols (`virtio_dma_alloc` and
//! friends) that only the kernel defines, and through a `&'static mut
//! VirtIOHeader` that only exists at a device's MMIO window. Both are
//! ordinary memory as far as Rust is concerned: `VirtIOHeader` is a
//! `#[repr(C)]` struct of `volatile` cells, so a zeroed region with the right
//! magic in it *is* a virtio device, and the four symbols can be answered out
//! of a bump arena with the physical and virtual address spaces laid on top of
//! each other.
//!
//! The model here is deliberately written from the specification (2.6 Split
//! Virtqueues) rather than from [`crate::queue`], so that a disagreement
//! between the driver's idea of the ring layout and the device's shows up as a
//! failing test instead of cancelling out.

use crate::header::VirtIOHeader;
use crate::PAGE_SIZE;
use core::convert::TryInto;
use std::sync::{Mutex, OnceLock};

/// A freshly handed-out DMA block is filled with this, not with zeroes.
///
/// `virtio_dma_alloc` is the kernel frame allocator
/// (`KHANDLER.frame_alloc_contiguous`), which recycles frames and does not
/// clear them -- `kernel-hal`'s DMA quarantine exists precisely because a
/// freed block comes back around. A driver that assumes its rings arrive
/// zeroed works on the first device of a fresh boot and then stops working,
/// which is the kind of thing that only ever happens on a real machine. The
/// arena poisons every block so that assumption fails here first.
const POISON: u8 = 0xab;

/// Where the arena pretends to be in physical memory.
///
/// It cannot simply be the address the host heap handed out: a legacy
/// `QueuePFN` register is 32 bits of page number, so a queue has to live below
/// 2^44, and a hosted process's heap sits far above that. The kernel's own
/// `virtio_phys_to_virt` is an offset (`paddr + phys_to_virt_offset`), so the
/// arena models exactly that -- a fixed offset between the two address spaces
/// -- rather than laying them on top of each other.
/// Above 4 GiB on purpose, and below 2^44 on purpose.
///
/// Above, because `DMA::paddr` used to be a `u32` and a block below 4 GiB
/// truncates to itself -- the bug is invisible on a small machine, which is
/// why it survived. Below, because a legacy `QueuePFN` cannot name a page
/// above 2^44 and a queue really does have to live where the register can
/// point at it.
const FAKE_PHYS_BASE: usize = 0x1_4000_0000;

struct Arena {
    /// Virtual address of the arena in this process.
    base: usize,
    len: usize,
    cursor: usize,
}

fn arena() -> &'static Mutex<Arena> {
    static ARENA: OnceLock<Mutex<Arena>> = OnceLock::new();
    ARENA.get_or_init(|| {
        // 32 MiB is enough for every queue and framebuffer the suite asks for;
        // the allocator never reclaims, so one test's freed block is never
        // handed to the next, and an address stays meaningful for the run.
        let pages = 8192;
        let bytes = pages * PAGE_SIZE + PAGE_SIZE;
        let region = vec![0u8; bytes].leak();
        let base = (region.as_ptr() as usize + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        Mutex::new(Arena {
            base,
            len: pages * PAGE_SIZE,
            cursor: 0,
        })
    })
}

/// Hand out `bytes` of arena, poisoned, and return its *physical* address.
fn bump(bytes: usize) -> usize {
    let bytes = (bytes + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut arena = arena().lock().unwrap();
    let offset = arena.cursor;
    assert!(
        offset + bytes <= arena.len,
        "the test arena is out of room: asked for {} bytes with {} left",
        bytes,
        arena.len - offset
    );
    arena.cursor += bytes;
    unsafe { core::ptr::write_bytes((arena.base + offset) as *mut u8, POISON, bytes) };
    FAKE_PHYS_BASE + offset
}

/// The two translations, for addresses the arena owns.
///
/// Anything else -- a caller's stack or heap buffer, which is most of what
/// goes into a descriptor -- passes through unchanged. A descriptor address is
/// a full 64-bit field, so a host pointer fits there; only the queue itself has
/// to be low, and the queue is always arena memory.
fn to_virt(paddr: usize) -> usize {
    let arena = arena().lock().unwrap();
    if (FAKE_PHYS_BASE..FAKE_PHYS_BASE + arena.len).contains(&paddr) {
        paddr - FAKE_PHYS_BASE + arena.base
    } else {
        paddr
    }
}

fn to_phys(vaddr: usize) -> usize {
    let arena = arena().lock().unwrap();
    if (arena.base..arena.base + arena.len).contains(&vaddr) {
        vaddr - arena.base + FAKE_PHYS_BASE
    } else {
        vaddr
    }
}

#[no_mangle]
extern "C" fn virtio_dma_alloc(pages: usize) -> usize {
    if pages == 0 {
        return 0;
    }
    bump(pages * PAGE_SIZE)
}

#[no_mangle]
extern "C" fn virtio_dma_dealloc(_paddr: usize, _pages: usize) -> i32 {
    0
}

#[no_mangle]
extern "C" fn virtio_phys_to_virt(paddr: usize) -> usize {
    to_virt(paddr)
}

#[no_mangle]
extern "C" fn virtio_virt_to_phys(vaddr: usize) -> usize {
    to_phys(vaddr)
}

/// The device side of one split virtqueue, addressed the way the device sees
/// it: offsets computed from the specification, not from the driver's layout.
pub(crate) struct Ring {
    base: usize,
    size: u16,
    avail: usize,
    used: usize,
}

impl Ring {
    /// Where the driver said the queue is (`QueuePFN` of `queue`), with the
    /// size it said it would use.
    pub(crate) fn of(header: &mut VirtIOHeader, queue: u32, size: u16) -> Self {
        let pfn = header.queue_physical_page_number(queue);
        assert_ne!(pfn, 0, "the driver never published queue {}", queue);
        Self::at((pfn as usize) << 12, size)
    }

    /// The same, from the physical address of the descriptor table.
    pub(crate) fn at(paddr: usize, size: u16) -> Self {
        let base = to_virt(paddr);
        let desc = 16 * size as usize;
        // avail: flags, idx, ring[size], used_event
        let avail_bytes = 2 + 2 + 2 * size as usize + 2;
        let used = (base + desc + avail_bytes + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        Ring {
            base,
            size,
            avail: base + desc,
            used,
        }
    }

    fn rd<T: Copy>(&self, addr: usize) -> T {
        unsafe { (addr as *const T).read_volatile() }
    }

    fn wr<T: Copy>(&self, addr: usize, value: T) {
        unsafe { (addr as *mut T).write_volatile(value) }
    }

    /// What the driver has published, as a count that only ever rises.
    pub(crate) fn avail_idx(&self) -> u16 {
        self.rd(self.avail + 2)
    }

    /// What the device has handed back, as a count that only ever rises.
    pub(crate) fn used_idx(&self) -> u16 {
        self.rd(self.used + 2)
    }

    /// The driver's flags word on the available ring.
    pub(crate) fn avail_flags(&self) -> u16 {
        self.rd(self.avail)
    }

    /// The device's flags word on the used ring.
    pub(crate) fn used_flags(&self) -> u16 {
        self.rd(self.used)
    }

    /// The descriptor-chain head the driver put in a slot of the avail ring.
    pub(crate) fn avail_entry(&self, slot: u16) -> u16 {
        self.rd(self.avail + 4 + 2 * (slot & (self.size - 1)) as usize)
    }

    /// One descriptor: `(address, length, flags, next)`.
    pub(crate) fn desc(&self, index: u16) -> (u64, u32, u16, u16) {
        let at = self.base + 16 * index as usize;
        (
            self.rd(at),
            self.rd(at + 8),
            self.rd(at + 12),
            self.rd(at + 14),
        )
    }

    /// Walk a chain from `head`, yielding `(address, length, writable)`.
    ///
    /// Bounded by the queue size: a chain that leads nowhere is the device's
    /// problem too, and a model that hangs teaches nothing.
    pub(crate) fn chain(&self, head: u16) -> Vec<(usize, usize, bool)> {
        let mut out = Vec::new();
        let mut index = head;
        for _ in 0..self.size {
            let (addr, len, flags, next) = self.desc(index);
            out.push((to_virt(addr as usize), len as usize, flags & 2 != 0));
            if flags & 1 == 0 {
                return out;
            }
            index = next;
        }
        panic!("the descriptor chain from {} does not end", head);
    }

    /// Hand a chain back to the driver, the way a device does.
    pub(crate) fn complete(&self, head: u16, written: u32) {
        self.complete_raw(head as u32, written);
    }

    /// Hand back an arbitrary id, for the tests about a device that lies.
    pub(crate) fn complete_raw(&self, id: u32, written: u32) {
        let idx: u16 = self.rd(self.used + 2);
        let slot = (idx & (self.size - 1)) as usize;
        let at = self.used + 4 + 8 * slot;
        self.wr(at, id);
        self.wr(at + 4, written);
        self.wr(self.used + 2, idx.wrapping_add(1));
    }

    /// Put an arbitrary value in the device-owned `used.idx`, to model a ring
    /// whose memory the driver never cleared.
    pub(crate) fn poison_used_idx(&self, value: u16) {
        self.wr(self.used + 2, value);
    }

    /// The bytes a chain's writable descriptors point at, concatenated.
    pub(crate) fn written(&self, head: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for (addr, len, writable) in self.chain(head) {
            if writable {
                out.extend_from_slice(unsafe {
                    core::slice::from_raw_parts(addr as *const u8, len)
                });
            }
        }
        out
    }

    /// Fill a chain's writable descriptors from `bytes`, and return how many
    /// bytes went in.
    pub(crate) fn fill(&self, head: u16, bytes: &[u8]) -> u32 {
        let mut done = 0usize;
        for (addr, len, writable) in self.chain(head) {
            if !writable {
                continue;
            }
            let take = len.min(bytes.len() - done);
            unsafe {
                core::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), addr as *mut u8, take)
            };
            done += take;
            if done == bytes.len() {
                break;
            }
        }
        done as u32
    }
}

/// A header made of ordinary memory, with a config space behind it.
///
/// Leaked on purpose: the drivers take `&'static mut VirtIOHeader` because a
/// device's window outlives everything, and a test that owns one would have to
/// prove otherwise to the borrow checker for no gain.
pub(crate) fn fake_header(device_id: u32, max_queue_size: u32) -> &'static mut VirtIOHeader {
    let at = to_virt(bump(0x200));
    // Zeroed, unlike a DMA block: a device's registers read as whatever the
    // device decides, and `begin_init` is what sets the ones that matter.
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, 0x200) };
    let header = unsafe { &mut *(at as *mut VirtIOHeader) };
    header.fake_init(device_id, max_queue_size);
    header
}

/// The first `u64` of the config space, which for virtio-blk is the capacity
/// in 512-byte sectors.
pub(crate) fn set_config_u64(header: &VirtIOHeader, value: u64) {
    unsafe { (header.config_space()).write_volatile(value) };
}

// A `Ring` is three addresses and a length. The device side of a queue runs in
// its own thread in the round-trip tests, which is the only way to drive a
// driver that spins on `can_pop`.
unsafe impl Send for Ring {}

/// A virtio-blk device: a disk in memory, serving one queue until it is told
/// to stop.
pub(crate) struct Disk {
    pub(crate) sectors: Mutex<Vec<u8>>,
    /// Set to make every request answer `VIRTIO_BLK_S_IOERR`.
    pub(crate) fail: std::sync::atomic::AtomicBool,
    pub(crate) served: std::sync::atomic::AtomicUsize,
    stop: std::sync::atomic::AtomicBool,
}

impl Disk {
    pub(crate) fn of(sectors: usize) -> std::sync::Arc<Self> {
        // Each sector holds its own number in its first four bytes, so a read
        // that lands on the wrong sector is a wrong number rather than a
        // buffer of zeroes that looks like every other buffer of zeroes.
        let mut bytes = vec![0u8; sectors * 512];
        for sector in 0..sectors {
            bytes[sector * 512..sector * 512 + 4].copy_from_slice(&(sector as u32).to_le_bytes());
        }
        std::sync::Arc::new(Disk {
            sectors: Mutex::new(bytes),
            fail: std::sync::atomic::AtomicBool::new(false),
            served: std::sync::atomic::AtomicUsize::new(0),
            stop: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) fn sector(&self, index: usize) -> Vec<u8> {
        self.sectors.lock().unwrap()[index * 512..(index + 1) * 512].to_vec()
    }

    pub(crate) fn served(&self) -> usize {
        self.served.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Serve `ring` on this thread until [`stop`](Self::stop) is called.
    pub(crate) fn serve(self: &std::sync::Arc<Self>, ring: Ring) {
        let mut seen: u16 = 0;
        while !self.stop.load(std::sync::atomic::Ordering::SeqCst) {
            while seen != ring.avail_idx() {
                let head = ring.avail_entry(seen);
                self.serve_one(&ring, head);
                seen = seen.wrapping_add(1);
            }
            std::thread::yield_now();
        }
    }

    fn serve_one(&self, ring: &Ring, head: u16) {
        let chain = ring.chain(head);
        // The request header is always the first descriptor: type, reserved,
        // sector.
        let (req_at, req_len, _) = chain[0];
        assert!(req_len >= 16, "a request header of {} bytes", req_len);
        let request = unsafe { core::slice::from_raw_parts(req_at as *const u8, 16) };
        let kind = u32::from_le_bytes(request[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(request[8..16].try_into().unwrap()) as usize;
        // The status byte is the last writable descriptor; the data, if any,
        // is what lies between.
        let status_at = chain.iter().rev().find(|d| d.2).expect("no status byte").0;
        let failing = self.fail.load(std::sync::atomic::Ordering::SeqCst);
        let mut written = 0u32;
        if !failing {
            let mut disk = self.sectors.lock().unwrap();
            match kind {
                0 => {
                    // read: every writable descriptor but the last
                    let mut at = sector * 512;
                    for (addr, len, writable) in chain.iter().skip(1) {
                        if !writable || *addr == status_at {
                            continue;
                        }
                        assert!(at + len <= disk.len(), "a read past the end of the disk");
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                disk[at..].as_ptr(),
                                *addr as *mut u8,
                                *len,
                            )
                        };
                        at += len;
                        written += *len as u32;
                    }
                }
                1 => {
                    // write: every read-only descriptor after the header
                    let mut at = sector * 512;
                    for (addr, len, writable) in chain.iter().skip(1) {
                        if *writable {
                            continue;
                        }
                        assert!(at + len <= disk.len(), "a write past the end of the disk");
                        let from = unsafe { core::slice::from_raw_parts(*addr as *const u8, *len) };
                        disk[at..at + len].copy_from_slice(from);
                        at += len;
                    }
                }
                other => panic!("the driver asked for request type {}", other),
            }
        }
        // VIRTIO_BLK_S_OK is 0, VIRTIO_BLK_S_IOERR is 1.
        unsafe { (status_at as *mut u8).write_volatile(u8::from(failing)) };
        self.served
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ring.complete(head, written + 1);
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
