# zCore on riscv64: T-HEAD C910 Light board

[Español](README-C910.md) · **English**

> Legacy notes from the original zCore (rCore-OS), translated from Chinese. The
> board still builds with `PLATFORM=c910light` in `zCore/Makefile`, but it has
> not been tested again on Eclipse.

## T-HEAD C910 Light validation board: how to use it

### Build the zCore kernel image

Build the zCore kernel:
```
cd zCore
make build LINUX=1 MODE=release ARCH=riscv64 PLATFORM=c910light
```

Create the u-boot system image:
```
mkimage -A riscv -O linux -C none -T kernel -a 0x200000 -e 0x200000 -n "zCore for c910" -d ../target/riscv64/release/zcore.bin uImageC910
```

### Build the opensbi image
```
git clone https://github.com/elliott10/opensbi.git -b thead_light-c910

cd opensbi

make PLATFORM=generic CROSS_COMPILE=/path/to/toolchain/bin/riscv64-unknown-linux-gnu-
# produces the required fw_dynamic.bin
```
Note: the original toolchain was built from the official repository
https://gitee.com/thead-yocto/xuantie-yocto.git. In principle another toolchain
can be used instead.

### Boot from u-boot

Put the built opensbi image `fw_dynamic.bin` and the system image `uImageC910`
in the directory of an already configured tftp server.<br>
On the u-boot command line of the C910 Light board, with networking set up,
run:
```
ext4load mmc 0:2 $aon_ram_addr light_aon_fpga.bin; ext4load mmc 0:2 $dtb_addr ${fdt_file};

tftp $opensbi_addr fw_dynamic.bin;
tftp $kernel_addr uImageC910;

bootslave; run finduuid; run set_bootargs; bootm $kernel_addr - $dtb_addr;

```

## T-HEAD C910 Light validation board: porting notes

### 1. Initial analysis of the system and building the image

<img src="img/c910-light.jpeg" alt="c910-light" style="zoom:25%;" />

The C910 Light development board is the one pictured. On getting it, first read
the official user manual to learn the basic hardware components and how power,
the serial port and the other interfaces are wired.<br>
User manual: https://gitee.com/thead-yocto/documents/blob/master/en/user_guide/T-Head%20Yeying1520%20Yocto%20User%20Guide.pdf <br>

With power connected and the serial port attached to the host, four serial
ports `/dev/ttyUSBX` show up; the debug one is the one the host connects to,
and pressing any key while u-boot starts drops into command-line mode.<br>
From the u-boot command line you can bring up wired networking and load the
operating system image to boot over tftp.<br>

```
# minicom -b 115200 -D /dev/ttyUSB2

U-Boot 2020.01-ge0ddd4721a (Dec 14 2021 - 22:26:59 +0800)

CPU:   rv64imafdcvsu
Model: T-HEAD c910 light
DRAM:  1 GiB
C910 CPU FREQ: 1500MHz
AHB2_CPUSYS_HCLK FREQ: 250MHz
AHB3_CPUSYS_PCLK FREQ: 125MHz
PERISYS_AHB_HCLK FREQ: 250MHz
PERISYS_APB_PCLK FREQ: 62MHz
GMAC PLL POSTDIV FREQ: 1000MHZ
DPU0 PLL POSTDIV FREQ: 1188MHZ
DPU1 PLL POSTDIV FREQ: 1188MHZ
MMC:   sdhci@ffe7080000: 0, sd@ffe7090000: 1
Loading Environment from MMC... OK
In:    serial@ffe7014000
Out:   serial@ffe7014000
Err:   serial@ffe7014000
Net:   eth0: ethernet@ffe7070000
Hit any key to stop autoboot:  0 
C910 Light# 

# setenv ipaddr <IP>
# setenv serverip <Server IP>

```

At first the same approach as for the fu740 board was tried for building the
zCore image and booting it over tftp: set the board's IP and the server's IP on
the u-boot command line, load the image over tftp and finally run it with
bootm.<br>

Everything else was fine, but booting the zCore image that way gave an error
saying the image was not recognised.
```
Filename 'uImageC910'.
Load address: 0x200000
Loading: #################################################################
         #############################################
         5.1 MiB/s
done
Bytes transferred = 1609651 (188fb3 hex)
Wrong Image Format for bootm command
ERROR: can't get kernel image!
```
Analysis showed that different u-boot versions recognise different image
formats.<br>
The fu740 board's zCore image is the new `FIT image`, based on an `.its`
script;<br>
the C910 Light board's u-boot only supports the `old legacy image`, which is
built like this:

```
mkimage -A riscv -O linux -C none -T kernel -a 0x200000 -e 0x200000 -n "zCore for c910" -d ../target/riscv64/release/zcore.bin uImageC910
```

### 2. Building the zCore boot

From the Linux image the board already shipped with, the load address is
`Load Address: 0x200000` and the entry point is `Entry Point: 0x200000`.

With the `0x200000` entry address, the `BASE_ADDRESS` variable in zCore's
`linker.ld` linker script was changed to match, the image was built and, in
u-boot, loaded over the network with `tftp` and booted with `bootm`; after
jumping to the kernel nothing happened.

There were two possibilities: either zCore's serial output, which by default
calls opensbi's print, was not working, or something went wrong while building
virtual memory and jumping into it. First zCore was made to boot using physical
memory, temporarily changing the boot offset variables in the
`zCore/src/platform/riscv/` directory to the values for a `0x200000` entry
address; that removed the second problem and left the first, opensbi's output.

To check whether opensbi's output worked, the smallest possible zCore image was
tried, calling opensbi's `SBI_CONSOLE_PUTCHAR` at the earliest point of the
Rust code. Still nothing came out, and the problem was stuck here.

The C910 Light is a large square board with a great many interfaces, including
a JTAG port for debugging the CPU. The debugging environment was set up
following T-Head's official documentation:
https://occ.t-head.cn/document?temp=linux&slug=t-head-debug-server-user-manual

<img src="img/c910-jtag.jpg" alt="jtag" style="zoom:25%;" />

With gdb connected to the JTAG Debug Server, the C910 CPU could be single
stepped, although sometimes a breakpoint could not be set and the zCore image
had to be loaded into memory first and the function breakpoints set
afterwards.

Single stepping over JTAG showed that after executing
`ecall SBI_CONSOLE_PUTCHAR` nothing happened, when a character should have come
out of the serial port. Either there was no valid opensbi, or the serial driver
was broken.

Fixing the serial output was the most direct route, so, using u-boot's serial
code as a reference, the basic output functionality was enabled:

```
// T-HEAD C910 light
pub fn uart_put(c: u8) {
    let ptr = BADDR as *mut u32;
    unsafe {
        //LSR bit:THRE
        while ptr.add(5).read_volatile() & (1 << 5) == 0 {}

        //transmitter empty, THR has 8 valid bits
        ptr.add(0).write_volatile(c as u32);
    }
}
pub fn uart_get() -> Option<u8> {
    let ptr = BADDR as *mut u32;
    unsafe {
        //if LSR's DR bit is 1 there is data
        if ptr.add(5).read_volatile() & 0b1 == 0 {
            None
        } else {
            Some((ptr.add(0).read_volatile() & 0xff) as u8)
        }
    }
}
```

With that zCore could run and print.

Next came a failure parsing the device tree DTB. After some investigation it
turned out the zCore kernel was overwriting the memory the DTB had been loaded
into; moving the DTB address a bit higher made it parse correctly.

![c910 uart](img/c910-run-uart.png)

Later, talking to T-Head's technical support, the documentation and code for
the C910 Light board were obtained, with more useful information: u-boot, the
Linux kernel, the filesystem and the other components the board needs can be
custom-built.

Analysing the board's documentation and source code together made its boot
chain clear:

```
U-Boot runs in M mode
	|
	V
OpenSBI is loaded by U-Boot, and jumps from M mode to S mode
	|
	V
Linux is loaded by U-Boot, and runs in S mode
```

So if U-Boot loads a zCore kernel on its own, there is no OpenSBI to print
with, and the result is no output at all. With the problem located, the fix
followed.

Running opensbi on its own from u-boot did not print the usual big "OPENSBI"
logo, so opensbi's serial port was fixed: the driver did not recognise the C910
Light board's model, `snps,dw-apb-uart`, and fell back to the generic
`ns16550`. This patch was applied:
https://github.com/elliott10/opensbi/commit/404951dd5b047873fa023545eafeb1fa2a9c5838

Built and run, OpenSBI's output works!

### 3. The zCore virtual memory page table problem

zCore carried on and hit a new problem: it stopped right after switching page
tables (`switch table`).

It was a very odd problem: in the normal flow, all the sections of the zCore
kernel and the other address spaces are mapped here into a newly created page
table, with the same parameters and process as on other boards such as qemu or
D1. In theory there was no reason for a bug. It stopped here:

```
[ 48.117274 INFO  0 0:0 kernel_hal::imp::arch::vm] initialized kernel page table @ 0x5c18000
[ 48.128044 DEBUG 0 0:0 kernel_hal::imp::arch::vm] cpu 0 switch table 2ec000 -> 5c18000
```

The JTAG debugger came out again to follow the CPU instruction by instruction
once it got here.

With a breakpoint before the `switch table` and single stepping, it turned out
that after `csrw satp` the program ran wild and raised an exception. Suspicion
fell on the page table satp was using.

Linux's code is an excellent reference for zCore development, so Linux's
process for switching page tables and writing satp was compared, and some
leads turned up:

<img src="img/c910-linux-pg.png" alt="c910 pagetable" style="zoom:75%;" />

In Linux's riscv code, when building virtual memory page table entries:

the kernel image's address space, `PAGE_KERNEL`, gets extra flags compared with
an ordinary page: `CACHE`, `SHARE`, `BUF`;

the device address space, `PAGE_IOREMAP`, gets the extra attributes `SHARE`,
`SO`.

The definition of these extra entry bits is in the C910 chip manual: the extra
page attributes are in bits `63:59` and include `SO`, `CACHE`, `BUF`, `SHARE`
and `SEC`.

According to other chip manuals, these T-Head CPU extended attributes do not
exist only on the C910: the C906 CPU has them as well.

![c910 pte flags](img/c910-pte-flags.png)

zCore first tried setting the kernel's `CACHE`, `SHARE` and `BUF` bits when
building the kernel page table, but the problem remained: it still stopped
after the `switch table`, for unknown reasons.

If these are bits of a custom extension on top of the RISC-V standard, in
theory it should be possible to turn them off, so the chip manual was searched
further and, sure enough, the matching bit, `MAEE`, was in the extended status
register `MXSTATUS`.

![c910 mxstatus](img/c910-mxstatus.png)

The C910 CPU's MAEE extension bit decides whether the MMU's extended address
attributes are enabled, that is `SO`, `CACHE`, `BUF`, `SHARE` and `SEC`.

With `MAEE = 0`, the MMU does not use the extended attributes `SO`, `CACHE`,
`BUF`, `SHARE` and `SEC`;

with `MAEE = 1`, the extended attributes `SO`, `CACHE`, `BUF`, `SHARE` and
`SEC` of the MMU's PTEs are enabled, and the matching bits can be set in each
entry.

![c910 mxstatus](img/c910-maee.jpeg)

So the page table entries' extended attributes were simply turned off, with
`MAEE = 0`, and indeed the problem of stopping at the `switch table` went away.
zCore could carry on and start parsing the device tree's nodes; next it ran
into device addresses that were too high, which caused errors when processing
the nodes' addresses...

### 4. Trying the MAEE setting for PTE extended attributes on the CR1825 board

In an earlier conversation with DF about the bug the CR1825 had at the time, the
first suggestion was that the kernel's `.BSS` segment might not be fully
cleared.

The year before, when the zCore freshly extended with riscv support was ported
to D1, there had also been a deadlock with the spinlock's atomic instructions.
JYK was asked about it back then and gave the lead: the `.bss` segment was not
cleared properly, so the uninitialised atomic variables held random values and
the deadlock was permanent.

<img src="img/c906-clear-bss.jpeg" alt="c906 clear bsss" style="zoom:50%;" />



In any case, clearing `.bss` was added soon after. Although the boot code was
later rewritten, DF confirmed that `.bss` was indeed being cleared.

While debugging the C910, studying the page attributes and the related register
bits in the chip manual, the C906 manual was compared too, and the two CPUs
barely differ in those attributes.

After fixing the C910 Light's stop at the `switch table`, DF was contacted and
the source code of the CR1825's opensbi, u-boot, Linux and so on was obtained,
mainly to compare it with the C910 Light's boot code.

The boot code of the CR1825, with a C906 CPU, turned out to be very similar to
that of the Light board with a C910 CPU, including the process of building
virtual memory: it also set the extended bits `cache`, `share` and `buf` in the
kernel's virtual memory entries, whereas the original zCore never set those
bits.

Hence the guess that the PTE extended-bit setting might have a lot to do with
the CR1825's atomic lock problem: an atomic operation has to check and rewrite
a value in virtual memory.

To verify it, a CR1825 boot image `fip.bin` was built that simply turns off the
page table entries' extended attributes, with `MAEE = 0`; the change is this:
https://github.com/elliott10/opensbi/commit/404951dd5b047873fa023545eafeb1fa2a9c5838

DF was asked to test it remotely and, luckily, the atomic lock problem was
solved too: it really was related to T-Head's extended page table entries!

<img src="img/c906-amo-1.jpg" alt="c906 amo 1" style="zoom:50%;" />

<img src="img/c906-amo-2.jpg" alt="c906 amo 1" style="zoom:50%;" />

Afterwards, in the group chat, the discussion with YDR and others about how
T-Head's extended bits work went on, and a T-Head engineer confirmed: "The
cacheable bit in the PTE means the page can be cached by the 906's cache; if
it is 0 it does not enter the cache. The bufferable bit in the PTE controls
whether the bufferable signal is raised on bus transfers; the CPU does not care
about it in its behaviour". And: "amo instructions need cache support, that is,
they cannot be used in non-cached regions".

Once the CR1825 lock-up was solved, the rest went fairly smoothly.

With `maee` on, keep in mind that the extra `PTE` attributes differ between the
kernel image and the device address space.

According to Linux's code, when building virtual memory page table entries:

the kernel image's address space, `PAGE_KERNEL`, gets extra flags compared with
an ordinary page: `CACHE`, `SHARE`, `BUF`;

the device address space, `PAGE_IOREMAP`, gets the extra attributes `SHARE`,
`SO`.

### 5. Device addresses that are too high

As mentioned above, after building virtual memory, while parsing the device
tree's nodes zCore runs into device addresses that are too high, which cause
errors when processing the nodes' addresses.

For example, the serial port base address the device tree gives is
`0xffe7014000`, larger than a typical device address such as Qemu's serial base
address, `0x10000000`. Several device base addresses on the C910 Light board
are this large. In the physical address space these addresses cause no
trouble, but once zCore has built the kernel's virtual memory and adds a common
`offset` to them, such as `0xffffffe000200000`, they overflow the SV39 virtual
address space.

A simple treatment is enough for the overflow: set the overflowed reserved bits
back to `1`:

```
paddr | (0x1ffffff << 39)
```

### 6. zCore running on the C910 Light board

zCore reaches the busybox shell without trouble:

![c910 zcore](img/c910-zcore-run.png)
