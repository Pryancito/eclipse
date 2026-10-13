use {super::*, numeric_enum_macro::numeric_enum, zircon_object::task::*};

impl Syscall<'_> {
    /// Creates a channel which will receive exceptions from the thread, process, or job.
    pub fn sys_create_exception_channel(
        &self,
        task: HandleValue,
        option: u32,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "create_exception_channel: task={:#x}, options={:#x}, out={:#x?}",
            task, option, out
        );
        let proc = self.thread.proc();
        let (task, rights) = proc.get_dyn_object_and_rights(task)?;
        // Type first: rights/options used to hide WRONG_TYPE on a VMO/Event/etc.
        let base = Rights::INSPECT | Rights::DUPLICATE | Rights::TRANSFER | Rights::MANAGE_THREAD;
        let exceptionate = if let Ok(job) = task.clone().downcast_arc::<Job>() {
            if !rights.contains(base | Rights::ENUMERATE) {
                return Err(ZxError::ACCESS_DENIED);
            }
            let option =
                ExceptionChannelOption::try_from(option).map_err(|_| ZxError::INVALID_ARGS)?;
            match option {
                ExceptionChannelOption::None => job.exceptionate(),
                ExceptionChannelOption::Debugger => job.debug_exceptionate(),
            }
        } else if let Ok(process) = task.clone().downcast_arc::<Process>() {
            if !rights.contains(base | Rights::ENUMERATE) {
                return Err(ZxError::ACCESS_DENIED);
            }
            let option =
                ExceptionChannelOption::try_from(option).map_err(|_| ZxError::INVALID_ARGS)?;
            match option {
                ExceptionChannelOption::None => process.exceptionate(),
                ExceptionChannelOption::Debugger => process.debug_exceptionate(),
            }
        } else if let Ok(thread) = task.clone().downcast_arc::<Thread>() {
            if !rights.contains(base) {
                return Err(ZxError::ACCESS_DENIED);
            }
            let option =
                ExceptionChannelOption::try_from(option).map_err(|_| ZxError::INVALID_ARGS)?;
            match option {
                ExceptionChannelOption::None => thread.exceptionate(),
                ExceptionChannelOption::Debugger => return Err(ZxError::INVALID_ARGS),
            }
        } else {
            return Err(ZxError::WRONG_TYPE);
        };
        install_handle(
            proc,
            Handle::new(
                exceptionate.create_channel(rights)?,
                Rights::TRANSFER | Rights::WAIT | Rights::READ,
            ),
            &mut out,
        )
    }

    /// Raise a user-generated exception on the calling thread.
    ///
    /// Nothing has gone wrong: the thread is reporting something of its own to
    /// whoever is debugging its job, which is what a C runtime does to tell a
    /// debugger about a failed assertion before it decides whether to die.
    ///
    /// The three ways this can be rejected are all `INVALID_ARGS`, and all of
    /// them were previously reachable only as `NOT_SUPPORTED`, because the
    /// call did not exist: the job debugger is the only target the call names,
    /// `ZX_EXCP_USER` the only type it accepts, and the context is not
    /// optional. Nobody listening is **not** one of them -- delivering a
    /// synthetic exception to an empty chain is success, and in particular it
    /// must not kill the process.
    pub async fn sys_thread_raise_exception(
        &self,
        target: u32,
        type_: u32,
        context: UserInPtr<u32>,
    ) -> ZxResult {
        info!(
            "thread.raise_exception: target={:#x}, type={:#x}, context={:#x?}",
            target, type_, context
        );
        if target != ZX_EXCEPTION_TARGET_JOB_DEBUGGER
            || type_ != ExceptionType::User as u32
            || context.is_null()
        {
            return Err(ZxError::INVALID_ARGS);
        }
        // The whole struct has to be the caller's own readable memory before a
        // word of it is read, exactly as the futex word is checked: reading
        // any other address would fault as the kernel, with no fixup behind
        // it.
        crate::user_memory::validate_user_range(
            self.thread.proc(),
            context.as_addr(),
            EXCEPTION_CONTEXT_SIZE,
            kernel_hal::MMUFlags::READ,
        )?;
        let synth = UserInPtr::<u32>::from(context.as_addr() + EXCEPTION_CONTEXT_SYNTH_OFFSET)
            .read_array(2)?;
        self.thread.raise_user_exception(synth[0], synth[1]).await;
        Ok(())
    }

    /// Create a handle for the exception's thread.
    ///    
    /// The exception handle out will be filled with a new handle to the exception thread.  
    pub fn sys_exception_get_thread(
        &self,
        exception: HandleValue,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!("exception_get_thread: exception={:#x}", exception);
        let proc = self.thread.proc();
        let exception =
            proc.get_object_with_rights::<ExceptionObject>(exception, Rights::default())?;
        install_handle(proc, exception.get_thread_handle(), &mut out)
    }

    /// Create a handle for the exception's process.  
    ///
    /// The exception handle out will be filled with a new handle to the exception process.    
    /// > Only available for job and process exception channels.      
    /// > Thread exceptions cannot access their parent process handles.    
    pub fn sys_exception_get_process(
        &self,
        exception: HandleValue,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!("exception_get_process: exception={:#x}", exception);
        let proc = self.thread.proc();
        let exception =
            proc.get_object_with_rights::<ExceptionObject>(exception, Rights::default())?;
        install_handle(proc, exception.get_process_handle()?, &mut out)
    }
}

/// `ZX_EXCEPTION_TARGET_JOB_DEBUGGER`, the only target
/// `zx_thread_raise_exception` takes.
const ZX_EXCEPTION_TARGET_JOB_DEBUGGER: u32 = 1;

numeric_enum! {
    #[repr(u32)]
    pub enum ExceptionChannelOption {
        None = 0,
        Debugger = 1
    }
}
