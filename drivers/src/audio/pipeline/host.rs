//! The host side of a playback pipeline: one [`HostStream`] per client.
//!
//! In Sound Open Firmware a playback pipeline is `host -> (src) -> mixer ->
//! volume -> dai`: the host component owns a buffer the application fills,
//! the mixer pulls from every host buffer at the DAI's pace and sums them,
//! and the DAI never sees the applications at all. This module is the host
//! component and the client-facing arithmetic that goes with it. A stream
//! holds the client's PCM, already converted to the link's format (the
//! [`Resampler`] runs on the way in), and hands it out in whatever block
//! size the DAI fill asks for. It knows nothing about the hardware ring,
//! which is what lets it be tested to the byte without one.
//!
//! Everything a client is told -- free, queued, delay, buffer -- is derived
//! from this buffer, and one identity holds at every fill level:
//!
//! ```text
//! buffer_bytes - queued_bytes == what write() accepts right now
//! ```
//!
//! The ALSA node computes `avail = buffer_size - queued` itself, offers that
//! many frames, and PulseAudio's `alsa-sink.c` aborts (`try_recover`:
//! `pa_assert(err != -EAGAIN)`) if a write it was just told had room comes
//! back with 0. Three separate ways of breaking that identity each cost the
//! daemon a SIGABRT (see [`client_counts`]); it is kept here in one place,
//! by construction: both counts come out of the same function, from the
//! same occupancy figure.
//!
//! Frame SIZE is the same on both sides of the converter (S16LE stereo
//! throughout); only the frame COUNT scales, by `client_rate / LINK_RATE`.

use super::src::Resampler;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// The one rate the link is ever programmed at. A client at another rate is
/// resampled into the stream on the way in; a client at exactly this rate
/// takes a byte-for-byte path with identity conversions.
pub const LINK_RATE: u32 = 48000;

/// Link frames held back below the free space when sizing a resampled
/// write. The converter's output for N input frames is `N * fout / fin`
/// give or take one (the fractional phase carries between calls); this keeps
/// that one, and a couple more, from landing past what fits.
pub const SRC_SLACK_FRAMES: usize = 4;

/// Client rates outside this range are clamped rather than refused: the
/// converter's window covers ratios down to about a third, and nothing a
/// desktop plays sits outside it.
pub const CLIENT_RATE_MIN: u32 = 8000;
pub const CLIENT_RATE_MAX: u32 = 192_000;

/// How many CLIENT frames a resampled write may take when the stream has
/// `free_link_frames` free: the largest count whose converted output is sure
/// to fit. Zero when there is no room for the slack -- the client then sees
/// a full buffer and polls, which is the normal answer.
pub fn accept_client_frames(free_link_frames: usize, fin: u32, fout: u32) -> usize {
    if fin == 0 || fout == 0 {
        return 0;
    }
    let usable = free_link_frames.saturating_sub(SRC_SLACK_FRAMES);
    (usable as u64 * fin as u64 / fout as u64) as usize
}

/// `link_bytes` of stream, seen as client bytes: the frame count scaled by
/// `fin / fout`, floored to a whole frame. Used for what is QUEUED and for
/// the buffer size, where under-reporting is the safe direction (a client
/// thinks slightly less is waiting, never more than it wrote).
pub fn link_to_client_bytes(link_bytes: usize, fin: u32, fout: u32, frame: usize) -> usize {
    if frame == 0 || fout == 0 {
        return 0;
    }
    let frames = (link_bytes / frame) as u64;
    (frames * fin as u64 / fout as u64) as usize * frame
}

/// `client_bytes` of a client's request, as link bytes: the inverse of
/// [`link_to_client_bytes`], floored to a whole frame. For `rewind` and
/// `forward`, which name an amount of the client's own PCM.
pub fn client_to_link_bytes(client_bytes: usize, fin: u32, fout: u32, frame: usize) -> usize {
    if frame == 0 || fin == 0 {
        return 0;
    }
    let frames = (client_bytes / frame) as u64;
    (frames * fout as u64 / fin as u64) as usize * frame
}

/// The two counts a client sees, `(free, queued)` in client bytes, for a
/// stream of `capacity_link` bytes with `queued_link` of them occupied.
/// `free` is exactly what `write` will accept ([`accept_client_frames`] of
/// the room left, or the room itself without a converter), and `queued` is
/// `buffer - free`, so `buffer - queued == free` holds to the byte.
///
/// The identity is load-bearing (see the module notes). Three ways of
/// losing it have each aborted PulseAudio: converting `queued` on its own
/// (three independently floored conversions plus the slack left `avail` at
/// one to four frames with nothing accepted), bounding the client's buffer
/// with the previous stream's capacity, and -- back when clients wrote
/// straight into the DMA ring -- leaving the driver's own silence out of
/// `queued` while `write` was bounded by it.
pub fn client_counts(
    capacity_link: usize,
    queued_link: usize,
    src: Option<(u32, u32)>,
    frame: usize,
) -> (usize, usize) {
    if frame == 0 {
        return (0, 0);
    }
    let room = capacity_link.saturating_sub(queued_link);
    let (buffer, free) = match src {
        Some((fin, fout)) => (
            link_to_client_bytes(capacity_link, fin, fout, frame),
            accept_client_frames(room / frame, fin, fout) * frame,
        ),
        None => (capacity_link, room),
    };
    (free, buffer.saturating_sub(free))
}

/// A stream's capacity as a client at `client_rate` counts it: identical to
/// the link's when the client is at [`LINK_RATE`], scaled by the rate ratio
/// otherwise. The rate is clamped the way `set_params` clamps it, so this is
/// exactly what `buffer_bytes` reports after that rate is set, and a front
/// end can size a buffer for a rate before applying it.
pub fn client_buffer_bytes(capacity_link: usize, client_rate: u32, frame: usize) -> usize {
    let client_rate = client_rate.clamp(CLIENT_RATE_MIN, CLIENT_RATE_MAX);
    if client_rate == LINK_RATE {
        capacity_link
    } else {
        link_to_client_bytes(capacity_link, client_rate, LINK_RATE, frame)
    }
}

/// How much of a `rewind`/`forward` request can actually be honoured: never
/// more than is queued, and always a whole number of frames.
///
/// The frame rounding is not cosmetic: a byte count that is not a multiple
/// of the frame size leaves the stream straddling a sample, and from there
/// on every left sample is read as a right one.
pub fn honourable_bytes(bytes: usize, queued: usize, frame: usize) -> usize {
    if frame == 0 {
        return 0;
    }
    bytes.min(queued) / frame * frame
}

/// One client's playback stream: its PCM, in link format, waiting for the
/// mixer.
///
/// The stream has a client-visible lifecycle that mirrors ALSA's: it is
/// *started* (contributing to the mix) once the first write lands unless a
/// start hold is set, in which case it starts when the hold is released;
/// it can be *paused* (it keeps its PCM and contributes silence); and a
/// prepare (`set_params`) or `reset` drops the PCM and stops it. None of
/// that touches the DAI: the link runs for as long as any stream is
/// started, and a stream that has nothing to give simply adds silence.
pub struct HostStream {
    client_rate: u32,
    channels: usize,
    /// Link bytes the stream may hold. The client's `buffer_bytes` is this,
    /// scaled to its rate.
    capacity: usize,
    src: Option<Resampler>,
    /// Link-format PCM, oldest first, always a whole number of frames.
    buf: VecDeque<u8>,
    started: bool,
    hold: bool,
    paused: bool,
    src_in: Vec<i16>,
    src_out: Vec<i16>,
    src_bytes: Vec<u8>,
    /// Link bytes accepted from the client, in total.
    written: u64,
    /// Link bytes handed to the mixer, in total.
    pulled: u64,
    /// Pulls that found a started, unpaused stream empty, counted once per
    /// run of them: the client fell behind.
    underruns: u64,
    empty_since_last_pull: bool,
}

impl HostStream {
    /// A stream of `capacity` link bytes at [`LINK_RATE`], `channels`
    /// interleaved, with the client at the link rate (no converter).
    pub fn new(capacity: usize, channels: usize) -> Self {
        HostStream {
            client_rate: LINK_RATE,
            channels: channels.max(1),
            capacity,
            src: None,
            buf: VecDeque::with_capacity(capacity),
            started: false,
            hold: false,
            paused: false,
            src_in: Vec::new(),
            src_out: Vec::new(),
            src_bytes: Vec::new(),
            written: 0,
            pulled: 0,
            underruns: 0,
            empty_since_last_pull: false,
        }
    }

    pub fn frame_bytes(&self) -> usize {
        self.channels * 2
    }

    /// `(client_rate, LINK_RATE)` when a converter is in the path.
    fn src_rates(&self) -> Option<(u32, u32)> {
        self.src.as_ref().map(|s| s.rates())
    }

    /// Link bytes as the client counts them (identity without a converter).
    pub fn to_client_bytes(&self, link_bytes: usize) -> usize {
        match self.src_rates() {
            Some((fin, fout)) => link_to_client_bytes(link_bytes, fin, fout, self.frame_bytes()),
            None => link_bytes,
        }
    }

    /// A client's byte count as link bytes (identity without a converter).
    pub fn to_link_bytes(&self, client_bytes: usize) -> usize {
        match self.src_rates() {
            Some((fin, fout)) => client_to_link_bytes(client_bytes, fin, fout, self.frame_bytes()),
            None => client_bytes,
        }
    }

    /// ALSA's prepare: the client's rate for what follows (clamped, and
    /// returned), a fresh converter, and whatever was queued dropped. The
    /// stream is stopped until its next write (or hold release) starts it.
    pub fn set_params(&mut self, rate: u32) -> u32 {
        let rate = rate.clamp(CLIENT_RATE_MIN, CLIENT_RATE_MAX);
        self.client_rate = rate;
        self.src = if rate != LINK_RATE {
            Some(Resampler::new(rate, LINK_RATE, self.channels))
        } else {
            None
        };
        self.buf.clear();
        self.started = false;
        self.paused = false;
        self.empty_since_last_pull = false;
        rate
    }

    pub fn client_rate(&self) -> u32 {
        self.client_rate
    }

    pub fn is_resampling(&self) -> bool {
        self.src.is_some()
    }

    /// `(free, queued)` in client bytes, from the one occupancy figure.
    pub fn counts(&self) -> (usize, usize) {
        client_counts(
            self.capacity,
            self.buf.len(),
            self.src_rates(),
            self.frame_bytes(),
        )
    }

    /// Client bytes a write may take right now.
    pub fn free_bytes(&self) -> usize {
        self.counts().0
    }

    /// Client bytes queued, as `buffer_bytes - free_bytes`.
    pub fn queued_bytes(&self) -> usize {
        self.counts().1
    }

    /// The stream's capacity in client bytes.
    pub fn buffer_bytes(&self) -> usize {
        client_buffer_bytes(self.capacity, self.client_rate, self.frame_bytes())
    }

    /// What `buffer_bytes` will be once `rate` is set.
    pub fn buffer_bytes_at(&self, rate: u32) -> usize {
        client_buffer_bytes(self.capacity, rate, self.frame_bytes())
    }

    /// Link bytes queued: what the mixer can still pull.
    pub fn queued_link(&self) -> usize {
        self.buf.len()
    }

    /// Queue client PCM. Takes whole frames only, never more than
    /// [`free_bytes`](HostStream::free_bytes) says, and returns the CLIENT
    /// bytes taken. Starts the stream unless a start hold is set.
    pub fn write(&mut self, pcm: &[u8]) -> usize {
        let frame = self.frame_bytes();
        let n = self.free_bytes().min(pcm.len()) / frame * frame;
        if n == 0 {
            return 0;
        }
        let room = self.capacity - self.buf.len();
        let link_len = if self.src.is_some() {
            let produced = self.resample(&pcm[..n]);
            // Cannot happen (the slack in `accept_client_frames` covers the
            // converter's ±1 frame, and the tests hold it to that), but a
            // stream past its capacity is the one outcome never worth
            // risking: drop the excess rather than overrun.
            let take = produced.min(room) / frame * frame;
            self.buf.extend(self.src_bytes[..take].iter().copied());
            take
        } else {
            self.buf.extend(pcm[..n].iter().copied());
            n
        };
        self.written += link_len as u64;
        if !self.hold {
            self.started = true;
        }
        n
    }

    /// Convert whole frames of client S16LE `pcm` through the resampler into
    /// `self.src_bytes` (link-rate S16LE). Returns the link byte count. Only
    /// called with a converter present.
    fn resample(&mut self, pcm: &[u8]) -> usize {
        let (samples, _) = pcm.as_chunks::<2>();
        self.src_in.clear();
        self.src_in
            .extend(samples.iter().map(|s| i16::from_le_bytes(*s)));
        self.src_out.clear();
        if let Some(src) = self.src.as_mut() {
            src.process(&self.src_in, &mut self.src_out);
        }
        self.src_bytes.clear();
        self.src_bytes
            .extend(self.src_out.iter().flat_map(|s| s.to_le_bytes()));
        self.src_bytes.len()
    }

    /// Whether the mixer should take from this stream at all: started, not
    /// paused, not held.
    pub fn is_active(&self) -> bool {
        self.started && !self.paused && !self.hold
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Hand the mixer the next `dst.len()` link bytes, or as many as are
    /// queued. Returns how many were written into `dst`; the rest of `dst`
    /// is the caller's to treat as silence. An inactive stream gives
    /// nothing. A started stream found empty is an underrun of the
    /// client's, counted once per run.
    pub fn pull(&mut self, dst: &mut [u8]) -> usize {
        if !self.is_active() {
            return 0;
        }
        let frame = self.frame_bytes();
        let n = dst.len().min(self.buf.len()) / frame * frame;
        if n == 0 {
            if !dst.is_empty() && !self.empty_since_last_pull {
                self.underruns += 1;
                self.empty_since_last_pull = true;
            }
            return 0;
        }
        self.empty_since_last_pull = false;
        for (d, s) in dst[..n].iter_mut().zip(self.buf.drain(..n)) {
            *d = s;
        }
        self.pulled += n as u64;
        n
    }

    /// Drop the most recently queued `bytes` (client bytes, whole frames,
    /// no more than is queued). Returns the client bytes dropped.
    pub fn rewind(&mut self, client_bytes: usize) -> usize {
        let link = self.to_link_bytes(client_bytes);
        let n = honourable_bytes(link, self.buf.len(), self.frame_bytes());
        let keep = self.buf.len() - n;
        self.buf.truncate(keep);
        self.to_client_bytes(n)
    }

    /// Skip the next `bytes` (client bytes) of queued PCM: they still take
    /// their time to play, as silence. Returns the client bytes skipped.
    pub fn forward(&mut self, client_bytes: usize) -> usize {
        let link = self.to_link_bytes(client_bytes);
        let n = honourable_bytes(link, self.buf.len(), self.frame_bytes());
        for b in self.buf.iter_mut().take(n) {
            *b = 0;
        }
        self.to_client_bytes(n)
    }

    /// Drop everything and stop contributing. The converter and the rate
    /// stay (a `reset` is not a prepare).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.started = false;
        self.paused = false;
        self.empty_since_last_pull = false;
    }

    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Hold the start back (writes queue without starting the stream), or
    /// release it (a stream with PCM waiting starts now). Returns whether
    /// the stream is started afterwards.
    pub fn set_start_hold(&mut self, hold: bool) -> bool {
        self.hold = hold;
        if !hold && !self.buf.is_empty() {
            self.started = true;
        }
        self.started
    }

    pub fn is_held(&self) -> bool {
        self.hold
    }

    /// `(link bytes written, link bytes pulled, underruns)` for diagnostics.
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.written, self.pulled, self.underruns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 4;
    const CAP: usize = 12288 * FRAME;

    /// Rates a desktop actually plays, paired against the fixed link.
    const CLIENT_RATES: [u32; 9] = [
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 96000, 192_000,
    ];

    fn pcm(frames: usize) -> Vec<u8> {
        (0..frames)
            .flat_map(|f| {
                let s = ((f % 200) as i16 - 100) * 100;
                [s.to_le_bytes(), s.to_le_bytes()].concat()
            })
            .collect()
    }

    /// The invariant everything rests on: whatever the stream has free, the
    /// client frames `accept_client_frames` sizes never convert into more
    /// link frames than that. Checked against the converter's own sizing
    /// hint for every rate and every free count up to a full buffer.
    #[test]
    fn an_accepted_write_always_fits_after_conversion() {
        for &fin in &CLIENT_RATES {
            let r = Resampler::new(fin, LINK_RATE, 2);
            for free in 0..=16384usize {
                let take = accept_client_frames(free, fin, LINK_RATE);
                let out = r.out_frames_hint(take);
                assert!(
                    take == 0 || out <= free,
                    "{} Hz: {} free, took {} -> {}",
                    fin,
                    free,
                    take,
                    out
                );
            }
        }
    }

    #[test]
    fn nothing_is_accepted_without_room_for_the_slack() {
        for free in 0..=SRC_SLACK_FRAMES {
            assert_eq!(accept_client_frames(free, 44100, LINK_RATE), 0);
            assert_eq!(accept_client_frames(free, 192_000, LINK_RATE), 0);
        }
        assert_eq!(accept_client_frames(5, 44100, LINK_RATE), 0);
        assert_eq!(
            accept_client_frames(SRC_SLACK_FRAMES + 48, 44100, LINK_RATE),
            44
        );
    }

    #[test]
    fn link_to_client_scales_the_frame_count_and_floors_to_a_frame() {
        assert_eq!(
            link_to_client_bytes(48000 * FRAME, 44100, LINK_RATE, FRAME),
            44100 * FRAME
        );
        assert_eq!(
            link_to_client_bytes(48000 * FRAME, 96000, LINK_RATE, FRAME),
            96000 * FRAME
        );
        assert_eq!(
            link_to_client_bytes(10 * FRAME, 44100, LINK_RATE, FRAME),
            9 * FRAME
        );
        assert_eq!(
            link_to_client_bytes(10 * FRAME + 3, 48000, LINK_RATE, FRAME),
            10 * FRAME
        );
        assert_eq!(link_to_client_bytes(100, 44100, 0, FRAME), 0);
        assert_eq!(link_to_client_bytes(100, 44100, LINK_RATE, 0), 0);
    }

    #[test]
    fn client_to_link_is_the_inverse_direction() {
        assert_eq!(
            client_to_link_bytes(44100 * FRAME, 44100, LINK_RATE, FRAME),
            48000 * FRAME
        );
        assert_eq!(
            client_to_link_bytes(9 * FRAME, 44100, LINK_RATE, FRAME),
            9 * FRAME
        );
        assert_eq!(client_to_link_bytes(100, 0, LINK_RATE, FRAME), 0);
    }

    /// `avail` (the ALSA node's `buffer - queued`) equals what `write`
    /// accepts at EVERY fill level, for every rate: both come out of
    /// `client_counts` from the same occupancy figure.
    #[test]
    fn avail_equals_what_write_accepts_at_every_fill_level() {
        for src in [
            None,
            Some((44_100, LINK_RATE)),
            Some((96_000, LINK_RATE)),
            Some((8_000, LINK_RATE)),
        ] {
            let buffer = match src {
                Some((fin, fout)) => link_to_client_bytes(CAP, fin, fout, FRAME),
                None => CAP,
            };
            for occupied_frames in 0..=12288usize {
                let occupied = occupied_frames * FRAME;
                let (free, queued) = client_counts(CAP, occupied, src, FRAME);
                let accept = match src {
                    Some((fin, fout)) => {
                        accept_client_frames((CAP - occupied) / FRAME, fin, fout) * FRAME
                    }
                    None => CAP - occupied,
                };
                assert_eq!(free, accept, "{:?} at {}", src, occupied_frames);
                assert_eq!(buffer - queued, accept, "{:?} at {}", src, occupied_frames);
                assert!(queued <= buffer);
            }
        }
    }

    /// The formula `client_counts` replaced: `queued` converted on its own.
    /// Near a full buffer it left `avail` at one to four frames with nothing
    /// accepted -- PulseAudio's first SIGABRT.
    #[test]
    fn converting_the_occupancy_on_its_own_left_avail_positive_with_nothing_accepted() {
        let fin = 44100;
        let buffer = link_to_client_bytes(CAP, fin, LINK_RATE, FRAME);
        let mut broken_levels = 0;
        for occupied_frames in 0..=12288usize {
            let occupied = occupied_frames * FRAME;
            let old_queued = link_to_client_bytes(occupied, fin, LINK_RATE, FRAME);
            let old_avail = buffer.saturating_sub(old_queued);
            let accept = accept_client_frames((CAP - occupied) / FRAME, fin, LINK_RATE) * FRAME;
            if old_avail > 0 && accept == 0 {
                broken_levels += 1;
            }
        }
        assert!(
            broken_levels > 0,
            "the old formula should have had a zero-accept level with avail > 0"
        );
    }

    #[test]
    fn queued_is_monotonic_in_the_occupancy_and_bounded_by_the_buffer() {
        for &fin in &CLIENT_RATES {
            let src = if fin == LINK_RATE {
                None
            } else {
                Some((fin, LINK_RATE))
            };
            let buffer = match src {
                Some((fin, fout)) => link_to_client_bytes(CAP, fin, fout, FRAME),
                None => CAP,
            };
            let mut last = 0usize;
            for occupied_frames in 0..=12288usize {
                let (_, q) = client_counts(CAP, occupied_frames * FRAME, src, FRAME);
                assert!(q >= last, "{} Hz: queued went {} -> {}", fin, last, q);
                assert!(q <= buffer);
                last = q;
            }
        }
    }

    /// The capacity a front end sizes a buffer with, for a rate it has not
    /// applied yet, is the capacity the stream reports once it has.
    #[test]
    fn the_capacity_promised_for_a_rate_is_the_capacity_after_it_is_set() {
        for &rate in &CLIENT_RATES {
            let mut s = HostStream::new(CAP, 2);
            let promised = s.buffer_bytes_at(rate);
            s.set_params(rate);
            assert_eq!(promised, s.buffer_bytes(), "{} Hz", rate);
            assert_eq!(promised % FRAME, 0);
        }
        assert_eq!(client_buffer_bytes(CAP, 48_000, FRAME), 49152);
        assert_eq!(client_buffer_bytes(CAP, 44_100, FRAME), 11289 * FRAME);
        assert_eq!(
            client_buffer_bytes(CAP, 4_000, FRAME),
            client_buffer_bytes(CAP, CLIENT_RATE_MIN, FRAME)
        );
        assert_eq!(
            client_buffer_bytes(CAP, 1_000_000, FRAME),
            client_buffer_bytes(CAP, CLIENT_RATE_MAX, FRAME)
        );
    }

    /// The whole client-facing contract, driven end to end through a real
    /// stream with a real converter: a client that reads `free`, offers
    /// exactly that, and is never handed 0 while `free > 0`; `queued` that
    /// is exactly `buffer - free` after every write and every pull; and a
    /// stream that never holds more than its capacity.
    #[test]
    fn a_client_that_offers_what_free_says_is_never_refused() {
        for &rate in &CLIENT_RATES {
            let mut s = HostStream::new(CAP, 2);
            s.set_params(rate);
            let buffer = s.buffer_bytes();
            let mut block = alloc::vec![0u8; 1024];
            let mut refused = 0;
            for step in 0..400 {
                let free = s.free_bytes();
                assert_eq!(buffer - s.queued_bytes(), free, "{} Hz step {}", rate, step);
                if free > 0 {
                    let data = pcm(free / FRAME);
                    let n = s.write(&data);
                    assert!(
                        n > 0,
                        "{} Hz step {}: free {} but write took 0",
                        rate,
                        step,
                        free
                    );
                    assert_eq!(n, free, "{} Hz: took {} of the {} offered", rate, n, free);
                } else {
                    refused += 1;
                }
                assert!(s.queued_link() <= CAP);
                assert_eq!(buffer - s.queued_bytes(), s.free_bytes());
                // The DAI takes a block.
                let got = s.pull(&mut block);
                assert_eq!(got % FRAME, 0);
            }
            assert!(refused < 400, "{} Hz: never got to write", rate);
        }
    }

    #[test]
    fn a_write_at_the_link_rate_is_byte_for_byte() {
        let mut s = HostStream::new(CAP, 2);
        assert_eq!(s.set_params(48_000), 48_000);
        assert!(!s.is_resampling());
        let data = pcm(100);
        assert_eq!(s.write(&data), data.len());
        let mut out = alloc::vec![0u8; data.len()];
        assert_eq!(s.pull(&mut out), data.len());
        assert_eq!(out, data);
    }

    #[test]
    fn a_resampled_write_lands_at_the_link_rate() {
        let mut s = HostStream::new(CAP, 2);
        assert_eq!(s.set_params(44_100), 44_100);
        assert!(s.is_resampling());
        let data = pcm(4410); // 100 ms
        assert_eq!(s.write(&data), data.len());
        // ~4800 link frames, less the converter's look-ahead.
        let link = s.queued_link() / FRAME;
        assert!((4700..=4800).contains(&link), "{} link frames", link);
        // What the client is told it has queued is its own count of it.
        let q = s.queued_bytes() / FRAME;
        assert!(q <= 4410 && q >= 4300, "{} client frames", q);
    }

    #[test]
    fn rates_are_clamped_the_way_the_buffer_is_sized() {
        let mut s = HostStream::new(CAP, 2);
        assert_eq!(s.set_params(4_000), CLIENT_RATE_MIN);
        assert_eq!(s.set_params(1_000_000), CLIENT_RATE_MAX);
        assert_eq!(
            s.buffer_bytes(),
            client_buffer_bytes(CAP, CLIENT_RATE_MAX, FRAME)
        );
    }

    #[test]
    fn the_first_write_starts_the_stream_unless_held() {
        let mut s = HostStream::new(CAP, 2);
        assert!(!s.is_active());
        s.write(&pcm(10));
        assert!(s.is_active());

        let mut h = HostStream::new(CAP, 2);
        h.set_start_hold(true);
        h.write(&pcm(10));
        assert!(!h.is_active(), "held: not started by the write");
        assert!(!h.is_started() && h.is_held());
        let mut out = [0u8; 40];
        assert_eq!(h.pull(&mut out), 0, "held: the mixer gets nothing");
        assert_eq!(h.queued_bytes(), 40, "held: but the PCM is queued");
        assert!(h.set_start_hold(false));
        assert!(h.is_active());
        assert_eq!(h.pull(&mut out), 40);

        // A hold released over an empty stream starts nothing.
        let mut e = HostStream::new(CAP, 2);
        e.set_start_hold(true);
        assert!(!e.set_start_hold(false));
        assert!(!e.is_active());
    }

    #[test]
    fn a_paused_stream_keeps_its_pcm_and_gives_silence() {
        let mut s = HostStream::new(CAP, 2);
        s.write(&pcm(10));
        s.pause();
        assert!(!s.is_active());
        let mut out = [0xffu8; 40];
        assert_eq!(s.pull(&mut out), 0);
        assert_eq!(s.queued_bytes(), 40);
        // Writes are still taken while paused (PulseAudio fills a corked sink).
        assert_eq!(s.write(&pcm(5)), 20);
        s.resume();
        assert_eq!(s.pull(&mut out), 40);
        assert_eq!(s.queued_bytes(), 20);
    }

    #[test]
    fn a_pull_from_an_empty_started_stream_is_one_underrun_per_run() {
        let mut s = HostStream::new(CAP, 2);
        s.write(&pcm(2));
        let mut out = [0u8; 40];
        assert_eq!(s.pull(&mut out), 8);
        assert_eq!(s.stats().2, 0);
        assert_eq!(s.pull(&mut out), 0);
        assert_eq!(s.pull(&mut out), 0);
        assert_eq!(s.stats().2, 1, "a run of empty pulls is one underrun");
        s.write(&pcm(1));
        assert_eq!(s.pull(&mut out), 4);
        assert_eq!(s.pull(&mut out), 0);
        assert_eq!(s.stats().2, 2);
        // A stream that was never started does not underrun.
        let mut idle = HostStream::new(CAP, 2);
        assert_eq!(idle.pull(&mut out), 0);
        assert_eq!(idle.stats().2, 0);
    }

    #[test]
    fn pulls_take_whole_frames_oldest_first() {
        let mut s = HostStream::new(CAP, 2);
        let data = pcm(3);
        s.write(&data);
        let mut out = [0u8; 6]; // a frame and a half
        assert_eq!(s.pull(&mut out), 4);
        assert_eq!(&out[..4], &data[..4]);
        assert_eq!(s.queued_link(), 8);
    }

    #[test]
    fn rewind_drops_the_tail_and_forward_silences_the_head() {
        let mut s = HostStream::new(CAP, 2);
        let data = pcm(10);
        s.write(&data);
        assert_eq!(s.rewind(3 * FRAME + 1), 3 * FRAME, "whole frames only");
        assert_eq!(s.queued_bytes(), 7 * FRAME);
        assert_eq!(s.rewind(100 * FRAME), 7 * FRAME, "never more than queued");
        assert_eq!(s.queued_bytes(), 0);

        s.write(&data);
        assert_eq!(s.forward(2 * FRAME), 2 * FRAME);
        assert_eq!(
            s.queued_bytes(),
            10 * FRAME,
            "skipped PCM still takes its time"
        );
        let mut out = alloc::vec![0xffu8; 10 * FRAME];
        assert_eq!(s.pull(&mut out), 10 * FRAME);
        assert!(out[..2 * FRAME].iter().all(|&b| b == 0));
        assert_eq!(&out[2 * FRAME..], &data[2 * FRAME..]);
    }

    #[test]
    fn rewind_and_forward_are_in_client_bytes_through_the_converter() {
        let mut s = HostStream::new(CAP, 2);
        s.set_params(96_000);
        s.write(&pcm(2000));
        let link_before = s.queued_link();
        let done = s.rewind(200 * FRAME);
        // 200 client frames at 96k are 100 link frames.
        assert_eq!(done, 200 * FRAME);
        assert_eq!(link_before - s.queued_link(), 100 * FRAME);
    }

    #[test]
    fn prepare_and_reset_drop_the_pcm_and_stop_the_stream() {
        let mut s = HostStream::new(CAP, 2);
        s.write(&pcm(10));
        s.pause();
        s.set_params(44_100);
        assert_eq!(s.queued_link(), 0);
        // An empty resampled stream still counts the converter's slack as
        // queued: `buffer - free` is the identity, and `free` holds back
        // `SRC_SLACK_FRAMES` so that nothing accepted can overflow.
        assert!(s.queued_bytes() <= SRC_SLACK_FRAMES * FRAME);
        assert!(!s.is_started() && !s.is_paused());
        s.write(&pcm(10));
        assert!(s.is_active());
        s.reset();
        assert_eq!(s.queued_link(), 0);
        assert!(!s.is_started());
        assert!(s.is_resampling(), "a reset keeps the rate");
    }

    #[test]
    fn a_full_stream_takes_nothing_and_says_so() {
        let mut s = HostStream::new(CAP, 2);
        let data = pcm(12288);
        assert_eq!(s.write(&data), CAP);
        assert_eq!(s.free_bytes(), 0);
        assert_eq!(s.queued_bytes(), CAP);
        assert_eq!(s.write(&data), 0);
        assert_eq!(s.queued_link(), CAP);
    }
}
