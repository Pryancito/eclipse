use super::gl_client_sequence_tests::{parse_events, Client, FLIP_COMPLETE};
use super::*;
use crate::fs::devfs::kms_emu;

/// Put one flip completion on `c`'s queue. The caller holds the attached
/// [`kms_emu::Screen`] the present needs.
fn queue_one_flip(c: &Client, user_data: u64) -> (u32, u32) {
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, user_data)
        .expect("flip");
    drm::flush_pending_flip_completions();
    (fb, buf.handle)
}

/// An empty queue is `EAGAIN`, not a short read and not `EINVAL`. A
/// compositor that gets anything else on the very common "poll woke me for
/// something else" path treats the card fd as broken and tears the output
/// down.
#[test]
fn an_empty_queue_reads_eagain_and_not_a_broken_fd() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let mut buf = [0u8; 32];

    assert_eq!(c.read_events(&mut buf), Err(FsError::Again));
    // And nothing was written into the buffer.
    assert!(buf.iter().all(|&b| b == 0));
}

/// A buffer too small for one event reads 0 bytes, as `drm_read()` puts
/// the event back and returns what it had read so far, and the event
/// STAYS queued. `EAGAIN` here is a livelock (the queue is non-empty, so
/// the file is still readable and the wait resolves instantly, over and
/// over), dropping the event instead would lose the flip completion
/// wlroots is waiting on -- a desktop frozen on its current frame -- and
/// `EINVAL`, which this answered, is a failed read to `drmHandleEvent`
/// where Linux hands it "nothing this time". And a write to the card fd
/// is EINVAL (the DRM file operations have no `.write`); this took the
/// bytes and said they were written.
#[test]
fn a_buffer_too_small_for_one_event_reads_nothing_and_keeps_the_event() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let (fb, handle) = queue_one_flip(&c, 0xABCD);

    assert_eq!(c.write(b"not a DRM event"), Err(FsError::InvalidParam));

    let mut small = [0u8; 16];
    assert_eq!(c.read_events(&mut small), Ok(0));
    assert!(
        small.iter().all(|&b| b == 0),
        "a partial event was delivered"
    );

    // Still there, and still whole.
    let mut full = [0u8; 32];
    assert_eq!(c.read_events(&mut full).expect("the event survived"), 32);
    let ev = parse_events(&full);
    assert_eq!(ev[0].ev_type, FLIP_COMPLETE);
    assert_eq!(ev[0].user_data, 0xABCD);
    // Drained now.
    assert_eq!(c.read_events(&mut full), Err(FsError::Again));

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(handle).expect("DESTROY_DUMB");
}

/// One client's completion is not readable by another. The queue lives on
/// the open file, like Linux's `struct drm_file`: with a single device-wide
/// stream, a probing client (Xwayland during session bring-up) reads the
/// compositor's flip completion out from under it and wlroots then waits on
/// an event that has already been consumed.
#[test]
fn one_clients_flip_completion_is_not_readable_by_another() {
    let _screen = kms_emu::attach(32, 8);
    let flipper = Client::open(0);
    let bystander = Client::open(0);
    let (fb, handle) = queue_one_flip(&flipper, 0x1234);

    let mut buf = [0u8; 32];
    assert_eq!(
        bystander.read_events(&mut buf),
        Err(FsError::Again),
        "another open file drained the completion"
    );
    assert!(!bystander.poll().expect("poll").read);

    // And the client that asked for it still has it.
    assert_eq!(flipper.read_events(&mut buf).expect("own completion"), 32);
    assert_eq!(parse_events(&buf)[0].user_data, 0x1234);

    flipper.rmfb(fb).expect("RMFB");
    flipper.destroy_dumb(handle).expect("DESTROY_DUMB");
}

/// `poll` says readable exactly while an event is queued. It is what parks
/// the compositor's main loop: stuck at readable burns a core in a spin, and
/// stuck at not-readable is a desktop that never sees its own flip land.
#[test]
fn poll_reports_readable_only_while_an_event_is_queued() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    assert!(
        !c.poll().expect("poll").read,
        "readable with an empty queue"
    );

    let (fb, handle) = queue_one_flip(&c, 1);
    assert!(
        c.poll().expect("poll").read,
        "not readable with an event queued"
    );

    // A short read, which takes nothing, must not clear it either.
    let mut small = [0u8; 8];
    assert_eq!(c.read_events(&mut small), Ok(0));
    assert!(
        c.poll().expect("poll").read,
        "a short read consumed the event"
    );

    let mut full = [0u8; 32];
    assert_eq!(c.read_events(&mut full).expect("drain"), 32);
    assert!(!c.poll().expect("poll").read, "readable after the drain");
    // Linux drm_poll never reports POLLOUT on the chardev.
    assert!(!c.poll().expect("poll").write);

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(handle).expect("DESTROY_DUMB");
}

/// Several queued events come out one read at a time, in order, and a
/// buffer big enough for two takes two. A reader that got them out of order
/// would mis-pair completions with the frames that asked for them.
#[test]
fn queued_events_come_out_in_order_and_a_big_buffer_takes_several() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);

    // Two frames, each completion collected... by a reader that waits until
    // both are in, which is what a compositor doing two outputs looks like.
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x11).expect("flip 1");
    drm::flush_pending_flip_completions();
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x22).expect("flip 2");
    drm::flush_pending_flip_completions();

    let mut both = [0u8; 64];
    assert_eq!(c.read_events(&mut both).expect("two events"), 64);
    let ev = parse_events(&both);
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0].user_data, 0x11, "the events came out reversed");
    assert_eq!(ev[1].user_data, 0x22);
    assert_eq!(
        ev[0].length, 32,
        "the wire length must match the reader's stride"
    );

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}
