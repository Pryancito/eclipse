#![cfg_attr(not(feature = "libos"), no_std)]
#![cfg_attr(not(feature = "libos"), feature(alloc_error_handler))]
#![deny(warnings)]
#![allow(unexpected_cfgs)]
#![cfg_attr(not(test), no_main)]
// Under `cfg(test)` the harness owns `main`, so the kernel entry point is gated
// out and *everything it reaches* -- the frame allocator, the logger, the klog
// ring, `primary_main` itself -- has no caller left. That is 39 dead-code
// errors under `#![deny(warnings)]`, none of which means anything: the code is
// reachable in every build that actually boots. The deny still holds there.
#![cfg_attr(test, allow(dead_code, unused_imports))]

use core::sync::atomic::{AtomicBool, Ordering};

extern crate alloc;
#[macro_use]
extern crate log;
#[macro_use]
extern crate cfg_if;

#[macro_use]
mod logging;

// `test` as well as the kernel build: these two modules are the panic and
// fault path, and `cargo test -p zcore` runs with `--features libos`, so
// until now NO job compiled either of them. The `#[panic_handler]` and
// `#[alloc_error_handler]` inside `lang` are the only items a host build
// cannot have (std supplies both); they carry their own `not(test)`.
#[cfg(any(not(feature = "libos"), test))]
mod lang;
#[cfg(any(not(feature = "libos"), test))]
mod oops;

/// The AHCI command-wait loop pumps the polled network stack through this
/// symbol so a slow disk command cannot starve an in-flight download. The
/// implementation lives in linux-object; a zircon kernel does not link that
/// crate, and `drivers` declares the symbol unconditionally, so without this
/// the zircon build fails to link with `undefined symbol: drivers_net_drain`.
/// There is no network stack to pump in that configuration.
#[cfg(not(feature = "linux"))]
#[no_mangle]
extern "C" fn drivers_net_drain() {}

mod fs;
mod handler;
mod invariants;
mod platform;
// `test` as well as the kernel build, and for the same reason `lang` and `oops`
// above carry one: the hammer is gated to a bare kernel, so no job that runs
// tests compiled a line of it -- and what it reads is the kernel command line,
// which is a string, which is the one thing a host test answers for exactly as
// a boot does. Nothing here calls it under `cfg(test)`; the crate-wide
// `allow(dead_code)` at the top covers that.
#[cfg(any(all(feature = "linux", not(feature = "libos")), test))]
mod tlb_hammer;
mod utils;

cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        #[path = "memory_x86_64.rs"]
        mod memory;
    } else {
        mod memory;
    }
}

// The generic arm of the pick above -- `memory.rs`, the kernel allocator on
// riscv64 and aarch64 -- is reached only by `target_arch`, and the host suite
// runs on x86_64. So no `cargo test` ever compiled a line of it, while its
// x86_64 twin has a suite of its own: any test written for it would have been
// run by nobody. Bring it in under its own name so the host suite can reach
// it. `#[path]` because the else-arm above already owns the name `memory`.
#[cfg(all(target_arch = "x86_64", test))]
#[path = "memory.rs"]
mod memory_generic;

static STARTED: AtomicBool = AtomicBool::new(false);

#[cfg(all(feature = "linux", not(feature = "libos")))]
static TLBHAMMER_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cfg(all(not(any(feature = "libos")), feature = "mock-disk"))]
static MOCK_CORE: AtomicBool = AtomicBool::new(false);

/// What `DEADLOCKSPINS=<n>` on the command line asks the deadlock detector for.
///
/// Three answers, and the third is the one that matters. `None` is a line that
/// never mentioned the knob. `Some(Ok(n))` is a spin count. `Some(Err(spelled))`
/// is somebody having written the knob and written something that is not a
/// count -- which used to read as the digits the value happened to start with,
/// so `DEADLOCKSPINS=0x4000000` came out as `0`, `set_deadlock_spins` turned
/// that back into the very default the knob exists to lower, and the klog said
/// "threshold set to 0". The detector then never fired, which is exactly the
/// failure the knob was added for: a hang read as "not a deadlock, no banner
/// appeared" when the threshold had simply not been reached.
///
/// Its one caller is in the bare-metal half of `primary_main`; a LibOS build
/// has no deadlock detector to tune, and under `deny(warnings)` the unused
/// helper stopped the LibOS build from compiling at all.
#[cfg(any(not(feature = "libos"), test))]
fn deadlock_spins(cmdline: &str) -> Option<Result<u64, &str>> {
    let spelled = kernel_hal::cmdline::value(cmdline, "DEADLOCKSPINS")?;
    Some(kernel_hal::cmdline::parse_number(spelled).ok_or(spelled))
}

/// The loader's record of the graphic-mode decision, from `_start` until the
/// klog exists to print it. See [`klog_gop_report`] and `rboot::GopReport`.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
static GOP_REPORT: spin::Mutex<rboot::GopReport> = spin::Mutex::new(rboot::GopReport::empty());

/// Called from `_start` with what rboot handed over.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn set_gop_report(report: rboot::GopReport) {
    *GOP_REPORT.lock() = report;
}

/// Print why the machine is in the graphic mode it is in.
///
/// "Changing the resolution in rboot.conf does nothing, it always keeps the
/// GOP's" has three causes -- the firmware does not offer the mode, it offers
/// it and refuses to set it, or the mode was set on a GPU with no monitor on
/// it -- and the loader cannot report any of them where it happens: its log
/// goes to the firmware's text console and the splash wipes the screen
/// immediately after, and the shipped `cmdline=LOG=error` discards it before
/// that. So the decision travels in `BootInfo` and comes out here, where
/// `dmesg | grep "\[gop\]"` finds it after boot.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn klog_gop_report() {
    let r = *GOP_REPORT.lock();
    if r.gop_count == 0 {
        // No GOP handle was enumerated at all, so there is nothing to report
        // beyond what the "no framebuffer from bootloader" path already says.
        return;
    }
    let asked = match r.asked_kind {
        rboot::GOP_ASKED_AUTO => alloc::string::String::from("auto"),
        rboot::GOP_ASKED_EXACT => alloc::format!("{}x{}", r.asked_width, r.asked_height),
        _ => alloc::string::String::from("(absent: keep the firmware's mode)"),
    };
    klog_info!(
        "[gop] rboot.conf resolution={} -> booted at {}x{}{}",
        asked,
        r.final_width,
        r.final_height,
        if r.set_called {
            if r.set_ok {
                " (SetMode ok)"
            } else {
                " (SetMode REFUSED by the firmware)"
            }
        } else {
            " (no mode set: kept the firmware's)"
        }
    );
    klog_info!(
        "[gop] GOP {} of {} handle(s), {} card(s), display attached to it: {}, panel EDID says {}x{}",
        r.gop_index,
        r.gop_count,
        r.card_count,
        if r.gop_has_edid { "yes" } else { "no" },
        r.edid_width,
        r.edid_height
    );
    if let Some(why) = r.why_not() {
        klog_info!("[gop] not the mode that was asked for: {}", why);
        // The klog ring is unfiltered, so the lines above always reach
        // `dmesg`; this one goes through the level filter on purpose, at
        // `error` so that the shipped `cmdline=LOG=error` still shows it on
        // the screen and the serial. A `resolution=` that did nothing has to
        // be visible without anybody knowing to go looking for it.
        log::error!(
            "[gop] rboot.conf resolution={} but the machine booted at {}x{}: {} (full list: dmesg | grep gop)",
            asked,
            r.final_width,
            r.final_height,
            why
        );
        // The list is the evidence for the first of the three causes, and it
        // is only worth its dmesg lines when something did go wrong.
        let n = (r.offered_len as usize).min(rboot::GOP_OFFERED_MAX);
        let mut line = alloc::string::String::new();
        for m in &r.offered[..n] {
            if !line.is_empty() {
                line.push(' ');
            }
            let _ = core::fmt::write(
                &mut line,
                format_args!(
                    "{}x{}{}",
                    m.width,
                    m.height,
                    if m.direct { "" } else { "(blt)" }
                ),
            );
        }
        klog_info!(
            "[gop] the firmware offers {} mode(s){}: {}",
            r.offered_len,
            if r.offered_len as usize > n {
                " (first 48)"
            } else {
                ""
            },
            line
        );
    }
}

/// What the command line and the enumerated cards say between them about this
/// being a multi-GPU box.
#[cfg_attr(not(any(feature = "linux", test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultiGpu {
    /// `nvidia.multigpu` is on the command line and more than one NVIDIA card
    /// enumerated. The declaration and the hardware agree.
    Declared,
    /// `nvidia.multigpu` is on the command line and exactly one NVIDIA card
    /// enumerated. The declaration is about hardware that is not there, so the
    /// box is treated as what it turned out to be.
    DeclaredButOneCard,
    /// More than one NVIDIA card enumerated and nothing declared.
    SeveralCardsUndeclared,
    /// One NVIDIA card, or none, and nothing declared.
    Single,
}

// Only the Linux personality asks this, and the tests. The `allow(dead_code)`
// on the enum above does NOT reach an inherent method, so without this the
// Zircon build fails outright on `deny(warnings)` -- which is how master
// stopped compiling its Zircon kernel for several hours. Same gate as
// `multigpu_verdict`, which builds this value.
#[cfg(any(feature = "linux", test))]
impl MultiGpu {
    /// Whether the card that scans out the console is brought up by default.
    ///
    /// True for both of the several-card cases, including the undeclared one.
    /// Moebius's convention is that a multi-GPU box says so --
    /// `nvidia.multigpu` -- and the log says as much when it did not; but
    /// *behaving* as a one-card box because the flag is missing would quietly
    /// halve a machine that enumerated two cards, so the cards win where they
    /// are the stronger evidence.
    ///
    /// On a declaration with one card the hardware wins the other way, and for
    /// a sharper reason: there the only card is the one scanning out, so the
    /// SEC2 `STARTCPU` store that can wedge takes the whole machine, with no
    /// second card to fall back on. `nvidia.scanout_gpu` and
    /// `nvidia.console_gpu` still ask for it by name.
    fn is_multigpu(self) -> bool {
        matches!(self, MultiGpu::Declared | MultiGpu::SeveralCardsUndeclared)
    }
}

/// [`MultiGpu`] from a command line and the card counts, which is the only way
/// to ask it in a test: the real counts come from the driver registry and the
/// real line from the bootloader.
#[cfg(any(feature = "linux", test))]
fn multigpu_verdict(cmdline: &str, nvidia_cards: usize, scanout_cards: usize) -> MultiGpu {
    let declared = kernel_hal::cmdline::NvidiaParams::parse(cmdline).multigpu;
    // No card scanning out means no card to bring up by default, whatever was
    // declared and however many cards there are: the question this answers is
    // about the card that holds the console.
    if scanout_cards == 0 {
        return MultiGpu::Single;
    }
    match (declared, nvidia_cards > 1) {
        (true, true) => MultiGpu::Declared,
        (true, false) => MultiGpu::DeclaredButOneCard,
        (false, true) => MultiGpu::SeveralCardsUndeclared,
        (false, false) => MultiGpu::Single,
    }
}

/// Whether the command line asks for the card that scans out to go through the
/// boot-time bring-up, the same one every other card goes through.
///
/// There is one bring-up path now, so this is not a choice of path: it is
/// whether that card takes it AT BOOT, while its GSP boot still wedges at the
/// SEC2 `STARTCPU` store.
///
/// **`nvidia.console_gpu` is not a spelling of this, on purpose.** It already
/// selects the deferred task -- wait for the desktop, park scanout, then bring
/// the card up -- and that is the one recipe that has finished with both cards
/// up on real hardware. Accepting it here would run the boot-time attempt
/// first, wedge, and never reach the desktop the deferred task waits for: the
/// flag would keep its name and lose its meaning. `nvidia.noconsole_gpu` beats
/// this one as well, so a cmdline written against the old default leaves that
/// card cold whichever flag it used.
#[cfg(any(feature = "linux", test))]
fn scanout_gpu_bringup_asked(cmdline: &str) -> bool {
    kernel_hal::cmdline::flag(cmdline, "nvidia.scanout_gpu")
        && !kernel_hal::cmdline::flag(cmdline, "nvidia.noconsole_gpu")
}

fn primary_main(config: kernel_hal::KernelConfig) {
    logging::init();
    kernel_hal::console::early_progress_bar(53);
    memory::init();
    kernel_hal::console::early_progress_bar(54);
    kernel_hal::primary_init_early(config, &handler::ZcoreKernelHandler);
    kernel_hal::console::early_progress_bar(55);
    kernel_hal::console::early_progress_bar(60);
    let options = utils::boot_options();
    logging::set_max_level(&options.log_level);
    kernel_hal::console::early_progress_bar(70);
    #[cfg(feature = "linux")]
    let (init_proc, shell_proc): (&str, &str) = (&options.init_proc, &options.shell_proc);
    #[cfg(not(feature = "linux"))]
    let (init_proc, shell_proc) = ("N/A", "N/A");

    // First line of every dmesg: which binary this is. See build.rs — real-HW
    // debugging kept tripping over stale builds masquerading as broken fixes.
    klog_info!("Eclipse: kernel build {}", env!("ECLIPSE_BUILD_ID"));
    klog_info!(
        "Eclipse: boot options log_level={} init={} shell={}",
        options.log_level,
        init_proc,
        shell_proc
    );
    // Diagnostic-build fingerprint. Bumped by hand on every commit that changes
    // what a crash log looks like, and printed unconditionally, so a pasted log
    // says WHICH kernel produced it — two hunts have already stalled on "did
    // this boot actually include the new detector, or is it last week's build?".
    klog_info!(
        "Eclipse: diag rev 10 (isolation-first containment: kill the culprit, keep the kernel up)"
    );
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    klog_gop_report();
    // Scheduler/timer A-B switches. Both default on; `TIMERDEADLINE=0` and
    // `WAKEPREEMPT=0` on the kernel command line restore the pre-change
    // behaviour. They exist so a suspected regression can be bisected on ONE
    // build by rebooting, instead of by rebuilding — a rebuild changes the
    // binary and its layout, and a QEMU/TCG run has enough variance to hide a
    // real difference either way. See docs/README-performance.md.
    #[cfg(not(feature = "libos"))]
    {
        if kernel_hal::cmdline::is_off(&options.cmdline, "TIMERDEADLINE") {
            kernel_hal::timer::set_deadline_timer(false);
            klog_info!("Eclipse: deadline timer DISABLED (TIMERDEADLINE=0)");
        }
        if kernel_hal::cmdline::is_off(&options.cmdline, "WAKEPREEMPT") {
            executor::set_wakeup_preempt(false);
            klog_info!("Eclipse: wake-up preemption DISABLED (WAKEPREEMPT=0)");
        }
        // The wake-to-run latency probe (`/proc/perf/kernel`'s `wake latency`
        // line). Sampled one wake in sixteen, so it costs an external wake
        // nothing measurable, but it is still a diagnostic and a measurement
        // of the scheduler should be able to measure it with the probe out of
        // the way.
        if kernel_hal::cmdline::is_off(&options.cmdline, "WAKELAT") {
            executor::set_wake_latency(false);
            klog_info!("Eclipse: wake-to-run latency probe DISABLED (WAKELAT=0)");
        }
        // Kernel fault containment (see `oops`). ON by default: a fault taken
        // while serving a process kills that process and the system carries on.
        // `PANICONOOPS=1` restores the immediate halt, which is what one wants
        // while debugging a specific failure — the machine freezes on the spot,
        // dump intact and the guilty coroutine still standing.
        if kernel_hal::cmdline::flag(&options.cmdline, "PANICONOOPS") {
            oops::set_panic_on_oops(true);
            klog_info!("Eclipse: kernel fault containment DISABLED (PANICONOOPS=1)");
        } else {
            klog_info!(
                "Eclipse: kernel fault containment ENABLED -- a kernel fault while \
                 serving a process kills that process, not the system \
                 (disable with PANICONOOPS=1)"
            );
        }
        // `STACKQUARANTINE=1` holds every freed coroutine stack write-protected
        // in a bounded ring instead of returning it to the heap, so a dangling
        // pointer that writes into freed stack memory faults AT THE WRITER's rip
        // (`[stack-uaf]`) — the diagnostic that pins the transient-executor UAF.
        // Costs held memory + a TLB shootdown per free, so it is opt-in.
        if kernel_hal::cmdline::flag(&options.cmdline, "STACKQUARANTINE") {
            executor::set_stack_quarantine_enabled(true);
            klog_info!(
                "Eclipse: freed-stack quarantine ENABLED (STACKQUARANTINE=1) — \
                 catching the use-after-free writer at its rip"
            );
        }
        // Deadlock-detector sensitivity. The default threshold is a spin COUNT
        // calibrated for real hardware (~8 s); under QEMU/TCG the guest runs
        // orders of magnitude slower, so that count takes many minutes and the
        // detector never fires within a test run — which is how a genuine hang
        // came to be misread as "not a deadlock, nothing was reported". Lower it
        // for emulated runs: `DEADLOCKSPINS=20000000`.
        match deadlock_spins(&options.cmdline) {
            None => {}
            Some(Ok(n)) => {
                lock::set_deadlock_spins(n);
                klog_info!("Eclipse: deadlock spin threshold set to {}", n);
            }
            Some(Err(spelled)) => klog_warn!(
                "Eclipse: DEADLOCKSPINS={} is not a spin count -- threshold left at the default",
                spelled
            ),
        }
        // Default is ON (see `COW_FORK`): the user-memory corruption that had it
        // rolled back was a stale writable TLB entry after `protect_for_cow`, now
        // fixed by the shootdown spin-pump — root cause and re-validation recorded
        // there. `FORKCOW=0` is the kill-switch; `FORKCOW=1` stays accepted so
        // existing command lines keep meaning what they say.
        if kernel_hal::cmdline::flag(&options.cmdline, "FORKCOW") {
            zircon_object::vm::set_cow_fork(true);
            klog_info!("Eclipse: copy-on-write fork ENABLED (FORKCOW=1)");
        }
        if kernel_hal::cmdline::is_off(&options.cmdline, "FORKCOW") {
            zircon_object::vm::set_cow_fork(false);
            klog_info!("Eclipse: copy-on-write fork DISABLED (FORKCOW=0) -- eager copy");
        }
        // Time every kernel heap allocation and free. Off by default because
        // this is the hottest path in the kernel; on, it is two `rdtsc`s and two
        // relaxed adds per allocation, which is enough to answer whether the
        // allocator is what makes `fork` quadratic in its mapping count.
        if kernel_hal::cmdline::flag(&options.cmdline, "HEAPPROF") {
            kernel_hal::kstats::set_heap_prof(true);
            klog_info!("Eclipse: kernel heap profiling ENABLED (HEAPPROF=1)");
        }
        // Boot-time file-access recorder. `BOOTTRACE=<comm>` arms it for the one
        // process whose comm matches (e.g. BOOTTRACE=labwc): every file it opens
        // is logged with a timestamp and result, surfaced at /proc/bootprofile
        // as a startup timeline plus a deduplicated preload list. Off by default
        // — one relaxed atomic load per openat when unset — so this is a
        // measurement build knob, not a permanent cost. The comm runs to the
        // next space/comma (labwc's comm has neither).
        // The recorder lives in linux-object, which a zircon-only kernel does
        // not link.
        #[cfg(feature = "linux")]
        if let Some(comm) = kernel_hal::cmdline::value(&options.cmdline, "BOOTTRACE") {
            if !comm.is_empty() {
                linux_object::boot_trace::set_target(comm);
                klog_info!("Eclipse: boot-trace ARMED for comm '{}' (BOOTTRACE)", comm);
            }
        }
        // How many syscalls share one clock reading for `/proc/perf`'s latency
        // columns. The dispatch brackets every syscall with `timer_now()`, and
        // `rdtsc` benches at 10.91 ns -- twice per call, about 22 ns on every
        // syscall the machine makes, more than three times what hunter's whole
        // per-syscall security gate costs, and paid whether or not anyone ever
        // reads the file. The call *counts* stay exact either way (a relaxed
        // increment); only the nanoseconds need the clock. Default 16.
        // `PERFSAMPLE=1` times every syscall and makes the columns exact,
        // which is the A/B against this.
        // Lives in linux-object, which a zircon-only kernel does not link.
        // `SYSCALLBANKS=off` collapses `/proc/syscalls`'s per-CPU histogram
        // banks onto one row, which is how the table behaved before the
        // banks existed. It is here to be the A/B: a `COUNTS[num]` shared by
        // every CPU making the same syscall benched at 8.5 ns alone, 32.6 ns
        // with two CPUs on it, 50.3 with three and ~90 with four, so what
        // the banks buy is whatever that contention is on real hardware --
        // and the host this was written on has one logical CPU, so it cannot
        // say. Default ON.
        #[cfg(feature = "linux")]
        if kernel_hal::cmdline::is_off(&options.cmdline, "SYSCALLBANKS") {
            linux_object::syscall_stats::set_banks_enabled(false);
            klog_info!("Eclipse: /proc/syscalls per-CPU banks OFF (SYSCALLBANKS=off)");
        }
        #[cfg(feature = "linux")]
        if let Some(n) = kernel_hal::cmdline::value(&options.cmdline, "PERFSAMPLE")
            .and_then(kernel_hal::cmdline::parse_number)
        {
            let n = n.min(u32::MAX as u64) as u32;
            linux_object::perf::set_sample_interval(n);
            klog_info!(
                "Eclipse: /proc/perf latency sampled 1 call in {} (PERFSAMPLE={})",
                linux_object::perf::sample_interval(),
                n
            );
        }
        // Hunter's flood/fork-bomb rate heuristics. OFF by default: they cost a
        // clock read + an IRQ-off shard-lock + BTreeMap update on EVERY syscall
        // of every process (all threads of one process serialize on the shard).
        // The constant-time sensitive-syscall WATCH stays on regardless — only
        // the rate counters are opt-in. Two spellings accepted: HUNTERANOMALY=1
        // and HUNTER_ANOMALY=1 (a parallel branch introduced the underscored
        // A/B knob when the default was still on; =0 is now the default state).
        if kernel_hal::cmdline::flag(&options.cmdline, "HUNTERANOMALY")
            || kernel_hal::cmdline::flag(&options.cmdline, "HUNTER_ANOMALY")
        {
            hunter::heuristics::set_anomaly_detection(true);
            klog_info!("Eclipse: hunter rate heuristics ENABLED (HUNTERANOMALY=1)");
        }
        // Batching a fork's cross-CPU TLB shootdowns into one. Default on, and
        // only ever applied when the forking process has a single thread — see
        // `VmAddressRegion::fork_from` for why that condition is what makes it
        // sound. `FORKGATHER=0` restores one shootdown per mapping, which is
        // what the benchmark's `fork cost per mapping` row exists to expose.
        if kernel_hal::cmdline::is_off(&options.cmdline, "FORKGATHER") {
            zircon_object::vm::set_fork_gather(false);
            klog_info!("Eclipse: batched fork TLB shootdown DISABLED (FORKGATHER=0)");
        }
        // The vDSO answers `clock_gettime` from the TSC in userspace, which
        // requires the counter to be invariant: constant-rate, and
        // reset-synchronized across cores. That is read from
        // CPUID.80000007H:EDX[8], which every x86 since about 2008 sets — and
        // which QEMU cannot set under TCG at all, because the feature word is
        // not in its TCG-supported set. `+invtsc` on the command line is
        // silently dropped there.
        //
        // So on an emulator the vDSO would sit permanently disabled and could
        // never be measured, on the one substrate where Eclipse and Linux can
        // be compared fairly. `VDSOFORCE=1` says "trust the TSC anyway". It is
        // sound under TCG, where every vCPU's `rdtsc` derives from a single
        // host clock and is therefore synchronized by construction — and it is
        // NOT a switch to set on real hardware that declines to advertise an
        // invariant TSC, because there the counter may genuinely drift between
        // sockets and userspace has no way to notice.
        if kernel_hal::cmdline::flag(&options.cmdline, "VDSOFORCE") {
            kernel_hal::timer::set_force_tsc_invariant(true);
            klog_info!("Eclipse: vDSO forced on despite CPUID (VDSOFORCE=1)");
        }
        // `FSGSBASE=0` takes `RD/WRFSBASE` and `RD/WRGSBASE` away from user
        // mode again. Default on, because the kernel does not intercept
        // `CPUID` and so cannot hide the feature from a userspace that reads
        // it -- see `kernel_hal::cpu::init_user_fsgsbase`. The hatch is here
        // because turning it on also puts an MSR read on every entry from user
        // mode (the entry stubs have to read back a base ring 3 can now move),
        // so a suspected regression in either direction can be bisected by
        // rebooting instead of by rebuilding.
        #[cfg(target_arch = "x86_64")]
        if kernel_hal::cmdline::is_off(&options.cmdline, "FSGSBASE") {
            kernel_hal::cpu::set_user_fsgsbase_allowed(false);
            klog_info!("Eclipse: user-mode FSGSBASE DISABLED (FSGSBASE=0)");
        }
        // SMP master switch. Default ON; `smp=off` forces a single-core boot.
        //
        // This used to default off because multi-core wedged real hardware at
        // the scheduler hand-off. That hang was the x2APIC id plumbing (the
        // LAPIC leaves its MMIO window behind in x2APIC mode, which only
        // physical machines enter — QEMU's default TCG CPU does not advertise
        // it), and is fixed; see `kernel_hal::common::ipi::SMP_ENABLED`.
        // `smp=off` remains for bringing a suspect machine up single-core
        // without a rebuild.
        //
        // `smp=<N>` caps the CPU COUNT instead, the boot CPU included, so a
        // machine with twenty hardware threads can be brought up on two of
        // them without a rebuild — the configuration a `-smp 20` QEMU makes
        // expensive (twenty vCPU threads, twenty 250 Hz ticks, one host to
        // emulate them all). `smp=1` is the same single-core boot as
        // `smp=off`, and reaches it through the same path.
        //
        // The number is read BEFORE the boolean on purpose: `smp=1` spells a
        // true boolean, so an `is_off` test alone would see "SMP requested"
        // and bring up every CPU under a log line claiming one.
        match kernel_hal::cmdline::smp_limit(&options.cmdline) {
            Some(1) => {
                // One CPU. The master switch stays the thing that says so --
                // it is what every arch's bring-up already reads -- and the
                // ceiling is set to match so nothing downstream can read the
                // two differently.
                kernel_hal::set_smp_enabled(false);
                kernel_hal::set_smp_max_cpus(1);
                klog_info!("Eclipse: single-core boot forced (smp=off)");
            }
            Some(n) => {
                kernel_hal::set_smp_max_cpus(n);
                klog_info!("Eclipse: SMP bring-up capped at {} CPU(s) (smp={})", n, n);
            }
            None => {
                klog_info!(
                    "Eclipse: SMP multi-core bring-up enabled (pass `smp=off`, or `smp=<N>` to \
                     cap the CPU count)"
                );
            }
        }
        // Tickless idle: an idle CPU stretches its tick to its own next
        // deadline (capped) instead of taking 250 Hz for nothing. Default ON;
        // `tickless=off` is the escape hatch, and it is one the bootloader can
        // set without a rebuild — which matters because the failure this
        // mechanism had the first time was an unresponsive keyboard, and a
        // machine you cannot type into is one you fix from the boot menu.
        if kernel_hal::cmdline::is_off(&options.cmdline, "tickless") {
            kernel_hal::timer::set_tickless_idle(false);
            klog_info!(
                "Eclipse: tickless idle disabled (tickless=off) — full-rate tick on every CPU"
            );
        }
        // Keep the kernel text console on screen even while a compositor holds
        // its VT in KD_GRAPHICS. Normally KD_GRAPHICS suppresses presentation
        // so labwc owns the display, which also means a hard hang's last
        // kernel line is never seen on a monitor-only box -- exactly the
        // console tail we keep having to ask for when the compositor dies.
        // `kernel_hal::console::set_diag_present_over_graphics` existed for
        // this and had no caller, so the diagnostic could not be turned on at
        // all; this is its switch.
        if kernel_hal::cmdline::flag(&options.cmdline, "console.overgraphics") {
            kernel_hal::console::set_diag_present_over_graphics(true);
            klog_info!(
                "Eclipse: console.overgraphics ON — el log del kernel se sigue pintando \
                 sobre un VT en KD_GRAPHICS (la última línea antes de un cuelgue queda en pantalla)"
            );
        }
        // Deterministic TLB-shootdown starvation hammer (see tlb_hammer.rs).
        #[cfg(all(feature = "linux", not(feature = "libos")))]
        if let Some(n) = tlb_hammer::parse_tlbhammer(&options.cmdline) {
            // Stash N in a static; spawn after primary_init / executor is live.
            TLBHAMMER_N.store(n as u64, Ordering::Relaxed);
        }
    }
    // Deadlock self-report: any CPU spinning >~8s on a kernel spinlock paints
    // the stuck call site(s) onto the red framebuffer banner (lock-free), so a
    // silent freeze names its own deadlock instead of needing a serial cable.
    // Kept permanently -- it is free until something actually deadlocks.
    #[cfg(not(feature = "libos"))]
    lock::set_deadlock_hook(lang::deadlock_report);
    // A CPU spinning on a kernel lock has IRQs off and cannot ack TLB
    // shootdowns; the shootdown initiator spin-waits for that ack. The pump
    // lets spinners drain their own shootdown queue mid-spin (lock-free work
    // only), breaking the convoy that made parallel VM operations measure 80x
    // slower in absolute terms than single-threaded.
    #[cfg(not(feature = "libos"))]
    lock::set_spin_pump(kernel_hal::tlb_shootdown_pump);
    // Arm the hardware write-watch on the IPI ring's `size` word before any
    // userspace runs. See `kernel_hal::arm_queue_watch`: that word is set once
    // at construction and never written again, so it needs no noise filtering
    // — any store into it is the corruption this hunt is chasing, and the trap
    // carries the writer's `rip`, which no post-mortem has been able to give.
    #[cfg(not(feature = "libos"))]
    if kernel_hal::arm_queue_watch() {
        // Direct serial, not `info!`: a crash hunt runs at LOG=error, and
        // "was the probe actually armed?" must be answerable from the same
        // paste as the hit it is waiting for.
        kernel_hal::console::serial_write_str(
            "[ipi-watch] armed: any write to the IPI ring integrity word names its writer\n",
        );
    }
    // Second hook: alongside the stuck WAITERS, paint the acquire site of the
    // lock's current HOLDER (snapshotted from the lock by kernel-sync). The
    // waiters in the banner are usually innocent readers; the HOLDER line is
    // the one that names the wedged code path.
    #[cfg(not(feature = "libos"))]
    lock::set_deadlock_holder_hook(lang::deadlock_holder_report);
    // NOTE: present-over-graphics diagnostic is now OFF (the lazy fork map
    // fixed the stall it was hunting) -- labwc owns the screen again in
    // KD_GRAPHICS; kernel logs go to dmesg and the text console only.
    memory::insert_regions(&kernel_hal::mem::free_pmem_regions());
    // The kernel runs on the BOOTLOADER's page tables, which live in memory
    // UEFI reports as reclaimable — the line above just fed those live
    // page-table frames to the allocator as free RAM. Pull them back out
    // before the first allocation can recycle one (see the function's doc for
    // the corruption/triple-fault chain this caused).
    #[cfg(target_arch = "x86_64")]
    memory::reserve_active_page_table_frames();
    // Reclaim: tell the allocator which caches can give memory back, now that
    // it has its regions and before anything can fill them. Until this existed
    // the only answer to a shortage was a bigger cap, which is why the kernel
    // heap is 512 MiB. The answer says whether the registry took it; a `false`
    // here means `shrinker::MAX_SHRINKERS` needs raising, and the only cost of
    // missing it is that the cache stops being reclaimable.
    if !linux_object::fs::register_shrinkers() {
        log::error!("shrinker registry is full: the ELF image cache will not be reclaimed");
    }
    kernel_hal::console::early_progress_bar(80);
    kernel_hal::primary_init();
    // Bring up the hunter security subsystem. Register the monotonic clock
    // first so the very first recorded event carries a real timestamp, then a
    // durable sink that streams high-severity (Warning+) events to the kernel
    // log before any in-memory ring eviction can erase them, then initialize
    // the policy/IDS engine.
    hunter::set_time_source(|| kernel_hal::timer::timer_now().as_nanos() as u64);
    hunter::set_sink(|e: &hunter::event_log::LogEntry| {
        kernel_hal::klog_info!(
            "hunter[{}] {} {} pid={} {}",
            e.severity.as_str(),
            e.category,
            e.action,
            e.pid,
            e.description
        );
    });
    hunter::init();
    // (The anomaly-rate heuristics now default OFF — see the HUNTERANOMALY=1 /
    // HUNTER_ANOMALY=1 opt-in in the cmdline-switch block above. The old
    // HUNTER_ANOMALY=0 A/B knob became the default state.)
    kernel_hal::console::early_progress_bar(90);
    cfg_if! {
        if #[cfg(all(feature = "linux", feature = "zircon"))] {
            panic!("Feature `linux` and `zircon` cannot be enabled at the same time!");
        } else if #[cfg(feature = "linux")] {
            use linux_object::process::ProcessExt;
            use zircon_object::object::KernelObject;
            // Parse "arg0?arg1?arg2"; an empty string yields no program.
            fn parse_proc(s: &str) -> alloc::vec::Vec<alloc::string::String> {
                if s.is_empty() {
                    alloc::vec::Vec::new()
                } else {
                    s.split('?').map(Into::into).collect()
                }
            }
            let init_args = parse_proc(&options.init_proc);
            let shell_args = parse_proc(&options.shell_proc);
            // Base environment for the shells and PID 1 init. `HOME`/`TERM`/
            // `USER`/`LOGNAME` are set HERE (not just in /etc/profile) because
            // bash, unlike POSIX sh, ignores `ENV` and only sources /etc/profile
            // as a *login* shell — without these in the real environment bash
            // greets with "I can't find my home directory!" and readline (tab
            // completion) misbehaves for lack of `TERM`.
            // UI language: `lang=en` on the kernel cmdline selects English.
            // Default Spanish. Do NOT set LC_ALL — it would freeze LANG
            // for gettext/GTK in every process that inherits this env
            // (busybox shells on the VTs). musl treats any *.UTF-8 name
            // as UTF-8, which foot requires.
            let lang_tag = options
                .cmdline
                .split(|c: char| c == ':' || c.is_whitespace())
                .find_map(|t| t.strip_prefix("lang=").map(str::trim))
                .unwrap_or("es");
            let (lang_var, language_var): (&str, &str) = match lang_tag {
                "en" | "EN" | "en_US" => ("LANG=en_US.UTF-8", "LANGUAGE=en"),
                _ => ("LANG=es_ES.UTF-8", "LANGUAGE=es:en"),
            };
            let envs: alloc::vec::Vec<alloc::string::String> = alloc::vec![
                // /usr/local/bin first so the Eclipse labwc wrapper (which
                // selects the renderer by the two-condition GL gate --
                // NVIDIA experiment knobs only on NVIDIA +
                // nvidia.nouveau_uapi, but with a software-default labwc
                // session unless explicit wlroots GPU flags are present)
                // shadows the apk-installed binary.
                "PATH=/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
                "ENV=/etc/profile".into(),
                "HOME=/root".into(),
                "TERM=xterm-256color".into(),
                "USER=root".into(),
                "LOGNAME=root".into(),
                lang_var.into(),
                language_var.into(),
                {
                    let mut country = "ES";
                    let mut tz: Option<&str> = None;
                    for tok in options
                        .cmdline
                        .split(|c: char| c == ':' || c.is_whitespace())
                    {
                        if let Some(v) = tok.strip_prefix("tz=").map(str::trim) {
                            if !v.is_empty() {
                                tz = Some(v);
                            }
                        }
                        if let Some(v) = tok.strip_prefix("country=").map(str::trim) {
                            if !v.is_empty() {
                                country = v;
                            }
                        }
                    }
                    let zone = tz.unwrap_or(match country {
                        "US" | "us" | "USA" | "usa" => "America/New_York",
                        _ => "Europe/Madrid",
                    });
                    alloc::format!("TZ={zone}")
                },
                // wlroots/labwc configuration. Set in the real environment (not
                // only /etc/profile) so a compositor launched from a *non-login*
                // shell — which busybox sh does not source /etc/profile for —
                // still gets them. Without WLR_RENDERER=pixman labwc tries
                // GLES2/EGL then Vulkan; with no GPU that path may fail outright,
                // or run on Mesa llvmpipe and be extremely slow because each
                // frame is rendered and copied on CPU. See xtask write_profile
                // for the rationale of each.
                //"WLR_RENDERER=pixman".into(),
                //"WLR_RENDERER_ALLOW_SOFTWARE=1".into(),
                // WLR_NO_HARDWARE_CURSORS deliberately NOT set: the kernel DRM
                // scheme composites the legacy MODE_CURSOR bitmap over every
                // scanned-out frame (drm.rs set_cursor_bo/move_cursor), so
                // wlroots' hardware-cursor path both works and avoids
                // re-rendering the whole scene on pointer moves.
                //"WLR_LIBINPUT_NO_DEVICES=1".into(),
                // On systems with multiple GPUs (e.g. two NVIDIA RTX 2060
                // SUPER cards) wlroots enumerates ALL DRM nodes unless this
                // variable restricts it. Opening card1 (compute GPU, no UEFI
                // framebuffer) in addition to card0 causes two problems:
                //   1. Both cards share the same global DRM_STATE and expose
                //      identical synthetic CRTC/connector IDs → wlroots
                //      allocates 0×0 dumb buffers for the "second" output.
                //   2. The NVIDIA stub DRM node has no real GLES2/GBM support;
                //      the gles2/GBM path hangs the whole OS at GL FBO
                //      creation (see xtask write_labwc_config comment).
                // Restricting to card0 (the console GPU that has the UEFI
                // framebuffer) prevents both issues. Also set in /etc/profile
                // and the /usr/local/bin/labwc wrapper, but only this kernel-
                // env entry is visible to labwc when launched from a non-login
                // shell or directly from an init script.
                "WLR_DRM_DEVICES=/dev/dri/card0".into(),
                // Last-resort software-GL override (kept commented). On NVIDIA
                // WITHOUT the nvidia.nouveau_uapi flag the DRM node identifies
                // as "zcore" and Mesa finds no hardware driver, so a GL client
                // already falls to llvmpipe on its own; WITH the flag the
                // nouveau uAPI (drivers/src/display/nouveau_uapi.rs) is live and
                // hardware GL is intended. These force Mesa's KMS software
                // rasteriser (kms_swrast → llvmpipe) explicitly, only needed on
                // a box where even that autodetect misbehaves.
                //"GALLIUM_DRIVER=llvmpipe".into(),
                //"MESA_LOADER_DRIVER_OVERRIDE=kms_swrast".into(),
            ];
            let rootfs = fs::rootfs();
            // The root filesystem is mounted. Its own mark, because the 90->95
            // stretch used to be the biggest unexplained gap in the boot
            // timeline and it holds two completely different things: mounting
            // the root (a disk, or unpacking the initramfs) and then a long run
            // of cmdline switches that only set flags. Separating them says
            // which of the two to look at.
            kernel_hal::console::early_progress_bar(91);
            // Load hunter's /etc/hunter/{whitelist,blacklist} from the root fs
            // and enable exec learning (trust-on-first-use). Safe if absent.
            linux_object::fs::hunter_config::load(&rootfs.root_inode());
            // Real NVIDIA GSP-RM firmware (see xtask's nvidia_firmware and
            // nvidia-rm-sys/vendor/eclipse_rm_init.c): the display driver
            // runs during early PCI enumeration, before any filesystem
            // exists, so it can't read this itself -- pushed down here,
            // right after rootfs mount, to every registered DRM driver
            // (only the real NVIDIA one does anything with it). Missing
            // file (fetch failed at image-build time, or a non-NVIDIA
            // build) is a normal, silent no-op.
            // Thread identity for the NVIDIA RM: deliberately NOT registered.
            // The RM keys its API-lock reentrancy guard and threadState
            // tracking on `os_get_current_thread`, and hardware tried both
            // regimes: with REAL per-thread ids the RM entered blocking
            // paths this port never validated -- one boot froze at the first
            // GPU probe, and serializing the calls (RM_CALL_GATE) only
            // turned the freeze into every RM-backed ioctl failing fast
            // (vkCreateDevice -13 before the compositor drew anything).
            // With the stub's constant id 0 the RM ran a whole bring-up --
            // enumeration, channels, VM_BIND, EXEC with confirmed fence --
            // flawlessly; its one failure mode (0x2f when two threads
            // overlapped inside the API) is now impossible because
            // RM_CALL_GATE serializes every ioctl-time RM call. A serialized
            // RM is exactly what one constant thread id describes, so the
            // stub is no longer a lie -- it is the truth of this port.
            // (`kernel_hal::drivers::set_rm_thread_id_provider` stays for
            // the day the gate is lifted and RM concurrency is done right.)
            load_nvidia_gsp_firmware(&rootfs.root_inode());
            // Latch the nouveau GEM CPU-mapping cache policy before any GEM
            // object can be created (the first mmap fixes a VMO's policy for
            // its whole life).
            linux_object::fs::devfs::drm::init_gem_cache_policy();
            // Auto bring-up every COMPUTE GPU (any NVIDIA GPU not driving the
            // boot display) now that the GSP firmware is available, so the
            // copy-engine present path (ce_present over PCIe P2P) is ready
            // before the compositor starts -- no manual `cat /proc/gpustep5;6;8;9`.
            // Runs once, synchronously, before any userspace/scanout touches RM.
            // Per-frame CE-offloaded present: when a compute GPU was brought up
            // at boot (dual RTX: render GPU P2P-copies into the console GOP FB),
            // enable CE present automatically — it replaces the ~7-10 FPS CPU
            // blit path with a GPU copy-engine transfer. Explicit opt-in via
            // `nvidia.cepresent` still works; the wlroots GPU-renderer opts
            // (`nvidia.wlr_gles2` / `nvidia.wlr_vulkan`) also request it, since
            // without CE the compositor can render on the secondary RTX yet
            // still present through the slow CPU scanout path. Opt-out with
            // `nvidia.nocepresent`. On failure CE auto-wedges and falls back to
            // CPU blit (see CE_PRESENT_WEDGED in nvidia.rs).
            let ce_ready = if !kernel_hal::cmdline::flag(&options.cmdline, "nvidia.noautoboot") {
                auto_bringup_compute_gpus()
            } else {
                klog_info!("Eclipse: NVIDIA compute-GPU auto bring-up disabled (nvidia.noautoboot)");
                false
            };
            // The user-facing `nvidia.*` flags, read once and in one place
            // (`kernel_hal::cmdline::NvidiaParams`, also behind
            // `/sys/module/nvidia/parameters`).
            let nv = kernel_hal::cmdline::NvidiaParams::parse(&options.cmdline);
            let cmdline_has = |token: &str| {
                options
                    .cmdline
                    .split([':', ' ', '\t', '\n'])
                    .any(|t| t == token)
            };
            let ce_requested_by = if cmdline_has("nvidia.cepresent") {
                Some("nvidia.cepresent")
            } else if nv.wlr_vulkan {
                Some("nvidia.wlr_vulkan")
            } else if nv.wlr_gles2 || nv.nouveau_uapi {
                // GLES2/zink is the default session with nouveau_uapi; CE
                // present avoids the CPU blit under that path.
                Some("nvidia.nouveau_uapi (GPU compositor)")
            } else if ce_ready {
                Some("auto: compute GPU ready")
            } else {
                None
            };
            if cmdline_has("nvidia.nocepresent") {
                klog_info!("Eclipse: present por CPU (CE-offload disabled: nvidia.nocepresent)");
            } else if let Some(reason) = ce_requested_by {
                linux_object::fs::devfs::drm::set_ce_present_enabled(true);
                klog_info!("Eclipse: NVIDIA CE-offload present ENABLED ({})", reason);
            } else {
                klog_info!("Eclipse: present por CPU (CE-offload: no compute GPU ready; try nvidia.cepresent)");
            }
            // NOTE: do NOT auto-bring-up the CONSOLE GPU on the boot path.
            // Deferred bring-up is scheduled AFTER 100% (see
            // schedule_deferred_console_bringup) with scanout paused.
            if nv.hwcursor {
                linux_object::fs::devfs::drm::set_hw_cursor_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.hwcursor ON — bring-up diferido tras escritorio \
                     (scanout pausado); cursor software hasta entonces"
                );
            }
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.console_gpu") {
                klog_info!(
                    "Eclipse: nvidia.console_gpu ON — bring-up diferido de la GPU de consola \
                     tras escritorio; si sube, el present pasa a su propio copy engine"
                );
            }
            // Opt-in CE present on DRM page_flip (GOP via copy-engine). Also
            // auto-on with nouveau_uapi (GPU compositor default) unless
            // `nvidia.nohwflip`. Pair with CE present / dual-GPU bring-up.
            if cmdline_has("nvidia.hwflip")
                || (nv.nouveau_uapi && !cmdline_has("nvidia.nohwflip"))
            {
                kernel_hal::drivers::set_hwflip_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.hwflip ON — page_flip intentará CE present al GOP"
                );
            }
            // NVC57E ISO surfaceflip: auto with nouveau_uapi (scanout from
            // VRAM without CPU blit) unless `nvidia.nosurfaceflip`.
            if cmdline_has("nvidia.surfaceflip")
                || (nv.nouveau_uapi && !cmdline_has("nvidia.nosurfaceflip"))
            {
                kernel_hal::drivers::set_surfaceflip_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.surfaceflip ON — page_flip intentará NVC57E ISO (VRAM)"
                );
            }
            // Atomic modesetting uAPI (DRM_CLIENT_CAP_ATOMIC +
            // DRM_IOCTL_MODE_ATOMIC) is OPT-IN while the legacy-KMS path
            // remains the one proven on real hardware — same rollout nouveau
            // used (`nouveau.atomic=1`). Boot with `drm.atomic` to let
            // compositors take the atomic path; without it they fall back to
            // legacy KMS exactly as before.
            if kernel_hal::cmdline::flag(&options.cmdline, "drm.atomic") {
                linux_object::fs::devfs::drm::set_atomic_enabled(true);
                klog_info!("Eclipse: DRM atomic modesetting ENABLED (drm.atomic)");
            }
            // `DRM_MASTER` on the modeset ioctls (`drm_ioctl_permit`: EACCES
            // to anyone but the current master) is OPT-IN too: switched on
            // blind it could leave a compositor that is not the first open of
            // card0 without a screen. Boot with `drm.master_check`; AUTH_MAGIC
            // and the leases demand the master either way.
            if kernel_hal::cmdline::flag(&options.cmdline, "drm.master_check") {
                linux_object::fs::devfs::drm_scheme::set_master_check_enabled(true);
                klog_info!("Eclipse: DRM_MASTER check ENABLED (drm.master_check)");
            }
            // Sincronizacion implicita en los buffers que un proceso presta
            // a otro, ENCENDIDA por defecto: un EXEC espera a lo que el dueno
            // del buffer tenga en vuelo antes de leerlo, que es lo que en
            // Linux hace el `dma_resv` del objeto. Sin ella, un compositor que
            // muestrea el frame recien dibujado de un cliente no esperaba a
            // nadie salvo que ambos hablasen `linux-drm-syncobj-v1`, y el
            // frame salia a medio pintar. `nvidia.implicit_sync=off` vuelve al
            // comportamiento anterior sin recompilar.
            if kernel_hal::cmdline::is_off(&options.cmdline, "nvidia.implicit_sync") {
                kernel_hal::drivers::set_implicit_sync_enabled(false);
                klog_info!(
                    "Eclipse: nvidia.implicit_sync=off -- un EXEC solo espera los syncobj que \
                     nombra su cliente (puede salir un frame a medio dibujar)"
                );
            }
            // Pre-present fence wait, ON by default: a legacy SETCRTC /
            // PAGE_FLIP waits for the GPU to finish writing the buffer before
            // it is scanned out, the way the atomic path already waits on
            // IN_FENCE_FD. `drm.flip_fence=off` restores the old behaviour
            // (present immediately, tear if the GPU is still drawing) without
            // a rebuild, in case the wait misbehaves on real hardware.
            if kernel_hal::cmdline::is_off(&options.cmdline, "drm.flip_fence") {
                linux_object::fs::devfs::drm::set_flip_fence_enabled(false);
                klog_info!(
                    "Eclipse: drm.flip_fence=off -- el present legacy NO espera al fence de \
                     dibujado (puede aparecer tearing)"
                );
            }
            // Diagnostico, apagado siempre salvo que se pida: cada present
            // lee la caja de dano una segunda vez y avisa si los pixeles
            // cambiaron mientras el kernel los copiaba, o sea si el cliente
            // sigue dibujando el buffer que acaba de presentar. Responde a una
            // sola pregunta -- si los pixeles ya venian mal -- y cuesta una
            // relectura por frame, asi que no se deja puesto.
            // Escotilla de salida, apagada: el puntero vuelve a leer el buffer
            // del cliente para saber lo que tapa, que es de donde salian los
            // rectangulos negros al abrir un menu. Solo tiene sentido si el
            // arreglo -- leer el panel -- sale caro en la apertura de verdad y el
            // puntero se arrastra; aqui no hay apertura que medir.
            if kernel_hal::cmdline::flag(&options.cmdline, "drm.cursor_from_client") {
                linux_object::fs::devfs::drm::set_cursor_from_client(true);
                klog_info!(
                    "Eclipse: drm.cursor_from_client ON -- el puntero lee nada mas el buffer \
                     del cliente, asi que los rectangulos negros de los menus vuelven"
                );
            }
            if kernel_hal::cmdline::flag(&options.cmdline, "drm.present_probe") {
                linux_object::fs::devfs::drm::set_present_probe_enabled(true);
                klog_info!(
                    "Eclipse: drm.present_probe ON -- cada present comprueba si el cliente \
                     sigue escribiendo el buffer que presento"
                );
            }
            // Y esto es el arreglo que sale de esa medida: cuando el origen se
            // mueve bajo la copia, volver a copiar las bandas que se movieron
            // en vez de dejar la baldosa rancia en el panel. No hace el frame
            // entero -- eso solo lo arregla la sincronizacion de arriba -- pero
            // quita la basura que se queda. Acotado a dos rondas.
            // Y esto es lo otro que se puede hacer desde aqui, pero por el
            // rendimiento y no por la basura: el compositor nunca manda caja de
            // dano, asi que cada frame paga la pantalla entera --- 8,1 MB, y en
            // las RTX la BAR1 sirve unos 42 MB/s, o sea ~99 ms por frame. Lo que
            // casi nunca cambia entre dos frames es la mayor parte de la
            // pantalla, asi que el present compara cada banda de 16 filas con lo
            // que ya puso en el panel y solo copia las que se movieron.
            if kernel_hal::cmdline::flag(&options.cmdline, "drm.present_skip") {
                linux_object::fs::devfs::drm::set_present_skip_enabled(true);
                klog_info!(
                    "Eclipse: drm.present_skip ON -- el present no vuelve a copiar \
                     las bandas de 16 filas que el panel ya tiene"
                );
            }
            // The stall table is ON by default: it only records a wait that
            // was going to park anyway, and what it buys is the one thing no
            // other counter here can say -- which wait a frozen window is
            // parked in, named in `dmesg` without anyone having to catch it
            // live. `stalls=off` turns it off.
            if kernel_hal::cmdline::is_off(&options.cmdline, "stalls") {
                linux_object::sync::stall::set_enabled(false);
                klog_info!(
                    "Eclipse: stalls=off -- una espera que no vuelve no sale en \
                     ningun contador ni en /proc/stalls"
                );
            }
            // Present repair is ON by default: GLES2/llvmpipe sessions (what
            // `renderer=auto` still picks on QEMU/VirtualBox) hand the kernel a
            // buffer whose tiles are still arriving, and there is no fence to
            // wait on. Re-copying the bands that moved under the blit is what
            // clears the menu garbage. `drm.present_repair=off` restores the
            // old "copy once and hope" behaviour.
            if kernel_hal::cmdline::is_off(&options.cmdline, "drm.present_repair") {
                linux_object::fs::devfs::drm::set_present_repair_enabled(false);
                klog_info!(
                    "Eclipse: drm.present_repair=off -- un present que pilla el origen \
                     moviendose deja las baldosas rancias en el panel"
                );
            }
            // DRM format modifiers: `DRM_CAP_ADDFB2_MODIFIERS` and an
            // `ADDFB2` that carries `DRM_MODE_FB_MODIFIERS`. OFF by default
            // because advertising them is a promise about what the present
            // can put on the panel, and the present is a LINEAR copy: a
            // block-linear framebuffer is declined at the flip, so a
            // compositor that negotiates one falls back rather than showing
            // garbage, but the fallback costs a swapchain test. Turn it on to
            // exercise the negotiation.
            if kernel_hal::cmdline::flag(&options.cmdline, "drm.scanout_modifiers") {
                linux_object::fs::devfs::drm::set_scanout_modifiers_enabled(true);
                klog_info!(
                    "Eclipse: drm.scanout_modifiers=on -- se anuncian modificadores; \
                     un framebuffer block-linear se declina en el flip hasta que el \
                     copy engine sepa destejerlo"
                );
            }
            // Nouveau-compatible driver-specific ioctl surface on the NVIDIA
            // DRM node (GETPARAM, CHANNEL_ALLOC, GEM_NEW/INFO, VM_INIT --
            // see drivers/src/display/nouveau_uapi.rs and
            // docs/README-nouveau-uapi.md for exactly what is and isn't
            // implemented yet). Off by default: untested against real
            // hardware in the sandbox that wrote it.
            //
            // The flag is a REQUEST; `nouveau_uapi_capable()` is the
            // CAPABILITY, and only `NvidiaGpu` reports it. Both must hold:
            // `GL=1` stamps `renderer=gl:nvidia.nouveau_uapi` into one cmdline
            // that is booted on the RTX box AND under QEMU, and honouring the
            // flag with no NVIDIA GPU registered would make QEMU's virtio-gpu
            // node answer `DRM_IOCTL_VERSION` with the name "nouveau" (so Mesa
            // loads NVK and aims the whole nouveau uAPI at a driver that
            // implements none of it), advertise `DRM_CAP_SYNCOBJ`/`_TIMELINE`,
            // and open the syncobj ioctl paths -- all of it on the software
            // desktop that is supposed to keep working. Same image, both
            // machines, and each gets only what its hardware can serve.
            // `nvidia.exec_rm`: make every nouveau-uAPI EXEC go through the RM
            // (lookup + BAR1 map + inline fence poll per submit) instead of
            // the direct-submit path. Only for A/B comparison; see
            // docs/README-nouveau-uapi.md ("Direct submit").
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.exec_rm") {
                kernel_hal::drivers::set_exec_fast_enabled(false);
                klog_info!(
                    "Eclipse: nvidia.exec_rm -- EXEC direct-submit path DISABLED (RM per-submit path, synchronous fence poll)"
                );
            }
            // `nvidia.compositor_ctx0`: keep the compositor on context 0 --
            // the singleton step16/step17 ladder, on the shared boot-time RM
            // client -- instead of giving it a context and a client of its own
            // like every other DRM client. Default off, which is what Linux
            // does and what lets `nvidia-smi` name the compositor at all: the
            // boot client is KERNEL + admin and the RM skips those when it
            // answers GPU_GET_PIDS. This flag is the way back without a
            // rebuild, because the compositor is the one process whose failure
            // takes the desktop down with it.
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.compositor_ctx0") {
                kernel_hal::drivers::set_compositor_ctx0(true);
                klog_info!(
                    "Eclipse: nvidia.compositor_ctx0 -- el compositor se queda en el contexto 0 (cliente del RM compartido; no saldra en la tabla de procesos de nvidia-smi)"
                );
            }
            if nv.nouveau_uapi {
                let capable = kernel_hal::drivers::all_drm()
                    .as_vec()
                    .iter()
                    .any(|d| d.nouveau_uapi_capable());
                if capable {
                    kernel_hal::drivers::set_nouveau_uapi_enabled(true);
                    klog_info!(
                        "Eclipse: nouveau-compatible uAPI ENABLED (nvidia.nouveau_uapi) -- see docs/README-nouveau-uapi.md"
                    );
                } else {
                    klog_info!(
                        "Eclipse: nvidia.nouveau_uapi pedido pero NO hay ninguna GPU NVIDIA registrada -- uAPI nouveau DESACTIVADA (el nodo DRM sigue identificandose como \"zcore\")"
                    );
                }
            }
            // On-demand console GSP bring-up (default OFF: use /proc/gpustep14).
            //
            // ONLY `nvidia.console_gsp` opens the gate here. `nvidia.console_gpu`
            // needs the same gate -- `ensure_console_gpu_brought_up` refuses
            // without it -- but it must NOT be opened this early: the gate is
            // also what lets the FIRST GPU client's `CHANNEL_ALLOC` run
            // `bringup_step14`, and that client can arrive as soon as the
            // desktop starts, well before the deferred task reaches
            // `set_scanout_paused(true)`. That is precisely the SEC2 STARTCPU
            // window with live scanout that the deferred path exists to avoid,
            // so `nvidia.console_gpu` opens the gate from inside the task
            // instead, once the pause is in place (see
            // `schedule_deferred_console_bringup`). Passing both flags is still
            // the way to ask for on-demand bring-up at the first client.
            // Narración de los gpustep en el arranque AUTOMATICO. Apagada por
            // defecto desde que se midió lo que cuesta: los pasos narran para
            // quien lee `cat /proc/gpustepN`, y en el arranque no la lee nadie
            // -- se capturaba cada línea del RM, se reformateaba entera y se
            // tiraba, y encima el live-echo la subía a ERROR para pintarla en
            // una consola que vive en el BAR1 de la tarjeta que se está
            // encendiendo. Con esta bandera vuelve tal cual estaba, para el
            // arranque en el que haga falta.
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.gputrace") {
                kernel_hal::drivers::set_gpu_trace_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.gputrace — narración completa de los gpustep en el \
                     arranque automático (más lenta: la GPU tarda más en estar lista)"
                );
            }
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.console_gsp") {
                kernel_hal::drivers::set_console_gsp_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.console_gsp -- console GPU GSP on-demand bring-up ENABLED"
                );
            }
            // A VRAM GEM_NEW object gets a CPU mapping through BAR1 instead of
            // coming back with map_handle=0. Default off because it publishes a
            // host address derived from BAR1's base plus the object's FBMEM
            // offset, and that arithmetic assumes BAR1 is a linear window onto
            // VRAM at offset 0 -- true of TU10x in the RM's default setup, not
            // verified on every board.
            // A compressed PTE kind reaches the page tables instead of being
            // swapped for its uncompressed pair, so the RM derives the comptag
            // line from the surface's physical address. Default off, and the
            // driver refuses it anyway unless the GPU reports a comptag mode
            // where that derivation holds -- there is a third mode where it
            // does not.
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.gem_comptags") {
                kernel_hal::drivers::set_gem_comptags_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.gem_comptags -- VM_BIND honours compressed PTE kinds"
                );
            }
            // The classic nvc0 (OpenGL) submission path actually submits,
            // instead of parsing the request and returning EOPNOTSUPP: every
            // listed buffer is mapped into the channel's VAS, the relocs are
            // patched, and each push goes to the ring. Default off because no
            // method stream from this path has run on real hardware yet.
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.gem_pushbuf") {
                kernel_hal::drivers::set_gem_pushbuf_enabled(true);
                klog_info!("Eclipse: nvidia.gem_pushbuf -- GEM_PUSHBUF submits for real");
            }
            // `nvidia.cuda` turns on every hatch below that CUDA needs, in
            // one go; each one still works by its own name, and its own
            // `=0` keeps it off under the umbrella.
            if nv.cuda {
                klog_info!("Eclipse: nvidia.cuda -- nvidia.nvidiactl and nvidia.uvm");
            }
            // The device files NVIDIA's own user-mode driver opens
            // (libnvidia-ml, so `nvidia-smi`), over the RM this kernel
            // carries. Default off: no NVIDIA user-mode driver has run on it
            // on real hardware yet.
            if kernel_hal::cmdline::grouped_flag(
                &options.cmdline,
                "nvidia.cuda",
                "nvidia.nvidiactl",
            ) {
                kernel_hal::drivers::set_nvidiactl_enabled(true);
                klog_info!("Eclipse: nvidia.nvidiactl -- /dev/nvidiactl and /dev/nvidia<N>");
            }
            // `/dev/nvidia-uvm`, without which CUDA's cuInit fails. Default
            // off under the hatch: when on, serves cuInit and cuCtxCreate's
            // UVM calls (and logs the first of every other command).
            if kernel_hal::cmdline::grouped_flag(&options.cmdline, "nvidia.cuda", "nvidia.uvm") {
                kernel_hal::drivers::set_nvidia_uvm_enabled(true);
                klog_info!("Eclipse: nvidia.uvm -- /dev/nvidia-uvm");
            }
            // labwc on Vulkan drives the card from two NVK devices at once;
            // each gets its own GPU context, as each file has its own VM in
            // Linux. GLES2 sessions keep one context per process unless asked.
            // `nvidia.ctx_per_file=0` keeps the old shape under Vulkan too.
            if kernel_hal::cmdline::grouped_flag(
                &options.cmdline,
                "nvidia.wlr_vulkan",
                "nvidia.ctx_per_file",
            ) {
                kernel_hal::drivers::set_ctx_per_file_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.ctx_per_file -- un contexto de GPU por fichero DRM \
                     (dos dispositivos NVK en un proceso ya no comparten VM ni anillo)"
                );
            }
            if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.gem_vram") {
                kernel_hal::drivers::set_gem_vram_enabled(true);
                klog_info!(
                    "Eclipse: nvidia.gem_vram -- buffers VRAM|GART en VRAM, mapeados por BAR1 \
                     (flip de nvkms sin copias)"
                );
            }
            // The GPU policy BEFORE init(1) is spawned, not after.
            //
            // `wait_for_gpu_ready` in eclipse-init holds a GPU session back
            // until `/proc/gpuready` stops saying `waiting`, and that file
            // answers `none` -- "start now" -- until
            // `set_console_bringup_expected` has run. This block used to sit
            // AFTER the spawn, so on real hardware init(1) read `none` at
            // 4.5 s, 0.2 s before the kernel got here and scheduled the task:
            // the compositor started, NVK asked a card whose RM was still
            // cold, got ENODEV, and wlroots fell back to pixman FOR THE REST
            // OF THE SESSION -- 3 s before that same card finished coming up.
            // Nothing here touches the processes below, so the only thing the
            // move changes is that userspace can no longer outrun the answer
            // it is about to ask for.
            // Either flag wants the console GPU up; the task runs once and
            // serves both (hardware cursor plane and/or local CE present).
            //
            // Linux attaches the RM to EVERY NVIDIA card at probe, the one
            // scanning out the console included -- that is why `nvidia-smi`
            // there lists both cards of a 1+1 box, graphics on one and
            // compute on the other. Eclipse left the console card cold and
            // the operator had to know about `nvidia.console_gpu` to get it,
            // so a dual-card box reported `2 GPU node(s), 1 with the RM up`
            // and NVML counted one GPU. On a MULTI-GPU box the bring-up is
            // now the default: it is the difference between the split Linux
            // gives and half a machine. What runs is the same deferred task
            // as before -- after the desktop, with scanout paused, never on
            // the boot progress path -- so the failure mode is unchanged and
            // the compute card is already up by then either way. Opt out
            // with `nvidia.noconsole_gpu`.
            //
            // A SINGLE-GPU box keeps the old default (off): there the console
            // card is the only card, and the SEC2 STARTCPU store that can
            // wedge takes the whole machine with no second card to fall back
            // on. That box still opts in with `nvidia.console_gpu`.
            let nvidia_cards = kernel_hal::drivers::all_drm().as_vec();
            let console_cards = nvidia_cards.iter().filter(|d| d.is_console_gpu()).count();
            let nvidia_cards = nvidia_cards
                .iter()
                .filter(|d| d.is_console_gpu() || d.is_compute_gpu())
                .count();
            let verdict = multigpu_verdict(&options.cmdline, nvidia_cards, console_cards);
            let console_gpu_by_default = verdict.is_multigpu()
                && !kernel_hal::cmdline::flag(&options.cmdline, "nvidia.noconsole_gpu");
            match verdict {
                MultiGpu::Declared => klog_info!(
                    "Eclipse: nvidia.multigpu — caja multi-GPU declarada, {} GPUs NVIDIA ({} \
                     escanean la consola); bring-up de la que escanea por defecto, como en \
                     Linux (apagalo con nvidia.noconsole_gpu)",
                    nvidia_cards,
                    console_cards
                ),
                MultiGpu::DeclaredButOneCard => klog_warn!(
                    "Eclipse: nvidia.multigpu declarado pero sólo se enumeró {} GPU NVIDIA — se \
                     trata como caja de una tarjeta. Con una sola, el store STARTCPU de SEC2 que \
                     puede colgar se lleva la máquina entera y no hay segunda a la que caer. \
                     Para subirla igual: nvidia.scanout_gpu (en el arranque) o nvidia.console_gpu \
                     (diferido)",
                    nvidia_cards
                ),
                MultiGpu::SeveralCardsUndeclared => klog_info!(
                    "Eclipse: {} GPUs NVIDIA ({} escanean la consola) y nvidia.multigpu NO está \
                     en la cmdline — se trata como multi-GPU igual, por lo que se enumeró. Lo \
                     normal es declararlo con nvidia.multigpu",
                    nvidia_cards,
                    console_cards
                ),
                MultiGpu::Single => {}
            }
            let want_console_gpu = console_gpu_by_default
                || kernel_hal::cmdline::flag(&options.cmdline, "nvidia.console_gpu");
            // One bring-up path for every card now; this only says whether the
            // card that scans out goes through it AT BOOT, while its GSP boot
            // still wedges at the SEC2 store.
            //
            // `nvidia.console_gpu` is deliberately NOT accepted as a spelling
            // of this. It already means something else and something that
            // works: the deferred task below, which waits for the desktop,
            // parks scanout and has finished with both cards up on real
            // hardware. Taking it as a synonym here would run the boot-time
            // attempt FIRST, wedge at `STARTCPU`, and the boot would never
            // reach the desktop the deferred task waits for -- so the one
            // recipe that works would be replaced by one that does not, under
            // the flag that used to select it. `nvidia.noconsole_gpu` still
            // beats this one too, so a cmdline written against the old default
            // leaves that card cold either way.
            kernel_hal::drivers::set_scanout_gpu_bringup(scanout_gpu_bringup_asked(
                &options.cmdline,
            ));
            // `/proc/gpuready` answers for the console card, and that card only
            // becomes usable because the task below brings it up. Tell the
            // reader whether anyone scheduled it: without this a box booted
            // with `nvidia.noconsole_gpu` answered `waiting` forever, and init
            // held the session back for its whole timeout on a bring-up nobody
            // had asked for.
            linux_object::fs::devfs::drm::set_console_bringup_expected(want_console_gpu);
            if nv.hwcursor || want_console_gpu {
                let reason = if kernel_hal::cmdline::flag(&options.cmdline, "nvidia.console_gpu") {
                    "nvidia.console_gpu"
                } else if want_console_gpu {
                    // Nobody asked by name: this is the multi-GPU default.
                    "GPU de consola (por defecto, caja multi-GPU)"
                } else {
                    "nvidia.hwcursor"
                };
                schedule_deferred_console_bringup(
                    reason,
                    !kernel_hal::cmdline::flag(&options.cmdline, "nvidia.nocepresent"),
                    // `nvidia.console_gpu` deliberately left the console-GSP
                    // gate shut at boot; the task opens it under the pause.
                    want_console_gpu && !kernel_hal::cmdline::flag(&options.cmdline, "nvidia.console_gsp"),
                );
            }
            if nv.nvkms {
                // The cursor is nvkms's cursor plane once it scans out the
                // console card, as nvidia-drm's is; until then, and on any
                // failure, the software cursor stays in charge.
                linux_object::fs::devfs::drm::set_hw_cursor_enabled(true);
                schedule_nvkms();
            }

            kernel_hal::console::early_progress_bar(95);

            // Whose exit takes the system down: INIT (PID 1) if present, else
            // the primary terminal's shell.
            let lifetime_proc = if !shell_args.is_empty() {
                // Spawn the SHELL on each virtual terminal (tty1..ttyN, reachable
                // via Ctrl+Alt+F1..F6) with a fixed PID 101.. — vt 0 is the
                // primary terminal and does the one-time boot work; the others
                // share its mounted root filesystem (by `Arc`, like `fork`).
                let mut shared_root = None;
                let mut primary_shell = None;
                let mut shell_fallback = None;
                for vt in 0..kernel_hal::console::NUM_VTS {
                    // The LAST VT (tty7) is reserved for the graphical session:
                    // no login shell there, so labwc/X11 owns it cleanly and the
                    // kernel text console never fights the compositor for the
                    // framebuffer. See `kernel_hal::console::GRAPHICS_VT`.
                    if vt == kernel_hal::console::GRAPHICS_VT {
                        continue;
                    }
                    let pid = (101 + vt) as u64;
                    let proc = zcore_loader::linux::run_shell_on_vt(
                        shell_args.clone(),
                        envs.clone(),
                        rootfs.clone(),
                        vt,
                        shared_root.clone(),
                        pid,
                    );
                    if let Some(proc) = proc {
                        if shell_fallback.is_none() {
                            shell_fallback = Some(proc.clone());
                        }
                        if vt == 0 {
                            shared_root = Some(proc.linux().root_inode().clone());
                            primary_shell = Some(proc);
                        }
                    } else {
                        warn!(
                            "SHELL {:?} failed on vt {}; continuing boot",
                            shell_args[0], vt
                        );
                    }
                }
                // Optionally run INIT as PID 1 (default /sbin/init -> eclipse-init,
                // busybox init fallback), if it exists. The shells already did
                // the boot work, so don't repeat.
                let init = zcore_loader::linux::run_init_if_present(
                    init_args,
                    envs.clone(),
                    rootfs.clone(),
                    shared_root,
                    primary_shell.is_none(),
                );
                init.or(primary_shell).or(shell_fallback)
            } else if !init_args.is_empty() {
                // No shells (e.g. libos): INIT is the single PID 1 program.
                Some(zcore_loader::linux::run(init_args, envs.clone(), rootfs.clone()))
            } else {
                None
            };
            // Make the outcome of PID 1 / base-program startup observable: log
            // which process the system's lifetime is now tied to (the PID 1
            // init when it came up, otherwise the fallback terminal shell).
            match &lifetime_proc {
                Some(p) => klog_info!(
                    "Eclipse: lifetime process pid={} name={:?}",
                    p.id(),
                    p.name()
                ),
                None => klog_info!("Eclipse: no lifetime process spawned"),
            }
            // Keep secondary CPUs idle until root is mounted and init is spawned.
            STARTED.store(true, Ordering::SeqCst);
            kernel_hal::console::early_progress_bar(100);
            // The kernel half of the boot, as a timeline. Printed here because
            // this is the last mark: by now init(1) is running, so the table is
            // complete and nothing is waiting on it. `dmesg` timestamps every
            // line it prints, which says when the kernel SPOKE; this says where
            // it spent the silence between two lines, which is the only figure
            // "make the boot faster" can start from.
            // The loader's marks are raw TSC readings; name the frequency to
            // divide them by now, not at `_start`, where the only figure
            // available was a CPUID guess. By here the device probe has checked
            // one against the ACPI PM timer.
            // `not(libos)` as well as the architecture: `kernel_hal::cpu` is
            // `bare/arch/x86_64/cpu.rs` on baremetal, which has `tsc_hz()`,
            // but `libos/cpu.rs` under the `libos` feature, which does not --
            // and a libos build on an x86_64 host satisfies `target_arch`
            // alone, so the guard without this broke `cargo build --package
            // zcore --features "linux libos"` outright.
            #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
            kernel_hal::boot_marks::set_loader_tsc_hz(kernel_hal::cpu::tsc_hz());
            for line in kernel_hal::boot_marks::render().lines() {
                // Both sinks, deliberately. `klog_info!` writes to the dmesg
                // ring ONLY, which is where one reads it on a machine that is
                // already up (`dmesg`, and the tail of `/proc/perf/kernel`);
                // the direct serial write is what puts it in a captured boot
                // log, which is the form a boot measurement is actually pasted
                // in. It does not go through `log::`, so `LOG=warn` -- the
                // level every image ships at -- cannot swallow it, the same
                // reason the oops and `[ipi-watch]` diagnostics bypass it.
                klog_info!("{}", line);
                kernel_hal::console::serial_write_str(line);
                kernel_hal::console::serial_write_str("\r\n");
            }
            #[cfg(all(feature = "linux", not(feature = "libos")))]
            {
                let n = TLBHAMMER_N.load(Ordering::Relaxed);
                if n > 0 {
                    tlb_hammer::start(n as usize);
                }
            }
            // Under `baremetal-test` the run ends when the lifetime process
            // does, not when this CPU happens to run out of work; see
            // `utils::reset_when_process_exits`.
            #[cfg(all(feature = "baremetal-test", not(feature = "libos")))]
            if let Some(proc) = lifetime_proc.clone() {
                utils::reset_when_process_exits(proc);
            }
            utils::wait_for_exit(lifetime_proc)
        } else if #[cfg(feature = "zircon")] {
            let zbi = fs::zbi();
            kernel_hal::console::early_progress_bar(95);
            let proc = zcore_loader::zircon::run_userboot(zbi, &options.cmdline);
            STARTED.store(true, Ordering::SeqCst);
            kernel_hal::console::early_progress_bar(100);
            utils::wait_for_exit(Some(proc))
        } else {
            panic!("One of the features `linux` or `zircon` must be specified!");
        }
    }
}

/// Reads NVIDIA GSP-RM firmware from the mounted rootfs and hands it to
/// every registered DRM driver (only the real NVIDIA one acts on it --
/// see `DrmScheme::set_gsp_firmware`). Missing file is a silent no-op:
/// happens for any non-NVIDIA build, or if xtask's firmware fetch failed
/// at image-build time (best-effort, see xtask/src/linux/nvidia_firmware.rs).
#[cfg(feature = "linux")]
fn load_nvidia_gsp_firmware(root: &alloc::sync::Arc<dyn rcore_fs::vfs::INode>) {
    use alloc::string::String;
    const PATH: &str = "/lib/firmware/nvidia/gsp/gsp.bin";

    // Push a status string to every DRM driver even on failure, so a driver
    // whose set_gsp_firmware never gets called can still report *why* in its
    // /proc/gpustep6 output (the kernel log is invisible on a headless-but-
    // monitored bring-up box). all_drm() is populated during primary_init()'s
    // PCI scan, well before this runs.
    fn report(status: String) {
        for d in kernel_hal::drivers::all_drm().as_vec().iter() {
            d.set_gsp_firmware_status(status.clone());
        }
    }

    let inode = match root.lookup(PATH) {
        Ok(i) => i,
        Err(e) => {
            report(alloc::format!("lookup({PATH}) failed: {e:?}"));
            return;
        }
    };
    let size = match inode.metadata() {
        Ok(m) => m.size,
        Err(e) => {
            report(alloc::format!("metadata failed: {e:?}"));
            return;
        }
    };
    if size == 0 {
        report(String::from("file is 0 bytes"));
        return;
    }

    // Read in bounded chunks and accumulate: a single read_at over a
    // multi-megabyte buffer can fail or short-read on some filesystems (SFS
    // walks block-by-block); looping is the robust way and also handles
    // partial reads (which the old one-shot read silently truncated to the
    // first chunk).
    let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(size);
    let mut chunk = alloc::vec![0u8; 256 * 1024];
    let mut off = 0usize;
    loop {
        match inode.read_at(off, &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                off += n;
                if off >= size {
                    break;
                }
            }
            Err(e) => {
                // Report how far we got -- pinpoints a bad block / read cap.
                report(alloc::format!(
                    "read_at(off={off}) failed after {} of {} bytes: {e:?}",
                    buf.len(),
                    size
                ));
                if buf.is_empty() {
                    return;
                }
                break;
            }
        }
    }

    if buf.is_empty() {
        report(String::from("read produced 0 bytes"));
        return;
    }

    let n = buf.len();
    klog_info!("Eclipse: loaded NVIDIA GSP-RM firmware ({} bytes)", n);
    let drms = kernel_hal::drivers::all_drm();
    let drm_count = drms.as_vec().len();
    for d in drms.as_vec().iter() {
        d.set_gsp_firmware(buf.clone());
    }
    report(alloc::format!(
        "OK: {n} of {size} bytes delivered to {drm_count} DRM driver(s)"
    ));
}

/// Boot-time auto bring-up of every COMPUTE GPU. Iterates all registered DRM
/// drivers and asks each to bring itself up if it does NOT drive the boot
/// display (`DrmScheme::auto_bringup_compute`, a no-op for the console GPU and
/// for non-NVIDIA drivers). Best-effort: each GPU logs its own outcome and a
/// failure never aborts boot. Called once, synchronously, right after the GSP
/// firmware load and before any userspace or scanout touches RM, so the
/// copy-engine present path (P2P from a compute GPU into the console's scanout
/// FB) is ready by the time the compositor runs. General over 1, 2, 3+ GPUs:
/// a single-console-GPU box brings up nothing and falls back to the CPU blit.
/// Returns true if at least one compute GPU is ready to offload present.
#[cfg(feature = "linux")]
fn auto_bringup_compute_gpus() -> bool {
    let mut summaries = alloc::vec::Vec::new();
    let mut any_ce_ready = false;
    // Suppress the driver's own (loud, ERROR-level) bring-up narration for the
    // whole ladder; `auto_bringup_compute` returns just one clean status line
    // per compute GPU (empty for the console GPU / non-NVIDIA drivers). The
    // detail is still captured into /proc/gpustep* for debugging.
    logging::with_output_suppressed(|| {
        for d in kernel_hal::drivers::all_drm().as_vec().iter() {
            if d.ce_present_ready() {
                any_ce_ready = true;
            }
            let line = d.auto_bringup_compute();
            if !line.is_empty() {
                summaries.push(line);
            }
        }
    });
    // Re-check after bring-up: auto_bringup_compute may have just state-loaded
    // the compute GPU(s).
    if !any_ce_ready {
        any_ce_ready = kernel_hal::drivers::all_drm()
            .as_vec()
            .iter()
            .any(|d| d.ce_present_ready());
    }
    // Emit only the tidy summary (klog_emit bypasses the level filter).
    // `summaries` now also carries the console GPU's "left cold, and here is
    // the hatch" line, so "sin GPU de cómputo" is decided by counting compute
    // GPUs, not by the summary being empty -- otherwise a box whose only
    // NVIDIA card is the console one would claim a compute GPU it has not got.
    let compute_gpus = kernel_hal::drivers::all_drm()
        .as_vec()
        .iter()
        .filter(|d| d.is_compute_gpu())
        .count();
    for line in summaries {
        klog_info!("Eclipse: NVIDIA {}", line);
    }
    if compute_gpus == 0 {
        klog_info!("Eclipse: NVIDIA — sin GPU de cómputo; present por CPU");
    }
    any_ce_ready
}

/// An operator notice that has to be READ, not logged: it goes to the active
/// VT (so it is on the screen of the machine it is about) and to the dmesg
/// ring, and never through `log::`, whose level `LOG=error` sets to filter
/// everything below an error. Used by the deferred console-GPU bring-up to
/// bracket the stretch where the screen deliberately stops moving.
/// Ask every card for its display heads and put the answer in the klog.
///
/// One place, because the point is to read the SAME four lines at several
/// points and compare them: a head that was `ADVANCING` before the unpause and
/// is `SLEEP` after says the display engine was torn down, and one that is
/// still `ADVANCING` says the raster survived and the fault is in what it
/// reads or in the link. Neither is answerable from a log that stops before
/// the unpause, which is where it stopped.
#[cfg(feature = "linux")]
fn log_scanout_liveness(phase: &str) {
    for d in kernel_hal::drivers::all_drm().as_vec().iter() {
        for line in d.scanout_liveness(phase).lines() {
            kernel_hal::klog_info!("{}", line);
        }
    }
}

#[cfg(feature = "linux")]
fn console_notice(msg: &str) {
    use kernel_hal::console::{active_vt, serial_vt, vt_console_write_str};
    klog_info!("{}", msg);
    // Both the VT on the monitor and the serial-bound one: with a compositor
    // holding the graphics VT those are different terminals, and the notice
    // has to reach whichever one the operator is actually looking at.
    let active = active_vt();
    let serial = serial_vt();
    let both = [active, serial];
    let vts = if active == serial {
        &both[..1]
    } else {
        &both[..]
    };
    for vt in vts {
        vt_console_write_str(*vt, msg);
        vt_console_write_str(*vt, "\r\n");
    }
}

/// After the progress bar hits 100% and labwc can start: wait for KD_GRAPHICS,
/// pause scanout (so BAR1 is quiet), run console-GPU gpustep14 once, then
/// resume. Never blocks boot, and never runs on the boot progress path — a
/// wedged SEC2 STARTCPU store there would hang the machine with nothing on
/// screen, whereas here the desktop is already up and the worst case is that
/// the console GPU stays cold.
///
/// Two flags land here and the task serves both (`reason` only names which one
/// asked, for the log):
///
/// * `nvidia.hwcursor` — on success the next `MODE_CURSOR` can take the
///   hardware plane; on failure the software cursor stays.
/// * `nvidia.console_gpu` — on success the console GPU can present its own
///   framebuffer with its own copy engine. On a single-GPU box this is the
///   only route to any GPU-accelerated present at all: there is no second card
///   to P2P from, so `auto_bringup_compute_gpus` brings up nothing and every
///   frame is a CPU blit.
///
/// `may_enable_ce` is false when the operator passed `nvidia.nocepresent`; the
/// bring-up still runs (the cursor plane is a separate win) but the present
/// path is left alone.
///
/// `open_console_gsp` is true for `nvidia.console_gpu` alone, which leaves the
/// console-GSP gate shut at boot on purpose: that gate is shared with the
/// first-GPU-client bring-up in `CHANNEL_ALLOC`, so opening it at boot would
/// let a client start SEC2 STARTCPU with scanout live -- exactly what this task
/// exists to avoid. The task opens it itself, after the pause is in place.
/// `nvidia.nvkms`: once every card that is coming up has its RM, load
/// nvidia-modeset and give it each one, then run its timers for the rest of
/// the boot -- Linux runs those from nvidia-modeset's kthread. The console
/// card is RM-attached only after the deferred bring-up above, so this waits
/// for it when one was scheduled; the other cards have theirs since boot.
#[cfg(feature = "linux")]
fn schedule_nvkms() {
    klog_info!("Eclipse: nvidia.nvkms — nvkms se carga cuando las GPUs tengan el RM");
    kernel_hal::thread::spawn(async move {
        use core::time::Duration;
        use kernel_hal::timer::timer_now;

        let deadline = timer_now() + Duration::from_secs(180);
        while linux_object::fs::devfs::drm::console_bringup_unfinished() && timer_now() < deadline {
            kernel_hal::thread::sleep_until(timer_now() + Duration::from_millis(500)).await;
        }
        for d in kernel_hal::drivers::all_drm().as_vec().iter() {
            for line in d.nvkms_load() {
                kernel_hal::klog_info!("Eclipse: [nvkms] {}", line);
            }
        }

        // Between 1 ms and 100 ms: the cap is for a timer a later ioctl adds
        // while this task sleeps, since nothing wakes it.
        loop {
            let mut owned = false;
            let mut next_us = u64::MAX;
            for d in kernel_hal::drivers::all_drm().as_vec().iter() {
                if let Some(us) = d.nvkms_service() {
                    owned = true;
                    next_us = next_us.min(us);
                }
            }
            if !owned {
                break;
            }
            let wait = Duration::from_micros(next_us.clamp(1_000, 100_000));
            kernel_hal::thread::sleep_until(timer_now() + wait).await;
        }
    });
}

#[cfg(feature = "linux")]
fn schedule_deferred_console_bringup(
    reason: &'static str,
    may_enable_ce: bool,
    open_console_gsp: bool,
) {
    klog_info!(
        "Eclipse: {} — tarea diferida programada (espera desktop, luego gpustep14)",
        reason
    );
    kernel_hal::thread::spawn(async move {
        use core::time::Duration;
        use kernel_hal::console::{kd_mode, KD_GRAPHICS};
        use kernel_hal::timer::timer_now;

        // Wait up to ~45s for the compositor to claim the VT (first present
        // sets KD_GRAPHICS). Poll every 500ms so we do not busy-spin.
        let deadline = timer_now() + Duration::from_secs(45);
        let mut saw_graphics = false;
        let mut asked = false;
        while timer_now() < deadline {
            if kd_mode() == KD_GRAPHICS {
                saw_graphics = true;
                break;
            }
            // ...or userspace asked whether this card is usable yet, which is
            // the same news arriving earlier. Init holds a GPU compositor back
            // until `/proc/gpuready` says the card serves, so waiting here for
            // a present that cannot happen until we run is a wait on
            // ourselves: both ends only ended on their timeouts, and the
            // session started on pixman anyway. See `drm::gpu_ready_was_read`.
            if linux_object::fs::devfs::drm::gpu_ready_was_read() {
                asked = true;
                break;
            }
            kernel_hal::thread::sleep_until(timer_now() + Duration::from_millis(500)).await;
        }
        if asked {
            // The read arrives when init reaches the session, which is when
            // labwc would have started -- about 4 s EARLIER than the bring-up
            // started in the one recipe that has finished with both cards up
            // (labwc at 6.9 s, KD_GRAPHICS at 7.3 s, the 3 s window below,
            // STARTCPU back in 2.3 s at 10.7 s). Starting on the read itself
            // put STARTCPU at 6.9 s, and on a dual-2060 box it never came back
            // there, every boot: the same early hour #2186 reached by another
            // route (see the window below). So the read wakes the task, but
            // the bring-up keeps the hour that works. Init is waiting on
            // `/proc/gpuready` meanwhile, so these seconds cost no frames.
            kernel_hal::thread::sleep_until(timer_now() + Duration::from_secs(4)).await;
            kernel_hal::klog_info!(
                "Eclipse: {} — userspace pregunto por /proc/gpuready; bring-up consola a la hora de siempre (scanout pausado)",
                reason
            );
        } else if saw_graphics {
            // Let labwc finish its first few frames before freezing scanout.
            //
            // This window is NOT cut short for a client parked on this card,
            // although that client cannot draw anything and there are no
            // frames left to protect. Cutting it was tried (#2186) and it
            // cost a dual-2060 box its monitor: the parked client arrives
            // before `KD_GRAPHICS`, so the bring-up started at 6.8 s instead
            // of 10.7 s, and the SEC2 `STARTCPU` store that completed in
            // 2.3 s at the later moment did not come back at all at the
            // earlier one -- the watchdog lifted the pause 90 s later and the
            // monitor never returned. What this window is really holding off
            // is not understood well enough to move it, and the thing it
            // guards is the one recipe that has ever finished with both cards
            // up. The parked client costs nothing by waiting: it is asleep in
            // its ioctl until the card attaches, which is the whole point of
            // `DrmDev::console_bringup_sleep`. So this stays a plain sleep,
            // and the line below only REPORTS whether anyone is parked, so
            // the next boot's log says so.
            kernel_hal::thread::sleep_until(timer_now() + Duration::from_secs(3)).await;
            kernel_hal::klog_info!(
                "Eclipse: {} — desktop en KD_GRAPHICS{}; pausando scanout e iniciando bring-up consola",
                reason,
                if linux_object::fs::devfs::drm::cold_client_waiting() {
                    " (con un cliente GPU ya aparcado esperando esta tarjeta)"
                } else {
                    ""
                }
            );
        } else {
            kernel_hal::klog_info!(
                "Eclipse: {} — timeout esperando KD_GRAPHICS; intentando bring-up igual (scanout pausado)",
                reason
            );
        }

        // Say it on the screen, before the screen stops moving. The pause
        // below freezes scanout for as long as a cold GSP boot plus a
        // state-load takes -- tens of seconds -- and the last thing the
        // console shows while it runs is the RM's own narration stopping at
        // `STARTCPU: ... about to post store`. On a real dual-2060 box that
        // read, correctly, as a dead machine: a still screen and a log that
        // ends mid-sequence is exactly what a wedge looks like. It was a
        // 46-second wait that finished with both cards up. This notice and
        // the one after the pause are what tell those two apart, so they go
        // straight to the VT and to dmesg instead of through `log::`, which
        // `LOG=error` -- the level every image boots at -- would swallow.
        //
        // And paint the kernel console over the compositor's VT while this
        // runs.
        // Without that a desktop box shows the notices to nobody: KD_GRAPHICS
        // suppresses presentation, which is right while labwc is drawing and
        // wrong for the one stretch where labwc is deliberately NOT drawing.
        // Restored to whatever `console.overgraphics` left it at.
        let over_graphics_was = kernel_hal::console::diag_present_over_graphics();
        kernel_hal::console::set_diag_present_over_graphics(true);
        console_notice(&alloc::format!(
            "[gpu-consola] subiendo la GPU de consola ({}): el GSP arranca en frio y \
             la pantalla se queda QUIETA hasta {} s. No es un cuelgue. \
             Para no hacerlo: nvidia.noconsole_gpu en la cmdline.",
            reason,
            linux_object::fs::devfs::drm::SCANOUT_PAUSE_MAX.as_secs(),
        ));
        let paused_at = timer_now();

        // Watchdogged: `bringup_step14` drives the SEC2 STARTCPU path, which is
        // the one known way this hardware wedges, and a latched pause freezes
        // the desktop forever (flips keep being acknowledged, nothing is
        // drawn). The pause lifts by itself if we never get back here.
        linux_object::fs::devfs::drm::set_scanout_paused_for(
            linux_object::fs::devfs::drm::SCANOUT_PAUSE_MAX,
        );
        // And the kernel console with it: the notice above is the last thing
        // drawn until the window closes. The state load inside rebinds the
        // console card's BAR1, which is its framebuffer, and a console present
        // landing in the bind left BAR1 faulted for the rest of the boot (every
        // channel on that card dead, nvkms's LUT unreadable). The RM's
        // narration still goes to dmesg; the screen was going to stand still
        // anyway. Pushed out first: a notice that fell inside the console's
        // present throttle is still pending, and a hold drops pending presents.
        kernel_hal::console::redraw_active_console();
        kernel_hal::console::hold_presents_for(linux_object::fs::devfs::drm::SCANOUT_PAUSE_MAX);
        // Brief settle so in-flight blits/CE finish before SEC2.
        kernel_hal::thread::sleep_until(timer_now() + Duration::from_millis(200)).await;

        // Only now: the gate that `ensure_console_gpu_brought_up` checks is the
        // same one a GPU client's CHANNEL_ALLOC uses, so it stays shut until
        // scanout is parked. From here on a first client racing us is harmless:
        // the bring-up is one-shot and the pause is already ours.
        if open_console_gsp {
            kernel_hal::drivers::set_console_gsp_enabled(true);
            kernel_hal::klog_info!(
                "Eclipse: {} — gate console-GSP abierto con el scanout ya pausado",
                reason
            );
        }

        // Every registered driver is asked; the method self-guards to the GPU
        // that drives the boot display and returns an empty line otherwise, so
        // this is correct for 1, 2 or N cards without counting them here.
        let mut any = false;
        for d in kernel_hal::drivers::all_drm().as_vec().iter() {
            let line = d.deferred_console_bringup();
            if !line.is_empty() {
                any = true;
                kernel_hal::klog_info!("Eclipse: NVIDIA {}", line);
            }
        }
        if !any {
            kernel_hal::klog_info!(
                "Eclipse: {} — ninguna GPU consola actuó en bring-up diferido",
                reason
            );
        }

        kernel_hal::console::release_presents();
        linux_object::fs::devfs::drm::set_scanout_paused(false);

        // The other half of the pair above: the screen moves again, and this
        // says how long it did not.
        let held = timer_now().saturating_sub(paused_at);
        console_notice(&alloc::format!(
            "[gpu-consola] bring-up terminado en {}.{:03} s; el scanout vuelve",
            held.as_secs(),
            held.subsec_millis(),
        ));
        kernel_hal::console::set_diag_present_over_graphics(over_graphics_was);
        // No-op on a VT a compositor owns: the panel already got its frame
        // back inside `set_scanout_paused(false)`, and painting the text
        // console over it is what left the screen on the boot log with labwc
        // running behind it. See `redraw_graphic_console_impl`.
        kernel_hal::console::redraw_active_console();
        // The three reads nobody had. Every head dump the bring-up takes is
        // from BEFORE this point, so when the screen went wrong here the log
        // had nothing to say about it.
        log_scanout_liveness("post-unpause");

        // The GR ladder runs HERE, with the screen moving again, never inside
        // the window above: step16/step17 drive the compute engine's cold
        // golden-context load on the card that scans out, and doing that with
        // the scanout parked is how a boot lost the monitor. Nothing waits on
        // it -- a client whose context could not be built yet retries on its
        // next ioctl.
        for d in kernel_hal::drivers::all_drm().as_vec().iter() {
            let line = d.build_console_gr_ladder();
            if !line.is_empty() {
                kernel_hal::klog_info!("Eclipse: NVIDIA {}", line);
            }
        }
        log_scanout_liveness("post-ladder");

        // The boot-time CE decision was taken before this GPU existed as a
        // presenter, so re-take it now. On a single-GPU box this is the moment
        // present stops being a CPU blit; on a dual-GPU box CE is normally
        // already on and this is a no-op. Only ever turns CE ON: an operator
        // who asked for `nvidia.nocepresent`, or a CE path that wedged itself
        // off earlier, is not overridden from here.
        if may_enable_ce && !linux_object::fs::devfs::drm::ce_present_enabled() {
            let now_ready = kernel_hal::drivers::all_drm()
                .as_vec()
                .iter()
                .any(|d| d.ce_present_ready());
            if now_ready {
                linux_object::fs::devfs::drm::set_ce_present_enabled(true);
                kernel_hal::klog_info!(
                    "Eclipse: NVIDIA CE-offload present ENABLED (auto: GPU lista tras bring-up diferido)"
                );
            }
        }

        // Last, and it matters whether the bring-up worked or not: an ioctl
        // parked in `console_bringup_sleep` waits for a bring-up that is still
        // coming. Past here none is, so a card that did not attach must refuse
        // its clients at once again instead of making each one pay the whole
        // timeout. See `drm::console_bringup_unfinished`.
        linux_object::fs::devfs::drm::note_console_bringup_done();

        // Only the hwcursor request has a "now move the mouse" follow-up; a
        // console_gpu request has nothing for the operator to do.
        if reason == "nvidia.hwcursor" {
            kernel_hal::klog_info!(
                "Eclipse: nvidia.hwcursor — bring-up diferido terminado; mueve el ratón para activar el plano HW si RM quedó listo"
            );
        } else {
            kernel_hal::klog_info!("Eclipse: {} — bring-up diferido terminado", reason);
        }
    });
}

#[cfg(not(feature = "libos"))]
fn secondary_main() -> ! {
    // Bring up this AP (trapframe/GS/LAPIC) before STARTED so the BSP can wait
    // for AP_ONLINE during SMP init. cpu_id() uses the trampoline logical id
    // until GS is configured (APIC id from CPUID is wrong on many APs).
    kernel_hal::secondary_init();
    while !STARTED.load(Ordering::SeqCst) {
        core::hint::spin_loop();
    }
    klog_info!("Eclipse: CPU {} online", kernel_hal::cpu::cpu_id());
    #[cfg(feature = "mock-disk")]
    {
        if MOCK_CORE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            utils::mock_disk();
        }
    }
    utils::wait_for_exit(None)
}

#[cfg(test)]
mod tests {
    use super::{deadlock_spins, multigpu_verdict, scanout_gpu_bringup_asked, MultiGpu};

    /// The line a live Eclipse boots with, which mentions no threshold.
    const REAL: &str = "LOG=error:TERM=xterm-256color:console.shell=true:ROOT=/dev/sda2";

    #[test]
    fn the_count_written_is_the_count_asked_for() {
        assert_eq!(
            deadlock_spins("DEADLOCKSPINS=20000000"),
            Some(Ok(20_000_000))
        );
        // The colon ends the number, and `cmdline` is what knows that.
        assert_eq!(
            deadlock_spins("LOG=error:DEADLOCKSPINS=20000000:ROOT=/dev/sda2"),
            Some(Ok(20_000_000))
        );
    }

    #[test]
    fn a_threshold_written_in_hex_is_the_number_it_spells() {
        // This is the one that cost something: the old parser stopped at the
        // `x` and read `0`, `set_deadlock_spins(0)` restores the default the
        // knob exists to lower, and the klog said "threshold set to 0". The
        // detector then never fired on an emulated run -- which is the failure
        // the knob was added for in the first place.
        assert_eq!(
            deadlock_spins("DEADLOCKSPINS=0x4000000"),
            Some(Ok(0x400_0000))
        );
        assert_eq!(
            deadlock_spins("DEADLOCKSPINS=0X4000000"),
            Some(Ok(0x400_0000))
        );
    }

    #[test]
    fn a_line_that_never_mentioned_the_knob_says_so() {
        // Distinct from a knob written with nonsense: nothing to report.
        assert_eq!(deadlock_spins(REAL), None);
        assert_eq!(deadlock_spins(""), None);
    }

    #[test]
    fn a_value_that_is_not_a_count_is_reported_rather_than_invented() {
        // `take_while(is_ascii_digit)` answered each of these with the digits
        // it happened to start with, and set that.
        assert_eq!(deadlock_spins("DEADLOCKSPINS=lots"), Some(Err("lots")));
        assert_eq!(
            deadlock_spins("DEADLOCKSPINS=20000000spins"),
            Some(Err("20000000spins"))
        );
        assert_eq!(deadlock_spins("DEADLOCKSPINS=-1"), Some(Err("-1")));
        // Naming the knob and writing no count at all is a typo too.
        assert_eq!(deadlock_spins("LOG=error:DEADLOCKSPINS"), Some(Err("")));
    }

    #[test]
    fn a_count_of_zero_is_a_count_and_means_the_default() {
        // `set_deadlock_spins` documents `0` as "restore the default", so this
        // is a real answer and not the misparse above.
        assert_eq!(deadlock_spins("DEADLOCKSPINS=0"), Some(Ok(0)));
    }

    #[test]
    fn the_name_inside_a_longer_key_is_not_the_knob() {
        assert!(
            "X.DEADLOCKSPINS=9".contains("DEADLOCKSPINS="),
            "which is what the old parser asked"
        );
        assert_eq!(deadlock_spins("X.DEADLOCKSPINS=9"), None);
        assert_eq!(deadlock_spins("NODEADLOCKSPINS=9"), None);
    }

    #[test]
    fn the_name_inside_somebody_elses_value_is_not_the_knob() {
        assert_eq!(
            deadlock_spins("ROOT=/dev/disk/by-id/DEADLOCKSPINS=9-part2"),
            None
        );
        assert_eq!(deadlock_spins("TERM=DEADLOCKSPINS=9"), None);
    }

    #[test]
    fn the_key_is_case_insensitive_like_every_other_one() {
        assert_eq!(deadlock_spins("deadlockspins=5"), Some(Ok(5)));
        assert_eq!(deadlock_spins("DeadlockSpins=5"), Some(Ok(5)));
    }

    #[test]
    fn the_spaces_a_person_leaves_around_the_count_are_not_part_of_it() {
        // The old parser wanted a digit where the space was, so the knob it
        // reported as absent was one somebody had just written.
        assert_eq!(
            deadlock_spins("LOG=error: DEADLOCKSPINS = 20000000 "),
            Some(Ok(20_000_000))
        );
    }

    /// `nvidia.scanout_gpu` is the only flag that puts the card that scans out
    /// through the boot-time bring-up.
    #[test]
    fn the_boot_time_bring_up_of_the_scanout_card_is_asked_for_by_one_name() {
        assert!(scanout_gpu_bringup_asked("LOG=error:nvidia.scanout_gpu"));
        assert!(!scanout_gpu_bringup_asked(REAL));
        assert!(!scanout_gpu_bringup_asked(""));
    }

    /// The one that would have cost his desktop: `nvidia.console_gpu` must NOT
    /// read as a synonym.
    ///
    /// It selects the deferred task -- wait for the desktop, park scanout,
    /// bring the card up -- which is the only recipe that has ever finished
    /// with both cards up. Read as a synonym, the boot-time attempt runs
    /// first, wedges at the SEC2 `STARTCPU` store, and the boot never reaches
    /// the desktop that the deferred task is waiting for. The flag would keep
    /// its name and lose the behaviour it names.
    #[test]
    fn the_old_console_gpu_flag_does_not_turn_on_the_boot_time_attempt() {
        assert!(!scanout_gpu_bringup_asked("LOG=error:nvidia.console_gpu"));
        assert!(!scanout_gpu_bringup_asked(
            "nvidia.console_gpu:nvidia.console_gsp"
        ));
    }

    /// `nvidia.noconsole_gpu` still means "leave that card alone", whichever
    /// flag asked for it.
    #[test]
    fn asking_for_the_card_to_be_left_cold_beats_asking_for_it_to_come_up() {
        assert!(!scanout_gpu_bringup_asked(
            "nvidia.scanout_gpu:nvidia.noconsole_gpu"
        ));
        assert!(!scanout_gpu_bringup_asked(
            "nvidia.noconsole_gpu:nvidia.scanout_gpu"
        ));
    }

    /// What Moebius says a multi-GPU box does: it declares itself.
    #[test]
    fn a_multigpu_box_declares_itself_and_is_taken_at_its_word() {
        assert_eq!(
            multigpu_verdict("LOG=error:nvidia.multigpu", 2, 1),
            MultiGpu::Declared
        );
        assert!(multigpu_verdict("nvidia.multigpu", 2, 1).is_multigpu());
        assert!(multigpu_verdict("nvidia.multigpu", 4, 1).is_multigpu());
    }

    /// Two cards and nobody said so: still two cards.
    ///
    /// The log says that declaring it is the convention, but behaving as a
    /// one-card box over a missing flag would quietly halve a machine that
    /// enumerated two.
    #[test]
    fn several_cards_without_the_flag_are_still_several_cards() {
        assert_eq!(
            multigpu_verdict(REAL, 2, 1),
            MultiGpu::SeveralCardsUndeclared
        );
        assert!(multigpu_verdict(REAL, 2, 1).is_multigpu());
    }

    /// The declaration is about hardware. With one card it loses, because
    /// there the wedge takes the whole machine.
    #[test]
    fn the_flag_does_not_invent_a_second_card() {
        assert_eq!(
            multigpu_verdict("nvidia.multigpu", 1, 1),
            MultiGpu::DeclaredButOneCard
        );
        assert!(!multigpu_verdict("nvidia.multigpu", 1, 1).is_multigpu());
    }

    /// One card and nothing said: the ordinary desktop, and the default stays
    /// off there.
    #[test]
    fn one_card_and_no_flag_is_a_single_gpu_box() {
        assert_eq!(multigpu_verdict(REAL, 1, 1), MultiGpu::Single);
        assert!(!multigpu_verdict(REAL, 1, 1).is_multigpu());
    }

    /// No card holds the console, so there is no card for this question to be
    /// about -- however many there are and whatever was declared.
    #[test]
    fn no_card_scanning_out_is_not_a_multigpu_question() {
        assert_eq!(multigpu_verdict("nvidia.multigpu", 2, 0), MultiGpu::Single);
        assert_eq!(multigpu_verdict(REAL, 3, 0), MultiGpu::Single);
        assert_eq!(multigpu_verdict(REAL, 0, 0), MultiGpu::Single);
    }
}
