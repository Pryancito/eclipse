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

/// The wrapper the rest of the kernel actually holds.
///
/// `UartU740Inner` is covered next door over a recording [`Io`], byte by byte.
/// This is the other half, and it was at zero. The recording port cannot reach
/// it: `UartU740Mmio` holds an `UartU740Inner<Mmio<V>>` specifically, so the
/// only device it accepts is ordinary memory -- which is all it needs, since
/// `Mmio<V>` is `repr(transparent)` and the register map `repr(C)`, so
/// register *n* is the *n*th word of a leaked block.
///
/// What was never called is the path from the kernel to that inner driver:
/// `new`, and the `Scheme`/`UartScheme` methods. A `new` that forgot `init`,
/// or a `try_recv` wired to the transmitter, leaves every test in `tests`
/// green and the riscv64 board console dead.
#[cfg(test)]
mod wrapper_tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::sync::Arc;

    /// Word indices into the register map, in `repr(C)` order.
    const TXDATA: usize = 0;
    const RXDATA: usize = 1;
    const TXCTRL: usize = 2;
    const RXCTRL: usize = 3;
    const IE: usize = 4;

    /// A bit of `txctrl` that belongs to whoever set the port up: the stop-bit
    /// field. `init` means to set TXEN and leave this alone.
    const NSTOP_LEFT_BY_FIRMWARE: u32 = TXCTRLFlags::NSTOP.bits();

    /// A value no freshly leaked register can hold by accident.
    const UNTOUCHED: u32 = 0x5EED_0000;

    /// A leaked block of words, which is an FU740 UART as far as this driver
    /// can tell. Eight words covers the seven registers and keeps the
    /// alignment `Mmio::<u32>::from_base_as` needs.
    struct Port {
        base: usize,
    }

    impl Port {
        fn new() -> Self {
            let block: &'static mut [u32; 8] = Box::leak(Box::new([0u32; 8]));
            Self {
                base: block as *mut [u32; 8] as usize,
            }
        }

        fn get(&self, word: usize) -> u32 {
            unsafe { core::ptr::read_volatile((self.base + word * 4) as *const u32) }
        }

        fn set(&self, word: usize, value: u32) {
            unsafe { core::ptr::write_volatile((self.base + word * 4) as *mut u32, value) }
        }

        fn uart(&self) -> UartU740Mmio<u32> {
            unsafe { UartU740Mmio::<u32>::new(self.base) }
        }

        /// A byte is waiting: RXEMPTY clear is what `try_recv` asks about.
        fn a_byte_is_waiting(&self, byte: u8) {
            self.set(RXDATA, byte as u32);
        }
    }

    /// Counts the events a subscriber is handed.
    fn listening(uart: &UartU740Mmio<u32>) -> Arc<Mutex<usize>> {
        let hits = Arc::new(Mutex::new(0usize));
        let cloned = hits.clone();
        crate::scheme::EventScheme::subscribe(uart, Box::new(move |_| *cloned.lock() += 1), false);
        hits
    }

    // --- what `new` leaves behind ---------------------------------------------

    /// `new` is the only caller of `init` there will ever be, and `init` is
    /// what makes the port able to carry a byte at all. Drop that call and
    /// every test in `tests` still passes, because they all call `init`
    /// themselves.
    #[test]
    fn a_new_uart_has_had_its_port_set_up() {
        let port = Port::new();
        let _uart = port.uart();

        assert_ne!(
            port.get(TXCTRL) & TXCTRLFlags::TXEN.bits(),
            0,
            "without TXEN nothing the kernel prints leaves the chip"
        );
        assert_ne!(
            port.get(RXCTRL) & RXCTRLFlags::RXEN.bits(),
            0,
            "without RXEN the port never receives"
        );
        let ie = port.get(IE);
        assert_ne!(ie & IEFlags::TXWM.bits(), 0, "IE is missing TXWM");
        assert_ne!(ie & IEFlags::RXWM.bits(), 0, "IE is missing RXWM");
    }

    /// `init` sets TXEN by reading the register and putting the bit back, so
    /// the stop-bit and watermark fields the firmware chose survive. A write
    /// of a bare `TXEN` would pass the test above and silently reset them.
    #[test]
    fn a_new_uart_keeps_the_transmit_settings_the_firmware_left() {
        let port = Port::new();
        port.set(TXCTRL, NSTOP_LEFT_BY_FIRMWARE);

        let _uart = port.uart();

        assert_eq!(
            port.get(TXCTRL) & NSTOP_LEFT_BY_FIRMWARE,
            NSTOP_LEFT_BY_FIRMWARE,
            "the firmware's stop-bit setting was overwritten, txctrl is {:#x}",
            port.get(TXCTRL)
        );
    }

    // --- what reaches the inner driver ----------------------------------------

    /// `try_recv` is the console's read path, and it has to route to the
    /// RECEIVE register. Routed to the transmitter it would read back the last
    /// byte printed, which looks like an echo and is not one.
    #[test]
    fn a_byte_waiting_in_the_port_comes_out_of_try_recv() {
        let port = Port::new();
        let uart = port.uart();
        port.a_byte_is_waiting(b'q');
        port.set(TXDATA, b'Z' as u32);

        assert_eq!(
            uart.try_recv().expect("a port with a byte is not an error"),
            Some(b'q')
        );
    }

    /// An empty port has to say so rather than hand over whatever bit pattern
    /// the register happens to hold.
    #[test]
    fn an_empty_port_hands_back_no_byte_rather_than_a_stale_one() {
        let port = Port::new();
        let uart = port.uart();
        port.set(RXDATA, RXDATAFlags::RXEMPTY.bits() | b'q' as u32);

        assert_eq!(
            uart.try_recv().expect("an empty port is not an error"),
            None
        );
    }

    /// `send` is the console's write path, and the one every panic banner
    /// takes.
    #[test]
    fn send_puts_the_byte_in_the_port() {
        let port = Port::new();
        let uart = port.uart();

        uart.send(b'Z')
            .expect("a transmitter with room accepts a byte");

        assert_eq!(port.get(TXDATA) & 0xFF, b'Z' as u32);
    }

    /// `write_str` is a loop, and the loop is the part worth pinning: one that
    /// stopped after the first byte would still put a plausible-looking
    /// character on screen. The register only keeps the LAST write, so this
    /// says the loop reached the end of the string -- not that it wrote every
    /// byte in between.
    #[test]
    fn a_string_leaves_the_port_through_to_its_last_byte() {
        let port = Port::new();
        let uart = port.uart();

        uart.write_str("ABC")
            .expect("a transmitter with room accepts a string");

        let last = port.get(TXDATA) & 0xFF;
        assert_ne!(last, b'A' as u32, "the loop stopped after the first byte");
        assert_eq!(last, b'C' as u32);
    }

    /// The bound. This runs under the uart's IRQ-masking lock on the path
    /// every panic banner takes, so a port that is not clocked -- TXFULL stuck
    /// -- must not freeze the CPU. The wait gives up and the byte is lost.
    #[test]
    fn a_port_that_never_drains_does_not_hold_the_console_for_good() {
        let port = Port::new();
        let uart = port.uart();
        port.set(TXDATA, TXDATAFlags::TXFULL.bits() | UNTOUCHED);

        uart.send(b'Z')
            .expect("giving up is not an error the caller sees");

        assert_eq!(
            port.get(TXDATA),
            TXDATAFlags::TXFULL.bits() | UNTOUCHED,
            "a byte was pushed at a transmitter that said it was full"
        );
    }

    // --- what reaches a client ------------------------------------------------

    /// `handle_irq` is the whole reason the listener exists: a handler that
    /// triggers nothing leaves the console input-dead with every register
    /// correctly programmed.
    #[test]
    fn an_interrupt_reaches_the_client_that_subscribed() {
        let port = Port::new();
        let uart = port.uart();
        let hits = listening(&uart);
        assert_eq!(*hits.lock(), 0, "nothing has happened yet");

        uart.handle_irq(7);

        assert_eq!(*hits.lock(), 1);
    }

    /// The name the device registry matches on. It used to say "UART 16550",
    /// which is a different peripheral in a different file.
    #[test]
    fn the_uart_answers_to_the_name_the_registry_matches_on() {
        assert_eq!(Port::new().uart().name(), "uart-u740-mmio");
    }
}
