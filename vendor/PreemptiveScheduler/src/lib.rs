#![no_std]
#![feature(allocator_api)]
#![feature(get_mut_unchecked)]
// `generators`/`generator_trait` fue renombrado y luego retirado; en nightlies nuevas se usa coroutines.
#![feature(coroutines, coroutine_trait, yield_expr)]
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
#[macro_use]
extern crate log;

mod context;
mod executor;
mod runtime;
mod task_collection;
mod waker_page;

pub use runtime::{handle_timeout, run_until_idle, sched_yield, spawn};

use core::sync::atomic::{AtomicUsize, Ordering};

static WAKE_CPU_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register a callback invoked right after a task becomes runnable on some
/// CPU's queue (the argument is that CPU's dense logical id). The kernel uses
/// this to IPI idle (HLT'ed) CPUs so cross-CPU wakeups don't wait for the
/// next periodic timer tick.
pub fn set_wake_cpu_hook(hook: fn(usize)) {
    WAKE_CPU_HOOK.store(hook as usize, Ordering::Release);
}

#[inline]
pub(crate) fn wake_cpu(cpu_id: usize) {
    let f = WAKE_CPU_HOOK.load(Ordering::Acquire);
    if f != 0 {
        let f: fn(usize) = unsafe { core::mem::transmute(f) };
        f(cpu_id);
    }
}

/// CPUs that actually came online; spawn distribution only targets these.
/// Defaults to all queues so behavior is unchanged until the kernel reports
/// the real count after SMP bring-up.
static ACTIVE_CPUS: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Tell the executor how many CPUs are online, so new tasks are not parked
/// on the run queue of a CPU that does not exist (those would only ever run
/// when some real CPU steals them on a later timer tick).
pub fn set_active_cpu_count(count: usize) {
    ACTIVE_CPUS.store(count.max(1), Ordering::Release);
}

pub(crate) fn active_cpu_count() -> usize {
    ACTIVE_CPUS.load(Ordering::Acquire)
}

#[macro_export]
macro_rules! run_with_intr_saved_on {
    ($($statements:stmt)*) => {
        let enable = crate::arch::intr_get();
        if !enable {
          crate::arch::intr_on();
        }
        $($statements)*
        if !enable {
          crate::arch::intr_off();
        }
    };
}

#[macro_export]
macro_rules! run_with_intr_saved_off {
    ($($statements:stmt)*) => {
        let enable = crate::arch::intr_get();
        if enable {
            crate::arch::intr_off();
        }
        $($statements)*
        if enable {
            crate::arch::intr_on();
        }
    };
}
