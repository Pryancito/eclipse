//! The two pieces of libc plumbing both clients need: a memfd-backed shm pool
//! for wl_shm, and a detached launch.

use std::os::fd::{FromRawFd, OwnedFd};

/// Allocate a `total`-byte memfd and map it read-write shared, ready to hand
/// to `wl_shm.create_pool`. `who` only names the memfd (visible in
/// /proc/*/fd), so a stuck mapping is attributable to the right client.
pub fn map_shm_pool(total: usize, who: &str) -> Option<(*mut u8, OwnedFd)> {
    let name = std::ffi::CString::new(who).ok()?;
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        eprintln!("{who}: memfd_create failed");
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::ftruncate(raw, total as libc::off_t) } != 0 {
        eprintln!("{who}: ftruncate failed");
        return None;
    }
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            total,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            raw,
            0,
        )
    };
    if map == libc::MAP_FAILED {
        eprintln!("{who}: mmap failed");
        return None;
    }
    Some((map as *mut u8, fd))
}

/// Run `cmd` through `/bin/sh -c`, detached via double-fork: the intermediate
/// child `setsid()`s and exits at once, so the grandchild is reparented to
/// init and the caller never accumulates zombies (the parent reaps the
/// intermediate with a waitpid that returns immediately).
pub fn spawn_detached(cmd: &str) {
    // Build the argv string BEFORE forking. Between fork() and exec() a child
    // may call only async-signal-safe functions, and CString::new allocates.
    // Today that is latent rather than live — par_rows joins its workers
    // before returning, so no thread is alive when the event loop calls this —
    // but the safety rests on an invariant nothing enforces: add any
    // background thread (an async icon loader, say) and a fork landing while
    // it held the allocator lock would leave the child deadlocked forever on a
    // lock whose owner does not exist in it, as a launch that silently never
    // happens. Cheap to make structural.
    let Ok(c) = std::ffi::CString::new(cmd) else {
        return; // an interior NUL cannot be passed to exec at all
    };
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            // Intermediate child: new session, fork the real child, exit.
            libc::setsid();
            if libc::fork() == 0 {
                let sh = b"/bin/sh\0";
                let dashc = b"-c\0";
                let argv = [
                    sh.as_ptr() as *const libc::c_char,
                    dashc.as_ptr() as *const libc::c_char,
                    c.as_ptr(),
                    std::ptr::null(),
                ];
                libc::execv(sh.as_ptr() as *const libc::c_char, argv.as_ptr());
                libc::_exit(127);
            }
            libc::_exit(0);
        }
        if pid > 0 {
            let mut st = 0;
            libc::waitpid(pid, &mut st, 0);
        }
    }
}
