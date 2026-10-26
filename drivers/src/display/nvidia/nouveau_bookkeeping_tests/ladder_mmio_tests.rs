//! The first three rungs of the copy-engine bring-up ladder
//! (`/proc/gpustep2`, `3` and `4`) on the test GPU, whose BAR0 is 16 MiB
//! of memory: every register write is echoed back by the next read, so
//! a poll on a bit the chip would clear spins to its limit unless the
//! spin hook plays the chip. Through the PRAMIN window the VRAM side of
//! the plan (instance block, runlist, USERD, BAR2 instance block) lands
//! in that same memory, where a test reads it back. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own
//! because the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::utils::host_hooks as shim;
use core::sync::atomic::{AtomicU32, Ordering};

fn wr32(gpu: &NvidiaGpu, off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((gpu._bar0 + off) as *mut u32, v) }
}

fn rd32(gpu: &NvidiaGpu, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((gpu._bar0 + off) as *const u32) }
}

/// Where a VRAM offset lands in the test BAR0 through the PRAMIN window
/// (the window base register is memory too, so only the low 16 bits of
/// the offset pick the cell).
fn pramin(gpu: &NvidiaGpu, vram_off: u64) -> usize {
    gpu._bar0 + 0x0070_0000 + (vram_off & 0xffff) as usize
}

fn pramin_rd(gpu: &NvidiaGpu, vram_off: u64) -> u32 {
    unsafe { core::ptr::read_volatile(pramin(gpu, vram_off) as *const u32) }
}

/// `NV_PTOP_DEVICE_INFO`: 64 words, each an entry's ENGINE_TYPE, DATA or
/// ENUM word, chained by bit 31.
const PTOP: usize = 0x0002_2700;

/// Writes one device-info entry at word `*i` and moves `*i` past it.
fn ptop_entry(gpu: &NvidiaGpu, i: &mut usize, ty: u32, runlist: u32, engine: u32) {
    wr32(gpu, PTOP + *i * 4, 3 | (ty << 2) | 0x8000_0000);
    *i += 1;
    wr32(
        gpu,
        PTOP + *i * 4,
        2 | 0x10 | (runlist << 21) | 0x20 | (engine << 26),
    );
    *i += 1;
}

/// A GR on runlist 0 and a copy engine on runlist 8 (engine 7), as the
/// board has them; PBDMA 9 serves runlist 8.
fn board_topology(gpu: &NvidiaGpu) {
    let mut i = 0;
    ptop_entry(gpu, &mut i, 0x00, 0, 0);
    ptop_entry(gpu, &mut i, 0x13, 8, 7);
    wr32(gpu, 0x0000_2390 + 9 * 4, 1 << 8);
}

/// The boot framebuffer as a test found it, put back on drop. `console`
/// makes the test GPU the one that drives it: the ladder targets that GPU
/// and skips every other.
struct BootFb(Option<BootFbInfo>);

impl BootFb {
    fn none() -> Self {
        Self(BOOT_FB_INFO.lock().take())
    }

    fn console() -> Self {
        let saved = Self(BOOT_FB_INFO.lock().take());
        set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
        saved
    }
}

impl Drop for BootFb {
    fn drop(&mut self) {
        *BOOT_FB_INFO.lock() = self.0;
    }
}

/// The spin hook for the duration of a test, cleared on drop.
struct Spin;

impl Spin {
    fn install(hook: impl Fn() + Send + 'static) -> Self {
        *GPU_SPIN_HOOK.lock() = Some(alloc::boxed::Box::new(hook));
        Spin
    }

    /// The MMU's side of a flush: the trigger's bit 31 drops on the next
    /// spin. Without it the echoing BAR0 keeps the bit set for ever.
    fn clearing_the_flush(gpu: &NvidiaGpu) -> Self {
        let bar0 = gpu._bar0;
        Self::install(move || unsafe {
            let p = (bar0 + 0x00b8_30b0) as *mut u32;
            p.write_volatile(p.read_volatile() & !0x8000_0000);
        })
    }

    /// The flush as above, plus the PBDMA's: GP_GET in USERD catches up
    /// with `target` after `after` spins (counted on `spins`).
    fn fetching(
        gpu: &NvidiaGpu,
        gp_get: usize,
        target: u32,
        after: u32,
        spins: Arc<AtomicU32>,
    ) -> Self {
        let bar0 = gpu._bar0;
        Self::install(move || unsafe {
            let p = (bar0 + 0x00b8_30b0) as *mut u32;
            p.write_volatile(p.read_volatile() & !0x8000_0000);
            if spins.fetch_add(1, Ordering::SeqCst) + 1 == after {
                (gp_get as *mut u32).write_volatile(target);
            }
        })
    }
}

impl Spin {
    /// The chip as a whole: the flush trigger and the BAR2 bind wait drop
    /// on the next spin; runlist 8's pending bit drops once the doorbell
    /// has been rung with runlist 8's token (the kick that wakes the
    /// scheduler, rung just before the commit); GP_GET catches up on spin
    /// `gp_get_at`, and the scheduler's runlist fetch goes idle on spin
    /// `fetch_idle_at`.
    fn chip(gpu: &NvidiaGpu, gp_get_at: u32, fetch_idle_at: u32) -> Self {
        let bar0 = gpu._bar0;
        let gp_get = pramin(gpu, 0x0300_2000 + 0x88);
        let spins = AtomicU32::new(0);
        Self::install(move || unsafe {
            let clear = |off: usize, bits: u32| {
                let p = (bar0 + off) as *mut u32;
                p.write_volatile(p.read_volatile() & !bits);
            };
            clear(0x00b8_30b0, 0x8000_0000);
            clear(0x00b8_0f50, 0xc);
            if ((bar0 + 0x00bb_0090) as *const u32).read_volatile() == 8 << 16 {
                clear(0x2b8c, 0x8000);
            }
            let n = spins.fetch_add(1, Ordering::SeqCst) + 1;
            if n == gp_get_at {
                (gp_get as *mut u32).write_volatile(1);
            }
            if n == fetch_idle_at {
                clear(0x263c, 4);
            }
        })
    }
}

impl Drop for Spin {
    fn drop(&mut self) {
        *GPU_SPIN_HOOK.lock() = None;
    }
}

/// The `exec_fast` switch as a test found it, put back on drop.
struct ExecFast(bool);

impl ExecFast {
    fn pinned(v: bool) -> Self {
        let saved = Self(nv::exec_fast_enabled());
        nv::set_exec_fast_enabled(v);
        saved
    }
}

impl Drop for ExecFast {
    fn drop(&mut self) {
        nv::set_exec_fast_enabled(self.0);
    }
}

fn line_starting(s: &str, prefix: &str) -> String {
    s.lines()
        .find(|l| l.starts_with(prefix))
        .map(String::from)
        .unwrap_or_else(|| panic!("no line starts with {:?} in:\n{}", prefix, s))
}

fn has_line(s: &str, line: &str) -> bool {
    s.lines().any(|l| l == line)
}

/// The three rungs look at the boot framebuffer first: a GPU that does
/// not drive it is skipped in one line, and nothing is allocated or
/// written. And on the console GPU with the DMA pool exhausted, each
/// says so and keeps no plan.
#[test]
fn the_three_rungs_skip_a_gpu_off_the_console_and_say_when_the_pool_is_dry() {
    let _g = LOCK.lock();
    let no_boot_fb = BootFb::none();
    let gpu = super::gpu();
    assert_eq!(
        DrmScheme::bringup_step2(&gpu),
        "[gpustep2] nvidia-test (GeForce RTX 2060 Super) SKIPPED — not the console GPU (bar1_phys=0x0); only testing the single available GPU\n"
    );
    assert_eq!(
        DrmScheme::bringup_step3(&gpu),
        "[gpustep3] nvidia-test SKIPPED — not the console GPU; only testing the single available GPU\n"
    );
    assert_eq!(
        DrmScheme::bringup_step4(&gpu),
        "[gpustep4] nvidia-test SKIPPED — not the console GPU; only testing the single available GPU\n"
    );
    assert!(gpu.bringup.lock().is_none());
    assert_eq!(rd32(&gpu, 0x1700), 0, "the PRAMIN window was never moved");

    drop(no_boot_fb);
    let _console = BootFb::console();
    assert!(gpu.drives_boot_display());
    let (s2, s3, s4) = shim::alone_with_the_allocator(|| {
        shim::FAIL_ALLOC.store(true, Ordering::SeqCst);
        (
            DrmScheme::bringup_step2(&gpu),
            DrmScheme::bringup_step3(&gpu),
            DrmScheme::bringup_step4(&gpu),
        )
    });
    assert_eq!(s2, "[gpustep2] nvidia-test alloc_coherent FAILED\n");
    assert_eq!(s3, "[gpustep3] nvidia-test alloc_coherent FAILED\n");
    assert_eq!(s4, "[gpustep4] nvidia-test alloc_coherent FAILED\n");
    assert!(gpu.bringup.lock().is_none(), "a plan that was not built");
    assert_eq!(rd32(&gpu, 0x1700), 0);
}

/// Step 2 on a chip that answers zero: the PRAMIN ladder reads no
/// sentinel anywhere, the instance block lands in VRAM with the PDB join
/// and the USERD pointer, the fault buffer is armed with the count the
/// chip reports (capped at the buffer), BAR2 is bound to its own
/// instance block, and the flush is accepted once the MMU (the hook)
/// drops the trigger bit.
#[test]
fn step2_lays_the_instance_block_arms_the_fault_buffer_binds_bar2_and_flushes() {
    let _g = LOCK.lock();
    let _console = BootFb::console();
    let gpu = super::gpu();
    let _mmu = Spin::clearing_the_flush(&gpu);
    // The chip reports 0x12 fault-buffer entries in flight.
    wr32(&gpu, 0x00b8_3010, 0x12);

    let s = DrmScheme::bringup_step2(&gpu);
    let plan = gpu.bringup.lock();
    let b = plan.as_ref().expect("the plan was built and kept");
    assert_eq!((b.va_base, b.vram_base), (0x0020_0000, 0x0300_0000));
    let root = b.root.paddr() as u64;
    let fb = b.fault_buf.paddr() as u64;

    assert!(has_line(
        &s,
        "[gpustep2] === nvidia-test (GeForce RTX 2060 Super) — Step 2: instance block + GMMU flush ==="
    ));
    assert!(has_line(
        &s,
        "[gpustep2]  PRAMIN ladder: 0=ok 1M=ok 4M=ok 16M=ok 64M=ok 256M=ok 512M=ok 1G=ok 2G=ok (highest ok=0x80000000)"
    ));
    assert!(has_line(
        &s,
        "[gpustep2]  PRAMIN self-test=true inst@VRAM 0x3000000"
    ));
    let base = root | 0xc06;
    assert!(
        has_line(
            &s,
            &alloc::format!(
                "[gpustep2]  inst@0x200={:08x}{:08x} @0x208=0001ffffffffffff userd@0x008=0000000003002000",
                base >> 32,
                base as u32
            )
        ),
        "{}",
        s
    );
    assert!(has_line(
        &s,
        "[gpustep2]  CE ctx (disarmed): inst@0x220=0000000000000000 @0x0ac=00000000"
    ));
    // The instance block in VRAM, read back through the window: the PDB
    // join's descriptor table (entry 0 the real PDB, 1..63 the 0x1/0x1
    // fillers), the GPFIFO VA with its log2 size, the ramfc constants.
    let inst = b.inst_vram();
    assert_eq!(pramin_rd(&gpu, inst + 0x2a0), base as u32);
    assert_eq!(pramin_rd(&gpu, inst + 0x2a4), (base >> 32) as u32);
    assert_eq!(pramin_rd(&gpu, inst + 0x2b0), 1);
    assert_eq!(pramin_rd(&gpu, inst + 0x2b4), 1);
    assert_eq!(pramin_rd(&gpu, inst + 0x2a0 + 63 * 0x10), 1);
    assert_eq!(pramin_rd(&gpu, inst + 0x298), 1);
    assert_eq!(pramin_rd(&gpu, inst + 0x048), 0x0020_4000);
    assert_eq!(
        pramin_rd(&gpu, inst + 0x04c),
        9 << 16,
        "512 entries of 8 B: log2 = 9"
    );
    assert_eq!(pramin_rd(&gpu, inst + 0x010), 0xface);
    assert_eq!(pramin_rd(&gpu, inst + 0x0e4), 0x20);
    assert_eq!(pramin_rd(&gpu, inst + 0x0f4), 0x1100);
    assert_eq!(pramin_rd(&gpu, inst + 0x0f8), 0x1000_3080);

    // The fault buffer: the chip's count latched and read back, the
    // buffer's address in PHYSICAL/SYS_COH/VOL form, ENABLE with the count.
    let lo = (fb as u32 & 0xffff_f000) | 0xd;
    assert!(
        has_line(
            &s,
            &alloc::format!(
                "[gpustep2]  FAULT_BUF: hw_count=0x12 buf_phys={:#x} LO(0xb83000)={:#010x} HI={:#010x} SIZE(0xb83010)=0x80000012",
                fb,
                lo,
                (fb >> 32) as u32
            )
        ),
        "{}",
        s
    );
    assert_eq!(rd32(&gpu, 0x00b8_3000), lo);
    assert_eq!(rd32(&gpu, 0x00b8_3004), (fb >> 32) as u32);
    assert_eq!(rd32(&gpu, 0x00b8_3010), 0x8000_0012);
    // BAR2: bound to the instance block at vram_base + 0x3000, which got
    // the same PDB join.
    assert!(has_line(
        &s,
        "[gpustep2]  BAR2(0xb80f48) 0x00000000->0x80003003 wait(0xb80f50)=0x00000000 PCE_MAP(0x104028)=0x00000000"
    ));
    assert_eq!(pramin_rd(&gpu, b.bar2_inst_vram() + 0x200), base as u32);
    assert_eq!(pramin_rd(&gpu, b.bar2_inst_vram() + 0x208), 0xffff_ffff);
    // The flush: PDB written as root >> 8, trigger seen clear.
    assert!(
        has_line(
            &s,
            &alloc::format!(
                "[gpustep2]  flush PDB=(root>>8)={:#x}  trigger(0xb830b0) pre=0x00000000 post=0x00000001 bit31_cleared=true",
                root >> 8
            )
        ),
        "{}",
        s
    );
    assert_eq!(rd32(&gpu, 0x00b8_30a0), (root >> 8) as u32);
    assert_eq!(rd32(&gpu, 0x00b8_30a4), 0);
    assert_eq!(
        rd32(&gpu, 0x1700),
        0x300,
        "the PRAMIN window was last moved to the instance block's 64 KiB"
    );
    assert!(s.ends_with(
        "[gpustep2]  OK — GMMU accepted the PDB, MMU not wedged. Ready for Step 3 (runlist + doorbell).\n"
    ));
}

/// The two ways the flush does not go through: with the trigger bit
/// already set nothing is written and the step says ABORTED; with a chip
/// that never drops it (no hook) the poll runs out and the step says
/// TIMEOUT. A second run reuses the plan (no DMA page asked for), and a
/// count past the buffer is capped at the buffer.
#[test]
fn step2_says_aborted_on_a_flush_in_flight_and_timeout_when_the_bit_never_drops() {
    let _g = LOCK.lock();
    let _console = BootFb::console();
    let gpu = super::gpu();
    wr32(&gpu, 0x00b8_30b0, 0x8000_0000);
    wr32(&gpu, 0x00b8_3010, 0x3000);
    let s = DrmScheme::bringup_step2(&gpu);
    assert!(
        line_starting(&s, "[gpustep2]  FAULT_BUF:").contains(" hw_count=0x3000 ")
            && s.contains(" SIZE(0xb83010)=0x80002000"),
        "{}",
        s
    );
    assert!(
        line_starting(&s, "[gpustep2]  flush PDB=")
            .ends_with("trigger(0xb830b0) pre=0x80000000 post=0x80000000 bit31_cleared=false"),
        "{}",
        s
    );
    assert!(s.ends_with(
        "[gpustep2]  ABORTED — a flush was already in flight (bit31 set); no write performed.\n"
    ));
    assert_eq!(
        rd32(&gpu, 0x00b8_30a0),
        0,
        "the PDB register was not written"
    );

    wr32(&gpu, 0x00b8_30b0, 0);
    let root = gpu.bringup.lock().as_ref().unwrap().root.paddr() as u64;
    let again = shim::alone_with_the_allocator(|| {
        let s = DrmScheme::bringup_step2(&gpu);
        assert_eq!(
            shim::ALLOC_CALLS.load(Ordering::SeqCst),
            0,
            "the plan was rebuilt"
        );
        s
    });
    assert!(
        line_starting(&again, "[gpustep2]  flush PDB=")
            .ends_with("trigger(0xb830b0) pre=0x00000000 post=0x80000001 bit31_cleared=false"),
        "{}",
        again
    );
    assert!(again.ends_with(
        "[gpustep2]  TIMEOUT — bit31 never cleared. Suspect bad PDB; inspect /proc/gpudbg fault regs (do NOT re-trigger).\n"
    ));
    assert_eq!(
        rd32(&gpu, 0x00b8_30a0),
        (root >> 8) as u32,
        "written this time"
    );
}

/// Step 3 enables the doorbell, writes the runlist (two entries, read
/// back through the window) and commits it, then enables the channel;
/// with the pending bit stuck it says TIMEOUT instead of OK.
#[test]
fn step3_enables_the_doorbell_commits_the_runlist_and_enables_the_channel() {
    let _g = LOCK.lock();
    let _console = BootFb::console();
    let gpu = super::gpu();
    let s = DrmScheme::bringup_step3(&gpu);
    assert!(has_line(
        &s,
        "[gpustep3] === nvidia-test (GeForce RTX 2060 Super) — Step 3: doorbell + runlist commit (empty GPFIFO) ==="
    ));
    assert!(has_line(
        &s,
        "[gpustep3]  doorbell-en(0xb65000) 0x00000000 -> 0x80000000 (bit31=1)"
    ));
    assert!(has_line(
        &s,
        "[gpustep3]  runlist@0x3001000 commit RUNL0 cfg(0x2b0c)=0x00000000 pending_cleared=true"
    ));
    assert!(has_line(
        &s,
        "[gpustep3]  chan0-cfg(0x800004) 0x00000000 -> 0x00000400"
    ));
    assert!(s.ends_with(
        "[gpustep3]  OK — scheduler accepted the runlist, no fault. Ready for Step 4 (ring doorbell, empty PB).\n"
    ));
    assert_eq!(
        (rd32(&gpu, 0x2b00), rd32(&gpu, 0x2b04), rd32(&gpu, 0x2b08)),
        (0x0300_1000, 0, 2),
        "runlist base and the submitting count"
    );
    let plan = gpu.bringup.lock();
    let b = plan.as_ref().unwrap();
    let rl = b.runlist_vram();
    let words: Vec<u32> = (0..8).map(|i| pramin_rd(&gpu, rl + i * 4)).collect();
    assert_eq!(
        words,
        [0x8003_0001, 1, 0, 0, 0x0300_2000, 0, 0x0300_0000, 0],
        "cgrp entry, then the chan entry with USERD and the instance block"
    );
    assert_eq!(pramin_rd(&gpu, b.inst_vram() + 0x008), 0x0300_2000);
    drop(plan);

    wr32(&gpu, 0x2b0c, 0x8000);
    let stuck = DrmScheme::bringup_step3(&gpu);
    assert!(has_line(
        &stuck,
        "[gpustep3]  runlist@0x3001000 commit RUNL0 cfg(0x2b0c)=0x00008000 pending_cleared=false"
    ));
    assert!(has_line(
        &stuck,
        "[gpustep3]  doorbell-en(0xb65000) 0x80000000 -> 0x80000000 (bit31=1)"
    ));
    assert!(stuck.ends_with(
        "[gpustep3]  TIMEOUT — runlist pending bit never cleared. Inspect /proc/gpudbg; runl_id 0 may be wrong (do NOT re-commit).\n"
    ));
}

/// Step 4 on the board's topology: the channel is set up on the copy
/// engine's runlist (8), devinit is seen done, the engine's status is
/// decoded, GP_PUT advances to 1, the doorbell takes the runlist/channel
/// token, GP_GET catches up (the hook's PBDMA) and the step says OK, with
/// PBDMA 9's block, status and map rows and the runlist as written.
#[test]
fn step4_sets_the_channel_up_rings_the_doorbell_and_sees_gp_get_catch_up() {
    let _g = LOCK.lock();
    let _console = BootFb::console();
    let gpu = super::gpu();
    board_topology(&gpu);
    wr32(&gpu, 0x0011_8128, 1);
    wr32(&gpu, 0x0011_8234, 0xff);
    wr32(&gpu, 0x2640 + 7 * 8, 0x3005);
    wr32(&gpu, 0x2644 + 7 * 8, 0xdead_beef);
    wr32(&gpu, 0x3080 + 9 * 4, 0x2005);
    let gp_get = pramin(&gpu, 0x0300_2000 + 0x88);
    let spins = Arc::new(AtomicU32::new(0));
    let _pbdma = Spin::fetching(&gpu, gp_get, 1, 100, spins.clone());

    let s = DrmScheme::bringup_step4(&gpu);
    assert!(has_line(
        &s,
        "[gpustep4] === nvidia-test (GeForce RTX 2060 Super) — Step 4: ring doorbell with SET_OBJECT(0xC5B5) ==="
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PMC_ENABLE(0x200) pre=0x00000000 post=0x00000100 (FIFO bit 0x100: pre=0 post=1)"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PTOP-discovered CE runlist id=8 engine_id=7 (fallback-to-0=false) channel setup: runlist_commit=true"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PTOP entries: GR(ty=0x0)/rl=0 CE(ty=0x13)/rl=8"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  DEVINIT_WAIT: 0x118128=0x00000001(bit0=1) 0x118234=0x000000ff(low8=0xff) devinit_done=true"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  SCHED_STATUS(0x263c)=0x00000000 chsw_in_progress=0 runlist_fetch_busy=0"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  ENGINE_STATUS(engine7)=0x00003005 ctx_status=1 id=0x5 id_type=1 engine_busy=0 faulted=0 eng_reload=0  DEBUG=0xdeadbeef"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PFIFO_INTR_0(0x2100) pre=0xffffffff post=0xffffffff (new bits=0x00000000)"
    ));
    assert!(!s.contains("retry:"), "{}", s);
    assert!(has_line(
        &s,
        "[gpustep4]  SCHED_STATUS re-poll(0x263c)=0x00000000 runlist_fetch_busy_cleared=true after_iters=0"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  fresh fault: INFO1=0x00000000 valid=0 access=0 reason=0 VA=0x000000000 eng=0x0"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  pb_va=0x203000 n=2 slot=0 GP_PUT 0->1 GP_GET 0->1 advanced=true doorbell=0xbb0090 token=0x80000"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  chan0-cfg(0x800004)=0x00000400 status=0"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PBDMA(runl8's, raw block): p9[STATUS=0x00000000 CHANNEL=0x00000000 GP_GET=0x00000000 GP_PUT=0x00000000 GET=0x00000000 INTR_0=0xffffffff]"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PFIFO_PBDMA_STATUS(runl8's PBDMAs): q9=0x00002005(chan_status=1 id=0x5)"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PBDMA_MAP servers-of-runl8: p9  all-nonzero: p9=0x0100"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  SCHED_DISABLE(0x2630)=0x00000000 (runl8 bit=0)  runlist@0x3001000 cgrp[80030001 00000001 00000000 00000000] chan[03002000 00000000 03000000 00000000]"
    ));
    assert!(s.ends_with(
        "[gpustep4]  OK — channel fetched the pushbuffer via GMMU and bound the copy class, no fault. Ready for Step 5 (real copy).\n"
    ));
    // The registers behind the lines: the fault clear, GP_PUT in USERD,
    // the doorbell token, the channel bound to its instance block, the
    // runlist committed on runlist 8's registers, PBDMA 9 un-suspended.
    assert_eq!(rd32(&gpu, 0x00b8_3094), 0x8000_0000);
    assert_eq!(pramin_rd(&gpu, 0x0300_2000 + 0x8c), 1);
    assert_eq!(rd32(&gpu, 0x00bb_0090), 8 << 16);
    assert_eq!(rd32(&gpu, 0x0080_0000), 0x8000_0000 | (0x0300_0000 >> 12));
    assert_eq!(
        (rd32(&gpu, 0x2b80), rd32(&gpu, 0x2b88)),
        (0x0300_1000, 2),
        "runlist 8's base and count"
    );
    assert_eq!(rd32(&gpu, 0x2b00), 0, "runlist 0 was left alone");
    assert_eq!(
        rd32(&gpu, 0x0004_012c + 9 * 0x2000),
        1_000_000,
        "PBDMA 9's timeout"
    );
    assert_eq!(rd32(&gpu, 0x0000_2140), 0x7fff_ffff);
    assert!(
        spins.load(Ordering::SeqCst) >= 100,
        "GP_GET was polled until it caught up"
    );
    // The GPFIFO entry at slot 0 names the pushbuffer and its two words.
    let plan = gpu.bringup.lock();
    let b = plan.as_ref().unwrap();
    let gp = b.gpfifo.vaddr() as *const u32;
    assert_eq!(
        unsafe { (gp.read_volatile(), gp.add(1).read_volatile()) },
        (0x0020_3000, 2 << 10)
    );
}

/// Without a PTOP table the runlist falls back to 0 and the engine is
/// unknown (no engine row, token 0); a GP_GET that never moves gets the
/// one retry (interrupts acked, channel set up again, doorbell rung
/// again) and the step says TIMEOUT with no PBDMA serving the runlist.
#[test]
fn step4_without_a_catch_up_retries_once_and_says_timeout() {
    let _g = LOCK.lock();
    let _console = BootFb::console();
    let gpu = super::gpu();
    let _mmu = Spin::clearing_the_flush(&gpu);
    let s = DrmScheme::bringup_step4(&gpu);
    assert!(has_line(
        &s,
        "[gpustep4]  PTOP-discovered CE runlist id=0 engine_id=4294967295 (fallback-to-0=true) channel setup: runlist_commit=true"
    ));
    assert!(has_line(&s, "[gpustep4]  PTOP entries:"));
    assert!(!s.contains("ENGINE_STATUS("), "{}", s);
    assert!(has_line(
        &s,
        "[gpustep4]  DEVINIT_WAIT: 0x118128=0x00000000(bit0=0) 0x118234=0x00000000(low8=0x00) devinit_done=false"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  retry: ack_intr + re-commit(true) + re-ring -> advanced=false"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  pb_va=0x203000 n=2 slot=0 GP_PUT 0->1 GP_GET 0->0 advanced=false doorbell=0xbb0090 token=0x0"
    ));
    assert!(has_line(&s, "[gpustep4]  PBDMA(runl0's, raw block):"));
    assert!(has_line(
        &s,
        "[gpustep4]  PFIFO_PBDMA_STATUS(runl0's PBDMAs):"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  PBDMA_MAP servers-of-runl0: NONE(!)  all-nonzero:"
    ));
    assert!(s.ends_with(
        "[gpustep4]  TIMEOUT — GP_GET did not advance; PBDMA likely faulted (GPFIFO/pushbuffer mapping). Inspect /proc/gpudbg (do NOT re-ring).\n"
    ));
    assert_eq!(rd32(&gpu, 0x00bb_0090), 0);
    assert_eq!((rd32(&gpu, 0x2b00), rd32(&gpu, 0x2b08)), (0x0300_1000, 2));
    assert_eq!(
        pramin_rd(&gpu, 0x0300_2000 + 0x8c),
        1,
        "GP_PUT stays advanced"
    );
}

/// The waits the first run never had to wait for: BAR2's bind settling,
/// the runlist commit's pending bit (which the chip only drops once the
/// doorbell carries runlist 8's token), GP_GET only catching up on the
/// retry, and the runlist fetch busy for a few polls. The step reports
/// the retry that worked and how many polls the fetch took.
#[test]
fn step4_waits_out_the_bind_the_commit_the_retry_and_the_fetch() {
    let _g = LOCK.lock();
    let _console = BootFb::console();
    let gpu = super::gpu();
    board_topology(&gpu);
    wr32(&gpu, 0x00b8_0f50, 0xc);
    wr32(&gpu, 0x2b8c, 0x8000);
    wr32(&gpu, 0x263c, 4);
    wr32(&gpu, 0x0011_8128, 1);
    // One spin for the flush and one for the commit, then the first
    // GP_GET poll runs its five million out; the catch-up lands in the
    // retry's poll.
    let _chip = Spin::chip(&gpu, 5_000_100, 5_000_103);
    let s = DrmScheme::bringup_step4(&gpu);
    assert!(has_line(
        &s,
        "[gpustep4]  PTOP-discovered CE runlist id=8 engine_id=7 (fallback-to-0=false) channel setup: runlist_commit=true"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  ENGINE_STATUS(engine7)=0x00000000 ctx_status=0 id=0x0 id_type=0 engine_busy=0 faulted=0 eng_reload=0  DEBUG=0x00000000"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  SCHED_STATUS(0x263c)=0x00000004 chsw_in_progress=0 runlist_fetch_busy=1"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  DEVINIT_WAIT: 0x118128=0x00000001(bit0=1) 0x118234=0x00000000(low8=0x00) devinit_done=false"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  retry: ack_intr + re-commit(true) + re-ring -> advanced=true"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  SCHED_STATUS re-poll(0x263c)=0x00000000 runlist_fetch_busy_cleared=true after_iters=3"
    ));
    assert!(has_line(
        &s,
        "[gpustep4]  pb_va=0x203000 n=2 slot=0 GP_PUT 0->1 GP_GET 0->1 advanced=true doorbell=0xbb0090 token=0x80000"
    ));
    assert!(s.ends_with(
        "[gpustep4]  OK — channel fetched the pushbuffer via GMMU and bound the copy class, no fault. Ready for Step 5 (real copy).\n"
    ));
    assert_eq!(rd32(&gpu, 0x00b8_0f50), 0, "the bind settled");
    assert_eq!(rd32(&gpu, 0x2b8c), 0, "the commit went through");
}

/// The compute ioctl's dispatcher: the console GPU is not the compute
/// device (ENODEV, whatever the op), INFO is the role line plus the two
/// switches and is ready only with the RM attached, SAXPY runs the GR
/// ladder up to step 23 and BENCH the benchmark (both say skipped on a
/// cold GPU and FAILED on the fake RM, status 5 either way), and an op
/// nobody defined is EINVAL.
#[test]
fn compute_launch_answers_by_role_and_readiness_and_runs_the_ladders() {
    use crate::scheme::drm::{COMPUTE_OP_BENCH, COMPUTE_OP_INFO, COMPUTE_OP_SAXPY};
    let _g = LOCK.lock();
    let console = BootFb::console();
    let gpu = super::gpu();
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_INFO);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (-19, 0, 0));
    assert_eq!(
        r.report,
        "GPU 00:00.0 is the console GPU — compute lives on the secondary\n"
    );
    assert_eq!(
        DrmScheme::compute_launch(&gpu, COMPUTE_OP_BENCH).status,
        -19
    );
    drop(console);
    let _no_boot_fb = BootFb::none();

    let _fast = ExecFast::pinned(false);
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_INFO);
    assert_eq!(r.status, -19, "cold");
    assert_eq!(
        r.report,
        "00:00.0 GeForce RTX 2060 Super role=compute rm=cold nodes=/dev/dri/card0,/dev/dri/card1,/dev/dri/renderD128,/dev/dri/renderD129\n\
         nouveau_uapi=on\nexec_fast=off\n\
         nvk_compute=/dev/dri/card0 (nouveau); this ioctl is eclipse-compute (Mesa skips card1)\n"
    );
    nv::set_exec_fast_enabled(true);
    assert!(DrmScheme::compute_launch(&gpu, COMPUTE_OP_INFO)
        .report
        .contains("\nnouveau_uapi=on\nexec_fast=on\n"));
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_SAXPY);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (5, 0, 32));
    assert_eq!(
        r.report,
        "[gpustep23] skipped (bring the GPU up first, then gpustep16/17)\n"
    );
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_BENCH);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (5, 0, 0));
    assert_eq!(r.report, "[gpubench] skipped (run /proc/gpuinit first)\n");
    let r = DrmScheme::compute_launch(&gpu, 9);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (-22, 0, 0));
    assert_eq!(r.report, "unknown compute op 9\n");

    let ready = super::gpu_rm();
    let r = DrmScheme::compute_launch(&ready, COMPUTE_OP_INFO);
    assert_eq!(r.status, 0);
    assert!(
        r.report
            .starts_with("00:00.0 GeForce RTX 2060 Super role=compute rm=ready nodes="),
        "{}",
        r.report
    );
    let r = DrmScheme::compute_launch(&ready, COMPUTE_OP_SAXPY);
    assert_eq!((r.status, r.grid_threads), (5, 32));
    assert!(
        r.report.contains("[gpustep23]") && r.report.contains("FAILED"),
        "{}",
        r.report
    );
    assert!(!r.report.contains("LOAD-COMPUTE-STORE PROVEN"));
    let r = DrmScheme::compute_launch(&ready, COMPUTE_OP_BENCH);
    assert_eq!((r.status, r.elapsed_ns), (5, 0));
    assert!(
        r.report.contains("[gpubench]") && r.report.contains("FAILED"),
        "{}",
        r.report
    );
}
