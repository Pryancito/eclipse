#![cfg(feature = "std")]
//! Data checksums (format v0.4): every file block has a CRC32C in the
//! checksum tree, reads verify it, and `scrub` checks the whole device.

mod common;

use std::sync::Arc;

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{EclipseFS, Error, FileKind};

/// Disk block holding exactly `content`.
fn find_block(disk: &MemDisk, content: &[u8]) -> usize {
    let mut image = vec![0u8; disk.size() as usize];
    disk.read_at(0, &mut image).unwrap();
    image
        .chunks(BLOCK)
        .position(|b| b == content)
        .expect("block on disk")
}

/// Tree nodes (`EFSN`) owned by tree `owner`.
fn nodes_of(disk: &MemDisk, owner: u64) -> Vec<usize> {
    let mut image = vec![0u8; disk.size() as usize];
    disk.read_at(0, &mut image).unwrap();
    image
        .chunks(BLOCK)
        .enumerate()
        .filter(|(_, b)| {
            &b[..4] == b"EFSN" && u64::from_le_bytes(b[24..32].try_into().unwrap()) == owner
        })
        .map(|(i, _)| i)
        .collect()
}

/// (inode, contents) of each file.
type Files = Vec<(u64, Vec<u8>)>;

fn with_files() -> (Arc<MemDisk>, EclipseFS, Files) {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for i in 0..6 {
        let f = fs
            .create(root, &format!("f{}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
        // Random bytes, so no two blocks on the disk are alike.
        let mut rng = Lcg(1000 + i as u64);
        let data: Vec<u8> = (0..50_000 + i * 70_000).map(|_| rng.next() as u8).collect();
        write_all(&mut fs, f, 0, &data);
        files.push((f, data));
    }
    let fs = remount(&disk, fs);
    (disk, fs, files)
}

#[test]
fn a_clean_filesystem_scrubs_clean() {
    let (_disk, mut fs, files) = with_files();
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
    let blocks: u64 = files
        .iter()
        .map(|(_, d)| (d.len() as u64).div_ceil(4096))
        .sum();
    assert_eq!(r.data_blocks, blocks);
    assert!(r.tree_nodes >= 4);
    fs.check().unwrap();
}

#[test]
fn bit_rot_in_file_data_is_caught_on_read_and_by_scrub() {
    let (disk, fs, files) = with_files();
    drop(fs);
    let (victim, data) = &files[3];
    // Third block of the victim.
    let b = find_block(&disk, &data[2 * BLOCK..3 * BLOCK]);
    disk.poke(b * BLOCK + 1234, 0x10);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    // Reads that touch the block fail, the rest of the file still reads.
    let mut buf = vec![0u8; 10];
    assert_eq!(
        fs.read(*victim, 2 * BLOCK as u64 + 5, &mut buf),
        Err(Error::Corrupt("data checksum"))
    );
    let mut head = vec![0u8; 2 * BLOCK];
    assert_eq!(fs.read(*victim, 0, &mut head).unwrap(), 2 * BLOCK);
    assert_eq!(head, data[..2 * BLOCK]);
    let mut whole = vec![0u8; data.len()];
    assert!(fs.read(*victim, 0, &mut whole).is_err());
    // Every other file is intact.
    for (f, d) in files.iter().filter(|(f, _)| f != victim) {
        assert_eq!(&read_all(&mut fs, *f), d);
    }
    let r = fs.scrub().unwrap();
    assert_eq!(r.bad_data_blocks, vec![b as u64]);
    assert!(r.bad_tree_nodes.is_empty());
    // Partial writes into the block need its old contents and fail too;
    // rewriting it whole replaces it with a fresh, checksummed copy.
    assert!(fs.write(*victim, 2 * BLOCK as u64 + 7, b"x").is_err());
    write_all(
        &mut fs,
        *victim,
        2 * BLOCK as u64,
        &data[2 * BLOCK..3 * BLOCK],
    );
    assert_eq!(&read_all(&mut fs, *victim), data);
    let mut fs = remount(&disk, fs);
    assert!(fs.scrub().unwrap().is_clean());
    fs.check().unwrap();
}

#[test]
fn damaged_tree_nodes_are_reported_by_scrub() {
    let (disk, fs, _files) = with_files();
    drop(fs);
    // Damage every checksum tree node on the disk, stale copies included.
    let csum_nodes = nodes_of(&disk, 7);
    assert!(!csum_nodes.is_empty());
    for &n in &csum_nodes {
        disk.poke(n * BLOCK + 2000, 1);
    }
    let mut fs = EclipseFS::mount(disk.clone(), true).unwrap();
    let r = fs.scrub().unwrap();
    assert!(!r.bad_tree_nodes.is_empty());
    assert!(r
        .bad_tree_nodes
        .iter()
        .all(|b| csum_nodes.contains(&(*b as usize))));
    // Without their checksums the data blocks under that leaf cannot be
    // trusted either.
    assert!(!r.bad_data_blocks.is_empty());
    assert!(fs.check().is_err());
}

#[test]
fn checksums_follow_overwrites_truncates_and_unlinks() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let mut model = pattern(200_000, 1);
    write_all(&mut fs, f, 0, &model);
    let mut rng = Lcg(5);
    for step in 0..120 {
        match rng.below(4) {
            0 | 1 => {
                let at = rng.below(model.len() as u64 + 3000) as usize;
                let chunk = pattern(1 + rng.below(20_000) as usize, step);
                write_all(&mut fs, f, at as u64, &chunk);
                if model.len() < at + chunk.len() {
                    model.resize(at + chunk.len(), 0);
                }
                model[at..at + chunk.len()].copy_from_slice(&chunk);
            }
            2 => {
                let to = rng.below(model.len() as u64 + 9000) as usize;
                fs.truncate(f, to as u64).unwrap();
                model.resize(to, 0);
            }
            _ => {
                let g = fs.create(root, "tmp", FileKind::Regular, 0o644, 0).unwrap();
                write_all(&mut fs, g, 0, &pattern(30_000, step));
                fs.unlink(root, "tmp").unwrap();
            }
        }
        if step % 20 == 19 {
            fs = remount(&disk, fs);
            fs.check().unwrap();
            assert!(fs.scrub().unwrap().is_clean());
        }
        assert_eq!(read_all(&mut fs, f), model, "step {}", step);
    }
}

#[test]
fn scrub_needs_the_cow_format() {
    let (_disk, mut fs) = fresh_v3(8 * MIB);
    assert_eq!(
        fs.scrub().unwrap_err(),
        Error::Unsupported("scrub needs format v0.4")
    );
}
