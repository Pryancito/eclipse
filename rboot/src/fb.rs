//! Shared GOP pixel addressing for the splash / progress bar.
//!
//! `FB_ROT180` / `FB_MIRROR_X` on the kernel command line remap every store.
//! They are opt-in (odd panels); they are not implied by the hypervisor.

use core::sync::atomic::{AtomicBool, Ordering};

static ROT180: AtomicBool = AtomicBool::new(false);
static MIRROR_X: AtomicBool = AtomicBool::new(false);

pub fn set_rot180(enable: bool) {
    ROT180.store(enable, Ordering::SeqCst);
}

pub fn set_mirror_x(enable: bool) {
    MIRROR_X.store(enable, Ordering::SeqCst);
}

#[inline]
pub fn map_xy(x: usize, y: usize, sw: usize, sh: usize) -> (usize, usize) {
    let (mut x, mut y) = (x, y);
    if ROT180.load(Ordering::SeqCst) {
        x = sw.saturating_sub(1).saturating_sub(x);
        y = sh.saturating_sub(1).saturating_sub(y);
    }
    if MIRROR_X.load(Ordering::SeqCst) {
        x = sw.saturating_sub(1).saturating_sub(x);
    }
    (x, y)
}
