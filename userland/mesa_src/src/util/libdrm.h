/*
 * Copyright © 2023 Google, Inc.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice (including the next
 * paragraph) shall be included in all copies or substantial portions of the
 * Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

/*
 * A simple header that either gives you the real libdrm or a no-op shim,
 * depending on whether HAVE_LIBDRM is defined.  This is intended to avoid
 * the proliferation of #ifdef'ery to support environments without libdrm.
 */

#ifdef HAVE_LIBDRM
#include <xf86drm.h>
#else

#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>

#define DRM_NODE_PRIMARY 0
#define DRM_NODE_CONTROL 1
#define DRM_NODE_RENDER  2
#define DRM_NODE_MAX     3

#define DRM_BUS_PCI       0
#define DRM_BUS_USB       1
#define DRM_BUS_PLATFORM  2
#define DRM_BUS_HOST1X    3

typedef unsigned int drm_magic_t;

static int
drmGetMagic(int fd, drm_magic_t * magic)
{
  return -EINVAL;
}

typedef struct _drmPciDeviceInfo {
    uint16_t vendor_id;
    uint16_t device_id;
    uint16_t subvendor_id;
    uint16_t subdevice_id;
    uint8_t revision_id;
} drmPciDeviceInfo, *drmPciDeviceInfoPtr;

#define DRM_PLATFORM_DEVICE_NAME_LEN 512

typedef struct _drmPlatformBusInfo {
    char fullname[DRM_PLATFORM_DEVICE_NAME_LEN];
} drmPlatformBusInfo, *drmPlatformBusInfoPtr;

typedef struct _drmPlatformDeviceInfo {
    char **compatible; /* NULL terminated list of compatible strings */
} drmPlatformDeviceInfo, *drmPlatformDeviceInfoPtr;

#define DRM_HOST1X_DEVICE_NAME_LEN 512

typedef struct _drmHost1xBusInfo {
    char fullname[DRM_HOST1X_DEVICE_NAME_LEN];
} drmHost1xBusInfo, *drmHost1xBusInfoPtr;

typedef struct _drmPciBusInfo {
   uint16_t domain;
   uint8_t bus;
   uint8_t dev;
   uint8_t func;
} drmPciBusInfo, *drmPciBusInfoPtr;

typedef struct _drmDevice {
    char **nodes; /* DRM_NODE_MAX sized array */
    int available_nodes; /* DRM_NODE_* bitmask */
    int bustype;
    union {
       drmPciBusInfoPtr pci;
       drmPlatformBusInfoPtr platform;
       drmHost1xBusInfoPtr host1x;
    } businfo;
    union {
        drmPciDeviceInfoPtr pci;
    } deviceinfo;
    /* ... */
} drmDevice, *drmDevicePtr;

#define DRM_DEVICE_GET_PCI_REVISION (1 << 0)
static inline int
drmGetDevice2(int fd, uint32_t flags, drmDevicePtr *device)
{
   return -ENOENT;
}

static inline int
drmGetDevices2(uint32_t flags, drmDevicePtr devices[], int max_devices)
{
   return -ENOENT;
}

static inline int
drmGetDeviceFromDevId(dev_t dev_id, uint32_t flags, drmDevicePtr *device)
{
   return -ENOENT;
}

static inline void
drmFreeDevice(drmDevicePtr *device) {}

static inline void
drmFreeDevices(drmDevicePtr devices[], int count) {}

static inline char*
drmGetDeviceNameFromFd2(int fd)
{
   /*
    * Minimal shim for environments built without libdrm.
    *
    * Mesa and wlroots mainly need a stable /dev node path. On Linux this can be
    * derived from the device major/minor. We implement the common DRM major
    * 226 mapping:
    *   - primary:  /dev/dri/cardN      (minor N)
    *   - control:  /dev/dri/controlD*  (minor 64+N)
    *   - render:   /dev/dri/renderD*   (minor 128+N)
    *
    * If the fd isn't a DRM char device, return NULL like the real libdrm does.
    */
   struct stat st;
   if (fstat(fd, &st) != 0)
      return NULL;
   if (!S_ISCHR(st.st_mode))
      return NULL;

   unsigned int maj = major(st.st_rdev);
   unsigned int min = minor(st.st_rdev);

   if (maj != 226)
      return NULL;

   char buf[64];
   if (min < 64) {
      snprintf(buf, sizeof(buf), "/dev/dri/card%u", min);
   } else if (min < 128) {
      snprintf(buf, sizeof(buf), "/dev/dri/controlD%u", min);
   } else {
      snprintf(buf, sizeof(buf), "/dev/dri/renderD%u", min);
   }

   return strdup(buf);
}

static inline char *
drmGetPrimaryDeviceNameFromFd(int fd)
{
   struct stat st;
   if (fstat(fd, &st) != 0)
      return NULL;
   if (!S_ISCHR(st.st_mode))
      return NULL;
   unsigned int maj = major(st.st_rdev);
   unsigned int min = minor(st.st_rdev);
   if (maj != 226 || min >= 64)
      return NULL;
   char buf[64];
   snprintf(buf, sizeof(buf), "/dev/dri/card%u", min);
   return strdup(buf);
}

static inline char *
drmGetRenderDeviceNameFromFd(int fd)
{
   struct stat st;
   if (fstat(fd, &st) != 0)
      return NULL;
   if (!S_ISCHR(st.st_mode))
      return NULL;
   unsigned int maj = major(st.st_rdev);
   unsigned int min = minor(st.st_rdev);
   if (maj != 226 || min < 128)
      return NULL;
   char buf[64];
   snprintf(buf, sizeof(buf), "/dev/dri/renderD%u", min);
   return strdup(buf);
}

typedef struct _drmVersion {
    int     version_major;        /**< Major version */
    int     version_minor;        /**< Minor version */
    int     version_patchlevel;   /**< Patch level */
    int     name_len;             /**< Length of name buffer */
    char    *name;                /**< Name of driver */
    int     date_len;             /**< Length of date buffer */
    char    *date;                /**< User-space buffer to hold date */
    int     desc_len;             /**< Length of desc buffer */
    char    *desc;                /**< User-space buffer to hold desc */
} drmVersion, *drmVersionPtr;

static inline struct _drmVersion *
drmGetVersion(int fd)
{
   /*
    * Minimal shim for environments built without libdrm.
    *
    * Callers typically just log/branch on version->name and expect the returned
    * pointer to remain valid until drmFreeVersion().
    *
    * Provide a stable synthetic "eclipse" driver version.
    */
   (void)fd;

   drmVersionPtr v = (drmVersionPtr)calloc(1, sizeof(*v));
   if (!v)
      return NULL;

   v->version_major = 1;
   v->version_minor = 0;
   v->version_patchlevel = 0;

   v->name = strdup("eclipse");
   v->date = strdup("20260420");
   v->desc = strdup("Eclipse DRM shim (no-libdrm)");

   if (!v->name || !v->date || !v->desc) {
      free(v->name);
      free(v->date);
      free(v->desc);
      free(v);
      return NULL;
   }

   v->name_len = (int)strlen(v->name) + 1;
   v->date_len = (int)strlen(v->date) + 1;
   v->desc_len = (int)strlen(v->desc) + 1;

   return v;
}

static inline void
drmFreeVersion(struct _drmVersion *v)
{
   if (!v)
      return;
   free(v->name);
   free(v->date);
   free(v->desc);
   free(v);
}

#endif
