//! Deterministic TLB-shootdown starvation hammer.
//!
//! Activated with `eclipse.tlbhammer=N` on the kernel cmdline (N = online CPU
//! budget, typically 6). Spawns:
//!
//! * `N-2` mapper threads that repeatedly map/unmap pages (each unmap forces
//!   `remote_flush_tlb_aspace` across peers that share the aspace filter),
//! * 1 holder thread that keeps a `lock::Mutex` (IRQ-off) across long
//!   non-pumping busy bursts — the fatfs `DirIter` signature from the real
//!   panics,
//! * 1 churn thread that creates short-lived processes and kills them so
//!   `VmAddressRegion::clear` races with the mappers.
//!
//! On a kernel PRE-#1026 (`24bac694`) this must trip `DIAG: shootdown
//! starvation` within seconds under QEMU `-smp 6`. On current master it must
//! survive 10+ minutes: the NMI rescue is a safety net, but the root fixes
//! (never-freeze `commit_entry`, IRQ-off holders that pump, NMI-safe
//! `cpu_id`) are what keep the normal ack path alive.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use kernel_hal::timer::timer_now;
use lock::Mutex;
use zircon_object::task::{Process, Task, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject, PAGE_SIZE};

/// Hammer diagnostics go to dmesg *and* serial so QEMU/hardware captures can
/// see progress without reading `/proc/kmsg` (klog_info alone is ring-buffer only).
fn hammer_log(msg: core::fmt::Arguments) {
    kernel_hal::console::serial_write_fmt_spin(format_args!("\n{}\n", msg));
    klog_info!("{}", msg);
}

/// Shared lock retained IRQ-off by the holder thread. Mappers that briefly
/// take it serialize behind the long non-pumping burst — amplifying the
/// convoy the real panics showed (HOLDER waits TLB ack from a CPU stuck in
/// unrelated IRQ-off work).
static HOLD: Mutex<()> = Mutex::new(());

/// The CPU budget `eclipse.tlbhammer=N` asks for. `None` = disabled.
///
/// Read through `kernel_hal::cmdline`, which is the kernel's one parser for
/// this string, rather than by splitting on `"eclipse.tlbhammer="` -- that had
/// no notion of a key, so `xeclipse.tlbhammer=6` armed the hammer and so did
/// the text appearing inside somebody else's value, and it read a value that
/// is not a number as however many digits it happened to start with.
///
/// `=0` DISABLES it. It used to clamp up to three, i.e. the one spelling
/// anybody reaches for to turn a thing off armed three threads hammering the
/// TLB instead -- the same kill-switch inversion `cmdline` was written to end.
pub fn parse_tlbhammer(cmdline: &str) -> Option<usize> {
    let spelled = kernel_hal::cmdline::value(cmdline, "eclipse.tlbhammer")?;
    let Some(n) = kernel_hal::cmdline::parse_number(spelled).and_then(|n| usize::try_from(n).ok())
    else {
        warn!(
            "eclipse.tlbhammer={} is not a CPU budget; the hammer stays off",
            spelled
        );
        return None;
    };
    if n == 0 {
        return None;
    }
    // Need at least 1 mapper + holder + churn.
    Some(n.max(3))
}

/// Spawn the hammer. Call once after SMP is up and the executor is running
/// (same window as other deferred boot tasks).
pub fn start(n: usize) {
    let online = kernel_hal::online_cpu_count().max(1);
    let n = n.min(online.max(3));
    let mappers = n.saturating_sub(2).max(1);
    hammer_log(format_args!(
        "Eclipse: TLB hammer ON (eclipse.tlbhammer={}) — {} mappers + 1 irq-off holder + 1 process churn",
        n, mappers
    ));

    for i in 0..mappers {
        kernel_hal::thread::spawn(async move {
            mapper_loop(i).await;
        });
    }
    kernel_hal::thread::spawn(async {
        holder_loop().await;
    });
    kernel_hal::thread::spawn(async {
        churn_loop().await;
    });
    kernel_hal::thread::spawn(async {
        progress_loop().await;
    });
}

async fn sleep_ms(ms: u64) {
    kernel_hal::thread::sleep_until(timer_now() + Duration::from_millis(ms)).await;
}

/// Map a page, touch it (install TLB entry), unmap — forces a cross-CPU
/// shootdown when peers have the aspace loaded / filter does not skip them.
async fn mapper_loop(id: usize) {
    let job = ROOT_JOB.create_child().unwrap_or_else(|_| ROOT_JOB.clone());
    let mut rounds: u64 = 0;
    loop {
        // Occasionally contend on HOLD so the holder's IRQ-off window and a
        // shootdown initiator share a convoy.
        if rounds & 63 == 0 {
            let _g = HOLD.lock();
            core::hint::spin_loop();
        }

        let Ok(proc) = Process::create(&job, "tlbhammer-map") else {
            sleep_ms(1).await;
            continue;
        };
        let vmar = proc.vmar();
        let vmo = VmObject::new_paged(1);
        match vmar.map(None, vmo, 0, PAGE_SIZE, MMUFlags::READ | MMUFlags::WRITE) {
            Ok(va) => {
                // Commit the page so a remote CPU that steals this aspace (or
                // shares kernel mappings) can hold a stale TLB entry.
                //
                // Through the VMAR, never by dereferencing `va`: that address
                // belongs to the NEW process's address space, while this
                // kernel thread runs on a different CR3. The old
                // `write_volatile(va as *mut u8)` therefore stored through an
                // address that is unmapped here, and the hammer's own first
                // round took a kernel-context `null-range #PF` at
                // `tlb_hammer::mapper_loop` -- the containment path then
                // retired the coroutine, so the harness killed its own mapper
                // instead of stressing anything. `write_memory` commits the
                // same page through the VMO, which is what the shootdowns
                // below are about.
                let _ = vmar.write_memory(va, &[(id as u8).wrapping_add(1)]);
                let _ = vmar.unmap(va, PAGE_SIZE);
            }
            Err(_) => {
                // Also hammer the unfiltered path: full remote flush.
                kernel_hal::remote_flush_tlb(Some(PAGE_SIZE * (2 + id)));
            }
        }
        // Teardown → Job::kill → VmAddressRegion::clear → per-range shootdowns
        // (same path as glxgears ^C / window close).
        proc.kill();
        rounds = rounds.wrapping_add(1);
        if rounds & 15 == 0 {
            kernel_hal::thread::yield_now().await;
        }
    }
}

/// Hold a spinlock with IRQs off and do a long non-pumping burst — mirrors
/// fatfs directory I/O under `lock::Mutex`. Waiters pump; the *holder* does
/// not, which is exactly the deaf-CPU window the NMI path had to cover.
async fn holder_loop() {
    let mut bursts: u64 = 0;
    loop {
        {
            let _g = HOLD.lock();
            // ~few ms of IRQ-off work without lock::pump(). Tuned to outlast
            // several IPI re-kicks so PRE-#1026 kernels trip the 8s detector
            // under the mapper storm; post-fix kernels keep acking via NMI
            // rescue + the root commit/IRQ-off fixes.
            for i in 0..2_000_000u64 {
                core::hint::spin_loop();
                // Deliberately NOT calling lock::pump() here.
                let _ = i;
            }
        }
        bursts = bursts.wrapping_add(1);
        sleep_ms(5).await;
        if bursts & 63 == 0 {
            klog_info!("tlbhammer: holder bursts={}", bursts);
        }
    }
}

/// Create/kill processes so Job::kill / VMAR clear race with mappers.
async fn churn_loop() {
    let mut kills: u64 = 0;
    loop {
        let job = ROOT_JOB.create_child().unwrap_or_else(|_| ROOT_JOB.clone());
        for _ in 0..4 {
            if let Ok(proc) = Process::create(&job, "tlbhammer-churn") {
                let vmo = VmObject::new_paged(4);
                let _ = proc.vmar().map(
                    None,
                    vmo,
                    0,
                    4 * PAGE_SIZE,
                    MMUFlags::READ | MMUFlags::WRITE,
                );
                proc.kill();
                kills = kills.wrapping_add(1);
            }
        }
        job.kill();
        if kills & 255 == 0 {
            klog_info!("tlbhammer: process kills≈{}", kills);
        }
        sleep_ms(2).await;
    }
}

static PROGRESS: AtomicU64 = AtomicU64::new(0);

async fn progress_loop() {
    loop {
        sleep_ms(10_000).await;
        let n = PROGRESS.fetch_add(1, Ordering::Relaxed) + 1;
        hammer_log(format_args!(
            "tlbhammer: alive {}0s (no shootdown starvation panic)",
            n
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line a live Eclipse boots with, which mentions no hammer.
    const REAL: &str = "LOG=error:TERM=xterm-256color:console.shell=true:ROOT=/dev/sda2";

    #[test]
    fn the_budget_written_is_the_budget_returned() {
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=6"), Some(6));
        assert_eq!(parse_tlbhammer("LOG=error:eclipse.tlbhammer=6"), Some(6));
        // The colon ends the number, and that is `cmdline`'s job to know.
        assert_eq!(
            parse_tlbhammer("eclipse.tlbhammer=6:LOG=error:ROOT=/dev/sda2"),
            Some(6)
        );
    }

    #[test]
    fn a_budget_too_small_to_run_the_hammer_is_raised_to_three() {
        // One mapper, the irq-off holder and the process churn: below three
        // there is no hammer to run, and asking for two is a mistake worth
        // correcting rather than refusing.
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=1"), Some(3));
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=2"), Some(3));
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=3"), Some(3));
    }

    #[test]
    fn zero_turns_the_hammer_off_rather_than_arming_three_threads() {
        // It used to be clamped up with the rest, so the one spelling anybody
        // reaches for to turn a thing off armed a TLB hammer instead.
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=0"), None);
        assert_eq!(parse_tlbhammer("LOG=error:eclipse.tlbhammer=0"), None);
    }

    #[test]
    fn a_line_that_never_mentioned_the_hammer_does_not_arm_it() {
        assert_eq!(parse_tlbhammer(REAL), None);
        assert_eq!(parse_tlbhammer(""), None);
    }

    #[test]
    fn the_name_inside_a_longer_key_is_not_the_hammer() {
        // `split("eclipse.tlbhammer=")` had no notion of a key.
        assert!(
            "xeclipse.tlbhammer=6".contains("eclipse.tlbhammer="),
            "which is what the old parser asked"
        );
        assert_eq!(parse_tlbhammer("xeclipse.tlbhammer=6"), None);
        assert_eq!(parse_tlbhammer("no.eclipse.tlbhammer=6"), None);
    }

    #[test]
    fn the_name_inside_somebody_elses_value_is_not_the_hammer() {
        // A root device is a path the installer substitutes, not a place to
        // look for a debugging knob -- and this one armed the hammer.
        assert_eq!(
            parse_tlbhammer("ROOT=/dev/disk/by-id/eclipse.tlbhammer=6-part2"),
            None
        );
        assert_eq!(parse_tlbhammer("TERM=eclipse.tlbhammer=6"), None);
    }

    #[test]
    fn a_value_that_is_not_a_budget_leaves_the_hammer_off() {
        // `take_while(is_ascii_digit)` read `6spins` as six.
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=6spins"), None);
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=six"), None);
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=-6"), None);
    }

    #[test]
    fn a_bare_key_with_no_budget_does_not_arm_it() {
        // There is no sensible default CPU budget, so naming the knob without
        // a number is a typo, not a request.
        assert_eq!(parse_tlbhammer("LOG=error:eclipse.tlbhammer"), None);
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer="), None);
    }

    #[test]
    fn a_budget_written_in_hex_is_the_number_it_spells() {
        // The old parser stopped at the `x`, read `0`, clamped it up and armed
        // three threads -- a different hammer than the one asked for.
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=0x8"), Some(8));
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=0x0"), None);
    }

    #[test]
    fn the_key_is_case_insensitive_like_every_other_one() {
        assert_eq!(parse_tlbhammer("ECLIPSE.TLBHAMMER=6"), Some(6));
        assert_eq!(parse_tlbhammer("Eclipse.TlbHammer=6"), Some(6));
    }

    #[test]
    fn the_spaces_a_person_leaves_around_the_budget_are_not_part_of_it() {
        // The old parser wanted a digit where the space was and took the knob
        // to be absent.
        assert_eq!(
            parse_tlbhammer("LOG=error: eclipse.tlbhammer = 6 "),
            Some(6)
        );
    }
}
