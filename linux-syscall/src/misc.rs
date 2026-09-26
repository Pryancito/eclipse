use super::*;
use core::time::Duration;
use kernel_hal::timer::timer_now;
use linux_object::process::{CAP_SYS_ADMIN, CAP_SYS_BOOT, CAP_SYS_NICE, CAP_SYS_RESOURCE};
use linux_object::thread::ThreadExt;
use linux_object::time::*;
use zircon_object::task::ThreadState;
use zircon_object::{ZxError, ZxResult};

impl Syscall<'_> {
    #[cfg(target_arch = "x86_64")]
    /// set architecture-specific thread state
    /// for x86_64 currently
    ///
    /// `GS` matters as much as `FS` here. wasm2c's *segue* bounds checking --
    /// what Firefox's RLBox sandboxes compile to -- addresses the sandbox heap
    /// through `%gs:`, and sets the base with `arch_prctl(ARCH_SET_GS, ..)`.
    /// It does not fall back when that fails:
    ///
    ///     wasm_rt_syscall_set_segue_base error: Invalid argument
    ///     Redirecting call to abort() to mozalloc_abort
    ///
    /// which is how Firefox died here while only `ARCH_SET_FS` was answered.
    pub fn sys_arch_prctl(&mut self, code: i32, addr: usize) -> SysResult {
        use kernel_hal::context::UserContextField;
        use zircon_object::vm::{USER_ASPACE_BASE, USER_ASPACE_SIZE};

        const ARCH_SET_GS: i32 = 0x1001;
        const ARCH_SET_FS: i32 = 0x1002;
        const ARCH_GET_FS: i32 = 0x1003;
        const ARCH_GET_GS: i32 = 0x1004;

        // Both bases reach the CPU through `wrmsr` on the way back to user
        // mode, and `wrmsr` to IA32_FS_BASE/IA32_GS_BASE raises #GP when the
        // value is not canonical -- in kernel mode, on the exit path, where it
        // is a kernel fault rather than a user one. Linux rejects the same
        // addresses with EPERM before they can get that far.
        let settable = |addr: usize| {
            if addr < (USER_ASPACE_BASE + USER_ASPACE_SIZE) as usize {
                Ok(())
            } else {
                Err(LxError::EPERM)
            }
        };

        match code {
            ARCH_SET_FS => {
                info!("sys_arch_prctl: set FSBASE to {:#x}", addr);
                settable(addr)?;
                self.thread
                    .with_context(|ctx| ctx.set_field(UserContextField::ThreadPointer, addr))?;
                Ok(0)
            }
            ARCH_SET_GS => {
                info!("sys_arch_prctl: set GSBASE to {:#x}", addr);
                // The libos trap path drops the user `gsbase` on the floor --
                // the host runtime owns `gs` there -- so accepting this would
                // promise user code a base it will never see. Say no instead.
                if kernel_hal::LIBOS {
                    return Err(LxError::EINVAL);
                }
                settable(addr)?;
                self.thread
                    .with_context(|ctx| ctx.general_mut().gsbase = addr)?;
                Ok(0)
            }
            ARCH_GET_FS => {
                let base = self
                    .thread
                    .with_context(|ctx| ctx.get_field(UserContextField::ThreadPointer))?;
                UserOutPtr::<usize>::from(addr).write(base)?;
                Ok(0)
            }
            ARCH_GET_GS => {
                let base = self.thread.with_context(|ctx| ctx.general().gsbase)?;
                UserOutPtr::<usize>::from(addr).write(base)?;
                Ok(0)
            }
            _ => Err(LxError::EINVAL),
        }
    }

    /// get name and information about current kernel
    ///
    /// All six `utsname` strings come from [`linux_object::uname`], the same
    /// source `/proc/version` and `/proc/sys/kernel/*` read, so every interface
    /// reports one consistent identity. The release string is Linux-formatted
    /// ("0.4.2-eclipse"): glibc and the Go runtime parse it at startup and
    /// refuse to run when it looks older than their build-time minimum — the
    /// crate version previously reported here ("0.1.0-…") failed exactly that.
    pub fn sys_uname(&self, mut buf: UserOutPtr<u8>) -> SysResult {
        info!("uname: buf={:?}", buf);

        use linux_object::uname;

        let version = uname::os_version();
        let hostname = uname::hostname();
        let domainname = uname::domainname();

        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else if cfg!(target_arch = "riscv64") {
            "riscv64"
        } else {
            "unknown"
        };

        // The whole 390-byte struct, each name cut to its 65-byte field
        // (`sys_newuname` copies the arrays, not the strings). Writing
        // each name with `write_cstring` at its offset let a name longer
        // than the field run into the next one, and the last one past the
        // caller's struct.
        let utsname = uname::utsname_bytes([
            uname::OS_TYPE.as_bytes(),    // sysname
            hostname.as_bytes(),          // nodename
            uname::OS_RELEASE.as_bytes(), // release
            version.as_bytes(),           // version
            arch.as_bytes(),              // machine
            domainname.as_bytes(),        // domainname
        ]);
        buf.write_array(&utsname)?;
        Ok(0)
    }

    /// set the system hostname
    /// (see [linux man sethostname(2)](https://www.man7.org/linux/man-pages/man2/sethostname.2.html)).
    ///
    /// Alpine's boot scripts (`busybox hostname -F /etc/hostname`) and the
    /// `hostname` utility depend on this; the new name is immediately visible
    /// through `uname`, `gethostname` and `/proc/sys/kernel/hostname`.
    pub fn sys_sethostname(&mut self, base: UserInPtr<u8>, len: usize) -> SysResult {
        info!("sethostname: base={:?}, len={}", base, len);
        // `kernel/sys.c`: `if (!ns_capable(current->nsproxy->uts_ns->user_ns,
        // CAP_SYS_ADMIN)) return -EPERM;`, ahead of the length check.
        if !self.linux_process().capable(CAP_SYS_ADMIN) {
            return Err(LxError::EPERM);
        }
        if len > linux_object::uname::HOST_NAME_MAX {
            return Err(LxError::EINVAL);
        }
        let bytes = base.read_array(len)?;
        let name = alloc::string::String::from_utf8_lossy(&bytes);
        linux_object::uname::set_hostname(&name);
        Ok(0)
    }

    /// set the system NIS domain name
    /// (see [linux man setdomainname(2)](https://www.man7.org/linux/man-pages/man2/setdomainname.2.html)).
    pub fn sys_setdomainname(&mut self, base: UserInPtr<u8>, len: usize) -> SysResult {
        info!("setdomainname: base={:?}, len={}", base, len);
        // `kernel/sys.c`: `if (!ns_capable(current->nsproxy->uts_ns->user_ns,
        // CAP_SYS_ADMIN)) return -EPERM;`, ahead of the length check.
        if !self.linux_process().capable(CAP_SYS_ADMIN) {
            return Err(LxError::EPERM);
        }
        if len > linux_object::uname::HOST_NAME_MAX {
            return Err(LxError::EINVAL);
        }
        let bytes = base.read_array(len)?;
        let name = alloc::string::String::from_utf8_lossy(&bytes);
        linux_object::uname::set_domainname(&name);
        Ok(0)
    }

    /// get/set the process execution domain
    /// (see [linux man personality(2)](https://www.man7.org/linux/man-pages/man2/personality.2.html)).
    ///
    /// The persona is a real per-process value: `0xffffffff` queries without
    /// changing it, anything else is stored and the previous persona returned.
    /// Everything runs as `PER_LINUX`; modifier bits such as
    /// `ADDR_NO_RANDOMIZE` (what `setarch -R` sets — trivially satisfied here,
    /// mappings are not randomized) are kept so the caller reads back what it
    /// configured and inheritance across fork/exec behaves like Linux.
    pub fn sys_personality(&self, persona: usize) -> SysResult {
        const QUERY: u32 = 0xffff_ffff;
        let proc = self.linux_process();
        if persona as u32 == QUERY {
            return Ok(proc.personality() as usize);
        }
        info!("personality: persona={:#x}", persona);
        Ok(proc.set_personality(persona as u32) as usize)
    }

    /// determine CPU and NUMA node on which the calling thread is running
    /// (see [linux man getcpu(2)](https://www.man7.org/linux/man-pages/man2/getcpu.2.html)).
    ///
    /// musl's `sched_getcpu` is a direct wrapper over this syscall, and thread
    /// pools / allocators (jemalloc arenas, Go's scheduler instrumentation) use
    /// it for CPU-local sharding. There is a single NUMA node here.
    pub fn sys_getcpu(
        &self,
        mut cpu: UserOutPtr<u32>,
        mut node: UserOutPtr<u32>,
        _tcache: usize,
    ) -> SysResult {
        cpu.write_if_not_null(kernel_hal::cpu::cpu_id() as u32)?;
        node.write_if_not_null(0)?;
        Ok(0)
    }

    /// `ioprio_get(2)`: the I/O priority of the task `which`/`who` names,
    /// or the best (numerically lowest) one across a process group or a
    /// user's tasks, as `ioprio_best` folds them.
    ///
    /// A task that never set one reports what `get_task_ioprio` derives
    /// from its nice value: best-effort, level `(nice + 20) / 5`. See
    /// [`effective_ioprio`].
    pub fn sys_ioprio_get(&self, which: usize, who: usize) -> SysResult {
        info!("ioprio_get: which={}, who={}", which, who);
        let which = ioprio_which(which)?;
        let best = self
            .priority_targets(which, who)?
            .iter()
            .map(|t| effective_ioprio(t.lock_linux().ioprio, t.sched_nice()))
            .reduce(ioprio_best)
            .ok_or(LxError::ESRCH)?;
        Ok(best as usize)
    }

    /// `ioprio_set(2)`: record the I/O priority of every task `which`/`who`
    /// names.
    ///
    /// This used to validate the class and answer 0, storing nothing and
    /// asking nothing: `ionice -c3 -p PID` on a pid that did not exist
    /// succeeded, on somebody else's process succeeded, `-c1` (real-time)
    /// was granted to anyone, and `ionice -p PID` afterwards reported the
    /// default whatever had been set. There is still no I/O scheduler to
    /// act on the value, but a setting that reads back as set is what every
    /// caller checks; the rest is Linux's order: `ioprio_check_cap` first
    /// (`EINVAL`/`EPERM` before anybody is looked up), then the tasks
    /// (`ESRCH` when there is none), then `set_task_ioprio`'s owner test on
    /// each, stopping at the first refusal like the kernel's loop does.
    ///
    /// See [linux man ioprio_set(2)](https://www.man7.org/linux/man-pages/man2/ioprio_set.2.html).
    pub fn sys_ioprio_set(&self, which: usize, who: usize, ioprio: usize) -> SysResult {
        info!(
            "ioprio_set: which={}, who={}, ioprio={:#x}",
            which, who, ioprio
        );
        let caller = self.linux_process().credentials();
        let capable = self.linux_process().capable(CAP_SYS_NICE)
            || self.linux_process().capable(CAP_SYS_ADMIN);
        let ioprio = ioprio_set_verdict(ioprio, capable)?;
        let which = ioprio_which(which)?;
        for thread in self.priority_targets(which, who)? {
            let linux = thread.proc().try_linux().ok_or(LxError::ESRCH)?;
            if !LinuxProcess::may_set_ioprio_of(&caller, &linux.credentials()) {
                return Err(LxError::EPERM);
            }
            thread.lock_linux().ioprio = ioprio;
        }
        Ok(0)
    }

    /// get capabilities of a process
    /// (see [linux man capget(2)](https://www.man7.org/linux/man-pages/man2/capget.2.html)).
    ///
    /// Implements the header version-negotiation protocol from
    /// `linux/capability.h`: an unrecognised version gets the preferred one
    /// (V3) written back plus `EINVAL`, which is how libcap probes. Every
    /// process running as root reports the full capability set; anything else
    /// reports empty sets. Tools like `ping` and container runtimes check
    /// themselves with this before attempting privileged operations.
    pub fn sys_capget(
        &self,
        mut header: UserInOutPtr<CapUserHeader>,
        mut data: UserOutPtr<CapUserData>,
    ) -> SysResult {
        let hdr = header.read()?;
        info!("capget: version={:#x}, pid={}", hdr.version, hdr.pid);
        if cap_version_elems(hdr.version).is_none() {
            header.write(CapUserHeader {
                version: LINUX_CAPABILITY_VERSION_3,
                pid: hdr.pid,
            })?;
        }
        let elems = match capget_plan(hdr.version, hdr.pid, data.is_null())? {
            Some(elems) => elems,
            None => return Ok(0),
        };
        // `cap_get_target_pid`: the caller's own sets, or those of the task
        // `pid` names, which must exist (`ESRCH`). Built out of the same
        // predicate every gate asks, so what this publishes and what the
        // kernel honours cannot drift apart.
        let euid = if capget_names_the_caller(hdr.pid, self.zircon_process().id()) {
            self.linux_process().euid()
        } else {
            process_of_task(hdr.pid as KoID)
                .and_then(|p| p.try_linux().map(|lp| lp.euid()))
                .ok_or(LxError::ESRCH)?
        };
        let caps = linux_object::process::published_capabilities(euid);
        let mut out = [CapUserData::default(); 2];
        out[0].effective = caps as u32;
        out[0].permitted = caps as u32;
        out[1].effective = (caps >> 32) as u32;
        out[1].permitted = (caps >> 32) as u32;
        data.write_array(&out[..elems])?;
        Ok(0)
    }

    /// set capabilities of a process
    /// (see [linux man capset(2)](https://www.man7.org/linux/man-pages/man2/capset.2.html)).
    ///
    /// The version negotiation is [`sys_capget`](Self::sys_capget)'s; then
    /// Linux's own order (`SYSCALL_DEFINE2(capset)`, then `cap_capset`): the
    /// call may only affect the caller (`EPERM` for any other pid, negative
    /// ones included: there is no `EINVAL` for a bad pid here, unlike
    /// `capget`), and the new sets must fit inside the old ones
    /// ([`capset_sets_verdict`]). A set that passes is then accepted without
    /// being stored: every root process holds the full set here and there is
    /// no privilege machinery for a reduced set to constrain, so a daemon
    /// dropping capabilities proceeds as if it worked, which on this
    /// single-user kernel it effectively has. What is refused is what Linux
    /// refuses: an unprivileged process granting itself capabilities, or
    /// anyone making effective what is not permitted.
    pub fn sys_capset(
        &self,
        mut header: UserInOutPtr<CapUserHeader>,
        data: UserInPtr<CapUserData>,
    ) -> SysResult {
        let hdr = header.read()?;
        info!("capset: version={:#x}, pid={}", hdr.version, hdr.pid);
        let elems = match cap_version_elems(hdr.version) {
            Some(n) => n,
            None => {
                header.write(CapUserHeader {
                    version: LINUX_CAPABILITY_VERSION_3,
                    pid: hdr.pid,
                })?;
                return Err(LxError::EINVAL);
            }
        };
        capset_target(hdr.pid, self.zircon_process().id())?;
        let new = CapSets::from_user(&data.read_array(elems)?);
        // What this process holds now: the published set is both its
        // permitted and its effective set, nothing is inheritable, and the
        // bounding set is every capability (`PR_CAPBSET_READ` says so).
        let held = linux_object::process::published_capabilities(self.linux_process().euid());
        let old = CapSets {
            effective: held,
            permitted: held,
            inheritable: 0,
        };
        capset_sets_verdict(&old, CAP_FULL_SET, &new)?;
        Ok(0)
    }

    /// Read and/or clear kernel message ring buffer; set console_loglevel
    pub fn sys_syslog(&self, type_: i32, mut buf: UserOutPtr<u8>, len: i32) -> SysResult {
        info!("syslog: type={}, buf={:?}, len={}", type_, buf, len);
        // syslog(2) action codes
        const SYSLOG_ACTION_CLOSE: i32 = 0;
        const SYSLOG_ACTION_OPEN: i32 = 1;
        const SYSLOG_ACTION_READ: i32 = 2; // read & clear (we treat as READ_ALL)
        const SYSLOG_ACTION_READ_ALL: i32 = 3;
        const SYSLOG_ACTION_READ_CLEAR: i32 = 4;
        const SYSLOG_ACTION_CLEAR: i32 = 5;
        const SYSLOG_ACTION_CONSOLE_OFF: i32 = 6;
        const SYSLOG_ACTION_CONSOLE_ON: i32 = 7;
        const SYSLOG_ACTION_CONSOLE_LEVEL: i32 = 8;
        const SYSLOG_ACTION_SIZE_UNREAD: i32 = 9;
        const SYSLOG_ACTION_SIZE_BUFFER: i32 = 10;

        match type_ {
            SYSLOG_ACTION_CLOSE
            | SYSLOG_ACTION_OPEN
            | SYSLOG_ACTION_CLEAR
            | SYSLOG_ACTION_CONSOLE_OFF
            | SYSLOG_ACTION_CONSOLE_ON
            | SYSLOG_ACTION_CONSOLE_LEVEL => Ok(0),

            SYSLOG_ACTION_SIZE_BUFFER => Ok(kernel_hal::console::klog_buf_size()),

            SYSLOG_ACTION_SIZE_UNREAD => Ok(kernel_hal::console::klog_buf_size()),

            SYSLOG_ACTION_READ | SYSLOG_ACTION_READ_ALL | SYSLOG_ACTION_READ_CLEAR => {
                // A negative `len` would sign-extend to a huge `usize`, letting
                // the read write past the (smaller) user buffer.
                if len < 0 {
                    return Err(LxError::EINVAL);
                }
                let cap = (len as usize).min(kernel_hal::console::klog_buf_size().max(1));
                let mut tmp = vec![0u8; cap];
                let n = kernel_hal::console::klog_read(&mut tmp);
                if n > 0 {
                    buf.write_array(&tmp[..n])?;
                }
                Ok(n)
            }

            _ => Ok(0),
        }
    }

    /// provides a simple way of getting overall system statistics
    pub fn sys_sysinfo(&mut self, mut sys_info: UserOutPtr<SysInfo>) -> SysResult {
        // `uptime` was the headline: returning the zeroed default made
        // `uptime`/`top` always report "up 0 min". Fill the fields we can
        // source cheaply so userspace tools show real numbers.
        let (used, total) = kernel_hal::mem::memory_usage();
        let (procs, _running) = linux_object::loadavg::count_processes();
        let sysinfo = SysInfo {
            // Seconds since boot, from the monotonic timer (same source as
            // /proc/uptime).
            uptime: timer_now().as_secs(),
            // Approximate 1/5/15-minute run-queue load averages, in the
            // fixed point sysinfo expects (value << 16).
            loads: linux_object::loadavg::loadavg_sysinfo(),
            totalram: total as u64,
            freeram: total.saturating_sub(used) as u64,
            procs: procs as u16,
            mem_unit: 1,
            ..SysInfo::default()
        };
        sys_info.write(sysinfo)?;
        Ok(0)
    }

    /// `membarrier` issues memory barriers across the threads of the system
    /// (see [membarrier(2)](https://man7.org/linux/man-pages/man2/membarrier.2.html)).
    ///
    /// We advertise and accept the query, the global/private *expedited*
    /// barrier commands and their SYNC_CORE variants. Registration commands are
    /// accepted as no-ops (expedited membarrier is always permitted here).
    ///
    /// The barrier itself is a local sequentially-consistent fence. We do not
    /// broadcast a cross-CPU IPI: on this kernel the IPI receive path always
    /// flushes the whole TLB, so forcing that on every `membarrier` (a call the
    /// runtimes that use it may issue often) would be far too costly. This is a
    /// deliberate simplification — adequate for the userspaces that call it,
    /// short of the full system-wide ordering guarantee.
    pub fn sys_membarrier(&self, cmd: i32, flags: u32, _cpu_id: i32) -> SysResult {
        use core::sync::atomic::{fence, Ordering};

        const QUERY: i32 = 0;
        const GLOBAL: i32 = 1 << 0;
        const GLOBAL_EXPEDITED: i32 = 1 << 1;
        const REGISTER_GLOBAL_EXPEDITED: i32 = 1 << 2;
        const PRIVATE_EXPEDITED: i32 = 1 << 3;
        const REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
        const PRIVATE_EXPEDITED_SYNC_CORE: i32 = 1 << 5;
        const REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: i32 = 1 << 6;

        info!("membarrier: cmd={:#x}, flags={:#x}", cmd, flags);

        // The only defined flag (MEMBARRIER_CMD_FLAG_CPU) applies to the RSEQ
        // command, which we don't advertise; reject any flag.
        if flags != 0 {
            return Err(LxError::EINVAL);
        }

        let supported = GLOBAL
            | GLOBAL_EXPEDITED
            | REGISTER_GLOBAL_EXPEDITED
            | PRIVATE_EXPEDITED
            | REGISTER_PRIVATE_EXPEDITED
            | PRIVATE_EXPEDITED_SYNC_CORE
            | REGISTER_PRIVATE_EXPEDITED_SYNC_CORE;

        match cmd {
            QUERY => Ok(supported as usize),
            GLOBAL | GLOBAL_EXPEDITED | PRIVATE_EXPEDITED | PRIVATE_EXPEDITED_SYNC_CORE => {
                fence(Ordering::SeqCst);
                Ok(0)
            }
            REGISTER_GLOBAL_EXPEDITED
            | REGISTER_PRIVATE_EXPEDITED
            | REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => Ok(0),
            _ => Err(LxError::EINVAL),
        }
    }

    /// The `Futex` a process-shared futex word names, or `None` when this
    /// word is not shared and the per-process table is the right home for it.
    ///
    /// Keyed by the pair Linux spells "inode + offset" -- the backing
    /// `VmObject`'s koid and the byte offset of the word inside it -- so two
    /// processes that map the same object at different addresses reach one
    /// queue. The word itself is reached through the kernel's linear map of
    /// the physical frame, never through either process's virtual address: the
    /// `Futex` outlives any one address space, and the process doing the
    /// `FUTEX_WAKE` is generally not the one that created it.
    ///
    /// Every failure here is a `None` that falls back to the per-process
    /// table, which is exactly the behaviour this kernel had before.
    #[allow(unsafe_code)]
    fn shared_futex(&self, uaddr: usize) -> Option<Arc<zircon_object::signal::Futex>> {
        use core::sync::atomic::AtomicI32;
        use zircon_object::object::KernelObject;
        use zircon_object::vm::MMUFlags;

        if uaddr == 0 || !uaddr.is_multiple_of(core::mem::align_of::<AtomicI32>()) {
            return None;
        }
        let vmar = self.zircon_process().vmar();
        let mapping = vmar.find_mapping(uaddr)?;
        let (vmo, offset) = mapping.vmo_and_offset(uaddr)?;
        // A PRIVATE mapping keeps the per-process table even without the
        // private flag: nobody else can observe that word, and sharing a queue
        // across a copy-on-write split would be wrong.
        if !vmo.is_shared_object() {
            return None;
        }
        // Force the page resident before translating: a lazily mapped word has
        // no page-table entry yet, and `query_vaddr` would simply fail.
        let _ = vmar.handle_page_fault(uaddr, MMUFlags::READ);
        let (paddr, flags, _) = mapping.query_vaddr(uaddr).ok()?;
        // Empty flags mean "present in the tables with no permissions", which
        // is how a guard page looks; there is nothing readable there.
        if flags.is_empty() {
            return None;
        }
        let kvaddr = kernel_hal::mem::phys_to_virt(paddr);
        if kvaddr == 0 {
            return None;
        }
        // Safe: `kvaddr` is the kernel's own linear-map address of a resident,
        // 4-byte-aligned frame the table now holds an `Arc<VmObject>` on, so
        // it stays mapped and owned for as long as the `Futex` can be reached.
        let word: &'static AtomicI32 = unsafe { &*(kvaddr as *const AtomicI32) };
        Some(linux_object::sync::shared_futex::intern(
            (vmo.id(), offset),
            vmo,
            word,
        ))
    }

    /// provides a method for waiting until a certain condition becomes true.
    /// - `uaddr` - points to the futex word.
    /// - `op` -  the operation to perform on the futex
    /// - `val` -  a value whose meaning and purpose depends on op
    /// - `val2` - provides a timeout for the attempt or acts as val2 when op is REQUEUE
    /// - `uaddr2` - when op is REQUEUE, points to the target futex
    /// - `val3` - expected futex value for CMP_REQUEUE; bitset mask for *_BITSET
    pub async fn sys_futex(
        &self,
        uaddr: usize,
        op: u32,
        val: u32,
        val2: usize,
        uaddr2: usize,
        val3: u32,
    ) -> SysResult {
        const FUTEX_WAIT: u32 = 0;
        const FUTEX_WAKE: u32 = 1;
        const FUTEX_REQUEUE: u32 = 3;
        const FUTEX_CMP_REQUEUE: u32 = 4;
        const FUTEX_WAIT_BITSET: u32 = 9;
        const FUTEX_WAKE_BITSET: u32 = 10;
        const FUTEX_LOCK_PI: u32 = 6;
        const FUTEX_UNLOCK_PI: u32 = 7;
        const FUTEX_TRYLOCK_PI: u32 = 8;
        const FUTEX_WAIT_REQUEUE_PI: u32 = 11;
        const FUTEX_CMP_REQUEUE_PI: u32 = 12;
        const FUTEX_LOCK_PI2: u32 = 13;
        const FUTEX_PRIVATE_FLAG: u32 = 0x80;
        const FUTEX_CLOCK_REALTIME: u32 = 0x100;

        debug!(
            "Futex uaddr: {:#x}, op: {:x}, val: {}, val2(timeout_addr): {:x}",
            uaddr, op, val, val2,
        );
        // NOTE: do NOT parse `op` as bitflags — command values are an enum
        // (WAIT_BITSET=9 would alias WAKE=1 when bits are truncated).
        let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
        // The requeue-PI pair (glibc condvars over PI mutexes) is not
        // implemented; glibc falls back to plain requeue on ENOSYS. Answer it
        // HERE, before `get_futex`, so the reply never depends on the address.
        if matches!(cmd, FUTEX_WAIT_REQUEUE_PI | FUTEX_CMP_REQUEUE_PI) {
            return Err(LxError::ENOSYS);
        }
        // Validate the futex word EVERY call, not just when the `Futex` is
        // created. `get_futex` caches an `&AtomicI32` made from this address
        // and keeps it in the process for good, so a check at insert time says
        // nothing about whether the mapping is still there now — and every op
        // below dereferences it. An unmapped futex word used to take the
        // machine down rather than answer EFAULT:
        //
        //     [KERNEL PAGE FAULT] vaddr=0x7125048a65 rip=<sys_futex...>
        //         (unresolved by the user vmar)
        //
        // reproducible to the byte across boots, from PulseAudio.
        let word: UserInPtr<i32> = uaddr.into();
        word.check()?;
        // A futex without FUTEX_PRIVATE_FLAG may name a word two DIFFERENT
        // processes share, so it cannot be served from the per-process table.
        // See `linux_object::sync::shared_futex` for why this is what every GL
        // application under Xwayland hangs on when it is missing.
        let futex = match (op & FUTEX_PRIVATE_FLAG == 0)
            .then(|| self.shared_futex(uaddr))
            .flatten()
        {
            Some(futex) => futex,
            None => self
                .linux_process()
                .get_futex(uaddr)
                .ok_or(LxError::EINVAL)?,
        };
        match cmd {
            // ── Priority-inheritance lock ops: LOCK_PI / LOCK_PI2 / TRYLOCK_PI ──
            //
            // These MUST work, not merely fail politely. musl's
            // pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT) probes the
            // kernel once (src/thread/pthread_mutexattr_setprotocol.c):
            //
            //     volatile int lk = 0;
            //     r = -__syscall(SYS_futex, &lk, FUTEX_LOCK_PI, 0, 0);
            //     a_store(&check_pi_result, r);
            //     if (r) return r;            <- the kernel errno, VERBATIM
            //
            // There is no translation layer: whatever errno the kernel answers
            // is what the caller sees. PulseAudio's pa_mutex_new() then asserts
            // `r == 0 || r == ENOTSUP` (pulsecore/mutex-posix.c:57) and aborts
            // the whole process otherwise -- which is how supertux2 (OpenAL ->
            // libpulse) died on real hardware with both ENOSYS (#1046) and,
            // had it been reached, any other non-95 errno. Rather than guess
            // which errno each libc/library pair tolerates, implement the ops:
            // an uncontended FUTEX_LOCK_PI on a zero word succeeds (r == 0),
            // musl then marks the mutex PI (`attr |= 8`) and routes every
            // contended lock/unlock of it through LOCK_PI/UNLOCK_PI below.
            //
            // Lock-word protocol (Linux ABI, shared with musl/glibc):
            //   bits 0..30  FUTEX_TID_MASK    owner TID, 0 = free
            //   bit  30     FUTEX_OWNER_DIED  robust-mutex owner death marker
            //   bit  31     FUTEX_WAITERS     someone is (or was) blocked in
            //                                 the kernel; unlock must go there
            // Acquire: CAS `owner==0` -> `tid | preserved flag bits`. Contended:
            // set FUTEX_WAITERS, sleep on the word (the enqueue re-checks the
            // value under the queue lock, so an unlock in between turns into
            // EAGAIN and a retry, never a lost wakeup), loop. No ownership
            // hand-off (Linux gives the word straight to the top waiter; here
            // the unlocker leaves `FUTEX_WAITERS` with owner 0 and wakes one,
            // and whoever gets there first takes it -- musl's fast path refuses
            // a non-zero word for PI mutexes, so it always comes back here).
            // Priority boosting itself is not implemented: this kernel has no
            // thread priorities, so there is nothing to inherit, and the
            // lock/unlock protocol above is the whole user-visible contract.
            FUTEX_LOCK_PI | FUTEX_LOCK_PI2 | FUTEX_TRYLOCK_PI => {
                const FUTEX_WAITERS: i32 = 0x8000_0000_u32 as i32;
                const FUTEX_OWNER_DIED: i32 = 0x4000_0000;
                const FUTEX_TID_MASK: i32 = 0x3fff_ffff;
                let tid = (self.thread.id() as i32) & FUTEX_TID_MASK;
                // The timeout is ABSOLUTE: CLOCK_REALTIME for LOCK_PI
                // (always), and for LOCK_PI2 whichever clock
                // FUTEX_CLOCK_REALTIME selects (monotonic otherwise).
                // TRYLOCK_PI never sleeps and ignores it.
                let deadline = if cmd == FUTEX_TRYLOCK_PI {
                    NO_FUTEX_DEADLINE
                } else {
                    let timeout_addr: UserInPtr<TimeSpec> = val2.into();
                    // Validated: a `timespec` out of range is EINVAL.
                    let timeout = timeout_addr
                        .read_if_not_null()?
                        .map(|timeout| timeout.try_into_duration())
                        .transpose()?;
                    let now = if cmd == FUTEX_LOCK_PI || op & FUTEX_CLOCK_REALTIME != 0 {
                        Duration::from(TimeSpec::now())
                    } else {
                        Duration::from(TimeSpec::now_monotonic())
                    };
                    futex_deadline(timer_now(), timeout, Some(now))
                };
                loop {
                    let cur = futex.load();
                    let owner = cur & FUTEX_TID_MASK;
                    if owner == 0 {
                        // Free (possibly with WAITERS / OWNER_DIED still set):
                        // take it, keeping those flag bits so a later unlock
                        // still visits the kernel and a robust owner-death
                        // marker survives to the new owner (EOWNERDEAD).
                        let new = tid | (cur & (FUTEX_WAITERS | FUTEX_OWNER_DIED));
                        if futex.compare_exchange(cur, new).is_ok() {
                            return Ok(0);
                        }
                        continue;
                    }
                    if owner == tid {
                        return Err(LxError::EDEADLK);
                    }
                    if cmd == FUTEX_TRYLOCK_PI {
                        return Err(LxError::EAGAIN);
                    }
                    // Contended: publish FUTEX_WAITERS so the owner's unlock
                    // comes through FUTEX_UNLOCK_PI, then sleep on the word.
                    let contended = cur | FUTEX_WAITERS;
                    if cur != contended && futex.compare_exchange(cur, contended).is_err() {
                        continue;
                    }
                    let future = futex.wait(contended);
                    // ALWAYS through `blocking_run`, even with no timeout:
                    // see `futex_deadline`. Awaiting the raw future here left
                    // a contended PI mutex unkillable.
                    let res: ZxResult = self
                        .thread
                        .blocking_run(future, ThreadState::BlockedFutex, deadline, None)
                        .await;
                    match res {
                        // Woken by UNLOCK_PI, or the word changed under us
                        // before we were queued: re-read and retry.
                        Ok(_) | Err(ZxError::BAD_STATE) => continue,
                        Err(ZxError::TIMED_OUT) => return Err(LxError::ETIMEDOUT),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            FUTEX_UNLOCK_PI => {
                const FUTEX_WAITERS: i32 = 0x8000_0000_u32 as i32;
                const FUTEX_TID_MASK: i32 = 0x3fff_ffff;
                let tid = (self.thread.id() as i32) & FUTEX_TID_MASK;
                if futex.load() & FUTEX_TID_MASK != tid {
                    // Only the owner may unlock (Linux: EPERM).
                    return Err(LxError::EPERM);
                }
                // Release under the queue lock (see `store_by_waiters`): with
                // a waiter queued leave `FUTEX_WAITERS` + owner 0 and wake one
                // of them; with nobody queued clear the word entirely so the
                // next lock is a pure-userspace CAS again.
                if futex.store_by_waiters(FUTEX_WAITERS, 0) {
                    futex.wake(1);
                }
                Ok(0)
            }
            FUTEX_WAIT | FUTEX_WAIT_BITSET => {
                // Fast-path EAGAIN: the userspace cmpxchg often loses by the
                // time we get here (the canonical contended-mutex case in
                // musl/glibc). Short-circuit before allocating the Waiter
                // future and engaging the blocking machinery — the slow path
                // re-checks under the queue lock so this is purely an
                // optimization.
                if !futex.value_eq(val as i32) {
                    return Err(LxError::EAGAIN);
                }
                // FUTEX_WAIT_BITSET with a mask is approximated as match-any;
                // both musl and glibc only use FUTEX_BITSET_MATCH_ANY here.
                let future = futex.wait(val as _);
                let timeout_addr: UserInPtr<TimeSpec> = val2.into();
                // Validated: a `timespec` out of range is EINVAL, and it is
                // rejected whether or not we end up waiting.
                let timeout = timeout_addr
                    .read_if_not_null()?
                    .map(|timeout| timeout.try_into_duration())
                    .transpose()?;
                // FUTEX_WAIT takes a RELATIVE timeout; FUTEX_WAIT_BITSET takes
                // an ABSOLUTE one on the clock FUTEX_CLOCK_REALTIME selects.
                let on_clock = (cmd == FUTEX_WAIT_BITSET).then(|| {
                    if op & FUTEX_CLOCK_REALTIME != 0 {
                        Duration::from(TimeSpec::now())
                    } else {
                        Duration::from(TimeSpec::now_monotonic())
                    }
                });
                // ALWAYS through `blocking_run`, even with no timeout: see
                // `futex_deadline`. A null `timespec` is the ordinary case --
                // every contended `pthread_mutex_lock` -- and awaiting the raw
                // future there made the wait uninterruptible by anything.
                let res: ZxResult = self
                    .thread
                    .blocking_run(
                        future,
                        ThreadState::BlockedFutex,
                        futex_deadline(timer_now(), timeout, on_clock),
                        None,
                    )
                    .await;
                match res {
                    Ok(_) => Ok(0),
                    Err(e) => Err(e.into()),
                }
            }
            FUTEX_WAKE | FUTEX_WAKE_BITSET => Ok(futex.wake(val as _)),
            FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
                // The TARGET word is resolved exactly like the source one
                // above. A requeue that carries no FUTEX_PRIVATE_FLAG names a
                // word another process may be waiting on, and taking its
                // queue from the per-process table would move those waiters
                // onto a queue nobody else can ever reach -- they would sleep
                // for good. Linux keys both sides through `get_futex_key`.
                let requeue_futex = match (op & FUTEX_PRIVATE_FLAG == 0)
                    .then(|| self.shared_futex(uaddr2))
                    .flatten()
                {
                    Some(futex) => futex,
                    None => self
                        .linux_process()
                        .get_futex(uaddr2)
                        .ok_or(LxError::EINVAL)?,
                };
                // FUTEX_CMP_REQUEUE checks *uaddr against val3 first; a
                // mismatch comes back as `BAD_STATE`, which maps to EAGAIN,
                // what Linux answers. The result is how many were woken plus
                // how many moved -- FUTEX_WAKE above already returns its own
                // count, and answering 0 here told every caller that a
                // broadcast had reached nobody.
                let res = futex.requeue(
                    val3 as i32,
                    val as _,
                    val2,
                    &requeue_futex,
                    None,
                    cmd == FUTEX_CMP_REQUEUE,
                );
                match res {
                    Ok(count) => Ok(count),
                    Err(e) => Err(e.into()),
                }
            }
            _ => {
                warn!("unsupported futex operation: {:#x} (cmd {})", op, cmd);
                Err(LxError::ENOSYS)
            }
        }
    }

    /// Combines and extends the functionality of setrlimit() and getrlimit()
    ///
    /// Four of the sixteen resources used to be answered -- three of them
    /// with a constant, whatever the caller had asked for -- and the other
    /// twelve with `ENOSYS`, which is not an answer a program expects:
    /// `ulimit -a` walks all sixteen, and a daemon that turns its core dumps
    /// off with `setrlimit(RLIMIT_CORE, {0, 0})` was told the call does not
    /// exist.
    ///
    /// And `pid` was read only to be printed. Every call landed on the
    /// CALLER, so `prlimit(other, RLIMIT_NOFILE, &new)` moved the caller's
    /// own descriptor budget and reported success: a program asking about
    /// somebody else got its own answer, and a program *setting* somebody
    /// else's limit changed the wrong process.
    pub fn sys_prlimit64(
        &mut self,
        pid: usize,
        resource: usize,
        new_limit: UserInPtr<RLimit>,
        mut old_limit: UserOutPtr<RLimit>,
    ) -> SysResult {
        info!(
            "prlimit64: pid: {}, resource: {}, new_limit: {:x?}, old_limit: {:x?}",
            pid, resource, new_limit, old_limit
        );
        let new_limit = new_limit.read_if_not_null()?;
        // `capable(CAP_SYS_RESOURCE)` is asked of whoever is CALLING, both
        // here and inside `prlimit_target`; the target's own privileges never
        // enter into it.
        let may_raise_hard = self.linux_process().capable(CAP_SYS_RESOURCE);
        // `tsk = pid ? find_task_by_vpid(pid) : current;`
        let target = self.prlimit_target(pid)?;
        let old = target.try_linux().ok_or(LxError::ESRCH)?.rlimit(
            resource,
            new_limit,
            may_raise_hard,
        )?;
        old_limit.write_if_not_null(old)?;
        Ok(0)
    }

    /// The process `prlimit64(pid, ...)` is about: the caller for `pid == 0`
    /// or for its own pid, otherwise whichever process that is -- `ESRCH`
    /// when there is none, `EPERM` when it is not the caller's to touch.
    fn prlimit_target(&self, pid: usize) -> linux_object::error::LxResult<Arc<Process>> {
        // A `pid_t`: the low 32 bits of the register, and a negative one is
        // a task `find_task_by_vpid` does not find.
        let pid = crate::intarg::task_pid(pid)?;
        let me = self.zircon_process();
        if pid == 0 || pid as u64 == me.id() {
            return Ok(me.clone());
        }
        let target = linux_object::process::find_process(pid as u64).ok_or(LxError::ESRCH)?;
        let allowed = {
            let other = target.try_linux().ok_or(LxError::ESRCH)?;
            LinuxProcess::may_touch_limits_of(
                &self.linux_process().credentials(),
                &other.credentials(),
            )
        };
        if allowed {
            Ok(target)
        } else {
            Err(LxError::EPERM)
        }
    }

    /// `getrlimit` gets resource limits.
    pub fn sys_getrlimit(&mut self, resource: usize, rlim: UserOutPtr<RLimit>) -> SysResult {
        info!("getrlimit: resource={}, rlim={:?}", resource, rlim);
        self.sys_prlimit64(0, resource, 0.into(), rlim)
    }

    /// `setrlimit` sets resource limits.
    pub fn sys_setrlimit(&mut self, resource: usize, rlim: UserInPtr<RLimit>) -> SysResult {
        info!("setrlimit: resource={}, rlim={:?}", resource, rlim);
        self.sys_prlimit64(0, resource, rlim, 0.into())
    }

    #[allow(unsafe_code)]
    /// fills the buffer pointed to by `buf` with up to `buflen` random bytes.
    /// - `buf` - buffer that needed to fill
    /// - `buflen` - length of buffer
    /// - `flag` - a bit mask that can contain zero or more of the following values ORed together:
    ///   - GRND_RANDOM
    ///   - GRND_NONBLOCK
    ///
    /// - returns the number of bytes that were copied to the buffer buf.
    ///
    /// reboot() reboots the system, or enables/disables the reboot keystroke.
    pub fn sys_reboot(
        &mut self,
        magic1: u32,
        magic2: u32,
        cmd: u32,
        _arg: UserInPtr<u8>,
    ) -> SysResult {
        warn!(
            "reboot: magic1={:#x}, magic2={:#x}, cmd={:#x}",
            magic1, magic2, cmd
        );
        // `kernel/reboot.c`, before the magic numbers are even looked at:
        //
        // ```c
        // /* We only trust the superuser with rebooting the system. */
        // if (!ns_capable(pid_ns->user_ns, CAP_SYS_BOOT))
        //         return -EPERM;
        // ```
        //
        // The magic numbers are there to catch a wild call, not to keep
        // anyone out: they are public constants. Without this line any
        // process at all could power the machine off.
        if !self.linux_process().capable(CAP_SYS_BOOT) {
            return Err(LxError::EPERM);
        }
        if magic1 != 0xfee1dead
            || (magic2 != 0x28121969
                && magic2 != 0x05121996
                && magic2 != 0x16041998
                && magic2 != 0x20112000)
        {
            warn!("reboot: invalid magic!");
            return Err(LxError::EINVAL);
        }
        match cmd {
            0x4321fedc => {
                // LINUX_REBOOT_CMD_POWER_OFF
                warn!("reboot: poweroff...");
                kernel_hal::cpu::power_off();
            }
            0x89abcdef => {
                // LINUX_REBOOT_CMD_CAD_ON
                Ok(0)
            }
            0x00000000 => {
                // LINUX_REBOOT_CMD_CAD_OFF
                Ok(0)
            }
            0xcdef0123 => {
                // LINUX_REBOOT_CMD_HALT
                warn!("reboot: halt...");
                kernel_hal::cpu::power_off();
            }
            0x456789ab => {
                // LINUX_REBOOT_CMD_SW_SUSPEND
                warn!("reboot: sw_suspend unimplemented");
                Err(LxError::EINVAL)
            }
            0x01234567 | 0xa1b2c3d4 => {
                // LINUX_REBOOT_CMD_RESTART / RESTART2
                warn!("reboot: restarting...");
                kernel_hal::cpu::reset();
            }
            _ => {
                warn!("reboot: unknown command {:#x}", cmd);
                Err(LxError::EINVAL)
            }
        }
    }

    #[allow(unsafe_code)]
    /// fills the buffer pointed to by `buf` with up to `buflen` random bytes.
    /// - `buf` - buffer that needed to fill
    /// - `buflen` - length of buffer
    /// - `flag` - a bit mask of `GRND_NONBLOCK`, `GRND_RANDOM` and
    ///   `GRND_INSECURE`; anything else is `EINVAL` (see [`getrandom_args`]).
    /// - returns the number of bytes that were copied to the buffer buf.
    ///
    /// The entropy source here is always ready and never blocks, so none of
    /// the three flags changes what comes back -- but which flags *exist* is
    /// still the caller's question to ask, and a program that probes for
    /// `GRND_INSECURE` needs to be told whether this kernel knows the bit.
    pub fn sys_getrandom(&mut self, buf: UserOutPtr<u8>, len: usize, flag: u32) -> SysResult {
        info!("getrandom: buf: {:?}, len: {:?}, flag {:?}", buf, len, flag);
        let len = getrandom_args(len, flag)?;
        let mut written = 0;
        let mut chunk = [0u8; 1024];
        while written < len {
            let left = len - written;
            let current_len = left.min(chunk.len());
            kernel_hal::rand::fill_random(&mut chunk[..current_len]);
            buf.add(written).write_array(&chunk[..current_len])?;
            written += current_len;
        }
        Ok(len)
    }
}

/// The deadline a `futex` wait gets when it was given no timeout: far enough
/// out never to fire, and the same "no deadline" this kernel already uses for
/// a thread blocked on an exception (`Thread::handle_exception`).
pub const NO_FUTEX_DEADLINE: Duration = Duration::from_nanos(u64::MAX);

/// How long a `futex` wait blocks, as a deadline on the kernel's monotonic
/// clock.
///
/// `timeout` is `None` when the caller passed a null `timespec`, which
/// futex(2) documents as "block indefinitely". **Indefinitely still means
/// interruptibly**, so this answers with a deadline that never fires rather
/// than an `Option` a caller could serve by awaiting the raw future -- and
/// that distinction is the whole reason the function exists. A futex wait
/// that skips `Thread::blocking_run` registers no killer and never enters
/// `BlockedFutex`, and `FutexFuture` has no timer, no signal check and no
/// wakeup of its own, so once a thread parks there NOTHING in the kernel can
/// end the wait but a matching `FUTEX_WAKE`: not a signal, not `kill -9`, not
/// the process exiting, and `/proc` does not even show it as blocked. The
/// untimed wait is the ordinary one -- it is what a contended
/// `pthread_mutex_lock` and a `pthread_cond_wait` without a deadline come
/// down to -- so that is a whole process wedged past rescue.
///
/// `on_clock` says how `timeout` is expressed:
/// * `None` -- RELATIVE to now, which is what plain `FUTEX_WAIT` takes.
/// * `Some(now)` -- ABSOLUTE, a point on the clock the operation selected,
///   with `now` read from that same clock. `FUTEX_WAIT_BITSET` and the PI
///   locks take these: on `CLOCK_REALTIME` when `FUTEX_CLOCK_REALTIME` is set
///   (and always, for `FUTEX_LOCK_PI`), on `CLOCK_MONOTONIC` otherwise.
pub fn futex_deadline(
    now_monotonic: Duration,
    timeout: Option<Duration>,
    on_clock: Option<Duration>,
) -> Duration {
    let timeout = match timeout {
        Some(timeout) => timeout,
        None => return NO_FUTEX_DEADLINE,
    };
    match on_clock {
        // An absolute deadline is a point on the CALLER's clock while the
        // kernel sleeps on the monotonic one, so carry over only what is left
        // of it. One already past saturates to nothing left, which is the
        // immediate ETIMEDOUT Linux gives.
        Some(clock_now) => now_monotonic.saturating_add(timeout.saturating_sub(clock_now)),
        // Saturating rather than `+`: adding a `Duration` panics on overflow,
        // and the timeout came from userspace.
        None => now_monotonic.saturating_add(timeout),
    }
}

/// `struct __user_cap_header_struct` from `linux/capability.h`, the in/out
/// header both `capget(2)` and `capset(2)` start with.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CapUserHeader {
    /// One of the `_LINUX_CAPABILITY_VERSION_{1,2,3}` magics; rewritten to the
    /// preferred version when the caller sends an unknown one (the libcap
    /// probing protocol).
    pub version: u32,
    /// Target process; 0 means the calling process.
    pub pid: i32,
}

/// `struct __user_cap_data_struct`: one 32-bit slice of the three capability
/// sets. Version 1 uses one element, versions 2/3 use two (64 bits).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct CapUserData {
    /// Capabilities the process may currently exercise.
    pub effective: u32,
    /// Capabilities the process is permitted to make effective.
    pub permitted: u32,
    /// Capabilities preserved across `execve`.
    pub inheritable: u32,
}

const LINUX_CAPABILITY_VERSION_1: u32 = 0x1998_0330;
const LINUX_CAPABILITY_VERSION_2: u32 = 0x2007_1026;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Number of `CapUserData` elements a capability ABI version transfers, or
/// `None` for an unrecognised version (which triggers the write-back probe
/// reply). Pure, so the negotiation table is unit-testable.
fn cap_version_elems(version: u32) -> Option<usize> {
    match version {
        LINUX_CAPABILITY_VERSION_1 => Some(1),
        LINUX_CAPABILITY_VERSION_2 | LINUX_CAPABILITY_VERSION_3 => Some(2),
        _ => None,
    }
}

/// Every capability there is: the bounding set of a process nothing has
/// ever narrowed (`CAP_FULL_SET`).
const CAP_FULL_SET: u64 = (1u64 << (linux_object::process::CAP_LAST_CAP + 1)) - 1;

/// What `capget(2)` does before it looks at any process, from
/// `SYSCALL_DEFINE2(capget)`:
///
/// ```c
/// ret = cap_validate_magic(header, &tocopy);
/// if ((dataptr == NULL) || (ret != 0))
///         return ((dataptr == NULL) && (ret == -EINVAL)) ? 0 : ret;
/// if (get_user(pid, &header->pid)) return -EFAULT;
/// if (pid < 0) return -EINVAL;
/// ```
///
/// `Ok(None)` is the probe: a null `data` is answered 0 whatever the header
/// says, after the version was written back if it was unknown. That is how
/// libcap learns the kernel's version (`capget(&hdr, NULL)` with version 0).
/// It used to be `EINVAL` here whenever the version was unknown, null data or
/// not, and `EINVAL` for a negative pid ahead of the null test as well.
/// `Ok(Some(n))` is a real read of `n` elements.
fn capget_plan(version: u32, pid: i32, data_is_null: bool) -> LxResult<Option<usize>> {
    let elems = cap_version_elems(version);
    if data_is_null {
        return Ok(None);
    }
    let elems = elems.ok_or(LxError::EINVAL)?;
    if pid < 0 {
        return Err(LxError::EINVAL);
    }
    Ok(Some(elems))
}

/// Whether a `capget` `pid` names the caller: 0, or its own process id
/// (`cap_get_target_pid`: `if (pid && (pid != task_pid_vnr(current)))`
/// looks the task up, else `current`).
fn capget_names_the_caller(pid: i32, self_pid: KoID) -> bool {
    pid == 0 || pid as KoID == self_pid
}

/// The process the task `pid` belongs to: the process of that id, or the
/// one owning a thread of that id, since Linux's `find_task_by_vpid` takes a
/// TID. `None` when there is no such task.
fn process_of_task(pid: KoID) -> Option<Arc<zircon_object::task::Process>> {
    linux_object::process::find_process(pid).or_else(|| {
        linux_object::process::all_live_processes()
            .into_iter()
            .find(|proc| proc.get_child(pid).is_ok())
    })
}

/// `capset(2)`'s only target is the caller: `SYSCALL_DEFINE2(capset)`, `/*
/// may only affect current now */ if (pid != 0 && pid != task_pid_vnr(current))
/// return -EPERM;`. There is no `EINVAL` for a negative pid, unlike `capget`:
/// -1 is just another process that is not the caller.
///
/// It used to answer 0 for any other pid, which told `setcap`-style tooling
/// and anything that drops another process's capabilities that it had.
fn capset_target(pid: i32, self_pid: KoID) -> LxResult {
    if pid == 0 || pid as KoID == self_pid {
        Ok(())
    } else {
        Err(LxError::EPERM)
    }
}

/// The three capability sets as 64-bit masks, assembled from the one or two
/// `CapUserData` elements the negotiated version transfers. Version 1
/// sends one element, and the upper 32 bits are then zero (the kernel:
/// `if (i < tocopy) ... else effective.val |= 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapSets {
    effective: u64,
    permitted: u64,
    inheritable: u64,
}

impl CapSets {
    fn from_user(data: &[CapUserData]) -> Self {
        let mut sets = CapSets {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        };
        for (i, elem) in data.iter().take(2).enumerate() {
            let shift = 32 * i as u32;
            sets.effective |= (elem.effective as u64) << shift;
            sets.permitted |= (elem.permitted as u64) << shift;
            sets.inheritable |= (elem.inheritable as u64) << shift;
        }
        sets
    }
}

/// `cap_capset` (`security/commoncap.c`): may a process holding `old`, with
/// bounding set `bset`, make `new` its sets? Four subset tests, each
/// `EPERM`, in the kernel's order:
///
/// ```c
/// if (!cap_issubset(*inheritable, cap_combine(old->cap_inheritable, old->cap_permitted)))
///         return -EPERM;   /* incapable of using this inheritable set */
/// if (!cap_issubset(*inheritable, cap_combine(old->cap_inheritable, old->cap_bset)))
///         return -EPERM;   /* no new pI capabilities outside bounding set */
/// if (!cap_issubset(*permitted, old->cap_permitted))
///         return -EPERM;   /* verify restrictions on target's new Permitted set */
/// if (!cap_issubset(*effective, *permitted))
///         return -EPERM;   /* verify the _new_Effective_ is a subset of the _new_Permitted_ */
/// ```
///
/// A set may only ever shrink: nothing checked this before, so a process
/// running as any user could `capset` itself every capability and be told
/// it had them (nothing stored them, but the answer was the lie).
fn capset_sets_verdict(old: &CapSets, bset: u64, new: &CapSets) -> LxResult {
    let subset = |part: u64, whole: u64| part & !whole == 0;
    if !subset(new.inheritable, old.inheritable | old.permitted) {
        return Err(LxError::EPERM);
    }
    if !subset(new.inheritable, old.inheritable | bset) {
        return Err(LxError::EPERM);
    }
    if !subset(new.permitted, old.permitted) {
        return Err(LxError::EPERM);
    }
    if !subset(new.effective, new.permitted) {
        return Err(LxError::EPERM);
    }
    Ok(())
}

/// `IOPRIO_CLASS_SHIFT`: the class sits above a 13-bit level.
const IOPRIO_CLASS_SHIFT: u16 = 13;
/// `IOPRIO_CLASS_NONE`: never set; reads as the nice-derived best-effort.
const IOPRIO_CLASS_NONE: u16 = 0;
/// `IOPRIO_CLASS_RT`: real-time, `CAP_SYS_NICE`/`CAP_SYS_ADMIN` only.
const IOPRIO_CLASS_RT: u16 = 1;
/// `IOPRIO_CLASS_BE`: best-effort, the default class.
const IOPRIO_CLASS_BE: u16 = 2;
/// `IOPRIO_CLASS_IDLE`: served when nobody else wants the disk.
const IOPRIO_CLASS_IDLE: u16 = 3;
/// `IOPRIO_NR_LEVELS`: RT and BE have levels `0..8`.
const IOPRIO_NR_LEVELS: u16 = 8;
/// `IOPRIO_BE_NORM`: the best-effort level of a nice-0 task.
const IOPRIO_BE_NORM: u16 = 4;

/// `IOPRIO_PRIO_VALUE(class, level)`.
const fn ioprio_value(class: u16, level: u16) -> u16 {
    (class << IOPRIO_CLASS_SHIFT) | level
}

/// `IOPRIO_PRIO_CLASS` and `IOPRIO_PRIO_LEVEL` (`IOPRIO_PRIO_DATA`).
fn ioprio_class_and_level(ioprio: u16) -> (u16, u16) {
    (
        ioprio >> IOPRIO_CLASS_SHIFT,
        ioprio & ((1 << IOPRIO_CLASS_SHIFT) - 1),
    )
}

/// `IOPRIO_WHO_PROCESS`/`_PGRP`/`_USER` are `1`/`2`/`3`; `setpriority`'s
/// `PRIO_PROCESS`/`_PGRP`/`_USER` are `0`/`1`/`2` and mean the same three
/// things (`who == 0` is "mine" in every flavour, in both calls). Anything
/// else is `EINVAL`.
fn ioprio_which(which: usize) -> LxResult<usize> {
    match which {
        1..=3 => Ok(which - 1),
        _ => Err(LxError::EINVAL),
    }
}

/// `ioprio_check_cap()`: what an `ioprio_set` word may say, and who may
/// say it.
///
/// ```c
/// switch (class) {
/// case IOPRIO_CLASS_RT:
///         if (!capable(CAP_SYS_NICE) && !capable(CAP_SYS_ADMIN))
///                 return -EPERM;
///         fallthrough;
/// case IOPRIO_CLASS_BE:
///         if (level >= IOPRIO_NR_LEVELS)
///                 return -EINVAL;
///         break;
/// case IOPRIO_CLASS_IDLE:
///         break;
/// case IOPRIO_CLASS_NONE:
///         if (level)
///                 return -EINVAL;
///         break;
/// default:
///         return -EINVAL;
/// }
/// ```
///
/// `capable` is the caller's `CAP_SYS_NICE || CAP_SYS_ADMIN`. The word
/// comes back as the `u16` a thread stores.
pub(crate) fn ioprio_set_verdict(ioprio: usize, capable: bool) -> LxResult<u16> {
    let word = u16::try_from(ioprio).map_err(|_| LxError::EINVAL)?;
    let (class, level) = ioprio_class_and_level(word);
    match class {
        IOPRIO_CLASS_RT | IOPRIO_CLASS_BE => {
            if class == IOPRIO_CLASS_RT && !capable {
                return Err(LxError::EPERM);
            }
            if level >= IOPRIO_NR_LEVELS {
                return Err(LxError::EINVAL);
            }
        }
        IOPRIO_CLASS_IDLE => {}
        IOPRIO_CLASS_NONE => {
            if level != 0 {
                return Err(LxError::EINVAL);
            }
        }
        _ => return Err(LxError::EINVAL),
    }
    Ok(word)
}

/// `task_nice_ioprio()`: the best-effort level a task that never set an I/O
/// priority is served at, `(nice + 20) / 5`, so nice 0 is level 4 and the
/// two ends of the nice range are levels 0 and 7.
pub(crate) fn nice_ioprio(nice: i8) -> u16 {
    let level = (nice as i16 + 20) / 5;
    ioprio_value(IOPRIO_CLASS_BE, level as u16)
}

/// `get_task_ioprio()`: what a task reports -- the word it set, or, when
/// its class is `IOPRIO_CLASS_NONE` (never set), [`nice_ioprio`].
pub(crate) fn effective_ioprio(stored: u16, nice: i8) -> u16 {
    if ioprio_class_and_level(stored).0 == IOPRIO_CLASS_NONE {
        nice_ioprio(nice)
    } else {
        stored
    }
}

/// `ioprio_best()`: of two effective priorities, the one served first. The
/// word orders the right way round -- RT (1) below BE (2) below IDLE (3),
/// and level 0 before level 7 within a class -- so it is the minimum; a
/// class of NONE is read as the best-effort default first.
pub(crate) fn ioprio_best(a: u16, b: u16) -> u16 {
    let default = ioprio_value(IOPRIO_CLASS_BE, IOPRIO_BE_NORM);
    let valid = |p: u16| {
        if ioprio_class_and_level(p).0 == IOPRIO_CLASS_NONE {
            default
        } else {
            p
        }
    };
    valid(a).min(valid(b))
}

#[cfg(test)]
mod ioprio_tests {
    use super::*;

    const RT: u16 = IOPRIO_CLASS_RT;
    const BE: u16 = IOPRIO_CLASS_BE;
    const IDLE: u16 = IOPRIO_CLASS_IDLE;

    fn word(class: u16, level: u16) -> usize {
        ioprio_value(class, level) as usize
    }

    #[test]
    fn real_time_is_for_cap_sys_nice_only() {
        assert_eq!(ioprio_set_verdict(word(RT, 4), false), Err(LxError::EPERM));
        assert_eq!(
            ioprio_set_verdict(word(RT, 4), true),
            Ok(ioprio_value(RT, 4))
        );
        // The capability opens the class, not the level range.
        assert_eq!(ioprio_set_verdict(word(RT, 8), true), Err(LxError::EINVAL));
    }

    #[test]
    fn best_effort_has_eight_levels() {
        for level in 0..IOPRIO_NR_LEVELS {
            assert_eq!(
                ioprio_set_verdict(word(BE, level), false),
                Ok(ioprio_value(BE, level))
            );
        }
        assert_eq!(ioprio_set_verdict(word(BE, 8), false), Err(LxError::EINVAL));
        assert_eq!(
            ioprio_set_verdict(word(BE, 0x1fff), true),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn idle_is_for_anyone_and_none_takes_no_level() {
        assert_eq!(
            ioprio_set_verdict(word(IDLE, 0), false),
            Ok(ioprio_value(IDLE, 0))
        );
        assert_eq!(ioprio_set_verdict(word(IOPRIO_CLASS_NONE, 0), false), Ok(0));
        assert_eq!(
            ioprio_set_verdict(word(IOPRIO_CLASS_NONE, 1), false),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn an_unknown_class_or_a_word_too_wide_is_einval() {
        assert_eq!(ioprio_set_verdict(word(4, 0), true), Err(LxError::EINVAL));
        assert_eq!(ioprio_set_verdict(word(7, 3), true), Err(LxError::EINVAL));
        assert_eq!(ioprio_set_verdict(1 << 16, true), Err(LxError::EINVAL));
        assert_eq!(ioprio_set_verdict(usize::MAX, true), Err(LxError::EINVAL));
    }

    #[test]
    fn a_task_that_never_set_one_is_best_effort_at_its_nice_level() {
        assert_eq!(nice_ioprio(0), ioprio_value(BE, 4));
        assert_eq!(nice_ioprio(-20), ioprio_value(BE, 0));
        assert_eq!(nice_ioprio(19), ioprio_value(BE, 7));
        assert_eq!(nice_ioprio(10), ioprio_value(BE, 6));
        assert_eq!(nice_ioprio(-1), ioprio_value(BE, 3));
        assert_eq!(effective_ioprio(0, 10), ioprio_value(BE, 6));
    }

    #[test]
    fn what_was_set_is_what_is_read_back_whatever_the_nice() {
        assert_eq!(
            effective_ioprio(ioprio_value(IDLE, 0), -20),
            ioprio_value(IDLE, 0)
        );
        assert_eq!(
            effective_ioprio(ioprio_value(RT, 7), 19),
            ioprio_value(RT, 7)
        );
        assert_eq!(
            effective_ioprio(ioprio_value(BE, 0), 19),
            ioprio_value(BE, 0)
        );
    }

    #[test]
    fn the_best_of_a_group_is_the_one_served_first() {
        assert_eq!(
            ioprio_best(ioprio_value(IDLE, 0), ioprio_value(BE, 7)),
            ioprio_value(BE, 7)
        );
        assert_eq!(
            ioprio_best(ioprio_value(BE, 7), ioprio_value(RT, 7)),
            ioprio_value(RT, 7)
        );
        assert_eq!(
            ioprio_best(ioprio_value(BE, 2), ioprio_value(BE, 5)),
            ioprio_value(BE, 2)
        );
        // NONE reads as the best-effort default, so it loses to BE 2 and
        // beats BE 5 -- it is not the numerically smallest word.
        assert_eq!(ioprio_best(0, ioprio_value(BE, 5)), ioprio_value(BE, 4));
        assert_eq!(ioprio_best(0, ioprio_value(BE, 2)), ioprio_value(BE, 2));
    }

    #[test]
    fn the_who_flavours_are_offset_by_one_from_setpriority() {
        assert_eq!(ioprio_which(1), Ok(0));
        assert_eq!(ioprio_which(2), Ok(1));
        assert_eq!(ioprio_which(3), Ok(2));
        assert_eq!(ioprio_which(0), Err(LxError::EINVAL));
        assert_eq!(ioprio_which(4), Err(LxError::EINVAL));
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn known_versions_map_to_element_counts() {
        assert_eq!(cap_version_elems(LINUX_CAPABILITY_VERSION_1), Some(1));
        assert_eq!(cap_version_elems(LINUX_CAPABILITY_VERSION_2), Some(2));
        assert_eq!(cap_version_elems(LINUX_CAPABILITY_VERSION_3), Some(2));
    }

    #[test]
    fn unknown_versions_are_refused() {
        // 0 is what libcap sends to probe; garbage must also be refused.
        for v in [0u32, 1, 0x2008_0523, u32::MAX] {
            assert_eq!(cap_version_elems(v), None, "version {:#x}", v);
        }
    }

    const V1: u32 = LINUX_CAPABILITY_VERSION_1;
    const V3: u32 = LINUX_CAPABILITY_VERSION_3;

    #[test]
    fn a_capget_with_null_data_is_a_probe_that_always_answers_zero() {
        // libcap: `capget(&hdr, NULL)` with version 0 to learn the kernel's
        // version from the written-back header. The kernel answers 0 to a
        // null `dataptr` whatever the header held, even a negative pid.
        assert_eq!(capget_plan(0, 0, true), Ok(None));
        assert_eq!(capget_plan(V3, 0, true), Ok(None));
        assert_eq!(capget_plan(0, -1, true), Ok(None));
        assert_eq!(capget_plan(V3, -1, true), Ok(None));
    }

    #[test]
    fn a_capget_that_reads_needs_a_known_version_and_a_non_negative_pid() {
        assert_eq!(capget_plan(V1, 0, false), Ok(Some(1)));
        assert_eq!(capget_plan(V3, 42, false), Ok(Some(2)));
        // The version first, then the pid: `cap_validate_magic` runs before
        // the pid is even read.
        assert_eq!(capget_plan(0, 0, false), Err(LxError::EINVAL));
        assert_eq!(capget_plan(0, -1, false), Err(LxError::EINVAL));
        assert_eq!(capget_plan(V3, -1, false), Err(LxError::EINVAL));
        assert_eq!(capget_plan(V3, i32::MIN, false), Err(LxError::EINVAL));
    }

    #[test]
    fn capget_looks_up_any_pid_but_zero_and_its_own() {
        assert!(capget_names_the_caller(0, 7));
        assert!(capget_names_the_caller(7, 7));
        // Another process: `cap_get_target_pid` finds it (or ESRCH). It used
        // to be answered with the caller's own sets, whether it existed or not.
        assert!(!capget_names_the_caller(8, 7));
        assert!(!capget_names_the_caller(1, 7));
    }

    #[test]
    fn capset_may_only_affect_the_caller_and_says_eperm_to_everyone_else() {
        assert_eq!(capset_target(0, 7), Ok(()));
        assert_eq!(capset_target(7, 7), Ok(()));
        // Another pid, existing or not, was answered 0 before.
        assert_eq!(capset_target(8, 7), Err(LxError::EPERM));
        assert_eq!(capset_target(1, 7), Err(LxError::EPERM));
        // `SYSCALL_DEFINE2(capset)` has no `pid < 0` rule: -1 is EPERM like
        // any other pid that is not the caller, not capget's EINVAL.
        assert_eq!(capset_target(-1, 7), Err(LxError::EPERM));
        assert_eq!(capset_target(i32::MIN, 7), Err(LxError::EPERM));
    }

    #[test]
    fn the_sets_are_assembled_low_element_first_and_v1_leaves_the_top_half_zero() {
        let low = CapUserData {
            effective: 0x1,
            permitted: 0x3,
            inheritable: 0x7,
        };
        let high = CapUserData {
            effective: 0x10,
            permitted: 0x30,
            inheritable: 0x70,
        };
        assert_eq!(
            CapSets::from_user(&[low]),
            CapSets {
                effective: 0x1,
                permitted: 0x3,
                inheritable: 0x7
            }
        );
        assert_eq!(
            CapSets::from_user(&[low, high]),
            CapSets {
                effective: 0x10_0000_0001,
                permitted: 0x30_0000_0003,
                inheritable: 0x70_0000_0007
            }
        );
        assert_eq!(
            CapSets::from_user(&[]),
            CapSets {
                effective: 0,
                permitted: 0,
                inheritable: 0
            }
        );
    }

    #[test]
    fn the_full_set_is_every_capability_up_to_cap_last_cap() {
        use linux_object::process::CAP_LAST_CAP;
        assert_eq!(CAP_FULL_SET.count_ones(), CAP_LAST_CAP + 1);
        assert_ne!(CAP_FULL_SET & (1 << CAP_LAST_CAP), 0);
        assert_eq!(CAP_FULL_SET & (1 << (CAP_LAST_CAP + 1)), 0);
    }

    fn sets(effective: u64, permitted: u64, inheritable: u64) -> CapSets {
        CapSets {
            effective,
            permitted,
            inheritable,
        }
    }

    #[test]
    fn a_process_may_drop_capabilities_but_never_gain_them() {
        let root = sets(CAP_FULL_SET, CAP_FULL_SET, 0);
        // Dropping to a subset, or to nothing, is what daemons do.
        assert_eq!(
            capset_sets_verdict(&root, CAP_FULL_SET, &sets(0x3, 0x3, 0)),
            Ok(())
        );
        assert_eq!(
            capset_sets_verdict(&root, CAP_FULL_SET, &sets(0, 0, 0)),
            Ok(())
        );
        assert_eq!(capset_sets_verdict(&root, CAP_FULL_SET, &root), Ok(()));
        // An unprivileged process holds nothing, so any bit is a gain: EPERM.
        // It used to be answered 0.
        let nobody = sets(0, 0, 0);
        assert_eq!(capset_sets_verdict(&nobody, CAP_FULL_SET, &nobody), Ok(()));
        assert_eq!(
            capset_sets_verdict(&nobody, CAP_FULL_SET, &sets(0, 0x1, 0)),
            Err(LxError::EPERM)
        );
        assert_eq!(
            capset_sets_verdict(&nobody, CAP_FULL_SET, &sets(0, 0, 0x1)),
            Err(LxError::EPERM)
        );
        // A permitted bit outside the old permitted set is a gain for root
        // too: bit 41 is above CAP_LAST_CAP and root does not hold it.
        assert_eq!(
            capset_sets_verdict(&root, CAP_FULL_SET, &sets(0, 1 << 41, 0)),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn the_effective_set_must_fit_in_the_new_permitted_set() {
        let root = sets(CAP_FULL_SET, CAP_FULL_SET, 0);
        // Keep bit 0 permitted and make bit 1 effective: `cap_capset`'s
        // last test, against the NEW permitted set, not the old one.
        assert_eq!(
            capset_sets_verdict(&root, CAP_FULL_SET, &sets(0x2, 0x1, 0)),
            Err(LxError::EPERM)
        );
        assert_eq!(
            capset_sets_verdict(&root, CAP_FULL_SET, &sets(0x1, 0x1, 0)),
            Ok(())
        );
    }

    #[test]
    fn the_inheritable_set_is_bounded_by_what_is_held_and_by_the_bounding_set() {
        let held = sets(0x7, 0x7, 0x8);
        // Old inheritable | old permitted = 0xf; bit 4 is in neither.
        assert_eq!(
            capset_sets_verdict(&held, CAP_FULL_SET, &sets(0, 0x7, 0xf)),
            Ok(())
        );
        assert_eq!(
            capset_sets_verdict(&held, CAP_FULL_SET, &sets(0, 0x7, 0x10)),
            Err(LxError::EPERM)
        );
        // Bit 2 is permitted but outside a bounding set of {0, 1, 3}: no new
        // inheritable capability outside the bounding set.
        assert_eq!(
            capset_sets_verdict(&held, 0xb, &sets(0, 0x7, 0x4)),
            Err(LxError::EPERM)
        );
        // ...unless it was inheritable already (old inheritable | bset).
        assert_eq!(capset_sets_verdict(&held, 0x7, &sets(0, 0x7, 0x8)), Ok(()));
    }
}

/// sysinfo() return information sturct
#[repr(C)]
#[derive(Debug, Default)]
pub struct SysInfo {
    /// Seconds since boot
    uptime: u64,
    /// 1, 5, and 15 minute load averages
    loads: [u64; 3],
    /// Total usable main memory size
    totalram: u64,
    /// Available memory size
    freeram: u64,
    /// Amount of shared memory
    sharedram: u64,
    /// Memory used by buffers
    bufferram: u64,
    /// Total swa Total swap space sizep space size
    totalswap: u64,
    /// swap space still available
    freeswap: u64,
    /// Number of current processes
    procs: u16,
    /// Total high memory size
    totalhigh: u64,
    /// Available high memory size
    freehigh: u64,
    /// Memory unit size in bytes
    mem_unit: u32,
}

#[cfg(test)]
mod futex_deadline_tests {
    use super::{futex_deadline, NO_FUTEX_DEADLINE};
    use core::time::Duration;

    /// The monotonic clock at the moment `sys_futex` computes a deadline.
    /// Anything but zero, so a deadline that forgot to start from *now* is
    /// visibly different from one that did.
    fn kernel_now() -> Duration {
        Duration::new(1_234, 500_000_000)
    }

    /// A `futex` wait with a null `timespec` blocks indefinitely, and that
    /// must be expressed as a deadline that never fires -- NOT as "no
    /// deadline", which is the shape that lets a caller skip
    /// `Thread::blocking_run`.
    ///
    /// The distinction is not cosmetic. Without `blocking_run` the thread
    /// registers no killer with `Thread::kill`, never enters `BlockedFutex`,
    /// and waits on a `FutexFuture` that has no timer and no signal check --
    /// so a plain contended `pthread_mutex_lock` becomes a wait nothing in
    /// the kernel can end but a matching `FUTEX_WAKE`.
    #[test]
    fn no_timeout_is_a_deadline_that_never_fires() {
        assert_eq!(
            futex_deadline(kernel_now(), None, None),
            NO_FUTEX_DEADLINE,
            "a relative wait with no timeout must still get a deadline"
        );
        assert_eq!(
            futex_deadline(kernel_now(), None, Some(Duration::from_secs(9_999))),
            NO_FUTEX_DEADLINE,
            "an absolute wait with a null timespec ignores the clock too"
        );
    }

    /// Pinned against literals, not against `NO_FUTEX_DEADLINE` itself: the
    /// point of the constant is that it is far enough out that a kernel that
    /// boots today never reaches it. `u64::MAX` nanoseconds is ~584 years of
    /// uptime, and this is the same value `Thread::handle_exception` already
    /// uses for a blocked-on-exception thread.
    #[test]
    fn the_never_deadline_is_centuries_away() {
        assert_eq!(NO_FUTEX_DEADLINE.as_nanos(), u64::MAX as u128);
        assert!(
            NO_FUTEX_DEADLINE.as_secs() > 500 * 365 * 24 * 3600,
            "a 'never' deadline that can be reached is a timeout: {:?}",
            NO_FUTEX_DEADLINE
        );
    }

    /// Plain `FUTEX_WAIT` takes a RELATIVE timeout, counted from now.
    #[test]
    fn a_relative_timeout_counts_from_now() {
        assert_eq!(
            futex_deadline(kernel_now(), Some(Duration::from_millis(250)), None),
            kernel_now() + Duration::from_millis(250)
        );
    }

    /// `FUTEX_WAIT_BITSET` and the PI locks take an ABSOLUTE deadline on the
    /// clock they selected, while the kernel sleeps on the monotonic one.
    /// Only what is LEFT of it carries over.
    #[test]
    fn an_absolute_deadline_carries_over_what_is_left_of_it() {
        // The caller asked for 09:00:03 on a clock that reads 09:00:01, so
        // there are two seconds left however far that clock is from the
        // kernel's own.
        let clock_now = Duration::from_secs(1_600_000_001);
        let absolute = Duration::from_secs(1_600_000_003);
        assert_eq!(
            futex_deadline(kernel_now(), Some(absolute), Some(clock_now)),
            kernel_now() + Duration::from_secs(2),
            "an absolute deadline must not be used as a monotonic one"
        );
    }

    /// The realtime and monotonic clocks read wildly different numbers (one
    /// is seconds since 1970, the other since boot), so reading the wrong one
    /// for an absolute deadline is the difference between "in 2 seconds" and
    /// "in fifty years". Both must give the same remaining time.
    #[test]
    fn the_clock_the_deadline_is_on_is_the_clock_it_is_measured_against() {
        let left = Duration::from_secs(2);
        let realtime = futex_deadline(
            kernel_now(),
            Some(Duration::from_secs(1_600_000_000) + left),
            Some(Duration::from_secs(1_600_000_000)),
        );
        let monotonic = futex_deadline(
            kernel_now(),
            Some(Duration::from_secs(42) + left),
            Some(Duration::from_secs(42)),
        );
        assert_eq!(realtime, monotonic);
        assert_eq!(realtime, kernel_now() + left);
    }

    /// An absolute deadline already in the past is not an error and not a
    /// wait: Linux answers `ETIMEDOUT` straight away, which here is a
    /// deadline of "now" that `blocking_run`'s `sleep_until` has already
    /// passed. It must saturate rather than underflow.
    #[test]
    fn an_absolute_deadline_already_past_times_out_at_once() {
        let clock_now = Duration::from_secs(1_600_000_010);
        let absolute = Duration::from_secs(1_600_000_000);
        assert_eq!(
            futex_deadline(kernel_now(), Some(absolute), Some(clock_now)),
            kernel_now(),
            "a deadline ten seconds in the past must expire now, not wrap"
        );
    }

    /// The timeout comes from userspace, and `Duration + Duration` PANICS on
    /// overflow -- in the kernel, from an unprivileged `futex` call. Both
    /// paths saturate.
    #[test]
    fn an_absurd_timeout_saturates_instead_of_panicking() {
        let huge = Duration::new(u64::MAX, 999_999_999);
        assert_eq!(
            futex_deadline(kernel_now(), Some(huge), None),
            Duration::MAX
        );
        assert_eq!(
            futex_deadline(kernel_now(), Some(huge), Some(Duration::ZERO)),
            Duration::MAX
        );
    }

    /// A deadline already reached and a deadline that never fires are the two
    /// ends of the range, and nothing in between may collapse onto either.
    #[test]
    fn a_finite_timeout_is_never_the_never_deadline() {
        for timeout in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_secs(60),
            Duration::from_secs(365 * 24 * 3600),
        ] {
            let deadline = futex_deadline(kernel_now(), Some(timeout), None);
            assert_ne!(
                deadline, NO_FUTEX_DEADLINE,
                "a {:?} timeout must stay a timeout",
                timeout
            );
            assert_eq!(deadline, kernel_now() + timeout);
        }
    }
}

/// `getrandom(2)` flags (`include/uapi/linux/random.h`).
const GRND_NONBLOCK: u32 = 0x0001;
const GRND_RANDOM: u32 = 0x0002;
const GRND_INSECURE: u32 = 0x0004;

/// How many bytes a `getrandom(len, flags)` will deliver, or `EINVAL` when
/// `flags` is not a word `getrandom(2)` defines.
///
/// The `flags` argument used to be read by nobody: every bit of it, including
/// the ones `getrandom` will never define, came back as a successful draw.
/// That is the wrong answer to a feature probe -- `GRND_INSECURE` arrived in
/// Linux 5.6, and the way a program finds out whether it has it is by asking
/// and reading the errno.
///
/// The cap is Linux's own (`if (len > INT_MAX) len = INT_MAX;`): the count is
/// handed back through the syscall's signed return value, and a request past
/// `INT_MAX` is answered with a short draw rather than a number the caller
/// cannot use.
pub(crate) fn getrandom_args(len: usize, flags: u32) -> Result<usize, LxError> {
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 {
        return Err(LxError::EINVAL);
    }
    // "Requesting insecure and blocking randomness at the same time makes no
    // sense" (`drivers/char/random.c`). `GRND_NONBLOCK` beside `GRND_INSECURE`
    // does make sense and is accepted.
    if flags & (GRND_INSECURE | GRND_RANDOM) == GRND_INSECURE | GRND_RANDOM {
        return Err(LxError::EINVAL);
    }
    Ok(len.min(i32::MAX as usize))
}

#[cfg(test)]
mod getrandom_tests {
    use super::*;

    #[test]
    fn the_three_flags_getrandom_defines_come_through() {
        for flags in [
            0,
            GRND_NONBLOCK,
            GRND_RANDOM,
            GRND_INSECURE,
            GRND_NONBLOCK | GRND_RANDOM,
            GRND_NONBLOCK | GRND_INSECURE,
        ] {
            assert_eq!(getrandom_args(32, flags), Ok(32), "{flags:#x}");
        }
    }

    #[test]
    fn asking_for_insecure_and_blocking_randomness_at_once_makes_no_sense() {
        assert_eq!(
            getrandom_args(32, GRND_INSECURE | GRND_RANDOM),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            getrandom_args(32, GRND_INSECURE | GRND_RANDOM | GRND_NONBLOCK),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_bit_getrandom_does_not_define_is_refused() {
        for flags in [0x8, 0x10, 1 << 31, u32::MAX] {
            assert_eq!(
                getrandom_args(32, flags),
                Err(LxError::EINVAL),
                "{flags:#x}"
            );
        }
    }

    #[test]
    fn a_request_larger_than_a_signed_int_is_cut_down_to_one() {
        assert_eq!(getrandom_args(i32::MAX as usize, 0), Ok(i32::MAX as usize));
        assert_eq!(
            getrandom_args(i32::MAX as usize + 1, 0),
            Ok(i32::MAX as usize)
        );
        assert_eq!(getrandom_args(usize::MAX, 0), Ok(i32::MAX as usize));
        // The cap is a cap, not a rounding: a short request is untouched.
        assert_eq!(getrandom_args(0, 0), Ok(0));
        assert_eq!(getrandom_args(1, 0), Ok(1));
    }
}
