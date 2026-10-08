use core::fmt;

use super::{event::EventScheme, Scheme};
use crate::input::input_event_codes::ev::*;

numeric_enum_macro::numeric_enum! {
    #[repr(u16)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    /// Linux input event codes.
    ///
    /// Reference: <https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/linux/input-event-codes.h>
    pub enum InputEventType {
        /// Used as markers to separate events. Events may be separated in time or in space,
        /// such as with the multitouch protocol.
        Syn = EV_SYN,
        /// Used to describe state changes of keyboards, buttons, or other key-like devices.
        Key = EV_KEY,
        /// Used to describe relative axis value changes, e.g. moving the mouse 5 units
        /// to the left.
        RelAxis = EV_REL,
        /// Used to describe absolute axis value changes, e.g. describing the coordinates
        /// of a touch on a touchscreen.
        AbsAxis = EV_ABS,
        /// Used to describe miscellaneous input data that do not fit into other types.
        Misc = EV_MSC,
        /// Used to describe binary state input switches.
        Switch = EV_SW,
        /// Used to turn LEDs on devices on and off.
        Led = EV_LED,
        /// Used to output sound to devices.
        Sound = EV_SND,
        /// Used for autorepeating devices.
        Repeat = EV_REP,
        /// Used to send force feedback commands to an input device.
        FeedBack = EV_FF,
        /// A special type for power button and switch input.
        Power = EV_PWR,
        /// Used to receive force feedback device status.
        FeedBackStatus = EV_FF_STATUS,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InputEvent {
    pub event_type: InputEventType,
    pub code: u16,
    pub value: i32,
}

#[repr(u16)]
#[derive(Clone, Copy, Debug)]
pub enum CapabilityType {
    Key = EV_KEY,
    RelAxis = EV_REL,
    AbsAxis = EV_ABS,
    Misc = EV_MSC,
    Switch = EV_SW,
    Led = EV_LED,
    Sound = EV_SND,
    FeedBack = EV_FF,
    Event,
    InputProp,
}

pub struct InputCapability {
    /// bitmap to support up to 1024 bits.
    bitmap: [u64; 16],
}

impl InputCapability {
    /// How many codes this bitmap can hold.
    ///
    /// And the reason [`set`](Self::set) and [`contains`](Self::contains) have to
    /// check it: the API takes a `u16`, i.e. 65536 possible codes, against an
    /// array that holds 1024 of them. Every one of the other 64512 indexed
    /// `bitmap[code / 64]` past the end of a 16-word array, which in a kernel is
    /// a panic -- taken from whatever code a driver was handed. Linux's own codes
    /// all fit (`KEY_MAX` is 0x2ff, `FF_MAX` 0x7f) and nothing in this tree
    /// passes a larger one today, but "nothing does today" is not a bound, and
    /// this bitmap is the wire format of `EVIOCGBIT`, whose other side is
    /// userspace.
    pub const BITS: u16 = 1024;

    pub fn empty() -> Self {
        Self { bitmap: [0; 16] }
    }

    /// The word and bit a code lives in, or `None` when it is past [`BITS`](Self::BITS).
    fn at(code: u16) -> Option<(usize, u32)> {
        if code >= Self::BITS {
            return None;
        }
        Some((code as usize / 64, (code % 64) as u32))
    }

    pub fn from_bitmap(bitmap: &[u8]) -> Self {
        let mut cap = Self::empty();
        // `bitmap.len() as u16 * 8` was two silent failures in one expression:
        // the cast truncates a slice longer than 65535, and the multiply
        // overflows a `u16` above 8191 bytes -- which wraps in release, so an
        // 8192-byte bitmap came back completely EMPTY instead of full. Anything
        // over 128 bytes then indexed past the array as well. Count in `usize`,
        // and stop at what this bitmap can actually hold.
        //
        // The `min` is redundant for correctness now that `set` refuses a code it
        // cannot hold -- a mutant that drops it survives every test here, and
        // that is honest. It is not redundant for the log: without it a 200-byte
        // bitmap walks 576 codes past the end and `set` warns about every one.
        let bits = (bitmap.len() * 8).min(Self::BITS as usize);
        for i in 0..bits {
            if bitmap[i / 8] & (1u8 << (i % 8)) != 0 {
                cap.set(i as u16);
            }
        }
        cap
    }

    pub fn set(&mut self, code: u16) {
        match Self::at(code) {
            Some((word, bit)) => self.bitmap[word] |= 1u64 << bit,
            // Dropping the bit is the only answer that keeps the machine up: a
            // capability this bitmap cannot express is one userspace will not
            // see, which costs that feature rather than the kernel.
            None => warn!(
                "[input] capability code {} is past the {}-bit bitmap; dropped",
                code,
                Self::BITS
            ),
        }
    }

    pub fn set_all(&mut self, codes: &[u16]) {
        for &c in codes {
            self.set(c);
        }
    }

    pub fn contains(&self, code: u16) -> bool {
        match Self::at(code) {
            Some((word, bit)) => self.bitmap[word] & (1u64 << bit) != 0,
            None => false,
        }
    }

    pub fn contains_all(&self, codes: &[u16]) -> bool {
        for &c in codes {
            if !self.contains(c) {
                return false;
            }
        }
        true
    }

    /// Serialise the bitmap to little-endian bytes, the layout Linux uses for
    /// the `EVIOCGBIT` family of input ioctls.
    pub fn to_le_bytes(&self) -> [u8; 128] {
        let mut out = [0u8; 128];
        for (i, word) in self.bitmap.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

impl fmt::Debug for InputCapability {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut skip_empty = true;
        write!(f, "[")?;
        for i in (0..16).rev() {
            if self.bitmap[i] > 0 || !skip_empty {
                write!(f, "{:#016x}", self.bitmap[i])?;
                if i > 0 {
                    write!(f, ", ")?;
                }
                skip_empty = false;
            }
        }
        write!(f, "]")?;
        Ok(())
    }
}

/// Linux `struct input_absinfo` fields returned by `EVIOCGABS`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

impl AbsInfo {
    pub fn range(minimum: i32, maximum: i32) -> Self {
        Self {
            value: 0,
            minimum,
            maximum,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        }
    }
}

pub trait InputScheme: Scheme + EventScheme<Event = InputEvent> {
    /// Returns the capability bitmap of the specific kind of event.
    fn capability(&self, cap_type: CapabilityType) -> InputCapability;

    /// Absolute-axis range for `EVIOCGABS`. `None` means the axis is unused.
    fn abs_info(&self, axis: u16) -> Option<AbsInfo> {
        let _ = axis;
        None
    }

    /// Human-readable diagnostic dump for `/proc/usbhid`. Empty for devices
    /// with nothing to report (the default). Used to debug real-hardware
    /// pointer issues from a text VT when no kernel log is reachable.
    fn debug_report(&self) -> alloc::string::String {
        alloc::string::String::new()
    }
}

/// `InputCapability` is the wire format of `EVIOCGBIT`/`EVIOCGPROP`: what this
/// bitmap says is what libinput and libevdev believe the device can do, and a
/// code missing from it means every event carrying that code is **dropped before
/// it reaches the compositor** (the "the mouse wheel does not work" of #1401).
/// It had no tests, and its API took a `u16` -- 65536 codes -- into an array of
/// 1024 bits, with the other 64512 indexing off the end.
#[cfg(test)]
mod capability_tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_code_the_bitmap_holds_round_trips_at_every_word_boundary() {
        // 63/64 and 1023 are where `code / 64` and `code % 64` change word, and
        // 1023 is the last bit that exists at all.
        let mut cap = InputCapability::empty();
        for code in [0u16, 1, 63, 64, 65, 127, 128, 512, 1022, 1023] {
            assert!(!cap.contains(code), "{} was set before anything was", code);
            cap.set(code);
            assert!(cap.contains(code), "{} did not survive being set", code);
        }
        // And nothing else came along with them.
        for code in [2u16, 62, 66, 126, 511, 1021] {
            assert!(!cap.contains(code), "{} was set by a neighbour", code);
        }
    }

    #[test]
    fn a_code_past_the_bitmap_is_dropped_instead_of_taking_the_kernel_down() {
        // `bitmap[code as usize / 64]` on a 16-word array: 1024 indexes word 16,
        // and 65535 indexes word 1023. In the kernel that is a panic, reached
        // from whatever code a driver was handed.
        let mut cap = InputCapability::empty();
        for code in [InputCapability::BITS, 1025, 4096, u16::MAX] {
            cap.set(code);
            assert!(!cap.contains(code), "{} cannot be held, so not held", code);
        }
        assert_eq!(
            cap.to_le_bytes(),
            [0u8; 128],
            "a code that does not fit still changed the bitmap"
        );
    }

    #[test]
    fn the_serialised_form_is_the_little_endian_layout_the_ioctl_expects() {
        // Userspace reads these 128 bytes straight out of `EVIOCGBIT` and tests
        // bit n of byte n/8. Bit 0 is the low bit of byte 0; bit 64 is the low
        // bit of byte 8, not of byte 7 or of the other end.
        for (code, byte, bit) in [
            (0u16, 0usize, 0u32),
            (7, 0, 7),
            (8, 1, 0),
            (63, 7, 7),
            (64, 8, 0),
            (1023, 127, 7),
        ] {
            let mut cap = InputCapability::empty();
            cap.set(code);
            let wire = cap.to_le_bytes();
            assert_eq!(
                wire[byte],
                1u8 << bit,
                "code {} should be byte {} bit {}",
                code,
                byte,
                bit
            );
            assert_eq!(
                wire.iter().map(|b| b.count_ones()).sum::<u32>(),
                1,
                "code {} set more than one bit on the wire",
                code
            );
        }
    }

    #[test]
    fn what_goes_out_on_the_wire_comes_back_as_the_same_capability() {
        // The two directions of the same format, and the reason they have to
        // agree: `from_bitmap` reads bit-per-byte while `to_le_bytes` writes
        // word-per-eight-bytes, so a disagreement about bit order would put
        // every capability in the wrong place and nothing else would notice.
        let codes = [0u16, 1, 30, 63, 64, 272, 273, 767, 1023];
        let mut cap = InputCapability::empty();
        cap.set_all(&codes);
        let back = InputCapability::from_bitmap(&cap.to_le_bytes());
        assert_eq!(back.to_le_bytes(), cap.to_le_bytes());
        assert!(back.contains_all(&codes), "a code was lost on the way back");
        for code in [2u16, 65, 271, 766, 1022] {
            assert!(!back.contains(code), "{} appeared out of nowhere", code);
        }
    }

    #[test]
    fn a_bitmap_longer_than_the_capability_is_taken_up_to_its_limit_and_no_further() {
        // 200 bytes is 1600 bits, and `cap.set(1024)` was an out-of-bounds index.
        // The bitmap a caller hands over is not its own: `virtio/input.rs` sizes
        // it from a byte the DEVICE writes.
        let cap = InputCapability::from_bitmap(&vec![0xffu8; 200]);
        assert!(cap.contains(1023), "the last bit it can hold");
        assert!(!cap.contains(1024), "and not one past it");
        assert_eq!(cap.to_le_bytes(), [0xffu8; 128], "every bit it can hold");
    }

    #[test]
    fn a_bitmap_of_eight_thousand_bytes_is_not_read_as_empty() {
        // `bitmap.len() as u16 * 8` overflows a `u16` above 8191 bytes, so 8192
        // bytes of solid ones produced a bit count of ZERO and an empty
        // capability -- a device that claims everything read as claiming
        // nothing, which is a device whose every event userspace then drops.
        let cap = InputCapability::from_bitmap(&vec![0xffu8; 8192]);
        assert!(cap.contains(0), "the whole bitmap was read as empty");
        assert!(cap.contains(1023));
        assert_eq!(cap.to_le_bytes(), [0xffu8; 128]);
    }

    #[test]
    fn an_empty_bitmap_and_an_empty_capability_are_the_same_thing() {
        assert_eq!(
            InputCapability::from_bitmap(&[]).to_le_bytes(),
            InputCapability::empty().to_le_bytes()
        );
        assert!(!InputCapability::empty().contains(0));
    }

    #[test]
    fn contains_all_wants_every_code_and_an_empty_list_is_no_obstacle() {
        // It gates `mouse.rs`'s "is this a usable pointer" check, so the
        // vacuous case matters: an empty list must not read as a refusal.
        let mut cap = InputCapability::empty();
        cap.set_all(&[1, 2, 3]);
        assert!(
            cap.contains_all(&[]),
            "nothing asked for is nothing missing"
        );
        assert!(cap.contains_all(&[1, 2, 3]));
        assert!(cap.contains_all(&[2]));
        assert!(!cap.contains_all(&[1, 2, 4]), "one missing is all missing");
    }

    #[test]
    fn set_all_sets_each_code_and_leaves_the_rest_alone() {
        let mut cap = InputCapability::empty();
        cap.set_all(&[EV_KEY, EV_REL]);
        assert!(cap.contains(EV_KEY) && cap.contains(EV_REL));
        assert!(!cap.contains(EV_ABS), "an axis type nobody asked for");
    }

    #[test]
    fn an_absolute_axis_range_carries_the_two_numbers_and_nothing_else() {
        // `EVIOCGABS` hands these straight to userspace, and a non-zero `flat`
        // or `fuzz` makes libinput discard small movements near the centre.
        let abs = AbsInfo::range(-32768, 32767);
        assert_eq!((abs.minimum, abs.maximum), (-32768, 32767));
        assert_eq!(
            (abs.value, abs.fuzz, abs.flat, abs.resolution),
            (0, 0, 0, 0)
        );
    }
}

/// Native `#[bench]` rows for the capability bitmap, which is the wire format
/// of `EVIOCGBIT`. `cargo +nightly bench -p zcore-drivers -- input::benches`.
///
/// Nothing here touches hardware or an aperture, so unlike the display rows
/// these figures are exact. What they bound is the per-ioctl cost of
/// publishing a device's capabilities and the per-query cost of asking whether
/// a code is in them -- the question `mouse.rs` asks before it believes a
/// device is a usable pointer.
#[cfg(test)]
mod benches {
    use super::*;
    use test::{black_box, Bencher};

    /// A device's worth of codes, spread across the words so no row measures
    /// one hot cache line: two event types, three buttons, two axes, a wheel.
    const CODES: &[u16] = &[0, 1, 272, 273, 274, 8, 1023, 63, 64];

    fn full() -> InputCapability {
        let mut cap = InputCapability::empty();
        for code in 0..InputCapability::BITS {
            cap.set(code);
        }
        cap
    }

    #[bench]
    fn set_one_code(b: &mut Bencher) {
        let mut cap = InputCapability::empty();
        b.iter(|| cap.set(black_box(273)));
    }

    #[bench]
    fn set_nine_codes(b: &mut Bencher) {
        let mut cap = InputCapability::empty();
        b.iter(|| cap.set_all(black_box(CODES)));
    }

    #[bench]
    fn ask_for_one_code(b: &mut Bencher) {
        let cap = full();
        b.iter(|| black_box(cap.contains(black_box(273))));
    }

    /// A code the bitmap cannot hold: the `at` guard refuses before indexing.
    /// Level with the row above means the bound that keeps an out-of-range
    /// code from panicking the kernel costs nothing.
    #[bench]
    fn ask_for_a_code_past_the_bitmap(b: &mut Bencher) {
        let cap = full();
        b.iter(|| black_box(cap.contains(black_box(u16::MAX))));
    }

    /// What `mouse.rs` asks: nine codes, all present, so the loop runs to the
    /// end.
    #[bench]
    fn ask_for_nine_codes_all_present(b: &mut Bencher) {
        let cap = full();
        b.iter(|| black_box(cap.contains_all(black_box(CODES))));
    }

    /// The same question answered no on the first code, which is the early
    /// exit. The gap to the row above is what the other eight cost.
    #[bench]
    fn ask_for_nine_codes_with_the_first_missing(b: &mut Bencher) {
        let cap = InputCapability::empty();
        b.iter(|| black_box(cap.contains_all(black_box(CODES))));
    }

    /// `EVIOCGBIT` inbound: 128 bytes of solid ones decoded bit by bit, which
    /// is 1024 trips through `set`. This is the whole cost of accepting a
    /// device's capability bitmap, and it is paid once per `(device, event
    /// type)` pair at probe -- never per event.
    #[bench]
    fn read_a_full_128_byte_bitmap(b: &mut Bencher) {
        let wire = [0xffu8; 128];
        b.iter(|| black_box(InputCapability::from_bitmap(black_box(&wire))));
    }

    /// The same 1024 trips with every bit clear, so `set` is never called. The
    /// gap to the row above is what setting 1024 bits costs; this row is what
    /// walking them costs whether they are set or not.
    #[bench]
    fn read_an_empty_128_byte_bitmap(b: &mut Bencher) {
        let wire = [0x00u8; 128];
        b.iter(|| black_box(InputCapability::from_bitmap(black_box(&wire))));
    }

    /// 8192 bytes, which is 65536 bits, clamped to the 1024 this bitmap holds.
    /// Level with `read_a_full_128_byte_bitmap` is the point: the clamp means a
    /// device that reports an enormous bitmap costs the same as one that
    /// reports the right size, instead of walking 64512 codes past the end and
    /// warning about every one. `virtio/input.rs` sizes this from a byte the
    /// DEVICE writes.
    #[bench]
    fn read_an_eight_kilobyte_bitmap(b: &mut Bencher) {
        let wire = [0xffu8; 8192];
        b.iter(|| black_box(InputCapability::from_bitmap(black_box(&wire))));
    }

    #[bench]
    fn read_an_empty_bitmap(b: &mut Bencher) {
        b.iter(|| black_box(InputCapability::from_bitmap(black_box(&[]))));
    }

    /// `EVIOCGBIT` outbound: sixteen words to 128 little-endian bytes.
    #[bench]
    fn write_the_wire_form(b: &mut Bencher) {
        let cap = full();
        b.iter(|| black_box(cap.to_le_bytes()));
    }

    /// The floor for the two `contains` rows, which hand back a `bool`: a call
    /// that decides nothing. A row level with this one is a row that measured
    /// the call and not the work.
    #[bench]
    fn the_capability_floor(b: &mut Bencher) {
        #[inline(never)]
        fn nothing(code: u16) -> bool {
            code != 0
        }
        b.iter(|| black_box(nothing(black_box(273))));
    }
}
