// Eclipse OS: a probe of CUDA itself, through NVIDIA's own libcuda.
//
// Where nvctl-probe drives /dev/nvidiactl by hand, this one asks libcuda,
// the library every CUDA program (ollama's runner included) goes through,
// and reports the first step that breaks, with CUDA's own name for the
// error:
//
//   preflight devices on disk, cmdline hatch, dlopen of libcuda.so.1
//   init      cuInit: opens /dev/nvidiactl and /dev/nvidia-uvm. Without
//             /dev/nvidia-uvm, or with any of its first three calls failing,
//             this is CUDA_ERROR_UNKNOWN (999).
//   devices   cuDeviceGetCount, then for each GPU its name, its memory and
//             its compute capability.
//   context   cuCtxCreate on the first GPU: where libcuda registers the GPU,
//             its VA space and its channels with /dev/nvidia-uvm.
//   memory    cuMemAlloc of 1 MiB, cuMemsetD8, and cuMemcpyDtoH of it back:
//             the pattern must come back.
//   launch    load a tiny PTX kernel, cuLaunchKernel, sync, and check a
//             device store (what Ollama's runner needs beyond alloc).
//
// When a step fails, `dmesg | grep -E 'nvidia|mmap'` names the NVIDIA call
// the kernel does not serve yet, if that is what failed, or the mmap it
// refused: CUDA_ERROR_OUT_OF_MEMORY (2) from cuInit is libcuda failing to
// reserve its address space. Any other failing call is in
// `/proc/bootprofile` when the kernel is booted with `BOOTTRACE=cuda-probe`
// as well: it lists every call of this program that failed, with its errno.
// CUDA_ERROR_OPERATING_SYSTEM (304) says no more than that one did.
//
// Needs the kernel booted with `nvidia.cuda` (or with both `nvidia.nvidiactl`
// and `nvidia.uvm`). A glibc program, like libcuda itself: it is built with
// the build machine's C compiler, not musl, and runs on the glibc that comes
// with NVIDIA's userspace. It reaches libcuda through dlopen, so it needs no
// CUDA headers.
#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
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
    printf("[%s] FAILED. What the kernel does not serve or refused: dmesg | grep -E 'nvidia|mmap'\n"
           "  Every call that failed, booted with BOOTTRACE=cuda-probe:\n"
           "  grep -E 'FAIL| e[0-9]' /proc/bootprofile\n",
           section);
    return 1;
}

static int exists(const char *path)
{
    return access(path, F_OK) == 0;
}

static int cmdline_has(const char *needle)
{
    char buf[4096];
    int fd = open("/proc/cmdline", O_RDONLY);
    if (fd < 0)
        return -1;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n < 0)
        return -1;
    buf[n] = 0;
    return strstr(buf, needle) != NULL;
}

// Devices before dlopen, so missing nodes are not mistaken for a broken
// libcuda. The cmdline hatch is advisory: operators enable nvidia.cuda by
// hand on the ESP; what matters at runtime is that the nodes exist.
static int preflight(void)
{
    int ok = 1;
    printf("[preflight]\n");
    int hatch = cmdline_has("nvidia.cuda");
    int ctl = cmdline_has("nvidia.nvidiactl");
    int uvm_flag = cmdline_has("nvidia.uvm");
    if (hatch < 0) {
        printf("  cmdline     (no /proc/cmdline)\n");
    } else if (hatch) {
        printf("  cmdline     nvidia.cuda\n");
    } else if (ctl && uvm_flag) {
        printf("  cmdline     nvidia.nvidiactl + nvidia.uvm\n");
    } else {
        printf("  cmdline     (no nvidia.cuda in /proc/cmdline — enable it on the ESP if nodes are missing)\n");
    }
    if (exists("/dev/nvidiactl"))
        printf("  /dev/nvidiactl ok\n");
    else {
        printf("  /dev/nvidiactl MISSING\n");
        ok = 0;
    }
    if (exists("/dev/nvidia-uvm"))
        printf("  /dev/nvidia-uvm ok\n");
    else {
        printf("  /dev/nvidia-uvm MISSING\n");
        ok = 0;
    }
    if (exists("/dev/nvidia0"))
        printf("  /dev/nvidia0 ok\n");
    else {
        printf("  /dev/nvidia0  MISSING (no GPU attached to the RM?)\n");
        ok = 0;
    }
    if (!ok) {
        printf("[preflight] FAILED. Need /dev/nvidia* (boot with nvidia.cuda on the ESP); see docs/README-cuda.md\n");
        return 0;
    }
    printf("  devices     ok\n");
    return 1;
}

// Tiny PTX: store one byte from a param into the device pointer. sm_50 is
// the floor Ollama documents; the JIT retargets for the card in use.
static const char write_byte_ptx[] =
    ".version 6.4\n"
    ".target sm_50\n"
    ".address_size 64\n"
    ".visible .entry write_byte(\n"
    "  .param .u64 write_byte_param_0,\n"
    "  .param .u32 write_byte_param_1\n"
    ")\n"
    "{\n"
    "  .reg .b64 %rd<2>;\n"
    "  .reg .b32 %r<2>;\n"
    "  ld.param.u64 %rd0, [write_byte_param_0];\n"
    "  ld.param.u32 %r0, [write_byte_param_1];\n"
    "  cvt.u8.u32 %r1, %r0;\n"
    "  st.u8 [%rd0], %r1;\n"
    "  ret;\n"
    "}\n";

int main(void)
{
    if (!preflight())
        return 1;

    printf("[init]\n");
    cuda = dlopen("libcuda.so.1", RTLD_NOW);
    if (!cuda) {
        printf("  dlopen      FAILED: %s\n", dlerror());
        printf("              need libcuda 580 from NVIDIA userspace in the rootfs\n");
        return fail("init");
    }
    printf("  dlopen      ok\n");
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
    CUresult (*ctx_sync)(void) = (CUresult(*)(void))sym("cuCtxSynchronize");
    CUresult (*mem_alloc)(CUdeviceptr *, size_t) = (CUresult(*)(CUdeviceptr *, size_t))sym("cuMemAlloc_v2");
    CUresult (*mem_free)(CUdeviceptr) = (CUresult(*)(CUdeviceptr))sym("cuMemFree_v2");
    CUresult (*memset_d8)(CUdeviceptr, unsigned char, size_t) = (CUresult(*)(CUdeviceptr, unsigned char, size_t))sym("cuMemsetD8_v2");
    CUresult (*dtoh)(void *, CUdeviceptr, size_t) = (CUresult(*)(void *, CUdeviceptr, size_t))sym("cuMemcpyDtoH_v2");
    CUresult (*mod_load)(CUmodule *, const void *) = (CUresult(*)(CUmodule *, const void *))sym("cuModuleLoadData");
    CUresult (*mod_get)(CUfunction *, CUmodule, const char *) =
        (CUresult(*)(CUfunction *, CUmodule, const char *))sym("cuModuleGetFunction");
    CUresult (*mod_unload)(CUmodule) = (CUresult(*)(CUmodule))sym("cuModuleUnload");
    CUresult (*launch)(CUfunction, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned,
                       unsigned, void *, void **, void **) =
        (CUresult(*)(CUfunction, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned,
                     unsigned, void *, void **, void **))sym("cuLaunchKernel");
    if (!init || !count || !get || !get_name || !total_mem || !attribute || !ctx_create ||
        !ctx_destroy || !ctx_sync || !mem_alloc || !mem_free || !memset_d8 || !dtoh || !mod_load ||
        !mod_get || !mod_unload || !launch) {
        printf("  symbols     FAILED: libcuda is missing an entry point\n");
        return fail("init");
    }

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
    if (!ok) {
        if (p)
            report("cuMemFree", mem_free(p));
        report("cuCtxDestroy", ctx_destroy(ctx));
        return fail("memory");
    }
    printf("  pattern     ok\n");

    printf("[launch]\n");
    CUmodule mod = 0;
    CUfunction fn = 0;
    int launch_ok = report("loadPTX", mod_load(&mod, write_byte_ptx));
    launch_ok = launch_ok && report("getFn", mod_get(&fn, mod, "write_byte"));
    if (launch_ok) {
        unsigned val = 0x5a;
        void *args[] = {&p, &val};
        launch_ok = report("cuLaunch", launch(fn, 1, 1, 1, 1, 1, 1, 0, 0, args, 0));
        launch_ok = launch_ok && report("sync", ctx_sync());
        memset(back, 0, 16);
        launch_ok = launch_ok && report("DtoH", dtoh(back, p, 16));
        if (launch_ok && back[0] != 0x5a) {
            printf("  store       FAILED: first byte %#x, want 0x5a\n", back[0]);
            launch_ok = 0;
        } else if (launch_ok) {
            printf("  store       ok\n");
        }
    }
    if (mod)
        report("unload", mod_unload(mod));
    if (p)
        report("cuMemFree", mem_free(p));
    report("cuCtxDestroy", ctx_destroy(ctx));
    if (!launch_ok)
        return fail("launch");

    printf("OK: CUDA works on GPU 0 (init, memory, launch)\n");
    return 0;
}
