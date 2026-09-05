//! Linux namespaces for bubblewrap / Flatpak.
//!
//! Isolation is real for the pieces Flatpak exercises:
//!
//! * **user** — uid/gid maps (`/proc/<pid>/uid_map`), overflowuid until mapped
//! * **mount** — per-ns overlay (bind + tmpfs) that never mutates the host VFS
//! * **uts** — hostname
//! * **net** — AF_INET/INET6 sockets fail; AF_UNIX still works (dbus/X11/Wayland)
//! * **pid / ipc / cgroup** — distinct ns inodes so `setns` / `stat` work;
//!   pid numbers stay the host ids (so `/proc` stays coherent)
//!
//! `unshare`/`clone(CLONE_NEW*)` used to return `ENOSYS`/`EINVAL` on purpose
//! so glycin could skip bwrap. Those arms now succeed with working semantics.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use rcore_fs_ramfs::RamFS;

use crate::error::{LxError, LxResult};

/// `CLONE_NEW*` bits (clone(2) / unshare(2)).
pub const CLONE_NEWNS: usize = 1 << 17;
pub const CLONE_NEWCGROUP: usize = 1 << 25;
pub const CLONE_NEWUTS: usize = 1 << 26;
pub const CLONE_NEWIPC: usize = 1 << 27;
pub const CLONE_NEWUSER: usize = 1 << 28;
pub const CLONE_NEWPID: usize = 1 << 29;
pub const CLONE_NEWNET: usize = 1 << 30;

const OVERFLOW_ID: u32 = 65534;

static NS_IDS: AtomicU64 = AtomicU64::new(1);

fn next_ns_id() -> u64 {
    NS_IDS.fetch_add(1, Ordering::Relaxed)
}

fn norm_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        return String::from("/");
    }
    let mut out = String::from("/");
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(slash) = out.rfind('/') {
                    if slash == 0 {
                        out.truncate(1);
                    } else {
                        out.truncate(slash);
                    }
                }
            }
            _ => {
                if out != "/" {
                    out.push('/');
                }
                out.push_str(seg);
            }
        }
    }
    out
}

fn join_prefix(prefix: &str, path: &str) -> String {
    let path = norm_path(path);
    if prefix.is_empty() || prefix == "/" {
        return path;
    }
    if path == "/" {
        return norm_path(prefix);
    }
    let mut out = norm_path(prefix);
    if out != "/" {
        out.push_str(&path);
    } else {
        out = path;
    }
    out
}

/// One uid/gid map range (`/proc/<pid>/uid_map` line).
#[derive(Clone, Copy, Debug)]
pub struct IdMap {
    pub inside: u32,
    pub outside: u32,
    pub length: u32,
}

impl IdMap {
    fn map_in(self, outer: u32) -> Option<u32> {
        if outer >= self.outside && (outer - self.outside) < self.length {
            Some(self.inside + (outer - self.outside))
        } else {
            None
        }
    }
}

fn parse_id_map(buf: &str) -> LxResult<Vec<IdMap>> {
    let mut maps = Vec::new();
    for line in buf.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let inside: u32 = it.next().and_then(|s| s.parse().ok()).ok_or(LxError::EINVAL)?;
        let outside: u32 = it.next().and_then(|s| s.parse().ok()).ok_or(LxError::EINVAL)?;
        let length: u32 = it.next().and_then(|s| s.parse().ok()).ok_or(LxError::EINVAL)?;
        if length == 0 || it.next().is_some() {
            return Err(LxError::EINVAL);
        }
        maps.push(IdMap {
            inside,
            outside,
            length,
        });
    }
    if maps.is_empty() {
        return Err(LxError::EINVAL);
    }
    Ok(maps)
}

fn format_id_map(maps: &[IdMap]) -> String {
    if maps.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    for m in maps {
        let _ = core::fmt::Write::write_fmt(
            &mut s,
            format_args!("{} {} {}\n", m.inside, m.outside, m.length),
        );
    }
    s
}

/// User namespace.
pub struct UserNs {
    pub id: u64,
    parent: Option<Weak<UserNs>>,
    uid_map: Mutex<Vec<IdMap>>,
    gid_map: Mutex<Vec<IdMap>>,
    setgroups_deny: Mutex<bool>,
}

impl UserNs {
    fn init() -> Arc<Self> {
        Arc::new(Self {
            id: 0,
            parent: None,
            uid_map: Mutex::new(vec![IdMap {
                inside: 0,
                outside: 0,
                length: u32::MAX,
            }]),
            gid_map: Mutex::new(vec![IdMap {
                inside: 0,
                outside: 0,
                length: u32::MAX,
            }]),
            setgroups_deny: Mutex::new(false),
        })
    }

    fn child_of(parent: &Arc<UserNs>) -> Arc<Self> {
        Arc::new(Self {
            id: next_ns_id(),
            parent: Some(Arc::downgrade(parent)),
            uid_map: Mutex::new(Vec::new()),
            gid_map: Mutex::new(Vec::new()),
            setgroups_deny: Mutex::new(false),
        })
    }

    pub fn is_init(&self) -> bool {
        self.id == 0
    }

    pub fn map_uid_in(&self, outer: u32) -> u32 {
        let maps = self.uid_map.lock();
        if maps.is_empty() {
            return OVERFLOW_ID;
        }
        maps.iter().find_map(|m| m.map_in(outer)).unwrap_or(OVERFLOW_ID)
    }

    pub fn map_gid_in(&self, outer: u32) -> u32 {
        let maps = self.gid_map.lock();
        if maps.is_empty() {
            return OVERFLOW_ID;
        }
        maps.iter().find_map(|m| m.map_in(outer)).unwrap_or(OVERFLOW_ID)
    }

    pub fn write_uid_map(&self, buf: &str) -> LxResult<()> {
        let mut slot = self.uid_map.lock();
        if !slot.is_empty() && self.id != 0 {
            return Err(LxError::EPERM);
        }
        if self.id == 0 {
            return Err(LxError::EPERM);
        }
        *slot = parse_id_map(buf)?;
        Ok(())
    }

    pub fn write_gid_map(&self, buf: &str) -> LxResult<()> {
        let mut slot = self.gid_map.lock();
        if !slot.is_empty() && self.id != 0 {
            return Err(LxError::EPERM);
        }
        if self.id == 0 {
            return Err(LxError::EPERM);
        }
        *slot = parse_id_map(buf)?;
        Ok(())
    }

    pub fn uid_map_text(&self) -> String {
        format_id_map(&self.uid_map.lock())
    }

    pub fn gid_map_text(&self) -> String {
        format_id_map(&self.gid_map.lock())
    }

    pub fn setgroups_deny(&self) -> bool {
        *self.setgroups_deny.lock()
    }

    pub fn write_setgroups(&self, buf: &str) -> LxResult<()> {
        let v = buf.trim();
        if v.eq_ignore_ascii_case("deny") {
            *self.setgroups_deny.lock() = true;
            Ok(())
        } else if v.eq_ignore_ascii_case("allow") {
            if !self.gid_map.lock().is_empty() {
                return Err(LxError::EPERM);
            }
            *self.setgroups_deny.lock() = false;
            Ok(())
        } else {
            Err(LxError::EINVAL)
        }
    }

    #[allow(dead_code)]
    pub fn parent(&self) -> Option<Arc<UserNs>> {
        self.parent.as_ref().and_then(|w| w.upgrade())
    }
}

/// A mount visible only inside this mount namespace.
#[derive(Clone)]
pub enum NsMount {
    Bind { inode: Arc<dyn INode> },
    Tmpfs { root: Arc<dyn INode> },
}

impl NsMount {
    fn inode(&self) -> &Arc<dyn INode> {
        match self {
            NsMount::Bind { inode } => inode,
            NsMount::Tmpfs { root } => root,
        }
    }
}

fn new_tmpfs_root() -> Arc<dyn INode> {
    RamFS::new().root_inode()
}

/// Mount namespace with a path overlay on top of the host VFS.
pub struct MountNs {
    pub id: u64,
    overlays: Mutex<BTreeMap<String, NsMount>>,
}

impl MountNs {
    fn init() -> Arc<Self> {
        Arc::new(Self {
            id: 0,
            overlays: Mutex::new(BTreeMap::new()),
        })
    }

    fn fork_from(parent: &MountNs) -> Arc<Self> {
        Arc::new(Self {
            id: next_ns_id(),
            overlays: Mutex::new(parent.overlays.lock().clone()),
        })
    }

    pub fn is_init(&self) -> bool {
        self.id == 0
    }

    pub fn bind(&self, target: &str, inode: Arc<dyn INode>) -> LxResult<()> {
        let target = norm_path(target);
        self.overlays
            .lock()
            .insert(target, NsMount::Bind { inode });
        Ok(())
    }

    pub fn mount_tmpfs(&self, target: &str) -> LxResult<()> {
        let target = norm_path(target);
        self.overlays.lock().insert(
            target,
            NsMount::Tmpfs {
                root: new_tmpfs_root(),
            },
        );
        Ok(())
    }

    pub fn umount(&self, target: &str) -> LxResult<()> {
        let target = norm_path(target);
        self.overlays
            .lock()
            .remove(&target)
            .map(|_| ())
            .ok_or(LxError::EINVAL)
    }

    /// Rewrite overlay keys after `pivot_root(new_root)`: drop the `new_root`
    /// prefix so `/tmp/newroot/usr` becomes `/usr`.
    pub fn rebase(&self, new_root: &str) {
        let prefix = norm_path(new_root);
        if prefix == "/" {
            return;
        }
        let mut overlays = self.overlays.lock();
        let old = core::mem::take(&mut *overlays);
        for (path, mnt) in old {
            if path == prefix {
                continue;
            }
            if let Some(rest) = path.strip_prefix(&prefix) {
                if rest.is_empty() || rest.starts_with('/') {
                    let new_path = if rest.is_empty() || rest == "/" {
                        String::from("/")
                    } else {
                        rest.to_string()
                    };
                    overlays.insert(new_path, mnt);
                    continue;
                }
            }
            overlays.insert(path, mnt);
        }
    }

    /// Longest-prefix overlay hit for an absolute path in this ns.
    pub fn lookup(&self, abs_path: &str) -> Option<LxResult<Arc<dyn INode>>> {
        let path = norm_path(abs_path);
        let (key, inode) = {
            let overlays = self.overlays.lock();
            let mut best: Option<(&str, Arc<dyn INode>)> = None;
            for (k, mnt) in overlays.iter() {
                if path == *k
                    || (k != "/"
                        && path.starts_with(k)
                        && path.as_bytes().get(k.len()) == Some(&b'/'))
                    || (k == "/" && path.starts_with('/'))
                {
                    match &best {
                        None => best = Some((k.as_str(), mnt.inode().clone())),
                        Some((prev, _)) if k.len() >= prev.len() => {
                            best = Some((k.as_str(), mnt.inode().clone()))
                        }
                        _ => {}
                    }
                }
            }
            best.map(|(k, i)| (k.to_string(), i))?
        };
        let rest = if path.len() <= key.len() {
            ""
        } else {
            path[key.len()..].trim_start_matches('/')
        };
        if rest.is_empty() {
            return Some(Ok(inode));
        }
        Some(inode.lookup_follow(rest, 8).map_err(LxError::from))
    }
}

/// UTS namespace (hostname).
pub struct UtsNs {
    pub id: u64,
    hostname: Mutex<String>,
}

impl UtsNs {
    fn init() -> Arc<Self> {
        Arc::new(Self {
            id: 0,
            hostname: Mutex::new(String::from("eclipse")),
        })
    }

    fn fork_from(parent: &UtsNs) -> Arc<Self> {
        Arc::new(Self {
            id: next_ns_id(),
            hostname: Mutex::new(parent.hostname.lock().clone()),
        })
    }

    pub fn hostname(&self) -> String {
        self.hostname.lock().clone()
    }

    pub fn set_hostname(&self, name: &str) {
        *self.hostname.lock() = name.trim().to_string();
    }
}

/// Network namespace. Isolated => no AF_INET/INET6.
pub struct NetNs {
    pub id: u64,
    isolated: bool,
}

impl NetNs {
    fn init() -> Arc<Self> {
        Arc::new(Self {
            id: 0,
            isolated: false,
        })
    }

    fn isolated() -> Arc<Self> {
        Arc::new(Self {
            id: next_ns_id(),
            isolated: true,
        })
    }

    pub fn blocks_inet(&self) -> bool {
        self.isolated
    }
}

/// PID / IPC / cgroup namespaces: identity only (inode + id) for setns/stat.
pub struct IdNs {
    pub id: u64,
    pub kind: NsKind,
}

impl IdNs {
    fn init(kind: NsKind) -> Arc<Self> {
        Arc::new(Self { id: 0, kind })
    }

    fn fresh(kind: NsKind) -> Arc<Self> {
        Arc::new(Self {
            id: next_ns_id(),
            kind,
        })
    }
}

/// Which namespace a `/proc/<pid>/ns/*` handle refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NsKind {
    Mnt,
    User,
    Uts,
    Pid,
    Net,
    Ipc,
    Cgroup,
}

impl NsKind {
    pub fn name(self) -> &'static str {
        match self {
            NsKind::Mnt => "mnt",
            NsKind::User => "user",
            NsKind::Uts => "uts",
            NsKind::Pid => "pid",
            NsKind::Net => "net",
            NsKind::Ipc => "ipc",
            NsKind::Cgroup => "cgroup",
        }
    }

    pub fn clone_flag(self) -> usize {
        match self {
            NsKind::Mnt => CLONE_NEWNS,
            NsKind::User => CLONE_NEWUSER,
            NsKind::Uts => CLONE_NEWUTS,
            NsKind::Pid => CLONE_NEWPID,
            NsKind::Net => CLONE_NEWNET,
            NsKind::Ipc => CLONE_NEWIPC,
            NsKind::Cgroup => CLONE_NEWCGROUP,
        }
    }

    pub fn from_clone_flag(flags: usize) -> Option<Self> {
        match flags {
            0 => None,
            CLONE_NEWNS => Some(NsKind::Mnt),
            CLONE_NEWUSER => Some(NsKind::User),
            CLONE_NEWUTS => Some(NsKind::Uts),
            CLONE_NEWPID => Some(NsKind::Pid),
            CLONE_NEWNET => Some(NsKind::Net),
            CLONE_NEWIPC => Some(NsKind::Ipc),
            CLONE_NEWCGROUP => Some(NsKind::Cgroup),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct NsInner {
    mount: Arc<MountNs>,
    user: Arc<UserNs>,
    uts: Arc<UtsNs>,
    pid: Arc<IdNs>,
    net: Arc<NetNs>,
    ipc: Arc<IdNs>,
    cgroup: Arc<IdNs>,
}

/// Per-process namespace proxy (cloned on fork, replaced on unshare).
pub struct NsProxy {
    inner: Mutex<NsInner>,
    /// `unshare(CLONE_NEWPID)` applies to the *next child*, not this task.
    pending_pid: Mutex<Option<Arc<IdNs>>>,
}

impl Clone for NsProxy {
    fn clone(&self) -> Self {
        Self {
            inner: Mutex::new(self.inner.lock().clone()),
            pending_pid: Mutex::new(self.pending_pid.lock().clone()),
        }
    }
}

impl NsProxy {
    pub fn init() -> Self {
        Self {
            inner: Mutex::new(INIT_NS.inner.lock().clone()),
            pending_pid: Mutex::new(None),
        }
    }

    pub fn mount(&self) -> Arc<MountNs> {
        self.inner.lock().mount.clone()
    }

    pub fn user(&self) -> Arc<UserNs> {
        self.inner.lock().user.clone()
    }

    pub fn uts(&self) -> Arc<UtsNs> {
        self.inner.lock().uts.clone()
    }

    pub fn net(&self) -> Arc<NetNs> {
        self.inner.lock().net.clone()
    }

    pub fn pid(&self) -> Arc<IdNs> {
        self.inner.lock().pid.clone()
    }

    pub fn ipc(&self) -> Arc<IdNs> {
        self.inner.lock().ipc.clone()
    }

    pub fn cgroup(&self) -> Arc<IdNs> {
        self.inner.lock().cgroup.clone()
    }

    /// `unshare(2)`: replace namespaces on *this* process.
    /// `CLONE_NEWPID` is remembered for the next fork instead.
    pub fn unshare(&self, flags: usize) -> LxResult<()> {
        self.apply(flags, false)
    }

    /// `clone(CLONE_NEW*)`: the new child is already in the new namespaces
    /// (including pid, where it is the ns init).
    pub fn clone_into(&self, flags: usize) -> LxResult<()> {
        self.apply(flags, true)
    }

    fn apply(&self, flags: usize, clone_child: bool) -> LxResult<()> {
        let mut inner = self.inner.lock();
        if flags & CLONE_NEWUSER != 0 {
            inner.user = UserNs::child_of(&inner.user);
        }
        if flags & CLONE_NEWNS != 0 {
            inner.mount = MountNs::fork_from(&inner.mount);
        }
        if flags & CLONE_NEWUTS != 0 {
            inner.uts = UtsNs::fork_from(&inner.uts);
        }
        if flags & CLONE_NEWNET != 0 {
            inner.net = NetNs::isolated();
        }
        if flags & CLONE_NEWIPC != 0 {
            inner.ipc = IdNs::fresh(NsKind::Ipc);
        }
        if flags & CLONE_NEWCGROUP != 0 {
            inner.cgroup = IdNs::fresh(NsKind::Cgroup);
        }
        if flags & CLONE_NEWPID != 0 {
            let fresh = IdNs::fresh(NsKind::Pid);
            if clone_child {
                inner.pid = fresh;
            } else {
                *self.pending_pid.lock() = Some(fresh);
            }
        }
        Ok(())
    }

    /// Consume a pending pid-ns from `unshare(CLONE_NEWPID)` into a new child.
    pub fn take_pending_pid_for_child(&self) -> Option<Arc<IdNs>> {
        self.pending_pid.lock().take()
    }

    pub fn set_pid_ns(&self, pid: Arc<IdNs>) {
        self.inner.lock().pid = pid;
    }

    pub fn setns(&self, kind: NsKind, other: &NsProxy) -> LxResult<()> {
        let mut inner = self.inner.lock();
        let src = other.inner.lock();
        match kind {
            NsKind::Mnt => inner.mount = src.mount.clone(),
            NsKind::User => inner.user = src.user.clone(),
            NsKind::Uts => inner.uts = src.uts.clone(),
            NsKind::Pid => inner.pid = src.pid.clone(),
            NsKind::Net => inner.net = src.net.clone(),
            NsKind::Ipc => inner.ipc = src.ipc.clone(),
            NsKind::Cgroup => inner.cgroup = src.cgroup.clone(),
        }
        Ok(())
    }

    pub fn ns_inode(&self, kind: NsKind) -> u64 {
        match kind {
            NsKind::Mnt => self.mount().id,
            NsKind::User => self.user().id,
            NsKind::Uts => self.uts().id,
            NsKind::Pid => self.pid().id,
            NsKind::Net => self.net().id,
            NsKind::Ipc => self.ipc().id,
            NsKind::Cgroup => self.cgroup().id,
        }
    }
}

lazy_static! {
    static ref INIT_NS: NsProxy = NsProxy {
        inner: Mutex::new(NsInner {
            mount: MountNs::init(),
            user: UserNs::init(),
            uts: UtsNs::init(),
            pid: IdNs::init(NsKind::Pid),
            net: NetNs::init(),
            ipc: IdNs::init(NsKind::Ipc),
            cgroup: IdNs::init(NsKind::Cgroup),
        }),
        pending_pid: Mutex::new(None),
    };
}

/// `/proc/<pid>/ns/<kind>` — setns(2) handle.
pub struct ProcNsFile {
    pid: u64,
    kind: NsKind,
}

impl ProcNsFile {
    pub fn new(pid: u64, kind: NsKind) -> Self {
        Self { pid, kind }
    }

    pub fn kind(&self) -> NsKind {
        self.kind
    }

    pub fn pid(&self) -> u64 {
        self.pid
    }
}

impl INode for ProcNsFile {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 0x6e53_0000 + self.kind as usize + (self.pid as usize).wrapping_mul(8),
            size: 0,
            blk_size: 4096,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::SymLink,
            mode: 0o777,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(MarkerFs)
    }
}

/// `/proc/<pid>/ns/` directory.
pub struct ProcNsDir {
    pid: u64,
}

impl ProcNsDir {
    pub fn new(pid: u64) -> Self {
        Self { pid }
    }

    fn entries() -> &'static [&'static str] {
        &[".", "..", "mnt", "user", "uts", "pid", "net", "ipc", "cgroup"]
    }
}

impl INode for ProcNsDir {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 0x6e53_d000 + self.pid as usize,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o555,
            nlinks: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(MarkerFs)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        let kind = match name {
            "mnt" => NsKind::Mnt,
            "user" => NsKind::User,
            "uts" => NsKind::Uts,
            "pid" => NsKind::Pid,
            "net" => NsKind::Net,
            "ipc" => NsKind::Ipc,
            "cgroup" => NsKind::Cgroup,
            "." => return Ok(Arc::new(ProcNsDir { pid: self.pid })),
            ".." => return Err(FsError::EntryNotFound),
            _ => return Err(FsError::EntryNotFound),
        };
        Ok(Arc::new(ProcNsFile::new(self.pid, kind)))
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].to_string())
    }
}

/// Writable `/proc/<pid>/{uid_map,gid_map,setgroups}`.
pub struct ProcIdMapFile {
    pid: u64,
    which: IdMapKind,
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum IdMapKind {
    UidMap,
    GidMap,
    Setgroups,
}

impl ProcIdMapFile {
    pub fn new(pid: u64, which: IdMapKind) -> Self {
        Self { pid, which }
    }

    fn target_user_ns(&self) -> Result<Arc<UserNs>> {
        use zircon_object::task::ROOT_JOB;
        use crate::process::ProcessExt;
        let proc = ROOT_JOB
            .find_process(self.pid as _)
            .ok_or(FsError::EntryNotFound)?;
        let lp = proc.try_linux().ok_or(FsError::EntryNotFound)?;
        Ok(lp.ns().user())
    }

    fn text(&self) -> Result<String> {
        let ns = self.target_user_ns()?;
        Ok(match self.which {
            IdMapKind::UidMap => ns.uid_map_text(),
            IdMapKind::GidMap => ns.gid_map_text(),
            IdMapKind::Setgroups => {
                if ns.setgroups_deny() {
                    String::from("deny\n")
                } else {
                    String::from("allow\n")
                }
            }
        })
    }
}

impl INode for ProcIdMapFile {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let bytes = self.text()?.into_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let s = core::str::from_utf8(buf).map_err(|_| FsError::InvalidParam)?;
        let ns = self.target_user_ns()?;
        match self.which {
            IdMapKind::UidMap => ns.write_uid_map(s).map_err(|_| FsError::InvalidParam)?,
            IdMapKind::GidMap => ns.write_gid_map(s).map_err(|_| FsError::InvalidParam)?,
            IdMapKind::Setgroups => ns.write_setgroups(s).map_err(|_| FsError::InvalidParam)?,
        }
        Ok(buf.len())
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 0x6964_0000 + self.pid as usize + self.which as usize,
            size: 64,
            blk_size: 4096,
            blocks: 1,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(MarkerFs)
    }
}

pub fn join_chroot_prefix(prefix: &str, path: &str) -> String {
    join_prefix(prefix, path)
}

struct MarkerFs;

impl FileSystem for MarkerFs {
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(ProcNsDir { pid: 0 })
    }
    fn info(&self) -> FsInfo {
        FsInfo {
            bsize: 4096,
            frsize: 4096,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 255,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_map_roundtrip() {
        let maps = parse_id_map("0 1000 1\n").unwrap();
        assert_eq!(maps[0].map_in(1000), Some(0));
        assert_eq!(maps[0].map_in(0), None);
    }

    #[test]
    fn rebase_strips_prefix() {
        let ns = MountNs {
            id: 1,
            overlays: Mutex::new(BTreeMap::new()),
        };
        ns.bind("/tmp/newroot/usr", new_tmpfs_root()).unwrap();
        ns.rebase("/tmp/newroot");
        assert!(ns.lookup("/usr").is_some());
        assert!(ns.lookup("/tmp/newroot/usr").is_none());
    }
}
