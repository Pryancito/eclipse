//! The driver's `log` records and its klog lines, on stderr, written by a
//! thread of their own.
//!
//! In the kernel they reach the kernel's log; in this process nothing took
//! them, so every line of the driver and of the RM that goes through `log`
//! (the console GPU's bring-up among them) was lost. eclipse-init keeps the
//! service's stderr in `/tmp/unvidia.log`.
//!
//! A line is not written where it is made. The driver logs inside stretches
//! that must not be stretched: between its last interrupt de-assert and the
//! `STARTCPU` store of the console GPU's SEC2 boot it makes a record
//! (`nvidia-rm-sys/src/os_boundary.rs`: "with NOTHING between them", a log
//! line there being what re-stalls the store). To the kernel that record is
//! a compare, its logger drops `Info`; here it was a `write(2)`: a system
//! call, the file's locks and a place to be descheduled at. So a line costs
//! here what one costs the kernel's ring: its text, copied into memory that
//! is already there. A thread writes what has gathered every few
//! milliseconds, and the process what is left when it ends.

use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use log::{LevelFilter, Log, Metadata, Record};

struct StderrLog;

impl Log for StderrLog {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            put(format_args!(
                "[{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {
        write_out();
    }
}

/// A klog line of the driver (`drivers_klog_emit`), at its priority.
pub fn klog(priority: u8, text: &str) {
    put(format_args!("<{priority}> {}", text.trim_end()));
}

/// The lines made and not yet written, and how many found no room.
struct Pending {
    text: Vec<u8>,
    lost: usize,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    text: Vec::new(),
    lost: 0,
});

/// One at a time takes what is pending and writes it, so the lines reach
/// the file in the order they were made, and the process's end waits for a
/// write the thread is in the middle of.
static WRITING: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The room for lines between two writes. Made once ([`install`]) and never
/// grown where a line is made: growing it is a call to the allocator, which
/// may be one to the kernel. A burst that fills it loses its last lines, and
/// says how many.
const ROOM: usize = 512 * 1024;

/// Between two writes: what a line waits for the file at most, and what a
/// process killed outright loses of its last lines.
const EVERY: Duration = Duration::from_millis(5);

fn pending() -> MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// `text`, refusing what it has no room for instead of growing.
struct Room<'a>(&'a mut Vec<u8>);

impl fmt::Write for Room<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.0.capacity() - self.0.len() < s.len() {
            return Err(fmt::Error);
        }
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// One line: who wrote it, when on the kernel's clock (`dmesg`'s), what.
fn put(what: fmt::Arguments) {
    let at = uptime();
    let mut pending = pending();
    if pending.text.capacity() == 0 {
        // No room was made, or the process is ending and took it: written
        // here and now.
        drop(pending);
        eprintln!("{} {} {what}", crate::NAME, Stamp(at));
        return;
    }
    if !line_into(&mut pending.text, at, what) {
        pending.lost += 1;
    }
}

/// Add the line to `text` if it has the room for all of it. Whether it had:
/// a line that does not fit leaves nothing of itself.
fn line_into(text: &mut Vec<u8>, at: Duration, what: fmt::Arguments) -> bool {
    let before = text.len();
    let whole = writeln!(Room(text), "{} {} {what}", crate::NAME, Stamp(at)).is_ok();
    if !whole {
        text.truncate(before);
    }
    whole
}

/// Seconds and milliseconds, as `dmesg` shows its own.
struct Stamp(Duration);

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:>4}.{:03}", self.0.as_secs(), self.0.subsec_millis())
    }
}

/// The time since boot: `CLOCK_MONOTONIC`, which the kernel's log stamps
/// its lines with, read through the vDSO as the driver reads its own clock.
fn uptime() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a clock the kernel has and a timespec of this function's.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

/// Write what is pending. The buffer written goes back as the room for the
/// next lines, so neither side ever allocates.
fn write_out() {
    let mut spare = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let lost = {
        let mut pending = pending();
        if pending.text.is_empty() && pending.lost == 0 {
            return;
        }
        std::mem::swap(&mut pending.text, &mut *spare);
        std::mem::take(&mut pending.lost)
    };
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(&spare);
    said_lost(&mut err, lost);
    spare.clear();
}

/// The process is ending: write what is pending, and have whatever is still
/// logged after this written where it is made.
///
/// Never waits for a lock: the thread that ends the process may be one that
/// holds it (a panic while a line was being made). It looks for a while, for
/// the write the other thread is in the middle of, and then lets go: lines
/// lost are better than a process that never ends.
fn write_all_and_stop() {
    use std::sync::TryLockError::{Poisoned, WouldBlock};
    for _ in 0..200 {
        let spare = match WRITING.try_lock() {
            Ok(spare) => Some(spare),
            Err(Poisoned(e)) => Some(e.into_inner()),
            Err(WouldBlock) => None,
        };
        let pending = match PENDING.try_lock() {
            Ok(pending) => Some(pending),
            Err(Poisoned(e)) => Some(e.into_inner()),
            Err(WouldBlock) => None,
        };
        let (Some(mut spare), Some(mut pending)) = (spare, pending) else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        let text = std::mem::take(&mut pending.text);
        let lost = std::mem::take(&mut pending.lost);
        drop(pending);
        *spare = Vec::new();
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(&text);
        said_lost(&mut err, lost);
        return;
    }
}

fn said_lost(err: &mut impl std::io::Write, lost: usize) {
    if lost > 0 {
        let _ = writeln!(
            err,
            "{}: {lost} line(s) of the driver's log found no room and are lost",
            crate::NAME
        );
    }
}

extern "C" fn at_exit() {
    write_all_and_stop();
}

static LOGGER: StderrLog = StderrLog;

/// Take the driver's records from here on, `Info` and louder (the RM turns
/// the level down and back up itself around its noisy stretches), and its
/// klog lines.
pub fn install() {
    if log::set_logger(&LOGGER).is_err() {
        return;
    }
    log::set_max_level(LevelFilter::Info);
    pending().text.reserve_exact(ROOM);
    WRITING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reserve_exact(ROOM);
    // What is pending when the process ends is written by whoever ends it:
    // `exit` (and a `main` that returns), and a panic before it says why.
    // SAFETY: a function of this program's, which takes no argument.
    unsafe { libc::atexit(at_exit) };
    let said = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |why| {
        write_all_and_stop();
        said(why);
    }));
    std::thread::spawn(|| loop {
        std::thread::sleep(EVERY);
        write_out();
        keep_the_log_of_a_wedge();
    });
}

/// `udrv-nvidia log-check`: a record and a klog line, then `exit`, and
/// both must be on stderr, in that order, written by the thread or by the
/// exit (no driver runs where there is no NVIDIA GPU, so nothing else logs).
pub fn check() -> ! {
    log::info!("log-check: a record");
    klog(6, "log-check: a klog line");
    std::process::exit(0)
}

/// The driver turns its log off for good when the console GPU stops
/// answering (`console_quiet_latch`): in the kernel the log is drawn on
/// that GPU's framebuffer, and the next line would take the machine with
/// it. Here the log is a file, and what the driver does after a wedge (its
/// recovery through the bridge above the GPU, or how it gave up) is what a
/// boot with a black monitor has to say. So it is turned on again, once.
fn keep_the_log_of_a_wedge() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static KEPT: AtomicBool = AtomicBool::new(false);
    if log::max_level() == LevelFilter::Off
        && nvidia_rm_sys::os_interface::console_quiet_latched()
        && !KEPT.swap(true, Ordering::Relaxed)
    {
        log::set_max_level(LevelFilter::Info);
        klog(
            4,
            "the driver turned its log off for a console GPU that stopped answering; \
             kept on here, where the log is a file",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_one_line_with_its_time_its_level_and_its_target() {
        let mut text = Vec::with_capacity(256);
        let made = line_into(
            &mut text,
            Duration::from_millis(12_345),
            format_args!(
                "[{}] {}: {}",
                log::Level::Warn,
                "zcore_drivers::display::nvidia",
                format_args!("step9 load_status={}", 0x40)
            ),
        );
        assert!(made);
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "udrv-nvidia   12.345 [WARN] zcore_drivers::display::nvidia: step9 load_status=64\n"
        );
    }

    #[test]
    fn a_line_with_no_room_leaves_nothing_of_itself_and_nothing_grows() {
        let mut text = Vec::with_capacity(64);
        assert!(line_into(&mut text, Duration::ZERO, format_args!("first")));
        let (before, capacity) = (text.clone(), text.capacity());
        // More than is left, in pieces: the first of them fit.
        let long = "x".repeat(40);
        assert!(!line_into(
            &mut text,
            Duration::ZERO,
            format_args!("{} {}", "second", long)
        ));
        assert_eq!(text, before);
        assert_eq!(text.capacity(), capacity);
        // And the room is still there for a line that fits.
        assert!(line_into(&mut text, Duration::ZERO, format_args!("third")));
    }

    #[test]
    fn a_stamp_reads_as_the_kernels_log_does() {
        assert_eq!(Stamp(Duration::from_millis(7_042)).to_string(), "   7.042");
        assert_eq!(
            Stamp(Duration::from_millis(1_234_567)).to_string(),
            "1234.567"
        );
    }

    #[test]
    fn the_clock_is_the_one_since_boot_and_goes_forward() {
        let a = uptime();
        let b = uptime();
        assert!(a > Duration::ZERO && b >= a);
    }
}
