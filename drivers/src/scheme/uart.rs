use super::{event::EventScheme, Scheme};
use crate::DeviceResult;

pub trait UartScheme: Scheme + EventScheme<Event = ()> {
    fn try_recv(&self) -> DeviceResult<Option<u8>>;
    fn send(&self, ch: u8) -> DeviceResult;
    fn write_str(&self, s: &str) -> DeviceResult {
        for c in s.bytes() {
            self.send(c)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Mutex;
    use crate::utils::EventListener;
    use crate::scheme::impl_event_scheme;
    use crate::DeviceError;
    use alloc::vec::Vec;

    /// A UART that records every byte the default `write_str` hands it, and
    /// can be told to fail on the n-th one.
    struct RecordingUart {
        sent: Mutex<Vec<u8>>,
        /// Fail the send whose index (0-based) is this one. `None` = never.
        fail_at: Option<usize>,
        listener: EventListener,
    }

    impl RecordingUart {
        fn new(fail_at: Option<usize>) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail_at,
                listener: EventListener::new(),
            }
        }
    }

    impl Scheme for RecordingUart {
        fn name(&self) -> &str {
            "recording-uart"
        }
    }

    impl_event_scheme!(RecordingUart);

    impl UartScheme for RecordingUart {
        fn try_recv(&self) -> DeviceResult<Option<u8>> {
            Ok(None)
        }
        fn send(&self, ch: u8) -> DeviceResult {
            let mut sent = self.sent.lock();
            if self.fail_at == Some(sent.len()) {
                return Err(DeviceError::NotReady);
            }
            sent.push(ch);
            Ok(())
        }
    }

    /// The default `write_str` is the only thing between `print!` and the
    /// serial port on every board whose driver does not override it: it must
    /// put the bytes out in order, one per `send`, and nothing else.
    #[test]
    fn writing_a_string_sends_each_byte_in_order() {
        let uart = RecordingUart::new(None);
        uart.write_str("ok\n").unwrap();
        assert_eq!(&*uart.sent.lock(), b"ok\n");
    }

    /// It is a byte loop, not a char loop: a multi-byte character must leave
    /// as its UTF-8 bytes rather than be truncated to one.
    #[test]
    fn a_multibyte_character_goes_out_as_its_utf8_bytes() {
        let uart = RecordingUart::new(None);
        uart.write_str("é").unwrap();
        assert_eq!(&*uart.sent.lock(), "é".as_bytes());
        assert_eq!(uart.sent.lock().len(), 2);
    }

    /// An empty string touches the port not at all; a device that sent a
    /// stray byte for it would corrupt the console on every empty `print!`.
    #[test]
    fn writing_an_empty_string_sends_nothing() {
        let uart = RecordingUart::new(None);
        uart.write_str("").unwrap();
        assert!(uart.sent.lock().is_empty());
    }

    /// The `?` is load-bearing: a port that stops accepting bytes half way
    /// has to report the failure, and must not keep pushing the rest of the
    /// line at a device that just said it was not ready.
    #[test]
    fn a_failing_send_stops_the_string_and_is_reported() {
        let uart = RecordingUart::new(Some(2));
        assert_eq!(uart.write_str("abcdef"), Err(DeviceError::NotReady));
        assert_eq!(&*uart.sent.lock(), b"ab");
    }
}
