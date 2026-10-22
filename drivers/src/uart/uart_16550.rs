use core::convert::TryInto;
use core::ops::{BitAnd, BitOr, Not};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::sync::{HeldByCurrentCpu, Mutex};
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

/// How many console writes have been served without taking the port's lock
/// because **this CPU already held it**.
///
/// The lock is an IRQ-disabling ticket mutex, so no interrupt can re-enter it
/// -- but a fault or a panic taken mid-write can, and on the bring-up box it
/// did:
///
/// ```text
/// cpu=18 at drivers/src/uart/uart_16550.rs:249
/// HOLDER cpu=18 at drivers/src/uart/uart_16550.rs:249
/// ```
///
/// one CPU, both ends of the same acquire. A ticket mutex is not re-entrant, so
/// that second acquire waits -- with interrupts off -- for a release only this
/// CPU could perform, and this CPU is inside the panic handler trying to print.
/// The machine halted with the report half out.
///
/// `kernel-hal`'s `ConsoleLock` already survives exactly this ("a nested write
/// from the CPU that already holds the lock goes straight through"), and then
/// calls down into here, where the escape ran out one layer short of the
/// hardware. The escape costs interleaved bytes on a console that is already
/// printing a crash; the alternative is the crash report and the machine with
/// it. The same trade [`crate::utils::shadow_fb`] makes for the framebuffer.
///
/// Non-zero on a live machine means that happened at least once.
static REENTRANT_WRITES: AtomicU32 = AtomicU32::new(0);

/// Read [`REENTRANT_WRITES`].
pub fn reentrant_console_writes() -> u32 {
    REENTRANT_WRITES.load(Ordering::Relaxed)
}

/// The line-status register's index in the 16550 map, for the raw path below.
/// Held to the `#[repr(C)]` field order by
/// `the_raw_path_reads_the_same_register_the_struct_calls_line_sts`.
const REG_LINE_STS: usize = 5;

/// Send one byte over the register block at `base` **without ever naming it**.
///
/// The re-entrant path may not build a second `&mut Uart16550Inner<Mmio<V>>`:
/// the first one is alive inside the mutex, held by the frame this fault
/// interrupted, and a second `&mut` to the same place is undefined behaviour
/// however device-like the memory is -- on the one path whose whole job is to
/// run when everything else has already gone wrong. Volatile accesses through
/// raw pointers create no reference at all, so there is nothing to alias.
///
/// `Mmio<V>` is `#[repr(transparent)]` over `V` and the map is `#[repr(C)]`, so
/// register *n* is at `base + n * size_of::<V>()` -- the same arithmetic the
/// struct layout does, checked against it in the tests.
///
/// Bounded like [`Uart16550Inner::send`], and for the same reason: an absent or
/// wedged port must not hold a CPU for ever.
///
/// # Safety
///
/// `base` must be the address of a live 16550 register block of width `V`.
unsafe fn raw_send<V>(base: usize, ch: u8)
where
    V: Copy + BitAnd<Output = V> + From<u8> + TryInto<u8>,
{
    let stride = core::mem::size_of::<V>();
    let data = base as *mut V;
    let line_sts = (base + REG_LINE_STS * stride) as *const V;
    for _ in 0..1_000_000u32 {
        // SAFETY: `line_sts` is register 5 of the caller's block.
        let sts = unsafe { core::ptr::read_volatile(line_sts) };
        #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
        // SAFETY: the same barrier `Mmio::read` issues after a device read.
        unsafe {
            core::arch::asm!("fence i,r")
        };
        let sts: u8 = (sts & V::from(0xFF)).try_into().unwrap_or(0);
        if sts & LineStsFlags::OUTPUT_EMPTY.bits() != 0 {
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            // SAFETY: the same barrier `Mmio::write` issues before a device write.
            unsafe {
                core::arch::asm!("fence w,o")
            };
            // SAFETY: `data` is register 0 of the caller's block.
            unsafe { core::ptr::write_volatile(data, V::from(ch)) };
            return;
        }
        core::hint::spin_loop();
    }
}

/// [`raw_send`] over a whole string, newline translation included -- the same
/// one [`Uart16550Inner::write_str`] does.
///
/// # Safety
///
/// As [`raw_send`].
unsafe fn raw_write_str<V>(base: usize, s: &str)
where
    V: Copy + BitAnd<Output = V> + From<u8> + TryInto<u8>,
{
    for b in s.bytes() {
        // SAFETY: the caller's contract, passed straight on.
        unsafe {
            if b == b'\n' {
                raw_send::<V>(base, b'\r');
            }
            raw_send::<V>(base, b);
        }
    }
}

/// MMIO driver for UART 16550
pub struct Uart16550Mmio<V: 'static>
where
    V: Copy + BitAnd<Output = V> + BitOr<Output = V> + Not<Output = V>,
{
    inner: Mutex<&'static mut Uart16550Inner<Mmio<V>>>,
    /// The register block's address, kept so [`raw_write_str`] can reach the
    /// registers on the one path that may not take `inner`.
    base: usize,
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
        // Declining, where the write paths take the raw escape: a re-entrant
        // read would consume a byte the interrupted reader is in the middle of
        // taking, and nothing on a fault path needs to receive.
        if self.inner.held_by_current_cpu() {
            return Ok(None);
        }
        self.inner.lock().try_recv()
    }

    fn send(&self, ch: u8) -> DeviceResult {
        if self.inner.held_by_current_cpu() {
            REENTRANT_WRITES.fetch_add(1, Ordering::Relaxed);
            // SAFETY: `self.base` is the block `new_common` initialised.
            unsafe { raw_send::<V>(self.base, ch) };
            return Ok(());
        }
        self.inner.lock().send(ch)
    }

    fn write_str(&self, s: &str) -> DeviceResult {
        if self.inner.held_by_current_cpu() {
            REENTRANT_WRITES.fetch_add(1, Ordering::Relaxed);
            // SAFETY: as above.
            unsafe { raw_write_str::<V>(self.base, s) };
            return Ok(());
        }
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
                base,
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
        /// The first of the seven ports, kept so [`Uart16550Pmio::with_regs`]
        /// can rebuild the map on the one path that may not take `inner`.
        base: u16,
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
            // See the MMIO twin: a re-entrant read would eat a byte out from
            // under the interrupted reader, and no fault path receives.
            if self.inner.held_by_current_cpu() {
                return Ok(None);
            }
            self.inner.lock().try_recv()
        }

        fn send(&self, ch: u8) -> DeviceResult {
            self.with_regs(|regs| regs.send(ch))
        }

        fn write_str(&self, s: &str) -> DeviceResult {
            self.with_regs(|regs| regs.write_str(s))
        }
    }

    /// The seven 16550 registers as consecutive byte ports from `base`.
    ///
    /// Separate from [`Uart16550Pmio::new`] because `new` goes straight on to
    /// `init`, which issues `out`: the map is the half that can be got wrong
    /// *and* checked without a serial port. Its MMIO twin derives each address
    /// from the `#[repr(C)]` layout, so it follows the field order on its own;
    /// here every address is written out by hand, and a wrong `base + n` sends
    /// one register's traffic to another's port. On x86_64 -- the only place
    /// this exists -- that is the console every panic banner leaves by.
    fn ports(base: u16) -> Uart16550Inner<Pmio<u8>> {
        Uart16550Inner::<Pmio<u8>> {
            data: Pmio::new(base),
            int_en: Pmio::new(base + 1),
            fifo_ctrl: Pmio::new(base + 2),
            line_ctrl: Pmio::new(base + 3),
            modem_ctrl: Pmio::new(base + 4),
            line_sts: ReadOnly::new(Pmio::new(base + 5)),
            modem_sts: ReadOnly::new(Pmio::new(base + 6)),
        }
    }

    impl Uart16550Pmio {
        /// Construct a `Uart16550Pmio` whose address starts at `base`.
        pub fn new(base: u16) -> Self {
            let mut uart = ports(base);
            uart.init();
            Self {
                inner: Mutex::new(uart),
                base,
                listener: EventListener::new(),
            }
        }

        /// Run `f` on the seven ports, through the lock unless **this CPU
        /// already holds it** -- see [`super::REENTRANT_WRITES`] for the
        /// capture this exists for. This is the x86_64 console: the port every
        /// panic banner leaves by, and the one the photograph named.
        ///
        /// `ports(base)` rebuilds the map rather than reusing the guarded one.
        /// Where the MMIO twin has to go through raw pointers, this one may
        /// build a whole second map: a [`Pmio`] is a port *number*, not a
        /// reference, so two of them for one port alias nothing -- only the
        /// device, which was already shared.
        ///
        /// It does not call `init`: re-initialising the FIFO mid-crash would
        /// throw away bytes of the report already queued in it.
        #[inline]
        fn with_regs<R>(&self, f: impl FnOnce(&mut Uart16550Inner<Pmio<u8>>) -> R) -> R {
            if self.inner.held_by_current_cpu() {
                super::REENTRANT_WRITES.fetch_add(1, super::Ordering::Relaxed);
                let mut regs = ports(self.base);
                return f(&mut regs);
            }
            f(&mut self.inner.lock())
        }
    }

    #[cfg(test)]
    mod pmio_tests {
        use super::*;

        /// The MMIO map has `every_register_sits_at_its_own_offset_in_the_16550_map`
        /// to hold its addresses in place; the PMIO map had nothing, and it is the
        /// one the x86_64 kernel actually opens (`0x3F8`, and `0x2F8` for the
        /// second port). A single wrong `base + n` is silent and total: send
        /// reading line status from the modem-status port never sees
        /// `OUTPUT_EMPTY`, so it spins out its whole bounded wait on every byte
        /// and the console stops -- on the path whose only job is to carry the
        /// evidence when everything else has already failed.
        #[test]
        fn every_register_sits_at_its_own_port_in_the_pmio_16550_map() {
            for base in [0x3F8u16, 0x2F8, 0] {
                let map = ports(base);
                for (i, (name, port)) in [
                    ("data", map.data.port()),
                    ("int_en", map.int_en.port()),
                    ("fifo_ctrl", map.fifo_ctrl.port()),
                    ("line_ctrl", map.line_ctrl.port()),
                    ("modem_ctrl", map.modem_ctrl.port()),
                    ("line_sts", map.line_sts.unit().port()),
                    ("modem_sts", map.modem_sts.unit().port()),
                ]
                .iter()
                .enumerate()
                {
                    assert_eq!(
                        *port - base,
                        i as u16,
                        "register {} is at the wrong port from base {:#x}",
                        name,
                        base
                    );
                }
            }
        }

        /// And the two maps must agree with each other: the PMIO offsets are
        /// hand-written while the MMIO ones come from the struct layout, so a
        /// register inserted, removed or reordered moves one and not the other.
        #[test]
        fn the_pmio_ports_and_the_mmio_offsets_are_the_same_map() {
            let map = ports(0);
            let mmio = core::mem::MaybeUninit::<Uart16550Inner<Mmio<u8>>>::uninit();
            let at = mmio.as_ptr() as usize;
            // SAFETY: `addr_of!` only takes each field's address; nothing in the
            // uninitialised map is read.
            let offsets = unsafe {
                [
                    core::ptr::addr_of!((*mmio.as_ptr()).data) as usize - at,
                    core::ptr::addr_of!((*mmio.as_ptr()).int_en) as usize - at,
                    core::ptr::addr_of!((*mmio.as_ptr()).fifo_ctrl) as usize - at,
                    core::ptr::addr_of!((*mmio.as_ptr()).line_ctrl) as usize - at,
                    core::ptr::addr_of!((*mmio.as_ptr()).modem_ctrl) as usize - at,
                    core::ptr::addr_of!((*mmio.as_ptr()).line_sts) as usize - at,
                    core::ptr::addr_of!((*mmio.as_ptr()).modem_sts) as usize - at,
                ]
            };
            let ports = [
                map.data.port(),
                map.int_en.port(),
                map.fifo_ctrl.port(),
                map.line_ctrl.port(),
                map.modem_ctrl.port(),
                map.line_sts.unit().port(),
                map.modem_sts.unit().port(),
            ];
            for (i, (offset, port)) in offsets.iter().zip(ports.iter()).enumerate() {
                assert_eq!(
                    *offset, *port as usize,
                    "register {} is at {:#x} over MMIO and port {:#x} over PMIO",
                    i, offset, port
                );
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

    // --- the re-entrant console escape ------------------------------------

    /// What `with_regs` does when this CPU already holds the port's lock: it
    /// builds a SECOND map over the same registers and writes through that.
    /// The capture it exists for had holder and waiter on one CPU and one line
    /// (`uart_16550.rs:249`), i.e. a panic taken inside the driver's own
    /// critical section asking for a release only itself could perform.
    ///
    /// What has to hold for that escape to be worth taking: the duplicate view
    /// must reach the same wire, and the interrupted write must still be intact
    /// when it resumes. Bytes interleave -- that is the accepted cost -- but
    /// none is lost or rerouted.
    #[test]
    fn a_second_map_over_the_same_ports_reaches_the_same_wire_and_loses_nothing() {
        let wire = Wire::new(READY);
        let mut outer = uart(wire);
        outer.write_str("ab").unwrap();
        // The fault lands mid-write. `with_regs` hands the panic handler a
        // fresh map instead of a guard it would wait forever for.
        let mut nested = uart(wire);
        nested.write_str("PANIC").unwrap();
        // ...and the interrupted writer carries on through its own map.
        outer.write_str("cd").unwrap();
        assert_eq!(
            wire.sent.borrow().as_slice(),
            b"abPANICcd",
            "the nested map rerouted or dropped bytes"
        );
    }

    /// The nested map must NOT re-initialise the port. `init` writes `0xC7` to
    /// the FIFO control register, which clears the transmit queue -- doing that
    /// mid-crash throws away the bytes of the report already sitting in it. So
    /// the escape rebuilds the map and nothing else.
    #[test]
    fn the_nested_map_does_not_clear_the_fifo_the_report_is_queued_in() {
        let wire = Wire::new(READY);
        let mut nested = uart(wire);
        nested.write_str("report").unwrap();
        assert!(
            !wire
                .control
                .borrow()
                .iter()
                .any(|(reg, _)| *reg == Reg::FifoCtrl),
            "the nested write touched FIFO control: {:?}",
            wire.control.borrow()
        );
    }

    /// The one way the MMIO escape can be worse than the deadlock it replaces:
    /// wrong register arithmetic. `raw_send` cannot name the fields -- building
    /// a second `&mut` to a block the interrupted frame still holds is UB -- so
    /// it computes `base + n * size_of::<V>()` by hand, which is a second place
    /// to get the 16550 map wrong, silently, on the panic path. These pin it to
    /// the `#[repr(C)]` layout the locked path uses, at both widths.
    #[test]
    fn the_raw_path_reads_the_same_register_the_struct_calls_line_sts() {
        fn offsets<V>() -> (usize, usize)
        where
            V: Copy + BitAnd<Output = V> + BitOr<Output = V> + Not<Output = V>,
        {
            let map = core::mem::MaybeUninit::<Uart16550Inner<Mmio<V>>>::uninit();
            let at = map.as_ptr() as usize;
            // SAFETY: `addr_of!` only takes addresses; nothing is read.
            unsafe {
                (
                    core::ptr::addr_of!((*map.as_ptr()).data) as usize - at,
                    core::ptr::addr_of!((*map.as_ptr()).line_sts) as usize - at,
                )
            }
        }
        for (width, (data, line_sts), stride) in [
            ("u8", offsets::<u8>(), core::mem::size_of::<u8>()),
            ("u32", offsets::<u32>(), core::mem::size_of::<u32>()),
        ] {
            assert_eq!(data, 0, "{}: the data register is not at the base", width);
            assert_eq!(
                line_sts,
                REG_LINE_STS * stride,
                "{}: raw_send would poll the wrong register for OUTPUT_EMPTY",
                width
            );
        }
    }

    /// And the bit it tests for is the one the locked path tests for: a byte
    /// goes out on `OUTPUT_EMPTY`, not on some neighbouring status bit.
    #[test]
    fn the_raw_path_waits_on_the_same_bit_the_locked_path_waits_on() {
        assert_eq!(LineStsFlags::OUTPUT_EMPTY.bits(), 1 << 5);
    }

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

/// The MMIO driver and the two crash-path writes, over a register block in
/// ordinary memory.
///
/// `mod tests` above drives `Uart16550Inner` through a fake `Io`, which is the
/// whole driver for a port that answers. It cannot reach either of the two
/// halves below.
///
/// `raw_send` and `raw_write_str` have **no `Io`**: the reason they exist is
/// that the path they serve may not build a second reference to the register
/// block, so they compute `base + n * size_of::<V>()` and go through raw
/// pointers. Two tests up there check the constants they use -- by recomputing
/// the offsets from the struct and comparing -- and neither one calls either
/// function. A `raw_send` that polled the right register and then put the byte
/// in the wrong one, or never wrote it at all, or never looked at the status
/// bit, passes both: the last write the kernel makes once it has started to
/// panic was held in place by arithmetic nothing ran.
///
/// And `Uart16550Mmio` itself -- the type the device tree builds, with the
/// lock, the name the console registry matches on, and the interrupt that
/// wakes a reader -- was never constructed by anything in the suite.
///
/// Volatile reads and writes at `base + offset` are all either half does, so a
/// leaked block of zeroes IS a 16550 as far as they can tell: the same bench
/// the PL011 driver next door uses.
#[cfg(test)]
mod mmio_tests {
    use super::*;
    use crate::scheme::EventScheme;
    use alloc::{boxed::Box, sync::Arc, vec::Vec};
    use core::cell::RefCell;
    use core::convert::TryFrom;
    use core::sync::atomic::AtomicBool;

    /// A line status with room in the transmit holding register.
    const READY: u8 = 1 << 5;
    /// ...and with a byte waiting to be read.
    const HAS_INPUT: u8 = 1;

    /// The seven registers of the 16550 map, by index. Register *n* sits at
    /// `base + n * size_of::<V>()`, which is the arithmetic under test.
    const DATA: usize = 0;
    const INT_EN: usize = 1;
    const FIFO_CTRL: usize = 2;
    const LINE_CTRL: usize = 3;
    const MODEM_CTRL: usize = 4;
    const LINE_STS: usize = 5;
    const MODEM_STS: usize = 6;
    /// How many of them there are.
    const REGISTERS: usize = 7;

    /// The address of a leaked block of seven units of `V`, which is all
    /// `raw_send`, `raw_write_str` and `Mmio::from_base_as` ever ask of a
    /// 16550.
    ///
    /// Leaked, and only the address comes back, so nothing in the test holds a
    /// reference to memory the driver builds its own `&mut` over; every look
    /// afterwards goes through a raw pointer, which is what the driver's own
    /// crash path does and for the same reason.
    fn a_block<V: Copy + 'static>(zero: V) -> usize {
        let block: &'static mut [V; REGISTERS] = Box::leak(Box::new([zero; REGISTERS]));
        block.as_mut_ptr() as usize
    }

    /// What register `n` of the block at `base` holds now.
    fn reg<V: Copy>(base: usize, n: usize) -> V {
        // SAFETY: `base` came from `a_block`, which leaked REGISTERS units.
        unsafe { core::ptr::read_volatile((base + n * core::mem::size_of::<V>()) as *const V) }
    }

    /// Put `value` in register `n`, the way the port itself would.
    fn set_reg<V: Copy>(base: usize, n: usize, value: V) {
        // SAFETY: as `reg`.
        unsafe {
            core::ptr::write_volatile((base + n * core::mem::size_of::<V>()) as *mut V, value)
        }
    }

    std::thread_local! {
        /// Every byte the raw path handed to a register of THIS test's block,
        /// in order. Thread-local because the harness runs tests in parallel
        /// and a process-global list could not say which test wrote which
        /// byte.
        static HANDED_OVER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// A register value type that records what was put in it.
    ///
    /// The raw path takes no `Io` -- that is the point of it -- so there is no
    /// trait object to put in front of the registers, and a block of plain
    /// memory keeps only the last byte written to the data register. What the
    /// raw path does take is the bus width as a type parameter, and
    /// `V::from(u8)` is called once per byte written and once per poll of the
    /// status register, so the type itself is the seam: the order it records is
    /// the order the bytes went out.
    ///
    /// One byte wide, like a 16550 register, so the block is laid out exactly
    /// as an 8-bit port's.
    #[repr(transparent)]
    #[derive(Copy, Clone)]
    struct Recorded(u8);

    impl From<u8> for Recorded {
        fn from(byte: u8) -> Self {
            HANDED_OVER.with(|log| log.borrow_mut().push(byte));
            Self(byte)
        }
    }

    impl BitAnd for Recorded {
        type Output = Self;

        fn bitand(self, other: Self) -> Self {
            Self(self.0 & other.0)
        }
    }

    impl TryFrom<Recorded> for u8 {
        type Error = core::convert::Infallible;

        fn try_from(value: Recorded) -> Result<Self, Self::Error> {
            Ok(value.0)
        }
    }

    /// The bytes handed over since the last look, and clear.
    fn handed_over() -> Vec<u8> {
        HANDED_OVER.with(|log| core::mem::take(&mut *log.borrow_mut()))
    }

    /// The mask `raw_send` builds to keep the low byte of a status word, which
    /// goes through the same `V::from` hook and so shows up in the log too --
    /// one per byte, which is what says the status was asked each time.
    const MASK: u8 = 0xFF;

    // --- the crash path ---------------------------------------------------

    /// The byte has to land in register 0 and nowhere else. `raw_send` names no
    /// field, so a wrong `*mut V` sends the console's last words to the FIFO
    /// control register -- which on a real port clears the queue the rest of
    /// the banner is sitting in.
    #[test]
    fn the_crash_path_puts_the_byte_in_the_data_register_and_touches_nothing_else() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, u32::from(READY));
        // SAFETY: `base` is a live seven-register block of 32-bit units.
        unsafe { raw_send::<u32>(base, b'x') };
        assert_eq!(
            reg::<u32>(base, DATA),
            u32::from(b'x'),
            "the byte did not reach the data register"
        );
        for n in [INT_EN, FIFO_CTRL, LINE_CTRL, MODEM_CTRL, MODEM_STS] {
            assert_eq!(
                reg::<u32>(base, n),
                0,
                "the crash path wrote register {}",
                n
            );
        }
        assert_eq!(
            reg::<u32>(base, LINE_STS),
            u32::from(READY),
            "the status register is read-only to the driver"
        );
    }

    /// And the register it polls has to be register 5. Either neighbour reads
    /// ready here and register 5 does not: a `REG_LINE_STS` off by one spins
    /// out the whole bounded wait on every byte, and the console goes quiet on
    /// the one path whose only job is to carry the evidence.
    #[test]
    fn the_crash_path_polls_the_status_register_and_not_one_beside_it() {
        let base = a_block(0u32);
        set_reg(base, MODEM_CTRL, u32::from(READY));
        set_reg(base, MODEM_STS, u32::from(READY));
        // SAFETY: as above.
        unsafe { raw_send::<u32>(base, b'x') };
        assert_eq!(
            reg::<u32>(base, DATA),
            0,
            "the byte went out on a status bit read from the wrong register"
        );
    }

    /// A port with no cable is the NORM on the bring-up box, and this path runs
    /// from inside the panic handler: an unbounded wait here is the machine
    /// halting with the report half out, which is the capture the whole escape
    /// exists for. Returning at all is the assertion; the dropped byte is the
    /// accepted cost.
    #[test]
    fn a_crash_path_write_to_a_port_that_never_drains_is_dropped_and_not_a_hang() {
        let base = a_block(0u32);
        // SAFETY: as above.
        unsafe { raw_send::<u32>(base, b'x') };
        assert_eq!(
            reg::<u32>(base, DATA),
            0,
            "the byte cannot reach a port that never said it had room"
        );
    }

    /// A 32-bit-wide 16550 answers a status read in the low byte and leaves the
    /// other three undefined, so the driver masks before it truncates to `u8`.
    /// Without the mask the `try_into` fails, `unwrap_or(0)` reports a port
    /// with no room, and every byte of the banner is dropped -- on a port that
    /// was ready the whole time.
    #[test]
    fn the_crash_path_masks_the_status_word_a_wide_port_reads_back() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, 0xDEAD_0000 | u32::from(READY));
        // SAFETY: as above.
        unsafe { raw_send::<u32>(base, b'x') };
        assert_eq!(
            reg::<u32>(base, DATA),
            u32::from(b'x'),
            "the junk in the upper bits of the status word stopped the byte"
        );
    }

    /// One poll per byte, and the bytes in the order they were given: a loop
    /// that stopped after the first byte, or wrote the string backwards, leaves
    /// the same single byte in a block of plain memory.
    #[test]
    fn the_crash_path_asks_the_status_once_per_byte_and_sends_them_in_order() {
        let base = a_block(Recorded(0));
        set_reg(base, LINE_STS, Recorded(READY));
        let _ = handed_over();
        // SAFETY: `base` is a live seven-register block of one-byte units.
        unsafe { raw_write_str::<Recorded>(base, "ab") };
        assert_eq!(
            handed_over().as_slice(),
            &[MASK, b'a', MASK, b'b'][..],
            "the crash path did not ask the status once per byte"
        );
    }

    /// The carriage return is the byte a block of memory cannot show -- it is
    /// always overwritten by the newline that follows it -- and it is the one
    /// that keeps a panic banner from stairstepping down the terminal. The
    /// recording type is what makes it visible, and the translation here is a
    /// SECOND copy of the one `write_str` does, on the path that only runs when
    /// the first copy's lock is unavailable.
    #[test]
    fn the_crash_path_turns_a_newline_into_a_carriage_return_and_a_newline() {
        let base = a_block(Recorded(0));
        set_reg(base, LINE_STS, Recorded(READY));
        let _ = handed_over();
        // SAFETY: as above.
        unsafe { raw_write_str::<Recorded>(base, "a\nb") };
        assert_eq!(
            handed_over().as_slice(),
            &[MASK, b'a', MASK, b'\r', MASK, b'\n', MASK, b'b'][..],
            "the newline did not go out as carriage return then newline"
        );
    }

    // --- the driver the device tree builds --------------------------------

    /// `new` hands the block to `init` and keeps the address, and both halves
    /// matter: the port cannot carry a byte until it has been told to, and the
    /// address is the only thing the crash path has to reach the registers by.
    #[test]
    fn building_the_driver_initialises_the_port_and_remembers_its_address() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, u32::from(READY));
        // SAFETY: `base` is a live seven-register block of 32-bit units.
        let uart = unsafe { Uart16550Mmio::<u32>::new(base) };
        assert_eq!(
            uart.base, base,
            "the crash path would write to some other address"
        );
        assert_eq!(reg::<u32>(base, INT_EN), 0x01, "interrupts were left off");
        assert_eq!(reg::<u32>(base, FIFO_CTRL), 0xC7, "the FIFO was left alone");
        assert_eq!(
            reg::<u32>(base, MODEM_CTRL),
            0x0B,
            "the modem lines were left alone"
        );
        assert_eq!(
            reg::<u32>(base, LINE_CTRL),
            0,
            "the word format is the firmware's, not ours"
        );
        assert_eq!(
            reg::<u32>(base, DATA),
            0,
            "bringing the port up must not put a byte on the wire"
        );
    }

    /// The 8-bit map is a different instantiation reached through a constructor
    /// of its own, and it is the one an aarch64 board opens.
    #[test]
    fn an_eight_bit_wide_port_is_built_through_its_own_constructor() {
        let base = a_block(0u8);
        set_reg(base, LINE_STS, READY);
        // SAFETY: `base` is a live seven-register block of one-byte units.
        let uart = unsafe { Uart16550Mmio::<u8>::new(base) };
        assert_eq!(uart.base, base);
        assert_eq!(reg::<u8>(base, INT_EN), 0x01, "interrupts were left off");
        assert_eq!(reg::<u8>(base, FIFO_CTRL), 0xC7, "the FIFO was left alone");
        assert_eq!(
            reg::<u8>(base, MODEM_CTRL),
            0x0B,
            "the modem lines were left alone"
        );
    }

    /// The string the console registry matches a device-tree node against. A
    /// change here is a console that is built and never opened.
    #[test]
    fn the_driver_answers_to_the_name_the_console_registry_matches_on() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, u32::from(READY));
        // SAFETY: as above.
        let uart = unsafe { Uart16550Mmio::<u32>::new(base) };
        assert_eq!(uart.name(), "uart16550-mmio");
    }

    /// An interrupt has to reach the listener, because that is the whole of
    /// what the serial console does to wake a reader: a `handle_irq` that
    /// returned without triggering leaves a `read` on the console blocked with
    /// the byte already in the port.
    #[test]
    fn an_interrupt_at_the_port_reaches_a_subscribed_handler() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, u32::from(READY));
        // SAFETY: as above.
        let uart = unsafe { Uart16550Mmio::<u32>::new(base) };
        let woken = Arc::new(AtomicBool::new(false));
        let flag = woken.clone();
        uart.subscribe(
            Box::new(move |_| flag.store(true, Ordering::Relaxed)),
            false,
        )
        .expect("the first subscription cannot be refused");
        uart.handle_irq(4);
        assert!(
            woken.load(Ordering::Relaxed),
            "the interrupt did not reach the listener"
        );
    }

    /// Sending through the scheme and sending through the crash escape put the
    /// SAME byte in the SAME register and both answer `Ok(())`: what tells them
    /// apart is whether the lock was taken, and the only witness to that is the
    /// counter the escape bumps. An ordinary send must not bump it.
    ///
    /// A global counter in a parallel suite is usually a race, and this one is
    /// read as a delta for that reason -- but it cannot move here at all: the
    /// escape is reached only through `held_by_current_cpu`, which is a
    /// constant `false` on a hosted build, so nothing in this binary can bump
    /// it. Should a seam ever make the escape reachable, this assert has to
    /// move to a witness local to this port.
    #[test]
    fn a_byte_sent_through_the_scheme_goes_out_through_the_lock_and_not_the_escape() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, u32::from(READY));
        // SAFETY: as above.
        let uart = unsafe { Uart16550Mmio::<u32>::new(base) };
        let before = reentrant_console_writes();
        uart.send(b'Z').unwrap();
        assert_eq!(
            reg::<u32>(base, DATA),
            u32::from(b'Z'),
            "the byte did not reach the port"
        );
        assert_eq!(
            reentrant_console_writes(),
            before,
            "an ordinary send took the crash escape"
        );
    }

    /// The same for a whole line, which is how every log record leaves: through
    /// the lock, and translated on the way.
    #[test]
    fn a_line_written_through_the_scheme_ends_with_the_newline_it_translated() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, u32::from(READY));
        // SAFETY: as above.
        let uart = unsafe { Uart16550Mmio::<u32>::new(base) };
        let before = reentrant_console_writes();
        uart.write_str("hi\n").unwrap();
        assert_eq!(
            reg::<u32>(base, DATA),
            u32::from(b'\n'),
            "the line did not end with the newline the driver translates to"
        );
        assert_eq!(
            reentrant_console_writes(),
            before,
            "an ordinary line took the crash escape"
        );
    }

    /// Receiving is the half the escape declines, so the scheme's own path is
    /// the only one there is: with nothing waiting it answers nothing, and with
    /// a byte waiting it answers that byte.
    #[test]
    fn a_byte_waiting_at_the_port_is_received_through_the_scheme() {
        let base = a_block(0u32);
        set_reg(base, LINE_STS, u32::from(READY));
        // SAFETY: as above.
        let uart = unsafe { Uart16550Mmio::<u32>::new(base) };
        assert_eq!(
            uart.try_recv().unwrap(),
            None,
            "a byte was reported with the input bit clear"
        );
        set_reg(base, LINE_STS, u32::from(READY | HAS_INPUT));
        set_reg(base, DATA, u32::from(b'q'));
        assert_eq!(
            uart.try_recv().unwrap(),
            Some(b'q'),
            "the waiting byte did not come back"
        );
    }
}
