//! The system calls: Zircon's, through the gateway, and Eclipse's own next
//! to them.
//!
//! A Linux process reaches a Zircon call as Linux system call
//! `GATEWAY_BASE + n`, with the arguments in the Linux registers; a call of
//! more than six sets `GATEWAY_INDIRECT` and passes a pointer to all eight.
//! The status comes back the way an `errno` does, so libc's `syscall` hands
//! it over as -1 and `errno == -status`.

use std::fmt;

/// A handle of this process, as the kernel numbers it.
pub type Handle = u32;

const GATEWAY_BASE: libc::c_long = 0x5a00_0000;
const GATEWAY_INDIRECT: libc::c_long = 0x8000;

/// The calls this crate makes (zircon-syscall's numbers, and Eclipse's from
/// 0x4000: see `loader/src/linux.rs`).
pub mod nr {
    pub const BTI_PIN: i64 = 1;
    pub const CHANNEL_CREATE: i64 = 7;
    pub const CHANNEL_READ: i64 = 8;
    pub const CHANNEL_WRITE: i64 = 10;
    pub const FIFO_CREATE: i64 = 34;
    pub const FIFO_READ: i64 = 35;
    pub const FIFO_WRITE: i64 = 36;
    pub const HANDLE_CLOSE: i64 = 45;
    pub const INTERRUPT_WAIT: i64 = 54;
    pub const OBJECT_WAIT_ONE: i64 = 81;
    pub const VMO_CREATE_CHILD: i64 = 192;
    pub const VMO_CREATE_CONTIGUOUS: i64 = 193;
    pub const ECLIPSE_HANDLE_TO_FD: i64 = 0x4000;
    pub const ECLIPSE_FD_TO_HANDLE: i64 = 0x4001;
    pub const ECLIPSE_CHANNEL_REPLY_WAIT: i64 = 0x4004;
    pub const ECLIPSE_DEVICE_CLAIM: i64 = 0x4005;
    pub const ECLIPSE_BLOCK_SERVE: i64 = 0x4006;
    pub const ECLIPSE_NET_SERVE: i64 = 0x4007;
    pub const ECLIPSE_FIFO_REPLY_WAIT: i64 = 0x4008;
    pub const ECLIPSE_NETSTACK_SERVE: i64 = 0x4009;
    pub const ECLIPSE_NETSTACK_USE: i64 = 0x400a;
}

/// A `zx_status_t` that is not `ZX_OK`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Status(pub i32);

impl Status {
    pub const INTERNAL_INTR_RETRY: Status = Status(-6);
    pub const INVALID_ARGS: Status = Status(-10);
    pub const OUT_OF_RANGE: Status = Status(-14);
    pub const TIMED_OUT: Status = Status(-21);
    pub const SHOULD_WAIT: Status = Status(-22);
    pub const PEER_CLOSED: Status = Status(-24);
    pub const ALREADY_BOUND: Status = Status(-27);
    pub const IO: Status = Status(-40);
    /// Not Zircon's: what Linux answers to a system call it does not have,
    /// which is what the gateway's numbers are when the gateway is off.
    pub const NO_GATEWAY: Status = Status(-libc::ENOSYS);
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What libc's `syscall` returned, as a status.
fn answer(r: libc::c_long) -> Result<usize, Status> {
    if r == -1 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        Err(Status(-errno))
    } else {
        Ok(r as usize)
    }
}

/// Call `n` with up to six arguments.
///
/// # Safety
/// The arguments are what call `n` takes: the kernel reads and writes this
/// process's memory through the pointers among them.
pub unsafe fn call(n: i64, a: [usize; 6]) -> Result<usize, Status> {
    answer(libc::syscall(
        GATEWAY_BASE + n as libc::c_long,
        a[0],
        a[1],
        a[2],
        a[3],
        a[4],
        a[5],
    ))
}

/// Call `n` with eight arguments.
///
/// # Safety
/// As [`call`].
pub unsafe fn call8(n: i64, a: [usize; 8]) -> Result<usize, Status> {
    answer(libc::syscall(
        GATEWAY_BASE + (n as libc::c_long | GATEWAY_INDIRECT),
        a.as_ptr(),
    ))
}

/// The signals of a FIFO end or a channel end: something to read, and the
/// other end gone.
pub const READABLE: u32 = 1 << 0;
pub const PEER_CLOSED: u32 = 1 << 2;

/// Sleep until `handle` has one of `signals`. The ones it has.
pub fn wait_one(handle: Handle, signals: u32) -> Result<u32, Status> {
    wait_one_until(handle, signals, i64::MAX) // no deadline
}

/// Sleep until `handle` has one of `signals` or the monotonic clock
/// ([`crate::now_ns`]) reads `deadline`, which is `TIMED_OUT`. The signals
/// it has.
pub fn wait_one_until(handle: Handle, signals: u32, deadline: i64) -> Result<u32, Status> {
    let mut observed: u32 = 0;
    // SAFETY: the kernel writes the signals it saw into `observed`.
    unsafe {
        call(
            nr::OBJECT_WAIT_ONE,
            [
                handle as usize,
                signals as usize,
                deadline as usize,
                &mut observed as *mut u32 as usize,
                0,
                0,
            ],
        )?;
    }
    Ok(observed)
}

/// Let go of `handle`.
pub fn close(handle: Handle) {
    // SAFETY: no pointers. A handle that is not one is the only way this
    // fails, and there is nothing to do about it.
    let _ = unsafe { call(nr::HANDLE_CLOSE, [handle as usize, 0, 0, 0, 0, 0]) };
}

/// Map all `len` bytes of the VMO `vmo`, readable and writable, shared
/// with whoever else has it. The handle is gone afterwards: it moves into
/// the descriptor the mapping is made from. The mapping is never taken
/// down: a driver's registers and DMA memory last as long as the process.
pub fn map(vmo: Handle, len: usize) -> Result<*mut u8, String> {
    // SAFETY: no pointers; the call answers a descriptor or a status.
    let fd = unsafe { call(nr::ECLIPSE_HANDLE_TO_FD, [vmo as usize, 0, 0, 0, 0, 0]) }
        .map_err(|e| format!("handle_to_fd: {e}"))? as libc::c_int;
    // SAFETY: a fresh mapping of a descriptor this process owns.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    let failed = p == libc::MAP_FAILED;
    let why = std::io::Error::last_os_error();
    // SAFETY: `fd` is this function's own.
    unsafe { libc::close(fd) };
    if failed {
        return Err(format!("mmap: {why}"));
    }
    Ok(p as *mut u8)
}
