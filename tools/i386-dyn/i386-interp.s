/* `/bin/i386-interp`: the smallest thing that is really a `PT_INTERP`
 * interpreter, and a probe of what the kernel hands one.
 *
 * Stage 4 of the 32-bit compat is "a DYNAMIC i386 program runs", and the
 * kernel's half of that has nothing to do with dynamic linking: it has to
 * notice the `PT_INTERP`, load the interpreter it names, build the stack and
 * the auxiliary vector for the PROGRAM, and enter the INTERPRETER. Everything
 * after that is the interpreter's own business. So the interpreter here does
 * only the one thing a kernel can be held to: it checks what it was handed,
 * says it ran, and jumps to `AT_ENTRY`.
 *
 * There is no `/lib/ld-musl-i386.so.1` in this image --- the rootfs is 64-bit
 * --- and waiting for one would mean not testing the kernel at all. A real
 * musl would do strictly more than this, never less.
 *
 * Every value it insists on was MEASURED on this container's Linux
 * (6.18.44-fc-v233) with this same pair of binaries, by a version of this
 * program that printed the whole auxiliary vector instead of checking it:
 *
 *   03=08048034 (AT_PHDR, the program's own headers)
 *   04=00000020 (AT_PHENT: 32, an Elf32_Phdr, NOT 56)
 *   05=00000002 (AT_PHNUM)
 *   06=00001000 (AT_PAGESZ)
 *   07=00000000 (AT_BASE: ZERO, because this interpreter is ET_EXEC)
 *   09=08048085 (AT_ENTRY, the program's entry and not this one's)
 *   20=f7f235e0 (AT_SYSINFO)     21=f7f23000 (AT_SYSINFO_EHDR)
 *
 * and `argv[0]` is the PROGRAM's path: a `PT_INTERP` interpreter is NOT
 * pushed into `argv`, which is where it differs from a `#!` one.
 *
 * Exit codes, so a boot log can say which check fell:
 *   30  no usable AT_ENTRY
 *   31  AT_PHDR does not point at this program's headers
 *   32  AT_BASE is not zero for an ET_EXEC interpreter
 *   33  AT_PAGESZ is not 4096
 *   34  argv[0] is not the program's path
 *   35  AT_PHENT/AT_PHNUM are not an ELF32 header table
 * and on success it writes `I` and jumps, so the pair prints `IP` and exits 0.
 */
	.code32
	.section .text
	.globl _start

/* What the program's own header says, which is what AT_PHDR has to describe. */
PROG_BASE  = 0x08048000
PROG_PHOFF = 0x34

_start:
	movl	%esp, %ebp		/* the handover stack, kept untouched */

	/* argv[0] first, while esi is easy to work out. */
	movl	4(%ebp), %esi		/* argv[0] */
	movl	$progpath, %edi
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

	/* AT_BASE (7): zero, for an ET_EXEC interpreter. */
	pushl	$7
	call	auxval
	testl	%eax, %eax
	jne	bad_base

	/* AT_PHENT (4) and AT_PHNUM (5) */
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
	/* And they really are its headers: the first is the PT_LOAD of the
	 * image, at the address the program was linked for. */
	cmpl	$1, (%eax)
	jne	bad_phdr
	cmpl	$PROG_BASE, 8(%eax)
	jne	bad_phdr

	/* Say we ran, before the jump: a kernel that ignored the PT_INTERP
	 * altogether would run the program directly and print only `P`. */
	movl	$4, %eax		/* write */
	movl	$1, %ebx
	movl	$imsg, %ecx
	movl	$1, %edx
	int	$0x80

	/* AT_ENTRY (9), and into the program with the stack as handed over. */
	pushl	$9
	call	auxval
	testl	%eax, %eax
	je	bad_entry
	movl	%ebp, %esp
	jmp	*%eax

/* The value of auxv type on the stack, or 0. edi -> the auxv pairs.
 * Takes its argument on the stack and pops it, so each call site is two
 * instructions and esp comes back where it was. */
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
	movl	$30, %ebx
	jmp	leave
bad_phdr:
	movl	$31, %ebx
	jmp	leave
bad_base:
	movl	$32, %ebx
	jmp	leave
bad_pagesz:
	movl	$33, %ebx
	jmp	leave
bad_argv:
	movl	$34, %ebx
	jmp	leave
bad_phent:
	movl	$35, %ebx
leave:
	movl	$1, %eax		/* exit */
	int	$0x80

	.section .data
imsg:		.ascii	"I"
progpath:	.asciz	"/bin/i386-dyn"
