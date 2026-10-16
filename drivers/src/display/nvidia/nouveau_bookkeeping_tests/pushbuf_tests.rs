//! `nouveau_gem_ioctl_pushbuf`: what GEM_PUSHBUF checks before it would
//! submit, in Linux's order. A child of `nouveau_bookkeeping_tests` so it
//! has the same harness (`LOCK`, `LiveBytes`, `gpu_rm`, `wr`, the channel
//! helpers); a file of its own because the parent is at its size ceiling
//! (`xtask tamano`).

use super::*;

fn pushbuf_req(
    channel: u32,
    nr_push: u32,
    push: u64,
    nr_buffers: u32,
    buffers: u64,
) -> nv::DrmNouveauGemPushbuf {
    nv::DrmNouveauGemPushbuf {
        channel,
        nr_buffers,
        buffers,
        nr_relocs: 0,
        nr_push,
        relocs: 0,
        push,
        suffix0: 0xdead_beef,
        suffix1: 0xdead_beef,
        vram_available: 0,
        gart_available: 0,
    }
}

fn pushbuf(gpu: &NvidiaGpu, r: &mut nv::DrmNouveauGemPushbuf, pid: u64) -> Result<usize, i32> {
    call(
        gpu,
        wr::<nv::DrmNouveauGemPushbuf>(nv::NR_GEM_PUSHBUF),
        r,
        pid,
    )
}

/// `nouveau_gem_ioctl_pushbuf` before the submission this driver does
/// not do: ENOSYS for a client with a uvmm (before the channel), ENOENT
/// for a channel that is not the caller's, the VRAM and GART budgets
/// written back, a request with no pushes is a success with the
/// suffixes zeroed, the counts capped (EINVAL, suffixes untouched), a
/// null array with a count EFAULT, a push naming a buffer past the list
/// EINVAL with the suffixes written, and only then the (unimplemented)
/// submission, EOPNOTSUPP. This answered EOPNOTSUPP to all of them.
#[test]
fn gem_pushbuf_checks_what_linux_checks_before_it_submits() {
    let _g = LOCK.lock();
    let gpu = gpu();
    let c = channel_alloc(&gpu, A).unwrap().channel as u32;
    let dead = (0xdead_beefu32, 0xdead_beefu32);

    // Nothing to submit: done, budgets and suffixes written.
    let mut r = pushbuf_req(c, 0, 0, 0, 0);
    assert_eq!(pushbuf(&gpu, &mut r, A), Ok(0));
    assert_eq!((r.suffix0, r.suffix1), (0, 0), "an indirect-buffer channel");
    assert_eq!(r.vram_available, 8192 * MIB, "GETPARAM FB_SIZE's number");
    assert_eq!(
        r.gart_available,
        getparam(&gpu, nv::NOUVEAU_GETPARAM_AGP_SIZE).unwrap(),
        "GETPARAM AGP_SIZE's number"
    );
    assert_eq!(r.gart_available, 512 << 40);

    // The channel has to be the caller's.
    let mut r = pushbuf_req(c + 7, 0, 0, 0, 0);
    assert_eq!(pushbuf(&gpu, &mut r, A), Err(nv::ENOENT), "no such channel");
    assert_eq!((r.suffix0, r.suffix1), dead, "nothing written");
    let mut r = pushbuf_req(c, 0, 0, 0, 0);
    assert_eq!(
        pushbuf(&gpu, &mut r, B),
        Err(nv::ENOENT),
        "another client's"
    );

    // A client with a uvmm is on the other uAPI, channel or not.
    with_uvmm(&gpu, B);
    let cb = channel_alloc(&gpu, B).unwrap().channel as u32;
    let mut r = pushbuf_req(cb, 0, 0, 0, 0);
    assert_eq!(pushbuf(&gpu, &mut r, B), Err(nv::ENOSYS));
    let mut r = pushbuf_req(cb + 99, 0, 0, 0, 0);
    assert_eq!(
        pushbuf(&gpu, &mut r, B),
        Err(nv::ENOSYS),
        "before the channel"
    );

    // The caps, before the arrays are read.
    let mut pushes = [nv::DrmNouveauGemPushbufPush {
        bo_index: 0,
        pad: 0,
        offset: 0,
        length: 16,
    }; 2];
    let mut bos = [nv::DrmNouveauGemPushbufBo {
        user_priv: 0,
        handle: 1,
        read_domains: 0,
        write_domains: 0,
        valid_domains: 0,
        presumed: nv::DrmNouveauGemPushbufBoPresumed {
            valid: 0,
            domain: 0,
            offset: 0,
        },
    }; 2];
    let (push, buffers) = (pushes.as_mut_ptr() as u64, bos.as_mut_ptr() as u64);
    let mut r = pushbuf_req(c, 513, push, 2, buffers);
    assert_eq!(pushbuf(&gpu, &mut r, A), Err(nv::EINVAL), "513 pushes");
    assert_eq!((r.suffix0, r.suffix1), dead, "refused before the copy");
    assert_eq!(r.vram_available, 8192 * MIB, "but the budgets are written");
    let mut r = pushbuf_req(c, 1, push, 1025, buffers);
    assert_eq!(pushbuf(&gpu, &mut r, A), Err(nv::EINVAL), "1025 buffers");
    let mut r = pushbuf_req(c, 1, push, 2, buffers);
    r.nr_relocs = 1025;
    assert_eq!(pushbuf(&gpu, &mut r, A), Err(nv::EINVAL), "1025 relocs");

    // A null array with a count is a copy that fails.
    let mut r = pushbuf_req(c, 1, 0, 2, buffers);
    assert_eq!(pushbuf(&gpu, &mut r, A), Err(nv::EFAULT), "null pushes");
    let mut r = pushbuf_req(c, 1, push, 2, 0);
    assert_eq!(pushbuf(&gpu, &mut r, A), Err(nv::EFAULT), "null buffers");
    assert_eq!((r.suffix0, r.suffix1), dead);

    // Every push names a buffer of the list.
    pushes[1].bo_index = 2;
    let mut r = pushbuf_req(c, 2, push, 2, buffers);
    assert_eq!(
        pushbuf(&gpu, &mut r, A),
        Err(nv::EINVAL),
        "push 1 buffer not in list"
    );
    assert_eq!((r.suffix0, r.suffix1), (0, 0), "written on the way out");
    pushes[1].bo_index = 1;
    let mut r = pushbuf_req(c, 2, push, 2, buffers);
    assert_eq!(
        pushbuf(&gpu, &mut r, A),
        Err(nv::EOPNOTSUPP),
        "and only then the submission this driver does not do"
    );
    assert_eq!((r.suffix0, r.suffix1), (0, 0));
    assert_eq!(
        (pushes[0].bo_index, pushes[1].bo_index),
        (0, 1),
        "the pushes are only read"
    );
}
