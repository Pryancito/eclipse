use super::*;
use crate::fs::devfs::kms_emu;

/// One open DRM file, driven the way libdrm drives it: a request number and
/// a pointer to a struct the caller owns. Deliberately NOT a set of direct
/// calls to `drm::*` helpers --- the entry point, the size reconciliation
/// and the `access_ok` check are part of what a client depends on, and a
/// test that skips them cannot see an ioctl go unreachable.
pub(super) struct Client {
    dev: DrmDev,
}

impl Client {
    /// `open("/dev/dri/card0")`.
    pub(super) fn open(minor: u32) -> Client {
        Client {
            dev: DrmDev::new(minor).open_client_dev(),
        }
    }

    /// `drmIoctl(fd, request, &arg)`.
    pub(super) fn ioctl<T>(&self, request: u32, arg: &mut T) -> Result<usize> {
        drm_ioctl(&self.dev, request, arg as *mut T as usize)
    }

    /// This open's `drm_file` state.
    pub(super) fn file_state(&self) -> &Arc<drm::DrmFileState> {
        self.dev.file_state()
    }

    /// `mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, offset)`,
    /// down to the object the mapping would be made of.
    pub(super) fn mmap(&self, offset: u64, len: usize) -> Result<Arc<VmObject>> {
        self.dev.get_vmo(offset as usize, len)
    }

    /// `DRM_IOCTL_MODE_MAP_DUMB`: the mmap offset for `handle`.
    pub(super) fn map_dumb(&self, handle: u32) -> Result<u64> {
        let mut req = DrmModeMapDumb {
            handle,
            pad: 0,
            offset: 0,
        };
        self.ioctl(DRM_IOCTL_MODE_MAP_DUMB, &mut req)?;
        Ok(req.offset)
    }

    /// `read(fd, buf, len)` --- how a compositor collects flip completions.
    pub(super) fn read_events(&self, buf: &mut [u8]) -> Result<usize> {
        self.dev.read_at(0, buf)
    }

    /// `write(fd, buf, len)`, which no DRM client has a reason to do.
    pub(super) fn write(&self, buf: &[u8]) -> Result<usize> {
        self.dev.write_at(0, buf)
    }

    /// `poll(fd, POLLIN)` --- the other half of how a compositor waits.
    pub(super) fn poll(&self) -> Result<PollStatus> {
        self.dev.poll()
    }

    /// `drmModeCreateDumbBuffer`: one scanout buffer.
    pub(super) fn create_dumb(&self, width: u32, height: u32) -> DrmModeCreateDumb {
        let mut req = DrmModeCreateDumb {
            height,
            width,
            bpp: 32,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        self.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut req)
            .expect("CREATE_DUMB");
        assert_ne!(req.handle, 0, "CREATE_DUMB gave no handle");
        assert_ne!(req.pitch, 0, "CREATE_DUMB gave no pitch");
        req
    }

    /// `drmModeAddFB2`: wrap a buffer in a framebuffer object.
    /// `ADDFB2` declaring a width narrower than the buffer's own pitch, so
    /// the framebuffer has off-screen padding at the end of every row --
    /// what a client with an alignment requirement, or a client whose
    /// surface is narrower than the mode, really registers.
    pub(super) fn addfb2_narrow(&self, buf: &DrmModeCreateDumb, width: u32) -> u32 {
        let mut cmd = DrmModeFbCmd2 {
            fb_id: 0,
            width,
            height: buf.height,
            pixel_format: drm::DRM_FORMAT_XRGB8888,
            flags: 0,
            handles: [buf.handle, 0, 0, 0],
            pitches: [buf.pitch, 0, 0, 0],
            offsets: [0; 4],
            modifier: [0; 4],
        };
        self.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd).expect("ADDFB2");
        assert_ne!(cmd.fb_id, 0, "ADDFB2 gave no fb id");
        cmd.fb_id
    }

    pub(super) fn addfb2(&self, buf: &DrmModeCreateDumb) -> u32 {
        // DRM_FORMAT_XRGB8888, which is what every GL swapchain on this
        // tree ends up presenting. (This used to spell the fourcc
        // "XRC4", and nothing noticed, because nothing looked.)
        let mut cmd = DrmModeFbCmd2 {
            fb_id: 0,
            width: buf.width,
            height: buf.height,
            pixel_format: drm::DRM_FORMAT_XRGB8888,
            flags: 0,
            handles: [buf.handle, 0, 0, 0],
            pitches: [buf.pitch, 0, 0, 0],
            offsets: [0; 4],
            modifier: [0; 4],
        };
        self.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd).expect("ADDFB2");
        assert_ne!(cmd.fb_id, 0, "ADDFB2 gave no fb id");
        cmd.fb_id
    }

    /// `drmModePageFlip` with `DRM_MODE_PAGE_FLIP_EVENT`.
    pub(super) fn page_flip(&self, crtc_id: u32, fb_id: u32, user_data: u64) -> Result<usize> {
        let mut flip = DrmModeCrtcPageFlip {
            crtc_id,
            fb_id,
            flags: 0x01, // DRM_MODE_PAGE_FLIP_EVENT
            reserved: 0,
            user_data,
        };
        self.ioctl(DRM_IOCTL_MODE_PAGE_FLIP, &mut flip)
    }

    /// `drmModeRmFB`.
    pub(super) fn rmfb(&self, fb_id: u32) -> Result<usize> {
        let mut id = fb_id;
        self.ioctl(DRM_IOCTL_MODE_RMFB, &mut id)
    }

    /// `drmModeDestroyDumbBuffer`.
    pub(super) fn destroy_dumb(&self, handle: u32) -> Result<usize> {
        let mut h = handle;
        self.ioctl(DRM_IOCTL_MODE_DESTROY_DUMB, &mut h)
    }
}

/// A `DRM_EVENT_FLIP_COMPLETE` as libdrm's `drmHandleEvent` reads it off
/// the fd: `struct drm_event_vblank`, 32 bytes.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct FlipEvent {
    pub(super) ev_type: u32,
    pub(super) length: u32,
    pub(super) user_data: u64,
    pub(super) crtc_id: u32,
}

fn le32(e: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([e[at], e[at + 1], e[at + 2], e[at + 3]])
}

fn le64(e: &[u8], at: usize) -> u64 {
    let (lo, hi) = (le32(e, at) as u64, le32(e, at + 4) as u64);
    if cfg!(target_endian = "little") {
        lo | (hi << 32)
    } else {
        hi | (lo << 32)
    }
}

pub(super) fn parse_events(buf: &[u8]) -> alloc::vec::Vec<FlipEvent> {
    buf.chunks_exact(32)
        .map(|e| FlipEvent {
            ev_type: le32(e, 0),
            length: le32(e, 4),
            user_data: le64(e, 8),
            crtc_id: le32(e, 28),
        })
        .collect()
}

/// `DRM_EVENT_FLIP_COMPLETE`.
pub(super) const FLIP_COMPLETE: u32 = 2;

/// A zeroed `struct drm_mode_card_res`, which is how libdrm starts both
/// passes of every `drmModeGetResources`.
pub(super) fn blank_card_res() -> DrmModeCardRes {
    DrmModeCardRes {
        fb_id_ptr: 0,
        crtc_id_ptr: 0,
        connector_id_ptr: 0,
        encoder_id_ptr: 0,
        count_fbs: 0,
        count_crtcs: 0,
        count_connectors: 0,
        count_encoders: 0,
        min_width: 0,
        max_width: 0,
        min_height: 0,
        max_height: 0,
    }
}

/// How many framebuffers and GEM handles the whole kernel is holding. The
/// tables are process-wide here (not per `drm_file` as in Linux), so a leak
/// shows up as these growing across a frame loop that should be in balance.
fn table_sizes() -> (usize, usize) {
    drm::table_sizes_for_test()
}

/// One frame, in the order libdrm issues it. This is the whole reason the
/// module exists: if any step of it regresses, no GL application can put a
/// pixel on the screen, and every one of these steps has broken at least
/// once.
#[test]
fn one_frame_allocates_wraps_flips_and_gets_its_completion() {
    // An output, so CRTC 1 exists: `PAGE_FLIP` looks the CRTC up first,
    // as Linux does, and a card with nothing to scan out reports no
    // CRTC at all. The screen holds the DRM test lock.
    let _screen = kms_emu::attach(64, 64);
    let client = Client::open(0);
    let before = table_sizes();

    let buf = client.create_dumb(64, 64);
    // The pitch is 64-byte aligned, which is what wlroots asks for and what
    // the copy-engine present path needs to match the scanout stride.
    assert_eq!(buf.pitch % 64, 0, "a dumb pitch must be 64-byte aligned");
    assert_eq!(buf.size, buf.pitch as u64 * 64);

    let fb = client.addfb2(&buf);

    // The flip itself, onto the emulated output; it owes an event.
    assert_eq!(client.page_flip(1, fb, 0xDEAD_BEEF), Ok(0));

    // The completion is scheduled for the next synthetic vblank, and
    // whether that slot is already past depends on the wall clock -- so
    // deliver it the way the timer tick would rather than letting the test
    // depend on the timing.
    drm::flush_pending_flip_completions();
    let mut events = [0u8; 64];
    let n = client.read_events(&mut events).expect("a completion event");
    assert_eq!(n, 32, "exactly one 32-byte event");
    assert_eq!(
        parse_events(&events[..n]),
        alloc::vec![FlipEvent {
            ev_type: FLIP_COMPLETE,
            length: 32,
            user_data: 0xDEAD_BEEF,
            crtc_id: 1,
        }],
        "the completion must carry back the client's own cookie",
    );

    // Teardown, in libdrm's order.
    assert_eq!(client.rmfb(fb), Ok(0));
    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    assert_eq!(
        table_sizes(),
        before,
        "a frame that came and went left something behind",
    );
}

/// The `glmark2` shape: scene after scene, each one a swapchain of two
/// buffers flipped alternately for many frames. What this catches is a leak
/// or a latch that only shows after the tenth frame -- the single-frame test
/// above passes happily with a flip counter that never decrements.
#[test]
fn a_double_buffered_loop_runs_clean_for_many_frames() {
    // Same output as the single frame above, for the same reason.
    let _screen = kms_emu::attach(64, 64);
    let client = Client::open(0);
    let before = table_sizes();

    let bufs = [client.create_dumb(64, 64), client.create_dumb(64, 64)];
    let fbs = [client.addfb2(&bufs[0]), client.addfb2(&bufs[1])];
    assert_ne!(fbs[0], fbs[1], "two ADDFB2 calls must give two fb ids");

    const FRAMES: u64 = 120;
    let mut collected = alloc::vec::Vec::new();
    for frame in 0..FRAMES {
        let fb = fbs[(frame % 2) as usize];
        assert_eq!(
            client.page_flip(1, fb, frame),
            Ok(0),
            "frame {} was refused",
            frame
        );
        // A compositor reads completions as they come; so does this.
        let mut events = [0u8; 256];
        if let Ok(n) = client.read_events(&mut events) {
            collected.extend(parse_events(&events[..n]));
        }
    }
    // The last flip's completion is scheduled for the next synthetic
    // vblank, and a host test has no timer tick to reach it -- every
    // earlier one was flushed by the flip that followed it. Deliver it the
    // way the timer would, then read what is there.
    drm::flush_pending_flip_completions();
    let mut events = [0u8; 4096];
    if let Ok(n) = client.read_events(&mut events) {
        collected.extend(parse_events(&events[..n]));
    }

    assert_eq!(
        collected.len(),
        FRAMES as usize,
        "one completion per flip, no more and no fewer",
    );
    // In order, and each carrying its own frame number: a dropped or
    // duplicated completion is what left wlroots handling a stale event.
    for (frame, ev) in collected.iter().enumerate() {
        assert_eq!(ev.ev_type, FLIP_COMPLETE);
        assert_eq!(
            ev.user_data, frame as u64,
            "completion {} carries the wrong cookie",
            frame
        );
    }

    for fb in fbs {
        assert_eq!(client.rmfb(fb), Ok(0));
    }
    for buf in &bufs {
        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }
    assert_eq!(
        table_sizes(),
        before,
        "{} frames leaked framebuffer or handle table entries",
        FRAMES,
    );
}

/// A flip onto a framebuffer the client already removed. Linux answers
/// `ENOENT` ("Unknown FB ID"), and the distinction matters: wlroots reads a
/// failed flip as the output being broken and retries the whole modeset, so
/// `EIO` here cost the entire desktop, while `ENOENT` names the real
/// problem (the client's own fb lifetime).
#[test]
fn a_flip_onto_a_removed_framebuffer_is_enoent() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);
    let fb = client.addfb2(&buf);
    assert_eq!(client.rmfb(fb), Ok(0));

    assert_eq!(
        client.page_flip(1, fb, 0),
        Err(FsError::EntryNotFound),
        "a flip onto a dead fb must name the fb, not the bus",
    );
    assert_eq!(
        crate::fs::LxError::from(FsError::EntryNotFound),
        crate::fs::LxError::ENOENT,
    );
    // And it owes no event: a completion for a flip that never happened is
    // what leaves a compositor waiting on a frame it will never get.
    let mut events = [0u8; 64];
    assert!(
        matches!(client.read_events(&mut events), Err(_) | Ok(0)),
        "a refused flip must not queue a completion",
    );
    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}

/// The flag validation `drm_mode_page_flip_ioctl` does before anything
/// else. `ASYNC` and the `TARGET_*` flags are `EINVAL` while the matching
/// capabilities report 0, and so is an unknown flag or a dirty reserved
/// word -- a client that gets one of these accepted goes on to believe in a
/// pacing guarantee this tree does not offer.
#[test]
fn the_page_flip_flags_are_validated_before_the_framebuffer_is_looked_up() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);
    let fb = client.addfb2(&buf);

    for (flags, reserved, what) in [
        (0x02u32, 0u32, "ASYNC while DRM_CAP_ASYNC_PAGE_FLIP is 0"),
        (
            0x04,
            0,
            "TARGET_ABSOLUTE while DRM_CAP_PAGE_FLIP_TARGET is 0",
        ),
        (
            0x08,
            0,
            "TARGET_RELATIVE while DRM_CAP_PAGE_FLIP_TARGET is 0",
        ),
        (0x10, 0, "a flag outside DRM_MODE_PAGE_FLIP_FLAGS"),
        (0x01, 1, "a non-zero reserved word"),
    ] {
        let mut flip = DrmModeCrtcPageFlip {
            crtc_id: 1,
            fb_id: fb,
            flags,
            reserved,
            user_data: 0,
        };
        assert_eq!(
            client.ioctl(DRM_IOCTL_MODE_PAGE_FLIP, &mut flip),
            Err(FsError::InvalidParam),
            "{} must be EINVAL",
            what,
        );
    }

    // Rejected flips owe no completions.
    let mut events = [0u8; 256];
    assert!(matches!(client.read_events(&mut events), Err(_) | Ok(0)));
    assert_eq!(client.rmfb(fb), Ok(0));
    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}

/// `drmIsKMS` wants `count_crtcs > 0 && count_connectors > 0 &&
/// count_encoders > 0`. With nothing to scan out -- no display registered and
/// no driver with hardware KMS, which is exactly this host run -- all three
/// must be zero, so a compositor does not adopt a card it cannot present on
/// and then fail to build a backend at all. The synthetic CRTC and connector
/// appear only once `software_kms_active()` does.
#[test]
fn a_node_with_nothing_to_scan_out_does_not_claim_to_be_a_kms_card() {
    let _serialised = drm::test_globals::lock();
    assert!(
        !drm::software_kms_active(),
        "this test is about the headless case; a display is registered",
    );
    let client = Client::open(0);
    let mut res = blank_card_res();
    client
        .ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut res)
        .expect("GETRESOURCES");
    assert_eq!(
        (res.count_crtcs, res.count_connectors, res.count_encoders),
        (0, 0, 0),
        "a node that cannot present must fail drmIsKMS",
    );
}

/// The count-then-fill protocol every libdrm getter uses: call once with
/// null pointers to learn the counts, allocate, call again to fill. The two
/// calls must agree, because the client sizes its heap allocation on the
/// first answer and the kernel writes on the strength of the second.
#[test]
fn the_resource_counts_do_not_change_between_the_probe_and_the_fill() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    // A framebuffer, so `count_fbs` has something to count and the two
    // passes have something to disagree about.
    let buf = client.create_dumb(64, 64);
    let fb = client.addfb2(&buf);

    let mut probe = blank_card_res();
    client
        .ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe)
        .expect("GETRESOURCES probe");
    assert!(probe.count_fbs >= 1, "the fb just created must be counted");

    // The fill pass, with a buffer sized from the probe.
    let mut ids = alloc::vec![0u32; probe.count_fbs as usize];
    let mut fill = blank_card_res();
    fill.fb_id_ptr = ids.as_mut_ptr() as u64;
    fill.count_fbs = probe.count_fbs;
    client
        .ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut fill)
        .expect("GETRESOURCES fill");
    assert_eq!(
        (fill.count_fbs, fill.count_crtcs, fill.count_connectors),
        (probe.count_fbs, probe.count_crtcs, probe.count_connectors),
        "the counts moved between the probe and the fill",
    );
    assert!(
        ids.contains(&fb),
        "the fill pass did not report the fb the probe counted",
    );

    assert_eq!(client.rmfb(fb), Ok(0));
    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}
