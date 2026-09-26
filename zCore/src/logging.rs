use core::fmt::{self, Write};
#[cfg(not(feature = "colorless-log"))]
use log::Level;
use log::{self, LevelFilter, Log, Metadata, Record};

// ---------------------------------------------------------------------------
// Kernel log ring buffer  (exposed as "dmesg")
// ---------------------------------------------------------------------------
//
// A fixed circular buffer holds all kernel log messages. Access is serialized
// with `lock::Mutex` (IRQ-safe ticket spinlock) so it is safe to call from any
// context, including interrupt handlers.
//
// Sized to hold a full X-server startup syscall trace (tens of thousands of
// lines) so the structurally interesting calls are not evicted before `dmesg`
// can read them back.
const KLOG_BUF_SIZE: usize = 8 * 1024 * 1024; // 8 MiB

/// The ring is generic over its size for one reason: at 8 MiB **no test can
/// reach the wrap-around**, and that is where a ring buffer's bugs live. The
/// kernel's own ring is `KlogBuf<KLOG_BUF_SIZE>`; the tests use a handful of
/// bytes and exercise the same code.
struct KlogBuf<const N: usize> {
    buf: [u8; N],
    head: usize, // write pointer (wraps around)
    used: usize, // bytes currently stored (≤ N)
}

impl<const N: usize> KlogBuf<N> {
    const fn new() -> Self {
        Self {
            buf: [0u8; N],
            head: 0,
            used: 0,
        }
    }

    /// Append bytes; oldest data is silently overwritten when full.
    fn write(&mut self, data: &[u8]) {
        for &b in data {
            self.buf[self.head] = b;
            self.head = (self.head + 1) % N;
            if self.used < N {
                self.used += 1;
            }
        }
    }

    /// Copy stored bytes into `dst`, oldest first, and return how many.
    ///
    /// When `dst` cannot hold everything the bytes returned are the **newest**
    /// ones, which is what `SYSLOG_ACTION_READ_ALL` promises ("if the buffer is
    /// too small to hold all messages, the most recent messages are returned")
    /// and what a reader actually wants: a `dmesg -s 8192` after a crash is
    /// asking for the end of the log, not for the first 8 KiB of boot. This
    /// used to copy from the oldest byte forward and hand back the *beginning*
    /// of an 8 MiB ring -- boot banners, with the fault nowhere in sight.
    fn read_all(&self, dst: &mut [u8]) -> usize {
        let len = self.used.min(dst.len());
        if len == 0 {
            return 0;
        }
        // `head` is one past the newest byte, and `len <= used <= N`, so
        // stepping back `len` from it cannot pass the oldest byte we hold.
        let start = (self.head + N - len) % N;
        for (i, d) in dst[..len].iter_mut().enumerate() {
            *d = self.buf[(start + i) % N];
        }
        len
    }

    fn size(&self) -> usize {
        self.used
    }
}

/// The largest `i <= at` at which `buf[..i]` does not end inside a UTF-8
/// character.
///
/// Every byte in these buffers came from a `&str`, so backing up over the
/// continuation bytes (`10xx_xxxx`) is enough, and a character is at most four
/// bytes long, so this looks at three of them at most.
fn floor_char_boundary(buf: &[u8], at: usize) -> usize {
    if at >= buf.len() {
        return buf.len();
    }
    let mut i = at;
    while i > 0 && buf[i] & 0b1100_0000 == 0b1000_0000 {
        i -= 1;
    }
    i
}

/// A fixed-size assembler for one dmesg line.
///
/// It replaces two hand-rolled `fmt::Write` impls that both cut at
/// `min(len, free)` and so got two things wrong:
///
/// * **The newline was optional.** Filling the buffer to its last byte left no
///   room for the `\n` that came last, so a long message **swallowed its own
///   terminator** and the next record was appended to it: one unparseable
///   dmesg line carrying two `<prio>[time]` stamps, and every tool that splits
///   the log on newlines reading the second record as part of the first.
///   [`Self::finish`] spends the last byte on the terminator instead of the
///   message, so a line always ends where it says it ends.
/// * **A cut could land inside a character.** These messages carry multi-byte
///   ones -- this very file logs an em dash -- and half of one in the ring is
///   a byte no reader can render.
struct LineBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
    /// Set once something did not fit. Later pieces of the same `format_args!`
    /// are then dropped too, so a truncated line is a prefix of the whole line
    /// rather than its beginning glued to its end.
    full: bool,
}

impl<'a> LineBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            full: false,
        }
    }

    /// Terminate the line and return its length, newline included.
    fn finish(self) -> usize {
        let mut pos = self.pos;
        if pos == self.buf.len() {
            // Nothing fits: the line is already truncated, so give up one more
            // character for the terminator rather than the terminator itself.
            pos = floor_char_boundary(self.buf, pos - 1);
        }
        self.buf[pos] = b'\n';
        pos + 1
    }
}

impl fmt::Write for LineBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.full {
            return Ok(());
        }
        let bytes = s.as_bytes();
        let free = self.buf.len().saturating_sub(self.pos);
        let n = if bytes.len() <= free {
            bytes.len()
        } else {
            self.full = true;
            floor_char_boundary(bytes, free)
        };
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

// The dmesg ring lock is a `lock::Mutex` (IRQ-disabling ticket lock), NOT a
// raw CAS spinlock. This is load-bearing — the previous hand-rolled AtomicBool
// lock froze the whole machine: it did not mask interrupts, and EVERY log line
// takes this lock (the ring copy in `SimpleLogger::log` and every `klog_*!`).
// If a timer IRQ landed while its CPU was inside the critical section and the
// handler itself logged anything (thermal-governor transition, an xhci/apic
// warn, frametrack), the handler re-entered the same lock ON THE SAME CPU and
// spun forever with IRQs off — no panic, no deadlock report (the raw loop was
// invisible to the spin-diagnostics), console dead mid-line, every other CPU
// wedging at its own next log line. Observed on real 16-thread hardware,
// reproducibly, ~6-7s into fork()'s CPU-pegged eager copy (right when the
// thermal governor logs its first throttle transition). `lock::Mutex` masks
// IRQs for the (short) critical section, making the re-entry impossible, and
// participates in the >8s-spin deadlock self-report.
static KLOG: lock::Mutex<KlogBuf<KLOG_BUF_SIZE>> = lock::Mutex::new(KlogBuf::new());

/// Write a slice of bytes into the kernel log ring buffer.
fn klog_write(data: &[u8]) {
    KLOG.lock().write(data);
}

/// Copy the full kernel log into `dst` (oldest first).
/// Returns the number of bytes written.
pub fn klog_read_all(dst: &mut [u8]) -> usize {
    KLOG.lock().read_all(dst)
}

/// Total bytes currently stored in the kernel log ring buffer.
pub fn klog_size() -> usize {
    KLOG.lock().size()
}

/// Write a kernel message into the dmesg ring buffer only (not echoed to the graphic/serial console).
/// `priority` follows syslog(3): 3=err, 4=warn, 6=info, 7=debug.
pub fn klog_emit(priority: u8, msg: &str) {
    let now = kernel_hal::timer::timer_now();
    let micros = now.as_micros();
    let mut line = [0u8; 512];
    // `write!`, not `writeln!`: the terminator is `LineBuf::finish`'s job, so
    // that a message which fills the buffer loses a character of itself rather
    // than the newline that separates it from the next record.
    let pos = {
        let mut w = LineBuf::new(&mut line);
        let _ = write!(
            w,
            "<{prio}>[{s:>3}.{us:06}] {msg}",
            prio = priority,
            s = micros / 1_000_000,
            us = micros % 1_000_000,
            msg = msg.trim_end_matches('\n'),
        );
        w.finish()
    };
    klog_write(&line[..pos]);
}

/// Initialize logging with the default max log level (WARN).
pub fn init() {
    static LOGGER: SimpleLogger = SimpleLogger;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(LevelFilter::Warn);
    // Register the ring-buffer accessors so linux-syscall can read them.
    kernel_hal::console::klog_register(klog_read_all, klog_size, klog_emit);
}

/// Reset max log level.
pub fn set_max_level(level: &str) {
    log::set_max_level(level.parse().unwrap_or(LevelFilter::Warn));
}

/// Run `f` with ALL `log`-crate output suppressed (level = Off), restoring the
/// previous max level afterwards. Used to bring the GPU up quietly at boot: the
/// RM / bring-up narration -- much of it deliberately at ERROR level so it is
/// visible during step debugging -- would otherwise flood the desktop console
/// with ugly, alarming-looking lines even though the bring-up is routine now.
/// Nothing is lost: the per-line detail is still captured into the
/// `/proc/gpustep*` buffers. Note `klog_emit` (and thus `klog_info!`) writes
/// straight to the ring buffer and is NOT gated by the level, so a clean
/// summary line emitted via `klog_info!` still prints while this is in effect.
// Used by the Linux userboot path; the zircon build never calls it.
#[allow(dead_code)]
pub fn with_output_suppressed<F: FnOnce()>(f: F) {
    let prev = log::max_level();
    log::set_max_level(LevelFilter::Off);
    f();
    log::set_max_level(prev);
}

#[macro_export]
macro_rules! klog_info {
    ($($arg:tt)*) => {
        $crate::logging::klog_emit(6, &::alloc::format!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_warn {
    ($($arg:tt)*) => {
        $crate::logging::klog_emit(4, &::alloc::format!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_err {
    ($($arg:tt)*) => {
        $crate::logging::klog_emit(3, &::alloc::format!($($arg)*))
    };
}

#[inline]
pub fn print(args: fmt::Arguments) {
    kernel_hal::console::console_write_fmt(args);
}

#[allow(dead_code)]
#[inline]
pub fn debug_print(args: fmt::Arguments) {
    kernel_hal::console::debug_write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::logging::print(core::format_args!($($arg)*));
    }
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\r\n"));
    ($($arg:tt)*) => {
        $crate::logging::print(core::format_args!($($arg)*));
        $crate::print!("\r\n");
    }
}

#[macro_export]
macro_rules! debug_print {
    ($($arg:tt)*) => {
        $crate::logging::debug_print(core::format_args!($($arg)*));
    }
}

#[macro_export]
macro_rules! debug_println {
    () => ($crate::print!("\r\n"));
    ($($arg:tt)*) => {
        $crate::logging::debug_print(core::format_args!($($arg)*));
        $crate::debug_print!("\r\n");
    }
}

#[allow(dead_code)]
#[repr(u8)]
enum ColorCode {
    Black = 30,
    Red = 31,
    Green = 32,
    Yellow = 33,
    Blue = 34,
    Magenta = 35,
    Cyan = 36,
    White = 37,
    BrightBlack = 90,
    BrightRed = 91,
    BrightGreen = 92,
    BrightYellow = 93,
    BrightBlue = 94,
    BrightMagenta = 95,
    BrightCyan = 96,
    BrightWhite = 97,
}

struct SimpleLogger;

impl Log for SimpleLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // After a heap/stack smash, a log Record's args can point at freed or
        // scribbled memory; formatting them is what re-faulted in a STORM right
        // here — a READ #PF inside `SimpleLogger::log` on a mangled arg pointer
        // (`rip` symbolizes to this function) — that buried the crash and could
        // keep the machine from surviving long enough to catch the writer. Once
        // a smash is suspected, stop touching Record args entirely. The direct
        // serial diagnostics (`[null-exec]`, `[watchpoint]`, `oops`) do NOT
        // route through `log::`, so they still print in full.
        if executor::heap_smash_suspected() {
            use core::sync::atomic::{AtomicBool, Ordering};
            static NOTED: AtomicBool = AtomicBool::new(false);
            if !NOTED.swap(true, Ordering::Relaxed) {
                kernel_hal::console::serial_write_str(
                    "[logging] heap smash suspected — suppressing log:: formatting \
                     from here (args may be corrupt); direct-serial diagnostics \
                     still print\r\n",
                );
            }
            return;
        }
        let now = kernel_hal::timer::timer_now();
        let cpu_id = kernel_hal::cpu::cpu_id();
        let (tid, pid) = (0, 0); //kernel_hal::thread::get_tid();
        let level = record.level();
        let target = record.target();
        #[cfg(not(feature = "colorless-log"))]
        let level_color = match level {
            Level::Error => ColorCode::BrightRed,
            Level::Warn => ColorCode::BrightYellow,
            Level::Info => ColorCode::BrightGreen,
            Level::Debug => ColorCode::BrightCyan,
            Level::Trace => ColorCode::BrightBlack,
        };
        #[cfg(not(feature = "colorless-log"))]
        let args_color = match level {
            Level::Error => ColorCode::Red,
            Level::Warn => ColorCode::Yellow,
            Level::Info => ColorCode::Green,
            Level::Debug => ColorCode::Cyan,
            Level::Trace => ColorCode::BrightBlack,
        };
        let time = {
            cfg_if! {
                if #[cfg(feature = "libos")] {
                    use chrono::{Local, TimeZone};
                    alloc::format!("{}", Local.timestamp_nanos(now.as_nanos() as _).format("%Y-%m-%d %H:%M:%S%.6f"))
                } else {
                    let micros = now.as_micros();
                    alloc::format!("{s:>3}.{us:06}", s = micros / 1_000_000, us = micros % 1_000_000)
                }
            }
        };
        #[cfg(feature = "colorless-log")]
        print(format_args!(
            "[{time} {level:<5} {cpu_id} {pid}:{tid} {target}] {}\n",
            record.args()
        ));
        #[cfg(not(feature = "colorless-log"))]
        print(format_args!(
            "\u{1b}[{}m[{time} \u{1b}[{}m{level:<5}\u{1b}[m \u{1b}[{}m{cpu_id} {pid}:{tid} {target}]\u{1b}[m \u{1b}[{}m{}\u{1b}[m\n",
            ColorCode::White as u8,
            level_color as u8,
            ColorCode::White as u8,
            args_color as u8,
            record.args(),
        ));

        // Also write a plain-text copy into the ring buffer for dmesg.
        {
            let mut line = [0u8; 1024];
            let mut w = LineBuf::new(&mut line);
            let micros = now.as_micros();
            let syslog_prio = match level {
                Level::Error => 3u8,
                Level::Warn => 4,
                Level::Info => 6,
                Level::Debug => 7,
                Level::Trace => 7,
            };
            let _ = core::fmt::write(
                &mut w,
                format_args!(
                    "<{prio}>[{s:>3}.{us:06}] {args}",
                    prio = syslog_prio,
                    s = micros / 1_000_000,
                    us = micros % 1_000_000,
                    args = record.args(),
                ),
            );
            let pos = w.finish();
            klog_write(&line[..pos]);
        }

        // When running with `LOG=debug` (or more verbose) we still don't have a native GPU
        // driver early in boot. Mirror logs to the UEFI GOP framebuffer console so we can
        // see early boot progress on real hardware.
        //
        // IMPORTANT: The early framebuffer console can't interpret ANSI escapes, so
        // keep this output plain (no colors).
        #[cfg(feature = "graphic")]
        if log::max_level() >= LevelFilter::Debug {
            cfg_if! {
                if #[cfg(feature = "libos")] {
                    use chrono::{TimeZone, Local};
                    kernel_hal::console::debug_write_fmt(format_args!(
                        "[{time} {level:<5} {cpu_id} {pid}:{tid} {target}] {args}\n",
                        time = Local.timestamp_nanos(now.as_nanos() as _).format("%Y-%m-%d %H:%M:%S%.6f"),
                        level = level,
                        cpu_id = cpu_id,
                        pid = pid,
                        tid = tid,
                        target = target,
                        args = record.args(),
                    ));
                } else {
                    let micros = now.as_micros();
                    let s = micros / 1_000_000;
                    let us = micros % 1_000_000;
                    kernel_hal::console::debug_write_fmt(format_args!(
                        "[{s:>3}.{us:06} {level:<5} {cpu_id} {pid}:{tid} {target}] {args}\n",
                        s = s,
                        us = us,
                        level = level,
                        cpu_id = cpu_id,
                        pid = pid,
                        tid = tid,
                        target = target,
                        args = record.args(),
                    ));
                }
            }
        }
    }

    fn flush(&self) {}
}

/// The dmesg ring and the line assembler that feeds it.
///
/// Everything here is reachable only because the ring is generic over its size:
/// the kernel's is 8 MiB, and no test can write eight megabytes to reach the
/// wrap-around, which is why the two bugs below survived. They run on the host
/// `libos` build, where `zCore` compiles as a test target at all.
#[cfg(test)]
mod klog_tests {
    use super::*;

    /// Read the whole ring back, exactly as `dmesg` does: ask the size, then
    /// ask for that much.
    fn dump<const N: usize>(ring: &KlogBuf<N>) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec![0u8; ring.size()];
        let n = ring.read_all(&mut out);
        out.truncate(n);
        out
    }

    fn line(buf: &mut [u8], msg: &str) -> usize {
        let mut w = LineBuf::new(buf);
        let _ = write!(w, "{msg}");
        w.finish()
    }

    // --- the ring ---------------------------------------------------------

    #[test]
    fn an_empty_ring_reads_back_as_nothing() {
        let ring = KlogBuf::<8>::new();
        assert_eq!(ring.size(), 0);
        let mut dst = [0xAAu8; 4];
        assert_eq!(ring.read_all(&mut dst), 0);
        assert_eq!(dst, [0xAA; 4], "read_all must not touch dst when empty");
    }

    #[test]
    fn what_fits_reads_back_unchanged() {
        let mut ring = KlogBuf::<8>::new();
        ring.write(b"abcde");
        assert_eq!(ring.size(), 5);
        assert_eq!(dump(&ring), b"abcde");
    }

    #[test]
    fn the_oldest_bytes_are_the_ones_overwritten() {
        let mut ring = KlogBuf::<8>::new();
        ring.write(b"0123456789ab");
        assert_eq!(ring.size(), 8, "the ring never reports more than it holds");
        assert_eq!(dump(&ring), b"456789ab");
    }

    #[test]
    fn a_write_longer_than_the_whole_ring_leaves_only_its_tail() {
        let mut ring = KlogBuf::<4>::new();
        ring.write(b"the quick brown fox");
        assert_eq!(dump(&ring), b" fox");
    }

    /// The bug: with a `dst` too small, this used to copy from the OLDEST byte
    /// and hand back the beginning of the ring -- for an 8 MiB kernel log,
    /// boot banners, with whatever you were looking for evicted long ago.
    /// `SYSLOG_ACTION_READ_ALL` promises the newest messages, and the newest
    /// is what anyone reading a short buffer is after.
    #[test]
    fn a_short_read_gets_the_newest_bytes_not_the_oldest() {
        let mut ring = KlogBuf::<8>::new();
        ring.write(b"12345678");
        let mut dst = [0u8; 3];
        assert_eq!(ring.read_all(&mut dst), 3);
        assert_eq!(&dst, b"678");
    }

    /// Same question with the write pointer parked mid-buffer, which is where
    /// an off-by-one in the backwards step would show.
    #[test]
    fn a_short_read_of_a_wrapped_ring_gets_the_newest_bytes() {
        let mut ring = KlogBuf::<8>::new();
        ring.write(b"0123456789"); // head is now at 2
        for take in 1..=8 {
            let mut dst = alloc::vec![0u8; take];
            assert_eq!(ring.read_all(&mut dst), take);
            let whole = b"23456789";
            assert_eq!(
                &dst[..],
                &whole[whole.len() - take..],
                "taking {take} of 8 must be the last {take}"
            );
        }
    }

    #[test]
    fn a_short_read_of_a_ring_that_never_wrapped_still_gets_the_newest() {
        let mut ring = KlogBuf::<8>::new();
        ring.write(b"abcd");
        let mut dst = [0u8; 2];
        assert_eq!(ring.read_all(&mut dst), 2);
        assert_eq!(&dst, b"cd");
    }

    #[test]
    fn a_dst_larger_than_the_log_is_filled_only_as_far_as_the_log_goes() {
        let mut ring = KlogBuf::<8>::new();
        ring.write(b"hi");
        let mut dst = [0xAAu8; 6];
        assert_eq!(ring.read_all(&mut dst), 2);
        assert_eq!(&dst[..2], b"hi");
        assert_eq!(
            &dst[2..],
            &[0xAA; 4],
            "the tail of dst is not ours to write"
        );
    }

    // --- the line assembler ----------------------------------------------

    #[test]
    fn a_line_that_fits_is_the_message_plus_one_newline() {
        let mut buf = [0u8; 32];
        let n = line(&mut buf, "hello");
        assert_eq!(&buf[..n], b"hello\n");
    }

    /// The bug: `write_str` filled the buffer to its last byte, so the `\n`
    /// that came last was dropped and the NEXT record was appended to this
    /// one -- a single dmesg line carrying two `<prio>[time]` stamps, which
    /// nothing splitting the log on newlines can read apart.
    #[test]
    fn an_overlong_line_still_ends_in_a_newline() {
        let mut buf = [0u8; 8];
        let n = line(&mut buf, "0123456789abcdef");
        assert_eq!(
            buf[n - 1],
            b'\n',
            "a truncated line must still terminate: {:?}",
            core::str::from_utf8(&buf[..n])
        );
        assert_eq!(&buf[..n], b"0123456\n");
    }

    /// Exactly on the boundary, where "it fit" and "it did not" meet.
    #[test]
    fn a_line_that_fills_the_buffer_exactly_spends_its_last_byte_on_the_newline() {
        let mut buf = [0u8; 8];
        let n = line(&mut buf, "01234567");
        assert_eq!(&buf[..n], b"0123456\n");
        let mut buf = [0u8; 8];
        let n = line(&mut buf, "0123456");
        assert_eq!(&buf[..n], b"0123456\n");
    }

    /// The bug: cutting at `min(len, free)` splits a multi-byte character, and
    /// these lines carry them -- this very module logs an em dash. Half a
    /// character in the ring is a byte no reader can render.
    #[test]
    fn a_truncated_line_is_still_valid_utf8() {
        // "ab" + em dash (3 bytes) = 5 bytes, and 4 bytes of room for the
        // message: the dash does not fit, and neither does any part of it.
        let mut buf = [0u8; 5];
        let n = line(&mut buf, "ab\u{2014}cd");
        let out = core::str::from_utf8(&buf[..n]).expect("the ring must hold valid UTF-8");
        assert_eq!(out, "ab\n");
    }

    #[test]
    fn a_multibyte_character_that_does_fit_is_kept_whole() {
        let mut buf = [0u8; 8];
        let n = line(&mut buf, "a\u{2014}b");
        assert_eq!(
            core::str::from_utf8(&buf[..n]).unwrap(),
            "a\u{2014}b\n",
            "5 bytes of message and a newline fit in 8"
        );
    }

    /// Truncation is sticky, so a cut line is a PREFIX of the whole line. It
    /// used to keep accepting later pieces of the same `format_args!`, which
    /// glued the head of a message to the tail of its format string.
    #[test]
    fn a_cut_line_does_not_pick_up_the_pieces_that_come_after_it() {
        // Three bytes of room and a first piece that ends in a character
        // needing three of its own: backing up to the boundary leaves two
        // bytes free, which the piece after it would fit into. That is what
        // makes this the case that tells "stopped" from "kept going" -- a
        // buffer filled to its last byte cannot, since nothing fits either way.
        //
        // The two pieces are written by hand rather than through
        // `write!(w, "{}{}", a, b)`, because `format_args!` folds adjacent
        // string LITERALS into one piece: that macro call reaches `write_str`
        // ONCE, with `a` and `b` already concatenated, and so cannot see this
        // at all. It is the shape of an `{args}` whose `Display` writes in
        // several goes, which is what every log line is.
        let mut buf = [0u8; 3];
        let mut w = LineBuf::new(&mut buf);
        w.write_str("a\u{2014}b").unwrap();
        w.write_str("XY").unwrap();
        let n = w.finish();
        let out = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(out, "a\n");
        assert!(!out.contains('X'), "a dropped middle must drop the end too");
    }

    /// The same property through a real `format_args!`, whose pieces a
    /// `Display` impl hands over one at a time.
    #[test]
    fn a_cut_format_does_not_pick_up_the_arguments_that_come_after_it() {
        struct InThreeGoes;
        impl core::fmt::Display for InThreeGoes {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a\u{2014}")?;
                f.write_str("b")?;
                f.write_str("XY")
            }
        }
        let mut buf = [0u8; 3];
        let mut w = LineBuf::new(&mut buf);
        let _ = write!(w, "{InThreeGoes}");
        let n = w.finish();
        assert_eq!(core::str::from_utf8(&buf[..n]).unwrap(), "a\n");
    }

    #[test]
    fn a_buffer_with_room_for_nothing_but_a_newline_holds_a_newline() {
        let mut buf = [0u8; 1];
        let n = line(&mut buf, "whatever");
        assert_eq!(&buf[..n], b"\n");
    }

    // --- the two together -------------------------------------------------

    /// What the whole thing is for: several records go in, and each comes back
    /// as its own line. This is the assertion the missing newline broke.
    #[test]
    fn every_record_comes_back_as_its_own_line() {
        let mut ring = KlogBuf::<512>::new();
        for msg in ["first", "0123456789abcdefghij", "last"] {
            let mut buf = [0u8; 12];
            let n = line(&mut buf, msg);
            ring.write(&buf[..n]);
        }
        let out = dump(&ring);
        let text = core::str::from_utf8(&out).unwrap();
        assert_eq!(
            text.lines().collect::<alloc::vec::Vec<_>>(),
            // The middle one is 20 bytes into a 12-byte buffer: 11 of message
            // and the terminator, which is the point -- it does not swallow
            // "last".
            alloc::vec!["first", "0123456789a", "last"],
        );
    }
}
