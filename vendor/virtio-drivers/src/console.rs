use super::*;
use crate::queue::VirtQueue;
use bitflags::*;
use core::{fmt, hint::spin_loop};
use log::*;
use volatile::{ReadOnly, WriteOnly};

const QUEUE_RECEIVEQ_PORT_0: usize = 0;
const QUEUE_TRANSMITQ_PORT_0: usize = 1;

/// Virtio console. Only one single port is allowed since ``alloc'' is disabled.
/// Emergency and cols/rows unimplemented.
pub struct VirtIOConsole<'a> {
    header: &'static mut VirtIOHeader,
    receiveq: VirtQueue<'a>,
    transmitq: VirtQueue<'a>,
    queue_buf_dma: DMA,
    queue_buf_rx: &'a mut [u8],
    cursor: usize,
    pending_len: usize,
}

impl<'a> VirtIOConsole<'a> {
    /// Create a new VirtIO-Console driver.
    pub fn new(header: &'static mut VirtIOHeader) -> Result<Self> {
        header.begin_init(|features| {
            let features = Features::from_bits_truncate(features);
            info!("Device features {:?}", features);
            let supported_features = Features::empty();
            (features & supported_features).bits()
        });
        let config = unsafe { &mut *(header.config_space() as *mut Config) };
        info!("Config: {:?}", config);
        let receiveq = VirtQueue::new(header, QUEUE_RECEIVEQ_PORT_0, 2)?;
        let transmitq = VirtQueue::new(header, QUEUE_TRANSMITQ_PORT_0, 2)?;
        let queue_buf_dma = DMA::new(1)?;
        let queue_buf_rx = unsafe { &mut queue_buf_dma.as_buf()[0..] };
        header.finish_init();
        let mut console = VirtIOConsole {
            header,
            receiveq,
            transmitq,
            queue_buf_dma,
            queue_buf_rx,
            cursor: 0,
            pending_len: 0,
        };
        console.poll_retrieve()?;
        Ok(console)
    }
    fn poll_retrieve(&mut self) -> Result<()> {
        self.receiveq.add(&[], &[self.queue_buf_rx])?;
        Ok(())
    }
    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> Result<bool> {
        let ack = self.header.ack_interrupt();
        if !ack {
            return Ok(false);
        }
        let mut flag = false;
        while let Ok((_token, len)) = self.receiveq.pop_used() {
            // `len` is the used ring's length word, and the DEVICE writes it.
            // It used to be asserted non-zero and then taken as-is, so the
            // device chose between two kernel panics: zero tripped the assert
            // in this interrupt handler, and anything past the receive buffer
            // walked `recv` off the end of a DMA page one byte at a time.
            let len = len as usize;
            let room = self.queue_buf_rx.len();
            if len > room {
                warn!(
                    "[virtio-console] device completed {} bytes into a {}-byte buffer, taking {}",
                    len, room, room
                );
            }
            self.cursor = 0;
            self.pending_len = len.min(room);
            if self.pending_len == 0 {
                // Nothing to read, and `recv` only offers the buffer again once
                // a reader has drained it -- so without this the console would
                // be deaf for the rest of the boot, which is what the assert
                // that used to stand here was really hiding.
                self.poll_retrieve()?;
                continue;
            }
            flag = true;
        }
        Ok(flag)
    }

    /// Try get char.
    pub fn recv(&mut self, pop: bool) -> Result<Option<u8>> {
        if self.cursor == self.pending_len {
            return Ok(None);
        }
        let ch = self.queue_buf_rx[self.cursor];
        if pop {
            self.cursor += 1;
            if self.cursor == self.pending_len {
                self.poll_retrieve()?;
            }
        }
        Ok(Some(ch))
    }
    /// Put a char onto the device.
    pub fn send(&mut self, chr: u8) -> Result<()> {
        let buf: [u8; 1] = [chr];
        self.transmitq.add(&[&buf], &[])?;
        self.header.notify(QUEUE_TRANSMITQ_PORT_0 as u32);
        while !self.transmitq.can_pop() {
            spin_loop();
        }
        self.transmitq.pop_used()?;
        Ok(())
    }
}

#[repr(C)]
struct Config {
    cols: ReadOnly<u16>,
    rows: ReadOnly<u16>,
    max_nr_ports: ReadOnly<u32>,
    emerg_wr: WriteOnly<u32>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Config")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("max_nr_ports", &self.max_nr_ports)
            .finish()
    }
}

bitflags! {
    struct Features: u64 {
        const SIZE                  = 1 << 0;
        const MULTIPORT             = 1 << 1;
        const EMERG_WRITE           = 1 << 2;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dev::{fake_header, Ring};

    /// virtio-console, from the device-id table (5.3: console device).
    const DEVICE_ID_CONSOLE: u32 = 3;

    /// The queue size `new` asks for.
    const QUEUE_SIZE: u16 = 2;

    /// A driver over a fake device, plus the device's side of the receive queue.
    ///
    /// `VirtIOConsole::new` cannot be called: it builds two queues back to back,
    /// and in the fake header one memory cell stands in for the per-queue
    /// `QueuePFN` register, so the second queue would be told the first one's
    /// address and refused with `AlreadyUsed`. What follows is `new`'s body with
    /// `fake_forget_queue_pfn` in between -- so every step of the real
    /// construction still runs here, in its real order.
    fn driver() -> (VirtIOConsole<'static>, Ring) {
        let header = fake_header(DEVICE_ID_CONSOLE, 16);
        header.begin_init(|_| 0);
        let receiveq = VirtQueue::new(header, QUEUE_RECEIVEQ_PORT_0, QUEUE_SIZE)
            .expect("the receive queue was refused");
        // The receive queue's ring has to be found NOW, while the single PFN
        // cell still holds its address: the transmit queue is about to
        // overwrite it.
        let ring = Ring::of(header, QUEUE_RECEIVEQ_PORT_0 as u32, QUEUE_SIZE);
        header.fake_forget_queue_pfn();
        let transmitq = VirtQueue::new(header, QUEUE_TRANSMITQ_PORT_0, QUEUE_SIZE)
            .expect("the transmit queue was refused");
        let queue_buf_dma = DMA::new(1).expect("no DMA page for the receive buffer");
        let queue_buf_rx = unsafe { &mut queue_buf_dma.as_buf()[0..] };
        header.finish_init();
        let mut console = VirtIOConsole {
            header,
            receiveq,
            transmitq,
            queue_buf_dma,
            queue_buf_rx,
            cursor: 0,
            pending_len: 0,
        };
        console
            .poll_retrieve()
            .expect("the receive buffer was refused");
        (console, ring)
    }

    /// Put `bytes` in the buffer the driver offered, hand it back with a length
    /// of the device's choosing, and raise the interrupt.
    fn device_says(
        console: &mut VirtIOConsole<'_>,
        ring: &Ring,
        slot: u16,
        bytes: &[u8],
        len: u32,
    ) {
        let head = ring.avail_entry(slot);
        ring.fill(head, bytes);
        ring.complete(head, len);
        console.header.fake_raise_interrupt(1);
    }

    /// Read the whole pending run, one `recv` at a time.
    fn drain(console: &mut VirtIOConsole<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(byte) = console.recv(true).expect("recv failed") {
            out.push(byte);
            // A run longer than the buffer means `recv` is walking past the end,
            // which is the bug this suite is here for: stop rather than let the
            // test die by index.
            assert!(
                out.len() <= console.queue_buf_rx.len(),
                "recv handed out more bytes than the receive buffer holds"
            );
        }
        out
    }

    #[test]
    fn the_config_space_is_the_layout_the_specification_describes() {
        // 5.3.4: cols and rows at 0 and 2, max_nr_ports at 4, emerg_wr at 8.
        // The driver reads these straight off the device's window, so a field
        // inserted or reordered here reads the wrong register in silence.
        use core::mem::MaybeUninit;
        let config = MaybeUninit::<Config>::uninit();
        let base = config.as_ptr() as usize;
        let at = |field: usize| field - base;
        let c = config.as_ptr();
        unsafe {
            assert_eq!(at(core::ptr::addr_of!((*c).cols) as usize), 0);
            assert_eq!(at(core::ptr::addr_of!((*c).rows) as usize), 2);
            assert_eq!(at(core::ptr::addr_of!((*c).max_nr_ports) as usize), 4);
            assert_eq!(at(core::ptr::addr_of!((*c).emerg_wr) as usize), 8);
        }
        assert_eq!(core::mem::size_of::<Config>(), 12);
    }

    #[test]
    fn the_receive_buffer_is_offered_to_the_device_at_construction() {
        // Nothing else offers it: `recv` only re-offers a buffer a reader has
        // drained, so a console built without this one never receives a byte.
        let (console, ring) = driver();
        assert_eq!(ring.avail_idx(), 1, "the receive buffer was not offered");
        let head = ring.avail_entry(0);
        let chain = ring.chain(head);
        assert_eq!(chain.len(), 1, "the receive buffer is one descriptor");
        assert!(chain[0].2, "the device cannot write to the receive buffer");
        assert_eq!(
            chain[0].1,
            console.queue_buf_rx.len(),
            "the descriptor is not the whole receive buffer"
        );
    }

    #[test]
    fn what_the_device_writes_comes_back_in_order_and_then_runs_out() {
        let (mut console, ring) = driver();
        device_says(&mut console, &ring, 0, b"hola", 4);
        assert!(console.ack_interrupt().expect("ack failed"));
        assert_eq!(drain(&mut console), b"hola".to_vec());
        assert_eq!(console.recv(true).expect("recv failed"), None);
    }

    #[test]
    fn a_read_without_popping_does_not_consume_the_byte() {
        let (mut console, ring) = driver();
        device_says(&mut console, &ring, 0, b"ab", 2);
        console.ack_interrupt().expect("ack failed");
        assert_eq!(console.recv(false).expect("recv failed"), Some(b'a'));
        assert_eq!(console.recv(false).expect("recv failed"), Some(b'a'));
        assert_eq!(console.recv(true).expect("recv failed"), Some(b'a'));
        assert_eq!(console.recv(true).expect("recv failed"), Some(b'b'));
    }

    #[test]
    fn draining_the_run_offers_the_buffer_to_the_device_again() {
        // The device has nowhere to put the next keystroke until this happens,
        // so a console that forgets it takes one line and goes quiet.
        let (mut console, ring) = driver();
        device_says(&mut console, &ring, 0, b"x", 1);
        console.ack_interrupt().expect("ack failed");
        assert_eq!(ring.avail_idx(), 1);
        drain(&mut console);
        assert_eq!(
            ring.avail_idx(),
            2,
            "the receive buffer was not offered again"
        );
    }

    #[test]
    fn a_completion_longer_than_the_receive_buffer_does_not_walk_off_a_dma_page() {
        // `len` is the used ring's length word, which the device writes. It used
        // to be taken as-is into `pending_len`, and `recv` indexes the receive
        // buffer with it: a device claiming 100000 bytes into one DMA page
        // panicked the kernel on the 4097th read.
        let (mut console, ring) = driver();
        let room = console.queue_buf_rx.len();
        let payload: Vec<u8> = (0..room).map(|i| (i % 251) as u8).collect();
        device_says(&mut console, &ring, 0, &payload, 100_000);
        assert!(console.ack_interrupt().expect("ack failed"));
        assert_eq!(
            console.pending_len, room,
            "the device's length was taken past the end of the buffer"
        );
        assert_eq!(drain(&mut console), payload);
    }

    #[test]
    fn a_completion_one_byte_past_the_buffer_is_cut_to_the_buffer() {
        // The boundary of the same bug, where an off-by-one lives.
        let (mut console, ring) = driver();
        let room = console.queue_buf_rx.len();
        device_says(&mut console, &ring, 0, b"z", room as u32 + 1);
        console.ack_interrupt().expect("ack failed");
        assert_eq!(console.pending_len, room);
    }

    #[test]
    fn an_empty_completion_does_not_take_the_kernel_down() {
        // A device that hands the buffer back with zero bytes used to trip
        // `assert_ne!(len, 0)` -- a kernel panic from an interrupt handler, for
        // one value the driver does not choose.
        let (mut console, ring) = driver();
        device_says(&mut console, &ring, 0, b"", 0);
        assert!(
            !console.ack_interrupt().expect("ack failed"),
            "an empty completion is not data to wake a reader for"
        );
        assert_eq!(console.recv(true).expect("recv failed"), None);
    }

    #[test]
    fn an_empty_completion_leaves_the_console_still_listening() {
        // And this is what the assert was really hiding: with nothing pending,
        // `recv` returns None without ever re-offering the buffer, so simply
        // dropping the assert would leave the console deaf for the rest of the
        // boot. The buffer has to go back to the device here.
        let (mut console, ring) = driver();
        device_says(&mut console, &ring, 0, b"", 0);
        console.ack_interrupt().expect("ack failed");
        assert_eq!(
            ring.avail_idx(),
            2,
            "the receive buffer was not offered again after an empty completion"
        );
        device_says(&mut console, &ring, 1, b"sigo aqui", 9);
        assert!(console.ack_interrupt().expect("ack failed"));
        assert_eq!(drain(&mut console), b"sigo aqui".to_vec());
    }

    #[test]
    fn an_interrupt_the_device_did_not_raise_is_not_data() {
        let (mut console, ring) = driver();
        ring.complete(ring.avail_entry(0), 3);
        // No `fake_raise_interrupt`: the status register reads zero, so there is
        // nothing to acknowledge and nothing to report.
        assert!(!console.ack_interrupt().expect("ack failed"));
        assert_eq!(console.pending_len, 0);
    }

    #[test]
    fn a_byte_sent_reaches_the_device_on_the_transmit_queue() {
        // `send` spins on the transmit queue until the device answers, so the
        // device side runs on another thread. It judges nothing over there: an
        // assert that fires in that thread leaves `send` spinning and hangs the
        // suite without naming a test, so it reads, completes, and hands back
        // what it saw.
        let (mut console, _ring) = driver();
        let transmit = Ring::of(console.header, QUEUE_TRANSMITQ_PORT_0 as u32, QUEUE_SIZE);
        let device = std::thread::spawn(move || {
            while transmit.avail_idx() == 0 {
                std::thread::yield_now();
            }
            let head = transmit.avail_entry(0);
            let seen: Vec<(bool, Vec<u8>)> = transmit
                .chain(head)
                .into_iter()
                .map(|(addr, len, writable)| {
                    let bytes =
                        unsafe { core::slice::from_raw_parts(addr as *const u8, len) }.to_vec();
                    (writable, bytes)
                })
                .collect();
            transmit.complete(head, 0);
            seen
        });
        console.send(b'Q').expect("send failed");
        let seen = device.join().expect("the device thread panicked");
        assert_eq!(seen.len(), 1, "a sent byte is one descriptor");
        assert!(!seen[0].0, "the device must not write to a sent byte");
        assert_eq!(seen[0].1, b"Q".to_vec(), "the wrong byte went out");
        assert_eq!(
            console.header.fake_notified(),
            QUEUE_TRANSMITQ_PORT_0 as u32
        );
    }
}
