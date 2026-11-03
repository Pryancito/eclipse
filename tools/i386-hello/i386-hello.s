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
 *   27  fstat64 failed
 *   28  st_mode does not say this is a regular file
 *   29  st_size does not agree with the size _llseek reports
 *   30  the inode is missing from one of the two places it is kept
 *   31  fcntl64 with a scalar command failed
 *   32  fcntl64(F_SETLK64) failed
 *   33  fcntl64(F_GETLK64) did not write the answer back
 *   34  fcntl(55) did not answer a wide command the way fcntl64 does
 *   35  writev did not write both entries of the gather list
 *   36  readv did not scatter into both destinations
 *   37  preadv read the wrong bytes, or moved the file position
 *   38  pwritev did not gather the two entries in order
 *   39  clock_gettime wrote the wrong shape of timespec
 *   40  gettimeofday wrote the wrong shape of timeval
 *   41  clock_getres wrote the wrong shape of timespec
 *   42  nanosleep did not read the request four bytes at a time
 *   43  times wrote the wrong shape of tms
 *   44  getrusage wrote the wrong shape of rusage
 *   45  setitimer/getitimer wrote the wrong shape of itimerval
 *   46  _newselect did not read and write back a 32-bit timeval
 *   47  the timerfd itimerspec did not survive the round trip
 *   48  futex did not answer EAGAIN/ETIMEDOUT with a 32-bit timeout
 *   49  ugetrlimit wrote the wrong shape of rlimit
 *   50  the narrow infinity did not widen to the wide one
 *   51  the two getrlimits did not clamp where they each clamp
 *   52  statx did not fill 256 bytes at a 4-aligned destination
 *   53  ppoll refused a signal mask aligned to 4 and not to 8
 *   54  writev accepted an iov_len that does not fit a signed word
 *   55  socketcall did not carry a socketpair through its argument block
 *   56  the guard page for the next check could not be set up (unused:
 *       the two halves report their own errno, see below)
 *   57  socketcall read past the end of a block that ends at a page
 *   58  ioctl(FIOCLEX)/ioctl(FIONCLEX) did not move the close-on-exec bit
 *   59  ioctl(TCGETS) did not write 36 bytes of termios, or refused a
 *       destination aligned to 4 and not to 8
 *   60  ioctl(TIOCGPTN) did not write exactly one 4-byte pty number
 *   62  socketcall(sendmsg) did not carry a 28-byte envelope with two
 *       SCM_RIGHTS messages in it
 *   63  socketcall(recvmsg) did not take the message back
 *   64  recvmsg reported msg_controllen or msg_flags in the wrong place, or
 *       the control header that came back is not 12 bytes wide
 *   65  a descriptor arrived but is not a descriptor
 *   66  socketcall(sendmmsg) did not walk two entries of a 32-byte stride
 *   67  socketcall(recvmmsg) did not take the batch back
 *   68  a per-message transfer count was not written at offset 28 of its
 *       own entry
 *   69  socket(AF_INET, SOCK_DGRAM) through socketcall failed
 *   70  socketcall(setsockopt) refused an eight-byte SO_RCVTIMEO, which is
 *       the width a 32-bit struct timeval is
 *   71  socketcall(getsockopt) did not answer
 *   72  the timeout that came back is not the one that went in, or it was
 *       reported at the native width (the microseconds are the field a clamp
 *       loses and a conversion keeps)
 *   73  socketcall(setsockopt) accepted a negative optlen, which is a word of
 *       0xffffffff in the block and four billion to a kernel that
 *       zero-extends it
 *   74  sigaltstack did not round-trip twelve bytes, or answered with the
 *       native twenty-four
 *   75  rt_sigaction refused a twenty-byte struct sigaction
 *   76  kill() to this process failed
 *   77  the handler never ran: the frame was not built, or the restorer
 *       never reached rt_sigreturn
 *   78  the handler's arguments were not on its stack, where i386 passes
 *       them (they were in registers, as x86_64 passes them)
 *   79  the frame is not a struct rt_sigframe_ia32: the siginfo is not
 *       sixteen bytes above %esp, or the ucontext is not 128 bytes above it
 *   80  the siginfo's union does not start at byte twelve
 *   81  the ucontext's sigcontext_32 is not where a 32-bit handler reads it,
 *       or reports a 64-bit code selector
 *   82  clock_gettime (265) stopped writing a 32-bit timespec
 *   83  clock_gettime64 (403) did not write a 64-bit one: the same process
 *       got the same width from both numbers
 *   84  clock_getres_time64 put its one nanosecond in the wrong word
 *   85  clock_nanosleep_time64 read its request eight bytes at a time
 *
 *   128+e  mmap2 of the two pages of the guard answered errno e
 *   160+e  munmap of just the second page answered errno e
 *   192+e  execve of this program itself answered errno e (step 61)
 *
 * There is no 61: step 61 IS the `execve`, and it is reported by the image it
 * exec'd rather than by a number of its own (see below). It is also the last
 * step, because it replaces the program, so step 62 runs BEFORE it --- the
 * numbers are the order they were written in, not the order they run in, and
 * this is the one place the two differ.
 *
 * Step 61 is the one step whose result this program cannot read: a
 * successful `execve` replaces it. So it execs ITSELF with one argument
 * added and lets the new image report. A run started with no arguments does
 * all 60 steps, execs, and the second image does all 60 again and exits 0
 * because `argc >= 2`; a run started WITH an argument does the 60 and stops
 * there, which is the same program with the step switched off. The status
 * that reaches the shell is therefore the second image's, so an `argv` that
 * arrived miscounted comes back as 13 or 14 --- which is what a kernel
 * reading a 32-bit `argv` eight bytes at a time produces, since pairing the
 * halves of two entries into one pointer neither faults nor is noticed by
 * anything else.
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

	/* Keep what step 61 needs, because this is the only place it is
	 * reachable: `%esp` is this program's to move, and nothing below
	 * promises to leave the initial frame where it is. `argc` decides
	 * whether this run is the first or the one the first one exec'd,
	 * `argv[0]` is the name to exec, and the environment begins one NULL
	 * past the end of the argv array --- 4 bytes of `argc`, 4 per argv
	 * entry, 4 for its terminator. */
	movl	4(%esp), %eax
	movl	%eax, saved_argv0
	movl	(%esp), %eax
	movl	%eax, saved_argc
	leal	8(%esp,%eax,4), %eax
	movl	%eax, saved_envp

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

	/* fstat64(fd, statbuf): i386 197, and the first call in this program
	 * whose answer is a STRUCTURE rather than a number. Its ninety-six
	 * bytes are what makes it a conversion: the real st_ino is the LAST
	 * field and not the second, and every 64-bit field sits at 4-byte
	 * alignment, so st_size begins at offset 44 and not 48.
	 *
	 * Checking it against this container's own Linux is the point. An
	 * offset table transcribed wrongly passes every unit test written
	 * from the same wrong table; it cannot pass here. */
	movl	$197, %eax
	movl	fd, %ebx
	movl	$statbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_fstat64

	/* st_mode at offset 16: this is a regular file, so the type bits are
	 * S_IFREG. Reading the field at the wrong offset finds st_nlink or
	 * __st_ino there, neither of which has 0x8000 in its type bits. */
	movl	statbuf + 16, %eax
	andl	$0xf000, %eax
	cmpl	$0x8000, %eax
	jne	bad_fstat64_mode

	/* st_size at offset 44, against the size _llseek reports for the same
	 * descriptor. The two have to agree, and offset 44 is the one that
	 * 8-byte alignment would have moved to 48 -- so a structure that was
	 * padded like a native one fails here and nowhere else. */
	movl	$140, %eax
	movl	fd, %ebx
	xorl	%ecx, %ecx
	xorl	%edx, %edx
	movl	page, %esi
	movl	$2, %edi		/* SEEK_END */
	int	$0x80
	testl	%eax, %eax
	jnz	bad_fstat64_size
	movl	page, %eax
	movl	(%eax), %ecx		/* the size, low half */
	movl	4(%eax), %edx		/* and high */
	cmpl	statbuf + 44, %ecx
	jne	bad_fstat64_size
	cmpl	statbuf + 48, %edx
	jne	bad_fstat64_size
	/* And it is not zero, so the comparison above is not two empty
	 * fields agreeing with each other. */
	testl	%ecx, %ecx
	jz	bad_fstat64_size

	/* The inode is in the structure TWICE: whole at offset 88, and
	 * truncated to 32 bits at offset 12, where `struct stat` used to keep
	 * it. Both have to be filled, and the low half of the one has to be
	 * the other. */
	movl	statbuf + 88, %eax
	testl	%eax, %eax
	jz	bad_fstat64_ino
	cmpl	statbuf + 12, %eax
	jne	bad_fstat64_ino
	/* st_blksize at offset 52, which no filesystem reports as zero. */
	movl	statbuf + 52, %eax
	testl	%eax, %eax
	jz	bad_fstat64_ino

	/* fcntl64(fd, F_GETFL, 0): i386 221 with a command whose argument is
	 * a scalar, so it goes straight through to the native handler. It is
	 * here to separate the two halves of fcntl64: if this fails, nothing
	 * about `struct flock` is to blame. */
	movl	$221, %eax
	movl	fd, %ebx
	movl	$3, %ecx		/* F_GETFL */
	xorl	%edx, %edx
	int	$0x80
	testl	%eax, %eax
	js	bad_fcntl64

	/* fcntl64(fd, F_SETLK64, &lock64): the other half. F_SETLK64 is a
	 * command that exists only in a 32-bit ABI, and its argument is the
	 * 24-byte `struct flock64` -- l_start at offset 4, l_len at 12,
	 * l_pid at 20. A read lock, because this descriptor is open for
	 * reading only. */
	movl	$221, %eax
	movl	fd, %ebx
	movl	$13, %ecx		/* F_SETLK64 */
	movl	$lock64, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_setlk64

	/* F_GETLK64 over the same range. This process holds the lock, and a
	 * lock never conflicts with its own owner, so the answer has to be
	 * l_type = F_UNLCK (2) written back into the structure -- which is
	 * the write-back path, the one that has to lay the 24 bytes out again
	 * rather than merely read them. */
	movl	$0, lock64		/* clear l_type and l_whence */
	movl	$221, %eax
	movl	fd, %ebx
	movl	$12, %ecx		/* F_GETLK64 */
	movl	$lock64, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_getlk64
	movzwl	lock64, %eax
	cmpl	$2, %eax		/* F_UNLCK */
	jne	bad_getlk64

	/* And release it, so the program leaves nothing behind. */
	movw	$2, lock64		/* F_UNLCK */
	movl	$221, %eax
	movl	fd, %ebx
	movl	$13, %ecx		/* F_SETLK64 */
	movl	$lock64, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_setlk64

	/* fcntl(fd, F_SETLK64, &lock64): i386 55, the OLD number with a
	 * command that exists only in a 32-bit ABI.
	 *
	 * This step was written expecting EINVAL, on the belief that
	 * `compat_sys_fcntl` refuses the wide commands and leaves them to
	 * `fcntl64`. Running it here said otherwise: 55 takes the lock and
	 * returns 0, and F_GETLK64 on 55 writes the 24 bytes back. So the two
	 * numbers are the same call, the width comes from the command alone,
	 * and this step asserts what Linux actually does rather than what the
	 * first draft assumed. */
	movw	$0, lock64		/* F_RDLCK */
	movl	$55, %eax
	movl	fd, %ebx
	movl	$13, %ecx		/* F_SETLK64 */
	movl	$lock64, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_fcntl_wide

	/* F_GETLK64 on the old number too, which has to write the answer back
	 * into the same 24 bytes. */
	movl	$0, lock64
	movl	$55, %eax
	movl	fd, %ebx
	movl	$12, %ecx		/* F_GETLK64 */
	movl	$lock64, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_fcntl_wide
	movzwl	lock64, %eax
	cmpl	$2, %eax		/* F_UNLCK */
	jne	bad_fcntl_wide

	/* And release, through the old number as well. */
	movw	$2, lock64		/* F_UNLCK */
	movl	$55, %eax
	movl	fd, %ebx
	movl	$13, %ecx		/* F_SETLK64 */
	movl	$lock64, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_fcntl_wide

	/* writev(1, iov, 2): i386 146, and the first call whose argument is a
	 * LIST of structures. A 32-bit `struct iovec` is eight bytes, a
	 * 4-byte base and a 4-byte length, where the native one is sixteen.
	 *
	 * Two entries, because one entry cannot tell the two widths apart: a
	 * list of one read at the wrong stride still finds the right base and
	 * the right length in the first eight bytes. With two, a kernel
	 * reading sixteen-byte entries takes the SECOND entry's base as the
	 * FIRST entry's length. */
	movl	$msg2a, iov + 0
	movl	$msg2alen, iov + 4
	movl	$msg2b, iov + 8
	movl	$msg2blen, iov + 12
	movl	$146, %eax
	movl	$1, %ebx		/* stdout */
	movl	$iov, %ecx
	movl	$2, %edx
	int	$0x80
	cmpl	$msg2alen + msg2blen, %eax
	jne	bad_writev

	/* readv(fd, iov, 2): i386 145, scattering four bytes of this file
	 * into two places that are not next to each other. Seek to 1 first,
	 * so the first three bytes have to be "ELF" --- and the fourth is
	 * EI_CLASS at file offset 4, which for this program is 1
	 * (ELFCLASS32), a value nothing else in the header happens to be.
	 *
	 * The gap between the two destinations is what makes this a scatter
	 * and not a read: a kernel that ignored the second entry would leave
	 * the byte at page+16 as the 0xee this program puts there. */
	movl	$140, %eax		/* _llseek to offset 1 */
	movl	fd, %ebx
	xorl	%ecx, %ecx
	movl	$1, %edx
	movl	page, %esi
	xorl	%edi, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_readv

	movl	page, %eax
	movl	$0, (%eax)
	movb	$0xee, 16(%eax)
	movl	%eax, iov + 0
	movl	$3, iov + 4
	addl	$16, %eax
	movl	%eax, iov + 8
	movl	$1, iov + 12
	movl	$145, %eax
	movl	fd, %ebx
	movl	$iov, %ecx
	movl	$2, %edx
	int	$0x80
	cmpl	$4, %eax
	jne	bad_readv
	movl	page, %eax
	movl	(%eax), %ecx
	andl	$0xffffff, %ecx
	cmpl	$0x464c45, %ecx		/* "ELF", little endian */
	jne	bad_readv
	movzbl	16(%eax), %ecx
	cmpl	$1, %ecx		/* EI_CLASS = ELFCLASS32 */
	jne	bad_readv

	/* preadv(fd, iov, 2, 1, 0): i386 333, the same scatter at an explicit
	 * offset that arrives in two registers, LOW FIRST --- `pread64`'s
	 * order and not `_llseek`'s. It must not move the file position, so
	 * the plain read that follows has to pick up where readv left it,
	 * which is file offset 5. */
	movl	page, %eax
	movl	$0, (%eax)
	movb	$0xee, 16(%eax)
	movl	$333, %eax
	movl	fd, %ebx
	movl	$iov, %ecx
	movl	$2, %edx
	movl	$1, %esi		/* offset low */
	xorl	%edi, %edi		/* offset high */
	int	$0x80
	cmpl	$4, %eax
	jne	bad_preadv
	movl	page, %eax
	movl	(%eax), %ecx
	andl	$0xffffff, %ecx
	cmpl	$0x464c45, %ecx
	jne	bad_preadv
	movzbl	16(%eax), %ecx
	cmpl	$1, %ecx
	jne	bad_preadv

	/* And the position: _llseek(fd, 0, SEEK_CUR) has to say 5, which is
	 * where readv stopped. If preadv had moved it, it would say 9. */
	movl	$140, %eax
	movl	fd, %ebx
	xorl	%ecx, %ecx
	xorl	%edx, %edx
	movl	page, %esi
	movl	$1, %edi		/* SEEK_CUR */
	int	$0x80
	testl	%eax, %eax
	jnz	bad_preadv
	movl	page, %eax
	cmpl	$5, (%eax)
	jne	bad_preadv
	cmpl	$0, 4(%eax)
	jne	bad_preadv

	/* pwritev(fd2, iov, 2, 0, 0): i386 334. It needs a writable
	 * descriptor, so this one opens a file of its own under /tmp and
	 * gathers six bytes into it from two places, then reads them back
	 * with pread64 to check that the two entries arrived in order and
	 * nothing from between them came along.
	 *
	 * The name is a fixed one in a directory anybody can write, so the
	 * file is created EXCLUSIVELY -- O_RDWR|O_CREAT|O_EXCL, mode 0600 --
	 * and the name is unlinked first. Without O_EXCL an existing path is
	 * opened and its first six bytes overwritten, and a SYMLINK at that
	 * path sends the write somewhere else entirely and the cleanup
	 * unlinks the name; this probe is documented as something to run on a
	 * Linux host, so that would be damage to a real machine's files.
	 * `unlink` removes the link and not what it points at, so the pair is
	 * safe: if anybody recreates the path in between, O_EXCL makes the
	 * step FAIL rather than write through it. */
	movl	$10, %eax		/* unlink, ENOENT is fine */
	movl	$tmppath, %ebx
	int	$0x80
	movl	$5, %eax		/* open */
	movl	$tmppath, %ebx
	movl	$0302, %ecx		/* O_RDWR|O_CREAT|O_EXCL = 2|64|128 */
	movl	$0600, %edx
	int	$0x80
	testl	%eax, %eax
	js	bad_pwritev
	movl	%eax, fd2

	movl	page, %eax
	movl	$0x434241, (%eax)	/* "ABC" */
	movl	%eax, iov + 0
	movl	$3, iov + 4
	addl	$16, %eax
	movl	$0x465445, (%eax)	/* "ETF" -- distinct from "ABC" */
	movl	%eax, iov + 8
	movl	$3, iov + 12
	movl	$334, %eax
	movl	fd2, %ebx
	movl	$iov, %ecx
	movl	$2, %edx
	xorl	%esi, %esi		/* offset low */
	xorl	%edi, %edi		/* offset high */
	int	$0x80
	cmpl	$6, %eax
	jne	bad_pwritev

	movl	page, %eax
	movl	$0, 32(%eax)
	movl	$0, 36(%eax)
	movl	$180, %eax		/* pread64 */
	movl	fd2, %ebx
	movl	page, %ecx
	addl	$32, %ecx
	movl	$6, %edx
	xorl	%esi, %esi
	xorl	%edi, %edi
	int	$0x80
	cmpl	$6, %eax
	jne	bad_pwritev
	movl	page, %eax
	movl	32(%eax), %ecx
	andl	$0xffffff, %ecx
	cmpl	$0x434241, %ecx		/* "ABC" first */
	jne	bad_pwritev
	movl	35(%eax), %ecx
	andl	$0xffffff, %ecx
	cmpl	$0x465445, %ecx		/* "ETF" right after it */
	jne	bad_pwritev

	/* And leave nothing behind. */
	movl	$6, %eax		/* close */
	movl	fd2, %ebx
	int	$0x80
	movl	$10, %eax		/* unlink */
	movl	$tmppath, %ebx
	int	$0x80

	/* The structures of time, and the one rule they all follow: a field
	 * that is eight bytes natively is four bytes here, in the same order,
	 * with no padding. Each step poisons the buffer with 0xaa first and
	 * then checks TWO things: that the right values arrived, and that the
	 * word just past the structure is still the poison. The second check
	 * is the one that matters, because a kernel that wrote the native
	 * layout would pass the first: `clock_gettime` would put its seconds
	 * at offset 0 all the same, and write eight bytes of somebody else's
	 * memory after it.
	 *
	 * clock_gettime(CLOCK_REALTIME, ts): i386 265. */
	call	poison_tbuf
	movl	$265, %eax
	xorl	%ebx, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_clock_gettime
	cmpl	$0xaaaaaaaa, tbuf + 8
	jne	bad_clock_gettime
	cmpl	$0xaaaaaaaa, tbuf + 12
	jne	bad_clock_gettime
	/* A wall clock later than 2001, and a nanosecond field that is a
	 * fraction of a second rather than the top half of a seconds count. */
	cmpl	$1000000000, tbuf + 0
	jbe	bad_clock_gettime
	cmpl	$1000000000, tbuf + 4
	jae	bad_clock_gettime

	/* gettimeofday(tv, NULL): i386 78, the call the module documentation
	 * uses as its example of why matching the two tables by name is
	 * unsound. Microseconds, so the second field is below a million. */
	call	poison_tbuf
	movl	$78, %eax
	movl	$tbuf, %ebx
	xorl	%ecx, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_gettimeofday
	cmpl	$0xaaaaaaaa, tbuf + 8
	jne	bad_gettimeofday
	cmpl	$1000000000, tbuf + 0
	jbe	bad_gettimeofday
	cmpl	$1000000, tbuf + 4
	jae	bad_gettimeofday

	/* clock_getres(CLOCK_MONOTONIC, res): i386 266. A resolution of whole
	 * seconds would be absurd, so the seconds are 0 and the nanoseconds
	 * are a real fraction -- which also means the two fields cannot be
	 * confused for one another. */
	call	poison_tbuf
	movl	$266, %eax
	movl	$1, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_clock_getres
	cmpl	$0xaaaaaaaa, tbuf + 8
	jne	bad_clock_getres
	cmpl	$0, tbuf + 0
	jne	bad_clock_getres
	cmpl	$0, tbuf + 4
	je	bad_clock_getres
	cmpl	$1000000000, tbuf + 4
	ja	bad_clock_getres

	/* nanosleep(req, NULL): i386 162, and the first of these that is read
	 * rather than written. One millisecond. Read at the native width the
	 * eight bytes at `nsreq` are ONE field, so the seconds would come out
	 * as 1000000 << 32 -- a sleep of 140 million years, which
	 * `timespec64_valid` refuses with EINVAL. A zero return is the proof
	 * that the request was read four bytes at a time. */
	movl	$0, nsreq + 0
	movl	$1000000, nsreq + 4
	movl	$162, %eax
	movl	$nsreq, %ebx
	xorl	%ecx, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_nanosleep

	/* times(buf): i386 43. Sixteen bytes, four clock_t. All four may
	 * legitimately be zero in a program this short, so the only thing
	 * worth asserting is the size -- which is the thing that would be
	 * wrong. */
	call	poison_tbuf
	movl	$43, %eax
	movl	$tbuf, %ebx
	int	$0x80
	testl	%eax, %eax
	js	bad_times
	cmpl	$0xaaaaaaaa, tbuf + 16
	jne	bad_times
	cmpl	$0xaaaaaaaa, tbuf + 20
	jne	bad_times

	/* getrusage(RUSAGE_SELF, buf): i386 77, and the big one -- 72 bytes
	 * against 144. The two `timeval`s are 8 bytes each, so `ru_minflt`
	 * lands at 32 and `ru_nvcsw` at 64 where a doubled layout would put
	 * them at 64 and 128. Both are checked as "written", not as a value:
	 * a count of zero is a legitimate answer and is still not 0xaa. */
	call	poison_tbuf
	movl	$77, %eax
	xorl	%ebx, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_getrusage
	cmpl	$0xaaaaaaaa, tbuf + 72
	jne	bad_getrusage
	cmpl	$0xaaaaaaaa, tbuf + 32
	je	bad_getrusage
	cmpl	$0xaaaaaaaa, tbuf + 64
	je	bad_getrusage

	/* setitimer(ITIMER_REAL, new, NULL) then getitimer: i386 104 and 105.
	 * Sixteen bytes, and the pair is in ABI order -- the interval first,
	 * the value second -- which is what makes a 100 second value readable
	 * as a countdown rather than as an interval. */
	movl	$0, itv + 0
	movl	$0, itv + 4
	movl	$100, itv + 8
	movl	$0, itv + 12
	movl	$104, %eax
	xorl	%ebx, %ebx
	movl	$itv, %ecx
	xorl	%edx, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_itimer
	call	poison_tbuf
	movl	$105, %eax
	xorl	%ebx, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_itimer
	cmpl	$0xaaaaaaaa, tbuf + 16
	jne	bad_itimer
	cmpl	$0, tbuf + 8
	je	bad_itimer
	cmpl	$100, tbuf + 8
	ja	bad_itimer
	/* Disarmed again, so that no SIGALRM arrives while the rest runs. */
	movl	$0, itv + 8
	movl	$104, %eax
	xorl	%ebx, %ebx
	movl	$itv, %ecx
	xorl	%edx, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_itimer

	/* _newselect(0, NULL, NULL, NULL, tv): i386 142, which is x86_64's
	 * `select`. i386's own `select` is 82 and passes its arguments in a
	 * block of memory, so getting the two confused would hand the native
	 * handler a pointer where it expects `nfds`. The timeout is a
	 * `timeval` read AND written back, which is the round trip. */
	movl	$0, seltv + 0
	movl	$1000, seltv + 4
	movl	$142, %eax
	xorl	%ebx, %ebx
	xorl	%ecx, %ecx
	xorl	%edx, %edx
	xorl	%esi, %esi
	movl	$seltv, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_select
	/* What is left of a one-millisecond timeout, which cannot be more
	 * than what was asked for and cannot have grown a seconds field. */
	cmpl	$0, seltv + 0
	jne	bad_select
	cmpl	$1000, seltv + 4
	ja	bad_select

	/* timerfd_create / timerfd_settime / timerfd_gettime: i386 322, 325
	 * and 326, the `itimerspec` pair both ways. The interval comes back
	 * exactly as it went in, which no misread of the layout survives, and
	 * the value is counting down from three seconds. */
	movl	$322, %eax
	movl	$1, %ebx		/* CLOCK_MONOTONIC */
	xorl	%ecx, %ecx
	int	$0x80
	testl	%eax, %eax
	js	bad_timerfd
	movl	%eax, fd3
	movl	$7, its + 0
	movl	$0, its + 4
	movl	$3, its + 8
	movl	$0, its + 12
	movl	$325, %eax
	movl	fd3, %ebx
	xorl	%ecx, %ecx
	movl	$its, %edx
	xorl	%esi, %esi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_timerfd
	call	poison_tbuf
	movl	$326, %eax
	movl	fd3, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_timerfd
	cmpl	$0xaaaaaaaa, tbuf + 16
	jne	bad_timerfd
	cmpl	$7, tbuf + 0
	jne	bad_timerfd
	cmpl	$0, tbuf + 8
	je	bad_timerfd
	cmpl	$3, tbuf + 8
	ja	bad_timerfd
	movl	$6, %eax
	movl	fd3, %ebx
	int	$0x80

	/* futex(FUTEX_WAIT_PRIVATE): i386 240. Two calls, and neither of them
	 * waits for anything real. The first passes a `val` that does NOT
	 * match the word, which `futex(2)` answers with EAGAIN before it looks
	 * at the timeout: that is the entry path and the futex word. The
	 * second passes the matching value and a one-millisecond timeout, so
	 * it comes back ETIMEDOUT -- and THAT is the narrow `timespec`, because
	 * read at the native width those eight bytes are one field and the
	 * timeout would be 1000000 << 32 nanoseconds, which is either a
	 * refusal or a wait of 140 million years. */
	movl	$0, futex_word
	movl	$240, %eax
	movl	$futex_word, %ebx
	movl	$128, %ecx		/* FUTEX_WAIT | FUTEX_PRIVATE_FLAG */
	movl	$1, %edx		/* a value the word does not hold */
	xorl	%esi, %esi
	int	$0x80
	cmpl	$-11, %eax		/* -EAGAIN */
	jne	bad_futex
	movl	$0, futex_to + 0
	movl	$1000000, futex_to + 4
	movl	$240, %eax
	movl	$futex_word, %ebx
	movl	$128, %ecx
	xorl	%edx, %edx		/* the value the word does hold */
	movl	$futex_to, %esi
	int	$0x80
	cmpl	$-110, %eax		/* -ETIMEDOUT */
	jne	bad_futex

	/* futex(FUTEX_WAKE_PRIVATE, 1): nobody is waiting, so the answer is
	 * the count woken, zero. The fourth argument is a COUNT here and not a
	 * timeout, which is the half of this call that has no structure in it
	 * at all. */
	movl	$240, %eax
	movl	$futex_word, %ebx
	movl	$129, %ecx		/* FUTEX_WAKE | FUTEX_PRIVATE_FLAG */
	movl	$1, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_futex

	/* ugetrlimit(RLIMIT_CORE, buf): i386 191, eight bytes and not sixteen.
	 * i386 calls it `ugetrlimit` because 76 was already taken by the one
	 * that clamps to a signed long instead. */
	call	poison_tbuf
	movl	$191, %eax
	movl	$4, %ebx		/* RLIMIT_CORE */
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_ugetrlimit
	cmpl	$0xaaaaaaaa, tbuf + 8
	jne	bad_ugetrlimit
	cmpl	$0xaaaaaaaa, tbuf + 12
	jne	bad_ugetrlimit

	/* setrlimit(RLIMIT_CORE, { 1234, 0xffffffff }): i386 75. The hard
	 * limit is written as the narrow infinity, and read back through
	 * `prlimit64` it has to be the WIDE infinity -- not the number
	 * 4294967295, which is what widening it as a plain number would give
	 * and which would quietly cap at four gigabytes a process that asked
	 * for no cap at all. */
	movl	$1234, rl32 + 0
	movl	$0xffffffff, rl32 + 4
	movl	$75, %eax
	movl	$4, %ebx
	movl	$rl32, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_setrlimit32
	call	poison_tbuf
	movl	$340, %eax		/* prlimit64, sixteen bytes in both ABIs */
	xorl	%ebx, %ebx
	movl	$4, %ecx
	xorl	%edx, %edx
	movl	$tbuf, %esi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_setrlimit32
	cmpl	$1234, tbuf + 0
	jne	bad_setrlimit32
	cmpl	$0, tbuf + 4
	jne	bad_setrlimit32
	cmpl	$0xffffffff, tbuf + 8
	jne	bad_setrlimit32
	cmpl	$0xffffffff, tbuf + 12
	jne	bad_setrlimit32
	cmpl	$0xaaaaaaaa, tbuf + 16
	jne	bad_setrlimit32

	/* And the two getters on that same limit, which is now infinite: 191
	 * clamps to 0xffffffff and 76 clamps to 0x7fffffff. Clamped, not
	 * truncated -- truncating an infinite limit gives 0xffffffff by
	 * accident, but truncating one of eight gigabytes gives zero, which
	 * says "forbidden" for a limit that forbids nothing. */
	call	poison_tbuf
	movl	$191, %eax
	movl	$4, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_rlimit_clamp
	cmpl	$1234, tbuf + 0
	jne	bad_rlimit_clamp
	cmpl	$0xffffffff, tbuf + 4
	jne	bad_rlimit_clamp
	call	poison_tbuf
	movl	$76, %eax		/* the older getrlimit */
	movl	$4, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_rlimit_clamp
	cmpl	$1234, tbuf + 0
	jne	bad_rlimit_clamp
	cmpl	$0x7fffffff, tbuf + 4
	jne	bad_rlimit_clamp
	cmpl	$0xaaaaaaaa, tbuf + 8
	jne	bad_rlimit_clamp

	/* statx(fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS, buf): i386 383, and
	 * the third shape of the same trouble. `struct statx` is the SAME 256
	 * bytes in both ABIs -- it was designed that way -- so nothing about
	 * its layout needs converting. What differs is that on i386 a
	 * `long long` aligns to 4, so this buffer, at an address that is a
	 * multiple of 4 and not of 8, is a perfectly good `struct statx *`
	 * here and one a typed store in the kernel would refuse.
	 *
	 * Which makes the FIRST check the address itself: if the linker ever
	 * puts `tbuf` on an eight-byte boundary this step stops testing what
	 * it is for, and saying so is better than passing for the wrong
	 * reason. */
	movl	$statxbuf, %eax
	testl	$4, %eax
	jz	bad_statx
	movl	$statxbuf, %edi
	movl	$320, %ecx
	movl	$0xaaaaaaaa, %eax
statx_poison:
	movl	%eax, (%edi)
	addl	$4, %edi
	subl	$4, %ecx
	jnz	statx_poison
	movl	$383, %eax
	movl	fd, %ebx
	movl	$emptystr, %ecx
	movl	$0x1000, %edx		/* AT_EMPTY_PATH */
	movl	$0x7ff, %esi		/* STATX_BASIC_STATS */
	movl	$statxbuf, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_statx
	/* 256 bytes, so the word at 256 is still the poison. */
	cmpl	$0xaaaaaaaa, statxbuf + 256
	jne	bad_statx
	/* stx_mask at 0 carries at least the basic bits it was asked for. */
	movl	statxbuf + 0, %eax
	andl	$0x7ff, %eax
	cmpl	$0x7ff, %eax
	jne	bad_statx
	/* stx_mode at 28 says regular file, and stx_size at 40 is not zero. */
	movzwl	statxbuf + 28, %eax
	andl	$0xf000, %eax
	cmpl	$0x8000, %eax		/* S_IFREG */
	jne	bad_statx
	cmpl	$0, statxbuf + 40
	je	bad_statx
	cmpl	$0, statxbuf + 44	/* the top half of a size under 4 GiB */
	jne	bad_statx

	/* socketcall(call, args): i386 102, and the only call here whose
	 * arguments are not in registers at all --- they are in a block of
	 * `unsigned long` in the caller's memory, four bytes each where this
	 * kernel's registers are eight.
	 *
	 * A whole connected pair, through socketcall and nothing else:
	 * socketpair(8), send(9), recv(10), getsockname(6), shutdown(13). An
	 * AF_UNIX pair needs no network, so this works the same on a Linux
	 * host and in Eclipse. */
	movl	$1, sargs + 0		/* AF_UNIX */
	movl	$1, sargs + 4		/* SOCK_STREAM */
	movl	$0, sargs + 8
	movl	$sv, sargs + 12
	movl	$102, %eax
	movl	$8, %ebx		/* SYS_SOCKETPAIR */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_socketcall

	/* send(sv[0], "ABC", 3, 0) */
	movl	$0x434241, sbuf
	movl	sv + 0, %eax
	movl	%eax, sargs + 0
	movl	$sbuf, sargs + 4
	movl	$3, sargs + 8
	movl	$0, sargs + 12
	movl	$102, %eax
	movl	$9, %ebx		/* SYS_SEND */
	movl	$sargs, %ecx
	int	$0x80
	cmpl	$3, %eax
	jne	bad_socketcall

	/* recv(sv[1], sbuf2, 3, 0), and the bytes have to be the ones sent:
	 * a block read at the wrong width would hand the descriptor and the
	 * length to each other's slots and this would not be "ABC". */
	movl	$0, sbuf2
	movl	sv + 4, %eax
	movl	%eax, sargs + 0
	movl	$sbuf2, sargs + 4
	movl	$3, sargs + 8
	movl	$0, sargs + 12
	movl	$102, %eax
	movl	$10, %ebx		/* SYS_RECV */
	movl	$sargs, %ecx
	int	$0x80
	cmpl	$3, %eax
	jne	bad_socketcall
	movl	sbuf2, %ecx
	andl	$0xffffff, %ecx
	cmpl	$0x434241, %ecx
	jne	bad_socketcall

	/* getsockname(sv[0], sa, &salen): three words, the last two pointers.
	 * The length comes back no bigger than it went in. */
	movl	$128, salen
	movl	sv + 0, %eax
	movl	%eax, sargs + 0
	movl	$sa, sargs + 4
	movl	$salen, sargs + 8
	movl	$102, %eax
	movl	$6, %ebx		/* SYS_GETSOCKNAME */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_socketcall
	cmpl	$128, salen
	ja	bad_socketcall

	/* shutdown(sv[0], SHUT_RDWR): two words, which is the SHORTEST block
	 * any call has. A kernel that read a fixed six would reach four words
	 * past it. */
	movl	sv + 0, %eax
	movl	%eax, sargs + 0
	movl	$2, sargs + 4
	movl	$102, %eax
	movl	$13, %ebx		/* SYS_SHUTDOWN */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_socketcall

	/* And the step that says the word count is not a guess: put a
	 * two-word block for `shutdown` at the very END of a mapped page
	 * whose next page is NOT mapped, so that reading a third word is
	 * EFAULT. Linux reads exactly two and answers 0; a kernel that read
	 * more would answer EFAULT for a block a program is entitled to put
	 * there.
	 *
	 * mmap2 two pages, drop the second. The address must be checked
	 * UNSIGNED: a valid high address like 0xf7xxxxxx is negative as a
	 * signed int, which cost me a run. */
	movl	$192, %eax
	xorl	%ebx, %ebx
	movl	$8192, %ecx
	movl	$3, %edx		/* PROT_READ|PROT_WRITE */
	movl	$0x22, %esi		/* MAP_PRIVATE|MAP_ANONYMOUS */
	movl	$-1, %edi
	xorl	%ebp, %ebp
	int	$0x80
	cmpl	$-4096, %eax
	jae	bad_mmap2_pair
	movl	%eax, twopages
	movl	$91, %eax		/* munmap the second page: the guard */
	movl	twopages, %ebx
	addl	$4096, %ebx
	movl	$4096, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_munmap_guard

	/* The block goes at page_end - 8, so words 0 and 1 are readable and
	 * word 2 is not. */
	movl	twopages, %edi
	addl	$4096, %edi
	subl	$8, %edi
	movl	sv + 4, %eax		/* the other end, still open */
	movl	%eax, (%edi)
	movl	$2, 4(%edi)		/* SHUT_RDWR */
	movl	$102, %eax
	movl	$13, %ebx
	movl	%edi, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_sargs_read

	/* ppoll(NULL, 0, {0,0}, mask, 8): i386 309, and the same trap in the
	 * READING direction. A kernel `sigset_t` is one 64-bit word, so a
	 * typed load wants 8; an i386 `sigset_t` is `unsigned long sig[2]`,
	 * which aligns to 4, so this mask -- deliberately 4 mod 8, like
	 * `statxbuf` -- is one Linux reads and a typed load refuses.
	 *
	 * Measured on a Linux host before it was written: the same call with
	 * its mask at an address 4 mod 8 answers 0.
	 *
	 * No descriptors and a zero timeout, so it returns at once and what
	 * the status reports is only whether the arguments were accepted. The
	 * timeout goes in `zerots`, which is 8-aligned on purpose: at a
	 * 4-mod-8 address the TIMEOUT would be the thing refused and the step
	 * would be testing the wrong argument. */
	movl	$sigmask4, %eax
	testl	$4, %eax
	jz	bad_ppoll_mask
	movl	$0, sigmask4 + 0
	movl	$0, sigmask4 + 4
	movl	$0, zerots + 0
	movl	$0, zerots + 4
	movl	$0, zerots + 8
	movl	$0, zerots + 12
	movl	$309, %eax
	xorl	%ebx, %ebx		/* no fds */
	xorl	%ecx, %ecx		/* nfds = 0 */
	movl	$zerots, %edx
	movl	$sigmask4, %esi
	movl	$8, %edi		/* sigsetsize */
	int	$0x80
	testl	%eax, %eax
	jnz	bad_ppoll_mask

	/* writev(1, iov, 1) with iov_len = 0x80000000: i386 146, and the one
	 * step here that asks for a REFUSAL. The wire field is unsigned, but
	 * Linux reads each length into a signed `compat_ssize_t` and answers
	 * EINVAL for a negative one, so the field's range and the ABI's are
	 * not the same.
	 *
	 * Measured on a Linux host: 0x7fffffff is accepted (a short write of
	 * whatever was mapped) and both 0x80000000 and 0xffffffff are EINVAL
	 * with nothing transferred. Only the refusal is asserted here,
	 * because the accepted case writes to the probe's own stdout and its
	 * byte count depends on how much of the page is mapped.
	 *
	 * -EINVAL is -22, and it has to be exactly that: a kernel that
	 * widened the length as a plain number would move bytes and report a
	 * count, which is a positive answer and not merely a different
	 * errno. */
	movl	page, %eax
	movl	%eax, iov + 0
	movl	$0x80000000, iov + 4
	movl	$146, %eax
	movl	$1, %ebx
	movl	$iov, %ecx
	movl	$1, %edx
	int	$0x80
	cmpl	$-22, %eax
	jne	bad_iov_len
	/* And 0xffffffff, which is the `(size_t)-1` a program reaches by
	 * mistake rather than on purpose. */
	movl	$0xffffffff, iov + 4
	movl	$146, %eax
	movl	$1, %ebx
	movl	$iov, %ecx
	movl	$1, %edx
	int	$0x80
	cmpl	$-22, %eax
	jne	bad_iov_len

	/* ioctl(FIOCLEX) and ioctl(FIONCLEX): i386 54, and the first request
	 * this kernel answers for a 32-bit process. These two take NO
	 * argument at all, so nothing is read or written through a pointer
	 * and what the step proves is only that the call arrives and acts ---
	 * which is why it comes first, before any request with a structure.
	 *
	 * The proof is the close-on-exec bit itself, read back with
	 * `fcntl64(F_GETFD)`: a kernel that answered 0 and did nothing would
	 * pass a step that only checked the return value. */
	movl	$54, %eax
	movl	fd, %ebx
	movl	$0x5451, %ecx		/* FIOCLEX */
	xorl	%edx, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_fioclex
	movl	$221, %eax
	movl	fd, %ebx
	movl	$1, %ecx		/* F_GETFD */
	xorl	%edx, %edx
	int	$0x80
	testl	$1, %eax		/* FD_CLOEXEC */
	jz	bad_fioclex
	movl	$54, %eax
	movl	fd, %ebx
	movl	$0x5450, %ecx		/* FIONCLEX */
	xorl	%edx, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_fioclex
	movl	$221, %eax
	movl	fd, %ebx
	movl	$1, %ecx
	xorl	%edx, %edx
	int	$0x80
	testl	$1, %eax
	jnz	bad_fioclex

	/* ioctl(TCGETS) on a pty master, into a destination 4 mod 8.
	 *
	 * Two things in one step, and both are measurements rather than
	 * beliefs. The kernel ABI `struct termios` is 36 bytes --- four
	 * `tcflag_t`, `c_line`, `c_cc[19]` --- and NOT the 60-byte structure
	 * libc carries, so the word at offset 36 has to still be the poison.
	 * And the destination is deliberately 4 mod 8: termios is made of
	 * u32, u16 and bytes, so it has none of the alignment trouble the
	 * 64-bit structures have, and this is the step that says so rather
	 * than a comment saying so.
	 *
	 * Measured on a Linux host: `TCGETS` on `/dev/ptmx` with its
	 * destination at an address 4 mod 8 answers 0. */
	movl	$5, %eax
	movl	$ptmxpath, %ebx
	movl	$2, %ecx		/* O_RDWR */
	xorl	%edx, %edx
	int	$0x80
	testl	%eax, %eax
	js	bad_tcgets
	movl	%eax, ptmxfd
	movl	$iobuf4, %eax
	testl	$4, %eax
	jz	bad_tcgets
	call	poison_iobuf4
	movl	$54, %eax
	movl	ptmxfd, %ebx
	movl	$0x5401, %ecx		/* TCGETS */
	movl	$iobuf4, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_tcgets
	/* c_iflag was written over the poison... */
	cmpl	$0xaaaaaaaa, iobuf4 + 0
	je	bad_tcgets
	/* ...and the word at 36 was not: 36 bytes, not 60. */
	cmpl	$0xaaaaaaaa, iobuf4 + 36
	jne	bad_tcgets

	/* ioctl(TIOCGPTN): the pty number, one `unsigned int`. Its request
	 * number has bit 31 set (0x80045430), so this is also the step that
	 * says a request arriving zero-extended out of the 32-bit trap frame
	 * reaches a handler that compares the low 32 bits --- a native caller
	 * sends the same request sign-extended. */
	call	poison_iobuf4
	movl	$54, %eax
	movl	ptmxfd, %ebx
	movl	$0x80045430, %ecx	/* TIOCGPTN */
	movl	$iobuf4, %edx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_tiocgptn
	cmpl	$0xaaaaaaaa, iobuf4 + 0
	je	bad_tiocgptn
	cmpl	$0xaaaaaaaa, iobuf4 + 4
	jne	bad_tiocgptn

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

	/* Step 62: a real descriptor through a real `SCM_RIGHTS`, at the
	 * 32-bit layout of BOTH structures it needs --- the envelope and the
	 * control header inside it.
	 *
	 * ```text
	 * struct msghdr, i386: 28 bytes       struct cmsghdr, i386: 12 bytes
	 *   msg_name        0, 4               cmsg_len     0, 4
	 *   msg_namelen     4, 4               cmsg_level   4, 4
	 *   msg_iov         8, 4               cmsg_type    8, 4
	 *   msg_iovlen     12, 4             CMSG_ALIGN is sizeof(long) = 4
	 *   msg_control    16, 4
	 *   msg_controllen 20, 4             native: 16 bytes, aligned to 8
	 *   msg_flags      24, 4
	 * ```
	 *
	 * Natively the envelope is 56 bytes and the header 16 aligned to 8,
	 * and the two sides of this step catch the two ways that matters:
	 *
	 *   - **TWO control messages**, the first carrying two descriptors.
	 *     Twenty bytes, which is a multiple of 4 and not of 8, so a walk
	 *     that kept the native `CMSG_ALIGN` steps 24 from the first and
	 *     lands four bytes inside the second --- and the second's
	 *     descriptor never arrives. One message of one descriptor is 16
	 *     bytes either way, which is why that is the round trip this does
	 *     NOT do.
	 *   - **Poison past the end of the receiving envelope**, as far as 56
	 *     bytes. `recvmsg` reports `msg_controllen` and `msg_flags`
	 *     through the caller's own structure, and at the native offsets
	 *     those land 12 and 20 bytes past the end of this one, over
	 *     whatever the program keeps next to it. The poison at 28 and at
	 *     48 has to survive, and the one at 24 has to be gone.
	 *
	 * A fresh pair: step 55 shut its own down. */
	movl	$1, sargs + 0		/* AF_UNIX */
	movl	$1, sargs + 4		/* SOCK_STREAM */
	movl	$0, sargs + 8
	movl	$msv, sargs + 12
	movl	$102, %eax
	movl	$8, %ebx		/* SYS_SOCKETPAIR */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_sendmsg32

	/* The control buffer: { len 20, SOL_SOCKET, SCM_RIGHTS, fd, fd } and
	 * then { len 16, SOL_SOCKET, SCM_RIGHTS, fd }. The descriptors passed
	 * are the pair's own ends, which are the two this program is sure to
	 * have. */
	movl	$20, mctrl + 0
	movl	$1, mctrl + 4		/* SOL_SOCKET */
	movl	$1, mctrl + 8		/* SCM_RIGHTS */
	movl	msv + 0, %eax
	movl	%eax, mctrl + 12
	movl	msv + 4, %eax
	movl	%eax, mctrl + 16
	movl	$16, mctrl + 20
	movl	$1, mctrl + 24
	movl	$1, mctrl + 28
	movl	msv + 0, %eax
	movl	%eax, mctrl + 32

	/* One byte of data, because ancillary data rides a message and a
	 * message of no bytes is not delivered. */
	movl	$0x5a, mbuf
	movl	$mbuf, miov + 0
	movl	$1, miov + 4

	movl	$0, mhdr + 0		/* msg_name */
	movl	$0, mhdr + 4		/* msg_namelen */
	movl	$miov, mhdr + 8		/* msg_iov */
	movl	$1, mhdr + 12		/* msg_iovlen */
	movl	$mctrl, mhdr + 16	/* msg_control */
	movl	$36, mhdr + 20		/* msg_controllen: both messages */
	movl	$0, mhdr + 24		/* msg_flags */

	movl	msv + 0, %eax
	movl	%eax, sargs + 0
	movl	$mhdr, sargs + 4
	movl	$0, sargs + 8
	movl	$102, %eax
	movl	$16, %ebx		/* SYS_SENDMSG */
	movl	$sargs, %ecx
	int	$0x80
	cmpl	$1, %eax
	jne	bad_sendmsg32

	/* And back. Both the control buffer and the whole 56 bytes from the
	 * envelope's start are poisoned first, so every field this step reads
	 * afterwards says by itself whether the kernel wrote it. */
	call	poison_recv_msg
	movl	$0, mbuf2
	movl	$mbuf2, miov2 + 0
	movl	$1, miov2 + 4
	movl	$0, mhdr2 + 0
	movl	$0, mhdr2 + 4
	movl	$miov2, mhdr2 + 8
	movl	$1, mhdr2 + 12
	movl	$rctrl, mhdr2 + 16
	movl	$64, mhdr2 + 20
	/* msg_flags stays poisoned: the kernel always reports it. */

	movl	msv + 4, %eax
	movl	%eax, sargs + 0
	movl	$mhdr2, sargs + 4
	movl	$0, sargs + 8
	movl	$102, %eax
	movl	$17, %ebx		/* SYS_RECVMSG */
	movl	$sargs, %ecx
	int	$0x80
	cmpl	$1, %eax
	jne	bad_recvmsg32

	/* msg_controllen at 20: a 12-byte header and the THREE descriptors. */
	cmpl	$24, mhdr2 + 20
	jne	bad_cmsg32
	/* msg_flags at 24 was written... */
	cmpl	$0xaaaaaaaa, mhdr2 + 24
	je	bad_cmsg32
	/* ...and nothing past the end of the envelope was: 28 is where a
	 * native `msg_controllen` would reach from 40, and 48 is the native
	 * `msg_flags` exactly. */
	cmpl	$0xaaaaaaaa, mhdr2 + 28
	jne	bad_cmsg32
	cmpl	$0xaaaaaaaa, mhdr2 + 40
	jne	bad_cmsg32
	cmpl	$0xaaaaaaaa, mhdr2 + 48
	jne	bad_cmsg32

	/* The header that came back, at the narrow width. */
	cmpl	$24, rctrl + 0
	jne	bad_cmsg32
	cmpl	$1, rctrl + 4
	jne	bad_cmsg32
	cmpl	$1, rctrl + 8
	jne	bad_cmsg32
	/* And nothing past the 24 bytes it reported. */
	cmpl	$0xaaaaaaaa, rctrl + 24
	jne	bad_cmsg32

	/* The three descriptors are descriptors: `fcntl(F_GETFD)` on each,
	 * which is the cheapest question a live fd answers and a stale number
	 * does not. */
	movl	rctrl + 12, %ebx
	call	check_passed_fd
	movl	rctrl + 16, %ebx
	call	check_passed_fd
	movl	rctrl + 20, %ebx
	call	check_passed_fd

	/* Step 63: a batch of two messages, by `sendmmsg` and `recvmmsg`
	 * through `socketcall`, which is the only door a 32-bit program has
	 * to either.
	 *
	 * ```text
	 * struct mmsghdr, i386: 32 bytes      native: 64 bytes
	 *   msg_hdr       0, 28                 msg_hdr       0, 56
	 *   msg_len      28, 4                  msg_len      56, 4
	 *                                       4 bytes of padding
	 * ```
	 *
	 * TWO entries, because the first one sits at offset 0 whatever the
	 * stride is: a batch of one says nothing about it. With the native
	 * stride the second entry is looked for at 64, which in a 32-bit
	 * array is where entry THREE would start --- so the second message's
	 * envelope comes out of whatever follows the array, and its transfer
	 * count is written there too.
	 *
	 * The counts are the sharper half of the check: they are 1 and 2
	 * bytes, so they differ from each other and from any length the
	 * kernel could have written by accident. They are poisoned first. */
	call	poison_mmsg
	movl	$1, sargs + 0		/* AF_UNIX */
	movl	$1, sargs + 4		/* SOCK_STREAM */
	movl	$0, sargs + 8
	movl	$bsv, sargs + 12
	movl	$102, %eax
	movl	$8, %ebx		/* SYS_SOCKETPAIR */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_sendmmsg32

	/* One byte in the first message, two in the second. */
	movl	$0x41, bbuf + 0
	movl	$0x4342, bbuf + 4
	movl	$bbuf, biov + 0
	movl	$1, biov + 4
	movl	$bbuf + 4, biov + 8
	movl	$2, biov + 12

	/* Entry 0 at bmsg + 0 and entry 1 at bmsg + 32: the envelope's seven
	 * words and then the count, which the kernel writes. */
	movl	$0, bmsg + 0
	movl	$0, bmsg + 4
	movl	$biov, bmsg + 8
	movl	$1, bmsg + 12
	movl	$0, bmsg + 16
	movl	$0, bmsg + 20
	movl	$0, bmsg + 24
	movl	$0, bmsg + 32
	movl	$0, bmsg + 36
	movl	$biov + 8, bmsg + 40
	movl	$1, bmsg + 44
	movl	$0, bmsg + 48
	movl	$0, bmsg + 52
	movl	$0, bmsg + 56

	movl	bsv + 0, %eax
	movl	%eax, sargs + 0
	movl	$bmsg, sargs + 4
	movl	$2, sargs + 8		/* vlen */
	movl	$0, sargs + 12
	movl	$102, %eax
	movl	$20, %ebx		/* SYS_SENDMMSG */
	movl	$sargs, %ecx
	int	$0x80
	cmpl	$2, %eax
	jne	bad_sendmmsg32

	/* The two counts, at 28 of each 32-byte entry. One and two, which no
	 * other write in this call could have produced. */
	cmpl	$1, bmsg + 28
	jne	bad_mmsg_count
	cmpl	$2, bmsg + 60
	jne	bad_mmsg_count
	/* And the word a native `msg_len` would have gone to --- 56 of entry
	 * 0, which is 24 into entry 1 --- is still this program's own zero,
	 * because that is where the envelope of entry 1 put its msg_flags. */

	/* And back. Two destinations, two entries, the counts poisoned again.
	 * A stream socket coalesces, so what the receive has to show is that
	 * it walked two entries at all. */
	call	poison_mmsg
	movl	$0, bbuf2 + 0
	movl	$0, bbuf2 + 4
	movl	$bbuf2, biov2 + 0
	movl	$1, biov2 + 4
	movl	$bbuf2 + 4, biov2 + 8
	movl	$2, biov2 + 12
	movl	$0, bmsg + 0
	movl	$0, bmsg + 4
	movl	$biov2, bmsg + 8
	movl	$1, bmsg + 12
	movl	$0, bmsg + 16
	movl	$0, bmsg + 20
	movl	$0, bmsg + 24
	movl	$0, bmsg + 32
	movl	$0, bmsg + 36
	movl	$biov2 + 8, bmsg + 40
	movl	$1, bmsg + 44
	movl	$0, bmsg + 48
	movl	$0, bmsg + 52
	movl	$0, bmsg + 56

	movl	bsv + 4, %eax
	movl	%eax, sargs + 0
	movl	$bmsg, sargs + 4
	movl	$2, sargs + 8
	movl	$0, sargs + 12
	movl	$0, sargs + 16		/* timeout: none */
	movl	$102, %eax
	movl	$19, %ebx		/* SYS_RECVMMSG */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jle	bad_recvmmsg32
	/* The first entry's count was written where a 32-bit entry keeps it,
	 * and it is the one byte the first destination asked for. */
	cmpl	$1, bmsg + 28
	jne	bad_mmsg_count
	cmpl	$0x41, bbuf2
	jne	bad_mmsg_count

	/* Step 64: `setsockopt`/`getsockopt` of `SO_RCVTIMEO`, the last two of
	 * the twenty `socketcall` numbers, and the only two whose conversion
	 * depends not on the call but on WHICH OPTION it is. `optval` is a
	 * byte buffer: for `SO_REUSEADDR` it is an `int` and the same in both
	 * ABIs, for `SO_RCVTIMEO` it is a `struct timeval`, which is eight
	 * bytes here and sixteen to the kernel.
	 *
	 * The two halves fail differently, and the second is why this step
	 * exists. Going in, the eight bytes this program owns are half a
	 * native `timeval` and the call is refused outright: EINVAL, so a
	 * program that sets a socket timeout before using the socket never
	 * gets to the read. Coming back, a sixteen-byte answer clamped to this
	 * buffer's eight is the native `tv_sec` alone --- little-endian, so
	 * the SECONDS come back right and `tv_usec` comes out of the seconds'
	 * high half, which is zero. So a whole-second timeout round-trips
	 * correctly with no conversion at all.
	 *
	 * Which is why the timeout here has microseconds in it: 500 000 is the
	 * field that a clamp loses and a conversion keeps. */
	movl	$5, tvin + 0
	movl	$500000, tvin + 4

	movl	$2, sargs + 0		/* AF_INET */
	movl	$2, sargs + 4		/* SOCK_DGRAM */
	movl	$0, sargs + 8
	movl	$102, %eax
	movl	$1, %ebx		/* SYS_SOCKET */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	js	bad_optsock
	movl	%eax, optfd

	movl	optfd, %eax
	movl	%eax, sargs + 0
	movl	$1, sargs + 4		/* SOL_SOCKET */
	movl	$20, sargs + 8		/* SO_RCVTIMEO */
	movl	$tvin, sargs + 12
	movl	$8, sargs + 16		/* the width THIS program's timeval is */
	movl	$102, %eax
	movl	$14, %ebx		/* SYS_SETSOCKOPT */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jne	bad_setsockopt32

	/* 0xaa over the whole output buffer, so that a sixteen-byte write into
	 * a buffer that asked for eight is visible as poison gone rather than
	 * as a fault: the buffer is given 32 bytes of this program's own. */
	movl	$0xaaaaaaaa, tvout + 0
	movl	$0xaaaaaaaa, tvout + 4
	movl	$0xaaaaaaaa, tvout + 8
	movl	$0xaaaaaaaa, tvout + 12
	movl	$0xaaaaaaaa, tvout + 16
	movl	$0xaaaaaaaa, tvout + 20
	movl	$0xaaaaaaaa, tvout + 24
	movl	$0xaaaaaaaa, tvout + 28
	movl	$8, optlenw

	movl	optfd, %eax
	movl	%eax, sargs + 0
	movl	$1, sargs + 4
	movl	$20, sargs + 8
	movl	$tvout, sargs + 12
	movl	$optlenw, sargs + 16
	movl	$102, %eax
	movl	$15, %ebx		/* SYS_GETSOCKOPT */
	movl	$sargs, %ecx
	int	$0x80
	testl	%eax, %eax
	jne	bad_getsockopt32

	/* Eight bytes reported, not sixteen: a caller told sixteen goes on to
	 * read eight bytes the kernel never wrote. */
	cmpl	$8, optlenw
	jne	bad_timeo_value
	cmpl	$5, tvout + 0
	jne	bad_timeo_value
	/* The microseconds, which is the whole step. */
	cmpl	$500000, tvout + 4
	jne	bad_timeo_value
	/* And nothing past the eight bytes this buffer asked for. */
	cmpl	$0xaaaaaaaa, tvout + 8
	jne	bad_timeo_value
	cmpl	$0xaaaaaaaa, tvout + 12
	jne	bad_timeo_value

	/* And the length itself, which is an `int`: `-1` is a word of
	 * 0xffffffff in the block, and a kernel that zero-extends it sees four
	 * billion --- more than the eight bytes the conversion needs, so the
	 * conversion reads its eight, never looks at the rest and the call
	 * SUCCEEDS on a length `__sys_setsockopt` refuses outright
	 * (`if (optlen < 0) return -EINVAL`). Copilot found that on this
	 * batch. EINVAL is 22. */
	movl	optfd, %eax
	movl	%eax, sargs + 0
	movl	$1, sargs + 4
	movl	$20, sargs + 8
	movl	$tvin, sargs + 12
	movl	$-1, sargs + 16
	movl	$102, %eax
	movl	$14, %ebx		/* SYS_SETSOCKOPT */
	movl	$sargs, %ecx
	int	$0x80
	cmpl	$-22, %eax
	jne	bad_optlen_sign

	movl	$6, %eax		/* close */
	movl	optfd, %ebx
	int	$0x80

	/* Step 74: a signal handler, which is the first thing in this probe
	 * that the KERNEL has to build a structure for rather than read one.
	 * Everything before it converts something a program wrote; this is
	 * the other direction, and it has four parts that can each be wrong
	 * on their own:
	 *
	 *   - `rt_sigaction` (174) takes a TWENTY-byte struct: three of its
	 *     four members halve and `sa_mask` does not, because `_NSIG` is
	 *     64 in both ABIs;
	 *   - the frame is `struct rt_sigframe_ia32`, whose first four words
	 *     ARE the handler's arguments, because i386 passes arguments on
	 *     the stack and not in registers;
	 *   - the `siginfo` in it is 128 bytes whose union starts at byte 12
	 *     and not at byte 16;
	 *   - and the handler's `ret` goes to `sa_restorer`, which makes the
	 *     `rt_sigreturn` (173) that reads the frame back.
	 *
	 * The handler records what it was given and returns; everything is
	 * checked here, afterwards, so that a handler that never ran and a
	 * handler that ran with rubbish are different failures. */

	/* First the alternate stack, which is twelve bytes to this program
	 * and twenty-four natively: the size is at byte 8 here and at byte 16
	 * there. */
	movl	$altstk, ssin + 0
	movl	$0, ssin + 4			/* ss_flags */
	movl	$8192, ssin + 8			/* ss_size */
	call	poison_ssout
	movl	$186, %eax			/* sigaltstack */
	movl	$ssin, %ebx
	movl	$ssout, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_sigaltstack32
	/* Read it back, into the poisoned buffer, and require twelve bytes:
	 * the stack, zero flags, the size --- and the poison still there at
	 * byte 12, where a native twenty-four-byte answer would have put the
	 * low half of `ss_size`. */
	call	poison_ssout
	movl	$186, %eax
	xorl	%ebx, %ebx
	movl	$ssout, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_sigaltstack32
	cmpl	$altstk, ssout + 0
	jne	bad_sigaltstack32
	cmpl	$0, ssout + 4
	jne	bad_sigaltstack32
	cmpl	$8192, ssout + 8
	jne	bad_sigaltstack32
	cmpl	$0xaaaaaaaa, ssout + 12
	jne	bad_sigaltstack32

	/* Now the handler. `SA_SIGINFO | SA_RESTORER` is what musl's
	 * `__libc_sigaction` sets, and `SA_RESTORER` is not optional on
	 * i386: there is no other way back into the kernel from a handler. */
	movl	$sighandler, sigact + 0
	movl	$0x04000004, sigact + 4		/* SA_SIGINFO | SA_RESTORER */
	movl	$sigrestore, sigact + 8
	movl	$0, sigact + 12			/* sa_mask, low word */
	movl	$0, sigact + 16			/* and high word */
	movl	$174, %eax			/* rt_sigaction */
	movl	$10, %ebx			/* SIGUSR1 */
	movl	$sigact, %ecx
	xorl	%edx, %edx			/* no oldact */
	movl	$8, %esi			/* sigsetsize */
	int	$0x80
	testl	%eax, %eax
	jnz	bad_sigaction32

	/* And send it to ourselves, which is also what makes `si_pid`
	 * checkable: it is this process. */
	movl	$20, %eax			/* getpid */
	int	$0x80
	movl	%eax, mypid
	movl	%eax, %ebx
	movl	$37, %eax			/* kill */
	movl	$10, %ecx			/* SIGUSR1 */
	int	$0x80
	testl	%eax, %eax
	js	bad_raise32

	/* The handler runs on the way back out of that `kill`, so by here it
	 * has run and returned through `rt_sigreturn`. If it had not, this is
	 * where it shows. */
	cmpl	$1, ranflag
	jne	bad_handler_ran

	/* Its three arguments, which it read off its own stack. A kernel
	 * that passed them the x86_64 way --- in %edi, %esi, %edx --- leaves
	 * whatever the interrupted code had at 4(%esp), and the handler reads
	 * that instead. */
	cmpl	$10, gotsig
	jne	bad_handler_args

	/* And where they point, which is the layout of the frame itself:
	 * `esp` at the four-word header, then the 128-byte `siginfo`, then
	 * the `ucontext`. This is the one check that measures the FRAME and
	 * not a field in it. */
	movl	gotsp, %eax
	addl	$16, %eax
	cmpl	%eax, gotinfo
	jne	bad_frame32
	movl	gotinfo, %eax
	addl	$128, %eax
	cmpl	%eax, gotuc
	jne	bad_frame32

	/* The `siginfo`: `si_signo`, `si_errno`, `si_code` --- and then the
	 * union at byte TWELVE, where `si_pid` is. Natively it starts at
	 * byte 16, so a kernel that copied its own struct across hands this
	 * program four bytes of padding as the sender's pid. */
	movl	gotinfo, %eax
	cmpl	$10, 0(%eax)			/* si_signo */
	jne	bad_siginfo32
	cmpl	$0, 4(%eax)			/* si_errno */
	jne	bad_siginfo32
	cmpl	$0, 8(%eax)			/* si_code: SI_USER */
	jne	bad_siginfo32
	movl	12(%eax), %edx			/* si_pid */
	cmpl	mypid, %edx
	jne	bad_siginfo32

	/* And the `ucontext`: `uc_flags`, `uc_link`, a twelve-byte
	 * `uc_stack`, then the 88-byte `sigcontext_32`. Its `ip` is at byte
	 * 56 of that and its `cs` at byte 60, and `cs` is the field that says
	 * whose frame this is: 0x23 is a 32-bit user code selector and 0x33
	 * is a 64-bit one. */
	movl	gotuc, %eax
	movl	20 + 56(%eax), %edx		/* uc_mcontext.ip */
	testl	%edx, %edx
	jz	bad_ucontext32
	movzwl	20 + 60(%eax), %edx		/* uc_mcontext.cs */
	cmpl	$0x23, %edx
	jne	bad_ucontext32
	movl	20 + 28(%eax), %edx		/* uc_mcontext.sp */
	testl	%edx, %edx
	jz	bad_ucontext32

	/* Put the alternate stack back the way it was, so nothing after this
	 * step inherits it. `SS_DISABLE` is 2. */
	movl	$0, ssin + 0
	movl	$2, ssin + 4
	movl	$0, ssin + 8
	movl	$186, %eax
	movl	$ssin, %ebx
	xorl	%ecx, %ecx
	int	$0x80

	/* Step 75: the `*_time64` calls, which are the one family where the
	 * width of a structure is a property of the CALL and not of this
	 * process. The same program gets eight bytes from `clock_gettime`
	 * (265) and SIXTEEN from `clock_gettime64` (403), because
	 * `struct __kernel_timespec` is two 64-bit words at every width ---
	 * that is what the `64` in the name means.
	 *
	 * It matters because a musl built for i386 in the last few years
	 * calls only the 64-bit form: with 403 answering `ENOSYS` a 32-bit
	 * program has no clock at all, not a wrong time.
	 *
	 * The step makes both calls into the same poisoned buffer and
	 * requires the two answers to be DIFFERENT widths. Either one alone
	 * would pass on a kernel that got everything the same way round. */
	call	poison_tbuf
	movl	$265, %eax		/* clock_gettime */
	movl	$1, %ebx		/* CLOCK_MONOTONIC */
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_gettime_narrow
	/* Eight bytes, and the poison still there after them. */
	cmpl	$0xaaaaaaaa, tbuf + 8
	jne	bad_gettime_narrow
	cmpl	$0xaaaaaaaa, tbuf + 12
	jne	bad_gettime_narrow

	call	poison_tbuf
	movl	$403, %eax		/* clock_gettime64 */
	movl	$1, %ebx		/* CLOCK_MONOTONIC */
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_gettime64
	/* Sixteen bytes. The seconds of a monotonic clock are a small number
	 * and its nanoseconds are anything, so no single word is reliably
	 * not the poison --- but the HIGH half of each 64-bit field is a
	 * zero, and a zero is not 0xaaaaaaaa. So the discriminating check is
	 * that bytes 8..12 (the low half of a 64-bit `tv_nsec`) are no longer
	 * the poison, which only a sixteen-byte write can do. */
	cmpl	$0xaaaaaaaa, tbuf + 8
	je	bad_gettime64
	cmpl	$0, tbuf + 4		/* tv_sec's high half */
	jne	bad_gettime64
	cmpl	$0, tbuf + 12		/* tv_nsec's high half */
	jne	bad_gettime64
	/* And nothing past the structure. */
	cmpl	$0xaaaaaaaa, tbuf + 16
	jne	bad_gettime64
	cmpl	$0xaaaaaaaa, tbuf + 20
	jne	bad_gettime64

	/* `clock_getres_time64` (406), whose answer is `{0, 1}`: the one call
	 * here whose VALUES say which layout was written, so a kernel that
	 * wrote the right number of bytes in the wrong places is caught too. */
	call	poison_tbuf
	movl	$406, %eax
	movl	$1, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_getres64
	cmpl	$0, tbuf + 0		/* tv_sec, low half */
	jne	bad_getres64
	cmpl	$0, tbuf + 4		/* tv_sec, high half */
	jne	bad_getres64
	cmpl	$1, tbuf + 8		/* tv_nsec, low half: 1 ns */
	jne	bad_getres64
	cmpl	$0, tbuf + 12
	jne	bad_getres64
	cmpl	$0xaaaaaaaa, tbuf + 16
	jne	bad_getres64

	/* `clock_nanosleep_time64` (407). A request of zero sleeps for
	 * nothing at either width, so the discriminating bytes are a 64-bit
	 * `tv_sec` of zero and a 64-bit `tv_nsec` of two thousand million:
	 * over a billion, so `EINVAL` when read as sixteen bytes --- while
	 * the SAME bytes read eight at a time are `{0, 0}`, a valid request
	 * that returns 0. EINVAL is 22. */
	movl	$0, tbuf + 0
	movl	$0, tbuf + 4
	movl	$2000000000, tbuf + 8
	movl	$0, tbuf + 12
	movl	$407, %eax
	movl	$1, %ebx		/* CLOCK_MONOTONIC */
	xorl	%ecx, %ecx		/* flags: relative */
	movl	$tbuf, %edx
	xorl	%esi, %esi		/* no remainder */
	int	$0x80
	cmpl	$-22, %eax
	jne	bad_nanosleep64

	/* Step 76: the rest of the `*_time64` family this kernel answers ---
	 * `timerfd_gettime64` (410), `timerfd_settime64` (411) and
	 * `futex_time64` (422).
	 *
	 * These three are what a threaded 32-bit program actually waits on.
	 * 422 is the number behind every `pthread_cond_timedwait`, every
	 * `sem_timedwait` and every `pthread_mutex_timedlock` of a modern
	 * i386 musl, and the two `timerfd` numbers are behind every timer it
	 * arms. A `struct itimerspec` is TWO `timespec`s, so sixteen bytes at
	 * this program's own width and thirty-two at the `_time64` one: a
	 * kernel that got the width from the process rather than from the
	 * number would hand back the interval where the value belongs.
	 *
	 * As in step 75, each pair is made from this one program so that
	 * neither half can pass on its own. */
	movl	$322, %eax		/* timerfd_create */
	movl	$1, %ebx		/* CLOCK_MONOTONIC */
	xorl	%ecx, %ecx
	int	$0x80
	testl	%eax, %eax
	js	bad_timerfd_create
	movl	%eax, tfd

	/* Armed through the NARROW number, so what follows is not one width
	 * all the way through: sixteen bytes in, and sixteen or thirty-two
	 * out. { it_interval = {0, 0}, it_value = {100, 0} }. */
	movl	$0, its + 0
	movl	$0, its + 4
	movl	$100, its + 8
	movl	$0, its + 12
	movl	$325, %eax		/* timerfd_settime */
	movl	tfd, %ebx
	xorl	%ecx, %ecx		/* relative */
	movl	$its, %edx
	xorl	%esi, %esi		/* no old value */
	int	$0x80
	testl	%eax, %eax
	jnz	bad_timerfd_arm

	/* 326: sixteen bytes, interval zero, value a hair under a hundred
	 * seconds, and the poison untouched after them. */
	call	poison_tbuf
	movl	$326, %eax		/* timerfd_gettime */
	movl	tfd, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_timerfd_narrow
	cmpl	$0, tbuf + 0
	jne	bad_timerfd_narrow
	cmpl	$0, tbuf + 4
	jne	bad_timerfd_narrow
	cmpl	$100, tbuf + 8		/* it_value.tv_sec, narrow offset */
	ja	bad_timerfd_narrow
	cmpl	$90, tbuf + 8
	jb	bad_timerfd_narrow
	cmpl	$0xaaaaaaaa, tbuf + 16
	jne	bad_timerfd_narrow
	cmpl	$0xaaaaaaaa, tbuf + 20
	jne	bad_timerfd_narrow

	/* 410: the same timer, thirty-two bytes. `it_value.tv_sec` is now at
	 * byte 16 and its high half at byte 20 --- which is where the narrow
	 * layout keeps nothing at all, so a kernel that wrote sixteen bytes
	 * leaves the poison there and this fails. */
	call	poison_tbuf
	movl	$410, %eax		/* timerfd_gettime64 */
	movl	tfd, %ebx
	movl	$tbuf, %ecx
	int	$0x80
	testl	%eax, %eax
	jnz	bad_timerfd64
	cmpl	$0, tbuf + 0		/* it_interval, both words of both */
	jne	bad_timerfd64
	cmpl	$0, tbuf + 4
	jne	bad_timerfd64
	cmpl	$0, tbuf + 8
	jne	bad_timerfd64
	cmpl	$0, tbuf + 12
	jne	bad_timerfd64
	cmpl	$100, tbuf + 16		/* it_value.tv_sec, wide offset */
	ja	bad_timerfd64
	cmpl	$90, tbuf + 16
	jb	bad_timerfd64
	cmpl	$0, tbuf + 20		/* its high half */
	jne	bad_timerfd64
	cmpl	$0xaaaaaaaa, tbuf + 32	/* and nothing past the structure */
	jne	bad_timerfd64

	/* 411 the other way: thirty-two bytes whose WIDE reading has an
	 * `it_value.tv_nsec` of two thousand million --- over a billion, so
	 * EINVAL --- while the same bytes read sixteen at a time are four
	 * zero words, a perfectly good disarm that returns 0. EINVAL is 22. */
	movl	$0, its + 0
	movl	$0, its + 4
	movl	$0, its + 8
	movl	$0, its + 12
	movl	$0, its + 16
	movl	$0, its + 20
	movl	$2000000000, its + 24
	movl	$0, its + 28
	movl	$411, %eax		/* timerfd_settime64 */
	movl	tfd, %ebx
	xorl	%ecx, %ecx
	movl	$its, %edx
	xorl	%esi, %esi
	int	$0x80
	cmpl	$-22, %eax
	jne	bad_timerfd_settime64

	movl	$6, %eax		/* close the timerfd */
	movl	tfd, %ebx
	int	$0x80

	/* 422: the futex timeout. The same discriminating bytes --- a 64-bit
	 * `tv_sec` of zero and a 64-bit `tv_nsec` of two thousand million ---
	 * which are EINVAL read as sixteen and `{0, 0}` read as eight. So
	 * 422 must answer -22, and i386's own 240, which DOES read eight,
	 * must park for nothing and come back ETIMEDOUT (110). Two different
	 * errors, so neither can be reached by accident. */
	movl	$0, tbuf + 0
	movl	$0, tbuf + 4
	movl	$2000000000, tbuf + 8
	movl	$0, tbuf + 12
	movl	$0, fword
	movl	$422, %eax		/* futex_time64 */
	movl	$fword, %ebx
	movl	$128, %ecx		/* FUTEX_WAIT | FUTEX_PRIVATE_FLAG */
	xorl	%edx, %edx		/* the value we expect: 0 */
	movl	$tbuf, %esi
	xorl	%edi, %edi
	int	$0x80
	cmpl	$-22, %eax
	jne	bad_futex_time64

	movl	$240, %eax		/* futex */
	movl	$fword, %ebx
	movl	$128, %ecx
	xorl	%edx, %edx
	movl	$tbuf, %esi
	xorl	%edi, %edi
	int	$0x80
	cmpl	$-110, %eax
	jne	bad_futex_narrow

	/* Step 77: `ipc` (117), the eleven System V calls behind one number.
	 *
	 * i386 never got the separate `semget`/`msgsnd`/`shmat` syscalls, so
	 * every System V program on this ABI goes through here. What makes
	 * the step worth having is that the MULTIPLEXER itself reads and
	 * writes this program's memory in three places --- places that are
	 * in no handler at all --- and that `struct msgbuf` keeps its type
	 * in a `long`, which is four bytes here and eight to the kernel.
	 *
	 * Sub-calls: SEMOP 1, SEMGET 2, SEMCTL 3, MSGSND 11, MSGRCV 12,
	 * MSGGET 13, MSGCTL 14, SHMAT 21, SHMDT 22, SHMGET 23, SHMCTL 24.
	 * The version is `call >> 16`.
	 *
	 * `0x100` is `__IPC_64`, which an i386 glibc ORs into the command of
	 * EVERY `*ctl`. Sent that way here on purpose: a kernel that does not
	 * take the bit off again answers EINVAL to all of them, `IPC_RMID`
	 * included. */

	/* shmget(IPC_PRIVATE, 4096, IPC_CREAT|0600) */
	movl	$117, %eax
	movl	$23, %ebx
	xorl	%ecx, %ecx		/* IPC_PRIVATE */
	movl	$4096, %edx
	movl	$0x600, %esi
	orl	$0x200, %esi		/* IPC_CREAT */
	xorl	%edi, %edi
	int	$0x80
	testl	%eax, %eax
	js	bad_shmget
	movl	%eax, shmid

	/* shmat: the address comes back in `third`, in FOUR bytes, and the
	 * call answers 0. Eight would drop a zero high half into the word
	 * after it, which is why `ipcout + 4` is checked. */
	movl	$0xaaaaaaaa, ipcout + 0
	movl	$0xaaaaaaaa, ipcout + 4
	movl	$117, %eax
	movl	$21, %ebx
	movl	shmid, %ecx
	xorl	%edx, %edx		/* no flags */
	movl	$ipcout, %esi		/* third: where the address goes */
	xorl	%edi, %edi		/* ptr: any address */
	int	$0x80
	testl	%eax, %eax
	jnz	bad_shmat		/* it answers 0, NOT the address */
	cmpl	$0xaaaaaaaa, ipcout + 0
	je	bad_shmat		/* nothing was stored */
	cmpl	$0, ipcout + 0
	je	bad_shmat		/* a null pointer was stored */
	cmpl	$0xaaaaaaaa, ipcout + 4
	jne	bad_shmat		/* EIGHT bytes were stored */
	movl	ipcout + 0, %eax
	movl	%eax, shmaddr

	/* And the segment is really there: write a word and read it back. */
	movl	shmaddr, %edi
	movl	$0x5a5a5a5a, (%edi)
	cmpl	$0x5a5a5a5a, (%edi)
	jne	bad_shmat

	/* shmdt takes its address in `ptr`, not in `first`. */
	movl	$117, %eax
	movl	$22, %ebx
	xorl	%ecx, %ecx
	xorl	%edx, %edx
	xorl	%esi, %esi
	movl	shmaddr, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_shmdt

	/* shmctl(id, IPC_RMID|__IPC_64, 0) */
	movl	$117, %eax
	movl	$24, %ebx
	movl	shmid, %ecx
	movl	$0x100, %edx		/* IPC_RMID | __IPC_64 */
	xorl	%esi, %esi
	xorl	%edi, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_ipc_rmid

	/* msgget, then a message whose type is 1 and whose payload is the
	 * four bytes "ABCD". At this program's width those are bytes 0..4
	 * and 4..8 of the `struct msgbuf`. Read eight bytes as one `long`,
	 * the type becomes 0x44434241_00000001 --- a large POSITIVE number,
	 * so not an error: the message would be queued under a type nobody
	 * asks for, with its payload taken from four bytes further on. */
	movl	$117, %eax
	movl	$13, %ebx
	xorl	%ecx, %ecx
	movl	$0x600, %edx
	orl	$0x200, %edx		/* IPC_CREAT */
	xorl	%esi, %esi
	xorl	%edi, %edi
	int	$0x80
	testl	%eax, %eax
	js	bad_msg
	movl	%eax, msqid

	movl	$1, msgbuf + 0		/* mtype */
	movl	$0x44434241, msgbuf + 4	/* "ABCD", little-endian */
	movl	$117, %eax
	movl	$11, %ebx		/* MSGSND */
	movl	msqid, %ecx
	movl	$4, %edx		/* msgsz: the payload only */
	xorl	%esi, %esi		/* msgflg */
	movl	$msgbuf, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_msg			/* msgsnd(2) answers 0 */

	/* MSGRCV version 0, which is the form glibc's i386 `msgrcv` has
	 * always used: `ptr` points at a `struct ipc_kludge { void *msgp;
	 * long msgtyp; }` and NOT at the buffer. A kernel that ignores the
	 * version takes the address of the kludge as the buffer.
	 *
	 * The type asked for is -5, which is `msgrcv`'s "the lowest type not
	 * above five" mode, so the sign matters: read unsigned it is four
	 * thousand million and nothing ever matches. IPC_NOWAIT (0o4000) so
	 * that a wrong type is ENOMSG and not a wait for ever. */
	movl	$msgout, kludge + 0
	movl	$-5, kludge + 4
	movl	$0xaaaaaaaa, msgout + 0
	movl	$0xaaaaaaaa, msgout + 4
	movl	$0xaaaaaaaa, msgout + 8
	movl	$117, %eax
	movl	$12, %ebx		/* MSGRCV, version 0 */
	movl	msqid, %ecx
	movl	$4, %edx
	movl	$04000, %esi		/* IPC_NOWAIT */
	movl	$kludge, %edi
	int	$0x80
	cmpl	$4, %eax
	jne	bad_msg
	cmpl	$1, msgout + 0
	jne	bad_msg			/* the type, in its four bytes */
	cmpl	$0x44434241, msgout + 4
	jne	bad_msg			/* the payload, at msgp + 4 */
	cmpl	$0xaaaaaaaa, msgout + 8
	jne	bad_msg			/* and nothing after it */

	movl	$117, %eax
	movl	$14, %ebx		/* MSGCTL */
	movl	msqid, %ecx
	movl	$0x100, %edx		/* IPC_RMID | __IPC_64 */
	xorl	%esi, %esi
	xorl	%edi, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_ipc_rmid

	/* semget, then SETVAL through SEMCTL --- the one sub-call where `ptr`
	 * does NOT point at the structure: it points at a word HOLDING the
	 * `union semun`, and that word is four bytes. GETVAL afterwards is
	 * what says the value arrived, with no structure anywhere for it to
	 * have come from by accident. */
	movl	$117, %eax
	movl	$2, %ebx		/* SEMGET */
	xorl	%ecx, %ecx
	movl	$1, %edx		/* one semaphore */
	movl	$0x600, %esi
	orl	$0x200, %esi		/* IPC_CREAT */
	xorl	%edi, %edi
	int	$0x80
	testl	%eax, %eax
	js	bad_sem
	movl	%eax, semid

	movl	$7, semarg
	movl	$117, %eax
	movl	$3, %ebx		/* SEMCTL */
	movl	semid, %ecx
	xorl	%edx, %edx		/* semnum */
	movl	$16, %esi		/* SETVAL */
	orl	$0x100, %esi		/* | __IPC_64 */
	movl	$semarg, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_sem

	movl	$117, %eax
	movl	$3, %ebx
	movl	semid, %ecx
	xorl	%edx, %edx
	movl	$12, %esi		/* GETVAL */
	orl	$0x100, %esi
	movl	$semarg, %edi
	int	$0x80
	cmpl	$7, %eax
	jne	bad_sem

	movl	$117, %eax
	movl	$3, %ebx
	movl	semid, %ecx
	xorl	%edx, %edx
	movl	$0x100, %esi		/* IPC_RMID | __IPC_64 */
	movl	$semarg, %edi
	int	$0x80
	testl	%eax, %eax
	jnz	bad_ipc_rmid

	/* Step 61: `execve` from a 32-bit process, which is what a 32-bit
	 * program needs to start any program at all --- a shell, `xargs`, the
	 * second half of a pipeline. Its `argv` and `envp` are arrays of
	 * FOUR-byte pointers ended by a four-byte NULL, and a kernel that
	 * reads them eight bytes at a time does not fault: it pairs the halves
	 * of two entries into one pointer and hands the new program arguments
	 * nobody passed. So the shape of the step is not "did the call
	 * return" --- a successful `execve` never returns --- but "does the
	 * program that comes out of it agree with the one that went in".
	 *
	 * This run execs ITSELF with one argument added, so the new image runs
	 * the whole probe again and, finding `argc >= 2`, exits 0 without
	 * exec'ing a third time. The status this program finally reports is
	 * therefore the SECOND image's: if the arguments arrived wrong the
	 * exec'd image fails `bad_argc` or `bad_argv` and says so with its own
	 * number, and if the exec is refused outright this one reports
	 * 192 plus the errno --- `ENOENT` is 2 and `EFAULT` is 14, so 194 and
	 * 206 are the two to expect, and no errno reaches the 128 and 160
	 * bands above. */
	cmpl	$2, saved_argc
	jge	all_steps_passed

	/* { argv[0], "x", NULL }, written as four-byte entries: this is the
	 * array a 32-bit program builds, not one this program could borrow
	 * from its own stack. */
	movl	saved_argv0, %eax
	movl	%eax, xargv + 0
	movl	$xarg1, %eax
	movl	%eax, xargv + 4
	movl	$0, xargv + 8

	movl	$11, %eax		/* execve */
	movl	saved_argv0, %ebx
	movl	$xargv, %ecx
	movl	saved_envp, %edx
	int	$0x80
	negl	%eax
	addl	$192, %eax
	movl	%eax, %ebx
	jmp	die

all_steps_passed:
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
	jmp	die
bad_fstat64:
	movl	$27, %ebx
	jmp	die
bad_fstat64_mode:
	movl	$28, %ebx
	jmp	die
bad_fstat64_size:
	movl	$29, %ebx
	jmp	die
bad_fstat64_ino:
	movl	$30, %ebx
	jmp	die
bad_fcntl64:
	movl	$31, %ebx
	jmp	die
bad_setlk64:
	movl	$32, %ebx
	jmp	die
bad_getlk64:
	movl	$33, %ebx
	jmp	die
bad_fcntl_wide:
	movl	$34, %ebx
	jmp	die
bad_writev:
	movl	$35, %ebx
	jmp	die
bad_readv:
	movl	$36, %ebx
	jmp	die
bad_preadv:
	movl	$37, %ebx
	jmp	die
bad_pwritev:
	movl	$38, %ebx
	jmp	die
bad_clock_gettime:
	movl	$39, %ebx
	jmp	die
bad_gettimeofday:
	movl	$40, %ebx
	jmp	die
bad_clock_getres:
	movl	$41, %ebx
	jmp	die
bad_nanosleep:
	movl	$42, %ebx
	jmp	die
bad_times:
	movl	$43, %ebx
	jmp	die
bad_getrusage:
	movl	$44, %ebx
	jmp	die
bad_itimer:
	movl	$45, %ebx
	jmp	die
bad_select:
	movl	$46, %ebx
	jmp	die
bad_timerfd:
	movl	$47, %ebx
	jmp	die
bad_futex:
	movl	$48, %ebx
	jmp	die
bad_ugetrlimit:
	movl	$49, %ebx
	jmp	die
bad_setrlimit32:
	movl	$50, %ebx
	jmp	die
bad_rlimit_clamp:
	movl	$51, %ebx
	jmp	die
bad_statx:
	movl	$52, %ebx
	jmp	die
bad_ppoll_mask:
	movl	$53, %ebx
	jmp	die
bad_iov_len:
	movl	$54, %ebx
	jmp	die
bad_socketcall:
	movl	$55, %ebx
	jmp	die
	/* The two halves of the guard-page setup, each reporting its own
	 * errno rather than one opaque number: the status is 128 plus the
	 * errno for the `mmap2` and 160 plus it for the `munmap`, so a run
	 * that cannot build the guard says WHY. `ENOMEM` is 12 and `EINVAL`
	 * is 22, so 140 and 150 are the two to expect from the first and
	 * 172 and 182 from the second; no errno is big enough for the two
	 * ranges to meet. Clobbers %eax, which neither path needs again. */
bad_mmap2_pair:
	negl	%eax
	addl	$128, %eax
	movl	%eax, %ebx
	jmp	die
bad_munmap_guard:
	negl	%eax
	addl	$160, %eax
	movl	%eax, %ebx
	jmp	die
bad_sargs_edge:
	movl	$56, %ebx
	jmp	die
bad_sargs_read:
	movl	$57, %ebx
	jmp	die
bad_fioclex:
	movl	$58, %ebx
	jmp	die
bad_tcgets:
	movl	$59, %ebx
	jmp	die
bad_tiocgptn:
	movl	$60, %ebx
	jmp	die
bad_sendmsg32:
	movl	$62, %ebx
	jmp	die
bad_recvmsg32:
	movl	$63, %ebx
	jmp	die
bad_cmsg32:
	movl	$64, %ebx
	jmp	die
bad_passed_fd:
	movl	$65, %ebx
	jmp	die
bad_sendmmsg32:
	movl	$66, %ebx
	jmp	die
bad_recvmmsg32:
	movl	$67, %ebx
	jmp	die
bad_mmsg_count:
	movl	$68, %ebx
	jmp	die
bad_optsock:
	movl	$69, %ebx
	jmp	die
bad_setsockopt32:
	movl	$70, %ebx
	jmp	die
bad_getsockopt32:
	movl	$71, %ebx
	jmp	die
bad_timeo_value:
	movl	$72, %ebx
	jmp	die
bad_optlen_sign:
	movl	$73, %ebx
	jmp	die
bad_sigaltstack32:
	movl	$74, %ebx
	jmp	die
bad_sigaction32:
	movl	$75, %ebx
	jmp	die
bad_raise32:
	movl	$76, %ebx
	jmp	die
bad_handler_ran:
	movl	$77, %ebx
	jmp	die
bad_handler_args:
	movl	$78, %ebx
	jmp	die
bad_frame32:
	movl	$79, %ebx
	jmp	die
bad_siginfo32:
	movl	$80, %ebx
	jmp	die
bad_ucontext32:
	movl	$81, %ebx
	jmp	die
bad_gettime_narrow:
	movl	$82, %ebx
	jmp	die
bad_gettime64:
	movl	$83, %ebx
	jmp	die
bad_getres64:
	movl	$84, %ebx
	jmp	die
bad_nanosleep64:
	movl	$85, %ebx
	jmp	die
bad_timerfd_create:
	movl	$86, %ebx
	jmp	die
bad_timerfd_narrow:
	movl	$87, %ebx
	jmp	die
bad_timerfd64:
	movl	$88, %ebx
	jmp	die
bad_timerfd_settime64:
	movl	$89, %ebx
	jmp	die
bad_futex_time64:
	movl	$90, %ebx
	jmp	die
bad_futex_narrow:
	movl	$91, %ebx
	jmp	die
bad_timerfd_arm:
	movl	$92, %ebx
	jmp	die
bad_shmget:
	movl	$93, %ebx
	jmp	die
bad_shmat:
	movl	$94, %ebx
	jmp	die
bad_shmdt:
	movl	$95, %ebx
	jmp	die
bad_msg:
	movl	$96, %ebx
	jmp	die
bad_sem:
	movl	$97, %ebx
	jmp	die
bad_ipc_rmid:
	movl	$98, %ebx
	jmp	die

	/* 0xaa over the two entries of the batch array, so that each transfer
	 * count the kernel writes is a byte the program did not put there and
	 * each one it does NOT write stays visible. Clobbers %eax, %ecx and
	 * %edi. */
poison_mmsg:
	movl	$bmsg, %edi
	movl	$64, %ecx
	movl	$0xaaaaaaaa, %eax
poison_mmsg_word:
	movl	%eax, (%edi)
	addl	$4, %edi
	subl	$4, %ecx
	jnz	poison_mmsg_word
	ret

	/* `fcntl(%ebx, F_GETFD)`, which answers 0 or 1 for a live descriptor
	 * and `EBADF` for a number that is not one. Jumps to the failure
	 * rather than returning a verdict, because there is nothing else to
	 * do with the answer. Clobbers %eax, %ecx and %edx. */
check_passed_fd:
	movl	$55, %eax		/* fcntl */
	movl	$1, %ecx		/* F_GETFD */
	xorl	%edx, %edx
	int	$0x80
	testl	%eax, %eax
	js	bad_passed_fd
	ret

	/* 0xaa over the receiving envelope's fifty-six bytes and the control
	 * buffer's ninety-six, so that every field step 62 reads afterwards
	 * says by itself whether the kernel wrote it --- and so that the
	 * bytes past the end of a 28-byte envelope can be required to still
	 * be there. Clobbers %eax, %ecx and %edi. */
poison_recv_msg:
	movl	$mhdr2, %edi
	movl	$152, %ecx		/* 56 of envelope + 96 of control */
	movl	$0xaaaaaaaa, %eax
poison_recv_word:
	movl	%eax, (%edi)
	addl	$4, %edi
	subl	$4, %ecx
	jnz	poison_recv_word
	ret

	/* Fill the scratch buffer with 0xaa, so that every structure written
	 * into it afterwards says by itself how many bytes it is: the first
	 * word past the end is still the poison, or the kernel wrote the
	 * native layout. Clobbers %eax, %ecx and %edi, so it is called before
	 * the arguments of the call it is poisoning for. */
poison_tbuf:
	movl	$tbuf, %edi
	movl	$160, %ecx
	movl	$0xaaaaaaaa, %eax
poison_word:
	movl	%eax, (%edi)
	addl	$4, %edi
	subl	$4, %ecx
	jnz	poison_word
	ret

	/* The same poison for `iobuf4`, which is its own buffer and not a
	 * corner of `tbuf`: it has to sit at an address 4 mod 8 and `tbuf`
	 * cannot promise that. Forty-eight bytes, which is past the end of
	 * every structure the ioctl steps write. */
poison_iobuf4:
	movl	$iobuf4, %edi
	movl	$48, %ecx
	movl	$0xaaaaaaaa, %eax
poison_iobuf4_word:
	movl	%eax, (%edi)
	addl	$4, %edi
	subl	$4, %ecx
	jnz	poison_iobuf4_word
	ret

	/* The handler itself, in the i386 calling convention: its arguments
	 * are on its own stack, at 4(%esp), 8(%esp) and 12(%esp), and
	 * 0(%esp) is the return address the kernel put there --- which is
	 * `sa_restorer` and nothing else, because a signal handler has no
	 * caller to return to. Everything is recorded and checked by the
	 * step, so that "the handler did not run" and "the handler ran with
	 * the wrong arguments" are two different failures. */
sighandler:
	movl	%esp, gotsp
	movl	4(%esp), %eax
	movl	%eax, gotsig
	movl	8(%esp), %eax
	movl	%eax, gotinfo
	movl	12(%esp), %eax
	movl	%eax, gotuc
	movl	$1, ranflag
	ret

	/* `sa_restorer`: the trampoline libc passes, and the only way out of
	 * a handler on i386. musl's is these two instructions. It pops
	 * nothing --- that is `sigreturn`'s trampoline, not `rt_sigreturn`'s
	 * --- so the kernel finds the frame four bytes below %esp. */
sigrestore:
	movl	$173, %eax		/* rt_sigreturn */
	int	$0x80
	/* It does not return. If it ever did, fault here rather than run on
	 * with a stack nobody can account for. */
	hlt

	/* 0xaa over the thirty-two bytes the alternate stack is read back
	 * into, so that a native twenty-four-byte answer is visible as bytes
	 * the program did not write. Clobbers %eax, %ecx and %edi. */
poison_ssout:
	movl	$ssout, %edi
	movl	$32, %ecx
	movl	$0xaaaaaaaa, %eax
poison_ssout_word:
	movl	%eax, (%edi)
	addl	$4, %edi
	subl	$4, %ecx
	jnz	poison_ssout_word
	ret

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
	/* Two pieces of one line, so that `writev` has something to gather
	 * whose halves can be told apart. */
msg2a:
	.ascii	"i386-hello: writev gathers "
	.set	msg2alen, . - msg2a
msg2b:
	.ascii	"two entries\n"
	.set	msg2blen, . - msg2b
tmppath:
	.asciz	"/tmp/i386-hello-pwritev"
	/* A pty master, for the terminal ioctls. Both this container's Linux
	 * and Eclipse have it, which is why the ioctl steps use it instead of
	 * descriptor 0: this program's stdin is a pipe as often as a tty, and
	 * `TCGETS` on a pipe is ENOTTY on both. */
ptmxpath:
	.asciz	"/dev/ptmx"
	/* The empty path of AT_EMPTY_PATH, which names the descriptor itself. */
emptystr:
	.asciz	""
	/* The one argument step 61 adds, which is only there to be counted:
	 * the exec'd image reads `argc`, not what the argument says. */
xarg1:
	.asciz	"x"

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
ptmxfd:
	.long	0
	/* 4 mod 8 on purpose, and the steps that use it say so out loud with
	 * a `testl $4` rather than trusting this. */
	.align	8
	.space	4
iobuf4:
	.space	48
fd2:
	.long	0
	/* Two entries of a 32-bit `struct iovec`: a 4-byte base and a 4-byte
	 * length each, sixteen bytes in all where two native entries would be
	 * thirty-two. Filled in at run time, because the buffers it points at
	 * come from mmap2. */
	.align 4
iov:
	.long	0, 0
	.long	0, 0
	/* The ninety-six bytes of a `struct stat64`, and the twenty-four of a
	 * `struct flock64`. The lock asks for the whole file from byte 0:
	 * l_type (F_RDLCK, 0), l_whence (SEEK_SET, 0), l_start (0), l_len
	 * (0, which means "to the end and beyond") and l_pid, which only
	 * F_GETLK fills in. */
	/* The scratch buffer every structure of time is written into, with
	 * room to spare past the biggest of them (`struct rusage`, 72 bytes)
	 * so that the poison past the end has somewhere to live. And the
	 * structures those calls READ, which have to be built by hand: the
	 * point of each is that it is half the native size.
	 *
	 *   nsreq  `struct timespec`,   8 bytes
	 *   itv    `struct itimerval`, 16 bytes
	 *   its    `struct itimerspec`,16 bytes
	 *   seltv  `struct timeval`,    8 bytes, read AND written back
	 *
	 * Each is declared with the room a native one would need, so that a
	 * kernel that read or wrote the wide layout touches this program's
	 * own memory and not the next variable along: the test is the value
	 * that comes back, not a fault. */
	.align 4
tbuf:
	.space	160
	/* The `timerfd` of step 76, and the futex word its `futex_time64`
	 * waits on: an `int` at both widths, so the only thing that changes
	 * about the call is its timeout. */
tfd:
	.long	0
fword:
	.long	0
	/* Step 77's own data: the ids, the address `shmat` stores through
	 * `third`, the `struct ipc_kludge` of the old `msgrcv`, and the word
	 * `SEMCTL` reads the `union semun` out of. */
shmid:
	.long	0
msqid:
	.long	0
semid:
	.long	0
shmaddr:
	.long	0
semarg:
	.long	0
ipcout:
	.space	16
kludge:
	.space	8
msgbuf:
	.space	32
msgout:
	.space	32
nsreq:
	.space	16
itv:
	.space	32
its:
	.space	32
seltv:
	.space	16
fd3:
	.long	0
	/* The futex word, which is a `u32` in both ABIs, its timeout, and the
	 * eight bytes of a narrow `struct rlimit`. Each is given the room a
	 * native one would need, so that a kernel reading or writing the wide
	 * layout touches this program's own memory. */
	.align 4
futex_word:
	.long	0
futex_to:
	.space	16
rl32:
	.space	16
	/* And the buffer `statx` writes: 256 bytes plus the poison behind it,
	 * placed DELIBERATELY at a multiple of 4 that is not a multiple of 8
	 * --- `.align 8` and then four bytes --- because that is the alignment
	 * a 32-bit program's `struct statx *` has and the whole point of the
	 * step that uses it. Its own symbol rather than a corner of `tbuf`, so
	 * that the placement is the assembler's promise and not the linker's
	 * accident. */
	.align 8
statx_pad:
	.long	0
statxbuf:
	.space	320

	/* A `timespec` of zeros, 8-aligned, so that in step 53 the only
	 * argument at a 4-mod-8 address is the signal mask. All zeros is the
	 * same bytes at either width, so one buffer serves both. */
	.align 8
zerots:
	.space	16

	/* And the mask, placed 4 mod 8 on purpose: `zerots` is 16 bytes from
	 * an 8-aligned start, so a single pad word puts this one 4 past a
	 * boundary. The step checks the address rather than trusting this. */
	.long	0
sigmask4:
	.space	8

	/* The socketcall argument block, and what its calls point at. */
sargs:
	.space	24
sv:
	.space	8
sbuf:
	.space	8
sbuf2:
	.space	8
sa:
	.space	128
salen:
	.long	0
twopages:
	.long	0
	.align 4
statbuf:
	.space	96
	/* What step 63 sends its batch of two through. `bmsg` is two 32-byte
	 * entries, which is what a 32-bit `struct mmsghdr` array is; it is
	 * given 96 bytes, half an entry more than it needs, so that a kernel
	 * walking with the NATIVE stride of 64 lands in this program's own
	 * memory and is caught by the poison rather than by a fault. */
	.align 4
bsv:
	.space	8
bbuf:
	.space	16
bbuf2:
	.space	16
biov:
	.space	16
biov2:
	.space	16
bmsg:
	.space	96

	/* What step 62 sends a descriptor through. `mhdr2` and `rctrl` are
	 * ADJACENT and in that order on purpose: `poison_recv_msg` fills the
	 * two in one pass, and the envelope is given the room a NATIVE one
	 * would need (56, not 28) so that a kernel writing at the native
	 * offsets lands in this program's own memory and is caught by the
	 * poison rather than by a fault. */
	.align 4
msv:
	.space	8
mbuf:
	.long	0
mbuf2:
	.long	0
miov:
	.space	8
miov2:
	.space	8
mctrl:
	.space	64
mhdr:
	.space	32
mhdr2:
	.space	56
rctrl:
	.space	96

	/* What step 64 sets and reads its timeout through. `tvout` is given 32
	 * bytes, four times the eight it asks for, so that a kernel writing a
	 * NATIVE sixteen-byte timeval lands in this program's own memory and
	 * is caught by the poison rather than by a fault. */
	.align 4
optfd:
	.long	0
optlenw:
	.long	0
tvin:
	.space	8
tvout:
	.space	32

	/* What step 74 uses. `ssout` is given thirty-two bytes, more than
	 * twice the twelve a `compat_stack_t` is, so that a native
	 * twenty-four-byte answer lands in this program's own memory and is
	 * caught by the poison rather than by a fault. `altstk` is a real
	 * region because `sigaltstack` checks the size against
	 * `MINSIGSTKSZ`; nothing ever runs on it. */
	.align 4
sigact:
	.space	20
ssin:
	.space	12
ssout:
	.space	32
ranflag:
	.long	0
gotsig:
	.long	0
gotinfo:
	.long	0
gotuc:
	.long	0
gotsp:
	.long	0
mypid:
	.long	0
	.align 16
altstk:
	.space	8192

	/* What step 61 keeps from the initial stack, and the array it builds.
	 * `xargv` is given the room a NATIVE read of three entries would
	 * need, so that a kernel reading it eight bytes at a time walks this
	 * program's own memory rather than off the end of the section: the
	 * test is the arguments the new image sees, not a fault. */
	.align 4
saved_argc:
	.long	0
saved_argv0:
	.long	0
saved_envp:
	.long	0
xargv:
	.space	48
lock64:
	.word	0			/* l_type: F_RDLCK */
	.word	0			/* l_whence: SEEK_SET */
	.long	0, 0			/* l_start, 64 bits at offset 4 */
	.long	0, 0			/* l_len, 64 bits at offset 12 */
	.long	0			/* l_pid at offset 20 */
