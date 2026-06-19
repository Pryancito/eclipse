# X11 kernel-interaction bench

A QEMU smoke test for the kernel surface an X server (`startx`) uses on Eclipse
OS (zCore). It boots zCore via UEFI with a tiny static C init (`xtest.c`) that
drives, in one real boot, every primitive Xorg relies on:

- **Framebuffer** — `open("/dev/fb0")`, `FBIOGET_VSCREENINFO`, `mmap`, draw.
- **Input (evdev)** — `open("/dev/input/event0")`, `EVIOCGNAME`, `EVIOCGBIT`.
- **TTY** — `TIOCGWINSZ` on a pipe (must succeed, not `ENOTTY`).
- **AF_UNIX** — filesystem *and* abstract (`\0/tmp/.X11-unix/Xn`, Xlib's primary
  transport): `bind`/`listen`/`connect`, a client write *before* `accept`, then
  `accept`/`read` (the X11 connection-setup handshake).
- **Event loop** — `select`, `epoll`, `eventfd` reporting a ready socket.
- **Scheduler** — `SIGALRM` via `setitimer`.
- **Input thread** — `pthread_create` + `join`.
- **`socketpair`**, and **xauth's** write-temp + atomic `rename`.

## Run

```sh
tools/x11-bench/run.sh
```

Prints one `XTEST: [PASS|FAIL] <name>` line per check and `BENCH OK` when all
pass. Expected:

```
XTEST: [PASS] fb screeninfo
XTEST: [PASS] fb mmap+draw
XTEST: [PASS] evdev EVIOCGNAME ps2-input
XTEST: [PASS] evdev EVIOCGBIT
XTEST: [PASS] TIOCGWINSZ on pipe
XTEST: [PASS] unix fs socket
XTEST: [PASS] unix abstract socket
XTEST: [PASS] select
XTEST: [PASS] epoll
XTEST: [PASS] eventfd
XTEST: [PASS] SIGALRM/setitimer
XTEST: [PASS] pthread
XTEST: [PASS] socketpair
XTEST: [PASS] xauth write+rename
XTEST: 14/14 passed
XTEST: done
```

## Requirements

- `qemu-system-x86_64`
- the `x86_64-linux-musl-cross` toolchain under `target/x86_64/`
  (downloaded by `cargo rootfs`)
- `rboot/OVMF.fd`

The complementary host-side unit tests for the AF_UNIX handshake live in
`linux-object/src/net/unix.rs` (`cargo test -p linux-object net::unix`).
