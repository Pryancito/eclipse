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
//   free      RM_FREE of the client: everything under it goes.
//
// Needs the kernel booted with `nvidia.nvidiactl`. Read-only: it allocates RM
// objects and frees them, and touches no engine.
//
// Build: static musl, like the other probes (xtask does it; by hand:
//   x86_64-linux-musl-gcc -static -O2 -o nvctl-probe nvctl-probe.c).

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

// nv-ioctl-numbers.h / nv_escape.h
#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_CARD_INFO 200
#define NV_ESC_REGISTER_FD 201
#define NV_ESC_CHECK_VERSION_STR 210
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_CONTROL 0x2A
#define NV_ESC_RM_ALLOC 0x2B

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

_Static_assert(sizeof(nv_ioctl_card_info_t) == 72, "nv_ioctl_card_info_t");
_Static_assert(sizeof(nv_ioctl_rm_api_version_t) == 72, "nv_ioctl_rm_api_version_t");
_Static_assert(sizeof(NVOS21_PARAMETERS) == 32, "NVOS21_PARAMETERS");
_Static_assert(sizeof(NVOS00_PARAMETERS) == 16, "NVOS00_PARAMETERS");
_Static_assert(sizeof(NVOS54_PARAMETERS) == 32, "NVOS54_PARAMETERS");
_Static_assert(sizeof(NV0080_ALLOC_PARAMETERS) == 56, "NV0080_ALLOC_PARAMETERS");

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
  case 0x2c: return "NV_ERR_INVALID_OBJECT_HANDLE";
  case 0x40: return "NV_ERR_INVALID_STATE";
  case 0x56: return "NV_ERR_NOT_SUPPORTED";
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
  printf("\n");
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
