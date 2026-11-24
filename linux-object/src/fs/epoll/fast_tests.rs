//! The quick first look of `epoll_wait` (see `fast`): what it may pass, and
//! that it is never the only look a wait takes before it sleeps.

use super::*;
use crate::sync::{Event, EventBus};
use core::future::Future;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

const IN: u32 = PollEvents::IN.bits() as u32;
const ADD: i32 = 1;

struct Nop;

impl alloc::task::Wake for Nop {
    fn wake(self: Arc<Self>) {}
}

/// A file whose readiness the test sets, with a bus it publishes on only
/// when asked, and a count of its polls.
struct Probe {
    base: KObjectBase,
    bus: Arc<lock::Mutex<EventBus>>,
    ready: AtomicBool,
    polls: AtomicUsize,
}
impl_kobject!(Probe);

#[async_trait]
impl FileLike for Probe {
    fn flags(&self) -> OpenFlags {
        OpenFlags::empty()
    }
    fn set_flags(&self, _f: OpenFlags) -> LxResult {
        Ok(())
    }
    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Ok(0)
    }
    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Ok(0)
    }
    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Ok(0)
    }
    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        Ok(PollStatus {
            read: self.ready.load(Ordering::Relaxed),
            write: false,
            error: false,
            hangup: false,
        })
    }
    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        self.poll(events)
    }
    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = super::super::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(&self.bus, mask, waker))
    }
}

fn probe() -> Arc<Probe> {
    Arc::new(Probe {
        base: KObjectBase::new(),
        bus: Arc::new(lock::Mutex::new(EventBus::default())),
        ready: AtomicBool::new(false),
        polls: AtomicUsize::new(0),
    })
}

fn watching(file: &Arc<Probe>) -> Arc<Epoll> {
    let ep = Epoll::new(OpenFlags::empty());
    ep.ctl(
        ADD,
        FileDesc::from(5),
        EpollEvent {
            events: IN,
            data: 7,
        },
        Some(file.clone() as Arc<dyn FileLike>),
    )
    .unwrap();
    ep
}

/// One poll of `epoll_wait(ep, events, 16, timeout)`: what it returned, or
/// `None` while it waits.
fn wait_once(ep: &Epoll, timeout: isize) -> Option<Vec<(u32, u64)>> {
    let waker = Waker::from(Arc::new(Nop));
    let mut cx = Context::from_waker(&waker);
    let mut fut = alloc::boxed::Box::pin(ep.wait(16, timeout));
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(r) => Some(r.unwrap().iter().map(|e| (e.events, e.data)).collect()),
        Poll::Pending => None,
    }
}

/// The next wait's first look is a quick one, however slow the test runs.
fn next_look_quick(ep: &Epoll) {
    ep.last_full_look.store(u64::MAX, Ordering::Relaxed);
}

#[test]
fn a_quiet_entry_is_passed_until_its_file_publishes() {
    let file = probe();
    let ep = watching(&file);
    assert_eq!(
        wait_once(&ep, 0),
        Some(alloc::vec![]),
        "a full look, which arms it"
    );
    assert_eq!(file.polls.load(Ordering::Relaxed), 1);
    next_look_quick(&ep);
    assert_eq!(wait_once(&ep, 0), Some(alloc::vec![]));
    assert_eq!(
        file.polls.load(Ordering::Relaxed),
        1,
        "the quick look polled a quiet entry"
    );
    file.ready.store(true, Ordering::Relaxed);
    file.bus.lock().set(Event::READABLE);
    assert_eq!(
        wait_once(&ep, 0),
        Some(alloc::vec![(IN, 7)]),
        "what it published"
    );
    assert_eq!(
        wait_once(&ep, 0),
        Some(alloc::vec![(IN, 7)]),
        "and while it stays ready"
    );
}

/// A file that became ready without saying so is passed by the quick look,
/// and found by the full one a wait takes before it would sleep.
#[test]
fn a_wait_looks_at_every_entry_before_it_sleeps() {
    let file = probe();
    let ep = watching(&file);
    assert_eq!(wait_once(&ep, 0), Some(alloc::vec![]));
    file.ready.store(true, Ordering::Relaxed);
    next_look_quick(&ep);
    assert_eq!(
        wait_once(&ep, 0),
        Some(alloc::vec![]),
        "nothing published, nothing seen"
    );
    next_look_quick(&ep);
    assert_eq!(wait_once(&ep, -1), Some(alloc::vec![(IN, 7)]));
}

/// A changed mask is a new entry: it is polled, not passed for what the old
/// one was told.
#[test]
fn a_modified_entry_is_looked_at_again() {
    let file = probe();
    let ep = watching(&file);
    assert_eq!(wait_once(&ep, 0), Some(alloc::vec![]));
    file.ready.store(true, Ordering::Relaxed);
    ep.ctl(
        3, // EPOLL_CTL_MOD
        FileDesc::from(5),
        EpollEvent {
            events: IN,
            data: 8,
        },
        Some(file.clone() as Arc<dyn FileLike>),
    )
    .unwrap();
    next_look_quick(&ep);
    assert_eq!(wait_once(&ep, 0), Some(alloc::vec![(IN, 8)]));
}

#[test]
fn an_empty_pipe_stays_quiet_after_a_full_look() {
    use crate::fs::pipe::Pipe;
    use crate::fs::File;
    let (r, _w) = Pipe::create_pair();
    let read_end: Arc<dyn FileLike> = File::new(
        Arc::new(r),
        OpenFlags::RDONLY,
        alloc::string::String::from("pipe"),
    );
    let ep = Epoll::new(OpenFlags::empty());
    ep.ctl(
        ADD,
        FileDesc::from(5),
        EpollEvent {
            events: IN,
            data: 7,
        },
        Some(read_end),
    )
    .unwrap();
    assert_eq!(wait_once(&ep, 0), Some(alloc::vec![]));
    let inner = ep.inner.lock();
    let armed = inner.armed.get(&FileDesc::from(5)).expect("armed");
    assert!(!armed.fired(), "fired at once");
    assert!(armed.quiet());
}
