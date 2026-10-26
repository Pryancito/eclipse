//! The `/proc` readers of the RM bring-up (`gpustep5` to `gpustep9`) on
//! the fake RM, and the lines `gpustep6` and `gpustep11`..`gpustep14`
//! print before they would touch hardware: the BAR0 chip-ID probe, the
//! core init that failed, the attach that landed (and what the RM was
//! told about the card), the one refused for want of a ladder slot, the
//! firmware's view of the card, step 8's three controls, step 9's three
//! phases, the blocks that are cached so a chunked `cat` reads one
//! string, and the ones that are not because nothing ran. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own because
//! the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::display::nvidia::rm_host_shims::Attached;

/// `RM_CORE_INIT_*` as they stood before a test, put back on drop: the
/// core is constructed once per process, and the tests here want to see
/// it asked.
struct CoreInit(Option<u32>, bool);

impl CoreInit {
    fn fresh() -> Self {
        let saved = Self(
            *RM_CORE_INIT_STATUS.lock(),
            RM_CORE_INIT_ATTEMPTED.load(Ordering::SeqCst),
        );
        *RM_CORE_INIT_STATUS.lock() = None;
        RM_CORE_INIT_ATTEMPTED.store(false, Ordering::SeqCst);
        saved
    }
}

impl Drop for CoreInit {
    fn drop(&mut self) {
        *RM_CORE_INIT_STATUS.lock() = self.0;
        RM_CORE_INIT_ATTEMPTED.store(self.1, Ordering::SeqCst);
    }
}

fn poke(gpu: &NvidiaGpu, off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((gpu._bar0 + off) as *mut u32, v) }
}

fn calls(name: &str) -> usize {
    FAKE_RM.lock().calls.iter().filter(|c| **c == name).count()
}

const PROBE_OF_ZEROS: &str =
    "[gpustep5]  BAR0 chip-ID probe: PMC_BOOT_0=0x00000000 PMC_BOOT_42=0x00000000 \
     (arch=0x0 impl=0x0; TU106 expects arch=0x16 impl=0x6)\n\
     [gpustep5]  PMC_BOOT_1=0x00000000 VGPU(bits17:16)=0x0 (0=REAL/bare-metal, 1=PV, 2=VF; \
     bare-metal PF must be 0)\n";

/// Before the attach there is nothing to read back: steps 7, 8 and 9 say
/// which step to run first, step 6's boot says the same, and on a card
/// that does not drive the monitor steps 11 to 14 are not for it.
#[test]
fn every_reader_after_the_attach_says_skipped_without_a_device_instance() {
    let _g = LOCK.lock();
    let gpu = gpu();
    reset_fake_rm();
    assert!(!gpu.drives_boot_display());
    assert_eq!(
        DrmScheme::bringup_step7(&gpu),
        "[gpustep7]  skipped (run /proc/gpustep5 (RM attach) first)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step8(&gpu),
        "[gpustep8]  skipped (run /proc/gpustep5 (RM attach) first)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step9(&gpu),
        "[gpustep9]  skipped (run /proc/gpustep5 (RM attach) first)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step6(&gpu),
        "[gpustep6]  --- Real GSP-RM boot: skipped (run /proc/gpustep5 (RM attach) first) ---\n"
    );
    assert_eq!(
        DrmScheme::bringup_step11(&gpu),
        "[gpustep11] SKIPPED on secondary GPU (already boots via /proc/gpustep6)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step12(&gpu),
        "[gpustep12] SKIPPED on secondary GPU (already boots via /proc/gpustep6)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step13(&gpu),
        "[gpustep13] SKIPPED on secondary GPU (already boots via /proc/gpustep6)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step14(&gpu),
        "[gpustep14] SKIPPED on secondary GPU (use gpustep5/6/8/9/10 directly)\n"
    );
    assert!(
        FAKE_RM.lock().calls.is_empty(),
        "{:?}",
        FAKE_RM.lock().calls
    );
}

/// Step 5 reads the chip ID off BAR0 and decodes it, then asks for the RM
/// core; when that refuses, the card is never attached, the RM's own
/// narration of the attempt is folded in, and the whole block after the
/// probe is cached so the second chunk `cat` reads is the same string.
#[test]
fn step5_decodes_the_chip_id_and_says_the_core_init_failed_without_attaching() {
    let _g = LOCK.lock();
    let _core = CoreInit::fresh();
    let gpu = gpu();
    reset_fake_rm();
    FAKE_RM.lock().narrates = true;
    poke(&gpu, 0x0, 0x166000a1);
    poke(&gpu, 0xa00, 0x16600000);
    poke(&gpu, 0x4, 0x00020000);
    let s = DrmScheme::bringup_step5(&gpu);
    assert_eq!(
        s,
        "[gpustep5]  BAR0 chip-ID probe: PMC_BOOT_0=0x166000a1 PMC_BOOT_42=0x16600000 \
         (arch=0x16 impl=0x6; TU106 expects arch=0x16 impl=0x6)\n\
         [gpustep5]  PMC_BOOT_1=0x00020000 VGPU(bits17:16)=0x2 (0=REAL/bare-metal, 1=PV, 2=VF; \
         bare-metal PF must be 0)\n\
         [gpustep5]  --- RM narration (captured) ---\n\
         [gpustep5]  | [fake-rm] init_core\n\
         [gpustep5]  --- end RM narration ---\n\
         [gpustep5]  --- Real RM attach: eclipse_rm_init_core FAILED, NV_STATUS=0x56 ---\n"
    );
    assert_eq!(calls("attach_gpu"), 0, "never attached");
    assert_eq!(*gpu.rm_device_instance.lock(), None);
    assert_eq!(DrmScheme::bringup_step5(&gpu), s, "cached, byte for byte");
    assert_eq!(calls("init_core"), 1);
}

/// With the core up, the attach hands the RM the card's PCI location, its
/// three BARs and -- as BAR1's length -- the VRAM size the driver uses,
/// and the instance that comes back is the card's from then on; an
/// instance past the ladder's slots is refused and never stored; and a
/// refusal by the RM is said in its own words. Each block is cached: a
/// second read asks the RM nothing.
#[test]
fn step5_attaches_the_card_once_and_refuses_an_instance_without_a_ladder_slot() {
    let _g = LOCK.lock();
    let _core = CoreInit::fresh();
    let mut card = gpu();
    card.pci_bus = 0x21;
    card.pci_device = 0;
    card.bar0_phys = 0xf600_0000;
    card.bar0_len = 16 << 20;
    card.bar1_phys = 0xc0_0000_0000;
    card.bar2_phys = 0xd0_0000_0000;
    card.bar2_len = 32 << 20;
    reset_fake_rm();
    {
        let mut f = FAKE_RM.lock();
        f.core = true;
        f.attach = Some(1);
        f.narrates = true;
    }
    let s = DrmScheme::bringup_step5(&card);
    assert_eq!(
        s,
        alloc::format!(
            "{}[gpustep5]  --- RM narration (captured) ---\n\
             [gpustep5]  | [fake-rm] init_core\n\
             [gpustep5]  | [fake-rm] attach_gpu\n\
             [gpustep5]  --- end RM narration ---\n\
             [gpustep5]  --- Real RM attach: gpumgrAttachGpu OK, deviceInstance=1 ---\n",
            PROBE_OF_ZEROS
        )
    );
    assert_eq!(*card.rm_device_instance.lock(), Some(1));
    assert_eq!(
        FAKE_RM.lock().attached,
        alloc::vec![Attached {
            domain: 0,
            bus: 0x21,
            device: 0,
            bar0_phys: 0xf600_0000,
            bar0_len: 16 << 20,
            bar1_phys: 0xc0_0000_0000,
            bar1_len: 8192 << 20,
            bar2_phys: 0xd0_0000_0000,
            bar2_len: 32 << 20,
        }]
    );
    assert_eq!(DrmScheme::bringup_step5(&card), s, "cached, byte for byte");
    assert_eq!(calls("attach_gpu"), 1, "asked once");
    assert_eq!(calls("init_core"), 1);

    // One slot per card, MAX_LADDER_GPUS of them: instance 8 has none.
    let refused = gpu();
    FAKE_RM.lock().attach = Some(nvidia_rm_sys::rm_init::MAX_LADDER_GPUS);
    FAKE_RM.lock().narrates = false;
    assert_eq!(
        DrmScheme::bringup_step5(&refused),
        alloc::format!(
            "{}[gpustep5]  --- Real RM attach: gpumgrAttachGpu OK, deviceInstance=8 -- REFUSED: \
             este puerto guarda ladder para 8 GPUs; sube ECLIPSE_MAX_GPUS ---\n",
            PROBE_OF_ZEROS
        )
    );
    assert_eq!(*refused.rm_device_instance.lock(), None, "never stored");

    let failed = gpu();
    FAKE_RM.lock().attach = None;
    assert_eq!(
        DrmScheme::bringup_step5(&failed),
        alloc::format!(
            "{}[gpustep5]  --- Real RM attach: gpumgrAttachGpu FAILED, NV_STATUS=0x56 ---\n",
            PROBE_OF_ZEROS
        )
    );
    assert_eq!(*failed.rm_device_instance.lock(), None);
    assert_eq!(calls("attach_gpu"), 3);
}

/// Step 7 prints the firmware's view of the card -- name, short name,
/// VRAM, L2, VBIOS identity -- as long as the name is there: an all-zero
/// name is a GSP that has not booted, a refused readback is said with its
/// status, and a name that fills its 64 bytes is printed whole.
#[test]
fn step7_prints_the_firmwares_view_of_the_card_or_says_why_not() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    assert_eq!(
        DrmScheme::bringup_step7(&gpu),
        "[gpustep7]  eclipse_rm_get_gsp_info FAILED, NV_STATUS=0x56\n"
    );
    FAKE_RM.lock().gsp_info = Some("");
    assert_eq!(
        DrmScheme::bringup_step7(&gpu),
        "[gpustep7]  GSP static info is all zeros -- GSP-RM not booted on this GPU yet (run /proc/gpustep6)\n"
    );
    FAKE_RM.lock().gsp_info = Some("NVIDIA GeForce RTX 2060 SUPER");
    assert_eq!(
        DrmScheme::bringup_step7(&gpu),
        "[gpustep7]  --- Firmware-reported GPU info (from live GSP-RM via GET_GSP_STATIC_INFO) ---\n\
         [gpustep7]  GPU name:   NVIDIA GeForce RTX 2060 SUPER\n\
         [gpustep7]  Short name: TU106\n\
         [gpustep7]  VRAM:       8192 MiB (8589934592 bytes), bus width 256 bits, ram type 11\n\
         [gpustep7]  L2 cache:   4096 KiB\n\
         [gpustep7]  VBIOS:      valid=true subvendor=0x1458 subdevice=0x3fe2\n"
    );
    let long = "NVIDIA GeForce RTX 2060 SUPER with a name the struct cannot hold whole";
    assert!(long.len() > 64);
    FAKE_RM.lock().gsp_info = Some(long);
    let s = DrmScheme::bringup_step7(&gpu);
    assert!(
        s.contains(&alloc::format!("[gpustep7]  GPU name:   {}\n", &long[..64])),
        "{}",
        s
    );
    assert_eq!(calls("get_gsp_info"), 4);
}

/// Step 8 folds the RM's narration in first, then each control's answer
/// or, control by control, the status it failed with; a refusal of the
/// whole call is said as such.
#[test]
fn step8_prints_each_controls_answer_or_its_status_under_the_rms_narration() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    FAKE_RM.lock().narrates = true;
    assert_eq!(
        DrmScheme::bringup_step8(&gpu),
        "[gpustep8]  | [fake-rm] step8\n\
         [gpustep8]  eclipse_rm_step8 FAILED, NV_STATUS=0x56 (GSP not booted? run /proc/gpustep6)\n"
    );
    FAKE_RM.lock().rm_api = Some((0, 0, 0));
    assert_eq!(
        DrmScheme::bringup_step8(&gpu),
        "[gpustep8]  | [fake-rm] step8\n\
         [gpustep8]  --- RM API controls served by live GSP-RM (GSP_RM_CONTROL RPC) ---\n\
         [gpustep8]  GET_NAME_STRING: NVIDIA GeForce RTX 2060 SUPER\n\
         [gpustep8]  GET_GID_INFO (UUID): GPU-6f3c9a1e-2b4d-4e8f-9a7c-0d1e2f3a4b5c\n\
         [gpustep8]  FB_GET_INFO_V2: heap 8192 MiB total, 7680 MiB free, bus width 256 bits\n"
    );
    FAKE_RM.lock().narrates = false;
    FAKE_RM.lock().rm_api = Some((0x51, 0x57, 0x40));
    assert_eq!(
        DrmScheme::bringup_step8(&gpu),
        "[gpustep8]  --- RM API controls served by live GSP-RM (GSP_RM_CONTROL RPC) ---\n\
         [gpustep8]  GET_NAME_STRING: NV_STATUS=0x51\n\
         [gpustep8]  GET_GID_INFO: NV_STATUS=0x57\n\
         [gpustep8]  FB_GET_INFO_V2: NV_STATUS=0x40\n"
    );
    FAKE_RM.lock().rm_api = Some((0, 0x57, 0));
    let s = DrmScheme::bringup_step8(&gpu);
    assert!(
        s.contains("GET_NAME_STRING: NVIDIA GeForce RTX 2060 SUPER\n"),
        "{}",
        s
    );
    assert!(s.contains("GET_GID_INFO: NV_STATUS=0x57\n"), "{}", s);
    assert!(s.contains("FB_GET_INFO_V2: heap 8192 MiB"), "{}", s);
    assert_eq!(calls("step8"), 4);
}

/// Step 9 names each phase's outcome -- OK, the status it failed with,
/// or not reached -- and the banner only when all three passed. The block
/// is cached when the RM ran, so a later read replays it; a refusal of
/// the call ran nothing and is not cached, so the next read asks again.
#[test]
fn step9_reports_each_phase_and_caches_only_a_run_that_ran() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    FAKE_RM.lock().narrates = true;
    assert_eq!(
        DrmScheme::bringup_step9(&gpu),
        "[gpustep9]  --- state-init narration (captured) ---\n\
         [gpustep9]  | [fake-rm] state_init\n\
         [gpustep9]  --- end narration ---\n\
         [gpustep9]  eclipse_rm_state_init FAILED, NV_STATUS=0x56 (GSP not booted? run /proc/gpustep6)\n"
    );
    assert!(
        gpu.state_init_result.lock().is_none(),
        "a refusal is not cached"
    );
    FAKE_RM.lock().narrates = false;
    FAKE_RM.lock().state_init = Some((0, 0x40, 0xFFFF_FFFF));
    let stopped = DrmScheme::bringup_step9(&gpu);
    assert_eq!(
        stopped,
        "[gpustep9]  gpuStatePreInit: OK\n\
         [gpustep9]  gpuStateInit:    FAILED NV_STATUS=0x40\n\
         [gpustep9]  gpuStateLoad:    not reached\n"
    );
    assert_eq!(calls("state_init"), 2);
    FAKE_RM.lock().state_init = Some((0, 0, 0));
    assert_eq!(
        DrmScheme::bringup_step9(&gpu),
        stopped,
        "cached: the RM is not re-run"
    );
    assert_eq!(calls("state_init"), 2);

    let loaded = gpu_rm();
    FAKE_RM.lock().state_init = Some((0, 0, 0));
    assert_eq!(
        DrmScheme::bringup_step9(&loaded),
        "[gpustep9]  gpuStatePreInit: OK\n\
         [gpustep9]  gpuStateInit:    OK\n\
         [gpustep9]  gpuStateLoad:    OK\n\
         [gpustep9]  --- FULL RmInitAdapter-equivalent bring-up COMPLETE: GPU is state-loaded ---\n"
    );
}

/// Step 6 on a card that does not drive the monitor goes straight to the
/// GSP boot, which without `gsp.bin` says so and repeats what the loader
/// recorded at boot; a boot already run is replayed from its cache.
#[test]
fn step6_on_the_secondary_says_the_firmware_is_missing_and_what_the_loader_said() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    assert!(!gpu.drives_boot_display());
    assert_eq!(
        DrmScheme::bringup_step6(&gpu),
        "[gpustep6]  --- Real GSP-RM boot: skipped (no gsp.bin in driver) ---\n\
         [gpustep6]  boot-time firmware load: no status recorded (loader never ran?)\n"
    );
    gpu.set_gsp_firmware_status(String::from("gsp.bin not found under /lib/firmware"));
    assert_eq!(
        DrmScheme::bringup_step6(&gpu),
        "[gpustep6]  --- Real GSP-RM boot: skipped (no gsp.bin in driver) ---\n\
         [gpustep6]  boot-time firmware load: gsp.bin not found under /lib/firmware\n"
    );
    *gpu.gsp_init_result.lock() = Some(String::from("[gpustep6]  --- Real GSP-RM boot: OK ---\n"));
    assert_eq!(
        DrmScheme::bringup_step6(&gpu),
        "[gpustep6]  --- Real GSP-RM boot: OK ---\n"
    );
    assert!(
        FAKE_RM.lock().calls.is_empty(),
        "{:?}",
        FAKE_RM.lock().calls
    );
}
