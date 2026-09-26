use super::*;
use alloc::boxed::Box;
use bitflags::*;
use log::*;
use volatile::{ReadOnly, WriteOnly};

/// Virtual human interface devices such as keyboards, mice and tablets.
///
/// An instance of the virtio device represents one such input device.
/// Device behavior mirrors that of the evdev layer in Linux,
/// making pass-through implementations on top of evdev easy.
pub struct VirtIOInput<'a> {
    header: &'static mut VirtIOHeader,
    event_queue: VirtQueue<'a>,
    status_queue: VirtQueue<'a>,
    event_buf: Box<[InputEvent; 32]>,
}

impl<'a> VirtIOInput<'a> {
    /// Create a new VirtIO-Input driver.
    pub fn new(header: &'static mut VirtIOHeader) -> Result<Self> {
        let mut event_buf = Box::new([InputEvent::default(); QUEUE_SIZE]);
        header.begin_init(|features| {
            let features = Feature::from_bits_truncate(features);
            info!("Device features: {:?}", features);
            // negotiate these flags only
            let supported_features = Feature::empty();
            (features & supported_features).bits()
        });

        let mut event_queue = VirtQueue::new(header, QUEUE_EVENT, QUEUE_SIZE as u16)?;
        let status_queue = VirtQueue::new(header, QUEUE_STATUS, QUEUE_SIZE as u16)?;
        for (i, event) in event_buf.as_mut().iter_mut().enumerate() {
            let token = event_queue.add(&[], &[event.as_buf_mut()])?;
            assert_eq!(token, i as u16);
        }

        header.finish_init();

        Ok(VirtIOInput {
            header,
            event_queue,
            status_queue,
            event_buf,
        })
    }

    /// Acknowledge interrupt and process events.
    pub fn ack_interrupt(&mut self) -> bool {
        self.header.ack_interrupt()
    }

    /// Pop the pending event.
    pub fn pop_pending_event(&mut self) -> Option<InputEvent> {
        if let Ok((token, _)) = self.event_queue.pop_used() {
            let slot = &mut self.event_buf[token as usize];
            // Copy the event out BEFORE handing the buffer back. `add`
            // publishes the descriptor to the available ring, and from that
            // moment the buffer belongs to the device (2.6.13): reading the
            // event afterwards raced it for the bytes. On a burst -- a mouse
            // being moved, a key held down -- what came back could be the next
            // event, or half of each.
            let event = *slot;
            // requeue
            if self.event_queue.add(&[], &[slot.as_buf_mut()]).is_err() {
                // The slot is lost, but the event is already in hand, and
                // answering `None` would lose it too: the only caller drains
                // until `None`, so it would also cut the pass short and leave
                // the rest of the queue sitting until the next interrupt.
                warn!(
                    "[virtio-input] event buffer {} could not be requeued",
                    token
                );
            }
            return Some(event);
        }
        None
    }

    /// Query a specific piece of information by `select` and `subsel`, and write
    /// result to `out`, returning the number of bytes actually written.
    ///
    /// The `size` byte is written by the **device**, so it is an untrusted
    /// length: anything up to 255, against a `u.bitmap` of 128 bytes and a
    /// caller's buffer that may be smaller still. Slicing either of them by it
    /// panicked, and this is on the device probe path -- and reachable again,
    /// on demand, from every `EVIOCGBIT`/`EVIOCGPROP` ioctl and from
    /// `/proc`/`sysfs` by way of `InputScheme::capability`. So the length is
    /// clamped to what both buffers can actually hold.
    ///
    /// What is returned is the clamped count, never the device's claim: the
    /// caller slices its own buffer by this value (`&bitmap[..size]`), so
    /// handing back an unclamped `size` would only move the panic one frame up.
    pub fn query_config_select(
        &mut self,
        select: InputConfigSelect,
        subsel: u8,
        out: &mut [u8],
    ) -> u8 {
        let config = unsafe { &mut *(self.header.config_space() as *mut Config) };
        config.select.write(select as u8);
        config.subsel.write(subsel);
        let size = config.size.read();
        let data = config.data.read();
        let n = (size as usize).min(out.len()).min(data.len());
        if n != size as usize {
            // A device that cannot describe its own bitmap is worth one line;
            // a silent clamp here reads as a device with fewer capabilities
            // than it has, which is indistinguishable from a working device.
            warn!(
                "[virtio-input] device claims {} bytes for select={:#x} subsel={:#x}, \
                 taking {} (u.bitmap is {}, caller's buffer {})",
                size,
                select as u8,
                subsel,
                n,
                data.len(),
                out.len()
            );
        }
        out[..n].copy_from_slice(&data[..n]);
        // `n <= 128`, so this cannot be the truncation it looks like.
        n as u8
    }
}

/// Select value used for [`VirtIOInput::query_config_select()`].
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum InputConfigSelect {
    /// Returns the name of the device, in u.string. subsel is zero.
    IdName = 0x01,
    /// Returns the serial number of the device, in u.string. subsel is zero.
    IdSerial = 0x02,
    /// Returns ID information of the device, in u.ids. subsel is zero.
    IdDevids = 0x03,
    /// Returns input properties of the device, in u.bitmap. subsel is zero.
    /// Individual bits in the bitmap correspond to INPUT_PROP_* constants used
    /// by the underlying evdev implementation.
    PropBits = 0x10,
    /// subsel specifies the event type using EV_* constants in the underlying
    /// evdev implementation. If size is non-zero the event type is supported
    /// and a bitmap of supported event codes is returned in u.bitmap. Individual
    /// bits in the bitmap correspond to implementation-defined input event codes,
    /// for example keys or pointing device axes.
    EvBits = 0x11,
    /// subsel specifies the absolute axis using ABS_* constants in the underlying
    /// evdev implementation. Information about the axis will be returned in u.abs.
    AbsInfo = 0x12,
}

#[repr(C)]
struct Config {
    select: WriteOnly<u8>,
    subsel: WriteOnly<u8>,
    size: ReadOnly<u8>,
    _reversed: [ReadOnly<u8>; 5],
    data: ReadOnly<[u8; 128]>,
}

#[repr(C)]
#[derive(Debug)]
struct AbsInfo {
    min: u32,
    max: u32,
    fuzz: u32,
    flat: u32,
    res: u32,
}

#[repr(C)]
#[derive(Debug)]
struct DevIDs {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// Both queues use the same `virtio_input_event` struct. `type`, `code` and `value`
/// are filled according to the Linux input layer (evdev) interface.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputEvent {
    /// Event type.
    pub event_type: u16,
    /// Event code.
    pub code: u16,
    /// Event value.
    pub value: u32,
}

unsafe impl AsBuf for InputEvent {}

bitflags! {
    struct Feature: u64 {
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

const QUEUE_EVENT: usize = 0;
const QUEUE_STATUS: usize = 1;

// a parameter that can change
const QUEUE_SIZE: usize = 32;

/// This driver carries every keystroke and every pointer movement of a virtio
/// machine, and it was the one file of this crate with no tests at all -- the
/// others got theirs when nothing in the tree compiled this crate as a test
/// target. What it had instead was a length written by the **device** used to
/// slice two fixed buffers, on the path a device takes to be probed.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dev::{fake_header, Ring};

    /// virtio-input, from the device-id table (5.8: input device).
    const DEVICE_ID_INPUT: u32 = 18;

    /// A driver over a fake device, plus the device's side of the event queue.
    ///
    /// `VirtIOInput::new` cannot be used: it builds two queues back to back, and
    /// in the fake header one memory cell stands in for the per-queue `QueuePFN`
    /// register, so the second queue reads the first one's PFN and is refused
    /// with `AlreadyUsed`. `fake_forget_queue_pfn` exists for exactly this, so
    /// what follows is `new`'s body with that call in between -- which also
    /// means every step of the real construction is asserted here.
    fn driver() -> (VirtIOInput<'static>, Ring) {
        let header = fake_header(DEVICE_ID_INPUT, 256);
        let mut event_buf = Box::new([InputEvent::default(); QUEUE_SIZE]);
        header.begin_init(|_| 0);
        let mut event_queue = VirtQueue::new(header, QUEUE_EVENT, QUEUE_SIZE as u16)
            .expect("the event queue was refused");
        // The event queue's ring has to be found NOW, while the single PFN cell
        // still holds its address: the status queue is about to overwrite it.
        let ring = Ring::of(header, QUEUE_EVENT as u32, QUEUE_SIZE as u16);
        header.fake_forget_queue_pfn();
        let status_queue = VirtQueue::new(header, QUEUE_STATUS, QUEUE_SIZE as u16)
            .expect("the status queue was refused");
        for (i, event) in event_buf.as_mut().iter_mut().enumerate() {
            let token = event_queue
                .add(&[], &[event.as_buf_mut()])
                .expect("an event buffer was refused");
            assert_eq!(token, i as u16, "the tokens are not the slot indices");
        }
        header.finish_init();
        (
            VirtIOInput {
                header,
                event_queue,
                status_queue,
                event_buf,
            },
            ring,
        )
    }

    /// The device's side of a config query: the `size` byte it reports and the
    /// bytes it puts in `u.bitmap`, written at the offsets [`Config`] declares.
    fn device_answers(input: &VirtIOInput<'_>, size: u8, data: &[u8]) {
        assert!(data.len() <= 128, "u.bitmap is 128 bytes");
        let config = input.header.config_space() as *mut u8;
        // SAFETY: the config space is 0x100 bytes of ordinary memory behind the
        // fake header, and `Config` occupies its first 136.
        unsafe {
            config.add(2).write_volatile(size);
            for (i, byte) in data.iter().enumerate() {
                config.add(8 + i).write_volatile(*byte);
            }
        }
    }

    /// What the driver last wrote into `select`/`subsel`.
    fn selector(input: &VirtIOInput<'_>) -> (u8, u8) {
        let config = input.header.config_space() as *const u8;
        // SAFETY: as above.
        unsafe { (config.read_volatile(), config.add(1).read_volatile()) }
    }

    #[test]
    fn the_config_space_is_the_layout_the_specification_describes() {
        // `Config` is overlaid on the device's config window, so each field's
        // offset IS the register: `select` at 0, `subsel` at 1, `size` at 2,
        // five reserved bytes, and `u` at 8 (5.8.4). A field added or a padding
        // byte miscounted and every query reads and writes the wrong bytes.
        let map = core::mem::MaybeUninit::<Config>::uninit();
        let at = map.as_ptr() as usize;
        // SAFETY: `addr_of!` only takes addresses; nothing uninitialised is read.
        unsafe {
            assert_eq!(core::ptr::addr_of!((*map.as_ptr()).select) as usize - at, 0);
            assert_eq!(core::ptr::addr_of!((*map.as_ptr()).subsel) as usize - at, 1);
            assert_eq!(core::ptr::addr_of!((*map.as_ptr()).size) as usize - at, 2);
            assert_eq!(
                core::ptr::addr_of!((*map.as_ptr()).data) as usize - at,
                8,
                "u.bitmap sits after five reserved bytes, not right after size"
            );
        }
        assert_eq!(core::mem::size_of::<Config>(), 8 + 128);
    }

    #[test]
    fn a_query_selects_what_was_asked_for_and_brings_back_what_the_device_put_there() {
        let (mut input, _ring) = driver();
        device_answers(&input, 4, &[0xde, 0xad, 0xbe, 0xef]);
        let mut out = [0u8; 128];
        let n = input.query_config_select(InputConfigSelect::EvBits, 0x11, &mut out);
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            selector(&input),
            (InputConfigSelect::EvBits as u8, 0x11),
            "the driver asked for something other than what it was told to"
        );
        assert_eq!(&out[4..8], &[0, 0, 0, 0], "it copied past the size");
    }

    #[test]
    fn a_device_that_claims_more_than_its_bitmap_holds_does_not_take_the_kernel_down() {
        // `size` is a byte the DEVICE writes, so up to 255, against a 128-byte
        // `u.bitmap`. Slicing by it panicked -- from the probe path, and again
        // from every `EVIOCGBIT` ioctl by way of `InputScheme::capability`.
        let (mut input, _ring) = driver();
        let filled = [0x5au8; 128];
        for claim in [129u8, 200, u8::MAX] {
            device_answers(&input, claim, &filled);
            let mut out = [0u8; 128];
            let n = input.query_config_select(InputConfigSelect::PropBits, 0, &mut out);
            assert_eq!(n, 128, "a claim of {} was not clamped to the bitmap", claim);
            assert_eq!(&out[..], &filled[..]);
        }
    }

    #[test]
    fn a_buffer_larger_than_the_bitmap_does_not_let_the_device_over_read_it() {
        // The clamp has to hold against `u.bitmap` too, not just against the
        // caller: with room to spare in `out`, a device claiming 200 bytes would
        // otherwise have 200 bytes read out of a 128-byte register window.
        let (mut input, _ring) = driver();
        device_answers(&input, 200, &[0x11; 128]);
        let mut out = [0u8; 256];
        let n = input.query_config_select(InputConfigSelect::PropBits, 0, &mut out) as usize;
        assert_eq!(
            n, 128,
            "the bitmap is 128 bytes however much room the caller has"
        );
        assert_eq!(&out[..128], &[0x11u8; 128][..]);
        assert_eq!(
            &out[128..],
            &[0u8; 128][..],
            "it read past the register window"
        );
    }

    #[test]
    fn a_buffer_smaller_than_the_bitmap_is_respected() {
        // The other half of the same clamp, and the one a caller controls: what
        // comes back must fit what was handed over, whatever the device claims.
        let (mut input, _ring) = driver();
        device_answers(&input, 64, &[0xff; 64]);
        let mut out = [0u8; 16];
        let n = input.query_config_select(InputConfigSelect::EvBits, 1, &mut out);
        assert_eq!(n, 16, "16 bytes of room, 64 bytes claimed");
        assert_eq!(&out[..], &[0xffu8; 16][..]);
    }

    #[test]
    fn the_count_that_comes_back_is_what_was_copied_and_not_what_was_claimed() {
        // The caller slices its own buffer by this number
        // (`&bitmap[..size]` in `zcore-drivers`), so handing back the device's
        // claim would only move the panic one frame up.
        let (mut input, _ring) = driver();
        device_answers(&input, u8::MAX, &[1u8; 128]);
        let mut out = [0u8; 8];
        let n = input.query_config_select(InputConfigSelect::EvBits, 0, &mut out) as usize;
        assert!(n <= out.len(), "{} is more than the buffer given", n);
        // The slice the caller would take, which must not panic.
        assert_eq!(out[..n].len(), 8);
    }

    #[test]
    fn a_size_of_zero_copies_nothing_and_leaves_the_buffer_alone() {
        // Which is how a device says "this event type is not supported", and
        // the `Event` branch of `capability()` reads it exactly that way.
        let (mut input, _ring) = driver();
        device_answers(&input, 0, &[0xaa; 32]);
        let mut out = [7u8; 32];
        assert_eq!(
            input.query_config_select(InputConfigSelect::EvBits, 3, &mut out),
            0
        );
        assert_eq!(&out[..], &[7u8; 32][..], "it wrote into the buffer anyway");
    }

    #[test]
    fn every_slot_of_the_event_queue_is_offered_to_the_device_at_construction() {
        // The driver's whole input path depends on this: the device has nowhere
        // to put an event except a buffer the driver has already published, so
        // a queue built with fewer than `QUEUE_SIZE` buffers silently caps how
        // many events can arrive between interrupts.
        let (input, ring) = driver();
        assert_eq!(
            ring.avail_idx(),
            QUEUE_SIZE as u16,
            "not every event buffer was offered"
        );
    }

    #[test]
    fn an_event_the_device_writes_comes_back_intact_and_its_slot_is_offered_again() {
        let (mut input, ring) = driver();
        let offered = ring.avail_idx();

        // EV_KEY, KEY_A, pressed -- as the device lays it out in the buffer.
        let event = InputEvent {
            event_type: 0x01,
            code: 30,
            value: 1,
        };
        let bytes = [
            event.event_type.to_le_bytes(),
            event.code.to_le_bytes(),
            [0, 0],
            [0, 0],
        ]
        .concat();
        let mut wire = bytes;
        wire[4] = event.value as u8;
        let written = ring.fill(0, &wire);
        ring.complete(0, written);

        let got = input.pop_pending_event().expect("the event never arrived");
        assert_eq!(got.event_type, event.event_type);
        assert_eq!(got.code, event.code);
        assert_eq!(got.value, event.value);
        assert_eq!(
            ring.avail_idx(),
            offered + 1,
            "the slot was not offered back, so the device loses a buffer per event"
        );
    }

    #[test]
    fn an_empty_queue_answers_none_rather_than_an_event_made_of_nothing() {
        // `handle_irq` drains until this says `None`, so a wrong answer here is
        // either a lost pass or an endless one.
        let (mut input, _ring) = driver();
        assert!(input.pop_pending_event().is_none());
    }
}
