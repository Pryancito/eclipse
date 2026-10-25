use super::*;

/// The console GPU's bring-up ladder goes through the RM gate, and it does NOT
/// wait for it.
///
/// The ladder's five RM entries (`attach_gpu`, `init_gsp`, `rm_api_demo`,
/// `state_init`, `step10`) are the one family that never took the gate for
/// itself, and it is the longest-running of all of them -- `kgspInitRm` keeps
/// the RM's API lock for seconds. A client's `step16` or `ctx_alloc` landing
/// in that stretch was not made to wait, it was REFUSED: `rmapiLockAcquire`
/// answers `NV_ERR_INVALID_LOCK_STATE` (0x2f) rather than block. The
/// compositor whose `ctx_alloc` is refused falls back to context 0, and on
/// context 0 it can never appear in `nvidia-smi`'s process table.
///
/// So the ladder takes the gate for its whole span. But it takes it with a
/// BUDGET, and that is the whole lesson of #2115: that change took the plain
/// unbounded gate inside `init_gsp`, putting an unbounded wait inside the
/// window the deferred bring-up opens by parking scanout -- so when somebody
/// else held the gate, the ladder reached the SEC2 `STARTCPU` store outside
/// the window that makes it safe, and a real box booted with no monitor at
/// all. Giving up is the safe answer: a console GPU that is not brought up is
/// exactly what this hardware does today without the hatch.
///
/// This is the half that can be tested without the hardware: with the gate
/// already held, `bringup_step14` must say so and return having touched
/// nothing -- no attach, no GSP boot, no PCI config write. The abort has to
/// come BEFORE stage 1 for that to be true, which is what makes this a test
/// about the ordering and not just about a string.
///
/// Note for whoever checks this by mutation: taking the guard away does kill
/// it, but it kills it with a SIGSEGV rather than an assertion. That is not
/// the test misbehaving -- it is the point. Stage 1 reads this card's PCI
/// configuration space, which does not exist in a host test process, so the
/// only reason this test can run at all is that the abort lands first.
#[test]
fn the_console_ladder_gives_up_instead_of_waiting_for_the_rm_gate() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    reset_fake_rm();
    // The boot framebuffer inside this card's BAR1 is what makes it the
    // console GPU; without it step14 self-guards away and proves nothing.
    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    assert!(gpu.drives_boot_display(), "this is the console GPU");

    // Somebody else is inside the RM. A budget of 1ms of the RM clock: the
    // real ladder asks for 5s, but the property under test is "gives up",
    // not "how long".
    let held = nvidia_rm_sys::rm_init::ladder_gate(1_000_000)
        .expect("the RM is idle at the start of this test");

    let report = gpu.bringup_step14();
    assert!(
        report.contains("ABORTED"),
        "the ladder ran while another caller was inside the RM: {}",
        report
    );
    assert!(
        !report.contains("stage 1"),
        "the abort has to land before the first stage: {}",
        report
    );
    assert!(
        gpu.rm_device_instance.lock().is_none(),
        "an aborted ladder must not have attached the RM"
    );
    assert!(
        FAKE_RM.lock().calls.is_empty(),
        "an aborted ladder must not have called into the RM at all: {:?}",
        FAKE_RM.lock().calls
    );

    drop(held);
    set_boot_fb_info(0, 0, 0, 0);
}
