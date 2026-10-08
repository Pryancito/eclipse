//! Which commands take the async sleep path, and the mmap cookie.
//!
//! `sys_ioctl` asks `is_syncobj_wait_ioctl` whether to park the caller
//! before running the sync arm. When it says no, the sync arm spin-polls
//! the whole timeout and pegs a core -- the starvation `WAIT_VBLANK` used
//! to cause. So this router has to recognise every wait the dispatcher
//! will accept, and the dispatcher resolves ioctls by NUMBER.
//!
//! It used to match four exact 32-bit commands instead, which carry the
//! struct size. Both wait structs already grew once (the 2023
//! `deadline_nsec`), and the next libdrm to append a field would have sent
//! a command the dispatcher handles and this router does not: the wait
//! would have worked, at the cost of a core spinning for its whole
//! timeout, with nothing in any log to say why.
//!
//! What is *not* covered, so nobody reads more into these than is there:
//! the two places that ask `is_syncobj_timeline_wait` which struct to read
//! -- the async sleeper and the dispatch arm -- both need a live device
//! and `nouveau_uapi_enabled()`, so inverting either one's answer leaves
//! this module green. What the tests pin is the rule itself, and that both
//! callers now ask the same one instead of spelling it out twice.

use super::*;

fn wait_cmd(nr: u32, size: usize) -> u32 {
    drm_iowr_core(nr, size)
}

const CLASSIC: usize = core::mem::size_of::<DrmSyncobjWait>();
const TIMELINE: usize = core::mem::size_of::<DrmSyncobjTimelineWait>();

/// The two sizes in the wild today, named so a change to either is loud.
#[test]
fn the_two_sizes_libdrm_sends_today_are_both_waits() {
    for cmd in [
        DRM_IOCTL_SYNCOBJ_WAIT,
        DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE,
        DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
        DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE,
    ] {
        assert!(
            is_syncobj_wait_ioctl(cmd),
            "{:#x} is a wait and must take the async path",
            cmd,
        );
    }
    assert_eq!(CLASSIC, 32, "drm_syncobj_wait grew");
    assert_eq!(TIMELINE, 40, "drm_syncobj_timeline_wait grew");
}

/// The one that was broken. A struct that grows again keeps working
/// through the dispatcher, which matches on the number, so the router has
/// to follow it there.
#[test]
fn a_struct_that_grows_again_is_still_a_wait() {
    for extra in [8, 16, 24, 64, 1000] {
        let classic = wait_cmd(NR_SYNCOBJ_WAIT, CLASSIC + extra);
        assert!(
            is_syncobj_wait_ioctl(classic),
            "a {}-byte drm_syncobj_wait stopped being a wait",
            CLASSIC + extra,
        );
        assert!(!is_syncobj_timeline_wait(classic), "and it is not timeline");

        let timeline = wait_cmd(NR_SYNCOBJ_TIMELINE_WAIT, TIMELINE + extra);
        assert!(
            is_syncobj_wait_ioctl(timeline),
            "a {}-byte drm_syncobj_timeline_wait stopped being a wait",
            TIMELINE + extra,
        );
        assert!(is_syncobj_timeline_wait(timeline));
    }
}

/// The floor is the async path's own requirement, not the dispatcher's.
/// The sleeper reads the struct **in place** in user memory, so it can
/// only run once the client has actually sent a whole one; a short request
/// still reaches the sync arm, which copies it into a zero-filled kernel
/// buffer and is safe with it. Saying so here because the asymmetry looks
/// like an oversight otherwise.
#[test]
fn a_request_too_short_to_read_in_place_is_left_to_the_sync_arm() {
    for size in [0, 1, CLASSIC - 1] {
        assert!(!is_syncobj_wait_ioctl(wait_cmd(NR_SYNCOBJ_WAIT, size)));
    }
    assert!(is_syncobj_wait_ioctl(wait_cmd(NR_SYNCOBJ_WAIT, CLASSIC)));

    for size in [0, CLASSIC, TIMELINE - 1] {
        assert!(!is_syncobj_timeline_wait(wait_cmd(
            NR_SYNCOBJ_TIMELINE_WAIT,
            size
        )));
    }
    assert!(is_syncobj_timeline_wait(wait_cmd(
        NR_SYNCOBJ_TIMELINE_WAIT,
        TIMELINE
    )));
}

/// The NUMBER decides which struct the sleeper reads, and it must decide
/// it the same way the dispatch arm does. Reading a timeline request as a
/// classic one takes `count_handles` and `flags` from the wrong offsets.
#[test]
fn the_number_decides_the_struct_not_the_size() {
    // A timeline-sized classic wait is still classic.
    let odd = wait_cmd(NR_SYNCOBJ_WAIT, TIMELINE);
    assert!(is_syncobj_wait_ioctl(odd));
    assert!(!is_syncobj_timeline_wait(odd));
    // And the canonical commands the dispatch arm sees agree.
    assert!(!is_syncobj_timeline_wait(DRM_IOCTL_SYNCOBJ_WAIT));
    assert!(is_syncobj_timeline_wait(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT));
    assert!(!is_syncobj_timeline_wait(DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE));
    assert!(is_syncobj_timeline_wait(
        DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE
    ));
}

/// Everything else stays off the sleep path. The neighbouring syncobj
/// numbers are the ones that would hurt: RESET and SIGNAL carry a
/// different struct entirely, and parking on one would read it wrong.
#[test]
fn nothing_but_the_two_waits_takes_the_sleep_path() {
    for cmd in [
        DRM_IOCTL_SYNCOBJ_CREATE,
        DRM_IOCTL_SYNCOBJ_DESTROY,
        DRM_IOCTL_SYNCOBJ_RESET,
        DRM_IOCTL_SYNCOBJ_SIGNAL,
        DRM_IOCTL_SYNCOBJ_QUERY,
        DRM_IOCTL_SYNCOBJ_TRANSFER,
        DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
    ] {
        assert!(!is_syncobj_wait_ioctl(cmd), "{:#x} is not a wait", cmd);
    }
    // And the type byte still has to be DRM's, whatever the number says.
    let not_drm = (3u32 << 30) | (0x65 << 8) | NR_SYNCOBJ_WAIT | ((CLASSIC as u32) << 16);
    assert!(!is_syncobj_wait_ioctl(not_drm));
}

/// `MAP_DUMB` hands userspace `handle << 12` as a fake file offset and
/// `get_vmo` shifts it back. The two live far apart in this file and are
/// the only thing standing between a client's `mmap()` and the right
/// buffer, so pin the round trip.
#[test]
fn the_mmap_cookie_round_trips_for_every_handle() {
    for handle in [1u32, 2, 0xFF, 0x1234, 0x000F_FFFF, 0x8000_0000, u32::MAX] {
        let offset = mmap_cookie_for(handle);
        assert_eq!(
            offset & 0xFFF,
            0,
            "musl rejects a non-page-aligned mmap offset before the syscall",
        );
        assert_eq!(
            handle_from_mmap_cookie(offset as usize),
            handle,
            "handle {} does not survive the cookie",
            handle,
        );
    }
}

/// And the limit of that encoding, written down rather than discovered.
/// The decode truncates to 32 bits, so offsets above `u32::MAX << 12`
/// alias onto a handle. It is not a way in -- both lookups behind it check
/// the caller owns the handle -- but it is a surprise worth naming.
#[test]
fn an_offset_above_the_handle_space_aliases_rather_than_failing() {
    let aliased = ((1u64 << 32) | 5) << 12;
    assert_eq!(handle_from_mmap_cookie(aliased as usize), 5);
}
