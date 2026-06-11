use super::*;
use core::fmt::Debug;
use core::mem::size_of;

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use bitflags::bitflags;

use kernel_hal::context::{UserContext, UserContextField};
use linux_object::fs::{FileLike, PidFd};
use linux_object::process::{wait_child, wait_child_any};
use linux_object::signal::SigInfo;
use linux_object::thread::{CurrentThreadExt, RobustList, ThreadExt};
use linux_object::time::TimeSpec;
use linux_object::{fs::INodeExt, loader::LinuxElfLoader};
use zircon_object::object::{KernelObject, KoID, Signal};
use zircon_object::task::Status;
use zircon_object::vm::USER_STACK_PAGES;

const P_ALL: i32 = 0;
const P_PID: i32 = 1;
const P_PGID: i32 = 2;
const P_PIDFD: i32 = 5;

fn write_sigchld_info(mut infop: UserOutPtr<SigInfo>, pid: KoID, status: i32) -> SysResult {
    if infop.is_null() {
        return Ok(0);
    }
    infop.write(SigInfo::child_exited(pid as i32, status))?;
    Ok(0)
}

fn is_child_process(parent: &zircon_object::task::Process, child: &zircon_object::task::Process) -> bool {
    parent.linux().has_child(child.id())
}

fn comm_from_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Syscalls for process.
///
/// # Menu
///
/// - [`fork`](Self::sys_fork)
/// - [`vfork`](Self::sys_vfork)
/// - [`clone`](Self::sys_clone)
/// - [`wait4`](Self::sys_wait4)
/// - [`execve`](Self::sys_execve)
/// - [`gettid`](Self::sys_gettid)
/// - [`getpid`](Self::sys_getpid)
/// - [`getppid`](Self::sys_getppid)
/// - [`exit`](Self::sys_exit)
/// - [`exit_group`](Self::sys_exit_group)
/// - [`nanosleep`](Self::sys_nanosleep)
/// - [`set_tid_address`](Self::sys_set_tid_address)
impl Syscall<'_> {
    /// `fork` creates a new process by duplicating the calling process
    /// (see [linux man fork(2)](https://www.man7.org/linux/man-pages/man2/fork.2.html)).
    /// The new process is referred to as the child process.
    /// The calling process is referred to as the parent process.
    ///
    /// The child process and the parent process run in separate memory spaces.
    /// At the time of `fork` both memory spaces have the same content.
    /// Memory writes, file mappings ([`Self::sys_mmap`]) and unmappings ([`Self::sys_munmap`])
    /// performed by one of the processes do not affect the other.
    ///
    /// The child process is an exact duplicate of the parent process except for the following points:
    ///
    /// - The child has its own unique process ID, and this PID does not match the ID of any existing process.
    /// - The child's parent process ID is the same as the parent's process ID.
    /// - Process resource utilizations ([`Self::sys_getrusage`]) and CPU time counters ([`Self::sys_times`]) are reset to zero in the child.
    /// - The child does not inherit semaphore adjustments from its parent ([`Self::sys_semop`]).
    /// - The child does not inherit process-associated record locks from its parent ([`Self::sys_fcntl`]).
    ///   (On the other hand, it does inherit [`Self::sys_fcntl`] open file description locks and [`Self::sys_flock`] locks from its parent.)
    ///
    /// Note the following further points:
    ///
    /// - The child process is created with a single thread—the one that called fork().
    ///   The entire virtual address space of the parent is replicated in the child,
    ///   including the states of mutexes and condition variables.
    /// - After a `fork` in a multithreaded program,
    ///   the child can safely call only async-signal-safe functions
    ///   until such time as it calls [`Self::sys_execve`].
    /// - The child inherits copies of the parent's set of open file descriptors.
    ///   Each file descriptor in the child refers to the same open file description (see [`Self::sys_open`])
    ///   as the corresponding file descriptor in the parent.
    ///   This means that the two file descriptors share open file status flags and file offset.
    pub fn sys_fork(&self, newsp: usize, newtls: usize) -> SysResult {
        info!("fork: newsp={:#x} newtls={:#x}", newsp, newtls);
        let new_proc = Process::fork_from(self.zircon_process(), false)?; // old pt NULL here
        let path = new_proc.linux().execute_path();
        if !path.is_empty() {
            new_proc.set_name(comm_from_path(&path));
        }
        let new_thread = Thread::create_linux(&new_proc)?;
        let mut new_ctx = self.thread.context_cloned()?;
        if newsp != 0 {
            new_ctx.set_field(UserContextField::StackPointer, newsp);
        }
        if newtls != 0 {
            new_ctx.set_field(UserContextField::ThreadPointer, newtls);
        }
        new_ctx.set_field(UserContextField::ReturnValue, 0);
        new_thread.with_context(|ctx| *ctx = new_ctx)?;
        new_thread.start(self.thread_fn)?;
        info!("fork: {} -> {}", self.zircon_process().id(), new_proc.id());
        Ok(new_proc.id() as usize)
    }

    /// `sys_vfork`, just like [`Self::sys_fork`], creates a child process of the calling process
    /// (see [linux man vfork(2)](https://www.man7.org/linux/man-pages/man2/vfork.2.html)).
    /// For details, see [`Self::sys_fork`].
    ///
    /// `sys_vfork` differs from [`Self::sys_fork`] in that the calling thread is suspended until the child terminates
    /// (either normally, by calling [`Self::sys_exit`], or abnormally, after delivery of a fatal signal),
    /// or it makes a call to [`Self::sys_execve`].
    pub async fn sys_vfork(&self, newsp: usize, newtls: usize) -> SysResult {
        info!("vfork: newsp={:#x} newtls={:#x}", newsp, newtls);
        self.zircon_process().vmar().dump();
        let new_proc = Process::fork_from(self.zircon_process(), true)?;
        new_proc.vmar().dump();
        let new_thread = Thread::create_linux(&new_proc)?;
        let mut new_ctx = self.thread.context_cloned()?;
        if newsp != 0 {
            new_ctx.set_field(UserContextField::StackPointer, newsp);
        }
        if newtls != 0 {
            new_ctx.set_field(UserContextField::ThreadPointer, newtls);
        }
        new_ctx.set_field(UserContextField::ReturnValue, 0);
        new_thread.with_context(|ctx| *ctx = new_ctx)?;
        new_thread.start(self.thread_fn)?;

        let new_proc: Arc<dyn KernelObject> = new_proc;
        info!(
            "vfork: {} -> {}. Waiting for execve SIGNALED",
            self.zircon_process().id(),
            new_proc.id()
        );
        new_proc.wait_signal(Signal::USER_SIGNAL_0 | Signal::PROCESS_TERMINATED).await; // wait for execve or termination
        Ok(new_proc.id() as usize)
    }

    /// `sys_clone` create a new thread in the current process.
    /// The new thread's stack pointer will be set to `newsp`,
    /// and thread pointer will be set to `newtls`.
    /// The child TID will be stored at both `parent_tid` and `child_tid`.
    ///
    /// > **NOTE!** This system call is not exactly the same as `clone` in Linux.
    ///
    /// > **NOTE!** This is partially implemented for `musl` only.
    pub async fn sys_clone(
        &self,
        flags: usize,
        newsp: usize,
        mut parent_tid: UserOutPtr<i32>,
        newtls: usize,
        mut child_tid: UserOutPtr<i32>,
    ) -> SysResult {
        let _flags = CloneFlags::from_bits_truncate(flags);
        info!(
            "clone: flags={:#x}, newsp={:#x}, parent_tid={:?}, child_tid={:?}, newtls={:#x}",
            flags, newsp, parent_tid, child_tid, newtls
        );
        // Fork-like clones: if the THREAD bit is not set, treat as fork.
        // This covers SIGCHLD (0x11), VFORK|SIGCHLD (0x4111), and other
        // fork-like combinations used by musl/glibc posix_spawn/system().
        let sigchld_bits = flags & 0xff;
        let has_thread = flags & CloneFlags::THREAD.bits() != 0;
        if !has_thread && (sigchld_bits == 0x11 || sigchld_bits == 0x00) && newsp == 0 {
            warn!("sys_clone is calling sys_fork for flags {:#x}", flags);
            return self.sys_fork(0, 0);
        }
        if flags == 0x4111 {
            // VFORK | VM | SIGCHILD
            warn!("sys_clone: dispatching to sys_vfork for flags {:#x}", flags);
            return self.sys_vfork(newsp, newtls).await;
        }
        if flags != 0x7d_0f00 && flags != 0x5d_0f00 {
            // 0x5d0f00: gcc of alpine linux
            // 0x7d0f00: pthread_create of alpine linux
            warn!("sys_clone: unsupported flags {:#x}, trying fork", flags);
            return self.sys_fork(newsp, newtls);
        }
        let new_thread = Thread::create_linux(self.zircon_process())?;
        let mut new_ctx = self.thread.context_cloned()?;
        new_ctx.set_field(UserContextField::StackPointer, newsp);
        new_ctx.set_field(UserContextField::ThreadPointer, newtls);
        new_ctx.set_field(UserContextField::ReturnValue, 0);
        info!(
            "clone: tid_parent={} context={:#x?}",
            self.thread.id(),
            new_ctx
        );
        new_thread.with_context(|ctx| *ctx = new_ctx)?;
        new_thread.start(self.thread_fn)?;

        let tid = new_thread.id();
        info!("clone: {} -> {}", self.thread.id(), tid);
        parent_tid.write(tid as i32)?;
        child_tid.write(tid as i32)?;
        new_thread.set_tid_address(child_tid);
        Ok(tid as usize)
    }

    /// `sys_wait4` suspends execution of the calling thread
    /// until a child specified by `pid` argument has changed state
    /// (see [linux man wait4(2)](https://www.man7.org/linux/man-pages/man2/wait4.2.html)).
    /// By default, `sys_wait4` waits only for terminated children,
    /// but this behavior is modifiable via the options argument, as described below.
    ///
    /// The value of `pid` can be:
    ///
    /// - **-1**: meaning wait for any child process.
    /// - **0**: meaning wait for any child process whose process group ID is equal to
    ///          that of the calling process at the time of the call to `sys_wait4`.
    /// - **>0**: meaning wait for the child whose process ID is equal to the value of `pid`.
    ///
    /// The value of options is an OR of zero or more of the following constants:
    ///
    /// - **NOHANG**    = 0x000_0001;
    ///
    ///   TODO
    ///
    /// - **STOPPED**   = 0x000_0002;
    ///
    ///   TODO
    ///
    /// - **EXITED**    = 0x000_0004;
    ///
    ///   TODO
    ///
    /// - **CONTINUED** = 0x000_0008;
    ///
    ///   TODO
    ///
    /// - **NOWAIT**    = 0x100_0000;
    ///
    ///   TODO
    ///
    /// On success, returns the process ID of the child whose state has changed;
    /// if `NOHANG` flag was specified and one or more child(ren) specified by pid exist,
    /// but have not yet changed state, then 0 is returned.
    /// On failure, -1 is returned.
    pub async fn sys_wait4(
        &self,
        pid: i32,
        mut wstatus: UserOutPtr<i32>,
        options: u32,
    ) -> SysResult {
        #[derive(Debug)]
        enum WaitTarget {
            AnyChild,
            AnyChildInGroup,
            Pid(KoID),
        }
        bitflags! {
            struct WaitFlags: u32 {
                const NOHANG    = 1;
                const STOPPED   = 2;
                const EXITED    = 4;
                const CONTINUED = 8;
                const NOWAIT    = 0x100_0000;
            }
        }
        let target = match pid {
            -1 => WaitTarget::AnyChild,
            0 => WaitTarget::AnyChildInGroup,
            p if p > 0 => WaitTarget::Pid(p as KoID),
            // pid < -1 means "any child in process group |pid|". Process groups
            // are not tracked here, so fall back to waiting on any child rather
            // than panicking the kernel on user-controlled input.
            _ => WaitTarget::AnyChildInGroup,
        };
        let flags = WaitFlags::from_bits_truncate(options);
        let nohang = flags.contains(WaitFlags::NOHANG);
        warn!(
            "wait4: target={:?}, wstatus={:?}, options={:?}",
            target, wstatus, flags,
        );
        let result = match target {
            WaitTarget::AnyChild | WaitTarget::AnyChildInGroup => {
                wait_child_any(self.zircon_process(), nohang).await
            }
            WaitTarget::Pid(pid) => wait_child(self.zircon_process(), pid, nohang)
                .await
                .map(|code| (pid, code)),
        };
        let (pid, code) = match result {
            Ok(pair) => pair,
            Err(LxError::EAGAIN) if nohang => {
                // WNOHANG: no child ready yet — return 0 per POSIX waitpid(2).
                wstatus.write_if_not_null(0)?;
                return Ok(0);
            }
            Err(e) => return Err(e),
        };
        wstatus.write_if_not_null(code)?;
        Ok(pid as usize)
    }

    /// Wait for a child state change (`waitid(2)`). Supports `P_PID`, `P_PIDFD`, and `P_ALL`.
    pub async fn sys_waitid(
        &self,
        idtype: i32,
        id: usize,
        infop: UserOutPtr<SigInfo>,
        options: u32,
    ) -> SysResult {
        bitflags! {
            struct WaitIdOptions: u32 {
                const WNOHANG   = 0x0100_0000;
                const WEXITED   = 0x0000_0004;
            }
        }
        let opts = WaitIdOptions::from_bits_truncate(options);
        let nohang = opts.contains(WaitIdOptions::WNOHANG);
        let caller = self.zircon_process();

        let (child_pid, status) = match idtype {
            P_PID => {
                if id == 0 {
                    return Err(LxError::EINVAL);
                }
                let code = wait_child(caller, id as KoID, nohang).await?;
                (id as KoID, code)
            }
            P_PIDFD => {
                let pidfd = PidFd::from_file_like(self.linux_process().get_file_like(id.into())?)?;
                let target = pidfd.target();
                if !is_child_process(caller, target) {
                    return Err(LxError::ECHILD);
                }
                if FileLike::flags(pidfd.as_ref()).non_block()
                    && !matches!(target.status(), Status::Exited(_))
                    && !nohang
                {
                    return Err(LxError::EAGAIN);
                }
                let code = wait_child(caller, target.id(), nohang).await?;
                (target.id(), code)
            }
            P_ALL => {
                let (pid, code) = wait_child_any(caller, nohang).await?;
                (pid, code)
            }
            P_PGID => return Err(LxError::ENOSYS),
            _ => return Err(LxError::EINVAL),
        };

        if opts.contains(WaitIdOptions::WEXITED) || options == 0 {
            let exit_status = status >> 8;
            write_sigchld_info(infop, child_pid, exit_status)?;
        }
        Ok(0)
    }

    /// `sys_execve` executes the program referred to by `path`
    /// (see [linux man execve(2)](https://www.man7.org/linux/man-pages/man2/execve.2.html)).
    /// This causes the program that is currently being run
    /// by the calling process to be replaced with a new program,
    /// with newly initialized stack, heap, and (initialized and uninitialized) data segments.
    ///
    /// `path` argument must be a binary executable file.
    ///
    /// `argv` is an array of argument strings passed to the new program.
    /// By convention, the first of these strings (i.e., `argv[0]`)
    /// should contain the filename associated with the file being executed.
    ///
    /// `envp` is an array of strings, conventionally of the form `key=value`,
    /// which are passed as environment to the new program.
    ///
    /// > **NOTE!** Differ from linux, `argv` & `envp` can not be NULL.
    ///
    /// > **NOTE!** For multi-thread programs,
    ///             A call to any exec function from a process with more than one thread
    ///             shall result in all threads being terminated and the new executable image
    ///             being loaded and executed.
    pub fn sys_execve(
        &mut self,
        path: UserInPtr<u8>,
        argv: UserInPtr<UserInPtr<u8>>,
        envp: UserInPtr<UserInPtr<u8>>,
    ) -> SysResult {
        let path_str = path.as_c_str().map_err(|e| {
            error!("execve: path.as_c_str() failed: {:?}", e);
            e
        })?;
        debug!("EXECVE ENTER: path={:?}", path_str);
        warn!("EXECVE: path={:?}", path_str);
        let args = argv.read_cstring_array().map_err(|e| {
            error!("execve: argv.read_cstring_array() failed: {:?}", e);
            e
        })?;
        let mut envs: Vec<String> = Vec::new();
        if !envp.is_null() {
            envs = envp.read_cstring_array().map_err(|e| {
                error!("execve: envp.read_cstring_array() failed: {:?}", e);
                e
            })?;
        }
        info!(
            "execve: path: {:?}, args: {:?}, envs: {:?}",
            path_str, args, envs
        );
        if args.is_empty() {
            error!("execve: args is empty");
            return Err(LxError::EINVAL);
        }
        if args[0].is_empty() {
            warn!("execve: argv[0] is empty for path {:?}", path_str);
        }

        // TODO: check and kill other threads

        // Read program file
        let proc = self.linux_process();
        let inode = proc.lookup_inode(path_str)?;
        let metadata = inode.metadata()?;
        proc.check_access(&metadata, 0o1, true)?;
        let vmo = inode.read_as_vmo()?;

        proc.remove_cloexec_files();

        // 注意！即将销毁旧应用程序的用户空间，现在将必要的信息拷贝到内核！
        // Notice! About to destroy the user space of the old application, now copy the necessary information into kernel!
        let path_str = path_str.to_string();
        let vmar = self.zircon_process().vmar();
        vmar.clear()?;

        let (entry, sp, initial_brk, execute_path) = LinuxElfLoader {
            syscall_entry: self.syscall_entry,
            stack_pages: USER_STACK_PAGES,
            root_inode: proc.root_inode().clone(),
        }
        .load(&vmar, &vmo, args.clone(), envs, path_str)
        .map_err(|e| {
            error!("execve: LinuxElfLoader::load failed: {:?}", e);
            e
        })?;
        proc.set_execute_path(&execute_path);
        proc.set_cmdline(args);
        proc.set_brk(initial_brk);
        proc.apply_exec_metadata(&metadata);
        self.zircon_process()
            .set_name(comm_from_path(&execute_path));

        self.zircon_process().signal_set(Signal::USER_SIGNAL_0);
        self.thread.with_context(|ctx| {
            *ctx = UserContext::new();
            ctx.setup_uspace(entry, sp, &[0, 0, 0]);
        })?;
        Ok(0)
    }

    //    pub fn sys_yield(&self) -> SysResult {
    //        thread::yield_now();
    //        Ok(0)
    //    }
    //

    /// `sys_gettid` returns the caller's thread ID (TID)
    /// (see [linux man gettid(2)](https://www.man7.org/linux/man-pages/man2/gettid.2.html)).
    /// In a single-threaded process, the thread ID is equal to the process ID (PID, as returned by [`Self::sys_getpid`]).
    /// In a multithreaded process, all threads have the same PID, but each one has a unique TID.
    pub fn sys_gettid(&self) -> SysResult {
        info!("gettid:");
        let tid = self.thread.id();
        Ok(tid as usize)
    }

    /// `sys_getpid` returns the process ID (PID) of the calling process
    /// (see [linux man getpid(2)](https://www.man7.org/linux/man-pages/man2/getpid.2.html)).
    pub fn sys_getpid(&self) -> SysResult {
        info!("getpid:");
        let proc = self.zircon_process();
        let pid = proc.id();
        Ok(pid as usize)
    }

    /// `sys_getppid` returns the process ID of the parent of the calling process
    /// (see [linux man getppid(2)](https://www.man7.org/linux/man-pages/man2/getpid.2.html)).
    /// This will be either the ID of the process that created this process using fork(),
    /// or, if that process has already terminated, 0.
    pub fn sys_getppid(&self) -> SysResult {
        info!("getppid:");
        let proc = self.linux_process();
        let ppid = proc.parent().map(|p| p.id()).unwrap_or(0);
        Ok(ppid as usize)
    }

    /// `sys_exit` system call terminates only the calling thread
    /// (see [linux man _exit(2)](https://www.man7.org/linux/man-pages/man2/exit.2.html),
    /// this syscall is same as a raw `_exit` in glibc),
    /// and actions such as reparenting child processes or sending
    /// SIGCHLD to the parent process are performed only if this is the
    /// last thread in the thread group.
    pub fn sys_exit(&mut self, exit_code: i32) -> SysResult {
        info!("exit: code={}", exit_code);
        self.thread.exit_linux(exit_code);
        Ok(0)
    }

    /// `sys_exit_group` is equivalent to [`Self::sys_exit`]
    /// except that it terminates not only the calling thread
    /// (see [linux man exit_group(2)](https://www.man7.org/linux/man-pages/man2/exit_group.2.html),
    /// but all threads in the calling process's thread group.
    /// As a result, the entire calling process will exit.
    pub fn sys_exit_group(&mut self, exit_code: i32) -> SysResult {
        info!("exit_group: code={}", exit_code);
        let proc = self.zircon_process();
        proc.exit(exit_code as i64);
        Ok(0)
    }

    /// Allows the calling thread to sleep for
    /// an interval specified with nanosecond precision
    /// (see [linux man nanosleep(2)](https://www.man7.org/linux/man-pages/man2/nanosleep.2.html).
    ///
    /// `nanosleep` suspends the execution of the calling thread
    /// until either at least the time specified in `req` has elapsed,
    /// or the delivery of a signal that triggers the invocation of a handler
    /// in the calling thread or that terminates the process.
    ///
    /// To represent a duration, see TimeSpec.
    pub async fn sys_nanosleep(&self, req: UserInPtr<TimeSpec>) -> SysResult {
        info!("nanosleep: deadline={:?}", req);
        let duration = req.read()?.into();
        let deadline = kernel_hal::timer::deadline_after(duration);
        loop {
            if let Err(e) = linux_object::process::check_signals() {
                return Err(e);
            }
            if kernel_hal::timer::timer_now() >= deadline {
                return Ok(0);
            }
            kernel_hal::thread::yield_now().await;
        }
    }

    //    pub fn sys_set_priority(&self, priority: usize) -> SysResult {
    //        let pid = thread::current().id();
    //        thread_manager().set_priority(pid, priority as u8);
    //        Ok(0)
    //    }

    /// `set_tid_address` sets the clear_child_tid value for the calling thread to `tidptr`,
    /// and return the caller's thread ID
    /// (see [linux man set_tid_address(2)](https://www.man7.org/linux/man-pages/man2/set_tid_address.2.html).
    pub fn sys_set_tid_address(&self, tidptr: UserOutPtr<i32>) -> SysResult {
        info!("set_tid_address: {:?}", tidptr);
        self.thread.set_tid_address(tidptr);
        let tid = self.thread.id();
        Ok(tid as usize)
    }

    /// Get robust list.
    pub fn sys_get_robust_list(
        &self,
        pid: i32,
        head_ptr: UserOutPtr<UserOutPtr<RobustList>>,
        len_ptr: UserOutPtr<usize>,
    ) -> SysResult {
        if pid == 0 {
            return self.thread.get_robust_list(head_ptr, len_ptr);
        }
        Ok(0)
    }

    /// Set robust list.
    pub fn sys_set_robust_list(&self, head: UserInPtr<RobustList>, len: usize) -> SysResult {
        if len != size_of::<RobustList>() {
            return Err(LxError::EINVAL);
        }
        self.thread.set_robust_list(head, len);
        Ok(0)
    }

    /// `getuid` returns the real user ID of the calling process.
    pub fn sys_getuid(&self) -> SysResult {
        debug!("getuid");
        Ok(self.linux_process().uid() as usize)
    }

    /// `geteuid` returns the effective user ID of the calling process.
    pub fn sys_geteuid(&self) -> SysResult {
        debug!("geteuid");
        Ok(self.linux_process().euid() as usize)
    }

    /// `getgid` returns the real group ID of the calling process.
    pub fn sys_getgid(&self) -> SysResult {
        debug!("getgid");
        Ok(self.linux_process().gid() as usize)
    }

    /// `getegid` returns the effective group ID of the calling process.
    pub fn sys_getegid(&self) -> SysResult {
        debug!("getegid");
        Ok(self.linux_process().egid() as usize)
    }

    /// `umask` updates and returns the previous creation mask.
    pub fn sys_umask(&self, mask: usize) -> SysResult {
        Ok(self.linux_process().set_umask(mask as u16) as usize)
    }

    /// `setuid` changes the calling process user identity.
    pub fn sys_setuid(&self, uid: usize) -> SysResult {
        self.linux_process().set_uid(uid as u32)?;
        Ok(0)
    }

    /// `setgid` changes the calling process group identity.
    pub fn sys_setgid(&self, gid: usize) -> SysResult {
        self.linux_process().set_gid(gid as u32)?;
        Ok(0)
    }

    /// `setreuid` changes the real/effective user IDs.
    pub fn sys_setreuid(&self, ruid: usize, euid: usize) -> SysResult {
        self.linux_process().set_reuid(ruid as u32, euid as u32)?;
        Ok(0)
    }

    /// `setregid` changes the real/effective group IDs.
    pub fn sys_setregid(&self, rgid: usize, egid: usize) -> SysResult {
        self.linux_process().set_regid(rgid as u32, egid as u32)?;
        Ok(0)
    }

    /// `setresuid` changes the real/effective/saved user IDs.
    pub fn sys_setresuid(&self, ruid: usize, euid: usize, suid: usize) -> SysResult {
        self.linux_process()
            .set_resuid(ruid as u32, euid as u32, suid as u32)?;
        Ok(0)
    }

    /// `setresgid` changes the real/effective/saved group IDs.
    pub fn sys_setresgid(&self, rgid: usize, egid: usize, sgid: usize) -> SysResult {
        self.linux_process()
            .set_resgid(rgid as u32, egid as u32, sgid as u32)?;
        Ok(0)
    }

    /// `getgroups` returns supplementary group IDs.
    pub fn sys_getgroups(&self, size: usize, mut list: UserOutPtr<u32>) -> SysResult {
        let groups = self.linux_process().groups();
        if size == 0 {
            return Ok(groups.len());
        }
        if size < groups.len() {
            return Err(LxError::EINVAL);
        }
        list.write_array(groups.as_slice())?;
        Ok(groups.len())
    }

    /// `setgroups` updates supplementary group IDs.
    pub fn sys_setgroups(&self, size: usize, list: UserInPtr<u32>) -> SysResult {
        if !self.linux_process().is_superuser() {
            return Err(LxError::EPERM);
        }
        let groups = if size == 0 {
            Vec::new()
        } else {
            list.read_array(size)?
        };
        self.linux_process().set_groups(groups);
        Ok(0)
    }

    /// `setpgid` sets the PGID of the process specified by pid to pgid.
    pub fn sys_setpgid(&self, pid: usize, pgid: usize) -> SysResult {
        debug!("setpgid: pid={}, pgid={}", pid, pgid);
        // Stub: return success
        Ok(0)
    }

    /// `getpgid` returns the PGID of the process specified by pid.
    pub fn sys_getpgid(&self, pid: usize) -> SysResult {
        debug!("getpgid: pid={}", pid);
        // Stub: return pid as its own pgid
        let proc = if pid == 0 {
            self.zircon_process().id()
        } else {
            pid as u64
        };
        Ok(proc as usize)
    }

    /// `setsid` creates a new session if the calling process is not a process group leader.
    pub fn sys_setsid(&self) -> SysResult {
        debug!("setsid");
        // Stub: return current pid as new sid
        Ok(self.zircon_process().id() as usize)
    }

    /// `chmod` changes the mode of the file specified by path.
    pub fn sys_chmod(&self, path: UserInPtr<u8>, mode: usize) -> SysResult {
        let path = path.as_c_str()?;
        debug!("chmod: path={:?}, mode={:#o}", path, mode);
        let proc = self.linux_process();
        let inode = proc.lookup_inode(path)?;
        let mut metadata = inode.metadata()?;
        proc.chmod_metadata(&mut metadata, mode as u16)?;
        inode.set_metadata(&metadata)?;
        Ok(0)
    }

    /// `getresuid` returns the real, effective, and saved user IDs.
    pub fn sys_getresuid(
        &self,
        mut ruid: UserOutPtr<u32>,
        mut euid: UserOutPtr<u32>,
        mut suid: UserOutPtr<u32>,
    ) -> SysResult {
        debug!("getresuid: ruid={:?}, euid={:?}, suid={:?}", ruid, euid, suid);
        let creds = self.linux_process().credentials();
        ruid.write(creds.ruid)?;
        euid.write(creds.euid)?;
        suid.write(creds.suid)?;
        Ok(0)
    }

    /// `getresgid` returns the real, effective, and saved group IDs.
    pub fn sys_getresgid(
        &self,
        mut rgid: UserOutPtr<u32>,
        mut egid: UserOutPtr<u32>,
        mut sgid: UserOutPtr<u32>,
    ) -> SysResult {
        debug!("getresgid: rgid={:?}, egid={:?}, sgid={:?}", rgid, egid, sgid);
        let creds = self.linux_process().credentials();
        rgid.write(creds.rgid)?;
        egid.write(creds.egid)?;
        sgid.write(creds.sgid)?;
        Ok(0)
    }

    /// `setfsuid` sets the user ID used for filesystem checks.
    pub fn sys_setfsuid(&self, fsuid: usize) -> SysResult {
        debug!("setfsuid: fsuid={}", fsuid);
        let old_fsuid = self.linux_process().euid() as usize;
        Ok(old_fsuid)
    }

    /// `setfsgid` sets the group ID used for filesystem checks.
    pub fn sys_setfsgid(&self, fsgid: usize) -> SysResult {
        debug!("setfsgid: fsgid={}", fsgid);
        let old_fsgid = self.linux_process().egid() as usize;
        Ok(old_fsgid)
    }
}

bitflags! {
    pub struct CloneFlags: usize {
        ///
        const CSIGNAL =         0xff;
        /// the calling process and the child process run in the same memory space
        const VM =              1 << 8;
        /// the caller and the child process share the same filesystem information
        const FS =              1 << 9;
        /// the calling process and the child process share the same file descriptor table
        const FILES =           1 << 10;
        /// the calling process and the child process share the same table of signal handlers.
        const SIGHAND =         1 << 11;
        /// the calling process is being traced
        const PTRACE =          1 << 13;
        /// the execution of the calling process is suspended until the child releases its virtual memory resources
        const VFORK =           1 << 14;
        /// the parent of the new child will be the same as that of the call‐ing process.
        const PARENT =          1 << 15;
        /// the child is placed in the same thread group as the calling process.
        const THREAD =          1 << 16;
        /// cloned child is started in a new mount namespace
        const NEWNS	=           1 << 17;
        /// the child and the calling process share a single list of System V semaphore adjustment values.
        const SYSVSEM =         1 << 18;
        /// architecture dependent, The TLS (Thread Local Storage) descriptor is set to tls.
        const SETTLS =          1 << 19;
        /// Store the child thread ID at the location in the parent's memory.
        const PARENT_SETTID =   1 << 20;
        /// Clear (zero) the child thread ID
        const CHILD_CLEARTID =  1 << 21;
        /// the parent not to receive a signal when the child terminated
        const DETACHED =        1 << 22;
        /// a tracing process cannot force CLONE_PTRACE on this child process.
        const UNTRACED =        1 << 23;
        /// Store the child thread ID
        const CHILD_SETTID =    1 << 24;
        /// Create the process in a new cgroup namespace.
        const NEWCGROUP =       1 << 25;
        /// create the process in a new UTS namespace
        const NEWUTS =          1 << 26;
        /// create the process in a new IPC namespace.
        const NEWIPC =          1 << 27;
        /// create the process in a new user namespace
        const NEWUSER =         1 << 28;
        /// create the process in a new PID namespace
        const NEWPID =          1 << 29;
        /// create the process in a new net‐work namespace.
        const NEWNET =          1 << 30;
        /// the new process shares an I/O context with the calling process.
        const IO =              1 << 31;
    }
}
