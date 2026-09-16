#![cfg(feature = "std")]
//! `Btrfs::parent`: the `..` of a directory, read back from its INODE_REF.
//! The kernel's mount layer needs it to follow a relative symlink such as
//! `/var/run -> ../run` on an installed root (PulseAudio's
//! `mkdir /var/run/pulse` returned ENOENT without it). No btrfs-progs needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::{mkfs, Btrfs, Error, FileKind};

fn tmpfile(name: &str, size: u64) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("btrfs-rs-parent-{}-{}", std::process::id(), name));
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    f.set_len(size).unwrap();
    path
}

fn open_dev(path: &Path) -> Arc<dyn BlockDevice> {
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

fn opts() -> mkfs::MkfsOptions {
    let mut seed = 0x0bad_cafe_f00d_1234u64;
    let mut uuid = || {
        let mut u = [0u8; 16];
        for b in u.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
        u[6] = (u[6] & 0x0f) | 0x40;
        u[8] = (u[8] & 0x3f) | 0x80;
        u
    };
    mkfs::MkfsOptions {
        label: "eclipse".into(),
        fsid: uuid(),
        chunk_uuid: uuid(),
        dev_uuid: uuid(),
        subvol_uuid: uuid(),
        now: (1_700_000_000, 0),
    }
}

#[test]
fn parent_of_nested_dirs_and_root() {
    let path = tmpfile("nested", 64 * 1024 * 1024);
    let dev = open_dev(&path);
    mkfs::format(&*dev, &opts()).unwrap();
    let mut fs = Btrfs::mount(dev, false).unwrap();
    let root = fs.root_ino();

    // /var/run -> ../run, /run/pulse: the layout the rootfs builder writes.
    let var = fs.create(root, "var", FileKind::Dir, 0o755, 0).unwrap();
    let run = fs.create(root, "run", FileKind::Dir, 0o755, 0).unwrap();
    let pulse = fs.create(run, "pulse", FileKind::Dir, 0o755, 0).unwrap();
    let link = fs.symlink(var, "run", b"../run").unwrap();
    let file = fs
        .create(pulse, "native", FileKind::Regular, 0o644, 0)
        .unwrap();

    assert_eq!(fs.parent(var).unwrap(), root);
    assert_eq!(fs.parent(run).unwrap(), root);
    assert_eq!(fs.parent(pulse).unwrap(), run);
    assert_eq!(fs.parent(link).unwrap(), var);
    assert_eq!(fs.parent(file).unwrap(), pulse);
    // The subvolume root refers to itself, like `/..` on Linux.
    assert_eq!(fs.parent(root).unwrap(), root);

    // Resolving `../run` from /var by hand, the way the kernel walks it.
    let up = fs.parent(var).unwrap();
    let target = fs.lookup(up, "run").unwrap();
    assert_eq!(target, run);
    assert_eq!(fs.lookup(target, "pulse").unwrap(), pulse);

    // Survives a remount (the INODE_REF is on disk, not in a cache).
    fs.sync().unwrap();
    drop(fs);
    let dev = open_dev(&path);
    let mut fs = Btrfs::mount(dev, true).unwrap();
    assert_eq!(fs.parent(pulse).unwrap(), run);
    assert_eq!(fs.parent(var).unwrap(), root);

    // An inode that no directory references has no parent.
    let orphan = fs.parent(0xdead_beef);
    assert!(matches!(orphan, Err(Error::NotFound)), "{orphan:?}");
    let _ = fs::remove_file(&path);
}
