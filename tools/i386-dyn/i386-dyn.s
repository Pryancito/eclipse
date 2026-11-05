/* `/bin/i386-dyn`: a 32-bit program with a `PT_INTERP`, whose ELF header and
 * program headers are written out by hand.
 *
 * By hand because there is no other way to get one: `ld` drops
 * `--dynamic-linker` when the output needs no dynamic linking, and a real
 * dynamic i386 executable would need a 32-bit libc this image does not have.
 * What stage 4 is about is the KERNEL's half --- notice the `PT_INTERP`, load
 * what it names, build the program's stack and auxv, enter the interpreter ---
 * and that half is exercised by any `PT_INTERP` at all.
 *
 * Two program headers: the whole image as one RWX `PT_LOAD`, and the
 * `PT_INTERP` that names `/bin/i386-interp`. It is `ET_EXEC` at a fixed
 * address, so nothing here needs relocating, and the interpreter sits at
 * `0x0a00_0000` where it cannot overlap this.
 *
 * Linked with `--oformat binary`, so what `ld` writes out is these bytes and
 * nothing else; see the Makefile and `i386-dyn.ld`. Checked against this
 * container's Linux, which runs it: the pair prints `IP` and exits 0.
 *
 * It writes `P` so that a log can tell the two failures apart: `IP` is the
 * interpreter running and handing over, a bare `P` is a kernel that ignored
 * the `PT_INTERP` and ran the program itself, and a bare `I` is an
 * interpreter that could not hand over.
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
	.asciz	"/bin/i386-interp"
interp_end:
_start:
	movl	$4, %eax		/* write(1, "P", 1) */
	movl	$1, %ebx
	movl	$pmsg, %ecx
	movl	$1, %edx
	int	$0x80
	movl	$1, %eax		/* _exit(0) */
	xorl	%ebx, %ebx
	int	$0x80
pmsg:	.ascii	"P"
image_end:
