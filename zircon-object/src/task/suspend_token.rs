use {
    super::*,
    crate::object::*,
    alloc::sync::{Arc, Weak},
};

/// Suspend the given task.
///
/// Currently only thread or process handles may be suspended.
///
/// # Example
/// ```no_run
/// # use std::sync::Arc;
/// # use zircon_object::task::*;
/// # use zircon_object::object::{KernelObject, Signal};
/// # kernel_hal::init();
/// let job = Job::root();
/// let proc = Process::create(&job, "proc").unwrap();
/// let thread = Thread::create(&proc, "thread").unwrap();
///
/// // start the thread
/// thread.start(|thread| Box::pin(async move {
///     async_std::task::yield_now().await;
///     let _ = thread;
/// })).unwrap();
///
/// // wait for the thread running
/// let object: Arc<dyn KernelObject> = thread.clone();
/// async_std::task::block_on(object.wait_signal(Signal::THREAD_RUNNING));
/// assert_eq!(thread.state(), ThreadState::Running);
///
/// // suspend the thread
/// {
///     let task: Arc<dyn Task> = thread.clone();
///     let suspend_token = SuspendToken::create(&task);
///     assert_eq!(thread.state(), ThreadState::Suspended);
/// }
/// // suspend token dropped, resume the thread
/// assert_eq!(thread.state(), ThreadState::Running);
/// ```
pub struct SuspendToken {
    base: KObjectBase,
    task: Weak<dyn Task>,
}

impl_kobject!(SuspendToken);

impl SuspendToken {
    /// Create a `SuspendToken` which can suspend the given task.
    pub fn create(task: &Arc<dyn Task>) -> Arc<Self> {
        task.suspend();
        Arc::new(SuspendToken {
            base: KObjectBase::new(),
            task: Arc::downgrade(task),
        })
    }
}

impl Drop for SuspendToken {
    fn drop(&mut self) {
        if let Some(task) = self.task.upgrade() {
            task.resume();
        }
    }
}

#[cfg(test)]
mod tests {
    //! The token *is* the suspension: `zx_task_suspend_token` hands userspace
    //! a handle, and the task stays suspended for exactly as long as that
    //! handle lives. Nothing checked that -- the example above is `no_run`,
    //! so it is documentation and not a test.
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// What a token did to its task, in its own `Arc` so the counts outlive
    /// the task itself.
    #[derive(Default)]
    struct Counts {
        suspends: AtomicUsize,
        resumes: AtomicUsize,
    }

    /// A task that only counts. `SuspendToken` calls two of the five methods
    /// of `Task`, and which one it calls at which end of its own life is the
    /// whole of what it does.
    struct CountingTask {
        counts: Arc<Counts>,
        exceptionate: Arc<Exceptionate>,
    }

    impl Task for CountingTask {
        fn kill(&self) {}
        fn suspend(&self) {
            self.counts.suspends.fetch_add(1, Ordering::SeqCst);
        }
        fn resume(&self) {
            self.counts.resumes.fetch_add(1, Ordering::SeqCst);
        }
        fn exceptionate(&self) -> Arc<Exceptionate> {
            self.exceptionate.clone()
        }
        fn debug_exceptionate(&self) -> Arc<Exceptionate> {
            self.exceptionate.clone()
        }
    }

    fn counting_task() -> (Arc<Counts>, Arc<dyn Task>) {
        let proc = Process::create(&Job::root(), "proc").unwrap();
        let thread = Thread::create(&proc, "thread").unwrap();
        let counts = Arc::new(Counts::default());
        let task: Arc<dyn Task> = Arc::new(CountingTask {
            counts: counts.clone(),
            exceptionate: thread.exceptionate(),
        });
        (counts, task)
    }

    /// One suspend when the token is made, one resume when it goes, in that
    /// order. The pairing is what `Thread::resume` counts on: it decrements a
    /// count, and a resume with no suspend behind it is a path it has to
    /// tolerate rather than one this object may create.
    #[test]
    fn the_token_suspends_when_it_is_made_and_resumes_when_it_goes() {
        let (counts, task) = counting_task();
        let token = SuspendToken::create(&task);
        assert_eq!(counts.suspends.load(Ordering::SeqCst), 1);
        assert_eq!(counts.resumes.load(Ordering::SeqCst), 0, "not yet");

        drop(token);
        assert_eq!(counts.suspends.load(Ordering::SeqCst), 1);
        assert_eq!(counts.resumes.load(Ordering::SeqCst), 1);
    }

    /// Two tokens over one task are two suspends and two resumes:
    /// `zx_task_suspend_token` may be called again before the first handle is
    /// closed, and the task comes back only once the last one goes.
    #[test]
    fn two_tokens_over_one_task_are_two_suspends_and_two_resumes() {
        let (counts, task) = counting_task();
        let first = SuspendToken::create(&task);
        let second = SuspendToken::create(&task);
        assert_eq!(counts.suspends.load(Ordering::SeqCst), 2);

        drop(first);
        assert_eq!(counts.resumes.load(Ordering::SeqCst), 1);
        drop(second);
        assert_eq!(counts.resumes.load(Ordering::SeqCst), 2);
    }

    /// The token holds a weak reference, so a task already gone when the
    /// handle closes is neither resumed nor reached for. Holding a strong one
    /// instead would keep a dead task alive for as long as the handle, which
    /// is the whole reason this is a `Weak`.
    #[test]
    fn a_token_outliving_its_task_resumes_nothing() {
        let (counts, task) = counting_task();
        let token = SuspendToken::create(&task);
        assert_eq!(counts.suspends.load(Ordering::SeqCst), 1);

        drop(task);
        drop(token);
        assert_eq!(
            counts.resumes.load(Ordering::SeqCst),
            0,
            "there was nothing left to resume",
        );
    }
}
