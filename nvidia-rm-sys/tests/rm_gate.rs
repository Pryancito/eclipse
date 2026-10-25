//! Every way into the RM is gated, and the list of the ones that are not is
//! exact.
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
//! gated"**, and a new one that forgets is the same bug again. This reads the
//! source and holds that line. The handful that stay ungated are named below,
//! each because it runs per frame and must never wait behind an
//! eight-second bring-up stage; a 0x2f from one of those costs a frame, not a
//! session.

const SRC: &str = include_str!("../src/rm_init.rs");

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

/// Every `pub fn` of `rm_init.rs` that reaches an `eclipse_rm_*` symbol,
/// with whether it takes the gate. A function ends at the closing brace in
/// column 0, which is what keeps the `extern "C"` blocks and the test
/// doubles that follow it out of its body.
fn entry_points() -> Vec<(String, bool)> {
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
        if body.contains("eclipse_rm_") || body.contains("ce_rm::") {
            out.push((name, body.contains("RmGate::lock")));
        }
    }
    assert!(
        out.len() > 20,
        "only {} RM entry points found -- the parser lost the file",
        out.len()
    );
    out
}

#[test]
fn every_rm_entry_point_takes_the_gate_except_the_ones_named_here() {
    let ungated: Vec<String> = entry_points()
        .into_iter()
        .filter(|(_, gated)| !gated)
        .map(|(name, _)| name)
        .collect();
    let mut expected: Vec<String> = UNGATED.iter().map(|s| s.to_string()).collect();
    expected.sort();
    let mut got = ungated.clone();
    got.sort();
    assert_eq!(
        got, expected,
        "the set of ungated RM entry points changed. A new one means a caller \
         that can be inside the RM beside the console GPU's deferred bring-up, \
         and every such caller got NV_ERR_INVALID_LOCK_STATE on real hardware. \
         If it really must stay ungated, add it to UNGATED with the reason."
    );
}

#[test]
fn the_console_bring_up_chain_is_gated() {
    // Named one by one rather than taken from the list above: this is the
    // chain the deferred task drives while a desktop is already running, and
    // it is the reason the invariant had to widen. A regression here would
    // otherwise only show up as a session stuck on pixman.
    let chain = [
        "init_core",
        "attach_gpu",
        "init_gsp",
        "get_gsp_info",
        "state_init",
        "step10",
        "mark_console_gpu",
    ];
    let points = entry_points();
    for name in chain {
        let (_, gated) = points
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("`{}` is no longer an RM entry point", name));
        assert!(
            gated,
            "`{}` runs during the deferred console bring-up and must hold the \
             gate: ungated it shares the RM with the compositor's ioctls, and \
             both report the same thread id",
            name
        );
    }
}
