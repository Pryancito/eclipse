use core::convert::TryInto;
use core::ops::{BitAnd, BitOr, Not};

use crate::sync::Mutex;
use bitflags::bitflags;

use crate::io::{Io, Mmio, ReadOnly};
use crate::scheme::{impl_event_scheme, Scheme, UartScheme};
use crate::utils::EventListener;
use crate::DeviceResult;

bitflags! {
    /// Interrupt enable flags
    struct IntEnFlags: u8 {
        const RECEIVED = 1;
        const SENT = 1 << 1;
        const ERRORED = 1 << 2;
        const STATUS_CHANGE = 1 << 3;
        // 4 to 7 are unused
    }
}

bitflags! {
    /// Line status flags
    struct LineStsFlags: u8 {
        const INPUT_FULL = 1;
        // 1 to 4 unknown
        const OUTPUT_EMPTY = 1 << 5;
        // 6 and 7 unknown
    }
}

#[repr(C)]
struct Uart16550Inner<T: Io> {
    /// Data register, read to receive, write to send
    data: T,
    /// Interrupt enable
    int_en: T,
    /// FIFO control
    fifo_ctrl: T,
    /// Line control
    line_ctrl: T,
    /// Modem control
    modem_ctrl: T,
    /// Line status
    line_sts: ReadOnly<T>,
    /// Modem status
    modem_sts: ReadOnly<T>,
}

impl<T: Io> Uart16550Inner<T>
where
    T::Value: From<u8> + TryInto<u8>,
{
    fn init(&mut self) {
        // Disable interrupts
        self.int_en.write(0x00.into());

        // Enable FIFO, clear TX/RX queues and
        // set interrupt watermark at 14 bytes
        self.fifo_ctrl.write(0xC7.into());

        // Mark data terminal ready, signal request to send
        // and enable auxilliary output #2 (used as interrupt line for CPU)
        self.modem_ctrl.write(0x0B.into());

        // Enable interrupts
        self.int_en.write(0x01.into());
    }

    fn line_sts(&self) -> LineStsFlags {
        LineStsFlags::from_bits_truncate(
            (self.line_sts.read() & 0xFF.into()).try_into().unwrap_or(0),
        )
    }

    fn try_recv(&mut self) -> DeviceResult<Option<u8>> {
        if self.line_sts().contains(LineStsFlags::INPUT_FULL) {
            Ok(Some(
                (self.data.read() & 0xFF.into()).try_into().unwrap_or(0),
            ))
        } else {
            Ok(None)
        }
    }

    fn send(&mut self, ch: u8) -> DeviceResult {
        // Bounded wait: this runs under the uart's IRQ-masking lock, so an
        // unbounded `while !OUTPUT_EMPTY {}` on a wedged/absent UART (no serial
        // cable is the NORM on the bring-up box) would spin forever with IRQs
        // off and freeze the CPU invisibly. A healthy 16550 drains a byte in
        // microseconds; after ~1M polls the port is dead — drop the byte and
        // move on. Losing serial bytes beats hanging the machine.
        for _ in 0..1_000_000u32 {
            if self.line_sts().contains(LineStsFlags::OUTPUT_EMPTY) {
                self.data.write(ch.into());
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn write_str(&mut self, s: &str) -> DeviceResult {
        for b in s.bytes() {
            match b {
                b'\n' => {
                    self.send(b'\r')?;
                    self.send(b'\n')?;
                }
                _ => {
                    self.send(b)?;
                }
            }
        }
        Ok(())
    }
}

/// MMIO driver for UART 16550
pub struct Uart16550Mmio<V: 'static>
where
    V: Copy + BitAnd<Output = V> + BitOr<Output = V> + Not<Output = V>,
{
    inner: Mutex<&'static mut Uart16550Inner<Mmio<V>>>,
    listener: EventListener,
}

impl_event_scheme!(Uart16550Mmio<V>
where
    V: Copy
        + BitAnd<Output = V>
        + BitOr<Output = V>
        + Not<Output = V>
        + From<u8>
        + TryInto<u8>
        + Send
);

impl<V> Scheme for Uart16550Mmio<V>
where
    V: Copy + BitAnd<Output = V> + BitOr<Output = V> + Not<Output = V> + Send,
{
    fn name(&self) -> &str {
        "uart16550-mmio"
    }

    fn handle_irq(&self, _irq_num: usize) {
        self.listener.trigger(());
    }
}

impl<V> UartScheme for Uart16550Mmio<V>
where
    V: Copy
        + BitAnd<Output = V>
        + BitOr<Output = V>
        + Not<Output = V>
        + From<u8>
        + TryInto<u8>
        + Send,
{
    fn try_recv(&self) -> DeviceResult<Option<u8>> {
        self.inner.lock().try_recv()
    }

    fn send(&self, ch: u8) -> DeviceResult {
        self.inner.lock().send(ch)
    }

    fn write_str(&self, s: &str) -> DeviceResult {
        self.inner.lock().write_str(s)
    }
}

impl<V> Uart16550Mmio<V>
where
    V: Copy
        + BitAnd<Output = V>
        + BitOr<Output = V>
        + Not<Output = V>
        + From<u8>
        + TryInto<u8>
        + Send,
{
    unsafe fn new_common(base: usize) -> Self {
        unsafe {
            let uart: &mut Uart16550Inner<Mmio<V>> = Mmio::<V>::from_base_as(base);
            uart.init();
            Self {
                inner: Mutex::new(uart),
                listener: EventListener::new(),
            }
        }
    }
}

impl Uart16550Mmio<u8> {
    /// # Safety
    ///
    /// This function is unsafe because `base_addr` may be an arbitrary address.
    pub unsafe fn new(base: usize) -> Self {
        unsafe { Self::new_common(base) }
    }
}

impl Uart16550Mmio<u32> {
    /// # Safety
    ///
    /// This function is unsafe because `base_addr` may be an arbitrary address.
    pub unsafe fn new(base: usize) -> Self {
        unsafe { Self::new_common(base) }
    }
}

#[cfg(target_arch = "x86_64")]
mod pmio {
    use super::*;
    use crate::io::Pmio;

    /// Pmio driver for UART 16550
    pub struct Uart16550Pmio {
        inner: Mutex<Uart16550Inner<Pmio<u8>>>,
        listener: EventListener,
    }

    impl_event_scheme!(Uart16550Pmio);

    impl Scheme for Uart16550Pmio {
        fn name(&self) -> &str {
            "uart16550-Pmio"
        }

        fn handle_irq(&self, _irq_num: usize) {
            self.listener.trigger(());
        }
    }

    impl UartScheme for Uart16550Pmio {
        fn try_recv(&self) -> DeviceResult<Option<u8>> {
            self.inner.lock().try_recv()
        }

        fn send(&self, ch: u8) -> DeviceResult {
            self.inner.lock().send(ch)
        }

        fn write_str(&self, s: &str) -> DeviceResult {
            self.inner.lock().write_str(s)
        }
    }

    impl Uart16550Pmio {
        /// Construct a `Uart16550Pmio` whose address starts at `base`.
        pub fn new(base: u16) -> Self {
            let mut uart = Uart16550Inner::<Pmio<u8>> {
                data: Pmio::new(base),
                int_en: Pmio::new(base + 1),
                fifo_ctrl: Pmio::new(base + 2),
                line_ctrl: Pmio::new(base + 3),
                modem_ctrl: Pmio::new(base + 4),
                line_sts: ReadOnly::new(Pmio::new(base + 5)),
                modem_sts: ReadOnly::new(Pmio::new(base + 6)),
            };
            uart.init();
            Self {
                inner: Mutex::new(uart),
                listener: EventListener::new(),
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub use pmio::Uart16550Pmio;

/// The 16550 register map and the three operations over it.
///
/// This is the serial console: every panic banner, every `[null-exec]` and
/// `[watchpoint]` line, every crash log Moebius pastes. It had no tests, and a
/// driver that overlays a `#[repr(C)]` struct on device memory has one failure
/// mode above all others -- a field at the wrong offset writes to the wrong
/// register, silently and for ever.
///
/// The port here is a real `Io` implementation over ordinary memory, so `init`,
/// `send`, `try_recv` and `write_str` run unchanged.
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec::Vec};
    use core::cell::{Cell, RefCell};

    /// Which of the seven registers a [`Port`] is.
    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    enum Reg {
        Data,
        IntEn,
        FifoCtrl,
        LineCtrl,
        ModemCtrl,
        LineSts,
        ModemSts,
    }

    /// The other end of the wire: what the driver wrote, and what the port
    /// answers when it reads.
    struct Wire {
        /// Every byte written to the data register, in order -- which is what a
        /// terminal on the other end would see.
        sent: RefCell<Vec<u8>>,
        /// Every write to a control register, as `(register, value)`.
        control: RefCell<Vec<(Reg, u8)>>,
        /// What the line-status register reads.
        line_sts: Cell<u8>,
        /// The byte the data register reads, once.
        pending: Cell<Option<u8>>,
    }

    impl Wire {
        fn new(line_sts: u8) -> &'static Self {
            Box::leak(Box::new(Self {
                sent: RefCell::new(Vec::new()),
                control: RefCell::new(Vec::new()),
                line_sts: Cell::new(line_sts),
                pending: Cell::new(None),
            }))
        }
    }

    struct Port {
        wire: &'static Wire,
        reg: Reg,
    }

    impl Io for Port {
        type Value = u8;

        fn read(&self) -> u8 {
            match self.reg {
                Reg::LineSts => self.wire.line_sts.get(),
                Reg::Data => self.wire.pending.take().unwrap_or(0),
                _ => 0,
            }
        }

        fn write(&mut self, value: u8) {
            match self.reg {
                Reg::Data => self.wire.sent.borrow_mut().push(value),
                reg => self.wire.control.borrow_mut().push((reg, value)),
            }
        }
    }

    fn port(wire: &'static Wire, reg: Reg) -> Port {
        Port { wire, reg }
    }

    fn uart(wire: &'static Wire) -> Uart16550Inner<Port> {
        Uart16550Inner {
            data: port(wire, Reg::Data),
            int_en: port(wire, Reg::IntEn),
            fifo_ctrl: port(wire, Reg::FifoCtrl),
            line_ctrl: port(wire, Reg::LineCtrl),
            modem_ctrl: port(wire, Reg::ModemCtrl),
            line_sts: ReadOnly::new(port(wire, Reg::LineSts)),
            modem_sts: ReadOnly::new(port(wire, Reg::ModemSts)),
        }
    }

    /// A line status with room in the transmit holding register.
    const READY: u8 = 1 << 5;
    /// ...and with a byte waiting to be read.
    const HAS_INPUT: u8 = 1;

    // --- the register map -------------------------------------------------

    /// The one that matters most, and the reason this file needed tests at all:
    /// the struct is overlaid on device memory, so each field's offset IS the
    /// register's address. A field added in the middle, a `repr(transparent)`
    /// dropped from `Mmio` or `ReadOnly`, or a reordering, and the driver
    /// silently drives the wrong registers -- on a port whose only job is to
    /// carry the evidence when everything else has already gone wrong.
    #[test]
    fn every_register_sits_at_its_own_offset_in_the_16550_map() {
        // The 16550's registers are consecutive units from the base, and the
        // unit is the bus width: one byte for an 8-bit port, four for a 32-bit
        // one (the same struct serves both).
        macro_rules! check {
            ($t:ty, $stride:expr) => {{
                type Inner = Uart16550Inner<Mmio<$t>>;
                assert_eq!(
                    core::mem::size_of::<Inner>(),
                    7 * $stride,
                    "the map must be exactly the seven registers, with no padding"
                );
                assert_eq!(core::mem::align_of::<Inner>(), core::mem::align_of::<$t>());
                let map = core::mem::MaybeUninit::<Inner>::uninit();
                let base = map.as_ptr() as usize;
                for (i, (name, at)) in [
                    ("data", unsafe { core::ptr::addr_of!((*map.as_ptr()).data) }
                        as usize),
                    (
                        "int_en",
                        unsafe { core::ptr::addr_of!((*map.as_ptr()).int_en) } as usize,
                    ),
                    ("fifo_ctrl", unsafe {
                        core::ptr::addr_of!((*map.as_ptr()).fifo_ctrl) as usize
                    }),
                    ("line_ctrl", unsafe {
                        core::ptr::addr_of!((*map.as_ptr()).line_ctrl) as usize
                    }),
                    ("modem_ctrl", unsafe {
                        core::ptr::addr_of!((*map.as_ptr()).modem_ctrl) as usize
                    }),
                    ("line_sts", unsafe {
                        core::ptr::addr_of!((*map.as_ptr()).line_sts) as usize
                    }),
                    ("modem_sts", unsafe {
                        core::ptr::addr_of!((*map.as_ptr()).modem_sts) as usize
                    }),
                ]
                .iter()
                .enumerate()
                {
                    assert_eq!(
                        at - base,
                        i * $stride,
                        "register {} is at the wrong offset for a {}-byte bus",
                        name,
                        $stride
                    );
                }
            }};
        }
        check!(u8, 1);
        check!(u32, 4);
    }

    // --- init -------------------------------------------------------------

    /// What a port must be told before it can carry a byte, in order: quiet
    /// first (interrupts off), then the FIFO, then the modem lines, and only
    /// then interrupts on. Enabling the receive interrupt before the FIFO is
    /// cleared would hand the handler the firmware's leftovers.
    #[test]
    fn init_quietens_the_port_before_it_arms_it() {
        let wire = Wire::new(READY);
        uart(&wire).init();
        assert_eq!(
            wire.control.borrow().as_slice(),
            &[
                (Reg::IntEn, 0x00),
                (Reg::FifoCtrl, 0xC7),
                (Reg::ModemCtrl, 0x0B),
                (Reg::IntEn, 0x01),
            ],
        );
        assert!(
            wire.sent.borrow().is_empty(),
            "init must not put a byte on the wire"
        );
    }

    /// `init` never writes the line-control register, so the word format and
    /// the baud divisor stay as the firmware left them. That is a real gap --
    /// a real 16550 init sets DLAB, writes the divisor and then 8N1 -- and it
    /// is recorded rather than changed, because whether a board's firmware has
    /// already done it can only be answered on the hardware.
    #[test]
    fn init_inherits_the_baud_rate_and_the_word_format_from_the_firmware() {
        let wire = Wire::new(READY);
        uart(&wire).init();
        assert!(
            !wire
                .control
                .borrow()
                .iter()
                .any(|&(reg, _)| reg == Reg::LineCtrl),
            "line control is written now: if that is on purpose, this test says so"
        );
    }

    // --- send and write_str -----------------------------------------------

    #[test]
    fn a_byte_goes_out_when_the_holding_register_is_empty() {
        let wire = Wire::new(READY);
        uart(&wire).send(b'A').unwrap();
        assert_eq!(wire.sent.borrow().as_slice(), b"A");
    }

    /// A port with no cable is the NORM on the bring-up box, so a full holding
    /// register that never drains must cost a dropped byte and not the machine:
    /// this runs under the uart's IRQ-masking lock, and an unbounded wait there
    /// freezes the CPU with nothing on screen to say why.
    #[test]
    fn a_wedged_port_drops_the_byte_instead_of_hanging_the_cpu() {
        let wire = Wire::new(0); // never OUTPUT_EMPTY
        uart(&wire).send(b'A').unwrap();
        assert!(
            wire.sent.borrow().is_empty(),
            "the byte cannot reach a port that never drained"
        );
    }

    /// And the loss is silent by design, which is what the caller has to be
    /// able to rely on: `write_str` keeps going and the machine stays up.
    #[test]
    fn a_wedged_port_does_not_stop_the_rest_of_the_line() {
        let wire = Wire::new(0);
        uart(&wire).write_str("panic!").unwrap();
        assert!(wire.sent.borrow().is_empty());
    }

    /// A terminal needs the carriage return; a bare `\n` leaves the next line
    /// starting where the last one ended, which is how a stairstepped panic
    /// banner happens.
    #[test]
    fn a_newline_goes_out_as_carriage_return_and_newline() {
        let wire = Wire::new(READY);
        uart(&wire).write_str("ab\ncd\n").unwrap();
        assert_eq!(wire.sent.borrow().as_slice(), b"ab\r\ncd\r\n");
    }

    /// A caller that writes its own `\r\n` -- and several do -- gets `\r\r\n`
    /// on the wire, because the translation looks only at the `\n`. A terminal
    /// ignores the repeat, so this records the behaviour rather than calling it
    /// a bug: the fix (skip the `\r` when one precedes) risks the opposite
    /// fault, a line that never gets its carriage return.
    #[test]
    fn a_caller_written_carriage_return_is_not_swallowed_and_not_merged() {
        let wire = Wire::new(READY);
        uart(&wire).write_str("ab\r\n").unwrap();
        assert_eq!(wire.sent.borrow().as_slice(), b"ab\r\r\n");
    }

    /// Bytes outside ASCII are passed through unchanged: these lines carry
    /// UTF-8, and a port that mangled the high bit would garble every em dash
    /// in the log.
    #[test]
    fn a_multibyte_character_goes_out_byte_for_byte() {
        let wire = Wire::new(READY);
        uart(&wire).write_str("a\u{2014}b").unwrap();
        assert_eq!(
            wire.sent.borrow().as_slice(),
            "a\u{2014}b".as_bytes(),
            "the port must not touch the bytes it is given"
        );
    }

    // --- try_recv ---------------------------------------------------------

    #[test]
    fn nothing_is_received_while_the_receive_register_is_empty() {
        let wire = Wire::new(READY);
        assert_eq!(uart(&wire).try_recv().unwrap(), None);
    }

    #[test]
    fn a_waiting_byte_is_received_once() {
        let wire = Wire::new(READY | HAS_INPUT);
        wire.pending.set(Some(b'q'));
        let mut uart = uart(&wire);
        assert_eq!(uart.try_recv().unwrap(), Some(b'q'));
        // The status bit is what says whether there is more, so the driver
        // asks it again rather than assuming.
        wire.line_sts.set(READY);
        assert_eq!(uart.try_recv().unwrap(), None);
    }

    /// The driver must not touch the data register while the status says there
    /// is nothing there: on a real 16550 that read is not free of consequence.
    #[test]
    fn the_receive_register_is_not_read_until_the_status_says_to() {
        let wire = Wire::new(READY);
        wire.pending.set(Some(b'x'));
        assert_eq!(uart(&wire).try_recv().unwrap(), None);
        assert_eq!(
            wire.pending.get(),
            Some(b'x'),
            "the data register was read with INPUT_FULL clear"
        );
    }
}
