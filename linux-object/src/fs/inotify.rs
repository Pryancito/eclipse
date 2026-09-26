//! Minimal `inotify(7)` implementation.
//!
//! labwc (and many GTK apps) create an inotify instance at startup to watch
//! their config directory for live-reload. This kernel had no inotify, so the
//! call returned `unknown syscall: INOTIFY_INIT1` (ENOSYS) — on a real-hardware
//! bring-up that aborted labwc's config-watcher setup before the compositor
//! ever came up.
//!
//! This is a *functional stub*: `inotify_init1` returns a real, pollable fd and
//! `inotify_add_watch`/`inotify_rm_watch` hand out and track watch descriptors,
//! but no filesystem here delivers change events, so the fd simply never
//! becomes readable. That is exactly the "no events" state a quiescent inotify
//! fd is already allowed to be in — the watcher exists and polls cleanly, the
//! client runs, and config hot-reload is silently disabled rather than fatal.

use super::*;
use crate::sync::{Event, EventBus};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use lock::Mutex;
use zircon_object::object::*;

/// inotify instance backing an `inotify_init1(2)` fd.
pub struct Inotify {
    base: KObjectBase,
    inner: Arc<Mutex<InotifyInner>>,
    eventbus: Arc<Mutex<EventBus>>,
    /// Behind a lock so `fcntl(F_SETFL)` can change it after creation.
    flags: Mutex<OpenFlags>,
}

#[derive(Default)]
struct InotifyInner {
    /// Next watch descriptor to hand out. Linux watch descriptors are small
    /// positive ints, unique per inotify instance and monotonically assigned.
    next_wd: i32,
    /// wd -> (pathname, mask). Kept so `inotify_add_watch` on an already
    /// watched path returns the SAME wd (Linux semantics) and `inotify_rm_watch`
    /// can validate the descriptor.
    watches: BTreeMap<i32, (String, u32)>,
}

impl_kobject!(Inotify);

/// `IN_ALL_EVENTS`: the twelve event bits a watch may ask for.
pub const IN_ALL_EVENTS: u32 = 0x0000_0fff;
/// `IN_UNMOUNT`, `IN_Q_OVERFLOW`, `IN_IGNORED`: the three the kernel sends
/// on its own, which a caller may name in a mask without it being an error.
const IN_KERNEL_EVENTS: u32 = 0x0000_2000 | 0x0000_4000 | 0x0000_8000;
/// `IN_ONLYDIR`: watch the path only if it is a directory (`ENOTDIR`).
pub const IN_ONLYDIR: u32 = 0x0100_0000;
/// `IN_DONT_FOLLOW`: do not dereference a symlink at the end of the path.
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
/// `IN_EXCL_UNLINK`: stop events for children once they are unlinked.
pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
/// `IN_MASK_CREATE`: this call must create a watch, `EEXIST` if one exists.
pub const IN_MASK_CREATE: u32 = 0x1000_0000;
/// `IN_MASK_ADD`: merge the events into an existing watch's mask.
pub const IN_MASK_ADD: u32 = 0x2000_0000;
/// `IN_ISDIR`: an event flag the kernel sets; harmless in a mask.
const IN_ISDIR: u32 = 0x4000_0000;
/// `IN_ONESHOT`: remove the watch after its first event.
pub const IN_ONESHOT: u32 = 0x8000_0000;
/// `ALL_INOTIFY_BITS`: every bit `inotify_add_watch` knows. A mask with no
/// bit in it is `EINVAL`.
pub const ALL_INOTIFY_BITS: u32 = IN_ALL_EVENTS
    | IN_KERNEL_EVENTS
    | IN_ONLYDIR
    | IN_DONT_FOLLOW
    | IN_EXCL_UNLINK
    | IN_MASK_CREATE
    | IN_MASK_ADD
    | IN_ISDIR
    | IN_ONESHOT;

/// How `inotify_add_watch` must look the path up, decided from the mask
/// before the path is touched.
#[derive(Debug, PartialEq, Eq)]
pub struct WatchLookup {
    /// Dereference a trailing symlink (`!IN_DONT_FOLLOW`, `LOOKUP_FOLLOW`).
    pub follow: bool,
    /// The path must be a directory (`IN_ONLYDIR`, `LOOKUP_DIRECTORY`).
    pub only_dir: bool,
}

/// `inotify_add_watch`'s two refusals of the mask, in its order: no known
/// bit at all (`!(mask & ALL_INOTIFY_BITS)`, `EINVAL`), then `IN_MASK_ADD`
/// together with `IN_MASK_CREATE` (`EINVAL`, they contradict). Then how to
/// look the path up.
///
/// Nothing was checked, and the path was never looked up at all: a mask of
/// zero took a watch, `IN_MASK_ADD|IN_MASK_CREATE` took one, and
/// `inotifywait /nonexistent` reported success and waited forever where
/// Linux says `ENOENT`, which is the answer glib's `GFileMonitor`, systemd's
/// path units and Python's watchdog read to fall back to watching the
/// parent directory.
pub fn inotify_watch_lookup(mask: u32) -> LxResult<WatchLookup> {
    if mask & ALL_INOTIFY_BITS == 0 {
        return Err(LxError::EINVAL);
    }
    if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(WatchLookup {
        follow: mask & IN_DONT_FOLLOW == 0,
        only_dir: mask & IN_ONLYDIR != 0,
    })
}

impl Inotify {
    /// Create an inotify instance. `flags` carries `IN_NONBLOCK`/`IN_CLOEXEC`,
    /// which share the `O_NONBLOCK`/`O_CLOEXEC` bit values.
    pub fn new(flags: OpenFlags) -> Arc<Self> {
        Arc::new(Inotify {
            base: KObjectBase::new(),
            inner: Arc::new(Mutex::new(InotifyInner {
                next_wd: 1,
                watches: BTreeMap::new(),
            })),
            eventbus: EventBus::new(),
            flags: Mutex::new(flags),
        })
    }

    /// Add or update a watch. Returns the watch descriptor. Re-watching an
    /// existing path returns its existing wd, and what happens to its mask
    /// is `inotify_update_existing_watch`'s: `IN_MASK_CREATE` refuses with
    /// `EEXIST` (the caller asked for a new watch and there is one),
    /// `IN_MASK_ADD` merges the events into the old mask, and neither
    /// replaces it. What is stored is `inotify_arg_to_mask`'s share of the
    /// word: the events, `IN_ONESHOT` and `IN_EXCL_UNLINK`; the lookup flags
    /// (`IN_ONLYDIR`, `IN_DONT_FOLLOW`) and the two `IN_MASK_*` are consumed
    /// by the call and never part of a watch.
    ///
    /// Both were ignored: `IN_MASK_CREATE` (which glib's file monitor and
    /// systemd use to learn whether a path is already watched) got the old
    /// watch back as if new, and `IN_MASK_ADD` replaced the mask it was
    /// asked to extend, so the watch lost the events it had.
    pub fn add_watch(&self, path: &str, mask: u32) -> LxResult<usize> {
        let stored = mask & (IN_ALL_EVENTS | IN_ONESHOT | IN_EXCL_UNLINK);
        let mut inner = self.inner.lock();
        if let Some((&wd, &(_, old))) = inner.watches.iter().find(|(_, (p, _))| p == path) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(LxError::EEXIST);
            }
            let new = if mask & IN_MASK_ADD != 0 {
                old | stored
            } else {
                stored
            };
            inner.watches.insert(wd, (path.into(), new));
            return Ok(wd as usize);
        }
        let wd = inner.next_wd;
        inner.next_wd += 1;
        inner.watches.insert(wd, (path.into(), stored));
        Ok(wd as usize)
    }

    /// Remove a watch by descriptor. `EINVAL` if it is not a live descriptor,
    /// as Linux does.
    pub fn rm_watch(&self, wd: i32) -> LxResult<usize> {
        let mut inner = self.inner.lock();
        if inner.watches.remove(&wd).is_some() {
            Ok(0)
        } else {
            Err(LxError::EINVAL)
        }
    }
}

#[async_trait]
impl FileLike for Inotify {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.flags.lock().take_settable(f);
        Ok(())
    }

    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        // No events are ever generated. A non-blocking reader gets EAGAIN
        // (the normal "nothing pending" answer); a blocking reader parks on
        // the eventbus that never fires — exactly how a real inotify fd with
        // no pending events behaves.
        if self.flags().non_block() {
            return Err(LxError::EAGAIN);
        }
        let bus = self.eventbus.clone();
        crate::sync::wait_for_event(bus, Event::READABLE).await?;
        Err(LxError::EAGAIN)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        // inotify fds are read-only.
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        self.read(buf).await
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        // Never readable (no events), never writable (read-only), no error.
        Ok(PollStatus {
            read: false,
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        // Park until an event that never comes; epoll/poll treat this fd as
        // simply not ready. Returning the synchronous status immediately would
        // spin a caller that only cares about POLLIN, so wait on the (silent)
        // eventbus like the blocking read does.
        let bus = self.eventbus.clone();
        crate::sync::wait_for_event(bus, Event::READABLE).await?;
        self.poll(_events)
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for the inotify stub.
    //!
    //! No events are ever delivered here, so what these pin down is the watch
    //! bookkeeping and the "quiescent fd" contract: a client that adds a watch
    //! and then polls must get a clean never-ready fd rather than an error or
    //! a busy loop. labwc and GTK both do exactly that at startup, and getting
    //! it wrong took the compositor down before it came up.

    use super::*;
    use async_std::task::block_on;

    fn inotify(flags: OpenFlags) -> Arc<Inotify> {
        Inotify::new(flags)
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on an inotify fd made without
    /// `IN_NONBLOCK` used to change nothing, so a read parked on the bus that
    /// never fires instead of answering EAGAIN. If this test hangs, that is
    /// the bug back.
    #[test]
    fn set_flags_turns_a_blocking_inotify_non_blocking() {
        let i = inotify(OpenFlags::empty());
        assert!(!i.flags().non_block());
        i.set_flags(OpenFlags::NON_BLOCK).unwrap();
        assert!(i.flags().non_block());
        let mut buf = [0u8; 64];
        assert_eq!(block_on(i.read(&mut buf)), Err(LxError::EAGAIN));
    }

    fn watched(i: &Inotify) -> alloc::vec::Vec<(i32, String, u32)> {
        i.inner
            .lock()
            .watches
            .iter()
            .map(|(wd, (p, m))| (*wd, p.clone(), *m))
            .collect()
    }

    const IN_MODIFY: u32 = 0x0000_0002;
    const IN_CREATE: u32 = 0x0000_0100;

    #[test]
    fn watch_descriptors_start_at_one_and_never_repeat() {
        let i = inotify(OpenFlags::empty());
        // Linux watch descriptors are small positive ints; zero is a valid fd
        // number and several clients treat a wd of 0 as "no watch", so the
        // first one handed out must be 1.
        assert_eq!(i.add_watch("/etc", IN_MODIFY).unwrap(), 1);
        assert_eq!(i.add_watch("/home", IN_MODIFY).unwrap(), 2);
        assert_eq!(i.add_watch("/tmp", IN_MODIFY).unwrap(), 3);
        // Removing one does not put its number back in circulation: a client
        // holding the old wd would otherwise start seeing another path's
        // events.
        i.rm_watch(2).unwrap();
        assert_eq!(i.add_watch("/var", IN_MODIFY).unwrap(), 4);
    }

    #[test]
    fn re_watching_a_path_keeps_its_descriptor_and_takes_the_new_mask() {
        let i = inotify(OpenFlags::empty());
        let wd = i.add_watch("/etc/labwc", IN_MODIFY).unwrap();
        // `inotify_add_watch` on a path already watched updates the existing
        // watch rather than making a second one; a client that re-registers
        // on every config reload would otherwise leak a descriptor each time.
        assert_eq!(i.add_watch("/etc/labwc", IN_CREATE).unwrap(), wd);
        assert_eq!(watched(&i).len(), 1);
        // The mask is replaced, not merged, unless IN_MASK_ADD asks.
        assert_eq!(watched(&i)[0].2, IN_CREATE);
    }

    /// `inotify_update_existing_watch`: `IN_MASK_CREATE` on a watched path
    /// is `EEXIST`, `IN_MASK_ADD` merges, and the lookup flags are never
    /// stored in the mask.
    #[test]
    fn mask_create_refuses_a_watched_path_and_mask_add_merges() {
        let i = inotify(OpenFlags::empty());
        let wd = i.add_watch("/etc/labwc", IN_MODIFY | IN_ONLYDIR).unwrap();
        assert_eq!(watched(&i)[0].2, IN_MODIFY, "IN_ONLYDIR is a lookup flag");
        assert_eq!(
            i.add_watch("/etc/labwc", IN_CREATE | IN_MASK_CREATE),
            Err(LxError::EEXIST)
        );
        assert_eq!(
            watched(&i)[0].2,
            IN_MODIFY,
            "and the refusal changed nothing"
        );
        assert_eq!(
            i.add_watch("/etc/labwc", IN_CREATE | IN_MASK_ADD).unwrap(),
            wd
        );
        assert_eq!(watched(&i)[0].2, IN_MODIFY | IN_CREATE);
        // A new path with IN_MASK_CREATE is simply created.
        assert_eq!(
            i.add_watch("/etc/foot", IN_MODIFY | IN_MASK_CREATE | IN_ONESHOT)
                .unwrap(),
            wd + 1
        );
        assert_eq!(watched(&i)[1].2, IN_MODIFY | IN_ONESHOT);
    }

    /// The mask checks `inotify_add_watch` makes before it looks anything
    /// up, and what it decides about the lookup.
    #[test]
    fn a_mask_needs_a_known_bit_and_may_not_both_add_and_create() {
        assert_eq!(inotify_watch_lookup(0), Err(LxError::EINVAL));
        assert_eq!(inotify_watch_lookup(0x0000_1000), Err(LxError::EINVAL));
        assert_eq!(inotify_watch_lookup(0x0080_0000), Err(LxError::EINVAL));
        assert_eq!(
            inotify_watch_lookup(IN_MODIFY | IN_MASK_ADD | IN_MASK_CREATE),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            inotify_watch_lookup(IN_MODIFY),
            Ok(WatchLookup {
                follow: true,
                only_dir: false
            })
        );
        // An unknown bit beside a known one is ignored, as Linux ignores it.
        assert_eq!(
            inotify_watch_lookup(IN_MODIFY | 0x0000_1000 | IN_DONT_FOLLOW | IN_ONLYDIR),
            Ok(WatchLookup {
                follow: false,
                only_dir: true
            })
        );
        // The lookup flags alone are a mask with a known bit.
        assert!(inotify_watch_lookup(IN_ONLYDIR).is_ok());
        assert!(inotify_watch_lookup(IN_MASK_CREATE).is_ok());
        assert_eq!(ALL_INOTIFY_BITS, 0xf700_efff);
    }

    #[test]
    fn distinct_paths_keep_distinct_watches() {
        let i = inotify(OpenFlags::empty());
        let a = i.add_watch("/etc/labwc", IN_MODIFY).unwrap();
        let b = i.add_watch("/etc/labwc/rc.xml", IN_MODIFY).unwrap();
        assert_ne!(a, b, "a path that merely shares a prefix is a new watch");
        assert_eq!(watched(&i).len(), 2);
    }

    #[test]
    fn removing_a_watch_twice_is_einval_and_so_is_one_never_handed_out() {
        let i = inotify(OpenFlags::empty());
        let wd = i.add_watch("/etc", IN_MODIFY).unwrap() as i32;
        assert_eq!(i.rm_watch(wd), Ok(0));
        assert!(watched(&i).is_empty());
        assert_eq!(i.rm_watch(wd), Err(LxError::EINVAL));
        assert_eq!(i.rm_watch(0), Err(LxError::EINVAL));
        assert_eq!(i.rm_watch(-1), Err(LxError::EINVAL));
        assert_eq!(i.rm_watch(99), Err(LxError::EINVAL));
    }

    #[test]
    fn a_quiescent_instance_polls_clean_and_never_goes_ready() {
        let i = inotify(OpenFlags::NON_BLOCK);
        i.add_watch("/etc/labwc", IN_MODIFY).unwrap();
        let s = i.poll(PollEvents::IN | PollEvents::OUT).unwrap();
        // Not readable (nothing happens), not writable (read-only), and above
        // all no error and no hangup: an event loop that saw either would tear
        // the watcher down and, in labwc's case, give up on its config.
        assert!(!s.read && !s.write && !s.error && !s.hangup);
        // A non-blocking read is the ordinary "nothing pending" answer rather
        // than an error the client has to understand.
        let mut buf = [0u8; 256];
        assert_eq!(block_on(i.read(&mut buf)), Err(LxError::EAGAIN));
    }

    #[test]
    fn an_inotify_fd_is_read_only() {
        let i = inotify(OpenFlags::NON_BLOCK);
        assert_eq!(i.write(b"anything"), Err(LxError::EINVAL));
    }

    #[test]
    fn a_dup_shares_the_watch_table() {
        let i = inotify(OpenFlags::NON_BLOCK);
        let d = i.clone();
        let wd = i.add_watch("/etc", IN_MODIFY).unwrap() as i32;
        // Same instance underneath, so a watch added through one fd is
        // removable through the other.
        assert_eq!(watched(&d).len(), 1);
        assert_eq!(d.rm_watch(wd), Ok(0));
        assert!(watched(&i).is_empty());
        assert_eq!(d.flags(), i.flags());
    }
}
