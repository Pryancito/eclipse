//! Boot timeline, loader half: when rboot reached each of its progress marks.
//!
//! rboot already walks the progress bar from 0% to 51% before the kernel takes
//! over at 52% ([`rboot::progress`]), with a mark at the end of each thing it
//! does: reading the config, selecting the GOP mode, reading the kernel ELF off
//! the ESP, reading the initramfs (the largest file in the boot by a wide
//! margin), building the page tables, `ExitBootServices`, the jump. The kernel
//! times its own half of that bar; without this, everything before its first
//! instruction -- which includes reading hundreds of megabytes through a
//! firmware FAT driver -- was simply outside the clock.
//!
//! What is recorded is the **raw TSC**, nothing else: no calibration, no
//! division, two instructions per mark. rboot has no cheap clock of its own --
//! the only one the firmware offers is `BootServices::stall`, and calibrating
//! against it would mean *spending* ten or twenty milliseconds of the boot to
//! measure it -- so the numbers are carried to the kernel in [`BootInfo`] and
//! converted there, against a frequency the kernel has already checked against
//! the ACPI PM timer. That is both free and more accurate than anything rboot
//! could work out for itself.
//!
//! The absolute value of the first mark is worth as much as the deltas: the TSC
//! counts from processor reset, so it is roughly what the firmware spent before
//! rboot ran at all -- the one part of the boot neither rboot nor the kernel can
//! otherwise see. "Roughly" because a warm reset or a hypervisor need not start
//! the counter at zero, which is why the kernel prints it apart from the table
//! rather than as its first row.

use core::sync::atomic::{AtomicU64, Ordering};

/// Raw TSC at each mark rboot has reached; `0` means "not reached".
static AT_TSC: [AtomicU64; rboot::LOADER_MARKS] =
    [const { AtomicU64::new(0) }; rboot::LOADER_MARKS];

/// Record that rboot reached `progress`.
///
/// First write wins: a percentage is meant to be passed once, and where one is
/// passed twice (48% and 49% are, around `ExitBootServices`, to pin a hang on
/// the screen) the first arrival is the one that bounds the stretch before it.
pub fn mark(progress: u32) {
    let Some(slot) = AT_TSC.get(progress as usize) else {
        return;
    };
    // SAFETY: `rdtsc` is unprivileged and always available on x86_64.
    let now = unsafe { core::arch::x86_64::_rdtsc() };
    // `max(1)`: a stamp of exactly 0 is this table's "not reached", and under an
    // emulator the counter really can read 0 at the first mark.
    let _ = slot.compare_exchange(0, now.max(1), Ordering::Relaxed, Ordering::Relaxed);
}

/// The marks reached so far, for [`BootInfo::loader_marks`].
pub fn taken() -> [u64; rboot::LOADER_MARKS] {
    let mut out = [0u64; rboot::LOADER_MARKS];
    for (slot, out) in AT_TSC.iter().zip(out.iter_mut()) {
        *out = slot.load(Ordering::Relaxed);
    }
    out
}
