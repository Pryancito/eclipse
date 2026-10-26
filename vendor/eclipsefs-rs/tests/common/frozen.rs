//! The frozen images of format 1.0 (`tests/format/`, see `FORMAT.md`):
//! loading them and describing what a filesystem holds, in one text that
//! does not depend on where anything sits on disk.
//!
//! Each image was made once by `bless_missing_images` in
//! `tests/format.rs` and is never made again: what a driver wrote on the
//! day it was frozen is what every later driver must read.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use super::MemDisk;
use eclipsefs::crc::crc32c;
use eclipsefs::device::BlockDevice;
use eclipsefs::{EclipseFS, FileKind};

/// Master key of the `encrypted` image.
pub const KEY: [u8; 32] = *b"eclipsefs-1.0-frozen-image-key!!";

/// The images, and how many devices each has.
pub const IMAGES: [(&str, usize); 12] = [
    ("basic", 1),
    ("subvolumes", 1),
    ("compressed", 1),
    ("encrypted", 1),
    ("fsync-log", 1),
    ("journal-pending", 1),
    ("resized", 1),
    ("raid1", 2),
    ("raid10", 4),
    ("raid5", 3),
    ("raid6", 3),
    ("tiers", 2),
];

/// The older in-place format, frozen as it was.
pub const V3_IMAGE: &str = "v3";

pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/format")
}

/// File of device `i` of image `name`.
pub fn image_path(name: &str, i: usize, devices: usize) -> PathBuf {
    if devices == 1 {
        dir().join(format!("{name}.img.z"))
    } else {
        dir().join(format!("{name}.dev{}.img.z", i + 1))
    }
}

pub fn listing_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.txt"))
}

/// The devices of a frozen image, as RAM disks.
pub fn load(name: &str, devices: usize) -> Vec<Arc<MemDisk>> {
    (0..devices)
        .map(|i| {
            let p = image_path(name, i, devices);
            let z = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            let bytes = miniz_oxide::inflate::decompress_to_vec_zlib(&z)
                .unwrap_or_else(|e| panic!("{}: {e:?}", p.display()));
            MemDisk::from_bytes(bytes)
        })
        .collect()
}

pub fn store(name: &str, disks: &[Arc<MemDisk>]) {
    std::fs::create_dir_all(dir()).unwrap();
    for (i, d) in disks.iter().enumerate() {
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&d.bytes(), 10);
        std::fs::write(image_path(name, i, disks.len()), z).unwrap();
    }
}

pub fn devs(d: &[Arc<MemDisk>]) -> Vec<Arc<dyn BlockDevice>> {
    d.iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect()
}

/// Mount the devices of an image, with the key of the encrypted one.
pub fn mount(d: &[Arc<MemDisk>], read_only: bool) -> eclipsefs::Result<EclipseFS> {
    let mut fs = if d.len() == 1 {
        EclipseFS::mount(d[0].clone(), read_only)?
    } else {
        EclipseFS::mount_devices(devs(d), read_only)?
    };
    fs.set_clock(super::clock);
    fs.set_random(super::frozen::random);
    if fs.format_version() == 4 {
        fs.add_encryption_key(&KEY)?;
    }
    Ok(fs)
}

/// Deterministic "random" bytes (nonces of the encrypted image).
pub fn random(buf: &mut [u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for b in buf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        *b = (n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8;
    }
}

fn kind(k: FileKind) -> &'static str {
    match k {
        FileKind::Regular => "file",
        FileKind::Dir => "dir",
        FileKind::Symlink => "symlink",
        FileKind::CharDevice => "chardev",
        FileKind::BlockDevice => "blockdev",
        FileKind::Fifo => "fifo",
        FileKind::Socket => "socket",
    }
}

/// Everything the filesystem holds, as text: its subvolumes, then every
/// name in each with the inode's attributes, flags, compression,
/// extended attributes and the CRC32C of its contents. Nothing about
/// where blocks are, so a driver that places them differently lists the
/// same; anything a reader gets wrong shows up.
pub fn listing(fs: &mut EclipseFS) -> String {
    let mut out = String::new();
    let st = fs.fsinfo();
    writeln!(
        out,
        "filesystem label={:?} version={:#x} block={} total={}",
        fs.label(),
        fs.format_version(),
        st.block_size,
        st.total_bytes
    )
    .unwrap();
    let mut roots = vec![(5u64, fs.root_ino())];
    if let Ok(list) = fs.list_subvolumes() {
        roots.clear();
        for s in list {
            writeln!(
                out,
                "subvolume {} name={:?} root={} parent={} dir={} ro={} snapshot_of={} snapshot_gen={} created={:?} uuid={} parent_uuid={} received_uuid={}",
                s.id, s.name, s.root, s.parent, s.dir, s.read_only, s.snapshot_of,
                s.snapshot_gen, s.created, hex(&s.uuid), hex(&s.parent_uuid), hex(&s.received_uuid)
            )
            .unwrap();
            roots.push((s.id, s.root));
        }
    }
    if fs.quota_enabled() {
        for q in fs.qgroup_show().unwrap() {
            writeln!(out, "qgroup {:?}", q).unwrap();
        }
    }
    for (id, root) in roots {
        writeln!(out, "== subvolume {id}").unwrap();
        entry(fs, &mut out, "/", root);
        walk(fs, &mut out, id, root, "");
    }
    out
}

fn walk(fs: &mut EclipseFS, out: &mut String, subvol: u64, dir: u64, path: &str) {
    let mut ents = fs.readdir(dir).unwrap();
    ents.retain(|e| e.name != "." && e.name != "..");
    ents.sort_by(|a, b| a.name.cmp(&b.name));
    for e in ents {
        let p = format!("{path}/{}", e.name);
        assert_eq!(fs.lookup(dir, &e.name).unwrap(), e.ino, "{p}");
        if fs.subvolume_of(e.ino) != subvol {
            writeln!(out, "{p} -> subvolume {}", fs.subvolume_of(e.ino)).unwrap();
            continue;
        }
        entry(fs, out, &p, e.ino);
        if e.kind == FileKind::Dir {
            walk(fs, out, subvol, e.ino, &p);
        }
    }
}

fn entry(fs: &mut EclipseFS, out: &mut String, path: &str, ino: u64) {
    let s = fs.stat(ino).unwrap();
    write!(
        out,
        "{path} ino={} {} mode={:o} uid={} gid={} nlink={} size={} nbytes={} rdev={} atime={:?} mtime={:?} ctime={:?}",
        s.ino, kind(s.kind), s.mode, s.uid, s.gid, s.nlink, s.size, s.nbytes, s.rdev,
        s.atime, s.mtime, s.ctime
    )
    .unwrap();
    if let Ok(f) = fs.get_flags(ino) {
        write!(out, " flags={f:#x}").unwrap();
    }
    if let Ok(Some(z)) = fs.compression(ino) {
        write!(out, " compression={}", z.name()).unwrap();
    }
    if let Ok(names) = fs.list_xattrs(ino) {
        for n in names {
            let v = fs.get_xattr(ino, &n).unwrap();
            write!(out, " xattr[{n}]={}:{:08x}", v.len(), crc32c(&v)).unwrap();
        }
    }
    match s.kind {
        FileKind::Regular => {
            let mut buf = vec![0u8; s.size as usize];
            let n = fs.read(ino, 0, &mut buf).unwrap();
            assert_eq!(n as u64, s.size, "{path}");
            write!(out, " data={:08x}", crc32c(&buf)).unwrap();
        }
        FileKind::Symlink => {
            let t = fs.read_link(ino).unwrap();
            write!(out, " target={}:{:08x}", t.len(), crc32c(&t)).unwrap();
        }
        _ => {}
    }
    out.push('\n');
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
