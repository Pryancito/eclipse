//! `/dev/dsp` — OSS PCM playback node over an [`AudioScheme`] device,
//! modelled on Linux's `snd-pcm-oss` (`sound/core/oss/pcm_oss.c`).
//!
//! The OSS interface is the smallest kernel audio ABI that stock userspace
//! can drive: `write(2)` carries interleaved PCM, and a handful of ioctls
//! negotiate rate/format/channels and the fragment geometry (`mpg123 -o oss`,
//! `sox -t oss`, ffmpeg's `-f oss`, SDL's `dsp` driver, mpv's `oss` output
//! all speak it). One node is created per HDA device found, in the same
//! order as `/dev/snd`: `dsp` is ALSA card 0 (HDMI/DP with a live display
//! outranks analog), `dsp1`, `dsp2`, … for the rest. `/dev/audio<N>` is the
//! same node with the Sun defaults (µ-law, 8 kHz, mono).
//!
//! What a program written against Linux's `/dev/dsp` gets here:
//!
//! * **Every format Linux's emulation converts** (`SNDCTL_DSP_GETFMTS`):
//!   µ-law, A-law, U8/S8, S16/U16 both endians, S24 (packed and in 32 bits),
//!   S32 both endians and float. The ring carries S16LE stereo; this node
//!   converts on the way in, the way Linux's `plug` layer does. Mono is
//!   duplicated onto both channels. An unknown format is answered with
//!   `AFMT_U8`, as on Linux.
//! * **Fragment geometry that means something.** `SETFRAGMENT`/`SUBDIVIDE`
//!   shape a buffer of `periods × period_bytes` client bytes (Linux's
//!   `snd_pcm_oss_period_size` algorithm, with the ring as the slave
//!   constraint); `GETBLKSIZE`, `GETOSPACE`, `GETOPTR`, `GETODELAY` and
//!   `poll(2)` all report against it, and a write never queues past it, so a
//!   client that asked for two 4 KiB fragments gets the latency it asked for
//!   instead of the whole ring.
//! * **Triggers.** `SETTRIGGER` without `PCM_ENABLE_OUTPUT` drops what is
//!   queued and holds the stream: writes fill the buffer, nothing plays until
//!   the bit is set again (the OSS "fill, then start" idiom).
//! * **Blocking and `O_NONBLOCK`/`SNDCTL_DSP_NONBLOCK` writes** with Linux's
//!   return values: a partial count when something fit, `EAGAIN` when
//!   nothing did. A blocking write retries against the device ring (the
//!   `INode` contract is synchronous, so there is no waker to park on).
//! * **The rest of the table**: `SYNC`, `POST`, `RESET`, `GETCAPS`,
//!   `OSS_GETVERSION`, `SOUND_PCM_READ_*`, `SETDUPLEX` (`EIO`, no capture),
//!   `GETISPACE`/`GETIPTR` (`EINVAL`), `MAPINBUF`/`MAPOUTBUF` (`EINVAL`),
//!   `SETSYNCRO`/`PROFILE` (accepted). Anything else is `EINVAL`, which is
//!   what Linux answers too.
//!
//! Deliberate differences from Linux: the default parameters on `/dev/dsp`
//! are 48 kHz S16LE stereo rather than 8 kHz U8 mono, so a plain
//! `cat music.raw > /dev/dsp` plays a modern raw file (every real client sets
//! all three explicitly, so only `cat` can tell); and rates are snapped to
//! the nearest one the HDA stream format encodes and reported back, where
//! Linux resamples to the exact request. Capture and mmap do not exist.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::Ordering;

use kernel_hal::drivers::scheme::AudioScheme;
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

use super::snd::{new_audio_claim, AudioClaim};

// OSS ioctl numbers (Linux _IOC encoding of <sys/soundcard.h>, x86_64).
// Matched on the full command word: several share a number and differ only
// in direction (`SNDCTL_DSP_SPEED` / `SOUND_PCM_READ_RATE`, `PROFILE` /
// `GETODELAY`).
const SNDCTL_DSP_RESET: u32 = 0x0000_5000; // _IO('P', 0)
const SNDCTL_DSP_SYNC: u32 = 0x0000_5001; // _IO('P', 1)
const SNDCTL_DSP_SPEED: u32 = 0xc004_5002; // _IOWR('P', 2, int)
const SOUND_PCM_READ_RATE: u32 = 0x8004_5002; // _IOR('P', 2, int)
const SNDCTL_DSP_STEREO: u32 = 0xc004_5003; // _IOWR('P', 3, int)
const SNDCTL_DSP_GETBLKSIZE: u32 = 0xc004_5004; // _IOWR('P', 4, int)
const SNDCTL_DSP_SETFMT: u32 = 0xc004_5005; // _IOWR('P', 5, int)
const SOUND_PCM_READ_BITS: u32 = 0x8004_5005; // _IOR('P', 5, int)
const SNDCTL_DSP_CHANNELS: u32 = 0xc004_5006; // _IOWR('P', 6, int)
const SOUND_PCM_READ_CHANNELS: u32 = 0x8004_5006; // _IOR('P', 6, int)
const SOUND_PCM_WRITE_FILTER: u32 = 0xc004_5007; // _IOWR('P', 7, int)
const SOUND_PCM_READ_FILTER: u32 = 0x8004_5007; // _IOR('P', 7, int)
const SNDCTL_DSP_POST: u32 = 0x0000_5008; // _IO('P', 8)
const SNDCTL_DSP_SUBDIVIDE: u32 = 0xc004_5009; // _IOWR('P', 9, int)
const SNDCTL_DSP_SETFRAGMENT: u32 = 0xc004_500a; // _IOWR('P', 10, int)
const SNDCTL_DSP_GETFMTS: u32 = 0x8004_500b; // _IOR('P', 11, int)
const SNDCTL_DSP_GETOSPACE: u32 = 0x8010_500c; // _IOR('P', 12, audio_buf_info)
const SNDCTL_DSP_GETISPACE: u32 = 0x8010_500d; // _IOR('P', 13, audio_buf_info)
const SNDCTL_DSP_NONBLOCK: u32 = 0x0000_500e; // _IO('P', 14)
const SNDCTL_DSP_GETCAPS: u32 = 0x8004_500f; // _IOR('P', 15, int)
const SNDCTL_DSP_GETTRIGGER: u32 = 0x8004_5010; // _IOR('P', 16, int)
const SNDCTL_DSP_SETTRIGGER: u32 = 0x4004_5010; // _IOW('P', 16, int)
const SNDCTL_DSP_GETIPTR: u32 = 0x800c_5011; // _IOR('P', 17, count_info)
const SNDCTL_DSP_GETOPTR: u32 = 0x800c_5012; // _IOR('P', 18, count_info)
const SNDCTL_DSP_MAPINBUF_NR: u32 = 0x13; // _IOR('P', 19, buffmem_desc)
const SNDCTL_DSP_MAPOUTBUF_NR: u32 = 0x14; // _IOR('P', 20, buffmem_desc)
const SNDCTL_DSP_SETSYNCRO: u32 = 0x0000_5015; // _IO('P', 21)
const SNDCTL_DSP_SETDUPLEX: u32 = 0x0000_5016; // _IO('P', 22)
const SNDCTL_DSP_GETODELAY: u32 = 0x8004_5017; // _IOR('P', 23, int)
const SNDCTL_DSP_PROFILE: u32 = 0x4004_5017; // _IOW('P', 23, int)
const OSS_GETVERSION: u32 = 0x8004_4d76; // _IOR('M', 118, int)

/// `SNDRV_OSS_VERSION` (0x030832), what Linux's emulation reports.
pub(crate) const SNDRV_OSS_VERSION: i32 = (3 << 16) | (8 << 8) | (1 << 4) | 50;

// AFMT_* (soundcard.h, plus the OSSv4 values pcm_oss.c defines privately).
const AFMT_QUERY: i32 = 0x0000_0000;
const AFMT_MU_LAW: i32 = 0x0000_0001;
const AFMT_A_LAW: i32 = 0x0000_0002;
const AFMT_U8: i32 = 0x0000_0008;
const AFMT_S16_LE: i32 = 0x0000_0010;
const AFMT_S16_BE: i32 = 0x0000_0020;
const AFMT_S8: i32 = 0x0000_0040;
const AFMT_U16_LE: i32 = 0x0000_0080;
const AFMT_U16_BE: i32 = 0x0000_0100;
const AFMT_S32_LE: i32 = 0x0000_1000;
const AFMT_S32_BE: i32 = 0x0000_2000;
const AFMT_FLOAT: i32 = 0x0000_4000;
const AFMT_S24_LE: i32 = 0x0000_8000;
const AFMT_S24_BE: i32 = 0x0001_0000;
const AFMT_S24_PACKED: i32 = 0x0004_0000;

/// Every format a client may set; the list Linux's `snd_pcm_oss_get_formats`
/// answers for a plugin-capable playback stream, plus A-law and float.
const SUPPORTED_FORMATS: i32 = AFMT_MU_LAW
    | AFMT_A_LAW
    | AFMT_U8
    | AFMT_S16_LE
    | AFMT_S16_BE
    | AFMT_S8
    | AFMT_U16_LE
    | AFMT_U16_BE
    | AFMT_S32_LE
    | AFMT_S32_BE
    | AFMT_FLOAT
    | AFMT_S24_LE
    | AFMT_S24_BE
    | AFMT_S24_PACKED;

const PCM_ENABLE_OUTPUT: i32 = 0x0000_0002;

const DSP_CAP_REVISION: i32 = 0x0000_00ff;
const DSP_CAP_REALTIME: i32 = 0x0000_0004;
const DSP_CAP_TRIGGER: i32 = 0x0000_1000;

/// What the ring does: S16LE stereo.
const HW_FRAME: usize = 4;
/// Slave constraints, the same the native PCM's `hw_params` refine applies
/// (`snd.rs`): at least 128 frames per period and 512 per buffer.
const MIN_PERIOD_FRAMES: usize = 128;
const MIN_BUFFER_FRAMES: usize = 512;

/// `audio_buf_info` for GETOSPACE / GETISPACE.
#[derive(Clone, Copy)]
#[repr(C)]
struct AudioBufInfo {
    fragments: i32,
    fragstotal: i32,
    fragsize: i32,
    bytes: i32,
}

/// `count_info` for GETOPTR / GETIPTR.
#[derive(Clone, Copy)]
#[repr(C)]
struct CountInfo {
    bytes: i32,
    blocks: i32,
    ptr: i32,
}

/// How long a blocking write waits before re-offering PCM to a full buffer.
const RETRY_BACKOFF: core::time::Duration = core::time::Duration::from_micros(250);

fn uread<T: Copy>(addr: usize) -> Result<T> {
    if !kernel_hal::user::user_range_ok(addr, core::mem::size_of::<T>())
        || !addr.is_multiple_of(core::mem::align_of::<T>())
    {
        return Err(FsError::BadAddress);
    }
    Ok(unsafe { core::ptr::read_unaligned(addr as *const T) })
}

fn uwrite<T: Copy>(addr: usize, val: T) -> Result<()> {
    if !kernel_hal::user::user_range_ok(addr, core::mem::size_of::<T>())
        || !addr.is_multiple_of(core::mem::align_of::<T>())
    {
        return Err(FsError::BadAddress);
    }
    unsafe { core::ptr::write_unaligned(addr as *mut T, val) };
    Ok(())
}

// ── Sample conversion ───────────────────────────────────────────────────────

/// Bytes per sample of a client format, `None` for one this node cannot
/// take (which `SETFMT` never installs).
fn sample_bytes(format: i32) -> Option<usize> {
    Some(match format {
        AFMT_MU_LAW | AFMT_A_LAW | AFMT_U8 | AFMT_S8 => 1,
        AFMT_S16_LE | AFMT_S16_BE | AFMT_U16_LE | AFMT_U16_BE => 2,
        AFMT_S24_PACKED => 3,
        AFMT_S32_LE | AFMT_S32_BE | AFMT_FLOAT | AFMT_S24_LE | AFMT_S24_BE => 4,
        _ => return None,
    })
}

/// G.711 µ-law byte to a linear 16-bit sample.
fn mulaw_to_i16(u: u8) -> i16 {
    let u = !u;
    let exp = (u >> 4) & 7;
    let mant = (u & 0xf) as i32;
    let mut s = ((mant << 3) + 0x84) << exp;
    s -= 0x84;
    if u & 0x80 != 0 {
        -s as i16
    } else {
        s as i16
    }
}

/// G.711 A-law byte to a linear 16-bit sample.
fn alaw_to_i16(a: u8) -> i16 {
    let a = a ^ 0x55;
    let exp = (a >> 4) & 7;
    let mant = (a & 0xf) as i32;
    let mut s = (mant << 4) + 8;
    if exp > 0 {
        s = (s + 0x100) << (exp - 1);
    }
    // A-law's sign bit, unlike µ-law's, marks the POSITIVE half.
    if a & 0x80 != 0 {
        s as i16
    } else {
        -s as i16
    }
}

/// One sample of `format` (exactly `sample_bytes(format)` bytes at `b`) as
/// the S16LE value the ring carries.
fn decode_sample(format: i32, b: &[u8]) -> i16 {
    match format {
        AFMT_MU_LAW => mulaw_to_i16(b[0]),
        AFMT_A_LAW => alaw_to_i16(b[0]),
        AFMT_U8 => ((b[0] as i16) - 128) << 8,
        AFMT_S8 => (b[0] as i8 as i16) << 8,
        AFMT_S16_LE => i16::from_le_bytes([b[0], b[1]]),
        AFMT_S16_BE => i16::from_be_bytes([b[0], b[1]]),
        AFMT_U16_LE => (u16::from_le_bytes([b[0], b[1]]) ^ 0x8000) as i16,
        AFMT_U16_BE => (u16::from_be_bytes([b[0], b[1]]) ^ 0x8000) as i16,
        // 24 significant bits, little-endian: the top two bytes are the
        // sample; the fourth byte of the 32-bit container is padding.
        AFMT_S24_PACKED | AFMT_S24_LE => i16::from_le_bytes([b[1], b[2]]),
        // Big-endian 24-in-32: [pad, msb, mid, lsb].
        AFMT_S24_BE => i16::from_be_bytes([b[1], b[2]]),
        AFMT_S32_LE => i16::from_le_bytes([b[2], b[3]]),
        AFMT_S32_BE => i16::from_be_bytes([b[0], b[1]]),
        AFMT_FLOAT => {
            let f = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            let f = if f.is_nan() { 0.0 } else { f.clamp(-1.0, 1.0) };
            (f * 32767.0) as i16
        }
        _ => 0,
    }
}

/// Convert `frames` client frames from `src` into S16LE stereo at the end of
/// `dst`. `src` holds at least `frames * client_frame_bytes` bytes.
fn convert_frames(format: i32, channels: usize, src: &[u8], frames: usize, dst: &mut Vec<u8>) {
    let sb = sample_bytes(format).unwrap_or(2);
    let cf = sb * channels;
    dst.reserve(frames * HW_FRAME);
    for i in 0..frames {
        let f = &src[i * cf..(i + 1) * cf];
        let l = decode_sample(format, &f[..sb]);
        let r = if channels >= 2 {
            decode_sample(format, &f[sb..2 * sb])
        } else {
            l
        };
        dst.extend_from_slice(&l.to_le_bytes());
        dst.extend_from_slice(&r.to_le_bytes());
    }
}

// ── Per-open runtime (Linux `snd_pcm_oss_runtime`) ──────────────────────────

/// The client-visible parameters. Changing one marks the runtime dirty; the
/// hardware is reconfigured lazily at the next `make_ready`, as on Linux.
#[derive(Clone, Copy)]
struct OssParams {
    format: i32,
    channels: usize,
    rate: u32,
    /// `SNDCTL_DSP_SETFRAGMENT`: log2 of the fragment size (0 = unset) and
    /// the maximum fragment count.
    fragshift: u32,
    maxfrags: usize,
    /// `SNDCTL_DSP_SUBDIVIDE` (0 = unset).
    subdivision: usize,
}

struct OssRuntime {
    params: OssParams,
    /// Linux `oss.params`: the hardware has not been configured for `params`
    /// yet.
    params_dirty: bool,
    /// Linux `oss.prepare`: the stream needs a fresh prepare (ring reset)
    /// before the next data, without a reconfigure.
    prepare: bool,
    /// The rate the device took, once configured.
    hw_rate: u32,
    /// Fragment geometry in CLIENT bytes.
    period_bytes: usize,
    periods: usize,
    /// `PCM_ENABLE_OUTPUT`.
    trigger: bool,
    /// `SNDCTL_DSP_NONBLOCK`: like `O_NONBLOCK`, but set through the node
    /// (the file's flags are not reachable from here).
    nonblock: bool,
    /// Bytes of an incomplete client frame carried over between writes.
    partial: [u8; 16],
    partial_len: usize,
    /// Client bytes accepted since open (Linux `oss.bytes`).
    bytes: u64,
    /// Whole fragments reported played by the last `GETOPTR`.
    optr_blocks: u64,
}

impl OssRuntime {
    fn new(defaults: OssParams) -> Self {
        OssRuntime {
            params: defaults,
            params_dirty: true,
            prepare: true,
            hw_rate: defaults.rate,
            period_bytes: 0,
            periods: 0,
            trigger: true,
            nonblock: false,
            partial: [0; 16],
            partial_len: 0,
            bytes: 0,
            optr_blocks: 0,
        }
    }

    fn client_frame(&self) -> usize {
        sample_bytes(self.params.format).unwrap_or(2) * self.params.channels
    }

    fn buffer_bytes(&self) -> usize {
        self.period_bytes * self.periods
    }

    /// Client bytes for `hw` ring bytes (whole frames).
    fn client_bytes(&self, hw: usize) -> usize {
        hw / HW_FRAME * self.client_frame()
    }

    /// Ring bytes for `client` client bytes (whole frames).
    fn hw_bytes(&self, client: usize) -> usize {
        client / self.client_frame() * HW_FRAME
    }
}

/// The per-node defaults: what a fresh open starts with.
#[derive(Clone, Copy)]
pub enum OssDefaults {
    /// `/dev/dsp`: 48 kHz S16LE stereo (see the module comment).
    Dsp,
    /// `/dev/audio`: the Sun conventions, µ-law 8 kHz mono.
    Audio,
}

impl OssDefaults {
    fn params(self) -> OssParams {
        let (format, channels, rate) = match self {
            OssDefaults::Dsp => (AFMT_S16_LE, 2, 48_000),
            OssDefaults::Audio => (AFMT_MU_LAW, 1, 8_000),
        };
        OssParams {
            format,
            channels,
            rate,
            fragshift: 0,
            maxfrags: 0,
            subdivision: 0,
        }
    }
}

pub struct DspDev {
    audio: Arc<dyn AudioScheme>,
    index: usize,
    inode_id: usize,
    defaults: OssDefaults,
    /// Shared with `/dev/snd/pcmC<index>D0p`: one writer per device ring.
    opened: AudioClaim,
    /// Set on the per-open handle [`open_client`](DspDev::open_client) hands
    /// out; the registry node that [`new`](DspDev::new) built owns nothing.
    release_opened_on_drop: bool,
    rt: Mutex<OssRuntime>,
}

impl DspDev {
    /// An OSS node with a claim of its own — exclusive against other opens of
    /// itself only. `/dev/dsp` proper is built with [`DspDev::with_claim`] so
    /// it is also exclusive against the native PCM on the same card.
    pub fn new(audio: Arc<dyn AudioScheme>, index: usize) -> Self {
        Self::with_claim(audio, index, new_audio_claim())
    }

    /// An OSS node sharing `opened` with the other front ends onto the same
    /// device (see [`AudioClaim`]).
    pub fn with_claim(audio: Arc<dyn AudioScheme>, index: usize, opened: AudioClaim) -> Self {
        Self::with_defaults(audio, index, opened, OssDefaults::Dsp)
    }

    /// As [`with_claim`](DspDev::with_claim), with the node's default
    /// parameters chosen (`/dev/audio` is the µ-law variant).
    pub fn with_defaults(
        audio: Arc<dyn AudioScheme>,
        index: usize,
        opened: AudioClaim,
        defaults: OssDefaults,
    ) -> Self {
        DspDev {
            audio,
            index,
            inode_id: DevFS::new_inode_id(),
            defaults,
            opened,
            release_opened_on_drop: false,
            rt: Mutex::new(OssRuntime::new(defaults.params())),
        }
    }

    /// `open(2)` on `/dev/dsp<N>`: take the device or fail with `EBUSY`.
    ///
    /// The OSS node and the native PCM are two front ends onto ONE hardware
    /// ring with no mixing, so the second one has to be refused — Linux
    /// refuses it too. The refusal is also what fixes a bare `mpg123
    /// file.mp3`: libout123 walks its built-in driver list and takes the
    /// first module that loads AND opens, so an `EBUSY` here sends it on to
    /// the ALSA module, `/etc/asound.conf`, the pulse plugin and a daemon
    /// that mixes. Answering the open instead put a second writer into
    /// PulseAudio's ring, and OSS playback came out silent.
    ///
    /// Every open starts from the node's defaults: the runtime (format,
    /// fragments, trigger) belongs to the fd, as on Linux.
    pub fn open_client(&self) -> Result<Arc<dyn INode>> {
        if self
            .opened
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(FsError::Busy);
        }
        Ok(Arc::new(DspDev {
            audio: self.audio.clone(),
            index: self.index,
            inode_id: self.inode_id,
            defaults: self.defaults,
            opened: self.opened.clone(),
            release_opened_on_drop: true,
            rt: Mutex::new(OssRuntime::new(self.defaults.params())),
        }))
    }

    /// Seconds it takes the device to drain `hw_bytes` at the current rate,
    /// rounded up, plus one — used to bound waits without cutting audio off.
    fn drain_secs(&self, hw_bytes: usize) -> u64 {
        let (rate, channels) = self.audio.params();
        let bps = (rate as u64) * (channels as u64) * 2;
        (hw_bytes as u64).div_ceil(bps.max(1)) + 1
    }

    fn backoff() {
        kernel_hal::deferred_job::drain_deferred_jobs();
        let resume = kernel_hal::timer::timer_now() + RETRY_BACKOFF;
        while kernel_hal::timer::timer_now() < resume {
            core::hint::spin_loop();
        }
    }

    // ── Geometry (Linux `snd_pcm_oss_period_size`) ──────────────────────

    /// Choose the fragment geometry for `params` against the ring, the way
    /// Linux derives its OSS period from the slave's buffer: the buffer is
    /// the largest power of two the ring holds, the period is that halved
    /// until it is under a second of audio and then subdivided, or exactly
    /// what `SETFRAGMENT` asked for, capped at half the buffer. Returns
    /// `(period_bytes, periods)` in client bytes.
    fn geometry(&self, params: &OssParams, hw_rate: u32) -> (usize, usize) {
        let cf = sample_bytes(params.format).unwrap_or(2) * params.channels;
        let ring_frames = self.audio.buffer_bytes() / HW_FRAME;
        let mut buffer = ring_frames * cf;
        buffer = if buffer == 0 {
            0
        } else {
            1 << (usize::BITS - 1 - buffer.leading_zeros())
        };
        let min_period = (MIN_PERIOD_FRAMES * cf).next_power_of_two();
        let max_period = (buffer / 2).max(min_period);

        let mut period;
        if params.fragshift != 0 {
            period = 1usize << params.fragshift.min(24);
            period = period.min(max_period);
        } else {
            let bytes_per_sec = hw_rate as usize * cf;
            period = buffer;
            loop {
                period /= 2;
                if period <= bytes_per_sec || period <= min_period {
                    break;
                }
            }
            let sd = if params.subdivision == 0 {
                let mut sd = 4;
                if period / sd > 4096 {
                    sd *= 2;
                }
                if period / sd < 4096 {
                    sd = 1;
                }
                sd
            } else {
                params.subdivision
            };
            period /= sd;
        }
        period = period.max(min_period).min(max_period).max(16);
        // A whole number of client frames per fragment, or GETBLKSIZE would
        // hand out a size no write can honour exactly.
        period = period / cf * cf;

        let mut periods = (buffer / period).max(2);
        if params.maxfrags != 0 {
            periods = periods.min(params.maxfrags.max(2));
        }
        while periods > 2 && period * periods / cf < MIN_BUFFER_FRAMES {
            periods += 1;
        }
        let buffer_frames = period * periods / cf;
        if buffer_frames > ring_frames {
            periods = (ring_frames * cf / period).max(2);
        }
        (period, periods)
    }

    /// Linux `snd_pcm_oss_make_ready`: apply pending parameter changes to the
    /// hardware, then prepare the stream if it needs it. Every data path and
    /// every geometry query goes through here first.
    fn make_ready(&self, rt: &mut OssRuntime) -> Result<()> {
        if rt.params_dirty {
            let (hw_rate, _) = self
                .audio
                .set_params(rt.params.rate, 2)
                .map_err(|_| FsError::DeviceError)?;
            rt.hw_rate = hw_rate;
            let (period, periods) = self.geometry(&rt.params, hw_rate);
            rt.period_bytes = period;
            rt.periods = periods;
            rt.params_dirty = false;
            // `set_params` wiped the ring: that is the prepare.
            rt.prepare = false;
            rt.partial_len = 0;
            info!(
                "[dsp{}] configured: fmt {:#x} {}ch {} Hz (device {} Hz), fragment {} B x {}",
                self.index,
                rt.params.format,
                rt.params.channels,
                rt.params.rate,
                hw_rate,
                period,
                periods
            );
        } else if rt.prepare {
            self.audio.reset().map_err(|_| FsError::DeviceError)?;
            rt.prepare = false;
            rt.partial_len = 0;
        }
        // A stream held by `SETTRIGGER` stays held across a prepare.
        self.audio
            .set_start_hold(!rt.trigger)
            .map_err(|_| FsError::DeviceError)?;
        Ok(())
    }

    /// Client bytes queued in the device and not yet played, partial frame
    /// included (Linux `snd_pcm_oss_get_odelay`).
    fn odelay(&self, rt: &OssRuntime) -> usize {
        // Everything still to play before the last written byte, driver
        // silence included: `snd_pcm_oss_get_odelay` adds the hardware's
        // own delay to the queue for the same reason.
        rt.client_bytes(self.audio.delay_bytes()) + rt.partial_len
    }

    /// Client bytes of whole frames a write would take right now without
    /// blocking: room in the OSS buffer, and never more than the device ring
    /// has.
    fn room(&self, rt: &OssRuntime) -> usize {
        let queued = self.audio.queued_bytes();
        let ring_free = self.audio.free_bytes();
        let hw_room = rt
            .hw_bytes(rt.buffer_bytes())
            .saturating_sub(queued)
            .min(ring_free);
        rt.client_bytes(hw_room)
    }

    // ── Data path ───────────────────────────────────────────────────────

    /// `write(2)`. `nonblock` is the file's `O_NONBLOCK`; the runtime's own
    /// `SNDCTL_DSP_NONBLOCK` is OR'd in.
    ///
    /// Whole client frames go to the device; a trailing partial frame is
    /// kept and completed by the next write (Linux buffers up to a period,
    /// so a byte-granular writer never sees a short count for it). The
    /// return value counts every byte taken, stashed ones included.
    pub fn write_pcm(&self, buf: &[u8], nonblock: bool) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut rt = self.rt.lock();
        self.make_ready(&mut rt)?;
        let nonblock = nonblock || rt.nonblock;
        let cf = rt.client_frame();
        let format = rt.params.format;
        let channels = rt.params.channels;
        let passthrough = format == AFMT_S16_LE && channels == 2;

        // Stitch the carried-over partial frame onto the front of this write.
        let mut head = [0u8; 16];
        let mut need = 0; // bytes of `buf` that complete the stashed frame
        if rt.partial_len > 0 {
            need = cf - rt.partial_len;
            if buf.len() < need {
                let at = rt.partial_len;
                rt.partial[at..at + buf.len()].copy_from_slice(buf);
                rt.partial_len += buf.len();
                rt.bytes += buf.len() as u64;
                return Ok(buf.len());
            }
            head[..rt.partial_len].copy_from_slice(&rt.partial[..rt.partial_len]);
            head[rt.partial_len..cf].copy_from_slice(&buf[..need]);
        }
        let body = &buf[need..];
        let body_frames = body.len() / cf;
        let tail = &body[body_frames * cf..];

        let mut hw = Vec::new();
        let mut head_done = need == 0;
        let mut frames_done = 0usize; // whole body frames handed to the device
        let deadline_step = core::time::Duration::from_secs(
            self.drain_secs(rt.hw_bytes(rt.buffer_bytes()).max(HW_FRAME)),
        );
        let mut deadline = kernel_hal::timer::timer_now() + deadline_step;
        loop {
            let (src, want) = if !head_done {
                (&head[..cf], 1)
            } else {
                (&body[frames_done * cf..], body_frames - frames_done)
            };
            if want == 0 {
                break;
            }
            let take = want.min(self.room(&rt) / cf);
            let accepted = if take == 0 {
                0
            } else if passthrough {
                self.audio
                    .write(&src[..take * cf])
                    .map_err(|_| FsError::DeviceError)?
                    / HW_FRAME
            } else {
                hw.clear();
                convert_frames(format, channels, src, take, &mut hw);
                self.audio.write(&hw).map_err(|_| FsError::DeviceError)? / HW_FRAME
            };
            if accepted > 0 {
                if head_done {
                    frames_done += accepted;
                } else {
                    head_done = true;
                    rt.partial_len = 0;
                }
                super::snd::arm_playback_watchdog();
                deadline = kernel_hal::timer::timer_now() + deadline_step;
                continue;
            }
            if nonblock {
                break;
            }
            // Buffer full: the device frees space at the PCM byte rate. The
            // synchronous INode contract leaves no waker to park on, so
            // retry with a backoff, bounded so a wedged stream cannot hang
            // the writer forever. The lock is released meanwhile so another
            // thread's `GETODELAY` is not held up behind a blocked write.
            if kernel_hal::timer::timer_now() >= deadline {
                warn!(
                    "[dsp{}] playback ring made no progress; giving up",
                    self.index
                );
                break;
            }
            drop(rt);
            Self::backoff();
            rt = self.rt.lock();
        }

        let mut consumed = if head_done { need } else { 0 } + frames_done * cf;
        if head_done && frames_done == body_frames && !tail.is_empty() {
            rt.partial[..tail.len()].copy_from_slice(tail);
            rt.partial_len = tail.len();
            consumed += tail.len();
        }
        rt.bytes += consumed as u64;
        if consumed == 0 {
            return Err(if nonblock {
                FsError::Again
            } else {
                FsError::DeviceError
            });
        }
        Ok(consumed)
    }

    /// `SNDCTL_DSP_SYNC`: play out everything queued, then leave the stream
    /// stopped so the next write starts it afresh (Linux sets `oss.prepare`).
    fn sync(&self, rt: &mut OssRuntime) -> Result<()> {
        self.make_ready(rt)?;
        rt.partial_len = 0;
        if rt.trigger && self.audio.is_playing() {
            let deadline = kernel_hal::timer::timer_now()
                + core::time::Duration::from_secs(self.drain_secs(self.audio.queued_bytes()));
            while self.audio.queued_bytes() > 0 && self.audio.is_playing() {
                if kernel_hal::timer::timer_now() >= deadline {
                    break;
                }
                Self::backoff();
            }
        }
        let _ = self.audio.reset();
        rt.prepare = true;
        Ok(())
    }

    /// `SNDCTL_DSP_RESET`: stop at once and drop what was queued.
    fn reset(&self, rt: &mut OssRuntime) {
        let _ = self.audio.reset();
        rt.prepare = true;
        rt.partial_len = 0;
        rt.optr_blocks = 0;
    }

    fn set_trigger(&self, rt: &mut OssRuntime, bits: i32) -> Result<()> {
        self.make_ready(rt)?;
        let enable = bits & PCM_ENABLE_OUTPUT != 0;
        if enable == rt.trigger {
            return Ok(());
        }
        rt.trigger = enable;
        if enable {
            self.audio
                .set_start_hold(false)
                .map_err(|_| FsError::DeviceError)?;
            super::snd::arm_playback_watchdog();
        } else {
            // Linux DROPs the stream when output is disabled: queued data is
            // gone, and what is written from now on waits for the enable.
            let _ = self.audio.reset();
            rt.partial_len = 0;
            self.audio
                .set_start_hold(true)
                .map_err(|_| FsError::DeviceError)?;
        }
        Ok(())
    }

    fn ospace(&self, rt: &mut OssRuntime) -> Result<AudioBufInfo> {
        self.make_ready(rt)?;
        // Linux: the free bytes less what sits in its partial-period buffer.
        let bytes = self.room(rt).saturating_sub(rt.partial_len);
        Ok(AudioBufInfo {
            fragments: (bytes / rt.period_bytes.max(1)) as i32,
            fragstotal: rt.periods as i32,
            fragsize: rt.period_bytes as i32,
            bytes: bytes as i32,
        })
    }

    fn optr(&self, rt: &mut OssRuntime) -> Result<CountInfo> {
        self.make_ready(rt)?;
        let played = rt.bytes.saturating_sub(self.odelay(rt) as u64);
        let blocks = played / rt.period_bytes.max(1) as u64;
        let delta = blocks.saturating_sub(rt.optr_blocks);
        rt.optr_blocks = blocks;
        Ok(CountInfo {
            bytes: (played & i32::MAX as u64) as i32,
            blocks: delta.min(i32::MAX as u64) as i32,
            ptr: (played % rt.buffer_bytes().max(1) as u64) as i32,
        })
    }
}

impl Drop for DspDev {
    fn drop(&mut self) {
        if !self.release_opened_on_drop {
            return;
        }
        // Close does NOT reset the stream: OSS `close(2)` drains by default
        // (`SNDCTL_DSP_SYNC` is the explicit form) and the ring plays out on
        // its own at the PCM byte rate. Dropping it here would cut the tail
        // off a `cat music.raw > /dev/dsp`. Releasing the claim is enough —
        // the next opener, PCM or OSS, resets on PREPARE / SETFMT anyway.
        // A start hold does not outlive the fd that set it, though: a held
        // ring is by definition not playing, and the next client must not
        // inherit it.
        let _ = self.audio.set_start_hold(false);
        self.opened.store(false, Ordering::Release);
    }
}

impl INode for DspDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        // Playback only: Linux answers ENXIO on a stream the fd does not have.
        Err(FsError::NoSuchDeviceOrAddress)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        // The file layer calls `write_pcm` with the fd's O_NONBLOCK; this is
        // the plain INode path, blocking.
        self.write_pcm(buf, false)
    }

    fn poll(&self) -> Result<PollStatus> {
        // Linux `snd_pcm_oss_poll`: writable while the stream is not running
        // (a write would start it) or once a whole fragment fits.
        let rt = self.rt.lock();
        let write = if rt.params_dirty || rt.prepare || rt.period_bytes == 0 {
            true
        } else {
            !self.audio.is_playing() || self.room(&rt) >= rt.period_bytes
        };
        Ok(PollStatus {
            read: false,
            write,
            error: false,
            hangup: false,
        })
    }

    #[allow(unsafe_code)]
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        let mut rt = self.rt.lock();
        match cmd {
            OSS_GETVERSION => uwrite::<i32>(data, SNDRV_OSS_VERSION)?,
            SNDCTL_DSP_RESET => self.reset(&mut rt),
            SNDCTL_DSP_SYNC => self.sync(&mut rt)?,
            SNDCTL_DSP_POST => {
                // "Start what is buffered": nothing to flush at frame
                // granularity, but a stopped stream with data gets kicked.
                self.make_ready(&mut rt)?;
                super::snd::arm_playback_watchdog();
            }
            SNDCTL_DSP_SPEED => {
                let req = uread::<i32>(data)?;
                // Linux clamps to 1000..=192000 and reports the rate in
                // effect; here that is the nearest one the stream format
                // encodes.
                let req = req.clamp(1000, 192_000) as u32;
                if req != rt.params.rate {
                    rt.params.rate = req;
                    rt.params_dirty = true;
                }
                self.make_ready(&mut rt)?;
                uwrite::<i32>(data, rt.hw_rate as i32)?;
            }
            SOUND_PCM_READ_RATE => {
                self.make_ready(&mut rt)?;
                uwrite::<i32>(data, rt.hw_rate as i32)?;
            }
            SNDCTL_DSP_SETFMT => {
                let req = uread::<i32>(data)?;
                if req != AFMT_QUERY {
                    // An unsupported format is answered with U8, as on Linux.
                    let fmt = if SUPPORTED_FORMATS & req == req {
                        req
                    } else {
                        AFMT_U8
                    };
                    if fmt != rt.params.format {
                        rt.params.format = fmt;
                        rt.params_dirty = true;
                    }
                }
                self.make_ready(&mut rt)?;
                uwrite::<i32>(data, rt.params.format)?;
            }
            SOUND_PCM_READ_BITS => {
                self.make_ready(&mut rt)?;
                uwrite::<i32>(data, rt.params.format)?;
            }
            SNDCTL_DSP_GETFMTS => uwrite::<i32>(data, SUPPORTED_FORMATS)?,
            SNDCTL_DSP_CHANNELS | SNDCTL_DSP_STEREO => {
                let req = uread::<i32>(data)?;
                let channels = if cmd == SNDCTL_DSP_STEREO {
                    if req > 0 {
                        2
                    } else {
                        1
                    }
                } else {
                    if req > 128 {
                        return Err(FsError::InvalidParam);
                    }
                    // The ring is stereo; anything wider is downmixed to it.
                    req.clamp(1, 2) as usize
                };
                if channels != rt.params.channels {
                    rt.params.channels = channels;
                    rt.params_dirty = true;
                }
                self.make_ready(&mut rt)?;
                let ch = rt.params.channels as i32;
                uwrite::<i32>(data, if cmd == SNDCTL_DSP_STEREO { ch - 1 } else { ch })?;
            }
            SOUND_PCM_READ_CHANNELS => {
                self.make_ready(&mut rt)?;
                uwrite::<i32>(data, rt.params.channels as i32)?;
            }
            SOUND_PCM_WRITE_FILTER | SOUND_PCM_READ_FILTER => return Err(FsError::DeviceError),
            SNDCTL_DSP_GETBLKSIZE => {
                self.make_ready(&mut rt)?;
                uwrite::<i32>(data, rt.period_bytes as i32)?;
            }
            SNDCTL_DSP_SETFRAGMENT => {
                let val = uread::<i32>(data)? as u32;
                // Once per open, and not after SUBDIVIDE (Linux).
                if rt.params.subdivision != 0 || rt.params.fragshift != 0 {
                    return Err(FsError::InvalidParam);
                }
                let fragshift = val & 0xffff;
                if fragshift >= 25 {
                    return Err(FsError::InvalidParam);
                }
                rt.params.fragshift = fragshift.max(4);
                rt.params.maxfrags = ((val >> 16) & 0xffff).max(2) as usize;
                rt.params_dirty = true;
            }
            SNDCTL_DSP_SUBDIVIDE => {
                let val = uread::<i32>(data)?;
                if val == 0 {
                    uwrite::<i32>(data, rt.params.subdivision.max(1) as i32)?;
                } else {
                    if rt.params.subdivision != 0 || rt.params.fragshift != 0 {
                        return Err(FsError::InvalidParam);
                    }
                    if val != 1 && val != 2 && val != 4 {
                        return Err(FsError::InvalidParam);
                    }
                    rt.params.subdivision = val as usize;
                    rt.params_dirty = true;
                    uwrite::<i32>(data, val)?;
                }
            }
            SNDCTL_DSP_GETOSPACE => {
                let info = self.ospace(&mut rt)?;
                uwrite::<AudioBufInfo>(data, info)?;
            }
            SNDCTL_DSP_GETISPACE | SNDCTL_DSP_GETIPTR => return Err(FsError::InvalidParam),
            SNDCTL_DSP_GETOPTR => {
                let info = self.optr(&mut rt)?;
                uwrite::<CountInfo>(data, info)?;
            }
            SNDCTL_DSP_GETODELAY => {
                self.make_ready(&mut rt)?;
                let delay = self.odelay(&rt);
                uwrite::<i32>(data, delay.min(i32::MAX as usize) as i32)?;
            }
            SNDCTL_DSP_NONBLOCK => rt.nonblock = true,
            SNDCTL_DSP_GETCAPS => {
                // Revision 1, real-time position, triggers. No capture, so no
                // duplex; no mmap.
                let caps = (DSP_CAP_REVISION & 1) | DSP_CAP_REALTIME | DSP_CAP_TRIGGER;
                uwrite::<i32>(data, caps)?;
            }
            SNDCTL_DSP_GETTRIGGER => {
                uwrite::<i32>(data, if rt.trigger { PCM_ENABLE_OUTPUT } else { 0 })?;
            }
            SNDCTL_DSP_SETTRIGGER => {
                let bits = uread::<i32>(data)?;
                self.set_trigger(&mut rt, bits)?;
            }
            SNDCTL_DSP_SETSYNCRO | SNDCTL_DSP_PROFILE => {}
            SNDCTL_DSP_SETDUPLEX => return Err(FsError::DeviceError),
            _ if (cmd >> 8) & 0xff == b'P' as u32
                && matches!(cmd & 0xff, SNDCTL_DSP_MAPINBUF_NR | SNDCTL_DSP_MAPOUTBUF_NR) =>
            {
                return Err(FsError::InvalidParam);
            }
            _ => {
                debug!("[dsp{}] unknown ioctl {:#x}", self.index, cmd);
                return Err(FsError::InvalidParam);
            }
        }
        Ok(0)
    }

    fn metadata(&self) -> Result<Metadata> {
        let minor = match self.defaults {
            // OSS numbering: /dev/dsp is (14, 3), /dev/dsp1 is (14, 19), …
            OssDefaults::Dsp => 3,
            // … and /dev/audio is (14, 4), /dev/audio1 (14, 20).
            OssDefaults::Audio => 4,
        };
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o666,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(14, minor + self.index * 16),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcore_drivers::DeviceResult;

    /// A device ring the size of the HDA one (64 KiB less its guard) that
    /// records what reaches it.
    struct FakeAudio {
        cap: usize,
        st: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        data: Vec<u8>,
        queued: usize,
        running: bool,
        hold: bool,
        rate: u32,
        params_calls: usize,
        resets: usize,
    }

    impl FakeAudio {
        fn new(cap: usize) -> Self {
            FakeAudio {
                cap,
                st: Mutex::new(FakeState {
                    rate: 48_000,
                    ..Default::default()
                }),
            }
        }

        /// Play out `n` bytes.
        fn drain(&self, n: usize) {
            let mut st = self.st.lock();
            st.queued = st.queued.saturating_sub(n);
            if st.queued == 0 {
                st.running = false;
            }
        }
    }

    impl zcore_drivers::scheme::Scheme for FakeAudio {
        fn name(&self) -> &str {
            "fake-audio"
        }
    }

    impl AudioScheme for FakeAudio {
        fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
            let mut st = self.st.lock();
            st.params_calls += 1;
            st.rate = rate;
            st.queued = 0;
            st.running = false;
            st.data.clear();
            Ok((rate, channels))
        }
        fn params(&self) -> (u32, u8) {
            (self.st.lock().rate, 2)
        }
        fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
            let mut st = self.st.lock();
            let n = self.cap.saturating_sub(st.queued).min(pcm.len()) / 4 * 4;
            st.data.extend_from_slice(&pcm[..n]);
            st.queued += n;
            if n > 0 && !st.hold {
                st.running = true;
            }
            Ok(n)
        }
        fn free_bytes(&self) -> usize {
            self.cap.saturating_sub(self.st.lock().queued)
        }
        fn buffer_bytes(&self) -> usize {
            self.cap
        }
        fn queued_bytes(&self) -> usize {
            self.st.lock().queued
        }
        fn is_playing(&self) -> bool {
            self.st.lock().running
        }
        fn reset(&self) -> DeviceResult {
            let mut st = self.st.lock();
            st.resets += 1;
            st.queued = 0;
            st.running = false;
            st.data.clear();
            Ok(())
        }
        fn set_start_hold(&self, hold: bool) -> DeviceResult {
            let mut st = self.st.lock();
            st.hold = hold;
            if !hold && st.queued > 0 {
                st.running = true;
            }
            Ok(())
        }
    }

    const RING: usize = 65536 - 16384;

    fn dsp() -> (Arc<FakeAudio>, Arc<dyn INode>) {
        let audio = Arc::new(FakeAudio::new(RING));
        let node = DspDev::new(audio.clone(), 0);
        (audio, node.open_client().unwrap())
    }

    fn as_dsp(node: &Arc<dyn INode>) -> &DspDev {
        node.downcast_ref::<DspDev>().unwrap()
    }

    fn ioctl_int(node: &Arc<dyn INode>, cmd: u32, val: i32) -> Result<i32> {
        let mut v = val;
        node.io_control(cmd, &mut v as *mut i32 as usize)?;
        Ok(v)
    }

    fn ospace(node: &Arc<dyn INode>) -> AudioBufInfo {
        let mut info = AudioBufInfo {
            fragments: 0,
            fragstotal: 0,
            fragsize: 0,
            bytes: 0,
        };
        node.io_control(
            SNDCTL_DSP_GETOSPACE,
            &mut info as *mut AudioBufInfo as usize,
        )
        .unwrap();
        info
    }

    fn optr(node: &Arc<dyn INode>) -> CountInfo {
        let mut info = CountInfo {
            bytes: 0,
            blocks: 0,
            ptr: 0,
        };
        node.io_control(SNDCTL_DSP_GETOPTR, &mut info as *mut CountInfo as usize)
            .unwrap();
        info
    }

    #[test]
    fn defaults_geometry_and_caps_are_what_linux_reports() {
        let (_, node) = dsp();
        assert_eq!(ioctl_int(&node, OSS_GETVERSION, 0).unwrap(), 0x0003_0832);
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_GETFMTS, 0).unwrap(),
            SUPPORTED_FORMATS
        );
        let caps = ioctl_int(&node, SNDCTL_DSP_GETCAPS, 0).unwrap();
        assert_eq!(caps, 1 | DSP_CAP_REALTIME | DSP_CAP_TRIGGER);
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_GETTRIGGER, 0).unwrap(),
            PCM_ENABLE_OUTPUT
        );
        // 48 kHz S16LE stereo: the ring's 49152 B round down to a 32 KiB
        // buffer, halved once under a second of audio then split in four.
        assert_eq!(ioctl_int(&node, SOUND_PCM_READ_RATE, 0).unwrap(), 48_000);
        assert_eq!(
            ioctl_int(&node, SOUND_PCM_READ_BITS, 0).unwrap(),
            AFMT_S16_LE
        );
        assert_eq!(ioctl_int(&node, SOUND_PCM_READ_CHANNELS, 0).unwrap(), 2);
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETBLKSIZE, 0).unwrap(), 4096);
        let sp = ospace(&node);
        assert_eq!(
            (sp.fragsize, sp.fragstotal, sp.fragments, sp.bytes),
            (4096, 8, 8, 32768)
        );
    }

    #[test]
    fn the_sun_node_defaults_to_mulaw_8k_mono() {
        let audio = Arc::new(FakeAudio::new(RING));
        let node = DspDev::with_defaults(audio.clone(), 0, new_audio_claim(), OssDefaults::Audio)
            .open_client()
            .unwrap();
        assert_eq!(
            ioctl_int(&node, SOUND_PCM_READ_BITS, 0).unwrap(),
            AFMT_MU_LAW
        );
        assert_eq!(ioctl_int(&node, SOUND_PCM_READ_CHANNELS, 0).unwrap(), 1);
        assert_eq!(ioctl_int(&node, SOUND_PCM_READ_RATE, 0).unwrap(), 8_000);
        assert_eq!(node.metadata().unwrap().rdev, make_rdev(14, 4));
        // µ-law 0xff is +0, 0x7f is -0: both decode to silence.
        assert_eq!(as_dsp(&node).write_pcm(&[0xff, 0x7f], false).unwrap(), 2);
        assert_eq!(audio.st.lock().data, alloc::vec![0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn every_supported_format_lands_as_s16le_stereo() {
        let cases: &[(i32, &[u8], i16)] = &[
            (AFMT_U8, &[0x80], 0),
            (AFMT_U8, &[0xff], 0x7f00),
            (AFMT_S8, &[0x80], -32768),
            (AFMT_S16_LE, &[0x34, 0x12], 0x1234),
            (AFMT_S16_BE, &[0x12, 0x34], 0x1234),
            (AFMT_U16_LE, &[0x00, 0x80], 0),
            (AFMT_U16_BE, &[0xff, 0xff], 0x7fff),
            (AFMT_S24_PACKED, &[0x56, 0x34, 0x12], 0x1234),
            (AFMT_S24_LE, &[0x56, 0x34, 0x12, 0x00], 0x1234),
            (AFMT_S24_BE, &[0x00, 0x12, 0x34, 0x56], 0x1234),
            (AFMT_S32_LE, &[0x78, 0x56, 0x34, 0x12], 0x1234),
            (AFMT_S32_BE, &[0x12, 0x34, 0x56, 0x78], 0x1234),
            (AFMT_FLOAT, &1.0f32.to_le_bytes(), 32767),
            (AFMT_FLOAT, &(-2.0f32).to_le_bytes(), -32767),
            // G.711: 0x00 is the loudest negative µ-law code, 0xd5 the
            // A-law silence byte (+8).
            (AFMT_MU_LAW, &[0x00], -32124),
            (AFMT_MU_LAW, &[0x80], 32124),
            (AFMT_A_LAW, &[0xd5], 8),
            (AFMT_A_LAW, &[0x55], -8),
            (AFMT_A_LAW, &[0xaa], 32256),
        ];
        for &(fmt, bytes, want) in cases {
            assert_eq!(decode_sample(fmt, bytes), want, "format {:#x}", fmt);
        }
        // A mono frame is duplicated onto both channels.
        let mut out = Vec::new();
        convert_frames(AFMT_U8, 1, &[0xc0], 1, &mut out);
        assert_eq!(out, alloc::vec![0x00, 0x40, 0x00, 0x40]);
    }

    #[test]
    fn setfmt_and_channels_convert_on_the_way_in() {
        let (audio, node) = dsp();
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_SETFMT, AFMT_U8).unwrap(),
            AFMT_U8
        );
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_CHANNELS, 1).unwrap(), 1);
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_SPEED, 22_050).unwrap(), 22_050);
        assert_eq!(audio.st.lock().rate, 22_050);
        // Each configuring ioctl reprogrammed the device, as on Linux.
        assert_eq!(audio.st.lock().params_calls, 3);
        // Four U8 mono samples become four S16LE stereo frames.
        assert_eq!(
            as_dsp(&node)
                .write_pcm(&[0x80, 0xc0, 0x40, 0xff], false)
                .unwrap(),
            4
        );
        let data = audio.st.lock().data.clone();
        assert_eq!(
            data,
            alloc::vec![0, 0, 0, 0, 0, 0x40, 0, 0x40, 0, 0xc0, 0, 0xc0, 0, 0x7f, 0, 0x7f]
        );
        // Delay and position are reported in the CLIENT's bytes.
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETODELAY, 0).unwrap(), 4);
        audio.drain(8);
        let p = optr(&node);
        assert_eq!((p.bytes, p.ptr), (2, 2));
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETODELAY, 0).unwrap(), 2);
    }

    #[test]
    fn an_unknown_format_is_answered_with_u8_and_query_reads_back() {
        let (_, node) = dsp();
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_SETFMT, 0x200).unwrap(), AFMT_U8);
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_SETFMT, AFMT_QUERY).unwrap(),
            AFMT_U8
        );
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_SETFMT, AFMT_S16_BE).unwrap(),
            AFMT_S16_BE
        );
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_SETFMT, AFMT_QUERY).unwrap(),
            AFMT_S16_BE
        );
    }

    #[test]
    fn stereo_ioctl_sets_channels_and_answers_channels_minus_one() {
        let (_, node) = dsp();
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_STEREO, 0).unwrap(), 0);
        assert_eq!(ioctl_int(&node, SOUND_PCM_READ_CHANNELS, 0).unwrap(), 1);
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_STEREO, 1).unwrap(), 1);
        assert_eq!(ioctl_int(&node, SOUND_PCM_READ_CHANNELS, 0).unwrap(), 2);
        // Wider than the ring is clamped to it; absurd counts are refused.
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_CHANNELS, 6).unwrap(), 2);
        assert!(matches!(
            ioctl_int(&node, SNDCTL_DSP_CHANNELS, 129),
            Err(FsError::InvalidParam)
        ));
    }

    #[test]
    fn setfragment_bounds_the_buffer_and_nonblocking_writes_honour_it() {
        let (audio, node) = dsp();
        // Two fragments of 4 KiB, SDL's usual request.
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_SETFRAGMENT, (2 << 16) | 12).unwrap(),
            (2 << 16) | 12
        );
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETBLKSIZE, 0).unwrap(), 4096);
        let sp = ospace(&node);
        assert_eq!(
            (sp.fragsize, sp.fragstotal, sp.fragments, sp.bytes),
            (4096, 2, 2, 8192)
        );
        // Only once per open.
        assert!(matches!(
            ioctl_int(&node, SNDCTL_DSP_SETFRAGMENT, (4 << 16) | 10),
            Err(FsError::InvalidParam)
        ));
        let pcm = alloc::vec![0u8; 12000];
        assert_eq!(as_dsp(&node).write_pcm(&pcm, true).unwrap(), 8192);
        assert_eq!(audio.queued_bytes(), 8192);
        assert!(matches!(
            as_dsp(&node).write_pcm(&pcm, true),
            Err(FsError::Again)
        ));
        let sp = ospace(&node);
        assert_eq!((sp.fragments, sp.bytes), (0, 0));
        assert!(!node.poll().unwrap().write);
        // A fragment played out is a fragment writable.
        audio.drain(4096);
        assert!(node.poll().unwrap().write);
        assert_eq!(ospace(&node).fragments, 1);
        assert_eq!(as_dsp(&node).write_pcm(&pcm, true).unwrap(), 4096);
        // GETOPTR counts whole fragments played since the last call.
        let p = optr(&node);
        assert_eq!((p.bytes, p.blocks, p.ptr), (4096, 1, 4096));
        audio.drain(8192);
        let p = optr(&node);
        assert_eq!((p.bytes, p.blocks, p.ptr), (12288, 2, 4096));
        assert_eq!(optr(&node).blocks, 0);
    }

    #[test]
    fn a_partial_frame_is_carried_over_to_the_next_write() {
        let (audio, node) = dsp();
        let d = as_dsp(&node);
        assert_eq!(d.write_pcm(&[1, 2, 3], false).unwrap(), 3);
        assert_eq!(audio.queued_bytes(), 0);
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETODELAY, 0).unwrap(), 3);
        assert_eq!(d.write_pcm(&[4, 5], false).unwrap(), 2);
        assert_eq!(audio.st.lock().data, alloc::vec![1, 2, 3, 4]);
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETODELAY, 0).unwrap(), 5);
        assert_eq!(ospace(&node).bytes, 32768 - 4 - 1);
        assert_eq!(d.write_pcm(&[6, 7, 8, 9, 10, 11], false).unwrap(), 6);
        assert_eq!(audio.st.lock().data, alloc::vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETODELAY, 0).unwrap(), 11);
    }

    #[test]
    fn nonblock_ioctl_makes_a_full_buffer_eagain_instead_of_a_wait() {
        let (_, node) = dsp();
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_SETFRAGMENT, (2 << 16) | 12).unwrap(),
            (2 << 16) | 12
        );
        ioctl_int(&node, SNDCTL_DSP_NONBLOCK, 0).unwrap();
        let pcm = alloc::vec![0u8; 8192];
        assert_eq!(node.write_at(0, &pcm).unwrap(), 8192);
        assert!(matches!(node.write_at(0, &pcm), Err(FsError::Again)));
    }

    #[test]
    fn a_cleared_trigger_drops_and_holds_and_a_set_one_starts() {
        let (audio, node) = dsp();
        let d = as_dsp(&node);
        let pcm = alloc::vec![0u8; 4096];
        assert_eq!(d.write_pcm(&pcm, false).unwrap(), 4096);
        assert!(audio.is_playing());
        // Disabling output DROPs, as on Linux, and holds the stream.
        ioctl_int(&node, SNDCTL_DSP_SETTRIGGER, 0).unwrap();
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_GETTRIGGER, 0).unwrap(), 0);
        assert_eq!(audio.queued_bytes(), 0);
        assert!(audio.st.lock().hold);
        // Writes fill the buffer without playing…
        assert_eq!(d.write_pcm(&pcm, false).unwrap(), 4096);
        assert_eq!(d.write_pcm(&pcm, false).unwrap(), 4096);
        assert!(!audio.is_playing());
        assert_eq!(audio.queued_bytes(), 8192);
        // …a held stream still polls writable (a write would not block)…
        assert!(node.poll().unwrap().write);
        // …and the enable starts it with everything primed.
        ioctl_int(&node, SNDCTL_DSP_SETTRIGGER, PCM_ENABLE_OUTPUT).unwrap();
        assert_eq!(
            ioctl_int(&node, SNDCTL_DSP_GETTRIGGER, 0).unwrap(),
            PCM_ENABLE_OUTPUT
        );
        assert!(audio.is_playing());
        assert!(!audio.st.lock().hold);
        assert_eq!(audio.queued_bytes(), 8192);
    }

    #[test]
    fn a_hold_does_not_outlive_the_open() {
        let (audio, node) = dsp();
        ioctl_int(&node, SNDCTL_DSP_SETTRIGGER, 0).unwrap();
        assert!(audio.st.lock().hold);
        drop(node);
        assert!(!audio.st.lock().hold);
    }

    #[test]
    fn sync_and_reset_leave_the_stream_to_be_prepared_again() {
        let (audio, node) = dsp();
        let d = as_dsp(&node);
        let pcm = alloc::vec![0u8; 4096];
        assert_eq!(d.write_pcm(&pcm, false).unwrap(), 4096);
        let before = audio.st.lock().resets;
        audio.drain(4096);
        ioctl_int(&node, SNDCTL_DSP_SYNC, 0).unwrap();
        assert_eq!(audio.st.lock().resets, before + 1);
        assert!(d.rt.lock().prepare);
        // The next write prepares (a reset) without reprogramming.
        let params = audio.st.lock().params_calls;
        assert_eq!(d.write_pcm(&pcm, false).unwrap(), 4096);
        assert_eq!(audio.st.lock().resets, before + 2);
        assert_eq!(audio.st.lock().params_calls, params);
        ioctl_int(&node, SNDCTL_DSP_RESET, 0).unwrap();
        assert_eq!(audio.queued_bytes(), 0);
        assert!(d.rt.lock().prepare);
    }

    #[test]
    fn the_rest_of_the_table_answers_like_linux() {
        let (_, node) = dsp();
        assert!(matches!(
            ioctl_int(&node, SNDCTL_DSP_SETDUPLEX, 0),
            Err(FsError::DeviceError)
        ));
        assert!(matches!(
            ioctl_int(&node, SNDCTL_DSP_GETISPACE, 0),
            Err(FsError::InvalidParam)
        ));
        assert!(matches!(
            ioctl_int(&node, 0x8010_5014, 0),
            Err(FsError::InvalidParam)
        ));
        assert!(matches!(
            ioctl_int(&node, SOUND_PCM_READ_FILTER, 0),
            Err(FsError::DeviceError)
        ));
        // TCGETS from isatty(): EINVAL, not ENOSYS.
        assert!(matches!(
            ioctl_int(&node, 0x5401, 0),
            Err(FsError::InvalidParam)
        ));
        ioctl_int(&node, SNDCTL_DSP_SETSYNCRO, 0).unwrap();
        ioctl_int(&node, SNDCTL_DSP_PROFILE, 0).unwrap();
        ioctl_int(&node, SNDCTL_DSP_POST, 0).unwrap();
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_SUBDIVIDE, 0).unwrap(), 1);
        assert_eq!(ioctl_int(&node, SNDCTL_DSP_SUBDIVIDE, 2).unwrap(), 2);
        assert!(matches!(
            ioctl_int(&node, SNDCTL_DSP_SUBDIVIDE, 3),
            Err(FsError::InvalidParam)
        ));
        assert!(matches!(
            ioctl_int(&node, SNDCTL_DSP_SETFRAGMENT, (2 << 16) | 12),
            Err(FsError::InvalidParam)
        ));
        let mut b = [0u8; 4];
        assert!(matches!(
            node.read_at(0, &mut b),
            Err(FsError::NoSuchDeviceOrAddress)
        ));
    }

    #[test]
    fn the_second_opener_is_refused_until_the_first_closes() {
        let audio = Arc::new(FakeAudio::new(RING));
        let node = DspDev::new(audio, 0);
        let first = node.open_client().unwrap();
        assert!(matches!(node.open_client(), Err(FsError::Busy)));
        drop(first);
        assert!(node.open_client().is_ok());
    }
}
