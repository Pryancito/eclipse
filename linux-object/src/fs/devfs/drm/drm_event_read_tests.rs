use super::*;

fn event(tag: u8, len: usize) -> Vec<u8> {
    alloc::vec![tag; len]
}

/// The livelock. A short read left the event queued with `READABLE` still
/// set, and answered EAGAIN -- so a blocking reader's wait resolved
/// immediately, it re-read, got EAGAIN again, and spun a core with no
/// yield point. `drm_read()` returns 0 there and never blocks.
#[test]
fn a_buffer_too_small_for_the_first_event_is_distinguishable_from_empty() {
    let file = DrmFileState::new();
    let mut buf = [0u8; 8];
    assert_eq!(file.read_events(&mut buf), EventRead::Empty);

    file.push_event(event(0xAB, 32));
    assert_eq!(
        file.read_events(&mut buf),
        EventRead::TooSmall,
        "a short read must not look like an empty queue"
    );
    // And the event is still there, unconsumed.
    assert!(file.has_events());
    let mut big = [0u8; 32];
    assert_eq!(file.read_events(&mut big), EventRead::Read(32));
    assert!(big.iter().all(|&b| b == 0xAB));
    assert!(!file.has_events());
}

/// Linux fills the buffer with every whole event that fits, not just one.
#[test]
fn one_read_drains_as_many_whole_events_as_fit() {
    let file = DrmFileState::new();
    file.push_event(event(1, 32));
    file.push_event(event(2, 32));
    file.push_event(event(3, 32));

    // Room for two and a half: two come back, the third stays queued.
    let mut buf = [0u8; 80];
    assert_eq!(file.read_events(&mut buf), EventRead::Read(64));
    assert!(buf[..32].iter().all(|&b| b == 1));
    assert!(buf[32..64].iter().all(|&b| b == 2));
    assert!(file.has_events());

    assert_eq!(file.read_events(&mut buf), EventRead::Read(32));
    assert_eq!(file.read_events(&mut buf), EventRead::Empty);
}
