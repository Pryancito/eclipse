//! A volume made by the btrfs code's own `mkfs`, in memory, served as the
//! kernel asks for it -- and served again by a server that comes after one
//! that died.

use super::*;
use std::io;
use std::sync::Mutex as StdMutex;

/// A device in memory that two servers, one after the other, share.
#[derive(Clone)]
struct Platter(Arc<StdMutex<Vec<u8>>>);

impl Blocks for Platter {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.lock().unwrap().read_at(offset, buf)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write_at(offset, buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl btrfs::BlockDevice for Platter {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> btrfs::Result<()> {
        let v = self.0.lock().unwrap();
        buf.copy_from_slice(&v[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> btrfs::Result<()> {
        let mut v = self.0.lock().unwrap();
        v[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn sync(&self) -> btrfs::Result<()> {
        Ok(())
    }

    fn size(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
}

const LEN: usize = 256 << 20;

fn platter() -> Platter {
    let platter = Platter(Arc::new(StdMutex::new(vec![0u8; LEN])));
    let opts = btrfs::mkfs::MkfsOptions {
        label: "served".into(),
        fsid: [1; 16],
        chunk_uuid: [2; 16],
        dev_uuid: [3; 16],
        subvol_uuid: [4; 16],
        now: (1_700_000_000, 0),
    };
    btrfs::mkfs::format(&platter, &opts).unwrap();
    platter
}

/// A page of memory, aligned as a mapping's.
#[derive(Clone)]
#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// The memory a mount holds, for one server and then the next.
struct Held(Vec<Page>);

impl Held {
    fn new() -> Self {
        Held(vec![Page([0; 4096]); KEPT_LEN.div_ceil(4096)])
    }

    fn memory(&mut self) -> Memory {
        // SAFETY: KEPT_LEN bytes from a page's start are this test's, used
        // by one volume at a time.
        unsafe { Memory::at(self.0.as_mut_ptr().cast::<u8>()) }
    }
}

fn open(platter: &Platter, held: &mut Held) -> Volume<Platter> {
    Volume::open(platter.clone(), LEN as u64, held.memory(), false).unwrap()
}

/// The names and modes of a listing.
fn listing(v: &mut Volume<Platter>, path: &str) -> Vec<(String, u32, u64)> {
    let mut out = vec![0u8; 64 * 1024];
    let mut all = Vec::new();
    let mut next = 0;
    while next != fatserve::wire::LIST_END {
        let (count, _, after) = v.list(path, next, &mut out).unwrap();
        let mut at = 0;
        for _ in 0..count {
            let rec_len = u16::from_ne_bytes([out[at], out[at + 1]]) as usize;
            let name_len = u16::from_ne_bytes([out[at + 2], out[at + 3]]) as usize;
            let mode = u32::from_ne_bytes(out[at + 4..at + 8].try_into().unwrap());
            let size = u64::from_ne_bytes(out[at + 8..at + 16].try_into().unwrap());
            let name = std::str::from_utf8(&out[at + RECORD_HEAD..at + RECORD_HEAD + name_len]);
            all.push((name.unwrap().to_string(), mode, size));
            at += rec_len;
        }
        next = after;
    }
    all.sort();
    all
}

fn read_all(v: &mut Volume<Platter>, path: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = v.read(path, done as u64, &mut out[done..]).unwrap();
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    out
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn what_is_made_and_written_lists_and_reads_back() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    let mut out = vec![0u8; 4096];
    v.create("docs", 0o040750, &mut out).unwrap();
    v.create("docs/a.txt", 0o100640, &mut out).unwrap();
    let data = bytes(300_000, 1);
    // A window at a time, as the kernel sends it.
    for (i, piece) in data.chunks(64 * 1024).enumerate() {
        assert_eq!(
            v.write("docs/a.txt", (i * 64 * 1024) as u64, piece),
            Ok(piece.len())
        );
    }
    assert_eq!(read_all(&mut v, "docs/a.txt", 400_000), data);
    assert_eq!(
        listing(&mut v, ""),
        [("docs".to_string(), 0o040750, 20)]
            .into_iter()
            .map(|(n, m, _)| (n, m, listing_size(&mut v, "docs")))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        listing(&mut v, "docs"),
        [("a.txt".to_string(), 0o100640, 300_000)]
    );
}

/// The size btrfs gives the directory `path` (its names, twice).
fn listing_size(v: &mut Volume<Platter>, path: &str) -> u64 {
    let mut out = vec![0u8; 4096];
    v.stat(path, &mut out).unwrap();
    u64::from_ne_bytes(out[8..16].try_into().unwrap())
}

#[test]
fn a_symbolic_link_is_made_its_target_written_and_read() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    let mut out = vec![0u8; 4096];
    v.create("lnk", 0o120777, &mut out).unwrap();
    v.write("lnk", 0, b"../elsewhere").unwrap();
    assert_eq!(read_all(&mut v, "lnk", 100), b"../elsewhere");
    assert_eq!(listing(&mut v, ""), [("lnk".to_string(), 0o120777, 12)]);
}

#[test]
fn the_owner_permissions_and_time_are_kept() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    let mut out = vec![0u8; 4096];
    v.create("f", 0o100644, &mut out).unwrap();
    v.set_attr("f", 1000, 100, 0o4711).unwrap();
    v.set_time("f", 1_234_567).unwrap();
    let len = v.stat("f", &mut out).unwrap();
    assert_eq!(len, record_len("f"));
    let u32_at = |i: usize| u32::from_ne_bytes(out[i..i + 4].try_into().unwrap());
    assert_eq!(u32_at(4), 0o104711);
    assert_eq!((u32_at(40), u32_at(44), u32_at(48)), (1000, 100, 1));
    assert_eq!(
        i64::from_ne_bytes(out[16..24].try_into().unwrap()),
        1_234_567
    );
}

#[test]
fn renamed_and_removed_are_where_they_were_asked_to_be() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    let mut out = vec![0u8; 4096];
    v.create("d", 0o040755, &mut out).unwrap();
    v.create("d/x", 0o100644, &mut out).unwrap();
    v.write("d/x", 0, b"x").unwrap();
    v.rename("d", "e", &mut out).unwrap();
    assert_eq!(read_all(&mut v, "e/x", 10), b"x");
    assert_eq!(v.read("d/x", 0, &mut out), Err(libc::ENOENT));
    assert_eq!(v.remove("e"), Err(libc::ENOTEMPTY));
    v.remove("e/x").unwrap();
    v.remove("e").unwrap();
    assert!(listing(&mut v, "").is_empty());
}

#[test]
fn a_dead_servers_changes_are_the_next_ones_and_so_is_what_it_kept_of_a_file() {
    let (platter, mut held) = (platter(), Held::new());
    let data = bytes(200_000, 7);
    {
        let mut v = open(&platter, &mut held);
        let mut out = vec![0u8; 4096];
        v.create("keep", 0o040700, &mut out).unwrap();
        v.create("keep/big", 0o100600, &mut out).unwrap();
        for (i, piece) in data.chunks(50_000).enumerate() {
            v.write("keep/big", (i * 50_000) as u64, piece).unwrap();
        }
        v.create("lnk", 0o120777, &mut out).unwrap();
        v.write("lnk", 0, b"keep/big").unwrap();
        // Some of the file is still only in the tail.
        v.write("keep/big", 200_000, b"end").unwrap();
        assert!(v.tail.kept().is_some());
        // Dies: nothing is flushed, nothing reaches the device.
    }
    let mut v = open(&platter, &mut held);
    assert!(v.adopted());
    let mut want = data.clone();
    want.extend_from_slice(b"end");
    assert_eq!(read_all(&mut v, "keep/big", 300_000), want);
    assert_eq!(read_all(&mut v, "lnk", 100), b"keep/big");
    // And once it has flushed, a volume with nothing kept has it all.
    v.sync().unwrap();
    drop(v);
    let mut fresh = Volume::open(platter.clone(), LEN as u64, Memory::private(), true).unwrap();
    assert_eq!(read_all(&mut fresh, "keep/big", 300_000), want);
}

#[test]
fn the_device_has_nothing_of_it_until_the_flush() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    v.flush().unwrap();
    let before = platter.0.lock().unwrap().clone();
    let mut out = vec![0u8; 4096];
    v.create("new", 0o100644, &mut out).unwrap();
    v.write("new", 0, b"hello").unwrap();
    assert!(v.is_dirty());
    assert!(*platter.0.lock().unwrap() == before);
    v.flush().unwrap();
    assert!(!v.is_dirty());
    let mut fresh = Volume::open(platter.clone(), LEN as u64, Memory::private(), true).unwrap();
    assert_eq!(read_all(&mut fresh, "new", 10), b"hello");
}

#[test]
fn a_read_only_volume_changes_nothing() {
    let platter = platter();
    let mut v = Volume::open(platter.clone(), LEN as u64, Memory::private(), true).unwrap();
    let mut out = vec![0u8; 4096];
    assert_eq!(v.create("f", 0o100644, &mut out), Err(libc::EROFS));
    assert_eq!(v.write("f", 0, b"x"), Err(libc::EROFS));
}

#[test]
fn a_change_writes_what_the_disks_journal_holds() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    let mut out = vec![0u8; 4096];
    v.create("d", 0o040755, &mut out).unwrap();
    for i in 0..400 {
        let path = format!("d/file-{i}");
        v.create(&path, 0o100644, &mut out).unwrap();
        v.write(&path, 0, &bytes(5000, i as u8)).unwrap();
        if i % 3 == 0 {
            v.rename(&path, &format!("d/moved-{i}"), &mut out).unwrap();
        }
        if i % 7 == 0 {
            v.remove(&format!("d/moved-{}", i - i % 3)).ok();
        }
    }
    eprintln!(
        "checkpoints {} largest {} bytes",
        v.checkpoints, v.largest_checkpoint
    );
    assert!(v.largest_checkpoint <= (fatserve::disk::GROUP_MAX * 512) as u64);
}

/// A copy of the device and of the kept memory, as they are now.
type Snapshot = (Vec<u8>, Vec<Page>);

#[test]
fn a_server_that_dies_anywhere_after_a_change_leaves_it_made() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    let mut out = vec![0u8; 4096];
    // Two directories far apart in the tree, so that moving a file from
    // one to the other changes several of its blocks.
    v.create("a", 0o040755, &mut out).unwrap();
    for i in 0..300 {
        v.create(&format!("a/f{i}"), 0o100644, &mut out).unwrap();
    }
    v.create("z", 0o040755, &mut out).unwrap();
    v.write("a/f1", 0, b"one").unwrap();
    v.flush().unwrap();
    let snapshots: Arc<StdMutex<Vec<Snapshot>>> = Arc::default();
    let (shots, dev, mem) = (snapshots.clone(), platter.clone(), held.0.as_ptr() as usize);
    let words = held.0.len();
    let snap = move || {
        // SAFETY: the test's memory, read while the volume writes nothing
        // into it (it is about to write the device).
        let copy = unsafe { std::slice::from_raw_parts(mem as *const Page, words) }.to_vec();
        shots
            .lock()
            .unwrap()
            .push((dev.0.lock().unwrap().clone(), copy));
    };
    v.rename("a/f1", "z/renamed", &mut out).unwrap();
    // The rename is answered: a server dying now, or at any write of the
    // trees that follows, leaves it made.
    snap();
    *v.before_write.lock().unwrap() = Some(Box::new(snap));
    v.flush().unwrap();
    *v.before_write.lock().unwrap() = None;
    let snapshots = core::mem::take(&mut *snapshots.lock().unwrap());
    assert!(snapshots.len() > 3, "{} writes", snapshots.len());
    for (device, memory) in snapshots {
        let platter = Platter(Arc::new(StdMutex::new(device)));
        let mut held = Held(memory);
        let mut next = open(&platter, &mut held);
        assert_eq!(listing(&mut next, "z").len(), 1);
        assert_eq!(read_all(&mut next, "z/renamed", 10), b"one");
    }
}

#[test]
fn the_changes_since_the_trees_were_written_are_made_again_by_the_next_server() {
    let (platter, mut held) = (platter(), Held::new());
    {
        let mut v = open(&platter, &mut held);
        let mut out = vec![0u8; 4096];
        // Fewer changes than a writing of the trees waits for, of each kind.
        v.create("d", 0o040750, &mut out).unwrap();
        v.create("d/x", 0o100600, &mut out).unwrap();
        v.write("d/x", 0, b"first").unwrap();
        v.create("d/y", 0o100644, &mut out).unwrap();
        v.write("d/y", 10, b"scattered").unwrap();
        v.write("d/y", 0, b"front").unwrap();
        v.set_attr("d/x", 5, 6, 0o640).unwrap();
        v.set_time("d/x", 1_000_000).unwrap();
        v.rename("d/x", "d/z", &mut out).unwrap();
        v.create("d/gone", 0o100644, &mut out).unwrap();
        v.remove("d/gone").unwrap();
        v.truncate("d/y", 12).unwrap();
        v.create("d/l", 0o120777, &mut out).unwrap();
        v.write("d/l", 0, b"z").unwrap();
        assert!(v.log.records() > 0 && v.log.records() < CHECKPOINT_EVERY);
    }
    let mut v = open(&platter, &mut held);
    assert_eq!(
        listing(&mut v, "d")
            .into_iter()
            .map(|e| (e.0, e.1))
            .collect::<Vec<_>>(),
        [
            ("l".into(), 0o120777),
            ("y".into(), 0o100644),
            ("z".into(), 0o100640)
        ]
    );
    assert_eq!(read_all(&mut v, "d/z", 10), b"first");
    let mut y = b"front".to_vec();
    y.extend_from_slice(&[0; 5]);
    y.extend_from_slice(b"sc");
    assert_eq!(read_all(&mut v, "d/y", 100), y);
    assert_eq!(read_all(&mut v, "d/l", 10), b"z");
    let mut out = vec![0u8; 4096];
    v.stat("d/z", &mut out).unwrap();
    let u32_at = |i: usize| u32::from_ne_bytes(out[i..i + 4].try_into().unwrap());
    assert_eq!((u32_at(40), u32_at(44)), (5, 6));
    assert_eq!(
        i64::from_ne_bytes(out[16..24].try_into().unwrap()),
        1_000_000
    );
    // And a third server, after the second wrote the trees, has the same.
    drop(v);
    let mut v = open(&platter, &mut held);
    assert_eq!(read_all(&mut v, "d/z", 10), b"first");
    assert_eq!(listing(&mut v, "d").len(), 3);
}

#[test]
#[ignore]
fn profile_writes() {
    let (platter, mut held) = (platter(), Held::new());
    let mut v = open(&platter, &mut held);
    let mut out = vec![0u8; 4096];
    v.create("big", 0o100644, &mut out).unwrap();
    let piece = bytes(64 * 1024, 3);
    let t = std::time::Instant::now();
    for i in 0..32 {
        v.write("big", i * 65536, &piece).unwrap();
    }
    v.flush().unwrap();
    eprintln!("64k new: {:?} per write", t.elapsed() / 32);
    let t = std::time::Instant::now();
    for i in 0..32 {
        v.write("big", i * 65536, &piece).unwrap();
    }
    v.flush().unwrap();
    eprintln!("64k over: {:?} per write", t.elapsed() / 32);
    let small = bytes(4096, 5);
    v.create("b4", 0o100644, &mut out).unwrap();
    let t = std::time::Instant::now();
    for i in 0..512 {
        v.write("b4", i * 4096, &small).unwrap();
    }
    v.flush().unwrap();
    eprintln!("4k new: {:?} per write", t.elapsed() / 512);
    let t = std::time::Instant::now();
    let mut buf = vec![0u8; 65536];
    for i in 0..32 {
        v.read("big", i * 65536, &mut buf).unwrap();
    }
    eprintln!("64k read: {:?} per read", t.elapsed() / 32);
    let t = std::time::Instant::now();
    for i in 0..200 {
        let p = format!("s{i}");
        v.create(&p, 0o100644, &mut out).unwrap();
        v.write(&p, 0, &small).unwrap();
    }
    v.flush().unwrap();
    eprintln!(
        "create+4k: {:?} each, checkpoints {}",
        t.elapsed() / 200,
        v.checkpoints
    );
}
