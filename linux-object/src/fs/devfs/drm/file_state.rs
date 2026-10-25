//! The per-open DRM file state (`DrmFileState`, Linux `struct drm_file`) and
//! what one `read()` on it finds (`EventRead`). Out of `drm.rs` for its size
//! ceiling; `drm.rs` re-exports both, so nothing else moved.

use super::*;

/// Per-open DRM file state (Linux `struct drm_file`).
///
/// `ATOMIC_CLIENT` and the readable event queue belong to the fd that
/// negotiated / queued them. GEM handles stay global for now (full F-M7
/// isolation is a larger change). Each `open(/dev/dri/card*)` gets a fresh
/// [`DrmFileState`] via [`super::super::drm_scheme::DrmDev::open_client`].
pub struct DrmFileState {
    atomic_client: AtomicBool,
    /// `drm_file.universal_planes`: set by `DRM_CLIENT_CAP_UNIVERSAL_PLANES`,
    /// and by `DRM_CLIENT_CAP_ATOMIC` together with `atomic_client`. Until a
    /// client sets it, `GETPLANERESOURCES` lists only overlay planes, as
    /// `drm_mode_getplane_res` does: a legacy client was written when the
    /// primary and the cursor were not planes, and would drive them as
    /// overlays.
    universal_planes: AtomicBool,
    /// `drm_file.aspect_ratio_allowed`: set by `DRM_CLIENT_CAP_ASPECT_RATIO`.
    /// Without it `GETCRTC` masks the aspect-ratio bits out of the mode.
    aspect_ratio: AtomicBool,
    events: Mutex<VecDeque<Vec<u8>>>,
    eventbus: Arc<Mutex<EventBus>>,
    /// The nouveau GEM handles this open imported through `PRIME_FD_TO_HANDLE`
    /// and has not `GEM_CLOSE`d: Linux's `drm_prime_file_private.lookup`, the
    /// table that makes a second import of the same dma-buf by the same file
    /// answer the handle it already has instead of taking another reference.
    prime_imports: Mutex<Vec<u32>>,
    /// The node this file was opened on: what its DRM master and its auth
    /// magic are scoped to. [`NO_MINOR`] for a registry placeholder that
    /// never issues an ioctl.
    minor: u32,
    /// `drm_file.magic`: minted by the first `GET_MAGIC`, 0 until then.
    magic: AtomicU32,
    /// The address of the blocking `WAIT_VBLANK` request whose sleep the
    /// 3 s cap stopped (`DrmDev::wait_vblank_sleep_capped`), or 0. The sync
    /// arm that runs next takes it and answers EBUSY with the count there
    /// is, as `drm_wait_vblank_ioctl` does when `DRM_WAIT_ON` runs out; it
    /// used to answer 0 as if the vblank asked for had come. Tied to the
    /// request, not just the file: a query another thread of the same open
    /// makes in between is not the wait that was stopped and must not take
    /// the mark.
    vblank_wait_timed_out: AtomicUsize,
    /// The address of the blocking `WAIT_VBLANK` request whose sleep ended
    /// because the CRTC went off under it, or 0. `drm_crtc_vblank_off`
    /// wakes the waiters and `wait_event_interruptible_timeout` ends on
    /// `!vblank->enabled`: that wait answers 0 with the count there is,
    /// where a request that arrives with the CRTC already off is EINVAL --
    /// so the sync arm takes this mark BEFORE it looks at the CRTC.
    vblank_wait_cut_short: AtomicUsize,
}

impl DrmFileState {
    pub fn new() -> Arc<Self> {
        Self::for_minor(NO_MINOR)
    }

    /// `drm_open_helper`: a fresh `drm_file` on `minor`, and, as
    /// `drm_master_open` does, the device's master when no open holds it.
    pub fn for_minor(minor: u32) -> Arc<Self> {
        let file = Arc::new(Self {
            atomic_client: AtomicBool::new(false),
            universal_planes: AtomicBool::new(false),
            aspect_ratio: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            eventbus: EventBus::new(),
            prime_imports: Mutex::new(Vec::new()),
            minor,
            magic: AtomicU32::new(0),
            vblank_wait_timed_out: AtomicUsize::new(0),
            vblank_wait_cut_short: AtomicUsize::new(0),
        });
        if minor != NO_MINOR {
            let owner = Arc::as_ptr(&file) as usize;
            master_open(minor, owner);
            if current_is_root() {
                authenticate(minor, owner);
            }
        }
        file
    }

    /// The node this file was opened on ([`NO_MINOR`] for a placeholder):
    /// which card's objects a handle of this file names.
    pub fn minor(&self) -> u32 {
        self.minor
    }

    /// `wait_vblank_sleep_capped` ran out of its cap on the request at `req`
    /// before the vblank it asked for came: the sync arm is to answer EBUSY.
    pub fn note_vblank_wait_timed_out(&self, req: usize) {
        self.vblank_wait_timed_out.store(req, Ordering::Release);
    }

    /// Whether the sleep for the request at `req` was the one the cap
    /// stopped. Taken once, so the same struct asked again is a fresh wait.
    pub fn take_vblank_wait_timed_out(&self, req: usize) -> bool {
        Self::take_vblank_wait_latch(&self.vblank_wait_timed_out, req)
    }

    /// `wait_vblank_sleep_capped` ended the sleep for the request at `req`
    /// because the CRTC went off under it: the sync arm is to answer 0
    /// with the count there is, not the EINVAL a CRTC that is off gets.
    pub fn note_vblank_wait_cut_short(&self, req: usize) {
        self.vblank_wait_cut_short.store(req, Ordering::Release);
    }

    /// Whether the sleep for the request at `req` was the one the CRTC
    /// going off ended. Taken once, like the cap's mark.
    pub fn take_vblank_wait_cut_short(&self, req: usize) -> bool {
        Self::take_vblank_wait_latch(&self.vblank_wait_cut_short, req)
    }

    /// A mark left for one request: only that request takes it, and once,
    /// so a query another thread of the same open makes in between is
    /// not the wait that was stopped, and the same struct asked again is
    /// a fresh wait.
    fn take_vblank_wait_latch(latch: &AtomicUsize, req: usize) -> bool {
        req != 0
            && latch
                .compare_exchange(req, 0, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
    }

    /// `drm_getmagic`: this file's magic, minted on first use and registered
    /// with its device so the master can `AUTH_MAGIC` it.
    pub fn magic(&self) -> u32 {
        let current = self.magic.load(Ordering::Acquire);
        if current != 0 {
            return current;
        }
        let mut state = DRM_STATE.lock();
        // Two GET_MAGIC on the same file can both see 0 before either stores.
        // Re-check under the lock so we mint once; the second caller must
        // return the same value Linux's `file_priv->magic` would.
        let current = self.magic.load(Ordering::Acquire);
        if current != 0 {
            return current;
        }
        let magic = state.next_magic;
        state.next_magic = state.next_magic.saturating_add(1);
        if state.next_magic == 0 {
            // 0 is libdrm's `drmIsMaster()` probe, never a minted magic.
            state.next_magic = 1;
        }
        state.magics.push((
            magic_device(self.minor),
            magic,
            self as *const DrmFileState as usize,
        ));
        self.magic.store(magic, Ordering::Release);
        magic
    }

    /// Record that this open imported `handle`. `true` when it is new to this
    /// file (a reference is owed), `false` when the file already holds it.
    pub fn note_prime_import(&self, handle: u32) -> bool {
        let mut imports = self.prime_imports.lock();
        if imports.contains(&handle) {
            return false;
        }
        imports.push(handle);
        true
    }

    /// This open let go of `handle` (`GEM_CLOSE`): a later import counts
    /// again. `true` when it was recorded.
    pub fn forget_prime_import(&self, handle: u32) -> bool {
        let mut imports = self.prime_imports.lock();
        match imports.iter().position(|h| *h == handle) {
            Some(i) => {
                imports.swap_remove(i);
                true
            }
            None => false,
        }
    }

    pub fn holds_prime_import(&self, handle: u32) -> bool {
        self.prime_imports.lock().contains(&handle)
    }

    pub fn set_atomic_client(&self, on: bool) {
        self.atomic_client.store(on, Ordering::Relaxed);
    }

    pub fn atomic_client(&self) -> bool {
        self.atomic_client.load(Ordering::Relaxed)
    }

    pub fn set_universal_planes(&self, on: bool) {
        self.universal_planes.store(on, Ordering::Relaxed);
    }

    pub fn universal_planes(&self) -> bool {
        self.universal_planes.load(Ordering::Relaxed)
    }

    pub fn set_aspect_ratio(&self, on: bool) {
        self.aspect_ratio.store(on, Ordering::Relaxed);
    }

    pub fn aspect_ratio(&self) -> bool {
        self.aspect_ratio.load(Ordering::Relaxed)
    }

    pub fn eventbus(&self) -> Arc<Mutex<EventBus>> {
        self.eventbus.clone()
    }

    pub fn has_events(&self) -> bool {
        !self.events.lock().is_empty()
    }

    /// Fill `buf` with as many whole pending DRM events as fit, like
    /// `drm_read()`.
    ///
    /// The three outcomes are distinct on purpose. Collapsing "the buffer is
    /// too small for the first event" into "nothing to read" livelocked the
    /// kernel: the caller saw EAGAIN, but the queue was NOT empty so
    /// `Event::READABLE` stayed set, so a blocking reader's wait resolved
    /// instantly, re-read, got EAGAIN again, and spun with no yield point.
    /// Linux puts the event back and returns what it has read so far (0 when
    /// nothing), and never blocks in that case.
    pub fn read_events(&self, buf: &mut [u8]) -> EventRead {
        let mut events = self.events.lock();
        let mut total = 0usize;
        while let Some(len) = events.front().map(|e| e.len()) {
            if total + len > buf.len() {
                // A buffer that cannot hold even one whole event is the
                // caller's error, not an empty queue.
                if total == 0 {
                    return EventRead::TooSmall;
                }
                break;
            }
            let ev = events.pop_front().expect("front() just returned it");
            buf[total..total + len].copy_from_slice(&ev[..len]);
            total += len;
        }
        if events.is_empty() {
            self.eventbus.lock().clear(Event::READABLE);
        }
        if total == 0 {
            EventRead::Empty
        } else {
            EventRead::Read(total)
        }
    }

    pub(super) fn push_event(&self, bytes: Vec<u8>) {
        let mut events = self.events.lock();
        events.push_back(bytes);
        self.eventbus.lock().set(Event::READABLE);
    }
}

/// What one `read()` on a DRM fd found. See [`DrmFileState::read_events`].
#[derive(Debug, PartialEq, Eq)]
pub enum EventRead {
    /// Nothing queued: EAGAIN, or block until one arrives.
    Empty,
    /// The buffer cannot hold even the first queued event: a read of 0
    /// bytes, as `drm_read()` answers when it has read nothing yet.
    TooSmall,
    /// This many bytes of whole events were copied.
    Read(usize),
}

impl Drop for DrmFileState {
    fn drop(&mut self) {
        // Linux destroys pending events when the drm_file closes. Timer jobs
        // hold only a Weak to us; drain any that still name this file so a
        // later tick does not clear FLIP_EVENT_PENDING for a stranger's flip.
        cancel_pending_timers_for_file(self as *const DrmFileState);
        // And frees the property blobs the file created
        // (`drm_property_destroy_user_blobs`); they were kept for ever.
        destroy_blobs_of(self as *const DrmFileState as usize);
        // `drm_master_release`: the master goes with the file that held it,
        // and so does the magic it minted.
        master_release(self.minor, self as *const DrmFileState as usize);
        super::super::drm_scheme::master_check::forget(self as *const DrmFileState as usize);
    }
}
