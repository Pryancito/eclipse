# rBoot

[English](README.md) · **Español**

El cargador UEFI de x86_64 de Eclipse OS (heredado de rCore / zCore).

## Compilar

```sh
cargo build --release --target x86_64-unknown-uefi
```

El binario EFI queda en `target/x86_64-unknown-uefi/release/rboot.efi`.

## Ejemplo

Mira [`example-kernel/`](example-kernel/), un kernel bare-metal mínimo que
arranca con rboot e imprime por el puerto serie.

Ejecuta `example-kernel/test.sh` para compilarlo y probarlo en QEMU.

## Configuración

Edita `rboot.conf` para configurar el cargador. Hay un ejemplo que funciona en
[`example-kernel/rboot.conf`](example-kernel/rboot.conf). Opciones:

- `kernel_path` - ruta al binario ELF del kernel
- `kernel_stack_address` - dirección virtual de la pila del kernel
- `kernel_stack_size` - tamaño de la pila del kernel en páginas de 4 KiB
- `physical_memory_offset` - desplazamiento virtual del mapeo de la memoria física
- `resolution` - resolución de la salida gráfica (p. ej. `1024x768`)
- `initramfs` - ruta a la imagen del disco RAM inicial
- `cmdline` - línea de comandos del kernel
- `measure` - qué resumir para el arranque medido: `kernel` (por defecto), `all` u `off` (ver la [integridad del arranque de hunter](../docs/hunter-security.es.md#integridad-del-arranque-arranque-medido))
