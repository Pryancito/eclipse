use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::hda::LEAD;
use crate::served::Node;
use crate::stream::Stream;

/// A stream that plays where it is told to be.
struct Fake {
    mem: Vec<u8>,
    running: bool,
    rate: u32,
    runs: u32,
    pos: usize,
}

impl Output for Fake {
    fn ring(&mut self) -> &mut [u8] {
        &mut self.mem
    }
    fn running(&self) -> bool {
        self.running
    }
    fn run(&mut self, rate: u32) -> Result<(), String> {
        self.running = true;
        self.rate = rate;
        self.runs += 1;
        self.pos = 0;
        Ok(())
    }
    fn stop(&mut self) {
        self.running = false;
    }
    fn resume(&mut self) {
        self.running = true;
    }
    fn position(&self) -> usize {
        self.pos
    }
}

static NOW: AtomicU64 = AtomicU64::new(0);

fn clock() -> u64 {
    NOW.load(Ordering::Relaxed)
}

/// The node over a stream of its own.
struct D {
    s: Stream<Fake>,
    d: Dsp,
}

impl D {
    fn open(&mut self, _flags: u64) -> Result<(u32, u64), i32> {
        self.d.open(&mut self.s)
    }
    fn release(&mut self, fh: u32) {
        self.d.release(&mut self.s, fh)
    }
    fn control(&mut self, cmd: u32, data: &mut [u8]) -> Result<(i64, usize), i32> {
        self.d.control(&mut self.s, cmd, data)
    }
    fn play(&mut self, data: &[u8]) -> Result<usize, i32> {
        self.d.play(&mut self.s, data)
    }
    fn tick(&mut self, node: Option<&Node>) {
        self.d.tick(&mut self.s, node)
    }
}

fn dsp() -> D {
    let mut d = D {
        s: Stream::new(Fake {
            mem: vec![0; RING_LEN],
            running: false,
            rate: 0,
            runs: 0,
            pos: 0,
        }),
        d: Dsp::new(clock),
    };
    d.open(1).unwrap();
    d
}

fn int(d: &mut D, cmd: u32, v: i32) -> i32 {
    let mut b = v.to_ne_bytes().to_vec();
    d.control(cmd, &mut b).unwrap();
    i32::from_ne_bytes(b[..4].try_into().unwrap())
}

fn s16_stereo(d: &mut D) {
    assert_eq!(int(d, SNDCTL_DSP_SETFMT, AFMT_S16_LE), AFMT_S16_LE);
    assert_eq!(int(d, SNDCTL_DSP_CHANNELS, 2), 2);
    assert_eq!(int(d, SNDCTL_DSP_SPEED, 48000), 48000);
}

#[test]
fn a_second_open_is_busy_until_the_first_closes() {
    let mut d = dsp();
    assert_eq!(d.open(1), Err(EBUSY));
    d.release(1);
    assert!(d.open(1).is_ok());
}

#[test]
fn an_open_starts_from_the_oss_defaults_and_can_write() {
    let mut d = dsp();
    assert_eq!(int(&mut d, SNDCTL_DSP_SETFMT, AFMT_QUERY), AFMT_U8);
    assert_eq!(int(&mut d, SOUND_PCM_READ_CHANNELS, 0), 1);
    assert_eq!(int(&mut d, SOUND_PCM_READ_RATE, 0), 8000);
    assert_eq!(d.open(0).err(), Some(EBUSY));
    assert_eq!(d.d.said, POLLOUT);
}

#[test]
fn the_rate_is_the_nearest_the_controller_plays() {
    let mut d = dsp();
    assert_eq!(int(&mut d, SNDCTL_DSP_SPEED, 44000), 44100);
    assert_eq!(int(&mut d, SNDCTL_DSP_SPEED, 0), 8000);
}

#[test]
fn a_format_it_has_not_is_answered_with_the_one_it_keeps() {
    let mut d = dsp();
    assert_eq!(int(&mut d, SNDCTL_DSP_SETFMT, 0x1000), AFMT_U8);
    assert_eq!(int(&mut d, SNDCTL_DSP_SETFMT, AFMT_S16_BE), AFMT_S16_BE);
    assert_eq!(
        int(&mut d, SNDCTL_DSP_GETFMTS, 0) & AFMT_S16_LE,
        AFMT_S16_LE
    );
    assert_eq!(int(&mut d, SNDCTL_DSP_STEREO, 1), 1);
    assert_eq!(int(&mut d, SOUND_PCM_READ_CHANNELS, 0), 2);
}

#[test]
fn a_write_starts_the_stream_at_its_rate_with_the_samples_as_they_were() {
    let mut d = dsp();
    s16_stereo(&mut d);
    let samples: Vec<u8> = [1000i16, -1000, 2000, -2000]
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    assert_eq!(d.play(&samples), Ok(8));
    assert!(d.s.out.running);
    assert_eq!(d.s.out.rate, 48000);
    assert_eq!(&d.s.out.mem[LEAD..LEAD + 8], &samples[..]);
}

#[test]
fn mono_u8_is_made_16_bit_stereo() {
    let mut d = dsp();
    assert_eq!(d.play(&[0x80, 0xff, 0x00]), Ok(3));
    let got: Vec<i16> = d.s.out.mem[LEAD..LEAD + 12]
        .chunks(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    assert_eq!(got, [0, 0, 0x7f00, 0x7f00, -0x8000, -0x8000]);
}

#[test]
fn big_endian_and_signed_8_bit_come_out_the_same_sound() {
    let mut d = dsp();
    int(&mut d, SNDCTL_DSP_SETFMT, AFMT_S16_BE);
    d.play(&0x1234i16.to_be_bytes()).unwrap();
    assert_eq!(&d.s.out.mem[LEAD..LEAD + 4], &[0x34, 0x12, 0x34, 0x12]);
    int(&mut d, SNDCTL_DSP_SETFMT, AFMT_S8);
    d.play(&[0xff]).unwrap();
    assert_eq!(&d.s.out.mem[LEAD + 4..LEAD + 8], &[0x00, 0xff, 0x00, 0xff]);
}

#[test]
fn a_full_ring_takes_nothing_and_says_so_until_it_has_played() {
    let mut d = dsp();
    s16_stereo(&mut d);
    let lots = vec![1u8; 2 * RING_LEN];
    let taken = d.play(&lots).unwrap();
    assert_eq!(taken, RING_LEN - GUARD - LEAD);
    assert_eq!(d.play(&lots), Err(EAGAIN));
    d.tick(None);
    assert_eq!(d.d.said, 0, "no room: the writer waits");
    d.s.out.pos = LEAD + 2 * FRAGMENT;
    d.tick(None);
    assert_eq!(d.d.said, POLLOUT);
    assert_eq!(d.play(&lots), Ok(LEAD + 2 * FRAGMENT));
}

#[test]
fn half_a_frame_is_left_for_the_next_write() {
    let mut d = dsp();
    s16_stereo(&mut d);
    assert_eq!(d.play(&[1, 2, 3, 4, 5, 6]), Ok(4));
    assert_eq!(d.play(&[1, 2]), Ok(0));
}

#[test]
fn space_and_delay_are_in_the_clients_bytes() {
    let mut d = dsp();
    // Mono U8: one byte a frame, a quarter of the ring's.
    assert_eq!(int(&mut d, SNDCTL_DSP_GETBLKSIZE, 0), (FRAGMENT / 4) as i32);
    d.play(&[0x80; 1000]).unwrap();
    // The lead counts as queued: it plays before the samples.
    assert_eq!(
        int(&mut d, SNDCTL_DSP_GETODELAY, 0),
        (1000 + LEAD / 4) as i32
    );
    let mut info = [0u8; 16];
    d.control(SNDCTL_DSP_GETOSPACE, &mut info).unwrap();
    let field = |i: usize| i32::from_ne_bytes(info[4 * i..4 * i + 4].try_into().unwrap());
    assert_eq!(field(1), ((RING_LEN - GUARD) / FRAGMENT) as i32);
    assert_eq!(field(2), (FRAGMENT / 4) as i32);
    assert_eq!(field(3), ((RING_LEN - GUARD - LEAD) / 4 - 1000) as i32);
    assert_eq!(field(0), field(3) / field(2));
}

#[test]
fn the_play_pointer_counts_bytes_and_fragments_played() {
    let mut d = dsp();
    s16_stereo(&mut d);
    d.play(&vec![0u8; 3 * FRAGMENT]).unwrap();
    d.s.out.pos = 2 * FRAGMENT + 8;
    d.tick(None);
    let mut info = [0u8; 12];
    d.control(SNDCTL_DSP_GETOPTR, &mut info).unwrap();
    let field = |b: &[u8], i: usize| i32::from_ne_bytes(b[4 * i..4 * i + 4].try_into().unwrap());
    assert_eq!(field(&info, 0), (2 * FRAGMENT + 8) as i32);
    assert_eq!(field(&info, 1), 2);
    // The next call counts from there.
    d.control(SNDCTL_DSP_GETOPTR, &mut info).unwrap();
    assert_eq!(field(&info, 1), 0);
}

#[test]
fn another_rate_drops_what_was_queued_at_the_last() {
    let mut d = dsp();
    s16_stereo(&mut d);
    d.play(&[9; 64]).unwrap();
    int(&mut d, SNDCTL_DSP_SPEED, 44100);
    assert!(!d.s.out.running);
    assert_eq!(int(&mut d, SNDCTL_DSP_GETODELAY, 0), 0);
    assert!(d.s.out.mem.iter().all(|b| *b == 0));
    d.play(&[9; 64]).unwrap();
    assert_eq!((d.s.out.rate, d.s.out.runs), (44100, 2));
}

#[test]
fn a_stream_left_with_nothing_stops_after_a_while() {
    let mut d = dsp();
    s16_stereo(&mut d);
    d.play(&[1; 64]).unwrap();
    d.s.out.pos = LEAD + 64;
    NOW.store(1_000, Ordering::Relaxed);
    d.tick(None);
    assert!(d.s.out.running, "a gap is not the end");
    NOW.store(1_000 + IDLE_NS + 1, Ordering::Relaxed);
    d.tick(None);
    assert!(!d.s.out.running);
    // And it starts again on the next write.
    d.play(&[1; 64]).unwrap();
    assert_eq!(d.s.out.runs, 2);
}

#[test]
fn what_it_has_not_is_einval_and_capture_is_eio() {
    let mut d = dsp();
    let mut b = [0u8; 4];
    assert_eq!(d.control(0x8004_500d, &mut [0; 16]), Err(EINVAL));
    assert_eq!(d.control(SNDCTL_DSP_SETDUPLEX, &mut b), Err(EIO));
    assert_eq!(d.control(SNDCTL_DSP_SYNC, &mut b), Ok((0, 0)));
    assert_eq!(int(&mut d, OSS_GETVERSION, 0), OSS_VERSION);
}
