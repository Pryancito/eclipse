extern crate std;

/// `DRM_STATE`, the GEM table, `gem_mmap` and the CRTC's current fb are
/// process-wide, and cargo runs a crate's tests in threads. Every test
/// module that touches them takes this first, so one test's framebuffers
/// are not another's.
///
/// Without it the suite failed about one run in two with a different test
/// each time -- `crtc_fb` still holding a neighbour's framebuffer, or an
/// `ADDFB2` refused because a neighbour had filled the table. That reads
/// as a real regression and is not one, which is the worst kind of noise
/// to leave in a suite people are meant to trust.
static LOCK: self::std::sync::Mutex<()> = self::std::sync::Mutex::new(());

pub(crate) fn lock() -> self::std::sync::MutexGuard<'static, ()> {
    // A test that panics while holding this poisons it. The poison is not
    // a failure for the tests that follow, so step over it -- the panicking
    // test has already been reported.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
