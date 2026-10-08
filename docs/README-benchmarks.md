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
| `mincore` and `msync`, which walk a range page by page | `vm` section | `linux-syscall` `vm::benches::mincore_*`, `msync_*` |
| the `(addr, len)` and flag words of the memory calls | — | `linux-syscall` `vm::benches::*_arg_check`, `mmap_*` |
| `clone`/`clone3`/`wait4` argument decoding | `proc` section | `linux-syscall` `task::benches::*` |
| `prctl(PR_SET_NAME)` on an untrusted name | — | `linux-syscall` `task::benches::comm_from_*` |
| an interval timer catching up after a stall | — | `linux-syscall` `time::benches::forward_periodic_*` |
| `adjtimex`, `settimeofday`, `getitimer`, `alarm` | — | `linux-syscall` `time::benches::*` |
| which target a `kill` names, what `sigaction` refuses | `sig` section | `linux-syscall` `signal::benches::*` |
| descriptors and credentials passed over a Unix socket | ~1.5 ns per descriptor + ~25 ns | `linux-syscall` `net::benches::parse_scm_rights_*`, `build_recv_cmsgs_*` |
| `semop`'s atomic plan over a semaphore set | 0.8 ns per semaphore **in the set** | `linux-syscall` `ipc::benches::plan_semop_*` |
| `semctl(GETALL)`/`(SETALL)` bulk transfers | 276 / 381 ns over 512 | `linux-syscall` `ipc::benches::semctl_*` |
| the `futex` operation word every contended mutex sends | `futex` section | `linux-syscall` `misc::benches::futex_op_*` |
| `capget`/`capset`, `syslog`, `ioprio` | — | `linux-syscall` `misc::benches::*` |
| the syscall number every dispatch decodes | — | `linux-syscall` `benches::sys_try_from_*` |
| the zeroed kernel buffer behind every sized read | — | `linux-syscall` `benches::try_zeroed_buf_*` |
| the tail of an extensible struct (`clone3`, `sched_setattr`) | — | `linux-syscall` `benches::extensible_tail_*` |
| the `(int)` of a `pid_t`, `id_t` or `loff_t` argument | — | `linux-syscall` `intarg::benches::*` |
| `setgroups`'s list, up to `NGROUPS_MAX` | — | `linux-syscall` `intarg::benches::groups_list_*` |
| taking a descriptor back when its number never arrives | — | `linux-syscall` `outparams::benches::*` |
| "is this fd a pipe", twice per `splice` | — | `linux-syscall` `file::splice::benches::pipe_inode_*` |
| the twelve xattr syscalls, which answer from a table | — | `linux-syscall` `file::xattr::benches::*` |
| `pidfd_open`'s flags and `pidfd_send_signal`'s `siginfo` | — | `linux-syscall` `file::pidfd::benches::*` |
| a FreeBSD binary's flag words, both ways | — | `linux-syscall` `bsd::translate::benches::*` |
| the FreeBSD `errno` every failing syscall is mapped to | — | `linux-syscall` `bsd::errno::benches::*` |
| `sysctl`, which libc and jemalloc read before `main` | — | `linux-syscall` `bsd::sysctl::benches::*` |

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

**There is a second floor, at about 6.5 ns, and it is the more dangerous of
the two** -- because a row sitting on it looks like a measurement. Dozens of
unrelated one-compare helpers land between 6.0 and 7.0 ns: `Sys::try_from`,
`sched_pid`, `task_pid`, `readlink_bufsiz`, `waitid_id`, `groups_size`,
`mincore_arg_check`, `munmap_arg_check`, `user_range_resolve`,
`semget_nsems_arg`, `unix_socket_type_arg`, `alsa_ioctl_name`. The tell is
that the *accepted* and the *refused* row of the same helper land there
together, to within noise, although they take different branches and return
different values -- and a figure that does not move when the work does is not
the work.

`benches::the_second_floor_control` is the row that settles it: a function
that decides nothing, takes a register and returns `Ok(raw)`, marked
`#[inline(never)]` so it stays a call the way a cross-module helper does.

```
test benches::the_second_floor_control ... bench: 6.34 ns/iter (+/- 1.71)
```

So **6.5 ns means "too cheap to measure this way", not "6.5 nanoseconds of
decision"**, and the earlier claim here that argument validation costs "0.6 to
12 ns" was reading the harness for the upper half of that range. Compare any
row near 6.5 against this control before quoting it as a cost.

A flat family has a third cause worth knowing, because it looks identical to
folding: a loop that never ran. `msync_distinct_vmos_over_64_pages` and `_over_512_pages`
both read 11 ns once the mapping moved off address 0, and the reason was that
`distinct_vmos` takes an absolute `end` while the rows were still passing a
length -- so `end` sat below `start` and the `while` body was never entered.
`black_box` cannot catch that one: it is a correct call on wrong arguments. The
rows read 30 ns per page, flat, once the end was an end.

## What the syscall rows said

- **`getdents64` is quadratic in the size of the directory.** The rows rewind
  the handle inside the timed loop, so `getdents64_rewind_only` is the baseline
  to subtract: **49 ns**, and flat from an 8-entry directory to a 256-entry one,
  so one baseline covers the family. Net of it, 8 entries cost 152 ns each and
  256 entries cost 348 ns each. The record encoding is 4.1 ns, so it is not
  the writer: reading entry *n* walks the entries before it. Fitted, that is
  **~146 ns per name plus ~1.6 ns per preceding name** — invisible on a home
  directory and about **80 ms per listing** on a directory of ten thousand,
  where the quadratic term is 98% of the bill. The scan is in the filesystem
  (`vendor/rcore-fs`), not in the syscall.
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
- **A pipe round trip is ~50 ns at 64 bytes and ~112 ns at 4 KiB**, so about
  45 ns of it is the call and the rest is the copy. That is the number a libc
  that buffers is buying its way out of. The ring is `PIPE_DEFAULT_CAPACITY`,
  64 KiB -- `PIPE_BUF` (4096) is only the length POSIX promises to write
  atomically -- so the 64 B, 4 KiB and 64 KiB rows are each one write/read
  pair: 3.2 us to fill the ring once, 20 GB/s through it, and 13.0 us for
  256 KiB, which is four ring loads at the same rate. The copy scales; the
  call does not shrink.
- **Argument validation is free**, everywhere it was measured: every row is at
  one of the harness's two floors, against the ~330 ns a syscall entry costs
  from userspace. The narrowing helpers in `intarg.rs` were added to fix wrong
  answers, and they cost nothing to have.

## What the memory, process, clock, signal, socket and IPC rows said

- **`msync` walks every page to discover one object.** `distinct_vmos` is
  30 ns per page whether the range is 64 pages or 512, and the whole range is
  one VMO in both. A `msync(MS_SYNC)` of a 100 MiB mapped file therefore spends
  about **0.8 ms** in `find_mapping` to learn what the first page already said.
  SQLite and Firefox both call it on their own schedules; the per-mapping walk
  that `walk_mapped` does is the shape this one wants.
- **`mincore` is 50 ns per page**, flat from 64 pages to 512 (66 ns for a
  single one, which is the call). An allocator probing a gigabyte of heap
  layout pays ~13 ms. The syscall already hands the range over in page-sized
  chunks, so this is per chunk, not per call — but it is the figure that makes
  `mincore` a poor way to ask about a large region.
- **An interval timer's catch-up is O(1), and now there is proof.** 7.9 ns
  when nothing was missed, 12.7 ns a thousand periods late, 12.1 ns a million
  periods late. A stalled machine re-arming a 10 ms timer does not pay for the
  stall, which is the property that stops a stall compounding itself.
- **A process name from userspace costs 27 ns when it is ASCII and 48 ns when
  it is not.** `prctl(PR_SET_NAME)` takes fifteen arbitrary bytes, and the
  invalid-UTF-8 path goes chunk by chunk replacing each bad byte — worth
  knowing only because the input is untrusted and the two paths differ.
- **`semop` pays for the semaphore set's width on every call, not for the
  operations it asked for.** One operation on a 16-semaphore set plans in
  38 ns; the same one operation on a 512-semaphore set takes 437 ns. That is
  **0.8 ns per semaphore in the set**, paid whether the call touches one of
  them or all of them, because `semop(2)` is atomic and the whole set's values
  are copied before a single semaphore moves. A program using a wide semaphore
  array as a barrier is therefore paying for the array's width on every
  `semop`, and the fix is to plan against the semaphores the operations name
  rather than snapshot the set. For scale, the work it did ask for is cheap by
  comparison: 16 operations on 16 semaphores is 154 ns, and 500 operations on
  512 is 1.6 us. A plan that would block bails in 15 ns, so a contended
  semaphore does not pay the copy twice.
- **A refused `SETALL` costs almost as much as one that is accepted**: 270 ns
  against 381 ns over 512 semaphores. The values are all read and validated
  before any is rejected, which is correct -- `semctl(2)` is all-or-nothing --
  and `GETALL` of the same set is 276 ns, so the read dominates either way.
- **Passing descriptors over a Unix socket costs ~1.5 ns each**, on top of
  about 25 ns for the call: 27 ns for one descriptor, 81 ns for 16, 426 ns for
  253 (`SCM_MAX_FD`, the cap that bounds the walk at all -- without it the
  length came from the sender). Building the receive side is the same order:
  36 ns for credentials alone, 118 ns for credentials plus 16 descriptors.
- **But a buffer over the cap is walked in full before it is refused**:
  254 descriptors cost 408 ns to reject, against 426 ns to accept 253. The cap
  stops the kernel *using* an unbounded descriptor list, not *reading* one, so
  the length a sender picks still buys it work. A malformed header, by
  contrast, is thrown out in 10 ns.
- **`clone3` decodes a `struct clone_args` in 5 ns**, nine refusals and all.
  `fork(3)` does not spend its time at the door.
- **Everything else is at one of the two floors**, which is the same answer the
  file batch gave once the floors are accounted for: the syscalls' argument
  work is free, and what a syscall costs is the entry, the copies and the
  subsystem. Read "at the floor" and not "6.5 ns"; see the control row above.

## What the dispatch layer said

- **Decoding the syscall number is free, and that matters more than it
  sounds.** `Sys::try_from` reads at the second floor for the first number in
  the table, the middle, the last Linux one (439), Eclipse's own at 601 past a
  gap of a hundred and sixty, and a number that is not in the table at all --
  all within noise of each other, so the generated `TryFrom` is a table and
  not a chain of compares. That is the answer to a thing worth checking:
  dispatch decodes the number once, and the `[einval-hunt]` logging decodes it
  up to **four more times** on an `EINVAL`. At the floor, four decodes cost
  nothing; against a 353-arm chain they would have.
- **`try_zeroed_buf` is 20 ns per KiB, and that is a buffer about to be
  overwritten.** 35 ns for a page, 1.30 us for `SYSCALL_IO_MAX`. Every sized
  read in this crate goes through it (and must, because `vec![0u8; n]` took
  the machine down on a 24 KiB `read`), but the zeroing is not what makes it
  safe -- `try_reserve_exact` is. A 64 KiB `read` therefore spends 1.3 us
  writing zeroes into memory the read then fills, which is about the cost of
  the pipe round trip itself. Linux does not zero a read buffer. The refusal
  is 1.3 ns, so a process hammering an impossible length costs nothing.
- **An extensible struct's tail is checked a byte at a time**: 0.34 ns per
  byte, so 1.40 us over a 4 KiB tail. In practice the tails are tiny -- 8.7 ns
  for the 24 bytes a `clone3` from a newer libc carries -- and a set byte is
  found and refused in 1.3 ns, so it does stop early. Worth knowing only
  because the length is the caller's to choose.
- **`setgroups`'s cost is the copy, not the validation.** Scanning 65536 gids
  for `(gid_t)-1` takes 3.9 us net of the vector it is handed, which is
  0.06 ns per gid -- vectorised -- against 6.4 us to clone the list. A
  `(gid_t)-1` at the front is found at once.
- **Keeping the caller intact costs nothing.** `commit_and_report_old` and
  `hand_out_pair` sit at the first floor, 0.6 to 1.1 ns, take-back path
  included. The objection to a helper for "a call that fails must leave the
  caller exactly as it found them" was always that it costs something; it does
  not, and writing it by hand at each call site is what left `pipe2((int *)1,
  0)` leaking two descriptors a turn.

## What the FreeBSD personality said

- **Asking "is this fd a pipe" costs more than a small `splice` moves.**
  `pipe_inode` is **32 ns**, hit or miss alike, and `splice_bytes` calls it
  once per side plus a third `downcast_ref` at each use site -- so about 95 ns
  of `TypeId` comparison through a vtable before a byte moves, against ~50 ns
  for a whole 64-byte pipe round trip. For a `splice` of a page it is noise;
  for the small ones it is most of the call. The shape that would fix it is
  asking the `FileLike` what it is rather than trying to cast it to each
  thing in turn.
- **A `sysctl` is mostly allocation.** `sysctlbyname("hw.ncpu")` resolves the
  name in 19.8 ns against 5.6 ns for a name that is not modelled, and the
  difference is the `to_vec` that turns a static two-element MIB into a heap
  allocation. Then `to_bytes` allocates again to produce the four bytes that
  get copied out: 17.8 ns. A string leaf allocates twice over, 15.9 ns to
  clone the hostname out of the context and 24.9 ns to serialise it. So an
  8-byte answer costs about 38 ns of allocator, and libc's
  `sysconf(_SC_NPROCESSORS_ONLN)` and jemalloc's initialisation both read
  these before `main` runs.
- **The flag translation is not the loop it looks like.** `sift` walks its
  whole map on every call, thirteen entries for `open`, rebuilding a value
  that never changes -- and it costs **0.17 ns per entry** (1.48 ns over a
  two-entry map, 3.35 ns over thirteen). The maps are `const`, so the loop is
  unrolled at compile time and the invariant folded away; there is nothing for
  a hand-written constant to save. A refusal is cheaper still, because it
  short-circuits.
- **`errno` translation is a jump table**: 0.70 ns for the first arm, for one
  past the identical run, and for the three with no FreeBSD peer alike. So
  does `dirent_type`, and `dirsiz` is 0.7 ns -- which matters because a
  FreeBSD `readdir` pays both once per name, on top of the filesystem walk
  that is quadratic in the directory's size.
- **The twelve xattr syscalls are free.** They answer from a table in this
  kernel, so what they cost IS `xattr_precheck` plus `xattr_answer`: 1.1 to
  1.8 ns for the checks, the answer at the second floor. Reading the name and
  the flags before looking the file up -- which is what `fs/xattr.c` does and
  what the dispatcher used to skip -- costs nothing.

A caveat that applies to every in-kernel row: they run under `libos`, the only
configuration that builds for the host. Object bookkeeping (VMO and VMAR
structures, the mapping list, the path cache, the descriptor table) is the
kernel's own code and the figures are real. Anything that must reach a frame or
a page table goes through the host underneath, so `vmar_map_unmap_*` includes a
host `mmap` and is a ceiling on the bare-metal cost rather than a measurement
of it.
