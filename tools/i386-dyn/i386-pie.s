/* `/bin/i386-pie`: the second half of stage 4 --- a 32-bit program whose
 * `PT_INTERP` names an interpreter that is `ET_DYN`, which is what every real
 * `ld.so` is.
 *
 * Identical to `i386-dyn.s` but for two things: the path in the `PT_INTERP`
 * segment, and the letter it writes. Kept as its own file rather than as an
 * assembler switch over that one, because the program a `PT_INTERP` names it
 * by an absolute path and the two pairs have to be installable side by side:
 * the `ET_EXEC` interpreter (`AT_BASE` zero, absolute vaddrs honoured) and the
 * `ET_DYN` one (`AT_BASE` the bias, everything relocated) are different
 * kernel paths, and a boot that runs only one of them says nothing about the
 * other.
 *
 * Its header and its two program headers are written out by hand for the same
 * reason as the other one's: `ld` drops `--dynamic-linker` when the output
 * needs no dynamic linking, and there is no 32-bit libc in this image to need
 * one, so there is no way to ask for a bare `PT_INTERP`.
 *
 * It writes `Q`, so a boot log tells the two pairs apart: `DQ` is the
 * interpreter and then the program, and a bare `Q` would mean the kernel
 * ignored the `PT_INTERP` and ran the program itself.
 */
	.code32
	.section .text
BASE = 0x08048000
ehdr:
	.byte	0x7f
	.ascii	"ELF"
	.byte	1			/* ELFCLASS32 */
	.byte	1			/* ELFDATA2LSB */
	.byte	1			/* EV_CURRENT */
	.byte	0			/* ELFOSABI_SYSV */
	.quad	0			/* EI_ABIVERSION and the padding */
	.word	2			/* ET_EXEC */
	.word	3			/* EM_386 */
	.long	1			/* e_version */
	.long	_start			/* e_entry */
	.long	phdrs - ehdr		/* e_phoff: 0x34, as AT_PHDR says */
	.long	0			/* e_shoff: no sections at all */
	.long	0			/* e_flags */
	.word	52			/* e_ehsize */
	.word	32			/* e_phentsize */
	.word	2			/* e_phnum */
	.word	40			/* e_shentsize */
	.word	0			/* e_shnum */
	.word	0			/* e_shstrndx */
phdrs:
	.long	1			/* PT_LOAD */
	.long	0			/* p_offset */
	.long	BASE			/* p_vaddr */
	.long	BASE			/* p_paddr */
	.long	image_end - ehdr	/* p_filesz */
	.long	image_end - ehdr	/* p_memsz */
	.long	7			/* R | W | X */
	.long	0x1000
	.long	3			/* PT_INTERP */
	.long	interp_path - ehdr
	.long	interp_path
	.long	interp_path
	.long	interp_end - interp_path
	.long	interp_end - interp_path
	.long	4			/* R */
	.long	1
interp_path:
	.asciz	"/bin/i386-interp-pie"
interp_end:
_start:
	movl	$4, %eax		/* write(1, "Q", 1) */
	movl	$1, %ebx
	movl	$qmsg, %ecx
	movl	$1, %edx
	int	$0x80
	movl	$1, %eax		/* _exit(0) */
	xorl	%ebx, %ebx
	int	$0x80
qmsg:	.ascii	"Q"
image_end:
