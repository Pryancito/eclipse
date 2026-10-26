#![cfg(feature = "std")]
//! Damaged images never panic the driver. The kernel mounts EclipseFS
//! under a spinlock with interrupts off, so a panic there (a slice out of
//! bounds, an overflow, an `unwrap` of something on disk) stops the whole
//! machine; a damaged disk has to come back as an error.
//!
//! Every frozen image of format 1.0 (`tests/format/`, all the on-disk
//! structures there are) is damaged again and again, each time in a
//! different way:
//! - random bits flipped in random blocks of the device;
//! - bits flipped inside a tree node, with its CRC32C made right again, so
//!   the damage gets past the checksum and reaches the decoders of every
//!   item, which is what a driver bug or a malicious image would do;
//! - a whole block zeroed or replaced by another one.
//!
//! Then it is mounted, walked (lookup, stat, read, readlink, xattrs),
//! written to, checked, committed and mounted again. Anything may fail;
//! nothing may panic.

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use common::frozen::{self, IMAGES, V3_IMAGE};
use common::*;
use eclipsefs::crc::crc32c;
use eclipsefs::device::BlockDevice;
use eclipsefs::{EclipseFS, FileKind};

/// Damaged copies of each image.
const RUNS_PER_IMAGE: u64 = 200;

fn walk(fs: &mut EclipseFS, dir: u64, depth: u32, budget: &mut u32) {
    if depth > 12 || *budget == 0 {
        return;
    }
    let Ok(ents) = fs.readdir(dir) else { return };
    for e in ents {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        if e.name == "." || e.name == ".." {
            continue;
        }
        let Ok(ino) = fs.lookup(dir, &e.name) else {
            continue;
        };
        let _ = fs.list_xattrs(ino).map(|l| {
            for n in l {
                let _ = fs.get_xattr(ino, &n);
            }
        });
        let _ = fs.get_flags(ino);
        let _ = fs.fiemap(ino, 0, u64::MAX);
        let Ok(st) = fs.stat(ino) else { continue };
        match st.kind {
            FileKind::Dir => walk(fs, ino, depth + 1, budget),
            FileKind::Symlink => {
                let _ = fs.read_link(ino);
            }
            FileKind::Regular => {
                let mut b = vec![0u8; (st.size as usize).min(1 << 20)];
                let _ = fs.read(ino, 0, &mut b);
                let _ = fs.read_shared(ino, 0, &mut b);
                let _ = fs.write(ino, st.size / 2, b"dano");
            }
            _ => {}
        }
    }
    let _ = fs.create(dir, "nuevo-tras-el-dano", FileKind::Regular, 0o644, 0);
    let _ = fs.unlink(dir, "nuevo-tras-el-dano");
}

/// Everything a user of a damaged filesystem might do.
fn exercise(disks: &[Arc<MemDisk>]) {
    for read_only in [true, false] {
        let Ok(mut fs) = frozen::mount(disks, read_only) else {
            continue;
        };
        let _ = fs.list_subvolumes();
        let root = fs.root_ino();
        let mut budget = 3000;
        walk(&mut fs, root, 0, &mut budget);
        let _ = fs.check();
        let _ = fs.sync();
    }
    // What the writable mount left, mounted once more.
    if let Ok(mut fs) = frozen::mount(disks, false) {
        let root = fs.root_ino();
        let _ = fs.readdir(root);
        let _ = fs.check();
    }
}

/// Blocks of the image that hold something (not the mkfs filler).
fn used_blocks(d: &MemDisk) -> (Vec<u64>, Vec<u64>) {
    let bytes = d.bytes();
    let mut used = Vec::new();
    let mut nodes = Vec::new();
    for (i, b) in bytes.chunks(BLOCK).enumerate() {
        if b.iter().all(|&x| x == 0xA5) || b.iter().all(|&x| x == 0) {
            continue;
        }
        used.push(i as u64);
        if &b[0..4] == b"EFSN" {
            nodes.push(i as u64);
        }
    }
    (used, nodes)
}

fn damage(d: &MemDisk, (used, nodes): &(Vec<u64>, Vec<u64>), rng: &mut Lcg, how: u64) {
    let pick = |rng: &mut Lcg, v: &[u64]| v[rng.below(v.len() as u64) as usize];
    let mut blk = vec![0u8; BLOCK];
    match how % 4 {
        // Random bits of a block in use.
        0 => {
            let b = pick(rng, used);
            for _ in 0..1 + rng.below(8) {
                let at = b as usize * BLOCK + rng.below(BLOCK as u64) as usize;
                d.poke(at, 1 << rng.below(8));
            }
        }
        // Inside a tree node, resealed: past the checksum, into the items.
        1 if !nodes.is_empty() => {
            let b = pick(rng, nodes);
            d.read_at(b * BLOCK as u64, &mut blk).unwrap();
            for _ in 0..1 + rng.below(6) {
                let at = 8 + rng.below(BLOCK as u64 - 8) as usize;
                blk[at] ^= 1 << rng.below(8);
                // Now and then a whole field: counts, offsets, lengths.
                if rng.below(4) == 0 {
                    let w = (at & !7).min(BLOCK - 8);
                    blk[w..w + 8].copy_from_slice(&rng.next().to_le_bytes());
                }
            }
            let c = crc32c(&blk[8..]);
            blk[4..8].copy_from_slice(&c.to_le_bytes());
            d.write_at(b * BLOCK as u64, &blk).unwrap();
        }
        // A block zeroed.
        2 => {
            let b = pick(rng, used);
            d.write_at(b * BLOCK as u64, &[0u8; BLOCK]).unwrap();
        }
        // A block replaced by another one (a misdirected write).
        _ => {
            let (a, b) = (pick(rng, used), pick(rng, used));
            d.read_at(a * BLOCK as u64, &mut blk).unwrap();
            d.write_at(b * BLOCK as u64, &blk).unwrap();
        }
    }
}

#[test]
fn damaged_images_never_panic() {
    let mut images: Vec<(&str, usize)> = IMAGES.to_vec();
    images.push((V3_IMAGE, 1));
    let mut panics = Vec::new();
    for (k, (name, devices)) in images.into_iter().enumerate() {
        let pristine = frozen::load(name, devices);
        let maps: Vec<_> = pristine.iter().map(|d| used_blocks(d)).collect();
        for run in 0..RUNS_PER_IMAGE {
            let seed = (k as u64) << 32 | run;
            let mut rng = Lcg(seed);
            let disks: Vec<Arc<MemDisk>> = pristine.iter().map(|d| d.copy()).collect();
            let victim = rng.below(devices as u64) as usize;
            for _ in 0..1 + rng.below(2) {
                let how = rng.next();
                damage(&disks[victim], &maps[victim], &mut rng, how);
            }
            if let Err(e) = catch_unwind(AssertUnwindSafe(|| exercise(&disks))) {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                panics.push(format!("{name} seed {seed:#x}: {msg}"));
            }
        }
    }
    assert!(
        panics.is_empty(),
        "{} panics:\n{}",
        panics.len(),
        panics.join("\n")
    );
}
