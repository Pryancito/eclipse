// The kernels behind cuda-probe's deep checks: one per kind of work that
// llama.cpp's CUDA backend (Ollama's runner) gives the GPU, each with an
// answer the host can check exactly.
//
// libcuda only loads PTX or cubins, and the image has no CUDA compiler, so
// cuda-probe carries the PTX these compile to (kernels_ptx.h). After an edit,
// regenerate it with ./gen-kernels.sh, which needs clang (NVPTX) and, to check
// the result, NVIDIA's ptxas.
//
// No CUDA headers: clang's builtins stand in for them.

#define __global__ __attribute__((global))
#define __device__ __attribute__((device))
#define __shared__ __attribute__((shared))

typedef unsigned int u32;

static __device__ u32 lane_id() { return __nvvm_read_ptx_sreg_tid_x(); }
static __device__ u32 block_id() { return __nvvm_read_ptx_sreg_ctaid_x(); }
static __device__ u32 block_dim() { return __nvvm_read_ptx_sreg_ntid_x(); }
static __device__ u32 grid_dim() { return __nvvm_read_ptx_sreg_nctaid_x(); }

// Elementwise over a grid-stride loop: what add, mul, scale, rope and the
// other elementwise ops do, over many blocks.
extern "C" __global__ void axpy_u32(u32 *c, const u32 *a, const u32 *b, u32 n)
{
    u32 stride = block_dim() * grid_dim();
    for (u32 i = block_id() * block_dim() + lane_id(); i < n; i += stride)
        c[i] = a[i] * 3u + b[i];
}

// A block's sum through warp shuffles and shared memory, as rms_norm and
// soft_max reduce a row.
extern "C" __global__ void block_sum(u32 *out, const u32 *in)
{
    __shared__ u32 warp_sums[32];
    u32 t = lane_id();
    u32 v = in[block_id() * block_dim() + t];
    for (int d = 16; d > 0; d >>= 1)
        v += __nvvm_shfl_sync_down_i32(0xffffffffu, v, d, 31);
    if ((t & 31) == 0)
        warp_sums[t >> 5] = v;
    __syncthreads();
    if (t < 32) {
        u32 w = t < block_dim() / 32 ? warp_sums[t] : 0;
        for (int d = 16; d > 0; d >>= 1)
            w += __nvvm_shfl_sync_down_i32(0xffffffffu, w, d, 31);
        if (t == 0)
            out[block_id()] = w;
    }
}

// Dynamic shared memory beyond the 48 KiB a kernel gets unasked, which
// llama.cpp's mmq and flash attention kernels opt into. Each thread reads
// back words other threads wrote.
extern "C" __global__ void big_shared(u32 *out, u32 words)
{
    extern __shared__ u32 smem[];
    u32 t = lane_id();
    u32 n = block_dim();
    for (u32 i = t; i < words; i += n)
        smem[i] = i * 2654435761u + block_id();
    __syncthreads();
    u32 sum = 0;
    for (u32 i = (t + 1) % n; i < words; i += n)
        sum += smem[i];
    out[block_id() * n + t] = sum;
}

// Four int8 products and a sum in one instruction (dp4a): llama.cpp's
// quantized matrix-vector kernels (mmvq), which every decoded token runs.
extern "C" __global__ void dot_i8(int *out, const int *a, const int *b, u32 n)
{
    u32 i = block_id() * block_dim() + lane_id();
    if (i < n) {
        int acc = (int)i;
        asm volatile("dp4a.s32.s32 %0, %1, %2, %0;" : "+r"(acc) : "r"(a[i]), "r"(b[i]));
        out[i] = acc;
    }
}

// One warp's int8 tensor-core product (mma m8n8k16): llama.cpp's mmq on
// Turing, which runs a prompt. Every A byte is 1 and every B byte 2, so each
// of the 64 sums is 16 * 2 on top of its accumulator, whatever the fragment
// layout.
extern "C" __global__ void mma_i8(int *out)
{
    u32 lane = lane_id() & 31;
    int d0 = (int)lane;
    int d1 = (int)lane + 100;
    u32 a = 0x01010101u;
    u32 b = 0x02020202u;
    asm volatile("mma.sync.aligned.m8n8k16.row.col.s32.s8.s8.s32 {%0, %1}, {%2}, {%3}, {%0, %1};"
                 : "+r"(d0), "+r"(d1)
                 : "r"(a), "r"(b));
    out[lane * 2] = d0;
    out[lane * 2 + 1] = d1;
}

// One warp's half-precision tensor-core product (mma m16n8k8, f32
// accumulate): cuBLAS's f16 GEMMs and flash attention. Every A element is 1.0
// and every B element 2.0, so each sum is 8 * 2 on top of its accumulator.
extern "C" __global__ void mma_f16(float *out)
{
    u32 lane = lane_id() & 31;
    float d0 = (float)lane, d1 = (float)lane + 0.5f, d2 = (float)lane + 1.0f, d3 = (float)lane + 1.5f;
    u32 a = 0x3c003c00u; // two halves of 1.0
    u32 b = 0x40004000u; // two halves of 2.0
    asm volatile("mma.sync.aligned.m16n8k8.row.col.f32.f16.f16.f32 "
                 "{%0, %1, %2, %3}, {%4, %5}, {%6}, {%0, %1, %2, %3};"
                 : "+f"(d0), "+f"(d1), "+f"(d2), "+f"(d3)
                 : "r"(a), "r"(a), "r"(b));
    out[lane * 4] = d0;
    out[lane * 4 + 1] = d1;
    out[lane * 4 + 2] = d2;
    out[lane * 4 + 3] = d3;
}

// Nothing at all: what a launch and a wait cost on their own.
extern "C" __global__ void empty(void) {}
