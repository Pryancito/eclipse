# FreeBSD compatibility (FreeBSD/amd64 ABI)

[Español](README-freebsd.md) · **English**

Eclipse OS runs Linux binaries natively. This document describes the
**FreeBSD/amd64 compatibility layer**: the infrastructure that lets an
executable branded as FreeBSD enter the kernel through the same `syscall`
instruction, but speaking the FreeBSD ABI (its own syscall numbers, flag
encoding, struct layout and return convention).

The layer is directly inspired by the
[`freebsd/freebsd-src`](https://github.com/freebsd/freebsd-src) source tree:
each constant table is transcribed from its original header and the comment
cites the corresponding file so it can be checked against upstream.

> **Scope.** This is a **compatibility base**, not a complete FreeBSD kernel. It
> is meant to let a **static** FreeBSD binary make it through libc start-up to
> `main` and perform file, memory, process and time I/O. What is **not** yet
> implemented is listed under [Limitations](#limitations).

It only applies to **x86_64**: the implemented ABI is FreeBSD/amd64. On other
architectures every process runs as Linux.

## How it works

### 1. ABI detection (personality)

When loading an ELF, the loader (`linux-object/src/loader/mod.rs`,
`detect_abi`) decides the process *personality*:

- **`EI_OSABI == ELFOSABI_FREEBSD` (9)** in `e_ident[7]` — the mark FreeBSD's
  toolchain stamps on static executables.
- As a fallback, the `PT_NOTE` segments are scanned for the vendor name
  `"FreeBSD"` (some binaries leave `EI_OSABI` as `SYSV` and use the
  `NT_FREEBSD_ABI_TAG` note).

The personality (`linux_object::process::Abi`) is stored in the
`LinuxProcess`, is **inherited across `fork`** and is **re-evaluated on
`execve`** (a Linux shell can `execve` a FreeBSD binary and vice versa).

### 2. Initial stack and auxiliary vector

A static FreeBSD binary needs a FreeBSD-shaped stack, different from Linux's
(`linux-object/src/loader/abi.rs`, `push_at_freebsd`):

- An auxiliary vector with FreeBSD's `AT_*` types (`sys/sys/elf_common.h`):
  `AT_PHDR/PHENT/PHNUM/BASE/ENTRY/PAGESZ`, plus the FreeBSD-specific
  `AT_EXECPATH`, `AT_CANARY`/`AT_CANARYLEN` (SSP canary), `AT_PAGESIZES`,
  `AT_OSRELDATE`, `AT_NCPUS`, `AT_STACKPROT`, `AT_PS_STRINGS`,
  `AT_USRSTACKBASE`/`LIM`.
- A `struct ps_strings` block and an SSP canary with real random bytes.
- Entry registers as in `exec_setregs` (`sys/amd64/amd64/exec_machdep.c`):
  `%rdi` = pointer to `argc`, `%rsp` aligned to 8 mod 16.

### 3. Syscall dispatch and return convention

In the trap handler (`loader/src/linux.rs`), if the personality is FreeBSD the
syscall is dispatched through `Syscall::bsd_syscall` (`linux-syscall/src/bsd/`)
instead of the Linux path. The amd64 return convention is applied to the user
context (`cpu_set_syscall_retval`, `sys/amd64/amd64/vm_machdep.c`):

| result | `%rax` | `%rdx` | carry (CF) |
|-----------|--------|--------|--------------|
| success   | value  | secondary value | 0 |
| error     | positive errno | — | **1** |

FreeBSD libc stubs branch on the carry (`jb .cerror`), so setting CF is what
tells an error from a success. The kernel's `sysret` restores `RFLAGS` from the
trap frame, so writing the carry bit into `general.rflags` is enough.

`fork` gets FreeBSD's special treatment: the child returns `%rax=0`, `%rdx=1`
and CF cleared (`cpu_fork`).

### 4. Translations

FreeBSD libc passes FreeBSD-encoded constants; the reused `sys_*` methods
expect Linux ones. All of this is translated:

| Aspect | File | Note |
|---------|---------|------|
| Syscall numbers | `bsd/consts.rs` (`sys`) | `sys/sys/syscall.h` |
| `errno` | `bsd/errno.rs` | `sys/sys/errno.h`. **`EAGAIN`↔`EDEADLK` are swapped** between Linux and FreeBSD |
| `open`/`*at` flags | `bsd/translate.rs` | `sys/sys/fcntl.h` (e.g. `O_CREAT` 0x0200 vs 0x40, `O_CLOEXEC` 0x100000 vs 0x80000) |
| `mmap` flags | `bsd/translate.rs` | `sys/sys/mman.h` (`MAP_ANON` 0x1000 vs 0x20) |
| `struct stat` (224 B) | `bsd/fs.rs` | `sys/sys/stat.h` |
| `getdirentries` / `dirent` | `bsd/fs.rs` | `sys/sys/dirent.h` |
| `clockid_t` | `bsd/mod.rs` | `sys/sys/_clock_id.h` (`MONOTONIC` 4 vs 1) |
| `sysctl`/`sysctlbyname` | `bsd/sysctl.rs` | `sys/kern/kern_mib.c` |
| `sysarch` (FS base / TLS) | `bsd/mod.rs` | `sys/amd64/amd64/sys_machdep.c` |

### Supported FreeBSD-specific syscalls

- `__sysctl` / `__sysctlbyname` for the `kern.*` and `hw.*` leaves libc queries
  at start-up (`kern.ostype`, `kern.osreldate`, `hw.pagesize`, `hw.ncpu`,
  `kern.usrstack`, `kern.arandom`, …).
- `sysarch(AMD64_SET_FSBASE/GET_FSBASE)` — TLS base.
- `thr_self` — thread id.
- `issetugid`, `getpid` (with `%rdx`), `__getcwd`, `getdirentries`.

## How to test it

It cannot be run end to end without booting the kernel with a FreeBSD binary in
the rootfs. A minimal static program (freestanding, no libc) is enough to
validate the whole path: detection → stack → dispatch → CF convention.

```asm
# freebsd-hello.s — FreeBSD/amd64, static, no libc.
# Assemble on a FreeBSD host (or with a cross-as) and set the ABI note:
#   cc -static -nostdlib -o hello freebsd-hello.s
#   brandelf -t FreeBSD hello      # sets EI_OSABI=9 in case the linker did not
.text
.global _start
_start:
    # write(1, msg, len)
    movq  $4, %rax          # SYS_write
    movq  $1, %rdi          # fd = stdout
    leaq  msg(%rip), %rsi
    movq  $14, %rdx         # len
    syscall
    jc    fail              # CF=1 => error (FreeBSD convention)

    # exit(0)
    xorq  %rdi, %rdi
    movq  $1, %rax          # SYS_exit
    syscall
fail:
    movq  $1, %rdi
    movq  $1, %rax          # SYS_exit(1)
    syscall
.data
msg:
    .ascii "hola FreeBSD\n\0"
```

Copy the resulting executable into the rootfs and launch it with `INIT=` or
`SHELL=` on the cmdline (see the [main README](../README.en.md#boot-options)).
The kernel log should show:

```
elf: detected FreeBSD ABI for "/bin/hello"
```

The pure mappings (numbers, `errno`, flags, `struct stat` size, `sysctl`,
`clockid`) are covered by unit tests:

```bash
cargo test -p linux-syscall --lib bsd::
```

## Limitations

These pieces are **not** implemented yet and are the natural follow-up work to
run real FreeBSD binaries:

- **Signals.** `sigaction`/`sigprocmask` are accepted as no-ops (they return 0)
  but delivery uses the default disposition: FreeBSD's signal frame
  (`sigframe`) and `sigreturn`, whose layout and struct differ from Linux, are
  missing.
- **Threads.** The `thr_new`/`thr_create`/`thr_exit` ABI is not implemented
  (`ENOSYS`); only `thr_self` answers. `_umtx_op` is a no-op that is only
  correct for the single-threaded, uncontended case.
- **Dynamic linker.** Only **static** binaries are supported. A dynamic
  executable needs `/libexec/ld-elf.so.1`, which this tree does not include.
- **`ioctl`.** FreeBSD `ioctl` numbers (e.g. `TIOCGETA`) differ from Linux's
  (`TCGETS`) and are not translated yet; `isatty` and terminal control may fail.
- **`sysctl`.** Only a subset of `kern.*`/`hw.*` leaves is modelled. Unmodelled
  ones return `ENOENT`.
- The exact shape of the stack (`ps_strings`, `%rsp` alignment) follows
  `exec_copyout_strings`/`exec_setregs` but has not yet been validated against a
  real FreeBSD binary running.

## File map

| File | Contents |
|---------|-----------|
| `linux-syscall/src/bsd/consts.rs` | ABI constants transcribed from `freebsd-src` |
| `linux-syscall/src/bsd/errno.rs` | Linux → FreeBSD `errno` translation |
| `linux-syscall/src/bsd/translate.rs` | `open`/`*at`/`mmap` flag translation |
| `linux-syscall/src/bsd/fs.rs` | FreeBSD `struct stat` (224 B) and `dirent` |
| `linux-syscall/src/bsd/sysctl.rs` | `sysctl`/`sysctlbyname` |
| `linux-syscall/src/bsd/mod.rs` | Dispatch + CF/`%rdx` return convention |
| `linux-object/src/loader/mod.rs` | ABI detection in the ELF |
| `linux-object/src/loader/abi.rs` | Building the FreeBSD stack/auxv |
| `linux-object/src/process.rs` | Per-process `Abi` personality |
| `loader/src/linux.rs` | Personality-based dispatch in the trap handler |
