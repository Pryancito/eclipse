# CUDA en Eclipse OS

Eclipse publica los mismos dispositivos de carácter que `nvidia.ko` en Linux
para que un `libcuda.so` de la misma versión que el RM hable con el Resource
Manager enlazado en el núcleo. El hatch de arranque es **`nvidia.cuda`**.

## Arranque

En la cmdline de `rboot.conf` (ESP):

```
nvidia.cuda
```

Equivale a encender `nvidia.nvidiactl` y `nvidia.uvm`. Se puede apagar solo
UVM con `nvidia.cuda:nvidia.uvm=0`. La imagen de producto
(`zCore/rboot.conf`) lo lleva por defecto; quítalo (`nvidia.cuda=0`) en
máquinas sin GPU NVIDIA de cómputo.

## Nodos

| Nodo | Major | Rol |
|------|-------|-----|
| `/dev/nvidiactl` | 195:255 | Control RM: versión, CARD_INFO, Alloc/Free/Control, MAP, eventos |
| `/dev/nvidiaN` | 195:N | Una por GPU con `attach_gpu` OK |
| `/dev/nvidia-uvm` | 510 | UVM: `cuInit` + `cuCtxCreate` (resto → `ENOSYS` + log) |

Versión fijada: **580.178.04** (`open-gpu-kernel-modules` + `gsp.bin`). Tiene
que coincidir con el `libcuda.so` del rootfs.

## Userspace en el ISO

`cargo image` instala (salvo `ECLIPSE_NVIDIA_USERSPACE=0`):

- `libcuda.so.1`, `libnvidia-ptxjitcompiler`, `libnvidia-ml`, `nvidia-smi`
- glibc Ubuntu 26.10 + `/lib64/ld-linux-x86-64.so.2` (libcuda es glibc)
- `/usr/bin/cuda-probe` — `cuInit` → devices → context → alloc/memset → **launch PTX**
- `/usr/bin/cuda-status` — hatch + nodos + libcuda (`-p` lanza el probe)

No hace falta cuBLAS del sistema: el runner de Ollama trae sus libs CUDA y
habla con este `libcuda` (driver API).

## Qué cubre el kernel hoy

**nvidiactl:** `CHECK_VERSION_STR`, `CARD_INFO`, `SYS_PARAMS`, `REGISTER_FD`,
OS events, `RM_ALLOC`/`FREE`/`CONTROL`/`DUP`/`SHARE`, `RM_MAP_MEMORY` + mmap,
`RM_UNMAP_MEMORY`, `RM_VID_HEAP_CONTROL` (VRAM de `cuMemAlloc`),
`RM_ALLOC_MEMORY` de OS descriptor, etc.

**nvidia-uvm (servidos):** `INITIALIZE`, `MM_INITIALIZE`, `PAGEABLE_MEM_ACCESS`,
`REGISTER`/`UNREGISTER_GPU`, range groups, GPU VA space, channels, external
map/unmap/free, semaphore pool (+ mmap CPU), SKED reflected, `VALIDATE_VA_RANGE`.

**Aún no:** page tables UVM propias / GPU page faults / HMM;
`cudaMallocManaged` (mmap managed → `ENODEV`); el resto de ioctls UVM →
`ENOSYS` (una línea en dmesg la primera vez).

## Prueba en la máquina

```sh
cuda-status          # hatch + /dev/nvidia* + libcuda
cuda-status -p       # lo anterior y cuda-probe completo
# o:
cuda-probe
nvidia-smi -L
dmesg | grep -E 'nvidia-uvm|nvidiactl|mmap'
```

Con `BOOTTRACE=cuda-probe` en cmdline, `/proc/bootprofile` lista syscalls
fallidas del probe (p.ej. detrás de `CUDA_ERROR_OPERATING_SYSTEM` 304).

Salida esperada al final: `OK: CUDA works on GPU 0 (init, memory, launch)`.

## Ollama (opcional)

**No** va en la imagen por defecto. Quien lo quiera:

```sh
apt-get install -y curl zstd ca-certificates   # si aún no están
install-ollama-cuda                 # → /usr/local (oficial .tar.zst + cuda_v*)
cuda-status
OLLAMA_DEBUG=1 /usr/local/bin/ollama serve
```

El binario oficial es **glibc** (no musl). Usa el mismo runtime Ubuntu que
`libcuda` (`/lib64/ld-linux-x86-64.so.2` + `libresolv` en
`/usr/lib/x86_64-linux-gnu`). El script reinstala esas piezas si faltan.

**No** uses Alpine `gcompat`: sustituye el loader glibc y falla con
`Error relocating …: fcntl64: symbol not found`. Si el loader quedó
apuntando a gcompat:

```sh
install-ollama-cuda   # restaura loader + libresolv y re-verifica ollama --version
```

Build opcional con el overlay ya metido: `ECLIPSE_OLLAMA_CUDA=1 cargo image …`
(cache en `ignored/ollama-cache`). Sin runners `cuda_v*`, `ollama ps` queda en
**100% CPU** aunque `cuda-probe` pase.

## Código

- [`linux-object/src/fs/devfs/nvidia.rs`](../linux-object/src/fs/devfs/nvidia.rs)
- [`linux-object/src/fs/devfs/nvidia_uvm.rs`](../linux-object/src/fs/devfs/nvidia_uvm.rs)
- [`tools/cuda-probe/`](../tools/cuda-probe/)
- [`xtask/src/linux/nvidia_userspace.rs`](../xtask/src/linux/nvidia_userspace.rs)
- Hatch: [`zCore/src/main.rs`](../zCore/src/main.rs) (`nvidia.cuda`)
