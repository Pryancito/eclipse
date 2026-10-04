//! GPU-independent survival channel for the console-GPU GSP-boot wedge.
//!
//! The console GPU wedges the CPU on a posted BAR1/fabric write ~1/3 of the
//! time during the SEC2 HS-resume window. When it does, nothing survives to
//! tell us *where*: the `/proc` report needs the machine alive to finish, and
//! the framebuffer is the very GPU we just wedged. With no serial port on this
//! box we need a breadcrumb that outlives a full CPU hang and a cold reboot and
//! never touches the GPU/PCIe.
//!
//! The RTC/CMOS NVRAM (I/O ports 0x70 index / 0x71 data) is exactly that: a
//! few battery-backed bytes reachable with two `out`/`in` instructions,
//! completely independent of the GPU, that keep their value across a hang and a
//! power cycle. We record a coarse **milestone** byte and a fine rolling
//! **narration counter** (bumped on every RM `nv_printf` line) as the boot
//! advances. After a wedge, `cat /proc/gpusurvive` on the next (healthy) boot
//! reads them back, so a single reboot tells us the exact operation the
//! previous attempt died on — e.g. milestone `STARTCPU_PRE` with narration
//! count N means it hung on the STARTCPU store after N RM narration lines.
//!
//! Bytes it *would* use: 0x40 magic, 0x41 milestone, 0x42 narration counter.
//!
//! WRITES ARE NOW DISABLED. On real hardware (ASUS PRIME X299-A II / AMI BIOS)
//! those bytes turned out to be INSIDE the firmware's checksummed NVRAM region
//! — not above it as the classic PC map suggested — so every write broke the
//! BIOS settings checksum and the next boot halted at POST with "Please enter
//! setup to recover BIOS setting / Press F1", reverting settings to defaults.
//! The breadcrumb was already superseded by the live console trace, so
//! `cmos_write` is a no-op; the GPU bring-up must never scribble on CMOS.
//! Reads are harmless and kept, so `/proc/gpusurvive` still compiles (it now
//! just reports "no breadcrumb").

use core::sync::atomic::{AtomicBool, Ordering};

const CMOS_MAGIC_OFF: u8 = 0x40;
const CMOS_MILESTONE_OFF: u8 = 0x41;
const CMOS_NARR_OFF: u8 = 0x42;
/// Distinguishes "Eclipse wrote this" from random battery-backed garbage.
const MAGIC: u8 = 0xEC;

/// Coarse milestones written to CMOS as the console-GPU GSP boot advances. The
/// value that survives a wedge names the last operation reached.
pub mod milestone {
    pub const NONE: u8 = 0x00;
    /// gsp_boot_run entered for the console GPU.
    pub const BOOT_ENTER: u8 = 0x10;
    /// PBUS PRI error retired; about to call into the vendored kgspInitRm.
    pub const INITRM_CALL: u8 = 0x20;
    /// os_boundary is about to issue the posted STARTCPU store (the wedge point).
    pub const STARTCPU_PRE: u8 = 0x40;
    /// The posted STARTCPU store returned (fabric did NOT wedge on it).
    pub const STARTCPU_POST: u8 = 0x50;
    /// PDISP restore ran after the SEC2 window (only if a quiesce was armed).
    pub const PDISP_RESTORE: u8 = 0x60;
    /// kgspInitRm returned (OK or a clean NV_STATUS error — not a wedge).
    pub const INITRM_RETURN: u8 = 0x70;
    /// bringup_step14: RM API controls stage reached.
    pub const CONTROLS: u8 = 0x80;
    /// bringup_step14: gpuStatePreInit/Init/Load stage reached.
    pub const STATE_LOAD: u8 = 0x90;
    /// bringup_step14: copy-engine data-movement stage reached.
    pub const CE_MOVE: u8 = 0xA0;
    /// Full console bring-up chain completed.
    pub const COMPLETE: u8 = 0xFF;
}

/// Human label for a milestone byte (for the /proc report).
pub fn milestone_label(m: u8) -> &'static str {
    match m {
        milestone::NONE => "none (no prior attempt recorded)",
        milestone::BOOT_ENTER => "BOOT_ENTER (console gsp_boot_run entered)",
        milestone::INITRM_CALL => "INITRM_CALL (PBUS cleared, about to call kgspInitRm)",
        milestone::STARTCPU_PRE => "STARTCPU_PRE (WEDGED ON the posted STARTCPU store)",
        milestone::STARTCPU_POST => "STARTCPU_POST (STARTCPU store completed; wedged later)",
        milestone::PDISP_RESTORE => "PDISP_RESTORE (past the SEC2 window)",
        milestone::INITRM_RETURN => "INITRM_RETURN (kgspInitRm returned)",
        milestone::CONTROLS => "CONTROLS (RM API controls stage)",
        milestone::STATE_LOAD => "STATE_LOAD (gpuState*Init/Load stage)",
        milestone::CE_MOVE => "CE_MOVE (copy-engine data movement stage)",
        milestone::COMPLETE => "COMPLETE (full chain finished)",
        _ => "unknown",
    }
}

// `cfg(test)` reads the breadcrumb through `test_cmos` instead (see
// `read_breadcrumb`), so on a host test build these three compile but nobody
// calls them; they are the real path and must keep compiling.
#[cfg(target_arch = "x86_64")]
#[inline]
#[cfg_attr(test, allow(dead_code))]
unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val,
        options(nomem, nostack, preserves_flags));
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[cfg_attr(test, allow(dead_code))]
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    core::arch::asm!("in al, dx", out("al") val, in("dx") port,
        options(nomem, nostack, preserves_flags));
    val
}

/// Read one CMOS byte. Bit 7 of the index port disables NMI for the duration of
/// the access (the standard idiom); we re-select register 0x0D (read-only,
/// harmless) with NMI re-enabled afterwards so we never leave NMI masked.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(test, allow(dead_code))]
unsafe fn cmos_read(idx: u8) -> u8 {
    outb(0x70, 0x80 | (idx & 0x7f));
    let v = inb(0x71);
    outb(0x70, 0x0d);
    let _ = inb(0x71);
    v
}

/// DISABLED. Writing these extended-CMOS bytes (0x40-0x42) corrupted the BIOS
/// settings checksum on real hardware (ASUS PRIME X299-A II / AMI BIOS): the
/// next boot halted at POST with "Please enter setup to recover BIOS setting --
/// Press F1 to Run SETUP" and the firmware reverted to defaults. Contrary to
/// the classic PC CMOS map, this firmware's checksummed NVRAM region DOES cover
/// 0x40-0x42, so any write there breaks the checksum. The survival breadcrumb
/// was already superseded by the live console trace, so the write is simply a
/// no-op now -- the GPU bring-up must never touch CMOS. Reads stay (harmless;
/// they only toggle NMI and never alter settings), so callers keep compiling
/// and `/proc/gpusurvive` just reports "no breadcrumb".
#[cfg(target_arch = "x86_64")]
unsafe fn cmos_write(_idx: u8, _val: u8) {}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn cmos_read(_idx: u8) -> u8 {
    0
}
#[cfg(not(target_arch = "x86_64"))]
unsafe fn cmos_write(_idx: u8, _val: u8) {}

/// Record a coarse milestone (and stamp the magic so a later read knows the
/// bytes are ours). Safe to call from anywhere, including the STARTCPU bracket
/// with interrupts off — it is two port writes and touches no lock, no GPU.
pub fn checkpoint(m: u8) {
    unsafe {
        cmos_write(CMOS_MAGIC_OFF, MAGIC);
        cmos_write(CMOS_MILESTONE_OFF, m);
    }
}

/// DISABLED, along with the write it fed.
///
/// It read the counter and wrote it back one higher, on **every** RM
/// `nv_printf` line. `cmos_write` is a deliberate no-op now (writing
/// 0x40-0x42 broke the firmware's NVRAM checksum on the bring-up box), so the
/// read had no consumer left -- and it was not free: four port accesses to
/// 0x70/0x71 per line, with NMI masked for the duration, times the hundreds of
/// lines the RM narrates per GSP boot. It also put a privileged instruction on
/// the RM's whole narration path, which is why nothing about that path could
/// be tested. Kept as a no-op so its callers still read as the boot they
/// describe.
pub fn narration_tick() {}

/// Zero the narration counter at the start of a fresh attempt.
pub fn reset_narration() {
    unsafe { cmos_write(CMOS_NARR_OFF, 0) };
}

// --- Console-GPU MSI accounting (shared drivers <-> os_boundary) ---------
// The console GSP boot brings the GPU's MSI delivery online (drivers side); the
// STARTCPU bracket (os_boundary) logs the state right before the posted store,
// so a wedge's frozen screen shows whether MSI was actually online and how many
// fired — the datum that scrolls off the top otherwise.
use core::sync::atomic::AtomicUsize;
static MSI_ONLINE_VECTOR: AtomicUsize = AtomicUsize::new(usize::MAX);
static MSI_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Mark the GPU's MSI delivery online for the boot (vector), zeroing the count.
pub fn msi_set_online(vector: usize) {
    MSI_COUNT.store(0, Ordering::Relaxed);
    MSI_ONLINE_VECTOR.store(vector, Ordering::Relaxed);
}
/// Mark MSI delivery offline (boot done / never came online).
pub fn msi_offline() {
    MSI_ONLINE_VECTOR.store(usize::MAX, Ordering::Relaxed);
}
/// One MSI serviced (called from the ISR closure). Returns the new count.
pub fn msi_tick() -> usize {
    MSI_COUNT.fetch_add(1, Ordering::Relaxed) + 1
}
/// (online-vector | usize::MAX, count) for the pre-STARTCPU status line.
pub fn msi_status() -> (usize, usize) {
    (
        MSI_ONLINE_VECTOR.load(Ordering::Relaxed),
        MSI_COUNT.load(Ordering::Relaxed),
    )
}

static REPORTED: AtomicBool = AtomicBool::new(false);

/// The one-block report for a breadcrumb that is not ours (or absent). Split
/// out from [`read_report_and_clear`] so the wording the `/proc` reader shows
/// can be exercised without two privileged port accesses: on this crate's own
/// target the CMOS read is an `in`/`out` pair, which no hosted test can issue.
fn format_no_breadcrumb(magic: u8) -> alloc::string::String {
    use alloc::string::String;
    use core::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "[gpusurvive] no console-GPU breadcrumb recorded (CMOS magic {:#04x} != {:#04x}) -- either no attempt ran since the last clear, or the firmware reused the bytes.",
        magic, MAGIC
    );
    s
}

/// The one-block report for a breadcrumb that *is* ours: the milestone, its
/// label, the narration count, and the verdict the milestone implies. Pure, so
/// the verdict it draws is testable; see [`format_no_breadcrumb`].
fn format_breadcrumb(milestone: u8, narr: u8) -> alloc::string::String {
    use alloc::string::String;
    use core::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "[gpusurvive] previous console-GPU boot attempt breadcrumb:"
    );
    let _ = writeln!(
        s,
        "[gpusurvive]   last milestone = {:#04x}  ({})",
        milestone,
        milestone_label(milestone)
    );
    let _ = writeln!(
        s,
        "[gpusurvive]   RM narration lines emitted before the freeze = {}",
        narr
    );
    if milestone == milestone::STARTCPU_PRE {
        let _ = writeln!(
            s,
            "[gpusurvive]   => WEDGED on the posted STARTCPU store, as the fabric-backpressure model predicts."
        );
    } else if milestone >= milestone::INITRM_RETURN {
        let _ = writeln!(
            s,
            "[gpusurvive]   => got past kgspInitRm; any freeze was in a LATER stage, not the SEC2 window."
        );
    }
    s
}

/// The three breadcrumb bytes, as one read.
fn read_breadcrumb() -> (u8, u8, u8) {
    (
        read_breadcrumb_byte(CMOS_MAGIC_OFF),
        read_breadcrumb_byte(CMOS_MILESTONE_OFF),
        read_breadcrumb_byte(CMOS_NARR_OFF),
    )
}

/// Zero the three breadcrumb bytes -- or rather, ask to: the write is a
/// deliberate no-op on every architecture (see [`cmos_write`]), so on the
/// machine this ships to the bytes keep their values and the "already read
/// this boot" note below is the only thing that marks a re-read. It is spelled
/// out all the same so the intent of the clear survives if the bytes ever move
/// somewhere writable.
fn clear_breadcrumb() {
    write_breadcrumb_byte(CMOS_MAGIC_OFF, 0);
    write_breadcrumb_byte(CMOS_MILESTONE_OFF, milestone::NONE);
    write_breadcrumb_byte(CMOS_NARR_OFF, 0);
}

/// The storage seam, and it sits BELOW `read_breadcrumb`/`clear_breadcrumb` on
/// purpose: both of those run the same code in a test as in the kernel, and
/// only the bytes underneath them change. The kernel's are the battery-backed
/// CMOS ones, reached with a pair of privileged `in`/`out` instructions that no
/// hosted test can issue; a test's are three words in [`test_cmos`], whose
/// write is the same no-op the hardware one is, so a test sees the clear path
/// behave exactly as it does on the machine.
#[cfg(not(test))]
fn read_breadcrumb_byte(idx: u8) -> u8 {
    unsafe { cmos_read(idx) }
}

#[cfg(not(test))]
fn write_breadcrumb_byte(idx: u8, value: u8) {
    unsafe { cmos_write(idx, value) }
}

#[cfg(test)]
fn read_breadcrumb_byte(idx: u8) -> u8 {
    test_cmos::read(idx)
}

#[cfg(test)]
fn write_breadcrumb_byte(idx: u8, value: u8) {
    test_cmos::write(idx, value)
}

/// Test-only stand-in for the battery-backed bytes.
#[cfg(test)]
mod test_cmos {
    use super::{CMOS_MAGIC_OFF, CMOS_MILESTONE_OFF, CMOS_NARR_OFF};
    use core::sync::atomic::{AtomicU8, Ordering};
    static MAGIC_BYTE: AtomicU8 = AtomicU8::new(0);
    static MILESTONE_BYTE: AtomicU8 = AtomicU8::new(0);
    static NARR_BYTE: AtomicU8 = AtomicU8::new(0);

    fn cell(idx: u8) -> &'static AtomicU8 {
        match idx {
            CMOS_MAGIC_OFF => &MAGIC_BYTE,
            CMOS_MILESTONE_OFF => &MILESTONE_BYTE,
            CMOS_NARR_OFF => &NARR_BYTE,
            other => panic!("the breadcrumb does not live at {:#04x}", other),
        }
    }

    /// What the previous boot or the firmware left in the bytes. This is test
    /// setup, not the kernel's write path: the kernel cannot put anything here
    /// at all, which is what [`write`] models.
    pub fn stamp(magic: u8, milestone: u8, narr: u8) {
        MAGIC_BYTE.store(magic, Ordering::SeqCst);
        MILESTONE_BYTE.store(milestone, Ordering::SeqCst);
        NARR_BYTE.store(narr, Ordering::SeqCst);
    }

    pub fn read(idx: u8) -> u8 {
        cell(idx).load(Ordering::SeqCst)
    }

    /// A no-op, exactly like `cmos_write` on hardware: writing 0x40-0x42 broke
    /// the firmware's NVRAM checksum on the bring-up box, so the GPU bring-up
    /// must never put anything in these bytes. Modelling it here is what makes
    /// the tests of the clear path describe the shipped kernel.
    pub fn write(idx: u8, _value: u8) {
        let _ = cell(idx);
    }

    pub fn bytes() -> (u8, u8, u8) {
        (
            read(CMOS_MAGIC_OFF),
            read(CMOS_MILESTONE_OFF),
            read(CMOS_NARR_OFF),
        )
    }
}

/// Read back the breadcrumb the previous attempt left, format a one-block
/// report, then clear it (idempotent within a boot: repeated calls after the
/// first return "already read this boot"). Call from `/proc/gpusurvive`.
pub fn read_report_and_clear() -> alloc::string::String {
    let (magic, milestone, narr) = read_breadcrumb();
    if magic != MAGIC {
        return format_no_breadcrumb(magic);
    }
    let mut s = format_breadcrumb(milestone, narr);
    // Clear so the next attempt starts from a known-empty slate.
    clear_breadcrumb();
    if REPORTED.swap(true, Ordering::Relaxed) {
        s.push_str(
            "[gpusurvive]   (note: breadcrumb already read once this boot; values now cleared)\n",
        );
    }
    s
}

#[cfg(test)]
mod survival_tests {
    use super::*;
    extern crate std;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Every milestone this module defines, in the order the console boot
    /// reaches them. The report that survives a wedge is the only thing that
    /// names where it died, so this list is what the two tests below check the
    /// labels and the verdict against.
    fn every_milestone() -> Vec<(u8, &'static str)> {
        vec![
            (milestone::NONE, "NONE"),
            (milestone::BOOT_ENTER, "BOOT_ENTER"),
            (milestone::INITRM_CALL, "INITRM_CALL"),
            (milestone::STARTCPU_PRE, "STARTCPU_PRE"),
            (milestone::STARTCPU_POST, "STARTCPU_POST"),
            (milestone::PDISP_RESTORE, "PDISP_RESTORE"),
            (milestone::INITRM_RETURN, "INITRM_RETURN"),
            (milestone::CONTROLS, "CONTROLS"),
            (milestone::STATE_LOAD, "STATE_LOAD"),
            (milestone::CE_MOVE, "CE_MOVE"),
            (milestone::COMPLETE, "COMPLETE"),
        ]
    }

    /// A milestone with no label of its own reports as "unknown" in the one
    /// report that outlives the wedge, which is exactly the boot stage we
    /// rebooted to find out. Every constant must name itself.
    #[test]
    fn every_milestone_has_a_label_that_names_it() {
        for (value, name) in every_milestone() {
            let label = milestone_label(value);
            assert_ne!(
                label, "unknown",
                "milestone {} ({:#04x}) has no label",
                name, value
            );
            assert!(
                label.contains(name) || value == milestone::NONE,
                "the label for {} ({:#04x}) does not name it: {:?}",
                name,
                value,
                label
            );
        }
    }

    /// A byte that is not one of ours (battery-backed garbage, or firmware
    /// reusing the slot) must say so rather than be read as a stage.
    #[test]
    fn a_byte_that_is_not_a_milestone_reports_unknown() {
        assert_eq!(milestone_label(0x11), "unknown");
        assert_eq!(milestone_label(0xFE), "unknown");
    }

    /// The verdict in the report is drawn from `milestone >= INITRM_RETURN`,
    /// so the numbering has to agree with the boot order: the stages before
    /// the SEC2 window must sort below it and the ones after it above, or the
    /// report tells us the freeze was in a later stage when it was the wedge.
    #[test]
    fn the_milestone_numbers_sort_in_boot_order() {
        let all = every_milestone();
        for pair in all.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "{} ({:#04x}) does not sort before {} ({:#04x})",
                pair[0].1,
                pair[0].0,
                pair[1].1,
                pair[1].0
            );
        }
        for m in [
            milestone::BOOT_ENTER,
            milestone::INITRM_CALL,
            milestone::STARTCPU_PRE,
            milestone::STARTCPU_POST,
            milestone::PDISP_RESTORE,
        ] {
            assert!(
                m < milestone::INITRM_RETURN,
                "{:#04x} is at or past INITRM_RETURN but happens before it",
                m
            );
        }
        for m in [
            milestone::CONTROLS,
            milestone::STATE_LOAD,
            milestone::CE_MOVE,
            milestone::COMPLETE,
        ] {
            assert!(
                m >= milestone::INITRM_RETURN,
                "{:#04x} happens after kgspInitRm returned but sorts below it",
                m
            );
        }
    }

    /// The whole reason the breadcrumb exists: a milestone of STARTCPU_PRE
    /// means the box died *on* the posted store, and the report has to say so
    /// outright -- that one line is the answer a reboot was spent on.
    #[test]
    fn a_wedge_on_the_posted_store_is_called_a_wedge() {
        let report = format_breadcrumb(milestone::STARTCPU_PRE, 37);
        assert!(
            report.contains("WEDGED on the posted STARTCPU store"),
            "{}",
            report
        );
        assert!(
            report.contains("0x40"),
            "the report must name the raw byte: {}",
            report
        );
        assert!(
            report.contains("37"),
            "the report must name the narration count: {}",
            report
        );
        assert!(
            !report.contains("got past kgspInitRm"),
            "a wedge on the store did not get past kgspInitRm: {}",
            report
        );
    }

    /// Past kgspInitRm the verdict flips: the freeze was somewhere later, not
    /// in the SEC2 window.
    #[test]
    fn a_milestone_past_initrm_clears_the_sec2_window() {
        for m in [
            milestone::INITRM_RETURN,
            milestone::CONTROLS,
            milestone::STATE_LOAD,
            milestone::CE_MOVE,
            milestone::COMPLETE,
        ] {
            let report = format_breadcrumb(m, 0);
            assert!(
                report.contains("got past kgspInitRm"),
                "{:#04x} should clear the SEC2 window: {}",
                m,
                report
            );
            assert!(
                !report.contains("WEDGED"),
                "{:#04x} is not the wedge: {}",
                m,
                report
            );
        }
    }

    /// A stage between the store and the return draws no verdict either way:
    /// claiming it got past kgspInitRm would be wrong, and calling it the
    /// wedge would be too.
    #[test]
    fn a_milestone_between_the_store_and_the_return_draws_no_verdict() {
        for m in [
            milestone::BOOT_ENTER,
            milestone::INITRM_CALL,
            milestone::STARTCPU_POST,
            milestone::PDISP_RESTORE,
        ] {
            let report = format_breadcrumb(m, 1);
            assert!(!report.contains("WEDGED"), "{:#04x}: {}", m, report);
            assert!(
                !report.contains("got past kgspInitRm"),
                "{:#04x}: {}",
                m,
                report
            );
            assert!(
                report.contains(milestone_label(m)),
                "{:#04x} must still name its stage: {}",
                m,
                report
            );
        }
    }

    /// With no magic of ours in the bytes the report says nothing about
    /// stages, and names both the byte it found and the one it wanted -- the
    /// firmware reusing the slot and no attempt at all look the same otherwise.
    #[test]
    fn a_foreign_magic_reports_no_breadcrumb_and_names_both_bytes() {
        let report = format_no_breadcrumb(0x00);
        assert!(
            report.contains("no console-GPU breadcrumb recorded"),
            "{}",
            report
        );
        assert!(report.contains("0x00"), "{}", report);
        assert!(report.contains("0xec"), "{}", report);
        assert!(
            !report.contains("last milestone"),
            "a foreign byte must not be read as a stage: {}",
            report
        );
    }

    /// The MSI accounting is three process-global words, and `cargo test` runs
    /// in parallel, so everything that touches them lives in this one test
    /// (asking a global counter for its value from two tests at once is a
    /// race, not a test) and it hands them back offline.
    #[test]
    fn the_msi_accounting_tracks_the_vector_and_counts_from_zero() {
        msi_offline();
        let (vector, _) = msi_status();
        assert_eq!(
            vector,
            usize::MAX,
            "offline must be unmistakable, not vector 0"
        );

        // Coming online zeroes the count: the pre-STARTCPU status line has to
        // show what fired for *this* boot, not a tally carried over.
        assert_eq!(msi_tick(), 1);
        msi_set_online(42);
        let (vector, count) = msi_status();
        assert_eq!(vector, 42);
        assert_eq!(count, 0, "coming online must zero the count");

        assert_eq!(msi_tick(), 1, "the tick returns the new count");
        assert_eq!(msi_tick(), 2);
        assert_eq!(msi_status(), (42, 2));

        // Going offline again says "no MSI" without erasing what was counted:
        // a wedge after the GPU stopped delivering still needs the tally.
        msi_offline();
        assert_eq!(msi_status(), (usize::MAX, 2));
        msi_offline();
    }

    /// Writing CMOS bytes 0x40-0x42 broke the BIOS settings checksum on the
    /// bring-up box (the next boot halted at POST), so the whole write path is
    /// a deliberate no-op. These three are still called from the boot, and
    /// from inside the STARTCPU bracket with interrupts off, so they must stay
    /// callable and do nothing at all.
    #[test]
    fn the_breadcrumb_writers_are_inert() {
        checkpoint(milestone::STARTCPU_PRE);
        narration_tick();
        reset_narration();
        checkpoint(milestone::COMPLETE);
    }

    /// `read_report_and_clear` end to end, over the same bytes the kernel
    /// reads and with the same disabled write underneath it. One test for the
    /// whole function because `REPORTED` is a one-shot process-global: a
    /// second test flipping it would depend on which ran first.
    #[test]
    fn the_report_reads_the_breadcrumb_and_marks_the_re_read() {
        // Bytes that are not ours: no stage is reported, and nothing is
        // touched -- there is nothing of ours there to clear.
        test_cmos::stamp(0x00, milestone::CE_MOVE, 9);
        let report = read_report_and_clear();
        assert!(
            report.contains("no console-GPU breadcrumb recorded"),
            "{}",
            report
        );
        assert!(!report.contains("CE_MOVE"), "{}", report);
        assert_eq!(
            test_cmos::bytes(),
            (0x00, milestone::CE_MOVE, 9),
            "a foreign breadcrumb must be left alone, not clobbered"
        );

        // Our own breadcrumb: the stage comes back, and the first read says
        // nothing about having been read before.
        test_cmos::stamp(MAGIC, milestone::STARTCPU_PRE, 41);
        let report = read_report_and_clear();
        assert!(report.contains("STARTCPU_PRE"), "{}", report);
        assert!(report.contains("41"), "{}", report);
        assert!(!report.contains("already read once"), "{}", report);

        // And the bytes still hold it afterwards, because the clear is the
        // same no-op the kernel ships: writing 0x40-0x42 broke the firmware's
        // NVRAM checksum, so nothing may be put there.
        assert_eq!(
            test_cmos::bytes(),
            (MAGIC, milestone::STARTCPU_PRE, 41),
            "the clear must not have written the bytes"
        );

        // Which is why the note matters: a second read this boot finds the
        // same breadcrumb and has to say it is the same one, or the one
        // attempt reads as two.
        let report = read_report_and_clear();
        assert!(report.contains("STARTCPU_PRE"), "{}", report);
        assert!(
            report.contains("already read once"),
            "a re-read of the same breadcrumb must say so: {}",
            report
        );

        test_cmos::stamp(0, milestone::NONE, 0);
    }
}
