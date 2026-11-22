# Eclipse OS in the browser

[Español](README.md) · **English**

The page [eclipse-os.dev/en/live/](https://eclipse-os.dev/en/live/) boots the
real Eclipse kernel inside the browser. It is not Linux and not a recording: it
is `zcore.bin` for riscv64 with its `eclipse-init`, on QEMU's `virt` machine,
and that QEMU is compiled to WebAssembly.

This directory builds the files that page downloads (`live/emu/` in the website
repository, `Pryancito/Pryancito.github.io`).

## How it works

| Piece | What it is | Size |
| --- | --- | --- |
| `qemu-system-riscv64.wasm` + `.js` + `.worker.js` | [qemu-wasm](https://github.com/ktock/qemu-wasm) (QEMU 8.2 with a TCG backend that emits WebAssembly, so it is a JIT, not an interpreter), built with Emscripten | 12 MB (3 MB gzipped) |
| `opensbi.bin` | The OpenSBI firmware that QEMU ships (`-bios`) | 135 KB |
| `zcore.bin.gz` | The Eclipse kernel, riscv64, `LINUX=1` | 3 MB |
| `initrd.img.gz` | The minimal variant's rootfs, trimmed (see below), as SFS | 8 MB |
| `manifest.json` | What `live/live.js` downloads and QEMU's arguments | |

The page downloads everything, decompresses it with `DecompressionStream`, puts
it in Emscripten's in-memory file system and starts QEMU with:

```
-machine virt -cpu rv64 -m 512M -smp 1 -nographic -nic none
-bios opensbi.bin -kernel zcore.bin -initrd initrd.img
-append LOG=off:TERM=xterm-256color:console.shell=true:virtcon.disable=true:desktop=none:init.quiet
```

The serial console goes to an [xterm.js](https://xtermjs.org/) terminal through
[xterm-pty](https://github.com/mame/xterm-pty). On an ordinary laptop the shell
prompt shows up in about 20-25 seconds.

With networking, `live/live.js` swaps `-nic none` for
`-netdev socket,id=net0,connect=eclipse-net:8888 -device virtio-net-device,netdev=net0`
(see "The network" below).

`init.quiet` is an `eclipse-init` option: its `[eclipse-init] ...` lines, and
what the keyboard, language and timezone scripts print, go to
`/run/eclipse-init.log` instead of the console, so they do not land in the
middle of what the visitor is typing.

### What is removed from the rootfs

The minimal variant is 114 MB, nearly all of it the graphical apps. For the web
`lunar*`, `apk`, `wavplay`, the `*-probe`s, `ecl-compute`, `ecl-vkcompute` and
the benchmarks are removed, and two services are turned off:

- `mountd`: on riscv64 it dies at start with `fifo_create: -38` (ENOSYS) and
  `eclipse-init` would restart it forever. That is a real riscv64 bug worth a
  separate look.
- `boot-sound`: there is no sound card.

What is left is 15 MB (8 MB gzipped).

### The network

Under Emscripten the TCP socket of `-netdev socket` is a WebSocket.
`live/net.js` puts a fake one in its place that never leaves the browser: it
takes QEMU's Ethernet frames and hands them to [v86](https://github.com/copy/v86)'s
TCP/IP stack (`fake_network.js`), which answers DHCP (Eclipse gets
192.168.86.100), ARP, ping and NTP, and resolves DNS over DoH. Each TCP
connection Eclipse opens goes out as a
[Wisp](https://github.com/MercuryWorkshop/wisp-protocol) stream over
`wss://…/wisp/` to a server that opens the real connection. That server, its
nginx and what it allows (only ports 80 and 443, no private IPs) are in
`deploy/wisp/` of the website repository.

Inside Eclipse the card is driven by `eclipse-netstack` (`unet.service`), which
asks for its address over DHCP about half a minute after the shell prompt.
With `?net=0` in the URL the machine boots without a network.

## Building it

```sh
scripts/live-web/build.sh ../Pryancito.github.io/live/emu
```

It needs docker, the repository's Rust toolchain, `cargo-binutils` and curl.
The first run takes about 30 minutes, mostly building glib and pixman with
Emscripten (in `scripts/live-web/Dockerfile`, which is qemu-wasm's own with the
sources downloaded and checked by `build.sh`). Intermediate files go to
`ignored/live-web/`.

For a new Eclipse version only the kernel and the rootfs need rebuilding, with
no docker or QEMU (the ones in `live/emu/` stay as they are):

```sh
git pull
scripts/live-web/build.sh --solo-eclipse ../Pryancito.github.io/live/emu
```

It leaves `zcore.bin.gz`, `initrd.img.gz` and `manifest.json` (with the new
version and commit) in `live/emu/`. It takes a few minutes.

If the build goes through a proxy that needs its own certificate, another
Dockerfile can be given with `LIVE_WEB_DOCKERFILE=...` and extra `docker build`
arguments with `LIVE_WEB_DOCKER_ARGS=...`.

## The server

The emulator uses threads (`SharedArrayBuffer`), which browsers only give to a
cross-origin isolated page. `/live/` and `/en/live/` need these headers:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: credentialless
```

The website repository has the nginx configuration (`deploy/nginx-live.conf`)
and an `.htaccess` for Apache.

## Limits

- It works in recent Chrome, Edge and Firefox. Safari has no
  `COEP: credentialless`, so there the page says it cannot run.
- The network is outgoing TCP only (no UDP), to the ports the Wisp server
  allows. No desktop and no persistent disk: everything is lost on reload.
- It is riscv64, not x86_64: a whole PC (UEFI, q35, 4 GB) in WebAssembly would
  be too slow and too big to download.
