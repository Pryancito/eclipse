//! Los tests del nodo de timer (y, dentro, los del PCM).
//!
//! Vivian dentro de `snd.rs` como un `#[cfg(test)] mod timer_tests { ... }` de
//! primer nivel. El fichero se habia pasado de su techo del trinquete
//! (`xtask tamano`), y esto es lo que el propio trinquete prescribe para eso:
//! sacar el modulo a su fichero no toca una linea de produccion, y deja
//! `snd.rs` por debajo del techo general.

use super::*;
use core::convert::TryInto;

fn ts(sec: i64) -> Timespec {
    Timespec { sec, nsec: 0 }
}

#[cfg(test)]
pub(super) mod pcm_tests {
    use super::*;
    use crate::fs::devfs::DspDev;
    use zcore_drivers::DeviceResult;

    pub(super) struct FakeAudio {
        cap: usize,
        queued: Mutex<usize>,
        hold: Mutex<bool>,
        /// `Some(link_rate)`: the ring holds `cap` bytes at THAT rate
        /// and a client at another rate fits `cap * rate / link` of its
        /// own, the way the HDA driver's fixed-rate sink counts. `None`:
        /// the capacity is the same at every rate.
        link: Option<u32>,
        rate: Mutex<u32>,
        /// `open_stream` answers with a fresh `FakeAudio` of the same
        /// capacity: a device that mixes.
        mixing: bool,
    }

    impl FakeAudio {
        /// As [`new`](FakeAudio::new), but every open gets a stream of
        /// its own.
        pub(super) fn mixing(cap: usize) -> Self {
            let mut fake = Self::new(cap);
            fake.mixing = true;
            fake
        }

        pub(super) fn new(cap: usize) -> Self {
            Self {
                cap,
                queued: Mutex::new(0),
                hold: Mutex::new(false),
                link: None,
                rate: Mutex::new(48_000),
                mixing: false,
            }
        }

        /// A ring of `cap` bytes at `link` Hz whose client-visible
        /// capacity scales with the client's rate.
        pub(super) fn fixed_link(cap: usize, link: u32) -> Self {
            Self {
                link: Some(link),
                ..Self::new(cap)
            }
        }

        fn cap_at(&self, rate: u32) -> usize {
            match self.link {
                Some(link) => {
                    let frames = self.cap / BYTES_PER_FRAME as usize;
                    (frames as u64 * rate as u64 / link as u64) as usize * BYTES_PER_FRAME as usize
                }
                None => self.cap,
            }
        }

        fn cap_now(&self) -> usize {
            self.cap_at(*self.rate.lock())
        }

        pub(super) fn held(&self) -> bool {
            *self.hold.lock()
        }

        /// Play out `frames`.
        pub(super) fn drain(&self, frames: u64) {
            let mut queued = self.queued.lock();
            *queued = queued.saturating_sub(frames as usize * BYTES_PER_FRAME as usize);
        }
    }

    impl zcore_drivers::scheme::Scheme for FakeAudio {
        fn name(&self) -> &str {
            "fake-audio"
        }
    }

    impl AudioScheme for FakeAudio {
        fn open_stream(&self) -> DeviceResult<Option<Arc<dyn AudioScheme>>> {
            Ok(if self.mixing {
                Some(Arc::new(FakeAudio::new(self.cap)))
            } else {
                None
            })
        }

        fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
            *self.rate.lock() = rate;
            Ok((rate, channels))
        }

        fn params(&self) -> (u32, u8) {
            match self.link {
                Some(_) => (*self.rate.lock(), 2),
                None => (48_000, 2),
            }
        }

        fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
            let cap = self.cap_now();
            let mut queued = self.queued.lock();
            let room = cap.saturating_sub(*queued);
            let n = room.min(pcm.len());
            *queued += n;
            Ok(n)
        }

        fn free_bytes(&self) -> usize {
            self.cap_now().saturating_sub(*self.queued.lock())
        }

        fn buffer_bytes(&self) -> usize {
            self.cap_now()
        }

        fn buffer_bytes_at(&self, rate: u32) -> usize {
            self.cap_at(rate)
        }

        fn queued_bytes(&self) -> usize {
            *self.queued.lock()
        }

        fn is_playing(&self) -> bool {
            self.queued_bytes() > 0 && !self.held()
        }

        fn reset(&self) -> DeviceResult {
            *self.queued.lock() = 0;
            Ok(())
        }

        fn set_start_hold(&self, hold: bool) -> DeviceResult {
            *self.hold.lock() = hold;
            Ok(())
        }

        fn rewind(&self, bytes: usize) -> DeviceResult<usize> {
            let mut queued = self.queued.lock();
            let n = (*queued).min(bytes);
            *queued -= n;
            Ok(n)
        }

        fn forward(&self, bytes: usize) -> DeviceResult<usize> {
            self.rewind(bytes)
        }
    }

    #[test]
    fn nonblocking_write_returns_partial_frames_and_marks_running() {
        let audio = Arc::new(FakeAudio::new(4 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio, 0);
        pcm.st.lock().state = STATE_PREPARED;
        let samples = [0u8; 8 * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 8,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(xfer.result, 4);
        let st = pcm.st.lock();
        assert_eq!(st.state, STATE_RUNNING);
        assert_eq!(st.appl_ptr, 4);
        assert_eq!(pcm.queued_frames(), 4);
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

    /// HW_PARAMS bounds the buffer with the ring's capacity FOR THE RATE
    /// IT IS ABOUT TO SET, not for the rate the device is at. On the HDA
    /// driver's fixed-rate sink the ring holds 12288 frames of a 48 kHz
    /// client and 11289 of a 44.1 kHz one; sizing a 44.1 kHz request
    /// with the 48 kHz figure (the device is at 48 kHz until the
    /// `set_params` inside HW_PARAMS) granted PulseAudio 999 frames the
    /// ring did not have. `avail = buffer_size - queued` then never
    /// reached zero: with the ring full it still promised 999 frames,
    /// the write took none, and alsa-sink's `try_recover` asserted on
    /// the EAGAIN (`pa_assert(err != -EAGAIN)`) -- the daemon died by
    /// SIGABRT a few seconds into every 44.1 kHz track.
    #[test]
    fn hw_params_sizes_the_buffer_for_the_rate_it_sets() {
        let audio = Arc::new(FakeAudio::fixed_link(
            12288 * BYTES_PER_FRAME as usize,
            48_000,
        ));
        let pcm = PcmDev::new(audio.clone(), 0);
        assert_eq!(audio.buffer_bytes() as u64 / BYTES_PER_FRAME, 12288);

        // The request PulseAudio makes: rate pinned first, buffer left
        // open so it gets the deepest one.
        let mut hp = any_hw_params();
        hp.intervals[PcmDev::IV_RATE].min = 44_100;
        hp.intervals[PcmDev::IV_RATE].max = 44_100;
        pcm.io_control(0x4111, &mut hp as *mut SndPcmHwParams as usize)
            .unwrap();

        let granted = pcm.st.lock().buffer_size;
        let ring_now = audio.buffer_bytes() as u64 / BYTES_PER_FRAME;
        assert_eq!(audio.params().0, 44_100);
        assert_eq!(ring_now, 11289);
        assert!(
            granted <= ring_now,
            "granted {} frames, the ring holds {} at 44.1 kHz",
            granted,
            ring_now
        );
        // ...and not shrunk out of caution either: the deepest buffer
        // that is a whole number of periods.
        assert!(granted > ring_now - 256, "granted only {}", granted);
        assert_eq!(hp.intervals[PcmDev::IV_BUFFER_SIZE].min as u64, granted);

        // The ring takes the whole granted buffer, and once it has,
        // `avail` is zero: nothing is promised that `write` would refuse.
        // (With the stale bound the write stopped at 11289 of 12288 and
        // `avail` stayed at 999.)
        pcm.st.lock().state = STATE_PREPARED;
        let samples = alloc::vec![0u8; 12288 * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 12288,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(xfer.result as u64, granted);
        let st = pcm.st.lock();
        assert_eq!(pcm.avail(&st), 0);
    }

    /// The refine tells the same truth: with the rate pinned it bounds
    /// the buffer by that rate's capacity, and with the rate still open
    /// by the lowest rate the request can end up at (the one `install`
    /// would pick), never by the rate the device happens to be at.
    #[test]
    fn hw_refine_bounds_the_buffer_by_the_requested_rate() {
        let audio = Arc::new(FakeAudio::fixed_link(
            12288 * BYTES_PER_FRAME as usize,
            48_000,
        ));
        let pcm = PcmDev::new(audio.clone(), 0);

        let mut hp = any_hw_params();
        hp.intervals[PcmDev::IV_RATE].min = 44_100;
        hp.intervals[PcmDev::IV_RATE].max = 44_100;
        pcm.io_control(0x4110, &mut hp as *mut SndPcmHwParams as usize)
            .unwrap();
        assert_eq!(hp.intervals[PcmDev::IV_BUFFER_SIZE].max, 11289);

        let mut hp = any_hw_params();
        hp.intervals[PcmDev::IV_RATE].min = 48_000;
        hp.intervals[PcmDev::IV_RATE].max = 48_000;
        pcm.io_control(0x4110, &mut hp as *mut SndPcmHwParams as usize)
            .unwrap();
        assert_eq!(hp.intervals[PcmDev::IV_BUFFER_SIZE].max, 12288);

        // Rate open: the bound every rate in the interval can honour.
        let mut hp = any_hw_params();
        hp.intervals[PcmDev::IV_RATE].min = 22_050;
        hp.intervals[PcmDev::IV_RATE].max = 48_000;
        pcm.io_control(0x4110, &mut hp as *mut SndPcmHwParams as usize)
            .unwrap();
        assert_eq!(
            hp.intervals[PcmDev::IV_BUFFER_SIZE].max,
            12288 * 22_050 / 48_000
        );
        // The device never moved: the refine changes nothing.
        assert_eq!(audio.params().0, 48_000);
    }

    #[test]
    fn nonblocking_write_is_accepted_while_paused() {
        // Linux's `__snd_pcm_lib_xfer` takes PAUSED alongside PREPARED and
        // RUNNING: a corked PulseAudio sink keeps filling the buffer.
        let audio = Arc::new(FakeAudio::new(4 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio, 0);
        pcm.st.lock().state = STATE_PAUSED;
        let samples = [0u8; 2 * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 2,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(xfer.result, 2);
    }

    #[test]
    fn write_after_an_underrun_is_epipe_not_ebadfd() {
        // EBADFD says "this fd is unusable" and a client has nothing to do
        // about it. EPIPE says "the stream broke", which is what
        // `snd_pcm_prepare()` recovers from -- the difference between
        // PulseAudio restarting the stream and PulseAudio aborting.
        let audio = Arc::new(FakeAudio::new(4 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio, 0);
        pcm.st.lock().state = STATE_XRUN;
        let samples = [0u8; BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 1,
        };
        assert!(matches!(
            pcm.writei(&mut xfer, OpenFlags::NON_BLOCK),
            Err(FsError::Broken)
        ));
    }

    #[test]
    fn a_full_ring_is_eagain_before_it_is_an_underrun() {
        // The stall clock starts on the first refusal, so an ordinary full
        // ring must still read as "poll and retry". Only a ring that stays
        // full past a whole buffer becomes an XRUN -- otherwise every
        // scheduling hiccup would reset the stream.
        let audio = Arc::new(FakeAudio::new(BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio, 0);
        {
            let mut st = pcm.st.lock();
            st.state = STATE_RUNNING;
            st.buffer_size = 1;
        }
        let samples = [0u8; BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 1,
        };
        // Fill it, then the next write finds no room.
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert!(matches!(
            pcm.writei(&mut xfer, OpenFlags::NON_BLOCK),
            Err(FsError::Again)
        ));
        assert!(pcm.st.lock().stalled_since.is_some());
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
    }

    #[test]
    fn write_in_an_unusable_state_is_ebadfd_not_einval() {
        // EINVAL means "bad arguments"; alsa-lib treats it as a caller bug
        // and PulseAudio aborts. A state mismatch is EBADFD in Linux.
        for state in [STATE_OPEN, STATE_SETUP] {
            let audio = Arc::new(FakeAudio::new(4 * BYTES_PER_FRAME as usize));
            let pcm = PcmDev::new(audio, 0);
            pcm.st.lock().state = state;
            let samples = [0u8; BYTES_PER_FRAME as usize];
            let mut xfer = SndXferI {
                result: 0,
                buf: samples.as_ptr() as usize as u64,
                frames: 1,
            };
            assert!(matches!(
                pcm.writei(&mut xfer, OpenFlags::NON_BLOCK),
                Err(FsError::BadState)
            ));
        }
    }

    #[test]
    fn nonblocking_write_returns_eagain_when_the_negotiated_buffer_is_full() {
        let audio = Arc::new(FakeAudio::new(2 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio.clone(), 0);
        {
            let mut st = pcm.st.lock();
            st.state = STATE_PREPARED;
            st.buffer_size = 2;
        }
        audio.write(&[0u8; 2 * BYTES_PER_FRAME as usize]).unwrap();
        let before = pcm.st.lock().appl_ptr;
        let samples = [0u8; BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 1,
        };
        assert!(matches!(
            pcm.writei(&mut xfer, OpenFlags::NON_BLOCK),
            Err(FsError::Again)
        ));
        assert_eq!(xfer.result, 0);
        assert_eq!(pcm.st.lock().appl_ptr, before);
    }

    #[test]
    fn pcm_open_is_exclusive_until_the_client_drops() {
        let audio = Arc::new(FakeAudio::new(4 * BYTES_PER_FRAME as usize));
        let pcm = Arc::new(PcmDev::new(audio, 0));
        let first = pcm.open_client().unwrap();
        assert!(matches!(pcm.open_client(), Err(FsError::Busy)));
        drop(first);
        assert!(pcm.open_client().is_ok());
    }

    /// `/dev/dsp<N>` and `/dev/snd/pcmC<N>D0p` are two front ends onto one
    /// unmixed ring, so whichever opens second must get `EBUSY` — in both
    /// directions. That refusal is what sends a bare `mpg123 file.mp3`
    /// from libout123's OSS module on to its ALSA one (and the daemon that
    /// mixes) instead of putting a second writer into PulseAudio's ring.
    #[test]
    fn the_oss_node_and_the_pcm_are_exclusive_against_each_other() {
        let audio = Arc::new(FakeAudio::new(4 * BYTES_PER_FRAME as usize));
        let claim = new_audio_claim();
        let pcm = Arc::new(PcmDev::with_claim(audio.clone(), 0, claim.clone()));
        let dsp = Arc::new(DspDev::with_claim(audio, 0, claim));

        let pcm_client = pcm.open_client().unwrap();
        assert!(
            matches!(dsp.open_client(), Err(FsError::Busy)),
            "/dev/dsp must be EBUSY while the native PCM is open"
        );
        drop(pcm_client);

        let dsp_client = dsp.open_client().unwrap();
        assert!(
            matches!(pcm.open_client(), Err(FsError::Busy)),
            "the native PCM must be EBUSY while /dev/dsp is open"
        );
        drop(dsp_client);

        assert!(pcm.open_client().is_ok(), "closing /dev/dsp frees the card");
    }

    /// On a device that mixes, every open is a stream of its own: the
    /// second `open` of `hw:0,0` is served, not refused, and the two
    /// clients have separate rings, pointers and states. This is what
    /// lets PulseAudio and a bare `mpg123 -o alsa` play at once.
    #[test]
    fn a_mixing_device_serves_every_open_with_a_stream_of_its_own() {
        let audio = Arc::new(FakeAudio::mixing(8 * BYTES_PER_FRAME as usize));
        let pcm = Arc::new(PcmDev::new(audio.clone(), 0));
        let first_open = pcm.open_client().unwrap();
        let second_open = pcm
            .open_client()
            .expect("a second open of a mixing device is a second stream");
        let first = first_open.downcast_ref::<PcmDev>().unwrap();
        let second = second_open.downcast_ref::<PcmDev>().unwrap();
        assert!(
            !Arc::ptr_eq(&first.st, &second.st),
            "each open has runtime state of its own"
        );
        let samples = [0u8; 4 * BYTES_PER_FRAME as usize];
        first.audio.write(&samples).unwrap();
        assert_eq!(first.audio.queued_bytes(), samples.len());
        assert_eq!(
            second.audio.queued_bytes(),
            0,
            "one client's PCM is not in the other's queue"
        );
        assert_eq!(
            audio.queued_bytes(),
            0,
            "nor in the device's own (the registry node's) queue"
        );
        // The claim is never taken on a mixing device, so `/dev/dsp`
        // sharing it is served too.
        let dsp = Arc::new(DspDev::with_claim(audio.clone(), 0, pcm.opened.clone()));
        let dsp_client = dsp
            .open_client()
            .expect("/dev/dsp is a third stream, not EBUSY");
        assert!(
            !pcm.opened.load(Ordering::Acquire),
            "a stream of its own, not the shared claim"
        );
        drop(dsp_client);
        // Closing a client still DROPs its own stream (and nobody
        // else's), claim or no claim.
        let first_stream = first.audio.clone();
        drop(first_open);
        assert_eq!(first_stream.queued_bytes(), 0, "close drops the stream");
        assert!(pcm.open_client().is_ok());
    }

    /// A node built with its own claim (no sharing) stays exclusive
    /// against itself: two `cat > /dev/dsp` at once still interleave.
    #[test]
    fn oss_open_is_exclusive_until_the_client_drops() {
        let audio = Arc::new(FakeAudio::new(4 * BYTES_PER_FRAME as usize));
        let dsp = Arc::new(DspDev::new(audio, 0));
        let first = dsp.open_client().unwrap();
        assert!(matches!(dsp.open_client(), Err(FsError::Busy)));
        drop(first);
        assert!(dsp.open_client().is_ok());
    }

    #[test]
    fn closing_the_pcm_releases_the_hardware_not_just_the_flag() {
        // Releasing only the open flag left the stream running with a full
        // ring, and `/dev/dsp` — a second front end onto that same ring —
        // then reported no free space forever, so `mpg123 -o oss` blocked
        // without playing a sample. Closing a PCM drops it, as on Linux.
        let audio = Arc::new(FakeAudio::new(8 * BYTES_PER_FRAME as usize));
        let pcm = Arc::new(PcmDev::new(audio.clone(), 0));
        let client = pcm.open_client().unwrap();
        let samples = [0u8; 4 * BYTES_PER_FRAME as usize];
        audio.write(&samples).unwrap();
        assert!(audio.queued_bytes() > 0, "the ring should hold the write");
        drop(client);
        assert_eq!(
            audio.queued_bytes(),
            0,
            "closing the PCM must discard what was queued, or the next \
             client finds a device with no room"
        );
        assert!(!audio.is_playing(), "closing the PCM must stop the stream");
    }

    /// pcm_hw.c's SYNC_PTR fallback: `query_status_and_control_data`
    /// (APPL|AVAIL_MIN) after a write must READ the kernel's advanced
    /// appl_ptr, `issue_applptr` (AVAIL_MIN) before START must WRITE the
    /// client's, and `issue_avail_min` (APPL) must write avail_min while
    /// leaving appl_ptr alone -- Linux's flag semantics.
    #[test]
    fn sync_ptr_flags_follow_linux() {
        let audio = Arc::new(FakeAudio::new(64 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio, 0);
        {
            let mut st = pcm.st.lock();
            st.state = STATE_PREPARED;
            st.buffer_size = 16;
            st.avail_min = 1;
        }
        let samples = [0u8; 8 * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 8,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(xfer.result, 8);

        let mut sp: SndPcmSyncPtr = unsafe { core::mem::zeroed() };
        // APPL|AVAIL_MIN: kernel -> user for both; the stale zeros in
        // the client's copy must not reach the kernel.
        sp.flags = SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN;
        pcm.sync_ptr(&mut sp);
        assert_eq!(sp.control.appl_ptr, 8);
        assert_eq!(sp.control.avail_min, 1);
        assert_eq!(sp.status.hw_ptr, 0, "8 written, 8 queued: hw_ptr sits at 0");
        assert_eq!(pcm.st.lock().appl_ptr, 8);

        // APPL alone (issue_avail_min): avail_min user -> kernel,
        // appl_ptr kernel -> user.
        sp.flags = SYNC_PTR_APPL;
        sp.control.avail_min = 4;
        sp.control.appl_ptr = 1234;
        pcm.sync_ptr(&mut sp);
        assert_eq!(pcm.st.lock().avail_min, 4);
        assert_eq!(pcm.st.lock().appl_ptr, 8);
        assert_eq!(sp.control.appl_ptr, 8);

        // AVAIL_MIN alone (issue_applptr): appl_ptr user -> kernel.
        sp.flags = SYNC_PTR_AVAIL_MIN;
        sp.control.appl_ptr = 8;
        sp.control.avail_min = 0;
        pcm.sync_ptr(&mut sp);
        assert_eq!(pcm.st.lock().appl_ptr, 8);
        assert_eq!(sp.control.avail_min, 4);
    }

    /// A stream in XRUN is reported to `poll()`, not hidden from it.
    ///
    /// The ring that stopped draining is full, so the ordinary "is there
    /// room" gate says no and a client that answered EAGAIN by going back
    /// to poll() waits there instead of collecting the EPIPE that tells it
    /// to re-prepare. Linux raises POLLERR with POLLOUT.
    #[test]
    fn poll_reports_an_underrun_instead_of_blocking_on_it() {
        let audio = Arc::new(FakeAudio::new(BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio, 0);
        {
            let mut st = pcm.st.lock();
            st.state = STATE_RUNNING;
            st.buffer_size = 1;
            st.avail_min = 1;
            st.period_size = 1;
        }
        let samples = [0u8; BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 1,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        // Full and not draining: the poller must see "no room" and no error.
        let full = pcm.poll().unwrap();
        assert!(!full.write && !full.error);
        // Once the stall has been declared an underrun, it must.
        pcm.st.lock().state = STATE_XRUN;
        let xrun = pcm.poll().unwrap();
        assert!(xrun.error, "POLLERR");
        assert!(xrun.write, "POLLOUT, so a client waiting to write wakes");
    }

    /// DRAIN must not wait on a stream whose DMA is stopped: nothing is
    /// going to consume the ring, and the caller's close(2) would block
    /// for the whole timeout. Linux only waits while the substream runs.
    #[test]
    fn drain_does_not_wait_on_a_paused_stream() {
        let audio = Arc::new(FakeAudio::new(8 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio.clone(), 0);
        pcm.st.lock().state = STATE_PREPARED;
        let samples = [0u8; 4 * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 4,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(audio.queued_bytes(), 4 * BYTES_PER_FRAME as usize);
        pcm.st.lock().state = STATE_PAUSED;

        let started = kernel_hal::timer::timer_now();
        pcm.drain().unwrap();
        let waited = kernel_hal::timer::timer_now().saturating_sub(started);
        assert!(
            waited < core::time::Duration::from_secs(1),
            "drain spun on a paused stream for {:?}",
            waited
        );
        assert_eq!(pcm.st.lock().state, STATE_SETUP);
        assert_eq!(audio.queued_bytes(), 0, "the ring is wiped either way");
    }

    /// `SNDRV_PCM_IOCTL_XRUN` is the client forcing its own underrun.
    /// Answering Ok without moving the state told it something had
    /// happened that had not.
    #[test]
    fn the_xrun_ioctl_moves_the_stream_into_xrun() {
        let audio = Arc::new(FakeAudio::new(8 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio.clone(), 0);
        pcm.st.lock().state = STATE_PREPARED;
        let samples = [0u8; 4 * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 4,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);

        pcm.io_control(0x4148, 0).unwrap();
        assert_eq!(pcm.st.lock().state, STATE_XRUN);
        assert_eq!(
            audio.queued_bytes(),
            0,
            "the stream is stopped, as on Linux"
        );

        // Not from an arbitrary state: Linux answers EBADFD.
        pcm.st.lock().state = STATE_SETUP;
        assert!(matches!(pcm.io_control(0x4148, 0), Err(FsError::BadState)));
    }

    /// FORWARD skips playhead content that was already written: `appl_ptr`
    /// stays put (those frames already count). FakeAudio drops them from
    /// the queue; HostStream would silence them in place — either way the
    /// application pointer must not advance a second time.
    #[test]
    fn forward_skips_queued_frames_without_moving_appl_ptr() {
        let audio = Arc::new(FakeAudio::new(8 * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio.clone(), 0);
        pcm.st.lock().state = STATE_PREPARED;
        let samples = [0u8; 8 * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 8,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(pcm.st.lock().appl_ptr, 8);
        assert_eq!(audio.queued_bytes(), 8 * BYTES_PER_FRAME as usize);

        let mut frames: u64 = 3;
        pcm.io_control(0x4149, &mut frames as *mut u64 as usize)
            .unwrap();
        assert_eq!(frames, 3, "three frames skipped");
        assert_eq!(
            pcm.st.lock().appl_ptr,
            8,
            "already-written frames stay counted once"
        );
        assert_eq!(audio.queued_bytes(), 5 * BYTES_PER_FRAME as usize);

        let mut back: u64 = 3;
        pcm.io_control(0x4146, &mut back as *mut u64 as usize)
            .unwrap();
        assert_eq!(back, 3);
        assert_eq!(
            pcm.st.lock().appl_ptr,
            5,
            "REWIND drops the tail and appl with it"
        );
    }
}

#[cfg(test)]
mod sw_params_tests {
    use super::pcm_tests_support::*;
    use super::*;

    /// alsa-lib's default: start_threshold 1, so the first frame written
    /// to a PREPARED stream starts it -- through the hold PREPARE set.
    #[test]
    fn prepare_holds_and_the_first_write_starts_with_the_default_threshold() {
        let (audio, pcm) = pcm(64);
        pcm.io_control(0x4140, 0).unwrap(); // PREPARE
        assert!(audio.held(), "PREPARE arms the driver's start hold");
        assert_eq!(pcm.st.lock().state, STATE_PREPARED);
        write(&pcm, 1);
        assert!(!audio.held());
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
    }

    /// aplay sets start_threshold to a period: writes below it queue
    /// without starting, the one that reaches it starts.
    #[test]
    fn a_start_threshold_is_waited_for() {
        let (audio, pcm) = pcm(64);
        pcm.io_control(0x4140, 0).unwrap();
        set_sw(&pcm, |p| p.start_threshold = 8);
        write(&pcm, 4);
        assert_eq!(pcm.st.lock().state, STATE_PREPARED);
        assert!(audio.held());
        assert_eq!(audio.queued_bytes(), 4 * BYTES_PER_FRAME as usize);
        // hw_ptr stays put while the engine is held.
        let mut status: SndPcmStatus = unsafe { core::mem::zeroed() };
        pcm.fill_status(&mut status);
        assert_eq!(
            (status.state, status.hw_ptr, status.appl_ptr),
            (STATE_PREPARED, 0, 4)
        );
        write(&pcm, 4);
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
        assert!(!audio.held());
    }

    /// PulseAudio's pattern: start_threshold at the boundary, fill,
    /// then an explicit START.
    #[test]
    fn a_boundary_start_threshold_waits_for_start() {
        let (audio, pcm) = pcm(32);
        pcm.io_control(0x4140, 0).unwrap();
        let boundary = pcm.st.lock().boundary;
        set_sw(&pcm, |p| p.start_threshold = boundary);
        // START with nothing queued is EPIPE, as in snd_pcm_pre_start...
        assert!(matches!(pcm.io_control(0x4142, 0), Err(FsError::Broken)));
        // ...unless the stream is free-running (stop_threshold at the
        // boundary, PulseAudio's setting), when it starts empty.
        set_sw(&pcm, |p| p.stop_threshold = boundary);
        pcm.io_control(0x4142, 0).unwrap();
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
        assert!(!audio.held());
        pcm.io_control(0x4143, 0).unwrap(); // DROP
        pcm.io_control(0x4140, 0).unwrap(); // PREPARE
        set_sw(&pcm, |p| p.stop_threshold = 32);
        write(&pcm, 16);
        write(&pcm, 16);
        assert_eq!(pcm.st.lock().state, STATE_PREPARED);
        assert!(audio.held());
        // A full buffer is EAGAIN, never an XRUN, while it waits to start.
        let samples = [0u8; BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 1,
        };
        assert!(matches!(
            pcm.writei(&mut xfer, OpenFlags::NON_BLOCK),
            Err(FsError::Again)
        ));
        assert!(pcm.st.lock().stalled_since.is_none());
        pcm.io_control(0x4142, 0).unwrap();
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
        assert!(!audio.held());
        assert!(pcm.st.lock().trigger_tstamp.sec >= 0);
        // START is from PREPARED only.
        assert!(matches!(pcm.io_control(0x4142, 0), Err(FsError::BadState)));
    }

    /// The ring running dry on a RUNNING stream is an underrun: EPIPE on
    /// write, POLLERR to a poller, and PREPARE recovers. With the stop
    /// threshold at the boundary it is not, and the stream free-runs.
    #[test]
    fn the_ring_running_dry_is_an_xrun_unless_the_stop_threshold_says_otherwise() {
        let (audio, pcm) = pcm(64);
        pcm.io_control(0x4140, 0).unwrap();
        write(&pcm, 8);
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
        audio.drain(8);
        let polled = pcm.poll().unwrap();
        assert!(polled.error && polled.write);
        assert_eq!(pcm.st.lock().state, STATE_XRUN);
        let samples = [0u8; BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames: 1,
        };
        assert!(matches!(
            pcm.writei(&mut xfer, OpenFlags::NON_BLOCK),
            Err(FsError::Broken)
        ));
        let mut delay: i64 = 0;
        assert!(matches!(
            pcm.io_control(0x4121, &mut delay as *mut i64 as usize),
            Err(FsError::Broken)
        ));
        pcm.io_control(0x4140, 0).unwrap();
        assert_eq!(pcm.st.lock().state, STATE_PREPARED);
        write(&pcm, 8);
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);

        // Free-running: the boundary as stop threshold.
        let boundary = pcm.st.lock().boundary;
        set_sw(&pcm, |p| p.stop_threshold = boundary);
        audio.drain(8);
        assert!(!pcm.poll().unwrap().error);
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
        write(&pcm, 1);
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
    }

    #[test]
    fn sw_params_validate_like_linux_and_hand_the_boundary_back() {
        let (_, pcm) = pcm(64);
        pcm.io_control(0x4140, 0).unwrap();
        let mut p: SndPcmSwParams = unsafe { core::mem::zeroed() };
        p.avail_min = 0;
        p.boundary = 12345;
        assert!(matches!(
            pcm.io_control(0x4113, &mut p as *mut SndPcmSwParams as usize),
            Err(FsError::InvalidParam)
        ));
        p.avail_min = 4;
        p.stop_threshold = 0;
        assert!(matches!(
            pcm.io_control(0x4113, &mut p as *mut SndPcmSwParams as usize),
            Err(FsError::InvalidParam)
        ));
        p.stop_threshold = 16;
        p.tstamp_mode = 2;
        assert!(matches!(
            pcm.io_control(0x4113, &mut p as *mut SndPcmSwParams as usize),
            Err(FsError::InvalidParam)
        ));
        p.tstamp_mode = 1;
        p.silence_threshold = 1 << 40;
        assert!(matches!(
            pcm.io_control(0x4113, &mut p as *mut SndPcmSwParams as usize),
            Err(FsError::InvalidParam)
        ));
        p.silence_threshold = 0;
        pcm.io_control(0x4113, &mut p as *mut SndPcmSwParams as usize)
            .unwrap();
        let st = pcm.st.lock();
        assert_eq!(p.boundary, st.boundary, "the boundary is the kernel's");
        assert_eq!(
            (st.avail_min, st.stop_threshold, st.tstamp_mode),
            (4, 16, 1)
        );
    }

    #[test]
    fn hw_params_hw_free_and_pause_refuse_the_wrong_states() {
        let (audio, pcm) = pcm(64);
        pcm.io_control(0x4140, 0).unwrap();
        write(&pcm, 8);
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
        // A live ring is not renegotiated or freed under the client.
        let mut hp: SndPcmHwParams = unsafe { core::mem::zeroed() };
        assert!(matches!(
            pcm.io_control(0x4111, &mut hp as *mut SndPcmHwParams as usize),
            Err(FsError::BadState)
        ));
        assert!(matches!(pcm.io_control(0x4112, 0), Err(FsError::BadState)));
        assert_eq!(audio.queued_bytes(), 8 * BYTES_PER_FRAME as usize);
        // PAUSE pauses RUNNING and resumes PAUSED, nothing else.
        let mut on: i32 = 1;
        let mut off: i32 = 0;
        assert!(matches!(
            pcm.io_control(0x4145, &mut off as *mut i32 as usize),
            Err(FsError::BadState)
        ));
        pcm.io_control(0x4145, &mut on as *mut i32 as usize)
            .unwrap();
        assert_eq!(pcm.st.lock().state, STATE_PAUSED);
        assert!(matches!(
            pcm.io_control(0x4145, &mut on as *mut i32 as usize),
            Err(FsError::BadState)
        ));
        pcm.io_control(0x4145, &mut off as *mut i32 as usize)
            .unwrap();
        assert_eq!(pcm.st.lock().state, STATE_RUNNING);
        // DROP then HW_FREE is the way down.
        pcm.io_control(0x4143, 0).unwrap();
        pcm.io_control(0x4112, 0).unwrap();
        assert_eq!(pcm.st.lock().state, STATE_OPEN);
        assert!(matches!(pcm.io_control(0x4140, 0), Err(FsError::BadState)));
    }

    #[test]
    fn prepare_on_a_running_stream_is_ebusy() {
        let (_, pcm) = pcm(64);
        pcm.io_control(0x4140, 0).unwrap();
        write(&pcm, 8);
        assert!(matches!(pcm.io_control(0x4140, 0), Err(FsError::Busy)));
    }

    #[test]
    fn closing_a_primed_pcm_releases_the_hold() {
        let audio = Arc::new(FakeAudio::new(64 * BYTES_PER_FRAME as usize));
        let pcm = Arc::new(PcmDev::new(audio.clone(), 0));
        pcm.st.lock().state = STATE_SETUP; // hw_params done
        let client = pcm.open_client().unwrap();
        client.io_control(0x4140, 0).unwrap();
        assert!(audio.held());
        drop(client);
        assert!(!audio.held());
    }
}

/// Shared scaffolding for the sw_params tests: a PREPARED-ready PCM with
/// `frames` of ring, a whole-frames write, and a sw_params edit.
#[cfg(test)]
mod pcm_tests_support {
    pub(super) use super::pcm_tests::FakeAudio;
    use super::*;

    pub(super) fn pcm(frames: usize) -> (Arc<FakeAudio>, PcmDev) {
        let audio = Arc::new(FakeAudio::new(frames * BYTES_PER_FRAME as usize));
        let pcm = PcmDev::new(audio.clone(), 0);
        {
            let mut st = pcm.st.lock();
            st.state = STATE_SETUP;
            st.buffer_size = frames as u64;
            st.stop_threshold = frames as u64;
            st.period_size = 4;
            st.avail_min = 4;
        }
        (audio, pcm)
    }

    pub(super) fn write(pcm: &PcmDev, frames: u64) {
        let samples = alloc::vec![0u8; frames as usize * BYTES_PER_FRAME as usize];
        let mut xfer = SndXferI {
            result: 0,
            buf: samples.as_ptr() as usize as u64,
            frames,
        };
        pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(xfer.result as u64, frames);
    }

    pub(super) fn set_sw(pcm: &PcmDev, edit: impl FnOnce(&mut SndPcmSwParams)) {
        let mut p: SndPcmSwParams = unsafe { core::mem::zeroed() };
        {
            let st = pcm.st.lock();
            p.avail_min = st.avail_min;
            p.start_threshold = st.start_threshold;
            p.stop_threshold = st.stop_threshold;
            p.period_step = 1;
            p.xfer_align = 1;
        }
        edit(&mut p);
        pcm.io_control(0x4113, &mut p as *mut SndPcmSwParams as usize)
            .unwrap();
    }
}

fn clock(running: bool, periods: u64) -> PeriodClock {
    PeriodClock {
        running,
        periods,
        resolution_ns: 25_000_000,
    }
}

/// The exact sequence pcm_hw.c `snd_pcm_hw_change_timer` issues.
fn alsa_lib_client() -> TimerState {
    let mut st = TimerState::new();
    st.tread = true;
    st.select(0);
    st.resolution_ns = 25_000_000;
    let p = SndTimerParams {
        flags: TIMER_PSFLG_AUTO,
        ticks: 1,
        queue_size: 0,
        reserved0: 0,
        filter: (1 << TIMER_EVENT_TICK) | (1 << 17) | (1 << 18),
        reserved: [0; 60],
    };
    st.params(&p, ts(0)).unwrap();
    st.start(TIMER_EVENT_START, ts(0)).unwrap();
    st
}

fn read_tread(st: &mut TimerState) -> Vec<(i32, u32)> {
    let mut buf = [0u8; 4 * TIMER_TREAD_BYTES];
    let Some(n) = st.read(&mut buf) else {
        return Vec::new();
    };
    assert_eq!(n % TIMER_TREAD_BYTES, 0);
    buf[..n]
        .chunks(TIMER_TREAD_BYTES)
        .map(|r| {
            (
                i32::from_ne_bytes(r[0..4].try_into().unwrap()),
                u32::from_ne_bytes(r[24..28].try_into().unwrap()),
            )
        })
        .collect()
}

#[test]
fn ticks_once_per_elapsed_period_and_coalesces_unread_ticks() {
    let mut st = alsa_lib_client();
    // START itself is filtered out by alsa-lib's TICK|MSUSPEND|MRESUME.
    assert!(st.queue.is_empty());
    // Stream not running yet: nothing.
    st.sample(clock(false, 0), ts(1));
    assert!(st.read(&mut [0u8; 64]).is_none());
    // Stream starts at pointer 0: the first sample only synchronises.
    st.sample(clock(true, 0), ts(2));
    assert!(st.queue.is_empty());
    st.sample(clock(true, 1), ts(3));
    assert_eq!(read_tread(&mut st), alloc::vec![(TIMER_EVENT_TICK, 1)]);
    // Drained: EAGAIN for alsa-lib's clear_timer_queue.
    assert!(st.read(&mut [0u8; 64]).is_none());
    // Two samples nobody read in between become one record with the sum.
    st.sample(clock(true, 3), ts(4));
    st.sample(clock(true, 4), ts(5));
    assert_eq!(read_tread(&mut st), alloc::vec![(TIMER_EVENT_TICK, 3)]);
    // Still running (AUTO).
    assert!(st.running);
}

#[test]
fn a_pointer_reset_or_a_stopped_stream_resynchronises_without_ticking_backwards() {
    let mut st = alsa_lib_client();
    st.sample(clock(true, 5), ts(1));
    st.sample(clock(true, 7), ts(2));
    assert_eq!(read_tread(&mut st), alloc::vec![(TIMER_EVENT_TICK, 2)]);
    // PREPARE reset the pointer: no tick, new baseline.
    st.sample(clock(true, 0), ts(3));
    assert!(st.queue.is_empty());
    st.sample(clock(true, 1), ts(4));
    assert_eq!(read_tread(&mut st), alloc::vec![(TIMER_EVENT_TICK, 1)]);
    // The stream stops and later restarts at 0: same rule.
    st.sample(clock(false, 1), ts(5));
    st.sample(clock(true, 0), ts(6));
    assert!(st.queue.is_empty());
    st.sample(clock(true, 2), ts(7));
    assert_eq!(read_tread(&mut st), alloc::vec![(TIMER_EVENT_TICK, 2)]);
}

#[test]
fn lifecycle_events_follow_the_filter_and_start_stop_answer_ebusy() {
    let mut st = TimerState::new();
    st.tread = true;
    st.select(0);
    st.resolution_ns = 25_000_000;
    let p = SndTimerParams {
        flags: TIMER_PSFLG_AUTO,
        ticks: 2,
        queue_size: 64,
        reserved0: 0,
        filter: (1 << TIMER_EVENT_TICK) | (1 << TIMER_EVENT_START),
        reserved: [0; 60],
    };
    st.params(&p, ts(0)).unwrap();
    assert!(matches!(
        st.stop(TIMER_EVENT_STOP, ts(0)),
        Err(FsError::Busy)
    ));
    st.start(TIMER_EVENT_START, ts(1)).unwrap();
    assert!(matches!(
        st.start(TIMER_EVENT_START, ts(1)),
        Err(FsError::Busy)
    ));
    // START is in the filter: one record carrying the resolution.
    assert_eq!(
        read_tread(&mut st),
        alloc::vec![(TIMER_EVENT_START, 25_000_000)]
    );
    // ticks=2: one period is not yet a tick, the second is.
    st.sample(clock(true, 0), ts(2));
    st.sample(clock(true, 1), ts(3));
    assert!(st.queue.is_empty());
    st.sample(clock(true, 2), ts(4));
    assert_eq!(read_tread(&mut st), alloc::vec![(TIMER_EVENT_TICK, 1)]);
    // STOP is not in the filter: no record, but the run ends.
    st.stop(TIMER_EVENT_STOP, ts(5)).unwrap();
    assert!(st.queue.is_empty());
    assert!(!st.running);
    st.sample(clock(true, 9), ts(6));
    assert!(st.queue.is_empty());
}

#[test]
fn one_shot_without_auto_ticks_once_then_stops() {
    let mut st = TimerState::new();
    st.tread = true;
    st.select(0);
    let p = SndTimerParams {
        flags: 0,
        ticks: 1,
        queue_size: 0,
        reserved0: 0,
        filter: 1 << TIMER_EVENT_TICK,
        reserved: [0; 60],
    };
    st.params(&p, ts(0)).unwrap();
    st.start(TIMER_EVENT_START, ts(0)).unwrap();
    st.sample(clock(true, 0), ts(1));
    st.sample(clock(true, 1), ts(2));
    assert!(!st.running);
    st.sample(clock(true, 5), ts(3));
    assert_eq!(read_tread(&mut st), alloc::vec![(TIMER_EVENT_TICK, 1)]);
}

#[test]
fn plain_reads_are_eight_byte_records_and_only_ticks() {
    let mut st = TimerState::new();
    st.select(0);
    let p = SndTimerParams {
        flags: TIMER_PSFLG_AUTO,
        ticks: 1,
        queue_size: 0,
        reserved0: 0,
        filter: 0xffff_ffff,
        reserved: [0; 60],
    };
    st.params(&p, ts(0)).unwrap();
    st.start(TIMER_EVENT_START, ts(0)).unwrap();
    // Not a TREAD client: START produced nothing.
    assert!(st.queue.is_empty());
    st.sample(clock(true, 0), ts(1));
    st.sample(clock(true, 3), ts(2));
    let mut buf = [0u8; 64];
    assert_eq!(st.read(&mut buf), Some(TIMER_READ_BYTES));
    assert_eq!(
        u32::from_ne_bytes(buf[0..4].try_into().unwrap()),
        25_000_000
    );
    assert_eq!(u32::from_ne_bytes(buf[4..8].try_into().unwrap()), 3);
    // A buffer too small for one record reads nothing.
    st.sample(clock(true, 4), ts(3));
    assert!(st.read(&mut buf[..4]).is_none());
    assert_eq!(st.read(&mut buf), Some(TIMER_READ_BYTES));
}

#[test]
fn params_validates_like_linux_and_a_full_queue_counts_overruns() {
    let mut st = TimerState::new();
    st.tread = true;
    assert!(matches!(
        st.params(
            &SndTimerParams {
                flags: 0,
                ticks: 1,
                queue_size: 0,
                reserved0: 0,
                filter: 0,
                reserved: [0; 60]
            },
            ts(0)
        ),
        Err(FsError::NoDevice)
    ));
    st.select(0);
    assert!(matches!(
        st.params(
            &SndTimerParams {
                flags: 0,
                ticks: 0,
                queue_size: 0,
                reserved0: 0,
                filter: 0,
                reserved: [0; 60]
            },
            ts(0)
        ),
        Err(FsError::InvalidParam)
    ));
    assert!(matches!(
        st.params(
            &SndTimerParams {
                flags: 0,
                ticks: 1,
                queue_size: 8,
                reserved0: 0,
                filter: 0,
                reserved: [0; 60]
            },
            ts(0)
        ),
        Err(FsError::InvalidParam)
    ));
    let p = SndTimerParams {
        flags: TIMER_PSFLG_AUTO,
        ticks: 1,
        queue_size: 32,
        reserved0: 0,
        filter: (1 << TIMER_EVENT_TICK) | (1 << TIMER_EVENT_CONTINUE) | (1 << TIMER_EVENT_PAUSE),
        reserved: [0; 60],
    };
    st.params(&p, ts(0)).unwrap();
    st.start(TIMER_EVENT_START, ts(0)).unwrap();
    // Alternating tick / pause / continue never coalesces: fill the queue.
    st.sample(clock(true, 0), ts(0));
    for i in 1..=40u64 {
        st.sample(clock(true, i), ts(i as i64));
        st.stop(TIMER_EVENT_PAUSE, ts(i as i64)).unwrap();
        st.start(TIMER_EVENT_CONTINUE, ts(i as i64)).unwrap();
        st.sample(clock(true, i), ts(i as i64));
    }
    assert_eq!(st.queue.len(), 32);
    assert!(st.overrun > 0);
}
