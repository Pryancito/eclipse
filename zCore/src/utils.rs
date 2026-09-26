use alloc::{string::String, sync::Arc};
use zircon_object::{object::KernelObject, task::Process};

#[derive(Debug)]
#[allow(dead_code)]
pub struct BootOptions {
    #[cfg_attr(feature = "linux", allow(dead_code))]
    pub cmdline: String,
    pub log_level: String,
    /// Process run as PID 1 / init (`INIT`), e.g. `/sbin/init`. Empty
    /// or a missing binary means the system boots without a PID 1 init.
    #[cfg(feature = "linux")]
    pub init_proc: String,
    /// Process run on every terminal shell (`SHELL`), with PIDs 101.. — empty
    /// means no terminal shells (e.g. libos, where `INIT` is the one program).
    #[cfg(feature = "linux")]
    pub shell_proc: String,
}

pub fn boot_options() -> BootOptions {
    cfg_if! {
        if #[cfg(feature = "libos")] {
            let args = std::env::args().collect::<Vec<_>>();
            if args.len() < 2 {
                #[cfg(feature = "linux")]
                println!("Usage: {} PROGRAM", args[0]);
                #[cfg(feature = "zircon")]
                println!("Usage: {} ZBI_FILE [CMDLINE]", args[0]);
                std::process::exit(-1);
            }

            let (cmdline, log_level) = if cfg!(feature = "zircon") {
                let cmdline = args.get(2).cloned().unwrap_or_default();
                let log_level =
                    String::from(kernel_hal::cmdline::value(&cmdline, "LOG").unwrap_or(""));
                (cmdline, log_level)
            } else {
                (String::new(), std::env::var("LOG").unwrap_or_default())
            };
            BootOptions {
                cmdline,
                log_level,
                // libos runs a single program: it is PID 1 (init), no shells.
                #[cfg(feature = "linux")]
                init_proc: args[1..].join("?"),
                #[cfg(feature = "linux")]
                shell_proc: String::new(),
            }
        } else {
            use alloc::string::ToString;
            let cmdline = kernel_hal::boot::cmdline();
            let opt = |k: &str| kernel_hal::cmdline::value(&cmdline, k);
            BootOptions {
                log_level: opt("LOG").unwrap_or("").to_string(),
                // `INIT` selects the PID 1 process. Default `/sbin/init`, which
                // the rootfs points at Eclipse's native `eclipse-init` (the
                // default init system); if its cross-build was unavailable the
                // same `/sbin/init` symlink falls back to busybox's `init` applet.
                // Run only if it exists. `SHELL` selects the per-terminal shells
                // at PIDs 101.. (default busybox); `ROOTPROC` is accepted as a
                // deprecated alias for `SHELL`.
                #[cfg(feature = "linux")]
                init_proc: opt("INIT").unwrap_or("/sbin/init").to_string(),
                #[cfg(feature = "linux")]
                shell_proc: opt("SHELL")
                    .or_else(|| opt("ROOTPROC"))
                    .unwrap_or("/bin/busybox?sh")
                    .to_string(),
                cmdline,
            }
        }
    }
}

/// The process exit status a raw [`Process::exit_code`] maps to.
///
/// A NEGATIVE code is "killed by signal `-raw`" (see
/// `linux_object::process::exit_code_killed_by`): the sign is how the process
/// object says which of the two ways it finished, because every real exit code
/// is a byte. What a PROCESS EXIT STATUS should carry for that is `128 + n` --
/// the shell convention -- and this is what the libos build exits with, and
/// what the CI reads. `-1` means no code at all: the process has not exited.
///
/// `saturating_*` throughout, not because either bound is reachable today
/// (`exit_code_killed_by` takes a `u8` and an exit code comes from an `i32`)
/// but because this maps a value from another crate's public API: an `i64`
/// whose extremes would otherwise panic the kernel's own shutdown path in a
/// debug build, and wrap in a release one. A saturating bound is a wrong
/// number; an overflow here is no number at all.
fn exit_status(raw: Option<i64>) -> i32 {
    let code = match raw {
        None => -1,
        Some(sig) if sig < 0 => 128i64.saturating_sub(sig),
        Some(code) => code,
    };
    code.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[cfg_attr(all(feature = "linux", feature = "zircon"), allow(dead_code))]
fn check_exit_code(proc: Arc<Process>) -> i32 {
    let code = exit_status(proc.exit_code());
    if code != 0 {
        error!(
            "process {:?}({}) exited with code {:?}",
            proc.name(),
            proc.id(),
            code
        );
    } else {
        info!(
            "process {:?}({}) exited with code 0",
            proc.name(),
            proc.id()
        )
    }
    code
}

#[cfg(feature = "libos")]
#[cfg_attr(all(feature = "linux", feature = "zircon"), allow(dead_code))]
pub fn wait_for_exit(proc: Option<Arc<Process>>) -> ! {
    let exit_code = if let Some(proc) = proc {
        let future = async move {
            use zircon_object::object::Signal;
            let object: Arc<dyn KernelObject> = proc.clone();
            let signal = if cfg!(any(feature = "linux", feature = "baremetal-test")) {
                Signal::PROCESS_TERMINATED
            } else {
                Signal::USER_SIGNAL_0
            };
            object.wait_signal(signal).await;
            check_exit_code(proc)
        };

        // If the graphic mode is on, run the process in another thread.
        #[cfg(feature = "graphic")]
        let future = {
            let handle = async_std::task::spawn(future);
            kernel_hal::libos::run_graphic_service();
            handle
        };

        async_std::task::block_on(future)
    } else {
        warn!("No process to run, exit!");
        0
    };
    std::process::exit(exit_code);
}

/// Reset the machine as soon as `proc` exits, for `baremetal-test` builds.
///
/// [`wait_for_exit`]'s queue-empty path cannot do this for a Linux guest:
/// `run_until_idle` returns only when THIS CPU's run queue drains, and the
/// kernel's own background work (deferred driver jobs, the net stack, the
/// console) keeps it non-empty for as long as the machine is up, so the body
/// of that loop never runs a second time. Watching the process's
/// `PROCESS_TERMINATED` signal is what "whose exit takes the system down" in
/// `main.rs` actually means, and it is exact rather than a heuristic.
///
/// Without it `INIT=/bin/busybox?uname` printed its line and QEMU then sat
/// there until the test runner killed it -- every case of `Linux Other Test
/// Baremetal`, on all three architectures.
///
/// Only the Linux personality arms this. The Zircon side reaches the
/// queue-empty path on its own, and a parked watcher task would keep that
/// queue non-empty forever.
#[cfg(all(not(feature = "libos"), feature = "baremetal-test", feature = "linux"))]
pub fn reset_when_process_exits(proc: Arc<Process>) {
    kernel_hal::thread::spawn(async move {
        use zircon_object::object::Signal;
        let object: Arc<dyn KernelObject> = proc.clone();
        object.wait_signal(Signal::PROCESS_TERMINATED).await;
        check_exit_code(proc);
        kernel_hal::cpu::reset();
    });
}

#[cfg(not(feature = "libos"))]
pub fn wait_for_exit(proc: Option<Arc<Process>>) -> ! {
    kernel_hal::timer::timer_enable();
    // Executors call this when their CPU runs out of tasks, right before
    // halting: drain NIC/driver work pushed from IRQ context and flush stdin
    // data whose EventBus notification was deferred (try_lock failed), so the
    // wakers fire now instead of waiting for a thread to re-enter the net
    // stack or for the next timer tick. Returning `true` makes the executor
    // re-check its run queue instead of halting.
    executor::set_idle_callback(|| {
        // Lazy-TLB restore point: this CPU is about to idle (or steal work),
        // so drop any lingering user CR3 and return to the kernel page table.
        // `ThreadSwitchFuture::poll` no longer restores the kernel CR3 after
        // every poll (that TLB flush dominated yield/syscall latency); it is
        // restored here instead, before the CPU can halt with a user CR3 that
        // a concurrent process exit might later free.
        kernel_hal::vm::activate_kernel_paging();
        let had_jobs = kernel_hal::deferred_job::pending_deferred_jobs() > 0;
        // Record the idle-callback hit rate so `/proc/perf/kernel` can show
        // whether the CPUs keep finding deferred work (busy-spin = heat) or
        // actually reach the halt below.
        kernel_hal::kstats::note_idle_callback(had_jobs);
        if had_jobs {
            kernel_hal::deferred_job::drain_deferred_jobs();
        }
        #[cfg(feature = "linux")]
        {
            use linux_object::fs::stdio::STDIN;
            STDIN.flush_ready_flag();
        }
        if !had_jobs {
            // No work and about to halt: stretch this CPU's scheduler tick to
            // the next pending timer (capped) so an idle CPU stops taking the
            // full 250 Hz tick. `timer_idle_exit` (on the next poll) restores it.
            kernel_hal::timer::timer_idle_enter();
        }
        had_jobs
    });
    info!("executor run!");
    // Build this CPU's lazily-constructed runtime (multi-MiB executor stack,
    // guard-page unmaps with their TLB shootdowns) BEFORE announcing IPI
    // readiness below: while constructing, this CPU cannot ack peers'
    // shootdowns, so it must not yet be a target — the same regime the old
    // eager warm-up had, where the BSP built every runtime while APs were
    // still not IPI-ready. No-op on the BSP (built in `primary_init`) and on
    // re-entry.
    executor::warm_runtimes();
    // This CPU is now entering the executor loop, where it runs with interrupts
    // enabled and will service TLB-shootdown IPIs. Announce it as a valid
    // shootdown target: until now an AP spins on `STARTED` with IRQs off and
    // could not ack, so a shootdown that waited on it would stall.
    kernel_hal::mark_cpu_ipi_ready(kernel_hal::cpu::cpu_id() as usize);
    loop {
        // In normal builds `run_until_idle` never returns (idle work happens in
        // the callback above); it only returns under `baremetal-test` when the
        // task queue is empty.
        let has_task = executor::run_until_idle();
        // `run_until_idle` decides "idle" from THIS CPU's own run queue --
        // `Runtime::task_num()` reads the per-CPU task collection of
        // `get_current_runtime()`. An AP's queue is empty the moment it comes
        // online, before the BSP has placed or a steal has moved any work onto
        // it, so letting every CPU end the run here reset the whole machine a
        // few ms after the APs entered the executor: the guest listed no tests
        // and QEMU exited 0, which is how `Zircon Core Test Baremetal (x86_64)`
        // failed its `--smp 2` and `--smp 4` port-stress runs.
        //
        // Only the CPU carrying the root process may call the run over; `proc`
        // is `Some` exactly there (`secondary_main()` passes `None`). The
        // others fall through to `wait_for_interrupt()` and idle, which is what
        // an AP with nothing to run should do anyway.
        if !has_task && cfg!(feature = "baremetal-test") && proc.is_some() {
            proc.map(check_exit_code);
            kernel_hal::cpu::reset();
        }
        kernel_hal::interrupt::wait_for_interrupt();
    }
}

#[cfg(all(not(feature = "libos"), feature = "mock-disk"))]
pub fn mock_disk() -> ! {
    use crate::fs::init_ram_disk;
    info!("mock core: {}", kernel_hal::cpu::cpu_id());
    if let Some(initrd) = init_ram_disk() {
        linux_object::fs::mocking_block(initrd)
    } else {
        panic!("can't find disk image in memory")
    }
}

// pub fn nvme_test(){
//     use alloc::boxed::Box;
//     let irq = kernel_hal::drivers::all_irq().find("riscv-plic").unwrap();
//     let nvme = kernel_hal::drivers::all_block().find("nvme").unwrap();
//     let irq_num = 33;
//     let _r = irq.register_handler(irq_num, Box::new(move || nvme.handle_irq(irq_num)));

//     let _r = irq.unmask(irq_num);

//     let nvme_block = kernel_hal::drivers::all_block()
//     .find("nvme")
//     .unwrap();

//     let buf1:&[u8] = &[1u8;512];
//     let _r = nvme_block.write_block(0, &buf1);
//     warn!("r {:?}", _r);
//     let mut read_buf = [0u8; 512];
//     let _r = nvme_block.read_block(0, &mut read_buf);
//     warn!("read_buf: {:?}", read_buf);

//     let buf2:&[u8] = &[2u8;512];
//     let _r = nvme_block.write_block(1, &buf2);
//     warn!("r {:?}", _r);
//     let mut read_buf = [0u8; 512];
//     let _r = nvme_block.read_block(1, &mut read_buf);
//     warn!("read_buf: {:?}", read_buf);
// }

/// The exit status the libos build returns to the shell, which is what the CI
/// reads to decide whether a run passed.
#[cfg(test)]
mod exit_status_tests {
    use super::exit_status;

    #[test]
    fn a_process_that_has_not_exited_has_no_status() {
        assert_eq!(exit_status(None), -1);
    }

    #[test]
    fn an_ordinary_exit_code_passes_through() {
        for code in [0i64, 1, 42, 127, 255] {
            assert_eq!(exit_status(Some(code)), code as i32);
        }
    }

    /// The sign is how the process object says WHICH of the two ways it
    /// finished, and the shell convention for "killed by signal n" is `128 + n`.
    /// `linux_object::process::exit_code_killed_by` stores `-n`.
    #[test]
    fn a_process_killed_by_a_signal_reports_the_shell_convention() {
        assert_eq!(exit_status(Some(-9)), 137, "SIGKILL is 128 + 9");
        assert_eq!(exit_status(Some(-11)), 139, "SIGSEGV is 128 + 11");
        assert_eq!(exit_status(Some(-1)), 129);
        assert_eq!(exit_status(Some(-64)), 192, "the highest real-time signal");
    }

    /// The two halves must not collide: a program that really calls `exit(137)`
    /// is not a process killed by SIGKILL, and the kernel already went to the
    /// trouble of keeping them apart (see `exit_code_killed_by`'s own comment).
    /// What this function does is join them back together for the shell, so the
    /// only thing left to pin is that a signal cannot be mistaken for `None`.
    #[test]
    fn no_signal_maps_onto_the_no_status_answer() {
        for sig in 1i64..=255 {
            assert_ne!(
                exit_status(Some(-sig)),
                -1,
                "signal {sig} collided with 'has not exited'"
            );
        }
    }

    /// `exit_code()` is another crate's `i64`, and `128 - i64::MIN` panics in a
    /// debug build and wraps in a release one. Neither extreme is reachable
    /// today -- a signal is a `u8`, an exit code an `i32` -- so this pins that
    /// the shutdown path answers with a number either way rather than taking
    /// the kernel down on its way out.
    #[test]
    fn an_absurd_raw_code_saturates_instead_of_overflowing() {
        assert_eq!(exit_status(Some(i64::MIN)), i32::MAX);
        assert_eq!(exit_status(Some(i64::MAX)), i32::MAX);
        assert_eq!(exit_status(Some(i32::MAX as i64 + 1)), i32::MAX);
    }
}
