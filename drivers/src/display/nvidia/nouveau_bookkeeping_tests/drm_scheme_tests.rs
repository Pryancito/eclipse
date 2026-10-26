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

/// The live lines of one method of `nvidia.rs`: from its `fn` line to the
/// next method at the same indentation, with comment lines dropped. The
/// comments matter here -- the one that replaced the ladder inside
/// `bringup_step14` names both steps it must not call, so a comment has to
/// be unable to answer the question. Per method rather than a search of the
/// whole file for the same reason three methods below mention the same two
/// calls: only one of them may make them.
fn live_method_lines(name: &str) -> String {
    let src = include_str!("../../nvidia.rs");
    let head = alloc::format!("    fn {}(", name);
    let lines: alloc::vec::Vec<&str> = src.lines().collect();
    let starts: alloc::vec::Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with(&head))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "expected exactly one `fn {}` in nvidia.rs",
        name
    );
    let start = starts[0];
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.starts_with("    fn ") || l.starts_with("    pub fn "))
        .map(|p| start + 1 + p)
        .unwrap_or(lines.len());
    let mut out = String::new();
    for line in &lines[start + 1..end] {
        if line.trim_start().starts_with("//") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(!out.is_empty(), "`fn {}` has no body left to read", name);
    out
}

/// WHERE the console GPU's GR ladder is built, which is the whole lesson of
/// two lost monitors. `bringup_step14` is the chain the deferred task drives
/// with the scanout PARKED by a timeout, and the ladder's cold
/// golden-context load is GR work on the very card that scans out the
/// screen: inside that window it took the display out twice and was
/// reverted twice. It belongs in `build_console_gr_ladder`, which `zCore`
/// calls after it has unpaused the scanout. `auto_bringup_compute` does
/// build the ladder and that is fine -- it returns early for the console
/// GPU, so it only ever runs on a card nobody is watching, which is why
/// this test also checks that early return is still there.
#[test]
fn the_console_gr_ladder_is_built_outside_the_window_with_the_scanout_parked() {
    for method in ["bringup_step14", "deferred_console_bringup"] {
        let body = live_method_lines(method);
        for call in [
            "bringup_step16",
            "bringup_step17",
            "rm_init::step16",
            "rm_init::step17",
        ] {
            assert!(
                !body.contains(call),
                "`{}` calls `{}`, and the scanout is parked in there",
                method,
                call
            );
        }
    }
    let ladder = live_method_lines("build_console_gr_ladder");
    for call in ["bringup_step16", "bringup_step17"] {
        assert!(
            ladder.contains(call),
            "`build_console_gr_ladder` no longer calls `{}`, so nothing builds \
             the console GPU's ladder and every client falls to a context 0 \
             with no VA space",
            call
        );
    }
    assert!(
        live_method_lines("auto_bringup_compute").contains("drives_boot_display"),
        "`auto_bringup_compute` builds the ladder too and must keep skipping \
         the console GPU"
    );
}

/// The same split, run rather than read: `deferred_console_bringup` brings
/// the card up and builds no ladder, and `build_console_gr_ladder` builds
/// one and caches the channel's notifier page. The second call does nothing
/// -- `gr_ready` is the C side's own `g_grAllocDone`, so a ladder that
/// stands is not rebuilt behind a live context 0.
#[test]
fn deferred_console_bringup_leaves_the_ladder_to_build_console_gr_ladder() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let saved_fb = *BOOT_FB_INFO.lock();
    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    assert!(
        gpu.drives_boot_display(),
        "this GPU's BAR1 holds the boot framebuffer"
    );
    {
        let mut f = FAKE_RM.lock();
        f.ladder = true;
        f.ladder_built = false;
        f.chan_built = false;
        f.calls.clear();
    }

    let parked = gpu.deferred_console_bringup();
    assert!(parked.contains("lista (bring-up diferido)"), "{}", parked);
    let during = FAKE_RM.lock().calls.clone();
    assert!(
        !during.iter().any(|c| *c == "step16" || *c == "step17"),
        "the ladder was built with the scanout parked: {:?}",
        during
    );
    assert!(
        !FAKE_RM.lock().ladder_built,
        "and context 0 still has no VA space, by design"
    );

    let after = gpu.build_console_gr_ladder();
    assert!(after.contains("ladder GR construido"), "{}", after);
    let built = FAKE_RM.lock().calls.clone();
    assert!(
        built.contains(&"step16") && built.contains(&"step17"),
        "{:?}",
        built
    );
    assert!(
        built.contains(&"chan_notifier_pa"),
        "the EXEC failure path reads that page: {:?}",
        built
    );
    assert!(
        FAKE_RM.lock().ladder_built,
        "now context 0 has a VA space to bind into"
    );

    FAKE_RM.lock().calls.clear();
    assert_eq!(
        gpu.build_console_gr_ladder(),
        String::new(),
        "a ladder that stands is not rebuilt"
    );
    let again = FAKE_RM.lock().calls.clone();
    assert!(again.is_empty(), "{:?}", again);

    nv::set_chan_notifier_pa(0);
    *BOOT_FB_INFO.lock() = saved_fb;
}

/// Every automatic bring-up path silences the narration, and the manual one
/// does not.
///
/// The `bringup_stepN` methods serve two readers with opposite interests. A
/// person running `cat /proc/gpustepN` wants every RM line. The automatic
/// path at boot calls the same methods and throws the result away: capturing
/// it costs a mutex and a `String` push per RM line, reformatting it costs a
/// second pass over a quarter of a megabyte, and `live_echo` lifts each line
/// to ERROR so a framebuffer console living in the BAR1 of the GPU under
/// bring-up renders it -- all of it on the path the desktop is waiting for.
///
/// This reads the live source, with comment lines dropped, so the paragraph
/// above cannot satisfy it. Both halves are asserted: a guard that spread to
/// the `/proc` readers would silently take away the only narration anyone
/// ever reads.
#[test]
fn the_automatic_bring_up_paths_are_the_quiet_ones() {
    for name in [
        "auto_bringup_compute",
        "ensure_console_gpu_brought_up",
        "build_console_gr_ladder",
    ] {
        let body = live_method_lines(name);
        assert!(
            body.contains("QuietBringup::arm()"),
            "`{}` runs at boot with nobody reading its narration, and producing \
             it delays the card the desktop is waiting for",
            name
        );
    }
    // The counterpart: these exist to be read.
    for name in ["bringup_step14", "bringup_step16", "bringup_step17"] {
        let body = live_method_lines(name);
        assert!(
            !body.contains("QuietBringup"),
            "`{}` is what a `cat /proc/gpustepN` runs: silencing it here would \
             leave the one reader who asked for the narration without it",
            name
        );
    }
}

/// `/proc/gpuready` only says yes once a client could actually use the card.
///
/// The file exists so a session can hold its compositor back instead of
/// spending its two renderer tries on a card that is not there yet. That is
/// only worth anything if "ready" means what a GL client needs: the RM
/// attached AND the GR ladder standing, because a client that lands on
/// context 0 without a ladder gets a channel with no GPFIFO and its first
/// EXEC answers ENODEV -- which is exactly the session that ended up on
/// pixman with a working GPU beside it.
#[test]
fn the_console_gpu_reports_itself_ready_only_once_a_client_could_use_it() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let saved_fb = *BOOT_FB_INFO.lock();
    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    {
        let mut f = FAKE_RM.lock();
        f.ladder = true;
        f.ladder_built = false;
        f.chan_built = false;
        f.calls.clear();
    }

    let (ready, line) = gpu
        .console_gpu_ready()
        .expect("this GPU drives the boot display, so it has an answer");
    assert!(!ready, "attached but with no ladder is NOT ready: {}", line);
    assert!(line.contains("gr=no"), "{}", line);
    let after_asking = FAKE_RM.lock().calls.clone();
    assert!(
        after_asking.is_empty(),
        "asking must have no side effects -- a boot polls this in a loop: {:?}",
        after_asking
    );

    let _ = gpu.build_console_gr_ladder();
    let (ready, line) = gpu.console_gpu_ready().expect("still the console GPU");
    assert!(ready, "with the ladder up a client can bind: {}", line);
    assert!(
        line.contains("rm=yes") && line.contains("gr=yes"),
        "{}",
        line
    );

    // And a card that does NOT scan out the screen says nothing at all. The
    // secondary is state-loaded at boot by `auto_bringup_compute`, long before
    // userspace; it is not what a session waits for, and answering for it
    // would hold the compositor back on a card nobody asked about.
    set_boot_fb_info(0xffff_0000_0000, 1920, 1080, 1920 * 4);
    assert!(
        !gpu.drives_boot_display(),
        "the boot framebuffer is now somewhere else"
    );
    assert!(
        gpu.console_gpu_ready().is_none(),
        "only the console GPU has an answer to give"
    );

    nv::set_chan_notifier_pa(0);
    *BOOT_FB_INFO.lock() = saved_fb;
}

/// `GEM_NEW` allocates under the asking process's own context.
///
/// This is the whole point of the batch, and it reverts to a one-word edit:
/// `let owner_ctx = 0;` compiles, every other test here still passes (the
/// lifetime stays consistent -- consistently wrong), and `nvidia-smi` goes
/// back to billing every buffer to nobody. The only thing that notices is a
/// hardware boot, so it is pinned here.
#[test]
fn gem_new_allocates_under_the_context_of_the_process_that_asked() {
    let src = include_str!("../../nvidia.rs");
    let at = src
        .find("rm_init::gem_alloc(")
        .expect("GEM_NEW must still allocate through the RM");
    let before = &src[at.saturating_sub(1200)..at];
    assert!(
        before.contains("let owner_ctx = self.ctx_idx_for_pid(owner_pid);"),
        "the owning context must come from the pid that issued GEM_NEW; a constant \
         here puts every process's buffers back on the shared ladder client, where \
         nvidia-smi counts them for nobody"
    );
    let call = &src[at..at + src[at..].find(';').unwrap()];
    assert!(
        call.contains("owner_ctx"),
        "...and it must be the value passed to the allocation: {:?}",
        call
    );
}

/// A GEM object's owning context is carried with it, never looked up again.
///
/// `h_memory` only exists in the client of the context that allocated it, so
/// every later call has to name that same context. Re-deriving it from
/// `owner_pid` reads the pid table as it is THEN -- and the two paths that
/// free an object (process exit, and the deferred free that waits for the
/// rings to drain) both run while, or after, that process's context is torn
/// down. The lookup would answer 0, the free would go to the shared ladder
/// client, the handle would not be there, and the VRAM would never come back.
/// Nothing reports that: the RM returns a status nobody is in a position to
/// act on, and the leak only shows up as a card that fills up.
#[test]
fn a_gem_object_is_freed_through_the_client_that_allocated_it() {
    let src = include_str!("../../nvidia.rs");
    let live: alloc::string::String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<alloc::vec::Vec<&str>>()
        .join("\n");
    let frees: alloc::vec::Vec<&str> = live
        .match_indices("gem_free(")
        .map(|(i, _)| {
            let rest = &live[i..];
            &rest[..rest.find(')').map(|p| p + 1).unwrap_or(rest.len())]
        })
        .collect();
    assert!(
        !frees.is_empty(),
        "nvidia.rs must still free the objects it allocates"
    );
    for call in &frees {
        assert!(
            call.contains("owner_ctx"),
            "`{}` must free through the object's own `owner_ctx`",
            call
        );
        assert!(
            !call.contains("ctx_idx_for_pid"),
            "`{}` re-derives the context from the pid; by the time a free runs \
             that table can already have forgotten the process",
            call
        );
    }
}

/// The deferred free outlives the process, so it has to keep the context.
#[test]
fn the_deferred_free_keeps_the_owning_context() {
    let queue = include_str!("../gem_free_queue.rs");
    let start = queue
        .find("struct DeferredGemFree {")
        .expect("the deferred free record must still exist");
    let decl = &queue[start..start + queue[start..].find('}').unwrap()];
    assert!(
        decl.contains("owner_ctx"),
        "a free that runs after the process is gone cannot look its context up; \
         it has to have been written down when the object was still alive"
    );
    // ...and written down from the OBJECT. A field that is declared and then
    // filled with a constant is the same leak with a reassuring name on it.
    let src = include_str!("../../nvidia.rs");
    let built = src
        .find("DeferredGemFree {\n                handle,")
        .expect("the deferred free must still be built from a GEM object");
    let ctor = &src[built..built + src[built..].find("});").unwrap()];
    assert!(
        ctor.contains("owner_ctx: obj.owner_ctx"),
        "the deferred free must take the context from the object being freed, \
         not fill the field with a constant: {:?}",
        ctor
    );
}

/// The console GPU's GSP boot runs with the card handed over to its firmware.
///
/// This is the fix for the wedge, and it is the kind that quietly rots: the
/// hand-over is three calls around one `gsp_boot_run`, and an edit that moves
/// the boot out of the window, or re-inlines the PCI/VGA restore the way it
/// used to be, costs nothing visible here and the monitor on hardware. The
/// 2-of-3 empirical bracket underneath hides it further, because the boot
/// still usually works.
#[test]
fn the_console_gsp_boot_happens_inside_the_hand_over_window() {
    let body = live_method_lines("bringup_step14");
    let enter = body
        .find("BootWindow::enter(")
        .expect("the console GSP boot must take the card away from the display first");
    let boot = body
        .find("gsp_boot_run(")
        .expect("`bringup_step14` must still boot the GSP");
    let leave = body
        .find(".leave(")
        .expect("the window must be given back, or the console never comes home");
    assert!(
        enter < boot && boot < leave,
        "the GSP boot must run INSIDE the hand-over window (enter {} / boot {} / leave {})",
        enter,
        boot,
        leave
    );
    // The restore used to be open-coded after the boot. If it comes back here,
    // the early-return and unwind paths lose it again.
    assert!(
        !body.contains("PCI I/O decode restored"),
        "giving the card back belongs to `BootWindow`, which also does it on \
         the paths that never reach the end of this function"
    );
}

/// Taking the display away is what makes the hand-over a hand-over; the rest
/// of the window (VGA decode, I/O decode) was already there while the monitor
/// kept being scanned out, and that combination failed.
#[test]
fn the_hand_over_actually_takes_the_display_away() {
    let src = include_str!("../boot_window.rs");
    let live: alloc::string::String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("//!"))
        .collect::<alloc::vec::Vec<&str>>()
        .join("\n");
    assert!(
        live.contains("pdisp_kill_arm()"),
        "without PDISP going into reset the display engine keeps fetching BAR1 \
         through the SEC2 window, which is the wedge this window exists to avoid"
    );
    assert!(
        live.contains("pdisp_kill_disarm()"),
        "an arm that outlives its boot fires on somebody else's register read"
    );
    assert!(
        live.contains("display_off_for_gsp()"),
        "`nvidia.nodispoff` must still be able to restore the old behaviour"
    );
}

/// Handing the display over is an opt-in, and stays one until a boot shows
/// the monitor coming back.
///
/// It shipped on by default for exactly one hardware boot: "al arrancar el
/// entorno grafico el monitor pierde senal y no se recupera". Part of that was
/// a restore that got cancelled instead of run (now fixed in
/// `pdisp_kill_disarm`), but the other part is not fixable from here --
/// `pdisp_restore` writes `NV_PMC_ENABLE` back and nothing restores the
/// scanout state the VBIOS programmed, which an engine-level reset wipes.
/// Linux does not have the problem because it never resets PDISP; it unbinds
/// the firmware framebuffer and does a modeset of its own.
///
/// Flipping this default back to `true` costs the screen for the whole session
/// on a box where it does not come back, and there is no console left to read
/// the log on. That is why it is pinned rather than left to a comment.
#[test]
fn the_display_is_only_handed_over_when_someone_asks() {
    // Process-wide switch, and `cargo test` runs these in parallel: without
    // the lock this asserts on a default another test is mid-way through
    // flipping, which is how a suite fails one run in three.
    let _s = crate::test_globals::LOCK.lock();
    assert!(
        !crate::display::nouveau_uapi::display_off_for_gsp(),
        "handing the console GPU's display over is OFF by default: the one boot that ran          with it on never got its picture back. `nvidia.dispoff` is the opt-IN"
    );
    crate::display::nouveau_uapi::set_display_off_for_gsp(true);
    assert!(
        crate::display::nouveau_uapi::display_off_for_gsp(),
        "`nvidia.dispoff` must still reach the boot path"
    );
    crate::display::nouveau_uapi::set_display_off_for_gsp(false);
}

/// The GSP boot no longer refuses the card that scans out.
///
/// That refusal -- `SKIPPED on console GPU` -- WAS the split: one card got the
/// chain and the other got a sentence. Moebius asked for it gone («nada de
/// compute y console, va a ser como en linux»), and it reverts to one `return`
/// that compiles, passes every other test here, and quietly restores two paths.
#[test]
fn the_gsp_boot_does_not_refuse_the_card_that_scans_out() {
    let body = live_method_lines("bringup_step6");
    assert!(
        !body.contains("SKIPPED on console GPU"),
        "`bringup_step6` must not refuse a card by role any more: {}",
        body
    );
    let mark = body.find("mark_primary_for_rm").expect(
        "the card that scans out must still declare itself PRIMARY to the RM, where Linux does",
    );
    let boot = body
        .find("gsp_boot_run(")
        .expect("`bringup_step6` must still boot the GSP");
    assert!(
        mark < boot,
        "the primary mark goes BEFORE the boot -- after it, GSP-RM has already \
         decided it reserves no console region (mark {} / boot {})",
        mark,
        boot
    );
    assert!(
        body.contains("BootWindow::enter("),
        "the card with a live raster still gets the hand-over window around its boot: {}",
        body
    );
}

/// Only ONE question about role survives in the automatic bring-up, and it is
/// policy, not a second chain.
///
/// The failure this pins is a re-split: an edit that puts the scanout card
/// back on a branch of its own reads as a small change and gives Eclipse two
/// bring-ups again.
#[test]
fn the_automatic_bring_up_asks_about_the_card_once_and_only_about_policy() {
    let body = live_method_lines("auto_bringup_compute");
    let asks: alloc::vec::Vec<&str> = body
        .lines()
        .filter(|l| l.contains("drives_boot_display()"))
        .collect();
    assert_eq!(
        asks.len(),
        1,
        "exactly one question about the card's role belongs here, the policy one; \
         found {:?}",
        asks
    );
    assert!(
        asks[0].contains("scanout_gpu_bringup()"),
        "...and that question must be the policy hatch, not a branch into a \
         different chain: {:?}",
        asks[0]
    );
}

/// Bringing the scanout card up is an opt-in while its GSP boot still wedges.
///
/// A wedge at boot is not a degraded desktop, it is a machine that does not
/// come back: no session, no console, nothing to read a log on. The default
/// flips when a boot survives the SEC2 store, and this test is the reminder
/// that flipping it is a decision, not a typo.
#[test]
fn the_scanout_card_is_brought_up_only_when_asked() {
    let _s = crate::test_globals::LOCK.lock();
    assert!(
        !crate::display::nouveau_uapi::scanout_gpu_bringup(),
        "default OFF while the GSP boot of a live-raster card wedges at STARTCPU"
    );
    crate::display::nouveau_uapi::set_scanout_gpu_bringup(true);
    assert!(
        crate::display::nouveau_uapi::scanout_gpu_bringup(),
        "`nvidia.scanout_gpu` must reach the bring-up path"
    );
    crate::display::nouveau_uapi::set_scanout_gpu_bringup(false);
}
