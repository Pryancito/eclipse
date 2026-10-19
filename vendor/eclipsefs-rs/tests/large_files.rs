#![cfg(feature = "std")]
//! Files with many extents: every whole-file operation must work a batch of
//! extents at a time, never asking for one allocation that grows with the
//! file. A real machine panicked on `memory allocation of 53785440 bytes
//! failed` handling an 88 GB file.
//!
//! Each test file is its own binary, so the allocator below only watches
//! this one; it records the largest single allocation made on the current
//! thread while [`largest_alloc`] runs.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::*;
use eclipsefs::{EclipseFS, FileKind};

const B: u64 = BLOCK as u64;
/// Extents in each data run of the fragmented file: many times the batch
/// size of 1024.
const N: u64 = 12_000;
/// What no step may ask for at once: the 1 MiB of contiguous tree nodes
/// `write_home` writes in one go, and a little. Reading every extent item
/// of one of these files in one list (48 bytes each, in a vector grown by
/// doubling) is past it; a batch is far below it.
const MAX_ALLOC: usize = (1 << 20) + (64 << 10);
/// Blocks of "other", written before and after "frag" so that they share
/// checksum items with it.
const OTHER: u64 = 100;
const DISK: usize = 160 * MIB;

struct Watch;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

fn note(size: usize) {
    let _ = WATCHING.try_with(|w| {
        if w.get() {
            let _ = LARGEST.try_with(|l| l.set(l.get().max(size)));
        }
    });
}

unsafe impl GlobalAlloc for Watch {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note(l.size());
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note(l.size());
        System.alloc_zeroed(l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        note(new);
        System.realloc(p, l, new)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static GLOBAL: Watch = Watch;

/// The largest single allocation `f` makes on this thread.
fn largest_alloc<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.with(|l| l.set(0));
    WATCHING.with(|w| w.set(true));
    let r = f();
    WATCHING.with(|w| w.set(false));
    (r, LARGEST.with(|l| l.get()))
}

fn block(i: u64) -> Vec<u8> {
    pattern(BLOCK, i as usize)
}

/// File "frag" with `N` one-block extents, each next to the one before in
/// the file but not on disk (written last to first, so each lands before
/// the one after it), then a hole of `N` blocks and `N` more such blocks.
/// "other" has `OTHER` blocks written before and `OTHER` after. Returns
/// (frag, other).
fn fragmented(fs: &mut EclipseFS) -> (u64, u64) {
    let root = fs.root_ino();
    let f = fs
        .create(root, "frag", FileKind::Regular, 0o644, 0)
        .unwrap();
    let o = fs
        .create(root, "other", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(fs, o, 0, &other_data(0..OTHER));
    for i in (0..N).chain(2 * N..3 * N).rev() {
        write_all(fs, f, i * B, &block(i));
    }
    write_all(fs, o, OTHER * B, &other_data(OTHER..2 * OTHER));
    fs.sync().unwrap();
    (f, o)
}

fn other_data(blocks: std::ops::Range<u64>) -> Vec<u8> {
    blocks.flat_map(|i| block(i + 7)).collect()
}

fn expected(len: u64) -> Vec<u8> {
    let mut want = Vec::new();
    for i in 0..len {
        if i < N || (2 * N..3 * N).contains(&i) {
            want.extend(block(i));
        } else {
            want.extend(vec![0u8; BLOCK]);
        }
    }
    want
}

#[test]
fn a_fragmented_file_really_has_more_extents_than_a_batch() {
    let (_disk, mut fs) = fresh(DISK);
    let (f, _) = fragmented(&mut fs);
    let m = fs.fiemap(f, 0, N * B).unwrap();
    assert!(m.len() as u64 > N / 2, "only {} extents", m.len());
}

#[test]
fn seek_data_and_hole_walk_past_many_extents_in_bounded_memory() {
    let (_disk, mut fs) = fresh(DISK);
    let (f, _) = fragmented(&mut fs);
    let (r, peak) = largest_alloc(|| {
        (
            fs.seek_hole(f, 0).unwrap(),
            fs.seek_data(f, N * B).unwrap(),
            fs.seek_hole(f, 2 * N * B + 10).unwrap(),
            fs.seek_data(f, 5 * B).unwrap(),
        )
    });
    // The first data piece runs across every batch of its extents.
    assert_eq!(
        r,
        (Some(N * B), Some(2 * N * B), Some(3 * N * B), Some(5 * B))
    );
    assert!(peak < MAX_ALLOC, "largest allocation {peak} bytes");
}

#[test]
fn fiemap_of_many_extents_marks_only_the_last() {
    let (_disk, mut fs) = fresh(DISK);
    let (f, _) = fragmented(&mut fs);
    let m = fs.fiemap(f, 0, u64::MAX).unwrap();
    let total: u64 = m.iter().map(|m| m.length).sum();
    assert_eq!(total, 2 * N * B);
    assert!(m
        .windows(2)
        .all(|w| w[0].logical + w[0].length <= w[1].logical));
    let last = eclipsefs::fs::FIEMAP_EXTENT_LAST;
    assert!(m[..m.len() - 1].iter().all(|m| m.flags & last == 0));
    assert_ne!(m.last().unwrap().flags & last, 0);
    // A range ending before the data does: LAST is not set there.
    let m = fs.fiemap(f, 0, N * B).unwrap();
    assert_eq!(m.last().unwrap().flags & last, 0);
    let (_, peak) = largest_alloc(|| fs.fiemap(f, 0, 16 * B).unwrap());
    assert!(peak < MAX_ALLOC, "largest allocation {peak} bytes");
}

#[test]
fn reflinking_a_file_of_many_extents_copies_it_in_bounded_memory() {
    let (disk, mut fs) = fresh(DISK);
    let (f, _) = fragmented(&mut fs);
    let root = fs.root_ino();
    let g = fs
        .create(root, "copy", FileKind::Regular, 0o644, 0)
        .unwrap();
    let size = 3 * N * B;
    let (n, peak) = largest_alloc(|| fs.clone_range(f, 0, g, 0, 0).unwrap());
    assert_eq!(n, size);
    assert!(peak < MAX_ALLOC, "largest allocation {peak} bytes");
    assert_eq!(fs.compsize(g).unwrap().disk_bytes, 2 * N * B);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, g), expected(3 * N));
    fs.check().unwrap();
}

#[test]
fn truncating_and_deleting_many_extents_frees_them_in_bounded_memory() {
    let (disk, mut fs) = fresh(DISK);
    let (f, o) = fragmented(&mut fs);
    let before = fs.check().unwrap().blocks_used;
    // Down to a block inside the first run: every extent of both runs goes
    // in one call.
    let (_, peak) = largest_alloc(|| {
        fs.truncate(f, 17 * B).unwrap();
        fs.sync().unwrap();
    });
    assert!(peak < MAX_ALLOC, "largest allocation {peak} bytes");
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), expected(17));
    let after = fs.check().unwrap().blocks_used;
    assert!(before - after >= 2 * N - 17, "freed {}", before - after);

    // The other file kept its blocks and their checksums.
    assert_eq!(read_all(&mut fs, o), other_data(0..2 * OTHER));
    fs.scrub().unwrap();
    let root = fs.root_ino();
    let (_, peak) = largest_alloc(|| {
        fs.unlink(root, "frag").unwrap();
        fs.sync().unwrap();
    });
    assert!(peak < MAX_ALLOC, "largest allocation {peak} bytes");
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, o), other_data(0..2 * OTHER));
    fs.check().unwrap();
    fs.scrub().unwrap();
}

#[test]
fn blocks_freed_and_written_again_in_one_transaction_keep_their_new_checksums() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    // Fresh blocks freed in their own transaction go back at once, so the
    // rewrite below lands on them before anything commits.
    write_all(&mut fs, f, 0, &pattern(64 * BLOCK, 1));
    fs.truncate(f, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(64 * BLOCK, 2));
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), pattern(64 * BLOCK, 2));
    fs.check().unwrap();
    fs.scrub().unwrap();
}

#[test]
fn defrag_of_many_extents_rewrites_them_across_batches() {
    let (disk, mut fs) = fresh(2 * DISK);
    let (f, _) = fragmented(&mut fs);
    let before = fs.fiemap(f, 0, u64::MAX).unwrap().len();
    let moved = fs.defrag(f, 0, u64::MAX, 0, None).unwrap();
    assert_eq!(moved, 2 * N * B);
    let after = fs.fiemap(f, 0, u64::MAX).unwrap().len();
    assert!(after * 100 < before, "{before} -> {after} extents");
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), expected(3 * N));
    fs.check().unwrap();
}

#[test]
fn repairing_a_file_of_many_extents_stays_in_bounded_memory() {
    let (disk, mut fs) = fresh(DISK);
    let (f, o) = fragmented(&mut fs);
    let (r, peak) = largest_alloc(|| fs.repair().unwrap());
    assert!(!r.changed() && r.is_clean(), "{:?}", r);
    assert!(peak < MAX_ALLOC, "largest allocation {peak} bytes");
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), expected(3 * N));
    assert_eq!(read_all(&mut fs, o), other_data(0..2 * OTHER));
}
