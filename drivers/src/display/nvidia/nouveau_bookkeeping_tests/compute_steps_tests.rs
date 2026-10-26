//! The `/proc` readers of the compute rungs (`gpustep18`..`gpustep23`),
//! `/proc/gpubench` and the copy-engine test (`gpustep10`) on the fake RM,
//! which answers how far each got (`FakeRm::compute`): the refusal, every
//! stage line, the three verdict banners and the diagnosis each printer
//! gives for the wait that never landed or the result that came out wrong.
//! A child of `nouveau_bookkeeping_tests` for the harness; a file of its
//! own because the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::display::nvidia::rm_host_shims::Ran;

fn with(ran: Option<Ran>) -> NvidiaGpu {
    let gpu = gpu_rm();
    FAKE_RM.lock().compute = ran;
    gpu
}

fn set(ran: Ran) {
    FAKE_RM.lock().compute = Some(ran);
}

/// `s` carries every line of `lines`, in order, and `absent` none.
fn check(s: &str, lines: &[&str], absent: &[&str]) {
    let mut from = 0;
    for l in lines {
        let Some(at) = s[from..].find(l) else {
            panic!("missing {:?} (after byte {}) in\n{}", l, from, s);
        };
        from += at + l.len();
    }
    for l in absent {
        assert!(!s.contains(l), "unexpected {:?} in\n{}", l, s);
    }
}

/// A build with no RM C objects: each rung refuses, each printer says so
/// in its own words under the RM's narration, and the step-10 printer
/// does not cache a refusal (an early `Err` ran nothing in the RM).
#[test]
fn every_compute_rung_says_failed_when_the_rm_refuses_and_narrates_it() {
    let _g = LOCK.lock();
    let gpu = with(None);
    FAKE_RM.lock().narrates = true;
    assert_eq!(
        DrmScheme::bringup_step18(&gpu),
        "[gpustep18] | [fake-rm] step18\n\
         [gpustep18] eclipse_rm_step18 FAILED, NV_STATUS=0x56 (run gpustep17 first in this boot; GPU state-loaded?)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step19(&gpu),
        "[gpustep19] | [fake-rm] step19\n\
         [gpustep19] eclipse_rm_step19 FAILED, NV_STATUS=0x56 (run gpustep17 first in this boot; GPU state-loaded?)\n"
    );
    for (n, s) in [
        (20, DrmScheme::bringup_step20(&gpu)),
        (21, DrmScheme::bringup_step21(&gpu)),
        (22, DrmScheme::bringup_step22(&gpu)),
        (23, DrmScheme::bringup_step23(&gpu)),
    ] {
        assert_eq!(
            s,
            alloc::format!(
                "[gpustep{n}] | [fake-rm] step{n}\n\
                 [gpustep{n}] eclipse_rm_step{n} FAILED, NV_STATUS=0x56 (run gpustep17 first in this boot)\n"
            )
        );
    }
    assert_eq!(
        DrmScheme::bringup_bench(&gpu),
        "[gpubench] | [fake-rm] bench\n\
         [gpubench] eclipse_rm_bench FAILED, NV_STATUS=0x56 (run /proc/gpuinit first)\n"
    );
    assert_eq!(
        DrmScheme::bringup_step10(&gpu),
        "[gpustep10] --- CE-test narration (captured) ---\n\
         [gpustep10] | [fake-rm] step10\n\
         [gpustep10] --- end narration ---\n\
         [gpustep10] eclipse_rm_step10 FAILED, NV_STATUS=0x56 (state not loaded? run /proc/gpustep9)\n"
    );
    // Not cached: the next read runs the RM again.
    set(Ran::Verified);
    FAKE_RM.lock().narrates = false;
    check(
        &DrmScheme::bringup_step10(&gpu),
        &["COPY ENGINE DATA MOVEMENT VERIFIED"],
        &["FAILED"],
    );
    assert_eq!(
        FAKE_RM.lock().calls,
        ["step18", "step19", "step20", "step21", "step22", "step23", "bench", "step10", "step10"]
    );
}

/// Step 18 proves two things separately, the PBDMA fetch (host semaphore)
/// and the compute engine's context switch (engine semaphore), and names
/// which of the two is missing.
#[test]
fn step18_tells_the_pbdma_from_the_compute_engine() {
    let _g = LOCK.lock();
    let gpu = with(Some(Ran::Verified));
    assert_eq!(
        DrmScheme::bringup_step18(&gpu),
        "[gpustep18] --- first Eclipse-authored submission on the live channel ---\n\
         [gpustep18] lookup (chan/buf/USERD):  OK\n\
         [gpustep18] CPU map (buf + USERD):    OK\n\
         [gpustep18] work-submit token:        OK (token=0x00080000, runlist=0)\n\
         [gpustep18] submit (12 dw + doorbell): OK\n\
         [gpustep18] HOST semaphore (PBDMA):   OK (value=0x18180001, 3 ms)\n\
         [gpustep18] ENGINE semaphore (compute FE): OK (value=0x18180002, 7 ms)\n\
         [gpustep18] --- THE GPU RAN OUR PUSHBUFFER: PBDMA fetch + compute-engine context switch both proven; step19 = QMD + SASS kernel (real compute launch) ---\n"
    );
    set(Ran::Fenced);
    check(
        &DrmScheme::bringup_step18(&gpu),
        &[
            "HOST semaphore (PBDMA):   OK (value=0x18180001, 3 ms)",
            "ENGINE semaphore (compute FE): TIMEOUT (never landed) (value=0x00000000, 7 ms)",
            "PBDMA ran our methods but the compute engine never reported: suspect ctxsw/golden-context or SET_OBJECT path",
        ],
        &["THE GPU RAN OUR PUSHBUFFER"],
    );
    set(Ran::Submitted);
    check(
        &DrmScheme::bringup_step18(&gpu),
        &[
            "HOST semaphore (PBDMA):   TIMEOUT (never landed) (value=0x00000000, 3 ms)",
            "ENGINE semaphore (compute FE): not reached (value=0x00000000, 0 ms)",
        ],
        &["THE GPU RAN", "PBDMA ran our methods"],
    );
}

/// Steps 19 and 20: the post-PCAS fence says whether the PBDMA consumed
/// the compute stream, RELEASE0 whether the grid ran, and step 20's
/// readback whether the store landed where it was aimed.
#[test]
fn steps_19_and_20_say_which_wait_never_landed_and_whether_the_store_took() {
    let _g = LOCK.lock();
    let gpu = with(Some(Ran::Verified));
    assert_eq!(
        DrmScheme::bringup_step19(&gpu),
        "[gpustep19] --- first Eclipse-authored COMPUTE LAUNCH (QMD + SM75 kernel) ---\n\
         [gpustep19] lookup (chan/buf/USERD):  OK\n\
         [gpustep19] CPU map (buf + USERD):    OK\n\
         [gpustep19] work-submit token:        OK (token=0x00080000, runlist=0)\n\
         [gpustep19] QMD @ 0x91000, kernel @ 0x90000\n\
         [gpustep19] launch (40 dw + SEND_PCAS + doorbell): OK\n\
         [gpustep19] post-PCAS host fence:     OK (value=0x00000019, 2 ms)\n\
         [gpustep19] QMD RELEASE0 semaphore:   OK (value=0x00001919, 5 ms)\n\
         [gpustep19] ============================================================\n\
         [gpustep19]  THE 34 SMs RAN OUR SASS KERNEL. Eclipse launched compute\n\
         [gpustep19]  on the TU106 and the grid completed. step20 = kernel that\n\
         [gpustep19]  stores a computed result to memory (params + STG).\n\
         [gpustep19] ============================================================\n"
    );
    set(Ran::Fenced);
    check(
        &DrmScheme::bringup_step19(&gpu),
        &[
            "QMD RELEASE0 semaphore:   TIMEOUT (grid never released) (value=0x00000000, 5 ms)",
            "PBDMA consumed the whole compute stream (fence landed) but the grid never released",
        ],
        &["THE 34 SMs"],
    );
    set(Ran::Submitted);
    check(
        &DrmScheme::bringup_step19(&gpu),
        &[
            "post-PCAS host fence:     TIMEOUT (grid never released) (value=0x00000000, 2 ms)",
            "QMD RELEASE0 semaphore:   not reached (value=0x00000000, 0 ms)",
            "doorbell rung but the post-PCAS fence never landed",
        ],
        &["THE 34 SMs", "fence landed) but"],
    );

    set(Ran::Verified);
    assert_eq!(
        DrmScheme::bringup_step20(&gpu),
        "[gpustep20] --- kernel STORE: GPU writes a value we chose to memory we chose ---\n\
         [gpustep20] lookup / CPU map:         OK / OK\n\
         [gpustep20] token:                    OK (token=0x00080000, runlist=0)\n\
         [gpustep20] QMD @ 0x91000, kernel @ 0x90000, dest @ 0x92000\n\
         [gpustep20] launch (44 dw):            OK\n\
         [gpustep20] post-PCAS host fence:     OK (value=0x00000020, 2 ms)\n\
         [gpustep20] QMD RELEASE0 semaphore:   OK (value=0x00002020, 5 ms)\n\
         [gpustep20] stored dword @ dest:      OK (value=0xec0de520, expect 0xec0de520)\n\
         [gpustep20] ============================================================\n\
         [gpustep20]  THE GPU COMPUTED FOR ECLIPSE: our SASS ran on an SM and\n\
         [gpustep20]  stored our value at our address. MOV+STG+EXIT verified.\n\
         [gpustep20]  The compute bring-up ladder is COMPLETE.\n\
         [gpustep20] ============================================================\n"
    );
    set(Ran::Completed);
    check(
        &DrmScheme::bringup_step20(&gpu),
        &[
            "stored dword @ dest:      FAILED NV_STATUS=0x40 (value=0x00000000, expect 0xec0de520)",
            "grid completed but the store is missing/wrong: STG encoding or GMMU write path suspect",
        ],
        &["COMPLETE."],
    );
    set(Ran::Fenced);
    check(
        &DrmScheme::bringup_step20(&gpu),
        &[
            "QMD RELEASE0 semaphore:   TIMEOUT (value=0x00000000, 5 ms)",
            "stored dword @ dest:      not reached (value=0x00000000, expect 0xec0de520)",
            "methods consumed but grid never released: MOV/STG encoding suspect (SM trap); RELEASE0 did not land",
        ],
        &["COMPLETE.", "store is missing"],
    );
    set(Ran::Submitted);
    check(
        &DrmScheme::bringup_step20(&gpu),
        &["post-PCAS host fence:     TIMEOUT (value=0x00000000, 2 ms)"],
        &["COMPLETE.", "store is missing", "grid never released"],
    );
}

/// Steps 21, 22 and 23 count the slots that verified, name the first
/// mismatch with the value it should have held (`3*i+7`; `0x3064+4i` for
/// the SAXPY), and step 23 adds the MMU fault it queried when the grid
/// did not verify.
#[test]
fn the_thread_grids_count_their_slots_and_name_the_first_mismatch() {
    let _g = LOCK.lock();
    let gpu = with(Some(Ran::Verified));
    assert_eq!(
        DrmScheme::bringup_step21(&gpu),
        "[gpustep21] --- 32-THREAD kernel: out[tid] = tid*3 + 7 ---\n\
         [gpustep21] lookup / CPU map:         OK / OK\n\
         [gpustep21] token:                    OK (token=0x00080000, runlist=0)\n\
         [gpustep21] QMD @ 0x91000, kernel @ 0x90000, out[] @ 0x92000\n\
         [gpustep21] launch (48 dw):            OK\n\
         [gpustep21] post-PCAS host fence:     OK (2 ms)\n\
         [gpustep21] QMD RELEASE0 semaphore:   OK (5 ms)\n\
         [gpustep21] per-thread verification:  OK (32/32 slots correct)\n\
         [gpustep21] ============================================================\n\
         [gpustep21]  32 THREADS, 32 CORRECT RESULTS: per-thread IDs, integer\n\
         [gpustep21]  math, per-thread addressing and stores, and real Volta\n\
         [gpustep21]  scoreboarding all verified. Eclipse now runs real\n\
         [gpustep21]  parallel compute on the TU106.\n\
         [gpustep21] ============================================================\n"
    );
    set(Ran::Completed);
    check(
        &DrmScheme::bringup_step21(&gpu),
        &[
            "per-thread verification:  FAILED NV_STATUS=0x40 (31/32 slots correct)",
            "first mismatch: out[5]=0x0000dead (expected 0x16)",
            "grid completed but results are wrong: math/addressing path suspect",
        ],
        &["32 CORRECT RESULTS"],
    );
    set(Ran::Fenced);
    check(
        &DrmScheme::bringup_step21(&gpu),
        &[
            "QMD RELEASE0 semaphore:   TIMEOUT (5 ms)",
            "per-thread verification:  not reached (0/32 slots correct)",
            "methods consumed but grid never released: S2R/IMAD encoding or scoreboard suspect",
        ],
        &["first mismatch", "results are wrong"],
    );
    set(Ran::Submitted);
    check(
        &DrmScheme::bringup_step21(&gpu),
        &["post-PCAS host fence:     TIMEOUT (2 ms)"],
        &["first mismatch", "results are wrong", "grid never released"],
    );

    set(Ran::Verified);
    check(
        &DrmScheme::bringup_step22(&gpu),
        &[
            "[gpustep22] --- CHIP-SCALE grid: 68 CTAs x 32 threads over all 34 SMs ---",
            "per-thread verification:  OK (2176/2176 slots correct)",
            "2176 THREADS, 68 CTAs, ALL 34 SMs: the whole TU106 chip",
        ],
        &["first mismatch"],
    );
    set(Ran::Completed);
    check(
        &DrmScheme::bringup_step22(&gpu),
        &[
            "per-thread verification:  FAILED NV_STATUS=0x40 (2175/2176 slots correct)",
            "first mismatch: out[5]=0x0000dead (expected 0x16)",
            "grid completed but results wrong (check first mismatch: CTA scheduling/addressing suspect)",
        ],
        &["2176 THREADS, 68 CTAs"],
    );
    set(Ran::Fenced);
    check(
        &DrmScheme::bringup_step22(&gpu),
        &["methods consumed but grid never released: multi-CTA dispatch suspect"],
        &["first mismatch", "results wrong"],
    );

    set(Ran::Verified);
    check(
        &DrmScheme::bringup_step23(&gpu),
        &[
            "[gpustep23] --- integer SAXPY: y[i] = 3*x[i] + y[i], x[i]=0x1000+i, y[i]=100+i ---",
            "QMD @ 0x91000, kernel @ 0x90000, y[] @ 0x92000",
            "SAXPY verification:       OK (32/32 elements = 0x3064+4i)",
            "LOAD-COMPUTE-STORE PROVEN: the GPU read two arrays from",
            "Eclipse has the full GPU compute primitive.",
        ],
        &["MMU fault query", "first mismatch"],
    );
    set(Ran::Completed);
    check(
        &DrmScheme::bringup_step23(&gpu),
        &[
            "SAXPY verification:       FAILED NV_STATUS=0x40 (31/32 elements = 0x3064+4i)",
            "MMU fault query:          ctrl=0x0 addr=0x1_00002000 type=0x3",
            "first mismatch: y[5]=0xdead (57005) expected 0x3078",
            "grid completed but results wrong: LDG address/data path suspect",
        ],
        &["LOAD-COMPUTE-STORE PROVEN"],
    );
    set(Ran::Fenced);
    check(
        &DrmScheme::bringup_step23(&gpu),
        &[
            "MMU fault query:          ctrl=0x0 addr=0x1_00002000 type=0x3",
            "methods consumed but grid never released: LDG encoding or load scoreboard suspect",
        ],
        &["first mismatch", "results wrong"],
    );
}

/// `/proc/gpubench` divides the grid's operations by the PTIMER's elapsed
/// time here (u128, three decimals), and says when the grid never
/// signalled or signalled with both timestamps at zero.
#[test]
fn the_bench_reports_giops_from_the_ptimer_or_says_why_not() {
    let _g = LOCK.lock();
    let gpu = with(Some(Ran::Verified));
    assert_eq!(
        DrmScheme::bringup_bench(&gpu),
        "[gpubench] --- integer-ALU throughput (IMAD.U32 dependent chain) ---\n\
         [gpubench] lookup/map:   OK / OK\n\
         [gpubench] launch (24 dw): OK\n\
         [gpubench] grid:         2176 threads x 1000000 IMAD = 2176000000 ops\n\
         [gpubench] timestamp sem: OK (@9 ms)\n\
         [gpubench] elapsed:      435200000 ns (435.200 ms)\n\
         [gpubench] ============================================================\n\
         [gpubench]  5.000 GIOPS (integer multiply-add) on the RTX 2060 Super\n\
         [gpubench] ============================================================\n"
    );
    FAKE_RM.lock().bench_zero_time = true;
    check(
        &DrmScheme::bringup_bench(&gpu),
        &[
            "timestamp sem: OK (@9 ms)",
            "grid ran but timestamps were zero (t0=0x0 t1=0x0)",
        ],
        &["GIOPS", "elapsed:"],
    );
    set(Ran::Submitted);
    check(
        &DrmScheme::bringup_bench(&gpu),
        &[
            "timestamp sem: FAILED NV_STATUS=0x65 (@9 ms)",
            "grid did not signal within poll window",
        ],
        &["GIOPS", "timestamps were zero"],
    );
}

/// Step 10 prints each copy-engine phase with the byte the memsets really
/// wrote, names the first wrong dword with the value `ECLIPSE_FILL(i)`
/// expected, and caches a run that reached the RM for the rest of the
/// boot, whatever a later read would have answered.
#[test]
fn step10_prints_each_ce_phase_and_caches_a_run_for_the_boot() {
    let _g = LOCK.lock();
    let gpu = with(Some(Ran::Verified));
    let verified = "[gpustep10] buffers: 1024 KiB each, A PA=0x10000000 B PA=0x10100000 (VRAM)\n\
         [gpustep10] CeUtils channel:   OK\n\
         [gpustep10] alloc A:           OK\n\
         [gpustep10] alloc B:           OK\n\
         [gpustep10] CE memset B (byte 0xef) + spot-check: OK\n\
         [gpustep10] CE memset A (byte 0xa5) + spot-check: OK\n\
         [gpustep10] CPU unique-fill A + CE copy A->B: OK\n\
         [gpustep10] CPU verify B (per-dword unique): OK (262144 dwords checked, 0 mismatches)\n\
         [gpustep10] --- COPY ENGINE DATA MOVEMENT VERIFIED: pattern written, copied and read back through real hardware ---\n";
    assert_eq!(DrmScheme::bringup_step10(&gpu), verified);
    // Cached: the RM is not asked again, and a knob turned since changes
    // nothing.
    set(Ran::Submitted);
    assert_eq!(DrmScheme::bringup_step10(&gpu), verified);
    assert_eq!(FAKE_RM.lock().calls, ["step10"]);

    let gpu = with(Some(Ran::Completed));
    check(
        &DrmScheme::bringup_step10(&gpu),
        &[
            "CPU unique-fill A + CE copy A->B: OK",
            "CPU verify B (per-dword unique): FAILED NV_STATUS=0x40 (262144 dwords checked, 3 mismatches)",
            "first mismatch: dword 7 = 0x00000000 (expected 0xeb110ea0)",
        ],
        &["VERIFIED"],
    );
    let gpu = with(Some(Ran::Submitted));
    check(
        &DrmScheme::bringup_step10(&gpu),
        &[
            "CE memset A (byte 0xa5) + spot-check: OK",
            "CPU unique-fill A + CE copy A->B: FAILED NV_STATUS=0x65",
            "CPU verify B (per-dword unique): not reached (0 dwords checked, 0 mismatches)",
        ],
        &["VERIFIED", "first mismatch"],
    );
}
