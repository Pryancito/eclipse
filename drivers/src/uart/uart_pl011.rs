//! PL011 UART.
use crate::scheme::{impl_event_scheme, Scheme, UartScheme};
use crate::utils::EventListener;
use crate::DeviceResult;
use bitflags::*;
use core::ptr;

bitflags! {
    /// UARTFR
    struct UartFrFlags: u16 {
        const TXFE = 1 << 7;
        const RXFF = 1 << 6;
        const TXFF = 1 << 5;
        const RXFE = 1 << 4;
        const BUSY = 1 << 3;
    }
}

bitflags! {
    /// UARTCR
    struct UartCrFlags: u16 {
        const RXE = 1 << 9;
        const TXE = 1 << 8;
        const UARTEN = 1 << 0;
    }
}

bitflags! {
    // UARTIMSC
    struct UartImscFlags: u16 {
        const RTIM = 1 << 6;
        const TXIM = 1 << 5;
        const RXIM = 1 << 4;
    }
}

bitflags! {
    // UARTICR
    struct UartIcrFlags: u16 {
        const RTIC = 1 << 6;
        const TXIC = 1 << 5;
        const RXIC = 1 << 4;
    }
}

bitflags! {
    //UARTMIS
    struct UartMisFlags: u16 {
        const TXMIS = 1 << 5;
        const RXMIS = 1 << 4;
    }
}

bitflags! {
    //UARTLCR_H
    struct UartLcrhFlags: u16 {
        const FEN = 1 << 4;
    }
}

#[allow(dead_code)]
pub struct Pl011Uart {
    inner: Pl011Inner,
    listener: EventListener,
}

impl Pl011Uart {
    pub fn new(base: usize) -> Self {
        Self {
            inner: {
                let inner = Pl011Inner::new(base);
                inner.init();
                inner
            },
            listener: EventListener::new(),
        }
    }

    fn getchar(&self) -> Option<u8> {
        self.inner.getchar()
    }

    fn putchar(&self, data: u8) {
        self.inner.putchar(data);
    }
}

struct Pl011Inner {
    base: usize,
    data_reg: u8,
    flag_reg: u8,
    line_ctrl_reg: u8,
    ctrl_reg: u8,
    intr_mask_setclr_reg: u8,
    intr_clr_reg: u8,
}

impl Pl011Inner {
    pub fn new(base: usize) -> Pl011Inner {
        Pl011Inner {
            base,
            data_reg: 0x00,
            flag_reg: 0x18,
            line_ctrl_reg: 0x2c,
            ctrl_reg: 0x30,
            intr_mask_setclr_reg: 0x38,
            intr_clr_reg: 0x44,
        }
    }

    fn read_reg(&self, register: u8) -> u16 {
        unsafe { ptr::read_volatile((self.base + register as usize) as *mut u16) }
    }

    fn write_reg(&self, register: u8, data: u16) {
        unsafe {
            ptr::write_volatile((self.base + register as usize) as *mut u16, data);
        }
    }

    fn init(&self) {
        // Clear pending interrupts FIRST. The firmware has been using this port
        // as its own console, so RX and receive-timeout interrupts are normally
        // already pending when we get here. Unmasking before clearing -- which
        // is what this used to do -- leaves a window where the handler fires on
        // the firmware's leftovers, and at that point `Pl011Uart::new` has not
        // built the listener that is supposed to receive them yet.
        self.write_reg(self.intr_clr_reg, 0x7ff);

        // Enable RX, TX, UART
        let flags = UartCrFlags::RXE | UartCrFlags::TXE | UartCrFlags::UARTEN;
        self.write_reg(self.ctrl_reg, flags.bits());

        // Disable the FIFOs (character mode), and touch NOTHING ELSE in
        // UARTLCR_H.
        //
        // The word length, parity, stop bits and stick parity all live in this
        // same register, and they belong to whoever set the port up -- exactly
        // as the 16550 leaves its line control and baud divisor alone. This read
        // -modify-write used to go through `UartLcrhFlags::from_bits_truncate`,
        // and that type knows about ONE bit: `from_bits_truncate` dropped every
        // other bit of the word it had just read, so the write put back a bare
        // zero. Zero in UARTLCR_H is WLEN=00, which is FIVE data bits: the
        // console inherited 8N1 from the firmware and came out the other side
        // configured for 5N1.
        //
        // QEMU's PL011 model ignores the word length and just forwards the byte,
        // so this was invisible here and broken on a real port.
        let lcrh = self.read_reg(self.line_ctrl_reg);
        self.write_reg(self.line_ctrl_reg, lcrh & !UartLcrhFlags::FEN.bits());

        // Enable IRQs
        let flags = UartImscFlags::RXIM;
        self.write_reg(self.intr_mask_setclr_reg, flags.bits());
    }

    fn line_sts(&self) -> UartFrFlags {
        UartFrFlags::from_bits_truncate(self.read_reg(self.flag_reg))
    }

    fn getchar(&self) -> Option<u8> {
        // The question is "is there a byte?", so the flag is RXFE (receive FIFO
        // EMPTY), negated -- not RXFF (receive FIFO FULL). The two are exact
        // complements only while the FIFOs are off, which `init` happens to
        // arrange, so asking for RXFF was right by accident and is a no-op to
        // change today. It stops being a no-op the moment anyone enables the
        // FIFOs for throughput: RXFF would then hold the console silent until
        // sixteen bytes had piled up, and hand them over one per interrupt.
        if !self.line_sts().contains(UartFrFlags::RXFE) {
            Some(self.read_reg(self.data_reg) as u8)
        } else {
            None
        }
    }

    fn putchar(&self, data: u8) {
        // Bounded wait, the same bound and the same reason as the 16550's
        // `send`: this is the path every panic banner takes, it runs with
        // interrupts masked, and an unbounded `while !TXFE {}` on a port that is
        // not clocked -- a board whose firmware left this UART alone -- freezes
        // that CPU for good with nothing on screen to say why. A healthy PL011
        // drains a byte in microseconds; after ~1M polls the port is dead.
        // Losing serial bytes beats hanging the machine, and the two UARTs
        // answering this question differently was the whole problem.
        for _ in 0..1_000_000u32 {
            if self.line_sts().contains(UartFrFlags::TXFE) {
                self.write_reg(self.data_reg, data as u16);
                return;
            }
            core::hint::spin_loop();
        }
    }
}

impl Scheme for Pl011Uart {
    fn name(&self) -> &str {
        "Pl011 ARM series uart"
    }

    fn handle_irq(&self, _irq_num: usize) {
        self.listener.trigger(())
    }
}

impl_event_scheme!(Pl011Uart);

impl UartScheme for Pl011Uart {
    fn try_recv(&self) -> DeviceResult<Option<u8>> {
        Ok(self.getchar())
    }

    fn send(&self, ch: u8) -> DeviceResult {
        self.putchar(ch);
        Ok(())
    }

    fn write_str(&self, s: &str) -> DeviceResult {
        for c in s.bytes() {
            self.send(c)?;
        }
        Ok(())
    }
}

/// The PL011, driven over ordinary memory.
///
/// This is the aarch64 console: every panic banner, every boot line and every
/// log that ends up pasted into a bug report leaves the machine through
/// `putchar`. It had no tests, and it could not have had any -- the module was
/// gated on `target_arch = "aarch64"`, and no `cargo test` invocation in the
/// tree builds for aarch64.
///
/// The driver reaches its registers with `read_volatile`/`write_volatile` at
/// `base + offset` and nothing else, so a leaked block of zeroes IS a device as
/// far as it can tell. The status word is whatever the test puts there, and
/// every write is readable afterwards at its own offset.
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;

    /// UARTLCR_H, the register whose read-modify-write used to flatten itself.
    const WLEN_8BIT: u16 = 0b11 << 5;
    const STP2: u16 = 1 << 3;
    const PEN: u16 = 1 << 1;

    struct Fake {
        base: usize,
    }

    impl Fake {
        fn new() -> Self {
            // 0x80 bytes covers UARTICR at 0x44, and a `u32` block is 4-aligned,
            // which every `u16` access in here needs.
            let block: &'static mut [u32; 0x20] = Box::leak(Box::new([0u32; 0x20]));
            Self {
                base: block as *mut [u32; 0x20] as usize,
            }
        }

        fn uart(&self) -> Pl011Inner {
            Pl011Inner::new(self.base)
        }

        fn get(&self, off: u8) -> u16 {
            unsafe { ptr::read_volatile((self.base + off as usize) as *const u16) }
        }

        fn set(&self, off: u8, value: u16) {
            unsafe { ptr::write_volatile((self.base + off as usize) as *mut u16, value) }
        }
    }

    /// The offsets are the addresses: get one wrong and the driver drives some
    /// other register of the same peripheral, in silence. These are the PL011
    /// TRM's own numbers.
    #[test]
    fn every_register_sits_at_the_offset_the_pl011_manual_gives() {
        let uart = Pl011Inner::new(0);
        assert_eq!(uart.data_reg, 0x00, "UARTDR");
        assert_eq!(uart.flag_reg, 0x18, "UARTFR");
        assert_eq!(uart.line_ctrl_reg, 0x2c, "UARTLCR_H");
        assert_eq!(uart.ctrl_reg, 0x30, "UARTCR");
        assert_eq!(uart.intr_mask_setclr_reg, 0x38, "UARTIMSC");
        assert_eq!(uart.intr_clr_reg, 0x44, "UARTICR");
    }

    /// The bug. UARTLCR_H holds the word length, the parity bits and the stop
    /// bits next to the FIFO-enable bit, and `init` only means to clear the
    /// latter. Going through a `bitflags` type that knows about ONE bit made
    /// `from_bits_truncate` drop the rest of the word it had just read, so the
    /// write put back a bare zero -- and zero is WLEN=00, five data bits. The
    /// console inherited 8N1 from the firmware and came out set to 5N1.
    #[test]
    fn init_does_not_wipe_the_word_format_the_firmware_left_behind() {
        let fake = Fake::new();
        let firmware = WLEN_8BIT | PEN | STP2 | UartLcrhFlags::FEN.bits();
        fake.set(0x2c, firmware);

        fake.uart().init();

        let lcrh = fake.get(0x2c);
        assert_eq!(
            lcrh & UartLcrhFlags::FEN.bits(),
            0,
            "the FIFOs are what init is here to turn off"
        );
        assert_eq!(
            lcrh,
            firmware & !UartLcrhFlags::FEN.bits(),
            "and every other bit of UARTLCR_H is left exactly as it was"
        );
        assert_eq!(lcrh & WLEN_8BIT, WLEN_8BIT, "still eight data bits");
    }

    /// The same thing said the way it would be noticed: whatever word length the
    /// port was set to, it still has it afterwards. 0b00 is the one value that
    /// survives a wipe, so it is the one value this cannot detect -- and it is
    /// also the value the wipe produced.
    #[test]
    fn init_keeps_any_word_length_not_just_the_common_one() {
        for wlen in [0b00u16, 0b01, 0b10, 0b11] {
            let fake = Fake::new();
            fake.set(0x2c, (wlen << 5) | UartLcrhFlags::FEN.bits());
            fake.uart().init();
            assert_eq!(
                (fake.get(0x2c) >> 5) & 0b11,
                wlen,
                "init changed the word length"
            );
        }
    }

    #[test]
    fn init_enables_the_port_and_both_directions() {
        let fake = Fake::new();
        fake.uart().init();
        let cr = fake.get(0x30);
        for (bit, name) in [
            (UartCrFlags::UARTEN, "UARTEN"),
            (UartCrFlags::RXE, "RXE"),
            (UartCrFlags::TXE, "TXE"),
        ] {
            assert_eq!(cr & bit.bits(), bit.bits(), "{} not set by init", name);
        }
    }

    /// `init` must leave the receive interrupt armed -- it is the only one the
    /// console needs -- and must have written the clear-interrupts register on
    /// the way, or the handler's first call is the firmware's leftovers. (The
    /// ORDER of those two writes is what the fix changed, and a plain memory
    /// fake cannot see order: what it can see is that neither write went away.)
    #[test]
    fn init_arms_the_receive_interrupt_and_clears_what_was_pending() {
        let fake = Fake::new();
        fake.uart().init();
        assert_eq!(
            fake.get(0x38) & UartImscFlags::RXIM.bits(),
            UartImscFlags::RXIM.bits(),
            "UARTIMSC must arm RXIM"
        );
        assert_eq!(fake.get(0x44), 0x7ff, "UARTICR must have been written");
    }

    /// A byte only goes out once the port says the transmitter is empty.
    #[test]
    fn a_byte_reaches_the_data_register_when_the_transmitter_is_empty() {
        let fake = Fake::new();
        fake.set(0x18, UartFrFlags::TXFE.bits());
        fake.uart().putchar(b'X');
        assert_eq!(fake.get(0x00), u16::from(b'X'));
    }

    /// A port that never reports an empty transmitter -- one that is mapped but
    /// not clocked, which is what a board that ignores this UART looks like --
    /// must cost a bounded spin and a lost byte, not the CPU. `putchar` runs on
    /// the panic path with interrupts masked, so an unbounded wait there freezes
    /// that processor with nothing on screen to explain it.
    #[test]
    fn a_wedged_port_drops_the_byte_instead_of_hanging_the_cpu() {
        let fake = Fake::new();
        fake.set(0x18, UartFrFlags::BUSY.bits()); // busy, and TXFE never set
        fake.set(0x00, 0xabcd); // a sentinel in the data register
        fake.uart().putchar(b'X');
        assert_eq!(
            fake.get(0x00),
            0xabcd,
            "a byte must not be written to a port that never said it could take one"
        );
    }

    /// The other flag read the wrong way round. `getchar` asked for RXFF
    /// (receive FIFO FULL) when it meant "not empty". With the FIFOs off -- which
    /// `init` arranges -- the two are complements and it worked; with one byte
    /// waiting in an ENABLED FIFO, RXFF is clear and the byte was dropped on the
    /// floor, over and over, with the console apparently deaf.
    #[test]
    fn a_waiting_byte_is_read_even_when_the_receive_fifo_is_not_full() {
        let fake = Fake::new();
        fake.set(0x00, u16::from(b'q'));
        // One byte pending: not empty, and nowhere near full.
        fake.set(0x18, UartFrFlags::TXFE.bits());
        assert!(!UartFrFlags::from_bits_truncate(fake.get(0x18)).contains(UartFrFlags::RXFF));
        assert_eq!(fake.uart().getchar(), Some(b'q'));
    }

    #[test]
    fn an_empty_port_hands_over_nothing() {
        let fake = Fake::new();
        fake.set(0x00, u16::from(b'q'));
        fake.set(0x18, UartFrFlags::RXFE.bits() | UartFrFlags::TXFE.bits());
        assert_eq!(fake.uart().getchar(), None);
    }

    /// Only the low byte of UARTDR is data; the rest of the word is the receive
    /// error flags, and they are not part of the character.
    #[test]
    fn only_the_low_byte_of_the_data_register_is_the_character() {
        let fake = Fake::new();
        fake.set(0x00, 0xf000 | u16::from(b'z'));
        fake.set(0x18, UartFrFlags::TXFE.bits());
        assert_eq!(fake.uart().getchar(), Some(b'z'));
    }

    /// `write_str` is what the console actually calls, so a line has to come out
    /// in order and unmangled.
    #[test]
    fn a_line_goes_out_byte_by_byte_in_order() {
        let fake = Fake::new();
        fake.set(0x18, UartFrFlags::TXFE.bits());
        let uart = fake.uart();
        let mut seen = alloc::vec::Vec::new();
        for byte in b"panic!" {
            uart.putchar(*byte);
            seen.push(fake.get(0x00) as u8);
        }
        assert_eq!(seen.as_slice(), b"panic!");
    }
}

/// The wrapper the rest of the kernel actually holds.
///
/// `Pl011Inner` is covered next door, register by register. This is the other
/// half, and it was at zero: `Pl011Uart::new` and the `Scheme`/`UartScheme`
/// methods are the only path by which a console byte, a console interrupt or
/// the port's own set-up reaches that inner driver, and none of them had ever
/// been called. A `try_recv` wired to the wrong half, a `new` that forgot to
/// call `init`, or a `write_str` that stopped after one byte would leave every
/// test in `tests` green and the aarch64 console broken.
#[cfg(test)]
mod wrapper_tests {
    use super::*;
    use crate::sync::Mutex;
    use alloc::boxed::Box;
    use alloc::sync::Arc;

    const UARTDR: u8 = 0x00;
    const UARTFR: u8 = 0x18;
    const UARTLCRH: u8 = 0x2c;
    const UARTCR: u8 = 0x30;
    const UARTIMSC: u8 = 0x38;
    const UARTICR: u8 = 0x44;

    /// UARTLCR_H bits that belong to whoever set the port up.
    const WLEN_8BIT: u16 = 0b11 << 5;

    /// A value no register of a freshly leaked block can hold by accident, so
    /// "the driver wrote nothing here" is distinguishable from "it wrote zero".
    const UNTOUCHED: u16 = 0xBEEF;

    /// A leaked block of zeroes, which is a PL011 as far as this driver can
    /// tell: it reaches its registers with `read_volatile`/`write_volatile` at
    /// `base + offset` and nothing else.
    struct Port {
        base: usize,
    }

    impl Port {
        fn new() -> Self {
            let block: &'static mut [u32; 0x20] = Box::leak(Box::new([0u32; 0x20]));
            Self {
                base: block as *mut [u32; 0x20] as usize,
            }
        }

        fn get(&self, off: u8) -> u16 {
            unsafe { ptr::read_volatile((self.base + off as usize) as *const u16) }
        }

        fn set(&self, off: u8, value: u16) {
            unsafe { ptr::write_volatile((self.base + off as usize) as *mut u16, value) }
        }

        fn uart(&self) -> Pl011Uart {
            Pl011Uart::new(self.base)
        }

        /// The transmitter is empty, so `putchar` writes on its first look.
        fn the_transmitter_is_ready(&self) {
            self.set(UARTFR, UartFrFlags::TXFE.bits());
        }

        /// A byte is waiting: RXFE (receive FIFO EMPTY) clear is what `getchar`
        /// asks about.
        fn a_byte_is_waiting(&self, byte: u8) {
            self.set(UARTFR, 0);
            self.set(UARTDR, byte as u16);
        }
    }

    /// Counts the events a subscriber is handed.
    fn listening(uart: &Pl011Uart) -> Arc<Mutex<usize>> {
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

        assert_eq!(
            port.get(UARTICR),
            0x7ff,
            "the firmware's pending interrupts have to be cleared, and FIRST"
        );
        let cr = port.get(UARTCR);
        for (bit, name) in &[
            (UartCrFlags::RXE, "RXE"),
            (UartCrFlags::TXE, "TXE"),
            (UartCrFlags::UARTEN, "UARTEN"),
        ] {
            assert_ne!(cr & bit.bits(), 0, "UARTCR is missing {}", name);
        }
        assert_ne!(
            port.get(UARTIMSC) & UartImscFlags::RXIM.bits(),
            0,
            "without RXIM no received byte ever raises an interrupt"
        );
    }

    /// The same bug `tests` pins on `init`, pinned through the constructor:
    /// the word length, parity and stop bits in UARTLCR_H belong to whoever
    /// set the port up, and only the FIFO-enable bit is `init`'s to clear.
    #[test]
    fn a_new_uart_keeps_the_word_format_the_firmware_left() {
        let port = Port::new();
        port.set(UARTLCRH, WLEN_8BIT | UartLcrhFlags::FEN.bits());

        let _uart = port.uart();

        let lcrh = port.get(UARTLCRH);
        assert_eq!(
            lcrh & WLEN_8BIT,
            WLEN_8BIT,
            "eight data bits went in and {:#x} came out",
            lcrh
        );
        assert_eq!(lcrh & UartLcrhFlags::FEN.bits(), 0, "the FIFOs stay off");
    }

    // --- what reaches the inner driver ----------------------------------------

    /// `try_recv` is the console's read path. It has to route to `getchar`,
    /// which asks whether a byte is there before handing one over.
    #[test]
    fn a_byte_waiting_in_the_port_comes_out_of_try_recv() {
        let port = Port::new();
        let uart = port.uart();
        port.a_byte_is_waiting(b'q');

        assert_eq!(
            uart.try_recv().expect("a port with a byte is not an error"),
            Some(b'q')
        );
    }

    /// An empty port has to say so. The data register still holds the last
    /// byte that arrived, so a `try_recv` that skipped the flag check would
    /// hand that stale byte over again, once per call, for ever.
    #[test]
    fn an_empty_port_hands_back_no_byte_rather_than_a_stale_one() {
        let port = Port::new();
        let uart = port.uart();
        port.set(UARTDR, b'q' as u16);
        port.set(UARTFR, UartFrFlags::RXFE.bits());

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
        port.the_transmitter_is_ready();

        uart.send(b'Z').expect("a ready transmitter accepts a byte");

        assert_eq!(port.get(UARTDR), b'Z' as u16);
    }

    /// `write_str` is a loop over `send`, and the loop is the part worth
    /// pinning: one that stopped after the first byte would still put a
    /// plausible-looking character on screen. The data register only keeps the
    /// LAST write, so this says the loop reached the end of the string -- not
    /// that it wrote every byte in between.
    #[test]
    fn a_string_leaves_the_port_through_to_its_last_byte() {
        let port = Port::new();
        let uart = port.uart();
        port.the_transmitter_is_ready();

        uart.write_str("ABC")
            .expect("a ready transmitter accepts a string");

        let last = port.get(UARTDR);
        assert_ne!(last, b'A' as u16, "the loop stopped after the first byte");
        assert_eq!(last, b'C' as u16);
    }

    /// The bound. This is the path a panic banner takes, with interrupts
    /// masked, so a port that is not clocked must not freeze the CPU: the wait
    /// gives up and the byte is lost. Losing serial bytes beats hanging the
    /// machine with nothing on screen to say why.
    #[test]
    fn a_port_that_never_drains_does_not_hold_the_console_for_good() {
        let port = Port::new();
        let uart = port.uart();
        port.set(UARTFR, 0);
        port.set(UARTDR, UNTOUCHED);

        uart.send(b'Z')
            .expect("giving up is not an error the caller sees");

        assert_eq!(
            port.get(UARTDR),
            UNTOUCHED,
            "a byte was pushed at a transmitter that never said it was ready"
        );
    }

    // --- what reaches a client ------------------------------------------------

    /// `handle_irq` is the whole reason the listener exists: the received byte
    /// is announced to whoever subscribed, and a handler that triggers nothing
    /// leaves the console input-dead with every register correctly programmed.
    #[test]
    fn an_interrupt_reaches_the_client_that_subscribed() {
        let port = Port::new();
        let uart = port.uart();
        let hits = listening(&uart);
        assert_eq!(*hits.lock(), 0, "nothing has happened yet");

        uart.handle_irq(7);

        assert_eq!(*hits.lock(), 1);
    }

    /// The name the device registry matches on.
    #[test]
    fn the_uart_answers_to_the_name_the_registry_matches_on() {
        assert_eq!(Port::new().uart().name(), "Pl011 ARM series uart");
    }
}
