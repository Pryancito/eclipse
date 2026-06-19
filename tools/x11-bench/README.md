# X11 kernel-interaction bench

A QEMU smoke test for the kernel surface an X server (`startx`) uses on Eclipse
OS (zCore). It boots zCore via UEFI with a tiny static C init (`xtest.c`) that
drives, in one real boot:

- **Framebuffer** — `open("/dev/fb0")`, `FBIOGET_VSCREENINFO`, `mmap`, draw.
- **Input (evdev)** — `open("/dev/input/event0")`, `EVIOCGNAME`, `EVIOCGBIT(0)`.
- **TTY** — `TIOCGWINSZ` on a pipe (must succeed, not `ENOTTY`).
- **AF_UNIX** — `bind`/`listen`/`connect`, a client write *before* `accept`,
  then `accept`/`read` (the X11 connection-setup handshake).

## Run

```sh
tools/x11-bench/run.sh
```

Prints the `XTEST:` lines and `BENCH OK` on success. Expected output:

```
XTEST: open /dev/fb0 = 3
XTEST: FBIOGET_VSCREENINFO r=0 1024x768 bpp=32
XTEST: mmap fb = 0x...
XTEST: fb write ok
XTEST: open event0 = 4
XTEST: EVIOCGNAME r=0 name='ps2-input'
XTEST: EVIOCGBIT(0) r=0 bits=0x7
XTEST: TIOCGWINSZ pipe r=0 113x42
XTEST: bind=0 listen=0
XTEST: connect=0 write=4 (before accept)
XTEST: accept=... server read n=4 'PING'
XTEST: done
```

## Requirements

- `qemu-system-x86_64`
- the `x86_64-linux-musl-cross` toolchain under `target/x86_64/`
  (downloaded by `cargo rootfs`)
- `rboot/OVMF.fd`

The complementary host-side unit tests for the AF_UNIX handshake live in
`linux-object/src/net/unix.rs` (`cargo test -p linux-object net::unix`).
