//! Thread spawning.

use alloc::sync::Arc;
use core::{any::Any, future::Future};

use crate::{config_common::MAX_CORE_NUM, utils::PerCpuCell};

#[allow(clippy::declare_interior_mutable_const)]
const DEFAULT_THREAD: PerCpuCell<Option<Arc<dyn Any + Send + Sync>>> = PerCpuCell::new(None);

static CURRENT_THREAD: [PerCpuCell<Option<Arc<dyn Any + Send + Sync>>>; MAX_CORE_NUM] =
    [DEFAULT_THREAD; MAX_CORE_NUM];

hal_fn_impl! {
    impl mod crate::hal_fn::thread {
        fn spawn(future: impl Future<Output = ()> + Send + 'static) {
            executor::spawn(future);
        }

        fn spawn_with_affinity(
            future: impl Future<Output = ()> + Send + 'static,
            affinity: Arc<core::sync::atomic::AtomicU64>,
        ) {
            executor::spawn_with_affinity(future, affinity);
        }

        fn set_current_thread(thread: Option<Arc<dyn Any + Send + Sync>>) {
            *percpu::current().current_thread.get_mut() = thread;
        }

        fn get_current_thread() -> Option<Arc<dyn Any + Send + Sync>> {
            percpu::current().current_thread.get().as_ref().cloned()
        }

        fn take_need_resched() -> bool {
            executor::take_need_resched()
        }

        fn runnable_task_count() -> usize {
            executor::runnable_task_count()
        }
    }
}
