//! The syncobj ioctls that take an array of handles, driven through the
//! ioctl entry point with the nouveau uAPI on: how many handles they
//! take, and what an empty array means.
use super::gl_client_sequence_tests::Client;
use super::*;

/// The nouveau uAPI switch, on for one test and put back after; under
/// `drm::test_globals::lock()`, like every process-wide DRM knob. The
/// tests also hold the eventfd tests' lock: signaling a syncobj fires
/// the process-wide signal hook those tests install, whose walk
/// delivers their waiter (see `syncobj_eventfd`'s `TEST_SERIAL`).
struct NouveauOn(bool);
impl NouveauOn {
    fn new() -> Self {
        let was = zcore_drivers::display::nouveau_uapi_enabled();
        zcore_drivers::display::set_nouveau_uapi_enabled(true);
        NouveauOn(was)
    }
}
impl Drop for NouveauOn {
    fn drop(&mut self) {
        zcore_drivers::display::set_nouveau_uapi_enabled(self.0);
    }
}

fn create(c: &Client, signaled: bool) -> u32 {
    let mut req = DrmSyncobjCreate {
        handle: 0,
        flags: if signaled {
            DRM_SYNCOBJ_CREATE_SIGNALED
        } else {
            0
        },
    };
    c.ioctl(DRM_IOCTL_SYNCOBJ_CREATE, &mut req).expect("CREATE");
    req.handle
}

fn destroy(c: &Client, handle: u32) {
    let mut req = DrmSyncobjDestroy { handle, pad: 0 };
    c.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut req)
        .expect("DESTROY");
}

/// `SYNCOBJ_WAIT` with an absolute deadline already passed: what is
/// signaled now decides. Gives back `first_signaled`.
fn wait(c: &Client, handles: &[u32], flags: u32) -> Result<u32> {
    let mut req = DrmSyncobjWait {
        handles: handles.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: handles.len() as u32,
        flags,
        first_signaled: 0xdead,
        pad: 0,
    };
    c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req)
        .map(|_| req.first_signaled)
}

fn timeline_wait(c: &Client, handles: &[u32], points: &[u64], flags: u32) -> Result<u32> {
    let mut req = DrmSyncobjTimelineWait {
        handles: handles.as_ptr() as u64,
        points: points.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: handles.len() as u32,
        flags,
        first_signaled: 0xdead,
        pad: 0,
    };
    c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut req)
        .map(|_| req.first_signaled)
}

/// `RESET` or `SIGNAL`.
fn array(c: &Client, cmd: u32, handles: &[u32]) -> Result<usize> {
    let mut req = DrmSyncobjArray {
        handles: handles.as_ptr() as u64,
        count_handles: handles.len() as u32,
        pad: 0,
    };
    c.ioctl(cmd, &mut req)
}

/// `TIMELINE_SIGNAL` or `QUERY`, `points` read or written per arm.
fn timeline_array(c: &Client, cmd: u32, handles: &[u32], points: &mut [u64]) -> Result<usize> {
    let mut req = DrmSyncobjTimelineArray {
        handles: handles.as_ptr() as u64,
        points: points.as_mut_ptr() as u64,
        count_handles: handles.len() as u32,
        flags: 0,
    };
    c.ioctl(cmd, &mut req)
}

/// NVK's `vk_drm_syncobj_get_type` probe exactly as Alpine's libdrm
/// 2.4.134 sends it: a WAIT on a SIGNALED syncobj, timeout 0, in the
/// deadline-sized struct (40 bytes; 48 for the timeline form). Anything
/// but 0 strips `VK_SYNC_FEATURE_CPU_WAIT` from NVK's syncobj type, and
/// the first submit that needs a binary CPU-wait type derefs the NULL at
/// the end of `supported_sync_types` (`libvulkan_nouveau.so+0xe9c48`).
///
/// These sizes go through `drm_ioctl_reconciled`'s kernel bounce buffer,
/// and the arm used to `ucheck` that kernel address: EFAULT on bare metal.
/// Under `libos` the user-half bound is off, so this test cannot see that
/// half by itself -- it pins the path (bounce buffer, reply copied back)
/// that the fix in `read_syncobj_wait` is about.
#[test]
fn the_deadline_sized_waits_nvk_probes_with_succeed_on_a_signaled_syncobj() {
    #[repr(C)]
    struct WaitDeadline {
        wait: DrmSyncobjWait,
        deadline_nsec: u64,
    }
    #[repr(C)]
    struct TimelineWaitDeadline {
        wait: DrmSyncobjTimelineWait,
        deadline_nsec: u64,
    }
    assert_eq!(core::mem::size_of::<WaitDeadline>(), 40);
    assert_eq!(core::mem::size_of::<TimelineWaitDeadline>(), 48);

    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _on = NouveauOn::new();
    let c = Client::open(0);
    let h = [create(&c, true)];

    let mut req = WaitDeadline {
        wait: DrmSyncobjWait {
            handles: h.as_ptr() as u64,
            timeout_nsec: 0,
            count_handles: 1,
            flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
            first_signaled: 0xdead,
            pad: 0,
        },
        deadline_nsec: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE, &mut req), Ok(0));
    assert_eq!(
        req.wait.first_signaled, 0,
        "reply copied back to the client"
    );

    let points = [1u64];
    let mut req = TimelineWaitDeadline {
        wait: DrmSyncobjTimelineWait {
            handles: h.as_ptr() as u64,
            points: points.as_ptr() as u64,
            timeout_nsec: 0,
            count_handles: 1,
            flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
            first_signaled: 0xdead,
            pad: 0,
        },
        deadline_nsec: 0,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE, &mut req),
        Ok(0)
    );
    assert_eq!(req.wait.first_signaled, 0);

    destroy(&c, h[0]);
}

/// `vkWaitForFences` on more fences than 64 is one `TIMELINE_WAIT` over
/// all of them (`vk_drm_syncobj_wait_many`), and `vkResetFences` one
/// `RESET`; Linux takes any number the allocator does. Every array arm
/// used to stop at 64 with EINVAL, which Mesa reports as a lost device.
#[test]
fn the_array_ioctls_take_more_than_sixty_four_handles() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _on = NouveauOn::new();
    let c = Client::open(0);
    const N: usize = 100;
    let handles: alloc::vec::Vec<u32> = (0..N).map(|_| create(&c, true)).collect();
    assert_eq!(wait(&c, &handles, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL), Ok(0));
    let ones = alloc::vec![1u64; N];
    assert_eq!(
        timeline_wait(&c, &handles, &ones, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL),
        Ok(0)
    );
    // RESET all: nothing is signaled any more, whichever is asked.
    assert_eq!(array(&c, DRM_IOCTL_SYNCOBJ_RESET, &handles), Ok(0));
    assert_eq!(wait(&c, &handles, 0), Err(FsError::TimedOut));
    assert_eq!(
        wait(&c, &handles[N - 1..], 0),
        Err(FsError::TimedOut),
        "the hundredth was reset too"
    );
    // SIGNAL all: the hundredth is signaled, and the first one found.
    assert_eq!(array(&c, DRM_IOCTL_SYNCOBJ_SIGNAL, &handles), Ok(0));
    assert_eq!(wait(&c, &handles[N - 1..], 0), Ok(0));
    assert_eq!(wait(&c, &handles, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL), Ok(0));
    let mut points = alloc::vec![0u64; N];
    assert_eq!(
        timeline_array(&c, DRM_IOCTL_SYNCOBJ_QUERY, &handles, &mut points),
        Ok(0)
    );
    assert!(points.iter().all(|&p| p == 1), "{:?}", points);
    // TIMELINE_SIGNAL all to 5: the query and the wait see every one.
    let mut fives = alloc::vec![5u64; N];
    assert_eq!(
        timeline_array(&c, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &handles, &mut fives),
        Ok(0)
    );
    assert_eq!(
        timeline_array(&c, DRM_IOCTL_SYNCOBJ_QUERY, &handles, &mut points),
        Ok(0)
    );
    assert!(points.iter().all(|&p| p == 5), "{:?}", points);
    assert_eq!(
        timeline_wait(&c, &handles, &fives, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL),
        Ok(0)
    );
    let sixes = alloc::vec![6u64; N];
    assert_eq!(
        timeline_wait(&c, &handles, &sixes, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL),
        Err(FsError::TimedOut)
    );
    // Only the last one at 6: found at its index, not at one of the 64.
    assert_eq!(
        timeline_array(
            &c,
            DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
            &handles[N - 1..],
            &mut [6u64]
        ),
        Ok(0)
    );
    assert_eq!(
        timeline_wait(&c, &handles, &sixes, 0),
        Ok(N as u32 - 1),
        "first_signaled"
    );
    for h in handles {
        destroy(&c, h);
    }
}

/// A wait on no handles is answered 0 at once, without reading the
/// array (Linux `drm_syncobj_wait_ioctl`); the arms that change or read
/// state refuse an empty array with EINVAL, as Linux does.
#[test]
fn a_wait_with_no_handles_returns_at_once_and_the_other_arrays_refuse_it() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _on = NouveauOn::new();
    let c = Client::open(0);
    let none: [u32; 0] = [];
    let mut req = DrmSyncobjWait {
        handles: 0,
        timeout_nsec: 0,
        count_handles: 0,
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
        first_signaled: 0xdead,
        pad: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req), Ok(0));
    assert_eq!(req.first_signaled, 0xdead, "not written");
    let mut req = DrmSyncobjTimelineWait {
        handles: 0,
        points: 0,
        timeout_nsec: 0,
        count_handles: 0,
        flags: 0,
        first_signaled: 0xdead,
        pad: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut req), Ok(0));
    assert_eq!(req.first_signaled, 0xdead);
    for cmd in [DRM_IOCTL_SYNCOBJ_RESET, DRM_IOCTL_SYNCOBJ_SIGNAL] {
        assert_eq!(
            array(&c, cmd, &none),
            Err(FsError::InvalidParam),
            "{:#x}",
            cmd
        );
    }
    // Real (if empty) arrays behind the pointers, so an arm that went
    // on to read its first entry would fail the assertion, not crash.
    let mut no_points = [0u64; 1];
    for cmd in [DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, DRM_IOCTL_SYNCOBJ_QUERY] {
        assert_eq!(
            timeline_array(&c, cmd, &none, &mut no_points[..0]),
            Err(FsError::InvalidParam),
            "{:#x}",
            cmd
        );
    }
}

/// Past what the kernel would allocate for the array, ENOMEM, before
/// the array is looked at (it is never read here: the pointer is null).
#[test]
fn an_array_past_what_the_kernel_would_allocate_is_enomem() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _on = NouveauOn::new();
    let c = Client::open(0);
    let too_many = SYNCOBJ_ARRAY_MAX + 1;
    let mut req = DrmSyncobjWait {
        handles: 0,
        timeout_nsec: 0,
        count_handles: too_many,
        flags: 0,
        first_signaled: 0,
        pad: 0,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req),
        Err(FsError::NoMemory)
    );
    let mut req = DrmSyncobjTimelineWait {
        handles: 0,
        points: 0,
        timeout_nsec: 0,
        count_handles: too_many,
        flags: 0,
        first_signaled: 0,
        pad: 0,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut req),
        Err(FsError::NoMemory)
    );
    for cmd in [DRM_IOCTL_SYNCOBJ_RESET, DRM_IOCTL_SYNCOBJ_SIGNAL] {
        let mut req = DrmSyncobjArray {
            handles: 0,
            count_handles: too_many,
            pad: 0,
        };
        assert_eq!(c.ioctl(cmd, &mut req), Err(FsError::NoMemory), "{:#x}", cmd);
    }
    for cmd in [DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, DRM_IOCTL_SYNCOBJ_QUERY] {
        let mut req = DrmSyncobjTimelineArray {
            handles: 0,
            points: 0,
            count_handles: too_many,
            flags: 0,
        };
        assert_eq!(c.ioctl(cmd, &mut req), Err(FsError::NoMemory), "{:#x}", cmd);
    }
    // The bound itself is fine, and a null array under it is EINVAL as
    // before (Linux: EFAULT from the copy).
    let mut req = DrmSyncobjArray {
        handles: 0,
        count_handles: SYNCOBJ_ARRAY_MAX,
        pad: 0,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_SYNCOBJ_SIGNAL, &mut req),
        Err(FsError::InvalidParam)
    );
    let mut req = DrmSyncobjWait {
        handles: 0,
        timeout_nsec: 0,
        count_handles: 1,
        flags: 0,
        first_signaled: 0,
        pad: 0,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req),
        Err(FsError::InvalidParam)
    );
}

/// `drm_syncobj_transfer_ioctl`: the padding is EINVAL, and so is any
/// flag but WAIT_FOR_SUBMIT (`drm_syncobj_find_fence`); without that
/// flag the source has to carry a fence at `src_point` already, so a
/// source with none, or a timeline point nothing has submitted, is
/// EINVAL and the destination is untouched; with it the transfer waits
/// for the submission. Neither field was read and every transfer was
/// deferred.
#[test]
fn transfer_reads_its_padding_and_flags_and_wants_a_submitted_source() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _on = NouveauOn::new();
    let c = Client::open(0);
    let signaled = create(&c, true);
    let fresh = create(&c, false);
    let timeline = create(&c, false);
    let dst = create(&c, false);
    let transfer = |dst_handle: u32, src_handle: u32, src_point: u64, flags: u32, pad: u32| {
        let mut req = DrmSyncobjTransfer {
            src_handle,
            dst_handle,
            src_point,
            dst_point: 0,
            flags,
            pad,
        };
        c.ioctl(DRM_IOCTL_SYNCOBJ_TRANSFER, &mut req)
    };
    let point = |handle: u32| {
        let mut points = [0xdeadu64];
        timeline_array(&c, DRM_IOCTL_SYNCOBJ_QUERY, &[handle], &mut points).expect("QUERY");
        points[0]
    };
    let einval = Err(FsError::InvalidParam);

    assert_eq!(transfer(dst, signaled, 0, 0, 1), einval, "padding");
    for bad in [1u32, 4, 0x8000_0000] {
        assert_eq!(
            transfer(dst, signaled, 0, bad, 0),
            einval,
            "flags {:#x}",
            bad
        );
    }
    assert_eq!(
        transfer(dst, fresh, 0, 0, 0),
        einval,
        "a source with no fence"
    );
    timeline_array(
        &c,
        DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
        &[timeline],
        &mut [3u64],
    )
    .expect("TIMELINE_SIGNAL 3");
    assert_eq!(
        transfer(dst, timeline, 5, 0, 0),
        einval,
        "a point nothing has submitted"
    );
    assert_eq!(point(dst), 0, "none of the refused transfers touched dst");

    assert_eq!(transfer(dst, timeline, 3, 0, 0), Ok(0), "a submitted point");
    assert_eq!(point(dst), 1);
    let dst2 = create(&c, false);
    assert_eq!(
        transfer(dst2, signaled, 0, 0, 0),
        Ok(0),
        "a binary with a fence"
    );
    assert_eq!(point(dst2), 1);

    // WAIT_FOR_SUBMIT: the transfer waits for point 5 to be submitted.
    let dst3 = create(&c, false);
    assert_eq!(
        transfer(dst3, timeline, 5, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, 0),
        Ok(0)
    );
    assert_eq!(point(dst3), 0, "not yet");
    timeline_array(
        &c,
        DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
        &[timeline],
        &mut [5u64],
    )
    .expect("TIMELINE_SIGNAL 5");
    assert_eq!(point(dst3), 1, "and then it lands");

    for h in [signaled, fresh, timeline, dst, dst2, dst3] {
        destroy(&c, h);
    }
}

/// The flag and padding rules of `drm_syncobj.c`, ioctl by ioctl:
/// CREATE takes SIGNALED alone, DESTROY/RESET/SIGNAL want their pad
/// empty, TIMELINE_SIGNAL has no flags, QUERY takes LAST_SUBMITTED alone,
/// WAIT takes ALL, FOR_SUBMIT and DEADLINE, and TIMELINE_WAIT those plus
/// AVAILABLE. Every one of them was accepted here; the binary WAIT even
/// honoured AVAILABLE.
#[test]
fn every_syncobj_ioctl_refuses_the_flags_and_padding_linux_refuses() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _on = NouveauOn::new();
    let c = Client::open(0);
    // CREATE.
    for bad in [2u32, 3, 0x8000_0000] {
        let mut req = DrmSyncobjCreate {
            handle: 0,
            flags: bad,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_CREATE, &mut req),
            Err(FsError::InvalidParam),
            "CREATE flags {:#x}",
            bad
        );
    }
    let s = create(&c, true);
    let t = create(&c, false);
    // DESTROY with a dirty pad: refused, and the handle survives.
    let mut req = DrmSyncobjDestroy { handle: s, pad: 1 };
    assert_eq!(
        c.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut req),
        Err(FsError::InvalidParam)
    );
    assert_eq!(wait(&c, &[s], 0), Ok(0), "still there, still signaled");
    // RESET / SIGNAL with a dirty pad: refused, and nothing happens.
    for cmd in [DRM_IOCTL_SYNCOBJ_RESET, DRM_IOCTL_SYNCOBJ_SIGNAL] {
        let handles = [s];
        let mut req = DrmSyncobjArray {
            handles: handles.as_ptr() as u64,
            count_handles: 1,
            pad: 7,
        };
        assert_eq!(c.ioctl(cmd, &mut req), Err(FsError::InvalidParam));
    }
    assert_eq!(wait(&c, &[s], 0), Ok(0), "the refused RESET reset nothing");
    // TIMELINE_SIGNAL / QUERY flags.
    let handles = [t];
    let mut points = [5u64];
    let mut req = DrmSyncobjTimelineArray {
        handles: handles.as_ptr() as u64,
        points: points.as_mut_ptr() as u64,
        count_handles: 1,
        flags: 1,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &mut req),
        Err(FsError::InvalidParam),
        "TIMELINE_SIGNAL has no flags"
    );
    for bad in [2u32, 3, 0x100] {
        req.flags = bad;
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_QUERY, &mut req),
            Err(FsError::InvalidParam),
            "QUERY flags {:#x}",
            bad
        );
    }
    req.flags = DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED;
    assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_QUERY, &mut req), Ok(0));
    assert_eq!(
        points[0], 0,
        "never signaled: the refused signal did not land"
    );
    // WAIT: AVAILABLE is the timeline form's; DEADLINE is a hint on both.
    assert_eq!(
        wait(&c, &[s], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE),
        Err(FsError::InvalidParam)
    );
    for bad in [0x10u32, 0x20, 0x8000_0000] {
        assert_eq!(
            wait(&c, &[s], bad),
            Err(FsError::InvalidParam),
            "WAIT {:#x}",
            bad
        );
        assert_eq!(
            timeline_wait(&c, &[s], &[1], bad),
            Err(FsError::InvalidParam),
            "TIMELINE_WAIT {:#x}",
            bad
        );
    }
    assert_eq!(
        wait(&c, &[s], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE),
        Ok(0),
        "DEADLINE is accepted"
    );
    assert_eq!(
        wait(&c, &[s], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT),
        Ok(0),
        "FOR_SUBMIT is accepted"
    );
    assert_eq!(
        timeline_wait(
            &c,
            &[s],
            &[1],
            DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE
        ),
        Ok(0)
    );
    // An empty array still goes through the flag check first.
    assert_eq!(wait(&c, &[], 0x10), Err(FsError::InvalidParam));
    destroy(&c, s);
    destroy(&c, t);
}
