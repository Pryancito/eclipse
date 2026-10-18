//! `nouveau_abi16_ioctl_channel_alloc` and its kin: what the channel
//! arms read from the client and what they write back. A child of
//! `nouveau_bookkeeping_tests` so it has the same harness (`LOCK`,
//! `gpu_rm`, `call`, `wr`, `channels_of`); a file of its own because the
//! parent is at its size ceiling (`xtask tamano`).

use super::*;

/// `nouveau_abi16_ioctl_channel_alloc` on Kepler and later: with
/// `fb_ctxdma_handle == ~0`, `tt_ctxdma_handle` names the engine (GR or
/// CE here; VP/PPP/BSP have no runlist on a Turing, EINVAL; anything
/// else ENOSYS) and both words come back as 0; otherwise they are legacy
/// handles and only `tt == ~0` is refused (EINVAL). This driver never
/// read either word.
#[test]
fn channel_alloc_reads_the_engine_the_client_names_like_kepler_and_later() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let alloc = |pid: u64, fb: u32, tt: u32| {
        let mut r = nv::DrmNouveauChannelAlloc {
            fb_ctxdma_handle: fb,
            tt_ctxdma_handle: tt,
            channel: -1,
            pushbuf_domains: 0,
            notifier_handle: 0xffff_ffff,
            subchan: [nv::DrmNouveauChannelAllocSubchan {
                handle: 0,
                grclass: 0,
            }; 8],
            nr_subchan: 7,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauChannelAlloc>(nv::NR_CHANNEL_ALLOC),
            &mut r,
            pid,
        )
        .map(|_| (r.channel, r.fb_ctxdma_handle, r.tt_ctxdma_handle))
    };
    // A name that is no engine, a pair of engines, and the engines a
    // Turing has not got: refused, and no channel came of it.
    for (tt, err) in [
        (0, nv::ENOSYS),
        (
            nv::NOUVEAU_FIFO_ENGINE_GR | nv::NOUVEAU_FIFO_ENGINE_CE,
            nv::ENOSYS,
        ),
        (0x10, nv::ENOSYS),
        (u32::MAX, nv::ENOSYS),
        (nv::NOUVEAU_FIFO_ENGINE_VP, nv::EINVAL),
        (nv::NOUVEAU_FIFO_ENGINE_PPP, nv::EINVAL),
        (nv::NOUVEAU_FIFO_ENGINE_BSP, nv::EINVAL),
    ] {
        assert_eq!(alloc(A, u32::MAX, tt), Err(err), "tt={tt:#x}");
    }
    assert_eq!(alloc(A, 0, u32::MAX), Err(nv::EINVAL), "legacy tt ~0");
    assert_eq!(channels_of(&gpu, A), 0);
    // GR and CE, as NVK and libdrm's nve0 channels ask: the engine words
    // are consumed.
    let (gr, fb, tt) = alloc(A, u32::MAX, nv::NOUVEAU_FIFO_ENGINE_GR).unwrap();
    assert!(gr >= 0);
    assert_eq!((fb, tt), (0, 0));
    let (ce, fb, tt) = alloc(A, u32::MAX, nv::NOUVEAU_FIFO_ENGINE_CE).unwrap();
    assert!(ce >= 0 && ce != gr);
    assert_eq!((fb, tt), (0, 0));
    // Legacy handles (libdrm's nvc0 channels send 0 and 0): untouched.
    assert_eq!(alloc(B, 7, 9).map(|r| (r.1, r.2)), Ok((7, 9)));
    assert_eq!(channels_of(&gpu, A), 2);
    assert_eq!(channels_of(&gpu, B), 1);
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
}

#[test]
fn nvif_reads_the_header_and_the_bodies_as_nouveau_abi16_ioctl_does() {
    let _g = LOCK.lock();
    let gpu = gpu();
    // The header: version 0, route 0x00 or 0xff, before the type.
    let mut r = device_new(u64::MAX);
    r.0[0] = 1;
    assert_eq!(r.send(&gpu, A), Err(nv::EINVAL), "header version 1");
    for route in [0x01u8, 0x7f, 0xfe] {
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_NEW, route, 0, 0, HDR + NEW)
                .put(HDR, new_body(nv::NVIF_CLASS_NV_DEVICE, 0xd0d0))
                .send(&gpu, A),
            Err(nv::EINVAL),
            "route {route:#04x}"
        );
    }
    // NEW: a version-0 body; on route 0 the device under the client.
    let mut r = device_new(u64::MAX);
    r.0[HDR] = 1;
    assert_eq!(r.send(&gpu, A), Err(nv::EINVAL), "NEW body version 1");
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0, 0, 0x77, HDR + NEW)
            .put(HDR, new_body(nv::NVIF_CLASS_NV_DEVICE, 0xd0d0))
            .send(&gpu, A),
        Err(nv::EINVAL),
        "a device under another object"
    );
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0, 0, 0, HDR + NEW)
            .put(HDR, new_body(0xc597, 0xd0d0))
            .send(&gpu, A),
        Err(nv::EINVAL),
        "an engine class on the client's route"
    );
    // On a channel's route the class is an engine object, whatever it
    // says: NV_DEVICE there makes no device object (and no channel is
    // EINVAL, as for any engine object).
    assert_eq!(
        subchan_new(0, nv::NVIF_CLASS_NV_DEVICE, 0xdead).send(&gpu, A),
        Err(nv::EINVAL),
        "no channel"
    );
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(
        subchan_new(0, nv::NVIF_CLASS_NV_DEVICE, 0xdead).send(&gpu, A),
        Ok(0),
        "an engine object of class 0x80 on a discovery channel"
    );
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_MTHD, 0, 0, 0xdead, HDR + MTHD + INFO)
            .put(
                HDR,
                nv::NvifIoctlMthdV0 {
                    version: 0,
                    method: nv::NV_DEVICE_V0_INFO,
                    pad02: [0; 6],
                },
            )
            .send(&gpu, A),
        Err(nv::EINVAL),
        "not a device object"
    );
    // A route that is neither: refused at the header, even where the
    // channel's path would have taken it.
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0x7f, 0, 0, HDR + NEW)
            .put(HDR, new_body(0xc597, 0x1000))
            .send(&gpu, A),
        Err(nv::EINVAL),
        "route 0x7f on a channel that exists"
    );
    // MTHD: on a device object the caller made, route 0, INFO only,
    // a body of exactly the reply's size, a version-0 template.
    assert_eq!(
        device_info(nv::NV_DEVICE_V0_INFO).send(&gpu, A),
        Err(nv::EINVAL),
        "no device object yet"
    );
    assert_eq!(device_new(u64::MAX).send(&gpu, A), Ok(0));
    assert_eq!(device_info(nv::NV_DEVICE_V0_INFO).send(&gpu, A), Ok(0));
    assert_eq!(
        device_info(nv::NV_DEVICE_V0_INFO).send(&gpu, B),
        Err(nv::EINVAL),
        "A's device object is not B's"
    );
    let mut r = device_info(nv::NV_DEVICE_V0_INFO);
    r.0[HDR + MTHD] = 1;
    assert_eq!(r.send(&gpu, A), Err(nv::EINVAL), "reply template version 1");
    let mut r = device_info(nv::NV_DEVICE_V0_INFO);
    r.0.push(0);
    assert_eq!(
        r.send(&gpu, A),
        Err(nv::EINVAL),
        "a byte more than the reply"
    );
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_MTHD, 0xff, 0, 0xd0d0, HDR + MTHD + INFO)
            .put(
                HDR,
                nv::NvifIoctlMthdV0 {
                    version: 0,
                    method: nv::NV_DEVICE_V0_INFO,
                    pad02: [0; 6],
                },
            )
            .send(&gpu, A),
        Err(nv::EINVAL),
        "MTHD on a channel's route"
    );
    // DEL: route 0 and no body; then the device object is gone.
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0xff, 0, 0xd0d0, HDR).send(&gpu, A),
        Err(nv::EINVAL),
        "DEL on a channel's route"
    );
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0, 0, 0xd0d0, HDR + 1).send(&gpu, A),
        Err(nv::EINVAL),
        "DEL with a body"
    );
    assert_eq!(device_info(nv::NV_DEVICE_V0_INFO).send(&gpu, A), Ok(0));
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0, 0, 0xd0d0, HDR).send(&gpu, B),
        Ok(0),
        "B's DEL of A's cookie is a no-op"
    );
    assert_eq!(device_info(nv::NV_DEVICE_V0_INFO).send(&gpu, A), Ok(0));
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0, 0, 0xd0d0, HDR).send(&gpu, A),
        Ok(0)
    );
    assert_eq!(
        device_info(nv::NV_DEVICE_V0_INFO).send(&gpu, A),
        Err(nv::EINVAL),
        "deleted"
    );
    // Process exit drops the device objects with everything else.
    assert_eq!(device_new(u64::MAX).send(&gpu, A), Ok(0));
    gpu.nouveau_release_process(A);
    assert_eq!(
        device_info(nv::NV_DEVICE_V0_INFO).send(&gpu, A),
        Err(nv::EINVAL)
    );
}

/// The abi16 object ioctls of a libdrm without NVIF, still in nouveau's
/// table and answered as `nouveau_abi16.c` does: `NOTIFIEROBJ_ALLOC` is
/// EINVAL on Fermi and later before the channel is looked at;
/// `GROBJ_ALLOC` refuses handle ~0 (EINVAL), a channel that is not the
/// caller's (ENOENT), the old software and video identifiers that map to
/// no class of this channel and a class of 0 (EINVAL), and otherwise
/// makes the engine object an NVIF NEW would; `GPUOBJ_FREE` refuses a
/// channel that is not the caller's (EINVAL, this one) and a handle that
/// is not an abi16 object of it (ENOENT) -- an NVIF object is on another
/// list, and an NVIF DEL does not reach an abi16 one. All three were
/// ENOSYS, "unhandled".
#[test]
fn the_abi16_object_ioctls_answer_as_nouveau_abi16_does() {
    let _g = LOCK.lock();
    let gpu = gpu();
    let grobj = |channel: i32, handle: u32, class: i32, pid: u64| {
        let mut r = nv::DrmNouveauGrobjAlloc {
            channel,
            handle,
            class,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauGrobjAlloc>(nv::NR_GROBJ_ALLOC),
            &mut r,
            pid,
        )
    };
    let ntfy = |channel: u32, pid: u64| {
        let mut r = nv::DrmNouveauNotifierobjAlloc {
            channel,
            handle: 0x1234,
            size: 4096,
            offset: 0,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauNotifierobjAlloc>(nv::NR_NOTIFIEROBJ_ALLOC),
            &mut r,
            pid,
        )
    };
    let free = |channel: i32, handle: u32, pid: u64| {
        let mut r = nv::DrmNouveauGpuobjFree { channel, handle };
        call(
            &gpu,
            wr::<nv::DrmNouveauGpuobjFree>(nv::NR_GPUOBJ_FREE),
            &mut r,
            pid,
        )
    };
    let einval = Err(nv::EINVAL);
    let enoent = Err(nv::ENOENT);

    assert_eq!(
        grobj(0, u32::MAX, 0xc597, A),
        einval,
        "handle ~0, before the channel"
    );
    assert_eq!(
        grobj(0, 0xbeef, 0xc597, A),
        enoent,
        "no channel: ENOENT here, not NVIF NEW's EINVAL"
    );
    assert_eq!(free(0, 0xbeef, A), einval, "no channel: EINVAL here");
    assert_eq!(
        ntfy(0, A),
        einval,
        "Fermi and later: EINVAL without looking"
    );

    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(ntfy(0, A), einval, "with a channel too");
    assert_eq!(grobj(0, 0xbeef, 0, A), einval, "class 0");
    assert_eq!(
        grobj(0, 0xbeef, 0x506e, A),
        einval,
        "no software class on Turing"
    );
    assert_eq!(grobj(0, 0xbeef, 0x90b1, A), einval, "nor a video one");
    assert_eq!(
        grobj(0, 0xbeef, 0xc597, B),
        enoent,
        "A's channel is not B's"
    );
    assert_eq!(grobj(3, 0xbeef, 0xc597, A), enoent);
    assert_eq!(grobj(0, 0xbeef, 0xc597, A), Ok(0));

    assert_eq!(free(3, 0xbeef, A), einval);
    assert_eq!(free(0, 0xbeef, B), einval, "A's channel is not B's");
    assert_eq!(free(0, 0x1234, A), enoent, "never made");
    // An NVIF object is not on GPUOBJ_FREE's list, and an NVIF DEL does
    // not find a GROBJ one (a no-op there, as for anything it did not
    // make).
    assert_eq!(subchan_new(0, 0xc597, 0x1000).send(&gpu, A), Ok(0));
    assert_eq!(free(0, 0x1000, A), enoent, "an NVIF object");
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0, 0, 0xbeef, HDR).send(&gpu, A),
        Ok(0)
    );
    // The lookup is per channel: the same handle on another channel of
    // the caller's is not it.
    let second = channel_alloc(&gpu, A).unwrap().channel;
    assert_ne!(second, 0);
    assert_eq!(free(second, 0xbeef, A), enoent, "another channel of A's");
    assert_eq!(channel_free(&gpu, second, A), Ok(0));
    assert_eq!(free(0, 0xbeef, A), Ok(0), "still there after the NVIF DEL");
    assert_eq!(free(0, 0xbeef, A), enoent, "twice");
    // CHANNEL_FREE takes the channel's abi16 objects with it.
    assert_eq!(grobj(0, 0xf00d, 0xc5c0, A), Ok(0));
    assert_eq!(channel_free(&gpu, 0, A), Ok(0));
    assert_eq!(grobj(0, 0xbeef, 0xc597, A), enoent, "freed");
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(free(0, 0xf00d, A), enoent, "went with the channel");
    assert_eq!(channel_free(&gpu, 0, A), Ok(0));
}
