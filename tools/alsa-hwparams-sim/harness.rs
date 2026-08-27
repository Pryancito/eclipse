// Standalone harness: runs the kernel's refine/install against a faithful
// replay of alsa-lib's negotiation (snd_pcm_hw_params_choose + the *_near
// helpers speaker-test uses).
#![allow(dead_code, unused_variables, unused_mut)]

const RING: u64 = 32640; // 130560 bytes / 4

const PAR_ACCESS: usize = 0;
const PAR_FORMAT: usize = 1;
const PAR_SUBFORMAT: usize = 2;
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
const INTERVAL_OPENMIN: u32 = 1 << 0;
const INTERVAL_OPENMAX: u32 = 1 << 1;
const INTERVAL_INTEGER: u32 = 1 << 2;
const INTERVAL_EMPTY: u32 = 1 << 3;

const RATES: [u32; 11] = [8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000];

#[derive(Clone, Copy, Default, Debug)]
struct SndInterval { min: u32, max: u32, flags: u32 }
#[derive(Clone, Copy)]
struct SndMask { bits: [u32; 8] }

struct SndPcmHwParams {
    flags: u32,
    masks: [SndMask; 3],
    intervals: [SndInterval; 12],
    rmask: u32, cmask: u32, info: u32, msbits: u32,
    rate_num: u32, rate_den: u32, fifo_size: u64,
}

#[derive(Debug)]
enum FsError { InvalidParam, DeviceError }
type Result<T> = core::result::Result<T, FsError>;

struct PcmDev;
impl PcmDev {
    fn ring_frames(&self) -> u64 { RING }
// @@KERNEL_BLOCK@@
}

fn open_params() -> SndPcmHwParams {
    // What alsa-lib sends for snd_pcm_hw_params_any(): every mask all-ones,
    // every interval [0, UINT_MAX].
    SndPcmHwParams {
        flags: 0,
        masks: [SndMask { bits: [!0u32; 8] }; 3],
        intervals: [SndInterval { min: 0, max: u32::MAX, flags: 0 }; 12],
        rmask: !0, cmask: 0, info: 0, msbits: 0,
        rate_num: 0, rate_den: 0, fifo_size: 0,
    }
}

fn iv(p: &SndPcmHwParams, par: usize) -> SndInterval { p.intervals[par - 8] }

/// alsa-lib snd_pcm_hw_param_set_min/max/near helpers, replayed against the
/// kernel refine.
fn set_near(dev: &PcmDev, p: &mut SndPcmHwParams, par: usize, val: u32, label: &str) -> bool {
    // Faithful to alsa-lib's snd_pcm_hw_param_set_near: find the SUPPORTED
    // value closest to the request across the whole refined interval, not just
    // a few units around it.
    let mut probe = clone_params(p);
    if !dev.refine(&mut probe) { println!("  set_{}_near({}): interval empty", label, val); return false; }
    let cur = iv(&probe, par);
    let lo = cur.min; let hi = cur.max;
    // Candidates: the request clamped into range, then walk outward from it.
    let start = val.clamp(lo, hi);
    let mut cands: Vec<u32> = vec![start];
    let span = (hi - lo).min(1 << 20);
    let mut step = 1u32;
    while step <= span {
        if start.saturating_add(step) <= hi { cands.push(start + step); }
        if start.saturating_sub(step) >= lo { cands.push(start - step); }
        step = if step < 64 { step + 1 } else { step + step / 4 };
    }
    cands.push(lo); cands.push(hi);
    for cand in cands {
        let mut t = clone_params(p);
        t.intervals[par - 8] = SndInterval { min: cand, max: cand, flags: INTERVAL_INTEGER };
        if dev.refine(&mut t) {
            *p = t;
            if cand != val { println!("  set_{}_near({}) -> {}", label, val, cand); }
            else { println!("  set_{}_near({}) -> {}", label, val, cand); }
            return true;
        }
    }
    println!("  set_{}_near({}) FAILED (range {}..{})", label, val, lo, hi);
    false
}

fn clone_params(p: &SndPcmHwParams) -> SndPcmHwParams {
    SndPcmHwParams {
        flags: p.flags, masks: p.masks, intervals: p.intervals,
        rmask: p.rmask, cmask: p.cmask, info: p.info, msbits: p.msbits,
        rate_num: p.rate_num, rate_den: p.rate_den, fifo_size: p.fifo_size,
    }
}

/// snd_pcm_hw_params_choose: set_first on everything except buffer_size
/// (set_last), refining after each — exactly alsa-lib's order.
fn choose(dev: &PcmDev, p: &mut SndPcmHwParams) -> bool {
    let vars = [PAR_SAMPLE_BITS, PAR_FRAME_BITS, PAR_CHANNELS, PAR_RATE,
                PAR_PERIOD_TIME, PAR_PERIOD_SIZE, PAR_PERIOD_BYTES, PAR_PERIODS,
                PAR_BUFFER_TIME, PAR_BUFFER_SIZE, PAR_BUFFER_BYTES, PAR_TICK_TIME];
    for v in vars {
        let cur = iv(p, v);
        let pick = if v == PAR_BUFFER_SIZE { cur.max } else { cur.min };
        p.intervals[v - 8] = SndInterval { min: pick, max: pick, flags: INTERVAL_INTEGER };
        if !dev.refine(p) {
            println!("  choose: refine FAILED after fixing par {} to {}", v, pick);
            return false;
        }
    }
    true
}

fn show(tag: &str, p: &SndPcmHwParams) {
    println!("  {}: rate={}..{} ps={}..{} pt={}..{} periods={}..{} bs={}..{} bt={}..{}",
        tag,
        iv(p,PAR_RATE).min, iv(p,PAR_RATE).max,
        iv(p,PAR_PERIOD_SIZE).min, iv(p,PAR_PERIOD_SIZE).max,
        iv(p,PAR_PERIOD_TIME).min, iv(p,PAR_PERIOD_TIME).max,
        iv(p,PAR_PERIODS).min, iv(p,PAR_PERIODS).max,
        iv(p,PAR_BUFFER_SIZE).min, iv(p,PAR_BUFFER_SIZE).max,
        iv(p,PAR_BUFFER_TIME).min, iv(p,PAR_BUFFER_TIME).max);
}

fn scenario_speaker_test(dev: &PcmDev) -> bool {
    println!("\n=== speaker-test -D plughw:N -c 2 -t sine (rate 48000) ===");
    let mut p = open_params();
    if !dev.refine(&mut p) { println!("  initial refine FAILED"); return false; }
    show("after any+refine", &p);

    // set_rate_near(48000)
    if !set_near(dev, &mut p, PAR_RATE, 48000, "rate") { return false; }
    // speaker-test: buffer_time = min(buffer_time_max, 500000); period_time = buffer_time/4
    let bt_max = iv(&p, PAR_BUFFER_TIME).max;
    let buffer_time = bt_max.min(500_000);
    println!("  buffer_time_max={} -> using {}", bt_max, buffer_time);
    let period_time = buffer_time / 4;
    if !set_near(dev, &mut p, PAR_PERIOD_TIME, period_time, "period_time") { return false; }
    if !set_near(dev, &mut p, PAR_BUFFER_TIME, buffer_time, "buffer_time") { return false; }
    show("after times", &p);

    if !choose(dev, &mut p) { return false; }
    show("after choose", &p);
    match dev.install(&mut p) {
        Ok(()) => { show("INSTALLED", &p); true }
        Err(e) => { println!("  HW_PARAMS FAILED: {:?}", e); false }
    }
}

fn scenario_aplay(dev: &PcmDev, rate: u32) -> bool {
    println!("\n=== aplay (rate {}, sizes left to the kernel) ===", rate);
    let mut p = open_params();
    if !dev.refine(&mut p) { println!("  initial refine FAILED"); return false; }
    if !set_near(dev, &mut p, PAR_RATE, rate, "rate") { return false; }
    // aplay -> snd_pcm_set_params-ish: buffer_time 500ms, period_time 125ms
    if !set_near(dev, &mut p, PAR_BUFFER_TIME, 500_000, "buffer_time") { return false; }
    if !set_near(dev, &mut p, PAR_PERIOD_TIME, 125_000, "period_time") { return false; }
    if !choose(dev, &mut p) { return false; }
    match dev.install(&mut p) {
        Ok(()) => { show("INSTALLED", &p); true }
        Err(e) => { println!("  HW_PARAMS FAILED: {:?}", e); false }
    }
}

fn scenario_explicit_sizes(dev: &PcmDev) -> bool {
    println!("\n=== explicit period_size=1024 / periods=4 (typical game/SDL) ===");
    let mut p = open_params();
    if !dev.refine(&mut p) { return false; }
    if !set_near(dev, &mut p, PAR_RATE, 44100, "rate") { return false; }
    if !set_near(dev, &mut p, PAR_PERIOD_SIZE, 1024, "period_size") { return false; }
    if !set_near(dev, &mut p, PAR_PERIODS, 4, "periods") { return false; }
    if !choose(dev, &mut p) { return false; }
    match dev.install(&mut p) {
        Ok(()) => { show("INSTALLED", &p); true }
        Err(e) => { println!("  HW_PARAMS FAILED: {:?}", e); false }
    }
}

fn scenario_plug_default(dev: &PcmDev) -> bool {
    println!("\n=== plug: client asks 44100 but slave runs whatever fits ===");
    let mut p = open_params();
    if !dev.refine(&mut p) { return false; }
    if !set_near(dev, &mut p, PAR_RATE, 44100, "rate") { return false; }
    if !choose(dev, &mut p) { return false; }
    match dev.install(&mut p) {
        Ok(()) => { show("INSTALLED", &p); true }
        Err(e) => { println!("  HW_PARAMS FAILED: {:?}", e); false }
    }
}


fn scenario_latency(dev: &PcmDev, rate: u32, buf_us: u32, per_us: u32, tag: &str) -> bool {
    println!("\n=== {} (rate {}, buffer {} us, period {} us) ===", tag, rate, buf_us, per_us);
    let mut p = open_params();
    if !dev.refine(&mut p) { return false; }
    if !set_near(dev, &mut p, PAR_RATE, rate, "rate") { return false; }
    if !set_near(dev, &mut p, PAR_PERIOD_TIME, per_us, "period_time") { return false; }
    if !set_near(dev, &mut p, PAR_BUFFER_TIME, buf_us, "buffer_time") { return false; }
    if !choose(dev, &mut p) { return false; }
    match dev.install(&mut p) {
        Ok(()) => { show("INSTALLED", &p); true }
        Err(e) => { println!("  HW_PARAMS FAILED: {:?}", e); false }
    }
}

fn scenario_sizes(dev: &PcmDev, rate: u32, ps: u32, n: u32, tag: &str) -> bool {
    println!("\n=== {} (rate {}, period_size {}, periods {}) ===", tag, rate, ps, n);
    let mut p = open_params();
    if !dev.refine(&mut p) { return false; }
    if !set_near(dev, &mut p, PAR_RATE, rate, "rate") { return false; }
    if !set_near(dev, &mut p, PAR_PERIOD_SIZE, ps, "period_size") { return false; }
    if !set_near(dev, &mut p, PAR_PERIODS, n, "periods") { return false; }
    if !choose(dev, &mut p) { return false; }
    match dev.install(&mut p) {
        Ok(()) => { show("INSTALLED", &p); true }
        Err(e) => { println!("  HW_PARAMS FAILED: {:?}", e); false }
    }
}

fn main() {
    let dev = PcmDev;
    let mut all = true;
    all &= scenario_speaker_test(&dev);
    all &= scenario_aplay(&dev, 48000);
    all &= scenario_aplay(&dev, 44100);
    all &= scenario_explicit_sizes(&dev);
    all &= scenario_plug_default(&dev);
    for r in [8000u32, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000] {
        all &= scenario_latency(&dev, r, 500_000, 125_000, "aplay-style");
    }
    all &= scenario_latency(&dev, 48000, 20_000, 5_000, "low latency (20ms/5ms)");
    all &= scenario_latency(&dev, 48000, 2_000_000, 500_000, "huge buffer (2s, clamps to ring)");
    all &= scenario_latency(&dev, 44100, 40_000, 10_000, "game audio 44.1k");
    all &= scenario_sizes(&dev, 48000, 480, 2, "pipewire-ish 10ms x2");
    all &= scenario_sizes(&dev, 48000, 64, 4, "client asks below the 256 floor");
    all &= scenario_sizes(&dev, 48000, 2048, 8, "SDL 2048x8");
    all &= scenario_sizes(&dev, 192000, 8192, 4, "max rate, max period");
    println!("\n==== {} ====", if all { "ALL SCENARIOS PASS" } else { "FAILURES PRESENT" });
    std::process::exit(if all { 0 } else { 1 });
}
