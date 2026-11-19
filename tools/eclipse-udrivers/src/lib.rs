//! The drivers of Eclipse OS that run as processes.
//!
//! A device the kernel was told to leave alone (`udriver=<kind>@<N>` on its
//! command line) is driven from here. A driver claims the device
//! ([`claim`]), which gives it the registers, a BTI for DMA memory and the
//! interrupt; brings it up over whichever virtio transport it sits on
//! ([`virtio`]); hands it to the kernel as a disk or an interface; and then
//! answers the kernel's requests on a FIFO ([`fifo`]) until it is killed.
//! `eclipse-init` runs each as a boot service and starts it again when it
//! dies; the kernel keeps the disk or the interface meanwhile.
//!
//! What can be decided without a device -- which requests are in range, how
//! a batch is cut, where a queue's rings sit, when to look before sleeping
//! -- is kept apart from the system calls, and tested on the host with
//! `cargo test`.

pub mod ahci;
pub mod blk;
pub mod channel;
pub mod claim;
pub mod e1000e;
pub mod evdev;
pub mod fifo;
pub mod hybrid;
pub mod input;
pub mod lent;
pub mod net;
pub mod nvme;
pub mod ps2;
pub mod queue;
pub mod served;
pub mod status;
pub mod virtio;
pub mod zx;

/// The page size of every machine these run on.
pub const PAGE: usize = 4096;

/// Nanoseconds on the monotonic clock.
/// Whether this machine runs on one CPU: then a look for a client's next
/// request is never answered while it lasts (the client cannot run), and
/// when a timer tick lets the client in, the look seems to pay and goes
/// on costing a system call per turn.
pub fn one_cpu() -> bool {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) <= 1 }
}

pub fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a timespec to write, and the clock exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
