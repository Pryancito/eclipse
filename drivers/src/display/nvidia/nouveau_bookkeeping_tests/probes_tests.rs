//! `probes.rs`: the channel self-test's three verdicts and its three ways
//! of not running, and the GR hang probe's recovery and FECS lines, which
//! only a BAR0 with a PBDMA routed and a context switch in flight reaches.
//! A child of `nouveau_bookkeeping_tests` for its harness (`LOCK`,
//! `gpu_rm`, the fake RM); the verdicts are dmesg lines, read back through
//! `host_hooks::take_dmesg`.

use super::super::rm_host_shims::{chan_buf_va, CHAN_BUF_SIZE};
use super::*;
use crate::utils::host_hooks::take_dmesg;

/// The self-test's fixed VA: the kernel-reserved half of NVK's layout.
const TEST_VA: u64 = 1 << 39;
/// Its semaphore page, and the payload it releases.
const SEM_VA: u64 = TEST_VA + 0x1000;
const PAYLOAD: u32 = 0x5e1f_7e57;
/// The self-test's `device_instance`, and the context it submits on.
const INST: u32 = 0;
const CTX: u32 = 0;

/// The dmesg lines since the last take, as text.
fn dmesg() -> Vec<alloc::string::String> {
    take_dmesg().into_iter().map(|(_, l)| l).collect()
}

/// The one line of `lines` holding `needle`.
fn line<'a>(lines: &'a [alloc::string::String], needle: &str) -> &'a str {
    let hits: Vec<&str> = lines
        .iter()
        .map(|l| l.as_str())
        .filter(|l| l.contains(needle))
        .collect();
    assert_eq!(hits.len(), 1, "{:?} in {:?}", needle, lines);
    hits[0]
}

fn selftest(gpu: &NvidiaGpu, buf_gpu_va: u64) -> Vec<alloc::string::String> {
    let _ = take_dmesg();
    gpu.nouveau_channel_selftest(INST, buf_gpu_va);
    dmesg()
}

/// Stage A passes when the ring runs the kernel's push: the fake decodes
/// the 6-dword host semaphore RELEASE the self-test authors -- the header
/// the C fence builder uses, the semaphore VA split as the host wants it,
/// the payload, `SEM_EXECUTE` = RELEASE -- fetched through the binding
/// the self-test made at the test VA, and the payload lands at the
/// semaphore page through the same binding. One submit of 24 bytes at
/// the test VA, no stage B, PASS named with the VA and the payload, and
/// the page unbound and freed after. Nothing had run the self-test with a
/// ring that executes, so the stream it writes was never checked against
/// a decoder.
#[test]
fn the_selftest_passes_stage_a_when_the_ring_runs_the_kernels_release() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    FAKE_RM.lock().runs = true;

    let lines = selftest(&gpu, 0x7_0000_0000);

    let f = FAKE_RM.lock();
    assert_eq!(
        f.calls,
        [
            "gem_alloc",
            "gem_map_cpu",
            "vm_bind_map",
            "exec_submit_signaled",
            "vm_bind_unmap",
            "gem_free",
        ]
    );
    assert_eq!(f.submits_of_ctx(CTX), alloc::vec![(TEST_VA, 24)]);
    assert_eq!(f.releases, alloc::vec![(CTX, SEM_VA, PAYLOAD)]);
    assert!(f.maps_of_ctx(CTX).is_empty(), "the test page stayed bound");
    assert!(f.gems.is_empty(), "the test page leaked");
    assert_eq!((f.unmaps, f.gem_frees, f.bad), (1, 1, 0));
    let pass = line(&lines, "SELFTEST stage A PASS");
    assert!(
        pass.contains("from VM_BIND VA 0x8000000000, fence landed, semaphore=0x5e1f7e57"),
        "{}",
        pass
    );
    assert!(!lines.iter().any(|l| l.contains("stage B")), "{:?}", lines);
}

/// When stage A fails the self-test pushes the same stream from the
/// channel's own RM-mapped buffer and names the layer: a fence that
/// landed with the semaphore still at zero is a failure of stage A (the
/// payload is checked, not just the wait), PTEs the GPU cannot see
/// (stage B passes), a channel that runs nothing (stage B fails too, with
/// its statuses), and submits the RM refuses outright (both `NV_STATUS`).
/// Each verdict is one dmesg line with the numbers a reader needs, the
/// page is unbound and freed whatever happened, and the fake's ring was
/// handed exactly the two pushes.
#[test]
fn the_selftest_names_the_layer_when_stage_a_fails() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    // The channel stage B pushes from: ctx 0's own buffer, where the fake's
    // `ctx_alloc` places it.
    nvidia_rm_sys::rm_init::ctx_alloc(INST, CTX).expect("ctx 0");
    let buf = chan_buf_va(CTX);
    let stage_b_va = buf + 0x9000;
    assert!(stage_b_va + 24 <= buf + CHAN_BUF_SIZE);
    FAKE_RM.lock().calls.clear();

    // The wait landed but the release's write never reached the page:
    // not a pass, and stage B runs (and passes: the ring does run).
    {
        let mut f = FAKE_RM.lock();
        f.runs = true;
        f.writes_lost = true;
    }
    let lines = selftest(&gpu, buf);
    let a = line(&lines, "SELFTEST stage A FAIL");
    assert!(
        a.contains("submit=0x0 fenceSubmit=0x0 fenceWait=0x0 semaphore=0x0 (expected 0x5e1f7e57)"),
        "{}",
        a
    );
    line(&lines, "SELFTEST stage B PASS");
    assert!(FAKE_RM.lock().releases.is_empty());

    // Our PTEs are not GPU-visible: the push from the test VA fetches
    // nothing, the one from the channel's buffer runs.
    {
        let mut f = FAKE_RM.lock();
        f.writes_lost = false;
        f.vm_bind_invisible = true;
    }
    let lines = selftest(&gpu, buf);
    {
        let f = FAKE_RM.lock();
        assert_eq!(
            f.submits_of_ctx(CTX),
            alloc::vec![
                (TEST_VA, 24),
                (stage_b_va, 24),
                (TEST_VA, 24),
                (stage_b_va, 24)
            ]
        );
        assert!(
            f.releases.is_empty(),
            "a push fetched through invisible PTEs ran"
        );
    }
    let a = line(&lines, "SELFTEST stage A FAIL");
    assert!(
        a.contains(
            "submit=0x0 fenceSubmit=0x0 fenceWait=0xffffffff semaphore=0x0 (expected 0x5e1f7e57)"
        ),
        "{}",
        a
    );
    let b = line(&lines, "SELFTEST stage B PASS");
    assert!(
        b.contains("from the channel's RM-mapped buffer (0x9000)")
            && b.contains("VM_BIND PTEs are NOT GPU-visible"),
        "{}",
        b
    );

    // The channel runs nothing: both fail, stage B with its statuses.
    FAKE_RM.lock().runs = false;
    let lines = selftest(&gpu, buf);
    line(&lines, "SELFTEST stage A FAIL");
    let b = line(&lines, "SELFTEST stage B FAIL too");
    assert!(
        b.contains(
            "submit=0x0 fenceSubmit=0x0 fenceWait=0xffffffff -- the channel executes NOTHING"
        ),
        "{}",
        b
    );

    // The RM refuses the calls themselves: both `NV_STATUS`.
    FAKE_RM.lock().refuse_submit = true;
    let lines = selftest(&gpu, buf);
    let a = line(&lines, "SELFTEST stage A FAIL");
    assert!(
        a.contains("submit=0x56 fenceSubmit=0xffffffff fenceWait=0xffffffff"),
        "{}",
        a
    );
    let b = line(&lines, "SELFTEST stage B FAIL too");
    assert!(
        b.contains("(NV_STATUS=0x56) -- channel-level problem"),
        "{}",
        b
    );

    let f = FAKE_RM.lock();
    assert_eq!(
        f.submits_of_ctx(CTX).len(),
        6,
        "three runs reached the ring"
    );
    assert_eq!(
        f.calls
            .iter()
            .filter(|c| **c == "exec_submit_signaled")
            .count(),
        8
    );
    assert!(f.maps_of_ctx(CTX).is_empty() && f.gems.is_empty());
    assert_eq!((f.unmaps, f.gem_frees, f.bad), (4, 4, 0));
}

/// The self-test cannot run without its page, a CPU mapping of it or a
/// binding, and says which: it stops at that step, submits nothing, and
/// frees what it got (the page once allocated; nothing to unbind until
/// bound). A step that failed and left the page allocated would leak a
/// GART page per boot.
#[test]
fn the_selftest_cannot_run_without_a_page_a_cpu_mapping_or_a_binding() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();

    FAKE_RM.lock().fail_gem_alloc = true;
    let lines = selftest(&gpu, 0);
    assert_eq!(FAKE_RM.lock().calls, ["gem_alloc"]);
    assert!(
        line(&lines, "SELFTEST: GART alloc failed").contains("(Ok(86)) -- cannot run"),
        "{:?}",
        lines
    );

    {
        let mut f = FAKE_RM.lock();
        f.fail_gem_alloc = false;
        f.fail_map_cpu = true;
        f.calls.clear();
    }
    let lines = selftest(&gpu, 0);
    assert_eq!(
        FAKE_RM.lock().calls,
        ["gem_alloc", "gem_map_cpu", "gem_free"]
    );
    line(&lines, "SELFTEST: gem_map_cpu failed -- cannot run");

    {
        let mut f = FAKE_RM.lock();
        f.fail_map_cpu = false;
        f.refuse_map = true;
        f.calls.clear();
    }
    let lines = selftest(&gpu, 0);
    assert_eq!(
        FAKE_RM.lock().calls,
        ["gem_alloc", "gem_map_cpu", "vm_bind_map", "gem_free"]
    );
    line(
        &lines,
        "SELFTEST: VM_BIND of the test page failed -- cannot run",
    );

    let f = FAKE_RM.lock();
    assert!(
        f.submits_of_ctx(CTX).is_empty(),
        "a step that failed still pushed"
    );
    assert!(f.gems.is_empty(), "a page leaked");
    assert_eq!((f.unmaps, f.gem_frees, f.bad), (0, 2, 0));
}

// ----- The GR hang probe with a PBDMA routed -----

fn bar0_write(gpu: &NvidiaGpu, off: usize, v: u32) {
    poke(gpu._bar0 + off, v)
}
fn bar0_read(gpu: &NvidiaGpu, off: usize) -> u32 {
    peek(gpu._bar0 + off)
}

/// `NV_PFIFO_PBDMA_MAP(2)`: PBDMA 2 serves runlist 0.
const PBDMA_MAP_2: usize = 0x2390 + 2 * 4;
/// PBDMA 2's registers.
const PB2: usize = 0x4_0000 + 2 * 0x2000;
const GP_PUT: usize = PB2;
const GP_GET: usize = PB2 + 0x14;
const INTR_0: usize = PB2 + 0x108;
const DOORBELL: usize = 0xbb_0090;
const FECS_STATUS: usize = 0x40_9c00;
const FECS_MAILBOX0: usize = 0x40_9800;
/// The probe's token for ctx 5: runlist 0 (bits [19:16]), channel 0x105
/// (bits [11:0]) -- a channel id wider than a byte, so the mask shows.
const TOKEN: u32 = 0x1105;
const CH: usize = 0x105;
const PCCSR_CH: usize = 0x80_0004 + CH * 8;

/// A probe with everything routed back to zero after.
fn probe(gpu: &NvidiaGpu, token: u32) -> (alloc::string::String, Vec<alloc::string::String>) {
    let _ = nv::take_gr_hang_probe();
    let _ = take_dmesg();
    gpu.gr_hang_probe(5, 4321, token, 0, 1000);
    let snap = nv::take_gr_hang_probe().expect("the probe recorded nothing");
    (snap, dmesg())
}

/// With `NV_PFIFO_PBDMA_MAP` routing the channel's runlist, the probe
/// reads THAT PBDMA's ring pointers and names it: GP_GET behind GP_PUT
/// with no interrupt is `PBDMA-NEVER-FETCHED`, and the recovery re-rings
/// the doorbell with the submit's own token, once, and says so; a pending
/// PBDMA interrupt is `PBDMA-BLOCKED` and no doorbell; a token of 0 has
/// no doorbell to ring. The channel's PCCSR word is decoded into the
/// snapshot. Every test of the probe ran on an all-zero BAR0, where no
/// PBDMA is routed and the recovery is dead code.
#[test]
fn the_probe_reads_the_routed_pbdma_and_rerings_a_lost_doorbell() {
    let _g = LOCK.lock();
    let gpu = gpu();
    bar0_write(&gpu, PBDMA_MAP_2, 1 << 0);
    bar0_write(&gpu, GP_PUT, 5);
    bar0_write(&gpu, GP_GET, 3);
    bar0_write(&gpu, PCCSR_CH, (3 << 24) | (1 << 28) | 1);
    nv::forget_prime(5);

    let (snap, lines) = probe(&gpu, TOKEN);
    assert_eq!(
        bar0_read(&gpu, DOORBELL),
        TOKEN,
        "the doorbell was not re-rung"
    );
    let first = line(&lines, "fence TIMEOUT after 1000ms");
    assert!(
        first.contains("ctx=5 fence TIMEOUT after 1000ms -- PBDMA-NEVER-FETCHED (pbdma=2 runlist=0 ch=261 GP_PUT=0x5 GP_GET=0x3 INTR_0=0x0)")
            && first.ends_with("-- full record in /proc/gpudbg\n"),
        "{}",
        first
    );
    line(&lines, "EXEC: PBDMA-NEVER-FETCHED: GP_GET is frozen");
    let rang = line(&lines, "PBDMA-stall recovery");
    assert!(
        rang.contains(
            "ctx=5 PBDMA-stall recovery: re-rang doorbell token=0x00001105 runlist=0 ch=261"
        ),
        "{}",
        rang
    );
    for want in [
        "ctx=5 pid=4321 fence TIMEOUT after 1000ms",
        "HINT: GP_GET is frozen with no PBDMA interrupt",
        "prime-state: NEVER RECORDED",
        "submit: work_token=0x00001105 runlist_id=0 ch_id=261",
        "PBDMA2(serves runl0, base=0x44000) GP_PUT=0x00000005 GP_GET=0x00000003 drained=false",
        "PBDMA2 STATUS=0x00000000 GET=0x00000000 INTR_0=0x00000000",
        "PCCSR ch261(0x80082c)=0x13000001 enable=1 busy=1 status=3",
    ] {
        assert!(snap.contains(want), "missing {:?} in:\n{}", want, snap);
    }
    assert!(
        !lines.iter().any(|l| l.contains("FECS/GPCCS")),
        "{:?}",
        lines
    );

    // A pending PBDMA interrupt: the host fetched the push and stalled in
    // it; nothing to re-ring.
    bar0_write(&gpu, DOORBELL, 0);
    bar0_write(&gpu, INTR_0, 0x100);
    let (snap, lines) = probe(&gpu, TOKEN);
    assert_eq!(
        bar0_read(&gpu, DOORBELL),
        0,
        "re-rang a doorbell the host had answered"
    );
    assert!(line(&lines, "fence TIMEOUT").contains(
        "-- PBDMA-BLOCKED (pbdma=2 runlist=0 ch=261 GP_PUT=0x5 GP_GET=0x3 INTR_0=0x100)"
    ));
    assert!(!lines.iter().any(|l| l.contains("recovery")), "{:?}", lines);
    assert!(snap.contains("INTR_0=0x00000100"));

    // No token, no doorbell: the frozen ring is named all the same.
    bar0_write(&gpu, INTR_0, 0);
    let (_, lines) = probe(&gpu, 0);
    assert_eq!(bar0_read(&gpu, DOORBELL), 0);
    line(&lines, "PBDMA-NEVER-FETCHED (pbdma=2 runlist=0 ch=0");
    assert!(!lines.iter().any(|l| l.contains("recovery")), "{:?}", lines);

    // And a runlist nothing routes: the pointers are not this channel's.
    bar0_write(&gpu, PBDMA_MAP_2, 0);
    let (snap, lines) = probe(&gpu, TOKEN);
    assert!(line(&lines, "fence TIMEOUT").contains("-- NO-PBDMA (pbdma=none runlist=0"));
    assert!(snap.contains("PBDMA?(none routes it)(serves runl0, base=0x40000)"));
    assert!(
        snap.contains("drained=false"),
        "an unknown ring read as drained"
    );
    assert_eq!(bar0_read(&gpu, DOORBELL), 0);
}

/// A drained ring with FECS mid context-switch is `FECS-CTXSW-HANG`, with
/// two dmesg lines of its own: the FECS and GPCCS states, the mailboxes
/// and the privilege error, then the sentence pointing at this context's
/// PRIME line in `/proc/gpudbg` -- which the snapshot carries when one was
/// recorded. The FECS line and the prime state are what a RESTORE hang is
/// diagnosed from, and they were ONE line: 300-odd bytes into a 256-byte
/// buffer, cut right before the pointer.
#[test]
fn the_probe_names_a_context_switch_hang_and_the_contexts_prime_state() {
    let _g = LOCK.lock();
    let gpu = gpu();
    bar0_write(&gpu, PBDMA_MAP_2, 1 << 0);
    bar0_write(&gpu, GP_PUT, 7);
    bar0_write(&gpu, GP_GET, 7);
    bar0_write(&gpu, FECS_STATUS, 2);
    bar0_write(&gpu, FECS_MAILBOX0, 0x1234);
    nv::record_prime(5, alloc::string::String::from("PRIME ctx=5 OK in 12ms"));

    let (snap, lines) = probe(&gpu, TOKEN);
    assert!(line(&lines, "fence TIMEOUT").contains(
        "-- FECS-CTXSW-HANG (pbdma=2 runlist=0 ch=261 GP_PUT=0x7 GP_GET=0x7 INTR_0=0x0)"
    ));
    let fecs = line(&lines, "FECS/GPCCS ctx-switch hang");
    assert!(
        fecs.ends_with("ctx=5 FECS/GPCCS ctx-switch hang (FECS_STATUS=0x00000002 GPCCS_STATUS=0x00000000 MAILBOX0=0x00001234 MAILBOX1=0x00000000 PRIV_ERR=0x00000000)\n"),
        "{}",
        fecs
    );
    let prime = line(&lines, "GR context-image for this client channel");
    assert!(
        prime.ends_with("(see /proc/gpudbg last PRIME line for ctx=5)\n"),
        "cut before the pointer: {}",
        prime
    );
    assert!(
        !lines.iter().any(|l| l.contains("recovery")),
        "a drained ring was re-rung"
    );
    for want in [
        "HINT: the GR context-image is incomplete",
        "prime-state: PRIME ctx=5 OK in 12ms",
        "GP_PUT=0x00000007 GP_GET=0x00000007 drained=true",
        "FECS CTXSW_STATUS(0x409c00)=0x00000002",
        "FECS MAILBOX0(0x409800)=0x00001234",
    ] {
        assert!(snap.contains(want), "missing {:?} in:\n{}", want, snap);
    }

    // GPCCS alone is a context switch too.
    bar0_write(&gpu, FECS_STATUS, 0);
    bar0_write(&gpu, 0x41_a000, 1);
    let (_, lines) = probe(&gpu, TOKEN);
    assert!(
        line(&lines, "ctx-switch hang").contains("FECS_STATUS=0x00000000 GPCCS_STATUS=0x00000001")
    );

    bar0_write(&gpu, 0x41_a000, 0);
    bar0_write(&gpu, PBDMA_MAP_2, 0);
    bar0_write(&gpu, GP_PUT, 0);
    bar0_write(&gpu, GP_GET, 0);
    bar0_write(&gpu, FECS_MAILBOX0, 0);
}
