//! `/dev/snd/controlC0` and `/dev/snd/pcmC0D0p` as Linux's ALSA answers
//! them, over the card's one output stream ([`crate::stream`]): what
//! `linux-object/src/fs/devfs/snd.rs` does in the kernel, for a driver that
//! serves the nodes ([`crate::served`]).
//!
//! It is for alsa-lib's `hw` plugin, as the kernel's is: RW-interleaved
//! S16LE stereo at the rates the controller plays, the data through
//! `WRITEI_FRAMES` (whose structure holds the address of the samples: the
//! node asks the kernel for them, [`Device::fetch`]), and the pointers
//! through `SYNC_PTR`, since the status and control pages cannot be mapped.
//! The stream's states follow `pcm_native.c`, as the kernel's do: the
//! refine, the thresholds, the underrun, the drain.
//!
//! What waits does not wait here: a write with no room and a drain with
//! sound still to play answer `EAGAIN`, and a program whose open waits is
//! asked again by the kernel once the node says something of the open,
//! which [`Pcm::tick`] does when the room it waits for is there, or the
//! drain is done.
//!
//! No capture, no mmap of the samples, no timer: the kernel's
//! `/dev/snd/timer` is not served.

use crate::hda::{nearest_rate, FRAME};
use crate::served::{
    Device, Iov, Node, EAGAIN, EBUSY, EFAULT, EINVAL, ENOTTY, POLLERR, POLLOUT, SLOT_LEN,
};
use crate::stream::{Output, Owner, Stream, CAPACITY};

pub const EPIPE: i32 = 32;
pub const EBADFD: i32 = 77;
pub const ENOENT: i32 = 2;
pub const ENXIO: i32 = 6;

/// 2.0.4, as the kernel's: the `hw_params` of 2.0.2 and later, without the
/// paths alsa-lib takes for 2.0.12 and on.
const SNDRV_PCM_VERSION: i32 = 0x0002_0004;
const SNDRV_CTL_VERSION: i32 = 0x0002_0007;

pub const STATE_OPEN: i32 = 0;
pub const STATE_SETUP: i32 = 1;
pub const STATE_PREPARED: i32 = 2;
pub const STATE_RUNNING: i32 = 3;
pub const STATE_XRUN: i32 = 4;
pub const STATE_DRAINING: i32 = 5;
pub const STATE_PAUSED: i32 = 6;
const TSTAMP_MODE_LAST: i32 = 1;
const TSTAMP_TYPE_LAST: u32 = 3;

pub const PAR_ACCESS: usize = 0;
pub const PAR_FORMAT: usize = 1;
pub const PAR_SUBFORMAT: usize = 2;
pub const ACCESS_RW_INTERLEAVED: u32 = 3;
pub const FORMAT_S16_LE: u32 = 2;
const SUBFORMAT_STD: u32 = 0;
const INFO_PAUSE: u32 = 0x0000_0020;
const INFO_INTERLEAVED: u32 = 0x0000_0100;
const INFO_BLOCK_TRANSFER: u32 = 0x0001_0000;

const SYNC_PTR_APPL: u32 = 2;
const SYNC_PTR_AVAIL_MIN: u32 = 4;

pub const INTERVAL_OPENMIN: u32 = 1 << 0;
pub const INTERVAL_OPENMAX: u32 = 1 << 1;
pub const INTERVAL_INTEGER: u32 = 1 << 2;
pub const INTERVAL_EMPTY: u32 = 1 << 3;

/// The intervals of `snd_pcm_hw_params`, by their place in `intervals`
/// (the parameter's number less 8, the first interval's).
pub const IV_SAMPLE_BITS: usize = 0;
pub const IV_FRAME_BITS: usize = 1;
pub const IV_CHANNELS: usize = 2;
pub const IV_RATE: usize = 3;
pub const IV_PERIOD_TIME: usize = 4;
pub const IV_PERIOD_SIZE: usize = 5;
pub const IV_PERIOD_BYTES: usize = 6;
pub const IV_PERIODS: usize = 7;
pub const IV_BUFFER_TIME: usize = 8;
pub const IV_BUFFER_SIZE: usize = 9;
pub const IV_BUFFER_BYTES: usize = 10;
pub const IV_TICK_TIME: usize = 11;

/// The rates the controller plays.
pub const RATES: [u32; 11] = [
    8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
];

/// S16LE stereo.
const BYTES_PER_FRAME: u64 = FRAME as u64;
/// The ring, in frames: the most a buffer can be.
pub const RING_FRAMES: u64 = (CAPACITY / FRAME) as u64;
/// How long a stream that does not drain is waited for before a write
/// with no room is an underrun, at least.
const STALL_FLOOR_NS: u64 = 200_000_000;

// ── uapi, LP64 (x86_64, aarch64 and riscv64 alike) ──────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SndInterval {
    pub min: u32,
    pub max: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SndMask {
    pub bits: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmHwParams {
    pub flags: u32,
    pub masks: [SndMask; 3],
    pub mres: [SndMask; 5],
    pub intervals: [SndInterval; 12],
    pub ires: [SndInterval; 9],
    pub rmask: u32,
    pub cmask: u32,
    pub info: u32,
    pub msbits: u32,
    pub rate_num: u32,
    pub rate_den: u32,
    pub fifo_size: u64,
    pub reserved: [u8; 64],
}

const _: () = assert!(core::mem::size_of::<SndPcmHwParams>() == 608);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmSwParams {
    pub tstamp_mode: i32,
    pub period_step: u32,
    pub sleep_min: u32,
    pub avail_min: u64,
    pub xfer_align: u64,
    pub start_threshold: u64,
    pub stop_threshold: u64,
    pub silence_threshold: u64,
    pub silence_size: u64,
    pub boundary: u64,
    pub proto: u32,
    pub tstamp_type: u32,
    pub reserved: [u8; 56],
}

const _: () = assert!(core::mem::size_of::<SndPcmSwParams>() == 136);

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmStatus {
    pub state: i32,
    _pad0: i32,
    pub trigger_tstamp: Timespec,
    pub tstamp: Timespec,
    pub appl_ptr: u64,
    pub hw_ptr: u64,
    pub delay: i64,
    pub avail: u64,
    pub avail_max: u64,
    pub overrange: u64,
    pub suspended_state: i32,
    pub audio_tstamp_data: u32,
    pub audio_tstamp: Timespec,
    pub driver_tstamp: Timespec,
    pub audio_tstamp_accuracy: u32,
    pub reserved: [u8; 20],
}

const _: () = assert!(core::mem::size_of::<SndPcmStatus>() == 152);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmMmapStatus {
    pub state: i32,
    _pad1: i32,
    pub hw_ptr: u64,
    pub tstamp: Timespec,
    pub suspended_state: i32,
    _pad2: i32,
    pub audio_tstamp: Timespec,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmMmapControl {
    pub appl_ptr: u64,
    pub avail_min: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmSyncPtr {
    pub flags: u32,
    _pad: u32,
    pub status: SndPcmMmapStatus,
    _spad: [u8; 64 - core::mem::size_of::<SndPcmMmapStatus>()],
    pub control: SndPcmMmapControl,
    _cpad: [u8; 64 - core::mem::size_of::<SndPcmMmapControl>()],
}

const _: () = assert!(core::mem::size_of::<SndPcmSyncPtr>() == 136);

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SndXferI {
    pub result: i64,
    pub buf: u64,
    pub frames: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmChannelInfo {
    pub channel: u32,
    _pad: u32,
    pub offset: i64,
    pub first: u32,
    pub step: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmInfo {
    pub device: u32,
    pub subdevice: u32,
    pub stream: i32,
    pub card: i32,
    pub id: [u8; 64],
    pub name: [u8; 80],
    pub subname: [u8; 32],
    pub dev_class: i32,
    pub dev_subclass: i32,
    pub subdevices_count: u32,
    pub subdevices_avail: u32,
    pub sync: [u8; 16],
    pub reserved: [u8; 64],
}

const _: () = assert!(core::mem::size_of::<SndPcmInfo>() == 288);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndCtlCardInfo {
    pub card: i32,
    _pad: i32,
    pub id: [u8; 16],
    pub driver: [u8; 16],
    pub name: [u8; 32],
    pub longname: [u8; 80],
    reserved_: [u8; 16],
    pub mixername: [u8; 80],
    pub components: [u8; 128],
}

const _: () = assert!(core::mem::size_of::<SndCtlCardInfo>() == 376);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndCtlElemId {
    pub numid: u32,
    pub iface: i32,
    pub device: u32,
    pub subdevice: u32,
    pub name: [u8; 44],
    pub index: u32,
}

const _: () = assert!(core::mem::size_of::<SndCtlElemId>() == 64);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndCtlElemList {
    pub offset: u32,
    pub space: u32,
    pub used: u32,
    pub count: u32,
    pub pids: u64,
    _reserved: [u8; 50],
}

const _: () = assert!(core::mem::size_of::<SndCtlElemList>() == 80);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndCtlElemInfo {
    pub id: SndCtlElemId,
    pub type_: i32,
    pub access: u32,
    pub count: u32,
    pub owner: i32,
    pub min: i64,
    pub max: i64,
    pub step: i64,
    _value_pad: [u8; 128 - 24],
    _dimen_reserved: [u8; 64],
}

const _: () = assert!(core::mem::size_of::<SndCtlElemInfo>() == 272);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndCtlElemValue {
    pub id: SndCtlElemId,
    _indirect: u32,
    _pad: u32,
    pub values: [i64; 2],
}

/// A structure of the uapi out of the bytes an `ioctl` carried: `EFAULT`
/// when they are fewer.
pub fn get<T: Copy>(data: &[u8]) -> Result<T, i32> {
    if data.len() < core::mem::size_of::<T>() {
        return Err(EFAULT);
    }
    // SAFETY: at least `size_of::<T>()` bytes, read unaligned; every `T`
    // here is plain integers, any bits of which are a value.
    Ok(unsafe { core::ptr::read_unaligned(data.as_ptr() as *const T) })
}

/// And back: how many bytes that was.
pub fn put<T: Copy>(data: &mut [u8], v: &T) -> Result<usize, i32> {
    let n = core::mem::size_of::<T>();
    if data.len() < n {
        return Err(EFAULT);
    }
    // SAFETY: as in `get`, written.
    unsafe { core::ptr::write_unaligned(data.as_mut_ptr() as *mut T, *v) };
    Ok(n)
}

/// A structure of zeros.
pub fn zeroed<T: Copy>() -> T {
    // SAFETY: plain integers, for which zeros are a value.
    unsafe { core::mem::zeroed() }
}

fn fill_cstr(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    dst[n..].fill(0);
}

fn timespec(ns: u64) -> Timespec {
    Timespec {
        sec: (ns / 1_000_000_000) as i64,
        nsec: (ns % 1_000_000_000) as i64,
    }
}

// ── hw_params refine ────────────────────────────────────────────────────────
//
// The kernel's, relation for relation (see its comments for why each bound
// floors or ceils as it does): alsa-lib narrows one parameter at a time and
// expects the rest to follow, so the relations of an S16LE stereo path are
// propagated both ways until nothing changes.
//
//   frame_bits = sample_bits × channels, period_bytes = period_size × 4,
//   buffer_bytes = buffer_size × 4, buffer_size ≈ period_size × periods,
//   period_time = period_size × 1e6 / rate, and the buffer's alike.

pub fn iv_normalize(iv: &mut SndInterval) {
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

pub fn iv_empty(iv: &SndInterval) -> bool {
    iv.flags & INTERVAL_EMPTY != 0 || iv.min > iv.max
}

/// Narrow `iv` to `[lo, hi]`: whether it changed.
pub fn iv_clamp(iv: &mut SndInterval, lo: u64, hi: u64) -> bool {
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

/// One sweep of the relations: whether anything narrowed.
pub fn propagate(iv: &mut [SndInterval; 12], ring: u64) -> bool {
    let mut changed = false;
    changed |= iv_clamp(&mut iv[IV_SAMPLE_BITS], 16, 16);
    changed |= iv_clamp(&mut iv[IV_CHANNELS], 2, 2);
    changed |= iv_clamp(&mut iv[IV_FRAME_BITS], 32, 32);
    {
        let r = &mut iv[IV_RATE];
        let lo = RATES.iter().copied().find(|&x| x >= r.min);
        let hi = RATES.iter().rev().copied().find(|&x| x <= r.max);
        match (lo, hi) {
            (Some(lo), Some(hi)) if lo <= hi => {
                changed |= iv_clamp(r, lo as u64, hi as u64);
            }
            _ => r.flags |= INTERVAL_EMPTY,
        }
    }
    // The period is at most half the ring, so that two fit.
    changed |= iv_clamp(&mut iv[IV_PERIOD_SIZE], 128, (ring / 2).max(128));
    changed |= iv_clamp(&mut iv[IV_BUFFER_SIZE], 512, ring);
    changed |= iv_clamp(&mut iv[IV_PERIODS], 1, 1024);
    changed |= iv_clamp(&mut iv[IV_TICK_TIME], 0, u32::MAX as u64);

    let (ps_lo, ps_hi) = iv_bounds(&iv[IV_PERIOD_SIZE]);
    changed |= iv_clamp(&mut iv[IV_PERIOD_BYTES], ps_lo * 4, ps_hi * 4);
    let (pb_lo, pb_hi) = iv_bounds(&iv[IV_PERIOD_BYTES]);
    changed |= iv_clamp(&mut iv[IV_PERIOD_SIZE], pb_lo / 4, pb_hi.div_ceil(4));

    let (bs_lo, bs_hi) = iv_bounds(&iv[IV_BUFFER_SIZE]);
    changed |= iv_clamp(&mut iv[IV_BUFFER_BYTES], bs_lo * 4, bs_hi * 4);
    let (bb_lo, bb_hi) = iv_bounds(&iv[IV_BUFFER_BYTES]);
    changed |= iv_clamp(&mut iv[IV_BUFFER_SIZE], bb_lo / 4, bb_hi.div_ceil(4));

    //     period_size × periods  ≤  buffer_size  ≤  period_size × (periods+1) − 1
    let (ps_lo, ps_hi) = iv_bounds(&iv[IV_PERIOD_SIZE]);
    let (n_lo, n_hi) = iv_bounds(&iv[IV_PERIODS]);
    changed |= iv_clamp(
        &mut iv[IV_BUFFER_SIZE],
        ps_lo * n_lo,
        (ps_hi * (n_hi + 1)).saturating_sub(1),
    );
    let (bs_lo, bs_hi) = iv_bounds(&iv[IV_BUFFER_SIZE]);
    if let Some(ps_max) = bs_hi.checked_div(n_lo) {
        changed |= iv_clamp(
            &mut iv[IV_PERIOD_SIZE],
            (bs_lo + 1).div_ceil(n_hi + 1),
            ps_max,
        );
    }
    let (ps_lo, ps_hi) = iv_bounds(&iv[IV_PERIOD_SIZE]);
    if ps_lo > 0 && ps_hi > 0 {
        changed |= iv_clamp(
            &mut iv[IV_PERIODS],
            (bs_lo + 1).div_ceil(ps_hi).saturating_sub(1).max(1),
            bs_hi / ps_lo,
        );
    }

    // size = floor(time × rate / 1e6): a size of N frames owns the time
    // cell [N/rate, (N+1)/rate).
    let (r_lo, r_hi) = iv_bounds(&iv[IV_RATE]);
    let (r_lo, r_hi) = (r_lo.max(1), r_hi.max(1));
    for (size_idx, time_idx) in [
        (IV_PERIOD_SIZE, IV_PERIOD_TIME),
        (IV_BUFFER_SIZE, IV_BUFFER_TIME),
    ] {
        let (s_lo, s_hi) = iv_bounds(&iv[size_idx]);
        changed |= iv_clamp(
            &mut iv[time_idx],
            (s_lo * 1_000_000).div_ceil(r_hi),
            (((s_hi + 1) * 1_000_000).div_ceil(r_lo)).saturating_sub(1),
        );
        let (t_lo, t_hi) = iv_bounds(&iv[time_idx]);
        changed |= iv_clamp(
            &mut iv[size_idx],
            t_lo * r_lo / 1_000_000,
            t_hi * r_hi / 1_000_000,
        );
    }
    changed
}

/// Constrain a request to what the stream plays, the relations run to a
/// fixed point: `false` when a parameter came out empty (`EINVAL`, which
/// alsa-lib's `*_near` helpers search with).
pub fn refine(p: &mut SndPcmHwParams) -> bool {
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
        iv_normalize(iv);
    }
    for _ in 0..8 {
        if !propagate(&mut p.intervals, RING_FRAMES) {
            break;
        }
    }
    if p.intervals.iter().any(iv_empty) {
        ok = false;
    }
    p.info = INFO_PAUSE | INFO_INTERLEAVED | INFO_BLOCK_TRANSFER;
    p.msbits = 16;
    p.rate_den = 1;
    p.rate_num = 0;
    p.fifo_size = 0;
    p.cmask = 0x000f_ff07;
    ok
}

fn reopen(p: &mut SndPcmHwParams, params: &[usize]) {
    for &idx in params {
        p.intervals[idx] = SndInterval {
            min: 0,
            max: u32::MAX,
            flags: 0,
        };
    }
}

/// The first recovery: let go of what alsa-lib derives, keeping the sizes
/// the client chose.
pub fn relax_derived_params(p: &mut SndPcmHwParams) {
    reopen(
        p,
        &[
            IV_PERIOD_TIME,
            IV_PERIOD_BYTES,
            IV_PERIODS,
            IV_BUFFER_TIME,
            IV_BUFFER_BYTES,
            IV_TICK_TIME,
        ],
    );
}

/// The second: the sizes too. Format, access, channels and rate never.
pub fn relax_size_params(p: &mut SndPcmHwParams) {
    relax_derived_params(p);
    reopen(p, &[IV_PERIOD_SIZE, IV_BUFFER_SIZE]);
}

/// What a request settles on: the rate, the period, the periods and the
/// buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub rate: u32,
    pub period: u64,
    pub periods: u64,
    pub buffer: u64,
}

/// Choose the values inside a request, as Linux's `snd_pcm_hw_params`
/// finishes it, and hand every interval back as the one value: `EINVAL`
/// for a request no recovery saves.
pub fn choose(p: &mut SndPcmHwParams) -> Result<Config, i32> {
    let requested = p.intervals;
    if !refine(p) {
        // alsa-lib's `snd_pcm_hw_params_choose` goes on past its own
        // errors, and mpg123's pinned buffer and period then disagree with
        // the period time by a frame: recover, from the request each time.
        p.intervals = requested;
        relax_derived_params(p);
        if !refine(p) {
            p.intervals = requested;
            relax_size_params(p);
            if !refine(p) {
                return Err(EINVAL);
            }
        }
    }
    let rate = RATES
        .iter()
        .copied()
        .find(|&r| r >= p.intervals[IV_RATE].min && r <= p.intervals[IV_RATE].max)
        .unwrap_or(48_000);
    iv_clamp(&mut p.intervals[IV_RATE], rate as u64, rate as u64);
    for _ in 0..8 {
        if !propagate(&mut p.intervals, RING_FRAMES) {
            break;
        }
    }
    let period = (p.intervals[IV_PERIOD_SIZE].min as u64).max(1);
    let ring = RING_FRAMES;
    let bs_lo = p.intervals[IV_BUFFER_SIZE].min as u64;
    let bs_hi = (p.intervals[IV_BUFFER_SIZE].max as u64).min(ring);
    let n_iv = p.intervals[IV_PERIODS];
    let mut periods = if n_iv.min == n_iv.max && n_iv.min > 0 {
        n_iv.min as u64
    } else {
        (bs_hi / period).max(1)
    };
    while periods > 1 && period * periods > bs_hi {
        periods -= 1;
    }
    while period * periods < bs_lo && period * (periods + 1) <= bs_hi.min(ring) {
        periods += 1;
    }
    let buffer = (period * periods).min(ring);
    let iv = &mut p.intervals;
    for (idx, v) in [
        (IV_SAMPLE_BITS, 16),
        (IV_FRAME_BITS, 32),
        (IV_CHANNELS, 2),
        (IV_RATE, rate as u64),
        (IV_PERIOD_SIZE, period),
        (IV_PERIOD_BYTES, period * 4),
        (IV_PERIODS, periods),
        (IV_BUFFER_SIZE, buffer),
        (IV_BUFFER_BYTES, buffer * 4),
        (IV_PERIOD_TIME, period * 1_000_000 / rate as u64),
        (IV_BUFFER_TIME, buffer * 1_000_000 / rate as u64),
    ] {
        let v = v.min(u32::MAX as u64) as u32;
        iv[idx] = SndInterval {
            min: v,
            max: v,
            flags: INTERVAL_INTEGER,
        };
    }
    iv[IV_TICK_TIME] = SndInterval {
        min: 0,
        max: 0,
        flags: INTERVAL_INTEGER,
    };
    p.rate_num = rate;
    p.rate_den = 1;
    Ok(Config {
        rate,
        period,
        periods,
        buffer,
    })
}

// ── The PCM ─────────────────────────────────────────────────────────────────

/// The PCM's state while it is open: the kernel's `PcmState`.
pub struct Pcm {
    open: Option<u32>,
    next_fh: u32,
    pub state: i32,
    pub rate: u32,
    pub buffer_size: u64,
    pub period_size: u64,
    pub boundary: u64,
    pub appl_ptr: u64,
    pub avail_min: u64,
    pub start_threshold: u64,
    pub stop_threshold: u64,
    silence_threshold: u64,
    silence_size: u64,
    tstamp_mode: i32,
    tstamp_type: u32,
    period_step: u32,
    sleep_min: u32,
    xfer_align: u64,
    proto: u32,
    trigger_tstamp: Timespec,
    /// When a write first found no room, while it still finds none.
    stalled_since: Option<u64>,
    /// When a drain gives up.
    drain_until: u64,
    /// The frames a writer whose open waits was told `EAGAIN` for: once
    /// they fit, the open is told, and the kernel asks again.
    wants: Option<u64>,
    /// A drain whose open waits was told `EAGAIN`: the open is told when
    /// it is done.
    drain_waits: bool,
    /// The readiness the open was last told.
    pub said: u64,
    clock: fn() -> u64,
}

impl Pcm {
    pub fn new(clock: fn() -> u64) -> Pcm {
        let mut p = Pcm {
            open: None,
            next_fh: 1,
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
            trigger_tstamp: Timespec::default(),
            stalled_since: None,
            drain_until: 0,
            wants: None,
            drain_waits: false,
            said: 0,
            clock,
        };
        p.fresh();
        p
    }

    /// As a new open finds it.
    fn fresh(&mut self) {
        let (open, next_fh, clock) = (self.open, self.next_fh, self.clock);
        *self = Pcm {
            open,
            next_fh,
            clock,
            ..Pcm {
                open: None,
                next_fh: 1,
                state: STATE_OPEN,
                rate: 48000,
                buffer_size: 16384.min(RING_FRAMES),
                period_size: 1024,
                boundary: 0x4000_0000_0000_0000,
                appl_ptr: 0,
                avail_min: 1024,
                start_threshold: 1,
                stop_threshold: 16384.min(RING_FRAMES),
                silence_threshold: 0,
                silence_size: 0,
                tstamp_mode: 0,
                tstamp_type: 0,
                period_step: 1,
                sleep_min: 0,
                xfer_align: 1,
                proto: 0,
                trigger_tstamp: Timespec::default(),
                stalled_since: None,
                drain_until: 0,
                wants: None,
                drain_waits: false,
                said: 0,
                clock,
            }
        };
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn queued_frames<O: Output>(&self, s: &Stream<O>) -> u64 {
        s.queued_samples() as u64 / BYTES_PER_FRAME
    }

    /// What is queued, as this stream's buffer sees it.
    fn queued_capped<O: Output>(&self, s: &Stream<O>) -> u64 {
        self.queued_frames(s).min(self.buffer_size)
    }

    /// Frames the client may write within its buffer.
    pub fn avail<O: Output>(&self, s: &Stream<O>) -> u64 {
        self.buffer_size.saturating_sub(self.queued_capped(s))
    }

    /// What a write takes now: its room in the buffer, and in the ring.
    fn writable<O: Output>(&self, s: &Stream<O>) -> u64 {
        self.avail(s).min(s.free() as u64 / BYTES_PER_FRAME)
    }

    pub fn hw_ptr<O: Output>(&self, s: &Stream<O>) -> u64 {
        (self.appl_ptr + self.boundary - self.queued_capped(s)) % self.boundary
    }

    /// Linux's underrun: a running stream whose `avail` reached its
    /// `stop_threshold` is in XRUN, unless the threshold is past the
    /// boundary (it then plays silence when starved).
    fn note_underrun<O: Output>(&mut self, s: &mut Stream<O>) {
        if self.state != STATE_RUNNING || self.stop_threshold >= self.boundary {
            return;
        }
        if self.avail(s) >= self.stop_threshold {
            self.state = STATE_XRUN;
            self.stalled_since = None;
            s.reset();
        }
    }

    fn start_running<O: Output>(&mut self, s: &mut Stream<O>) -> Result<(), i32> {
        s.start()?;
        self.state = STATE_RUNNING;
        self.stalled_since = None;
        self.trigger_tstamp = timespec(self.now());
        Ok(())
    }

    fn readiness<O: Output>(&mut self, s: &mut Stream<O>) -> u64 {
        self.note_underrun(s);
        if !matches!(
            self.state,
            STATE_RUNNING | STATE_PREPARED | STATE_PAUSED | STATE_DRAINING
        ) {
            // As Linux: a stream that cannot be waited on is writable and
            // in error, so a poller collects the `EPIPE` that tells it to
            // prepare again.
            return POLLOUT | POLLERR;
        }
        if self.writable(s) >= self.avail_min.max(self.period_size) {
            POLLOUT
        } else {
            0
        }
    }

    /// Tell the open its readiness when it changed, or when `kick`: a wait
    /// the kernel is in is over.
    fn tell<O: Output>(&mut self, s: &mut Stream<O>, node: Option<&Node>, kick: bool) {
        let now = self.readiness(s);
        if let (Some(fh), true) = (self.open, now != self.said || kick) {
            self.said = now;
            if let Some(node) = node {
                node.ready(fh, now);
            }
        }
    }

    /// The controller played on: an underrun is noticed, a drain that is
    /// done ends, and a writer waiting for room is told it is there.
    pub fn tick<O: Output>(&mut self, s: &mut Stream<O>, node: Option<&Node>) {
        s.follow();
        let mut kick = false;
        if self.state == STATE_DRAINING && !self.drain_pending(s) {
            self.drain_finish(s);
            kick |= std::mem::take(&mut self.drain_waits);
        }
        self.note_underrun(s);
        if let Some(wants) = self.wants {
            if self.writable(s) >= wants || self.state == STATE_XRUN {
                self.wants = None;
                kick = true;
            }
        }
        if self.state != STATE_DRAINING && self.drain_waits {
            self.drain_waits = false;
            kick = true;
        }
        self.tell(s, node, kick);
    }

    /// A write that found no room: `EAGAIN` while the stream drains,
    /// `EPIPE` once it plainly does not (a buffer's time with no room).
    fn no_room(&mut self) -> i32 {
        if self.state == STATE_PREPARED {
            return EAGAIN;
        }
        let now = self.now();
        let since = *self.stalled_since.get_or_insert(now);
        let buffer_ns = self.buffer_size.saturating_mul(1_000_000_000) / self.rate.max(1) as u64;
        if now.saturating_sub(since) < buffer_ns.max(STALL_FLOOR_NS) {
            return EAGAIN;
        }
        self.state = STATE_XRUN;
        self.stalled_since = None;
        EPIPE
    }

    /// The states a write is taken in: Linux's `__snd_pcm_lib_xfer`.
    fn admit<O: Output>(&mut self, s: &mut Stream<O>) -> Result<(), i32> {
        self.note_underrun(s);
        match self.state {
            STATE_XRUN => Err(EPIPE),
            STATE_PREPARED | STATE_RUNNING | STATE_PAUSED => Ok(()),
            _ => Err(EBADFD),
        }
    }

    /// How many frames of an interleaved write of `frames` go in this
    /// call: all that fit, or, on an open that waits, none until all do
    /// (or a whole buffer does, for a write longer than it).
    fn take<O: Output>(&mut self, s: &mut Stream<O>, frames: u64, waits: bool) -> Result<u64, i32> {
        let cap = ((SLOT_LEN - Iov::len_of(1, 1)) / FRAME) as u64;
        let want = frames.min(cap);
        let need = if waits {
            want.min(self.buffer_size).max(1)
        } else {
            1
        };
        let room = self.writable(s);
        if room < need {
            if waits {
                self.wants = Some(need);
            }
            return Err(self.no_room());
        }
        Ok(want.min(room))
    }

    fn commit<O: Output>(&mut self, s: &mut Stream<O>, frames: u64) -> Result<(), i32> {
        self.appl_ptr = (self.appl_ptr + frames) % self.boundary.max(1);
        self.stalled_since = None;
        if self.state == STATE_PREPARED && self.queued_capped(s) >= self.start_threshold {
            self.start_running(s)?;
        }
        Ok(())
    }

    fn drain_pending<O: Output>(&self, s: &Stream<O>) -> bool {
        self.state == STATE_DRAINING
            && s.queued_samples() > 0
            && s.running()
            && self.now() < self.drain_until
    }

    fn drain_finish<O: Output>(&mut self, s: &mut Stream<O>) {
        s.reset();
        self.state = STATE_SETUP;
        self.appl_ptr = 0;
        self.stalled_since = None;
    }

    /// `DRAIN`: done at once when nothing plays, else `EAGAIN` until what
    /// is queued has played (a waiting open is asked again then).
    fn drain<O: Output>(&mut self, s: &mut Stream<O>, waits: bool) -> Result<(i64, usize), i32> {
        if self.state == STATE_DRAINING {
            if self.drain_pending(s) {
                self.drain_waits |= waits;
                return Err(EAGAIN);
            }
            self.drain_finish(s);
            return Ok((0, 0));
        }
        if self.state == STATE_OPEN {
            return Err(EBADFD);
        }
        if self.state == STATE_PREPARED && self.queued_frames(s) > 0 {
            self.start_running(s)?;
        }
        if self.state == STATE_PAUSED || !s.running() {
            self.drain_finish(s);
            return Ok((0, 0));
        }
        self.state = STATE_DRAINING;
        let secs = (s.queued() as u64)
            .div_ceil(self.rate.max(1) as u64 * BYTES_PER_FRAME)
            .saturating_add(2)
            .min(10);
        self.drain_until = self.now() + secs * 1_000_000_000;
        self.drain_waits |= waits;
        Err(EAGAIN)
    }

    fn sync_ptr<O: Output>(&mut self, s: &mut Stream<O>, sp: &mut SndPcmSyncPtr) {
        self.note_underrun(s);
        // A flag set: hand the node's copy back; clear: take the client's.
        if sp.flags & SYNC_PTR_APPL != 0 {
            sp.control.appl_ptr = self.appl_ptr;
        } else {
            self.appl_ptr = sp.control.appl_ptr % self.boundary.max(1);
        }
        if sp.flags & SYNC_PTR_AVAIL_MIN != 0 {
            sp.control.avail_min = self.avail_min;
        } else if sp.control.avail_min > 0 {
            self.avail_min = sp.control.avail_min;
        }
        sp.status.state = self.state;
        sp.status.hw_ptr = self.hw_ptr(s);
        sp.status.suspended_state = self.state;
        sp.status.tstamp = timespec(self.now());
    }

    fn status<O: Output>(&mut self, s: &mut Stream<O>) -> SndPcmStatus {
        self.note_underrun(s);
        let mut st: SndPcmStatus = zeroed();
        st.state = self.state;
        st.trigger_tstamp = self.trigger_tstamp;
        st.appl_ptr = self.appl_ptr;
        st.hw_ptr = self.hw_ptr(s);
        st.delay = (s.queued() as u64 / BYTES_PER_FRAME) as i64;
        st.avail = self.avail(s);
        st.avail_max = self.buffer_size;
        st.tstamp = timespec(self.now());
        st
    }

    fn info(&self) -> SndPcmInfo {
        let mut info: SndPcmInfo = zeroed();
        fill_cstr(&mut info.id, "Eclipse HDA");
        fill_cstr(&mut info.name, NAME);
        fill_cstr(&mut info.subname, "subdevice #0");
        info.subdevices_count = 1;
        info.subdevices_avail = 1;
        info
    }

    fn hw_params<O: Output>(
        &mut self,
        s: &mut Stream<O>,
        p: &mut SndPcmHwParams,
    ) -> Result<(), i32> {
        if !matches!(self.state, STATE_OPEN | STATE_SETUP | STATE_PREPARED) {
            return Err(EBADFD);
        }
        let c = choose(p)?;
        // The ring is wiped, as the kernel's `set_params` wipes it.
        s.reset();
        s.set_rate(nearest_rate(c.rate));
        self.rate = s.rate;
        self.period_size = c.period;
        self.buffer_size = c.buffer;
        self.appl_ptr = 0;
        let mut boundary = c.buffer.max(1);
        while boundary < 0x4000_0000_0000_0000u64 {
            boundary *= 2;
        }
        self.boundary = boundary;
        self.avail_min = c.period;
        self.start_threshold = 1;
        self.stop_threshold = c.buffer;
        self.silence_threshold = 0;
        self.silence_size = 0;
        self.state = STATE_SETUP;
        self.stalled_since = None;
        Ok(())
    }

    fn sw_params<O: Output>(
        &mut self,
        s: &mut Stream<O>,
        p: &mut SndPcmSwParams,
    ) -> Result<(), i32> {
        if self.state == STATE_OPEN {
            return Err(EBADFD);
        }
        if p.tstamp_mode < 0 || p.tstamp_mode > TSTAMP_MODE_LAST {
            return Err(EINVAL);
        }
        if p.proto >= 0x0002_000c && p.tstamp_type > TSTAMP_TYPE_LAST {
            return Err(EINVAL);
        }
        if p.avail_min == 0 || p.stop_threshold == 0 {
            return Err(EINVAL);
        }
        if p.silence_size >= self.boundary {
            if p.silence_threshold != 0 {
                return Err(EINVAL);
            }
        } else if p.silence_size > p.silence_threshold || p.silence_threshold > self.buffer_size {
            return Err(EINVAL);
        }
        self.tstamp_mode = p.tstamp_mode;
        self.tstamp_type = p.tstamp_type;
        self.period_step = p.period_step;
        self.sleep_min = p.sleep_min;
        self.avail_min = p.avail_min;
        self.xfer_align = p.xfer_align;
        self.start_threshold = p.start_threshold;
        self.stop_threshold = p.stop_threshold;
        self.silence_threshold = p.silence_threshold;
        self.silence_size = p.silence_size;
        self.proto = p.proto;
        p.boundary = self.boundary;
        if self.state == STATE_PREPARED
            && self.queued_frames(s) > 0
            && self.queued_capped(s) >= self.start_threshold
        {
            self.start_running(s)?;
        }
        Ok(())
    }

    /// An `ioctl` of type `'A'` that carries no more than its number says.
    pub fn control<O: Output>(
        &mut self,
        s: &mut Stream<O>,
        cmd: u32,
        data: &mut [u8],
        waits: bool,
    ) -> Result<(i64, usize), i32> {
        if (cmd >> 8) & 0xff != b'A' as u32 {
            return Err(ENOTTY);
        }
        let ok = |n: usize| Ok((0, n));
        match cmd & 0xff {
            // PVERSION
            0x00 => ok(put(data, &SNDRV_PCM_VERSION)?),
            // INFO
            0x01 => ok(put(data, &self.info())?),
            // TSTAMP, TTSTAMP, USER_PVERSION: taken, and nothing changes.
            0x02..=0x04 => ok(0),
            // HW_REFINE
            0x10 => {
                let mut p: SndPcmHwParams = get(data)?;
                let fine = refine(&mut p);
                let n = put(data, &p)?;
                if fine {
                    ok(n)
                } else {
                    Err(EINVAL)
                }
            }
            // HW_PARAMS
            0x11 => {
                let mut p: SndPcmHwParams = get(data)?;
                self.hw_params(s, &mut p)?;
                ok(put(data, &p)?)
            }
            // HW_FREE
            0x12 => {
                if !matches!(self.state, STATE_SETUP | STATE_PREPARED) {
                    return Err(EBADFD);
                }
                s.reset();
                self.state = STATE_OPEN;
                self.stalled_since = None;
                ok(0)
            }
            // SW_PARAMS
            0x13 => {
                let mut p: SndPcmSwParams = get(data)?;
                self.sw_params(s, &mut p)?;
                ok(put(data, &p)?)
            }
            // STATUS, STATUS_EXT
            0x20 | 0x24 => {
                let st = self.status(s);
                ok(put(data, &st)?)
            }
            // DELAY
            0x21 => {
                self.note_underrun(s);
                if self.state == STATE_XRUN {
                    return Err(EPIPE);
                }
                ok(put(data, &((s.queued() as u64 / BYTES_PER_FRAME) as i64))?)
            }
            // HWSYNC
            0x22 => {
                self.note_underrun(s);
                if self.state == STATE_XRUN {
                    return Err(EPIPE);
                }
                ok(0)
            }
            // SYNC_PTR
            0x23 => {
                let mut sp: SndPcmSyncPtr = get(data)?;
                self.sync_ptr(s, &mut sp);
                ok(put(data, &sp)?)
            }
            // CHANNEL_INFO: interleaved, 32 bits a frame.
            0x32 => {
                let mut info: SndPcmChannelInfo = get(data)?;
                if info.channel > 1 {
                    return Err(EINVAL);
                }
                info.offset = 0;
                info.first = info.channel * 16;
                info.step = 32;
                ok(put(data, &info)?)
            }
            // PREPARE: also the way out of an underrun.
            0x40 => {
                if self.state == STATE_OPEN {
                    return Err(EBADFD);
                }
                if matches!(self.state, STATE_RUNNING | STATE_DRAINING) {
                    return Err(EBUSY);
                }
                s.reset();
                self.stalled_since = None;
                self.appl_ptr = 0;
                self.state = STATE_PREPARED;
                ok(0)
            }
            // RESET
            0x41 => {
                s.reset();
                self.appl_ptr = 0;
                self.stalled_since = None;
                if self.state == STATE_RUNNING {
                    // What played is gone; the stream runs on, on silence.
                    s.start()?;
                }
                ok(0)
            }
            // START: from PREPARED, and with something to play unless the
            // stream is free-running (Pulse's sink).
            0x42 => {
                if self.state != STATE_PREPARED {
                    return Err(EBADFD);
                }
                if self.stop_threshold < self.boundary && self.queued_frames(s) == 0 {
                    return Err(EPIPE);
                }
                self.start_running(s)?;
                ok(0)
            }
            // DROP
            0x43 => {
                if self.state == STATE_OPEN {
                    return Err(EBADFD);
                }
                s.reset();
                self.state = STATE_SETUP;
                self.stalled_since = None;
                ok(0)
            }
            // DRAIN
            0x44 => self.drain(s, waits),
            // PAUSE: 1 pauses a running stream, 0 resumes a paused one.
            0x45 => {
                let enable: i32 = get(data).unwrap_or(0);
                if enable != 0 {
                    if self.state != STATE_RUNNING {
                        return Err(EBADFD);
                    }
                    s.pause();
                    self.state = STATE_PAUSED;
                    self.stalled_since = None;
                } else {
                    if self.state != STATE_PAUSED {
                        return Err(EBADFD);
                    }
                    s.start()?;
                    self.state = STATE_RUNNING;
                }
                ok(0)
            }
            // REWIND: take back the frames written last.
            0x46 => {
                let frames: u64 = get(data)?;
                let dropped =
                    s.rewind((frames * BYTES_PER_FRAME) as usize) as u64 / BYTES_PER_FRAME;
                self.appl_ptr = (self.appl_ptr + self.boundary - dropped) % self.boundary.max(1);
                ok(put(data, &dropped)?)
            }
            // RESUME, from a suspend there never is: as an unpause.
            0x47 => {
                if self.state == STATE_PAUSED {
                    s.start()?;
                    self.state = STATE_RUNNING;
                }
                ok(0)
            }
            // XRUN: the client's own, from RUNNING only.
            0x48 => {
                if self.state != STATE_RUNNING {
                    return Err(EBADFD);
                }
                self.state = STATE_XRUN;
                self.stalled_since = None;
                s.reset();
                ok(0)
            }
            // FORWARD: silence what plays next, which still takes its time.
            0x49 => {
                let frames: u64 = get(data)?;
                let skipped =
                    s.forward((frames * BYTES_PER_FRAME) as usize) as u64 / BYTES_PER_FRAME;
                ok(put(data, &skipped)?)
            }
            // WRITEI_FRAMES of no frames; the rest go through `fetch`.
            0x50 => {
                self.admit(s)?;
                let mut x: SndXferI = get(data)?;
                x.result = 0;
                ok(put(data, &x)?)
            }
            _ => Err(ENOTTY),
        }
    }

    pub fn open<O: Output>(&mut self, s: &mut Stream<O>) -> Result<(u32, u64), i32> {
        if self.open.is_some() || s.owner.is_some() {
            return Err(EBUSY);
        }
        let fh = self.next_fh;
        self.next_fh = self.next_fh.wrapping_add(1).max(1);
        self.fresh();
        self.open = Some(fh);
        s.owner = Some(Owner::Pcm(fh));
        self.said = self.readiness(s);
        Ok((fh, self.said))
    }

    /// Closing a PCM drops it, as on Linux: what was queued goes.
    pub fn release<O: Output>(&mut self, s: &mut Stream<O>, fh: u32) {
        if self.open == Some(fh) {
            s.reset();
            self.open = None;
            s.owner = None;
            self.fresh();
        }
    }
}

/// The card's name, as the PCM and the control say it.
pub const NAME: &str = "Eclipse HDA (udrv-hda)";

/// The PCM node, over the card's stream.
pub struct PcmNode<'a, O> {
    pub pcm: &'a mut Pcm,
    pub stream: &'a mut Stream<O>,
}

/// `SNDRV_PCM_IOCTL_WRITEI_FRAMES`, by its type and number.
fn is_writei(cmd: u32) -> bool {
    cmd & 0xffff == 0x4150
}

impl<O: Output> Device for PcmNode<'_, O> {
    fn open(&mut self, _flags: u64) -> Result<(u32, u64), i32> {
        self.pcm.open(self.stream)
    }

    fn release(&mut self, fh: u32) {
        self.pcm.release(self.stream, fh)
    }

    fn read(&mut self, _: &Node, _: u32, _: u64, _: &mut [u8]) -> Result<usize, i32> {
        Err(EINVAL)
    }

    fn write(&mut self, _: &Node, _: u32, _: u64, _: &[u8]) -> Result<usize, i32> {
        Err(EINVAL)
    }

    fn ioctl(
        &mut self,
        node: &Node,
        fh: u32,
        cmd: u32,
        arg: u64,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        if self.pcm.open != Some(fh) {
            return Err(EBADFD);
        }
        let _ = arg;
        // DRAIN alone needs to know whether the open waits, and it goes
        // through `fetch` for that.
        let r = self.pcm.control(self.stream, cmd, data, false);
        self.pcm.tell(self.stream, Some(node), false);
        r
    }

    fn fetch(
        &mut self,
        fh: u32,
        cmd: u32,
        arg: u64,
        waits: bool,
        data: &[u8],
    ) -> Result<Option<Iov>, i32> {
        if self.pcm.open != Some(fh) {
            return Err(EBADFD);
        }
        if cmd & 0xffff == 0x4144 {
            // DRAIN: answered here, where it is known whether to wait.
            self.pcm.drain(self.stream, waits)?;
            // Done: an empty fetch answers it, writing nothing.
            return Ok(Some(Iov::default()));
        }
        if !is_writei(cmd) {
            return Ok(None);
        }
        let x: SndXferI = get(data)?;
        if x.frames == 0 {
            return Ok(None);
        }
        self.pcm.admit(self.stream)?;
        let take = self.pcm.take(self.stream, x.frames, waits)?;
        Ok(Some(Iov {
            ins: vec![(x.buf, take * BYTES_PER_FRAME)],
            outs: vec![(arg, 8)],
        }))
    }

    fn ioctl_iov(
        &mut self,
        node: &Node,
        fh: u32,
        cmd: u32,
        _arg: u64,
        _waits: bool,
        _iov: &Iov,
        ins: &[u8],
        out: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        if self.pcm.open != Some(fh) {
            return Err(EBADFD);
        }
        if !is_writei(cmd) {
            // DRAIN, done.
            return Ok((0, 0));
        }
        let r = (|| {
            self.pcm.admit(self.stream)?;
            let room = self.pcm.writable(self.stream);
            let frames = (ins.len() as u64 / BYTES_PER_FRAME).min(room);
            if frames == 0 {
                return Err(self.pcm.no_room());
            }
            let n = self
                .stream
                .write(&ins[..(frames * BYTES_PER_FRAME) as usize]) as u64
                / BYTES_PER_FRAME;
            self.pcm.commit(self.stream, n)?;
            out[..8].copy_from_slice(&(n as i64).to_ne_bytes());
            Ok((0, 8))
        })();
        self.pcm.tell(self.stream, Some(node), false);
        r
    }
}

// ── The control ─────────────────────────────────────────────────────────────

const IFACE_MIXER: i32 = 2;
const ELEM_BOOLEAN: i32 = 1;
const ELEM_INTEGER: i32 = 2;
const ACCESS_READ_WRITE: u32 = 0x1 | 0x2;
const NUMID_VOLUME: u32 = 1;
const NUMID_SWITCH: u32 = 2;
const NAME_VOLUME: &str = "Master Playback Volume";
const NAME_SWITCH: &str = "Master Playback Switch";
pub const MIXER_ELEMS: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Elem {
    Volume,
    Switch,
}

fn elem_id(elem: Elem) -> SndCtlElemId {
    let mut id: SndCtlElemId = zeroed();
    id.iface = IFACE_MIXER;
    let (numid, name) = match elem {
        Elem::Volume => (NUMID_VOLUME, NAME_VOLUME),
        Elem::Switch => (NUMID_SWITCH, NAME_SWITCH),
    };
    id.numid = numid;
    fill_cstr(&mut id.name, name);
    id
}

fn elem_of(id: &SndCtlElemId) -> Option<Elem> {
    match id.numid {
        NUMID_VOLUME => return Some(Elem::Volume),
        NUMID_SWITCH => return Some(Elem::Switch),
        _ => {}
    }
    let n = id
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(id.name.len());
    match core::str::from_utf8(&id.name[..n]).unwrap_or("") {
        NAME_VOLUME => Some(Elem::Volume),
        NAME_SWITCH => Some(Elem::Switch),
        _ => None,
    }
}

fn elem_at(index: u32) -> Option<Elem> {
    match index {
        0 => Some(Elem::Volume),
        1 => Some(Elem::Switch),
        _ => None,
    }
}

/// The control node: its opens, any number of them.
#[derive(Default)]
pub struct Ctl {
    next_fh: u32,
}

impl Ctl {
    /// An `ioctl` of type `'U'`.
    pub fn control<O: Output>(
        &mut self,
        s: &mut Stream<O>,
        cmd: u32,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        if (cmd >> 8) & 0xff != b'U' as u32 {
            return Err(ENOTTY);
        }
        let ok = |n: usize| Ok((0, n));
        match cmd & 0xff {
            0x00 => ok(put(data, &SNDRV_CTL_VERSION)?),
            // CARD_INFO
            0x01 => {
                let mut info: SndCtlCardInfo = zeroed();
                fill_cstr(&mut info.id, "EclipseHDA0");
                fill_cstr(&mut info.driver, "eclipse-hda");
                fill_cstr(&mut info.name, NAME);
                fill_cstr(&mut info.longname, NAME);
                fill_cstr(&mut info.mixername, "Eclipse Mixer");
                ok(put(data, &info)?)
            }
            // ELEM_LIST with nowhere to put the ids: the count alone.
            0x10 => {
                let mut list: SndCtlElemList = get(data)?;
                list.count = MIXER_ELEMS;
                list.used = 0;
                ok(put(data, &list)?)
            }
            // ELEM_INFO
            0x11 => {
                let info: SndCtlElemInfo = get(data)?;
                let elem = elem_of(&info.id).ok_or(ENOENT)?;
                let mut info: SndCtlElemInfo = zeroed();
                info.id = elem_id(elem);
                info.access = ACCESS_READ_WRITE;
                info.count = 2;
                info.owner = -1;
                info.step = 1;
                match elem {
                    Elem::Volume => {
                        info.type_ = ELEM_INTEGER;
                        info.max = 100;
                    }
                    Elem::Switch => {
                        info.type_ = ELEM_BOOLEAN;
                        info.max = 1;
                    }
                }
                ok(put(data, &info)?)
            }
            // ELEM_READ
            0x12 => {
                let mut val: SndCtlElemValue = get(data)?;
                let elem = elem_of(&val.id).ok_or(ENOENT)?;
                val.id = elem_id(elem);
                let g = s.gain;
                val.values = match elem {
                    Elem::Volume => [g.left as i64, g.right as i64],
                    // An ALSA switch is 1 when it sounds.
                    Elem::Switch => [i64::from(!g.mute_left), i64::from(!g.mute_right)],
                };
                ok(put(data, &val)?)
            }
            // ELEM_WRITE
            0x13 => {
                let val: SndCtlElemValue = get(data)?;
                let elem = elem_of(&val.id).ok_or(ENOENT)?;
                let g = &mut s.gain;
                match elem {
                    Elem::Volume => {
                        g.left = val.values[0].clamp(0, 100) as u8;
                        g.right = val.values[1].clamp(0, 100) as u8;
                    }
                    Elem::Switch => {
                        g.mute_left = val.values[0] == 0;
                        g.mute_right = val.values[1] == 0;
                    }
                }
                ok(0)
            }
            // ELEM_LOCK, ELEM_UNLOCK
            0x14 | 0x15 => ok(0),
            // SUBSCRIBE_EVENTS: none are sent, but alsamixer needs it to work.
            0x16 => {
                let v: i32 = get(data)?;
                ok(put(data, &v.max(0))?)
            }
            // TLV_READ: no dB scale, as Linux answers a control without one.
            0x1a => Err(ENXIO),
            // HWDEP_NEXT_DEVICE, RAWMIDI_NEXT_DEVICE: none.
            0x20 | 0x40 => ok(put(data, &-1i32)?),
            // PCM_NEXT_DEVICE: device 0 alone.
            0x30 => {
                let v: i32 = get(data)?;
                ok(put(data, &if v < 0 { 0i32 } else { -1 })?)
            }
            // PCM_INFO: playback device 0 alone; anything else is ENOENT,
            // which is how a client learns there is no capture.
            0x31 => {
                let asked: SndPcmInfo = get(data)?;
                if asked.device != 0 || asked.stream != 0 {
                    return Err(ENOENT);
                }
                let mut info: SndPcmInfo = zeroed();
                info.subdevice = asked.subdevice;
                fill_cstr(&mut info.id, "Eclipse HDA");
                fill_cstr(&mut info.name, NAME);
                fill_cstr(&mut info.subname, "subdevice #0");
                info.subdevices_count = 1;
                info.subdevices_avail = 1;
                ok(put(data, &info)?)
            }
            // PCM_PREFER_SUBDEVICE, POWER
            0x32 | 0xd0 => ok(0),
            // POWER_STATE: D0.
            0xd1 => ok(put(data, &0i32)?),
            _ => Err(ENOTTY),
        }
    }

    /// `ELEM_LIST`: where its ids go is the address it holds.
    fn list_fetch(&self, arg: u64, data: &[u8]) -> Result<Option<Iov>, i32> {
        let list: SndCtlElemList = get(data)?;
        let n = list
            .space
            .min(MIXER_ELEMS.saturating_sub(list.offset.min(MIXER_ELEMS)));
        if list.pids == 0 || n == 0 {
            return Ok(None);
        }
        Ok(Some(Iov {
            ins: vec![(arg, core::mem::size_of::<SndCtlElemList>() as u64)],
            outs: vec![
                (arg, core::mem::size_of::<SndCtlElemList>() as u64),
                (
                    list.pids,
                    n as u64 * core::mem::size_of::<SndCtlElemId>() as u64,
                ),
            ],
        }))
    }

    fn list(&self, ins: &[u8], out: &mut [u8]) -> Result<(i64, usize), i32> {
        let mut list: SndCtlElemList = get(ins)?;
        list.count = MIXER_ELEMS;
        let head = core::mem::size_of::<SndCtlElemList>();
        let id = core::mem::size_of::<SndCtlElemId>();
        let mut used = 0u32;
        while used < list.space {
            let Some(elem) = elem_at(list.offset.saturating_add(used)) else {
                break;
            };
            let at = head + used as usize * id;
            if out.len() < at + id {
                break;
            }
            put(&mut out[at..], &elem_id(elem))?;
            used += 1;
        }
        list.used = used;
        put(out, &list)?;
        Ok((0, head + used as usize * id))
    }
}

/// The control node, over the card's stream (for its volume).
pub struct CtlNode<'a, O> {
    pub ctl: &'a mut Ctl,
    pub stream: &'a mut Stream<O>,
}

impl<O: Output> Device for CtlNode<'_, O> {
    fn open(&mut self, _flags: u64) -> Result<(u32, u64), i32> {
        self.ctl.next_fh = self.ctl.next_fh.wrapping_add(1).max(1);
        Ok((self.ctl.next_fh, 0))
    }

    fn release(&mut self, _fh: u32) {}

    fn read(&mut self, _: &Node, _: u32, _: u64, _: &mut [u8]) -> Result<usize, i32> {
        // No events are queued.
        Err(EAGAIN)
    }

    fn write(&mut self, _: &Node, _: u32, _: u64, _: &[u8]) -> Result<usize, i32> {
        Err(EINVAL)
    }

    fn ioctl(
        &mut self,
        _node: &Node,
        _fh: u32,
        cmd: u32,
        _arg: u64,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        self.ctl.control(self.stream, cmd, data)
    }

    fn fetch(
        &mut self,
        _fh: u32,
        cmd: u32,
        arg: u64,
        _waits: bool,
        data: &[u8],
    ) -> Result<Option<Iov>, i32> {
        if cmd & 0xffff == 0x5510 {
            self.ctl.list_fetch(arg, data)
        } else {
            Ok(None)
        }
    }

    fn ioctl_iov(
        &mut self,
        _node: &Node,
        _fh: u32,
        _cmd: u32,
        _arg: u64,
        _waits: bool,
        _iov: &Iov,
        ins: &[u8],
        out: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        self.ctl.list(ins, out)
    }
}

#[cfg(test)]
mod tests;
