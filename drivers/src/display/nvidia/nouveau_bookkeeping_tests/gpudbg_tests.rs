//! `/proc/gpudbg` (`debug_dump`, every register read, nothing written)
//! on the test GPU, whose BAR0 is 16 MiB of zeros a test can write to:
//! the silent chip, then a latched MMU fault and a copy engine with a
//! runlist of its own; and the two readers of the PTOP device-info
//! table behind it, `find_ce_runlist` and `ptop_report`. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own
//! because the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::utils::host_hooks as shim;
use core::sync::atomic::Ordering;

fn wr32(gpu: &NvidiaGpu, off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((gpu._bar0 + off) as *mut u32, v) }
}

fn rd32(gpu: &NvidiaGpu, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((gpu._bar0 + off) as *const u32) }
}

/// `NV_PTOP_DEVICE_INFO`: 64 words, each an entry's ENGINE_TYPE, DATA or
/// ENUM word, chained by bit 31.
const PTOP: usize = 0x0002_2700;

/// Writes one device-info entry at word `*i` and moves `*i` past it: the
/// ENGINE_TYPE word with "more follow", then the ENUM word carrying the
/// runlist and, when given, the engine.
fn ptop_entry(gpu: &NvidiaGpu, i: &mut usize, ty: u32, runlist: Option<u32>, engine: Option<u32>) {
    wr32(gpu, PTOP + *i * 4, 3 | (ty << 2) | 0x8000_0000);
    *i += 1;
    let mut w = 2u32;
    if let Some(r) = runlist {
        w |= 0x10 | (r << 21);
    }
    if let Some(e) = engine {
        w |= 0x20 | (e << 26);
    }
    wr32(gpu, PTOP + *i * 4, w);
    *i += 1;
}

/// The boot framebuffer as a test found it, put back on drop: whether
/// this GPU "drives the console" is read off it.
struct BootFb(Option<BootFbInfo>);

impl BootFb {
    fn none() -> Self {
        Self(BOOT_FB_INFO.lock().take())
    }
}

impl Drop for BootFb {
    fn drop(&mut self) {
        *BOOT_FB_INFO.lock() = self.0;
    }
}

fn line_starting(s: &str, prefix: &str) -> Option<String> {
    s.lines().find(|l| l.starts_with(prefix)).map(String::from)
}

/// On a chip that answers zero to everything the dump still says it all:
/// the FFI smoke test and the vendored FNV hash (both run for real on the
/// host), the chip id, the FIFO and MMU rows (no fault latched, so no
/// fault address), the two PBDMAs and no third, an empty PTOP, and the
/// GMMU plan it builds on the first read and keeps for the next. With
/// the DMA pool exhausted the plan is a line saying so, and nothing is
/// kept.
#[test]
fn gpudbg_on_a_silent_chip_reads_zeros_builds_the_plan_once_and_says_when_it_cannot() {
    let _g = LOCK.lock();
    let _no_boot_fb = BootFb::none();
    let gpu = super::gpu();
    assert!(gpu.bringup.lock().is_none());
    let s = DrmScheme::debug_dump(&gpu);
    let has = |needle: &str| s.contains(needle);
    assert!(
        s.starts_with(&alloc::format!(
            "[gpudbg] === nvidia-test ({}) ===\n",
            gpu.gpu_model
        )),
        "{}",
        s
    );
    assert!(
        has("nvrm-sys smoke test: C-add(17,25)=42 C->Rust-callback-saw=42 (both should be 42)"),
        "{}",
        s
    );
    assert!(
        has(
            "fnv1Hash64(\"\")=0xcbf29ce484222325 (expect 0xcbf29ce484222325) \
             fnv1Hash64(\"hello\")=0x7b495389bdbdd4c7"
        ),
        "{}",
        s
    );
    assert!(
        has(&alloc::format!(
            "arch=Turing BAR0={:#x} BAR1/fb_vaddr=0x0 fb_size=0x10000000 VRAM=8192MB",
            gpu._bar0
        )),
        "{}",
        s
    );
    for row in [
        "[gpudbg]  PMC_BOOT_0(0x0)=0x00000000 -> chipset=0x000",
        "[gpudbg]  PCFG(0x88000)=0x00000000 vendor=0x0000 device=0x0000",
        "[gpudbg]  PFB_CSTATUS(0x10020c)=0x00000000 drives_console=false",
        "[gpudbg]  --- FIFO/MMU (Step 0, read-only) ---",
        "[gpudbg]  DOORBELL_EN(0xb65000)=0x00000000 (bit31=0)",
        "[gpudbg]  RUNL0 base_lo(0x2b00)=0x00000000 base_hi=0x00000000 submit=0x00000000 cfg(0x2b0c)=0x00000000",
        "[gpudbg]  RUNL1 base_lo(0x2b10)=0x00000000 base_hi=0x00000000 submit=0x00000000 cfg(0x2b1c)=0x00000000",
        "[gpudbg]  CHAN0_CFG(0x800004)=0x00000000",
        "[gpudbg]  MMU flush PDB(0xb830a0)=0x00000000 hi(0xb830a4)=0x00000000 trigger(0xb830b0)=0x00000000",
        "[gpudbg]  --- MMU fault snapshot (read-only) ---",
        "[gpudbg]  FAULT_INFO1(0xb83090)=0x00000000 valid=0 hub=0 access=0(READ) client=0x0 reason=0(PDE)",
        "[gpudbg]  PCCSR0(0x800004)=0x00000000 enable=0 busy=0 status=0 pbdma_faulted=0 eng_faulted=0",
        "[gpudbg]  PCCSR0_INST(0x800000)=0x00000000",
        "[gpudbg]  PBDMA0 STATUS(0x40100)=0x00000000 CHANNEL=0x00000000 GP_GET=0x00000000 GP_PUT=0x00000000 GET=0x00000000 INTR_0=0x00000000",
        "[gpudbg]  PBDMA1 STATUS(0x42100)=0x00000000 CHANNEL=0x00000000 GP_GET=0x00000000 GP_PUT=0x00000000 GET=0x00000000 INTR_0=0x00000000",
        "[gpudbg]  --- PTOP device-info (0x022700, non-zero) ---\n[gpudbg]  --- GMMU tables (Step 1, built in RAM; GPU not yet pointed at them) ---",
    ] {
        assert!(has(row), "missing {:?} in {}", row, s);
    }
    for absent in [
        "(CE)",
        "FAULT_VA",
        "serves CE",
        "DEVINFO[",
        "alloc_coherent FAILED",
    ] {
        assert!(!has(absent), "{:?} in {}", absent, s);
    }
    let plan = line_starting(&s, "[gpudbg]  PD  phys: root=").expect("the plan's tables");
    assert!(gpu.bringup.lock().is_some(), "the plan is kept");
    let last = s.trim_end().lines().last().expect("a last line");
    assert!(
        last.starts_with("[gpudbg]  surfaceflip: state="),
        "{}",
        last
    );
    assert!(s.ends_with('\n'));

    // The second read finds the plan already built: the same tables, and
    // not one DMA page asked for.
    let again = shim::alone_with_the_allocator(|| {
        let again = DrmScheme::debug_dump(&gpu);
        assert_eq!(shim::ALLOC_CALLS.load(Ordering::SeqCst), 0, "rebuilt");
        again
    });
    assert_eq!(
        line_starting(&again, "[gpudbg]  PD  phys: root="),
        Some(plan),
        "the plan was rebuilt"
    );

    // No DMA pages for it: the dump says so and keeps nothing.
    *gpu.bringup.lock() = None;
    let starved = shim::alone_with_the_allocator(|| {
        shim::FAIL_ALLOC.store(true, Ordering::SeqCst);
        DrmScheme::debug_dump(&gpu)
    });
    assert!(
        starved.contains("[gpudbg]  GMMU: alloc_coherent FAILED (DMA pool exhausted)"),
        "{}",
        starved
    );
    assert!(!starved.contains("PD  phys"));
    assert!(gpu.bringup.lock().is_none(), "a plan that was not built");
}

/// A latched non-replayable fault is decoded field by field and its
/// address, engine and instance block follow; a PTOP table with a copy
/// engine on a runlist of its own (8, as the real board has it) adds the
/// CE's runlist row and the PBDMA the map routes that runlist to; and
/// every non-zero device-info word is listed raw.
#[test]
fn gpudbg_with_a_latched_fault_and_a_copy_engine_of_its_own_prints_the_fault_and_the_ce_rows() {
    let _g = LOCK.lock();
    let gpu = super::gpu();
    // FAULT_INFO1: valid, hub, ATOMIC access, client 0x3a, BAD_APERTURE.
    wr32(
        &gpu,
        0x00b8_3090,
        0x8000_0000 | (1 << 20) | (2 << 16) | (0x3a << 8) | 0xa,
    );
    wr32(&gpu, 0x00b8_3080, 0x1234_5678);
    wr32(&gpu, 0x00b8_3084, 0x1);
    wr32(&gpu, 0x00b8_3088, 0x0000_1007);
    wr32(&gpu, 0x00b8_308c, 0x2);
    // GR on runlist 0, CE (engine 5) on runlist 8.
    let mut i = 0;
    ptop_entry(&gpu, &mut i, 0x0, Some(0), None);
    ptop_entry(&gpu, &mut i, 0x13, Some(8), Some(5));
    // Runlist 8's registers, and PBDMA 9 serving it (PBDMA 1 too, which
    // the dump has already shown and does not repeat).
    wr32(&gpu, 0x2b80, 0xdead_0000);
    wr32(&gpu, 0x2b8c, 0x0000_00c0);
    wr32(&gpu, 0x2390 + 9 * 4, 1 << 8);
    wr32(&gpu, 0x2390 + 4, 1 << 8);
    wr32(&gpu, 0x0005_2100, 0x0000_0a0a);

    assert_eq!(gpu.find_ce_runlist(), Some((8, 5)));
    let s = DrmScheme::debug_dump(&gpu);
    let has = |needle: &str| s.contains(needle);
    for row in [
        "[gpudbg]  FAULT_INFO1(0xb83090)=0x80123a0a valid=1 hub=1 access=2(ATOMIC) client=0x3a reason=10(BAD_APERTURE)",
        "[gpudbg]  FAULT_VA=0x112345000 engine_id=0x7 inst=0x200001000",
        "[gpudbg]  RUNL8(CE) base_lo(0x2b80)=0xdead0000 base_hi=0x00000000 submit=0x00000000 cfg(0x2b8c)=0x000000c0",
        "[gpudbg]  PBDMA9(serves CE runl8) STATUS(0x52100)=0x00000a0a CHANNEL=0x00000000 GP_GET=0x00000000 GP_PUT=0x00000000 GET=0x00000000 INTR_0=0x00000000",
        "[gpudbg]  DEVINFO[ 0]=0x80000003\n[gpudbg]  DEVINFO[ 1]=0x00000012\n[gpudbg]  DEVINFO[ 2]=0x8000004f\n[gpudbg]  DEVINFO[ 3]=0x15000032\n[gpudbg]  --- GMMU",
    ] {
        assert!(has(row), "missing {:?} in {}", row, s);
    }
    assert_eq!(
        s.matches("PBDMA1 STATUS").count(),
        1,
        "PBDMA 1 is shown once: {}",
        s
    );
    assert!(!has("PBDMA1(serves"), "{}", s);
    assert_eq!(
        gpu.ptop_report(),
        " GR(ty=0x0)/rl=0 CE(ty=0x13)/rl=8",
        "the report of the same table"
    );
    // The last word a CE scan reads is still the 64th: the fault registers
    // live elsewhere and were not disturbed by the table.
    assert_eq!(rd32(&gpu, 0x00b8_3090), 0x8012_3a0a);
}

/// The two PTOP readers: a CE whose runlist is GR's own does not count
/// as standalone even when the table lists it before GR (the re-check
/// after the pass), a CE with no engine field reports `u32::MAX`, a
/// standalone CE wins over the first one, the words that are not entries
/// (NOT_VALID, DATA, an ENUM with neither field, a type with no runlist)
/// are stepped over, and the report names every engine type it knows,
/// with `-1` for an entry whose runlist the table does not give.
#[test]
fn the_ptop_scan_picks_the_copy_engines_runlist_and_the_report_names_every_engine() {
    let _g = LOCK.lock();
    // Nothing: no CE, no report.
    let gpu = super::gpu();
    assert_eq!(gpu.find_ce_runlist(), None);
    assert_eq!(gpu.ptop_report(), "");

    // CE before GR, on GR's runlist: the pass thought it standalone; the
    // re-check knows better and the answer is the first CE, same thing.
    let gpu = super::gpu();
    let mut i = 0;
    ptop_entry(&gpu, &mut i, 0x1, Some(8), Some(3));
    ptop_entry(&gpu, &mut i, 0x0, Some(8), None);
    ptop_entry(&gpu, &mut i, 0x2, Some(1), None);
    assert_eq!(
        gpu.find_ce_runlist(),
        Some((8, 3)),
        "the first CE, GR's runlist being no one's own"
    );
    assert_eq!(
        gpu.ptop_report(),
        " CE(ty=0x1)/rl=8 GR(ty=0x0)/rl=8 CE(ty=0x2)/rl=1"
    );

    // GR first, a CE on its runlist, then a CE of its own with no engine
    // field: the standalone one, engine unknown.
    let gpu = super::gpu();
    let mut i = 0;
    ptop_entry(&gpu, &mut i, 0x0, Some(0), None);
    ptop_entry(&gpu, &mut i, 0x3, Some(0), Some(4));
    ptop_entry(&gpu, &mut i, 0x13, Some(5), None);
    assert_eq!(gpu.find_ce_runlist(), Some((5, u32::MAX)));

    // The words that are not an engine on a runlist, and every name.
    let gpu = super::gpu();
    let mut i = 0;
    // A NOT_VALID word, a chained DATA word, then an ENUM with neither
    // field: an entry with a type and no runlist.
    wr32(&gpu, PTOP + i * 4, 0);
    i += 1;
    wr32(&gpu, PTOP + i * 4, 3 | (0xc << 2) | 0x8000_0000);
    i += 1;
    wr32(&gpu, PTOP + i * 4, 1 | 0x8000_0000);
    i += 1;
    wr32(&gpu, PTOP + i * 4, 2);
    i += 1;
    // An ENUM word alone: no type, not reported.
    wr32(&gpu, PTOP + i * 4, 2 | 0x10 | (3 << 21));
    i += 1;
    for (ty, rl) in [
        (0x8u32, 2u32),
        (0x9, 2),
        (0xa, 2),
        (0xb, 3),
        (0xd, 4),
        (0xe, 5),
        (0xf, 5),
        (0x10, 6),
        (0x14, 7),
        (0x15, 7),
        (0x7, 9),
    ] {
        ptop_entry(&gpu, &mut i, ty, Some(rl), None);
    }
    assert!(i <= 64);
    assert_eq!(gpu.find_ce_runlist(), None, "no CE among them");
    assert_eq!(
        gpu.ptop_report(),
        " VIC(ty=0xc)/rl=-1 MSPDEC(ty=0x8)/rl=2 MSPPP(ty=0x9)/rl=2 MSVLD(ty=0xa)/rl=2 \
         MSENC(ty=0xb)/rl=3 SEC2(ty=0xd)/rl=4 NVENC(ty=0xe)/rl=5 NVENC(ty=0xf)/rl=5 \
         NVDEC(ty=0x10)/rl=6 GSP(ty=0x14)/rl=7 NVJPG(ty=0x15)/rl=7 OTHER(ty=0x7)/rl=9"
    );
}
