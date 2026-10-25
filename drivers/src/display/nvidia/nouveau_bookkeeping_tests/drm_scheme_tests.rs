//! The `DrmScheme` arms the desktop reaches WITHOUT a nouveau ioctl
//! (`create_fb`, the PRIME import, the cursor, the synthetic resources,
//! the legacy `0x10DE` ioctls and the pid-less `ioctl` entry), the
//! per-architecture tables behind GETPARAM and NVIF, and the context
//! registry's fallbacks. None of it needs hardware: a GPU the RM never
//! attached answers every one from bookkeeping. A child of
//! `nouveau_bookkeeping_tests` so it has the same harness; a file of its
//! own because the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::utils::host_hooks::take_dmesg;

/// `create_fb` (ADDFB) takes a nouveau GEM object or a PRIME-imported
/// handle as the backing, caches what the scanout paths read from it, and
/// refuses a framebuffer the backing cannot hold. `import_buffer` keeps one
/// record per handle (a re-import replaces it), and `free_buffer` takes the
/// record AND every framebuffer built on it, unlatching the CRTC and the
/// plane -- an fb left behind would scan out whatever the allocator hands
/// the next caller.
#[test]
fn create_fb_takes_a_gem_object_or_an_imported_handle_and_refuses_what_does_not_fit() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    let h = object(&gpu, A, 65536, Some(0x2000_0000));
    let fb = |handle: u32, w: u32, hgt: u32, pitch: u32| {
        DrmScheme::create_fb(&gpu, handle, w, hgt, pitch)
    };
    assert_eq!(fb(h, 0, 64, 256), None, "no width");
    assert_eq!(fb(h, 64, 0, 256), None, "no height");
    assert_eq!(fb(h, 64, 64, 0), None, "no pitch");
    assert_eq!(
        fb(999_999, 64, 64, 256),
        None,
        "neither a GEM object nor an import"
    );
    assert_eq!(
        fb(h, 64, 300, 256),
        None,
        "76800 bytes do not fit a 65536-byte object"
    );
    let fb1 = fb(h, 64, 64, 256).expect("16384 bytes fit");
    let whole = fb(h, 256, 256, 256).expect("exactly the object's 65536 bytes fit");
    {
        let fbs = gpu.kms_framebuffers.lock();
        let f = fbs.iter().find(|f| f.id == fb1).expect("recorded");
        assert_eq!(
            (f.handle_id, f.width, f.height, f.pitch, f.phys_addr, f.size),
            (h, 64, 64, 256, 0x2000_0000, 16384)
        );
        assert_eq!(
            f.h_memory,
            0xcafe_0000 | (h & 0xffff),
            "the RM handle the ISO ctxdma needs"
        );
        assert_eq!(f.vram_offset, None, "a GART object");
    }

    // A dumb/PRIME handle this driver never allocated.
    let import = |size: usize, pa: u64| {
        DrmScheme::import_buffer(
            &gpu,
            GemHandle {
                id: 4242,
                size,
                phys_addr: pa,
            },
        )
    };
    assert!(import(8192, 0x3000_0000));
    assert_eq!(fb(4242, 64, 64, 256), None, "16384 bytes do not fit 8192");
    let fb2 = fb(4242, 32, 32, 128).expect("4096 bytes fit");
    assert!(
        fb(4242, 32, 64, 128).is_some(),
        "exactly the import's 8192 bytes fit"
    );
    let last = gpu.kms_framebuffers.lock().pop().unwrap();
    assert_eq!((last.handle_id, last.size), (4242, 8192));
    assert!(
        import(32768, 0x3000_0000),
        "a re-import of the same id replaces the record"
    );
    assert_eq!(gpu.imported_handles.lock().len(), 1);
    let fb3 = fb(4242, 64, 64, 256).expect("and the new size counts");
    {
        let fbs = gpu.kms_framebuffers.lock();
        let f = fbs.iter().find(|f| f.id == fb3).unwrap();
        assert_eq!((f.phys_addr, f.size, f.h_memory), (0x3000_0000, 16384, 0));
    }
    assert!(fb1 < whole && whole < fb2 && fb2 < fb3, "ids rise");
    assert!(gpu.kms_framebuffers.lock().iter().any(|f| f.id == whole));
    gpu.kms_framebuffers.lock().retain(|f| f.id != whole);
    {
        let mut st = gpu.kms_state.lock();
        st.crtc_fb = fb2;
        st.plane_fb = fb3;
    }
    DrmScheme::free_buffer(
        &gpu,
        GemHandle {
            id: 4242,
            size: 32768,
            phys_addr: 0x3000_0000,
        },
    );
    assert!(gpu.imported_handles.lock().is_empty(), "the import went");
    let left: Vec<u32> = gpu.kms_framebuffers.lock().iter().map(|f| f.id).collect();
    assert_eq!(left, [fb1], "both fbs built on it went, the GEM one stays");
    let st = *gpu.kms_state.lock();
    assert_eq!(
        (st.crtc_fb, st.plane_fb),
        (0, 0),
        "nothing scans out a freed buffer"
    );
    // Freeing what was never imported is a no-op, not a panic.
    DrmScheme::free_buffer(
        &gpu,
        GemHandle {
            id: 7,
            size: 1,
            phys_addr: 1,
        },
    );
    assert_eq!(gpu.kms_framebuffers.lock().len(), 1);
    assert!(gpu.nouveau_gem_close(h, A));
}

/// `set_cursor` serves only the MOVE flag (the software cursor is the
/// compositor's); the hardware cursor plane exists only on the GPU driving
/// the boot display, after a 64x64 bound, and `hw_cursor_hide` needs the
/// RM. On the host the RM refuses every cursor call, so `move` is false
/// and `hide` is false even with a device instance.
#[test]
fn the_cursor_arms_answer_from_the_flags_the_bounds_and_the_boot_display() {
    let _g = LOCK.lock();
    let gpu = gpu();
    const MOVE: u32 = 0x02;
    assert!(DrmScheme::set_cursor(&gpu, 2001, 10, 20, 0, MOVE));
    assert!(DrmScheme::set_cursor(&gpu, 2001, 10, 20, 0, MOVE | 0x01));
    assert!(!DrmScheme::set_cursor(&gpu, 2001, 0, 0, 5, 0));
    assert!(!DrmScheme::set_cursor(&gpu, 2001, 0, 0, 5, 0x01 | 0x04));
    let argb = alloc::vec![0u32; 65 * 65];
    for (w, h) in [(0, 64), (64, 0), (65, 64), (64, 65), (65, 65)] {
        assert!(!DrmScheme::hw_cursor_set(&gpu, &argb, w, h), "{}x{}", w, h);
    }
    assert!(
        !DrmScheme::hw_cursor_set(&gpu, &argb, 64, 64),
        "in bounds, but this GPU does not drive the boot display"
    );
    assert!(!DrmScheme::hw_cursor_hide(&gpu), "no RM");
    *gpu.rm_device_instance.lock() = Some(0);
    assert!(
        !DrmScheme::hw_cursor_set(&gpu, &argb, 64, 64),
        "the RM alone does not make it the console GPU"
    );
    assert!(
        !DrmScheme::hw_cursor_hide(&gpu),
        "the host RM has no cursor plane"
    );
    assert!(!DrmScheme::hw_cursor_move(&gpu, 3, 4));
}

/// Before the RM reports the display topology, GETRESOURCES advertises
/// one CRTC (2001) and one synthetic connector (1001), GETCONNECTOR
/// answers for that id alone, and there is no EDID to hand out. The CRTC
/// reports the framebuffer latched on it.
#[test]
fn the_resources_before_the_rm_are_the_one_crtc_and_the_synthetic_connector() {
    let _g = LOCK.lock();
    let gpu = gpu();
    assert_eq!(
        DrmScheme::get_resources(&gpu),
        (Vec::new(), alloc::vec![2001], alloc::vec![1001])
    );
    let c = DrmScheme::get_connector(&gpu, 1001).expect("the synthetic connector");
    assert_eq!(
        (c.id, c.connected, c.mm_width, c.mm_height),
        (1001, true, 0, 0)
    );
    assert_eq!(c.connector_type, 11, "HDMI-A");
    assert!(DrmScheme::get_connector(&gpu, 1002).is_none());
    assert!(DrmScheme::get_connector(&gpu, 7).is_none());
    assert_eq!(
        DrmScheme::get_connector_edid(&gpu, 1001),
        None,
        "no RM: no EDID"
    );
    gpu.kms_state.lock().crtc_fb = 77;
    let crtc = DrmScheme::get_crtc(&gpu, 2001).expect("the one CRTC");
    assert_eq!((crtc.id, crtc.fb_id, crtc.x, crtc.y), (2001, 77, 0, 0));
    assert!(DrmScheme::get_crtc(&gpu, 2002).is_none());
}

/// `pitch_pixels`: an explicit override wins; a mode with no size has
/// no padding to infer; an aperture of 16 MiB or more is not the small
/// mapping the inference is for; otherwise the aperture divided by the
/// rows gives the padded pitch, accepted only between the width and the
/// width plus 4096 pixels.
#[test]
fn the_pitch_is_the_override_or_what_a_small_aperture_implies() {
    let _g = LOCK.lock();
    let mut gpu = gpu();
    assert_eq!(gpu.pitch_pixels(), 0, "no mode: the width, which is 0");
    gpu.pitch_override = Some(8192);
    assert_eq!(gpu.pitch_pixels(), 2048, "bytes over four");
    gpu.pitch_override = None;
    gpu.info.width = 640;
    gpu.info.height = 480;
    gpu.info.fb_size = 256 << 20;
    assert_eq!(
        gpu.pitch_pixels(),
        640,
        "a big aperture says nothing about padding"
    );
    gpu.info.width = 1920;
    gpu.info.height = 1080;
    gpu.info.fb_size = 16 << 20;
    assert_eq!(
        gpu.pitch_pixels(),
        1920,
        "16 MiB would infer 3883, but is already a whole aperture, not a small mapping"
    );
    gpu.info.fb_size = (16 << 20) - 1;
    assert_eq!(
        gpu.pitch_pixels(),
        3883,
        "one byte less and it is a mapping to infer from"
    );
    gpu.info.width = 640;
    gpu.info.height = 480;
    gpu.info.fb_size = 640 * 480 * 4;
    assert_eq!(gpu.pitch_pixels(), 640, "exactly the visible pixels");
    gpu.info.fb_size = 768 * 480 * 4;
    assert_eq!(gpu.pitch_pixels(), 768, "128 pixels of padding per row");
    gpu.info.fb_size = (640 + 4096) * 480 * 4;
    assert_eq!(gpu.pitch_pixels(), 640 + 4096, "the most padding accepted");
    gpu.info.fb_size = (640 + 4097) * 480 * 4;
    assert_eq!(gpu.pitch_pixels(), 640, "more than that is not padding");
    gpu.info.fb_size = 100;
    assert_eq!(
        gpu.pitch_pixels(),
        640,
        "smaller than the visible pixels: the width"
    );
    gpu.info.height = 0;
    gpu.info.fb_size = 768 * 480 * 4;
    assert_eq!(gpu.pitch_pixels(), 640, "no rows to divide by");
}

/// The legacy `0x10DE` ioctls this driver had before nouveau's: the
/// temperature (none without a readable BAR0), the VRAM size, and the two
/// rectangle ops, which check their argument block before reading it.
/// Everything else goes to the nouveau dispatcher through the pid-aware
/// entry, which profiles the driver-private range and records the last
/// error for `/proc/gpudbg`; the pid-less `ioctl` entry is the same path
/// as pid 0.
#[test]
fn the_legacy_ioctls_answer_themselves_and_the_rest_reach_nouveau_with_a_record() {
    let _g = LOCK.lock();
    let gpu = gpu();
    assert_eq!(
        DrmScheme::ioctl_owned(&gpu, 0x10DE0001, 0, A),
        Err(22),
        "BAR0 reads as zero"
    );
    assert_eq!(DrmScheme::ioctl_owned(&gpu, 0x10DE0002, 0, A), Ok(8192));
    assert_eq!(
        DrmScheme::ioctl_owned(&gpu, 0x10DE0010, 0, A),
        Err(nv::EFAULT)
    );
    assert_eq!(
        DrmScheme::ioctl_owned(&gpu, 0x10DE0011, 0, A),
        Err(nv::EFAULT)
    );
    let req = wr::<nv::DrmNouveauGetparam>(nv::NR_GETPARAM);
    let mut r = nv::DrmNouveauGetparam {
        param: nv::NOUVEAU_GETPARAM_PCI_VENDOR,
        value: 0,
    };
    assert_eq!(
        DrmScheme::ioctl_owned(&gpu, req, &mut r as *mut _ as usize, A),
        Ok(0)
    );
    assert_eq!(r.value, 0x10de);
    let before = nv::format_last_exec();
    let mut bad = nv::DrmNouveauGetparam {
        param: 99,
        value: 0,
    };
    assert_eq!(
        DrmScheme::ioctl(&gpu, req, &mut bad as *mut _ as usize),
        Err(nv::EINVAL)
    );
    let after = nv::format_last_exec();
    assert_ne!(before, after, "the error was recorded");
    let line = alloc::format!(
        "pid=0 ioctl {:#010x} (nr={:#04x} GETPARAM) -> errno={}",
        req,
        nv::NR_GETPARAM,
        nv::EINVAL
    );
    assert!(after.contains(&line), "{}", after);
    assert!(
        !before.contains(&line),
        "a success leaves no record: {}",
        before
    );
    // PTIMER_TIME: a rising nanosecond clock, the host clock times 1000.
    let t0 = getparam(&gpu, nv::NOUVEAU_GETPARAM_PTIMER_TIME).unwrap();
    test_clock::advance(5);
    let t1 = getparam(&gpu, nv::NOUVEAU_GETPARAM_PTIMER_TIME).unwrap();
    assert_eq!(t1 - t0, 5_000);
    assert_eq!(t0 % 1000, 0);
}

/// Every architecture the id table knows answers with its own chip id,
/// chip name, engine classes, full-die topology and VRAM floor. Hopper has
/// no consumer board in the table, so it is set on the GPU by hand.
#[test]
fn every_architecture_answers_with_its_own_chip_classes_topology_and_floor() {
    let _g = LOCK.lock();
    use NvidiaArchitecture::*;
    let rows: [(
        u16,
        NvidiaArchitecture,
        u16,
        &str,
        (i32, i32, i32),
        (u64, u64),
        u64,
    ); 5] = [
        (
            0x1f06,
            Turing,
            0x162,
            "TU1xx",
            nv::CLASSES_TURING,
            (6, 36),
            4096,
        ),
        (
            0x2204,
            Ampere,
            0x172,
            "GA1xx",
            nv::CLASSES_AMPERE,
            (7, 42),
            8192,
        ),
        (
            0x2684,
            AdaLovelace,
            0x192,
            "AD1xx",
            nv::CLASSES_ADA,
            (12, 72),
            8192,
        ),
        (
            0x1fff,
            Hopper,
            0x180,
            "GH1xx",
            nv::CLASSES_HOPPER,
            (8, 72),
            16384,
        ),
        (
            0x2B85,
            Blackwell,
            0x1b2,
            "GB2xx",
            nv::CLASSES_BLACKWELL,
            (12, 96),
            12288,
        ),
    ];
    for (id, arch, chipset, chip, classes, (gpc, tpc), floor) in rows {
        let mut gpu = NvidiaGpu::for_test(id, 0);
        gpu.architecture = arch;
        let why = alloc::format!("{:?}", arch);
        assert_eq!(gpu.nouveau_arch(), arch, "{}", why);
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_CHIPSET_ID),
            Ok(u64::from(chipset)),
            "{}",
            why
        );
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
            Ok(gpc | (tpc << 8)),
            "{}",
            why
        );
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_FB_SIZE),
            Ok(floor * MIB),
            "{}: the VRAM floor of a board the table cannot size",
            why
        );
        assert_eq!(device_new(u64::MAX).send(&gpu, A), Ok(0));
        let mut r = device_info(nv::NV_DEVICE_V0_INFO);
        assert_eq!(r.send(&gpu, A), Ok(0), "{}", why);
        let info: nv::NvDeviceInfoV0 = r.get(HDR + MTHD);
        assert_eq!(info.chipset, chipset, "{}", why);
        assert_eq!(cstr(&info.chip), chip, "{}", why);
        assert_eq!(info.ram_user, floor * MIB, "{}", why);
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
        let mut r = sclass(0, 0xff, 16, 16);
        assert_eq!(r.send(&gpu, A), Ok(0), "{}", why);
        let (count, list) = classes_in(&r);
        assert_eq!(count, 5, "{}", why);
        assert_eq!(
            &list[..5],
            &[0x902d, 0xa140, classes.0, classes.1, classes.2],
            "{}",
            why
        );
    }
}

/// `ensure_ctx_for_pid`: the compositor is context 0 without a lookup only
/// under `nvidia.compositor_ctx0` (by default it is a DRM client like any
/// other and gets a context of its own, which is what lets `nvidia-smi` name
/// it); a client finds no free slot when every index 1..MAX_CTX is
/// registered, or reserved by a teardown still running, and falls back to
/// software without an RM call; a second thread of a pid whose build is in
/// flight waits for it -- and takes the built context, or the fallback when
/// the build failed (reservation gone) or three seconds passed.
#[test]
fn a_client_without_a_free_context_slot_or_with_a_build_in_flight_falls_back() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    let before = FAKE_RM.lock().calls.len();
    nv::set_compositor_ctx0(true);
    assert_eq!(
        gpu.ensure_ctx_for_pid(COMP, true),
        (0, 0, 0),
        "the compositor, pinned to ctx 0 by the hatch"
    );
    nv::set_compositor_ctx0(false);
    assert!(
        rm_calls_since(before).is_empty(),
        "and without a lookup either way"
    );
    {
        let mut map = gpu.nouveau_pid_ctx.lock();
        for i in 1..nv::MAX_CTX {
            map.push((70_000 + u64::from(i), i, 0, 0, true));
        }
    }
    assert_eq!(
        gpu.ensure_ctx_for_pid(A, false),
        (0, 0, 0),
        "every slot taken"
    );
    gpu.nouveau_pid_ctx.lock().clear();
    *gpu.nouveau_ctx_teardown.lock() = (1..nv::MAX_CTX).collect();
    assert_eq!(
        gpu.ensure_ctx_for_pid(A, false),
        (0, 0, 0),
        "every slot still the RM's until its teardown runs"
    );
    gpu.nouveau_ctx_teardown.lock().clear();
    assert!(
        rm_calls_since(before).is_empty(),
        "no slot, no ctx_alloc: {:?}",
        rm_calls_since(before)
    );

    // A's first thread reserved slot 5 and is in the RM; the hook is the
    // GPU spin of the waiting thread.
    let publish = |ready: Option<(u32, u32)>| {
        gpu.nouveau_pid_ctx.lock().push((A, 5, 0, 0, false));
        // The hook outlives no test (the GPU is on this stack frame), so
        // the registry's address travels into it as a plain word.
        let map = &gpu.nouveau_pid_ctx as *const Mutex<Vec<(u64, u32, u32, u32, bool)>> as usize;
        *GPU_SPIN_HOOK.lock() = Some(alloc::boxed::Box::new(move || {
            let mut m = unsafe { &*(map as *const Mutex<Vec<(u64, u32, u32, u32, bool)>>) }.lock();
            match ready {
                Some((vas, notifier)) => {
                    let t = m.iter_mut().find(|t| t.0 == A).unwrap();
                    t.2 = vas;
                    t.3 = notifier;
                    t.4 = true;
                }
                None => m.retain(|t| t.0 != A),
            }
        }));
        let got = gpu.ensure_ctx_for_pid(A, false);
        *GPU_SPIN_HOOK.lock() = None;
        gpu.nouveau_pid_ctx.lock().retain(|t| t.0 != A);
        got
    };
    assert_eq!(
        publish(Some((0xa5, 0x5a))),
        (5, 0xa5, 0x5a),
        "the sibling's build, once READY"
    );
    assert_eq!(publish(None), (0, 0, 0), "the sibling's build failed");
    // Nobody ever publishes: three seconds of virtual time, then software.
    gpu.nouveau_pid_ctx.lock().push((A, 5, 0, 0, false));
    *GPU_SPIN_HOOK.lock() = Some(alloc::boxed::Box::new(|| test_clock::advance(1_000_000)));
    take_dmesg();
    let t0 = test_clock::now();
    assert_eq!(gpu.ensure_ctx_for_pid(A, false), (0, 0, 0));
    *GPU_SPIN_HOOK.lock() = None;
    let waited = test_clock::now() - t0;
    assert!(
        (3_000_000..=5_000_000).contains(&waited),
        "three seconds, give or take a spin: {} us",
        waited
    );
    let dmesg = take_dmesg();
    assert!(
        dmesg
            .iter()
            .any(|(_, l)| l.contains("waited >3s for its own in-flight ctx build")),
        "{:?}",
        dmesg
    );
    assert_eq!(
        gpu.nouveau_pid_ctx.lock().len(),
        1,
        "the reservation is the builder's to remove"
    );
    gpu.nouveau_pid_ctx.lock().clear();
    assert!(rm_calls_since(before).is_empty());
}

/// A channel built on the RM claims RM backing for every engine object
/// made on it; if the device instance is gone by then, the NEW is ENODEV
/// (not a bookkeeping-only accept that would MMU-fault the channel at the
/// first method), and the owner's exit, which has ctx 0 to reset, says
/// once that there is no RM to reset it with.
#[test]
fn an_rm_backed_channel_without_its_device_refuses_engine_objects_and_says_so_at_exit() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    assert_eq!(channel_alloc(&gpu, COMP).unwrap().channel, 0);
    assert_eq!(ctx0_owner(&gpu), COMP);
    assert_eq!(
        subchan_new(0, 0xc597, 0x1000).send(&gpu, COMP),
        Ok(0),
        "with the RM"
    );
    *gpu.rm_device_instance.lock() = None;
    assert_eq!(
        subchan_new(0, 0xc5c0, 0x1001).send(&gpu, COMP),
        Err(nv::ENODEV),
        "the channel claims RM backing the device cannot give"
    );
    take_dmesg();
    gpu.nouveau_release_process(COMP);
    let dmesg = take_dmesg();
    assert!(
        dmesg.iter().any(|(_, l)| {
            l.contains("ctx0 reset: process exit pid=66000 but no RM device instance is attached")
        }),
        "{:?}",
        dmesg
    );
    assert!(gpu.nouveau_channels.lock().is_empty(), "the channels went");
    assert_eq!(
        ctx0_owner(&gpu),
        0,
        "and the seat comes free anyway: there is nothing to tear down, but a \
         claim left on a dead pid makes the respawn a GL client for the rest of \
         its life"
    );
}

/// `CPU_PREP` on a buffer that was never PRIME-registered (no physical
/// address: nothing can have imported a fence onto it) is done at once,
/// and one whose imported fences have all landed is done at once too.
#[test]
fn cpu_prep_is_done_at_once_without_a_registration_or_with_landed_fences() {
    use crate::scheme::resv;
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    let bare = object(&gpu, A, 4096, None);
    assert_eq!(cpu_prep_nowait(&gpu, bare, A), Ok(0));
    assert_eq!(
        cpu_prep_flags(&gpu, bare, nv::NOUVEAU_GEM_CPU_PREP_WRITE, A),
        Ok(0)
    );
    let shared = object(&gpu, A, 4096, Some(0x4000_0000));
    let landed = syncobj::create(true);
    assert!(resv::add_fence(0x4000_0000, landed, 1, true));
    assert_eq!(
        cpu_prep_nowait(&gpu, shared, A),
        Ok(0),
        "a signaled fence is no wait"
    );
    assert_eq!(cpu_prep_flags(&gpu, shared, 0, A), Ok(0));
    syncobj::destroy(landed);
    assert!(gpu.nouveau_gem_close(bare, A));
    assert!(gpu.nouveau_gem_close(shared, A));
}

/// `/proc/gpudbg`'s surface-flip line names the ladder's state in a word,
/// and the PCI driver matches NVIDIA's vendor id and nothing else.
#[test]
fn the_surfaceflip_line_names_its_state_and_the_pci_driver_matches_nvidia() {
    let _g = LOCK.lock();
    let was = SURFACEFLIP_STATE.load(Ordering::Relaxed);
    for (state, word) in [(0u8, "untried"), (1, "ready"), (2, "failed"), (9, "failed")] {
        SURFACEFLIP_STATE.store(state, Ordering::Relaxed);
        let line = surfaceflip_stats_line();
        assert!(
            line.starts_with(&alloc::format!(
                "[gpudbg]  surfaceflip: state={} flips=",
                word
            )),
            "{}",
            line
        );
    }
    SURFACEFLIP_STATE.store(was, Ordering::Relaxed);
    let drv = NvidiaGpuDriverPci;
    assert_eq!(PciDriver::name(&drv), "Nvidia GPU");
    assert!(PciDriver::matched(&drv, 0x10DE, 0x1f06));
    assert!(PciDriver::matched(&drv, 0x10DE, 0));
    assert!(!PciDriver::matched(&drv, 0x8086, 0x1f06));
    assert_eq!(Scheme::name(&gpu()), "nvidia-test");
}

/// The one plane this driver lists is 3001, the primary of CRTC 2001,
/// and `GETPLANE` on it reports the framebuffer `set_plane` last put there
/// (`plane_fb`); any other id is no plane. `set_plane` with no framebuffer
/// clears the plane and succeeds; with one it presents it, which a
/// framebuffer this driver never made, or one without a CPU-visible
/// address (a dumb buffer before its first map), refuses -- the plane
/// keeps what it showed.
#[test]
fn the_primary_plane_is_3001_and_set_plane_presents_or_clears_it() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    assert_eq!(DrmScheme::get_planes(&gpu), alloc::vec![3001]);
    assert!(DrmScheme::get_plane(&gpu, 3002).is_none(), "not a plane");
    gpu.kms_state.lock().plane_fb = 77;
    let plane = DrmScheme::get_plane(&gpu, 3001).expect("the primary");
    assert_eq!(
        (
            plane.id,
            plane.crtc_id,
            plane.fb_id,
            plane.possible_crtcs,
            plane.plane_type
        ),
        (3001, 2001, 77, 1, 1),
        "the primary of CRTC 2001, showing what set_plane last put there"
    );
    let set = |plane_id: u32, fb_id: u32| {
        DrmScheme::set_plane(
            &gpu,
            plane_id,
            2001,
            fb_id,
            0,
            0,
            64,
            64,
            0,
            0,
            64 << 16,
            64 << 16,
        )
    };
    assert!(!set(3002, 0), "not a plane");
    assert_eq!(gpu.kms_state.lock().plane_fb, 77, "nothing touched");
    let h = object(&gpu, A, 16384, None);
    let fb = framebuffer(&gpu, h);
    assert!(!set(3001, fb + 100), "a framebuffer this driver never made");
    assert!(
        !set(3001, fb),
        "a framebuffer without a CPU-visible address cannot be presented"
    );
    assert_eq!(
        gpu.kms_state.lock().plane_fb,
        77,
        "the plane keeps what it showed"
    );
    assert!(set(3001, 0), "no framebuffer: the plane is cleared");
    assert_eq!(gpu.kms_state.lock().plane_fb, 0);
    assert_eq!(
        DrmScheme::get_plane(&gpu, 3001).map(|p| p.fb_id),
        Some(0),
        "and GETPLANE says so"
    );
    gpu.nouveau_release_process(A);
}

/// `nvidia.hwflip` on: `page_flip` copies the framebuffer into the GOP
/// scanout with the copy engine, pitched (a flat copy painted the
/// diagonal-snow desktop). The copy wants a sysmem source with a
/// geometry the engine can do: a framebuffer this driver never made, one
/// without a physical address, a VRAM one (its address is not a CE sysmem
/// PA), or one whose row does not fit its own pitch is refused without
/// touching the scanout; and when the engine itself cannot run (no RM
/// attached) the flip falls back to software and says so once. Nothing
/// moves the CRTC's framebuffer.
#[test]
fn page_flip_with_hwflip_refuses_what_the_copy_engine_cannot_copy() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    struct HwflipOn;
    impl Drop for HwflipOn {
        fn drop(&mut self) {
            nv::set_hwflip_enabled(false);
        }
    }
    nv::set_hwflip_enabled(true);
    let _on = HwflipOn;
    let mut gpu = gpu();
    // The scanout the copy lands in: a 64x64 panel with 256-byte rows
    // (`for_test` has no GOP, so no geometry of its own).
    gpu.info.width = 64;
    gpu.info.height = 64;
    gpu.info.pitch = 256;
    let h = object(&gpu, A, 65536, Some(0x2000_0000));
    let mut fbs = alloc::vec::Vec::new();
    let mut fb = |phys_addr: u64, pitch: u32, vram_offset: Option<u64>| {
        let id = gpu.next_kms_fb_id.fetch_add(1, Ordering::Relaxed);
        gpu.kms_framebuffers.lock().push(NvidiaKmsFramebuffer {
            id,
            handle_id: h,
            width: 64,
            height: 64,
            pitch,
            phys_addr,
            size: 65536,
            h_memory: 0,
            vram_offset,
        });
        fbs.push(id);
        id
    };
    let unknown = fb(0, 256, None) + 1000;
    let no_address = fb(0, 256, None);
    let vram = fb(0x2000_0000, 256, Some(0x100_0000));
    let narrow = fb(0x2000_0000, 128, None);
    let good = fb(0x2000_0000, 256, None);
    let crtc_fb = gpu.kms_state.lock().crtc_fb;
    let said = |what: &str| take_dmesg().iter().any(|(_, l)| l.contains(what));
    let _ = take_dmesg();
    assert!(
        !gpu.page_flip(unknown),
        "a framebuffer this driver never made"
    );
    assert!(!gpu.page_flip(no_address), "no physical address");
    assert!(!said("hwflip"), "refused without a word: nothing to copy");
    assert!(
        !gpu.page_flip(vram),
        "a VRAM framebuffer is not a CE sysmem source"
    );
    assert!(said("hwflip: refusing VRAM GEM fb="), "and says why, once");
    assert!(
        !gpu.page_flip(narrow),
        "a 256-byte row does not fit a 128-byte pitch"
    );
    assert!(
        !said("hwflip"),
        "a geometry the engine cannot do is refused in silence"
    );
    assert!(
        !gpu.page_flip(good),
        "the engine cannot run without the RM: software fallback"
    );
    assert!(
        said("hwflip: CE 2D pitched failed for fb="),
        "the fallback is said, once"
    );
    assert_eq!(
        gpu.kms_state.lock().crtc_fb,
        crtc_fb,
        "the scanout did not move"
    );
    for id in fbs {
        gpu.kms_framebuffers.lock().retain(|f| f.id != id);
    }
    gpu.nouveau_release_process(A);
}
