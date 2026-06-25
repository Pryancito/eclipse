#![cfg_attr(not(feature = "libos"), no_std)]
#![cfg_attr(not(feature = "libos"), feature(alloc_error_handler))]
#![deny(warnings)]
#![no_main]

use core::sync::atomic::{AtomicBool, Ordering};

extern crate alloc;
#[macro_use]
extern crate log;
#[macro_use]
extern crate cfg_if;

#[macro_use]
mod logging;

#[cfg(not(feature = "libos"))]
mod lang;

mod fs;
mod handler;
mod invariants;
mod platform;
mod utils;

cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        #[path = "memory_x86_64.rs"]
        mod memory;
    } else {
        mod memory;
    }
}

static STARTED: AtomicBool = AtomicBool::new(false);

#[cfg(all(not(any(feature = "libos")), feature = "mock-disk"))]
static MOCK_CORE: AtomicBool = AtomicBool::new(false);

fn primary_main(config: kernel_hal::KernelConfig) {
    logging::init();
    memory::init();
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

    klog_info!(
        "Eclipse: boot options log_level={} init={} shell={}",
        options.log_level,
        init_proc,
        shell_proc
    );
    memory::insert_regions(&kernel_hal::mem::free_pmem_regions());
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
            let envs: alloc::vec::Vec<alloc::string::String> = alloc::vec![
                "PATH=/usr/sbin:/usr/bin:/sbin:/bin".into(),
                "ENV=/etc/profile".into(),
                "HOME=/root".into(),
                "TERM=xterm-256color".into(),
                "USER=root".into(),
                "LOGNAME=root".into(),
                // UTF-8 locale so ncurses/readline use Unicode box-drawing and
                // compute character widths correctly (the console renders the
                // box-drawing/block code points procedurally).
                "LANG=C.UTF-8".into(),
                "LC_ALL=C.UTF-8".into(),
            ];
            let rootfs = fs::rootfs();
            // Load hunter's /etc/hunter/{whitelist,blacklist} from the root fs
            // and enable exec learning (trust-on-first-use). Safe if absent.
            linux_object::fs::hunter_config::load(&rootfs.root_inode());
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
                for vt in 0..kernel_hal::console::NUM_VTS {
                    let pid = (101 + vt) as u64;
                    let proc = zcore_loader::linux::run_shell_on_vt(
                        shell_args.clone(),
                        envs.clone(),
                        rootfs.clone(),
                        vt,
                        shared_root.clone(),
                        pid,
                    );
                    if vt == 0 {
                        shared_root = Some(proc.linux().root_inode().clone());
                        primary_shell = Some(proc);
                    }
                }
                // Optionally run INIT as PID 1 (default /sbin/init -> openrc-init,
                // busybox init fallback), if it exists. The shells already did
                // the boot work, so don't repeat.
                let init = zcore_loader::linux::run_init_if_present(
                    init_args,
                    envs.clone(),
                    rootfs.clone(),
                    shared_root,
                    false,
                );
                init.or(primary_shell)
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
