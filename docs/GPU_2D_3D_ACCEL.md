# Cálculos 2D/3D y aceleración por GPU en Eclipse OS

Resumen de qué está acelerado por GPU y qué corre en CPU.

---

## Matriz de aceleración

| Operación | VirtIO-GPU | NVIDIA | Software (CPU) |
|-----------|------------|--------|----------------|
| **Display (present)** | ✅ `gpu_present` | ✅ BAR1 copy / fill_rect/blit destino | ✅ memcpy a GOP/BAR1 |
| **2D fill_rect** | ⚠ Placeholder (VirGL) | ✅ Kernel escribe en BAR1 | ✅ sidewind_opengl / manual |
| **2D blit (copia rect)** | ⚠ Placeholder | ✅ Kernel copia en BAR1 | ✅ memcpy / manual |
| **3D (triángulos, shaders)** | ✅ VirGL (Mesa) | 🔲 GSP/OpenGL (futuro) | ✅ sidewind_opengl (CPU) |

Leyenda: ✅ disponible, ⚠ parcial/placeholder, 🔲 planeado.

---

## 2D acelerado (NVIDIA)

- **fill_rect(x, y, w, h, color)**  
  `sys_gpu_command(1, 0, payload)`. El kernel rellena el rectángulo en el framebuffer BAR1.

- **blit(src_x, src_y, dst_x, dst_y, w, h)**  
  `sys_gpu_command(1, 1, payload)`. Copia un rectángulo dentro del mismo framebuffer; el kernel maneja regiones solapadas (misma fila o filas hacia abajo).

Uso desde Rust (p. ej. en el compositor):

```rust
use sidewind_sdk::gpu::{GpuDevice, GpuBackend, GpuCommandEncoder};

let gpu = GpuDevice::for_backend(GpuBackend::Nvidia);
let mut enc = GpuCommandEncoder::new(&gpu);
enc.fill_rect(0, 0, 1920, 1080, 0xFF_1a_1a_2e);  // Fondo
enc.blit(0, 0, 100, 100, 200, 150);               // Copiar región
```

---

## 2D (VirtIO-GPU)

- **Present**: `gpu_alloc_display_buffer` + `gpu_present(resource_id, …)` — sí acelerado (2D VirtIO).
- **fill_rect/blit**: el encoder usa `submit(0, …)`; el kernel lo trata como VirGL submit 3D. Para 2D puro habría que usar órdenes VirtIO 2D (transfer to host, etc.) o un flujo VirGL 2D.

---

## 3D

### VirtIO-GPU + VirGL (3D real)

- Syscalls: `virgl_ctx_create`, `virgl_ctx_destroy`, `virgl_submit_3d`, `virgl_ctx_attach_resource`, `virgl_alloc_backing`, `virgl_resource_attach_backing`, etc.
- El kernel envía el stream de comandos VirGL al dispositivo; la GPU del host (Mesa/virgl) hace el render 3D.
- Userspace: falta una lib que construya el stream VirGL (Gallium) y llame a estos syscalls. Referencia: [virglrenderer](https://gitlab.freedesktop.org/virgl/virglrenderer).

### NVIDIA 3D (futuro)

- **sidewind_nvidia**: GSP RPC, `OpenGLContextCreate`, `OpenGLSurfaceMap`, etc. El kernel ya carga el firmware GSP y tiene la cola RPC.
- 3D por GPU NVIDIA requeriría orquestar GSP (o un motor 2D/3D por registros) y exponerlo vía nuevos comandos en `sys_gpu_command(1, …)` o syscalls dedicados.

### Software 3D (CPU)

- **sidewind_opengl**: rasterizador OpenGL-like en CPU (triángulos, depth, texturas, pipelines). Escribe en un framebuffer (mapeado o BAR1). No usa la GPU para cálculos.

---

## Resumen de rutas recomendadas

| Objetivo | Ruta |
|----------|------|
| **2D en NVIDIA** | `GpuDevice::for_backend(Nvidia)` + `fill_rect` / `blit` (kernel BAR1). |
| **2D en QEMU/VirtIO** | `gpu_alloc_display_buffer` + `gpu_present`; fill/blit por CPU al buffer o futuro 2D VirtIO. |
| **3D en QEMU** | VirGL: implementar cliente que construya comandos Gallium y use los syscalls `virgl_*`. |
| **3D en NVIDIA** | A futuro: GSP/OpenGL o motor 2D/3D vía GSP RPC. |
| **3D sin GPU** | `sidewind_opengl::GlContext` sobre cualquier framebuffer. |

---

## Archivos clave

- **Kernel 2D NVIDIA**: `eclipse_kernel/src/nvidia.rs` (`fill_rect`, `blit_rect`), `syscalls.rs` (`sys_gpu_command` kind 1).
- **API userspace 2D**: `eclipse-apps/sidewind_sdk/src/gpu.rs` (`GpuDevice`, `GpuCommandEncoder`).
- **Kernel 3D VirtIO**: `eclipse_kernel/src/virtio.rs` (VirGL), `syscalls.rs` (syscalls 42–48).
- **OpenGL software**: `eclipse-apps/sidewind_opengl/`.
- **NVIDIA GSP/protocolo**: `eclipse-apps/sidewind_nvidia/`, `eclipse_kernel/src/nvidia.rs` (GSP loader, RPC).
