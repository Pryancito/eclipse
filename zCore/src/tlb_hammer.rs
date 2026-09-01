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
use zircon_object::vm::{VmObject, MMUFlags, PAGE_SIZE};

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

/// Parse `eclipse.tlbhammer=N` from the cmdline. `None` = disabled.
pub fn parse_tlbhammer(cmdline: &str) -> Option<usize> {
    let rest = cmdline.split("eclipse.tlbhammer=").nth(1)?;
    let digits: alloc::string::String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let n: usize = digits.parse().ok()?;
    if n >= 3 {
        Some(n)
    } else {
        // Need at least 1 mapper + holder + churn.
        Some(3)
    }
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
                // Fault the page in so a remote CPU that steals this aspace
                // (or shares kernel mappings) can hold a stale TLB entry.
                unsafe {
                    core::ptr::write_volatile(va as *mut u8, (id as u8).wrapping_add(1));
                }
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
