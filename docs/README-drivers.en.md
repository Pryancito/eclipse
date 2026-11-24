# Writing a driver for Eclipse OS

On Eclipse OS a driver is an **ordinary program** that runs as a process,
not a kernel module. The kernel hands it a whole PCI device: its registers,
memory for DMA and its interrupt. If the driver crashes, the rest of the
system stays up. This guide goes from nothing to a `.drv` file that anyone
can install from lunarfile or from the control panel.

The complete example driver is
[`tools/eclipse-udrivers/src/bin/udrv-ejemplo.rs`](../tools/eclipse-udrivers/src/bin/udrv-ejemplo.rs).
It builds, and it is the recommended starting point.

## How it works

1. The user installs `my-driver-14c3-0616.drv`. The panel copies the file,
   creates its eclipse-init service and adds `pci-14c3-0616@0` to `udriver=`
   in `rboot.conf`.
2. At boot the kernel sees `udriver=pci-14c3-0616@0` and leaves alone the
   first PCI function with vendor `14c3` and device `0616`, and so does its
   own driver if it has one. It records the function's first memory BAR and
   its MSI, and lists it in `/proc/udrivers`.
3. eclipse-init sees the device in `/proc/udrivers` and starts the service:
   `/usr/lib/eclipse/udrivers/udrv-14c3-0616 pci-14c3-0616@0`.
4. The driver **claims** the device (`claim_pci`), maps its registers, asks
   for DMA memory and waits for interrupts. What it offers the rest of the
   system it offers as a node in `/dev` (or as a disk or a network card, as
   the shipped drivers do).
5. If the process dies, eclipse-init starts it again (`type = respawn`).

## What you need

- Rust with the musl target: `rustup target add x86_64-unknown-linux-musl`.
- The `eclipse-udrivers` crate from this repository. It has the call that
  claims the device, DMA memory, interrupts and `/dev` nodes.

```toml
# your driver's Cargo.toml
[package]
name = "my-driver"
version = "0.1.0"
edition = "2021"

[dependencies]
udrivers = { package = "eclipse-udrivers", git = "https://github.com/Pryancito/eclipse" }
```

## The driver, step by step

Its argument is the device, as the kernel names it: `pci-VVVV-DDDD@N`.

### 1. Claim the device

```rust
use udrivers::claim;

let claim = claim::claim_pci(0x14c3, 0x0616, 0, 100)?;
```

Only one process can hold the device at a time. The last number is how many
times it retries (every 10 ms) while another process still has it, such as
the previous driver, which has just died. It returns a `Claim`:

| Field | What it is |
|---|---|
| `size` | Size of the register window (the first memory BAR). |
| `pci_id` | `vendor << 16 \| device`. |
| `irq` | The line, for the record only (`u32::MAX` when there is none). |
| `interrupt` | The interrupt object to wait on, or `None`. |
| `bti` | For asking for DMA memory. |

### 2. The registers

```rust
let regs: *mut u8 = claim.map_regs()?;
let status = unsafe { (regs.add(0x08) as *const u32).read_volatile() };
```

The window is mapped uncached. **Always** use `read_volatile` and
`write_volatile`, and only within `claim.size` bytes.

### 3. DMA memory

```rust
use udrivers::claim::Dma;

let ring = Dma::new(claim.bti, 4096, None)?;
// ring.ptr: where you see it. ring.daddr: the address to give the device.
```

It is contiguous, pinned and zeroed, in whole pages. It is yours for the
life of the process.

### 4. Interrupts

```rust
if let Some(irq) = claim.interrupt {
    std::thread::spawn(move || {
        while claim::interrupt_wait(irq).is_ok() {
            // read the device's status and handle what is there
        }
    });
}
```

The interrupt is an MSI and is not masked: the driver has to quiet the
device on each round, as its datasheet says.

### 5. Offering it to the system

The most general way is a node in `/dev` that the process itself serves:

```rust
use udrivers::served::{Device, Node};

let node = Node::serve("my-driver", 240, 0, 0o666)?;
node.run(&std::sync::Mutex::new(MyDevice { /* ... */ }));
```

`MyDevice` implements the `served::Device` trait: `open`, `read`, `write`,
`release` and `ioctl`, each returning an `errno` on failure. Major 240 is
the local range. Pick a minor no other driver uses.

For a disk or a network card, see how `udrv-nvme` / `udrv-ahci` (`blk.rs`)
and `udrv-virtio-net` (`net.rs`) do it.

Add `panic = "abort"` to `[profile.release]`: a driver that panics has
nothing to unwind into.

## Build and package

Eclipse OS binaries are static and non-PIE:

```sh
RUSTFLAGS="-C relocation-model=static -C target-feature=+crt-static" \
  cargo build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/my-driver my-driver-14c3-0616.drv
```

The `.drv` name **must carry the device's ids** (`VVVV-DDDD`, four hex
digits each, at the end of the name), so the panel knows which device it is
for. The ids are on the panel's "Devices" page (COPY IDS) or in `lspci -n`.

On aarch64 and riscv64 the target is `aarch64-unknown-linux-musl` or
`riscv64gc-unknown-linux-musl`.

## Install and test

- **From lunarfile:** double-click the `.drv`. The panel opens with the
  install dialog already set to the device.
- **From the panel:** go to Devices, pick the device and press INSTALL
  DRIVER…. It looks for `.drv` files in `~`, `~/Descargas` and
  `~/Downloads`.
- **By hand:** run `eclipse-udriver instalar 14c3 0616 my-driver-14c3-0616.drv`,
  then add `pci-14c3-0616@0` to `udriver=` on the `cmdline=` line of
  `/boot/efi/rboot.conf`.

Then restart and check:

| What | Where |
|---|---|
| The kernel handed the device over | `cat /proc/udrivers` |
| The service is running | `/run/eclipse-init/services`, or the panel's page ("in use · userspace") |
| The driver's output | `/tmp/udrv-14c3-0616.log` |

To develop without reinstalling each time, stop the service
(`: > /run/eclipse-init/ctl/stop.udrv-14c3-0616; kill -HUP 1`) and run your
binary by hand with the same argument.

To remove it, use REMOVE DRIVER in the panel or
`eclipse-udriver quitar 14c3 0616`, and take the `pci-…` out of `udriver=`.

## Limits to know

- **No IOMMU.** DMA addresses are physical, so a buggy driver can write
  anywhere in memory. The panel warns about this on install. Test on a
  machine you can restart.
- **If the driver dies, the device waits for a restart.** The kernel does
  not know how to stop a device it does not know, so it keeps it out of use
  with its memory pinned. Exit cleanly: leave the device stopped before you
  quit.
- **One device per identity.** The installer hands over the first function
  with those ids (`@0`). For a second identical card, add `@1` by hand.
- **Only the first memory BAR.** There is no access to PCI configuration
  space or to I/O-port BARs.
- **On x86_64 the interrupt is an MSI.** On the other architectures it is a
  level-triggered line.

## Reference

- `tools/eclipse-udrivers/src/claim.rs`: `claim_pci`, `Dma`, `interrupt_wait`.
- `tools/eclipse-udrivers/src/served.rs`: `/dev` nodes.
- `drivers/src/user.rs`: what the kernel hands over and how (`udriver=`).
- `loader/src/linux.rs`, `ECLIPSE_DEVICE_CLAIM`: the system call.
- `xtask/src/linux/panel.rs`, `write_udriver`: what installing does.
- Shipped drivers worth reading: `udrv-nvme`, `udrv-ahci`, `udrv-hda`,
  `udrv-xhci`, `udrv-virtio-net`.
