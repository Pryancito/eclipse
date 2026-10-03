//! File handle for process

use alloc::{boxed::Box, string::String, sync::Arc};

use async_trait::async_trait;
use kernel_hal::sync::RwLock;

use rcore_fs::vfs::{FileType, FsError, INode, Metadata, PollStatus, Timespec};
use zircon_object::object::*;
use zircon_object::vm::{pages, VmObject, PAGE_SIZE};

use super::FileLike;
use crate::error::{LxError, LxResult};

bitflags::bitflags! {
    /// File open flags
    pub struct OpenFlags: usize {
        /// read only
        const RDONLY = 0;
        /// write only
        const WRONLY = 1;
        /// read write
        const RDWR = 2;
        /// create file if it does not exist
        const CREATE = 1 << 6;
        /// error if CREATE and the file exists
        const EXCLUSIVE = 1 << 7;
        /// do not make this the controlling terminal
        const NOCTTY = 1 << 8;
        /// truncate file upon open
        const TRUNCATE = 1 << 9;
        /// append on each write
        const APPEND = 1 << 10;
        /// non block open
        const NON_BLOCK = 1 << 11;
        /// a write returns once the data is on the disk (`O_DSYNC`)
        const DSYNC = 1 << 12;
        /// signal-driven I/O
        const ASYNC = 1 << 13;
        /// bypass the page cache
        const DIRECT = 1 << 14;
        /// allow a size that does not fit a 32-bit `off_t`; meaningless here
        const LARGEFILE = 1 << 15;
        /// `ENOTDIR` unless the path names a directory
        const DIRECTORY = 1 << 16;
        /// `ELOOP` if the last component of the path is a symbolic link
        const NOFOLLOW = 1 << 17;
        /// do not update the access time on read
        const NOATIME = 1 << 18;
        /// close on exec
        const CLOEXEC = 1 << 19;
        /// a write returns once data AND metadata are on the disk.
        /// `O_SYNC` is `__O_SYNC | O_DSYNC` upstream, both bits together.
        const SYNC = (1 << 20) | (1 << 12);
        /// `O_PATH`: open the FILE ITSELF, not its contents. The descriptor
        /// names a place in the tree and nothing more -- it is neither
        /// readable nor writable -- and what it is for is `fstat`, `fchdir`
        /// and standing in as the `dirfd` of an `*at` call.
        ///
        /// It is also what `ps` opens: procps-ng's `look_up_our_self`
        /// (`library/readproc.c`) does
        /// `open("/proc/self", O_PATH|O_DIRECTORY)` and, when that fails,
        /// prints "Error, do this: mount -t proc proc /proc" and `_exit(47)`.
        /// Refusing the flag therefore took down every procps tool -- `ps`,
        /// `top`, `free`, `vmstat`, `w`, `uptime`, `pgrep`, `pkill` -- with a
        /// message about a filesystem that was mounted all along.
        const PATH = 1 << 21;
    }
}

impl OpenFlags {
    /// `O_PATH`: the descriptor names a place in the tree, not an open file.
    pub fn is_path(self) -> bool {
        self.contains(Self::PATH)
    }
    /// check if the OpenFlags is readable
    ///
    /// Never under `O_PATH`, whatever the access mode says. `O_RDONLY` is
    /// `0`, so an `O_PATH` open that named no mode at all would otherwise
    /// read as readable, and `read(2)` on such a descriptor is `EBADF` --
    /// which every reader here already answers off this one question.
    pub fn readable(self) -> bool {
        if self.is_path() {
            return false;
        }
        let b = self.bits() & 0b11;
        b == Self::RDONLY.bits() || b == Self::RDWR.bits()
    }
    /// check if the OpenFlags is writable
    pub fn writable(self) -> bool {
        if self.is_path() {
            return false;
        }
        let b = self.bits() & 0b11;
        b == Self::WRONLY.bits() || b == Self::RDWR.bits()
    }
    /// check if the OpenFlags caontains append
    pub fn is_append(self) -> bool {
        self.contains(Self::APPEND)
    }
    /// check if the OpenFlags caontains non-block
    pub fn non_block(self) -> bool {
        self.contains(Self::NON_BLOCK)
    }
    /// close on exec
    pub fn close_on_exec(self) -> bool {
        self.contains(Self::CLOEXEC)
    }
    /// The bits `fcntl(F_SETFL)` may change: `SETFL_MASK` in `fs/fcntl.c`,
    /// the status flags. The access mode, the creation flags and `O_CLOEXEC`
    /// (per descriptor, `F_SETFD`) are not among them. Linux's mask is
    /// `O_APPEND|O_ASYNC|O_DIRECT|O_NOATIME|O_NONBLOCK`; without `DIRECT` and
    /// `NOATIME` here, `F_SETFL` could neither set nor clear them.
    fn setfl_mask() -> Self {
        Self::APPEND | Self::NON_BLOCK | Self::ASYNC | Self::DIRECT | Self::NOATIME
    }
    /// What an open file's flags become after `fcntl(F_SETFL, requested)`.
    /// Linux (`setfl`) copies only `SETFL_MASK` out of the argument and keeps
    /// everything else as it was, so `F_SETFL(O_NONBLOCK)` on a read-write
    /// file leaves its `O_CLOEXEC` record alone and `F_SETFL(O_CLOEXEC)`
    /// sets nothing. Handing the raw argument to `set_flags`, which copies
    /// that record, did both.
    pub fn after_setfl(current: Self, requested: Self) -> Self {
        (current - Self::setfl_mask()) | (requested & Self::setfl_mask())
    }
    /// Take from `requested` the bits [`FileLike::set_flags`] accepts: the
    /// status flags, and the creation-time `O_CLOEXEC` record that the dup
    /// paths clear before installing a copy. Every `set_flags` used to spell
    /// this out by hand, and the ones that spelled nothing (eventfd,
    /// signalfd, timerfd, inotify) silently dropped the request, so an
    /// `fcntl(F_SETFL, O_NONBLOCK)` on them changed nothing and the next read
    /// with nothing pending blocked for good.
    pub fn take_settable(&mut self, requested: Self) {
        for bit in [
            Self::APPEND,
            Self::NON_BLOCK,
            Self::ASYNC,
            Self::DIRECT,
            Self::NOATIME,
            Self::CLOEXEC,
        ] {
            self.set(bit, requested.contains(bit));
        }
    }
}

bitflags::bitflags! {
    pub struct PollEvents: u16 {
        /// There is data to read.
        const IN = 0x0001;
        /// There is urgent data to read. Nothing here has out-of-band data,
        /// so it is accepted and never reported.
        const PRI = 0x0002;
        /// Writing is now possible.
        const OUT = 0x0004;
        /// Error condition (return only)
        const ERR = 0x0008;
        /// Hang up (return only)
        const HUP = 0x0010;
        /// Invalid request: fd not open (return only)
        const INVAL = 0x0020;
        /// Normal data may be read: the same condition as `IN`, under the
        /// name System V streams gave it. Linux reports the two together
        /// (`EPOLLIN | EPOLLRDNORM` in every `poll` method), so a caller
        /// that asks for this one alone is woken and told.
        const RDNORM = 0x0040;
        /// Priority band data may be read. Never reported, like `PRI`.
        const RDBAND = 0x0080;
        /// Normal data may be written: `OUT` under its streams name, and
        /// reported with it.
        const WRNORM = 0x0100;
        /// Priority data may be written. Never reported.
        const WRBAND = 0x0200;
    }
}

impl PollEvents {
    /// Every bit that asks about reading: `IN` and its streams alias, plus
    /// the urgent-data pair, which a poller sets to be woken by the same
    /// readable transition.
    pub const READ_INTEREST: Self = Self::from_bits_truncate(
        Self::IN.bits() | Self::PRI.bits() | Self::RDNORM.bits() | Self::RDBAND.bits(),
    );
    /// Every bit that asks about writing. See [`READ_INTEREST`](Self::READ_INTEREST).
    pub const WRITE_INTEREST: Self =
        Self::from_bits_truncate(Self::OUT.bits() | Self::WRNORM.bits() | Self::WRBAND.bits());

    /// Whether this interest set asks about reading. Every reader that used
    /// to spell this `contains(IN)` answered "no" to a set of `RDNORM`
    /// alone, so a pidfd, an eventfd or a syncobj polled under the streams
    /// name was never reported and never woken.
    pub fn wants_read(self) -> bool {
        self.intersects(Self::READ_INTEREST)
    }

    /// Whether this interest set asks about writing. See
    /// [`wants_read`](Self::wants_read).
    pub fn wants_write(self) -> bool {
        self.intersects(Self::WRITE_INTEREST)
    }

    /// The readiness a [`PollStatus`] stands for, as the bits every Linux
    /// `poll` method returns for it before the caller's mask is applied:
    /// `IN | RDNORM` when there is something to read, `OUT | WRNORM` when a
    /// write would not block, `ERR` and `HUP` as they are.
    ///
    /// The `RDNORM`/`WRNORM` half used to be missing, so a `pollfd` asking
    /// for `POLLRDNORM` alone (the shape Windows-born code and the streams
    /// manuals use) never had its `revents` set: `poll` slept through the
    /// data and `epoll_wait` never reported the fd.
    pub fn ready(status: &PollStatus) -> Self {
        let mut ready = Self::empty();
        if status.read {
            ready |= Self::IN | Self::RDNORM;
        }
        if status.write {
            ready |= Self::OUT | Self::WRNORM;
        }
        if status.error {
            ready |= Self::ERR;
        }
        if status.hangup {
            ready |= Self::HUP;
        }
        ready
    }

    /// What `poll(2)` writes to `revents` for a status, given the `events`
    /// the caller asked for: [`ready`](Self::ready) masked by the request,
    /// except that `ERR` and `HUP` are reported whether asked for or not
    /// (`poll(2)`: "these bits are output only"). `epoll` does the same by
    /// forcing the two into every stored mask.
    pub fn revents(status: &PollStatus, events: Self) -> Self {
        Self::ready(status) & (events | Self::ERR | Self::HUP)
    }
}

/// file seek type
#[derive(Debug)]
pub enum SeekFrom {
    /// seek from start point
    Start(u64),
    /// seek from end
    End(i64),
    /// seek from current
    Current(i64),
}

/// file inner mut data struct
#[derive(Clone)]
struct FileInner {
    /// content offset on read/write
    offset: u64,
    /// file open options
    flags: OpenFlags,
    /// file INode
    inode: Arc<dyn INode>,
}

/// file implement struct
pub struct File {
    /// object base
    base: KObjectBase,
    /// file path
    path: String,
    /// file inner mut data
    inner: RwLock<FileInner>,
}

impl_kobject!(File);

impl Drop for File {
    /// The last close of this open file description: the `flock(2)` locks
    /// it holds go with it (see `fs::flock`), which is what lets a process
    /// that took `LOCK_EX` and then exited, or just closed the fd, stop
    /// holding the file.
    fn drop(&mut self) {
        crate::fs::flock::release_owner(self as *const File as usize);
    }
}

/// Demand-paging source for a file-backed `mmap` (see [`get_vmo`]).
///
/// Reads one page from the backing inode the first time that page is touched,
/// so a large mapping (e.g. `libLLVM.so`) is paged in lazily instead of being
/// read into memory in full at map time.
///
/// [`get_vmo`]: File::get_vmo
struct FileFrameFiller {
    inode: Arc<dyn INode>,
    /// File offset that VMO offset 0 maps to.
    file_offset: usize,
    /// Upper bound on the readable bytes from `file_offset` (the mapping
    /// length for a private snapshot; unbounded for the page cache). The
    /// readable length itself is the inode's CURRENT size, read at fill time:
    /// a length frozen at creation left every page a file gained afterwards
    /// (ftruncate, write past EOF) reading as zero forever.
    max_len: usize,
}

/// Per-inode file-VMO registry — the PAGE CACHE. Entry: `(cache VMO, weak
/// inode handle for pruning, ever_shared)`.
///
/// One VMO per inode serves BOTH mapping flavours: `MAP_SHARED` maps it
/// directly (stores propagate between processes), and `MAP_PRIVATE` creates a
/// borrower over it (`VmObject::new_paged_borrowing`) so clean pages are the
/// cache's frames and only dirtied pages are copied. `ever_shared` records
/// whether a MAP_SHARED mapping was ever handed out: only then can the cache's
/// pages differ from the file, so only then is eviction-time writeback needed
/// -- without the flag every library file would be rewritten with identical
/// bytes when its last user exits.
type SharedVmoMap = alloc::collections::BTreeMap<
    (usize, usize),
    (Arc<VmObject>, alloc::sync::Weak<dyn INode>, bool),
>;

/// Registry key for a file: `(filesystem identity, inode number)`.
///
/// The old key -- the inode ARC's data pointer -- deduplicated nothing across
/// `open()`s: the VFS builds a fresh `Arc<dyn INode>` per lookup, so eight
/// processes opening the same library produced eight distinct "shared" caches
/// (measured: 16 PagedSource VMOs / 512 MiB for 8 readers of one 32 MiB file).
/// It only ever worked when the SAME fd was inherited or SCM_RIGHTS-passed,
/// which is why wl_shm masked it. Files whose filesystem reports inode 0 fall
/// back to the Arc pointer: no cross-open dedup, but never a false merge.
///
/// Only a regular file can have a page cache, so `fs()` is asked of regular
/// files only. Every `read(2)`/`write(2)` consults the cache registry through
/// this key, and the inode behind a tty, a pipe, a socket or `/dev/null` may
/// not have a file system at all: the vendored default returns the `no_fs`
/// placeholder (recognised here), but `rcore-fs-devfs` overrides `fs()` with
/// `unimplemented!()` -- the `write(2)` to `/dev/null` that halted the
/// desktop. Anything that is not a regular file keys by its Arc pointer and
/// is never asked.
fn cache_key(inode: &Arc<dyn INode>) -> (usize, usize) {
    let by_pointer = (Arc::as_ptr(inode) as *const () as usize, usize::MAX);
    match inode.metadata() {
        Ok(md) if md.inode != 0 && md.type_ == FileType::File => {
            let fs = inode.fs();
            if rcore_fs::vfs::is_no_fs(&fs) {
                by_pointer
            } else {
                (Arc::as_ptr(&fs) as *const () as usize, md.inode)
            }
        }
        _ => by_pointer,
    }
}

lazy_static::lazy_static! {
    /// Per-inode shared VMOs for `MAP_SHARED` file mappings, keyed by the
    /// inode's Arc data pointer. Every mapper of the same file gets the SAME
    /// VmObject, so stores by one process are visible to all others — the
    /// wl_shm contract. Without this each mmap produced an independent
    /// demand-paged snapshot: foot rendered its terminal into its own copy
    /// while labwc composited an all-zeros copy — every client window and the
    /// lunarbg background showed pure black.
    ///
    /// The VMO is held with a STRONG ref, anchored to the *inode's* lifetime
    /// (a Weak<INode> alongside it), NOT to any one mapping's. This is what
    /// makes the wl_keyboard keymap work: wlroots writes the keymap into a
    /// memfd via mmap(MAP_SHARED)+memcpy and then *munmaps and closes the write
    /// fd* before sending the read fd — with a Weak VMO the shared pages would
    /// be freed on that munmap, so the client's later read (and the MAP_PRIVATE
    /// COW off this VMO) would see zeros and foot would SIGSEGV on the empty
    /// keymap. There is no MAP_SHARED->inode writeback path (msync is a no-op),
    /// so the shared VMO *is* the file's storage for as long as an fd keeps the
    /// inode alive. Dead-inode entries are pruned on every access, freeing the
    /// VMO (and its frames) once the last fd closes.
    ///
    /// LOCKING RULE: this is a ticket spinlock, held with interrupts off and
    /// not re-entrant. Under it, do refcount reads, pointer compares and map
    /// surgery -- nothing else. No filesystem call, no VMO creation or resize,
    /// and no VMO drop: see `take_prunable`.
    static ref SHARED_FILE_VMOS: lock::Mutex<SharedVmoMap> =
        lock::Mutex::new(alloc::collections::BTreeMap::new());
}

/// Entry count and committed bytes held by the MAP_SHARED file-VMO registry.
///
/// These VMOs are held with a STRONG ref and are NOT attributable to any
/// process: once every mapper has exited, the pages stay committed for as long
/// as the backing inode is alive. If a filesystem caches its inodes, "alive"
/// means forever, and this registry becomes a one-way memory sink. This is the
/// number that turns "physical RAM is exhausted and no process accounts for it"
/// into a specific answer, so `/proc/memhogs` reports it next to the per-process
/// totals.
pub fn shared_file_vmo_stats() -> (usize, u64) {
    let registry = SHARED_FILE_VMOS.lock();
    let mut bytes = 0u64;
    for (vmo, _, _) in registry.values() {
        let pages = vmo.len() / PAGE_SIZE;
        bytes += (vmo.committed_pages_in_range(0, pages) * PAGE_SIZE) as u64;
    }
    (registry.len(), bytes)
}

/// Reference-count histogram of the registry, for `/proc/memhogs`.
///
/// Returns `(entries, sole_vmo_holder, inode_refs_min, inode_refs_max)`:
/// how many entries exist, how many have the registry as the ONLY holder of
/// their VMO (i.e. nothing maps them any more), and the range of strong
/// references on their inodes.
///
/// These two counts decide what a correct eviction rule can look like. The
/// documented rule -- "pruned once the last fd closes" -- is unimplementable as
/// written, because the registry's own entry keeps the inode alive:
///
///   registry --Arc--> VmObject --Arc--> FileFrameFiller --Arc--> INode
///
/// so `inode_weak.strong_count()` can never reach zero while the entry exists.
/// What matters is how much of the count that cycle accounts for.
pub fn shared_file_vmo_refs() -> (usize, usize, usize, usize) {
    let registry = SHARED_FILE_VMOS.lock();
    let mut sole = 0;
    let mut lo = usize::MAX;
    let mut hi = 0;
    for (vmo, inode_weak, _) in registry.values() {
        if Arc::strong_count(vmo) == 1 {
            sole += 1;
        }
        let n = inode_weak.strong_count();
        lo = lo.min(n);
        hi = hi.max(n);
    }
    (
        registry.len(),
        sole,
        if lo == usize::MAX { 0 } else { lo },
        hi,
    )
}

/// Evict registry entries that nothing can reach any more.
///
/// The rule this used to implement -- "drop entries whose backing inode has
/// been freed (all fds closed)" -- was unimplementable as written, because the
/// entry itself keeps the inode alive:
///
///   registry --Arc--> VmObject --Arc--> FileFrameFiller --Arc--> INode
///
/// `inode_weak.strong_count()` therefore never reached 0 and NOTHING was ever
/// pruned. Measured in QEMU: six `mmap(MAP_SHARED)` + munmap + close + unlink
/// cycles over an 8 MiB file left six entries holding 48 MiB, with no mapper
/// left and inode strong refs `1..1` -- that single reference being the cycle's
/// own. Thirty cycles held 240 MiB. Physical use tracked it exactly, and the
/// pages belong to no process, so nothing in per-process accounting showed
/// them. This is what exhausted RAM in the XFCE session (X11 MIT-SHM, Wayland
/// shm pools and font caches all map shared): `frame_alloc FAILED: 2561 MiB
/// used / 2561 MiB managed`, about two minutes in.
///
/// The cycle contributes exactly ONE strong inode reference, which is what
/// makes a correct rule expressible:
///
///   keep while  `Arc::strong_count(vmo) > 1`      something still maps it
///          or   `inode_weak.strong_count() > 1`   some fd is still open
///
/// Both halves matter. The second is what makes the wl_keyboard keymap work --
/// a writer that mmaps, writes, munmaps and closes its fd while the reader's fd
/// is already open keeps the entry, which is the case the strong ref was added
/// for in the first place.
///
/// Get-or-create the per-inode page-cache VMO, covering at least
/// `offset + len` bytes (grown to the file size). `mark_shared` records that a
/// MAP_SHARED mapping was handed out, which is what arms eviction-time
/// writeback. Returns `None` when an existing cache is too short for the
/// requested window (file grew after creation) -- the caller falls back to a
/// private snapshot exactly as before.
fn inode_cache_vmo(
    inode: &Arc<dyn INode>,
    path: &str,
    file_size: usize,
    offset: usize,
    len: usize,
    mark_shared: bool,
) -> Option<Arc<VmObject>> {
    // Every arm below adds `offset + len`, and `offset` is a byte offset that
    // came from userspace via `mmap`. Unchecked, the addition is a kernel panic
    // in debug and a wrap in release -- and a wrap is worse than the panic here,
    // because it makes the "does the cache cover this window?" test below answer
    // yes for a window the cache does not cover at all. The syscall layer
    // (`validate_mmap_offset`) rejects such an offset now, but `get_vmo_shared`
    // is a trait method, so the arithmetic defends itself too.
    let end = offset.checked_add(len)?;
    let key = cache_key(inode);

    // Pass 1: find a usable cache and collect what is prunable. Everything in
    // this block is a pointer compare or a refcount read -- see
    // `take_prunable` for why nothing heavier may happen under this lock.
    let (hit, evicted) = {
        let mut registry = SHARED_FILE_VMOS.lock();
        let evicted = take_prunable(&mut registry);
        let hit = registry
            .get_mut(&key)
            .map(|(vmo, inode_weak, ever_shared)| {
                if mark_shared {
                    *ever_shared = true;
                }
                // Point the weak handle at the LATEST opener's Arc: the one
                // captured at creation dies with its fd even while other opens
                // keep the file busy, and eviction-time writeback needs a live
                // inode to write to.
                *inode_weak = Arc::downgrade(inode);
                vmo.clone()
            });
        (hit, evicted)
    };
    finish_eviction(evicted);

    if let Some(vmo) = hit {
        // The file grew since the cache was made: grow the cache, so the new
        // window shares the very same pages as every earlier mapper and
        // `read(2)`. The old fallback (a private snapshot) was what made a
        // mapping of a grown memfd read zeros for its head.
        return grow_cache_vmo(&vmo, end).then_some(vmo);
    }

    // Miss. Build the cache VMO with NO lock held: `new_paged_cache` allocates,
    // and the registry lock runs with interrupts off.
    let vmo_len = file_size.max(end);
    let source: Arc<dyn zircon_object::vm::FrameFiller> = Arc::new(FileFrameFiller {
        inode: inode.clone(),
        file_offset: 0,
        max_len: usize::MAX,
    });
    let vmo = VmObject::new_paged_cache(pages(vmo_len), source);
    vmo.set_name(path);

    // Pass 2: publish it, unless another CPU created one for this inode while
    // we were building ours. Whoever loses hands their VMO back as `loser` and
    // drops it below, outside the lock -- freeing its frames goes through the
    // heap, and that is exactly the dealloc the deadlock report caught a
    // registry holder sitting in.
    let (winner, loser) = {
        let mut registry = SHARED_FILE_VMOS.lock();
        match registry.get_mut(&key) {
            Some((cached, inode_weak, ever_shared)) => {
                if mark_shared {
                    *ever_shared = true;
                }
                *inode_weak = Arc::downgrade(inode);
                (cached.clone(), Some(vmo))
            }
            None => {
                registry.insert(key, (vmo.clone(), Arc::downgrade(inode), mark_shared));
                (vmo, None)
            }
        }
    };
    drop(loser);

    grow_cache_vmo(&winner, end).then_some(winner)
}

/// Make `vmo` cover at least `end` bytes, growing only.
///
/// Called with no registry lock held, so two CPUs can be here at once for the
/// same VMO: the one that wants less can shrink the object under the one that
/// wants more (`set_len` sets an exact length). Re-reading after the resize is
/// what makes that benign -- the loser simply asks again. Bounded, because a
/// caller that keeps losing is being raced by `cache_truncate`, and then the
/// honest answer is "this cache does not cover your window" and the caller
/// falls back to a private snapshot.
fn grow_cache_vmo(vmo: &Arc<VmObject>, end: usize) -> bool {
    for _ in 0..8 {
        if vmo.len() >= end {
            return true;
        }
        if vmo.set_len(end).is_err() {
            return false;
        }
    }
    vmo.len() >= end
}

/// The page cache of `inode`, if one exists. Never creates one.
fn cache_vmo_of(inode: &Arc<dyn INode>) -> Option<Arc<VmObject>> {
    SHARED_FILE_VMOS
        .lock()
        .get(&cache_key(inode))
        .map(|(vmo, _, _)| vmo.clone())
}

/// `read(2)`/`pread(2)` on a file with a live page cache: bytes already
/// faulted into the cache -- and possibly written through a `MAP_SHARED`
/// mapping -- must be what the read returns. `buf` holds what the inode gave
/// for `[offset, offset + buf.len())`; every page the cache has committed
/// overrides it. Uncommitted pages are the inode's, so they are left alone.
///
/// Without this, mmap and read were two copies of the file: a store through
/// a mapping was invisible to `pread` until the cache was evicted
/// (`firefox-probe`: "pread() sees stores made through a mapping").
pub(crate) fn cache_overlay_read(inode: &Arc<dyn INode>, offset: usize, buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    let Some(vmo) = cache_vmo_of(inode) else {
        return;
    };
    let end = offset + buf.len();
    let last = (end - 1) / PAGE_SIZE;
    for page in offset / PAGE_SIZE..=last {
        if page * PAGE_SIZE >= vmo.len() || vmo.committed_paddr(page).is_none() {
            continue;
        }
        let s = (page * PAGE_SIZE).max(offset);
        let e = ((page + 1) * PAGE_SIZE).min(end);
        let _ = vmo.read(s, &mut buf[s - offset..e - offset]);
    }
}

/// `write(2)`/`pwrite(2)` on a file with a live page cache: the bytes went to
/// the inode; copy them into every cache page that is already committed, so
/// mappings that faulted those pages see the write. Pages not yet committed
/// fill from the inode on their first fault and need nothing.
pub(crate) fn cache_overlay_write(inode: &Arc<dyn INode>, offset: usize, buf: &[u8]) {
    if buf.is_empty() {
        return;
    }
    let Some(vmo) = cache_vmo_of(inode) else {
        return;
    };
    let end = offset + buf.len();
    let last = (end - 1) / PAGE_SIZE;
    for page in offset / PAGE_SIZE..=last {
        if page * PAGE_SIZE >= vmo.len() || vmo.committed_paddr(page).is_none() {
            continue;
        }
        let s = (page * PAGE_SIZE).max(offset);
        let e = ((page + 1) * PAGE_SIZE).min(end);
        let _ = vmo.write(s, &buf[s - offset..e - offset]);
    }
}

/// The file was resized to `new_len` (ftruncate, O_TRUNC, truncate): drop
/// every cached page past the new end, and zero the tail of the last kept
/// page, exactly as the inode does. Otherwise the cache kept serving the old
/// bytes to mappings, and -- worse -- eviction-time writeback copied them
/// back over a file that had been rewritten in the meantime
/// (`firefox-probe`: "O_TRUNC rewrite of a once-mapped file survives").
pub fn cache_truncate(inode: &Arc<dyn INode>, new_len: usize) {
    let Some(vmo) = cache_vmo_of(inode) else {
        return;
    };
    let keep = new_len.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    if keep < vmo.len() {
        let _ = vmo.decommit(keep, vmo.len() - keep);
    }
    let tail = keep - new_len;
    if tail > 0 && vmo.committed_paddr(new_len / PAGE_SIZE).is_some() {
        let zeros = [0u8; PAGE_SIZE];
        let _ = vmo.write(new_len, &zeros[..tail]);
    }
}

/// Entries the registry no longer needs, REMOVED from it but still alive.
///
/// Returned by `take_prunable` so that `finish_eviction` can do the work the
/// registry lock must never cover.
type EvictedVmos = alloc::vec::Vec<(Arc<VmObject>, alloc::sync::Weak<dyn INode>, bool)>;

/// Take every entry whose backing inode has been freed (all fds closed) OUT of
/// the registry, without touching it further.
///
/// Called under the registry lock, which is a ticket spinlock held with
/// interrupts off. That is why this does nothing but compare refcounts and
/// move entries out: the version that pruned in place called
/// `writeback_shared_vmo` -- `inode.write_at`, a whole filesystem write, plus
/// the page faults `vmo.read` triggers back into `inode.read_at` -- from
/// inside the critical section, and dropped the evicted VMOs (and all their
/// frames) there too. That is the KERNEL STOP Moebius photographed: the holder
/// of this lock parked in `GlobalAlloc::dealloc` while every other CPU spun on
/// `cache_vmo_of`. It is also a genuine AB-BA, because a filesystem write
/// takes inode locks that `read(2)`/`write(2)` already hold when they come the
/// other way round through `cache_overlay_read` / `cache_overlay_write`.
///
/// The returned vector is the only allocation left here, and only when there
/// IS something to evict: an empty `collect` does not allocate.
fn take_prunable(registry: &mut SharedVmoMap) -> EvictedVmos {
    let dead: alloc::vec::Vec<(usize, usize)> = registry
        .iter()
        .filter(|(_, (vmo, inode_weak, _))| {
            // The cycle contributes exactly ONE strong reference to each, so
            // an entry is live while something still maps the VMO or some fd
            // is still open. Both halves matter -- see `inode_cache_vmo`.
            Arc::strong_count(vmo) <= 1 && inode_weak.strong_count() <= 1
        })
        .map(|(key, _)| *key)
        .collect();
    dead.into_iter()
        .filter_map(|key| registry.remove(&key))
        .collect()
}

/// Write back and drop what `take_prunable` removed. MUST be called with the
/// registry lock down.
fn finish_eviction(evicted: EvictedVmos) {
    for (vmo, inode_weak, ever_shared) in evicted {
        // Writeback is only for durable (still-linked) files so a later open
        // sees MAP_SHARED writes.
        //
        // memfd / unlinked shm (`nlinks == 0`) dies with the inode --
        // densifying into ramfs first doubles residency (VMO frames still held
        // + one heap 4KiB block per page) and was the desktop-start OOM
        // (`4096B x ~99000`, leaktrace -> `PagedBytes::write_at`).
        if ever_shared {
            if let Some(inode) = inode_weak.upgrade() {
                let nlinks = inode.metadata().map(|m| m.nlinks).unwrap_or(0);
                if nlinks > 0 {
                    writeback_shared_vmo(&vmo, &inode);
                }
            }
        }
        // ... and the frames go back to the allocator here, lock-free.
        drop(vmo);
    }
}

/// Flush a shared VMO's committed pages to its inode before the VMO is dropped.
///
/// Bound by the inode's CURRENT size: a shared VMO is rounded up to whole
/// pages and may be longer than the file (`vmo_len = file_size.max(offset+len)`),
/// and writing those pages back would silently EXTEND the file.
fn writeback_shared_vmo(vmo: &Arc<VmObject>, inode: &Arc<dyn INode>) {
    let size = match inode.metadata() {
        Ok(m) => m.size,
        Err(_) => return,
    };
    if size == 0 {
        return;
    }
    let mut buf = alloc::vec![0u8; PAGE_SIZE];
    for idx in 0..vmo.len() / PAGE_SIZE {
        let offset = idx * PAGE_SIZE;
        if offset >= size {
            break;
        }
        // Only pages that were actually faulted in can differ from the file.
        if vmo.committed_pages_in_range(idx, idx + 1) == 0 {
            continue;
        }
        let n = PAGE_SIZE.min(size - offset);
        if vmo.read(offset, &mut buf[..n]).is_err() {
            continue;
        }
        // Keep ramfs holes as holes: writing zeros would allocate a 4KiB heap
        // block per page and recreate the densification OOM on sparse pools.
        if buf[..n].iter().all(|&b| b == 0) {
            continue;
        }
        // Best effort: a read-only filesystem, or an inode that no longer
        // accepts writes, must not turn eviction into a failure.
        let _ = inode.write_at(offset, &buf[..n]);
    }
}

/// Write back a MAP_SHARED file VMO to its inode (Stage A `msync`).
///
/// Looks the VMO up in the shared-file registry and flushes committed pages.
/// No-op when the VMO is anonymous, private, or not registered — callers can
/// invoke this on every mapping in an `msync` range without filtering first.
pub fn sync_shared_file_vmo(vmo: &Arc<VmObject>) {
    if !vmo.is_shared_object() && !vmo.is_file_backed() {
        return;
    }
    let (vmo, inode) = {
        let registry = SHARED_FILE_VMOS.lock();
        let mut found = None;
        for (cached, inode_weak, _) in registry.values() {
            if Arc::ptr_eq(cached, vmo) {
                if let Some(inode) = inode_weak.upgrade() {
                    found = Some((cached.clone(), inode));
                }
                break;
            }
        }
        match found {
            Some(pair) => pair,
            None => return,
        }
    };
    writeback_shared_vmo(&vmo, &inode);
}

impl zircon_object::vm::FrameFiller for FileFrameFiller {
    fn source_len(&self) -> usize {
        let size = self.inode.metadata().map(|m| m.size).unwrap_or(0);
        size.saturating_sub(self.file_offset).min(self.max_len)
    }

    fn fill_page(&self, offset: usize, buf: &mut [u8]) {
        self.fill_range(offset, buf);
    }

    /// One `read_at` for the whole range (the inode reads as much as it can
    /// per call; the loop only covers a short read). This is the read a page
    /// fault's 16-page window costs now: one filesystem walk and one block
    /// cache lookup, instead of one per page.
    fn fill_range(&self, offset: usize, buf: &mut [u8]) {
        let source_len = self.source_len();
        if offset >= source_len {
            return;
        }
        let want = (source_len - offset).min(buf.len());
        let file_pos = self.file_offset + offset;
        let mut done = 0;
        while done < want {
            match self.inode.read_at(file_pos + done, &mut buf[done..want]) {
                Ok(0) => break,
                Ok(n) => done += n,
                // A read error mid-mapping leaves the rest zero-filled; the
                // faulting access proceeds rather than wedging the kernel.
                Err(_) => break,
            }
        }
    }
}

// NOTE: the former `VmoFrameFiller` (a MAP_PRIVATE copy sourced from a live
// MAP_SHARED VMO — the wl_keyboard keymap coherence path) is gone: private
// mappings now BORROW from the same per-inode cache VMO the shared mappings
// write into, so they read those writes directly instead of copying them.

impl FileInner {
    /// write to file
    fn write(&mut self, buf: &[u8]) -> LxResult<usize> {
        let offset = if self.flags.is_append() {
            self.inode.metadata()?.size as u64
        } else {
            self.offset
        };
        let len = self.write_at(offset, buf)?;
        self.offset = offset + len as u64;
        Ok(len)
    }

    /// write to file at given offset
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> LxResult<usize> {
        if !self.flags.writable() {
            return Err(LxError::EBADF);
        }
        // A memfd sealed against writes refuses them here, at the one
        // chokepoint every write(2)/pwrite(2)/writev(2) passes through
        // (fcntl(2), F_SEAL_WRITE / F_SEAL_FUTURE_WRITE).
        if !crate::fs::memfd_write_allowed(&self.inode) {
            return Err(LxError::EPERM);
        }
        // `/dev/dsp`: the OSS write needs the fd's O_NONBLOCK, which the
        // `INode::write_at` contract cannot carry (the same reason the ALSA
        // PCM ioctl is routed with its flags in `File::ioctl`).
        use super::devfs::DspDev;
        let n = if let Some(dsp) = self.inode.downcast_ref::<DspDev>() {
            dsp.write_pcm(buf, self.flags.non_block())
        } else {
            self.inode.write_at(offset as usize, buf)
        }
        .map_err(|e| fs_grow_error(&self.inode, e))?;
        cache_overlay_write(&self.inode, offset as usize, &buf[..n]);
        Ok(n)
    }
}

/// Errno for a failed write/resize. A regular file that cannot grow is
/// ENOSPC, as on Linux. The blanket `FsError -> LxError` table says ENOMEM,
/// which is right for the device nodes that reuse `NoDeviceSpace` for a
/// failed buffer allocation (DRM CREATE_DUMB) but wrong for a file: a writer
/// told "out of memory" backs off and retries, one told "no space left on
/// device" stops. Shared by write(2)/pwrite(2), ftruncate(2) and truncate(2).
pub fn fs_grow_error(inode: &Arc<dyn INode>, e: FsError) -> LxError {
    match e {
        FsError::NoDeviceSpace
            if matches!(inode.metadata().map(|m| m.type_), Ok(FileType::File)) =>
        {
            LxError::ENOSPC
        }
        e => e.into(),
    }
}

/// Say which file filled the disk. The root image is an SFS sized with ~40%
/// headroom over its payload, so when it fills the interesting fact is WHAT
/// filled it -- a runaway log, a browser cache, a leak in the allocator -- and
/// nothing else in the log answers that. Printed for the first few distinct
/// paths only; a writer that keeps retrying would otherwise flood the console.
fn report_enospc(path: &str, offset: u64, len: usize, size: Option<u64>) {
    use core::sync::atomic::{AtomicU64, Ordering};
    // One slot per distinct path (FNV-1a of the path; 0 = free), so a writer
    // retrying the same file does not use up the budget of the others.
    const SLOTS: usize = 8;
    static SEEN: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    let key = path.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    }) | 1;
    let fresh = SEEN.iter().all(|s| s.load(Ordering::Relaxed) != key)
        && SEEN.iter().any(|s| {
            s.compare_exchange(0, key, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        });
    if fresh {
        error!(
            "[enospc] write of {} bytes at offset {:#x} to {} refused: no space left on device              (file is {} bytes now)",
            len,
            offset,
            path,
            size.map_or_else(|| String::from("?"), |s| alloc::format!("{s}")),
        );
    }
}

impl File {
    /// create a file struct
    pub fn new(inode: Arc<dyn INode>, flags: OpenFlags, path: String) -> Arc<Self> {
        Arc::new(File {
            base: KObjectBase::new(),
            path,
            inner: RwLock::new(FileInner {
                offset: 0,
                flags,
                inode,
            }),
        })
    }

    /// Returns the file path.
    pub fn path(&self) -> &String {
        &self.path
    }

    /// True for a `pipe2` pipe or a FIFO node — not seekable / not
    /// `pread`/`pwrite`-able (`ESPIPE`).
    fn is_pipe_or_fifo(&self) -> bool {
        let inner = self.inner.read();
        inner.inode.downcast_ref::<super::pipe::Pipe>().is_some()
            || matches!(
                inner.inode.metadata(),
                Ok(m) if m.type_ == FileType::NamedPipe
            )
    }

    /// seek from given type and offset
    pub fn seek(&self, pos: SeekFrom) -> LxResult<u64> {
        // Pipes and FIFOs are not seekable (`ESPIPE`); `fallocate` already
        // refuses them the same way. Without this, `lseek(pipe_fd, 0, SEEK_SET)`
        // "succeeds" and advances a phantom offset that nothing else uses.
        if self.is_pipe_or_fifo() {
            return Err(LxError::ESPIPE);
        }
        let mut inner = self.inner.write();
        // Compute the new offset with checked arithmetic and reject results
        // that would be negative; otherwise a negative relative seek would wrap
        // to a huge `u64` and let later reads/writes use an out-of-range offset.
        let new_offset: i64 = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => (inner.inode.metadata()?.size as i64)
                .checked_add(offset)
                .ok_or(LxError::EINVAL)?,
            SeekFrom::Current(offset) => (inner.offset as i64)
                .checked_add(offset)
                .ok_or(LxError::EINVAL)?,
        };
        if new_offset < 0 {
            return Err(LxError::EINVAL);
        }
        inner.offset = new_offset as u64;
        Ok(inner.offset)
    }

    /// resize the file
    pub fn set_len(&self, len: u64) -> LxResult {
        let inner = self.inner.write();
        if !inner.flags.writable() {
            return Err(LxError::EBADF);
        }
        inner
            .inode
            .resize(len as usize)
            .map_err(|e| fs_grow_error(&inner.inode, e))?;
        cache_truncate(&inner.inode, len as usize);
        Ok(())
    }

    /// Sync all data and metadata
    pub fn sync_all(&self) -> LxResult {
        self.inner.read().inode.sync_all()?;
        Ok(())
    }

    /// Sync data (not include metadata)
    pub fn sync_data(&self) -> LxResult {
        self.inner.read().inode.sync_data()?;
        Ok(())
    }

    /// get metadata of file
    /// fstat
    pub fn metadata(&self) -> LxResult<Metadata> {
        Ok(self.inner.read().inode.metadata()?)
    }

    /// lookup the file following the link
    pub fn lookup_follow(&self, path: &str, max_follow: usize) -> LxResult<Arc<dyn INode>> {
        Ok(self.inner.read().inode.lookup_follow(path, max_follow)?)
    }

    /// get the name of dir entry
    pub fn read_entry(&self) -> LxResult<String> {
        Ok(self.read_entry_with_metadata()?.1)
    }

    /// Hand back the entry [`read_entry_with_metadata`](Self::read_entry_with_metadata)
    /// just returned, so the next read yields it again.
    ///
    /// `getdents64` reads an entry before it knows whether the record fits
    /// in what is left of the caller's buffer, and the one that did not fit
    /// used to be gone for good: consumed from the directory position, never
    /// written out, absent from every later call. A listing that took more
    /// than one buffer lost one name per buffer.
    pub fn unread_entry(&self) {
        let mut inner = self.inner.write();
        inner.offset = inner.offset.saturating_sub(1);
    }

    /// The directory position after the entry
    /// [`read_entry_with_metadata`](Self::read_entry_with_metadata) just
    /// returned: what `lseek(fd, pos, SEEK_SET)` takes to resume right after
    /// it, and therefore what `getdents64` has to report as that entry's
    /// `d_off`.
    ///
    /// It was reported as 0 for every entry. glibc's `telldir` is the `d_off`
    /// of the last entry `readdir` handed out, and `seekdir` is an `lseek` to
    /// it, so `seekdir(dir, telldir(dir))`, the way a program marks a place
    /// in a listing and comes back to it, rewound to the start instead.
    pub fn dir_position(&self) -> u64 {
        self.inner.read().offset
    }

    /// get the next directory entry and its metadata
    pub fn read_entry_with_metadata(&self) -> LxResult<(Metadata, String)> {
        let mut inner = self.inner.write();
        if !inner.flags.readable() {
            return Err(LxError::EBADF);
        }
        let offset = inner.offset as usize;
        match inner.inode.get_entry_with_metadata(offset) {
            Ok(entry) => {
                inner.offset += 1;
                Ok(entry)
            }
            Err(e) => {
                // `get_entry_with_metadata`'s default implementation resolves
                // the entry's metadata via `find(name)`, which can fail even
                // though the entry exists — e.g. the devfs root's ".." has no
                // parent, so `find("..")` returns EntryNotFound. Treating that
                // as end-of-directory truncates the listing (this made
                // `ls /dev` appear empty). Distinguish the two: if the name
                // still resolves, emit it with a synthetic directory metadata;
                // only a missing name means we reached the end.
                let name = inner.inode.get_entry(offset).map_err(|_| e)?;
                inner.offset += 1;
                let meta = Metadata {
                    dev: 0,
                    inode: 0,
                    size: 0,
                    blk_size: 0,
                    blocks: 0,
                    atime: Timespec { sec: 0, nsec: 0 },
                    mtime: Timespec { sec: 0, nsec: 0 },
                    ctime: Timespec { sec: 0, nsec: 0 },
                    type_: FileType::Dir,
                    mode: 0,
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                };
                Ok((meta, name))
            }
        }
    }

    /// get INode of this file
    pub fn inode(&self) -> Arc<dyn INode> {
        self.inner.read().inode.clone()
    }
}

#[async_trait]
impl FileLike for File {
    fn flags(&self) -> OpenFlags {
        self.inner.read().flags
    }

    /// A regular file or a directory has no `poll` operation on any disk
    /// filesystem, so `epoll_ctl` on one is `EPERM`. procfs and sysfs are
    /// the exception Linux itself makes: `sysfs_notify` and `mounts_poll`
    /// give their nodes a real poll, so those stay pollable here. Every
    /// other kind of node (a device, a FIFO, a socket) polls.
    fn can_epoll(&self) -> bool {
        let type_ = match self.inner.read().inode.metadata() {
            Ok(m) => m.type_,
            Err(_) => return true,
        };
        if type_ != FileType::File && type_ != FileType::Dir {
            return true;
        }
        self.path.starts_with("/proc/") || self.path.starts_with("/sys/")
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.inner.write().flags.take_settable(f);
        Ok(())
    }

    fn metadata(&self) -> LxResult<Metadata> {
        File::metadata(self)
    }

    fn seek(&self, pos: SeekFrom) -> LxResult<u64> {
        // Delegate to the inherent offset-tracking seek (UFCS avoids recursing
        // into this trait method).
        File::seek(self, pos)
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        let (offset, flags, inode) = {
            let inner = self.inner.read();
            (inner.offset, inner.flags, inner.inode.clone())
        };

        if !flags.readable() {
            return Err(LxError::EBADF);
        }

        let len = if !flags.non_block() {
            // block
            loop {
                match inode.read_at(offset as usize, buf) {
                    Ok(read_len) => break read_len,
                    Err(FsError::Again) => {
                        // DRM card fd: park on the shared eventbus directly.
                        // Going through INode::async_poll Box::pin's another
                        // async loop on top of this already-boxed File::read
                        // future — deep enough under page-flip storms to
                        // contribute to coroutine stack overflow at labwc start.
                        use super::devfs::DrmDev;
                        if let Some(drmdev) = inode.downcast_ref::<DrmDev>() {
                            let bus = drmdev.file_state().eventbus();
                            crate::sync::wait_for_event(bus, crate::sync::Event::READABLE).await?;
                        } else {
                            // Interruptible: the pipe's own future waits on a
                            // bus only a writer or a close ever fires, so a
                            // blocking `read` took no signal at all. See
                            // `process::interruptible`.
                            crate::process::interruptible(inode.async_poll()).await??;
                        }
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        } else {
            inode.read_at(offset as usize, buf)?
        };
        cache_overlay_read(&inode, offset as usize, &mut buf[..len]);

        let mut inner = self.inner.write();
        inner.offset += len as u64;
        Ok(len)
    }

    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        let mut inner = self.inner.write();
        // The offset `FileInner::write` will actually try (O_APPEND writes at
        // the current end, not at `inner.offset`).
        let offset = if inner.flags.is_append() {
            inner
                .inode
                .metadata()
                .map(|m| m.size as u64)
                .unwrap_or(inner.offset)
        } else {
            inner.offset
        };
        let r = inner.write(buf);
        if matches!(r, Err(LxError::ENOSPC)) {
            let size = inner.inode.metadata().ok().map(|m| m.size as u64);
            report_enospc(&self.path, offset, buf.len(), size);
        }
        r
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        // `pread` on a pipe is `ESPIPE`, same as `lseek` (Linux `pipe_read`
        // has no `FMODE_PREAD` path that succeeds).
        if self.is_pipe_or_fifo() {
            return Err(LxError::ESPIPE);
        }
        let (flags, inode) = {
            let inner = self.inner.read();
            (inner.flags, inner.inode.clone())
        };

        if !flags.readable() {
            return Err(LxError::EBADF);
        }

        if !flags.non_block() {
            // block
            loop {
                match inode.read_at(offset as usize, buf) {
                    Ok(read_len) => {
                        cache_overlay_read(&inode, offset as usize, &mut buf[..read_len]);
                        return Ok(read_len);
                    }
                    Err(FsError::Again) => {
                        use super::devfs::DrmDev;
                        if let Some(drmdev) = inode.downcast_ref::<DrmDev>() {
                            let bus = drmdev.file_state().eventbus();
                            crate::sync::wait_for_event(bus, crate::sync::Event::READABLE).await?;
                        } else {
                            // Interruptible: the pipe's own future waits on a
                            // bus only a writer or a close ever fires, so a
                            // blocking `read` took no signal at all. See
                            // `process::interruptible`.
                            crate::process::interruptible(inode.async_poll()).await??;
                        }
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }
        let len = inode.read_at(offset as usize, buf)?;
        cache_overlay_read(&inode, offset as usize, &mut buf[..len]);
        Ok(len)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> LxResult<usize> {
        // `pwrite` on a pipe is `ESPIPE`, same as `lseek`/`pread`.
        if self.is_pipe_or_fifo() {
            return Err(LxError::ESPIPE);
        }
        let mut inner = self.inner.write();
        let r = inner.write_at(offset, buf);
        if matches!(r, Err(LxError::ENOSPC)) {
            let size = inner.inode.metadata().ok().map(|m| m.size as u64);
            report_enospc(&self.path, offset, buf.len(), size);
        }
        r
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let inner = self.inner.read();
        // A FIFO node opened from the fs has no pipe-buffer / writer tracking
        // here, so a reader polling an empty FIFO (e.g. a control FIFO that
        // never gets a writer) would spin: the node reads as an
        // empty regular file (0 bytes = EOF) yet polls "readable". Treat it as
        // readable only when it actually holds bytes, so the reader blocks
        // instead of busy-looping on repeated 0-byte reads.
        //
        // Use metadata() best-effort: some fds (sockets, special devices) don't
        // implement it and return ENOSYS — those must fall through to the inode's
        // own poll(), NOT propagate the error (that regressed `poll()` on packet
        // sockets, e.g. udhcpc, to "Function not implemented").
        if let Ok(meta) = inner.inode.metadata() {
            if meta.type_ == FileType::NamedPipe {
                return Ok(PollStatus {
                    read: meta.size > 0,
                    write: true,
                    error: false,
                    hangup: false,
                });
            }
        }
        Ok(inner.inode.poll()?)
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let inode = self.inner.read().inode.clone();

        // See `poll`: special-case an empty FIFO so the reader blocks, but only
        // when metadata() is available — sockets/special devices return ENOSYS
        // and must fall through to the inode's own async_poll() rather than
        // failing the whole poll() syscall.
        if let Ok(meta) = inode.metadata() {
            if meta.type_ == FileType::NamedPipe {
                return Ok(PollStatus {
                    read: meta.size > 0,
                    write: true,
                    error: false,
                    hangup: false,
                });
            }
        }

        // `INode::async_poll` takes no events, so every inode that implements it
        // waits for READABLE whatever the caller asked about: `PtyReadFuture`
        // subscribes to the bus and re-checks `slave_read_ready`, the DRM wait
        // waits for a vblank. A caller that asked only about writability then
        // parks until something arrives to *read* -- on a terminal nobody is
        // typing into, that is for ever, and it is the wait `sys_write` uses to
        // hold a blocking writer until there is room.
        //
        // So ask the synchronous `poll`, which does answer both halves, before
        // parking on the wrong one. This was done for `DrmDev` alone, by name;
        // every other inode fell through. `poll` is best-effort here for the
        // same reason as in `poll` below: a socket answers `ENOSYS` to
        // `metadata` and must still reach its own `async_poll`. And after the
        // FIFO answer above, not before: that one contradicts the node's own
        // `poll`, which calls a FIFO readable whether or not it holds bytes.
        let want_read = _events.wants_read();
        let want_write = _events.wants_write();
        if want_read != want_write {
            if let Ok(status) = inode.poll() {
                if (want_read && status.read) || (want_write && status.write) {
                    return Ok(status);
                }
            }
        }
        Ok(inode.async_poll().await?)
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let inode = self.inner.read().inode.clone();
        // DRM card fd: park on the shared DRM event bus (same detection the
        // blocking-read path uses above).
        use super::devfs::DrmDev;
        if let Some(drmdev) = inode.downcast_ref::<DrmDev>() {
            let bus = drmdev.file_state().eventbus();
            let mask = super::poll_events_to_bus_mask(events);
            return Some(crate::sync::subscribe_readiness_on(&bus, mask, waker));
        }
        if let Some(pipe) = inode.downcast_ref::<super::pipe::Pipe>() {
            return Some(pipe.subscribe_readiness(events, waker));
        }
        if let Some(master) = inode.downcast_ref::<super::pty::PtyMaster>() {
            return Some(master.subscribe_readiness(events, waker));
        }
        if let Some(slave) = inode.downcast_ref::<super::pty::PtySlave>() {
            return Some(slave.subscribe_readiness(events, waker));
        }
        // Regular files / directories are always ready (poll never parks on
        // them), and everything else — device nodes without an event bus,
        // FIFOs resolved through the fs — keeps the caller's short re-poll
        // backstop by reporting "not subscribable".
        None
    }

    fn readiness_seq(&self) -> Option<u64> {
        let inode = self.inner.read().inode.clone();
        inode
            .downcast_ref::<super::pipe::Pipe>()
            .map(|pipe| pipe.readiness_seq())
    }

    fn subscribe_edge(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
        seen: u64,
    ) -> Option<crate::sync::ReadinessSub> {
        let inode = self.inner.read().inode.clone();
        inode
            .downcast_ref::<super::pipe::Pipe>()
            .map(|pipe| pipe.subscribe_edge(events, waker, seen))
    }

    fn ioctl(&self, request: usize, arg1: usize, _arg2: usize, _arg3: usize) -> LxResult<usize> {
        // ioctl syscall
        let inner = self.inner.read();
        use super::devfs::PcmDev;
        if let Some(pcm) = inner.inode.downcast_ref::<PcmDev>() {
            pcm.io_control_with_flags(request as u32, arg1, inner.flags)?;
        } else {
            inner.inode.io_control(request as u32, arg1)?;
        }
        Ok(0)
    }

    fn is_input_device(&self) -> bool {
        use super::devfs::{EventDev, MiceDev};
        let inode = self.inner.read().inode.clone();
        inode.downcast_ref::<MiceDev>().is_some() || inode.downcast_ref::<EventDev>().is_some()
    }

    fn is_char_device(&self) -> bool {
        self.metadata()
            .map(|m| m.type_ == FileType::CharDevice)
            .unwrap_or(false)
    }

    /// `FIONREAD` on a regular file: what is left between the position and
    /// the end (`file_ioctl`, fs/ioctl.c: `i_size_read(inode) - filp->f_pos`).
    /// It fell through to the inode's `io_control`, which knows no ioctl, so
    /// the answer was ENOTTY: bash's `read -t 0` (`input_avail`), which asks
    /// this first, took a redirected file for a terminal with nothing typed.
    ///
    /// A pipe answers the same way Linux `pipe_ioctl` does: the shared
    /// buffer's occupancy (either end). Without this, `FIONREAD` on a pipe
    /// also fell through to ENOTTY.
    fn readable_bytes(&self) -> Option<usize> {
        let inner = self.inner.read();
        if let Some(pipe) = inner.inode.downcast_ref::<super::pipe::Pipe>() {
            return Some(pipe.buffered_len());
        }
        let metadata = inner.inode.metadata().ok()?;
        regular_file_readable_bytes(metadata.type_, metadata.size, inner.offset)
    }

    fn is_terminal(&self) -> bool {
        use super::devfs::UartDev;
        use super::stdio::{CurrentVtTty, Stdin, Stdout};
        let inode = self.inner.read().inode.clone();
        inode.downcast_ref::<Stdin>().is_some()
            || inode.downcast_ref::<Stdout>().is_some()
            || inode.downcast_ref::<CurrentVtTty>().is_some()
            || inode.downcast_ref::<UartDev>().is_some()
            || inode.downcast_ref::<super::pty::PtyMaster>().is_some()
            || inode.downcast_ref::<super::pty::PtySlave>().is_some()
            || inode
                .downcast_ref::<super::devfs::pty::PtyMaster>()
                .is_some()
            || inode
                .downcast_ref::<super::devfs::pty::PtySlave>()
                .is_some()
    }

    /// Returns the [`VmObject`] representing the file with given `offset` and `len`.
    fn get_vmo(&self, offset: usize, len: usize) -> LxResult<Arc<VmObject>> {
        let inner = self.inner.read();
        match inner.inode.metadata()?.type_ {
            FileType::File => {
                // Back the file mapping with a *paged* (non-contiguous) VMO that
                // is demand-paged from the file: each page is read in on the
                // page fault that first touches it, instead of reading the whole
                // mapping up front.
                //
                // Eagerly reading the whole mapping used to stall the machine:
                // the dynamic linker maps a library's entire LOAD span in one
                // `mmap`, and the ~150 MiB `libLLVM.so` pulled in by `perf`
                // forced ~9.6k synchronous 16 KiB reads plus a full commit of
                // every page before the syscall returned — on real hardware that
                // looked like a hard freeze (couldn't even switch VT). A non-PIE
                // program only touches a fraction of such a library, so paging it
                // in on demand reads (and commits) only what is actually used.
                //
                // The source captures the file inode and the file offset; bytes
                // past end-of-file stay zero (the BSS tail of a file mapping).
                let file_size = inner.inode.metadata()?.size;

                // MAP_PRIVATE borrows from the per-inode page cache: clean
                // pages resolve to the cache's frames (shared by every process
                // mapping this file) and the first write copies just that page.
                // This is what keeps N GTK processes from holding N private
                // copies of libgtk/libglib -- the failure measured as three
                // 266 MiB processes and OOM half a minute into the session.
                // It also subsumes the old coherence special-case for files
                // with a live MAP_SHARED VMO: the borrower reads the very same
                // cache those writes land in.
                //
                // An unaligned offset cannot borrow (frames are page-grained);
                // a cache too short for the window (file grew) declines. Both
                // fall back to the private demand-paged snapshot below.
                if offset.is_multiple_of(PAGE_SIZE) {
                    if let Some(cache) =
                        inode_cache_vmo(&inner.inode, &self.path, file_size, offset, len, false)
                    {
                        let vmo = VmObject::new_paged_borrowing(pages(len), cache, offset);
                        vmo.set_name(&self.path);
                        vmo.set_file_offset(offset);
                        return Ok(vmo);
                    }
                }

                let source_len = file_size.saturating_sub(offset).min(len);
                if len >= 16 * 1024 * 1024 {
                    info!(
                        "get_vmo: demand-paged file map len={} MiB offset={:#x} source={} MiB",
                        len / (1024 * 1024),
                        offset,
                        source_len / (1024 * 1024),
                    );
                }
                let source: Arc<dyn zircon_object::vm::FrameFiller> = Arc::new(FileFrameFiller {
                    inode: inner.inode.clone(),
                    file_offset: offset,
                    max_len: len,
                });
                // Name the VMO after the file it is paged from. The name is what
                // `/proc/<pid>/maps` shows, and what turns a bare crash address
                // into "<library>+<offset>" in the [exit]/[crash-bt] dumps — a
                // constant `pc=0x499f8c` is useless on its own, but
                // `libglib-2.0.so.0+0x…` can be fed straight to addr2line.
                let vmo = VmObject::new_paged_with_source(pages(len), source);
                vmo.set_name(&self.path);
                // The mapping is created at `vmo_offset == 0` over this window,
                // so the file offset lives here; `/proc/<pid>/maps` and the
                // crash reports add it back.
                vmo.set_file_offset(offset);
                Ok(vmo)
            }
            FileType::CharDevice => {
                use super::devfs::{DrmDev, FbDev};
                if let Some(fbdev) = inner.inode.downcast_ref::<FbDev>() {
                    fbdev.get_vmo(offset, len)
                } else if let Some(drmdev) = inner.inode.downcast_ref::<DrmDev>() {
                    drmdev.get_vmo(offset, len).map_err(Into::into)
                } else {
                    Err(LxError::ENOSYS)
                }
            }
            _ => Err(LxError::ENOSYS),
        }
    }

    fn get_vmo_shared(&self, offset: usize, len: usize) -> LxResult<(Arc<VmObject>, usize)> {
        let inner = self.inner.read();
        if inner.inode.metadata()?.type_ != FileType::File {
            // Devices keep their own get_vmo (fb/drm are inherently shared).
            drop(inner);
            return self.get_vmo(offset, len).map(|vmo| (vmo, 0));
        }
        if !offset.is_multiple_of(4096) {
            return Err(LxError::EINVAL);
        }
        let file_size = inner.inode.metadata()?.size;
        // One VMO per inode, shared with the MAP_PRIVATE borrowers; marking it
        // `shared` is what arms eviction-time writeback (a MAP_SHARED mapping
        // can dirty the cache's own pages; borrowers cannot). Held STRONG,
        // anchored to the inode's lifetime (see SHARED_FILE_VMOS) so MAP_SHARED
        // writes survive the writer's munmap — the wl_keyboard keymap depends
        // on this.
        if let Some(vmo) = inode_cache_vmo(&inner.inode, &self.path, file_size, offset, len, true) {
            return Ok((vmo, offset));
        }
        // Mapping reaches past the cache VMO (file grew after creation). Rare;
        // fall back to a snapshot rather than silently truncating — but warn,
        // because writes through this mapping will NOT be visible to other
        // mappers.
        warn!(
            "get_vmo_shared: mapping {:#x}+{:#x} exceeds the cache vmo for {} — snapshot fallback",
            offset,
            len,
            self.path()
        );
        drop(inner);
        self.get_vmo(offset, len).map(|vmo| (vmo, 0))
    }
}

/// The `FIONREAD` answer of a `File` over an inode of `type_`: bytes from
/// `offset` to `size` for a regular file (a position past the end is 0,
/// not a wrap), and `None`, which hands the request to the inode's own
/// `ioctl`, for anything else (a directory is ENOTTY; a device answers for
/// itself).
fn regular_file_readable_bytes(type_: FileType, size: usize, offset: u64) -> Option<usize> {
    match type_ {
        FileType::File => Some((size as u64).saturating_sub(offset) as usize),
        _ => None,
    }
}

#[cfg(test)]
mod fionread_tests {
    //! `FIONREAD` on a regular file, which was ENOTTY.

    use super::*;
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_ramfs::RamFS;

    /// A regular file answers with what is left; anything else does not
    /// answer, so its own `ioctl` decides.
    #[test]
    fn a_regular_file_reports_what_is_left_after_the_position() {
        assert_eq!(regular_file_readable_bytes(FileType::File, 10, 0), Some(10));
        assert_eq!(regular_file_readable_bytes(FileType::File, 10, 4), Some(6));
        assert_eq!(regular_file_readable_bytes(FileType::File, 10, 10), Some(0));
        assert_eq!(
            regular_file_readable_bytes(FileType::File, 10, 11),
            Some(0),
            "past the end"
        );
        assert_eq!(regular_file_readable_bytes(FileType::File, 0, 0), Some(0));
        for other in [
            FileType::Dir,
            FileType::CharDevice,
            FileType::BlockDevice,
            FileType::SymLink,
        ] {
            assert_eq!(
                regular_file_readable_bytes(other, 10, 0),
                None,
                "{:?}",
                other
            );
        }
    }

    /// Through the `File` itself, over a ramfs: the position `lseek` set is
    /// the one the answer counts from.
    #[test]
    fn the_file_counts_from_its_own_position() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let inode = root.create("f", FileType::File, 0o644).unwrap();
        inode.write_at(0, &[7u8; 10]).unwrap();
        let file = File::new(inode, OpenFlags::RDWR, String::from("/f"));
        assert_eq!(FileLike::readable_bytes(&*file), Some(10));
        file.seek(SeekFrom::Start(4)).unwrap();
        assert_eq!(FileLike::readable_bytes(&*file), Some(6));
        file.seek(SeekFrom::Start(40)).unwrap();
        assert_eq!(FileLike::readable_bytes(&*file), Some(0));
        let dir = File::new(root, OpenFlags::RDONLY, String::from("/"));
        assert_eq!(FileLike::readable_bytes(&*dir), None);
    }

    /// `FIONREAD` on a pipe must report buffer occupancy, not fall through
    /// to ENOTTY via a missing `metadata`/`io_control`.
    #[test]
    fn a_pipe_reports_how_many_bytes_are_queued() {
        let (r, w) = crate::fs::Pipe::create_pair();
        let reader = File::new(Arc::new(r), OpenFlags::RDONLY, String::from("pipe:[r]"));
        let writer = File::new(Arc::new(w), OpenFlags::WRONLY, String::from("pipe:[w]"));
        assert_eq!(FileLike::readable_bytes(&*reader), Some(0));
        assert_eq!(FileLike::readable_bytes(&*writer), Some(0));
        writer.write(&[1, 2, 3, 4, 5]).unwrap();
        assert_eq!(FileLike::readable_bytes(&*reader), Some(5));
        assert_eq!(
            FileLike::readable_bytes(&*writer),
            Some(5),
            "Linux reports the same occupancy on either end"
        );
    }
}

#[cfg(test)]
mod pipe_seek_tests {
    use super::*;

    /// `lseek` on a pipe must be `ESPIPE`, not a silent success.
    #[test]
    fn seeking_a_pipe_is_espipe() {
        let (r, w) = crate::fs::Pipe::create_pair();
        let reader = File::new(Arc::new(r), OpenFlags::RDONLY, String::from("pipe:[r]"));
        let _writer = File::new(Arc::new(w), OpenFlags::WRONLY, String::from("pipe:[w]"));
        assert_eq!(
            FileLike::seek(&*reader, SeekFrom::Start(0)),
            Err(LxError::ESPIPE)
        );
        assert_eq!(
            FileLike::seek(&*reader, SeekFrom::Current(0)),
            Err(LxError::ESPIPE)
        );
    }

    /// `pread`/`pwrite` on a pipe must be `ESPIPE` too (same as Linux).
    #[test]
    fn positioned_io_on_a_pipe_is_espipe() {
        use async_std::task::block_on;
        let (r, w) = crate::fs::Pipe::create_pair();
        let reader = File::new(Arc::new(r), OpenFlags::RDONLY, String::from("pipe:[r]"));
        let writer = File::new(Arc::new(w), OpenFlags::WRONLY, String::from("pipe:[w]"));
        let mut buf = [0u8; 4];
        assert_eq!(
            block_on(FileLike::read_at(&*reader, 0, &mut buf)),
            Err(LxError::ESPIPE)
        );
        assert_eq!(
            FileLike::write_at(&*writer, 0, &[1, 2, 3, 4]),
            Err(LxError::ESPIPE)
        );
    }
}

#[cfg(test)]
mod seek_tests {
    //! `lseek(2)` is arithmetic on a number userspace chose, and the result
    //! becomes the offset every later read and write uses. A relative seek
    //! that goes below zero, or one that overflows, has to be `EINVAL`: if
    //! it wraps instead, the file position lands somewhere near 2^64 and the
    //! next read or write is aimed at an offset the filesystem was never
    //! asked about.

    use super::*;
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_ramfs::RamFS;

    /// A writable file of `len` bytes on a fresh ramfs.
    fn file(len: usize) -> Arc<File> {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let inode = root.create("f", FileType::File, 0o644).unwrap();
        if len > 0 {
            inode.write_at(0, &alloc::vec![0u8; len]).unwrap();
        }
        File::new(inode, OpenFlags::RDWR, String::from("/f"))
    }

    /// A directory with `names` in it, open for reading.
    fn dir(names: &[&str]) -> Arc<File> {
        let fs = RamFS::new();
        let root = fs.root_inode();
        for name in names {
            root.create(name, FileType::File, 0o644).unwrap();
        }
        File::new(root, OpenFlags::RDONLY, String::from("/"))
    }

    #[test]
    fn an_unread_entry_comes_out_again() {
        let d = dir(&["a", "b", "c"]);
        let first = d.read_entry().unwrap();
        let second = d.read_entry().unwrap();
        let third = d.read_entry().unwrap();
        d.unread_entry();
        assert_eq!(d.read_entry().unwrap(), third);
        d.unread_entry();
        d.unread_entry();
        assert_eq!(d.read_entry().unwrap(), second);
        // Reading on from there is the rest of the directory, once each.
        let mut rest = alloc::vec![d.read_entry().unwrap()];
        while let Ok(name) = d.read_entry() {
            rest.push(name);
        }
        assert_eq!(rest.len() + 2, 5, "\".\", \"..\", a, b and c: {:?}", rest);
        assert!(!rest.contains(&first) && !rest.contains(&second));
    }

    #[test]
    fn the_directory_position_is_where_a_seek_resumes() {
        let d = dir(&["a", "b", "c"]);
        let first = d.read_entry().unwrap();
        let second = d.read_entry().unwrap();
        let after_second = d.dir_position();
        let third = d.read_entry().unwrap();
        assert_ne!(
            after_second, 0,
            "the position after an entry is never the start"
        );
        // Seeking to the position reported after the second entry lands on
        // the third, which is the contract `d_off` and `seekdir` rest on.
        File::seek(&d, SeekFrom::Start(after_second)).unwrap();
        assert_eq!(d.read_entry().unwrap(), third);
        // And the positions climb with the entries.
        File::seek(&d, SeekFrom::Start(0)).unwrap();
        assert_eq!(d.read_entry().unwrap(), first);
        let after_first = d.dir_position();
        assert!(after_first < after_second);
        File::seek(&d, SeekFrom::Start(after_first)).unwrap();
        assert_eq!(d.read_entry().unwrap(), second);
    }

    #[test]
    fn unreading_at_the_start_stays_at_the_start() {
        let d = dir(&["a"]);
        let first = d.read_entry().unwrap();
        d.unread_entry();
        d.unread_entry();
        assert_eq!(d.read_entry().unwrap(), first);
    }

    #[test]
    fn an_absolute_seek_lands_where_it_was_told() {
        let f = file(100);
        assert_eq!(File::seek(&f, SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(File::seek(&f, SeekFrom::Start(42)).unwrap(), 42);
    }

    #[test]
    fn seeking_past_the_end_is_allowed() {
        // POSIX says so, and it is how every sparse-file writer works: seek
        // out past the end, write, and the hole in between reads as zeroes.
        let f = file(10);
        assert_eq!(
            File::seek(&f, SeekFrom::Start(1_000_000)).unwrap(),
            1_000_000
        );
    }

    #[test]
    fn a_relative_seek_starts_from_where_the_file_is() {
        let f = file(100);
        File::seek(&f, SeekFrom::Start(30)).unwrap();
        assert_eq!(File::seek(&f, SeekFrom::Current(10)).unwrap(), 40);
        assert_eq!(File::seek(&f, SeekFrom::Current(-15)).unwrap(), 25);
    }

    #[test]
    fn a_seek_from_the_end_counts_backwards() {
        // `SeekFrom::End(0)` is how a program asks for the file's size, and
        // a negative offset is the normal way to read a trailer.
        let f = file(100);
        assert_eq!(File::seek(&f, SeekFrom::End(0)).unwrap(), 100);
        assert_eq!(File::seek(&f, SeekFrom::End(-20)).unwrap(), 80);
    }

    #[test]
    fn a_relative_seek_below_zero_is_refused_rather_than_wrapped() {
        // This is the one that matters: without the check the position
        // becomes something near 2^64 and every later read and write is
        // aimed at an offset nobody asked for.
        let f = file(100);
        File::seek(&f, SeekFrom::Start(10)).unwrap();
        assert!(matches!(
            File::seek(&f, SeekFrom::Current(-11)),
            Err(LxError::EINVAL)
        ));
        assert_eq!(
            File::seek(&f, SeekFrom::Current(0)).unwrap(),
            10,
            "a refused seek must not have moved the position"
        );
    }

    #[test]
    fn a_seek_before_the_start_of_the_file_is_refused() {
        let f = file(100);
        assert!(matches!(
            File::seek(&f, SeekFrom::End(-101)),
            Err(LxError::EINVAL)
        ));
    }

    #[test]
    fn a_negative_absolute_seek_is_refused() {
        // `lseek(fd, -1, SEEK_SET)`. The offset is signed in the uAPI and
        // arrives here as an enormous `u64`.
        let f = file(100);
        assert!(matches!(
            File::seek(&f, SeekFrom::Start(u64::MAX)),
            Err(LxError::EINVAL)
        ));
        assert!(matches!(
            File::seek(&f, SeekFrom::Start(1 << 63)),
            Err(LxError::EINVAL)
        ));
    }

    #[test]
    fn a_seek_that_would_overflow_is_refused() {
        // Adding to a position near the top of the range.
        //
        // The `checked_add` in `seek` is belt and braces here rather than
        // the thing doing the work: both operands are non-negative (the
        // stored offset can never have its sign bit set, because this very
        // function refuses one), so a wrap always lands negative and the
        // `new_offset < 0` test below catches it either way. Replacing the
        // `checked_add` with a `wrapping_add` therefore changes nothing
        // observable -- worth knowing before anyone goes looking for a test
        // that tells the two apart.
        let f = file(100);
        File::seek(&f, SeekFrom::Start(i64::MAX as u64)).unwrap();
        assert!(matches!(
            File::seek(&f, SeekFrom::Current(1)),
            Err(LxError::EINVAL)
        ));
        assert!(matches!(
            File::seek(&f, SeekFrom::Current(i64::MAX)),
            Err(LxError::EINVAL)
        ));
    }

    #[test]
    fn the_largest_offset_an_off_t_can_name_is_still_a_valid_position() {
        let f = file(0);
        assert_eq!(
            File::seek(&f, SeekFrom::Start(i64::MAX as u64)).unwrap(),
            i64::MAX as u64
        );
    }

    #[test]
    fn a_seek_of_zero_reports_the_position_without_moving_it() {
        // `lseek(fd, 0, SEEK_CUR)` is how every program asks "where am I",
        // and it must not disturb anything.
        let f = file(100);
        File::seek(&f, SeekFrom::Start(37)).unwrap();
        assert_eq!(File::seek(&f, SeekFrom::Current(0)).unwrap(), 37);
        assert_eq!(File::seek(&f, SeekFrom::Current(0)).unwrap(), 37);
    }
}

#[cfg(test)]
mod setfl_tests {
    //! `fcntl(F_SETFL)` used to hand its whole argument to `set_flags`, and
    //! `set_flags` took the bits it knew from it. What Linux's `setfl` does is
    //! narrower: `SETFL_MASK` in, everything else as it was.

    use super::OpenFlags;

    #[test]
    fn setfl_changes_only_the_status_flags() {
        let rw = OpenFlags::RDWR | OpenFlags::CLOEXEC;
        // The common call: make an existing fd non-blocking. The access mode
        // and the close-on-exec record survive it.
        let after = OpenFlags::after_setfl(rw, OpenFlags::NON_BLOCK);
        assert_eq!(after, rw | OpenFlags::NON_BLOCK);
        assert!(after.readable() && after.writable());
        // The next common call: `F_SETFL(fcntl(F_GETFL) & ~O_NONBLOCK)`,
        // which glibc spells as the flags word with the bit cleared.
        let back = OpenFlags::after_setfl(after, after - OpenFlags::NON_BLOCK);
        assert_eq!(back, rw);
        // An argument of 0 clears the status flags and nothing else: it does
        // not turn the file read-only (`RDONLY` is 0) or drop CLOEXEC.
        let cleared = OpenFlags::after_setfl(
            rw | OpenFlags::APPEND | OpenFlags::NON_BLOCK | OpenFlags::ASYNC,
            OpenFlags::empty(),
        );
        assert_eq!(cleared, rw);
        // O_ASYNC is a status flag too (FASYNC / ioctl FIOASYNC).
        let async_on = OpenFlags::after_setfl(rw, OpenFlags::ASYNC);
        assert_eq!(async_on, rw | OpenFlags::ASYNC);
        // O_DIRECT and O_NOATIME are in Linux's SETFL_MASK; without them
        // here, F_SETFL could neither set nor clear either bit.
        let direct = OpenFlags::after_setfl(rw, OpenFlags::DIRECT | OpenFlags::NOATIME);
        assert_eq!(direct, rw | OpenFlags::DIRECT | OpenFlags::NOATIME);
        let cleared_direct = OpenFlags::after_setfl(direct, OpenFlags::NON_BLOCK);
        assert_eq!(
            cleared_direct,
            rw | OpenFlags::NON_BLOCK,
            "F_SETFL(O_NONBLOCK) must clear O_DIRECT|O_NOATIME that were set"
        );
    }

    #[test]
    fn setfl_cannot_set_what_is_not_a_status_flag() {
        let ro = OpenFlags::RDONLY;
        // `F_SETFL(O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC)`: none of it
        // takes. The access mode is fixed at open, creation flags are for
        // `open`, and close-on-exec is `F_SETFD`'s.
        let asked =
            OpenFlags::WRONLY | OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::CLOEXEC;
        let after = OpenFlags::after_setfl(ro, asked);
        assert_eq!(after, ro);
        assert!(!after.writable() && !after.close_on_exec());
        // While APPEND rides along with NON_BLOCK.
        let after = OpenFlags::after_setfl(ro, asked | OpenFlags::APPEND);
        assert_eq!(after, ro | OpenFlags::APPEND);
    }

    #[test]
    fn take_settable_is_the_status_bits_every_set_flags_used_to_copy_by_hand() {
        let mut flags = OpenFlags::RDWR | OpenFlags::NON_BLOCK;
        flags.take_settable(
            OpenFlags::APPEND
                | OpenFlags::ASYNC
                | OpenFlags::DIRECT
                | OpenFlags::NOATIME
                | OpenFlags::CLOEXEC,
        );
        assert_eq!(
            flags,
            OpenFlags::RDWR
                | OpenFlags::APPEND
                | OpenFlags::ASYNC
                | OpenFlags::DIRECT
                | OpenFlags::NOATIME
                | OpenFlags::CLOEXEC
        );
        // Bits it does not own are left alone in both directions.
        flags.take_settable(OpenFlags::WRONLY | OpenFlags::CREATE);
        assert_eq!(flags, OpenFlags::RDWR);
    }

    #[test]
    fn each_status_flag_is_asked_about_by_its_own_name() {
        // Every one of the three questions is answered by its own bit and
        // never by a neighbour. Making `is_append` read `O_NONBLOCK` passed
        // the whole suite green, and `O_APPEND` is what sends a write to the
        // end of the file: every shell `>>` and every log line would have
        // landed at the current offset instead, while a non-blocking socket
        // would have appended.
        let append = OpenFlags::RDWR | OpenFlags::APPEND;
        assert!(append.is_append(), "O_APPEND was not seen");
        assert!(!append.non_block(), "O_APPEND answered for O_NONBLOCK");
        assert!(!append.close_on_exec(), "O_APPEND answered for O_CLOEXEC");

        let non_block = OpenFlags::RDWR | OpenFlags::NON_BLOCK;
        assert!(non_block.non_block(), "O_NONBLOCK was not seen");
        assert!(!non_block.is_append(), "O_NONBLOCK answered for O_APPEND");
        assert!(
            !non_block.close_on_exec(),
            "O_NONBLOCK answered for O_CLOEXEC"
        );

        let cloexec = OpenFlags::RDWR | OpenFlags::CLOEXEC;
        assert!(cloexec.close_on_exec(), "O_CLOEXEC was not seen");
        assert!(!cloexec.is_append(), "O_CLOEXEC answered for O_APPEND");
        assert!(!cloexec.non_block(), "O_CLOEXEC answered for O_NONBLOCK");

        // And a plain `open(path, O_RDONLY)` is none of the three.
        let plain = OpenFlags::RDONLY;
        assert!(!plain.is_append() && !plain.non_block() && !plain.close_on_exec());
    }

    #[test]
    fn the_access_mode_is_the_low_two_bits_read_as_a_number() {
        // `O_RDONLY`, `O_WRONLY` and `O_RDWR` are 0, 1 and 2: one number in
        // the low two bits, not three independent flags. That is why masking
        // with `0b1` instead of `0b11` answers the same for all four values
        // -- `RDWR & 0b1` is 0, which is `RDONLY` -- and why no test can tell
        // the two masks apart. The table is what makes that true, so it is
        // pinned here rather than left to be rediscovered.
        let cases = [
            (OpenFlags::RDONLY, true, false),
            (OpenFlags::WRONLY, false, true),
            (OpenFlags::RDWR, true, true),
            // 3 is `O_ACCMODE`, which upstream `open` refuses; nothing here
            // calls it readable or writable either.
            (OpenFlags::from_bits_truncate(3), false, false),
        ];
        for (flags, readable, writable) in cases {
            assert_eq!(flags.readable(), readable, "readable() for {flags:?}");
            assert_eq!(flags.writable(), writable, "writable() for {flags:?}");
            // Nothing above the access mode changes either answer.
            let dressed = flags | OpenFlags::CLOEXEC | OpenFlags::APPEND | OpenFlags::TRUNCATE;
            assert_eq!(dressed.readable(), readable, "readable() for {dressed:?}");
            assert_eq!(dressed.writable(), writable, "writable() for {dressed:?}");
        }
    }
}

#[cfg(test)]
mod poll_events_tests {
    //! `poll(2)` on `POLLERR` and `POLLHUP`: "these bits are output only, and
    //! are ignored in `events`". They are reported whether the caller asked
    //! for them or not, and `epoll` gets there by forcing the two into every
    //! stored mask. Dropping either one from the pair `revents` forces in
    //! passed the whole suite green.

    use super::{PollEvents, PollStatus};

    fn status(read: bool, write: bool, error: bool, hangup: bool) -> PollStatus {
        PollStatus {
            read,
            write,
            error,
            hangup,
        }
    }

    #[test]
    fn a_hangup_and_an_error_are_reported_whether_they_were_asked_for_or_not() {
        // The caller asked about reading only. `poll` still has to tell it the
        // other end is gone, or it goes back to sleep on a pipe nobody will
        // ever write to again: that is a shell pipeline hanging instead of
        // seeing end-of-file.
        let hung_up = status(false, false, false, true);
        assert_eq!(
            PollEvents::revents(&hung_up, PollEvents::IN),
            PollEvents::HUP,
            "POLLHUP was not reported to a caller that only asked to read"
        );
        let broken = status(false, false, true, false);
        assert_eq!(
            PollEvents::revents(&broken, PollEvents::OUT),
            PollEvents::ERR,
            "POLLERR was not reported to a caller that only asked to write"
        );
        // Both at once, to a caller that asked for neither.
        let both = status(false, false, true, true);
        assert_eq!(
            PollEvents::revents(&both, PollEvents::empty()),
            PollEvents::ERR | PollEvents::HUP
        );
        // And a caller that did ask for them gets the same answer, so the
        // forcing is not what carries them.
        assert_eq!(
            PollEvents::revents(&both, PollEvents::ERR | PollEvents::HUP),
            PollEvents::ERR | PollEvents::HUP
        );
    }

    #[test]
    fn readiness_the_caller_did_not_ask_about_is_not_reported() {
        // The other half of the rule, which is what keeps the forced pair from
        // turning into "report everything": a poller waiting to write is not
        // woken because there is something to read.
        let both_ways = status(true, true, false, false);
        assert_eq!(
            PollEvents::revents(&both_ways, PollEvents::IN),
            PollEvents::IN
        );
        assert_eq!(
            PollEvents::revents(&both_ways, PollEvents::OUT),
            PollEvents::OUT
        );
        assert_eq!(
            PollEvents::revents(&both_ways, PollEvents::empty()),
            PollEvents::empty(),
            "a request for nothing was answered with something"
        );
        // Asking under the streams name gets the streams name back: `ready`
        // reports both names and the mask keeps the one that was asked for.
        assert_eq!(
            PollEvents::revents(&both_ways, PollEvents::RDNORM),
            PollEvents::RDNORM
        );
        assert_eq!(
            PollEvents::revents(&both_ways, PollEvents::IN | PollEvents::RDNORM),
            PollEvents::IN | PollEvents::RDNORM
        );
    }
}

#[cfg(test)]
mod async_poll_tests {
    //! `INode::async_poll` takes no events, so the inodes that override it all
    //! wait for readability whatever was asked. A caller that asked only about
    //! writability must not be parked on that wait: on a terminal nobody is
    //! typing into it never fires.

    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn flag_waker(flag: &'static AtomicBool) -> Waker {
        fn raw(ptr: *const ()) -> RawWaker {
            unsafe fn clone(ptr: *const ()) -> RawWaker {
                raw(ptr)
            }
            unsafe fn wake(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn wake_by_ref(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn drop(_: *const ()) {}
            RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
        }
        unsafe { Waker::from_raw(raw(flag as *const AtomicBool as *const ())) }
    }

    /// One poll of the future, so a regression is a failed assertion rather
    /// than a test that never returns.
    fn poll_once(file: &Arc<dyn FileLike>, events: PollEvents) -> Poll<LxResult<PollStatus>> {
        static WOKE: AtomicBool = AtomicBool::new(false);
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = FileLike::async_poll(&**file, events);
        core::future::Future::poll(fut.as_mut(), &mut cx)
    }

    /// A terminal with nothing typed into it, through the `File` a process
    /// actually holds.
    fn a_terminal() -> Arc<dyn FileLike> {
        use crate::fs::pty::{alloc_ptmx, open_pts, PtyMaster};
        let master = alloc_ptmx();
        let id = master
            .downcast_ref::<PtyMaster>()
            .expect("alloc_ptmx gives a master")
            .pty_id();
        // The master is left alive on purpose: dropping it hangs the pair up,
        // and a hung-up terminal answers every poll at once for the wrong
        // reason. `core::mem::forget` rather than a binding, because
        // `Drop for PtyMaster` reaches into the global map.
        core::mem::forget(master);
        let slave = open_pts(id).expect("the pair is registered");
        File::new(slave, OpenFlags::RDWR, String::from("/dev/pts/x"))
    }

    #[test]
    fn asking_a_terminal_about_writability_does_not_wait_for_something_to_read() {
        // The terminal is writable at once -- its output queue is empty -- and
        // nothing is queued to read. This used to park on `PtyReadFuture`,
        // which subscribes to the bus and re-checks `slave_read_ready`, so the
        // answer arrived when somebody typed, or never. The same wait is what
        // `sys_write` uses to hold a blocking writer until there is room.
        let f = a_terminal();
        assert!(!f.poll(PollEvents::IN).unwrap().read, "nothing typed");
        assert!(f.poll(PollEvents::OUT).unwrap().write, "room to print");
        match poll_once(&f, PollEvents::OUT) {
            Poll::Ready(Ok(s)) => assert!(s.write),
            other => panic!("parked on readability instead: {:?}", other.is_pending()),
        }
    }

    #[test]
    fn an_empty_fifo_keeps_its_own_answer_and_not_the_nodes() {
        // The FIFO case above the fast path says `read: meta.size > 0`, and the
        // ramfs node's own `poll` says a file is always readable. The order
        // decides which one a reader hears, and hearing "readable" over an
        // empty FIFO is the 0-byte read the FIFO case exists to stop.
        use rcore_fs::vfs::FileSystem;
        use rcore_fs_ramfs::RamFS;
        let fs = RamFS::new();
        let inode = fs
            .root_inode()
            .create("fifo", FileType::NamedPipe, 0o644)
            .unwrap();
        core::mem::forget(fs);
        assert!(inode.poll().unwrap().read, "the node says readable");
        let f: Arc<dyn FileLike> = File::new(inode, OpenFlags::RDWR, String::from("/fifo"));
        match poll_once(&f, PollEvents::IN) {
            Poll::Ready(Ok(s)) => assert!(!s.read, "empty, so not readable"),
            other => panic!("unexpected: pending={}", other.is_pending()),
        }
    }

    #[test]
    fn asking_about_readability_still_waits() {
        // The other half: the fast path must not answer a reader that has
        // nothing to read, or a blocking `read` turns into a spin.
        let f = a_terminal();
        assert!(
            poll_once(&f, PollEvents::IN).is_pending(),
            "a reader with nothing to read waits"
        );
    }
}

#[cfg(test)]
mod prefill_tests {
    //! What a page fault on a library costs the file system and the disk.
    //!
    //! A fault on a file mapping used to read its page and then the fifteen
    //! after it (`fault_around`) one `read_at` each: sixteen filesystem walks
    //! and, on the polled AHCI/NVMe drivers, one synchronous command per page
    //! before the block cache's read-ahead even started. `libxul.so` is
    //! ~150 MiB of that. Now the fault makes the whole window resident with
    //! ONE read (`VmObject::prefill` -> `FrameFiller::fill_range`), and these
    //! tests pin the count at every layer: the filler, the page cache behind
    //! `mmap`, and a real btrfs on a fake disk that counts its commands.
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use lock::Mutex;
    use zircon_object::vm::FrameFiller;

    /// A regular file whose byte at `i` is the low byte of its page number,
    /// that counts its reads and serves at most `per_call` bytes per read.
    struct CountingInode {
        size: usize,
        per_call: usize,
        reads: AtomicUsize,
        /// `(offset, len asked)` of every read, in order.
        log: Mutex<Vec<(usize, usize)>>,
    }

    impl CountingInode {
        fn new(size: usize, per_call: usize) -> Arc<Self> {
            Arc::new(Self {
                size,
                per_call,
                reads: AtomicUsize::new(0),
                log: Mutex::new(Vec::new()),
            })
        }
        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }
        fn log(&self) -> Vec<(usize, usize)> {
            self.log.lock().clone()
        }
    }

    fn page_byte(offset: usize) -> u8 {
        (offset / PAGE_SIZE) as u8
    }

    impl INode for CountingInode {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.log.lock().push((offset, buf.len()));
            if offset >= self.size {
                return Ok(0);
            }
            let n = buf.len().min(self.size - offset).min(self.per_call);
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = page_byte(offset + i);
            }
            Ok(n)
        }
        fn write_at(&self, _offset: usize, _buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::NotSupported)
        }
        fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
            Ok(PollStatus {
                read: true,
                write: false,
                error: false,
                hangup: false,
            })
        }
        fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
            Ok(Metadata {
                dev: 1,
                inode: 7,
                size: self.size,
                blk_size: 4096,
                blocks: self.size.div_ceil(512),
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
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    fn filler(inode: Arc<CountingInode>, file_offset: usize, max_len: usize) -> FileFrameFiller {
        FileFrameFiller {
            inode,
            file_offset,
            max_len,
        }
    }

    /// The whole window in one read, and the bytes are the file's.
    #[test]
    fn fill_range_reads_a_window_with_one_read() {
        let inode = CountingInode::new(64 * PAGE_SIZE, usize::MAX);
        let f = filler(inode.clone(), 0, usize::MAX);
        let mut buf = vec![0u8; 16 * PAGE_SIZE];
        f.fill_range(3 * PAGE_SIZE, &mut buf);
        assert_eq!(inode.log(), vec![(3 * PAGE_SIZE, 16 * PAGE_SIZE)]);
        for page in 0..16 {
            assert_eq!(buf[page * PAGE_SIZE], (3 + page) as u8, "page {page}");
            assert_eq!(buf[page * PAGE_SIZE + PAGE_SIZE - 1], (3 + page) as u8);
        }
        // A single page is a single read of a page, as before.
        let mut one = vec![0u8; PAGE_SIZE];
        f.fill_page(9 * PAGE_SIZE, &mut one);
        assert_eq!(inode.log()[1], (9 * PAGE_SIZE, PAGE_SIZE));
        assert_eq!(one[0], 9);
    }

    /// An inode that serves short reads is read until the window is full,
    /// each read picking up where the last stopped: bytes land where they
    /// belong, never shifted.
    #[test]
    fn a_short_read_is_continued_not_taken_for_the_end() {
        let inode = CountingInode::new(64 * PAGE_SIZE, PAGE_SIZE + 100);
        let f = filler(inode.clone(), 0, usize::MAX);
        let mut buf = vec![0u8; 4 * PAGE_SIZE];
        f.fill_range(PAGE_SIZE, &mut buf);
        let log = inode.log();
        assert_eq!(log[0], (PAGE_SIZE, 4 * PAGE_SIZE));
        assert_eq!(log[1], (2 * PAGE_SIZE + 100, 3 * PAGE_SIZE - 100));
        assert_eq!(log.len(), 4);
        for (i, b) in buf.iter().enumerate() {
            assert_eq!(*b, page_byte(PAGE_SIZE + i), "byte {i}");
        }
    }

    /// The read stops at the end of the file, and at the mapping's own
    /// bound (`max_len`) counted from where the mapping starts in the file
    /// (`file_offset`): a 16-page window at the tail of a library must not
    /// ask the file system for bytes past EOF.
    #[test]
    fn fill_range_is_clipped_to_the_file_and_the_mapping() {
        let inode = CountingInode::new(5 * PAGE_SIZE, usize::MAX);
        let f = filler(inode.clone(), 0, usize::MAX);
        let mut buf = vec![0xffu8; 16 * PAGE_SIZE];
        f.fill_range(2 * PAGE_SIZE, &mut buf);
        assert_eq!(inode.log(), vec![(2 * PAGE_SIZE, 3 * PAGE_SIZE)]);
        assert_eq!(buf[3 * PAGE_SIZE - 1], 4, "last byte of the file");
        assert_eq!(buf[3 * PAGE_SIZE], 0xff, "past EOF is the caller's");
        // Entirely past the end: no read at all.
        f.fill_range(5 * PAGE_SIZE, &mut buf);
        assert_eq!(inode.reads(), 1);

        let inode = CountingInode::new(64 * PAGE_SIZE, usize::MAX);
        let f = filler(inode.clone(), 2 * PAGE_SIZE, 4 * PAGE_SIZE);
        let mut buf = vec![0u8; 16 * PAGE_SIZE];
        f.fill_range(0, &mut buf);
        assert_eq!(inode.log(), vec![(2 * PAGE_SIZE, 4 * PAGE_SIZE)]);
        assert_eq!(buf[0], 2, "VMO offset 0 is file page 2");
    }

    /// Through `mmap`'s own objects: `get_vmo` hands out a borrower over the
    /// per-inode page cache, and prefilling a window through the borrower
    /// costs the inode ONE read; the page fault's commits after it are hits.
    #[test]
    fn a_private_mapping_prefills_its_page_cache_with_one_read() {
        let inode = CountingInode::new(64 * PAGE_SIZE, usize::MAX);
        let file = File::new(inode.clone(), OpenFlags::RDONLY, String::from("/libxul.so"));
        let vmo = FileLike::get_vmo(&*file, 0, 32 * PAGE_SIZE).unwrap();
        assert!(vmo.is_borrower());
        vmo.prefill(4, 16);
        assert_eq!(inode.log(), vec![(4 * PAGE_SIZE, 16 * PAGE_SIZE)]);
        for page in 4..20 {
            vmo.commit_page(page, zircon_object::vm::MMUFlags::READ)
                .unwrap();
        }
        assert_eq!(inode.reads(), 1, "the window's faults are hits");
        let mut b = [0u8; 1];
        vmo.read(19 * PAGE_SIZE, &mut b).unwrap();
        assert_eq!(b[0], 19);
        // The next page is outside the window: it is read, on its own.
        vmo.commit_page(20, zircon_object::vm::MMUFlags::READ)
            .unwrap();
        assert_eq!(inode.log()[1], (20 * PAGE_SIZE, PAGE_SIZE));
    }

    /// In-memory 512-byte-sector disk that counts the read commands it is
    /// given. On the polled AHCI/NVMe drivers each one is a synchronous
    /// round trip with one command in flight, so this, not the byte count,
    /// is what demand paging pays for.
    struct MockBlock {
        sectors: Mutex<Vec<u8>>,
        commands: AtomicUsize,
        /// `(sector, sectors)` of every read command, in order.
        log: Mutex<Vec<(usize, usize)>>,
    }

    impl MockBlock {
        fn new(bytes: usize) -> Arc<Self> {
            Arc::new(Self {
                sectors: Mutex::new(vec![0u8; bytes]),
                commands: AtomicUsize::new(0),
                log: Mutex::new(Vec::new()),
            })
        }
        fn commands(&self) -> usize {
            self.commands.load(Ordering::Relaxed)
        }
        fn reset(&self) {
            self.commands.store(0, Ordering::Relaxed);
            self.log.lock().clear();
        }
    }

    impl zcore_drivers::scheme::Scheme for MockBlock {
        fn name(&self) -> &str {
            "mockblock"
        }
    }

    impl zcore_drivers::scheme::BlockScheme for MockBlock {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> zcore_drivers::DeviceResult {
            let start = block_id * 512;
            let d = self.sectors.lock();
            if buf.is_empty() || buf.len() % 512 != 0 || start + buf.len() > d.len() {
                return Err(zcore_drivers::DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&d[start..start + buf.len()]);
            self.commands.fetch_add(1, Ordering::Relaxed);
            self.log.lock().push((block_id, buf.len() / 512));
            Ok(())
        }
        fn write_block(&self, block_id: usize, buf: &[u8]) -> zcore_drivers::DeviceResult {
            let start = block_id * 512;
            let mut d = self.sectors.lock();
            if buf.is_empty() || buf.len() % 512 != 0 || start + buf.len() > d.len() {
                return Err(zcore_drivers::DeviceError::InvalidParam);
            }
            d[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> zcore_drivers::DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            self.sectors.lock().len() / 512
        }
    }

    /// `mkfs` writes straight into the disk's bytes, outside the count.
    struct Formatter(Arc<MockBlock>);

    impl btrfs::BlockDevice for Formatter {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> btrfs::Result<()> {
            let d = self.0.sectors.lock();
            let o = offset as usize;
            buf.copy_from_slice(&d[o..o + buf.len()]);
            Ok(())
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> btrfs::Result<()> {
            let mut d = self.0.sectors.lock();
            let o = offset as usize;
            d[o..o + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn sync(&self) -> btrfs::Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            self.0.sectors.lock().len() as u64
        }
    }

    fn mkfs_opts() -> btrfs::mkfs::MkfsOptions {
        let mut seed = 0x1234_5678_9abc_def0u64;
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
        btrfs::mkfs::MkfsOptions {
            label: String::from("libxul"),
            fsid: uuid(),
            chunk_uuid: uuid(),
            dev_uuid: uuid(),
            subvol_uuid: uuid(),
            now: (1_700_000_000, 0),
        }
    }

    /// The real chain, end to end: a btrfs on a disk that counts its
    /// commands, a `File` over it, the page cache `mmap` uses, and a window
    /// made resident through it. A cold 16-page window is one command, not
    /// sixteen page reads (which the block cache turns into two 4 KiB
    /// commands and then a megabyte of read-ahead).
    #[test]
    fn a_cold_window_of_a_library_on_btrfs_is_one_disk_command() {
        use super::super::block_mount::MountBackend;
        use super::super::btrfs_mount::BtrfsMountFs;
        use rcore_fs::vfs::FileSystem;
        use zircon_object::vm::MMUFlags;

        const LIB_PAGES: usize = 2048; // 8 MiB
        let disk = MockBlock::new(32 * 1024 * 1024);
        btrfs::mkfs::format(&Formatter(disk.clone()), &mkfs_opts()).unwrap();
        let backend = MountBackend::Block(disk.clone());
        {
            let fs = BtrfsMountFs::open(&backend, false).unwrap();
            let lib = fs
                .root_inode()
                .create("libxul.so", FileType::File, 0o755)
                .unwrap();
            let mut chunk = vec![0u8; 64 * PAGE_SIZE];
            for at in (0..LIB_PAGES * PAGE_SIZE).step_by(chunk.len()) {
                for (i, b) in chunk.iter_mut().enumerate() {
                    *b = page_byte(at + i) ^ 0x5a;
                }
                assert_eq!(lib.write_at(at, &chunk).unwrap(), chunk.len());
            }
            lib.sync_all().unwrap();
            fs.sync().unwrap();
        }

        // A fresh mount: nothing of the file in any cache.
        let fs = BtrfsMountFs::open(&backend, true).unwrap();
        let lib = fs.root_inode().find("libxul.so").unwrap();
        assert_eq!(lib.metadata().unwrap().size, LIB_PAGES * PAGE_SIZE);
        let file = File::new(lib, OpenFlags::RDONLY, String::from("/usr/lib/libxul.so"));
        let vmo = FileLike::get_vmo(&*file, 0, LIB_PAGES * PAGE_SIZE).unwrap();
        // The first touch of a file pays for its extent walk too; the fault
        // in the middle of the library is the steady state.
        vmo.prefill(0, 16);
        disk.reset();

        vmo.prefill(1024, 16);
        assert_eq!(
            disk.commands(),
            1,
            "one 64 KiB command for the window: {:?}",
            disk.log.lock()
        );
        assert_eq!(disk.log.lock()[0].1, 16 * PAGE_SIZE / 512);
        // The commits the fault then makes are hits, and carry the file.
        for page in 1024..1040 {
            vmo.commit_page(page, MMUFlags::READ).unwrap();
        }
        assert_eq!(disk.commands(), 1);
        let mut b = [0u8; 2];
        vmo.read(1039 * PAGE_SIZE + PAGE_SIZE - 2, &mut b).unwrap();
        assert_eq!(b, [page_byte(1039 * PAGE_SIZE) ^ 0x5a; 2]);

        // The old way, for the record: the same window page by page.
        disk.reset();
        for page in 1536..1552 {
            vmo.commit_page(page, MMUFlags::READ).unwrap();
        }
        assert!(
            disk.commands() > 1,
            "page by page was {} commands",
            disk.commands()
        );
    }
}

#[cfg(test)]
mod shared_vmo_registry_tests {
    //! The registry lock is a ticket spinlock held with interrupts off, so
    //! what may run under it is the whole story. A KERNEL STOP on real
    //! hardware (build `c7bdfaf`) caught the holder of this lock parked in
    //! `GlobalAlloc::dealloc` while eight other CPUs spun on `cache_vmo_of`:
    //! eviction used to write back to the filesystem and free the evicted
    //! VMO's frames from *inside* the critical section.
    //!
    //! These tests pin the two halves of the fix: the writeback still happens,
    //! and it happens with the lock down.

    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use rcore_fs::vfs::{FileSystem, FsError, Metadata, PollStatus};
    use rcore_fs_ramfs::RamFS;

    /// What the probe saw, kept by the test: the probe inode itself must die
    /// with the registry entry, so it cannot hold the counters.
    #[derive(Default)]
    struct Probe {
        writes: AtomicUsize,
        /// `write_at` calls that found the registry lock already held.
        writes_under_lock: AtomicUsize,
    }

    /// A ramfs file that reports, on every `write_at`, whether the registry
    /// lock was free at that moment.
    struct ProbeInode {
        inner: Arc<dyn INode>,
        probe: Arc<Probe>,
    }

    impl INode for ProbeInode {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
            self.inner.read_at(offset, buf)
        }

        fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, FsError> {
            self.probe.writes.fetch_add(1, Ordering::SeqCst);
            match SHARED_FILE_VMOS.try_lock() {
                // Free: taking and releasing it here is exactly what a real
                // filesystem write may end up doing underneath us.
                Some(guard) => drop(guard),
                None => {
                    self.probe.writes_under_lock.fetch_add(1, Ordering::SeqCst);
                }
            }
            self.inner.write_at(offset, buf)
        }

        fn poll(&self) -> Result<PollStatus, FsError> {
            self.inner.poll()
        }

        fn metadata(&self) -> Result<Metadata, FsError> {
            self.inner.metadata()
        }

        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    /// A fresh ramfs file of `content`. Returns the raw ramfs node (to read
    /// back through, since the probe will be gone) and the probe wrapper.
    fn probe_file(content: &[u8]) -> (Arc<dyn INode>, Arc<dyn INode>, Arc<Probe>) {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let node = root
            .create("f", FileType::File, 0o777)
            .expect("create failed");
        node.resize(content.len()).expect("resize failed");
        node.write_at(0, content).expect("write failed");
        let probe = Arc::new(Probe::default());
        let wrapper: Arc<dyn INode> = Arc::new(ProbeInode {
            inner: node.clone(),
            probe: probe.clone(),
        });
        (node, wrapper, probe)
    }

    /// Register a cache for `wrapper`, dirty its first page, and consume every
    /// reference to it so the registry's own cycle is all that keeps it alive:
    /// the state `take_prunable` evicts.
    ///
    /// Takes `wrapper` BY VALUE on purpose. The eviction rule is "nothing maps
    /// the VMO and no fd is still open", and a second `Arc<dyn INode>` in the
    /// test is indistinguishable from an open fd.
    fn register_dirty_cache(wrapper: Arc<dyn INode>, dirty: &[u8]) {
        let size = wrapper.metadata().unwrap().size;
        let vmo = inode_cache_vmo(&wrapper, "probe", size, 0, PAGE_SIZE, true)
            .expect("the cache vmo should have been created");
        vmo.write(0, dirty).expect("dirtying the cache failed");
        // Both handles go; the registry entry stays, holding the only strong
        // reference to the VMO and -- through the VMO's FrameFiller -- the only
        // strong reference to the inode.
        drop(vmo);
        drop(wrapper);
    }

    /// The evicted entry's dirty pages still reach the file. This is the whole
    /// point of eviction-time writeback: a `MAP_SHARED` store must survive the
    /// writer's `munmap`, which is what the wl_keyboard keymap depends on.
    #[test]
    fn eviction_writes_the_dirty_cache_back_to_the_file() {
        let (node, wrapper, probe) = probe_file(b"old content, to be overwritten..");
        register_dirty_cache(wrapper, b"NEW");

        // Any later registry access prunes. Use an unrelated file so nothing
        // about this call can resurrect the entry under test.
        let (_other_node, other, _) = probe_file(b"x");
        let _ = inode_cache_vmo(&other, "other", 1, 0, PAGE_SIZE, false);

        assert!(
            probe.writes.load(Ordering::SeqCst) > 0,
            "writeback never called the inode"
        );
        let mut buf = [0u8; 3];
        node.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"NEW", "the evicted cache was not written back");
    }

    /// ... and no part of that writeback runs under the registry lock. This is
    /// the regression: with the lock held, a filesystem write reaches inode
    /// locks that `read(2)` and `write(2)` already hold when they come the
    /// other way round through `cache_overlay_read` -- an AB-BA -- and any
    /// allocation it makes parks the holder on the heap lock with interrupts
    /// off.
    #[test]
    fn eviction_writeback_runs_with_the_registry_lock_down() {
        let (_node, wrapper, probe) = probe_file(b"old content, to be overwritten..");
        register_dirty_cache(wrapper, b"NEW");

        let (_other_node, other, _) = probe_file(b"x");
        let _ = inode_cache_vmo(&other, "other", 1, 0, PAGE_SIZE, false);

        assert!(
            probe.writes.load(Ordering::SeqCst) > 0,
            "writeback never ran, so this test proved nothing"
        );
        assert_eq!(
            probe.writes_under_lock.load(Ordering::SeqCst),
            0,
            "writeback ran inside the registry's critical section"
        );
    }

    /// `take_prunable` keeps what is still in use and takes only the dead,
    /// handing the entries out alive so the caller drops them outside the
    /// lock.
    #[test]
    fn take_prunable_keeps_live_entries_and_hands_dead_ones_out_alive() {
        let (_live_node, live, _) = probe_file(b"still mapped");
        let live_vmo = inode_cache_vmo(&live, "live", 12, 0, PAGE_SIZE, false)
            .expect("the cache vmo should have been created");

        let (_dead_node, dead, _) = probe_file(b"nobody holds this");
        register_dirty_cache(dead, b"Z");

        let evicted = {
            let mut registry = SHARED_FILE_VMOS.lock();
            take_prunable(&mut registry)
        };
        assert_eq!(evicted.len(), 1, "exactly the dead entry should be taken");
        for (vmo, _, _) in &evicted {
            assert_eq!(
                Arc::strong_count(vmo),
                1,
                "the evicted VMO must still be alive, for the caller to drop"
            );
        }
        finish_eviction(evicted);

        // The live one is untouched and still registered.
        assert!(
            cache_vmo_of(&live).is_some(),
            "a mapped cache was pruned out from under its mapper"
        );
        drop(live_vmo);
    }
}
