//! The RM-side edges of the channel, EXEC, abi16 and GEM arms that the
//! bookkeeping tests reach with the fake RM: a channel table full under
//! the compositor, an empty EXEC whose wait never lands, a `GPUOBJ_FREE`
//! whose RM object is already gone, a GPU the id table cannot size, and
//! the ioctl that answers ENOSYS on purpose. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own
//! because the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::utils::host_hooks::take_dmesg;

/// `CHANNEL_ALLOC` on a table already holding `MAX_CHANNELS` channels is
/// EBUSY before any RM work -- the check sits ahead of the ladder, so a
/// full table costs no RM call -- and the compositor keeps its sticky
/// context-0 seat through it: the retry, once a discovery channel has
/// been freed, is the compositor's again and not a GL client's.
#[test]
fn channel_alloc_on_a_full_table_is_ebusy_before_the_rm_and_keeps_the_compositors_seat() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    assert!(channel_alloc(&gpu, COMP).is_ok());
    assert_eq!(ctx0_owner(&gpu), COMP, "the compositor holds context 0");
    let mut strangers = alloc::vec::Vec::new();
    while gpu.nouveau_channels.lock().len() < nv::MAX_CHANNELS {
        strangers.push(channel_alloc(&gpu, STRANGER).unwrap().channel);
    }
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        channel_alloc(&gpu, COMP).map(|c| c.channel),
        Err(nv::EBUSY),
        "the table is full"
    );
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "refused before the RM"
    );
    assert_eq!(ctx0_owner(&gpu), COMP, "the seat is kept for the retry");
    assert_eq!(
        gpu.nouveau_channels.lock().len(),
        nv::MAX_CHANNELS,
        "nothing was added"
    );
    assert_eq!(
        channel_free(&gpu, strangers.pop().unwrap(), STRANGER),
        Ok(0)
    );
    assert!(channel_alloc(&gpu, COMP).is_ok(), "the retry");
    assert_eq!(ctx0_owner(&gpu), COMP, "still the compositor's");
    gpu.nouveau_release_process(STRANGER);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// An empty EXEC (the health probe) waits on the CPU for a fence another
/// context's submission has not landed yet, like a real submission: ten
/// seconds, then EIO -- the fence's own timeout (the same ten seconds,
/// counted from the submit) fires first, the wait reads as satisfied, and
/// the EXEC sees it was satisfied only by a timeout and refuses to go on
/// -- with the wait named in dmesg, and its own sig list stays unsignaled
/// (an `nvkmd` WaitIdle that returned 0 here would let NVK reuse buffers
/// the GPU never released). A wait on a syncobj with no fence at all is
/// EINVAL, as `drm_syncobj_find_fence` answers.
#[test]
fn an_empty_exec_whose_wait_never_lands_is_eio_after_ten_seconds_and_signals_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let never = syncobj::create(false);
    let out = syncobj::create(false);
    // A's push signals `never` with its fence; nothing runs A's ring.
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(never)]),
        Ok(0)
    );
    assert_eq!(syncobj::query(never), Some(0), "pending, not signaled");
    let bare = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[], &[sync(bare)], &[sync(out)]),
        Err(nv::EINVAL),
        "a syncobj with no fence to wait for"
    );
    let _ = take_dmesg();
    // A second per clock read: the ten seconds pass in a dozen reads.
    let tick = AutoAdvance::of(1_000_000);
    assert_eq!(
        exec(&gpu, B, ch_b, &[], &[sync(never)], &[sync(out)]),
        Err(nv::EIO),
        "the fence never landed"
    );
    drop(tick);
    assert_eq!(syncobj::query(out), Some(0), "nothing signaled");
    let dmesg = take_dmesg();
    assert!(
        dmesg.iter().any(|(_, l)| l.contains(
            "EXEC: a wait syncobj was reached only by a fence that TIMED OUT -- NOT submitting"
        )),
        "{:?}",
        dmesg
    );
    assert!(
        !nv::ctx_is_wedged(2),
        "a timed-out wait is not a wedged ring"
    );
    for h in [never, out, bare] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(A);
}

/// `GPUOBJ_FREE` of an engine object on an RM-backed channel frees the
/// RM class behind it; when the RM no longer has that object (it went
/// with the context, or was never made), the ioctl still succeeds -- the
/// abi16 record is gone either way -- and says what the RM answered.
#[test]
fn gpuobj_free_frees_the_rm_class_and_reports_one_the_rm_no_longer_has() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = channel_alloc(&gpu, A).unwrap().channel;
    let grobj = |handle: u32| {
        let mut r = nv::DrmNouveauGrobjAlloc {
            channel: ch,
            handle,
            class: 0xc597,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauGrobjAlloc>(nv::NR_GROBJ_ALLOC),
            &mut r,
            A,
        )
    };
    let free = |handle: u32| {
        let mut r = nv::DrmNouveauGpuobjFree {
            channel: ch,
            handle,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauGpuobjFree>(nv::NR_GPUOBJ_FREE),
            &mut r,
            A,
        )
    };
    assert_eq!(grobj(0xbeef), Ok(0));
    assert_eq!(FAKE_RM.lock().classes.len(), 1, "a real RM object");
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(free(0xbeef), Ok(0));
    assert_eq!(rm_calls_since(before), ["class_free"]);
    {
        let rm = FAKE_RM.lock();
        assert!(rm.classes.is_empty(), "the RM object went with it");
        assert_eq!((rm.class_frees, rm.bad), (1, 0));
    }
    assert_eq!(
        free(0xbeef),
        Err(nv::ENOENT),
        "gone from the abi16 list too"
    );
    assert_eq!(grobj(0xf00d), Ok(0));
    FAKE_RM.lock().classes.clear();
    let _ = take_dmesg();
    assert_eq!(
        free(0xf00d),
        Ok(0),
        "the record goes even when the RM has nothing"
    );
    assert_eq!(
        FAKE_RM.lock().bad,
        1,
        "the RM was asked for an object it did not have"
    );
    let dmesg = take_dmesg();
    assert!(
        dmesg
            .iter()
            .any(|(_, l)| l.contains("GPUOBJ_FREE: class_free(")),
        "{:?}",
        dmesg
    );
    gpu.nouveau_release_process(A);
}

/// A GPU the id table cannot size (no VRAM figure and an architecture
/// without a floor) is capped at the global GEM ceiling alone, so a
/// GEM_NEW that fits it goes on to the RM (ENODEV here, none attached)
/// where a board sized at 1 MiB refuses the same request at its own
/// ceiling (ENOMEM). And `GET_ZCULL_INFO` is ENOSYS by design: mesa
/// probes it and tolerates the refusal.
#[test]
fn a_gpu_without_a_vram_figure_is_capped_at_the_global_ceiling_and_zcull_info_is_enosys() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    nv::set_enabled(true);
    let mut gpu = NvidiaGpu::for_test(0x1f06, 0);
    gpu.architecture = NvidiaArchitecture::Unknown;
    assert_eq!(
        gem_new(&gpu, 3 * MIB, nv::NOUVEAU_GEM_DOMAIN_GART, A),
        Err(nv::ENODEV),
        "within the global ceiling: on to the RM, which is not there"
    );
    gpu.architecture = NvidiaArchitecture::Turing;
    gpu.vram_size_mb = 1;
    assert_eq!(
        gem_new(&gpu, 3 * MIB, nv::NOUVEAU_GEM_DOMAIN_GART, A),
        Err(nv::ENOMEM),
        "twice the board's VRAM is the ceiling: 2 MiB"
    );
    let mut probe = 0u64;
    assert_eq!(
        call(&gpu, wr::<u64>(nv::NR_GET_ZCULL_INFO), &mut probe, A),
        Err(nv::ENOSYS)
    );
}
