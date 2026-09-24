//! Who is this CPU, and how deep is it in kernel locks.
//!
//! Every lock this crate hands out brackets its critical section with
//! `push_off`/`pop_off`, and that pair keeps its interrupt-disable depth in a
//! **per-CPU slot indexed by the dense logical cpu id**. So the answer to
//! "which CPU am I?" is not a diagnostic detail: it picks the slot. Two CPUs
//! that answer the same number share one counter, and from then on one of them
//! re-enables interrupts inside the other's critical section (or trips
//! `pop_off`'s underflow).
//!
//! The two things that decide that are here, as plain state machines with no
//! architecture in them, because the modules that use them
//! ([`crate::interrupt`] and every mutex in this crate) are
//! `cfg(target_os = "none")` from top to bottom and never compile on a host:
//!
//! * [`LogicalIdMap`] — the hardware id (Local APIC ID, hart id, MPIDR
//!   affinity) to dense logical id map, and the cross-check for a logical id
//!   published somewhere corruptible (x86 GS, aarch64 `TPIDR_EL1`).
//! * [`IrqDepth`] — the matched `push_off`/`pop_off` counter.
//!
//! The reverse direction — "which hart is logical N?", used to aim an IPI —
//! lives in `kernel-hal`'s `common::cpu_topology` and refuses to answer for an
//! id nobody registered. This is the same question from the other side, and it
//! used to answer **0** — the boot CPU — for a hardware id nobody had ever
//! registered.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::MAX_CORE_NUM;

const _: () = assert!(
    MAX_CORE_NUM <= 64,
    "the registered/ap-boot masks are u64; widen them before raising MAX_CORE_NUM"
);

/// "This hardware CPU has no logical id."
///
/// Deliberately out of range for every per-CPU array in the kernel, so the
/// answer cannot be used as an index by accident: [`crate::mycpu`] refuses it
/// by name and [`crate::lock_depth`] takes its conservative branch.
///
/// The alternative — answering 0 — is not a smaller mistake, it is a larger
/// one: 0 is the boot CPU on all three architectures, so an unknown CPU would
/// silently nest its interrupt-disable depth in the BSP's slot.
pub const NO_CPU: u8 = u8::MAX;

/// Hardware CPU id <-> dense logical cpu id, plus the two overrides the x86
/// bring-up path needs.
///
/// One instance per machine (`crate::interrupt::LOGICAL_IDS`). Every method is
/// lock-free and allocation-free: `cpu_id()` runs on every kernel lock acquire
/// and release, and on the fault and panic paths, where taking a lock to
/// answer "who am I?" would re-enter this very question.
pub struct LogicalIdMap {
    /// Hardware id (the ones that fit in a byte) -> logical id. The fast path,
    /// and the only one that is a single load.
    byte_to_logical: [AtomicU8; 256],
    /// Logical id -> hardware id. Authoritative, and the only table that can
    /// represent a hardware id wider than a byte (x2APIC ids are 32 bits, and
    /// hart ids are sparse by definition).
    hw_of_logical: [AtomicU32; MAX_CORE_NUM],
    /// Which entries of `hw_of_logical` were ever written. Needed because
    /// every table above starts at zero and hardware id 0 is a legal boot-CPU
    /// id, so 0 cannot mean "unset".
    registered: AtomicU64,
    /// Hardware id of each CPU currently inside an [`Self::ap_boot_enter`]
    /// window, indexed by the logical id it claims.
    ap_boot_hw: [AtomicU32; MAX_CORE_NUM],
    /// Which logical ids are inside such a window. Zero in the steady state,
    /// which is what lets the caller skip the (expensive) hardware id read.
    ap_boot_active: AtomicU64,
    /// Last logical id read out of a corruptible publisher that names no
    /// registered CPU, and how many times that happened.
    bogus_id: AtomicU32,
    bogus_count: AtomicU32,
}

impl Default for LogicalIdMap {
    fn default() -> Self {
        Self::new()
    }
}

impl LogicalIdMap {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const fn new() -> Self {
        Self {
            byte_to_logical: [const { AtomicU8::new(0) }; 256],
            hw_of_logical: [const { AtomicU32::new(0) }; MAX_CORE_NUM],
            registered: AtomicU64::new(0),
            ap_boot_hw: [const { AtomicU32::new(u32::MAX) }; MAX_CORE_NUM],
            ap_boot_active: AtomicU64::new(0),
            bogus_id: AtomicU32::new(u32::MAX),
            bogus_count: AtomicU32::new(0),
        }
    }

    /// Record that hardware CPU `hw` was given dense logical id `logical`.
    ///
    /// Returns `false` for a logical id that no per-CPU array can hold, so the
    /// caller can say so rather than write an entry that indexes nothing.
    pub fn register(&self, hw: u32, logical: u8) -> bool {
        let idx = logical as usize;
        if idx >= MAX_CORE_NUM {
            return false;
        }
        self.hw_of_logical[idx].store(hw, Ordering::Release);
        self.registered.fetch_or(1u64 << idx, Ordering::Release);
        // The byte table is a cache of the line above, not a second source of
        // truth: a hardware id that does not fit stays out of it and is found
        // by the scan instead. Writing `hw as u8` here is what used to alias
        // hart 256 onto hart 0 — the boot hart.
        if hw < 256 {
            self.byte_to_logical[hw as usize].store(logical, Ordering::Release);
        }
        true
    }

    /// Whether any CPU has been registered yet.
    ///
    /// Before the first one, the boot CPU is the only thing running and is
    /// logical 0 by definition; that window is the one place a fallback to 0
    /// is right rather than dangerous.
    pub fn any_registered(&self) -> bool {
        self.registered.load(Ordering::Acquire) != 0
    }

    /// Resolve a hardware CPU id to its dense logical id, or [`NO_CPU`].
    pub fn resolve(&self, hw: u32) -> u8 {
        let registered = self.registered.load(Ordering::Acquire);
        if registered == 0 {
            // Pre-SMP: only the boot CPU runs, and it is logical 0.
            return 0;
        }
        if hw < 256 {
            let logical = self.byte_to_logical[hw as usize].load(Ordering::Acquire);
            if self.confirms(logical, hw, registered) {
                return logical;
            }
            // Fall through: the byte slot may simply never have been written,
            // in which case it reads 0 and would name the boot CPU.
        }
        self.scan(hw, registered)
    }

    /// Whether `logical` is a registered id that really does name `hw`.
    fn confirms(&self, logical: u8, hw: u32, registered: u64) -> bool {
        let idx = logical as usize;
        idx < MAX_CORE_NUM
            && registered & (1u64 << idx) != 0
            && self.hw_of_logical[idx].load(Ordering::Acquire) == hw
    }

    /// Walk the registered set looking for `hw`. At most `MAX_CORE_NUM`
    /// iterations, and only reached for a hardware id too wide for the byte
    /// table or one the byte table does not vouch for.
    fn scan(&self, hw: u32, mut registered: u64) -> u8 {
        while registered != 0 {
            let logical = registered.trailing_zeros() as usize;
            registered &= registered - 1;
            if self.hw_of_logical[logical].load(Ordering::Acquire) == hw {
                return logical as u8;
            }
        }
        NO_CPU
    }

    /// The hardware id registered for `logical`, if any.
    pub fn hw_of(&self, logical: u8) -> Option<u32> {
        let idx = logical as usize;
        if idx >= MAX_CORE_NUM || self.registered.load(Ordering::Acquire) & (1u64 << idx) == 0 {
            return None;
        }
        Some(self.hw_of_logical[idx].load(Ordering::Acquire))
    }

    /// Whether a logical id read out of a *corruptible publisher* — x86 `GS`,
    /// aarch64 `TPIDR_EL1` — may be believed.
    ///
    /// Both are the fast path (one register read, no APIC/MPIDR access), and
    /// both can be made to lie: a `swapgs` imbalance on a fault path, or a
    /// wild write into the per-CPU area. A 6-vCPU guest reported `panic
    /// cpu=48`, and 48 is in range, so the bounds check waved it through —
    /// which is worse than a panic, because `push_off`/`pop_off` then nest
    /// their depth on a foreign slot.
    ///
    /// The registered set is authoritative and costs one relaxed load of a
    /// line that is read-only in the steady state.
    pub fn accepts_published(&self, logical: u8) -> bool {
        let registered = self.registered.load(Ordering::Relaxed);
        // `registered == 0` is the pre-SMP window, where the publisher is all
        // we have.
        registered == 0
            || ((logical as usize) < MAX_CORE_NUM && registered & (1u64 << logical) != 0)
    }

    /// Record a logical id that [`Self::accepts_published`] rejected. No
    /// printing from here: every console writer takes a lock and would
    /// re-enter the question this answers.
    pub fn note_bogus(&self, logical: u8) {
        self.bogus_id.store(logical as u32, Ordering::Relaxed);
        self.bogus_count.fetch_add(1, Ordering::Relaxed);
    }

    /// `(last rejected id, how many)`. Count 0 means the publisher has always
    /// agreed with the registered set.
    pub fn bogus_events(&self) -> (u32, u32) {
        (
            self.bogus_id.load(Ordering::Relaxed),
            self.bogus_count.load(Ordering::Relaxed),
        )
    }

    /// Open a window in which hardware CPU `hw` answers `logical`, for the
    /// stretch of AP bring-up that runs before the per-CPU publisher is set up.
    ///
    /// Keyed by the logical id — unique per AP by construction — and looked up
    /// by hardware id, so two APs booting at once cannot clobber each other's
    /// window even if their hardware ids are too wide for the byte table.
    pub fn ap_boot_enter(&self, logical: u8, hw: u32) -> bool {
        let idx = logical as usize;
        if idx >= MAX_CORE_NUM {
            return false;
        }
        self.ap_boot_hw[idx].store(hw, Ordering::Release);
        self.ap_boot_active.fetch_or(1u64 << idx, Ordering::Release);
        true
    }

    /// Close the window opened by [`Self::ap_boot_enter`].
    pub fn ap_boot_leave(&self, logical: u8) {
        let idx = logical as usize;
        if idx >= MAX_CORE_NUM {
            return;
        }
        self.ap_boot_active
            .fetch_and(!(1u64 << idx), Ordering::Release);
        self.ap_boot_hw[idx].store(u32::MAX, Ordering::Release);
    }

    /// Whether any window is open. One relaxed load, which is what keeps the
    /// steady-state `cpu_id()` free of a hardware id read.
    pub fn ap_boot_any(&self) -> bool {
        self.ap_boot_active.load(Ordering::Relaxed) != 0
    }

    /// The logical id hardware CPU `hw` claims, if it is inside a window.
    pub fn ap_boot_logical(&self, hw: u32) -> Option<u8> {
        let mut active = self.ap_boot_active.load(Ordering::Acquire);
        while active != 0 {
            let logical = active.trailing_zeros() as usize;
            active &= active - 1;
            if self.ap_boot_hw[logical].load(Ordering::Acquire) == hw {
                return Some(logical as u8);
            }
        }
        None
    }
}

/// Why a [`IrqDepth::pop`] could not be honoured.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PopError {
    /// Interrupts were already on. Either somebody enabled them inside a
    /// critical section, or this `pop` is running against a **different CPU's**
    /// slot than the `push` that matched it.
    InterruptsEnabled,
    /// More pops than pushes on this slot. Same two causes, plus a guard
    /// whose destructor ran twice.
    Underflow,
}

/// The matched interrupt-disable depth of one CPU.
///
/// `push`/`pop` are like `intr_off()`/`intr_on()` except that they nest: it
/// takes two pops to undo two pushes, and if interrupts were off to begin
/// with, a push/pop pair leaves them off.
///
/// `noff == 0` means the CPU holds **no** kernel lock, which is the
/// precondition the panic-recovery path tests before it dares run recovery
/// code that itself takes locks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IrqDepth {
    /// Depth of `push` nesting.
    pub noff: i32,
    /// Were interrupts enabled before the outermost `push`?
    pub interrupt_enable: bool,
}

impl IrqDepth {
    pub const fn new() -> Self {
        Self {
            noff: 0,
            interrupt_enable: false,
        }
    }

    /// Enter one level. `irq_was_on` is the interrupt state sampled *before*
    /// disabling, and is remembered only by the outermost level — an inner
    /// push always samples "off", and letting it overwrite the saved state
    /// would lose the machine's interrupts for good.
    pub fn push(&mut self, irq_was_on: bool) {
        if self.noff == 0 {
            self.interrupt_enable = irq_was_on;
        }
        self.noff += 1;
    }

    /// Leave one level. `Ok(true)` means the caller must now re-enable
    /// interrupts, which it has to do *after* dropping its borrow of this
    /// slot, since enabling may take an interrupt immediately.
    pub fn pop(&mut self, irq_on_now: bool) -> Result<bool, PopError> {
        if irq_on_now {
            return Err(PopError::InterruptsEnabled);
        }
        if self.noff < 1 {
            return Err(PopError::Underflow);
        }
        self.noff -= 1;
        Ok(self.noff == 0 && self.interrupt_enable)
    }
}

#[cfg(test)]
mod logical_id_tests {
    use super::*;

    #[test]
    fn pre_smp_window_answers_the_boot_cpu() {
        let map = LogicalIdMap::new();
        assert!(!map.any_registered());
        // Nothing registered: the boot CPU is the only thing running, whatever
        // hardware id it reports.
        assert_eq!(map.resolve(0), 0);
        assert_eq!(map.resolve(7), 0);
        assert_eq!(map.resolve(4096), 0);
    }

    #[test]
    fn an_unregistered_hardware_id_is_not_the_boot_cpu() {
        let map = LogicalIdMap::new();
        map.register(0, 0);
        map.register(6, 1);
        assert_eq!(map.resolve(0), 0);
        assert_eq!(map.resolve(6), 1);
        // The byte table reads 0 for every slot nobody wrote, and 0 is the
        // boot CPU. Answering it would put this CPU's lock depth in the BSP's
        // slot.
        assert_eq!(map.resolve(4), NO_CPU);
        assert_eq!(map.resolve(255), NO_CPU);
    }

    #[test]
    fn a_hardware_id_too_wide_for_a_byte_keeps_its_own_identity() {
        let map = LogicalIdMap::new();
        map.register(0, 0);
        // x2APIC ids are 32 bits and hart ids are sparse: 256 does not fit the
        // byte table, and truncating it to `hw as u8` lands on hart 0.
        map.register(256, 1);
        map.register(0x8000_0001, 2);
        assert_eq!(map.resolve(256), 1);
        assert_eq!(map.resolve(0x8000_0001), 2);
        // ...and must not have disturbed the boot CPU's own entry.
        assert_eq!(map.resolve(0), 0);
    }

    #[test]
    fn two_wide_ids_that_share_a_low_byte_do_not_alias() {
        let map = LogicalIdMap::new();
        map.register(0, 0);
        map.register(0x100, 1);
        map.register(0x200, 2);
        assert_eq!(map.resolve(0x100), 1);
        assert_eq!(map.resolve(0x200), 2);
        assert_eq!(map.resolve(0x300), NO_CPU);
    }

    #[test]
    fn a_logical_id_past_the_per_cpu_arrays_is_refused() {
        let map = LogicalIdMap::new();
        assert!(map.register(9, 3));
        assert!(!map.register(11, MAX_CORE_NUM as u8));
        assert!(!map.register(12, NO_CPU));
        // The refused ones left nothing behind.
        assert_eq!(map.resolve(11), NO_CPU);
        assert_eq!(map.resolve(12), NO_CPU);
        assert_eq!(map.hw_of(3), Some(9));
    }

    #[test]
    fn hw_of_answers_none_rather_than_zero_for_an_unregistered_id() {
        let map = LogicalIdMap::new();
        map.register(0, 0);
        assert_eq!(map.hw_of(0), Some(0));
        assert_eq!(map.hw_of(1), None);
        assert_eq!(map.hw_of(NO_CPU), None);
    }

    #[test]
    fn a_published_id_is_believed_only_while_it_names_a_registered_cpu() {
        let map = LogicalIdMap::new();
        // Pre-SMP the publisher is all we have.
        assert!(map.accepts_published(0));
        assert!(map.accepts_published(48));
        map.register(0, 0);
        map.register(1, 1);
        assert!(map.accepts_published(0));
        assert!(map.accepts_published(1));
        // In range but not a CPU: exactly the `panic cpu=48` on a 6-vCPU guest.
        assert!(!map.accepts_published(48));
        assert!(!map.accepts_published(NO_CPU));
    }

    #[test]
    fn rejected_published_ids_are_counted_for_the_panic_report() {
        let map = LogicalIdMap::new();
        assert_eq!(map.bogus_events(), (u32::MAX, 0));
        map.note_bogus(48);
        map.note_bogus(13);
        assert_eq!(map.bogus_events(), (13, 2));
    }

    #[test]
    fn the_ap_boot_window_names_only_the_cpu_that_opened_it() {
        let map = LogicalIdMap::new();
        map.register(0, 0);
        assert!(!map.ap_boot_any());
        assert!(map.ap_boot_enter(3, 0x1234));
        assert!(map.ap_boot_any());
        assert_eq!(map.ap_boot_logical(0x1234), Some(3));
        // Any other CPU asking gets nothing and falls back to the real map.
        assert_eq!(map.ap_boot_logical(0), None);
        map.ap_boot_leave(3);
        assert!(!map.ap_boot_any());
        assert_eq!(map.ap_boot_logical(0x1234), None);
    }

    #[test]
    fn two_aps_booting_at_once_keep_separate_windows() {
        let map = LogicalIdMap::new();
        map.ap_boot_enter(1, 0x100);
        map.ap_boot_enter(2, 0x200);
        assert_eq!(map.ap_boot_logical(0x100), Some(1));
        assert_eq!(map.ap_boot_logical(0x200), Some(2));
        map.ap_boot_leave(1);
        // Closing one window must not close the other.
        assert_eq!(map.ap_boot_logical(0x100), None);
        assert_eq!(map.ap_boot_logical(0x200), Some(2));
    }

    #[test]
    fn an_ap_boot_window_for_an_impossible_logical_id_is_refused() {
        let map = LogicalIdMap::new();
        assert!(!map.ap_boot_enter(MAX_CORE_NUM as u8, 0x500));
        assert!(!map.ap_boot_any());
        assert_eq!(map.ap_boot_logical(0x500), None);
        // ...and closing it is not a shift past the width of the mask.
        map.ap_boot_leave(MAX_CORE_NUM as u8);
        map.ap_boot_leave(NO_CPU);
    }

    #[test]
    fn the_whole_table_can_be_filled_and_read_back() {
        let map = LogicalIdMap::new();
        // Deliberately sparse and partly wider than a byte, like real x2APIC.
        for logical in 0..MAX_CORE_NUM {
            assert!(map.register((logical as u32) * 5 + 1000, logical as u8));
        }
        for logical in 0..MAX_CORE_NUM {
            assert_eq!(map.resolve((logical as u32) * 5 + 1000), logical as u8);
        }
        assert_eq!(map.resolve(1000 + 5 * MAX_CORE_NUM as u32), NO_CPU);
    }

    #[test]
    fn re_registering_a_hardware_id_moves_it_rather_than_duplicating_it() {
        let map = LogicalIdMap::new();
        map.register(0, 0);
        map.register(4, 1);
        assert_eq!(map.resolve(4), 1);
        // An AP that latched a different slot on a retry: the byte cache must
        // not keep vouching for the old logical id.
        map.register(4, 2);
        assert_eq!(map.resolve(4), 2);
    }
}

#[cfg(test)]
mod irq_depth_tests {
    use super::*;

    #[test]
    fn a_matched_pair_restores_interrupts() {
        let mut d = IrqDepth::new();
        d.push(true);
        assert_eq!(d.noff, 1);
        assert_eq!(d.pop(false), Ok(true));
        assert_eq!(d.noff, 0);
    }

    #[test]
    fn interrupts_off_on_entry_stay_off_on_exit() {
        let mut d = IrqDepth::new();
        d.push(false);
        assert_eq!(d.pop(false), Ok(false));
    }

    #[test]
    fn only_the_outermost_level_records_the_interrupt_state() {
        let mut d = IrqDepth::new();
        d.push(true);
        // Every inner push samples "off", because the outer one disabled them.
        d.push(false);
        d.push(false);
        assert_eq!(d.noff, 3);
        assert_eq!(d.pop(false), Ok(false));
        assert_eq!(d.pop(false), Ok(false));
        // Letting an inner push overwrite the saved state would lose the
        // machine's interrupts here.
        assert_eq!(d.pop(false), Ok(true));
    }

    #[test]
    fn an_inner_level_never_re_enables_interrupts() {
        let mut d = IrqDepth::new();
        d.push(true);
        d.push(false);
        assert_eq!(d.pop(false), Ok(false));
        assert_eq!(d.noff, 1);
    }

    #[test]
    fn popping_with_interrupts_on_is_reported_apart_from_underflow() {
        let mut d = IrqDepth::new();
        d.push(true);
        // Somebody enabled interrupts inside the critical section, or this pop
        // is running against a different CPU's slot than its push.
        assert_eq!(d.pop(true), Err(PopError::InterruptsEnabled));
        // ...and the failed pop did not consume a level.
        assert_eq!(d.noff, 1);
    }

    #[test]
    fn popping_a_slot_nobody_pushed_is_an_underflow() {
        let mut d = IrqDepth::new();
        assert_eq!(d.pop(false), Err(PopError::Underflow));
        assert_eq!(d.noff, 0);
    }

    #[test]
    fn one_push_and_two_pops_underflows_rather_than_going_negative() {
        let mut d = IrqDepth::new();
        d.push(true);
        assert_eq!(d.pop(false), Ok(true));
        // A guard whose destructor ran twice, or a `downgrade` that both drops
        // its old guard and hands out a new one.
        assert_eq!(d.pop(false), Err(PopError::Underflow));
        assert_eq!(d.noff, 0);
    }

    #[test]
    fn a_fresh_slot_holds_no_lock() {
        let d = IrqDepth::new();
        assert_eq!(d.noff, 0);
        assert_eq!(d, IrqDepth::default());
    }
}
