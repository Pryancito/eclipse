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
const STATE_PAUSED: i32 = 6;

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
}

pub struct PcmDev {
    audio: Arc<dyn AudioScheme>,
    card: usize,
    inode_id: usize,
    st: Mutex<PcmState>,
}

impl PcmDev {
    pub fn new(audio: Arc<dyn AudioScheme>, card: usize) -> Self {
        PcmDev {
            audio,
            card,
            inode_id: DevFS::new_inode_id(),
            st: Mutex::new(PcmState {
                state: STATE_OPEN,
                rate: 48000,
                buffer_size: 16384,
                period_size: 1024,
                boundary: 0x4000_0000_0000_0000,
                appl_ptr: 0,
                avail_min: 1024,
            }),
        }
    }

    fn ring_frames(&self) -> u64 {
        self.audio.buffer_bytes() as u64 / BYTES_PER_FRAME
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

    /// What the ALSA timer bound to this stream samples: whether the stream
    /// runs, how many whole periods the hardware pointer has passed, and the
    /// period length in ns (the timer's resolution).
    pub(crate) fn period_clock(&self) -> PeriodClock {
        // Lock order everywhere in this file is `st` then the audio driver
        // (`hw_ptr` -> `queued_frames` refreshes the hardware position);
        // nothing takes them the other way round.
        let st = self.st.lock();
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
        let ring = self.ring_frames();

        // Fixed points of this path: S16LE stereo.
        changed |= Self::iv_clamp(&mut iv[Self::IV_SAMPLE_BITS], 16, 16);
        changed |= Self::iv_clamp(&mut iv[Self::IV_CHANNELS], 2, 2);
        changed |= Self::iv_clamp(&mut iv[Self::IV_FRAME_BITS], 32, 32);

        // Rate: snap to the discrete set the HDA stream format encodes.
        {
            let r = &mut iv[Self::IV_RATE];
            let lo = RATES.iter().copied().find(|&x| x >= r.min);
            let hi = RATES.iter().rev().copied().find(|&x| x <= r.max);
            match (lo, hi) {
                (Some(lo), Some(hi)) if lo <= hi => {
                    changed |= Self::iv_clamp(r, lo as u64, hi as u64);
                }
                _ => {
                    r.flags |= INTERVAL_EMPTY;
                }
            }
        }

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
        let ring = self.ring_frames();
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
            st.state = STATE_SETUP;
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
        st.state = STATE_RUNNING;
        arm_playback_watchdog();
    }

    /// Interleaved write. Blocking callers keep the old spin-retry behaviour;
    /// nonblocking callers get Linux-style "write what fits right now, else
    /// EAGAIN" semantics so alsa-lib/PulseAudio can return to poll().
    fn writei(&self, xfer: &mut SndXferI, flags: OpenFlags) -> Result<()> {
        {
            let st = self.st.lock();
            if st.state != STATE_PREPARED && st.state != STATE_RUNNING {
                return Err(FsError::InvalidParam);
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
                return Err(FsError::Again);
            }
            let buf = unsafe { core::slice::from_raw_parts(src, chunk) };
            let n = self.audio.write(buf).map_err(|_| FsError::DeviceError)?;
            if n == 0 {
                return Err(FsError::Again);
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
            core::hint::spin_loop();
        }
        xfer.result = (done as u64 / BYTES_PER_FRAME) as i64;
        Ok(())
    }

    fn drain(&self) -> Result<()> {
        let rate = self.st.lock().rate.max(1) as u64;
        let deadline = kernel_hal::timer::timer_now()
            + core::time::Duration::from_secs(
                (self.audio.queued_bytes() as u64)
                    .div_ceil(rate * BYTES_PER_FRAME)
                    .saturating_add(2)
                    .min(10),
            );
        while self.audio.queued_bytes() > 0 {
            if kernel_hal::timer::timer_now() >= deadline {
                break;
            }
            kernel_hal::deferred_job::drain_deferred_jobs();
            core::hint::spin_loop();
        }
        // Drain done (or timed out): wipe the ring so leftover PCM cannot
        // loop. PREPARE/DROP also reset; this covers PulseAudio's end-of-stream
        // path that only DRAIN'd.
        let _ = self.audio.reset();
        self.st.lock().state = STATE_SETUP;
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
        let st = self.st.lock();
        s.state = st.state;
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
                // HW_PARAMS
                ucheck::<SndPcmHwParams>(data)?;
                let p = unsafe { &mut *(data as *mut SndPcmHwParams) };
                self.install(p)?;
                Ok(0)
            }
            0x12 => {
                // HW_FREE
                let _ = self.audio.reset();
                self.st.lock().state = STATE_OPEN;
                Ok(0)
            }
            0x13 => {
                // SW_PARAMS
                ucheck::<SndPcmSwParams>(data)?;
                let p = unsafe { &mut *(data as *mut SndPcmSwParams) };
                let mut st = self.st.lock();
                if p.avail_min > 0 {
                    st.avail_min = p.avail_min;
                }
                if p.boundary > 0 {
                    st.boundary = p.boundary;
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
                unsafe { *(data as *mut i64) = self.queued_frames() as i64 };
                Ok(0)
            }
            0x22 => {
                let _ = self.queued_frames();
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
                // PREPARE
                let _ = self.audio.reset();
                let mut st = self.st.lock();
                st.appl_ptr = 0;
                st.state = STATE_PREPARED;
                Ok(0)
            }
            0x41 => {
                // RESET
                let _ = self.audio.reset();
                let mut st = self.st.lock();
                st.appl_ptr = 0;
                Ok(0)
            }
            0x42 => {
                // START — the driver starts the stream on first data; just
                // reflect the state change.
                self.st.lock().state = STATE_RUNNING;
                Ok(0)
            }
            0x43 => {
                // DROP
                let _ = self.audio.reset();
                self.st.lock().state = STATE_SETUP;
                Ok(0)
            }
            0x44 => self.drain().map(|_| 0),
            0x45 => {
                // PAUSE: arg 1 = pause, 0 = resume. Pulse suspend-on-idle and
                // stream cork both land here; a no-op left DMA looping the
                // last buffer (mpg123 remnants).
                ucheck::<i32>(data)?;
                let enable = unsafe { *(data as *const i32) };
                let state = self.st.lock().state;
                if enable != 0 {
                    if state == STATE_RUNNING || state == STATE_PREPARED {
                        let _ = self.audio.pause();
                        // Idle cork with nothing left to play: wipe remnants so
                        // a later RUN cannot loop the last mpg123 period.
                        if self.audio.queued_bytes() == 0 {
                            let _ = self.audio.reset();
                            self.st.lock().state = STATE_SETUP;
                        } else {
                            self.st.lock().state = STATE_PAUSED;
                        }
                    }
                } else if state == STATE_PAUSED {
                    let _ = self.audio.resume();
                    self.st.lock().state = STATE_RUNNING;
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
            0x48 => Ok(0), // XRUN
            0x49 => {
                // FORWARD — skip unplayed frames from the playhead.
                ucheck::<u64>(data)?;
                let frames = unsafe { *(data as *mut u64) };
                let bytes = (frames * BYTES_PER_FRAME) as usize;
                let skipped = self.audio.forward(bytes).unwrap_or(0);
                unsafe { *(data as *mut u64) = skipped as u64 / BYTES_PER_FRAME };
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

impl INode for PcmDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        let _ = self.audio.queued_bytes();
        let st = self.st.lock();
        let avail = self.avail(&st);
        // Linux reports POLLOUT when `avail >= avail_min`, but it only
        // re-evaluates that on a period interrupt, so a feeder wakes at most
        // once per period however small its avail_min (PulseAudio's tsched=0
        // sink asks for 1). This fd has no interrupt: sys_poll re-scans it
        // every 4 ms, and reporting every freed frame would wake such a
        // feeder 250 times a second to write a few frames each. Gating on a
        // whole period gives it the wake-up cadence it was written for.
        Ok(PollStatus {
            read: false,
            write: avail >= st.avail_min.max(st.period_size),
            error: false,
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
    mod pcm_tests {
        use super::*;
        use zcore_drivers::DeviceResult;

        struct FakeAudio {
            cap: usize,
            queued: Mutex<usize>,
        }

        impl FakeAudio {
            fn new(cap: usize) -> Self {
                Self {
                    cap,
                    queued: Mutex::new(0),
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
                Ok((rate, channels))
            }

            fn params(&self) -> (u32, u8) {
                (48_000, 2)
            }

            fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
                let mut queued = self.queued.lock();
                let room = self.cap.saturating_sub(*queued);
                let n = room.min(pcm.len());
                *queued += n;
                Ok(n)
            }

            fn free_bytes(&self) -> usize {
                self.cap.saturating_sub(*self.queued.lock())
            }

            fn buffer_bytes(&self) -> usize {
                self.cap
            }

            fn queued_bytes(&self) -> usize {
                *self.queued.lock()
            }

            fn is_playing(&self) -> bool {
                self.queued_bytes() > 0
            }

            fn reset(&self) -> DeviceResult {
                *self.queued.lock() = 0;
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
