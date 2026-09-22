//! The ALSA and OSS nodes on a real `HdaDevice`: the driver probed from an
//! HDA controller in host memory (`zcore_drivers::audio::hda::hda_fake`),
//! the way `/dev/snd/pcmC0D0p` and `/dev/dsp` sit on `hda0` on the machine.
//!
//! Every other test of these nodes drives them against a `FakeAudio` that
//! takes bytes and counts them. These put the nodes' arithmetic (avail,
//! hw_ptr, the buffer bound, the thresholds, the delay) on the driver's own
//! counts, with the link taking PCM out of the ring at 48 kHz, and say what
//! the cable heard. The clock is the driver's test clock: it moves when the
//! link runs and nowhere else, so the engine's timing (its fill cadence,
//! its idle stop) is exact.

use super::*;
use crate::fs::devfs::DspDev;
use zcore_drivers::audio::hda::{hda_fake, HdaDevice};

const FRAME: usize = BYTES_PER_FRAME as usize;

/// A probed `hda0` on the fake controller, and the registry node on it.
struct Card {
    hw: hda_fake::Hw,
    dev: Arc<HdaDevice>,
    node: PcmDev,
}

fn card() -> Card {
    let hw = hda_fake::controller(4);
    let dev = Arc::new(HdaDevice::new(hw.bar, "hda0".into(), false).expect("probe"));
    let node = PcmDev::new(dev.clone(), 0);
    Card { hw, dev, node }
}

impl Card {
    /// Time passes: the link plays for `us`, and every 4 ms the playback
    /// watchdog polls the device (`queued_bytes` on it runs the engine's
    /// progress poll and the ring fill), exactly as `snd.rs` arms it while
    /// something plays.
    fn run(&self, us: u64) {
        let mut left = us;
        while left > 0 {
            let step = left.min(4_000);
            self.hw.run_for_us(step);
            self.dev.queued_bytes();
            left -= step;
        }
    }

    fn client(&self) -> Arc<dyn INode> {
        self.node.open_client().unwrap()
    }

    /// Whole frames the link has heard.
    fn heard(&self) -> usize {
        self.hw.played().len() / FRAME
    }
}

fn pcm(node: &Arc<dyn INode>) -> &PcmDev {
    node.downcast_ref::<PcmDev>().unwrap()
}

/// A wide-open hw_params request, as `snd_pcm_hw_params_any` builds it.
fn any_hw_params() -> SndPcmHwParams {
    let mut p: SndPcmHwParams = unsafe { core::mem::zeroed() };
    for m in p.masks.iter_mut() {
        m.bits[0] = !0;
        m.bits[1] = !0;
    }
    for iv in p.intervals.iter_mut() {
        iv.min = 0;
        iv.max = u32::MAX;
    }
    p.rmask = !0;
    p
}

/// HW_PARAMS with the rate pinned and the sizes left open, as PulseAudio
/// asks: the buffer the node grants, in frames.
fn hw_params(pcm: &PcmDev, rate: u32) -> u64 {
    let mut hp = any_hw_params();
    hp.intervals[PcmDev::IV_RATE].min = rate;
    hp.intervals[PcmDev::IV_RATE].max = rate;
    pcm.io_control(0x4111, &mut hp as *mut SndPcmHwParams as usize)
        .unwrap();
    assert_eq!(hp.intervals[PcmDev::IV_RATE].min, rate);
    pcm.st.lock().buffer_size
}

/// SW_PARAMS with the two thresholds given, `None` meaning the boundary
/// (PulseAudio sets both there). Returns the boundary the kernel handed
/// back.
fn sw_params(pcm: &PcmDev, start_threshold: Option<u64>, stop_threshold: Option<u64>) -> u64 {
    let boundary = pcm.st.lock().boundary;
    let mut p: SndPcmSwParams = unsafe { core::mem::zeroed() };
    p.avail_min = 1;
    p.period_step = 1;
    p.xfer_align = 1;
    p.start_threshold = start_threshold.unwrap_or(boundary);
    p.stop_threshold = stop_threshold.unwrap_or(boundary);
    pcm.io_control(0x4113, &mut p as *mut SndPcmSwParams as usize)
        .unwrap();
    p.boundary
}

fn prepare(pcm: &PcmDev) {
    pcm.io_control(0x4140, 0).unwrap();
}

fn start(pcm: &PcmDev) {
    pcm.io_control(0x4142, 0).unwrap();
}

fn pause(pcm: &PcmDev, on: bool) {
    let mut v: i32 = on as i32;
    pcm.io_control(0x4145, &mut v as *mut i32 as usize).unwrap();
}

/// Nonblocking WRITEI_FRAMES of whole `bytes`: the frames taken, or the
/// error.
fn write(pcm: &PcmDev, bytes: &[u8]) -> Result<u64> {
    let mut xfer = SndXferI {
        result: 0,
        buf: bytes.as_ptr() as usize as u64,
        frames: (bytes.len() / FRAME) as u64,
    };
    pcm.writei(&mut xfer, OpenFlags::NON_BLOCK)?;
    Ok(xfer.result as u64)
}

fn avail(pcm: &PcmDev) -> u64 {
    let st = pcm.st.lock();
    pcm.avail(&st)
}

fn state(pcm: &PcmDev) -> i32 {
    pcm.st.lock().state
}

/// SYNC_PTR with HWSYNC, as alsa-lib's `snd_pcm_hwsync` + avail path
/// issues it: the hardware pointer.
fn hw_ptr(pcm: &PcmDev) -> u64 {
    let mut sp: SndPcmSyncPtr = unsafe { core::mem::zeroed() };
    sp.flags = SYNC_PTR_HWSYNC | SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN;
    pcm.sync_ptr(&mut sp);
    sp.status.hw_ptr
}

/// The DELAY ioctl, in frames.
fn delay(pcm: &PcmDev) -> Result<i64> {
    let mut d: i64 = 0;
    pcm.io_control(0x4121, &mut d as *mut i64 as usize)?;
    Ok(d)
}

/// `n` stereo frames whose sample is `from` plus the frame's index, kept
/// in 1..=30000: none silent, and no two alike within a buffer's length.
fn ramp(from: u64, n: usize) -> Vec<u8> {
    (0..n as u64)
        .flat_map(|k| {
            let s = (((from + k) % 30_000) as i16 + 1).to_le_bytes();
            [s[0], s[1], s[0], s[1]]
        })
        .collect()
}

/// `n` stereo frames of the sample `v`.
fn constant(v: i16, n: usize) -> Vec<u8> {
    let s = v.to_le_bytes();
    core::iter::repeat_n([s[0], s[1], s[0], s[1]], n)
        .flatten()
        .collect()
}

/// The samples the link heard, left channel.
fn samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<FRAME>()
        .0
        .iter()
        .map(|f| i16::from_le_bytes([f[0], f[1]]))
        .collect()
}

/// The first `n` frames heard, against the first `n` of `expected`, with
/// the first difference named.
fn assert_heard(hw: &hda_fake::Hw, expected: &[u8], n: usize) {
    let got = hw.played();
    assert!(
        got.len() >= n * FRAME,
        "heard {} frames, expected {}",
        got.len() / FRAME,
        n
    );
    if let Some(at) = (0..n * FRAME).find(|&i| got[i] != expected[i]) {
        panic!(
            "frame {} differs: heard {:?}, sent {:?}",
            at / FRAME,
            &got[at / FRAME * FRAME..at / FRAME * FRAME + FRAME],
            &expected[at / FRAME * FRAME..at / FRAME * FRAME + FRAME]
        );
    }
}

// ── PulseAudio on hw:0,0 ────────────────────────────────────────────────

/// The loop PulseAudio's ALSA sink runs, on the real driver, at `rate`:
/// hw_params with the rate pinned and the buffer left open, both
/// thresholds at the boundary, PREPARE, fill, an explicit START, and then
/// forever `snd_pcm_avail()` followed by a write of exactly that many
/// frames. The daemon aborts (`pa_assert(err != -EAGAIN)` in alsa-sink.c's
/// `try_recover`) the first time the write takes fewer than avail
/// promised, and three different gaps between the node's avail and the
/// driver's accept have killed it that way (#1306, #1310, #1313). So the
/// invariant is checked at every fill level on the way up, at every
/// position the link stops at, and across the ring running dry -- which,
/// with the stop threshold at the boundary, is a free-running stream
/// playing silence, not an XRUN.
fn pulseaudio_loop(rate: u32) -> (Card, Vec<u8>) {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    let buffer = hw_params(pcm, rate);
    let capacity = pcm.audio.buffer_bytes() as u64 / BYTES_PER_FRAME;
    assert!(
        buffer <= capacity && buffer > capacity - 256,
        "granted {} of {}",
        buffer,
        capacity
    );
    let boundary = sw_params(pcm, None, None);
    assert!(boundary >= buffer && boundary.is_multiple_of(buffer));
    prepare(pcm);

    // Up: odd-sized chunks, so every fill level between the multiples of
    // anything gets a turn. What is promised is taken; with avail at zero
    // the node may still hand the driver a chunk (the ring's slack past
    // the granted buffer, up to the driver's own capacity), or answer
    // EAGAIN -- both are fine by the daemon, which only writes after a
    // positive avail.
    let mut sent = Vec::new();
    let mut next = 0u64;
    let mut chunk_frames = 97usize;
    loop {
        let promised = avail(pcm);
        let chunk = ramp(next, chunk_frames);
        match write(pcm, &chunk) {
            Ok(n) => {
                if promised > 0 {
                    assert_eq!(
                        n,
                        promised.min(chunk_frames as u64),
                        "at {} queued",
                        sent.len() / FRAME
                    );
                }
                sent.extend_from_slice(&chunk[..n as usize * FRAME]);
                next += n;
            }
            Err(FsError::Again) => {
                assert_eq!(promised, 0, "EAGAIN with {} frames promised", promised);
                break;
            }
            Err(e) => panic!("write: {:?}", e),
        }
        chunk_frames = 97 + (sent.len() / FRAME) % 13;
    }
    // A resampled stream takes a little more than the buffer: the frames
    // held inside the converter (its group delay) are in no queue.
    let filled = sent.len() as u64 / BYTES_PER_FRAME;
    assert!(
        filled + 4 >= buffer && filled <= buffer + 128,
        "{} frames fit of {} granted",
        filled,
        buffer
    );
    assert_eq!(state(pcm), STATE_PREPARED);
    assert!(!card.hw.running(), "held until START");
    start(pcm);
    assert_eq!(state(pcm), STATE_RUNNING);
    assert!(card.hw.running());

    // Steady state: the link takes an irregular bite, the sink asks avail
    // and writes exactly that.
    let mut last_hw = 0;
    for round in 0..60u64 {
        card.run(2_900 + (round % 7) * 1_100);
        let ptr = hw_ptr(pcm);
        assert!(ptr >= last_hw, "hw_ptr went back: {} -> {}", last_hw, ptr);
        last_hw = ptr;
        let promised = avail(pcm);
        if promised == 0 {
            continue;
        }
        let chunk = ramp(next, promised as usize);
        let n = write(pcm, &chunk)
            .unwrap_or_else(|e| panic!("round {}: {:?} with {} promised", round, e, promised));
        assert_eq!(
            n, promised,
            "round {}: promised {} frames, the write took {}",
            round, promised, n
        );
        sent.extend_from_slice(&chunk);
        next += n;
        // The converter's rounding can leave a frame or two of avail
        // behind a write of exactly avail; the daemon writes them next
        // and they are taken.
        let left = avail(pcm);
        assert!(
            left <= 4,
            "round {}: avail {} after writing avail",
            round,
            left
        );
        if left > 0 {
            let chunk = ramp(next, left as usize);
            assert_eq!(
                write(pcm, &chunk).unwrap(),
                left,
                "round {}: the rounding frames",
                round
            );
            sent.extend_from_slice(&chunk);
            next += left;
        }
        assert_eq!(state(pcm), STATE_RUNNING);
    }
    let kept_up = sent.len();

    // Dry: a stall of the daemon longer than the buffer. Free-running, so
    // the stream stays RUNNING, nothing is left to play, avail is the
    // whole buffer (less the converter's slack on a resampled stream), and
    // that is what the next write takes.
    card.run(400_000);
    assert_eq!(state(pcm), STATE_RUNNING);
    assert_eq!(
        delay(pcm).unwrap(),
        0,
        "nothing left to play after a dry spell"
    );
    let promised = avail(pcm);
    assert!(
        promised + 4 >= buffer,
        "avail {} of {} after a dry spell",
        promised,
        buffer
    );
    let chunk = ramp(next, promised as usize);
    assert_eq!(write(pcm, &chunk).unwrap(), promised);
    assert_eq!(avail(pcm), 0);
    next += promised;
    for round in 0..10u64 {
        card.run(5_000 + round * 700);
        let promised = avail(pcm);
        if promised == 0 {
            continue;
        }
        let chunk = ramp(next, promised as usize);
        assert_eq!(
            write(pcm, &chunk).unwrap(),
            promised,
            "after the dry spell, round {}",
            round
        );
        next += promised;
    }
    sent.truncate(kept_up);
    (card, sent)
}

#[test]
fn pulseaudio_at_the_link_rate_is_never_promised_a_frame_the_write_refuses() {
    let (card, sent) = pulseaudio_loop(48_000);
    // At the link rate nothing is resampled: what went in is what came
    // out, from the first frame, for as long as the daemon kept up.
    let n = sent.len() / FRAME;
    assert!(
        card.heard() > n,
        "the link heard {} of {} sent",
        card.heard(),
        n
    );
    assert_heard(&card.hw, &sent, n);
}

#[test]
fn pulseaudio_at_44100_is_never_promised_a_frame_the_write_refuses() {
    let (card, sent) = pulseaudio_loop(44_100);
    // Resampled 44.1 -> 48: more frames come out than went in, and none
    // of them is a straight copy, so only the count is checked here.
    assert!(card.heard() > sent.len() / FRAME);
}

/// The buffer PulseAudio is granted follows the rate of each reopen (the
/// #1310 gap: a 44.1 kHz open sized with the 48 kHz figure was promised
/// 999 frames the ring did not have).
#[test]
fn the_buffer_granted_follows_the_rate_of_the_reopen() {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    let at_48k = hw_params(pcm, 48_000);
    assert_eq!(at_48k, 12288, "48 KiB of link ring at 48 kHz");
    pcm.io_control(0x4112, 0).unwrap(); // HW_FREE
    let at_44k = hw_params(pcm, 44_100);
    let ring_now = pcm.audio.buffer_bytes_at(44_100) as u64 / BYTES_PER_FRAME;
    assert_eq!(ring_now, 11289, "12288 link frames, as 44.1 kHz frames");
    assert!(
        at_44k <= ring_now,
        "granted {} of {} frames",
        at_44k,
        ring_now
    );
    assert!(
        at_44k > ring_now - 256,
        "granted only {} of {}",
        at_44k,
        ring_now
    );
    // And the bound is real: avail on the empty stream is the granted
    // buffer less the converter's slack, the ring takes exactly that, and
    // what the converter keeps inside (its group delay, a millisecond)
    // comes back as avail once, taken in full again.
    prepare(pcm);
    let promised = avail(pcm);
    assert!(
        promised >= at_44k - 4 && promised <= at_44k,
        "avail {} of {}",
        promised,
        at_44k
    );
    let all = ramp(0, at_44k as usize + 1);
    assert_eq!(write(pcm, &all).unwrap(), promised);
    let again = avail(pcm);
    assert!(again < 128, "avail {} after the buffer went in", again);
    assert_eq!(write(pcm, &all[..again as usize * FRAME]).unwrap(), again);
    assert_eq!(avail(pcm), 0);
}

// ── Positions ───────────────────────────────────────────────────────────

/// `snd_pcm_delay()` is what a player syncs video to: frames between the
/// last one written and the one the cable is playing now. On the driver
/// that is the client's queue plus the ring ahead of the DMA position, and
/// the fake's link makes that exact to the frame.
#[test]
fn delay_counts_to_the_frame_what_the_link_has_not_heard_yet() {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    hw_params(pcm, 48_000);
    prepare(pcm);
    let all = ramp(0, 6144);
    assert_eq!(write(pcm, &all[..4096 * FRAME]).unwrap(), 4096);
    assert_eq!(
        state(pcm),
        STATE_RUNNING,
        "start_threshold 1: the first write starts it"
    );
    // The engine primed its fill depth from the stream: those frames are
    // in the ring, not heard. The client's counts do not move for that.
    assert_eq!(delay(pcm).unwrap(), 4096);
    assert_eq!(hw_ptr(pcm), 0);
    assert_eq!(avail(pcm), 12288 - 4096);
    let mut written = 4096i64;
    for step in 0..7u64 {
        card.run(9_000 + step * 700);
        let heard = card.heard() as i64;
        assert!(heard > 0);
        assert_eq!(
            delay(pcm).unwrap(),
            written - heard,
            "after {} frames heard",
            heard
        );
        assert_eq!(hw_ptr(pcm) as i64, heard, "hw_ptr is the link's position");
        assert_eq!(avail(pcm) as i64, 12288 - (written - heard));
        if step == 3 {
            assert_eq!(write(pcm, &all[4096 * FRAME..]).unwrap(), 2048);
            written += 2048;
            assert_eq!(delay(pcm).unwrap(), written - heard);
        }
    }
    assert_heard(&card.hw, &all, card.heard());
}

/// The ring running dry on a stream with the default stop threshold is an
/// underrun: EPIPE from then on, and the engine, with nothing left to play,
/// stops itself after its idle time. PREPARE is the way back, and the
/// first write after it starts the engine again where the new PCM begins.
#[test]
fn running_dry_is_an_xrun_the_engine_idles_out_and_prepare_brings_it_back() {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    hw_params(pcm, 48_000);
    prepare(pcm);
    let first = ramp(0, 1024);
    assert_eq!(write(pcm, &first).unwrap(), 1024);
    // Not an underrun while the sound plays: the frames are in the ring.
    card.run(12_000);
    assert!(card.heard() < 1024);
    assert_eq!(state(pcm), STATE_RUNNING);
    assert_eq!(delay(pcm).unwrap(), 1024 - card.heard() as i64);
    card.run(40_000);
    assert_heard(&card.hw, &first, 1024);
    assert!(
        matches!(delay(pcm), Err(FsError::Broken)),
        "DELAY on an underrun is EPIPE"
    );
    assert_eq!(state(pcm), STATE_XRUN);
    assert!(
        matches!(write(pcm, &first), Err(FsError::Broken)),
        "so is a write"
    );
    assert!(
        card.hw.running(),
        "the engine loops silence for a while first"
    );
    card.run(5_500_000);
    assert!(!card.hw.running(), "and stops once idle");
    assert_eq!(card.hw.stops(), 1);

    prepare(pcm);
    assert_eq!(state(pcm), STATE_PREPARED);
    let second = ramp(2000, 1024);
    assert_eq!(write(pcm, &second).unwrap(), 1024);
    assert_eq!(state(pcm), STATE_RUNNING);
    assert!(card.hw.running());
    assert_eq!(card.hw.starts(), 2);
    card.run(50_000);
    let played = card.hw.played();
    let at = samples(&played)
        .iter()
        .position(|&s| s == 2001)
        .expect("the new PCM reached the link");
    assert_eq!(&played[at * FRAME..(at + 1024) * FRAME], &second[..]);
}

/// A start threshold holds the engine: the PCM queues, nothing plays, and
/// the write that reaches the threshold starts it with the held frames
/// first.
#[test]
fn a_start_threshold_holds_the_engine_until_the_client_reaches_it() {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    let buffer = hw_params(pcm, 48_000);
    sw_params(pcm, Some(2048), Some(buffer));
    prepare(pcm);
    let all = ramp(0, 2048);
    assert_eq!(write(pcm, &all[..1024 * FRAME]).unwrap(), 1024);
    assert_eq!(state(pcm), STATE_PREPARED);
    assert!(!card.hw.running());
    assert_eq!(card.hw.starts(), 0);
    card.run(100_000);
    assert_eq!(card.heard(), 0, "nothing plays below the threshold");
    assert_eq!(avail(pcm), buffer - 1024);
    assert_eq!(delay(pcm).unwrap(), 1024);
    assert_eq!(write(pcm, &all[1024 * FRAME..]).unwrap(), 1024);
    assert_eq!(state(pcm), STATE_RUNNING);
    assert!(card.hw.running());
    assert_eq!(card.hw.starts(), 1);
    card.run(50_000);
    assert_heard(&card.hw, &all, 2048);
}

/// PAUSE stops the stream's PCM reaching the link (after what the engine
/// had already mixed ahead plays out) and RESUME continues it from the
/// very next frame: nothing lost, nothing repeated, and writes taken
/// meanwhile, as Linux takes them on a paused stream.
#[test]
fn pause_silences_the_link_and_resume_carries_on_without_losing_a_frame() {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    hw_params(pcm, 48_000);
    prepare(pcm);
    let all = ramp(0, 12000);
    assert_eq!(write(pcm, &all[..8192 * FRAME]).unwrap(), 8192);
    card.run(10_000);
    pause(pcm, true);
    assert_eq!(state(pcm), STATE_PAUSED);
    let nonzero = |card: &Card| {
        samples(&card.hw.played())
            .iter()
            .filter(|&&s| s != 0)
            .count()
    };
    let at_pause = nonzero(&card);
    card.run(150_000);
    let mixed_ahead = nonzero(&card) - at_pause;
    assert!(
        mixed_ahead <= 4096,
        "{} frames of PCM after PAUSE, more than the fill depth",
        mixed_ahead
    );
    let settled = nonzero(&card);
    card.run(100_000);
    assert_eq!(nonzero(&card), settled, "paused: the link hears silence");
    assert_eq!(
        write(pcm, &all[8192 * FRAME..]).unwrap(),
        12000 - 8192,
        "writes are taken while paused"
    );
    // The frames the link heard are the ones the client is past.
    assert_eq!(hw_ptr(pcm) as usize, settled);
    assert_eq!(delay(pcm).unwrap() as usize, 12000 - settled);
    pause(pcm, false);
    assert_eq!(state(pcm), STATE_RUNNING);
    card.run(200_000);
    let played = card.hw.played();
    let heard: Vec<u8> = played
        .as_chunks::<FRAME>()
        .0
        .iter()
        .filter(|f| f.iter().any(|&b| b != 0))
        .flatten()
        .copied()
        .collect();
    assert!(
        heard.len() / FRAME > settled + 4000,
        "resumed: {} frames heard",
        heard.len() / FRAME
    );
    let n = heard.len();
    assert_eq!(
        &heard[..],
        &all[..n],
        "the stream continues where it paused"
    );
}

// ── More than one client ────────────────────────────────────────────────

/// The device mixes, so two opens of the PCM node are two streams: the
/// link hears their sum, and one closing mid-stream leaves the other
/// playing on the same engine.
#[test]
fn two_clients_are_mixed_and_one_leaving_leaves_the_other_playing() {
    let card = card();
    let a = card.client();
    let b = card
        .node
        .open_client()
        .expect("a mixing device takes a second open");
    for c in [&a, &b] {
        hw_params(pcm(c), 48_000);
        prepare(pcm(c));
    }
    assert_eq!(write(pcm(&a), &constant(1000, 12000)).unwrap(), 12000);
    assert_eq!(write(pcm(&b), &constant(2000, 12000)).unwrap(), 12000);
    // a's write started the engine and primed it from a alone; b joined
    // at the fill point, so the sum is heard from a fill depth in.
    card.run(120_000);
    let mix = samples(&card.hw.played());
    assert!(mix.len() >= 5000);
    assert!(
        mix[..4096].iter().all(|&s| s == 1000),
        "primed from a: {:?}",
        &mix[..8]
    );
    assert!(
        mix[4096..].iter().all(|&s| s == 3000),
        "then the sum: {:?}",
        &mix[4096..4104]
    );
    assert_eq!(hw_ptr(pcm(&a)) as usize, mix.len());
    assert_eq!(
        hw_ptr(pcm(&b)) as usize,
        mix.len() - 4096,
        "b's position counts from where it joined"
    );

    drop(a);
    card.run(120_000);
    assert!(card.hw.running(), "b keeps the engine");
    let mix = samples(&card.hw.played());
    let tail = &mix[mix.len() - 480..];
    assert!(
        tail.iter().all(|&s| s == 2000),
        "after a left, b alone: {:?}",
        &tail[..8]
    );
    assert_eq!(state(pcm(&b)), STATE_RUNNING);
    let room = avail(pcm(&b));
    assert!(room > 0);
    assert_eq!(
        write(pcm(&b), &constant(2000, room as usize)).unwrap(),
        room
    );
}

/// `/dev/dsp` and the PCM node on the same card, open at once: each is a
/// stream of its own (no EBUSY between them since the mixer), the link
/// hears both, and the OSS node's GETODELAY counts its own frames to the
/// one playing.
#[test]
fn dev_dsp_and_the_pcm_node_share_the_card_each_with_a_stream_of_its_own() {
    let card = card();
    let alsa = card.client();
    let oss = DspDev::new(card.dev.clone(), 0)
        .open_client()
        .expect("the OSS node opens beside the PCM one");
    let dsp = oss.downcast_ref::<DspDev>().unwrap();
    // The OSS write starts the engine, primed from that stream alone; the
    // PCM node's client joins at the fill point (a fill depth in).
    let oss_pcm = constant(500, 6000);
    assert_eq!(dsp.write_pcm(&oss_pcm, true).unwrap(), oss_pcm.len());
    hw_params(pcm(&alsa), 48_000);
    prepare(pcm(&alsa));
    assert_eq!(write(pcm(&alsa), &constant(700, 6000)).unwrap(), 6000);
    card.run(100_000);
    let mix = samples(&card.hw.played());
    assert!(mix.len() >= 4800);
    assert!(
        mix[..4096].iter().all(|&s| s == 500),
        "primed from the OSS stream: {:?}",
        &mix[..8]
    );
    assert!(
        mix[4096..].iter().all(|&s| s == 1200),
        "the link hears both: {:?}",
        &mix[4096..4104]
    );
    let mut odelay: i32 = 0;
    oss.io_control(0x8004_5017, &mut odelay as *mut i32 as usize)
        .unwrap(); // SNDCTL_DSP_GETODELAY
    assert_eq!(
        odelay as usize,
        oss_pcm.len() - mix.len() * FRAME,
        "OSS delay: its bytes not yet heard"
    );
    assert_eq!(
        delay(pcm(&alsa)).unwrap() as usize,
        6000 - (mix.len() - 4096),
        "and the PCM client's, from where it joined"
    );
}

/// A controller that fetches ahead of what it plays (the one this driver
/// was written against ran 15-27 KB ahead of the link; QEMU 106 KB): the
/// client's counts follow the fetch, as Linux's hardware pointer does, so
/// the lead does not eat into the client's buffer, while DELAY follows the
/// link, so a player syncing to it is still right to the frame.
#[test]
fn a_controller_fetching_ahead_of_the_link_leaves_the_client_its_buffer() {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    let buffer = hw_params(pcm, 48_000);
    prepare(pcm);
    let mut sent = ramp(0, buffer as usize);
    assert_eq!(write(pcm, &sent).unwrap(), buffer);
    let mut next = buffer;
    // The lead grows a little per poll, within what a position read may
    // advance, up to 24 KiB (125 ms) and stays there.
    let mut lead = 0usize;
    for round in 0..80u64 {
        lead = (lead + 3072).min(24 * 1024);
        card.hw.set_lead(lead);
        card.run(4_000);
        let heard = card.heard() as u64;
        let fetched = heard + (lead / FRAME) as u64;
        assert_eq!(
            delay(pcm).unwrap() as u64,
            next - heard,
            "round {}: delay follows the link",
            round
        );
        assert_eq!(
            hw_ptr(pcm),
            fetched,
            "round {}: hw_ptr follows the fetch",
            round
        );
        assert_eq!(avail(pcm), buffer - (next - fetched), "round {}", round);
        assert_eq!(
            state(pcm),
            STATE_RUNNING,
            "round {}: no underrun with the buffer kept up",
            round
        );
        let promised = avail(pcm);
        if promised > 0 {
            let chunk = ramp(next, promised as usize);
            assert_eq!(write(pcm, &chunk).unwrap(), promised, "round {}", round);
            sent.extend_from_slice(&chunk);
            next += promised;
        }
    }
    assert!(card.heard() > 12288);
    assert_heard(&card.hw, &sent, card.heard());
}

/// DROP then PREPARE while the stream plays: what the mixer already took
/// plays out, but it is no longer the client's, whose position starts
/// over with the next write -- never behind it, which is how alsa-lib's
/// avail wraps to the boundary.
#[test]
fn drop_and_prepare_mid_stream_start_the_client_position_over() {
    let card = card();
    let node = card.client();
    let pcm = pcm(&node);
    let buffer = hw_params(pcm, 48_000);
    prepare(pcm);
    let first = ramp(0, 8192);
    assert_eq!(write(pcm, &first).unwrap(), 8192);
    card.run(50_000);
    let heard_at_drop = card.heard();
    assert!(heard_at_drop > 2000 && heard_at_drop < 8192);
    pcm.io_control(0x4143, 0).unwrap(); // DROP
    prepare(pcm);
    let second = ramp(20_000, 1024);
    assert_eq!(write(pcm, &second).unwrap(), 1024);
    assert_eq!(
        hw_ptr(pcm),
        0,
        "the old frames in the ring are not the client's"
    );
    assert_eq!(delay(pcm).unwrap(), 1024);
    assert_eq!(avail(pcm), buffer - 1024);
    card.run(150_000);
    assert_eq!(hw_ptr(pcm), 1024);
    let played = card.hw.played();
    let at = samples(&played)
        .iter()
        .position(|&s| s == 20_001)
        .expect("the new PCM reached the link");
    assert!(
        at >= heard_at_drop,
        "it came after what was already fetched"
    );
    assert_eq!(&played[at * FRAME..(at + 1024) * FRAME], &second[..]);
}
