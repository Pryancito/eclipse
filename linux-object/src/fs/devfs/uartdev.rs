use alloc::sync::Arc;
use core::any::Any;
use lock::Mutex;
use rcore_fs::vfs::{make_rdev, FileType, FsError, INode, Metadata, PollStatus, Result, Timespec};
use rcore_fs_devfs::DevFS;
use zcore_drivers::{scheme::UartScheme, DeviceError};

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
    inode_id: usize,
}

impl UartDev {
    pub fn new(index: usize, port: Arc<dyn UartScheme>) -> Self {
        Self {
            index,
            port,
            pending: Mutex::new(None),
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

    #[allow(unsafe_code)]
    fn io_control(&self, _cmd: u32, _data: usize) -> Result<usize> {
        warn!("uart ioctl unimplemented");
        Err(FsError::NotSupported)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
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
