// Eclipse OS: a Firefox-shaped audio probe.
//
// `firefox-probe` answers "what does the kernel lack for a browser?". This
// answers the same question for sound: it drives every layer Firefox's audio
// output rests on, bottom-up, exactly the way that layer is driven in the
// field, and reports the first one that breaks. Sound has one failure mode
// no other subsystem has -- silence with every call returning success -- so
// each playback layer also emits an audible test tone (440 Hz, ~0.4 s).
// What you HEAR tells you which layer reaches the speaker; what you READ
// tells you which layer the kernel refuses.
//
// The layers, in the order Firefox's cubeb walks them:
//
//   devices   the nodes the kernel created, and /proc/gpusnd (the codec state
//             read back OUT of the hardware: the only thing that separates
//             "silent but Ok" from a real fault).
//   wake      the kernel's timer tick, measured with no audio involved: 1 ms
//             deadlines (poll, nanosleep) and poll(0 fds, 9..21 ms), whose
//             overshoot is the 4 ms re-scan tick every bus-less fd wait
//             (pcm, timer) is served at -- it bounds every wake-up below.
//   oss       /dev/dsp: the smallest kernel PCM ABI, a plain write(2). If this
//             is silent, nothing above it can be heard.
//   alsa-pcm  /dev/snd/pcmC0D0p driven with the raw SNDRV_PCM_IOCTL_* sequence
//             alsa-lib issues (PVERSION, HW_REFINE with a wide-open request,
//             HW_PARAMS, SW_PARAMS, PREPARE, WRITEI_FRAMES). This is the
//             surface both cubeb's ALSA backend and PulseAudio's
//             module-alsa-sink stand on.
//   alsa-timer /dev/snd/timer: the PCM period timer alsa-lib binds when a
//             client asks for period_event -- PulseAudio's tsched=0 sink does,
//             from snd_pcm_sw_params -- and polls beside the PCM fd.
//   alsa-ctl  /dev/snd/controlC0: CARD_INFO and the "Master" mixer elements
//             alsa-lib's simple mixer, amixer and Pulse's card probe look up.
//   daemon    the pulseaudio binary, the `pulse` account, a live process,
//             and the daemon's own log -- WHY the socket is missing.
//   unix-sock the AF_UNIX connect() errno split PulseAudio's stale-socket
//             check depends on: ENOENT for a path nobody bound.
//   pulse     the server socket cubeb-pulse / libpulse connect to first, and
//             whether a real (module-alsa-sink) sink sits behind it: a server
//             with only auto_null answers every connect and plays nothing.
//   pulse-play the tone through the server itself (pacat), with the sink and
//             stream state from pactl and the HDA stream's account from
//             /proc/gpusnd: the path mpg123 and Firefox actually take.
//   verdict   which cubeb backend would initialise from what was measured --
//             the same choice Firefox's OpenCubeb() makes.
//
// Every ioctl number and struct layout below is the x86_64 uapi
// (include/uapi/sound/asound.h, <sys/soundcard.h>); musl ships neither
// header, so they are spelled out. The kernel side is
// linux-object/src/fs/devfs/{snd,dsp}.rs.
//
// Build:  musl-gcc -O2 -static -o audio-probe audio-probe.c
// Run:    audio-probe            (-v per-check detail, --no-tone silent,
//                                 --card N for a card other than 0)
//
// Exit status is the number of failed checks, capped at 125.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// ── reporting (same shape as firefox-probe) ─────────────────────────────────

static int g_verbose;
static int g_no_tone;
static int g_card;
static int g_pass, g_fail, g_skip;
static const char *g_skipped[16];
static int g_nskipped;

// `--skip NAME` (repeatable): leave a section out, so a run can get past one
// that takes the machine down and still reach the ones after it.
static int skipped(const char *name) {
  for (int i = 0; i < g_nskipped; i++)
    if (!strcmp(g_skipped[i], name)) return 1;
  return 0;
}
static const char *g_section = "";

static void section(const char *name) {
  g_section = name;
  printf("\n[%s]\n", name);
}

static void ok(const char *name, const char *why) {
  g_pass++;
  if (g_verbose) printf("  [ok]   %s  (%s)\n", name, why);
}

static void fail(const char *name, const char *why, int err) {
  g_fail++;
  printf("  [FAIL] %s: %s  (%s)\n", name, err ? strerror(err) : "unexpected result", why);
}

static void skip(const char *name, const char *why) {
  g_skip++;
  if (g_verbose) printf("  [skip] %s  (%s)\n", name, why);
}

static void check(int good, const char *name, const char *why, int err) {
  if (good) ok(name, why);
  else fail(name, why, err);
}

// Evaluate the call FIRST, then read errno in a separate statement. Passing
// `errno` as a sibling argument to the call that sets it is unspecified-order
// in C (GCC on x86-64 reads it before the call), which reported the wrong
// error for every inline ioctl check on the first smoke run.
#define CHECK_CALL(expr, name, why) \
  do {                              \
    int _good = (expr);             \
    int _err = errno;               \
    check(_good, name, why, _err);  \
  } while (0)

static void info(const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  printf("         ");
  vprintf(fmt, ap);
  printf("\n");
  va_end(ap);
}

// ── test tone ───────────────────────────────────────────────────────────────

#define TONE_RATE 48000
#define TONE_HZ 440
#define TONE_MS 400
#define TONE_FRAMES (TONE_RATE * TONE_MS / 1000)

// Interleaved S16LE stereo sine, with a short fade at both ends so the
// start/stop do not click. The amplitude is moderate on purpose: a probe
// that is run repeatedly should not be painful.
static int16_t *make_tone(size_t *bytes) {
  size_t n = TONE_FRAMES;
  int16_t *pcm = calloc(n * 2, sizeof(int16_t));
  if (!pcm) return NULL;
  size_t fade = TONE_RATE / 100; // 10 ms
  for (size_t i = 0; i < n; i++) {
    double env = 1.0;
    if (i < fade) env = (double)i / fade;
    else if (n - i < fade) env = (double)(n - i) / fade;
    double s = sin(2.0 * M_PI * TONE_HZ * (double)i / TONE_RATE) * 0.35 * env;
    int16_t v = (int16_t)(s * 32767.0);
    pcm[2 * i] = v;
    pcm[2 * i + 1] = v;
  }
  *bytes = n * 2 * sizeof(int16_t);
  return pcm;
}

// ── devices ────────────────────────────────────────────────────────────────

static int node_present(const char *path, mode_t want_type) {
  struct stat st;
  if (stat(path, &st) != 0) return 0;
  return (st.st_mode & S_IFMT) == want_type;
}

static void dump_file(const char *path, const char *label) {
  FILE *f = fopen(path, "r");
  if (!f) {
    info("%s: not readable (%s)", label, strerror(errno));
    return;
  }
  char line[512];
  int lines = 0;
  while (fgets(line, sizeof line, f)) {
    size_t l = strlen(line);
    if (l && line[l - 1] == '\n') line[l - 1] = '\0';
    info("%s", line);
    if (++lines >= 60) {
      info("... (truncated)");
      break;
    }
  }
  fclose(f);
}

// Read a /proc file with as few read() calls as possible. procfs generates
// its text afresh on every read(2), so a stdio-sized read followed by another
// at an offset can land in a report whose earlier lines just changed length
// (rates), cutting or skipping a line -- seen as the tick-gap line vanishing
// from one of two consecutive reads. Returns the byte count, -1 on error.
static ssize_t read_whole(const char *path, char *buf, size_t size) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) return -1;
  size_t got = 0;
  while (got + 1 < size) {
    ssize_t n = read(fd, buf + got, size - 1 - got);
    if (n <= 0) break;
    got += (size_t)n;
  }
  close(fd);
  buf[got] = '\0';
  return (ssize_t)got;
}

// Lines of a file containing `needle`, for a kernel counter the section
// is about (e.g. the tick gaps in /proc/perf/kernel).
static void dump_matching(const char *path, const char *needle) {
  static char buf[65536];
  if (read_whole(path, buf, sizeof buf) < 0) {
    info("%s: not readable (%s)", path, strerror(errno));
    return;
  }
  int hits = 0;
  for (char *line = buf; line && *line;) {
    char *nl = strchr(line, '\n');
    if (nl) *nl = '\0';
    if (strstr(line, needle)) {
      info("%s: %s", path, line);
      hits++;
    }
    line = nl ? nl + 1 : NULL;
  }
  if (!hits) info("%s: no line mentions '%s' (older kernel)", path, needle);
}

// The kernel's counts of late timer-tick gaps (a CPU that took more than
// three tick periods between two ticks), from /proc/perf/kernel, split by
// whether the tick interrupted a busy CPU or its idle halt. -1 when the
// kernel does not report them. Read before and after a measurement, the
// differences say what the kernel itself saw meanwhile.
struct tick_gaps {
  long busy, idle;
};

static struct tick_gaps tick_gaps_late(void) {
  struct tick_gaps g = {-1, -1};
  static char buf[65536];
  if (read_whole("/proc/perf/kernel", buf, sizeof buf) < 0) return g;
  const char *p = strstr(buf, "timer tick gaps:");
  if (!p) return g;
  if (sscanf(p, "timer tick gaps: max %*f ms, %ld over", &g.busy) != 1) g.busy = -2;
  // "..., N on an idle one": the number just before that phrase.
  const char *q = strstr(p, " on an idle one");
  if (q) {
    const char *d = q;
    while (d > p && d[-1] >= '0' && d[-1] <= '9') d--;
    if (d < q) g.idle = atol(d);
  }
  return g;
}

// What the kernel's tick saw during a measurement. A late gap on a busy CPU
// is a CPU that stood still with work on it. A late gap on a halted CPU only
// matters when every CPU was halted and a deadline was due -- the tick of
// any awake CPU drains the shared timer heap -- so it is reported as such.
static void report_tick_gaps(const char *during, struct tick_gaps before, struct tick_gaps after) {
  if (before.busy < 0 || after.busy < 0) {
    info("kernel tick-gap counter not readable (before %ld, after %ld; -1 no /proc/perf/kernel line, -2 unparsed)", before.busy,
         after.busy);
    return;
  }
  long busy = after.busy - before.busy;
  long idle = (before.idle >= 0 && after.idle >= 0) ? after.idle - before.idle : 0;
  if (busy > 0)
    info("=> the kernel logged %ld late tick gap%s (>12 ms on a busy CPU) during %s: that CPU stood still -- host preemption of the vCPU or an interrupts-off stretch%s",
         busy, busy == 1 ? "" : "s", during, idle > 0 ? " (and late ticks on halted CPUs too)" : "");
  else if (idle > 0)
    info("%ld late tick gap%s on halted CPUs during %s, none on a busy one: harmless unless every CPU was asleep with a deadline due", idle,
         idle == 1 ? "" : "s", during);
  else
    info("no late tick gaps during %s: the kernel took its 4 ms ticks on time", during);
}

// Last `n` lines of a log: the daemon's own last words are the diagnosis,
// so they belong in the same paste as everything else.
static void dump_tail(const char *path, int n) {
  FILE *f = fopen(path, "r");
  if (!f) {
    info("%s: not readable (%s)", path, strerror(errno));
    return;
  }
  char ring[64][400];
  int count = 0, head = 0;
  char line[400];
  if (n > 64) n = 64;
  while (fgets(line, sizeof line, f)) {
    size_t l = strlen(line);
    if (l && line[l - 1] == '\n') line[l - 1] = '\0';
    snprintf(ring[head], sizeof ring[head], "%s", line);
    head = (head + 1) % n;
    if (count < n) count++;
  }
  fclose(f);
  if (count == 0) {
    info("%s: empty", path);
    return;
  }
  int start = (head - count + n) % n;
  info("%s (last %d line%s):", path, count, count == 1 ? "" : "s");
  for (int i = 0; i < count; i++) info("  %s", ring[(start + i) % n]);
}

static int file_has_prefix(const char *path, const char *prefix) {
  FILE *f = fopen(path, "r");
  if (!f) return 0;
  char line[512];
  size_t pl = strlen(prefix);
  int hit = 0;
  while (fgets(line, sizeof line, f)) {
    if (!strncmp(line, prefix, pl)) {
      hit = 1;
      break;
    }
  }
  fclose(f);
  return hit;
}

// mpg123's output drivers (output_*.so) as installed, on one line.
static void list_mpg123_modules(const char *dir) {
  DIR *d = opendir(dir);
  if (!d) {
    info("%s: %s -- mpg123 has no output modules here (-o pulse/alsa cannot load)", dir, strerror(errno));
    return;
  }
  char line[512] = "";
  size_t used = 0;
  struct dirent *e;
  int count = 0;
  while ((e = readdir(d))) {
    if (strncmp(e->d_name, "output_", 7)) continue;
    const char *name = e->d_name + 7;
    size_t nl = strlen(name);
    if (nl > 3 && !strcmp(name + nl - 3, ".so")) nl -= 3;
    if (used + nl + 2 < sizeof line) {
      memcpy(line + used, name, nl);
      used += nl;
      line[used++] = ' ';
      line[used] = '\0';
    }
    count++;
  }
  closedir(d);
  if (count == 0) info("%s: no output_*.so at all", dir);
  else info("mpg123 output modules in %s: %s", dir, line);
}

// pid of a process whose /proc/<pid>/comm is `name`, or -1.
static long find_process(const char *name) {
  DIR *d = opendir("/proc");
  if (!d) return -1;
  struct dirent *e;
  long found = -1;
  while ((e = readdir(d)) && found < 0) {
    if (e->d_name[0] < '0' || e->d_name[0] > '9') continue;
    // d_name is up to 255 bytes; size the path for it (pid dirs are short,
    // but the compiler cannot know that, and -Wformat-truncation is right).
    char path[sizeof "/proc//comm" + 256], comm[64];
    snprintf(path, sizeof path, "/proc/%s/comm", e->d_name);
    FILE *f = fopen(path, "r");
    if (!f) continue;
    if (fgets(comm, sizeof comm, f)) {
      size_t l = strlen(comm);
      if (l && comm[l - 1] == '\n') comm[l - 1] = '\0';
      if (!strcmp(comm, name)) found = atol(e->d_name);
    }
    fclose(f);
  }
  closedir(d);
  return found;
}

static void test_devices(void) {
  section("devices");
  char pcm[64], ctl[64], dsp[64];
  snprintf(pcm, sizeof pcm, "/dev/snd/pcmC%dD0p", g_card);
  snprintf(ctl, sizeof ctl, "/dev/snd/controlC%d", g_card);
  if (g_card == 0) snprintf(dsp, sizeof dsp, "/dev/dsp");
  else snprintf(dsp, sizeof dsp, "/dev/dsp%d", g_card);

  // Nodes the HDA driver creates at probe. Their absence means the PCI probe
  // failed (CORB/RIRB handshake, codec enumeration) -- see hda.rs -- and
  // nothing above can work; every later section is then skipped rather than
  // reported as its own failure.
  CHECK_CALL(node_present(pcm, S_IFCHR), "ALSA playback node exists", pcm);
  CHECK_CALL(node_present(ctl, S_IFCHR), "ALSA control node exists", ctl);
  CHECK_CALL(node_present(dsp, S_IFCHR), "OSS node exists", dsp);

  DIR *d = opendir("/dev/snd");
  if (d) {
    struct dirent *e;
    char names[512] = "";
    while ((e = readdir(d))) {
      if (e->d_name[0] == '.') continue;
      strncat(names, e->d_name, sizeof names - strlen(names) - 2);
      strncat(names, " ", sizeof names - strlen(names) - 1);
    }
    closedir(d);
    info("/dev/snd: %s", names[0] ? names : "(empty)");
  }

  // The codec and stream state read back out of the hardware: pin presence,
  // converter stream id/format, the descriptor's RUN bit and position.
  info("/proc/gpusnd:");
  dump_file("/proc/gpusnd", "/proc/gpusnd");
}

// ── kernel wake-up granularity ─────────────────────────────────────────────
//
// Every wait on an fd without a readiness bus -- the ALSA PCM and timer nodes
// among them -- is re-scanned by this kernel on its timer tick (250 Hz, 4 ms),
// so the tick IS the audio wake-up latency: PulseAudio writes its buffer,
// sleeps in poll(), and runs again only when the tick fires. Measured here
// with no audio involved at all: poll() with no fds and a 1 ms timeout, and
// nanosleep(1 ms), both return at the first tick after their deadline. A tick
// far above 4 ms is a kernel timer bug (the LAPIC count is derived from the
// TSC rate; QEMU and KVM clock the LAPIC timer at 1 GHz instead), and every
// audio number below inherits it. Also checks that the clock the probe
// measures with is fine-grained, so the numbers can be believed.

static long elapsed_us(const struct timespec *t0) {
  struct timespec t1;
  clock_gettime(CLOCK_MONOTONIC, &t1);
  return (t1.tv_sec - t0->tv_sec) * 1000000L + (t1.tv_nsec - t0->tv_nsec) / 1000;
}

static int cmp_long(const void *a, const void *b) {
  long x = *(const long *)a, y = *(const long *)b;
  return x < y ? -1 : x > y;
}

// min / median / max of `n` microsecond samples, printed as ms.
static void report_us(const char *what, long *v, int n, long *median_out) {
  qsort(v, n, sizeof v[0], cmp_long);
  *median_out = v[n / 2];
  info("%s: min %.1f ms, median %.1f ms, max %.1f ms (%d samples)", what, v[0] / 1000.0, v[n / 2] / 1000.0,
       v[n - 1] / 1000.0, n);
}

#define WAKE_SAMPLES 20
// One 4 ms tick plus a little scheduling slack.
#define WAKE_BOUND_US 6000
// Median overshoot of a multi-tick poll past its timeout: sub-millisecond
// when the tick is right, several ms when it is stretched.
#define TICK_OVERSHOOT_BOUND_US 2000

static int count_over(const long *v, int n, long bound) {
  int c = 0;
  for (int i = 0; i < n; i++)
    if (v[i] > bound) c++;
  return c;
}

static void test_wake(void) {
  section("wake");
  int stalls = 0;
  struct tick_gaps gaps_before = tick_gaps_late();
  struct timespec prev, cur;
  long maxstep_ns = 0;
  int distinct = 0;
  clock_gettime(CLOCK_MONOTONIC, &prev);
  for (int i = 0; i < 4000; i++) {
    clock_gettime(CLOCK_MONOTONIC, &cur);
    long d = (cur.tv_sec - prev.tv_sec) * 1000000000L + (cur.tv_nsec - prev.tv_nsec);
    if (d > 0) {
      distinct++;
      if (d > maxstep_ns) maxstep_ns = d;
    }
    prev = cur;
  }
  // Fine-grained means most consecutive reads differ; the largest gap is
  // reported, not judged: one gap of several ms in a loop of back-to-back
  // reads is the scheduler holding this thread (a tick's housekeeping), and
  // that is worth seeing, but it is not a coarse clock.
  check(distinct > 100, "clock_gettime(CLOCK_MONOTONIC) advances finely",
        "a coarse clock would report every latency here as 0 or as one step", 0);
  info("%d advances in 4000 reads, largest gap between two reads %.1f us", distinct, maxstep_ns / 1000.0);

  // 1. A deadline shorter than the tick is armed on the LAPIC directly, so
  //    this measures the arm path, not the tick: a mis-scaled LAPIC count
  //    stretches it by the TSC/LAPIC ratio only.
  long v[WAKE_SAMPLES], median;
  for (int i = 0; i < WAKE_SAMPLES; i++) {
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    poll(NULL, 0, 1);
    v[i] = elapsed_us(&t0);
  }
  report_us("poll(0 fds, 1 ms)", v, WAKE_SAMPLES, &median);
  check(median <= WAKE_BOUND_US, "poll(0 fds, 1 ms): a 1 ms deadline fires on time",
        "the armed-deadline path (sys_poll arms min(timeout, tick)); mis-scaled by the TSC/LAPIC ratio when the count is wrong", 0);
  stalls += count_over(v, WAKE_SAMPLES, WAKE_BOUND_US);

  for (int i = 0; i < WAKE_SAMPLES; i++) {
    struct timespec t0, req = {0, 1000000};
    clock_gettime(CLOCK_MONOTONIC, &t0);
    nanosleep(&req, NULL);
    v[i] = elapsed_us(&t0);
  }
  report_us("nanosleep(1 ms)", v, WAKE_SAMPLES, &median);
  check(median <= WAKE_BOUND_US, "nanosleep(1 ms) fires on time",
        "sleep-paced feeders (cubeb-alsa's refill loop, mpg123 -o oss) rest on this", 0);
  stalls += count_over(v, WAKE_SAMPLES, WAKE_BOUND_US);

  // 2. The re-scan tick itself. A timeout longer than the tick makes sys_poll
  //    arm min(remaining, 4 ms) pass after pass: the tick until the timeout
  //    is near, then the exact remainder. With a correctly scaled LAPIC the
  //    overshoot past the timeout is therefore a fraction of a millisecond
  //    (0.1-0.8 ms measured in QEMU); with the count off by a ratio r every
  //    pass lands r times late and the overshoot grows to several ms. Four
  //    timeouts that are not multiples of one another, and the median, so
  //    neither a lucky landing nor one preempted sample decides it.
  static const int timeouts_ms[] = {9, 13, 17, 21};
  long over[4 * 5], worst = 0;
  int n = 0;
  for (size_t t = 0; t < sizeof timeouts_ms / sizeof timeouts_ms[0]; t++) {
    for (int i = 0; i < 5; i++) {
      struct timespec t0;
      clock_gettime(CLOCK_MONOTONIC, &t0);
      poll(NULL, 0, timeouts_ms[t]);
      long o = elapsed_us(&t0) - timeouts_ms[t] * 1000L;
      over[n++] = o;
      if (o > worst) worst = o;
    }
  }
  report_us("poll(0 fds, 9/13/17/21 ms) overshoot past the timeout", over, n, &median);
  check(median <= TICK_OVERSHOOT_BOUND_US, "poll(0 fds, N > 4 ms) lands on its timeout, tick after tick",
        "the 4 ms tick every bus-less fd wait (pcm, timer) is re-scanned on: the audio wake-up latency", 0);
  if (median > TICK_OVERSHOOT_BOUND_US)
    info("=> re-scan passes land up to %.1f ms late: the tick is stretched (LAPIC count not calibrated?); Pulse's sink is woken that late",
         worst / 1000.0);
  stalls += count_over(over, n, WAKE_BOUND_US);

  // 3. The kernel's own view: gaps between consecutive ticks on one CPU. A
  //    good median with a few samples tens of ms late means this thread was
  //    not run for that long; if the kernel saw the same gap in its tick, the
  //    whole CPU stood still (a KVM vCPU the host descheduled, an
  //    interrupts-off section) -- if it did not, the delay is in the wake path.
  dump_matching("/proc/perf/kernel", "timer tick gaps");
  if (stalls)
    info("=> %d of %d samples stalled past %d ms: this thread was not run for that long (host preemption? kernel stall?)", stalls,
         2 * WAKE_SAMPLES + n, WAKE_BOUND_US / 1000);
  report_tick_gaps("these measurements", gaps_before, tick_gaps_late());
}

// ── OSS: /dev/dsp ──────────────────────────────────────────────────────────
//
// <sys/soundcard.h>, Linux _IOC encoding. Mirrors dsp.rs one for one.

#define SNDCTL_DSP_RESET 0x00005000u
#define SNDCTL_DSP_SYNC 0x00005001u
#define SNDCTL_DSP_SPEED 0xc0045002u
#define SNDCTL_DSP_STEREO 0xc0045003u
#define SNDCTL_DSP_GETBLKSIZE 0xc0045004u
#define SNDCTL_DSP_SETFMT 0xc0045005u
#define SNDCTL_DSP_CHANNELS 0xc0045006u
#define SNDCTL_DSP_GETFMTS 0x8004500bu
#define SNDCTL_DSP_GETOSPACE 0x8010500cu
#define AFMT_S16_LE 0x10

struct audio_buf_info {
  int fragments, fragstotal, fragsize, bytes;
};

static void test_oss(void) {
  section("oss");
  char dsp[64];
  if (g_card == 0) snprintf(dsp, sizeof dsp, "/dev/dsp");
  else snprintf(dsp, sizeof dsp, "/dev/dsp%d", g_card);
  if (!node_present(dsp, S_IFCHR)) {
    skip("open /dev/dsp", "no OSS node (driver probe failed)");
    return;
  }

  int fd = open(dsp, O_WRONLY);
  check(fd >= 0, "open(O_WRONLY)", "mpg123 -o oss, sox -t oss", errno);
  if (fd < 0) return;

  int fmt = AFMT_S16_LE;
  CHECK_CALL(ioctl(fd, SNDCTL_DSP_SETFMT, &fmt) == 0 && fmt == AFMT_S16_LE,
        "SNDCTL_DSP_SETFMT -> S16LE", "the only format the ring carries");
  int fmts = 0;
  CHECK_CALL(ioctl(fd, SNDCTL_DSP_GETFMTS, &fmts) == 0 && (fmts & AFMT_S16_LE),
        "SNDCTL_DSP_GETFMTS lists S16LE", "format capability");
  int ch = 2;
  CHECK_CALL(ioctl(fd, SNDCTL_DSP_CHANNELS, &ch) == 0 && ch == 2,
        "SNDCTL_DSP_CHANNELS -> 2", "stereo");
  int st = 1;
  CHECK_CALL(ioctl(fd, SNDCTL_DSP_STEREO, &st) == 0 && st == 1,
        "SNDCTL_DSP_STEREO -> 1", "legacy stereo flag");
  int rate = TONE_RATE;
  int r = ioctl(fd, SNDCTL_DSP_SPEED, &rate);
  check(r == 0 && rate == TONE_RATE, "SNDCTL_DSP_SPEED -> 48000", "set_params(48000, 2)", errno);
  if (r == 0 && rate != TONE_RATE) info("device took %d Hz", rate);
  int blk = 0;
  CHECK_CALL(ioctl(fd, SNDCTL_DSP_GETBLKSIZE, &blk) == 0 && blk > 0,
        "SNDCTL_DSP_GETBLKSIZE", "fragment size");
  struct audio_buf_info bi = {0};
  r = ioctl(fd, SNDCTL_DSP_GETOSPACE, &bi);
  check(r == 0 && bi.bytes > 0 && bi.fragsize > 0,
        "SNDCTL_DSP_GETOSPACE reports free space", "ring is empty and writable", errno);
  if (r == 0) info("ring: %d bytes free, %d/%d fragments of %d", bi.bytes, bi.fragments, bi.fragstotal, bi.fragsize);

  if (g_no_tone) {
    skip("write() a 440 Hz tone", "--no-tone");
  } else {
    size_t bytes;
    int16_t *pcm = make_tone(&bytes);
    if (!pcm) {
      fail("allocate tone", "calloc", errno);
    } else {
      // A blocking write of the whole tone: dsp.rs spin-retries a full ring,
      // so a short count means the ring made no progress (DMA not running).
      ssize_t w = write(fd, pcm, bytes);
      check(w == (ssize_t)bytes, "write() the whole tone", "ring drains at the PCM rate", errno);
      if (w >= 0 && w != (ssize_t)bytes) info("wrote %zd of %zu bytes", w, bytes);
      CHECK_CALL(ioctl(fd, SNDCTL_DSP_SYNC, 0) == 0, "SNDCTL_DSP_SYNC drains", "waits for queued_bytes == 0");
      free(pcm);
      info("if the tone was audible, the kernel -> HDA -> speaker path works");
    }
  }
  close(fd);
}

// ── ALSA PCM: /dev/snd/pcmC<card>D0p ───────────────────────────────────────
//
// include/uapi/sound/asound.h, x86_64. The kernel dispatches on
// (cmd >> 8) & 0xff == 'A' and cmd & 0xff == nr, exactly like Linux.

#define _IOC_(dir, type, nr, size) (((uint32_t)(dir) << 30) | ((uint32_t)(size) << 16) | ((uint32_t)(type) << 8) | (uint32_t)(nr))
#define IOC_NONE 0u
#define IOC_W 1u
#define IOC_R 2u
#define IOC_RW 3u

struct snd_interval {
  uint32_t min, max, flags;
};
struct snd_mask {
  uint32_t bits[8];
};
struct snd_pcm_hw_params {
  uint32_t flags;
  struct snd_mask masks[3];
  struct snd_mask mres[5];
  struct snd_interval intervals[12];
  struct snd_interval ires[9];
  uint32_t rmask, cmask, info, msbits, rate_num, rate_den;
  unsigned long fifo_size;
  unsigned char reserved[64];
};
struct snd_pcm_sw_params {
  int32_t tstamp_mode;
  uint32_t period_step, sleep_min;
  uint64_t avail_min, xfer_align, start_threshold, stop_threshold, silence_threshold, silence_size, boundary;
  uint32_t proto, tstamp_type;
  unsigned char reserved[56];
};
struct snd_xferi {
  int64_t result;
  uint64_t buf, frames;
};
struct snd_timespec_ {
  int64_t sec, nsec;
};
struct snd_pcm_status {
  int32_t state, _pad0;
  struct snd_timespec_ trigger_tstamp, tstamp;
  uint64_t appl_ptr, hw_ptr;
  int64_t delay;
  uint64_t avail, avail_max, overrange;
  int32_t suspended_state;
  uint32_t audio_tstamp_data;
  struct snd_timespec_ audio_tstamp, driver_tstamp;
  uint32_t audio_tstamp_accuracy;
  unsigned char reserved[20];
};

#define SNDRV_PCM_IOCTL_PVERSION _IOC_(IOC_R, 'A', 0x00, sizeof(int))
#define SNDRV_PCM_IOCTL_HW_REFINE _IOC_(IOC_RW, 'A', 0x10, sizeof(struct snd_pcm_hw_params))
#define SNDRV_PCM_IOCTL_HW_PARAMS _IOC_(IOC_RW, 'A', 0x11, sizeof(struct snd_pcm_hw_params))
#define SNDRV_PCM_IOCTL_SW_PARAMS _IOC_(IOC_RW, 'A', 0x13, sizeof(struct snd_pcm_sw_params))
#define SNDRV_PCM_IOCTL_STATUS _IOC_(IOC_R, 'A', 0x20, sizeof(struct snd_pcm_status))
#define SNDRV_PCM_IOCTL_DELAY _IOC_(IOC_R, 'A', 0x21, sizeof(int64_t))
#define SNDRV_PCM_IOCTL_PREPARE _IOC_(IOC_NONE, 'A', 0x40, 0)
#define SNDRV_PCM_IOCTL_DROP _IOC_(IOC_NONE, 'A', 0x43, 0)
#define SNDRV_PCM_IOCTL_DRAIN _IOC_(IOC_NONE, 'A', 0x44, 0)
#define SNDRV_PCM_IOCTL_WRITEI_FRAMES _IOC_(IOC_W, 'A', 0x50, sizeof(struct snd_xferi))

// hw_param indexes (masks are 0..2; intervals biased by FIRST_INTERVAL = 8).
#define HWP_ACCESS 0
#define HWP_FORMAT 1
#define HWP_SUBFORMAT 2
#define IV_SAMPLE_BITS 0
#define IV_FRAME_BITS 1
#define IV_CHANNELS 2
#define IV_RATE 3
#define IV_PERIOD_TIME 4
#define IV_PERIOD_SIZE 5
#define IV_PERIOD_BYTES 6
#define IV_PERIODS 7
#define IV_BUFFER_TIME 8
#define IV_BUFFER_SIZE 9
#define IV_BUFFER_BYTES 10
#define ACCESS_RW_INTERLEAVED 3
#define FORMAT_S16_LE 2
#define SUBFORMAT_STD 0
#define INTERVAL_INTEGER 4
#define PCM_STATE_PREPARED 2
#define PCM_STATE_RUNNING 3

// snd_pcm_hw_params_any(): every mask full, every interval wide open. This is
// the FIRST thing alsa-lib sends, and the one refine the kernel treats as a
// real fault when it comes back empty ("ALSA sees no configs").
static void hw_params_any(struct snd_pcm_hw_params *p) {
  memset(p, 0, sizeof *p);
  for (int m = 0; m < 3; m++)
    for (int w = 0; w < 8; w++) p->masks[m].bits[w] = 0xffffffffu;
  for (int i = 0; i < 12; i++) {
    p->intervals[i].min = 0;
    p->intervals[i].max = 0xffffffffu;
    p->intervals[i].flags = 0;
  }
  p->rmask = 0xffffffffu;
}

static void iv_set(struct snd_pcm_hw_params *p, int iv, uint32_t v) {
  p->intervals[iv].min = v;
  p->intervals[iv].max = v;
  p->intervals[iv].flags = INTERVAL_INTEGER;
}

static void mask_only(struct snd_pcm_hw_params *p, int m, unsigned bit) {
  memset(&p->masks[m], 0, sizeof p->masks[m]);
  p->masks[m].bits[bit / 32] = 1u << (bit % 32);
}

static int g_alsa_pcm_ok;
static int pcm_open_prepared(int oflags, unsigned *period, unsigned *buffer);

static void test_alsa_pcm(void) {
  section("alsa-pcm");
  char pcm[64];
  snprintf(pcm, sizeof pcm, "/dev/snd/pcmC%dD0p", g_card);
  if (!node_present(pcm, S_IFCHR)) {
    skip("open pcm node", "no ALSA playback node (driver probe failed)");
    return;
  }

  int fail_at_entry = g_fail;
  int fd = open(pcm, O_WRONLY);
  check(fd >= 0, "open(O_WRONLY)", "snd_pcm_open(SND_PCM_STREAM_PLAYBACK)", errno);
  if (fd < 0) return;

  int ver = 0;
  int r = ioctl(fd, SNDRV_PCM_IOCTL_PVERSION, &ver);
  check(r == 0 && (ver >> 16) == 2, "PVERSION is 2.x", "alsa-lib refuses other majors", errno);
  if (r == 0) info("SNDRV_PCM_VERSION %d.%d.%d", ver >> 16, (ver >> 8) & 0xff, ver & 0xff);

  // 1. hw_params_any + HW_REFINE. If THIS comes back EINVAL the device
  //    advertises no configuration at all and every ALSA client fails in
  //    snd_pcm_hw_params_any().
  struct snd_pcm_hw_params hp;
  hw_params_any(&hp);
  r = ioctl(fd, SNDRV_PCM_IOCTL_HW_REFINE, &hp);
  check(r == 0, "HW_REFINE(any) has a non-empty result", "snd_pcm_hw_params_any()", errno);
  if (r == 0) {
    info("refined: rate %u..%u, channels %u..%u, period_size %u..%u, buffer_size %u..%u",
         hp.intervals[IV_RATE].min, hp.intervals[IV_RATE].max,
         hp.intervals[IV_CHANNELS].min, hp.intervals[IV_CHANNELS].max,
         hp.intervals[IV_PERIOD_SIZE].min, hp.intervals[IV_PERIOD_SIZE].max,
         hp.intervals[IV_BUFFER_SIZE].min, hp.intervals[IV_BUFFER_SIZE].max);
    check(hp.masks[HWP_FORMAT].bits[0] & (1u << FORMAT_S16_LE), "refine keeps S16_LE in the format mask", "format capability", 0);
    check(hp.masks[HWP_ACCESS].bits[0] & (1u << ACCESS_RW_INTERLEAVED), "refine keeps RW_INTERLEAVED", "access capability", 0);
    check(hp.intervals[IV_RATE].min <= TONE_RATE && hp.intervals[IV_RATE].max >= TONE_RATE,
          "48000 Hz is inside the refined rate range", "cubeb asks for the device's preferred rate", 0);
  }

  // 2. Narrow to what cubeb/Pulse pick (S16LE, interleaved, stereo, 48 kHz)
  //    and install. period/buffer are left open on purpose: install() derives
  //    a coherent triple the way Linux's snd_pcm_hw_params does, which is what
  //    a client that only pins format/rate/channels relies on.
  hw_params_any(&hp);
  mask_only(&hp, HWP_ACCESS, ACCESS_RW_INTERLEAVED);
  mask_only(&hp, HWP_FORMAT, FORMAT_S16_LE);
  mask_only(&hp, HWP_SUBFORMAT, SUBFORMAT_STD);
  iv_set(&hp, IV_CHANNELS, 2);
  iv_set(&hp, IV_RATE, TONE_RATE);
  r = ioctl(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &hp);
  check(r == 0, "HW_PARAMS S16LE/2ch/48000 installs", "snd_pcm_hw_params()", errno);
  uint32_t period = 0, buffer = 0;
  if (r == 0) {
    period = hp.intervals[IV_PERIOD_SIZE].min;
    buffer = hp.intervals[IV_BUFFER_SIZE].min;
    info("granted: rate %u, period %u frames, buffer %u frames, periods %u",
         hp.intervals[IV_RATE].min, period, buffer, hp.intervals[IV_PERIODS].min);
    check(hp.intervals[IV_RATE].min == TONE_RATE, "granted rate is 48000", "no silent resample", 0);
    check(hp.intervals[IV_CHANNELS].min == 2 && hp.intervals[IV_SAMPLE_BITS].min == 16 && hp.intervals[IV_FRAME_BITS].min == 32,
          "granted frame is 2ch x 16 bit", "frame_bits = 32", 0);
    check(period >= 1 && buffer >= period && hp.intervals[IV_PERIOD_BYTES].min == period * 4 && hp.intervals[IV_BUFFER_BYTES].min == buffer * 4,
          "period/buffer sizes are coherent singletons", "install() hands back exact values", 0);
  }

  // 3. SW_PARAMS, then PREPARE.
  struct snd_pcm_sw_params sw;
  memset(&sw, 0, sizeof sw);
  sw.avail_min = period ? period : 1024;
  sw.start_threshold = 1;
  sw.stop_threshold = buffer ? buffer : 16384;
  sw.boundary = 0x4000000000000000ull;
  CHECK_CALL(ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw) == 0, "SW_PARAMS", "snd_pcm_sw_params()");
  CHECK_CALL(ioctl(fd, SNDRV_PCM_IOCTL_PREPARE, 0) == 0, "PREPARE", "snd_pcm_prepare()");

  struct snd_pcm_status st;
  memset(&st, 0, sizeof st);
  r = ioctl(fd, SNDRV_PCM_IOCTL_STATUS, &st);
  check(r == 0 && st.state == PCM_STATE_PREPARED, "STATUS reports PREPARED", "state machine", errno);
  if (r == 0) info("status: state %d, avail %llu, hw_ptr %llu, appl_ptr %llu",
                   st.state, (unsigned long long)st.avail, (unsigned long long)st.hw_ptr, (unsigned long long)st.appl_ptr);

  // 4. WRITEI_FRAMES: the data path. cubeb-alsa and aplay both end up here.
  if (g_no_tone) {
    skip("WRITEI_FRAMES a 440 Hz tone", "--no-tone");
  } else {
    size_t bytes;
    int16_t *pcm_buf = make_tone(&bytes);
    if (!pcm_buf) {
      fail("allocate tone", "calloc", errno);
    } else {
      // Feed in period-sized chunks the way snd_pcm_writei does, so the ring
      // limit (buffer_size) is respected and a stall shows up as a short
      // result instead of a hang.
      uint64_t total = TONE_FRAMES, done = 0;
      uint64_t chunk = period ? period : 1024;
      int stalled = 0;
      while (done < total) {
        struct snd_xferi x;
        x.result = 0;
        x.buf = (uint64_t)(uintptr_t)(pcm_buf + done * 2);
        x.frames = (total - done) < chunk ? (total - done) : chunk;
        if (ioctl(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x) != 0) {
          fail("WRITEI_FRAMES", "snd_pcm_writei()", errno);
          stalled = 1;
          break;
        }
        if (x.result <= 0) {
          fail("WRITEI_FRAMES made progress", "writei stalled: ring not draining (DMA not running?)", 0);
          stalled = 1;
          break;
        }
        done += (uint64_t)x.result;
      }
      if (!stalled) ok("WRITEI_FRAMES the whole tone", "snd_pcm_writei()");

      memset(&st, 0, sizeof st);
      if (ioctl(fd, SNDRV_PCM_IOCTL_STATUS, &st) == 0) {
        check(st.state == PCM_STATE_RUNNING, "STATUS reports RUNNING after data", "the driver starts on first data", 0);
      }
      int64_t delay = -1;
      if (ioctl(fd, SNDRV_PCM_IOCTL_DELAY, &delay) == 0) info("DELAY: %lld frames still queued", (long long)delay);

      // poll(POLLOUT) is how PulseAudio (tsched=0 -> snd_pcm_wait) and
      // cubeb-alsa learn there is room for the next period. The ring was just
      // filled, so this measures the WAKE: the PCM node has no readiness
      // bus, and the kernel re-scans such an fd every 4 ms (IO_WAIT_TICK_MS),
      // which is what bounds the latency; a 256 ms ring cannot underrun on
      // that. A timeout (0) or an error here means poll-driven feeders stall.
      {
        struct pollfd pf = {fd, POLLOUT, 0};
        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        int pr = poll(&pf, 1, 1000);
        int perr = errno;
        clock_gettime(CLOCK_MONOTONIC, &t1);
        long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
        check(pr == 1 && (pf.revents & POLLOUT), "poll(POLLOUT) wakes with room in the ring",
              "snd_pcm_wait(): pulse tsched=0 and cubeb-alsa feed on this", pr < 0 ? perr : 0);
        info("poll(POLLOUT) returned in %ld ms (bus-less fd: kernel re-scan tick is 4 ms)", ms);
        if (pr == 1 && ms > 50) info("NOTE: %ld ms is far above the 4 ms tick -- poll-driven feeders would stutter", ms);
      }
      CHECK_CALL(ioctl(fd, SNDRV_PCM_IOCTL_DRAIN, 0) == 0, "DRAIN", "snd_pcm_drain() waits for the ring to empty");
      free(pcm_buf);
      info("if the tone was audible, the ALSA path cubeb-alsa/Pulse use works");
    }
  }
  close(fd);

  // 5. PulseAudio opens the hw PCM nonblocking and expects snd_pcm_writei() to
  //    return what fits right now (or EAGAIN), never spin in-kernel waiting
  //    for the whole request. A sink thread stuck in one WRITEI_FRAMES call
  //    never gets back to snd_pcm_avail()/poll() to refill after an underrun.
  unsigned nb_buffer = 0;
  int nfd = pcm_open_prepared(O_WRONLY | O_NONBLOCK, &(unsigned){0}, &nb_buffer);
  check(nfd >= 0, "open/prep O_NONBLOCK PCM for the short-write check",
        "PulseAudio's module-alsa-sink opens hw:0,0 with SND_PCM_NONBLOCK", nfd < 0 ? -nfd : 0);
  if (nfd >= 0) {
    size_t fill_bytes = (size_t)(nb_buffer ? nb_buffer : 4096) * 4;
    int16_t *silence = calloc(fill_bytes / sizeof *silence, sizeof *silence);
    if (!silence) {
      fail("allocate silence buffer", "calloc", errno);
    } else {
      struct snd_xferi fill = {0, (uint64_t)(uintptr_t)silence, nb_buffer ? nb_buffer : 4096};
      int fr = ioctl(nfd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &fill);
      int fe = errno;
      check(fr == 0 && fill.result == (nb_buffer ? (int64_t)nb_buffer : 4096),
            "nonblocking WRITEI fills one empty buffer", "baseline for the immediate short-write/EAGAIN check", fr != 0 ? fe : 0);

      struct snd_xferi extra = {0, (uint64_t)(uintptr_t)silence, nb_buffer ? nb_buffer : 4096};
      struct timespec t0;
      clock_gettime(CLOCK_MONOTONIC, &t0);
      int er = ioctl(nfd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &extra);
      int ee = errno;
      long ms = elapsed_us(&t0) / 1000;
      check(ms < 500, "nonblocking WRITEI returns promptly on a full ring",
            "Pulse's sink thread must get back to snd_pcm_avail()/poll(), not spin in one ioctl for seconds", 0);
      check((er < 0 && ee == EAGAIN) || (er == 0 && extra.result > 0 && (uint64_t)extra.result < extra.frames),
            "nonblocking WRITEI returns EAGAIN or a short count when the ring is full",
            "Linux-style semantics for snd_pcm_writei() on SND_PCM_NONBLOCK fds", er < 0 ? ee : 0);
      info("second O_NONBLOCK WRITEI returned in %ld ms with %s%lld frame%s", ms,
           er < 0 ? "errno " : "",
           er < 0 ? (long long)ee : (long long)extra.result,
           (er == 0 && extra.result == 1) ? "" : "s");
      ioctl(nfd, SNDRV_PCM_IOCTL_DROP, 0);
      free(silence);
    }
    close(nfd);
  }
  // Only a sequence that actually ran and passed counts: a skipped section
  // (node absent) must not read as a working backend.
  g_alsa_pcm_ok = (g_fail == fail_at_entry);
}

// ── ALSA timer: /dev/snd/timer ─────────────────────────────────────────────
//
// alsa-lib opens this node the moment a client enables period_event in its
// sw_params (pcm_hw.c snd_pcm_hw_change_timer). PulseAudio's module-alsa-sink
// does exactly that for every tsched=0 sink (alsa-util.c pa_alsa_set_sw_params,
// period_event = !use_tsched), so on a kernel without the node the sink dies
// at "Unable to set sw params: No such file or directory" -- the errno of
// that open(2), reported through the sw_params call. The SW_PARAMS ioctl in
// [alsa-pcm] never sees it: period_event lives in alsa-lib, the kernel only
// sees the timer. Mirrored here call for call: open O_RDONLY|O_NONBLOCK,
// PVERSION, TREAD, SELECT the PCM's timer, PARAMS (auto, 1 tick, TICK filter),
// START; then, with the PCM running, the fd must go POLLIN once per period
// and read() hand back tread records the way snd_pcm_hw_clear_timer_queue
// drains them. Pulse's thread polls the PCM fd and this one side by side and
// snd_pcm_hw_poll_revents turns POLLIN here into POLLOUT on the PCM.

struct snd_timer_id {
  int32_t dev_class, dev_sclass, card, device, subdevice;
};
struct snd_timer_select {
  struct snd_timer_id id;
  unsigned char reserved[32];
};
struct snd_timer_params {
  uint32_t flags, ticks, queue_size, reserved0, filter;
  unsigned char reserved[60];
};
struct snd_timer_status {
  struct snd_timespec_ tstamp;
  uint32_t resolution, lost, overrun, queue;
  unsigned char reserved[64];
};
struct snd_timer_tread {
  int32_t event;
  uint32_t pad1;
  struct snd_timespec_ tstamp;
  uint32_t val, pad2;
};

#define SNDRV_TIMER_IOCTL_PVERSION _IOC_(IOC_R, 'T', 0x00, sizeof(int))
#define SNDRV_TIMER_IOCTL_TREAD_OLD _IOC_(IOC_W, 'T', 0x02, sizeof(int))
#define SNDRV_TIMER_IOCTL_SELECT _IOC_(IOC_W, 'T', 0x10, sizeof(struct snd_timer_select))
#define SNDRV_TIMER_IOCTL_PARAMS _IOC_(IOC_W, 'T', 0x12, sizeof(struct snd_timer_params))
#define SNDRV_TIMER_IOCTL_STATUS _IOC_(IOC_R, 'T', 0x14, sizeof(struct snd_timer_status))
#define SNDRV_TIMER_IOCTL_START _IOC_(IOC_NONE, 'T', 0xa0, 0)
#define SNDRV_TIMER_IOCTL_STOP _IOC_(IOC_NONE, 'T', 0xa1, 0)
#define SNDRV_PCM_IOCTL_START _IOC_(IOC_NONE, 'A', 0x42, 0)
#define SNDRV_PCM_IOCTL_DROP _IOC_(IOC_NONE, 'A', 0x43, 0)
#define TIMER_CLASS_PCM 3
#define TIMER_PSFLG_AUTO 1u
#define TIMER_EVENT_TICK 1
#define TIMER_EVENT_MSUSPEND 17
#define TIMER_EVENT_MRESUME 18

// Quiet counterpart of the [alsa-pcm] sequence: open, S16LE/2ch/48000 with
// the sizes left to the kernel, Pulse's sw_params (avail_min 1, never
// auto-start), PREPARE. Returns the fd or -errno; granted sizes in frames.
static int pcm_open_prepared(int oflags, unsigned *period, unsigned *buffer) {
  char path[64];
  snprintf(path, sizeof path, "/dev/snd/pcmC%dD0p", g_card);
  int fd = open(path, oflags);
  if (fd < 0) return -errno;
  struct snd_pcm_hw_params hp;
  hw_params_any(&hp);
  mask_only(&hp, HWP_ACCESS, ACCESS_RW_INTERLEAVED);
  mask_only(&hp, HWP_FORMAT, FORMAT_S16_LE);
  mask_only(&hp, HWP_SUBFORMAT, SUBFORMAT_STD);
  iv_set(&hp, IV_CHANNELS, 2);
  iv_set(&hp, IV_RATE, TONE_RATE);
  // Pulse's sink: fragment_size=4800 bytes = 1200 frames = 25 ms at 48 kHz.
  // The tick test measures wake-up latency against this period; the 128-frame
  // minimum the kernel would otherwise pick is 2.7 ms, shorter than a line of
  // serial output, and ticks then land between two consecutive read()s.
  iv_set(&hp, IV_PERIOD_SIZE, 1200);
  if (ioctl(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &hp) != 0) {
    int e = errno;
    close(fd);
    return -e;
  }
  *period = hp.intervals[IV_PERIOD_SIZE].min;
  *buffer = hp.intervals[IV_BUFFER_SIZE].min;
  struct snd_pcm_sw_params sw;
  memset(&sw, 0, sizeof sw);
  sw.avail_min = 1;
  sw.start_threshold = 0x4000000000000000ull; // (snd_pcm_uframes_t)-1 clipped to boundary
  sw.stop_threshold = 0x4000000000000000ull;
  sw.boundary = 0x4000000000000000ull;
  if (ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw) != 0 || ioctl(fd, SNDRV_PCM_IOCTL_PREPARE, 0) != 0) {
    int e = errno;
    close(fd);
    return -e;
  }
  return fd;
}

static long elapsed_ms(const struct timespec *t0) {
  struct timespec t1;
  clock_gettime(CLOCK_MONOTONIC, &t1);
  return (t1.tv_sec - t0->tv_sec) * 1000 + (t1.tv_nsec - t0->tv_nsec) / 1000000;
}

static void test_alsa_timer(void) {
  section("alsa-timer");
  char pcm[64];
  snprintf(pcm, sizeof pcm, "/dev/snd/pcmC%dD0p", g_card);
  if (!node_present(pcm, S_IFCHR)) {
    skip("open /dev/snd/timer", "no ALSA playback node to bind a period timer to");
    return;
  }
  int tfd = open("/dev/snd/timer", O_RDONLY | O_NONBLOCK);
  int e = errno;
  check(tfd >= 0, "open(/dev/snd/timer, O_RDONLY|O_NONBLOCK)",
        "snd_timer_hw_open(): alsa-lib's period_event timer, opened from snd_pcm_sw_params for Pulse's tsched=0 sink", e);
  if (tfd < 0) {
    if (e == ENOENT)
      info("=> exactly module-alsa-sink's 'Unable to set sw params: No such file or directory': no sink, no sound");
    return;
  }

  int ver = 0;
  CHECK_CALL(ioctl(tfd, SNDRV_TIMER_IOCTL_PVERSION, &ver) == 0 && (ver >> 16) == 2, "PVERSION is 2.x",
             "SNDRV_TIMER_VERSION_MAX gate in snd_timer_hw_open()");
  info("SNDRV_TIMER_VERSION %d.%d.%d%s", ver >> 16, (ver >> 8) & 0xff, ver & 0xff,
       ver < 0x20005 ? " (below 2.0.5: alsa-lib would use pause/continue events and poll before read)" : "");
  int one = 1;
  CHECK_CALL(ioctl(tfd, SNDRV_TIMER_IOCTL_TREAD_OLD, &one) == 0, "TREAD enables timestamped records",
             "SND_TIMER_OPEN_TREAD: alsa-lib asks for it first and only falls back without it");

  struct snd_timer_select sel;
  memset(&sel, 0, sizeof sel);
  sel.id.dev_class = TIMER_CLASS_PCM;
  sel.id.card = g_card;
  sel.id.device = 0;
  sel.id.subdevice = 0; // (subdevice << 1) | stream, playback = 0
  CHECK_CALL(ioctl(tfd, SNDRV_TIMER_IOCTL_SELECT, &sel) == 0, "SELECT binds the PCM playback timer",
             "class PCM, card, device 0, subdevice<<1|stream -- ENODEV here = no timer for this PCM");

  struct snd_timer_params tp;
  memset(&tp, 0, sizeof tp);
  tp.flags = TIMER_PSFLG_AUTO;
  tp.ticks = 1;
  tp.filter = (1u << TIMER_EVENT_TICK) | (1u << TIMER_EVENT_MSUSPEND) | (1u << TIMER_EVENT_MRESUME);
  CHECK_CALL(ioctl(tfd, SNDRV_TIMER_IOCTL_PARAMS, &tp) == 0, "PARAMS auto-start, 1 tick, TICK filter",
             "snd_timer_params() with what snd_pcm_hw_change_timer sets");
  CHECK_CALL(ioctl(tfd, SNDRV_TIMER_IOCTL_START, 0) == 0, "START", "snd_timer_start()");

  // A PCM timer ticks only while its stream runs: nothing may be queued yet.
  struct snd_timer_tread tr[4];
  ssize_t n = read(tfd, tr, sizeof tr);
  e = errno;
  check(n < 0 && e == EAGAIN, "read() before the stream runs -> EAGAIN", "a PCM timer ticks only while the PCM runs",
        n < 0 && e != EAGAIN ? e : 0);
  if (n >= 0) info("read() returned %zd bytes with no stream running", n);

  // Now run the PCM (silence, one buffer) and expect the tick.
  unsigned period = 0, buffer = 0;
  int pfd = pcm_open_prepared(O_WRONLY, &period, &buffer);
  check(pfd >= 0, "PCM opened, configured and PREPAREd for the tick test", "same hw_params as [alsa-pcm]", pfd < 0 ? -pfd : 0);
  if (pfd >= 0) {
    long period_ms = period ? (long)period * 1000 / TONE_RATE : 0;
    info("period %u frames = %ld ms, buffer %u frames", period, period_ms, buffer);
    size_t frames = buffer ? buffer : 4096;
    int16_t *silence = calloc(frames * 2, sizeof(int16_t));
    struct snd_xferi x = {0, (uint64_t)(uintptr_t)silence, frames};
    int wr = silence ? ioctl(pfd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x) : -1;
    int we = errno;
    // Pulse's sw_params never auto-start: it calls snd_pcm_start after
    // the first write (alsa-sink.c "Starting playback."). Mirror that.
    int st = wr == 0 ? ioctl(pfd, SNDRV_PCM_IOCTL_START, 0) : -1;
    int se = errno;
    check(wr == 0 && st == 0, "WRITEI a buffer of silence, then START", "snd_pcm_writei + snd_pcm_start as alsa-sink.c does",
          wr != 0 ? we : se);

    // alsa-lib's poll_descriptors: [pcm POLLOUT, timer POLLIN]. Pulse lives
    // on the cadence of these wakes -- one per period -- so four of them are
    // taken: the first from START (it includes the stream's ramp-up), then
    // the intervals between consecutive wakes, each divided by the ticks the
    // record carries (a stalled wake coalesces two). Judged on the median
    // per-tick interval, so one stalled wake (a host preemption, a kernel
    // stall) is reported, not fatal: Pulse's 100 ms buffer absorbs one.
    long bound = period_ms + 20;
    long limit = period_ms * 2 + 100;
    struct tick_gaps gaps_before = tick_gaps_late();
    long wake_ms[4];
    unsigned wake_val[4];
    int wakes = 0, timed_out = 0;
    ssize_t first_n = -1, first_n2 = -1;
    int first_e = 0, first_e2 = 0;
    struct snd_timer_tread first_tr = {0, 0, {0, 0}, 0, 0};
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (int w = 0; w < 4; w++) {
      struct pollfd pf[2] = {{pfd, POLLOUT, 0}, {tfd, POLLIN, 0}};
      long start = elapsed_ms(&t0);
      int pr = poll(pf, 2, (int)limit);
      if (pr > 0 && !(pf[1].revents & POLLIN)) {
        // The PCM reports POLLOUT a little before the period boundary the
        // timer counts; wait on the timer alone for the rest of the window.
        struct pollfd pt = {tfd, POLLIN, 0};
        long left = limit - (elapsed_ms(&t0) - start);
        pr = poll(&pt, 1, (int)(left > 0 ? left : 1));
        if (!(pr > 0 && (pt.revents & POLLIN))) pr = 0;
      }
      if (pr <= 0) {
        timed_out = 1;
        break;
      }
      wake_ms[wakes] = elapsed_ms(&t0);
      n = read(tfd, tr, sizeof tr);
      e = errno;
      if (w == 0) {
        // The second read follows immediately: printing first would let
        // another period elapse and turn a drained queue into a fresh tick.
        struct snd_timer_tread again[4];
        first_n2 = read(tfd, again, sizeof again);
        first_e2 = errno;
        first_n = n;
        first_e = e;
        if (n > 0) first_tr = tr[0];
      }
      wake_val[wakes] = n >= (ssize_t)sizeof tr[0] ? tr[0].val : 0;
      wakes++;
    }
    long per_tick[3];
    int intervals = 0, stalled = 0;
    for (int i = 1; i < wakes; i++) {
      long dt = wake_ms[i] - wake_ms[i - 1];
      long v = wake_val[i] ? (long)wake_val[i] : 1;
      per_tick[intervals++] = dt / v;
      if (dt > bound * v) stalled++;
    }
    long med = 0;
    if (intervals) {
      long sorted[3];
      memcpy(sorted, per_tick, sizeof(long) * intervals);
      qsort(sorted, intervals, sizeof sorted[0], cmp_long);
      med = sorted[intervals / 2];
    }
    check(wakes == 4 && med <= bound, "poll(POLLIN) on the timer wakes once per period",
          "snd_pcm_hw_poll_revents: POLLIN here becomes POLLOUT for Pulse's unix_write, one wake per period", 0);
    if (wakes)
      info("first timer POLLIN %ld ms after START (period %ld ms + 4 ms tick; %u tick%s in the record)", wake_ms[0], period_ms,
           wake_val[0], wake_val[0] == 1 ? "" : "s");
    for (int i = 1; i < wakes; i++) {
      long dt = wake_ms[i] - wake_ms[i - 1];
      long v = wake_val[i] ? (long)wake_val[i] : 1;
      info("wake %d: +%ld ms, %u tick%s -> %ld ms per period%s", i, dt, wake_val[i], wake_val[i] == 1 ? "" : "s", per_tick[i - 1],
           dt > bound * v ? "  (STALLED)" : "");
    }
    if (timed_out) info("=> a wake never came within %ld ms: the timer stopped ticking", limit);
    else if (med > bound) info("=> later than one period, wake after wake: Pulse would see late wake-ups (audible as stutter)");
    else if (stalled)
      info("=> %d of %d wakes stalled (Pulse's 100 ms buffer absorbs one; see [wake] and the tick gaps in /proc/perf/kernel)", stalled,
           wakes - 1);
    check(first_n >= (ssize_t)sizeof tr[0] && first_n % (ssize_t)sizeof tr[0] == 0 && first_tr.event == TIMER_EVENT_TICK &&
              first_tr.val >= 1,
          "read() returns a TICK tread record", "32-byte {event, tstamp, val}; alsa-lib reads and discards up to 4",
          first_n < 0 ? first_e : 0);
    if (first_n > 0)
      info("%zd byte%s: event %d val %u (ticks elapsed) at %lld.%09lld", first_n, first_n == 1 ? "" : "s", first_tr.event, first_tr.val,
           (long long)first_tr.tstamp.sec, (long long)first_tr.tstamp.nsec);
    check(first_n2 < 0 && first_e2 == EAGAIN, "read() again -> EAGAIN (queue drained)",
          "snd_pcm_hw_clear_timer_queue reads once; a queue that never empties would spin Pulse", first_n2 < 0 && first_e2 != EAGAIN ? first_e2 : 0);
    if (first_n2 > 0) info("second read() returned %zd bytes: a period elapsed between the two reads", first_n2);
    report_tick_gaps("the timer test", gaps_before, tick_gaps_late());
    // The driver's own timing of its position-register reads: under a
    // hypervisor each is a VM exit, and one that waited on the host shows.
    dump_matching("/proc/gpusnd", "[gpusnd] position reads");

    struct snd_timer_status ts;
    memset(&ts, 0, sizeof ts);
    if (ioctl(tfd, SNDRV_TIMER_IOCTL_STATUS, &ts) == 0)
      info("STATUS: resolution %u ns (= period), overrun %u, queued %u", ts.resolution, ts.overrun, ts.queue);
    CHECK_CALL(ioctl(tfd, SNDRV_TIMER_IOCTL_STOP, 0) == 0, "STOP", "snd_timer_stop() on snd_pcm_close");
    ioctl(pfd, SNDRV_PCM_IOCTL_DROP, 0);
    free(silence);
    close(pfd);
  }
  close(tfd);
}

// ── ALSA control: /dev/snd/controlC<card> ──────────────────────────────────

struct snd_ctl_card_info {
  int32_t card, _pad;
  unsigned char id[16], driver[16], name[32], longname[80], reserved_[16], mixername[80], components[128];
};
struct snd_ctl_elem_id {
  uint32_t numid;
  int32_t iface;
  uint32_t device, subdevice;
  unsigned char name[44];
  uint32_t index;
};
struct snd_ctl_elem_list {
  uint32_t offset, space, used, count;
  uint64_t pids;
  unsigned char reserved[50];
};
struct snd_ctl_elem_info {
  struct snd_ctl_elem_id id;
  int32_t type;
  uint32_t access, count;
  int32_t owner;
  int64_t min, max, step;
  unsigned char value_pad[128 - 24];
  unsigned char dimen_reserved[64];
};
struct snd_ctl_elem_value {
  struct snd_ctl_elem_id id;
  uint32_t indirect, _pad;
  int64_t values[2];
};

#define SNDRV_CTL_IOCTL_PVERSION _IOC_(IOC_R, 'U', 0x00, sizeof(int))
#define SNDRV_CTL_IOCTL_CARD_INFO _IOC_(IOC_R, 'U', 0x01, sizeof(struct snd_ctl_card_info))
#define SNDRV_CTL_IOCTL_ELEM_LIST _IOC_(IOC_RW, 'U', 0x10, sizeof(struct snd_ctl_elem_list))
#define SNDRV_CTL_IOCTL_ELEM_INFO _IOC_(IOC_RW, 'U', 0x11, sizeof(struct snd_ctl_elem_info))
#define SNDRV_CTL_IOCTL_ELEM_READ _IOC_(IOC_RW, 'U', 0x12, sizeof(struct snd_ctl_elem_value))
#define CTL_IFACE_MIXER 2
#define CTL_ELEM_INTEGER 2

static void test_alsa_ctl(void) {
  section("alsa-ctl");
  char ctl[64];
  snprintf(ctl, sizeof ctl, "/dev/snd/controlC%d", g_card);
  if (!node_present(ctl, S_IFCHR)) {
    skip("open control node", "no ALSA control node (driver probe failed)");
    return;
  }
  int fd = open(ctl, O_RDWR);
  check(fd >= 0, "open(O_RDWR)", "snd_ctl_open()", errno);
  if (fd < 0) return;

  int ver = 0;
  CHECK_CALL(ioctl(fd, SNDRV_CTL_IOCTL_PVERSION, &ver) == 0 && (ver >> 16) == 2, "PVERSION is 2.x", "snd_ctl_open() version gate");

  struct snd_ctl_card_info ci;
  memset(&ci, 0, sizeof ci);
  int r = ioctl(fd, SNDRV_CTL_IOCTL_CARD_INFO, &ci);
  check(r == 0 && ci.id[0] && ci.name[0], "CARD_INFO names the card", "aplay -l, Pulse module-alsa-card", errno);
  if (r == 0) info("card %d: id '%s' driver '%s' name '%s' mixer '%s'", ci.card, ci.id, ci.driver, ci.name, ci.mixername);

  // ELEM_LIST twice like alsa-lib: once with space=0 to learn the count,
  // then with room for the ids.
  struct snd_ctl_elem_list el;
  memset(&el, 0, sizeof el);
  r = ioctl(fd, SNDRV_CTL_IOCTL_ELEM_LIST, &el);
  check(r == 0 && el.count >= 2, "ELEM_LIST(space=0) counts >= 2 controls", "Master Playback Volume + Switch", errno);
  struct snd_ctl_elem_id ids[8];
  memset(ids, 0, sizeof ids);
  el.space = 8;
  el.pids = (uint64_t)(uintptr_t)ids;
  r = ioctl(fd, SNDRV_CTL_IOCTL_ELEM_LIST, &el);
  check(r == 0 && el.used >= 2, "ELEM_LIST fills the ids", "snd_ctl_elem_list()", errno);
  int have_vol = 0, have_sw = 0;
  for (uint32_t i = 0; r == 0 && i < el.used && i < 8; i++) {
    info("control #%u iface %d '%s'", ids[i].numid, ids[i].iface, ids[i].name);
    if (!strcmp((char *)ids[i].name, "Master Playback Volume")) have_vol = 1;
    if (!strcmp((char *)ids[i].name, "Master Playback Switch")) have_sw = 1;
  }
  check(have_vol && have_sw, "'Master' volume and switch exist", "alsa-lib simple mixer composes Master from these", 0);

  if (have_vol) {
    struct snd_ctl_elem_info ei;
    memset(&ei, 0, sizeof ei);
    ei.id.iface = CTL_IFACE_MIXER;
    strcpy((char *)ei.id.name, "Master Playback Volume");
    r = ioctl(fd, SNDRV_CTL_IOCTL_ELEM_INFO, &ei);
    check(r == 0 && ei.type == CTL_ELEM_INTEGER && ei.count == 2 && ei.max > ei.min,
          "ELEM_INFO Master volume is a 2-channel integer range", "amixer set Master", errno);
    if (r == 0) info("Master Playback Volume: %lld..%lld step %lld", (long long)ei.min, (long long)ei.max, (long long)ei.step);
    struct snd_ctl_elem_value ev;
    memset(&ev, 0, sizeof ev);
    ev.id.iface = CTL_IFACE_MIXER;
    strcpy((char *)ev.id.name, "Master Playback Volume");
    r = ioctl(fd, SNDRV_CTL_IOCTL_ELEM_READ, &ev);
    check(r == 0, "ELEM_READ Master volume", "current gain", errno);
    if (r == 0) info("Master volume now L %lld R %lld", (long long)ev.values[0], (long long)ev.values[1]);
    memset(&ev, 0, sizeof ev);
    ev.id.iface = CTL_IFACE_MIXER;
    strcpy((char *)ev.id.name, "Master Playback Switch");
    r = ioctl(fd, SNDRV_CTL_IOCTL_ELEM_READ, &ev);
    check(r == 0, "ELEM_READ Master switch", "1 = unmuted", errno);
    if (r == 0) {
      info("Master switch L %lld R %lld", (long long)ev.values[0], (long long)ev.values[1]);
      if (!ev.values[0] && !ev.values[1]) info("NOTE: Master is MUTED -- every layer above will be silent");
    }
  }
  close(fd);
}

// ── PulseAudio: the server socket ───────────────────────────────────────────
//
// cubeb-pulse (media/libcubeb/src/cubeb_pulse.c, pulse_init) builds a
// context and connects to the default server; libpulse resolves that to
// client.conf's default-server, here unix:/run/pulse/native. A refused
// connect is the whole story: cubeb returns CUBEB_ERROR and Firefox tries the
// next backend. Speaking the native protocol is out of scope; the connect is
// the gate that decides the backend.

static int unix_connect(const char *path, int *err) {
  int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (s < 0) {
    *err = errno;
    return 0;
  }
  struct sockaddr_un sa;
  memset(&sa, 0, sizeof sa);
  sa.sun_family = AF_UNIX;
  strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);
  int r = connect(s, (struct sockaddr *)&sa, sizeof sa);
  *err = r == 0 ? 0 : errno;
  close(s);
  return r == 0;
}

// ── PulseAudio daemon ──────────────────────────────────────────────────────
//
// The socket check below says WHETHER the server answers; this says WHY not.
// eclipse-pulseaudio (init service, type=respawn) exits early with no
// binary or no `pulse` account, and otherwise runs `pulseaudio --system`
// whose stderr init captures in /tmp/pulseaudio.log. All of it in one paste.

static void test_daemon(void) {
  section("daemon");
  int bin = node_present("/usr/bin/pulseaudio", S_IFREG) || node_present("/bin/pulseaudio", S_IFREG);
  check(bin, "pulseaudio binary installed", "package pulseaudio; the wrapper exits 127 without it", 0);
  check(node_present("/usr/bin/pactl", S_IFREG), "pactl installed", "pulseaudio-utils; the boot chime uses it", 0);
  check(file_has_prefix("/etc/passwd", "pulse:"), "`pulse` user exists", "--system drops to it; the wrapper exits 1 without it", 0);
  // Explicit present/missing for every group: a silent branch would make an
  // absent permission prerequisite indistinguishable from a check that never
  // ran. `--system` puts the daemon in `pulse`; clients needing the socket
  // are gated by `pulse-access`; ALSA device nodes by `audio`.
  static const char *const groups[] = {"pulse", "pulse-access", "audio"};
  for (size_t i = 0; i < sizeof groups / sizeof groups[0]; i++) {
    char prefix[32];
    snprintf(prefix, sizeof prefix, "%s:", groups[i]);
    info("group %-13s %s", groups[i], file_has_prefix("/etc/group", prefix) ? "present" : "MISSING");
  }

  long pid = find_process("pulseaudio");
  check(pid > 0, "a pulseaudio process is running", "init respawns eclipse-pulseaudio; none alive = it keeps dying", 0);
  if (pid > 0) info("pulseaudio pid %ld", pid);

  // The daemon's own last words, and the chime's.
  dump_tail("/tmp/pulseaudio.log", 40);
  dump_tail("/tmp/boot-sound.log", 12);
  // The chime is `mpg123 -o pulse`: libout123 dlopen()s output_pulse.so from
  // here. "Failed to open module pulse" in that log is this directory (or a
  // library the module links, libpulse-simple) -- not the server.
  list_mpg123_modules("/usr/lib/mpg123");
}

// ── AF_UNIX connect() semantics ────────────────────────────────────────────
//
// Before binding its socket, module-native-protocol-unix calls
// pa_unix_socket_remove_stale(): connect() to the path, and if that fails
// with ECONNREFUSED it concludes a dead socket FILE is in the way and
// unlink()s it. Linux answers ENOENT for a path that does not exist, so
// nothing is unlinked and the module binds. A kernel that answers
// ECONNREFUSED for "nobody bound here" sends Pulse to unlink a phantom file,
// that fails with ENOENT, the module refuses to load, and there is no socket
// for any client. That was this kernel (unix.rs connect(): lookup miss ->
// ECONNREFUSED). Checked here on a path that certainly has no socket.

static int unix_connect_errno(const char *path) {
  int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (s < 0) return errno;
  struct sockaddr_un sa;
  memset(&sa, 0, sizeof sa);
  sa.sun_family = AF_UNIX;
  strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);
  int r = connect(s, (struct sockaddr *)&sa, sizeof sa);
  int e = r == 0 ? 0 : errno;
  close(s);
  return e;
}

static void test_unix_socket(void) {
  section("unix-sock");
  char nobody[96], lpath[96];
  snprintf(nobody, sizeof nobody, "/tmp/audio-probe-nobody-%ld", (long)getpid());
  snprintf(lpath, sizeof lpath, "/tmp/audio-probe-listen-%ld", (long)getpid());
  unlink(nobody);
  unlink(lpath);

  int e = unix_connect_errno(nobody);
  check(e == ENOENT, "connect() to a path nobody bound fails with ENOENT",
        "pa_unix_socket_remove_stale(): ECONNREFUSED here makes Pulse unlink a phantom socket and refuse to bind", e);
  if (e == ECONNREFUSED) info("=> got ECONNREFUSED: exactly the kernel bug that left PulseAudio with no socket (unix.rs connect)");
  else if (e != ENOENT) info("connect() errno was %d (%s)", e, strerror(e));

  // A real listener must be reachable: bind + listen, then connect.
  int ls = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (ls < 0) {
    fail("socket(AF_UNIX)", "listener", errno);
    return;
  }
  struct sockaddr_un sa;
  memset(&sa, 0, sizeof sa);
  sa.sun_family = AF_UNIX;
  strncpy(sa.sun_path, lpath, sizeof sa.sun_path - 1);
  CHECK_CALL(bind(ls, (struct sockaddr *)&sa, sizeof sa) == 0, "bind() a listener", "pa_socket_server_new_unix()");
  CHECK_CALL(listen(ls, 4) == 0, "listen()", "pa_socket_server_new_unix()");
  e = unix_connect_errno(lpath);
  check(e == 0, "connect() to the live listener succeeds", "pa_context_connect() once the socket exists", e);
  close(ls);
  // After the listener is gone: Linux keeps the socket FILE (ECONNREFUSED);
  // a kernel with no socket inodes answers ENOENT. Either lets Pulse's
  // remove_stale() proceed, so this is reported, not judged.
  e = unix_connect_errno(lpath);
  info("connect() after the listener closed: %s", e ? strerror(e) : "connected?!");
  unlink(lpath);
  unlink(nobody);
}

static int g_pulse_ok;
// 1 when the server has a module-alsa-sink sink, 0 when it only has
// auto_null (module-always-sink) or none, -1 when pactl could not say.
static int g_pulse_sink_ok = -1;

// A reachable server is not a working one: with module-alsa-sink failed to
// load, module-always-sink puts up `auto_null`, every stream connects to it
// and plays into nothing -- mpg123 "plays", Firefox's cubeb initialises,
// there is no sound. Ask the server the way the boot chime does (pactl).
static void pulse_sinks(void) {
  FILE *p = popen("timeout 5 pactl list short sinks 2>&1", "r");
  if (!p) {
    info("pactl not runnable: %s", strerror(errno));
    return;
  }
  char line[512];
  int real = 0, null = 0, other = 0;
  while (fgets(line, sizeof line, p)) {
    size_t l = strlen(line);
    if (l && line[l - 1] == '\n') line[l - 1] = '\0';
    info("sink: %s", line);
    if (strstr(line, "module-alsa-sink")) real++;
    else if (strstr(line, "module-null-sink") || strstr(line, "auto_null")) null++;
    else other++;
  }
  int rc = pclose(p);
  if (rc != 0 && real + null + other == 0) {
    info("pactl exited %d with no output", rc);
    return;
  }
  g_pulse_sink_ok = real > 0;
  check(real > 0, "a module-alsa-sink sink is loaded", "pactl list short sinks; only auto_null = streams play into silence", 0);
  if (real == 0 && null > 0)
    info("=> only the null sink: module-alsa-sink failed to load -- the reason is in the [daemon] log above");
}

static int is_socket(const char *path) {
  struct stat st;
  return stat(path, &st) == 0 && (st.st_mode & S_IFMT) == S_IFSOCK;
}

static void test_pulse(void) {
  section("pulse");
  // Where the clients look (client.conf: default-server = unix:/run/pulse/native)
  // versus where a `pulseaudio --system` daemon actually listens: its
  // compiled-in /var/run/pulse/native. On a rootfs where /var/run is the FHS
  // symlink to /run those are one file; where it is a separate directory the
  // daemon is alive and every client still gets ENOENT. Report both, and the
  // link, so that mismatch is read off the output instead of guessed.
  const char *want = "/run/pulse/native";
  const char *sys = "/var/run/pulse/native";
  const char *user = "/run/user/0/pulse/native";

  char link[256];
  ssize_t ll = readlink("/var/run", link, sizeof link - 1);
  if (ll > 0) {
    link[ll] = '\0';
    info("/var/run -> %s (symlink)", link);
  } else {
    info("/var/run is %s", errno == EINVAL ? "a real directory, NOT a symlink to /run" : strerror(errno));
  }

  int have_want = is_socket(want), have_sys = is_socket(sys), have_user = is_socket(user);
  info("%s: %s", want, have_want ? "socket" : "absent");
  info("%s: %s", sys, have_sys ? "socket" : "absent");
  if (have_user) info("%s: socket (per-user path, not what client.conf names)", user);

  int err = 0;
  int c_want = have_want && unix_connect(want, &err);
  check(c_want, "connect() to /run/pulse/native (what client.conf names)",
        "pa_context_connect(): this is the connect cubeb-pulse and every libpulse client make", have_want ? err : ENOENT);
  g_pulse_ok = c_want;
  if (c_want) pulse_sinks();

  if (!c_want && have_sys) {
    int e2 = 0;
    int c_sys = unix_connect(sys, &e2);
    if (c_sys) {
      fail("daemon reachable only at /var/run/pulse/native", "the server is ALIVE but not where client.conf points", 0);
      info("=> /var/run is not a symlink to /run: pulseaudio --system listens at its compiled-in");
      info("   /var/run/pulse/native while client.conf, Firefox, the boot chime and pactl use");
      info("   /run/pulse/native. Fix: make /var/run -> ../run (xtask ensure_var_run).");
    } else {
      info("%s exists but connect failed: %s", sys, strerror(e2));
    }
  } else if (!c_want) {
    info("no PulseAudio socket at any path: the daemon is not running -- see /tmp/pulseaudio.log");
  }
  if (c_want) info("a Pulse server is listening where clients look; if the ALSA path above is silent, the sink is next (pactl list short sinks)");
  info("server log: /tmp/pulseaudio.log; boot chime: /tmp/boot-sound.log");
}

// ── PulseAudio playback: the path mpg123 and Firefox actually take ─────────
//
// Everything above drives the kernel directly. mpg123 (alsa -> pulse plugin)
// and Firefox (cubeb-pulse) hand their audio to the PulseAudio server, and
// it is the server's module-alsa-sink thread that opens hw:0,0 and feeds it
// for them. A green kernel and a silent mpg123 means that thread is where to
// look, so play the tone through the server with its own client (pacat) and
// read what each side saw: the sink's state, volume and mute from pactl,
// the stream's while it plays, whether pacat returned at all (an mpg123
// that "never finishes" is a stream that never drains), and /proc/gpusnd's
// account of the HDA stream the sink ran -- ring fed or not, bytes written,
// underruns, restarts.

static int g_pulse_play_ok = -1; // -1 not run, 0 failed, 1 played to the hardware

// /proc/gpusnd holds one block per card ("[gpusnd] --- card N ---"); only
// card g_card's block is read, so a second HDA device (an HDMI codec on a
// GPU) cannot lend its events to this verdict. NUL-terminated in place.
static char *gpusnd_card_block(char *buf) {
  char head[48];
  snprintf(head, sizeof head, "[gpusnd] --- card %d ---", g_card);
  char *start = strstr(buf, head);
  if (!start) return NULL;
  char *next = strstr(start + strlen(head), "[gpusnd] --- card ");
  if (next) *next = '\0';
  return start;
}

// "[gpusnd] events: D drains, U underruns, R stream restarts, ..." for g_card.
static int gpusnd_events(long *drains, long *underruns, long *restarts) {
  static char buf[65536];
  if (read_whole("/proc/gpusnd", buf, sizeof buf) < 0) return 0;
  const char *blk = gpusnd_card_block(buf);
  if (!blk) return 0;
  const char *p = strstr(blk, "[gpusnd] events:");
  return p && sscanf(p, "[gpusnd] events: %ld drains, %ld underruns, %ld stream restarts", drains, underruns, restarts) == 3;
}

// The newest stop event of g_card: its kind and how many bytes that stream got.
static int gpusnd_last_stop(char *kind, size_t kind_len, long *at_ms, long *written) {
  static char buf[65536];
  if (read_whole("/proc/gpusnd", buf, sizeof buf) < 0) return 0;
  const char *blk = gpusnd_card_block(buf);
  if (!blk) return 0;
  const char *last = NULL;
  for (const char *p = blk; (p = strstr(p, "stop: ")) != NULL; p += 6) last = p;
  if (!last) return 0;
  char k[32];
  if (sscanf(last, "stop: %31s at %ld ms by kernel clock / %*d ms by HDA wall clock, %ld B written", k, at_ms, written) != 3) return 0;
  snprintf(kind, kind_len, "%s", k);
  return 1;
}

// The PulseAudio sink that sits on hw:<g_card>,0, by its alsa.card property
// in `pactl list sinks`; 0 when there is none. pacat is pointed at it so the
// tone goes through the sink this probe's hardware checks are about, not
// whatever the server's default is.
static int pulse_sink_for_card(char *name, size_t len) {
  FILE *p = popen("timeout 5 pactl list sinks 2>&1", "r");
  if (!p) return 0;
  char line[512], cur[128] = "";
  char want_card[32], want_dev[32];
  snprintf(want_card, sizeof want_card, "alsa.card = \"%d\"", g_card);
  snprintf(want_dev, sizeof want_dev, "device.string = \"hw:%d", g_card);
  int found = 0;
  while (fgets(line, sizeof line, p)) {
    const char *s = line;
    while (*s == ' ' || *s == '\t') s++;
    if (!strncmp(s, "Name: ", 6)) {
      snprintf(cur, sizeof cur, "%s", s + 6);
      size_t l = strlen(cur);
      while (l && (cur[l - 1] == '\n' || cur[l - 1] == ' ')) cur[--l] = '\0';
    } else if (!found && cur[0] && (strstr(s, want_card) || strstr(s, want_dev))) {
      snprintf(name, len, "%s", cur);
      found = 1;
    }
  }
  pclose(p);
  return found;
}

// Lines of a command's output that contain one of the needles, trimmed.
static void dump_cmd_matching(const char *cmd, const char *const *needles, int n) {
  FILE *p = popen(cmd, "r");
  if (!p) {
    info("%s: %s", cmd, strerror(errno));
    return;
  }
  char line[512];
  int hits = 0;
  while (fgets(line, sizeof line, p)) {
    size_t l = strlen(line);
    if (l && line[l - 1] == '\n') line[l - 1] = '\0';
    const char *s = line;
    while (*s == ' ' || *s == '\t') s++;
    for (int i = 0; i < n; i++) {
      if (strstr(s, needles[i])) {
        info("  %s", s);
        hits++;
        break;
      }
    }
  }
  pclose(p);
  if (!hits) info("  (nothing)");
}

// A child `sh -c cmd` in its own process group with its stdin on a pipe we
// write. popen(3) would do, but pclose() waits without limit: a pacat whose
// stream never drains, under a timeout(1) whose SIGTERM never lands, hung
// the probe here forever -- and SIGPIPE from a pacat that died early killed
// it silently. main() ignores SIGPIPE; feed_wait() below has a deadline.
static pid_t feed_start(const char *cmd, int *wfd) {
  int pfd[2];
  if (pipe(pfd) != 0) return -1;
  pid_t pid = fork();
  if (pid < 0) {
    close(pfd[0]);
    close(pfd[1]);
    return -1;
  }
  if (pid == 0) {
    setpgid(0, 0);
    dup2(pfd[0], 0);
    close(pfd[0]);
    close(pfd[1]);
    execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
    _exit(127);
  }
  close(pfd[0]);
  *wfd = pfd[1];
  return pid;
}

// Wait for the child up to `limit_ms`, calling `progress` every 5 s of
// waiting; past the limit, kill its whole process group. Returns the exit
// code, -1 for a signal death, -2 when it had to be killed.
static int feed_wait(pid_t pid, long limit_ms, void (*progress)(long)) {
  struct timespec t0;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  long next_report = 5000;
  for (;;) {
    int rc = 0;
    pid_t w = waitpid(pid, &rc, WNOHANG);
    if (w == pid) return WIFEXITED(rc) ? WEXITSTATUS(rc) : -1;
    if (w < 0 && errno != EINTR) return -1;
    long ms = elapsed_ms(&t0);
    if (ms >= limit_ms) {
      kill(-pid, SIGKILL);
      kill(pid, SIGKILL);
      waitpid(pid, &rc, 0);
      return -2;
    }
    if (ms >= next_report) {
      if (progress) progress(ms);
      next_report += 5000;
    }
    struct timespec nap = {0, 100000000};
    nanosleep(&nap, NULL);
  }
}

static const char *const g_sink_keys[] = {"State:", "Mute:", "Volume: front-left", "Latency:", "Flags:", "Sample Specification:"};

static void pulse_play_progress(long ms) {
  info("pacat still running after %ld ms:", ms);
  dump_cmd_matching("timeout 5 pactl list sinks 2>&1", g_sink_keys, 6);
  dump_matching("/proc/gpusnd", "[gpusnd] ring:");
  dump_tail("/tmp/pulseaudio.log", 4);
}

static void test_pulse_play(void) {
  section("pulse-play");
  if (g_pulse_sink_ok != 1) {
    skip("play the tone through PulseAudio", "no ALSA sink loaded (see [pulse])");
    return;
  }
  if (!node_present("/usr/bin/pacat", S_IFREG) && !node_present("/bin/pacat", S_IFREG)) {
    skip("play the tone through PulseAudio", "pacat not installed (pulseaudio-utils)");
    return;
  }
  if (g_no_tone) {
    skip("play the tone through PulseAudio", "--no-tone");
    return;
  }
  char sink[128];
  if (!pulse_sink_for_card(sink, sizeof sink)) {
    skip("play the tone through PulseAudio", "no PulseAudio sink on this card (pactl list sinks: alsa.card)");
    info("the server's sinks are not on hw:%d; module-alsa-sink for it did not load (see [daemon])", g_card);
    return;
  }
  info("sink on hw:%d: %s", g_card, sink);
  size_t bytes;
  int16_t *pcm = make_tone(&bytes);
  if (!pcm) {
    fail("allocate tone", "calloc", errno);
    return;
  }
  info("sink before (pactl list sinks):");
  dump_cmd_matching("timeout 5 pactl list sinks 2>&1", g_sink_keys, 6);
  long d0 = 0, u0 = 0, r0 = 0;
  int have0 = gpusnd_events(&d0, &u0, &r0);

  // pacat: raw S16LE stereo 48 kHz from stdin, 100 ms latency, as
  // libpulse-simple clients (mpg123 -o pulse) and cubeb-pulse set up.
  struct timespec t0;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  char cmd[320];
  snprintf(cmd, sizeof cmd,
           "timeout 15 pacat --raw --format=s16le --rate=48000 --channels=2 --latency-msec=100 --client-name=audio-probe --device=%s 2>&1",
           sink);
  int wfd = -1;
  pid_t pid = feed_start(cmd, &wfd);
  if (pid < 0) {
    fail("run pacat", "fork/pipe", errno);
    free(pcm);
    return;
  }
  size_t wrote = 0;
  int write_err = 0;
  while (wrote < bytes) {
    ssize_t n = write(wfd, (const char *)pcm + wrote, bytes - wrote);
    if (n < 0 && errno == EINTR) continue;
    if (n <= 0) {
      write_err = errno;
      break;
    }
    wrote += (size_t)n;
  }
  // While it plays: the stream as the server sees it, the sink's state, the
  // ring as the kernel sees it, and what the daemon logged. 150 ms in, the
  // stream and the ring should both be busy. All of this comes out BEFORE
  // the wait below, so a play that hangs still leaves its evidence.
  struct timespec nap = {0, 150000000};
  nanosleep(&nap, NULL);
  static const char *const input_keys[] = {"Corked:", "Mute:", "Volume: front-left", "Buffer Latency:", "Sink Latency:", "Sample Specification:", "application.name"};
  info("stream while playing (pactl list sink-inputs):");
  dump_cmd_matching("timeout 5 pactl list sink-inputs 2>&1", input_keys, 7);
  info("sink while playing (pactl list sinks):");
  static const char *const live_keys[] = {"State:", "Latency:"};
  dump_cmd_matching("timeout 5 pactl list sinks 2>&1", live_keys, 2);
  info("hardware while playing (/proc/gpusnd):");
  dump_matching("/proc/gpusnd", "[gpusnd] ring:");
  dump_matching("/proc/gpusnd", "[gpusnd] stream:");
  info("daemon while playing:");
  dump_tail("/tmp/pulseaudio.log", 8);
  if (write_err) info("pacat stopped reading its stdin after %zu of %zu bytes: %s", wrote, bytes, strerror(write_err));
  close(wfd);
  int exit_code = feed_wait(pid, 20000, pulse_play_progress);
  long ms = elapsed_ms(&t0);
  check(wrote == bytes && exit_code == 0 && ms < 5000, "pacat plays the tone through the server and returns",
        "the stream drains and the client exits; an mpg123 that never finishes is a stream that never drains", 0);
  info("pacat exit %d after %ld ms (tone %d ms + 100 ms latency)%s", exit_code, ms, TONE_MS,
       exit_code == 124   ? " -- killed by timeout(1): the stream never drained"
       : exit_code == -2 ? " -- KILLED by the probe: still alive 5 s past timeout(1)'s SIGTERM (a client the kernel cannot interrupt?)"
       : exit_code == -1 ? " -- died of a signal"
                         : "");

  // Let the sink go idle (module-suspend-on-idle timeout=1) so the kernel
  // records the stream's end, then read its account.
  struct timespec settle = {1, 500000000};
  nanosleep(&settle, NULL);
  long d1 = 0, u1 = 0, r1 = 0;
  int have1 = gpusnd_events(&d1, &u1, &r1);
  if (have0 && have1) {
    check(r1 > r0 || d1 > d0 || u1 > u0, "the sink thread drove the HDA stream",
          "/proc/gpusnd recorded a stream start or stop during the play: the server's writes reached hw:0,0", 0);
    info("events during the play: +%ld drains, +%ld underruns, +%ld restarts", d1 - d0, u1 - u0, r1 - r0);
  } else {
    info("/proc/gpusnd not readable: cannot say what reached the hardware");
  }
  char kind[32];
  long at_ms = 0, written = 0;
  if (gpusnd_last_stop(kind, sizeof kind, &at_ms, &written)) {
    // The sink resamples 48 kHz to its own rate; expect at least most of it.
    long expect = (long)bytes * 44100 / 48000 * 8 / 10;
    check(written >= expect, "the whole tone reached the hardware",
          "bytes the newest HDA stream got before it stopped, against the tone's size at the sink's rate", 0);
    info("newest stream: %s at %ld ms, %ld B written (tone is %zu B at 48 kHz)", kind, at_ms, written, bytes);
    if (written < expect && written > 0)
      info("=> the sink wrote %ld B and never refilled: its thread stopped feeding hw:0,0 (blocked in a write? never woken?)", written);
    g_pulse_play_ok = (exit_code == 0 && written >= expect);
  } else {
    info("no stream stop recorded: the sink never started an HDA stream, or it is still running");
    g_pulse_play_ok = 0;
  }
  info("sink after:");
  dump_cmd_matching("timeout 5 pactl list sinks 2>&1", g_sink_keys, 6);
  dump_tail("/tmp/pulseaudio.log", 6);
  free(pcm);
}

// ── PulseAudio's sink thread, call for call ────────────────────────────────
//
// module-alsa-sink (tsched=0, mmap=0 on this image) never drives the ring the
// way [alsa-pcm] does. It opens hw:0,0 O_NONBLOCK at the sink's rate with
// fragments=4 x fragment_size=4800 B (period 1200 frames, buffer 4800),
// CLOSES it when module-suspend-on-idle suspends the sink, and REOPENS it on
// the next stream -- at that stream's rate when daemon.conf's alternate rate
// lets the sink switch (a 48 kHz pacat on a sink parked at 44.1 kHz) --
// demanding the same period/buffer bytes back ("Resume failed, couldn't
// restore original fragment settings" otherwise, and the sink stays
// SUSPENDED: every stream plays into silence and never drains).
//
// alsa-lib then talks SYNC_PTR, since this kernel has no mmap of the
// status/control pages: snd_pcm_avail = SYNC_PTR(HWSYNC|APPL|AVAIL_MIN) then
// SYNC_PTR(APPL|AVAIL_MIN), avail = hw_ptr + buffer - appl_ptr;
// snd_pcm_writei = WRITEI_FRAMES then SYNC_PTR(APPL|AVAIL_MIN);
// snd_pcm_start = SYNC_PTR(AVAIL_MIN) then START. unix_write()
// (alsa-sink.c) writes whatever avail says fits, and the FIRST writei after
// an avail > 0 must not answer EAGAIN: try_recover() asserts on it and the
// daemon aborts. After "Starting playback." the thread sleeps in poll() on
// [pcm, timer] with NO timeout -- every later refill hangs on the timer's
// period tick or the PCM's POLLOUT. This section replays exactly that and
// watches the hardware while it does.

struct snd_pcm_mmap_status_ {
  int32_t state, pad1;
  uint64_t hw_ptr;
  struct snd_timespec_ tstamp;
  int32_t suspended_state, pad2;
  struct snd_timespec_ audio_tstamp;
};
struct snd_pcm_mmap_control_ {
  uint64_t appl_ptr, avail_min;
};
struct snd_pcm_sync_ptr {
  uint32_t flags, pad1;
  union {
    struct snd_pcm_mmap_status_ status;
    unsigned char reserved[64];
  } s;
  union {
    struct snd_pcm_mmap_control_ control;
    unsigned char reserved[64];
  } c;
};
#define SNDRV_PCM_IOCTL_SYNC_PTR _IOC_(IOC_RW, 'A', 0x23, sizeof(struct snd_pcm_sync_ptr))
#define SYNC_PTR_HWSYNC 1u
#define SYNC_PTR_APPL 2u
#define SYNC_PTR_AVAIL_MIN 4u
#define SINK_PERIOD 3000 // fragment_size=12000 B / 4 B per frame
#define SINK_BUFFER 12000 // fragments=4
#define SINK_RATE_CREATED 48000 // daemon.conf default-sample-rate
#define SINK_RATE_ALTERNATE 44100 // alternate-sample-rate (the boot chime's)

static int g_pulse_sink_path_ok = -1; // -1 not run, 0 broken, 1 the kernel side of the sink works

// pa_alsa_open_by_device_string + pa_alsa_set_hw_params as unsuspend() calls
// them: O_NONBLOCK, S16LE stereo at `rate`, the sink's period and buffer.
// Returns the fd or -errno; the granted sizes in frames.
static int sink_hw_open(unsigned rate, unsigned *period, unsigned *buffer) {
  char path[64];
  snprintf(path, sizeof path, "/dev/snd/pcmC%dD0p", g_card);
  int fd = open(path, O_WRONLY | O_NONBLOCK);
  if (fd < 0) return -errno;
  struct snd_pcm_hw_params hp;
  hw_params_any(&hp);
  mask_only(&hp, HWP_ACCESS, ACCESS_RW_INTERLEAVED);
  mask_only(&hp, HWP_FORMAT, FORMAT_S16_LE);
  mask_only(&hp, HWP_SUBFORMAT, SUBFORMAT_STD);
  iv_set(&hp, IV_CHANNELS, 2);
  iv_set(&hp, IV_RATE, rate);
  iv_set(&hp, IV_PERIOD_SIZE, SINK_PERIOD);
  iv_set(&hp, IV_BUFFER_SIZE, SINK_BUFFER);
  if (ioctl(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &hp) != 0) {
    int e = errno;
    close(fd);
    return -e;
  }
  *period = hp.intervals[IV_PERIOD_SIZE].min;
  *buffer = hp.intervals[IV_BUFFER_SIZE].min;
  return fd;
}

// The period timer exactly as [alsa-timer] opens it, quietly. fd or -errno.
static int period_timer_open(void) {
  int tfd = open("/dev/snd/timer", O_RDONLY | O_NONBLOCK);
  if (tfd < 0) return -errno;
  int one = 1;
  struct snd_timer_select sel;
  memset(&sel, 0, sizeof sel);
  sel.id.dev_class = TIMER_CLASS_PCM;
  sel.id.card = g_card;
  struct snd_timer_params tp;
  memset(&tp, 0, sizeof tp);
  tp.flags = TIMER_PSFLG_AUTO;
  tp.ticks = 1;
  tp.filter = (1u << TIMER_EVENT_TICK) | (1u << TIMER_EVENT_MSUSPEND) | (1u << TIMER_EVENT_MRESUME);
  if (ioctl(tfd, SNDRV_TIMER_IOCTL_TREAD_OLD, &one) != 0 || ioctl(tfd, SNDRV_TIMER_IOCTL_SELECT, &sel) != 0 ||
      ioctl(tfd, SNDRV_TIMER_IOCTL_PARAMS, &tp) != 0 || ioctl(tfd, SNDRV_TIMER_IOCTL_START, 0) != 0) {
    int e = errno;
    close(tfd);
    return -e;
  }
  return tfd;
}

// Step tracing for the calls a machine has died in: one flushed line before
// each ioctl while g_trace_steps is set, so the console names the last one.
static int g_trace_steps;
static void step(const char *what) {
  if (!g_trace_steps) return;
  printf("         step: %s\n", what);
  fflush(stdout);
}

// snd_pcm_avail() on the SYNC_PTR fallback: hwsync (HWSYNC|APPL|AVAIL_MIN),
// then avail_update's query (APPL|AVAIL_MIN), then alsa-lib's own arithmetic
// on the two pointers it got back. -1 with errno in *err on an ioctl error.
static long sink_avail(int fd, struct snd_pcm_sync_ptr *sp, unsigned buffer, long long boundary, int *err) {
  sp->flags = SYNC_PTR_HWSYNC | SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN;
  step("SYNC_PTR(HWSYNC|APPL|AVAIL_MIN) -- snd_pcm_avail hwsync");
  if (ioctl(fd, SNDRV_PCM_IOCTL_SYNC_PTR, sp) != 0) {
    *err = errno;
    return -1;
  }
  sp->flags = SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN;
  step("SYNC_PTR(APPL|AVAIL_MIN) -- snd_pcm_avail_update");
  if (ioctl(fd, SNDRV_PCM_IOCTL_SYNC_PTR, sp) != 0) {
    *err = errno;
    return -1;
  }
  long long avail = (long long)sp->s.status.hw_ptr + (long long)buffer - (long long)sp->c.control.appl_ptr;
  if (avail < 0) avail += boundary;
  else if (avail >= boundary) avail -= boundary;
  *err = 0;
  return (long)avail;
}

struct sink_stats {
  long writes, short_writes, eagain_later, spurious_wakes, max_write;
};

// alsa-sink.c unix_write() with tsched=0 (hwbuf_unused = 0): fill what avail
// says, at most 10 rounds, stop once less than 3% of the buffer is free.
// Returns 1 when something was written, 0 when not, -1 on what would make
// the daemon abort or restart the PCM (`why` says which).
static int sink_unix_write(int fd, struct snd_pcm_sync_ptr *sp, unsigned buffer, long long boundary, const int16_t *pcm,
                           size_t *pos, size_t total, int polled, struct sink_stats *st, char *why, size_t why_len) {
  int work_done = 0;
  for (unsigned j = 0;;) {
    int err;
    long n = sink_avail(fd, sp, buffer, boundary, &err);
    if (n < 0) {
      snprintf(why, why_len, "snd_pcm_avail: SYNC_PTR failed: %s", strerror(err));
      return -1;
    }
    if (n == 0) {
      if (polled) st->spurious_wakes++;
      break;
    }
    j++;
    if (j > 10) break;
    if (j >= 2 && (unsigned long)n * 100 < (unsigned long)buffer * 3) break;
    polled = 0;
    size_t room = (size_t)n;
    int after_avail = 1;
    for (;;) {
      if (*pos >= total) return work_done;
      size_t frames = total - *pos;
      if (frames > room) frames = room;
      struct snd_xferi x = {0, (uint64_t)(uintptr_t)(pcm + *pos * 2), frames};
      if (g_trace_steps) {
        char what[96];
        snprintf(what, sizeof what, "WRITEI %zu frames on the O_NONBLOCK fd (the driver starts the HDA stream here)", frames);
        step(what);
      }
      int r = ioctl(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x);
      int e = errno;
      if (r == 0) {
        // query_status_and_control_data(): learn the advanced appl_ptr.
        sp->flags = SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN;
        step("SYNC_PTR(APPL|AVAIL_MIN) after WRITEI");
        if (ioctl(fd, SNDRV_PCM_IOCTL_SYNC_PTR, sp) != 0) {
          snprintf(why, why_len, "SYNC_PTR after WRITEI failed: %s", strerror(errno));
          return -1;
        }
      } else {
        if (!after_avail && e == EAGAIN) {
          st->eagain_later++;
          break;
        }
        if (e == EAGAIN)
          snprintf(why, why_len,
                   "WRITEI -> EAGAIN right after avail said %ld frames fit: try_recover() asserts err != -EAGAIN and the daemon ABORTS", n);
        else
          snprintf(why, why_len, "WRITEI(%zu frames) failed: %s -> try_recover/snd_pcm_recover, then a PCM restart", frames, strerror(e));
        return -1;
      }
      long got = (long)x.result;
      if (!after_avail && got == 0) break;
      if (got <= 0) {
        snprintf(why, why_len, "WRITEI wrote %ld frames right after avail said %ld fit (pa_assert(frames > 0))", got, n);
        return -1;
      }
      after_avail = 0;
      st->writes++;
      if (got < (long)frames) st->short_writes++;
      if (got > st->max_write) st->max_write = got;
      *pos += (size_t)got;
      work_done = 1;
      if ((size_t)got >= room) break;
      room -= (size_t)got;
    }
    if (*pos >= total) return work_done;
  }
  return work_done;
}

// "[gpusnd] ring: running=<bool> queued=<n> ..." for this card.
static int gpusnd_ring(int *running, long *queued) {
  static char buf[65536];
  if (read_whole("/proc/gpusnd", buf, sizeof buf) < 0) return 0;
  const char *blk = gpusnd_card_block(buf);
  if (!blk) return 0;
  const char *p = strstr(blk, "[gpusnd] ring: running=");
  char word[8];
  if (!p || sscanf(p, "[gpusnd] ring: running=%7s queued=%ld", word, queued) != 2) return 0;
  *running = !strcmp(word, "true");
  return 1;
}

static void test_pulse_sink(void) {
  section("pulse-sink");
  char pcm_path[64];
  snprintf(pcm_path, sizeof pcm_path, "/dev/snd/pcmC%dD0p", g_card);
  if (!node_present(pcm_path, S_IFCHR)) {
    skip("replay module-alsa-sink", "no ALSA playback node");
    return;
  }
  int fail_at_entry = g_fail;

  // The sink's life: created at 48 kHz, suspended, resumed at the chime's
  // 44.1 kHz, suspended, resumed at a 48 kHz stream's rate. Each resume is
  // a fresh open that must hand back the creation-time sizes.
  unsigned period0 = 0, buffer0 = 0;
  int fd = sink_hw_open(SINK_RATE_CREATED, &period0, &buffer0);
  check(fd >= 0, "open O_NONBLOCK + HW_PARAMS 48000 Hz, period 1200, buffer 4800 (sink creation)",
        "module-alsa-sink device=hw:0,0 mmap=0 tsched=0 fragments=4 fragment_size=4800", fd < 0 ? -fd : 0);
  if (fd < 0) {
    g_pulse_sink_path_ok = 0;
    return;
  }
  info("granted at 48000 Hz: period %u frames, buffer %u frames", period0, buffer0);
  close(fd);
  const unsigned resume_rates[2] = {SINK_RATE_ALTERNATE, SINK_RATE_CREATED};
  fd = -1;
  for (int i = 0; i < 2; i++) {
    unsigned period = 0, buffer = 0;
    int f = sink_hw_open(resume_rates[i], &period, &buffer);
    char name[96];
    snprintf(name, sizeof name, "reopen at %u Hz grants the same period/buffer (sink resume)", resume_rates[i]);
    check(f >= 0 && period == period0 && buffer == buffer0, name,
          "unsuspend(): 'Resume failed, couldn't restore original fragment settings' otherwise, and the sink stays SUSPENDED", f < 0 ? -f : 0);
    if (f >= 0 && (period != period0 || buffer != buffer0))
      info("granted at %u Hz: period %u, buffer %u (creation: %u / %u)", resume_rates[i], period, buffer, period0, buffer0);
    if (f < 0) {
      g_pulse_sink_path_ok = 0;
      return;
    }
    if (i == 0) close(f);
    else fd = f;
  }
  unsigned period = period0, buffer = buffer0;
  long period_ms = (long)period * 1000 / SINK_RATE_CREATED;

  // pa_alsa_set_sw_params(avail_min=1, period_event=1): alsa-lib keeps
  // period_event for itself (the timer below) and sends the rest.
  // alsa-lib: boundary = buffer_size, doubled while boundary*2 <= LONG_MAX -
  // buffer_size (unsigned arithmetic there; here the bound is halved instead
  // so the doubling never overflows a signed value into an endless loop).
  long long boundary = buffer;
  while (boundary <= (0x7fffffffffffffffll - (long long)buffer) / 2) boundary *= 2;
  struct snd_pcm_sw_params sw;
  memset(&sw, 0, sizeof sw);
  sw.tstamp_mode = 1;
  sw.period_step = 1;
  sw.avail_min = 1;
  sw.start_threshold = (uint64_t)boundary; // (snd_pcm_uframes_t)-1, clipped
  sw.stop_threshold = (uint64_t)boundary;
  sw.boundary = (uint64_t)boundary;
  CHECK_CALL(ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw) == 0, "SW_PARAMS avail_min 1, start/stop threshold = boundary",
             "pa_alsa_set_sw_params(): never auto-start, never auto-stop");
  int tfd = period_timer_open();
  check(tfd >= 0, "period_event timer bound and started", "snd_pcm_hw_change_timer(): the sink's only wake-up source besides POLLOUT",
        tfd < 0 ? -tfd : 0);
  CHECK_CALL(ioctl(fd, SNDRV_PCM_IOCTL_PREPARE, 0) == 0, "PREPARE", "snd_pcm_prepare() in unsuspend()");

  struct snd_pcm_sync_ptr sp;
  memset(&sp, 0, sizeof sp);
  sp.flags = SYNC_PTR_APPL | SYNC_PTR_AVAIL_MIN; // prepare's query_status_and_control_data
  CHECK_CALL(ioctl(fd, SNDRV_PCM_IOCTL_SYNC_PTR, &sp) == 0 && sp.s.status.state == PCM_STATE_PREPARED && sp.c.control.appl_ptr == 0 &&
                 sp.s.status.hw_ptr == 0,
             "SYNC_PTR after PREPARE: PREPARED, appl_ptr 0, hw_ptr 0", "alsa-lib's view of the fresh stream");
  info("SYNC_PTR: state %d appl_ptr %llu hw_ptr %llu avail_min %llu", sp.s.status.state, (unsigned long long)sp.c.control.appl_ptr,
       (unsigned long long)sp.s.status.hw_ptr, (unsigned long long)sp.c.control.avail_min);
  int err = 0;
  long avail = sink_avail(fd, &sp, buffer, boundary, &err);
  check(avail == (long)buffer, "snd_pcm_avail() after PREPARE = the whole buffer", "hw_ptr + buffer - appl_ptr, as alsa-lib computes it", err);
  if (avail != (long)buffer) info("avail %ld (buffer %u)", avail, buffer);

  // The stream's data: the tone (or silence with --no-tone), 48 kHz, no
  // resampling in the way.
  size_t bytes = 0;
  int16_t *pcm = g_no_tone ? calloc(TONE_FRAMES * 2, sizeof(int16_t)) : make_tone(&bytes);
  if (g_no_tone) bytes = (size_t)TONE_FRAMES * 4;
  if (!pcm) {
    fail("allocate the stream's data", "calloc", errno);
    close(tfd);
    close(fd);
    return;
  }
  size_t total = bytes / 4, pos = 0;
  struct sink_stats st;
  memset(&st, 0, sizeof st);
  char why[256] = "";
  long d0 = 0, u0 = 0, r0 = 0;
  int have0 = gpusnd_events(&d0, &u0, &r0);
  struct tick_gaps gaps_before = tick_gaps_late();
  struct timespec t0;
  clock_gettime(CLOCK_MONOTONIC, &t0);

  // The first fill and START are where a machine has died with nothing on
  // the console: say each step before taking it.
  g_trace_steps = 1;
  int r = sink_unix_write(fd, &sp, buffer, boundary, pcm, &pos, total, 0, &st, why, sizeof why);
  check(r == 1 && pos >= buffer, "first unix_write() fills the buffer (avail, then nonblocking WRITEI of what fits)",
        "the first WRITEI after avail > 0 must write, never EAGAIN (try_recover asserts on it)", 0);
  if (r < 0) info("=> %s", why);
  info("first fill: %zu of %zu frames in %ld write%s (largest %ld frames)%s", pos, total, st.writes, st.writes == 1 ? "" : "s", st.max_write,
       st.short_writes ? " -- short writes seen" : "");

  // "Starting playback.": issue_applptr (SYNC_PTR AVAIL_MIN: our appl_ptr
  // into the kernel, which must already hold it), then START.
  unsigned long long appl_before = sp.c.control.appl_ptr;
  sp.flags = SYNC_PTR_AVAIL_MIN;
  step("SYNC_PTR(AVAIL_MIN) before START");
  int sync_ok = ioctl(fd, SNDRV_PCM_IOCTL_SYNC_PTR, &sp) == 0;
  step("START");
  int start_ok = sync_ok && ioctl(fd, SNDRV_PCM_IOCTL_START, 0) == 0;
  int se = errno;
  step("START returned");
  g_trace_steps = 0;
  avail = sink_avail(fd, &sp, buffer, boundary, &err);
  check(start_ok && sp.s.status.state == PCM_STATE_RUNNING && sp.c.control.appl_ptr == appl_before,
        "START after the first fill -> RUNNING, appl_ptr kept", "snd_pcm_start(): SYNC_PTR(AVAIL_MIN) commits the client's appl_ptr, then START",
        start_ok ? 0 : se);
  info("after START: state %d appl_ptr %llu hw_ptr %llu avail %ld", sp.s.status.state, (unsigned long long)sp.c.control.appl_ptr,
       (unsigned long long)sp.s.status.hw_ptr, avail);

  // The refill loop: poll [pcm POLLOUT, timer POLLIN] the way pa_rtpoll does
  // with the timer disabled (tsched=0) -- no timeout in the daemon; here a
  // bound of two periods + the 4 ms tick + a stall allowance, so a wake that
  // never comes is reported instead of hanging the probe.
  long limit = period_ms * 2 + 100;
  long wakes = 0, timer_wakes = 0, late = 0, max_gap = 0, timed_out = 0, bad_revents = 0, idle_wakes = 0;
  long last = elapsed_ms(&t0);
  int hw_running_seen = -1;
  long hw_queued_seen = 0;
  while (r >= 0 && pos < total) {
    struct pollfd pf[2] = {{fd, POLLOUT, 0}, {tfd, POLLIN, 0}};
    int pr = poll(pf, 2, (int)limit);
    long now = elapsed_ms(&t0);
    if (pr <= 0) {
      timed_out = 1;
      avail = sink_avail(fd, &sp, buffer, boundary, &err);
      info("=> no wake within %ld ms after %zu of %zu frames (avail now %ld, state %d): Pulse polls with NO timeout, its sink thread would sleep forever",
           limit, pos, total, avail, sp.s.status.state);
      break;
    }
    long gap = now - last;
    last = now;
    wakes++;
    if (gap > max_gap) max_gap = gap;
    if (gap > period_ms + 20) late++;
    unsigned short rev = pf[0].revents;
    if (pf[1].revents & POLLIN) {
      // snd_pcm_hw_poll_revents: drain the timer queue, report POLLOUT.
      struct snd_timer_tread tr[4];
      if (read(tfd, tr, sizeof tr) < 0 && errno != EAGAIN) bad_revents++;
      rev |= POLLOUT;
      timer_wakes++;
    }
    if (rev & ~POLLOUT) {
      bad_revents++;
      info("=> PCM revents 0x%x at wake %ld: pa_alsa_recover_from_poll() would restart the PCM", rev, wakes);
      break;
    }
    if (!(rev & POLLOUT)) {
      idle_wakes++;
      continue;
    }
    if (hw_running_seen != 1) {
      int running = 0;
      long queued = 0;
      if (gpusnd_ring(&running, &queued)) {
        if (running || hw_running_seen < 0) {
          hw_running_seen = running;
          hw_queued_seen = queued;
        }
      }
    }
    r = sink_unix_write(fd, &sp, buffer, boundary, pcm, &pos, total, 1, &st, why, sizeof why);
    if (r < 0) info("=> at wake %ld (%zu of %zu frames): %s", wakes, pos, total, why);
  }
  long play_ms = elapsed_ms(&t0);
  check(r >= 0 && !timed_out && !bad_revents && pos >= total, "the refill loop writes the whole stream, one wake per period",
        "poll [pcm, timer] -> snd_pcm_avail -> WRITEI, as the sink thread lives", 0);
  info("%zu of %zu frames in %ld ms: %ld wakes (%ld from the timer), gap max %ld ms, %ld late (> period %ld ms + 20), %ld with nothing to write, %ld idle",
       pos, total, play_ms, wakes, timer_wakes, max_gap, late, period_ms, st.spurious_wakes, idle_wakes);
  info("writes: %ld, short %ld, EAGAIN after a first write %ld, largest %ld frames", st.writes, st.short_writes, st.eagain_later, st.max_write);
  if (st.spurious_wakes)
    info("=> %ld wake%s with avail 0: Pulse logs 'ALSA woke us up to write new data to the device, but there was actually nothing to write' once",
         st.spurious_wakes, st.spurious_wakes == 1 ? "" : "s");
  check(hw_running_seen == 1, "the HDA stream runs while the sink refills",
        "/proc/gpusnd ring: running=true at the third wake -- the driver started DMA on the sink's first fill", 0);
  if (hw_running_seen >= 0) info("hardware while refilling: running=%s queued=%ld B", hw_running_seen ? "true" : "false", hw_queued_seen);
  else info("hardware at wake 3: /proc/gpusnd not readable or fewer than 3 wakes");

  // The end of the stream: DRAIN (Pulse's suspend after the last input
  // goes idle drains too), then what the hardware recorded.
  CHECK_CALL(ioctl(fd, SNDRV_PCM_IOCTL_DRAIN, 0) == 0, "DRAIN", "waits for the ring to empty, then resets it");
  // The newest stop is the stream's end: DRAIN polls the ring and the DMA
  // engine usually passes the tail a few bytes before that poll sees it, so
  // the driver may label the end "underrun". A stop that holds every byte
  // the sink wrote IS the end, whatever its label; an underrun with fewer
  // bytes is a refill that came too late, and the restart that follows it
  // splits the stream in two.
  long d1 = 0, u1 = 0, r1 = 0;
  int have1 = gpusnd_events(&d1, &u1, &r1);
  char kind[32] = "";
  long at_ms = 0, written = 0;
  int have_stop = gpusnd_last_stop(kind, sizeof kind, &at_ms, &written);
  if (have_stop) {
    check(written >= (long)bytes, "the whole stream reached the hardware", "the newest HDA stream ended holding every byte the sink wrote", 0);
    info("newest stream: %s at %ld ms, %ld B written (stream is %zu B)%s", kind, at_ms, written, bytes,
         !strcmp(kind, "underrun") && written >= (long)bytes ? " -- the engine passed the tail before DRAIN's poll: the end, not a dropout" : "");
  } else {
    fail("the whole stream reached the hardware", "no stop recorded in /proc/gpusnd: the stream never started, or never ended", 0);
  }
  if (have0 && have1) {
    int end_only = u1 - u0 == 0 || (u1 - u0 == 1 && have_stop && written >= (long)bytes);
    check(end_only, "no underrun before the stream's end", "an underrun mid-stream = a refill that came too late (a missed wake), then a restart", 0);
    info("events: +%ld drains, +%ld underruns, +%ld restarts", d1 - d0, u1 - u0, r1 - r0);
  }
  report_tick_gaps("the sink replay", gaps_before, tick_gaps_late());
  ioctl(tfd, SNDRV_TIMER_IOCTL_STOP, 0);
  close(tfd);
  close(fd);
  free(pcm);
  g_pulse_sink_path_ok = g_fail == fail_at_entry;
  if (g_pulse_sink_path_ok)
    info("the kernel gives module-alsa-sink everything it asks for; if [pulse-play] is still silent, the daemon is where to look (/tmp/pulseaudio.log)");
}

// ── ALSA "default" routing ─────────────────────────────────────────────────
//
// cubeb-alsa does snd_pcm_open("default"), and alsa-lib resolves that name
// through /etc/asound.conf. On this image `pcm.!default { type pulse }`
// routes it into the pulse PLUGIN, so cubeb-alsa is only as alive as the
// Pulse server -- the raw hw:0 ioctls this probe drives say nothing about it.
// Read the file so the verdict states which case applies instead of assuming.

static int g_default_is_pulse = -1; // -1 unknown (no asound.conf), 0 no, 1 yes

static void detect_default_pcm(void) {
  FILE *f = fopen("/etc/asound.conf", "r");
  if (!f) {
    g_default_is_pulse = -1;
    return;
  }
  char buf[8192];
  size_t n = fread(buf, 1, sizeof buf - 1, f);
  fclose(f);
  buf[n] = '\0';
  g_default_is_pulse = 0;
  const char *d = strstr(buf, "pcm.!default");
  if (!d) return;
  const char *close = strchr(d, '}');
  const char *pulse = strstr(d, "type pulse");
  if (pulse && (!close || pulse < close)) g_default_is_pulse = 1;
}

// ── verdict ────────────────────────────────────────────────────────────────

int main(int argc, char **argv) {
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "-v")) g_verbose = 1;
    else if (!strcmp(argv[i], "--no-tone")) g_no_tone = 1;
    else if (!strcmp(argv[i], "--card") && i + 1 < argc) g_card = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--skip") && i + 1 < argc && g_nskipped < 16) g_skipped[g_nskipped++] = argv[++i];
    else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
      printf("usage: audio-probe [-v] [--no-tone] [--card N] [--skip SECTION]...\n"
             "Drives every layer Firefox's audio rests on -- /dev/dsp, the raw ALSA\n"
             "PCM and control ABI, and the PulseAudio socket -- the way each is driven\n"
             "in the field, plays a 440 Hz tone through each playback layer, and says\n"
             "which cubeb backend Firefox's OpenCubeb() would get.\n"
             "--skip SECTION leaves one out (devices, wake, oss, alsa-pcm, alsa-timer,\n"
             "pulse-sink, alsa-ctl, daemon, unix-sock, pulse, pulse-play).\n");
      return 0;
    }
  }

  signal(SIGPIPE, SIG_IGN); // a pacat that dies early must not take the probe with it
  printf("Firefox-shaped audio probe (card %d)\n", g_card);
  printf("Each check mirrors what cubeb, alsa-lib or PulseAudio asks of the kernel.\n");

  struct {
    const char *name;
    void (*run)(void);
  } sections[] = {
      {"devices", test_devices},     {"wake", test_wake},           {"oss", test_oss},
      {"alsa-pcm", test_alsa_pcm},   {"alsa-timer", test_alsa_timer}, {"pulse-sink", test_pulse_sink},
      {"alsa-ctl", test_alsa_ctl},   {"daemon", test_daemon},       {"unix-sock", test_unix_socket},
      {"pulse", test_pulse},         {"pulse-play", test_pulse_play},
  };
  for (size_t i = 0; i < sizeof sections / sizeof sections[0]; i++) {
    if (skipped(sections[i].name)) {
      printf("\n[%s] skipped (--skip)\n", sections[i].name);
      continue;
    }
    sections[i].run();
  }

  section("verdict");
  detect_default_pcm();
  // cubeb's backend order on Linux: pulse (if libpulse loads and the server
  // answers) then alsa. OpenCubeb() fails only when every backend fails.
  printf("  raw ALSA hw:%d: %s (what this probe drove directly)\n", g_card,
         g_alsa_pcm_ok ? "works" : "BROKEN above");
  if (g_pulse_sink_path_ok == 1) printf("  kernel side of module-alsa-sink (resume, avail, nonblocking writei, wakes): works\n");
  else if (g_pulse_sink_path_ok == 0) printf("  kernel side of module-alsa-sink (resume, avail, nonblocking writei, wakes): BROKEN (see [pulse-sink])\n");
  if (g_pulse_play_ok == 1) printf("  PulseAudio playback (pacat -> sink -> hw:%d): works -- what mpg123 and Firefox use\n", g_card);
  else if (g_pulse_play_ok == 0) printf("  PulseAudio playback (pacat -> sink -> hw:%d): BROKEN (see [pulse-play]) -- this is the silence mpg123 and Firefox hit\n", g_card);
  if (g_pulse_ok && g_pulse_sink_ok == 0)
    printf("  cubeb-pulse: would initialise, but the server has NO real sink -> streams play into silence (see [pulse]/[daemon])\n");
  else if (g_pulse_ok) printf("  cubeb-pulse: would initialise (server reachable%s)\n",
                              g_pulse_sink_ok == 1 ? ", ALSA sink loaded" : "");
  else printf("  cubeb-pulse: would FAIL (no reachable server) -> Firefox falls through to alsa\n");
  // cubeb-alsa opens ALSA "default"; where asound.conf routes that into the
  // pulse plugin it lives or dies with the server, not with the raw hw path.
  int alsa_backend_ok;
  if (g_default_is_pulse == 1) {
    alsa_backend_ok = g_pulse_ok;
    if (alsa_backend_ok) printf("  cubeb-alsa:  would initialise (\"default\" -> pulse plugin, server reachable)\n");
    else printf("  cubeb-alsa:  would FAIL (\"default\" -> pulse plugin in /etc/asound.conf, and no server)\n");
  } else {
    alsa_backend_ok = g_alsa_pcm_ok;
    if (g_default_is_pulse == 0) printf("  cubeb-alsa:  \"default\" is not routed to pulse; it rides the raw hw path -> %s\n",
                                         alsa_backend_ok ? "would initialise" : "would FAIL");
    else printf("  cubeb-alsa:  no /etc/asound.conf read; assuming raw hw path -> %s\n",
                alsa_backend_ok ? "would initialise" : "would FAIL");
  }
  if (!g_pulse_ok && !alsa_backend_ok)
    printf("  => this is exactly Firefox's 'OpenCubeb() failed to init cubeb'.\n");
  else
    printf("  => OpenCubeb() should succeed; if Firefox still fails, run it with MOZ_LOG=cubeb:5.\n");

  printf("\n%d passed, %d failed, %d skipped\n", g_pass, g_fail, g_skip);
  if (g_fail == 0) printf("Nothing here stands in the way of sound.\n");
  return g_fail > 125 ? 125 : g_fail;
}
