# Kernel multithreading tests (clone/futex)

[Español](README.md) · **English**

*Freestanding* tests (no libc) that reproduce how musl's `pthread_create`
interacts with the kernel and validate the primitives every multithreaded
program (sysbench, etc.) relies on:

- **thr.c** — thread creation: `clone` with musl 1.2's exact flags
  (`0x7d0f00`), TLS through `CLONE_SETTLS`, `CLONE_PARENT_SETTID`, `ctid`
  semantics (the kernel must **not** write the TID at creation without
  `CLONE_CHILD_SETTID`, and must clear and wake it on exit through
  `CLONE_CHILD_CLEARTID` — `pthread_join` depends on this).
- **thr2.c** — a `FUTEX_WAIT`/`FUTEX_WAKE` stress test between two threads
  (20k ping-pong iterations). It catches *lost wakeups*: if the kernel checks
  the futex value outside the wait-queue lock, a WAKE can slip in between the
  check and the enqueue and the thread sleeps forever (that is how sysbench
  hung at "Initializing worker threads...").

Both make the syscalls by hand: with the `syscall` instruction on bare metal
and through the `rcore_syscall_entry` pointer (patched by the loader) on
libos.

See the `Makefile` to build and run them.

## Checking with real sysbench

`build-sysbench.sh` builds sysbench 1.0.20 (dynamic, musl, with bundled
LuaJIT) — the same program that hung at "Initializing worker threads...". With
the fixed kernel it reaches "Threads started!" and completes the CPU
benchmark. Checked in QEMU (`-smp 4`) with 1 and with 4 threads:

    sysbench cpu --time=10 run            -> ~240 ev/s, finishes
    sysbench cpu --time=10 --threads=4 run -> ~960 ev/s (scales ~4x), finishes

If sysbench still hangs on your system but these tests pass, you are almost
certainly booting a kernel from before the fixes (reinstall `zcore.elf` on the
ESP / regenerate the ISO before testing).
