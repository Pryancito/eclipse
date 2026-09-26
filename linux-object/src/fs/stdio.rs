//! Implement INode for Stdin & Stdout
#![allow(dead_code)]

use super::ioctl::*;
use super::kbd_layout::{self, KeyMods};
use crate::{sync::Event, sync::EventBus};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::convert::TryFrom;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use core::task::{Context, Poll};
use core::time::Duration;
use kernel_hal::console::{self, ConsoleWinSize};
use kernel_hal::sync::Mutex;
use kernel_hal::user::{Error as UserError, UserInPtr, UserOutPtr};
use lazy_static::lazy_static;
use rcore_fs::vfs::*;
use zcore_drivers::prelude::{InputEvent, InputEventType};
use zircon_object::object::KernelObject;
use zircon_object::task::Thread;

// c_iflag
const IGNBRK: u32 = 0x0001;
const BRKINT: u32 = 0x0002;
const IGNPAR: u32 = 0x0004;
const PARMRK: u32 = 0x0008;
const INPCK: u32 = 0x0010;
const IXON: u32 = 0x0400;
const IXANY: u32 = 0x0800;
const IXOFF: u32 = 0x1000;
const IMAXBEL: u32 = 0x2000;
const IUTF8: u32 = 0x4000;

// c_oflag: the whole word now lives next to `Termios` in `ioctl.rs`, as
// `O_OPOST`..`O_ONLRET`. It was copied here, and in the two PTYs, and only
// `OPOST` and `ONLCR` were ever read -- under this file's `allow(dead_code)`,
// which is why six flags could sit here unread without a word from the
// compiler. `OFILL` and `OFDEL` are padding delays for a printing terminal
// and are not implemented anywhere, so they are simply gone.

// c_lflag
const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const XCASE: u32 = 0x0004;
const ECHO: u32 = 0x0008;
const ECHOE: u32 = 0x0010;
const ECHONL: u32 = 0x0040;
// ECHOK and ECHOKE are `L_ECHOK`/`L_ECHOKE` in `ioctl.rs`, next to the
// `Termios::kill_echo` both line disciplines ask.
const NOFLSH: u32 = 0x0080;
const TOSTOP: u32 = 0x0100;
const ECHOCTL: u32 = 0x0200;
const ECHOPRT: u32 = 0x0400;
const FLUSHO: u32 = 0x1000;
const PENDIN: u32 = 0x4000;

// c_cc indices
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
// VTIME and VMIN are `VTIME_CC`/`VMIN_CC` in `ioctl.rs`, next to the
// `Termios::noncanon_read` both line disciplines ask.
const VSWTC: usize = 7;
const VSTART: usize = 8;
const VSTOP: usize = 9;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VREPRINT: usize = 12;
const VDISCARD: usize = 13;
const VWERASE: usize = 14;
const VLNEXT: usize = 15;
const VEOL2: usize = 16;

// Per-VT TTY state: each virtual terminal has its own termios and foreground
// process group, so e.g. a shell putting its terminal in raw mode on tty2 does
// not disturb the cooked shell on tty1.
struct TtyState {
    termios: Mutex<Termios>,
    fg_pgrp: AtomicI32,
    /// VT switch signalling mode (`VT_GETMODE` / `VT_SETMODE`).
    vt_mode: Mutex<VtMode>,
    /// Keyboard translation mode (`KDGKBMODE` / `KDSKBMODE`). When an X server
    /// sets `K_RAW`/`K_OFF`, the line discipline stops cooking key presses into
    /// this TTY (the raw events still reach userspace via `/dev/input/event*`).
    kbd_mode: AtomicI32,
    /// Scroll/Num/Caps LED bits last set via `KDSETLED`.
    kbd_leds: AtomicU8,
    /// Software flow control (IXON): output to this VT is paused after a `VSTOP`
    /// (Ctrl-S) and resumed by `VSTART` (Ctrl-Q).
    flow_stopped: AtomicBool,
    /// The KoID of the process that put this VT into `VT_PROCESS` mode via
    /// `VT_SETMODE` -- the graphics session (X/Wayland) that owns the VT-switch
    /// handshake for it. 0 = none (VT_AUTO / a plain text console). Used to
    /// deliver `relsig` on a switch away and `acqsig` on a switch back,
    /// Linux-style (see [`request_vt_switch`]).
    vt_owner: AtomicU64,
    /// Double-Ctrl-C arm: first VINTR for a pgrp stores it here; a second
    /// VINTR for the same pgrp escalates to SIGKILL (see
    /// [`crate::process::interrupt_or_force_pgrp`]).
    ctrl_c_armed_pgid: AtomicI32,
    /// Where the cursor sits on this VT's line, counted the way the driver
    /// counts it. `ONOCR` is defined in terms of it and `ONLRET` exists to
    /// keep it honest; see [`Termios::output_char`]. Program output and echo
    /// share it because they share the screen.
    out_column: AtomicUsize,
}

lazy_static! {
    static ref TTY_STATES: Vec<TtyState> = (0..kernel_hal::console::NUM_VTS)
        .map(|_| TtyState {
            termios: Mutex::new(Termios::default_tty()),
            fg_pgrp: AtomicI32::new(0),
            vt_mode: Mutex::new(VtMode::auto()),
            kbd_mode: AtomicI32::new(K_XLATE),
            kbd_leds: AtomicU8::new(0),
            flow_stopped: AtomicBool::new(false),
            vt_owner: AtomicU64::new(0),
            ctrl_c_armed_pgid: AtomicI32::new(0),
            out_column: AtomicUsize::new(0),
        })
        .collect();
}

#[inline]
fn vt_clamp(vt: usize) -> usize {
    vt.min(kernel_hal::console::NUM_VTS - 1)
}

fn tty_termios(vt: usize) -> &'static Mutex<Termios> {
    &TTY_STATES[vt_clamp(vt)].termios
}

/// This VT's output column. See [`TtyState::out_column`].
fn tty_column(vt: usize) -> &'static AtomicUsize {
    &TTY_STATES[vt_clamp(vt)].out_column
}

fn tty_fg_pgrp(vt: usize) -> &'static AtomicI32 {
    &TTY_STATES[vt_clamp(vt)].fg_pgrp
}

fn tty_vt_mode(vt: usize) -> &'static Mutex<VtMode> {
    &TTY_STATES[vt_clamp(vt)].vt_mode
}

fn tty_kbd_mode(vt: usize) -> i32 {
    TTY_STATES[vt_clamp(vt)].kbd_mode.load(Ordering::Relaxed)
}

/// Whether output to `vt` is currently paused by software flow control (IXON).
fn tty_flow_stopped(vt: usize) -> bool {
    TTY_STATES[vt_clamp(vt)]
        .flow_stopped
        .load(Ordering::Relaxed)
}

/// Whether VT `vt` is translating key presses into TTY characters. False while
/// an X server holds the keyboard in `K_RAW`/`K_OFF`/`K_MEDIUMRAW`.
fn tty_kbd_cooked(vt: usize) -> bool {
    matches!(tty_kbd_mode(vt), K_XLATE | K_UNICODE)
}

/// The VT an X server is driving in `K_MEDIUMRAW`, if any. kdrive (TinyX) puts
/// the keyboard of the VT it opened into medium-raw and reads keycodes from that
/// VT's tty. Routing is strictly by the *active* VT: medium-raw keycodes are
/// only delivered to the foreground VT when that VT is the one in medium-raw.
///
/// Earlier this fell back to "the first VT in medium-raw" whenever the active VT
/// wasn't, to paper over a feared `VT_OPENQRY`/`active_vt()` desync. But X takes
/// over the VT it was launched on (`VT_OPENQRY` returns the active VT and X sets
/// that same VT to medium-raw, so they are always in sync), and the fallback had
/// a nasty side effect: after the user switched to a text VT with Ctrl+Alt+Fx,
/// the X VT was still in medium-raw, so *every* keystroke kept being swallowed
/// into X's tty and the text console could never be typed into. Gating on the
/// active VT fixes that — keys go to X only while X is foreground, and to the
/// cooked console otherwise. Returns `None` for a normal cooked console.
fn medium_raw_vt() -> Option<usize> {
    let active = vt_clamp(kernel_hal::console::active_vt());
    if tty_kbd_mode(active) == K_MEDIUMRAW {
        Some(active)
    } else {
        None
    }
}

/// Target VT (0-based) of a deferred `VT_PROCESS` switch, or -1 when none is
/// pending. Set when we deliver `relsig` to a graphics session and wait for its
/// `VT_RELDISP` acknowledgement; cleared when the switch completes or is
/// refused. Lock-free so it is safe to touch from the keyboard input path.
static VT_SWITCH_PENDING: AtomicI32 = AtomicI32::new(-1);

/// The KoID owning VT `vt` under `VT_PROCESS` (0 if none / VT_AUTO).
fn vt_owner(vt: usize) -> u64 {
    TTY_STATES[vt_clamp(vt)].vt_owner.load(Ordering::Relaxed)
}

/// Whether `VT_SETMODE` accepts this `vt_mode`.
///
/// Linux: `if (vc->vt_mode.mode != VT_AUTO && vc->vt_mode.mode != VT_PROCESS)
/// return -EINVAL`. Storing an unrecognised mode instead meant every later
/// `mode == VT_PROCESS` test read false, so the VT looked unowned to the
/// switch handshake while the caller believed it had taken it -- and nothing
/// told the caller otherwise.
fn vt_mode_accepted(mode: u8) -> bool {
    mode == VT_AUTO || mode == VT_PROCESS
}

/// Whether `KDSKBMODE` accepts this keyboard mode: Linux's `vt_do_kdskbmode`
/// takes exactly `K_RAW`, `K_MEDIUMRAW`, `K_XLATE`, `K_UNICODE` and `K_OFF`,
/// and answers `-EINVAL` to anything else.
///
/// The value used to be stored as it came. That was not harmless: the line
/// discipline decides "cooked" as `matches!(mode, K_XLATE | K_UNICODE)`, so a
/// VT left in any other number stopped translating key presses -- the
/// keyboard went dead on that console, `KDGKBMODE` echoed the nonsense back,
/// and the caller had been told `Ok`. The third ioctl of this family with the
/// same hole, after `VT_SETMODE` and `KDSETMODE`.
fn kbd_mode_accepted(mode: i32) -> bool {
    matches!(mode, K_RAW | K_MEDIUMRAW | K_XLATE | K_UNICODE | K_OFF)
}

/// Give a VT back to the kernel, the way Linux's `reset_vc()` does.
///
/// Used when the process that took the VT with `VT_SETMODE(VT_PROCESS)` can no
/// longer be reached: it died, or the signal it asked for is not one that can
/// be sent. Linux reverts the handshake AND puts the VT back into `KD_TEXT`,
/// and the comment above its call site in `vt_ioctl.c` says exactly why:
///
/// > The controlling process has died, so we revert back to normal operation.
/// > In this case, we'll also change back to KD_TEXT mode. I'm not sure if
/// > this is strictly correct but it saves the agony when the X server dies
/// > and the screen remains blanked due to KD_GRAPHICS!
///
/// That last part was the piece missing here, in both of the two places that
/// spotted a dead owner: they reverted the mode and the owner and left the VT
/// in `KD_GRAPHICS`, so nothing drew on it again. A compositor that crashes
/// took the screen with it.
fn reset_vc(vt: usize) {
    let vt = vt_clamp(vt);
    *tty_vt_mode(vt).lock() = VtMode::auto();
    TTY_STATES[vt].vt_owner.store(0, Ordering::Relaxed);
    kernel_hal::console::set_kd_mode_vt(vt, kernel_hal::console::KD_TEXT);
    kernel_hal::klog_info!("[vt] reset_vc {} -- owner gone, back to text", vt);
}

/// Whether the process that owns `vt`'s switch handshake is still alive.
///
/// A KoID that no longer names a process is not an owner. The one consumer,
/// [`graphics_vt_seat_owned`], uses this to decide whether somebody else will
/// restore the console — and the somebody else is the process that just died.
fn vt_owner_alive(vt: usize) -> bool {
    let owner = vt_owner(vt);
    owner != 0 && crate::process::process_exists(owner)
}

/// True when a graphics session (seatd/libseat) owns the reserved graphics VT
/// (`tty7`) via `VT_SETMODE(VT_PROCESS)`. In that case the seat -- not the DRM
/// master mechanism -- drives the console's KD mode and VT switching (seatd
/// issues `KDSETMODE`/`VT_RELDISP` on the VT), so the DRM `DROP_MASTER`
/// console-restore must NOT independently flip the graphics VT back to text:
/// on real hardware a transient `DROP_MASTER` during wlroots/Xwayland renderer
/// fallback fired `set_kd_mode(KD_TEXT)` on the active graphics VT, which
/// `switch_vt_impl(0)` reverted to tty1 -- so the compositor's first present
/// landed on VT 0 and the desktop never showed on tty7.
pub fn graphics_vt_seat_owned() -> bool {
    // The owner must still EXIST. Its only consumer is the `DROP_MASTER`
    // console restore, which stands down because "the seat will do it" -- and
    // when the compositor is what died, the seat is the corpse. The restore
    // that exists precisely for this case was suppressed by it.
    vt_owner_alive(kernel_hal::console::GRAPHICS_VT)
}

/// KoID of the calling process, or 0 if it can't be resolved. Used to record
/// the owner of a VT when it enters `VT_PROCESS` mode.
fn current_koid() -> u64 {
    if let Some(arc) = kernel_hal::thread::get_current_thread() {
        if let Ok(thread) = arc.downcast::<Thread>() {
            return thread.proc().id();
        }
    }
    0
}

/// Map a `vt_mode` signal number (`relsig`/`acqsig`) to a [`Signal`]. Returns
/// `None` for 0 (no signal) or an out-of-range value.
fn vt_signal(n: i16) -> Option<crate::signal::Signal> {
    if !(1..=64).contains(&n) {
        return None;
    }
    crate::signal::Signal::try_from(n as u8).ok()
}

/// Request a switch to `target` (0-based VT), running the Linux `VT_PROCESS`
/// release handshake when the current foreground VT is driven by a graphics
/// session.
///
/// Mirrors the kernel's `change_console()`:
///  * If the current VT is in `VT_PROCESS` mode with a live owner, record the
///    pending target, send `relsig`, and return *without* switching. The owner
///    drops DRM master and replies `VT_RELDISP(1)`, which runs
///    [`complete_vt_switch`]. (`VT_RELDISP(0)` refuses and cancels.)
///  * Otherwise (VT_AUTO, no owner, or a dead owner) switch immediately.
///
/// Escape hatch for a wedged compositor that never answers `relsig`: a second
/// request while one is still pending forces the switch through at once (e.g.
/// pressing Ctrl+Alt+Fn again). The `drm.rs` blit suppression on a non-active
/// graphics VT remains the ultimate safety net once the switch lands.
fn request_vt_switch(target: usize) {
    let target = vt_clamp(target);
    let cur = vt_clamp(kernel_hal::console::active_vt());
    if cur == target {
        VT_SWITCH_PENDING.store(-1, Ordering::Relaxed);
        return;
    }

    // A prior handshake never completed (uncooperative / hung owner): force it.
    if VT_SWITCH_PENDING.swap(-1, Ordering::Relaxed) >= 0 {
        complete_vt_switch(target);
        return;
    }

    let mode = *tty_vt_mode(cur).lock();
    let owner = vt_owner(cur);
    if mode.mode == VT_PROCESS && owner != 0 {
        // An owner whose `relsig` is 0 or out of range cannot be told to
        // release, so it is as unreachable as a dead one. This used to skip
        // the whole branch and switch away with the VT still owned; in Linux
        // `kill_pid` refuses the invalid signal and the same `reset_vc` runs.
        match vt_signal(mode.relsig).ok_or(()).and_then(|sig| {
            crate::process::send_signal_to_process(owner as usize, sig).map_err(|_| ())
        }) {
            Ok(()) => {
                // Defer: the owner will VT_RELDISP once it has released.
                VT_SWITCH_PENDING.store(target as i32, Ordering::Relaxed);
                return;
            }
            Err(()) => reset_vc(cur),
        }
    }
    complete_vt_switch(target);
}

/// Finish a VT switch: activate `target` and, when it is owned by a graphics
/// session in `VT_PROCESS` mode, deliver `acqsig` so the session re-takes DRM
/// master and repaints. Mirrors the kernel's `complete_change_console()`.
fn complete_vt_switch(target: usize) {
    let target = vt_clamp(target);
    kernel_hal::console::switch_vt(target);
    // klog (survives LOG=error): the display is now on `target`. `owner` != 0
    // means a graphics session (seatd) will get acqsig below and can activate;
    // owner == 0 on the graphics VT (tty7 == 6) means nobody is registered to
    // acquire it -- the compositor stays paused and never presents.
    kernel_hal::klog_info!(
        "[vt] switch -> active={} owner={:#x}",
        target,
        vt_owner(target)
    );
    // Re-blit the compositor's last frame if we landed back on its VT (a no-op
    // for a text target, which the console repaints itself).
    crate::fs::devfs::drm::represent_if_owner_active();

    let mode = *tty_vt_mode(target).lock();
    let owner = vt_owner(target);
    if mode.mode == VT_PROCESS && owner != 0 {
        let delivered = vt_signal(mode.acqsig)
            .map(|sig| crate::process::send_signal_to_process(owner as usize, sig).is_ok())
            .unwrap_or(false);
        if !delivered {
            // Nobody left to acquire this VT: give it back to the kernel, text
            // mode included, or it stays blank with no owner to draw on it.
            reset_vc(target);
        }
    }
}

fn user_copy<T>(r: core::result::Result<T, UserError>) -> Result<T> {
    r.map_err(|_| FsError::InvalidParam)
}

/// `c_cc[idx] == 0` means the special character is disabled (`_POSIX_VDISABLE`).
#[inline]
fn cc_match(cc: &[u8; 19], idx: usize, c: u8) -> bool {
    let v = cc[idx];
    v != 0 && c == v
}

/// Shared TTY/console ioctl handling for every VT-backed device node
/// (`/dev/tty`, `/dev/tty[0-9]`, `/dev/console`, stdin and stdout). `vt` is the
/// virtual terminal the file refers to.
fn tty_ioctl(vt: usize, cmd: u32, data: usize) -> Result<usize> {
    match cmd as usize {
        TIOCGWINSZ => {
            user_copy(UserOutPtr::<ConsoleWinSize>::from(data).write(console::console_win_size()))?;
            Ok(0)
        }
        // Honor a caller-supplied window size. The framebuffer-derived default
        // is right for the on-screen graphic console but far too large for a
        // serial viewer (e.g. 227x113), so `resize`/`stty` over serial — and
        // the /etc/profile size probe — set the real terminal size here and
        // full-screen apps (nano, less, top) then render correctly. A 0x0 size
        // clears the override and restores the framebuffer size.
        TIOCSWINSZ => {
            let ws = user_copy(UserInPtr::<ConsoleWinSize>::from(data).read())?;
            let old = console::console_win_size();
            console::set_console_win_size(ws);
            let new = console::console_win_size();
            if old.ws_row != new.ws_row || old.ws_col != new.ws_col {
                let pgid = tty_fg_pgrp(vt).load(Ordering::Relaxed);
                if pgid > 0 {
                    let _ = crate::process::send_signal_to_pgrp(
                        pgid as usize,
                        crate::signal::Signal::SIGWINCH,
                    );
                }
            }
            Ok(0)
        }
        TCGETS => {
            user_copy(UserOutPtr::<Termios>::from(data).write(*tty_termios(vt).lock()))?;
            Ok(0)
        }
        TIOCSPGRP => {
            let pgid = unsafe { *(data as *const i32) };
            let vt_i = vt_clamp(vt);
            let old = tty_fg_pgrp(vt).swap(pgid, Ordering::Relaxed);
            if old != pgid {
                crate::process::clear_interrupt_arm(&TTY_STATES[vt_i].ctrl_c_armed_pgid);
            }
            Ok(0)
        }
        TIOCGPGRP => {
            let mut pgid = tty_fg_pgrp(vt).load(Ordering::Relaxed);
            if pgid == 0 {
                if let Some(arc) = kernel_hal::thread::get_current_thread() {
                    if let Ok(thread) = arc.downcast::<Thread>() {
                        pgid = crate::process::get_process_pgid(thread.proc().id()).unwrap_or(0)
                            as i32;
                    }
                }
            }
            if pgid == 0 {
                pgid = 1;
            }
            user_copy(UserOutPtr::<i32>::from(data).write(pgid))?;
            Ok(0)
        }
        TCSETS | TCSETSW => {
            *tty_termios(vt).lock() = user_copy(UserInPtr::<Termios>::from(data).read())?;
            Ok(0)
        }
        TCSETSF => {
            *tty_termios(vt).lock() = user_copy(UserInPtr::<Termios>::from(data).read())?;
            let sin = vt_stdin(vt);
            sin.buf.lock().clear();
            sin.canon_buf.lock().clear();
            sin.eof_pending.store(false, Ordering::Release);
            sin.vtime_deadline_ns.store(0, Ordering::Release);
            sin.eventbus.lock().clear(Event::READABLE);
            Ok(0)
        }
        // Argument is by value: TCIFLUSH=0, TCOFLUSH=1, TCIOFLUSH=2. A fourth
        // value is `EINVAL` (`tty_perform_flush`'s `default:`), which is worth
        // saying out loud rather than answering 0: a caller that got the
        // selector wrong -- and it is easy to get wrong, since `tcflush(3)`
        // takes it as its own argument and the ioctl takes it by value --
        // otherwise believed the queue had been thrown away.
        TCFLSH => {
            const TCIFLUSH: usize = 0;
            const TCOFLUSH: usize = 1;
            const TCIOFLUSH: usize = 2;
            if !matches!(data, TCIFLUSH | TCOFLUSH | TCIOFLUSH) {
                return Err(FsError::InvalidParam);
            }
            if data == TCIFLUSH || data == TCIOFLUSH {
                let sin = vt_stdin(vt);
                sin.buf.lock().clear();
                sin.canon_buf.lock().clear();
                sin.eof_pending.store(false, Ordering::Release);
                sin.vtime_deadline_ns.store(0, Ordering::Release);
                sin.eventbus.lock().clear(Event::READABLE);
            }
            Ok(0)
        }
        KDGETMODE => {
            unsafe { *(data as *mut i32) = console::kd_mode_vt(vt) as i32 };
            Ok(0)
        }
        KDSETMODE => {
            // `KDSETMODE` passes the mode (KD_TEXT/KD_GRAPHICS) by *value* in
            // the ioctl argument, not via a pointer — matches Linux
            // drivers/tty/vt/vt_ioctl.c. Dereferencing it would read a bogus
            // user address (e.g. KD_GRAPHICS == 1 → *0x1) and fault instead of
            // switching the console to graphics mode, which is exactly the step
            // an X server (TinyX/Xorg) performs to seize the display.
            let mode = data as u32;
            // Linux answers a mode that is neither KD_TEXT (nor its obsolete
            // aliases KD_TEXT0/KD_TEXT1) nor KD_GRAPHICS with EINVAL. Saying
            // Ok(0) to one was worse than it sounds: the caller believed the
            // console was in the mode it asked for, and the value went on to
            // be stored, where every downstream test reads "not KD_TEXT" and
            // stops presenting the text console -- a screen that goes blank
            // and stays blank, with nothing reported to the process that did
            // it. Same hole `vt_mode_accepted` closed for `VT_SETMODE`.
            let Some(mode) = console::normalize_kd_mode(mode) else {
                warn!("[vt] KDSETMODE vt={} rejected mode={:#x}", vt, mode);
                return Err(FsError::InvalidParam);
            };
            // Once-per-session VT diagnostic. seatd drives KD_GRAPHICS through
            // /dev/tty0, which now resolves to the active VT: this line should
            // show the graphics mode landing on the graphics VT (tty7 == 6),
            // not VT 0. `active` is the foreground VT at the time of the call.
            // klog (not warn!) so it survives the default LOG=error boot level.
            kernel_hal::klog_info!(
                "[vt] KDSETMODE vt={} mode={:#x} (active={})",
                vt,
                mode,
                kernel_hal::console::active_vt()
            );
            console::set_kd_mode_vt(vt, mode);
            Ok(0)
        }
        // X validates a console fd with KDGKBTYPE; reply with a PC keyboard.
        KDGKBTYPE => {
            unsafe { *(data as *mut u8) = KB_101 };
            Ok(0)
        }
        KDGKBMODE => {
            unsafe { *(data as *mut i32) = tty_kbd_mode(vt) };
            Ok(0)
        }
        KDSKBMODE => {
            // Like `KDSETMODE`, the keyboard mode (K_RAW/K_XLATE/K_OFF/…) is the
            // ioctl argument by value, not a pointer. X puts the keyboard into
            // K_RAW/K_OFF this way during console takeover.
            let mode = data as i32;
            if !kbd_mode_accepted(mode) {
                warn!("[vt] KDSKBMODE vt={} rejected mode={:#x}", vt, mode);
                return Err(FsError::InvalidParam);
            }
            TTY_STATES[vt_clamp(vt)]
                .kbd_mode
                .store(mode, Ordering::Relaxed);
            Ok(0)
        }
        // kdrive/TinyX reads the kernel keymap entry-by-entry to map medium-raw
        // keycodes to X keysyms. `struct kbentry { u8 kb_table; u8 kb_index;
        // u16 kb_value; }`: the caller fills kb_table/kb_index, we fill kb_value.
        KDGKBENT => {
            if data == 0 {
                return Err(FsError::InvalidParam);
            }
            let p = data as *mut u8;
            let (table, index) = unsafe { (*p, *p.add(1)) };
            let value = linux_keycode_to_evdev(index)
                .map(|evdev| kdgkbent_value(evdev, table))
                .unwrap_or(0);
            unsafe { *(p.add(2) as *mut u16) = value };
            Ok(0)
        }
        KDGETLED => {
            user_copy(
                UserOutPtr::<i32>::from(data)
                    .write(TTY_STATES[vt_clamp(vt)].kbd_leds.load(Ordering::Relaxed) as i32),
            )?;
            Ok(0)
        }
        KDSETLED => {
            TTY_STATES[vt_clamp(vt)]
                .kbd_leds
                .store(data as u8, Ordering::Relaxed);
            Ok(0)
        }
        KDMKTONE => Ok(0),
        // VT management. VT numbers in these ioctls are 1-based (tty1 == VT 1),
        // while the kernel tracks VTs 0-based internally.
        VT_OPENQRY => {
            // Hand the caller the active VT: an X server then takes over the
            // terminal it was launched from, which always has a device node.
            let vtno = kernel_hal::console::active_vt() as i32 + 1;
            unsafe { *(data as *mut i32) = vtno };
            Ok(0)
        }
        VT_GETMODE => {
            unsafe { *(data as *mut VtMode) = *tty_vt_mode(vt).lock() };
            Ok(0)
        }
        VT_SETMODE => {
            let mode = data as *const VtMode;
            if mode.is_null() {
                return Err(FsError::InvalidParam);
            }
            let vm = unsafe { *mode };
            if !vt_mode_accepted(vm.mode) {
                return Err(FsError::InvalidParam);
            }
            *tty_vt_mode(vt).lock() = vm;
            // Record (or clear) the graphics session that now owns this VT's
            // switch handshake. In `VT_PROCESS` the caller becomes the owner we
            // signal with `relsig`/`acqsig`; `VT_AUTO` relinquishes it.
            let owner = if vm.mode == VT_PROCESS {
                current_koid()
            } else {
                0
            };
            TTY_STATES[vt_clamp(vt)]
                .vt_owner
                .store(owner, Ordering::Relaxed);
            // Once-per-session VT diagnostic. This is the load-bearing call: in
            // VT_PROCESS mode seatd becomes the switch-handshake owner of `vt`.
            // With /dev/tty0 resolving to the active VT, `vt` must be the
            // graphics VT (tty7 == 6) so `complete_vt_switch` can deliver acqsig
            // back to seatd and libseat activates the compositor. owner=0 means
            // VT_AUTO (relinquished). klog so it survives LOG=error.
            kernel_hal::klog_info!(
                "[vt] VT_SETMODE vt={} mode={} owner={:#x} (active={})",
                vt,
                vm.mode,
                owner,
                kernel_hal::console::active_vt()
            );
            Ok(0)
        }
        VT_GETSTATE => {
            let active = kernel_hal::console::active_vt() as u16 + 1;
            // v_state is a bitmask of in-use VTs; bit N == VT N (bit 0 unused).
            let in_use = (((1u32 << kernel_hal::console::NUM_VTS) - 1) << 1) as u16;
            unsafe {
                *(data as *mut VtStat) = VtStat {
                    v_active: active,
                    v_signal: 0,
                    v_state: in_use,
                }
            };
            Ok(0)
        }
        VT_ACTIVATE => {
            // klog (survives LOG=error): eclipse-init switches to tty7 via
            // VT_ACTIVATE(7). If `num_vts` is 1 here (graphic console not yet
            // set up) the range check silently drops the switch yet still
            // returns success -- this line names that case (target vs num_vts).
            kernel_hal::klog_info!(
                "[vt] VT_ACTIVATE target={} (num_vts={} active={})",
                data,
                kernel_hal::console::num_vts(),
                kernel_hal::console::active_vt()
            );
            if data >= 1 && data <= kernel_hal::console::num_vts() {
                // Route through the VT_PROCESS handshake: if the current VT is
                // owned by a graphics session, this defers until it releases.
                request_vt_switch(data - 1);
            }
            Ok(0)
        }
        // A cooperating switch may still be deferred while the previous owner
        // releases; our switches otherwise complete synchronously. We don't
        // block here (no async ioctl context), so report success — the pending
        // handshake completes shortly via `VT_RELDISP`.
        VT_WAITACTIVE => Ok(0),
        // The graphics session acknowledges a release/acquire request. On a
        // switch-*from* (a pending switch is waiting): arg 0 refuses and cancels
        // it, any other value releases and completes it. On a switch-*to*
        // (`VT_ACKACQ`, nothing pending) it is just an acquire ack — accept it.
        VT_RELDISP => {
            let target = VT_SWITCH_PENDING.load(Ordering::Relaxed);
            if target >= 0 {
                if data == 0 {
                    // Refused: abandon the pending switch, stay put.
                    VT_SWITCH_PENDING.store(-1, Ordering::Relaxed);
                } else {
                    VT_SWITCH_PENDING.store(-1, Ordering::Relaxed);
                    complete_vt_switch(target as usize);
                }
            }
            Ok(0)
        }
        // Our VTs are never freed; accept so X's teardown proceeds.
        VT_DISALLOCATE => Ok(0),
        TIOCSCTTY | TIOCNOTTY => Ok(0),
        // Bytes available to read: the cooked input queue for this VT.
        FIONREAD => {
            let n = vt_stdin(vt).buf.lock().len() as i32;
            user_copy(UserOutPtr::<i32>::from(data).write(n))?;
            Ok(0)
        }
        // Console output is drawn synchronously, so nothing is ever queued.
        TIOCOUTQ => {
            user_copy(UserOutPtr::<i32>::from(data).write(0))?;
            Ok(0)
        }
        // Session ID of the terminal. We don't track sessions separately, so
        // report the foreground process group (same fallback as TIOCGPGRP).
        TIOCGSID => {
            let mut sid = tty_fg_pgrp(vt).load(Ordering::Relaxed);
            if sid <= 0 {
                sid = 1;
            }
            user_copy(UserOutPtr::<i32>::from(data).write(sid))?;
            Ok(0)
        }
        // Modem control lines. A VT has no real RS-232 lines, so report a
        // permanently-connected local terminal (DTR/RTS asserted, carrier up)
        // and accept writes as no-ops — matching how Linux treats a console.
        TIOCMGET => {
            let lines = TIOCM_DTR | TIOCM_RTS | TIOCM_CAR | TIOCM_CTS | TIOCM_DSR;
            user_copy(UserOutPtr::<i32>::from(data).write(lines))?;
            Ok(0)
        }
        TIOCMSET | TIOCMBIS | TIOCMBIC => Ok(0),
        // No real UART behind a VT, so all serial line counters are zero.
        TIOCGICOUNT => {
            user_copy(UserOutPtr::<SerialIcounter>::from(data).write(SerialIcounter::default()))?;
            Ok(0)
        }
        // Linux console multiplexor. The subcommand is the first byte of the
        // argument. We implement TIOCL_GETSHIFTSTATE (read modifier state),
        // which programs poll — sometimes in a tight loop — to read Shift/Ctrl/
        // Alt/AltGr without an evdev device; leaving it unhandled returned
        // ENOTTY and could spin a poller, flooding the console. Bits match
        // Linux's `shift_state` (KG_SHIFT/ALTGR/CTRL/ALT = 0/1/2/3).
        TIOCLINUX => {
            let p = data as *mut u8;
            if p.is_null() {
                return Err(FsError::InvalidParam);
            }
            let subcmd = unsafe { *p };
            match subcmd {
                TIOCL_GETSHIFTSTATE => {
                    let mut state = 0u8;
                    if SHIFT_DOWN.load(Ordering::SeqCst) {
                        state |= 1 << 0;
                    }
                    if ALTGR_DOWN.load(Ordering::SeqCst) {
                        state |= 1 << 1;
                    }
                    if CTRL_DOWN.load(Ordering::SeqCst) {
                        state |= 1 << 2;
                    }
                    if LEFT_ALT_DOWN.load(Ordering::SeqCst) {
                        state |= 1 << 3;
                    }
                    unsafe { *p = state };
                    Ok(0)
                }
                other => {
                    // Surface the actual subcommand once so an unhandled poller
                    // can be identified, then report EINVAL as Linux does.
                    static LOGGED: AtomicBool = AtomicBool::new(false);
                    if !LOGGED.swap(true, Ordering::Relaxed) {
                        warn!("TIOCLINUX: unhandled subcommand {}", other);
                    }
                    Err(FsError::InvalidParam)
                }
            }
        }
        _ => Err(FsError::NotSupported),
    }
}

/// Foreground process group of the *active* terminal (for signal delivery).
pub fn get_foreground_pgrp() -> i32 {
    tty_fg_pgrp(kernel_hal::console::active_vt()).load(Ordering::Relaxed)
}

/// The foreground process group of a *named* VT, for a Ctrl-C that was typed
/// on a VT other than the one on screen.
pub fn vt_foreground_pgrp(vt: usize) -> i32 {
    tty_fg_pgrp(vt).load(Ordering::Relaxed)
}

pub fn set_foreground_pgrp(pgid: i32) {
    tty_fg_pgrp(kernel_hal::console::active_vt()).store(pgid, Ordering::Relaxed);
}

/// Seed a *specific* VT's foreground process group. Called when a per-VT shell
/// is spawned so it starts as the foreground job of its own tty. Without this
/// the VT's `fg_pgrp` stays 0 while the shell's pgrp is its pid, so `tcgetpgrp`
/// != `getpgrp`: an interactive `sh` then believes it is a *background* job and
/// spins sending itself `SIGTTIN` (a tight enter_uspace loop = wasted CPU/heat).
pub fn set_vt_foreground_pgrp(vt: usize, pgid: i32) {
    tty_fg_pgrp(vt).store(pgid, Ordering::Relaxed);
}

/// Replace termios on the active VT (used when `TCSETS` hits a non-tty fd).
pub fn set_active_vt_termios(termios: Termios) {
    *tty_termios(kernel_hal::console::active_vt()).lock() = termios;
}

/// Like [`set_active_vt_termios`] plus input-buffer flush for `TCSETSF`.
pub fn set_active_vt_termios_flush(termios: Termios) {
    let vt = kernel_hal::console::active_vt();
    *tty_termios(vt).lock() = termios;
    let sin = vt_stdin(vt);
    sin.buf.lock().clear();
    sin.canon_buf.lock().clear();
    sin.eventbus.lock().clear(Event::READABLE);
}

/// Global Ctrl-C latch. Many programs (udhcpc among them) never read stdin
/// while they run, so `recvfrom`, `poll` and the rest need a way to notice a
/// terminal interrupt and answer `EINTR`.
///
/// One word, so a consumer takes the whole thing at once: 0 is nothing
/// pending, and otherwise bit 0 is the latch, bit 1 says the `SIGINT` has
/// still to be sent, and the rest is the VT the keystroke arrived on.
static CTRL_C_PENDING: AtomicU32 = AtomicU32::new(0);
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static SHIFT_DOWN: AtomicBool = AtomicBool::new(false);
/// AltGr (Alt derecho) — third XKB level on the console layout.
static ALTGR_DOWN: AtomicBool = AtomicBool::new(false);
/// Alt izquierdo — usado para la conmutación de VT (Ctrl+Alt+F1..F6).
static LEFT_ALT_DOWN: AtomicBool = AtomicBool::new(false);
static CAPSLOCK_ON: AtomicBool = AtomicBool::new(false);
/// Modo cursor de aplicación (DECCKM). Lo activa/desactiva la aplicación con
/// `ESC [ ? 1 h` / `ESC [ ? 1 l` (p. ej. el `smkx`/`rmkx` de ncurses). Cuando
/// está activo, las teclas de cursor emiten `ESC O x` en lugar de `ESC [ x`,
/// igual que `vc_decckm` en el VT de Linux.
static APP_CURSOR_KEYS: AtomicBool = AtomicBool::new(false);

/// A latched Ctrl-C, and what is left to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtrlCInterrupt {
    /// The VT the keystroke arrived on -- **not** the one on screen. A Ctrl-C
    /// typed on the serial console while the desktop holds the graphics VT
    /// belongs to the serial VT's foreground group, and answering with
    /// `active_vt()` signalled the desktop's group instead.
    pub vt: usize,
    /// Whether the `SIGINT` still has to be sent. The keystroke handler sends
    /// it itself whenever the VT has a foreground group, so this is false in
    /// the ordinary case: sending it again is how one keypress came to deliver
    /// two `SIGINT`s, which a handler ("press Ctrl-C twice to quit", a shell
    /// `trap`, python's `KeyboardInterrupt`) sees as two interrupts.
    pub signal_owed: bool,
}

/// Take the latch, if anything is latched.
pub fn ctrl_c_pending_take() -> Option<CtrlCInterrupt> {
    let state = CTRL_C_PENDING.swap(0, Ordering::SeqCst);
    (state & 1 != 0).then_some(CtrlCInterrupt {
        vt: (state >> 8) as usize,
        signal_owed: state & 2 != 0,
    })
}

/// Latch a Ctrl-C from VT `vt`. `signal_owed` says whether the caller has
/// already signalled the VT's foreground group.
pub fn ctrl_c_pending_set(vt: usize, signal_owed: bool) {
    let state = 1 | u32::from(signal_owed) << 1 | (vt_clamp(vt) as u32) << 8;
    CTRL_C_PENDING.store(state, Ordering::SeqCst);
    wake_tty_intr_waiters();
}

/// Non-consuming check for multiplex wait loops.
pub fn ctrl_c_pending_peek() -> bool {
    CTRL_C_PENDING.load(Ordering::SeqCst) & 1 != 0
}

lazy_static! {
    static ref TTY_INTR_WAKERS: Mutex<Vec<core::task::Waker>> = Mutex::new(Vec::new());
}

const MAX_TTY_INTR_WAKERS: usize = 64;

fn register_tty_waker_once(wakers: &mut Vec<core::task::Waker>, waker: &core::task::Waker) {
    if wakers.iter().any(|w| w.will_wake(waker)) {
        return;
    }
    if wakers.len() >= MAX_TTY_INTR_WAKERS {
        wakers.remove(0);
    }
    wakers.push(waker.clone());
}

pub fn wake_tty_intr_waiters() {
    let wakers: Vec<core::task::Waker> = core::mem::take(&mut *TTY_INTR_WAKERS.lock());
    for w in wakers {
        w.wake();
    }
}

pub fn register_tty_intr_waker(waker: core::task::Waker) {
    register_tty_waker_once(&mut TTY_INTR_WAKERS.lock(), &waker);
}

/// Keep THIS waker registered for the next sleep cycle without disturbing the
/// other waiters — see `kernel_hal::net::retain_net_rx_waker`, which had the
/// same inverted `retain` predicate and the same consequence: one waiter's
/// re-arm deleted everybody else's registration, so they lost their wakes and
/// fell back to the poll backstop. `wake_tty_intr_waiters` above takes the
/// whole list, so there is nothing to retain after a wake either.
pub fn retain_tty_intr_waker(waker: &core::task::Waker) {
    register_tty_waker_once(&mut TTY_INTR_WAKERS.lock(), waker);
}

/// Remove a wait's TTY-intr registration on Ready/`Drop` (see net clear_io_wait).
pub fn clear_tty_intr_waker(waker: &core::task::Waker) {
    TTY_INTR_WAKERS.lock().retain(|w| !w.will_wake(waker));
}

lazy_static! {
    /// One [`Stdin`] per virtual terminal. Building this also wires keyboard /
    /// UART input to the active terminal.
    pub static ref STDINS: Vec<Arc<Stdin>> = {
        let v: Vec<Arc<Stdin>> = (0..kernel_hal::console::NUM_VTS)
            .map(|i| Arc::new(Stdin::new(i)))
            .collect();

        // UART input goes to the serial-bound VT (the active VT in text mode,
        // or tty1 while the desktop holds the graphics VT -- see serial_stdin).
        if let Some(uart) = kernel_hal::drivers::all_uart().first() {
            uart.clone().subscribe(
                Box::new(move |_| {
                    while let Some(c) = uart.try_recv().unwrap_or(None) {
                        trace!("UART received byte: 0x{:02x}", c);
                        // Route serial input to the serial-bound VT, not the raw
                        // active VT: while the desktop holds the graphics VT the
                        // active VT is the compositor's, so serial keystrokes
                        // must reach the text shell (tty1) instead.
                        serial_stdin().push(c as char);
                    }
                }),
                false,
            );
        }

        // Keyboards (USB / virtio / PS2): translated + routed by `handle_key_event`.
        for input in kernel_hal::drivers::all_input().as_vec().iter() {
            input.subscribe(Box::new(handle_key_event), false);
        }
        v
    };
    /// One [`Stdout`] per virtual terminal.
    pub static ref STDOUTS: Vec<Arc<Stdout>> = (0..kernel_hal::console::NUM_VTS)
        .map(|i| Arc::new(Stdout { vt: i }))
        .collect();
    /// Backwards-compatible alias for the first VT's stdin.
    pub static ref STDIN: Arc<Stdin> = STDINS[0].clone();
    /// Backwards-compatible alias for the first VT's stdout.
    pub static ref STDOUT: Arc<Stdout> = STDOUTS[0].clone();
}

/// Stdin of the currently active virtual terminal.
fn active_stdin() -> Arc<Stdin> {
    STDINS[vt_clamp(kernel_hal::console::active_vt())].clone()
}

/// Stdin of the VT the serial line is bound to (see `console::serial_vt`): the
/// active VT in text mode, or `tty1` while the desktop holds the graphics VT, so
/// serial keystrokes reach the text shell instead of the compositor.
fn serial_stdin() -> Arc<Stdin> {
    STDINS[vt_clamp(kernel_hal::console::serial_vt())].clone()
}

/// Stdin of a specific virtual terminal.
pub fn vt_stdin(vt: usize) -> Arc<Stdin> {
    STDINS[vt_clamp(vt)].clone()
}

/// Stdout of a specific virtual terminal.
pub fn vt_stdout(vt: usize) -> Arc<Stdout> {
    STDOUTS[vt_clamp(vt)].clone()
}

/// Keyboard handler: tracks modifiers, switches VTs on Ctrl+Alt+F1..F6, handles
/// scrollback (Shift+PageUp/Down) and Ctrl+C, and feeds translated characters
/// to the active terminal.
fn handle_key_event(event: &InputEvent) {
    use zcore_drivers::input::input_event_codes::key::*;
    if event.event_type != InputEventType::Key {
        return;
    }
    // Linux input: value 1 = press, 0 = release, 2 = autorepeat. Track the
    // modifier state but don't return — the medium-raw path below has to emit
    // the modifier keycodes too.
    match event.code {
        KEY_LEFTCTRL | KEY_RIGHTCTRL => CTRL_DOWN.store(event.value != 0, Ordering::SeqCst),
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => SHIFT_DOWN.store(event.value != 0, Ordering::SeqCst),
        KEY_RIGHTALT => ALTGR_DOWN.store(event.value != 0, Ordering::SeqCst),
        KEY_LEFTALT => LEFT_ALT_DOWN.store(event.value != 0, Ordering::SeqCst),
        KEY_CAPSLOCK if event.value == 1 => {
            let on = CAPSLOCK_ON.load(Ordering::SeqCst);
            CAPSLOCK_ON.store(!on, Ordering::SeqCst);
        }
        _ => {}
    }

    // Ctrl+Alt+F1..F6 → switch virtual terminal. Checked before the medium-raw
    // hand-off below so the user can always leave an X session.
    if (event.value == 1 || event.value == 2)
        && CTRL_DOWN.load(Ordering::SeqCst)
        && LEFT_ALT_DOWN.load(Ordering::SeqCst)
    {
        // F7 reaches the dedicated graphics VT (tty7); F1..F6 the text consoles.
        let target = match event.code {
            KEY_F1 => Some(0),
            KEY_F2 => Some(1),
            KEY_F3 => Some(2),
            KEY_F4 => Some(3),
            KEY_F5 => Some(4),
            KEY_F6 => Some(5),
            KEY_F7 => Some(6),
            _ => None,
        };
        if let Some(n) = target {
            if n < kernel_hal::console::num_vts() {
                // Route through the VT_PROCESS handshake: when a graphics
                // session owns the current VT, this sends `relsig` and defers
                // the switch until the session releases (or a second press
                // forces it). `request_vt_switch` re-blits on arrival.
                request_vt_switch(n);
            }
            return;
        }
    }

    // While an X server holds the keyboard in K_MEDIUMRAW (kdrive/TinyX), feed
    // it raw keycodes straight off the console: one byte per event, the keycode
    // with bit 7 set on release. Every key matters — presses, releases and
    // modifiers — but not autorepeat (value 2): kdrive generates its own
    // repeat. Keycodes ≥ 128 don't fit kdrive's single-byte reader; skip them.
    if let Some(vt) = medium_raw_vt() {
        if event.code < 0x80 && (event.value == 0 || event.value == 1) {
            let byte = (event.code as u8 & 0x7f) | if event.value == 0 { 0x80 } else { 0 };
            vt_stdin(vt).push_bytes(&[byte]);
        }
        return;
    }

    if event.value != 1 && event.value != 2 {
        return;
    }

    // Shift+PageUp / Shift+PageDown scrollback on the active VT -- but only
    // while the kernel still owns the framebuffer. This block sits ABOVE the
    // KD_GRAPHICS gate further down, so with labwc holding the VT (it reads
    // the keyboard through evdev and never changes the tty's keyboard mode,
    // which is exactly why that gate exists) a Shift+PageUp typed inside a
    // terminal used to repaint the whole text console over the compositor.
    // Linux's scrollback is a text-console function and does nothing in
    // graphics mode either; the keystroke still reaches the terminal through
    // /dev/input/event*.
    if SHIFT_DOWN.load(Ordering::SeqCst)
        && kernel_hal::console::kd_mode_vt(kernel_hal::console::active_vt())
            == kernel_hal::console::KD_TEXT
    {
        if event.code == KEY_PAGEUP {
            kernel_hal::console::scroll_graphic_console(1);
            return;
        }
        if event.code == KEY_PAGEDOWN {
            kernel_hal::console::scroll_graphic_console(-1);
            return;
        }
    }

    // No entregamos caracteres "cocidos" al TTY cuando un servidor gráfico posee
    // el VT activo. Dos casos:
    //  * K_RAW/K_OFF (`KDSKBMODE`, p. ej. kdrive/X): el teclado ya está en crudo.
    //  * KD_GRAPHICS con el teclado aún en K_XLATE (labwc/wlroots vía libseat, que
    //    lee el teclado por evdev y NO cambia el modo del tty). Sin este segundo
    //    gate el kernel cocina cada tecla en el login-shell que comparte el VT con
    //    el compositor, de modo que un `vkcube` tecleado en foot se ejecuta DOS
    //    veces —una por foot vía evdev, otra por el shell del VT— y aparecen dos
    //    ventanas. Los eventos crudos siguen llegando por `/dev/input/event*`, que
    //    es de donde los lee el compositor. Los cambios de VT (Ctrl+Alt+Fn) ya se
    //    procesaron arriba, así que siempre se puede salir de la sesión gráfica.
    let active = kernel_hal::console::active_vt();
    if kernel_hal::console::kd_mode_vt(active) == kernel_hal::console::KD_GRAPHICS
        || !tty_kbd_cooked(active)
    {
        return;
    }

    // Modifier snapshot, equivalent to Linux's `shift_state`
    // (drivers/tty/vt/keyboard.c). Layout tables live in `kbd_layout`.
    let mods = KeyMods {
        shift: SHIFT_DOWN.load(Ordering::SeqCst),
        altgr: ALTGR_DOWN.load(Ordering::SeqCst),
        caps: CAPSLOCK_ON.load(Ordering::SeqCst),
        ctrl: CTRL_DOWN.load(Ordering::SeqCst),
    };

    // Traducción keycode -> keysym, al estilo de los tipos KT_LATIN / KT_CUR /
    // KT_FN del VT de Linux. Ctrl+letra produce su carácter de control (Ctrl+C
    // = 0x03, que el TTY interpreta como VINTR/SIGINT).
    let stdin = active_stdin();
    match translate_key(event.code, mods) {
        Some(KeySym::Char(c)) => stdin.push(c),
        Some(KeySym::Cursor(final_byte)) => {
            // applkey(): ESC O x en modo cursor de aplicación (DECCKM activo),
            // ESC [ x en modo normal — igual que `applkey()` en el VT de Linux.
            let mid = if APP_CURSOR_KEYS.load(Ordering::SeqCst) {
                b'O'
            } else {
                b'['
            };
            stdin.push_bytes(&[0x1b, mid, final_byte]);
        }
        Some(KeySym::Func(seq)) => stdin.push_bytes(seq),
        None => {}
    }
}

/// Resultado de traducir un keycode, análogo a los tipos de keysym del VT de
/// Linux (`drivers/tty/vt/keyboard.c`).
enum KeySym {
    /// Carácter imprimible (KT_LATIN/KT_LETTER). El bit de control ya está
    /// aplicado si correspondía.
    Char(char),
    /// Tecla de cursor (KT_CUR): se entrega la letra final (`A`..`D`, `H`, `F`).
    /// El emisor añade el prefijo `ESC O` (modo aplicación, DECCKM) o `ESC [`
    /// (modo normal), igual que `applkey()` en el kernel.
    Cursor(u8),
    /// Cadena fija de tecla de función / navegación (KT_FN): se emite tal cual.
    Func(&'static [u8]),
}

/// Traduce un keycode + modificadores a un keysym, replicando el modelo del VT
/// de Linux: teclas de cursor (KT_CUR), teclas de función con cadena fija
/// (KT_FN) y caracteres imprimibles (KT_LATIN) con el bit de control aplicado
/// como la columna `control` del keymap.
fn translate_key(code: u16, mods: KeyMods) -> Option<KeySym> {
    use zcore_drivers::input::input_event_codes::key::*;

    // Teclas de cursor: el prefijo lo decide el modo DECCKM al emitir.
    match code {
        KEY_UP => return Some(KeySym::Cursor(b'A')),
        KEY_DOWN => return Some(KeySym::Cursor(b'B')),
        KEY_RIGHT => return Some(KeySym::Cursor(b'C')),
        KEY_LEFT => return Some(KeySym::Cursor(b'D')),
        KEY_HOME => return Some(KeySym::Cursor(b'H')),
        KEY_END => return Some(KeySym::Cursor(b'F')),
        // Teclas con cadena fija (no dependen de DECCKM).
        KEY_PAGEUP => return Some(KeySym::Func(b"\x1b[5~")),
        KEY_PAGEDOWN => return Some(KeySym::Func(b"\x1b[6~")),
        KEY_INSERT => return Some(KeySym::Func(b"\x1b[2~")),
        KEY_DELETE => return Some(KeySym::Func(b"\x1b[3~")),
        KEY_ESC => return Some(KeySym::Func(b"\x1b")),
        _ => {}
    }

    let c = kbd_layout::to_char(code, mods)?;

    // Bit de control (KG_CTRL). El kernel toma el carácter de la columna
    // `control` del keymap; para el rango ASCII relevante equivale a estas
    // reglas: letras y `@ [ \ ] ^ _ ? espacio` -> carácter de control.
    if mods.ctrl {
        let ctrl_c = match c {
            ' ' | '@' => Some('\u{0}'), // NUL
            '?' => Some('\u{7f}'),      // DEL
            '[' => Some('\u{1b}'),      // ESC
            '\\' => Some('\u{1c}'),     // FS
            ']' => Some('\u{1d}'),      // GS
            '^' => Some('\u{1e}'),      // RS
            '_' => Some('\u{1f}'),      // US
            c if c.is_ascii_alphabetic() => Some(((c.to_ascii_uppercase() as u8) & 0x1f) as char),
            _ => None,
        };
        if let Some(ctrl_c) = ctrl_c {
            return Some(KeySym::Char(ctrl_c));
        }
    }

    Some(KeySym::Char(c))
}

/// Map a Linux VT keycode (`kb_index` in `struct kbentry`) to evdev `KEY_*`.
fn linux_keycode_to_evdev(kc: u8) -> Option<u16> {
    use zcore_drivers::input::input_event_codes::key::*;
    match kc {
        1 => Some(KEY_ESC),
        2 => Some(KEY_1),
        3 => Some(KEY_2),
        4 => Some(KEY_3),
        5 => Some(KEY_4),
        6 => Some(KEY_5),
        7 => Some(KEY_6),
        8 => Some(KEY_7),
        9 => Some(KEY_8),
        10 => Some(KEY_9),
        11 => Some(KEY_0),
        12 => Some(KEY_MINUS),
        13 => Some(KEY_EQUAL),
        14 => Some(KEY_BACKSPACE),
        15 => Some(KEY_TAB),
        16 => Some(KEY_Q),
        17 => Some(KEY_W),
        18 => Some(KEY_E),
        19 => Some(KEY_R),
        20 => Some(KEY_T),
        21 => Some(KEY_Y),
        22 => Some(KEY_U),
        23 => Some(KEY_I),
        24 => Some(KEY_O),
        25 => Some(KEY_P),
        26 => Some(KEY_LEFTBRACE),
        27 => Some(KEY_RIGHTBRACE),
        28 => Some(KEY_ENTER),
        30 => Some(KEY_A),
        31 => Some(KEY_S),
        32 => Some(KEY_D),
        33 => Some(KEY_F),
        34 => Some(KEY_G),
        35 => Some(KEY_H),
        36 => Some(KEY_J),
        37 => Some(KEY_K),
        38 => Some(KEY_L),
        39 => Some(KEY_SEMICOLON),
        40 => Some(KEY_APOSTROPHE),
        41 => Some(KEY_GRAVE),
        43 => Some(KEY_BACKSLASH),
        44 => Some(KEY_Z),
        45 => Some(KEY_X),
        46 => Some(KEY_C),
        47 => Some(KEY_V),
        48 => Some(KEY_B),
        49 => Some(KEY_N),
        50 => Some(KEY_M),
        51 => Some(KEY_COMMA),
        52 => Some(KEY_DOT),
        53 => Some(KEY_SLASH),
        57 => Some(KEY_SPACE),
        59 => Some(KEY_F1),
        60 => Some(KEY_F2),
        61 => Some(KEY_F3),
        62 => Some(KEY_F4),
        63 => Some(KEY_F5),
        64 => Some(KEY_F6),
        65 => Some(KEY_F7),
        66 => Some(KEY_F8),
        67 => Some(KEY_F9),
        68 => Some(KEY_F10),
        87 => Some(KEY_F11),
        88 => Some(KEY_F12),
        _ => None,
    }
}

/// Build the `kb_value` for a `KDGKBENT` query (kdrive/TinyX keymap loader).
fn kdgkbent_value(keycode: u16, table: u8) -> u16 {
    use zcore_drivers::input::input_event_codes::key::*;
    const KT_LATIN: u16 = 0;
    const KT_SPEC: u16 = 2;
    const KT_CUR: u16 = 6;
    const KT_SHIFT: u16 = 7;
    const NO_SYMBOL: u16 = 0;
    const K_ENTER: u16 = (KT_SPEC << 8) | 1;
    const K_SHIFT: u16 = KT_SHIFT << 8;
    const K_ALTGR: u16 = (KT_SHIFT << 8) | 1;
    const K_CTRL: u16 = (KT_SHIFT << 8) | 2;
    const K_ALT: u16 = (KT_SHIFT << 8) | 3;
    const K_DOWN: u16 = KT_CUR << 8;
    const K_LEFT: u16 = (KT_CUR << 8) | 1;
    const K_RIGHT: u16 = (KT_CUR << 8) | 2;
    const K_UP: u16 = (KT_CUR << 8) | 3;

    match keycode {
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => return K_SHIFT,
        KEY_LEFTCTRL | KEY_RIGHTCTRL => return K_CTRL,
        KEY_LEFTALT => return K_ALT,
        KEY_RIGHTALT => return K_ALTGR,
        KEY_ENTER | KEY_KPENTER => return K_ENTER,
        KEY_ESC => return (KT_LATIN << 8) | 0x1b,
        KEY_UP => return K_UP,
        KEY_DOWN => return K_DOWN,
        KEY_LEFT => return K_LEFT,
        KEY_RIGHT => return K_RIGHT,
        _ => {}
    }

    let mods = KeyMods {
        shift: table & 1 != 0,
        altgr: table & 2 != 0,
        caps: false,
        ctrl: false,
    };
    match translate_key(keycode, mods) {
        Some(KeySym::Char(c)) if (c as u32) <= 0xff => c as u32 as u16,
        _ => NO_SYMBOL,
    }
}

/// Stdin struct, for Stdin buffer.
///
/// Design: `push()` is called from IRQ-handler callbacks (UART / xHCI HID).
/// To avoid deep nested spinlock chains from interrupt context (which caused
/// deadlocks after ~20-30 keystrokes), `push()` only touches the buffer lock
/// and sets an atomic flag — it does NOT touch the EventBus.  The EventBus
/// notification happens lazily from the executor side (SerialFuture / pop).
/// This is aligned with the Eclipse OS 1 pattern (usb_hid.rs → push_key),
/// where the ISR only writes to a circular buffer with interrupts disabled.
pub struct Stdin {
    /// Index of the virtual terminal this stdin belongs to.
    vt: usize,
    /// Bytes a reader may take. **Bytes, not characters**: this is what
    /// `read(2)` hands over, and the system speaks UTF-8, so a key that is not
    /// ASCII goes in as the two to four bytes that spell it. The line being
    /// edited lives in `canon_buf` as `char`s instead, because erasing works
    /// on characters and not on the bytes they are made of.
    buf: Mutex<VecDeque<u8>>,
    canon_buf: Mutex<VecDeque<char>>,
    eventbus: Mutex<EventBus>,
    /// Atomic flag set by `push()` so `SerialFuture` can detect new data
    /// without requiring `eventbus.lock()` from the IRQ path.
    data_ready: core::sync::atomic::AtomicBool,
    /// `VLNEXT` (literal-next, Ctrl-V) latch: when set, the next character is
    /// inserted verbatim, bypassing signal and line-editing processing.
    lnext: core::sync::atomic::AtomicBool,
    /// Canonical VEOF on an empty line: next `read` returns 0 (EOF).
    eof_pending: AtomicBool,
    /// Non-canonical `VMIN=0,VTIME>0`: monotonic deadline (ns) after which an
    /// empty read returns `Ok(0)`. Zero means no timer armed.
    vtime_deadline_ns: AtomicU64,
    /// Non-canonical `VMIN`: how many queued bytes would end the read that
    /// parked, already lowered to what its buffer can take. The wait sees the
    /// queue but not the buffer, so the read leaves the number behind for it.
    read_need: core::sync::atomic::AtomicUsize,
}

/// The line being edited, in bytes.
///
/// `canon_buf` holds `char`s -- erasing works on characters and not on the
/// bytes that spell them -- but the bound in `ioctl.rs` is a buffer size, so
/// the line has to be measured in what it would occupy. Walked rather than
/// tracked alongside: the line is what one person has typed since the last
/// Enter, and a counter kept in step across VERASE, VWERASE, VKILL and the
/// commit is a counter that drifts.
/// Append one character to a reader's queue as the bytes that spell it.
///
/// The queue is what `read(2)` hands over, and this system speaks UTF-8, so a
/// character outside ASCII is two to four bytes there. Casting it to one byte
/// instead — which is what this did — writes Latin-1 into a UTF-8 world: on the
/// Spanish layout `ñ` came out as the single byte `0xf1`, which is not valid
/// UTF-8 at all, and `€` (U+20AC) came out as `0xac`, which is the byte for
/// `¬`, so two different keys arrived as the same one.
///
/// The character is never split: a reader with room for one byte gets that one
/// byte and the rest on its next read, which is what a byte queue gives for
/// free and what `n_tty` does too.
fn editing_bytes(canon: &VecDeque<char>) -> usize {
    canon.iter().map(|c| c.len_utf8()).sum()
}

fn push_utf8(queue: &mut VecDeque<u8>, c: char) {
    let mut buf = [0u8; 4];
    for &b in c.encode_utf8(&mut buf).as_bytes() {
        queue.push_back(b);
    }
}

impl Stdin {
    /// The virtual terminal this stdin is bound to.
    pub fn vt(&self) -> usize {
        self.vt
    }

    fn new(vt: usize) -> Self {
        Self {
            vt,
            buf: Mutex::new(VecDeque::new()),
            canon_buf: Mutex::new(VecDeque::new()),
            eventbus: Mutex::new(EventBus::default()),
            data_ready: core::sync::atomic::AtomicBool::new(false),
            lnext: core::sync::atomic::AtomicBool::new(false),
            eof_pending: AtomicBool::new(false),
            vtime_deadline_ns: AtomicU64::new(0),
            read_need: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// True when the blocking reader parked in `read_at` can make progress.
    ///
    /// `File::read` is `loop { read_at; on EAGAIN await the wait }`, so this
    /// and `read_at` have to give the same answer to the same queue. Answering
    /// `can_read()` here — which is what this did — spun the CPU at full tilt
    /// between the first byte and the `VMIN`th under `stty -icanon min 4`:
    /// `read_at` said not yet, the wait said ready, round again with nothing
    /// changed.
    fn read_ready(&self) -> bool {
        if self.eof_pending.load(Ordering::Acquire) {
            return true;
        }
        let termios = *tty_termios(self.vt).lock();
        if termios.canonical() {
            return self.can_read();
        }
        if termios.vmin() == 0 && termios.vtime() == 0 {
            // A read that may come back with nothing is never not ready.
            return true;
        }
        if self.buf.lock().len() >= self.read_need.load(Ordering::Relaxed).max(1) {
            return true;
        }
        self.vtime_expired()
    }

    /// `poll(2)`'s answer, which is not the blocking read's.
    ///
    /// Linux's `n_tty_poll` asks `input_available_p` for
    /// `TIME_CHAR ? 0 : MIN_CHAR` bytes: with a timer running, or with no
    /// minimum, a read is going to come back shortly whatever is queued, so
    /// the terminal counts as readable. The wait above may not say that — it
    /// would spin — which is why the two are separate.
    fn poll_ready(&self) -> bool {
        if self.can_read() || self.eof_pending.load(Ordering::Acquire) {
            return true;
        }
        let termios = *tty_termios(self.vt).lock();
        if termios.canonical() {
            return false;
        }
        termios.vmin() == 0 || termios.vtime() > 0
    }

    /// True when the `VTIME` timer armed for the read in progress has run out.
    fn vtime_expired(&self) -> bool {
        let dl = self.vtime_deadline_ns.load(Ordering::Acquire);
        dl != 0 && kernel_hal::timer::timer_now().as_nanos() as u64 >= dl
    }

    /// Start the `VTIME` timer for a read that is about to wait, if it asked
    /// for one and has not started it already.
    ///
    /// With `VMIN > 0` the timer measures the gap *between* bytes, so it does
    /// not start until the first one is in.
    fn arm_vtime(&self, termios: &Termios, queued: usize) {
        if termios.vtime() == 0 || (termios.vmin() > 0 && queued == 0) {
            return;
        }
        if self.vtime_deadline_ns.load(Ordering::Acquire) != 0 {
            return;
        }
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        // VTIME is in deciseconds.
        let deadline = now.saturating_add((termios.vtime() as u64) * 100_000_000);
        self.vtime_deadline_ns.store(deadline, Ordering::Release);
    }

    /// Echo a run through the terminal's own output rules, moving its cursor.
    ///
    /// Echoed bytes go through the same `c_oflag` as program output and share
    /// its column: `__process_echoes` hands each byte it is about to put out
    /// to `do_output_char` whenever `OPOST` is set (`drivers/tty/n_tty.c`).
    ///
    /// The only way out, now. There used to be a raw one beside it that wrote
    /// to the console directly, and ten echoes went that way: the erase
    /// sequences of `VERASE`, `VWERASE` and `VKILL`, the `^R` reprint, and the
    /// `^C`, `^\` and `^Z` labels. Every one of those is cursor movement --
    /// `\x08 \x08` is "back, blank, back" -- and the column they moved was not
    /// the one the terminal keeps, so after a single backspace the kernel's idea
    /// of where the cursor sat was one column off, and stayed off. It is that
    /// column that `ONOCR` and `ONLRET` are defined in terms of and that a `\t`
    /// in the program's own output expands against, so a tab printed after the
    /// user had edited a line landed at the wrong stop.
    fn echo_post(&self, s: &str) {
        tty_post_out(self.vt, s.as_bytes());
    }

    fn echo_char(&self, c: char) {
        let termios = *tty_termios(self.vt).lock();
        let echo = termios.c_lflag & ECHO != 0;
        let echoctl = termios.c_lflag & ECHOCTL != 0;

        if !echo {
            return;
        }

        match c {
            '\u{8}' | '\u{7f}' => {
                self.echo_post("\x08 \x08");
            }
            c if c.is_control() && c != '\n' && c != '\r' && c != '\t' => {
                if echoctl {
                    let mut s = [0u8; 2];
                    s[0] = b'^';
                    s[1] = (c as u8 + 64) & 0x7f;
                    if let Ok(s_str) = core::str::from_utf8(&s) {
                        self.echo_post(s_str);
                    }
                }
            }
            c => {
                let mut buf = [0u8; 4];
                self.echo_post(c.encode_utf8(&mut buf));
            }
        }
    }

    /// Mark new data available and wake any blocked reader / poller. Mirrors the
    /// notification dance used by the canonical/raw paths in [`push`].
    fn wake_readers(&self) {
        self.data_ready.store(true, Ordering::Release);
        if let Some(mut eb) = self.eventbus.try_lock() {
            self.data_ready.store(false, Ordering::Relaxed);
            eb.set(Event::READABLE);
        } else {
            wake_tty_intr_waiters();
        }
    }

    /// `VWERASE` (word erase, Ctrl-W): drop trailing whitespace, then the
    /// preceding word, from the pending canonical line, echoing the erase.
    fn word_erase(&self, lflag: u32) {
        let echo = lflag & ECHO != 0;
        let mut canon = self.canon_buf.lock();
        // Skip any trailing blanks first.
        while matches!(canon.back(), Some(' ') | Some('\t')) {
            canon.pop_back();
            if echo {
                self.echo_post("\x08 \x08");
            }
        }
        // Then erase the word itself, up to (not including) the next blank.
        while let Some(&ch) = canon.back() {
            if ch == ' ' || ch == '\t' {
                break;
            }
            canon.pop_back();
            if echo {
                self.echo_post("\x08 \x08");
            }
        }
    }

    /// `VREPRINT` (reprint, Ctrl-R): redraw the pending canonical line on a
    /// fresh line so the user can see input after noise (e.g. a kernel message).
    fn reprint(&self, lflag: u32) {
        if lflag & ECHO == 0 {
            return;
        }
        if lflag & ECHOCTL != 0 {
            self.echo_post("^R");
        }
        self.echo_post("\r\n");
        let canon = self.canon_buf.lock();
        let mut buf = [0u8; 4];
        for &ch in canon.iter() {
            self.echo_post(ch.encode_utf8(&mut buf));
        }
    }

    /// Put one character in front of a reader, if there is room for it.
    ///
    /// Returns whether it went in. Canonical mode adds it to the line being
    /// edited, raw mode makes it readable at once, and both share one bound
    /// ([`input_room`]): they are two indices into one buffer in `n_tty`.
    ///
    /// Every keystroke that ends up queued comes through here, which is what
    /// makes the bound a bound -- one path pushing straight onto a queue is
    /// enough to lose it.
    ///
    /// What this end cannot do is the third of the rule's three answers. A
    /// keyboard has no writer to hand a short count to and no way to be asked
    /// to wait, so `Full` is treated like `Process`: everything is still
    /// interpreted -- the signal characters above all, since Ctrl-C is how a
    /// user rescues the program that stopped reading in the first place, and
    /// losing it here would be losing it exactly when it is needed -- and only
    /// the storing stops. A character goes in whole or not at all, so a queue
    /// can end up to three bytes over the bound; what matters is that it stops.
    fn store_char(&self, c: char, canonical: bool) -> bool {
        let mut canon = self.canon_buf.lock();
        let mut buf = self.buf.lock();
        if !has_input_room(buf.len(), editing_bytes(&canon)) {
            return false;
        }
        if canonical {
            canon.push_back(c);
        } else {
            push_utf8(&mut buf, c);
        }
        true
    }

    /// Insert a character verbatim (used after `VLNEXT`): no signal/edit
    /// interpretation. In canonical mode it joins the pending line; in raw mode
    /// it becomes immediately readable.
    fn input_literal(&self, c: char, lflag: u32) {
        let canonical = lflag & ICANON != 0;
        if !self.store_char(c, canonical) {
            return;
        }
        self.echo_char(c);
        if !canonical {
            self.wake_readers();
        }
    }

    /// Push a char into the Stdin buffer.
    ///
    /// Safe to call from IRQ context: acquires `buf` lock briefly (with
    /// interrupts disabled by the spinlock), sets an atomic flag, and
    /// *tries* to propagate to the EventBus via try_lock().  If the
    /// EventBus is contended the flag is left set for the next
    /// executor-side flush_ready_flag() call.
    pub fn push(&self, mut c: char) {
        // Copied out, so the lock is gone by the end of this statement.
        let t = *tty_termios(self.vt).lock();
        let iflag = t.c_iflag;
        let lflag = t.c_lflag;
        let c_cc = t.c_cc;
        let kill_echo = t.kill_echo();

        // 1. What the character becomes on the way in: `I_ISTRIP`, `I_IUCLC`,
        //    then the CR/NL rules, in that order. The block used to be here,
        //    again in `fs/pty.rs` and again in `fs/devfs/pty.rs`, and none of
        //    the three had `I_ISTRIP` or `I_IUCLC`; it is one question per input
        //    byte, so it is answered in `ioctl.rs`.
        //
        //    A terminal's unit is the byte everywhere but here, where the
        //    queue is `char`. A scalar that does not fit in one is left
        //    alone: no 7- or 8-bit terminal can send it, and `I_ISTRIP` over a
        //    UTF-8 sequence is not a character this queue could hold. `\r`
        //    and `\n` are ASCII, so the CR/NL half sees everything either
        //    way.
        if let Ok(b) = u8::try_from(c as u32) {
            match t.input_char(b) {
                Some(b) => c = b as char,
                None => return,
            }
        }

        // 1b. Literal-next (VLNEXT): the previous keystroke was Ctrl-V, so take
        // this character verbatim, bypassing signal and line-editing handling.
        if self.lnext.swap(false, Ordering::Relaxed) {
            self.input_literal(c, lflag);
            return;
        }

        // 1c. Software flow control (IXON): VSTOP (Ctrl-S) pauses console output
        // and VSTART (Ctrl-Q) resumes it. These bytes are consumed, never
        // delivered. With IXANY, any input byte resumes paused output.
        if iflag & IXON != 0 {
            let state = &TTY_STATES[vt_clamp(self.vt)].flow_stopped;
            if cc_match(&c_cc, VSTOP, c as u8) {
                state.store(true, Ordering::Relaxed);
                return;
            }
            if cc_match(&c_cc, VSTART, c as u8) {
                state.store(false, Ordering::Relaxed);
                return;
            }
            if iflag & IXANY != 0 && state.swap(false, Ordering::Relaxed) {
                // Resumed by this byte; fall through to process it normally.
            }
        }

        // 1d. Discard (VDISCARD, Ctrl-O): toggles output flushing. The console
        // has no output queue to drop, so just consume the byte when IEXTEN is
        // on so it doesn't leak into the cooked line.
        if lflag & L_IEXTEN != 0 && c_cc[VDISCARD] != 0 && c as u8 == c_cc[VDISCARD] {
            return;
        }

        // 2. Signals
        if lflag & ISIG != 0 {
            // A job-control signal (Ctrl-C/Ctrl-\/Ctrl-Z) also lifts an IXON
            // output freeze, so the signalled process can run, print, or die
            // instead of staying blocked behind a Ctrl-S.
            if cc_match(&c_cc, VINTR, c as u8)
                || cc_match(&c_cc, VQUIT, c as u8)
                || cc_match(&c_cc, VSUSP, c as u8)
            {
                TTY_STATES[vt_clamp(self.vt)]
                    .flow_stopped
                    .store(false, Ordering::Relaxed);
            }
            if cc_match(&c_cc, VINTR, c as u8) {
                let pgid = tty_fg_pgrp(self.vt).load(Ordering::Relaxed);
                let vt_i = vt_clamp(self.vt);
                let sent = if pgid > 0 {
                    // First Ctrl-C → SIGINT; second for the same pgrp → SIGKILL
                    // so a hung glxgears/etc. can be torn down without closing
                    // the terminal.
                    crate::process::interrupt_or_force_pgrp(
                        pgid,
                        &TTY_STATES[vt_i].ctrl_c_armed_pgid,
                    )
                } else {
                    crate::signal::Signal::SIGINT
                };
                // The latch is for the `EINTR` and the wakeup. The signal has
                // gone out already unless this VT has no foreground group,
                // which is the only case the consumer still has to deliver.
                ctrl_c_pending_set(self.vt, pgid <= 0);
                if lflag & NOFLSH == 0 {
                    self.buf.lock().clear();
                    self.canon_buf.lock().clear();
                    self.eof_pending.store(false, Ordering::Release);
                }
                if lflag & ECHO != 0 {
                    if sent == crate::signal::Signal::SIGKILL {
                        self.echo_post("^C (killed)\n");
                    } else {
                        self.echo_post("^C\n");
                    }
                }
                // Wake waiters without latching READABLE on an empty queue
                // (that caused busy-loops in blocking `read`).
                if let Some(mut eb) = self.eventbus.try_lock() {
                    eb.clear(Event::READABLE);
                }
                wake_tty_intr_waiters();
                return;
            }
            if cc_match(&c_cc, VQUIT, c as u8) {
                let pgid = tty_fg_pgrp(self.vt).load(Ordering::Relaxed);
                if pgid > 0 {
                    let _ = crate::process::send_signal_to_pgrp(
                        pgid as usize,
                        crate::signal::Signal::SIGQUIT,
                    );
                }
                if lflag & NOFLSH == 0 {
                    self.buf.lock().clear();
                    self.canon_buf.lock().clear();
                    self.eof_pending.store(false, Ordering::Release);
                }
                if lflag & ECHO != 0 {
                    self.echo_post("^\\\n");
                }
                if let Some(mut eb) = self.eventbus.try_lock() {
                    eb.clear(Event::READABLE);
                }
                wake_tty_intr_waiters();
                return;
            }
            if cc_match(&c_cc, VSUSP, c as u8) {
                let pgid = tty_fg_pgrp(self.vt).load(Ordering::Relaxed);
                if pgid > 0 {
                    let _ = crate::process::send_signal_to_pgrp(
                        pgid as usize,
                        crate::signal::Signal::SIGTSTP,
                    );
                }
                if lflag & NOFLSH == 0 {
                    self.buf.lock().clear();
                    self.canon_buf.lock().clear();
                    self.eof_pending.store(false, Ordering::Release);
                }
                if lflag & ECHO != 0 {
                    self.echo_post("^Z\n");
                }
                if let Some(mut eb) = self.eventbus.try_lock() {
                    eb.clear(Event::READABLE);
                }
                wake_tty_intr_waiters();
                return;
            }
        }

        // 3. Canon vs Raw mode
        if lflag & ICANON != 0 {
            // Extended line editing (VWERASE / VREPRINT / VLNEXT) is gated on
            // IEXTEN, as in Linux n_tty. A c_cc of 0 means the char is disabled.
            let iexten = lflag & L_IEXTEN != 0;
            if iexten && c_cc[VWERASE] != 0 && c as u8 == c_cc[VWERASE] {
                self.word_erase(lflag);
            } else if iexten && c_cc[VREPRINT] != 0 && c as u8 == c_cc[VREPRINT] {
                self.reprint(lflag);
            } else if iexten && c_cc[VLNEXT] != 0 && c as u8 == c_cc[VLNEXT] {
                self.lnext.store(true, Ordering::Relaxed);
                if lflag & ECHO != 0 && lflag & ECHOCTL != 0 {
                    // Show "^" with the cursor parked on it until the quoted
                    // char arrives (Linux echoes ^ then a backspace).
                    self.echo_post("^\x08");
                }
            } else if cc_match(&c_cc, VERASE, c as u8) {
                let mut canon = self.canon_buf.lock();
                if let Some(_popped) = canon.pop_back() {
                    if lflag & ECHO != 0 {
                        if lflag & ECHOE != 0 {
                            self.echo_post("\x08 \x08");
                        } else {
                            let mut buf = [0u8; 4];
                            let erase_char = (c_cc[VERASE] as char).encode_utf8(&mut buf);
                            self.echo_post(erase_char);
                        }
                    }
                }
            } else if cc_match(&c_cc, VKILL, c as u8) {
                let mut canon = self.canon_buf.lock();
                let len = canon.len();
                canon.clear();
                match kill_echo {
                    KillEcho::Rubout => {
                        for _ in 0..len {
                            self.echo_post("\x08 \x08");
                        }
                    }
                    KillEcho::Newline => self.echo_post("\n"),
                    KillEcho::Nothing => {}
                }
            } else if cc_match(&c_cc, VEOF, c as u8) {
                let mut canon = self.canon_buf.lock();
                let empty = canon.is_empty();
                let mut buf = self.buf.lock();
                while let Some(ch) = canon.pop_front() {
                    push_utf8(&mut buf, ch);
                }
                drop(buf);
                drop(canon);
                if empty {
                    // Empty line + VEOF → EOF for the next reader (Linux n_tty).
                    self.eof_pending.store(true, Ordering::Release);
                }
                self.wake_readers();
            } else {
                let stored = self.store_char(c, true);
                self.echo_char(c);
                // A line is delivered to readers on newline or on either of the
                // configurable end-of-line delimiters (VEOL / VEOL2).
                //
                // Not on one that did not fit: a line that cannot hold its own
                // terminator is not a line to hand the reader, and the user
                // still has VERASE and VKILL to shorten it with.
                let is_eol = c == '\n'
                    || (c_cc[VEOL] != 0 && c as u8 == c_cc[VEOL])
                    || (c_cc[VEOL2] != 0 && c as u8 == c_cc[VEOL2]);
                if is_eol && stored {
                    let mut canon = self.canon_buf.lock();
                    let mut buf = self.buf.lock();
                    while let Some(ch) = canon.pop_front() {
                        push_utf8(&mut buf, ch);
                    }
                    self.wake_readers();
                }
            }
        } else {
            // Raw mode
            self.store_char(c, false);
            self.echo_char(c);
            // Wake readers
            self.data_ready.store(true, Ordering::Release);
            if let Some(mut eb) = self.eventbus.try_lock() {
                self.data_ready.store(false, Ordering::Relaxed);
                eb.set(Event::READABLE);
            } else {
                wake_tty_intr_waiters();
            }
        }
    }

    /// Drain the atomic flag and propagate to EventBus.
    /// Called from executor context (SerialFuture::poll, pop, executor loop).
    pub fn flush_ready_flag(&self) {
        if self.data_ready.swap(false, Ordering::Acquire) {
            self.eventbus.lock().set(Event::READABLE);
        }
    }

    /// pop a byte from the Stdin buffer
    pub fn pop(&self) -> u8 {
        self.flush_ready_flag();
        let mut buf_lock = self.buf.lock();
        let c = buf_lock.pop_front().unwrap();
        if buf_lock.is_empty() {
            self.eventbus.lock().clear(Event::READABLE);
        }
        c
    }

    /// specify whether the Stdin buffer is readable
    pub fn can_read(&self) -> bool {
        !self.buf.lock().is_empty()
    }

    /// Push raw bytes into stdin without echo (TTY query responses for userland).
    ///
    /// Bounded like every other way into the queue, and this is the one a
    /// program can drive on its own: a `\x1b[5n` written to its **own stdout**
    /// makes the kernel answer `\x1b[0n` here. A loop of those, never reading
    /// stdin, is an unprivileged process asking the kernel for memory with no
    /// keyboard and no privilege involved. There is nobody to hand a short
    /// count to -- the caller is the kernel answering a query -- so the only
    /// thing the bound can do is stop.
    pub fn push_bytes(&self, bytes: &[u8]) {
        let canon = self.canon_buf.lock();
        let editing = editing_bytes(&canon);
        let mut buf = self.buf.lock();
        for &b in bytes {
            if !has_input_room(buf.len(), editing) {
                break;
            }
            buf.push_back(b);
        }
        drop(buf);
        drop(canon);
        self.data_ready.store(true, Ordering::Release);
        if let Some(mut eb) = self.eventbus.try_lock() {
            self.data_ready.store(false, Ordering::Relaxed);
            eb.set(Event::READABLE);
        } else {
            wake_tty_intr_waiters();
        }
    }
}

/// Helper function to post-process output data (e.g. translating \n to \r\n if OPOST and ONLCR are set)
fn tty_write_out(vt: usize, buf: &[u8]) {
    // Honor software flow control: while output is stopped by VSTOP (Ctrl-S),
    // block here until a VSTART (Ctrl-Q) keystroke clears the flag. Keyboard
    // IRQs keep firing while we spin, so the flag can still be cleared. Bail out
    // on a pending Ctrl-C so a process blocked on a frozen terminal stays
    // killable (the SIGINT will be handled once this syscall returns).
    while tty_flow_stopped(vt) {
        if ctrl_c_pending_peek() {
            break;
        }
        core::hint::spin_loop();
    }
    tty_post_out(vt, buf);
}

/// Put `buf` on the VT through the terminal's `c_oflag` rules, moving its
/// cursor. Unlike [`tty_write_out`] this does **not** wait on software flow
/// control, because the echo path calls it: a Ctrl-S arrives on the same path
/// as the Ctrl-Q that would release it, so an echo that waited for the flow to
/// resume would be waiting for itself.
fn tty_post_out(vt: usize, buf: &[u8]) {
    let termios = *tty_termios(vt).lock();
    let mut col = tty_column(vt).load(Ordering::Relaxed);

    // Post-process into a staging buffer rather than a call per byte, and
    // prefer to flush it where a character ends, so the console is handed
    // whole characters.
    let mut staged = [0u8; 256];
    let mut n = 0;
    for &b in buf {
        let out = termios.output_char(b, &mut col);
        let out = out.as_bytes();
        // Two rules, and the second one is what the old single rule got
        // wrong. "Nearly full and at a character boundary" is the one that
        // keeps a character whole; "would not fit" is the one that guarantees
        // room, because a boundary may never come: a continuation byte
        // (0x80..=0xbf) is not a control byte, so `output_char` hands it back
        // as itself and the boundary test says "not here". Eight orphan
        // continuation bytes in a row -- `cat` on any binary file, or on
        // /dev/urandom -- walked `n` from 248 to 256 without ever flushing
        // and indexed off the end of the array: a kernel panic in the console
        // write path, from an unprivileged `write(1, ...)`.
        if n + out.len() > staged.len() || (n + 8 > staged.len() && !utf8_continuation(b)) {
            tty_write_staged(vt, &staged[..n]);
            n = 0;
        }
        staged[n..n + out.len()].copy_from_slice(out);
        n += out.len();
    }
    tty_column(vt).store(col, Ordering::Relaxed);
    tty_write_staged(vt, &staged[..n]);
}

/// Write `bytes` to VT `vt` as text, without taking it on faith that they are
/// valid UTF-8.
///
/// `tty_post_out` used to hand its staging buffer to `from_utf8_unchecked`,
/// which is undefined behaviour for anything a program actually writes to a
/// terminal: a lone byte from 0x80 up is not a character, and every `cat` of a
/// binary file, every mis-encoded log line and every byte of `/dev/urandom` is
/// made of them. What a terminal does with one is show a replacement glyph, so
/// that is what this does -- the valid run, then U+FFFD for the byte that is
/// not part of a character, then on with the rest.
fn tty_write_staged(vt: usize, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let (good, skip) = match core::str::from_utf8(bytes) {
            Ok(_) => (bytes.len(), 0),
            // `error_len() == None` is a character cut short by the end of the
            // buffer. There is no state kept between calls to finish it with,
            // and the caller only cuts at a boundary it chose, so the tail is
            // as unfinished as any other broken sequence.
            Err(e) => (e.valid_up_to(), e.error_len().unwrap_or(usize::MAX)),
        };
        if good > 0 {
            // SAFETY: `from_utf8` has just said that these bytes decode.
            let s = unsafe { core::str::from_utf8_unchecked(&bytes[..good]) };
            kernel_hal::console::vt_console_write_str(vt, s);
        }
        if good == bytes.len() {
            return;
        }
        kernel_hal::console::vt_console_write_str(vt, "\u{fffd}");
        // `skip` is at least one, so this always makes progress.
        bytes = &bytes[(good + skip.max(1)).min(bytes.len())..];
    }
}

/// Track DEC private modes and answer DSR status (`CSI 5 n`).
///
/// Do **not** synthesize Cursor Position Reports for `CSI 6 n`: QEMU `-serial`
/// stdio/pty forwards those to the host terminal, which replies with the real
/// size. Injecting `\e[1;1R` races ahead of that reply, so `/etc/profile`'s
/// resize probe runs `stty rows 1 cols 1` and full-screen apps (nano) exit.
fn tty_handle_outgoing(vt: usize, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let mut need_status = false;
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            // CSI ? Pm (h|l): modos privados DEC. Rastreamos DECCKM (modo 1)
            // para alternar el prefijo de las teclas de cursor, como el VT
            // de Linux con `\E[?1h` (smkx) / `\E[?1l` (rmkx).
            if i + 2 < data.len() && data[i + 2] == b'?' {
                let mut j = i + 3;
                let mut num: u32 = 0;
                let mut has_digit = false;
                while j < data.len() && data[j].is_ascii_digit() {
                    num = num.saturating_mul(10) + (data[j] - b'0') as u32;
                    has_digit = true;
                    j += 1;
                }
                if has_digit && j < data.len() && (data[j] == b'h' || data[j] == b'l') {
                    if num == 1 {
                        APP_CURSOR_KEYS.store(data[j] == b'h', Ordering::SeqCst);
                    }
                    i = j + 1;
                    continue;
                }
            }
            if i + 3 < data.len() && data[i + 2] == b'5' && data[i + 3] == b'n' {
                need_status = true;
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    if need_status {
        vt_stdin(vt).push_bytes(b"\x1b[0n");
    }
}

/// Per-VT stdout/stderr endpoint.
pub struct Stdout {
    /// Index of the virtual terminal this stdout belongs to.
    vt: usize,
}

impl INode for Stdin {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.flush_ready_flag();
        let termios = *tty_termios(self.vt).lock();
        let is_canon = termios.canonical();

        let mut stdin_buf = self.buf.lock();
        if stdin_buf.is_empty() {
            // Never leave READABLE latched on an empty queue.
            self.eventbus.lock().clear(Event::READABLE);
            if self.eof_pending.swap(false, Ordering::AcqRel) {
                return Ok(0);
            }
        }

        // POSIX non-canonical VMIN/VTIME, the same rule the pseudo-terminal's
        // line discipline follows (`Termios::noncanon_read`).
        let limit = if is_canon {
            buf.len()
        } else {
            let queued = stdin_buf.len();
            match termios.noncanon_read(queued, buf.len(), self.vtime_expired()) {
                TtyRead::Take(n) => n,
                TtyRead::Now => {
                    self.vtime_deadline_ns.store(0, Ordering::Release);
                    return Ok(0);
                }
                TtyRead::Wait => {
                    self.read_need
                        .store(termios.noncanon_need(buf.len()), Ordering::Relaxed);
                    self.arm_vtime(&termios, queued);
                    return Err(FsError::Again);
                }
            }
        };
        if stdin_buf.is_empty() {
            // Canonical mode only: non-canonical never reaches here empty.
            return Err(FsError::Again);
        }

        self.vtime_deadline_ns.store(0, Ordering::Release);
        let mut read_bytes = 0;
        while read_bytes < limit && !stdin_buf.is_empty() {
            let b = stdin_buf.pop_front().unwrap();
            buf[read_bytes] = b;
            read_bytes += 1;
            if is_canon && b == b'\n' {
                break;
            }
        }
        if stdin_buf.is_empty() {
            self.eventbus.lock().clear(Event::READABLE);
        }
        Ok(read_bytes)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        tty_handle_outgoing(self.vt, buf);
        tty_write_out(self.vt, buf);
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        self.flush_ready_flag();
        Ok(PollStatus {
            read: self.poll_ready(),
            // VT nodes are RDWR (`/dev/ttyN`); Linux always reports POLLOUT.
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        /// Parks on the stdin eventbus (+ interactive IO wait). Must unsubscribe
        /// on Ready/`Drop`: every poll/epoll re-scan boxes a fresh future and
        /// drops it while Pending — without cleanup each pass leaves an orphan
        /// waker (UAF when a later keystroke fires into freed task memory).
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct SerialFuture<'a> {
            stdin: &'a Stdin,
            armed: bool,
            sub_id: Option<u64>,
            timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
            io_waker: Option<core::task::Waker>,
        }

        impl Drop for SerialFuture<'_> {
            fn drop(&mut self) {
                if let Some(id) = self.sub_id.take() {
                    self.stdin.eventbus.lock().unsubscribe(id);
                }
                kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
                if let Some(w) = self.io_waker.take() {
                    crate::net::clear_io_wait_wakers(&w, false, true);
                }
            }
        }

        impl<'a> Future for SerialFuture<'a> {
            type Output = Result<PollStatus>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                let this = self.as_mut().get_mut();
                this.stdin.flush_ready_flag();

                // Arm / refresh the VTIME deadline. `read_at` normally does
                // this on the way past, but the wait is reachable on its own
                // (splice, a re-poll after a spurious wakeup) and a wait with
                // no deadline is a wait with no end.
                {
                    let termios = *tty_termios(this.stdin.vt).lock();
                    if !termios.canonical() {
                        let queued = this.stdin.buf.lock().len();
                        this.stdin.arm_vtime(&termios, queued);
                    }
                }

                if this.stdin.read_ready() {
                    if let Some(id) = this.sub_id.take() {
                        this.stdin.eventbus.lock().unsubscribe(id);
                    }
                    kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
                    if let Some(w) = this.io_waker.take() {
                        crate::net::clear_io_wait_wakers(&w, false, true);
                    }
                    return Poll::Ready(Ok(PollStatus {
                        read: true,
                        write: true,
                        error: false,
                        hangup: false,
                    }));
                }

                if this.armed {
                    crate::net::retain_io_wait_wakers(cx.waker(), false, true);
                    this.armed = false;
                } else {
                    crate::net::register_io_wait_wakers(cx.waker(), false, true);
                    this.io_waker = Some(cx.waker().clone());
                    if this.sub_id.is_none() {
                        let waker = cx.waker().clone();
                        this.sub_id = this.stdin.eventbus.lock().subscribe(Box::new(move |_| {
                            waker.wake_by_ref();
                            true
                        }));
                    }
                    // Cancellable VTIME timer so `dd`/`read` with min 0 time N
                    // return after N deciseconds instead of blocking forever.
                    let dl_ns = this.stdin.vtime_deadline_ns.load(Ordering::Acquire);
                    if dl_ns != 0 {
                        kernel_hal::timer_waker::ensure_timer_waker(
                            &mut this.timer,
                            Duration::from_nanos(dl_ns),
                            cx,
                        );
                    }
                    this.armed = true;
                }

                // Poll xHCI from read() path (does not go through poll(2)).
                crate::net::io_wait_tick(false, true);

                this.stdin.flush_ready_flag();
                if this.stdin.read_ready() {
                    if let Some(id) = this.sub_id.take() {
                        this.stdin.eventbus.lock().unsubscribe(id);
                    }
                    kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
                    if let Some(w) = this.io_waker.take() {
                        crate::net::clear_io_wait_wakers(&w, false, true);
                    }
                    Poll::Ready(Ok(PollStatus {
                        read: true,
                        write: true,
                        error: false,
                        hangup: false,
                    }))
                } else {
                    Poll::Pending
                }
            }
        }

        Box::pin(SerialFuture {
            stdin: self,
            armed: false,
            sub_id: None,
            timer: None,
            io_waker: None,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        tty_ioctl(self.vt, cmd, data)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    /// Get metadata of the INode
    fn metadata(&self) -> Result<Metadata> {
        // Linux VT nodes: `/dev/ttyN` is major 4, minor N. `/dev/tty` (current)
        // uses major 5 minor 0 — that node is the shared `STDIN` alias; per-VT
        // nodes here use (4, vt+1) so `ttyname()` can tell them apart.
        Ok(Metadata {
            dev: 1,
            inode: 100 + self.vt,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o666,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(4, self.vt + 1),
        })
    }
}

/// `/dev/tty0` and `/dev/console` — Linux's "current VT" control nodes. Unlike a
/// fixed `/dev/ttyN`, their VT-management ioctls must act on the **active** VT.
///
/// This matters for the graphical session. `seatd` (the seat manager `libseat`
/// talks to) does ALL of its VT management — `VT_SETMODE(VT_PROCESS)`,
/// `KDSETMODE(KD_GRAPHICS)`, `VT_RELDISP` — through `/dev/tty0`. When that node
/// was backed by the first VT's stdin (a FIXED `vt = 0`), every one of those
/// ioctls landed on VT 0 instead of the active graphics VT (`tty7`):
///
///  * `VT_SETMODE(VT_PROCESS)` registered the switch-handshake owner on VT 0, so
///    `complete_vt_switch(tty7)` delivered `acqsig` to `vt_owner(tty7) == 0` —
///    nobody. `seatd` never learned its session had become active, and `libseat`
///    kept the compositor PAUSED: `labwc` started and its Wayland clients
///    connected, but it never presented a frame to DRM (reproduced in QEMU: a
///    full session bring-up with ZERO `[drm] first present`).
///  * `KDSETMODE(KD_GRAPHICS)` on VT 0 while `tty7` was active flipped the
///    display back to the text VT (see `set_kd_mode_vt`), and left the keyboard
///    cooking gate keyed off the wrong VT's mode.
///
/// Routing these two nodes' ioctls through [`active_vt`](kernel_hal::console::active_vt)
/// makes `seatd`'s VT ops land on the real foreground VT, exactly like Linux's
/// `/dev/tty0`. Reads and writes still go to the inner (fixed) stdin, so the
/// boot console is byte-for-byte unchanged — only the ioctl target follows the
/// active VT.
pub struct CurrentVtTty {
    inner: Arc<Stdin>,
}

impl INode for CurrentVtTty {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        self.inner.write_at(offset, buf)
    }

    fn poll(&self) -> Result<PollStatus> {
        self.inner.poll()
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        self.inner.async_poll()
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        // THE fix: a "current VT" control node resolves its ioctls to the
        // active VT, never a fixed one. Before a graphical session switches
        // away from tty1 this is still VT 0 (active_vt() == 0), so early boot
        // is unchanged; once the session lands on tty7 seatd's VT_SETMODE /
        // KDSETMODE / VT_RELDISP correctly target it.
        tty_ioctl(kernel_hal::console::active_vt(), cmd, data)
    }

    fn as_any_ref(&self) -> &dyn Any {
        // Delegate identity to the inner stdin so any `downcast::<Stdin>()`
        // against `/dev/tty0`/`/dev/console` keeps working as before.
        self.inner.as_any_ref()
    }

    fn metadata(&self) -> Result<Metadata> {
        self.inner.metadata()
    }
}

/// The `/dev/tty0` and `/dev/console` control node: I/O backed by the first VT's
/// stdin, but VT-management ioctls follow the active VT (see [`CurrentVtTty`]).
pub fn current_vt_tty() -> Arc<dyn INode> {
    Arc::new(CurrentVtTty {
        inner: STDINS[0].clone(),
    })
}

impl INode for Stdout {
    /// The write side of a terminal has nothing to read. `File::read` refuses
    /// a `WRONLY` descriptor before it gets here, and no `/dev` node is backed
    /// by a `Stdout` (the per-VT nodes are `Stdin`), so nothing reaches this
    /// today -- but the body was `unimplemented!()`, which is a kernel panic
    /// rather than an errno, and one `devfs_root.add` away from being
    /// reachable from a `read(2)`.
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        tty_handle_outgoing(self.vt, buf);
        tty_write_out(self.vt, buf);
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: false,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        tty_ioctl(self.vt, cmd, data)
    }

    /// Get metadata of the INode
    fn metadata(&self) -> Result<Metadata> {
        // Same (dev, inode) as `Stdin` for this VT so `ttyname()` on fd 1/2
        // matches `stat("/dev/ttyN")`.
        Ok(Metadata {
            dev: 1,
            inode: 100 + self.vt,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o666,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(4, self.vt + 1),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod line_discipline_tests {
    //! Host tests for the TTY line discipline — `Stdin::push`.
    //!
    //! This is the code between a keystroke and what a shell's `read()`
    //! returns. Breaking it does not fail loudly; it leaves a console that
    //! swallows backspace, never completes a line, or stops answering Ctrl-C,
    //! and none of that shows up until someone sits in front of a terminal.
    //!
    //! What a test can see here is which characters become *readable* and
    //! when, which is the half that matters. Echo is not observable: it goes
    //! to `vt_console_write_str`, a no-op without the `graphic` feature, which
    //! `linux-object` does not enable.
    //!
    //! `Stdin::new` gives each test its own buffers, but `c_termios` and the
    //! flow-control flag are per-VT globals, so the tests share one VT and
    //! serialise on [`SERIAL`], resetting both as they start.

    use super::*;
    use alloc::string::String;
    use lock::Mutex as TestMutex;

    static SERIAL: TestMutex<()> = TestMutex::new(());

    /// A VT that is neither the serial console (0) nor the graphics VT.
    const VT: usize = 3;

    /// `Termios::default_tty()` with `c_lflag`/`c_iflag` overridden, installed
    /// on [`VT`], plus a fresh `Stdin` bound to it.
    fn tty(lflag: u32, iflag: u32) -> Stdin {
        let mut t = Termios::default_tty();
        t.c_lflag = lflag;
        t.c_iflag = iflag;
        *tty_termios(VT).lock() = t;
        TTY_STATES[VT].flow_stopped.store(false, Ordering::Relaxed);
        TTY_STATES[VT].fg_pgrp.store(0, Ordering::Relaxed);
        crate::process::clear_interrupt_arm(&TTY_STATES[VT].ctrl_c_armed_pgid);
        ctrl_c_pending_take();
        Stdin::new(VT)
    }

    /// The cooked default: ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN,
    /// ICRNL | IXON | IMAXBEL.
    fn cooked() -> Stdin {
        let d = Termios::default_tty();
        tty(d.c_lflag, d.c_iflag)
    }

    fn feed(s: &Stdin, text: &str) {
        for c in text.chars() {
            s.push(c);
        }
    }

    /// Everything a reader could take right now, leaving the buffer empty.
    ///
    /// Decoded back from the bytes the queue holds, so a test may write what
    /// the user typed and read it back as such; [`drain_bytes`] is for the
    /// tests that care about the bytes themselves.
    fn drain(s: &Stdin) -> String {
        String::from_utf8_lossy(&drain_bytes(s)).into_owned()
    }

    /// The same, as the bytes a `read(2)` would actually hand over.
    fn drain_bytes(s: &Stdin) -> Vec<u8> {
        let mut out = Vec::new();
        while s.can_read() {
            out.push(s.pop());
        }
        out
    }

    fn flow_stopped() -> bool {
        TTY_STATES[VT].flow_stopped.load(Ordering::Relaxed)
    }

    const DEL: char = '\u{7f}';
    const CTRL_C: char = '\u{3}';
    const CTRL_D: char = '\u{4}';
    const CTRL_O: char = '\u{f}';
    const CTRL_Q: char = '\u{11}';
    const CTRL_R: char = '\u{12}';
    const CTRL_S: char = '\u{13}';
    const CTRL_U: char = '\u{15}';
    const CTRL_V: char = '\u{16}';
    const CTRL_W: char = '\u{17}';

    // ---- canonical line assembly ---------------------------------------

    #[test]
    fn a_canonical_line_is_delivered_only_on_its_newline() {
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "hola");
        // Still being edited: a reader must see nothing at all.
        assert!(!s.can_read());
        s.push('\n');
        // And then the whole line at once, newline included.
        assert_eq!(drain(&s), "hola\n");
        assert!(!s.can_read());
    }

    #[test]
    fn a_key_that_is_not_ascii_reaches_the_program_as_utf8() {
        // The queue used to hold `char`s and the read cast each one to a
        // single byte. On the Spanish layout that made `ñ` (U+00F1) arrive as
        // the lone byte 0xf1 — Latin-1 in a system that is UTF-8 everywhere
        // else, and not a valid character at all.
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "ñ\n");
        assert_eq!(drain_bytes(&s), b"\xc3\xb1\n");
    }

    #[test]
    fn two_different_keys_no_longer_arrive_as_the_same_byte() {
        // `€` is U+20AC and `¬` is U+00AC. Cast to one byte both came out as
        // 0xac, so AltGr+5 and AltGr+6 on the Spanish layout were the same
        // key as far as any program could tell.
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "€\n");
        let euro = drain_bytes(&s);
        feed(&s, "¬\n");
        let not = drain_bytes(&s);
        assert_eq!(euro, "€\n".as_bytes());
        assert_eq!(not, "¬\n".as_bytes());
        assert_ne!(euro, not);
    }

    #[test]
    fn every_key_the_spanish_layout_can_produce_survives_the_trip() {
        // The eleven characters outside ASCII that `symbols/es` can reach.
        let _g = SERIAL.lock();
        let s = cooked();
        let keys = "ñÑ¡¿ºª´¨€·¬";
        feed(&s, keys);
        s.push('\n');
        assert_eq!(drain(&s), alloc::format!("{}\n", keys));
    }

    #[test]
    fn a_reader_with_room_for_one_byte_gets_the_character_in_pieces() {
        // `read(fd, buf, 1)` is what `getchar` does. A character may not be
        // dropped because it does not fit, and it may not be handed over
        // whole into a buffer that has no room for it.
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "ñ\n");
        let mut got = Vec::new();
        for _ in 0..3 {
            let mut one = [0u8; 1];
            assert_eq!(s.read_at(0, &mut one), Ok(1));
            got.push(one[0]);
        }
        assert_eq!(got, b"\xc3\xb1\n");
    }

    #[test]
    fn erasing_an_accented_letter_takes_the_whole_letter() {
        // The console edits the pending line as characters, so this already
        // worked; the test is here because the pseudo-terminal's discipline
        // is a separate implementation that did not, and the two are supposed
        // to agree.
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "añ");
        s.push(DEL);
        s.push('\n');
        assert_eq!(drain_bytes(&s), b"a\n");
    }

    #[test]
    fn a_line_with_an_accent_comes_back_in_one_read() {
        // The read stops at the newline that ends the line, and at nothing
        // else: no byte inside a character may be mistaken for one.
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "añón\n");
        let mut buf = [0u8; 64];
        let n = s.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..n], "añón\n".as_bytes());
    }

    #[test]
    fn how_many_bytes_are_waiting_is_bytes_and_not_keys() {
        // `TIOCINQ`/`FIONREAD` answers in bytes, and a reader sizes its
        // buffer with it.
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "ñ\n");
        assert_eq!(s.buf.lock().len(), 3);
    }

    #[test]
    fn icrnl_completes_a_line_but_inlcr_and_igncr_do_not() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        // ICRNL (the default): Enter sends CR, and it must land as '\n'.
        let s = tty(d.c_lflag, I_ICRNL);
        feed(&s, "ab\r");
        assert_eq!(drain(&s), "ab\n");

        // IGNCR: the CR is dropped outright, so the line stays open.
        let s = tty(d.c_lflag, I_IGNCR);
        feed(&s, "ab\r");
        assert!(!s.can_read());
        s.push('\n');
        assert_eq!(drain(&s), "ab\n");

        // INLCR: '\n' becomes '\r', which is not an end-of-line, so the line
        // does NOT complete -- the translation runs before the EOL test.
        let s = tty(d.c_lflag, I_INLCR);
        feed(&s, "ab\n");
        assert!(!s.can_read());
    }

    #[test]
    fn a_seven_bit_terminal_ends_its_line_with_the_return_key() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        // On a 7-bit line Enter arrives as 0x8d, and ISTRIP is what makes it
        // a carriage return for ICRNL to end the line with. This discipline
        // carried its own copy of the CR/NL block with no ISTRIP in it, so
        // the key did nothing at all.
        let s = tty(d.c_lflag, I_ISTRIP | I_ICRNL);
        feed(&s, "ab\u{8d}");
        assert_eq!(drain(&s), "ab\n");

        // Without it the byte is just a byte and the line stays open.
        let s = tty(d.c_lflag, I_ICRNL);
        feed(&s, "ab\u{8d}");
        assert!(!s.can_read());
    }

    #[test]
    fn iuclc_folds_what_is_typed_and_iexten_turns_it_off() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag, I_IUCLC | I_ICRNL);
        assert_ne!(d.c_lflag & L_IEXTEN, 0);
        feed(&s, "HOLA\r");
        assert_eq!(drain(&s), "hola\n");

        let s = tty(d.c_lflag & !L_IEXTEN, I_IUCLC | I_ICRNL);
        feed(&s, "HOLA\r");
        assert_eq!(drain(&s), "HOLA\n");
    }

    #[test]
    fn a_latin1_keystroke_is_the_byte_the_line_sent() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        // This queue is chars where the other two disciplines are bytes, and
        // the input flags are a byte's rules. The serial console is the
        // caller ISTRIP exists for and it hands in `byte as char`, so a
        // scalar that fits in a byte IS that byte: `Ñ` is 0xd1, and a 7-bit
        // line carrying 0xd1 delivered 0x51.
        let s = tty(d.c_lflag, I_ISTRIP | I_ICRNL);
        feed(&s, "Ñ\r");
        assert_eq!(drain(&s), "Q\n");
    }

    #[test]
    fn a_scalar_no_byte_could_have_carried_goes_through_untouched() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        // Above U+00FF there is no byte to apply a byte's rules to, and the
        // char must reach the queue whole rather than be truncated into one.
        let s = tty(d.c_lflag, I_ISTRIP | I_IUCLC | I_ICRNL);
        feed(&s, "€\r");
        assert_eq!(drain(&s), "€\n");
    }

    #[test]
    fn veol_and_veol2_terminate_a_line_like_newline_does() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let mut t = Termios::default_tty();
        t.c_cc[VEOL] = b';';
        t.c_cc[VEOL2] = b'!';
        *tty_termios(VT).lock() = t;
        let s = Stdin::new(VT);
        feed(&s, "uno;");
        assert_eq!(drain(&s), "uno;");
        feed(&s, "dos!");
        assert_eq!(drain(&s), "dos!");
        // A VEOL of 0 means "disabled", never "matches NUL".
        let s = tty(d.c_lflag, d.c_iflag);
        feed(&s, "tres\0");
        assert!(!s.can_read());
    }

    #[test]
    fn a_control_char_set_to_zero_is_disabled_not_a_match_for_nul() {
        let _g = SERIAL.lock();
        // The same rule on the `cc_match` side, which every signal and
        // editing character goes through. Disabling VINTR must make Ctrl-C
        // ordinary input AND must not turn NUL into the interrupt character.
        let mut t = Termios::default_tty();
        t.c_cc[VINTR] = 0;
        *tty_termios(VT).lock() = t;
        TTY_STATES[VT].flow_stopped.store(false, Ordering::Relaxed);
        ctrl_c_pending_take();
        let s = Stdin::new(VT);
        s.push('\0');
        s.push(CTRL_C);
        s.push('\n');
        assert_eq!(drain(&s), alloc::format!("\0{}\n", CTRL_C));
        assert!(!ctrl_c_pending_peek());
    }

    // ---- line editing ---------------------------------------------------

    #[test]
    fn verase_removes_one_character_and_vkill_the_whole_line() {
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "abc");
        s.push(DEL);
        s.push('\n');
        assert_eq!(drain(&s), "ab\n");

        feed(&s, "borrame");
        s.push(CTRL_U);
        s.push('\n');
        assert_eq!(drain(&s), "\n");
    }

    #[test]
    fn verase_on_an_empty_line_erases_nothing() {
        let _g = SERIAL.lock();
        let s = cooked();
        // Backspace at the prompt must not eat into a previous line, and must
        // not leave anything readable.
        s.push(DEL);
        s.push(DEL);
        assert!(!s.can_read());
        feed(&s, "x\n");
        assert_eq!(drain(&s), "x\n");
    }

    #[test]
    fn vwerase_drops_trailing_blanks_then_one_word() {
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "foo bar  ");
        s.push(CTRL_W);
        s.push('\n');
        assert_eq!(drain(&s), "foo \n");
        // A second word erase takes "foo " down to empty (the blank it stops
        // at belongs to the word it just removed).
        feed(&s, "foo bar");
        s.push(CTRL_W);
        s.push(CTRL_W);
        s.push('\n');
        assert_eq!(drain(&s), "\n");
    }

    #[test]
    fn the_extended_editing_chars_need_iexten() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        // Without IEXTEN, Ctrl-W / Ctrl-R / Ctrl-V are ordinary input.
        let s = tty(d.c_lflag & !L_IEXTEN, d.c_iflag);
        feed(&s, "ab");
        s.push(CTRL_W);
        s.push(CTRL_R);
        s.push(CTRL_V);
        s.push('\n');
        let line = drain(&s);
        assert_eq!(line, alloc::format!("ab{}{}{}\n", CTRL_W, CTRL_R, CTRL_V));
        // With it, the same Ctrl-W edits the line instead.
        let s = cooked();
        feed(&s, "ab cd");
        s.push(CTRL_W);
        s.push('\n');
        assert_eq!(drain(&s), "ab \n");
    }

    #[test]
    fn vlnext_quotes_the_next_character_even_a_signal_one() {
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "x");
        s.push(CTRL_V);
        // Ctrl-C right after Ctrl-V is data, not SIGINT: the line survives and
        // the interrupt latch stays clear.
        s.push(CTRL_C);
        s.push('\n');
        assert_eq!(drain(&s), alloc::format!("x{}\n", CTRL_C));
        assert!(!ctrl_c_pending_peek());
        // The latch is one-shot: the next Ctrl-C is a signal again.
        feed(&s, "y");
        s.push(CTRL_C);
        assert!(ctrl_c_pending_take().is_some());
        assert!(!s.can_read());
    }

    #[test]
    fn vreprint_redraws_without_changing_the_pending_line() {
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "intacta");
        s.push(CTRL_R);
        s.push('\n');
        assert_eq!(drain(&s), "intacta\n");
    }

    // ---- end of file ----------------------------------------------------

    #[test]
    fn veof_delivers_a_partial_line_and_only_signals_eof_on_an_empty_one() {
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "abc");
        s.push(CTRL_D);
        // The partial line becomes readable with no newline appended, and this
        // is NOT end of file -- the shell must keep reading.
        assert_eq!(drain(&s), "abc");
        assert!(!s.eof_pending.load(Ordering::Acquire));
        // Ctrl-D at the start of a line is end of file.
        s.push(CTRL_D);
        assert!(!s.can_read());
        assert!(s.eof_pending.load(Ordering::Acquire));
    }

    #[test]
    fn an_interrupt_clears_a_pending_eof() {
        let _g = SERIAL.lock();
        let s = cooked();
        s.push(CTRL_D);
        assert!(s.eof_pending.load(Ordering::Acquire));
        s.push(CTRL_C);
        // Otherwise the next read would return 0 and the shell would exit on
        // a Ctrl-C.
        assert!(!s.eof_pending.load(Ordering::Acquire));
    }

    // ---- raw mode -------------------------------------------------------

    #[test]
    fn raw_mode_delivers_every_character_as_it_arrives() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag & !ICANON, d.c_iflag);
        s.push('a');
        assert_eq!(drain(&s), "a");
        feed(&s, "bc");
        assert_eq!(drain(&s), "bc");
    }

    /// A raw console with `min`/`time` set, as `stty -icanon min N time M`
    /// leaves it.
    fn raw_tty(vmin: u8, vtime: u8) -> Stdin {
        let d = Termios::default_tty();
        let s = tty(d.c_lflag & !ICANON, d.c_iflag);
        let mut t = *tty_termios(VT).lock();
        t.c_cc[VMIN_CC] = vmin;
        t.c_cc[VTIME_CC] = vtime;
        *tty_termios(VT).lock() = t;
        s
    }

    #[test]
    fn a_raw_read_with_min_zero_comes_back_empty_instead_of_waiting() {
        // `stty -icanon min 0 time 0`: POSIX says the read returns at once
        // with whatever is there, including nothing.
        let _g = SERIAL.lock();
        let s = raw_tty(0, 0);
        let mut buf = [0u8; 16];
        assert_eq!(s.read_at(0, &mut buf), Ok(0));
        feed(&s, "abc");
        assert_eq!(s.read_at(0, &mut buf), Ok(3));
        assert_eq!(&buf[..3], b"abc");
    }

    #[test]
    fn the_wait_and_the_read_agree_on_when_a_raw_read_is_over() {
        // `File::read` is `loop { read_at; on EAGAIN await the wait }`. The
        // wait used to answer "is there a byte", so under `min 4` every byte
        // between the first and the fourth had the loop going round at full
        // tilt: the read said not yet, the wait said ready, nothing changed.
        let _g = SERIAL.lock();
        let s = raw_tty(4, 0);
        let mut buf = [0u8; 16];
        for n in 0..4 {
            assert_eq!(s.read_at(0, &mut buf), Err(FsError::Again), "{} queued", n);
            assert!(!s.read_ready(), "{} byte(s) queued", n);
            s.push('x');
        }
        assert!(s.read_ready());
        assert_eq!(s.read_at(0, &mut buf), Ok(4));
    }

    #[test]
    fn poll_calls_a_console_readable_as_soon_as_a_byte_is_there() {
        // `n_tty_poll` and `n_tty_read` do not ask the same question: poll
        // reports the terminal readable on the first byte whatever VMIN says,
        // while the read itself still waits. Giving poll the read's answer
        // would leave a `select` loop asleep on a terminal that has input.
        let _g = SERIAL.lock();
        let s = raw_tty(4, 0);
        let mut buf = [0u8; 16];
        assert_eq!(s.read_at(0, &mut buf), Err(FsError::Again));
        s.push('x');
        assert!(s.poll_ready(), "poll sees the byte");
        assert!(
            INode::poll(&s).unwrap().read,
            "and poll(2) is wired to that answer, not the read's"
        );
        assert!(!s.read_ready(), "the read is still short of its minimum");
    }

    #[test]
    fn a_raw_read_hands_over_everything_queued_and_not_just_the_minimum() {
        // `n_tty_read` copies up to the buffer size; VMIN decides when the
        // read returns, not how much it may carry. Capping at VMIN left the
        // rest queued and sent the reader round again for bytes it had room
        // for.
        let _g = SERIAL.lock();
        let s = raw_tty(2, 0);
        feed(&s, "abcdef");
        let mut buf = [0u8; 16];
        assert_eq!(s.read_at(0, &mut buf), Ok(6));
        assert_eq!(&buf[..6], b"abcdef");
    }

    #[test]
    fn a_buffer_smaller_than_the_minimum_does_not_wait_for_ever() {
        // `read(fd, buf, 2)` under `min 4`: the caller cannot take four bytes,
        // so a read that insisted on four would never come back, and a wait
        // that insisted on four would sleep through the wakeup.
        let _g = SERIAL.lock();
        let s = raw_tty(4, 0);
        let mut buf = [0u8; 2];
        assert_eq!(s.read_at(0, &mut buf), Err(FsError::Again));
        feed(&s, "ab");
        assert!(s.read_ready());
        assert_eq!(s.read_at(0, &mut buf), Ok(2));
    }

    #[test]
    fn the_between_bytes_timer_does_not_start_on_an_empty_queue() {
        // With VMIN > 0 the timer measures the gap between bytes. Starting it
        // at the read would let a `min 1 time N` program come back empty on an
        // idle terminal, which every reader takes for end of file.
        let _g = SERIAL.lock();
        let s = raw_tty(2, 5);
        let mut buf = [0u8; 16];
        assert_eq!(s.read_at(0, &mut buf), Err(FsError::Again));
        assert_eq!(s.vtime_deadline_ns.load(Ordering::Relaxed), 0);
        s.push('a');
        assert_eq!(s.read_at(0, &mut buf), Err(FsError::Again));
        assert_ne!(
            s.vtime_deadline_ns.load(Ordering::Relaxed),
            0,
            "the first byte starts it"
        );
    }

    #[test]
    fn a_read_that_came_back_leaves_no_timer_running_behind_it() {
        let _g = SERIAL.lock();
        let s = raw_tty(0, 5);
        let mut buf = [0u8; 16];
        assert_eq!(s.read_at(0, &mut buf), Err(FsError::Again));
        assert_ne!(s.vtime_deadline_ns.load(Ordering::Relaxed), 0);
        s.push('a');
        assert_eq!(s.read_at(0, &mut buf), Ok(1));
        assert_eq!(s.vtime_deadline_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_read_of_no_bytes_is_over_before_the_settings_are_consulted() {
        // Every mode, including `min 1`, which otherwise may not come back
        // empty.
        let _g = SERIAL.lock();
        let s = raw_tty(1, 0);
        feed(&s, "abc");
        assert_eq!(s.read_at(0, &mut []), Ok(0));
        // And the bytes are still there for a read with room.
        let mut buf = [0u8; 16];
        assert_eq!(s.read_at(0, &mut buf), Ok(3));
    }

    // ---- signals --------------------------------------------------------

    #[test]
    fn vintr_latches_ctrl_c_and_flushes_what_was_typed() {
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "a medio escribir");
        s.push(CTRL_C);
        assert!(!s.can_read());
        assert!(ctrl_c_pending_take().is_some());
        // And the latch is consumed by that take.
        assert!(!ctrl_c_pending_peek());
    }

    #[test]
    fn noflsh_keeps_the_pending_input_across_an_interrupt() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag | NOFLSH, d.c_iflag);
        feed(&s, "abc");
        s.push(CTRL_C);
        assert!(ctrl_c_pending_take().is_some());
        // The half-typed line is still there, so the newline still delivers it.
        s.push('\n');
        assert_eq!(drain(&s), "abc\n");
    }

    #[test]
    fn without_isig_an_interrupt_character_is_ordinary_input() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag & !ISIG, d.c_iflag);
        feed(&s, "a");
        s.push(CTRL_C);
        s.push('\n');
        assert_eq!(drain(&s), alloc::format!("a{}\n", CTRL_C));
        assert!(!ctrl_c_pending_peek());
    }

    // ---- software flow control -----------------------------------------

    #[test]
    fn ixon_consumes_stop_and_start_without_delivering_them() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag, IXON | I_ICRNL);
        feed(&s, "a");
        s.push(CTRL_S);
        assert!(flow_stopped());
        s.push(CTRL_Q);
        assert!(!flow_stopped());
        s.push('\n');
        // Neither Ctrl-S nor Ctrl-Q may reach the reader.
        assert_eq!(drain(&s), "a\n");
    }

    #[test]
    fn ixany_lets_any_character_resume_stopped_output() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag, IXON | IXANY | I_ICRNL);
        s.push(CTRL_S);
        assert!(flow_stopped());
        // The resuming byte is still delivered as input.
        s.push('z');
        assert!(!flow_stopped());
        s.push('\n');
        assert_eq!(drain(&s), "z\n");
    }

    #[test]
    fn without_ixany_input_does_not_resume_stopped_output() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag, IXON | I_ICRNL);
        s.push(CTRL_S);
        s.push('z');
        assert!(flow_stopped());
    }

    #[test]
    fn a_job_control_signal_lifts_an_output_freeze() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag, IXON | I_ICRNL);
        s.push(CTRL_S);
        assert!(flow_stopped());
        // Otherwise the signalled process stays blocked behind the Ctrl-S and
        // the terminal looks hung.
        s.push(CTRL_C);
        assert!(!flow_stopped());
        assert!(ctrl_c_pending_take().is_some());
    }

    #[test]
    fn vdiscard_is_swallowed_under_iexten() {
        let _g = SERIAL.lock();
        let d = Termios::default_tty();
        let s = tty(d.c_lflag, d.c_iflag);
        feed(&s, "ab");
        s.push(CTRL_O);
        s.push('\n');
        assert_eq!(drain(&s), "ab\n");
        // Without IEXTEN it is ordinary input.
        let s = tty(d.c_lflag & !L_IEXTEN, d.c_iflag);
        feed(&s, "ab");
        s.push(CTRL_O);
        s.push('\n');
        assert_eq!(drain(&s), alloc::format!("ab{}\n", CTRL_O));
    }

    // ---- the outgoing escape-sequence sniffer ---------------------------

    /// A VT of its own for the outgoing tests, because they drive the shared
    /// `vt_stdin(vt)` rather than a private `Stdin`.
    const VT_OUT: usize = 4;

    fn drain_vt_out() -> String {
        let s = vt_stdin(VT_OUT);
        let mut out = Vec::new();
        while s.can_read() {
            out.push(s.pop());
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn decckm_follows_smkx_and_rmkx() {
        let _g = SERIAL.lock();
        // `\E[?1h` / `\E[?1l` is how a shell switches the arrow keys between
        // application and normal mode; miss it and every arrow key sends the
        // wrong prefix.
        APP_CURSOR_KEYS.store(false, Ordering::SeqCst);
        tty_handle_outgoing(VT_OUT, b"\x1b[?1h");
        assert!(APP_CURSOR_KEYS.load(Ordering::SeqCst));
        tty_handle_outgoing(VT_OUT, b"\x1b[?1l");
        assert!(!APP_CURSOR_KEYS.load(Ordering::SeqCst));
        // Another private mode must not move it.
        APP_CURSOR_KEYS.store(true, Ordering::SeqCst);
        tty_handle_outgoing(VT_OUT, b"\x1b[?25l");
        assert!(APP_CURSOR_KEYS.load(Ordering::SeqCst));
        APP_CURSOR_KEYS.store(false, Ordering::SeqCst);
    }

    #[test]
    fn a_device_status_report_is_answered_on_the_same_vt() {
        let _g = SERIAL.lock();
        let _ = drain_vt_out();
        tty_handle_outgoing(VT_OUT, b"\x1b[5n");
        assert_eq!(drain_vt_out(), "\x1b[0n");
        // Embedded in ordinary output, and answered once per write however
        // many times it appears.
        tty_handle_outgoing(VT_OUT, b"hola\x1b[5nadios\x1b[5n");
        assert_eq!(drain_vt_out(), "\x1b[0n");
        // Ordinary output alone is never answered.
        tty_handle_outgoing(VT_OUT, b"nada que ver");
        assert_eq!(drain_vt_out(), "");
    }

    #[test]
    fn a_truncated_escape_sequence_is_ignored_not_misread() {
        let _g = SERIAL.lock();
        let _ = drain_vt_out();
        APP_CURSOR_KEYS.store(false, Ordering::SeqCst);
        // Each of these ends mid-sequence; the parser must run off none of
        // them, answer nothing and change nothing.
        for partial in [
            &b"\x1b"[..],
            &b"\x1b["[..],
            &b"\x1b[?"[..],
            &b"\x1b[?1"[..],
            &b"\x1b[5"[..],
        ] {
            tty_handle_outgoing(VT_OUT, partial);
        }
        tty_handle_outgoing(VT_OUT, b"");
        assert_eq!(drain_vt_out(), "");
        assert!(!APP_CURSOR_KEYS.load(Ordering::SeqCst));
    }

    // ---- the bypass -----------------------------------------------------

    #[test]
    fn push_bytes_bypasses_the_line_discipline_entirely() {
        let _g = SERIAL.lock();
        let s = cooked();
        // A TTY query response goes straight to the reader: no line assembly,
        // no signal handling, no echo -- even though this is cooked mode and
        // the payload contains an interrupt character.
        s.push_bytes(&[b'\x1b', b'[', b'0', b'n', 3]);
        assert_eq!(drain(&s), alloc::format!("\x1b[0n{}", CTRL_C));
        assert!(!ctrl_c_pending_peek());
    }

    // ---- the queue is bounded -------------------------------------------
    //
    // A console has no way to ask a keyboard to wait and no writer to hand a
    // short count to, so all a bound can do at this end is stop storing. What
    // it may never stop doing is *interpreting*: Ctrl-C is how a user rescues
    // the program that stopped reading, which is the very thing that filled
    // the queue.

    #[test]
    fn the_queue_stops_at_the_bound() {
        let _g = SERIAL.lock();
        let s = tty(ECHO, 0); // raw: every character becomes readable at once
        for _ in 0..(N_TTY_BUF_SIZE + 500) {
            s.push('x');
        }
        assert_eq!(s.buf.lock().len(), N_TTY_BUF_SIZE);
    }

    #[test]
    fn a_query_a_program_asks_for_cannot_grow_the_queue_without_end() {
        // The one path here a program drives on its own, with no keyboard and
        // no privilege: `\x1b[5n` written to its **own stdout** makes the
        // kernel answer `\x1b[0n` into its stdin. A loop of those, never
        // reading, used to be an unprivileged process asking for every page
        // the kernel had.
        let _g = SERIAL.lock();
        let s = vt_stdin(VT);
        drain_bytes(&s);
        for _ in 0..N_TTY_BUF_SIZE {
            tty_handle_outgoing(VT, b"\x1b[5n");
        }
        assert_eq!(s.buf.lock().len(), N_TTY_BUF_SIZE);
        // And the answer is still a whole answer, not a bound cutting through
        // the middle of one.
        let got = drain_bytes(&s);
        assert_eq!(got.len(), N_TTY_BUF_SIZE);
        assert_eq!(&got[..4], b"\x1b[0n");
    }

    #[test]
    fn a_line_that_fills_the_queue_can_still_be_erased() {
        let _g = SERIAL.lock();
        let s = cooked();
        for _ in 0..N_TTY_BUF_SIZE {
            s.push('x');
        }
        assert_eq!(s.canon_buf.lock().len(), N_TTY_BUF_SIZE);
        // Full, and the erase still gets through -- otherwise the terminal is
        // wedged with no way out but killing whatever holds it.
        s.push(DEL);
        assert_eq!(s.canon_buf.lock().len(), N_TTY_BUF_SIZE - 1);
        s.push('z');
        assert_eq!(s.canon_buf.lock().len(), N_TTY_BUF_SIZE);
        s.push(CTRL_U);
        assert!(s.canon_buf.lock().is_empty());
    }

    #[test]
    fn a_line_that_fills_the_queue_does_not_commit() {
        let _g = SERIAL.lock();
        let s = cooked();
        for _ in 0..N_TTY_BUF_SIZE {
            s.push('x');
        }
        // The newline does not fit either, so the line is not handed over: a
        // line that cannot hold its own terminator is not one to deliver.
        s.push('\n');
        assert!(!s.can_read());
        assert_eq!(s.canon_buf.lock().len(), N_TTY_BUF_SIZE);
        // One column back and it ends.
        s.push(DEL);
        s.push('\n');
        assert_eq!(drain_bytes(&s).len(), N_TTY_BUF_SIZE);
    }

    #[test]
    fn a_full_queue_still_answers_ctrl_c() {
        // With a line already waiting for a reader the rule's answer is
        // `Full`, which a PTY turns into a short write. A keyboard has nobody
        // to tell, so it keeps interpreting and only stops storing.
        let _g = SERIAL.lock();
        let s = cooked();
        feed(&s, "hecho\n");
        for _ in 0..N_TTY_BUF_SIZE {
            s.push('x');
        }
        assert_eq!(
            input_room(s.buf.lock().len(), editing_bytes(&s.canon_buf.lock()), true),
            InputRoom::Full
        );
        assert!(!ctrl_c_pending_peek());
        s.push(CTRL_C);
        assert!(ctrl_c_pending_peek());
        ctrl_c_pending_take();
    }

    #[test]
    fn the_line_being_edited_is_measured_in_bytes_and_not_in_keystrokes() {
        // `canon_buf` holds characters, the bound is a buffer size, and on the
        // Spanish layout those are not the same count: `ñ` is two bytes, so it
        // fills the queue in half the keystrokes `x` needs.
        let _g = SERIAL.lock();
        let s = cooked();
        for _ in 0..(N_TTY_BUF_SIZE / 2) {
            s.push('ñ');
        }
        assert_eq!(s.canon_buf.lock().len(), N_TTY_BUF_SIZE / 2);
        assert_eq!(editing_bytes(&s.canon_buf.lock()), N_TTY_BUF_SIZE);
        // Full: the next character is dropped, in half the keystrokes.
        s.push('z');
        assert_eq!(s.canon_buf.lock().len(), N_TTY_BUF_SIZE / 2);
    }

    #[test]
    fn room_comes_back_when_the_program_reads() {
        let _g = SERIAL.lock();
        let s = tty(ECHO, 0);
        for _ in 0..N_TTY_BUF_SIZE {
            s.push('x');
        }
        assert_eq!(s.buf.lock().len(), N_TTY_BUF_SIZE);
        for _ in 0..10 {
            s.pop();
        }
        for _ in 0..50 {
            s.push('y');
        }
        assert_eq!(s.buf.lock().len(), N_TTY_BUF_SIZE);
        // Exactly the room that was freed, and nothing the queue had before.
        let tail: Vec<u8> = s.buf.lock().iter().rev().take(10).copied().collect();
        assert_eq!(tail, vec![b'y'; 10]);
    }

    // ---- the output word, c_oflag ---------------------------------------
    //
    // What goes out is not observable here (`vt_console_write_str` is a no-op
    // without the `graphic` feature), but the cursor is: it is per-VT state,
    // and it is what `ONOCR` decides on. A wiring mistake shows up as a
    // column that stops moving, or that moves for the program and not for the
    // echo.

    /// Install `bits` as this VT's output flags and put the cursor at zero.
    fn oflag(bits: u32) -> Stdin {
        let s = cooked();
        tty_termios(VT).lock().c_oflag = bits;
        tty_column(VT).store(0, Ordering::Relaxed);
        s
    }

    fn column() -> usize {
        tty_column(VT).load(Ordering::Relaxed)
    }

    #[test]
    fn program_output_moves_the_console_cursor() {
        let _g = SERIAL.lock();
        let _s = oflag(O_OPOST | O_ONLCR);
        tty_post_out(VT, b"hola");
        assert_eq!(column(), 4);
        // ONLCR takes it back to the start of the next line.
        tty_post_out(VT, b"\n");
        assert_eq!(column(), 0);
    }

    #[test]
    fn the_cursor_carries_across_separate_writes() {
        // Not a local: the first byte of every write would look like column
        // zero, and ONOCR would swallow every carriage return on the machine.
        let _g = SERIAL.lock();
        let _s = oflag(O_OPOST | O_ONOCR);
        tty_post_out(VT, b"ab");
        tty_post_out(VT, b"cd");
        assert_eq!(column(), 4);
    }

    #[test]
    fn a_raw_console_moves_no_cursor_at_all() {
        // With OPOST off not one bit of the word is consulted, the column
        // included -- which is right, because nothing is being translated.
        let _g = SERIAL.lock();
        let _s = oflag(0);
        tty_post_out(VT, b"hola");
        assert_eq!(column(), 0);
    }

    #[test]
    fn the_echo_moves_the_same_cursor_as_the_program() {
        // One line on one screen. `__process_echoes` post-processes every byte
        // it puts out when OPOST is set (`drivers/tty/n_tty.c`), so the echo
        // has to count -- otherwise a terminal with ONOCR drops the carriage
        // return that ends a line the user typed.
        let _g = SERIAL.lock();
        let s = oflag(O_OPOST | O_ONOCR);
        s.echo_char('x');
        assert_eq!(column(), 1);
        s.echo_char('y');
        assert_eq!(column(), 2);
        // And the program's output continues from there.
        tty_post_out(VT, b"z");
        assert_eq!(column(), 3);
    }

    #[test]
    fn a_tab_echoes_as_a_tab_and_not_as_caret_i() {
        // Linux's `echo_char` spells out the exception: `L_ECHOCTL && iscntrl(c)
        // && c != '\t'`. This end captioned the tab, which put `^I` on screen
        // where the user expected the cursor to move, and the live PTY -- the
        // same rule, written separately -- did not.
        let _g = SERIAL.lock();
        let s = oflag(O_OPOST);
        s.echo_char('\t');
        assert_eq!(
            column(),
            8,
            "a tab moves to the next stop, it is not two characters"
        );
    }

    #[test]
    fn a_multi_byte_character_takes_one_column() {
        let _g = SERIAL.lock();
        let s = oflag(O_OPOST);
        tty_termios(VT).lock().c_iflag |= I_IUTF8;
        tty_column(VT).store(0, Ordering::Relaxed);
        s.echo_char('ñ');
        assert_eq!(column(), 1);
    }

    #[test]
    fn a_staged_run_longer_than_the_buffer_still_comes_out_whole() {
        // `tty_post_out` flushes a fixed buffer in pieces, and may only do so
        // where a character ends. The column is what proves every byte was
        // accounted for across the flushes.
        let _g = SERIAL.lock();
        let _s = oflag(O_OPOST);
        let long = alloc::vec![b'x'; 1000];
        tty_post_out(VT, &long);
        assert_eq!(column(), 1000);
    }

    #[test]
    fn a_run_of_multi_byte_characters_crosses_the_flush_boundary_intact() {
        let _g = SERIAL.lock();
        let _s = oflag(O_OPOST);
        tty_termios(VT).lock().c_iflag |= I_IUTF8;
        tty_column(VT).store(0, Ordering::Relaxed);
        let mut run = alloc::vec::Vec::new();
        for _ in 0..500 {
            run.extend_from_slice("ñ".as_bytes());
        }
        tty_post_out(VT, &run);
        assert_eq!(column(), 500, "500 characters, 1000 bytes, 500 columns");
    }

    /// Erasing a character walks the cursor back. The erase sequence used to
    /// go out raw, so the column stayed where the typed character had left it
    /// and every later `\t` in the program's own output expanded against a
    /// count one too high -- and it never came back into line.
    #[test]
    fn an_erase_echo_moves_the_cursor_back() {
        let _g = SERIAL.lock();
        let s = oflag(O_OPOST);
        s.push('a');
        s.push('b');
        assert_eq!(column(), 2);
        // VERASE (0x7f by default) with ECHOE: back, blank, back.
        s.push('\x7f');
        assert_eq!(column(), 1, "the erase took the cursor back with it");
        s.push('\x7f');
        assert_eq!(column(), 0);
    }

    /// `VKILL` erases the whole line, so the cursor ends where the line began.
    #[test]
    fn a_kill_echo_takes_the_cursor_back_to_the_start_of_the_line() {
        let _g = SERIAL.lock();
        let s = oflag(O_OPOST);
        for c in "hola".chars() {
            s.push(c);
        }
        assert_eq!(column(), 4);
        // VKILL is Ctrl-U, and ECHOKE makes it rub the line out.
        tty_termios(VT).lock().c_lflag |= crate::fs::ioctl::L_ECHOKE | ECHOE;
        s.push('\x15');
        assert_eq!(column(), 0);
    }

    /// The `^C` label is two columns and a newline, and the newline is what
    /// `ONLCR` acts on.
    #[test]
    fn the_interrupt_label_goes_through_the_output_rules() {
        let _g = SERIAL.lock();
        let s = oflag(O_OPOST | O_ONLCR);
        for c in "hola".chars() {
            s.push(c);
        }
        s.push(CTRL_C);
        assert_eq!(
            column(),
            0,
            "^C and a newline leave the cursor at the start"
        );
    }
}

#[cfg(test)]
mod vt_ownership_tests {
    //! Host tests for who owns a VT and what happens when that owner is gone.
    //!
    //! This is the code between "the compositor died" and "the screen comes
    //! back". Nothing here fails loudly: a VT left in `KD_GRAPHICS` with no
    //! owner is a black screen, and the machine is still running perfectly
    //! well behind it.
    //!
    //! The KD mode and the per-VT owner are process-wide statics and cargo
    //! runs a crate's tests in threads, so every test here takes
    //! [`test_lock`] first.

    extern crate std;

    use super::*;
    use kernel_hal::console::{
        kd_mode_vt, set_kd_mode_vt, GRAPHICS_VT, KD_GRAPHICS, KD_TEXT, KD_TEXT0, KD_TEXT1,
    };
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Put `vt` in the state a compositor leaves it in: taken with
    /// `VT_SETMODE(VT_PROCESS)` by `owner`, and in graphics mode.
    fn a_compositor_owns(vt: usize, owner: u64) {
        *tty_vt_mode(vt).lock() = VtMode {
            mode: VT_PROCESS,
            waitv: 0,
            relsig: 10,
            acqsig: 12,
            frsig: 0,
        };
        TTY_STATES[vt_clamp(vt)]
            .vt_owner
            .store(owner, Ordering::Relaxed);
        set_kd_mode_vt(vt, KD_GRAPHICS);
    }

    fn release(vt: usize) {
        *tty_vt_mode(vt).lock() = VtMode::auto();
        TTY_STATES[vt_clamp(vt)]
            .vt_owner
            .store(0, Ordering::Relaxed);
        set_kd_mode_vt(vt, KD_TEXT);
    }

    // ---- the one that matters ---------------------------------------------

    #[test]
    fn giving_a_vt_back_returns_it_to_text() {
        // Linux's `reset_vc()` does this, and the comment at its call site
        // says why: "it saves the agony when the X server dies and the screen
        // remains blanked due to KD_GRAPHICS". Reverting the handshake and
        // leaving the VT in graphics mode is a black screen on a machine that
        // is otherwise fine.
        let _g = test_lock();
        a_compositor_owns(GRAPHICS_VT, 4321);
        assert_eq!(kd_mode_vt(GRAPHICS_VT), KD_GRAPHICS, "arrange failed");

        reset_vc(GRAPHICS_VT);

        assert_eq!(
            kd_mode_vt(GRAPHICS_VT),
            KD_TEXT,
            "the VT stayed in graphics mode with nobody drawing on it"
        );
        release(GRAPHICS_VT);
    }

    #[test]
    fn giving_a_vt_back_drops_the_handshake_and_the_owner() {
        let _g = test_lock();
        a_compositor_owns(GRAPHICS_VT, 4321);

        reset_vc(GRAPHICS_VT);

        assert_eq!(tty_vt_mode(GRAPHICS_VT).lock().mode, VT_AUTO);
        assert_eq!(vt_owner(GRAPHICS_VT), 0);
        release(GRAPHICS_VT);
    }

    #[test]
    fn giving_back_a_vt_that_does_not_exist_lands_on_the_last_one() {
        // `reset_vc` is reached from a switch request, whose VT number comes
        // from userspace. Clamping rather than indexing is what keeps that
        // from being a panic.
        let _g = test_lock();
        reset_vc(usize::MAX);
        assert_eq!(vt_owner(kernel_hal::console::NUM_VTS - 1), 0);
    }

    // ---- a corpse is not a seat -------------------------------------------

    #[test]
    fn a_dead_owner_does_not_count_as_a_graphics_session() {
        // The only consumer is the `DROP_MASTER` console restore, which stands
        // down when a seat owns the VT because the seat will restore text
        // itself. When the compositor is what died, the seat IS the corpse:
        // the restore that exists for exactly this case was suppressed by it.
        // No process has this KoID, so the owner is not there.
        let _g = test_lock();
        TTY_STATES[GRAPHICS_VT]
            .vt_owner
            .store(0xdead_beef, Ordering::Relaxed);

        assert!(
            !graphics_vt_seat_owned(),
            "a KoID that names no process was taken for a live seat"
        );
        release(GRAPHICS_VT);
    }

    #[test]
    fn an_unowned_graphics_vt_is_not_a_graphics_session() {
        let _g = test_lock();
        TTY_STATES[GRAPHICS_VT].vt_owner.store(0, Ordering::Relaxed);
        assert!(!graphics_vt_seat_owned());
    }

    #[test]
    fn a_process_that_was_never_there_does_not_exist() {
        // What `vt_owner_alive` leans on. If this ever started answering true
        // for an arbitrary number, every owner would look alive for ever.
        assert!(!crate::process::process_exists(0xdead_beef));
        assert!(!crate::process::process_exists(0));
    }

    // ---- the signal the owner asked to be told with ------------------------

    #[test]
    fn a_relsig_of_zero_names_no_signal() {
        // `vt_mode.relsig` is an `i16` straight from userspace, and 0 is the
        // "do not signal me" of a `struct vt_mode` left zeroed. An owner that
        // cannot be signalled is as unreachable as a dead one, which is why
        // the switch path now gives the VT back instead of skipping the check.
        assert!(vt_signal(0).is_none());
    }

    #[test]
    fn a_signal_number_out_of_range_names_no_signal() {
        for n in [-1i16, -32, 65, 100, i16::MIN, i16::MAX] {
            assert!(vt_signal(n).is_none(), "{} is not a signal", n);
        }
    }

    #[test]
    fn a_signal_number_that_wraps_into_range_is_refused() {
        // This is what the range check is actually for. `vt_signal` narrows
        // with `n as u8`, which TRUNCATES: 257 would come out as SIGHUP and
        // 265 as SIGKILL, and `Signal::try_from` cannot tell the difference
        // because it only ever sees the low byte.
        //
        // It is also why moving either end of the range changes nothing on
        // its own -- `try_from` already refuses 0 and 65..=255. The numbers
        // that need the guard are the ones above a byte.
        for n in [256i16, 257, 265, 320, 521] {
            assert!(
                vt_signal(n).is_none(),
                "relsig {} wrapped into signal {:?}",
                n,
                vt_signal(n)
            );
        }
    }

    #[test]
    fn the_signals_a_compositor_actually_asks_for_resolve() {
        // wlroots/seatd use SIGUSR1/SIGUSR2 (10 and 12) for relsig/acqsig.
        assert_eq!(vt_signal(10), Some(crate::signal::Signal::SIGUSR1));
        assert_eq!(vt_signal(12), Some(crate::signal::Signal::SIGUSR2));
    }

    #[test]
    fn every_signal_number_resolves_or_is_refused() {
        // No number from userspace may panic on the way in.
        for n in i16::MIN..=i16::MAX {
            let got = vt_signal(n);
            assert_eq!(got.is_some(), (1..=64).contains(&n), "relsig {}", n);
        }
    }

    // ---- VT_SETMODE takes two modes, not any byte --------------------------

    #[test]
    fn vt_setmode_takes_auto_and_process() {
        assert!(vt_mode_accepted(VT_AUTO));
        assert!(vt_mode_accepted(VT_PROCESS));
    }

    #[test]
    fn vt_setmode_refuses_a_mode_it_does_not_know() {
        // Linux answers EINVAL. Storing it meant every later
        // `mode == VT_PROCESS` read false, so the VT looked unowned to the
        // switch handshake while the caller believed it had taken it.
        for mode in [2u8, 3, 0x10, 0xff] {
            assert!(!vt_mode_accepted(mode), "mode {:#x} was accepted", mode);
        }
    }

    #[test]
    fn the_two_vt_modes_are_the_numbers_linux_uses() {
        // Against the literals, not against themselves: these are the ABI's.
        assert_eq!(VT_AUTO, 0);
        assert_eq!(VT_PROCESS, 1);
    }

    // ---- the VT number from userspace --------------------------------------

    #[test]
    fn a_vt_number_past_the_last_one_is_clamped() {
        assert_eq!(vt_clamp(usize::MAX), kernel_hal::console::NUM_VTS - 1);
        assert_eq!(
            vt_clamp(kernel_hal::console::NUM_VTS),
            kernel_hal::console::NUM_VTS - 1
        );
    }

    #[test]
    fn a_real_vt_number_is_left_alone() {
        for vt in 0..kernel_hal::console::NUM_VTS {
            assert_eq!(vt_clamp(vt), vt);
        }
    }

    #[test]
    fn the_graphics_vt_is_the_last_one() {
        // `GRAPHICS_VT` is the reserved one with no login shell, and several
        // decisions here compare against it. If it stopped being the last VT
        // the reservation in `zCore/src/main.rs` would move without them.
        assert_eq!(GRAPHICS_VT, kernel_hal::console::NUM_VTS - 1);
    }

    // ---- KDSETMODE / KDGETMODE -------------------------------------------

    /// `KDSETMODE` handed its argument straight to the console with no
    /// validation. Linux's `vt_ioctl.c` accepts `KD_TEXT`, its two obsolete
    /// aliases and `KD_GRAPHICS`, and answers everything else with `-EINVAL`;
    /// here anything at all was accepted and stored, and since every decision
    /// downstream compares against `KD_TEXT`, a VT in a value that is neither
    /// constant stops being drawn AND matches no arm of `set_kd_mode_vt`, so
    /// nothing repaints it either. A blank screen, reported as success.
    ///
    /// The two halves of the fix are deliberately redundant: this arm is what
    /// REPORTS the refusal, and `set_kd_mode_vt` refuses the store on its own,
    /// so no caller can put a VT into a mode that is not one of the two. A
    /// mutation that stores the bad mode here anyway is therefore a no-op, and
    /// that is the point -- the check is not load-bearing for the console's
    /// state, only for the errno.
    #[test]
    fn kdsetmode_refuses_a_mode_that_is_not_a_mode() {
        let _g = test_lock();
        set_kd_mode_vt(0, KD_TEXT);

        assert!(
            tty_ioctl(0, KDSETMODE as u32, 0x2a).is_err(),
            "an unknown mode has to be EINVAL, not a silently blanked console"
        );
        assert_eq!(kd_mode_vt(0), KD_TEXT, "and the VT must be left as it was");

        // The two real modes still go through.
        assert!(tty_ioctl(0, KDSETMODE as u32, KD_GRAPHICS as usize).is_ok());
        assert_eq!(kd_mode_vt(0), KD_GRAPHICS);
        assert!(tty_ioctl(0, KDSETMODE as u32, KD_TEXT as usize).is_ok());
        assert_eq!(kd_mode_vt(0), KD_TEXT);
    }

    /// The obsolete text aliases are a request for text mode, and
    /// `KD_GETMODE` has to answer with the canonical value Linux stores
    /// (`vc_mode = KD_TEXT`), not with the alias that was sent -- a caller
    /// comparing the reply against `KD_TEXT` would otherwise conclude the
    /// console is in graphics mode.
    #[test]
    fn kdsetmode_folds_the_obsolete_text_aliases() {
        let _g = test_lock();

        for alias in [KD_TEXT0, KD_TEXT1] {
            set_kd_mode_vt(0, KD_GRAPHICS);
            assert!(tty_ioctl(0, KDSETMODE as u32, alias as usize).is_ok());
            assert_eq!(kd_mode_vt(0), KD_TEXT, "alias {:#x}", alias);

            let mut out: i32 = -1;
            assert!(tty_ioctl(0, KDGETMODE as u32, &mut out as *mut i32 as usize).is_ok());
            assert_eq!(out, KD_TEXT as i32, "KD_GETMODE after alias {:#x}", alias);
        }

        set_kd_mode_vt(0, KD_TEXT);
    }

    // ---- KDSKBMODE / KDGKBMODE -------------------------------------------

    /// `vt_do_kdskbmode` accepts five modes and refuses the rest with EINVAL.
    /// Storing anything else used to switch the VT's line discipline off:
    /// "cooked" is decided by comparing the stored value against `K_XLATE`
    /// and `K_UNICODE`, so a number that is neither stopped every key press
    /// from becoming a character. A dead keyboard, reported as success.
    #[test]
    fn kdskbmode_refuses_a_mode_that_is_not_a_mode() {
        let _g = test_lock();
        TTY_STATES[0].kbd_mode.store(K_XLATE, Ordering::Relaxed);

        for bogus in [5usize, 0x2a, usize::MAX] {
            assert!(
                tty_ioctl(0, KDSKBMODE as u32, bogus).is_err(),
                "mode {:#x} has to be EINVAL",
                bogus
            );
            assert_eq!(
                tty_kbd_mode(0),
                K_XLATE,
                "and the VT must be left as it was"
            );
            assert!(tty_kbd_cooked(0), "the keyboard keeps cooking");
        }
    }

    /// The five modes Linux knows all go through, and `KDGKBMODE` reads back
    /// exactly what was set.
    #[test]
    fn kdskbmode_takes_the_five_modes_linux_knows() {
        let _g = test_lock();
        for mode in [K_RAW, K_MEDIUMRAW, K_XLATE, K_UNICODE, K_OFF] {
            assert!(
                tty_ioctl(0, KDSKBMODE as u32, mode as usize).is_ok(),
                "{:#x}",
                mode
            );
            let mut out: i32 = -1;
            assert!(tty_ioctl(0, KDGKBMODE as u32, &mut out as *mut i32 as usize).is_ok());
            assert_eq!(out, mode);
        }
        // Only the two translating modes cook; the X-server modes do not.
        TTY_STATES[0].kbd_mode.store(K_RAW, Ordering::Relaxed);
        assert!(!tty_kbd_cooked(0));
        TTY_STATES[0].kbd_mode.store(K_XLATE, Ordering::Relaxed);
        assert!(tty_kbd_cooked(0));
    }
}

#[cfg(test)]
mod stdout_node_tests {
    //! The write side of a virtual terminal, as an inode.

    use super::*;

    /// A read has to be an errno. `unimplemented!()` here was a kernel panic
    /// waiting for the first caller that did not go through `File::read`'s
    /// `WRONLY` check -- a `/dev` node backed by a `Stdout`, a `sendfile`
    /// source, a page-cache fill.
    #[test]
    fn reading_the_write_side_of_a_terminal_is_an_errno_not_a_panic() {
        let out = Stdout { vt: 0 };
        let mut buf = [0u8; 8];
        assert_eq!(out.read_at(0, &mut buf), Err(FsError::NotSupported));
    }
}
