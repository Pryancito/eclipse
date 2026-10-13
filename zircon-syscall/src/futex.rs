use {
    super::*,
    zircon_object::task::{Thread, ThreadState},
};

/// A futex word the kernel will read (or, with `access` = WRITE, store to):
/// non-null, aligned, and mapped that way in the caller's own address
/// space. `zx_futex_wake` needs only the first two, as an unallocated
/// address is not an error there.
pub(crate) fn check_futex_word(
    proc: &zircon_object::task::Process,
    word: &UserInPtr<AtomicI32>,
    access: kernel_hal::MMUFlags,
) -> ZxResult {
    if word.is_null() || !word.as_addr().is_multiple_of(4) {
        return Err(ZxError::INVALID_ARGS);
    }
    crate::user_memory::validate_user_range(
        proc,
        word.as_addr(),
        core::mem::size_of::<AtomicI32>(),
        access,
    )
}

impl Syscall<'_> {
    /// Resolve the `new_futex_owner` handle of a `zx_futex_wait` or a
    /// `zx_futex_requeue`.
    ///
    /// Zircon validates the owner at the syscall boundary, and two of its rules
    /// are ones the futex object cannot see from its queue:
    ///
    /// * the owner may not be the very thread that is about to block on the futex
    ///   (`caller`), and
    /// * the owner must have STARTED.
    ///
    /// A thread that has already DIED is a legal owner -- the core tests ask for a
    /// *successful* `zx_futex_wait` (it times out) and a successful
    /// `zx_futex_requeue` with a dead thread -- so the predicate is
    /// `ThreadState::New`, "created but never started", and not "not running".
    ///
    /// Both rules were missing, and that is not a wrong error code: the cases
    /// `FutexTest.WaitInvalidOwner` asks about carry `ZX_TIME_INFINITE`, so an
    /// accepted wait is a thread that never comes back. The `zircon-core` suite
    /// stopped dead there, with 207 cases green behind it.
    ///
    /// `caller` is `None` on the requeue side on purpose: the suite proves the
    /// started rule for both calls, and says nothing about a requeue naming its
    /// own thread, so that case stays exactly as it was.
    fn new_futex_owner(
        &self,
        handle: HandleValue,
        caller: Option<&Arc<Thread>>,
    ) -> ZxResult<Option<Arc<Thread>>> {
        if handle == INVALID_HANDLE {
            return Ok(None);
        }
        let owner = self.thread.proc().get_object::<Thread>(handle)?;
        if caller.is_some_and(|caller| Arc::ptr_eq(caller, &owner)) {
            return Err(ZxError::INVALID_ARGS);
        }
        if owner.state() == ThreadState::New {
            return Err(ZxError::INVALID_ARGS);
        }
        Ok(Some(owner))
    }

    /// Wait on a futex.
    ///
    /// This system call function atomically verifies that `value_ptr` still contains the value `current_value`
    /// and sleeps until the futex is made available by a call to `zx_futex_wake`
    pub async fn sys_futex_wait(
        &self,
        value_ptr: UserInPtr<AtomicI32>,
        current_value: i32,
        new_futex_owner: HandleValue,
        deadline: Deadline,
    ) -> ZxResult {
        info!(
            "futex.wait: value_ptr={:#x?}, current_value={:#x}, new_futex_owner={:#x}, deadline={:?}",
            value_ptr, current_value, new_futex_owner, deadline
        );
        let proc = self.thread.proc();
        // The word is read to compare it with `current_value`, so it has to
        // be the caller's own mapped memory: a null check and an alignment
        // check were all there was, and `as_ref` on any other address read
        // it as the kernel, a fault with no fixup behind it.
        check_futex_word(proc, &value_ptr, kernel_hal::MMUFlags::READ)?;
        let value = value_ptr.as_ref();
        let futex = proc.get_futex(value);
        let caller = self.thread.inner();
        let new_owner = self.new_futex_owner(new_futex_owner, Some(&caller))?;
        let future = futex.wait_with_owner(current_value, Some(caller), new_owner);
        let _: () = self
            .thread
            .blocking_run(future, ThreadState::BlockedFutex, deadline.into(), None)
            .await?;
        Ok(())
    }
    /// Wake some waiters and requeue other waiters.
    ///
    /// Wake some number of threads waiting on a futex, and move more waiters to another wait queue.
    pub fn sys_futex_requeue(
        &self,
        value_ptr: UserInPtr<AtomicI32>,
        wake_count: u32,
        current_value: i32,
        requeue_ptr: UserInPtr<AtomicI32>,
        requeue_count: u32,
        new_requeue_owner: HandleValue,
    ) -> ZxResult {
        info!(
            "futex.requeue: value_ptr={:?}, wake_count={:#x}, current_value={:#x}, requeue_ptr={:?}, requeue_count={:#x}, new_requeue_owner={:?}",
            value_ptr, wake_count, current_value, requeue_ptr, requeue_count, new_requeue_owner
        );
        let proc = self.thread.proc();
        // The wake word is read (compared with `current_value`); the requeue
        // word only names a queue, but it still has to be a word: a null or
        // misaligned `requeue_ptr` went unchecked.
        check_futex_word(proc, &value_ptr, kernel_hal::MMUFlags::READ)?;
        if requeue_ptr.is_null() || !requeue_ptr.as_addr().is_multiple_of(4) {
            return Err(ZxError::INVALID_ARGS);
        }
        let value = value_ptr.as_ref();
        let requeue = requeue_ptr.as_ref();
        if value_ptr.as_addr() == requeue_ptr.as_addr() {
            return Err(ZxError::INVALID_ARGS);
        }
        let new_requeue_owner = self.new_futex_owner(new_requeue_owner, None)?;
        let wake_futex = proc.get_futex(value);
        let requeue_futex = proc.get_futex(requeue);
        wake_futex.requeue(
            current_value,
            wake_count as usize,
            requeue_count as usize,
            &requeue_futex,
            new_requeue_owner,
            true,
        )?;
        Ok(())
    }

    /// Wake some number of threads waiting on a futex.
    ///
    /// > Waking up zero threads is not an error condition. Passing in an unallocated address for value_ptr is not an error condition.
    pub fn sys_futex_wake(&self, value_ptr: UserInPtr<AtomicI32>, count: u32) -> ZxResult {
        info!("futex.wake: value_ptr={:?}, count={:#x}", value_ptr, count);
        if value_ptr.is_null() || !value_ptr.as_addr().is_multiple_of(4) {
            return Err(ZxError::INVALID_ARGS);
        }
        let value = value_ptr.as_ref();
        let proc = self.thread.proc();
        let futex = proc.get_futex(value);
        futex.wake(count as usize);
        Ok(())
    }

    /// Wake some number of threads waiting on a futex, and move more waiters to another wait queue.
    pub fn sys_futex_wake_single_owner(&self, value_ptr: UserInPtr<AtomicI32>) -> ZxResult {
        info!("futex.wake_single_owner: value_ptr={:?}", value_ptr);
        if value_ptr.is_null() || !value_ptr.as_addr().is_multiple_of(4) {
            return Err(ZxError::INVALID_ARGS);
        }
        let value = value_ptr.as_ref();
        let proc = self.thread.proc();
        proc.get_futex(value).wake_single_owner();
        Ok(())
    }
}
