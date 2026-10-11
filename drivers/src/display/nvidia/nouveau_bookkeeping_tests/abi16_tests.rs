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
