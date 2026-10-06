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
cargo bench -p linux-object  --bench vfs --features mock-disk
cargo bench -p zircon-object --bench vm  --features libos,aspace-separate
cargo bench --manifest-path smoltcp/Cargo.toml          # wire parsing, upstream's
```

CI builds every bench target with `--no-run` and runs none of them: a figure
from a shared runner means nothing, and a bench target nothing compiles rots
silently. Run them locally, on an idle machine, before and after a change.

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
| allocator and anonymous mappings | `heap` section | `zircon-object` `vmar_map_unmap_*` (the mapping half only) |
| signal delivery and faults | `sig` section | — |
| futex wake, contention, condvars | `futex` section | — |

The two empty cells are not an oversight: signal delivery and the futex code
both need a task to deliver to, and `libos` has no scheduler of its own to
provide one. Their in-kernel halves have to be measured on the machine, which
the `psched` section of the C suite does from userspace.

A caveat that applies to every in-kernel row: they run under `libos`, the only
configuration that builds for the host. Object bookkeeping (VMO and VMAR
structures, the mapping list, the path cache, the descriptor table) is the
kernel's own code and the figures are real. Anything that must reach a frame or
a page table goes through the host underneath, so `vmar_map_unmap_*` includes a
host `mmap` and is a ceiling on the bare-metal cost rather than a measurement
of it.
