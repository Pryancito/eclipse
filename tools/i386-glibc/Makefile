# i386-glibc — the smallest program that proves Eclipse runs a 32-bit binary
# that is DYNAMICALLY linked. See the comment at the top of i386-glibc.s.
#
#   make LIBC32DIR=../../rootfs/x86_64/lib/i386-linux-gnu
#   make clean
#
# No cross toolchain and no 32-bit C compiler: the host's binutils can target
# i386, and what it links against is the `libc.so.6` that
# `apt_rootfs::multilib32` already staged into the rootfs. `xtask` builds it
# the same way (see `LinuxRootfs::i386_glibc`); this Makefile is for reading
# it by hand.
#
# `-rpath-link` is for the LINK only: it is how `ld` finds the
# `ld-linux.so.2` that `libc.so.6` itself needs in order to resolve its
# GLIBC_PRIVATE symbols. It puts no path in the binary, which names
# `/lib/ld-linux.so.2` and `libc.so.6` and nothing else.

AS ?= as
LD ?= ld
LIBC32DIR ?= ../../rootfs/x86_64/lib/i386-linux-gnu

i386-glibc: i386-glibc.o
	$(LD) -m elf_i386 -dynamic-linker /lib/ld-linux.so.2 -o $@ $< \
	      -rpath-link $(LIBC32DIR) $(LIBC32DIR)/libc.so.6

i386-glibc.o: i386-glibc.s
	$(AS) --32 -o $@ $<

clean:
	rm -f i386-glibc i386-glibc.o

.PHONY: clean
