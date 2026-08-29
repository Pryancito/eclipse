//! `/dev/snd/*` — native ALSA kernel ABI over an [`AudioScheme`] device.
//!
//! This is what stock alsa-lib (`aplay`, SDL, mpg123, anything using
//! `snd_pcm_*`) talks to: one `controlC<card>` + `pcmC<card>D0p` pair per HDA
//! controller. The implementation targets alsa-lib's `hw` plugin in its
//! **RW-interleaved + SYNC_PTR** mode:
//!
//! * `SNDRV_PCM_IOCTL_HW_REFINE/HW_PARAMS` constrain to what the driver ring
//!   does natively — S16LE, stereo, the discrete HDA rate set — and alsa-lib's
//!   `plug` layer (routed in by the shipped `/etc/asound.conf`) converts
//!   everything else in userspace.
//! * Data moves through `SNDRV_PCM_IOCTL_WRITEI_FRAMES`; the status/control
//!   pages are *not* mmap-able here, which alsa-lib detects and transparently
//!   falls back to `SNDRV_PCM_IOCTL_SYNC_PTR` — implemented with Linux's exact
//!   flag semantics.
//! * No capture, no mmap data access, no pause/resume: the `info` flags and
//!   refine masks never advertise them, so clients don't try.
//!
//! Ioctls are matched on the `_IOC` type+nr bytes only (not the size bits):
//! the struct layouts here mirror the x86_64 uapi, but matching the full cmd
//! word would break the moment alsa-lib is built against a slightly newer
//! header that grew a reserved field.

#![allow(unsafe_code)]

use alloc::sync::Arc;
use core::any::Any;

use kernel_hal::drivers::scheme::AudioScheme;
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

// ── uapi mirror (x86_64) ────────────────────────────────────────────────────

const SNDRV_PCM_VERSION: i32 = 0x0002_000e; // 2.0.14
const SNDRV_CTL_VERSION: i32 = 0x0002_0007;

// snd_pcm_state_t
const STATE_SETUP: i32 = 1;
const STATE_PREPARED: i32 = 2;
const STATE_RUNNING: i32 = 3;
const STATE_OPEN: i32 = 0;

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

    /// Frames the client may write within its negotiated buffer.
    fn avail(&self, st: &PcmState) -> u64 {
        st.buffer_size.saturating_sub(self.queued_frames())
    }

    fn hw_ptr(&self, st: &PcmState) -> u64 {
        (st.appl_ptr + st.boundary - self.queued_frames()) % st.boundary
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
        if n_lo > 0 {
            // ps ≥ (buffer+1)/(periods+1)  and  ps ≤ buffer/periods
            changed |= Self::iv_clamp(
                &mut iv[Self::IV_PERIOD_SIZE],
                (bs_lo + 1).div_ceil(n_hi + 1),
                bs_hi / n_lo,
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

        p.info = INFO_INTERLEAVED | INFO_BLOCK_TRANSFER;
        p.msbits = 16;
        p.rate_den = 1;
        p.rate_num = 0;
        p.fifo_size = 0;
        // Report every parameter as (potentially) changed; alsa-lib re-reads all.
        p.cmask = 0x000f_ff07;
        ok
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
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| b.checked_sub(1))
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
            let empty = p
                .intervals
                .iter()
                .position(Self::iv_empty)
                .map(Self::iv_name)
                .unwrap_or("mask");
            Self::log_hw_params_failure(empty, &requested, &p.intervals);
            return Err(FsError::InvalidParam);
        }

        // Rate first: everything time-related depends on it.
        let rate = RATES
            .iter()
            .copied()
            .find(|&r| r >= p.intervals[Self::IV_RATE].min && r <= p.intervals[Self::IV_RATE].max)
            .ok_or(FsError::InvalidParam)?;
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

    /// Blocking interleaved write: the ALSA equivalent of the OSS node's
    /// spin-retry (the synchronous INode contract has no waker to park on).
    fn writei(&self, xfer: &mut SndXferI) -> Result<()> {
        {
            let st = self.st.lock();
            if st.state != STATE_PREPARED && st.state != STATE_RUNNING {
                return Err(FsError::InvalidParam);
            }
        }
        let total_bytes = (xfer.frames * BYTES_PER_FRAME) as usize;
        let src = xfer.buf as *const u8;
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
                let mut st = self.st.lock();
                st.appl_ptr = (st.appl_ptr + n as u64 / BYTES_PER_FRAME) % st.boundary;
                st.state = STATE_RUNNING;
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
        let deadline = kernel_hal::timer::timer_now()
            + core::time::Duration::from_secs(
                (self.audio.queued_bytes() as u64 / (48_000 * BYTES_PER_FRAME) + 2).min(10),
            );
        while self.audio.queued_bytes() > 0 {
            if kernel_hal::timer::timer_now() >= deadline {
                break;
            }
            kernel_hal::deferred_job::drain_deferred_jobs();
            core::hint::spin_loop();
        }
        self.st.lock().state = STATE_SETUP;
        Ok(())
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
}

impl INode for PcmDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        let st = self.st.lock();
        let avail = self.avail(&st);
        Ok(PollStatus {
            read: false,
            write: avail >= st.avail_min,
            error: false,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        let ty = (cmd >> 8) & 0xff;
        let nr = cmd & 0xff;
        if ty != b'A' as u32 {
            return Err(FsError::NotSupported);
        }
        match nr {
            0x00 => {
                // PVERSION
                unsafe { *(data as *mut i32) = SNDRV_PCM_VERSION };
                Ok(0)
            }
            0x01 => {
                // INFO
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
            0x02 | 0x03 | 0x04 => Ok(0),
            0x10 => {
                // HW_REFINE
                let p = unsafe { &mut *(data as *mut SndPcmHwParams) };
                if self.refine(p) {
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            0x11 => {
                // HW_PARAMS
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
                let s = unsafe { &mut *(data as *mut SndPcmStatus) };
                self.fill_status(s);
                Ok(0)
            }
            0x21 => {
                // DELAY
                unsafe { *(data as *mut i64) = self.queued_frames() as i64 };
                Ok(0)
            }
            0x22 => Ok(0), // HWSYNC — queued_bytes() reads live hardware state
            0x23 => {
                // SYNC_PTR — Linux flag semantics.
                let sp = unsafe { &mut *(data as *mut SndPcmSyncPtr) };
                let mut st = self.st.lock();
                if sp.flags & SYNC_PTR_APPL != 0 {
                    st.appl_ptr = sp.control.appl_ptr % st.boundary.max(1);
                } else {
                    sp.control.appl_ptr = st.appl_ptr;
                }
                if sp.flags & SYNC_PTR_AVAIL_MIN != 0 {
                    if sp.control.avail_min > 0 {
                        st.avail_min = sp.control.avail_min;
                    }
                } else {
                    sp.control.avail_min = st.avail_min;
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
            0x47 => Ok(0), // RESUME
            0x48 => Ok(0), // XRUN
            0x50 => {
                // WRITEI_FRAMES
                let xfer = unsafe { &mut *(data as *mut SndXferI) };
                self.writei(xfer)?;
                Ok(0)
            }
            _ => {
                debug!("[snd] pcm ioctl 'A' nr={:#x} unsupported", nr);
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
                unsafe { *(data as *mut i32) = SNDRV_CTL_VERSION };
                Ok(0)
            }
            0x01 => {
                // CARD_INFO
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
                fill_cstr(&mut info.mixername, "Eclipse HDA (no mixer)");
                fill_cstr(&mut info.components, "");
                Ok(0)
            }
            0x10 => {
                // ELEM_LIST: no mixer controls yet.
                // struct: u32 offset, space, used, count; ptr; reserved.
                unsafe {
                    *(data as *mut u32).add(2) = 0; // used
                    *(data as *mut u32).add(3) = 0; // count
                }
                Ok(0)
            }
            // ELEM_INFO / ELEM_READ / ELEM_WRITE: no elements exist.
            0x11 | 0x12 | 0x13 => Err(FsError::EntryNotFound),
            0x30 => {
                // PCM_NEXT_DEVICE: single PCM device (0).
                let v = unsafe { &mut *(data as *mut i32) };
                *v = if *v < 0 { 0 } else { -1 };
                Ok(0)
            }
            0x31 => {
                // PCM_INFO
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
