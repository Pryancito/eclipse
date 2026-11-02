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
	 * descriptor, so this one opens a file of its own under /tmp
	 * --- O_RDWR | O_CREAT | O_TRUNC, mode 0600 --- gathers six bytes
	 * into it from two places, and reads them back with pread64 to check
	 * that the two entries arrived in order and nothing from between them
	 * came along. */
	movl	$5, %eax		/* open */
	movl	$tmppath, %ebx
	movl	$0102, %ecx		/* O_RDWR|O_CREAT|O_TRUNC = 2|64|512 */
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
	.align 4
statbuf:
	.space	96
lock64:
	.word	0			/* l_type: F_RDLCK */
	.word	0			/* l_whence: SEEK_SET */
	.long	0, 0			/* l_start, 64 bits at offset 4 */
	.long	0, 0			/* l_len, 64 bits at offset 12 */
	.long	0			/* l_pid at offset 20 */
