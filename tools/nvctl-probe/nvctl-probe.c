// Eclipse OS: a probe of /dev/nvidiactl, driven the way libnvidia-ml drives it.
//
// NVIDIA's own user-mode driver (libnvidia-ml, so nvidia-smi; libcuda) does
// not speak DRM. It opens /dev/nvidiactl and /dev/nvidia<N> and talks to the
// Resource Manager through `_IOWR('F', escape, ...)` ioctls. This probe walks
// the same steps nvidia-smi takes to print `nvidia-smi -L`, with no NVIDIA
// library at all, and reports the first that breaks:
//
//   version   CHECK_VERSION_STR: ask the RM for its version, then the strict
//             check libnvidia-ml makes with it. A mismatch here is what
//             nvidia-smi calls "Driver/library version mismatch".
//   cards     CARD_INFO: every GPU the kernel lists, with its minor, PCI
//             location and apertures.
//   client    RM_ALLOC NV01_ROOT_CLIENT on /dev/nvidiactl.
//   attached  NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS: the GPU ids the RM has.
//   per GPU   open /dev/nvidia<minor>, REGISTER_FD, GET_ID_INFO_V2, then
//             NV01_DEVICE_0 + NV20_SUBDEVICE_0 (the RM only allows the device
//             to a process holding the GPU's node open), and from the
//             subdevice the name, the UUID and the memory size -- one
//             `nvidia-smi -L` line.
//   map       the two mappings every CUDA context makes, the way libcuda
//             makes them: RM_MAP_MEMORY recorded on a fresh fd, then mmap
//             of that fd at offset 0, then RM_UNMAP_MEMORY.
//               sysmem    64 KiB of NV01_MEMORY_SYSTEM, on a fresh
//                         /dev/nvidiactl: a pattern written and read back.
//               usermode  TURING_USERMODE_A, BAR0's user page, on a fresh
//                         /dev/nvidia<minor>: the GPU timer read twice
//                         must move forward.
//   event     an OS event, the way libnvidia-ml waits for one: a fresh
//             /dev/nvidiactl registered with ALLOC_OS_EVENT, an
//             NV01_EVENT_OS_EVENT on the subdevice bound to it, and the
//             subdevice's software notifier armed and fired
//             (EVENT_SET_NOTIFICATION, EVENT_SET_TRIGGER). The fd must poll
//             readable, and RM_GET_EVENT_DATA must hand back that event.
//   free      RM_FREE of the client: everything under it goes.
//
// Needs the kernel booted with `nvidia.nvidiactl`. Read-only as far as the
// GPU goes: it allocates RM objects and frees them, writes only the system
// memory it allocated, reads the GPU's timer, fires the software notifier
// it armed, and touches no engine.
//
// Build: static musl, like the other probes (xtask does it; by hand:
//   x86_64-linux-musl-gcc -static -O2 -o nvctl-probe nvctl-probe.c).

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

// nv-ioctl-numbers.h / nv_escape.h
#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_CARD_INFO 200
#define NV_ESC_REGISTER_FD 201
#define NV_ESC_ALLOC_OS_EVENT 206
#define NV_ESC_FREE_OS_EVENT 207
#define NV_ESC_CHECK_VERSION_STR 210
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_CONTROL 0x2A
#define NV_ESC_RM_ALLOC 0x2B
#define NV_ESC_RM_MAP_MEMORY 0x4E
#define NV_ESC_RM_UNMAP_MEMORY 0x4F
#define NV_ESC_RM_GET_EVENT_DATA 0x52

// classes and controls (class/, ctrl/)
#define NV01_ROOT_CLIENT 0x41
#define NV01_DEVICE_0 0x80
#define NV20_SUBDEVICE_0 0x2080
#define NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS 0x201
#define NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2 0x205
#define NV2080_CTRL_CMD_GPU_GET_NAME_STRING 0x20800110
#define NV2080_CTRL_CMD_GPU_GET_GID_INFO 0x2080014a
#define NV2080_CTRL_CMD_FB_GET_INFO_V2 0x20801303
#define NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE 7
#define NV_DEVICE_ALLOCATION_VAMODE_MULTIPLE_VASPACES 2
#define NV0000_CTRL_GPU_INVALID_ID 0xffffffffu
#define NV01_MEMORY_SYSTEM 0x3e
#define TURING_USERMODE_A 0xc461
#define NVC361_NV_USERMODE__SIZE 0x10000
#define NVC361_TIME_0 0x80
#define NVC361_TIME_1 0x84
#define NV01_EVENT_OS_EVENT 0x79
#define NV2080_NOTIFIERS_SW 0
#define NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION 0x20800301
#define NV2080_CTRL_CMD_EVENT_SET_TRIGGER 0x20800302
#define NV2080_CTRL_EVENT_SET_NOTIFICATION_ACTION_SINGLE 1
// NVOS32_ATTR_*: 4 KiB pages (24:23), in system memory (26:25), cached (31:29)
#define NVOS32_ATTR_SYSMEM_4K_CACHED ((1u << 23) | (1u << 25) | (1u << 29))
#define NVOS32_TYPE_IMAGE 0

#define NV_MAX_DEVICES 32

// nv-ioctl.h, x86_64
typedef struct {
  uint32_t domain;
  uint8_t bus, slot, function;
  uint16_t vendor_id, device_id;
} nv_pci_info_t;

typedef struct {
  uint8_t valid;
  nv_pci_info_t pci_info;
  uint32_t gpu_id;
  uint16_t interrupt_line;
  uint64_t reg_address __attribute__((aligned(8)));
  uint64_t reg_size __attribute__((aligned(8)));
  uint64_t fb_address __attribute__((aligned(8)));
  uint64_t fb_size __attribute__((aligned(8)));
  uint32_t minor_number;
  uint8_t dev_name[10];
} nv_ioctl_card_info_t;

typedef struct {
  uint32_t cmd;
  uint32_t reply;
  char versionString[64];
} nv_ioctl_rm_api_version_t;

typedef struct {
  int ctl_fd;
} nv_ioctl_register_fd_t;

// nv_ioctl_alloc_os_event_t, and nv_ioctl_free_os_event_t, which is the same.
typedef struct {
  uint32_t hClient, hDevice, fd, Status;
} nv_ioctl_alloc_os_event_t;

// nvos.h
typedef struct {
  uint32_t hRoot, hObjectParent, hObjectNew, hClass;
  uint64_t pAllocParms __attribute__((aligned(8)));
  uint32_t paramsSize, status;
} NVOS21_PARAMETERS;

typedef struct {
  uint32_t hRoot, hObjectParent, hObjectOld, status;
} NVOS00_PARAMETERS;

typedef struct {
  uint32_t hClient, hObject, cmd, flags;
  uint64_t params __attribute__((aligned(8)));
  uint32_t paramsSize, status;
} NVOS54_PARAMETERS;

typedef struct {
  uint32_t deviceId, hClientShare, hTargetClient, hTargetDevice, flags;
  uint64_t vaSpaceSize __attribute__((aligned(8)));
  uint64_t vaStartInternal __attribute__((aligned(8)));
  uint64_t vaLimitInternal __attribute__((aligned(8)));
  uint32_t vaMode;
} NV0080_ALLOC_PARAMETERS;

typedef struct {
  uint32_t subDeviceId;
} NV2080_ALLOC_PARAMETERS;

typedef struct {
  uint32_t owner, type, flags, width, height;
  int32_t pitch;
  uint32_t attr, attr2, format, comprCovg, zcullCovg;
  uint64_t rangeLo __attribute__((aligned(8)));
  uint64_t rangeHi __attribute__((aligned(8)));
  uint64_t size __attribute__((aligned(8)));
  uint64_t alignment __attribute__((aligned(8)));
  uint64_t offset __attribute__((aligned(8)));
  uint64_t limit __attribute__((aligned(8)));
  uint64_t address __attribute__((aligned(8)));
  uint32_t ctagOffset, hVASpace, internalflags, tag;
  int32_t numaNode;
} NV_MEMORY_ALLOCATION_PARAMS;

typedef struct {
  uint32_t hClient, hDevice, hMemory;
  uint64_t offset __attribute__((aligned(8)));
  uint64_t length __attribute__((aligned(8)));
  uint64_t pLinearAddress __attribute__((aligned(8)));
  uint32_t status, flags;
} NVOS33_PARAMETERS;

// nv-unix-nvos-params-wrappers.h: the fd the mapping is recorded on.
typedef struct {
  NVOS33_PARAMETERS params;
  int fd;
} nv_ioctl_nvos33_parameters_with_fd;

typedef struct {
  uint32_t hClient, hDevice, hMemory;
  uint64_t pLinearAddress __attribute__((aligned(8)));
  uint32_t status, flags;
} NVOS34_PARAMETERS;

typedef struct {
  uint64_t pEvent __attribute__((aligned(8)));
  uint32_t MoreEvents, status;
} NVOS41_PARAMETERS;

typedef struct {
  uint32_t hObject, NotifyIndex, info32;
  uint16_t info16;
} NvUnixEvent;

// cl0005.h
typedef struct {
  uint32_t hParentClient, hSrcResource, hClass, notifyIndex;
  uint64_t data __attribute__((aligned(8)));
} NV0005_ALLOC_PARAMETERS;

typedef struct {
  uint32_t gpuId, gpuFlags, deviceInstance, subDeviceInstance, sliStatus, boardId, gpuInstance;
  int32_t numaId;
} NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS;

typedef struct {
  uint32_t gpuNameStringFlags;
  union {
    uint8_t ascii[64];
    uint16_t unicode[64];
  } gpuNameString;
} NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS;

typedef struct {
  uint32_t index, flags, length;
  uint8_t data[256];
} NV2080_CTRL_GPU_GET_GID_INFO_PARAMS;

typedef struct {
  uint32_t index, data;
} NV2080_CTRL_FB_INFO;

typedef struct {
  uint32_t fbInfoListSize;
  NV2080_CTRL_FB_INFO fbInfoList[128];
} NV2080_CTRL_FB_GET_INFO_V2_PARAMS;

typedef struct {
  uint32_t event, action;
  uint8_t bNotifyState;
  uint32_t info32;
  uint16_t info16;
} NV2080_CTRL_EVENT_SET_NOTIFICATION_PARAMS;

_Static_assert(sizeof(nv_ioctl_card_info_t) == 72, "nv_ioctl_card_info_t");
_Static_assert(sizeof(nv_ioctl_rm_api_version_t) == 72, "nv_ioctl_rm_api_version_t");
_Static_assert(sizeof(NVOS21_PARAMETERS) == 32, "NVOS21_PARAMETERS");
_Static_assert(sizeof(NVOS00_PARAMETERS) == 16, "NVOS00_PARAMETERS");
_Static_assert(sizeof(NVOS54_PARAMETERS) == 32, "NVOS54_PARAMETERS");
_Static_assert(sizeof(NV0080_ALLOC_PARAMETERS) == 56, "NV0080_ALLOC_PARAMETERS");
_Static_assert(sizeof(NV_MEMORY_ALLOCATION_PARAMS) == 128, "NV_MEMORY_ALLOCATION_PARAMS");
_Static_assert(sizeof(nv_ioctl_nvos33_parameters_with_fd) == 56,
               "nv_ioctl_nvos33_parameters_with_fd");
_Static_assert(sizeof(NVOS34_PARAMETERS) == 32, "NVOS34_PARAMETERS");
_Static_assert(sizeof(nv_ioctl_alloc_os_event_t) == 16, "nv_ioctl_alloc_os_event_t");
_Static_assert(sizeof(NVOS41_PARAMETERS) == 16, "NVOS41_PARAMETERS");
_Static_assert(sizeof(NvUnixEvent) == 16, "NvUnixEvent");
_Static_assert(sizeof(NV0005_ALLOC_PARAMETERS) == 24, "NV0005_ALLOC_PARAMETERS");
_Static_assert(sizeof(NV2080_CTRL_EVENT_SET_NOTIFICATION_PARAMS) == 20,
               "NV2080_CTRL_EVENT_SET_NOTIFICATION_PARAMS");

#define NV_IOWR(nr, size) _IOC(_IOC_READ | _IOC_WRITE, NV_IOCTL_MAGIC, (nr), (size))

static int g_failures;

static void ok(const char *what) { printf("  [ok]   %s\n", what); }

static void fail(const char *what, const char *detail) {
  printf("  [FAIL] %s: %s\n", what, detail);
  g_failures++;
}

static int nv_ioctl(int fd, unsigned nr, void *arg, unsigned size) {
  return ioctl(fd, NV_IOWR(nr, size), arg) < 0 ? errno : 0;
}

// An RM status, named for the handful a probe meets; the rest as a number.
static const char *rm_status(uint32_t s) {
  static char buf[32];
  switch (s) {
  case 0x00: return "NV_OK";
  case 0x1a: return "NV_ERR_INSUFFICIENT_RESOURCES";
  case 0x1b: return "NV_ERR_INSUFFICIENT_PERMISSIONS";
  case 0x1f: return "NV_ERR_INVALID_ARGUMENT";
  case 0x22: return "NV_ERR_INVALID_CLASS";
  case 0x26: return "NV_ERR_INVALID_DEVICE";
  case 0x28: return "NV_ERR_INVALID_EVENT";
  case 0x2c: return "NV_ERR_INVALID_OBJECT_HANDLE";
  case 0x40: return "NV_ERR_INVALID_STATE";
  case 0x56: return "NV_ERR_NOT_SUPPORTED";
  case 0x57: return "NV_ERR_OBJECT_NOT_FOUND";
  case 0x59: return "NV_ERR_OPERATING_SYSTEM";
  }
  snprintf(buf, sizeof buf, "RM status %#x", s);
  return buf;
}

// RM_ALLOC: 0 and the new handle in *out, or the failure printed.
static int rm_alloc(int ctl, uint32_t root, uint32_t parent, uint32_t cls, void *params,
                    uint32_t size, uint32_t *out, const char *what) {
  NVOS21_PARAMETERS p = {.hRoot = root, .hObjectParent = parent, .hClass = cls,
                         .pAllocParms = (uintptr_t)params, .paramsSize = size};
  char detail[96];
  int err = nv_ioctl(ctl, NV_ESC_RM_ALLOC, &p, sizeof p);
  if (err) {
    snprintf(detail, sizeof detail, "ioctl: %s", strerror(err));
    fail(what, detail);
    return -1;
  }
  if (p.status) {
    fail(what, rm_status(p.status));
    return -1;
  }
  *out = p.hObjectNew;
  return 0;
}

static int rm_control(int ctl, uint32_t client, uint32_t object, uint32_t cmd, void *params,
                      uint32_t size, const char *what) {
  NVOS54_PARAMETERS p = {.hClient = client, .hObject = object, .cmd = cmd,
                         .params = (uintptr_t)params, .paramsSize = size};
  char detail[96];
  int err = nv_ioctl(ctl, NV_ESC_RM_CONTROL, &p, sizeof p);
  if (err) {
    snprintf(detail, sizeof detail, "ioctl: %s", strerror(err));
    fail(what, detail);
    return -1;
  }
  if (p.status) {
    fail(what, rm_status(p.status));
    return -1;
  }
  return 0;
}

// RM_MAP_MEMORY of `memory`, recorded on `fd`, then the mmap of `fd` that
// takes it: the address, with the RM's address in *linear, or NULL with the
// failure printed (and the RM's mapping undone, if there was one).
static void *rm_map(int ctl, int fd, uint32_t client, uint32_t device, uint32_t memory,
                    uint64_t length, uint64_t *linear, const char *what) {
  nv_ioctl_nvos33_parameters_with_fd p = {
      .params = {.hClient = client, .hDevice = device, .hMemory = memory, .length = length},
      .fd = fd};
  char detail[96];
  int err = nv_ioctl(ctl, NV_ESC_RM_MAP_MEMORY, &p, sizeof p);
  if (err) {
    snprintf(detail, sizeof detail, "RM_MAP_MEMORY ioctl: %s", strerror(err));
    fail(what, detail);
    return NULL;
  }
  if (p.params.status) {
    snprintf(detail, sizeof detail, "RM_MAP_MEMORY: %s", rm_status(p.params.status));
    fail(what, detail);
    return NULL;
  }
  *linear = p.params.pLinearAddress;
  void *va = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (va == MAP_FAILED) {
    snprintf(detail, sizeof detail, "mmap: %s", strerror(errno));
    fail(what, detail);
    NVOS34_PARAMETERS u = {.hClient = client, .hDevice = device, .hMemory = memory,
                           .pLinearAddress = *linear};
    nv_ioctl(ctl, NV_ESC_RM_UNMAP_MEMORY, &u, sizeof u);
    return NULL;
  }
  return va;
}

static void rm_unmap(int ctl, uint32_t client, uint32_t device, uint32_t memory, uint64_t linear,
                     const char *what) {
  NVOS34_PARAMETERS u = {.hClient = client, .hDevice = device, .hMemory = memory,
                         .pLinearAddress = linear};
  char detail[96];
  int err = nv_ioctl(ctl, NV_ESC_RM_UNMAP_MEMORY, &u, sizeof u);
  if (err) {
    snprintf(detail, sizeof detail, "RM_UNMAP_MEMORY ioctl: %s", strerror(err));
    fail(what, detail);
  } else if (u.status) {
    snprintf(detail, sizeof detail, "RM_UNMAP_MEMORY: %s", rm_status(u.status));
    fail(what, detail);
  }
}

static void rm_free(int ctl, uint32_t client, uint32_t parent, uint32_t object) {
  NVOS00_PARAMETERS f = {.hRoot = client, .hObjectParent = parent, .hObjectOld = object};
  nv_ioctl(ctl, NV_ESC_RM_FREE, &f, sizeof f);
}

// A pattern through a mapping of 64 KiB of the client's own system memory.
static void probe_sysmem(int ctl, uint32_t client, uint32_t device) {
  const uint64_t size = 0x10000;
  char detail[160];
  NV_MEMORY_ALLOCATION_PARAMS mp = {.owner = client, .type = NVOS32_TYPE_IMAGE,
                                    .attr = NVOS32_ATTR_SYSMEM_4K_CACHED, .size = size};
  uint32_t memory = 0;
  if (rm_alloc(ctl, client, device, NV01_MEMORY_SYSTEM, &mp, sizeof mp, &memory,
               "NV01_MEMORY_SYSTEM"))
    return;
  ok("NV01_MEMORY_SYSTEM, 64 KiB");
  int fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    snprintf(detail, sizeof detail, "open: %s", strerror(errno));
    fail("sysmem /dev/nvidiactl", detail);
    rm_free(ctl, client, device, memory);
    return;
  }
  uint64_t linear = 0;
  volatile uint32_t *va = rm_map(ctl, fd, client, device, memory, size, &linear, "sysmem");
  if (va) {
    size_t n = size / sizeof *va, bad = 0;
    for (size_t i = 0; i < n; i++)
      va[i] = 0x5eed0000u ^ (uint32_t)i;
    for (size_t i = 0; i < n; i++)
      bad += va[i] != (0x5eed0000u ^ (uint32_t)i);
    if (bad) {
      snprintf(detail, sizeof detail, "%zu of %zu words read back wrong", bad, n);
      fail("sysmem pattern", detail);
    } else {
      snprintf(detail, sizeof detail, "sysmem at %#llx: mapped, written and read back",
               (unsigned long long)linear);
      ok(detail);
    }
    munmap((void *)va, size);
    rm_unmap(ctl, client, device, memory, linear, "sysmem");
  }
  close(fd);
  rm_free(ctl, client, device, memory);
}

static uint64_t gpu_time(volatile uint32_t *regs) {
  uint32_t hi, lo;
  do {
    hi = regs[NVC361_TIME_1 / 4];
    lo = regs[NVC361_TIME_0 / 4];
  } while (hi != regs[NVC361_TIME_1 / 4]);
  return (uint64_t)hi << 32 | lo;
}

// The GPU timer, through a mapping of the subdevice's usermode page in BAR0.
static void probe_usermode(int ctl, uint32_t client, uint32_t subdevice, const char *path) {
  char detail[160];
  uint32_t usermode = 0;
  if (rm_alloc(ctl, client, subdevice, TURING_USERMODE_A, NULL, 0, &usermode,
               "TURING_USERMODE_A"))
    return;
  ok("TURING_USERMODE_A");
  int fd = open(path, O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    snprintf(detail, sizeof detail, "open: %s", strerror(errno));
    fail("usermode node", detail);
    rm_free(ctl, client, subdevice, usermode);
    return;
  }
  uint64_t linear = 0;
  volatile uint32_t *regs =
      rm_map(ctl, fd, client, subdevice, usermode, NVC361_NV_USERMODE__SIZE, &linear, "usermode");
  if (regs) {
    uint64_t t0 = gpu_time(regs);
    struct timespec ms = {0, 1000000};
    nanosleep(&ms, NULL);
    uint64_t t1 = gpu_time(regs);
    if (t1 > t0) {
      snprintf(detail, sizeof detail, "usermode at %#llx: the GPU timer moved %llu ns in 1 ms",
               (unsigned long long)linear, (unsigned long long)(t1 - t0));
      ok(detail);
    } else {
      snprintf(detail, sizeof detail, "the GPU timer read %#llx then %#llx",
               (unsigned long long)t0, (unsigned long long)t1);
      fail("usermode timer", detail);
    }
    munmap((void *)regs, NVC361_NV_USERMODE__SIZE);
    rm_unmap(ctl, client, subdevice, usermode, linear, "usermode");
  }
  close(fd);
  rm_free(ctl, client, subdevice, usermode);
}

// ALLOC_OS_EVENT or FREE_OS_EVENT of `efd` for the client: 0, or the
// failure printed.
static int os_event(int efd, unsigned nr, uint32_t client, uint32_t device, const char *what) {
  nv_ioctl_alloc_os_event_t p = {.hClient = client, .hDevice = device, .fd = (uint32_t)efd};
  char detail[96];
  int err = nv_ioctl(efd, nr, &p, sizeof p);
  if (err) {
    snprintf(detail, sizeof detail, "ioctl: %s", strerror(err));
    fail(what, detail);
    return -1;
  }
  if (p.Status) {
    fail(what, rm_status(p.Status));
    return -1;
  }
  ok(what);
  return 0;
}

// The event on `efd`: it must poll readable, and RM_GET_EVENT_DATA must hand
// back the software notifier of event object `event`.
static void read_event(int efd, uint32_t event) {
  char detail[160];
  struct pollfd pfd = {.fd = efd, .events = POLLIN | POLLPRI};
  int n = poll(&pfd, 1, 1000);
  if (n < 0) {
    snprintf(detail, sizeof detail, "poll: %s", strerror(errno));
    fail("poll", detail);
  } else if (n == 0 || !(pfd.revents & POLLIN)) {
    snprintf(detail, sizeof detail, "not readable within 1 s (revents %#x)", pfd.revents);
    fail("poll", detail);
  } else {
    ok("poll: readable");
  }
  NvUnixEvent ue;
  memset(&ue, 0xee, sizeof ue);
  NVOS41_PARAMETERS g = {.pEvent = (uintptr_t)&ue};
  int err = nv_ioctl(efd, NV_ESC_RM_GET_EVENT_DATA, &g, sizeof g);
  if (err) {
    snprintf(detail, sizeof detail, "ioctl: %s", strerror(err));
    fail("RM_GET_EVENT_DATA", detail);
  } else if (g.status) {
    fail("RM_GET_EVENT_DATA", rm_status(g.status));
  } else if (ue.hObject != event || ue.NotifyIndex != NV2080_NOTIFIERS_SW) {
    snprintf(detail, sizeof detail, "hObject %#x NotifyIndex %u, not %#x %u", ue.hObject,
             ue.NotifyIndex, event, NV2080_NOTIFIERS_SW);
    fail("RM_GET_EVENT_DATA", detail);
  } else {
    snprintf(detail, sizeof detail, "RM_GET_EVENT_DATA: hObject %#x NotifyIndex %u MoreEvents %u",
             ue.hObject, ue.NotifyIndex, g.MoreEvents);
    ok(detail);
  }
}

// An OS event on the subdevice's software notifier, fired by the probe
// itself (see the top of the file).
static void probe_event(int ctl, uint32_t client, uint32_t device, uint32_t subdevice) {
  char detail[160];
  int efd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
  if (efd < 0) {
    snprintf(detail, sizeof detail, "open: %s", strerror(errno));
    fail("event fd", detail);
    return;
  }
  if (os_event(efd, NV_ESC_ALLOC_OS_EVENT, client, device, "ALLOC_OS_EVENT")) {
    close(efd);
    return;
  }
  uint32_t event = 0;
  NV0005_ALLOC_PARAMETERS ep = {.hParentClient = client, .hSrcResource = subdevice,
                                .hClass = NV01_EVENT_OS_EVENT,
                                .notifyIndex = NV2080_NOTIFIERS_SW, .data = (uint64_t)efd};
  if (rm_alloc(ctl, client, subdevice, NV01_EVENT_OS_EVENT, &ep, sizeof ep, &event,
               "NV01_EVENT_OS_EVENT") == 0) {
    ok("NV01_EVENT_OS_EVENT");
    NV2080_CTRL_EVENT_SET_NOTIFICATION_PARAMS sn = {
        .event = NV2080_NOTIFIERS_SW, .action = NV2080_CTRL_EVENT_SET_NOTIFICATION_ACTION_SINGLE};
    if (rm_control(ctl, client, subdevice, NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION, &sn,
                   sizeof sn, "EVENT_SET_NOTIFICATION") == 0 &&
        rm_control(ctl, client, subdevice, NV2080_CTRL_CMD_EVENT_SET_TRIGGER, NULL, 0,
                   "EVENT_SET_TRIGGER") == 0) {
      ok("EVENT_SET_TRIGGER");
      read_event(efd, event);
    }
    rm_free(ctl, client, subdevice, event);
  }
  os_event(efd, NV_ESC_FREE_OS_EVENT, client, device, "FREE_OS_EVENT");
  close(efd);
}

static void probe_gpu(int ctl, uint32_t client, const nv_ioctl_card_info_t *card, int index) {
  char path[32], detail[160];
  snprintf(path, sizeof path, "/dev/nvidia%u", card->minor_number);
  printf("\n[gpu %d: %s, %04x:%02x:%02x.0]\n", index, path, card->pci_info.domain,
         card->pci_info.bus, card->pci_info.slot);

  int fd = open(path, O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    snprintf(detail, sizeof detail, "open: %s", strerror(errno));
    fail(path, detail);
    return;
  }
  ok("open");

  nv_ioctl_register_fd_t reg = {.ctl_fd = ctl};
  int err = nv_ioctl(fd, NV_ESC_REGISTER_FD, &reg, sizeof reg);
  if (err)
    fail("REGISTER_FD", strerror(err));
  else
    ok("REGISTER_FD");

  NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS id = {.gpuId = card->gpu_id};
  if (rm_control(ctl, client, client, NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2, &id, sizeof id,
                 "GET_ID_INFO_V2") == 0) {
    snprintf(detail, sizeof detail, "GET_ID_INFO_V2: deviceInstance %u subDeviceInstance %u",
             id.deviceInstance, id.subDeviceInstance);
    ok(detail);
  } else {
    close(fd);
    return;
  }

  uint32_t device = 0, subdevice = 0;
  NV0080_ALLOC_PARAMETERS dp = {.deviceId = id.deviceInstance, .hClientShare = client,
                                .vaMode = NV_DEVICE_ALLOCATION_VAMODE_MULTIPLE_VASPACES};
  if (rm_alloc(ctl, client, client, NV01_DEVICE_0, &dp, sizeof dp, &device, "NV01_DEVICE_0")) {
    close(fd);
    return;
  }
  ok("NV01_DEVICE_0");
  NV2080_ALLOC_PARAMETERS sp = {.subDeviceId = 0};
  if (rm_alloc(ctl, client, device, NV20_SUBDEVICE_0, &sp, sizeof sp, &subdevice,
               "NV20_SUBDEVICE_0")) {
    close(fd);
    return;
  }
  ok("NV20_SUBDEVICE_0");

  NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS name = {0};
  NV2080_CTRL_GPU_GET_GID_INFO_PARAMS gid = {0};
  NV2080_CTRL_FB_GET_INFO_V2_PARAMS fb = {.fbInfoListSize = 1};
  fb.fbInfoList[0].index = NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE;
  int have_name = rm_control(ctl, client, subdevice, NV2080_CTRL_CMD_GPU_GET_NAME_STRING, &name,
                             sizeof name, "GET_NAME_STRING") == 0;
  int have_gid = rm_control(ctl, client, subdevice, NV2080_CTRL_CMD_GPU_GET_GID_INFO, &gid,
                            sizeof gid, "GET_GID_INFO") == 0;
  int have_fb = rm_control(ctl, client, subdevice, NV2080_CTRL_CMD_FB_GET_INFO_V2, &fb,
                           sizeof fb, "FB_GET_INFO_V2") == 0;
  name.gpuNameString.ascii[63] = 0;
  gid.data[sizeof gid.data - 1] = 0;
  printf("\n  GPU %d: %s (UUID: %s)", index, have_name ? (char *)name.gpuNameString.ascii : "?",
         have_gid ? (char *)gid.data : "?");
  if (have_fb)
    printf(", %u MiB", fb.fbInfoList[0].data / 1024);
  printf("\n\n  [map]\n");
  probe_sysmem(ctl, client, device);
  probe_usermode(ctl, client, subdevice, path);
  printf("\n  [event]\n");
  probe_event(ctl, client, device, subdevice);
  close(fd);
}

int main(void) {
  printf("[version]\n");
  int ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
  if (ctl < 0) {
    printf("  [FAIL] /dev/nvidiactl: %s (is the kernel booted with nvidia.nvidiactl?)\n",
           strerror(errno));
    return 1;
  }
  nv_ioctl_rm_api_version_t v = {.cmd = '2'};
  int err = nv_ioctl(ctl, NV_ESC_CHECK_VERSION_STR, &v, sizeof v);
  if (err) {
    fail("CHECK_VERSION_STR query", strerror(err));
    return 1;
  }
  v.versionString[63] = 0;
  printf("  [ok]   the RM is %s\n", v.versionString);
  nv_ioctl_rm_api_version_t strict = {.cmd = 0};
  memcpy(strict.versionString, v.versionString, sizeof strict.versionString);
  err = nv_ioctl(ctl, NV_ESC_CHECK_VERSION_STR, &strict, sizeof strict);
  if (err)
    fail("CHECK_VERSION_STR strict", strerror(err));
  else
    ok("CHECK_VERSION_STR strict: user-mode components of this version will be accepted");

  printf("\n[cards]\n");
  nv_ioctl_card_info_t cards[NV_MAX_DEVICES];
  memset(cards, 0, sizeof cards);
  err = nv_ioctl(ctl, NV_ESC_CARD_INFO, cards, sizeof cards);
  if (err) {
    fail("CARD_INFO", strerror(err));
    return 1;
  }
  int listed = 0;
  for (int i = 0; i < NV_MAX_DEVICES; i++) {
    if (!cards[i].valid)
      continue;
    listed++;
    printf("  [ok]   minor %u: %04x:%02x:%02x.0 %04x:%04x gpu_id %#x BAR0 %#llx+%#llx "
           "BAR1 %#llx+%#llx\n",
           cards[i].minor_number, cards[i].pci_info.domain, cards[i].pci_info.bus,
           cards[i].pci_info.slot, cards[i].pci_info.vendor_id, cards[i].pci_info.device_id,
           cards[i].gpu_id, (unsigned long long)cards[i].reg_address,
           (unsigned long long)cards[i].reg_size, (unsigned long long)cards[i].fb_address,
           (unsigned long long)cards[i].fb_size);
  }
  if (!listed)
    fail("CARD_INFO", "no GPU listed: the RM has none up");

  printf("\n[client]\n");
  uint32_t client = 0;
  if (rm_alloc(ctl, 0, 0, NV01_ROOT_CLIENT, NULL, 0, &client, "NV01_ROOT_CLIENT"))
    return 1;
  printf("  [ok]   NV01_ROOT_CLIENT %#x\n", client);

  printf("\n[attached]\n");
  uint32_t ids[32];
  memset(ids, 0xff, sizeof ids);
  if (rm_control(ctl, client, client, NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS, ids, sizeof ids,
                 "GET_ATTACHED_IDS") == 0) {
    for (int i = 0; i < 32 && ids[i] != NV0000_CTRL_GPU_INVALID_ID; i++)
      printf("  [ok]   gpu id %#x\n", ids[i]);
  }

  for (int i = 0, n = 0; i < NV_MAX_DEVICES; i++)
    if (cards[i].valid)
      probe_gpu(ctl, client, &cards[i], n++);

  printf("\n[free]\n");
  NVOS00_PARAMETERS f = {.hRoot = client, .hObjectParent = client, .hObjectOld = client};
  err = nv_ioctl(ctl, NV_ESC_RM_FREE, &f, sizeof f);
  if (err)
    fail("RM_FREE", strerror(err));
  else if (f.status)
    fail("RM_FREE", rm_status(f.status));
  else
    ok("RM_FREE of the client");
  close(ctl);

  printf("\n%s: %d failure(s)\n", g_failures ? "FAIL" : "PASS", g_failures);
  return g_failures ? 1 : 0;
}
