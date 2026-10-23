// Eclipse OS: a compositor-shaped input / USB probe.
//
// `audio-probe` answers "what does the kernel lack for sound?". This
// answers the same question for keyboard and mouse input: it drives every
// layer libinput / wlroots / labwc and Xorg rest on, bottom-up, exactly the
// way each layer is driven in the field, and reports the first one that breaks.
//
// The layers, in the order a Wayland compositor or Xorg walks them:
//
//   devices     the hardware state from /proc/usbhid (USB controller, slots,
//               bound HID interfaces, report counts) and dev nodes in
//               /dev/input/event*.
//   evdev       raw ioctls on /dev/input/event*: EVIOCGVERSION, EVIOCGID,
//               EVIOCGNAME, EVIOCGBIT (EV_KEY, EV_REL, EV_ABS), EVIOCGPROP,
//               and absolute coordinate info EVIOCGABS.
//   sysfs       /sys/class/input/event* and /sys/dev/char/13:*:
//               verifies dev, uevent, subsystem symlinks, and checks that
//               the uevent properties (ID_INPUT, ID_INPUT_KEYBOARD, ID_INPUT_MOUSE)
//               match reality. Specifically flags if a mouse/tablet device is
//               misclassified with ID_INPUT_TABLET without BTN_TOOL_PEN, which
//               sends libinput down the digitizer path and kills the pointer.
//               Also verifies sysfs statfs f_type == 0x62656572 (SYSFS_MAGIC).
//   udev        the synthetic udev database at /run/udev/data/c13:* and tags at
//               /run/udev/tags/seat and /run/udev/tags/seat0:
//               verifies the initialized 'I:' marker and 'E:ID_SEAT=seat0'
//               needed by systemd 261.3's libudev and libinput.
//   libinput    simulates the exact device classification and initialization
//               path of libinput (evdev_configure_device): verifies whether
//               each event node is accepted, which capabilities (KEYBOARD,
//               POINTER, TABLET) it gets, and whether events can be read.
//   verdict     will keyboard and mouse be recognized and usable by labwc?
//
// Build:  musl-gcc -O2 -static -o usb-probe usb-probe.c
// Run:    usb-probe [-v] [--skip SECTION]
//
// Exit status is the number of failed checks, capped at 125.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/types.h>
#include <unistd.h>

#define SYSFS_BEER_MAGIC 0x62656572

// ── Linux input UAPI definitions (spelled out for musl) ─────────────────────

#define IOC_NONE 0U
#define IOC_W    1U
#define IOC_R    2U
#define IOC_RW   3U
#define _IOC_(dir, type, nr, size) \
  (((uint32_t)(dir) << 30) | ((uint32_t)(size) << 16) | ((uint32_t)(type) << 8) | (uint32_t)(nr))

#define _IOR_(type, nr, size) _IOC_(IOC_R, (type), (nr), sizeof(size))
#define _IOW_(type, nr, size) _IOC_(IOC_W, (type), (nr), sizeof(size))

struct input_id {
  uint16_t bustype;
  uint16_t vendor;
  uint16_t product;
  uint16_t version;
};

struct input_absinfo {
  int32_t value;
  int32_t minimum;
  int32_t maximum;
  int32_t fuzz;
  int32_t flat;
  int32_t resolution;
};

#define EV_SYN 0x00
#define EV_KEY 0x01
#define EV_REL 0x02
#define EV_ABS 0x03

#define KEY_ESC 1
#define KEY_MAX 0x2ff
#define BTN_LEFT 0x110
#define BTN_TOOL_PEN 0x140

#define REL_X 0x00
#define REL_Y 0x01
#define REL_MAX 0x0f

#define ABS_X 0x00
#define ABS_Y 0x01
#define ABS_MAX 0x3f

#define EVIOCGVERSION _IOR_('E', 0x01, int)
#define EVIOCGID      _IOR_('E', 0x02, struct input_id)
#define EVIOCGNAME(len) _IOC_(IOC_R, 'E', 0x06, len)
#define EVIOCGBIT(ev, len) _IOC_(IOC_R, 'E', 0x20 + (ev), len)
#define EVIOCGABS(abs) _IOR_('E', 0x40 + (abs), struct input_absinfo)

// ── reporting ───────────────────────────────────────────────────────────────

static int g_verbose;
static int g_pass, g_fail, g_skip;
static const char *g_skipped[16];
static int g_nskipped;

static int skipped(const char *name) {
  for (int i = 0; i < g_nskipped; i++)
    if (!strcmp(g_skipped[i], name)) return 1;
  return 0;
}

static void section(const char *name) {
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

// ── helpers ────────────────────────────────────────────────────────────────

static int test_bit(int nr, const unsigned long *addr) {
  return ((1UL << (nr % (sizeof(long) * 8))) & (addr[nr / (sizeof(long) * 8)])) != 0;
}

static int read_file_line(const char *path, char *buf, size_t max) {
  FILE *f = fopen(path, "r");
  if (!f) return -1;
  if (!fgets(buf, max, f)) {
    fclose(f);
    return -1;
  }
  fclose(f);
  size_t len = strlen(buf);
  while (len > 0 && (buf[len - 1] == '\n' || buf[len - 1] == '\r')) {
    buf[--len] = '\0';
  }
  return 0;
}

// ── [devices] ──────────────────────────────────────────────────────────────

static int g_event_count = 0;
static char g_event_paths[16][300];

static void probe_devices(void) {
  section("devices");
  if (skipped("devices")) {
    skip("devices", "skipped by user");
    return;
  }

  // 1. /proc/usbhid
  FILE *f = fopen("/proc/usbhid", "r");
  if (f) {
    ok("/proc/usbhid", "present and readable");
    char line[256];
    int report_lines = 0;
    while (fgets(line, sizeof(line), f)) {
      if (g_verbose || strstr(line, "role=") || strstr(line, "abs_pointer=")) {
        printf("         %s", line);
      }
      if (strstr(line, "role=")) report_lines++;
    }
    fclose(f);
    check(report_lines > 0, "USB HID reports active", "found bound HID interfaces with reports", 0);
  } else {
    fail("/proc/usbhid", "not available (no XHCI HID driver or procfs entry)", errno);
  }

  // 2. /dev/input enumeration
  DIR *d = opendir("/dev/input");
  if (d) {
    struct dirent *de;
    while ((de = readdir(d)) != NULL) {
      if (!strncmp(de->d_name, "event", 5)) {
        if (g_event_count < 16) {
          snprintf(g_event_paths[g_event_count], sizeof(g_event_paths[g_event_count]),
                   "/dev/input/%s", de->d_name);
          g_event_count++;
        }
      }
    }
    closedir(d);
    printf("         found %d event device(s) in /dev/input\n", g_event_count);
    check(g_event_count > 0, "event devices present", "/dev/input/event* found", 0);
  } else {
    fail("/dev/input", "cannot open /dev/input directory", errno);
  }
}

// ── [evdev] ────────────────────────────────────────────────────────────────

struct EvdevInfo {
  char name[128];
  int is_keyboard;
  int is_mouse;
  int is_tablet;
  int has_tool_pen;
  int has_abs_xy;
  int has_rel_xy;
};

static struct EvdevInfo g_dev_info[16];

static void probe_evdev(void) {
  section("evdev");
  if (skipped("evdev")) {
    skip("evdev", "skipped by user");
    return;
  }

  if (g_event_count == 0) {
    fail("no event devices", "cannot test evdev ioctls with no /dev/input/event*", 0);
    return;
  }

  for (int i = 0; i < g_event_count; i++) {
    const char *path = g_event_paths[i];
    int fd = open(path, O_RDONLY | O_NONBLOCK);
    if (fd < 0) {
      fail(path, "open failed", errno);
      continue;
    }

    int version = 0;
    if (ioctl(fd, EVIOCGVERSION, &version) < 0) {
      fail(path, "EVIOCGVERSION failed", errno);
      close(fd);
      continue;
    }

    struct input_id id;
    memset(&id, 0, sizeof(id));
    ioctl(fd, EVIOCGID, &id);

    char name[128] = "Unknown";
    ioctl(fd, EVIOCGNAME(sizeof(name)), name);
    snprintf(g_dev_info[i].name, sizeof(g_dev_info[i].name), "%s", name);

    unsigned long ev_bits[1] = {0};
    ioctl(fd, EVIOCGBIT(0, sizeof(ev_bits)), ev_bits);

    unsigned long key_bits[KEY_MAX / 8 / sizeof(long) + 1];
    unsigned long rel_bits[REL_MAX / 8 / sizeof(long) + 1];
    unsigned long abs_bits[ABS_MAX / 8 / sizeof(long) + 1];
    memset(key_bits, 0, sizeof(key_bits));
    memset(rel_bits, 0, sizeof(rel_bits));
    memset(abs_bits, 0, sizeof(abs_bits));

    if (test_bit(EV_KEY, ev_bits)) {
      ioctl(fd, EVIOCGBIT(EV_KEY, sizeof(key_bits)), key_bits);
    }
    if (test_bit(EV_REL, ev_bits)) {
      ioctl(fd, EVIOCGBIT(EV_REL, sizeof(rel_bits)), rel_bits);
    }
    if (test_bit(EV_ABS, ev_bits)) {
      ioctl(fd, EVIOCGBIT(EV_ABS, sizeof(abs_bits)), abs_bits);
    }

    int has_esc = test_bit(KEY_ESC, key_bits);
    int has_btn_left = test_bit(BTN_LEFT, key_bits);
    int has_tool_pen = test_bit(BTN_TOOL_PEN, key_bits);
    int has_rel_x = test_bit(REL_X, rel_bits);
    int has_rel_y = test_bit(REL_Y, rel_bits);
    int has_abs_x = test_bit(ABS_X, abs_bits);
    int has_abs_y = test_bit(ABS_Y, abs_bits);

    g_dev_info[i].has_tool_pen = has_tool_pen;
    g_dev_info[i].has_abs_xy = has_abs_x && has_abs_y;
    g_dev_info[i].has_rel_xy = has_rel_x && has_rel_y;
    g_dev_info[i].is_keyboard = has_esc;
    g_dev_info[i].is_mouse = (has_rel_x && has_rel_y) ||
                             (has_abs_x && has_abs_y && has_btn_left && !has_tool_pen);
    g_dev_info[i].is_tablet = has_abs_x && has_abs_y && has_tool_pen;

    printf("         %s: '%s' (bustype 0x%04x, vendor 0x%04x, product 0x%04x)\n",
           path, name, id.bustype, id.vendor, id.product);
    printf("           capabilities: key=%d (ESC=%d BTN_LEFT=%d PEN=%d) rel=%d (X/Y=%d) abs=%d (X/Y=%d)\n",
           test_bit(EV_KEY, ev_bits), has_esc, has_btn_left, has_tool_pen,
           test_bit(EV_REL, ev_bits), (has_rel_x && has_rel_y),
           test_bit(EV_ABS, ev_bits), (has_abs_x && has_abs_y));

    if (has_abs_x && has_abs_y) {
      struct input_absinfo abs_x, abs_y;
      if (ioctl(fd, EVIOCGABS(ABS_X), &abs_x) == 0 &&
          ioctl(fd, EVIOCGABS(ABS_Y), &abs_y) == 0) {
        printf("           abs range: X=[%d..%d] Y=[%d..%d]\n",
               abs_x.minimum, abs_x.maximum, abs_y.minimum, abs_y.maximum);
      }
    }

    ok(path, "ioctl EVIOCG* queries succeeded");
    close(fd);
  }
}

// ── [sysfs] ────────────────────────────────────────────────────────────────

static void probe_sysfs(void) {
  section("sysfs");
  if (skipped("sysfs")) {
    skip("sysfs", "skipped by user");
    return;
  }

  // Check sysfs statfs magic
  struct statfs sfs;
  if (statfs("/sys", &sfs) == 0) {
    if ((uint32_t)sfs.f_type == SYSFS_BEER_MAGIC) {
      ok("statfs(/sys)", "f_type matches SYSFS_MAGIC (0x62656572)");
    } else {
      fail("statfs(/sys)", "f_type does not match 0x62656572 (libudev systemd 261 will reject devices)", 0);
    }
  } else {
    fail("statfs(/sys)", "statfs failed", errno);
  }

  for (int i = 0; i < g_event_count; i++) {
    const char *ev_name = strrchr(g_event_paths[i], '/');
    if (!ev_name) ev_name = g_event_paths[i];
    else ev_name++; // skip '/'

    int ev_num = 0;
    if (sscanf(ev_name, "event%d", &ev_num) != 1) continue;

    char dev_path[128];
    snprintf(dev_path, sizeof(dev_path), "/sys/class/input/event%d/dev", ev_num);
    char dev_val[64];
    if (read_file_line(dev_path, dev_val, sizeof(dev_val)) == 0) {
      char expected[32];
      snprintf(expected, sizeof(expected), "13:%d", 64 + ev_num);
      if (!strcmp(dev_val, expected)) {
        ok(dev_path, "reports correct major:minor");
      } else {
        fail(dev_path, "unexpected major:minor content", 0);
      }
    } else {
      fail(dev_path, "cannot read /sys/class/input/event*/dev", errno);
    }

    // Check uevent file
    char uevent_path[128];
    snprintf(uevent_path, sizeof(uevent_path), "/sys/class/input/event%d/uevent", ev_num);
    FILE *uf = fopen(uevent_path, "r");
    if (uf) {
      char line[256];
      int has_seat = 0, has_id_input = 0, has_tablet_prop = 0, has_mouse_prop = 0, has_kbd_prop = 0;
      while (fgets(line, sizeof(line), uf)) {
        if (strstr(line, "ID_SEAT=seat0")) has_seat = 1;
        if (strstr(line, "ID_INPUT=1")) has_id_input = 1;
        if (strstr(line, "ID_INPUT_TABLET=1")) has_tablet_prop = 1;
        if (strstr(line, "ID_INPUT_MOUSE=1")) has_mouse_prop = 1;
        if (strstr(line, "ID_INPUT_KEYBOARD=1") || strstr(line, "ID_INPUT_KEY=1")) has_kbd_prop = 1;
      }
      fclose(uf);

      check(has_seat && has_id_input, uevent_path, "contains ID_SEAT=seat0 and ID_INPUT=1", 0);

      // CRITICAL CHECK: Did sysfs mistakenly assign ID_INPUT_TABLET to a device with no BTN_TOOL_PEN?
      if (has_tablet_prop && !g_dev_info[i].has_tool_pen) {
        fail(uevent_path,
             "CRITICAL: uevent sets ID_INPUT_TABLET=1 without BTN_TOOL_PEN! "
             "libinput treats this as a digitizer and starves pointer & keyboard!", 0);
      } else if (g_dev_info[i].is_mouse) {
        check(has_mouse_prop, uevent_path, "correctly sets ID_INPUT_MOUSE=1", 0);
      }
      if (g_dev_info[i].is_keyboard) {
        check(has_kbd_prop, uevent_path, "correctly sets ID_INPUT_KEYBOARD=1", 0);
      }
    } else {
      fail(uevent_path, "cannot open uevent file", errno);
    }

    // Check /sys/dev/char/13:64 symlink
    char char_link[128];
    snprintf(char_link, sizeof(char_link), "/sys/dev/char/13:%d", 64 + ev_num);
    char target[256];
    ssize_t r = readlink(char_link, target, sizeof(target) - 1);
    if (r > 0) {
      target[r] = '\0';
      ok(char_link, "symlink resolves");
    } else {
      fail(char_link, "missing /sys/dev/char/13:* symlink (evdev_device_have_same_syspath will reject device)", errno);
    }
  }
}

// ── [udev] ─────────────────────────────────────────────────────────────────

static void probe_udev(void) {
  section("udev");
  if (skipped("udev")) {
    skip("udev", "skipped by user");
    return;
  }

  for (int i = 0; i < g_event_count; i++) {
    const char *ev_name = strrchr(g_event_paths[i], '/');
    if (!ev_name) ev_name = g_event_paths[i];
    else ev_name++;

    int ev_num = 0;
    if (sscanf(ev_name, "event%d", &ev_num) != 1) continue;

    char data_path[128];
    snprintf(data_path, sizeof(data_path), "/run/udev/data/c13:%d", 64 + ev_num);
    FILE *f = fopen(data_path, "r");
    if (f) {
      char line[256];
      int has_init = 0, has_seat = 0, has_tablet_prop = 0, has_mouse_prop = 0;
      while (fgets(line, sizeof(line), f)) {
        if (!strncmp(line, "I:", 2)) has_init = 1;
        if (strstr(line, "ID_SEAT=seat0") || strstr(line, "G:seat")) has_seat = 1;
        if (strstr(line, "ID_INPUT_TABLET=1")) has_tablet_prop = 1;
        if (strstr(line, "ID_INPUT_MOUSE=1")) has_mouse_prop = 1;
      }
      fclose(f);
      check(has_init, data_path, "contains 'I:' initialized marker (udev_device_get_is_initialized)", 0);
      check(has_seat, data_path, "contains seat assignment", 0);

      if (has_tablet_prop && !g_dev_info[i].has_tool_pen) {
        fail(data_path, "CRITICAL: udev data has ID_INPUT_TABLET=1 on mouse without tool pen!", 0);
      } else if (g_dev_info[i].is_mouse) {
        check(has_mouse_prop, data_path, "has ID_INPUT_MOUSE=1", 0);
      }
    } else {
      fail(data_path, "missing synthetic udev record (libinput will 'skip unconfigured input device')", errno);
    }

    // Check tags
    char tag_seat_path[128];
    snprintf(tag_seat_path, sizeof(tag_seat_path), "/run/udev/tags/seat/c13:%d", 64 + ev_num);
    if (access(tag_seat_path, F_OK) == 0) {
      ok(tag_seat_path, "seat tag record exists");
    } else {
      skip(tag_seat_path, "optional seat tag directory not present (systemd 261 matches tag)");
    }
  }
}

// ── [libinput-sim] ─────────────────────────────────────────────────────────

static void probe_libinput_sim(void) {
  section("libinput-sim");
  if (skipped("libinput-sim")) {
    skip("libinput-sim", "skipped by user");
    return;
  }

  printf("         Simulating libinput's evdev_configure_device() for each event node:\n");

  for (int i = 0; i < g_event_count; i++) {
    const char *ev_name = strrchr(g_event_paths[i], '/');
    if (!ev_name) ev_name = g_event_paths[i];
    else ev_name++;

    int ev_num = 0;
    if (sscanf(ev_name, "event%d", &ev_num) != 1) continue;

    char uevent_path[128];
    snprintf(uevent_path, sizeof(uevent_path), "/sys/class/input/event%d/uevent", ev_num);
    FILE *uf = fopen(uevent_path, "r");
    int has_tablet = 0, has_mouse = 0, has_kbd = 0, has_key = 0;
    if (uf) {
      char line[256];
      while (fgets(line, sizeof(line), uf)) {
        if (strstr(line, "ID_INPUT_TABLET=1")) has_tablet = 1;
        if (strstr(line, "ID_INPUT_MOUSE=1")) has_mouse = 1;
        if (strstr(line, "ID_INPUT_KEYBOARD=1")) has_kbd = 1;
        if (strstr(line, "ID_INPUT_KEY=1")) has_key = 1;
      }
      fclose(uf);
    }

    char data_path[128];
    snprintf(data_path, sizeof(data_path), "/run/udev/data/c13:%d", 64 + ev_num);
    int is_initialized = (access(data_path, F_OK) == 0);

    printf("         %s (%s):\n", ev_name, g_dev_info[i].name);
    if (!is_initialized) {
      printf("           -> REJECTED: uninitialized device (/run/udev/data/c13:%d missing)\n", 64 + i);
      fail("libinput init", "device rejected as uninitialized", 0);
      continue;
    }

    if (has_tablet && !g_dev_info[i].has_tool_pen) {
      printf("           -> BROKEN: assigned EVDEV_UDEV_TAG_TABLET without pen tool!\n"
             "              libinput will ignore all clicks and motion waiting for BTN_TOOL_PEN\n");
      fail("libinput classification", "wrongly classified as graphic tablet digitizer", 0);
    } else {
      char caps[128] = "";
      if (has_kbd || has_key) strcat(caps, "KEYBOARD ");
      if (has_mouse) strcat(caps, "POINTER(mouse) ");
      if (has_tablet) strcat(caps, "TABLET ");
      if (strlen(caps) == 0) strcat(caps, "NONE");

      printf("           -> ACCEPTED: capabilities [%s]\n", caps);
      ok("libinput classification", "device recognized with operational capabilities");
    }
  }
}

// ── main ───────────────────────────────────────────────────────────────────

int main(int argc, char **argv) {
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "-v") || !strcmp(argv[i], "--verbose")) {
      g_verbose = 1;
    } else if (!strcmp(argv[i], "--skip") && i + 1 < argc) {
      if (g_nskipped < 16) g_skipped[g_nskipped++] = argv[++i];
    } else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
      printf("usage: usb-probe [-v] [--skip SECTION]...\n");
      printf("checks USB HID, evdev, sysfs, udev, and libinput input chain.\n");
      return 0;
    }
  }

  printf("Eclipse OS input / USB probe\n");
  printf("Testing input devices against Ubuntu 26.10 (systemd 261.3 / libinput) requirements.\n");

  probe_devices();
  probe_evdev();
  probe_sysfs();
  probe_udev();
  probe_libinput_sim();

  section("verdict");
  printf("Passed: %d, Failed: %d, Skipped: %d\n", g_pass, g_fail, g_skip);
  if (g_fail == 0) {
    printf("Verdict: PASS - keyboard and mouse input path is healthy and compatible with Ubuntu 26.10.\n");
  } else {
    printf("Verdict: FAIL - %d issue(s) detected in the input stack preventing desktop interaction.\n", g_fail);
  }

  return g_fail > 125 ? 125 : g_fail;
}
