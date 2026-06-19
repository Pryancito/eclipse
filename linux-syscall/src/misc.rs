use super::*;
use core::time::Duration;
use kernel_hal::timer::timer_now;
use linux_object::time::*;
use zircon_object::task::ThreadState;

impl Syscall<'_> {
    #[cfg(target_arch = "x86_64")]
    /// set architecture-specific thread state
    /// for x86_64 currently
    pub fn sys_arch_prctl(&mut self, code: i32, addr: usize) -> SysResult {
        const ARCH_SET_FS: i32 = 0x1002;
        match code {
            ARCH_SET_FS => {
                info!("sys_arch_prctl: set FSBASE to {:#x}", addr);
                self.thread.with_context(|ctx| {
                    ctx.set_field(kernel_hal::context::UserContextField::ThreadPointer, addr)
                })?;
                Ok(0)
            }
            _ => Err(LxError::EINVAL),
        }
    }

    /// get name and information about current kernel
    pub fn sys_uname(&self, buf: UserOutPtr<u8>) -> SysResult {
        info!("uname: buf={:?}", buf);

        let release = alloc::string::String::from(concat!(env!("CARGO_PKG_VERSION"), "-zcore"));
        #[cfg(not(target_os = "none"))]
        let release = release + "-libos";

        let vdso_const = kernel_hal::vdso::vdso_constants();

        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else if cfg!(target_arch = "riscv64") {
            "riscv64"
        } else {
            "unknown"
        };

        let strings = [
            "Linux",                            // sysname
            "Eclipse",                          // nodename
            release.as_str(),                   // release
            vdso_const.version_string.as_str(), // version
            arch,                               // machine
            "Eclipse-OS",                       // domainname
        ];

        for (i, &s) in strings.iter().enumerate() {
            const OFFSET: usize = 65;
            buf.add(i * OFFSET).write_cstring(s)?;
        }
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
        let sysinfo = SysInfo {
            // Seconds since boot, from the monotonic timer (same source as
            // /proc/uptime).
            uptime: timer_now().as_secs(),
            // We don't track real load averages yet; report 0 (the kernel
            // fixed-point format is value << 16, so 0 stays 0).
            loads: [0; 3],
            totalram: total as u64,
            freeram: total.saturating_sub(used) as u64,
            mem_unit: 1,
            ..SysInfo::default()
        };
        sys_info.write(sysinfo)?;
        Ok(0)
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
        const FUTEX_PRIVATE_FLAG: u32 = 0x80;
        const FUTEX_CLOCK_REALTIME: u32 = 0x100;

        debug!(
            "Futex uaddr: {:#x}, op: {:x}, val: {}, val2(timeout_addr): {:x}",
            uaddr, op, val, val2,
        );
        if op & FUTEX_PRIVATE_FLAG == 0 {
            // Futexes are per-process objects here, which is correct for
            // private futexes and a usable approximation for shared ones
            // within a single process (e.g. musl pthread_join passes priv=0).
            debug!("process-shared futex is treated as process-private");
        }
        // NOTE: do NOT parse `op` as bitflags — command values are an enum
        // (WAIT_BITSET=9 would alias WAKE=1 when bits are truncated).
        let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
        let futex = self
            .linux_process()
            .get_futex(uaddr)
            .ok_or(LxError::EINVAL)?;
        match cmd {
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
                let res = if let Some(timeout) = timeout_addr.read_if_not_null()? {
                    // FUTEX_WAIT takes a relative timeout; FUTEX_WAIT_BITSET
                    // takes an absolute one on the clock selected by
                    // FUTEX_CLOCK_REALTIME. Convert absolute deadlines to the
                    // kernel's monotonic deadline base.
                    let deadline = if cmd == FUTEX_WAIT_BITSET {
                        let now = if op & FUTEX_CLOCK_REALTIME != 0 {
                            Duration::from(TimeSpec::now())
                        } else {
                            Duration::from(TimeSpec::now_monotonic())
                        };
                        timer_now() + Duration::from(timeout).saturating_sub(now)
                    } else {
                        timer_now() + Duration::from(timeout)
                    };
                    self.thread
                        .blocking_run(future, ThreadState::BlockedFutex, deadline, None)
                        .await
                } else {
                    future.await
                };
                match res {
                    Ok(_) => Ok(0),
                    Err(e) => Err(e.into()),
                }
            }
            FUTEX_WAKE | FUTEX_WAKE_BITSET => Ok(futex.wake(val as _)),
            FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
                let requeue_futex = self
                    .linux_process()
                    .get_futex(uaddr2)
                    .ok_or(LxError::EINVAL)?;
                // FUTEX_CMP_REQUEUE checks *uaddr against val3 first.
                let res = futex.requeue(
                    val3 as i32,
                    val as _,
                    val2,
                    &requeue_futex,
                    None,
                    cmd == FUTEX_CMP_REQUEUE,
                );
                match res {
                    Ok(_) => Ok(0),
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
        let proc = self.linux_process();
        match resource {
            RLIMIT_STACK => {
                old_limit.write_if_not_null(RLimit {
                    cur: USER_STACK_SIZE as u64,
                    max: USER_STACK_SIZE as u64,
                })?;
                Ok(0)
            }
            RLIMIT_NOFILE => {
                let new_limit = new_limit.read_if_not_null()?;
                old_limit.write_if_not_null(proc.file_limit(new_limit))?;
                Ok(0)
            }
            RLIMIT_RSS | RLIMIT_AS => {
                old_limit.write_if_not_null(RLimit {
                    cur: 1024 * 1024 * 1024,
                    max: 1024 * 1024 * 1024,
                })?;
                Ok(0)
            }
            _ => Err(LxError::ENOSYS),
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
    /// - returns the number of bytes that were copied to the buffer buf.
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
                kernel_hal::cpu::reset();
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
                kernel_hal::cpu::reset();
            }
            0x456789ab => {
                // LINUX_REBOOT_CMD_SW_SUSPEND
                warn!("reboot: sw_suspend unimplemented");
                Err(LxError::EINVAL)
            }
            0x01234567 => {
                // LINUX_REBOOT_CMD_RESTART
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
    /// - `flag` - a bit mask that can contain zero or more of the following values ORed together:
    ///   - GRND_RANDOM
    ///   - GRND_NONBLOCK
    /// - returns the number of bytes that were copied to the buffer buf.
    pub fn sys_getrandom(&mut self, buf: UserOutPtr<u8>, len: usize, flag: u32) -> SysResult {
        info!("getrandom: buf: {:?}, len: {:?}, flag {:?}", buf, len, flag);
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

const USER_STACK_SIZE: usize = 8 * 1024 * 1024; // 8 MB, the default config of Linux

const RLIMIT_STACK: usize = 3;
const RLIMIT_RSS: usize = 5;
const RLIMIT_NOFILE: usize = 7;
const RLIMIT_AS: usize = 9;

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
