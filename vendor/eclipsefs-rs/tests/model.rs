#![cfg(feature = "std")]
//! Whole-filesystem model check: long random sequences of every namespace,
//! data and attribute operation run against EclipseFS and against a plain
//! in-memory model of POSIX semantics, and the two must agree after every
//! step. Directories nest, files have hard links, names collide on
//! purpose, renames replace and move subtrees, data goes through every
//! write path (inline, extents, compression, preallocation, holes, reflinks)
//! and the tree is compared in full after remounts, power cuts and in
//! read-only snapshots taken along the way.
//!
//! `EFS_MODEL_SEEDS=n` runs n seeds per test instead of the default few, and
//! `EFS_MODEL_STEPS=n` makes each run longer (`--release` is advisable).

mod common;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use common::*;
use eclipsefs::cowfmt::{
    FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FALLOC_FL_ZERO_RANGE, XATTR_CREATE, XATTR_REPLACE,
};
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, Compression, EclipseFS, Error, FileKind, Profile};

const BS: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Body {
    File(Vec<u8>),
    Dir(BTreeMap<String, u64>),
    Sym(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Node {
    body: Body,
    nlink: u32,
    mode: u32,
    xattrs: BTreeMap<String, Vec<u8>>,
    // Directories only.
    parent: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct Model {
    nodes: BTreeMap<u64, Node>,
}

impl Model {
    fn dir(&self, ino: u64) -> &BTreeMap<String, u64> {
        match &self.nodes[&ino].body {
            Body::Dir(d) => d,
            _ => panic!("{} is not a directory", ino),
        }
    }
    fn dir_mut(&mut self, ino: u64) -> &mut BTreeMap<String, u64> {
        match &mut self.nodes.get_mut(&ino).unwrap().body {
            Body::Dir(d) => d,
            _ => panic!("{} is not a directory", ino),
        }
    }
    fn data_mut(&mut self, ino: u64) -> &mut Vec<u8> {
        match &mut self.nodes.get_mut(&ino).unwrap().body {
            Body::File(d) => d,
            _ => panic!("{} is not a file", ino),
        }
    }
    fn data(&self, ino: u64) -> &Vec<u8> {
        match &self.nodes[&ino].body {
            Body::File(d) => d,
            _ => panic!("{} is not a file", ino),
        }
    }
    fn is_dir(&self, ino: u64) -> bool {
        matches!(self.nodes[&ino].body, Body::Dir(_))
    }
    fn dirs(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|(_, n)| matches!(n.body, Body::Dir(_)))
            .map(|(&i, _)| i)
            .collect()
    }
    fn files(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|(_, n)| matches!(n.body, Body::File(_)))
            .map(|(&i, _)| i)
            .collect()
    }
    /// Whether `a` is `b` or one of its ancestors.
    fn is_ancestor(&self, a: u64, mut b: u64, root: u64) -> bool {
        loop {
            if a == b {
                return true;
            }
            if b == root {
                return false;
            }
            b = self.nodes[&b].parent;
        }
    }
    /// Drop one link to `ino`; frees it with its last one.
    fn drop_link(&mut self, ino: u64) {
        let n = self.nodes.get_mut(&ino).unwrap();
        if matches!(n.body, Body::Dir(_)) {
            self.nodes.remove(&ino);
            return;
        }
        n.nlink -= 1;
        if n.nlink == 0 {
            self.nodes.remove(&ino);
        }
    }
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

/// The same tree read back from a mounted filesystem, in model form (inode
/// numbers as the filesystem reports them), skipping `skip` in the root.
fn read_tree(fs: &mut EclipseFS, root: u64, skip: Option<&str>) -> Model {
    let mut m = Model::default();
    let mut todo = vec![(root, root)];
    while let Some((ino, parent)) = todo.pop() {
        if m.nodes.contains_key(&ino) {
            continue;
        }
        let st = fs.stat(ino).unwrap();
        let mut xattrs = BTreeMap::new();
        for name in fs.list_xattrs(ino).unwrap() {
            let v = fs.get_xattr(ino, &name).unwrap();
            xattrs.insert(name, v);
        }
        let body = match st.kind {
            FileKind::Dir => {
                let mut d = BTreeMap::new();
                for e in fs.readdir(ino).unwrap() {
                    if ino == root && Some(e.name.as_str()) == skip {
                        continue;
                    }
                    assert_eq!(fs.lookup(ino, &e.name).unwrap(), e.ino, "lookup {}", e.name);
                    let kind = fs.stat(e.ino).unwrap().kind;
                    assert_eq!(kind, e.kind, "readdir kind of {}", e.name);
                    d.insert(e.name, e.ino);
                    todo.push((e.ino, ino));
                }
                Body::Dir(d)
            }
            FileKind::Symlink => Body::Sym(fs.read_link(ino).unwrap()),
            FileKind::Regular => Body::File(read_all(fs, ino)),
            k => panic!("unexpected kind {:?}", k),
        };
        if let Body::File(d) = &body {
            assert_eq!(d.len() as u64, st.size);
        }
        let nlink = if st.kind == FileKind::Dir {
            1
        } else {
            st.nlink
        };
        m.nodes.insert(
            ino,
            Node {
                body,
                nlink,
                mode: st.mode & 0o7777,
                xattrs,
                parent: if st.kind == FileKind::Dir { parent } else { 0 },
            },
        );
    }
    m
}

fn assert_same(fs: &mut EclipseFS, model: &Model, root: u64, skip: Option<&str>, what: &str) {
    let got = read_tree(fs, root, skip);
    if got != *model {
        for (ino, n) in &model.nodes {
            match got.nodes.get(ino) {
                None => panic!("{}: inode {} missing: {:?}", what, ino, short(n)),
                Some(g) if g != n => {
                    let at = match (&g.body, &n.body) {
                        (Body::File(a), Body::File(b)) => first_diff(a, b),
                        _ => None,
                    };
                    panic!(
                        "{}: inode {} differs (data at byte {:?})\n  fs:    {:?}\n  model: {:?}",
                        what,
                        ino,
                        at,
                        short(g),
                        short(n)
                    )
                }
                _ => {}
            }
        }
        let extra: Vec<_> = got
            .nodes
            .keys()
            .filter(|k| !model.nodes.contains_key(k))
            .collect();
        panic!("{}: extra inodes {:?}", what, extra);
    }
}

fn short(n: &Node) -> String {
    match &n.body {
        Body::File(d) => format!(
            "file len {} nlink {} mode {:o} xattrs {:?} first-diff-data {:?}",
            d.len(),
            n.nlink,
            n.mode,
            n.xattrs.keys().collect::<Vec<_>>(),
            &d[..d.len().min(16)]
        ),
        other => format!(
            "{:?} nlink {} mode {:o} xattrs {:?}",
            other, n.nlink, n.mode, n.xattrs
        ),
    }
}

/// Compare two file bodies and say where they first differ.
fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    if a.len() != b.len() {
        return Some(
            a.iter()
                .zip(b)
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len())),
        );
    }
    a.iter().zip(b).position(|(x, y)| x != y)
}

struct Run {
    rng: Lcg,
    disks: Vec<Arc<MemDisk>>,
    fs: EclipseFS,
    model: Model,
    root: u64,
    // Snapshots of the root subvolume live under /snaps, which random
    // operations never touch: (snapshot root, model at snapshot time).
    snaps_dir: u64,
    snaps: Vec<(u64, String, Model)>,
    // Fingerprints of every model state since the last sync: what a power
    // cut may leave.
    since_sync: HashSet<u64>,
    fsynced: bool,
    no_snaps: bool,
    seed: u64,
    // Small disk: operations that need space may fail with NoSpace (and
    // must then change nothing); the ones that give space back may not.
    full: bool,
    // Every snapshot is also sent (incrementally from the one before) to
    // this second filesystem: (received root, model).
    recv: Option<(Arc<MemDisk>, EclipseFS, u64)>,
    received: Vec<(u64, Model)>,
    last_sent: Option<u64>,
    // Files fsynced since the last sync, with a hash of what they held.
    fsync_seen: BTreeMap<u64, u64>,
    // Encryption key to add after every mount.
    key: Option<Vec<u8>>,
    step: usize,
}

const NAMES: &[&str] = &[
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "data.bin",
    "ñandú",
    "日本語",
    "x y",
    "-",
    ".hidden",
    "...",
    "a.b.c",
    COLLIDING[0],
    COLLIDING[1],
    COLLIDING[2],
    COLLIDING[3],
];

/// Four names with the same directory hash (CRC32C and CRC32 both), so
/// entries share one hash bucket: see `tests/edge_cases.rs`.
const COLLIDING: [&str; 4] = [
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "caaaccacaaccacccaccaaacacccaacaaacaacaacccaccacaacccaccacacccacccaaaaaaaaaaaaaaaaaaaaaaaaa",
    "ccaacacccacaccaaccacaacccaacaccaaccaccacaaccacccacaaccaccccaaccaacaaaaaaaaaaaaaaaaaaaaaaaa",
    "acaaaccacaaccacccaccaaacacccaacaaacaacaacccaccacaacccaccacacccacccaaaaaaaaaaaaaaaaaaaaaaaa",
];

impl Run {
    fn new(seed: u64, disks: Vec<Arc<MemDisk>>) -> Self {
        let mut fs = open(&disks);
        let root = fs.root_ino();
        let snaps_dir = fs.create(root, "snaps", FileKind::Dir, 0o755, 0).unwrap();
        fs.sync().unwrap();
        let mut model = Model::default();
        let st = fs.stat(root).unwrap();
        model.nodes.insert(
            root,
            Node {
                body: Body::Dir(BTreeMap::new()),
                nlink: 1,
                mode: st.mode & 0o7777,
                xattrs: BTreeMap::new(),
                parent: root,
            },
        );
        let mut since_sync = HashSet::new();
        since_sync.insert(model.fingerprint());
        Run {
            seed,
            rng: Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xEC11_95E5),
            disks,
            fs,
            model,
            root,
            snaps_dir,
            snaps: Vec::new(),
            since_sync,
            fsynced: false,
            no_snaps: false,
            full: false,
            recv: None,
            received: Vec::new(),
            last_sent: None,
            fsync_seen: BTreeMap::new(),
            key: None,
            step: 0,
        }
    }

    fn below(&mut self, n: u64) -> u64 {
        self.rng.below(n.max(1))
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
    fn name(&mut self) -> String {
        match self.below(40) {
            0 => "n".repeat(255),
            1 => format!("long-{}", "z".repeat(200 + self.below(51) as usize)),
            _ => {
                let n = NAMES.len() as u64;
                NAMES[self.below(n) as usize].to_string()
            }
        }
    }
    /// A random entry `(dir, name, ino)` of the tree (outside /snaps).
    fn entry(&mut self) -> Option<(u64, String, u64)> {
        let dirs = self.model.dirs();
        for _ in 0..8 {
            let d = self.pick(&dirs);
            let names: Vec<(String, u64)> = self
                .model
                .dir(d)
                .iter()
                .map(|(n, &i)| (n.clone(), i))
                .collect();
            if !names.is_empty() {
                let (n, i) = names[self.below(names.len() as u64) as usize].clone();
                return Some((d, n, i));
            }
        }
        None
    }
    fn some_dir(&mut self) -> u64 {
        let dirs = self.model.dirs();
        self.pick(&dirs)
    }
    fn some_file(&mut self) -> Option<u64> {
        let f = self.model.files();
        if f.is_empty() {
            None
        } else {
            Some(self.pick(&f))
        }
    }
    fn offset(&mut self, size: usize) -> u64 {
        match self.below(6) {
            0 => 0,
            1 => self.below(size as u64 + 1),
            2 => (self.below(size as u64 / BS as usize as u64 + 2)) * BS,
            3 => size as u64,
            4 => size as u64 + self.below(3 * BS),
            _ => self.below(3 * BS) * self.below(40),
        }
    }
    fn len(&mut self) -> usize {
        match self.below(8) {
            0 => 1 + self.below(64) as usize,
            1 => 1 + self.below(4096) as usize,
            2 => BS as usize,
            3 => 1 + self.below(20_000) as usize,
            4 => 1 + self.below(150_000) as usize,
            5 => 3000 + self.below(2000) as usize, // around the inline limit
            _ => 1 + self.below(9000) as usize,
        }
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let seed = self.rng.next();
        match self.below(4) {
            // Compressible.
            0 => (0..len).map(|i| b"eclipsefs "[i % 10]).collect(),
            1 => vec![(seed & 0xff) as u8; len],
            2 => vec![0; len],
            _ => {
                let mut x = seed | 1;
                (0..len)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        x as u8
                    })
                    .collect()
            }
        }
    }

    fn after_op(&mut self) {
        self.since_sync.insert(self.model.fingerprint());
    }

    fn durable(&mut self) {
        self.since_sync.clear();
        self.fsynced = false;
        self.fsync_seen.clear();
        self.since_sync.insert(self.model.fingerprint());
    }

    fn ctx(&self) -> String {
        format!("seed {} step {}", self.seed, self.step)
    }

    fn step(&mut self) {
        let ctx = self.ctx();
        let op = self.below(100);
        if tracing() {
            eprintln!("{} op {}", self.step, op);
        }
        match op {
            0..=9 => self.op_create(FileKind::Regular),
            10..=13 => self.op_create(FileKind::Dir),
            14 => self.op_symlink(),
            15..=29 => self.op_write(),
            30..=34 => self.op_truncate(),
            35..=41 => self.op_read(),
            42..=45 => self.op_link(),
            46..=51 => self.op_unlink(),
            52..=58 => self.op_rename(),
            59..=62 => self.op_fallocate(),
            63..=66 => self.op_clone(),
            67..=71 => self.op_xattr(),
            72 => self.op_chmod(),
            73 => self.op_compression(),
            74..=75 => self.op_dedupe(),
            76 => {
                self.fs.sync().unwrap();
                self.durable();
            }
            77 => {
                if let Some(f) = self.some_file() {
                    self.fs.fsync(f).unwrap();
                    if tracing() {
                        eprintln!(
                            "{} fsync {} ({} bytes)",
                            self.step,
                            f,
                            self.model.data(f).len()
                        );
                    }
                    self.fsynced = true;
                    let h = hash(self.model.data(f));
                    self.fsync_seen.insert(f, h);
                    if self.below(2) == 0 {
                        self.power_cut();
                    }
                }
            }
            78 => self.fs.drop_caches(),
            79 => self.power_cut(),
            80 => self.op_remount(),
            81 => {
                if self.snaps.len() < 4 {
                    self.op_snapshot();
                }
            }
            82 => self.op_bad_names(),
            83 => self.op_seek(),
            84 => self.op_atomic_write(),
            85 => self.op_defrag(),
            86 if self.below(4) == 0 => self.op_maintenance(),
            87 if self.below(2) == 0 => self.op_delete_snapshot(),
            _ => self.op_write(),
        }
        let _ = ctx;
        if tracing() {
            let i = self.fs.fsinfo();
            eprintln!("{} free {}", self.step, (i.total_bytes - i.bytes_used) / BS);
            if std::env::var_os("EFS_MODEL_CHECK_EVERY").is_some() {
                if let Err(e) = self.fs.check() {
                    panic!("{} check after the operation: {:?}", self.ctx(), e);
                }
            }
        }
        self.after_op();
    }

    fn op_create(&mut self, kind: FileKind) {
        let d = self.some_dir();
        let name = self.name();
        let r = self.fs.create(d, &name, kind, 0o640, 0);
        if tracing() {
            eprintln!("{} create {}/{} {:?} -> {:?}", self.step, d, name, kind, r);
        }
        if self.no_space(&r) {
            return;
        }
        if self.model.dir(d).contains_key(&name) || self.reserved(d, &name) {
            assert_eq!(r, Err(Error::Exists), "{} create {}", self.ctx(), name);
            return;
        }
        let ino = r.unwrap_or_else(|e| panic!("{} create {}: {:?}", self.ctx(), name, e));
        assert!(
            !self.model.nodes.contains_key(&ino),
            "{} inode {} reused while live",
            self.ctx(),
            ino
        );
        let mode = self.fs.stat(ino).unwrap().mode & 0o7777;
        let body = match kind {
            FileKind::Dir => Body::Dir(BTreeMap::new()),
            _ => Body::File(Vec::new()),
        };
        self.model.nodes.insert(
            ino,
            Node {
                body,
                nlink: 1,
                mode,
                xattrs: BTreeMap::new(),
                parent: if kind == FileKind::Dir { d } else { 0 },
            },
        );
        self.model.dir_mut(d).insert(name, ino);
    }

    fn op_symlink(&mut self) {
        let d = self.some_dir();
        let name = self.name();
        let tlen = match self.below(4) {
            0 => 1,
            1 => 4095,
            _ => 1 + self.below(300) as usize,
        };
        let target: Vec<u8> = (0..tlen).map(|i| b'a' + (i % 26) as u8).collect();
        let r = self.fs.symlink(d, &name, &target);
        if tracing() {
            eprintln!("{} symlink {}/{} -> {:?}", self.step, d, name, r);
        }
        if self.no_space(&r) {
            return;
        }
        if self.model.dir(d).contains_key(&name) || self.reserved(d, &name) {
            assert_eq!(r, Err(Error::Exists), "{}", self.ctx());
            return;
        }
        let ino = r.unwrap();
        let mode = self.fs.stat(ino).unwrap().mode & 0o7777;
        self.model.nodes.insert(
            ino,
            Node {
                body: Body::Sym(target),
                nlink: 1,
                mode,
                xattrs: BTreeMap::new(),
                parent: 0,
            },
        );
        self.model.dir_mut(d).insert(name, ino);
    }

    fn op_write(&mut self) {
        let Some(f) = self.some_file() else {
            return self.op_create(FileKind::Regular);
        };
        let size = self.model.data(f).len();
        let off = self.offset(size);
        let len = self.len();
        let data = self.bytes(len);
        // Keep files bounded so a run stays inside the disk.
        if off as usize + len > 2 * 1024 * 1024 {
            return;
        }
        // Writes are short only when the device is full.
        let mut done = 0;
        while done < len {
            let r = self.fs.write(f, off + done as u64, &data[done..]);
            if tracing() {
                eprintln!(
                    "{} write {} {}+{} -> {:?}",
                    self.step,
                    f,
                    off + done as u64,
                    len - done,
                    r
                );
            }
            match r {
                Ok(n) if n > 0 => done += n,
                Ok(_) if self.full => break,
                r if self.no_space(&r) => break,
                r => panic!("{} write {}+{}: {:?}", self.ctx(), off, len, r),
            }
        }
        if done == 0 {
            // Refused: the file is as it was.
            if self.full {
                let got = read_all(&mut self.fs, f);
                if let Some(at) = first_diff(&got, self.model.data(f)) {
                    panic!(
                        "{} write {}+{} refused but changed the file at byte {} (len {} want {})",
                        self.ctx(),
                        off,
                        len,
                        at,
                        got.len(),
                        self.model.data(f).len()
                    );
                }
            }
            return;
        }
        let m = self.model.data_mut(f);
        let end = off as usize + done;
        if m.len() < end {
            m.resize(end, 0);
        }
        m[off as usize..end].copy_from_slice(&data[..done]);
    }

    fn op_atomic_write(&mut self) {
        let Some(f) = self.some_file() else { return };
        let size = self.model.data(f).len();
        let off = self.offset(size);
        let len = 1 + self.below(64 * 1024) as usize;
        let data = self.bytes(len);
        match self.fs.write_atomic(f, off, &data) {
            Ok(n) => {
                assert_eq!(n, len, "{}", self.ctx());
                let m = self.model.data_mut(f);
                let end = off as usize + len;
                if m.len() < end {
                    m.resize(end, 0);
                }
                m[off as usize..end].copy_from_slice(&data);
            }
            Err(Error::Invalid) => assert!(len as u64 > eclipsefs::cow::ATOMIC_WRITE_MAX),
            Err(Error::NoSpace) if self.full => {}
            Err(e) => panic!("{} write_atomic {:?}", self.ctx(), e),
        }
    }

    fn op_truncate(&mut self) {
        let Some(f) = self.some_file() else { return };
        let size = self.model.data(f).len();
        let to = match self.below(5) {
            0 => 0,
            1 => self.below(size as u64 + 1),
            2 => size as u64 + self.below(100_000),
            3 => (self.below(size as u64 / BS + 2)) * BS,
            _ => self.below(4000),
        };
        let r = self.fs.truncate(f, to);
        // Shrinking to the middle of a block rewrites that block (zeros
        // after the new end), which needs a free one.
        if to < size as u64 && (to % BS != 0 || self.no_space_held(&r)) && self.no_space(&r) {
            return;
        }
        if tracing() {
            eprintln!("{} truncate {} {} -> {} {:?}", self.step, f, size, to, r);
        }
        if to > size as u64 && self.no_space(&r) {
            return;
        }
        r.unwrap_or_else(|e| panic!("{} truncate {} -> {}: {:?}", self.ctx(), size, to, e));
        self.model.data_mut(f).resize(to as usize, 0);
    }

    fn op_read(&mut self) {
        let Some(f) = self.some_file() else { return };
        let data = self.model.data(f).clone();
        let off = self.offset(data.len());
        let len = self.len();
        let mut buf = vec![0xEE; len];
        let n = self.fs.read(f, off, &mut buf).unwrap();
        let s = (off as usize).min(data.len());
        let e = (off as usize + len).min(data.len());
        assert_eq!(n, e - s, "{} read length", self.ctx());
        if let Some(at) = first_diff(&buf[..n], &data[s..e]) {
            panic!(
                "{} read of {} at {}+{} differs at byte {}",
                self.ctx(),
                f,
                off,
                len,
                at
            );
        }
        let mut buf2 = vec![0x11; len];
        if let Some(n2) = self.fs.read_shared(f, off, &mut buf2).unwrap() {
            assert_eq!(n2, n, "{} shared read length", self.ctx());
            if let Some(at) = first_diff(&buf2[..n2], &data[s..e]) {
                panic!(
                    "{} shared read of {} at {}+{} differs at byte {}",
                    self.ctx(),
                    f,
                    off,
                    len,
                    at
                );
            }
        }
        if let Some(st) = self.fs.stat_shared(f).unwrap() {
            assert_eq!(
                st.size,
                data.len() as u64,
                "{} shared stat size",
                self.ctx()
            );
        }
    }

    fn op_link(&mut self) {
        let Some((_, _, ino)) = self.entry() else {
            return;
        };
        let d = self.some_dir();
        let name = self.name();
        let r = self.fs.link(d, &name, ino);
        if tracing() {
            eprintln!("{} link {} {}/{} -> {:?}", self.step, ino, d, name, r);
        }
        if self.no_space(&r) {
            return;
        }
        if self.model.is_dir(ino) {
            assert_eq!(r, Err(Error::IsDir), "{}", self.ctx());
        } else if self.model.dir(d).contains_key(&name) || self.reserved(d, &name) {
            assert_eq!(r, Err(Error::Exists), "{}", self.ctx());
        } else {
            r.unwrap_or_else(|e| panic!("{} link: {:?}", self.ctx(), e));
            self.model.nodes.get_mut(&ino).unwrap().nlink += 1;
            self.model.dir_mut(d).insert(name, ino);
        }
    }

    fn op_unlink(&mut self) {
        let (d, name) = if self.below(10) == 0 {
            (self.some_dir(), self.name())
        } else {
            match self.entry() {
                Some((d, n, _)) => (d, n),
                None => return,
            }
        };
        let r = self.fs.unlink(d, &name);
        if tracing() {
            eprintln!("{} unlink {}/{} -> {:?}", self.step, d, name, r);
        }
        if self.reserved(d, &name) || self.no_space_held(&r) {
            return;
        }
        match self.model.dir(d).get(&name).copied() {
            None => assert_eq!(r, Err(Error::NotFound), "{}", self.ctx()),
            Some(ino) if self.model.is_dir(ino) && !self.model.dir(ino).is_empty() => {
                assert_eq!(r, Err(Error::NotEmpty), "{}", self.ctx())
            }
            Some(ino) => {
                r.unwrap_or_else(|e| panic!("{} unlink {}: {:?}", self.ctx(), name, e));
                self.model.dir_mut(d).remove(&name);
                self.model.drop_link(ino);
            }
        }
    }

    fn op_rename(&mut self) {
        let Some((od, on, ino)) = self.entry() else {
            return;
        };
        let nd = self.some_dir();
        let nn = if self.below(3) == 0 {
            // Often onto an existing name.
            let names: Vec<String> = self.model.dir(nd).keys().cloned().collect();
            if names.is_empty() {
                self.name()
            } else {
                self.pick(&names.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                    .to_string()
            }
        } else {
            self.name()
        };
        let r = self.fs.rename(od, &on, nd, &nn);
        if tracing() {
            eprintln!(
                "{} rename {}/{} {}/{} -> {:?}",
                self.step, od, on, nd, nn, r
            );
        }
        let ctx = format!("{} rename {}/{} -> {}/{}", self.ctx(), od, on, nd, nn);
        if self.no_space(&r) {
            return;
        }
        if self.reserved(nd, &nn) {
            assert!(r.is_err(), "{}", ctx);
            return;
        }
        if od == nd && on == nn {
            r.unwrap();
            return;
        }
        let is_dir = self.model.is_dir(ino);
        if is_dir && self.model.is_ancestor(ino, nd, self.root) {
            assert_eq!(r, Err(Error::Invalid), "{}", ctx);
            return;
        }
        if let Some(&ex) = self.model.dir(nd).get(&nn) {
            if ex == ino {
                // Two links to the same file: rename does nothing.
                r.unwrap_or_else(|e| panic!("{}: {:?}", ctx, e));
                return;
            }
            let ex_dir = self.model.is_dir(ex);
            let want = match (is_dir, ex_dir) {
                (true, false) => Some(Error::NotDir),
                (false, true) => Some(Error::IsDir),
                (true, true) if !self.model.dir(ex).is_empty() => Some(Error::NotEmpty),
                _ => None,
            };
            if let Some(w) = want {
                assert_eq!(r, Err(w), "{}", ctx);
                return;
            }
            r.unwrap_or_else(|e| panic!("{}: {:?}", ctx, e));
            self.model.dir_mut(nd).remove(&nn);
            self.model.drop_link(ex);
        } else {
            r.unwrap_or_else(|e| panic!("{}: {:?}", ctx, e));
        }
        self.model.dir_mut(od).remove(&on);
        self.model.dir_mut(nd).insert(nn, ino);
        if is_dir {
            self.model.nodes.get_mut(&ino).unwrap().parent = nd;
        }
    }

    fn op_fallocate(&mut self) {
        let Some(f) = self.some_file() else { return };
        let size = self.model.data(f).len();
        let off = self.offset(size);
        let len = 1 + self.below(200_000);
        if off + len > 2 * 1024 * 1024 {
            return;
        }
        let (mode, kind) = match self.below(4) {
            0 => (0, "alloc"),
            1 => (FALLOC_FL_KEEP_SIZE, "keep"),
            2 => (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, "punch"),
            _ => (FALLOC_FL_ZERO_RANGE, "zero"),
        };
        let r = self.fs.fallocate(f, mode, off, len);
        if tracing() {
            eprintln!(
                "{} fallocate {} {} {}+{} -> {:?}",
                self.step, f, kind, off, len, r
            );
        }
        // Punching zeros the partial blocks at its edges, which takes new
        // ones; whole blocks always go.
        let edges = !off.is_multiple_of(BS) || !(off + len).is_multiple_of(BS);
        if (kind != "punch" || edges || self.no_space_held(&r)) && self.no_space(&r) {
            return;
        }
        r.unwrap_or_else(|e| panic!("{} fallocate {} {}+{}: {:?}", self.ctx(), kind, off, len, e));
        let m = self.model.data_mut(f);
        let end = (off + len) as usize;
        match kind {
            "alloc" | "zero" => {
                if m.len() < end {
                    m.resize(end, 0);
                }
                if kind == "zero" {
                    m[off as usize..end].fill(0);
                }
            }
            "punch" => {
                let e = end.min(m.len());
                if (off as usize) < e {
                    m[off as usize..e].fill(0);
                }
            }
            _ => {}
        }
    }

    fn op_clone(&mut self) {
        let Some(src) = self.some_file() else { return };
        let Some(dst) = self.some_file() else { return };
        let ssize = self.model.data(src).len() as u64;
        let dsize = self.model.data(dst).len() as u64;
        let soff = self.below(ssize / BS + 1) * BS;
        let doff = self.below(dsize / BS + 2) * BS;
        // Whole blocks, or up to the end of the source (0) when that end
        // also covers the end of the destination.
        let to_end = self.below(3) == 0;
        let len = if to_end {
            0
        } else {
            self.below((ssize - soff) / BS + 1) * BS
        };
        let real = if len == 0 { ssize - soff } else { len };
        let r = self.fs.clone_range(src, soff, dst, doff, len);
        if tracing() {
            eprintln!(
                "{} clone {}@{} {}@{} {} -> {:?}",
                self.step, src, soff, dst, doff, len, r
            );
        }
        if self.key.is_some() && real > 0 {
            // Refused, after the arguments are checked.
            assert!(
                matches!(r, Err(Error::Unsupported(_) | Error::Invalid)),
                "{} {:?}",
                self.ctx(),
                r
            );
            return;
        }
        let ctx = format!(
            "{} clone {}@{} -> {}@{} len {} (src {} dst {})",
            self.ctx(),
            src,
            soff,
            dst,
            doff,
            len,
            ssize,
            dsize
        );
        if real == 0 {
            assert_eq!(r, Ok(0), "{}", ctx);
            return;
        }
        if src == dst && soff < doff + real && doff < soff + real {
            assert_eq!(r, Err(Error::Invalid), "{}", ctx);
            return;
        }
        // An unaligned tail is only cloned onto the end of the destination.
        let unaligned_tail = real % BS != 0;
        match r {
            Ok(n) => {
                assert_eq!(n, real, "{}", ctx);
                let s = self.model.data(src)[soff as usize..(soff + real) as usize].to_vec();
                let m = self.model.data_mut(dst);
                let end = (doff + real) as usize;
                if m.len() < end {
                    m.resize(end, 0);
                }
                m[doff as usize..end].copy_from_slice(&s);
            }
            Err(Error::Invalid) if unaligned_tail && doff + real < dsize => {}
            Err(Error::NoSpace) if self.full => {}
            Err(e) => panic!("{}: {:?}", ctx, e),
        }
    }

    fn op_dedupe(&mut self) {
        let Some(src) = self.some_file() else { return };
        let Some(dst) = self.some_file() else { return };
        // Make the ranges equal first half of the time.
        let ssize = self.model.data(src).len() as u64;
        if ssize < BS {
            return;
        }
        let soff = self.below(ssize / BS) * BS;
        let len = (1 + self.below((ssize - soff) / BS)) * BS;
        let doff = self.below(4) * BS;
        if src == dst {
            return;
        }
        if self.below(2) == 0 {
            let s = self.model.data(src)[soff as usize..(soff + len) as usize].to_vec();
            if self.full {
                // Not worth tracking a partial copy here.
                return;
            }
            write_all(&mut self.fs, dst, doff, &s);
            let m = self.model.data_mut(dst);
            let end = (doff + len) as usize;
            if m.len() < end {
                m.resize(end, 0);
            }
            m[doff as usize..end].copy_from_slice(&s);
        }
        let dsize = self.model.data(dst).len() as u64;
        let r = self.fs.dedupe_range(src, soff, dst, doff, len);
        if self.key.is_some() {
            // Ciphertext is per file: nothing to share (ranges that
            // differ are told apart before that).
            assert!(
                matches!(r, Err(Error::Unsupported(_)) | Ok(0)),
                "{} {:?}",
                self.ctx(),
                r
            );
            return;
        }
        if doff + len > dsize {
            // Out of range on the destination: refused or 0.
            assert!(
                matches!(r, Err(Error::Invalid) | Ok(0)),
                "{} {:?}",
                self.ctx(),
                r
            );
            return;
        }
        let same = self.model.data(src)[soff as usize..(soff + len) as usize]
            == self.model.data(dst)[doff as usize..(doff + len) as usize];
        match r {
            Ok(n) => assert_eq!(n, if same { len } else { 0 }, "{} dedupe", self.ctx()),
            Err(Error::NoSpace) if self.full => {}
            Err(e) => panic!("{} dedupe {:?}", self.ctx(), e),
        }
    }

    fn op_xattr(&mut self) {
        let Some((_, _, ino)) = self.entry() else {
            return;
        };
        let names = [
            "user.a",
            "user.mime_type",
            "trusted.x",
            "security.selinux",
            "user.big",
        ];
        let name = self.pick(&names);
        let has = self.model.nodes[&ino].xattrs.get(name).cloned();
        match self.below(5) {
            0 | 1 => {
                let len = if name == "user.big" {
                    self.below(65537) as usize
                } else {
                    self.below(200) as usize
                };
                let v = self.bytes(len);
                let flags = self.pick(&[0, XATTR_CREATE, XATTR_REPLACE]);
                let r = self.fs.set_xattr(ino, name, &v, flags);
                if self.no_space(&r) {
                    return;
                }
                match (&has, flags) {
                    (Some(_), XATTR_CREATE) => assert_eq!(r, Err(Error::Exists), "{}", self.ctx()),
                    (None, XATTR_REPLACE) => assert_eq!(r, Err(Error::NotFound), "{}", self.ctx()),
                    _ if r == Err(Error::NoSpace) => {
                        // An attribute (with any others of the same name
                        // hash) is one tree item: at most about half a
                        // 4 KiB leaf.
                        assert!(
                            name.len() + len > eclipsefs::btree::MAX_ITEM - 64,
                            "{} setxattr {} NoSpace at {} bytes",
                            self.ctx(),
                            name,
                            len
                        );
                    }
                    _ => {
                        r.unwrap_or_else(|e| {
                            panic!("{} setxattr {} len {}: {:?}", self.ctx(), name, len, e)
                        });
                        self.model
                            .nodes
                            .get_mut(&ino)
                            .unwrap()
                            .xattrs
                            .insert(name.to_string(), v);
                    }
                }
            }
            2 => {
                let r = self.fs.remove_xattr(ino, name);
                if self.no_space_held(&r) {
                    return;
                }
                if has.is_some() {
                    r.unwrap();
                    self.model.nodes.get_mut(&ino).unwrap().xattrs.remove(name);
                } else {
                    assert_eq!(r, Err(Error::NotFound), "{}", self.ctx());
                }
            }
            _ => {
                let r = self.fs.get_xattr(ino, name);
                match has {
                    Some(v) => assert_eq!(r.unwrap(), v, "{}", self.ctx()),
                    None => assert_eq!(r, Err(Error::NotFound), "{}", self.ctx()),
                }
                let mut want: Vec<String> = self.model.nodes[&ino].xattrs.keys().cloned().collect();
                want.sort();
                assert_eq!(self.fs.list_xattrs(ino).unwrap(), want, "{}", self.ctx());
            }
        }
    }

    fn op_chmod(&mut self) {
        let Some((_, _, ino)) = self.entry() else {
            return;
        };
        if !matches!(self.model.nodes[&ino].body, Body::Sym(_)) {
            let mode = self.below(0o7777 + 1) as u32;
            let r = self.fs.set_attr(ino, Some(mode), None, None, None, None);
            if self.no_space(&r) {
                return;
            }
            r.unwrap();
            self.model.nodes.get_mut(&ino).unwrap().mode = mode;
        }
    }

    fn op_compression(&mut self) {
        let algos = [
            None,
            Some(Compression::Zlib),
            Some(Compression::Lz4),
            Some(Compression::Zstd),
        ];
        let a = self.pick(&algos);
        if self.below(2) == 0 {
            self.fs.set_mount_compression(a).unwrap();
        } else if let Some((_, _, ino)) = self.entry() {
            if !matches!(self.model.nodes[&ino].body, Body::Sym(_)) {
                let r = self.fs.set_compression(ino, a);
                if !self.no_space(&r) {
                    r.unwrap();
                }
            }
        }
    }

    fn op_bad_names(&mut self) {
        let d = self.some_dir();
        for bad in ["", ".", "..", "a/b", "nul\0x"] {
            assert_eq!(
                self.fs.create(d, bad, FileKind::Regular, 0o644, 0),
                Err(Error::Invalid),
                "{} name {:?}",
                self.ctx(),
                bad
            );
        }
        let long = "l".repeat(256);
        assert_eq!(
            self.fs.create(d, &long, FileKind::Regular, 0o644, 0),
            Err(Error::Invalid)
        );
        assert_eq!(self.fs.lookup(d, "nope-nope"), Err(Error::NotFound));
        if let Some(f) = self.some_file().filter(|_| !self.full) {
            assert_eq!(
                self.fs.create(f, "x", FileKind::Regular, 0o644, 0),
                Err(Error::NotDir)
            );
            assert_eq!(self.fs.readdir(f).map(|_| ()), Err(Error::NotDir));
            let mut buf = [0u8; 8];
            assert_eq!(self.fs.read(d, 0, &mut buf), Err(Error::IsDir));
            assert_eq!(self.fs.write(d, 0, b"x"), Err(Error::IsDir));
        }
    }

    fn op_seek(&mut self) {
        let Some(f) = self.some_file() else { return };
        let size = self.model.data(f).len() as u64;
        let off = self.below(size + 2);
        // Whatever the holes are, SEEK_DATA/SEEK_HOLE answers stay inside the
        // file and data reported is where nonzero bytes may be.
        let d = self.fs.seek_data(f, off).unwrap();
        let h = self.fs.seek_hole(f, off).unwrap();
        if off >= size {
            assert_eq!(d, None, "{} seek_data past EOF", self.ctx());
            assert_eq!(h, None, "{} seek_hole past EOF", self.ctx());
            return;
        }
        let data = self.model.data(f).clone();
        if let Some(d) = d {
            assert!(
                d >= off && d < size,
                "{} seek_data {} off {} size {}",
                self.ctx(),
                d,
                off,
                size
            );
            // Everything skipped was zero.
            assert!(
                data[off as usize..d as usize].iter().all(|&b| b == 0),
                "{} seek_data skipped data",
                self.ctx()
            );
        } else {
            assert!(
                data[off as usize..].iter().all(|&b| b == 0),
                "{} seek_data none",
                self.ctx()
            );
        }
        let h = h.expect("a hole at EOF at least");
        assert!(
            h >= off && h <= size,
            "{} seek_hole {} off {} size {}",
            self.ctx(),
            h,
            off,
            size
        );
    }

    fn op_remount(&mut self) {
        let mut fs = std::mem::replace(&mut self.fs, placeholder());
        fs.sync().unwrap();
        drop(fs);
        self.fs = open(&self.disks);
        self.unlock_fs();
        self.durable();
        if self.below(2) == 0 {
            self.verify("after remount");
        }
    }

    /// A growing operation refused for lack of space, on a small disk.
    fn no_space<T>(&self, r: &Result<T, Error>) -> bool {
        self.full && matches!(r, Err(Error::NoSpace))
    }

    /// The same for one that gives space back: only when snapshots hold
    /// on to it (then it frees nothing, and copying tree nodes may not eat
    /// the nodes the commit needs).
    fn no_space_held<T>(&self, r: &Result<T, Error>) -> bool {
        !self.snaps.is_empty() && self.no_space(r)
    }

    fn reserved(&self, d: u64, name: &str) -> bool {
        self.snaps_dir != 0 && d == self.root && name == "snaps"
    }

    fn skip(&self) -> Option<&'static str> {
        (self.snaps_dir != 0).then_some("snaps")
    }

    fn unlock_fs(&mut self) {
        if let Some(k) = &self.key {
            self.fs.set_random(random);
            self.fs.add_encryption_key(k).unwrap();
        }
    }

    /// Run everything inside a directory encrypted with a fresh key
    /// (snapshots are of whole subvolumes, so they are left out).
    fn encrypt(&mut self) {
        let key = vec![0x42u8; 64];
        self.fs.set_random(random);
        let id = self.fs.add_encryption_key(&key).unwrap();
        let vault = self
            .fs
            .create(self.root, "vault", FileKind::Dir, 0o700, 0)
            .unwrap();
        self.fs
            .set_encryption_policy(vault, &eclipsefs::EncryptionPolicy::new(id))
            .unwrap();
        self.fs.sync().unwrap();
        self.key = Some(key);
        let mut n = self.model.nodes.remove(&self.root).unwrap();
        n.mode = self.fs.stat(vault).unwrap().mode & 0o7777;
        n.parent = vault;
        self.model.nodes.insert(vault, n);
        self.root = vault;
        self.snaps_dir = 0;
        self.durable();
    }

    fn op_snapshot(&mut self) {
        if self.snaps_dir == 0 || self.no_snaps {
            return;
        }
        let name = format!("s{}", self.step);
        let r = self.fs.snapshot(self.root, self.snaps_dir, &name, true);
        if tracing() {
            eprintln!("{} snapshot {} -> {:?}", self.step, name, r);
        }
        if self.no_space(&r) {
            return;
        }
        let snap = r.unwrap_or_else(|e| panic!("{} snapshot: {:?}", self.ctx(), e));
        self.snaps.push((snap, name, self.model.clone()));
        self.send_snapshot(snap);
    }

    /// Send `snap` to the receiving filesystem, incrementally from the
    /// snapshot sent before when it is still there.
    fn send_snapshot(&mut self, snap: u64) {
        let parent = self.last_sent;
        if tracing() {
            eprintln!("{} send {} parent {:?}", self.step, snap, parent);
        }
        let mut stream = Vec::new();
        self.fs
            .send(snap, parent, &mut |b| {
                stream.extend_from_slice(b);
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{} send: {:?}", self.ctx(), e));
        if self.recv.is_none() {
            let (disk, fs) = fresh(96 * MIB);
            let root = fs.root_ino();
            self.recv = Some((disk, fs, root));
        }
        let (_, rfs, rroot) = self.recv.as_mut().unwrap();
        let mut pos = 0;
        let got = rfs
            .receive(*rroot, &mut |buf| {
                let end = pos + buf.len();
                if end > stream.len() {
                    return Err(Error::Corrupt("short stream"));
                }
                buf.copy_from_slice(&stream[pos..end]);
                pos = end;
                Ok(())
            })
            .unwrap_or_else(|e| {
                panic!(
                    "seed {} step {} receive (parent {:?}): {:?}",
                    self.seed, self.step, parent, e
                )
            });
        self.received.push((got, self.model.clone()));
        self.last_sent = Some(snap);
    }

    fn op_delete_snapshot(&mut self) {
        if self.snaps.is_empty() {
            return;
        }
        let i = self.below(self.snaps.len() as u64) as usize;
        let r = self
            .fs
            .delete_subvolume(self.snaps_dir, &self.snaps[i].1.clone());
        if self.no_space(&r) {
            return;
        }
        let (snap, name, _) = self.snaps.remove(i);
        if tracing() {
            eprintln!("{} delete snapshot {} {}", self.step, name, snap);
        }
        // Its id (and so its handle) may be given to a later snapshot.
        if self.last_sent == Some(snap) {
            self.last_sent = None;
        }
        r.unwrap_or_else(|e| panic!("{} delete snapshot {}: {:?}", self.ctx(), name, e));
        assert_eq!(self.fs.lookup(self.snaps_dir, &name), Err(Error::NotFound));
    }

    fn op_defrag(&mut self) {
        let Some(f) = self.some_file() else { return };
        let size = self.model.data(f).len() as u64;
        let start = self.below(size / BS + 1) * BS;
        let len = if self.below(2) == 0 {
            0
        } else {
            self.below(size + 1)
        };
        let comp = [
            None,
            Some(Compression::Zlib),
            Some(Compression::Lz4),
            Some(Compression::Zstd),
        ];
        let c = self.pick(&comp);
        let threshold = self.pick(&[0, BS, 64 * 1024]);
        match self.fs.defrag(f, start, len, threshold, c) {
            Ok(_) => {}
            Err(Error::Unsupported(_)) if self.key.is_some() => {}
            Err(Error::NoSpace) if self.full => {}
            Err(e) => panic!("{} defrag {}: {:?}", self.ctx(), f, e),
        }
    }

    /// Scrub, balance and repair a healthy filesystem: none of them may
    /// find anything to fix or change a byte of the tree.
    fn op_maintenance(&mut self) {
        let ctx = self.ctx();
        let r = self.fs.scrub().unwrap();
        assert!(r.is_clean(), "{} scrub: {:?}", ctx, r);
        match self.below(2) {
            0 => {
                let r = self.fs.balance(None, None);
                if tracing() {
                    eprintln!("{} balance -> {:?}", self.step, r);
                }
                if !self.no_space(&r) {
                    r.unwrap_or_else(|e| panic!("{} balance: {:?}", ctx, e));
                }
            }
            _ => {
                let r = self.fs.repair();
                if tracing() {
                    eprintln!("{} repair -> {:?}", self.step, r);
                }
                // Repair copies tree nodes too, so a full disk whose
                // snapshots keep everything refuses it like any other change.
                if self.no_space_held(&r) {
                    return;
                }
                let r = r.unwrap_or_else(|e| panic!("{} repair: {:?}", ctx, e));
                assert!(
                    r.is_clean() && !r.changed(),
                    "{} repair of a healthy fs: {:?}",
                    ctx,
                    r
                );
            }
        }
        self.durable();
        self.verify("after maintenance");
    }

    /// Mount a copy of the disk as it is now, as after a power cut: it
    /// must pass the full check and hold one of the states since the last
    /// sync (or, after an fsync, anything that passes the check).
    fn power_cut(&mut self) {
        if tracing() {
            eprintln!("{} power cut", self.step);
        }
        let copies: Vec<Arc<MemDisk>> = self.disks.iter().map(|d| d.copy()).collect();
        let mut fs = EclipseFS::mount_devices(devs(&copies), false)
            .unwrap_or_else(|e| panic!("{} mount after power cut: {:?}", self.ctx(), e));
        fs.set_clock(clock);
        if let Some(k) = &self.key {
            fs.set_random(random);
            fs.add_encryption_key(k).unwrap();
        }
        let _rep = fs
            .check()
            .unwrap_or_else(|e| panic!("{} check after power cut: {:?}", self.ctx(), e));
        let got = read_tree(&mut fs, self.root, self.skip());
        // A file fsynced and left alone since then holds what it held
        // then, whatever else the power cut took.
        for (&f, &h) in &self.fsync_seen {
            if self
                .model
                .nodes
                .get(&f)
                .is_some_and(|n| matches!(&n.body, Body::File(d) if hash(d) == h))
            {
                match got.nodes.get(&f) {
                    Some(Node {
                        body: Body::File(d),
                        ..
                    }) => assert!(
                        hash(d) == h,
                        "{} fsynced file {} lost its data in a power cut (len {} want {})",
                        self.ctx(),
                        f,
                        d.len(),
                        self.model.data(f).len()
                    ),
                    other => panic!(
                        "{} fsynced file {} gone after a power cut: {:?}",
                        self.ctx(),
                        f,
                        other.map(short)
                    ),
                }
            }
        }
        if !self.fsynced {
            let mut want_any = false;
            for &fp in &self.since_sync {
                if fp == got.fingerprint() {
                    want_any = true;
                }
            }
            assert!(
                want_any,
                "{} power cut left a state that never existed ({} states since sync)",
                self.ctx(),
                self.since_sync.len()
            );
        }
    }

    fn verify(&mut self, what: &str) {
        let what = format!("{} {}", self.ctx(), what);
        let skip = self.skip();
        assert_same(&mut self.fs, &self.model, self.root, skip, &what);
        for (snap, name, m) in self.snaps.clone() {
            assert_eq!(self.fs.lookup(self.snaps_dir, &name).unwrap(), snap);
            let got = read_tree(&mut self.fs, snap, Some("snaps"));
            // Inode numbers differ by subvolume: compare structure.
            assert_eq!(
                canon(&got, snap),
                canon(&m, self.root),
                "{} snapshot {}",
                what,
                name
            );
        }
        if let Some((_, rfs, _)) = self.recv.as_mut() {
            for (r, m) in &self.received {
                let got = read_tree(rfs, *r, Some("snaps"));
                assert_eq!(
                    canon(&got, *r),
                    canon(m, self.root),
                    "{} received snapshot",
                    what
                );
            }
        }
    }
}

/// The tree as a path-keyed listing, for trees whose inode numbers differ.
fn canon(m: &Model, root: u64) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut todo = vec![(root, String::new())];
    while let Some((ino, path)) = todo.pop() {
        let n = &m.nodes[&ino];
        let desc = match &n.body {
            Body::Dir(d) => {
                for (name, &c) in d {
                    todo.push((c, format!("{}/{}", path, name)));
                }
                format!("dir mode {:o} x {:?}", n.mode, n.xattrs)
            }
            Body::File(d) => format!(
                "file {:x} len {} links {} mode {:o} x {:?}",
                hash(d),
                d.len(),
                n.nlink,
                n.mode,
                n.xattrs
            ),
            Body::Sym(t) => format!("sym {:?}", String::from_utf8_lossy(t)),
        };
        out.push((path, desc));
    }
    out.sort();
    out
}

fn hash(d: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.hash(&mut h);
    h.finish()
}

fn random(buf: &mut [u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for b in buf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        *b = (n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8;
    }
}

fn placeholder() -> EclipseFS {
    let (_d, fs) = fresh(MIB);
    fs
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn devs(d: &[Arc<MemDisk>]) -> Vec<Arc<dyn BlockDevice>> {
    d.iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect()
}

fn open(d: &[Arc<MemDisk>]) -> EclipseFS {
    let mut fs = EclipseFS::mount_devices(devs(d), false).unwrap();
    fs.set_clock(clock);
    fs
}

/// One formatted disk of `size` bytes.
fn single(size: usize) -> Vec<Arc<MemDisk>> {
    let (disk, fs) = fresh(size);
    drop(fs);
    vec![disk]
}

/// `n` disks formatted as one volume.
fn volume(n: usize, size: usize, data: Profile, meta: Profile) -> Vec<Arc<MemDisk>> {
    let d: Vec<Arc<MemDisk>> = (0..n).map(|_| MemDisk::new(size)).collect();
    mkfs::format_devices(&devs(&d), &opts(), data, meta).unwrap();
    d
}

fn run(seed: u64, steps: usize, disks: Vec<Arc<MemDisk>>, setup: impl Fn(&mut Run)) {
    let mut r = Run::new(seed, disks);
    setup(&mut r);
    for step in 0..steps {
        r.step = step;
        r.step();
        if step % 250 == 249 {
            r.verify("periodic");
        }
        if step % 1000 == 999 {
            let rep = r.fs.check().unwrap();
            let _ = rep;
        }
    }
    r.verify("end");
    r.op_remount();
    r.verify("end after remount");
    let rep = r.fs.check().unwrap();
    let _ = rep;
    assert!(r.fs.scrub().unwrap().is_clean(), "seed {} scrub", seed);
}

/// Seeds to run; `EFS_MODEL_SEED=n` replays only seed n (with
/// `EFS_MODEL_TRACE=1` printing every operation).
fn seeds(default: usize) -> std::ops::Range<u64> {
    match std::env::var("EFS_MODEL_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(n) => n..n + 1,
        None => 0..env("EFS_MODEL_SEEDS", default) as u64,
    }
}

fn tracing() -> bool {
    std::env::var_os("EFS_MODEL_TRACE").is_some()
}

#[test]
fn whole_filesystem_matches_a_posix_model() {
    let steps = env("EFS_MODEL_STEPS", 1500);
    for seed in seeds(4) {
        run(seed, steps, single(64 * MIB), |_| {});
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_with_compression() {
    let steps = env("EFS_MODEL_STEPS", 1500);
    for seed in seeds(2) {
        run(1000 + seed, steps, single(64 * MIB), |r| {
            r.fs.set_mount_compression(Some(Compression::Zstd)).unwrap()
        });
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_with_timed_commits() {
    let steps = env("EFS_MODEL_STEPS", 1500);
    for seed in seeds(2) {
        run(2000 + seed, steps, single(64 * MIB), |r| {
            r.fs.set_commit_interval(Some(3600))
        });
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_on_raid1() {
    let steps = env("EFS_MODEL_STEPS", 1200);
    for seed in seeds(1) {
        run(
            3000 + seed,
            steps,
            volume(2, 48 * MIB, Profile::Raid1, Profile::Raid1),
            |_| {},
        );
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_on_raid5() {
    let steps = env("EFS_MODEL_STEPS", 1200);
    for seed in seeds(1) {
        run(
            4000 + seed,
            steps,
            volume(3, 48 * MIB, Profile::Raid5, Profile::Raid1),
            |_| {},
        );
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_on_raid6() {
    let steps = env("EFS_MODEL_STEPS", 1200);
    for seed in seeds(1) {
        run(
            5000 + seed,
            steps,
            volume(4, 48 * MIB, Profile::Raid6, Profile::Raid1c3),
            |_| {},
        );
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_on_raid10_and_dup() {
    let steps = env("EFS_MODEL_STEPS", 1200);
    for seed in seeds(1) {
        run(
            6000 + seed,
            steps,
            volume(4, 48 * MIB, Profile::Raid10, Profile::Dup),
            |_| {},
        );
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_inside_an_encrypted_directory() {
    let steps = env("EFS_MODEL_STEPS", 1500);
    for seed in seeds(2) {
        run(7000 + seed, steps, single(64 * MIB), |r| r.encrypt());
    }
}

/// A disk too small for the workload: it runs full over and over. Every
/// operation that needs space fails cleanly (or writes short) and changes
/// nothing else, while deleting, shrinking and punching still work, and
/// everything stays consistent through remounts and power cuts.
#[test]
fn whole_filesystem_matches_a_posix_model_on_a_full_disk() {
    let steps = env("EFS_MODEL_STEPS", 2000);
    for seed in seeds(3) {
        run(8000 + seed, steps, single(6 * MIB), |r| r.full = true);
    }
}

#[test]
fn whole_filesystem_matches_a_posix_model_on_a_full_raid1_volume() {
    let steps = env("EFS_MODEL_STEPS", 2000);
    for seed in seeds(1) {
        run(
            9000 + seed,
            steps,
            volume(2, 12 * MIB, Profile::Raid1, Profile::Raid1),
            |r| r.full = true,
        );
    }
}

/// The same without snapshots: then every delete really frees space, and
/// deleting, shrinking and punching whole blocks always work.
#[test]
fn whole_filesystem_matches_a_posix_model_on_a_full_disk_without_snapshots() {
    let steps = env("EFS_MODEL_STEPS", 2000);
    for seed in seeds(3) {
        run(10000 + seed, steps, single(6 * MIB), |r| {
            r.full = true;
            r.no_snaps = true;
        });
    }
}

/// The run that found the reference tree drifting from the counts it was
/// written from: during the commit, copying the tree's own nodes joined
/// two neighbouring count entries in memory, so the next commit tried to
/// insert an entry the tree already had and every commit from then on
/// failed with `Exists` (snapshot at step 2899). Kept as a fixed replay.
#[test]
fn reference_counts_reach_the_tree_as_they_are_in_memory() {
    run(2002, 2950, single(64 * MIB), |r| {
        r.fs.set_commit_interval(Some(3600))
    });
}
