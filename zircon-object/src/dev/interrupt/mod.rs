use {
    self::event_interrupt::*,
    self::pci_interrupt::*,
    self::virtual_interrupt::*,
    crate::dev::pci::IPciNode,
    crate::object::*,
    crate::signal::*,
    alloc::{boxed::Box, sync::Arc},
    bitflags::bitflags,
    kernel_hal::sync::Mutex,
};

mod event_interrupt;
mod pci_interrupt;
mod virtual_interrupt;

trait InterruptTrait: Sync + Send {
    /// Mask the interrupt.
    fn mask(&self);
    /// Unmask the interrupt.
    fn unmask(&self);
    /// Register the interrupt to the given handler.
    fn register_handler(&self, handler: Box<dyn Fn() + Send + Sync>) -> ZxResult;
    /// Unregister the interrupt to the given handler.
    fn unregister_handler(&self) -> ZxResult;
}

impl_kobject!(Interrupt);

/// Interrupts - Usermode I/O interrupt delivery.
///
/// ## SYNOPSIS
///
/// Interrupt objects allow userspace to create, signal, and wait on hardware interrupts.
pub struct Interrupt {
    base: KObjectBase,
    has_vcpu: bool,
    flags: InterruptFlags,
    inner: Mutex<InterruptInner>,
    trait_: Box<dyn InterruptTrait>,
}

#[derive(Default)]
struct InterruptInner {
    state: InterruptState,
    port: Option<Arc<Port>>,
    key: u64,
    timestamp: i64,
    defer_unmask: bool,
    packet_id: u64,
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        // Closing a handle has nobody to report to, and `destroy` has two
        // ordinary ways to answer with an error: `NOT_FOUND`, when the
        // interrupt's packet has already been taken out of the port -- which is
        // what happens every single time the program actually received the
        // interrupt it was waiting for -- and whatever `unregister_handler`
        // answers. An `unwrap` here made closing such a handle a kernel panic,
        // and `zx_interrupt_create(ZX_INTERRUPT_VIRTUAL)` asks for no resource
        // at all, so any process could reach it.
        if let Err(err) = self.destroy() {
            debug!("interrupt dropped: destroy answered {:?}", err);
        }
    }
}

impl Interrupt {
    /// Create a new virtual interrupt.
    pub fn new_virtual() -> Arc<Self> {
        Arc::new(Interrupt {
            base: KObjectBase::new(),
            has_vcpu: false,
            flags: InterruptFlags::VIRTUAL,
            inner: Default::default(),
            trait_: VirtualInterrupt::new(),
        })
    }

    /// Create a new physical interrupt.
    pub fn new_physical(vector: usize, options: InterruptOptions) -> ZxResult<Arc<Self>> {
        let mode = options.to_mode();
        if mode != InterruptOptions::MODE_DEFAULT && mode != InterruptOptions::MODE_EDGE_HIGH {
            // Only the edge-high line is wired up here. The rest are real modes
            // a driver may ask for -- level-high is what legacy PCI INTx uses --
            // so this is a supported syscall answering about an unsupported
            // option, not a place for `unimplemented!()`.
            warn!("interrupt mode {:?} is not supported", mode);
            return Err(ZxError::NOT_SUPPORTED);
        }
        if options.contains(InterruptOptions::REMAP_IRQ) {
            warn!("Skip Interrupt.Remap");
        }
        let interrupt = Arc::new(Interrupt {
            base: KObjectBase::new(),
            has_vcpu: false,
            flags: InterruptFlags::empty(),
            inner: Default::default(),
            trait_: EventInterrupt::new(vector),
        });
        let interrupt_clone = interrupt.clone();
        interrupt
            .trait_
            .register_handler(Box::new(move || interrupt_clone.handle_interrupt()))?;
        interrupt.trait_.unmask();
        Ok(interrupt)
    }

    /// Create a new PCI interrupt.
    pub fn new_pci(device: Arc<dyn IPciNode>, vector: u32, maskable: bool) -> ZxResult<Arc<Self>> {
        let interrupt = Arc::new(Interrupt {
            base: KObjectBase::new(),
            has_vcpu: false,
            flags: InterruptFlags::UNMASK_PREWAIT_UNLOCKED,
            inner: Default::default(),
            trait_: PciInterrupt::new(device, vector, maskable),
        });
        let interrupt_clone = interrupt.clone();
        interrupt
            .trait_
            .register_handler(Box::new(move || interrupt_clone.handle_interrupt()))?;
        interrupt.trait_.unmask();
        Ok(interrupt)
    }

    /// Bind the interrupt object to a port.
    pub fn bind(&self, port: &Arc<Port>, key: u64) -> ZxResult {
        let mut inner = self.inner.lock();
        match inner.state {
            InterruptState::Destroy => return Err(ZxError::CANCELED),
            InterruptState::Waiting => return Err(ZxError::BAD_STATE),
            _ => (),
        }
        if inner.port.is_some() || self.has_vcpu {
            return Err(ZxError::ALREADY_BOUND);
        }
        if self
            .flags
            .contains(InterruptFlags::UNMASK_PREWAIT_UNLOCKED | InterruptFlags::MASK_POSTWAIT)
        {
            return Err(ZxError::INVALID_ARGS);
        }
        inner.port = Some(port.clone());
        inner.key = key;
        if inner.state == InterruptState::Triggered {
            inner.packet_id = port.as_ref().push_interrupt(inner.timestamp, inner.key);
            inner.state = InterruptState::NeedAck;
        }
        Ok(())
    }

    /// Unbind the interrupt object from a port.
    ///
    /// Unbinding the port removes previously queued packets to the port.
    pub fn unbind(&self, port: &Arc<Port>) -> ZxResult {
        let mut inner = self.inner.lock();
        if inner.port.is_none() || inner.port.as_ref().unwrap().id() != port.id() {
            return Err(ZxError::NOT_FOUND);
        }
        if inner.state == InterruptState::Destroy {
            return Err(ZxError::CANCELED);
        }
        port.remove_interrupt(inner.packet_id);
        inner.port = None;
        inner.key = 0;
        Ok(())
    }

    /// Triggers a virtual interrupt object.
    pub fn trigger(&self, timestamp: i64) -> ZxResult {
        if !self.flags.contains(InterruptFlags::VIRTUAL) {
            return Err(ZxError::BAD_STATE);
        }
        let mut inner = self.inner.lock();
        if inner.timestamp == 0 {
            inner.timestamp = timestamp;
        }
        if inner.state == InterruptState::Destroy {
            return Err(ZxError::CANCELED);
        }
        if inner.state == InterruptState::NeedAck && inner.port.is_some() {
            return Ok(());
        }
        if let Some(port) = &inner.port {
            // TODO: use a function to send the package
            inner.packet_id = port.push_interrupt(timestamp, inner.key);
            if self.flags.contains(InterruptFlags::MASK_POSTWAIT) {
                self.trait_.mask();
            }
            inner.timestamp = 0;
            inner.state = InterruptState::NeedAck;
        } else {
            inner.state = InterruptState::Triggered;
            self.base.signal_set(Signal::INTERRUPT_SIGNAL);
        }
        Ok(())
    }

    /// Acknowledge the interrupt and re-arm it.
    pub fn ack(&self) -> ZxResult {
        let mut inner = self.inner.lock();
        if inner.port.is_none() {
            return Err(ZxError::BAD_STATE);
        }
        if inner.state == InterruptState::Destroy {
            return Err(ZxError::CANCELED);
        }
        if inner.state == InterruptState::NeedAck {
            if self.flags.contains(InterruptFlags::UNMASK_PREWAIT) {
                self.trait_.unmask();
            } else if self.flags.contains(InterruptFlags::UNMASK_PREWAIT_UNLOCKED) {
                inner.defer_unmask = true;
            }
            if inner.timestamp > 0 {
                // TODO: use a function to send the package
                inner.packet_id = inner
                    .port
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .push_interrupt(inner.timestamp, inner.key);
                if self.flags.contains(InterruptFlags::MASK_POSTWAIT) {
                    self.trait_.mask();
                }
                inner.timestamp = 0;
            } else {
                inner.state = InterruptState::Idle;
            }
        }
        if inner.defer_unmask {
            self.trait_.unmask();
        }
        Ok(())
    }

    /// Destroy the interrupt.
    pub fn destroy(&self) -> ZxResult {
        self.trait_.mask();
        self.trait_.unregister_handler()?;
        let mut inner = self.inner.lock();
        if let Some(port) = &inner.port {
            let in_queue = port.remove_interrupt(inner.packet_id);
            match inner.state {
                InterruptState::NeedAck => {
                    inner.state = InterruptState::Destroy;
                    if !in_queue {
                        Err(ZxError::NOT_FOUND)
                    } else {
                        Ok(())
                    }
                }
                InterruptState::Idle => {
                    inner.state = InterruptState::Destroy;
                    Ok(())
                }
                _ => Ok(()),
            }
        } else {
            inner.state = InterruptState::Destroy;
            self.base.signal_set(Signal::INTERRUPT_SIGNAL);
            Ok(())
        }
    }

    /// Wait until the interrupt is triggered.
    pub async fn wait(self: &Arc<Self>) -> ZxResult<i64> {
        let mut defer_unmask = false;
        let object = self.clone() as Arc<dyn KernelObject>;
        loop {
            {
                let mut inner = self.inner.lock();
                if inner.port.is_some() || self.has_vcpu {
                    return Err(ZxError::BAD_STATE);
                }
                match inner.state {
                    InterruptState::Destroy => return Err(ZxError::CANCELED),
                    InterruptState::Triggered => {
                        inner.state = InterruptState::NeedAck;
                        let timestamp = inner.timestamp;
                        inner.timestamp = 0;
                        self.base.signal_clear(Signal::INTERRUPT_SIGNAL);
                        return Ok(timestamp);
                    }
                    InterruptState::NeedAck => {
                        if self.flags.contains(InterruptFlags::UNMASK_PREWAIT) {
                            self.trait_.unmask();
                        } else if self.flags.contains(InterruptFlags::UNMASK_PREWAIT_UNLOCKED) {
                            defer_unmask = true;
                        }
                    }
                    InterruptState::Idle => (),
                    _ => return Err(ZxError::BAD_STATE),
                }
                inner.state = InterruptState::Waiting;
            }
            if defer_unmask {
                self.trait_.unmask();
            }
            object.wait_signal(Signal::INTERRUPT_SIGNAL).await;
        }
    }

    fn handle_interrupt(&self) {
        let mut inner = self.inner.lock();
        if self.flags.contains(InterruptFlags::MASK_POSTWAIT) {
            self.trait_.mask();
        }
        if inner.timestamp == 0 {
            // Not sure ZX_CLOCK_MONOTONIC or ZX_CLOCK_UTC
            inner.timestamp = kernel_hal::timer::timer_now().as_nanos() as i64;
        }
        match &inner.port {
            Some(port) => {
                if inner.state != InterruptState::NeedAck {
                    // TODO: use a function to send the package
                    inner.packet_id = port.as_ref().push_interrupt(inner.timestamp, inner.key);
                    if self.flags.contains(InterruptFlags::MASK_POSTWAIT) {
                        self.trait_.mask();
                    }
                    inner.timestamp = 0;

                    inner.state = InterruptState::NeedAck;
                }
            }
            None => {
                self.base.signal_set(Signal::INTERRUPT_SIGNAL);
                inner.state = InterruptState::Triggered;
            }
        }
    }
}

#[derive(PartialEq, Debug, Default)]
enum InterruptState {
    Waiting = 0,
    Destroy = 1,
    Triggered = 2,
    NeedAck = 3,
    #[default]
    Idle = 4,
}

bitflags! {
    /// Bits for Interrupt.flags.
    pub struct InterruptFlags: u32 {
        #[allow(clippy::identity_op)]
        /// The interrupt is virtual.
        const VIRTUAL                  = 1 << 0;
        /// The interrupt should be unmasked before waiting on the event.
        const UNMASK_PREWAIT           = 1 << 1;
        /// The same as **INTERRUPT_UNMASK_PREWAIT** except release the dispatcher
        /// spinlock before waiting.
        const UNMASK_PREWAIT_UNLOCKED  = 1 << 2;
        /// The interrupt should be masked following waiting.
        const MASK_POSTWAIT            = 1 << 4;
    }
}

bitflags! {
    /// Interrupt bind flags.
    pub struct InterruptOptions: u32 {
        #[allow(clippy::identity_op)]
        /// Remap interrupt request(IRQ).
        const REMAP_IRQ = 0x1;
        /// Default mode.
        const MODE_DEFAULT = 0 << 1;
        /// Falling edge triggered.
        const MODE_EDGE_LOW = 1 << 1;
        /// Rising edge triggered.
        const MODE_EDGE_HIGH = 2 << 1;
        /// Low level triggered.
        const MODE_LEVEL_LOW = 3 << 1;
        /// High level triggered.
        const MODE_LEVEL_HIGH = 4 << 1;
        /// Falling/rising edge triggered.
        const MODE_EDGE_BOTH = 5 << 1;
        /// Virtual interrupt.
        const VIRTUAL = 0x10;
    }
}

impl InterruptOptions {
    /// Extract the mode bits.
    pub fn to_mode(self) -> Self {
        InterruptOptions::from_bits_truncate(0xe) & self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    #[async_std::test]
    async fn bind() {
        let interrupt = Interrupt::new_virtual();
        let port = Port::new(1).unwrap();
        assert_eq!(interrupt.unbind(&port).unwrap_err(), ZxError::NOT_FOUND);
        assert!(interrupt.bind(&port, 1).is_ok());

        assert!(interrupt.destroy().is_ok());
        assert_eq!(interrupt.unbind(&port).unwrap_err(), ZxError::CANCELED);

        let interrupt = Interrupt::new_virtual();
        assert_eq!(interrupt.unbind(&port).unwrap_err(), ZxError::NOT_FOUND);
        assert!(interrupt.bind(&port, 1).is_ok());

        assert!(interrupt.trigger(1234).is_ok());
        let packet = port.wait().await;
        assert_eq!(
            packet.decode().unwrap(),
            PortPacketRepr {
                key: 1,
                status: ZxError::OK as i32,
                data: PayloadRepr::Interrupt(PacketInterrupt {
                    timestamp: 1234,
                    _reserved0: 0,
                    _reserved1: 0,
                    _reserved2: 0,
                }),
            }
        );
        assert!(interrupt.unbind(&port).is_ok());
    }

    /// The key of the sentinel interrupt `drain` queues behind everything else.
    const SENTINEL: u64 = u64::MAX;

    /// Bind a fresh virtual interrupt to a fresh port and hand both back.
    fn bound() -> (Arc<Interrupt>, Arc<Port>) {
        let interrupt = Interrupt::new_virtual();
        let port = Port::new(1).unwrap();
        interrupt.bind(&port, 42).unwrap();
        (interrupt, port)
    }

    /// Every interrupt packet waiting on `port` right now, as `(key,
    /// timestamp)` in the order they come out.
    ///
    /// The sentinel queued behind them is what makes this terminate: a `wait`
    /// on an empty port blocks for ever, so a test whose packet was never
    /// queued would hang the whole suite instead of failing with a name. The
    /// timeout is the second half of the same guard, for when the sentinel
    /// itself does not come back recognisable.
    async fn drain(port: &Arc<Port>) -> Vec<(u64, i64)> {
        let sentinel = Interrupt::new_virtual();
        sentinel.bind(port, SENTINEL).unwrap();
        sentinel.trigger(1).unwrap();
        let mut out = Vec::new();
        loop {
            let packet = answers("the port, with a sentinel queued on it", port.wait()).await;
            let repr = packet.decode().unwrap();
            let timestamp = match repr.data {
                PayloadRepr::Interrupt(i) => i.timestamp,
                other => panic!("not an interrupt packet: {:?}", other),
            };
            if repr.key == SENTINEL {
                return out;
            }
            out.push((repr.key, timestamp));
        }
    }

    /// `f`, or a panic if it is still not finished after a second. The waits
    /// under test either answer at once or never, so a second is generous and
    /// a timeout is a failure, not a slow machine.
    async fn answers<T>(what: &str, f: impl core::future::Future<Output = T>) -> T {
        match async_std::future::timeout(core::time::Duration::from_secs(1), f).await {
            Ok(value) => value,
            Err(_) => panic!("{} never answered", what),
        }
    }

    #[async_std::test]
    /// The ordinary life of an interrupt: bind it to a port, trigger it, read
    /// the packet, close the handle. Reading the packet takes it out of the
    /// port, so the `destroy` that runs on drop answers `NOT_FOUND` -- and
    /// `unwrap`ping that answer was a kernel panic on the last step, reachable
    /// from any process, since a virtual interrupt asks for no resource.
    async fn closing_a_handle_after_the_packet_arrived_is_not_a_panic() {
        let (interrupt, port) = bound();
        interrupt.trigger(1234).unwrap();
        assert_eq!(drain(&port).await, vec![(42, 1234)]);
        drop(interrupt);
    }

    #[async_std::test]
    /// `zx_interrupt_destroy` still reports it, though: the error is how the
    /// caller learns the packet was already consumed. Only closing the handle
    /// has nobody to tell.
    async fn destroy_still_reports_that_the_packet_was_already_taken() {
        let (interrupt, port) = bound();
        interrupt.trigger(1234).unwrap();
        assert_eq!(drain(&port).await, vec![(42, 1234)]);
        assert_eq!(interrupt.destroy(), Err(ZxError::NOT_FOUND));
        // And the second destroy, the one on drop, is quiet.
        drop(interrupt);
    }

    #[async_std::test]
    /// Closing a handle takes the interrupt's own packet out of the port.
    /// Otherwise the program would be handed an interrupt from an object it no
    /// longer has, and the line would stay unmasked.
    async fn closing_a_handle_takes_its_pending_packet_with_it() {
        let port = Port::new(1).unwrap();
        let going = Interrupt::new_virtual();
        let staying = Interrupt::new_virtual();
        going.bind(&port, 1).unwrap();
        staying.bind(&port, 2).unwrap();
        going.trigger(1000).unwrap();
        staying.trigger(2000).unwrap();

        drop(going);
        assert_eq!(drain(&port).await, vec![(2, 2000)]);
    }

    #[async_std::test]
    /// An interrupt never bound, and one destroyed by hand first: neither is an
    /// error on the way out either.
    async fn the_other_ways_of_closing_a_handle_are_quiet_too() {
        drop(Interrupt::new_virtual());

        let (interrupt, port) = bound();
        interrupt.destroy().unwrap();
        drop(interrupt);
        assert_eq!(drain(&port).await, vec![]);
    }

    #[test]
    /// Every interrupt mode but the one wired up answers `NOT_SUPPORTED`.
    /// Level-high is what legacy PCI INTx asks for, so this is not an exotic
    /// option, and it used to panic the kernel from `zx_interrupt_create`.
    fn a_mode_this_kernel_does_not_wire_up_is_an_error_not_a_panic() {
        for mode in [
            InterruptOptions::MODE_EDGE_LOW,
            InterruptOptions::MODE_LEVEL_LOW,
            InterruptOptions::MODE_LEVEL_HIGH,
            InterruptOptions::MODE_EDGE_BOTH,
        ] {
            assert_eq!(
                Interrupt::new_physical(33, mode).err(),
                Some(ZxError::NOT_SUPPORTED),
                "{:?}",
                mode,
            );
            // The rest of the options word does not change the answer.
            assert_eq!(
                Interrupt::new_physical(33, mode | InterruptOptions::REMAP_IRQ).err(),
                Some(ZxError::NOT_SUPPORTED),
                "{:?}",
                mode,
            );
        }
    }

    #[test]
    /// `to_mode` reads the mode bits and nothing else, whatever else the
    /// options word carries. This is what decides which of the arms above a
    /// `zx_interrupt_create` lands in.
    fn the_mode_bits_are_the_only_thing_to_mode_reads() {
        let noise = InterruptOptions::REMAP_IRQ | InterruptOptions::VIRTUAL;
        for mode in [
            InterruptOptions::MODE_DEFAULT,
            InterruptOptions::MODE_EDGE_LOW,
            InterruptOptions::MODE_EDGE_HIGH,
            InterruptOptions::MODE_LEVEL_LOW,
            InterruptOptions::MODE_LEVEL_HIGH,
            InterruptOptions::MODE_EDGE_BOTH,
        ] {
            assert_eq!((mode | noise).to_mode(), mode, "{:?}", mode);
            assert_eq!(mode.to_mode(), mode, "{:?}", mode);
        }
        // And the bits it reads are exactly the three the modes live in.
        assert_eq!(noise.to_mode(), InterruptOptions::MODE_DEFAULT);
    }

    #[async_std::test]
    /// An interrupt that fires again before the last one was acknowledged is
    /// remembered, not queued twice: nothing more arrives until the `ack`, and
    /// what arrives then is the timestamp that was waiting.
    async fn a_second_trigger_is_remembered_until_it_is_acknowledged() {
        let (interrupt, port) = bound();
        interrupt.trigger(1000).unwrap();
        assert_eq!(drain(&port).await, vec![(42, 1000)]);

        interrupt.trigger(2000).unwrap();
        interrupt.trigger(3000).unwrap();
        assert_eq!(
            drain(&port).await,
            vec![],
            "a trigger before the ack queues nothing",
        );

        interrupt.ack().unwrap();
        assert_eq!(drain(&port).await, vec![(42, 2000)]);
        // That left it needing another ack, and nothing is pending now.
        interrupt.ack().unwrap();
        assert_eq!(drain(&port).await, vec![]);
    }

    #[async_std::test]
    /// Binding is once, to one port, and only while the interrupt is alive.
    async fn binding_answers_for_every_way_of_getting_it_wrong() {
        let interrupt = Interrupt::new_virtual();
        let port = Port::new(1).unwrap();
        let other = Port::new(1).unwrap();

        assert_eq!(interrupt.unbind(&port), Err(ZxError::NOT_FOUND));
        interrupt.bind(&port, 42).unwrap();
        assert_eq!(interrupt.bind(&port, 42), Err(ZxError::ALREADY_BOUND));
        assert_eq!(interrupt.bind(&other, 42), Err(ZxError::ALREADY_BOUND));
        assert_eq!(interrupt.unbind(&other), Err(ZxError::NOT_FOUND));
        interrupt.unbind(&port).unwrap();
        // Unbound again, it can be bound somewhere else.
        interrupt.bind(&other, 7).unwrap();

        interrupt.destroy().unwrap();
        assert_eq!(interrupt.bind(&port, 42), Err(ZxError::CANCELED));
        assert_eq!(interrupt.unbind(&other), Err(ZxError::CANCELED));
    }

    #[async_std::test]
    /// A destroyed interrupt answers `CANCELED` and stops queueing packets.
    async fn a_destroyed_interrupt_stops_answering() {
        let (interrupt, port) = bound();
        interrupt.destroy().unwrap();
        assert_eq!(interrupt.trigger(1234), Err(ZxError::CANCELED));
        assert_eq!(interrupt.ack(), Err(ZxError::CANCELED));
        assert_eq!(drain(&port).await, vec![]);

        // Unbound, `wait` is the one that answers. Bound, it answers
        // `BAD_STATE` first -- the port is how a bound interrupt is read, and
        // that is true whether or not it has been destroyed.
        let alone = Interrupt::new_virtual();
        alone.destroy().unwrap();
        assert_eq!(
            answers("wait on a destroyed interrupt", alone.wait()).await,
            Err(ZxError::CANCELED),
        );
        assert_eq!(
            answers("wait on a bound interrupt", interrupt.wait()).await,
            Err(ZxError::BAD_STATE),
        );
    }

    #[async_std::test]
    /// Waiting and a port are the two ways of receiving an interrupt, and they
    /// are exclusive: a bound interrupt is read through its port.
    async fn an_interrupt_bound_to_a_port_is_not_waited_on() {
        let (interrupt, _port) = bound();
        assert_eq!(
            answers("wait on a bound interrupt", interrupt.wait()).await,
            Err(ZxError::BAD_STATE),
        );
        // Acknowledging is the other way round: it needs the port.
        let alone = Interrupt::new_virtual();
        assert_eq!(alone.ack(), Err(ZxError::BAD_STATE));
    }

    #[async_std::test]
    /// Without a port, the trigger raises the signal and `wait` hands back the
    /// timestamp it was given and clears the signal behind it.
    async fn waiting_hands_back_the_timestamp_and_clears_the_signal() {
        let interrupt = Interrupt::new_virtual();
        assert!(!interrupt.signal().contains(Signal::INTERRUPT_SIGNAL));
        interrupt.trigger(777).unwrap();
        assert!(interrupt.signal().contains(Signal::INTERRUPT_SIGNAL));
        assert_eq!(
            answers("wait after a trigger", interrupt.wait()).await,
            Ok(777),
        );
        assert!(!interrupt.signal().contains(Signal::INTERRUPT_SIGNAL));
    }

    #[async_std::test]
    /// A trigger before the port is bound is not lost: binding queues it.
    async fn an_interrupt_that_fired_before_the_bind_is_delivered_by_it() {
        let interrupt = Interrupt::new_virtual();
        interrupt.trigger(555).unwrap();
        let port = Port::new(1).unwrap();
        interrupt.bind(&port, 9).unwrap();
        assert_eq!(drain(&port).await, vec![(9, 555)]);
    }
}
