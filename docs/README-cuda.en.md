# CUDA on Eclipse OS

[Español](README-cuda.md) · **English**

Eclipse exposes the same character devices as `nvidia.ko` does on Linux, so
that a `libcuda.so` of the same version as the RM can talk to the Resource
Manager linked into the kernel. The boot hatch is **`nvidia.cuda`**.

## Boot

On the `rboot.conf` command line (ESP):

```
nvidia.cuda
```

This is equivalent to turning on `nvidia.nvidiactl` and `nvidia.uvm`. UVM alone
can be turned off with `nvidia.cuda:nvidia.uvm=0`. The product image
(`zCore/rboot.conf`) carries it by default; remove it (`nvidia.cuda=0`) on
machines without an NVIDIA compute GPU.

## Nodes

| Node | Major | Role |
|------|-------|-----|
| `/dev/nvidiactl` | 195:255 | RM control: version, CARD_INFO, Alloc/Free/Control, MAP, events |
| `/dev/nvidiaN` | 195:N | One per GPU whose `attach_gpu` succeeded |
| `/dev/nvidia-uvm` | 510 | UVM: `cuInit` + `cuCtxCreate` (everything else → `ENOSYS` + log) |

Pinned version: **580.178.04** (`open-gpu-kernel-modules` + `gsp.bin`). It must
match the `libcuda.so` in the rootfs.

## Userspace in the ISO

`cargo image` installs (unless `ECLIPSE_NVIDIA_USERSPACE=0`):

- `libcuda.so.1`, `libnvidia-ptxjitcompiler`, `libnvidia-ml`, `nvidia-smi`
- Ubuntu 26.10 glibc + `/lib64/ld-linux-x86-64.so.2` (libcuda is glibc)
- `/usr/bin/cuda-probe` — `cuInit` → devices → context → alloc/memset → **PTX launch**
- `/usr/bin/cuda-status` — hatch + nodes + libcuda (`-p` runs the probe)

No system cuBLAS is needed: Ollama's runner ships its own CUDA libraries and
talks to this `libcuda` (driver API).

## What the kernel covers today

**nvidiactl:** `CHECK_VERSION_STR`, `CARD_INFO`, `SYS_PARAMS`, `REGISTER_FD`,
OS events, `RM_ALLOC`/`FREE`/`CONTROL`/`DUP`/`SHARE`, `RM_MAP_MEMORY` + mmap,
`RM_UNMAP_MEMORY`, `RM_VID_HEAP_CONTROL` (VRAM for `cuMemAlloc`),
OS-descriptor `RM_ALLOC_MEMORY`, etc.

**nvidia-uvm (served):** `INITIALIZE`, `MM_INITIALIZE`, `PAGEABLE_MEM_ACCESS`,
`REGISTER`/`UNREGISTER_GPU`, range groups, GPU VA space, channels, external
map/unmap/free, semaphore pool (+ CPU mmap), reflected SKED, `VALIDATE_VA_RANGE`.

**Not yet:** UVM's own page tables / GPU page faults / HMM;
`cudaMallocManaged` (managed mmap → `ENODEV`); the remaining UVM ioctls →
`ENOSYS` (one line in dmesg the first time).

## Testing on the machine

```sh
cuda-status          # hatch + /dev/nvidia* + libcuda
cuda-status -p       # the above plus the full cuda-probe
# or:
cuda-probe
nvidia-smi -L
dmesg | grep -E 'nvidia-uvm|nvidiactl|mmap'
```

With `BOOTTRACE=cuda-probe` on the cmdline, `/proc/bootprofile` lists the
probe's failed syscalls (e.g. behind `CUDA_ERROR_OPERATING_SYSTEM` 304).

Expected output at the end: `OK: CUDA works on GPU 0 (init, memory, launch)`.

`cuda-probe -d` goes further, on every GPU: what Ollama's runner (llama.cpp)
asks for, with the exact answer checked and what it costs. 64 MiB copies
(normal memory, `cuMemAllocHost`, `cuMemHostRegister` on a stream, and a mapped
file the way llama.cpp maps the model) with their MB/s; llama.cpp's pool (a
32 GiB reservation with memory mapped in pieces via `cuMemCreate`/`cuMemMap`);
many-block kernels, reductions with shuffles and shared memory, 64 KiB of
dynamic shared memory, local memory, atomics, `dp4a`, int8 and f16 tensor
cores, a CUDA graph; and the latency of launching a kernel and of waiting for
it, both spinning and sleeping (`CU_EVENT_BLOCKING_SYNC`, which needs the GPU
interrupt to wake up). A wrong answer says where; a GPU that never finishes is
named after 60 s instead of hanging. The kernels are in
`tools/cuda-probe/kernels.cu` and ship as PTX in `kernels_ptx.h`
(`gen-kernels.sh` regenerates it). For reference, on a 2060 SUPER under Linux:
copies of 6 to 12 GB/s, about 3-5 µs to launch and about 10-20 µs to launch
and wait.

## Ollama (optional)

It is **not** in the default image. For those who want it:

```sh
apt-get install -y curl zstd ca-certificates   # if not already there
install-ollama-cuda                 # → /usr/local (official .tar.zst + cuda_v*)
cuda-status
```

The script also leaves the eclipse-init service
`/etc/eclipse/services/ollama.service` (`eclipse-ollama serve`, `respawn`, log
in `/tmp/ollama.log`): `ollama serve` is started on every boot and init
restarts it if it dies, so `killall ollama` only restarts it. Init reads its
services at boot, so the first time is on the next boot; the script prints how
to start it right away. `install-ollama-cuda --service` only rewrites the
service, without downloading anything; `OLLAMA_SERVICE=0` does not write it; to
remove it, delete the file.

The official binary is **glibc** (not musl). It uses the same Ubuntu runtime as
`libcuda` (`/lib64/ld-linux-x86-64.so.2` + `libresolv` in
`/usr/lib/x86_64-linux-gnu`). The script reinstalls those pieces if they are
missing.

Do **not** use Alpine's `gcompat`: it replaces the glibc loader and fails with
`Error relocating …: fcntl64: symbol not found`. If the loader ended up pointing
at gcompat:

```sh
install-ollama-cuda   # restores loader + libresolv and re-checks ollama --version
```

Optional build with the overlay already included: `ECLIPSE_OLLAMA_CUDA=1 cargo image …`
(cache in `ignored/ollama-cache`). Without the `cuda_v*` runners, `ollama ps`
stays at **100% CPU** even when `cuda-probe` passes.

## Code

- [`linux-object/src/fs/devfs/nvidia.rs`](../linux-object/src/fs/devfs/nvidia.rs)
- [`linux-object/src/fs/devfs/nvidia_uvm.rs`](../linux-object/src/fs/devfs/nvidia_uvm.rs)
- [`tools/cuda-probe/`](../tools/cuda-probe/)
- [`xtask/src/linux/nvidia_userspace.rs`](../xtask/src/linux/nvidia_userspace.rs)
- Hatch: [`zCore/src/main.rs`](../zCore/src/main.rs) (`nvidia.cuda`)
