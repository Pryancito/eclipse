//! `udrv-dsp-check [NODE] [SECONDS]`: play a 1 kHz tone through an OSS
//! node (default `/dev/dsp`), as a program does, and check what it says on
//! the way: the format, rate and channels set, the room and the delay it
//! reports, a non-blocking write that finds no room, and blocking writes
//! that keep the pace of the sound (`SECONDS`, default 2, take about that
//! long). Run against a driver that serves the node (`udrv-hda`), or the
//! kernel's.
//!
//! The tone is 16-bit stereo at 48 kHz, amplitude 8000, left and right the
//! same: what the host hears (QEMU's `-audiodev wav`) can be checked for
//! it. The last line is `PASS N` or `FAIL N of M`.

use std::ffi::CString;
use std::process::exit;
use std::time::Instant;

const SNDCTL_DSP_SPEED: u64 = 0xc004_5002;
const SNDCTL_DSP_SETFMT: u64 = 0xc004_5005;
const SNDCTL_DSP_CHANNELS: u64 = 0xc004_5006;
const SNDCTL_DSP_GETOSPACE: u64 = 0x8010_500c;
const SNDCTL_DSP_GETODELAY: u64 = 0x8004_5017;
const SNDCTL_DSP_GETBLKSIZE: u64 = 0xc004_5004;
const OSS_GETVERSION: u64 = 0x8004_4d76;
const AFMT_S16_LE: i32 = 0x10;
const RATE: usize = 48000;
const AMPLITUDE: f64 = 8000.0;

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

fn int_ioctl(fd: i32, cmd: u64, v: i32) -> Option<i32> {
    let mut v = v;
    // SAFETY: an int the call reads and writes.
    let r = unsafe { libc::ioctl(fd, cmd as _, &mut v as *mut i32) };
    (r >= 0).then_some(v)
}

fn ospace(fd: i32) -> Option<[i32; 4]> {
    let mut info = [0i32; 4];
    // SAFETY: an audio_buf_info, four ints.
    let r = unsafe { libc::ioctl(fd, SNDCTL_DSP_GETOSPACE as _, info.as_mut_ptr()) };
    (r >= 0).then_some(info)
}

fn write_all(fd: i32, mut data: &[u8]) -> Result<(), i32> {
    while !data.is_empty() {
        // SAFETY: `data` is readable for its length.
        let n = unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
        if n < 0 {
            return Err(errno());
        }
        data = &data[n as usize..];
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let node = args.get(1).map(String::as_str).unwrap_or("/dev/dsp");
    let seconds: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2);
    let mut c = Checks {
        passed: 0,
        failed: 0,
    };
    let path = CString::new(node).unwrap();
    // SAFETY: a path.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY) };
    if fd < 0 {
        println!("open {node}: errno {}", errno());
        println!("FAIL 1 of 1");
        exit(1);
    }
    c.check(
        "OSS_GETVERSION answers",
        int_ioctl(fd, OSS_GETVERSION, 0).is_some_and(|v| v > 0),
    );
    // SAFETY: as above.
    let second = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY) };
    // A driver that plays one stream refuses a second open; the kernel's,
    // which mixes, gives it a stream of its own.
    let busy = second < 0 && errno() == libc::EBUSY;
    if second >= 0 {
        println!("a second open is a stream of its own: this node mixes");
        // SAFETY: the open just made.
        unsafe { libc::close(second) };
    }
    c.check(
        "a second open is EBUSY or a stream of its own",
        busy || second >= 0,
    );
    c.check(
        "SETFMT S16_LE",
        int_ioctl(fd, SNDCTL_DSP_SETFMT, AFMT_S16_LE) == Some(AFMT_S16_LE),
    );
    c.check(
        "CHANNELS 2",
        int_ioctl(fd, SNDCTL_DSP_CHANNELS, 2) == Some(2),
    );
    c.check(
        "SPEED 48000",
        int_ioctl(fd, SNDCTL_DSP_SPEED, RATE as i32) == Some(RATE as i32),
    );
    let block = int_ioctl(fd, SNDCTL_DSP_GETBLKSIZE, 0).unwrap_or(0);
    c.check("GETBLKSIZE is a fragment", block > 0);
    let space = ospace(fd).unwrap_or_default();
    println!(
        "space: {} of {} fragments of {} bytes, {} bytes free",
        space[0], space[1], space[2], space[3]
    );
    c.check(
        "GETOSPACE says the room of an empty buffer",
        space[2] == block && space[3] > 0 && space[0] == space[3] / space[2],
    );

    // The tone, one second of it.
    let mut tone = Vec::with_capacity(RATE * 4);
    for i in 0..RATE {
        let s = (AMPLITUDE * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / RATE as f64).sin())
            as i16;
        tone.extend_from_slice(&s.to_le_bytes());
        tone.extend_from_slice(&s.to_le_bytes());
    }

    let start = Instant::now();
    let mut ok = true;
    for _ in 0..seconds {
        if let Err(e) = write_all(fd, &tone) {
            println!("write: errno {e}");
            ok = false;
            break;
        }
    }
    let wrote = start.elapsed();
    c.check(&format!("{seconds} s of tone written"), ok);
    let delay = int_ioctl(fd, SNDCTL_DSP_GETODELAY, 0).unwrap_or(-1);
    let buffered = delay as f64 / (RATE * 4) as f64;
    println!(
        "written in {:.3} s, {delay} bytes ({buffered:.3} s) still to play",
        wrote.as_secs_f64()
    );
    // Blocking writes wait for the sound: what went in, less what is still
    // queued, took its own time to play (a tenth of a second each way).
    let played = seconds as f64 - buffered;
    c.check(
        "blocking writes keep the pace of the sound",
        delay >= 0 && (wrote.as_secs_f64() - played).abs() < 0.1 + 0.05 * seconds as f64,
    );
    // A full buffer and a non-blocking write: EAGAIN, nothing taken.
    // SAFETY: the open's flags.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let mut again = false;
    for _ in 0..64 {
        // SAFETY: `tone` is readable.
        let n = unsafe { libc::write(fd, tone.as_ptr() as *const _, tone.len()) };
        if n < 0 {
            again = errno() == libc::EAGAIN;
            break;
        }
    }
    c.check("a non-blocking write with no room is EAGAIN", again);
    // Then the buffer plays out.
    let full = int_ioctl(fd, SNDCTL_DSP_GETODELAY, 0).unwrap_or(0);
    std::thread::sleep(std::time::Duration::from_millis(150));
    let later = int_ioctl(fd, SNDCTL_DSP_GETODELAY, 0).unwrap_or(0);
    println!("delay {full} bytes, {later} bytes 150 ms later");
    c.check(
        "the delay goes down as it plays",
        later < full && full - later > (RATE * 4 / 10) as i32,
    );
    // SAFETY: the open.
    unsafe { libc::close(fd) };
    if c.failed == 0 {
        println!("PASS {}", c.passed);
    } else {
        println!("FAIL {} of {}", c.failed, c.passed + c.failed);
        exit(1);
    }
}
