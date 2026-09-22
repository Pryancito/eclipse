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
    flags: OpenFlags,
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
            flags,
        })
    }

    /// Add or update a watch. Returns the watch descriptor. Re-watching an
    /// existing path returns its existing wd with the mask merged/replaced,
    /// matching `inotify_add_watch(2)`.
    pub fn add_watch(&self, path: &str, mask: u32) -> LxResult<usize> {
        let mut inner = self.inner.lock();
        if let Some((&wd, _)) = inner.watches.iter().find(|(_, (p, _))| p == path) {
            inner.watches.insert(wd, (path.into(), mask));
            return Ok(wd as usize);
        }
        let wd = inner.next_wd;
        inner.next_wd += 1;
        inner.watches.insert(wd, (path.into(), mask));
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
        self.flags
    }

    fn set_flags(&self, _f: OpenFlags) -> LxResult {
        Ok(())
    }

    fn dup(&self) -> Arc<dyn FileLike> {
        Arc::new(Self {
            base: KObjectBase::new(),
            inner: self.inner.clone(),
            eventbus: self.eventbus.clone(),
            flags: self.flags,
        })
    }

    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        // No events are ever generated. A non-blocking reader gets EAGAIN
        // (the normal "nothing pending" answer); a blocking reader parks on
        // the eventbus that never fires — exactly how a real inotify fd with
        // no pending events behaves.
        if self.flags.contains(OpenFlags::NON_BLOCK) {
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
        // The mask is replaced, not merged. (IN_MASK_ADD, which asks for a
        // merge, is not honoured — inert while no events are delivered.)
        assert_eq!(watched(&i)[0].2, IN_CREATE);
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
        let d = i.dup();
        let d = d.downcast_arc::<Inotify>().ok().unwrap();
        let wd = i.add_watch("/etc", IN_MODIFY).unwrap() as i32;
        // Same instance underneath, so a watch added through one fd is
        // removable through the other.
        assert_eq!(watched(&d).len(), 1);
        assert_eq!(d.rm_watch(wd), Ok(0));
        assert!(watched(&i).is_empty());
        assert_eq!(d.flags(), i.flags());
    }
}
