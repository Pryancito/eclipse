//! `udrv-evdev-check NODE [KEYS]`: check an evdev node as a program sees
//! it -- one that `udrv-virtio-input` serves, or one of the kernel's -- and
//! `udrv-evdev-check time NODE [ROUNDS]`: time its ioctls, and
//! `udrv-evdev-check mouse NODE`: check a mouse's node.
//!
//! The check probes the node as libevdev does (version, ids, name, the
//! types and keys it has), grabs it and lets go, sets the clock, and then
//! prints `TYPE KEYS NOW` and waits, 60 s at most, for `KEYS` keys
//! (default 3) to be pressed and let go on the device: whoever runs it
//! types them, a person or QEMU's `input-send-event`. Each event is
//! printed; every frame must end in a `SYN_REPORT`, carry one time, of the
//! clock that was set, and come to a second open as well. Then the LEDs:
//! Caps Lock lit by a write, which the second open reads back.
//!
//! The mouse's check asks for what a mouse has (moves, a left button),
//! prints `MOVE NOW` and waits, 60 s at most, for a move and a click of
//! the left button; every frame must end in a `SYN_REPORT`.
//!
//! `udrv-evdev-check tablet NODE` checks a tablet's the same way: absolute
//! X and Y with their ranges, a left button; it prints `MOVE ABS NOW` and
//! waits for a position and a click.
//!
//! The last line is `PASS N` or `FAIL N of M`; the exit status is 0 only
//! when every check passed.

use std::ffi::CString;
use std::process::exit;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const EV_LED: u16 = 0x11;
const REL_X: u16 = 0;
const ABS_X: u16 = 0;
const BTN_LEFT: u16 = 0x110;
const SYN_REPORT: u16 = 0;
const LED_CAPSL: u16 = 1;
const EVENT_LEN: usize = 24;

const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    dir << 30 | size << 16 | (b'E' as u32) << 8 | nr
}
const R: u32 = 2;
const W: u32 = 1;
const EVIOCGVERSION: u32 = ioc(R, 0x01, 4);
const EVIOCGID: u32 = ioc(R, 0x02, 8);
const EVIOCGNAME: u32 = ioc(R, 0x06, 256);
const EVIOCGPHYS: u32 = ioc(R, 0x07, 256);
const EVIOCGKEY: u32 = ioc(R, 0x18, 96);
const EVIOCGLED: u32 = ioc(R, 0x19, 8);
const EVIOCGBIT_EV: u32 = ioc(R, 0x20, 8);
const EVIOCGBIT_KEY: u32 = ioc(R, 0x21, 96);
const EVIOCGBIT_REL: u32 = ioc(R, 0x22, 8);
const EVIOCGBIT_ABS: u32 = ioc(R, 0x23, 8);
/// `EVIOCGABS(ABS_X)`: its `struct input_absinfo`, six ints.
const EVIOCGABS_X: u32 = ioc(R, 0x40 + ABS_X as u32, 24);
const EVIOCGRAB: u32 = ioc(W, 0x90, 4);
const EVIOCSCLOCKID: u32 = ioc(W, 0xa0, 4);

struct Checks {
    passed: u32,
    failed: u32,
}

impl Checks {
    fn check(&mut self, what: &str, ok: bool) {
        if ok {
            self.passed += 1;
            println!("ok   {what}");
        } else {
            self.failed += 1;
            println!("FAIL {what}");
        }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn open(node: &str, flags: i32) -> i32 {
    let path = CString::new(node).unwrap();
    // SAFETY: a path, NUL-terminated.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        println!("open {node}: {}", std::io::Error::last_os_error());
        exit(1);
    }
    fd
}

/// `ioctl(fd, cmd, buf)`: its value, or `-errno`.
fn ioctl_buf(fd: i32, cmd: u32, buf: &mut [u8]) -> i64 {
    // SAFETY: `buf` is as long as the command says it carries.
    let r = unsafe { libc::ioctl(fd, cmd as _, buf.as_mut_ptr()) };
    if r < 0 {
        -(errno() as i64)
    } else {
        r as i64
    }
}

/// `ioctl(fd, cmd, value)`, for a command that takes its argument by value.
fn ioctl_value(fd: i32, cmd: u32, value: usize) -> i64 {
    // SAFETY: the kernel takes `value` as a number.
    let r = unsafe { libc::ioctl(fd, cmd as _, value) };
    if r < 0 {
        -(errno() as i64)
    } else {
        r as i64
    }
}

fn bit(map: &[u8], code: usize) -> bool {
    map[code / 8] & (1 << (code % 8)) != 0
}

#[derive(Clone, Copy, Debug)]
struct Ev {
    sec: i64,
    usec: i64,
    kind: u16,
    code: u16,
    value: i32,
}

fn events(buf: &[u8]) -> Vec<Ev> {
    buf.chunks(EVENT_LEN)
        .map(|b| Ev {
            sec: i64::from_ne_bytes(b[0..8].try_into().unwrap()),
            usec: i64::from_ne_bytes(b[8..16].try_into().unwrap()),
            kind: u16::from_ne_bytes([b[16], b[17]]),
            code: u16::from_ne_bytes([b[18], b[19]]),
            value: i32::from_ne_bytes(b[20..24].try_into().unwrap()),
        })
        .collect()
}

/// Read what there is, without waiting: the events, or `-errno`.
fn read_now(fd: i32) -> Result<Vec<Ev>, i32> {
    let mut buf = [0u8; EVENT_LEN * 64];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n < 0 {
        return Err(errno());
    }
    Ok(events(&buf[..n as usize]))
}

fn monotonic_us() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a timespec to write.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000 + ts.tv_nsec / 1000
}

fn check(node: &str, keys: usize) -> bool {
    let mut c = Checks {
        passed: 0,
        failed: 0,
    };
    let fd = open(node, libc::O_RDWR | libc::O_NONBLOCK);
    let other = open(node, libc::O_RDONLY | libc::O_NONBLOCK);

    let mut b = [0u8; 256];
    c.check(
        "EVIOCGVERSION is 1.0.1",
        ioctl_buf(fd, EVIOCGVERSION, &mut b) >= 0
            && i32::from_ne_bytes(b[..4].try_into().unwrap()) == 0x01_0001,
    );
    let ids = ioctl_buf(fd, EVIOCGID, &mut b);
    let bus = u16::from_ne_bytes([b[0], b[1]]);
    c.check("EVIOCGID answers", ids >= 0);
    let n = ioctl_buf(fd, EVIOCGNAME, &mut b);
    let name = if n > 0 {
        String::from_utf8_lossy(&b[..n as usize - 1]).into_owned()
    } else {
        String::new()
    };
    c.check(
        "EVIOCGNAME is a name and its NUL",
        n > 1 && b[n as usize - 1] == 0,
    );
    println!("name \"{name}\" bus {bus:#x}");
    let phys = ioctl_buf(fd, EVIOCGPHYS, &mut b);
    c.check(
        "EVIOCGPHYS is a string or ENOENT",
        phys > 0 || phys == -(libc::ENOENT as i64),
    );
    let mut types = [0u8; 8];
    c.check(
        "EVIOCGBIT(0) has EV_SYN and EV_KEY",
        ioctl_buf(fd, EVIOCGBIT_EV, &mut types) > 0
            && bit(&types, EV_SYN as usize)
            && bit(&types, EV_KEY as usize),
    );
    let mut have = [0u8; 96];
    c.check(
        "EVIOCGBIT(EV_KEY) has KEY_A",
        ioctl_buf(fd, EVIOCGBIT_KEY, &mut have) > 0 && bit(&have, 30),
    );
    c.check("EVIOCGRAB(1) by value", ioctl_value(fd, EVIOCGRAB, 1) == 0);
    c.check(
        "a second EVIOCGRAB is EBUSY",
        ioctl_value(other, EVIOCGRAB, 1) == -(libc::EBUSY as i64),
    );
    c.check("EVIOCGRAB(0) lets go", ioctl_value(fd, EVIOCGRAB, 0) == 0);
    let mut mono = 1i32.to_ne_bytes();
    c.check(
        "EVIOCSCLOCKID(CLOCK_MONOTONIC)",
        ioctl_buf(fd, EVIOCSCLOCKID, &mut mono) == 0,
    );
    c.check(
        "a read with nothing there is EAGAIN",
        read_now(fd).err() == Some(libc::EAGAIN),
    );

    println!("TYPE {keys} NOW");
    let mut got: Vec<Ev> = Vec::new();
    let mut frames_whole = true;
    let mut one_time = true;
    let mut near_now = true;
    let mut late_us = 0i64;
    let deadline = monotonic_us() + 60_000_000;
    let key_events = |got: &[Ev]| got.iter().filter(|e| e.kind == EV_KEY).count();
    while key_events(&got) < 2 * keys && monotonic_us() < deadline {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd.
        if unsafe { libc::poll(&mut p, 1, 1000) } <= 0 {
            continue;
        }
        let now = monotonic_us();
        let batch = match read_now(fd) {
            Ok(b) => b,
            Err(e) => {
                println!("read: errno {e}");
                break;
            }
        };
        // Whole frames: the last event read is a SYN_REPORT, and each frame
        // has one time.
        if batch
            .last()
            .is_some_and(|e| e.kind != EV_SYN || e.code != SYN_REPORT)
        {
            frames_whole = false;
        }
        let mut frame_time = None;
        for e in &batch {
            let t = e.sec * 1_000_000 + e.usec;
            if *frame_time.get_or_insert(t) != t {
                one_time = false;
            }
            if e.kind == EV_SYN && e.code == SYN_REPORT {
                frame_time = None;
            }
            if !(0..1_000_000).contains(&(now - t)) {
                near_now = false;
            }
            late_us = late_us.max(now - t);
            if e.kind != EV_SYN {
                println!("event {} {} {}", e.kind, e.code, e.value);
            }
        }
        got.extend(batch);
    }
    let presses = got
        .iter()
        .filter(|e| e.kind == EV_KEY && e.value == 1)
        .count();
    let releases = got
        .iter()
        .filter(|e| e.kind == EV_KEY && e.value == 0)
        .count();
    c.check(
        &format!("{keys} keys pressed and let go ({presses} down, {releases} up)"),
        presses >= keys && releases >= keys,
    );
    c.check("every read ends a frame", frames_whole);
    c.check("each frame has one time", one_time);
    c.check(
        &format!("on the monotonic clock, read within a second ({late_us} us at most)"),
        near_now,
    );
    let theirs: Vec<Ev> = std::iter::from_fn(|| read_now(other).ok().filter(|v| !v.is_empty()))
        .flatten()
        .collect();
    c.check(
        "the second open read the same keys",
        theirs
            .iter()
            .filter(|e| e.kind == EV_KEY)
            .map(|e| (e.code, e.value))
            .eq(got
                .iter()
                .filter(|e| e.kind == EV_KEY)
                .map(|e| (e.code, e.value))),
    );
    let mut down = [0u8; 96];
    c.check(
        "EVIOCGKEY: nothing down",
        ioctl_buf(fd, EVIOCGKEY, &mut down) >= 0 && down.iter().all(|b| *b == 0),
    );

    // Caps Lock, lit by a write, when the device has LEDs.
    if bit(&types, EV_LED as usize) {
        let mut w = Vec::new();
        for (kind, code, value) in [(EV_LED, LED_CAPSL, 1i32), (EV_SYN, SYN_REPORT, 0)] {
            w.extend_from_slice(&[0u8; 16]);
            w.extend_from_slice(&kind.to_ne_bytes());
            w.extend_from_slice(&code.to_ne_bytes());
            w.extend_from_slice(&value.to_ne_bytes());
        }
        // SAFETY: `w` is readable for its length.
        let n = unsafe { libc::write(fd, w.as_ptr() as *const _, w.len()) };
        c.check("a LED written is taken whole", n == w.len() as isize);
        let seen = read_now(other).unwrap_or_default();
        c.check(
            "the other open reads the LED",
            seen.iter()
                .any(|e| e.kind == EV_LED && e.code == LED_CAPSL && e.value == 1),
        );
        let mut leds = [0u8; 8];
        c.check(
            "EVIOCGLED: Caps Lock lit",
            ioctl_buf(other, EVIOCGLED, &mut leds) >= 0 && bit(&leds, LED_CAPSL as usize),
        );
    }
    // SAFETY: descriptors of this process's.
    unsafe {
        libc::close(fd);
        libc::close(other);
    }
    if c.failed == 0 {
        println!("PASS {}", c.passed);
    } else {
        println!("FAIL {} of {}", c.failed, c.passed + c.failed);
    }
    c.failed == 0
}

/// A mouse's check, or with `abs` a tablet's.
fn mouse(node: &str, abs: bool) -> bool {
    let mut c = Checks {
        passed: 0,
        failed: 0,
    };
    let fd = open(node, libc::O_RDONLY | libc::O_NONBLOCK);
    let mut ev = [0u8; 8];
    let (axes, axes_name) = if abs {
        (EV_ABS, "EV_ABS")
    } else {
        (EV_REL, "EV_REL")
    };
    c.check(
        &format!("EVIOCGBIT(0) has EV_KEY and {axes_name}"),
        ioctl_buf(fd, EVIOCGBIT_EV, &mut ev) > 0
            && bit(&ev, EV_KEY as usize)
            && bit(&ev, axes as usize),
    );
    if abs {
        let mut bits = [0u8; 8];
        c.check(
            "EVIOCGBIT(EV_ABS) has ABS_X",
            ioctl_buf(fd, EVIOCGBIT_ABS, &mut bits) > 0 && bit(&bits, ABS_X as usize),
        );
        let mut info = [0u8; 24];
        let n = ioctl_buf(fd, EVIOCGABS_X, &mut info);
        let int = |i: usize| i32::from_ne_bytes(info[i * 4..i * 4 + 4].try_into().unwrap());
        c.check("EVIOCGABS(ABS_X) says a range", n >= 0 && int(2) > int(1));
    } else {
        let mut rel = [0u8; 8];
        c.check(
            "EVIOCGBIT(EV_REL) has REL_X",
            ioctl_buf(fd, EVIOCGBIT_REL, &mut rel) > 0 && bit(&rel, REL_X as usize),
        );
    }
    let mut keys = [0u8; 96];
    c.check(
        "EVIOCGBIT(EV_KEY) has BTN_LEFT",
        ioctl_buf(fd, EVIOCGBIT_KEY, &mut keys) > 0 && bit(&keys, BTN_LEFT as usize),
    );
    println!("{}", if abs { "MOVE ABS NOW" } else { "MOVE NOW" });
    let (mut moved, mut down, mut up, mut unended) = (false, false, false, false);
    let mut open_frame = false;
    let start = monotonic_us();
    while !(moved && down && up) && monotonic_us() - start < 60_000_000 {
        match read_now(fd) {
            Ok(evs) => {
                for e in evs {
                    println!("event {} {:#x} {}", e.kind, e.code, e.value);
                    match (e.kind, e.code, e.value) {
                        (EV_SYN, SYN_REPORT, _) => open_frame = false,
                        (EV_REL, REL_X, v) if v != 0 && !abs => moved = true,
                        (EV_ABS, ABS_X, _) if abs => moved = true,
                        (EV_KEY, BTN_LEFT, 1) => down = true,
                        (EV_KEY, BTN_LEFT, 0) => up = down,
                        _ => {}
                    }
                    if e.kind != EV_SYN {
                        open_frame = true;
                    }
                }
                unended |= open_frame;
            }
            Err(libc::EAGAIN) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(e) => {
                println!("read: errno {e}");
                break;
            }
        }
    }
    c.check("a move came", moved);
    c.check("the left button went down and up", down && up);
    c.check("every frame ended in SYN_REPORT", !unended);
    if c.failed == 0 {
        println!("PASS {}", c.passed);
    } else {
        println!("FAIL {} of {}", c.failed, c.passed + c.failed);
    }
    c.failed == 0
}

fn time(node: &str, rounds: u32) {
    let fd = open(node, libc::O_RDONLY | libc::O_NONBLOCK);
    let mut b = [0u8; 256];
    let run = |cmd: u32, b: &mut [u8]| {
        let t = monotonic_us();
        for _ in 0..rounds {
            if ioctl_buf(fd, cmd, b) < 0 {
                println!("ioctl {cmd:#x}: errno {}", errno());
                exit(1);
            }
        }
        (monotonic_us() - t) as f64 / rounds as f64
    };
    let version = run(EVIOCGVERSION, &mut b);
    let name = run(EVIOCGNAME, &mut b);
    let keys = run(EVIOCGBIT_KEY, &mut b);
    // An empty read: what a poll loop costs when nothing came.
    let t = monotonic_us();
    for _ in 0..rounds {
        let _ = read_now(fd);
    }
    let empty = (monotonic_us() - t) as f64 / rounds as f64;
    println!(
        "{node}: EVIOCGVERSION {version:.1} us, EVIOCGNAME {name:.1} us, EVIOCGBIT(KEY) {keys:.1} us, empty read {empty:.1} us ({rounds} rounds)"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: udrv-evdev-check NODE [KEYS] | udrv-evdev-check time NODE [ROUNDS] \
                 | udrv-evdev-check mouse NODE | udrv-evdev-check tablet NODE";
    match args.get(1).map(String::as_str) {
        Some(kind @ ("mouse" | "tablet")) => match args.get(2) {
            Some(node) if mouse(node, kind == "tablet") => {}
            Some(_) => exit(1),
            None => {
                println!("{usage}");
                exit(1)
            }
        },
        Some("time") => {
            let Some(node) = args.get(2) else {
                println!("{usage}");
                exit(1)
            };
            time(
                node,
                args.get(3).and_then(|r| r.parse().ok()).unwrap_or(2000),
            );
        }
        Some(node) => {
            let keys = args.get(2).and_then(|k| k.parse().ok()).unwrap_or(3);
            if !check(node, keys) {
                exit(1);
            }
        }
        None => {
            println!("{usage}");
            exit(1);
        }
    }
}
