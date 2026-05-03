//! Implement INode for Stdin & Stdout

use super::ioctl::*;
use crate::{sync::Event, sync::EventBus};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use kernel_hal::console::{self, ConsoleWinSize};
use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::*;

lazy_static! {
    /// STDIN global reference
    pub static ref STDIN: Arc<Stdin> = {
        let stdin = Arc::new(Stdin::default());
        let cloned = stdin.clone();
        if let Some(uart) = kernel_hal::drivers::all_uart().first() {
            uart.clone().subscribe(
                Box::new(move |_| {
                    while let Some(c) = uart.try_recv().unwrap_or(None) {
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
                    if event.event_type == InputEventType::Key && event.value == 1 {
                        if let Some(c) = input_event_to_char(event.code) {
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

fn input_event_to_char(code: u16) -> Option<char> {
    use zcore_drivers::input::input_event_codes::key::*;
    match code {
        KEY_A => Some('a'), KEY_B => Some('b'), KEY_C => Some('c'), KEY_D => Some('d'),
        KEY_E => Some('e'), KEY_F => Some('f'), KEY_G => Some('g'), KEY_H => Some('h'),
        KEY_I => Some('i'), KEY_J => Some('j'), KEY_K => Some('k'), KEY_L => Some('l'),
        KEY_M => Some('m'), KEY_N => Some('n'), KEY_O => Some('o'), KEY_P => Some('p'),
        KEY_Q => Some('q'), KEY_R => Some('r'), KEY_S => Some('s'), KEY_T => Some('t'),
        KEY_U => Some('u'), KEY_V => Some('v'), KEY_W => Some('w'), KEY_X => Some('x'),
        KEY_Y => Some('y'), KEY_Z => Some('z'),
        KEY_1 => Some('1'), KEY_2 => Some('2'), KEY_3 => Some('3'), KEY_4 => Some('4'),
        KEY_5 => Some('5'), KEY_6 => Some('6'), KEY_7 => Some('7'), KEY_8 => Some('8'),
        KEY_9 => Some('9'), KEY_0 => Some('0'),
        KEY_ENTER | KEY_KPENTER => Some('\n'),
        KEY_SPACE => Some(' '),
        KEY_BACKSPACE => Some('\x08'),
        KEY_TAB => Some('\t'),
        KEY_DOT | KEY_KPDOT => Some('.'),
        KEY_SLASH | KEY_KPSLASH => Some('/'),
        KEY_MINUS | KEY_KPMINUS => Some('-'),
        KEY_EQUAL => Some('='),
        KEY_COMMA => Some(','),
        KEY_SEMICOLON => Some(';'),
        KEY_APOSTROPHE => Some('\''),
        KEY_BACKSLASH => Some('\\'),
        KEY_GRAVE => Some('`'),
        _ => None,
    }
}

/// Stdin struct, for Stdin buffer
#[derive(Default)]
pub struct Stdin {
    buf: Mutex<VecDeque<char>>,
    eventbus: Mutex<EventBus>,
}

impl Stdin {
    /// push a char in Stdin buffer
    pub fn push(&self, c: char) {
        self.buf.lock().push_back(c);
        self.eventbus.lock().set(Event::READABLE);
    }
    /// pop a char in Stdin buffer
    pub fn pop(&self) -> char {
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
        if self.can_read() {
            buf[0] = self.pop() as u8;
            Ok(1)
        } else {
            Err(FsError::Again)
        }
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        unimplemented!()
    }
    fn poll(&self) -> Result<PollStatus> {
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
                if self.stdin.can_read() {
                    return Poll::Ready(self.stdin.poll());
                }
                let waker = cx.waker().clone();
                self.stdin.eventbus.lock().subscribe(Box::new({
                    move |_| {
                        waker.wake_by_ref();
                        true
                    }
                }));
                Poll::Pending
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
            TCGETS | TIOCSPGRP => {
                warn!("stdin TCGETS | TIOCSPGRP, pretend to be tty.");
                // pretend to be tty
                Ok(0)
            }
            TIOCGPGRP => {
                warn!("stdin TIOCGPGRP, pretend to be have a tty process group.");
                // pretend to be have a tty process group
                // TODO: verify pointer
                unsafe { *(data as *mut u32) = 0 };
                Ok(0)
            }
            _ => Err(FsError::NotSupported),
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
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
            TCGETS | TIOCSPGRP => {
                warn!("stdout TCGETS | TIOCSPGRP, pretend to be tty.");
                // pretend to be tty
                Ok(0)
            }
            TIOCGPGRP => {
                // pretend to be have a tty process group
                // TODO: verify pointer
                unsafe { *(data as *mut u32) = 0 };
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
