# rBoot

The x86_64 and aarch64 UEFI bootloader for rCore / zCore, as carried by
Eclipse OS. This is upstream [rcore-os/rboot](https://github.com/rcore-os/rboot)
plus the Eclipse additions listed below; upstream changes are ported in
(the tree is not a submodule).

## Build

```sh
make build                                  # x86_64 (target/x86_64-unknown-uefi/release/rboot.efi)
make build TARGET=aarch64-unknown-uefi      # aarch64
make clippy && cargo fmt --check            # what CI expects to pass
```

The toolchain is pinned in `rust-toolchain.toml` (nightly, for the
`abi_x86_interrupt` ABI used by the diagnostic IDT); `rustup` installs it and
both UEFI targets on first use. Eclipse's `cargo bin -m virt-x86_64` runs
`make build` here and copies the EFI binary into the boot image.

## Example

See [`example-kernel/`](example-kernel/) for a minimal bare-metal kernel that boots via rboot and prints to serial.

Run `example-kernel/test.sh` to build and test in QEMU.
Run `example-kernel/test-aarch64.sh` for the equivalent aarch64 QEMU test.

## Configuration

Edit `rboot.conf` (read from `\EFI\Boot\rboot.conf`; the built-in defaults
apply when it is missing). Available options:

- `kernel_path` - path to the kernel ELF binary. When it is missing, `\os`,
  `\EFI\Boot\os` and `\EFI\zCore\zcore.elf` are tried in that order.
- `kernel_stack_address` - virtual address for the kernel stack
- `kernel_stack_size` - kernel stack size in 4KiB pages
- `physical_memory_offset` - virtual address offset for physical memory mapping
- `resolution` - graphic output mode: `auto` (the display's EDID-preferred
  mode, capped at 4K, falling back to the largest firmware mode within the
  cap), or an exact `WxH` (kept current if the firmware does not offer it).
  Absent: keep the firmware mode.
- `initramfs` - path to the initial ramdisk image (loaded in 8 MiB chunks so
  slow firmware FAT drivers keep the progress bar moving)
- `cmdline` - kernel command line. rboot itself honours `LOG=<level>`, and
  `FB_ROT180` / `FB_MIRROR_X` to flip its splash/progress drawing.
- `uart_base` - UART physical address passed to an aarch64 kernel
- `gic_base` - GIC distributor physical address passed to an aarch64 kernel
- `firmware_type` - platform identifier passed to an aarch64 kernel

Numbers accept decimal or `0x` hex; a malformed value keeps the default and
logs a warning instead of aborting the boot.

On aarch64, `physical_memory_offset` must be aligned to 512 GiB. The temporary
boot page tables derive RAM and device memory attributes from the UEFI memory
map before transferring control to the kernel.

## What Eclipse adds on top of upstream

- Multi-GPU aware GOP selection (the largest directly-addressable
  framebuffer wins) and the active display's EDID passed to the kernel in
  `BootInfo::edid` (used for HDMI audio ELD and mode selection).
- Splash logo and a 0..50% progress bar drawn straight into the framebuffer;
  the kernel continues it from 50%.
- `ExitBootServices` through the raw tables with retries, so a failure
  reports its `Status` instead of resetting the machine.
- A tiny IDT installed right before the hand-off so a fault on the first
  kernel instruction paints a marker instead of a frozen screen.
- Tolerant mappings when firmware already mapped a page to the same frame,
  and no forced NX on the kernel image.
