//! A lock that provides data access to either one writer or many readers.

use core::{
    cell::UnsafeCell,
    fmt,
    hint::spin_loop,
    // marker::PhantomData,
    mem,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::interrupt::{pop_off, push_off};

pub struct RwLock<T: ?Sized> {
    lock: AtomicUsize,
    data: UnsafeCell<T>,
}

/// One unit of the reader count, which occupies every bit above the two
/// flags. `pub(crate)` so the tests in `tests.rs` can speak the protocol.
pub(crate) const READER: usize = 1 << 2;
pub(crate) const UPGRADED: usize = 1 << 1;
pub(crate) const WRITER: usize = 1;

/// The one spin discipline this file's four waiting loops share.
///
/// Every mutex flavour in this crate brackets its acquire with `push_off`, so
/// a waiter that got here from a caller who already had interrupts off spins
/// with them **still** off — and is therefore deaf to the TLB-shootdown IPI.
/// A peer performing a shootdown spin-waits for that ack while holding the
/// VMAR lock, so a silent waiter wedges it, and every CPU queued behind that
/// lock wedges with it. `deadlock::pump`'s own doc names the shape: *"Only
/// this crate's own ticket lock pumped; a CPU parked in any other IRQs-off
/// spinner was an ack black hole."* This file held four of them.
///
/// `ticket.rs` and `spin.rs` each answer this with the same two lines —
/// drain our own queue every 512 spins, report the stuck call site once at
/// the threshold — so the answer goes in one place and the loops read from
/// it. Two of the four had neither half: `upgradeable_read` and `upgrade`
/// spun in complete silence, so a wedge there produced no banner either, and
/// the machine stopped with nothing on the console.
///
/// A pump is one relaxed load when no hook is installed, and one queue-pointer
/// compare when the queue is empty, so the cadence costs a contended acquire
/// nothing measurable.
struct SpinDiscipline {
    spins: u64,
    caller: &'static core::panic::Location<'static>,
}

impl SpinDiscipline {
    #[inline]
    fn new(caller: &'static core::panic::Location<'static>) -> Self {
        Self { spins: 0, caller }
    }

    /// One turn of a waiting loop.
    #[inline]
    fn spin(&mut self) {
        spin_loop();
        self.spins += 1;
        if self.spins & 511 == 0 {
            crate::deadlock::spin_pump();
        }
        if self.spins == crate::deadlock::deadlock_spins() {
            crate::deadlock::report_deadlock(self.caller.file(), self.caller.line());
        }
    }
}

/// A guard that provides immutable data access.
///
/// When the guard falls out of scope it will decrement the read count,
/// potentially releasing the lock.
pub struct RwLockReadGuard<'a, T: 'a + ?Sized> {
    lock: &'a AtomicUsize,
    data: &'a T,
}

/// A guard that provides mutable data access.
///
/// When the guard falls out of scope it will release the lock.
pub struct RwLockWriteGuard<'a, T: 'a + ?Sized> {
    // phantom: PhantomData<R>,
    inner: &'a RwLock<T>,
    data: &'a mut T,
}

/// A guard that provides immutable data access but can be upgraded to [`RwLockWriteGuard`].
///
/// No writers or other upgradeable guards can exist while this is in scope. New reader
/// creation is prevented (to alleviate writer starvation) but there may be existing readers
/// when the lock is acquired.
///
/// When the guard falls out of scope it will release the lock.
pub struct RwLockUpgradableGuard<'a, T: 'a + ?Sized> {
    // phantom: PhantomData<R>,
    inner: &'a RwLock<T>,
    data: &'a T,
}

// Same unsafe impls as `std::sync::RwLock`
unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    /// Creates a new spinlock wrapping the supplied data.
    ///
    /// May be used statically:
    ///
    /// ```
    /// use spin;
    ///
    /// static RW_LOCK: spin::RwLock<()> = spin::RwLock::new(());
    ///
    /// fn demo() {
    ///     let lock = RW_LOCK.read();
    ///     // do something with lock
    ///     drop(lock);
    /// }
    /// ```
    #[inline]
    pub const fn new(data: T) -> Self {
        RwLock {
            // phantom: PhantomData,
            lock: AtomicUsize::new(0),
            data: UnsafeCell::new(data),
        }
    }

    /// Consumes this `RwLock`eturning the underlying data.
    #[inline]
    pub fn into_inner(self) -> T {
        // We know statically that there are no outstanding references to
        // `self` so there's no need to lock.
        let RwLock { data, .. } = self;
        data.into_inner()
    }
    /// Returns a mutable pointer to the underying data.
    ///
    /// This is mostly meant to be used for applications which require manual unlocking, but where
    /// storing both the lock and the pointer to the inner data gets inefficient.
    ///
    /// While this is safe, writing to the data is undefined behavior unless the current thread has
    /// acquired a write lock, and reading requires either a read or write lock.
    ///
    /// # Example
    /// ```
    /// let lock = spin::RwLock::new(42);
    ///
    /// unsafe {
    ///     core::mem::forget(lock.write());
    ///     
    ///     assert_eq!(lock.as_mut_ptr().read(), 42);
    ///     lock.as_mut_ptr().write(58);
    ///
    ///     lock.force_write_unlock();
    /// }
    ///
    /// assert_eq!(*lock.read(), 58);
    ///
    /// ```
    #[inline(always)]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.data.get()
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Locks this rwlock with shared read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// The calling thread will be blocked until there are no more writers which
    /// hold the lock. There may be other readers currently inside the lock when
    /// this method returns. This method does not provide any guarantees with
    /// respect to the ordering of whether contentious readers or writers will
    /// acquire the lock first.
    ///
    /// Returns an RAII guard which will release this thread's shared access
    /// once it is dropped.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    /// {
    ///     let mut data = mylock.read();
    ///     // The lock is now locked and the data can be read
    ///     println!("{}", *data);
    ///     // The lock is dropped
    /// }
    /// ```
    #[track_caller]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        let mut spin = SpinDiscipline::new(core::panic::Location::caller());
        loop {
            match self.try_read() {
                Some(guard) => return guard,
                // Many seconds of continuous spinning is almost certainly a
                // deadlock (e.g. a writer wedged while holding this lock);
                // `SpinDiscipline` self-reports once and keeps spinning, and
                // drains our own shootdown queue on the way.
                None => spin.spin(),
            }
        }
    }

    /// Lock this rwlock with exclusive write access, blocking the current
    /// thread until it can be acquired.
    ///
    /// This function will not return while other writers or other readers
    /// currently have access to the lock.
    ///
    /// Returns an RAII guard which will drop the write access of this rwlock
    /// when dropped.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    /// {
    ///     let mut data = mylock.write();
    ///     // The lock is now locked and the data can be written
    ///     *data += 1;
    ///     // The lock is dropped
    /// }
    /// ```
    #[track_caller]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        let mut spin = SpinDiscipline::new(core::panic::Location::caller());
        loop {
            match self.try_write_internal(false) {
                Some(guard) => return guard,
                None => spin.spin(),
            }
        }
    }

    /// Obtain a readable lock guard that can later be upgraded to a writable lock guard.
    /// Upgrades can be done through the [`RwLockUpgradableGuard::upgrade`](RwLockUpgradableGuard::upgrade) method.
    #[inline]
    #[track_caller]
    pub fn upgradeable_read(&self) -> RwLockUpgradableGuard<'_, T> {
        let mut spin = SpinDiscipline::new(core::panic::Location::caller());
        loop {
            match self.try_upgradeable_read() {
                Some(guard) => return guard,
                None => spin.spin(),
            }
        }
    }

    /// Attempt to acquire this lock with shared read access.
    ///
    /// This function will never block and will return immediately if `read`
    /// would otherwise succeed. Returns `Some` of an RAII guard which will
    /// release the shared access of this thread when dropped, or `None` if the
    /// access could not be granted. This method does not provide any
    /// guarantees with respect to the ordering of whether contentious readers
    /// or writers will acquire the lock first.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    /// {
    ///     match mylock.try_read() {
    ///         Some(data) => {
    ///             // The lock is now locked and the data can be read
    ///             println!("{}", *data);
    ///             // The lock is dropped
    ///         },
    ///         None => (), // no cigar
    ///     };
    /// }
    /// ```
    #[inline]
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        push_off();
        let value = self.lock.fetch_add(READER, Ordering::Acquire);

        // We check the UPGRADED bit here so that new readers are prevented when an UPGRADED lock is held.
        // This helps reduce writer starvation.
        if value & (WRITER | UPGRADED) != 0 {
            // Lock is taken, undo.
            self.lock.fetch_sub(READER, Ordering::Release);
            pop_off();
            None
        } else {
            Some(RwLockReadGuard {
                lock: &self.lock,
                data: unsafe { &*self.data.get() },
            })
        }
    }

    /// Return the number of readers that currently hold the lock (including upgradable readers).
    ///
    /// # Safety
    ///
    /// This function provides no synchronization guarantees and so its result should be considered 'out of date'
    /// the instant it is called. Do not use it for synchronization purposes. However, it may be useful as a heuristic.
    pub fn reader_count(&self) -> usize {
        let state = self.lock.load(Ordering::Relaxed);
        state / READER + (state & UPGRADED) / UPGRADED
    }

    /// Return the number of writers that currently hold the lock.
    ///
    /// Because [`RwLock`] guarantees exclusive mutable access, this function may only return either `0` or `1`.
    ///
    /// # Safety
    ///
    /// This function provides no synchronization guarantees and so its result should be considered 'out of date'
    /// the instant it is called. Do not use it for synchronization purposes. However, it may be useful as a heuristic.
    pub fn writer_count(&self) -> usize {
        (self.lock.load(Ordering::Relaxed) & WRITER) / WRITER
    }

    /// Force decrement the reader count.
    ///
    /// # Safety
    ///
    /// This is *extremely* unsafe if there are outstanding `RwLockReadGuard`s
    /// live, or if called more times than `read` has been called, but can be
    /// useful in FFI contexts where the caller doesn't know how to deal with
    /// RAII. The underlying atomic operation uses `Ordering::Release`.
    #[inline]
    pub unsafe fn force_read_decrement(&self) {
        debug_assert!(self.lock.load(Ordering::Relaxed) & !WRITER > 0);
        self.lock.fetch_sub(READER, Ordering::Release);
    }

    /// Force unlock exclusive write access.
    ///
    /// # Safety
    ///
    /// This is *extremely* unsafe if there are outstanding `RwLockWriteGuard`s
    /// live, or if called when there are current readers, but can be useful in
    /// FFI contexts where the caller doesn't know how to deal with RAII. The
    /// underlying atomic operation uses `Ordering::Release`.
    #[inline]
    pub unsafe fn force_write_unlock(&self) {
        debug_assert_eq!(self.lock.load(Ordering::Relaxed) & !(WRITER | UPGRADED), 0);
        self.lock.fetch_and(!(WRITER | UPGRADED), Ordering::Release);
    }

    #[inline(always)]
    fn try_write_internal(&self, strong: bool) -> Option<RwLockWriteGuard<'_, T>> {
        push_off();
        if compare_exchange(
            &self.lock,
            0,
            WRITER,
            Ordering::Acquire,
            Ordering::Relaxed,
            strong,
        )
        .is_ok()
        {
            Some(RwLockWriteGuard {
                // phantom: PhantomData,
                inner: self,
                data: unsafe { &mut *self.data.get() },
            })
        } else {
            pop_off();
            None
        }
    }

    /// Attempt to lock this rwlock with exclusive write access.
    ///
    /// This function does not ever block, and it will return `None` if a call
    /// to `write` would otherwise block. If successful, an RAII guard is
    /// returned.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    /// {
    ///     match mylock.try_write() {
    ///         Some(mut data) => {
    ///             // The lock is now locked and the data can be written
    ///             *data += 1;
    ///             // The lock is implicitly dropped
    ///         },
    ///         None => (), // no cigar
    ///     };
    /// }
    /// ```
    #[inline]
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.try_write_internal(true)
    }

    /// Tries to obtain an upgradeable lock guard.
    #[inline]
    pub fn try_upgradeable_read(&self) -> Option<RwLockUpgradableGuard<'_, T>> {
        push_off();
        if self.lock.fetch_or(UPGRADED, Ordering::Acquire) & (WRITER | UPGRADED) == 0 {
            Some(RwLockUpgradableGuard {
                // phantom: PhantomData,
                inner: self,
                data: unsafe { &*self.data.get() },
            })
        } else {
            // We can't unflip the UPGRADED bit back just yet as there is another upgradeable or write lock.
            // When they unlock, they will clear the bit.
            pop_off();
            None
        }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `RwLock` mutably, no actual locking needs to
    /// take place -- the mutable borrow statically guarantees no locks exist.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut lock = spin::RwLock::new(0);
    /// *lock.get_mut() = 10;
    /// assert_eq!(*lock.read(), 10);
    /// ```
    pub fn get_mut(&mut self) -> &mut T {
        // We know statically that there are no other references to `self`, so
        // there's no need to lock the inner lock.
        unsafe { &mut *self.data.get() }
    }

    /// The raw lock word, for the tests that speak the bit protocol.
    #[cfg(test)]
    pub(crate) fn raw_state(&self) -> usize {
        self.lock.load(Ordering::SeqCst)
    }

    /// Add a reader count by hand, as `try_read` does *before* it looks at the
    /// flags — the half-finished state another CPU is in while we hold the
    /// lock. Paired with [`Self::take_reader_bit_back`], which is the line
    /// `try_read` runs next when it finds the lock taken.
    #[cfg(test)]
    pub(crate) fn add_reader_bit_as_try_read_does(&self) {
        self.lock.fetch_add(READER, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn take_reader_bit_back(&self) {
        self.lock.fetch_sub(READER, Ordering::AcqRel);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_read() {
            Some(guard) => write!(f, "RwLock {{ data: ")
                .and_then(|()| (&*guard).fmt(f))
                .and_then(|()| write!(f, "}}")),
            None => write!(f, "RwLock {{ <locked> }}"),
        }
    }
}

impl<T: ?Sized + Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(data: T) -> Self {
        Self::new(data)
    }
}

impl<'rwlock, T: ?Sized> RwLockReadGuard<'rwlock, T> {
    /// Leak the lock guard, yielding a reference to the underlying data.
    ///
    /// Note that this function will permanently lock the original lock for all but reading locks.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    ///
    /// let data: &i32 = spin::RwLockReadGuard::leak(mylock.read());
    ///
    /// assert_eq!(*data, 0);
    /// ```
    #[inline]
    pub fn leak(this: Self) -> &'rwlock T {
        pop_off();
        // `let Self { data, .. } = this` does NOT consume `this`: `data` is a
        // shared reference, which is `Copy`, so the pattern copies it out and
        // leaves the guard standing — to be dropped at the end of this
        // function. That made `leak` the opposite of what it says: the read
        // lock was RELEASED, and the `&'rwlock T` handed back outlived it, so
        // a later `write()` produced an `&mut T` aliasing it. And the
        // destructor's `pop_off` ran on top of the one above, taking a level
        // that belongs to whatever this CPU is really holding.
        let data = this.data as *const T;
        mem::forget(this);
        // SAFETY: the read count stays raised for good, so no writer can ever
        // be handed this data. That is what leaking the guard means.
        unsafe { &*data }
    }
}

impl<'rwlock, T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'rwlock, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'rwlock, T: ?Sized + fmt::Display> fmt::Display for RwLockReadGuard<'rwlock, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<'rwlock, T: ?Sized> RwLockUpgradableGuard<'rwlock, T> {
    /// Upgrades an upgradeable lock guard to a writable lock guard.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    ///
    /// let upgradeable = mylock.upgradeable_read(); // Readable, but not yet writable
    /// let writable = upgradeable.upgrade();
    /// ```
    #[inline]
    #[track_caller]
    pub fn upgrade(mut self) -> RwLockWriteGuard<'rwlock, T> {
        let mut spin = SpinDiscipline::new(core::panic::Location::caller());
        loop {
            self = match self.try_upgrade_internal(false) {
                Ok(guard) => return guard,
                Err(e) => e,
            };
            spin.spin();
        }
    }
}

impl<'rwlock, T: ?Sized> RwLockUpgradableGuard<'rwlock, T> {
    #[inline(always)]
    fn try_upgrade_internal(self, strong: bool) -> Result<RwLockWriteGuard<'rwlock, T>, Self> {
        if compare_exchange(
            &self.inner.lock,
            UPGRADED,
            WRITER,
            Ordering::Acquire,
            Ordering::Relaxed,
            strong,
        )
        .is_ok()
        {
            let inner = self.inner;

            // Forget the old guard so its destructor doesn't run (before mutably aliasing data below)
            mem::forget(self);

            // Upgrade successful
            Ok(RwLockWriteGuard {
                // phantom: PhantomData,
                inner,
                data: unsafe { &mut *inner.data.get() },
            })
        } else {
            Err(self)
        }
    }

    /// Tries to upgrade an upgradeable lock guard to a writable lock guard.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    /// let upgradeable = mylock.upgradeable_read(); // Readable, but not yet writable
    ///
    /// match upgradeable.try_upgrade() {
    ///     Ok(writable) => /* upgrade successful - use writable lock guard */ (),
    ///     Err(upgradeable) => /* upgrade unsuccessful */ (),
    /// };
    /// ```
    #[inline]
    pub fn try_upgrade(self) -> Result<RwLockWriteGuard<'rwlock, T>, Self> {
        self.try_upgrade_internal(true)
    }

    #[inline]
    /// Downgrades the upgradeable lock guard to a readable, shared lock guard. Cannot fail and is guaranteed not to spin.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(1);
    ///
    /// let upgradeable = mylock.upgradeable_read();
    /// assert!(mylock.try_read().is_none());
    /// assert_eq!(*upgradeable, 1);
    ///
    /// let readable = upgradeable.downgrade(); // This is guaranteed not to spin
    /// assert!(mylock.try_read().is_some());
    /// assert_eq!(*readable, 1);
    /// ```
    pub fn downgrade(self) -> RwLockReadGuard<'rwlock, T> {
        // Reserve the read guard for ourselves
        self.inner.lock.fetch_add(READER, Ordering::Acquire);

        let inner = self.inner;

        // Clear the UPGRADED bit by hand and FORGET the old guard, rather than
        // dropping it. In upstream `spin` the destructor does nothing else, so
        // dropping it here is free; in this fork it also calls `pop_off`, and
        // the read guard handed out below will call `pop_off` again when IT is
        // dropped. One `push_off`, two `pop_off`s: the lock is still held, but
        // this CPU has already re-enabled interrupts inside the critical
        // section — and the second release underflows somebody else's slot.
        inner.lock.fetch_sub(UPGRADED, Ordering::AcqRel);
        mem::forget(self);

        RwLockReadGuard {
            lock: &inner.lock,
            data: unsafe { &*inner.data.get() },
        }
    }

    /// Leak the lock guard, yielding a reference to the underlying data.
    ///
    /// Note that this function will permanently lock the original lock.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    ///
    /// let data: &i32 = spin::RwLockUpgradableGuard::leak(mylock.upgradeable_read());
    ///
    /// assert_eq!(*data, 0);
    /// ```
    #[inline]
    pub fn leak(this: Self) -> &'rwlock T {
        // The lock stays held for the rest of the machine's life; this CPU's
        // interrupt-disable level must not, since nobody is left to release
        // it and a CPU with interrupts off forever is deaf to the
        // TLB-shootdown IPI a peer is spin-waiting on. The read and write
        // guards' `leak` both do this; this one did not.
        pop_off();
        // Same as `RwLockReadGuard::leak`: the pattern copies the reference
        // out and leaves the guard to be dropped, so this neither leaked the
        // lock nor kept the level straight.
        let data = this.data as *const T;
        mem::forget(this);
        // SAFETY: the UPGRADED bit stays set for good, so no writer can ever
        // be handed this data.
        unsafe { &*data }
    }
}

impl<'rwlock, T: ?Sized + fmt::Debug> fmt::Debug for RwLockUpgradableGuard<'rwlock, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'rwlock, T: ?Sized + fmt::Display> fmt::Display for RwLockUpgradableGuard<'rwlock, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<'rwlock, T: ?Sized> RwLockWriteGuard<'rwlock, T> {
    /// Downgrades the writable lock guard to a readable, shared lock guard. Cannot fail and is guaranteed not to spin.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    ///
    /// let mut writable = mylock.write();
    /// *writable = 1;
    ///
    /// let readable = writable.downgrade(); // This is guaranteed not to spin
    /// # let readable_2 = mylock.try_read().unwrap();
    /// assert_eq!(*readable, 1);
    /// ```
    #[inline]
    pub fn downgrade(self) -> RwLockReadGuard<'rwlock, T> {
        // Reserve the read guard for ourselves
        self.inner.lock.fetch_add(READER, Ordering::Acquire);

        let inner = self.inner;

        // Release WRITER (and UPGRADED, which an upgrade attempt may have set
        // while we held it) by hand and FORGET the old guard — see
        // `RwLockUpgradableGuard::downgrade` for why dropping it is wrong
        // here: its destructor calls `pop_off`, and so will the read guard
        // below, leaving this CPU with interrupts back on inside a critical
        // section it still holds.
        inner
            .lock
            .fetch_and(!(WRITER | UPGRADED), Ordering::Release);
        mem::forget(self);

        RwLockReadGuard {
            lock: &inner.lock,
            data: unsafe { &*inner.data.get() },
        }
    }

    /// Downgrades the writable lock guard to an upgradable, shared lock guard. Cannot fail and is guaranteed not to spin.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    ///
    /// let mut writable = mylock.write();
    /// *writable = 1;
    ///
    /// let readable = writable.downgrade_to_upgradeable(); // This is guaranteed not to spin
    /// assert_eq!(*readable, 1);
    /// ```
    #[inline]
    pub fn downgrade_to_upgradeable(self) -> RwLockUpgradableGuard<'rwlock, T> {
        debug_assert_eq!(
            self.inner.lock.load(Ordering::Acquire) & (WRITER | UPGRADED),
            WRITER
        );

        // Take UPGRADED first and give WRITER back second, both with
        // read-modify-write ops.
        //
        // This was one blind `store(UPGRADED)`, which is the whole word: it
        // wiped the reader count in the bits above the two flags. Holding
        // WRITER does not mean the count is zero. `try_read` adds its READER
        // *before* it looks at the flags, and takes it back only on the next
        // line, so at any instant a lock held for writing can read
        // `WRITER | n*READER` with `n` readers in that window. The store ate
        // those additions; their `fetch_sub` then underflowed the word to
        // `0xffff_ffff_ffff_fffe`, and once this guard dropped, to
        // `…fffc` — WRITER clear, UPGRADED clear, and a reader count of 2^62.
        // `try_write` compare-exchanges from **0**, so from that moment no
        // writer can ever take this lock again and no reader can either. A
        // kernel `RwLock` wedged for good, with interrupts off.
        //
        // The two sibling downgrades never had this: both already use
        // `fetch_add`/`fetch_and`, which leave the other bits alone. Nor does
        // the lock ever look free in between here — after the `fetch_or` it
        // reads `WRITER | UPGRADED`, which refuses readers, writers and
        // upgradeable readers alike.
        self.inner.lock.fetch_or(UPGRADED, Ordering::Relaxed);
        self.inner.lock.fetch_and(!WRITER, Ordering::Release);

        let inner = self.inner;

        // Dropping self removes the UPGRADED bit
        mem::forget(self);

        RwLockUpgradableGuard {
            // phantom: PhantomData,
            inner,
            data: unsafe { &*inner.data.get() },
        }
    }

    /// Leak the lock guard, yielding a mutable reference to the underlying data.
    ///
    /// Note that this function will permanently lock the original lock.
    ///
    /// ```
    /// let mylock = spin::RwLock::new(0);
    ///
    /// let data: &mut i32 = spin::RwLockWriteGuard::leak(mylock.write());
    ///
    /// *data = 1;
    /// assert_eq!(*data, 1);
    /// ```
    #[inline]
    pub fn leak(this: Self) -> &'rwlock mut T {
        pop_off();
        let data = this.data as *mut _; // Keep it in pointer form temporarily to avoid double-aliasing
        core::mem::forget(this);
        unsafe { &mut *data }
    }
}

impl<'rwlock, T: ?Sized + fmt::Debug> fmt::Debug for RwLockWriteGuard<'rwlock, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'rwlock, T: ?Sized + fmt::Display> fmt::Display for RwLockWriteGuard<'rwlock, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<'rwlock, T: ?Sized> Deref for RwLockReadGuard<'rwlock, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<'rwlock, T: ?Sized> Deref for RwLockUpgradableGuard<'rwlock, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<'rwlock, T: ?Sized> Deref for RwLockWriteGuard<'rwlock, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<'rwlock, T: ?Sized> DerefMut for RwLockWriteGuard<'rwlock, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.data
    }
}

impl<'rwlock, T: ?Sized> Drop for RwLockReadGuard<'rwlock, T> {
    fn drop(&mut self) {
        debug_assert!(self.lock.load(Ordering::Relaxed) & !(WRITER | UPGRADED) > 0);
        self.lock.fetch_sub(READER, Ordering::Release);
        pop_off();
    }
}

impl<'rwlock, T: ?Sized> Drop for RwLockUpgradableGuard<'rwlock, T> {
    fn drop(&mut self) {
        debug_assert_eq!(
            self.inner.lock.load(Ordering::Relaxed) & (WRITER | UPGRADED),
            UPGRADED
        );
        self.inner.lock.fetch_sub(UPGRADED, Ordering::AcqRel);
        pop_off();
    }
}

impl<'rwlock, T: ?Sized> Drop for RwLockWriteGuard<'rwlock, T> {
    fn drop(&mut self) {
        debug_assert_eq!(self.inner.lock.load(Ordering::Relaxed) & WRITER, WRITER);

        // Writer is responsible for clearing both WRITER and UPGRADED bits.
        // The UPGRADED bit may be set if an upgradeable lock attempts an upgrade while this lock is held.
        self.inner
            .lock
            .fetch_and(!(WRITER | UPGRADED), Ordering::Release);
        pop_off();
    }
}

#[inline(always)]
fn compare_exchange(
    atomic: &AtomicUsize,
    current: usize,
    new: usize,
    success: Ordering,
    failure: Ordering,
    strong: bool,
) -> Result<usize, usize> {
    if strong {
        atomic.compare_exchange(current, new, success, failure)
    } else {
        atomic.compare_exchange_weak(current, new, success, failure)
    }
}

/// The spin discipline itself, driven directly.
///
/// The four loops that use it are exercised in `tests.rs`, where a second
/// thread is a second CPU and the waiting is real. These pin the two numbers
/// those tests cannot see from outside: how often a waiter drains its own
/// shootdown queue, and that the stuck call site is named once and not on
/// every turn afterwards.
#[cfg(test)]
mod spin_discipline_tests {
    use super::*;
    use core::sync::atomic::AtomicU32;

    /// These install process-wide hooks, so they take the shared hook lock.
    fn hook_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::deadlock::hook_test_lock()
    }

    static PUMPS: AtomicU32 = AtomicU32::new(0);
    static REPORTS: AtomicU32 = AtomicU32::new(0);

    fn count_pump() {
        PUMPS.fetch_add(1, Ordering::SeqCst);
    }

    fn count_report(_file: &'static str, _line: u32) {
        REPORTS.fetch_add(1, Ordering::SeqCst);
    }

    fn armed(threshold: u64) -> std::sync::MutexGuard<'static, ()> {
        let guard = hook_lock();
        PUMPS.store(0, Ordering::SeqCst);
        REPORTS.store(0, Ordering::SeqCst);
        crate::deadlock::set_spin_pump(count_pump);
        crate::deadlock::set_deadlock_hook(count_report);
        crate::deadlock::set_deadlock_spins(threshold);
        guard
    }

    fn here() -> &'static core::panic::Location<'static> {
        core::panic::Location::caller()
    }

    #[test]
    fn a_waiter_drains_its_own_queue_every_five_hundred_and_twelve_turns() {
        let _g = armed(0);
        let mut spin = SpinDiscipline::new(here());
        for _ in 0..511 {
            spin.spin();
        }
        assert_eq!(
            PUMPS.load(Ordering::SeqCst),
            0,
            "the cadence is coarse on purpose: a pump per turn would put the \
             shootdown queue on the hot path of every contended acquire"
        );
        spin.spin();
        assert_eq!(PUMPS.load(Ordering::SeqCst), 1);
        for _ in 0..512 {
            spin.spin();
        }
        assert_eq!(PUMPS.load(Ordering::SeqCst), 2);
        crate::deadlock::set_deadlock_spins(0);
    }

    #[test]
    fn a_wedged_waiter_names_its_call_site_once_and_keeps_spinning() {
        let _g = armed(1_000);
        let mut spin = SpinDiscipline::new(here());
        for _ in 0..999 {
            spin.spin();
        }
        assert_eq!(REPORTS.load(Ordering::SeqCst), 0);
        spin.spin();
        assert_eq!(REPORTS.load(Ordering::SeqCst), 1, "the wedge went unnamed");
        // The hook paints a banner somewhere lock-free; repeating it every
        // turn from then on would bury the machine under its own report.
        for _ in 0..5_000 {
            spin.spin();
        }
        assert_eq!(REPORTS.load(Ordering::SeqCst), 1);
        // …and it never stops waiting: a lock that is merely very contended
        // must still be acquired once it frees up.
        assert!(PUMPS.load(Ordering::SeqCst) > 1);
        crate::deadlock::set_deadlock_spins(0);
    }
}

