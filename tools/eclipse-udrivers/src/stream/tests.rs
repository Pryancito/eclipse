use super::*;

/// A controller that plays where it is told to be.
pub struct Fake {
    pub mem: Vec<u8>,
    pub running: bool,
    pub rate: u32,
    pub runs: u32,
    pub resumes: u32,
    pub pos: usize,
}

impl Fake {
    pub fn new() -> Fake {
        Fake {
            mem: vec![0; RING_LEN],
            running: false,
            rate: 0,
            runs: 0,
            resumes: 0,
            pos: 0,
        }
    }
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
        self.resumes += 1;
    }
    fn position(&self) -> usize {
        self.pos
    }
}

fn frames(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[test]
fn the_samples_queued_leave_out_the_lead_ahead_of_them() {
    let mut s = Stream::new(Fake::new());
    assert_eq!(s.write(&frames(&[1, 1, 2, 2])), 8);
    assert_eq!(s.queued(), LEAD + 8);
    assert_eq!(s.queued_samples(), 8);
    s.start().unwrap();
    s.out.pos = LEAD + 4;
    s.follow();
    assert_eq!((s.queued(), s.queued_samples()), (4, 4));
}

#[test]
fn the_volume_scales_and_mutes_each_side() {
    let mut s = Stream::new(Fake::new());
    s.gain = Gain {
        left: 50,
        right: 100,
        mute_left: false,
        mute_right: true,
    };
    s.write(&frames(&[1000, 1000, -2000, -2000]));
    assert_eq!(&s.out.mem[LEAD..LEAD + 8], &frames(&[500, 0, -1000, 0])[..]);
}

#[test]
fn a_pause_keeps_the_place_and_what_is_queued() {
    let mut s = Stream::new(Fake::new());
    s.write(&frames(&[7; 64]));
    s.start().unwrap();
    s.out.pos = LEAD + 32;
    s.follow();
    s.pause();
    assert!(!s.running() && s.paused());
    assert_eq!(s.queued_samples(), 96);
    s.start().unwrap();
    assert_eq!((s.out.runs, s.out.resumes), (1, 1));
    assert_eq!(s.queued_samples(), 96);
}

#[test]
fn a_rewind_takes_back_the_last_written_and_a_forward_silences_the_next() {
    let mut s = Stream::new(Fake::new());
    s.write(&frames(&[5; 32]));
    assert_eq!(s.rewind(16), 16);
    assert_eq!(s.queued_samples(), 48);
    assert!(s.out.mem[LEAD + 48..LEAD + 64].iter().all(|b| *b == 0));
    assert_eq!(s.forward(1000), 48, "no more than is queued");
    assert_eq!(s.queued_samples(), 48, "a forward still takes its time");
    assert!(s.out.mem[LEAD..LEAD + 48].iter().all(|b| *b == 0));
}

#[test]
fn another_rate_drops_what_was_queued() {
    let mut s = Stream::new(Fake::new());
    s.set_rate(48000);
    s.write(&frames(&[3; 8]));
    s.set_rate(48000);
    assert_eq!(s.queued_samples(), 16);
    s.set_rate(44100);
    assert_eq!(s.queued(), 0);
    assert!(s.out.mem.iter().all(|b| *b == 0));
}
