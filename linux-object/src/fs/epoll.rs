use super::*;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use lock::Mutex;
use zircon_object::object::*;

/// Max nesting depth for one epoll watching another (mirrors Linux's
/// `EP_MAX_NESTS`). Also bounds the cycle-detection walk in
/// `Epoll::contains_epoll` itself, so a bug elsewhere that let a cycle slip
/// past this check couldn't make the check's own recursion unbounded either.
const EPOLL_MAX_NEST_DEPTH: usize = 4;

/// `epoll_ctl(2)` operations.
const EPOLL_CTL_ADD: i32 = 1;
/// See [`EPOLL_CTL_ADD`].
const EPOLL_CTL_DEL: i32 = 2;
/// See [`EPOLL_CTL_ADD`].
const EPOLL_CTL_MOD: i32 = 3;

/// The four `epoll_ctl(2)` event bits that live above the 16 bits a
/// [`PollEvents`] can hold.
///
/// `EpollEvent::events` is a `u32` because Linux's `struct epoll_event` is,
/// but every reader in this file used to narrow it with `event.events as u16`
/// before handing it to `PollEvents::from_bits_truncate`. That cast dropped
/// all four on the floor, and the one that matters is `EPOLLONESHOT`: a
/// thread pool arms an fd with it precisely so that **one** worker is handed
/// the event and the rest are not, and re-arms with `EPOLL_CTL_MOD` when it
/// is done. Losing the bit turned that guarantee into its opposite -- the fd
/// stayed armed and every waiter got it, which is the race the flag exists to
/// prevent.
const EPOLLEXCLUSIVE: u32 = 1 << 28;
/// See [`EPOLLEXCLUSIVE`]. Keeps the system awake while the event is pending;
/// this kernel has no suspend, so it is accepted and does nothing.
const EPOLLWAKEUP: u32 = 1 << 29;
/// See [`EPOLLEXCLUSIVE`]. Report the fd once, then disable the entry until
/// an `EPOLL_CTL_MOD` re-arms it.
const EPOLLONESHOT: u32 = 1 << 30;
/// See [`EPOLLEXCLUSIVE`]. Edge-triggered: see [`edge_step`]. Honoured for
/// the files that count their readiness publications
/// (`FileLike::readiness_seq`: eventfd, unix sockets, pipes); every other file
/// is still reported by level, which costs wakeups but never loses one.
const EPOLLET: u32 = 1 << 31;

/// How long an edge-triggered entry stays quiet about a level that has not
/// moved, before it is reported again anyway.
///
/// Linux would stay quiet for ever. This is the insurance for an edge this
/// kernel does not see: a file whose level can rise without its producer
/// publishing anything. A program waiting on such an edge is late by this
/// much instead of asleep for good; one that is not waiting gets a spurious
/// event, which edge-triggered programs are written to absorb.
const EDGE_REARM: core::time::Duration = core::time::Duration::from_millis(250);

/// Where an edge-triggered entry stands: the file's publication counter when
/// the entry was last scanned, the bits it has already reported and has not
/// seen fall since, and when it last reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EdgeState {
    seq: u64,
    quiet: u32,
    since: core::time::Duration,
}

/// One scan of an edge-triggered entry: whether it reports `ready` now, and
/// its state afterwards.
///
/// An edge is a publication (`seq` moved: data arrived, room freed, a write to
/// an eventfd that was readable already) or a bit that was low the last time
/// and is high now. Without one, a bit that was already reported stays quiet
/// however long it stays high -- which is the whole point. `mio`, under every
/// `tokio` program, never reads its eventfd waker: it registers it with
/// `EPOLLET` and lets each `write` be the edge. Reported by level, an eventfd
/// woken once is readable for ever and its event loop spins on `epoll_wait`.
/// Firefox's parent process did 31 million of them in five minutes (01-oct).
///
/// When it reports, it reports every ready bit, as `ep_item_poll` does.
fn edge_step(
    prev: Option<EdgeState>,
    seq: u64,
    ready: u32,
    now: core::time::Duration,
) -> (bool, EdgeState) {
    let quiet = match prev {
        Some(p) if p.seq == seq && now.saturating_sub(p.since) < EDGE_REARM => p.quiet & ready,
        _ => 0,
    };
    if ready & !quiet != 0 {
        let next = EdgeState {
            seq,
            quiet: ready,
            since: now,
        };
        (true, next)
    } else {
        let since = prev.map_or(now, |p| p.since);
        (false, EdgeState { seq, quiet, since })
    }
}

/// The bits `EPOLLEXCLUSIVE` may be combined with (Linux's
/// `EPOLLEXCLUSIVE_OK_BITS`): the two readiness bits, the two that are always
/// reported, and the three flags that do not change who gets woken.
const EPOLLEXCLUSIVE_OK_BITS: u32 = PollEvents::IN.bits() as u32
    | PollEvents::OUT.bits() as u32
    | PollEvents::ERR.bits() as u32
    | PollEvents::HUP.bits() as u32
    | EPOLLWAKEUP
    | EPOLLET
    | EPOLLEXCLUSIVE;

/// The events one interest-list entry reports for a readiness status.
///
/// The stored mask decides all four bits, `EPOLLERR`/`EPOLLHUP` included.
/// Those two are reported whether or not the caller asked for them, which is
/// why [`epoll_ctl_events`] puts them into every mask it stores -- so that the
/// one mask that reports nothing at all is the empty one, which only an
/// `EPOLLONESHOT` delivery can produce. `ep_item_poll` is the same `&`.
fn ready_events(events: u32, status: &PollStatus) -> u32 {
    let interest = PollEvents::from_bits_truncate(events as u16);
    (PollEvents::ready(status) & interest).bits() as u32
}

/// The event mask `epoll_ctl` stores for one interest-list entry, or the
/// error Linux reports for the request.
///
/// `EPOLLEXCLUSIVE` is the only bit `do_epoll_ctl` validates, and it validates
/// it three ways: it cannot be added by `EPOLL_CTL_MOD`, it cannot be put on a
/// nested epoll, and it cannot be mixed with a bit outside
/// [`EPOLLEXCLUSIVE_OK_BITS`]. All three exist because the flag changes *who*
/// is woken, and a request whose wake-up set is ambiguous is refused rather
/// than guessed at.
///
/// `EPOLLERR` and `EPOLLHUP` are forced in, as `do_epoll_ctl` does, so that
/// the stored mask alone says what the entry reports. That is what lets an
/// `EPOLLONESHOT` entry be disabled by zeroing its mask, the way
/// `ep_send_events` does it, instead of carrying a second flag that every
/// reader would have to remember to consult.
fn epoll_ctl_events(op: i32, events: u32, target_is_epoll: bool) -> LxResult<u32> {
    if events & EPOLLEXCLUSIVE != 0 {
        if op == EPOLL_CTL_MOD {
            return Err(LxError::EINVAL);
        }
        if target_is_epoll || events & !EPOLLEXCLUSIVE_OK_BITS != 0 {
            return Err(LxError::EINVAL);
        }
    }
    Ok(events | PollEvents::ERR.bits() as u32 | PollEvents::HUP.bits() as u32)
}

/// The two refusals `do_epoll_ctl` makes of the target file before it
/// looks at the op or the mask, in its order: a file with no poll
/// operation (`!file_can_poll`, `EPERM`), then the epoll itself
/// (`f.file == tf.file`, `EINVAL`).
///
/// A regular file was accepted and reported always ready, so an event
/// loop handed one (a log written by another process, a config file)
/// spun at full speed where Linux tells it `EPERM` and it falls back to
/// inotify or a timer. And adding an epoll to itself was `ELOOP`, the
/// answer for a cycle through other epolls, not the `EINVAL` Linux gives
/// the direct case.
fn epoll_target_verdict(target_can_epoll: bool, target_is_self: bool) -> LxResult<()> {
    if !target_can_epoll {
        return Err(LxError::EPERM);
    }
    if target_is_self {
        return Err(LxError::EINVAL);
    }
    Ok(())
}

lazy_static::lazy_static! {
    /// Serializes EPOLL_CTL_ADD of a nested epoll (one epoll fd watching
    /// another) so the cycle/depth check and the insert happen as one atomic
    /// step. Without this, two concurrent adds in opposite directions
    /// (thread 1: add B to A; thread 2: add A to B) could each pass
    /// `contains_epoll` before either inserts, still creating a cycle.
    /// Mirrors Linux's global `epmutex`, used for the same reason.
    static ref EPOLL_NEST_LOCK: Mutex<()> = Mutex::new(());
}

/// epoll implementation
pub struct Epoll {
    base: KObjectBase,
    inner: Mutex<EpollInner>,
    flags: Mutex<OpenFlags>,
}

struct EpollInner {
    /// Each watched fd maps to its requested event mask plus a handle to the
    /// underlying file. The file handle lets `Epoll::poll()` report readiness
    /// of the watched fds *without* a process context — which is what makes a
    /// nested epoll (an epoll fd added to another epoll's interest list) work.
    /// wlroots/labwc relies on exactly this: it adds `libinput_get_fd()` (an
    /// epoll fd) to its `wl_event_loop` epoll, so the outer epoll must surface
    /// the inner epoll's readiness or input events are never dispatched.
    interest_list: BTreeMap<FileDesc, (EpollEvent, Arc<dyn FileLike>)>,
    /// [`EdgeState`] of the `EPOLLET` entries that have been scanned. Any
    /// `EPOLL_CTL_*` on the fd starts it over, as `ep_modify` re-reports a
    /// level that is already up.
    edge: BTreeMap<FileDesc, EdgeState>,
}

/// epoll event
#[repr(C)]
#[cfg_attr(target_arch = "x86_64", repr(packed))]
#[derive(Clone, Copy, Debug)]
pub struct EpollEvent {
    /// events
    pub events: u32,
    /// data
    pub data: u64,
}

impl_kobject!(Epoll);

impl Epoll {
    /// create an epoll instance
    pub fn new(flags: OpenFlags) -> Arc<Self> {
        Arc::new(Epoll {
            base: KObjectBase::new(),
            inner: Mutex::new(EpollInner {
                interest_list: BTreeMap::new(),
                edge: BTreeMap::new(),
            }),
            flags: Mutex::new(flags),
        })
    }

    /// add, modify, or remove a file descriptor from the interest list. `file`
    /// is the resolved handle for `fd` (required for ADD/MOD; ignored for DEL).
    pub fn ctl(
        &self,
        op: i32,
        fd: FileDesc,
        event: EpollEvent,
        file: Option<Arc<dyn FileLike>>,
    ) -> LxResult<usize> {
        // The target is judged first, for every op, as `do_epoll_ctl` does
        // it: a file that cannot be polled is EPERM and this epoll itself is
        // EINVAL, before the op, the mask or the interest list.
        if let Some(target) = file.as_ref() {
            let target_is_self = core::ptr::eq(
                Arc::as_ptr(target) as *const u8,
                self as *const Self as *const u8,
            );
            epoll_target_verdict(target.can_epoll(), target_is_self)?;
        }
        // The mask is settled before the interest list is touched, as
        // `do_epoll_ctl` does it: a request this kernel will not honour is
        // EINVAL whether or not the fd happens to be watched already, and the
        // stored mask is the one the readiness scan will be held to.
        let event = if op == EPOLL_CTL_ADD || op == EPOLL_CTL_MOD {
            let target = file.as_ref().ok_or(LxError::EBADF)?;
            let target_is_epoll = target.clone().downcast_arc::<Epoll>().is_ok();
            EpollEvent {
                events: epoll_ctl_events(op, event.events, target_is_epoll)?,
                data: event.data,
            }
        } else {
            event
        };
        let mut inner = self.inner.lock();
        inner.edge.remove(&fd);
        match op {
            EPOLL_CTL_ADD => {
                let file = file.ok_or(LxError::EBADF)?;
                // Linux keys the interest list by (file, fd), so `EEXIST` is
                // for the SAME description under the same number. An entry
                // whose description is no longer the one at `fd` is a number
                // that was closed and handed out again: the caller cannot
                // reach the old description through `fd` any more, and it
                // cannot `EPOLL_CTL_DEL` a file it has no descriptor for.
                if let Some((_, watched)) = inner.interest_list.get(&fd) {
                    if Arc::ptr_eq(watched, &file) {
                        return Err(LxError::EEXIST);
                    }
                    inner.interest_list.remove(&fd);
                }
                // If the target is itself an epoll, reject a self-add or any
                // nesting that would create a cycle or exceed
                // EPOLL_MAX_NEST_DEPTH. any_ready()'s recursion through
                // file.poll() has no other terminating condition -- a cycle
                // recurses forever, and even an acyclic chain deep enough can
                // overflow a coroutine's guard-page-less stack (the same bug
                // class root-caused once already for unbounded path
                // recursion, commit b448c77e).
                if let Ok(target) = file.clone().downcast_arc::<Epoll>() {
                    // Serialize against a concurrent nested add elsewhere so
                    // the check and the insert are atomic as a unit (see
                    // EPOLL_NEST_LOCK's doc comment) -- drop our own lock
                    // first since contains_epoll never needs to re-lock
                    // `self` (it returns as soon as it reaches `self` by
                    // pointer, before locking that node), but a *different*
                    // epoll's `ctl` could still be walking through us.
                    drop(inner);
                    let _nest_guard = EPOLL_NEST_LOCK.lock();
                    if target.contains_epoll(self, 0) {
                        return Err(LxError::ELOOP);
                    }
                    inner = self.inner.lock();
                    if let Some((_, watched)) = inner.interest_list.get(&fd) {
                        if Arc::ptr_eq(watched, &file) {
                            return Err(LxError::EEXIST);
                        }
                    }
                }
                inner.interest_list.insert(fd, (event, file));
            }
            EPOLL_CTL_DEL => {
                inner.interest_list.remove(&fd).ok_or(LxError::ENOENT)?;
            }
            EPOLL_CTL_MOD => {
                let file = file.ok_or(LxError::EBADF)?;
                let e = inner.interest_list.get_mut(&fd).ok_or(LxError::ENOENT)?;
                *e = (event, file);
            }
            _ => return Err(LxError::EINVAL),
        }
        Ok(0)
    }

    /// Disable the entries an `EPOLLONESHOT` delivery has just used up.
    ///
    /// `ep_send_events` clears the event bits of a one-shot entry as it hands
    /// the event out, leaving the entry in the interest list reporting nothing
    /// until an `EPOLL_CTL_MOD` puts a mask back. Zeroing the mask is the
    /// whole mechanism, which is why `epoll_ctl` forces `EPOLLERR`/`EPOLLHUP`
    /// in: an armed entry can never have an empty mask by accident.
    ///
    /// The scan runs on a snapshot taken without the lock, so the file handle
    /// is compared by pointer before anything is written: between the scan and
    /// here, another thread may have dropped this fd and handed the number to
    /// a different file, and that one was never delivered anything.
    fn disarm_oneshot(&self, delivered: &[(FileDesc, Arc<dyn FileLike>)]) {
        if delivered.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        for (fd, file) in delivered {
            if let Some((event, current)) = inner.interest_list.get_mut(fd) {
                if event.events & EPOLLONESHOT != 0 && Arc::ptr_eq(current, file) {
                    event.events = 0;
                }
            }
        }
    }

    /// Drop every entry that watches `file`: a descriptor of it was closed,
    /// and it was the last one in its table holding that description.
    ///
    /// This is `eventpoll_release`, which `__fput` runs when the last
    /// reference to an open file description goes away, and it is what
    /// epoll(7) promises under "Will closing a file descriptor cause it to be
    /// removed from all epoll interest lists?". Nothing here did it, so a
    /// process that closed a watched descriptor without an `EPOLL_CTL_DEL`
    /// first (most of them: the manual says close is enough) kept it in three
    /// ways at once. The interest list held the description alive, so the
    /// peer of a closed socket or pipe never saw EOF or `EPIPE` while the
    /// event loop lived. `epoll_wait` kept returning the dead entry with its
    /// old `data`, and the `read` the program then did on that number was
    /// `EBADF`, forever, at full speed. And when the number came back from
    /// `accept` or `open`, `EPOLL_CTL_ADD` on it was `EEXIST`.
    ///
    /// Matched by description, not by number: the entry is keyed by the
    /// number it was added under, and the descriptor whose close ended the
    /// description may be a `dup` under another one. Compared by pointer, as
    /// `disarm_oneshot` does, because a number that was closed and reopened
    /// may already watch its new description. Returns how many entries went.
    pub fn forget_closed(&self, file: &Arc<dyn FileLike>) -> usize {
        let mut inner = self.inner.lock();
        let before = inner.interest_list.len();
        inner
            .interest_list
            .retain(|_, (_, watched)| !Arc::ptr_eq(watched, file));
        let EpollInner {
            interest_list,
            edge,
        } = &mut *inner;
        edge.retain(|fd, _| interest_list.contains_key(fd));
        before - inner.interest_list.len()
    }

    /// Store the [`EdgeState`]s a scan computed, for the entries that still
    /// watch the file the scan looked at.
    fn store_edges(&self, scanned: &[(FileDesc, Arc<dyn FileLike>, EdgeState)]) {
        if scanned.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        for (fd, file, state) in scanned {
            let current = match inner.interest_list.get(fd) {
                Some((event, current)) => event.events & EPOLLET != 0 && Arc::ptr_eq(current, file),
                None => false,
            };
            if current {
                inner.edge.insert(*fd, *state);
            }
        }
    }

    /// Whether `needle` (some other epoll, compared by identity) is
    /// reachable by walking outward from `self` through nested epolls --
    /// i.e. whether `self` already (transitively) watches `needle`. Used by
    /// `ctl`'s ADD path to reject a nesting that would create a cycle.
    /// `depth >= EPOLL_MAX_NEST_DEPTH` conservatively answers "yes" (refuse
    /// rather than risk missing a cycle further down) rather than growing
    /// unbounded itself.
    fn contains_epoll(&self, needle: *const Epoll, depth: usize) -> bool {
        if core::ptr::eq(self, needle) {
            return true;
        }
        if depth >= EPOLL_MAX_NEST_DEPTH {
            return true;
        }
        // Snapshot then drop the lock before recursing, same reasoning as
        // any_ready(): a watched fd can itself be an epoll that re-enters.
        let entries: Vec<Arc<dyn FileLike>> = self
            .inner
            .lock()
            .interest_list
            .values()
            .map(|(_, f)| f.clone())
            .collect();
        for f in entries {
            if let Ok(child) = f.downcast_arc::<Epoll>() {
                if child.contains_epoll(needle, depth + 1) {
                    return true;
                }
            }
        }
        false
    }

    /// Returns whether any watched fd is currently ready for its requested
    /// events. Shared by `poll`/`async_poll` so a nested epoll surfaces its
    /// inner readiness to an outer epoll/poll.
    ///
    /// Nested epolls (libinput's fd inside wlroots) are walked **iteratively**
    /// with an explicit heap stack — recursive `file.poll() → any_ready()`
    /// stacked a fresh interest-list snapshot frame per nest level on the
    /// coroutine stack during labwc bring-up.
    fn any_ready(&self) -> bool {
        let mut stack: Vec<(EpollEvent, Arc<dyn FileLike>, usize)> = self
            .inner
            .lock()
            .interest_list
            .values()
            .cloned()
            .map(|(e, f)| (e, f, 0))
            .collect();
        while let Some((event, file, depth)) = stack.pop() {
            if let Ok(child) = file.clone().downcast_arc::<Epoll>() {
                if depth >= EPOLL_MAX_NEST_DEPTH {
                    continue;
                }
                let nested: Vec<(EpollEvent, Arc<dyn FileLike>, usize)> = child
                    .inner
                    .lock()
                    .interest_list
                    .values()
                    .cloned()
                    .map(|(e, f)| (e, f, depth + 1))
                    .collect();
                stack.extend(nested);
                continue;
            }
            let interest = PollEvents::from_bits_truncate(event.events as u16);
            if let Ok(status) = file.poll(interest) {
                if ready_events(event.events, &status) != 0 {
                    return true;
                }
            }
        }
        false
    }
}

#[async_trait]
impl FileLike for Epoll {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        // Same trap as eventfd/signalfd/timerfd before `take_settable`: a
        // no-op `Ok(())` made `fcntl(F_SETFL, O_NONBLOCK)` "succeed" while
        // `F_GETFL` never changed.
        self.flags.lock().take_settable(f);
        Ok(())
    }

    // Linux `eventpoll_fops` has no `.read`/`.write` — vfs returns `-EINVAL`,
    // not `-ENOSYS` (which means "syscall missing"). Probes that treat those
    // differently must see the Linux errno.
    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        // An epoll fd is readable iff any watched fd is ready. Surfacing this is
        // what lets a nested epoll (e.g. libinput's fd inside wlroots' event
        // loop) wake an outer epoll/poll.
        Ok(PollStatus {
            read: self.any_ready(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: self.any_ready(),
            write: false,
            error: false,
            hangup: false,
        })
    }
}

/// One interest-list entry as a scan sees it: the fd, its mask, its file.
type Watched = (FileDesc, EpollEvent, Arc<dyn FileLike>);

impl Epoll {
    /// wait for events on the interest list
    pub async fn wait(&self, maxevents: usize, timeout_msecs: isize) -> LxResult<Vec<EpollEvent>> {
        let begin_time = kernel_hal::timer::timer_now();
        loop {
            // Snapshot the interest list, keeping each watched file's OWN
            // `Arc<dyn FileLike>`. Two reasons this is the handle to poll,
            // rather than re-resolving the fd number through the process:
            //  * Correctness — epoll watches the open file *description*, not
            //    the fd. If userspace closes the fd and reuses the number for
            //    something else, Linux keeps reporting the original
            //    description until EPOLL_CTL_DEL; re-resolving by number
            //    watched the wrong file.
            //  * Lifetime — polling the stored Arc means this future holds NO
            //    `&LinuxProcess` borrow across its `.await` points. The borrow
            //    reached the process through the thread, and a stale
            //    net/timer waker re-polling a parked epoll after the owning
            //    thread had torn down dereferenced freed process memory
            //    (a use-after-free #GP with the free-poison pattern in the
            //    faulting register). The Arc keeps exactly what we touch
            //    alive for as long as we touch it.
            let (interest_list, edges): (Vec<Watched>, _) = {
                let inner = self.inner.lock();
                let list = inner
                    .interest_list
                    .iter()
                    .map(|(fd, (event, file))| (*fd, *event, file.clone()))
                    .collect();
                (list, inner.edge.clone())
            };
            let watch_net = interest_list
                .iter()
                .any(|(fd, _, _)| crate::net::fd_is_socket(*fd));
            let watch_interactive = interest_list
                .iter()
                .any(|(fd, _, _)| crate::net::fd_is_interactive(*fd));
            crate::net::io_wait_tick(watch_net, watch_interactive);
            // Sync readiness scan. Do NOT call `async_poll` here: each scan used
            // to Box::pin+poll+drop a future per watched fd (libinput epoll in
            // labwc touches dozens). The futures never stayed parked across
            // the await below anyway (Dropped before IoMultiplexWait), but
            // constructing them blew the guard-page-less coroutine stack → #DF
            // in `__from_user` at session start. Wakeups come from the 4 ms
            // IoMultiplexWait tick (and HID/NET IRQ registration there).
            let mut events = Vec::new();
            let mut delivered = Vec::new();
            let mut scanned_edges = Vec::new();
            let now = kernel_hal::timer::timer_now();
            for (fd, event, file) in &interest_list {
                let interest = PollEvents::from_bits_truncate(event.events as u16);
                // An edge-triggered entry on a file that counts its
                // publications. The counter is read BEFORE the level: a
                // publication after this line moves it past what is stored,
                // and the next scan (or the edge subscription below) sees it.
                let seq = if event.events & EPOLLET != 0 {
                    file.readiness_seq(interest)
                } else {
                    None
                };
                let status = match file.poll(interest) {
                    Ok(status) => status,
                    Err(err) => return Err(err),
                };
                let mut ready = ready_events(event.events, &status);
                if let Some(seq) = seq {
                    let (report, state) = edge_step(edges.get(fd).copied(), seq, ready, now);
                    scanned_edges.push((*fd, file.clone(), state));
                    if !report {
                        ready = 0;
                    }
                }
                if ready != 0 {
                    events.push(EpollEvent {
                        events: ready,
                        data: event.data,
                    });
                    // Which of these is an `EPOLLONESHOT` entry is decided in
                    // `disarm_oneshot`, under the lock: this snapshot was
                    // taken without it, and a mask read from it can already
                    // be stale.
                    delivered.push((*fd, file.clone()));
                    if events.len() >= maxevents {
                        break;
                    }
                }
            }

            self.store_edges(&scanned_edges);
            if !events.is_empty() {
                self.disarm_oneshot(&delivered);
                return Ok(events);
            }

            if timeout_msecs >= 0 {
                let deadline = begin_time + core::time::Duration::from_millis(timeout_msecs as u64);
                if kernel_hal::timer::timer_now() >= deadline {
                    return Ok(Vec::new());
                }
            }

            // A blocking wait may be interrupted by a deliverable signal, but a
            // signal must not hide events that are already ready, nor turn a
            // non-blocking/expired wait into `EINTR`.
            crate::process::check_signals()?;

            // Park a readiness waker on every watched fd that can carry one, so
            // a pipe write / unix-socket send / timerfd expiry wakes this task
            // the moment it happens instead of on the next re-scan tick. The
            // subscriptions are flat, synchronous registrations (see
            // `FileLike::subscribe_readiness` — NOT nested async_poll futures,
            // which overflowed the coroutine stack at desktop start). They are
            // taken AFTER the scan above found nothing: an event racing in
            // between is caught by the EventBus's latched flags, which fire the
            // waker at subscribe time.
            //
            // When every fd subscribed, the fallback re-scan stretches from
            // 4 ms to the covered tick — that alone removes ~96 % of the idle
            // wakeups of a parked desktop process. Any fd without an event
            // source (evdev nodes, signalfd, nested epolls) keeps the short
            // tick for its whole set, preserving the old behavior exactly.
            let waker =
                core::future::poll_fn(|cx| core::task::Poll::Ready(cx.waker().clone())).await;
            //
            // An edge-triggered entry parks on its file's next publication
            // instead: a level subscription fires at once on a flag that is
            // already up, and the level of a quiet entry is up by definition.
            let mut subs = alloc::vec::Vec::with_capacity(interest_list.len());
            let mut covered = !interest_list.is_empty();
            for (fd, event, file) in &interest_list {
                let interest = PollEvents::from_bits_truncate(event.events as u16);
                let edge = scanned_edges
                    .iter()
                    .find(|(f, _, _)| f == fd)
                    .map(|(_, _, state)| state.seq);
                let sub = match edge {
                    Some(seen) => file.subscribe_edge(interest, &waker, seen),
                    None => file.subscribe_readiness(interest, &waker),
                };
                match sub {
                    Some(sub) => subs.push(sub),
                    None => covered = false,
                }
            }
            let tick_ms = if covered {
                crate::net::wait::IO_WAIT_COVERED_TICK_MS
            } else {
                crate::net::wait::IO_WAIT_TICK_MS
            };
            crate::net::wait::IoMultiplexWait::with_tick(
                timeout_msecs,
                watch_net,
                watch_interactive,
                tick_ms,
            )
            .await;
            drop(subs);
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod abi_tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn epoll_event_matches_linux_uapi() {
        assert_eq!(size_of::<EpollEvent>(), 12);
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for `EPOLL_CTL_*` and the readiness walk.
    //!
    //! `Epoll::wait` needs a process and the timer, but everything a
    //! regression would break first does not: the interest list bookkeeping,
    //! and `any_ready`, which is what makes a **nested** epoll surface its
    //! inner readiness. wlroots/labwc depends on that exact behaviour — it
    //! puts `libinput_get_fd()` (an epoll fd) inside its own event loop, so
    //! losing it means input events are silently never dispatched.
    //!
    //! The watched files are real `EventFd`s: they are `FileLike`, they need
    //! no process context, and a `write` makes one readable.

    use super::*;
    use crate::fs::eventfd::EventFd;

    fn epoll() -> Arc<Epoll> {
        Epoll::new(OpenFlags::empty())
    }

    /// `read`/`write` on an epoll fd must be `-EINVAL`, not `-ENOSYS`:
    /// Linux has no fops for them, and glibc probes treat the two differently.
    #[test]
    fn read_write_on_epoll_are_einval_not_enosys() {
        use async_std::task::block_on;
        let ep = epoll();
        let mut buf = [0u8; 8];
        assert_eq!(block_on(ep.read(&mut buf)), Err(LxError::EINVAL));
        assert_eq!(ep.write(&[0u8; 8]), Err(LxError::EINVAL));
        assert_eq!(block_on(ep.read_at(0, &mut buf)), Err(LxError::EINVAL));
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on an epoll fd used to return success
    /// while leaving the bit clear — the same silent no-op eventfd had
    /// before `take_settable`.
    #[test]
    fn set_flags_turns_a_blocking_epoll_non_blocking() {
        let ep = epoll();
        assert!(!ep.flags().non_block());
        ep.set_flags(OpenFlags::NON_BLOCK).unwrap();
        assert!(ep.flags().non_block());
        ep.set_flags(OpenFlags::empty()).unwrap();
        assert!(!ep.flags().non_block());
        // F_SETFL goes through `after_setfl`, which keeps CLOEXEC; the ioctl
        // path mutates a copy of the current flags the same way.
        let with_cloexec = Epoll::new(OpenFlags::CLOEXEC);
        let mut f = with_cloexec.flags();
        f.set(OpenFlags::NON_BLOCK, true);
        with_cloexec.set_flags(f).unwrap();
        assert!(with_cloexec.flags().non_block() && with_cloexec.flags().close_on_exec());
    }

    /// An eventfd, readable iff `ready`.
    fn evfd(ready: bool) -> Arc<dyn FileLike> {
        let fd = EventFd::new(0, OpenFlags::empty());
        if ready {
            fd.write(&1u64.to_ne_bytes()).unwrap();
        }
        fd
    }

    const ADD: i32 = 1;
    const DEL: i32 = 2;
    const MOD: i32 = 3;

    fn ev(mask: PollEvents, data: u64) -> EpollEvent {
        EpollEvent {
            events: mask.bits() as u32,
            data,
        }
    }

    fn readable(e: &Epoll) -> bool {
        e.poll(PollEvents::IN).unwrap().read
    }

    #[test]
    fn ctl_add_del_mod_report_the_linux_errors() {
        let ep = epoll();
        let fd = FileDesc::from(5);
        let f = evfd(false);
        // MOD and DEL before the fd is in the interest list.
        assert_eq!(
            ep.ctl(MOD, fd, ev(PollEvents::IN, 0), Some(f.clone())),
            Err(LxError::ENOENT)
        );
        assert_eq!(
            ep.ctl(DEL, fd, ev(PollEvents::IN, 0), None),
            Err(LxError::ENOENT)
        );
        // ADD needs the resolved handle.
        assert_eq!(
            ep.ctl(ADD, fd, ev(PollEvents::IN, 0), None),
            Err(LxError::EBADF)
        );
        assert_eq!(
            ep.ctl(ADD, fd, ev(PollEvents::IN, 0), Some(f.clone())),
            Ok(0)
        );
        // A second ADD of the same fd is EEXIST, not a silent replace.
        assert_eq!(
            ep.ctl(ADD, fd, ev(PollEvents::IN, 0), Some(f.clone())),
            Err(LxError::EEXIST)
        );
        // An unknown op is EINVAL.
        assert_eq!(
            ep.ctl(9, fd, ev(PollEvents::IN, 0), Some(f)),
            Err(LxError::EINVAL)
        );
        // DEL removes it, and is ENOENT the second time.
        assert_eq!(ep.ctl(DEL, fd, ev(PollEvents::IN, 0), None), Ok(0));
        assert_eq!(
            ep.ctl(DEL, fd, ev(PollEvents::IN, 0), None),
            Err(LxError::ENOENT)
        );
    }

    #[test]
    fn readiness_follows_the_requested_mask() {
        let ep = epoll();
        let fd = FileDesc::from(3);
        let quiet = evfd(false);
        ep.ctl(ADD, fd, ev(PollEvents::IN, 7), Some(quiet.clone()))
            .unwrap();
        assert!(!readable(&ep));
        // An eventfd is always writable, so watching it for OUT is ready now.
        ep.ctl(MOD, fd, ev(PollEvents::OUT, 7), Some(quiet.clone()))
            .unwrap();
        assert!(readable(&ep));
        // Back to IN, and make the eventfd actually readable.
        ep.ctl(MOD, fd, ev(PollEvents::IN, 7), Some(quiet.clone()))
            .unwrap();
        assert!(!readable(&ep));
        quiet.write(&1u64.to_ne_bytes()).unwrap();
        assert!(readable(&ep));
        // Removing the only watched fd makes it quiet again.
        ep.ctl(DEL, fd, ev(PollEvents::IN, 7), None).unwrap();
        assert!(!readable(&ep));
    }

    #[test]
    fn an_empty_interest_list_is_never_ready() {
        assert!(!readable(&epoll()));
    }

    #[test]
    fn a_nested_epoll_surfaces_its_inner_readiness() {
        // The wlroots shape: inner watches a device fd, outer watches inner.
        let inner = epoll();
        let outer = epoll();
        let dev = evfd(false);
        inner
            .ctl(
                ADD,
                FileDesc::from(4),
                ev(PollEvents::IN, 1),
                Some(dev.clone()),
            )
            .unwrap();
        outer
            .ctl(
                ADD,
                FileDesc::from(5),
                ev(PollEvents::IN, 2),
                Some(inner.clone()),
            )
            .unwrap();
        assert!(!readable(&outer));
        // An event on the device must reach the outer epoll.
        dev.write(&1u64.to_ne_bytes()).unwrap();
        assert!(readable(&inner));
        assert!(readable(&outer));
    }

    /// A file with no poll operation, the way a regular file on ext4 is.
    struct Unpollable {
        base: KObjectBase,
    }

    impl_kobject!(Unpollable);

    #[async_trait]
    impl FileLike for Unpollable {
        fn flags(&self) -> OpenFlags {
            OpenFlags::empty()
        }
        fn set_flags(&self, _f: OpenFlags) -> LxResult {
            Ok(())
        }
        async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn write(&self, _buf: &[u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn can_epoll(&self) -> bool {
            false
        }
        fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
            Ok(PollStatus {
                read: true,
                write: true,
                error: false,
                hangup: false,
            })
        }
        async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
            self.poll(events)
        }
    }

    /// `do_epoll_ctl`: a target with no poll operation is `EPERM` for
    /// every op, judged before the op, the mask and the interest list; and
    /// it is never added, so `poll(2)` on the epoll stays quiet.
    #[test]
    fn a_file_that_cannot_be_polled_is_eperm_for_every_op() {
        let ep = epoll();
        let plain: Arc<dyn FileLike> = Arc::new(Unpollable {
            base: KObjectBase::new(),
        });
        for op in [ADD, MOD, DEL, 9] {
            assert_eq!(
                ep.ctl(
                    op,
                    FileDesc::from(7),
                    ev(PollEvents::IN, 0),
                    Some(plain.clone())
                ),
                Err(LxError::EPERM),
                "op {op}"
            );
        }
        // EPERM comes before the mask is looked at.
        assert_eq!(
            ep.ctl(
                MOD,
                FileDesc::from(7),
                EpollEvent {
                    events: EPOLLEXCLUSIVE,
                    data: 0
                },
                Some(plain)
            ),
            Err(LxError::EPERM)
        );
        assert!(!readable(&ep), "a refused file was watched anyway");
        assert_eq!(epoll_target_verdict(true, false), Ok(()));
        assert_eq!(epoll_target_verdict(false, true), Err(LxError::EPERM));
    }

    #[test]
    fn an_epoll_cannot_watch_itself_or_close_a_cycle() {
        let a = epoll();
        // The direct case is `f.file == tf.file`, EINVAL; a cycle through
        // another epoll is `ep_loop_check`, ELOOP.
        assert_eq!(
            a.ctl(
                ADD,
                FileDesc::from(1),
                ev(PollEvents::IN, 0),
                Some(a.clone())
            ),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            a.ctl(
                DEL,
                FileDesc::from(1),
                ev(PollEvents::IN, 0),
                Some(a.clone())
            ),
            Err(LxError::EINVAL),
            "for DEL too: the target is judged before the op"
        );
        let b = epoll();
        // a watches b is fine...
        a.ctl(
            ADD,
            FileDesc::from(2),
            ev(PollEvents::IN, 0),
            Some(b.clone()),
        )
        .unwrap();
        // ...but b watching a would close the loop, and `any_ready` would
        // then walk it forever.
        assert_eq!(
            b.ctl(
                ADD,
                FileDesc::from(3),
                ev(PollEvents::IN, 0),
                Some(a.clone())
            ),
            Err(LxError::ELOOP)
        );
    }

    #[test]
    fn nesting_deeper_than_ep_max_nests_is_refused() {
        // A chain a0 -> a1 -> ... -> a4, five levels deep.
        let chain: Vec<Arc<Epoll>> = (0..=EPOLL_MAX_NEST_DEPTH).map(|_| epoll()).collect();
        for i in 0..EPOLL_MAX_NEST_DEPTH {
            chain[i]
                .ctl(
                    ADD,
                    FileDesc::from(i as i32),
                    ev(PollEvents::IN, 0),
                    Some(chain[i + 1].clone()),
                )
                .unwrap();
        }
        // Hanging that whole chain off one more epoll exceeds the limit, and
        // is refused conservatively rather than walked.
        let root = epoll();
        assert_eq!(
            root.ctl(
                ADD,
                FileDesc::from(99),
                ev(PollEvents::IN, 0),
                Some(chain[0].clone())
            ),
            Err(LxError::ELOOP)
        );
        // The chain itself still works: readiness at the bottom reaches the top.
        let dev = evfd(true);
        chain[EPOLL_MAX_NEST_DEPTH]
            .ctl(ADD, FileDesc::from(50), ev(PollEvents::IN, 0), Some(dev))
            .unwrap();
        assert!(readable(&chain[0]));
    }

    /// `dup(epfd)` gives a second descriptor for the SAME `struct eventpoll`
    /// (`fs/eventpoll.c` never copies one), so `epoll_ctl` through either fd
    /// is visible through the other. The fd table hands out this very `Arc`
    /// for a dup, which is what makes that true here.
    #[test]
    fn a_dup_of_an_epoll_fd_watches_the_same_interest_list() {
        let ep = epoll();
        let dev = evfd(true);
        ep.ctl(ADD, FileDesc::from(6), ev(PollEvents::IN, 0), Some(dev))
            .unwrap();
        let copy: Arc<dyn FileLike> = ep.clone();
        assert!(copy.poll(PollEvents::IN).unwrap().read);
        ep.ctl(DEL, FileDesc::from(6), ev(PollEvents::IN, 0), None)
            .unwrap();
        assert!(!readable(&ep));
        assert!(
            !copy.poll(PollEvents::IN).unwrap().read,
            "a watch dropped through one fd is dropped for both"
        );
    }
}

#[cfg(test)]
mod flag_tests {
    //! The four `epoll_ctl(2)` bits that live above the sixteen a
    //! [`PollEvents`] holds, and what `EPOLLONESHOT` owes the caller once its
    //! event has been handed out.
    //!
    //! `Epoll::wait` needs a process and the timer, so the decisions it makes
    //! are tested where they live: [`epoll_ctl_events`] for what a request
    //! stores, [`ready_events`] for what an entry reports, and
    //! `disarm_oneshot` for what a delivery uses up.

    use super::*;
    use crate::fs::eventfd::EventFd;

    const ADD: i32 = EPOLL_CTL_ADD;
    const DEL: i32 = EPOLL_CTL_DEL;
    const MOD: i32 = EPOLL_CTL_MOD;

    const IN: u32 = PollEvents::IN.bits() as u32;
    const OUT: u32 = PollEvents::OUT.bits() as u32;
    const ERR: u32 = PollEvents::ERR.bits() as u32;
    const HUP: u32 = PollEvents::HUP.bits() as u32;

    fn epoll() -> Arc<Epoll> {
        Epoll::new(OpenFlags::empty())
    }

    /// An eventfd, readable iff `ready`. Always writable.
    fn evfd(ready: bool) -> Arc<dyn FileLike> {
        let fd = EventFd::new(0, OpenFlags::empty());
        if ready {
            fd.write(&1u64.to_ne_bytes()).unwrap();
        }
        fd
    }

    fn ev(events: u32) -> EpollEvent {
        EpollEvent { events, data: 0 }
    }

    /// The mask `ep` currently holds for `fd`, or `None` if it holds none.
    fn stored(ep: &Epoll, fd: FileDesc) -> Option<u32> {
        ep.inner
            .lock()
            .interest_list
            .get(&fd)
            .map(|(event, _)| event.events)
    }

    fn status(read: bool, write: bool, error: bool, hangup: bool) -> PollStatus {
        PollStatus {
            read,
            write,
            error,
            hangup,
        }
    }

    /// A `FileLike` whose readiness is whatever the test says it is. An
    /// `EventFd` can be made readable, but nothing in the host tests can be
    /// made to report `POLLERR`/`POLLHUP` -- which is exactly what an entry's
    /// stored mask now decides.
    struct Fixed {
        base: KObjectBase,
        status: PollStatus,
    }

    impl_kobject!(Fixed);

    impl Fixed {
        fn new(status: PollStatus) -> Arc<Self> {
            Arc::new(Fixed {
                base: KObjectBase::new(),
                status,
            })
        }
    }

    #[async_trait]
    impl FileLike for Fixed {
        fn flags(&self) -> OpenFlags {
            OpenFlags::empty()
        }
        fn set_flags(&self, _f: OpenFlags) -> LxResult {
            Ok(())
        }
        async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn write(&self, _buf: &[u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
            Ok(status(
                self.status.read,
                self.status.write,
                self.status.error,
                self.status.hangup,
            ))
        }
        async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
            self.poll(events)
        }
    }

    /// Their `uapi` values, and the reason they went missing: `events` is a
    /// `u32` and every reader narrowed it to a `u16` first.
    #[test]
    fn the_four_flags_above_sixteen_bits_have_their_linux_values() {
        assert_eq!(EPOLLEXCLUSIVE, 0x1000_0000);
        assert_eq!(EPOLLWAKEUP, 0x2000_0000);
        assert_eq!(EPOLLONESHOT, 0x4000_0000);
        assert_eq!(EPOLLET, 0x8000_0000);
        for flag in [EPOLLEXCLUSIVE, EPOLLWAKEUP, EPOLLONESHOT, EPOLLET] {
            assert_eq!(flag as u16, 0, "a u16 cannot carry {flag:#x}");
        }
    }

    /// A stored mask always reports `EPOLLERR`/`EPOLLHUP`, so the one mask
    /// that reports nothing is the empty one -- which is what makes zeroing a
    /// used-up `EPOLLONESHOT` entry mean "disabled" and nothing else.
    #[test]
    fn epoll_ctl_forces_err_and_hup_into_every_stored_mask() {
        assert_eq!(epoll_ctl_events(ADD, IN, false), Ok(IN | ERR | HUP));
        assert_eq!(epoll_ctl_events(ADD, 0, false), Ok(ERR | HUP));
        assert_eq!(epoll_ctl_events(MOD, OUT, false), Ok(OUT | ERR | HUP));
    }

    /// The bits a `PollEvents` cannot hold reach the interest list intact.
    #[test]
    fn oneshot_and_edge_trigger_survive_the_trip_through_epoll_ctl() {
        let ep = epoll();
        let fd = FileDesc::from(3);
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT | EPOLLET), Some(evfd(false)))
            .unwrap();
        let mask = stored(&ep, fd).unwrap();
        assert_eq!(mask & EPOLLONESHOT, EPOLLONESHOT);
        assert_eq!(mask & EPOLLET, EPOLLET);
        assert_eq!(mask, IN | EPOLLONESHOT | EPOLLET | ERR | HUP);
        // A MOD is held to the same mask as an ADD: it replaces the entry
        // outright, so anything it forgets is forgotten for good.
        ep.ctl(MOD, fd, ev(OUT | EPOLLET), Some(evfd(false)))
            .unwrap();
        assert_eq!(stored(&ep, fd), Some(OUT | EPOLLET | ERR | HUP));
    }

    /// `EPOLLEXCLUSIVE` changes *who* is woken, so Linux refuses every request
    /// whose wake-up set would be ambiguous instead of guessing.
    #[test]
    fn exclusive_is_refused_by_mod_by_a_nested_epoll_and_by_a_bit_it_cannot_share() {
        assert_eq!(
            epoll_ctl_events(MOD, IN | EPOLLEXCLUSIVE, false),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            epoll_ctl_events(ADD, IN | EPOLLEXCLUSIVE, true),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            epoll_ctl_events(ADD, IN | EPOLLEXCLUSIVE | EPOLLONESHOT, false),
            Err(LxError::EINVAL)
        );
    }

    /// ...and accepts the ones it can share a wake-up with.
    #[test]
    fn exclusive_is_accepted_with_the_bits_linux_allows_beside_it() {
        assert_eq!(
            epoll_ctl_events(
                ADD,
                IN | OUT | EPOLLEXCLUSIVE | EPOLLET | EPOLLWAKEUP,
                false
            ),
            Ok(IN | OUT | EPOLLEXCLUSIVE | EPOLLET | EPOLLWAKEUP | ERR | HUP)
        );
        // And the refusal reaches userspace through `ctl`, not just the helper.
        let ep = epoll();
        assert_eq!(
            ep.ctl(
                ADD,
                FileDesc::from(4),
                ev(IN | EPOLLEXCLUSIVE),
                Some(epoll())
            ),
            Err(LxError::EINVAL)
        );
    }

    /// An entry reports only what its mask asks for -- `EPOLLERR`/`EPOLLHUP`
    /// included, which is what lets an empty mask mean "report nothing".
    #[test]
    fn an_entry_reports_the_readiness_its_mask_asked_for() {
        let loud = status(true, true, true, true);
        assert_eq!(ready_events(IN | ERR | HUP, &loud), IN | ERR | HUP);
        assert_eq!(ready_events(OUT | ERR | HUP, &loud), OUT | ERR | HUP);
        assert_eq!(ready_events(0, &loud), 0);
        // The high flags are not readiness and are never reported back.
        assert_eq!(
            ready_events(IN | ERR | HUP | EPOLLONESHOT | EPOLLET, &loud),
            IN | ERR | HUP
        );
        // Nothing ready is nothing reported, however wide the mask.
        assert_eq!(
            ready_events(IN | OUT | ERR | HUP, &status(false, false, false, false)),
            0
        );
        // And each of the four stands on its own: a peer that hung up without
        // leaving anything to read is still a hangup, which is the whole
        // reason `poll(2)` has a bit for it.
        let mask = IN | OUT | ERR | HUP;
        assert_eq!(ready_events(mask, &status(false, false, false, true)), HUP);
        assert_eq!(ready_events(mask, &status(false, false, true, false)), ERR);
        assert_eq!(ready_events(mask, &status(false, true, false, false)), OUT);
        assert_eq!(ready_events(mask, &status(true, false, false, false)), IN);
    }

    /// `EPOLLRDNORM` and `EPOLLWRNORM` are `EPOLLIN` and `EPOLLOUT` under
    /// their streams names, and Linux reports each pair together. An entry
    /// armed with `EPOLLRDNORM` alone used to be one `epoll_wait` never
    /// returned.
    #[test]
    fn rdnorm_and_wrnorm_are_in_and_out_under_another_name() {
        const RDNORM: u32 = PollEvents::RDNORM.bits() as u32;
        const WRNORM: u32 = PollEvents::WRNORM.bits() as u32;
        let readable = status(true, false, false, false);
        let writable = status(false, true, false, false);
        assert_eq!(ready_events(RDNORM, &readable), RDNORM);
        assert_eq!(ready_events(IN | RDNORM, &readable), IN | RDNORM);
        assert_eq!(ready_events(RDNORM, &writable), 0);
        assert_eq!(ready_events(WRNORM, &writable), WRNORM);
        assert_eq!(ready_events(OUT | WRNORM, &writable), OUT | WRNORM);
        assert_eq!(ready_events(WRNORM, &readable), 0);
    }

    /// The point of the flag: one delivery, then the entry is off until the
    /// caller re-arms it. Losing the bit meant every waiter kept being handed
    /// the same fd, which is the race `EPOLLONESHOT` exists to prevent.
    #[test]
    fn a_oneshot_entry_stops_reporting_once_its_event_has_been_handed_out() {
        let ep = epoll();
        let fd = FileDesc::from(7);
        let file = evfd(true);
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(file.clone()))
            .unwrap();
        assert!(ep.poll(PollEvents::IN).unwrap().read);

        ep.disarm_oneshot(&[(fd, file.clone())]);
        assert_eq!(stored(&ep, fd), Some(0), "disabled, not removed");
        assert!(
            !ep.poll(PollEvents::IN).unwrap().read,
            "the fd is still readable; the entry is the thing that is spent"
        );

        // A re-arm through EPOLL_CTL_MOD brings it back, which is the whole
        // handshake a thread pool relies on.
        ep.ctl(MOD, fd, ev(IN | EPOLLONESHOT), Some(file)).unwrap();
        assert!(ep.poll(PollEvents::IN).unwrap().read);
    }

    /// A spent entry is silent about everything, errors and hangups included.
    #[test]
    fn a_spent_oneshot_entry_reports_neither_error_nor_hangup() {
        let ep = epoll();
        let fd = FileDesc::from(8);
        let file: Arc<dyn FileLike> = Fixed::new(status(false, false, true, true));
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(file.clone()))
            .unwrap();
        assert!(
            ep.poll(PollEvents::IN).unwrap().read,
            "an error is reported even though only EPOLLIN was asked for"
        );
        ep.disarm_oneshot(&[(fd, file)]);
        assert!(!ep.poll(PollEvents::IN).unwrap().read);
    }

    /// Everything else keeps firing for as long as it is ready.
    #[test]
    fn an_entry_that_did_not_ask_for_oneshot_is_never_disabled() {
        let ep = epoll();
        let fd = FileDesc::from(9);
        let file = evfd(true);
        ep.ctl(ADD, fd, ev(IN), Some(file.clone())).unwrap();
        ep.disarm_oneshot(&[(fd, file)]);
        assert_eq!(stored(&ep, fd), Some(IN | ERR | HUP));
        assert!(ep.poll(PollEvents::IN).unwrap().read);
    }

    /// The scan runs on a snapshot taken without the lock, so between the
    /// delivery and the disarm the fd number can have been dropped and reused
    /// for another file -- one that was never handed anything.
    #[test]
    fn disarming_leaves_alone_an_fd_that_now_holds_a_different_file() {
        let ep = epoll();
        let fd = FileDesc::from(10);
        let old = evfd(true);
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(old.clone()))
            .unwrap();
        let new = evfd(true);
        ep.ctl(DEL, fd, ev(0), None).unwrap();
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(new)).unwrap();

        ep.disarm_oneshot(&[(fd, old)]);
        assert_eq!(
            stored(&ep, fd),
            Some(IN | EPOLLONESHOT | ERR | HUP),
            "the replacement entry keeps its mask"
        );
        assert!(ep.poll(PollEvents::IN).unwrap().read);
    }
}

/// Closing a watched descriptor is what removes it from the interest list,
/// as epoll(7) promises; nothing here did it.
#[cfg(test)]
mod close_forgets_tests {
    use super::*;
    use crate::fs::eventfd::EventFd;
    use crate::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;

    const ADD: i32 = 1;
    const DEL: i32 = 2;

    fn ev(data: u64) -> EpollEvent {
        EpollEvent {
            events: PollEvents::IN.bits() as u32,
            data,
        }
    }

    fn ready_eventfd() -> Arc<dyn FileLike> {
        let fd = EventFd::new(0, OpenFlags::empty());
        fd.write(&1u64.to_ne_bytes()).unwrap();
        fd
    }

    /// A process with an epoll at `epfd` watching a readable eventfd at `fd`.
    fn watching() -> (LinuxProcess, Arc<Epoll>, FileDesc, Arc<dyn FileLike>) {
        let proc = LinuxProcess::new(RamFS::new(), 0);
        let ep = Epoll::new(OpenFlags::empty());
        proc.add_file(ep.clone()).unwrap();
        let file = ready_eventfd();
        let fd = proc.add_file(file.clone()).unwrap();
        ep.ctl(ADD, fd, ev(7), Some(file.clone())).unwrap();
        assert!(ep.poll(PollEvents::IN).unwrap().read);
        (proc, ep, fd, file)
    }

    fn watches(ep: &Epoll, fd: FileDesc) -> bool {
        ep.inner.lock().interest_list.contains_key(&fd)
    }

    #[test]
    fn closing_the_watched_descriptor_removes_it_from_the_interest_list() {
        let (proc, ep, fd, file) = watching();
        // The table and the interest list hold it, besides this test.
        assert_eq!(Arc::strong_count(&file), 3);
        proc.close_file(fd).unwrap();
        assert!(
            !watches(&ep, fd),
            "close is enough, no EPOLL_CTL_DEL needed"
        );
        assert!(!ep.poll(PollEvents::IN).unwrap().read);
        assert_eq!(ep.ctl(DEL, fd, ev(0), None), Err(LxError::ENOENT));
        // And the interest list no longer keeps the description alive: this
        // is what lets the other end of a closed socket or pipe see EOF.
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn a_dup_keeps_the_entry_until_the_last_descriptor_of_the_description_closes() {
        let (proc, ep, fd, file) = watching();
        let dup = proc.add_file_cloexec(file.clone(), false).unwrap();
        assert_ne!(dup, fd);
        proc.close_file(fd).unwrap();
        assert!(
            watches(&ep, fd),
            "the description is still open through the dup, as in Linux"
        );
        assert!(ep.poll(PollEvents::IN).unwrap().read);
        proc.close_file(dup).unwrap();
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn the_number_can_be_watched_again_once_it_names_a_new_file() {
        let (proc, ep, fd, _old) = watching();
        proc.close_file(fd).unwrap();
        let new = ready_eventfd();
        // The lowest free number is the one just closed.
        assert_eq!(proc.add_file(new.clone()).unwrap(), fd);
        assert_eq!(ep.ctl(ADD, fd, ev(8), Some(new.clone())), Ok(0));
        // The same description under the same number is still EEXIST.
        assert_eq!(ep.ctl(ADD, fd, ev(8), Some(new)), Err(LxError::EEXIST));
    }

    /// The interest list is keyed by (file, fd) in Linux, so a stale entry
    /// under a reused number never stops `EPOLL_CTL_ADD` of the new file,
    /// whichever path let the entry survive.
    #[test]
    fn add_replaces_an_entry_whose_file_is_no_longer_the_one_at_that_number() {
        let ep = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(5);
        let old = ready_eventfd();
        ep.ctl(ADD, fd, ev(1), Some(old.clone())).unwrap();
        let new: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::empty());
        assert_eq!(ep.ctl(ADD, fd, ev(2), Some(new.clone())), Ok(0));
        let (event, watched) = ep.inner.lock().interest_list[&fd].clone();
        let data = event.data;
        assert_eq!(data, 2);
        assert!(Arc::ptr_eq(&watched, &new));
        assert!(!ep.poll(PollEvents::IN).unwrap().read, "old is not watched");
        assert_eq!(ep.ctl(ADD, fd, ev(3), Some(new)), Err(LxError::EEXIST));
    }

    /// The nested-epoll path re-checks after taking the nesting lock; it
    /// answers the same two ways as the plain one.
    #[test]
    fn a_nested_epoll_is_eexist_twice_under_its_number_and_replaces_a_stale_one() {
        let outer = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(6);
        let stale = ready_eventfd();
        outer.ctl(ADD, fd, ev(1), Some(stale)).unwrap();
        let inner: Arc<dyn FileLike> = Epoll::new(OpenFlags::empty());
        assert_eq!(outer.ctl(ADD, fd, ev(2), Some(inner.clone())), Ok(0));
        assert!(!outer.poll(PollEvents::IN).unwrap().read, "stale is gone");
        assert_eq!(outer.ctl(ADD, fd, ev(3), Some(inner)), Err(LxError::EEXIST));
    }

    #[test]
    fn dup2_over_a_watched_descriptor_closes_it_for_the_epoll_too() {
        let (proc, ep, fd, file) = watching();
        let other = EventFd::new(0, OpenFlags::empty());
        let old = proc.replace_file(fd, other, false).unwrap().unwrap();
        assert!(Arc::ptr_eq(&old, &file));
        drop(old);
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn dup2_of_a_descriptor_onto_itself_changes_nothing() {
        let (proc, ep, fd, file) = watching();
        proc.replace_file(fd, file.clone(), false).unwrap();
        assert!(watches(&ep, fd));
    }

    #[test]
    fn close_range_and_the_exec_sweep_forget_what_they_close() {
        let (proc, ep, fd, file) = watching();
        proc.close_range(fd, fd);
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);

        let (proc, ep, fd, file) = watching();
        proc.set_fd_cloexec(fd, true).unwrap();
        proc.remove_cloexec_files();
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn every_entry_of_the_description_goes_whatever_number_it_was_added_under() {
        let (proc, ep, fd, file) = watching();
        let dup = proc.add_file_cloexec(file.clone(), false).unwrap();
        ep.ctl(ADD, dup, ev(9), Some(file.clone())).unwrap();
        proc.close_file(dup).unwrap();
        assert!(
            watches(&ep, fd) && watches(&ep, dup),
            "still open through fd"
        );
        proc.close_file(fd).unwrap();
        assert!(!watches(&ep, fd) && !watches(&ep, dup));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    /// An epoll that is itself closed takes its list with it; one that lives
    /// on in a dup keeps watching.
    #[test]
    fn closing_the_epoll_itself_is_not_a_watched_descriptor_going_away() {
        let (proc, ep, fd, _file) = watching();
        let epfd = proc.add_file_cloexec(ep.clone(), false).unwrap();
        proc.close_file(epfd).unwrap();
        assert!(watches(&ep, fd));
    }
}

/// `EPOLLET`: an edge-triggered entry is reported once per edge, not once per
/// `epoll_wait`. Reported by level, `mio`'s eventfd waker (which nobody ever
/// reads) made every `epoll_wait` of its loop return at once, for ever.
#[cfg(test)]
mod edge_trigger_tests {
    use super::*;
    use crate::fs::eventfd::EventFd;
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::future::Future;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};
    use core::time::Duration;

    const ADD: i32 = 1;
    const MOD: i32 = 3;
    const IN: u32 = PollEvents::IN.bits() as u32;
    const OUT: u32 = PollEvents::OUT.bits() as u32;

    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<Count>, Waker) {
        let count = Arc::new(Count(AtomicUsize::new(0)));
        (count.clone(), Waker::from(count))
    }

    /// One `epoll_wait(ep, events, 16, 0)`: the events it returns.
    fn wait_now(ep: &Epoll) -> Vec<(u32, u64)> {
        let (_, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = alloc::boxed::Box::pin(ep.wait(16, 0));
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(v)) => v.iter().map(|e| (e.events, e.data)).collect(),
            other => panic!(
                "a zero-timeout wait finishes at once: {:?}",
                other.is_ready()
            ),
        }
    }

    fn ev(events: u32, data: u64) -> EpollEvent {
        EpollEvent { events, data }
    }

    fn mio_waker() -> (Arc<EventFd>, Arc<Epoll>) {
        let efd = EventFd::new(0, OpenFlags::empty());
        let ep = Epoll::new(OpenFlags::empty());
        ep.ctl(
            ADD,
            FileDesc::from(5),
            ev(IN | EPOLLET, 9),
            Some(efd.clone()),
        )
        .unwrap();
        (efd, ep)
    }

    fn wake(efd: &EventFd) {
        efd.write(&1u64.to_ne_bytes()).unwrap();
    }

    #[test]
    fn an_eventfd_nobody_reads_is_reported_once_per_write() {
        let (efd, ep) = mio_waker();
        assert_eq!(wait_now(&ep), Vec::new(), "nothing written yet");
        wake(&efd);
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 9)]);
        // Still readable -- mio never reads it -- and nothing new happened.
        assert_eq!(wait_now(&ep), Vec::new());
        assert_eq!(wait_now(&ep), Vec::new());
        // A second write to a file that is already readable is an edge.
        wake(&efd);
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 9)]);
        assert_eq!(wait_now(&ep), Vec::new());
    }

    #[test]
    fn without_epollet_the_same_eventfd_is_reported_on_every_wait() {
        let efd = EventFd::new(0, OpenFlags::empty());
        let ep = Epoll::new(OpenFlags::empty());
        ep.ctl(ADD, FileDesc::from(5), ev(IN, 9), Some(efd.clone()))
            .unwrap();
        wake(&efd);
        for _ in 0..3 {
            assert_eq!(wait_now(&ep), alloc::vec![(IN, 9)]);
        }
    }

    #[test]
    fn epoll_ctl_mod_reports_a_level_that_is_already_up() {
        let (efd, ep) = mio_waker();
        wake(&efd);
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 9)]);
        assert_eq!(wait_now(&ep), Vec::new());
        ep.ctl(
            MOD,
            FileDesc::from(5),
            ev(IN | EPOLLET, 10),
            Some(efd.clone()),
        )
        .unwrap();
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 10)]);
        assert_eq!(wait_now(&ep), Vec::new());
    }

    #[test]
    fn a_quiet_entry_parks_and_the_next_write_wakes_it() {
        let (efd, ep) = mio_waker();
        wake(&efd);
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 9)]);
        // A blocking wait on the quiet entry must sleep. Parked with a level
        // subscription, the latched READABLE fired the waker at once, and the
        // wait went round again: the same spin, inside the kernel.
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = alloc::boxed::Box::pin(ep.wait(16, -1));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(count.0.load(Ordering::SeqCst), 0, "woken with nothing new");
        wake(&efd);
        assert!(count.0.load(Ordering::SeqCst) >= 1, "the write is the edge");
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(v)) => assert_eq!(v.len(), 1),
            _ => panic!("the woken wait reports the write"),
        }
    }

    #[test]
    fn a_publication_between_the_scan_and_the_park_still_wakes_the_wait() {
        // The counter is read before the level: a write that lands after the
        // scan moved it past what the subscription is told it has seen.
        let mut bus = crate::sync::EventBus::default();
        let seen = bus.seq_for(crate::sync::Event::READABLE);
        bus.set(crate::sync::Event::READABLE);
        let (count, waker) = counting_waker();
        assert_eq!(
            bus.subscribe_edge(crate::sync::Event::READABLE, &waker, seen),
            None
        );
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_bus_counts_every_publication_and_wakes_edge_waiters_on_the_next() {
        use crate::sync::{Event, EventBus};
        let mut bus = EventBus::default();
        bus.set(Event::READABLE);
        let s1 = bus.seq();
        // Already set: no change of the flags, still a publication.
        bus.set(Event::READABLE);
        assert_eq!(bus.seq(), s1 + 1);
        // Clearing publishes nothing.
        bus.clear(Event::READABLE);
        assert_eq!(bus.seq(), s1 + 1);
        bus.set(Event::READABLE);
        // `seen` is the counter for the mask the subscription asks about, not
        // the whole bus: a `WRITABLE` publication below must leave it alone.
        let seen = bus.seq_for(Event::READABLE);
        let (count, waker) = counting_waker();
        // A latched flag does not fire an edge subscription...
        let id = bus.subscribe_edge(Event::READABLE, &waker, seen);
        assert!(id.is_some());
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
        // ...nor does a publication of something it did not ask for...
        bus.set(Event::WRITABLE);
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
        // ...and the next one it did ask for fires it, once.
        bus.set(Event::READABLE);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        bus.set(Event::READABLE);
        assert_eq!(count.0.load(Ordering::SeqCst), 1, "one-shot");
        // Unsubscribing removes a parked edge waiter.
        let seen = bus.seq_for(Event::READABLE);
        let id = bus.subscribe_edge(Event::READABLE, &waker, seen).unwrap();
        assert_eq!(bus.get_callback_len(), 1);
        bus.unsubscribe(id);
        assert_eq!(bus.get_callback_len(), 0);
        bus.set(Event::READABLE);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_unix_socket_with_unread_data_is_reported_once_per_message() {
        use crate::net::unix::UnixSocketState;
        let a = UnixSocketState::new();
        let b = UnixSocketState::new();
        UnixSocketState::connect_pair(&a, &b);
        let ep = Epoll::new(OpenFlags::empty());
        ep.ctl(ADD, FileDesc::from(5), ev(IN | EPOLLET, 1), Some(a.clone()))
            .unwrap();
        assert_eq!(wait_now(&ep), Vec::new());
        FileLike::write(&*b, b"one").unwrap();
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 1)]);
        // Unread, and nothing new: tokio leaves it there under backpressure.
        assert_eq!(wait_now(&ep), Vec::new());
        FileLike::write(&*b, b"two").unwrap();
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 1)]);
        assert_eq!(wait_now(&ep), Vec::new());
    }

    #[test]
    fn a_pipe_with_unread_data_is_reported_once_per_write() {
        use crate::fs::pipe::Pipe;
        use crate::fs::File;
        use rcore_fs::vfs::INode;
        let (r, w) = Pipe::create_pair();
        let w = Arc::new(w);
        let read_end: Arc<dyn FileLike> = File::new(
            Arc::new(r),
            OpenFlags::RDONLY,
            alloc::string::String::from("pipe"),
        );
        let ep = Epoll::new(OpenFlags::empty());
        ep.ctl(ADD, FileDesc::from(5), ev(IN | EPOLLET, 2), Some(read_end))
            .unwrap();
        assert_eq!(wait_now(&ep), Vec::new());
        w.write_at(0, b"one").unwrap();
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 2)]);
        assert_eq!(wait_now(&ep), Vec::new());
        w.write_at(0, b"two").unwrap();
        assert_eq!(wait_now(&ep), alloc::vec![(IN, 2)]);
        assert_eq!(wait_now(&ep), Vec::new());
    }

    /// Always readable, and keeps no publication counter.
    struct Uncounted {
        base: KObjectBase,
    }
    impl_kobject!(Uncounted);
    #[async_trait]
    impl FileLike for Uncounted {
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
            Ok(PollStatus {
                read: true,
                write: false,
                error: false,
                hangup: false,
            })
        }
        async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
            self.poll(events)
        }
    }

    #[test]
    fn a_file_that_counts_nothing_is_still_reported_by_level() {
        // Without a counter this kernel cannot see the file's edges, and an
        // edge it cannot see must not become an event it never reports.
        let ep = Epoll::new(OpenFlags::empty());
        let file: Arc<dyn FileLike> = Arc::new(Uncounted {
            base: KObjectBase::new(),
        });
        ep.ctl(ADD, FileDesc::from(5), ev(IN | EPOLLET, 3), Some(file))
            .unwrap();
        for _ in 0..3 {
            assert_eq!(wait_now(&ep), alloc::vec![(IN, 3)]);
        }
    }

    const T0: Duration = Duration::from_secs(10);

    #[test]
    fn edge_step_reports_new_bits_and_every_ready_bit_with_them() {
        let (report, st) = edge_step(None, 4, IN, T0);
        assert!(report);
        assert_eq!(st.quiet, IN);
        // OUT rises with no publication: new, and IN rides along.
        let (report, st) = edge_step(Some(st), 4, IN | OUT, T0);
        assert!(report);
        assert_eq!(st.quiet, IN | OUT);
        let (report, _) = edge_step(Some(st), 4, IN | OUT, T0);
        assert!(!report);
    }

    #[test]
    fn edge_step_rearms_a_bit_it_has_seen_fall() {
        let (_, st) = edge_step(None, 4, IN, T0);
        // IN fell (the program drained it) with no publication to see.
        let (report, st) = edge_step(Some(st), 4, 0, T0);
        assert!(!report);
        assert_eq!(st.quiet, 0);
        let (report, _) = edge_step(Some(st), 4, IN, T0);
        assert!(report, "low then high is an edge");
    }

    #[test]
    fn edge_step_follows_the_counter_even_when_nothing_is_ready() {
        let (_, st) = edge_step(None, 4, IN, T0);
        let (report, st) = edge_step(Some(st), 7, 0, T0);
        assert!(!report);
        // The stored counter is the one just read: a park after this scan
        // must not be woken by the publications this scan already saw.
        assert_eq!(st.seq, 7);
        let (report, _) = edge_step(Some(st), 7, 0, T0);
        assert!(!report);
    }

    #[test]
    fn edge_step_reports_a_quiet_level_again_after_the_rearm_delay() {
        let (_, st) = edge_step(None, 4, IN, T0);
        let (report, st2) = edge_step(Some(st), 4, IN, T0 + EDGE_REARM / 2);
        assert!(!report);
        assert_eq!(st2.since, T0, "quiet scans do not push the deadline");
        let (report, st3) = edge_step(Some(st2), 4, IN, T0 + EDGE_REARM);
        assert!(report);
        assert_eq!(st3.since, T0 + EDGE_REARM);
    }
}

#[cfg(test)]
mod wait_batch_tests {
    //! What one `epoll_wait` hands back, and what it leaves for the next one.
    //!
    //! The whole of `Epoll::wait` had been exercised through one shape:
    //! `epoll_wait(ep, events, 16, 0)` on a single watched fd. The two things
    //! it does with a set of several -- stop at `maxevents`, and give up on a
    //! file whose `poll` fails -- had no test, and both are where userspace
    //! either loses an event or is told a wrong number of them.

    use super::*;
    use crate::fs::eventfd::EventFd;
    use core::future::Future;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};

    const IN: u32 = PollEvents::IN.bits() as u32;
    const ADD: i32 = 1;

    struct Nop;

    impl alloc::task::Wake for Nop {
        fn wake(self: Arc<Self>) {}
    }

    /// `epoll_wait(ep, events, maxevents, 0)`: the `(events, data)` pairs it
    /// returns, or the error it fails with.
    fn wait_now(ep: &Epoll, maxevents: usize) -> LxResult<Vec<(u32, u64)>> {
        let waker = Waker::from(Arc::new(Nop));
        let mut cx = Context::from_waker(&waker);
        let mut fut = alloc::boxed::Box::pin(ep.wait(maxevents, 0));
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(r) => r.map(|v| v.iter().map(|e| (e.events, e.data)).collect()),
            Poll::Pending => panic!("a zero-timeout wait finishes at once"),
        }
    }

    /// Three readable eventfds under three numbers, each with its own `data`.
    ///
    /// `extra` goes into every stored mask, so the caller can make the
    /// entries one-shot and have each delivery consume its own.
    fn three_ready(extra: u32) -> Arc<Epoll> {
        let ep = Epoll::new(OpenFlags::empty());
        for n in 0..3u64 {
            let efd = EventFd::new(0, OpenFlags::empty());
            efd.write(&1u64.to_ne_bytes()).unwrap();
            ep.ctl(
                ADD,
                FileDesc::from(n as i32 + 3),
                EpollEvent {
                    events: IN | extra,
                    data: n,
                },
                Some(efd),
            )
            .unwrap();
        }
        ep
    }

    /// `maxevents` is the size of the caller's array, so returning more than
    /// it asked for is a write past the end of it. One event means one, and
    /// the fds that did not fit are still ready for the next call -- a batch
    /// that dropped them would lose the event for good, which is the bug
    /// `maxevents` exists to prevent rather than cause.
    ///
    /// The entries are `EPOLLONESHOT` so that each delivery consumes its own
    /// and nothing else: on level-triggered entries nothing is consumed
    /// either way, and a `wait` that scanned every entry and only truncated
    /// the batch it returns would pass just the same while disarming the
    /// one-shots it never handed out -- userspace would never see those
    /// events, which is exactly what `maxevents` must not cost. So the
    /// second call is asked for an exact set, not a count that happens to
    /// match.
    #[test]
    fn a_batch_stops_at_maxevents_and_the_rest_wait_for_the_next_call() {
        for ask in 1..=3usize {
            let ep = three_ready(EPOLLONESHOT);
            let first = wait_now(&ep, ask).unwrap();
            assert_eq!(first.len(), ask, "asked for {}", ask);
            // Exactly the entries the first batch did not reach, and they are
            // still armed: a scan that went past `ask` would have spent them.
            let rest = wait_now(&ep, 16).unwrap();
            assert_eq!(rest.len(), 3 - ask, "left over after asking {}", ask);
            let mut seen: Vec<u64> = first
                .iter()
                .chain(rest.iter())
                .map(|(_, data)| *data)
                .collect();
            seen.sort_unstable();
            assert_eq!(seen, alloc::vec![0, 1, 2], "after asking {}", ask);
            // And now every one of them is spent, none twice.
            assert_eq!(wait_now(&ep, 16).unwrap(), Vec::new());
        }
    }

    /// Asking for more than are ready returns what there is, not a short
    /// batch padded or a wait.
    #[test]
    fn asking_for_more_than_are_ready_returns_what_there_is() {
        let ep = three_ready(0);
        let got = wait_now(&ep, 16).unwrap();
        assert_eq!(got.len(), 3);
        let mut data: Vec<u64> = got.iter().map(|(_, d)| *d).collect();
        data.sort_unstable();
        assert_eq!(data, alloc::vec![0, 1, 2]);
    }

    /// A file whose `poll` fails, the way a device node whose driver has gone
    /// away does.
    struct Broken {
        base: KObjectBase,
        polled: AtomicUsize,
    }

    impl_kobject!(Broken);

    #[async_trait]
    impl FileLike for Broken {
        fn flags(&self) -> OpenFlags {
            OpenFlags::empty()
        }
        fn set_flags(&self, _f: OpenFlags) -> LxResult {
            Ok(())
        }
        async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::EIO)
        }
        fn write(&self, _buf: &[u8]) -> LxResult<usize> {
            Err(LxError::EIO)
        }
        async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::EIO)
        }
        fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
            self.polled.fetch_add(1, Ordering::SeqCst);
            Err(LxError::ENODEV)
        }
        async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
            self.poll(events)
        }
    }

    fn broken() -> Arc<Broken> {
        Arc::new(Broken {
            base: KObjectBase::new(),
            polled: AtomicUsize::new(0),
        })
    }

    /// The scan gives up on the whole wait with that error. The alternative
    /// -- skip the fd and report the others -- is what makes a broken fd
    /// invisible: an event loop that never learns the fd is gone keeps it in
    /// the interest list and is scanned for it on every pass for ever.
    #[test]
    fn a_file_whose_poll_fails_fails_the_whole_wait() {
        let ep = Epoll::new(OpenFlags::empty());
        let bad = broken();
        ep.ctl(
            ADD,
            FileDesc::from(4),
            EpollEvent {
                events: IN,
                data: 1,
            },
            Some(bad.clone()),
        )
        .unwrap();
        assert_eq!(wait_now(&ep, 16), Err(LxError::ENODEV));
        assert_eq!(bad.polled.load(Ordering::SeqCst), 1);
    }

    /// And it does so whatever else is in the set: the error is the answer
    /// even when another fd is ready, and the ready fd is still ready once
    /// the broken one is gone. The ready one must not have been counted as
    /// delivered -- an `EPOLLONESHOT` entry disarmed by a wait that returned
    /// an error would have been spent without userspace ever seeing it.
    #[test]
    fn a_ready_fd_beside_a_broken_one_is_not_spent_by_the_failed_wait() {
        let ep = Epoll::new(OpenFlags::empty());
        let efd = EventFd::new(0, OpenFlags::empty());
        efd.write(&1u64.to_ne_bytes()).unwrap();
        ep.ctl(
            ADD,
            FileDesc::from(3),
            EpollEvent {
                events: IN | EPOLLONESHOT,
                data: 7,
            },
            Some(efd),
        )
        .unwrap();
        let bad = broken();
        ep.ctl(
            ADD,
            FileDesc::from(4),
            EpollEvent {
                events: IN,
                data: 9,
            },
            Some(bad),
        )
        .unwrap();
        assert_eq!(wait_now(&ep, 16), Err(LxError::ENODEV));
        // Drop the broken fd the way `EPOLL_CTL_DEL` would, and the one-shot
        // entry is still armed.
        ep.ctl(
            2,
            FileDesc::from(4),
            EpollEvent { events: 0, data: 0 },
            None,
        )
        .unwrap();
        assert_eq!(wait_now(&ep, 16).unwrap(), alloc::vec![(IN, 7)]);
        assert_eq!(wait_now(&ep, 16).unwrap(), Vec::new(), "now it is spent");
    }
}

#[cfg(test)]
mod edge_bookkeeping_tests {
    //! The two places the edge-trigger state is pruned, neither of which any
    //! test named.
    //!
    //! The state is what keeps an edge-triggered entry quiet about a level
    //! that has not moved. Left behind on an entry that is no longer the one
    //! it was computed for, it silences the wrong fd -- and a silenced fd is
    //! an event loop that never wakes, which is the whole failure mode
    //! `EPOLLET` support exists to get right (`mio`, under every `tokio`
    //! program, registers its eventfd waker this way).

    use super::*;
    use crate::fs::eventfd::EventFd;

    const ADD: i32 = 1;
    const DEL: i32 = 2;
    const MOD: i32 = 3;
    const IN: u32 = PollEvents::IN.bits() as u32;

    fn evfd() -> Arc<dyn FileLike> {
        let fd = EventFd::new(0, OpenFlags::empty());
        fd.write(&1u64.to_ne_bytes()).unwrap();
        fd
    }

    fn state() -> EdgeState {
        EdgeState {
            seq: 11,
            quiet: IN,
            since: core::time::Duration::from_millis(5),
        }
    }

    fn stored_edge(ep: &Epoll, fd: FileDesc) -> Option<EdgeState> {
        ep.inner.lock().edge.get(&fd).copied()
    }

    fn add(ep: &Epoll, fd: FileDesc, events: u32, file: Arc<dyn FileLike>) {
        ep.ctl(ADD, fd, EpollEvent { events, data: 0 }, Some(file))
            .unwrap();
    }

    /// The entry the scan looked at is still there, still edge-triggered and
    /// still the same file: its state is what the next scan must compare
    /// against.
    #[test]
    fn a_scan_of_a_live_edge_entry_stores_what_it_computed() {
        let ep = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(4);
        let f = evfd();
        add(&ep, fd, IN | EPOLLET, f.clone());
        assert_eq!(stored_edge(&ep, fd), None, "nothing scanned yet");
        ep.store_edges(&[(fd, f, state())]);
        assert_eq!(stored_edge(&ep, fd), Some(state()));
    }

    /// `EPOLL_CTL_MOD` that drops `EPOLLET` puts the entry back on level
    /// reporting, and a level-reported entry has no quiet state to honour.
    /// Storing one anyway would hand the next `MOD` back to `EPOLLET` a
    /// counter and a `quiet` mask from before the entry was re-armed, and
    /// `edge_step` would hold the entry quiet on a level it had never
    /// reported under this mask.
    #[test]
    fn a_scan_stores_nothing_for_an_entry_that_is_no_longer_edge_triggered() {
        let ep = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(4);
        let f = evfd();
        add(&ep, fd, IN | EPOLLET, f.clone());
        ep.ctl(
            MOD,
            fd,
            EpollEvent {
                events: IN,
                data: 0,
            },
            Some(f.clone()),
        )
        .unwrap();
        ep.store_edges(&[(fd, f, state())]);
        assert_eq!(stored_edge(&ep, fd), None);
    }

    /// The scan runs on a snapshot taken without the lock, so by the time it
    /// writes back, the number may have been closed and handed to another
    /// file -- one this scan never looked at. Its state belongs to the file
    /// that is gone, and the new one must start from nothing. Same reasoning
    /// as `disarm_oneshot`, and the same pointer comparison.
    #[test]
    fn a_scan_stores_nothing_for_a_number_that_now_holds_a_different_file() {
        let ep = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(4);
        let scanned = evfd();
        add(&ep, fd, IN | EPOLLET, scanned.clone());
        ep.ctl(DEL, fd, EpollEvent { events: 0, data: 0 }, None)
            .unwrap();
        let reused = evfd();
        add(&ep, fd, IN | EPOLLET, reused);
        ep.store_edges(&[(fd, scanned, state())]);
        assert_eq!(stored_edge(&ep, fd), None);
    }

    /// And nothing at all for a number that is no longer watched.
    #[test]
    fn a_scan_stores_nothing_for_an_entry_that_has_been_removed() {
        let ep = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(4);
        let f = evfd();
        add(&ep, fd, IN | EPOLLET, f.clone());
        ep.ctl(DEL, fd, EpollEvent { events: 0, data: 0 }, None)
            .unwrap();
        ep.store_edges(&[(fd, f, state())]);
        assert_eq!(stored_edge(&ep, fd), None);
        assert!(ep.inner.lock().interest_list.is_empty());
    }

    /// `forget_closed` answers how many entries it dropped: `eventpoll_release`
    /// runs for every epoll in the system when a description dies, and the
    /// count is how its caller knows which ones held it.
    #[test]
    fn forget_closed_counts_the_entries_of_that_description_and_no_others() {
        let ep = Epoll::new(OpenFlags::empty());
        let closing = evfd();
        let other = evfd();
        // The same description under two numbers, as a `dup` leaves it.
        add(&ep, FileDesc::from(4), IN, closing.clone());
        add(&ep, FileDesc::from(9), IN, closing.clone());
        add(&ep, FileDesc::from(5), IN, other);
        assert_eq!(ep.forget_closed(&closing), 2);
        assert_eq!(ep.forget_closed(&closing), 0, "already gone");
        assert_eq!(ep.inner.lock().interest_list.len(), 1);
    }

    /// The edge state of a dropped entry goes with it. Kept, it would silence
    /// the number as soon as it was watched again: `epoll_ctl` clears the
    /// state on ADD, but only the state under the number being added -- an
    /// entry under a *different* number, never re-added, would carry a
    /// counter belonging to a file that no longer exists for as long as the
    /// epoll lived.
    #[test]
    fn forget_closed_takes_the_edge_state_of_what_it_dropped() {
        let ep = Epoll::new(OpenFlags::empty());
        let closing = evfd();
        let other = evfd();
        let gone = FileDesc::from(4);
        let staying = FileDesc::from(5);
        add(&ep, gone, IN | EPOLLET, closing.clone());
        add(&ep, staying, IN | EPOLLET, other.clone());
        ep.store_edges(&[(gone, closing.clone(), state()), (staying, other, state())]);
        assert_eq!(stored_edge(&ep, gone), Some(state()));
        assert_eq!(ep.forget_closed(&closing), 1);
        assert_eq!(stored_edge(&ep, gone), None, "state of a dropped entry");
        assert_eq!(
            stored_edge(&ep, staying),
            Some(state()),
            "the entry that stayed must keep what its own scan computed"
        );
    }
}
