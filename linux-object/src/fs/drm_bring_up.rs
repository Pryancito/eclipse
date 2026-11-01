//! The process-wide half of the DRM bring-up, and why a test build skips it.

#[cfg(not(test))]
use {super::devfs, kernel_hal::drivers};

/// The process-wide half of the DRM bring-up: register every card
/// `kernel_hal` probed, then build the `/dev/dri` node table from them.
///
/// Called from [`create_root_fs`], which the kernel runs **once** -- for
/// init's root; every later process gets `LinuxProcess::with_root`, which is
/// what that constructor exists for.
///
/// The unit-test binary is the other story: 236 call sites make a process with
/// `LinuxProcess::new`, which does call `create_root_fs`, and the harness runs
/// them in parallel with the DRM tests. There is no real GPU in a host test
/// build, so `drivers::all_drm()` is empty unless a DRM test running in
/// another thread has just attached an emulated card -- and then this code
/// takes a card that belongs to that test's `kms_emu::Screen` (which is
/// holding `drm::test_globals::lock()` precisely so nobody else touches these
/// globals) and hands it to `register_driver`, and `build_gpu_nodes` renumbers
/// the node table, or empties it, under the running test's feet.
///
/// That is what the suite's one-run-in-ten red was, with a different victim
/// each time: `prime_import_card_tests` reading `Some(1)` where its own two
/// cards said `Some(3)`, `fs::sysfs::tree_tests` listing a
/// `/dev/char/226:128` it could then not look up, `out_fence_tests` asking
/// the synthetic CRTC of a software-KMS path that a stray hardware card had
/// just switched off. #2324 stopped this from DUPLICATING entries, which was a
/// real bug of its own, but a first registration from another thread is not a
/// duplicate and the renumbering stayed.
///
/// So under `cfg(test)` it does nothing at all, and nothing is lost by that:
/// with no card to find it was already a no-op plus a table wipe, the DRM
/// tests build the table themselves with `build_gpu_nodes()` under the lock,
/// and what this function does with cards present is covered by
/// `driver_registry::dedupe_tests`, which walks `all_drm()` exactly as the
/// loop above does.
#[cfg(not(test))]
pub(super) fn bring_up_drm_globals() {
    for drm in drivers::all_drm().as_vec().iter() {
        devfs::drm::register_driver(drm.clone());
    }
    devfs::drm::build_gpu_nodes();
}

/// See [`bring_up_drm_globals`]: in the test binary this is boot-time work
/// run hundreds of times, concurrently, over globals another thread's test
/// owns.
#[cfg(test)]
pub(super) fn bring_up_drm_globals() {}

#[cfg(test)]
mod tests;
