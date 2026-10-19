#![cfg(feature = "std")]
//! Timed commits (`commit=` seconds): operations stop committing every
//! `COMMIT_OPS`, a timer (here a hand-moved clock) commits, and `fsync`
//! uses the log in between.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use common::*;
use eclipsefs::fs::COMMIT_OPS;
use eclipsefs::{mkfs, EclipseFS, FileKind};

/// Each test has its own clock, by index.
static CLOCKS: [AtomicU64; 4] = [
    AtomicU64::new(1_000),
    AtomicU64::new(1_000),
    AtomicU64::new(1_000),
    AtomicU64::new(1_000),
];

fn clock0() -> (u64, u32) {
    (CLOCKS[0].load(Ordering::SeqCst), 0)
}
fn clock1() -> (u64, u32) {
    (CLOCKS[1].load(Ordering::SeqCst), 0)
}
fn clock2() -> (u64, u32) {
    (CLOCKS[2].load(Ordering::SeqCst), 0)
}
fn clock3() -> (u64, u32) {
    (CLOCKS[3].load(Ordering::SeqCst), 0)
}

fn timed(size: usize, clock: fn() -> (u64, u32)) -> (std::sync::Arc<MemDisk>, EclipseFS) {
    let disk = MemDisk::new(size);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    fs.set_commit_interval(Some(5));
    (disk, fs)
}

#[test]
fn operations_do_not_commit_before_the_interval() {
    let (disk, mut fs) = timed(32 * MIB, clock0);
    let root = fs.root_ino();
    let gen = fs.generation();
    for i in 0..(COMMIT_OPS * 4) {
        let f = fs
            .create(root, &format!("f{}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
        fs.write(f, 0, b"data").unwrap();
    }
    assert_eq!(fs.generation(), gen, "committed before the interval");
    // Nothing due yet.
    assert!(!fs.commit_if_due().unwrap());
    CLOCKS[0].fetch_add(5, Ordering::SeqCst);
    assert!(fs.commit_if_due().unwrap());
    assert!(fs.generation() > gen);
    // Clean and not due: nothing.
    assert!(!fs.commit_if_due().unwrap());
    let copy = disk.copy();
    let mut after = EclipseFS::mount(copy, false).unwrap();
    assert!(after
        .lookup(root, &format!("f{}", COMMIT_OPS * 4 - 1))
        .is_ok());
    after.check().unwrap();
}

#[test]
fn the_next_operation_after_the_interval_commits() {
    let (_disk, mut fs) = timed(32 * MIB, clock1);
    let root = fs.root_ino();
    let gen = fs.generation();
    fs.create(root, "a", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(fs.generation(), gen);
    CLOCKS[1].fetch_add(6, Ordering::SeqCst);
    fs.create(root, "b", FileKind::Regular, 0o644, 0).unwrap();
    assert!(fs.generation() > gen);
}

#[test]
fn big_transactions_still_commit_early() {
    let (_disk, mut fs) = timed(512 * MIB, clock2);
    let root = fs.root_ino();
    let f = fs.create(root, "big", FileKind::Regular, 0o644, 0).unwrap();
    let gen = fs.generation();
    let chunk = vec![0x5Au8; MIB];
    // 300 MiB of data: more checksums than one transaction holds.
    for i in 0..300u64 {
        fs.write(f, i * MIB as u64, &chunk).unwrap();
    }
    assert!(fs.generation() > gen, "never committed");
    fs.check().unwrap();
}

#[test]
fn fsync_between_timed_commits_uses_the_log() {
    let (disk, mut fs) = timed(64 * MIB, clock3);
    let root = fs.root_ino();
    let f = fs.create(root, "db", FileKind::Regular, 0o644, 0).unwrap();
    fs.write(f, 0, &vec![1u8; 100_000]).unwrap();
    fs.fsync(f).unwrap();
    let gen = fs.generation();
    let mut want = Vec::new();
    for i in 0..500u64 {
        let block = vec![(i % 251) as u8; 4096];
        fs.write(f, (i * 7 % 24) * 4096, &block).unwrap();
        fs.fsync(f).unwrap();
        if i == 499 {
            want = vec![0u8; fs.stat(f).unwrap().size as usize];
            fs.read(f, 0, &mut want).unwrap();
        }
    }
    assert!(fs.fsync_log_records() >= 400);
    // Only the log filling up commits.
    assert!(fs.generation() - gen < 50);
    let mut after = EclipseFS::mount(disk.copy(), false).unwrap();
    let mut got = vec![0u8; want.len()];
    after.read(f, 0, &mut got).unwrap();
    assert_eq!(got, want);
    after.check().unwrap();
}

#[test]
fn the_default_still_commits_every_commit_ops_operations() {
    let disk = MemDisk::new(32 * MIB);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk, false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    let gen = fs.generation();
    for i in 0..COMMIT_OPS {
        fs.create(root, &format!("f{}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
    }
    assert!(fs.generation() > gen);
}
