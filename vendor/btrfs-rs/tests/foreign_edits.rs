#![cfg(feature = "std")]
//! Changes to images btrfs-progs made, where the layout is not the one our
//! own `mkfs` writes: data with checksums, one checksum item that covers
//! several files, no `NO_HOLES`, inline files, and two names of one file in
//! the same directory.
//!
//! Every path here was unexercised before (coverage run 2026-11-25):
//! splitting a checksum item when the file in its middle goes, trimming
//! one at either end, growing a file with an explicit hole extent on a
//! filesystem without `NO_HOLES`, shrinking an inline file, and dropping one
//! of two back-references an inode keeps in the same directory. Each is a
//! change a root filesystem built by `mkfs.btrfs --rootdir` (as Eclipse's
//! rootfs images are) gets on its first boot. The judge is
//! `btrfs check --check-data-csum`, which reads every data block against
//! the checksum tree, so a checksum left behind or cut at the wrong sector
//! fails it.
//!
//! Skipped when btrfs-progs is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::{Btrfs, Error, FileKind};

const SIZE: u64 = 128 * 1024 * 1024;

fn have_progs() -> bool {
    Command::new("mkfs.btrfs")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct Scratch {
    img: PathBuf,
    seed: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.img);
        let _ = fs::remove_dir_all(&self.seed);
    }
}

fn scratch(name: &str) -> Scratch {
    let base = std::env::temp_dir().join(format!("btrfs-foreign-{}-{}", std::process::id(), name));
    let s = Scratch {
        img: base.with_extension("img"),
        seed: base.with_extension("seed"),
    };
    let _ = fs::remove_dir_all(&s.seed);
    fs::create_dir_all(&s.seed).unwrap();
    s
}

/// What `mkfs.btrfs --rootdir` makes of `seed`: checksummed data, and,
/// without `NO_HOLES`, explicit hole extents.
fn mkfs(s: &Scratch) {
    let f = fs::File::create(&s.img).unwrap();
    f.set_len(SIZE).unwrap();
    drop(f);
    let out = Command::new("mkfs.btrfs")
        .args(["-q", "-O", "^free-space-tree,^no-holes", "--rootdir"])
        .arg(&s.seed)
        .arg(&s.img)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mkfs.btrfs failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn mount(img: &Path) -> Btrfs {
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(img)
        .unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(f).unwrap());
    Btrfs::mount(dev, false).unwrap()
}

fn check(img: &Path) {
    let out = Command::new("btrfs")
        .args(["check", "--force", "--check-data-csum"])
        .arg(img)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "btrfs check failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn read_all(fs: &mut Btrfs, ino: u64) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn pattern(seed: u32, len: usize) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) ^ seed) as u8)
        .collect()
}

#[test]
fn removing_files_cuts_a_shared_checksum_item_at_the_right_sectors() {
    if !have_progs() {
        eprintln!("btrfs-progs not available; skipping");
        return;
    }
    let s = scratch("csum");
    // Five files written one after the other: their data lands back to back
    // and their checksums in one item of the checksum tree.
    let files: Vec<(String, Vec<u8>)> = (0..5)
        .map(|i| (format!("f{i}"), pattern(i, 48 * 1024 + 512 * i as usize)))
        .collect();
    for (name, data) in &files {
        fs::write(s.seed.join(name), data).unwrap();
    }
    mkfs(&s);

    let mut fs = mount(&s.img);
    let root = fs.root_ino();
    // The middle one: the item is split, head and tail kept.
    fs.unlink(root, "f2").unwrap();
    // The first one: the item that now starts with it keeps its tail.
    fs.unlink(root, "f0").unwrap();
    // The last one: the item that ends with it keeps its head.
    fs.unlink(root, "f4").unwrap();
    fs.sync().unwrap();
    drop(fs);
    check(&s.img);

    let mut fs = mount(&s.img);
    let root = fs.root_ino();
    for (i, (name, data)) in files.iter().enumerate() {
        match i {
            1 | 3 => {
                let ino = fs.lookup(root, name).unwrap();
                assert_eq!(&read_all(&mut fs, ino), data, "{name}");
            }
            _ => assert_eq!(fs.lookup(root, name), Err(Error::NotFound), "{name}"),
        }
    }
}

#[test]
fn truncating_checksummed_and_inline_files_keeps_the_image_consistent() {
    if !have_progs() {
        eprintln!("btrfs-progs not available; skipping");
        return;
    }
    let s = scratch("truncate");
    let big = pattern(7, 300 * 1024);
    fs::write(s.seed.join("big"), &big).unwrap();
    fs::write(s.seed.join("grow"), pattern(9, 10_000)).unwrap();
    fs::write(
        s.seed.join("inline"),
        b"a small file that btrfs keeps inline\n",
    )
    .unwrap();
    fs::write(s.seed.join("inline0"), b"gone soon\n").unwrap();
    fs::write(s.seed.join("tail"), pattern(11, 64 * 1024)).unwrap();
    mkfs(&s);

    let mut fs = mount(&s.img);
    let root = fs.root_ino();
    let bigf = fs.lookup(root, "big").unwrap();
    let grow = fs.lookup(root, "grow").unwrap();
    let inline = fs.lookup(root, "inline").unwrap();
    let inline0 = fs.lookup(root, "inline0").unwrap();

    // Shrink a checksummed file mid-extent: the data before stays.
    fs.truncate(bigf, 100_000).unwrap();
    assert_eq!(read_all(&mut fs, bigf), &big[..100_000]);
    // Grow one on a filesystem without NO_HOLES: an explicit hole extent
    // past the old end, read back as zeros.
    fs.truncate(grow, 1_000_000).unwrap();
    let got = read_all(&mut fs, grow);
    assert_eq!(&got[..10_000], &pattern(9, 10_000)[..]);
    assert!(
        got[10_000..].iter().all(|&b| b == 0),
        "the grown range is not zeros"
    );
    // Shrink an inline file, and empty another.
    fs.truncate(inline, 7).unwrap();
    assert_eq!(read_all(&mut fs, inline), b"a small");
    fs.truncate(inline0, 0).unwrap();
    assert_eq!(fs.stat(inline0).unwrap().size, 0);
    assert_eq!(fs.stat(inline0).unwrap().nbytes, 0);
    // Truncate to the size it has: nothing to do.
    fs.truncate(inline, 7).unwrap();
    // Only regular files have a size to set.
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    assert_eq!(fs.truncate(d, 0), Err(Error::IsDir));
    let l = fs.symlink(root, "l", b"big").unwrap();
    assert_eq!(fs.truncate(l, 0), Err(Error::Invalid));

    fs.sync().unwrap();
    drop(fs);
    check(&s.img);

    let mut fs = mount(&s.img);
    assert_eq!(read_all(&mut fs, bigf), &big[..100_000]);
    assert_eq!(read_all(&mut fs, inline), b"a small");
    assert_eq!(fs.stat(grow).unwrap().size, 1_000_000);
    let tail = fs.lookup(fs.root_ino(), "tail").unwrap();
    assert_eq!(read_all(&mut fs, tail), pattern(11, 64 * 1024));
}

#[test]
fn two_names_of_one_file_in_one_directory_go_one_at_a_time() {
    if !have_progs() {
        eprintln!("btrfs-progs not available; skipping");
        return;
    }
    let s = scratch("refs");
    let data = pattern(3, 20_000);
    fs::write(s.seed.join("a"), &data).unwrap();
    mkfs(&s);

    let mut fs = mount(&s.img);
    let root = fs.root_ino();
    let ino = fs.lookup(root, "a").unwrap();
    // Two more names in the same directory: one INODE_REF item holding
    // three names.
    fs.link(root, "b", ino).unwrap();
    fs.link(root, "c", ino).unwrap();
    fs.sync().unwrap();
    drop(fs);
    check(&s.img);

    let mut fs = mount(&s.img);
    // The middle name goes: the other two stay in the item.
    fs.unlink(root, "b").unwrap();
    assert_eq!(fs.lookup(root, "b"), Err(Error::NotFound));
    assert_eq!(fs.lookup(root, "a").unwrap(), ino);
    assert_eq!(fs.lookup(root, "c").unwrap(), ino);
    assert_eq!(fs.stat(ino).unwrap().nlink, 2);
    // And a rename of one of them within the directory.
    fs.rename(root, "a", root, "z").unwrap();
    fs.sync().unwrap();
    drop(fs);
    check(&s.img);

    let mut fs = mount(&s.img);
    let mut names: Vec<String> = fs
        .readdir(root)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, ["c", "z"]);
    assert_eq!(read_all(&mut fs, ino), data);
    fs.unlink(root, "c").unwrap();
    fs.unlink(root, "z").unwrap();
    assert!(fs.stat(ino).is_err());
    fs.sync().unwrap();
    drop(fs);
    check(&s.img);
}
