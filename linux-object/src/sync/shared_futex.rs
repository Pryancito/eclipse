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

use crate::process::ProcessExt;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::AtomicI32;
use kernel_hal::sync::Mutex;
use zircon_object::object::{KernelObject, KoID};
use zircon_object::signal::Futex;
use zircon_object::task::Process;
use zircon_object::vm::{MMUFlags, VmAddressRegion, VmObject, PAGE_SIZE};

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

/// The `Futex` a process-shared futex word names, or `None` when this
/// word is not shared and the per-process table is the right home for it.
///
/// Keyed by the pair Linux spells "inode + offset" -- the backing
/// `VmObject`'s koid and the byte offset of the word inside it -- so two
/// processes that map the same object at different addresses reach one
/// queue. The word itself is reached through the kernel's linear map of the
/// physical frame, never through either process's virtual address: the
/// `Futex` outlives any one address space, and the process doing the
/// `FUTEX_WAKE` is generally not the one that created it.
///
/// Every failure here is a `None` that falls back to the per-process
/// table, which is exactly the behaviour this kernel had before.
pub fn shared_futex_at(vmar: &Arc<VmAddressRegion>, uaddr: usize) -> Option<Arc<Futex>> {
    if uaddr == 0 || !uaddr.is_multiple_of(core::mem::align_of::<AtomicI32>()) {
        return None;
    }
    // A second descent for the address the bounds check just walked to, and
    // a third lock to read the mapping's VMO -- all so the common answer can
    // be "no". Counted so the next bench run says what that costs.
    let found = vmar
        .find_mapping(uaddr)
        .and_then(|mapping| mapping.vmo_and_offset(uaddr).map(|vo| (mapping, vo)));
    let Some((mapping, (vmo, offset))) = found else {
        kernel_hal::kstats::note_futex_shared_probe(1, false);
        return None;
    };
    // A PRIVATE mapping keeps the per-process table even without the
    // private flag: nobody else can observe that word, and sharing a queue
    // across a copy-on-write split would be wrong.
    if !vmo.is_shared_object() {
        kernel_hal::kstats::note_futex_shared_probe(1, false);
        return None;
    }
    kernel_hal::kstats::note_futex_shared_probe(1, true);
    // Force the page resident before translating: a lazily mapped word has
    // no page-table entry yet, and `query_vaddr` would simply fail. ONLY
    // then: a read fault on a page that is already there re-installs it
    // read-only (the copy-on-write rule of `VmMapping::handle_page_fault`),
    // so asking unconditionally took WRITE away from the caller's own page on
    // every shared futex call -- a TLB flush here and a write fault on its
    // next store, for every `xshmfence` wait and wake of a GL client.
    //
    // Asked of the page, and the word found inside it below: the bare-metal
    // page table adds the offset itself, but the libos one only answers for
    // a page boundary (`debug_assert!(is_aligned(vaddr))`, and the frame's
    // base without the offset in a release build), so a word anywhere but at
    // the start of its page was never found on the host.
    let in_page = uaddr & (PAGE_SIZE - 1);
    if vmar.get_vaddr_flags(uaddr - in_page).is_err() {
        let _ = vmar.handle_page_fault(uaddr, MMUFlags::READ);
    }
    let (page, flags, _) = mapping.query_vaddr(uaddr - in_page).ok()?;
    // Empty flags mean "present in the tables with no permissions", which
    // is how a guard page looks; there is nothing readable there.
    if flags.is_empty() {
        return None;
    }
    let kvaddr = kernel_hal::mem::phys_to_virt(page + in_page);
    if kvaddr == 0 {
        return None;
    }
    // Safe: `kvaddr` is the kernel's own linear-map address of a resident,
    // 4-byte-aligned frame the table now holds an `Arc<VmObject>` on, so
    // it stays mapped and owned for as long as the `Futex` can be reached.
    let word: &'static AtomicI32 = unsafe { &*(kvaddr as *const AtomicI32) };
    Some(intern((vmo.id(), offset), vmo, word))
}

/// The `Futex` a futex operation WITHOUT `FUTEX_PRIVATE_FLAG` on `uaddr`
/// reaches in `proc`: the shared one when the word lives in a shared object,
/// the per-process one otherwise.
///
/// The two wakes the kernel issues on its own when a thread dies are of this
/// kind. Linux's `handle_futex_death` (a robust lock the dead thread still
/// held) and the `clear_child_tid` wake in `mm_release` both pass shared
/// flags, so a waiter in ANOTHER process -- a `PTHREAD_PROCESS_SHARED`
/// robust mutex in a `MAP_SHARED` page, the very case robust lists exist
/// for -- is queued on the shared futex, and a wake sent to the dead
/// thread's per-process table reaches nobody.
pub fn futex_for_shared_op(proc: &Process, uaddr: usize) -> Option<Arc<Futex>> {
    shared_futex_at(&proc.vmar(), uaddr).or_else(|| proc.linux().get_futex(uaddr))
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
extern crate std;

/// `SHARED_FUTEXES` is process-wide and cargo runs a crate's tests in
/// threads, so `clear_for_test` in one test wipes the table another is
/// counting. Every test that interns a word or clears the table takes this
/// first, in this file or elsewhere in the crate.
///
/// CI runs with `--test-threads=1` and therefore never sees it; on a
/// developer's machine it failed about one run in eight, in
/// `an_entry_is_reclaimed_once_nobody_holds_the_futex`, with a length of 2
/// where it wanted 1 -- which reads as a real regression in the interning
/// and is not one.
#[cfg(test)]
pub(crate) fn serialised_for_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that panics while holding this poisons it; the tests that
    // follow are not at fault, so step over the poison.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A one-page `MAP_SHARED | MAP_ANONYMOUS` object mapped read-write into
/// `vmar`, for the tests that need a word a shared futex can name. Returns the
/// object, the user address of the page and the frame's own address in the
/// host, which is where the test reads and writes the word.
///
/// The user address space of a hosted test starts at 0 and the libos page
/// table is a `MAP_FIXED` of the frame at the user address, so the page is
/// mapped over one this function allocates and leaks: anywhere else it would
/// land on whatever the host keeps there. The mapping must never be undone
/// for the same reason, so callers leak the VMAR's process as well.
#[cfg(test)]
pub(crate) fn map_shared_page_for_test(
    vmar: &Arc<VmAddressRegion>,
) -> (Arc<VmObject>, usize, usize) {
    let vmo = VmObject::new_paged(1);
    vmo.set_share_on_fork();
    vmo.commit(0, PAGE_SIZE).unwrap();
    let frame = kernel_hal::mem::phys_to_virt(vmo.committed_paddr(0).unwrap());
    let page = core::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();
    let user = unsafe { std::alloc::alloc_zeroed(page) } as usize;
    let rw = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    vmar.map_at(user - vmar.addr(), vmo.clone(), 0, PAGE_SIZE, rw)
        .unwrap();
    (vmo, user, frame)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::boxed::Box;

    fn serialised() -> self::std::sync::MutexGuard<'static, ()> {
        serialised_for_test()
    }

    fn leaked_word() -> &'static AtomicI32 {
        Box::leak(Box::new(AtomicI32::new(0)))
    }

    fn some_vmo() -> Arc<VmObject> {
        VmObject::new_paged(1)
    }

    #[test]
    fn the_same_key_gives_the_same_futex() {
        let _serialised = serialised();
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
        let _serialised = serialised();
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

        let _serialised = serialised();
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
        let _serialised = serialised();
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

    #[test]
    fn naming_a_shared_word_leaves_its_page_writable() {
        let _serialised = serialised();
        let vmar = VmAddressRegion::new_root();
        let (_vmo, user, frame) = map_shared_page_for_test(&vmar);
        let writable = |vmar: &Arc<VmAddressRegion>| {
            vmar.get_vaddr_flags(user)
                .unwrap()
                .contains(MMUFlags::WRITE)
        };
        assert!(writable(&vmar));
        unsafe {
            (*((frame + 0x24) as *const AtomicI32))
                .store(0x1234, core::sync::atomic::Ordering::SeqCst)
        };
        let futex = shared_futex_at(&vmar, user + 0x24).expect("a shared word");
        assert!(
            writable(&vmar),
            "a futex call took WRITE away from the caller's page: its next store \
             to the word faults"
        );
        assert_eq!(
            futex.load(),
            0x1234,
            "the futex must sit on the word asked for, not on the start of its page"
        );
        core::mem::forget(vmar);
    }
}
