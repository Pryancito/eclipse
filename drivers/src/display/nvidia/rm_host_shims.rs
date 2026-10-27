const NV_ERR_NOT_SUPPORTED: u32 = 0x56;

/// `gpumgrAttachGpu` (`gpustep5`): with `attach = Some(di)` the card
/// attaches as RM device instance `di` and what the driver told the RM
/// about it -- PCI location, the three BARs, the VRAM size it passes as
/// BAR1's length -- is kept in `attached`. Without the knob, the stub's
/// refusal.
#[no_mangle]
extern "C" fn eclipse_rm_attach_gpu(
    domain: u32,
    bus: u8,
    device: u8,
    bar0_phys: u64,
    _bar0_virt: *mut u8,
    bar0_len: u64,
    bar1_phys: u64,
    bar1_len: u64,
    bar2_phys: u64,
    bar2_len: u64,
    out_device_instance: *mut u32,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("attach_gpu");
    f.narrate("attach_gpu");
    let Some(di) = f.attach else {
        return NV_ERR_NOT_SUPPORTED;
    };
    f.attached.push(Attached {
        domain,
        bus,
        device,
        bar0_phys,
        bar0_len,
        bar1_phys,
        bar1_len,
        bar2_phys,
        bar2_len,
    });
    unsafe { *out_device_instance = di };
    NV_OK
}

/// What one `eclipse_rm_attach_gpu` was told, field for field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Attached {
    pub domain: u32,
    pub bus: u8,
    pub device: u8,
    pub bar0_phys: u64,
    pub bar0_len: u64,
    pub bar1_phys: u64,
    pub bar1_len: u64,
    pub bar2_phys: u64,
    pub bar2_len: u64,
}
/// The GIOPS bench: 2176 threads of a million IMADs each, 435.2 ms on
/// the PTIMER, which is 5.000 GIOPS; `bench_zero_time` is a grid that
/// ran with both timestamps at 0.
#[no_mangle]
extern "C" fn eclipse_rm_bench(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrBench) -> u32 {
    let Some(ran) = rung("bench") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    let (_, sem) = waits(ran);
    let sem = if sem == NOT_REACHED {
        NV_ERR_TIMEOUT
    } else {
        sem
    };
    let zero = FAKE_RM.lock().bench_zero_time;
    let t1 = if zero || sem != NV_OK {
        0
    } else {
        1000 + 435_200_000
    };
    unsafe {
        *out = nvidia_rm_sys::rm_init::GrBench {
            lookup_status: NV_OK,
            map_status: NV_OK,
            token_status: NV_OK,
            submit_status: NV_OK,
            sem_status: sem,
            num_threads: 2176,
            imads_per_thread: 1_000_000,
            push_dwords: 24,
            sem_iters: 9,
            reserved_pad: 0,
            t0_ns: if t1 == 0 { 0 } else { 1000 },
            t1_ns: t1,
            elapsed_ns: t1.saturating_sub(1000),
            total_ops: 2_176_000_000,
            kernel_va: COMPUTE_KERNEL_VA,
            qmd_va: COMPUTE_QMD_VA,
        };
    }
    NV_OK
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
    if f.fail_ctx0_reset {
        return NV_ERR_INVALID_STATE;
    }
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
/// The GSP's static info (`gpustep7`): `gsp_info = Some(name)` is a card
/// the firmware calls `name` -- cut at the 64 bytes the C struct holds,
/// with no terminator when it fills them -- with the short name, VRAM,
/// L2 and VBIOS identity of the fake's TU106 (`GSP_*`); an empty name is
/// the all-zero struct the RPC leaves until step 6 ran. Without the knob,
/// the stub's refusal.
#[no_mangle]
extern "C" fn eclipse_rm_get_gsp_info(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GspInfo) -> u32 {
    let name = {
        let mut f = FAKE_RM.lock();
        f.calls.push("get_gsp_info");
        f.narrate("get_gsp_info");
        f.gsp_info
    };
    let Some(name) = name else {
        return NV_ERR_NOT_SUPPORTED;
    };
    let mut info = nvidia_rm_sys::rm_init::GspInfo {
        gpu_name: [0; 64],
        gpu_short_name: [0; 64],
        fb_length: 0,
        fb_bus_width: 0,
        fb_ram_type: 0,
        l2_cache_size: 0,
        vbios_valid: 0,
        vbios_sub_vendor: 0,
        vbios_sub_device: 0,
    };
    if !name.is_empty() {
        let n = name.len().min(64);
        info.gpu_name[..n].copy_from_slice(&name.as_bytes()[..n]);
        info.gpu_short_name[..GSP_SHORT_NAME.len()].copy_from_slice(GSP_SHORT_NAME.as_bytes());
        info.fb_length = GSP_FB_BYTES;
        info.fb_bus_width = GSP_BUS_WIDTH;
        info.fb_ram_type = GSP_RAM_TYPE;
        info.l2_cache_size = GSP_L2_BYTES;
        info.vbios_valid = 1;
        info.vbios_sub_vendor = GSP_SUB_VENDOR;
        info.vbios_sub_device = GSP_SUB_DEVICE;
    }
    unsafe { *out = info };
    NV_OK
}

/// The fake's card as its firmware describes it: a TU106 with 8 GiB on a
/// 256-bit bus, 4 MiB of L2, and a board vendor's VBIOS.
const GSP_SHORT_NAME: &str = "TU106";
const GSP_FB_BYTES: u64 = 8 << 30;
const GSP_BUS_WIDTH: u32 = 256;
/// The RM's own enum value; the printer passes it through as a number.
const GSP_RAM_TYPE: u32 = 11;
const GSP_L2_BYTES: u32 = 4 << 20;
const GSP_SUB_VENDOR: u32 = 0x1458;
const GSP_SUB_DEVICE: u32 = 0x3fe2;
/// What `GET_NAME_STRING` answers, and the GID the firmware hands out.
const RM_API_NAME: &str = "NVIDIA GeForce RTX 2060 SUPER";
const RM_API_GID: &str = "GPU-6f3c9a1e-2b4d-4e8f-9a7c-0d1e2f3a4b5c";
#[no_mangle]
extern "C" fn eclipse_rm_hdmi_audio(_a0: u32, _a1: u32, _a2: u32, _a3: u8, _a4: *mut u8) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_hide(_a0: u32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
/// The cursor image upload, as the `hwcursor` knob's third status says.
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_image(_a0: u32, _a1: *const u8, _a2: u32, _a3: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("hwcursor_image");
    match f.hwcursor {
        Some((_, _, image_status)) => image_status,
        None => NV_ERR_NOT_SUPPORTED,
    }
}
/// The display cursor ladder: the `hwcursor` knob's init status, and its
/// head count written where `HwCursorInit::num_heads` is (the eleventh
/// word; the ten status words before it are the caller's own fill).
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_init(_a0: u32, _a1: u32, out: *mut u8) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("hwcursor_init");
    let Some((init_status, num_heads, _)) = f.hwcursor else {
        return NV_ERR_NOT_SUPPORTED;
    };
    unsafe { (out as *mut u32).add(10).write_unaligned(num_heads) };
    init_status
}
#[no_mangle]
extern "C" fn eclipse_rm_hwcursor_move(_gpu: u32, _a0: i32, _a1: i32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_gem_client(_gpu: u32, _ctx_idx: u32, h_client: *mut u32) -> u32 {
    unsafe { *h_client = 0 };
    NV_ERR_NOT_SUPPORTED
}
#[no_mangle]
extern "C" fn eclipse_rm_write_memory(
    _h_client: u32,
    _h_memory: u32,
    _offset: u64,
    _src: *const u8,
    _len: u64,
) -> u32 {
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
extern "C" fn eclipse_rm_hwflip_ready(_gpu: u32) -> u8 {
    FAKE_HWFLIP.lock().ready as u8
}
#[no_mangle]
extern "C" fn eclipse_rm_hwflip_pending(_gpu: u32) -> u8 {
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
extern "C" fn eclipse_rm_hwflip_iso_builds(_gpu: u32) -> u64 {
    FAKE_HWFLIP.lock().iso_mems.len() as u64
}
/// The RM core's construction: `core` says it stands; otherwise the
/// stub's refusal. Either way it is asked at most once per process
/// (`rm_core_init_once`), so a test that wants it asked again resets
/// that cache itself.
#[no_mangle]
extern "C" fn eclipse_rm_init_core() -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("init_core");
    f.narrate("init_core");
    if f.core {
        NV_OK
    } else {
        NV_ERR_NOT_SUPPORTED
    }
}
#[no_mangle]
extern "C" fn eclipse_rm_init_gsp(_a0: u32, _a1: *const u8, _a2: u32) -> u32 {
    NV_ERR_NOT_SUPPORTED
}
/// The GSP's interrupt kernel table, as the `intr_table` knob says it:
/// the control's own status and the rows, cut at the 128 the mirror
/// holds. Without the knob, `NV_ERR_NOT_SUPPORTED`, like every other
/// probe of a GPU this fake has no silicon for.
#[no_mangle]
extern "C" fn eclipse_rm_intr_table(_a0: u32, out: *mut nvidia_rm_sys::rm_init::IntrTable) -> u32 {
    let f = FAKE_RM.lock();
    f.narrate("intr_table");
    let Some((ctrl_status, rows)) = f.intr_table.as_ref() else {
        return NV_ERR_NOT_SUPPORTED;
    };
    unsafe {
        (*out).ctrl_status = *ctrl_status;
        let n = rows.len().min((*out).entries.len());
        (*out).table_len = n as u32;
        for (slot, &(engine_idx, pmc_intr_mask, vector_stall, vector_non_stall)) in
            (*out).entries.iter_mut().zip(rows.iter())
        {
            *slot = nvidia_rm_sys::rm_init::IntrTableEntry {
                engine_idx,
                pmc_intr_mask,
                vector_stall,
                vector_non_stall,
            };
        }
    }
    NV_OK
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
/// The console-GPU declaration: `console_mark = Some(status)` answers
/// `status` and keeps what was declared in `console_marks`; without the
/// knob, the stub's refusal.
#[no_mangle]
extern "C" fn eclipse_rm_mark_console_gpu(di: u32, console_size: u64, at_bar1_base: u8) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("mark_console_gpu");
    f.narrate("mark_console_gpu");
    let Some(status) = f.console_mark else {
        return NV_ERR_NOT_SUPPORTED;
    };
    f.console_marks.push((di, console_size, at_bar1_base != 0));
    status
}
/// `gpuStatePreInit`/`StateInit`/`StateLoad` (`gpustep9`): `state_init =
/// Some((pre, init, load))` is the status of each phase, `0xFFFF_FFFF`
/// for one never reached; without the knob, the stub's refusal.
#[no_mangle]
extern "C" fn eclipse_rm_state_init(
    _a0: u32,
    out: *mut nvidia_rm_sys::rm_init::StateInitResult,
) -> u32 {
    let phases = {
        let mut f = FAKE_RM.lock();
        f.calls.push("state_init");
        f.narrate("state_init");
        f.state_init
    };
    let Some((pre_init_status, init_status, load_status)) = phases else {
        return NV_ERR_NOT_SUPPORTED;
    };
    unsafe {
        *out = nvidia_rm_sys::rm_init::StateInitResult {
            pre_init_status,
            init_status,
            load_status,
        };
    }
    NV_OK
}
/// Step 10, the copy-engine test: two 1 MiB buffers, the memsets always
/// land; `Submitted` is a copy that timed out, `Fenced`/`Completed` a
/// copy whose readback has three wrong dwords, the first at index 7.
#[no_mangle]
extern "C" fn eclipse_rm_step10(_a0: u32, out: *mut nvidia_rm_sys::rm_init::Step10Result) -> u32 {
    let Some(ran) = rung("step10") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    let (copy, verify) = match ran {
        Ran::Submitted => (NV_ERR_TIMEOUT, NOT_REACHED),
        Ran::Fenced | Ran::Completed => (NV_OK, NV_ERR_INVALID_STATE),
        Ran::Verified => (NV_OK, NV_OK),
    };
    let checked = verify != NOT_REACHED;
    unsafe {
        *out = nvidia_rm_sys::rm_init::Step10Result {
            ce_utils_status: NV_OK,
            alloc_a_status: NV_OK,
            alloc_b_status: NV_OK,
            poison_status: NV_OK,
            memset_status: NV_OK,
            copy_status: copy,
            verify_status: verify,
            buffer_size: 1 << 20,
            pa_a: 0x1000_0000,
            pa_b: 0x1010_0000,
            pattern: 0xec11_05a5,
            poison: 0xdead_beef,
            dwords_checked: if checked { 262_144 } else { 0 },
            mismatch_count: if verify == NV_ERR_INVALID_STATE { 3 } else { 0 },
            first_mismatch_idx: if verify == NV_ERR_INVALID_STATE { 7 } else { 0 },
            first_mismatch_val: 0,
        };
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_step15(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrProbe) -> u32 {
    // Not recorded in `calls`: a GRAPH_UNITS answer is no RM work the
    // tests count, and the ones that count calls ask for it in passing.
    let f = FAKE_RM.lock();
    f.narrate("step15");
    let Some((num_gpc, total_tpc)) = f.gr_probe else {
        return NV_ERR_NOT_SUPPORTED;
    };
    // The mask names the first `num_gpc` GPCs and the TPCs split evenly
    // across them, the remainder on the lowest: what `/proc/gpustep15`
    // prints per GPC, which GRAPH_UNITS never looks at. A probe with no
    // GPCs in it is a GPC-mask control that failed: the shape the C side
    // has for "no answer", which both readers treat as none.
    let gpc_mask = if num_gpc >= 32 {
        u32::MAX
    } else {
        (1u32 << num_gpc) - 1
    };
    unsafe {
        (*out).gpc_mask_status = if num_gpc == 0 {
            NV_ERR_INVALID_STATE
        } else {
            0
        };
        (*out).gpc_mask = gpc_mask;
        (*out).tpc_mask_status = 0;
        (*out).num_gpc = num_gpc;
        (*out).total_tpc = total_tpc;
        (*out).per_gpc_tpc = [0; 8];
        for g in 0..(num_gpc.min(8) as usize) {
            (*out).per_gpc_tpc[g] =
                total_tpc / num_gpc + u32::from((g as u32) < total_tpc % num_gpc);
        }
    }
    NV_OK
}
/// Whether the ladder has been built, the C side's `g_grAllocDone`. Sticky:
/// freeing every channel does not take it back down.
#[no_mangle]
extern "C" fn eclipse_rm_gr_ready(inst: u32) -> u8 {
    let mut f = FAKE_RM.lock();
    f.gr_ready_asked_of = Some(inst);
    u8::from(f.ladder_built)
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
    f.narrate("step16");
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
    f.narrate("step17");
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
// ----- The compute rungs (steps 18..23), the bench and step 10 -----
//
// One knob, `FakeRm::compute`, says how far each of them got; the per-stage
// statuses the C side fills are derived from that, in the shape its
// `EclipseGr*` structs have: `0xFFFF_FFFF` for a stage never reached,
// `NV_ERR_TIMEOUT` for the wait that gave up, `NV_ERR_INVALID_STATE` for a
// verification that found the wrong answer. `None` is the stub's refusal.

/// How far a compute rung got. Each later state implies the earlier
/// stages passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Ran {
    /// The doorbell rang and nothing landed: the first wait timed out.
    Submitted,
    /// The host side landed (the PBDMA semaphore, the post-PCAS fence,
    /// step 10's copy) and the engine never answered: the second wait
    /// timed out.
    Fenced,
    /// The engine answered and the result is wrong (the stored dword, a
    /// thread's slot, a copied dword).
    Completed,
    /// Everything verified.
    Verified,
}

const NOT_REACHED: u32 = 0xFFFF_FFFF;
const NV_ERR_TIMEOUT: u32 = nvidia_rm_sys::types::NV_ERR_TIMEOUT;
/// The work-submit token and the runlist the fake's channel sits on.
const COMPUTE_TOKEN: u32 = 0x0008_0000;
const COMPUTE_KERNEL_VA: u64 = 0x9_0000;
const COMPUTE_QMD_VA: u64 = 0x9_1000;
const COMPUTE_OUT_VA: u64 = 0x9_2000;

/// Records and narrates the call, and answers the knob.
fn rung(call: &'static str) -> Option<Ran> {
    let mut f = FAKE_RM.lock();
    f.calls.push(call);
    f.narrate(call);
    f.compute
}

/// `(first wait, second wait)` statuses for `ran`.
fn waits(ran: Ran) -> (u32, u32) {
    match ran {
        Ran::Submitted => (NV_ERR_TIMEOUT, NOT_REACHED),
        Ran::Fenced => (NV_OK, NV_ERR_TIMEOUT),
        Ran::Completed | Ran::Verified => (NV_OK, NV_OK),
    }
}

/// The verification status for `ran`: not reached until the engine
/// answered, wrong on `Completed`, right on `Verified`.
fn verdict(ran: Ran) -> u32 {
    match ran {
        Ran::Submitted | Ran::Fenced => NOT_REACHED,
        Ran::Completed => NV_ERR_INVALID_STATE,
        Ran::Verified => NV_OK,
    }
}

#[no_mangle]
extern "C" fn eclipse_rm_step18(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrLaunch) -> u32 {
    let Some(ran) = rung("step18") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    let (host, eng) = waits(ran);
    unsafe {
        *out = nvidia_rm_sys::rm_init::GrLaunch {
            lookup_status: NV_OK,
            map_status: NV_OK,
            token_status: NV_OK,
            submit_status: NV_OK,
            host_sem_status: host,
            eng_sem_status: eng,
            work_token: COMPUTE_TOKEN,
            runlist_id: 0,
            host_sem_value: if host == NV_OK { 0x1818_0001 } else { 0 },
            eng_sem_value: if eng == NV_OK { 0x1818_0002 } else { 0 },
            host_poll_iters: 3,
            eng_poll_iters: if eng == NOT_REACHED { 0 } else { 7 },
            push_dwords: 12,
        };
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_step19(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrCompute) -> u32 {
    let Some(ran) = rung("step19") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    let (fence, sem) = waits(ran);
    unsafe {
        *out = nvidia_rm_sys::rm_init::GrCompute {
            lookup_status: NV_OK,
            map_status: NV_OK,
            token_status: NV_OK,
            submit_status: NV_OK,
            fence_status: fence,
            sem_status: sem,
            work_token: COMPUTE_TOKEN,
            runlist_id: 0,
            fence_value: if fence == NV_OK { 0x19 } else { 0 },
            fence_iters: 2,
            sem_value: if sem == NV_OK { 0x1919 } else { 0 },
            poll_iters: if sem == NOT_REACHED { 0 } else { 5 },
            push_dwords: 40,
            reserved_pad: 0,
            kernel_va: COMPUTE_KERNEL_VA,
            qmd_va: COMPUTE_QMD_VA,
        };
    }
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_step20(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrStore) -> u32 {
    let Some(ran) = rung("step20") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    let (fence, sem) = waits(ran);
    let store = verdict(ran);
    unsafe {
        *out = nvidia_rm_sys::rm_init::GrStore {
            lookup_status: NV_OK,
            map_status: NV_OK,
            token_status: NV_OK,
            submit_status: NV_OK,
            fence_status: fence,
            sem_status: sem,
            store_status: store,
            work_token: COMPUTE_TOKEN,
            runlist_id: 0,
            fence_value: if fence == NV_OK { 0x20 } else { 0 },
            fence_iters: 2,
            sem_value: if sem == NV_OK { 0x2020 } else { 0 },
            sem_iters: if sem == NOT_REACHED { 0 } else { 5 },
            store_value: if store == NV_OK { 0xec0d_e520 } else { 0 },
            push_dwords: 44,
            reserved_pad: 0,
            kernel_va: COMPUTE_KERNEL_VA,
            qmd_va: COMPUTE_QMD_VA,
            dest_va: COMPUTE_OUT_VA,
        };
    }
    NV_OK
}

/// Steps 21, 22 and 23 share `GrThreads`: `slots` results, slot 5 wrong on
/// `Completed`. Step 23 also queries the MMU fault status, which it
/// reports whenever the grid did not verify.
fn threads(ran: Ran, slots: u32, faults: bool) -> nvidia_rm_sys::rm_init::GrThreads {
    let (fence, sem) = waits(ran);
    let verify = verdict(ran);
    let wrong = verify == NV_ERR_INVALID_STATE;
    let queried = faults && ran != Ran::Verified;
    nvidia_rm_sys::rm_init::GrThreads {
        lookup_status: NV_OK,
        map_status: NV_OK,
        token_status: NV_OK,
        submit_status: NV_OK,
        fence_status: fence,
        sem_status: sem,
        verify_status: verify,
        work_token: COMPUTE_TOKEN,
        runlist_id: 0,
        fence_iters: 2,
        sem_iters: if sem == NOT_REACHED { 0 } else { 5 },
        match_count: match ran {
            Ran::Verified => slots,
            Ran::Completed => slots - 1,
            _ => 0,
        },
        first_bad_idx: if wrong { 5 } else { NOT_REACHED },
        first_bad_val: if wrong { 0xdead } else { 0 },
        push_dwords: 48,
        reserved_pad: 0,
        kernel_va: COMPUTE_KERNEL_VA,
        qmd_va: COMPUTE_QMD_VA,
        out_va: COMPUTE_OUT_VA,
        fault_ctrl_status: if queried { NV_OK } else { NOT_REACHED },
        fault_addr_hi: if queried { 0x1 } else { 0 },
        fault_addr_lo: if queried { 0x2000 } else { 0 },
        fault_type: if queried { 0x3 } else { 0 },
    }
}
#[no_mangle]
extern "C" fn eclipse_rm_step21(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrThreads) -> u32 {
    let Some(ran) = rung("step21") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    unsafe { *out = threads(ran, 32, false) };
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_step22(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrThreads) -> u32 {
    let Some(ran) = rung("step22") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    unsafe { *out = threads(ran, 2176, false) };
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_step23(_a0: u32, out: *mut nvidia_rm_sys::rm_init::GrThreads) -> u32 {
    let Some(ran) = rung("step23") else {
        return NV_ERR_NOT_SUPPORTED;
    };
    unsafe { *out = threads(ran, 32, true) };
    NV_OK
}
/// Step 8's three controls: `rm_api = Some((name, gid, fb))` is the
/// status of each, and a control that answers `NV_OK` fills its fields --
/// the card's name, a 40-byte GID, a heap of 8 GiB with 7.5 GiB free on
/// a 256-bit bus. Without the knob, the stub's refusal.
#[no_mangle]
extern "C" fn eclipse_rm_step8(_a0: u32, out: *mut nvidia_rm_sys::rm_init::RmApiDemo) -> u32 {
    let statuses = {
        let mut f = FAKE_RM.lock();
        f.calls.push("step8");
        f.narrate("step8");
        f.rm_api
    };
    let Some((name_status, gid_status, fb_status)) = statuses else {
        return NV_ERR_NOT_SUPPORTED;
    };
    let mut demo = nvidia_rm_sys::rm_init::RmApiDemo {
        name_status,
        name: [0; 64],
        gid_status,
        gid_length: 0,
        gid: [0; 136],
        fb_status,
        heap_size_kb: 0,
        heap_free_kb: 0,
        bus_width: 0,
    };
    if name_status == NV_OK {
        demo.name[..RM_API_NAME.len()].copy_from_slice(RM_API_NAME.as_bytes());
    }
    if gid_status == NV_OK {
        demo.gid[..RM_API_GID.len()].copy_from_slice(RM_API_GID.as_bytes());
        demo.gid_length = RM_API_GID.len() as u32;
    }
    if fb_status == NV_OK {
        demo.heap_size_kb = 8 << 20;
        demo.heap_free_kb = 7680 << 10;
        demo.bus_width = GSP_BUS_WIDTH;
    }
    unsafe { *out = demo };
    NV_OK
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
    CtxAlloc, ExecFast, ExecSignal, ExecSubmit, GemAlloc, GemMapCpu, GrAlloc, GrChannel,
    RmComptagCaps, VmBind, ADDR_SYSMEM,
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
    /// `(h_virt, ctx, h_memory, va, size, bo_offset, pte_kind, h_dup)`.
    /// `h_dup` is the context client's reference to the object, which the
    /// real RM dups in for any bind outside ctx 0 and which the unbind has to
    /// hand back; 0 for a ctx-0 bind, which crosses no client.
    pub maps: Vec<(u32, u32, u32, u64, u64, u64, u32, u32)>,
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
    /// What `comptag_caps` answers; `None` is a failed query.
    pub comptag_caps: Option<RmComptagCaps>,
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
    /// Step 16 ran to the end (`g_grAllocDone`). What `gr_ready` answers,
    /// and so the precondition for VA-space work. STICKY: it does not care
    /// how many channels are live, which is the whole point -- NVK frees its
    /// enumeration channel before it binds anything.
    pub ladder_built: bool,
    /// Which card the last `gr_ready` asked about. The C flag is
    /// `g_grAllocDone[gpuInstance]`, one per card, so a caller that asks
    /// about the wrong one gets another card's answer -- and a caller that
    /// hardcodes 0 is right on a single-GPU box and wrong on Moebius's.
    pub gr_ready_asked_of: Option<u32>,
    /// Step 17's channel stands (`g_grChanDone`): cleared by `ctx0_reset`.
    pub chan_built: bool,
    /// How many channels step 17 built behind index 0.
    pub step17_builds: u32,
    /// How many times `ctx0_reset` found a channel to tear down.
    pub ctx0_resets: u32,
    /// Where the next mapping the RM is left to place (`requested_va == 0`)
    /// goes: 2 MiB-aligned ranges handed out upward from `RM_PICKS_FROM`.
    /// A test may set it to 0 for a mapping the RM "placed" at VA 0.
    pub picked_va: u64,
    /// `gem_map_cpu` answers `lookup_status` set for an object it holds:
    /// the C side's shape for a lookup the RM refused.
    pub fail_map_cpu: bool,
    /// `exec_submit` and `exec_submit_signaled` fail as calls, before any
    /// stage: what the Rust wrappers turn into `Err`.
    pub refuse_submit: bool,
    /// `vm_bind_map` fails as a CALL, before the RM looks at the object or
    /// the range: what the Rust wrapper turns into `Err`, as against the
    /// `RM_VA_TAKEN` refusal `refuse_map` answers inside an `Ok`.
    pub fail_bind: bool,
    /// The ring runs what it fetches: a signalled submit lands its wait
    /// (`fence_wait_status == 0`), and a push the fake can read -- the
    /// 6-dword host semaphore RELEASE, fetched through a binding of a
    /// sysmem object -- writes its payload where it says, through the
    /// same bindings. Without it the fence is submitted and never waited
    /// out, which is what a fake with no GPU behind it always answered.
    pub runs: bool,
    /// With `runs`: the PTEs `vm_bind_map` wrote are not GPU-visible, so a
    /// push from a bound VA fetches nothing and its wait never lands; only
    /// a push from the channel's own RM-mapped buffer runs.
    pub vm_bind_invisible: bool,
    /// With `runs`: the ring runs and the wait lands, but a release's write
    /// never reaches memory -- the coherency failure `GR-IDLE-NOFENCE`
    /// names, and the one shape of "the fence landed" that is not a pass.
    pub writes_lost: bool,
    /// Every semaphore RELEASE the ring executed: `(ctx, VA, payload)`.
    pub releases: Vec<(u32, u64, u32)>,
    /// `gem_alloc` fails as a CALL, before it can report an `alloc_status`:
    /// what the Rust wrapper turns into `Err`, as against the refusal
    /// `fail_gem_alloc` answers inside an `Ok`.
    pub fail_gem_alloc_call: bool,
    /// `gem_map_cpu` fails as a CALL: the `Err` shape, as against the
    /// `lookup_status` refusal `fail_map_cpu` answers inside an `Ok`.
    pub fail_map_cpu_call: bool,
    /// `step15` answers a GR probe of `(num_gpc, total_tpc)` instead of
    /// `NV_ERR_NOT_SUPPORTED`: the floorswept truth GRAPH_UNITS reports.
    pub gr_probe: Option<(u32, u32)>,
    /// `ctx0_reset` refuses: the singleton stands, and the exit says so.
    pub fail_ctx0_reset: bool,
    /// `intr_table` answers the GSP's interrupt kernel table instead of
    /// `NV_ERR_NOT_SUPPORTED`: the control's status and the rows as
    /// `(engine_idx, pmc_intr_mask, vector_stall, vector_non_stall)`.
    pub intr_table: Option<(u32, Vec<(u32, u32, u32, u32)>)>,
    /// The RM narrates: each bring-up probe leaves one line in the log
    /// capture the proc readers fold into their output, as the real RM's
    /// printf does between `capture_begin` and `capture_take`.
    pub narrates: bool,
    /// The display cursor plane: `(init status, heads, image status)`;
    /// `None` is a build without one (every call `NV_ERR_NOT_SUPPORTED`).
    pub hwcursor: Option<(u32, u32, u32)>,
    /// How far the compute rungs (`step18`..`step23`, `bench`, `step10`)
    /// get; `None` is the stub's `NV_ERR_NOT_SUPPORTED`. See [`Ran`].
    pub compute: Option<Ran>,
    /// The bench grid ran but the PTIMER read 0 both times.
    pub bench_zero_time: bool,
    /// `init_core` constructs the RM core instead of refusing.
    pub core: bool,
    /// `attach_gpu` attaches the card as this RM device instance.
    pub attach: Option<u32>,
    /// What every attach was told, in order.
    pub attached: Vec<Attached>,
    /// `get_gsp_info` answers a card of this name (`""` is the all-zero
    /// struct of a GSP that has not booted).
    pub gsp_info: Option<&'static str>,
    /// `step8`'s three control statuses: name, GID, FB info.
    pub rm_api: Option<(u32, u32, u32)>,
    /// `state_init`'s three phase statuses: pre-init, init, load.
    pub state_init: Option<(u32, u32, u32)>,
    /// What `mark_console_gpu` answers.
    pub console_mark: Option<u32>,
    /// Every console declaration: `(device instance, console bytes, at
    /// BAR1 base)`.
    pub console_marks: Vec<(u32, u64, bool)>,
}

/// `ECLIPSE_CHAN_BUF_SIZE`: the channel's own buffer, pushbuffer and ring,
/// which the RM maps itself at the VA `ctx_alloc` reports.
pub(super) const CHAN_BUF_SIZE: u64 = 0x1_0000;

/// Where the fake's `ctx_alloc` puts the channel buffer of context `ctx`.
pub(super) fn chan_buf_va(ctx: u32) -> u64 {
    0x10_0000 * u64::from(ctx)
}

/// `ECLIPSE_PUSH_HDR(subch=0, NVC46F_SEM_ADDR_LO, count=5)`: the header of
/// the host semaphore stream, as the C fence builder and the kernel's
/// self-test write it.
const SEM_STREAM_HDR: u32 = (1 << 29) | (5 << 16) | (0x5c >> 2);

/// The first VA the fake hands out when the caller lets the RM pick.
pub(super) const RM_PICKS_FROM: u64 = 0x20_0000_0000;

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
    comptag_caps: Some(RmComptagCaps {
        one_to_one: 0,
        one_to_four: 0,
        raw_mode: 0,
        compbit_backing_off: 0,
        post_l2_off: 0,
        scrub_on_free: 0,
        vas_for_ce: 0,
        compr_page_shift: 0,
    }),
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
    gr_ready_asked_of: None,
    chan_built: false,
    step17_builds: 0,
    ctx0_resets: 0,
    picked_va: RM_PICKS_FROM,
    fail_map_cpu: false,
    refuse_submit: false,
    fail_bind: false,
    runs: false,
    vm_bind_invisible: false,
    writes_lost: false,
    releases: Vec::new(),
    fail_gem_alloc_call: false,
    fail_map_cpu_call: false,
    gr_probe: None,
    fail_ctx0_reset: false,
    intr_table: None,
    narrates: false,
    hwcursor: None,
    compute: None,
    bench_zero_time: false,
    core: false,
    attach: None,
    attached: Vec::new(),
    gsp_info: None,
    rm_api: None,
    state_init: None,
    console_mark: None,
    console_marks: Vec::new(),
};

pub(super) static FAKE_RM: lock::Mutex<FakeRm> = lock::Mutex::new(EMPTY_RM);

pub(super) fn reset_fake_rm() {
    *FAKE_RM.lock() = EMPTY_RM;
}

impl FakeRm {
    /// One line of RM narration for `call`, when the knob says so.
    fn narrate(&self, call: &str) {
        if self.narrates {
            nvidia_rm_sys::os_interface::capture_line(&alloc::format!("[fake-rm] {}", call));
        }
    }

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
        self.in_chan_buf(ctx, va, len)
            || self.maps.iter().any(|m| {
                m.1 == ctx && va >= m.3 && va.wrapping_add(u64::from(len)) <= m.3.wrapping_add(m.4)
            })
    }

    /// Whether `[va, va + len)` lies inside the channel buffer of a context
    /// `ctx_alloc` built: the RM mapped that one itself, so its lookup
    /// finds it with no binding of the client's.
    fn in_chan_buf(&self, ctx: u32, va: u64, len: u32) -> bool {
        let base = chan_buf_va(ctx);
        self.ctxs.contains(&ctx)
            && va >= base
            && va.wrapping_add(u64::from(len)) <= base.wrapping_add(CHAN_BUF_SIZE)
    }

    /// The host address of `va` in context `ctx`, through a binding of a
    /// sysmem object; `None` for a VA the CPU cannot reach that way.
    fn host_addr(&self, ctx: u32, va: u64) -> Option<u64> {
        self.maps
            .iter()
            .find(|m| m.1 == ctx && va >= m.3 && va < m.3.wrapping_add(m.4))
            .and_then(|m| Some(self.pa_of(m.2)? + m.5 + (va - m.3)))
    }

    /// Whether the ring runs a push fetched at `va`: with `runs`, unless
    /// the PTEs are invisible and the push is not the channel's own.
    fn runs_push(&self, ctx: u32, va: u64, len: u32) -> bool {
        self.runs && (!self.vm_bind_invisible || self.in_chan_buf(ctx, va, len))
    }

    /// Run a push the fake can read: a host semaphore RELEASE writes its
    /// payload at its VA, through the context's bindings, as the PBDMA
    /// would. Anything else, or a VA the fake has no memory behind
    /// (the channel's own buffer), runs without a trace.
    fn execute(&mut self, ctx: u32, va: u64, len: u32) {
        let Some(push) = self.host_addr(ctx, va) else {
            return;
        };
        if len < 24 {
            return;
        }
        let w = |i: u64| unsafe { core::ptr::read_volatile((push + 4 * i) as *const u32) };
        // SEM_EXECUTE's OPERATION field, bits [2:0]: 1 is RELEASE.
        if w(0) != SEM_STREAM_HDR || w(5) & 0x7 != 1 {
            return;
        }
        let sem_va = u64::from(w(1)) | (u64::from(w(2) & 0xff) << 32);
        let payload = w(3);
        if self.writes_lost {
            return;
        }
        if let Some(sem) = self.host_addr(ctx, sem_va) {
            unsafe { core::ptr::write_volatile(sem as *mut u32, payload) };
            self.releases.push((ctx, sem_va, payload));
        }
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

    /// Every push the ring of context `ctx` was handed, in order: its VA
    /// and its length.
    pub fn submits_of_ctx(&self, ctx: u32) -> Vec<(u64, u32)> {
        self.submits
            .iter()
            .filter(|s| s.0 == ctx)
            .map(|s| (s.1, s.2))
            .collect()
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
            buf_gpu_va: chan_buf_va(ctx_idx),
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
extern "C" fn eclipse_rm_gem_alloc(
    _inst: u32,
    _ctx_idx: u32,
    size: u64,
    sysmem: u32,
    out: *mut GemAlloc,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("gem_alloc");
    if f.fail_gem_alloc_call {
        return NV_ERR_INVALID_STATE;
    }
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
extern "C" fn eclipse_rm_gem_map_cpu(
    _inst: u32,
    _ctx_idx: u32,
    h_memory: u32,
    out: *mut GemMapCpu,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("gem_map_cpu");
    if f.fail_map_cpu_call {
        return NV_ERR_INVALID_STATE;
    }
    let elsewhere = f.map_cpu_elsewhere;
    let refused = f.fail_map_cpu;
    let r = match f.gem(h_memory) {
        Some(_) if refused => GemMapCpu {
            lookup_status: NV_ERR_OBJECT_NOT_FOUND,
            address_space: 0,
            phys_addr: 0,
            size: 0,
        },
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
extern "C" fn eclipse_rm_gem_fbmem_offset(
    _inst: u32,
    _ctx_idx: u32,
    h_memory: u32,
    p_offset: *mut u64,
) -> u32 {
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
extern "C" fn eclipse_rm_gem_free(_inst: u32, _ctx_idx: u32, h_memory: u32) -> u32 {
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
/// Succeeds, unless a test said otherwise, with every capability clear, which
/// is the "query worked, this GPU is in no mode we understand" case. Deliberately not a plausible Turing: a
/// double that claimed one-to-one would make `may_honour_compressed_kind`
/// return true across the host suite and quietly turn a behaviour change on in
/// tests that are about something else. A test that wants the permissive case
/// sets it through `comptags::set_caps`, which is where that decision belongs.
#[no_mangle]
extern "C" fn eclipse_rm_comptag_caps(_inst: u32, out: *mut RmComptagCaps) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("comptag_caps");
    match f.comptag_caps {
        Some(caps) => {
            unsafe { *out = caps };
            NV_OK
        }
        None => NV_ERR_NOT_SUPPORTED,
    }
}

#[no_mangle]
extern "C" fn eclipse_rm_vm_bind_map(
    _inst: u32,
    ctx_idx: u32,
    _owner_ctx_idx: u32,
    h_memory: u32,
    size: u64,
    requested_va: u64,
    bo_offset: u64,
    pte_kind: u32,
    _honour_compressed_kind: u32,
    out: *mut VmBind,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("vm_bind_map");
    if f.fail_bind {
        return NV_ERR_NOT_SUPPORTED;
    }
    // `requested_va == 0` leaves the placement to the RM (what a legacy
    // `GEM_PUSHBUF` client's buffers get): the fake places it like one.
    let va = if requested_va == 0 {
        f.picked_va
    } else {
        requested_va
    };
    let r = if f.gem(h_memory).is_none() {
        VmBind {
            virt_status: 0,
            map_status: NV_ERR_OBJECT_NOT_FOUND,
            h_virt: 0,
            h_dup: 0,
            actual_va: 0,
        }
    } else if f.refuse_map || f.va_taken(ctx_idx, va, size) {
        VmBind {
            virt_status: RM_VA_TAKEN,
            map_status: RM_VA_TAKEN,
            h_virt: 0,
            h_dup: 0,
            actual_va: 0,
        }
    } else {
        if requested_va == 0 {
            f.picked_va = va.wrapping_add((size + 0x1f_ffff) & !0x1f_ffff);
        }
        let h_virt = f.fresh();
        // A bind outside ctx 0 crosses RM clients, so the real RM dups the
        // object into the context's client and maps the dup. Model it, so a
        // test can see the dup come back on the unbind.
        let h_dup = if ctx_idx == 0 { 0 } else { f.fresh() };
        f.maps.push((
            h_virt, ctx_idx, h_memory, va, size, bo_offset, pte_kind, h_dup,
        ));
        VmBind {
            virt_status: 0,
            map_status: 0,
            h_virt,
            h_dup,
            actual_va: va,
        }
    };
    unsafe { *out = r };
    NV_OK
}
#[no_mangle]
extern "C" fn eclipse_rm_vm_bind_unmap(
    _inst: u32,
    ctx_idx: u32,
    h_virt: u32,
    h_dup: u32,
    _size: u64,
    _va: u64,
) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("vm_bind_unmap");
    f.unmaps += 1;
    match f.maps.iter().position(|m| m.0 == h_virt) {
        Some(pos) => {
            // The handles only exist in the context's own client, so a free
            // naming the wrong context -- or forgetting the dup -- is the real
            // RM freeing nothing, or the wrong object. Counted as bad rather
            // than refused, so a caller that gets it wrong shows up in a test
            // even though the status is still NV_OK.
            if f.maps[pos].1 != ctx_idx || f.maps[pos].7 != h_dup {
                f.bad += 1;
            }
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
extern "C" fn eclipse_rm_class_free(_inst: u32, ctx_idx: u32, h_object: u32) -> u32 {
    let mut f = FAKE_RM.lock();
    f.calls.push("class_free");
    match f.classes.iter().position(|c| c.0 == h_object) {
        Some(pos) => {
            // Same as the unbind: the handle lives in the client of the
            // context it was allocated for, so a free naming another context
            // is a free in the wrong client.
            if f.classes[pos].1 != ctx_idx {
                f.bad += 1;
            }
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
    if f.refuse_submit {
        return NV_ERR_NOT_SUPPORTED;
    }
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
    if f.refuse_submit {
        return NV_ERR_NOT_SUPPORTED;
    }
    let [lookup, map, token, submit] = f.submit(ctx_idx, push_va, push_len, Some(fence_payload));
    let submitted = lookup == 0 && submit == 0;
    // The GPU "runs" the push before the call returns: the fence lands
    // unless told to stall. The driver polls it itself (async path).
    if submitted && !f.fence_stalls {
        FENCE_SEM.store(fence_payload, Ordering::Release);
    }
    // And the C side's own wait on it: only a ring that runs lands it.
    let ran = submitted && f.runs_push(ctx_idx, push_va, push_len);
    if ran {
        f.execute(ctx_idx, push_va, push_len);
    }
    unsafe {
        *out = ExecSignal {
            lookup_status: lookup,
            map_status: map,
            token_status: token,
            submit_status: submit,
            fence_submit_status: if submitted { 0 } else { 0xFFFF_FFFF },
            fence_wait_status: if ran { 0 } else { 0xFFFF_FFFF },
            fence_value: if ran { fence_payload } else { 0 },
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

/// The double's own invariants: a fake that silently stops refusing makes
/// every suite above it pass while asserting nothing. Its own file because
/// this one is already the RM's whole surface.
#[cfg(test)]
mod double_tests;
