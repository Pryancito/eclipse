# Uso de la GPU NVIDIA en Eclipse OS

## Estado actual

En hardware real con GPU NVIDIA (sin VirtIO, sin EFI GOP válido), Eclipse ya **usa la GPU NVIDIA** para la salida de pantalla:

1. **Kernel** (`eclipse_kernel/src/nvidia.rs`): detecta la GPU por PCI, mapea BAR0 (registros) y guarda **BAR1** (apertura lineal de VRAM) como framebuffer. `sys_get_framebuffer_info` y `sys_map_framebuffer` devuelven ese framebuffer.
2. **Compositor (smithay_app)**: si no hay VirtIO GPU, llama a `map_framebuffer()` y obtiene el BAR1 mapeado en su espacio de direcciones. Dibuja en un back buffer en RAM y en `present()` copia a ese framebuffer (BAR1) + `sfence`. La imagen sale por la NVIDIA.

Por tanto, **la pantalla ya va por la NVIDIA** cuando corres en metal con una NVIDIA.

## Backend NVIDIA explícito

- **smithay_app** asigna `GpuDevice::for_backend(GpuBackend::Nvidia)` cuando usa el camino de framebuffer lineal (BAR1/GOP), así cualquier código que consulte el backend ve `Nvidia`.
- Mensaje en log: `[SMITHAY] Using linear framebuffer (NVIDIA BAR1 or GOP)`.

## Aceleración 2D (fill_rect)

El kernel implementa un comando 2D para NVIDIA vía `sys_gpu_command(1, 0, payload)`:

- **kind = 1**: backend NVIDIA  
- **command = 0**: fill rect  
- **payload**: 20 bytes, little-endian: `x (u32), y (u32), w (u32), h (u32), color (u32)` (32bpp ARGB).

El kernel mapea BAR1 en espacio kernel (una vez), escribe el rectángulo en el framebuffer y devuelve 0 en caso de éxito.

En **userspace** puedes usar:

```rust
use sidewind_sdk::gpu::{GpuDevice, GpuBackend, GpuCommandEncoder};

let gpu = GpuDevice::for_backend(GpuBackend::Nvidia);
let mut enc = GpuCommandEncoder::new(&gpu);
enc.fill_rect(100, 100, 200, 150, 0xFF_00_00_FF); // x, y, w, h, color ARGB
```

Eso llama a `gpu_command(1, 0, &payload)` y el kernel rellena el rectángulo en BAR1.

## GPUs soportadas

Turing y posteriores (GSP): RTX 20, 30, 40, 50, serie H, etc. Ver tabla de device IDs en `eclipse_kernel/src/nvidia.rs`.

## Blit 2D (copia de rectángulo)

- **Comando**: `sys_gpu_command(1, 1, payload)` con 24 bytes: `src_x, src_y, dst_x, dst_y, w, h` (u32 LE).
- El kernel copia el rectángulo dentro del mismo BAR1; si origen y destino se solapan, copia en orden inverso (por filas o por píxeles en la misma fila) para no corromper datos.
- En userspace: `GpuCommandEncoder::blit(src_x, src_y, dst_x, dst_y, w, h)` con backend Nvidia.

## Próximos pasos posibles

- Más comandos 2D (copy desde CPU, formatos) en `sys_gpu_command(1, ...)`.
- Usar el motor 2D de la GPU (si se expone vía GSP/registros) en lugar de rellenar por CPU en el kernel.
- GSP: el kernel ya puede cargar firmware GSP y arrancar Falcon; la infraestructura RPC está para futuras órdenes de display/compute.

Para la matriz completa de **cálculos 2D/3D y aceleración por GPU** (VirtIO, NVIDIA, software), ver **[GPU_2D_3D_ACCEL.md](GPU_2D_3D_ACCEL.md)**.
