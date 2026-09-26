use core::mem::{size_of, MaybeUninit};

use super::*;
use bitflags::*;
use core::hint::spin_loop;
use log::*;
use volatile::{ReadOnly, Volatile};

/// The virtio network device is a virtual ethernet card.
///
/// It has enhanced rapidly and demonstrates clearly how support for new
/// features are added to an existing device.
/// Empty buffers are placed in one virtqueue for receiving packets, and
/// outgoing packets are enqueued into another for transmission in that order.
/// A third command queue is used to control advanced filtering features.
pub struct VirtIONet<'a> {
    header: &'static mut VirtIOHeader,
    mac: EthernetAddress,
    recv_queue: VirtQueue<'a>,
    send_queue: VirtQueue<'a>,
}

impl VirtIONet<'_> {
    /// Create a new VirtIO-Net driver.
    pub fn new(header: &'static mut VirtIOHeader) -> Result<Self> {
        header.begin_init(|features| {
            let features = Features::from_bits_truncate(features);
            info!("Device features {:?}", features);
            let supported_features = Features::MAC | Features::STATUS;
            (features & supported_features).bits()
        });
        // read configuration space
        let config = unsafe { &mut *(header.config_space() as *mut Config) };
        let mac = config.mac.read();
        debug!("Got MAC={:?}, status={:?}", mac, config.status.read());

        let queue_num = 2; // for simplicity
        let recv_queue = VirtQueue::new(header, QUEUE_RECEIVE, queue_num)?;
        let send_queue = VirtQueue::new(header, QUEUE_TRANSMIT, queue_num)?;

        header.finish_init();

        Ok(VirtIONet {
            header,
            mac,
            recv_queue,
            send_queue,
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.header.ack_interrupt()
    }

    /// Get MAC address.
    pub fn mac(&self) -> EthernetAddress {
        self.mac
    }

    /// Whether can send packet.
    pub fn can_send(&self) -> bool {
        self.send_queue.available_desc() >= 2
    }

    /// Whether can receive packet.
    pub fn can_recv(&self) -> bool {
        self.recv_queue.can_pop()
    }

    /// Receive a packet.
    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut header = MaybeUninit::<Header>::uninit();
        let header_buf = unsafe { (*header.as_mut_ptr()).as_buf_mut() };
        self.recv_queue.add(&[], &[header_buf, buf])?;
        self.header.notify(QUEUE_RECEIVE as u32);
        while !self.recv_queue.can_pop() {
            spin_loop();
        }

        let (_, len) = self.recv_queue.pop_used()?;
        // let header = unsafe { header.assume_init() };
        // `len` is the used ring's length word, and the DEVICE writes it: it is
        // not bounded by what was offered here. Below the header it used to
        // underflow the subtraction -- a panic in debug, and in release a length
        // near `usize::MAX` handed back as if it were a packet -- and above
        // `buf.len()` it promised the caller bytes that were never written.
        let len = len as usize;
        if len < size_of::<Header>() {
            warn!(
                "[virtio-net] device completed {} bytes, short of the {}-byte header",
                len,
                size_of::<Header>()
            );
            return Err(Error::IoError);
        }
        let payload = len - size_of::<Header>();
        if payload > buf.len() {
            warn!(
                "[virtio-net] device claims {} bytes of payload for a {}-byte buffer, taking {}",
                payload,
                buf.len(),
                buf.len()
            );
        }
        Ok(payload.min(buf.len()))
    }

    /// Send a packet.
    pub fn send(&mut self, buf: &[u8]) -> Result {
        let header = unsafe { MaybeUninit::<Header>::zeroed().assume_init() };
        self.send_queue.add(&[header.as_buf(), buf], &[])?;
        self.header.notify(QUEUE_TRANSMIT as u32);
        while !self.send_queue.can_pop() {
            spin_loop();
        }
        self.send_queue.pop_used()?;
        Ok(())
    }
}

bitflags! {
    struct Features: u64 {
        /// Device handles packets with partial checksum.
        /// This "checksum offload" is a common feature on modern network cards.
        const CSUM = 1 << 0;
        /// Driver handles packets with partial checksum.
        const GUEST_CSUM = 1 << 1;
        /// Control channel offloads reconfiguration support.
        const CTRL_GUEST_OFFLOADS = 1 << 2;
        /// Device maximum MTU reporting is supported.
        ///
        /// If offered by the device, device advises driver about the value of
        /// its maximum MTU. If negotiated, the driver uses mtu as the maximum
        /// MTU value.
        const MTU = 1 << 3;
        /// Device has given MAC address.
        const MAC = 1 << 5;
        /// Device handles packets with any GSO type. (legacy)
        const GSO = 1 << 6;
        /// Driver can receive TSOv4.
        const GUEST_TSO4 = 1 << 7;
        /// Driver can receive TSOv6.
        const GUEST_TSO6 = 1 << 8;
        /// Driver can receive TSO with ECN.
        const GUEST_ECN = 1 << 9;
        /// Driver can receive UFO.
        const GUEST_UFO = 1 << 10;
        /// Device can receive TSOv4.
        const HOST_TSO4 = 1 << 11;
        /// Device can receive TSOv6.
        const HOST_TSO6 = 1 << 12;
        /// Device can receive TSO with ECN.
        const HOST_ECN = 1 << 13;
        /// Device can receive UFO.
        const HOST_UFO = 1 << 14;
        /// Driver can merge receive buffers.
        const MRG_RXBUF = 1 << 15;
        /// Configuration status field is available.
        const STATUS = 1 << 16;
        /// Control channel is available.
        const CTRL_VQ = 1 << 17;
        /// Control channel RX mode support.
        const CTRL_RX = 1 << 18;
        /// Control channel VLAN filtering.
        const CTRL_VLAN = 1 << 19;
        ///
        const CTRL_RX_EXTRA = 1 << 20;
        /// Driver can send gratuitous packets.
        const GUEST_ANNOUNCE = 1 << 21;
        /// Device supports multiqueue with automatic receive steering.
        const MQ = 1 << 22;
        /// Set MAC address through control channel.
        const CTL_MAC_ADDR = 1 << 23;

        // device independent
        const RING_INDIRECT_DESC = 1 << 28;
        const RING_EVENT_IDX = 1 << 29;
        const VERSION_1 = 1 << 32; // legacy
    }
}

bitflags! {
    struct Status: u16 {
        const LINK_UP = 1;
        const ANNOUNCE = 2;
    }
}

bitflags! {
    struct InterruptStatus : u32 {
        const USED_RING_UPDATE = 1 << 0;
        const CONFIGURATION_CHANGE = 1 << 1;
    }
}

#[repr(C)]
#[derive(Debug)]
struct Config {
    mac: ReadOnly<EthernetAddress>,
    status: ReadOnly<Status>,
}

type EthernetAddress = [u8; 6];

// virtio 5.1.6 Device Operation
#[repr(C)]
#[derive(Debug)]
struct Header {
    flags: Volatile<Flags>,
    gso_type: Volatile<GsoType>,
    hdr_len: Volatile<u16>, // cannot rely on this
    gso_size: Volatile<u16>,
    csum_start: Volatile<u16>,
    csum_offset: Volatile<u16>,
    // payload starts from here
}

unsafe impl AsBuf for Header {}

bitflags! {
    struct Flags: u8 {
        const NEEDS_CSUM = 1;
        const DATA_VALID = 2;
        const RSC_INFO   = 4;
    }
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum GsoType {
    NONE = 0,
    TCPV4 = 1,
    UDP = 3,
    TCPV6 = 4,
    ECN = 0x80,
}

const QUEUE_RECEIVE: usize = 0;
const QUEUE_TRANSMIT: usize = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dev::{fake_header, Ring};

    /// virtio-net, from the device-id table (5.1: network device).
    const DEVICE_ID_NET: u32 = 1;

    /// The queue size `new` asks for is 2; the tests use 4 so a chain of two
    /// descriptors can be outstanding twice over.
    const QUEUE_SIZE: u16 = 4;

    /// A driver over a fake device, plus the device's side of the receive queue.
    ///
    /// `VirtIONet::new` cannot be called: it builds two queues back to back, and
    /// in the fake header one memory cell stands in for the per-queue `QueuePFN`
    /// register, so the second queue would be told the first one's address and
    /// refused with `AlreadyUsed`. What follows is `new`'s body with
    /// `fake_forget_queue_pfn` in between.
    fn driver() -> (VirtIONet<'static>, Ring) {
        let header = fake_header(DEVICE_ID_NET, 16);
        header.begin_init(|_| 0);
        let recv_queue = VirtQueue::new(header, QUEUE_RECEIVE, QUEUE_SIZE)
            .expect("the receive queue was refused");
        // The receive queue's ring has to be found NOW, while the single PFN
        // cell still holds its address: the send queue is about to overwrite it.
        let ring = Ring::of(header, QUEUE_RECEIVE as u32, QUEUE_SIZE);
        header.fake_forget_queue_pfn();
        let send_queue =
            VirtQueue::new(header, QUEUE_TRANSMIT, QUEUE_SIZE).expect("the send queue was refused");
        header.finish_init();
        (
            VirtIONet {
                header,
                mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
                recv_queue,
                send_queue,
            },
            ring,
        )
    }

    /// The device side of one receive: wait for the driver to offer a chain,
    /// write `frame` into it after the 10-byte header, and complete it with a
    /// length of the device's choosing.
    ///
    /// `recv` spins on `can_pop`, so the device has to run on another thread.
    fn device_receives(
        ring: Ring,
        slot: u16,
        frame: Vec<u8>,
        len: u32,
    ) -> std::thread::JoinHandle<Vec<(usize, usize, bool)>> {
        std::thread::spawn(move || {
            while ring.avail_idx() <= slot {
                std::thread::yield_now();
            }
            let head = ring.avail_entry(slot);
            let chain = ring.chain(head);
            let mut bytes = vec![0u8; size_of::<Header>()];
            bytes.extend_from_slice(&frame);
            ring.fill(head, &bytes);
            ring.complete(head, len);
            chain
        })
    }

    #[test]
    fn the_config_space_is_the_layout_the_specification_describes() {
        // 5.1.4: the MAC at offset 0, the status word at 6. The driver reads
        // these off the device's window, so a field inserted here reads the
        // wrong register without a word of complaint.
        use core::mem::MaybeUninit;
        let config = MaybeUninit::<Config>::uninit();
        let base = config.as_ptr() as usize;
        let c = config.as_ptr();
        unsafe {
            assert_eq!(core::ptr::addr_of!((*c).mac) as usize - base, 0);
            assert_eq!(core::ptr::addr_of!((*c).status) as usize - base, 6);
        }
    }

    #[test]
    fn the_frame_header_is_the_ten_bytes_the_specification_describes() {
        // 5.1.6: flags, gso_type, hdr_len, gso_size, csum_start, csum_offset,
        // and no num_buffers because MRG_RXBUF is not negotiated. `recv`
        // subtracts this size from what the device reports, so getting it wrong
        // shifts every received frame by the difference.
        use core::mem::MaybeUninit;
        let header = MaybeUninit::<Header>::uninit();
        let base = header.as_ptr() as usize;
        let h = header.as_ptr();
        unsafe {
            assert_eq!(core::ptr::addr_of!((*h).flags) as usize - base, 0);
            assert_eq!(core::ptr::addr_of!((*h).gso_type) as usize - base, 1);
            assert_eq!(core::ptr::addr_of!((*h).hdr_len) as usize - base, 2);
            assert_eq!(core::ptr::addr_of!((*h).gso_size) as usize - base, 4);
            assert_eq!(core::ptr::addr_of!((*h).csum_start) as usize - base, 6);
            assert_eq!(core::ptr::addr_of!((*h).csum_offset) as usize - base, 8);
        }
        assert_eq!(size_of::<Header>(), 10);
    }

    #[test]
    fn a_frame_the_device_writes_comes_back_whole_and_behind_its_header() {
        let (mut net, ring) = driver();
        let frame: Vec<u8> = (0..60).map(|i| (i * 7 % 251) as u8).collect();
        let device = device_receives(
            ring,
            0,
            frame.clone(),
            (size_of::<Header>() + frame.len()) as u32,
        );
        let mut buf = [0u8; 1514];
        let n = net.recv(&mut buf).expect("recv failed");
        let chain = device.join().expect("the device thread panicked");
        assert_eq!(n, frame.len(), "the payload length is not what arrived");
        assert_eq!(&buf[..n], &frame[..], "the payload is not what was written");
        // The header goes in its own descriptor, before the caller's buffer, so
        // the caller never sees it and never has to skip it.
        assert_eq!(chain.len(), 2, "a receive is header plus payload");
        assert_eq!(chain[0].1, size_of::<Header>());
        assert!(
            chain[0].2 && chain[1].2,
            "the device cannot write a receive"
        );
    }

    #[test]
    fn a_completion_short_of_the_header_is_refused_instead_of_underflowing() {
        // `len` is the used ring's length word, which the DEVICE writes.
        // Subtracting the 10-byte header from it used to underflow: a panic in
        // debug, and in release a length near `usize::MAX` handed to the caller
        // as the size of a frame.
        for short in 0..size_of::<Header>() as u32 {
            let (mut net, ring) = driver();
            let device = device_receives(ring, 0, Vec::new(), short);
            let mut buf = [0u8; 64];
            let err = net
                .recv(&mut buf)
                .expect_err("a short completion was accepted");
            device.join().expect("the device thread panicked");
            assert_eq!(err, Error::IoError, "short completion {}", short);
        }
    }

    #[test]
    fn a_completion_exactly_the_header_is_an_empty_frame_and_not_an_error() {
        // The boundary between the two: ten bytes is a header and no payload,
        // which is odd but not broken, and it must not be refused.
        let (mut net, ring) = driver();
        let device = device_receives(ring, 0, Vec::new(), size_of::<Header>() as u32);
        let mut buf = [0u8; 64];
        let n = net.recv(&mut buf).expect("an empty frame was refused");
        device.join().expect("the device thread panicked");
        assert_eq!(n, 0);
    }

    #[test]
    fn a_completion_longer_than_the_buffer_promises_only_what_the_buffer_holds() {
        // The other end of the same word: a device claiming 100000 bytes used to
        // have `recv` return 99990, and the caller reads that many bytes out of
        // a buffer that holds 64.
        let (mut net, ring) = driver();
        let device = device_receives(ring, 0, vec![0xcd; 64], 100_000);
        let mut buf = [0u8; 64];
        let n = net.recv(&mut buf).expect("recv failed");
        device.join().expect("the device thread panicked");
        assert_eq!(n, buf.len(), "recv promised more than the buffer holds");
    }

    /// What the device saw in one chain: `(writable, bytes)` per descriptor.
    ///
    /// The device thread reads and completes, and judges nothing: `send` and
    /// `recv` spin until the chain comes back, so an assert that fires over
    /// there leaves the driver spinning and the suite hangs without naming a
    /// single test. Every judgement is made on the test thread, after the join.
    fn device_sends(ring: Ring) -> std::thread::JoinHandle<Vec<(bool, Vec<u8>)>> {
        std::thread::spawn(move || {
            while ring.avail_idx() == 0 {
                std::thread::yield_now();
            }
            let head = ring.avail_entry(0);
            let seen = ring
                .chain(head)
                .into_iter()
                .map(|(addr, len, writable)| {
                    let bytes =
                        unsafe { core::slice::from_raw_parts(addr as *const u8, len) }.to_vec();
                    (writable, bytes)
                })
                .collect();
            ring.complete(head, 0);
            seen
        })
    }

    #[test]
    fn a_frame_is_sent_with_its_header_in_front_of_the_payload() {
        // `send` spins on the send queue, so the device runs on another thread.
        let (mut net, _ring) = driver();
        let device = device_sends(Ring::of(net.header, QUEUE_TRANSMIT as u32, QUEUE_SIZE));
        let payload: Vec<u8> = (0..40).map(|i| (i * 3 % 251) as u8).collect();
        net.send(&payload).expect("send failed");
        let seen = device.join().expect("the device thread panicked");
        assert_eq!(seen.len(), 2, "a send is header plus payload");
        assert!(
            !seen[0].0 && !seen[1].0,
            "the device must not write to a frame being sent"
        );
        assert_eq!(
            seen[0].1.len(),
            size_of::<Header>(),
            "the header is not 10 bytes"
        );
        // The header is zeroed on the way out: no checksum offload, no GSO.
        assert!(
            seen[0].1.iter().all(|b| *b == 0),
            "the sent header is not zeroed"
        );
        assert_eq!(
            seen[1].1, payload,
            "the payload that went out is not the payload that was sent"
        );
    }

    #[test]
    fn the_queue_the_driver_notifies_is_the_queue_it_published_on() {
        // Receive is queue 0 and transmit is queue 1 (5.1.2). Notifying the
        // wrong one leaves the frame sitting in the ring with the device idle,
        // which on a real machine looks like a network that has simply stopped.
        let (mut net, _ring) = driver();
        let device = device_sends(Ring::of(net.header, QUEUE_TRANSMIT as u32, QUEUE_SIZE));
        net.send(b"x").expect("send failed");
        device.join().expect("the device thread panicked");
        assert_eq!(net.header.fake_notified(), QUEUE_TRANSMIT as u32);
    }

    #[test]
    fn the_mac_is_read_from_the_device_and_handed_back_unchanged() {
        let (net, _ring) = driver();
        assert_eq!(net.mac(), [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    }

    #[test]
    fn a_send_needs_two_descriptors_and_says_so_before_it_blocks() {
        // `send` puts a header and a payload in the ring, so it needs two free
        // descriptors, and `can_send` is what the caller asks first. A queue
        // with one descriptor left cannot take a frame, and a `can_send` that
        // says otherwise turns a full ring into a driver that waits forever.
        let (mut net, _ring) = driver();
        assert!(net.can_send());
        let mut filled = 0;
        while net.send_queue.available_desc() >= 2 {
            net.send_queue
                .add(&[b"x"], &[])
                .expect("a descriptor was refused");
            filled += 1;
        }
        assert!(filled > 0, "the queue took nothing at all");
        assert!(
            !net.can_send(),
            "can_send says yes with {} descriptors left",
            net.send_queue.available_desc()
        );
    }

    #[test]
    fn nothing_arrived_is_not_something_arrived() {
        let (net, _ring) = driver();
        assert!(!net.can_recv(), "an untouched queue has a packet waiting");
    }
}
