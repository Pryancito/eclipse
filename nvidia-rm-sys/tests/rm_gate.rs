//! Who waits for the RM gate, who takes it without waiting, and who does not
//! take it at all -- all three lists, exact.
//!
//! `RM_CALL_GATE` keeps the RM effectively single-threaded, which is the only
//! state any line of RM code in this port has ever been validated in. It is
//! not a performance choice: `os_get_current_thread` reports the **same
//! constant id for every caller** (deliberately -- see the comment at
//! `set_rm_thread_id_provider`'s caller in `zCore/src/main.rs`), so two
//! threads inside the RM at once make the second one look like the owner of
//! the first one's locks and the RM answers `NV_ERR_INVALID_LOCK_STATE`
//! (0x2f) to everything it is asked.
//!
//! That used to be unreachable, because the only RM work outside an ioctl ran
//! before userspace existed. The console GPU's deferred bring-up ended that:
//! it runs in the middle of a live session, for about eight seconds, and a
//! real boot spent that window answering 0x2f to every RM-backed ioctl --
//! labwc gave up on its renderer after its two tries and spent the whole
//! session on pixman.
//!
//! So the invariant is not "ioctls are gated", it is **"RM entry points are
//! gated"** -- but not all in the same way, and the difference is not a
//! detail. The bring-up's early stages run inside a window the deferred task
//! opens by parking the scanout on a clock; a wait in there, of any length,
//! pushes the ladder's SEC2 `STARTCPU` store out of the hole that makes it
//! safe, and that is how two earlier attempts at this fix lost the monitor.
//! Hence three groups, each asserted below:
//!
//!   * waits (`RmGate::lock`) -- everything arriving from an ioctl or a
//!     `/proc` read, plus the GR ladder, which is built with the scanout
//!     already running. The waiting is the fix.
//!   * never waits (`RmGate::try_lock`) -- the bring-up chain, and nothing
//!     else. Takes the gate when free (the normal case, and what makes the
//!     waiters wait at all); otherwise behaves exactly as it did before the
//!     gate existed.
//!   * ungated -- per-frame work that must never queue behind an
//!     eight-second bring-up stage. A 0x2f there costs a frame, not a
//!     session.

const SRC: &str = include_str!("../src/rm_init.rs");

/// How an RM entry point relates to the gate.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Gate {
    /// `RmGate::lock()`: spins until the RM is free.
    Waits,
    /// `RmGate::try_lock()`: takes it if free, carries on regardless.
    NeverWaits,
    /// Neither.
    Ungated,
}

/// The RM entry points that deliberately do NOT take the gate.
const UNGATED: &[&str] = &[
    // Per-frame reads of a C global. No allocation, no RM lock taken.
    "gr_ready",
    "hwflip_ready",
    "hwflip_pending",
    "hwflip_iso_builds",
    // The cursor follows the mouse: a cursor that waits for a bring-up stage
    // is a cursor that stutters for eight seconds.
    "hwcursor_move",
];

/// The entry points that run inside the window the deferred console bring-up
/// opens by pausing the scanout, and which therefore must never wait.
///
/// Named one by one rather than derived: this is the chain `bringup_step14`
/// drives while a desktop is already on screen, and both the reason it has to
/// hold the gate and the reason it must not block on it come from that one
/// fact. A regression either way shows up only on real hardware -- as a
/// session stuck on pixman, or as a dead monitor.
const BRINGUP: &[&str] = &[
    "init_core",        // rm_core_init_once
    "attach_gpu",       // bringup_step5
    "get_gsp_info",     // bringup_step5
    "mark_console_gpu", // bringup_step6
    "rm_api_demo",      // bringup_step8
    "state_init",       // bringup_step9
    "step10",           // bringup_step10
    "init_gsp",         // gsp_boot_run, from bringup_step14
    // bringup_step9, between state_init and the state load: the console's
    // BAR1 mapping and the probe that checks it.
    "preserve_bar1_console",
    "bar1_recover",
];

/// `SRC` with `//` comments blanked, line structure kept. This file's own
/// prose names every symbol it asserts about, and so does the gate's header
/// comment in `rm_init.rs`; neither may answer the question.
fn live_source() -> String {
    SRC.lines()
        .map(|l| match l.find("//") {
            // Not inside a string literal: no RM entry point has a `//` in one.
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `pub fn` of `rm_init.rs` that reaches an `eclipse_rm_*` symbol, with
/// how it relates to the gate. A function ends at the closing brace in column
/// 0, which is what keeps the `extern "C"` blocks and the test doubles that
/// follow it out of its body.
fn entry_points() -> Vec<(String, Gate)> {
    let src = live_source();
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(rest) = l.strip_prefix("pub fn ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let end = lines[i..]
            .iter()
            .position(|l| *l == "}")
            .map(|p| i + p)
            .unwrap_or_else(|| panic!("`{}` has no closing brace in column 0", name));
        let body = lines[i..=end].join("\n");
        if !(body.contains("eclipse_rm_") || body.contains("ce_rm::")) {
            continue;
        }
        // `RmGate::lock` is not a substring of `RmGate::try_lock`, so these do
        // not shadow each other.
        let how = if body.contains("RmGate::try_lock") {
            Gate::NeverWaits
        } else if body.contains("RmGate::lock") {
            Gate::Waits
        } else {
            Gate::Ungated
        };
        out.push((name, how));
    }
    assert!(
        out.len() > 20,
        "only {} RM entry points found -- the parser lost the file",
        out.len()
    );
    out
}

/// The names in one group, sorted.
fn group(how: Gate) -> Vec<String> {
    let mut v: Vec<String> = entry_points()
        .into_iter()
        .filter(|(_, g)| *g == how)
        .map(|(name, _)| name)
        .collect();
    v.sort();
    v
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

#[test]
fn every_rm_entry_point_takes_the_gate_except_the_ones_named_here() {
    assert_eq!(
        group(Gate::Ungated),
        sorted(UNGATED),
        "the set of ungated RM entry points changed. A new one means a caller \
         that can be inside the RM beside the console GPU's deferred bring-up, \
         and every such caller got NV_ERR_INVALID_LOCK_STATE on real hardware. \
         If it really must stay ungated, add it to UNGATED with the reason."
    );
}

#[test]
fn the_console_bring_up_chain_takes_the_gate_without_ever_waiting_for_it() {
    assert_eq!(
        group(Gate::NeverWaits),
        sorted(BRINGUP),
        "the set of entry points that decline to wait for the gate changed. \
         Only the chain that runs with the scanout parked may decline: adding \
         one elsewhere silently gives that caller back the 0x2f it was gated \
         to avoid, and dropping one from the chain puts a wait inside the \
         parked window, which cost the monitor twice (#2115, #2122)."
    );
}

#[test]
fn the_take_that_must_not_wait_really_does_not() {
    // The whole safety argument for the group above is that `try_lock`
    // returns rather than spins. A `while`, a `spin_loop` or a `pump` in
    // there and the bring-up is back inside the shape that lost the monitor,
    // with every test above still green.
    let src = live_source();
    let start = src
        .find("    pub(crate) fn try_lock(")
        .expect("`RmGate::try_lock` is gone, but the bring-up still calls it");
    let body = &src[start..];
    let end = body
        .find("\n    }")
        .expect("`try_lock` has no closing brace");
    let body = &body[..end];
    for waiting in ["while", "loop", "spin_loop", "pump", "yield"] {
        assert!(
            !body.contains(waiting),
            "`try_lock` contains `{}`: it is supposed to return immediately, \
             because its callers run with the scanout paused on a clock",
            waiting
        );
    }
}

#[test]
fn the_gr_ladder_waits_because_it_is_built_with_the_scanout_running() {
    // The counterpart to the test above, and the line between this fix and
    // the two that were reverted. The ladder is the expensive part of the
    // bring-up (step16 = GR channel + context, step17 = GPFIFO + schedule),
    // and #2129 moved it out of the paused window precisely so it CAN wait:
    // the compositor's ioctls must not interleave with it.
    let points = entry_points();
    for name in ["step16", "step17", "chan_notifier_pa"] {
        let (_, how) = points
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("`{}` is no longer an RM entry point", name));
        assert_eq!(
            *how,
            Gate::Waits,
            "`{}` is part of the GR ladder, which runs after the scanout is \
             unpaused: it has nothing to be afraid of and must hold the RM \
             alone while it builds the context",
            name
        );
    }
}
