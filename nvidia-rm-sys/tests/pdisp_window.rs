//! Leaving the PDISP window has to give the display back.
//!
//! `pdisp_kill_arm` takes the console GPU's display engine away for the SEC2
//! HS-resume window of its GSP boot, and `pdisp_restore` gives it back. The
//! restore is guarded by `PDISP_RESTORE_PENDING`, and it early-returns when
//! that flag is clear.
//!
//! `pdisp_kill_disarm` -- the call every exit path makes -- used to clear that
//! flag and nothing else. So a boot that armed the kill, triggered it, and then
//! did not reach its own restore anchor had its pending restore thrown away:
//! PDISP stayed in engine reset until the next reboot. On hardware that is a
//! monitor that goes dark when the card is handed over and never comes back,
//! with no console left to read a log on. Both callers (`BootWindow::restore`
//! and `/proc/gpustep12`) had the order wrong, which is why the restore now
//! lives inside the disarm rather than next to it.
//!
//! This reads the source: the statics are private, the crate links the real RM,
//! and the write itself only happens against a mapped BAR0.

fn source() -> &'static str {
    include_str!("../src/os_boundary.rs")
}

/// The body of `pub fn <name>()`, comments stripped -- a rule that only exists
/// in a comment is not a rule.
fn body(name: &str) -> String {
    let src = source();
    let head = format!("pub fn {}(", name);
    let start = src
        .find(&head)
        .unwrap_or_else(|| panic!("no definition of `{}` in os_boundary.rs", name));
    let open = start
        + src[start..]
            .find('{')
            .expect("a function definition has a body");
    let mut depth = 0i32;
    let mut end = open;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    src[open..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<&str>>()
        .join("\n")
}

#[test]
fn leaving_the_window_restores_the_display_before_cancelling_the_arm() {
    let disarm = body("pdisp_kill_disarm");
    let restore = disarm.find("pdisp_restore()").expect(
        "leaving the PDISP window must RESTORE the display engine, not just cancel the \
         intention to: `pdisp_restore` early-returns on the very flag this function clears, \
         so clearing it alone leaves PDISP in reset until reboot",
    );
    let clear = disarm
        .find("PDISP_RESTORE_PENDING")
        .expect("the disarm must still clear the pending flag");
    assert!(
        restore < clear,
        "the restore has to run BEFORE the pending flag is cleared; after it, it is a no-op \
         and the screen never comes back:\n{}",
        disarm
    );
}

/// The restore is a no-op unless the kill actually fired, so every exit path
/// can call it unconditionally. That is what makes putting it inside the
/// disarm safe, and what stops a caller from needing to know.
#[test]
fn the_restore_is_a_no_op_when_the_kill_never_fired() {
    let restore = body("pdisp_restore");
    assert!(
        restore.contains("PDISP_RESTORE_PENDING.swap(false"),
        "`pdisp_restore` must consume the pending flag, so it is idempotent and safe on a \
         boot that never took the display away:\n{}",
        restore
    );
}
