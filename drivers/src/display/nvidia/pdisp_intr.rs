//! What the display engine has pending, read where it is safe to read it.
//!
//! His wedge photo of 26-oct ends at the SEC2 `STARTCPU` store with
//! `LEAF[4] STILL pending after W1C = 0x40000000 (LIVE LEVEL source -- clear
//! at engine)` and `PMC_INTR0 = 0x40000000`. Bit 30 of `PMC_INTR0` is device
//! 30: `NV_PMC_INTR(i)` and `NV_PMC_ENABLE` are both 32-wide per-device bit
//! arrays in `turing/tu102/dev_boot.h` (`NV_PMC_INTR_EN_DEVICE(i)` /
//! `NV_PMC_ENABLE_DEVICE(i)`, `__SIZE_1 = 32`), and `NV_PMC_ENABLE_PDISP` is
//! `30:30` (that header, line 116). `os_boundary` already reads the two trees
//! as mirrors of each other -- it called the earlier `LEAF[4]` bit 28 "mirrored
//! in the legacy PMC_INTR0 bit28" and went after PBUS on that basis. So device
//! 30 is **PDISP**, and the display engine is holding a level interrupt nobody
//! services, because Eclipse is fully polled with INTx masked.
//!
//! That is an inference from the device indexing, not a named
//! `NV_PMC_INTR_0_PDISP` -- the published Turing header gives no per-device
//! names for the interrupt register. These lines are what turns it into a fact
//! or kills it: they ask the display engine itself.
//!
//! ## Why it reads here and nowhere near the store
//!
//! `os_boundary::sec2_intr_snapshot_and_drain` used to read this same cluster
//! (`NV_PDISP_FE_RM_INTR_STAT_CTRL_DISP`) just before the STARTCPU write, and
//! that was REMOVED with the reason written down: the only two boots that ever
//! survived STARTCPU ran builds without those reads, and the commit that added
//! them wedged every time. Touching the display PRI cluster microseconds
//! before the SEC2 HS resume correlates with the wedge.
//!
//! So this hangs off `dump_head_liveness`, which already reads
//! `NV_PDISP_FE_CORE_HEAD_STATE` and `NV_PDISP_RG_DPCA` at `preboot` and after
//! the state load -- phases that boots have survived, both far from the HS
//! window. Read-only, no writes: clearing or masking anything is a later
//! change, and it needs to know which bit first.
//!
//! Offsets from `disp/v03_00/dev_disp.h` (Turing):
//!   `NV_PDISP_FE_RM_INTR_DISPATCH`            0x00611EC0, bit (0+i) per head
//!   `NV_PDISP_FE_RM_INTR_STAT_HEAD_TIMING(i)` 0x00611C00 + i*4
//!   `NV_PDISP_FE_RM_INTR_STAT_CTRL_DISP`      0x00611C30, AWAKEN 8:8
//!   `NV_PDISP_FE_EVT_STAT_HEAD_TIMING(i)`     0x00611800 + i*4 (the writable
//!                                             one -- the "clear at engine")

use alloc::string::String;
use core::fmt::Write;

const DISPATCH: usize = 0x0061_1EC0;
const RM_INTR_STAT_HEAD: usize = 0x0061_1C00;
const RM_INTR_STAT_CTRL_DISP: usize = 0x0061_1C30;
const EVT_STAT_HEAD: usize = 0x0061_1800;

/// The heads `dump_head_liveness` already walks.
const HEADS: usize = 4;

/// Name the bits the Turing header gives names to. An unnamed bit still gets
/// reported as part of the raw word -- the point is to find out which one is
/// set, so hiding the ones we have no name for would defeat it.
fn head_bits(stat: u32) -> &'static str {
    match (
        stat & (1 << 1) != 0,
        stat & (1 << 5) != 0,
        stat & (1 << 6) != 0,
    ) {
        (false, false, false) => "nothing named",
        (true, false, false) => "LAST_DATA",
        (false, true, false) => "RG_LINE_A",
        (false, false, true) => "RG_LINE_B",
        (true, true, false) => "LAST_DATA+RG_LINE_A",
        (true, false, true) => "LAST_DATA+RG_LINE_B",
        (false, true, true) => "RG_LINE_A+RG_LINE_B",
        (true, true, true) => "LAST_DATA+RG_LINE_A+RG_LINE_B",
    }
}

/// Render the report from register values. Pure, so the decode is testable
/// without a GPU -- which is the only way it gets tested at all here.
fn report(tag: &str, phase: &str, dispatch: u32, ctrl_disp: u32, heads: &[(u32, u32)]) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "[{}] {} PDISP intr: DISPATCH={:#010x} CTRL_DISP={:#010x} (AWAKEN {})",
        tag,
        phase,
        dispatch,
        ctrl_disp,
        if ctrl_disp & (1 << 8) != 0 {
            "PENDING"
        } else {
            "idle"
        }
    );
    for (i, (rm_stat, evt_stat)) in heads.iter().enumerate() {
        // A head with nothing pending and no dispatch bit is noise in a log
        // that already runs to hundreds of lines; say so once, below.
        if *rm_stat == 0 && *evt_stat == 0 && dispatch & (1 << i) == 0 {
            continue;
        }
        let _ = writeln!(
            s,
            "[{}] {} PDISP intr head{}: DISPATCH={} RM_INTR_STAT={:#010x} ({}) EVT_STAT={:#010x}",
            tag,
            phase,
            i,
            if dispatch & (1 << i) != 0 {
                "PENDING"
            } else {
                "-"
            },
            rm_stat,
            head_bits(*rm_stat),
            evt_stat
        );
    }
    if dispatch == 0 && ctrl_disp == 0 && heads.iter().all(|(a, b)| *a == 0 && *b == 0) {
        let _ = writeln!(
            s,
            "[{}] {} PDISP intr: nothing pending at the display engine -- \
             whatever holds PMC_INTR0 bit 30 is not this",
            tag, phase
        );
    }
    s
}

/// Read the display engine's own interrupt state and report it. Caller has
/// already established that PDISP is enabled in `NV_PMC_ENABLE`; reading this
/// cluster with PDISP in reset is what produces `0xBADF####`.
pub(super) fn dump(bar0: usize, tag: &str, phase: &str) -> String {
    let rd =
        |off: usize| -> u32 { unsafe { core::ptr::read_volatile((bar0 + off) as *const u32) } };
    let dispatch = rd(DISPATCH);
    let ctrl_disp = rd(RM_INTR_STAT_CTRL_DISP);
    let mut heads = [(0u32, 0u32); HEADS];
    for (i, slot) in heads.iter_mut().enumerate() {
        *slot = (rd(RM_INTR_STAT_HEAD + i * 4), rd(EVT_STAT_HEAD + i * 4));
    }
    let s = report(tag, phase, dispatch, ctrl_disp, &heads);
    for line in s.lines() {
        log::error!("{}", line);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_raising_rg_line_a_is_named() {
        let out = report(
            "t",
            "preboot",
            1 << 0,
            0,
            &[(1 << 5, 1 << 5), (0, 0), (0, 0), (0, 0)],
        );
        assert!(
            out.contains("head0: DISPATCH=PENDING"),
            "the dispatch bit for head 0 must be decoded: {}",
            out
        );
        assert!(
            out.contains("RG_LINE_A"),
            "a named bit must be named, or the report cannot answer which one it is: {}",
            out
        );
    }

    /// The whole point is to distinguish "the display engine is holding it"
    /// from "it is not the display engine". A silent report answers neither.
    #[test]
    fn nothing_pending_says_so_out_loud() {
        let out = report("t", "preboot", 0, 0, &[(0, 0); 4]);
        assert!(
            out.contains("not this"),
            "an all-zero read must state that the display engine is NOT the source, \
             because that result is as useful as the other one: {}",
            out
        );
    }

    /// A bit the Turing header gives no name to must still reach the log: the
    /// pending bit we are hunting may well be one of them, and a decode that
    /// only prints what it recognises would hide exactly that case.
    #[test]
    fn an_unnamed_bit_still_reaches_the_report() {
        let out = report(
            "t",
            "preboot",
            0,
            0,
            &[(1 << 17, 0), (0, 0), (0, 0), (0, 0)],
        );
        assert!(
            out.contains("0x00020000"),
            "the raw word must be printed whether or not we have a name for the bit: {}",
            out
        );
        assert!(
            !out.contains("not this"),
            "something WAS pending, so the all-clear line must not fire: {}",
            out
        );
    }

    #[test]
    fn the_awaken_bit_of_ctrl_disp_is_decoded() {
        let out = report("t", "preboot", 0, 1 << 8, &[(0, 0); 4]);
        assert!(out.contains("AWAKEN PENDING"), "{}", out);
    }
}
