//! Bootstrap and initialization.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::{KernelConfig, KernelHandler, KCONFIG, KHANDLER};

/// Set once the primary CPU has a heap, a `KernelHandler` and its own logical
/// CPU id. No secondary may run a line of `secondary_init` before that.
///
/// On x86_64 the BSP starts the APs itself, at the end of `primary_init()`, so
/// this is already true by the time any of them runs. RISC-V is the other way
/// round: SBI releases every hart at once, and
/// `zCore/src/platform/riscv/entry.rs` calls `boot_secondary_harts()` BEFORE
/// `primary_main()`. A secondary therefore went straight into the per-CPU
/// setup below while the primary had not yet run `memory::init()` or
/// `primary_init_early()`, and died one of two ways: allocating against a heap
/// that did not exist -- `kernel OOM: alloc 2686976 bytes failed (used 0 /
/// total 0 MiB)`, the PercpuBlock -- or reading the handler and panicking with
/// `uninitialized InitOnce<&dyn KernelHandler>`. Eight riscv64 boots out of
/// eight died, which is what `Linux Other Test Baremetal (riscv64)` was
/// reporting as 22 FAILED and 7 TIMEOUT.
///
/// `percpu::register()` is the earliest it could be set, and the reason is
/// `lock`'s hart -> logical table: it starts out all zeroes, so every CPU that
/// has not registered yet reports id 0 and they all share `CPUS[0]`'s
/// lock-depth counter. Two of them bracketing a lock at the same time leaves
/// that counter wrong and the next release panics with `pop_off`. Registering
/// the primary first also keeps the boot CPU at logical id 0, which
/// `LOGICAL_TO_HART` and the SBI IPI path assume. It is in fact set later
/// still, at the end of `primary_init` -- see the comment at the store.
///
/// [`super::arch::secondary_init`]'s own `DRIVERS_READY` gate is a later,
/// narrower one: it covers the device-tree walk alone.
static PRIMARY_READY: AtomicBool = AtomicBool::new(false);

hal_fn_impl! {
    impl mod crate::hal_fn::boot {
        fn cmdline() -> alloc::string::String {
            super::arch::cmdline()
        }

        fn init_ram_disk() -> Option<&'static mut [u8]> {
            super::arch::init_ram_disk()
        }

        fn primary_init_early(cfg: KernelConfig, handler: &'static impl KernelHandler) {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            lock::set_phys_virt_offset(cfg.phys_to_virt_offset as u64);
            KCONFIG.init_once_by(cfg);
            KHANDLER.init_once_by(handler);
            // Parse the framebuffer cmdline flags NOW, while the bootloader's
            // command-line string is still mapped: the panic console used to
            // re-read it on every write, so a late panic double-faulted inside
            // its own banner and printed nothing.
            // The module itself is `graphic`-gated; zircon mode builds without it.
            #[cfg(all(target_arch = "x86_64", feature = "graphic"))]
            super::arch::early_fb_console::latch_cmdline_flags();
            crate::klog_info!("Eclipse: primary CPU {} init early", crate::cpu::cpu_id());
            super::arch::primary_init_early();
        }

        fn primary_init() {
            crate::klog_info!("Eclipse: primary CPU {} init", crate::cpu::cpu_id());
            // AVX/XSAVE escape hatch: `noavx` (or `avx=off`) on the kernel
            // command line forces the FXSAVE path, for isolating AVX enablement
            // on real hardware without a rebuild. Must run before trapframe::init
            // (which sets XCR0), and it persists for the APs' init_ap too.
            #[cfg(target_arch = "x86_64")]
            {
                let cl = super::arch::cmdline();
                if cl.contains("noavx") || cl.contains("avx=off") {
                    trapframe::set_avx_disabled(true);
                }
            }
            unsafe { trapframe::init() };
            #[cfg(target_arch = "x86_64")]
            crate::klog_info!(
                "fpu: XSAVE/AVX {}",
                if trapframe::xsave_avx_enabled() {
                    "ENABLED (256-bit user vectors)"
                } else {
                    "disabled (FXSAVE path)"
                }
            );
            crate::vm::pin_kernel_vmtoken();
            // And run on it from here, rather than several steps further down
            // in `arch::primary_init`. `stack_guard::init` and the first
            // `Executor::new` are below, and they edit the page table the CPU
            // is actually running on -- which, until this call, was the one the
            // boot code built to leave physical addressing behind.
            super::arch::activate_kernel_page_table();
            // Bind this CPU to its PercpuBlock (sets the GS fast-path on x86_64).
            super::percpu::register();
            // Let the scheduler kick halted CPUs on cross-CPU wakes instead of
            // waiting for their next periodic tick (up to 4 ms of latency per
            // pipe write / IO completion / process exit otherwise).
            executor::set_resched_ipi_sender(|cpu| {
                crate::interrupt::send_wake_ipi(cpu);
            });
            // Unmapped guard pages under coroutine stacks — must run after the
            // kernel page tables are pinned so install can punch 4K holes in
            // the BSS heap mapping (see `stack_guard`). Must also run *before*
            // any `Executor::new`: warm the lazy GLOBAL_RUNTIME here so the
            // first strong stacks get hard guards (not soft-only leftovers).
            super::stack_guard::init();
            if !executor::stack_guard_hooks_registered() {
                panic!("stack_guard::init failed to register executor hooks");
            }
            executor::warm_runtimes();
            // Say out loud whether the coroutine stacks actually got unmapped
            // guard bands. Without them an overflow silently overwrites
            // neighbouring heap instead of faulting, and the machine dies later
            // and elsewhere — indirect calls to `rip=0x0`/`0x3`, `Arc` vtables
            // replaced by small integers — with nothing linking the wreckage
            // back to the stack that caused it. That failure is expensive
            // enough to diagnose that its precondition belongs in the boot log.
            let (installed, refused) = super::stack_guard::stats();
            let (hard, soft) = executor::hard_guard_executor_counts();
            let (pool, spent) = super::stack_guard::split_pool_stats();
            crate::klog_info!(
                "stack_guard: {} guard band(s) installed, {} refused; \
                 executors hard={} soft={}; split frames {}/{}",
                installed,
                refused,
                hard,
                soft,
                spent,
                pool
            );
            // The primary now holds logical id 0 and everything a secondary
            // needs; release any that are already spinning (see `PRIMARY_READY`).
            //
            // Deliberately down here rather than right after `percpu::register`,
            // which is as early as correctness allows. Everything above is the
            // single-CPU part of the boot: reserving the guard frames walks the
            // heap 128 times, and the first `Executor::new` rewrites the kernel
            // page table under the coroutine stacks and shoots the TLB down.
            // With the secondaries still parked, `remote_flush_tlb_aspace` sees
            // one online CPU and does not send an IPI at all — which matters
            // because a secondary between its release and its `trapframe::init`
            // has no trap vector, and a trap there jumps to address 0. That is
            // exactly the `null-range #PF ... rip=0x0` this used to produce on
            // roughly one riscv64 boot in five, once the guard bands started
            // installing instead of being refused.
            PRIMARY_READY.store(true, Ordering::Release);
            super::arch::primary_init();
        }

        fn secondary_init() {
            // Nothing below may run before the primary is ready; on RISC-V it
            // has not even started yet. Plain spin: this runs once per CPU at
            // boot, with no scheduler to yield to.
            while !PRIMARY_READY.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            #[cfg(target_arch = "x86_64")]
            {
                let logical = super::arch::ap_trampoline_logical_id();
                // We have now latched our logical id out of the shared trampoline
                // slot — release the BSP to reuse the slots for the next AP. Do
                // this *before* any slow per-CPU init so a busy-waiting BSP can't
                // outrun us and clobber the slot mid-flight (see `smp.rs`).
                super::arch::ap_signal_slot_consumed();
                lock::with_ap_boot_logical(logical, || unsafe {
                    trapframe::init_ap();
                });
                unsafe {
                    trapframe::write_logical_cpu_id(logical);
                }
                // Provisional: this AP's LAPIC is still in xAPIC mode (INIT
                // leaves it there, whatever mode the BSP runs in), so this is
                // the 8-bit id. `secondary_init` re-registers the authoritative
                // one once the AP has switched its own LAPIC to x2APIC.
                lock::set_logical_cpu_id(lock::hardware_apic_id(), logical);
            }
            // Claim this CPU's logical id BEFORE anything that can take a
            // lock. `lock`'s hart -> logical table is all zeroes to start with,
            // so a CPU that has not registered reports id 0 and brackets its
            // lock guards on the primary's depth counter; the moment the two
            // interleave, one of them releases a lock the counter says it does
            // not hold and `pop_off` panics. x86_64 solves the same problem its
            // own way above, with `with_ap_boot_logical` around `init_ap`.
            #[cfg(not(target_arch = "x86_64"))]
            {
                super::percpu::register();
                unsafe { trapframe::init() };
            }
            #[cfg(target_arch = "x86_64")]
            super::percpu::register();
            super::arch::secondary_init();
        }
    }
}
