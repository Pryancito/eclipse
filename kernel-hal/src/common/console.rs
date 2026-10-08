//! Console input and output.

use crate::common::panic_lock::{ConsoleLock, PANIC_SPIN_BUDGET};
use crate::drivers;
use core::fmt::{Arguments, Result, Write};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Kernel log (dmesg) callback
// ---------------------------------------------------------------------------
// The `zcore` crate owns the actual ring buffer; it registers function
// pointers here so that `linux-syscall` can call `klog_read` / `klog_buf_size`
// without a direct crate dependency on `zcore`.

/// The word to jump to for one klog slot, or `None` when the slot must not be
/// called: [`lock::fn_slot::live_fn`], through one name.
///
/// Seamed for the same reason `kaddr::publish_fn_slot_window` is: **no build
/// this suite compiles publishes a `.text` window**, and that is deliberate --
/// a window that holds no host function turns every live hook into a refused
/// one, which is what `drivers`' dmesg tests rely on not happening. With the
/// window read inside, every host call comes back `Unchecked`, so a slot
/// judged against nothing looks exactly like a slot that was never judged: the
/// three refusals below are unreachable and dropping the judgement altogether
/// -- jumping to whatever word is in the slot, which is the smash this guard
/// exists for -- passes the whole suite. The hook is thread-local, so a test
/// that arms it cannot reach another test's.
#[cfg(not(test))]
#[inline(always)]
fn klog_slot(slot: usize) -> Option<usize> {
    lock::fn_slot::live_fn(slot)
}

#[cfg(test)]
fn klog_slot(slot: usize) -> Option<usize> {
    match KLOG_SLOT_HOOK.with(|c| c.get()) {
        Some(f) => f(slot),
        None => lock::fn_slot::live_fn(slot),
    }
}

#[cfg(test)]
std::thread_local! {
    static KLOG_SLOT_HOOK: core::cell::Cell<Option<fn(usize) -> Option<usize>>> =
        const { core::cell::Cell::new(None) };
}

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
    // Judged before the jump: see `lock::fn_slot`. `dmesg` is read from
    // userspace, so this slot is reachable on demand by an unprivileged
    // process -- the last one to call on trust.
    let Some(p) = klog_slot(KLOG_READ_FN.load(Ordering::SeqCst)) else {
        return 0;
    };
    let f: fn(&mut [u8]) -> usize = unsafe { core::mem::transmute(p) };
    f(dst)
}

/// Total bytes currently stored in the kernel log ring buffer.
pub fn klog_buf_size() -> usize {
    let Some(p) = klog_slot(KLOG_SIZE_FN.load(Ordering::SeqCst)) else {
        return 0;
    };
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
    let Some(p) = klog_slot(KLOG_EMIT_FN.load(Ordering::SeqCst)) else {
        return;
    };
    let f: fn(u8, &str) = unsafe { core::mem::transmute(p) };
    f(priority, msg);
}

struct SerialWriter;

/// Not a `spin::Mutex`: the panic path has to be able to get out of this one.
/// See [`crate::panic_lock`] — `SerialWriter::write_str` below ends in an
/// `unwrap` inside the critical section, so a panic there used to ask this very
/// CPU to hand back a lock it was still holding, with interrupts off.
static SERIAL_LOCK: ConsoleLock = ConsoleLock::new();

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

static DEBUG_LOCK: ConsoleLock = ConsoleLock::new();

impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> Result {
        #[cfg(all(feature = "qemu-debug-console", not(feature = "libos")))]
        crate::imp::debug_console::write(s);
        #[cfg(not(all(feature = "qemu-debug-console", not(feature = "libos"))))]
        crate::hal_fn::console::console_write_early(s);
        Ok(())
    }
}

cfg_if! {
    if #[cfg(feature = "graphic")] {
        use crate::utils::init_once::InitOnce;
        use alloc::sync::Arc;
        use zcore_drivers::{scheme::DisplayScheme, utils::GraphicConsole};

        use alloc::vec::Vec;

        static GRAPHIC_VTS: InitOnce<Vec<spin::Mutex<Option<GraphicConsole>>>> = InitOnce::new();
        static CONSOLE_WIN_SIZE: InitOnce<ConsoleWinSize> = InitOnce::new();
        static GRAPHIC_DISPLAY: InitOnce<Arc<dyn DisplayScheme>> = InitOnce::new();
        static ACTIVE_VT: AtomicUsize = AtomicUsize::new(0);
        static CLEAR_ON_NEXT_GRAPHIC_WRITE: AtomicBool = AtomicBool::new(false);

        /// Pixels above 4K: extra VTs must not clone the shadow. VirtualBox
        /// EFI GOP can hand us 8K (~126 MiB/VT); three of those exhaust the
        /// 512 MiB heap. VT 0 still comes up so boot logs reach the screen.
        const MAX_CONSOLE_PIXELS: usize = 3840 * 2160;

        fn console_pixels(display: &dyn DisplayScheme) -> usize {
            let info = display.info();
            (info.width as usize).saturating_mul(info.height as usize)
        }

        pub(crate) fn init_graphic_console(display: Arc<dyn DisplayScheme>) {
            let info = display.info();
            GRAPHIC_DISPLAY.init_once_by(display.clone());
            serial_write_fmt_spin(format_args!(
                "[graphic] GOP {}x{} (~{} MiB shadow/VT)\n",
                info.width,
                info.height,
                (console_pixels(&*display).saturating_mul(4)) >> 20,
            ));
            // Eager VT 0 only. Each GraphicConsole shadows the whole GOP FB
            // (`width×height×4`); allocating all 7 at boot OOMs a 512 MiB
            // heap when the firmware hands us a huge mode (VirtualBox 8K).
            // The other VTs are created on first write / switch, and skipped
            // entirely when the mode is larger than 4K.
            let cons0 = GraphicConsole::new(display.clone());
            let winsz = ConsoleWinSize {
                ws_row: cons0.rows() as u16,
                ws_col: cons0.columns() as u16,
                ws_xpixel: info.width as u16,
                ws_ypixel: info.height as u16,
            };
            let mut vts = Vec::with_capacity(NUM_VTS);
            vts.push(spin::Mutex::new(Some(cons0)));
            for _ in 1..NUM_VTS {
                vts.push(spin::Mutex::new(None));
            }
            CONSOLE_WIN_SIZE.init_once_by(winsz);
            GRAPHIC_VTS.init_once_by(vts);
            // Make boot UX robust on real hardware: clear once on first graphic write
            // even if userspace/loader ordering differs.
            CLEAR_ON_NEXT_GRAPHIC_WRITE.store(true, Ordering::SeqCst);
        }

        fn vt_mutex(n: usize) -> Option<&'static spin::Mutex<Option<GraphicConsole>>> {
            GRAPHIC_VTS.try_get().and_then(|v| v.get(n))
        }

        fn instantiate_vt(slot: &mut Option<GraphicConsole>) -> Option<&mut GraphicConsole> {
            if slot.is_none() {
                let display = GRAPHIC_DISPLAY.try_get()?;
                if console_pixels(&**display) > MAX_CONSOLE_PIXELS {
                    return None;
                }
                *slot = Some(GraphicConsole::new(display.clone()));
            }
            slot.as_mut()
        }

        /// Try-lock VT `n`, creating its GraphicConsole on first use.
        fn with_vt_try<R>(n: usize, f: impl FnOnce(&mut GraphicConsole) -> R) -> Option<R> {
            let cons = vt_mutex(n)?;
            let mut g = cons.try_lock()?;
            let inner = instantiate_vt(&mut g)?;
            Some(f(inner))
        }

        /// Timer/IRQ path: never allocate a console from interrupt context.
        fn with_vt_existing<R>(n: usize, f: impl FnOnce(&mut GraphicConsole) -> R) -> Option<R> {
            let cons = vt_mutex(n)?;
            let mut g = cons.try_lock()?;
            Some(f(g.as_mut()?))
        }

        /// Present coalescing for write-driven console output.
        ///
        /// A scroll redraws the whole shadow buffer, so presenting after every
        /// `write` turned line-by-line output (`cat`, build logs — stdio splits
        /// its buffer at each `\n`) into one full-screen blit per line. Writes
        /// inside the window only latch [`PRESENT_PENDING`]; the 250 Hz timer
        /// tick flushes the tail via [`flush_pending_present`], so a burst
        /// coalesces to at most ~60 presents/s and the final line still reaches
        /// the screen within one tick (≤4 ms). An isolated write (interactive
        /// echo) is past the window and presents immediately, so key-echo
        /// latency is unchanged.
        const PRESENT_MIN_INTERVAL_NS: u64 = 16_000_000; // ~60 Hz
        // Qualified (not imported at the top): this block is graphic-gated,
        // and the bare import broke `deny(warnings)` in non-graphic builds.
        static LAST_PRESENT_NS: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0);
        static PRESENT_PENDING: AtomicBool = AtomicBool::new(false);

        fn present_throttled(g: &mut GraphicConsole) {
            let now = crate::hal_fn::timer::timer_now().as_nanos() as u64;
            let last = LAST_PRESENT_NS.load(Ordering::Relaxed);
            if now.wrapping_sub(last) >= PRESENT_MIN_INTERVAL_NS {
                LAST_PRESENT_NS.store(now, Ordering::Relaxed);
                PRESENT_PENDING.store(false, Ordering::Release);
                g.present();
            } else {
                PRESENT_PENDING.store(true, Ordering::Release);
            }
        }

        /// Timer-tick side of the coalescer: push a deferred present once the
        /// throttle window has elapsed. `try_lock` only — this runs in IRQ
        /// context and a busy console simply retries next tick.
        pub(crate) fn flush_pending_present() {
            if !PRESENT_PENDING.load(Ordering::Acquire) {
                return;
            }
            let vt = ACTIVE_VT.load(Ordering::SeqCst);
            if !present_allowed(vt) {
                // Userspace took the framebuffer while a present was pending;
                // drop it (KD_TEXT re-entry repaints the whole VT anyway).
                PRESENT_PENDING.store(false, Ordering::Release);
                return;
            }
            let now = crate::hal_fn::timer::timer_now().as_nanos() as u64;
            let last = LAST_PRESENT_NS.load(Ordering::Relaxed);
            if now.wrapping_sub(last) < PRESENT_MIN_INTERVAL_NS {
                return;
            }
            let _ = with_vt_existing(vt, |g| {
                LAST_PRESENT_NS.store(now, Ordering::Relaxed);
                PRESENT_PENDING.store(false, Ordering::Release);
                g.present();
            });
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
                // try_lock, NOT lock: this was the console path's only BLOCKING
                // uninstrumented spin::Mutex acquisition, reachable from any
                // logging context (incl. IRQ) — a wedged holder turned it into
                // an invisible infinite spin. On contention, re-arm the flag so
                // the clear happens on the next write instead.
                if let Some(mut g) = cons.try_lock() {
                    // Clear to black with opaque alpha (ARGB8888) and reset the console state.
                    let _ = crate::boot_logo::clear_screen(
                        &**display,
                        zcore_drivers::prelude::RgbColor::new(0, 0, 0),
                    );
                    // Drop the old shadow before allocating the replacement.
                    // At huge GOP modes the heap has no room for both.
                    *g = None;
                    *g = Some(GraphicConsole::new(display.clone()));
                } else {
                    CLEAR_ON_NEXT_GRAPHIC_WRITE.store(true, Ordering::SeqCst);
                }
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
            let _ = with_vt_try(vt, |g| {
                let _ = g.write_str(s);
                if active && present_allowed(vt) {
                    present_throttled(g);
                }
            });
        }

        pub(crate) fn vt_write_fmt_impl(vt: usize, fmt: Arguments) {
            let active = vt == ACTIVE_VT.load(Ordering::SeqCst);
            if active {
                maybe_clear_graphic_before_write(vt);
            }
            let _ = with_vt_try(vt, |g| {
                let _ = g.write_fmt(fmt);
                if active && present_allowed(vt) {
                    present_throttled(g);
                }
            });
        }

        /// Make VT `n` the active one and repaint it to the display.
        pub(crate) fn switch_vt_impl(n: usize) {
            if let Some(v) = GRAPHIC_VTS.try_get() {
                if n >= v.len() {
                    return;
                }
                let prev = ACTIVE_VT.swap(n, Ordering::SeqCst);
                // klog (survives LOG=error): ground truth for EVERY active-VT
                // change, including the internal ones that bypass the VT ioctl
                // path (set_kd_mode's KD_TEXT-on-graphics-VT revert). A desktop
                // that lands on the wrong VT shows up here as an unexpected
                // `-> 0` right after the switch to the graphics VT.
                if prev != n {
                    crate::klog_info!("[vt] switch_vt_impl {} -> {}", prev, n);
                }
                if kd_mode_vt(n) == KD_TEXT {
                    let _ = with_vt_try(n, |g| {
                        g.repaint();
                    });
                }
            }
        }

        pub(crate) fn scroll_active_vt(direction: i32) {
            let vt = ACTIVE_VT.load(Ordering::SeqCst);
            // Userspace owns the framebuffer in KD_GRAPHICS: scrolling the
            // shadow buffer is fine, presenting it is not -- that paints the
            // text console over the compositor. Every other present in this
            // file is gated this way; this one was the hole.
            if !present_allowed(vt) {
                return;
            }
            let _ = with_vt_try(vt, |g| {
                g.buf_mut().scroll_history(direction);
                g.present();
            });
        }

        pub(crate) fn blink_active_vt(visible: bool) {
            let vt = ACTIVE_VT.load(Ordering::SeqCst);
            // labwc/KD_GRAPHICS owns the FB: never call into DisplayScheme from
            // the timer path (present_with_cursor → dyn blit/flush). A half-
            // torn-down or compositor-owned display was a null-vtable EXECUTE
            // vector with `in_timer_callback` previously unset.
            if !present_allowed(vt) {
                return;
            }
            let _ = with_vt_existing(vt, |g| {
                g.set_cursor_blink(visible);
            });
        }

        /// Repaint the active VT from its backing buffer.
        ///
        /// Used when returning from `KD_GRAPHICS` to `KD_TEXT`: a userspace
        /// graphics server may have overwritten the framebuffer.
        pub(crate) fn redraw_graphic_console_impl() {
            let _ = with_vt_try(ACTIVE_VT.load(Ordering::SeqCst), |g| {
                g.repaint();
            });
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
/// Obsolete alias of [`KD_TEXT`]. Linux's `vt_ioctl.c` still accepts it (and
/// [`KD_TEXT1`]) as a request for text mode, and old X servers and VT tools
/// still send it.
pub const KD_TEXT0: u32 = 0x02;
/// Obsolete alias of [`KD_TEXT`]; see [`KD_TEXT0`].
pub const KD_TEXT1: u32 = 0x03;

/// The canonical mode a `KD_SETMODE` argument asks for, or `None` when it asks
/// for nothing Linux would accept (`vt_ioctl.c`'s `default: ret = -EINVAL`).
///
/// This has to be decided in ONE place, and it has to be decided before the
/// value is stored, because everything downstream compares against
/// [`KD_TEXT`]: [`present_allowed`] reads anything else as "userspace owns the
/// framebuffer" and stops pushing the text console to the display. So a mode
/// that is neither constant -- `KD_TEXT0` from an old X server, or any stray
/// value, since nothing rejected them -- blanked the console and left it
/// blank: [`set_kd_mode_vt`]'s own arms match on the two constants too, so
/// neither the repaint nor the fall back to the primary text VT ran either.
/// Same shape as `linux-object`'s `vt_mode_accepted` for `VT_SETMODE`, which
/// had the identical hole.
pub fn normalize_kd_mode(mode: u32) -> Option<u32> {
    match mode {
        KD_TEXT | KD_TEXT0 | KD_TEXT1 => Some(KD_TEXT),
        KD_GRAPHICS => Some(KD_GRAPHICS),
        _ => None,
    }
}

// KD mode is per-VT (like Linux): an X server putting *its* VT into
// `KD_GRAPHICS` must not stop the kernel drawing the other text consoles, so
// switching away from the graphics VT still shows a normal text terminal.
static KD_MODES: [AtomicU32; NUM_VTS] = [const { AtomicU32::new(KD_TEXT) }; NUM_VTS];

/// Diagnostic: when true, kernel text-console writes are PRESENTED to the
/// display even while a userspace compositor holds the VT in `KD_GRAPHICS`.
/// Normally KD_GRAPHICS suppresses presentation so labwc owns the screen, but
/// that also hides a hard hang's last kernel log line on a monitor-only box.
/// With this on, the console log stays visible over the compositor, so the last
/// message before a freeze is frozen on screen.
static DIAG_PRESENT_OVER_GRAPHICS: AtomicBool = AtomicBool::new(false);

/// Enable/disable presenting kernel console output over a KD_GRAPHICS VT.
pub fn set_diag_present_over_graphics(on: bool) {
    DIAG_PRESENT_OVER_GRAPHICS.store(on, Ordering::Relaxed);
}

/// Whether a write to VT `vt` should be pushed to the display now: always in
/// text mode, and in graphics mode only when the diagnostic override is on.
#[inline]
#[allow(dead_code)]
fn present_allowed(vt: usize) -> bool {
    kd_mode_vt(vt) == KD_TEXT || DIAG_PRESENT_OVER_GRAPHICS.load(Ordering::Relaxed)
}

/// Set the KD mode of a specific VT (`KD_TEXT` or `KD_GRAPHICS`; the obsolete
/// text aliases are accepted and stored as `KD_TEXT`). An unrecognised mode is
/// ignored -- see [`normalize_kd_mode`] -- so the stored mode is always one of
/// the two the rest of this module compares against.
pub fn set_kd_mode_vt(vt: usize, mode: u32) {
    let Some(mode) = normalize_kd_mode(mode) else {
        return;
    };
    if let Some(m) = KD_MODES.get(vt) {
        m.store(mode, Ordering::SeqCst);
    }
    #[cfg(feature = "graphic")]
    {
        if mode == KD_GRAPHICS {
            // A compositor/X server just claimed VT `vt` for graphics. Make the
            // display FOLLOW it: switch the active VT to `vt` so launching a
            // desktop lands the user on the graphics VT (tty7) with no manual VT
            // switch, and the kernel text console stops drawing over the
            // compositor's framebuffer. `switch_vt_impl` does NOT repaint text on
            // a KD_GRAPHICS target, so this never scribbles on the compositor.
            if vt != active_vt() {
                switch_vt_impl(vt);
            }
        } else if mode == KD_TEXT && vt == active_vt() {
            // This VT is returning to text. If it is the graphics VT releasing
            // the display (the compositor exited and restored KD_TEXT), fall
            // back to the primary text console so the user is not stranded on a
            // now-blank graphics VT; otherwise just repaint this VT.
            if vt == GRAPHICS_VT {
                switch_vt_impl(0);
            } else {
                redraw_graphic_console_impl();
            }
        }
    }
}

/// Repaint the active VT from its cell buffer when it is a text console (a
/// no-op without the graphic console). For a presenter that finds, after its
/// blit, that the active VT changed under it: its trailing bands landed on
/// the text console and must be painted over again.
pub fn redraw_active_console() {
    #[cfg(feature = "graphic")]
    redraw_graphic_console_impl();
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
///
/// The LAST VT ([`GRAPHICS_VT`], `tty7` at the default of 7) is reserved for
/// the graphical session (labwc/X11/whatever): no login shell is spawned on it
/// (see `zCore/src/main.rs`), and a compositor putting it into `KD_GRAPHICS`
/// makes the kernel switch the display to it automatically (see `set_kd_mode_vt`).
/// Keeping graphics on its own VT stops the kernel text console and the
/// compositor fighting over the framebuffer. If this changes, keep the
/// `tty7` the userspace launcher targets (`eclipse-init` / the labwc wrapper)
/// in sync with `GRAPHICS_VT + 1`.
pub const NUM_VTS: usize = 7;

/// Index of the VT dedicated to the graphical session (`tty7` = index 6). The
/// last VT, so bumping `NUM_VTS` moves it and its reservation together.
pub const GRAPHICS_VT: usize = NUM_VTS - 1;

/// Number of virtual terminals available.
pub fn num_vts() -> usize {
    #[cfg(feature = "graphic")]
    {
        GRAPHIC_VTS.try_get().map(|v| v.len()).unwrap_or(1)
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
        ACTIVE_VT.load(Ordering::SeqCst)
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

/// The VT the serial line is bound to for a text shell.
///
/// Normally the active VT, so the serial log mirrors what is on screen. But
/// NEVER the graphics VT: once a compositor puts the reserved graphics VT
/// (`tty7`) into `KD_GRAPHICS` and the display follows it, that VT carries the
/// desktop's framebuffer, not a text shell -- binding serial to it silences the
/// line entirely (UART input would go to the compositor, and the only output
/// mirrored would be the graphics VT's, which never writes text). Fall back to
/// `tty1` (index 0), which backs `/dev/console`'s shell, so the serial UART
/// stays a usable text console while the desktop runs on screen. In pure text
/// mode this is just `active_vt()`, so serial still follows VT switches there.
pub fn serial_vt() -> usize {
    serial_vt_for(active_vt())
}

/// [`serial_vt`]'s rule, with the active VT as an argument rather than read
/// inside.
///
/// The reserved graphics VT (tty7) is the ONLY VT with no login shell (see
/// `zCore/src/main.rs`). Whenever it is foreground -- the desktop is on screen
/// -- serial binds to tty1's shell instead, independent of KD-mode timing
/// during compositor start/teardown. Any other (text) VT is the active
/// terminal itself, so serial still follows VT switches there.
///
/// It is a free function because the rule used to live inside a
/// `cfg(feature = "graphic")` block, and **no configuration this suite
/// compiles has that feature**: without it `active_vt()` is the constant 0, so
/// the whole rule folded away and deleting it passed every test. Here it is
/// compiled and reachable in every build; without `graphic` the argument is
/// still the constant 0, which is not `GRAPHICS_VT`, so it folds to the same 0
/// the old arm returned.
fn serial_vt_for(active: usize) -> usize {
    if active == GRAPHICS_VT {
        0
    } else {
        active
    }
}

/// Write a string to VT `vt`: always to its graphic console, and to the serial
/// port when `vt` is the serial-bound terminal (see [`serial_vt`]) -- the active
/// VT in text mode, or `tty1` while the desktop holds the graphics VT.
pub fn vt_console_write_str(vt: usize, s: &str) {
    if vt == serial_vt() {
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
        // No graphic consoles yet: do not touch DisplayScheme from IRQ/timer
        // context.
        if GRAPHIC_VTS.try_get().is_none() {
            return;
        }
        // Push any present deferred by the write-path coalescer (it checks
        // `present_allowed` itself, and drops the pending flag when userspace
        // owns the framebuffer).
        flush_pending_present();
        if !present_allowed(active_vt()) {
            return;
        }
        static LAST_PHASE: AtomicUsize = AtomicUsize::new(usize::MAX);
        let ms = crate::hal_fn::timer::timer_now().as_millis() as usize;
        let phase = blink_phase(ms);
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

/// Half of the cursor's blink period, in milliseconds: the cursor is shown for
/// this long, then hidden for this long, so the blink itself is ~1 Hz.
const BLINK_HALF_PERIOD_MS: usize = 500;

/// Which half of the blink cycle the monotonic clock is in at `ms`: 0 while the
/// cursor is shown, 1 while it is hidden. [`cursor_blink_tick`] runs at ~250 Hz
/// and only does work when this flips, so it is also what keeps the common tick
/// down to one atomic load.
///
/// A free function for the same reason as [`serial_vt_for`]: it lived inside a
/// `cfg(feature = "graphic")` block that no build this suite compiles, so the
/// rate was whatever the literal said and nothing could disagree. A cursor
/// blinking at the wrong rate is not a failure anyone reports; it is just a
/// console that looks slightly wrong.
#[allow(dead_code)]
fn blink_phase(ms: usize) -> usize {
    (ms / BLINK_HALF_PERIOD_MS) & 1
}

/// This CPU, as the console locks name it.
fn me() -> u32 {
    crate::cpu::cpu_id() as u32
}

/// Writes a string slice into the serial.
pub fn serial_write_str(s: &str) {
    if let Some(how) = SERIAL_LOCK.try_acquire(me()) {
        let _ = SerialWriter.write_str(s);
        SERIAL_LOCK.release(how);
    }
}

/// Writes formatted data into the serial.
pub fn serial_write_fmt(fmt: Arguments) {
    if let Some(how) = SERIAL_LOCK.try_acquire(me()) {
        let _ = SerialWriter.write_fmt(fmt);
        SERIAL_LOCK.release(how);
    }
}

/// Writes formatted data into the serial, waiting for the lock rather than
/// dropping the output.
///
/// Use in panic/abort context, where a silently dropped report is the real
/// failure. Unlike a plain `lock()`, this always returns: a nested write from
/// the CPU that already holds the lock goes straight through, and a holder that
/// never comes back has the lock taken away after [`PANIC_SPIN_BUDGET`] spins.
/// Interleaved output is a bad report; no output is not a report.
///
/// Caller should still have interrupts disabled, so an IRQ cannot interleave
/// mid-line on this CPU.
pub fn serial_write_fmt_spin(fmt: Arguments) {
    let how = SERIAL_LOCK.acquire_for_panic(me(), PANIC_SPIN_BUDGET, core::hint::spin_loop);
    let _ = SerialWriter.write_fmt(fmt);
    SERIAL_LOCK.release(how);
}

/// How many times a panic write has had to take the serial lock away from a
/// holder that never gave it back. Non-zero means a CPU died mid-print.
pub fn serial_lock_steals() -> u32 {
    SERIAL_LOCK.steals()
}

/// Writes a string slice into the serial through sbi call.
pub fn debug_write_str(s: &str) {
    if let Some(how) = DEBUG_LOCK.try_acquire(me()) {
        let _ = DebugWriter.write_str(s);
        DEBUG_LOCK.release(how);
    }
}

/// Writes formatted data into the serial through sbi call..
pub fn debug_write_fmt(fmt: Arguments) {
    if let Some(how) = DEBUG_LOCK.try_acquire(me()) {
        let _ = DebugWriter.write_fmt(fmt);
        DEBUG_LOCK.release(how);
    }
}

/// Draw a boot progress bar on the early framebuffer console (UEFI GOP), if available.
///
/// This is intended for very early boot stages before the native graphic driver exists.
pub fn early_progress_bar(progress: u32) {
    // Timestamp it too: the same marks that draw the bar are the kernel's boot
    // timeline (see `boot_marks`). One relaxed store per mark, fewer than
    // twenty in a boot.
    crate::boot_marks::mark(progress);
    crate::hal_fn::console::console_progress_early(progress);
}

/// Prime GOP geometry from the bootloader before [`crate::KCONFIG`] exists.
pub fn early_fb_prime(fb_vaddr: usize, width: usize, height: usize, stride_pixels: usize) {
    crate::hal_fn::console::console_progress_prime(fb_vaddr, width, height, stride_pixels);
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

struct EmergencyGraphicWriter {
    buf: [u8; 512],
    len: usize,
}

/// Where a flushed panic line goes.
///
/// Seamed because the whole job of this writer is getting bytes OUT, and on
/// every target the host suite can build the real sink is an empty function
/// (`hal_fn`'s libos stub): dropping the buffer instead of flushing it leaves
/// the same empty buffer behind and was invisible. A panic report is written
/// once, by a machine that is already dying, so a lost line is a lost
/// diagnosis. Thread-local, so a test that reads it cannot see another's.
#[cfg(not(test))]
#[inline(always)]
fn panic_write_str(s: &str) {
    crate::hal_fn::console::console_panic_write_str(s);
}

#[cfg(test)]
fn panic_write_str(s: &str) {
    PANIC_WRITES.with(|c| c.borrow_mut().push_str(s));
}

#[cfg(test)]
std::thread_local! {
    static PANIC_WRITES: core::cell::RefCell<alloc::string::String> =
        const { core::cell::RefCell::new(alloc::string::String::new()) };
}

impl EmergencyGraphicWriter {
    fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        if let Ok(s) = core::str::from_utf8(&self.buf[..self.len]) {
            panic_write_str(s);
        }
        self.len = 0;
    }
}

impl Write for EmergencyGraphicWriter {
    fn write_str(&mut self, s: &str) -> Result {
        for ch in s.chars() {
            self.write_char(ch)?;
        }
        Ok(())
    }

    fn write_char(&mut self, ch: char) -> Result {
        let mut tmp = [0u8; 4];
        let enc = ch.encode_utf8(&mut tmp).as_bytes();
        if self.buf.len() - self.len < enc.len() {
            self.flush();
        }
        self.buf[self.len..self.len + enc.len()].copy_from_slice(enc);
        self.len += enc.len();
        Ok(())
    }
}

/// Panic/fault-path graphic write: bypass the VT console and append straight to
/// the early framebuffer text renderer, so the report never dispatches through
/// the `DisplayScheme` trait object that may already be corrupted.
#[allow(unused_variables)]
pub fn graphic_console_write_fmt_spin(fmt: Arguments) {
    #[cfg(feature = "graphic")]
    {
        let mut w = EmergencyGraphicWriter {
            buf: [0; 512],
            len: 0,
        };
        let _ = w.write_fmt(fmt);
        w.flush();
    }
}

/// Tell the graphic console that a panic is in progress, so it stops trusting
/// its own cell cache: no resizing, no repainting, every glyph drawn straight
/// to the framebuffer. See `zcore_drivers::utils::note_panicking`.
///
/// Call this FIRST in the panic handler — before any console output — because
/// what it protects against is the console dying while reporting the fault
/// that corrupted it.
pub fn note_panicking() {
    #[cfg(feature = "graphic")]
    zcore_drivers::utils::note_panicking();
}

/// Absolute last-resort panic output: rasterize `s` onto a red band at the top
/// of the framebuffer with raw pixel writes — no locks, no RefCell, no
/// allocation. Works even when every console lock is wedged (e.g. a panic
/// inside an IRQ handler while another CPU holds the VT/serial locks). No-op on
/// targets with no direct framebuffer.
pub fn panic_banner(s: &str) {
    crate::hal_fn::console::console_panic_banner(s);
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

/// A userspace-supplied window size (via `TIOCSWINSZ`) that overrides the
/// framebuffer-derived one. The framebuffer console is huge (e.g. 227x113 at a
/// 2048x2048 mode), which is right for the on-screen graphic console but wrong
/// for a *serial* viewer whose terminal window is much smaller: full-screen
/// apps (nano, less, top) then lay out for 227x113 and overflow, wrapping their
/// status/help lines into garbage. A serial login can now run `resize`/`stty`
/// (or the /etc/profile helper) to report its real size, and it sticks here.
static CONSOLE_WIN_SIZE_OVERRIDE: spin::Mutex<Option<ConsoleWinSize>> = spin::Mutex::new(None);

/// Record a caller-supplied console window size (`TIOCSWINSZ`). A row/col of 0
/// clears the override and falls back to the framebuffer-derived size.
pub fn set_console_win_size(ws: ConsoleWinSize) {
    let mut ov = CONSOLE_WIN_SIZE_OVERRIDE.lock();
    if ws.ws_row == 0 && ws.ws_col == 0 {
        *ov = None;
    } else {
        *ov = Some(ws);
    }
}

/// Returns the size information of the console, see [`ConsoleWinSize`].
pub fn console_win_size() -> ConsoleWinSize {
    if let Some(ws) = *CONSOLE_WIN_SIZE_OVERRIDE.lock() {
        return ws;
    }
    #[cfg(feature = "graphic")]
    if let Some(&winsz) = CONSOLE_WIN_SIZE.try_get() {
        return winsz;
    }
    // Sensible serial default when no graphic console and no `TIOCSWINSZ`
    // override yet. Returning 0×0 makes ncurses/busybox assume 80×24 anyway,
    // but an explicit size keeps `stty size` and apps consistent.
    ConsoleWinSize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
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

#[cfg(test)]
mod kd_mode_tests {
    extern crate std;

    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// The KD mode table and the diagnostic flag are process-wide statics, so
    /// every test here takes the same lock. (`--test-threads=1`, which CI uses,
    /// hides that; the default parallel run does not.)
    fn test_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Leave the globals as a fresh boot has them.
    fn reset() {
        for vt in 0..NUM_VTS {
            set_kd_mode_vt(vt, KD_TEXT);
        }
        set_diag_present_over_graphics(false);
    }

    /// Linux's `vt_ioctl.c` folds `KD_TEXT0` and `KD_TEXT1` into `KD_TEXT` and
    /// answers anything else with `-EINVAL`. Storing an unrecognised value
    /// instead was not a cosmetic divergence: everything downstream tests
    /// `== KD_TEXT`, so the console stopped being pushed to the display and
    /// `set_kd_mode_vt`'s own arms matched neither mode, so neither the repaint
    /// nor the fall back to the primary text VT ran. The screen went blank and
    /// stayed blank, and the process that asked got `Ok`.
    #[test]
    fn a_kd_mode_is_stored_only_in_its_canonical_form() {
        let _g = test_lock();
        reset();

        for alias in [KD_TEXT, KD_TEXT0, KD_TEXT1] {
            assert_eq!(
                normalize_kd_mode(alias),
                Some(KD_TEXT),
                "{:#x} is a request for text mode",
                alias
            );
        }
        assert_eq!(normalize_kd_mode(KD_GRAPHICS), Some(KD_GRAPHICS));
        for bogus in [4u32, 0x10, 0xffff_ffff] {
            assert_eq!(normalize_kd_mode(bogus), None, "{:#x} is not a mode", bogus);
        }

        // And the store never lets a non-canonical value through, whichever
        // caller it came from.
        set_kd_mode_vt(0, KD_TEXT1);
        assert_eq!(
            kd_mode_vt(0),
            KD_TEXT,
            "an obsolete text alias must read back as text, so KD_GETMODE \
             answers what Linux answers and the console keeps drawing"
        );
        reset();
    }

    /// An unrecognised mode must leave the VT exactly as it was. It used to
    /// overwrite it, and since the new value was neither constant the VT was
    /// then in no mode at all -- unrecoverable except by setting a real mode
    /// again, which a process that thought it had succeeded will not do.
    #[test]
    fn an_unrecognised_mode_leaves_the_vt_untouched() {
        let _g = test_lock();
        reset();

        set_kd_mode_vt(GRAPHICS_VT, KD_GRAPHICS);
        set_kd_mode_vt(GRAPHICS_VT, 0x2a);
        assert_eq!(
            kd_mode_vt(GRAPHICS_VT),
            KD_GRAPHICS,
            "a rejected mode must not disturb the mode the VT is in"
        );

        set_kd_mode_vt(0, KD_TEXT);
        set_kd_mode_vt(0, 0x2a);
        assert_eq!(kd_mode_vt(0), KD_TEXT);
        reset();
    }

    /// KD mode is per-VT, like Linux: a compositor putting tty7 into graphics
    /// must not stop the kernel drawing the other text consoles, or switching
    /// away from the graphics VT lands on a blank terminal.
    #[test]
    fn graphics_mode_on_one_vt_does_not_silence_the_others() {
        let _g = test_lock();
        reset();

        set_kd_mode_vt(GRAPHICS_VT, KD_GRAPHICS);
        assert!(!present_allowed(GRAPHICS_VT));
        for vt in 0..GRAPHICS_VT {
            assert!(
                present_allowed(vt),
                "text console {} must keep being presented",
                vt
            );
        }
        reset();
    }

    /// A VT index outside the table reads as text rather than as "graphics",
    /// so a stray index can never be the reason the console stops drawing.
    #[test]
    fn a_vt_outside_the_table_reads_as_text() {
        let _g = test_lock();
        reset();

        assert_eq!(kd_mode_vt(NUM_VTS), KD_TEXT);
        assert_eq!(kd_mode_vt(usize::MAX), KD_TEXT);
        // And setting one is ignored rather than aliasing onto a real VT.
        set_kd_mode_vt(NUM_VTS, KD_GRAPHICS);
        for vt in 0..NUM_VTS {
            assert_eq!(kd_mode_vt(vt), KD_TEXT, "VT {} was written through", vt);
        }
        reset();
    }

    /// The diagnostic that keeps the kernel log on screen over a compositor.
    /// It is what a monitor-only box has to read the last line before a freeze,
    /// and until `console.overgraphics` was wired in `zCore/src/main.rs` its
    /// setter had no caller at all, so it could not be turned on.
    #[test]
    fn the_diagnostic_presents_over_a_graphics_vt() {
        let _g = test_lock();
        reset();

        set_kd_mode_vt(GRAPHICS_VT, KD_GRAPHICS);
        assert!(
            !present_allowed(GRAPHICS_VT),
            "off by default: the compositor owns the screen"
        );

        set_diag_present_over_graphics(true);
        assert!(
            present_allowed(GRAPHICS_VT),
            "with the diagnostic on, the kernel console reaches the display \
             even while the VT is held in graphics mode"
        );

        set_diag_present_over_graphics(false);
        assert!(!present_allowed(GRAPHICS_VT));
        reset();
    }

    /// The graphics VT is the last one, and `zCore/src/main.rs` spawns no login
    /// shell on it while the userspace launcher targets `tty7`. Bumping
    /// `NUM_VTS` has to move both together, and this is the assertion that
    /// notices if only one of them moves.
    #[test]
    fn the_graphics_vt_is_the_last_one() {
        assert_eq!(GRAPHICS_VT, NUM_VTS - 1);
        assert_eq!(GRAPHICS_VT + 1, 7, "the launcher targets tty7");
        assert!(KD_MODES.len() >= NUM_VTS, "one mode slot per VT");
    }
}

/// The kernel log (`dmesg`) hooks: three function-pointer slots another crate
/// fills in, and the judgement each one gets before it is jumped to.
#[cfg(test)]
mod klog_tests {
    extern crate std;

    use super::*;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use std::sync::{Mutex, MutexGuard};

    /// The three slots are process-wide, and `drivers`' dmesg tests install
    /// their own recording sink in them. Every test here takes this lock and
    /// puts back what it found. (`--test-threads=1`, which CI uses, hides
    /// that; the default parallel run does not.)
    fn test_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What the three slots held on the way in. Restored on the way out so a
    /// sink another module installed survives this test.
    struct SavedSlots(usize, usize, usize);

    impl SavedSlots {
        fn take() -> Self {
            Self(
                KLOG_READ_FN.load(Ordering::SeqCst),
                KLOG_SIZE_FN.load(Ordering::SeqCst),
                KLOG_EMIT_FN.load(Ordering::SeqCst),
            )
        }
    }

    impl Drop for SavedSlots {
        fn drop(&mut self) {
            KLOG_READ_FN.store(self.0, Ordering::SeqCst);
            KLOG_SIZE_FN.store(self.1, Ordering::SeqCst);
            KLOG_EMIT_FN.store(self.2, Ordering::SeqCst);
            KLOG_SLOT_HOOK.with(|c| c.set(None));
            EMITTED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    /// What the stub ring buffer holds. Its length is the answer `klog_read`
    /// and `klog_buf_size` must give, and the two are told apart by content.
    const STORED: &[u8] = b"lo que el kernel dijo";

    static EMITTED: Mutex<Vec<(u8, String)>> = Mutex::new(Vec::new());

    fn stub_read(dst: &mut [u8]) -> usize {
        let n = dst.len().min(STORED.len());
        dst[..n].copy_from_slice(&STORED[..n]);
        n
    }

    fn stub_size() -> usize {
        STORED.len()
    }

    fn stub_emit(priority: u8, msg: &str) {
        EMITTED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((priority, msg.to_string()));
    }

    fn emitted() -> Vec<(u8, String)> {
        EMITTED.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A judgement that refuses everything, whatever is in the slot.
    fn refuse_everything(_slot: usize) -> Option<usize> {
        None
    }

    /// `dmesg` is the one of these three a process drives, so the buffer it
    /// hands in is the one that has to come back filled -- and the count it
    /// gets back is how many bytes it then reads out of it.
    #[test]
    fn what_dmesg_reads_is_what_the_registered_ring_buffer_holds() {
        let _g = test_lock();
        let _saved = SavedSlots::take();

        klog_register(stub_read, stub_size, stub_emit);

        let mut buf = [0xabu8; 64];
        assert_eq!(
            klog_read(&mut buf),
            STORED.len(),
            "el contador es el de bytes copiados"
        );
        assert_eq!(
            &buf[..STORED.len()],
            STORED,
            "y los bytes son los del buffer de verdad, no los de otro hueco"
        );
        assert!(
            buf[STORED.len()..].iter().all(|&b| b == 0xab),
            "nada mas alla de lo copiado se toca"
        );
        assert_eq!(
            klog_buf_size(),
            STORED.len(),
            "el tamano sale de SU hueco, no del de la lectura"
        );
    }

    /// The normal state of all three slots on every boot until `zcore`
    /// installs them, and the state they are back in if the judgement below
    /// ever refuses one. Nothing is called, and -- the part that matters --
    /// the caller is told nothing was.
    ///
    /// A `dmesg` that answered "I filled your 64 bytes" without filling them
    /// hands userspace whatever was on its stack, and a size that answered
    /// `usize::MAX` is a read loop that never ends.
    #[test]
    fn a_log_hook_nobody_installed_is_not_called_and_says_so() {
        let _g = test_lock();
        let _saved = SavedSlots::take();

        KLOG_READ_FN.store(0, Ordering::SeqCst);
        KLOG_SIZE_FN.store(0, Ordering::SeqCst);
        KLOG_EMIT_FN.store(0, Ordering::SeqCst);

        let mut buf = [0xabu8; 64];
        assert_eq!(klog_read(&mut buf), 0, "no se ha copiado nada");
        assert!(buf.iter().all(|&b| b == 0xab), "y no se ha tocado nada");
        assert_eq!(klog_buf_size(), 0);

        klog_emit(LOG_ERR, "esto no llega a ninguna parte");
        assert!(emitted().is_empty());
    }

    /// The guard this module exists for. A word is not a function just because
    /// a function was stored there: the soft smash these slots are watched for
    /// leaves a stack pointer or a half-zeroed `.text` address behind, and
    /// jumping to it is the end of the machine.
    ///
    /// The judgement is [`lock::fn_slot::live_fn`], and what it answers depends
    /// on a `.text` window that **no build this suite compiles publishes** --
    /// by design, since a window holding no host function would refuse every
    /// live hook. So with the window read inside, the only answer reachable
    /// from here is "allowed", and dropping the judgement altogether is
    /// invisible. [`klog_slot`] is the seam that makes the other answer
    /// reachable.
    #[test]
    fn a_word_the_judge_refuses_is_not_jumped_to_either() {
        let _g = test_lock();
        let _saved = SavedSlots::take();

        klog_register(stub_read, stub_size, stub_emit);
        KLOG_SLOT_HOOK.with(|c| c.set(Some(refuse_everything)));

        let mut buf = [0xabu8; 64];
        assert_eq!(klog_read(&mut buf), 0, "un hueco refusado no se llama");
        assert!(buf.iter().all(|&b| b == 0xab));
        assert_eq!(klog_buf_size(), 0);
        klog_emit(LOG_ERR, "un hueco refusado no emite");
        assert!(emitted().is_empty());

        // And the registration is still there: what changed is the verdict,
        // not who is installed. Otherwise the three assertions above would
        // pass just as well with no judgement at all.
        KLOG_SLOT_HOOK.with(|c| c.set(None));
        assert_eq!(klog_read(&mut buf), STORED.len());
        assert_eq!(klog_buf_size(), STORED.len());
    }

    /// The number a message carries is the syslog priority, which is what
    /// userspace filters on: `dmesg -l err`, journald's levels, and the
    /// `<N>` prefix of `/dev/kmsg`. Emitting an error as a warning does not
    /// lose the line, it loses it from the filter that would have shown it.
    #[test]
    fn a_message_keeps_the_syslog_priority_userspace_filters_on() {
        let _g = test_lock();
        let _saved = SavedSlots::take();

        klog_register(stub_read, stub_size, stub_emit);

        // Straight out of Linux's `syslog.h`: KERN_ERR is 3, KERN_WARNING 4
        // and KERN_INFO 6. They are an ABI, not this kernel's own numbering.
        assert_eq!((LOG_ERR, LOG_WARNING, LOG_INFO), (3, 4, 6));

        klog_emit(LOG_ERR, "un error");
        klog_emit(LOG_WARNING, "un aviso");
        klog_emit(LOG_INFO, "una nota");

        assert_eq!(
            emitted(),
            alloc::vec![
                (3u8, "un error".to_string()),
                (4u8, "un aviso".to_string()),
                (6u8, "una nota".to_string()),
            ],
            "cada linea llega con la prioridad con la que se emitio"
        );
    }
}

/// The console window size: what `TIOCSWINSZ` stores and what `TIOCGWINSZ`
/// answers when nobody has stored anything.
#[cfg(test)]
mod win_size_tests {
    extern crate std;

    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// The override is one process-wide cell.
    fn test_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ws(row: u16, col: u16) -> ConsoleWinSize {
        ConsoleWinSize {
            ws_row: row,
            ws_col: col,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    fn clear() {
        set_console_win_size(ws(0, 0));
    }

    /// The whole point of `TIOCSWINSZ` here: the framebuffer console is huge
    /// (227x113 at 2048x2048), which is right on screen and wrong for a serial
    /// viewer whose window is 80x24. A size that is stored and then not
    /// answered leaves `less`, `nano` and `top` laying out for a screen that
    /// is not there.
    #[test]
    fn the_size_a_serial_login_reports_is_the_size_it_gets_back() {
        let _g = test_lock();
        clear();

        set_console_win_size(ws(30, 100));
        let got = console_win_size();
        assert_eq!((got.ws_row, got.ws_col), (30, 100));

        clear();
    }

    /// `stty rows 0` and `stty cols 0` are how a terminal says it does not
    /// know one of its dimensions, and Linux stores them. Only a `0x0` --
    /// both unknown -- means "forget what I told you". Treating either zero as
    /// the clear throws away a size the user just set, and the console jumps
    /// back to the framebuffer's.
    #[test]
    fn only_a_size_unknown_in_both_directions_clears_the_override() {
        let _g = test_lock();
        clear();

        set_console_win_size(ws(0, 100));
        let got = console_win_size();
        assert_eq!(
            (got.ws_row, got.ws_col),
            (0, 100),
            "media dimension sigue siendo lo que dijo el usuario"
        );

        set_console_win_size(ws(30, 0));
        let got = console_win_size();
        assert_eq!((got.ws_row, got.ws_col), (30, 0));

        set_console_win_size(ws(0, 0));
        let got = console_win_size();
        assert_eq!(
            (got.ws_row, got.ws_col),
            (24, 80),
            "las dos a cero si borran, y se vuelve al defecto"
        );

        clear();
    }

    /// With no framebuffer and no `TIOCSWINSZ` the answer is the size every
    /// serial terminal has had since the VT100. Rows and columns the wrong way
    /// round is a console 80 lines tall and 24 wide: every full-screen app
    /// draws off the edge, and nothing reports an error.
    #[test]
    fn with_nothing_to_go_on_the_console_is_eighty_columns_by_twentyfour_rows() {
        let _g = test_lock();
        clear();

        let got = console_win_size();
        assert_eq!(got.ws_row, 24, "veinticuatro FILAS");
        assert_eq!(got.ws_col, 80, "ochenta COLUMNAS");
        assert_ne!(
            (got.ws_row, got.ws_col),
            (0, 0),
            "un tamano explicito, para que `stty size` y las apps coincidan"
        );
    }
}

/// The last-resort writer of the panic path, and the two rules the rest of
/// this module keeps that no test could reach while they lived inside a
/// `cfg(feature = "graphic")` block.
#[cfg(test)]
mod panic_path_tests {
    extern crate std;

    use super::*;
    use alloc::string::String;

    fn taken() -> String {
        PANIC_WRITES.with(|c| core::mem::take(&mut *c.borrow_mut()))
    }

    fn writer() -> EmergencyGraphicWriter {
        let _ = taken();
        EmergencyGraphicWriter {
            buf: [0; 512],
            len: 0,
        }
    }

    /// The buffer is 512 bytes and the report is longer than that, so it has
    /// to go out in pieces -- and every byte has to be in one of them. A
    /// writer that drops its buffer instead of flushing it loses the piece
    /// that was in flight, which on the panic path is the part naming the
    /// fault.
    #[test]
    fn a_report_longer_than_the_buffer_goes_out_whole_and_in_order() {
        let mut w = writer();
        let report: String = (0..700u32)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();

        w.write_str(&report).unwrap();
        w.flush();

        assert_eq!(taken(), report, "ni un byte perdido, ni uno desordenado");
        assert_eq!(w.len, 0, "y el buffer queda vacio detras");
    }

    /// The flush happens because the next character does not fit, not one
    /// character early and not one late: the buffer is exactly full at 512.
    #[test]
    fn the_buffer_flushes_when_the_next_character_does_not_fit() {
        let mut w = writer();

        let full: String = core::iter::repeat_n('x', 512).collect();
        w.write_str(&full).unwrap();
        assert_eq!(w.len, 512, "512 caracteres de uno caben justos");
        assert_eq!(taken(), "", "y nada ha salido todavia");

        w.write_str("y").unwrap();
        assert_eq!(taken(), full, "el 513 saca los 512 de golpe");
        assert_eq!(w.len, 1, "y se queda el que no cabia");
    }

    /// A character is flushed whole or not at all. Measuring the room in
    /// characters rather than in bytes splits a multi-byte one across two
    /// flushes, and `from_utf8` then refuses BOTH halves -- so a report with
    /// an accent in it loses the 512 bytes around it, silently, because the
    /// flush drops what it cannot decode.
    #[test]
    fn a_multibyte_character_is_never_split_across_two_flushes() {
        let mut w = writer();

        // 510 bytes used, and a 3-byte character asking for room.
        let head: String = core::iter::repeat_n('x', 510).collect();
        w.write_str(&head).unwrap();
        assert_eq!(w.len, 510);

        w.write_str("€").unwrap();
        assert_eq!(taken(), head, "los 510 salen enteros");
        assert_eq!(w.len, 3, "y el euro entra entero detras, sus tres bytes");

        w.flush();
        assert_eq!(taken(), "€");
    }

    /// The serial console follows VT switches, with one exception: the
    /// reserved graphics VT (tty7) is the only one with no login shell, so
    /// binding serial to it while the desktop is on screen silences the line
    /// -- the only text a monitor-less box has.
    #[test]
    fn the_serial_line_never_follows_the_desktop_onto_the_graphics_vt() {
        for vt in 0..GRAPHICS_VT {
            assert_eq!(
                serial_vt_for(vt),
                vt,
                "en modo texto la serie sigue al VT activo"
            );
        }
        assert_eq!(
            serial_vt_for(GRAPHICS_VT),
            0,
            "con el escritorio delante, la serie se queda en tty1"
        );
    }

    /// ~1 Hz: half a second showing, half a second hidden, starting shown.
    /// The tick that calls this runs at ~250 Hz and does nothing until the
    /// phase flips, so the divisor is both the blink rate and the reason the
    /// common tick is one atomic load.
    #[test]
    fn the_cursor_shows_for_half_a_second_and_hides_for_half_a_second() {
        assert_eq!(blink_phase(0), 0, "arranca visible");
        assert_eq!(
            blink_phase(499),
            0,
            "y sigue visible hasta el medio segundo"
        );
        assert_eq!(blink_phase(500), 1, "ahi se apaga");
        assert_eq!(blink_phase(999), 1);
        assert_eq!(blink_phase(1000), 0, "y un segundo despues vuelve");

        // Two states and no third: the tick compares this against the last one
        // it saw, so anything but 0/1 would make every tick look like a flip.
        for ms in (0..4000).step_by(37) {
            assert!(blink_phase(ms) <= 1, "{} da una fase que no existe", ms);
        }
    }
}
