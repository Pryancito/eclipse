# The two `PT_INTERP` pairs that prove Eclipse runs a 32-bit program with a
# `PT_INTERP`, which is the kernel's half of stage 4. Two pairs because the
# kernel has two paths and a boot that runs one says nothing about the other:
# `i386-dyn`/`i386-interp`, whose interpreter is `ET_EXEC` (absolute vaddrs,
# `AT_BASE` zero), and `i386-pie`/`i386-interp-pie`, whose interpreter is
# `ET_DYN` (relocated, `AT_BASE` the bias) --- which is what every real
# `ld.so` is.
#
# `i386-dyn` is an ELFCLASS32 / EM_386 executable whose ELF header and program
# headers are assembled by hand and written out with `--oformat binary`,
# because `ld` drops `--dynamic-linker` when the output needs no dynamic
# linking. `i386-interp` is the interpreter it names: a static i386 executable
# at a fixed address that checks what the kernel handed it and jumps to
# `AT_ENTRY`. `i386-interp-pie` does the same from an `ET_DYN` image, and adds
# the check that case is for: that `AT_BASE` is the address its own code
# actually ended up at, which it works out with `call`/`pop`.
#
#   make                  # all four, ready for the rootfs
#   make clean
#
# `xtask` builds them the same way (see `LinuxRootfs::i386_dyn_pairs`); this
# Makefile is for reading them by hand.
#
# Run them on a Linux host first, with the interpreter installed where the
# program names it:
#
#   make
#   cp i386-interp i386-interp-pie /bin/
#   cp i386-dyn i386-pie /bin/
#   /bin/i386-dyn          # prints IP and exits 0
#   /bin/i386-pie          # prints DQ and exits 0
#
# Every check is one Linux passes, so a non-zero status there is a bug in the
# probe and a non-zero status only on Eclipse is a bug in Eclipse. The status
# says which check fell -- see the comments at the top of i386-interp.s (30-35)
# and i386-interp-pie.s (40-46), which use different ranges on purpose.

AS ?= as
LD ?= ld

all: i386-dyn i386-interp i386-pie i386-interp-pie

# `--oformat binary`: the bytes assembled ARE the image, headers and all.
i386-dyn: i386-dyn.o i386-dyn.ld
	$(LD) -m elf_i386 -T i386-dyn.ld --oformat binary -o $@ $<

i386-interp: i386-interp.o
	$(LD) -m elf_i386 -static -Ttext 0x0a000000 -o $@ $<

i386-pie: i386-pie.o i386-dyn.ld
	$(LD) -m elf_i386 -T i386-dyn.ld --oformat binary -o $@ $<

# Linked at zero: it is ET_DYN, so its addresses are offsets and the kernel
# biases them. See i386-pie.ld.
i386-interp-pie: i386-interp-pie.o i386-pie.ld
	$(LD) -m elf_i386 -T i386-pie.ld --oformat binary -o $@ $<

%.o: %.s
	$(AS) --32 -o $@ $<

clean:
	rm -f i386-dyn i386-dyn.o i386-interp i386-interp.o \
	      i386-pie i386-pie.o i386-interp-pie i386-interp-pie.o

.PHONY: all clean
