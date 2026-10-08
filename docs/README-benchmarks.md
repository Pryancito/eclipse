# Reference benchmarks

Two different instruments, pointed at the same paths from opposite ends.

**`tools/eclipse-bench`** is a userspace suite: a static C binary that runs on
Eclipse and, unchanged, on a Linux control booted under the same QEMU. It
measures what a *process* pays — syscall entry, the scheduler, the emulator and
the kernel path, all in one number. That is the figure a user feels, and the
only one that can be compared against Linux. It cannot say which of those four
a slow number came from.

**`cargo bench`** targets in the kernel crates (`benches/`) measure the kernel
code alone: same machine, same run, no syscall and no guest. They cannot be
compared against Linux — there is no equivalent — but they say what the crate
itself costs, which is what a code change moves.

Neither replaces the other. A gap in the C suite with a flat Rust bench is the
syscall path or the scheduler; a gap in both is the subsystem.

## Running them

```sh
cargo bench -p linux-object  --bench vfs    --features mock-disk
cargo bench -p linux-object  --bench signal --features mock-disk
cargo bench -p zircon-object --bench vm     --features libos,aspace-separate
cargo bench -p zircon-object --bench futex  --features libos,aspace-separate
cargo bench -p linux-syscall                            # the syscalls themselves
cargo bench --manifest-path smoltcp/Cargo.toml          # wire parsing, upstream's
```

`-- <substring>` narrows any of them to the rows whose name matches, which is
what you want while working on one call: `cargo bench -p linux-syscall --
getdents`.

CI builds every bench target with `--no-run` and runs none of them: a figure
from a shared runner means nothing, and a bench target nothing compiles rots
silently. Run them locally, on an idle machine, before and after a change.

## Where a bench file lives

`benches/` is the usual place and is where the `linux-object` and
`zircon-object` targets are. `linux-syscall` is the exception: every submodule
of that crate is private (`mod file;`, `mod task;`, `mod vm;` ...), so a
`benches/` target there would not see a single syscall helper. Its benches are
`#[cfg(test)] mod benches` blocks beside the code they measure, which is the
form the native harness documents first, and they build with the lib:
`cargo bench -p linux-syscall`. `#![cfg_attr(test, feature(test))]` in its
`lib.rs` asks for the unstable attribute only in a test build, so a kernel
build never sees it.

Beside the code has a second advantage worth having on purpose: a bench that
sits under the function it measures is read by whoever changes that function.

## Which harness, and why

The native `#[bench]` harness (`test::Bencher`), which needs nightly. This tree
already pins `nightly-2026-09-01` in `rust-toolchain.toml`, and
`smoltcp/benches/bench.rs` was already using it, so no benchmarking dependency
enters the kernel's `Cargo.lock`.

`criterion` and `divan` are the usual choice on stable and give things this
does not: confidence intervals, stored baselines, and a regression verdict
between two runs. If those are wanted, the bench bodies port essentially
unchanged — what would change is the dependency tree of a kernel workspace,
which is the reason they were not reached for first.

## How to read a row

A single row is almost never the answer. The benches come in families whose
**slope** is the result:

- `vmar_find_mapping_of_1` / `_64` / `_512` — the lookup every page fault
  performs, always hitting the last mapping. Flat means the lookup is
  sub-linear; growing means every fault in a large address space pays for its
  size.
- `lookup_inode_cached_*` / `cold_*` / `miss_*` — absolute path lookups are
  served from a global cache (`dcache`) whose coherence is a single epoch that
  **every namespace mutation bumps**. So the cached rows are what a repeated
  `open` pays, and the cold ones are what a build or an installer pays, which
  is most of what they do. Benching only the hit would report that path depth
  is free.
- `fd_get_file_of_1` / `_256` — whether a process holding many descriptors pays
  for holding them.
- `vmar_map_unmap_one_page` / `_with_512_mappings` — whether maintaining the
  mapping list costs more as an address space fills up.

The C suite's `futex` section is the sharpest case of this and worth reading
before adding a family of your own. `FUTEX_WAKE issue` at 1, 16 and 64 parked
waiters times the syscall alone; `wake+observe` adds the woken thread reporting
back. Only the first answers "does the pick scan the queue", because the second
is dominated by that thread waiting for a CPU on any machine with fewer CPUs
than waiters. The first version of those rows measured the round trip only, read
50x at 64 waiters, and was wrong: the waiters were spinning on `EAGAIN` and
preempting the process doing the timing. Both mistakes are the same mistake —
a figure that includes something other than what its label names.

Two families reading the same quantity are worth more than either: the cold
lookup pair and the miss pair each give a per-component path cost, and when the
two agree the measurement is standing on something.

## What is measured where

| Path | Userspace (`eclipse-bench`) | In-kernel (`cargo bench`) |
| --- | --- | --- |
| `mmap`/`munmap`, page fault | `vm` section | `zircon-object` `vmar_map_unmap_*`, `vmar_find_mapping_*` |
| copies to and from userspace | — | `zircon-object` `vmar_{read,write}_memory_*` |
| `fork` of resident memory | `proc` section | `zircon-object` `vmo_fork_copy_*` |
| path resolution | `fs` section, `path cost per component` | `linux-object` `lookup_inode_*` |
| descriptor table | `fs` section, `dup`, `fcntl` | `linux-object` `fd_*` |
| procfs report formatting | `fs` section, `/proc/self/*` | `linux-object` `perf_*_report` |
| TCP/UDP wire parsing | `net` section round trips | `smoltcp` `benches/bench.rs` |
| the futex table every wait and wake looks a word up in | — | `zircon-object` `futex_table_*` |
| `FUTEX_WAKE` with nobody waiting | `futex` section | `zircon-object` `futex_wake_*` |
| the pending-set scan and the handler's mask | `sig` section | `linux-object` `sigset_*`, `signal_action_*` |
| `siginfo` and `sigaltstack` bookkeeping | `sig` section verdicts | `linux-object` `siginfo_*`, `signal_stack_*` |
| allocator and anonymous mappings | `heap` section | `zircon-object` `vmar_map_unmap_*` (the mapping half only) |
| `stat`/`fstat`/`statx` encoding | `fs` section | `linux-syscall` `file::stat::benches::*` |
| `select`/`poll`/`epoll` per-call and per-fd work | `fs` section | `linux-syscall` `file::poll::benches::*` |
| `getdents64` | `fs` section | `linux-syscall` `file::dir::benches::getdents64_*` |
| `read`/`write` through a pipe ring | `net`/`fs` round trips | `linux-syscall` `file::file::benches::pipe_*` |
| the flag and descriptor arguments of `open`, `dup`, `fcntl` | — | `linux-syscall` `file::fd::benches::*` |

The one empty cell is not an oversight. What the futex table costs, what a
wake with no waiters costs, and every signal decision are all benched above;
what cannot be is the moment of DELIVERY — a signal reaching a task, a waiter
being woken and run. Those need a task and a scheduler, and `libos` has
neither, so `Futex::wait` returns a future nothing can complete. The C suite's
`psched` section measures that half from userspace instead.

Two brackets worth reading, because they are the reason both instruments
exist:

- `FUTEX_WAKE` with nobody waiting is **~330 ns** from userspace and **11 ns**
  in-kernel. Ninety-seven per cent of what a `pthread_mutex_unlock` pays for
  that call is the syscall, not the futex code — so a change to the futex code
  cannot move it, and the syscall path is where to look.
- `kill(self)+handler` is **~2 us** from userspace, while every piece of signal
  bookkeeping it runs through is **1 to 12 ns**. The cost is the frame and the
  return through it, not the decisions.

And one slope: `futex_table_hit_of_1` / `_of_64` / `_of_512` is flat at ~20 ns,
so a process that has used many futex words does not pay for them on every
later call. Building a 512-entry table costs ~134 ns per insert against ~87 ns
for an insert that triggers no sweep, which is the sweep staying amortized the
way its threshold doubling intends.

## The floor of this harness, and the folding signature

Two cycles, about **0.6 ns**, is as low as a row goes here: that is `b.iter`'s
own loop. Several of the argument checks sit on it — `copy_file_range`'s flag
word, `fadvise`'s `advice`, `create_perm`'s mask — and those figures are real,
because their input went through `black_box` and the check is a compare and a
branch. They are the answer to "does validating this argument cost anything",
which is no.

What is *not* real is a row at the floor whose input is a **constant**. The
compiler then folds the call into its result and the row times an empty loop.
`linux-object/benches/signal.rs` read 0.6 ns on nearly every row the first time
it ran, for exactly that reason; `select_closed_fd_scan_of_1024` read the same
6 ns as the 64-fd row until both predicates read a black-boxed table instead of
computing an answer from `fd`. **So `black_box` goes around the inputs, not
only around the result**, and a suspiciously flat family is the thing to
re-check first.

## What the syscall rows said

- **`getdents64` is quadratic in the size of the directory.** 8 entries cost
  156 ns each; 256 entries cost 329 ns each. The record encoding is 3.7 ns, so
  it is not the writer: reading entry *n* walks the entries before it. Fitted,
  that is ~150 ns per name plus ~1.4 ns per preceding name — invisible on a
  home directory and 140 ms per listing on a directory of ten thousand. The
  scan is in the filesystem (`vendor/rcore-fs`), not in the syscall.
- **An `fd_set` costs ~53 ns at 64 fds and ~77 ns at 1024**, against 11 ns for
  a NULL one. `select` builds three per call, so the fixed cost of a `select`
  with one small set is around 130 ns before a single file is looked at — and
  it is nearly all the two allocations, not the bitmap copy, which is why the
  slope is so shallow.
- **The closed-descriptor sweep `select` runs before every wait is linear at
  0.6 ns per fd**: 40 ns at 64 fds, 612 ns at 1024. It runs once per pass, so a
  caller that passes a wide `nfds` pays two thirds of a microsecond per wake
  for the check alone — which is why `select_nfds` clamps to what the `fd_set`
  can hold instead of trusting the argument.
- **A pipe round trip is ~50 ns at 64 bytes and ~123 ns at 4 KiB**, so about
  40 ns of it is the call and the rest is the copy. That is the number a libc
  that buffers is buying its way out of.
- **Argument validation is free**, everywhere it was measured: 0.6 to 7 ns
  against the ~330 ns a syscall entry costs from userspace. The narrowing
  helpers in `intarg.rs` were added to fix wrong answers, and they cost
  nothing to have.

A caveat that applies to every in-kernel row: they run under `libos`, the only
configuration that builds for the host. Object bookkeeping (VMO and VMAR
structures, the mapping list, the path cache, the descriptor table) is the
kernel's own code and the figures are real. Anything that must reach a frame or
a page table goes through the host underneath, so `vmar_map_unmap_*` includes a
host `mmap` and is a ceiling on the bare-metal cost rather than a measurement
of it.
