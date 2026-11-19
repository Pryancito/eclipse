//! `udrv-snd-check [CARD] [SECONDS]`: play a 1 kHz tone through ALSA's
//! `/dev/snd/pcmC<CARD>D0p` (default card 0) the way alsa-lib's `hw` plugin
//! does, with the ioctls it makes, and check what the nodes say on the
//! way: the control's card and its two elements, the refine and the
//! parameters chosen, blocking writes that keep the pace of the sound
//! (`SECONDS`, default 2), a drain that waits for it to play, a write
//! with no room on an open that does not wait, and an underrun that
//! `PREPARE` recovers. Run against a driver that serves the nodes
//! (`udrv-hda`), or the kernel's.
//!
//! The tone is 16-bit stereo at 48 kHz, amplitude 8000, left and right the
//! same, as `udrv-dsp-check`'s; what else is written is silence, so what
//! the host hears (QEMU's `-audiodev wav`) is the tone alone. The last line
//! is `PASS N` or `FAIL N of M`.

use std::ffi::CString;
use std::process::exit;
use std::time::{Duration, Instant};

use udrivers::alsa::{
    get, zeroed, SndCtlCardInfo, SndCtlElemId, SndCtlElemList, SndPcmHwParams, SndPcmInfo,
    SndPcmStatus, SndPcmSwParams, SndPcmSyncPtr, SndXferI, IV_BUFFER_SIZE, IV_PERIODS,
    IV_PERIOD_SIZE, IV_RATE, STATE_PREPARED, STATE_RUNNING, STATE_SETUP, STATE_XRUN,
};

const RATE: u64 = 48000;
const PERIOD: u64 = 1024;
const PERIODS: u64 = 8;
const AMPLITUDE: f64 = 8000.0;

fn ioc(dir: u64, ty: u8, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr
}
const R: u64 = 2;
const W: u64 = 1;
const RW: u64 = 3;

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

/// `ioctl(fd, cmd, buf)`: 0 or the `errno`.
fn call(fd: i32, cmd: u64, buf: *mut u8) -> i32 {
    // SAFETY: `buf` is as long as `cmd` says, or more.
    let r = unsafe { libc::ioctl(fd, cmd as _, buf) };
    if r < 0 {
        errno()
    } else {
        0
    }
}

/// An `ioctl` carrying `T` both ways (or in, or out, as `dir` says).
fn with<T: Copy>(fd: i32, dir: u64, ty: u8, nr: u64, v: &mut T) -> i32 {
    call(
        fd,
        ioc(dir, ty, nr, core::mem::size_of::<T>()),
        v as *mut T as *mut u8,
    )
}

fn bare(fd: i32, nr: u64) -> i32 {
    call(fd, ioc(0, b'A', nr, 0), std::ptr::null_mut())
}

fn open(path: &str, flags: i32) -> i32 {
    let p = CString::new(path).unwrap();
    // SAFETY: a path.
    unsafe { libc::open(p.as_ptr(), flags) }
}

fn any() -> SndPcmHwParams {
    let mut p: SndPcmHwParams = zeroed();
    for m in p.masks.iter_mut() {
        m.bits = [!0; 8];
    }
    for iv in p.intervals.iter_mut() {
        iv.min = 0;
        iv.max = u32::MAX;
    }
    p.rmask = !0;
    p
}

fn pin(p: &mut SndPcmHwParams, idx: usize, v: u32) {
    p.intervals[idx].min = v;
    p.intervals[idx].max = v;
    p.intervals[idx].flags = 0;
}

/// `WRITEI_FRAMES` of `pcm`: the frames taken, or the `errno`.
fn writei(fd: i32, pcm: &[u8]) -> Result<u64, i32> {
    let mut x = SndXferI {
        result: 0,
        buf: pcm.as_ptr() as u64,
        frames: pcm.len() as u64 / 4,
    };
    match with(fd, W, b'A', 0x50, &mut x) {
        0 => Ok(x.result as u64),
        e => Err(e),
    }
}

/// All of `pcm`, as a program loops on `snd_pcm_writei`.
fn write_all(fd: i32, mut pcm: &[u8]) -> Result<(), i32> {
    while !pcm.is_empty() {
        let n = writei(fd, pcm)?;
        pcm = &pcm[(n * 4) as usize..];
    }
    Ok(())
}

/// The stream's boundary, which a start threshold at it never reaches.
fn sw_boundary(fd: i32, buffer: u64) -> u64 {
    let mut sw: SndPcmSwParams = zeroed();
    sw.avail_min = PERIOD;
    sw.start_threshold = 1;
    sw.stop_threshold = buffer;
    with(fd, RW, b'A', 0x13, &mut sw);
    sw.boundary
}

fn sync_ptr(fd: i32) -> SndPcmSyncPtr {
    let mut sp: SndPcmSyncPtr = zeroed();
    // As alsa-lib after a write: hand back the pointer and avail_min.
    sp.flags = 2 | 4;
    with(fd, RW, b'A', 0x23, &mut sp);
    sp
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let card: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2);
    let mut c = Checks {
        passed: 0,
        failed: 0,
    };

    // The control, as alsa-lib opens it first.
    let ctl_path = format!("/dev/snd/controlC{card}");
    let ctl = open(&ctl_path, libc::O_RDWR);
    if ctl < 0 {
        println!("open {ctl_path}: errno {}", errno());
        println!("FAIL 1 of 1");
        exit(1);
    }
    let mut v = 0i32;
    c.check(
        "the control's version",
        with(ctl, R, b'U', 0x00, &mut v) == 0 && v >> 16 == 2,
    );
    let mut info: SndCtlCardInfo = zeroed();
    let r = with(ctl, R, b'U', 0x01, &mut info);
    let driver = String::from_utf8_lossy(&info.driver)
        .trim_end_matches('\0')
        .to_string();
    println!("card {}: driver {driver}", info.card);
    c.check("CARD_INFO names a driver", r == 0 && !driver.is_empty());
    let mut list: SndCtlElemList = zeroed();
    let r = with(ctl, RW, b'U', 0x10, &mut list);
    let count = list.count;
    let mut ids = vec![[0u8; 64]; count as usize];
    list.space = count;
    list.pids = ids.as_mut_ptr() as u64;
    let r2 = with(ctl, RW, b'U', 0x10, &mut list);
    let names: Vec<String> = ids
        .iter()
        .map(|b| {
            let id: SndCtlElemId = get(b).unwrap();
            String::from_utf8_lossy(&id.name)
                .trim_end_matches('\0')
                .to_string()
        })
        .collect();
    println!("elements: {names:?}");
    c.check(
        "ELEM_LIST writes the ids where it points",
        r == 0
            && r2 == 0
            && list.used == count
            && names.iter().any(|n| n == "Master Playback Volume"),
    );
    let mut pi: SndPcmInfo = zeroed();
    pi.stream = 1;
    c.check(
        "no capture: PCM_INFO of it is ENOENT",
        with(ctl, RW, b'U', 0x31, &mut pi) == libc::ENOENT,
    );

    let pcm_path = format!("/dev/snd/pcmC{card}D0p");
    let fd = open(&pcm_path, libc::O_RDWR);
    if fd < 0 {
        println!("open {pcm_path}: errno {}", errno());
        println!("FAIL {} of {}", c.failed + 1, c.passed + c.failed + 1);
        exit(1);
    }
    let second = open(&pcm_path, libc::O_RDWR);
    let busy = second < 0 && errno() == libc::EBUSY;
    if second >= 0 {
        println!("a second open is a stream of its own: this card mixes");
        // SAFETY: the open just made.
        unsafe { libc::close(second) };
    }
    c.check(
        "a second open is EBUSY or a stream of its own",
        busy || second >= 0,
    );
    c.check(
        "PVERSION",
        with(fd, R, b'A', 0x00, &mut v) == 0 && v >> 16 == 2,
    );

    let mut p = any();
    c.check(
        "HW_REFINE of anything",
        with(fd, RW, b'A', 0x10, &mut p) == 0,
    );
    let mut p = any();
    pin(&mut p, IV_RATE, RATE as u32);
    pin(&mut p, IV_PERIOD_SIZE, PERIOD as u32);
    pin(&mut p, IV_PERIODS, PERIODS as u32);
    let r = with(fd, RW, b'A', 0x11, &mut p);
    let buffer = p.intervals[IV_BUFFER_SIZE].min as u64;
    println!(
        "hw_params: {} Hz, period {}, buffer {buffer}",
        p.intervals[IV_RATE].min, p.intervals[IV_PERIOD_SIZE].min
    );
    c.check(
        "HW_PARAMS: 48 kHz, 8 periods of 1024",
        r == 0 && buffer == PERIOD * PERIODS && p.intervals[IV_RATE].min == RATE as u32,
    );
    let mut sw: SndPcmSwParams = zeroed();
    sw.avail_min = PERIOD;
    sw.start_threshold = buffer;
    sw.stop_threshold = buffer;
    let r = with(fd, RW, b'A', 0x13, &mut sw);
    c.check(
        "SW_PARAMS, and the boundary back",
        r == 0 && sw.boundary >= buffer,
    );
    let r = bare(fd, 0x40);
    let sp = sync_ptr(fd);
    c.check(
        "PREPARE, and SYNC_PTR says so",
        r == 0 && sp.status.state == STATE_PREPARED && sp.control.appl_ptr == 0,
    );
    // The status page cannot be mapped: alsa-lib then uses SYNC_PTR.
    // SAFETY: a mapping of the status page, unmapped if made.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0x8000_0000,
        )
    };
    println!(
        "mmap of the status page: {}",
        if map == libc::MAP_FAILED {
            "refused"
        } else {
            "made"
        }
    );
    if map != libc::MAP_FAILED {
        // SAFETY: the mapping just made.
        unsafe { libc::munmap(map, 4096) };
    }

    // The tone, one second of it.
    let mut tone = Vec::with_capacity(RATE as usize * 4);
    for i in 0..RATE {
        let s = (AMPLITUDE * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / RATE as f64).sin())
            as i16;
        tone.extend_from_slice(&s.to_le_bytes());
        tone.extend_from_slice(&s.to_le_bytes());
    }
    let start = Instant::now();
    let mut ok = true;
    let mut longest = Duration::ZERO;
    for _ in 0..seconds {
        // A period at a time, as alsa-lib's callers mostly write.
        for chunk in tone.chunks((PERIOD * 4) as usize) {
            let at = Instant::now();
            if let Err(e) = write_all(fd, chunk) {
                println!(
                    "writei: errno {e} after {:.3} s",
                    start.elapsed().as_secs_f64()
                );
                ok = false;
                break;
            }
            longest = longest.max(at.elapsed());
        }
    }
    let wrote = start.elapsed();
    println!(
        "the longest write took {:.1} ms",
        longest.as_secs_f64() * 1000.0
    );
    c.check(&format!("{seconds} s of tone written"), ok);
    let sp = sync_ptr(fd);
    let mut delay = 0i64;
    let r = with(fd, R, b'A', 0x21, &mut delay);
    let queued = sp.control.appl_ptr as i64 - sp.status.hw_ptr as i64;
    println!(
        "written in {:.3} s; appl {} hw {} delay {delay}",
        wrote.as_secs_f64(),
        sp.control.appl_ptr,
        sp.status.hw_ptr
    );
    c.check(
        "the pointers: all written, a buffer at most queued, running",
        sp.control.appl_ptr == seconds * RATE
            && (0..=buffer as i64).contains(&queued)
            && sp.status.state == STATE_RUNNING
            && r == 0
            && delay >= queued,
    );
    let played = seconds as f64 - queued as f64 / RATE as f64;
    c.check(
        "blocking writes keep the pace of the sound",
        (wrote.as_secs_f64() - played).abs() < 0.1 + 0.05 * seconds as f64,
    );
    let start = Instant::now();
    let r = bare(fd, 0x44);
    let drained = start.elapsed().as_secs_f64();
    let mut st: SndPcmStatus = zeroed();
    with(fd, R, b'A', 0x20, &mut st);
    let left = delay as f64 / RATE as f64;
    println!("drained in {drained:.3} s ({left:.3} s was left)");
    c.check(
        "DRAIN waits for what was queued, then the stream is set up",
        r == 0 && st.state == STATE_SETUP && (drained - left).abs() < 0.1,
    );

    // An open that does not wait: what fits, then EAGAIN.
    let silence = vec![0u8; (PERIOD * 4 * 8) as usize];
    // SAFETY: the open's flags.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    // Not started (the threshold is the buffer, then the ring's slack the
    // kernel lets in): what is taken is what fits, and then nothing.
    let mut sw: SndPcmSwParams = zeroed();
    sw.avail_min = PERIOD;
    sw.start_threshold = sw_boundary(fd, buffer);
    sw.stop_threshold = buffer;
    with(fd, RW, b'A', 0x13, &mut sw);
    bare(fd, 0x40);
    let first = writei(fd, &silence);
    let mut again = first;
    for _ in 0..16 {
        again = writei(fd, &silence);
        if again.is_err() {
            break;
        }
    }
    println!("no wait: {first:?}, then {again:?}");
    c.check(
        "a write with no room on an open that does not wait: what fits, then EAGAIN",
        first.is_ok_and(|n| n >= buffer.min(silence.len() as u64 / 4))
            && again == Err(libc::EAGAIN),
    );
    bare(fd, 0x43);
    // SAFETY: as above.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl & !libc::O_NONBLOCK);
    }

    // An underrun: a stream left with nothing to play is in XRUN, a write
    // is EPIPE, and PREPARE recovers it.
    let mut sw: SndPcmSwParams = zeroed();
    sw.avail_min = PERIOD;
    sw.start_threshold = 1;
    sw.stop_threshold = buffer;
    with(fd, RW, b'A', 0x13, &mut sw);
    bare(fd, 0x40);
    let _ = writei(fd, &silence[..(PERIOD * 4) as usize]);
    std::thread::sleep(Duration::from_millis(200));
    let sp = sync_ptr(fd);
    let epipe = writei(fd, &silence[..4096]);
    let r = bare(fd, 0x40);
    let after = writei(fd, &silence[..4096]);
    println!(
        "underrun: state {} then {epipe:?}; after PREPARE {after:?}",
        sp.status.state
    );
    c.check(
        "a stream that ran dry is XRUN, EPIPE, and PREPARE recovers it",
        sp.status.state == STATE_XRUN && epipe == Err(libc::EPIPE) && r == 0 && after == Ok(1024),
    );
    bare(fd, 0x43);
    // SAFETY: the opens.
    unsafe {
        libc::close(fd);
        libc::close(ctl);
    }
    if c.failed == 0 {
        println!("PASS {}", c.passed);
    } else {
        println!("FAIL {} of {}", c.failed, c.passed + c.failed);
        exit(1);
    }
}
