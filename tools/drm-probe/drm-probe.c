// Eclipse OS: a compositor-shaped DRM/KMS probe.
//
// `audio-probe` answers "what does the kernel lack for sound?" and reports the
// first layer that breaks, because audio has a failure mode no return value
// shows: silence with every call returning success. DRM/KMS has the *same*
// failure mode -- every ioctl returns 0 and the screen stays black -- so this
// probe mirrors audio-probe: it drives every layer wlroots/labwc and Mesa-GBM
// rest on, bottom-up, exactly the way each is driven in the field, reports the
// first that breaks, and (with --scanout) paints a test pattern to the real
// display: what you SEE is the DRM analogue of audio-probe's test tone.
//
// The layers, in the order a compositor walks them:
//
//   nodes      the device nodes the kernel created (/dev/dri/card0,
//              renderD128/129) and /sys/class/drm/card0 (status, modes, edid),
//              what libdrm's drmGetDevices2 and libudev enumerate.
//   version    DRM_IOCTL_VERSION: the driver name/date/desc a client keys on
//              ("virtio_gpu" in QEMU, "nouveau"/"nvidia" on real hardware).
//   caps       DRM_IOCTL_GET_CAP (DUMB_BUFFER, PRIME, ADDFB2_MODIFIERS,
//              CURSOR_W/H, TIMESTAMP_MONOTONIC, CRTC_IN_VBLANK_EVENT, SYNCOBJ)
//              and SET_CLIENT_CAP (UNIVERSAL_PLANES, ATOMIC) -- what wlroots
//              negotiates before it will drive the device.
//   resources  MODE_GETRESOURCES + GETCRTC/GETENCODER: the CRTC/encoder/
//              connector graph a modeset is built from.
//   connectors MODE_GETCONNECTOR: connection status and mode list; the
//              preferred mode a compositor scans out at.
//   edid       OBJ_GETPROPERTIES + GETPROPERTY + GETPROPBLOB: the connector's
//              EDID blob (the monitor's native mode / audio caps come from it).
//   planes     MODE_GETPLANERESOURCES + GETPLANE: the primary/cursor planes
//              the universal-planes path commits to.
//   dumb       CREATE_DUMB -> MAP_DUMB -> mmap -> draw a test pattern ->
//              ADDFB + ADDFB2: the CPU framebuffer wlroots' pixman backend and
//              lunarbg/foot's wl_shm path put on screen. Non-destructive: it
//              allocates and registers a framebuffer without scanning it out.
//   prime      PRIME_HANDLE_TO_FD + FD_TO_HANDLE: the dma-buf round-trip Mesa/
//              GBM uses to hand a renderD128 buffer to card0.
//   render     /dev/dri/renderD128: open + VERSION + GET_CAP, the compute node
//              Mesa llvmpipe renders on.
//   scanout    (--scanout, opt-in) SET_MASTER -> SETCRTC with the drawn fb ->
//              the pattern appears -> PAGE_FLIP + the flip event on the fd ->
//              WAIT_VBLANK -> DROP_MASTER. This STEALS the display from the
//              running compositor, so it is opt-in and meant for a free VT or
//              with labwc stopped.
//   nvidia     (real hardware only) the nouveau/RM specifics -- driver name,
//              HDMI connector ELD, GSP -- SKIPped under QEMU's virtio-gpu.
//   verdict    would wlroots' DRM backend initialise from what was measured?
//
// Every ioctl number and struct layout below is the x86_64 uapi
// (include/uapi/drm/{drm,drm_mode}.h); musl ships no <drm/*> headers, so they
// are spelled out. The kernel side is linux-object/src/fs/devfs/drm_scheme.rs.
//
// Build:  musl-gcc -O2 -static -o drm-probe drm-probe.c
// Run:    drm-probe               (-v per-check detail, --card N for cardN,
//                                  --scanout for the on-screen pattern test,
//                                  --hw to force the NVIDIA/hardware section,
//                                  --skip SECTION to leave one out)
//
// Exit status is the number of failed checks, capped at 125.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

// ── DRM uapi (x86_64) ────────────────────────────────────────────────────────

#define DRM_IOCTL_VERSION 0xC0406400u
#define DRM_IOCTL_GET_CAP 0xC010640Cu
#define DRM_IOCTL_SET_CLIENT_CAP 0x4010640Du
#define DRM_IOCTL_SET_MASTER 0x0000641Eu
#define DRM_IOCTL_DROP_MASTER 0x0000641Fu
#define DRM_IOCTL_MODE_GETRESOURCES 0xC04064A0u
#define DRM_IOCTL_MODE_GETCRTC 0xC06864A1u
#define DRM_IOCTL_MODE_SETCRTC 0xC06864A2u
#define DRM_IOCTL_MODE_GETENCODER 0xC01464A6u
#define DRM_IOCTL_MODE_GETCONNECTOR 0xC05064A7u
#define DRM_IOCTL_MODE_GETPROPERTY 0xC04064AAu
#define DRM_IOCTL_MODE_GETPROPBLOB 0xC01064ACu
#define DRM_IOCTL_MODE_GETPLANERESOURCES 0xC01064B5u
#define DRM_IOCTL_MODE_GETPLANE 0xC02064B6u
#define DRM_IOCTL_MODE_OBJ_GETPROPERTIES 0xC02064B9u
#define DRM_IOCTL_MODE_CREATE_DUMB 0xC02064B2u
#define DRM_IOCTL_MODE_MAP_DUMB 0xC01064B3u
#define DRM_IOCTL_MODE_DESTROY_DUMB 0xC00464B4u
#define DRM_IOCTL_MODE_ADDFB 0xC01C64AEu
#define DRM_IOCTL_MODE_ADDFB2 0xC06864B8u
#define DRM_IOCTL_MODE_RMFB 0xC00464AFu
#define DRM_IOCTL_MODE_PAGE_FLIP 0xC01864B0u
#define DRM_IOCTL_WAIT_VBLANK 0xC018643Au
#define DRM_IOCTL_PRIME_HANDLE_TO_FD 0xC00C642Du
#define DRM_IOCTL_PRIME_FD_TO_HANDLE 0xC00C642Eu

// GET_CAP capabilities
#define DRM_CAP_DUMB_BUFFER 0x1
#define DRM_CAP_VBLANK_HIGH_CRTC 0x2
#define DRM_CAP_DUMB_PREFERRED_DEPTH 0x3
#define DRM_CAP_PRIME 0x5
#define DRM_CAP_TIMESTAMP_MONOTONIC 0x6
#define DRM_CAP_ASYNC_PAGE_FLIP 0x7
#define DRM_CAP_CURSOR_WIDTH 0x8
#define DRM_CAP_CURSOR_HEIGHT 0x9
#define DRM_CAP_ADDFB2_MODIFIERS 0x10
#define DRM_CAP_CRTC_IN_VBLANK_EVENT 0x12
#define DRM_CAP_SYNCOBJ 0x13

// SET_CLIENT_CAP capabilities
#define DRM_CLIENT_CAP_UNIVERSAL_PLANES 2
#define DRM_CLIENT_CAP_ATOMIC 3

// connector connection status
#define DRM_MODE_CONNECTED 1
#define DRM_MODE_DISCONNECTED 2
// mode type bit
#define DRM_MODE_TYPE_PREFERRED (1 << 3)
// page-flip / vblank flags
#define DRM_MODE_PAGE_FLIP_EVENT 0x01
#define _DRM_VBLANK_RELATIVE 0x1
#define _DRM_VBLANK_EVENT 0x4000000
// drm event types
#define DRM_EVENT_VBLANK 0x01
#define DRM_EVENT_FLIP_COMPLETE 0x02
// XRGB8888 fourcc ('XR24')
#define DRM_FORMAT_XRGB8888 0x34325258u

struct drm_version {
  int version_major, version_minor, version_patchlevel;
  size_t name_len;
  char *name;
  size_t date_len;
  char *date;
  size_t desc_len;
  char *desc;
};
struct drm_get_cap {
  uint64_t capability, value;
};
struct drm_set_client_cap {
  uint64_t capability, value;
};
struct drm_mode_card_res {
  uint64_t fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
  uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
  uint32_t min_width, max_width, min_height, max_height;
};
struct drm_mode_modeinfo {
  uint32_t clock;
  uint16_t hdisplay, hsync_start, hsync_end, htotal, hskew;
  uint16_t vdisplay, vsync_start, vsync_end, vtotal, vscan;
  uint32_t vrefresh, flags, type;
  char name[32];
};
struct drm_mode_get_connector {
  uint64_t encoders_ptr, modes_ptr, props_ptr, prop_values_ptr;
  uint32_t count_modes, count_props, count_encoders;
  uint32_t encoder_id, connector_id, connector_type, connector_type_id;
  uint32_t connection, mm_width, mm_height, subpixel, pad;
};
struct drm_mode_get_encoder {
  uint32_t encoder_id, encoder_type, crtc_id, possible_crtcs, possible_clones;
};
struct drm_mode_crtc {
  uint64_t set_connectors_ptr;
  uint32_t count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
  struct drm_mode_modeinfo mode;
};
struct drm_mode_get_plane_res {
  uint64_t plane_id_ptr;
  uint32_t count_planes;
};
struct drm_mode_get_plane {
  uint32_t plane_id, crtc_id, fb_id, possible_crtcs, gamma_size, count_format_types;
  uint64_t format_type_ptr;
};
struct drm_mode_obj_get_properties {
  uint64_t props_ptr, prop_values_ptr;
  uint32_t count_props, obj_id, obj_type;
};
struct drm_mode_get_property {
  uint64_t values_ptr, enum_blob_ptr;
  uint32_t prop_id, flags;
  char name[32];
  uint32_t count_values, count_enum_blobs;
};
struct drm_mode_get_blob {
  uint32_t blob_id, length;
  uint64_t data;
};
struct drm_mode_create_dumb {
  uint32_t height, width, bpp, flags, handle, pitch;
  uint64_t size;
};
struct drm_mode_map_dumb {
  uint32_t handle, pad;
  uint64_t offset;
};
struct drm_mode_destroy_dumb {
  uint32_t handle;
};
struct drm_mode_fb_cmd {
  uint32_t fb_id, width, height, pitch, bpp, depth, handle;
};
struct drm_mode_fb_cmd2 {
  uint32_t fb_id, width, height, pixel_format, flags;
  uint32_t handles[4], pitches[4], offsets[4];
  uint64_t modifier[4];
};
struct drm_mode_crtc_page_flip {
  uint32_t crtc_id, fb_id, flags, reserved;
  uint64_t user_data;
};
struct drm_wait_vblank_request {
  uint32_t type, sequence;
  uint64_t signal;
};
union drm_wait_vblank {
  struct drm_wait_vblank_request request;
  struct {
    uint32_t type, sequence;
    int64_t tval_sec, tval_usec;
  } reply;
};
struct drm_prime_handle {
  uint32_t handle, flags;
  int32_t fd;
};
struct drm_event {
  uint32_t type, length;
};

// ── framework (mirrors audio-probe) ──────────────────────────────────────────

static int g_verbose, g_card, g_scanout, g_force_hw;
static int g_pass, g_fail, g_skip;
static const char *g_skipped[16];
static int g_nskipped;
static const char *g_section = "";

static int skipped(const char *name) {
  for (int i = 0; i < g_nskipped; i++)
    if (!strcmp(g_skipped[i], name)) return 1;
  return 0;
}
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

// ioctl with EINTR retry; returns 0 on success, -1 with errno set.
static int drm(int fd, unsigned long req, void *arg) {
  int r;
  do {
    r = ioctl(fd, req, arg);
  } while (r < 0 && errno == EINTR);
  return r;
}

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
    info("%s: %s", label, line);
    if (++lines >= 8) {
      info("%s: ... (truncated)", label);
      break;
    }
  }
  fclose(f);
}

// ── shared state discovered as sections run ──────────────────────────────────

static int g_fd = -1;                     // /dev/dri/cardN
static char g_driver[64] = "";            // from VERSION
static int g_cap_dumb = 0, g_cap_prime = 0;
static uint32_t g_crtc_id = 0, g_conn_id = 0;
static int g_connected = 0;
static struct drm_mode_modeinfo g_mode;   // preferred/first mode of g_conn_id
static int g_have_mode = 0;
static uint32_t g_dumb_handle = 0, g_pitch = 0, g_fb_id = 0, g_fb2_id = 0;
static uint64_t g_dumb_size = 0;
static void *g_map = MAP_FAILED;
static uint32_t g_w = 0, g_h = 0;
static int g_edid_ok = 0;

static char g_cardpath[64];

static int have_fd(const char *name) {
  if (g_fd < 0) {
    skip(name, "no card fd (see [version])");
    return 0;
  }
  return 1;
}

// ── nodes ────────────────────────────────────────────────────────────────────

static void test_nodes(void) {
  section("nodes");
  check(node_present(g_cardpath, S_IFCHR), "card node", g_cardpath, errno);
  // Render nodes are how Mesa (llvmpipe) allocates; card node is scanout/KMS.
  if (node_present("/dev/dri/renderD128", S_IFCHR)) ok("renderD128", "compute/render node present");
  else skip("renderD128", "no render node (Mesa GBM would use card0)");
  if (node_present("/dev/dri/renderD129", S_IFCHR))
    info("renderD129 present (second GPU render node)");
  char sysdir[64];
  snprintf(sysdir, sizeof sysdir, "/sys/class/drm/card%d", g_card);
  if (node_present(sysdir, S_IFDIR)) {
    ok("/sys/class/drm", "sysfs DRM class present (libudev/drmGetDevices2)");
    if (g_verbose) {
      char p[128];
      snprintf(p, sizeof p, "%s-*/status", sysdir);  // best-effort; dirs vary
      (void)p;
    }
  } else {
    skip("/sys/class/drm", "no sysfs card dir (libudev enumeration is blind)");
  }
}

// ── version ──────────────────────────────────────────────────────────────────

static void test_version(void) {
  section("version");
  g_fd = open(g_cardpath, O_RDWR | O_CLOEXEC);
  if (g_fd < 0) {
    fail("open card", g_cardpath, errno);
    return;
  }
  ok("open card", g_cardpath);
  char name[64] = "", date[64] = "", desc[128] = "";
  struct drm_version v;
  memset(&v, 0, sizeof v);
  v.name = name;
  v.name_len = sizeof name - 1;
  v.date = date;
  v.date_len = sizeof date - 1;
  v.desc = desc;
  v.desc_len = sizeof desc - 1;
  if (drm(g_fd, DRM_IOCTL_VERSION, &v) == 0) {
    snprintf(g_driver, sizeof g_driver, "%s", name);
    ok("DRM_IOCTL_VERSION", "driver identified");
    info("driver=\"%s\" v%d.%d.%d  date=\"%s\"", name, v.version_major, v.version_minor,
         v.version_patchlevel, date);
    if (desc[0]) info("desc=\"%s\"", desc);
  } else {
    fail("DRM_IOCTL_VERSION", "no driver identity", errno);
  }
}

// ── caps ─────────────────────────────────────────────────────────────────────

static void cap(const char *label, uint64_t capability, int mandatory, int *out) {
  struct drm_get_cap c;
  memset(&c, 0, sizeof c);
  c.capability = capability;
  if (drm(g_fd, DRM_IOCTL_GET_CAP, &c) == 0) {
    if (out) *out = c.value != 0;
    if (mandatory)
      check(c.value != 0, label, "capability advertised", 0);
    else
      info("%s = %llu", label, (unsigned long long)c.value);
  } else {
    if (mandatory) fail(label, "GET_CAP failed", errno);
    else info("%s: unsupported (GET_CAP %s)", label, strerror(errno));
  }
}

static void set_client_cap(const char *label, uint64_t capability) {
  struct drm_set_client_cap c;
  memset(&c, 0, sizeof c);
  c.capability = capability;
  c.value = 1;
  if (drm(g_fd, DRM_IOCTL_SET_CLIENT_CAP, &c) == 0)
    ok(label, "client cap accepted");
  else
    // Not fatal: legacy KMS still works without universal planes / atomic.
    info("%s: not accepted (%s) -- compositor falls back to legacy KMS", label, strerror(errno));
}

static void test_caps(void) {
  section("caps");
  if (!have_fd("caps")) return;
  cap("DUMB_BUFFER", DRM_CAP_DUMB_BUFFER, 1, &g_cap_dumb);  // mandatory: no dumb = no SW fb
  cap("PRIME", DRM_CAP_PRIME, 0, &g_cap_prime);
  cap("TIMESTAMP_MONOTONIC", DRM_CAP_TIMESTAMP_MONOTONIC, 0, NULL);
  cap("CRTC_IN_VBLANK_EVENT", DRM_CAP_CRTC_IN_VBLANK_EVENT, 0, NULL);
  cap("ADDFB2_MODIFIERS", DRM_CAP_ADDFB2_MODIFIERS, 0, NULL);
  cap("CURSOR_WIDTH", DRM_CAP_CURSOR_WIDTH, 0, NULL);
  cap("CURSOR_HEIGHT", DRM_CAP_CURSOR_HEIGHT, 0, NULL);
  cap("SYNCOBJ", DRM_CAP_SYNCOBJ, 0, NULL);
  set_client_cap("UNIVERSAL_PLANES", DRM_CLIENT_CAP_UNIVERSAL_PLANES);
  set_client_cap("ATOMIC", DRM_CLIENT_CAP_ATOMIC);
}

// ── resources ────────────────────────────────────────────────────────────────

static uint32_t g_crtc_ids[16];
static int g_ncrtc = 0;

static void test_resources(void) {
  section("resources");
  if (!have_fd("resources")) return;
  struct drm_mode_card_res res;
  memset(&res, 0, sizeof res);
  if (drm(g_fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) {
    fail("MODE_GETRESOURCES", "no KMS resources (not a modeset device?)", errno);
    return;
  }
  ok("MODE_GETRESOURCES", "counted KMS objects");
  info("crtcs=%u connectors=%u encoders=%u fbs=%u  fb %ux%u..%ux%u", res.count_crtcs,
       res.count_connectors, res.count_encoders, res.count_fbs, res.min_width, res.min_height,
       res.max_width, res.max_height);
  check(res.count_crtcs > 0, "has CRTC", "at least one CRTC to scan out from", 0);
  check(res.count_connectors > 0, "has connector", "at least one output connector", 0);
  if (res.count_crtcs == 0 || res.count_connectors == 0) return;

  uint32_t crtcs[16] = {0}, conns[16] = {0}, encs[16] = {0};
  uint32_t nc = res.count_crtcs < 16 ? res.count_crtcs : 16;
  uint32_t no = res.count_connectors < 16 ? res.count_connectors : 16;
  uint32_t ne = res.count_encoders < 16 ? res.count_encoders : 16;
  res.crtc_id_ptr = (uint64_t)(uintptr_t)crtcs;
  res.connector_id_ptr = (uint64_t)(uintptr_t)conns;
  res.encoder_id_ptr = (uint64_t)(uintptr_t)encs;
  res.count_crtcs = nc;
  res.count_connectors = no;
  res.count_encoders = ne;
  res.count_fbs = 0;
  if (drm(g_fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) {
    fail("MODE_GETRESOURCES (ids)", "second call to fill id arrays", errno);
    return;
  }
  g_ncrtc = (int)nc;
  for (uint32_t i = 0; i < nc; i++) g_crtc_ids[i] = crtcs[i];
  if (nc) g_crtc_id = crtcs[0];

  // GETCRTC on the first CRTC (what a compositor reads to learn the current fb).
  struct drm_mode_crtc c;
  memset(&c, 0, sizeof c);
  c.crtc_id = crtcs[0];
  CHECK_CALL(drm(g_fd, DRM_IOCTL_MODE_GETCRTC, &c) == 0, "MODE_GETCRTC", "read CRTC 0 state");

  // Remember the connectors so [connectors] can walk them (re-query is cheap).
  info("crtc[0]=%u connector[0]=%u encoder[0]=%u", crtcs[0], no ? conns[0] : 0, ne ? encs[0] : 0);
}

// ── connectors ───────────────────────────────────────────────────────────────

static void test_connectors(void) {
  section("connectors");
  if (!have_fd("connectors")) return;
  struct drm_mode_card_res res;
  memset(&res, 0, sizeof res);
  if (drm(g_fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0 || res.count_connectors == 0) {
    skip("connectors", "no connectors (see [resources])");
    return;
  }
  uint32_t conns[16] = {0};
  uint32_t no = res.count_connectors < 16 ? res.count_connectors : 16;
  memset(&res, 0, sizeof res);
  res.connector_id_ptr = (uint64_t)(uintptr_t)conns;
  res.count_connectors = no;
  if (drm(g_fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) {
    fail("MODE_GETRESOURCES", "connector ids", errno);
    return;
  }
  int any_connected = 0;
  for (uint32_t i = 0; i < no; i++) {
    struct drm_mode_get_connector conn;
    memset(&conn, 0, sizeof conn);
    conn.connector_id = conns[i];
    if (drm(g_fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn) != 0) {
      fail("MODE_GETCONNECTOR", "count query", errno);
      continue;
    }
    const char *st = conn.connection == DRM_MODE_CONNECTED
                         ? "connected"
                         : (conn.connection == DRM_MODE_DISCONNECTED ? "disconnected" : "unknown");
    info("connector %u type=%u %s modes=%u encoders=%u", conn.connector_id, conn.connector_type,
         st, conn.count_modes, conn.count_encoders);
    if (conn.connection != DRM_MODE_CONNECTED || conn.count_modes == 0) continue;
    any_connected = 1;

    // Pull the mode list and pick the preferred (or first) mode.
    uint32_t nm = conn.count_modes < 64 ? conn.count_modes : 64;
    struct drm_mode_modeinfo *modes = calloc(nm, sizeof *modes);
    uint32_t enc_ids[16] = {0};
    if (!modes) continue;
    struct drm_mode_get_connector q;
    memset(&q, 0, sizeof q);
    q.connector_id = conns[i];
    q.modes_ptr = (uint64_t)(uintptr_t)modes;
    q.count_modes = nm;
    q.encoders_ptr = (uint64_t)(uintptr_t)enc_ids;
    q.count_encoders = conn.count_encoders < 16 ? conn.count_encoders : 16;
    if (drm(g_fd, DRM_IOCTL_MODE_GETCONNECTOR, &q) == 0 && q.count_modes > 0) {
      int pick = 0;
      for (uint32_t m = 0; m < q.count_modes && m < nm; m++)
        if (modes[m].type & DRM_MODE_TYPE_PREFERRED) {
          pick = (int)m;
          break;
        }
      if (!g_have_mode) {
        g_mode = modes[pick];
        g_have_mode = 1;
        g_conn_id = conns[i];
        g_connected = 1;
        info("picked connector %u mode %s (%ux%u@%u)", g_conn_id, g_mode.name, g_mode.hdisplay,
             g_mode.vdisplay, g_mode.vrefresh);
      }
    }
    free(modes);
  }
  check(any_connected, "connected output", "at least one connector with a mode to scan out", 0);
}

// ── edid ─────────────────────────────────────────────────────────────────────

static void test_edid(void) {
  section("edid");
  if (!have_fd("edid")) return;
  if (!g_conn_id) {
    skip("edid", "no connected connector (see [connectors])");
    return;
  }
  struct drm_mode_obj_get_properties op;
  uint32_t prop_ids[64] = {0};
  uint64_t prop_vals[64] = {0};
  memset(&op, 0, sizeof op);
  op.obj_id = g_conn_id;
  op.obj_type = 0xc0c0c0c0;  // DRM_MODE_OBJECT_CONNECTOR
  op.props_ptr = (uint64_t)(uintptr_t)prop_ids;
  op.prop_values_ptr = (uint64_t)(uintptr_t)prop_vals;
  op.count_props = 64;
  if (drm(g_fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &op) != 0) {
    fail("OBJ_GETPROPERTIES", "connector properties", errno);
    return;
  }
  ok("OBJ_GETPROPERTIES", "connector has properties");
  uint32_t edid_blob = 0;
  for (uint32_t i = 0; i < op.count_props && i < 64; i++) {
    struct drm_mode_get_property gp;
    memset(&gp, 0, sizeof gp);
    gp.prop_id = prop_ids[i];
    if (drm(g_fd, DRM_IOCTL_MODE_GETPROPERTY, &gp) != 0) continue;
    if (!strcmp(gp.name, "EDID")) {
      edid_blob = (uint32_t)prop_vals[i];
      break;
    }
  }
  if (!edid_blob) {
    skip("EDID", "connector exposes no EDID blob (headless/virtio may omit it)");
    return;
  }
  struct drm_mode_get_blob gb;
  memset(&gb, 0, sizeof gb);
  gb.blob_id = edid_blob;
  if (drm(g_fd, DRM_IOCTL_MODE_GETPROPBLOB, &gb) != 0 || gb.length < 8) {
    fail("GETPROPBLOB", "read EDID blob length", errno);
    return;
  }
  unsigned char buf[256];
  uint32_t len = gb.length < sizeof buf ? gb.length : (uint32_t)sizeof buf;
  gb.data = (uint64_t)(uintptr_t)buf;
  gb.length = len;
  if (drm(g_fd, DRM_IOCTL_MODE_GETPROPBLOB, &gb) != 0) {
    fail("GETPROPBLOB", "read EDID blob data", errno);
    return;
  }
  static const unsigned char hdr[8] = {0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00};
  g_edid_ok = (len >= 8 && memcmp(buf, hdr, 8) == 0);
  check(g_edid_ok, "EDID header", "valid 00 FF..FF 00 EDID signature", 0);
  if (g_edid_ok) info("EDID %u bytes, mfg block present", len);
}

// ── planes ───────────────────────────────────────────────────────────────────

static void test_planes(void) {
  section("planes");
  if (!have_fd("planes")) return;
  struct drm_mode_get_plane_res pr;
  memset(&pr, 0, sizeof pr);
  if (drm(g_fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pr) != 0) {
    // Legacy-only KMS without the universal-planes ioctl is still usable.
    info("GETPLANERESOURCES unsupported (%s) -- legacy KMS only", strerror(errno));
    skip("planes", "no plane resources ioctl");
    return;
  }
  info("planes=%u", pr.count_planes);
  check(pr.count_planes > 0, "has plane", "at least one (primary) plane", 0);
  if (pr.count_planes == 0) return;
  uint32_t ids[32] = {0};
  uint32_t n = pr.count_planes < 32 ? pr.count_planes : 32;
  pr.plane_id_ptr = (uint64_t)(uintptr_t)ids;
  pr.count_planes = n;
  if (drm(g_fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pr) != 0) {
    fail("GETPLANERESOURCES", "plane ids", errno);
    return;
  }
  struct drm_mode_get_plane pl;
  memset(&pl, 0, sizeof pl);
  pl.plane_id = ids[0];
  CHECK_CALL(drm(g_fd, DRM_IOCTL_MODE_GETPLANE, &pl) == 0, "MODE_GETPLANE", "read plane 0");
  info("plane[0]=%u possible_crtcs=%#x formats=%u", ids[0], pl.possible_crtcs,
       pl.count_format_types);
}

// ── dumb buffer + framebuffer (draws the pattern; does NOT scan out) ──────────

static void draw_pattern(void) {
  // XRGB8888 colour bars + a diagonal gradient, so a torn/duplicated scanout
  // is obvious and a black screen is unmistakable.
  static const uint32_t bars[8] = {0xffffffff, 0xffffff00, 0xff00ffff, 0xff00ff00,
                                   0xffff00ff, 0xffff0000, 0xff0000ff, 0xff101010};
  for (uint32_t y = 0; y < g_h; y++) {
    uint32_t *row = (uint32_t *)((char *)g_map + (size_t)y * g_pitch);
    for (uint32_t x = 0; x < g_w; x++) {
      uint32_t bar = bars[(x * 8) / (g_w ? g_w : 1)];
      uint8_t g = (uint8_t)(((x + y) * 255) / (g_w + g_h ? g_w + g_h : 1));
      // blend a touch of the gradient into the green channel
      uint32_t px = bar ^ ((uint32_t)(g & 0x20) << 8);
      row[x] = px;
    }
  }
}

static void test_dumb(void) {
  section("dumb");
  if (!have_fd("dumb")) return;
  if (!g_cap_dumb) {
    fail("dumb", "kernel does not advertise DUMB_BUFFER -- no CPU framebuffer path", 0);
    return;
  }
  g_w = g_have_mode ? g_mode.hdisplay : 1024;
  g_h = g_have_mode ? g_mode.vdisplay : 768;
  if (g_w == 0 || g_h == 0) {
    g_w = 1024;
    g_h = 768;
  }
  struct drm_mode_create_dumb cd;
  memset(&cd, 0, sizeof cd);
  cd.width = g_w;
  cd.height = g_h;
  cd.bpp = 32;
  if (drm(g_fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd) != 0) {
    fail("CREATE_DUMB", "allocate a dumb framebuffer", errno);
    return;
  }
  g_dumb_handle = cd.handle;
  g_pitch = cd.pitch;
  g_dumb_size = cd.size;
  ok("CREATE_DUMB", "dumb buffer allocated");
  info("%ux%u pitch=%u size=%llu handle=%u", g_w, g_h, g_pitch,
       (unsigned long long)g_dumb_size, g_dumb_handle);

  struct drm_mode_map_dumb md;
  memset(&md, 0, sizeof md);
  md.handle = g_dumb_handle;
  if (drm(g_fd, DRM_IOCTL_MODE_MAP_DUMB, &md) != 0) {
    fail("MAP_DUMB", "get mmap offset", errno);
    return;
  }
  g_map = mmap(NULL, g_dumb_size, PROT_READ | PROT_WRITE, MAP_SHARED, g_fd, (off_t)md.offset);
  if (g_map == MAP_FAILED) {
    fail("mmap dumb", "map the framebuffer for CPU draw", errno);
    return;
  }
  ok("MAP_DUMB+mmap", "framebuffer mapped for CPU access");
  draw_pattern();
  ok("draw pattern", "colour bars written to the framebuffer");

  // ADDFB (legacy) — the simplest registration wlroots' pixman fallback uses.
  struct drm_mode_fb_cmd fb;
  memset(&fb, 0, sizeof fb);
  fb.width = g_w;
  fb.height = g_h;
  fb.pitch = g_pitch;
  fb.bpp = 32;
  fb.depth = 24;
  fb.handle = g_dumb_handle;
  if (drm(g_fd, DRM_IOCTL_MODE_ADDFB, &fb) == 0) {
    g_fb_id = fb.fb_id;
    ok("ADDFB", "framebuffer registered (legacy)");
  } else {
    fail("ADDFB", "register the dumb buffer as a framebuffer", errno);
  }

  // ADDFB2 (modern, fourcc) — what universal-planes compositors use.
  struct drm_mode_fb_cmd2 fb2;
  memset(&fb2, 0, sizeof fb2);
  fb2.width = g_w;
  fb2.height = g_h;
  fb2.pixel_format = DRM_FORMAT_XRGB8888;
  fb2.handles[0] = g_dumb_handle;
  fb2.pitches[0] = g_pitch;
  if (drm(g_fd, DRM_IOCTL_MODE_ADDFB2, &fb2) == 0) {
    g_fb2_id = fb2.fb_id;
    ok("ADDFB2", "framebuffer registered (fourcc XRGB8888)");
  } else {
    info("ADDFB2 unsupported (%s) -- compositor uses legacy ADDFB", strerror(errno));
  }

  if (!g_scanout) {
    // Non-destructive run: tear everything down, leave the screen untouched.
    if (g_fb2_id) {
      struct drm_mode_destroy_dumb d;  // reuse a u32 arg; RMFB takes just the id
      (void)d;
      uint32_t id = g_fb2_id;
      drm(g_fd, DRM_IOCTL_MODE_RMFB, &id);
    }
    if (g_fb_id) {
      uint32_t id = g_fb_id;
      drm(g_fd, DRM_IOCTL_MODE_RMFB, &id);
    }
    if (g_map != MAP_FAILED) munmap(g_map, g_dumb_size);
    struct drm_mode_destroy_dumb dd;
    memset(&dd, 0, sizeof dd);
    dd.handle = g_dumb_handle;
    drm(g_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dd);
    g_fb_id = g_fb2_id = 0;
    g_map = MAP_FAILED;
    info("torn down (read-only run; use --scanout to display the pattern)");
  } else {
    info("kept fb %u for [scanout]", g_fb_id ? g_fb_id : g_fb2_id);
  }
}

// ── prime / dma-buf ──────────────────────────────────────────────────────────

static void test_prime(void) {
  section("prime");
  if (!have_fd("prime")) return;
  if (!g_cap_prime) {
    skip("prime", "PRIME capability not advertised (no dma-buf sharing)");
    return;
  }
  // We need a live handle; make a small throwaway dumb buffer for the round-trip.
  struct drm_mode_create_dumb cd;
  memset(&cd, 0, sizeof cd);
  cd.width = 64;
  cd.height = 64;
  cd.bpp = 32;
  if (drm(g_fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd) != 0) {
    skip("prime", "could not allocate a scratch buffer");
    return;
  }
  struct drm_prime_handle ph;
  memset(&ph, 0, sizeof ph);
  ph.handle = cd.handle;
  ph.flags = O_CLOEXEC;
  ph.fd = -1;
  int exported = drm(g_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) == 0 && ph.fd >= 0;
  check(exported, "PRIME_HANDLE_TO_FD", "export a dumb buffer as a dma-buf fd", errno);
  if (exported) {
    struct drm_prime_handle ih;
    memset(&ih, 0, sizeof ih);
    ih.fd = ph.fd;
    CHECK_CALL(drm(g_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &ih) == 0, "PRIME_FD_TO_HANDLE",
               "re-import the dma-buf fd (Mesa/GBM round-trip)");
    close(ph.fd);
  }
  struct drm_mode_destroy_dumb dd;
  memset(&dd, 0, sizeof dd);
  dd.handle = cd.handle;
  drm(g_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dd);
}

// ── render node ──────────────────────────────────────────────────────────────

static void test_render(void) {
  section("render");
  if (!node_present("/dev/dri/renderD128", S_IFCHR)) {
    skip("render", "no /dev/dri/renderD128");
    return;
  }
  int rfd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
  if (rfd < 0) {
    fail("open renderD128", "compute node", errno);
    return;
  }
  ok("open renderD128", "compute/render node opened");
  char name[64] = "";
  struct drm_version v;
  memset(&v, 0, sizeof v);
  v.name = name;
  v.name_len = sizeof name - 1;
  if (drm(rfd, DRM_IOCTL_VERSION, &v) == 0)
    info("renderD128 driver=\"%s\"", name);
  else
    info("renderD128 VERSION failed (%s)", strerror(errno));
  close(rfd);
}

// ── scanout (opt-in; steals the display from the compositor) ─────────────────

static void test_scanout(void) {
  section("scanout");
  if (!g_scanout) {
    skip("scanout", "not requested (pass --scanout to display the pattern)");
    return;
  }
  if (!have_fd("scanout")) return;
  if (!g_fb_id && !g_fb2_id) {
    fail("scanout", "no framebuffer from [dumb] to scan out", 0);
    return;
  }
  if (!g_have_mode || !g_crtc_id || !g_conn_id) {
    fail("scanout", "need a CRTC, a connected connector and a mode", 0);
    return;
  }
  uint32_t fb = g_fb_id ? g_fb_id : g_fb2_id;
  info("STEALING the display: SETCRTC crtc=%u connector=%u fb=%u %ux%u", g_crtc_id, g_conn_id, fb,
       g_mode.hdisplay, g_mode.vdisplay);

  // A compositor holds DRM master; become master for the modeset. This fails
  // with EBUSY/EACCES if labwc is still running -- that is the expected guard.
  if (drm(g_fd, DRM_IOCTL_SET_MASTER, NULL) != 0)
    info("SET_MASTER failed (%s) -- stop labwc or run from a free VT", strerror(errno));

  uint32_t conns[1] = {g_conn_id};
  struct drm_mode_crtc set;
  memset(&set, 0, sizeof set);
  set.crtc_id = g_crtc_id;
  set.fb_id = fb;
  set.set_connectors_ptr = (uint64_t)(uintptr_t)conns;
  set.count_connectors = 1;
  set.mode = g_mode;
  set.mode_valid = 1;
  if (drm(g_fd, DRM_IOCTL_MODE_SETCRTC, &set) == 0) {
    ok("SETCRTC", "pattern should now be ON SCREEN -- look at the display");
    struct timespec ts = {1, 500 * 1000 * 1000};
    nanosleep(&ts, NULL);  // hold the pattern up long enough to see

    // PAGE_FLIP to the same fb, asking for the completion event on the fd.
    struct drm_mode_crtc_page_flip flip;
    memset(&flip, 0, sizeof flip);
    flip.crtc_id = g_crtc_id;
    flip.fb_id = fb;
    flip.flags = DRM_MODE_PAGE_FLIP_EVENT;
    if (drm(g_fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip) == 0) {
      struct pollfd pfd = {.fd = g_fd, .events = POLLIN};
      if (poll(&pfd, 1, 1000) > 0) {
        char ev[256];
        ssize_t n = read(g_fd, ev, sizeof ev);
        struct drm_event *e = (struct drm_event *)ev;
        int got = n >= (ssize_t)sizeof(*e) &&
                  (e->type == DRM_EVENT_FLIP_COMPLETE || e->type == DRM_EVENT_VBLANK);
        check(got, "PAGE_FLIP event", "flip-complete event delivered on the fd", 0);
      } else {
        fail("PAGE_FLIP event", "no completion event within 1s (vblank not delivered)", errno);
      }
    } else {
      info("PAGE_FLIP unsupported (%s) -- compositor would use SETCRTC per frame", strerror(errno));
    }

    // Explicit vblank wait (the other timing primitive compositors use).
    union drm_wait_vblank wv;
    memset(&wv, 0, sizeof wv);
    wv.request.type = _DRM_VBLANK_RELATIVE;
    wv.request.sequence = 1;
    if (drm(g_fd, DRM_IOCTL_WAIT_VBLANK, &wv) == 0)
      ok("WAIT_VBLANK", "blocked one vblank and returned");
    else
      info("WAIT_VBLANK failed (%s)", strerror(errno));
  } else {
    fail("SETCRTC", "modeset the drawn framebuffer onto the connector", errno);
  }

  drm(g_fd, DRM_IOCTL_DROP_MASTER, NULL);
  // teardown
  if (g_fb2_id) {
    uint32_t id = g_fb2_id;
    drm(g_fd, DRM_IOCTL_MODE_RMFB, &id);
  }
  if (g_fb_id) {
    uint32_t id = g_fb_id;
    drm(g_fd, DRM_IOCTL_MODE_RMFB, &id);
  }
  if (g_map != MAP_FAILED) munmap(g_map, g_dumb_size);
  if (g_dumb_handle) {
    struct drm_mode_destroy_dumb dd;
    memset(&dd, 0, sizeof dd);
    dd.handle = g_dumb_handle;
    drm(g_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dd);
  }
}

// ── nvidia (real hardware only) ──────────────────────────────────────────────

static void test_nvidia(void) {
  section("nvidia");
  int is_nv = strstr(g_driver, "nouveau") || strstr(g_driver, "nvidia");
  if (!is_nv && !g_force_hw) {
    skip("nvidia", "driver is not nouveau/nvidia (QEMU virtio-gpu) -- needs real GPU");
    info("on real hardware this section checks: nouveau BAR0 modeset, HDMI");
    info("connector ELD (via /proc/gpusnd), GSP state, and multi-GPU card order");
    return;
  }
  // Structured placeholders to fill in when a run happens on real hardware.
  info("driver=\"%s\": running the hardware-specific checks", g_driver);
  if (node_present("/proc/gpusnd", S_IFREG)) {
    ok("/proc/gpusnd", "HDMI/codec state file present");
    if (g_verbose) dump_file("/proc/gpusnd", "gpusnd");
  } else {
    skip("/proc/gpusnd", "no audio-pipeline state file");
  }
  // TODO(v0.5.5+, real HW): assert an HDMI/DP connector reports present+ELD,
  // that the nouveau BAR0 ELD/HPD path armed, and that a second GPU with no
  // display is NOT registered as card0. These need a monitor on the GPU.
  info("(hardware assertions are TODO -- structured for a real-GPU run)");
}

// ── main ─────────────────────────────────────────────────────────────────────

int main(int argc, char **argv) {
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "-v")) g_verbose = 1;
    else if (!strcmp(argv[i], "--scanout")) g_scanout = 1;
    else if (!strcmp(argv[i], "--hw")) g_force_hw = 1;
    else if (!strcmp(argv[i], "--card") && i + 1 < argc) g_card = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--skip") && i + 1 < argc && g_nskipped < 16)
      g_skipped[g_nskipped++] = argv[++i];
    else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
      printf("usage: drm-probe [-v] [--card N] [--scanout] [--hw] [--skip SECTION]...\n"
             "Drives every DRM/KMS layer wlroots/labwc and Mesa-GBM rest on -- the\n"
             "device nodes, VERSION/GET_CAP, the CRTC/connector/plane graph, EDID,\n"
             "a dumb framebuffer with a drawn test pattern, and the PRIME round-trip\n"
             "-- and says whether a compositor's DRM backend would initialise.\n"
             "Read-only by default (safe under a running compositor).\n"
             "--scanout  additionally modesets the drawn pattern ONTO the display\n"
             "           (steals DRM master; run from a free VT or with labwc stopped).\n"
             "--hw       force the nvidia/hardware section even under virtio-gpu.\n"
             "--skip SECTION leaves one out (nodes, version, caps, resources,\n"
             "           connectors, edid, planes, dumb, prime, render, scanout, nvidia).\n");
      return 0;
    }
  }

  snprintf(g_cardpath, sizeof g_cardpath, "/dev/dri/card%d", g_card);
  printf("Compositor-shaped DRM/KMS probe (card %d%s)\n", g_card,
         g_scanout ? ", scanout ON" : "");
  printf("Each check mirrors what wlroots/labwc or Mesa-GBM asks of the kernel.\n");

  struct {
    const char *name;
    void (*run)(void);
  } sections[] = {
      {"nodes", test_nodes},         {"version", test_version}, {"caps", test_caps},
      {"resources", test_resources}, {"connectors", test_connectors}, {"edid", test_edid},
      {"planes", test_planes},       {"dumb", test_dumb},       {"prime", test_prime},
      {"render", test_render},        {"scanout", test_scanout}, {"nvidia", test_nvidia},
  };
  for (size_t i = 0; i < sizeof sections / sizeof sections[0]; i++) {
    if (skipped(sections[i].name)) {
      printf("\n[%s] skipped (--skip)\n", sections[i].name);
      continue;
    }
    sections[i].run();
  }

  section("verdict");
  int kms = g_fd >= 0 && g_crtc_id && g_conn_id && g_have_mode;
  printf("  driver: %s\n", g_driver[0] ? g_driver : "unknown (VERSION failed)");
  printf("  KMS modeset path: %s\n",
         kms ? "present (CRTC + connected connector + mode)" : "INCOMPLETE (see above)");
  printf("  dumb framebuffer: %s\n", g_fb_id || g_fb2_id || (g_cap_dumb && g_fd >= 0)
                                         ? "works (CREATE_DUMB/MAP/ADDFB)"
                                         : "BROKEN -- pixman/wl_shm backend cannot present");
  printf("  EDID: %s\n", g_edid_ok ? "readable" : "absent/unreadable (native-mode detection blind)");
  if (kms && g_cap_dumb)
    printf("  => wlroots' DRM backend should initialise; labwc/pixman can scan out.\n");
  else
    printf("  => a compositor's DRM backend would FAIL here -- this is the black screen.\n");
  if (!g_scanout)
    printf("  (run with --scanout, from a free VT or with labwc stopped, to SEE the pattern.)\n");

  printf("\n%d passed, %d failed, %d skipped\n", g_pass, g_fail, g_skip);
  if (g_fail == 0) printf("Nothing here stands in the way of the display.\n");
  return g_fail > 125 ? 125 : g_fail;
}
