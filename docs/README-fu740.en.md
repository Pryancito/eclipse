# zCore on riscv64: SiFive fu740 board

[Español](README-fu740.md) · **English**

> Legacy notes from the original zCore (rCore-OS), translated from Chinese. The
> board still builds with `PLATFORM=fu740` in `zCore/Makefile`, but it has not
> been tested again on Eclipse.

### Build the zCore image for fu740

First, at the root of the source tree, build the riscv64 filesystem.

Then go into the zCore subdirectory and build the kernel; this produces the
system image `zcore-fu740.itb`.

The image includes the fu740 board's device tree. The dtb can be taken from the
`/boot` directory of the Linux the fu740 ships with, or from SiFive's official
image:
https://github.com/sifive/freedom-u-sdk/releases/download/2022.04.00/demo-coreip-cli-unmatched-2022.04.00.rootfs.wic.xz

```sh
make rootfs ARCH=riscv64 && make image ARCH=riscv64
cd zCore
make build MODE=release LINUX=1 ARCH=riscv64 PLATFORM=fu740
```

### Boot the image from U-Boot

First set up a tftp server; for example, install `tftpd-hpa` on a Linux
server; the tftp service directory is usually `/srv/tftp/`.

Then copy the built fu740 image, `zcore-fu740.itb`, to the tftp service
directory.

Power on the fu740 board and enter the U-Boot command line:

```
# Set the board's IP and the server's IP
setenv ipaddr <IP>
setenv serverip <Server IP>

# Load the image over tftp
tftp 0xa0000000 zcore-fu740.itb

# Run it
bootm 0xa0000000
```

### fu740 documentation

Collected documentation for the Unmatched fu740 board is at:
https://github.com/oscomp/DocUnmatchedU740/blob/main/unmatched.md
