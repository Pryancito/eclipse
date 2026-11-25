#![cfg(feature = "std")]
//! Names: hard links, removing directories, renaming onto what is already
//! there, the kinds `readdir` reports, and what each call refuses.
//!
//! A coverage run (2026-11-25) found none of this exercised by the rest of
//! the suite: no test removed a directory, so the check that keeps `rmdir`
//! from dropping a directory with entries in it (`dir_is_empty`) had never
//! run; no test made a hard link, so `link` and the "keep the inode while
//! another name holds it" branches of `unlink` and `rename` had never run
//! either. The served btrfs (`tools/eclipse-btrfs`) and the kernel mount
//! both call these, so they are stated here, and every image the tests leave
//! behind is checked with `btrfs check` when btrfs-progs is installed.
//!
//! The last test is about a power cut: with the commit interval at
//! `u32::MAX` (what the served btrfs sets, because it keeps its own redo
//! log of the changes since the last sync) nothing may reach the device
//! between two syncs, so a mount that is dropped without one must come
//! back exactly as it was at the last sync.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::{mkfs, Btrfs, Error, FileKind};

const MIB: u64 = 1024 * 1024;
const SIZE: u64 = 32 * MIB;

fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "names".into(),
        fsid: [0x61; 16],
        chunk_uuid: [0x62; 16],
        dev_uuid: [0x63; 16],
        subvol_uuid: [0x64; 16],
        now: (1_700_000_000, 0),
    }
}

/// Removes the image when the test ends, passed or not.
struct Image(PathBuf);

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn open_dev(path: &Path) -> Arc<dyn BlockDevice> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    f.set_len(SIZE).unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

fn fresh(name: &str) -> (Btrfs, Image) {
    let path = std::env::temp_dir().join(format!("btrfs-names-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_file(&path);
    let dev = open_dev(&path);
    mkfs::format(&*dev, &opts()).unwrap();
    drop(dev);
    let mut fs = Btrfs::mount(open_dev(&path), false).unwrap();
    fs.set_clock(|| (1_700_000_100, 0));
    (fs, Image(path))
}

fn remount(img: &Image) -> Btrfs {
    Btrfs::mount(open_dev(&img.0), false).unwrap()
}

/// `btrfs check` of the image, when btrfs-progs is there to ask.
fn check(img: &Image) {
    let Ok(out) = Command::new("btrfs")
        .args(["check", "--force"])
        .arg(&img.0)
        .output()
    else {
        eprintln!("btrfs-progs not available; skipping btrfs check");
        return;
    };
    assert!(
        out.status.success(),
        "btrfs check failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn file(fs: &mut Btrfs, dir: u64, name: &str, data: &[u8]) -> u64 {
    let ino = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    if !data.is_empty() {
        assert_eq!(fs.write(ino, 0, data).unwrap(), data.len());
    }
    ino
}

fn dir(fs: &mut Btrfs, parent: u64, name: &str) -> u64 {
    fs.create(parent, name, FileKind::Dir, 0o755, 0).unwrap()
}

fn contents(fs: &mut Btrfs, ino: u64) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn names(fs: &mut Btrfs, d: u64) -> Vec<String> {
    let mut v: Vec<String> = fs.readdir(d).unwrap().into_iter().map(|e| e.name).collect();
    v.sort();
    v
}

#[test]
fn a_directory_with_entries_is_not_removed_and_an_empty_one_is() {
    let (mut fs, img) = fresh("rmdir");
    let root = fs.root_ino();
    let d = dir(&mut fs, root, "d");
    file(&mut fs, d, "inside", b"kept");

    // rmdir(2) of a directory with something in it is ENOTEMPTY, and the
    // directory and what is in it stay.
    assert_eq!(fs.unlink(root, "d"), Err(Error::NotEmpty));
    assert_eq!(fs.lookup(root, "d").unwrap(), d);
    let inside = fs.lookup(d, "inside").unwrap();
    assert_eq!(contents(&mut fs, inside), b"kept");

    // Emptied, it goes, and so does its inode.
    fs.unlink(d, "inside").unwrap();
    fs.unlink(root, "d").unwrap();
    assert_eq!(fs.lookup(root, "d"), Err(Error::NotFound));
    assert!(
        fs.stat(d).is_err(),
        "the removed directory's inode is still there"
    );
    assert!(names(&mut fs, root).is_empty());

    fs.sync().unwrap();
    drop(fs);
    check(&img);
    let mut fs = remount(&img);
    assert_eq!(fs.lookup(root, "d"), Err(Error::NotFound));
}

#[test]
fn a_hard_link_keeps_the_data_until_its_last_name_goes() {
    let (mut fs, img) = fresh("link");
    let root = fs.root_ino();
    let sub = dir(&mut fs, root, "sub");
    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
    let ino = file(&mut fs, root, "a", &data);

    fs.link(sub, "b", ino).unwrap();
    assert_eq!(fs.lookup(sub, "b").unwrap(), ino);
    assert_eq!(fs.stat(ino).unwrap().nlink, 2);
    assert_eq!(fs.stat(ino).unwrap().ctime, (1_700_000_100, 0));

    // link(2) refuses a directory and a name that is taken.
    assert_eq!(fs.link(root, "sub2", sub), Err(Error::IsDir));
    assert_eq!(fs.link(sub, "b", ino), Err(Error::Exists));
    assert_eq!(fs.stat(ino).unwrap().nlink, 2, "a refused link counted");

    // The first name goes; the data stays under the other.
    fs.unlink(root, "a").unwrap();
    assert_eq!(fs.lookup(root, "a"), Err(Error::NotFound));
    assert_eq!(fs.stat(ino).unwrap().nlink, 1);
    assert_eq!(contents(&mut fs, ino), data);

    fs.sync().unwrap();
    drop(fs);
    check(&img);

    // Across a mount, and then the last name frees it.
    let mut fs = remount(&img);
    assert_eq!(fs.lookup(sub, "b").unwrap(), ino);
    assert_eq!(contents(&mut fs, ino), data);
    let used = fs.fsinfo().bytes_used;
    fs.unlink(sub, "b").unwrap();
    assert!(fs.stat(ino).is_err(), "the inode outlived its last name");
    assert!(
        fs.fsinfo().bytes_used < used,
        "freeing the last name freed no space"
    );
    fs.sync().unwrap();
    drop(fs);
    check(&img);
}

#[test]
fn rename_onto_an_existing_name_follows_rename_2() {
    let (mut fs, img) = fresh("rename");
    let root = fs.root_ino();

    // Onto a file: the destination is replaced and its data freed.
    let a = file(&mut fs, root, "a", b"new contents");
    let b = file(&mut fs, root, "b", b"old contents");
    fs.rename(root, "a", root, "b").unwrap();
    assert_eq!(fs.lookup(root, "a"), Err(Error::NotFound));
    assert_eq!(fs.lookup(root, "b").unwrap(), a);
    assert!(
        fs.stat(b).is_err(),
        "the replaced file's inode is still there"
    );

    // Onto a file another name still holds: that name keeps it.
    let c = file(&mut fs, root, "c", b"c");
    let keep = file(&mut fs, root, "keep", b"held twice");
    fs.link(root, "alias", keep).unwrap();
    fs.rename(root, "c", root, "alias").unwrap();
    assert_eq!(fs.lookup(root, "alias").unwrap(), c);
    assert_eq!(fs.stat(keep).unwrap().nlink, 1);
    assert_eq!(contents(&mut fs, keep), b"held twice");

    // Onto another name of the same file: nothing happens, both stay.
    fs.link(root, "keep2", keep).unwrap();
    fs.rename(root, "keep", root, "keep2").unwrap();
    assert_eq!(fs.lookup(root, "keep").unwrap(), keep);
    assert_eq!(fs.lookup(root, "keep2").unwrap(), keep);
    assert_eq!(fs.stat(keep).unwrap().nlink, 2);

    // Onto itself: nothing happens.
    fs.rename(root, "b", root, "b").unwrap();
    assert_eq!(fs.lookup(root, "b").unwrap(), a);

    // Onto a directory with entries: ENOTEMPTY, and neither side moves.
    let full = dir(&mut fs, root, "full");
    file(&mut fs, full, "x", b"x");
    let mover = dir(&mut fs, root, "mover");
    assert_eq!(fs.rename(root, "mover", root, "full"), Err(Error::NotEmpty));
    assert_eq!(fs.lookup(root, "mover").unwrap(), mover);
    assert_eq!(fs.lookup(root, "full").unwrap(), full);

    // Onto an empty directory: replaced.
    let empty = dir(&mut fs, root, "empty");
    fs.rename(root, "mover", root, "empty").unwrap();
    assert_eq!(fs.lookup(root, "empty").unwrap(), mover);
    assert!(fs.stat(empty).is_err());

    // Into another directory, with a new name, and its parent follows.
    fs.rename(root, "b", full, "moved").unwrap();
    assert_eq!(fs.lookup(full, "moved").unwrap(), a);
    assert_eq!(fs.parent(a).unwrap(), full);

    // A name that is not there.
    assert_eq!(fs.rename(root, "nope", root, "z"), Err(Error::NotFound));

    fs.sync().unwrap();
    drop(fs);
    check(&img);
    let mut fs = remount(&img);
    assert_eq!(
        names(&mut fs, root),
        ["alias", "empty", "full", "keep", "keep2"]
    );
    assert_eq!(names(&mut fs, full), ["moved", "x"]);
    assert_eq!(contents(&mut fs, a), b"new contents");
}

#[test]
fn readdir_reports_every_kind_it_was_given() {
    let (mut fs, img) = fresh("kinds");
    let root = fs.root_ino();
    let made = [
        ("reg", FileKind::Regular, 0o100644u32, 0u64),
        ("dir", FileKind::Dir, 0o040755, 0),
        ("chr", FileKind::CharDevice, 0o020600, (1 << 8) | 3),
        ("blk", FileKind::BlockDevice, 0o060600, (8 << 8) | 1),
        ("fifo", FileKind::Fifo, 0o010600, 0),
        ("sock", FileKind::Socket, 0o140600, 0),
    ];
    for (name, kind, mode, rdev) in made {
        fs.create(root, name, kind, mode, rdev).unwrap();
    }
    fs.symlink(root, "link", b"reg").unwrap();

    fs.sync().unwrap();
    drop(fs);
    check(&img);
    let mut fs = remount(&img);

    let mut seen: Vec<(String, FileKind)> = fs
        .readdir(root)
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.kind))
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want: Vec<(String, FileKind)> = made
        .iter()
        .map(|(n, k, _, _)| (n.to_string(), *k))
        .collect();
    want.push(("link".into(), FileKind::Symlink));
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(seen, want);

    // The device number of each device node survives the mount.
    for (name, kind, _, rdev) in made {
        let ino = fs.lookup(root, name).unwrap();
        let st = fs.stat(ino).unwrap();
        assert_eq!(st.kind, kind, "{name}");
        if matches!(kind, FileKind::CharDevice | FileKind::BlockDevice) {
            assert_eq!(st.rdev, rdev, "{name}");
        }
    }
    let l = fs.lookup(root, "link").unwrap();
    assert_eq!(fs.read_link(l).unwrap(), b"reg");
}

#[test]
fn calls_on_the_wrong_kind_of_inode_are_refused() {
    let (mut fs, img) = fresh("refusals");
    let root = fs.root_ino();
    let f = file(&mut fs, root, "f", b"data");

    // A file is not a directory.
    assert_eq!(fs.lookup(f, "x"), Err(Error::NotDir));
    assert_eq!(fs.readdir(f).map(|_| ()), Err(Error::NotDir));
    assert_eq!(
        fs.create(f, "x", FileKind::Regular, 0o644, 0),
        Err(Error::NotDir)
    );
    // A taken name.
    assert_eq!(
        fs.create(root, "f", FileKind::Regular, 0o644, 0),
        Err(Error::Exists)
    );
    // Symlink calls on a file, and an empty target.
    assert_eq!(fs.read_link(f), Err(Error::Invalid));
    assert_eq!(fs.write_symlink(f, 0, b"x"), Err(Error::Invalid));
    assert_eq!(fs.symlink(root, "empty", b""), Err(Error::Invalid));
    assert_eq!(fs.lookup(root, "empty"), Err(Error::NotFound));
    // A target past what fits inline in one tree block.
    let long = vec![b'a'; fs.nodesize()];
    assert_eq!(fs.symlink(root, "long", &long), Err(Error::Invalid));
    // Data calls on a directory.
    let d = dir(&mut fs, root, "d");
    assert_eq!(fs.write(d, 0, b"x"), Err(Error::IsDir));
    assert_eq!(fs.read(d, 0, &mut [0u8; 4]), Err(Error::IsDir));
    // Removing what is not there.
    assert_eq!(fs.unlink(root, "nope"), Err(Error::NotFound));

    // None of the refusals left anything behind.
    assert_eq!(names(&mut fs, root), ["d", "f"]);
    assert_eq!(contents(&mut fs, f), b"data");
    fs.sync().unwrap();
    drop(fs);
    check(&img);
}

#[test]
fn the_label_reads_back_and_the_root_is_its_own_parent() {
    let (mut fs, _img) = fresh("label");
    assert_eq!(fs.label(), "names");
    let root = fs.root_ino();
    assert_eq!(fs.parent(root).unwrap(), root);
    assert_eq!(fs.stat(root).unwrap().kind, FileKind::Dir);
}

#[test]
fn a_mount_dropped_between_syncs_comes_back_as_the_last_sync_left_it() {
    let (mut fs, img) = fresh("powercut");
    let root = fs.root_ino();
    fs.set_commit_interval(u32::MAX);
    fs.prepare_writes().unwrap();

    let before: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let kept = file(&mut fs, root, "kept", &before);
    let d = dir(&mut fs, root, "dir");
    file(&mut fs, d, "inner", b"inner");
    fs.sync().unwrap();

    // Plenty of changes after the sync, past the default interval of 32
    // that would have written tree blocks: new files, an append, a rename,
    // a removal, a link. Not an overwrite: files are made NODATACOW, as
    // Linux's are with `chattr +C`, so an overwrite of written blocks goes
    // to the device in place at once and is not undone by a cut (the served
    // btrfs's redo log replays it anyway).
    for i in 0..48 {
        file(&mut fs, root, &format!("lost-{i:02}"), &[i as u8; 3000]);
    }
    fs.write(kept, before.len() as u64, &[0xee; 9000]).unwrap();
    fs.rename(d, "inner", root, "moved").unwrap();
    fs.unlink(root, "dir").unwrap();
    let moved = fs.lookup(root, "moved").unwrap();
    fs.link(root, "moved2", moved).unwrap();

    // The power goes: no sync, the mount is just gone.
    drop(fs);
    check(&img);

    let mut fs = remount(&img);
    assert_eq!(names(&mut fs, root), ["dir", "kept"]);
    assert_eq!(contents(&mut fs, kept), before);
    let d = fs.lookup(root, "dir").unwrap();
    let inner = fs.lookup(d, "inner").unwrap();
    assert_eq!(contents(&mut fs, inner), b"inner");
    assert_eq!(fs.stat(inner).unwrap().nlink, 1);

    // And the mount that came back writes on, as a fresh one would.
    file(&mut fs, root, "after", b"after the cut");
    fs.sync().unwrap();
    drop(fs);
    check(&img);
    let mut fs = remount(&img);
    let after = fs.lookup(root, "after").unwrap();
    assert_eq!(contents(&mut fs, after), b"after the cut");
}
