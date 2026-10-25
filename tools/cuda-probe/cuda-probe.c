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
// `cuda-probe -d` (deep) runs those steps quietly and then, on every GPU,
// the kinds of work llama.cpp's CUDA backend (Ollama's runner) gives it,
// each with an answer checked exactly, plus what each costs:
//
//   copy      64 MiB host to GPU and back from ordinary memory, from
//             cuMemAllocHost memory, and from memory cuMemHostRegister pins
//             (on a stream), with MB/s each way; and from a file mapped the
//             way llama.cpp maps a model, untouched by the CPU.
//   vmm       llama.cpp's buffer pool: a 32 GiB reservation with physical
//             memory mapped into it piece by piece (cuMemCreate, cuMemMap,
//             cuMemSetAccess), written by a kernel across the pieces.
//   kernels   elementwise over many blocks, a block reduction (shuffles and
//             shared memory), 64 KiB of dynamic shared memory, int8 dot
//             products (dp4a, a decoded token), int8 and f16 tensor cores
//             (mma, a prompt), and a CUDA graph (how llama.cpp decodes).
//   latency   what a launch costs, and a launch and the wait for it.
//
// A wrong answer prints where it went wrong; a GPU that never finishes is
// named after 60 s instead of hanging the probe. Its kernels are
// kernels.cu, carried as PTX in kernels_ptx.h.
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
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include "kernels_ptx.h"

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
typedef void *CUstream;
typedef void *CUevent;
typedef void *CUgraph;
typedef void *CUgraphExec;
typedef unsigned long long CUdeviceptr;
typedef unsigned long long CUmemGenericAllocationHandle;

// cuda.h's CUmemLocation, CUmemAllocationProp and CUmemAccessDesc.
typedef struct {
    int type;
    int id;
} CUmemLocation;
typedef struct {
    int type;
    int requestedHandleTypes;
    CUmemLocation location;
    void *win32HandleMetaData;
    unsigned char compressionType;
    unsigned char gpuDirectRDMACapable;
    unsigned short usage;
    unsigned char reserved[4];
} CUmemAllocationProp;
typedef struct {
    CUmemLocation location;
    int flags;
} CUmemAccessDesc;

// -d: the basic steps print only what fails, and the deep checks follow.
static int quiet;

static void *cuda;
static int (*get_error_name)(CUresult, const char **);
static int (*get_error_string)(CUresult, const char **);

// What a basic step prints when it goes right: nothing under -d.
static void say(const char *line)
{
    if (!quiet)
        fputs(line, stdout);
}

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
        if (!quiet)
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
    say("[preflight]\n");
    int hatch = cmdline_has("nvidia.cuda");
    int ctl = cmdline_has("nvidia.nvidiactl");
    int uvm_flag = cmdline_has("nvidia.uvm");
    if (hatch < 0) {
        say("  cmdline     (no /proc/cmdline)\n");
    } else if (hatch) {
        say("  cmdline     nvidia.cuda\n");
    } else if (ctl && uvm_flag) {
        say("  cmdline     nvidia.nvidiactl + nvidia.uvm\n");
    } else {
        printf("  cmdline     (no nvidia.cuda in /proc/cmdline — enable it on the ESP if nodes are missing)\n");
    }
    if (exists("/dev/nvidiactl"))
        say("  /dev/nvidiactl ok\n");
    else {
        printf("  /dev/nvidiactl MISSING\n");
        ok = 0;
    }
    if (exists("/dev/nvidia-uvm"))
        say("  /dev/nvidia-uvm ok\n");
    else {
        printf("  /dev/nvidia-uvm MISSING\n");
        ok = 0;
    }
    if (exists("/dev/nvidia0"))
        say("  /dev/nvidia0 ok\n");
    else {
        printf("  /dev/nvidia0  MISSING (no GPU attached to the RM?)\n");
        ok = 0;
    }
    if (!ok) {
        printf("[preflight] FAILED. Need /dev/nvidia* (boot with nvidia.cuda on the ESP); see docs/README-cuda.md\n");
        return 0;
    }
    say("  devices     ok\n");
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

// ---- Deep checks (-d) ------------------------------------------------------

static CUresult (*d_ctx_create)(CUcontext *, unsigned, CUdevice);
static CUresult (*d_ctx_destroy)(CUcontext);
static CUresult (*d_ctx_sync)(void);
static CUresult (*d_attribute)(int *, int, CUdevice);
static CUresult (*d_alloc)(CUdeviceptr *, size_t);
static CUresult (*d_free)(CUdeviceptr);
static CUresult (*d_htod)(CUdeviceptr, const void *, size_t);
static CUresult (*d_dtoh)(void *, CUdeviceptr, size_t);
static CUresult (*d_alloc_host)(void **, size_t);
static CUresult (*d_free_host)(void *);
static CUresult (*d_register)(void *, size_t, unsigned);
static CUresult (*d_unregister)(void *);
static CUresult (*d_stream_create)(CUstream *, unsigned);
static CUresult (*d_stream_destroy)(CUstream);
static CUresult (*d_stream_sync)(CUstream);
static CUresult (*d_htod_async)(CUdeviceptr, const void *, size_t, CUstream);
static CUresult (*d_dtoh_async)(void *, CUdeviceptr, size_t, CUstream);
static CUresult (*d_mod_load)(CUmodule *, const void *);
static CUresult (*d_mod_get)(CUfunction *, CUmodule, const char *);
static CUresult (*d_mod_unload)(CUmodule);
static CUresult (*d_func_attr)(CUfunction, int, int);
static CUresult (*d_launch)(CUfunction, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned,
                            unsigned, CUstream, void **, void **);
static CUresult (*d_event_create)(CUevent *, unsigned);
static CUresult (*d_event_record)(CUevent, CUstream);
static CUresult (*d_event_sync)(CUevent);
static CUresult (*d_event_elapsed)(float *, CUevent, CUevent);
static CUresult (*d_event_destroy)(CUevent);
static CUresult (*d_capture_begin)(CUstream, int);
static CUresult (*d_capture_end)(CUstream, CUgraph *);
static CUresult (*d_graph_instantiate)(CUgraphExec *, CUgraph, unsigned long long);
static CUresult (*d_graph_launch)(CUgraphExec, CUstream);
static CUresult (*d_graph_exec_destroy)(CUgraphExec);
static CUresult (*d_graph_destroy)(CUgraph);
static CUresult (*d_can_peer)(int *, CUdevice, CUdevice);
static CUresult (*d_granularity)(size_t *, const CUmemAllocationProp *, int);
static CUresult (*d_reserve)(CUdeviceptr *, size_t, size_t, CUdeviceptr, unsigned long long);
static CUresult (*d_address_free)(CUdeviceptr, size_t);
static CUresult (*d_create)(CUmemGenericAllocationHandle *, size_t, const CUmemAllocationProp *, unsigned long long);
static CUresult (*d_release)(CUmemGenericAllocationHandle);
static CUresult (*d_map)(CUdeviceptr, size_t, size_t, CUmemGenericAllocationHandle, unsigned long long);
static CUresult (*d_unmap)(CUdeviceptr, size_t);
static CUresult (*d_set_access)(CUdeviceptr, size_t, const CUmemAccessDesc *, size_t);

#define DEEP_SYM(var, name) (var = (__typeof__(var))sym(name)) != NULL

static int deep_symbols(void)
{
    return DEEP_SYM(d_ctx_create, "cuCtxCreate_v2") && DEEP_SYM(d_ctx_destroy, "cuCtxDestroy_v2") &&
           DEEP_SYM(d_ctx_sync, "cuCtxSynchronize") && DEEP_SYM(d_attribute, "cuDeviceGetAttribute") &&
           DEEP_SYM(d_alloc, "cuMemAlloc_v2") && DEEP_SYM(d_free, "cuMemFree_v2") &&
           DEEP_SYM(d_htod, "cuMemcpyHtoD_v2") && DEEP_SYM(d_dtoh, "cuMemcpyDtoH_v2") &&
           DEEP_SYM(d_alloc_host, "cuMemAllocHost_v2") && DEEP_SYM(d_free_host, "cuMemFreeHost") &&
           DEEP_SYM(d_register, "cuMemHostRegister_v2") && DEEP_SYM(d_unregister, "cuMemHostUnregister") &&
           DEEP_SYM(d_stream_create, "cuStreamCreate") && DEEP_SYM(d_stream_destroy, "cuStreamDestroy_v2") &&
           DEEP_SYM(d_stream_sync, "cuStreamSynchronize") &&
           DEEP_SYM(d_htod_async, "cuMemcpyHtoDAsync_v2") && DEEP_SYM(d_dtoh_async, "cuMemcpyDtoHAsync_v2") &&
           DEEP_SYM(d_mod_load, "cuModuleLoadData") && DEEP_SYM(d_mod_get, "cuModuleGetFunction") &&
           DEEP_SYM(d_mod_unload, "cuModuleUnload") && DEEP_SYM(d_func_attr, "cuFuncSetAttribute") &&
           DEEP_SYM(d_launch, "cuLaunchKernel") && DEEP_SYM(d_event_create, "cuEventCreate") &&
           DEEP_SYM(d_event_record, "cuEventRecord") && DEEP_SYM(d_event_sync, "cuEventSynchronize") &&
           DEEP_SYM(d_event_elapsed, "cuEventElapsedTime") && DEEP_SYM(d_event_destroy, "cuEventDestroy_v2") &&
           DEEP_SYM(d_capture_begin, "cuStreamBeginCapture_v2") && DEEP_SYM(d_capture_end, "cuStreamEndCapture") &&
           DEEP_SYM(d_graph_instantiate, "cuGraphInstantiateWithFlags") &&
           DEEP_SYM(d_graph_launch, "cuGraphLaunch") && DEEP_SYM(d_graph_exec_destroy, "cuGraphExecDestroy") &&
           DEEP_SYM(d_graph_destroy, "cuGraphDestroy") && DEEP_SYM(d_can_peer, "cuDeviceCanAccessPeer") &&
           DEEP_SYM(d_granularity, "cuMemGetAllocationGranularity") &&
           DEEP_SYM(d_reserve, "cuMemAddressReserve") && DEEP_SYM(d_address_free, "cuMemAddressFree") &&
           DEEP_SYM(d_create, "cuMemCreate") && DEEP_SYM(d_release, "cuMemRelease") &&
           DEEP_SYM(d_map, "cuMemMap") && DEEP_SYM(d_unmap, "cuMemUnmap") &&
           DEEP_SYM(d_set_access, "cuMemSetAccess");
}

// The check under way, for the watchdog to name.
static const char *volatile deep_step = "";

static void deep_hung(int sig)
{
    static const char a[] = "  ";
    static const char b[] = " HUNG: the GPU never finished it in 60 s. dmesg | grep -E 'nvidia|NVRM'\n";
    (void)sig;
    if (write(1, a, sizeof a - 1) < 0 || write(1, deep_step, strlen(deep_step)) < 0 ||
        write(1, b, sizeof b - 1) < 0)
        _exit(4);
    _exit(3);
}

static void deep_begin(const char *step)
{
    deep_step = step;
    alarm(60);
}

static double now_us(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

// A failed call during a deep check: its name, and CUDA's for the error.
static int deep_call(const char *what, CUresult r)
{
    const char *name = "?";
    if (r == 0)
        return 1;
    if (get_error_name)
        get_error_name(r, &name);
    printf("  %-10s FAILED: %s: %d %s\n", deep_step, what, r, name ? name : "?");
    return 0;
}

static void deep_fill(unsigned *p, size_t n, unsigned seed)
{
    unsigned x = seed | 1;
    for (size_t i = 0; i < n; i++) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        p[i] = x;
    }
}

// The first word that differs, as a failure line; whether all matched.
static int deep_same(const unsigned *got, const unsigned *want, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        if (got[i] != want[i]) {
            printf("  %-10s FAILED: word %zu of %zu is %#x, want %#x\n", deep_step, i, n, got[i], want[i]);
            return 0;
        }
    }
    return 1;
}

static double mbps(size_t bytes, double us)
{
    return us > 0 ? bytes / us : 0;
}

enum { DEEP_WORDS = 16 << 20, DEEP_BYTES = DEEP_WORDS * 4, DEEP_BLOCKS = 4096, DEEP_THREADS = 256 };

struct deep {
    CUdeviceptr a, b, c;
    unsigned *ha, *hb, *hc, *want;
    CUstream stream;
    CUmodule mod;
};

static int deep_copy(struct deep *g)
{
    deep_begin("copy");
    double t0 = now_us();
    if (!deep_call("HtoD", d_htod(g->a, g->ha, DEEP_BYTES)))
        return 0;
    double t1 = now_us();
    if (!deep_call("DtoH", d_dtoh(g->hc, g->a, DEEP_BYTES)))
        return 0;
    double t2 = now_us();
    if (!deep_same(g->hc, g->ha, DEEP_WORDS))
        return 0;
    printf("  copy       64 MiB  HtoD %5.0f MB/s  DtoH %5.0f MB/s  ok\n", mbps(DEEP_BYTES, t1 - t0),
           mbps(DEEP_BYTES, t2 - t1));
    return 1;
}

static int deep_pinned(struct deep *g)
{
    deep_begin("pinned");
    void *p = NULL, *q = NULL;
    int ok = deep_call("cuMemAllocHost", d_alloc_host(&p, DEEP_BYTES)) &&
             deep_call("cuMemAllocHost", d_alloc_host(&q, DEEP_BYTES));
    double t0 = 0, t1 = 0, t2 = 0;
    if (ok) {
        memcpy(p, g->hb, DEEP_BYTES);
        memset(q, 0, DEEP_BYTES);
        t0 = now_us();
        ok = deep_call("HtoD", d_htod(g->b, p, DEEP_BYTES));
        t1 = now_us();
        ok = ok && deep_call("DtoH", d_dtoh(q, g->b, DEEP_BYTES));
        t2 = now_us();
        ok = ok && deep_same(q, g->hb, DEEP_WORDS);
    }
    if (ok)
        printf("  pinned     64 MiB  HtoD %5.0f MB/s  DtoH %5.0f MB/s  ok\n", mbps(DEEP_BYTES, t1 - t0),
               mbps(DEEP_BYTES, t2 - t1));
    if (p)
        d_free_host(p);
    if (q)
        d_free_host(q);
    return ok;
}

// Memory of the program's own, pinned by cuMemHostRegister, copied on a
// stream: the OS descriptor path.
static int deep_registered(struct deep *g)
{
    enum { WORDS = 4 << 20, BYTES = WORDS * 4 };
    deep_begin("registered");
    unsigned *r = aligned_alloc(4096, BYTES);
    if (!r) {
        printf("  %-10s FAILED: no host memory\n", deep_step);
        return 0;
    }
    deep_fill(r, WORDS, 0x5eed);
    deep_fill(g->want, WORDS, 0x5eed);
    int registered = deep_call("cuMemHostRegister", d_register(r, BYTES, 0));
    int ok = registered && deep_call("HtoDAsync", d_htod_async(g->c, r, BYTES, g->stream)) &&
             deep_call("sync", d_stream_sync(g->stream));
    if (ok) {
        memset(r, 0, BYTES);
        ok = deep_call("DtoHAsync", d_dtoh_async(r, g->c, BYTES, g->stream)) &&
             deep_call("sync", d_stream_sync(g->stream)) && deep_same(r, g->want, WORDS);
    }
    if (ok)
        printf("  registered 16 MiB on a stream  ok\n");
    if (registered)
        d_unregister(r);
    free(r);
    return ok;
}

static int deep_fn(struct deep *g, const char *name, CUfunction *fn);
static int deep_run(struct deep *g, CUfunction fn, unsigned blocks, unsigned threads, unsigned shared,
                    void **args, float *ms);

// A file mapped as llama.cpp maps a model (MAP_SHARED, PROT_READ) and
// copied to the GPU before the CPU has touched a page of it: the weights
// reach the GPU this way, from the page cache of the disk the file is on.
static int deep_file(struct deep *g)
{
    enum { WORDS = 16 << 20, BYTES = WORDS * 4 };
    deep_begin("file");
    const char *home = getenv("HOME");
    char path[512];
    snprintf(path, sizeof path, "%s/.cuda-probe-file", home && *home ? home : "/tmp");
    deep_fill(g->want, WORDS, 0xf11e);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    int ok = fd >= 0;
    for (size_t done = 0; ok && done < BYTES;) {
        ssize_t n = write(fd, (const char *)g->want + done, BYTES - done);
        ok = n > 0;
        done += ok ? (size_t)n : 0;
    }
    if (fd >= 0)
        close(fd);
    void *map = MAP_FAILED;
    if (ok) {
        fd = open(path, O_RDONLY);
        map = fd >= 0 ? mmap(NULL, BYTES, PROT_READ, MAP_SHARED, fd, 0) : MAP_FAILED;
        if (fd >= 0)
            close(fd);
    }
    if (!ok || map == MAP_FAILED) {
        printf("  %-10s FAILED: could not write and map %s\n", deep_step, path);
        unlink(path);
        return 0;
    }
    double t0 = now_us();
    ok = deep_call("HtoD", d_htod(g->c, map, BYTES));
    double t1 = now_us();
    munmap(map, BYTES);
    unlink(path);
    ok = ok && deep_call("DtoH", d_dtoh(g->hc, g->c, BYTES)) && deep_same(g->hc, g->want, WORDS);
    if (ok)
        printf("  file       64 MiB mapped  HtoD %5.0f MB/s  ok\n", mbps(BYTES, t1 - t0));
    return ok;
}

// llama.cpp's buffer pool: a large reservation of addresses (it reserves
// 32 GiB), physical memory made separately and mapped into it piece by
// piece, then opened to the GPU with cuMemSetAccess. A kernel writes across
// the two pieces.
static int deep_vmm(struct deep *g, CUdevice dev)
{
    enum { RESERVE_GIB = 32 };
    deep_begin("vmm");
    CUfunction fn;
    // CU_MEM_ALLOCATION_TYPE_PINNED, CU_MEM_LOCATION_TYPE_DEVICE
    CUmemAllocationProp prop;
    memset(&prop, 0, sizeof prop);
    prop.type = 1;
    prop.location.type = 1;
    prop.location.id = dev;
    size_t chunk = 0;
    CUdeviceptr base = 0;
    size_t reserved = (size_t)RESERVE_GIB << 30;
    CUmemGenericAllocationHandle h[2] = {0, 0};
    int mapped = 0;
    // CU_MEM_ALLOC_GRANULARITY_RECOMMENDED
    int ok = deep_fn(g, "axpy_u32", &fn) && deep_call("granularity", d_granularity(&chunk, &prop, 1)) &&
             deep_call("cuMemAddressReserve 32 GiB", d_reserve(&base, reserved, 0, 0, 0));
    if (!ok)
        return 0;
    for (int i = 0; ok && i < 2; i++) {
        ok = deep_call("cuMemCreate", d_create(&h[i], chunk, &prop, 0)) &&
             deep_call("cuMemMap", d_map(base + i * chunk, chunk, 0, h[i], 0));
        mapped += ok;
    }
    // CU_MEM_ACCESS_FLAGS_PROT_READWRITE
    CUmemAccessDesc access = {{1, dev}, 3};
    ok = ok && deep_call("cuMemSetAccess", d_set_access(base, 2 * chunk, &access, 1));
    // The host buffers hold 64 MiB; a larger granularity is checked that far.
    size_t bytes = 2 * chunk < DEEP_BYTES ? 2 * chunk : DEEP_BYTES;
    unsigned n = (unsigned)(bytes / 4);
    void *args[] = {&base, &g->a, &g->b, &n};
    ok = ok && deep_run(g, fn, DEEP_BLOCKS, DEEP_THREADS, 0, args, NULL) &&
         deep_call("DtoH", d_dtoh(g->hc, base, bytes));
    if (ok) {
        for (size_t i = 0; i < n; i++)
            g->want[i] = g->ha[i] * 3u + g->hb[i];
        ok = deep_same(g->hc, g->want, n);
    }
    for (int i = 0; i < mapped; i++)
        d_unmap(base + i * chunk, chunk);
    for (int i = 0; i < 2; i++) {
        if (h[i])
            d_release(h[i]);
    }
    d_address_free(base, reserved);
    if (ok)
        printf("  vmm        2 x %zu KiB in a 32 GiB reservation  ok\n", chunk >> 10);
    return ok;
}

static int deep_fn(struct deep *g, const char *name, CUfunction *fn)
{
    return deep_call(name, d_mod_get(fn, g->mod, name));
}

// Launches on the probe's stream and waits, timing the GPU with events.
static int deep_run(struct deep *g, CUfunction fn, unsigned blocks, unsigned threads, unsigned shared,
                    void **args, float *ms)
{
    CUevent e0 = NULL, e1 = NULL;
    int ok = deep_call("cuEventCreate", d_event_create(&e0, 0)) &&
             deep_call("cuEventCreate", d_event_create(&e1, 0)) &&
             deep_call("cuEventRecord", d_event_record(e0, g->stream)) &&
             deep_call("cuLaunchKernel", d_launch(fn, blocks, 1, 1, threads, 1, 1, shared, g->stream, args, NULL)) &&
             deep_call("cuEventRecord", d_event_record(e1, g->stream)) &&
             deep_call("sync", d_stream_sync(g->stream));
    if (ok && ms)
        ok = deep_call("cuEventElapsedTime", d_event_elapsed(ms, e0, e1));
    if (e0)
        d_event_destroy(e0);
    if (e1)
        d_event_destroy(e1);
    return ok;
}

static int deep_axpy(struct deep *g)
{
    deep_begin("axpy");
    CUfunction fn;
    unsigned n = DEEP_WORDS;
    void *args[] = {&g->c, &g->a, &g->b, &n};
    float ms = 0;
    if (!deep_fn(g, "axpy_u32", &fn) || !deep_run(g, fn, DEEP_BLOCKS, DEEP_THREADS, 0, args, &ms) ||
        !deep_call("DtoH", d_dtoh(g->hc, g->c, DEEP_BYTES)))
        return 0;
    for (size_t i = 0; i < DEEP_WORDS; i++)
        g->want[i] = g->ha[i] * 3u + g->hb[i];
    if (!deep_same(g->hc, g->want, DEEP_WORDS))
        return 0;
    printf("  axpy       16M words  GPU %.2f ms (%.0f GB/s)  ok\n", ms, ms > 0 ? 3.0 * DEEP_BYTES / (ms * 1e6) : 0);
    return 1;
}

static int deep_reduce(struct deep *g)
{
    deep_begin("reduce");
    CUfunction fn;
    void *args[] = {&g->c, &g->a};
    if (!deep_fn(g, "block_sum", &fn) || !deep_run(g, fn, DEEP_BLOCKS, DEEP_THREADS, 0, args, NULL) ||
        !deep_call("DtoH", d_dtoh(g->hc, g->c, DEEP_BLOCKS * 4)))
        return 0;
    for (size_t b = 0; b < DEEP_BLOCKS; b++) {
        unsigned s = 0;
        for (size_t t = 0; t < DEEP_THREADS; t++)
            s += g->ha[b * DEEP_THREADS + t];
        g->want[b] = s;
    }
    if (!deep_same(g->hc, g->want, DEEP_BLOCKS))
        return 0;
    printf("  reduce     shuffles + shared memory  ok\n");
    return 1;
}

static int deep_shared(struct deep *g, CUdevice dev)
{
    enum { BLOCKS = 64 };
    deep_begin("shared");
    CUfunction fn;
    int optin = 0;
    // CU_DEVICE_ATTRIBUTE_MAX_SHARED_MEMORY_PER_BLOCK_OPTIN
    if (!deep_call("attribute", d_attribute(&optin, 97, dev)) || !deep_fn(g, "big_shared", &fn))
        return 0;
    unsigned bytes = optin > 65536 ? 65536 : (unsigned)optin;
    unsigned words = bytes / 4;
    void *args[] = {&g->c, &words};
    // CU_FUNC_ATTRIBUTE_MAX_DYNAMIC_SHARED_SIZE_BYTES
    if (!deep_call("cuFuncSetAttribute", d_func_attr(fn, 8, (int)bytes)) ||
        !deep_run(g, fn, BLOCKS, DEEP_THREADS, bytes, args, NULL) ||
        !deep_call("DtoH", d_dtoh(g->hc, g->c, BLOCKS * DEEP_THREADS * 4)))
        return 0;
    for (unsigned b = 0; b < BLOCKS; b++) {
        for (unsigned t = 0; t < DEEP_THREADS; t++) {
            unsigned s = 0;
            for (unsigned i = (t + 1) % DEEP_THREADS; i < words; i += DEEP_THREADS)
                s += i * 2654435761u + b;
            g->want[b * DEEP_THREADS + t] = s;
        }
    }
    if (!deep_same(g->hc, g->want, BLOCKS * DEEP_THREADS))
        return 0;
    printf("  shared     %u KiB dynamic  ok\n", bytes >> 10);
    return 1;
}

static int deep_dp4a(struct deep *g)
{
    enum { N = 1 << 20 };
    deep_begin("dp4a");
    CUfunction fn;
    unsigned n = N;
    void *args[] = {&g->c, &g->a, &g->b, &n};
    if (!deep_fn(g, "dot_i8", &fn) || !deep_run(g, fn, N / DEEP_THREADS, DEEP_THREADS, 0, args, NULL) ||
        !deep_call("DtoH", d_dtoh(g->hc, g->c, N * 4)))
        return 0;
    for (unsigned i = 0; i < N; i++) {
        int acc = (int)i;
        for (int k = 0; k < 4; k++)
            acc += (signed char)(g->ha[i] >> (8 * k)) * (signed char)(g->hb[i] >> (8 * k));
        g->want[i] = (unsigned)acc;
    }
    if (!deep_same(g->hc, g->want, N))
        return 0;
    printf("  dp4a       int8 dot products  ok\n");
    return 1;
}

// One warp of tensor cores: each answer is its accumulator plus K * 1 * 2.
static int deep_mma(struct deep *g, const char *name, const char *step, int f16)
{
    deep_begin(step);
    CUfunction fn;
    void *args[] = {&g->c};
    unsigned words = f16 ? 128 : 64;
    if (!deep_fn(g, name, &fn) || !deep_run(g, fn, 1, 32, 0, args, NULL) ||
        !deep_call("DtoH", d_dtoh(g->hc, g->c, words * 4)))
        return 0;
    for (unsigned lane = 0; lane < 32; lane++) {
        if (f16) {
            for (unsigned j = 0; j < 4; j++) {
                float want = (float)lane + 0.5f * j + 16.0f;
                memcpy(&g->want[lane * 4 + j], &want, 4);
            }
        } else {
            g->want[lane * 2] = lane + 32;
            g->want[lane * 2 + 1] = lane + 100 + 32;
        }
    }
    if (!deep_same(g->hc, g->want, words))
        return 0;
    printf("  %-10s tensor cores  ok\n", step);
    return 1;
}

// llama.cpp decodes a token as one CUDA graph: capture, instantiate, launch.
static int deep_graph(struct deep *g)
{
    deep_begin("graph");
    CUfunction fn;
    unsigned n = DEEP_WORDS;
    void *args[] = {&g->c, &g->b, &g->a, &n};
    CUgraph graph = NULL;
    CUgraphExec exec = NULL;
    if (!deep_fn(g, "axpy_u32", &fn))
        return 0;
    // CU_STREAM_CAPTURE_MODE_RELAXED, as llama.cpp captures.
    int ok = deep_call("cuStreamBeginCapture", d_capture_begin(g->stream, 2));
    if (ok) {
        int launched = deep_call("cuLaunchKernel", d_launch(fn, DEEP_BLOCKS, 1, 1, DEEP_THREADS, 1, 1, 0, g->stream,
                                                            args, NULL));
        ok = deep_call("cuStreamEndCapture", d_capture_end(g->stream, &graph)) && launched;
    }
    ok = ok && deep_call("cuGraphInstantiate", d_graph_instantiate(&exec, graph, 0)) &&
         deep_call("cuGraphLaunch", d_graph_launch(exec, g->stream)) &&
         deep_call("sync", d_stream_sync(g->stream)) && deep_call("DtoH", d_dtoh(g->hc, g->c, DEEP_BYTES));
    if (ok) {
        for (size_t i = 0; i < DEEP_WORDS; i++)
            g->want[i] = g->hb[i] * 3u + g->ha[i];
        ok = deep_same(g->hc, g->want, DEEP_WORDS);
    }
    if (exec)
        d_graph_exec_destroy(exec);
    if (graph)
        d_graph_destroy(graph);
    if (ok)
        printf("  graph      captured and replayed  ok\n");
    return ok;
}

// What a launch costs, and a launch with the wait for it: llama.cpp waits
// for the GPU after every token.
static int deep_latency(struct deep *g)
{
    enum { WAITED = 200, QUEUED = 1000 };
    deep_begin("latency");
    CUfunction fn;
    if (!deep_fn(g, "empty", &fn) || !deep_call("sync", d_ctx_sync()))
        return 0;
    double t0 = now_us();
    for (int i = 0; i < WAITED; i++) {
        if (!deep_call("cuLaunchKernel", d_launch(fn, 1, 1, 1, 1, 1, 1, 0, NULL, NULL, NULL)) ||
            !deep_call("sync", d_ctx_sync()))
            return 0;
    }
    double t1 = now_us();
    for (int i = 0; i < QUEUED; i++) {
        if (!deep_call("cuLaunchKernel", d_launch(fn, 1, 1, 1, 1, 1, 1, 0, NULL, NULL, NULL)))
            return 0;
    }
    double t2 = now_us();
    if (!deep_call("sync", d_ctx_sync()))
        return 0;
    double t3 = now_us();
    printf("  latency    launch %.1f us  launch+wait %.1f us  1000 queued drain %.1f ms\n", (t2 - t1) / QUEUED,
           (t1 - t0) / WAITED, (t3 - t1) / 1e3);
    return 1;
}

// Every check on one GPU, in the order llama.cpp needs them; the first that
// fails ends that GPU's run.
static int deep_gpu(CUdevice dev, int index)
{
    struct deep g;
    memset(&g, 0, sizeof g);
    CUcontext ctx = NULL;
    int major = 0, minor = 0;
    printf("[deep GPU %d]\n", index);
    deep_step = "context";
    if (!deep_call("capability", d_attribute(&major, 75, dev)) ||
        !deep_call("capability", d_attribute(&minor, 76, dev)))
        return 0;
    if (major * 10 + minor < 75) {
        printf("  compute %d.%d: the deep checks need 7.5 (Turing) or newer\n", major, minor);
        return 1;
    }
    if (!deep_call("cuCtxCreate", d_ctx_create(&ctx, 0, dev)))
        return 0;
    g.ha = malloc(DEEP_BYTES);
    g.hb = malloc(DEEP_BYTES);
    g.hc = malloc(DEEP_BYTES);
    g.want = malloc(DEEP_BYTES);
    int ok = g.ha && g.hb && g.hc && g.want;
    if (!ok)
        printf("  no host memory for the deep checks\n");
    if (ok) {
        deep_fill(g.ha, DEEP_WORDS, 0x1234 + index);
        deep_fill(g.hb, DEEP_WORDS, 0xabcd + index);
        deep_step = "alloc";
        ok = deep_call("cuMemAlloc", d_alloc(&g.a, DEEP_BYTES)) && deep_call("cuMemAlloc", d_alloc(&g.b, DEEP_BYTES)) &&
             deep_call("cuMemAlloc", d_alloc(&g.c, DEEP_BYTES)) &&
             deep_call("cuStreamCreate", d_stream_create(&g.stream, 0));
    }
    if (ok) {
        deep_begin("module");
        ok = deep_call("cuModuleLoadData", d_mod_load(&g.mod, kernels_ptx));
    }
    ok = ok && deep_copy(&g) && deep_pinned(&g) && deep_registered(&g) && deep_file(&g) && deep_axpy(&g) &&
         deep_vmm(&g, dev) && deep_reduce(&g) &&
         deep_shared(&g, dev) && deep_dp4a(&g) && deep_mma(&g, "mma_i8", "mma int8", 0) &&
         deep_mma(&g, "mma_f16", "mma f16", 1) && deep_graph(&g) && deep_latency(&g);
    alarm(0);
    if (g.mod)
        d_mod_unload(g.mod);
    if (g.stream)
        d_stream_destroy(g.stream);
    if (g.a)
        d_free(g.a);
    if (g.b)
        d_free(g.b);
    if (g.c)
        d_free(g.c);
    free(g.ha);
    free(g.hb);
    free(g.hc);
    free(g.want);
    d_ctx_destroy(ctx);
    return ok;
}

static int deep_all(int gpus, CUresult (*get)(CUdevice *, int))
{
    if (!deep_symbols()) {
        printf("[deep] FAILED: libcuda is missing an entry point\n");
        return 1;
    }
    signal(SIGALRM, deep_hung);
    int failed = 0;
    for (int i = 0; i < gpus; i++) {
        CUdevice dev;
        deep_step = "device";
        if (!deep_call("cuDeviceGet", get(&dev, i)) || !deep_gpu(dev, i))
            failed++;
    }
    if (gpus > 1) {
        CUdevice d0, d1;
        int peer = 0;
        if (get(&d0, 0) == 0 && get(&d1, 1) == 0 && d_can_peer(&peer, d0, d1) == 0)
            printf("[peer] GPU 0 -> GPU 1 direct access: %s\n", peer ? "yes" : "no (copies go through the host)");
    }
    if (failed) {
        printf("DEEP FAILED on %d of %d GPU(s); the line above each FAILED names the check\n", failed, gpus);
        return 1;
    }
    printf("DEEP OK on %d GPU(s)\n", gpus);
    return 0;
}

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "-d") == 0 || strcmp(argv[i], "--deep") == 0) {
            quiet = 1;
        } else {
            printf("usage: cuda-probe [-d|--deep]\n"
                   "  -d  the basic steps print only failures; then every GPU gets the\n"
                   "      deep checks (copies, kernels, tensor cores, graphs, latency)\n");
            return 2;
        }
    }
    // Line by line, so a check the watchdog names follows the lines before it.
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (!preflight())
        return 1;

    say("[init]\n");
    cuda = dlopen("libcuda.so.1", RTLD_NOW);
    if (!cuda) {
        printf("  dlopen      FAILED: %s\n", dlerror());
        printf("              need libcuda 580 from NVIDIA userspace in the rootfs\n");
        return fail("init");
    }
    say("  dlopen      ok\n");
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

    say("[devices]\n");
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

    say("[context]\n");
    CUdevice dev;
    CUcontext ctx = 0;
    if (!report("get", get(&dev, 0)) || !report("cuCtxCreate", ctx_create(&ctx, 0, dev)))
        return fail("context");

    say("[memory]\n");
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
    say("  pattern     ok\n");

    say("[launch]\n");
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
            say("  store       ok\n");
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
    if (quiet)
        return deep_all(n, get);
    printf("Deeper (every GPU, what Ollama's runner does, with timings): cuda-probe -d\n");
    return 0;
}
