#![no_std]
#![feature(allocator_api)]
// some interfaces is still under developing
#![allow(dead_code)]

cfg_if::cfg_if! {
  if #[cfg(target_arch = "x86_64")] {
      #[path = "arch/x86_64/mod.rs"]
      #[macro_use]
      mod arch;
  } else if #[cfg(target_arch = "riscv64")] {
      #[path = "arch/riscv64/mod.rs"]
      #[macro_use]
      mod arch;
  } else if #[cfg(target_arch = "aarch64")] {
      #[path = "arch/aarch64/mod.rs"]
      #[macro_use]
      mod arch;
  }
}

extern crate alloc;

/// What the three `switch.S` files take for granted; see the module docs.
#[cfg(test)]
mod switch_contract;

/// Concurrency stress across the shared scheduler state; see the module docs.
#[cfg(test)]
mod stress_tests;
#[macro_use]
extern crate log;

// The unit tests run on the host, where the scheduler's own globals are what
// they exercise; `std::sync::Mutex` is what serialises them (a `spin::Mutex`
// held across a failed assertion would hang the rest of the suite instead of
// reporting it).
#[cfg(test)]
extern crate std;

mod context;
mod diag;
mod executor;
mod runtime;
mod task_collection;
mod waker_page;

pub use executor::sched_stats;
pub use executor::{
    alloc_overlaps_live_stack, hard_guard_executor_counts, overlapping_live_stack,
    set_stack_guard_hooks, set_stack_quarantine_enabled, set_stack_quarantine_hooks, spine_gen,
    spine_owner_of, spine_sample, spine_snapshot, spine_verify, stack_guard_hooks_registered, stack_pool_stats,
    untracked_alloc_stacks, untracked_live_stacks, unwatched_spine_slots, SpineSmash, GUARD_SIZE,
    STACK_SIZE, TOP_GUARD_SIZE,
};
pub use runtime::{
    abandon_current_executor, abandon_current_task, abandon_executor_chain,
    abandon_executor_for_sp, affinity_changed, attribute_fault_stack_ptrs, begin_voluntary_yield,
    check_current_executor_canary, check_current_executor_stack_proximity,
    current_executor_abandonable, current_stack_top_looks_null, current_task_abandonable,
    end_voluntary_yield, fault_sp_abandonable, handle_timeout, heap_smash_suspected,
    irq_on_idle_executor, irq_should_skip_dyn_dispatch, irq_should_skip_heavy_work,
    need_resched_pending, note_heap_smash_suspected, run_until_idle, runnable_task_count,
    sched_steal_stats, sched_weak_stats, sched_yield, set_idle_callback, set_resched_ipi_sender,
    set_wakeup_preempt, spawn, spawn_with_affinity, stack_high_water, take_need_resched,
    wakeup_preempt_enabled, wakeup_preempt_stats, warm_runtimes, FaultStackAttr, StackAttrHit,
    StackPtrRegion,
};

#[doc(hidden)]
pub struct InterruptGuard {
    enabled: bool,
}

impl InterruptGuard {
    #[doc(hidden)]
    pub fn new(enabled: bool) -> Self {
        let guard = Self {
            enabled: arch::intr_get(),
        };
        if enabled {
            arch::intr_on();
        } else {
            arch::intr_off();
        }
        guard
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        if self.enabled {
            arch::intr_on();
        } else {
            arch::intr_off();
        }
    }
}

#[macro_export]
macro_rules! run_with_intr_saved_on {
    ($($statements:stmt)*) => {{
        let _interrupt_guard = $crate::InterruptGuard::new(true);
        $($statements)*
    }};
}

#[macro_export]
macro_rules! run_with_intr_saved_off {
    ($($statements:stmt)*) => {{
        let _interrupt_guard = $crate::InterruptGuard::new(false);
        $($statements)*
    }};
}

#[cfg(all(test, target_arch = "x86_64"))]
mod interrupt_macro_tests {
    #[test]
    fn scopes_restore_the_previous_interrupt_state() {
        let _original = crate::InterruptGuard::new(true);
        crate::run_with_intr_saved_off! {
            assert!(!crate::arch::intr_get());
            crate::run_with_intr_saved_on! {
                assert!(crate::arch::intr_get());
            }
            assert!(!crate::arch::intr_get());
        }
        assert!(crate::arch::intr_get());
    }

    #[test]
    fn early_return_restores_interrupts() {
        fn leave() {
            crate::run_with_intr_saved_off! {
                assert!(!crate::arch::intr_get());
                return;
            }
        }
        let _original = crate::InterruptGuard::new(true);
        leave();
        assert!(crate::arch::intr_get());
    }

    #[test]
    fn question_mark_restores_disabled_interrupts() {
        fn fail() -> Result<(), ()> {
            crate::run_with_intr_saved_on! {
                assert!(crate::arch::intr_get());
                Err(())?;
            }
            Ok(())
        }
        let _original = crate::InterruptGuard::new(false);
        assert_eq!(fail(), Err(()));
        assert!(!crate::arch::intr_get());
    }
}
