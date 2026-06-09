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
    let root_proc = &options.root_proc;
    #[cfg(not(feature = "linux"))]
    let root_proc = "N/A";

    klog_info!(
        "Eclipse: boot options log_level={} root_proc={}",
        options.log_level,
        root_proc
    );
    memory::insert_regions(&kernel_hal::mem::free_pmem_regions());
    kernel_hal::console::early_progress_bar(80);
    kernel_hal::primary_init();
    kernel_hal::console::early_progress_bar(90);
    cfg_if! {
        if #[cfg(all(feature = "linux", feature = "zircon"))] {
            panic!("Feature `linux` and `zircon` cannot be enabled at the same time!");
        } else if #[cfg(feature = "linux")] {
            let args = options.root_proc.split('?').map(Into::into).collect(); // parse "arg0?arg1?arg2"
            let envs = alloc::vec![
                "PATH=/usr/sbin:/usr/bin:/sbin:/bin".into(),
                "ENV=/etc/profile".into(),
            ];
            let rootfs = fs::rootfs();
            kernel_hal::console::early_progress_bar(95);
            let proc = zcore_loader::linux::run(args, envs, rootfs);
            // Keep secondary CPUs idle until root is mounted and init is spawned.
            STARTED.store(true, Ordering::SeqCst);
            kernel_hal::console::early_progress_bar(100);
            utils::wait_for_exit(Some(proc))
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
