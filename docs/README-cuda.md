# CUDA en Eclipse OS

**Español** · [English](README-cuda.en.md)

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
- `libnvcuvid` (NVDEC), `libnvidia-opticalflow`, `libnvidia-encode` (NVENC)
- glibc Ubuntu 26.10 + `/lib64/ld-linux-x86-64.so.2` (libcuda es glibc)
- `/usr/bin/cuda-probe` — `cuInit` → devices → context → alloc/memset → **launch PTX**
- `/usr/bin/cuda-status` — hatch + nodos + libcuda (`-p` lanza el probe)
- `/usr/bin/nvcodec-probe` — NVENC codifica y NVDEC decodifica unos frames en cada GPU (`-g N` para una)

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

**Memoria managed (`cudaMallocManaged`, `cuMemAllocManaged`):** el mmap de la
UVM crea páginas de sistema fijadas (pinned) que la CPU y cada GPU con GPU VA
space mapean en la misma dirección (memoria del RM con OS descriptor, como
`cuMemHostRegister`), también en las GPU que registran su VA space después.
La GPU las lee por PCIe (snooped), sin copias ni faults. `cudaMemAdvise`
(preferred location, accessed-by, read duplication), range groups y
system-wide atomics se validan como en uvm.c y se guardan; `cudaMemPrefetchAsync`
(`UVM_MIGRATE`) se completa al momento y libera su semáforo. El rango se libera
cuando la CPU lo desmapea (en la siguiente llamada a la UVM).

Límites frente a Linux: la memoria nunca migra a VRAM (velocidad de PCIe desde
la GPU), se reserva y fija entera en el mmap (no bajo demanda) y no hay
oversubscription.

**P2P entre GPUs:** `cuDeviceCanAccessPeer` es la respuesta del RM
(`p2pGetCapsStatus`), y la primera vez que dos GPUs se registran en la UVM el
log lo dice con el motivo (`[nvidia-uvm] P2P ... no (reads NOT_SUPPORTED...)`).
`UVM_ENABLE_PEER_ACCESS`/`DISABLE` se sirven como en Linux: solo para una pareja
que el RM deja ir por PCIe. En GeForce el RM suele decir que no también en
Linux; `nvidia.registry=ForceP2P=0x11` (como `NVreg_RegistryDwords`) le pide
que lo intente. Sin P2P, `cuMemcpyPeer` copia pasando por la memoria del
sistema. `cuda-probe -d` prueba las dos cosas (paso `peer`).

**Vídeo (NVDEC/NVENC):** la imagen trae `libnvcuvid` (NVDEC, con
`libnvidia-opticalflow`) y `libnvidia-encode` (NVENC), de los paquetes
`libnvidia-decode` y `libnvidia-encode` de la misma versión que el RM. Como
libcuda, hablan con el RM por `/dev/nvidiactl` y no necesitan nada más del
kernel: los motores NVDEC/NVENC los reparte GSP-RM. `nvcodec-probe` lo
comprueba en cada GPU: codifica 16 frames con NVENC (H.264 y HEVC), los
decodifica con NVDEC y compara cada imagen con la original (PSNR ≥ 30 dB).
Es lo que usan `ffmpeg -hwaccel cuda` y `-c:v h264_nvenc`. VDPAU
(`libvdpau_nvidia`) no se incluye porque necesita X11.

**Aún no:** page tables UVM propias / GPU page faults / HMM; el resto de ioctls
UVM → `ENOSYS` (una línea en dmesg la primera vez).

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

`cuda-probe -d` va más allá, en cada GPU: lo que el runner de Ollama
(llama.cpp) le pide, con la respuesta comprobada exacta y lo que cuesta. Copias
de 64 MiB (memoria normal, `cuMemAllocHost`, `cuMemHostRegister` en un stream y
un fichero mapeado como llama.cpp mapea el modelo) con sus MB/s; el pool de
llama.cpp (una reserva de 32 GiB con memoria mapeada a trozos por
`cuMemCreate`/`cuMemMap`); kernels de muchos bloques, reducciones con shuffles
y memoria compartida, 64 KiB de compartida dinámica, memoria local, atómicos,
`dp4a`, tensor cores int8 y f16, un CUDA graph; y la latencia de lanzar un
kernel y de esperarlo, dando vueltas y durmiendo (`CU_EVENT_BLOCKING_SYNC`, que
para despertar necesita la interrupción de la GPU). Una respuesta mal dice
dónde; una GPU que no termina se nombra a los 60 s en vez de colgar. Los
kernels están en `tools/cuda-probe/kernels.cu` y viajan como PTX en
`kernels_ptx.h` (`gen-kernels.sh` lo regenera). En una 2060 SUPER con Linux,
como referencia: copias de 6 a 12 GB/s, lanzar unos 3-5 µs y lanzar y esperar
unos 10-20 µs.

## Ollama (opcional)

**No** va en la imagen por defecto. Quien lo quiera:

```sh
apt-get install -y curl zstd ca-certificates   # si aún no están
install-ollama-cuda                 # → /usr/local (oficial .tar.zst + cuda_v*)
cuda-status
```

El script deja además el servicio de eclipse-init
`/etc/eclipse/services/ollama.service` (`eclipse-ollama serve`, `respawn`, log
en `/tmp/ollama.log`): `ollama serve` se lanza en cada arranque y el init lo
relanza si muere, así que `killall ollama` solo lo reinicia. El init lee sus
servicios al arrancar, de modo que la primera vez es en el siguiente arranque;
el script imprime cómo lanzarlo ya. `install-ollama-cuda --service` solo
reescribe el servicio, sin descargar nada; `OLLAMA_SERVICE=0` no lo escribe;
para quitarlo, borrar el fichero.

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
