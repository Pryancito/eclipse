#![cfg(feature = "std")]
//! Does the write path ever hand back a pointer the allocator never gave it?
//!
//! The kernel panic photographed on 2026-10-21 (`d0c30cd957-dirty`) showed the
//! heap's wild-block guard refusing block after block of exactly 0x4000 bytes
//! with `align=1`, every one of them from the same chain:
//!
//! ```text
//! [heap-wild] dealloc ptr=0xffff80013dd40000 size=0x4000 align=1
//!             -- outside every region the kernel heap owns
//!   CowFs::write_plain+0x724 <- CowFs::write_data <- CowFs::write_1
//!   <- EclipseMountINode::write_at <- ... <- sys_pwrite
//! ```
//!
//! Disassembling that return address pins the free: it is the `Vec<u8>` that
//! `fill_blocks` returns (`vec![0u8; got * BS]`, 4 blocks = 0x4000, `align=1`),
//! dropped at the end of each loop pass in `write_plain`. Each refusal leaks
//! 16 KiB, which is enough to take the kernel heap to its 2 GiB ceiling in a
//! desktop session.
//!
//! Two readings fit that evidence, and they want opposite fixes: either this
//! crate really does free a pointer it was never given (an alloc/free mismatch
//! here), or the pointer is sound and something outside this crate is wrong
//! about it. This file settles the first one, on the host, with an allocator
//! that remembers every live block and refuses to forget one it never made.
//!
//! It is deliberately a whole test binary of its own: a `#[global_allocator]`
//! is per binary, and this one has to see every allocation the write path
//! makes, including the ones inside the crate's dependencies.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use eclipsefs::FileKind;

/// Live blocks, open-addressed in a fixed table so the allocator itself never
/// allocates (a `HashSet` here would recurse into the allocation it is
/// recording). A pointer is its own key; 0 marks a free slot.
const SLOTS: usize = 1 << 20;
static LIVE: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];

/// Frees of a pointer that was not in [`LIVE`]: the thing the kernel's guard
/// reported. Kept with the first offending pointer and size for the message.
static WILD: AtomicUsize = AtomicUsize::new(0);
static WILD_PTR: AtomicUsize = AtomicUsize::new(0);
static WILD_SIZE: AtomicUsize = AtomicUsize::new(0);
/// Blocks that found no slot: the table is finite, and a full one must be
/// reported rather than turning every later free into a false accusation.
static NO_SLOT: AtomicUsize = AtomicUsize::new(0);
/// Set once the test proper starts, so the harness's own start-up traffic
/// (which allocated before anything could watch it) is not judged.
static WATCHING: AtomicUsize = AtomicUsize::new(0);

fn slot_of(ptr: usize) -> usize {
    // The low bits of a malloc'd pointer are alignment, not entropy.
    (ptr.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 24) & (SLOTS - 1)
}

/// Linear probe from `slot_of`, at most this far. A table under half full
/// finds a free slot or its own key long before this.
const PROBES: usize = 64;

fn remember(ptr: usize) {
    let h = slot_of(ptr);
    for i in 0..PROBES {
        let s = (h + i) & (SLOTS - 1);
        if LIVE[s]
            .compare_exchange(0, ptr, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
    NO_SLOT.fetch_add(1, Ordering::Relaxed);
}

/// Whether `ptr` was live, clearing it. False means nothing on record.
fn forget(ptr: usize) -> bool {
    let h = slot_of(ptr);
    for i in 0..PROBES {
        let s = (h + i) & (SLOTS - 1);
        if LIVE[s]
            .compare_exchange(ptr, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
    false
}

struct Watcher;

unsafe impl GlobalAlloc for Watcher {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            remember(p as usize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if !forget(ptr as usize) && WATCHING.load(Ordering::Relaxed) != 0 {
            // A table that ran out of slots cannot tell "never allocated"
            // from "we dropped the record", so it stops accusing.
            if NO_SLOT.load(Ordering::Relaxed) == 0 {
                WILD.fetch_add(1, Ordering::Relaxed);
                let _ =
                    WILD_PTR.compare_exchange(0, ptr as usize, Ordering::AcqRel, Ordering::Relaxed);
                let _ = WILD_SIZE.compare_exchange(
                    0,
                    layout.size(),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
        }
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static A: Watcher = Watcher;

fn verdict(what: &str) {
    let wild = WILD.load(Ordering::SeqCst);
    assert_eq!(
        NO_SLOT.load(Ordering::SeqCst),
        0,
        "{what}: the live-block table filled up, so the run proves nothing",
    );
    assert_eq!(
        wild,
        0,
        "{what}: {} free(s) of a pointer this allocator never handed out; \
         first was ptr={:#x} size={:#x}",
        wild,
        WILD_PTR.load(Ordering::SeqCst),
        WILD_SIZE.load(Ordering::SeqCst),
    );
}

/// The shape of the crash: `pwrite` after `pwrite` on one file, at offsets
/// that are not block-aligned (so `fill_blocks` reads the head and tail
/// blocks back) and in sizes around the 4-block buffer the report named.
#[test]
fn the_plain_write_path_frees_only_what_it_allocated() {
    let (_disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let f = fs
        .create(root, "wild", FileKind::Regular, 0o644, 0)
        .unwrap();
    WATCHING.store(1, Ordering::SeqCst);
    let data = pattern(16 * 1024, 3);
    // Aligned, unaligned at the head, unaligned at both ends, and a rewrite
    // over blocks the file already owns (the in-place branch).
    for off in [0u64, 1, 4095, 4096, 8191, 12_288, 100, 65_536 + 7] {
        write_all(&mut fs, f, off, &data);
    }
    for off in [0u64, 7, 4096, 9000] {
        write_all(&mut fs, f, off, &data[..8192]);
    }
    fs.sync().unwrap();
    verdict("plain writes");
}

/// The same file read back and rewritten across a remount, which is where the
/// extent map comes from disk rather than from the cache the writes built.
#[test]
fn a_remounted_file_rewrites_without_a_wild_free() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let f = fs
        .create(root, "wild2", FileKind::Regular, 0o644, 0)
        .unwrap();
    let data = pattern(32 * 1024, 11);
    write_all(&mut fs, f, 0, &data);
    let mut fs = remount(&disk, fs);
    WATCHING.store(1, Ordering::SeqCst);
    write_all(&mut fs, f, 1234, &data);
    write_all(&mut fs, f, 16 * 1024, &data[..16 * 1024]);
    assert_eq!(
        read_all(&mut fs, f).len(),
        (1234 + data.len()).max(32 * 1024)
    );
    fs.sync().unwrap();
    verdict("rewrite after remount");
}
