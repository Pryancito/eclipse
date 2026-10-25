//! The `/proc` readers of the bring-up ladder on the fake RM: `gpustep15`
//! (the GR probe and the GSP's interrupt kernel table, which names every
//! engine and marks the hot rows), the loose arms of `gpustep16` and
//! `gpustep17` (skipped, narrated, refused, stopped part-way), `gpuedid`
//! on a hosted build (skipped, then the stub's refusal), and the two
//! questions the HDA driver asks this module: what the last HDMI audio
//! enable did, and which GPU drives the monitor. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own because
//! the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::utils::host_hooks::take_dmesg;

/// The GPUs the registry held before a test, put back on drop whatever
/// the test did in between (a panic included).
struct Registry(usize);

impl Registry {
    fn hold() -> Self {
        Self(NVIDIA_GPUS.lock().len())
    }

    fn add(&self, gpu: NvidiaGpu) {
        NVIDIA_GPUS.lock().push(Arc::new(gpu));
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        NVIDIA_GPUS.lock().truncate(self.0);
        nv::ctx_clear_wedged(2);
    }
}

/// Context `ctx` of `gpu` has a direct-submit slot whose landing zone is
/// a word of ours, still at 0: the fence the slot issued never landed.
fn unlanded(gpu: &NvidiaGpu, ctx: u32, payload: u32) -> &'static core::sync::atomic::AtomicU32 {
    let cell: &'static core::sync::atomic::AtomicU32 = alloc::boxed::Box::leak(
        alloc::boxed::Box::new(core::sync::atomic::AtomicU32::new(0)),
    );
    gpu.nouveau_fast.lock()[ctx as usize] = FastSlot::Ready(nv::FastCtx {
        userd_gpget: 0,
        userd_gpput: 0,
        gpfifo_va: 0,
        fence_pb_va: 0,
        fence_sem_va: cell.as_ptr() as usize,
        buf_gpu_va: 0,
        fence_pb_off: 0,
        fence_sem_off: 0,
        slot_bytes: 0,
        entries: 0,
        work_token: 0,
        runlist_id: 0,
        doorbell_va: 0,
        next_payload: payload + 1,
        submits: 1,
        fenced: 1,
        last_fence: Some((1, payload)),
        entries_put: 2,
    });
    cell
}

/// A GPU the boot never attached to the RM has no device instance, and
/// every proc reader of the ladder says so first, each in its own words,
/// without touching the RM.
#[test]
fn every_proc_reader_of_the_ladder_says_skipped_without_a_device_instance() {
    let _g = LOCK.lock();
    let gpu = gpu();
    reset_fake_rm();
    assert_eq!(
        DrmScheme::bringup_step15(&gpu),
        "[gpustep15] skipped (bring the GPU up first: gpustep5/6/8/9 on the secondary, or gpustep14 on the console)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step16(&gpu),
        "[gpustep16] skipped (bring the GPU up first: gpustep5/6/8/9 on the secondary, or gpustep14 on the console)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step17(&gpu),
        "[gpustep17] skipped (bring the GPU up first, then gpustep16)\n"
    );
    assert_eq!(
        DrmScheme::bringup_edid(&gpu),
        "[gpuedid] === nvidia-test === skipped (run /proc/gpuinit first)\n"
    );
    assert!(
        FAKE_RM.lock().calls.is_empty(),
        "a skipped step asks the RM nothing: {:?}",
        FAKE_RM.lock().calls
    );
}

/// With the probe answering, step 15 prints the GPC mask, one line per
/// GPC with its TPC share, the SM count at two per TPC, and the RM's own
/// narration folded in above; the interrupt table, which this RM does not
/// serve, is one FAILED line, not a missing section, and its narration is
/// not folded: the capture closes before the table is asked.
#[test]
fn step15_prints_the_gr_probe_and_says_the_interrupt_table_failed() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    {
        let mut f = FAKE_RM.lock();
        f.gr_probe = Some((2, 5));
        f.narrates = true;
    }
    let s = DrmScheme::bringup_step15(&gpu);
    let want = [
        "[gpustep15] | [fake-rm] step15",
        "[gpustep15] --- GR (graphics/compute) engine config from live GSP-RM ---",
        "[gpustep15] GR_GET_GPC_MASK: OK mask=0x00000003 (2 GPCs)",
        "[gpustep15]   GPC0: 3 TPCs",
        "[gpustep15]   GPC1: 2 TPCs",
        "[gpustep15] GR_GET_TPC_MASK: OK",
        "[gpustep15] --- 5 TPCs total => 10 usable SMs (Turing: 2 SMs/TPC) ---",
        // The table is asked after the capture closed, so its narration
        // is not folded in: the reader captures the probe alone.
        "[gpustep15] intr_table FAILED, NV_STATUS=0x56",
    ];
    let lines: Vec<&str> = s.lines().collect();
    for (i, w) in want.iter().enumerate() {
        assert_eq!(lines.get(i).copied(), Some(*w), "line {} of {}", i, s);
    }
    assert_eq!(lines.len(), want.len(), "{}", s);
    assert!(
        !s.contains("GPC2"),
        "a GPC outside the mask gets no line: {}",
        s
    );

    // A probe with no GPCs in it is a GPC-mask control that failed: the
    // status is printed in place of OK and nothing per GPC follows it.
    {
        let mut f = FAKE_RM.lock();
        f.gr_probe = Some((0, 30));
        f.narrates = false;
    }
    let s = DrmScheme::bringup_step15(&gpu);
    assert_eq!(
        s,
        "[gpustep15] --- GR (graphics/compute) engine config from live GSP-RM ---\n\
         [gpustep15] GR_GET_GPC_MASK: NV_STATUS=0x40 mask=0x00000000 (0 GPCs)\n\
         [gpustep15] intr_table FAILED, NV_STATUS=0x56\n"
    );
}

/// A probe the RM refuses is the FAILED line with the status, and still
/// the interrupt table after it: the two halves of step 15 fail on their
/// own.
#[test]
fn step15_without_the_probe_says_failed_and_goes_on_to_the_table() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    let s = DrmScheme::bringup_step15(&gpu);
    assert_eq!(
        s,
        "[gpustep15] eclipse_rm_step15 FAILED, NV_STATUS=0x56 (GR not state-loaded? run gpustep9 or gpustep14)\n\
         [gpustep15] intr_table FAILED, NV_STATUS=0x56\n"
    );
}

/// Every engine index the table knows, with the name it prints.
const ENGINES: [(u32, &str); 39] = [
    (0, "NULL"),
    (1, "TMR"),
    (2, "DISP"),
    (3, "FB"),
    (4, "FIFO"),
    (7, "BUS"),
    (8, "PMGR"),
    (11, "BIF"),
    (13, "PRIVRING"),
    (14, "PMU"),
    (15, "CE0"),
    (16, "CE1"),
    (17, "CE2"),
    (18, "CE3"),
    (19, "CE4"),
    (20, "CE5"),
    (43, "LTC"),
    (44, "FBHUB"),
    (45, "HDACODEC"),
    (46, "GMMU"),
    (47, "SEC2"),
    (49, "NVLINK"),
    (50, "GSP"),
    (59, "REPLAYABLE_FAULT"),
    (60, "ACCESS_CNTR"),
    (61, "NON_REPLAYABLE_FAULT"),
    (64, "INFO_FAULT"),
    (65, "NVDEC0"),
    (73, "CPU_DOORBELL"),
    (74, "PRIV_DOORBELL"),
    (75, "MMU_ECC_ERROR"),
    (77, "PERFMON"),
    (84, "GR0"),
    (156, "GR_FECS_LOG"),
    (164, "TMR_SWRL"),
    (165, "DISP_GSP"),
    (166, "REPLAYABLE_FAULT_CPU"),
    (167, "NON_REPLAYABLE_FAULT_CPU"),
    (99, "?"),
];

/// The interrupt kernel table prints one row per entry that has a vector
/// or a legacy PMC mask, with the engine's name (a `?` for one the table
/// does not know), `-` for a vector the entry lacks, and `>>>` on the
/// rows that answer the two questions the dump exists for: CPU vector
/// 156 and PMC mask bit 28. An entry with neither vector nor mask is not
/// a row.
#[test]
fn the_interrupt_table_names_every_engine_and_marks_the_hot_rows() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    let mut rows: Vec<(u32, u32, u32, u32)> = ENGINES
        .iter()
        .map(|&(idx, _)| (idx, 0, idx + 1000, u32::MAX))
        .collect();
    // Hot by the stall vector, hot by the non-stall vector, hot by the
    // mask with no vector at all, a mask-only row that is not hot, and
    // an entry with nothing in it.
    rows.push((84, 0, 156, u32::MAX));
    rows.push((50, 0, u32::MAX, 156));
    rows.push((7, 0x1000_0000, u32::MAX, u32::MAX));
    rows.push((2, 0x0000_0004, u32::MAX, u32::MAX));
    rows.push((3, 0, u32::MAX, u32::MAX));
    let n = rows.len();
    FAKE_RM.lock().intr_table = Some((0, rows));
    let s = DrmScheme::bringup_step15(&gpu);
    let table: Vec<&str> = s.lines().filter(|l| l.contains(" engine=")).collect();
    assert_eq!(table.len(), n - 1, "every entry but the empty one: {}", s);
    assert!(
        s.contains(&alloc::format!(
            "--- GSP interrupt kernel table ({} entries;",
            n
        )),
        "{}",
        s
    );
    for (idx, name) in ENGINES {
        let row = alloc::format!(
            "[gpustep15]     engine={:3} ({:<22}) pmcMask=0x00000000 vecStall={:>5} vecNonStall=    -",
            idx,
            name,
            idx + 1000
        );
        assert!(table.contains(&row.as_str()), "{:?} in {:#?}", row, table);
    }
    for hot in [
        "[gpustep15] >>> engine= 84 (GR0                   ) pmcMask=0x00000000 vecStall=  156 vecNonStall=    -",
        "[gpustep15] >>> engine= 50 (GSP                   ) pmcMask=0x00000000 vecStall=    - vecNonStall=  156",
        "[gpustep15] >>> engine=  7 (BUS                   ) pmcMask=0x10000000 vecStall=    - vecNonStall=    -",
        "[gpustep15]     engine=  2 (DISP                  ) pmcMask=0x00000004 vecStall=    - vecNonStall=    -",
    ] {
        assert!(table.contains(&hot), "{:?} in {:#?}", hot, table);
    }
    assert!(!s.contains("INTR_GET_KERNEL_TABLE control FAILED"), "{}", s);
}

/// A control that failed inside an `NV_OK` call is said once above an
/// empty table; and the table is cut at the 128 rows the mirror holds.
#[test]
fn a_failed_table_control_is_said_above_the_empty_table() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    FAKE_RM.lock().intr_table = Some((0x40, Vec::new()));
    let s = DrmScheme::bringup_step15(&gpu);
    assert!(
        s.contains(
            "[gpustep15] INTR_GET_KERNEL_TABLE control FAILED, NV_STATUS=0x40 (table below is empty)\n\
             [gpustep15] --- GSP interrupt kernel table (0 entries;"
        ),
        "{}", s
    );
    assert!(!s.contains(" engine="), "{}", s);

    let rows = (0..200u32).map(|i| (1, 0, i, u32::MAX)).collect();
    FAKE_RM.lock().intr_table = Some((0, rows));
    let s = DrmScheme::bringup_step15(&gpu);
    assert!(s.contains("(128 entries;"), "{}", s);
    assert_eq!(s.lines().filter(|l| l.contains(" engine=")).count(), 128);
}

/// Steps 16 and 17 on an RM that serves no ladder: the FAILED line with
/// the status and the RM's narration above it. With a ladder that stops
/// part-way, the phase that stopped it is FAILED with its status and the
/// completion line is not printed.
#[test]
fn steps_16_and_17_say_failed_when_the_rm_refuses_and_name_the_phase_that_stopped() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    FAKE_RM.lock().narrates = true;
    assert_eq!(
        DrmScheme::bringup_step16(&gpu),
        "[gpustep16] | [fake-rm] step16\n\
         [gpustep16] eclipse_rm_step16 FAILED, NV_STATUS=0x56 (GPU not GSP-booted/state-loaded?)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step17(&gpu),
        "[gpustep17] | [fake-rm] step17\n\
         [gpustep17] eclipse_rm_step17 FAILED, NV_STATUS=0x56 (run gpustep16 first; GPU state-loaded?)\n"
    );

    {
        let mut f = FAKE_RM.lock();
        f.narrates = false;
        f.ladder = true;
        f.incomplete_step16 = true;
        f.incomplete_step17 = true;
    }
    let s = DrmScheme::bringup_step16(&gpu);
    assert!(
        s.contains("[gpustep16] KEPLER_CHANNEL_GROUP_A:  OK (hTsg=0x00004004, engineType=GRAPHICS)\n\
                    [gpustep16] FERMI_CONTEXT_SHARE_A:   FAILED NV_STATUS=0x40 (hCtxShare=0x00000000)\n"),
        "{}", s
    );
    assert!(!s.contains("LADDER COMPLETE"), "{}", s);
    let s = DrmScheme::bringup_step17(&gpu);
    assert!(
        s.contains("[gpustep17] GPFIFO_SCHEDULE:          FAILED NV_STATUS=0x40\n"),
        "{}",
        s
    );
    assert!(!s.contains("CHANNEL LIVE"), "{}", s);
}

/// `/proc/gpuedid` on a hosted build: the header with the instance, then
/// the stub's refusal, which is the only answer a build with no RM C
/// objects can give.
#[test]
fn gpuedid_on_a_hosted_build_is_the_header_and_the_stubs_refusal() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    assert_eq!(
        DrmScheme::bringup_edid(&gpu),
        "[gpuedid] === nvidia-test (rm instance 0) ===\n\
         [gpuedid] eclipse_rm_edid FAILED, NV_STATUS=0x56 (run /proc/gpuinit first)\n"
    );
}

/// What the HDA driver reads: until something records an HDMI audio
/// enable, the two lines saying it never ran; after one, that record
/// with its newline.
#[test]
fn the_hdmi_audio_status_says_never_ran_until_something_records() {
    let _g = LOCK.lock();
    let saved = HDMI_AUDIO_STATUS.lock().take();
    let never = hdmi_audio_status();
    assert!(
        never.starts_with("[hdmi-audio] never ran — no RM display query"),
        "{}",
        never
    );
    assert_eq!(never.lines().count(), 2, "{}", never);
    record_hdmi_audio_status(String::from("[hdmi-audio] head 0: enabled"));
    assert_eq!(hdmi_audio_status(), "[hdmi-audio] head 0: enabled\n");
    *HDMI_AUDIO_STATUS.lock() = saved;
}

/// The HDA driver's other question, answered from the registry: the
/// GPU whose BAR1 holds the boot framebuffer drives the monitor, a
/// registered GPU whose BAR1 does not hold it does not (its bus
/// neighbour's answer is not its own), and a card the registry does not
/// know is left to its BAR1 -- which no boot framebuffer, or one at
/// address 0, rules out before any config read.
#[test]
fn the_hda_driver_asks_the_registry_which_gpu_drives_the_monitor() {
    let _g = LOCK.lock();
    let reg = Registry::hold();
    let saved_fb = *BOOT_FB_INFO.lock();
    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    reg.add(gpu_rm());
    let mut twin = gpu();
    twin.pci_bus = 0x41;
    reg.add(twin);
    let mut other = gpu();
    other.pci_bus = 0x41;
    other.pci_device = 3;
    other.bar1_phys = 0x1_0000_0000;
    reg.add(other);
    assert!(nvidia_hda_is_monitor_gpu(0, 0), "BAR1 at 0 holds 0x1000");
    assert!(nvidia_hda_is_monitor_gpu(0x41, 0), "so does the twin's");
    assert!(
        !nvidia_hda_is_monitor_gpu(0x41, 3),
        "BAR1 at 4 GiB does not, whatever its bus neighbour's holds"
    );

    *BOOT_FB_INFO.lock() = None;
    assert!(
        !nvidia_hda_is_monitor_gpu(9, 9),
        "no boot framebuffer: nobody drives the monitor"
    );
    set_boot_fb_info(0, 0, 0, 0);
    assert!(
        !nvidia_hda_is_monitor_gpu(9, 9),
        "a boot framebuffer at 0 is none"
    );
    *BOOT_FB_INFO.lock() = saved_fb;
}

/// `syncobj`'s fence-timeout upcall is routed to every registered GPU:
/// the one whose slot issued the fence lands the payload from the CPU
/// and says so, the others shrug, and a context with no slot anywhere
/// is nothing to land and nothing to say.
#[test]
fn the_fence_timeout_upcall_reaches_the_gpu_whose_slot_issued_the_fence() {
    let _g = LOCK.lock();
    let reg = Registry::hold();
    let issuer = gpu();
    let cell = unlanded(&issuer, 2, 5);
    let fence_va = cell.as_ptr() as usize;
    reg.add(gpu());
    reg.add(issuer);
    let _ = take_dmesg();
    nouveau_fence_timeout_hook(2, fence_va, 5, 9, 1);
    assert_eq!(
        cell.load(Ordering::Acquire),
        5,
        "the payload landed from the CPU"
    );
    let said: Vec<_> = take_dmesg()
        .into_iter()
        .filter(|(_, l)| l.contains("fence TIMEOUT (direct submit)"))
        .collect();
    assert_eq!(said.len(), 1, "one GPU had the slot: {:?}", said);
    assert!(
        said[0].1.contains("ctx2 payload=5 never landed")
            && said[0].1.contains("syncobj handle=9 point=1"),
        "{:?}",
        said
    );

    nouveau_fence_timeout_hook(7, 0x4000, 1, 5, 2);
    let said: Vec<_> = take_dmesg()
        .into_iter()
        .filter(|(_, l)| l.contains("fence TIMEOUT"))
        .collect();
    assert!(said.is_empty(), "no slot 7 anywhere: {:?}", said);
}
