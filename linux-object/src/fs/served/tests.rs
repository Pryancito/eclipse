//! The kernel's side against a server that is a thread: what is asked, what
//! is kept, and what a server that lies or dies gets.

extern crate std;

use super::wire::{
    self, DATA_AREA, LIST_END, MODE_DIR, MSG, NAME_AREA, OP_LIST, OP_READ, OP_STATFS, RECORD_HEAD,
    SLOTS, SLOT_LEN,
};
use super::*;
use alloc::vec;
use core::sync::atomic::AtomicUsize;
use std::thread;
use zircon_object::ZxError;

/// The bytes of the files the fake server has: byte `i` is `i % 251`.
fn content(offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len).map(|i| (i % 251) as u8).collect()
}

const BIG: usize = 200_000;

/// The entries of each directory the fake server has: `(name, mode, size)`.
fn tree(path: &str) -> Option<Vec<(String, u32, u64)>> {
    match path {
        "" => Some(vec![
            (String::from("Readme.TXT"), 0o100644, 10_000),
            (String::from("docs"), MODE_DIR | 0o755, 0),
            (String::from("big.bin"), 0o100644, BIG as u64),
            (String::from("evil"), MODE_DIR | 0o755, 0),
            (String::from("gone.txt"), 0o100644, 5),
        ]),
        "docs" => Some(
            (0..300)
                .map(|i| (alloc::format!("file-{i:03}"), 0o100644, i))
                .collect(),
        ),
        // A name that would lead out of the mount.
        "evil" => Some(vec![(String::from("../escape"), 0o100644, 1)]),
        _ => None,
    }
}

fn record(name: &str, mode: u32, size: u64) -> Vec<u8> {
    let rec_len = (RECORD_HEAD + name.len()).next_multiple_of(8);
    let mut r = vec![0u8; rec_len];
    r[0..2].copy_from_slice(&(rec_len as u16).to_ne_bytes());
    r[2..4].copy_from_slice(&(name.len() as u16).to_ne_bytes());
    r[4..8].copy_from_slice(&mode.to_ne_bytes());
    r[8..16].copy_from_slice(&size.to_ne_bytes());
    r[16..24].copy_from_slice(&1_700_000_000i64.to_ne_bytes());
    r[RECORD_HEAD..RECORD_HEAD + name.len()].copy_from_slice(name.as_bytes());
    r
}

/// A mount of the fake server's tree, and how many requests of each kind it
/// has answered.
struct Mount {
    fs: Arc<ServedFs>,
    link: Arc<Link>,
    lists: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    server: Option<thread::JoinHandle<ZxError>>,
}

impl Mount {
    fn new() -> Self {
        let (ours, servers) = Fifo::create(SLOTS, MSG);
        let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
        let link = Arc::new(Link::new(ours, window.clone()).unwrap());
        let fs = ServedFs::new(link.clone(), true);
        fs.mounted.store(true, Ordering::Release);
        let lists = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let (l, r) = (lists.clone(), reads.clone());
        let server = thread::spawn(move || loop {
            let mut m = [0u8; MSG];
            match servers.read(MSG, &mut m, 1) {
                Ok(_) => {}
                Err(ZxError::SHOULD_WAIT) => {
                    thread::yield_now();
                    continue;
                }
                // The kernel's end is gone: the mount is.
                Err(e) => return e,
            }
            let op = u32::from_ne_bytes([m[0], m[1], m[2], m[3]]);
            let request = wire::decode(&m);
            let names = u32::from_ne_bytes([m[20], m[21], m[22], m[23]]) as usize;
            let slot = (request.reqid as usize & (SLOTS - 1)) * SLOT_LEN;
            let mut path = vec![0u8; names];
            window.read(slot, &mut path).unwrap();
            let path = core::str::from_utf8(&path).unwrap();
            let (status, a, data): (i64, u64, Vec<u8>) = match op {
                OP_LIST => {
                    l.fetch_add(1, Ordering::Relaxed);
                    match tree(path) {
                        None => (-2, 0, Vec::new()),
                        Some(entries) => {
                            // A hundred to a reply, so that a long listing
                            // takes several.
                            let from = request.a as usize;
                            let upto = (from + 100).min(entries.len());
                            let data: Vec<u8> = entries[from..upto]
                                .iter()
                                .flat_map(|(n, m, s)| record(n, *m, *s))
                                .collect();
                            let next = if upto == entries.len() {
                                LIST_END
                            } else {
                                upto as u64
                            };
                            ((upto - from) as i64, next, data)
                        }
                    }
                }
                OP_READ if path == "gone.txt" => (-2, 0, Vec::new()),
                OP_READ => {
                    r.fetch_add(1, Ordering::Relaxed);
                    let size = if path == "big.bin" { BIG } else { 10_000 };
                    let from = (request.a as usize).min(size);
                    let n = (request.b as usize).min(size - from).min(DATA_AREA);
                    (n as i64, 0, content(from, n))
                }
                OP_STATFS => {
                    let numbers = [1024u64, 5000, 1200, 255, 0x4d44];
                    let data = numbers.iter().flat_map(|n| n.to_ne_bytes()).collect();
                    (0, 0, data)
                }
                _ => (-38, 0, Vec::new()),
            };
            window.write(slot + NAME_AREA, &data).unwrap();
            m[8..16].copy_from_slice(&a.to_ne_bytes());
            m[16..20].copy_from_slice(&(data.len() as u32).to_ne_bytes());
            m[24..32].copy_from_slice(&status.to_ne_bytes());
            servers.write(MSG, &m, 1).unwrap();
        });
        Mount {
            fs,
            link,
            lists,
            reads,
            server: Some(server),
        }
    }

    fn root(&self) -> Arc<dyn INode> {
        self.fs.root_inode()
    }

    /// Unmount, and what the server's end of the FIFO then said.
    fn unmount(mut self) -> ZxError {
        self.link.close();
        self.server.take().unwrap().join().unwrap()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.link.close();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

#[test]
fn a_directory_is_what_the_server_listed_and_is_asked_for_once() {
    let mount = Mount::new();
    let root = mount.root();
    assert_eq!(root.get_entry(0).unwrap(), ".");
    assert_eq!(root.get_entry(1).unwrap(), "..");
    assert_eq!(root.get_entry(2).unwrap(), "Readme.TXT");
    assert_eq!(root.get_entry(3).unwrap(), "docs");
    assert_eq!(root.get_entry(7), Err(FsError::EntryNotFound));
    let readme = root.find("Readme.TXT").unwrap();
    let meta = readme.metadata().unwrap();
    assert_eq!((meta.size, meta.type_), (10_000, FileType::File));
    assert_eq!(meta.mtime.sec, 1_700_000_000);
    assert_eq!(meta.blk_size, 1024);
    assert_eq!(root.find("nothing").err(), Some(FsError::EntryNotFound));
    // All of that was one listing.
    assert_eq!(mount.lists.load(Ordering::Relaxed), 1);
}

#[test]
fn a_name_matches_whatever_its_case_and_keeps_the_directorys_spelling() {
    let mount = Mount::new();
    let root = mount.root();
    let a = root.find("readme.txt").unwrap().metadata().unwrap();
    let b = root.find("Readme.TXT").unwrap().metadata().unwrap();
    // One file, however it was asked for: the same inode number.
    assert_eq!(a.inode, b.inode);
    assert_eq!(a.dev, b.dev);
}

#[test]
fn a_listing_that_takes_several_replies_is_whole() {
    let mount = Mount::new();
    let docs = mount.root().find("docs").unwrap();
    assert_eq!(docs.metadata().unwrap().type_, FileType::Dir);
    let (meta, name) = docs.get_entry_with_metadata(2 + 299).unwrap();
    assert_eq!((name.as_str(), meta.size), ("file-299", 299));
    assert_eq!(docs.get_entry(2 + 300), Err(FsError::EntryNotFound));
    // The top once, `docs` in three replies.
    assert_eq!(mount.lists.load(Ordering::Relaxed), 4);
    // `..` of a directory is the directory above, by number too.
    let up = docs.find("..").unwrap().metadata().unwrap();
    assert_eq!(up.inode, mount.root().metadata().unwrap().inode);
    let (dotdot, _) = docs.get_entry_with_metadata(1).unwrap();
    assert_eq!(dotdot.inode, up.inode);
}

#[test]
fn a_file_reads_what_the_server_has_a_window_at_a_time() {
    let mount = Mount::new();
    let big = mount.root().find("big.bin").unwrap();
    let mut buf = vec![0u8; 150_000];
    assert_eq!(big.read_at(60_000, &mut buf), Ok(140_000));
    assert_eq!(buf[..140_000], content(60_000, 140_000)[..]);
    // 140000 bytes are three windows of 64 KiB.
    assert_eq!(mount.reads.load(Ordering::Relaxed), 3);
    assert_eq!(big.read_at(BIG, &mut buf), Ok(0));
    assert_eq!(big.read_at(BIG + 5, &mut buf), Ok(0));
    assert_eq!(mount.reads.load(Ordering::Relaxed), 3);
    assert_eq!(mount.root().read_at(0, &mut buf), Err(FsError::IsDir));
    assert_eq!(big.write_at(0, b"x"), Err(FsError::ReadOnly));
}

#[test]
fn the_servers_errno_is_the_error() {
    let mount = Mount::new();
    let gone = mount.root().find("gone.txt").unwrap();
    assert_eq!(gone.read_at(0, &mut [0u8; 8]), Err(FsError::EntryNotFound));
}

#[test]
fn a_listing_with_a_name_that_leads_out_is_refused_whole() {
    let mount = Mount::new();
    let evil = mount.root().find("evil").unwrap();
    assert_eq!(evil.get_entry(2), Err(FsError::DeviceError));
    assert!(evil.find("../escape").is_err());
    assert!(evil.find("escape").is_err());
}

#[test]
fn the_numbers_are_the_servers() {
    let mount = Mount::new();
    let info = mount.fs.info();
    assert_eq!((info.bsize, info.blocks, info.bfree), (1024, 5000, 1200));
    assert_eq!((info.namemax, info.ftype), (255, 0x4d44));
}

#[test]
fn a_filesystem_being_mounted_is_asked_nothing() {
    let (ours, _servers) = Fifo::create(SLOTS, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let fs = ServedFs::new(Arc::new(Link::new(ours, window).unwrap()), false);
    // Nobody answers on the other end: these return at once all the same.
    assert_eq!(fs.info().bsize, 4096);
    let meta = fs.root_inode().metadata().unwrap();
    assert_eq!((meta.type_, meta.blk_size), (FileType::Dir, 4096));
}

#[test]
fn what_was_listed_outlives_the_server_and_the_rest_is_an_error() {
    let mount = Mount::new();
    let root = mount.root();
    let readme = root.find("Readme.TXT").unwrap();
    let fs = mount.fs.clone();
    // The mount goes: the server sees its FIFO closed.
    assert_eq!(mount.unmount(), ZxError::PEER_CLOSED);
    assert_eq!(readme.metadata().unwrap().size, 10_000);
    assert_eq!(root.get_entry(3).unwrap(), "docs");
    assert_eq!(readme.read_at(0, &mut [0u8; 8]), Err(FsError::DeviceError));
    assert!(root.find("docs").unwrap().get_entry(2).is_err());
    drop(fs);
}

#[test]
fn a_server_that_dies_is_an_error_and_not_a_wait() {
    let (ours, servers) = Fifo::create(SLOTS, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let fs = ServedFs::new(Arc::new(Link::new(ours, window).unwrap()), false);
    fs.mounted.store(true, Ordering::Release);
    drop(servers);
    assert_eq!(fs.root_inode().get_entry(2), Err(FsError::DeviceError));
    assert_eq!(fs.root_inode().get_entry(2), Err(FsError::DeviceError));
}

#[test]
fn a_window_too_short_for_a_slot_is_not_a_link() {
    let (ours, _servers) = Fifo::create(SLOTS, MSG);
    assert!(Link::new(ours, VmObject::new_paged(4)).is_err());
    let (ours, _servers) = Fifo::create(SLOTS, MSG + 8);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    assert!(Link::new(ours, window).is_err());
}

#[test]
fn an_unmount_closes_the_mounts_at_and_under_the_place() {
    let link_at = |at: &str| {
        let (ours, servers) = Fifo::create(SLOTS, MSG);
        let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
        let link = Arc::new(Link::new(ours, window).unwrap());
        MOUNTS.lock().push((String::from(at), link.clone()));
        (link, servers)
    };
    let (a, _a_end) = link_at("/served-test/a");
    let (under, _under_end) = link_at("/served-test/a/deep/er");
    let (beside, _beside_end) = link_at("/served-test/ab");
    unmounted("/served-test/a");
    assert!(a.is_dead() && under.is_dead());
    assert!(!beside.is_dead());
    unmounted("/served-test/ab");
    assert!(beside.is_dead());
}
