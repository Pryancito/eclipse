use alloc::{boxed::Box, collections::VecDeque, string::String, sync::Arc};

use crate::sync::Mutex;

use crate::scheme::{impl_event_scheme, Scheme, UartScheme};
use crate::utils::EventListener;
use crate::DeviceResult;

const BUF_CAPACITY: usize = 4096;

/// The drain loop below used to run until `try_recv` said there was nothing
/// left, which is fine for a FIFO that empties and is a hang for one that does
/// not: a wedged UART that keeps reporting "data ready" — a stuck line, a
/// device whose status register never clears — never lets the handler return,
/// and the `Vec` it fills grows until memory runs out. On the two
/// architectures being brought up this UART *is* the console, so the machine
/// dies with nothing to say.
///
/// The bound and the reasoning now live in [`crate::utils::bounded_drain`],
/// because the same loop was written without one in three drivers.
use crate::utils::{bounded_drain, DRAIN_BURST};

pub struct BufferedUart {
    inner: Arc<dyn UartScheme>,
    buf: Mutex<VecDeque<u8>>,
    listener: EventListener,
    name: String,
}

impl_event_scheme!(BufferedUart);

impl BufferedUart {
    pub fn new(uart: Arc<dyn UartScheme>) -> Arc<Self> {
        let ret = Arc::new(Self {
            inner: uart.clone(),
            name: alloc::format!("{}-buffered", uart.name()),
            buf: Mutex::new(VecDeque::with_capacity(BUF_CAPACITY)),
            listener: EventListener::new(),
        });
        let cloned = ret.clone();
        uart.subscribe(Box::new(move |_| cloned.handle_irq(0)), false);
        ret
    }
}

impl Scheme for BufferedUart {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn handle_irq(&self, _unused: usize) {
        // Drain the hardware FIFO first (lock-free on our side; the inner
        // UART driver acquires its own lock per byte), for at most one burst.
        let mut drained = alloc::vec::Vec::with_capacity(DRAIN_BURST.min(16));
        bounded_drain(DRAIN_BURST, || {
            match self.inner.try_recv().unwrap_or(None) {
                Some(c) => {
                    drained.push(if c == b'\r' { b'\n' } else { c });
                    true
                }
                None => false,
            }
        });
        if drained.is_empty() {
            return;
        }
        // Batch-insert into the ring buffer under a single lock acquisition
        // to minimise push_off/pop_off churn.
        let dropped = {
            let mut buf = self.buf.lock();
            let room = BUF_CAPACITY.saturating_sub(buf.len());
            let taken = room.min(drained.len());
            for &c in &drained[..taken] {
                buf.push_back(c);
            }
            drained.len() - taken
        };
        if dropped > 0 {
            // Silently losing console input is how a boot looks hung when it
            // is merely deaf. One line per burst, not one per byte: this runs
            // in interrupt context.
            warn!(
                "{}: input buffer full, dropped {} bytes",
                self.name, dropped
            );
        }
        self.listener.trigger(());
    }
}

impl UartScheme for BufferedUart {
    fn try_recv(&self) -> DeviceResult<Option<u8>> {
        Ok(self.buf.lock().pop_front())
    }
    fn send(&self, ch: u8) -> DeviceResult {
        self.inner.send(ch)
    }
    fn write_str(&self, s: &str) -> DeviceResult {
        self.inner.write_str(s)
    }
}

/// `BufferedUart` sits between the hardware FIFO and the console, and had no
/// tests.
///
/// The one that mattered: the drain loop ran until the device said it had
/// nothing left, which a wedged UART never says. On the architectures being
/// brought up this is the console, so the failure mode was a machine that
/// hangs in an interrupt handler with no output at all.
///
/// Two notes for whoever mutates this next. Removing the bound makes
/// `a_stuck_uart_does_not_hang_the_handler` **hang** rather than fail, which
/// is the symptom itself; a test run that stops making progress here is the
/// answer, not a broken runner. And changing `DRAIN_BURST` to another value
/// leaves everything green on purpose: the tests are written against the
/// constant because the exact size is a tunable, and only the existence of a
/// bound is a contract.
#[cfg(test)]
mod buffered_uart_tests {
    use super::*;
    use crate::scheme::EventScheme;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// A UART whose receive side we drive from the test.
    struct FakeUart {
        /// Bytes still in the "hardware FIFO".
        rx: Mutex<VecDeque<u8>>,
        /// Everything written out, in order.
        tx: Mutex<alloc::vec::Vec<u8>>,
        /// When set, `try_recv` never runs dry — a stuck line.
        endless: bool,
        /// How many times `try_recv` was called, so a bounded loop is visible.
        reads: AtomicUsize,
        listener: EventListener,
    }

    impl FakeUart {
        fn new(bytes: &[u8], endless: bool) -> Arc<Self> {
            Arc::new(Self {
                rx: Mutex::new(bytes.iter().copied().collect()),
                tx: Mutex::new(alloc::vec::Vec::new()),
                endless,
                reads: AtomicUsize::new(0),
                listener: EventListener::new(),
            })
        }
    }

    impl_event_scheme!(FakeUart);

    impl Scheme for FakeUart {
        fn name(&self) -> &str {
            "fake-uart"
        }
    }

    impl UartScheme for FakeUart {
        fn try_recv(&self) -> DeviceResult<Option<u8>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.endless {
                return Ok(Some(b'x'));
            }
            Ok(self.rx.lock().pop_front())
        }
        fn send(&self, ch: u8) -> DeviceResult {
            self.tx.lock().push(ch);
            Ok(())
        }
    }

    /// Everything currently readable from `uart`, in order.
    fn read_all(uart: &BufferedUart) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::new();
        while let Some(c) = uart.try_recv().unwrap() {
            out.push(c);
        }
        out
    }

    #[test]
    fn a_stuck_uart_does_not_hang_the_handler() {
        // This is the bug: with an unbounded loop this call never returns and
        // the `Vec` behind it grows until the machine is out of memory. If
        // this test ever hangs instead of failing, the bound is gone again.
        let fake = FakeUart::new(&[], true);
        let uart = BufferedUart::new(fake.clone());
        uart.handle_irq(0);
        // Exactly one burst came out of the device, no more.
        assert_eq!(fake.reads.load(Ordering::SeqCst), DRAIN_BURST);
        assert_eq!(read_all(&uart).len(), DRAIN_BURST);
    }

    #[test]
    fn a_fifo_that_empties_is_drained_and_not_over_read() {
        let fake = FakeUart::new(b"hola", false);
        let uart = BufferedUart::new(fake.clone());
        uart.handle_irq(0);
        assert_eq!(read_all(&uart), b"hola".to_vec());
        // Four bytes plus the one read that returns `None` and stops the loop.
        assert_eq!(fake.reads.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn what_a_burst_leaves_behind_comes_back_on_the_next_interrupt() {
        // The bound is not allowed to lose the rest of the FIFO.
        let long: alloc::vec::Vec<u8> = (0..DRAIN_BURST + 10).map(|i| (i % 251) as u8).collect();
        let fake = FakeUart::new(&long, false);
        let uart = BufferedUart::new(fake);
        uart.handle_irq(0);
        uart.handle_irq(0);
        let got = read_all(&uart);
        // `\r` is folded to `\n` on the way in, so compare against the same
        // transformation rather than the raw bytes.
        let want: alloc::vec::Vec<u8> = long
            .iter()
            .map(|&c| if c == b'\r' { b'\n' } else { c })
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn carriage_return_becomes_newline() {
        // The console line discipline never sees a `\r`, so pressing Enter on
        // a serial terminal has to arrive as `\n` or nothing submits a line.
        let fake = FakeUart::new(b"ls\rpwd\r\n", false);
        let uart = BufferedUart::new(fake);
        uart.handle_irq(0);
        assert_eq!(read_all(&uart), b"ls\npwd\n\n".to_vec());
    }

    #[test]
    fn the_buffer_stops_at_its_capacity_instead_of_growing() {
        let fake = FakeUart::new(&[], true);
        let uart = BufferedUart::new(fake);
        // More bursts than the buffer can hold.
        for _ in 0..(BUF_CAPACITY / DRAIN_BURST) + 4 {
            uart.handle_irq(0);
        }
        assert_eq!(uart.buf.lock().len(), BUF_CAPACITY);
        assert_eq!(read_all(&uart).len(), BUF_CAPACITY);
    }

    #[test]
    fn a_full_buffer_keeps_the_oldest_bytes() {
        // Dropping the tail rather than the head: a command already typed
        // must not be rewritten by the overflow that follows it.
        let fake = FakeUart::new(&[], true);
        let uart = BufferedUart::new(fake);
        uart.buf
            .lock()
            .extend(core::iter::repeat_n(b'a', BUF_CAPACITY - 2));
        uart.handle_irq(0);
        let got = read_all(&uart);
        assert_eq!(got.len(), BUF_CAPACITY);
        assert_eq!(got[BUF_CAPACITY - 3], b'a');
        assert_eq!(&got[BUF_CAPACITY - 2..], b"xx");
    }

    #[test]
    fn an_empty_fifo_notifies_nobody() {
        // A spurious interrupt must not wake every reader: each wake-up walks
        // the whole listener list, and on a shared line this fires constantly.
        let woken = Arc::new(AtomicUsize::new(0));
        let fake = FakeUart::new(&[], false);
        let uart = BufferedUart::new(fake.clone());
        let seen = woken.clone();
        uart.subscribe(
            Box::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
            false,
        );
        uart.handle_irq(0);
        assert_eq!(woken.load(Ordering::SeqCst), 0);
        // And a byte does wake them.
        fake.rx.lock().push_back(b'k');
        uart.handle_irq(0);
        assert_eq!(woken.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn writes_go_straight_through_to_the_device() {
        // Only the receive side is buffered; output must not be delayed, or
        // a panic message never reaches the wire.
        let fake = FakeUart::new(&[], false);
        let uart = BufferedUart::new(fake.clone());
        uart.send(b'!').unwrap();
        uart.write_str("ok").unwrap();
        assert_eq!(*fake.tx.lock(), b"!ok".to_vec());
    }

    #[test]
    fn the_name_says_what_it_wraps() {
        let uart = BufferedUart::new(FakeUart::new(&[], false));
        assert_eq!(uart.name(), "fake-uart-buffered");
    }
}
