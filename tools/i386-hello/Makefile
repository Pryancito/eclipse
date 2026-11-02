# i386-hello — the smallest program that proves Eclipse runs a 32-bit binary.
#
# Freestanding: an ELFCLASS32 / EM_386 static executable with no libc, so what
# it exercises is the kernel's compatibility path and nothing else. That also
# means it needs no cross toolchain -- just an assembler and a linker that can
# target i386, which the host binutils can:
#
#   make                  # i386-hello, ready for the rootfs
#   make clean
#
# `xtask` builds it the same way (see `LinuxRootfs::i386_hello`); this Makefile
# is for reading it by hand.
#
# Run it on a Linux host first. Every check in it is one Linux passes, so a
# non-zero status there is a bug in the probe and a non-zero status only on
# Eclipse is a bug in Eclipse. The status says which step failed -- see the
# comment at the top of i386-hello.s.

AS ?= as
LD ?= ld

i386-hello: i386-hello.o
	$(LD) -m elf_i386 -static -o $@ $<

i386-hello.o: i386-hello.s
	$(AS) --32 -o $@ $<

clean:
	rm -f i386-hello i386-hello.o

.PHONY: clean
