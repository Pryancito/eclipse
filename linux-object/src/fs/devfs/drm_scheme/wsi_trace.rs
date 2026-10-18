//! The WSI refusal trace: which query was refused for which object, said
//! once per (nr, id) and once more when the table is full. Out of
//! `drm_scheme.rs` for its size ceiling.

use super::*;

/// Per-boot budget for the `[drm-wsi]` klog traces. klog writes SYNCHRONOUSLY
/// to the UART with no level filter; if any session process turns out to POLL
/// the KMS query ioctls (rather than probing once at startup), an uncapped
/// trace becomes a console storm -- and a klog storm has starved input on
/// this kernel before (see the EXEC-failure dedup note in nouveau_uapi.rs).
/// ~48 lines cover a full vulkaninfo VK_KHR_display probe sequence with room
/// to spare; after that the tracer goes silent for the rest of the boot and
/// says so once.
/// The six KMS query ioctls Mesa's `wsi_display` issues, by the name a
/// dmesg reader recognises. `None` for anything else.
pub(super) fn wsi_query_name(cmd: u32) -> Option<&'static str> {
    match cmd {
        DRM_IOCTL_MODE_GETRESOURCES => Some("GETRESOURCES"),
        DRM_IOCTL_MODE_GETCONNECTOR => Some("GETCONNECTOR"),
        DRM_IOCTL_MODE_GETENCODER => Some("GETENCODER"),
        DRM_IOCTL_MODE_GETCRTC => Some("GETCRTC"),
        DRM_IOCTL_MODE_GETPLANERESOURCES => Some("GETPLANERESOURCES"),
        DRM_IOCTL_MODE_GETPLANE => Some("GETPLANE"),
        _ => None,
    }
}

/// The DRM object a KMS query asks about, for the refusal trace: `0` for the
/// two that enumerate rather than name one (`GETRESOURCES`,
/// `GETPLANERESOURCES`), since those carry no id at all.
///
/// The id is a different field in each struct -- `drm_mode_get_connector`
/// puts `connector_id` after four pointers and three counts, `drm_mode_crtc`
/// puts `crtc_id` second -- so this reads each one by name rather than
/// assuming a common prefix, which would print a pointer as an id.
#[allow(unsafe_code)]
pub(super) fn wsi_query_object_id(cmd: u32, data: usize) -> u32 {
    unsafe {
        match cmd {
            DRM_IOCTL_MODE_GETCONNECTOR => (*(data as *const DrmModeGetConnector)).connector_id,
            DRM_IOCTL_MODE_GETENCODER => (*(data as *const DrmModeGetEncoder)).encoder_id,
            DRM_IOCTL_MODE_GETCRTC => (*(data as *const DrmModeGetCrtc)).crtc_id,
            DRM_IOCTL_MODE_GETPLANE => (*(data as *const DrmModeGetPlane)).plane_id,
            _ => 0,
        }
    }
}

/// Whether to klog this refusal of a KMS query. Separate from
/// [`wsi_trace_take`] on purpose: a refusal is the one line worth a boot, and
/// it must not be spent by the successes that precede it.
///
/// Deduped by `(nr, object id)` rather than merely counted, because libdrm
/// retries (`drmModeGetConnector`'s `goto retry`) and a client that polls the
/// KMS queries would otherwise repeat one refusal until the budget is gone.
/// Each distinct refusal prints once; the table is small and fixed, so a
/// client inventing ids cannot grow it -- past the last slot the channel
/// simply stops, after saying so.
pub(super) fn wsi_fail_take(nr: u32, id: u32) -> bool {
    use core::sync::atomic::{AtomicBool, AtomicU64};
    static SEEN: [AtomicU64; WSI_FAIL_SLOTS] = [const { AtomicU64::new(0) }; WSI_FAIL_SLOTS];
    static FULL_SAID: AtomicBool = AtomicBool::new(false);
    wsi_fail_take_in(&SEEN, &FULL_SAID, nr, id)
}

/// How many distinct refusals one boot reports.
pub(super) const WSI_FAIL_SLOTS: usize = 16;

/// [`wsi_fail_take`] over a caller-supplied table.
///
/// The seam is here, BELOW the global, rather than around the whole function:
/// the kernel and a test run the identical claim-a-slot code, and a test gets
/// its own table instead of competing for the one every other test in this
/// binary is also filling through the dispatch wrapper.
pub(super) fn wsi_fail_take_in(
    seen: &[core::sync::atomic::AtomicU64],
    full_said: &core::sync::atomic::AtomicBool,
    nr: u32,
    id: u32,
) -> bool {
    use core::sync::atomic::Ordering;
    // `(nr << 32) | id`, +1 so a zeroed slot reads as "empty" rather than as
    // "GETRESOURCES on object 0" -- which is exactly the entry GETRESOURCES
    // would claim, and the one refusal that needs no id to be worth printing.
    let key = (((nr as u64) << 32) | id as u64) + 1;
    for slot in seen.iter() {
        match slot.compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed) {
            // Claimed an empty slot: first sight of this refusal.
            Ok(_) => return true,
            // Occupied. By this same refusal? Then it has been reported.
            Err(prev) if prev == key => return false,
            Err(_) => continue,
        }
    }
    if !full_said.swap(true, Ordering::Relaxed) {
        kernel_hal::klog_info!(
            "[drm-wsi] {} distinct KMS-query refusals reported -- silencing the rest \
             for this boot",
            seen.len()
        );
    }
    false
}

pub(super) fn wsi_trace_take() -> bool {
    use core::sync::atomic::{AtomicU32, Ordering};
    static BUDGET: AtomicU32 = AtomicU32::new(0);
    const MAX: u32 = 48;
    let n = BUDGET.fetch_add(1, Ordering::Relaxed);
    if n == MAX {
        kernel_hal::klog_info!(
            "[drm-wsi] trace budget ({} lines) exhausted -- silencing for this boot \
             (something polls the KMS queries; capped to protect the console path)",
            MAX
        );
    }
    n < MAX
}
