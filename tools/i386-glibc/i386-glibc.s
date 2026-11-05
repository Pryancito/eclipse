/* i386-glibc --- the smallest program that proves Eclipse runs a 32-bit
 * binary that is DYNAMICALLY linked.
 *
 * `i386-hello` and the `PT_INTERP` pairs next to it are freestanding: they
 * are ELFCLASS32 / EM_386 static executables with no libc, so what they
 * measure is the kernel's compatibility path and nothing else. That is the
 * right shape for measuring one system call at a time, and it leaves one
 * thing unmeasured --- the thing every real 32-bit program is:
 *
 *   PT_INTERP=/lib/ld-linux.so.2, DT_NEEDED libc.so.6
 *
 * so the kernel loads an interpreter, the interpreter opens and maps a real
 * shared library of two megabytes, installs a thread area, and only then is
 * there any program. Eighteen system calls happen before the first
 * instruction of this file runs, and all eighteen are the loader's.
 *
 * Which makes the shape of this probe different from the others: it does not
 * measure a call, it measures that a program STARTS. What is below is only
 * what checks that the start worked:
 *
 *   1. `write` through the PLT: a symbol the loader had to resolve, in a
 *      library it had to map, reached through a GOT it had to relocate.
 *   2. `getpid`, which is the same again for a call that answers something.
 *   3. `malloc` and `free`: libc's own data, so its `.data` relocations and
 *      its `brk`/`mmap2` work from inside the library and not from here.
 *   4. **`__errno_location`**, which is the one that matters: it reads the
 *      thread pointer out of `%gs`, so it answers correctly only if the
 *      kernel put the descriptor the loader asked for in the slot the
 *      loader named. A kernel that got `set_thread_area` nearly right
 *      returns rubbish here, and every errno in every 32-bit program is
 *      wrong afterwards.
 *   5. `exit`, which leaves through glibc rather than through `int $0x80`.
 *
 * Exit status:
 *
 *     0   it started and all five hold
 *    21   `write` through the PLT did not write what it was given
 *    22   `getpid` answered a pid that is not one
 *    23   `malloc` gave nothing, or what it gave does not hold a word
 *    24   `__errno_location` does not point at this thread's errno
 *   127   from the SHELL, not from here: the interpreter could not be
 *         resolved, which is what "/lib/ld-linux.so.2 missing" looks like
 *   132   from the shell: SIGILL, the usual shape of a GOT that was
 *         relocated wrong
 *
 * The failures leave by `exit_group` (252) directly, never through libc: a
 * probe that needs the thing it is testing in order to report cannot report.
 *
 * Built with the HOST's binutils and linked against the i386 `libc.so.6`
 * that `apt_rootfs::multilib32` stages into the rootfs --- no cross
 * toolchain and no 32-bit C compiler, exactly like `i386-hello`. See
 * `LinuxRootfs::i386_glibc`.
 *
 * Run it on a Linux host first: every check is one Linux passes.
 */

	.text
	.globl	_start
_start:
	/* 1. write(1, msg, msglen) */
	pushl	$msglen
	pushl	$msg
	pushl	$1
	call	write
	addl	$12, %esp
	cmpl	$msglen, %eax
	jne	bad_write

	/* 2. getpid() */
	call	getpid
	testl	%eax, %eax
	jle	bad_getpid

	/* 3. malloc(64), a word through it, free() */
	pushl	$64
	call	malloc
	addl	$4, %esp
	testl	%eax, %eax
	jz	bad_malloc
	movl	%eax, %ebx
	movl	$0x1386d1f, (%ebx)
	cmpl	$0x1386d1f, (%ebx)
	jne	bad_malloc
	pushl	%ebx
	call	free
	addl	$4, %esp

	/* 4. errno, which lives in thread-local storage: `close(-1)` is
	 * EBADF (9) and nothing else, so the value is known and a thread
	 * pointer that reads somewhere else cannot happen to be right. */
	pushl	$-1
	call	close
	addl	$4, %esp
	cmpl	$-1, %eax
	jne	bad_errno
	call	__errno_location
	testl	%eax, %eax
	jz	bad_errno
	cmpl	$9, (%eax)
	jne	bad_errno

	/* 5. and out through glibc. */
	pushl	$0
	call	exit
	/* `exit` does not return; if it did, that is not a success. */
	jmp	bad_errno

/* Every failure leaves by the raw system call: reporting a broken libc
 * through that libc is not a report. 252 is i386's `exit_group`. */
bad_write:
	movl	$21, %ebx
	jmp	die
bad_getpid:
	movl	$22, %ebx
	jmp	die
bad_malloc:
	movl	$23, %ebx
	jmp	die
bad_errno:
	movl	$24, %ebx
die:
	movl	$252, %eax
	int	$0x80
	hlt

	.section .rodata
msg:
	.ascii	"i386-glibc: dynamic 32-bit program started\n"
	.set	msglen, . - msg
