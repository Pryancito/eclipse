//! Futexes shared between processes, keyed the way Linux keys them.
//!
//! A futex issued WITHOUT `FUTEX_PRIVATE_FLAG` names a word that two different
//! processes wait on and wake. [`LinuxProcess::get_futex`] cannot serve those:
//! it keys a per-process map by the caller's own virtual address, so each side
//! of a shared word gets its own queue and a `FUTEX_WAKE` in one process can
//! never reach a waiter in the other.
//!
//! That is not a corner case. **libxshmfence** — and with it every DRI3 client
//! on X11, which is every GL application running under Xwayland —
//! synchronises with the X server through exactly this shape: a shared `memfd`
//! holding one `int32`, `FUTEX_WAIT` on the client side and `FUTEX_WAKE` on
//! the server side, neither carrying the private flag. The client draws,
//! blocks waiting for the server to release the buffer, and stays there
//! forever: a frozen window and no return to the prompt. Wayland clients do
//! not use xshmfence, which is the whole asymmetry between `eglgears_wayland`
//! working and `glxgears` hanging on the same machine.
//!
//! Linux keys a shared futex by "inode + offset". The equivalent pair here is
//! the backing [`VmObject`]'s koid and the byte offset of the word inside it,
//! so two processes that map the same object at different addresses land on
//! one [`Futex`].
//!
//! Two details to respect if this is ever touched:
//!
//! - The word is reached through the kernel's linear map of the physical
//!   frame, never through either process's virtual address: the `Futex`
//!   outlives any one address space, and whichever process happens to wake it
//!   must be able to read it. [`shared_futex_at`] therefore forces the page
//!   resident first and then translates.
//! - A **private** mapping still goes to the per-process table even when the
//!   futex carries no private flag. Nobody else can observe that word, and
//!   unifying them would be wrong under copy-on-write. The test is
//!   [`VmObject::is_shared_object`].
//!
//! The table holds a strong reference to the backing object — so the frame the
//! word lives in cannot be freed under the `&'static` the `Futex` keeps — and
//! a weak one to the `Futex` itself, so it prunes rather than growing one
//! entry per distinct word for the life of the machine.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::AtomicI32;
use kernel_hal::sync::Mutex;
use zircon_object::object::KoID;
use zircon_object::signal::Futex;
use zircon_object::vm::VmObject;

/// "Inode + offset": the backing object's koid and the byte offset of the
/// futex word inside it.
type SharedKey = (KoID, usize);

/// What the table keeps per word: a weak reference to the `Futex`, so an entry
/// nobody uses is reclaimed rather than accumulating for the life of the
/// machine, and a strong one to the backing object, so the frame the word
/// lives in cannot be freed under the `&'static` the `Futex` holds.
type Entry = (Weak<Futex>, Arc<VmObject>);

/// Every shared futex word currently known, by [`SharedKey`].
static SHARED_FUTEXES: Mutex<BTreeMap<SharedKey, Entry>> = Mutex::new(BTreeMap::new());

/// Intern the [`Futex`] for `key`, creating it over `word` if this is the
/// first caller to ask for it.
///
/// `vmo` is the object the word lives in; the table keeps it alive for as long
/// as the entry exists, which is what makes `word`'s `'static` lifetime honest.
/// `word` must point into the kernel's linear map of that object's frame, not
/// into any process's address space.
pub fn intern(key: SharedKey, vmo: Arc<VmObject>, word: &'static AtomicI32) -> Arc<Futex> {
    let mut table = SHARED_FUTEXES.lock();
    if let Some((weak, _)) = table.get(&key) {
        if let Some(futex) = weak.upgrade() {
            return futex;
        }
    }
    // Absent, or present but dead: replace it. Pruning the rest here would
    // cost a full walk on every futex call, and a dead entry is reclaimed the
    // next time its own key is asked for.
    let futex = Futex::new(word);
    table.insert(key, (Arc::downgrade(&futex), vmo));
    futex
}

/// How many entries the table currently holds, live and not yet reclaimed.
#[cfg(test)]
pub fn len() -> usize {
    SHARED_FUTEXES.lock().len()
}

/// Drop every entry. Tests only: the table is process-global, and a leftover
/// entry from one test would be found by the next.
#[cfg(test)]
pub fn clear_for_test() {
    SHARED_FUTEXES.lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;

    fn leaked_word() -> &'static AtomicI32 {
        Box::leak(Box::new(AtomicI32::new(0)))
    }

    fn some_vmo() -> Arc<VmObject> {
        VmObject::new_paged(1)
    }

    #[test]
    fn the_same_key_gives_the_same_futex() {
        clear_for_test();
        let vmo = some_vmo();
        let word = leaked_word();
        let a = intern((7, 0x40), vmo.clone(), word);
        let b = intern((7, 0x40), vmo, word);
        assert!(
            Arc::ptr_eq(&a, &b),
            "two processes mapping the same object at different addresses must \
             share one queue, or a wake in one never reaches a waiter in the other"
        );
    }

    #[test]
    fn different_offsets_in_one_object_are_different_futexes() {
        clear_for_test();
        let vmo = some_vmo();
        let a = intern((9, 0), vmo.clone(), leaked_word());
        let b = intern((9, 4), vmo.clone(), leaked_word());
        let c = intern((10, 0), vmo, leaked_word());
        assert!(!Arc::ptr_eq(&a, &b), "offset is part of the key");
        assert!(!Arc::ptr_eq(&a, &c), "the object is part of the key");
    }

    #[test]
    fn a_wake_through_one_lookup_reaches_a_waiter_queued_through_the_other() {
        use core::future::Future;
        use core::pin::Pin;
        use core::sync::atomic::Ordering;
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn noop_waker() -> Waker {
            fn clone(_: *const ()) -> RawWaker {
                RawWaker::new(core::ptr::null(), &VTABLE)
            }
            fn nop(_: *const ()) {}
            static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, nop, nop, nop);
            unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
        }

        clear_for_test();
        let vmo = some_vmo();
        let word = leaked_word();
        word.store(1, Ordering::SeqCst);
        let key = (12, 0x100);

        // The waiting process interns the word and queues on it.
        let waiter_side = intern(key, vmo.clone(), word);
        let mut fut = Box::pin(waiter_side.wait(1));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(
            Pin::new(&mut fut).poll(&mut cx).is_pending(),
            "the value still matches, so this must block"
        );

        // The waking process looks the same word up by itself -- a different
        // address space, the same object and offset -- and wakes it. This is
        // the whole point: with per-process queues it woke nobody.
        let waker_side = intern(key, vmo, word);
        assert_eq!(
            waker_side.wake(1),
            1,
            "a wake through a second lookup of the same key must reach the waiter"
        );
        assert!(matches!(
            Pin::new(&mut fut).poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    #[test]
    fn an_entry_is_reclaimed_once_nobody_holds_the_futex() {
        clear_for_test();
        let vmo = some_vmo();
        let key = (11, 0x80);
        let first = intern(key, vmo.clone(), leaked_word());
        let first_ptr = Arc::as_ptr(&first);
        drop(first);
        // The weak entry is still there, but dead: the next lookup must build
        // a new object rather than hand back a corpse.
        let second = intern(key, vmo, leaked_word());
        assert_ne!(
            Arc::as_ptr(&second),
            first_ptr,
            "a dead entry must be replaced, not upgraded"
        );
        assert_eq!(len(), 1, "and replaced in place, not accumulated");
    }
}
