//! Linux Shared memory ipc
use super::*;
use crate::error::LxError;
use crate::time::TimeSpec;
use alloc::{collections::BTreeMap, sync::Arc, sync::Weak};
use kernel_hal::sync::{Mutex, RwLock};
use lazy_static::lazy_static;
use zircon_object::vm::*;

lazy_static! {
    static ref KEY2SHM: RwLock<BTreeMap<u32, Weak<Mutex<ShmGuard>>>> = RwLock::new(BTreeMap::new());
    /// Every shared segment in the system, under the id `shmget(2)` handed
    /// out for it. See [`shm_register`].
    static ref SHMID2SHM: RwLock<BTreeMap<ShmId, Arc<Mutex<ShmGuard>>>> =
        RwLock::new(BTreeMap::new());
    /// The next id to hand out. Ids are never reused, so a stale `shmid` kept
    /// by a program that already detached names nothing rather than somebody
    /// else's segment (Linux gets the same property from the sequence number
    /// it packs into the id).
    static ref NEXT_SHMID: Mutex<ShmId> = Mutex::new(1);
}

/// `SHMMNI`: how many segments may exist at once, Linux's own default.
const SHMMNI: usize = 4096;

/// Register a segment under a **system-wide** id and return it, or return the
/// id it already has.
///
/// `shmget(2)` returns an identifier that means the same thing in every
/// process: that is the whole mechanism by which two unrelated programs share
/// memory without a common ancestor, because the id is passed between them by
/// some other channel.
///
/// Here the id used to be an index into the CALLING process's own little map,
/// starting at 0 and counted per process — so a second program handed that id
/// either found nothing under it (`shmat` -> `EINVAL`) or, worse, found **its
/// own unrelated segment** at the same index and attached to that.
///
/// The X11 shared-memory extension is exactly this pattern, on every frame:
/// the client calls `shmget(IPC_PRIVATE, ...)`, sends the id to the X server
/// over the X socket (`XShmAttach`), and the server calls `shmat` on it. The
/// server has segments of its own, so the low ids were taken: the id the
/// client sent named a different buffer inside the server. Wayland passes a
/// `memfd` over the socket instead and never comes this way, which is the
/// shape of "it works under Wayland and not under X".
pub fn shm_register(guard: &Arc<Mutex<ShmGuard>>) -> Result<ShmId, LxError> {
    let mut table = SHMID2SHM.write();
    // `shmget` on an existing key answers with the id that key already has,
    // as Linux does -- not a second id for the same segment.
    if let Some((&id, _)) = table.iter().find(|(_, g)| Arc::ptr_eq(g, guard)) {
        return Ok(id);
    }
    // A segment outlives the process that made it: that is what shmget(2)
    // promises, and it is why `ipcs -m` can show one with nothing attached.
    // So the table is the only thing holding those alive, and without a bound
    // a loop of `shmget(IPC_PRIVATE, ...)` would pin memory for ever from an
    // ordinary process. Linux bounds it the same way and with the same error.
    if table.len() >= SHMMNI {
        return Err(LxError::ENOSPC);
    }
    let mut next = NEXT_SHMID.lock();
    let id = *next;
    *next += 1;
    table.insert(id, guard.clone());
    Ok(id)
}

/// The segment an id names, from any process.
pub fn shm_lookup(id: ShmId) -> Option<Arc<Mutex<ShmGuard>>> {
    SHMID2SHM.read().get(&id).cloned()
}

/// `shmctl(id, IPC_RMID, ..)`: the id stops naming the segment and no further
/// `shmat` can find it.
///
/// The memory itself lives on while anyone is attached -- `shmat` mapped the
/// VMO into the address space, which holds its own reference -- which is what
/// shmget(2) requires and what every user of the X11 extension depends on:
/// the idiom is `shmget` + `shmat` + `shmctl(IPC_RMID)` **immediately**, so
/// the segment cannot outlive the client that made it, and then the buffer is
/// used for the lifetime of the image.
pub fn shm_unregister(id: ShmId) -> bool {
    SHMID2SHM.write().remove(&id).is_some()
}

/// shmid data structure
///
/// struct shmid_ds
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmidDs {
    /// Ownership and permissions
    pub perm: IpcPerm,
    /// Size of segment (bytes)
    pub segsz: usize,
    /// Last attach time
    pub atime: usize,
    /// Last detach time
    pub dtime: usize,
    /// Last change time
    pub ctime: usize,
    /// PID of creator
    pub cpid: u32,
    /// PID of last shmat(2)/shmdt(2)
    pub lpid: u32,
    /// number of current attaches
    pub nattch: usize,
}

/// shared memory Identifier for process
#[derive(Clone)]
pub struct ShmIdentifier {
    /// Shared memory address
    pub addr: usize,
    /// Shared memory buffer and data
    pub guard: Arc<Mutex<ShmGuard>>,
}

/// shared memory buffer and data
pub struct ShmGuard {
    /// shared memory buffer
    pub shared_guard: Arc<VmObject>,
    /// shared memory data
    pub shmid_ds: Mutex<ShmidDs>,
}

impl ShmIdentifier {
    /// set the shared memory address on attach
    pub fn set_addr(&mut self, addr: usize) {
        self.addr = addr;
    }

    /// Get or create a ShmGuard, following shmget(2).
    ///
    /// `key == 0` is `IPC_PRIVATE`, and the whole point of it is that the
    /// caller gets a segment nobody else can name. This used to look `0` up in
    /// the key map like any other key, so the *second* `shmget(IPC_PRIVATE, ..)`
    /// in the lifetime of the first segment came back with that same segment:
    /// two unrelated buffers on the same pages, and a caller that asked for
    /// more bytes than the first one silently got the smaller VMO. X11's
    /// MIT-SHM allocates every image with `shmget(IPC_PRIVATE, ...)`, so an X
    /// session aliased all of them onto one buffer; Wayland, which uses
    /// `memfd`, never went through here. A private segment now bypasses the
    /// map on the way in and on the way out.
    ///
    /// The other half is the one `msgget` already had and this did not: a key
    /// that does not exist and no `IPC_CREAT` is `ENOENT`, not a silent
    /// create. Programs use a bare `shmget(key, 0, 0)` to ask whether a
    /// segment is there.
    pub fn new_shared_guard(
        key: u32,
        memsize: usize,
        flags: usize,
        cpid: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Mutex<ShmGuard>>, LxError> {
        let mut key2shm = KEY2SHM.write();
        let flag = IpcGetFlag::from_bits_truncate(flags);

        if key != 0 {
            // found in the map
            if let Some(weak_guard) = key2shm.get(&key) {
                if let Some(guard) = weak_guard.upgrade() {
                    if flag.contains(IpcGetFlag::CREAT) && flag.contains(IpcGetFlag::EXCLUSIVE) {
                        // exclusive
                        return Err(LxError::EEXIST);
                    }
                    return Ok(guard);
                }
            }
            if !flag.contains(IpcGetFlag::CREAT) {
                return Err(LxError::ENOENT);
            }
        }
        let shared_guard = Arc::new(Mutex::new(ShmGuard {
            shared_guard: {
                let vmo = VmObject::new_paged(pages(memsize));
                // A SysV segment is shared memory by definition; a fork must
                // never privatize an attached segment.
                vmo.set_share_on_fork();
                vmo
            },
            shmid_ds: Mutex::new(ShmidDs {
                perm: IpcPerm {
                    key,
                    uid,
                    gid,
                    cuid: uid,
                    cgid: gid,
                    // least significant 9 bits
                    mode: (flags as u32) & 0x1ff,
                    __seq: 0,
                    __pad1: 0,
                    __pad2: 0,
                },
                segsz: memsize,
                atime: 0,
                dtime: 0,
                ctime: TimeSpec::now().sec,
                cpid,
                lpid: 0,
                nattch: 0,
            }),
        }));
        // insert to global map — but never a private segment: it has no name,
        // so nothing may ever find it again.
        if key != 0 {
            key2shm.insert(key, Arc::downgrade(&shared_guard));
        }
        Ok(shared_guard)
    }
}

impl ShmGuard {
    /// set last attach time
    pub fn attach(&self, pid: u32) {
        let mut ds = self.shmid_ds.lock();
        ds.atime = TimeSpec::now().sec;
        ds.nattch += 1;
        ds.lpid = pid;
    }

    /// set last detach time
    pub fn detach(&self, pid: u32) {
        let mut ds = self.shmid_ds.lock();
        ds.dtime = TimeSpec::now().sec;
        // Guard against underflow if detach is called without a matching
        // attach, which would otherwise wrap `nattch` to a huge value and
        // prevent the segment from ever being cleaned up.
        ds.nattch = ds.nattch.saturating_sub(1);
        ds.lpid = pid;
    }

    /// set last change time
    pub fn ctime(&self) {
        self.shmid_ds.lock().ctime = TimeSpec::now().sec;
    }

    /// `IPC_SET`: owner and permission bits, per shmctl(2).
    ///
    /// Only the owner, the creator or a privileged caller may do this; anyone
    /// else gets `EPERM`. Without that check the `shmid_ds` userspace handed
    /// in rewrote `uid` and `gid`, so naming the id was enough to take the
    /// segment over -- see [`IpcPerm::may_control`].
    pub fn set(&self, new: &ShmidDs, euid: u32) -> Result<(), LxError> {
        let mut lock = self.shmid_ds.lock();
        if !lock.perm.may_control(euid) {
            return Err(LxError::EPERM);
        }
        lock.perm.uid = new.perm.uid;
        lock.perm.gid = new.perm.gid;
        lock.perm.mode = new.perm.mode & 0x1ff;
        Ok(())
    }

    /// Whether `euid` may `IPC_SET` or `IPC_RMID` this segment.
    pub fn may_control(&self, euid: u32) -> bool {
        self.shmid_ds.lock().perm.may_control(euid)
    }

    /// remove Shared memory
    ///
    /// A private segment carries key 0 and was never filed under it, so for
    /// one of those this finds nothing: the segment dies with its last `Arc`.
    pub fn remove(&self) {
        let mut key2shm = KEY2SHM.write();
        let key = self.shmid_ds.lock().perm.key;
        key2shm.remove(&key);
    }
}

/// System V shared memory had no tests, and `shmget` had the same two gaps as
/// `semget` — with a much sharper edge on one of them.
///
/// `shmget(IPC_PRIVATE, size, ...)` is how X11's MIT-SHM allocates every
/// single image, and how most code asks for "a shared buffer that is mine".
/// Key 0 was looked up in the key table like any other key, so from the second
/// call onwards, for as long as the first segment stayed alive, every caller
/// got **that same segment back**: unrelated buffers on the same pages, and a
/// caller asking for more bytes than the first one silently handed a VMO too
/// small for what it was about to write. Wayland allocates through `memfd` and
/// never comes here, which is the shape of "it works under Wayland and not
/// under X".
#[cfg(test)]
mod shm_tests {
    use super::*;

    extern crate std;

    /// `IPC_CREAT`, as userspace spells it.
    const CREAT: usize = 0o1000;
    /// `IPC_EXCL`.
    const EXCL: usize = 0o2000;
    /// Any pid; the field is only bookkeeping here.
    const PID: u32 = 42;
    /// `euid == 0`.
    const ROOT: u32 = 0;
    /// The owner in the ownership tests.
    const OWNER: u32 = 1000;
    /// Somebody else.
    const STRANGER: u32 = 1001;

    /// `KEY2SHM` is process-wide and cargo runs a crate's tests in threads.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(key: u32, size: usize, flags: usize) -> Result<Arc<Mutex<ShmGuard>>, LxError> {
        owned_get(key, size, flags, ROOT)
    }

    /// `shmget` as `uid` -- the creator `shmctl` then measures a caller
    /// against.
    fn owned_get(
        key: u32,
        size: usize,
        flags: usize,
        uid: u32,
    ) -> Result<Arc<Mutex<ShmGuard>>, LxError> {
        ShmIdentifier::new_shared_guard(key, size, flags, PID, uid, uid)
    }

    /// Clear the system-wide id table between tests: it is process-wide and
    /// other tests in this module create segments too.
    fn clear_ids() {
        SHMID2SHM.write().clear();
    }

    // ---- the id `shmget` hands out ---------------------------------------

    /// The bug that broke the X11 shared-memory extension. The id was an
    /// index into the CALLING process's own map, counted from 0 per process,
    /// so the number a client sent to the X server named a different segment
    /// there -- or nothing at all. It has to mean the same segment
    /// everywhere, because passing it to another program is what it is for.
    #[test]
    fn an_id_names_the_same_segment_from_anywhere() {
        let _guard = test_lock();
        clear_ids();
        let client_image = get(0, 4096, CREAT | 0o666).unwrap();
        let id = shm_register(&client_image).unwrap();
        // The other program has only the number.
        let seen_by_the_server = shm_lookup(id).unwrap();
        assert!(Arc::ptr_eq(&seen_by_the_server, &client_image));
        // And what it writes is what the client reads.
        seen_by_the_server
            .lock()
            .shared_guard
            .write(0, b"un frame")
            .unwrap();
        let mut back = [0u8; 8];
        client_image.lock().shared_guard.read(0, &mut back).unwrap();
        assert_eq!(&back, b"un frame");
    }

    /// Two segments never share an id, whoever made them. Per-process
    /// counting gave both of them 0.
    #[test]
    fn two_segments_never_share_an_id() {
        let _guard = test_lock();
        clear_ids();
        let a = get(0, 4096, CREAT | 0o666).unwrap();
        let b = get(0, 4096, CREAT | 0o666).unwrap();
        let ia = shm_register(&a).unwrap();
        let ib = shm_register(&b).unwrap();
        assert_ne!(ia, ib);
        assert!(Arc::ptr_eq(&shm_lookup(ia).unwrap(), &a));
        assert!(Arc::ptr_eq(&shm_lookup(ib).unwrap(), &b));
    }

    /// `shmget` on a key that already has a segment answers with the id that
    /// segment already has, as Linux does.
    #[test]
    fn the_same_segment_keeps_the_same_id() {
        let _guard = test_lock();
        clear_ids();
        let first = get(0x515e, 4096, CREAT | 0o666).unwrap();
        let again = get(0x515e, 4096, 0o666).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(shm_register(&first).unwrap(), shm_register(&again).unwrap());
    }

    /// An id is never handed out twice, so a program holding a stale one
    /// finds nothing rather than somebody else's memory.
    #[test]
    fn an_id_is_never_reused() {
        let _guard = test_lock();
        clear_ids();
        let a = get(0, 4096, CREAT | 0o666).unwrap();
        let id = shm_register(&a).unwrap();
        assert!(shm_unregister(id));
        drop(a);
        let b = get(0, 4096, CREAT | 0o666).unwrap();
        assert_ne!(shm_register(&b).unwrap(), id);
        assert!(shm_lookup(id).is_none());
    }

    /// `shmctl(id, IPC_RMID)` is what every user of the X11 extension calls
    /// the moment it has attached: the id must stop working while the memory
    /// stays alive for whoever is already attached.
    #[test]
    fn removing_the_id_does_not_take_the_memory_with_it() {
        let _guard = test_lock();
        clear_ids();
        let attached = get(0, 4096, CREAT | 0o666).unwrap();
        let id = shm_register(&attached).unwrap();
        assert!(shm_unregister(id));
        assert!(shm_lookup(id).is_none());
        // Still writable by whoever holds it.
        attached
            .lock()
            .shared_guard
            .write(0, b"sigue viva")
            .unwrap();
        // And removing it twice is not an error the second time round.
        assert!(!shm_unregister(id));
    }

    /// A segment outlives the process that created it, so the table is the
    /// only thing keeping it alive -- and a loop of `shmget` would pin memory
    /// for ever. Linux bounds it at `SHMMNI` and answers `ENOSPC`.
    #[test]
    fn the_number_of_segments_is_bounded() {
        let _guard = test_lock();
        clear_ids();
        {
            let mut table = SHMID2SHM.write();
            let one = get(0, 4096, CREAT | 0o666).unwrap();
            for i in 0..SHMMNI {
                table.insert(1_000_000 + i, one.clone());
            }
        }
        let extra = get(0, 4096, CREAT | 0o666).unwrap();
        assert_eq!(shm_register(&extra), Err(LxError::ENOSPC));
        clear_ids();
        // The number is Linux's, so it is asserted apart from the code that
        // uses it -- a test built out of the constant moves with it.
        assert_eq!(SHMMNI, 4096);
    }

    /// The per-process map records where this process attached the segment,
    /// and that is what `shmdt(addr)` looks up. A later `shmget` for the same
    /// id must not reset it to 0, or the detach finds nothing and the mapping
    /// stays in the address space for good.
    #[test]
    fn asking_again_for_a_segment_does_not_forget_where_it_is_attached() {
        let _guard = test_lock();
        let seg = get(0, 4096, CREAT | 0o666).unwrap();
        let mut proc = crate::ipc::ShmProc::default();
        proc.add(7, seg.clone());
        proc.set(
            7,
            ShmIdentifier {
                addr: 0x4000,
                guard: seg.clone(),
            },
        );
        proc.add(7, seg.clone());
        assert_eq!(proc.get(7).unwrap().addr, 0x4000);
        assert_eq!(proc.get_id(0x4000), Some(7));
        proc.pop(7);
        assert!(proc.get(7).is_none());
    }

    #[test]
    fn a_number_that_names_nothing_is_not_a_segment() {
        let _guard = test_lock();
        clear_ids();
        assert!(shm_lookup(0).is_none());
        assert!(shm_lookup(999_999).is_none());
    }

    #[test]
    fn a_private_segment_is_fresh_every_time() {
        let _guard = test_lock();
        // The first one is kept alive on purpose: that is the whole condition
        // for the bug — an existing key-0 entry to alias onto.
        let a = get(0, 4096, CREAT | 0o666).unwrap();
        let b = get(0, 4096, CREAT | 0o666).unwrap();
        let c = get(0, 4096, CREAT | 0o666).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        assert!(!Arc::ptr_eq(&b, &c));
    }

    #[test]
    fn a_private_segment_leaves_nothing_behind_in_the_key_table() {
        let _guard = test_lock();
        // The invariant the fix rests on, stated where a later edit will trip
        // over it: key 0 is never a row in the table, so no lookup can come
        // back with somebody's private segment.
        let _private = get(0, 4096, CREAT | 0o666).unwrap();
        assert!(!KEY2SHM.read().contains_key(&0));
    }

    #[test]
    fn a_private_segment_never_needs_ipc_creat() {
        let _guard = test_lock();
        // shmget(2): with IPC_PRIVATE a new segment is created whatever the
        // flags say, so the ENOENT rule must not reach this path.
        let a = get(0, 8192, 0o666).unwrap();
        assert_eq!(a.lock().shared_guard.len(), 8192);
    }

    #[test]
    fn two_private_segments_do_not_share_memory() {
        let _guard = test_lock();
        // This is what the aliasing actually did to a program: one buffer's
        // pixels turning up in another's.
        let a = get(0, 4096, CREAT | 0o666).unwrap();
        let b = get(0, 4096, CREAT | 0o666).unwrap();
        a.lock().shared_guard.write(0, b"del primero").unwrap();
        b.lock().shared_guard.write(0, b"del segundo").unwrap();
        let mut buf = [0u8; 11];
        a.lock().shared_guard.read(0, &mut buf).unwrap();
        assert_eq!(&buf, b"del primero");
    }

    #[test]
    fn a_private_segment_gets_the_size_it_asked_for() {
        let _guard = test_lock();
        // The nastiest form of the aliasing: ask for 64 KiB, be handed the
        // 4 KiB segment somebody else made first, then write 64 KiB into it.
        let _small = get(0, 4096, CREAT | 0o666).unwrap();
        let big = get(0, 64 * 1024, CREAT | 0o666).unwrap();
        assert_eq!(big.lock().shared_guard.len(), 64 * 1024);
        assert_eq!(big.lock().shmid_ds.lock().segsz, 64 * 1024);
        // And the far end of it is really there.
        big.lock()
            .shared_guard
            .write(64 * 1024 - 4, b"fin")
            .unwrap();
    }

    #[test]
    fn a_private_segment_records_key_zero() {
        let _guard = test_lock();
        let a = get(0, 4096, CREAT | 0o666).unwrap();
        assert_eq!(a.lock().shmid_ds.lock().perm.key, 0);
    }

    #[test]
    fn a_key_that_does_not_exist_without_ipc_creat_is_enoent() {
        let _guard = test_lock();
        // A bare shmget is how a program asks whether a segment is there. It
        // used to make one and answer "yes".
        assert_eq!(get(0x5b_0001, 4096, 0o666).err(), Some(LxError::ENOENT));
        let made = get(0x5b_0001, 4096, CREAT | 0o666).unwrap();
        let found = get(0x5b_0001, 4096, 0o666).unwrap();
        assert!(Arc::ptr_eq(&made, &found));
    }

    #[test]
    fn the_same_key_comes_back_as_the_same_segment() {
        let _guard = test_lock();
        let a = get(0x5b_0002, 4096, CREAT | 0o666).unwrap();
        let b = get(0x5b_0002, 4096, CREAT | 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        a.lock().shared_guard.write(0, b"compartido").unwrap();
        let mut buf = [0u8; 10];
        b.lock().shared_guard.read(0, &mut buf).unwrap();
        assert_eq!(&buf, b"compartido");
    }

    #[test]
    fn creat_and_excl_together_on_an_existing_key_is_eexist() {
        let _guard = test_lock();
        let _a = get(0x5b_0003, 4096, CREAT | 0o666).unwrap();
        assert_eq!(
            get(0x5b_0003, 4096, CREAT | EXCL | 0o666).err(),
            Some(LxError::EEXIST)
        );
    }

    #[test]
    fn excl_without_creat_returns_the_segment_that_is_there() {
        let _guard = test_lock();
        // shmget(2): IPC_EXCL only means anything alongside IPC_CREAT.
        let a = get(0x5b_0004, 4096, CREAT | 0o666).unwrap();
        let b = get(0x5b_0004, 4096, EXCL | 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn dropping_the_last_reference_frees_the_key() {
        let _guard = test_lock();
        let a = get(0x5b_0005, 4096, CREAT | 0o666).unwrap();
        drop(a);
        assert_eq!(get(0x5b_0005, 4096, 0o666).err(), Some(LxError::ENOENT));
    }

    #[test]
    fn removing_a_segment_frees_its_key_even_while_it_is_still_referenced() {
        let _guard = test_lock();
        let a = get(0x5b_0006, 4096, CREAT | 0o666).unwrap();
        a.lock().remove();
        assert_eq!(get(0x5b_0006, 4096, 0o666).err(), Some(LxError::ENOENT));
        let b = get(0x5b_0006, 4096, CREAT | 0o666).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn removing_a_private_segment_touches_nobody_elses_key() {
        let _guard = test_lock();
        // A private segment carries key 0, and `remove` used to take key 0 out
        // of the table — which, back when private segments were filed under
        // key 0, took the *next* caller's segment with it.
        let keyed = get(0x5b_0007, 4096, CREAT | 0o666).unwrap();
        let private = get(0, 4096, CREAT | 0o666).unwrap();
        private.lock().remove();
        let still_there = get(0x5b_0007, 4096, 0o666).unwrap();
        assert!(Arc::ptr_eq(&keyed, &still_there));
    }

    #[test]
    fn the_size_is_rounded_up_to_whole_pages_but_segsz_is_what_was_asked() {
        let _guard = test_lock();
        // shmat maps `vmo.len()`, so a segment of 100 bytes must still be a
        // whole page of mapping; `shmid_ds.segsz` is what shmctl(IPC_STAT)
        // reports and stays the requested size, as Linux does.
        let a = get(0, 100, CREAT | 0o666).unwrap();
        assert_eq!(a.lock().shared_guard.len(), 4096);
        assert_eq!(a.lock().shmid_ds.lock().segsz, 100);
        let b = get(0, 4097, CREAT | 0o666).unwrap();
        assert_eq!(b.lock().shared_guard.len(), 8192);
    }

    #[test]
    fn a_segment_survives_a_fork_as_shared_memory() {
        let _guard = test_lock();
        // A System V segment is shared by definition: were it privatised on
        // fork, a parent and child sharing a buffer would stop seeing each
        // other's writes.
        let a = get(0, 4096, CREAT | 0o666).unwrap();
        assert!(a.lock().shared_guard.is_shared_object());
    }

    #[test]
    fn attaching_and_detaching_counts_and_never_goes_below_zero() {
        let _guard = test_lock();
        let a = get(0, 4096, CREAT | 0o666).unwrap();
        let g = a.lock();
        assert_eq!(g.shmid_ds.lock().nattch, 0);
        g.attach(7);
        g.attach(8);
        assert_eq!(g.shmid_ds.lock().nattch, 2);
        assert_eq!(g.shmid_ds.lock().lpid, 8);
        g.detach(8);
        assert_eq!(g.shmid_ds.lock().nattch, 1);
        // A detach with no attach behind it must not wrap the count to
        // 2^64-1, which would keep the segment alive for ever.
        g.detach(9);
        g.detach(9);
        assert_eq!(g.shmid_ds.lock().nattch, 0);
        assert_eq!(g.shmid_ds.lock().lpid, 9);
    }

    #[test]
    fn the_shmid_ds_records_the_key_the_mode_and_the_creator() {
        let _guard = test_lock();
        let a = get(0x5b_0008, 4096, CREAT | 0o1666).unwrap();
        let g = a.lock();
        let ds = *g.shmid_ds.lock();
        assert_eq!(ds.perm.key, 0x5b_0008);
        assert_eq!(ds.cpid, PID);
        // Only the low nine bits are permissions; IPC_CREAT is not one.
        assert_eq!(ds.perm.mode, 0o666);
        assert_eq!(ds.atime, 0);
        assert_eq!(ds.dtime, 0);
    }

    #[test]
    fn ipc_set_takes_the_owner_and_the_low_nine_bits_of_the_mode() {
        let _guard = test_lock();
        let a = get(0x5b_0009, 4096, CREAT | 0o666).unwrap();
        let g = a.lock();
        let mut ds = *g.shmid_ds.lock();
        ds.perm.uid = 1000;
        ds.perm.gid = 1001;
        ds.perm.mode = 0o7654;
        ds.segsz = 1;
        assert_eq!(g.set(&ds, ROOT), Ok(()));
        let now = *g.shmid_ds.lock();
        assert_eq!(now.perm.uid, 1000);
        assert_eq!(now.perm.gid, 1001);
        assert_eq!(now.perm.mode, 0o654);
        assert_eq!(now.perm.key, 0x5b_0009, "IPC_SET never moves the key");
        assert_eq!(now.segsz, 4096, "nor resizes the segment");
    }

    // ---- who may change the segment --------------------------------------

    /// `shmget` filled `uid`, `gid`, `cuid` and `cgid` with zeros, whoever
    /// called it, so `shmctl(IPC_STAT)` reported every segment in the system
    /// as root's and there was nothing for a permission check to read.
    #[test]
    fn shmget_records_the_caller_as_owner_and_creator() {
        let _guard = test_lock();
        let a =
            ShmIdentifier::new_shared_guard(0, 4096, CREAT | 0o666, PID, OWNER, OWNER + 5).unwrap();
        let perm = a.lock().shmid_ds.lock().perm;
        assert_eq!(perm.uid, OWNER);
        assert_eq!(perm.gid, OWNER + 5);
        assert_eq!(perm.cuid, OWNER);
        assert_eq!(perm.cgid, OWNER + 5);
    }

    /// The whole of the old `set`: uid, gid and mode straight out of the
    /// buffer userspace handed in, with nobody asked, so naming the id was
    /// enough to take the segment over (shmctl(2): `EPERM`).
    #[test]
    fn ipc_set_from_a_stranger_is_eperm_and_changes_nothing() {
        let _guard = test_lock();
        let a = owned_get(0, 4096, CREAT | 0o600, OWNER).unwrap();
        let g = a.lock();
        let mut ds = *g.shmid_ds.lock();
        ds.perm.uid = STRANGER;
        ds.perm.mode = 0o666;
        assert_eq!(g.set(&ds, STRANGER), Err(LxError::EPERM));
        let now = g.shmid_ds.lock().perm;
        assert_eq!(now.uid, OWNER);
        assert_eq!(now.mode, 0o600);
        assert_eq!(g.set(&ds, OWNER), Ok(()));
        assert_eq!(g.shmid_ds.lock().perm.uid, STRANGER);
    }

    /// The same predicate gates `IPC_RMID`, which is how `shmctl` decides
    /// whether a caller may destroy somebody else's segment.
    #[test]
    fn only_the_owner_the_creator_and_root_may_control_a_segment() {
        let _guard = test_lock();
        let a = owned_get(0, 4096, CREAT | 0o666, OWNER).unwrap();
        let g = a.lock();
        assert!(g.may_control(OWNER));
        assert!(g.may_control(ROOT));
        assert!(!g.may_control(STRANGER));
        // Handed over, the creator keeps its rights (ipcctl_obtain_check).
        let mut ds = *g.shmid_ds.lock();
        ds.perm.uid = STRANGER;
        assert_eq!(g.set(&ds, OWNER), Ok(()));
        assert!(g.may_control(OWNER));
        assert!(g.may_control(STRANGER));
    }
}
