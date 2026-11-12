# fp-probe

[Español](README.md) · **English**

A probe of the floating-point and vector registers (x87, SSE, AVX) in
Eclipse OS: whether a thread finds them as it left them when it comes back
from the kernel. A static musl binary, x86_64 only, copied into the rootfs and
run from the shell.

## Why it exists

The kernel saves those registers when a thread comes in and, on the way out,
loads them back only if the CPU is holding something else (`FpOwner`, in
`vendor/trapframe`). Getting that wrong raises no error: every call returns
what it should and another thread's numbers are in the registers. A compiler
keeps anything in them (a `memcpy` in flight, a counter, a `double`), so the
symptom is corrupted data far from the cause.

## What it checks

Each check loads a pattern of its own into xmm0-15 (the whole ymm where there
is AVX), an x87 value and an MXCSR rounding mode, makes a system call from the
same block of assembly and compares what is there afterwards:

- one thread alone, call after call (the kernel loads nothing);
- four processes, then four threads, yielding and sleeping (the kernel must
  load each one's own, on whichever CPU);
- `fork`, where both sides come back with the parent's registers;
- a signal handler that dirties every register, every millisecond.

## Build

```sh
make            # x86_64-linux-musl-gcc
make linux      # with the host compiler, to run it on Linux
```

`cargo image` builds it and puts it at `/bin/fp-probe` in the x86_64 image.

## Run

```sh
fp-probe        # ymm where there is AVX
fp-probe noavx  # xmm only
```

With one CPU and with several (`-smp 1`, `-smp 2`, `-smp 4`), and with `noavx`
on the kernel command line for the FXSAVE path. It ends with
`fp-probe: all checks passed`, or with `fp-probe: FAIL` and exit status 1.
Every check holds on Linux: a FAIL there is a bug in the probe.
