use super::*;
use crate::sync::{Event, EventBus};
use alloc::sync::Arc;
use core::convert::TryInto;
use core::sync::atomic::{AtomicU64, Ordering};
use lock::Mutex;
use zircon_object::object::*;

/// `EFD_SEMAPHORE`. It is bit 0 of the `eventfd2(2)` flags word, which
/// `sys_eventfd2` funnels through `OpenFlags::from_bits_truncate` — where bit 0
/// spells `WRONLY`. The two meanings coexist only because nothing checks
/// `flags().readable()` on an anonymous fd; read the bit by this name so the
/// overlap is visible to whoever changes either side.
const EFD_SEMAPHORE: usize = 1;

/// eventfd implementation
pub struct EventFd {
    base: KObjectBase,
    counter: Arc<AtomicU64>,
    eventbus: Arc<Mutex<EventBus>>,
    /// Behind a lock so `fcntl(F_SETFL)` can change it after creation.
    flags: Mutex<OpenFlags>,
}

impl_kobject!(EventFd);

impl EventFd {
    /// create an eventfd
    pub fn new(initval: u32, flags: OpenFlags) -> Arc<Self> {
        let fd = EventFd {
            base: KObjectBase::new(),
            counter: Arc::new(AtomicU64::new(initval as u64)),
            eventbus: EventBus::new(),
            flags: Mutex::new(flags),
        };
        // A non-zero initial value is already an event to deliver.
        fd.publish_readiness();
        Arc::new(fd)
    }

    fn semaphore(&self) -> bool {
        self.flags().bits() & EFD_SEMAPHORE != 0
    }

    /// Publish readiness from the counter, which is the only thing that makes
    /// this fd readable.
    ///
    /// The bit must never say READABLE while the counter is zero. A blocking
    /// `read` loops "check the counter, then `wait_for_event(READABLE)`", and
    /// `wait_for_event` returns immediately when the bit is already set — so a
    /// set bit over a zero counter is not a spurious wakeup, it is a spin at
    /// 100 % CPU that never makes progress. `write` used to set the bit for
    /// every accepted write, and `write(0)` is legal and adds nothing, which
    /// reached that state on purpose; a reader that drained between a writer's
    /// compare-exchange and its `set` reached it by accident.
    ///
    /// Taking the bus lock *before* reading the counter is what makes this
    /// safe against a concurrent reader or writer: every change to the counter
    /// is followed by this call, so whichever of them publishes last read the
    /// counter after the other's change.
    fn publish_readiness(&self) {
        let mut bus = self.eventbus.lock();
        if self.counter.load(Ordering::SeqCst) > 0 {
            bus.set(Event::READABLE);
        } else {
            bus.clear(Event::READABLE);
        }
    }
}

#[async_trait]
impl FileLike for EventFd {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.flags.lock().take_settable(f);
        Ok(())
    }

    fn dup(&self) -> Arc<dyn FileLike> {
        Arc::new(Self {
            base: KObjectBase::new(),
            counter: self.counter.clone(),
            eventbus: self.eventbus.clone(),
            flags: Mutex::new(self.flags()),
        })
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        if buf.len() < 8 {
            return Err(LxError::EINVAL);
        }
        loop {
            let counter = self.counter.load(Ordering::SeqCst);
            if counter > 0 {
                let res = if self.semaphore() {
                    if self
                        .counter
                        .compare_exchange(counter, counter - 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        1
                    } else {
                        continue;
                    }
                } else {
                    if self
                        .counter
                        .compare_exchange(counter, 0, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        counter
                    } else {
                        continue;
                    }
                };
                buf[..8].copy_from_slice(&res.to_ne_bytes());
                self.publish_readiness();
                return Ok(8);
            }
            if self.flags().non_block() {
                return Err(LxError::EAGAIN);
            }
            self.async_poll(PollEvents::IN).await?;
        }
    }

    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        if buf.len() < 8 {
            return Err(LxError::EINVAL);
        }
        let val = u64::from_ne_bytes(buf[..8].try_into().unwrap());
        if val == u64::MAX {
            return Err(LxError::EINVAL);
        }
        loop {
            let counter = self.counter.load(Ordering::SeqCst);
            if u64::MAX - counter > val {
                if self
                    .counter
                    .compare_exchange(counter, counter + val, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    self.publish_readiness();
                    return Ok(8);
                }
            } else {
                if self.flags().non_block() {
                    return Err(LxError::EAGAIN);
                }
                // TODO: wait for writeable? EventFd is almost always writeable unless overflow
                return Err(LxError::EAGAIN);
            }
        }
    }

    async fn read_at(&self, _offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        self.read(buf).await
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let counter = self.counter.load(Ordering::SeqCst);
        Ok(PollStatus {
            read: counter > 0,
            write: counter < u64::MAX - 1,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        loop {
            let status = self.poll(_events)?;
            let want_read = _events.contains(PollEvents::IN);
            let want_write = _events.contains(PollEvents::OUT);
            let ready = (want_read && status.read)
                || (want_write && status.write)
                || (!want_read && !want_write);
            if ready {
                return Ok(status);
            }
            let bus = self.eventbus.clone();
            crate::sync::wait_for_event(bus, Event::READABLE | Event::WRITABLE).await?;
        }
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = super::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(
            &self.eventbus,
            mask,
            waker,
        ))
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for `eventfd(2)`.
    //!
    //! An eventfd is the wakeup primitive under every event loop in userspace
    //! (glib, libuv, libwayland) — it is how one thread tells another's
    //! `poll()` to return. The failures that matter are not "the counter is
    //! wrong": they are the readiness bit disagreeing with the counter, which
    //! is either a wakeup that never comes or a reader spinning at 100 % CPU.
    //!
    //! `async_std` drives the futures; `block_on` is safe here only because
    //! every blocking path in these tests has something that completes it.

    use super::*;
    use async_std::task::block_on;
    use core::sync::atomic::AtomicBool;
    use core::task::{RawWaker, RawWakerVTable, Waker};

    fn efd(initval: u32, flags: OpenFlags) -> Arc<EventFd> {
        EventFd::new(initval, flags)
    }

    fn nonblock() -> OpenFlags {
        OpenFlags::NON_BLOCK
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on an eventfd made without `EFD_NONBLOCK`
    /// used to change nothing (`set_flags` ignored its argument), so the
    /// caller's next read with a zero counter parked for good instead of
    /// answering EAGAIN. If this test hangs, that is the bug back.
    #[test]
    fn set_flags_turns_a_blocking_eventfd_non_blocking() {
        let fd = efd(0, OpenFlags::empty());
        assert!(!fd.flags().non_block());
        fd.set_flags(nonblock()).unwrap();
        assert!(fd.flags().non_block());
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
        // And back: the bit is settable both ways, not sticky.
        fd.set_flags(OpenFlags::empty()).unwrap();
        assert!(!fd.flags().non_block());
    }

    /// `dup2` clears `O_CLOEXEC` on the copy it installs, through
    /// `set_flags`; with that call ignored, a dup of an `EFD_CLOEXEC` eventfd
    /// was registered close-on-exec and vanished across the child's exec.
    /// The two objects keep their own flags: clearing the copy's must not
    /// touch the original's.
    #[test]
    fn a_dup_has_its_own_flags_and_can_drop_cloexec() {
        let fd = efd(0, nonblock() | OpenFlags::CLOEXEC);
        let copy = fd.dup();
        assert!(copy.flags().close_on_exec(), "a dup starts as a copy");
        copy.set_flags(copy.flags() - OpenFlags::CLOEXEC).unwrap();
        assert!(!copy.flags().close_on_exec());
        assert!(copy.flags().non_block(), "only the named bit changed");
        assert!(fd.flags().close_on_exec(), "the original is untouched");
    }

    fn read8(fd: &EventFd) -> LxResult<u64> {
        let mut buf = [0u8; 8];
        let n = block_on(fd.read(&mut buf))?;
        assert_eq!(n, 8);
        Ok(u64::from_ne_bytes(buf))
    }

    fn write8(fd: &EventFd, v: u64) -> LxResult<usize> {
        fd.write(&v.to_ne_bytes())
    }

    /// A waker that records whether it was woken, plus a reader for the flag.
    ///
    /// Tests here drive futures by hand instead of handing them to an
    /// executor: what has to be proved is that a future parks and that a
    /// specific call wakes it, and a real executor hides both behind a
    /// scheduler that may or may not have a thread free.
    fn flag_waker() -> (Waker, impl Fn() -> bool) {
        let flag = Arc::new(AtomicBool::new(false));
        let data = Arc::into_raw(flag.clone()) as *const ();

        unsafe fn clone(p: *const ()) -> RawWaker {
            Arc::increment_strong_count(p as *const AtomicBool);
            RawWaker::new(p, &VTABLE)
        }
        unsafe fn wake(p: *const ()) {
            let arc = Arc::from_raw(p as *const AtomicBool);
            arc.store(true, Ordering::SeqCst);
        }
        unsafe fn wake_by_ref(p: *const ()) {
            (*(p as *const AtomicBool)).store(true, Ordering::SeqCst);
        }
        unsafe fn drop_it(p: *const ()) {
            drop(Arc::from_raw(p as *const AtomicBool));
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_it);

        let waker = unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) };
        (waker, move || flag.load(Ordering::SeqCst))
    }

    /// The readiness bit as a parked poller would observe it, which is not the
    /// same thing as `poll()` recomputing it from the counter.
    fn bit_says_readable(fd: &EventFd) -> bool {
        fd.eventbus.lock().events().contains(Event::READABLE)
    }

    #[test]
    fn writes_accumulate_and_one_read_drains_the_lot() {
        let fd = efd(0, nonblock());
        assert_eq!(write8(&fd, 3).unwrap(), 8);
        assert_eq!(write8(&fd, 4).unwrap(), 8);
        // Not a queue of messages: a counter, drained in one go.
        assert_eq!(read8(&fd).unwrap(), 7);
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
    }

    #[test]
    fn semaphore_mode_hands_the_count_out_one_at_a_time() {
        let fd = efd(0, nonblock() | OpenFlags::from_bits_truncate(EFD_SEMAPHORE));
        write8(&fd, 3).unwrap();
        assert_eq!(read8(&fd).unwrap(), 1);
        assert_eq!(read8(&fd).unwrap(), 1);
        assert!(fd.poll(PollEvents::IN).unwrap().read, "one left");
        assert_eq!(read8(&fd).unwrap(), 1);
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
        assert!(!bit_says_readable(&fd));
    }

    #[test]
    fn writing_zero_is_accepted_and_leaves_the_fd_unreadable() {
        // `write(0)` is legal and adds nothing. Setting READABLE for it anyway
        // is not a harmless spurious wakeup: a blocking `read` re-checks the
        // counter, finds nothing, and waits on a bit that is already set, so
        // `wait_for_event` returns at once and the loop spins forever without
        // ever yielding. This assertion is the one that catches it — `poll()`
        // recomputes from the counter and looks fine either way.
        let fd = efd(0, nonblock());
        assert_eq!(write8(&fd, 0).unwrap(), 8);
        assert_eq!(fd.counter.load(Ordering::SeqCst), 0);
        assert!(!fd.poll(PollEvents::IN).unwrap().read);
        assert!(
            !bit_says_readable(&fd),
            "a zero counter must never be advertised as readable"
        );
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
    }

    #[test]
    fn the_readable_bit_tracks_the_counter_through_a_whole_cycle() {
        let fd = efd(0, nonblock());
        assert!(!bit_says_readable(&fd));
        write8(&fd, 1).unwrap();
        assert!(bit_says_readable(&fd));
        // Writing zero on top of a pending count must not clear it either.
        write8(&fd, 0).unwrap();
        assert!(bit_says_readable(&fd));
        read8(&fd).unwrap();
        assert!(!bit_says_readable(&fd));
    }

    #[test]
    fn an_initial_value_is_an_event_that_is_already_pending() {
        let fd = efd(5, nonblock());
        assert!(fd.poll(PollEvents::IN).unwrap().read);
        assert!(
            bit_says_readable(&fd),
            "a poller parked before the first write would never be woken"
        );
        assert_eq!(read8(&fd).unwrap(), 5);
    }

    #[test]
    fn a_read_or_write_shorter_than_the_counter_is_einval() {
        let fd = efd(1, nonblock());
        let mut small = [0u8; 7];
        assert_eq!(block_on(fd.read(&mut small)), Err(LxError::EINVAL));
        assert_eq!(fd.write(&[0u8; 7]), Err(LxError::EINVAL));
        // The failed read consumed nothing.
        assert_eq!(read8(&fd).unwrap(), 1);
    }

    #[test]
    fn the_counter_stops_one_short_of_u64_max() {
        let fd = efd(0, nonblock());
        // u64::MAX is the reserved "would overflow" value and is never a
        // legal write, whatever the counter holds.
        assert_eq!(fd.write(&u64::MAX.to_ne_bytes()), Err(LxError::EINVAL));
        // The ceiling is u64::MAX - 1, so this is the largest single write.
        assert_eq!(write8(&fd, u64::MAX - 1).unwrap(), 8);
        // One more would cross it: refused rather than wrapped to nothing.
        assert_eq!(write8(&fd, 1), Err(LxError::EAGAIN));
        assert_eq!(fd.counter.load(Ordering::SeqCst), u64::MAX - 1);
        // Adding zero at the ceiling is still fine.
        assert_eq!(write8(&fd, 0).unwrap(), 8);
        assert_eq!(read8(&fd).unwrap(), u64::MAX - 1);
    }

    #[test]
    fn poll_says_writable_until_the_counter_is_full() {
        let fd = efd(0, nonblock());
        let s = fd.poll(PollEvents::IN | PollEvents::OUT).unwrap();
        assert!(!s.read && s.write && !s.error && !s.hangup);
        write8(&fd, u64::MAX - 1).unwrap();
        let s = fd.poll(PollEvents::IN | PollEvents::OUT).unwrap();
        assert!(s.read, "the count is pending");
        assert!(
            !s.write,
            "a writer would block, so it must not be advertised"
        );
    }

    #[test]
    fn a_dup_shares_the_counter_and_the_readiness() {
        let fd = efd(0, nonblock());
        let dup = fd.dup();
        let dup = dup.downcast_arc::<EventFd>().ok().unwrap();
        write8(&fd, 2).unwrap();
        // Same object underneath: the dup sees the count and draining through
        // it clears the original's readiness too.
        assert!(dup.poll(PollEvents::IN).unwrap().read);
        assert!(bit_says_readable(&dup));
        assert_eq!(read8(&dup).unwrap(), 2);
        assert!(!bit_says_readable(&fd));
        assert!(!fd.poll(PollEvents::IN).unwrap().read);
    }

    #[test]
    fn the_semaphore_flag_is_the_low_bit_of_the_open_flags() {
        // `sys_eventfd2` runs the eventfd2 flags word through
        // `OpenFlags::from_bits_truncate`, where EFD_SEMAPHORE (1) lands on
        // WRONLY and EFD_NONBLOCK (0o4000) on NON_BLOCK. That overlap is load
        // bearing; pin it here so a renumbering of `OpenFlags` cannot quietly
        // turn semaphore mode on or off.
        assert_eq!(EFD_SEMAPHORE, OpenFlags::WRONLY.bits());
        assert_eq!(0o4000, OpenFlags::NON_BLOCK.bits());
        assert!(efd(0, OpenFlags::from_bits_truncate(0o4000 | 1)).semaphore());
        assert!(!efd(0, OpenFlags::from_bits_truncate(0o4000)).semaphore());
    }

    #[test]
    fn a_blocking_read_parks_and_is_woken_by_the_writer() {
        use core::task::{Context, Poll};

        // Driven by hand rather than by an executor: this has to prove the
        // read PARKS (one poll, still pending, no wakeup) and that the write
        // is what releases it. A spawned writer and a sleep would prove
        // neither, and would depend on a worker thread being free.
        let fd = efd(0, OpenFlags::empty());
        let (waker, woke) = flag_waker();
        let mut cx = Context::from_waker(&waker);
        let mut buf = [0u8; 8];
        {
            let mut fut = fd.read(&mut buf);
            assert!(
                fut.as_mut().poll(&mut cx).is_pending(),
                "a blocking read with nothing pending must park"
            );
            assert!(!woke(), "nothing has happened yet");

            assert_eq!(fd.write(&9u64.to_ne_bytes()).unwrap(), 8);
            assert!(woke(), "the parked reader was never woken by the write");

            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(r) => assert_eq!(r.unwrap(), 8),
                Poll::Pending => panic!("the count was there and the read parked again"),
            }
        }
        assert_eq!(u64::from_ne_bytes(buf), 9);
        assert!(!bit_says_readable(&fd));
    }

    #[test]
    fn async_poll_returns_at_once_when_the_count_is_already_there() {
        let fd = efd(1, OpenFlags::empty());
        let s = block_on(fd.async_poll(PollEvents::IN)).unwrap();
        assert!(s.read);
        // Asking only about writability is answered from the counter too,
        // without waiting for a reader to make room.
        let s = block_on(fd.async_poll(PollEvents::OUT)).unwrap();
        assert!(s.write);
    }

    #[test]
    fn a_subscriber_is_woken_when_the_count_arrives() {
        let fd = efd(0, nonblock());
        let (waker, woke) = flag_waker();
        let sub = fd.subscribe_readiness(PollEvents::IN, &waker);
        assert!(sub.is_some(), "epoll relies on this being wired up");
        assert!(!woke());

        write8(&fd, 1).unwrap();
        assert!(woke(), "a parked epoll waiter was never told");
    }
}
