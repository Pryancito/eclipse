# Eclipse OS en el navegador

**Español** · [English](README.en.md)

La página [eclipse-os.dev/live/](https://eclipse-os.dev/live/) arranca el núcleo
de verdad de Eclipse dentro del navegador. No es Linux ni una grabación: es
`zcore.bin` para riscv64 con su `eclipse-init`, en la máquina `virt` de QEMU, y
ese QEMU está compilado a WebAssembly.

Este directorio construye los ficheros que esa página descarga
(`live/emu/` del repositorio de la web, `Pryancito/Pryancito.github.io`).

## Cómo funciona

| Pieza | Qué es | Tamaño |
| --- | --- | --- |
| `qemu-system-riscv64.wasm` + `.js` + `.worker.js` | [qemu-wasm](https://github.com/ktock/qemu-wasm) (QEMU 8.2 con un backend TCG que genera WebAssembly, así que hay JIT y no un intérprete), compilado con Emscripten | 12 MB (3 MB con gzip) |
| `opensbi.bin` | El firmware OpenSBI que trae ese QEMU (`-bios`) | 135 KB |
| `zcore.bin.gz` | El núcleo de Eclipse, riscv64, `LINUX=1` | 3 MB |
| `initrd.img.gz` | El rootfs de la variante minimal, recortado (ver abajo), como SFS | 8 MB |
| `manifest.json` | Lo que `live/live.js` descarga y los argumentos de QEMU | |

La página descarga todo, lo descomprime con `DecompressionStream`, lo deja en
el sistema de ficheros en memoria de Emscripten y arranca QEMU con:

```
-machine virt -cpu rv64 -m 512M -smp 1 -nographic -nic none
-bios opensbi.bin -kernel zcore.bin -initrd initrd.img
-append LOG=off:TERM=xterm-256color:console.shell=true:virtcon.disable=true:desktop=none:init.quiet
```

La consola serie va a una terminal [xterm.js](https://xtermjs.org/) a través de
[xterm-pty](https://github.com/mame/xterm-pty). En un portátil normal el
intérprete de órdenes sale en unos 20-25 segundos.

Con red, `live/live.js` cambia `-nic none` por
`-netdev socket,id=net0,connect=eclipse-net:8888 -device virtio-net-device,netdev=net0`
(ver «La red», abajo).

`init.quiet` es una opción de `eclipse-init`: sus líneas `[eclipse-init] ...` y
lo que imprimen los scripts de teclado, idioma y zona horaria van a
`/run/eclipse-init.log` en vez de a la consola, para que no caigan en medio de
lo que escribe quien prueba la demo.

### Qué se quita del rootfs

La variante minimal pesa 114 MB, casi todo en las apps gráficas. Para la web se
quitan `lunar*`, `apk`, `wavplay`, los `*-probe`, `ecl-compute`,
`ecl-vkcompute` y los benchmarks, y se desactivan dos servicios:

- `mountd`: en riscv64 muere al arrancar con `fifo_create: -38` (ENOSYS) y
  `eclipse-init` lo relanzaría sin parar. Es un fallo real de riscv64 que
  conviene mirar aparte.
- `boot-sound`: no hay tarjeta de sonido.

Queda en 15 MB (8 MB con gzip).

### La red

En Emscripten el socket TCP de `-netdev socket` es un WebSocket. `live/net.js`
pone en su lugar uno falso que no sale del navegador: recibe las tramas
Ethernet de QEMU y se las da a la pila TCP/IP de
[v86](https://github.com/copy/v86) (`fake_network.js`), que contesta DHCP
(Eclipse recibe 192.168.86.100), ARP, ping y NTP, y resuelve DNS por DoH. Cada
conexión TCP de Eclipse sale como un stream de
[Wisp](https://github.com/MercuryWorkshop/wisp-protocol) por `wss://…/wisp/`
hasta un servidor que abre la conexión de verdad. Ese servidor, su nginx y lo
que permite (solo los puertos 80 y 443, ninguna IP privada) están en
`deploy/wisp/` del repositorio de la web.

Dentro de Eclipse la tarjeta la lleva `eclipse-netstack` (`unet.service`), y
pide su dirección por DHCP medio minuto después de que salga el intérprete. Con
`?net=0` en la URL la máquina arranca sin red.

## Construirlo

```sh
scripts/live-web/build.sh ../Pryancito.github.io/live/emu
```

Necesita docker, la toolchain de Rust del repositorio, `cargo-binutils` y curl.
La primera vez tarda unos 30 minutos, casi todo en compilar glib y pixman con
Emscripten (en `scripts/live-web/Dockerfile`, que es el de qemu-wasm con las
fuentes descargadas y comprobadas por `build.sh`). Lo intermedio queda en
`ignored/live-web/`.

Para una versión nueva de Eclipse basta con rehacer el núcleo y el rootfs, sin
docker ni QEMU (los de `live/emu/` se quedan como están):

```sh
git pull
scripts/live-web/build.sh --solo-eclipse ../Pryancito.github.io/live/emu
```

Deja `zcore.bin.gz`, `initrd.img.gz` y `manifest.json` (con la versión y el
commit nuevos) en `live/emu/`. Tarda unos minutos.

Si el build pasa por un proxy que necesita su propio certificado, se puede dar
otro Dockerfile con `LIVE_WEB_DOCKERFILE=...` y argumentos de `docker build`
con `LIVE_WEB_DOCKER_ARGS=...`.

## El servidor

El emulador usa hilos (`SharedArrayBuffer`), y el navegador solo los da a una
página aislada entre orígenes. `/live/` y `/en/live/` necesitan estas cabeceras:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: credentialless
```

El repositorio de la web trae la configuración para nginx
(`deploy/nginx-live.conf`) y un `.htaccess` para Apache.

## Límites

- Funciona en Chrome, Edge y Firefox recientes. Safari no tiene
  `COEP: credentialless`, así que allí la página dice que no puede.
- La red es solo TCP saliente (UDP no sale) y solo a los puertos que deje el
  servidor Wisp. Sin escritorio y sin disco persistente: todo se pierde al
  recargar.
- Es riscv64, no x86_64: un PC completo (UEFI, q35, 4 GB) en WebAssembly sería
  demasiado lento y demasiado grande para descargarlo.
