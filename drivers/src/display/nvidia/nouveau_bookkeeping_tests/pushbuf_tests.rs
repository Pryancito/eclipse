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

    // Every push names a buffer of the list; the list itself is two
    // objects of the caller's (`pushbuf_validate` reads it next).
    for b in bos.iter_mut() {
        b.handle = object(&gpu, A, 4096, None);
        b.valid_domains = nv::NOUVEAU_GEM_DOMAIN_GART;
    }
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

/// `nouveau_gem_pushbuf_validate` before the submission, hatch or no
/// hatch: `validate_init` walks the list in order -- a handle that is not
/// a GEM object the caller may use is ENOENT (another client's, unless
/// the caller holds it as a PRIME import; a handle nobody has), and one
/// listed twice is EINVAL -- and only then `validate_list` wants
/// `valid_domains` to name VRAM or GART (EINVAL for an empty mask or one
/// with other bits only; either domain alone passes). Every refusal
/// writes the suffixes, like any error past the copy. The list went
/// unread: a stranger's handle or a duplicate reached the submission
/// as good as a real buffer.
#[test]
fn gem_pushbuf_validates_the_buffer_list_like_validate_init() {
    let _g = LOCK.lock();
    let gpu = gpu();
    let c = channel_alloc(&gpu, A).unwrap().channel as u32;
    let mine = object(&gpu, A, 65536, Some(0x1000_0000));
    let mine_too = object(&gpu, A, 4096, None);
    let imported = object(&gpu, B, 8192, Some(0x2000_0000));
    assert!(crate::scheme::gem_mmap::add_ref(imported, A).is_some());
    let theirs = object(&gpu, B, 4096, None);
    let mut pushes = [push_of(0, 0, 16)];
    let push = pushes.as_mut_ptr() as u64;
    let gart = nv::NOUVEAU_GEM_DOMAIN_GART;
    let vram = nv::NOUVEAU_GEM_DOMAIN_VRAM;
    let try_list = |list: &mut [nv::DrmNouveauGemPushbufBo]| {
        let mut r = pushbuf_req(c, 1, push, list.len() as u32, list.as_mut_ptr() as u64);
        let got = pushbuf(&gpu, &mut r, A);
        assert_eq!((r.suffix0, r.suffix1), (0, 0), "written on the way out");
        got
    };
    let with = |handle: u32, valid: u32| {
        let mut b = bo(handle);
        b.valid_domains = valid;
        b
    };

    assert_eq!(
        try_list(&mut [with(mine, gart), with(imported, gart)]),
        Err(nv::EOPNOTSUPP),
        "own and imported: past validation, to the submission this driver does not do"
    );
    assert_eq!(
        try_list(&mut [with(mine, gart), with(theirs, gart)]),
        Err(nv::ENOENT),
        "another client's, not imported"
    );
    assert_eq!(
        try_list(&mut [with(mine, gart), with(0xdead, gart)]),
        Err(nv::ENOENT),
        "nobody's"
    );
    assert_eq!(
        try_list(&mut [with(0xdead, gart), with(mine, gart)]),
        Err(nv::ENOENT),
        "nobody's, listed first"
    );
    assert_eq!(
        try_list(&mut [with(mine, gart), with(mine_too, gart), with(mine, gart)]),
        Err(nv::EINVAL),
        "multiple instances of buffer"
    );
    assert_eq!(
        try_list(&mut [with(mine, 0)]),
        Err(nv::EINVAL),
        "no valid domain"
    );
    assert_eq!(
        // `NOUVEAU_GEM_DOMAIN_CPU` alone.
        try_list(&mut [with(mine, 1)]),
        Err(nv::EINVAL),
        "neither VRAM nor GART"
    );
    assert_eq!(
        try_list(&mut [with(mine, vram)]),
        Err(nv::EOPNOTSUPP),
        "VRAM alone"
    );
    assert_eq!(
        try_list(&mut [with(mine_too, gart)]),
        Err(nv::EOPNOTSUPP),
        "GART alone"
    );
    // `validate_init` runs over the whole list before `validate_list`
    // looks at a domain: the unknown handle wins over the empty mask
    // listed before it.
    assert_eq!(
        try_list(&mut [with(mine, 0), with(0xdead, gart)]),
        Err(nv::ENOENT),
        "the list is looked up before any domain is judged"
    );
    assert_eq!(
        try_list(&mut [with(mine, gart), with(mine_too, 0)]),
        Err(nv::EINVAL),
        "and the domain of every entry is judged"
    );
}

/// The `nvidia.gem_pushbuf` switch for the scope, and back after.
struct Submitting(bool);
impl Submitting {
    fn on() -> Self {
        let was = pushbuf::enabled();
        pushbuf::set_enabled(true);
        Submitting(was)
    }
}
impl Drop for Submitting {
    fn drop(&mut self) {
        pushbuf::set_enabled(self.0);
    }
}

/// A listed buffer, with a `presumed` the driver has to overwrite.
fn bo(handle: u32) -> nv::DrmNouveauGemPushbufBo {
    nv::DrmNouveauGemPushbufBo {
        user_priv: 0,
        handle,
        read_domains: 0,
        write_domains: 0,
        valid_domains: nv::NOUVEAU_GEM_DOMAIN_VRAM | nv::NOUVEAU_GEM_DOMAIN_GART,
        presumed: nv::DrmNouveauGemPushbufBoPresumed {
            valid: 0,
            domain: 0,
            offset: 0xdead_beef,
        },
    }
}
fn push_of(bo_index: u32, offset: u64, length: u64) -> nv::DrmNouveauGemPushbufPush {
    nv::DrmNouveauGemPushbufPush {
        bo_index,
        pad: 0,
        offset,
        length,
    }
}
/// A reloc that patches word `reloc_bo_offset` of buffer `reloc_bo_index`
/// with buffer `bo_index`'s VA.
fn reloc(
    reloc_bo_index: u32,
    reloc_bo_offset: u32,
    bo_index: u32,
    flags: u32,
    data: u32,
) -> nv::DrmNouveauGemPushbufReloc {
    nv::DrmNouveauGemPushbufReloc {
        reloc_bo_index,
        reloc_bo_offset,
        bo_index,
        flags,
        data,
        vor: 0x1100_0000,
        tor: 0x2200_0000,
    }
}
fn submit(
    gpu: &NvidiaGpu,
    pid: u64,
    channel: u32,
    bos: &mut [nv::DrmNouveauGemPushbufBo],
    relocs: &[nv::DrmNouveauGemPushbufReloc],
    pushes: &[nv::DrmNouveauGemPushbufPush],
) -> Result<usize, i32> {
    let mut r = pushbuf_req(
        channel,
        pushes.len() as u32,
        ptr_of(pushes),
        bos.len() as u32,
        if bos.is_empty() {
            0
        } else {
            bos.as_mut_ptr() as u64
        },
    );
    r.nr_relocs = relocs.len() as u32;
    r.relocs = ptr_of(relocs);
    let out = pushbuf(gpu, &mut r, pid);
    assert_eq!((r.suffix0, r.suffix1), (0, 0), "written past the copy");
    out
}
fn presumed(b: &nv::DrmNouveauGemPushbufBo) -> (u32, u32, u64) {
    (b.presumed.valid, b.presumed.domain, b.presumed.offset)
}
/// Word `off` of a GART object, through the CPU mapping the RM gave it.
fn word_at(gpu: &NvidiaGpu, handle: u32, off: u64) -> u32 {
    let pa = gpu
        .nouveau_gem
        .lock()
        .iter()
        .find(|o| o.handle == handle)
        .and_then(|o| o.phys_addr)
        .expect("a CPU mapping");
    unsafe { core::ptr::read_volatile((pa + off) as *const u32) }
}
fn ctx_idx(gpu: &NvidiaGpu, pid: u64) -> u32 {
    ctx_of(gpu, pid).expect("an RM-backed context").0
}

/// With `nvidia.gem_pushbuf`, a submission is `nouveau_gem_ioctl_pushbuf`'s:
/// every listed buffer gets a VA in the channel's VAS (one `vm_bind_map`
/// each, the RM placing it, recorded like a `VM_BIND` so teardown frees
/// it), `presumed` comes back valid with that VA and the domain the buffer
/// is in, and each push goes to the ring as `(VA + offset, length)` in the
/// client's order. The next frame reuses the mappings: no RM call but the
/// pushes. A partial mapping of the same buffer is not reused, because a
/// reloc may name a byte outside it. This path answered EOPNOTSUPP.
#[test]
fn gem_pushbuf_maps_each_buffer_once_writes_presumed_back_and_pushes_in_order() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let _on = Submitting::on();
    let gpu = gpu_rm();
    let ch = channel_alloc(&gpu, A).unwrap().channel as u32;
    let ctx = ctx_idx(&gpu, A);
    let gart = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let vram = gem_new_rm(&gpu, 8192, nv::NOUVEAU_GEM_DOMAIN_VRAM, A)
        .unwrap()
        .handle;
    let mut bos = [bo(gart), bo(vram)];
    let pushes = [push_of(1, 0x1000, 0x20), push_of(0, 0x100, 0x40)];

    let before = FAKE_RM.lock().calls.len();
    assert_eq!(submit(&gpu, A, ch, &mut bos, &[], &pushes), Ok(0));
    assert_eq!(
        rm_calls_since(before),
        ["vm_bind_map", "vm_bind_map", "exec_submit", "exec_submit"]
    );
    let (va_gart, va_vram) = (bos[0].presumed.offset, bos[1].presumed.offset);
    assert_ne!(va_gart, 0, "the RM placed it");
    assert_ne!(va_vram, va_gart);
    assert_eq!(
        presumed(&bos[0]),
        (1, nv::NOUVEAU_GEM_DOMAIN_GART, va_gart),
        "valid, where it is, and the domain it is in"
    );
    assert_eq!(presumed(&bos[1]), (1, nv::NOUVEAU_GEM_DOMAIN_VRAM, va_vram));
    let maps = driver_maps(&gpu, A);
    assert_eq!(maps.len(), 2, "recorded like a VM_BIND");
    assert_eq!((maps[0].0, maps[0].1, maps[0].2), (gart, va_gart, 65536));
    assert_eq!((maps[1].0, maps[1].1, maps[1].2), (vram, va_vram, 8192));
    assert_ne!(maps[0].3, 0, "the RM's h_virt, so teardown can unmap it");
    let rm_maps = FAKE_RM.lock().maps_of_ctx(ctx);
    assert_eq!(
        rm_maps,
        [
            (va_gart, 65536, nv::PTE_KIND_GENERIC),
            (va_vram, 8192, nv::PTE_KIND_GENERIC)
        ],
        "in the channel's context, whole, generic kind"
    );
    assert_eq!(
        FAKE_RM.lock().submits_of_ctx(ctx),
        [(va_vram + 0x1000, 0x20), (va_gart + 0x100, 0x40)],
        "the client's order, each at its own VA"
    );

    // The next frame: nothing to map, the same VAs.
    let mut again = [bo(gart), bo(vram)];
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(submit(&gpu, A, ch, &mut again, &[], &pushes[..1]), Ok(0));
    assert_eq!(rm_calls_since(before), ["exec_submit"]);
    assert_eq!(presumed(&again[0]), presumed(&bos[0]));
    assert_eq!(presumed(&again[1]), presumed(&bos[1]));
    assert_eq!(driver_maps(&gpu, A).len(), 2, "no second mapping");

    // A partial mapping is not a whole one, and another process's mapping
    // of the same handle is not this one's: a fresh VA, both kept.
    let other = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    for (owner, va, size, bo_offset) in [
        (A, 0x5_0000_0000, 0x1000, 0x1000),
        (B, 0x6_0000_0000, 65536, 0),
    ] {
        gpu.nouveau_vm_mappings.lock().push(nv::NouveauVmMapping {
            gem_handle: other,
            h_virt: 0xbeef,
            owner_pid: owner,
            va,
            size,
            bo_offset,
            pte_kind: 0,
        });
    }
    let mut third = [bo(other)];
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(submit(&gpu, A, ch, &mut third, &[], &pushes[1..]), Ok(0));
    assert_eq!(rm_calls_since(before), ["vm_bind_map", "exec_submit"]);
    assert_ne!(third[0].presumed.offset, 0x5_0000_0000, "not the partial");
    assert_ne!(third[0].presumed.offset, 0x6_0000_0000, "not B's");
    assert_eq!(driver_maps(&gpu, A).len(), 4);
    assert_eq!(driver_maps(&gpu, B).len(), 1);
}

/// Relocs are applied after the buffers are placed and before anything is
/// pushed: the word at `reloc_bo_offset` of the patched buffer becomes the
/// referenced buffer's VA (low or high half, `data` added in 64 bits) with
/// `tor` or `vor` OR'd in by the domain it is in, stored through the
/// patched buffer's CPU mapping. A reloc naming a buffer past the list, an
/// offset that is not a whole word of its buffer, a buffer without a CPU
/// mapping, or a null array with a count refuses the whole request before
/// the ring sees a push; the placement and `presumed` stand, so the next
/// frame still needs no reloc for those buffers.
#[test]
fn gem_pushbuf_relocs_patch_the_words_through_the_cpu_mapping_before_any_push() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let _on = Submitting::on();
    let gpu = gpu_rm();
    let ch = channel_alloc(&gpu, A).unwrap().channel as u32;
    let ctx = ctx_idx(&gpu, A);
    let pb = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let data = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let tex = gem_new_rm(&gpu, 8192, nv::NOUVEAU_GEM_DOMAIN_VRAM, A)
        .unwrap()
        .handle;
    let mut bos = [bo(pb), bo(data), bo(tex)];
    let pushes = [push_of(0, 0, 0x20)];
    let low = pushbuf::NOUVEAU_GEM_RELOC_LOW;
    let high = pushbuf::NOUVEAU_GEM_RELOC_HIGH;
    let or = pushbuf::NOUVEAU_GEM_RELOC_OR;
    let relocs = [
        reloc(0, 0x10, 1, low, 0x30),
        reloc(0, 0x14, 1, high, 0x30),
        reloc(0, 0x18, 1, low | or, 0),
        reloc(0, 0x1c, 2, high | or, 0),
        reloc(0, 0xfffc, 2, or, 0x0000_00aa),
    ];
    assert_eq!(submit(&gpu, A, ch, &mut bos, &relocs, &pushes), Ok(0));
    let (va_data, va_tex) = (bos[1].presumed.offset, bos[2].presumed.offset);
    assert_eq!(word_at(&gpu, pb, 0x10), (va_data + 0x30) as u32, "LOW");
    assert_eq!(
        word_at(&gpu, pb, 0x14),
        ((va_data + 0x30) >> 32) as u32,
        "HIGH"
    );
    assert_eq!(
        word_at(&gpu, pb, 0x18),
        va_data as u32 | 0x2200_0000,
        "OR: tor for a GART buffer"
    );
    assert_eq!(
        word_at(&gpu, pb, 0x1c),
        (va_tex >> 32) as u32 | 0x1100_0000,
        "OR: vor for a VRAM buffer"
    );
    assert_eq!(word_at(&gpu, pb, 0xfffc), 0x1100_00aa, "the last word");
    assert_eq!(word_at(&gpu, pb, 0x0c), 0, "its neighbours untouched");
    assert_eq!(word_at(&gpu, pb, 0x20), 0);
    assert_eq!(FAKE_RM.lock().submits_of_ctx(ctx).len(), 1);

    // Each refusal: nothing pushed, the words as they were, `presumed`
    // written all the same.
    let before_words = [word_at(&gpu, pb, 0x10), word_at(&gpu, pb, 0x18)];
    for (bad, why, err) in [
        (
            reloc(0, 0x10, 3, low, 0),
            "bo_index past the list",
            nv::EINVAL,
        ),
        (
            reloc(3, 0x10, 1, low, 0),
            "reloc_bo_index past the list",
            nv::EINVAL,
        ),
        (reloc(0, 0x11, 1, low, 0), "not a word", nv::EINVAL),
        (reloc(0, 0xfffd, 1, low, 0), "not a whole word", nv::EINVAL),
        (reloc(0, 0x1_0000, 1, low, 0), "past the buffer", nv::EINVAL),
        (
            reloc(2, 0, 1, low, 0),
            "no CPU mapping to store through",
            nv::EINVAL,
        ),
    ] {
        let mut bos = [bo(pb), bo(data), bo(tex)];
        assert_eq!(
            submit(&gpu, A, ch, &mut bos, &[relocs[2], bad], &pushes),
            Err(err),
            "{why}"
        );
        assert_eq!(
            presumed(&bos[1]),
            (1, nv::NOUVEAU_GEM_DOMAIN_GART, va_data),
            "{why}"
        );
        assert_eq!(FAKE_RM.lock().submits_of_ctx(ctx).len(), 1, "{why}");
    }
    assert_eq!(
        [word_at(&gpu, pb, 0x10), word_at(&gpu, pb, 0x18)],
        before_words,
        "the reloc ahead of the bad one was stored, nothing else"
    );
    let mut r = pushbuf_req(ch, 1, ptr_of(&pushes), 3, bos.as_mut_ptr() as u64);
    r.nr_relocs = 1;
    assert_eq!(pushbuf(&gpu, &mut r, A), Err(nv::EFAULT), "null relocs");
    assert_eq!((r.suffix0, r.suffix1), (0, 0));
    assert_eq!(FAKE_RM.lock().submits_of_ctx(ctx).len(), 1);
}

/// What stops a submission that passed the ioctl's own checks: a listed
/// handle that is not the caller's (ENOENT, before the RM is asked), a
/// mapping the RM refuses or places at VA 0 (EIO, nothing recorded), a push
/// that is not a range of its buffer (EINVAL, after the buffers are
/// placed, before the ring), a ring that will not take it (EIO), a
/// discovery channel with no ring behind it (EOPNOTSUPP), and a GPU the RM
/// never attached (EIO).
#[test]
fn gem_pushbuf_refuses_what_it_cannot_map_place_or_push() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let _on = Submitting::on();
    let gpu = gpu_rm();
    let ch = channel_alloc(&gpu, A).unwrap().channel as u32;
    let ctx = ctx_idx(&gpu, A);
    let mine = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let theirs = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, B)
        .unwrap()
        .handle;
    let one = [push_of(0, 0, 0x20)];
    let eio = Err(nv::EIO);

    // The list is validated whole before anything is mapped: `mine`,
    // ahead of the bad one, is not placed either, and the RM is not asked.
    for (handle, why) in [
        (0xdead_0001, "no such handle"),
        (theirs, "another client's"),
    ] {
        let before = FAKE_RM.lock().calls.len();
        let mut bos = [bo(mine), bo(handle)];
        assert_eq!(
            submit(&gpu, A, ch, &mut bos, &[], &[push_of(1, 0, 0x20)]),
            Err(nv::ENOENT),
            "{why}"
        );
        assert!(rm_calls_since(before).is_empty(), "{}: before the RM", why);
        assert!(driver_maps(&gpu, A).is_empty(), "{}", why);
    }

    let fresh = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    FAKE_RM.lock().refuse_map = true;
    let mut bos = [bo(fresh)];
    assert_eq!(
        submit(&gpu, A, ch, &mut bos, &[], &one),
        eio,
        "the RM refused"
    );
    FAKE_RM.lock().refuse_map = false;
    assert!(driver_maps(&gpu, A).is_empty(), "nothing recorded");
    let was = core::mem::replace(&mut FAKE_RM.lock().picked_va, 0);
    assert_eq!(
        submit(&gpu, A, ch, &mut bos, &[], &one),
        eio,
        "placed at VA 0"
    );
    FAKE_RM.lock().picked_va = was;
    assert!(driver_maps(&gpu, A).is_empty());
    assert_eq!(presumed(&bos[0]).0, 0, "never placed: presumed untouched");

    let before = FAKE_RM.lock().calls.len();
    for (p, why) in [
        (push_of(0, 0xff00, 0x200), "past the buffer"),
        (push_of(0, 0, 0), "empty"),
        (push_of(0, 2, 0x20), "not word-aligned"),
    ] {
        let mut bos = [bo(fresh)];
        assert_eq!(
            submit(&gpu, A, ch, &mut bos, &[], &[p]),
            Err(nv::EINVAL),
            "{why}"
        );
        assert_eq!(presumed(&bos[0]).0, 1, "{why}: placed first");
    }
    assert_eq!(
        rm_calls_since(before),
        ["vm_bind_map"],
        "mapped once, never pushed"
    );
    assert_eq!(FAKE_RM.lock().submits_of_ctx(ctx).len(), 0);

    FAKE_RM.lock().ring_full = true;
    let mut bos = [bo(fresh)];
    assert_eq!(
        submit(&gpu, A, ch, &mut bos, &[], &one),
        eio,
        "the ring refused"
    );
    FAKE_RM.lock().ring_full = false;
    assert_eq!(
        FAKE_RM.lock().submits_of_ctx(ctx).len(),
        1,
        "it was offered"
    );

    FAKE_RM.lock().fail_ctx_alloc = true;
    let disc = channel_alloc(&gpu, STRANGER).unwrap().channel as u32;
    FAKE_RM.lock().fail_ctx_alloc = false;
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, STRANGER)
        .unwrap()
        .handle;
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        submit(&gpu, STRANGER, disc, &mut [bo(h)], &[], &one),
        Err(nv::EOPNOTSUPP),
        "a discovery channel has no ring"
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);

    let bare = super::gpu();
    let c = channel_alloc(&bare, A).unwrap().channel as u32;
    let h = object(&bare, A, 65536, Some(0x4000));
    assert_eq!(
        submit(&bare, A, c, &mut [bo(h)], &[], &one),
        eio,
        "no RM behind the GPU"
    );
}
