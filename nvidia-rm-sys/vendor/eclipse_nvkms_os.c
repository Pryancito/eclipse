/*
 * Eclipse's side of nvidia-modeset's OS interface
 * (src/nvidia-modeset/os-interface/include/nvidia-modeset-os-interface.h).
 *
 * In Linux these are kernel-open/nvidia-modeset/nvidia-modeset-linux.c.
 * Here, for now, they are what it takes for nvkms to LINK and nothing more:
 * nothing in Eclipse calls into nvkms yet (docs/README-nvkms-port.md, PR 1).
 * Every hook that would need the kernel -- memory, timers, semaphores, the
 * RM -- fails, so an accidental call comes back as an allocation failure
 * nvkms already knows how to unwind, never as a half-working device. The
 * string and memory helpers are real because they need nothing.
 *
 * PR 2 of the port replaces the failing ones with the real kernel services.
 */

#include "nvidia-modeset-os-interface.h"

/* The version banner nvkms prints when it loads ("Loading %s"). In Linux it
 * is generated into g_nvid_string.c by the Makefile. */
const char *const pNV_KMS_ID = "NVIDIA nvidia-modeset (Eclipse port, not active)";

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

/* Not wired to the RM yet, and unreachable: nvKmsModuleLoad never runs, so no
 * device is allocated and nothing builds an RM call. PR 2 puts the real
 * rm_kernel_rmapi_op behind this. */
void nvkms_call_rm(void *ops) { (void)ops; }

void *nvkms_alloc(size_t size, NvBool zero)
{
    (void)size;
    (void)zero;
    return NULL;
}
void nvkms_free(void *ptr, size_t size)
{
    (void)ptr;
    (void)size;
}

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

void nvkms_usleep(NvU64 usec) { (void)usec; }
NvU64 nvkms_get_usec(void) { return 0; }

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

void nvkms_yield(void) {}
void nvkms_dump_stack(void) {}

NvBool nvkms_syncpt_op(enum NvKmsSyncPtOp op, NvKmsSyncPtOpParams *params)
{
    (void)op;
    (void)params;
    return NV_FALSE;
}

int nvkms_vsnprintf(char *str, size_t size, const char *format, va_list ap)
{
    (void)format;
    (void)ap;
    if (size)
        str[0] = '\0';
    return 0;
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

void nvkms_log(const int level, const char *gpuPrefix, const char *msg)
{
    (void)level;
    (void)gpuPrefix;
    (void)msg;
}

struct nvkms_ref_ptr *nvkms_alloc_ref_ptr(void *ptr)
{
    (void)ptr;
    return NULL;
}
void nvkms_free_ref_ptr(struct nvkms_ref_ptr *ref_ptr) { (void)ref_ptr; }
void nvkms_inc_ref(struct nvkms_ref_ptr *ref_ptr) { (void)ref_ptr; }
void *nvkms_dec_ref(struct nvkms_ref_ptr *ref_ptr)
{
    (void)ref_ptr;
    return NULL;
}

nvkms_timer_handle_t *nvkms_alloc_timer(nvkms_timer_proc_t *proc, void *dataPtr,
                                        NvU32 dataU32, NvU64 usec)
{
    (void)proc;
    (void)dataPtr;
    (void)dataU32;
    (void)usec;
    return NULL;
}
NvBool nvkms_alloc_timer_with_ref_ptr(nvkms_timer_proc_t *proc,
                                      struct nvkms_ref_ptr *ref_ptr,
                                      NvU32 dataU32, NvU64 usec)
{
    (void)proc;
    (void)ref_ptr;
    (void)dataU32;
    (void)usec;
    return NV_FALSE;
}
void nvkms_free_timer(nvkms_timer_handle_t *handle) { (void)handle; }

void nvkms_event_queue_changed(nvkms_per_open_handle_t *pOpenKernel,
                               NvBool eventsAvailable)
{
    (void)pOpenKernel;
    (void)eventsAvailable;
}

void *nvkms_get_per_open_data(int fd)
{
    (void)fd;
    return NULL;
}

NvBool nvkms_open_gpu(NvU32 gpuId)
{
    (void)gpuId;
    return NV_FALSE;
}
void nvkms_close_gpu(NvU32 gpuId) { (void)gpuId; }
NvU32 nvkms_enumerate_gpus(nv_gpu_info_t *gpu_info)
{
    (void)gpu_info;
    return 0;
}

NvBool nvkms_allow_write_combining(void) { return NV_FALSE; }
NvBool nvkms_kernel_supports_syncpts(void) { return NV_FALSE; }
NvBool nvkms_fd_is_nvidia_chardev(int fd)
{
    (void)fd;
    return NV_FALSE;
}

/*
 * A kapi client's open of nvkms: nvkms_open_common / nvkms_ioctl_common /
 * nvkms_close_pm_locked of nvidia-modeset-linux.c, minus what this port does
 * not have yet. Missing until PR 2: the global nvkms_lock every entry into
 * nvkms takes, and the kthread queue that delivers kapi events. Today
 * nvkms_alloc fails, so no open ever gets as far as nvKmsOpen -- but these
 * are real calls, which is what makes the linker resolve the whole of nvkms
 * (nvKmsIoctl reaches EVO, modeset and DisplayPort) instead of the handful
 * of kapi functions the table points at.
 */
struct nvkms_per_open {
    void *data;
};

struct nvkms_per_open *nvkms_open_from_kapi(struct NvKmsKapiDevice *device)
{
    struct nvkms_per_open *popen = nvkms_alloc(sizeof(*popen), NV_TRUE);

    (void)device;
    if (popen == NULL)
        return NULL;
    popen->data = nvKmsOpen(0 /* pid: the kernel */, NVKMS_CLIENT_KERNEL_SPACE, popen);
    if (popen->data == NULL) {
        nvkms_free(popen, sizeof(*popen));
        return NULL;
    }
    return popen;
}

void nvkms_close_from_kapi(struct nvkms_per_open *popen)
{
    nvKmsClose(popen->data);
    popen->data = NULL;
    nvkms_free(popen, sizeof(*popen));
}

NvBool nvkms_ioctl_from_kapi(struct nvkms_per_open *popen, NvU32 cmd,
                             void *params_address, const size_t params_size)
{
    if (popen->data == NULL)
        return NV_FALSE;
    return nvKmsIoctl(popen->data, cmd, (NvU64)(NvUPtr)params_address, params_size);
}

/* No power management here, so the PM lock never fails to be taken. */
NvBool nvkms_ioctl_from_kapi_try_pmlock(struct nvkms_per_open *popen, NvU32 cmd,
                                        void *params_address,
                                        const size_t params_size)
{
    return nvkms_ioctl_from_kapi(popen, cmd, params_address, params_size);
}

nvkms_sema_handle_t *nvkms_sema_alloc(void) { return NULL; }
void nvkms_sema_free(nvkms_sema_handle_t *sema) { (void)sema; }
void nvkms_sema_down(nvkms_sema_handle_t *sema) { (void)sema; }
void nvkms_sema_up(nvkms_sema_handle_t *sema) { (void)sema; }

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
