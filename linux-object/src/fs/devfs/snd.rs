//! `/dev/snd/*` — native ALSA kernel ABI over an [`AudioScheme`] device.
//!
//! This is what stock alsa-lib (`aplay`, SDL, mpg123, anything using
//! `snd_pcm_*`) talks to: one `controlC<card>` + `pcmC<card>D0p` pair per HDA
//! controller. The implementation targets alsa-lib's `hw` plugin in its
//! **RW-interleaved + SYNC_PTR** mode:
//!
//! * `SNDRV_PCM_IOCTL_HW_REFINE/HW_PARAMS` constrain to what the driver ring
//!   does natively — S16LE, stereo, the discrete HDA rate set. `/etc/asound.conf`
//!   routes ALSA `default` through PulseAudio (`type pulse`); the kernel node
//!   stays `hw:0,0` / `eclipse_hw` for the Pulse sink. `plug` remains available
//!   as `aplay -D plug`.
//! * Data moves through `SNDRV_PCM_IOCTL_WRITEI_FRAMES`; the status/control
//!   pages are *not* mmap-able here, which alsa-lib detects and transparently
//!   falls back to `SNDRV_PCM_IOCTL_SYNC_PTR` — implemented with Linux's exact
//!   flag semantics. PulseAudio's ALSA sink is started with `mmap=0` for the
//!   same reason.
//! * Mixer: `Master Playback Volume` (INTEGER 0..=100, stereo) and
//!   `Master Playback Switch` (BOOLEAN mute). HDMI has no analog volume, so
//!   the HDA driver scales S16LE in `write`; `amixer set Master 50%` and the
//!   lunarbar slider both land on that path (or on `pactl` when Pulse is up).
//! * No capture, no mmap data access: the `info` flags and refine masks never
//!   advertise them. `PAUSE` stops DMA (ring kept for resume). `REWIND` /
//!   `FORWARD` drop queued tail/head and silence that range so PulseAudio
//!   cannot leave remnants of the last mpg123 track looping in the HDA ring.
//! * Stream state follows `pcm_native.c`: `PREPARE` arms the driver's start
//!   hold and the stream runs once `start_threshold` frames are queued (or
//!   on `START`, which needs data and PREPARED); the ring running dry on a
//!   RUNNING stream is an XRUN (`stop_threshold`, `EPIPE`, `POLLERR`) unless
//!   the threshold sits at the boundary; `HW_PARAMS`, `HW_FREE`, `PREPARE`,
//!   `START`, `DROP` and `PAUSE` refuse the states Linux refuses with
//!   `EBADFD`; `SW_PARAMS` validates every field and hands the boundary back.
//!
//! Ioctls are matched on the `_IOC` type+nr bytes only (not the size bits):
//! the struct layouts here mirror the x86_64 uapi, but matching the full cmd
//! word would break the moment alsa-lib is built against a slightly newer
//! header that grew a reserved field.

#![allow(unsafe_code)]

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use kernel_hal::drivers::scheme::AudioScheme;
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

use crate::fs::OpenFlags;

fn ucheck<T>(addr: usize) -> Result<()> {
    if kernel_hal::user::user_range_ok(addr, core::mem::size_of::<T>()) {
        Ok(())
    } else {
        Err(FsError::BadAddress)
    }
}

/// Periodic LPIB poll while HDA DMA is in RUN. PulseAudio corks at end of
/// mpg123 and stops calling write/poll; without this the cyclic ring keeps
/// fetching the last fragment (~0.5 s) forever.
static AUDIO_WATCHDOG_ARMED: AtomicBool = AtomicBool::new(false);

pub(crate) fn arm_playback_watchdog() {
    if AUDIO_WATCHDOG_ARMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let deadline = kernel_hal::timer::timer_now() + Duration::from_millis(4);
    kernel_hal::timer::timer_set(deadline, Box::new(audio_watchdog_tick));
}

fn audio_watchdog_tick(_now: Duration) {
    AUDIO_WATCHDOG_ARMED.store(false, Ordering::Release);
    let mut playing = false;
    {
        let devices = kernel_hal::drivers::all_audio().as_vec();
        for dev in devices.iter() {
            let _ = dev.queued_bytes();
            if dev.is_playing() {
                playing = true;
            }
        }
    }
    if playing {
        arm_playback_watchdog();
    }
}

// ── uapi mirror (x86_64) ────────────────────────────────────────────────────

// 2.0.4: new hw_params struct (>=2.0.2) without USER_PVERSION (2.0.14),
// STATUS_EXT (2.0.13) or monotonic-raw (2.0.12). Reporting 2.0.14 made
// alsa-lib take those paths; mpg123 then died in snd_pcm_hw_params with
// "cannot set hw params" after every set_*_near had succeeded.
const SNDRV_PCM_VERSION: i32 = 0x0002_0004;
const SNDRV_CTL_VERSION: i32 = 0x0002_0007;

// snd_pcm_state_t
const STATE_OPEN: i32 = 0;
const STATE_SETUP: i32 = 1;
const STATE_PREPARED: i32 = 2;
const STATE_RUNNING: i32 = 3;
/// `SNDRV_PCM_STATE_XRUN`. Reached when the ring stops draining for longer
/// than a whole buffer: the device is not consuming, so no amount of polling
/// will make room. Linux reports that as an underrun, `writei` answers EPIPE,
/// and every client recovers by re-preparing the stream. Without it a stalled
/// device produces EAGAIN forever, which is not a recovery path — it is how
/// PulseAudio ended up aborting inside libasound.
const STATE_XRUN: i32 = 4;
/// Shortest stall that counts as an underrun, however small the negotiated
/// buffer. A 64-frame buffer at 48 kHz is 1.3 ms; declaring XRUN that fast
/// would turn ordinary scheduling jitter into a stream reset.
const BUFFER_STALL_FLOOR: core::time::Duration = core::time::Duration::from_millis(200);
/// How long a blocking `writei` waits before re-offering PCM to a full ring.
/// The same backoff `/dev/dsp` uses; see the loop in [`PcmDev::writei`].
const WRITE_RETRY_BACKOFF: core::time::Duration = core::time::Duration::from_micros(250);
const STATE_DRAINING: i32 = 5;
const STATE_PAUSED: i32 = 6;
const TSTAMP_MODE_LAST: i32 = 1;
const TSTAMP_TYPE_LAST: u32 = 3;

// snd_pcm_hw_param indexes.
const PAR_ACCESS: usize = 0;
const PAR_FORMAT: usize = 1;
const PAR_SUBFORMAT: usize = 2;
// Interval params, biased by FIRST_INTERVAL = 8 when indexing `intervals`.
const PAR_SAMPLE_BITS: usize = 8;
const PAR_FRAME_BITS: usize = 9;
const PAR_CHANNELS: usize = 10;
const PAR_RATE: usize = 11;
const PAR_PERIOD_TIME: usize = 12;
const PAR_PERIOD_SIZE: usize = 13;
const PAR_PERIOD_BYTES: usize = 14;
const PAR_PERIODS: usize = 15;
const PAR_BUFFER_TIME: usize = 16;
const PAR_BUFFER_SIZE: usize = 17;
const PAR_BUFFER_BYTES: usize = 18;
const PAR_TICK_TIME: usize = 19;

const ACCESS_RW_INTERLEAVED: u32 = 3;
const FORMAT_S16_LE: u32 = 2;
const SUBFORMAT_STD: u32 = 0;

const INFO_PAUSE: u32 = 0x0000_0020;
const INFO_INTERLEAVED: u32 = 0x0000_0100;
const INFO_BLOCK_TRANSFER: u32 = 0x0001_0000;

const SYNC_PTR_HWSYNC: u32 = 1;
const SYNC_PTR_APPL: u32 = 2;
const SYNC_PTR_AVAIL_MIN: u32 = 4;

const INTERVAL_OPENMIN: u32 = 1 << 0;
const INTERVAL_OPENMAX: u32 = 1 << 1;
const INTERVAL_INTEGER: u32 = 1 << 2;
const INTERVAL_EMPTY: u32 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SndInterval {
    min: u32,
    max: u32,
    flags: u32, // openmin | openmax<<1 | integer<<2 | empty<<3
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SndMask {
    bits: [u32; 8],
}

#[repr(C)]
struct SndPcmHwParams {
    flags: u32,
    masks: [SndMask; 3],
    mres: [SndMask; 5],
    intervals: [SndInterval; 12],
    ires: [SndInterval; 9],
    rmask: u32,
    cmask: u32,
    info: u32,
    msbits: u32,
    rate_num: u32,
    rate_den: u32,
    fifo_size: u64,
    reserved: [u8; 64],
}

const _: () = assert!(core::mem::size_of::<SndPcmHwParams>() == 608);

#[repr(C)]
struct SndPcmSwParams {
    tstamp_mode: i32,
    period_step: u32,
    sleep_min: u32,
    avail_min: u64,
    xfer_align: u64,
    start_threshold: u64,
    stop_threshold: u64,
    silence_threshold: u64,
    silence_size: u64,
    boundary: u64,
    proto: u32,
    tstamp_type: u32,
    reserved: [u8; 56],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

#[repr(C)]
struct SndPcmStatus {
    state: i32,
    _pad0: i32,
    trigger_tstamp: Timespec,
    tstamp: Timespec,
    appl_ptr: u64,
    hw_ptr: u64,
    delay: i64,
    avail: u64,
    avail_max: u64,
    overrange: u64,
    suspended_state: i32,
    audio_tstamp_data: u32,
    audio_tstamp: Timespec,
    driver_tstamp: Timespec,
    audio_tstamp_accuracy: u32,
    reserved: [u8; 20],
}

#[repr(C)]
struct SndPcmMmapStatus {
    state: i32,
    _pad1: i32,
    hw_ptr: u64,
    tstamp: Timespec,
    suspended_state: i32,
    _pad2: i32,
    audio_tstamp: Timespec,
}

#[repr(C)]
struct SndPcmMmapControl {
    appl_ptr: u64,
    avail_min: u64,
}

#[repr(C)]
struct SndPcmSyncPtr {
    flags: u32,
    _pad: u32,
    status: SndPcmMmapStatus, // union with u8[64]
    _spad: [u8; 64 - core::mem::size_of::<SndPcmMmapStatus>()],
    control: SndPcmMmapControl, // union with u8[64]
    _cpad: [u8; 64 - core::mem::size_of::<SndPcmMmapControl>()],
}

#[repr(C)]
struct SndXferI {
    result: i64,
    buf: u64,
    frames: u64,
}

/// `struct snd_pcm_channel_info` (x86_64).
#[repr(C)]
struct SndPcmChannelInfo {
    channel: u32,
    _pad: u32,
    offset: i64,
    first: u32,
    step: u32,
}

#[repr(C)]
struct SndPcmInfo {
    device: u32,
    subdevice: u32,
    stream: i32,
    card: i32,
    id: [u8; 64],
    name: [u8; 80],
    subname: [u8; 32],
    dev_class: i32,
    dev_subclass: i32,
    subdevices_count: u32,
    subdevices_avail: u32,
    sync: [u8; 16],
    reserved: [u8; 64],
}

#[repr(C)]
struct SndCtlCardInfo {
    card: i32,
    _pad: i32,
    id: [u8; 16],
    driver: [u8; 16],
    name: [u8; 32],
    longname: [u8; 80],
    reserved_: [u8; 16],
    mixername: [u8; 80],
    components: [u8; 128],
}

// ── Mixer uapi (x86_64) ─────────────────────────────────────────────────────
// Layouts match include/uapi/sound/asound.h. alsa-lib's simple mixer composes
// "Master" from Playback Volume + Playback Switch with these exact names.

const IFACE_MIXER: i32 = 2;
const ELEM_BOOLEAN: i32 = 1;
const ELEM_INTEGER: i32 = 2;
const ACCESS_READ_WRITE: u32 = 0x1 | 0x2;

const NUMID_VOLUME: u32 = 1;
const NUMID_SWITCH: u32 = 2;
const NAME_VOLUME: &str = "Master Playback Volume";
const NAME_SWITCH: &str = "Master Playback Switch";
const MIXER_ELEMS: u32 = 2;

#[repr(C)]
struct SndCtlElemId {
    numid: u32,
    iface: i32,
    device: u32,
    subdevice: u32,
    name: [u8; 44],
    index: u32,
}

#[repr(C)]
struct SndCtlElemList {
    offset: u32,
    space: u32,
    used: u32,
    count: u32,
    pids: u64,
    _reserved: [u8; 50],
}

#[repr(C)]
struct SndCtlElemInfo {
    id: SndCtlElemId,
    type_: i32,
    access: u32,
    count: u32,
    owner: i32,
    min: i64,
    max: i64,
    step: i64,
    _value_pad: [u8; 128 - 24],
    _dimen_reserved: [u8; 64],
}

#[repr(C)]
struct SndCtlElemValue {
    id: SndCtlElemId,
    _indirect: u32,
    _pad: u32,
    values: [i64; 2],
}

#[derive(Clone, Copy)]
enum MixerElem {
    Volume,
    Switch,
}

fn cstr_name(buf: &[u8; 44]) -> &str {
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

fn fill_elem_id(id: &mut SndCtlElemId, elem: MixerElem) {
    unsafe {
        core::ptr::write_bytes(
            id as *mut SndCtlElemId as *mut u8,
            0,
            core::mem::size_of::<SndCtlElemId>(),
        )
    };
    id.iface = IFACE_MIXER;
    match elem {
        MixerElem::Volume => {
            id.numid = NUMID_VOLUME;
            fill_cstr(&mut id.name, NAME_VOLUME);
        }
        MixerElem::Switch => {
            id.numid = NUMID_SWITCH;
            fill_cstr(&mut id.name, NAME_SWITCH);
        }
    }
}

fn elem_from_id(id: &SndCtlElemId) -> Option<MixerElem> {
    match id.numid {
        NUMID_VOLUME => return Some(MixerElem::Volume),
        NUMID_SWITCH => return Some(MixerElem::Switch),
        _ => {}
    }
    match cstr_name(&id.name) {
        NAME_VOLUME => Some(MixerElem::Volume),
        NAME_SWITCH => Some(MixerElem::Switch),
        _ => None,
    }
}

fn mixer_elem_at(index: u32) -> Option<MixerElem> {
    match index {
        0 => Some(MixerElem::Volume),
        1 => Some(MixerElem::Switch),
        _ => None,
    }
}

const _: () = assert!(core::mem::size_of::<SndCtlElemId>() == 64);
const _: () = assert!(core::mem::size_of::<SndCtlElemInfo>() == 272);

fn fill_cstr(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    dst[n..].fill(0);
}

/// The discrete rates the HDA driver encodes (see `stream_format`).
const RATES: [u32; 11] = [
    8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
];

const BYTES_PER_FRAME: u64 = 4; // S16LE stereo

// ── PCM device node ─────────────────────────────────────────────────────────

struct PcmState {
    state: i32,
    rate: u32,
    buffer_size: u64, // frames
    period_size: u64, // frames
    boundary: u64,
    appl_ptr: u64,
    avail_min: u64,
    /// Frames queued ahead of `hw_ptr` at which a PREPARED stream starts on
    /// its own (Linux `start_threshold`; alsa-lib's default is 1, PulseAudio
    /// sets the boundary so only an explicit START runs it). Until it is
    /// reached the driver holds the engine (`AudioScheme::set_start_hold`).
    start_threshold: u64,
    /// `avail` at which a RUNNING stream is an underrun (Linux
    /// `stop_threshold`; the buffer size by default, the boundary to never
    /// underrun -- a free-running stream that plays silence when starved).
    stop_threshold: u64,
    /// The rest of `snd_pcm_sw_params`, kept so the ioctl hands back what
    /// the client set. The ring's own silence-ahead zone stands in for
    /// silence_threshold/silence_size.
    silence_threshold: u64,
    silence_size: u64,
    tstamp_mode: i32,
    tstamp_type: u32,
    period_step: u32,
    sleep_min: u32,
    xfer_align: u64,
    proto: u32,
    /// When the stream last went RUNNING (`snd_pcm_status.trigger_tstamp`).
    trigger_tstamp: Timespec,
    /// When the ring first refused a write for want of room, or `None` while
    /// it is still draining. See [`STATE_XRUN`].
    stalled_since: Option<core::time::Duration>,
}

/// The single-client claim on one audio device, shared by every front end
/// onto it: the native ALSA PCM at `/dev/snd/pcmC<card>D0p` and the OSS node
/// at `/dev/dsp<card>`. It is only used on a device that has one ring and
/// no mixer (`open_stream` answers `None`): there both front ends drive
/// the SAME hardware ring, two writers would interleave into each other's
/// frames, and the second opener has to be refused (`EBUSY`), as Linux
/// refuses a second open of one substream. A device that mixes hands each
/// open a stream of its own instead, and the claim is never taken.
///
/// On an unmixed device that refusal is what makes clients recover on
/// their own: `mpg123` with no `-o` walks libout123's built-in driver list
/// and takes the first module that both loads AND opens, OSS included, so
/// an `EBUSY` from `/dev/dsp` moves it on to the next one (ALSA -> the
/// pulse plugin -> the daemon that mixes).
pub type AudioClaim = Arc<AtomicBool>;

/// An unheld claim, for one audio device.
pub fn new_audio_claim() -> AudioClaim {
    Arc::new(AtomicBool::new(false))
}

pub struct PcmDev {
    audio: Arc<dyn AudioScheme>,
    card: usize,
    inode_id: usize,
    st: Arc<Mutex<PcmState>>,
    opened: AudioClaim,
    /// A client handle (from [`open_client`](PcmDev::open_client)): drops
    /// the stream when it goes. The registry node that `new` built is not
    /// one and owns nothing.
    client: bool,
    /// This handle took [`opened`](PcmDev::opened) and releases it on drop:
    /// a client of an unmixed device. A client with a stream of its own
    /// never took it.
    release_opened_on_drop: bool,
}

impl PcmDev {
    /// A PCM node with a claim of its own: exclusive against other opens of
    /// itself, but not against `/dev/dsp<card>`. Tests and any single-front-end
    /// setup use this; `/dev/snd` proper is built with [`PcmDev::with_claim`].
    pub fn new(audio: Arc<dyn AudioScheme>, card: usize) -> Self {
        Self::with_claim(audio, card, new_audio_claim())
    }

    /// A PCM node sharing `opened` with the other front ends onto the same
    /// device (see [`AudioClaim`]).
    pub fn with_claim(audio: Arc<dyn AudioScheme>, card: usize, opened: AudioClaim) -> Self {
        PcmDev {
            audio,
            card,
            inode_id: DevFS::new_inode_id(),
            st: Self::fresh_state(),
            opened,
            client: false,
            release_opened_on_drop: false,
        }
    }

    /// The runtime state of a PCM that has just been opened.
    fn fresh_state() -> Arc<Mutex<PcmState>> {
        Arc::new(Mutex::new(PcmState {
            state: STATE_OPEN,
            rate: 48000,
            buffer_size: 16384,
            period_size: 1024,
            boundary: 0x4000_0000_0000_0000,
            appl_ptr: 0,
            avail_min: 1024,
            start_threshold: 1,
            stop_threshold: 16384,
            silence_threshold: 0,
            silence_size: 0,
            tstamp_mode: 0,
            tstamp_type: 0,
            period_step: 1,
            sleep_min: 0,
            xfer_align: 1,
            proto: 0,
            trigger_tstamp: Timespec { sec: 0, nsec: 0 },
            stalled_since: None,
        }))
    }

    /// `open(2)` on `hw:card,0`.
    ///
    /// On a device that mixes, every open is a client of its own: a stream
    /// from [`AudioScheme::open_stream`] with its own rate, buffer, pointer
    /// and state, mixed in the kernel with every other open (PulseAudio's
    /// and a bare `mpg123 -o alsa` at once). The runtime state is fresh
    /// per open, so nothing one client sets reaches another. The timer
    /// node (`/dev/snd/timer`) still samples the registry node's state,
    /// which no client-of-a-stream drives; a `dmix` on top of a device
    /// that already mixes has nothing to add anyway.
    ///
    /// On a device with one ring and no mixer, the one process that owns
    /// it keeps the shared runtime state and timer view until close, and
    /// everyone else gets `EBUSY` as on Linux — `/dev/dsp<card>`, which
    /// shares the claim, included.
    pub fn open_client(&self) -> Result<Arc<dyn INode>> {
        if let Some(stream) = self.audio.open_stream().map_err(|_| FsError::DeviceError)? {
            return Ok(Arc::new(PcmDev {
                audio: stream,
                card: self.card,
                inode_id: self.inode_id,
                st: Self::fresh_state(),
                opened: self.opened.clone(),
                client: true,
                release_opened_on_drop: false,
            }));
        }
        if self
            .opened
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(FsError::Busy);
        }
        Ok(Arc::new(PcmDev {
            audio: self.audio.clone(),
            card: self.card,
            inode_id: self.inode_id,
            st: self.st.clone(),
            opened: self.opened.clone(),
            client: true,
            release_opened_on_drop: true,
        }))
    }

    /// The ring's capacity in frames for a client at `rate`, BEFORE that
    /// rate is applied. This is the bound the refine and `install` put on
    /// `buffer_size`: on a device that resamples into a fixed-rate ring the
    /// capacity in client frames depends on the rate, and bounding a
    /// 44.1 kHz request with the previous 48 kHz stream's figure granted
    /// PulseAudio a buffer 999 frames deeper than the ring. `avail`
    /// (`buffer_size - queued`) then stayed positive with the ring full,
    /// `write` took nothing, and alsa-sink's `try_recover` asserted on the
    /// EAGAIN that followed its own `snd_pcm_avail()`.
    fn ring_frames_at(&self, rate: u32) -> u64 {
        self.audio.buffer_bytes_at(rate) as u64 / BYTES_PER_FRAME
    }

    fn queued_frames(&self) -> u64 {
        self.audio.queued_bytes() as u64 / BYTES_PER_FRAME
    }

    /// Frames still queued as THIS stream's ring sees them: never more than
    /// its negotiated buffer. `queued_bytes` is per device, so another writer
    /// on the same card (a second PCM open, `/dev/dsp`) can push it past this
    /// stream's `buffer_size`. Reporting that put `hw_ptr` behind
    /// `appl_ptr - buffer_size`, and alsa-lib's avail (`hw_ptr + buffer -
    /// appl_ptr`, mod boundary) wrapped to ~boundary: PulseAudio then wrote
    /// into room the kernel did not have, got EAGAIN right after
    /// `snd_pcm_avail()`, and `try_recover()` asserted (`err != -11`),
    /// aborting the daemon -- seen on real hardware with audio-probe and the
    /// daemon's own sink both on hw:0,0.
    fn queued_capped(&self, st: &PcmState) -> u64 {
        self.queued_frames().min(st.buffer_size)
    }

    /// Frames the client may write within its negotiated buffer.
    fn avail(&self, st: &PcmState) -> u64 {
        st.buffer_size.saturating_sub(self.queued_capped(st))
    }

    fn hw_ptr(&self, st: &PcmState) -> u64 {
        (st.appl_ptr + st.boundary - self.queued_capped(st)) % st.boundary
    }

    /// Linux's underrun rule (`snd_pcm_update_state`): a RUNNING playback
    /// stream whose `avail` has reached `stop_threshold` is in XRUN. With the
    /// default threshold, the buffer size, that is the ring running dry: the
    /// HDA engine then stops itself and loops silence, so the next write
    /// would restart it seamlessly -- which is not what a client that set a
    /// stop threshold asked for. It expects `EPIPE` and `POLLERR`, and every
    /// ALSA client recovers from that with `snd_pcm_prepare()`, so the
    /// stream state has to say what happened. A threshold at or past the
    /// boundary keeps the seamless behaviour (Linux's free-running mode).
    fn note_underrun(&self, st: &mut PcmState) {
        if st.state != STATE_RUNNING || st.stop_threshold >= st.boundary {
            return;
        }
        if self.avail(st) >= st.stop_threshold {
            st.state = STATE_XRUN;
            st.stalled_since = None;
            info!(
                "[snd] pcmC{}D0p: ring ran dry (avail {} >= stop_threshold {}) -> XRUN",
                self.card,
                self.avail(st),
                st.stop_threshold
            );
        }
    }

    /// Move a PREPARED stream to RUNNING: release the driver's start hold so
    /// the primed ring plays, and stamp the trigger time.
    fn start_running(&self, st: &mut PcmState) {
        let _ = self.audio.set_start_hold(false);
        st.state = STATE_RUNNING;
        st.stalled_since = None;
        let now = kernel_hal::timer::timer_now();
        st.trigger_tstamp = Timespec {
            sec: now.as_secs() as i64,
            nsec: now.subsec_nanos() as i64,
        };
        arm_playback_watchdog();
    }

    /// What the ALSA timer bound to this stream samples: whether the stream
    /// runs, how many whole periods the hardware pointer has passed, and the
    /// period length in ns (the timer's resolution).
    pub(crate) fn period_clock(&self) -> PeriodClock {
        // Lock order everywhere in this file is `st` then the audio driver
        // (`hw_ptr` -> `queued_frames` refreshes the hardware position);
        // nothing takes them the other way round.
        let mut st = self.st.lock();
        self.note_underrun(&mut st);
        let period = st.period_size.max(1);
        PeriodClock {
            running: st.state == STATE_RUNNING,
            periods: self.hw_ptr(&st) / period,
            resolution_ns: period.saturating_mul(1_000_000_000) / u64::from(st.rate.max(1)),
        }
    }

    // ── hw_params refine ────────────────────────────────────────────────────
    //
    // alsa-lib does NOT hand the kernel a finished configuration: it negotiates
    // one parameter at a time (`snd_pcm_hw_params_choose` walks access, format,
    // …, period_time, period_size, period_bytes, periods, buffer_time,
    // buffer_size, buffer_bytes), calling HW_REFINE after every single
    // narrowing and expecting the kernel to derive every *dependent* parameter.
    // speaker-test, for one, only ever sets period_time/buffer_time and lets
    // the sizes fall out of them.
    //
    // So refining each parameter against a fixed range in isolation is not
    // enough — it leaves the sizes wide open while the times are pinned, and
    // the combination alsa-lib then picks is internally inconsistent, which the
    // final refine rejects (the EINVAL from `snd_pcm_hw_params`). Linux solves
    // this with constraint rules (`snd_pcm_hw_rule_*`) re-run to a fixed point;
    // this is the same idea with the relations our fixed S16LE-stereo path has:
    //
    //   frame_bits  = sample_bits × channels                     (= 32)
    //   period_bytes = period_size × frame_bits / 8              (= ps × 4)
    //   buffer_bytes = buffer_size × frame_bits / 8              (= bs × 4)
    //   buffer_size  = period_size × periods
    //   period_time  = period_size × 1e6 / rate                  (µs)
    //   buffer_time  = buffer_size × 1e6 / rate                  (µs)
    //
    // Each relation is propagated in BOTH directions until nothing changes.

    /// Interval index inside `SndPcmHwParams::intervals` (biased by 8, the
    /// first interval parameter id).
    const IV_SAMPLE_BITS: usize = PAR_SAMPLE_BITS - 8;
    const IV_FRAME_BITS: usize = PAR_FRAME_BITS - 8;
    const IV_CHANNELS: usize = PAR_CHANNELS - 8;
    const IV_RATE: usize = PAR_RATE - 8;
    const IV_PERIOD_TIME: usize = PAR_PERIOD_TIME - 8;
    const IV_PERIOD_SIZE: usize = PAR_PERIOD_SIZE - 8;
    const IV_PERIOD_BYTES: usize = PAR_PERIOD_BYTES - 8;
    const IV_PERIODS: usize = PAR_PERIODS - 8;
    const IV_BUFFER_TIME: usize = PAR_BUFFER_TIME - 8;
    const IV_BUFFER_SIZE: usize = PAR_BUFFER_SIZE - 8;
    const IV_BUFFER_BYTES: usize = PAR_BUFFER_BYTES - 8;
    const IV_TICK_TIME: usize = PAR_TICK_TIME - 8;

    /// Every parameter here is integer-valued, so open bounds are folded into
    /// closed ones up front — the same normalization Linux's
    /// `snd_interval_refine` does for an INTEGER interval.
    fn iv_normalize(iv: &mut SndInterval) {
        if iv.flags & INTERVAL_OPENMIN != 0 {
            iv.min = iv.min.saturating_add(1);
            iv.flags &= !INTERVAL_OPENMIN;
        }
        if iv.flags & INTERVAL_OPENMAX != 0 {
            iv.max = iv.max.saturating_sub(1);
            iv.flags &= !INTERVAL_OPENMAX;
        }
        iv.flags |= INTERVAL_INTEGER;
        if iv.min > iv.max {
            iv.flags |= INTERVAL_EMPTY;
        }
    }

    fn iv_empty(iv: &SndInterval) -> bool {
        iv.flags & INTERVAL_EMPTY != 0 || iv.min > iv.max
    }

    /// Intersect `iv` with `[lo, hi]`. Returns whether anything changed, so the
    /// propagation loop knows when it has reached a fixed point.
    fn iv_clamp(iv: &mut SndInterval, lo: u64, hi: u64) -> bool {
        let lo = lo.min(u32::MAX as u64) as u32;
        let hi = hi.min(u32::MAX as u64) as u32;
        let mut changed = false;
        if iv.min < lo {
            iv.min = lo;
            changed = true;
        }
        if iv.max > hi {
            iv.max = hi;
            changed = true;
        }
        if iv.min > iv.max {
            iv.flags |= INTERVAL_EMPTY;
        }
        changed
    }

    fn iv_bounds(iv: &SndInterval) -> (u64, u64) {
        (iv.min as u64, iv.max as u64)
    }

    /// Human name for a parameter index, for the diagnostic log below.
    fn iv_name(idx: usize) -> &'static str {
        match idx {
            Self::IV_SAMPLE_BITS => "sample_bits",
            Self::IV_FRAME_BITS => "frame_bits",
            Self::IV_CHANNELS => "channels",
            Self::IV_RATE => "rate",
            Self::IV_PERIOD_TIME => "period_time",
            Self::IV_PERIOD_SIZE => "period_size",
            Self::IV_PERIOD_BYTES => "period_bytes",
            Self::IV_PERIODS => "periods",
            Self::IV_BUFFER_TIME => "buffer_time",
            Self::IV_BUFFER_SIZE => "buffer_size",
            Self::IV_BUFFER_BYTES => "buffer_bytes",
            Self::IV_TICK_TIME => "tick_time",
            _ => "?",
        }
    }

    /// One propagation sweep. Returns `true` if any interval narrowed.
    fn propagate(&self, iv: &mut [SndInterval; 12]) -> bool {
        let mut changed = false;

        // Fixed points of this path: S16LE stereo.
        changed |= Self::iv_clamp(&mut iv[Self::IV_SAMPLE_BITS], 16, 16);
        changed |= Self::iv_clamp(&mut iv[Self::IV_CHANNELS], 2, 2);
        changed |= Self::iv_clamp(&mut iv[Self::IV_FRAME_BITS], 32, 32);

        // Rate: snap to the discrete set the HDA stream format encodes.
        let rate_lo = {
            let r = &mut iv[Self::IV_RATE];
            let lo = RATES.iter().copied().find(|&x| x >= r.min);
            let hi = RATES.iter().rev().copied().find(|&x| x <= r.max);
            match (lo, hi) {
                (Some(lo), Some(hi)) if lo <= hi => {
                    changed |= Self::iv_clamp(r, lo as u64, hi as u64);
                    lo
                }
                _ => {
                    r.flags |= INTERVAL_EMPTY;
                    r.min
                }
            }
        };
        // The ring's capacity for the rate this request can still end up
        // at. The lowest rate left in the interval, which is also the one
        // `install` picks when the client leaves the rate open: on a
        // fixed-rate ring a lower client rate means fewer client frames, so
        // this is the bound every rate in the interval can honour. A client
        // that pins its rate first (alsa-lib's `set_*_near` helpers do, as
        // do PulseAudio, aplay and SDL) gets that rate's exact capacity.
        let ring = self.ring_frames_at(rate_lo);

        // Hard bounds of the driver ring. The period ceiling is half the ring
        // so at least two periods always fit; without it a long period at a
        // high rate (125 ms at 96 kHz is 12000 frames) is refused outright
        // instead of merely being deep. The floor stays low enough for the
        // low-latency configurations clients legitimately ask for — 5 ms at
        // 48 kHz is 240 frames, so a 256-frame floor would reject it.
        changed |= Self::iv_clamp(&mut iv[Self::IV_PERIOD_SIZE], 128, (ring / 2).max(128));
        changed |= Self::iv_clamp(&mut iv[Self::IV_BUFFER_SIZE], 512, ring);
        changed |= Self::iv_clamp(&mut iv[Self::IV_PERIODS], 1, 1024);
        changed |= Self::iv_clamp(&mut iv[Self::IV_TICK_TIME], 0, u32::MAX as u64);

        // Every derived bound below is deliberately CONSERVATIVE: the lower
        // bound floors, the upper bound ceils. Linux does the same
        // (`snd_interval_div` floors the min and bumps the max on a remainder)
        // and the reason is not cosmetic — a division whose lower bound ceils
        // carves out the legal value whenever the operands don't divide
        // evenly. At 44.1 kHz a 0.5 s buffer is 22050 frames and a 125 ms
        // period is 5512.5: rounding the derived period_size UP to 5513 while
        // the reverse rule rounds it DOWN to 5512 empties the interval and the
        // whole hw_params fails with EINVAL. Exact internal consistency is not
        // the refine's job — `install` picks the coherent triple at the end.

        // period_bytes = period_size × 4  (both directions)
        let (ps_lo, ps_hi) = Self::iv_bounds(&iv[Self::IV_PERIOD_SIZE]);
        changed |= Self::iv_clamp(&mut iv[Self::IV_PERIOD_BYTES], ps_lo * 4, ps_hi * 4);
        let (pb_lo, pb_hi) = Self::iv_bounds(&iv[Self::IV_PERIOD_BYTES]);
        changed |= Self::iv_clamp(&mut iv[Self::IV_PERIOD_SIZE], pb_lo / 4, pb_hi.div_ceil(4));

        // buffer_bytes = buffer_size × 4
        let (bs_lo, bs_hi) = Self::iv_bounds(&iv[Self::IV_BUFFER_SIZE]);
        changed |= Self::iv_clamp(&mut iv[Self::IV_BUFFER_BYTES], bs_lo * 4, bs_hi * 4);
        let (bb_lo, bb_hi) = Self::iv_bounds(&iv[Self::IV_BUFFER_BYTES]);
        changed |= Self::iv_clamp(&mut iv[Self::IV_BUFFER_SIZE], bb_lo / 4, bb_hi.div_ceil(4));

        // buffer_size ≈ period_size × periods.
        //
        // Approximately, not exactly: the DMA ring this sits on is continuous
        // and is not carved into period segments, so a buffer that is not a
        // whole number of periods is perfectly playable here. Demanding the
        // exact multiple during refine would reject ordinary requests — 0.5 s
        // at 44.1 kHz is 22050 frames while four 125 ms periods are 22048 —
        // so the upper bound carries one period of slack and `install` is what
        // settles on the exactly-coherent triple.
        // The exact relation, with the slack written down once so all three
        // directions agree: `periods` is how many WHOLE periods fit in the
        // buffer, i.e.
        //
        //     period_size × periods  ≤  buffer_size  ≤  period_size × (periods+1) − 1
        //
        // Every bound below is that inequality solved for one unknown. They
        // have to be mutual inverses or the propagation contradicts itself:
        // deriving period_size from (buffer 512, periods 2) as 170 while the
        // reverse rule caps a 170-frame 2-period buffer at 509 empties the
        // interval, and hw_params fails — which is exactly what a client
        // asking for two periods used to hit.
        let (ps_lo, ps_hi) = Self::iv_bounds(&iv[Self::IV_PERIOD_SIZE]);
        let (n_lo, n_hi) = Self::iv_bounds(&iv[Self::IV_PERIODS]);
        changed |= Self::iv_clamp(
            &mut iv[Self::IV_BUFFER_SIZE],
            ps_lo * n_lo,
            (ps_hi * (n_hi + 1)).saturating_sub(1),
        );
        let (bs_lo, bs_hi) = Self::iv_bounds(&iv[Self::IV_BUFFER_SIZE]);
        // ps ≥ (buffer+1)/(periods+1)  and  ps ≤ buffer/periods (only once
        // the periods interval has a non-zero lower bound to divide by).
        if let Some(ps_max) = bs_hi.checked_div(n_lo) {
            changed |= Self::iv_clamp(
                &mut iv[Self::IV_PERIOD_SIZE],
                (bs_lo + 1).div_ceil(n_hi + 1),
                ps_max,
            );
        }
        let (ps_lo, ps_hi) = Self::iv_bounds(&iv[Self::IV_PERIOD_SIZE]);
        if ps_lo > 0 && ps_hi > 0 {
            // periods ≥ (buffer+1)/ps − 1  and  periods ≤ buffer/ps
            changed |= Self::iv_clamp(
                &mut iv[Self::IV_PERIODS],
                (bs_lo + 1).div_ceil(ps_hi).saturating_sub(1).max(1),
                bs_hi / ps_lo,
            );
        }

        // period_time = period_size × 1e6 / rate, and the same for the buffer.
        //
        // Time is continuous but sizes are whole frames, so the two directions
        // must describe the SAME relation or they fight each other: one hands
        // out a size, the other then rejects the very time it was derived
        // from. The relation is `size = floor(time × rate / 1e6)`, i.e. a size
        // of N frames owns the half-open time cell [N/rate, (N+1)/rate):
        //
        //   size ← time : [floor(t_lo·r_lo/1e6), floor(t_hi·r_hi/1e6)]
        //   time ← size : [ceil(N_lo·1e6/r_hi), ceil((N_hi+1)·1e6/r_lo) − 1]
        //
        // Getting this asymmetric is what rejected 0.5 s at 11.025 kHz: the
        // size rule rounded 5512.5 up to 5513, and the time rule then said
        // 5513 frames is 500045 µs, which no longer contains the 500000 the
        // client asked for. Linux encodes the same half-open cell with its
        // open-bound flags.
        let (r_lo, r_hi) = Self::iv_bounds(&iv[Self::IV_RATE]);
        let (r_lo, r_hi) = (r_lo.max(1), r_hi.max(1));
        for (size_idx, time_idx) in [
            (Self::IV_PERIOD_SIZE, Self::IV_PERIOD_TIME),
            (Self::IV_BUFFER_SIZE, Self::IV_BUFFER_TIME),
        ] {
            let (s_lo, s_hi) = Self::iv_bounds(&iv[size_idx]);
            changed |= Self::iv_clamp(
                &mut iv[time_idx],
                (s_lo * 1_000_000).div_ceil(r_hi),
                (((s_hi + 1) * 1_000_000).div_ceil(r_lo)).saturating_sub(1),
            );
            let (t_lo, t_hi) = Self::iv_bounds(&iv[time_idx]);
            changed |= Self::iv_clamp(
                &mut iv[size_idx],
                t_lo * r_lo / 1_000_000,
                t_hi * r_hi / 1_000_000,
            );
        }

        changed
    }

    /// Constrain a hw_params request to what the hardware path does, running
    /// the relations above to a fixed point. Returns `false` when a parameter
    /// became empty (→ EINVAL, which alsa-lib's `*_near` searches use to home
    /// in on supported values).
    fn refine(&self, p: &mut SndPcmHwParams) -> bool {
        p.masks[PAR_ACCESS].bits[0] &= 1 << ACCESS_RW_INTERLEAVED;
        p.masks[PAR_FORMAT].bits[0] &= 1 << FORMAT_S16_LE;
        p.masks[PAR_FORMAT].bits[1] = 0;
        p.masks[PAR_SUBFORMAT].bits[0] &= 1 << SUBFORMAT_STD;
        for m in &mut p.masks {
            for w in &mut m.bits[2..] {
                *w = 0;
            }
        }
        let mut ok = p.masks[PAR_ACCESS].bits[0] != 0
            && p.masks[PAR_FORMAT].bits[0] != 0
            && p.masks[PAR_SUBFORMAT].bits[0] != 0;

        for iv in p.intervals.iter_mut() {
            Self::iv_normalize(iv);
        }
        // Fixed point: each sweep can only narrow intervals, so this
        // terminates; the cap is a belt-and-braces bound.
        for _ in 0..8 {
            if !self.propagate(&mut p.intervals) {
                break;
            }
        }
        if p.intervals.iter().any(Self::iv_empty) {
            // Deliberately silent: alsa-lib's `*_near` helpers find a supported
            // value BY probing until the refine rejects one, so a rejection
            // here is the normal case, not a fault. Only HW_PARAMS failing is
            // worth a log line — see `install`.
            ok = false;
        }

        p.info = INFO_PAUSE | INFO_INTERLEAVED | INFO_BLOCK_TRANSFER;
        p.msbits = 16;
        p.rate_den = 1;
        p.rate_num = 0;
        p.fifo_size = 0;
        // Report every parameter as (potentially) changed; alsa-lib re-reads all.
        p.cmask = 0x000f_ff07;
        ok
    }

    /// Drop period/buffer/time constraints so a later refine can pick any
    /// legal ring configuration. Rate, channels and the format masks stay.
    fn reopen(p: &mut SndPcmHwParams, params: &[usize]) {
        for &idx in params {
            p.intervals[idx] = SndInterval {
                min: 0,
                max: u32::MAX,
                flags: 0,
            };
        }
    }

    /// First recovery stage: reopen only the parameters alsa-lib DERIVES,
    /// keeping period_size/buffer_size — the two the client actually chose and
    /// the only ones that set its latency. The inconsistency this recovers
    /// from is a derived one (choose pins a period_time that disagrees with
    /// the frame count by a frame), so throwing the sizes away too would
    /// answer a 10 ms buffer request with the full 683 ms ring.
    fn relax_derived_params(p: &mut SndPcmHwParams) {
        Self::reopen(
            p,
            &[
                Self::IV_PERIOD_TIME,
                Self::IV_PERIOD_BYTES,
                Self::IV_PERIODS,
                Self::IV_BUFFER_TIME,
                Self::IV_BUFFER_BYTES,
                Self::IV_TICK_TIME,
            ],
        );
    }

    /// Second stage: let the sizes go as well. Format, access, channels and
    /// rate are deliberately NOT touched at any stage — a client asking for
    /// mono S24_LE has to get EINVAL, because answering `Ok` and then playing
    /// its bytes as stereo S16LE is worse than failing: it is wrong audio
    /// reported as success.
    fn relax_size_params(p: &mut SndPcmHwParams) {
        Self::relax_derived_params(p);
        Self::reopen(p, &[Self::IV_PERIOD_SIZE, Self::IV_BUFFER_SIZE]);
    }

    /// Log a rejected HW_PARAMS with both the client's request and the state
    /// it refined to. Userspace only sees a bare EINVAL, so without this there
    /// is no way to tell which constraint bit — and the request side matters
    /// as much as the result, because a plugin (`plughw`) rewrites the values
    /// on their way down. Budgeted so a client that retries cannot flood the
    /// console.
    fn log_hw_params_failure(name: &str, req: &[SndInterval; 12], iv: &[SndInterval; 12]) {
        use core::sync::atomic::{AtomicU32, Ordering};
        static BUDGET: AtomicU32 = AtomicU32::new(8);
        if BUDGET
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |b| b.checked_sub(1))
            .is_err()
        {
            return;
        }
        warn!(
            "[snd] hw_params REQUESTED: rate {}..{}, period_size {}..{}, \
             period_time {}..{}, periods {}..{}, buffer_size {}..{}, buffer_time {}..{}",
            req[Self::IV_RATE].min,
            req[Self::IV_RATE].max,
            req[Self::IV_PERIOD_SIZE].min,
            req[Self::IV_PERIOD_SIZE].max,
            req[Self::IV_PERIOD_TIME].min,
            req[Self::IV_PERIOD_TIME].max,
            req[Self::IV_PERIODS].min,
            req[Self::IV_PERIODS].max,
            req[Self::IV_BUFFER_SIZE].min,
            req[Self::IV_BUFFER_SIZE].max,
            req[Self::IV_BUFFER_TIME].min,
            req[Self::IV_BUFFER_TIME].max,
        );
        warn!(
            "[snd] hw_params REJECTED: '{}' empty — rate {}..{}, period_size {}..{}, \
             period_time {}..{}, periods {}..{}, buffer_size {}..{}, buffer_time {}..{}",
            name,
            iv[Self::IV_RATE].min,
            iv[Self::IV_RATE].max,
            iv[Self::IV_PERIOD_SIZE].min,
            iv[Self::IV_PERIOD_SIZE].max,
            iv[Self::IV_PERIOD_TIME].min,
            iv[Self::IV_PERIOD_TIME].max,
            iv[Self::IV_PERIODS].min,
            iv[Self::IV_PERIODS].max,
            iv[Self::IV_BUFFER_SIZE].min,
            iv[Self::IV_BUFFER_SIZE].max,
            iv[Self::IV_BUFFER_TIME].min,
            iv[Self::IV_BUFFER_TIME].max,
        );
    }

    /// Choose concrete values inside the (already refined) request, the way
    /// Linux's `snd_pcm_hw_params` finishes the job: pick each still-open
    /// parameter, re-propagate so the rest follows, and hand every interval
    /// back as an exact singleton.
    fn install(&self, p: &mut SndPcmHwParams) -> Result<()> {
        let requested = p.intervals;
        if !self.refine(p) {
            // alsa-lib's `snd_pcm_hw_params_choose` ignores its own errors
            // (`_snd_pcm_hw_params_internal` in pcm_params.c) and still
            // issues HW_PARAMS with a period/buffer/time triple that our
            // refine just emptied. mpg123 hits this: it pins buffer_size =
            // rate*0.5 and period_size = buffer/4, then choose set_first's
            // period_time and the two disagree by a frame. Recover by
            // keeping format/rate/channels and letting the sizes fall out.
            // Each stage restarts from the ORIGINAL request: the failed
            // refine above already narrowed (and emptied) the intervals, so
            // relaxing what it left behind would be relaxing garbage.
            p.intervals = requested;
            Self::relax_derived_params(p);
            if !self.refine(p) {
                p.intervals = requested;
                Self::relax_size_params(p);
                if !self.refine(p) {
                    let empty = p
                        .intervals
                        .iter()
                        .position(Self::iv_empty)
                        .map(Self::iv_name)
                        .unwrap_or("mask");
                    Self::log_hw_params_failure(empty, &requested, &p.intervals);
                    return Err(FsError::InvalidParam);
                }
            }
        }

        // Rate first: everything time-related depends on it.
        let rate = RATES
            .iter()
            .copied()
            .find(|&r| r >= p.intervals[Self::IV_RATE].min && r <= p.intervals[Self::IV_RATE].max)
            .unwrap_or(48_000);
        Self::iv_clamp(&mut p.intervals[Self::IV_RATE], rate as u64, rate as u64);
        for _ in 0..8 {
            if !self.propagate(&mut p.intervals) {
                break;
            }
        }

        // Then the period, then the buffer — the order alsa-lib itself uses
        // (`snd_pcm_hw_params_choose`: first value for the period, last for the
        // buffer, so a client that left the buffer open gets the deepest one).
        //
        // The triple is made EXACTLY coherent here (buffer = period × periods)
        // instead of by more propagation, because the refine's conservative
        // rounding can leave the client's requested time and the integer sizes
        // an odd frame apart — 0.5 s at 44.1 kHz is 22050 frames, which is not
        // a whole number of 125 ms periods. Deriving the answer and reporting
        // it back is exactly what Linux does; the client reads the granted
        // values (they are what `aplay -v` prints) rather than its request.
        let period = (p.intervals[Self::IV_PERIOD_SIZE].min as u64).max(1);
        // For the rate about to be set, not the one the device is at: the
        // buffer granted here must be one the ring holds once `set_params`
        // below has switched the device to `rate` (see `ring_frames_at`).
        let ring = self.ring_frames_at(rate);
        let bs_lo = p.intervals[Self::IV_BUFFER_SIZE].min as u64;
        let bs_hi = (p.intervals[Self::IV_BUFFER_SIZE].max as u64).min(ring);
        let n_iv = p.intervals[Self::IV_PERIODS];
        let mut periods = if n_iv.min == n_iv.max && n_iv.min > 0 {
            n_iv.min as u64 // the client pinned a period count: honour it
        } else {
            (bs_hi / period).max(1)
        };
        // Fit period × periods inside the buffer bounds the refine settled on.
        // Shrink first, then grow only while the result still fits the upper
        // bound, so the two adjustments cannot oscillate.
        while periods > 1 && period * periods > bs_hi {
            periods -= 1;
        }
        while period * periods < bs_lo && period * (periods + 1) <= bs_hi.min(ring) {
            periods += 1;
        }
        let buffer = (period * periods).min(ring);

        let (actual_rate, _ch) = self
            .audio
            .set_params(rate, 2)
            .map_err(|_| FsError::DeviceError)?;
        if actual_rate != rate {
            warn!(
                "[snd] device took {} Hz for a {} Hz request",
                actual_rate, rate
            );
        }

        {
            let mut st = self.st.lock();
            st.rate = actual_rate;
            st.period_size = period;
            st.buffer_size = buffer;
            st.appl_ptr = 0;
            // Same boundary algorithm as Linux and alsa-lib, so both sides
            // wrap the pointers at the same value.
            let mut boundary = buffer;
            while boundary < 0x4000_0000_0000_0000u64 {
                boundary *= 2;
            }
            st.boundary = boundary;
            st.avail_min = period;
            // Linux resets the software parameters with every hw_params.
            st.start_threshold = 1;
            st.stop_threshold = buffer;
            st.silence_threshold = 0;
            st.silence_size = 0;
            st.state = STATE_SETUP;
            // The ring was just wiped by `set_params`, so any stall that was
            // being timed is over. Leaving the clock running meant a stream
            // that recovered by re-negotiating (HW_FREE + HW_PARAMS rather
            // than PREPARE) was already past the limit on its very first full
            // ring, and the next refusal came back as an XRUN it had not
            // earned.
            st.stalled_since = None;
        }

        // Hand back exact singletons for everything.
        let iv = &mut p.intervals;
        for (idx, v) in [
            (Self::IV_SAMPLE_BITS, 16),
            (Self::IV_FRAME_BITS, 32),
            (Self::IV_CHANNELS, 2),
            (Self::IV_RATE, actual_rate as u64),
            (Self::IV_PERIOD_SIZE, period),
            (Self::IV_PERIOD_BYTES, period * 4),
            (Self::IV_PERIODS, periods),
            (Self::IV_BUFFER_SIZE, buffer),
            (Self::IV_BUFFER_BYTES, buffer * 4),
            (
                Self::IV_PERIOD_TIME,
                period * 1_000_000 / actual_rate as u64,
            ),
            (
                Self::IV_BUFFER_TIME,
                buffer * 1_000_000 / actual_rate as u64,
            ),
        ] {
            let v = v.min(u32::MAX as u64) as u32;
            iv[idx] = SndInterval {
                min: v,
                max: v,
                flags: INTERVAL_INTEGER,
            };
        }
        iv[Self::IV_TICK_TIME] = SndInterval {
            min: 0,
            max: 0,
            flags: INTERVAL_INTEGER,
        };
        p.rate_num = actual_rate;
        p.rate_den = 1;

        info!(
            "[snd] pcmC{}D0p configured: {} Hz, 2ch S16LE, period {} frames, buffer {} frames ({} periods)",
            self.card, actual_rate, period, buffer, periods
        );
        Ok(())
    }

    fn commit_write_progress(&self, bytes: usize) {
        let mut st = self.st.lock();
        st.appl_ptr = (st.appl_ptr + bytes as u64 / BYTES_PER_FRAME) % st.boundary.max(1);
        st.stalled_since = None;
        // A PREPARED stream starts once `start_threshold` frames are queued
        // ahead of the hardware pointer (Linux `snd_pcm_lib_write`); below
        // that the driver keeps holding the engine and the data waits.
        if st.state == STATE_PREPARED && self.queued_capped(&st) >= st.start_threshold {
            self.start_running(&mut st);
        } else {
            arm_playback_watchdog();
        }
    }

    /// Answer a write that found no room: `EAGAIN` while the ring is still
    /// draining, `EPIPE` once it has clearly stopped.
    ///
    /// `EAGAIN` means "not right now, poll and retry", and a client acts on it
    /// by going back to `poll()`. That is the correct answer only if the room
    /// is going to appear. When the device stops consuming — the HDA engine
    /// halted, the DMA position frozen — it never does, and the client spins
    /// against a full ring with nothing to recover from. PulseAudio's ALSA
    /// sink ends that spin by aborting inside libasound:
    ///
    /// ```text
    /// [alsa-hunt] pid=1029 ioctl PCM_WRITEI_FRAMES -> EAGAIN (11)
    /// [exit] pid=1029 (pulseaudio) killed by signal SIGABRT (6)
    /// ```
    ///
    /// Linux calls a ring that stopped draining an underrun: the state goes to
    /// `XRUN`, `writei` answers `EPIPE`, and the client re-prepares the stream.
    /// That is a recovery path; an endless `EAGAIN` is not. A whole buffer's
    /// worth of time with zero progress is well past any scheduling hiccup —
    /// one period would be the normal refill cadence — so that is the line.
    fn no_room(&self, total_bytes: usize) -> Result<()> {
        let now = kernel_hal::timer::timer_now();
        let mut st = self.st.lock();
        if st.state == STATE_PREPARED {
            // A primed stream that has not started (start_threshold not
            // reached, or PulseAudio's explicit START still to come) is not
            // stalled: nothing is supposed to be draining it yet.
            return Err(FsError::Again);
        }
        let since = *st.stalled_since.get_or_insert(now);
        // buffer_size frames at `rate` Hz, floored so a bogus rate cannot make
        // the timeout infinite.
        let buffer_us = st.buffer_size.saturating_mul(1_000_000) / st.rate.max(1) as u64;
        let limit = core::time::Duration::from_micros(buffer_us).max(BUFFER_STALL_FLOOR);
        if now.saturating_sub(since) < limit {
            return Err(FsError::Again);
        }
        st.state = STATE_XRUN;
        st.stalled_since = None;
        let queued = self.audio.queued_bytes();
        let free = self.audio.free_bytes();
        let playing = self.audio.is_playing();
        drop(st);
        error!(
            "[snd] pcmC{}D0p: ring stopped draining for {:?} with {} bytes queued, \
             {} free, is_playing={} — reporting XRUN/EPIPE so the client can \
             re-prepare instead of spinning on EAGAIN (wanted {} bytes)",
            self.card, limit, queued, free, playing, total_bytes,
        );
        Err(FsError::Broken)
    }

    /// Interleaved write. Blocking callers keep the old spin-retry behaviour;
    /// nonblocking callers get Linux-style "write what fits right now, else
    /// EAGAIN" semantics so alsa-lib/PulseAudio can return to poll().
    fn writei(&self, xfer: &mut SndXferI, flags: OpenFlags) -> Result<()> {
        {
            let mut st = self.st.lock();
            self.note_underrun(&mut st);
            // Linux (`__snd_pcm_lib_xfer`) accepts PREPARED, RUNNING *and*
            // PAUSED, and answers every other state with EBADFD -- never
            // EINVAL. Both halves of that matter to a client:
            //
            //  * PAUSED was rejected here, so a client that pauses and keeps
            //    filling the buffer (what PulseAudio's ALSA sink does while a
            //    stream is corked) got an error where Linux takes the data.
            //  * EINVAL means "bad arguments" and alsa-lib treats it as a
            //    caller bug rather than a stream-state problem, which is how a
            //    plain state mismatch reached PulseAudio as a fatal error:
            //      [einval-hunt] pid=1028 syscall=16 (IOCTL) a1=0x40184150 -> EINVAL
            //    (0x40184150 = _IOW('A', 0x50, 24) = WRITEI_FRAMES), and then
            //    an abort inside libasound.
            // An underrun is not "this fd is unusable": Linux answers EPIPE and
            // every client recovers with `snd_pcm_prepare()`. Reporting EBADFD
            // here would tell PulseAudio the stream is dead when it is merely
            // stalled.
            if st.state == STATE_XRUN {
                return Err(FsError::Broken);
            }
            if st.state != STATE_PREPARED && st.state != STATE_RUNNING && st.state != STATE_PAUSED {
                // Name the state: the errno alone cannot say whether the
                // stream was never prepared (SETUP), already torn down (OPEN)
                // or something else.
                warn!(
                    "[snd] pcmC{}D0p: writei in state {} (need prepared/running/paused) -> EBADFD",
                    self.card, st.state
                );
                return Err(FsError::BadState);
            }
        }
        let total_bytes = (xfer.frames * BYTES_PER_FRAME) as usize;
        let src = xfer.buf as *const u8;
        if flags.non_block() {
            let st_buffer = self.st.lock().buffer_size;
            let queued = self.queued_frames();
            let room_frames = st_buffer.saturating_sub(queued);
            let frame = BYTES_PER_FRAME as usize;
            let mut chunk = ((room_frames * BYTES_PER_FRAME) as usize).min(total_bytes);
            if chunk < frame {
                // A nonblocking client only writes after its avail said there
                // was room; another writer on the same device may have taken
                // it since. Let the ring's slack beyond this stream's buffer
                // absorb the write instead of answering EAGAIN, which
                // PulseAudio's try_recover() treats as fatal right after
                // snd_pcm_avail(). EAGAIN only when the ring itself is full.
                chunk = (self.audio.free_bytes() / frame * frame).min(total_bytes);
            }
            if chunk < frame {
                return self.no_room(total_bytes);
            }
            let buf = unsafe { core::slice::from_raw_parts(src, chunk) };
            let n = self.audio.write(buf).map_err(|_| FsError::DeviceError)?;
            if n == 0 {
                return self.no_room(total_bytes);
            }
            self.commit_write_progress(n);
            xfer.result = (n as u64 / BYTES_PER_FRAME) as i64;
            return Ok(());
        }
        let mut done = 0usize;
        let deadline_step = core::time::Duration::from_secs(4);
        let mut deadline = kernel_hal::timer::timer_now() + deadline_step;
        while done < total_bytes {
            // Respect the negotiated buffer size: never queue past it.
            let st_buffer = self.st.lock().buffer_size;
            let queued = self.queued_frames();
            let room_frames = st_buffer.saturating_sub(queued);
            let room = (room_frames * BYTES_PER_FRAME) as usize;
            let chunk = room.min(total_bytes - done);
            let n = if chunk >= BYTES_PER_FRAME as usize {
                let buf = unsafe { core::slice::from_raw_parts(src.add(done), chunk) };
                self.audio.write(buf).map_err(|_| FsError::DeviceError)?
            } else {
                0
            };
            if n > 0 {
                done += n;
                self.commit_write_progress(n);
                deadline = kernel_hal::timer::timer_now() + deadline_step;
                continue;
            }
            if kernel_hal::timer::timer_now() >= deadline {
                warn!("[snd] pcmC{}D0p: writei stalled", self.card);
                break;
            }
            kernel_hal::deferred_job::drain_deferred_jobs();
            // Back off before re-offering, exactly as the OSS node does. A
            // bare spin re-entered `queued_frames` as fast as the CPU could
            // go, and every one of those takes the driver's lock -- which on
            // this kernel means interrupts off -- to look at a ring that
            // frees space at 192 KB/s. The device-read side of it is already
            // throttled inside the driver; this throttles the lock traffic
            // that surrounds it. The ring holds 341 ms, so a quarter of a
            // millisecond of granularity costs the refill nothing.
            let resume = kernel_hal::timer::timer_now() + WRITE_RETRY_BACKOFF;
            while kernel_hal::timer::timer_now() < resume {
                core::hint::spin_loop();
            }
        }
        xfer.result = (done as u64 / BYTES_PER_FRAME) as i64;
        Ok(())
    }

    fn drain(&self) -> Result<()> {
        let (rate, state) = {
            let mut st = self.st.lock();
            // Linux starts a PREPARED stream that holds data before draining
            // it, whatever its start_threshold.
            if st.state == STATE_PREPARED && self.queued_frames() > 0 {
                self.start_running(&mut st);
            }
            (st.rate.max(1) as u64, st.state)
        };
        // Only a stream whose DMA is actually running can drain. Waiting on a
        // paused one -- PulseAudio corks a sink and then drains it on the way
        // out -- burns the whole timeout in a spin loop with nothing on the
        // other end, and the caller's close(2) blocks for seconds. Linux
        // treats DRAIN on a stopped stream as immediately complete
        // (`snd_pcm_drain` only waits while the substream is RUNNING).
        let drainable = state != STATE_PAUSED && self.audio.is_playing();
        let deadline = kernel_hal::timer::timer_now()
            + core::time::Duration::from_secs(
                (self.audio.queued_bytes() as u64)
                    .div_ceil(rate * BYTES_PER_FRAME)
                    .saturating_add(2)
                    .min(10),
            );
        while drainable && self.audio.queued_bytes() > 0 {
            if kernel_hal::timer::timer_now() >= deadline {
                break;
            }
            // The engine stopping mid-drain (the ring ran dry, or it was
            // reset under us) means the rest is never going to play out.
            if !self.audio.is_playing() {
                break;
            }
            kernel_hal::deferred_job::drain_deferred_jobs();
            core::hint::spin_loop();
        }
        // Drain done (or timed out): wipe the ring so leftover PCM cannot
        // loop. PREPARE/DROP also reset; this covers PulseAudio's end-of-stream
        // path that only DRAIN'd.
        let _ = self.audio.reset();
        let mut st = self.st.lock();
        st.state = STATE_SETUP;
        st.stalled_since = None;
        Ok(())
    }

    /// SYNC_PTR with Linux flag semantics (snd_pcm_sync_ptr in
    /// sound/core/pcm_native.c): a SET flag means "do not take my copy, hand
    /// me yours" (kernel -> user), a CLEAR flag pushes the user's copy into
    /// the kernel. alsa-lib's SYNC_PTR fallback (no mmap of the
    /// status/control pages) relies on this: after every WRITEI it sends
    /// APPL|AVAIL_MIN to learn the advanced appl_ptr, and before START it
    /// sends AVAIL_MIN alone to commit its own appl_ptr. With the flags read
    /// the other way round the kernel's appl_ptr was rewound to the client's
    /// stale copy after each write, and hw_ptr (appl_ptr - queued) wrapped to
    /// just under the boundary.
    fn sync_ptr(&self, sp: &mut SndPcmSyncPtr) {
        let mut st = self.st.lock();
        self.note_underrun(&mut st);
        if sp.flags & SYNC_PTR_APPL != 0 {
            sp.control.appl_ptr = st.appl_ptr;
        } else {
            st.appl_ptr = sp.control.appl_ptr % st.boundary.max(1);
        }
        if sp.flags & SYNC_PTR_AVAIL_MIN != 0 {
            sp.control.avail_min = st.avail_min;
        } else if sp.control.avail_min > 0 {
            st.avail_min = sp.control.avail_min;
        }
        let _ = sp.flags & SYNC_PTR_HWSYNC; // hw state is always live
        sp.status.state = st.state;
        sp.status.hw_ptr = self.hw_ptr(&st);
        sp.status.suspended_state = st.state;
        let now = kernel_hal::timer::timer_now();
        sp.status.tstamp = Timespec {
            sec: now.as_secs() as i64,
            nsec: now.subsec_nanos() as i64,
        };
    }

    fn fill_status(&self, s: &mut SndPcmStatus) {
        unsafe {
            core::ptr::write_bytes(
                s as *mut SndPcmStatus as *mut u8,
                0,
                core::mem::size_of::<SndPcmStatus>(),
            )
        };
        let mut st = self.st.lock();
        self.note_underrun(&mut st);
        s.state = st.state;
        s.trigger_tstamp = st.trigger_tstamp;
        s.appl_ptr = st.appl_ptr;
        s.hw_ptr = self.hw_ptr(&st);
        s.delay = self.queued_frames() as i64;
        s.avail = self.avail(&st);
        s.avail_max = st.buffer_size;
        let now = kernel_hal::timer::timer_now();
        s.tstamp = Timespec {
            sec: now.as_secs() as i64,
            nsec: now.subsec_nanos() as i64,
        };
    }

    pub(crate) fn io_control_with_flags(
        &self,
        cmd: u32,
        data: usize,
        flags: OpenFlags,
    ) -> Result<usize> {
        self.io_control_impl(cmd, data, flags)
    }

    fn io_control_impl(&self, cmd: u32, data: usize, flags: OpenFlags) -> Result<usize> {
        let ty = (cmd >> 8) & 0xff;
        let nr = cmd & 0xff;
        if ty != b'A' as u32 {
            return Err(FsError::NotSupported);
        }
        match nr {
            0x00 => {
                // PVERSION
                ucheck::<i32>(data)?;
                unsafe { *(data as *mut i32) = SNDRV_PCM_VERSION };
                Ok(0)
            }
            0x01 => {
                // INFO
                ucheck::<SndPcmInfo>(data)?;
                let info = unsafe { &mut *(data as *mut SndPcmInfo) };
                unsafe {
                    core::ptr::write_bytes(
                        info as *mut SndPcmInfo as *mut u8,
                        0,
                        core::mem::size_of::<SndPcmInfo>(),
                    )
                };
                info.device = 0;
                info.subdevice = 0;
                info.stream = 0; // playback
                info.card = self.card as i32;
                fill_cstr(&mut info.id, "Eclipse HDA");
                fill_cstr(&mut info.name, self.audio.name());
                fill_cstr(&mut info.subname, "subdevice #0");
                info.subdevices_count = 1;
                info.subdevices_avail = 1;
                Ok(0)
            }
            // TSTAMP / TTSTAMP / USER_PVERSION: accepted, ignored.
            0x02..=0x04 => Ok(0),
            0x10 => {
                // HW_REFINE. Returning EINVAL is how alsa-lib's `*_near`
                // helpers search; only an empty result on the initial
                // `hw_params_any` (wide-open intervals) is a real fault.
                ucheck::<SndPcmHwParams>(data)?;
                let p = unsafe { &mut *(data as *mut SndPcmHwParams) };
                let is_any = p.rmask == !0
                    && p.intervals[Self::IV_RATE].min == 0
                    && p.intervals[Self::IV_RATE].max == u32::MAX;
                let requested = p.intervals;
                if self.refine(p) {
                    Ok(0)
                } else {
                    if is_any {
                        error!(
                            "[snd] hw_refine(any) empty — ALSA sees no configs on '{}'",
                            self.audio.name()
                        );
                        Self::log_hw_params_failure("any", &requested, &p.intervals);
                    }
                    Err(FsError::InvalidParam)
                }
            }
            0x11 => {
                // HW_PARAMS: only on a stream that is not live (Linux
                // `snd_pcm_hw_params`: OPEN, SETUP or PREPARED, else EBADFD).
                // alsa-lib drops or frees a running stream before it
                // renegotiates, so this only ever refuses a caller that
                // skipped that -- and reconfiguring a live ring under it is
                // exactly what must not happen.
                ucheck::<SndPcmHwParams>(data)?;
                let state = self.st.lock().state;
                if !matches!(state, STATE_OPEN | STATE_SETUP | STATE_PREPARED) {
                    return Err(FsError::BadState);
                }
                let p = unsafe { &mut *(data as *mut SndPcmHwParams) };
                self.install(p)?;
                let _ = self.audio.set_start_hold(false);
                Ok(0)
            }
            0x12 => {
                // HW_FREE: from SETUP or PREPARED only, as on Linux.
                let mut st = self.st.lock();
                if !matches!(st.state, STATE_SETUP | STATE_PREPARED) {
                    return Err(FsError::BadState);
                }
                let _ = self.audio.reset();
                let _ = self.audio.set_start_hold(false);
                st.state = STATE_OPEN;
                st.stalled_since = None;
                Ok(0)
            }
            0x13 => {
                // SW_PARAMS, with Linux's validation (`snd_pcm_sw_params`)
                // and the struct handed back the way the kernel keeps it:
                // the boundary is the kernel's to report, never the client's
                // to set.
                ucheck::<SndPcmSwParams>(data)?;
                let p = unsafe { &mut *(data as *mut SndPcmSwParams) };
                let mut st = self.st.lock();
                if st.state == STATE_OPEN {
                    return Err(FsError::BadState);
                }
                if p.tstamp_mode < 0 || p.tstamp_mode > TSTAMP_MODE_LAST {
                    return Err(FsError::InvalidParam);
                }
                if p.proto >= 0x0002_000c && p.tstamp_type > TSTAMP_TYPE_LAST {
                    return Err(FsError::InvalidParam);
                }
                if p.avail_min == 0 {
                    return Err(FsError::InvalidParam);
                }
                if p.silence_size >= st.boundary {
                    if p.silence_threshold != 0 {
                        return Err(FsError::InvalidParam);
                    }
                } else if p.silence_size > p.silence_threshold
                    || p.silence_threshold > st.buffer_size
                {
                    return Err(FsError::InvalidParam);
                }
                st.tstamp_mode = p.tstamp_mode;
                st.tstamp_type = p.tstamp_type;
                st.period_step = p.period_step;
                st.sleep_min = p.sleep_min;
                st.avail_min = p.avail_min;
                st.xfer_align = p.xfer_align;
                st.start_threshold = p.start_threshold;
                st.stop_threshold = p.stop_threshold;
                st.silence_threshold = p.silence_threshold;
                st.silence_size = p.silence_size;
                st.proto = p.proto;
                p.boundary = st.boundary;
                // A running stream whose start_threshold just dropped to
                // what is already queued starts now (Linux does the same
                // check on the spot).
                if st.state == STATE_PREPARED
                    && self.queued_frames() > 0
                    && self.queued_capped(&st) >= st.start_threshold
                {
                    self.start_running(&mut st);
                }
                Ok(0)
            }
            0x20 | 0x24 => {
                // STATUS / STATUS_EXT
                ucheck::<SndPcmStatus>(data)?;
                let s = unsafe { &mut *(data as *mut SndPcmStatus) };
                self.fill_status(s);
                Ok(0)
            }
            0x21 => {
                // DELAY
                ucheck::<i64>(data)?;
                let mut st = self.st.lock();
                self.note_underrun(&mut st);
                if st.state == STATE_XRUN {
                    return Err(FsError::Broken);
                }
                drop(st);
                // Frames until the last written one plays: the client's own
                // queue plus whatever silence the driver has ahead of it
                // (Linux adds `runtime->delay`, the hardware's own latency,
                // the same way).
                let delay = self.audio.delay_bytes() as u64 / BYTES_PER_FRAME;
                unsafe { *(data as *mut i64) = delay as i64 };
                Ok(0)
            }
            0x22 => {
                // HWSYNC
                let mut st = self.st.lock();
                self.note_underrun(&mut st);
                if st.state == STATE_XRUN {
                    return Err(FsError::Broken);
                }
                Ok(0)
            }
            0x23 => {
                // SYNC_PTR
                ucheck::<SndPcmSyncPtr>(data)?;
                let sp = unsafe { &mut *(data as *mut SndPcmSyncPtr) };
                self.sync_ptr(sp);
                Ok(0)
            }
            0x32 => {
                // CHANNEL_INFO — interleaved S16LE stereo (32 bits per frame).
                ucheck::<SndPcmChannelInfo>(data)?;
                let info = unsafe { &mut *(data as *mut SndPcmChannelInfo) };
                let ch = info.channel;
                if ch > 1 {
                    return Err(FsError::InvalidParam);
                }
                info.offset = 0;
                info.first = ch * 16;
                info.step = 32;
                Ok(0)
            }
            0x40 => {
                // PREPARE — also the recovery path out of XRUN, so the stall
                // clock starts over with the freshly reset ring. Linux
                // refuses it from OPEN (no hw_params yet, EBADFD) and while
                // the stream is live (`snd_pcm_pre_prepare`: EBUSY).
                let mut st = self.st.lock();
                if st.state == STATE_OPEN {
                    return Err(FsError::BadState);
                }
                if matches!(st.state, STATE_RUNNING | STATE_DRAINING) {
                    return Err(FsError::Busy);
                }
                let _ = self.audio.reset();
                // The engine waits for start_threshold, or for START.
                let _ = self.audio.set_start_hold(true);
                st.stalled_since = None;
                st.appl_ptr = 0;
                st.state = STATE_PREPARED;
                Ok(0)
            }
            0x41 => {
                // RESET
                let _ = self.audio.reset();
                let mut st = self.st.lock();
                st.appl_ptr = 0;
                st.stalled_since = None;
                Ok(0)
            }
            0x42 => {
                // START: from PREPARED only (EBADFD otherwise), and a
                // playback stream with nothing queued has nothing to start
                // (EPIPE) -- unless its stop_threshold sits at the boundary,
                // in which case it is free-running and may start empty
                // (`snd_pcm_pre_start` via `snd_pcm_playback_data`). That
                // second half is what PulseAudio relies on: its sink sets
                // both thresholds to the boundary, and an EPIPE here is
                // logged and ignored, leaving the stream primed and held
                // with nothing ever releasing it.
                let mut st = self.st.lock();
                if st.state != STATE_PREPARED {
                    return Err(FsError::BadState);
                }
                if st.stop_threshold < st.boundary && self.queued_frames() == 0 {
                    return Err(FsError::Broken);
                }
                self.start_running(&mut st);
                Ok(0)
            }
            0x43 => {
                // DROP
                let mut st = self.st.lock();
                if st.state == STATE_OPEN {
                    return Err(FsError::BadState);
                }
                let _ = self.audio.reset();
                let _ = self.audio.set_start_hold(false);
                st.state = STATE_SETUP;
                st.stalled_since = None;
                Ok(0)
            }
            0x44 => self.drain().map(|_| 0),
            0x45 => {
                // PAUSE: arg 1 = pause, 0 = resume. Linux (`snd_pcm_pre_pause`)
                // pauses a RUNNING stream and resumes a PAUSED one; any other
                // state is EBADFD. A primed PREPARED stream in particular is
                // not pausable: its engine has never run (see the driver's
                // start hold), so there is nothing to stop, and pretending
                // there was is how a resume ends up setting RUN on a
                // descriptor with no stream tag.
                ucheck::<i32>(data)?;
                let enable = unsafe { *(data as *const i32) };
                let mut st = self.st.lock();
                if enable != 0 {
                    if st.state != STATE_RUNNING {
                        return Err(FsError::BadState);
                    }
                    let _ = self.audio.pause();
                    st.state = STATE_PAUSED;
                    st.stalled_since = None;
                } else {
                    if st.state != STATE_PAUSED {
                        return Err(FsError::BadState);
                    }
                    let _ = self.audio.resume();
                    st.state = STATE_RUNNING;
                    drop(st);
                    arm_playback_watchdog();
                }
                Ok(0)
            }
            0x46 => {
                // REWIND — drop the most recently written frames.
                ucheck::<u64>(data)?;
                let frames = unsafe { *(data as *mut u64) };
                let bytes = (frames * BYTES_PER_FRAME) as usize;
                let dropped = self.audio.rewind(bytes).unwrap_or(0);
                let dropped_frames = dropped as u64 / BYTES_PER_FRAME;
                if dropped_frames > 0 {
                    let mut st = self.st.lock();
                    st.appl_ptr = (st.appl_ptr + st.boundary - dropped_frames) % st.boundary.max(1);
                }
                unsafe { *(data as *mut u64) = dropped_frames };
                Ok(0)
            }
            0x47 => {
                // RESUME (system-suspend counterpart); treat as unpause.
                let _ = self.audio.resume();
                let mut st = self.st.lock();
                if st.state == STATE_PAUSED {
                    st.state = STATE_RUNNING;
                }
                drop(st);
                arm_playback_watchdog();
                Ok(0)
            }
            0x48 => {
                // XRUN — the client forcing its own stream into the underrun
                // state (alsa-lib's `snd_pcm_hw_xrun`, used by its test and
                // recovery paths). Linux only accepts it from RUNNING; from
                // anything else it is EBADFD. Answering `Ok` without moving
                // the state told the client it had happened when it had not,
                // and the PREPARE that follows arrived against a stream the
                // kernel still believed was running.
                let mut st = self.st.lock();
                if st.state != STATE_RUNNING {
                    return Err(FsError::BadState);
                }
                st.state = STATE_XRUN;
                st.stalled_since = None;
                drop(st);
                let _ = self.audio.reset();
                let _ = self.audio.set_start_hold(false);
                Ok(0)
            }
            0x49 => {
                // FORWARD — skip unplayed frames from the playhead.
                ucheck::<u64>(data)?;
                let frames = unsafe { *(data as *mut u64) };
                let bytes = (frames * BYTES_PER_FRAME) as usize;
                let skipped = self.audio.forward(bytes).unwrap_or(0);
                let skipped_frames = skipped as u64 / BYTES_PER_FRAME;
                if skipped_frames > 0 {
                    // The mirror image of REWIND: the client has given up
                    // those frames, so the application pointer moves over
                    // them. Leaving it behind made the kernel's `avail` and
                    // alsa-lib's disagree by exactly the skipped amount for
                    // the rest of the stream.
                    let mut st = self.st.lock();
                    st.appl_ptr = (st.appl_ptr + skipped_frames) % st.boundary.max(1);
                }
                unsafe { *(data as *mut u64) = skipped_frames };
                Ok(0)
            }
            0x50 => {
                // WRITEI_FRAMES
                ucheck::<SndXferI>(data)?;
                let xfer = unsafe { &mut *(data as *mut SndXferI) };
                self.writei(xfer, flags)?;
                Ok(0)
            }
            _ => {
                debug!("[snd] pcm ioctl 'A' nr={:#x} unsupported", nr);
                Err(FsError::NotSupported)
            }
        }
    }
}

impl Drop for PcmDev {
    fn drop(&mut self) {
        // Only a client handle releases the device; the registry node that
        // `new` built owns nothing.
        if !self.client {
            return;
        }
        // Closing a PCM DROPS it, as on Linux: the stream stops and whatever
        // was queued is discarded. Releasing only the open flag left the
        // hardware running with a full ring, and that starved every other
        // client on the card for as long as the machine stayed up:
        //
        //   [gpusnd] ring: running=true queued=48000 ... free=1152B of 65536B
        //
        // with PulseAudio's sink IDLE and its fd already closed by
        // module-suspend-on-idle. `/dev/dsp` is a second front end onto that
        // same ring, so it reported `0/12 fragments` free forever and
        // `mpg123 -o oss` blocked without ever playing a sample. A re-open of
        // hw:0,0 fared no better: the flag was free but the ring was not.
        //
        // Same two steps as the DROP ioctl, in the same order. A start hold
        // belongs to this open and must not reach the next client.
        let _ = self.audio.reset();
        let _ = self.audio.set_start_hold(false);
        {
            let mut st = self.st.lock();
            st.state = STATE_SETUP;
            st.appl_ptr = 0;
            st.stalled_since = None;
        }
        if self.release_opened_on_drop {
            self.opened.store(false, Ordering::Release);
        }
    }
}

impl INode for PcmDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        let _ = self.audio.queued_bytes();
        let mut st = self.st.lock();
        self.note_underrun(&mut st);
        let avail = self.avail(&st);
        // Linux reports POLLOUT when `avail >= avail_min`, but it only
        // re-evaluates that on a period interrupt, so a feeder wakes at most
        // once per period however small its avail_min (PulseAudio's tsched=0
        // sink asks for 1). This fd has no interrupt: sys_poll re-scans it
        // every 4 ms, and reporting every freed frame would wake such a
        // feeder 250 times a second to write a few frames each. Gating on a
        // whole period gives it the wake-up cadence it was written for.
        // Gate on what `writei` will ACTUALLY accept, not just on this
        // stream's notional buffer.
        //
        // `avail` is `buffer_size - queued`, per stream. The device ring is a
        // single shared resource, and this card really does get two PCM
        // streams open at once (a live log shows `PCM_WRITEI_FRAMES` on fd=15
        // and fd=16 from one process). So one stream's `avail` can promise
        // room that the other stream's data is already occupying, `writei`
        // answers EAGAIN, and alsa-lib — which was told there was space —
        // asserts and aborts the daemon:
        //
        //     [alsa-hunt] pid=1031 ioctl PCM_WRITEI_FRAMES -> EAGAIN (11)
        //     [exit] pid=1031 (pulseaudio) killed by signal SIGABRT (6)
        //     [crash-bt] ... /usr/lib/libasound.so.2+0x323f3 ...
        //
        // Reporting the minimum of the two keeps poll() honest: a feeder is
        // woken only when the write it is about to make can land. It cannot
        // help a client that writes without polling, and it does not fix the
        // underlying single-ring-for-two-streams design, but it removes the
        // case where the kernel invites a write it is going to refuse.
        let ring_free = self.audio.free_bytes() as u64 / BYTES_PER_FRAME;
        let writable = avail.min(ring_free);
        if !matches!(
            st.state,
            STATE_RUNNING | STATE_PREPARED | STATE_PAUSED | STATE_DRAINING
        ) {
            // An underrun is reported to a poller, not hidden from it. The
            // ring that stopped draining is by definition full, so the gate
            // above says "not writable" and a client that answered EAGAIN by
            // going back to poll() -- which is exactly what alsa-lib does --
            // waits there until its own timeout expires instead of collecting
            // the EPIPE that tells it to re-prepare. That is the difference
            // between a stream that recovers with a click and one that goes
            // quiet for seconds. Linux raises POLLERR together with POLLOUT
            // (`snd_pcm_poll`) for XRUN and for every other state in which
            // the stream cannot be waited on (OPEN, SETUP); POLLERR is
            // return-only, so it reaches the client whether or not it asked.
            return Ok(PollStatus {
                read: false,
                write: true,
                error: true,
                hangup: false,
            });
        }
        Ok(PollStatus {
            read: false,
            write: writable >= st.avail_min.max(st.period_size),
            error: false,
            hangup: false,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.io_control_impl(cmd, data, OpenFlags::empty())
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec_ZERO,
            mtime: Timespec_ZERO,
            ctime: Timespec_ZERO,
            type_: FileType::CharDevice,
            mode: 0o666,
            nlinks: 1,
            uid: 0,
            gid: 0,
            // ALSA: major 116, PCM playback dev = card*32 + 16 + device.
            rdev: make_rdev(116, self.card * 32 + 16),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[allow(non_upper_case_globals)]
const Timespec_ZERO: rcore_fs::vfs::Timespec = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };

// ── ALSA timer: /dev/snd/timer ──────────────────────────────────────────────
//
// alsa-lib's `hw` PCM plugin opens this node the moment a client enables
// `period_event` in its sw_params (pcm_hw.c `snd_pcm_hw_change_timer`).
// PulseAudio's module-alsa-sink does exactly that for every `tsched=0` sink
// (alsa-util.c `pa_alsa_set_sw_params`, `period_event = !use_tsched`), so
// without this node the sink dies at "Unable to set sw params: No such file
// or directory" — the errno of the open(2), surfaced through the sw_params
// call. What alsa-lib then does, in order: PVERSION, TREAD=1,
// SELECT{class PCM, card, device, subdevice<<1|stream}, PARAMS{AUTO,
// ticks=1, filter=TICK|MSUSPEND|MRESUME}, START. From then on the timer fd
// sits next to the PCM fd in poll(): POLLIN here is turned into POLLOUT on
// the PCM by `snd_pcm_hw_poll_revents`, and the queued records are read and
// discarded (`snd_pcm_hw_clear_timer_queue`).
//
// Every open is its own instance (Linux `snd_timer_user`): selection,
// params, run state and the event queue belong to the fd, so the node is a
// cloning device like `/dev/ptmx` — the open path swaps in a fresh
// [`TimerClient`]. A PCM timer ticks once per period **while the stream
// runs**. This driver has no period interrupt; the PCM exposes a period
// clock derived from its hardware pointer and the instance samples it on
// every poll/read (sys_poll re-scans bus-less fds every 4 ms), which yields
// the same "one tick per elapsed period" a Linux client observes.

const SNDRV_TIMER_VERSION: i32 = 0x0002_0007;
const TIMER_CLASS_PCM: i32 = 3;
const TIMER_PSFLG_AUTO: u32 = 1 << 0;
const TIMER_EVENT_RESOLUTION: i32 = 0;
const TIMER_EVENT_TICK: i32 = 1;
const TIMER_EVENT_START: i32 = 2;
const TIMER_EVENT_STOP: i32 = 3;
const TIMER_EVENT_CONTINUE: i32 = 4;
const TIMER_EVENT_PAUSE: i32 = 5;
const TIMER_QUEUE_DEFAULT: usize = 128;
/// `/dev/snd/timer` is char 116:33 on Linux.
const TIMER_MINOR: usize = 33;

/// What a PCM stream hands its timer: see [`PcmDev::period_clock`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PeriodClock {
    pub running: bool,
    pub periods: u64,
    pub resolution_ns: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SndTimerId {
    dev_class: i32,
    dev_sclass: i32,
    card: i32,
    device: i32,
    subdevice: i32,
}

#[repr(C)]
struct SndTimerSelect {
    id: SndTimerId,
    reserved: [u8; 32],
}

#[repr(C)]
struct SndTimerInfo {
    flags: u32,
    card: i32,
    id: [u8; 64],
    name: [u8; 80],
    reserved0: u64,
    resolution: u64,
    reserved: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SndTimerParams {
    flags: u32,
    ticks: u32,
    queue_size: u32,
    reserved0: u32,
    filter: u32,
    reserved: [u8; 60],
}

#[repr(C)]
struct SndTimerStatus {
    tstamp: Timespec,
    resolution: u32,
    lost: u32,
    overrun: u32,
    queue: u32,
    reserved: [u8; 64],
}

/// `struct snd_timer_read`: what a client without TREAD gets.
const TIMER_READ_BYTES: usize = 8;
/// `struct snd_timer_tread` on x86_64: `int event; pad; timespec; u32 val;
/// pad` — 32 bytes.
const TIMER_TREAD_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimerEvent {
    event: i32,
    tstamp: Timespec,
    val: u32,
}

impl PartialEq for Timespec {
    fn eq(&self, o: &Self) -> bool {
        self.sec == o.sec && self.nsec == o.nsec
    }
}
impl Eq for Timespec {}
impl core::fmt::Debug for Timespec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{:09}", self.sec, self.nsec)
    }
}

/// One `snd_timer_user` instance: everything an open fd of `/dev/snd/timer`
/// owns. Pure state machine over [`PeriodClock`] samples, so it is unit
/// tested without a sound device.
struct TimerState {
    tread: bool,
    /// Card index of the PCM timer bound by SELECT.
    selected: Option<usize>,
    running: bool,
    auto: bool,
    /// Periods per delivered tick (PARAMS `ticks`, >= 1).
    ticks: u32,
    filter: u32,
    queue_size: usize,
    queue: VecDeque<TimerEvent>,
    overrun: u32,
    /// Period count at the last sample while the stream ran; `None` until
    /// the first running sample after START (or after the stream stopped),
    /// so a stream that (re)starts at pointer 0 does not tick backwards.
    last_periods: Option<u64>,
    /// Elapsed periods not yet worth a tick (`< ticks`).
    partial: u32,
    resolution_ns: u64,
    /// Timestamp of the last START/STOP/CONTINUE/PAUSE, as STATUS reports it.
    tstamp: Timespec,
}

impl TimerState {
    fn new() -> Self {
        TimerState {
            tread: false,
            selected: None,
            running: false,
            auto: false,
            ticks: 1,
            filter: 0,
            queue_size: TIMER_QUEUE_DEFAULT,
            queue: VecDeque::new(),
            overrun: 0,
            last_periods: None,
            partial: 0,
            resolution_ns: 0,
            tstamp: Timespec::default(),
        }
    }

    /// Linux `snd_timer_user_ccallback`/`tinterrupt` gate: a TREAD client
    /// gets exactly the events its filter names; a plain client only ticks.
    fn push(&mut self, event: i32, tstamp: Timespec, val: u32) {
        if self.tread {
            if self.filter & (1u32 << event) == 0 {
                return;
            }
        } else if event != TIMER_EVENT_TICK {
            return;
        }
        if event == TIMER_EVENT_TICK {
            if let Some(last) = self.queue.back_mut() {
                if last.event == TIMER_EVENT_TICK {
                    // Ticks nobody has read yet coalesce (tinterrupt does the
                    // same), so a slow reader sees one record with the count.
                    last.tstamp = tstamp;
                    last.val = last.val.saturating_add(val);
                    return;
                }
            }
        }
        if self.queue.len() >= self.queue_size {
            self.overrun = self.overrun.saturating_add(1);
            return;
        }
        self.queue.push_back(TimerEvent { event, tstamp, val });
    }

    fn select(&mut self, card: usize) {
        self.selected = Some(card);
        self.running = false;
        self.queue.clear();
        self.overrun = 0;
        self.last_periods = None;
        self.partial = 0;
    }

    fn params(&mut self, p: &SndTimerParams, now: Timespec) -> Result<()> {
        if self.selected.is_none() {
            return Err(FsError::NoDevice);
        }
        if p.ticks < 1 {
            return Err(FsError::InvalidParam);
        }
        let queue_size = match p.queue_size {
            0 => TIMER_QUEUE_DEFAULT,
            32..=1024 => p.queue_size as usize,
            _ => return Err(FsError::InvalidParam),
        };
        // PARAMS stops a running timer and starts the queue afresh.
        self.running = false;
        self.auto = p.flags & TIMER_PSFLG_AUTO != 0;
        self.ticks = p.ticks;
        self.filter = p.filter;
        self.queue_size = queue_size;
        self.queue.clear();
        self.overrun = 0;
        self.partial = 0;
        if self.resolution_ns > 0 {
            let res = self.resolution_ns as u32;
            self.push(TIMER_EVENT_RESOLUTION, now, res);
        }
        Ok(())
    }

    /// START / CONTINUE: `event` names which, for the record a TREAD client
    /// with that bit in its filter receives. EBUSY when already running, as
    /// `snd_timer_start1` answers.
    fn start(&mut self, event: i32, now: Timespec) -> Result<()> {
        if self.selected.is_none() {
            return Err(FsError::NoDevice);
        }
        if self.running {
            return Err(FsError::Busy);
        }
        self.running = true;
        self.last_periods = None;
        self.partial = 0;
        self.tstamp = now;
        let res = self.resolution_ns as u32;
        self.push(event, now, res);
        Ok(())
    }

    /// STOP / PAUSE. EBUSY when not running (`snd_timer_stop1`).
    fn stop(&mut self, event: i32, now: Timespec) -> Result<()> {
        if self.selected.is_none() {
            return Err(FsError::NoDevice);
        }
        if !self.running {
            return Err(FsError::Busy);
        }
        self.running = false;
        self.last_periods = None;
        self.tstamp = now;
        self.push(event, now, 0);
        Ok(())
    }

    /// Fold a fresh reading of the bound stream into the queue: one tick per
    /// `ticks` periods elapsed while both the timer and the stream run. A
    /// pointer that went backwards (PREPARE reset it) re-synchronises
    /// silently, exactly like a stream that was stopped and started again.
    fn sample(&mut self, clock: PeriodClock, now: Timespec) {
        self.resolution_ns = clock.resolution_ns;
        if !self.running {
            return;
        }
        if !clock.running {
            self.last_periods = None;
            return;
        }
        let elapsed = match self.last_periods {
            Some(last) if clock.periods >= last => clock.periods - last,
            _ => 0,
        };
        self.last_periods = Some(clock.periods);
        if elapsed == 0 {
            return;
        }
        let total = u64::from(self.partial).saturating_add(elapsed);
        let ticks = u64::from(self.ticks.max(1));
        let events = total / ticks;
        self.partial = (total % ticks) as u32;
        if events == 0 {
            return;
        }
        self.push(
            TIMER_EVENT_TICK,
            now,
            events.min(u64::from(u32::MAX)) as u32,
        );
        if !self.auto {
            // One-shot: the first tick ends the run (no AUTO flag).
            self.running = false;
            self.last_periods = None;
        }
    }

    /// Serialise queued records into `buf` the way `snd_timer_user_read`
    /// does: whole records only, `struct snd_timer_tread` for a TREAD client
    /// and `struct snd_timer_read` otherwise. `None` when nothing is queued
    /// (the caller answers EAGAIN: alsa-lib opens this fd O_NONBLOCK).
    fn read(&mut self, buf: &mut [u8]) -> Option<usize> {
        let rec = if self.tread {
            TIMER_TREAD_BYTES
        } else {
            TIMER_READ_BYTES
        };
        let mut done = 0;
        while done + rec <= buf.len() {
            let Some(ev) = self.queue.pop_front() else {
                break;
            };
            let out = &mut buf[done..done + rec];
            if self.tread {
                out[0..4].copy_from_slice(&ev.event.to_ne_bytes());
                out[4..8].fill(0);
                out[8..16].copy_from_slice(&ev.tstamp.sec.to_ne_bytes());
                out[16..24].copy_from_slice(&ev.tstamp.nsec.to_ne_bytes());
                out[24..28].copy_from_slice(&ev.val.to_ne_bytes());
                out[28..32].fill(0);
            } else {
                let res = self.resolution_ns.min(u64::from(u32::MAX)) as u32;
                out[0..4].copy_from_slice(&res.to_ne_bytes());
                out[4..8].copy_from_slice(&ev.val.to_ne_bytes());
            }
            done += rec;
        }
        if done == 0 {
            None
        } else {
            Some(done)
        }
    }
}

fn now_timespec() -> Timespec {
    let now = kernel_hal::timer::timer_now();
    Timespec {
        sec: now.as_secs() as i64,
        nsec: i64::from(now.subsec_nanos()),
    }
}

fn timer_metadata(inode_id: usize) -> Metadata {
    Metadata {
        dev: 1,
        inode: inode_id,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: Timespec_ZERO,
        mtime: Timespec_ZERO,
        ctime: Timespec_ZERO,
        type_: FileType::CharDevice,
        mode: 0o666,
        nlinks: 1,
        uid: 0,
        gid: 0,
        rdev: make_rdev(116, TIMER_MINOR),
    }
}

/// The `/dev/snd/timer` node. Resolving it is normal; the open path
/// downcasts to this type and calls [`TimerDev::open_client`] so every fd
/// gets its own [`TimerClient`].
pub struct TimerDev {
    pcms: Vec<Arc<PcmDev>>,
    inode_id: usize,
}

impl TimerDev {
    /// `pcms[card]` is the playback stream of card `card`.
    pub fn new(pcms: Vec<Arc<PcmDev>>) -> Self {
        TimerDev {
            pcms,
            inode_id: DevFS::new_inode_id(),
        }
    }

    pub fn open_client(&self) -> Arc<dyn INode> {
        Arc::new(TimerClient {
            pcms: self.pcms.clone(),
            inode_id: self.inode_id,
            st: Mutex::new(TimerState::new()),
        })
    }
}

impl INode for TimerDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: false,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(timer_metadata(self.inode_id))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// One open of `/dev/snd/timer`.
pub struct TimerClient {
    pcms: Vec<Arc<PcmDev>>,
    inode_id: usize,
    st: Mutex<TimerState>,
}

impl TimerClient {
    /// Refresh the queue from the bound stream. Held lock: `st`.
    fn sample(&self, st: &mut TimerState) {
        if let Some(pcm) = st.selected.and_then(|card| self.pcms.get(card)) {
            let clock = pcm.period_clock();
            st.sample(clock, now_timespec());
        }
    }
}

impl INode for TimerClient {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        let mut st = self.st.lock();
        self.sample(&mut st);
        st.read(buf).ok_or(FsError::Again)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        let mut st = self.st.lock();
        self.sample(&mut st);
        Ok(PollStatus {
            read: !st.queue.is_empty(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        let ty = (cmd >> 8) & 0xff;
        let nr = cmd & 0xff;
        if ty != b'T' as u32 {
            return Err(FsError::NotSupported);
        }
        match nr {
            0x00 => {
                // PVERSION
                ucheck::<i32>(data)?;
                unsafe { *(data as *mut i32) = SNDRV_TIMER_VERSION };
                Ok(0)
            }
            0x02 | 0xa4 => {
                // TREAD (old) / TREAD64: extended, timestamped records. Only
                // before SELECT, as Linux insists.
                ucheck::<i32>(data)?;
                let on = unsafe { *(data as *const i32) };
                let mut st = self.st.lock();
                if st.selected.is_some() {
                    return Err(FsError::Busy);
                }
                st.tread = on != 0;
                Ok(0)
            }
            0x10 => {
                // SELECT: only the PCM playback timers exist here — class
                // PCM, device 0, subdevice<<1|stream == 0 (playback of
                // substream 0) — anything else is ENODEV, like a card that
                // has no such timer.
                ucheck::<SndTimerSelect>(data)?;
                let sel = unsafe { &*(data as *const SndTimerSelect) };
                let id = sel.id;
                let card = usize::try_from(id.card).map_err(|_| FsError::NoDevice)?;
                if id.dev_class != TIMER_CLASS_PCM
                    || id.device != 0
                    || id.subdevice != 0
                    || card >= self.pcms.len()
                {
                    return Err(FsError::NoDevice);
                }
                let mut st = self.st.lock();
                st.select(card);
                st.resolution_ns = self.pcms[card].period_clock().resolution_ns;
                Ok(0)
            }
            0x11 => {
                // INFO
                ucheck::<SndTimerInfo>(data)?;
                let info = unsafe { &mut *(data as *mut SndTimerInfo) };
                let mut st = self.st.lock();
                let Some(card) = st.selected else {
                    return Err(FsError::NoDevice);
                };
                self.sample(&mut st);
                unsafe {
                    core::ptr::write_bytes(
                        info as *mut SndTimerInfo as *mut u8,
                        0,
                        core::mem::size_of::<SndTimerInfo>(),
                    )
                };
                info.card = card as i32;
                fill_cstr(&mut info.id, "pcm");
                fill_cstr(&mut info.name, "PCM playback");
                info.resolution = st.resolution_ns;
                Ok(0)
            }
            0x12 => {
                // PARAMS
                ucheck::<SndTimerParams>(data)?;
                let p = unsafe { *(data as *const SndTimerParams) };
                let mut st = self.st.lock();
                self.sample(&mut st);
                st.params(&p, now_timespec())?;
                Ok(0)
            }
            0x14 => {
                // STATUS
                ucheck::<SndTimerStatus>(data)?;
                let s = unsafe { &mut *(data as *mut SndTimerStatus) };
                let mut st = self.st.lock();
                if st.selected.is_none() {
                    return Err(FsError::NoDevice);
                }
                self.sample(&mut st);
                unsafe {
                    core::ptr::write_bytes(
                        s as *mut SndTimerStatus as *mut u8,
                        0,
                        core::mem::size_of::<SndTimerStatus>(),
                    )
                };
                s.tstamp = st.tstamp;
                s.resolution = st.resolution_ns.min(u64::from(u32::MAX)) as u32;
                s.overrun = st.overrun;
                s.queue = st.queue.len() as u32;
                Ok(0)
            }
            // START / STOP / CONTINUE / PAUSE, new (0xa0..) and pre-1.0.9
            // (0x20..) numbers: alsa-lib picks by the version we report.
            0xa0 | 0x20 => {
                let mut st = self.st.lock();
                self.sample(&mut st);
                st.start(TIMER_EVENT_START, now_timespec())?;
                Ok(0)
            }
            0xa1 | 0x21 => {
                let mut st = self.st.lock();
                self.sample(&mut st);
                st.stop(TIMER_EVENT_STOP, now_timespec())?;
                Ok(0)
            }
            0xa2 | 0x22 => {
                let mut st = self.st.lock();
                self.sample(&mut st);
                st.start(TIMER_EVENT_CONTINUE, now_timespec())?;
                Ok(0)
            }
            0xa3 | 0x23 => {
                let mut st = self.st.lock();
                self.sample(&mut st);
                st.stop(TIMER_EVENT_PAUSE, now_timespec())?;
                Ok(0)
            }
            _ => {
                // NEXT_DEVICE / GINFO / GPARAMS / GSTATUS (timer enumeration,
                // `snd_timer_query`) and the user-driven timers: nothing on
                // the PCM period path asks for them.
                debug!("[snd] timer ioctl 'T' nr={:#x} unsupported", nr);
                Err(FsError::NotSupported)
            }
        }
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(timer_metadata(self.inode_id))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod timer_tests {
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
                        (frames as u64 * rate as u64 / link as u64) as usize
                            * BYTES_PER_FRAME as usize
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

        /// FORWARD is REWIND's mirror image: the frames it gives up move the
        /// application pointer with them, or the kernel's `avail` and
        /// alsa-lib's disagree by that much for the rest of the stream.
        #[test]
        fn forward_moves_the_application_pointer_like_rewind_does() {
            let audio = Arc::new(FakeAudio::new(8 * BYTES_PER_FRAME as usize));
            let pcm = PcmDev::new(audio, 0);
            pcm.st.lock().state = STATE_PREPARED;
            let samples = [0u8; 8 * BYTES_PER_FRAME as usize];
            let mut xfer = SndXferI {
                result: 0,
                buf: samples.as_ptr() as usize as u64,
                frames: 8,
            };
            pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
            assert_eq!(pcm.st.lock().appl_ptr, 8);

            let mut frames: u64 = 3;
            pcm.io_control(0x4149, &mut frames as *mut u64 as usize)
                .unwrap();
            assert_eq!(frames, 3, "three frames skipped");
            assert_eq!(pcm.st.lock().appl_ptr, 11);

            let mut back: u64 = 3;
            pcm.io_control(0x4146, &mut back as *mut u64 as usize)
                .unwrap();
            assert_eq!(back, 3);
            assert_eq!(pcm.st.lock().appl_ptr, 8, "REWIND puts it back");
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
            p.stop_threshold = 16;
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
            filter: (1 << TIMER_EVENT_TICK)
                | (1 << TIMER_EVENT_CONTINUE)
                | (1 << TIMER_EVENT_PAUSE),
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
}

// ── Control device node ─────────────────────────────────────────────────────

pub struct CtlDev {
    audio: Arc<dyn AudioScheme>,
    card: usize,
    inode_id: usize,
}

impl CtlDev {
    pub fn new(audio: Arc<dyn AudioScheme>, card: usize) -> Self {
        CtlDev {
            audio,
            card,
            inode_id: DevFS::new_inode_id(),
        }
    }

    fn elem_list(&self, data: usize) -> Result<usize> {
        ucheck::<SndCtlElemList>(data)?;
        let list = unsafe { &mut *(data as *mut SndCtlElemList) };
        list.count = MIXER_ELEMS;
        let offset = list.offset;
        let space = list.space;
        let mut used = 0u32;
        // `pids` is a second user pointer carried INSIDE the struct, so
        // validating the struct says nothing about it. Everything else in
        // this file checks the buffer it is about to dereference
        // (`user_range_ok`, the kernel's `access_ok`); this one wrote up to
        // `MIXER_ELEMS` element ids wherever the caller pointed, kernel
        // addresses included. Only what is actually going to be written is
        // checked, so a client that offers a huge `space` (alsa-lib sizes it
        // from `count`) is not refused for space it never uses.
        let will_write = space.min(MIXER_ELEMS.saturating_sub(offset.min(MIXER_ELEMS)));
        if will_write > 0
            && !kernel_hal::user::user_range_ok(
                list.pids as usize,
                will_write as usize * core::mem::size_of::<SndCtlElemId>(),
            )
        {
            return Err(FsError::BadAddress);
        }
        if list.pids != 0 && space > 0 {
            let ids = list.pids as *mut SndCtlElemId;
            while used < space {
                let idx = offset.saturating_add(used);
                let Some(elem) = mixer_elem_at(idx) else {
                    break;
                };
                unsafe { fill_elem_id(&mut *ids.add(used as usize), elem) };
                used += 1;
            }
        }
        list.used = used;
        Ok(0)
    }

    fn elem_info(&self, data: usize) -> Result<usize> {
        ucheck::<SndCtlElemInfo>(data)?;
        let info = unsafe { &mut *(data as *mut SndCtlElemInfo) };
        let elem = elem_from_id(&info.id).ok_or(FsError::EntryNotFound)?;
        unsafe {
            core::ptr::write_bytes(
                info as *mut SndCtlElemInfo as *mut u8,
                0,
                core::mem::size_of::<SndCtlElemInfo>(),
            )
        };
        fill_elem_id(&mut info.id, elem);
        info.access = ACCESS_READ_WRITE;
        info.count = 2;
        info.owner = -1;
        match elem {
            MixerElem::Volume => {
                info.type_ = ELEM_INTEGER;
                info.min = 0;
                info.max = 100;
                info.step = 1;
            }
            MixerElem::Switch => {
                info.type_ = ELEM_BOOLEAN;
                info.min = 0;
                info.max = 1;
                info.step = 1;
            }
        }
        Ok(0)
    }

    fn elem_read(&self, data: usize) -> Result<usize> {
        ucheck::<SndCtlElemValue>(data)?;
        let val = unsafe { &mut *(data as *mut SndCtlElemValue) };
        let elem = elem_from_id(&val.id).ok_or(FsError::EntryNotFound)?;
        fill_elem_id(&mut val.id, elem);
        let (l, r, mute_l, mute_r) = self.audio.gain();
        match elem {
            MixerElem::Volume => {
                val.values[0] = l as i64;
                val.values[1] = r as i64;
            }
            MixerElem::Switch => {
                // ALSA switch: 1 = unmuted.
                val.values[0] = i64::from(!mute_l);
                val.values[1] = i64::from(!mute_r);
            }
        }
        Ok(0)
    }

    fn elem_write(&self, data: usize) -> Result<usize> {
        ucheck::<SndCtlElemValue>(data)?;
        let val = unsafe { &*(data as *const SndCtlElemValue) };
        let elem = elem_from_id(&val.id).ok_or(FsError::EntryNotFound)?;
        let (mut l, mut r, mut mute_l, mut mute_r) = self.audio.gain();
        match elem {
            MixerElem::Volume => {
                l = val.values[0].clamp(0, 100) as u8;
                r = val.values[1].clamp(0, 100) as u8;
            }
            MixerElem::Switch => {
                mute_l = val.values[0] == 0;
                mute_r = val.values[1] == 0;
            }
        }
        self.audio
            .set_gain(l, r, mute_l, mute_r)
            .map_err(|_| FsError::DeviceError)?;
        Ok(0)
    }
}

impl INode for CtlDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: false,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        let ty = (cmd >> 8) & 0xff;
        let nr = cmd & 0xff;
        if ty != b'U' as u32 {
            return Err(FsError::NotSupported);
        }
        match nr {
            0x00 => {
                ucheck::<i32>(data)?;
                unsafe { *(data as *mut i32) = SNDRV_CTL_VERSION };
                Ok(0)
            }
            0x01 => {
                // CARD_INFO
                ucheck::<SndCtlCardInfo>(data)?;
                let info = unsafe { &mut *(data as *mut SndCtlCardInfo) };
                unsafe {
                    core::ptr::write_bytes(
                        info as *mut SndCtlCardInfo as *mut u8,
                        0,
                        core::mem::size_of::<SndCtlCardInfo>(),
                    )
                };
                info.card = self.card as i32;
                fill_cstr(&mut info.id, &alloc::format!("EclipseHDA{}", self.card));
                fill_cstr(&mut info.driver, "eclipse-hda");
                fill_cstr(&mut info.name, self.audio.name());
                fill_cstr(&mut info.longname, self.audio.name());
                fill_cstr(&mut info.mixername, "Eclipse Mixer");
                fill_cstr(&mut info.components, "");
                Ok(0)
            }
            0x10 => self.elem_list(data),
            0x11 => self.elem_info(data),
            0x12 => self.elem_read(data),
            0x13 => self.elem_write(data),
            0x14 | 0x15 => Ok(0), // ELEM_LOCK / ELEM_UNLOCK
            0x16 => {
                // SUBSCRIBE_EVENTS: we don't queue notifications yet (poll
                // never signals), but alsamixer aborts if this ioctl fails.
                ucheck::<i32>(data)?;
                let v = unsafe { &mut *(data as *mut i32) };
                if *v < 0 {
                    *v = 0;
                }
                Ok(0)
            }
            0x30 => {
                // PCM_NEXT_DEVICE: single PCM device (0).
                ucheck::<i32>(data)?;
                let v = unsafe { &mut *(data as *mut i32) };
                *v = if *v < 0 { 0 } else { -1 };
                Ok(0)
            }
            0x31 => {
                // PCM_INFO
                ucheck::<SndPcmInfo>(data)?;
                let info = unsafe { &mut *(data as *mut SndPcmInfo) };
                if info.device != 0 || info.stream != 0 {
                    return Err(FsError::EntryNotFound);
                }
                let device = info.device;
                let subdevice = info.subdevice;
                unsafe {
                    core::ptr::write_bytes(
                        info as *mut SndPcmInfo as *mut u8,
                        0,
                        core::mem::size_of::<SndPcmInfo>(),
                    )
                };
                info.device = device;
                info.subdevice = subdevice;
                info.stream = 0;
                info.card = self.card as i32;
                fill_cstr(&mut info.id, "Eclipse HDA");
                fill_cstr(&mut info.name, self.audio.name());
                fill_cstr(&mut info.subname, "subdevice #0");
                info.subdevices_count = 1;
                info.subdevices_avail = 1;
                Ok(0)
            }
            0x32 => Ok(0), // PCM_PREFER_SUBDEVICE
            0x1a => {
                // TLV_READ: neither control carries a dB scale (the gain is
                // a percentage the driver applies in software). Linux
                // answers ENXIO for a control without TLV data; alsamixer
                // then shows plain percentages.
                Err(FsError::NoSuchDeviceOrAddress)
            }
            0x20 | 0x40 => {
                // HWDEP_NEXT_DEVICE / RAWMIDI_NEXT_DEVICE: none on this card.
                ucheck::<i32>(data)?;
                unsafe { *(data as *mut i32) = -1 };
                Ok(0)
            }
            0xd0 => Ok(0), // POWER (no power management)
            0xd1 => {
                // POWER_STATE: SNDRV_CTL_POWER_D0.
                ucheck::<i32>(data)?;
                unsafe { *(data as *mut i32) = 0 };
                Ok(0)
            }
            _ => {
                debug!("[snd] ctl ioctl 'U' nr={:#x} unsupported", nr);
                Err(FsError::NotSupported)
            }
        }
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec_ZERO,
            mtime: Timespec_ZERO,
            ctime: Timespec_ZERO,
            type_: FileType::CharDevice,
            mode: 0o666,
            nlinks: 1,
            uid: 0,
            gid: 0,
            // ALSA: major 116, control dev = card*32.
            rdev: make_rdev(116, self.card * 32),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}
