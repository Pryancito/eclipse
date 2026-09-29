use alloc::sync::Arc;
use core::any::Any;
use kernel_hal::console::{self, ConsoleWinSize};
use kernel_hal::user::{UserInPtr, UserOutPtr};
use lock::Mutex;
use rcore_fs::vfs::{make_rdev, FileType, FsError, INode, Metadata, PollStatus, Result, Timespec};
use rcore_fs_devfs::DevFS;
use zcore_drivers::{scheme::UartScheme, DeviceError};

use crate::fs::ioctl::{Termios, TCFLSH, TCGETS, TCSETS, TCSETSF, TCSETSW, TIOCGWINSZ, TIOCSWINSZ};

/// First minor of the serial range on major 4.
///
/// Major 4 is shared: `4:0`..=`4:63` are the virtual consoles `/dev/tty0`..
/// `/dev/tty63`, and the serial ports start at `4:64` as `/dev/ttyS0`
/// (`Documentation/admin-guide/devices.txt`). These nodes are added to devfs as
/// `/dev/ttyS{i}` but numbered `4:{i}`, so `/dev/ttyS0` claimed to be
/// `/dev/tty0` -- and the collision is real, not only nominal: `stdio.rs` gives
/// the consoles `4:{vt + 1}`, so `/dev/ttyS1` and `/dev/tty1` were **the same
/// device by number**. Anything that identifies a terminal by its numbers
/// rather than its name then names the wrong one: the `tty_nr` of
/// `/proc/<pid>/stat`, which is what `ps` prints as the controlling terminal,
/// and `agetty`, which decides from the numbers whether the line is a serial
/// line or a console.
const TTYS_MINOR_BASE: usize = 64;

/// Uart device.
pub struct UartDev {
    index: usize,
    port: Arc<dyn UartScheme>,
    /// Byte peeked by `poll` via consuming `try_recv`, held until `read_at`.
    pending: Mutex<Option<u8>>,
    /// This line's terminal settings, its own: the kernel's console does not
    /// go through these nodes (it writes via `console`), so nothing else reads
    /// them and two serial ports do not share one `stty`.
    termios: Mutex<Termios>,
    /// `TIOCSWINSZ` from this line, or `None` to answer with the console's.
    winsize: Mutex<Option<ConsoleWinSize>>,
    inode_id: usize,
}

impl UartDev {
    pub fn new(index: usize, port: Arc<dyn UartScheme>) -> Self {
        Self {
            index,
            port,
            pending: Mutex::new(None),
            termios: Mutex::new(Termios::default_tty()),
            winsize: Mutex::new(None),
            inode_id: DevFS::new_inode_id(),
        }
    }

    /// Non-destructive readability check: reuse a previously peeked byte, or
    /// peek via `try_recv` and stash it so a later `read_at` does not lose it.
    fn can_recv(&self) -> Result<bool> {
        let mut pending = self.pending.lock();
        if pending.is_some() {
            return Ok(true);
        }
        match self.port.try_recv() {
            Ok(Some(b)) => {
                *pending = Some(b);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(convert_error(e)),
        }
    }
}

impl INode for UartDev {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        info!(
            "uart read_at: offset={:#x} buf_len={:#x}",
            offset,
            buf.len()
        );

        let mut len = 0;
        for b in buf.iter_mut() {
            let next = {
                let mut pending = self.pending.lock();
                if let Some(p) = pending.take() {
                    Ok(Some(p))
                } else {
                    drop(pending);
                    self.port.try_recv().map_err(convert_error)
                }
            };
            let next = match next {
                Ok(v) => v,
                // The bytes already in `buf` are gone the moment this returns
                // an error, and nothing can ask for them again: they left the
                // UART's receive register. A read that has something reports
                // what it has, as a tty read does, and the error comes back on
                // the next call, when it costs nothing.
                Err(e) => {
                    if len > 0 {
                        return Ok(len);
                    }
                    return Err(e);
                }
            };
            match next {
                Some(b_) => {
                    *b = b_;
                    len += 1;
                }
                None => break,
            }
        }
        Ok(len)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        info!(
            "uart write_at: offset={:#x} buf_len={:#x}",
            offset,
            buf.len()
        );

        // A short write, for the same reason as the short read above: the bytes
        // before the failing one are already out of the port. Answering with an
        // error made the caller resend from the start, so what actually arrived
        // at the other end was the beginning of the buffer twice.
        for (sent, b) in buf.iter().enumerate() {
            if let Err(e) = self.port.send(*b) {
                if sent > 0 {
                    return Ok(sent);
                }
                return Err(convert_error(e));
            }
        }
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.can_recv()?,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o600, // owner read & write
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(4, TTYS_MINOR_BASE + self.index),
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        match cmd as usize {
            // `isatty(3)` is `tcgetattr(3)` is this, and nothing else: a line
            // that answers it with `ENOSYS` is not a terminal as far as every
            // program on the machine is concerned, however the kernel thinks
            // of it. `File::is_terminal` lists `UartDev` by name -- so the two
            // halves of the tree disagreed, and the half userspace can see was
            // the wrong one. A shell on a serial line then runs as if its input
            // were a file: no line editing, no colour, no job control.
            TCGETS => {
                copy(UserOutPtr::<Termios>::from(data).write(*self.termios.lock()))?;
                Ok(0)
            }
            // A UART has no queue of its own to drain or discard, so the three
            // forms differ only in what `TCSETSF` throws away: the byte `poll`
            // peeked. Answering `ENOSYS` to these is what makes `stty` on a
            // serial line fail, and with it every `getty` that sets the line up
            // before handing it to `login`.
            TCSETS | TCSETSW | TCSETSF => {
                let t = copy(UserInPtr::<Termios>::from(data).read())?;
                *self.termios.lock() = t;
                if cmd as usize == TCSETSF {
                    *self.pending.lock() = None;
                }
                Ok(0)
            }
            TCFLSH => {
                // TCIFLUSH (0) and TCIOFLUSH (2) take the input side.
                if data != 1 {
                    *self.pending.lock() = None;
                }
                Ok(0)
            }
            TIOCGWINSZ => {
                let ws = self
                    .winsize
                    .lock()
                    .unwrap_or_else(console::console_win_size);
                copy(UserOutPtr::<ConsoleWinSize>::from(data).write(ws))?;
                Ok(0)
            }
            // The framebuffer-derived default is right for the screen and much
            // too large for a serial viewer, which is why `resize` and `stty`
            // send this. A 0x0 size gives the console's answer back.
            TIOCSWINSZ => {
                let ws = copy(UserInPtr::<ConsoleWinSize>::from(data).read())?;
                *self.winsize.lock() = if ws.ws_row == 0 && ws.ws_col == 0 {
                    None
                } else {
                    Some(ws)
                };
                Ok(0)
            }
            _ => {
                warn!("uart ioctl {:#x} unimplemented", cmd);
                Err(FsError::NotSupported)
            }
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A copy to or from userspace that failed is `EINVAL` here, as it is on the
/// console's own nodes.
fn copy<T>(r: core::result::Result<T, kernel_hal::user::Error>) -> Result<T> {
    r.map_err(|_| FsError::InvalidParam)
}

fn convert_error(e: DeviceError) -> FsError {
    match e {
        DeviceError::NotSupported => FsError::NotSupported,
        DeviceError::NotReady => FsError::Busy,
        DeviceError::InvalidParam => FsError::InvalidParam,
        DeviceError::BufferTooSmall
        | DeviceError::DmaError
        | DeviceError::IoError
        | DeviceError::AlreadyExists
        | DeviceError::NoResources => FsError::DeviceError,
    }
}

#[cfg(test)]
mod uart_dev_tests {
    //! `/dev/ttyS{i}` was numbered `4:{i}`, which on major 4 is a virtual
    //! console; and both halves of the transfer threw away the bytes that had
    //! already moved when a later one failed.

    use super::*;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;
    use lock::Mutex as SpinMutex;
    use zcore_drivers::scheme::{EventScheme, Scheme};
    use zcore_drivers::DeviceResult;

    /// A port that hands out the bytes it was given and then fails, and records
    /// every byte that was actually sent to it.
    struct FakePort {
        incoming: SpinMutex<VecDeque<u8>>,
        /// `Err` once `incoming` runs out, instead of `Ok(None)`.
        fail_when_empty: bool,
        sent: SpinMutex<Vec<u8>>,
        /// Refuse to send after this many bytes.
        send_budget: SpinMutex<usize>,
    }

    impl FakePort {
        fn new(incoming: &[u8], fail_when_empty: bool, send_budget: usize) -> Arc<Self> {
            Arc::new(FakePort {
                incoming: SpinMutex::new(incoming.iter().copied().collect()),
                fail_when_empty,
                sent: SpinMutex::new(Vec::new()),
                send_budget: SpinMutex::new(send_budget),
            })
        }
    }

    impl Scheme for FakePort {
        fn name(&self) -> &str {
            "fake-uart"
        }
    }

    impl EventScheme for FakePort {
        type Event = ();
        fn trigger(&self, _event: ()) {}
        fn subscribe(
            &self,
            _handler: zcore_drivers::utils::EventHandler<()>,
            _once: bool,
        ) -> Option<u64> {
            None
        }
        fn unsubscribe(&self, _id: u64) {}
    }

    impl UartScheme for FakePort {
        fn try_recv(&self) -> DeviceResult<Option<u8>> {
            match self.incoming.lock().pop_front() {
                Some(b) => Ok(Some(b)),
                None if self.fail_when_empty => Err(DeviceError::IoError),
                None => Ok(None),
            }
        }
        fn send(&self, ch: u8) -> DeviceResult {
            let mut budget = self.send_budget.lock();
            if *budget == 0 {
                return Err(DeviceError::IoError);
            }
            *budget -= 1;
            self.sent.lock().push(ch);
            Ok(())
        }
    }

    fn dev(port: Arc<FakePort>, index: usize) -> UartDev {
        UartDev::new(index, port)
    }

    #[test]
    fn a_serial_port_is_numbered_in_the_serial_range_and_not_over_a_console() {
        // `stdio.rs` numbers the consoles `4:{vt + 1}`, so `/dev/ttyS1` used to
        // be `4:1`, exactly `/dev/tty1`.
        let first = dev(FakePort::new(&[], false, 0), 0)
            .metadata()
            .unwrap()
            .rdev;
        let second = dev(FakePort::new(&[], false, 0), 1)
            .metadata()
            .unwrap()
            .rdev;
        assert_eq!(first, make_rdev(4, 64), "/dev/ttyS0 is 4:64");
        assert_eq!(second, make_rdev(4, 65));
        // The consoles, as `stdio.rs` numbers them, and none of them is a port.
        for vt in 0..8usize {
            assert_ne!(first, make_rdev(4, vt + 1));
            assert_ne!(second, make_rdev(4, vt + 1), "ttyS1 collided with tty1");
        }
    }

    #[test]
    fn a_read_that_already_has_bytes_reports_them_instead_of_losing_them() {
        // The three bytes are out of the receive register: nothing can ask for
        // them again, so an error in their place is data lost for good.
        let port = FakePort::new(b"abc", true, 0);
        let d = dev(port, 0);
        let mut buf = [0u8; 8];
        assert_eq!(d.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"abc");
        // And the error is still there on the next call, where it costs nothing.
        assert_eq!(d.read_at(0, &mut buf), Err(FsError::DeviceError));
    }

    #[test]
    fn a_read_that_has_nothing_still_reports_the_error() {
        let d = dev(FakePort::new(&[], true, 0), 0);
        let mut buf = [0u8; 4];
        assert_eq!(d.read_at(0, &mut buf), Err(FsError::DeviceError));
    }

    #[test]
    fn a_write_that_already_sent_bytes_reports_a_short_count() {
        // Answering with an error made the caller resend from the start, so the
        // beginning of the buffer arrived at the other end twice.
        let port = FakePort::new(&[], false, 2);
        let d = dev(port.clone(), 0);
        assert_eq!(d.write_at(0, b"hello").unwrap(), 2);
        assert_eq!(&port.sent.lock()[..], b"he");
        // Nothing left in the budget, so now the error comes back.
        assert_eq!(d.write_at(0, b"x"), Err(FsError::DeviceError));
        assert_eq!(&port.sent.lock()[..], b"he", "no byte went out after it");
    }

    #[test]
    fn a_whole_write_still_answers_with_the_whole_length() {
        let port = FakePort::new(&[], false, 64);
        let d = dev(port.clone(), 0);
        assert_eq!(d.write_at(0, b"hello").unwrap(), 5);
        assert_eq!(d.write_at(0, b"").unwrap(), 0);
        assert_eq!(&port.sent.lock()[..], b"hello");
    }

    #[test]
    fn a_serial_line_answers_the_question_that_makes_it_a_terminal() {
        // `isatty(3)` is `tcgetattr(3)` is `TCGETS`, and this answered `ENOSYS`
        // to every ioctl while `File::is_terminal` listed `UartDev` by name.
        // The half userspace can see was the wrong one.
        let d = dev(FakePort::new(&[], false, 0), 0);
        let mut t = Termios::default_tty();
        t.c_lflag = 0;
        assert_eq!(
            d.io_control(TCGETS as u32, &mut t as *mut Termios as usize),
            Ok(0)
        );
        assert_eq!(
            t.c_lflag,
            Termios::default_tty().c_lflag,
            "a fresh line is a cooked terminal"
        );
        // And what `stty` sets is what the next `tcgetattr` reads back.
        t.c_lflag = 0;
        assert_eq!(
            d.io_control(TCSETS as u32, &t as *const Termios as usize),
            Ok(0)
        );
        let mut back = Termios::default_tty();
        assert_eq!(
            d.io_control(TCGETS as u32, &mut back as *mut Termios as usize),
            Ok(0)
        );
        assert_eq!(back.c_lflag, 0, "raw, as it was set");
    }

    #[test]
    fn flushing_the_input_side_drops_the_byte_poll_peeked() {
        // The one thing a UART has to throw away: `poll` consumes a byte from
        // the receive register to answer, and `TCIFLUSH` means the program does
        // not want what was typed ahead -- a password prompt, usually.
        let d = dev(FakePort::new(b"typed-ahead", false, 0), 0);
        assert!(d.poll().unwrap().read);
        assert_eq!(d.io_control(TCFLSH as u32, 0), Ok(0));
        assert!(d.pending.lock().is_none(), "the peeked byte is gone");
        // TCOFLUSH is the other side, and leaves the input alone.
        let d = dev(FakePort::new(b"x", false, 0), 0);
        assert!(d.poll().unwrap().read);
        assert_eq!(d.io_control(TCFLSH as u32, 1), Ok(0));
        assert!(d.pending.lock().is_some(), "output flush, not input");
    }

    #[test]
    fn a_serial_line_is_a_character_device_only_its_owner_can_reach() {
        // A block device here would make the shell's own stdin seekable and
        // buffered; and the mode is the one thing keeping another account from
        // reading what is typed at the port.
        let m = dev(FakePort::new(&[], false, 0), 0).metadata().unwrap();
        assert_eq!(m.type_, FileType::CharDevice);
        assert_eq!(m.mode, 0o600, "owner read and write, and nobody else");
        assert_eq!(m.size, 0, "a line has no length to seek within");
    }

    #[test]
    fn a_read_of_one_single_byte_before_the_error_still_reports_that_byte() {
        // One byte is the whole of what a prompt waits for, and it is already
        // out of the receive register: the boundary of "has something" is one,
        // not two.
        let d = dev(FakePort::new(b"a", true, 0), 0);
        let mut buf = [0u8; 8];
        assert_eq!(d.read_at(0, &mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'a');
        assert_eq!(d.read_at(0, &mut buf), Err(FsError::DeviceError));
    }

    #[test]
    fn a_write_of_one_single_byte_before_the_error_still_reports_it() {
        // Same boundary on the way out: answering with an error makes the
        // caller resend from the start, so that one byte arrives twice.
        let port = FakePort::new(&[], false, 1);
        let d = dev(port.clone(), 0);
        assert_eq!(d.write_at(0, b"ab").unwrap(), 1);
        assert_eq!(&port.sent.lock()[..], b"a");
    }

    #[test]
    fn a_serial_line_is_always_ready_to_take_what_is_written_to_it() {
        // `poll` is how a shell decides whether it may write without blocking.
        // A line that never says it can is a terminal nothing ever prints to.
        let d = dev(FakePort::new(&[], false, 0), 0);
        let st = d.poll().unwrap();
        assert!(st.write);
        assert!(!st.read, "nothing has been typed");
        assert!(!st.error);
        assert!(!st.hangup);
    }

    #[test]
    fn only_the_flushing_form_of_tcsets_drops_the_byte_poll_peeked() {
        // The three forms differ in exactly one thing: `TCSETSF` discards what
        // was typed ahead. `getty` uses it before handing the line to `login`,
        // which is what keeps a password out of the shell that follows.
        for (cmd, gone) in [(TCSETS, false), (TCSETSW, false), (TCSETSF, true)] {
            let d = dev(FakePort::new(b"typed-ahead", false, 0), 0);
            assert!(d.poll().unwrap().read);
            let t = Termios::default_tty();
            assert_eq!(
                d.io_control(cmd as u32, &t as *const Termios as usize),
                Ok(0)
            );
            assert_eq!(
                d.pending.lock().is_none(),
                gone,
                "cmd {:#x} took the wrong side",
                cmd
            );
        }
    }

    #[test]
    fn the_window_size_this_line_was_given_is_the_one_it_answers_with() {
        // The framebuffer-derived default is right for the screen and far too
        // big for a serial viewer, which is why `resize` and `stty` send this.
        let d = dev(FakePort::new(&[], false, 0), 0);
        let set = ConsoleWinSize {
            ws_row: 40,
            ws_col: 132,
            ..Default::default()
        };
        assert_eq!(
            d.io_control(TIOCSWINSZ as u32, &set as *const ConsoleWinSize as usize),
            Ok(0)
        );
        let mut back = ConsoleWinSize::default();
        assert_eq!(
            d.io_control(TIOCGWINSZ as u32, &mut back as *mut ConsoleWinSize as usize),
            Ok(0)
        );
        assert_eq!((back.ws_row, back.ws_col), (40, 132));
        // And 0x0 is how a viewer hands the question back to the console.
        let zero = ConsoleWinSize::default();
        assert_eq!(
            d.io_control(TIOCSWINSZ as u32, &zero as *const ConsoleWinSize as usize),
            Ok(0)
        );
        assert!(
            d.winsize.lock().is_none(),
            "0x0 is not a size, it is a reset"
        );
    }

    #[test]
    fn a_window_size_with_one_dimension_left_at_zero_is_still_this_lines_own() {
        // Only 0x0 means "ask the console". A viewer that knows its height and
        // not its width still gets to say so, and losing that answer sends it
        // back the framebuffer's size, which is what it was trying to avoid.
        let d = dev(FakePort::new(&[], false, 0), 0);
        for (row, col) in [(40u16, 0u16), (0, 132)] {
            let set = ConsoleWinSize {
                ws_row: row,
                ws_col: col,
                ..Default::default()
            };
            assert_eq!(
                d.io_control(TIOCSWINSZ as u32, &set as *const ConsoleWinSize as usize),
                Ok(0)
            );
            let mut back = ConsoleWinSize::default();
            assert_eq!(
                d.io_control(TIOCGWINSZ as u32, &mut back as *mut ConsoleWinSize as usize),
                Ok(0)
            );
            assert_eq!((back.ws_row, back.ws_col), (row, col));
        }
    }

    #[test]
    fn every_port_error_becomes_the_filesystem_error_that_means_the_same() {
        // What a program sees for a line that is not there, a line that is
        // busy, and a line that broke mid-transfer has to be three different
        // things, or a retry loop spins on a port that will never answer.
        for (dev_err, fs_err) in [
            (DeviceError::NotSupported, FsError::NotSupported),
            (DeviceError::NotReady, FsError::Busy),
            (DeviceError::InvalidParam, FsError::InvalidParam),
            (DeviceError::BufferTooSmall, FsError::DeviceError),
            (DeviceError::DmaError, FsError::DeviceError),
            (DeviceError::IoError, FsError::DeviceError),
            (DeviceError::AlreadyExists, FsError::DeviceError),
            (DeviceError::NoResources, FsError::DeviceError),
        ] {
            assert_eq!(convert_error(dev_err), fs_err, "{:?}", dev_err);
        }
    }

    #[test]
    fn an_ioctl_that_cannot_reach_the_callers_memory_is_invalid_not_unsupported() {
        // A null `struct termios *` is a caller bug, and `ENOSYS` in its place
        // says the line is not a terminal -- which is what `isatty(3)` reads,
        // and a shell believes it.
        let d = dev(FakePort::new(&[], false, 0), 0);
        assert_eq!(d.io_control(TCGETS as u32, 0), Err(FsError::InvalidParam));
        assert_eq!(d.io_control(TCSETS as u32, 0), Err(FsError::InvalidParam));
        assert_eq!(
            d.io_control(TIOCGWINSZ as u32, 0),
            Err(FsError::InvalidParam)
        );
    }

    #[test]
    fn an_ioctl_this_line_does_not_know_is_still_refused() {
        let d = dev(FakePort::new(&[], false, 0), 0);
        assert_eq!(d.io_control(0x1234, 0), Err(FsError::NotSupported));
    }

    #[test]
    fn the_byte_poll_peeked_is_still_the_first_one_read() {
        // `poll` consumes a byte from the port to answer, so it has to be
        // handed back by the next read rather than dropped.
        let d = dev(FakePort::new(b"xy", false, 0), 0);
        assert!(d.poll().unwrap().read);
        let mut buf = [0u8; 4];
        assert_eq!(d.read_at(0, &mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"xy");
    }
}
