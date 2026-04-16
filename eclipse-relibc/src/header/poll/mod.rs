//! poll.h - Poll implementation
use crate::types::*;

/// Events bitmask: data ready to read.
pub const POLLIN: c_short = 0x0001;
/// Events bitmask: data ready to write.
pub const POLLOUT: c_short = 0x0004;
/// Events bitmask: error condition.
pub const POLLERR: c_short = 0x0008;
/// Events bitmask: hung up.
pub const POLLHUP: c_short = 0x0010;
/// Events bitmask: invalid request.
pub const POLLNVAL: c_short = 0x0020;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct pollfd {
    pub fd: c_int,
    pub events: c_short,
    pub revents: c_short,
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn poll(fds: *mut pollfd, nfds: nfds_t, timeout: c_int) -> c_int;
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn poll(fds: *mut pollfd, nfds: nfds_t, timeout: c_int) -> c_int {
    use crate::read;
    use crate::{clock_gettime, CLOCK_MONOTONIC, timespec, nanosleep};
    use crate::EAGAIN;
    
    if fds.is_null() {
        return -1;
    }

    let start_time = if timeout > 0 {
        let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
        clock_gettime(CLOCK_MONOTONIC, &mut ts);
        Some(ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000)
    } else {
        None
    };

    loop {
        let mut ready_count = 0;
        let mut fds_slice = core::slice::from_raw_parts_mut(fds, nfds as usize);

        for pfd in fds_slice.iter_mut() {
            pfd.revents = 0;
            if pfd.fd < 0 {
                continue;
            }

            // Check for POLLIN by attempting a zero-byte non-blocking read
            // NOTE: We rely on the kernel handling 0-byte reads by checking readiness
            let mut buf = [0u8; 0];
            let res = read(pfd.fd, buf.as_mut_ptr() as *mut c_void, 0);
            
            if res >= 0 {
                if (pfd.events & POLLIN) != 0 {
                    pfd.revents |= POLLIN;
                    ready_count += 1;
                }
            } else {
                extern "C" {
                    fn __errno_location() -> *mut c_int;
                }
                let err = *__errno_location();
                if err != EAGAIN {
                    pfd.revents |= POLLERR;
                    ready_count += 1;
                }
            }
            
            // For now, we stub POLLOUT as always ready if requested
            if (pfd.events & POLLOUT) != 0 {
                pfd.revents |= POLLOUT;
                // If we didn't already count it for POLLIN/POLLERR
                if (pfd.revents & !POLLOUT) == 0 {
                    ready_count += 1;
                }
            }
        }

        if ready_count > 0 || timeout == 0 {
            return ready_count;
        }

        if let Some(start) = start_time {
            let mut current_ts = timespec { tv_sec: 0, tv_nsec: 0 };
            clock_gettime(CLOCK_MONOTONIC, &mut current_ts);
            let now = current_ts.tv_sec * 1000 + current_ts.tv_nsec / 1_000_000;
            if now - start >= timeout as i64 {
                return 0;
            }
        }

        // Sleep for a short interval to avoid 100% CPU usage
        let req = timespec { tv_sec: 0, tv_nsec: 1_000_000 }; // 1ms
        nanosleep(&req, core::ptr::null_mut());
    }
}
