//! System metrics for lunarbar, read straight from /proc and libc — no external
//! crates. cpu% (busy delta from /proc/stat), mem% (/proc/meminfo), and the
//! wall clock (libc localtime, UTC when no TZ is set).

/// Rolling CPU-usage sampler. `/proc/stat`'s aggregate `cpu` line gives
/// cumulative jiffies; usage is the busy fraction of the delta between reads.
#[derive(Default)]
pub struct CpuMeter {
    prev_busy: u64,
    prev_total: u64,
    have_prev: bool,
}

impl CpuMeter {
    /// Returns integer CPU-busy percent since the previous call, or `None`
    /// on the first call / if /proc/stat is unreadable.
    pub fn sample(&mut self) -> Option<u32> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        self.sample_from(&stat)
    }

    /// The pure half of [`Self::sample`], given the text of `/proc/stat`.
    ///
    /// The columns are read by POSITION, so a field that does not parse is a
    /// hard `None` and never a silent shift: `filter_map(parse)` dropped an
    /// unreadable column and slid every later index down one, which turns
    /// `vals[3]` from idle into iowait and reports a busy percent computed
    /// from the wrong counter. This kernel writes its own `/proc/stat`, so the
    /// column list is not a given the way it is on Linux.
    pub fn sample_from(&mut self, stat: &str) -> Option<u32> {
        let line = stat.lines().next()?; // "cpu  u n s idle iowait irq softirq steal ..."
        if !line.starts_with("cpu ") && !line.starts_with("cpu\t") {
            return None;
        }
        let vals = line
            .split_whitespace()
            .skip(1)
            .map(|v| v.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()?;
        if vals.len() < 4 {
            return None;
        }
        let idle = vals[3].saturating_add(vals.get(4).copied().unwrap_or(0)); // idle + iowait
        let total: u64 = vals.iter().copied().fold(0u64, u64::saturating_add);
        // `saturating_sub` cannot actually trigger -- idle's two columns are
        // part of `total` -- and is kept only so a future column change cannot
        // turn a wrap into 18 exabytes of busy time.
        let busy = total.saturating_sub(idle);

        let out = if self.have_prev {
            let dt = total.saturating_sub(self.prev_total);
            let db = busy.saturating_sub(self.prev_busy);
            // No delta at all (two reads inside one jiffy, or a stopped clock)
            // is 0%, via `checked_div` rather than a guard, so there is one
            // place the division can be wrong instead of two.
            let pct = db.saturating_mul(100).checked_div(dt).unwrap_or(0);
            // The ceiling is reachable: after a counter rebuild the busy delta
            // can exceed the total delta. See the test.
            Some(pct.min(100) as u32)
        } else {
            None
        };
        self.prev_busy = busy;
        self.prev_total = total;
        self.have_prev = true;
        out
    }
}

/// Memory-used percent from /proc/meminfo: 100 * (MemTotal - MemAvailable) /
/// MemTotal. Falls back to MemFree when MemAvailable is absent.
pub fn mem_percent() -> Option<u32> {
    mem_percent_from(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// The pure half of [`mem_percent`], given the text of `/proc/meminfo`.
///
/// `MemAvailable` is distinguished by being PRESENT, not by being non-zero:
/// the old `if avail > 0 { avail } else { free }` fell back to `MemFree` when
/// the kernel reported no memory available at all, and `MemFree` is the larger
/// number (it excludes reclaimable cache), so the bar under-reported usage
/// exactly on the machine that was out of memory.
pub fn mem_percent_from(meminfo: &str) -> Option<u32> {
    let mut total = None;
    let mut avail = None;
    let mut free = None;
    for line in meminfo.lines() {
        let mut it = line.split_whitespace();
        let key = it.next().unwrap_or("");
        let slot = match key {
            "MemTotal:" => &mut total,
            "MemAvailable:" => &mut avail,
            "MemFree:" => &mut free,
            _ => continue,
        };
        // A key whose value does not parse is left unset rather than read as
        // 0: a `MemTotal:` of 0 is "no idea", which is what `None` says.
        if let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) {
            *slot = Some(v);
        }
    }
    used_percent(total?, avail.or(free).unwrap_or(0))
}

/// Keyboard layout from `/proc/kbd` (`es` / `us`), shown uppercase on the bar.
pub fn kbd_layout() -> String {
    std::fs::read_to_string("/proc/kbd")
        .ok()
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ES".into())
}

/// Write `es` / `us` to `/proc/kbd` without `O_TRUNC`. Shell `echo x > file`
/// truncates on open; this kernel used to reject that and the pill snapped
/// back to ES on the next 1 Hz tick.
pub fn set_kbd_layout(layout: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/proc/kbd")
    else {
        return;
    };
    let _ = f.write_all(layout.as_bytes());
    let _ = f.write_all(b"\n");
}

/// Wall clock "HH:MM" (24h). Uses libc localtime_r so a set TZ is honoured;
/// with no TZ, musl returns UTC — fine for a bar clock.
pub fn clock_hhmm() -> String {
    unsafe {
        // Infer the platform time type — naming `libc::time_t` is deprecated
        // (musl 1.2.0 moves it to 64-bit; see rust-lang/libc#1848).
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = core::mem::zeroed();
        // localtime_r is thread-safe and never allocates a static buffer.
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "--:--".into();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// Monotonic seconds (CLOCK_MONOTONIC) as f64 — used to rate-scale network
/// counters independently of how often the render loop actually fires.
fn mono_secs() -> f64 {
    unsafe {
        let mut ts: libc::timespec = core::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) != 0 {
            return 0.0;
        }
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    }
}

/// Network throughput sampler. Sums rx/tx byte counters across every non-
/// loopback interface in /proc/net/dev and divides the delta by real elapsed
/// (monotonic) time, so the rate is correct even if repaints are irregular.
#[derive(Default)]
pub struct NetMeter {
    prev_rx: u64,
    prev_tx: u64,
    prev_t: f64,
    have_prev: bool,
}

/// A single network sample: down/up bytes-per-second and whether any real
/// interface is administratively up.
#[derive(Clone, Copy)]
pub struct NetRate {
    pub down: f64,
    pub up: f64,
    pub link: bool,
}

impl NetMeter {
    pub fn sample(&mut self) -> Option<NetRate> {
        let dev = std::fs::read_to_string("/proc/net/dev").ok()?;
        let (rx, tx) = net_totals_from(&dev);
        let now = mono_secs();
        let out = if self.have_prev {
            let dt = (now - self.prev_t).max(1e-3);
            NetRate {
                down: rx.saturating_sub(self.prev_rx) as f64 / dt,
                up: tx.saturating_sub(self.prev_tx) as f64 / dt,
                link: net_link_up(),
            }
        } else {
            NetRate {
                down: 0.0,
                up: 0.0,
                link: net_link_up(),
            }
        };
        self.prev_rx = rx;
        self.prev_tx = tx;
        self.prev_t = now;
        self.have_prev = true;
        Some(out)
    }
}

/// Sum rx and tx bytes over every non-loopback interface in `/proc/net/dev`.
///
/// Lines are recognised by having a `name:` prefix, not by their position:
/// `lines().skip(2)` assumed exactly two header lines, and this kernel writes
/// its own `/proc/net/dev` (`proc_net_dev_content`), so a header line added or
/// dropped there would silently stop counting the FIRST interface -- the panel
/// would show 0 B/s on a busy link. A line whose columns do not all parse is
/// skipped whole, rather than having its later indices slide down one and
/// charge some other counter as tx_bytes.
pub fn net_totals_from(dev: &str) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in dev.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        // Neither header line carries a colon ("Inter-|   Receive ..." and
        // "  face |bytes ...|bytes ..."), so they never get here; and a line
        // that somehow did would fail the all-columns-parse below. That is why
        // there is no further name filter: an extra one would be code no test
        // can reach.
        if name.is_empty() || name == "lo" {
            continue;
        }
        let Some(f) = rest
            .split_whitespace()
            .map(|v| v.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()
        else {
            continue;
        };
        // /proc/net/dev columns: rx_bytes=0 … tx_bytes=8
        if f.len() >= 9 {
            rx = rx.saturating_add(f[0]);
            tx = tx.saturating_add(f[8]);
        }
    }
    (rx, tx)
}

/// True if any non-loopback interface reports operstate "up".
fn net_link_up() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
        return false;
    };
    for e in dir.flatten() {
        let name = e.file_name();
        if name == "lo" {
            continue;
        }
        let p = e.path().join("operstate");
        if let Ok(s) = std::fs::read_to_string(&p) {
            if s.trim() == "up" {
                return true;
            }
        }
    }
    false
}

/// Human-readable byte-rate, e.g. "1.2M", "834K", "12B". Kept to ≤4 chars so
/// the module width stays stable even for absurd rates.
pub fn fmt_rate(bps: f64) -> String {
    // The unit letter is the whole point of the string, so the DIGITS give way
    // to it, not the other way round: pick the first unit whose rendering fits
    // the four characters the module is wide.
    //
    // This used to pick the unit by decimal thresholds (10^3, 10^6, 10^9) but
    // divide by binary ones (2^10, 2^20, 2^30), always format the mega branch
    // with one decimal, and then truncate anything over four characters. So
    // "119.0M" became "119." -- a number with a trailing dot and NO UNIT --
    // and that is the whole useful range of a gigabit link (125 MB/s is
    // 119 MiB/s). Everything from 10 MiB/s up lost its letter.
    //
    // Walking the units instead of comparing against a threshold also removes
    // the rounding trap: "1023B" and "1024M" do not fit either, and a
    // threshold written as 1000 or as 1024 gets one of them wrong.
    if !bps.is_finite() || bps < 0.0 {
        return "--".into();
    }
    const UNITS: [(f64, char); 4] = [
        (1.0, 'B'),
        (1024.0, 'K'),
        (1024.0 * 1024.0, 'M'),
        (1024.0 * 1024.0 * 1024.0, 'G'),
    ];
    for (div, unit) in UNITS {
        let v = bps / div;
        // One decimal only under 10, which is exactly when "9.9K" still fits.
        let s = if unit != 'B' && v < 10.0 {
            format!("{v:.1}{unit}")
        } else {
            format!("{v:.0}{unit}")
        };
        if s.chars().count() <= 4 {
            return s;
        }
    }
    // Past 1000 GiB/s there is no unit left; the digits go, never the letter.
    "999G".into()
}

/// Uptime as a compact "Nd Nh", "Nh Nm" or "Nm" string from /proc/uptime.
pub fn uptime() -> Option<String> {
    uptime_from(&std::fs::read_to_string("/proc/uptime").ok()?)
}

/// The pure half of [`uptime`], given the text of `/proc/uptime`.
pub fn uptime_from(text: &str) -> Option<String> {
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    // `as u64` on a negative or NaN seconds count saturates to 0 silently, so
    // an unusable value is refused instead of showing "0m" as if it were real.
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    let secs = secs as u64;
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    Some(if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    })
}

/// 1-minute load average from /proc/loadavg.
pub fn loadavg() -> Option<f32> {
    loadavg_from(&std::fs::read_to_string("/proc/loadavg").ok()?)
}

/// The pure half of [`loadavg`], given the text of `/proc/loadavg`.
pub fn loadavg_from(text: &str) -> Option<f32> {
    let v: f32 = text.split_whitespace().next()?.parse().ok()?;
    // "inf" and "nan" both parse as f32; neither is a load average, and both
    // would draw a bar of undefined length.
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// Root-filesystem used percent via statvfs("/").
pub fn disk_root_percent() -> Option<u32> {
    unsafe {
        let mut st: libc::statvfs = core::mem::zeroed();
        let path = b"/\0";
        if libc::statvfs(path.as_ptr() as *const libc::c_char, &mut st) != 0 {
            return None;
        }
        used_percent(st.f_blocks as u64, st.f_bavail as u64)
    }
}

/// Used percent from a total and an available count, in whatever unit both
/// share. `None` when the total is 0, which is how statvfs and meminfo both
/// say "no idea" -- a 0 total would divide by zero.
///
/// `available` over `total` (a filesystem with reserved blocks, or a kernel
/// that fills these in inconsistently) is 0% used, never a wrapped 18
/// exabytes.
pub fn used_percent(total: u64, available: u64) -> Option<u32> {
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    // In `u128`, because `used * 100` saturating in `u64` does not clamp the
    // RESULT, it clamps the numerator: a total near `u64::MAX` came out as 1%
    // used when every block was in use. The `.min(100)` cannot trigger either
    // (`used <= total` by the line above); it is there so the return type's
    // promise holds if that ever stops being true.
    Some(((used as u128 * 100) / total as u128).min(100) as u32)
}

/// CPU temperature in whole °C from a CPU/x86-package thermal zone when
/// typed as such, else the first readable zone (absent → None, module hidden).
pub fn temp_c() -> Option<u32> {
    let mut fallback = None;
    for zone in 0..8 {
        let base = format!("/sys/class/thermal/thermal_zone{zone}");
        let typed = std::fs::read_to_string(format!("{base}/type"))
            .map(|t| {
                let t = t.trim().to_ascii_lowercase();
                t.contains("cpu") || t.contains("x86") || t.contains("pkg") || t.contains("core")
            })
            .unwrap_or(false);
        if let Ok(milli) = std::fs::read_to_string(format!("{base}/temp")) {
            if let Some(c) = temp_from_milli(milli.trim()) {
                if typed {
                    return Some(c);
                }
                if fallback.is_none() {
                    fallback = Some(c);
                }
            }
        }
    }
    fallback
}

/// Whole degrees Celsius from a thermal zone's milli-degree `temp` file.
///
/// A zone reading exactly 0 is a READING, not an absence: the old `m > 0` made
/// a zone at or below freezing look like a missing sensor, so the module either
/// hid itself or silently fell through to a different zone. Only an unreadable
/// or absurd value is refused; the range is clamped the way the caller's
/// display expects.
pub fn temp_from_milli(text: &str) -> Option<u32> {
    let m: i64 = text.parse().ok()?;
    // Below -273C is not a temperature; above 200C is not a CPU that is still
    // running. Either means the file is not milli-degrees.
    if !(-273_000..=200_000).contains(&m) {
        return None;
    }
    // The upper half of the clamp is already implied by the range check above;
    // both are kept because they say different things (one hides an implausible
    // sensor, the other bounds what the pill can draw).
    Some((m / 1000).clamp(0, 200) as u32)
}

/// Battery percent + charging flag from /sys/class/power_supply, if any
/// battery is present (desktops → None, module hidden).
pub fn battery() -> Option<(u32, bool)> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for e in dir.flatten() {
        let p = e.path();
        let is_batt = std::fs::read_to_string(p.join("type"))
            .map(|t| t.trim() == "Battery")
            .unwrap_or(false);
        if !is_batt {
            continue;
        }
        let cap: u32 = std::fs::read_to_string(p.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse().ok())?;
        let status = std::fs::read_to_string(p.join("status")).ok();
        return Some((cap.min(100), status_is_charging(status.as_deref())));
    }
    None
}

/// Whether a `/sys/class/power_supply/*/status` value means "not discharging".
///
/// Kept beside the caller so the set of accepted words is in one place: the bar
/// draws a bolt for `Charging` and for `Full` (a battery at 100% on mains reads
/// `Full`, and showing a discharging icon there is what users report as "the
/// battery indicator is wrong"). Case and trailing newline do not matter;
/// anything else, and an unreadable file, is discharging.
///
/// NOT included, deliberately: Linux's `Not charging`, which means plugged in
/// but held below full by a charge threshold. It is not discharging either, so
/// the icon is arguably wrong there too, but that is a display decision and not
/// a bug to fix in passing.
pub fn status_is_charging(status: Option<&str>) -> bool {
    match status {
        Some(s) => {
            let s = s.trim();
            s.eq_ignore_ascii_case("charging") || s.eq_ignore_ascii_case("full")
        }
        None => false,
    }
}

/// Date "wkd dd mon" for the top bar's date pill. Language follows
/// [`crate::i18n::Lang::current`] (`es` / `en`). FONT_9X15 is ISO-8859-1
/// so Spanish accents (`mié`/`sáb`) render; English is ASCII.
pub fn date_dm() -> String {
    let lang = crate::i18n::Lang::current();
    let wd = lang.weekday_sun_first();
    let mo = lang.month_short();
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = core::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "-- --".into();
        }
        let wd = wd.get(tm.tm_wday as usize).copied().unwrap_or("");
        let mo = mo.get(tm.tm_mon as usize).copied().unwrap_or("");
        format!("{} {:02} {}", wd, tm.tm_mday, mo)
    }
}

// ── Calendar support (the clock-click popup) ─────────────────────────────────

/// Today's local (year, month 0-11, day 1-31), or None if localtime fails.
pub fn today() -> Option<(i32, u32, u32)> {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = core::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some((tm.tm_year + 1900, tm.tm_mon as u32, tm.tm_mday as u32))
    }
}

/// Number of days in the given month (0-based), Gregorian leap rules.
pub fn days_in_month(y: i32, m0: u32) -> u32 {
    const D: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    // The wrap is applied ONCE, before both branches: the day table indexed
    // `m0 % 12` while the leap test compared the raw `m0`, so month 13 read
    // February's 28 from the table and could never get its 29th.
    let m0 = (m0 % 12) as usize;
    if m0 == 1 && is_leap(y) {
        29
    } else {
        D[m0]
    }
}

/// Gregorian leap year. Split out because the rule is the one piece of the
/// calendar that is easy to write as `y % 4 == 0` and be wrong about for a
/// century at a time.
pub fn is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Weekday (0 = Monday .. 6 = Sunday) of the 1st of the given month, via the
/// standard days-from-civil algorithm (1970-01-01, day 0, was a Thursday).
pub fn first_weekday_mon0(y: i32, m0: u32) -> u32 {
    let (y, m, d) = (y as i64, m0 as i64 + 1, 1i64);
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    ((days + 3).rem_euclid(7)) as u32 // day 0 Thursday → Monday-first index 3
}

/// System audio volume percent (0..100). Returns None when no supported
/// audio subsystem is present; the volume module is hidden in that case.
/// Setting the volume is handled by spawning pactl/amixer/wpctl from the popup.
///
/// Forks a helper once (startup / after the user changes the slider) — never
/// call this from the 1 Hz tick.
pub fn volume() -> Option<u32> {
    if let Some(v) = volume_pactl() {
        return Some(v);
    }
    if let Some(v) = volume_wpctl() {
        return Some(v);
    }
    volume_amixer()
}

fn volume_pactl() -> Option<u32> {
    let out = std::process::Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // "Volume: front-left: 32768 /  50% / -18.06 dB,   front-right: ..."
    for part in s.split('[') {
        if let Some(rest) = part.strip_suffix("%]") {
            if let Ok(v) = rest.parse::<u32>() {
                return Some(v.min(100));
            }
        }
    }
    for token in s.split_whitespace() {
        if let Some(num) = token.strip_suffix('%') {
            if let Ok(v) = num.parse::<u32>() {
                return Some(v.min(100));
            }
        }
    }
    None
}

fn volume_wpctl() -> Option<u32> {
    let out = std::process::Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // "Volume: 0.50" or "Volume: 0.50 [MUTED]"
    let frac: f64 = s.split_whitespace().nth(1)?.parse().ok()?;
    Some((frac * 100.0).round().clamp(0.0, 100.0) as u32)
}

fn volume_amixer() -> Option<u32> {
    let out = std::process::Command::new("amixer")
        .args(["get", "Master"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // Prefer the first "[NN%]" token.
    for part in s.split('[') {
        if let Some(rest) = part.strip_suffix("%]") {
            if let Ok(v) = rest.parse::<u32>() {
                return Some(v.min(100));
            }
        }
        // amixer sometimes prints "NN%] [on]" without a second bracket close
        // in the same split — also accept "NN%" prefix.
        if let Some((num, _)) = part.split_once('%') {
            if let Ok(v) = num.parse::<u32>() {
                return Some(v.min(100));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two header lines this kernel's `proc_net_dev_content` writes.
    const NET_HDR: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";

    fn net_line(name: &str, rx: u64, tx: u64) -> String {
        format!("{name:>6}: {rx:>7} 12 0 0    0     0          0         0 {tx:>8} 9 0 0    0     0       0          0\n")
    }

    #[test]
    fn the_cpu_meter_needs_two_reads_and_then_reports_the_busy_delta() {
        let mut m = CpuMeter::default();
        // user nice system idle iowait: 0 busy, 100 idle.
        assert_eq!(m.sample_from("cpu  0 0 0 100 0\n"), None, "the first read");
        // +50 busy, +50 idle -> half the delta was busy.
        assert_eq!(m.sample_from("cpu  50 0 0 150 0\n"), Some(50));
        // Nothing moved at all: 0%, not a divide by zero.
        assert_eq!(m.sample_from("cpu  50 0 0 150 0\n"), Some(0));
        // All of the delta busy.
        assert_eq!(m.sample_from("cpu  150 0 0 150 0\n"), Some(100));
        // iowait counts as idle, not as busy.
        let mut m = CpuMeter::default();
        assert_eq!(m.sample_from("cpu  0 0 0 0 0\n"), None);
        assert_eq!(m.sample_from("cpu  0 0 0 0 100\n"), Some(0));
    }

    #[test]
    fn a_column_the_cpu_meter_cannot_read_is_a_refusal_and_never_a_shift() {
        // The regression: `filter_map(parse)` dropped an unreadable column and
        // slid every later index down one, so `vals[3]` stopped being idle and
        // the bar drew a percentage computed from iowait.
        let mut m = CpuMeter::default();
        assert_eq!(m.sample_from("cpu  0 0 0 100 0\n"), None);
        assert_eq!(m.sample_from("cpu  10 - 0 190 0\n"), None, "a bad column");
        assert_eq!(m.sample_from("cpu  10 0 0 -1 0\n"), None, "a negative");
        // And the refusal must not have poisoned the stored previous sample:
        // the next good read still measures from the first one.
        assert_eq!(m.sample_from("cpu  50 0 0 150 0\n"), Some(50));
    }

    #[test]
    fn the_cpu_meter_refuses_anything_that_is_not_the_aggregate_line() {
        let mut m = CpuMeter::default();
        assert_eq!(m.sample_from(""), None);
        assert_eq!(m.sample_from("cpu0 1 2 3 4\n"), None, "a per-core line");
        assert_eq!(m.sample_from("intr 1 2 3 4\n"), None);
        assert_eq!(m.sample_from("cpu  1 2 3\n"), None, "no idle column");
        // Tab-separated is still the aggregate line.
        let mut m = CpuMeter::default();
        assert_eq!(m.sample_from("cpu\t0 0 0 100 0\n"), None);
        assert_eq!(m.sample_from("cpu\t50 0 0 150 0\n"), Some(50));
    }

    #[test]
    fn a_busy_delta_larger_than_the_total_delta_still_reads_as_a_percent() {
        // Reachable after a counter rebuild, where idle drops while the rest
        // climbs: busy gains 50 jiffies while the total gains 20. Without the
        // ceiling the pill is drawn at 250%.
        let mut m = CpuMeter::default();
        // busy 0, idle 100, total 100.
        assert_eq!(m.sample_from("cpu  0 0 0 100 0\n"), None);
        // busy 50, idle 70, total 120: +50 busy on +20 total.
        assert_eq!(m.sample_from("cpu  50 0 0 70 0\n"), Some(100));
    }

    #[test]
    fn a_counter_that_goes_backwards_reads_as_idle_rather_than_wrapping() {
        // /proc/stat resets across a suspend or a kernel that recreates its
        // counters; `u64` subtraction there would wrap to 18 exabytes busy.
        let mut m = CpuMeter::default();
        assert_eq!(m.sample_from("cpu  1000 0 0 1000 0\n"), None);
        assert_eq!(m.sample_from("cpu  0 0 0 0 0\n"), Some(0));
    }

    #[test]
    fn memory_available_is_used_when_it_is_present_even_at_zero() {
        // The regression: `if avail > 0 { avail } else { free }` fell back to
        // MemFree when the kernel said no memory was available, and MemFree is
        // the LARGER number, so the bar under-reported usage exactly on the
        // machine that had run out.
        let out_of_memory = "MemTotal: 1000 kB\nMemAvailable: 0 kB\nMemFree: 400 kB\n";
        assert_eq!(mem_percent_from(out_of_memory), Some(100));
        // Present and non-zero: still preferred over MemFree.
        let normal = "MemTotal: 1000 kB\nMemAvailable: 600 kB\nMemFree: 200 kB\n";
        assert_eq!(mem_percent_from(normal), Some(40));
        // Absent: MemFree is the documented fallback.
        let no_avail = "MemTotal: 1000 kB\nMemFree: 250 kB\n";
        assert_eq!(mem_percent_from(no_avail), Some(75));
        // Neither: everything is used, as far as anyone here knows.
        assert_eq!(mem_percent_from("MemTotal: 1000 kB\n"), Some(100));
        // An unreadable MemAvailable is ABSENT, not zero: reading it as 0 would
        // draw a full memory bar off a line the kernel merely formatted oddly,
        // and MemFree is right there to fall back to.
        let bad_avail = "MemTotal: 1000 kB\nMemAvailable: ? kB\nMemFree: 400 kB\n";
        assert_eq!(mem_percent_from(bad_avail), Some(60));
    }

    #[test]
    fn memory_refuses_a_total_it_cannot_believe() {
        assert_eq!(mem_percent_from(""), None);
        assert_eq!(mem_percent_from("MemTotal: 0 kB\n"), None);
        assert_eq!(mem_percent_from("MemAvailable: 100 kB\n"), None);
        // A value that does not parse leaves the key unset rather than reading
        // as 0, which for MemTotal is the difference between "no idea" (None)
        // and "nothing" (a divide by zero).
        assert_eq!(mem_percent_from("MemTotal: lots kB\n"), None);
        // Line order does not matter, and unknown keys are ignored.
        let jumbled =
            "Cached: 9 kB\nMemFree: 1 kB\nMemTotal: 100 kB\nMemAvailable: 25 kB\nSwapTotal: 8 kB\n";
        assert_eq!(mem_percent_from(jumbled), Some(75));
    }

    #[test]
    fn the_network_totals_skip_loopback_and_sum_every_real_interface() {
        let dev = format!(
            "{NET_HDR}{}{}{}",
            net_line("lo", 999_999, 888_888),
            net_line("eth0", 1_000, 2_000),
            net_line("wlan0", 30, 40)
        );
        assert_eq!(net_totals_from(&dev), (1_030, 2_040));
    }

    #[test]
    fn the_network_totals_do_not_depend_on_how_many_header_lines_there_are() {
        // `lines().skip(2)` assumed exactly two, and this kernel writes its own
        // /proc/net/dev: a header line added or removed there would silently
        // stop counting the FIRST interface, so a busy link reads 0 B/s.
        let body = format!(
            "{}{}",
            net_line("eth0", 1_000, 2_000),
            net_line("eth1", 5, 7)
        );
        assert_eq!(net_totals_from(&format!("{NET_HDR}{body}")), (1_005, 2_007));
        let one_header = NET_HDR.lines().next().unwrap();
        assert_eq!(
            net_totals_from(&format!("{one_header}\n{body}")),
            (1_005, 2_007)
        );
        assert_eq!(net_totals_from(&body), (1_005, 2_007), "no header at all");
        assert_eq!(net_totals_from(""), (0, 0));
    }

    #[test]
    fn the_transmit_total_comes_from_the_ninth_column_and_not_a_neighbour() {
        // rx_bytes is 0 and tx_bytes is 8; a column read by the wrong index
        // charges packets or errors as bytes, which looks plausible on a bar.
        let one = "  eth0: 100 1 2 3 4 5 6 7 800 9 10 11 12 13 14 15\n";
        assert_eq!(net_totals_from(one), (100, 800));
        // A line too short to hold column 8 contributes nothing.
        assert_eq!(net_totals_from("  eth0: 1 2 3 4 5 6 7 8\n"), (0, 0));
    }

    #[test]
    fn a_network_line_with_an_unreadable_column_is_skipped_whole() {
        // Not partially: dropping the bad column would slide indices down and
        // make some other counter the transmit total.
        let dev = format!(
            "{NET_HDR}  eth0: 100 - 2 3 4 5 6 7 800 9 10 11 12 13 14 15\n{}",
            net_line("eth1", 11, 22)
        );
        assert_eq!(net_totals_from(&dev), (11, 22));
        // And the header's own second line, which has no colon, is not a device.
        assert_eq!(net_totals_from(NET_HDR), (0, 0));
    }

    #[test]
    fn a_rate_never_loses_its_unit_letter() {
        // The regression, and the reason it mattered: a gigabit link runs at
        // about 119 MiB/s, and "119.0M" was truncated to "119." -- a number
        // with a trailing dot and no unit. Everything from 10 MiB/s up had it.
        let gigabit = 125_000_000.0; // 1 Gb/s in bytes per second
        assert_eq!(fmt_rate(gigabit), "119M");
        for bps in [
            0.0,
            1.0,
            999.0,
            1023.0,
            1024.0,
            5_000.0,
            999_999.0,
            1_048_576.0,
            9_000_000.0,
            10_485_760.0,
            100_000_000.0,
            125_000_000.0,
            999_999_999.0,
            1_073_741_824.0,
            5e10,
            1e15,
            1e30,
        ] {
            let s = fmt_rate(bps);
            assert!(
                s.chars().count() <= 4,
                "fmt_rate({bps}) = {s:?}, wider than the module"
            );
            let last = s.chars().last().unwrap();
            assert!(
                matches!(last, 'B' | 'K' | 'M' | 'G'),
                "fmt_rate({bps}) = {s:?} ends in {last:?}, not a unit"
            );
            assert!(
                !s.contains(".") || s.chars().filter(|c| c.is_ascii_digit()).count() >= 2,
                "fmt_rate({bps}) = {s:?} has a dot with nothing after it"
            );
        }
    }

    #[test]
    fn a_rate_picks_its_unit_at_the_same_number_it_divides_by() {
        // The thresholds were decimal (10^3, 10^6, 10^9) and the divisors
        // binary (2^10, 2^20, 2^30), so a value could be labelled in a unit it
        // had not reached.
        assert_eq!(fmt_rate(0.0), "0B");
        // "1023B" is five characters, so it steps up to the next unit rather
        // than losing its letter.
        assert_eq!(fmt_rate(999.0), "999B");
        assert_eq!(fmt_rate(1023.0), "1.0K");
        assert_eq!(fmt_rate(1024.0), "1.0K");
        assert_eq!(fmt_rate(1024.0 * 9.5), "9.5K");
        assert_eq!(fmt_rate(1024.0 * 10.0), "10K");
        assert_eq!(fmt_rate(1024.0 * 999.0), "999K");
        assert_eq!(fmt_rate(1024.0 * 1023.0), "1.0M", "1023K does not fit");
        assert_eq!(fmt_rate(1024.0 * 1024.0), "1.0M");
        assert_eq!(fmt_rate(1024.0 * 1024.0 * 1024.0), "1.0G");
        // Nothing sensible to draw: say so rather than "NaNB" or "-1B".
        assert_eq!(fmt_rate(f64::NAN), "--");
        assert_eq!(fmt_rate(f64::INFINITY), "--");
        assert_eq!(fmt_rate(-1.0), "--");
    }

    #[test]
    fn uptime_reads_as_days_hours_or_minutes_and_refuses_nonsense() {
        assert_eq!(uptime_from("0.00 0.00").as_deref(), Some("0m"));
        assert_eq!(uptime_from("59.9 1.0").as_deref(), Some("0m"));
        assert_eq!(uptime_from("60.0 1.0").as_deref(), Some("1m"));
        assert_eq!(uptime_from("3660.0 1.0").as_deref(), Some("1h 1m"));
        assert_eq!(uptime_from("90061.0 1.0").as_deref(), Some("1d 1h"));
        // A negative or NaN would become 0 seconds through `as u64` and show
        // "0m" as if it were a real uptime.
        assert_eq!(uptime_from("-5.0 1.0"), None);
        assert_eq!(uptime_from("nan 1.0"), None);
        assert_eq!(uptime_from("inf 1.0"), None);
        assert_eq!(uptime_from(""), None);
        assert_eq!(uptime_from("hello"), None);
    }

    #[test]
    fn the_load_average_is_a_finite_non_negative_number_or_nothing() {
        assert_eq!(loadavg_from("0.42 0.30 0.25 1/99 1234"), Some(0.42));
        assert_eq!(loadavg_from("12.00 1 1"), Some(12.0));
        assert_eq!(loadavg_from("nan 1 1"), None);
        assert_eq!(loadavg_from("inf 1 1"), None);
        assert_eq!(loadavg_from("-1.0 1 1"), None);
        assert_eq!(loadavg_from(""), None);
    }

    #[test]
    fn a_used_percent_cannot_divide_by_zero_wrap_or_exceed_a_hundred() {
        assert_eq!(used_percent(0, 0), None, "a zero total is 'no idea'");
        assert_eq!(used_percent(100, 100), Some(0));
        assert_eq!(used_percent(100, 0), Some(100));
        assert_eq!(used_percent(100, 25), Some(75));
        // More available than there is in total (reserved blocks, or a kernel
        // filling these in inconsistently) is 0% used, not 18 exabytes.
        assert_eq!(used_percent(100, 500), Some(0));
        // And a total near the top of `u64` is still 100% used, not 1%: the
        // `saturating_mul` clamped the numerator, not the ratio.
        assert_eq!(used_percent(u64::MAX, 0), Some(100));
        assert_eq!(used_percent(u64::MAX, u64::MAX), Some(0));
        assert_eq!(used_percent(u64::MAX, u64::MAX / 4), Some(75));
        assert_eq!(used_percent(u64::MAX / 2, 0), Some(100));
    }

    #[test]
    fn a_thermal_zone_reading_zero_is_a_reading_and_not_an_absence() {
        // The regression: `m > 0` made a zone at or below freezing look like a
        // missing sensor, so the module hid itself or silently reported a
        // different zone.
        assert_eq!(temp_from_milli("0"), Some(0));
        assert_eq!(temp_from_milli("-5000"), Some(0), "clamped for display");
        assert_eq!(temp_from_milli("45000"), Some(45));
        assert_eq!(temp_from_milli("45999"), Some(45), "truncates, not rounds");
        assert_eq!(temp_from_milli("200000"), Some(200));
        // Not milli-degrees at all.
        assert_eq!(temp_from_milli("200001"), None);
        assert_eq!(temp_from_milli("-273001"), None);
        assert_eq!(temp_from_milli(""), None);
        assert_eq!(temp_from_milli("warm"), None);
        assert_eq!(temp_from_milli("45.0"), None);
    }

    #[test]
    fn the_battery_shows_a_bolt_for_charging_and_for_full() {
        // A battery at 100% on mains reads `Full`, and a discharging icon
        // there is what gets reported as "the battery indicator is wrong".
        assert!(status_is_charging(Some("Charging")));
        assert!(status_is_charging(Some("Full")));
        assert!(status_is_charging(Some("  Charging\n")));
        assert!(status_is_charging(Some("FULL")));
        assert!(!status_is_charging(Some("Discharging")));
        assert!(!status_is_charging(Some("Unknown")));
        assert!(!status_is_charging(Some("")));
        assert!(
            !status_is_charging(None),
            "an unreadable file is discharging"
        );
        // Deliberately not charging-looking; see the doc comment.
        assert!(!status_is_charging(Some("Not charging")));
    }

    #[test]
    fn every_month_has_the_days_it_should_and_february_follows_the_leap_rule() {
        let lengths = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        for (m0, want) in lengths.iter().enumerate() {
            assert_eq!(days_in_month(2026, m0 as u32), *want, "month {m0} of 2026");
        }
        assert_eq!(days_in_month(2024, 1), 29, "2024 is a leap year");
        assert_eq!(days_in_month(2023, 1), 28);
        assert_eq!(days_in_month(2000, 1), 29, "divisible by 400");
        assert_eq!(days_in_month(1900, 1), 28, "divisible by 100, not 400");
        assert_eq!(days_in_month(2100, 1), 28);
    }

    #[test]
    fn a_month_index_past_december_wraps_the_same_way_in_both_branches() {
        // The day table was indexed `m0 % 12` while the leap test compared the
        // raw `m0`, so month 13 read February's 28 and could never get a 29th.
        for m0 in 0..36u32 {
            assert_eq!(
                days_in_month(2024, m0),
                days_in_month(2024, m0 % 12),
                "month {m0} of a leap year disagrees with month {}",
                m0 % 12
            );
        }
        assert_eq!(days_in_month(2024, 13), 29);
    }

    #[test]
    fn the_leap_rule_is_the_gregorian_one_and_not_just_divisible_by_four() {
        assert!(is_leap(2024) && is_leap(2000) && is_leap(1600));
        assert!(!is_leap(2023) && !is_leap(1900) && !is_leap(2100) && !is_leap(1700));
    }

    #[test]
    fn the_first_of_the_month_lands_on_the_weekday_it_really_does() {
        // Monday = 0. Checked against dates that can be looked up, including
        // the algorithm's own epoch and both sides of a skipped leap day.
        for (y, m0, want) in [
            (1970, 0, 3), // 1970-01-01, a Thursday: the algorithm's day 0
            (2000, 0, 5),
            (2024, 1, 3),
            (2026, 0, 3),
            (2026, 8, 1), // 2026-09-01, a Tuesday
            (2026, 11, 1),
            (1900, 2, 3), // just after the leap day 1900 did NOT have
            (2100, 1, 0),
            (1999, 11, 2),
        ] {
            assert_eq!(
                first_weekday_mon0(y, m0),
                want,
                "the 1st of month {m0} of {y}"
            );
        }
    }

    #[test]
    fn consecutive_months_chain_by_exactly_their_own_length() {
        // The two calendar functions have to agree, because the popup uses one
        // to place the 1st and the other to know where to stop. If either
        // drifts, every month after it is drawn on the wrong weekday.
        for y in 1998..2102i32 {
            for m0 in 0..12u32 {
                let (ny, nm) = if m0 == 11 { (y + 1, 0) } else { (y, m0 + 1) };
                let expected = (first_weekday_mon0(y, m0) + days_in_month(y, m0)) % 7;
                assert_eq!(
                    first_weekday_mon0(ny, nm),
                    expected,
                    "month {m0} of {y} does not lead into month {nm} of {ny}"
                );
            }
        }
    }

    #[test]
    fn the_weekday_a_month_starts_on_is_always_a_column_the_header_has() {
        // `first_weekday_mon0` indexes the Monday-first header, so the two have
        // to stay in step. Spelled out because the Sunday-first table is right
        // next to it in `i18n` and picking the wrong one shifts every date in
        // the grid by a day, which is a bug nobody notices for a month.
        for lang in [crate::i18n::Lang::Es, crate::i18n::Lang::En] {
            let mon = lang.weekday_mon_first();
            let sun = lang.weekday_sun_first();
            for i in 0..7usize {
                // Monday-first index i is Sunday-first index i+1.
                let a = mon[i].to_lowercase();
                let b = sun[(i + 1) % 7].to_lowercase();
                assert!(
                    b.starts_with(&a),
                    "{lang:?}: the Monday-first header {a:?} at {i} is not the \
                     start of the Sunday-first {b:?} for the same day"
                );
            }
            for y in 2024..2030i32 {
                for m0 in 0..12u32 {
                    let idx = first_weekday_mon0(y, m0) as usize;
                    assert!(idx < mon.len(), "{y}-{m0}: column {idx} does not exist");
                }
            }
        }
    }
}
