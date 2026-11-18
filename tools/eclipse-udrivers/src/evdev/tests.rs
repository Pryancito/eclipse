use super::*;

const KEY_A: u16 = 30;
const KEY_B: u16 = 48;
const LED_CAPSL: u16 = 1;
const ABS_X: u16 = 0;

/// A clock that reads one second later each time, on both: monotonic at
/// `n`, the wall clock at `1000 + n`.
fn ticking() -> [Time; 2] {
    use std::sync::atomic::{AtomicI64, Ordering};
    static N: AtomicI64 = AtomicI64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    [
        Time { sec: n, usec: 0 },
        Time {
            sec: 1000 + n,
            usec: 0,
        },
    ]
}

fn map(codes: &[u16], len: usize) -> Vec<u8> {
    let mut m = vec![0u8; len];
    for c in codes {
        set_bit(&mut m, *c as usize, true);
    }
    m
}

fn keyboard() -> Evdev {
    let mut info = Info {
        name: "QEMU Virtio Keyboard".into(),
        id: [0x06, 0x0627, 0x0001, 0x0001],
        ..Info::default()
    };
    info.bits.insert(EV_KEY, map(&[KEY_A, KEY_B], 96));
    info.bits.insert(EV_LED, map(&[LED_CAPSL], 8));
    info.bits.insert(EV_REP, vec![]);
    Evdev::new(info, ticking)
}

fn tablet() -> Evdev {
    let mut info = Info {
        name: "QEMU Virtio Tablet".into(),
        ..Info::default()
    };
    info.bits.insert(EV_ABS, map(&[ABS_X], 8));
    info.abs.insert(ABS_X, [0, 0, 32767, 0, 0, 0]);
    Evdev::new(info, ticking)
}

fn press(dev: &mut Evdev, key: u16, value: i32) {
    dev.feed(EV_KEY, key, value);
    dev.feed(EV_SYN, SYN_REPORT, 0);
}

fn events(dev: &mut Evdev, fh: u32) -> Vec<Event> {
    let mut buf = [0u8; EVENT_LEN * 64];
    match dev.read(fh, &mut buf) {
        Ok(n) => buf[..n].chunks(EVENT_LEN).map(Event::from_bytes).collect(),
        Err(EAGAIN) => Vec::new(),
        Err(e) => panic!("read: {e}"),
    }
}

fn kinds(evs: &[Event]) -> Vec<(u16, u16, i32)> {
    evs.iter().map(|e| (e.kind, e.code, e.value)).collect()
}

fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    dir << 30 | size << 16 | (b'E' as u32) << 8 | nr
}

#[test]
fn an_event_is_the_struct_linux_reads() {
    let e = Event {
        time: Time { sec: 5, usec: 7 },
        kind: EV_KEY,
        code: KEY_A,
        value: 1,
    };
    let b = e.to_bytes();
    assert_eq!(&b[0..8], &5i64.to_ne_bytes());
    assert_eq!(&b[8..16], &7i64.to_ne_bytes());
    assert_eq!(&b[16..18], &1u16.to_ne_bytes());
    assert_eq!(&b[18..20], &30u16.to_ne_bytes());
    assert_eq!(&b[20..24], &1i32.to_ne_bytes());
    assert_eq!(Event::from_bytes(&b), e);
}

#[test]
fn each_open_reads_every_frame_with_one_time() {
    let mut dev = keyboard();
    let (a, mask) = dev.open();
    let (b, _) = dev.open();
    assert_eq!(mask, POLLOUT, "nothing to read yet");
    press(&mut dev, KEY_A, 1);
    let got = events(&mut dev, a);
    assert_eq!(kinds(&got), [(EV_KEY, KEY_A, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(got[0].time, got[1].time, "the frame's time");
    assert_eq!(events(&mut dev, b).len(), 2, "the other open has its own");
    assert!(events(&mut dev, a).is_empty());
}

#[test]
fn half_a_frame_is_not_there_to_read() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    dev.feed(EV_KEY, KEY_A, 1);
    assert_eq!(dev.read(fh, &mut [0; 48]), Err(EAGAIN));
    dev.feed(EV_SYN, SYN_REPORT, 0);
    assert_eq!(dev.read(fh, &mut [0; 48]), Ok(48));
}

#[test]
fn readiness_is_told_when_it_changes_and_only_then() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    dev.take_told();
    dev.feed(EV_KEY, KEY_A, 1);
    assert!(dev.take_told().is_empty(), "half a frame");
    dev.feed(EV_SYN, SYN_REPORT, 0);
    assert_eq!(dev.take_told(), [(fh, POLLIN | POLLOUT)]);
    press(&mut dev, KEY_A, 0);
    assert!(dev.take_told().is_empty(), "already readable");
    // A read that leaves a frame keeps it readable.
    assert_eq!(dev.read(fh, &mut [0; 48]), Ok(48));
    assert!(dev.take_told().is_empty());
    assert_eq!(dev.read(fh, &mut [0; 480]), Ok(48));
    assert_eq!(dev.take_told(), [(fh, POLLOUT)]);
}

#[test]
fn a_key_already_down_going_down_is_no_event() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    press(&mut dev, KEY_A, 1);
    press(&mut dev, KEY_A, 1);
    press(&mut dev, KEY_A, 2); // the device's repeat, while down
    press(&mut dev, KEY_A, 0);
    press(&mut dev, KEY_A, 2); // a repeat of a key that is up
    let got = kinds(&events(&mut dev, fh));
    assert_eq!(
        got,
        [
            (EV_KEY, KEY_A, 1),
            (EV_SYN, SYN_REPORT, 0),
            (EV_KEY, KEY_A, 2),
            (EV_SYN, SYN_REPORT, 0),
            (EV_KEY, KEY_A, 0),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn a_code_the_device_does_not_have_is_dropped() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    assert!(!dev.feed(EV_KEY, 31, 1));
    assert!(!dev.feed(EV_REL, 0, 3));
    dev.feed(EV_SYN, SYN_REPORT, 0);
    assert!(events(&mut dev, fh).is_empty(), "an empty frame is none");
}

#[test]
fn an_axis_that_does_not_move_is_no_event() {
    let mut dev = tablet();
    let (fh, _) = dev.open();
    dev.feed(EV_ABS, ABS_X, 100);
    dev.feed(EV_SYN, SYN_REPORT, 0);
    dev.feed(EV_ABS, ABS_X, 100);
    dev.feed(EV_SYN, SYN_REPORT, 0);
    assert_eq!(events(&mut dev, fh).len(), 2);
    let mut abs = [0u8; 24];
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_READ, 0x40 + ABS_X as u32, 24), 0, &mut abs),
        Ok((0, 24))
    );
    assert_eq!(&abs[0..4], &100i32.to_ne_bytes(), "the value now");
    assert_eq!(&abs[8..12], &32767i32.to_ne_bytes(), "its maximum");
}

#[test]
fn an_open_that_falls_behind_is_told_it_lost_its_queue() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    for _ in 0..QUEUE_LEN {
        press(&mut dev, KEY_A, 1);
        press(&mut dev, KEY_A, 0);
    }
    let got = events(&mut dev, fh);
    assert_eq!((got[0].kind, got[0].code), (EV_SYN, SYN_DROPPED));
    assert!(got.len() < QUEUE_LEN);
    // What follows the SYN_DROPPED is whole frames.
    assert_eq!(kinds(&got[got.len() - 1..]), [(EV_SYN, SYN_REPORT, 0)]);
}

#[test]
fn a_read_takes_whole_events_and_no_fewer_than_one() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    press(&mut dev, KEY_A, 1);
    assert_eq!(dev.read(fh, &mut [0; 10]), Err(EINVAL));
    assert_eq!(dev.read(fh, &mut [0; 0]), Ok(0));
    assert_eq!(dev.read(fh, &mut [0; 30]), Ok(24));
    assert_eq!(dev.read(fh, &mut [0; 30]), Ok(24));
    assert_eq!(dev.read(fh, &mut [0; 30]), Err(EAGAIN));
}

#[test]
fn each_open_has_its_own_clock() {
    let mut dev = keyboard();
    let (mono, _) = dev.open();
    let (real, _) = dev.open();
    let one = 1i32.to_ne_bytes();
    let zero = 0i32.to_ne_bytes();
    assert_eq!(
        dev.ioctl(real, ioc(IOC_WRITE, 0xa0, 4), 0, &mut zero.clone()),
        Ok((0, 0))
    );
    assert_eq!(
        dev.ioctl(mono, ioc(IOC_WRITE, 0xa0, 4), 0, &mut one.clone()),
        Ok((0, 0))
    );
    assert_eq!(
        dev.ioctl(mono, ioc(IOC_WRITE, 0xa0, 4), 0, &mut 5i32.to_ne_bytes()),
        Err(EINVAL)
    );
    press(&mut dev, KEY_A, 1);
    let m = events(&mut dev, mono)[0].time.sec;
    let r = events(&mut dev, real)[0].time.sec;
    assert_eq!(r - m, 1000, "the same moment, on the other clock");
}

#[test]
fn a_clock_changed_with_events_queued_drops_them_and_says_so() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    press(&mut dev, KEY_A, 1);
    dev.ioctl(fh, ioc(IOC_WRITE, 0xa0, 4), 0, &mut 0i32.to_ne_bytes())
        .unwrap();
    assert!(events(&mut dev, fh).is_empty(), "not a frame yet");
    press(&mut dev, KEY_A, 0);
    let got = kinds(&events(&mut dev, fh));
    assert_eq!(
        got,
        [
            (EV_SYN, SYN_DROPPED, 0),
            (EV_KEY, KEY_A, 0),
            (EV_SYN, SYN_REPORT, 0)
        ]
    );
}

#[test]
fn a_grab_gives_one_open_every_event() {
    let mut dev = keyboard();
    let (a, _) = dev.open();
    let (b, _) = dev.open();
    let grab = ioc(IOC_WRITE, 0x90, 4);
    assert!(by_value(grab));
    assert_eq!(dev.ioctl(a, grab, 1, &mut []), Ok((0, 0)));
    assert_eq!(dev.ioctl(b, grab, 1, &mut []), Err(EBUSY));
    assert_eq!(dev.ioctl(b, grab, 0, &mut []), Err(EINVAL), "not b's");
    press(&mut dev, KEY_A, 1);
    assert_eq!(events(&mut dev, a).len(), 2);
    assert!(events(&mut dev, b).is_empty());
    assert_eq!(dev.ioctl(a, grab, 0, &mut []), Ok((0, 0)));
    press(&mut dev, KEY_A, 0);
    assert_eq!(events(&mut dev, b).len(), 2);
    // A grab ends with its open.
    dev.ioctl(a, grab, 1, &mut []).unwrap();
    dev.release(a);
    press(&mut dev, KEY_A, 1);
    assert_eq!(events(&mut dev, b).len(), 2);
}

#[test]
fn a_revoked_open_is_done_for() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    press(&mut dev, KEY_A, 1);
    dev.take_told();
    let revoke = ioc(IOC_WRITE, 0x91, 4);
    assert!(by_value(revoke));
    assert_eq!(dev.ioctl(fh, revoke, 1, &mut []), Err(EINVAL));
    assert_eq!(dev.ioctl(fh, revoke, 0, &mut []), Ok((0, 0)));
    assert_eq!(dev.take_told(), [(fh, POLLHUP | POLLERR)]);
    assert_eq!(dev.read(fh, &mut [0; 48]), Err(ENODEV));
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_READ, 1, 4), 0, &mut [0; 4]),
        Err(ENODEV)
    );
    assert!(
        !by_value(ioc(IOC_WRITE, 0xa0, 4)),
        "EVIOCSCLOCKID is a pointer"
    );
}

#[test]
fn the_probe_of_a_client_is_answered_as_linux_answers_it() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    let mut d = [0u8; 256];
    assert_eq!(dev.ioctl(fh, ioc(IOC_READ, 0x01, 4), 0, &mut d), Ok((0, 4)));
    assert_eq!(&d[..4], &0x01_0001i32.to_ne_bytes());
    assert_eq!(dev.ioctl(fh, ioc(IOC_READ, 0x02, 8), 0, &mut d), Ok((0, 8)));
    assert_eq!(&d[..4], &[0x06, 0, 0x27, 0x06]);
    // EVIOCGNAME(len): the name and its NUL, cut to the buffer.
    let n = dev.ioctl(fh, ioc(IOC_READ, 0x06, 256), 0, &mut d).unwrap();
    assert_eq!(n, (21, 21));
    assert_eq!(&d[..21], b"QEMU Virtio Keyboard\0");
    assert_eq!(dev.ioctl(fh, ioc(IOC_READ, 0x06, 4), 0, &mut d), Ok((4, 4)));
    // No physical place, no serial: ENOENT, which libevdev takes.
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_READ, 0x07, 64), 0, &mut d),
        Err(ENOENT)
    );
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_READ, 0x08, 64), 0, &mut d),
        Err(ENOENT)
    );
    // EVIOCGBIT(0): the types, SYN with them, and EV_REP without codes.
    assert_eq!(dev.ioctl(fh, ioc(IOC_READ, 0x20, 4), 0, &mut d), Ok((4, 4)));
    let types = 1u32 << EV_SYN | 1 << EV_KEY | 1 << EV_LED | 1 << EV_REP;
    assert_eq!(&d[..4], &types.to_le_bytes());
    // EVIOCGBIT(EV_KEY): a long-whole bitmap, cut to the buffer.
    let got = dev.ioctl(fh, ioc(IOC_READ, 0x21, 256), 0, &mut d).unwrap();
    assert_eq!(got, (96, 96));
    assert!(bit(&d, KEY_A as usize) && bit(&d, KEY_B as usize));
    assert!(!bit(&d, 31));
    // A type EVIOCGBIT does not answer.
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_READ, 0x20 + EV_REP as u32, 8), 0, &mut d),
        Err(EINVAL)
    );
    // Nothing of another ioctl group.
    assert_eq!(dev.ioctl(fh, 0x5401, 0, &mut d), Err(EINVAL));
}

#[test]
fn what_is_down_is_what_eviocgkey_answers() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    press(&mut dev, KEY_B, 1);
    let mut d = [0u8; 96];
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_READ, 0x18, 96), 0, &mut d),
        Ok((96, 96))
    );
    assert!(bit(&d, KEY_B as usize) && !bit(&d, KEY_A as usize));
    press(&mut dev, KEY_B, 0);
    dev.ioctl(fh, ioc(IOC_READ, 0x18, 96), 0, &mut d).unwrap();
    assert!(!bit(&d, KEY_B as usize));
}

#[test]
fn the_repeat_set_is_the_repeat_read_back() {
    let mut dev = keyboard();
    let (fh, _) = dev.open();
    let mut d = [0u8; 8];
    d[..4].copy_from_slice(&500u32.to_ne_bytes());
    d[4..].copy_from_slice(&40u32.to_ne_bytes());
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_WRITE, 0x03, 8), 0, &mut d.clone()),
        Ok((0, 0))
    );
    let mut back = [0u8; 8];
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_READ, 0x03, 8), 0, &mut back),
        Ok((0, 8))
    );
    assert_eq!(back, d);
}

#[test]
fn what_is_written_goes_to_every_reader_and_a_led_to_the_device() {
    let mut dev = keyboard();
    let (a, _) = dev.open();
    let (b, _) = dev.open();
    let led = |v| Event {
        kind: EV_LED,
        code: LED_CAPSL,
        value: v,
        ..Event::default()
    };
    let syn = Event {
        kind: EV_SYN,
        code: SYN_REPORT,
        ..Event::default()
    };
    let mut data = Vec::new();
    for e in [led(1), syn] {
        data.extend_from_slice(&e.to_bytes());
    }
    data.extend_from_slice(&[0; 5]); // a part of an event: not taken
    assert_eq!(dev.write(a, &data), Ok(48));
    assert_eq!(dev.take_outgoing(), [(EV_LED, LED_CAPSL, 1)]);
    assert_eq!(
        kinds(&events(&mut dev, b)),
        [(EV_LED, LED_CAPSL, 1), (EV_SYN, SYN_REPORT, 0)]
    );
    // Lit already: nothing to tell the device.
    dev.write(a, &led(1).to_bytes()).unwrap();
    assert!(dev.take_outgoing().is_empty());
    assert_eq!(dev.write(a, &[0; 10]), Err(EINVAL));
    let mut d = [0u8; 8];
    dev.ioctl(b, ioc(IOC_READ, 0x19, 8), 0, &mut d).unwrap();
    assert!(bit(&d, LED_CAPSL as usize), "EVIOCGLED: lit");
}

#[test]
fn an_absinfo_is_set_only_for_an_axis_the_device_has() {
    let mut dev = tablet();
    let (fh, _) = dev.open();
    let mut d = [0u8; 24];
    d[8..12].copy_from_slice(&1000i32.to_ne_bytes());
    assert_eq!(
        dev.ioctl(
            fh,
            ioc(IOC_WRITE, 0xc0 + ABS_X as u32, 24),
            0,
            &mut d.clone()
        ),
        Ok((0, 0))
    );
    assert_eq!(dev.info().abs[&ABS_X][2], 1000);
    assert_eq!(
        dev.ioctl(fh, ioc(IOC_WRITE, 0xc1, 24), 0, &mut d),
        Err(EINVAL)
    );
}
