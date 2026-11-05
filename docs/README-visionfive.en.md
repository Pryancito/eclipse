# zCore on the StarFive VisionFive board

[Español](README-visionfive.md) · **English**

> Legacy notes from the original zCore (rCore-OS), translated from Chinese. They
> have not been tested again on Eclipse.

## 1. Flashing the VisionFive board

> **NOTICE** The host-side steps described here have been tested on Ubuntu 20.04.

### 1. Set up the required services on the industrial PC

```bash
# Set up dhcpd
# 1. Install dhcp3-server
sudo apt install isc-dhcp-server

# 2. Pick the dhcp service interface: the real physical network interface
vim /etc/default/isc-dhcp-server
------------------------------------------------
INTERFACESv4="enp2s0" # the dhcpv4 port is enp2s0
INTERFACESv6=""
------------------------------------------------

# 3. Give that interface an IP address and bring it up
vim /etc/netplan/01-network-manager-all.yaml
-----------------------------------------------------------
# Let NetworkManager manage all devices on this system
network:
  version: 2
  renderer: NetworkManager
  ethernets:
    eno1:
      addresses: []
      dhcp4: true
      optional: true
    enp2s0:
      addresses: [192.168.10.10/24]
      gateway4: 192.168.10.1
      nameservers:
        addresses: [114.114.114.114,8.8.8.8]
      dhcp4: no
------------------------------------------------------------
sudo netplan appply

# 4. Configure dhcp
vim /etc/dhcp/dhcpd.conf
----------------------------------------------------
subnet 192.168.10.0 netmask 255.255.255.0 {
  range 192.168.10.120 192.168.10.128;
  option routers 192.168.10.10;
  option subnet-mask 255.255.255.0;
  option broadcast-address 192.168.10.255;
  option domain-name-servers 192.168.10.10;
}
-----------------------------------------------------

# 5. Start the dhcpd service
systemctl restart isc-dhcp-server

# 6. Check the dhcpd service status
systemctl status isc-dhcp-server

# Set up tftpd
# 1. Install tftpd-hpa
sudo apt install tftpd-hpa

# 2. Edit the configuration
--------------------------------------------------------------
TFTP_USERNAME="tftp"
TFTP_DIRECTORY="/root/work/tftpboot" # tftp file download path
TFTP_ADDRESS=":69"
TFTP_OPTIONS="--secure"
--------------------------------------------------------------

# 3. Start the tftpd service
systemctl restart tftpd-hpa

# 4. Check the tftpd service status
systemctl status tftpd-hpa
```

### 2. Connect the industrial PC and the VisionFive board

![avatar](visionfive.jpg)

```bash
# The black round-tipped cable is the industrial PC power cable
# The yellow network cable is what the board uses to get an address over dhcp
# The blue network cable is what the industrial PC uses to download from the public network; it can be ignored
# The white USB connector powers the board, 5 V/3 A
# The black USB connector is the serial console
```

Open the serial assistant (minicom) on the industrial PC; after powering on the board, within a few seconds it drops into the u-boot command line by itself

```bash
Welcome to minicom 2.7.1

OPTIONS: I18n
Compiled on Dec 23 2019, 02:06:26.
Port /dev/ttyUSB0, 12:54:20

Press CTRL-A Z for help on special keys


VisionFive #
VisionFive #
VisionFive #

# Here you can use `help` to see which commands are available
# and `printenv` to see which environment variables are set

# 1. Type the `dhcp` command so the board gets an IP address
# Note: it may start downloading the image from the server by itself; interrupt it with Ctrl+C
dhcp

# 2. tftp client address (this machine)
setenv ipaddr 192.168.10.121 # the address dhcp handed out

# 3. tftp server address (the industrial PC)
setenv serverip 192.168.10.10 # must be on the same subnet as your IP

# 4. Check that the network works
ping 192.168.10.10 # try pinging the host (optional)

# 5. Save the configuration
saveenv # saves it to flash; no need to configure it again
```

### 3. Build the itb image

```bash
# 1. Download `starfive_fb.h`, `starfive_vic7100_clk.dtsi` and `starfive_vic7100_evb.dts` from the official VisionFive repository

# 2. Build the dtb with these commands
cpp -nostdinc -I include -undef -x assembler-with-cpp starfive_vic7100_evb.dts starfive.dts.0

dtc -o starfive.dtb starfive.dts.0	# a prebuilt starfive.dtb is in the prebuilt directory

# 3. Compress the kernel image into `z.bin.gz`
gzip -9 -cvf z.bin > z.bin.gz

# 4. Build the itb file
mkimage -f zcore-starfive.its z.itb	# zcore-starfive.its is the starfive_fdt.its in the prebuilt directory, so mkimage -f prebuilt/firmware/riscv/starfive_fdt.its z.itb does the same

These are the contents of zcore-starfive.its
--------------------------------------------------------------------
/*
 * U-Boot uImage source file for "zCore-visionfive"
 */

/dts-v1/;

/ {
    description = "U-Boot uImage source file for zCore-visionfive";
    #address-cells = <1>;

    images {
        kernel {
            description = "Linux kernel for zCore-visionfive";
            data = /incbin/("./z.bin.gz");
            type = "kernel";
            arch = "riscv";
            os = "linux";
            compression = "gzip";
            load = <0x80200000>;
            entry = <0x80200000>;
        };
        fdt {
            description = "Flattened Device Tree blob for zCore-visionfive";
            data = /incbin/("./starfive.dtb");
            type = "flat_dt";
            arch = "riscv";
            compression = "none";
        };
    };

    configurations {
        default = "conf";
        conf {
            description = "Boot Linux kernel with FDT blob";
            kernel = "kernel";
            fdt = "fdt";
        };
    };
};
--------------------------------------------------------------------

# 5. Put `z.itb` in the tftp server download path
cp z.itb /path/to/tftpboot
```

### 4. Upload the itb file to the board memory and boot from that address

```bash
VisionFive #tftpboot 0xc0000000 z.itb
Speed: 1000, full duplex
Using dwmac.10020000 device
TFTP from server 192.168.10.10; our IP address is 192.168.10.121
Filename 'z.itb'.
Load address: 0xc0000000
Loading: ##################################################  1.5 MiB
         11.4 MiB/s
done
Bytes transferred = 1584003 (182b83 hex)
VisionFive #bootm 0xc0000000
## Loading kernel from FIT Image at c0000000 ...
   Using 'conf' configuration
   Trying 'kernel' kernel subimage
     Description:  Linux kernel for zCore-FU740
     Type:         Kernel Image
     Compression:  gzip compressed
     Data Start:   0xc00000e8
     Data Size:    1564993 Bytes = 1.5 MiB
     Architecture: RISC-V
     OS:           Linux
     Load Address: 0x80200000
     Entry Point:  0x80200000
   Verifying Hash Integrity ... OK
## Loading fdt from FIT Image at c0000000 ...
   Using 'conf' configuration
   Trying 'fdt' fdt subimage
     Description:  Flattened Device Tree blob for zCoreFU740
     Type:         Flat Device Tree
     Compression:  uncompressed
     Data Start:   0xc017e2ec
     Data Size:    17270 Bytes = 16.9 KiB
     Architecture: RISC-V
   Verifying Hash Integrity ... OK
   Booting using the fdt blob at 0xc017e2ec
   Uncompressing Kernel Image
   Loading Device Tree to 00000000ff6e1000, end 00000000ff6e8375 ... OK

Starting kernel ...


boot page table launched, sstatus = 0x8000000200046000
kernel (physical): 0000000080200000..0000000085ce70b8
kernel (remapped): ffffffc080200000..ffffffc085ce70b8
device tree:       00000000ff6e1000..00000000ff6e6000

hart0 is booting...
hart1 is the primary hart.

[ 28.007868 WARN  1 0:0 zcore_drivers::builder::devicetree] device-tree: failed to parsing node "gpio@11910000": InvalidParam
[ 28.022102 ERROR 1 0:0 zcore_drivers::irq::riscv_intc] invalid SCAUSE value 0xb!
[ 28.032369 ERROR 1 0:0 zcore_drivers::irq::riscv_intc] invalid SCAUSE value 0xb!
[ 28.074321 WARN  1 0:0 linux_syscall] brk: unimplemented
[ 28.082355 WARN  1 0:0 linux_syscall] brk: unimplemented
[ 28.090609 WARN  1 0:0 linux_syscall::vm] mprotect: unimplemented
[ 28.100315 WARN  1 0:0 linux_syscall::vm] mprotect: unimplemented
[ 28.109179 WARN  1 0:0 linux_syscall] getuid: unimplemented
[ 28.117922 WARN  1 0:0 linux_syscall] getpgid: unimplemented
[ 28.126330 WARN  1 0:0 linux_syscall] setpgid: unimplemented
[ 28.134857 WARN  1 0:0 linux_object::fs::stdio] stdout TCGETS | TIOCSPGRP, pretend to be tty.
[ 28.146419 WARN  1 0:0 linux_object::fs::stdio] stdin TCGETS | TIOCSPGRP, pretend to be tty.
[ 28.157926 WARN  1 0:0 linux_syscall] geteuid: unimplemented
[ 28.166389 WARN  1 0:0 linux_syscall] geteuid: unimplemented
/ #


## 2. VisionFive porting issues

### 1. Flashing

We flash with the command `tftpboot 0xc0000000 z.itb`; the `0xc0000000` address has to leave enough room for the decompressed kernel.

### 2. Memory mapping

The StarFive board u-boot has to set where the kernel and the device tree are loaded, and then tells the kernel the device tree position in register `a1`. In practice the device tree ends up above 1 GiB, and zCore only supported 1 GiB, so it reported an illegal memory error. The memory map layout has to change; the changes are in these two commits:

[commit1](https://github.com/rcore-os/zCore/commit/227956d5df401c8f8f2fa746f8aa911d3530637f)

[commit2](https://github.com/rcore-os/zCore/commit/16772b9363d02945863008c6a4639ad1cb37eed4)

### 3. There was a bug in interrupt handling: each interrupt has to be handled by the current core. The fix is in

[commit](https://github.com/rcore-os/zCore/commit/55b3145442f0f70c01527c20e87988d26c01a39b)

### 4. Serial driver

The StarFive board supports the `16550` driver; adapting the code is enough. The change is in

[commit](https://github.com/rcore-os/zCore/commit/55b3145442f0f70c01527c20e87988d26c01a39b)

### 5. Unregistered interrupt

We found that the StarFive board, when booting from core `1`, raises interrupt `131`, which is not in the device tree and so is not registered in the kernel, which causes an error. When handling interrupts we lowered its priority to mask it, see:

[commit](https://github.com/rcore-os/zCore/commit/55b3145442f0f70c01527c20e87988d26c01a39b)
