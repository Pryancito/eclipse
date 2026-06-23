//! Console input and output.

use crate::drivers;
use core::fmt::{Arguments, Result, Write};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Kernel log (dmesg) callback
// ---------------------------------------------------------------------------
// The `zcore` crate owns the actual ring buffer; it registers function
// pointers here so that `linux-syscall` can call `klog_read` / `klog_buf_size`
// without a direct crate dependency on `zcore`.

static KLOG_READ_FN: AtomicUsize = AtomicUsize::new(0);
static KLOG_SIZE_FN: AtomicUsize = AtomicUsize::new(0);
static KLOG_EMIT_FN: AtomicUsize = AtomicUsize::new(0);

/// Called once by `zcore` at startup to register the ring-buffer accessors.
pub fn klog_register(
    read_fn: fn(&mut [u8]) -> usize,
    size_fn: fn() -> usize,
    emit_fn: fn(u8, &str),
) {
    KLOG_READ_FN.store(read_fn as usize, Ordering::SeqCst);
    KLOG_SIZE_FN.store(size_fn as usize, Ordering::SeqCst);
    KLOG_EMIT_FN.store(emit_fn as usize, Ordering::SeqCst);
}

/// Copy the kernel log ring buffer into `dst`.  Returns bytes written.
/// Returns 0 if no callback has been registered yet.
pub fn klog_read(dst: &mut [u8]) -> usize {
    let p = KLOG_READ_FN.load(Ordering::SeqCst);
    if p == 0 {
        return 0;
    }
    let f: fn(&mut [u8]) -> usize = unsafe { core::mem::transmute(p) };
    f(dst)
}

/// Total bytes currently stored in the kernel log ring buffer.
pub fn klog_buf_size() -> usize {
    let p = KLOG_SIZE_FN.load(Ordering::SeqCst);
    if p == 0 {
        return 0;
    }
    let f: fn() -> usize = unsafe { core::mem::transmute(p) };
    f()
}

/// Syslog priorities (Linux `syslog.h`).
pub const LOG_ERR: u8 = 3;
pub const LOG_WARNING: u8 = 4;
pub const LOG_INFO: u8 = 6;

/// Append a vital kernel message to the dmesg ring buffer (syslog priority 0–7).
/// Always recorded regardless of the `log` crate max level.
pub fn klog_emit(priority: u8, msg: &str) {
    let p = KLOG_EMIT_FN.load(Ordering::SeqCst);
    if p == 0 {
        return;
    }
    let f: fn(u8, &str) = unsafe { core::mem::transmute(p) };
    f(priority, msg);
}

struct SerialWriter;

static SERIAL_WRITER: spin::Mutex<SerialWriter> = spin::Mutex::new(SerialWriter);

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> Result {
        if let Some(uart) = drivers::all_uart().first() {
            uart.write_str(s).unwrap();
            #[cfg(feature = "graphic")]
            if GRAPHIC_VTS.try_get().is_none() {
                crate::hal_fn::console::console_write_early(s);
            }
        } else {
            crate::hal_fn::console::console_write_early(s);
        }
        Ok(())
    }
}

struct DebugWriter;

static DEBUG_WRITER: spin::Mutex<DebugWriter> = spin::Mutex::new(DebugWriter);

impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> Result {
        crate::hal_fn::console::console_write_early(s);
        Ok(())
    }
}

cfg_if! {
    if #[cfg(feature = "graphic")] {
        use crate::utils::init_once::InitOnce;
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;
        use zcore_drivers::{scheme::DisplayScheme, utils::GraphicConsole};

        use alloc::vec::Vec;

        static GRAPHIC_VTS: InitOnce<Vec<spin::Mutex<GraphicConsole>>> = InitOnce::new();
        static CONSOLE_WIN_SIZE: InitOnce<ConsoleWinSize> = InitOnce::new();
        static GRAPHIC_DISPLAY: InitOnce<Arc<dyn DisplayScheme>> = InitOnce::new();
        static ACTIVE_VT: AtomicUsize = AtomicUsize::new(0);
        static CLEAR_ON_NEXT_GRAPHIC_WRITE: AtomicBool = AtomicBool::new(false);

        pub(crate) fn init_graphic_console(display: Arc<dyn DisplayScheme>) {
            let info = display.info();
            GRAPHIC_DISPLAY.init_once_by(display.clone());
            let mut vts = Vec::with_capacity(NUM_VTS);
            let mut winsz = ConsoleWinSize::default();
            for i in 0..NUM_VTS {
                let cons = GraphicConsole::new(display.clone());
                if i == 0 {
                    winsz = ConsoleWinSize {
                        ws_row: cons.rows() as u16,
                        ws_col: cons.columns() as u16,
                        ws_xpixel: info.width as u16,
                        ws_ypixel: info.height as u16,
                    };
                }
                vts.push(spin::Mutex::new(cons));
            }
            CONSOLE_WIN_SIZE.init_once_by(winsz);
            GRAPHIC_VTS.init_once_by(vts);
            // Make boot UX robust on real hardware: clear once on first graphic write
            // even if userspace/loader ordering differs.
            CLEAR_ON_NEXT_GRAPHIC_WRITE.store(true, Ordering::SeqCst);
        }

        fn vt_mutex(n: usize) -> Option<&'static spin::Mutex<GraphicConsole>> {
            GRAPHIC_VTS.try_get().and_then(|v| v.get(n))
        }

        /// Request a one-shot clear-to-black of the graphic console before the next write.
        pub fn request_clear_graphic_on_next_write() {
            // Finalize the boot progress indicator before switching to a cleared
            // native graphic console.
            crate::hal_fn::console::console_progress_early(100);
            CLEAR_ON_NEXT_GRAPHIC_WRITE.store(true, Ordering::SeqCst);
        }

        fn maybe_clear_graphic_before_write(vt: usize) {
            if !CLEAR_ON_NEXT_GRAPHIC_WRITE.swap(false, Ordering::SeqCst) {
                return;
            }
            if let (Some(display), Some(cons)) = (GRAPHIC_DISPLAY.try_get(), vt_mutex(vt)) {
                // Clear to black with opaque alpha (ARGB8888) and reset the console state.
                let _ = crate::boot_logo::clear_screen(
                    &**display,
                    zcore_drivers::prelude::RgbColor::new(0, 0, 0),
                );
                *cons.lock() = GraphicConsole::new(display.clone());  // spin::Mutex — IRQs stay enabled
            }
        }

        /// Write to a specific VT's console buffer. The pixels are only pushed to
        /// the display when this is the active VT and we are in text mode;
        /// background VTs keep accumulating in their own shadow buffer.
        pub(crate) fn vt_write_str_impl(vt: usize, s: &str) {
            let active = vt == ACTIVE_VT.load(Ordering::SeqCst);
            if active {
                maybe_clear_graphic_before_write(vt);
            }
            if let Some(cons) = vt_mutex(vt) {
                if let Some(mut g) = cons.try_lock() {
                    let _ = g.write_str(s);
                    if active && kd_mode_vt(vt) == KD_TEXT {
                        g.present();
                    }
                }
            }
        }

        pub(crate) fn vt_write_fmt_impl(vt: usize, fmt: Arguments) {
            let active = vt == ACTIVE_VT.load(Ordering::SeqCst);
            if active {
                maybe_clear_graphic_before_write(vt);
            }
            if let Some(cons) = vt_mutex(vt) {
                if let Some(mut g) = cons.try_lock() {
                    let _ = g.write_fmt(fmt);
                    if active && kd_mode_vt(vt) == KD_TEXT {
                        g.present();
                    }
                }
            }
        }

        /// Make VT `n` the active one and repaint it to the display.
        pub(crate) fn switch_vt_impl(n: usize) {
            if let Some(v) = GRAPHIC_VTS.try_get() {
                if n >= v.len() {
                    return;
                }
                ACTIVE_VT.store(n, Ordering::SeqCst);
                if kd_mode_vt(n) == KD_TEXT {
                    if let Some(mut g) = v[n].try_lock() {
                        g.repaint();
                    }
                }
            }
        }

        pub(crate) fn scroll_active_vt(direction: i32) {
            if let Some(cons) = vt_mutex(ACTIVE_VT.load(Ordering::SeqCst)) {
                if let Some(mut g) = cons.try_lock() {
                    g.buf_mut().scroll_history(direction);
                    g.present();
                }
            }
        }

        pub(crate) fn blink_active_vt(visible: bool) {
            if let Some(cons) = vt_mutex(ACTIVE_VT.load(Ordering::SeqCst)) {
                if let Some(mut g) = cons.try_lock() {
                    g.set_cursor_blink(visible);
                }
            }
        }

        /// Repaint the active VT from its backing buffer.
        ///
        /// Used when returning from `KD_GRAPHICS` to `KD_TEXT`: a userspace
        /// graphics server may have overwritten the framebuffer.
        pub(crate) fn redraw_graphic_console_impl() {
            if let Some(cons) = vt_mutex(ACTIVE_VT.load(Ordering::SeqCst)) {
                if let Some(mut g) = cons.try_lock() {
                    g.repaint();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// KD console mode (Linux VT `KD_SETMODE` / `KD_GETMODE` semantics)
// ---------------------------------------------------------------------------
// In `KD_GRAPHICS` the kernel stops drawing the text console so a userspace
// graphics server (X/Wayland/DRM client) can own the framebuffer. Switching
// back to `KD_TEXT` repaints the text console.

/// Text mode: the kernel owns and draws the framebuffer console.
pub const KD_TEXT: u32 = 0x00;
/// Graphics mode: userspace owns the framebuffer; the console stops drawing.
pub const KD_GRAPHICS: u32 = 0x01;

// KD mode is per-VT (like Linux): an X server putting *its* VT into
// `KD_GRAPHICS` must not stop the kernel drawing the other text consoles, so
// switching away from the graphics VT still shows a normal text terminal.
static KD_MODES: [AtomicU32; NUM_VTS] = [const { AtomicU32::new(KD_TEXT) }; NUM_VTS];

/// Set the KD mode of a specific VT (`KD_TEXT` or `KD_GRAPHICS`).
pub fn set_kd_mode_vt(vt: usize, mode: u32) {
    if let Some(m) = KD_MODES.get(vt) {
        m.store(mode, Ordering::SeqCst);
    }
    #[cfg(feature = "graphic")]
    if mode == KD_TEXT && vt == active_vt() {
        redraw_graphic_console_impl();
    }
}

/// Get the KD mode of a specific VT.
pub fn kd_mode_vt(vt: usize) -> u32 {
    KD_MODES
        .get(vt)
        .map(|m| m.load(Ordering::SeqCst))
        .unwrap_or(KD_TEXT)
}

/// Set the KD mode of the currently active VT.
pub fn set_kd_mode(mode: u32) {
    set_kd_mode_vt(active_vt(), mode);
}

/// Get the KD mode of the currently active VT.
pub fn kd_mode() -> u32 {
    kd_mode_vt(active_vt())
}

// ---------------------------------------------------------------------------
// Virtual terminals (VT) — Linux-style tty1..ttyN multiplexed on one display
// ---------------------------------------------------------------------------

/// Number of virtual terminals (Linux-style `tty1..ttyN`).
pub const NUM_VTS: usize = 6;

/// Number of virtual terminals available.
pub fn num_vts() -> usize {
    #[cfg(feature = "graphic")]
    {
        return GRAPHIC_VTS.try_get().map(|v| v.len()).unwrap_or(1);
    }
    #[cfg(not(feature = "graphic"))]
    {
        1
    }
}

/// Index of the currently active VT.
pub fn active_vt() -> usize {
    #[cfg(feature = "graphic")]
    {
        return ACTIVE_VT.load(Ordering::SeqCst);
    }
    #[cfg(not(feature = "graphic"))]
    {
        0
    }
}

/// Make VT `n` the active one and repaint it to the display.
#[allow(unused_variables)]
pub fn switch_vt(n: usize) {
    #[cfg(feature = "graphic")]
    switch_vt_impl(n);
}

/// Write a string into a specific VT's graphic console.
#[allow(unused_variables)]
pub fn vt_write_str(vt: usize, s: &str) {
    #[cfg(feature = "graphic")]
    vt_write_str_impl(vt, s);
}

/// Write formatted data into a specific VT's graphic console.
#[allow(unused_variables)]
pub fn vt_write_fmt(vt: usize, fmt: Arguments) {
    #[cfg(feature = "graphic")]
    vt_write_fmt_impl(vt, fmt);
}

/// Write a string to VT `vt`: always to its graphic console, and to the serial
/// port when `vt` is the active terminal (so the serial log mirrors the screen).
pub fn vt_console_write_str(vt: usize, s: &str) {
    if vt == active_vt() {
        serial_write_str(s);
    }
    vt_write_str(vt, s);
}

/// Blink the graphic-console text cursor.
///
/// Invoked from the timer tick (~250 Hz). It rate-limits itself to a ~2 Hz
/// blink using the monotonic clock and only does work when the blink phase
/// actually flips, so the common tick is just one atomic load. A no-op when the
/// `graphic` feature is disabled or while in `KD_GRAPHICS`.
pub fn cursor_blink_tick() {
    #[cfg(feature = "graphic")]
    {
        if kd_mode() != KD_TEXT {
            return;
        }
        static LAST_PHASE: AtomicUsize = AtomicUsize::new(usize::MAX);
        let ms = crate::hal_fn::timer::timer_now().as_millis() as usize;
        let phase = (ms / 500) & 1;
        if LAST_PHASE.swap(phase, Ordering::SeqCst) == phase {
            return;
        }
        blink_active_vt(phase == 0);
    }
}

/// Request a one-shot clear-to-black of the graphic console before the next write.
///
/// When `feature="graphic"` is disabled, this is a no-op.
#[cfg(not(feature = "graphic"))]
pub fn request_clear_graphic_on_next_write() {
    crate::hal_fn::console::console_progress_early(100);
}

/// Writes a string slice into the serial.
pub fn serial_write_str(s: &str) {
    if let Some(mut w) = SERIAL_WRITER.try_lock() {
        let _ = w.write_str(s);
    }
}

/// Writes formatted data into the serial.
pub fn serial_write_fmt(fmt: Arguments) {
    if let Some(mut w) = SERIAL_WRITER.try_lock() {
        let _ = w.write_fmt(fmt);
    }
}

/// Writes formatted data into the serial, spinning until the lock is free.
///
/// Use in panic/abort context where dropping output silently is unacceptable.
/// Caller must ensure interrupts are disabled to avoid deadlock on the same CPU.
pub fn serial_write_fmt_spin(fmt: Arguments) {
    let _ = SERIAL_WRITER.lock().write_fmt(fmt);
}

/// Writes a string slice into the serial through sbi call.
pub fn debug_write_str(s: &str) {
    if let Some(mut w) = DEBUG_WRITER.try_lock() {
        let _ = w.write_str(s);
    }
}

/// Writes formatted data into the serial through sbi call..
pub fn debug_write_fmt(fmt: Arguments) {
    if let Some(mut w) = DEBUG_WRITER.try_lock() {
        let _ = w.write_fmt(fmt);
    }
}

/// Draw a boot progress bar on the early framebuffer console (UEFI GOP), if available.
///
/// This is intended for very early boot stages before the native graphic driver exists.
pub fn early_progress_bar(progress: u32) {
    crate::hal_fn::console::console_progress_early(progress);
}

/// Write text to the very early boot framebuffer console (UEFI GOP) — the same
/// surface that renders the splash logo and progress bar — bypassing the native
/// graphic console / VT layer. Intended for boot diagnostics around the first
/// userspace output, when a `warn!` to the native console may not yet paint.
/// No-op on non-graphic / non-bare builds.
pub fn early_console_write_str(s: &str) {
    crate::hal_fn::console::console_write_early(s);
}

/// Scrolls the graphic console history up (direction > 0) or down (direction < 0).
#[allow(unused_variables)]
pub fn scroll_graphic_console(direction: i32) {
    #[cfg(feature = "graphic")]
    scroll_active_vt(direction);
}

/// Writes a string slice into the graphic console.
#[allow(unused_variables)]
pub fn graphic_console_write_str(s: &str) {
    #[cfg(feature = "graphic")]
    vt_write_str_impl(active_vt(), s);
}

/// Writes formatted data into the graphic console.
#[allow(unused_variables)]
pub fn graphic_console_write_fmt(fmt: Arguments) {
    #[cfg(feature = "graphic")]
    vt_write_fmt_impl(active_vt(), fmt);
}

/// Writes a string slice into the serial, and the graphic console if it exists.
pub fn console_write_str(s: &str) {
    serial_write_str(s);
    graphic_console_write_str(s);
}

/// Writes formatted data into the serial, and the graphic console if it exists.
pub fn console_write_fmt(fmt: Arguments) {
    serial_write_fmt(fmt);
    graphic_console_write_fmt(fmt);
}

/// Read buffer data from console (serial).
pub async fn console_read(buf: &mut [u8]) -> usize {
    super::future::SerialReadFuture::new(buf).await
}

/// The POSIX `winsize` structure.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ConsoleWinSize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// Returns the size information of the console, see [`ConsoleWinSize`].
pub fn console_win_size() -> ConsoleWinSize {
    #[cfg(feature = "graphic")]
    if let Some(&winsz) = CONSOLE_WIN_SIZE.try_get() {
        return winsz;
    }
    ConsoleWinSize::default()
}

#[macro_export]
macro_rules! klog_info {
    ($($arg:tt)*) => {
        $crate::console::klog_emit(
            $crate::console::LOG_INFO,
            &::alloc::format!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! klog_warn {
    ($($arg:tt)*) => {
        $crate::console::klog_emit(
            $crate::console::LOG_WARNING,
            &::alloc::format!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! klog_err {
    ($($arg:tt)*) => {
        $crate::console::klog_emit(
            $crate::console::LOG_ERR,
            &::alloc::format!($($arg)*),
        )
    };
}
