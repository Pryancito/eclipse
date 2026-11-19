//! The kernel's side of the mount helper, against a fake helper on a
//! thread: what `mount(2)` sends it and what comes back.

extern crate std;

use super::*;
use crate::fs::served::wire::{self, MSG, NAME_AREA, SLOTS, SLOT_LEN};
use alloc::string::String;
use alloc::vec;
use core::convert::TryInto;
use std::sync::mpsc;
use std::thread;
use zircon_object::ZxError;

/// What the fake helper was asked: the names, `a`, and the data area.
type Asked = (String, u64, String);

/// A fake helper that answers each mount with `status`, and tells what it
/// was asked. It goes after `answers` mounts.
fn helper(status: i64, answers: usize) -> (Helper, mpsc::Receiver<Asked>) {
    let (ours, theirs) = Fifo::create(SLOTS, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let link = Arc::new(Link::new(ours, window.clone()).unwrap());
    let (tell, asked) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..answers {
            let mut m = [0u8; MSG];
            loop {
                match theirs.read(MSG, &mut m, 1) {
                    Ok(_) => break,
                    Err(ZxError::SHOULD_WAIT) => thread::yield_now(),
                    Err(_) => return,
                }
            }
            let request = wire::decode(&m);
            assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()), OP_MOUNT);
            let names = u32::from_ne_bytes(m[20..24].try_into().unwrap()) as usize;
            let slot = (request.reqid as usize & (SLOTS - 1)) * SLOT_LEN;
            let mut path = vec![0u8; names];
            window.read(slot, &mut path).unwrap();
            let mut data = vec![0u8; request.b as usize];
            window.read(slot + NAME_AREA, &mut data).unwrap();
            let _ = tell.send((
                String::from_utf8(path).unwrap(),
                request.a,
                String::from_utf8(data).unwrap(),
            ));
            m[24..32].copy_from_slice(&status.to_ne_bytes());
            theirs.write(MSG, &m, 1).unwrap();
        }
        // Gone: its end of the FIFO with it.
    });
    (install(link), asked)
}

/// One test, as the helper is one for the whole kernel.
#[test]
fn a_mount_goes_to_the_helper_and_mount_returns_its_answer() {
    // None there: the type is one the kernel has no code for.
    *HELPER.lock() = None;
    assert!(!present());
    assert_eq!(
        ask("/dev/sda1", "/mnt", "vfat", 0, ""),
        Err(LxError::ENODEV)
    );

    // The names, the flags and the options, as the helper reads them.
    let (h, asked) = helper(0, 1);
    assert!(present());
    assert_eq!(
        ask("/dev/sda1", "/boot/efi", "vfat", 1, "umask=077,utf8"),
        Ok(())
    );
    assert_eq!(
        asked.recv().unwrap(),
        (
            String::from("/dev/sda1\0/boot/efi\0vfat"),
            1,
            String::from("umask=077,utf8")
        )
    );
    // It went after one: ENODEV, as before it came.
    let gone = ask("/dev/sda1", "/mnt", "vfat", 0, "");
    assert_eq!(gone, Err(LxError::ENODEV));
    h.left();
    assert!(!present());

    // Its errno is mount(2)'s.
    let (h, _asked) = helper(-(libc_einval()), 1);
    assert_eq!(ask("/img", "/mnt", "vfat", 0, ""), Err(LxError::EINVAL));
    h.left();

    // A NUL in a name would make it another name: refused here.
    let (h, _asked) = helper(0, 1);
    assert_eq!(ask("/a\0b", "/mnt", "vfat", 0, ""), Err(LxError::EINVAL));
    // The helper that went is not the one there: its going leaves it.
    let (next, _asked2) = helper(0, 1);
    h.left();
    assert!(present());
    next.left();
    assert!(!present());
}

fn libc_einval() -> i64 {
    LxError::EINVAL as i64
}
