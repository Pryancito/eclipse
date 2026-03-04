//! Funciones de I/O estándar
use crate::syscall::write;
use crate::stdlib::Spinlock;

pub const STDOUT: u32 = 1;
pub const STDERR: u32 = 2;

/// Global spinlock that serialises all stdout writes.
/// This prevents output from different threads / preempted contexts from
/// interleaving at the character level on SMP systems.
static STDOUT_LOCK: Spinlock<()> = Spinlock::new(());

pub fn puts(s: &str) {
    let _guard = STDOUT_LOCK.lock();
    write(STDOUT, s.as_bytes());
}

pub fn putchar(c: char) {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    let _guard = STDOUT_LOCK.lock();
    write(STDOUT, s.as_bytes());
}

pub struct StdoutWriter;

impl core::fmt::Write for StdoutWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write(STDOUT, s.as_bytes());
        core::result::Result::Ok(())
    }
}

/// Locked stdout writer – holds `STDOUT_LOCK` for the duration of the format
/// operation so that multi-fragment format strings are printed atomically.
pub struct LockedStdoutWriter<'a> {
    _guard: crate::stdlib::SpinlockGuard<'a, ()>,
}

impl<'a> core::fmt::Write for LockedStdoutWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write(STDOUT, s.as_bytes());
        core::result::Result::Ok(())
    }
}

/// Acquire the stdout lock and return a writer that holds it.
pub fn locked_stdout() -> LockedStdoutWriter<'static> {
    LockedStdoutWriter { _guard: STDOUT_LOCK.lock() }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut w = $crate::stdio::locked_stdout();
        let _ = write!(w, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut w = $crate::stdio::locked_stdout();
        let _ = write!(w, $($arg)*);
        let _ = write!(w, "\n");
    }};
}
