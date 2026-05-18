//! Implement INode for Stdin & Stdout

use super::ioctl::*;
use crate::{sync::Event, sync::EventBus};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicI32, Ordering};
use core::task::{Context, Poll};
use kernel_hal::console::{self, ConsoleWinSize};
use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::*;
use zircon_object::object::KernelObject;
use zircon_object::task::Thread;

// Foreground process group for the (single) controlling TTY.
// This is a minimal job-control hook for Ctrl+C / SIGINT delivery.
static TTY_FG_PGRP: AtomicI32 = AtomicI32::new(0);

pub fn get_foreground_pgrp() -> i32 {
    TTY_FG_PGRP.load(Ordering::Relaxed)
}

pub fn set_foreground_pgrp(pgid: i32) {
    TTY_FG_PGRP.store(pgid, Ordering::Relaxed);
}

// Global Ctrl+C latch. Since many programs (e.g. udhcpc) never read stdin while running,
// we need a way for syscalls like recvfrom/poll to observe a pending terminal interrupt.
static CTRL_C_PENDING: AtomicBool = AtomicBool::new(false);
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static SHIFT_DOWN: AtomicBool = AtomicBool::new(false);
/// AltGr (Alt derecho) — layout `es` de Linux/XKB.
static ALTGR_DOWN: AtomicBool = AtomicBool::new(false);
static CAPSLOCK_ON: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub fn ctrl_c_pending_take() -> bool {
    CTRL_C_PENDING.swap(false, Ordering::SeqCst)
}

#[allow(dead_code)]
pub fn ctrl_c_pending_set() {
    CTRL_C_PENDING.store(true, Ordering::SeqCst);
}

lazy_static! {
    /// STDIN global reference
    pub static ref STDIN: Arc<Stdin> = {
        let stdin = Arc::new(Stdin::default());
        let cloned = stdin.clone();
        if let Some(uart) = kernel_hal::drivers::all_uart().first() {
            uart.clone().subscribe(
                Box::new(move |_| {
                    while let Some(c) = uart.try_recv().unwrap_or(None) {
                        warn!("UART received byte: 0x{:02x}", c);
                        cloned.push(c as char);
                    }
                }),
                false,
            );
        }

        // Suscribirse a dispositivos de entrada (teclados USB/virtio)
        for input in kernel_hal::drivers::all_input().as_vec().iter() {
            let cloned = stdin.clone();
            use zcore_drivers::prelude::{InputEventType, InputEvent};
            input.subscribe(
                Box::new(move |event: &InputEvent| {
                    if event.event_type != InputEventType::Key {
                        return;
                    }
                    // Linux input: value 1 = key press, 0 = release, 2 = autorepeat.
                    use zcore_drivers::input::input_event_codes::key::*;
                    match event.code {
                        KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                            if event.value == 1 {
                                CTRL_DOWN.store(true, Ordering::SeqCst);
                            } else if event.value == 0 {
                                CTRL_DOWN.store(false, Ordering::SeqCst);
                            }
                            return;
                        }
                        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                            if event.value == 1 {
                                SHIFT_DOWN.store(true, Ordering::SeqCst);
                            } else if event.value == 0 {
                                SHIFT_DOWN.store(false, Ordering::SeqCst);
                            }
                            return;
                        }
                        KEY_RIGHTALT => {
                            if event.value == 1 {
                                ALTGR_DOWN.store(true, Ordering::SeqCst);
                            } else if event.value == 0 {
                                ALTGR_DOWN.store(false, Ordering::SeqCst);
                            }
                            return;
                        }
                        KEY_CAPSLOCK => {
                            if event.value == 1 {
                                let on = CAPSLOCK_ON.load(Ordering::SeqCst);
                                CAPSLOCK_ON.store(!on, Ordering::SeqCst);
                            }
                            return;
                        }
                        _ => {}
                    }

                    if event.value == 1 || event.value == 2 {
                        // Ctrl+C => ETX (0x03)
                        if CTRL_DOWN.load(Ordering::SeqCst) && event.code == KEY_C {
                            cloned.push('\u{3}');
                            return;
                        }
                        let mods = KeyMods {
                            shift: SHIFT_DOWN.load(Ordering::SeqCst),
                            altgr: ALTGR_DOWN.load(Ordering::SeqCst),
                            caps: CAPSLOCK_ON.load(Ordering::SeqCst),
                        };
                        if let Some(c) = input_event_to_char_es(event.code, mods) {
                            cloned.push(c);
                        }
                    }
                }),
                false,
            );
        }
        stdin
    };
    /// STDOUT global reference
    pub static ref STDOUT: Arc<Stdout> = Default::default();
}

/// Estado de modificadores para el layout español (XKB `es`).
struct KeyMods {
    shift: bool,
    altgr: bool,
    caps: bool,
}

impl KeyMods {
    fn letter(self, lower: char) -> char {
        if self.caps ^ self.shift {
            lower.to_ascii_uppercase()
        } else {
            lower
        }
    }

    /// Elige entre cuatro niveles (como XKB: base, Shift, AltGr, Shift+AltGr).
    fn pick(self, base: char, shifted: char, altgr: char, shift_altgr: char) -> char {
        if self.altgr && self.shift {
            shift_altgr
        } else if self.altgr {
            altgr
        } else if self.shift {
            shifted
        } else {
            base
        }
    }
}

/// Layout QWERTY español (España), alineado con `symbols/es` de xkeyboard-config.
fn input_event_to_char_es(code: u16, mods: KeyMods) -> Option<char> {
    use zcore_drivers::input::input_event_codes::key::*;
    match code {
        KEY_A => Some(mods.letter('a')),
        KEY_B => Some(mods.letter('b')),
        KEY_C => Some(mods.letter('c')),
        KEY_D => Some(mods.letter('d')),
        KEY_E => Some(mods.letter('e')),
        KEY_F => Some(mods.letter('f')),
        KEY_G => Some(mods.letter('g')),
        KEY_H => Some(mods.letter('h')),
        KEY_I => Some(mods.letter('i')),
        KEY_J => Some(mods.letter('j')),
        KEY_K => Some(mods.letter('k')),
        KEY_L => Some(mods.letter('l')),
        KEY_M => Some(mods.letter('m')),
        KEY_N => Some(mods.letter('n')),
        KEY_O => Some(mods.letter('o')),
        KEY_P => Some(mods.letter('p')),
        KEY_Q => Some(mods.letter('q')),
        KEY_R => Some(mods.letter('r')),
        KEY_S => Some(mods.letter('s')),
        KEY_T => Some(mods.letter('t')),
        KEY_U => Some(mods.letter('u')),
        KEY_V => Some(mods.letter('v')),
        KEY_W => Some(mods.letter('w')),
        KEY_X => Some(mods.letter('x')),
        KEY_Y => Some(mods.letter('y')),
        KEY_Z => Some(mods.letter('z')),
        KEY_1 => Some(mods.pick('1', '!', '|', '@')),
        KEY_2 => Some(mods.pick('2', '"', '@', '@')),
        KEY_3 => Some(mods.pick('3', '·', '#', '#')),
        KEY_4 => Some(mods.pick('4', '$', '~', '~')),
        KEY_5 => Some(mods.pick('5', '%', '½', '½')),
        KEY_6 => Some(mods.pick('6', '&', '¾', '¾')),
        KEY_7 => Some(mods.pick('7', '/', '{', '{')),
        KEY_8 => Some(mods.pick('8', '(', '[', '[')),
        KEY_9 => Some(mods.pick('9', ')', ']', ']')),
        KEY_0 => Some(mods.pick('0', '=', '}', '}')),
        KEY_MINUS => Some(mods.pick('\'', '?', '\\', '|')),
        KEY_EQUAL => Some(mods.pick('¡', '¿', '¡', '¿')),
        KEY_GRAVE => Some(mods.pick('º', 'ª', 'º', 'ª')),
        KEY_LEFTBRACE => Some(mods.pick('`', '^', '[', '^')),
        KEY_RIGHTBRACE => Some(mods.pick('+', '*', '^', '~')),
        KEY_BACKSLASH => Some(mods.pick('\\', '|', '|', '|')),
        KEY_SEMICOLON => Some(mods.pick('ñ', 'Ñ', '~', '`')),
        KEY_APOSTROPHE => Some(mods.pick('´', '¨', '[', '`')),
        KEY_102ND => Some(mods.pick('<', '>', '\\', '|')),
        KEY_COMMA => Some(mods.pick(',', ';', ',', ';')),
        KEY_DOT | KEY_KPDOT => Some(mods.pick('.', ':', '.', ':')),
        KEY_SLASH => Some(mods.pick('-', '_', '-', '_')),
        KEY_ENTER | KEY_KPENTER => Some('\n'),
        KEY_SPACE => Some(' '),
        KEY_BACKSPACE => Some('\x08'),
        KEY_TAB => Some('\t'),
        KEY_KP0 => Some('0'),
        KEY_KP1 => Some('1'),
        KEY_KP2 => Some('2'),
        KEY_KP3 => Some('3'),
        KEY_KP4 => Some('4'),
        KEY_KP5 => Some('5'),
        KEY_KP6 => Some('6'),
        KEY_KP7 => Some('7'),
        KEY_KP8 => Some('8'),
        KEY_KP9 => Some('9'),
        KEY_KPSLASH => Some('/'),
        KEY_KPASTERISK => Some('*'),
        KEY_KPMINUS => Some('-'),
        KEY_KPPLUS => Some('+'),
        _ => None,
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
    buf: Mutex<VecDeque<char>>,
    eventbus: Mutex<EventBus>,
    /// Atomic flag set by `push()` so `SerialFuture` can detect new data
    /// without requiring `eventbus.lock()` from the IRQ path.
    data_ready: core::sync::atomic::AtomicBool,
}

impl Default for Stdin {
    fn default() -> Self {
        Self {
            buf: Mutex::new(VecDeque::new()),
            eventbus: Mutex::new(EventBus::default()),
            data_ready: core::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Stdin {
    /// Push a char into the Stdin buffer.
    ///
    /// Safe to call from IRQ context: acquires `buf` lock briefly (with
    /// interrupts disabled by the spinlock), sets an atomic flag, and
    /// *tries* to propagate to the EventBus via try_lock().  If the
    /// EventBus is contended the flag is left set for the next
    /// executor-side flush_ready_flag() call.
    pub fn push(&self, c: char) {
        if c == '\u{3}' {
            ctrl_c_pending_set();
        }
        self.buf.lock().push_back(c);
        // Signal availability. If we can grab the eventbus cheaply, notify
        // waiters immediately; otherwise leave the flag for later executor-side
        // flush_ready_flag() call to avoid lock inversion from input callbacks.
        self.data_ready.store(true, Ordering::Release);
        if let Some(mut eb) = self.eventbus.try_lock() {
            self.data_ready.store(false, Ordering::Relaxed);
            eb.set(Event::READABLE);
        }
    }

    /// Drain the atomic flag and propagate to EventBus.
    /// Called from executor context (SerialFuture::poll, pop, executor loop).
    pub fn flush_ready_flag(&self) {
        if self.data_ready.swap(false, Ordering::Acquire) {
            self.eventbus.lock().set(Event::READABLE);
        }
    }

    /// pop a char from the Stdin buffer
    pub fn pop(&self) -> char {
        // Propagate any pending push signals first.
        self.flush_ready_flag();
        let mut buf_lock = self.buf.lock();
        let c = buf_lock.pop_front().unwrap();
        if buf_lock.len() == 0 {
            self.eventbus.lock().clear(Event::READABLE);
        }
        c
    }
    /// specify whether the Stdin buffer is readable
    pub fn can_read(&self) -> bool {
        self.buf.lock().len() > 0
    }
}

/// Stdout struct, empty now
#[derive(Default)]
pub struct Stdout;

impl INode for Stdin {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.flush_ready_flag();
        if self.can_read() {
            buf[0] = self.pop() as u8;
            Ok(1)
        } else {
            Err(FsError::Again)
        }
    }
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let s = unsafe { core::str::from_utf8_unchecked(buf) };
        kernel_hal::console::console_write_str(s);
        Ok(buf.len())
    }
    fn poll(&self) -> Result<PollStatus> {
        self.flush_ready_flag();
        Ok(PollStatus {
            read: self.can_read(),
            write: false,
            error: false,
        })
    }
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct SerialFuture<'a> {
            stdin: &'a Stdin,
        }

        impl<'a> Future for SerialFuture<'a> {
            type Output = Result<PollStatus>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                // Propagate any IRQ-side pushes into the EventBus.
                self.stdin.flush_ready_flag();
                if self.stdin.can_read() {
                    return Poll::Ready(Ok(PollStatus {
                        read: true,
                        write: false,
                        error: false,
                    }));
                }
                let waker = cx.waker().clone();
                self.stdin.eventbus.lock().subscribe(Box::new({
                    move |_| {
                        waker.wake_by_ref();
                        true
                    }
                }));
                self.stdin.flush_ready_flag();
                if self.stdin.can_read() {
                    Poll::Ready(Ok(PollStatus {
                        read: true,
                        write: false,
                        error: false,
                    }))
                } else {
                    Poll::Pending
                }
            }
        }

        Box::pin(SerialFuture { stdin: self })
    }

    //
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        match cmd as usize {
            TIOCGWINSZ => {
                let winsize = data as *mut ConsoleWinSize;
                unsafe { *winsize = console::console_win_size() };
                Ok(0)
            }
            TCGETS => {
                warn!("stdin TCGETS, pretend to be tty.");
                Ok(0)
            }
            TIOCSPGRP => {
                // Set foreground process group.
                // `data` is a user pointer to an int.
                // TODO: validate pointer in a proper usercopy layer.
                let pgid = unsafe { *(data as *const i32) };
                TTY_FG_PGRP.store(pgid, Ordering::Relaxed);
                Ok(0)
            }
            TIOCGPGRP => {
                // Get foreground process group.
                let mut pgid = TTY_FG_PGRP.load(Ordering::Relaxed);
                if pgid == 0 {
                    // If no foreground group is set, pretend the caller is in foreground.
                    // This is a common hack for simple OSs to support interactive shells.
                    if let Some(arc) = kernel_hal::thread::get_current_thread() {
                        if let Ok(thread) = arc.downcast::<Thread>() {
                            pgid = thread.proc().id() as i32;
                        }
                    }
                }
                if pgid == 0 {
                    pgid = 1;
                }
                unsafe { *(data as *mut i32) = pgid };
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF => {
                debug!("stdin TCSETS/W/F, stubbed.");
                Ok(0)
            }
            _ => Err(FsError::NotSupported),
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    /// Get metadata of the INode
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: 12,
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
            rdev: make_rdev(5, 0),
        })
    }
}

impl INode for Stdout {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        unimplemented!()
    }
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        // we do not care the utf-8 things, we just want to print it!
        let s = unsafe { core::str::from_utf8_unchecked(buf) };
        kernel_hal::console::console_write_str(s);
        Ok(buf.len())
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: false,
            write: true,
            error: false,
        })
    }
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        match cmd as usize {
            TIOCGWINSZ => {
                let winsize = data as *mut ConsoleWinSize;
                unsafe { *winsize = console::console_win_size() };
                Ok(0)
            }
            TCGETS => {
                warn!("stdout TCGETS, pretend to be tty.");
                Ok(0)
            }
            TIOCSPGRP => {
                let pgid = unsafe { *(data as *const i32) };
                TTY_FG_PGRP.store(pgid, Ordering::Relaxed);
                Ok(0)
            }
            TIOCGPGRP => {
                // pretend to be have a tty process group
                let mut pgid = TTY_FG_PGRP.load(Ordering::Relaxed);
                if pgid == 0 {
                    if let Some(arc) = kernel_hal::thread::get_current_thread() {
                        if let Ok(thread) = arc.downcast::<Thread>() {
                            pgid = thread.proc().id() as i32;
                        }
                    }
                }
                if pgid == 0 {
                    pgid = 1;
                }
                unsafe { *(data as *mut i32) = pgid };
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF => {
                debug!("stdout TCSETS/W/F, stubbed.");
                Ok(0)
            }
            _ => Err(FsError::NotSupported),
        }
    }

    /// Get metadata of the INode
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: 13,
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
            rdev: make_rdev(5, 0),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}
