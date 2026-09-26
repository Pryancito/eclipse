use alloc::vec::Vec;
use core::convert::TryFrom;
use kernel_hal::context::UserContextField;
use {super::*, zircon_object::task::*};

impl Syscall<'_> {
    /// Create a new process.
    ///
    /// Upon success, handles for the new process and the root of its address space are returned.
    pub fn sys_process_create(
        &self,
        job: HandleValue,
        name: UserInPtr<u8>,
        name_size: usize,
        options: u32,
        mut proc_handle: UserOutPtr<HandleValue>,
        mut vmar_handle: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        let name = name.as_str(name_size)?;
        info!(
            "proc.create: job={:#x?}, name={:?}, options={:#x?}",
            job, name, options,
        );
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let job = proc
            .get_object_with_rights::<Job>(job, Rights::MANAGE_PROCESS)
            .or_else(|_| proc.get_object_with_rights::<Job>(job, Rights::WRITE))?;
        // Zircon processes get an address space that does not start at zero;
        // see `VmAddressRegion::new_root_zircon`.
        let new_proc = Process::create_with_vmar(
            &job,
            name,
            zircon_object::vm::VmAddressRegion::new_root_zircon(),
            (),
        )?;
        let new_vmar = new_proc.vmar();
        install_handle_pair(
            proc,
            (
                Handle::new(new_proc, Rights::DEFAULT_PROCESS),
                Handle::new(
                    new_vmar,
                    Rights::DEFAULT_VMAR | Rights::READ | Rights::WRITE | Rights::EXECUTE,
                ),
            ),
            (&mut proc_handle, &mut vmar_handle),
        )
    }

    /// Exits the currently running process.
    pub fn sys_process_exit(&mut self, code: i64) -> ZxResult {
        info!("proc.exit: code={:?}", code);
        let proc = self.thread.proc();
        proc.exit(code);
        Ok(())
    }

    /// Creates a thread within the specified process.
    ///
    /// Upon success a handle for the new thread is returned.
    pub fn sys_thread_create(
        &self,
        proc_handle: HandleValue,
        name: UserInPtr<u8>,
        name_size: usize,
        options: u32,
        mut thread_handle: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        let name = name.as_str(name_size)?;
        info!(
            "thread.create: proc={:#x?}, name={:?}, options={:#x?}",
            proc_handle, name, options,
        );
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let process = proc.get_object_with_rights::<Process>(proc_handle, Rights::MANAGE_THREAD)?;
        let thread = Thread::create(&process, name)?;
        install_handle(
            proc,
            Handle::new(thread, Rights::DEFAULT_THREAD),
            &mut thread_handle,
        )
    }

    /// Start execution on a process.
    ///
    /// This system call is similar to `zx_thread_start()`, but is used for the purpose of starting the first thread in a process.
    pub fn sys_process_start(
        &self,
        proc_handle: HandleValue,
        thread_handle: HandleValue,
        entry: usize,
        stack: usize,
        arg1_handle: HandleValue,
        arg2: usize,
    ) -> ZxResult {
        info!(
            "process.start: proc_handle={:?}, thread_handle={:?}, entry={:?}, stack={:?}, arg1_handle={:?}, arg2={:?}",
            proc_handle, thread_handle, entry, stack, arg1_handle, arg2
        );
        let proc = self.thread.proc();
        let process = proc.get_object_with_rights::<Process>(proc_handle, Rights::WRITE)?;
        let thread = proc.get_object_with_rights::<Thread>(thread_handle, Rights::WRITE)?;
        if !Arc::ptr_eq(thread.proc(), &process) {
            return Err(ZxError::ACCESS_DENIED);
        }
        let arg1 = if arg1_handle != INVALID_HANDLE {
            let arg1 = proc.remove_handle(arg1_handle)?;
            if !arg1.rights.contains(Rights::TRANSFER) {
                return Err(ZxError::ACCESS_DENIED);
            }
            Some(arg1)
        } else {
            None
        };
        process.start(&thread, entry, stack, arg1, arg2, self.thread_fn)?;
        Ok(())
    }

    /// Read one aspect of thread state.
    ///
    /// The thread state may only be written when the thread is halted for an exception or the thread is suspended.
    pub fn sys_thread_read_state(
        &self,
        handle: HandleValue,
        kind: u32,
        mut buffer: UserOutPtr<u8>,
        buffer_size: usize,
    ) -> ZxResult {
        let kind = ThreadStateKind::try_from(kind).map_err(|_| ZxError::INVALID_ARGS)?;
        info!(
            "thread.read_state: handle={:#x?}, kind={:#x?}, buf=({:#x?}; {:#x?})",
            handle, kind, buffer, buffer_size,
        );
        let proc = self.thread.proc();
        let thread = proc.get_object_with_rights::<Thread>(handle, Rights::READ)?;
        // The kernel buffer is as large as a state can be, not as large as the
        // caller says: `vec![0; buffer_size]` with a `buffer_size` from
        // userspace was an allocation of any size, and past what the heap
        // has, a kernel panic. Only the bytes of the state come back, as
        // `zx_thread_read_state` promises.
        let mut buf = vec![0; buffer_size.min(MAX_THREAD_STATE_SIZE)];
        let len = thread.read_state(kind, &mut buf)?;
        buffer.write_array(&buf[..len])?;
        Ok(())
    }

    /// Write one aspect of thread state.
    ///
    /// The thread state may only be written when the thread is halted for an exception or the thread is suspended.
    pub fn sys_thread_write_state(
        &self,
        handle: HandleValue,
        kind: u32,
        buffer: UserInPtr<u8>,
        buffer_size: usize,
    ) -> ZxResult {
        let kind = ThreadStateKind::try_from(kind).map_err(|_| ZxError::INVALID_ARGS)?;
        info!(
            "thread.write_state: handle={:#x?}, kind={:#x?}, buf=({:#x?}; {:#x?})",
            handle, kind, buffer, buffer_size,
        );
        self.thread
            .proc()
            .get_object_with_rights::<Thread>(handle, Rights::WRITE)?
            .write_state(kind, buffer.as_slice(buffer_size)?)
    }

    /// Sets process as critical to job.
    ///
    /// When process terminates, job will be terminated as if `zx_task_kill()` was called on it.
    pub fn sys_job_set_critical(
        &self,
        job_handle: HandleValue,
        options: u32,
        process_handle: HandleValue,
    ) -> ZxResult {
        info!(
            "job.set_critical: job={:#x?}, options={:#x}, process={:#x?}",
            job_handle, options, process_handle,
        );
        // Any other option is the caller's mistake, not a kernel panic: this
        // used to be `unimplemented!()`.
        let retcode_nonzero = match options {
            0 => false,
            1 => true,
            _ => return Err(ZxError::INVALID_ARGS),
        };
        let proc = self.thread.proc();
        let job = proc.get_object_with_rights::<Job>(job_handle, Rights::DESTROY)?;
        let process = proc.get_object_with_rights::<Process>(process_handle, Rights::WAIT)?;
        process.set_critical_at_job(&job, retcode_nonzero)?;
        Ok(())
    }

    /// Start execution on a thread.
    pub fn sys_thread_start(
        &self,
        handle_value: HandleValue,
        entry: usize,
        stack: usize,
        arg1: usize,
        arg2: usize,
    ) -> ZxResult {
        info!(
            "thread.start: handle={:#x?}, entry={:#x}, stack={:#x}, arg1={:#x} arg2={:#x}",
            handle_value, entry, stack, arg1, arg2
        );
        let proc = self.thread.proc();
        let thread = proc.get_object_with_rights::<Thread>(handle_value, Rights::MANAGE_THREAD)?;
        if thread.proc().status() != Status::Running {
            return Err(ZxError::BAD_STATE);
        }
        thread.with_context(|ctx| {
            ctx.setup_uspace(entry, stack, &[arg1, arg2, 0]);
            // Match process startup: every Zircon thread can use FP/vector
            // state. AArch64 bare metal enables it on the first access trap.
            #[cfg(not(all(target_arch = "aarch64", not(feature = "libos"))))]
            ctx.enable_extended_state();
        })?;
        thread.start(self.thread_fn)?;
        Ok(())
    }

    /// Starts a thread and initializes the architecture TLS and ABI registers.
    #[allow(clippy::too_many_arguments)]
    pub fn sys_thread_start_regs(
        &self,
        handle_value: HandleValue,
        entry: usize,
        stack: usize,
        arg1: usize,
        arg2: usize,
        tp: usize,
        abi_reg: usize,
    ) -> ZxResult {
        info!(
            "thread.start_regs: handle={:#x?}, entry={:#x}, stack={:#x}, arg1={:#x}, arg2={:#x}, tp={:#x}, abi_reg={:#x}",
            handle_value, entry, stack, arg1, arg2, tp, abi_reg
        );
        let proc = self.thread.proc();
        let thread = proc.get_object_with_rights::<Thread>(handle_value, Rights::MANAGE_THREAD)?;
        if thread.proc().status() != Status::Running {
            return Err(ZxError::BAD_STATE);
        }
        thread.with_context(|ctx| {
            ctx.setup_uspace(entry, stack, &[arg1, arg2, 0]);
            ctx.set_field(UserContextField::ThreadPointer, tp);
            ctx.set_field(UserContextField::AbiRegister, abi_reg);
            #[cfg(not(all(target_arch = "aarch64", not(feature = "libos"))))]
            ctx.enable_extended_state();
        })?;
        thread.start(self.thread_fn)?;
        Ok(())
    }

    /// Yields execution to another runnable thread.
    pub async fn sys_thread_legacy_yield(&self, options: u32) -> ZxResult {
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        kernel_hal::thread::yield_now().await;
        Ok(())
    }

    /// Terminate the current running thread.
    ///
    /// Causes the currently running thread to cease running and exit.
    pub fn sys_thread_exit(&mut self) -> ZxResult {
        info!("thread.exit:");
        self.thread.exit();
        Ok(())
    }

    /// Suspend the given task.
    ///
    /// > This function replaces task_suspend. When all callers are updated, `zx_task_suspend()` will be deleted and this function will be renamed ```zx_task_suspend()```.
    pub fn sys_task_suspend_token(
        &self,
        handle: HandleValue,
        mut token: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!("task.suspend_token: handle={:?}, token={:?}", handle, token);
        let proc = self.thread.proc();
        // A handle that is not a thread's is an error that says which: a
        // process is `NOT_SUPPORTED`, anything else `WRONG_TYPE`, a thread
        // without the write right `ACCESS_DENIED`. All three used to answer
        // `OK` and write no token, so the caller read one that was never
        // there.
        let (object, rights) = proc.get_dyn_object_and_rights(handle)?;
        let thread = match object.downcast_arc::<Thread>() {
            Ok(thread) => thread,
            Err(object) => {
                return Err(if object.downcast_arc::<Process>().is_ok() {
                    ZxError::NOT_SUPPORTED
                } else {
                    ZxError::WRONG_TYPE
                })
            }
        };
        if !rights.contains(Rights::WRITE) {
            return Err(ZxError::ACCESS_DENIED);
        }
        if Arc::ptr_eq(&thread, self.thread) {
            return Err(ZxError::NOT_SUPPORTED);
        }
        if thread.state() == ThreadState::Dying || thread.state() == ThreadState::Dead {
            return Err(ZxError::BAD_STATE);
        }
        let thread: Arc<dyn Task> = thread;
        let token_handle =
            Handle::new(SuspendToken::create(&thread), Rights::DEFAULT_SUSPEND_TOKEN);
        install_handle(proc, token_handle, &mut token)
    }

    /// Kill the provided task (job, process, or thread).
    pub fn sys_task_kill(&mut self, handle: HandleValue) -> ZxResult {
        info!("task.kill: handle={:?}", handle);
        let proc = self.thread.proc();

        // As `sys_task_kill` in Zircon: the handle and its DESTROY right
        // first, whatever the object is, then the three task types. This
        // used to try the three types in turn and answer `WRONG_TYPE` to
        // whatever failed all three, so a bad handle was `WRONG_TYPE` instead
        // of `BAD_HANDLE`, and a job, process or thread handle without the
        // right was `WRONG_TYPE` instead of `ACCESS_DENIED`: the caller was
        // told it had the wrong kind of object when it had the right one and
        // not enough rights to it.
        let (object, rights) = proc.get_dyn_object_and_rights(handle)?;
        if !rights.contains(Rights::DESTROY) {
            return Err(ZxError::ACCESS_DENIED);
        }
        let object = match object.downcast_arc::<Job>() {
            Ok(job) => return Ok(job.kill()),
            Err(object) => object,
        };
        let object = match object.downcast_arc::<Process>() {
            Ok(process) => return Ok(process.kill()),
            Err(object) => object,
        };
        match object.downcast_arc::<Thread>() {
            Ok(thread) => Ok(thread.kill()),
            Err(_) => Err(ZxError::WRONG_TYPE),
        }
    }

    /// Create a new child job object given a parent job.
    pub fn sys_job_create(
        &self,
        parent: HandleValue,
        options: u32,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "job.create: parent={:#x}, options={:#x}, out={:#x?}",
            parent, options, out
        );
        if options != 0 {
            Err(ZxError::INVALID_ARGS)
        } else {
            let proc = self.thread.proc();
            let parent_job = proc
                .get_object_with_rights::<Job>(parent, Rights::MANAGE_JOB)
                .or_else(|_| proc.get_object_with_rights::<Job>(parent, Rights::WRITE))?;
            let child = parent_job.create_child()?;
            install_handle(proc, Handle::new(child, Rights::DEFAULT_JOB), &mut out)
        }
    }

    /// Sets one or more security and/or resource policies to an empty job.
    pub fn sys_job_set_policy(
        &self,
        handle: HandleValue,
        options: u32,
        topic: u32,
        policy: usize,
        count: u32,
    ) -> ZxResult {
        info!(
            "job.set_policy: handle={:#x}, options={:#x}, topic={:#x}, policy={:#x?}, count={:#x}",
            handle, options, topic, policy, count,
        );
        let proc = self.thread.proc();
        let job = proc.get_object_with_rights::<Job>(handle, Rights::SET_POLICY)?;
        match topic {
            JOB_POL_BASE_V1 | JOB_POL_BASE_V2 => {
                let policy_option = match options {
                    JOB_POL_RELATIVE => SetPolicyOptions::Relative,
                    JOB_POL_ABSOLUTE => SetPolicyOptions::Absolute,
                    _ => return Err(ZxError::INVALID_ARGS),
                };
                let policies = basic_policies(topic, policy, count)?;
                job.set_policy_basic(policy_option, &policies)
            }
            JOB_POL_TIMER_SLACK => {
                if options != JOB_POL_RELATIVE {
                    return Err(ZxError::INVALID_ARGS);
                }
                if count != 1 {
                    return Err(ZxError::INVALID_ARGS);
                }
                let timer_policy = UserInPtr::<TimerSlackPolicy>::from(policy).read()?;
                job.set_policy_timer_slack(timer_policy)
            }
            _ => Err(ZxError::INVALID_ARGS),
        }
    }

    /// Read from the given process's address space.
    ///
    /// > This function will eventually be replaced with something vmo-centric.
    pub fn sys_process_read_memory(
        &self,
        handle_value: HandleValue,
        vaddr: usize,
        buffer: UserOutPtr<u8>,
        buffer_size: usize,
        mut actual: UserOutPtr<usize>,
    ) -> ZxResult {
        if buffer.is_null() || buffer_size == 0 || buffer_size > MAX_BLOCK {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let process =
            proc.get_object_with_rights::<Process>(handle_value, Rights::READ | Rights::WRITE)?;
        // Through a bounded kernel buffer, a chunk at a time. `vec![0u8;
        // buffer_size]` was an allocation of whatever the caller asked for, up
        // to `MAX_BLOCK`: 64 MiB of kernel heap that a machine may not have
        // to spare, and an allocation the heap cannot serve is not an error
        // here but a kernel panic. Zircon copies straight into the caller's
        // pages for the same reason.
        let vmar = process.vmar();
        let mut chunk = vec![0u8; buffer_size.min(READ_MEMORY_CHUNK)];
        let mut done = 0;
        while done < buffer_size {
            let want = (buffer_size - done).min(chunk.len());
            let len = vmar.read_memory(vaddr + done, &mut chunk[..want])?;
            buffer.add(done).write_array(&chunk[..len])?;
            done += len;
            // The mapping ends here: what `read_memory` answers, and the
            // whole of what Zircon reads for one call.
            if len < want {
                break;
            }
        }
        actual.write(done)?;
        Ok(())
    }

    /// Write into the given process's address space.
    pub fn sys_process_write_memory(
        &self,
        handle_value: HandleValue,
        vaddr: usize,
        buffer: UserInPtr<u8>,
        buffer_size: usize,
        mut actual: UserOutPtr<usize>,
    ) -> ZxResult {
        if buffer.is_null() || buffer_size == 0 || buffer_size > MAX_BLOCK {
            Err(ZxError::INVALID_ARGS)
        } else {
            let len = self
                .thread
                .proc()
                .get_object_with_rights::<Process>(handle_value, Rights::READ | Rights::WRITE)?
                .vmar()
                .write_memory(vaddr, buffer.as_slice(buffer_size)?)?;
            actual.write(len)?;
            Ok(())
        }
    }
}

const JOB_POL_BASE_V1: u32 = 0;
const JOB_POL_BASE_V2: u32 = 0x0100_0000;
const JOB_POL_TIMER_SLACK: u32 = 1;

const JOB_POL_RELATIVE: u32 = 0;
const JOB_POL_ABSOLUTE: u32 = 1;

const MAX_BLOCK: usize = 64 * 1024 * 1024; //64M
/// Larger than any register set `zx_thread_read_state` can answer with.
const MAX_THREAD_STATE_SIZE: usize = 4096;
/// The kernel buffer `zx_process_read_memory` copies through, per round.
const READ_MEMORY_CHUNK: usize = 64 * 1024;

/// `zx_policy_basic_v2_t`: `zx_policy_basic_v1_t` (condition, action) plus a
/// `flags` word, twelve bytes to the v1's eight.
///
/// `ZX_JOB_POL_BASIC_V2` used to be read with the v1 layout: the first entry
/// came out right, and from the second on the kernel was reading each
/// entry's `flags` as the next entry's condition and each condition as an
/// action. Two v2 entries `[NEW_VMO deny, NEW_CHANNEL allow]` were parsed as
/// `[NEW_VMO deny, BAD_HANDLE action=5]`, which is not an action, so the call
/// was `INVALID_ARGS`; other arrays parsed into policies nobody asked for.
/// `ZX_JOB_POL_BASIC` has meant the v2 layout in the Zircon SDK since 2020,
/// so this is what every policy-setting program sends.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct BasicPolicyV2 {
    condition: u32,
    action: u32,
    /// `ZX_POL_OVERRIDE_ALLOW` (0) or `ZX_POL_OVERRIDE_DENY` (1): whether a
    /// child job may set this condition differently. Every policy here
    /// behaves as `OVERRIDE_DENY`, which is what a v1 policy is; the value is
    /// checked, not modelled.
    flags: u32,
}

const ZX_POL_OVERRIDE_ALLOW: u32 = 0;
const ZX_POL_OVERRIDE_DENY: u32 = 1;

/// The basic policies of a `zx_job_set_policy` call, read at the size the
/// topic says they have.
fn basic_policies(topic: u32, policy: usize, count: u32) -> ZxResult<Vec<BasicPolicy>> {
    let count = count as usize;
    if topic == JOB_POL_BASE_V1 {
        return UserInPtr::<BasicPolicy>::from(policy)
            .read_array(count)
            .map_err(ZxError::from);
    }
    UserInPtr::<BasicPolicyV2>::from(policy)
        .read_array(count)?
        .into_iter()
        .map(|v2| {
            if v2.flags != ZX_POL_OVERRIDE_ALLOW && v2.flags != ZX_POL_OVERRIDE_DENY {
                return Err(ZxError::INVALID_ARGS);
            }
            Ok(BasicPolicy {
                condition: v2.condition,
                action: v2.action,
            })
        })
        .collect()
}
