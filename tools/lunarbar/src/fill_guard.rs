//! Soft ceilings for state that must not grow without bound across a long
//! desktop session (the KERNEL PAGE FAULT class at ~30–40 min was often a
//! filled EventBus / waker list; userspace must not amplify that with its own
//! unbounded Vec/HashMap growth).

/// Foreign-toplevel handles tracked for the taskbar. A healthy session has a
/// handful; if `Closed` is missed, this bound refuses further inserts so the
/// bar cannot grow without limit (and so we never `destroy()` a still-live
/// handle just to make room).
pub const MAX_TRACKED_TOPLEVELS: usize = 512;

/// Claimed `wl_output` globals we keep metadata for.
pub const MAX_OUTPUTS: usize = 32;

/// Bar surfaces (two per output: top + bottom).
pub const MAX_BARS: usize = MAX_OUTPUTS * 2;

/// Soft cap on retired shm mappings awaiting `wl_buffer.Release`. Under a
/// resize storm we keep more than this only until Releases arrive; past
/// `MAX_RETIRED_MAPS * 4` the oldest is force-unmapped as a last resort
/// (logged) so a stuck compositor cannot OOM the panel.
pub const MAX_RETIRED_MAPS: usize = 16;

/// Maximum edge of a bar/popup/tooltip buffer in pixels (after scale).
pub const MAX_BUFFER_DIM: u32 = 8192;

/// Peak pixels for one surface (logical or buffer). 64 Mpx covers 8K twice.
pub const MAX_BUFFER_PIXELS: usize = 64 << 20;

/// Reject / drop when a list would grow past `max`. Returns false if the push
/// was refused (list unchanged). Prefer this over eviction that destroys a
/// still-live protocol object.
pub fn try_push_bounded<T>(list: &mut Vec<T>, item: T, max: usize) -> bool {
    if list.len() >= max {
        return false;
    }
    list.push(item);
    true
}

/// Truncate a protocol/UI string so a hostile/noisy compositor cannot grow
/// the heap without bound via titles / app_ids.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

pub const MAX_TITLE_CHARS: usize = 512;
pub const MAX_APP_ID_CHARS: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_push_refuses_past_cap() {
        let mut v = Vec::new();
        for i in 0..(MAX_TRACKED_TOPLEVELS + 50) {
            let ok = try_push_bounded(&mut v, i, MAX_TRACKED_TOPLEVELS);
            if i < MAX_TRACKED_TOPLEVELS {
                assert!(ok);
            } else {
                assert!(!ok);
            }
        }
        assert_eq!(v.len(), MAX_TRACKED_TOPLEVELS);
        assert_eq!(v[0], 0);
        assert_eq!(*v.last().unwrap(), MAX_TRACKED_TOPLEVELS - 1);
    }

    #[test]
    fn long_session_missed_closed_cannot_grow_past_cap() {
        let mut v = Vec::new();
        for i in 0..(40 * 60) {
            let _ = try_push_bounded(&mut v, i, MAX_TRACKED_TOPLEVELS);
        }
        assert!(v.len() <= MAX_TRACKED_TOPLEVELS);
    }

    #[test]
    fn truncate_chars_honours_unicode() {
        let s = "áéíóú".repeat(200);
        let t = truncate_chars(&s, 10);
        assert_eq!(t.chars().count(), 10);
    }
}
