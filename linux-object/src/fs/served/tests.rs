//! The kernel's side against a server that is a thread: what is asked, what
//! is kept, and what a server that lies or dies gets.

extern crate std;

use super::wire::{self, MSG, NAME_AREA, RECORD_HEAD, SLOTS, SLOT_LEN};
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

/// What the fake server has: each path from the top, whether it is a
/// directory, and a file's bytes, in the order they were made.
type Tree = Vec<(String, bool, Vec<u8>)>;

/// The most bytes `full.bin` takes: the fake volume is full after them.
const ROOM: usize = 100_000;

fn tree() -> Tree {
    let mut tree = vec![
        (String::from("Readme.TXT"), false, content(0, 10_000)),
        (String::from("docs"), true, Vec::new()),
        (String::from("big.bin"), false, content(0, BIG)),
        (String::from("evil"), true, Vec::new()),
        (String::from("gone.txt"), false, content(0, 5)),
        (String::from("full.bin"), false, Vec::new()),
    ];
    tree.extend((0..300).map(|i| (alloc::format!("docs/file-{i:03}"), false, content(0, i))));
    tree
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

fn record_of(path: &str, is_dir: bool, bytes: &[u8]) -> Vec<u8> {
    let mode = if is_dir { MODE_DIR | 0o755 } else { 0o100644 };
    record(split_path(path).1, mode, bytes.len() as u64)
}

/// One request to the fake server: `(status, a, data)`.
fn serve(
    tree: &mut Tree,
    op: u32,
    names: &str,
    a: u64,
    b: usize,
    data: &[u8],
) -> (i64, u64, Vec<u8>) {
    const OK: (i64, u64, Vec<u8>) = (0, 0, Vec::new());
    let errno = |e: i64| (-e, 0, Vec::new());
    let (path, to) = names.split_once('\0').unwrap_or((names, ""));
    let at = tree.iter().position(|(p, _, _)| p == path);
    let is_dir =
        |tree: &Tree, path: &str| path.is_empty() || tree.iter().any(|(p, d, _)| p == path && *d);
    match op {
        // A name that would lead out of the mount.
        OP_LIST if path == "evil" => (1, LIST_END, record("../escape", 0o100644, 1)),
        OP_LIST if !is_dir(tree, path) => errno(2),
        OP_LIST => {
            let entries: Vec<_> = tree
                .iter()
                .filter(|(p, _, _)| split_path(p).0 == path)
                .collect();
            // A hundred to a reply, so that a long listing takes several.
            let from = (a as usize).min(entries.len());
            let upto = (from + 100).min(entries.len());
            let data = entries[from..upto]
                .iter()
                .flat_map(|(p, d, bytes)| record_of(p, *d, bytes))
                .collect();
            let next = if upto == entries.len() {
                LIST_END
            } else {
                upto as u64
            };
            ((upto - from) as i64, next, data)
        }
        OP_STATFS => {
            let numbers = [1024u64, 5000, 1200 - tree.len() as u64, 255, 0x4d44];
            (0, 0, numbers.iter().flat_map(|n| n.to_ne_bytes()).collect())
        }
        OP_SYNC => OK,
        OP_CREATE if at.is_some() => errno(17),
        OP_CREATE if !is_dir(tree, split_path(path).0) => errno(2),
        OP_CREATE => {
            tree.push((String::from(path), a == MODE_DIR as u64, Vec::new()));
            (1, 0, record_of(path, a == MODE_DIR as u64, &[]))
        }
        _ if at.is_none() || path == "gone.txt" => errno(2),
        OP_READ => {
            let bytes = &tree[at.unwrap()].2;
            let from = (a as usize).min(bytes.len());
            let n = b.min(bytes.len() - from).min(DATA_AREA);
            (n as i64, 0, bytes[from..from + n].to_vec())
        }
        OP_WRITE => {
            let bytes = &mut tree[at.unwrap()].2;
            let room = if path == "full.bin" {
                ROOM.saturating_sub(a as usize)
            } else {
                usize::MAX
            };
            let n = b.min(room);
            if n == 0 {
                return errno(28);
            }
            let end = a as usize + n;
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[a as usize..end].copy_from_slice(&data[..n]);
            (n as i64, 0, Vec::new())
        }
        OP_TRUNCATE => {
            tree[at.unwrap()].2.resize(a as usize, 0);
            OK
        }
        OP_SETTIME => OK,
        OP_REMOVE if tree.iter().any(|(p, _, _)| split_path(p).0 == path) => errno(39),
        OP_REMOVE => {
            tree.remove(at.unwrap());
            OK
        }
        OP_RENAME => {
            tree.retain(|(p, _, _)| p != to);
            for (p, _, _) in tree.iter_mut() {
                if let Some(rest) = under(p, path) {
                    *p = alloc::format!("{to}{rest}");
                }
            }
            let (p, d, bytes) = tree.iter().find(|(p, _, _)| p == to).unwrap();
            (1, 0, record_of(p, *d, bytes))
        }
        _ => errno(38),
    }
}

/// How a fake server ends: on the first request `op` it gets, after doing
/// it or before, it goes without answering.
#[derive(Clone, Copy)]
struct DiesOn {
    op: u32,
    done_first: bool,
}

/// A fake server of `tree` as a thread, and the kernel's link to it.
fn server(
    tree: &Arc<std::sync::Mutex<Tree>>,
    asked: &Arc<[AtomicUsize; 16]>,
    dies_on: Option<DiesOn>,
) -> (Arc<Link>, thread::JoinHandle<ZxError>) {
    let (ours, servers) = Fifo::create(SLOTS, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let link = Arc::new(Link::new(ours, window.clone()).unwrap());
    let (t, n) = (tree.clone(), asked.clone());
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
        let mut sent = vec![
            0u8;
            if op == OP_WRITE {
                request.b as usize
            } else {
                0
            }
        ];
        window.read(slot + NAME_AREA, &mut sent).unwrap();
        let dies = dies_on.filter(|d| d.op == op);
        if dies.is_some_and(|d| !d.done_first) {
            return ZxError::CANCELED;
        }
        n[op as usize].fetch_add(1, Ordering::Relaxed);
        let (status, a, data) = serve(
            &mut t.lock().unwrap(),
            op,
            path,
            request.a,
            request.b as usize,
            &sent,
        );
        if dies.is_some() {
            return ZxError::CANCELED;
        }
        window.write(slot + NAME_AREA, &data).unwrap();
        m[8..16].copy_from_slice(&a.to_ne_bytes());
        m[16..20].copy_from_slice(&(data.len() as u32).to_ne_bytes());
        m[24..32].copy_from_slice(&status.to_ne_bytes());
        servers.write(MSG, &m, 1).unwrap();
    });
    (link, server)
}

/// A mount of the fake server's tree, and how many requests of each kind it
/// has answered.
struct Mount {
    fs: Arc<ServedFs>,
    tree: Arc<std::sync::Mutex<Tree>>,
    asked: Arc<[AtomicUsize; 16]>,
    servers: Vec<thread::JoinHandle<ZxError>>,
}

impl Mount {
    fn new() -> Self {
        Self::with(false, None)
    }

    fn with(read_only: bool, dies_on: Option<DiesOn>) -> Self {
        let tree = Arc::new(std::sync::Mutex::new(tree()));
        let asked = Arc::new([const { AtomicUsize::new(0) }; 16]);
        let (link, server) = server(&tree, &asked, dies_on);
        let fs = ServedFs::new(link, true, read_only);
        fs.mounted.store(true, Ordering::Release);
        Mount {
            fs,
            tree,
            asked,
            servers: vec![server],
        }
    }

    /// A mount whose first server goes on its first request `op`.
    fn dying_on(op: u32, done_first: bool) -> Self {
        Self::with(false, Some(DiesOn { op, done_first }))
    }

    /// Another server of the same tree takes the mount over: whether it
    /// was let.
    fn next_server(&mut self) -> bool {
        let (link, server) = server(&self.tree, &self.asked, None);
        let taken = self.fs.take_over(link.clone()).is_some();
        if !taken {
            link.close();
        }
        self.servers.push(server);
        taken
    }

    /// The server of the moment goes, as a process that is killed does.
    fn kill_server(&mut self) {
        self.fs.link().close();
        for server in self.servers.drain(..) {
            let _ = server.join();
        }
    }

    /// The same mount, taken over by another server `after_ms` from now.
    fn next_server_in(mut self, after_ms: u64) -> thread::JoinHandle<Self> {
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(after_ms));
            // The one before is seen to be gone first: a request may be
            // what finds out.
            while !self.next_server() {
                thread::yield_now();
            }
            self
        })
    }

    /// How many requests `op` the server has answered.
    fn asked(&self, op: u32) -> usize {
        self.asked[op as usize].load(Ordering::Relaxed)
    }

    /// The bytes the server has for the file `path`.
    fn on_server(&self, path: &str) -> Option<Vec<u8>> {
        let tree = self.tree.lock().unwrap();
        tree.iter()
            .find(|(p, _, _)| p == path)
            .map(|(_, _, bytes)| bytes.clone())
    }

    /// The names an open directory reads, from the place `at` on.
    fn names_from(dir: &Arc<dyn INode>, mut at: usize) -> Vec<String> {
        let mut names = Vec::new();
        while let Some(Ok((_, name, next))) = entry_at(&**dir, at) {
            names.push(name);
            at = next;
        }
        names
    }

    fn root(&self) -> Arc<dyn INode> {
        self.fs.root_inode()
    }

    /// Unmount, and what the server's end of the FIFO then said.
    fn unmount(mut self) -> ZxError {
        self.fs.mounted.store(false, Ordering::Release);
        self.fs.link().close();
        self.servers.pop().unwrap().join().unwrap()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.fs.mounted.store(false, Ordering::Release);
        self.fs.link().close();
        for server in self.servers.drain(..) {
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
    assert_eq!(root.get_entry(8), Err(FsError::EntryNotFound));
    let readme = root.find("Readme.TXT").unwrap();
    let meta = readme.metadata().unwrap();
    assert_eq!((meta.size, meta.type_), (10_000, FileType::File));
    assert_eq!(meta.mtime.sec, 1_700_000_000);
    assert_eq!(meta.blk_size, 1024);
    assert_eq!(root.find("nothing").err(), Some(FsError::EntryNotFound));
    // All of that was one listing.
    assert_eq!(mount.asked(OP_LIST), 1);
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
    assert_eq!(mount.asked(OP_LIST), 4);
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
    assert_eq!(mount.asked(OP_READ), 3);
    assert_eq!(big.read_at(BIG, &mut buf), Ok(0));
    assert_eq!(big.read_at(BIG + 5, &mut buf), Ok(0));
    assert_eq!(mount.asked(OP_READ), 3);
    assert_eq!(mount.root().read_at(0, &mut buf), Err(FsError::IsDir));
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
    assert_eq!((info.bsize, info.blocks, info.bfree), (1024, 5000, 894));
    assert_eq!((info.namemax, info.ftype), (255, 0x4d44));
}

#[test]
fn a_filesystem_being_mounted_is_asked_nothing() {
    let (ours, _servers) = Fifo::create(SLOTS, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let fs = ServedFs::new(Arc::new(Link::new(ours, window).unwrap()), false, false);
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
fn a_server_that_dies_with_none_after_it_is_an_error_once_the_wait_is_over() {
    let (ours, servers) = Fifo::create(SLOTS, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let fs = ServedFs::new(Arc::new(Link::new(ours, window).unwrap()), false, false);
    fs.mounted.store(true, Ordering::Release);
    fs.grace_ms.store(30, Ordering::Relaxed);
    drop(servers);
    let before = std::time::Instant::now();
    assert_eq!(fs.root_inode().get_entry(2), Err(FsError::DeviceError));
    assert!(before.elapsed() >= std::time::Duration::from_millis(30));
    // And at once from then on.
    let before = std::time::Instant::now();
    assert_eq!(fs.root_inode().get_entry(2), Err(FsError::DeviceError));
    assert!(before.elapsed() < std::time::Duration::from_millis(30));
}

#[test]
fn a_request_with_no_server_waits_for_the_next_one() {
    let mut mount = Mount::new();
    let readme = mount.root().find("Readme.TXT").unwrap();
    mount.kill_server();
    let coming = mount.next_server_in(50);
    // Asked while there is no server: answered by the one that comes.
    let mut buf = [0u8; 100];
    assert_eq!(readme.read_at(5000, &mut buf), Ok(100));
    assert_eq!(buf[..], content(5000, 100)[..]);
    let mount = coming.join().unwrap();
    // What was listed before is still what the mount has.
    assert_eq!(mount.asked(OP_LIST), 1);
    assert_eq!(
        mount.root().find("docs").unwrap().get_entry(2).unwrap(),
        "file-000"
    );
}

#[test]
fn a_mount_with_a_server_is_not_taken_over() {
    let mut mount = Mount::new();
    assert!(!mount.next_server());
    assert_eq!(mount.root().get_entry(2).unwrap(), "Readme.TXT");
    mount.kill_server();
    assert!(mount.next_server());
    assert_eq!(
        mount.root().find("docs").unwrap().get_entry(2).unwrap(),
        "file-000"
    );
}

#[test]
fn a_server_that_comes_after_the_wait_still_finds_the_mount() {
    let mut mount = Mount::new();
    mount.fs.grace_ms.store(0, Ordering::Relaxed);
    let root = mount.root();
    mount.kill_server();
    thread::sleep(std::time::Duration::from_millis(5));
    assert!(root.find("docs").is_err());
    assert_eq!(mount.fs.sync(), Ok(()));
    assert!(mount.next_server());
    assert_eq!(root.find("docs").unwrap().get_entry(2).unwrap(), "file-000");
}

#[test]
fn a_write_the_server_died_under_is_written_by_the_next() {
    for done_first in [false, true] {
        let mount = Mount::dying_on(OP_WRITE, done_first);
        let file = mount.root().find("Readme.TXT").unwrap();
        let coming = mount.next_server_in(20);
        assert_eq!(file.write_at(3, b"written"), Ok(7));
        let mount = coming.join().unwrap();
        assert_eq!(
            mount.on_server("Readme.TXT").unwrap()[..10],
            *b"\x00\x01\x02written"
        );
        assert_eq!(file.metadata().unwrap().size, 10_000);
    }
}

#[test]
fn a_file_the_dead_server_made_is_the_file_that_was_asked_for() {
    for done_first in [false, true] {
        let mount = Mount::dying_on(OP_CREATE, done_first);
        let root = mount.root();
        root.get_entry(2).unwrap();
        let coming = mount.next_server_in(20);
        // The second server says it exists, when the first made it: that
        // is the answer to the first request, not a refusal.
        let made = root.create("new.txt", FileType::File, 0o644).unwrap();
        let mount = coming.join().unwrap();
        assert_eq!(mount.on_server("new.txt").unwrap(), b"");
        assert_eq!(made.write_at(0, b"abc"), Ok(3));
        assert_eq!(mount.on_server("new.txt").unwrap(), b"abc");
        // And one that is there is still refused.
        assert_eq!(
            root.create("new.txt", FileType::File, 0o644).err(),
            Some(FsError::EntryExist)
        );
    }
}

#[test]
fn a_file_the_dead_server_removed_is_removed() {
    for done_first in [false, true] {
        let mount = Mount::dying_on(OP_REMOVE, done_first);
        let root = mount.root();
        let coming = mount.next_server_in(20);
        assert_eq!(root.unlink("Readme.TXT"), Ok(()));
        let mount = coming.join().unwrap();
        assert!(mount.on_server("Readme.TXT").is_none());
        assert!(root.find("Readme.TXT").is_err());
        assert_eq!(root.unlink("Readme.TXT"), Err(FsError::EntryNotFound));
    }
}

#[test]
fn a_file_the_dead_server_renamed_is_under_its_new_name() {
    for done_first in [false, true] {
        let mount = Mount::dying_on(OP_RENAME, done_first);
        let root = mount.root();
        let file = root.find("Readme.TXT").unwrap();
        let coming = mount.next_server_in(20);
        assert_eq!(root.move_("Readme.TXT", &root, "here.txt"), Ok(()));
        let mount = coming.join().unwrap();
        assert!(mount.on_server("Readme.TXT").is_none());
        assert_eq!(mount.on_server("here.txt").unwrap(), content(0, 10_000));
        assert!(root.find("Readme.TXT").is_err());
        // The open file is the renamed one.
        assert_eq!(file.write_at(10_000, b"!"), Ok(1));
        assert_eq!(mount.on_server("here.txt").unwrap().len(), 10_001);
    }
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
    let mount_at = |at: &str| {
        let (ours, servers) = Fifo::create(SLOTS, MSG);
        let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
        let link = Arc::new(Link::new(ours, window).unwrap());
        let fs = ServedFs::new(link.clone(), false, false);
        fs.mounted.store(true, Ordering::Release);
        MOUNTS.lock().push((String::from(at), fs));
        (link, servers)
    };
    let (a, _a_end) = mount_at("/served-test/a");
    let (under, _under_end) = mount_at("/served-test/a/deep/er");
    let (beside, _beside_end) = mount_at("/served-test/ab");
    unmounted("/served-test/a");
    assert!(a.is_dead() && under.is_dead());
    assert!(!beside.is_dead());
    unmounted("/served-test/ab");
    assert!(beside.is_dead());
}

#[test]
fn a_write_is_the_servers_when_it_returns_and_the_size_is_the_files() {
    let mount = Mount::new();
    let root = mount.root();
    let readme = root.find("Readme.TXT").unwrap();
    assert_eq!(readme.write_at(9_990, &[7; 30]), Ok(30));
    assert_eq!(mount.on_server("Readme.TXT").unwrap()[9_990..], [7; 30]);
    // Every inode of the file has the new size, and nobody was asked.
    assert_eq!(readme.metadata().unwrap().size, 10_020);
    assert_eq!(
        root.find("readme.txt").unwrap().metadata().unwrap().size,
        10_020
    );
    assert!(readme.metadata().unwrap().mtime.sec > 1_700_000_000);
    assert_eq!(mount.asked(OP_LIST), 1);
    let mut back = [0u8; 40];
    assert_eq!(readme.read_at(9_985, &mut back), Ok(35));
    assert_eq!(back[5..35], [7; 30]);
    // A long write goes a window at a time.
    let long = content(3, 150_000);
    assert_eq!(readme.write_at(0, &long), Ok(150_000));
    assert_eq!(mount.asked(OP_WRITE), 1 + 3);
    assert_eq!(mount.on_server("Readme.TXT").unwrap(), long);
    assert_eq!(readme.write_at(0, &[]), Ok(0));
    assert_eq!(root.write_at(0, b"x"), Err(FsError::IsDir));
}

#[test]
fn a_volume_that_fills_up_is_a_short_write_and_then_no_space() {
    let mount = Mount::new();
    let full = mount.root().find("full.bin").unwrap();
    let piece = vec![1u8; 150_000];
    assert_eq!(full.write_at(0, &piece), Ok(ROOM));
    assert_eq!(full.metadata().unwrap().size, ROOM);
    assert_eq!(full.write_at(ROOM, &piece), Err(FsError::NoDeviceSpace));
    assert_eq!(full.metadata().unwrap().size, ROOM);
}

#[test]
fn a_file_is_cut_and_grown() {
    let mount = Mount::new();
    let big = mount.root().find("big.bin").unwrap();
    big.resize(1000).unwrap();
    assert_eq!(big.metadata().unwrap().size, 1000);
    assert_eq!(mount.on_server("big.bin").unwrap(), content(0, 1000));
    big.resize(3000).unwrap();
    let mut back = vec![1u8; 4000];
    assert_eq!(big.read_at(0, &mut back), Ok(3000));
    assert!(back[1000..3000].iter().all(|b| *b == 0));
    assert_eq!(mount.root().resize(0), Err(FsError::IsDir));
}

#[test]
fn what_is_made_is_in_the_listing_without_asking_for_it_again() {
    let mount = Mount::new();
    let root = mount.root();
    let made = root.create("new.txt", FileType::File, 0o644).unwrap();
    assert_eq!(made.metadata().unwrap().type_, FileType::File);
    assert_eq!(made.write_at(0, b"hello"), Ok(5));
    let found = root.find("NEW.TXT").unwrap().metadata().unwrap();
    assert_eq!(
        (found.size, found.inode),
        (5, made.metadata().unwrap().inode)
    );
    let dir = root.create("sub", FileType::Dir, 0o755).unwrap();
    assert_eq!(dir.metadata().unwrap().type_, FileType::Dir);
    // A directory just made is listed, and is empty.
    let inside = dir.create("inside", FileType::File, 0o644).unwrap();
    assert_eq!(inside.write_at(0, b"x"), Ok(1));
    assert_eq!(mount.on_server("sub/inside").unwrap(), b"x");
    assert_eq!(Mount::names_from(&dir, 2), ["inside"]);
    assert_eq!(root.get_entry(2 + 7).unwrap(), "sub");
    // A place left empty is the next one taken.
    root.unlink("big.bin").unwrap();
    root.create("small.bin", FileType::File, 0o644).unwrap();
    assert_eq!(root.get_entry(2 + 2).unwrap(), "small.bin");
    // The top was listed once; `sub` once, empty.
    assert_eq!(mount.asked(OP_LIST), 2);
    // What is there is not made again, and the server is not asked.
    assert_eq!(
        root.create("Docs", FileType::File, 0o644).err(),
        Some(FsError::EntryExist)
    );
    assert_eq!(
        root.create("a/b", FileType::File, 0o644).err(),
        Some(FsError::InvalidParam)
    );
    assert_eq!(
        root.create("link", FileType::SymLink, 0o777).err(),
        Some(FsError::NotSupported)
    );
    assert_eq!(
        made.create("x", FileType::File, 0o644).err(),
        Some(FsError::NotDir)
    );
    assert_eq!(mount.asked(OP_CREATE), 4);
}

#[test]
fn a_directory_read_while_it_is_emptied_misses_nothing() {
    let mount = Mount::new();
    let docs = mount.root().find("docs").unwrap();
    // As `rm -r` does: read some names, remove them, read on.
    let mut at = 0;
    let mut removed = 0;
    loop {
        let mut names = Vec::new();
        while names.len() < 40 {
            match entry_at(&*docs, at) {
                Some(Ok((_, name, next))) => {
                    names.push(name);
                    at = next;
                }
                _ => break,
            }
        }
        if names.is_empty() {
            break;
        }
        for name in names.iter().filter(|n| *n != "." && *n != "..") {
            docs.unlink(name).unwrap();
            removed += 1;
        }
    }
    assert_eq!(removed, 300);
    assert_eq!(Mount::names_from(&docs, 0), [".", ".."]);
    assert_eq!(docs.get_entry(2), Err(FsError::EntryNotFound));
    mount.root().unlink("docs").unwrap();
    assert_eq!(
        mount.root().find("docs").err(),
        Some(FsError::EntryNotFound)
    );
    assert!(mount.on_server("docs/file-000").is_none());
}

#[test]
fn a_removed_file_is_nobodys_and_its_name_is_free() {
    let mount = Mount::new();
    let root = mount.root();
    let readme = root.find("Readme.TXT").unwrap();
    assert_eq!(root.unlink("docs"), Err(FsError::DirNotEmpty));
    assert_eq!(root.unlink("nothing"), Err(FsError::EntryNotFound));
    root.unlink("readme.txt").unwrap();
    assert_eq!(root.find("Readme.TXT").err(), Some(FsError::EntryNotFound));
    // Whoever still holds it is told, and the server is not asked about a
    // path that may be another file's by now.
    assert_eq!(readme.metadata().unwrap().nlinks, 0);
    let asked = (mount.asked(OP_READ), mount.asked(OP_WRITE));
    assert_eq!(
        readme.read_at(0, &mut [0u8; 8]),
        Err(FsError::EntryNotFound)
    );
    assert_eq!(readme.write_at(0, b"x"), Err(FsError::EntryNotFound));
    assert_eq!((mount.asked(OP_READ), mount.asked(OP_WRITE)), asked);
    let again = root.create("Readme.TXT", FileType::File, 0o644).unwrap();
    assert_ne!(
        again.metadata().unwrap().inode,
        readme.metadata().unwrap().inode
    );
    assert_eq!(again.metadata().unwrap().size, 0);
}

#[test]
fn a_renamed_file_is_the_same_file_under_its_new_name() {
    let mount = Mount::new();
    let root = mount.root();
    let docs = root.find("docs").unwrap();
    let readme = root.find("Readme.TXT").unwrap();
    let ino = readme.metadata().unwrap().inode;
    root.move_("Readme.TXT", &docs, "moved.txt").unwrap();
    assert_eq!(root.find("Readme.TXT").err(), Some(FsError::EntryNotFound));
    assert_eq!(
        docs.find("moved.txt").unwrap().metadata().unwrap().inode,
        ino
    );
    // The inode held from before goes on, under the new path.
    assert_eq!(readme.write_at(0, b"still here"), Ok(10));
    assert_eq!(
        mount.on_server("docs/moved.txt").unwrap()[..10],
        *b"still here"
    );
    // Over a file that is there: that one is gone.
    let victim = docs.find("file-007").unwrap();
    docs.move_("moved.txt", &docs, "file-007").unwrap();
    assert_eq!(victim.metadata().unwrap().nlinks, 0);
    assert_eq!(
        victim.read_at(0, &mut [0u8; 8]),
        Err(FsError::EntryNotFound)
    );
    let now = docs.find("file-007").unwrap().metadata().unwrap();
    assert_eq!((now.inode, now.size), (ino, 10_000));
    assert_eq!(Mount::names_from(&docs, 2).len(), 300);
    assert_eq!(
        root.move_("nothing", &docs, "x"),
        Err(FsError::EntryNotFound)
    );
    assert_eq!(root.move_("big.bin", &readme, "x"), Err(FsError::NotDir));
    // To the name it has: nothing to do, and nothing asked.
    let renames = mount.asked(OP_RENAME);
    root.move_("big.bin", &root, "big.bin").unwrap();
    assert_eq!(mount.asked(OP_RENAME), renames);
}

#[test]
fn a_renamed_directory_takes_what_was_listed_below_it() {
    let mount = Mount::new();
    let root = mount.root();
    let docs = root.find("docs").unwrap();
    let file = docs.find("file-100").unwrap();
    let lists = mount.asked(OP_LIST);
    root.move_("docs", &root, "papers").unwrap();
    assert_eq!(root.find("docs").err(), Some(FsError::EntryNotFound));
    // The directory held from before, and the file in it, go on.
    assert_eq!(
        docs.find("file-100").unwrap().metadata().unwrap().inode,
        file.metadata().unwrap().inode
    );
    let mut back = [0u8; 100];
    assert_eq!(file.read_at(0, &mut back), Ok(100));
    assert_eq!(back[..], content(0, 100)[..]);
    let made = docs.create("new", FileType::File, 0o644).unwrap();
    assert_eq!(made.write_at(0, b"x"), Ok(1));
    assert_eq!(mount.on_server("papers/new").unwrap(), b"x");
    let papers = root.find("papers").unwrap();
    assert_eq!(Mount::names_from(&papers, 2).len(), 301);
    // None of it was listed again.
    assert_eq!(mount.asked(OP_LIST), lists);
    // Not into itself.
    assert_eq!(
        root.move_("papers", &papers, "inside"),
        Err(FsError::InvalidParam)
    );
}

#[test]
fn a_read_only_mount_is_asked_for_no_change() {
    let mount = Mount::with(true, None);
    let root = mount.root();
    let readme = root.find("Readme.TXT").unwrap();
    assert_eq!(readme.write_at(0, b"x"), Err(FsError::ReadOnly));
    assert_eq!(readme.resize(0), Err(FsError::ReadOnly));
    assert_eq!(
        root.create("x", FileType::File, 0o644).err(),
        Some(FsError::ReadOnly)
    );
    assert_eq!(root.unlink("Readme.TXT"), Err(FsError::ReadOnly));
    assert_eq!(root.move_("Readme.TXT", &root, "x"), Err(FsError::ReadOnly));
    mount.fs.sync().unwrap();
    let changes = [
        OP_WRITE,
        OP_TRUNCATE,
        OP_CREATE,
        OP_REMOVE,
        OP_RENAME,
        OP_SYNC,
    ];
    assert!(changes.iter().all(|op| mount.asked(*op) == 0));
}

#[test]
fn a_sync_is_the_servers_to_do_and_a_dead_server_has_nothing_to_sync() {
    let mount = Mount::new();
    let readme = mount.root().find("Readme.TXT").unwrap();
    readme.sync_all().unwrap();
    mount.fs.sync().unwrap();
    assert_eq!(mount.asked(OP_SYNC), 2);
    let fs = mount.fs.clone();
    mount.unmount();
    assert_eq!(fs.sync(), Ok(()));
}

#[test]
fn free_space_is_asked_for_again_after_a_change_and_the_block_size_is_not() {
    let mount = Mount::new();
    let root = mount.root();
    assert_eq!(mount.fs.info().bfree, 894);
    root.create("one-more", FileType::File, 0o644).unwrap();
    assert_eq!(root.metadata().unwrap().blk_size, 1024);
    assert_eq!(mount.asked(OP_STATFS), 1);
    assert_eq!(mount.fs.info().bfree, 893);
    assert_eq!(mount.asked(OP_STATFS), 2);
}

#[test]
fn bytes_fetched_while_the_file_was_written_are_not_kept() {
    let mount = Mount::new();
    let readme = mount.root().find("Readme.TXT").unwrap();
    let token = read_token(&*readme).unwrap();
    let mut kept = false;
    assert!(if_unchanged(&*readme, token, || kept = true) && kept);
    // A write between the fetch and the keeping.
    let token = read_token(&*readme).unwrap();
    readme.write_at(0, b"x").unwrap();
    assert!(!if_unchanged(&*readme, token, || panic!("kept")));
    // A cut too.
    let token = read_token(&*readme).unwrap();
    readme.resize(10).unwrap();
    assert!(!if_unchanged(&*readme, token, || panic!("kept")));
    let token = read_token(&*readme).unwrap();
    assert!(if_unchanged(&*readme, token, || ()));
    // While a write is with the server there is nothing to keep.
    let writing = Changing::new(&readme.downcast_ref::<ServedINode>().unwrap().node);
    assert!(read_token(&*readme).is_none());
    drop(writing);
    assert!(read_token(&*readme).is_some());
}

#[test]
fn an_open_file_reads_what_was_written_from_its_pages() {
    use crate::fs::{File, FileLike, OpenFlags};
    use async_std::task::block_on;
    let _registry = crate::fs::file::REGISTRY_TEST_LOCK.lock();
    let mount = Mount::new();
    let inode = mount.root().find("big.bin").unwrap();
    let file = File::new(inode.clone(), OpenFlags::RDWR, String::from("/mnt/big.bin"));
    let read = |at: usize, len: usize| {
        let mut buf = vec![0u8; len];
        let n = block_on(FileLike::read_at(&*file, at as u64, &mut buf)).unwrap();
        buf.truncate(n);
        buf
    };
    // The first window, from the server, once.
    assert_eq!(read(100, 5000), content(100, 5000));
    assert_eq!(read(0, 100), content(0, 100));
    assert_eq!(mount.asked(OP_READ), 1);
    // A write over pages that are kept changes them, and the server.
    assert_eq!(FileLike::write_at(&*file, 4000, &[9; 300]), Ok(300));
    assert_eq!(mount.on_server("big.bin").unwrap()[4000..4300], [9; 300]);
    let back = read(3990, 320);
    assert_eq!(back[..10], content(3990, 10)[..]);
    assert_eq!(back[10..310], [9; 300]);
    assert_eq!(back[310..], content(4300, 10)[..]);
    assert_eq!(mount.asked(OP_READ), 1);
    // A write over pages that are not kept: they come from the server
    // with it when they are read.
    assert_eq!(FileLike::write_at(&*file, 150_000, &[8; 10]), Ok(10));
    assert_eq!(read(150_000, 10), [8; 10]);
    // Past the end: the file is longer, zeros between, to every reader.
    assert_eq!(
        FileLike::write_at(&*file, BIG as u64 + 5000, b"tail"),
        Ok(4)
    );
    assert_eq!(inode.metadata().unwrap().size, BIG + 5004);
    let back = read(BIG - 10, 6000);
    assert_eq!(back.len(), 5014);
    assert_eq!(back[..10], content(BIG - 10, 10)[..]);
    assert!(back[10..5010].iter().all(|b| *b == 0));
    assert_eq!(&back[5010..], b"tail");
    // Cut: what was past the cut is gone from the pages too, and is zeros
    // when the file grows again.
    file.set_len(2000).unwrap();
    assert_eq!(read(1990, 100), content(1990, 10));
    file.set_len(6000).unwrap();
    let back = read(1990, 5000);
    assert_eq!(back.len(), 4010);
    assert!(back[10..].iter().all(|b| *b == 0));
}

#[test]
fn a_removed_files_pages_are_forgotten() {
    use crate::fs::{File, FileLike, OpenFlags};
    use async_std::task::block_on;
    let _registry = crate::fs::file::REGISTRY_TEST_LOCK.lock();
    let mount = Mount::new();
    let root = mount.root();
    let inode = root.find("Readme.TXT").unwrap();
    let file = File::new(
        inode.clone(),
        OpenFlags::RDWR,
        String::from("/mnt/Readme.TXT"),
    );
    let mut buf = [0u8; 100];
    assert_eq!(block_on(FileLike::read_at(&*file, 0, &mut buf)), Ok(100));
    let (kept, _) = crate::fs::file::shared_file_vmo_stats();
    root.unlink("Readme.TXT").unwrap();
    assert_eq!(crate::fs::file::shared_file_vmo_stats().0, kept - 1);
    // The open file has nothing to read, and keeps no pages for it.
    assert!(block_on(FileLike::read_at(&*file, 0, &mut buf)).is_err());
    assert_eq!(crate::fs::file::shared_file_vmo_stats().0, kept - 1);
}
