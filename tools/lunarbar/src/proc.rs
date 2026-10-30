//! The two pieces of libc plumbing both clients need: a memfd-backed shm pool
//! for wl_shm, and a detached launch.

use std::os::fd::{FromRawFd, OwnedFd};

/// Frames per `wl_shm` pool: one to draw into while the compositor holds the
/// other. **One constant for the whole crate**, because the pool's size, the
/// frame offsets and the `busy` array all have to agree about it, and both
/// clients used to declare their own.
pub const BUFFERS: usize = 2;

/// The buffer a frame can be drawn into, or `None` while the compositor holds
/// every one of them.
///
/// `next` first so that the buffers alternate: always drawing into the lowest
/// free one would reuse the buffer just released while the other sits idle,
/// which is single buffering with two allocations.
///
/// **`None` is a frame OWED, not a frame skipped.** A client that returns here
/// and forgets leaves the surface showing the previous contents until something
/// else happens to redraw it, which on a keystroke-driven overlay is the next
/// keystroke. The caller has to remember and redraw when a buffer comes back.
pub fn free_buffer(busy: [bool; BUFFERS], next: usize) -> Option<usize> {
    if !busy[next] {
        Some(next)
    } else if !busy[1 - next] {
        Some(1 - next)
    } else {
        None
    }
}

/// The sizes one `wl_shm` pool of [`BUFFERS`] ARGB8888 frames needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolGeometry {
    /// Bytes for the whole pool. Fits in an `i32`.
    pub total: usize,
    /// Bytes per row: `w * 4`.
    pub stride: usize,
    /// Bytes for one frame: `stride * h`. Frame `i` starts at `i * frame_size`.
    pub frame_size: usize,
}

/// The pool sizes for a `w` x `h` surface, or `None` when they do not fit.
///
/// **`i32`, not `usize`, is the real ceiling**: `wl_shm.create_pool` takes the
/// size as an `i32` and `wl_shm_pool.create_buffer` takes its offset and stride
/// as `i32` too, so a pool past `i32::MAX` becomes a NEGATIVE number on the
/// wire and the compositor kills the client with a protocol error. Every caller
/// was computing this itself -- the same eleven lines in four places, three bars
/// and an overlay -- which is three chances for one of them to drift and lose
/// the check.
pub fn pool_geometry(w: u32, h: u32) -> Option<PoolGeometry> {
    // `try_from` rather than a comparison, so the ceiling is the protocol's own
    // type and not a number anyone can get the wrong way round.
    //
    // The STRIDE is checked first and on its own, because `create_buffer` takes
    // it as an i32 too and a surface of zero height has a total of zero however
    // wide it is: `pool_geometry(u32::MAX, 0)` passes every size check after this
    // one and still hands the compositor a negative stride.
    //
    // It also bounds everything below it: with the stride at most i32::MAX and
    // the height at most u32::MAX, the frame is under 2^63 and the pool under
    // 2^64, so on a 64-bit usize neither multiply can wrap. They stay `checked_`
    // because that reasoning is about this target's word size and not about the
    // protocol, and a wrapped product lands back UNDER the ceiling and passes.
    let stride = (w as usize).checked_mul(4)?;
    i32::try_from(stride).ok()?;
    let frame_size = stride.checked_mul(h as usize)?;
    let total = frame_size.checked_mul(BUFFERS)?;
    i32::try_from(total).ok()?;
    Some(PoolGeometry {
        total,
        stride,
        frame_size,
    })
}

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

/// `cmd` wrapped so that the shell running it backgrounds the whole thing and
/// exits at once.
///
/// The braces are separated by NEWLINES, not `;`. `{ cmd ;}` is a syntax error
/// the moment `cmd` itself ends in `&` or `;` — `{ foo & ;}` — and a launcher
/// must not care how the line it was handed happens to end. A newline
/// terminates a list in every POSIX shell and leaves `}` the separate token it
/// has to be, so any `cmd` that `sh -c` accepts today survives the wrap.
///
/// Backgrounding inside the shell is what keeps the launcher childless: the
/// shell forks for the `&` and exits, so the program is reparented to init and
/// the only child the launcher ever has is a shell that is already gone. The
/// fork for the `&` copies the SHELL's address space, which is a rounding
/// error next to the panel's.
fn backgrounded(cmd: &str) -> String {
    // A blank line needs the no-op colon: braces take a non-empty list, so
    // `{\n\n} &` is a syntax error where plain `sh -c ""` was a silent no-op.
    // A launcher handed nothing must still do nothing, not fail to parse.
    if cmd.trim().is_empty() {
        return String::from("{\n:\n} &\n");
    }
    format!("{{\n{cmd}\n}} &\n")
}

/// Run `cmd` through `/bin/sh -c`, detached, without copying this process's
/// address space.
///
/// This used to be a double `fork()`: the intermediate `setsid()`ed, forked the
/// real child and exited, so the grandchild was reparented to init and the
/// launcher accumulated no zombies. Correct, and on Eclipse ruinously
/// expensive — **two** of them. Eclipse runs with `cow-fork=OFF` (Zircon's
/// hidden-node snapshot tree can resurrect a page, so `fork` takes the eager
/// path and copies every committed page: `fork eager` in `/proc/perf/kernel`
/// measured 167 us per mapping), so each `fork` copied the whole panel —
/// GTK, Mesa, every icon surface — to throw it away microseconds later at
/// `exec`. And the parent blocked in `waitpid` while both copies ran, so the
/// panel froze for the length of its own launch.
///
/// `posix_spawn` is the same thing for free: both glibc and musl implement it
/// with `clone(CLONE_VM | CLONE_VFORK)`, which Eclipse routes through
/// `vfork_from` → `ChildMemory::Parents` — the child runs in the parent's
/// address space and nothing is copied. `POSIX_SPAWN_SETSID` gives the new
/// session that the intermediate fork existed only to provide, so the
/// intermediate goes away with it.
///
/// It also retires a latent deadlock. Between `fork()` and `exec()` a child
/// may call only async-signal-safe functions, so the old code had to build its
/// `CString` before forking; that was safe only because no other thread of the
/// panel is alive at this point, an invariant nothing enforced (add one
/// background icon loader and a fork landing while it held the allocator lock
/// leaves the child deadlocked on a lock whose owner does not exist in it — a
/// launch that silently never happens). `posix_spawn` never runs user code
/// between the clone and the exec, so there is no such window to protect.
pub fn spawn_detached(cmd: &str) {
    let Ok(c) = std::ffi::CString::new(backgrounded(cmd)) else {
        return; // an interior NUL cannot be passed to exec at all
    };
    let sh = c"/bin/sh";
    let dashc = c"-c";
    let argv = [
        sh.as_ptr(),
        dashc.as_ptr(),
        c.as_ptr(),
        std::ptr::null::<libc::c_char>(),
    ];
    let mut pid: libc::pid_t = 0;
    let mut attr: libc::posix_spawnattr_t = unsafe { core::mem::zeroed() };
    // SAFETY: `attr` is owned here and destroyed on every path out; the argv
    // array is NULL-terminated and outlives the call, which copies what it
    // needs before returning.
    let rc = unsafe {
        if libc::posix_spawnattr_init(&mut attr) != 0 {
            return fork_detached(&c);
        }
        let flags = libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETSID as _);
        let rc = if flags == 0 {
            libc::posix_spawn(
                &mut pid,
                sh.as_ptr(),
                std::ptr::null(),
                &attr,
                argv.as_ptr() as *const *mut libc::c_char,
                // The environment the panel itself was given: WAYLAND_DISPLAY
                // and XDG_RUNTIME_DIR are how anything launched finds the
                // compositor, and `fork` passed them down by inheriting them.
                environ(),
            )
        } else {
            flags
        };
        libc::posix_spawnattr_destroy(&mut attr);
        rc
    };
    if rc != 0 {
        // A kernel without the `CLONE_VM|CLONE_VFORK` path, or an attr this
        // libc will not set: a slow launch beats no launch.
        return fork_detached(&c);
    }
    // The shell backgrounded the program and is already exiting; this returns
    // at once and is what keeps the launcher childless.
    let mut st = 0;
    unsafe { libc::waitpid(pid, &mut st, 0) };
}

/// This process's environment block, for `posix_spawn` to hand to the child.
fn environ() -> *const *mut libc::c_char {
    extern "C" {
        static environ: *const *mut libc::c_char;
    }
    // SAFETY: `environ` is libc's own global; read, never written.
    unsafe { environ }
}

/// The double-`fork()` launch, kept as the fallback for a `posix_spawn` that
/// will not run. `c` is already the backgrounded line, so the shell still
/// exits at once and the no-zombie property is the same.
fn fork_detached(c: &std::ffi::CStr) {
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            libc::setsid();
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
        if pid > 0 {
            let mut st = 0;
            libc::waitpid(pid, &mut st, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_goes_into_the_buffer_the_compositor_handed_back() {
        assert_eq!(free_buffer([false, false], 0), Some(0));
        assert_eq!(free_buffer([false, false], 1), Some(1));
        // The one just released, not the lowest free one: always answering 0
        // would be single buffering with two allocations.
        assert_eq!(free_buffer([true, false], 0), Some(1));
        assert_eq!(free_buffer([false, true], 1), Some(0));
        // And drawing into a buffer the compositor is still reading is a tear.
        assert_eq!(free_buffer([true, true], 0), None);
        assert_eq!(free_buffer([true, true], 1), None);
    }

    #[test]
    fn a_pool_is_sized_the_way_wl_shm_reads_it() {
        let g = pool_geometry(1920, 1080).expect("an ordinary output");
        assert_eq!(g.stride, 1920 * 4);
        assert_eq!(g.frame_size, 1920 * 4 * 1080);
        assert_eq!(g.total, g.frame_size * BUFFERS);
        // The three numbers have to agree with each other, because the frame
        // offsets are computed from `frame_size` and the pool from `total`: a
        // byte of disagreement puts a frame astride two buffers.
        for (w, h) in [(1, 1), (720, 52), (3840, 40), (2560, 1440)] {
            let g = pool_geometry(w, h).unwrap();
            assert_eq!(g.stride, w as usize * 4);
            assert_eq!(g.frame_size, g.stride * h as usize);
            assert_eq!(g.total, g.frame_size * BUFFERS);
            // And every frame's offset stays inside the pool.
            for i in 0..BUFFERS {
                assert!(i * g.frame_size + g.frame_size <= g.total);
            }
        }
        // Two frames, so one can be drawn while the compositor holds the other.
        assert_eq!(BUFFERS, 2);
        // A zero-sized surface is not an error here; the callers decide.
        assert_eq!(
            pool_geometry(0, 0),
            Some(PoolGeometry {
                total: 0,
                stride: 0,
                frame_size: 0
            })
        );
    }

    #[test]
    fn a_pool_past_the_i32_the_protocol_uses_is_refused() {
        // THE point of this function. `create_pool` takes an i32, so a pool over
        // i32::MAX arrives as a NEGATIVE number and the compositor kills the
        // client. 16384x16384 at 4 bytes is 1 GiB a frame, so two of them are
        // 2 GiB -- just past i32::MAX.
        assert_eq!(pool_geometry(16384, 16384), None);
        // The largest that does fit, and the first that does not. Every size is
        // a multiple of four bytes, so the boundary is exercised at a multiple
        // of four rather than at i32::MAX itself.
        let max = i32::MAX as usize;
        let biggest = pool_geometry(8192, (max / BUFFERS / (8192 * 4)) as u32).unwrap();
        assert!(biggest.total <= max);
        assert!(i32::try_from(biggest.total).is_ok());
        for (w, h) in [
            (1u32, 1u32),
            (7680, 4320),
            (8192, 8192),
            (1920, 1080),
            (u16::MAX as u32, 1),
        ] {
            if let Some(g) = pool_geometry(w, h) {
                assert!(g.total <= max, "{w}x{h} = {}", g.total);
                assert!(i32::try_from(g.stride).is_ok());
            }
        }
    }

    #[test]
    fn a_size_that_overflows_is_refused_and_not_wrapped_into_a_small_one() {
        // `wrapping_mul` here would be worse than useless: the product comes back
        // UNDER the i32 ceiling and the pool is accepted at a size that has
        // nothing to do with what was asked for. Both of these wrap to exactly 0
        // on a 64-bit usize, which passes every check after them.
        //
        // `2^31 x 2^31`: the stride is 2^33 and 2^33 * 2^31 is 2^64.
        assert_eq!(pool_geometry(1 << 31, 1 << 31), None);
        // The frame size times BUFFERS, for a shape whose own product fits.
        assert_eq!(pool_geometry(1 << 31, 1 << 30), None);
        assert_eq!(pool_geometry(u32::MAX, u32::MAX), None);
        assert_eq!(pool_geometry(u32::MAX, 1 << 30), None);
        // (The 4-byte stride cannot overflow from a u32 width on a 64-bit usize,
        // so that one is `checked_mul` for a 32-bit target rather than for this;
        // there is no witness for it here, and the i32 ceiling refuses any width
        // that large long before the multiply could matter.)
        assert_eq!(pool_geometry(u32::MAX, 1), None);
    }

    #[test]
    fn a_stride_past_the_i32_is_refused_even_when_the_pool_is_empty() {
        // A surface of zero height has a total of zero HOWEVER WIDE it is, so
        // every check on the total waves it through -- and `create_buffer` takes
        // the stride as an i32 as well, so the compositor gets a negative one and
        // kills the client. Reachable: a wl_output whose mode has not settled
        // reports a height of 0 while carrying a real width.
        assert_eq!(pool_geometry(u32::MAX, 0), None);
        assert_eq!(pool_geometry(1 << 30, 0), None);
        // The widest stride that does fit, and the first that does not.
        let max_w = (i32::MAX / 4) as u32;
        assert_eq!(pool_geometry(max_w, 0).unwrap().stride, max_w as usize * 4);
        assert_eq!(pool_geometry(max_w + 1, 0), None);
        // A zero WIDTH is fine at any height: the stride is zero.
        assert!(pool_geometry(0, u32::MAX).is_some());
    }

    #[test]
    fn a_pool_is_mapped_at_the_size_it_was_asked_for() {
        // memfd_create + ftruncate + mmap, for real: this is the allocation both
        // clients hand to the compositor, and a short mapping is a client that
        // writes past the end of what the compositor can see.
        let g = pool_geometry(64, 32).unwrap();
        let (map, fd) = map_shm_pool(g.total, "lunarbar-test").expect("a 16 KiB pool");
        assert!(!map.is_null());
        // Writable, and the whole range: the last byte of the second frame is
        // the one a short ftruncate would leave outside the file.
        unsafe {
            std::ptr::write_bytes(map, 0xAB, g.total);
            assert_eq!(*map.add(g.total - 1), 0xAB);
            assert_eq!(*map.add(g.frame_size), 0xAB);
        }
        use std::os::fd::AsRawFd;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) }, 0);
        assert_eq!(st.st_size as usize, g.total);
        unsafe { libc::munmap(map as *mut libc::c_void, g.total) };
    }

    #[test]
    fn what_the_client_writes_reaches_the_file_the_compositor_reads() {
        // MAP_SHARED, not MAP_PRIVATE. A private mapping accepts every write and
        // shows them back, so nothing here fails -- except that the compositor,
        // which reads the memfd and not this process's pages, would see an
        // untouched buffer. The panel would come up blank with no error anywhere.
        let (map, fd) = map_shm_pool(4096, "lunarbar-test").expect("a page");
        unsafe { std::ptr::write_bytes(map, 0x5A, 4096) };
        use std::os::fd::AsRawFd;
        let mut back = [0u8; 16];
        let n = unsafe {
            libc::pread(
                fd.as_raw_fd(),
                back.as_mut_ptr() as *mut libc::c_void,
                back.len(),
                0,
            )
        };
        assert_eq!(n, back.len() as isize);
        assert_eq!(back, [0x5A; 16], "the write never reached the memfd");
        unsafe { libc::munmap(map as *mut libc::c_void, 4096) };
    }

    #[test]
    fn a_zero_sized_pool_is_declined_rather_than_mapped() {
        // `mmap` of zero bytes fails with EINVAL, so this must come back None
        // and not a pointer the caller then writes a frame through. It is
        // reachable: a wl_output whose mode has not settled reports 0x0.
        assert!(map_shm_pool(0, "lunarbar-test").is_none());
    }

    /// `waitpid(-1)` is process-wide, so everything that forks lives in THIS one
    /// test: two tests doing it in parallel would reap each other's children and
    /// fail at random.
    #[test]
    fn a_detached_launch_leaves_no_zombie_and_does_not_wait_for_the_program() {
        let unreaped = || {
            let mut st = 0;
            let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
            (r, std::io::Error::last_os_error().raw_os_error())
        };
        assert_eq!(unreaped(), (-1, Some(libc::ECHILD)), "started with a child");
        // The launcher never accumulates children: the only child it has is the
        // shell, which backgrounds the program and exits at once, so the wait
        // returns immediately and the program is reparented to init. Without
        // that, every launch would leave a zombie for as long as the session
        // lives -- and a session launches something every time someone presses
        // Alt+Space. This is the property that survived the move from the
        // double fork to `posix_spawn`, and the reason the `&` is in
        // `backgrounded` rather than being a second clone here.
        spawn_detached("true");
        assert_eq!(unreaped(), (-1, Some(libc::ECHILD)), "a child was left");
        spawn_detached("exit 7");
        assert_eq!(
            unreaped(),
            (-1, Some(libc::ECHILD)),
            "a failure left a child"
        );
        // A command that cannot be passed to exec at all is dropped BEFORE the
        // fork: between fork and exec a child may call only async-signal-safe
        // functions, and building the CString allocates.
        spawn_detached("echo \0 hola");
        assert_eq!(unreaped(), (-1, Some(libc::ECHILD)), "an unrunnable forked");
        for _ in 0..8 {
            spawn_detached("true");
        }
        assert_eq!(
            unreaped(),
            (-1, Some(libc::ECHILD)),
            "eight launches left one"
        );

        // And DETACHED means the call returns while the program runs on. Drop
        // the `&` from `backgrounded` and the shell execs the program itself,
        // so the `waitpid` above waits for the PROGRAM: the overlay would sit
        // frozen on screen for as long as whatever was launched takes to exit.
        let t = std::time::Instant::now();
        spawn_detached("sleep 3");
        let waited = t.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(2),
            "the launch blocked for {waited:?} on a program that runs for 3s"
        );
        assert_eq!(unreaped(), (-1, Some(libc::ECHILD)), "sleep left a child");

        // The wrap has to be a line a shell will actually parse, for every
        // ending in `AWKWARD_ENDINGS`. `sh -n` parses without running, and it
        // forks -- which is why it lives in this test and not beside the
        // string assertions.
        for cmd in AWKWARD_ENDINGS {
            let line = backgrounded(cmd);
            let st = std::process::Command::new("/bin/sh")
                .args(["-n", "-c"])
                .arg(&line)
                .status()
                .expect("a shell to parse with");
            assert!(st.success(), "sh cannot parse {line:?} (from {cmd:?})");
        }
        // And the command still reaches the shell intact.
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(backgrounded("printf hola"))
            .output()
            .expect("a shell to run with");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hola");
        assert_eq!(unreaped(), (-1, Some(libc::ECHILD)), "a probe left a child");
    }

    /// Command lines whose ending is what breaks a wrap. `{ cmd ;}` is a
    /// syntax error the moment `cmd` ends in `&` or `;`, and the empty line
    /// needs the no-op colon because braces take a non-empty list. A launch
    /// that dies of this says nothing at all: the shell exits 2 and nobody is
    /// waiting for its status.
    const AWKWARD_ENDINGS: [&str; 7] = [
        "firefox",
        "foo; bar",
        "foo &",
        "foo;",
        "foo &&bar",
        "x=1 foo",
        "",
    ];

    /// The shape of the wrap, without forking: newline-separated braces, the
    /// background `&` last, and the command carried through unmangled (a wrap
    /// that mangled it would launch the wrong thing, which is worse than
    /// launching nothing).
    ///
    /// Whether a shell *accepts* these lines is checked in the one forking
    /// test below, not here -- see its note on `waitpid(-1)`.
    #[test]
    fn the_wrap_is_newline_separated_and_backgrounded() {
        for cmd in AWKWARD_ENDINGS {
            let line = backgrounded(cmd);
            assert!(line.starts_with("{\n"), "{cmd:?} -> {line:?}");
            assert!(line.ends_with("\n} &\n"), "{cmd:?} -> {line:?}");
            let body = line
                .strip_prefix("{\n")
                .and_then(|l| l.strip_suffix("\n} &\n"))
                .expect("the shape just asserted");
            // The blank line is the colon; everything else is carried verbatim.
            assert_eq!(body, if cmd.trim().is_empty() { ":" } else { cmd });
        }
    }
}
