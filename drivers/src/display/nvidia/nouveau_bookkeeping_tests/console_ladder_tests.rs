use super::*;

/// A console GPU whose RM is already attached: the state the deferred boot
/// task finds it in once its bring-up has run, and the state an operator's
/// `/proc/gpustep14` leaves it in. The ladder is explicitly NOT built, which
/// is the situation this is about.
fn console_gpu_up(ladder_available: bool) -> NvidiaGpu {
    let gpu = gpu_rm();
    {
        let mut f = FAKE_RM.lock();
        f.ladder = ladder_available;
        // `gpu_rm` sets this so clients can get contexts; here the whole
        // point is that nobody has built the ladder yet.
        f.ladder_built = false;
        f.calls.clear();
    }
    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    assert!(gpu.drives_boot_display(), "this is the console GPU");
    gpu
}

/// Once the console GPU is up, its GR allocation ladder gets built there and
/// then -- not left to whichever client happens to rebuild it later.
///
/// `eclipse_rm_ctx_alloc` refuses with `NV_ERR_INVALID_STATE` while this
/// card's `g_grAllocDone` is false, so until the ladder exists every client on
/// this GPU falls back to context 0, and on context 0 a process cannot appear
/// in `nvidia-smi`'s process table at all. The secondary cards never had the
/// problem: `auto_bringup_compute` runs step16/step17 itself right after the
/// state-load. The console GPU is the one card that does not go through that
/// function -- it is skipped there on purpose, because its GSP wedges on the
/// SEC2 `STARTCPU` store with scanout live -- so its ladder was nobody's job.
/// A compositor that allocated its channel at startup, long before this
/// card's GSP came up, never asks again.
#[test]
fn a_console_gpu_that_came_up_late_gets_its_gr_ladder_built_right_there() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = console_gpu_up(true);

    let line = gpu.deferred_console_bringup();
    assert!(
        line.contains("ladder GR construido"),
        "the ladder was not built: {}",
        line
    );
    let calls = FAKE_RM.lock().calls.clone();
    assert!(
        calls.contains(&"step16") && calls.contains(&"step17"),
        "the ladder is step16 + step17, and both have to run: {:?}",
        calls
    );
    // Which is what `ctx_alloc` asks about, so a client can now have a
    // context of its own instead of falling back to context 0.
    assert!(nvidia_rm_sys::rm_init::gr_ready(0));

    set_boot_fb_info(0, 0, 0, 0);
}

/// And when the ladder cannot be built, the line says so rather than
/// reporting the card ready: that is the difference between "the desktop will
/// show up in nvidia-smi" and "it will not", and it is the one thing a boot
/// log can be asked for after the fact.
#[test]
fn a_console_gpu_whose_ladder_will_not_build_says_its_clients_fall_back() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = console_gpu_up(false);

    let line = gpu.deferred_console_bringup();
    assert!(
        line.contains("ladder GR NO construido"),
        "a ladder that did not build must not be reported as built: {}",
        line
    );
    assert!(
        line.contains("contexto 0"),
        "the line has to name the consequence, not just the failure: {}",
        line
    );

    set_boot_fb_info(0, 0, 0, 0);
}
