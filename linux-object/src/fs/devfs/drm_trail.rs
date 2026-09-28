//! The last DRM ioctls a process issued, kept so a crash report can name them.
//!
//! A userspace GPU stack crashes in a stripped shared object: the report says
//! `/usr/lib/libvulkan_nouveau.so+0xe9c48` and nothing more, because a Vulkan
//! ICD exports a handful of symbols and carries no `.symtab` to resolve the
//! rest against. The interesting half of the question is on this side of the
//! syscall boundary anyway -- *what did the driver last answer, and with
//! what?* -- and that half the kernel can print for free.
//!
//! So every DRM ioctl lands one fixed-size entry in a per-process ring, and
//! [`dump_for`] renders the ring when that process dies on an unhandled page
//! fault. There is no allocation and no growth: `SLOTS` processes, `DEPTH`
//! entries each, reused round-robin by last use, which is exactly enough for a
//! compositor plus the client that was talking to it.
//!
//! What an entry can and cannot say: the ioctl NUMBER is the stable identity
//! of the call and `arg0` is the first 64-bit field of its argument struct --
//! the `param` of a `GETPARAM`, the `handle` of a `GEM_*`, the `channel` of an
//! `EXEC`. That is deliberately shallow. A trail is a sequence, not a decode:
//! its job is to say which of NVK's phases the process was in (enumeration,
//! allocation, binding, submission) and whether the answer just before the
//! fault was an error userspace then ignored.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lock::Mutex;

/// Entries kept per process. One screenful of serial console once rendered,
/// and deep enough to cover a whole submission (waits, binds, EXEC, signal).
pub const DEPTH: usize = 24;

/// Processes tracked at once. A compositor and the client it is serving, plus
/// slack; the least recently used slot is taken when a third shows up.
pub const SLOTS: usize = 4;

/// One recorded ioctl.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The full request number as the client encoded it, size and direction
    /// included -- `decode` in the driver keys on the low byte, but a wrong
    /// size is itself a thing worth seeing.
    pub request: u32,
    /// Thread that issued it. A compositor submits from more than one.
    pub tid: u32,
    /// First 64-bit field of the argument struct, or 0 when it could not be
    /// read. See the module doc for what this is worth.
    pub arg0: u64,
    /// `Ok(n)` as `n`, an error as its negative errno, exactly as the syscall
    /// return would carry it.
    pub ret: i64,
}

#[derive(Clone, Copy)]
struct Slot {
    pid: u32,
    /// Total entries ever recorded for this pid: the write cursor, and what
    /// tells a partly-filled ring from a wrapped one.
    count: u64,
    /// Monotonic stamp of the last write, for LRU eviction.
    used: u64,
    ring: [Entry; DEPTH],
}

impl Slot {
    const EMPTY: Slot = Slot {
        pid: 0,
        count: 0,
        used: 0,
        ring: [Entry {
            request: 0,
            tid: 0,
            arg0: 0,
            ret: 0,
        }; DEPTH],
    };
}

struct Trail {
    slots: [Slot; SLOTS],
    clock: u64,
}

static TRAIL: Mutex<Trail> = Mutex::new(Trail {
    slots: [Slot::EMPTY; SLOTS],
    clock: 0,
});

/// Record one completed DRM ioctl against `pid`.
///
/// Called on the hot path (an `EXEC` per draw), so it does exactly one
/// bounded array write under one uncontended lock and allocates nothing.
///
/// `pid` 0 is ignored: it is the kernel's own calls, which have no process to
/// report a fault for and would otherwise evict a real slot.
pub fn record(pid: u32, tid: u32, request: u32, arg0: u64, ret: i64) {
    if pid == 0 {
        return;
    }
    let mut t = TRAIL.lock();
    t.clock += 1;
    let clock = t.clock;
    let idx = match t.slots.iter().position(|s| s.count != 0 && s.pid == pid) {
        Some(i) => i,
        None => {
            // A free slot first, then the least recently used one. `used` is a
            // monotonic clock, so the minimum is unambiguous and a slot that
            // has never been written (used = 0) always wins.
            let i = t
                .slots
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.used)
                .map(|(i, _)| i)
                .unwrap_or(0);
            t.slots[i] = Slot::EMPTY;
            t.slots[i].pid = pid;
            i
        }
    };
    let slot = &mut t.slots[idx];
    let pos = (slot.count % DEPTH as u64) as usize;
    slot.ring[pos] = Entry {
        request,
        tid,
        arg0,
        ret,
    };
    slot.count += 1;
    slot.used = clock;
}

/// Forget everything recorded for `pid`.
///
/// Nothing calls this on process exit on purpose -- a slot is cheap and LRU
/// reclaims it -- but a test needs to start from a known state, and so does
/// anything that wants a trail to begin at a known point.
pub fn forget(pid: u32) {
    let mut t = TRAIL.lock();
    for s in t.slots.iter_mut() {
        if s.count != 0 && s.pid == pid {
            *s = Slot::EMPTY;
        }
    }
}

/// The entries recorded for `pid`, oldest first, or empty if this process
/// never issued a DRM ioctl (which is most of them).
pub fn entries_for(pid: u32) -> Vec<Entry> {
    let t = TRAIL.lock();
    let Some(slot) = t.slots.iter().find(|s| s.count != 0 && s.pid == pid) else {
        return Vec::new();
    };
    let kept = core::cmp::min(slot.count, DEPTH as u64) as usize;
    // The oldest surviving entry: the write cursor itself once the ring has
    // wrapped, index 0 while it is still filling.
    let first = if slot.count > DEPTH as u64 {
        (slot.count % DEPTH as u64) as usize
    } else {
        0
    };
    (0..kept).map(|i| slot.ring[(first + i) % DEPTH]).collect()
}

/// The trail for `pid`, rendered one line per ioctl, oldest first.
///
/// Empty when the process never issued a DRM ioctl, so a caller can print the
/// result unconditionally and a non-GPU crash gains no lines.
pub fn dump_for(pid: u32) -> Vec<String> {
    let entries = entries_for(pid);
    if entries.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(entries.len() + 1);
    out.push(format!(
        "drm ioctl trail for pid={} (oldest first, {} kept):",
        pid,
        entries.len()
    ));
    for e in entries {
        out.push(format!(
            "  {:>24} request={:#010x} tid={} arg0={:#x} -> {}",
            ioctl_name(e.request & 0xff),
            e.request,
            e.tid,
            e.arg0,
            render_ret(e.ret),
        ));
    }
    out
}

/// `ret` as userspace saw it: a count, or a negative errno with its name when
/// it is one of the handful a DRM client actually branches on.
fn render_ret(ret: i64) -> String {
    if ret >= 0 {
        return format!("{}", ret);
    }
    let name = match -ret {
        1 => "EPERM",
        2 => "ENOENT",
        9 => "EBADF",
        12 => "ENOMEM",
        13 => "EACCES",
        14 => "EFAULT",
        16 => "EBUSY",
        22 => "EINVAL",
        25 => "ENOTTY",
        28 => "ENOSPC",
        38 => "ENOSYS",
        62 => "ETIMEDOUT",
        95 => "EOPNOTSUPP",
        _ => return format!("-{}", -ret),
    };
    format!("-{} ({})", -ret, name)
}

/// Name for a DRM ioctl NR (`_IOC_NR`, the low byte).
///
/// Core DRM numbers are named here; the driver-private range delegates to the
/// driver's own vocabulary so `EXEC` and `VM_BIND` read as themselves rather
/// than as `0x52`/`0x51`. Anything unrecognised is rendered as its number,
/// which is still the useful half.
fn ioctl_name(nr: u32) -> String {
    let core = match nr {
        0x00 => "VERSION",
        0x01 => "GET_UNIQUE",
        0x02 => "GET_MAGIC",
        0x07 => "SET_VERSION",
        0x09 => "GEM_CLOSE",
        0x0C => "GET_CAP",
        0x0D => "SET_CLIENT_CAP",
        0x11 => "AUTH_MAGIC",
        0x1E => "SET_MASTER",
        0x1F => "DROP_MASTER",
        0x2D => "PRIME_HANDLE_TO_FD",
        0x2E => "PRIME_FD_TO_HANDLE",
        0x3A => "WAIT_VBLANK",
        0xA0 => "MODE_GETRESOURCES",
        0xA1 => "MODE_GETCRTC",
        0xA2 => "MODE_SETCRTC",
        0xA3 => "MODE_CURSOR",
        0xA4 => "MODE_GETGAMMA",
        0xA5 => "MODE_SETGAMMA",
        0xA6 => "MODE_GETENCODER",
        0xA7 => "MODE_GETCONNECTOR",
        0xAA => "MODE_GETPROPERTY",
        0xAB => "MODE_SETPROPERTY",
        0xAC => "MODE_GETPROPBLOB",
        0xAD => "MODE_GETFB",
        0xAE => "MODE_ADDFB",
        0xAF => "MODE_RMFB",
        0xB0 => "MODE_PAGE_FLIP",
        0xB1 => "MODE_DIRTYFB",
        0xB2 => "MODE_CREATE_DUMB",
        0xB3 => "MODE_MAP_DUMB",
        0xB4 => "MODE_DESTROY_DUMB",
        0xB5 => "MODE_GETPLANERESOURCES",
        0xB6 => "MODE_GETPLANE",
        0xB7 => "MODE_SETPLANE",
        0xB8 => "MODE_ADDFB2",
        0xB9 => "MODE_OBJ_GETPROPERTIES",
        0xBA => "MODE_OBJ_SETPROPERTY",
        0xBB => "MODE_CURSOR2",
        0xBC => "MODE_ATOMIC",
        0xBD => "MODE_CREATEPROPBLOB",
        0xBE => "MODE_DESTROYPROPBLOB",
        0xBF => "SYNCOBJ_CREATE",
        0xC0 => "SYNCOBJ_DESTROY",
        0xC1 => "SYNCOBJ_HANDLE_TO_FD",
        0xC2 => "SYNCOBJ_FD_TO_HANDLE",
        0xC3 => "SYNCOBJ_WAIT",
        0xC4 => "SYNCOBJ_RESET",
        0xC5 => "SYNCOBJ_SIGNAL",
        0xC6 => "MODE_CREATE_LEASE",
        0xC7 => "MODE_LIST_LESSEES",
        0xCA => "SYNCOBJ_TIMELINE_WAIT",
        0xCB => "SYNCOBJ_QUERY",
        0xCC => "SYNCOBJ_TRANSFER",
        0xCD => "SYNCOBJ_TIMELINE_SIGNAL",
        0xCE => "MODE_GETFB2",
        0xCF => "SYNCOBJ_EVENTFD",
        0xD0 => "MODE_CLOSEFB",
        0xD1 => "SYNCOBJ_WAIT_DEADLINE",
        0xD2 => "SYNCOBJ_TIMELINE_WAIT_DEADLINE",
        _ => "",
    };
    if !core.is_empty() {
        return String::from(core);
    }
    if (0x40..=0x9F).contains(&nr) {
        let driver = driver_private_name(nr);
        if driver != "unknown" {
            return String::from(driver);
        }
        return format!("private({:#04x})", nr);
    }
    format!("nr({:#04x})", nr)
}

/// The driver's own name for a private-range NR.
///
/// Only the x86_64 build has the nouveau uAPI at all, so elsewhere every
/// private number is simply unnamed rather than mis-named after a driver this
/// kernel does not carry.
#[cfg(target_arch = "x86_64")]
fn driver_private_name(nr: u32) -> &'static str {
    zcore_drivers::display::nouveau_ioctl_name(nr)
}

/// See the x86_64 arm.
#[cfg(not(target_arch = "x86_64"))]
fn driver_private_name(_nr: u32) -> &'static str {
    "unknown"
}

#[cfg(test)]
mod tests {
    //! Host tests for the ring: what it keeps, what it drops, and what the
    //! rendered report says. They run against the one global trail, so each
    //! picks pids of its own and clears them first.

    use super::*;

    /// The trail is ONE global with `SLOTS` slots, and the eviction test needs
    /// every one of them, so these cannot overlap. The turnstile lives beside
    /// the global it protects -- this module owns the only writer -- and is
    /// taken for the whole body of each test.
    static TURNSTILE: Mutex<()> = Mutex::new(());

    /// Pids well above anything another test in this file uses, so the four
    /// slots are never fought over.
    fn fresh(pid: u32) -> u32 {
        forget(pid);
        pid
    }

    #[test]
    fn a_process_that_never_touched_drm_has_no_trail() {
        let _turn = TURNSTILE.lock();
        assert!(dump_for(fresh(9_001)).is_empty());
        assert!(entries_for(9_001).is_empty());
    }

    #[test]
    fn entries_come_back_oldest_first() {
        let _turn = TURNSTILE.lock();
        let pid = fresh(9_002);
        for i in 0..3u64 {
            record(pid, 7, 0xC000_0040 | i as u32, i, i as i64);
        }
        let got = entries_for(pid);
        assert_eq!(got.len(), 3);
        assert_eq!(got.iter().map(|e| e.arg0).collect::<Vec<_>>(), [0, 1, 2]);
        forget(pid);
    }

    /// The ring is the point: a compositor issues thousands of ioctls and the
    /// report must show the LAST `DEPTH`, in order, not the first.
    #[test]
    fn a_wrapped_ring_keeps_the_newest_in_order() {
        let _turn = TURNSTILE.lock();
        let pid = fresh(9_003);
        for i in 0..(DEPTH as u64 * 3 + 5) {
            record(pid, 7, 0xC000_0040, i, 0);
        }
        let got = entries_for(pid);
        assert_eq!(got.len(), DEPTH);
        let total = DEPTH as u64 * 3 + 5;
        let expected: Vec<u64> = ((total - DEPTH as u64)..total).collect();
        assert_eq!(got.iter().map(|e| e.arg0).collect::<Vec<_>>(), expected);
        forget(pid);
    }

    /// Exactly `DEPTH` recorded is the boundary between "still filling" and
    /// "wrapped", and the cursor arithmetic differs on each side of it.
    #[test]
    fn a_ring_filled_exactly_to_the_brim_is_not_rotated() {
        let _turn = TURNSTILE.lock();
        let pid = fresh(9_004);
        for i in 0..DEPTH as u64 {
            record(pid, 7, 0xC000_0040, i, 0);
        }
        let got = entries_for(pid);
        assert_eq!(
            got.iter().map(|e| e.arg0).collect::<Vec<_>>(),
            (0..DEPTH as u64).collect::<Vec<_>>()
        );
        forget(pid);
    }

    #[test]
    fn one_process_cannot_see_anothers_calls() {
        let _turn = TURNSTILE.lock();
        let (a, b) = (fresh(9_005), fresh(9_006));
        record(a, 1, 0xC000_0040, 0xaa, 0);
        record(b, 1, 0xC000_0040, 0xbb, 0);
        assert_eq!(entries_for(a).len(), 1);
        assert_eq!(entries_for(a)[0].arg0, 0xaa);
        assert_eq!(entries_for(b)[0].arg0, 0xbb);
        forget(a);
        forget(b);
    }

    /// With more processes than slots the OLDEST-used one goes, not the
    /// newest: the process about to fault is the one that just ran.
    #[test]
    fn a_new_process_evicts_the_least_recently_used_slot() {
        let _turn = TURNSTILE.lock();
        let base = 9_100;
        for i in 0..SLOTS as u32 {
            forget(base + i);
        }
        forget(base + SLOTS as u32);
        for i in 0..SLOTS as u32 {
            record(base + i, 1, 0xC000_0040, i as u64, 0);
        }
        // Touch every slot but the first, so the first is the LRU.
        for i in 1..SLOTS as u32 {
            record(base + i, 1, 0xC000_0040, 0xff, 0);
        }
        record(base + SLOTS as u32, 1, 0xC000_0040, 0x99, 0);
        assert!(
            entries_for(base).is_empty(),
            "the least recently used slot should have been taken"
        );
        for i in 1..SLOTS as u32 {
            assert!(!entries_for(base + i).is_empty(), "slot {} was evicted", i);
        }
        assert_eq!(entries_for(base + SLOTS as u32)[0].arg0, 0x99);
        for i in 0..=SLOTS as u32 {
            forget(base + i);
        }
    }

    /// pid 0 is the kernel's own driver calls. Recording them would evict a
    /// real process's slot for a trail no crash report can ever ask for.
    #[test]
    fn the_kernels_own_calls_are_not_recorded() {
        let _turn = TURNSTILE.lock();
        forget(0);
        record(0, 0, 0xC000_0040, 1, 0);
        assert!(entries_for(0).is_empty());
    }

    #[test]
    fn a_line_names_the_ioctl_and_spells_the_errno() {
        let _turn = TURNSTILE.lock();
        let pid = fresh(9_007);
        // GETPARAM (private 0x40) asking for param 13, refused.
        record(pid, 42, 0xC010_6440, 13, -22);
        let lines = dump_for(pid);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("pid=9007"), "{}", lines[0]);
        assert!(lines[1].contains("GETPARAM"), "{}", lines[1]);
        assert!(lines[1].contains("tid=42"), "{}", lines[1]);
        assert!(lines[1].contains("arg0=0xd"), "{}", lines[1]);
        assert!(lines[1].contains("-22 (EINVAL)"), "{}", lines[1]);
        forget(pid);
    }

    /// The new submission path is what a Vulkan crash is about, so its names
    /// must survive the trip through the private-range delegation.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn the_private_range_is_named_by_the_driver() {
        assert_eq!(ioctl_name(0x40), "GETPARAM");
        assert_eq!(ioctl_name(0x51), "VM_BIND");
        assert_eq!(ioctl_name(0x52), "EXEC");
        assert_eq!(ioctl_name(0x84), "GEM_INFO");
    }

    #[test]
    fn core_numbers_are_named_and_unknown_ones_still_show_their_number() {
        assert_eq!(ioctl_name(0xC3), "SYNCOBJ_WAIT");
        assert_eq!(ioctl_name(0x2D), "PRIME_HANDLE_TO_FD");
        assert_eq!(ioctl_name(0xFE), "nr(0xfe)");
    }

    /// A successful call carries a count, not an errno, and must not be
    /// dressed up as one.
    #[test]
    fn a_success_renders_as_a_plain_number() {
        assert_eq!(render_ret(0), "0");
        assert_eq!(render_ret(7), "7");
        assert_eq!(render_ret(-99), "-99");
    }
}
