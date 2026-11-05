# zCore on riscv64: Allwinner D1 board

[Español](README-D1.md) · **English**

> Legacy notes from the original zCore (rCore-OS), translated from Chinese. The
> board still builds with `PLATFORM=d1` in `zCore/Makefile`, but it has not been
> tested again on Eclipse.

## D1 development board

### Build the zCore system image

First, at the root of the source tree, build the riscv64 filesystem.

Then go into the zCore subdirectory and build the kernel; this produces the
system image `zcore.bin`.

```sh
make rootfs ARCH=riscv64 && make image ARCH=riscv64
cd zCore
make build LINUX=1 ARCH=riscv64 PLATFORM=d1 MODE=release
```

### Flashing the riscv64 board

We use the Allwinner D1 board with a C906 as the example.

Download and build the flashing tool `xfel`:

```sh
git clone https://github.com/xboot/xfel.git
cd xfel
make
```

### Automatic flash and run

With `xfel` installed, put the board into FEL mode; from the board's Linux, the
`reboot efex` command enters it. Then run:

```sh
make run LINUX=1 ARCH=riscv64 PLATFORM=d1 MODE=release
```

### (Optional) Manual flash and run

1. Download the [OpenSBI](https://github.com/elliott10/opensbi) source for the
   D1 board and build the image
   build/platform/thead/c910/firmware/fw_payload.elf:

    ```sh
    git clone https://github.com/elliott10/opensbi -b thead
    cd opensbi
    make PLATFORM=thead/c910 CROSS_COMPILE=/path/to/toolchain/bin/riscv64-unknown-linux-gnu- SUNXI_CHIP=sun20iw1p1 PLATFORM_RISCV_ISA=rv64gcxthead
    ```

    Or use the prebuilt image [prebuilt/firmware/riscv/d1_fw_payload.elf](../prebuilt/firmware/riscv/d1_fw_payload.elf).

2. Build the firmware to flash, which contains OpenSBI, the dtb and zCore:

    ```sh
    rust-objcopy --binary-architecture=riscv64 ../prebuilt/firmware/d1/fw_payload.elf --strip-all -O binary ./zcore_d1.bin
    dd if=../target/riscv64/release/zcore.bin of=zcore_d1.bin bs=512 seek=2048
    ```

3. Power on the Allwinner D1 C906 board and put it into FEL mode. Then load the
   zCore image into DDR with the `xfel` tool:

    ```
    sudo xfel ddr d1
    sudo xfel write 0x40000000 zcore_d1.bin
    sudo xfel exec 0x40000000
    ```

### Booting

When zCore boots successfully, OpenSBI loads the dtb at the high address
`0x5ff00000`, and it looks like this:

```
OpenSBI smartx-d1-tina-v1.0.1-release
   ____                    _____ ____ _____
  / __ \                  / ____|  _ \_   _|
 | |  | |_ __   ___ _ __ | (___ | |_) || |
 | |  | | '_ \ / _ \ '_ \ \___ \|  _ < | |
 | |__| | |_) |  __/ | | |____) | |_) || |_
  \____/| .__/ \___|_| |_|_____/|____/_____|
        | |
        |_|

Platform Name          : T-HEAD Xuantie Platform
Platform HART Features : RV64ACDFIMSUVX
Platform Max HARTs     : 1
Current Hart           : 0
Firmware Base          : 0x40000400
Firmware Size          : 75 KB
Runtime SBI Version    : 0.2

MIDELEG : 0x0000000000000222
MEDELEG : 0x000000000000b1ff
PMP0    : 0x0000000040000000-0x000000004001ffff (A)
PMP1    : 0x0000000040000000-0x000000007fffffff (A,R,W,X)
PMP2    : 0x0000000080000000-0x00000000bfffffff (A,R,W,X)
PMP3    : 0x0000000000020000-0x0000000000027fff (A,R,W,X)
PMP4    : 0x0000000000000000-0x000000003fffffff (A,R,W)
      ____
 ____/ ___|___  _ __ ___
|_  / |   / _ \| '__/ _ \
 / /| |__| (_) | | |  __/
/___|\____\___/|_|  \___|

Welcome to zCore rust_main( hartid: 0x0, device_tree_paddr: 0x44ddc )
Uart output testing
+++ Setting up UART interrupts +++
+++ Setting up PLIC +++
+++ setup interrupt +++
Exception::Breakpoint: A breakpoint set @0xffffffffc0167f56
Device Tree @ 0x0
[138.8430296s  WARN 0 0:0] elf relocate Err:".rela.dyn not found"
[139.2137079s  WARN 0 0:0] brk: unimplemented
[139.8335662s  WARN 0 0:0] TCGETS | TIOCGWINSZ | TIOCSPGRP, pretend to be tty.
[140.5358217s  WARN 0 0:0] TIOCGPGRP, pretend to be have a tty process group.
[140.6017734s  WARN 0 0:0] getpgid: unimplemented
[140.8971624s  WARN 0 0:0] setpgid: unimplemented
/ #
/ # ls
bin  dev  tmp
/ # hello
Hello world from user mode program!
                                   By xiaoluoyuan@163.com
/ #

```
