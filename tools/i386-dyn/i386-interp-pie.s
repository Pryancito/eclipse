/* `/bin/i386-interp-pie`: a `PT_INTERP` interpreter that is `ET_DYN`, which is
 * what every real `ld.so` is, and a probe of the one thing that distinguishes
 * that case from the `ET_EXEC` one its sibling covers.
 *
 * `i386-interp.s` is `ET_EXEC`: it is loaded at the absolute addresses its
 * header asks for and `AT_BASE` comes through as zero. This one carries no
 * absolute address at all --- its `p_vaddr` is 0 and the kernel puts it
 * wherever it likes --- so the kernel owes it two things that the other case
 * never tests:
 *
 *   1. a bias, applied to every segment, and
 *   2. `AT_BASE` equal to that bias, which is the ONLY way the interpreter can
 *      find its own image.
 *
 * So the check that matters here is not that `AT_BASE` holds some number: it
 * is that `AT_BASE` is the number where this code actually ended up. A kernel
 * that relocated the image and then reported a stale or zero `AT_BASE` would
 * pass every check the `ET_EXEC` probe makes and still break every real
 * `ld.so`, which reads its own dynamic section through exactly that value.
 * This program works out where it is by itself (`call`/`pop`, the only way a
 * 32-bit x86 image has: there is no RIP-relative addressing) and compares.
 *
 * Being `ET_DYN` is also what forces the shape of the code: every reference to
 * its own data goes through the base in `%ebx`, because an absolute one would
 * name the link-time address --- an offset from zero --- and read whatever is
 * mapped down there instead.
 *
 * Run on this container's Linux (6.18.44-fc-v233), this pair prints `DQ` and
 * exits 0, which is every check below passing --- `AT_BASE` included, so on a
 * real kernel `AT_BASE` is the bias and not something else that happens to be
 * non-zero. The values the checks hold it to are the sibling's measured ones
 * (`AT_PHDR` 0x08048034, `AT_PHENT` 32, `AT_PHNUM` 2, `AT_PAGESZ` 0x1000,
 * `AT_ENTRY` the program's), because the program is the same shape as its.
 *
 * Deliberately NOT a fixed number: `AT_BASE` itself. Linux puts an `ET_DYN`
 * interpreter at a different address on every run --- three consecutive runs
 * here, with a variant that printed it, gave 0xf7f63000, 0xf7f53000 and
 * 0xf7ff2000 --- so quoting one measured value would be quoting an accident.
 * That is the whole reason the check is "equal to where this code is" rather
 * than "equal to 0xf7f63000".
 *
 * `argv[0]` is the PROGRAM's path, as for any `PT_INTERP` interpreter.
 *
 * Its header and its single program header are written by hand, like its
 * sibling's: linking this with `-shared` would get an `ET_DYN` file, but also
 * a `PT_DYNAMIC`, a symbol table and relocations --- the dynamic linker's own
 * furniture, none of which the KERNEL reads. What the kernel is held to is the
 * load and the hand-over, and this is the smallest file that asks for both.
 *
 * Exit codes, so a boot log can say which check fell:
 *   40  no usable AT_ENTRY
 *   41  AT_PHDR does not point at the program's headers
 *   42  AT_BASE is zero, so nothing was relocated
 *   43  AT_BASE is not where this code actually is  <-- the one this adds
 *   44  argv[0] is not the program's path
 *   45  AT_PHENT/AT_PHNUM are not an ELF32 header table
 *   46  AT_PAGESZ is not 4096
 * and on success it writes `D` and jumps, so the pair prints `DQ` and exits 0.
 */
	.code32
	.section .text
	.globl _start

/* What the program's own header says, which is what AT_PHDR has to describe. */
PROG_BASE  = 0x08048000
PROG_PHOFF = 0x34

ehdr:
	.byte	0x7f
	.ascii	"ELF"
	.byte	1			/* ELFCLASS32 */
	.byte	1			/* ELFDATA2LSB */
	.byte	1			/* EV_CURRENT */
	.byte	0			/* ELFOSABI_SYSV */
	.quad	0			/* EI_ABIVERSION and the padding */
	.word	3			/* ET_DYN: the whole point */
	.word	3			/* EM_386 */
	.long	1			/* e_version */
	.long	_start - ehdr		/* e_entry, an offset: there is no base */
	.long	phdrs - ehdr		/* e_phoff */
	.long	0			/* e_shoff: no sections at all */
	.long	0			/* e_flags */
	.word	52			/* e_ehsize */
	.word	32			/* e_phentsize */
	.word	1			/* e_phnum */
	.word	40			/* e_shentsize */
	.word	0			/* e_shnum */
	.word	0			/* e_shstrndx */
phdrs:
	.long	1			/* PT_LOAD */
	.long	0			/* p_offset */
	.long	0			/* p_vaddr: relocate me */
	.long	0			/* p_paddr */
	.long	image_end - ehdr	/* p_filesz */
	.long	image_end - ehdr	/* p_memsz */
	.long	7			/* R | W | X */
	.long	0x1000

_start:
	movl	%esp, %ebp		/* the handover stack, kept untouched */

	/* Where am I? `call` pushes the address of the next instruction, so
	 * popping it gives the runtime address of `here`; its link-time value
	 * is its offset from the start of the image, because this is linked at
	 * zero. The difference is the base the kernel chose. */
	call	here
here:
	popl	%ebx
	subl	$here, %ebx		/* ebx = the real load base */

	/* Relocated at all? A base of zero would mean this ET_DYN image was
	 * loaded as if it were ET_EXEC, at the link-time addresses. */
	testl	%ebx, %ebx
	je	bad_base_zero

	/* argv[0]: the program's path, not this interpreter's. */
	movl	4(%ebp), %esi		/* argv[0] */
	leal	progpath(%ebx), %edi
	call	streq
	testl	%eax, %eax
	je	bad_argv

	/* Walk to the auxiliary vector: argc, argv[], NULL, envp[], NULL. */
	movl	%ebp, %esi
	movl	(%esi), %eax		/* argc */
	leal	4(%esi, %eax, 4), %esi	/* -> argv's NULL */
	addl	$4, %esi		/* -> envp[0] */
1:	cmpl	$0, (%esi)
	je	2f
	addl	$4, %esi
	jmp	1b
2:	addl	$4, %esi		/* -> the auxv pairs */
	movl	%esi, %edi		/* keep the auxv for the lookups */

	/* AT_PAGESZ (6) */
	pushl	$6
	call	auxval
	cmpl	$0x1000, %eax
	jne	bad_pagesz

	/* AT_BASE (7): the bias, and it has to be the bias this code is at.
	 * Not merely non-zero: a stale or invented value passes that and
	 * breaks every real ld.so, which finds its own dynamic section
	 * through it. */
	pushl	$7
	call	auxval
	testl	%eax, %eax
	je	bad_base_zero
	cmpl	%ebx, %eax
	jne	bad_base_wrong

	/* AT_PHENT (4) and AT_PHNUM (5): the PROGRAM's table, 32-byte entries. */
	pushl	$4
	call	auxval
	cmpl	$32, %eax
	jne	bad_phent
	pushl	$5
	call	auxval
	cmpl	$2, %eax
	jne	bad_phent

	/* AT_PHDR (3): the program's headers, where its own header put them. */
	pushl	$3
	call	auxval
	cmpl	$PROG_BASE + PROG_PHOFF, %eax
	jne	bad_phdr
	cmpl	$1, (%eax)		/* its first is the PT_LOAD ... */
	jne	bad_phdr
	cmpl	$PROG_BASE, 8(%eax)	/* ... at the address it was linked for */
	jne	bad_phdr

	/* Say we ran, before the jump: a kernel that ignored the PT_INTERP
	 * would run the program directly and print only `Q`. The base has to
	 * survive the call, because `write` wants its fd in ebx. */
	leal	dmsg(%ebx), %ecx
	pushl	%ebx
	movl	$4, %eax		/* write */
	movl	$1, %ebx
	movl	$1, %edx
	int	$0x80
	popl	%ebx

	/* AT_ENTRY (9), and into the program with the stack as handed over. */
	pushl	$9
	call	auxval
	testl	%eax, %eax
	je	bad_entry
	movl	%ebp, %esp
	jmp	*%eax

/* The value of auxv type on the stack, or 0. edi -> the auxv pairs.
 * Takes its argument on the stack and pops it, so each call site is two
 * instructions and esp comes back where it was. Leaves ebx alone. */
auxval:
	movl	4(%esp), %ecx		/* the type asked for */
	movl	%edi, %edx
1:	movl	(%edx), %eax
	testl	%eax, %eax
	je	2f
	cmpl	%ecx, %eax
	je	3f
	addl	$8, %edx
	jmp	1b
2:	xorl	%eax, %eax		/* AT_NULL: not there */
	jmp	4f
3:	movl	4(%edx), %eax
4:	movl	(%esp), %ecx		/* return address */
	addl	$8, %esp		/* pop it and the argument */
	jmp	*%ecx

/* esi and edi: two NUL-terminated strings. eax = 1 if equal. */
streq:
	pushl	%esi
	pushl	%edi
1:	movb	(%esi), %al
	movb	(%edi), %ah
	cmpb	%ah, %al
	jne	2f
	testb	%al, %al
	je	3f
	incl	%esi
	incl	%edi
	jmp	1b
2:	popl	%edi
	popl	%esi
	xorl	%eax, %eax
	ret
3:	popl	%edi
	popl	%esi
	movl	$1, %eax
	ret

bad_entry:
	movl	$40, %ebx
	jmp	leave
bad_phdr:
	movl	$41, %ebx
	jmp	leave
bad_base_zero:
	movl	$42, %ebx
	jmp	leave
bad_base_wrong:
	movl	$43, %ebx
	jmp	leave
bad_argv:
	movl	$44, %ebx
	jmp	leave
bad_phent:
	movl	$45, %ebx
	jmp	leave
bad_pagesz:
	movl	$46, %ebx
leave:
	movl	$1, %eax		/* exit */
	int	$0x80

dmsg:		.ascii	"D"
progpath:	.asciz	"/bin/i386-pie"
image_end:
