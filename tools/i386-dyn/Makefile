# i386-dyn and i386-interp — the pair that proves Eclipse runs a 32-bit
# program with a `PT_INTERP`, which is the kernel's half of stage 4.
#
# `i386-dyn` is an ELFCLASS32 / EM_386 executable whose ELF header and program
# headers are assembled by hand and written out with `--oformat binary`,
# because `ld` drops `--dynamic-linker` when the output needs no dynamic
# linking. `i386-interp` is the interpreter it names: a static i386 executable
# at a fixed address that checks what the kernel handed it and jumps to
# `AT_ENTRY`.
#
#   make                  # both, ready for the rootfs
#   make clean
#
# `xtask` builds them the same way (see `LinuxRootfs::i386_dyn`); this
# Makefile is for reading them by hand.
#
# Run them on a Linux host first, with the interpreter installed where the
# program names it:
#
#   make && cp i386-interp /bin/i386-interp && cp i386-dyn /bin/i386-dyn
#   /bin/i386-dyn          # prints IP and exits 0
#
# Every check is one Linux passes, so a non-zero status there is a bug in the
# probe and a non-zero status only on Eclipse is a bug in Eclipse. The status
# says which check fell -- see the comment at the top of i386-interp.s.

AS ?= as
LD ?= ld

all: i386-dyn i386-interp

# `--oformat binary`: the bytes assembled ARE the image, headers and all.
i386-dyn: i386-dyn.o i386-dyn.ld
	$(LD) -m elf_i386 -T i386-dyn.ld --oformat binary -o $@ $<

i386-interp: i386-interp.o
	$(LD) -m elf_i386 -static -Ttext 0x0a000000 -o $@ $<

%.o: %.s
	$(AS) --32 -o $@ $<

clean:
	rm -f i386-dyn i386-dyn.o i386-interp i386-interp.o

.PHONY: all clean
