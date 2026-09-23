pub use self::port_packet::*;
use crate::object::*;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::sync::Arc;
use bitflags::bitflags;
use futures::channel::oneshot::Receiver;
use kernel_hal::sync::Mutex;

#[path = "port_packet.rs"]
mod port_packet;

const MAX_ALLOCATED_PACKET_COUNT: usize = 16 * 1024;
const MAX_ALLOCATED_PACKET_COUNT_PER_PORT: usize = MAX_ALLOCATED_PACKET_COUNT / 8;

/// Signaling and mailbox primitive
///
/// ## SYNOPSIS
///
/// Ports allow threads to wait for packets to be delivered from various
/// events. These events include explicit queueing on the port,
/// asynchronous waits on other handles bound to the port, and
/// asynchronous message delivery from IPC transports.
pub struct Port {
    base: KObjectBase,
    options: PortOptions,
    inner: Mutex<PortInner>,
}

impl_kobject!(Port);

#[derive(Default, Debug)]
struct PortInner {
    queue: VecDeque<QueuedPacket>,
    observers: BTreeMap<u64, Observer>,
    next_observer: u64,
    interrupt_queue: VecDeque<PortInterruptPacket>,
    interrupt_grave: BTreeSet<u64>,
    interrupt_pid: u64,
}

#[derive(Debug)]
struct QueuedPacket {
    packet: PortPacket,
    observer: Option<u64>,
}

#[derive(Debug)]
struct Observer {
    source: (KoID, HandleValue),
    key: u64,
    signals: Signal,
    options: WaitAsyncOptions,
    cancel: Option<Receiver<()>>,
    /// Whether an asserted signal may now queue this observer's packet.
    ///
    /// `WaitAsyncOptions::EDGE` asks for the packet on an inactive→active
    /// transition *after* the call, so an observer registered while its
    /// signals are already asserted starts unarmed and arms the first time
    /// they are seen inactive. Everything else is armed from the start.
    ///
    /// It lives here rather than as a "first call" latch in the callback.
    /// A latch only suppresses the very first evaluation, so the next
    /// change on the object — including a change to a signal this observer
    /// does not even watch — queued a packet for a signal that had never
    /// gone inactive. Keeping the state beside the observer also makes the
    /// callback a pure function of it and the signal it is handed, so
    /// evaluating it twice for one signal cannot invent an edge.
    edge_armed: bool,
}

impl Observer {
    fn handle_closed(&mut self) -> bool {
        self.cancel
            .as_mut()
            .is_some_and(|cancel| !matches!(cancel.try_recv(), Ok(None)))
    }
}

#[derive(Debug)]
struct PortInterruptPacket {
    timestamp: i64,
    key: u64,
    pid: u64,
}

impl From<PortInterruptPacket> for PacketInterrupt {
    fn from(packet: PortInterruptPacket) -> Self {
        PacketInterrupt {
            timestamp: packet.timestamp,
            _reserved0: 0,
            _reserved1: 0,
            _reserved2: 0,
        }
    }
}

impl Port {
    /// Create a new `Port`.
    pub fn new(options: u32) -> ZxResult<Arc<Self>> {
        Ok(Arc::new(Port {
            base: KObjectBase::default(),
            options: PortOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)?,
            inner: Mutex::default(),
        }))
    }

    /// Push a `packet` into the port.
    pub fn push(&self, packet: impl Into<PortPacket>) {
        let mut inner = self.inner.lock();
        inner.queue.push_back(QueuedPacket {
            packet: packet.into(),
            observer: None,
        });
        self.base.signal_set(Signal::READABLE);
    }

    /// Push a `User` type `packet` into the port.
    pub fn push_user(&self, packet: impl Into<PortPacket>) -> ZxResult<()> {
        let mut packet = packet.into();
        // Zircon's `PortDispatcher::QueueUser` does the same: whatever type the
        // caller wrote, a queued packet is a user packet.
        packet.type_ = PacketType::User as u32;
        let mut inner = self.inner.lock();
        if inner.queue.len() >= MAX_ALLOCATED_PACKET_COUNT_PER_PORT {
            return Err(ZxError::SHOULD_WAIT);
        }
        inner.queue.push_back(QueuedPacket {
            packet,
            observer: None,
        });
        self.base.signal_set(Signal::READABLE);
        Ok(())
    }

    /// Register a one-shot signal wait, identified by its source handle and key.
    pub fn wait_async(
        self: &Arc<Self>,
        object: &Arc<dyn KernelObject>,
        source: (KoID, HandleValue),
        key: u64,
        signals: Signal,
        options: WaitAsyncOptions,
        cancel: Option<Receiver<()>>,
    ) {
        let id = {
            let mut inner = self.inner.lock();
            inner.next_observer += 1;
            let id = inner.next_observer;
            inner.observers.insert(
                id,
                Observer {
                    source,
                    key,
                    signals,
                    edge_armed: !options.contains(WaitAsyncOptions::EDGE),
                    options,
                    cancel,
                },
            );
            id
        };
        let port = Arc::downgrade(self);
        object.add_signal_callback(Box::new(move |observed| {
            let Some(port) = port.upgrade() else {
                return true;
            };
            port.signal_observer(id, observed)
        }));
    }

    fn signal_observer(&self, id: u64, observed: Signal) -> bool {
        let mut inner = self.inner.lock();
        let Some(observer) = inner.observers.get_mut(&id) else {
            return true;
        };
        if observer.handle_closed() {
            inner.observers.remove(&id);
            return true;
        }
        if !observer.edge_armed {
            // Edge-triggered, and the signals were asserted when the wait
            // was registered: nothing queues until they have gone inactive.
            observer.edge_armed = !observed.intersects(observer.signals);
            return false;
        }
        if !observed.intersects(observer.signals) {
            return false;
        }
        let timestamp = if observer
            .options
            .intersects(WaitAsyncOptions::TIMESTAMP | WaitAsyncOptions::BOOT_TIMESTAMP)
        {
            kernel_hal::timer::timer_now().as_nanos() as u64
        } else {
            0
        };
        let packet = PortPacketRepr {
            key: observer.key,
            status: ZxError::OK as i32,
            data: PayloadRepr::Signal(PacketSignal {
                trigger: observer.signals,
                observed,
                count: 1,
                timestamp,
                _reserved1: 0,
            }),
        }
        .into();
        inner.queue.push_back(QueuedPacket {
            packet,
            observer: Some(id),
        });
        self.base.signal_set(Signal::READABLE);
        true
    }

    /// Cancel pending waits and queued packets matching a key and optional source.
    pub fn cancel(&self, source: Option<(KoID, HandleValue)>, key: u64) -> ZxResult {
        let mut inner = self.inner.lock();
        let mut found = false;
        inner.observers.retain(|_, observer| {
            let matched = observer.key == key && source.is_none_or(|s| s == observer.source);
            found |= matched;
            !matched
        });
        let PortInner {
            queue, observers, ..
        } = &mut *inner;
        queue.retain(|queued| {
            let remove = match queued.observer {
                Some(id) => !observers.contains_key(&id),
                None => source.is_none() && queued.packet.key == key,
            };
            found |= remove;
            !remove
        });
        if inner.queue.is_empty() && inner.interrupt_queue.is_empty() {
            self.base.signal_clear(Signal::READABLE);
        }
        if found {
            Ok(())
        } else {
            Err(ZxError::NOT_FOUND)
        }
    }

    /// Push an `InterruptPacket` into the port.
    pub(crate) fn push_interrupt(&self, timestamp: i64, key: u64) -> u64 {
        let mut inner = self.inner.lock();
        inner.interrupt_pid += 1;
        let pid = inner.interrupt_pid;
        inner.interrupt_queue.push_back(PortInterruptPacket {
            timestamp,
            key,
            pid,
        });
        inner.interrupt_grave.insert(pid);
        self.base.signal_set(Signal::READABLE);
        pid
    }

    /// Remove an `InterruptPacket` from the port.
    /// Return whether the packet is in the port
    pub(crate) fn remove_interrupt(&self, pid: u64) -> bool {
        let mut inner = self.inner.lock();
        inner.interrupt_grave.remove(&pid)
    }

    /// Asynchronous wait until at least one packet is available, then take out the earliest
    /// (in FIFO order) available packet.
    pub async fn wait(self: &Arc<Self>) -> PortPacket {
        let object = self.clone() as Arc<dyn KernelObject>;
        loop {
            object.wait_signal(Signal::READABLE).await;
            let mut inner = self.inner.lock();
            if self.can_bind_to_interrupt() {
                while let Some(packet) = inner.interrupt_queue.pop_front() {
                    let in_queue = inner.interrupt_grave.remove(&packet.pid);
                    if inner.queue.is_empty() && inner.interrupt_queue.is_empty() {
                        self.base.signal_clear(Signal::READABLE);
                    }
                    if !in_queue {
                        continue;
                    }
                    return PortPacketRepr {
                        key: packet.key,
                        status: ZxError::OK as i32,
                        data: PayloadRepr::Interrupt(packet.into()),
                    }
                    .into();
                }
            }
            while let Some(queued) = inner.queue.pop_front() {
                if inner.queue.is_empty()
                    && (inner.interrupt_queue.is_empty() || !self.can_bind_to_interrupt())
                {
                    self.base.signal_clear(Signal::READABLE);
                }
                if let Some(id) = queued.observer {
                    let Some(mut observer) = inner.observers.remove(&id) else {
                        continue;
                    };
                    if observer.handle_closed() {
                        continue;
                    }
                }
                return queued.packet;
            }
        }
    }

    /// Get the number of packets in queue.
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.inner.lock().queue.len()
    }

    /// Check whether the port can be bound to an interrupt.
    pub fn can_bind_to_interrupt(&self) -> bool {
        self.options.contains(PortOptions::BIND_TO_INTERUPT)
    }
}

bitflags! {
    /// Options for one-shot asynchronous signal waits.
    pub struct WaitAsyncOptions: u32 {
        /// Include a monotonic timestamp in the signal packet.
        const TIMESTAMP = 1;
        /// Ignore the signal state at registration.
        const EDGE = 2;
        /// Include a boot timeline timestamp in the signal packet.
        const BOOT_TIMESTAMP = 4;
    }

    /// If you need this port to be bound to an interrupt, pass **BIND_TO_INTERRUPT** to *options*,
    /// otherwise it should be **0**.
    pub struct PortOptions: u32 {
        #[allow(clippy::identity_op)]
        /// Allow this port to be bound to an interrupt.
        const BIND_TO_INTERUPT         = 1 << 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// `ZX_WAIT_ASYNC_EDGE` asks for the packet on an inactive→active
    /// transition *after* the call, so a signal that was already asserted
    /// when the wait was registered has to go away before it can arrive.
    #[test]
    fn an_edge_wait_needs_its_signal_to_go_inactive_first() {
        use futures::FutureExt;
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        object.signal_set(Signal::READABLE);
        port.wait_async(
            &object,
            (1, 4),
            7,
            Signal::READABLE,
            WaitAsyncOptions::EDGE,
            None,
        );
        assert!(port.wait().now_or_never().is_none());
        // A change to a signal this wait does not even watch is not an edge
        // on the one it does.
        object.signal_set(Signal::USER_SIGNAL_0);
        assert!(port.wait().now_or_never().is_none());
        // Nor is the signal going away.
        object.signal_clear(Signal::READABLE);
        assert!(port.wait().now_or_never().is_none());
        // This is the transition it asked for.
        object.signal_set(Signal::READABLE);
        assert_eq!(port.wait().now_or_never().unwrap().key, 7);
    }

    #[test]
    fn an_edge_wait_on_an_idle_signal_fires_on_the_first_assertion() {
        use futures::FutureExt;
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        port.wait_async(
            &object,
            (1, 4),
            7,
            Signal::READABLE,
            WaitAsyncOptions::EDGE,
            None,
        );
        assert!(port.wait().now_or_never().is_none());
        object.signal_set(Signal::READABLE);
        assert_eq!(port.wait().now_or_never().unwrap().key, 7);
    }

    #[test]
    fn a_level_wait_fires_for_a_signal_that_is_already_asserted() {
        use futures::FutureExt;
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        object.signal_set(Signal::READABLE);
        port.wait_async(
            &object,
            (1, 4),
            7,
            Signal::READABLE,
            WaitAsyncOptions::empty(),
            None,
        );
        assert_eq!(port.wait().now_or_never().unwrap().key, 7);
    }

    /// Asking for a timestamp is the only reason `signal_observer` reads the
    /// clock; without it the packet carries a zero, which is what a reader
    /// checks to know whether the field means anything.
    #[test]
    fn only_a_timestamped_wait_stamps_its_packet() {
        use futures::FutureExt;
        for (options, stamped) in [
            (WaitAsyncOptions::empty(), false),
            (WaitAsyncOptions::TIMESTAMP, true),
            (WaitAsyncOptions::BOOT_TIMESTAMP, true),
        ] {
            let before = kernel_hal::timer::timer_now().as_nanos() as u64;
            let port = Port::new(0).unwrap();
            let object = DummyObject::new() as Arc<dyn KernelObject>;
            port.wait_async(&object, (1, 4), 7, Signal::READABLE, options, None);
            object.signal_set(Signal::READABLE);
            let packet = port.wait().now_or_never().unwrap().decode().unwrap();
            let PayloadRepr::Signal(signal) = packet.data else {
                panic!("a signal wait answers with a signal packet");
            };
            if stamped {
                assert!(signal.timestamp >= before, "options: {:?}", options);
            } else {
                assert_eq!(signal.timestamp, 0, "options: {:?}", options);
            }
        }
    }

    /// The cancel token is the handle the wait was registered against. When
    /// it closes the wait is off, and the observer goes with it there and
    /// then: a port that waited until the next `port.wait()` to notice would
    /// answer `cancel` as if the wait were still live.
    #[test]
    fn a_wait_whose_handle_closed_queues_nothing_and_is_forgotten() {
        use futures::FutureExt;
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        let (sender, receiver) = futures::channel::oneshot::channel();
        port.wait_async(
            &object,
            (1, 4),
            7,
            Signal::READABLE,
            WaitAsyncOptions::empty(),
            Some(receiver),
        );
        drop(sender);
        object.signal_set(Signal::READABLE);
        assert_eq!(port.cancel(None, 7), Err(ZxError::NOT_FOUND));
        assert!(port.wait().now_or_never().is_none());
    }

    /// And when the handle closes after the packet is already queued, the
    /// packet is dropped on the way out rather than reported to a wait
    /// nobody is on any more.
    #[test]
    fn a_queued_packet_whose_handle_closed_is_dropped_on_the_way_out() {
        use futures::FutureExt;
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        let (sender, receiver) = futures::channel::oneshot::channel();
        port.wait_async(
            &object,
            (1, 4),
            7,
            Signal::READABLE,
            WaitAsyncOptions::empty(),
            Some(receiver),
        );
        object.signal_set(Signal::READABLE);
        drop(sender);
        assert!(port.wait().now_or_never().is_none());
    }

    #[test]
    fn cancellation_distinguishes_source_handles_and_queued_packets() {
        use futures::FutureExt;
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        for source in [(1, 4), (1, 8)] {
            port.wait_async(
                &object,
                source,
                7,
                Signal::READABLE,
                WaitAsyncOptions::empty(),
                None,
            );
        }
        port.cancel(Some((1, 4)), 7).unwrap();
        object.signal_set(Signal::READABLE);
        assert_eq!(port.cancel(Some((1, 4)), 7), Err(ZxError::NOT_FOUND));
        assert_eq!(port.wait().now_or_never().unwrap().key, 7);
        assert_eq!(port.cancel(None, 7), Err(ZxError::NOT_FOUND));
        port.wait_async(
            &object,
            (1, 4),
            9,
            Signal::READABLE,
            WaitAsyncOptions::empty(),
            None,
        );
        port.cancel(None, 9).unwrap();
        assert!(port.wait().now_or_never().is_none());
    }

    #[test]
    fn observers_do_not_keep_ports_alive() {
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        let port = Port::new(0).unwrap();
        let weak = Arc::downgrade(&port);
        port.wait_async(
            &object,
            (1, 4),
            7,
            Signal::READABLE,
            WaitAsyncOptions::empty(),
            None,
        );
        drop(port);
        assert!(weak.upgrade().is_none());
        object.signal_set(Signal::READABLE);
    }

    #[test]
    fn new() {
        assert!(Port::new(0).is_ok());
        assert!(Port::new(1).is_ok());
        assert_eq!(Port::new(2).unwrap_err(), ZxError::INVALID_ARGS);
    }

    #[async_std::test]
    async fn wait() {
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        object.send_signal_to_port_async(Signal::READABLE, &port, 1);

        let packet_repr2 = PortPacketRepr {
            key: 2,
            status: ZxError::OK as i32,
            data: PayloadRepr::Signal(PacketSignal {
                trigger: Signal::WRITABLE,
                observed: Signal::WRITABLE,
                count: 1,
                timestamp: 0,
                _reserved1: 0,
            }),
        };
        async_std::task::spawn({
            let port = port.clone();
            let object = object.clone();
            let packet2 = packet_repr2.clone();
            async move {
                // Assert an irrelevant signal to test the `false` branch of the callback for `READABLE`.
                object.signal_set(Signal::USER_SIGNAL_0);
                object.signal_clear(Signal::USER_SIGNAL_0);
                object.signal_set(Signal::READABLE);
                async_std::task::sleep(Duration::from_millis(1)).await;
                port.push(packet2);
            }
        });

        let packet = port.wait().await;
        let packet_repr = PortPacketRepr {
            key: 1,
            status: ZxError::OK as i32,
            data: PayloadRepr::Signal(PacketSignal {
                trigger: Signal::READABLE,
                observed: Signal::READABLE,
                count: 1,
                timestamp: 0,
                _reserved1: 0,
            }),
        };
        assert_eq!(packet.decode().unwrap(), packet_repr);

        let packet = port.wait().await;
        assert_eq!(packet.decode().unwrap(), packet_repr2);

        // Test asserting signal before `send_signal_to_port_async`.
        let port = Port::new(0).unwrap();
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        object.signal_set(Signal::READABLE);
        object.send_signal_to_port_async(Signal::READABLE, &port, 1);
        let packet = port.wait().await;
        assert_eq!(packet.decode().unwrap(), packet_repr);
    }

    /// `push_user` gets the struct `sys_port_queue` read out of the caller's
    /// memory, so its type is whatever the process wrote. Zircon's
    /// `PortDispatcher::QueueUser` overwrites it and leaves the rest alone.
    #[async_std::test]
    async fn a_queued_packet_is_a_user_packet_whatever_the_caller_wrote() {
        #[allow(unsafe_code)]
        fn packet_from_bytes(type_: u32, status: i32) -> PortPacket {
            let mut bytes = [0u8; 48];
            bytes[0..8].copy_from_slice(&9u64.to_le_bytes());
            bytes[8..12].copy_from_slice(&type_.to_le_bytes());
            bytes[12..16].copy_from_slice(&status.to_le_bytes());
            bytes[16..48].copy_from_slice(&[0x5a; 32]);
            unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const PortPacket) }
        }

        // 9 is `ZX_PKT_TYPE_PAGE_REQUEST`, which has no payload and used to
        // panic the kernel on the way in; 8 is not a type at all.
        for type_ in [0u32, 6, 8, 9, u32::MAX] {
            let port = Port::new(0).unwrap();
            port.push_user(packet_from_bytes(type_, -77)).unwrap();
            let queued = port.wait().await;
            assert_eq!(queued.type_, PacketType::User as u32);
            assert_eq!(queued.status, -77, "the status is carried through");
            assert_eq!(queued.decode().unwrap().data, PayloadRepr::User([0x5a; 32]));
        }
    }
}
