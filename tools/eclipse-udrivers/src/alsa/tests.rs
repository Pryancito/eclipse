use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::hda::LEAD;
use crate::oss::Dsp;
use crate::stream::tests::Fake;

static NOW: AtomicU64 = AtomicU64::new(1_000_000_000);

fn clock() -> u64 {
    NOW.load(Ordering::Relaxed)
}

/// A request that asks nothing: alsa-lib's `snd_pcm_hw_params_any`.
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
    p.intervals[idx] = SndInterval {
        min: v,
        max: v,
        flags: 0,
    };
}

fn iv(p: &SndPcmHwParams, idx: usize) -> (u32, u32) {
    (p.intervals[idx].min, p.intervals[idx].max)
}

/// The command `nr` of type `'A'` or `'U'`, carrying `T` both ways.
fn cmd<T>(ty: u8, nr: u32) -> u32 {
    (3 << 30) | ((core::mem::size_of::<T>() as u32) << 16) | ((ty as u32) << 8) | nr
}

struct Card {
    s: Stream<Fake>,
    pcm: Pcm,
}

impl Card {
    fn new() -> Card {
        let mut c = Card {
            s: Stream::new(Fake::new()),
            pcm: Pcm::new(clock),
        };
        c.pcm.open(&mut c.s).unwrap();
        c
    }

    fn call<T: Copy>(&mut self, nr: u32, v: &T) -> Result<T, i32> {
        let mut b = vec![0u8; core::mem::size_of::<T>()];
        put(&mut b, v).unwrap();
        self.pcm
            .control(&mut self.s, cmd::<T>(b'A', nr), &mut b, false)?;
        get(&b)
    }

    fn bare(&mut self, nr: u32) -> Result<(), i32> {
        self.pcm
            .control(&mut self.s, ((b'A' as u32) << 8) | nr, &mut [], false)
            .map(|_| ())
    }

    /// `hw_params` of 48 kHz with what else `f` pins, then `PREPARE`.
    fn setup(&mut self, f: impl Fn(&mut SndPcmHwParams)) -> SndPcmHwParams {
        let mut p = any();
        pin(&mut p, IV_RATE, 48000);
        f(&mut p);
        let p = self.call(0x11, &p).unwrap();
        self.bare(0x40).unwrap();
        p
    }

    /// `WRITEI_FRAMES` of `frames` frames of `v`, as the kernel makes it:
    /// the frames the node asks for, then the call with them.
    fn writei(&mut self, frames: u64, v: i16, waits: bool) -> Result<u64, i32> {
        self.pcm.admit(&mut self.s)?;
        let take = self.pcm.take(&mut self.s, frames, waits)?;
        let data: Vec<u8> = (0..take * 2).flat_map(|_| v.to_le_bytes()).collect();
        let room = self.pcm.writable(&self.s);
        let n = (take.min(room) * 4) as usize;
        let n = self.s.write(&data[..n]) as u64 / 4;
        self.pcm.commit(&mut self.s, n)?;
        Ok(n)
    }

    fn sync(&mut self, flags: u32, appl: u64) -> SndPcmSyncPtr {
        let mut sp: SndPcmSyncPtr = zeroed();
        sp.flags = flags;
        sp.control.appl_ptr = appl;
        self.call(0x23, &sp).unwrap()
    }

    /// The controller played on to `bytes` into the ring.
    fn play_to(&mut self, bytes: usize) {
        self.s.out.pos = bytes % crate::hda::RING_LEN;
        self.pcm.tick(&mut self.s, None);
    }
}

#[test]
fn the_format_is_s16le_stereo_and_anything_else_is_refused() {
    let mut p = any();
    assert!(refine(&mut p));
    assert_eq!(iv(&p, IV_SAMPLE_BITS), (16, 16));
    assert_eq!(iv(&p, IV_CHANNELS), (2, 2));
    assert_eq!(p.masks[PAR_FORMAT].bits[0], 1 << FORMAT_S16_LE);
    assert_eq!(p.masks[PAR_ACCESS].bits[0], 1 << ACCESS_RW_INTERLEAVED);
    for bad in [
        |p: &mut SndPcmHwParams| pin(p, IV_CHANNELS, 1),
        |p: &mut SndPcmHwParams| pin(p, IV_SAMPLE_BITS, 24),
        |p: &mut SndPcmHwParams| p.masks[PAR_FORMAT].bits[0] = 1 << 10,
        |p: &mut SndPcmHwParams| p.masks[PAR_ACCESS].bits[0] = 1,
    ] {
        let mut p = any();
        bad(&mut p);
        assert!(!refine(&mut p));
        let mut p = any();
        bad(&mut p);
        assert_eq!(
            choose(&mut p),
            Err(EINVAL),
            "no recovery makes it another format"
        );
    }
}

#[test]
fn the_rate_snaps_to_the_set_the_controller_plays() {
    for &rate in RATES.iter() {
        let mut p = any();
        pin(&mut p, IV_RATE, rate);
        assert!(refine(&mut p), "{rate} Hz");
    }
    let mut p = any();
    p.intervals[IV_RATE] = SndInterval {
        min: 44101,
        max: 47999,
        flags: 0,
    };
    assert!(!refine(&mut p));
}

#[test]
fn the_requests_the_kernel_learned_to_take_are_taken() {
    // mpg123: half a second at 44.1 kHz, periods of 125 ms.
    let mut p = any();
    pin(&mut p, IV_RATE, 44_100);
    pin(&mut p, IV_BUFFER_SIZE, 22_050);
    pin(&mut p, IV_PERIOD_TIME, 125_000);
    assert!(refine(&mut p));
    // Two periods and the smallest buffer.
    let mut p = any();
    pin(&mut p, IV_PERIODS, 2);
    pin(&mut p, IV_BUFFER_SIZE, 512);
    assert!(refine(&mut p));
    let (lo, hi) = iv(&p, IV_PERIOD_SIZE);
    assert!(lo <= 256 && 256 <= hi);
    // Half a second at 11.025 kHz by its time.
    let mut p = any();
    pin(&mut p, IV_RATE, 11_025);
    pin(&mut p, IV_BUFFER_TIME, 500_000);
    assert!(refine(&mut p));
    // A period whose size and time disagree is recovered, keeping the size.
    let mut p = any();
    pin(&mut p, IV_RATE, 48_000);
    pin(&mut p, IV_PERIOD_SIZE, 1024);
    pin(&mut p, IV_PERIOD_TIME, 10_000);
    assert!(!refine(&mut p.clone()));
    let c = choose(&mut p).unwrap();
    assert_eq!(c.period, 1024);
    assert_eq!(iv(&p, IV_PERIOD_SIZE), (1024, 1024));
}

#[test]
fn hw_params_hands_back_one_coherent_value_each_and_wipes_the_ring() {
    let mut c = Card::new();
    let p = c.setup(|p| {
        pin(p, IV_PERIOD_SIZE, 1024);
        pin(p, IV_PERIODS, 4);
    });
    assert_eq!(iv(&p, IV_BUFFER_SIZE), (4096, 4096));
    assert_eq!(iv(&p, IV_PERIOD_BYTES), (4096, 4096));
    assert_eq!(iv(&p, IV_PERIOD_TIME), (21333, 21333));
    assert_eq!(
        (c.pcm.buffer_size, c.pcm.period_size, c.s.rate),
        (4096, 1024, 48000)
    );
    assert_eq!(c.pcm.state, STATE_PREPARED);
    // Left open, the buffer is the deepest the ring holds.
    let mut c = Card::new();
    c.setup(|_| {});
    assert!(c.pcm.buffer_size <= RING_FRAMES && c.pcm.buffer_size > RING_FRAMES / 2);
}

#[test]
fn a_write_starts_the_stream_at_the_start_threshold_and_the_pointers_follow_it() {
    let mut c = Card::new();
    c.setup(|p| pin(p, IV_PERIOD_SIZE, 1024));
    let mut sw: SndPcmSwParams = zeroed();
    sw.avail_min = 1024;
    sw.start_threshold = 2048;
    sw.stop_threshold = c.pcm.buffer_size;
    let sw = c.call(0x13, &sw).unwrap();
    assert_eq!(sw.boundary, c.pcm.boundary);
    assert_eq!(c.writei(1024, 100, false), Ok(1024));
    assert!(!c.s.running(), "below the threshold it waits");
    assert_eq!(c.writei(1024, 100, false), Ok(1024));
    assert!(c.s.running());
    assert_eq!(c.pcm.state, STATE_RUNNING);
    // alsa-lib learns the pointer the write moved, and the hardware's.
    let sp = c.sync(SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN, 0);
    assert_eq!(sp.control.appl_ptr, 2048);
    assert_eq!(sp.status.hw_ptr, 0);
    c.play_to(LEAD + 1000 * 4);
    let sp = c.sync(SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN, 0);
    assert_eq!(sp.status.hw_ptr, 1000);
    assert_eq!(sp.status.state, STATE_RUNNING);
    let st: SndPcmStatus = c.call(0x20, &zeroed()).unwrap();
    assert_eq!(st.avail, c.pcm.buffer_size - 1048);
    assert_eq!(st.delay, 1048);
    // And the samples are in the ring as they were written.
    assert_eq!(&c.s.out.mem[LEAD + 4000..LEAD + 4004], &[100, 0, 100, 0]);
}

#[test]
fn a_full_buffer_takes_some_from_one_that_does_not_wait_and_all_or_none_from_one_that_does() {
    let mut c = Card::new();
    c.setup(|p| {
        pin(p, IV_PERIOD_SIZE, 1024);
        pin(p, IV_PERIODS, 4);
    });
    assert_eq!(c.writei(3000, 1, false), Ok(3000));
    assert_eq!(c.writei(3000, 1, false), Ok(1096), "what fits");
    assert_eq!(c.writei(1, 1, false), Err(EAGAIN));
    c.play_to(LEAD + 2000 * 4);
    // 2000 free: a waiting write of 3000 waits for all of them.
    assert_eq!(c.writei(3000, 1, true), Err(EAGAIN));
    assert_eq!(c.pcm.wants, Some(3000));
    c.play_to(LEAD + 2900 * 4);
    assert_eq!(c.pcm.wants, Some(3000), "not yet");
    c.play_to(LEAD + 3100 * 4);
    assert_eq!(c.pcm.wants, None, "told: the kernel asks again");
    assert_eq!(c.writei(3000, 1, true), Ok(3000));
    // One longer than the buffer goes a buffer at a time.
    c.bare(0x43).unwrap();
    c.bare(0x40).unwrap();
    assert_eq!(c.writei(10_000, 1, true), Ok(4096));
}

#[test]
fn a_stream_that_runs_dry_is_an_underrun_that_prepare_recovers() {
    let mut c = Card::new();
    c.setup(|p| pin(p, IV_PERIOD_SIZE, 1024));
    c.writei(1024, 5, false).unwrap();
    assert_eq!(c.pcm.state, STATE_RUNNING);
    c.play_to(LEAD + 1024 * 4);
    assert_eq!(c.pcm.state, STATE_XRUN);
    assert!(!c.s.running());
    assert_eq!(c.writei(64, 5, false), Err(EPIPE));
    assert_eq!(c.pcm.readiness(&mut c.s), POLLOUT | POLLERR);
    let mut d = [0u8; 8];
    assert_eq!(
        c.pcm
            .control(&mut c.s, cmd::<i64>(b'A', 0x21), &mut d, false),
        Err(EPIPE)
    );
    c.bare(0x40).unwrap();
    assert_eq!(c.writei(64, 5, false), Ok(64));
    assert_eq!(c.pcm.state, STATE_RUNNING);
}

#[test]
fn a_free_running_stream_plays_silence_when_starved() {
    let mut c = Card::new();
    c.setup(|p| pin(p, IV_PERIOD_SIZE, 1024));
    let mut sw: SndPcmSwParams = zeroed();
    sw.avail_min = 1;
    sw.start_threshold = c.pcm.boundary;
    sw.stop_threshold = c.pcm.boundary;
    c.call(0x13, &sw).unwrap();
    // Pulse's sink: START with nothing queued is taken.
    c.bare(0x42).unwrap();
    assert_eq!(c.pcm.state, STATE_RUNNING);
    c.play_to(LEAD + 8000);
    assert_eq!(c.pcm.state, STATE_RUNNING);
    assert_eq!(c.writei(256, 3, false), Ok(256));
}

#[test]
fn a_drain_waits_until_what_is_queued_has_played() {
    let mut c = Card::new();
    c.setup(|p| pin(p, IV_PERIOD_SIZE, 1024));
    c.writei(2048, 7, false).unwrap();
    let mut none = [0u8; 0];
    let drain = cmd::<()>(b'A', 0x44) & !(3 << 30);
    assert_eq!(c.pcm.control(&mut c.s, drain, &mut none, true), Err(EAGAIN));
    assert_eq!(c.pcm.state, STATE_DRAINING);
    c.play_to(LEAD + 1000 * 4);
    assert_eq!(c.pcm.control(&mut c.s, drain, &mut none, true), Err(EAGAIN));
    c.play_to(LEAD + 2048 * 4);
    assert_eq!(c.pcm.state, STATE_SETUP, "done once played, by the tick");
    assert!(!c.pcm.drain_waits, "and the waiting open told");
    assert_eq!(c.pcm.control(&mut c.s, drain, &mut none, true), Ok((0, 0)));
    // A drain with nothing playing is done at once.
    c.bare(0x40).unwrap();
    assert_eq!(c.pcm.control(&mut c.s, drain, &mut none, false), Ok((0, 0)));
}

#[test]
fn a_pause_keeps_the_stream_where_it_was() {
    let mut c = Card::new();
    c.setup(|p| pin(p, IV_PERIOD_SIZE, 1024));
    c.writei(2048, 1, false).unwrap();
    c.play_to(LEAD + 400);
    assert_eq!(c.call(0x45, &1i32), Ok(1));
    assert_eq!(c.pcm.state, STATE_PAUSED);
    assert!(c.s.paused());
    assert_eq!(c.writei(64, 1, false), Ok(64), "a paused stream takes data");
    assert_eq!(c.call(0x45, &0i32), Ok(0));
    assert_eq!(
        (c.pcm.state, c.s.out.runs, c.s.out.resumes),
        (STATE_RUNNING, 1, 1)
    );
    assert_eq!(c.call(0x45, &0i32), Err(EBADFD));
}

#[test]
fn the_states_linux_refuses_are_refused() {
    let mut c = Card::new();
    assert_eq!(c.writei(64, 1, false), Err(EBADFD), "no hw_params yet");
    assert_eq!(c.bare(0x40), Err(EBADFD));
    assert_eq!(
        c.call(0x13, &zeroed::<SndPcmSwParams>()).err(),
        Some(EBADFD)
    );
    c.setup(|_| {});
    assert_eq!(c.bare(0x42), Err(EPIPE), "nothing to start");
    c.writei(64, 1, false).unwrap();
    assert_eq!(c.bare(0x40), Err(EBUSY), "running");
    assert_eq!(c.call(0x11, &any()).err(), Some(EBADFD));
    c.bare(0x43).unwrap();
    assert_eq!(c.pcm.state, STATE_SETUP);
    assert!(!c.s.running());
}

#[test]
fn one_open_plays_of_the_pcm_and_dsp() {
    let mut c = Card::new();
    let mut dsp = Dsp::new(clock);
    assert_eq!(dsp.open(&mut c.s), Err(EBUSY));
    assert_eq!(c.pcm.open(&mut c.s), Err(EBUSY));
    c.setup(|_| {});
    c.writei(64, 1, false).unwrap();
    c.pcm.release(&mut c.s, 1);
    assert!(!c.s.running(), "closing drops it");
    assert!(dsp.open(&mut c.s).is_ok());
    assert_eq!(c.pcm.open(&mut c.s), Err(EBUSY));
}

#[test]
fn the_control_lists_its_two_elements_and_sets_the_cards_volume() {
    let mut s = Stream::new(Fake::new());
    let mut ctl = Ctl::default();
    let mut b = vec![0u8; 376];
    ctl.control(&mut s, cmd::<SndCtlCardInfo>(b'U', 0x01), &mut b)
        .unwrap();
    let info: SndCtlCardInfo = get(&b).unwrap();
    assert_eq!(&info.driver[..11], b"eclipse-hda");
    // The list: how many, then where they go.
    let mut list: SndCtlElemList = zeroed();
    let mut b = vec![0u8; 80];
    put(&mut b, &list).unwrap();
    ctl.control(&mut s, cmd::<SndCtlElemList>(b'U', 0x10), &mut b)
        .unwrap();
    assert_eq!(get::<SndCtlElemList>(&b).unwrap().count, 2);
    list.space = 8;
    list.pids = 0x5000;
    put(&mut b, &list).unwrap();
    let iov = ctl.list_fetch(0x9000, &b).unwrap().unwrap();
    assert_eq!(iov.outs, vec![(0x9000, 80), (0x5000, 128)]);
    let mut out = vec![0u8; 80 + 128];
    assert_eq!(ctl.list(&b, &mut out), Ok((0, 208)));
    let back: SndCtlElemList = get(&out).unwrap();
    assert_eq!((back.used, back.count), (2, 2));
    let second: SndCtlElemId = get(&out[80 + 64..]).unwrap();
    assert_eq!(&second.name[..22], b"Master Playback Switch");
    // The volume is the stream's.
    let mut val: SndCtlElemValue = zeroed();
    val.id = elem_id(Elem::Volume);
    val.values = [40, 60];
    let mut b = vec![0u8; core::mem::size_of::<SndCtlElemValue>()];
    put(&mut b, &val).unwrap();
    ctl.control(&mut s, cmd::<SndCtlElemValue>(b'U', 0x13), &mut b)
        .unwrap();
    assert_eq!((s.gain.left, s.gain.right), (40, 60));
    val.id = elem_id(Elem::Switch);
    val.values = [0, 1];
    put(&mut b, &val).unwrap();
    ctl.control(&mut s, cmd::<SndCtlElemValue>(b'U', 0x13), &mut b)
        .unwrap();
    assert!(s.gain.mute_left && !s.gain.mute_right);
    ctl.control(&mut s, cmd::<SndCtlElemValue>(b'U', 0x12), &mut b)
        .unwrap();
    assert_eq!(get::<SndCtlElemValue>(&b).unwrap().values, [0, 1]);
    // No capture.
    let mut pi: SndPcmInfo = zeroed();
    pi.stream = 1;
    let mut b = vec![0u8; 288];
    put(&mut b, &pi).unwrap();
    assert_eq!(
        ctl.control(&mut s, cmd::<SndPcmInfo>(b'U', 0x31), &mut b),
        Err(ENOENT)
    );
}

/// The setup the kernel keeps of an open, as it keeps it
/// (`linux-object`'s `Open::keep`): the latest of each command the node
/// [`pcm_keeps`], last, with what came back of it.
#[derive(Default)]
struct Kept(Vec<(u32, Vec<u8>)>);

impl Kept {
    fn call(&mut self, c: &mut Card, cmd: u32, mut b: Vec<u8>) -> Result<Vec<u8>, i32> {
        c.pcm.control(&mut c.s, cmd, &mut b, false)?;
        if pcm_keeps(cmd) {
            self.0.retain(|(k, _)| *k != cmd);
            self.0.push((cmd, b.clone()));
        }
        Ok(b)
    }

    /// What the kernel tells the next driver's open.
    fn replay(&self, c: &mut Card) {
        for (cmd, b) in &self.0 {
            let mut b = b.clone();
            c.pcm.control(&mut c.s, *cmd, &mut b, false).unwrap();
        }
    }
}

fn bytes<T: Copy>(v: &T) -> Vec<u8> {
    let mut b = vec![0u8; core::mem::size_of::<T>()];
    put(&mut b, v).unwrap();
    b
}

#[test]
fn the_next_driver_told_the_kept_setup_has_the_stream_set_up_and_prepared() {
    let mut k = Kept::default();
    let mut c = Card::new();
    let bare = |nr: u32| ((b'A' as u32) << 8) | nr;
    k.call(&mut c, bare(0x04), vec![]).unwrap();
    let mut p = any();
    pin(&mut p, IV_RATE, 44100);
    pin(&mut p, IV_PERIOD_SIZE, 1024);
    k.call(&mut c, cmd::<SndPcmHwParams>(b'A', 0x11), bytes(&p))
        .unwrap();
    let mut sw: SndPcmSwParams = zeroed();
    sw.avail_min = 1024;
    sw.start_threshold = 2048;
    sw.stop_threshold = c.pcm.buffer_size;
    k.call(&mut c, cmd::<SndPcmSwParams>(b'A', 0x13), bytes(&sw))
        .unwrap();
    k.call(&mut c, bare(0x40), vec![]).unwrap();
    // It ran, was dropped and prepared again: what is kept is the latest
    // prepare, after the drop.
    assert_eq!(c.writei(4096, 7, false), Ok(4096));
    assert_eq!(c.pcm.state, STATE_RUNNING);
    k.call(&mut c, bare(0x43), vec![]).unwrap();
    k.call(&mut c, bare(0x40), vec![]).unwrap();
    // Asked, not set up: not kept.
    k.call(
        &mut c,
        cmd::<SndPcmStatus>(b'A', 0x20),
        bytes(&zeroed::<SndPcmStatus>()),
    )
    .unwrap();
    let kept: Vec<u32> = k.0.iter().map(|(cmd, _)| cmd & 0xff).collect();
    assert_eq!(kept, [0x04, 0x11, 0x13, 0x43, 0x40]);
    let (buffer, period, rate) = (c.pcm.buffer_size, c.pcm.period_size, c.pcm.rate);

    // The driver dies; the next one is told what was kept.
    let mut next = Card::new();
    k.replay(&mut next);
    assert_eq!(next.pcm.state, STATE_PREPARED);
    assert_eq!(
        (next.pcm.buffer_size, next.pcm.period_size, next.pcm.rate),
        (buffer, period, rate)
    );
    // The program writes on, and the stream starts at its threshold.
    assert_eq!(next.writei(1024, 7, false), Ok(1024));
    assert!(!next.s.running());
    assert_eq!(next.writei(1024, 7, false), Ok(1024));
    assert_eq!(next.pcm.state, STATE_RUNNING);
}
