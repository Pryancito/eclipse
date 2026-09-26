use crate::sync::Mutex;
use bitflags::bitflags;
use core::convert::TryInto;
use core::ops::{BitAnd, BitOr, Not};

use crate::io::{Io, Mmio, ReadOnly};
use crate::scheme::{impl_event_scheme, Scheme, UartScheme};
use crate::utils::EventListener;
use crate::DeviceResult;

bitflags! {
    /// TXDATA fields
    struct TXDATAFlags: u32 {
        const TXFULL = 1 << 31;
    }
}

bitflags! {
    /// RXDATA fields
    struct RXDATAFlags: u32 {
        const RXEMPTY = 1 << 31;
    }
}

bitflags! {
    /// TXCTRL fields
    struct TXCTRLFlags: u32 {
        const TXEN = 1;
        const NSTOP = 1 << 1;
    }
}

bitflags! {
    /// RXCTRL fields
    struct RXCTRLFlags: u32 {
        const RXEN = 1;
    }
}

bitflags! {
    /// IE fields
    struct IEFlags: u32 {
        const TXWM = 1;
        const RXWM = 1 << 1;
    }
}

#[repr(C)]
struct UartU740Inner<T: Io> {
    /// Transmit data register
    tx_data: T,
    /// Receive data register
    rx_data: ReadOnly<T>,
    /// Transmit control register
    tx_ctrl: T,
    /// Receive control register
    rx_ctrl: T,
    /// UART interrupt enable
    ie: T,
    /// UART interrupt pending
    ip: ReadOnly<T>,
    /// Baud rate divisor
    div: T,
}

impl<T: Io> UartU740Inner<T>
where
    T::Value: From<u8> + TryInto<u8> + From<u32> + TryInto<u32>,
{
    fn init(&mut self) {
        // Enable transmit. The stop-bit and watermark fields of `txctrl` are
        // deliberately left as the firmware set them -- the same policy the
        // 16550 follows for its line control -- so `NSTOP` is defined above and
        // unused on purpose. (The comment here used to claim this set both.)
        self.tx_ctrl
            .write((self.tx_ctrl.read().try_into().unwrap_or(0) | TXCTRLFlags::TXEN.bits()).into());

        // Enable receive and set interrupt watermark
        self.rx_ctrl
            .write((self.rx_ctrl.read().try_into().unwrap_or(0) | RXCTRLFlags::RXEN.bits()).into());

        // Enable TX & RX interrupt
        self.ie.write(
            (self.ie.read().try_into().unwrap_or(0) | IEFlags::TXWM.bits() | IEFlags::RXWM.bits())
                .into(),
        );
    }

    fn try_recv(&mut self) -> DeviceResult<Option<u8>> {
        let word: u32 = self.rx_data.read().try_into().unwrap_or(0);
        if RXDATAFlags::from_bits_truncate(word).contains(RXDATAFlags::RXEMPTY) {
            Ok(None)
        } else {
            // Mask FIRST, then narrow. This used to read
            // `ch.try_into().unwrap_or(0) & 0xFF`, where the `try_into` targets
            // `u8` and so FAILS for any word above 255: a single bit set above
            // the data byte turned the character into a NUL rather than being
            // masked off, which is the failure mode you cannot see in a log
            // because the log is what breaks. SiFive's `rxdata` defines only bit
            // 31 and bits 7:0 today, and that is the only reason it never bit.
            Ok(Some((word & 0xFF) as u8))
        }
    }

    fn send(&mut self, ch: u8) -> DeviceResult {
        // Bounded wait, the same bound and the same reason as the 16550's `send`
        // and the PL011's `putchar`: this runs under the uart's IRQ-masking lock
        // on the path every panic banner takes, so an unbounded `while TXFULL {}`
        // on a port that is not clocked freezes that CPU for good with nothing on
        // screen to say why. Losing serial bytes beats hanging the machine.
        for _ in 0..1_000_000u32 {
            let status: u32 = self.tx_data.read().try_into().unwrap_or(0);
            if !TXDATAFlags::from_bits_truncate(status).contains(TXDATAFlags::TXFULL) {
                self.tx_data.write(ch.into());
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

/// MMIO driver for the SiFive FU740 UART (this said "UART 16550", which is a
/// different peripheral in a different file).
pub struct UartU740Mmio<V: 'static>
where
    V: Copy + BitAnd<Output = V> + BitOr<Output = V> + Not<Output = V>,
{
    inner: Mutex<&'static mut UartU740Inner<Mmio<V>>>,
    listener: EventListener,
}

impl_event_scheme!(UartU740Mmio<V>
where
    V: Copy
        + BitAnd<Output = V>
        + BitOr<Output = V>
        + Not<Output = V>
        + From<u8>
        + TryInto<u8>
        + Send
);

impl<V> Scheme for UartU740Mmio<V>
where
    V: Copy + BitAnd<Output = V> + BitOr<Output = V> + Not<Output = V> + Send,
{
    fn name(&self) -> &str {
        "uart-u740-mmio"
    }

    fn handle_irq(&self, _irq_num: usize) {
        self.listener.trigger(());
    }
}

impl<V> UartScheme for UartU740Mmio<V>
where
    V: Copy
        + BitAnd<Output = V>
        + BitOr<Output = V>
        + Not<Output = V>
        + From<u8>
        + TryInto<u8>
        + From<u32>
        + TryInto<u32>
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

impl<V> UartU740Mmio<V>
where
    V: Copy
        + BitAnd<Output = V>
        + BitOr<Output = V>
        + Not<Output = V>
        + From<u8>
        + TryInto<u8>
        + From<u32>
        + TryInto<u32>
        + Send,
{
    unsafe fn new_common(base: usize) -> Self {
        let uart: &mut UartU740Inner<Mmio<V>> = unsafe { Mmio::<V>::from_base_as(base) };
        uart.init();
        Self {
            inner: Mutex::new(uart),
            listener: EventListener::new(),
        }
    }
}

impl UartU740Mmio<u32> {
    /// # Safety
    ///
    /// This function is unsafe because `base_addr` may be an arbitrary address.
    pub unsafe fn new(base: usize) -> Self {
        unsafe { Self::new_common(base) }
    }
}

/// The SiFive FU740 UART, driven over ordinary memory.
///
/// This is the riscv64 board console, and it had no tests -- it could not have
/// had any, because the module was gated on `feature = "fu740"` and no
/// `cargo test` line in the tree turns that feature on. Nothing compiled it,
/// `deny(warnings)` included.
///
/// Same recipe as the 16550 next door: the driver is generic over [`Io`], so a
/// real `Io` over ordinary memory is a device as far as it can tell, and every
/// write to every register is recorded in order.
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec::Vec};
    use core::cell::{Cell, RefCell};
    use core::mem::{size_of, MaybeUninit};
    use core::ptr::addr_of;

    /// Which of the seven registers a [`Port`] is.
    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    enum Reg {
        TxData,
        RxData,
        TxCtrl,
        RxCtrl,
        Ie,
        Ip,
        Div,
    }

    struct Wire {
        /// Every byte written to `txdata`, in order: what a terminal on the other
        /// end would see.
        sent: RefCell<Vec<u8>>,
        /// Every write to a control register, as `(register, value)`.
        control: RefCell<Vec<(Reg, u32)>>,
        /// What `txdata` reads -- the place the TXFULL flag lives.
        tx_status: Cell<u32>,
        /// What `rxdata` reads: the RXEMPTY flag and the character together.
        rx_word: Cell<u32>,
        /// What the control registers read back, i.e. what the firmware left.
        ctrl_readback: Cell<u32>,
        /// How many times `rxdata` was read.
        rx_reads: Cell<usize>,
    }

    impl Wire {
        fn new() -> &'static Self {
            Box::leak(Box::new(Self {
                sent: RefCell::new(Vec::new()),
                control: RefCell::new(Vec::new()),
                tx_status: Cell::new(0),
                rx_word: Cell::new(RXDATAFlags::RXEMPTY.bits()),
                ctrl_readback: Cell::new(0),
                rx_reads: Cell::new(0),
            }))
        }

        fn wrote(&self, reg: Reg) -> Option<u32> {
            self.control
                .borrow()
                .iter()
                .rev()
                .find(|(r, _)| *r == reg)
                .map(|(_, v)| *v)
        }
    }

    struct Port {
        wire: &'static Wire,
        reg: Reg,
    }

    impl Io for Port {
        type Value = u32;

        fn read(&self) -> u32 {
            match self.reg {
                Reg::TxData => self.wire.tx_status.get(),
                Reg::RxData => {
                    self.wire.rx_reads.set(self.wire.rx_reads.get() + 1);
                    self.wire.rx_word.get()
                }
                _ => self.wire.ctrl_readback.get(),
            }
        }

        fn write(&mut self, value: u32) {
            match self.reg {
                Reg::TxData => self.wire.sent.borrow_mut().push(value as u8),
                reg => self.wire.control.borrow_mut().push((reg, value)),
            }
        }
    }

    fn uart(wire: &'static Wire) -> UartU740Inner<Port> {
        UartU740Inner {
            tx_data: Port {
                wire,
                reg: Reg::TxData,
            },
            rx_data: ReadOnly::new(Port {
                wire,
                reg: Reg::RxData,
            }),
            tx_ctrl: Port {
                wire,
                reg: Reg::TxCtrl,
            },
            rx_ctrl: Port {
                wire,
                reg: Reg::RxCtrl,
            },
            ie: Port { wire, reg: Reg::Ie },
            ip: ReadOnly::new(Port { wire, reg: Reg::Ip }),
            div: Port {
                wire,
                reg: Reg::Div,
            },
        }
    }

    /// The struct is laid over device memory, so a field's OFFSET is the
    /// register's address. A field inserted in the middle, a reordering, or
    /// `repr(transparent)` falling off `Mmio` or `ReadOnly`, and the driver
    /// drives the wrong registers in silence -- on the only port that can tell
    /// you anything when the board will not boot. These are the FU740 manual's
    /// own offsets.
    #[test]
    fn every_register_sits_at_its_own_offset_in_the_fu740_map() {
        let block = MaybeUninit::<UartU740Inner<Mmio<u32>>>::uninit();
        let base = block.as_ptr();
        macro_rules! offset {
            ($field:ident) => {
                unsafe { addr_of!((*base).$field) as usize - base as usize }
            };
        }
        assert_eq!(size_of::<Mmio<u32>>(), 4, "Mmio<u32> must be transparent");
        assert_eq!(
            size_of::<ReadOnly<Mmio<u32>>>(),
            4,
            "ReadOnly must be transparent too, or every register after rxdata moves"
        );
        assert_eq!(offset!(tx_data), 0x00, "txdata");
        assert_eq!(offset!(rx_data), 0x04, "rxdata");
        assert_eq!(offset!(tx_ctrl), 0x08, "txctrl");
        assert_eq!(offset!(rx_ctrl), 0x0c, "rxctrl");
        assert_eq!(offset!(ie), 0x10, "ie");
        assert_eq!(offset!(ip), 0x14, "ip");
        assert_eq!(offset!(div), 0x18, "div");
        assert_eq!(
            size_of::<UartU740Inner<Mmio<u32>>>(),
            0x1c,
            "no padding may creep in after div"
        );
    }

    /// `init` turns both directions on and arms the interrupts, and leaves the
    /// bits it found alone: the watermark and stop-bit fields share these
    /// registers and belong to whoever set the port up.
    #[test]
    fn init_enables_both_directions_without_clearing_what_it_found() {
        let wire = Wire::new();
        // A firmware-configured txctrl/rxctrl: watermark 1, two stop bits.
        let firmware = (1 << 16) | TXCTRLFlags::NSTOP.bits();
        wire.ctrl_readback.set(firmware);

        uart(wire).init();

        let tx = wire.wrote(Reg::TxCtrl).expect("txctrl must be written");
        let rx = wire.wrote(Reg::RxCtrl).expect("rxctrl must be written");
        assert_eq!(tx & TXCTRLFlags::TXEN.bits(), TXCTRLFlags::TXEN.bits());
        assert_eq!(rx & RXCTRLFlags::RXEN.bits(), RXCTRLFlags::RXEN.bits());
        assert_eq!(tx & firmware, firmware, "init wiped part of txctrl");
        assert_eq!(rx & firmware, firmware, "init wiped part of rxctrl");
    }

    #[test]
    fn init_arms_both_interrupt_watermarks() {
        let wire = Wire::new();
        uart(wire).init();
        let ie = wire.wrote(Reg::Ie).expect("ie must be written");
        assert_eq!(ie & IEFlags::RXWM.bits(), IEFlags::RXWM.bits(), "RXWM");
        assert_eq!(ie & IEFlags::TXWM.bits(), IEFlags::TXWM.bits(), "TXWM");
    }

    /// `init` must not touch the baud divisor: the firmware set it, and guessing
    /// it wrong is a mute console. Same call as the 16550's.
    #[test]
    fn init_inherits_the_baud_rate_from_the_firmware() {
        let wire = Wire::new();
        uart(wire).init();
        assert_eq!(
            wire.wrote(Reg::Div),
            None,
            "the baud divisor must be left as the firmware set it"
        );
    }

    #[test]
    fn a_byte_goes_out_when_the_transmitter_has_room() {
        let wire = Wire::new();
        wire.tx_status.set(0); // not full
        uart(wire).send(b'X').unwrap();
        assert_eq!(wire.sent.borrow().as_slice(), b"X");
    }

    /// A port that is always full -- mapped but not clocked, which is what a
    /// board that ignores this UART looks like -- must cost a bounded spin and a
    /// lost byte, not the CPU. `send` runs with interrupts masked.
    #[test]
    fn a_wedged_port_drops_the_byte_instead_of_hanging_the_cpu() {
        let wire = Wire::new();
        wire.tx_status.set(TXDATAFlags::TXFULL.bits());
        uart(wire).send(b'X').unwrap();
        assert!(
            wire.sent.borrow().is_empty(),
            "nothing may be written to a port that never said it had room"
        );
    }

    /// And the rest of the line still goes out: one wedged byte must not abort
    /// the panic banner.
    #[test]
    fn a_wedged_port_does_not_stop_the_rest_of_the_line() {
        let wire = Wire::new();
        wire.tx_status.set(TXDATAFlags::TXFULL.bits());
        let mut uart = uart(wire);
        uart.write_str("panic").unwrap();
        assert!(wire.sent.borrow().is_empty());
        wire.tx_status.set(0);
        uart.write_str("!").unwrap();
        assert_eq!(wire.sent.borrow().as_slice(), b"!");
    }

    #[test]
    fn a_newline_goes_out_as_carriage_return_and_newline() {
        let wire = Wire::new();
        uart(wire).write_str("a\nb").unwrap();
        assert_eq!(wire.sent.borrow().as_slice(), b"a\r\nb");
    }

    #[test]
    fn an_empty_receiver_hands_over_nothing() {
        let wire = Wire::new();
        wire.rx_word
            .set(RXDATAFlags::RXEMPTY.bits() | u32::from(b'q'));
        assert_eq!(uart(wire).try_recv().unwrap(), None);
    }

    #[test]
    fn a_waiting_character_is_handed_over() {
        let wire = Wire::new();
        wire.rx_word.set(u32::from(b'q'));
        assert_eq!(uart(wire).try_recv().unwrap(), Some(b'q'));
    }

    /// The bug. `rxdata` is a 32-bit word with the character in its low byte, and
    /// the old code narrowed the whole word to `u8` with `try_into().unwrap_or(0)`
    /// BEFORE masking -- a conversion that fails for anything above 255. One bit
    /// set above the data byte and the character became a NUL, silently, in the
    /// one place that exists to tell you what went wrong.
    #[test]
    fn a_reserved_bit_above_the_character_does_not_blank_it() {
        for noise in [1u32 << 8, 1 << 15, 0x7fff_ff00] {
            let wire = Wire::new();
            wire.rx_word.set(noise | u32::from(b'q'));
            assert_eq!(
                uart(wire).try_recv().unwrap(),
                Some(b'q'),
                "a reserved bit turned the character into a NUL"
            );
        }
    }

    /// RXEMPTY is bit 31 and the character is the low byte, so a character of
    /// 0xFF must not read as "empty" and an empty register must not read as
    /// the character 0x00 -- the two ends of the same word.
    #[test]
    fn the_empty_flag_and_the_character_do_not_bleed_into_each_other() {
        let wire = Wire::new();
        wire.rx_word.set(0xff);
        assert_eq!(uart(wire).try_recv().unwrap(), Some(0xff));

        let wire = Wire::new();
        wire.rx_word.set(RXDATAFlags::RXEMPTY.bits());
        assert_eq!(uart(wire).try_recv().unwrap(), None);
    }

    /// One read of `rxdata` per call: on a real SiFive UART the read POPS the
    /// FIFO, so a second read would throw away the next character.
    #[test]
    fn the_receive_register_is_read_exactly_once_per_call() {
        let wire = Wire::new();
        wire.rx_word.set(u32::from(b'q'));
        uart(wire).try_recv().unwrap();
        assert_eq!(wire.rx_reads.get(), 1);

        let wire = Wire::new();
        wire.rx_word.set(RXDATAFlags::RXEMPTY.bits());
        uart(wire).try_recv().unwrap();
        assert_eq!(wire.rx_reads.get(), 1, "even when there is nothing to take");
    }
}
