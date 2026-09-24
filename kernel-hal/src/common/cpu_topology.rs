//! The dense logical CPU id registry, shared by every architecture's SMP
//! bring-up.
//!
//! Hardware CPU identifiers are sparse: Local APIC ids leave gaps between
//! cores, threads and sockets (and are 32-bit wide in x2APIC mode), riscv hart
//! ids skip the harts a board reserves, and an MPIDR affinity repeats its Aff0
//! field in every cluster. None of them can index a per-CPU array, so each CPU
//! is handed a *dense logical id* (0..`MAX_CORE_NUM`, the boot CPU always 0)
//! the moment SMP bring-up reaches it, and everything above this layer — the
//! IPI queues, the per-CPU blocks, `cpu_online_mask`, the lock crate's
//! per-CPU depth counters — is indexed by that id alone.
//!
//! Each architecture used to keep its own copy of this bookkeeping and they
//! had drifted, in the direction that costs a machine rather than a core:
//!
//!  * **x86_64** kept a `LOGICAL_REGISTERED` bitmask beside its
//!    `logical -> apic` table, precisely so an id nobody had wired resolved to
//!    `None` rather than to APIC 0. Its own doc comment says why: falling back
//!    to 0 would aim another CPU's IPI at the BSP. **riscv and aarch64 had no
//!    such mask**, and their tables start at zero, so
//!    `logical_to_hart(never_registered)` was the boot hart and
//!    `logical_to_affinity(never_registered)` was the boot CPU — while the
//!    shootdown initiator waited for an acknowledgement from a CPU that was
//!    never signalled, which is the wait with no timeout in
//!    [`remote_flush_tlb_on`](super::ipi::remote_flush_tlb_on).
//!  * **x86_64** asserted that the ids it hands out fit the tables; riscv and
//!    aarch64 counted in an `AtomicU8` and stored through a `.get()` that
//!    silently dropped anything past the end — so CPU 64 got an id that
//!    passes every `< MAX_CORE_NUM` check *nowhere*, no reverse-map entry, and
//!    a `lock` forward-map entry pointing at a per-CPU slot it shares with
//!    somebody else.
//!  * **x86_64** could give an id back when the AP it was meant for turned out
//!    not to be startable. Nothing else could, and x86_64 only did it on one
//!    of the two paths that need it (see `unregister` below).
//!
//! One registry, one set of rules, and the rules are testable on the host —
//! which matters here more than usual, because none of this code runs in CI:
//! the emulator boots one or two cores and the bring-up paths that use it are
//! `target_os = "none"` to a CPU.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::config::MAX_CORE_NUM;

/// The `registered` bitmask is a `u64`, so it can only name 64 CPUs. Every
/// guard in this module is spelled `< MAX_CORE_NUM` rather than `< 64` so the
/// day that limit moves there is exactly one thing to widen, and this is it.
const _: () = assert!(
    MAX_CORE_NUM <= 64,
    "the registered mask is a u64: widen it before raising MAX_CORE_NUM past 64"
);

/// The dense logical id ↔ hardware id map for one machine.
///
/// A hardware id is whatever the architecture uses to address a CPU: a Local
/// APIC id on x86_64, a hart id on riscv, a packed MPIDR affinity on aarch64.
/// It is kept as a `u32` because an x2APIC id is that wide — truncating it to
/// a byte aliases two CPUs onto one entry and sends everything addressed to
/// one of them, TLB shootdowns included, to the other.
pub struct CpuTopology {
    /// Dense logical ids handed out so far, i.e. the next id to hand out.
    count: AtomicUsize,
    /// Which logical ids are actually wired. Needed as a separate bit because
    /// hardware id 0 is a perfectly good boot-CPU id, so 0 in the table below
    /// cannot mean "unset".
    registered: AtomicU64,
    /// logical id -> hardware id. Meaningful only where `registered` says so.
    hw_of_logical: [AtomicU32; MAX_CORE_NUM],
}

impl Default for CpuTopology {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuTopology {
    pub const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            registered: AtomicU64::new(0),
            hw_of_logical: [const { AtomicU32::new(0) }; MAX_CORE_NUM],
        }
    }

    /// Hand out the next dense logical id to the CPU whose hardware id is
    /// `hw_id`, or `None` when the table is full.
    ///
    /// `None` rather than a panic: this runs on the BSP in the middle of AP
    /// bring-up, where the caller's answer to "one core too many" is to stop
    /// launching APs and boot with the ones it has, not to take the machine
    /// down. It must also not *consume* an id when it refuses — a plain
    /// `fetch_add` past the limit leaves `count` climbing for every CPU the
    /// firmware reports, and `count` is what userspace reads as the CPU count
    /// through `/proc/cpuinfo` and `sched_getaffinity`.
    pub fn register(&self, hw_id: u32) -> Option<usize> {
        let logical = self
            .count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CORE_NUM).then_some(n + 1)
            })
            .ok()?;
        self.hw_of_logical[logical].store(hw_id, Ordering::Release);
        self.registered.fetch_or(1u64 << logical, Ordering::Release);
        Some(logical)
    }

    /// Give back the id [`register`](Self::register) just handed out, for a CPU
    /// that turns out not to be startable — no stack for it, or it never
    /// latched the trampoline slot it was told to.
    ///
    /// Returns whether the id was still the most recent one. Only the newest
    /// id can be given back: anything else would punch a hole in the dense
    /// numbering, and dense is the whole point — the ids index `[_;
    /// MAX_CORE_NUM]` tables and are counted, not searched. The registration
    /// bit is cleared either way, so a CPU that will never run stops inviting
    /// IPIs even when its id cannot be reclaimed.
    ///
    /// Leaving an id registered is not cosmetic. It inflates the CPU count
    /// userspace sees, and it leaves a `logical -> hardware` entry that says a
    /// core which never executed an instruction is reachable — so a shootdown
    /// addressed to it is delivered into the void while its initiator waits,
    /// without a timeout, for the acknowledgement.
    pub fn unregister(&self, logical: usize) -> bool {
        if logical >= MAX_CORE_NUM {
            return false;
        }
        self.registered
            .fetch_and(!(1u64 << logical), Ordering::Release);
        self.count
            .compare_exchange(logical + 1, logical, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Republish `logical`'s hardware id, for a CPU that can only now read its
    /// own id authoritatively. Returns the id this replaced, when the two
    /// disagreed, so the caller can say so.
    ///
    /// x86_64 needs this and the reason generalises: the ids known before a
    /// CPU runs are provisional. The BSP seeds the map from the ACPI MADT, and
    /// the AP's first self-registration happens while its LAPIC is still in
    /// xAPIC mode — INIT leaves every AP there whatever mode the BSP is in —
    /// so it can only see the low 8 bits. If either disagrees with the truth,
    /// every IPI to this CPU is addressed to a core that does not exist.
    pub fn confirm(&self, logical: usize, hw_id: u32) -> Option<u32> {
        if logical >= MAX_CORE_NUM {
            return None;
        }
        let was_registered = self.registered.load(Ordering::Acquire) & (1u64 << logical) != 0;
        let previous = self.hw_of_logical[logical].swap(hw_id, Ordering::AcqRel);
        self.registered.fetch_or(1u64 << logical, Ordering::Release);
        (was_registered && previous != hw_id).then_some(previous)
    }

    /// The hardware id to address `logical` with, or `None` when no CPU was
    /// ever given that id.
    ///
    /// Callers must **not** substitute a hardware id of their own for `None`.
    /// Zero in particular is the boot CPU on all three architectures, so
    /// "unknown" resolving to zero does not drop the message — it delivers it
    /// to the BSP, which then flushes a TLB nobody asked it about while the
    /// initiator waits on a CPU that heard nothing.
    pub fn hw_id(&self, logical: usize) -> Option<u32> {
        if logical >= MAX_CORE_NUM {
            return None;
        }
        (self.registered.load(Ordering::Acquire) & (1u64 << logical) != 0)
            .then(|| self.hw_of_logical[logical].load(Ordering::Acquire))
    }

    /// Whether `logical` names a CPU that was registered and not given back.
    pub fn is_registered(&self, logical: usize) -> bool {
        self.hw_id(logical).is_some()
    }

    /// How many dense logical ids have been handed out — the CPU count the
    /// rest of the kernel and userspace read.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Bitmask of the logical ids that are wired right now.
    pub fn registered_mask(&self) -> u64 {
        self.registered.load(Ordering::Acquire)
    }
}

// ─── Addressing a CPU once its hardware id is known ──────────────────────────

/// Number of CPU interfaces a GICv2 SGI target list can name.
///
/// `GICD_SGIR`'s CPUTargetList field is 8 bits wide, one per CPU *interface*,
/// which is the whole of what GICv2 can address — it has no affinity routing.
pub const GICV2_SGI_TARGETS: u32 = 8;

/// The `GICD_SGIR` CPU-target-list bit for the core whose packed MPIDR
/// affinity is `affinity`, or `None` when GICv2 cannot address that core.
///
/// The target list names CPU **interfaces**, and on the single-cluster systems
/// GICv2 exists on, interface `n` is the core with `Aff0 == n`. A dense logical
/// id is not that number: ids are handed out in the order cores *arrive* at
/// `register_logical_id`, and aarch64 fires every `CPU_ON` before waiting for
/// any of them, so the arrival order is the order the firmware happens to
/// schedule them in — not affinity order. Using the logical id as the target
/// bit therefore aims the IPI at whichever core got that Aff0, and a TLB
/// shootdown that reaches the wrong core is one the initiator waits for
/// forever: the core it asked never flushes, and the core it woke acknowledges
/// nothing on its behalf.
///
/// A core in another cluster (any of Aff1..Aff3 set) is refused rather than
/// having its Aff0 used: two clusters both have an Aff0 0, so the bit would
/// name the wrong core with nothing to distinguish them.
pub fn gicv2_sgi_target(affinity: u32) -> Option<u32> {
    if affinity >> 8 != 0 {
        return None; // not in the boot cluster: GICv2 has no way to say which
    }
    (affinity < GICV2_SGI_TARGETS).then(|| 1u32 << affinity)
}

/// The legacy-SBI hart mask word naming `hart` alone, or `None` when the mask
/// cannot reach it.
///
/// `sbi_rt::legacy::send_ipi` takes one `usize` of bits, so a hart numbered
/// past the word width is a hart this call cannot address. Hart ids are sparse
/// by definition — that is why dense logical ids exist at all — so this is
/// reachable on a real board, and `1 << hart` past the word width is not a
/// no-op: riscv masks the shift amount, so it would ring hart `hart % 64`.
pub fn sbi_hart_mask(hart: usize) -> Option<usize> {
    (hart < usize::BITS as usize).then(|| 1usize << hart)
}

/// How many slots a per-CPU table needs: one per CPU, **plus one**.
///
/// See [`percpu_slot`] for what the extra one is.
pub const PERCPU_SLOTS: usize = MAX_CORE_NUM + 1;

/// The slot a CPU that has no logical id lands in. It belongs to no CPU, so
/// whatever is written there is written to nobody.
pub const QUARANTINE_SLOT: usize = MAX_CORE_NUM;

const _: () = assert!(
    QUARANTINE_SLOT < PERCPU_SLOTS,
    "the quarantine slot must be inside the table it is an index into"
);

/// Which slot of a `[_; PERCPU_SLOTS]` table belongs to `cpu_id`.
///
/// A per-CPU table has to answer *something* for a CPU whose id resolved to
/// nothing (`lock`'s `NO_CPU`), because the caller — the per-CPU block, say —
/// hands out a reference and has nowhere to put a `None`. The answer must not
/// be slot 0. Slot 0 is the boot CPU's, and its per-CPU block holds, among
/// other things, *the thread currently running on it*: two CPUs sharing it
/// means two CPUs that each believe they are running that thread, and both of
/// them writing its quantum, its timer state and its callback depth.
///
/// So the table carries one extra slot that is nobody's, and an unknown CPU
/// scribbles there instead. It is still wrong for two unknown CPUs to share
/// it, but nothing correct is lost when they do — whereas sharing the boot
/// CPU's slot corrupts a CPU that was doing nothing wrong.
pub fn percpu_slot(cpu_id: usize) -> usize {
    if cpu_id < MAX_CORE_NUM {
        cpu_id
    } else {
        QUARANTINE_SLOT
    }
}

/// The bring-up registry is one of the two places a CPU id is decided (the
/// other is [`ipi`](super::ipi), which then trusts it to index with), and
/// neither has ever run in CI: the emulator boots one or two cores, and every
/// caller here is behind `target_os = "none"`. What these pin down is what the
/// three architectures used to disagree about, each disagreement being a way
/// for a shootdown to wait forever on a CPU that was never told anything.
#[cfg(test)]
mod topology_tests {
    use super::*;

    /// A fresh registry per test: unlike the IPI queues these are not global,
    /// which is the point of making the registry a value rather than a module
    /// of statics.
    fn topology() -> CpuTopology {
        CpuTopology::new()
    }

    #[test]
    fn the_boot_cpu_gets_logical_zero_and_the_ids_stay_dense() {
        let t = topology();
        assert_eq!(t.register(0x00), Some(0), "the boot CPU is logical 0");
        // Deliberately sparse hardware ids: this is what the map exists for.
        assert_eq!(t.register(0x04), Some(1));
        assert_eq!(t.register(0x20), Some(2));
        assert_eq!(t.count(), 3);
        assert_eq!(t.hw_id(0), Some(0x00));
        assert_eq!(t.hw_id(1), Some(0x04));
        assert_eq!(t.hw_id(2), Some(0x20));
    }

    #[test]
    fn an_id_nobody_was_given_is_unknown_rather_than_the_boot_cpu() {
        let t = topology();
        t.register(0x00);
        // The table under `hw_of_logical` reads zero here, and zero is a valid
        // boot-CPU hardware id on all three architectures. Reporting it would
        // aim this CPU's IPI at the BSP: it flushes a TLB nobody asked about,
        // the intended target hears nothing, and the initiator waits for its
        // acknowledgement in a loop that has no timeout.
        assert_eq!(t.hw_id(1), None);
        assert_eq!(t.hw_id(MAX_CORE_NUM - 1), None);
        assert!(!t.is_registered(1));
    }

    #[test]
    fn an_id_past_the_tables_is_refused_rather_than_shifted_with() {
        let t = topology();
        assert_eq!(t.hw_id(MAX_CORE_NUM), None);
        assert_eq!(t.hw_id(64), None);
        assert_eq!(t.hw_id(usize::MAX), None);
        // `1u64 << logical` is undefined past 63 and on x86 wraps to bit
        // `logical % 64`, so an unchecked id does not merely fail — it answers
        // for a different CPU.
        assert!(!t.is_registered(usize::MAX));
        assert_eq!(t.confirm(MAX_CORE_NUM, 0x99), None);
        assert!(!t.unregister(MAX_CORE_NUM));
    }

    #[test]
    fn one_cpu_too_many_is_refused_without_inflating_the_count() {
        let t = topology();
        for expected in 0..MAX_CORE_NUM {
            assert_eq!(t.register(expected as u32 + 1), Some(expected));
        }
        assert_eq!(t.count(), MAX_CORE_NUM);
        // Firmware that reports more CPUs than the tables hold must not leave
        // the count climbing: `count()` is what `/proc/cpuinfo` and
        // `sched_getaffinity` report, and a plain `fetch_add` that refuses
        // afterwards has already counted the CPU it refused.
        for _ in 0..8 {
            assert_eq!(t.register(0xdead), None);
        }
        assert_eq!(t.count(), MAX_CORE_NUM, "a refused CPU was still counted");
        assert_eq!(t.registered_mask().count_ones() as usize, MAX_CORE_NUM);
    }

    #[test]
    fn a_cpu_that_never_started_gives_its_id_back() {
        let t = topology();
        t.register(0x00);
        let logical = t.register(0x04).unwrap();
        assert_eq!(t.count(), 2);
        // The AP had its id before it was launched — that is the point, its
        // very first lock must resolve to the right per-CPU slot — so a launch
        // that fails has to undo it.
        assert!(t.unregister(logical));
        assert_eq!(t.count(), 1, "a CPU that never ran is still counted");
        assert_eq!(t.hw_id(logical), None, "a dead CPU is still addressable");
        // And the id is handed out again to the next CPU that can use it.
        assert_eq!(t.register(0x08), Some(logical));
        assert_eq!(t.hw_id(logical), Some(0x08));
    }

    #[test]
    fn only_the_newest_id_can_be_given_back_but_any_of_them_can_be_silenced() {
        let t = topology();
        t.register(0x00);
        t.register(0x04);
        t.register(0x20);
        assert_eq!(t.count(), 3);
        // Returning a middle id would leave a hole in numbering that the whole
        // kernel indexes with and counts, so the count stands.
        assert!(!t.unregister(1));
        assert_eq!(t.count(), 3);
        // The bit goes regardless: whatever the count says, a CPU that will
        // never run must stop being a legal IPI destination.
        assert_eq!(t.hw_id(1), None);
        assert_eq!(t.hw_id(2), Some(0x20), "an unrelated CPU was silenced too");
    }

    #[test]
    fn a_cpu_that_reads_its_own_id_late_corrects_the_provisional_one() {
        let t = topology();
        let logical = t.register(0x04).unwrap();
        // The id seeded from firmware (or read through the wrong LAPIC
        // interface) disagrees with what the CPU itself reports.
        assert_eq!(t.confirm(logical, 0x1004), Some(0x04));
        assert_eq!(
            t.hw_id(logical),
            Some(0x1004),
            "IPIs would still go to the id firmware guessed"
        );
        // A confirmation that agrees is not worth reporting.
        assert_eq!(t.confirm(logical, 0x1004), None);
    }

    #[test]
    fn a_confirmation_wires_an_id_that_was_never_registered() {
        let t = topology();
        // Nothing to disagree with, so nothing is reported — but the CPU is
        // addressable afterwards, which is what the caller asked for.
        assert_eq!(t.confirm(3, 0x0c), None);
        assert_eq!(t.hw_id(3), Some(0x0c));
    }

    #[test]
    fn a_wide_hardware_id_survives_the_table() {
        let t = topology();
        // x2APIC ids are full 32-bit values. Keeping them in a byte — which is
        // what the `lock` crate's direct table can hold — aliases two CPUs onto
        // one entry and sends everything addressed to one of them to the other.
        let logical = t.register(0xdead_beef).unwrap();
        assert_eq!(t.hw_id(logical), Some(0xdead_beef));
        let top = t.register(u32::MAX).unwrap();
        assert_eq!(t.hw_id(top), Some(u32::MAX));
    }

    #[test]
    fn the_registered_mask_names_exactly_the_cpus_that_are_addressable() {
        let t = topology();
        for hw in 0..5u32 {
            t.register(hw * 4);
        }
        assert_eq!(t.registered_mask(), 0b11111);
        t.unregister(4);
        assert_eq!(t.registered_mask(), 0b01111);
        for logical in 0..MAX_CORE_NUM {
            assert_eq!(
                t.is_registered(logical),
                t.registered_mask() & (1u64 << logical) != 0,
                "the mask and the lookup disagree about CPU {}",
                logical
            );
        }
    }

    #[test]
    fn the_last_id_the_tables_hold_is_a_usable_one() {
        let t = topology();
        for _ in 0..MAX_CORE_NUM - 1 {
            t.register(0);
        }
        let last = t.register(0x7f).unwrap();
        assert_eq!(last, MAX_CORE_NUM - 1);
        // `1u64 << 63` is the last bit that fits; one more and the mask, not
        // the bounds check, is what gives way.
        assert_eq!(t.hw_id(last), Some(0x7f));
        assert!(t.registered_mask() & (1u64 << last) != 0);
    }

    // ── addressing a CPU once its hardware id is known ─────────────────────

    #[test]
    fn the_gic_target_bit_is_the_cores_affinity_not_the_order_it_booted() {
        // Ids are handed out in arrival order and aarch64 fires every CPU_ON
        // before waiting for any of them, so this is the ordinary case, not a
        // contrived one: logical 1 is whichever core got there first.
        let t = topology();
        t.register(0x00); // the boot CPU, Aff0 = 0
        let logical = t.register(0x03).unwrap(); // Aff0 = 3, arrived second
        assert_eq!(logical, 1);
        let affinity = t.hw_id(logical).unwrap();
        assert_eq!(
            gicv2_sgi_target(affinity),
            Some(1 << 3),
            "the SGI would have gone to the core with Aff0 = 1"
        );
    }

    #[test]
    fn every_interface_the_gic_target_list_holds_gets_its_own_bit() {
        for aff0 in 0..GICV2_SGI_TARGETS {
            assert_eq!(gicv2_sgi_target(aff0), Some(1 << aff0));
        }
        // The field is 8 bits wide and that is all of GICv2's addressing.
        assert_eq!(gicv2_sgi_target(GICV2_SGI_TARGETS), None);
        assert_eq!(gicv2_sgi_target(63), None);
        assert_eq!(gicv2_sgi_target(u32::MAX), None);
    }

    #[test]
    fn a_core_outside_the_boot_cluster_has_no_gic_target_bit() {
        // Aff1 = 1, Aff0 = 2: a second cluster. Its Aff0 collides with the boot
        // cluster's core 2, and the target list has no field to tell them
        // apart, so naming the bit would aim the IPI at the wrong core.
        assert_eq!(gicv2_sgi_target(0x0000_0102), None);
        assert_eq!(gicv2_sgi_target(0x0001_0000), None);
        assert_eq!(gicv2_sgi_target(0x0100_0000), None);
    }

    #[test]
    fn a_hart_the_sbi_mask_cannot_reach_is_refused_rather_than_wrapped() {
        assert_eq!(sbi_hart_mask(0), Some(1));
        assert_eq!(sbi_hart_mask(63), Some(1usize << 63));
        // `1 << 64` on riscv masks the shift amount to 0, so the mask would
        // name hart 0 — the boot hart — instead of naming nothing.
        assert_eq!(sbi_hart_mask(usize::BITS as usize), None);
        assert_eq!(sbi_hart_mask(usize::MAX), None);
    }

    #[test]
    fn concurrent_bring_up_never_hands_the_same_id_to_two_cpus() {
        // Two APs registering at once share one `PercpuBlock`, one GS slot and
        // one scheduler slot if they get the same id — silent cross-CPU memory
        // corruption, and the reason x86_64's trampoline has a slot-consumed
        // handshake at all.
        use std::sync::Arc;
        static T: CpuTopology = CpuTopology::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let threads: Vec<_> = (0..8u32)
            .map(|hw| {
                let seen = Arc::clone(&seen);
                std::thread::spawn(move || {
                    if let Some(logical) = T.register(hw) {
                        seen.lock().unwrap().push(logical);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut ids = seen.lock().unwrap().clone();
        ids.sort_unstable();
        let unique = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            unique,
            "two CPUs were handed the same logical id"
        );
        assert_eq!(ids.len(), 8);
        assert_eq!(T.count(), 8);
    }
}

#[cfg(test)]
mod percpu_slot_tests {
    use super::*;

    #[test]
    fn every_real_cpu_keeps_its_own_slot() {
        for cpu in 0..MAX_CORE_NUM {
            assert_eq!(percpu_slot(cpu), cpu);
        }
    }

    #[test]
    fn a_cpu_with_no_logical_id_does_not_land_on_the_boot_cpu() {
        // `lock`'s NO_CPU, and anything else past the tables.
        for cpu in [MAX_CORE_NUM, MAX_CORE_NUM + 1, 255, usize::MAX] {
            let slot = percpu_slot(cpu);
            assert_ne!(slot, 0, "cpu {} took the boot CPU's slot", cpu);
            assert_eq!(slot, QUARANTINE_SLOT);
        }
    }

    #[test]
    fn the_quarantine_slot_belongs_to_no_cpu() {
        // It is inside the table by `const _: () = assert!` at its definition;
        // what a test has to pin is that no real CPU is ever given it.
        assert!((0..MAX_CORE_NUM).all(|cpu| percpu_slot(cpu) != QUARANTINE_SLOT));
    }
}
