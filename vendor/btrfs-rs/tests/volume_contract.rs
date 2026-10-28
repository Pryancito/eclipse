#![cfg(feature = "std")]
//! What a mount refuses, and what a format lays down.
//!
//! `volume.rs` and `mkfs.rs` had no tests between them, and a wide lot of 34
//! mutations says why that was not obvious: thirteen died by rebound, because
//! every test in this crate formats an image and mounts it. What lived through
//! it is almost all of the validation `Volume::open` does -- the checks that
//! only ever fire on an image this driver did not write.
//!
//! That matters more here than it would in a library. `crc32c` is not a MAC,
//! and the Eclipse installer mounts btrfs filesystems it did not create, so
//! every one of these is reachable from a disk somebody handed you. One of
//! them would be worse than a bad answer: loosen `map_logical`'s end-of-chunk
//! bound by one and an address at the very end of a chunk maps to a run of
//! zero bytes, which `read_logical` turns into `while done < buf.len()`
//! adding zero to `done` for ever.
//!
//! Two of these rules are stated over two images on purpose. The superblock
//! mirrors are written by two separate copies of one loop -- `mkfs::format`
//! and `Volume::write_superblock` -- so reading an image once proves only one
//! of them; and the metadata floor and ceiling are reachable only at opposite
//! ends of the device-size range, so neither shows up on the image the other
//! one needs.
//!
//! The crafting helpers are deliberately local to this file. Each file under
//! `tests/` is its own binary, and what each one forges differs: this one
//! rewrites the superblock and leaves a tree block's checksum *wrong* on
//! purpose, which is the opposite of what `crafted_block.rs` needs.

use std::path::PathBuf;
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::structs::{
    header, sb, ChunkItem, Stripe, Superblock, BLOCK_GROUP_DATA, BLOCK_GROUP_DUP,
    BLOCK_GROUP_METADATA, BLOCK_GROUP_RAID_MASK, CSUM_SIZE, EXTENT_TREE, HEADER_SIZE,
    INCOMPAT_RAID56, KEY_SIZE, SUPERBLOCK_MAGIC, SUPERBLOCK_OFFSETS, SUPERBLOCK_SIZE,
};
use btrfs::volume::Volume;
use btrfs::{mkfs, Btrfs, Error};

/// 1 MiB reserved + 4 MiB system + 8 MiB metadata + a data chunk.
const MKFS_SIZE: u64 = 24 * 1024 * 1024;

const MIB: u64 = 1024 * 1024;

fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "volume".into(),
        fsid: [0x11; 16],
        chunk_uuid: [0x22; 16],
        dev_uuid: [0x33; 16],
        subvol_uuid: [0x44; 16],
        now: (1_700_000_000, 0),
    }
}

fn tmpfile(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("btrfs-volume-{}-{}", std::process::id(), name))
}

fn open_dev_sized(path: &PathBuf, size: u64) -> Arc<dyn BlockDevice> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Explicitly not truncating: every helper here reopens the same image.
        .truncate(false)
        .open(path)
        .unwrap();
    f.set_len(size).unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

fn open_dev(path: &PathBuf) -> Arc<dyn BlockDevice> {
    open_dev_sized(path, MKFS_SIZE)
}

/// Read `buf.len()` bytes at `off` straight out of the image file, outside
/// the driver. False means the file is shorter than that, which is how the
/// absence of a superblock mirror shows up.
fn read_exact_at(f: &std::fs::File, off: u64, buf: &mut [u8]) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = f;
    if f.seek(SeekFrom::Start(off)).is_err() {
        return false;
    }
    f.read_exact(buf).is_ok()
}

/// Format an image of `size` bytes and check it mounts, so a later refusal is
/// the crafting and not the image.
fn fresh_sized(name: &str, size: u64) -> PathBuf {
    let path = tmpfile(name);
    let _ = std::fs::remove_file(&path);
    let dev = open_dev_sized(&path, size);
    mkfs::format(&*dev, &opts()).unwrap();
    drop(dev);
    // A format writes nothing past the end of the device. Here that is
    // invisible unless it is checked right now: writing past the end of a
    // file just extends it, and the next `set_len` would hide the extension
    // again. On a real disk the same write is an error the format would
    // return, so this is the only place the rule can be stated.
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        size,
        "the format wrote past the end of a {} MiB device",
        size / MIB
    );
    let mut fs = Btrfs::mount(open_dev_sized(&path, size), false).unwrap();
    fs.readdir(fs.root_ino()).unwrap();
    path
}

fn fresh(name: &str) -> PathBuf {
    fresh_sized(name, MKFS_SIZE)
}

/// Rewrite the primary superblock: read it, let `edit` touch the raw bytes,
/// recompute its checksum the way a forger would, and write it back.
fn craft_sb(path: &PathBuf, edit: impl FnOnce(&mut [u8])) {
    let dev = open_dev(path);
    let mut raw = vec![0u8; SUPERBLOCK_SIZE];
    dev.read_at(SUPERBLOCK_OFFSETS[0], &mut raw).unwrap();
    edit(&mut raw);
    let mut sb = Superblock::parse(raw).expect("the crafted superblock is still a superblock");
    sb.update_csum();
    dev.write_at(SUPERBLOCK_OFFSETS[0], &sb.raw).unwrap();
}

/// Rewrite a tree block. With `recsum` the checksum is recomputed, which is
/// what a forger would do; without it the block is left with the checksum of
/// the bytes it used to hold, which is what a flipped bit on a disk looks
/// like.
fn craft_block(path: &PathBuf, logical: u64, recsum: bool, edit: impl FnOnce(&mut [u8])) {
    let vol = Volume::open(open_dev(path)).unwrap();
    let mut raw = vec![0u8; vol.nodesize];
    vol.read_logical(logical, &mut raw).unwrap();
    edit(&mut raw);
    if recsum {
        header::update_csum(&mut raw);
        assert!(
            header::csum_ok(&raw),
            "the crafted block has to pass the checksum, or it never reaches the walk"
        );
    }
    vol.write_logical(logical, &raw).unwrap();
}

fn mount(path: &PathBuf) -> Result<Btrfs, Error> {
    Btrfs::mount(open_dev(path), false)
}

/// Mounting `path` has to come back as exactly `want`, and the message is part
/// of the statement: these checks sit next to each other in `Volume::open`,
/// and a mount that fails for the wrong reason has not been shown to make the
/// one under test.
fn refused(path: &PathBuf, want: Error) {
    assert_eq!(mount(path).err(), Some(want), "mounting {:?}", path);
}

// -- what `Volume::open` refuses ------------------------------------------

#[test]
fn a_superblock_claiming_more_than_one_device_is_refused() {
    // This reader maps a logical address through one device's stripes. On a
    // two-device filesystem half the stripes name the other disk, so the
    // reads would land at those offsets on this one and come back as whatever
    // happened to be there.
    let path = fresh("numdev");
    craft_sb(&path, |raw| {
        raw[sb::OFF_NUM_DEVICES..sb::OFF_NUM_DEVICES + 8].copy_from_slice(&2u64.to_le_bytes());
    });
    refused(&path, Error::Unsupported("multi-device filesystems"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_incompat_flag_this_reader_cannot_read_is_refused() {
    // An incompat flag means the on-disk format changed in a way an older
    // reader cannot cope with. RAID56 is one this driver does not implement,
    // and mounting it anyway would read parity stripes as data.
    let path = fresh("incompat");
    craft_sb(&path, |raw| {
        let cur = u64::from_le_bytes(
            raw[sb::OFF_INCOMPAT_FLAGS..sb::OFF_INCOMPAT_FLAGS + 8]
                .try_into()
                .unwrap(),
        );
        raw[sb::OFF_INCOMPAT_FLAGS..sb::OFF_INCOMPAT_FLAGS + 8]
            .copy_from_slice(&(cur | INCOMPAT_RAID56).to_le_bytes());
    });
    refused(&path, Error::Unsupported("unknown incompat flags"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_node_or_sector_size_outside_what_this_reader_accepts_is_refused() {
    // `read_block` allocates `nodesize` bytes for every tree block it reads,
    // from a number off the disk. Without a ceiling a crafted superblock asks
    // a kernel for a 4 GiB allocation per block; without a floor the header
    // does not even fit in one.
    for (name, nodesize) in [("nodebig", 1u32 << 30), ("nodesmall", 256)] {
        let path = fresh(name);
        craft_sb(&path, |raw| {
            raw[sb::OFF_NODESIZE..sb::OFF_NODESIZE + 4].copy_from_slice(&nodesize.to_le_bytes());
        });
        refused(&path, Error::Corrupt("bad node/sector size"));
        let _ = std::fs::remove_file(&path);
    }

    let path = fresh("sectorbig");
    craft_sb(&path, |raw| {
        raw[sb::OFF_SECTORSIZE..sb::OFF_SECTORSIZE + 4]
            .copy_from_slice(&(1u32 << 20).to_le_bytes());
    });
    refused(&path, Error::Corrupt("bad node/sector size"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_sys_chunk_array_entry_that_is_not_a_chunk_is_refused() {
    // The superblock carries its own little array of chunk items, enough to
    // find the chunk tree. Each entry is a key followed by a chunk item, so
    // the key's type is the only thing saying the bytes after it are a chunk
    // at all: read something else as one and every field, the stripe count
    // included, comes from whatever those bytes were.
    let path = fresh("notachunk");
    craft_sb(&path, |raw| {
        // objectid(8) then item_type(1): the first entry's type byte.
        raw[sb::OFF_SYS_CHUNK_ARRAY + 8] = 1;
    });
    refused(&path, Error::Corrupt("sys_chunk_array entry"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_chunk_in_a_raid_profile_this_reader_cannot_map_is_refused() {
    // This reader maps a chunk one of two ways: SINGLE, or DUP, where every
    // stripe holds the same bytes. Any other profile stripes or computes the
    // data across its stripes, so reading stripe 0 as if it were the whole
    // chunk returns a piece of the file, or parity, with no error at all --
    // which is worse than refusing to mount.
    //
    // 1 << 3 is RAID0 on disk; this crate only names the one profile it can
    // map, so the bit is spelled out here.
    const RAID0: u64 = 1 << 3;
    const {
        assert!(
            RAID0 & BLOCK_GROUP_RAID_MASK != 0 && RAID0 != BLOCK_GROUP_DUP,
            "the bit has to be a RAID bit, and not the one profile this reader maps"
        )
    };

    let path = fresh("raid");
    craft_sb(&path, |raw| {
        // The first entry's chunk item starts past the key; `type_` is at 24.
        let at = sb::OFF_SYS_CHUNK_ARRAY + KEY_SIZE + 24;
        let cur = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap());
        raw[at..at + 8].copy_from_slice(&((cur & !BLOCK_GROUP_RAID_MASK) | RAID0).to_le_bytes());
    });
    refused(&path, Error::Unsupported("RAID profile"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_chunk_map_holds_one_entry_per_logical_start() {
    // Every mount registers the system chunks twice: once from the array in
    // the superblock, and again when the chunk tree is walked. The map has to
    // be keyed by logical start and replace, not append -- a second entry for
    // the same address makes which one `map_logical` finds a matter of where
    // the binary search happens to land.
    let path = fresh("dupchunk");
    let vol = Volume::open(open_dev(&path)).unwrap();
    let chunks = vol.chunks();
    assert!(
        chunks.len() > 1,
        "a fresh image has a system, a metadata and a data chunk, not {}",
        chunks.len()
    );
    for pair in chunks.windows(2) {
        assert!(
            pair[0].logical < pair[1].logical,
            "two chunk entries at {:#x} and {:#x}: the map is not one entry per start",
            pair[0].logical,
            pair[1].logical
        );
    }
    let _ = std::fs::remove_file(&path);
}

// -- the logical to physical map -------------------------------------------

#[test]
fn an_address_at_the_very_end_of_a_chunk_is_refused_not_mapped_to_nothing() {
    // The one failure here that is not a wrong answer. `map_logical` returns
    // how much of the request lives in the chunk it landed in, and
    // `read_logical` loops `while done < buf.len()`, adding that to `done`.
    // So an address the map accepts but has no room in returns zero, `done`
    // never moves, and the read never comes back: a hang in a kernel, from a
    // crafted extent pointing one byte past a chunk.
    let path = fresh("hole");
    let vol = Volume::open(open_dev(&path)).unwrap();
    let chunks: Vec<(u64, u64)> = vol.chunks().iter().map(|c| (c.logical, c.length)).collect();
    assert!(!chunks.is_empty(), "a fresh image has chunks");

    for (logical, length) in &chunks {
        // Inside the chunk there is always something to read.
        let (phys, avail) = vol
            .map_logical(*logical, 4096)
            .unwrap_or_else(|e| panic!("the start of chunk {:#x} did not map: {:?}", logical, e));
        assert!(!phys.is_empty(), "chunk {:#x} mapped to no stripe", logical);
        assert!(
            avail > 0,
            "chunk {:#x} has room for nothing at its own start, so a read there would spin",
            logical
        );

        // One byte before the end is the last address in it, and it has
        // exactly one byte of room.
        let (_, avail) = vol.map_logical(logical + length - 1, 4096).unwrap();
        assert_eq!(
            avail, 1,
            "the last byte of chunk {:#x} is one byte of room, not {}",
            logical, avail
        );

        // The end itself belongs to the next chunk, if there is one.
        let end = logical + length;
        if chunks.iter().any(|(l, _)| *l == end) {
            continue;
        }
        assert_eq!(
            vol.map_logical(end, 4096).err(),
            Some(Error::Corrupt("logical address in chunk hole")),
            "{:#x} is one past the end of chunk {:#x} and belongs to no chunk",
            end,
            logical
        );
    }
    let _ = std::fs::remove_file(&path);
}

// -- a tree block's identity ------------------------------------------------

#[test]
fn a_tree_block_whose_header_names_another_address_is_refused() {
    // Every tree block records the address it belongs at. That is what stops
    // a crafted pointer from aiming at a different block of the same
    // filesystem: the bytes there are a real block with a real checksum, so
    // the checksum says nothing, and only the address does.
    let path = fresh("bytenr");
    let logical = Volume::open(open_dev(&path)).unwrap().sb.chunk_root();
    craft_block(&path, logical, true, |b| {
        header::set_bytenr(b, logical + 0x1000);
    });
    refused(&path, Error::Corrupt("tree block bytenr mismatch"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_flipped_byte_in_a_tree_block_is_caught_by_its_checksum() {
    // And the checksum is what stops the other case: a disk that returned
    // the right block with a bit rotted in it. Note what this test does NOT
    // do, which is recompute the checksum -- that is the whole point.
    let path = fresh("csum");
    let logical = Volume::open(open_dev(&path)).unwrap().sb.chunk_root();
    craft_block(&path, logical, false, |b| {
        b[HEADER_SIZE] ^= 0x40;
    });
    refused(&path, Error::Corrupt("tree block checksum"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_superblock_whose_checksum_does_not_match_its_bytes_is_refused() {
    // The superblock was the one block not checked against its own checksum,
    // although `write_superblock` computes it on every commit and `read_block`
    // checks every tree block. A flipped bit here is not a bad read: it is a
    // volume whose root pointer, generation and chunk array are whatever the
    // bit made them.
    //
    // The byte flipped is in the label, which nothing validates, so the only
    // thing wrong with the image is the number -- and it is written back with
    // the checksum it used to have, which is what a bad disk looks like rather
    // than a forgery.
    // On by default, and stated here because a default of off is a guard that
    // is not there. This is also the only test in this binary that touches
    // that flag: two would race, since the tests of one binary share it and
    // run in parallel threads.
    assert!(
        btrfs::volume::superblock_csum_enforced(),
        "the superblock checksum check has to be on unless something turns it off"
    );
    let path = fresh("sbcsum");
    let dev = open_dev(&path);
    let mut raw = vec![0u8; SUPERBLOCK_SIZE];
    dev.read_at(SUPERBLOCK_OFFSETS[0], &mut raw).unwrap();
    raw[sb::OFF_LABEL] ^= 0x20;
    dev.write_at(SUPERBLOCK_OFFSETS[0], &raw).unwrap();
    drop(dev);
    refused(&path, Error::Corrupt("superblock checksum"));

    // And the way back in, because a machine whose root disk took that bit has
    // to boot before anyone can repair it. `btrfs.sbcsum=off` on the kernel
    // command line clears this and the mount warns instead of refusing.
    btrfs::volume::set_superblock_csum_enforced(false);
    let err = mount(&path).err();
    btrfs::volume::set_superblock_csum_enforced(true);
    assert_eq!(
        err, None,
        "with the check off a bad superblock checksum has to mount anyway"
    );
    // Restored, or every test after this one in this binary would be measuring
    // the wrong thing.
    refused(&path, Error::Corrupt("superblock checksum"));
    let _ = std::fs::remove_file(&path);
}

// -- what a format lays down ------------------------------------------------

/// Read back every superblock mirror that fits on `path` and check each one
/// stands alone, returning how many were found. `who` names the code that
/// wrote them, because two different loops do and the message has to say
/// which one is wrong.
fn mirrors_that_stand_alone(path: &PathBuf, size: u64, who: &str) -> usize {
    let dev = open_dev_sized(path, size);
    let total = dev.size();
    let mut seen = 0usize;
    for &off in SUPERBLOCK_OFFSETS.iter() {
        if off + SUPERBLOCK_SIZE as u64 > total {
            continue;
        }
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        dev.read_at(off, &mut raw).unwrap();
        assert_eq!(
            u64::from_le_bytes(raw[sb::OFF_MAGIC..sb::OFF_MAGIC + 8].try_into().unwrap()),
            SUPERBLOCK_MAGIC,
            "the mirror {} wrote at {:#x} does not start with the btrfs magic",
            who,
            off
        );
        assert_eq!(
            u64::from_le_bytes(raw[sb::OFF_BYTENR..sb::OFF_BYTENR + 8].try_into().unwrap()),
            off,
            "the mirror {} wrote at {:#x} records someone else's address",
            who,
            off
        );
        let stored = u32::from_le_bytes(raw[..4].try_into().unwrap());
        let want = btrfs_crc32c(&raw[CSUM_SIZE..]);
        assert_eq!(
            stored, want,
            "the checksum {} stored in the mirror at {:#x} is not the one its own bytes make",
            who, off
        );
        assert!(
            raw[4..CSUM_SIZE].iter().all(|&b| b == 0),
            "the mirror {} wrote at {:#x} left {} bytes of its checksum field unzeroed",
            who,
            off,
            raw[4..CSUM_SIZE].iter().filter(|&&b| b != 0).count()
        );
        seen += 1;
    }
    seen
}

#[test]
fn every_superblock_mirror_is_a_superblock_of_its_own() {
    // A mirror exists to be read when the primary cannot be. So each one has
    // to stand alone: its own magic, its own address in its own `bytenr`
    // field (which is what tells a checker it is the mirror it claims to be),
    // and a checksum over everything past the checksum itself -- with the
    // rest of that 32-byte field zeroed, because crc32c fills four of them
    // and btrfs-progs reads the other 28 as part of the sum.
    //
    // Checked twice, because the loop that does this exists twice: once in
    // `mkfs::format` and once in `Volume::write_superblock`, which is where
    // every commit after the format goes. Nothing commits on drop, so a
    // freshly formatted image says nothing about the second one and an image
    // that has been synced says nothing about the first.
    //
    // 68 MiB so the 64 MiB mirror falls inside the image: with two mirrors
    // the `bytenr` field differs between them, and one carrying the other's
    // address is a visible lie rather than a constant.
    const SIZE: u64 = 68 * MIB;
    let path = fresh_sized("mirrors", SIZE);
    assert_eq!(
        mirrors_that_stand_alone(&path, SIZE, "the format"),
        2,
        "a {} MiB image has room for the first two mirrors",
        SIZE / MIB
    );

    let mut fs = Btrfs::mount(open_dev_sized(&path, SIZE), false).unwrap();
    fs.sync().unwrap();
    drop(fs);
    assert_eq!(
        mirrors_that_stand_alone(&path, SIZE, "the commit"),
        2,
        "a commit has to rewrite every mirror the format wrote"
    );
    let _ = std::fs::remove_file(&path);
}

/// crc32c of `data`, computed here rather than taken from the crate, so this
/// test says what the number on the disk has to be and not merely that two
/// calls to the same function agree.
fn btrfs_crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            // The Castagnoli polynomial, reflected.
            crc = (crc >> 1) ^ (0x82F6_3B78 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

#[test]
fn a_write_into_a_dup_chunk_reaches_every_stripe_of_it() {
    // DUP means the chunk has two stripes and both are supposed to hold the
    // same bytes. Reads only ever take stripe 0, so a write that stopped
    // there would look perfectly healthy until the day the first copy could
    // not be read -- which is the one day the second copy was the point.
    //
    // Nothing in this crate makes a DUP chunk: `mkfs` here writes SINGLE
    // profiles and so does the runtime allocator, one stripe each. DUP
    // arrives from images written by somebody else, and `mkfs.btrfs` has
    // defaulted to DUP metadata on a single device for years, so it is the
    // ordinary case for a disk handed to the installer. The chunk therefore
    // goes in through `register_chunk`, which is the same door the allocator
    // uses, rather than by forging one into the superblock's array.
    let size = 128 * MIB;
    let path = fresh_sized("dup", size);
    let mut vol = Volume::open(open_dev_sized(&path, size)).unwrap();
    let nodesize = vol.nodesize;

    // Above every chunk the format laid down, so the map finds this one; and
    // the two stripes land past the data area, where nothing live sits.
    let logical = 1024 * MIB;
    let stripes = vec![
        Stripe {
            devid: 1,
            offset: 70 * MIB,
        },
        Stripe {
            devid: 1,
            offset: 80 * MIB,
        },
    ];
    assert!(
        vol.chunks().iter().all(|c| c.logical + c.length <= logical),
        "the test chunk has to sit above the ones the format made"
    );
    vol.register_chunk(
        logical,
        &ChunkItem {
            length: 4 * MIB,
            owner: EXTENT_TREE,
            stripe_len: 65536,
            type_: BLOCK_GROUP_METADATA | BLOCK_GROUP_DUP,
            io_align: 65536,
            io_width: 65536,
            sector_size: vol.sectorsize as u32,
            sub_stripes: 1,
            stripes: stripes.clone(),
        },
    )
    .unwrap();

    let mut block = vec![0u8; nodesize];
    header::set_bytenr(&mut block, logical);
    for (i, b) in block.iter_mut().enumerate().skip(HEADER_SIZE) {
        // Not a constant run, so a stripe left as zeros cannot pass.
        *b = (i % 251) as u8;
    }
    vol.write_block(logical, block).unwrap();
    vol.flush_dirty().unwrap();

    let (phys, avail) = vol.map_logical(logical, nodesize as u64).unwrap();
    assert_eq!(
        avail as usize, nodesize,
        "the block does not fit in the chunk"
    );
    assert_eq!(
        phys,
        stripes.iter().map(|s| s.offset).collect::<Vec<_>>(),
        "a DUP chunk maps to both of its stripes, at their own offsets"
    );

    let dev = open_dev_sized(&path, size);
    let mut first = vec![0u8; nodesize];
    dev.read_at(phys[0], &mut first).unwrap();
    assert!(
        first[HEADER_SIZE..].iter().any(|&b| b != 0),
        "stripe 0 did not get the block at all"
    );
    for (i, p) in phys.iter().enumerate().skip(1) {
        let mut other = vec![0u8; nodesize];
        dev.read_at(*p, &mut other).unwrap();
        let differs = first.iter().zip(other.iter()).position(|(a, b)| a != b);
        assert!(
            differs.is_none(),
            "stripe {} of the DUP chunk first differs from stripe 0 at byte {:?}",
            i,
            differs
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_device_below_the_minimum_size_is_refused() {
    // The minimum is what the fixed layout costs: 1 MiB reserved, 4 MiB of
    // system chunk, 8 MiB of metadata and 4 MiB of data. Formatting below it
    // cannot produce those chunks, and the place to say so is here rather
    // than at the first write that finds no room.
    let path = tmpfile("tooshort");
    let _ = std::fs::remove_file(&path);
    let dev = open_dev_sized(&path, mkfs::MIN_DEVICE_SIZE - 1);
    assert_eq!(
        mkfs::format(&*dev, &opts()),
        Err(Error::Invalid),
        "formatting one byte below the minimum of {} MiB",
        mkfs::MIN_DEVICE_SIZE / MIB
    );
    drop(dev);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_metadata_area_keeps_its_floor_on_the_smallest_device_this_format_takes() {
    // Metadata is an eighth of the device, but never less than 8 MiB: the
    // format lays down eight tree leaves and a writable mount allocates more
    // before it can do anything. The floor is only reachable below 64 MiB,
    // which is why the plan test below cannot see it -- at 68 MiB an eighth is
    // already above the floor and lowering the floor changes nothing.
    const SIZE: u64 = mkfs::MIN_DEVICE_SIZE;
    const { assert!(SIZE < 64 * MIB) };
    let path = fresh_sized("floor", SIZE);
    let vol = Volume::open(open_dev_sized(&path, SIZE)).unwrap();
    let meta: u64 = vol
        .chunks()
        .iter()
        .filter(|c| c.type_ & BLOCK_GROUP_METADATA != 0)
        .map(|c| c.length)
        .sum();
    assert!(
        meta >= 8 * MIB,
        "a {} MiB device got {} MiB of metadata; an eighth of it is {} MiB, so the floor is what \
         has to decide here",
        SIZE / MIB,
        meta / MIB,
        SIZE / 8 / MIB
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_metadata_area_keeps_its_ceiling_on_a_device_big_enough_to_hit_it() {
    // The other end of the same rule: an eighth of the device, capped at
    // 32 MiB, because past that the metadata chunk is paid for in data space
    // nobody gets back. The cap only bites above 256 MiB, so no other image
    // in this file reaches it -- 300 MiB does, and the file is sparse, so it
    // costs the eight tree leaves the format actually writes.
    const SIZE: u64 = 300 * MIB;
    const { assert!(SIZE / 8 > 32 * MIB) };
    let path = fresh_sized("ceiling", SIZE);
    let vol = Volume::open(open_dev_sized(&path, SIZE)).unwrap();
    let meta: u64 = vol
        .chunks()
        .iter()
        .filter(|c| c.type_ & BLOCK_GROUP_METADATA != 0)
        .map(|c| c.length)
        .sum();
    assert!(
        meta <= 32 * MIB,
        "a {} MiB device got {} MiB of metadata; an eighth of it is {} MiB, so the cap is what \
         has to decide here",
        SIZE / MIB,
        meta / MIB,
        SIZE / 8 / MIB
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_chunks_a_fresh_image_gets_are_the_ones_the_plan_promises() {
    // 68 MiB on purpose: the second superblock mirror sits at 64 MiB, inside
    // the image, so the plan has to break the data area around it and the
    // 3 MiB tail left over is too small to be a chunk of its own. A 24 MiB
    // image has one data chunk and would not exercise either rule.
    let path = fresh_sized("plan", 68 * MIB);
    let vol = Volume::open(open_dev_sized(&path, 68 * MIB)).unwrap();
    let total = vol.dev.size();

    let data: Vec<(u64, u64)> = vol
        .chunks()
        .iter()
        .filter(|c| c.type_ & BLOCK_GROUP_DATA != 0)
        .map(|c| (c.logical, c.length))
        .collect();
    assert!(!data.is_empty(), "a 68 MiB image has room for data");
    for (logical, length) in &data {
        assert!(
            *length >= 4 * MIB,
            "the data chunk at {:#x} is {} bytes; a chunk smaller than 4 MiB is not worth its \
             block group",
            logical,
            length
        );
    }

    // And nothing the plan hands out may cover a superblock mirror: the next
    // commit would write the superblock straight through somebody's file.
    for c in vol.chunks() {
        for s in &c.stripes {
            for &off in SUPERBLOCK_OFFSETS.iter() {
                if off + SUPERBLOCK_SIZE as u64 > total {
                    continue;
                }
                assert!(
                    off < s.offset || off >= s.offset + c.length,
                    "the chunk at {:#x} covers the superblock mirror at {:#x}",
                    c.logical,
                    off
                );
            }
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn no_chunk_a_format_lays_down_reaches_past_the_end_of_the_device() {
    // A device whose size is not a whole number of mebibytes is the ordinary
    // case: a partition ends where the partition table says it does, not where
    // the chunk alignment would prefer. The plan rounds the device size *down*
    // to the alignment for exactly that reason, and rounding it up instead is
    // invisible to everything else in this crate -- a format writes nothing
    // out there (`fresh_sized` checks that), and a data write that lands past
    // the end of an image *file* only extends the file. On the disk the
    // installer writes to, those addresses belong to the next partition.
    for (name, size) in [
        ("tail0", MKFS_SIZE),
        ("tail1", MKFS_SIZE + 1),
        ("tailsector", MKFS_SIZE + 4096),
        ("tailhalf", MKFS_SIZE + 512 * 1024),
        ("tailalmost", MKFS_SIZE + MIB - 1),
    ] {
        let path = fresh_sized(name, size);
        let vol = Volume::open(open_dev_sized(&path, size)).unwrap();
        let total = vol.dev.size();
        assert_eq!(total, size);
        for c in vol.chunks() {
            for s in &c.stripes {
                assert!(
                    s.offset + c.length <= total,
                    "the chunk at {:#x} occupies [{:#x}, {:#x}) of a device that ends at {:#x}",
                    c.logical,
                    s.offset,
                    s.offset + c.length,
                    total
                );
            }
        }
        drop(vol);
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn the_metadata_area_is_an_eighth_of_a_device_between_the_two_bounds() {
    // The floor and the ceiling are each stated above, on the only device
    // sizes that reach them. Neither says what the rule between them is, and
    // the share is what decides every real image.
    //
    // 100 MiB on purpose: an eighth of it is 12.5 MiB, which is inside both
    // bounds *and* not a whole number of mebibytes, so this states the
    // rounding as well. A chunk has to start and end on the alignment, and
    // rounding this one up instead takes the extra megabyte out of the data
    // area -- where nothing else in this file is counting.
    const SIZE: u64 = 100 * MIB;
    const { assert!(SIZE / 8 > 8 * MIB && SIZE / 8 < 32 * MIB) };
    const { assert!(SIZE / 8 / MIB * MIB != SIZE / 8) };
    let path = fresh_sized("eighth", SIZE);
    let vol = Volume::open(open_dev_sized(&path, SIZE)).unwrap();
    let meta: u64 = vol
        .chunks()
        .iter()
        .filter(|c| c.type_ & BLOCK_GROUP_METADATA != 0)
        .map(|c| c.length)
        .sum();
    assert_eq!(
        meta,
        SIZE / 8 / MIB * MIB,
        "a {} MiB device got {} bytes of metadata; an eighth of it is {} bytes, and a chunk ends \
         on the alignment below that",
        SIZE / MIB,
        meta,
        SIZE / 8
    );
    drop(vol);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_smallest_device_this_format_takes_still_has_room_for_a_file() {
    // The minimum device size is the fixed layout plus four megabytes of
    // data, and four megabytes is also the smallest gap the plan will turn
    // into a data chunk. So at exactly the minimum the last gap is exactly
    // the smallest acceptable chunk: the one device size where that bound is
    // reachable from outside the allocator, and the only one where raising it
    // leaves a filesystem that mounts, lists and refuses to store anything.
    const SIZE: u64 = mkfs::MIN_DEVICE_SIZE;
    let path = fresh_sized("smallest", SIZE);
    let vol = Volume::open(open_dev_sized(&path, SIZE)).unwrap();
    let data: u64 = vol
        .chunks()
        .iter()
        .filter(|c| c.type_ & BLOCK_GROUP_DATA != 0)
        .map(|c| c.length)
        .sum();
    assert_eq!(
        data,
        4 * MIB,
        "a {} MiB device got {} bytes of data chunk; the gap left over after the fixed layout is \
         exactly the 4 MiB minimum, so it is a chunk or there is none",
        SIZE / MIB,
        data
    );
    drop(vol);

    // And the chunk is usable, not merely recorded.
    let mut fs = Btrfs::mount(open_dev_sized(&path, SIZE), false).unwrap();
    let root = fs.root_ino();
    let ino = fs
        .create(root, "f", btrfs::FileKind::Regular, 0o644, 0)
        .unwrap();
    let block = vec![0x3cu8; 64 * 1024];
    assert_eq!(
        fs.write(ino, 0, &block).unwrap(),
        block.len(),
        "the smallest filesystem this format makes could not store 64 KiB"
    );
    let mut got = vec![0u8; block.len()];
    assert_eq!(fs.read(ino, 0, &mut got).unwrap(), got.len());
    assert_eq!(got, block);
    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_fresh_image_reports_the_blocks_its_own_trees_take() {
    // df's "used" on a filesystem nobody has written to yet. The format lays
    // down eight tree leaves and says so in the superblock; a mount adds its
    // own delta to that number and nothing else sets the floor it starts
    // from, so a wrong count here is a wrong number for the life of the
    // filesystem.
    let path = fresh_sized("used", MKFS_SIZE);
    let fs = Btrfs::mount(open_dev_sized(&path, MKFS_SIZE), true).unwrap();
    assert_eq!(
        fs.fsinfo().bytes_used,
        8 * mkfs::NODESIZE as u64,
        "a fresh image accounts for something other than the eight leaves the format writes"
    );
    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_superblock_mirror_that_fits_by_one_sector_is_written() {
    // The mirrors are what a filesystem is recovered from, and the second one
    // sits at 64 MiB. A device of exactly 64 MiB plus the superblock has room
    // for it to the byte, and a bound that asks for one byte more quietly
    // leaves such a device with a single copy of its superblock.
    const OFF: u64 = SUPERBLOCK_OFFSETS[1];
    for (name, size, want) in [
        ("fits", OFF + SUPERBLOCK_SIZE as u64, true),
        ("short", OFF + SUPERBLOCK_SIZE as u64 - 1, false),
    ] {
        let path = fresh_sized(name, size);
        let mut magic = [0u8; 8];
        let f = std::fs::File::open(&path).unwrap();
        let got = read_exact_at(&f, OFF + sb::OFF_MAGIC as u64, &mut magic);
        let present = got && u64::from_le_bytes(magic) == SUPERBLOCK_MAGIC;
        assert_eq!(
            present, want,
            "the mirror at {:#x} on a device of {} bytes: present={}, expected={}",
            OFF, size, present, want
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn the_label_a_format_writes_stays_inside_the_field_it_lives_in() {
    // The label field is 256 bytes and whoever reads it expects a C string:
    // `btrfs filesystem label` prints up to the first NUL. Filling all 256
    // leaves no terminator, and the reader walks on into the fsid that
    // follows.
    const FIELD: usize = 256;
    let path = tmpfile("label");
    let _ = std::fs::remove_file(&path);
    let dev = open_dev_sized(&path, MKFS_SIZE);
    let mut opts = opts();
    opts.label = "l".repeat(FIELD + 44);
    mkfs::format(&*dev, &opts).unwrap();
    drop(dev);

    let mut field = [0u8; FIELD];
    let f = std::fs::File::open(&path).unwrap();
    assert!(read_exact_at(
        &f,
        SUPERBLOCK_OFFSETS[0] + sb::OFF_LABEL as u64,
        &mut field
    ));
    assert_eq!(
        field.iter().position(|&b| b == 0),
        Some(FIELD - 1),
        "a label longer than the field left {} of its bytes and no terminator",
        field.iter().filter(|&&b| b != 0).count()
    );
    let _ = std::fs::remove_file(&path);
}
