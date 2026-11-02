/*
 * i386-hello: the smallest program that proves this kernel runs a 32-bit
 * binary, and the only one that can prove it without a 32-bit libc.
 *
 * It is an ELFCLASS32 / EM_386 static executable with no libc at all, so
 * everything it exercises is the kernel's own compatibility path and nothing
 * else:
 *
 *   - the loader accepted an ELF32 header and its 32-byte program headers;
 *   - the image, its stack and its break all landed below 4 GiB;
 *   - `iretq` put the CPU in compatibility mode -- these instructions are
 *     decoded as 32-bit ones, and `movl $msg, %ecx` would be a different
 *     instruction in 64-bit mode;
 *   - `int $0x80` reaches the kernel at all, which needs that IDT entry to
 *     have DPL 3;
 *   - the number in %eax was read as the i386 numbering (4 is `write`, not
 *     `stat`) and the arguments came out of %ebx/%ecx/%edx;
 *   - the initial stack was built out of 4-byte words, which is what reading
 *     argc at (%esp) and argv[0] at 4(%esp) checks.
 *
 * Every failure has its own exit status, so the run says which step broke
 * even when nothing was printed:
 *
 *   0   everything worked
 *   11  getpid returned something that is not a pid
 *   12  brk(0) returned no break
 *   13  argc at (%esp) is less than 1
 *   14  argv[0] is a null pointer
 *   15  write() did not write the whole message
 *   16  an ordinary load through %ds did not read what was stored there
 *   17  set_thread_area failed
 *   18  set_thread_area did not name one of the three slots
 *   19  %gs:0 is not the base of the thread area
 *   20  the thread area is not writable through %gs
 *   21  get_thread_area did not hand the base back
 *   22  mmap2 failed
 *   23  the page mmap2 returned is not readable and writable
 *   24  open(argv[0]) failed
 *   25  _llseek did not seek to where it was asked
 *   26  pread64 did not read the bytes at that offset
 *
 * The last six are the segment registers, which is the second half of what
 * compatibility mode needs and the half `iretq` does not do: it reloads `CS`
 * and `SS` and leaves `DS`, `ES`, `FS` and `GS` as the kernel left them. In
 * 64-bit mode nothing notices, because a data access ignores the segment; a
 * 32-bit thread addresses through `%ds` on every ordinary load and through
 * `%gs` for its TLS, so for it those four registers are the difference
 * between running and a `#GP` on the first instruction that touches memory.
 *
 * Assembled with the host's binutils (`as --32`, `ld -m elf_i386`): a
 * freestanding binary needs no cross toolchain, and the musl cross compiler
 * this tree downloads cannot target i386 anyway.
 */

	.section .text
	.globl _start
_start:
	/* getpid(): i386 20. A scalar in, a scalar out -- the simplest call
	 * there is, so if this one does not come back the fault is in the
	 * entry path and not in any argument. */
	movl	$20, %eax
	int	$0x80
	testl	%eax, %eax
	jle	bad_pid

	/* brk(0): i386 45. Returns the current break, which the loader placed
	 * at a quarter of the way up a 4 GiB address space. Zero would mean it
	 * was placed somewhere a 32-bit register cannot hold. */
	movl	$45, %eax
	xorl	%ebx, %ebx
	int	$0x80
	testl	%eax, %eax
	jz	bad_brk

	/* The initial stack, as a 32-bit process reads it: argc first, then
	 * the argv pointers, four bytes each. */
	movl	(%esp), %eax
	cmpl	$1, %eax
	jl	bad_argc
	movl	4(%esp), %eax
	testl	%eax, %eax
	jz	bad_argv

	/* write(1, msg, msglen): i386 4, where 4 is `stat` in the native
	 * numbering. Nothing else in this program would notice an untranslated
	 * number; this does. */
	movl	$4, %eax
	movl	$1, %ebx
	movl	$msg, %ecx
	movl	$msglen, %edx
	int	$0x80
	cmpl	$msglen, %eax
	jne	bad_write

	/* An ordinary load and store through %ds, which is every memory
	 * access a 32-bit program makes that is not on the stack. Nothing
	 * above this line used it: `(%esp)` goes through %ss, and the buffer
	 * `write` took was dereferenced by the kernel and not by this
	 * program. So this is the first instruction here that needs %ds to
	 * hold a segment at all. */
	movl	$0xd5d5d5, %eax
	movl	%eax, ds_probe
	movl	ds_probe, %eax
	cmpl	$0xd5d5d5, %eax
	jne	bad_ds

	/* set_thread_area(&tls_desc): i386 243, and the reason this program
	 * needs a .data section at all. The thread pointer of i386 is the
	 * BASE OF A SEGMENT, so asking for one means handing the kernel a
	 * `struct user_desc` and getting back the slot it put the descriptor
	 * in. `entry_number` is -1 on the way in: the slot numbers are not
	 * something a program can know. */
	movl	$243, %eax
	movl	$tls_desc, %ebx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_set_tls

	/* The kernel wrote the entry it chose back into `entry_number`, which
	 * is a GDT INDEX and not a number of the kernel's choosing: ring 3
	 * turns it into a selector itself, two instructions below. Linux's is
	 * 12, 13 or 14 (`GDT_ENTRY_TLS_MIN`); Eclipse starts from the
	 * firmware's table and appends, so its index is whatever is left. So
	 * what is checked here is not a value but that there IS one: -1 still
	 * in the field means nothing was chosen, and an index that does not
	 * fit the 13 bits of a selector could not be loaded. */
	movl	tls_desc, %eax
	cmpl	$-1, %eax
	je	bad_tls_entry
	cmpl	$0x2000, %eax
	jae	bad_tls_entry
	testl	%eax, %eax
	jz	bad_tls_entry

	/* selector = (entry_number << 3) | 3 -- an index into the GDT, with
	 * RPL 3 -- and loading it is what makes %gs mean this thread's area.
	 * The kernel never sees this instruction, which is why it has to
	 * remember the selector it just handed out: `iretq` will not put %gs
	 * back by itself. */
	shll	$3, %eax
	orl	$3, %eax
	movw	%ax, %gs

	/* %gs:0 is now the first word of the thread area, which is where a C
	 * library would keep the pointer to its own thread structure. */
	movl	%gs:0, %eax
	cmpl	$0xc0ffee, %eax
	jne	bad_tls_read

	/* And it has to be writable through the same segment: musl writes its
	 * TLS through %gs, so a descriptor that came out read-only would let
	 * a program start and then fail inside it. */
	movl	$0x1dea, %gs:4
	cmpl	$0x1dea, tls_scratch
	jne	bad_tls_write

	/* mmap2(NULL, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS,
	 * -1, 0): i386 192, and the first call here with SIX arguments, so the
	 * sixth goes in %ebp. `mmap`(90) is a different call that passes its
	 * arguments in a block of memory; this one is the one a 32-bit program
	 * actually uses, and its offset counts 4 KiB units rather than bytes.
	 *
	 * An error comes back as -1..-4095, which is why the compare is
	 * unsigned: a valid mapping can have the top bit set. */
	movl	$192, %eax
	xorl	%ebx, %ebx
	movl	$4096, %ecx
	movl	$3, %edx
	movl	$0x22, %esi
	movl	$-1, %edi
	xorl	%ebp, %ebp
	int	$0x80
	cmpl	$-4095, %eax
	jae	bad_mmap2
	movl	%eax, page

	/* And the page has to be the memory it says it is. */
	movl	page, %eax
	movl	$0x5a5a5a, (%eax)
	cmpl	$0x5a5a5a, (%eax)
	jne	bad_mmap2_rw

	/* open(argv[0], O_RDONLY): this program's own file, which is the one
	 * file it knows the name of. */
	movl	$5, %eax
	movl	4(%esp), %ebx
	xorl	%ecx, %ecx
	xorl	%edx, %edx
	int	$0x80
	cmpl	$-4095, %eax
	jae	bad_open
	movl	%eax, fd

	/* _llseek(fd, 0, 1, page, SEEK_SET): i386 140, whose offset arrives
	 * HIGH FIRST -- it is the one call in this family that does -- and
	 * whose result goes to a pointer, because a 32-bit return value could
	 * not hold a file offset. The pointer is the page mmap2 just handed
	 * over, so this also proves the kernel can write to it.
	 *
	 * Seeking to 1 and then reading three bytes is a check with teeth: an
	 * ELF file reads "ELF" at offset 1, so an offset that did not take
	 * effect reads the 0x7f of the magic instead. */
	movl	$140, %eax
	movl	fd, %ebx
	xorl	%ecx, %ecx
	movl	$1, %edx
	movl	page, %esi
	xorl	%edi, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_llseek
	movl	page, %eax
	cmpl	$1, (%eax)
	jne	bad_llseek
	movl	4(%eax), %eax
	testl	%eax, %eax
	jnz	bad_llseek

	movl	$3, %eax
	movl	fd, %ebx
	movl	page, %ecx
	movl	$3, %edx
	int	$0x80
	cmpl	$3, %eax
	jne	bad_llseek
	movl	page, %eax
	movl	(%eax), %eax
	andl	$0xffffff, %eax
	cmpl	$0x464c45, %eax		/* "ELF", little endian */
	jne	bad_llseek

	/* pread64(fd, page, 3, 1, 0): i386 180, whose offset arrives LOW
	 * FIRST -- the opposite order to _llseek, which is why each one says
	 * so where it is used. It reads at an offset without moving the file
	 * position, so asking for offset 1 again has to give "ELF" again
	 * whatever the seek above left behind. */
	movl	page, %eax
	movl	$0, (%eax)
	movl	$180, %eax
	movl	fd, %ebx
	movl	page, %ecx
	movl	$3, %edx
	movl	$1, %esi
	xorl	%edi, %edi
	int	$0x80
	cmpl	$3, %eax
	jne	bad_pread
	movl	page, %eax
	movl	(%eax), %eax
	andl	$0xffffff, %eax
	cmpl	$0x464c45, %eax
	jne	bad_pread

	/* get_thread_area(&tls_desc): i386 244, the other half, and the one a
	 * debugger uses. It has to hand back the base this program asked for,
	 * which means the descriptor really does hold it rather than
	 * something that merely works for %gs:0. */
	movl	$244, %eax
	movl	$tls_desc, %ebx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_get_tls
	movl	tls_desc + 4, %eax
	cmpl	$tls_area, %eax
	jne	bad_get_tls

	xorl	%ebx, %ebx
	jmp	die

bad_pid:
	movl	$11, %ebx
	jmp	die
bad_brk:
	movl	$12, %ebx
	jmp	die
bad_argc:
	movl	$13, %ebx
	jmp	die
bad_argv:
	movl	$14, %ebx
	jmp	die
bad_write:
	movl	$15, %ebx
	jmp	die
bad_ds:
	movl	$16, %ebx
	jmp	die
bad_set_tls:
	movl	$17, %ebx
	jmp	die
bad_tls_entry:
	movl	$18, %ebx
	jmp	die
bad_tls_read:
	movl	$19, %ebx
	jmp	die
bad_tls_write:
	movl	$20, %ebx
	jmp	die
bad_get_tls:
	movl	$21, %ebx
	jmp	die
bad_mmap2:
	movl	$22, %ebx
	jmp	die
bad_mmap2_rw:
	movl	$23, %ebx
	jmp	die
bad_open:
	movl	$24, %ebx
	jmp	die
bad_llseek:
	movl	$25, %ebx
	jmp	die
bad_pread:
	movl	$26, %ebx

	/* exit_group(status): i386 252. `exit` (1) would do for a single
	 * thread, but exit_group is what a C library calls and it is in the
	 * same allowlist. */
die:
	movl	$252, %eax
	int	$0x80
	/* exit_group does not return. If it ever did, fault rather than run
	 * off into whatever follows. */
	hlt

	.section .rodata
msg:
	.ascii	"i386-hello: compatibility mode works\n"
	.set	msglen, . - msg

	.section .data
	.align 4
	/* The `struct user_desc` of set_thread_area(2), sixteen bytes: the
	 * slot, the base, the limit, and a word of one-bit flags. This one
	 * asks for what musl asks for -- a flat, 4 GiB, 32-bit, writable data
	 * segment whose base is the thread pointer. The flags are
	 * seg_32bit(bit 0) | limit_in_pages(bit 4) | useable(bit 6). */
tls_desc:
	.long	-1
	.long	tls_area
	.long	0xfffff
	.long	0x51
	/* The thread area itself: a word to read through %gs:0 and a word to
	 * write through %gs:4. In a real program this is where the C library
	 * keeps its per-thread structure. */
	.align 4
tls_area:
	.long	0xc0ffee
tls_scratch:
	.long	0
	/* And a word that is reached through %ds, like every ordinary global. */
ds_probe:
	.long	0
	/* The page mmap2 hands over, and the descriptor of this program's own
	 * file. Kept in memory rather than in registers because the calls
	 * below need five and six arguments, and there are not enough
	 * registers left over to hold these too. */
page:
	.long	0
fd:
	.long	0
