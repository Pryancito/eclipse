//! What the GPU driving the monitor tells the DRM layer once the RM has
//! described its outputs (`rm_display_snap`): the connectors by output
//! bit and the `1001` alias, the EDID the RM's 32-byte head is finished
//! into or the boot EDID that matches it, the HDMI audio enable that
//! rides every query (and its answers by instance and mask), the HDA
//! driver's stream-start kick across the registry with the GOP path
//! for a console GPU without RM, the hardware cursor ladder on the fake
//! RM, and the one-shot RM core init with its poison. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own
//! because the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::nvme::nvme_queue::test_clock;
use crate::utils::host_hooks::take_dmesg;
use nvidia_rm_sys::rm_init::GrEdid;

/// `NV_ERR_NOT_SUPPORTED`: what every hosted RM stub answers.
const NOT_SUPPORTED: u32 = 0x56;

const EDID_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

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
    }
}

/// The boot framebuffer and the last HDMI audio line as a test found
/// them, put back on drop.
struct Globals {
    fb: Option<BootFbInfo>,
    hdmi: Option<String>,
}

impl Globals {
    fn save() -> Self {
        Self {
            fb: *BOOT_FB_INFO.lock(),
            hdmi: HDMI_AUDIO_STATUS.lock().take(),
        }
    }
}

impl Drop for Globals {
    fn drop(&mut self) {
        *BOOT_FB_INFO.lock() = self.fb;
        *HDMI_AUDIO_STATUS.lock() = self.hdmi.take();
    }
}

/// Two outputs (bits 0 and 2), the second connected with its EDID head
/// read (manufacturer bytes 8..12, 60 x 34 cm), HDMI on the first and
/// DisplayPort on the second.
fn topology() -> GrEdid {
    let mut edid_head = [0u8; 32];
    edid_head[..8].copy_from_slice(&EDID_HEADER);
    edid_head[8..12].copy_from_slice(&[0x4c, 0x2d, 0x0e, 0x0f]);
    edid_head[21] = 60;
    edid_head[22] = 34;
    let mut conn_type_display_id = [0u32; 16];
    conn_type_display_id[0] = 1;
    conn_type_display_id[1] = 4;
    let mut conn_type = [0u32; 16];
    conn_type[0] = 0x61;
    conn_type[1] = 0x46;
    GrEdid {
        alloc_status: 0,
        supported_status: 0,
        display_mask: 0b101,
        display_mask_ddc: 0b101,
        connect_status: 0,
        connected_mask: 0b100,
        edid_status: 0,
        edid_display_id: 4,
        edid_size: 128,
        edid_valid: 1,
        edid_head,
        conn_type_status: 0,
        conn_type_count: 2,
        conn_type_display_id,
        conn_type,
    }
}

/// A whole EDID block whose first 32 bytes are `head`'s, checksummed.
fn block_from(head: &[u8; 32]) -> [u8; 128] {
    let mut block = [0u8; 128];
    block[..32].copy_from_slice(head);
    let sum = block[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    block[127] = sum.wrapping_neg();
    block
}

fn connector(gpu: &NvidiaGpu, id: u32) -> Option<(u32, bool, u32, u32, u32)> {
    DrmScheme::get_connector(gpu, id)
        .map(|c| (c.id, c.connected, c.mm_width, c.mm_height, c.connector_type))
}

/// With the RM's snapshot in hand the resources are one connector per
/// output bit (`1001 + 100 * instance + bit`); a connector answers from
/// its bit with the physical size only for the output whose EDID the RM
/// read, and the type from the connector-data sweep (0 when the sweep
/// has no entry); `1001` is the alias of the first supported output on
/// any instance; a bit outside the mask, an id below the instance's or
/// 32 past it is no connector. And the first query enabled HDMI audio,
/// which the hosted RM refused.
#[test]
fn the_connectors_come_from_the_rm_snapshot_by_output_bit_with_1001_as_the_alias_of_the_first() {
    let _g = LOCK.lock();
    let _globals = Globals::save();
    let gpu = super::gpu();
    *gpu.rm_display_snap.lock() = Some((0, topology()));
    assert_eq!(
        DrmScheme::get_resources(&gpu),
        (Vec::new(), alloc::vec![2001], alloc::vec![1001, 1003])
    );
    assert_eq!(connector(&gpu, 1003), Some((1003, true, 600, 340, 10)));
    assert_eq!(
        connector(&gpu, 1001),
        Some((1001, false, 0, 0, 11)),
        "bit 0: HDMI, not connected, size unknown"
    );
    assert_eq!(connector(&gpu, 1002), None, "bit 1 is not an output");
    assert_eq!(connector(&gpu, 1000), None, "below the instance's ids");
    assert_eq!(connector(&gpu, 1001 + 40), None, "past bit 31");
    assert_eq!(
        hdmi_audio_status(),
        alloc::format!(
            "[hdmi-audio] gpu0: enable FAILED, NV_STATUS={:#x} (will retry)\n",
            NOT_SUPPORTED
        ),
        "the query enabled audio, and the host RM refused"
    );

    // No connector-data entry for an output: type unknown.
    let mut bare = topology();
    bare.conn_type_count = 0;
    *gpu.rm_display_snap.lock() = Some((0, bare));
    assert_eq!(connector(&gpu, 1003), Some((1003, true, 600, 340, 0)));

    // Instance 1: its own ids, and 1001 still the alias of its first.
    *gpu.rm_display_snap.lock() = Some((1, topology()));
    assert_eq!(DrmScheme::get_resources(&gpu).2, alloc::vec![1101, 1103]);
    assert_eq!(connector(&gpu, 1103), Some((1103, true, 600, 340, 10)));
    assert_eq!(connector(&gpu, 1001), Some((1001, false, 0, 0, 11)));
    assert_eq!(connector(&gpu, 1003), None, "instance 0's id");
    // An alias on a snapshot with no supported output at all: no bit.
    let mut none = topology();
    none.display_mask = 0;
    *gpu.rm_display_snap.lock() = Some((0, none));
    assert_eq!(connector(&gpu, 1001), None);
    // No snapshot yet, the RM attached: the query is asked, and the host
    // RM refuses it, so the synthetic connector stays and nothing is kept.
    *gpu.rm_display_snap.lock() = None;
    *gpu.rm_device_instance.lock() = Some(0);
    assert_eq!(DrmScheme::get_resources(&gpu).2, alloc::vec![1001]);
    assert!(gpu.rm_display_snap.lock().is_none());
    assert_eq!(connector(&gpu, 1003), None);
}

/// The EDID of a connector: nothing for an output that is not connected
/// or whose EDID the RM did not read; for the one it did, the boot EDID
/// when its manufacturer and product bytes match the RM's head (the
/// bootloader kept the whole block, the RM only 32 bytes), else the head
/// finished into a block with its checksum. A boot EDID shorter than a
/// block, or of another panel, does not stand in.
#[test]
fn get_connector_edid_finishes_the_rm_head_or_serves_the_boot_edid_that_matches_it() {
    let _g = LOCK.lock();
    let _globals = Globals::save();
    let gpu = super::gpu();
    let saved_boot = boot_edid::swap_boot_edid(None);
    let t = topology();
    *gpu.rm_display_snap.lock() = Some((0, t));
    assert_eq!(
        DrmScheme::get_connector_edid(&gpu, 1001),
        None,
        "not connected"
    );
    let finished = DrmScheme::get_connector_edid(&gpu, 1003).expect("the head, finished");
    assert_eq!(finished[..32], t.edid_head);
    assert_eq!(
        finished,
        block_from(&t.edid_head),
        "finished with its checksum"
    );
    assert!(crate::display::edid::block_valid(&finished));

    // The bootloader's block of the same panel: served whole.
    let mut boot = block_from(&t.edid_head);
    boot[60] = 0xaa;
    boot[127] = boot[127].wrapping_sub(0xaa);
    boot_edid::swap_boot_edid(Some((boot, 128)));
    assert_eq!(DrmScheme::get_connector_edid(&gpu, 1003), Some(boot));
    // Too short to be a block: the head is finished instead.
    boot_edid::swap_boot_edid(Some((boot, 64)));
    assert_eq!(DrmScheme::get_connector_edid(&gpu, 1003), Some(finished));
    // Another panel's: likewise.
    boot[9] ^= 0x01;
    boot_edid::swap_boot_edid(Some((boot, 128)));
    assert_eq!(DrmScheme::get_connector_edid(&gpu, 1003), Some(finished));
    boot_edid::swap_boot_edid(saved_boot);

    // Connected, but the RM read no EDID: nothing.
    let mut unread = topology();
    unread.edid_valid = 0;
    *gpu.rm_display_snap.lock() = Some((0, unread));
    assert_eq!(DrmScheme::get_connector_edid(&gpu, 1003), None);
    // The RM's EDID is another output's: nothing for this one.
    let mut other = topology();
    other.edid_display_id = 1;
    *gpu.rm_display_snap.lock() = Some((0, other));
    assert_eq!(DrmScheme::get_connector_edid(&gpu, 1003), None);
    // No snapshot and no RM: no state at all.
    *gpu.rm_display_snap.lock() = None;
    assert_eq!(DrmScheme::get_connector_edid(&gpu, 1003), None);
    assert_eq!(
        DrmScheme::get_resources(&gpu).2,
        alloc::vec![1001],
        "the synthetic connector until the RM describes the outputs"
    );
}

/// The HDMI audio enable answers before the RM for an instance past 31
/// and for a topology with nothing connected (recorded, retried later);
/// otherwise it asks the RM, which the host refuses, forced or not, and
/// records that. `already enabled` needs an ELD the RM accepted, which
/// no host run has.
#[test]
fn the_hdmi_audio_enable_answers_by_instance_and_mask_before_it_asks_the_rm() {
    let _g = LOCK.lock();
    let _globals = Globals::save();
    let t = topology();
    assert_eq!(
        NvidiaGpu::rm_enable_hdmi_audio(40, &t, false),
        "[hdmi-audio] gpu40: RM instance out of range"
    );
    assert!(
        hdmi_audio_status().starts_with("[hdmi-audio] never ran"),
        "not recorded"
    );
    let mut none = topology();
    none.connected_mask = 0;
    assert_eq!(
        NvidiaGpu::rm_enable_hdmi_audio(3, &none, false),
        "[hdmi-audio] gpu3: no connected outputs — will retry"
    );
    assert_eq!(
        hdmi_audio_status(),
        "[hdmi-audio] gpu3: no connected outputs — will retry\n"
    );
    let refused = alloc::format!(
        "[hdmi-audio] gpu3: enable FAILED, NV_STATUS={:#x} (will retry)",
        NOT_SUPPORTED
    );
    assert_eq!(NvidiaGpu::rm_enable_hdmi_audio(3, &t, false), refused);
    assert_eq!(NvidiaGpu::rm_enable_hdmi_audio(3, &t, true), refused);
    assert_eq!(hdmi_audio_status(), alloc::format!("{}\n", refused));
}

/// The stream-start kick: no GPU probed, then across the registry: a
/// GPU without the monitor is skipped, the console GPU without RM takes
/// the GOP path (which cannot build an ELD without the boot EDID), and
/// one with RM asks it for the outputs, which the host refuses. The
/// whole report is what `/proc/gpusnd` then shows.
#[test]
fn the_stream_start_kick_walks_the_registry_and_takes_the_gop_path_on_the_console_gpu() {
    let _g = LOCK.lock();
    let _globals = Globals::save();
    let reg = Registry::hold();
    NVIDIA_GPUS.lock().clear();
    NvidiaGpu::kick_hdmi_audio_all();
    assert_eq!(
        hdmi_audio_status(),
        "[hdmi-audio] stream start: no NVIDIA GPU probed\n"
    );

    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    let saved_boot = boot_edid::swap_boot_edid(None);
    let mut elsewhere = super::gpu();
    elsewhere.pci_bus = 0x41;
    elsewhere.bar1_phys = 0x1_0000_0000;
    reg.add(elsewhere);
    reg.add(super::gpu());
    let mut with_rm = super::gpu();
    with_rm.pci_device = 1;
    *with_rm.rm_device_instance.lock() = Some(0);
    reg.add(with_rm);
    NvidiaGpu::kick_hdmi_audio_all();
    assert_eq!(
        hdmi_audio_status(),
        alloc::format!(
            "[hdmi-audio] 41:00.0 GeForce RTX 2060 Super: skipped — no monitor on this GPU (Linux also leaves its HDMI pins without ELD)\n\
             [hdmi-audio] 00:00.0 GeForce RTX 2060 Super [console GPU, drives the monitor]: RM not attached, GOP HDMI audio path\n\
             [hdmi-audio] GOP path: no UEFI EDID — cannot build an ELD\n\
             [hdmi-audio] 00:01.0 GeForce RTX 2060 Super [console GPU, drives the monitor] (rm0): display query FAILED NV_STATUS={:#x}\n",
            NOT_SUPPORTED
        )
    );
    boot_edid::swap_boot_edid(saved_boot);
}

/// The GOP path with the boot EDID: the SOR the armed core channel
/// shows as owned by a head, with its protocol (DisplayPort here, and a
/// SOR whose protocol is neither is passed over), gets the ELD, PD and
/// ELDV, presence and the SF unmute; with no SOR armed it is SOR 0 on
/// head 0 as HDMI, with the GCP unmute. The ELD goes in word by word
/// and the codec gets its two milliseconds.
#[test]
fn the_gop_audio_path_programs_the_sor_the_core_channel_names_or_sor0_as_hdmi() {
    let _g = LOCK.lock();
    let _globals = Globals::save();
    let edid = block_from(&topology().edid_head);
    let saved_boot = boot_edid::swap_boot_edid(Some((edid, 128)));
    test_clock::set_auto_advance(100);
    let wr32 = |gpu: &NvidiaGpu, off: usize, v: u32| unsafe {
        core::ptr::write_volatile((gpu._bar0 + off) as *mut u32, v)
    };
    let rd32 = |gpu: &NvidiaGpu, off: usize| unsafe {
        core::ptr::read_volatile((gpu._bar0 + off) as *const u32)
    };
    const ARMED: usize = 0x0068_8000;

    // SOR 0 owned by head 0 with a protocol that is neither; SOR 1 owned
    // by head 1 as DisplayPort.
    let gpu = super::gpu();
    wr32(&gpu, ARMED + 0x300, 0x1 | (0x3 << 8));
    wr32(&gpu, ARMED + 0x300 + 0x20, 0x2 | (0x8 << 8));
    assert_eq!(
        gpu.gop_enable_hdmi_audio(),
        "[hdmi-audio] GOP 00:00.0: SOR1 head1 DP ELD+PD+SF unmute (no GSP, like nouveau)"
    );
    let soff = 0x30 + 4;
    assert_eq!(rd32(&gpu, 0x10ec00 + soff) >> 8, 95, "the last ELD word");
    assert_eq!(
        rd32(&gpu, 0x10ec10 + soff) & 0x8000_0003,
        0x8000_0003,
        "PD, ELDV, presence"
    );
    assert_eq!(
        rd32(&gpu, 0x616548 + 0x800) & 0x70,
        0x10,
        "HDA device entry = head 1"
    );
    assert_eq!(
        rd32(&gpu, 0x616618 + 0x800) & 0x8000_000d,
        0x8000_0001,
        "SF unmute"
    );
    assert_eq!(rd32(&gpu, 0x6f00cc), 0, "no GCP on DisplayPort");

    // Two SORs armed, HDMI on head 0 and DisplayPort on head 1: a line
    // each, and nothing from the assembly channel, the armed one having
    // answered.
    let gpu = super::gpu();
    wr32(&gpu, ARMED + 0x300, 0x1 | (0x1 << 8));
    wr32(&gpu, ARMED + 0x300 + 0x20, 0x2 | (0x9 << 8));
    wr32(&gpu, 0x0068_0000 + 0x300 + 0x40, 0x4 | (0x2 << 8));
    assert_eq!(
        gpu.gop_enable_hdmi_audio(),
        "[hdmi-audio] GOP 00:00.0: SOR0 head0 HDMI ELD+PD+GCP unmute (no GSP, like nouveau)\n\
         [hdmi-audio] GOP 00:00.0: SOR1 head1 DP ELD+PD+SF unmute (no GSP, like nouveau)"
    );

    // Nothing armed: SOR 0, head 0, HDMI, GCP unmute.
    let mut gpu = super::gpu();
    gpu.pci_bus = 0x21;
    assert_eq!(
        gpu.gop_enable_hdmi_audio(),
        "[hdmi-audio] GOP 21:00.0: SOR0 head0 HDMI ELD+PD+GCP unmute (no GSP, like nouveau)"
    );
    assert_eq!(rd32(&gpu, 0x10ec00) >> 8, 95);
    assert_eq!(rd32(&gpu, 0x10ec10) & 0x8000_0003, 0x8000_0003);
    assert_eq!(rd32(&gpu, 0x6f00cc), 0x10, "GCP unmute");
    assert_eq!(rd32(&gpu, 0x6f00c0) & 1, 1);
    assert_eq!(rd32(&gpu, 0x616618), 0, "no SF on HDMI");

    test_clock::set_auto_advance(0);
    boot_edid::swap_boot_edid(saved_boot);
}

/// The display cursor ladder comes up once, on the console GPU with the
/// RM attached, and from then on each call is its image upload: refused
/// (the software cursor takes over for that frame) or taken. An image
/// smaller than its size is refused before the RM. The ladder's own
/// failure latches for the whole boot, which one process can show only
/// instead of this: it stays on the real board's side.
#[test]
fn the_hardware_cursor_ladder_comes_up_once_and_each_image_upload_decides_its_call() {
    let _g = LOCK.lock();
    let _globals = Globals::save();
    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    let argb = alloc::vec![0xff00_ff00u32; 64 * 64];
    let no_rm = super::gpu();
    assert!(no_rm.drives_boot_display());
    assert!(
        !DrmScheme::hw_cursor_set(&no_rm, &argb, 64, 64),
        "the console GPU, but no RM to build the ladder with"
    );
    let gpu = gpu_rm();
    assert!(gpu.drives_boot_display());
    FAKE_RM.lock().hwcursor = Some((0, 2, NOT_SUPPORTED));
    let _ = take_dmesg();
    assert!(
        !DrmScheme::hw_cursor_set(&gpu, &argb, 64, 64),
        "the upload was refused"
    );
    let said: Vec<String> = take_dmesg().into_iter().map(|(_, l)| l).collect();
    assert!(
        said.iter()
            .any(|l| l.contains("hwcursor: display cursor plane READY (heads=2)")),
        "{:?}",
        said
    );
    assert!(
        said.iter().any(|l| l
            .contains("hwcursor: image upload failed status=0x56 -- software cursor takes over")),
        "{:?}",
        said
    );
    let cursor_calls = || -> Vec<&'static str> {
        FAKE_RM
            .lock()
            .calls
            .iter()
            .copied()
            .filter(|c| c.starts_with("hwcursor"))
            .collect()
    };
    assert_eq!(cursor_calls(), ["hwcursor_init", "hwcursor_image"]);

    FAKE_RM.lock().hwcursor = Some((0, 2, 0));
    assert!(DrmScheme::hw_cursor_set(&gpu, &argb, 64, 64));
    assert!(DrmScheme::hw_cursor_set(&gpu, &argb, 1, 1));
    assert_eq!(
        cursor_calls(),
        [
            "hwcursor_init",
            "hwcursor_image",
            "hwcursor_image",
            "hwcursor_image"
        ],
        "the ladder is not built again"
    );
    assert!(
        !DrmScheme::hw_cursor_set(&gpu, &argb[..10], 4, 4),
        "fewer pixels than the size says"
    );
    assert_eq!(cursor_calls().len(), 4, "refused before the RM");
}

/// The RM core is constructed once per boot: the first call asks the RM
/// and every later one answers from the cache; and a first attempt that
/// died (asked, never answered) poisons every later one instead of
/// asking again.
#[test]
fn the_rm_core_init_runs_once_and_a_dead_first_attempt_poisons_every_later_one() {
    let _g = LOCK.lock();
    let saved_status = *RM_CORE_INIT_STATUS.lock();
    let saved_attempted = RM_CORE_INIT_ATTEMPTED.load(Ordering::SeqCst);
    *RM_CORE_INIT_STATUS.lock() = None;
    RM_CORE_INIT_ATTEMPTED.store(false, Ordering::SeqCst);
    reset_fake_rm();
    let init_calls = || {
        FAKE_RM
            .lock()
            .calls
            .iter()
            .filter(|c| **c == "init_core")
            .count()
    };
    assert_eq!(rm_core_init_once(), NOT_SUPPORTED);
    assert_eq!(*RM_CORE_INIT_STATUS.lock(), Some(NOT_SUPPORTED));
    assert_eq!(rm_core_init_once(), NOT_SUPPORTED);
    assert_eq!(init_calls(), 1, "asked once");
    // Asked, never answered: poisoned from here on, and not asked again.
    *RM_CORE_INIT_STATUS.lock() = None;
    assert_eq!(rm_core_init_once(), RM_INIT_POISONED);
    assert_eq!(*RM_CORE_INIT_STATUS.lock(), None);
    assert_eq!(init_calls(), 1);
    *RM_CORE_INIT_STATUS.lock() = saved_status;
    RM_CORE_INIT_ATTEMPTED.store(saved_attempted, Ordering::SeqCst);
}
