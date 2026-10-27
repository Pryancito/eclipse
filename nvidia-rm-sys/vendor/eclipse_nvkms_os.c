/*
 * Eclipse's side of nvidia-modeset's OS interface
 * (src/nvidia-modeset/os-interface/include/nvidia-modeset-os-interface.h),
 * which Linux implements in kernel-open/nvidia-modeset/nvidia-modeset-linux.c.
 *
 * Split in two. The hooks that need the kernel -- memory, time, semaphores,
 * refcounted pointers, timers, nvkms_lock, the RM -- are Rust, in
 * src/nvkms_os.rs. What is here needs nothing from it: module parameters,
 * string and printf helpers, and the kapi open/ioctl/close glue with its
 * event work. Hooks for things Eclipse does not have (user-space clients,
 * syncpoints, backlight devices) answer that they are not there.
 */

#include "nvidia-modeset-os-interface.h"

/* The version banner nvkms prints when it loads ("Loading %s"). In Linux it
 * is generated into g_nvid_string.c by the Makefile. */
const char *const pNV_KMS_ID = "NVIDIA nvidia-modeset (Eclipse port)";

/* Module parameters: every one at its Linux default. */
NvBool nvkms_test_fail_alloc_core_channel(enum FailAllocCoreChannelMethod method)
{
    (void)method;
    return NV_FALSE;
}
NvBool nvkms_conceal_vrr_caps(void) { return NV_FALSE; }
NvBool nvkms_output_rounding_fix(void) { return NV_TRUE; }
NvBool nvkms_disable_hdmi_frl(void) { return NV_FALSE; }
NvBool nvkms_disable_vrr_memclk_switch(void) { return NV_FALSE; }
NvBool nvkms_hdmi_deepcolor(void) { return NV_TRUE; }
NvBool nvkms_vblank_sem_control(void) { return NV_TRUE; }
NvBool nvkms_opportunistic_display_sync(void) { return NV_TRUE; }
enum NvKmsDebugForceColorSpace nvkms_debug_force_color_space(void)
{
    return NVKMS_DEBUG_FORCE_COLOR_SPACE_NONE;
}
NvBool nvkms_enable_overlay_layers(void) { return NV_TRUE; }
NvBool nvkms_debug_logging(void) { return NV_FALSE; }

void *nvkms_memset(void *ptr, NvU8 c, size_t size)
{
    NvU8 *p = ptr;
    while (size--)
        *p++ = c;
    return ptr;
}
void *nvkms_memcpy(void *dest, const void *src, size_t n)
{
    NvU8 *d = dest;
    const NvU8 *s = src;
    while (n--)
        *d++ = *s++;
    return dest;
}
void *nvkms_memmove(void *dest, const void *src, size_t n)
{
    NvU8 *d = dest;
    const NvU8 *s = src;
    if (d < s) {
        while (n--)
            *d++ = *s++;
    } else {
        while (n--)
            d[n] = s[n];
    }
    return dest;
}
int nvkms_memcmp(const void *s1, const void *s2, size_t n)
{
    const NvU8 *a = s1, *b = s2;
    for (; n; n--, a++, b++)
        if (*a != *b)
            return *a < *b ? -1 : 1;
    return 0;
}
size_t nvkms_strlen(const char *s)
{
    size_t n = 0;
    while (s[n])
        n++;
    return n;
}
int nvkms_strcmp(const char *s1, const char *s2)
{
    while (*s1 && *s1 == *s2) {
        s1++;
        s2++;
    }
    return (int)(unsigned char)*s1 - (int)(unsigned char)*s2;
}

/* No user pointers reach nvkms in Eclipse: kapi hands it kernel memory. */
int nvkms_copyin(void *kptr, NvU64 uaddr, size_t n)
{
    (void)kptr;
    (void)uaddr;
    (void)n;
    return -1;
}
int nvkms_copyout(NvU64 uaddr, const void *kptr, size_t n)
{
    (void)uaddr;
    (void)kptr;
    (void)n;
    return -1;
}

void nvkms_dump_stack(void) {}

NvBool nvkms_syncpt_op(enum NvKmsSyncPtOp op, NvKmsSyncPtOpParams *params)
{
    (void)op;
    (void)params;
    return NV_FALSE;
}

/* The RM's own printf (src/nvidia/src/kernel/core/printf.c), which handles
 * every conversion nvkms formats with. */
int nvDbgVsnprintf(char *dest, NvU32 destSize, const char *fmt, va_list args);

int nvkms_vsnprintf(char *str, size_t size, const char *format, va_list ap)
{
    return nvDbgVsnprintf(str, (NvU32)size, format, ap);
}
int nvkms_snprintf(char *str, size_t size, const char *format, ...)
{
    va_list ap;
    int ret;
    va_start(ap, format);
    ret = nvkms_vsnprintf(str, size, format, ap);
    va_end(ap);
    return ret;
}

void *nvkms_get_per_open_data(int fd)
{
    (void)fd;
    return NULL;
}

/* The boot framebuffer and BAR1 are mapped write-combined on this kernel
 * (kernel_hal ensure_framebuffer_wc), which is what Linux reports on x86. */
NvBool nvkms_allow_write_combining(void) { return NV_TRUE; }
NvBool nvkms_kernel_supports_syncpts(void) { return NV_FALSE; }
NvBool nvkms_fd_is_nvidia_chardev(int fd)
{
    (void)fd;
    return NV_FALSE;
}

/*
 * A kapi client's open of nvkms: nvkms_open_common, nvkms_ioctl_common and
 * nvkms_close_pm_locked of nvidia-modeset-linux.c. Every entry takes
 * nvkms_lock (src/nvkms_os.rs). There is no power management here, so the
 * PM lock Linux also takes is always free.
 */
void eclipse_nvkms_lock(void);
void eclipse_nvkms_unlock(void);
void eclipse_nvkms_schedule_work(void (*proc)(void *, NvU32), void *data);
void eclipse_nvkms_cancel_work(void *data);

struct nvkms_per_open {
    void *data;
    struct NvKmsKapiDevice *device;
};

/* nvkms_kapi_event_kthread_q_callback: runs without nvkms_lock, because
 * handling the events calls back into nvkms through ioctls. */
static void eclipse_nvkms_kapi_event_work(void *arg, NvU32 unused)
{
    struct nvkms_per_open *popen = arg;

    (void)unused;
    nvKmsKapiHandleEventQueueChange(popen->device);
}

void nvkms_event_queue_changed(nvkms_per_open_handle_t *pOpenKernel,
                               NvBool eventsAvailable)
{
    /* Only kernel (kapi) clients exist here. */
    if (eventsAvailable)
        eclipse_nvkms_schedule_work(eclipse_nvkms_kapi_event_work, pOpenKernel);
}

struct nvkms_per_open *nvkms_open_from_kapi(struct NvKmsKapiDevice *device)
{
    struct nvkms_per_open *popen = nvkms_alloc(sizeof(*popen), NV_TRUE);

    if (popen == NULL)
        return NULL;
    popen->device = device;
    eclipse_nvkms_lock();
    popen->data = nvKmsOpen(0 /* pid: the kernel */, NVKMS_CLIENT_KERNEL_SPACE, popen);
    eclipse_nvkms_unlock();
    if (popen->data == NULL) {
        nvkms_free(popen, sizeof(*popen));
        return NULL;
    }
    return popen;
}

void nvkms_close_from_kapi(struct nvkms_per_open *popen)
{
    eclipse_nvkms_lock();
    nvKmsClose(popen->data);
    popen->data = NULL;
    eclipse_nvkms_unlock();
    /* After nvKmsClose no more event work can be queued for this open;
     * drop what is queued, then free it (Linux flushes its kthread queue). */
    eclipse_nvkms_cancel_work(popen);
    nvkms_free(popen, sizeof(*popen));
}

NvBool nvkms_ioctl_from_kapi(struct nvkms_per_open *popen, NvU32 cmd,
                             void *params_address, const size_t params_size)
{
    NvBool ret = NV_FALSE;

    eclipse_nvkms_lock();
    if (popen->data != NULL)
        ret = nvKmsIoctl(popen->data, cmd, (NvU64)(NvUPtr)params_address, params_size);
    eclipse_nvkms_unlock();
    return ret;
}

NvBool nvkms_ioctl_from_kapi_try_pmlock(struct nvkms_per_open *popen, NvU32 cmd,
                                        void *params_address,
                                        const size_t params_size)
{
    return nvkms_ioctl_from_kapi(popen, cmd, params_address, params_size);
}

struct nvkms_backlight_device *nvkms_register_backlight(NvU32 gpu_id,
                                                        NvU32 display_id,
                                                        void *drv_priv,
                                                        NvU32 current_brightness)
{
    (void)gpu_id;
    (void)display_id;
    (void)drv_priv;
    (void)current_brightness;
    return NULL;
}
void nvkms_unregister_backlight(struct nvkms_backlight_device *nvkms_bd)
{
    (void)nvkms_bd;
}

/*
 * The kapi entry points nvidia-drm links against, which Linux exports from
 * nvidia-modeset-linux.c as thin wrappers over the *Internal versions in
 * nvkms. Eclipse's driver will call them the same way.
 */
NvBool nvKmsKapiGetFunctionsTable(struct NvKmsKapiFunctionsTable *funcsTable)
{
    return nvKmsKapiGetFunctionsTableInternal(funcsTable);
}
NvU32 nvKmsKapiF16ToF32(NvU16 a) { return nvKmsKapiF16ToF32Internal(a); }
NvU16 nvKmsKapiF32ToF16(NvU32 a) { return nvKmsKapiF32ToF16Internal(a); }
NvU32 nvKmsKapiF32Mul(NvU32 a, NvU32 b) { return nvKmsKapiF32MulInternal(a, b); }
NvU32 nvKmsKapiF32Div(NvU32 a, NvU32 b) { return nvKmsKapiF32DivInternal(a, b); }
NvU32 nvKmsKapiF32Add(NvU32 a, NvU32 b) { return nvKmsKapiF32AddInternal(a, b); }
NvU32 nvKmsKapiF32ToUI32RMinMag(NvU32 a, NvBool exact)
{
    return nvKmsKapiF32ToUI32RMinMagInternal(a, exact);
}
NvU32 nvKmsKapiUI32ToF32(NvU32 a) { return nvKmsKapiUI32ToF32Internal(a); }
