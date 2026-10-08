/// The bodies of the pre-waits that probe with `wait_ready`, from their
/// signature to the start of the next item at the same indentation.
/// `#[cfg(test)]` cannot delimit them: the first one in this file is on
/// line 10, so splitting there would leave nothing to look at and the
/// test would pass on anything.
fn body<'a>(src: &'a str, signature: &str) -> &'a str {
    let after = src
        .split_once(signature)
        .unwrap_or_else(|| panic!("{} is no longer in this file", signature))
        .1;
    let end = after.find("\n    /// ").unwrap_or(after.len());
    &after[..end]
}

#[test]
fn a_pre_wait_does_not_resolve_the_table_twice_per_look() {
    let src = include_str!("../drm_scheme.rs");
    for name in [
        "pub async fn syncobj_wait_sleep(",
        "pub async fn atomic_in_fence_sleep(",
    ] {
        let body = body(src, name);
        assert!(
            body.contains("wait_ready(") || body.contains("ready_fn("),
            "{} no longer probes with wait_ready: this test is measuring nothing",
            name
        );
        let hits = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("poll_pending("))
            .count();
        assert_eq!(
            hits, 0,
            "a poll_pending() is back in {}: wait_ready already resolves \
             the table, so this is a second walk of the pending list and a \
             second take of the lock the signalling side needs",
            name
        );
    }
}

/// A pre-wait that holds a LIST of fences reads each landing zone once
/// per look, not once per fence.
///
/// `fences.iter().all(hw_fence_landed)` reads the zone once per entry, and
/// these lists name one zone twice whenever two submits of one ring wrote
/// the buffer -- a channel has a single semaphore word. `hw_fences_landed`
/// gives the same answer off one read per address. The bench cannot reach
/// here (these need a live `DrmDev`, a process and a GPU) and what they
/// cost is a count, not an answer, so the shape is what is guarded; the
/// count itself is measured next to the function, in
/// `a_list_of_fences_reads_each_landing_zone_once_and_answers_like_all`.
#[test]
fn a_pre_wait_on_a_list_of_fences_reads_each_landing_zone_once() {
    let src = include_str!("../drm_scheme.rs");
    for name in [
        "pub async fn cpu_prep_sleep(",
        "pub async fn present_fence_sleep(",
    ] {
        let body = body(src, name);
        let code = || {
            body.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<alloc::string::String>()
        };
        assert!(
            code().contains("hw_fences_landed("),
            "{} no longer waits with hw_fences_landed",
            name
        );
        // Named exactly: `hw_fence_landed(` is the per-fence call, and
        // `hw_fences_landed(` -- with the s -- is not a superstring of
        // it, so this catches the reversion and nothing else. Looking for
        // a bare `all(` as well would fail on any unrelated `all` a later
        // refactor puts in these bodies.
        assert!(
            !code().contains("hw_fence_landed("),
            "{} is back to reading its landing zones one fence at a time",
            name
        );
    }
}

/// Every pre-wait is interruptible, and so is the backoff the fence ones
/// share.
///
/// These five are the only places in this kernel where a thread sleeps
/// with no fd behind it and no readiness waker to fire: nothing but the
/// fence it waits for, or its own deadline, ever ends them. They used to
/// ask nothing about signals, so a `^C` on a GL client did nothing until
/// the wait finished on its own -- three seconds for `WAIT_VBLANK`, the
/// client's own deadline for the syncobj waits, `GEM_CPU_PREP`'s own for
/// that one. On a desktop that is the first `^C` on `glxgears` landing
/// nowhere and the second one killing it.
///
/// Linux sleeps all of these interruptibly and answers `-ERESTARTSYS`
/// (`drm_syncobj_wait`, `drm_wait_vblank`), and libdrm's `drmIoctl()`
/// retries on `EINTR`, so the only caller that notices the change is the
/// one being killed -- which is the point: the signal is taken at the
/// syscall boundary.
///
/// Nothing can run these in a test (they need a live `DrmDev`, a process
/// and a GPU), so the guard is on the shape, like the two above.
#[test]
fn every_pre_wait_can_be_interrupted_by_a_signal() {
    let src = include_str!("../drm_scheme.rs");
    for name in [
        "pub async fn wait_vblank_sleep(",
        "pub async fn syncobj_wait_sleep(",
        "pub async fn atomic_in_fence_sleep(",
        "pub async fn cpu_prep_sleep(",
        "pub async fn present_fence_sleep(",
    ] {
        let body = body(src, name);
        assert!(
            body.lines()
                .next()
                .unwrap_or_default()
                .contains("-> LxResult<()>"),
            "{} has no way to tell its caller a signal arrived, so the \
             ioctl cannot answer EINTR",
            name
        );
        let code: alloc::string::String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        // A bare `sleep_until` is a sleep no signal can cut short, which
        // is exactly what these were.
        assert_eq!(
            code.matches("kernel_hal::thread::sleep_until(").count(),
            code.matches("interruptible(kernel_hal::thread::sleep_until(")
                .count(),
            "{} sleeps somewhere a signal cannot reach",
            name
        );
        assert!(
            code.contains("interruptible(kernel_hal::thread::sleep_until(")
                || code.contains("fence_poll_wait(acct.probes, deadline).await?"),
            "{} waits without ever asking whether a signal arrived",
            name
        );
    }
    // The backoff the four fence waits park in. Sliced by hand: `body`
    // stops at the next indented doc comment, and this one is a free
    // function whose neighbours are not indented.
    let fpw = src
        .split_once("async fn fence_poll_wait(")
        .expect("fence_poll_wait is no longer in this file")
        .1;
    let fpw = &fpw[..fpw.find("\n}\n").expect("an unterminated function")];
    assert!(
        fpw.contains("check_signals()?"),
        "fence_poll_wait no longer asks about signals, so a client in its \
         busy phase takes none for the whole frame"
    );
    assert!(
        fpw.contains("interruptible(kernel_hal::thread::sleep_until("),
        "fence_poll_wait is back to an uninterruptible sleep"
    );
}

/// A wait satisfied on its first look reports no parking, however long
/// its argument took to read; one that looked twice reports the lot.
#[test]
fn only_a_wait_that_looked_twice_reports_parked_time() {
    use super::pre_wait_parked_us;
    assert_eq!(
        pre_wait_parked_us(37, 0),
        0,
        "argument parsing was charged as parking"
    );
    assert_eq!(pre_wait_parked_us(37, 1), 37);
    assert_eq!(
        pre_wait_parked_us(0, 9),
        0,
        "a coarse clock is not a reason to drop the park"
    );
}

/// Every pre-wait accounts itself, so `/proc/gpudbg` can say how much of
/// a frame went into parking.
///
/// A pre-wait parks the thread BEFORE the driver is called, so none of it
/// lands in the driver's per-ioctl profile. One of these five left
/// unaccounted is time that simply does not appear anywhere, and the
/// symptom is a profile that adds up to far less than the frame -- which
/// is exactly the question the table was added to answer. Nothing can run
/// these functions in a test (they need a live `DrmDev`, a process and a
/// GPU), so the guard is on the shape.
#[test]
fn every_pre_wait_accounts_its_parking() {
    let src = include_str!("../drm_scheme.rs");
    for (name, kind) in [
        ("pub async fn wait_vblank_sleep(", "Kind::WaitVblank"),
        ("pub async fn syncobj_wait_sleep(", "Kind::SyncobjWait"),
        ("pub async fn atomic_in_fence_sleep(", "Kind::AtomicInFence"),
        ("pub async fn cpu_prep_sleep(", "Kind::CpuPrep"),
        ("pub async fn present_fence_sleep(", "Kind::PresentFence"),
    ] {
        let body = body(src, name);
        assert!(
            body.contains(kind),
            "{} does not open a PreWaitAccount for {}",
            name,
            kind
        );
        // An accountant that is never told about a probe reports every
        // park as a wait that was satisfied at once -- the counters would
        // be there and would say nothing.
        assert!(
            body.contains("acct.probe()"),
            "{} never counts a probe",
            name
        );
    }
    // And each kind is used by exactly one of them: two waits sharing a
    // kind would sum into one line and neither could be read.
    for kind in [
        "Kind::WaitVblank",
        "Kind::SyncobjWait",
        "Kind::AtomicInFence",
        "Kind::CpuPrep",
        "Kind::PresentFence",
    ] {
        assert_eq!(
            src.matches(&alloc::format!(
                "PreWaitAccount::new(zcore_drivers::scheme::prewait::{})",
                kind
            ))
            .count(),
            1,
            "{} is opened by more or fewer than one pre-wait",
            kind
        );
    }
}
