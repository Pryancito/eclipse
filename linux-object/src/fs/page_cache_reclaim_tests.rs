//! What this reclaimer has to get right is not how much it frees, it is what
//! it refuses to free: a mapped entry (page tables point at those frames) and
//! a `MAP_SHARED` entry with no file behind it (the VMO *is* the storage).
//! Each has a test here, next to the ones for the writeback that must happen
//! before the frames go.
//!
//! Every test asserts DELTAS and IDENTITY, never a total: the registry is one
//! for the whole binary, and another module's test may well have an entry in
//! it.

use super::*;
use crate::fs::file::{inode_cache_vmo, REGISTRY_TEST_LOCK};
use core::sync::atomic::{AtomicUsize, Ordering};
use rcore_fs::vfs::{FileSystem, FileType, FsError, Metadata, PollStatus};
use rcore_fs_ramfs::RamFS;

/// A ramfs file that counts what was written back to it.
struct ProbeInode {
    inner: Arc<dyn INode>,
    writes: Arc<AtomicUsize>,
}

impl INode for ProbeInode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, FsError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
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

/// What `probe_file` hands back: the filesystem root (to unlink through), the
/// raw node (to read back through), the probe wrapper over it, and the probe's
/// write counter.
type ProbeFile = (
    Arc<dyn INode>,
    Arc<dyn INode>,
    Arc<dyn INode>,
    Arc<AtomicUsize>,
);

/// A fresh ramfs holding one file of `content`.
fn probe_file(content: &[u8]) -> ProbeFile {
    let fs = RamFS::new();
    let root = fs.root_inode();
    let node = root
        .create("f", FileType::File, 0o777)
        .expect("create failed");
    node.resize(content.len()).expect("resize failed");
    node.write_at(0, content).expect("write failed");
    let writes = Arc::new(AtomicUsize::new(0));
    let wrapper: Arc<dyn INode> = Arc::new(ProbeInode {
        inner: node.clone(),
        writes: writes.clone(),
    });
    (root, node, wrapper, writes)
}

/// Give `inode` a page cache with its first page resident, and drop the VMO
/// handle so that the registry holds the only reference: an entry nothing maps
/// any more, which is what this reclaimer is about.
///
/// `dirty` is written THROUGH the cache, so the page differs from the file and
/// only writeback can reconcile them.
fn cache_one_page(inode: &Arc<dyn INode>, shared: bool, dirty: Option<&[u8]>) {
    let size = inode.metadata().unwrap().size;
    let vmo = inode_cache_vmo(inode, "probe", size, 0, PAGE_SIZE, shared)
        .expect("the cache vmo should have been created");
    if let Some(bytes) = dirty {
        vmo.write(0, bytes).expect("dirtying the cache failed");
    } else {
        // A clean resident page still has to BE resident, or there is nothing
        // to reclaim and the test proves nothing.
        let mut buf = [0u8; 8];
        vmo.read(0, &mut buf).expect("faulting the page failed");
    }
    drop(vmo);
}

/// Clear out whatever earlier tests left prunable, and keep the registry from
/// shrinking again while this test measures.
///
/// `take_prunable` runs on EVERY registry access, so a test that reads a
/// figure, creates an entry and reads the figure again is also measuring the
/// dead entries its own creation pruned -- the first version of the test below
/// measured exactly zero that way. Touching an unrelated file first does the
/// pruning up front, and holding its handles means nothing of this one's is
/// prunable either.
fn settle_registry() -> (Arc<dyn INode>, Arc<VmObject>) {
    let (_root, _node, wrapper, _) = probe_file(b"settle");
    let vmo = inode_cache_vmo(&wrapper, "settle", 6, 0, PAGE_SIZE, false)
        .expect("the cache vmo should have been created");
    (wrapper, vmo)
}

/// An entry nobody maps is counted, and its figure is its resident pages --
/// not the length of the file it caches.
#[test]
fn an_unmapped_entry_is_counted_by_its_resident_pages() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let _settled = settle_registry();
    let before = count();
    // Four pages long, one page resident.
    let (_root, _node, wrapper, _) = probe_file(&[7u8; 4 * PAGE_SIZE]);
    cache_one_page(&wrapper, false, None);
    assert_eq!(
        count() - before,
        PAGE_SIZE,
        "the count is not the resident bytes of the new entry"
    );
}

/// An entry something still maps is neither counted nor evicted. Those frames
/// are in a page table; handing them back is a use-after-free with the MMU as
/// the accomplice.
#[test]
fn an_entry_that_is_still_mapped_is_left_alone() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let _settled = settle_registry();
    let before = count();
    let (_root, _node, wrapper, _) = probe_file(&[1u8; PAGE_SIZE]);
    let size = wrapper.metadata().unwrap().size;
    // The live handle IS the mapping, as far as the registry can tell.
    let mapped = inode_cache_vmo(&wrapper, "mapped", size, 0, PAGE_SIZE, false)
        .expect("the cache vmo should have been created");
    let mut buf = [0u8; 8];
    mapped.read(0, &mut buf).unwrap();

    assert_eq!(count(), before, "a mapped entry was counted as reclaimable");
    assert_eq!(scan(8 * PAGE_SIZE), 0, "a mapped entry was evicted");
    assert!(
        crate::fs::file::cache_vmo_of(&wrapper).is_some(),
        "the mapped entry was taken out of the registry"
    );
    drop(mapped);
}

/// A `MAP_SHARED` entry whose file is unlinked is a memfd or an unlinked shm
/// pool: the VMO is its only storage, there is nowhere to write it back to,
/// and evicting it loses data instead of freeing memory. The wl_keyboard
/// keymap lives exactly there.
#[test]
fn a_shared_entry_with_no_file_behind_it_is_never_evicted() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let _settled = settle_registry();
    let before = count();
    let (root, _node, wrapper, _) = probe_file(&[3u8; PAGE_SIZE]);
    cache_one_page(&wrapper, true, Some(b"KEYMAP"));
    root.unlink("f").expect("unlink failed");
    assert_eq!(
        wrapper.metadata().unwrap().nlinks,
        0,
        "this test needs an unlinked file"
    );

    assert_eq!(
        count(),
        before,
        "an unlinked shared entry was counted as reclaimable"
    );
    assert_eq!(
        scan(8 * PAGE_SIZE),
        0,
        "an unlinked shared entry was evicted"
    );
    assert!(
        crate::fs::file::cache_vmo_of(&wrapper).is_some(),
        "the shared pages of an unlinked file were thrown away"
    );
}

/// The ordinary case: an entry nothing maps, whose file is still there, goes
/// out of the registry and its resident bytes are what the pass answers.
#[test]
fn an_unmapped_entry_of_a_live_file_is_evicted_and_its_bytes_reported() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let (_root, _node, wrapper, _) = probe_file(&[9u8; 2 * PAGE_SIZE]);
    cache_one_page(&wrapper, false, None);
    assert!(
        crate::fs::file::cache_vmo_of(&wrapper).is_some(),
        "the entry under test was never registered"
    );

    let freed = scan(PAGE_SIZE);
    assert!(
        freed >= PAGE_SIZE,
        "the pass freed {} bytes, less than the one resident page it had",
        freed
    );
    assert!(
        crate::fs::file::cache_vmo_of(&wrapper).is_none(),
        "the entry was reported as freed but is still registered"
    );
}

/// Dirty pages reach the file BEFORE the frames go. Without this, reclaim
/// turns a `MAP_SHARED` store into silent corruption -- which is what makes
/// writeback the reason this cache cannot be a direct shrinker.
#[test]
fn a_dirty_entry_is_written_back_before_its_frames_go() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let (_root, node, wrapper, writes) = probe_file(b"old content, to be overwritten..");
    cache_one_page(&wrapper, true, Some(b"NEW"));

    assert!(scan(PAGE_SIZE) >= PAGE_SIZE, "nothing was reclaimed at all");
    assert!(
        writes.load(Ordering::SeqCst) > 0,
        "the evicted entry was never written back"
    );
    let mut buf = [0u8; 3];
    node.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf, b"NEW", "the reclaimed page never reached the file");
}

/// The pass stops once the target is met, rather than emptying the cache: a
/// 4 MiB shortage must not cost the machine its whole page cache.
#[test]
fn the_pass_stops_once_the_target_is_met() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let files: Vec<_> = (0..3).map(|_| probe_file(&[5u8; PAGE_SIZE])).collect();
    for (_, _, wrapper, _) in files.iter() {
        cache_one_page(wrapper, false, None);
    }

    assert_eq!(
        scan(PAGE_SIZE),
        PAGE_SIZE,
        "the pass freed more than the one page it was asked for"
    );
    let left = files
        .iter()
        .filter(|(_, _, wrapper, _)| crate::fs::file::cache_vmo_of(wrapper).is_some())
        .count();
    assert_eq!(left, 2, "the pass emptied the cache instead of stopping");
}

/// The re-check under the lock, which is what makes planning outside it safe.
/// Only "this very VMO, and still nobody but the registry and the plan hold
/// it" may be taken out.
#[test]
fn only_an_entry_still_held_by_the_registry_alone_is_taken() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let (_root, _node, wrapper, _) = probe_file(&[2u8; PAGE_SIZE]);
    cache_one_page(&wrapper, false, None);
    let key = crate::fs::file::cache_key(&wrapper);
    let vmo = crate::fs::file::cache_vmo_of(&wrapper).expect("not registered");

    let registry = SHARED_FILE_VMOS.lock();
    // The registry's reference plus the one the plan holds: the state a pass
    // may act on.
    assert!(
        still_unmapped(&registry, &key, &vmo),
        "the entry the plan holds was refused"
    );
    // One more holder is a mapping that appeared while the plan was being
    // made.
    let mapper = vmo.clone();
    assert!(
        !still_unmapped(&registry, &key, &vmo),
        "an entry somebody mapped after the plan was still taken"
    );
    drop(mapper);
    // A key that is no longer there, and a key whose entry is a different VMO.
    assert!(
        !still_unmapped(&registry, &(usize::MAX, usize::MAX), &vmo),
        "a key the registry does not hold was accepted"
    );
    let other = VmObject::new_paged(1);
    assert!(
        !still_unmapped(&registry, &key, &other),
        "a different VMO under the same key was accepted"
    );
}

/// The figure a pass answers with is what came OUT of the registry, not what
/// it planned to take. An entry somebody mapped while the plan was being made
/// stays, and its bytes are not memory anybody can use.
#[test]
fn the_bytes_reported_are_the_ones_that_came_out() {
    let _g = REGISTRY_TEST_LOCK.lock();
    let (_root, _node, wrapper, _) = probe_file(&[4u8; PAGE_SIZE]);
    cache_one_page(&wrapper, false, None);
    let vmo = crate::fs::file::cache_vmo_of(&wrapper).expect("not registered");
    let key = crate::fs::file::cache_key(&wrapper);

    let kept = VmObject::new_paged(1);
    let plan = alloc::vec![
        (key, vmo.clone(), PAGE_SIZE),
        ((usize::MAX, usize::MAX), kept, 4 * PAGE_SIZE),
    ];
    // Only the first one came out.
    let evicted: EvictedVmos = alloc::vec![(vmo, Weak::<ProbeInode>::new(), false)];
    assert_eq!(
        freed_bytes(&plan, &evicted),
        PAGE_SIZE,
        "bytes that stayed in the registry were reported as freed"
    );
    assert_eq!(
        freed_bytes(&plan, &EvictedVmos::new()),
        0,
        "a pass that took nothing out still reported bytes"
    );
}
