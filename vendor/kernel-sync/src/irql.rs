//! Software interrupt-priority levels (IRQL), with a hardware TPR/CR8 raise
//! on x86_64 when the Local APIC is live.
//!
//! Three levels, the RTIC / Windows split applied to a general-purpose kernel:
//!
//! * [`Irql::Passive`] — ordinary thread context; every maskable IRQ may fire.
//! * [`Irql::Dispatch`] — kernel lock held. Device IRQs (vectors `0x20..0xF0`)
//!   are blocked; the LAPIC timer (`0xF1`) and the resched/TLB IPI (`0xF3`)
//!   still run, so a driver spinlock does not stretch the tick or a shootdown.
//! * [`Irql::High`] — everything maskable is off (`cli`, and CR8 at class 15).
//!
//! On anything that is not a live x86_64 Local APIC (other arches, early boot,
//! host tests) Dispatch and High both fall back to `cli`, so the existing
//! `push_off`/`pop_off` pairing tests keep their meaning.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::MAX_CORE_NUM;

/// Interrupt-priority level of this CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Irql {
    Passive = 0,
    Dispatch = 1,
    High = 2,
}

impl Irql {
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Irql::Dispatch,
            2 => Irql::High,
            _ => Irql::Passive,
        }
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// CR8 / TPR class that blocks device IRQs (`vector >> 4 <= 14`) and lets the
/// LAPIC timer and IPI (class 15, vectors `0xF0..0xFF`) through.
const CR8_DISPATCH: u64 = 0x0E;
/// Class 15: every maskable vector is at or below this.
const CR8_HIGH: u64 = 0x0F;

static CURRENT: [AtomicU8; MAX_CORE_NUM] = [const { AtomicU8::new(0) }; MAX_CORE_NUM];
static RAISES: AtomicU64 = AtomicU64::new(0);
static TPR_APPLIES: AtomicU64 = AtomicU64::new(0);
static DEFERRED_TICKS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn slot() -> usize {
    let id = crate::interrupt::current_cpu_id() as usize;
    if id < MAX_CORE_NUM {
        id
    } else {
        0
    }
}

/// Whether this CPU can mask by priority instead of by `RFLAGS.IF`.
///
/// True only on x86_64 once the Local APIC is globally enabled. Before that,
/// and on every other target, [`raise_irql`] falls back to `cli`.
#[inline]
pub fn priority_masking() -> bool {
    #[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        tpr_available()
    }
    #[cfg(not(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64"))))]
    {
        false
    }
}

#[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
fn tpr_available() -> bool {
    const IA32_APIC_BASE: u32 = 0x1B;
    const APIC_BASE_ENABLE: u64 = 1 << 11;
    let base = unsafe { x86_64::registers::model_specific::Msr::new(IA32_APIC_BASE).read() };
    base & APIC_BASE_ENABLE != 0
}

#[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
fn read_cr8() -> u64 {
    let v: u64;
    unsafe {
        core::arch::asm!(
            "mov {0}, cr8",
            out(reg) v,
            options(nomem, nostack, preserves_flags)
        );
    }
    v
}

#[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
fn write_cr8(v: u64) {
    unsafe {
        core::arch::asm!(
            "mov cr8, {0}",
            in(reg) v,
            options(nomem, nostack, preserves_flags)
        );
    }
}

fn apply(level: Irql) {
    #[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
    if tpr_available() {
        TPR_APPLIES.fetch_add(1, Ordering::Relaxed);
        match level {
            // CR8 only. Never `sti`: an IRQ handler (timer, IPI) enters with
            // IF already off, then takes kernel locks. `sti` here used to
            // re-enable maskable IRQs in the middle of that handler — nested
            // ticks, `memcpy` interrupted by `memcpy`, `rip=0` on a zeroed
            // return slot. IF is restored by `pop_off` only if the outermost
            // acquire sampled it on.
            Irql::Passive => write_cr8(0),
            Irql::Dispatch => write_cr8(CR8_DISPATCH),
            Irql::High => {
                write_cr8(CR8_HIGH);
                crate::interrupt::intr_off_raw();
            }
        }
        return;
    }
    // Fallback: any raise above Passive is a full `cli`.
    match level {
        Irql::Passive => crate::interrupt::intr_on_raw(),
        Irql::Dispatch | Irql::High => crate::interrupt::intr_off_raw(),
    }
}

/// Current software IRQL of this CPU.
#[inline]
pub fn current_irql() -> Irql {
    Irql::from_u8(CURRENT[slot()].load(Ordering::Relaxed))
}

/// Raise this CPU to at least `level`. Returns the previous level so the
/// caller can restore it with [`lower_irql`]. Nested raises keep the max.
pub fn raise_irql(level: Irql) -> Irql {
    let old = current_irql();
    if level > old {
        RAISES.fetch_add(1, Ordering::Relaxed);
        apply(level);
        CURRENT[slot()].store(level.as_u8(), Ordering::Relaxed);
    }
    old
}

/// Restore the level returned by a matching [`raise_irql`].
///
/// Leaving [`Irql::High`] on the TPR path re-enables IF: High is the only
/// level that cleared it. Dispatch/Passive never touch IF here; `push_off`
/// is what remembers whether the *caller* had interrupts on.
pub fn lower_irql(prev: Irql) {
    let now = current_irql();
    if prev < now {
        let leaving_high = now == Irql::High;
        apply(prev);
        CURRENT[slot()].store(prev.as_u8(), Ordering::Relaxed);
        if leaving_high && priority_masking() && prev != Irql::High {
            crate::interrupt::intr_on_raw();
        }
    }
}

/// `(raises that actually changed the level, TPR/CR8 writes, ticks deferred
/// because a lock was held)`.
pub fn irql_stats() -> (u64, u64, u64) {
    (
        RAISES.load(Ordering::Relaxed),
        TPR_APPLIES.load(Ordering::Relaxed),
        DEFERRED_TICKS.load(Ordering::Relaxed),
    )
}

/// Count one timer interrupt that found a kernel lock held and skipped the
/// work that would have taken another lock on this CPU.
pub fn note_deferred_tick() {
    DEFERRED_TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Host/unit-test helper: force the software level back to Passive and the
/// hardware mask back to "interrupts on". The per-CPU slot is what
/// `push_off` nests in; a test that raised IRQL and then panicked would
/// otherwise leave the next test on that slot deaf.
#[cfg(test)]
pub fn reset_irql_for_test() {
    CURRENT[slot()].store(Irql::Passive.as_u8(), Ordering::Relaxed);
    crate::interrupt::intr_on_raw();
}
