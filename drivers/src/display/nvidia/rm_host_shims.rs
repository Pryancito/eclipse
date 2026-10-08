const NV_ERR_NOT_SUPPORTED: u32 = 0x56;

#[no_mangle]
extern "C" fn eclipse_rm_attach_gpu(
    _a0: u32,
    _a1: u8,
    _a2: u8,
    _a3: u64,
    _a4: *mut u8,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
    _a9: u64,
    _a10: *mut u8,
) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_bench(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_ce_blit(_a0: u32, _a1: u64, _a2: u64, _a3: u64, _a4: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_ce_blit_p2p(_a0: u32, _a1: u64, _a2: u64, _a3: u64, _a4: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_ce_blit_p2p_2d(
    _a0: u32,
    _a1: u64,
    _a2: u32,
    _a3: u64,
    _a4: u32,
    _a5: u32,
    _a6: u32,
    _a7: *mut u8,
) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_ce_fill_fb(_a0: u32, _a1: u64, _a2: u64, _a3: u32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_ce_fill_fb_p2p(_a0: u32, _a1: u64, _a2: u64, _a3: u32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_ce_release_inflight() -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_ce_wait(_a0: u32, _a1: u64) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
/// The singleton channel's error notifier (`NvNotification`, 16 bytes).
/// Real memory: the EXEC failure path reads it through `phys_to_virt`,
/// the identity here, with no RM call.
static NOTIFIER: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
pub(super) fn notifier_pa() -> u64 {
    NOTIFIER.as_ptr() as u64
}
/// Cached at `CHANNEL_ALLOC` for that failure path; there is one only
/// while step 17's channel stands.
#[no_mangle]
extern "C" fn eclipse_rm_chan_notifier_pa(_inst: u32, out: *mut u64) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("chan_notifier_pa");
    if !f.chan_built {
        return NV_ERR_INVALID_STATE;
    }
    unsafe { *out = notifier_pa() };
    NV_OK
}
/// Tear down the singleton channel and clear step 17's cache, so the
/// next `step17` builds a new channel behind index 0; the ladder's VAS
/// stays. Drops every peer-fence mapping context 0 takes part in, as
/// the C does. A no-op before step 17, also as the C.
#[no_mangle]
extern "C" fn eclipse_rm_ctx0_reset(_inst: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("ctx0_reset");
    if !f.chan_built {
        return NV_OK;
    }
    f.chan_built = false;
    f.ctx0_resets += 1;
    f.ctxs.retain(|c| *c != 0);
    f.peer_maps.retain(|m| m.0 != 0 && m.1 != 0);
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_edid(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
/// A direct-submit channel the fake handed out: the pages the driver
/// writes GP entries, semaphore streams and GPPut into and reads GPGet
/// from. Real memory, since `phys_to_virt` is the identity here.
#[derive(Clone, Copy, Debug)]
pub(super) struct FastChan {
    pub ctx: u32,
    /// 64 KiB: the fence slot page at `FAST_PB_OFF`, the landing zone
    /// at `FAST_SEM_OFF`, the GPFIFO ring at `FAST_GPFIFO_OFF`.
    pub buf: usize,
    /// 256 B USERD window: GPGet at 0x88, GPPut at 0x8c.
    pub userd: usize,
    /// Where `buf` is bound in the context's VAS.
    pub gpu_va: u64,
    pub token: u32,
    /// Which build of the context this window belongs to: the RM
    /// refuses a window left over from a channel that was freed.
    build: u32,
}

/// BAR0 offset of the usermode doorbell the fake reports.
pub(super) const FAKE_DOORBELL: u32 = 0x0081_0000;
pub(super) const FAST_PB_OFF: u32 = 0xA000;
pub(super) const FAST_SEM_OFF: u32 = 0xB000;
pub(super) const FAST_GPFIFO_OFF: u32 = 0xC000;
pub(super) const FAST_ENTRIES: u32 = 128;
pub(super) const FAST_SLOT_BYTES: u32 = 32;
/// `NV_ERR_INVALID_STATE`: what the C side answers for a channel that
/// is not ready, and for a USERD window it mapped for a channel that
/// has since been freed.
const NV_ERR_INVALID_STATE: u32 = 0x40;

/// The constant half of a direct submit, once per context, the way
/// `eclipse_rm_exec_fast_prepare` computes it: `NV_OK` with the
/// verdict in `status`, as the C side does past its argument checks.
#[no_mangle]
extern "C" fn eclipse_rm_exec_fast_prepare(_inst: u32, ctx_idx: u32, out: *mut ExecFast) -> u32 {
    use super::super::nouveau_uapi::{gp_entry0, gp_entry1, sem_release_stream};
    use nvidia_rm_sys::rm_init::{EXEC_FAST_CHK_LEN, EXEC_FAST_CHK_VA};
    let mut f = FAKE_RM.lock();
    f.calls.push("exec_fast_prepare");
    if !f.fast {
        return NV_ERR_NOT_SUPPORTED;
    }
    let build = f.ctx_build.iter().find(|b| b.0 == ctx_idx).map(|b| b.1);
    let chan = match (f.ctxs.contains(&ctx_idx), build) {
        (true, Some(build)) => match f.fast_ctxs.iter().find(|c| c.ctx == ctx_idx) {
            Some(c) if c.build == build => Some(*c),
            // The window still maps the USERD of a channel that was
            // freed: the RM refuses rather than poke freed BAR1.
            Some(_) => None,
            None => {
                let buf = alloc::boxed::Box::leak(alloc::vec![0u64; 8192].into_boxed_slice())
                    .as_ptr() as usize;
                let userd = alloc::boxed::Box::leak(alloc::vec![0u64; 32].into_boxed_slice())
                    .as_ptr() as usize;
                let c = FastChan {
                    ctx: ctx_idx,
                    buf,
                    userd,
                    gpu_va: 0x7000_0000 + (u64::from(ctx_idx) << 20),
                    token: 0xC0DE_0000 | ctx_idx,
                    build,
                };
                f.fast_ctxs.push(c);
                Some(c)
            }
        },
        _ => None,
    };
    let result = if let Some(c) = chan {
        let stream = sem_release_stream(EXEC_FAST_CHK_VA, 0);
        let sem_execute = if f.fast_bad_encoding {
            stream[5] ^ (1 << 20)
        } else {
            stream[5]
        };
        ExecFast {
            status: NV_OK,
            work_token: c.token,
            runlist_id: 7,
            userd_size: 0x100,
            userd_cpu: c.userd as u64,
            fence_pb_phys: (c.buf + FAST_PB_OFF as usize) as u64,
            fence_sem_phys: (c.buf + FAST_SEM_OFF as usize) as u64,
            gpfifo_phys: (c.buf + FAST_GPFIFO_OFF as usize) as u64,
            buf_gpu_va: c.gpu_va,
            gpfifo_entries: FAST_ENTRIES,
            doorbell_reg: FAKE_DOORBELL,
            fence_pb_off: FAST_PB_OFF,
            fence_sem_off: FAST_SEM_OFF,
            gpfifo_off: FAST_GPFIFO_OFF,
            slot_bytes: FAST_SLOT_BYTES,
            chk_gp_entry0: gp_entry0(EXEC_FAST_CHK_VA),
            chk_gp_entry1: gp_entry1(EXEC_FAST_CHK_VA, EXEC_FAST_CHK_LEN),
            chk_sem_hdr: stream[0],
            chk_sem_addr_hi: stream[2],
            chk_sem_execute: sem_execute,
            userd_gpget_off: 0x88,
            userd_gpput_off: 0x8c,
        }
    } else {
        ExecFast {
            status: NV_ERR_INVALID_STATE,
            ..ExecFast::default()
        }
    };
    unsafe { *out = result };
    NV_OK
}
/// Drop the window of `ctx_idx`; a no-op when there is none, as in C.
#[no_mangle]
extern "C" fn eclipse_rm_exec_fast_release(_inst: u32, ctx_idx: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("exec_fast_release");
    if let Some(i) = f.fast_ctxs.iter().position(|c| c.ctx == ctx_idx) {
        f.fast_ctxs.remove(i);
        f.fast_releases.push(ctx_idx);
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_get_gsp_info(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_hdmi_audio(_a0: u32, _a1: u32, _a2: u32, _a3: u8, _a4: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_hide(_a0: u32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_image(_a0: u32, _a1: *const u8, _a2: u32, _a3: u32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_init(_a0: u32, _a1: u32, _a2: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_move(_a0: i32, _a1: i32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
/// The NVC57E surface-flip ladder as the host sees it. The display front
/// end's fetch of a flip is modelled in polls: each accepted flip leaves
/// `fetch_polls` answers of "still pending" behind it, and the drain in
/// `surfaceflip_drain` sees them one per `hwflip_pending` call.
pub(super) struct FakeHwflip {
    /// `hwflip_init` succeeds.
    pub init_ok: bool,
    pub ready: bool,
    pub init_calls: usize,
    /// "Pending" answers still owed before `Get == Put`.
    pub pending_polls: u64,
    /// The front end never catches up: a display that stopped fetching.
    pub pending_forever: bool,
    /// `pending_forever` was polled ten million times: the drain has no
    /// bound. The fake then answers "clear" so the test fails instead of
    /// hanging (a panic cannot unwind out of an `extern "C"` shim).
    pub overpolled: bool,
    /// Polls each accepted flip leaves pending.
    pub fetch_polls: u64,
    /// Microseconds of the test clock each accepted flip stays pending
    /// (0: none): a front end that fetches in time, not in polls.
    pub fetch_delay_us: u64,
    /// The clock reading at which the current flip is fetched.
    pub pending_until_us: u64,
    /// Every `hwflip_pending` call.
    pub polls: u64,
    /// Accepted flips: `(h_memory, plane offset, width, height, pitch)`.
    pub surfaces: Vec<(u32, u64, u32, u32, u32)>,
    /// Flips the C side's safety net refused (`NV_ERR_BUSY_RETRY`)
    /// because the caller wrote while the front end was still fetching.
    pub refused_busy: usize,
    /// Refuse the next flip BUSY although nothing is pending: what a
    /// cursor image change kicking the core between the drain and the
    /// flip looks like.
    pub refuse_busy_once: bool,
    /// Distinct `h_memory` values ever flipped. The C side builds one ISO
    /// context DMA per distinct framebuffer and keeps it across flips, so
    /// this is what `eclipse_rm_hwflip_iso_builds` answers: the healthy
    /// count is the swapchain's depth and then flat, not one per flip.
    pub iso_mems: Vec<u32>,
}

const EMPTY_HWFLIP: FakeHwflip = FakeHwflip {
    init_ok: true,
    ready: false,
    init_calls: 0,
    pending_polls: 0,
    pending_forever: false,
    overpolled: false,
    fetch_polls: 0,
    fetch_delay_us: 0,
    pending_until_us: 0,
    polls: 0,
    surfaces: Vec::new(),
    refused_busy: 0,
    refuse_busy_once: false,
    iso_mems: Vec::new(),
};

pub(super) static FAKE_HWFLIP: lock::Mutex<FakeHwflip> = lock::Mutex::new(EMPTY_HWFLIP);

pub(super) fn reset_fake_hwflip() {
    *FAKE_HWFLIP.lock() = EMPTY_HWFLIP;
}

#[no_mangle]
extern "C" fn eclipse_rm_hwflip_init(_a0: u32, _a1: u32, out: *mut u8) -> u32 {
    let mut f = FAKE_HWFLIP.lock();
    f.init_calls += 1;
    if !f.init_ok {
        return NV_ERR_NOT_SUPPORTED;
    }
    // Seven `NV_STATUS` words, every stage NV_OK, window 0.
    unsafe { core::ptr::write_bytes(out, 0, 7 * 4) };
    f.ready = true;
    0
}
#[no_mangle]
extern "C" fn eclipse_rm_hwflip_ready() -> u8 {
    FAKE_HWFLIP.lock().ready as u8
}
#[no_mangle]
extern "C" fn eclipse_rm_hwflip_pending() -> u8 {
    let mut f = FAKE_HWFLIP.lock();
    f.polls += 1;
    if !f.ready {
        return 0;
    }
    if f.pending_forever {
        // A drain with no bound would sit here for good: give in, and
        // let the test see that it had to.
        if f.polls >= 10_000_000 {
            f.overpolled = true;
            f.pending_forever = false;
            return 0;
        }
        return 1;
    }
    if f.pending_polls > 0 {
        f.pending_polls -= 1;
        return 1;
    }
    if f.pending_until_us != 0 {
        if crate::nvme::nvme_queue::test_clock::now() < f.pending_until_us {
            return 1;
        }
        f.pending_until_us = 0;
    }
    0
}
#[no_mangle]
extern "C" fn eclipse_rm_hwflip_surface(
    _a0: u32,
    h_memory: u32,
    plane_offset: u64,
    width: u32,
    height: u32,
    pitch: u32,
) -> u32 {
    let mut f = FAKE_HWFLIP.lock();
    if !f.ready {
        return NV_ERR_INVALID_STATE;
    }
    // The C side's safety net: nothing is written while the front end
    // still owes a fetch.
    let fetching =
        f.pending_until_us != 0 && crate::nvme::nvme_queue::test_clock::now() < f.pending_until_us;
    if f.pending_forever || f.pending_polls > 0 || fetching || f.refuse_busy_once {
        f.refuse_busy_once = false;
        f.refused_busy += 1;
        return NV_ERR_BUSY_RETRY;
    }
    f.surfaces
        .push((h_memory, plane_offset, width, height, pitch));
    if !f.iso_mems.contains(&h_memory) {
        f.iso_mems.push(h_memory);
    }
    f.pending_polls = f.fetch_polls;
    f.pending_until_us = if f.fetch_delay_us > 0 {
        crate::nvme::nvme_queue::test_clock::now() + f.fetch_delay_us
    } else {
        0
    };
    0
}
#[no_mangle]
extern "C" fn eclipse_rm_hwflip_iso_builds() -> u64 {
    FAKE_HWFLIP.lock().iso_mems.len() as u64
}
#[no_mangle]
extern "C" fn eclipse_rm_init_core() -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_init_gsp(_a0: u32, _a1: *const u8, _a2: u32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_intr_table(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
/// The consumer VAs `map_peer_fence` hands out start here: above every
/// channel's own buffer, and each mapping ever made gets its own, so a
/// stale one can never alias a fresh one by accident.
const PEER_VA_BASE: u64 = 0x7800_0000;
#[no_mangle]
extern "C" fn eclipse_rm_map_peer_fence(
    _inst: u32,
    consumer: u32,
    producer: u32,
    producer_va: u64,
    out: *mut u64,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("map_peer_fence");
    if !f.peer {
        return NV_ERR_NOT_SUPPORTED;
    }
    if consumer == producer {
        unsafe { *out = producer_va };
        return NV_OK;
    }
    // Cached for the life of both contexts, whatever VA is asked for
    // now: the C compares nothing but the pair.
    if let Some(m) = f
        .peer_maps
        .iter()
        .find(|m| m.0 == consumer && m.1 == producer)
    {
        unsafe { *out = m.3 };
        return NV_OK;
    }
    // The producer's buffer and the consumer's VAS must both exist.
    if !f.fast_ctxs.iter().any(|c| c.ctx == producer) || !f.ctxs.contains(&consumer) {
        return NV_ERR_INVALID_STATE;
    }
    f.peer_maps_made += 1;
    let local = PEER_VA_BASE + (u64::from(f.peer_maps_made) << 16) + u64::from(FAST_SEM_OFF);
    f.peer_maps.push((consumer, producer, producer_va, local));
    unsafe { *out = local };
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_mark_console_gpu(_a0: u32, _a1: u64, _a2: u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_state_init(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step10(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step15(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
/// The VAS of the compositor's ladder: context 0's.
pub(super) const LADDER_H_VAS: u32 = 0x5000;
/// Step 16, the compositor's allocation ladder (client, device,
/// subdevice, VAS, TSG, context share). Idempotent: a repeat call
/// answers the cached, still-alive allocation.
#[no_mangle]
extern "C" fn eclipse_rm_step16(_inst: u32, out: *mut GrAlloc) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("step16");
    if !f.ladder {
        return NV_ERR_NOT_SUPPORTED;
    }
    if f.fail_step16 {
        return NV_ERR_INVALID_STATE;
    }
    let ctxshare_status = if f.incomplete_step16 {
        NV_ERR_INVALID_STATE
    } else {
        f.ladder_built = true;
        NV_OK
    };
    unsafe {
        *out = GrAlloc {
            client_status: 0,
            device_status: 0,
            subdev_status: 0,
            vas_status: 0,
            tsg_status: 0,
            ctxshare_status,
            h_client: 0x4000,
            h_device: 0x4001,
            h_subdevice: 0x4002,
            h_vas: LADDER_H_VAS,
            h_tsg: 0x4004,
            h_ctxshare: if ctxshare_status == NV_OK { 0x4005 } else { 0 },
        };
    }
    NV_OK
}
/// Step 17 on the cached ladder: the singleton channel of context 0.
/// Idempotent until `ctx0_reset`; a rebuild is a new channel behind
/// index 0, so a direct-submit window of the old one is refused.
#[no_mangle]
extern "C" fn eclipse_rm_step17(_inst: u32, out: *mut GrChannel) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("step17");
    if !f.ladder {
        return NV_ERR_NOT_SUPPORTED;
    }
    if !f.ladder_built || f.fail_step17 {
        return NV_ERR_INVALID_STATE;
    }
    let sched_status = if f.incomplete_step17 {
        NV_ERR_INVALID_STATE
    } else {
        if !f.chan_built {
            f.chan_built = true;
            f.step17_builds += 1;
            if !f.ctxs.contains(&0) {
                f.ctxs.push(0);
            }
            if let Some(b) = f.ctx_build.iter_mut().find(|b| b.0 == 0) {
                b.1 += 1;
            } else {
                f.ctx_build.push((0, 1));
            }
        }
        NV_OK
    };
    unsafe {
        *out = GrChannel {
            userd_status: 0,
            buf_status: 0,
            virt_status: 0,
            map_status: 0,
            notif_status: 0,
            chan_status: 0,
            compute_status: 0,
            sched_status,
            h_userd: 0x4100,
            h_phys_buf: 0x4101,
            h_virt_buf: 0x4102,
            h_notifier: 0x6000,
            h_channel: 0x7000,
            h_compute: 0x4106,
            channel_class: 0xc46f,
            userd_size: 0x100,
            buf_gpu_va: 0x8_0000,
        };
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_step18(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step19(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step20(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step21(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step22(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step23(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_step8(_a0: u32, _a1: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}

// ----- A fake RM with state, for the arms that cannot run without one -----
//
// Enough of `eclipse_rm_*` to walk the path a GL client takes: a context
// per pid (`ctx_alloc`/`ctx_prime`/`ctx_free`), GEM memory
// (`gem_alloc`/`gem_map_cpu`/`gem_fbmem_offset`/`gem_free`), VA bindings
// (`vm_bind_map`/`vm_bind_unmap`) and engine classes
// (`class_alloc`/`class_free`). It refuses what the real RM refuses (a
// fixed VA over a live range of the same VAS, a memory handle it never
// handed out) and counts every free or unmap of something it does not
// hold, which on hardware is a use-after-free inside the vendor RM. The
// compositor's own ladder (`step16`/`step17`) is not faked: the
// compositor's channel keeps answering ENODEV, and every test client is
// a GL client with a context of its own.
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use nvidia_rm_sys::rm_init::{
    CtxAlloc, ExecFast, ExecSignal, ExecSubmit, GemAlloc, GemMapCpu, GrAlloc, GrChannel, VmBind,
    ADDR_SYSMEM,
};

const NV_OK: u32 = 0;
const NV_ERR_BUSY_RETRY: u32 = nvidia_rm_sys::types::NV_ERR_BUSY_RETRY;

/// The channel's fence semaphore. The driver polls it through
/// `phys_to_virt(fence_sem_phys)`, which is the identity in this binary,
/// so its "physical" address is simply its address.
static FENCE_SEM: AtomicU32 = AtomicU32::new(0);
/// `NV_ERR_OBJECT_NOT_FOUND`.
const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x57;
/// What the RM's eheap answers to a fixed-address allocation over a
/// live range: the status the VM_BIND replace semantics were written for.
const RM_VA_TAKEN: u32 = 0x51;

pub(super) struct FakeRm {
    next: u32,
    /// Live context indices.
    pub ctxs: Vec<u32>,
    pub primed: Vec<u32>,
    /// `(h_memory, size, sysmem)`.
    pub gems: Vec<(u32, u64, bool)>,
    /// The host memory behind each sysmem object: `(h_memory, address)`.
    /// Real, because `phys_to_virt` is the identity here and the driver
    /// reads the first pushbuffer through it.
    bufs: Vec<(u32, usize)>,
    /// Every submit, in order: `(ctx, push_va, push_len, fence payload)`.
    pub submits: Vec<(u32, u64, u32, Option<u32>)>,
    /// `(h_virt, ctx, h_memory, va, size, bo_offset, pte_kind)`.
    pub maps: Vec<(u32, u32, u32, u64, u64, u64, u32)>,
    /// `(h_object, ctx, class)`.
    pub classes: Vec<(u32, u32, u32)>,
    pub unmaps: usize,
    pub gem_frees: usize,
    pub ctx_frees: usize,
    pub class_frees: usize,
    /// Frees and unmaps of something this RM does not hold.
    pub bad: usize,
    /// Every entry point in call order.
    pub calls: Vec<&'static str>,
    pub fail_ctx_alloc: bool,
    /// `ctx_alloc` returns `NV_OK` with a non-zero `sched_status`: the
    /// C side's shape for a ladder that failed part-way. It has already
    /// freed every handle it made, so nothing of this context lives.
    pub incomplete_ctx: bool,
    pub fail_prime: bool,
    /// `ctx_free` refuses: the context, and its VAS, stay alive.
    pub fail_ctx_free: bool,
    /// `gem_alloc` returns `NV_OK` with `alloc_status` set: the C side's
    /// shape for an allocation the RM refused.
    pub fail_gem_alloc: bool,
    /// `gem_map_cpu` answers with an aperture that is not system memory:
    /// the object exists but the CPU cannot map it.
    pub map_cpu_elsewhere: bool,
    pub refuse_map: bool,
    pub refuse_class: bool,
    /// The GPFIFO has no room: `submit_status = NV_ERR_BUSY_RETRY`.
    pub ring_full: bool,
    /// The submit goes through but the fence never lands.
    pub fence_stalls: bool,
    /// `exec_fast_prepare` hands out a channel instead of
    /// `NV_ERR_NOT_SUPPORTED`: EXEC takes the direct-submit path.
    pub fast: bool,
    /// The SDK's `SEM_EXECUTE` disagrees with the kernel's encoder.
    pub fast_bad_encoding: bool,
    /// The direct-submit channels handed out and not released.
    pub fast_ctxs: Vec<FastChan>,
    /// Every `exec_fast_release` that found a window, in order.
    pub fast_releases: Vec<u32>,
    /// `(ctx, build)`: how many times each index was built.
    ctx_build: Vec<(u32, u32)>,
    /// `map_peer_fence` maps instead of answering `NV_ERR_NOT_SUPPORTED`.
    pub peer: bool,
    /// The live peer-fence mappings: `(consumer, producer, producer VA,
    /// consumer VA)`. Dropped with either context, as the C does.
    pub peer_maps: Vec<(u32, u32, u64, u64)>,
    /// Mappings ever made: each gets a consumer VA of its own.
    peer_maps_made: u32,
    /// `step16`/`step17` build the compositor's ladder instead of
    /// answering `NV_ERR_NOT_SUPPORTED`.
    pub ladder: bool,
    pub fail_step16: bool,
    /// `step16` returns `NV_OK` with the context share unallocated:
    /// the C side's shape for a ladder that stopped part-way.
    pub incomplete_step16: bool,
    pub fail_step17: bool,
    /// `step17` returns `NV_OK` with the channel never scheduled.
    pub incomplete_step17: bool,
    /// Step 16 ran to the end (`g_grAllocDone`).
    ladder_built: bool,
    /// Step 17's channel stands (`g_grChanDone`): cleared by `ctx0_reset`.
    pub chan_built: bool,
    /// How many channels step 17 built behind index 0.
    pub step17_builds: u32,
    /// How many times `ctx0_reset` found a channel to tear down.
    pub ctx0_resets: u32,
}

const EMPTY_RM: FakeRm = FakeRm {
    next: 0x100,
    ctxs: Vec::new(),
    primed: Vec::new(),
    gems: Vec::new(),
    bufs: Vec::new(),
    submits: Vec::new(),
    maps: Vec::new(),
    classes: Vec::new(),
    unmaps: 0,
    gem_frees: 0,
    ctx_frees: 0,
    class_frees: 0,
    bad: 0,
    calls: Vec::new(),
    fail_ctx_alloc: false,
    incomplete_ctx: false,
    fail_prime: false,
    fail_ctx_free: false,
    fail_gem_alloc: false,
    map_cpu_elsewhere: false,
    refuse_map: false,
    refuse_class: false,
    ring_full: false,
    fence_stalls: false,
    fast: false,
    fast_bad_encoding: false,
    fast_ctxs: Vec::new(),
    fast_releases: Vec::new(),
    ctx_build: Vec::new(),
    peer: false,
    peer_maps: Vec::new(),
    peer_maps_made: 0,
    ladder: false,
    fail_step16: false,
    incomplete_step16: false,
    fail_step17: false,
    incomplete_step17: false,
    ladder_built: false,
    chan_built: false,
    step17_builds: 0,
    ctx0_resets: 0,
};

pub(super) static FAKE_RM: lock::Mutex<FakeRm> = lock::Mutex::new(EMPTY_RM);

pub(super) fn reset_fake_rm() {
    *FAKE_RM.lock() = EMPTY_RM;
}

impl FakeRm {
    fn fresh(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    fn gem(&self, h_memory: u32) -> Option<(u32, u64, bool)> {
        self.gems.iter().copied().find(|g| g.0 == h_memory)
    }

    /// Whether `[va, va + size)` meets a live range of context `ctx`.
    fn va_taken(&self, ctx: u32, va: u64, size: u64) -> bool {
        self.maps
            .iter()
            .any(|m| m.1 == ctx && m.3 < va.wrapping_add(size) && va < m.3.wrapping_add(m.4))
    }

    /// The host address of a sysmem object's memory.
    pub fn pa_of(&self, h_memory: u32) -> Option<u64> {
        self.bufs
            .iter()
            .find(|b| b.0 == h_memory)
            .map(|b| b.1 as u64)
    }

    /// Whether `[va, va + len)` lies inside one binding of context
    /// `ctx`: what the RM's own lookup answers before it rings the
    /// doorbell.
    fn push_mapped(&self, ctx: u32, va: u64, len: u32) -> bool {
        self.maps.iter().any(|m| {
            m.1 == ctx && va >= m.3 && va.wrapping_add(u64::from(len)) <= m.3.wrapping_add(m.4)
        })
    }

    /// One submit through the fake: the RM's lookup of the push VA in
    /// the context's VAS, then the ring. Returns the four stage statuses
    /// of `ExecSubmit` the way the C side reports them: a stage that
    /// was never reached stays at `0xFFFF_FFFF`.
    fn submit(&mut self, ctx: u32, va: u64, len: u32, payload: Option<u32>) -> [u32; 4] {
        const UNREACHED: u32 = 0xFFFF_FFFF;
        self.submits.push((ctx, va, len, payload));
        if !self.push_mapped(ctx, va, len) {
            return [NV_ERR_OBJECT_NOT_FOUND, UNREACHED, UNREACHED, UNREACHED];
        }
        if self.ring_full {
            return [0, 0, 0, NV_ERR_BUSY_RETRY];
        }
        [0, 0, 0, 0]
    }

    pub fn maps_of_ctx(&self, ctx: u32) -> Vec<(u64, u64, u32)> {
        self.maps
            .iter()
            .filter(|m| m.1 == ctx)
            .map(|m| (m.3, m.4, m.6))
            .collect()
    }
}

#[no_mangle]
extern "C" fn eclipse_rm_ctx_alloc(_inst: u32, ctx_idx: u32, out: *mut CtxAlloc) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("ctx_alloc");
    if f.fail_ctx_alloc {
        return NV_ERR_NOT_SUPPORTED;
    }
    let sched_status = if f.incomplete_ctx {
        NV_ERR_NOT_SUPPORTED
    } else {
        if !f.ctxs.contains(&ctx_idx) {
            f.ctxs.push(ctx_idx);
            // A new channel behind this index: the USERD of the old one
            // went with it.
            if let Some(b) = f.ctx_build.iter_mut().find(|b| b.0 == ctx_idx) {
                b.1 += 1;
            } else {
                f.ctx_build.push((ctx_idx, 1));
            }
        }
        NV_OK
    };
    unsafe {
        *out = CtxAlloc {
            vas_status: 0,
            tsg_status: 0,
            ctxshare_status: 0,
            userd_status: 0,
            buf_status: 0,
            virt_status: 0,
            map_status: 0,
            notif_status: 0,
            chan_status: 0,
            compute_status: 0,
            sched_status,
            h_vas: 0x5000 + ctx_idx,
            h_tsg: 0,
            h_ctxshare: 0,
            h_userd: 0,
            h_phys_buf: 0,
            h_virt_buf: 0,
            h_notifier: 0x6000 + ctx_idx,
            h_channel: 0x7000 + ctx_idx,
            h_compute: 0,
            channel_class: 0xc46f,
            userd_size: 0,
            buf_gpu_va: 0x10_0000 * u64::from(ctx_idx),
        };
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_ctx_prime(_inst: u32, ctx_idx: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("ctx_prime");
    if f.fail_prime {
        return NV_ERR_NOT_SUPPORTED;
    }
    f.primed.push(ctx_idx);
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_ctx_free(_inst: u32, ctx_idx: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("ctx_free");
    if f.fail_ctx_free {
        return NV_ERR_NOT_SUPPORTED;
    }
    // Destroying the VAS takes every binding in it with it.
    if let Some(pos) = f.ctxs.iter().position(|c| *c == ctx_idx) {
        f.ctxs.remove(pos);
        f.maps.retain(|m| m.1 != ctx_idx);
        // And every peer-fence mapping it takes part in, as the C does.
        f.peer_maps.retain(|m| m.0 != ctx_idx && m.1 != ctx_idx);
        f.ctx_frees += 1;
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_gem_alloc(_inst: u32, size: u64, sysmem: u32, out: *mut GemAlloc) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("gem_alloc");
    if f.fail_gem_alloc {
        unsafe {
            *out = GemAlloc {
                alloc_status: NV_ERR_NOT_SUPPORTED,
                h_memory: 0,
            };
        }
        return NV_OK;
    }
    let h = f.fresh();
    f.gems.push((h, size, sysmem != 0));
    if sysmem != 0 {
        let buf = alloc::boxed::Box::leak(alloc::vec![0u8; size as usize].into_boxed_slice());
        f.bufs.push((h, buf.as_ptr() as usize));
    }
    unsafe {
        *out = GemAlloc {
            alloc_status: 0,
            h_memory: h,
        };
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_gem_map_cpu(_inst: u32, h_memory: u32, out: *mut GemMapCpu) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("gem_map_cpu");
    let elsewhere = f.map_cpu_elsewhere;
    let r = match f.gem(h_memory) {
        Some((_, size, true)) if elsewhere => GemMapCpu {
            lookup_status: 0,
            address_space: 2,
            phys_addr: 0xdead_0000,
            size,
        },
        // A host PA only for system memory; vidmem is FBMEM (0).
        Some((h, size, true)) => GemMapCpu {
            lookup_status: 0,
            address_space: ADDR_SYSMEM,
            phys_addr: f.pa_of(h).expect("a sysmem object has memory"),
            size,
        },
        Some((_, _, false)) => GemMapCpu {
            lookup_status: 0,
            address_space: 0,
            phys_addr: 0,
            size: 0,
        },
        None => GemMapCpu {
            lookup_status: NV_ERR_OBJECT_NOT_FOUND,
            address_space: 0,
            phys_addr: 0,
            size: 0,
        },
    };
    unsafe { *out = r };
    NV_OK
}
/// The host PA the fake gives a sysmem object.
/// The FBMEM offset the fake gives a vidmem object.
pub(super) fn fake_fbmem_offset(h_memory: u32) -> u64 {
    u64::from(h_memory) << 20
}
#[no_mangle]
extern "C" fn eclipse_rm_gem_fbmem_offset(_inst: u32, h_memory: u32, p_offset: *mut u64) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("gem_fbmem_offset");
    match f.gem(h_memory) {
        Some((h, _, false)) => {
            unsafe { *p_offset = fake_fbmem_offset(h) };
            NV_OK
        }
        _ => NV_ERR_NOT_SUPPORTED,
    }
}
#[no_mangle]
extern "C" fn eclipse_rm_gem_free(_inst: u32, h_memory: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("gem_free");
    match f.gems.iter().position(|g| g.0 == h_memory) {
        Some(pos) => {
            f.gems.remove(pos);
            f.gem_frees += 1;
            NV_OK
        }
        None => {
            f.bad += 1;
            NV_ERR_OBJECT_NOT_FOUND
        }
    }
}
#[no_mangle]
extern "C" fn eclipse_rm_vm_bind_map(
    _inst: u32,
    ctx_idx: u32,
    h_memory: u32,
    size: u64,
    requested_va: u64,
    bo_offset: u64,
    pte_kind: u32,
    out: *mut VmBind,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("vm_bind_map");
    let r = if f.gem(h_memory).is_none() {
        VmBind {
            virt_status: 0,
            map_status: NV_ERR_OBJECT_NOT_FOUND,
            h_virt: 0,
            actual_va: 0,
        }
    } else if f.refuse_map || f.va_taken(ctx_idx, requested_va, size) {
        VmBind {
            virt_status: RM_VA_TAKEN,
            map_status: RM_VA_TAKEN,
            h_virt: 0,
            actual_va: 0,
        }
    } else {
        let h_virt = f.fresh();
        f.maps.push((
            h_virt,
            ctx_idx,
            h_memory,
            requested_va,
            size,
            bo_offset,
            pte_kind,
        ));
        VmBind {
            virt_status: 0,
            map_status: 0,
            h_virt,
            actual_va: requested_va,
        }
    };
    unsafe { *out = r };
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_vm_bind_unmap(_inst: u32, h_virt: u32, _size: u64, _va: u64) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("vm_bind_unmap");
    f.unmaps += 1;
    match f.maps.iter().position(|m| m.0 == h_virt) {
        Some(pos) => {
            f.maps.remove(pos);
            NV_OK
        }
        None => {
            f.bad += 1;
            NV_ERR_OBJECT_NOT_FOUND
        }
    }
}
#[no_mangle]
extern "C" fn eclipse_rm_class_alloc(
    _inst: u32,
    ctx_idx: u32,
    class_id: u32,
    h_object: *mut u32,
    alloc_status: *mut u32,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("class_alloc");
    if f.refuse_class {
        unsafe {
            *h_object = 0;
            *alloc_status = NV_ERR_NOT_SUPPORTED;
        }
        return NV_OK;
    }
    let h = f.fresh();
    f.classes.push((h, ctx_idx, class_id));
    unsafe {
        *h_object = h;
        *alloc_status = 0;
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_class_free(_inst: u32, h_object: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("class_free");
    match f.classes.iter().position(|c| c.0 == h_object) {
        Some(pos) => {
            f.classes.remove(pos);
            f.class_frees += 1;
            NV_OK
        }
        None => {
            f.bad += 1;
            NV_ERR_OBJECT_NOT_FOUND
        }
    }
}
#[no_mangle]
extern "C" fn eclipse_rm_exec_submit(
    _inst: u32,
    ctx_idx: u32,
    push_va: u64,
    push_len: u32,
    out: *mut ExecSubmit,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("exec_submit");
    let [lookup, map, token, submit] = f.submit(ctx_idx, push_va, push_len, None);
    unsafe {
        *out = ExecSubmit {
            lookup_status: lookup,
            map_status: map,
            token_status: token,
            submit_status: submit,
            work_token: 0x1000 + ctx_idx,
            runlist_id: 0,
            gp_put_after: f.submits.len() as u32,
        };
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_exec_submit_signaled(
    _inst: u32,
    ctx_idx: u32,
    push_va: u64,
    push_len: u32,
    fence_payload: u32,
    _timeout_ms: u32,
    out: *mut ExecSignal,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("exec_submit_signaled");
    let [lookup, map, token, submit] = f.submit(ctx_idx, push_va, push_len, Some(fence_payload));
    let submitted = lookup == 0 && submit == 0;
    // The GPU "runs" the push before the call returns: the fence lands
    // unless told to stall. The driver polls it itself (async path).
    if submitted && !f.fence_stalls {
        FENCE_SEM.store(fence_payload, Ordering::Release);
    }
    unsafe {
        *out = ExecSignal {
            lookup_status: lookup,
            map_status: map,
            token_status: token,
            submit_status: submit,
            fence_submit_status: if submitted { 0 } else { 0xFFFF_FFFF },
            fence_wait_status: 0xFFFF_FFFF,
            fence_value: 0,
            work_token: 0x1000 + ctx_idx,
            runlist_id: 0,
            fence_sem_phys: if submitted {
                &FENCE_SEM as *const AtomicU32 as u64
            } else {
                0
            },
        };
    }
    NV_OK
}
