# Eclipse OS

[![Repository clones](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FPryancito%2Feclipse%2Fbadges%2Fclones.json)](.github/workflows/clones-badge.yml)
[![Unique clones](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FPryancito%2Feclipse%2Fbadges%2Fclones-uniques.json)](.github/workflows/clones-badge.yml)

[Español](README.md) · **English**

Eclipse OS is a complete operating system written in Rust. Its kernel grew out
of [zCore](https://github.com/rcore-os/zCore) (a reimplementation of Fuchsia's
Zircon microkernel in safe Rust) and adds a **Linux-compatible** personality
good enough to boot a Wayland desktop, web browsers, audio and NVIDIA
acceleration on top of Ubuntu packages.

- **Kernel**: Zircon plus a Linux compatibility layer (`linux-object`,
  `linux-syscall`), and a FreeBSD/amd64 ABI as well.
- **Architectures**: x86_64 (QEMU and real hardware), aarch64 (QEMU) and riscv64
  (QEMU and several boards).
- **Userspace**: its own init (`eclipse-init`), a static busybox base, and
  **Ubuntu 26.10** packages installable with `apt`.
- **Desktop**: a labwc Wayland session with native panel and wallpaper
  (`lunarbar`, `lunarbg`) and several looks; Xorg is available as an
  alternative.
- **Storage**: an **EclipseFS** root, Eclipse's own copy-on-write file
  system; **btrfs** with an in-house driver as an alternative, plus ext2/3/4
  and FAT32.
- **Security**: `hunter`, an in-kernel policy and IDS subsystem.

Technical documentation lives in [`docs/`](docs/README.en.md).

## Contents

- [Quick start](#quick-start)
- [Preparing the host](#preparing-the-host)
- [Building](#building)
  - [xtask commands](#xtask-commands)
  - [Makefile targets](#makefile-targets)
  - [Variants and release ISOs](#variants-and-release-isos)
- [Boot options](#boot-options)
- [Platforms](#platforms)
- [Packages (apt)](#packages-apt)
- [Documentation](#documentation)

## Quick start

```bash
make qemu ARCH=x86_64
```

This builds the rootfs, the image and the kernel, and boots Eclipse in QEMU with
the `labwc` desktop session. For the console variant:

```bash
make qemu VARIANT=minimal
```

Other useful `make qemu` variables: `SMP=4`, `GRAPHIC=off`, `AUDIO=off`,
`DESKTOP=xorg` (an X session instead of labwc) and `KOPTS=a:b` to pass extra
options on the kernel command line.

## Preparing the host

The build host is **Linux x86_64**, preferably **Debian or Ubuntu**: the
desktop rootfs is assembled with the host's own `apt-get download` and
`dpkg-deb` (using an isolated apt state, so the host release does not matter;
Ubuntu 24.04 is used day to day). Install these tools **before** running
`cargo rootfs` / `make qemu`: xtask calls them by name and the build will not
start without them.

### Rust toolchain

[`rust-toolchain.toml`](rust-toolchain.toml) pins `nightly-2026-09-01` with the
`rust-src`, `llvm-tools-preview`, `rustfmt` and `clippy` components. With
[rustup](https://rustup.rs) installed, `cargo` downloads that toolchain by
itself.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
# rootfs userspace (eclipse-init, lunarbar, …):
rustup target add x86_64-unknown-linux-musl
```

`PATH` must include `~/.cargo/bin`.

### System packages

| Program | Used for | Debian/Ubuntu package |
|---|---|---|
| `git` | busybox and submodules | `git` |
| `make`, `gcc` | busybox, rboot, the rootfs C tools | `build-essential` |
| `tar`, `wget` or `curl` | musl cross toolchain, static apk, `cacert.pem` | `tar`, `wget`, `curl` |
| `apt-get`, `dpkg-deb` | Ubuntu packages for the desktop rootfs | `apt`, `dpkg` (preinstalled on Debian/Ubuntu) |
| `sed` | tweaking busybox's `.config` | `sed` |
| `python3` | regenerating SPIR-V headers (optional) | `python3` |
| `mkfs.vfat`, `mmd`/`mcopy` | FAT32 ESP (`cargo image`, `make iso`) | `dosfstools`, `mtools` |
| `gzip` | compressing `efi.img` / rootfs / home | `gzip` |
| `qemu-system-x86_64`, `qemu-img` | `make qemu`, `make qcow2` | `qemu-system-x86`, `qemu-utils` |
| `xorriso` | `make iso` | `xorriso` |

The **musl** cross toolchain (`x86_64-linux-musl-gcc`, `…-strip`) is not
installed by hand: `cargo rootfs` downloads it into
`ignored/target/x86_64/x86_64-linux-musl-cross/`.

```bash
sudo apt update
sudo apt install --no-install-recommends \
  build-essential git make gcc tar wget curl sed python3 \
  dosfstools mtools gzip qemu-system-x86 qemu-utils xorriso
```

The same tools can be installed on Fedora or Arch
(`dnf install gcc make git tar wget curl python3 dosfstools mtools gzip qemu-system-x86 qemu-img xorriso`,
`pacman -S base-devel git wget python dosfstools mtools qemu-system-x86 xorriso`),
but without `apt-get` the desktop rootfs cannot fetch its packages; use
`VARIANT=minimal` or `ECLIPSE_PKG=apk` (the legacy Alpine path).

Check that the host is ready:

```bash
command -v cargo git make gcc tar wget curl qemu-system-x86_64 mkfs.vfat mcopy gzip apt-get dpkg-deb
rustup show
```

`btrfs-progs` is not needed: xtask generates the EclipseFS and btrfs images itself. `cmake`,
OpenCV and FFmpeg are only needed for `cargo opencv` / `cargo ffmpeg`. A
development Docker image is available in [`tools/docker`](tools/docker/README.md).

## Building

The build follows the [xtask pattern](https://github.com/matklad/cargo-xtask):
`cargo <command> [--args]` is shorthand for
`cargo run --package xtask --release -- <command> [--args]`. The
[Makefile](Makefile) wraps the same steps and is what CI uses.

### xtask commands

Many commands prepare what they need first:

```text
┌────────────┐ ┌─────────────┐ ┌─────────────┐
| update-all | | check-style | | zircon-init |
└────────────┘ └─────────────┘ └─────────────┘
┌─────┐ ┌──────┐  ┌─────┐  ┌─────────────┐ ┌─────────────────┐
| asm | | qemu |─→| bin |  | linux-libos | | libos-libc-test |
└─────┘ └──────┘  └─────┘  └─────────────┘ └─────────────────┘
                     |            └───┐┌─────┘   ┌───────────┐
                     ↓                ↓↓      ┌──| libc-test |
                 ┌───────┐        ┌────────┐←─┘  └───────────┘
                 | image |───────→| rootfs |←─┐ ┌────────────┐
                 └───────┘        └────────┘  └─| other-test |
                 ┌────────┐           ↑         └────────────┘
                 | opencv |────→┌───────────┐
                 └────────┘  ┌─→| musl-libc |
                 ┌────────┐  |  └───────────┘
                 | ffmpeg |──┘
                 └────────┘
Legend: A → B (A depends on B; running A runs B first)
```

| Command | What it does | Example |
|---|---|---|
| `update-all` | Updates the toolchain, dependencies and submodules | `cargo update-all` |
| `check-style` | Checks the code compiles under several feature combinations | `cargo check-style` |
| `zircon-init` | Downloads the Zircon-mode prebuilt binaries ([details](prebuilt/README.md)) | `cargo zircon-init` |
| `dump` | Prints the build configuration | `cargo xtask dump` |
| `git-proxy` | Sets or clears a git proxy | `cargo git-proxy --global --port 12345` |
| `rootfs` | Rebuilds the Linux rootfs for one architecture | `cargo rootfs --arch x86_64 [--variant minimal]` |
| `verify-rootfs` | Checks the rootfs holds what boot needs | `cargo verify-rootfs --arch x86_64` |
| `musl-libs`, `opencv`, `ffmpeg` | Copy those libraries into the rootfs | `cargo musl-libs --arch riscv64` |
| `libc-test`, `other-test` | Add test suites to the rootfs | `cargo libc-test --arch x86_64` |
| `image` | Builds the rootfs image | `cargo image --arch x86_64 [--variant minimal]` |
| `asm` | Disassembles the kernel (default `target/zcore.asm`) | `cargo asm -m virt-riscv64 -o riscv64.asm` |
| `bin` | Builds the raw kernel binary for a machine | `cargo bin -m nezha -o z.bin` |
| `qemu` | Boots in QEMU (`--gdb <port>` to debug) | `cargo qemu --arch x86_64 --smp 4` |
| `gdb` | Launches gdb attached to a port | `cargo gdb --arch riscv64 --port 1234` |
| `linux-libos`, `libos-libc-test` | Run LibOS mode on the host | `cargo linux-libos --args /bin/busybox` |

The full list and its options: `cargo xtask --help`. Valid machines for `-m` are
listed in [`config/machine-features.toml`](config/machine-features.toml).

### Makefile targets

| Target | What it does |
|---|---|
| `make qemu` | Image + boot in QEMU (`ARCH`, `VARIANT`, `SMP`, `DESKTOP`, `KOPTS`, …) |
| `make image` / `make rootfs` | Only the image / only the rootfs |
| `make iso` | A bootable UEFI ISO for one variant and architecture |
| `make release` | Every ISO in the variant × architecture matrix, into `dist/` |
| `make qcow2` / `make img` | Virtual disk / raw installed image |
| `make vbox` | Boots the image in VirtualBox |
| `make qemu-iso-aarch64` | Boots the aarch64 ISO with the right memory recipe |
| `make check`, `make doc` | `cargo check-style` / rustdoc documentation |
| `make clean`, `make cleanup`, `make clean-everything` | Remove rootfs and images; also `ignored/target`; also all of `ignored/` (downloads) |

### Variants and release ISOs

Eclipse ships in two **variants**, each with its own rootfs and its own
artifacts:

| Variant | What it contains | Rootfs | Session |
|---|---|---|---|
| `desktop` (default) | labwc/Xorg and all the Ubuntu desktop packages: Mesa, Firefox, XFCE, PulseAudio, freedoom | `rootfs/<arch>` | whatever `desktop=` / `/etc/eclipse/desktop` says, `labwc` by default |
| `minimal` | the base system: busybox, networking, `eclipse-init`, `install-eclipse` | `rootfs/<arch>-minimal` | console (`/etc/eclipse/desktop` = `none`) |

`minimal` is **not a trimmed-down** desktop: it is a rootfs that never installs
the graphics stack, so there is no list of things to prune that has to be kept
up to date.

> Today `minimal` **has no sound and no named time zones**: ALSA, PulseAudio,
> `mpg123` and `tzdata` are in the desktop package list. Splitting them out is
> pending work.

The `desktop` variant carries **no suffix** in any path, so `rootfs/x86_64`,
`zCore/x86_64.img` and `ignored/target/efi.img.gz` stay where they always were.

```bash
make release                                  # the whole matrix
make release ARCHS=x86_64                     # both x86_64 variants
make release VARIANTS=minimal                 # minimal on both architectures
make iso                                      # desktop, x86_64
make iso VARIANT=minimal                      # minimal, x86_64
```

```text
dist/eclipse-desktop-x86_64.iso
dist/eclipse-minimal-x86_64.iso
dist/eclipse-desktop-aarch64.iso
dist/eclipse-minimal-aarch64.iso
```

A combination that fails **does not abort the rest**: `make release` tries them
all and ends with a summary. CI builds and boots the x86_64 minimal ISO on every
change (`scripts/iso-smoke.sh`).

Per-architecture notes:

- **x86_64**: the ISO boots the installer on the console (`desktop=none`) and
  carries the payloads `install-eclipse` writes to disk. It needs 4 GB of RAM in
  QEMU.
- **aarch64**: the image is **linked into the kernel** (`LINK_USER_IMG=1`, the
  `link-user-img` feature), because the prebuilt `rayboot` cannot pass an
  initramfs. Boot it with `make qemu-iso-aarch64 ARCH=aarch64 VARIANT=minimal`
  and **1 GB** of RAM (with `-m 4G` nothing is printed after "jump to kernel").
  It does not install: it boots the console session.
- **riscv64**: no ISO. QEMU boots the kernel directly, without UEFI (see the
  `iso-unsupported-arch` target in the `Makefile`).

## Boot options

On x86_64 the kernel command line lives in [`zCore/rboot.conf`](zCore/rboot.conf)
(on the ESP: `\EFI\Boot\rboot.conf`), under the `cmdline` key. Options are
separated by `:` and, inside a command, `?` separates the program from its
arguments.

| Option | Effect | Default |
|---|---|---|
| `INIT=` | The PID 1 process. Only run if it exists. | `/sbin/init` → `eclipse-init` (busybox `init` as fallback) |
| `SHELL=` | The shell on each virtual terminal (tty1–tty6, PIDs 101–106) | `/bin/busybox?sh` |
| `ROOTPROC=` | Deprecated alias for `SHELL=` | — |
| `LOG=` | Kernel log level: `error`, `warn`, `info`, `debug`, `trace` | `error` |
| `ROOT=` | Root device; `install-eclipse` fills it in when installing | the boot medium |
| `desktop=` | Session: `labwc`, `xorg` or `none` (wins over `/etc/eclipse/desktop`) | `labwc` |

```ini
cmdline=LOG=warn:INIT=/sbin/init:SHELL=/bin/busybox?sh
cmdline=LOG=warn:INIT=/path/to/init?--option?value
```

Each subsystem has more parameters (NVIDIA, DRM, hunter…), described in its
guide; for example [`nvidia.*`](docs/README-nvidia-cmdline.en.md). The boot
process and how to measure it are in [docs/README-boot.en.md](docs/README-boot.en.md),
and the init system in [docs/README-init.en.md](docs/README-init.en.md).

## Platforms

### x86_64 (QEMU and real hardware)

- **Storage**: AHCI/SATA (with BIOS/OS handoff, PHY link stabilisation and PCI
  Bus Mastering) and NVMe (DMA cache coherency with `clflush`). MBR and GPT
  partition detection; each partition shows up in `devfs` (`/dev/sda1`,
  `/dev/nvme0n1p1`…).
- **File systems**: the installed system uses **EclipseFS** by default
  ([`vendor/eclipsefs-rs`](vendor/eclipsefs-rs/README.md)): copy-on-write B+
  trees, CRC32C on every node and atomic commits. With `ROOTFS_FS=btrfs` (or
  `ECLIPSE_ROOTFS_FS=btrfs` for xtask) **btrfs** is installed instead, with an
  in-house driver (`vendor/btrfs-rs`) and images that pass `btrfs check`. xtask
  generates both images, and they grow to the partition size on first mount.
  Also ext2/ext3/ext4 and vfat/FAT32 (EFI partition).
- **Graphics**: the Linux DRM/KMS UAPI with PRIME; Wayland (wlroots/labwc, with
  `pixman` when there is no GPU) and Xorg. NVIDIA GPUs with the RM driver, CUDA
  and a nouveau-compatible uAPI for NVK/zink. See
  [DRM](docs/README-drm.en.md), [Xorg](docs/README-xorg.en.md) and
  [NVIDIA](docs/README-nouveau-uapi.en.md).
- **Input**: PS/2 and USB HID (xHCI) keyboards, with a complete Spanish layout
  (`ñ`, `@`, `€`, `{`, `}`… via AltGr and Shift).
- **Audio**: Intel HDA with ALSA and PulseAudio ([guide](docs/README-audio.md)).
- **Network**: Intel e1000/e1000e/ixgbe, Realtek and others
  ([`drivers/src/net`](drivers/src/net)), with an in-house DHCP client (`edhcpc`).
- **Installer**: `install-eclipse` writes the system to a physical or virtual
  disk, sizing it via `sysfs` and `BLKGETSIZE64`, and fills in `/etc/fstab` and
  `rboot.conf`.
- **Memory**: chunked I/O syscalls and ELF loading through paged `VmObject`s so
  the kernel heap is not exhausted; diagnostics in `/proc/memhogs` and
  [oops](docs/README-oops.en.md).
- **Security**: [hunter](docs/hunter-security.md), LSM-style policies plus a
  behavioural IDS, with a forensic log in `/proc/hunter` and measured boot.

### aarch64 (QEMU virt)

`make qemu ARCH=aarch64`, or the ISO described above.

### RISC-V

- **QEMU virt**: `make qemu ARCH=riscv64`.
- **Boards** (the user image is linked into the kernel):

| Board | Command | Guide |
|---|---|---|
| Allwinner D1 / Nezha | `cargo bin -m nezha -o z.bin` | [README-D1](docs/README-D1.en.md) |
| StarFive VisionFive | `cargo bin -m visionfive -o z.bin` | [README-visionfive](docs/README-visionfive.en.md) |
| CVITEK CR1825 | `cargo bin -m cr1825 -o z.bin` | — |
| SiFive FU740 (Unmatched) | `cargo bin -m fu740 -o z.bin` | [README-fu740](docs/README-fu740.en.md) |
| T-Head C910 | — | [README-C910](docs/README-C910.en.md) |

## Packages (apt)

Eclipse uses **apt/dpkg** with **Ubuntu 26.10 (`stonking`)** packages for the
dynamic userspace. It is not a full Ubuntu: the init system and Eclipse's own
tools stay static.

- On the running system: `apt-get update && apt-get install <package>`.
- At build time: `ECLIPSE_PKG=apt` (default on x86_64 and aarch64) or
  `ECLIPSE_PKG=apk` (the legacy Alpine path; riscv64 still uses apk).
- The desktop package set is assembled in
  [`xtask/src/linux/apt_rootfs.rs`](xtask/src/linux/apt_rootfs.rs).

## Documentation

The full index, with every guide in English and Spanish, is in
[docs/README.en.md](docs/README.en.md). The most used ones:

- [labwc desktop and looks](docs/README-desktop.en.md)
- [Boot](docs/README-boot.en.md) and [init](docs/README-init.en.md)
- [Linux compatibility](docs/linux-compat.en.md) and [FreeBSD compatibility](docs/README-freebsd.en.md)
- [DRM/KMS](docs/README-drm.en.md), [Xorg](docs/README-xorg.en.md), [audio](docs/README-audio.md)
- [hunter](docs/hunter-security.md)
- [Developer notes](docs/for-developers.en.md) and [CI](docs/ci-testing.md)
- [Original zCore README](README-arch.md)

## License

[MIT](LICENSE).
