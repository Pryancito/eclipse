// Eclipse OS: a probe of CUDA itself, through NVIDIA's own libcuda.
//
// Where nvctl-probe drives /dev/nvidiactl by hand, this one asks libcuda,
// the library every CUDA program (ollama's runner included) goes through,
// and reports the first step that breaks, with CUDA's own name for the
// error:
//
//   init      cuInit: opens /dev/nvidiactl and /dev/nvidia-uvm. Without
//             /dev/nvidia-uvm, or with any of its first three calls failing,
//             this is CUDA_ERROR_UNKNOWN (999).
//   devices   cuDeviceGetCount, then for each GPU its name, its memory and
//             its compute capability.
//   context   cuCtxCreate on the first GPU: where libcuda registers the GPU,
//             its VA space and its channels with /dev/nvidia-uvm.
//   memory    cuMemAlloc of 1 MiB, cuMemsetD8, and cuMemcpyDtoH of it back:
//             the pattern must come back.
//
// When a step fails on /dev/nvidia-uvm, `dmesg | grep nvidia-uvm` names the
// call the kernel does not serve yet.
//
// Needs the kernel booted with `nvidia.nvidiactl` and `nvidia.uvm`. A glibc
// program, like libcuda itself: it is built with the build machine's C
// compiler, not musl, and runs on the glibc that comes with NVIDIA's
// userspace. It reaches libcuda through dlopen, so it needs no CUDA headers.
#include <dlfcn.h>
#include <stdio.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef unsigned long long CUdeviceptr;

static void *cuda;
static int (*get_error_name)(CUresult, const char **);
static int (*get_error_string)(CUresult, const char **);

static void *sym(const char *name)
{
    void *p = dlsym(cuda, name);
    if (!p)
        printf("  libcuda has no %s\n", name);
    return p;
}

// Prints one step's result; returns whether it succeeded.
static int report(const char *step, CUresult r)
{
    const char *name = "?", *text = "";
    if (r == 0) {
        printf("  %-12s ok\n", step);
        return 1;
    }
    if (get_error_name)
        get_error_name(r, &name);
    if (get_error_string)
        get_error_string(r, &text);
    printf("  %-12s FAILED: %d %s (%s)\n", step, r, name ? name : "?", text ? text : "");
    return 0;
}

static int fail(const char *section)
{
    printf("[%s] FAILED. If it is /dev/nvidia-uvm: dmesg | grep nvidia-uvm\n", section);
    return 1;
}

int main(void)
{
    cuda = dlopen("libcuda.so.1", RTLD_NOW);
    if (!cuda) {
        printf("[init] FAILED: dlopen libcuda.so.1: %s\n", dlerror());
        return 1;
    }
    get_error_name = (int (*)(CUresult, const char **))dlsym(cuda, "cuGetErrorName");
    get_error_string = (int (*)(CUresult, const char **))dlsym(cuda, "cuGetErrorString");

    CUresult (*init)(unsigned) = (CUresult(*)(unsigned))sym("cuInit");
    CUresult (*count)(int *) = (CUresult(*)(int *))sym("cuDeviceGetCount");
    CUresult (*get)(CUdevice *, int) = (CUresult(*)(CUdevice *, int))sym("cuDeviceGet");
    CUresult (*get_name)(char *, int, CUdevice) = (CUresult(*)(char *, int, CUdevice))sym("cuDeviceGetName");
    CUresult (*total_mem)(size_t *, CUdevice) = (CUresult(*)(size_t *, CUdevice))sym("cuDeviceTotalMem_v2");
    CUresult (*attribute)(int *, int, CUdevice) = (CUresult(*)(int *, int, CUdevice))sym("cuDeviceGetAttribute");
    CUresult (*ctx_create)(CUcontext *, unsigned, CUdevice) = (CUresult(*)(CUcontext *, unsigned, CUdevice))sym("cuCtxCreate_v2");
    CUresult (*ctx_destroy)(CUcontext) = (CUresult(*)(CUcontext))sym("cuCtxDestroy_v2");
    CUresult (*mem_alloc)(CUdeviceptr *, size_t) = (CUresult(*)(CUdeviceptr *, size_t))sym("cuMemAlloc_v2");
    CUresult (*mem_free)(CUdeviceptr) = (CUresult(*)(CUdeviceptr))sym("cuMemFree_v2");
    CUresult (*memset_d8)(CUdeviceptr, unsigned char, size_t) = (CUresult(*)(CUdeviceptr, unsigned char, size_t))sym("cuMemsetD8_v2");
    CUresult (*dtoh)(void *, CUdeviceptr, size_t) = (CUresult(*)(void *, CUdeviceptr, size_t))sym("cuMemcpyDtoH_v2");
    if (!init || !count || !get || !get_name || !total_mem || !attribute || !ctx_create ||
        !ctx_destroy || !mem_alloc || !mem_free || !memset_d8 || !dtoh) {
        printf("[init] FAILED: libcuda is missing an entry point\n");
        return 1;
    }

    printf("[init]\n");
    if (!report("cuInit", init(0)))
        return fail("init");

    printf("[devices]\n");
    int n = 0;
    if (!report("count", count(&n)))
        return fail("devices");
    for (int i = 0; i < n; i++) {
        CUdevice dev;
        char name[256] = "";
        size_t bytes = 0;
        int major = 0, minor = 0;
        if (!report("get", get(&dev, i)) || !report("name", get_name(name, sizeof name, dev)) ||
            !report("memory", total_mem(&bytes, dev)) ||
            // CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR / _MINOR
            !report("capability", attribute(&major, 75, dev)) ||
            !report("capability", attribute(&minor, 76, dev)))
            return fail("devices");
        printf("  GPU %d: %s, %zu MiB, compute %d.%d\n", i, name, bytes >> 20, major, minor);
    }
    if (n == 0) {
        printf("[devices] FAILED: no GPU\n");
        return 1;
    }

    printf("[context]\n");
    CUdevice dev;
    CUcontext ctx = 0;
    if (!report("get", get(&dev, 0)) || !report("cuCtxCreate", ctx_create(&ctx, 0, dev)))
        return fail("context");

    printf("[memory]\n");
    enum { SIZE = 1 << 20 };
    static unsigned char back[SIZE];
    CUdeviceptr p = 0;
    int ok = report("cuMemAlloc", mem_alloc(&p, SIZE));
    ok = ok && report("cuMemsetD8", memset_d8(p, 0xa5, SIZE));
    ok = ok && report("cuMemcpyDtoH", dtoh(back, p, SIZE));
    if (ok) {
        for (size_t i = 0; i < SIZE; i++) {
            if (back[i] != 0xa5) {
                printf("  byte %zu came back %#x, not 0xa5\n", i, back[i]);
                ok = 0;
                break;
            }
        }
    }
    if (p)
        report("cuMemFree", mem_free(p));
    report("cuCtxDestroy", ctx_destroy(ctx));
    if (!ok)
        return fail("memory");

    printf("OK: CUDA works on GPU 0\n");
    return 0;
}
