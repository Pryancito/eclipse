use super::*;
use zcore_drivers::scheme::gem_mmap::{self, DecRef};

const CLIENT: u64 = 88_201;

/// Register a nouveau GEM object the way `GEM_NEW` does: one holder, its
/// creator.
fn gem_new(handle: u32, pid: u64) {
    gem_mmap::register(handle, 0x40_0000, 4096, pid);
}

/// The regression, end to end and in the compositor's own order:
/// `ADDFB2`, then `GEM_CLOSE`. The close must NOT be the last reference,
/// because the framebuffer holds one.
#[test]
fn addfb2_then_gem_close_leaves_the_framebuffer_backed() {
    let _serialised = super::test_globals::lock();
    let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x81;
    gem_new(handle, CLIENT);

    let fb_id = create_fb(handle, 1, 1, 4).expect("ADDFB2 over a nouveau GEM object");

    // The client lets go of its handle, exactly as wlroots does the
    // instant ADDFB2 returns. Before the fb took a reference this was the
    // last one: the memory went back to the RM and the fb went with it.
    assert_eq!(
        gem_mmap::dec_ref(handle, CLIENT),
        DecRef::StillReferenced(1),
        "the framebuffer's own reference must outlive the client's handle",
    );
    assert!(
        gem_mmap::lookup(handle).is_some(),
        "the GEM object must still be alive for the fb to scan out",
    );

    // And the framebuffer is still there to be presented -- this is the
    // ENOENT storm, reduced to one assertion.
    assert_ne!(
        present_now_checked(fb_id, 1, None),
        Err(PresentError::NoSuchFb),
        "SETCRTC would have answered ENOENT for the rest of the session",
    );

    // RMFB is what finally releases it, as on Linux.
    assert!(rmfb(fb_id));
    assert!(
        gem_mmap::lookup(handle).is_none(),
        "RMFB dropped the last reference, so the object is freed",
    );
}

/// The reference is the framebuffer's, not the creating process's: it has
/// to survive that process's exit sweep, or a buffer still being scanned
/// out is freed under the compositor.
#[test]
fn the_framebuffers_reference_belongs_to_no_process() {
    let _serialised = super::test_globals::lock();
    let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x82;
    gem_new(handle, CLIENT);
    let fb_id = create_fb(handle, 1, 1, 4).expect("ADDFB2 over a nouveau GEM object");

    // Everything the client held goes; the fb's reference is not the
    // client's to give up.
    gem_mmap::release_pid(CLIENT);
    assert!(
        gem_mmap::lookup(handle).is_some(),
        "a process exit must not free memory a framebuffer still names",
    );

    assert!(rmfb(fb_id));
    assert!(gem_mmap::lookup(handle).is_none());
}

/// Two framebuffers over one buffer take two references, and it takes both
/// `RMFB`s to free it. A compositor really does this -- one fb per
/// modifier/format it tests a buffer with.
#[test]
fn each_framebuffer_takes_its_own_reference() {
    let _serialised = super::test_globals::lock();
    let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x83;
    gem_new(handle, CLIENT);
    let a = create_fb(handle, 1, 1, 4).expect("first ADDFB2");
    let b = create_fb(handle, 1, 1, 4).expect("second ADDFB2");
    assert_ne!(a, b);

    assert_eq!(
        gem_mmap::dec_ref(handle, CLIENT),
        DecRef::StillReferenced(2)
    );
    assert!(rmfb(a));
    assert!(
        gem_mmap::lookup(handle).is_some(),
        "the second framebuffer still names this memory",
    );
    assert!(rmfb(b));
    assert!(gem_mmap::lookup(handle).is_none());
}

/// A dumb buffer is not tracked in `gem_mmap` at all, and must not be
/// touched by any of this: its reference is the `Arc<VmObject>` in
/// `fb_backing`, and `gem_close_keeps_a_framebuffer_and_its_memory_alive`
/// owns that half of the contract.
#[test]
fn a_dumb_buffer_framebuffer_takes_no_gem_reference() {
    let _serialised = super::test_globals::lock();
    let low = 4242; // below DRIVER_HANDLE_BASE: a CREATE_DUMB handle
    assert!(gem_mmap::lookup(low).is_none());
    fb_take_gem_ref(low);
    assert!(
        gem_mmap::lookup(low).is_none(),
        "a dumb handle must never appear in the nouveau table",
    );
    fb_drop_gem_ref(low); // and dropping one that was never taken is safe
}
