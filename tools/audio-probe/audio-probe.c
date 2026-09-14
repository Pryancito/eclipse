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
#include <time.h>
#include <unistd.h>

// ── reporting (same shape as firefox-probe) ─────────────────────────────────

static int g_verbose;
static int g_no_tone;
static int g_card;
static int g_pass, g_fail, g_skip;
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
static int pcm_open_prepared(unsigned *period, unsigned *buffer) {
  char path[64];
  snprintf(path, sizeof path, "/dev/snd/pcmC%dD0p", g_card);
  int fd = open(path, O_WRONLY);
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
  int pfd = pcm_open_prepared(&period, &buffer);
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

    // alsa-lib's poll_descriptors: [pcm POLLOUT, timer POLLIN]. Within one
    // period (+ the 4 ms re-scan tick and a little scheduling slack) the
    // timer must report POLLIN: that is the bound the check enforces. The
    // poll itself waits longer so a late wake is measured and printed
    // rather than reported as a bare timeout.
    struct pollfd pf[2] = {{pfd, POLLOUT, 0}, {tfd, POLLIN, 0}};
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    long bound = period_ms + 20;
    long limit = period_ms * 2 + 100;
    int pr = poll(pf, 2, (int)limit);
    int pe = errno;
    long ms = elapsed_ms(&t0);
    // Whichever fd woke first, the TIMER must be readable within a period.
    int timer_in = pf[1].revents & POLLIN;
    if (pr > 0 && !timer_in) {
      // The PCM may report POLLOUT slightly before the period boundary the
      // timer counts; give the timer the rest of the period.
      struct pollfd pt = {tfd, POLLIN, 0};
      pr = poll(&pt, 1, (int)(limit - ms > 0 ? limit - ms : 1));
      pe = errno;
      ms = elapsed_ms(&t0);
      timer_in = pr > 0 && (pt.revents & POLLIN);
    }
    check(pr > 0 && timer_in && ms <= bound, "poll(POLLIN) on the timer wakes within a period",
          "snd_pcm_hw_poll_revents: POLLIN here becomes POLLOUT for Pulse's unix_write", pr < 0 ? pe : 0);
    info("timer POLLIN after %ld ms (period %ld ms + 4 ms re-scan tick; bound %ld ms)%s", ms, period_ms, bound,
         pr == 0 ? " -- TIMED OUT" : "");
    if (timer_in && ms > bound)
      info("=> later than one period: Pulse would see late wake-ups (audible as stutter)");

    n = read(tfd, tr, sizeof tr);
    e = errno;
    // The second read follows immediately: printing first would let another
    // period elapse (serial output is slow) and turn a drained queue into a
    // fresh tick.
    struct snd_timer_tread again[4];
    ssize_t n2 = read(tfd, again, sizeof again);
    int e2 = errno;
    check(n >= (ssize_t)sizeof tr[0] && n % (ssize_t)sizeof tr[0] == 0 && tr[0].event == TIMER_EVENT_TICK && tr[0].val >= 1,
          "read() returns a TICK tread record", "32-byte {event, tstamp, val}; alsa-lib reads and discards up to 4", n < 0 ? e : 0);
    if (n > 0) info("%zd byte%s: event %d val %u (ticks elapsed) at %lld.%09lld", n, n == 1 ? "" : "s", tr[0].event,
                    tr[0].val, (long long)tr[0].tstamp.sec, (long long)tr[0].tstamp.nsec);
    check(n2 < 0 && e2 == EAGAIN, "read() again -> EAGAIN (queue drained)",
          "snd_pcm_hw_clear_timer_queue reads once; a queue that never empties would spin Pulse", n2 < 0 && e2 != EAGAIN ? e2 : 0);
    if (n2 > 0) info("second read() returned %zd bytes (val %u): a period elapsed between the two reads", n2, again[0].val);

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
    else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
      printf("usage: audio-probe [-v] [--no-tone] [--card N]\n"
             "Drives every layer Firefox's audio rests on -- /dev/dsp, the raw ALSA\n"
             "PCM and control ABI, and the PulseAudio socket -- the way each is driven\n"
             "in the field, plays a 440 Hz tone through each playback layer, and says\n"
             "which cubeb backend Firefox's OpenCubeb() would get.\n");
      return 0;
    }
  }

  printf("Firefox-shaped audio probe (card %d)\n", g_card);
  printf("Each check mirrors what cubeb, alsa-lib or PulseAudio asks of the kernel.\n");

  test_devices();
  test_oss();
  test_alsa_pcm();
  test_alsa_timer();
  test_alsa_ctl();
  test_daemon();
  test_unix_socket();
  test_pulse();

  section("verdict");
  detect_default_pcm();
  // cubeb's backend order on Linux: pulse (if libpulse loads and the server
  // answers) then alsa. OpenCubeb() fails only when every backend fails.
  printf("  raw ALSA hw:%d: %s (what this probe drove directly)\n", g_card,
         g_alsa_pcm_ok ? "works" : "BROKEN above");
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
