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
`zircon-object` targets are. `linux-syscall`, `kernel-hal` and `zcore-drivers`
are the exceptions, for the same reason in all three: the modules worth
measuring are private. Every submodule of `linux-syscall` is (`mod file;`,
`mod task;`, `mod vm;` ...); `kernel-hal` declares `mod common;` with a
handful of re-exports in `lib.rs`, so the user-copy and timer machinery is
unreachable from outside; and in `zcore-drivers` the fence helpers, the scrollback
console, the receive-path arithmetic, the GPFIFO ring accounting and the
present path are each private to their own module. A `benches/` target in any of them would not see a single
helper. Their benches are `#[cfg(test)] mod benches` blocks beside the code
they measure, which is the form the native harness documents first, and they
build with the lib: `cargo bench -p linux-syscall`, `-p kernel-hal`,
`-p zcore-drivers --features graphic,virtio,xhci-usb-hid`.
`#![cfg_attr(test, feature(test))]` in each `lib.rs` asks for the unstable
attribute only in a test build, so a kernel build never sees it.

Two of them nest one level further. The rows for the generic 2D primitives
live in `#[cfg(test)] mod benches` *inside* `scheme::display`'s `blit_tests`,
because what they need is that module's fake framebuffer backend, and a child
module sees its ancestors' private items while a sibling would see none of
them. The alternative was marking a fixture `pub(super)` for every row that
wanted it.

Beside the code has a second advantage worth having on purpose: a bench that
sits under the function it measures is read by whoever changes that function.

One family needs `--test-threads=1`: the display rows that choose between
`blit_from`'s two paths hold a process-wide flag and a spin lock while they
measure, so run in parallel the whole family reads wrong. CI pins one thread
already; a hand run should too.

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
| the GPFIFO ring wrap every `EXEC` pays per push | — | `zcore-drivers` `display::nouveau_uapi::benches::advance_*`, `ring_state_*` |
| GP entry and host-semaphore encoding | — | `zcore-drivers` `display::nouveau_uapi::benches::encode_*`, `build_a_semaphore_*` |
| the per-ioctl nouveau decode and the crash trail's names | — | `zcore-drivers` `display::nouveau_uapi::benches::decode_*`, `recognise_*`, `name_*` |
| a page flip and its swapchain | — | `zcore-drivers` `display::nvidia::present_benches::flip_*` |
| GPU identification and the BOOT0 decode | — | `zcore-drivers` `display::nvidia::present_benches::identify_*`, `decode_*_boot0` |
| what a monitor says about itself | — | `zcore-drivers` `display::edid::benches::*` |
| the GEM handle every driver-private ioctl looks up | — | `zcore-drivers` `scheme::gem_mmap::benches::*` |
| the 2D primitives every framebuffer backend inherits | — | `zcore-drivers` `scheme::display::blit_tests::benches::*` |
| the capability bitmap behind `EVIOCGBIT` | — | `zcore-drivers` `scheme::input::benches::*` |
| the console's shadow buffer and its dirty region | — | `zcore-drivers` `utils::shadow_fb::tests::benches::*` |
| `stat`/`fstat`/`statx` encoding | `fs` section | `linux-syscall` `file::stat::benches::*` |
| `select`/`poll`/`epoll` per-call and per-fd work | `fs` section | `linux-syscall` `file::poll::benches::*` |
| `getdents64` | `fs` section | `linux-syscall` `file::dir::benches::getdents64_*` |
| `read`/`write` through a pipe ring | `net`/`fs` round trips | `linux-syscall` `file::file::benches::pipe_*` |
| the permission check every copy to or from userspace pays | — | `kernel-hal` `common::user::benches::check_len_*` |
| a one-value and a buffer copy across the user boundary | — | `kernel-hal` `common::user::benches::{read,write}_{one_u64,array_of_*}` |
| taking a path string from userspace | `fs` section | `kernel-hal` `common::user::benches::as_c_str_*` |
| `readv`/`writev`/`sendmsg` gather lists | — | `kernel-hal` `common::user::benches::{read_iovecs,read_to_vec,read_bytes_at,drain}_*` |
| arming and refreshing a syscall timeout | — | `kernel-hal` `common::timer_waker::benches::*` |
| looking at a GPU fence's landing zone | — | `zcore-drivers` `scheme::syncobj::benches::landed_*` |
| a character, a newline and a repaint on the kernel console | — | `zcore-drivers` `utils::graphic_console::benches::*` |
| the receive path's per-frame checksum and throttle work | `net` section throughput | `zcore-drivers` `net::e1000e::benches::*` |
| the CPU-side fence wait's backoff policy | — | `zcore-drivers` `scheme::syncobj::benches::{wait_spin_backoff,fence_poll_step}_*` |
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

What the control establishes is bounded, and the bound matters when applying
it. It is the floor **for a row of that shape**: a helper the compiler keeps
out of line, taking a register and returning an `LxResult` in the two-register
layout. It is not a tax every row pays. A helper the compiler inlines has no
such floor and sits near 0.6 ns, a helper returning something of a different
size pays a different call cost, and two rows of a *branchless* helper landing
together is what branchless code does rather than evidence of a floor. The
dozen helpers listed above do match the control's shape, which is why the
reading holds for them; a row whose shape does not match wants its own empty
equivalent measured beside it before anything is read into the number.

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
- **Keeping the caller intact costs about a nanosecond.** With each closure's
  whole `Result` black-boxed -- so the helper cannot be told which way its `?`
  goes before it runs -- `hand_out_one` is 0.75 ns when the number reaches the
  caller and 0.91 ns when the descriptor comes back out of the table;
  `hand_out_pair` is 0.99 / 1.15 ns, and 1.34 ns when the second descriptor is
  the one that fails; `commit_and_report_old` is 0.92 ns with a NULL
  out-pointer and 1.26 ns with a real copy out, which is the copy showing up,
  and 0.91 ns when the change itself fails and the copy never happens. The
  ordering is the control flow: every row differs from its sibling in the
  direction the extra work goes, which is how you know the rows ran.
  Black-boxing only the value inside the `Ok` left the discriminant visible
  and the branch foldable; these are the figures after fixing that. The
  objection to a helper for "a call that fails must leave the caller exactly
  as it found them" was always that it costs something: it costs a
  nanosecond, against the ~330 ns of the syscall entry it sits inside, and
  writing it by hand at each call site is what left `pipe2((int *)1, 0)`
  leaking two descriptors a turn.

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

## What the HAL's copies said

Three batches of syscall rows ended on the same sentence: what a syscall costs
is "the entry, the copies and the subsystem". The subsystem had rows and the
argument work turned out to be free, so that sentence was a budget with its
largest line item unmeasured. These are the copies.

- **A small copy is the permission check, not the memory.** `check_len` --
  null, alignment, the user-half bound, and an indirect call into the address
  space to ask whether anything is mapped there -- is **9.1 ns and flat**: the
  same for one byte as for 4 KiB. Reading one `u64` across the boundary is
  **9.2 ns**, which is that check and nothing else. So for a `timespec`, a
  `sockaddr` or an `int`, the eight bytes are free and the permission question
  is the whole price.
- **`as_slice` costs exactly the check, and `read_array` costs the copy on top
  of it.** `as_slice` is 8.9 ns at 64 bytes and 9.2 ns at 64 KiB -- flat,
  because it checks the range and forms a slice over the caller's own memory.
  `read_array` allocates and copies: 16.0 ns at 64 bytes, 56.5 ns at 4 KiB,
  **1.58 us at 64 KiB**. A syscall that takes a copy it does not need pays
  **1.6 us per 64 KiB** for the privilege. (That is the same shape as
  `try_zeroed_buf`'s 20 ns/KiB of zeroing a buffer about to be overwritten,
  and compounds with it.)
- **The allocation is ~7 ns of a small read.** `write_array` does no
  allocating and is 10.6 ns at 64 bytes against `read_array`'s 16.0; at 4 KiB
  and above they converge (41.8 vs 56.5 ns, 1.56 vs 1.58 us) as memory
  bandwidth takes over. Net of the check, a 64 KiB copy runs at about
  0.024 ns/byte.
- **Taking a path is mostly hunting for its NUL.** `as_c_str` is 20.9 ns for a
  24-byte name, 87.9 ns for 256 bytes and 905 ns for 4 KiB -- about
  **0.22 ns per byte of name**. `as_str` with the length already known is
  15.4 ns at 256 bytes, so of `as_c_str`'s 87.9 the scan is **72 ns and the
  UTF-8 validation is nearly free**. The scan is a byte-at-a-time
  `find(|i| *ptr.add(i) == 0)`, and it runs over bytes the `check()` above it
  validated one of. `execve`'s `argv` pays it per entry, plus a
  `String` allocation each: 8 entries of 24 bytes is **303 ns** and 256
  entries is **8.69 us**, about 34 ns per argument.
- **`read_to_vec`'s cost is one permission check per iovec.** Gathering 1024
  iovecs of 64 bytes takes **10.8 us** for 64 KiB of payload -- 0.164 ns/byte,
  seven times the 0.024 ns/byte a single `read_array` of the same 64 KiB
  manages. 1024 checks at 9.1 ns is 9.3 us of it, so the per-entry check is
  essentially the whole figure.
- **And the bounded alternative is quadratic.** `read_bytes_at` was added so
  `writev` could drain an arbitrarily large gather list through a bounded
  kernel buffer instead of `read_to_vec`'s single caller-sized allocation. It
  bounds the allocation, which was the point. But it restarts its walk at
  `vec[0]` on every call and skips forward, so reaching entry *n* walks the
  *n* before it: one call for the front of a 1024-entry list is 11.9 ns, one
  for the back is **354.6 ns**, about 0.34 ns per skipped entry. Draining the
  whole list one iovec at a time, measured rather than fitted, is **183 ns for
  16 entries and 183 us for 1024** -- 64 times the entries, 997 times the
  time. Against `read_to_vec`'s 10.8 us for the same list, the bounded path is
  **17x slower at `IOV_MAX`**. A cursor carried across calls would make it
  linear; the function's signature already takes the offset the caller
  tracks, so the information is there.
- **The user-half bound is free.** 1.06 ns to accept, 1.01 ns to refuse a
  kernel-half pointer, measured through `in_user_half_with` with the `libos`
  exemption passed in as `false` -- because under `libos`, the only
  configuration `kernel-hal` builds in on the host, `in_user_half` folds to a
  constant `true` and no row can see the bound. `user_range_ok`, which every
  DRM/KMS ioctl passes through, is 0.81 ns.

## What the syscall timeout said

`kernel-hal`'s `timer_waker` is what every timed wait in the kernel goes
through -- `poll`, `epoll_wait`, `select`, `nanosleep`, a futex or `semop` with
a timeout. It is called from `poll`, not once per wait, so the question is not
what arming a timer costs but what the ninth poll costs when the timer is
already armed for the right instant.

- **Refreshing an armed timeout is the waker clone and nothing else.**
  42.44 ns, against 42.70 ns for cloning the waker on its own. The slot
  bookkeeping -- two atomic reads, a store under a lock, the race the second
  `is_done` closes -- is inside the noise. Reading the slot's deadline is
  0.81 ns and `is_done` is 0.64 ns.
- **Re-arming costs 18 times as much: 753 ns.** Arming an empty slot is
  361 ns (an `Arc`, a `Box` for the callback, a waker clone) and cancelling an
  armed one is 400 ns, so a re-arm pays both. Cancelling a slot that was never
  armed -- a `poll` with no timeout -- is 1.86 ns, so the machinery costs
  nothing when it is not used.
- **`poll` takes the expensive path every time, and its own comment says it
  does not.** `schedule_poll_wakeup` in `linux-syscall/src/file/poll.rs`
  computes `let deadline = mono_now() + after` on each poll, and
  `ensure_timer_waker` keeps a timer only when `existing.inner.deadline ==
  deadline`. `timer_now()` has nanosecond resolution, so that equality is
  false on every poll and the refresh path is unreachable from there: each
  round of a `poll`, `select` or `epoll_wait` that is waiting allocates an
  `Arc` and a `Box`, cancels the previous timer and arms a new one. The
  comment above the call reads "Refresh in place while the previous tick is
  still pending; re-arm only after it fired. Avoids AtomicBool TOCTOU +
  timer-heap churn."
  The shape that does refresh is three files away and in the tree already:
  `linux-object/src/net/wait.rs` stores an absolute `self.deadline` once and
  passes the same value every poll, which lands on the 42 ns path. Holding
  the deadline instead of recomputing it is the difference between 42 ns and
  753 ns per poll of every timed wait in the system.

## What the GPU fence path said

A fence's landing zone is pinned sysmem the GPU writes, mapped uncached, so
reading it is a trip off the CPU's caches rather than a load. Under `mock`,
where these rows run, it is an ordinary L1 load -- so every figure below
**under-weights the reads** relative to the arithmetic around them, and the
savings from reading fewer of them are lower bounds.

- **Reading each zone once instead of once per fence is worth 5.4x, at
  least.** 256 fences in one zone -- a buffer two submits of one ring wrote,
  which is the shape the comment on `hw_fences_landed` describes -- cost
  1.48 us when the code read the word per fence, and **274 ns** reading it
  once: about 1.0 ns per fence of pure compare after the first read. On
  hardware each saved read is an uncached round trip, so the real ratio is
  wider than 5.4.
- **The `Vec` the fix keeps is scanned linearly, so the call is quadratic in
  the number of DISTINCT zones**, and the code says so in as many words:
  "these lists are a handful of entries long (one per ring that wrote the
  buffer), and a `Vec` of pairs beats a map at that size". Measured, the
  handful holds and the cliff past it is steep: 2 distinct zones 21.8 ns,
  8 zones 68.5 ns, **256 zones 14.2 us** -- 52 times the one-zone case of the
  same width. Crossing back over the per-fence reading it replaced happens at
  roughly **28 distinct zones on host memory**, and higher on hardware, where
  each avoided read is worth more. So the comment is right about today's
  lists, and 256 is not an idle top end to have checked: it is the group size
  NVK submits in.
- **A wait that is going to park finds out for almost nothing.** The first
  fence still in flight ends the call: 21.7 ns over a 256-fence list against
  274 ns when all of them have landed. One fence, the overwhelmingly common
  look, is 8.1 ns, and the wrapping compare that makes a `u32` counter past
  its top behave is 0.86 ns.
- **Choosing a backoff is free**: 0.59 ns on the eager probes, 0.96 ns once
  doubling, 0.93 ns at the cap, and `fence_poll_step` is at the harness's
  second floor. Worth stating because the reason the CPU-side `wait` used to
  starve the compositor was that it re-took the table on every spin turn; if
  deciding how long to pause had cost anything, that fix would have traded one
  problem for another.

## What the kernel console said

Two things make this worth a number rather than a shrug. A panic report that
must get out goes over the serial line and *not* through here, and that split
should be a decision rather than a habit. And the cursor blink reaches
`present` from a hard IRQ with no current thread -- the path that needed a
fat-pointer guard before it would stop faulting -- where anything expensive is
expensive with interrupts off.

The rows run against `FakeDisplay`, whose aperture is a heap buffer. A real
framebuffer is uncached write-combining across a PCI aperture, so **every
pixel figure below is a lower bound**; the cell bookkeeping is faithful, the
stores are not.

- **One character costs 855 ns**, which is the cache store plus an 8x16 glyph
  blit: about 6.7 ns per pixel. So an 80-column line of kernel log is
  **~68 us of glyph blitting** before anything scrolls. Two independent
  families agree on this: `redraw_80x25` is 1.65 ms for 2000 cells, which is
  825 ns a cell.
- **A newline is a pixel band, and it grows with the console.** `new_line` is
  **34 us at 80x25, 104 us at 80x50 and 494 us at 240x67** -- the geometry the
  machine actually boots into. Together with the line above, **one line of
  kernel output on a 1080p console costs over half a millisecond**, which is
  the measurement behind keeping the graphical console off the path a panic
  report takes.
- **A region scroll is flat in the number of lines.** Scrolling the whole
  screen up by one line is 38.3 us and by twelve is 34.7 us: one band move
  either way, so an editor that scrolls a page pays once, not per line.
  Downward is the same (36.9 us), and a four-row region is 6.9 us, so the cost
  follows the band and not the screen. Clearing the screen is 29.5 us.
- **A repaint is the expensive one, as designed: 1.65 ms at 80x25 and
  14.7 ms at 240x67.** That is what a VT switch and a scrollback scroll each
  run (`scroll_history_by_one_line` is 1.63 ms, which is the redraw inside
  it), and at 1080p it is a dropped frame and then some.
- **The blink is cheap and its guard is free.** `present` is 212 ns at 80x25
  and 220 ns at 240x67 with the cursor drawn, against **19.7 ns** without --
  so the blink's cost is the cursor itself and the rest of `present` is
  nearly nothing at either size. And `display_dispatchable`, the fat-pointer
  liveness check the blink path grew so it would stop calling through a
  smashed vtable, is **1.28 ns**: asking on every blink costs nothing, which
  is the answer that makes that guard uncontroversial.

## What the receive path said

Per-frame work on the way in, which is per-packet cost at line rate. All of it
is pure arithmetic over a byte slice, so unlike the console and the fence rows
there is no mock in the way: this is exactly what runs on the machine.

- **Every per-frame decision except the checksum is free.** The gate that asks
  whether the NIC already validated a frame is **0.76 ns**, the
  interrupt-throttle choice taken on every receive IRQ is ~1.0 ns, the
  multicast hash is 0.63 ns and matching a PCI device id against the
  supported chipsets is 2.1 ns.
- **The IPv4 header check is bounded, and the rows prove it**: 8.33 ns on a
  64-byte frame and 8.42 ns on a 1500-byte one. Twenty bytes either way, which
  is what it should be and what a bug here would break. A frame that is not
  IPv4 at all leaves in 1.0 ns.
- **Checksumming the payload in software runs at 0.16 ns/byte**: 11.5 ns over
  64 bytes, 235 ns over 1460, 1.43 us over a jumbo 8960. That is a
  byte-at-a-time one's-complement loop, about 6.3 GB/s.
- **So the NIC's L4 offload is worth 243 ns a frame.** The whole software
  fallback on a 1500-byte TCP frame the NIC did not check is **252.6 ns**,
  against **9.66 ns** for the same frame when the NIC validated the L4
  checksum and the walk is skipped. At gigabit line rate with full-MTU frames
  (~82,500 frames/s) that difference is **about 20 ms of CPU per second, or
  2% of a core** -- per gigabit, and the same 2% with jumbo frames, since the
  cost is per byte. It is the one thing on this path that is O(bytes) rather
  than O(1), and the 0.76 ns gate in front of it is what keeps it off the
  common frame.

## What the GPU submit path said

The `EXEC` uAPI is the path NVK drives every frame, and `fast_submit` writes
one GPFIFO entry per push with NVK batching hundreds of pushes into a single
submission. All of this is integer arithmetic over values read out of the
channel's USERD window, so there is no mock in the way and these are the real
figures.

- **The ring wrap was the most expensive thing on the submit path, and it was
  a division.** `fast_submit` advanced its write cursor with `slot = (slot +
  1) % entries` after each entry. `entries` comes from outside this kernel —
  `eclipse_rm_exec_fast_prepare` fills it in and it may not be a power of two
  — so the compiler could not turn it into a mask and emitted a hardware
  divide per entry: **556 ns for a batch of 256 against 86 ns for the same
  advance written as a compare**, 2.17 ns a slot against 0.34. For scale,
  *encoding* those 256 GP entries costs **242 ns**, so the wrap was more than
  twice the work it was wrapping. `nouveau_uapi::next_slot` is now that
  compare: the cursor starts at the wrapped `put` and never leaves the ring,
  so `slot + 1` can only reach `entries`, and
  `the_slot_cursor_wraps_exactly_as_the_division_did` walks every slot of
  several ring sizes against the division it replaced.
- **`ring_state` took three divisions and only two were forced.** 5.60 ns per
  call, against **2.96 ns** with the last wrap written as a conditional
  subtract. `put_raw % entries` and `get_raw % entries` have to stay — both
  come from the GPU and from the RM's own self-test submissions on channel 0
  and may be anything — but `(put + entries - get) % entries` runs on values
  already inside the ring, so the dividend is in `[1, 2*entries)` and the
  wrap can only subtract once. It is now that subtract, with
  `the_in_flight_count_is_what_the_division_answered` walking every pointer
  pair of several ring sizes, the overflow case included.
  `fast_submit` pays this once per `EXEC` *and again on every poll of the
  ring-full wait*, so the saving is per poll, not per submission.
- Together that is **about 470 ns of pure division removed from every `EXEC`
  that carries a 256-push batch**, and the shape of the fix is the point: the
  figure came out of one bench pair, not from reading the code.
- **Everything else on the path is already free.** A GP entry pair encodes in
  **0.91 ns**, the push variant that also reads the no-prefetch flag in 1.06,
  a push header in 1.14, and either six-dword host semaphore stream — the
  RELEASE that every fence rides on and the ACQUIRE that a same-channel wait
  does — in **2.5 ns**. A ring of no entries is refused in 1.29 ns, before any
  division, which is what keeps an `EXEC` on a channel the RM handed over
  without a GPFIFO from being a divide-by-zero panic.
- **The per-ioctl decode is nothing, and the ioctl *name* lookup is less than
  nothing.** `is_cpu_prep_ioctl` answers in **0.70 ns** on a prep and 0.74 ns
  rejecting an `EXEC`; the nowait flag test is 0.63 ns. `nouveau_ioctl_name`
  reads **6.5 ns** for `EXEC`, for the last NR in nouveau's range and for an
  unknown one alike — and `the_name_lookup_floor`, a non-inlined call
  returning a `&'static str` and deciding nothing, reads **6.42 ns**. So the
  whole `match` over the `nouveau_drm.h` vocabulary is free and those three
  rows are the harness. `decode_ioc`'s 6.34 ns is the same story in the same
  shape.

## What the present path said

`page_flip` under the host RM shim: the surface is recorded rather than
programmed into a display front end, so every flip figure here is **the
driver's own per-frame bookkeeping and a lower bound on a real flip**, which
adds a pushbuffer write across a PCI aperture and whatever the front end then
takes to fetch it.

- **A steady-state flip is 81 ns**, of which **9.87 ns** is the framebuffer
  lookup that a flip naming an unregistered id pays and nothing else. So the
  drain check, the KMS bookkeeping and the eight counters together are about
  70 ns a frame.
- **The flip is flat in the swapchain's depth**: 78.5 ns round a two-buffer
  swapchain, 78.6 round four, 77.2 round sixteen. That is the claim
  `rotating_a_swapchain_builds_one_ctxdma_per_buffer_not_one_per_flip` makes,
  now with numbers behind it — the lookup walking the swapchain per flip would
  be unmissable at sixteen. Their spread is wide enough to rule out a walk,
  not a nanosecond of drift with depth.
- **`wait_vblank` lands on `b.iter`'s own floor, 0.56 ns**, which says nothing
  about how long a real wait takes — the test clock does not advance under
  this fixture — and everything about the regression it guards: a path that
  spun a frame on the calling CPU could not land there however the clock
  behaved.
- The `/proc` stats line is **180 ns**, eight atomics and a formatted
  `String`: nothing in the flip path has a reason to avoid it.
- **There is deliberately no figure for bringing the flip ladder up.**
  Measuring it needs a fresh GPU per iteration and the fixture costs hundreds
  of microseconds with a spread wider than its own mean, so the build sits
  under the fixture's noise: the two rows that tried came out with a variance
  sixteen times the mean and an "empty equivalent" dearer than the row it was
  supposed to bound. What justifies the lazy build and its latch is the shape
  — once per modeset, against a flip that is 81 ns per frame — not a number
  this harness can produce.
- **Identifying the GPU is free.** `identify_gpu` reads 6.87 ns on this
  hardware's own device id and 6.56 on one in no arm of the table, against
  **6.84 ns** for `the_identification_floor` — a non-inlined call returning
  the same three-field tuple and deciding nothing. The `match` over every
  NVIDIA part the table names is a jump table and costs nothing; those rows
  are the call. `arch_from_pmc_boot0` is inlined and reads **1.4 to 1.6 ns**
  whether the chip id is Turing's or past every range, and naming a GPU fault's
  reason and access type — the one decode here that runs per fault rather than
  once per probe — is 1.87 ns.

## What the EDID decoder said

Once per connector probe rather than per frame, so what matters is the shape.
Pure functions over 128 bytes, no mock: these are the real figures.

- **Validating a block is 1.84 ns** — header compare plus the 128-byte
  checksum — against 0.88 ns for a block whose header is wrong, so the whole
  fold is about a nanosecond because the compiler vectorises it. A block with
  a good header and a bad checksum costs **1.52 ns**, level with accepting
  one, which is what it must be: a refusal that were cheaper would mean the
  fold short-circuits.
- **A probe folding the same block three times costs nothing.** The three
  public entry points each validate for themselves, and
  `a_whole_connector_probe_as_the_caller_makes_it` (19.48 ns) is level with
  `a_whole_connector_probe_validated_once` (19.28). That pair was written
  expecting to show waste and answers no, so the public shape stays as it is —
  every entry point safe to call on bytes nobody has checked — and the pair
  stays as the evidence.
- Decoding the preferred mode is **7.46 ns**, the whole preferred timing
  **10.35 ns**, and reading the physical size **8.82 ns** from the first
  descriptor or **9.81 ns** after walking all four and falling back to the
  centimetre bytes. The derived numbers are cheap too: `is_valid`'s nine
  comparisons 1.66 ns, the refresh in whole hertz 1.90 ns and in millihertz
  2.97 — a 64-bit divide by a product the compiler cannot see, which is what
  stopped a 75, 120 or 144 Hz panel being told it runs at 60.
- **The partial-block completion rows are all floor.** Completing the 32 bytes
  the NVIDIA RM hands back is **12.83 ns** against **12.73 ns** for
  `the_completion_floor` — a non-inlined call returning the same
  `Option<[u8; 128]>` and deciding nothing — so the copy and the 127-byte fold
  cost nothing measurable. The two refusals read 17.7 ns, *dearer* than doing
  the work, which is the giveaway: at this scale the row is the 129-byte
  return value being moved, and the ordering between 12.7 and 17.8 ns is not
  work. A third floor shape worth remembering, after the ~0.6 ns `b.iter` loop
  and the ~6.4 ns call returning an `LxResult`.

## What the GEM handle registry said

`gem_mmap::MAPPINGS` is a `Vec<MappedGem>` behind one mutex, and every
driver-private GEM ioctl goes through it: `GEM_NEW` registers, `GEM_CLOSE`
drops a reference, `GEM_INFO`, `VM_BIND`, the driver-private mmap, PRIME
export and `ADDFB` all look a handle up, and a process exit walks the whole
table. Each operation is a linear scan, with each entry's holder list scanned
in turn. No hardware and no mock: it is integers and a mutex, so these are the
real figures.

The rows ask where "a handful of buffers" stops, because a compositor with
Xwayland and a browser under it holds one object per surface, per texture
upload staging buffer, per dma-buf in flight and per KMS framebuffer.

- **The scan is linear and it is the cost.** A lookup of a handle in the
  middle of the table reads **7.56 ns with one object, 9.52 with 16, 15.05
  with 64, 58.13 with 256 and 175.81 with 1024** — about 0.35 ns per entry
  walked. A **miss** in a table of 1024 is **362.64 ns**, exactly twice the
  middle hit, which is the arithmetic closing: a miss runs to the end. That is
  the figure a generic (dumb) handle pays on every call that asks here first.
- **`lookup_for` walked the table twice, and now walks it once.** It is what
  the driver-private mmap, PRIME export and `ADDFB` call, and it used to ask
  `holds` and then `lookup` — two independent scans for every accepted
  handle: **120.91 ns at 256 live objects where one walk is 58.13**. The
  ownership decision is now a helper over the entry a single scan already
  found, and the row reads **58.57 ns**, level with the bare lookup: the
  check is free on top of the walk it already needed.
  `one_pass_answers_what_the_two_passes_did` walks the matrix the old pair
  covered — tracked or not, driver-private range or below it, held or not,
  and the pid-0 arm — against the two-pass arrangement spelled out in the
  test. It and two of the existing ownership tests all fail if the decision
  is broken.
- **`GEM_NEW` is quadratic in the table.** `register` scans for an existing
  entry before appending, so a fresh handle costs **22.86 ns into an empty
  table and 203.43 ns into one of 256**. Filling the table to 256 is
  therefore about 25 us of scanning, and to 1024 about 400 us — paid once
  per buffer at allocation, not per frame, but paid again by every
  re-register.
- **Process exit is linear, not quadratic.** `release_pid` for a pid that
  holds nothing walks every object and every holder: 78.66 ns over 64 objects
  and 262.76 over 256. `rekey_pid`, the zombie-context handover, walks every
  holder of every object with no early exit: 394.38 ns over 256. Both are
  once-per-process.
- A holder list is not where the cost is: `holds` on the last of **64**
  holders of a single object is **16.88 ns**, against 57.93 ns for one holder
  of an object in the middle of a table of 256. It is the table that is long,
  not the lists.
- **The remaining shape is the `Vec` itself**, and that is a bigger change
  than this batch: keying the registry by handle would turn every row above
  into a constant, but the module's ownership semantics (holders by pid, the
  permissive arm for low handles, the dma-buf sentinel) are what the existing
  tests are about, and swapping the container under them is a decision for
  the author, not a measurement. The numbers to decide with are the ones
  above.

There is no empty-equivalent row in this section and the lookup family does
not need one: a floor says whether a *flat* family is measuring work, and this
family is not flat — it scales with the table and a miss costs exactly twice
a middle hit. A non-inlined control taking the same lock reads 13.5 ns, above
the one-object row it would be bounding, because `lookup` inlines and the
control pays a call it does not. A floor built out of the wrong shape measures
the control.

## What the generic 2D primitives said

`scheme::display::blit_tests::benches`, 30 rows over the primitives every
framebuffer backend inherits: the present path's `blit_from`, the console's
`fill_rect` and `copy_rect`, the kernel-composited cursor's `blit_argb_over`,
the software cursor's `read_into`, and the per-pixel arithmetic under all of
them. The fixture is `blit_tests`'s own fake aperture, a heap `Vec`.

**Run these with `--test-threads=1`.** Two rows hold a process-wide flag and a
spin lock while they measure; run in parallel the family comes out incoherent,
and the first time this batch was measured a 64x64 blit read 1.2 us in one run
and 13 us in the next. CI already pins one thread.

**And every figure is a lower bound.** The fake aperture is write-back, cached
and prefetched. A real scanout is uncached write-combining behind a PCI BAR,
where a store costs far more and a read back costs more still. Ratios inside a
family carry across; absolute numbers do not, and the non-temporal rows do not
carry across at all (see below).

### A test was comparing the non-temporal path with itself

`the_two_write_combining_paths_agree_byte_for_byte` pinned
`dma_sync::test_flag` to false to get `blit_from`'s scalar row copies. It does
not get them. `blit_from` gates the non-temporal path on
`self.fb_write_combining() && has_nt_store()`, and `has_nt_store()` is a
`const fn` returning `cfg!(target_arch = "x86_64")`: it reads no flag, and
`test_flag` moves `HAS_NT_BLIT`, which only `nt_blit_rows` consults. So on
x86_64 both sides of that comparison went down `nt_store_rows` and the test
compared one path with itself -- the exact failure its own doc warns about,
and its guard (`nt_store_calls()` moved) only ever checked the fast side.

The slow side is now a backend that answers `false` to
`fb_write_combining()`, which is what a virtio-gpu host-shared framebuffer is,
and the test asserts `nt_store_calls()` did **not** move for it. Advancing the
wide loop by 68 instead of 64 bytes, and running the old `pinned(false)`
arrangement, both fail it.

### The fill wrote four bytes per loop trip

Measuring it is what found it. The ARGB8888 fast path in `fill_rect` -- the
path the comment above it says becomes "a tight word-store loop" -- wrote one
pixel per trip, and a full-screen clear cost **four times** what `copy_rect`
cost moving the same screen *and reading it back*. A loop bound, not a memory
bound. It now lays a row down in 64-byte lines with a four-byte tail, which on
a real write-combining aperture is better still for the reason
`expand_x_for_wc` exists: the line arrives complete instead of as sixteen
partial combines.

| row | before | after |
| --- | --- | --- |
| `fill_one_console_row_of_1080p` (1920x16) | 9,613 ns | **2,420 ns** |
| `fill_a_64x64_rectangle_in_argb8888` | 1,637 ns | **511 ns** |
| `clear_a_whole_1080p_screen` | 788,039 ns | **549,931 ns** |

Byte for byte the same fill: `end - start` is `(right - left) * 4`, the wide
trips cover sixteen pixels each, the tail covers the rest, and the bound is
still checked once per row before anything is written.
`the_wide_fill_lays_down_exactly_what_the_four_byte_loop_did` compares the
whole aperture against the old loop, spelled out in the test, over nineteen
cases: spans of 1, 15, 16, 17 and 31 pixels, left edges at 1, 3 and 15, padded
and unpadded pitches, every edge clipped, and apertures 1, 40 and 200 bytes
short of `pitch * height`.

### The rest of what the rows say

- **The ARGB8888 special case in `fill_rect` is worth 63x.** The same 64x64
  rectangle: 511 ns through the wide loop, 32,270 ns through `draw_pixel` on a
  24-bit mode. `draw_one_pixel` is 7.95 ns and 4096 of them is 32.6 us, so the
  pixel-by-pixel figure is exactly 4096 `draw_pixel` calls and nothing else.
- **`XMap` is free.** Every mapping row sits on the floor:
  `map_one_pixel_unmirrored` 0.62 ns, `map_one_pixel_mirrored` 0.68,
  `map_a_mirrored_range` 1.21, against `the_mapping_floor` 0.48. Reading the
  mirror flag once per operation instead of once per pixel cost nothing, and
  the mirror itself costs nothing. `check_that_one_pixel_fits` is 0.72 against
  a `bool` floor of 0.94, i.e. inlined away.
- **Clipping is free.** A blit clipped away entirely is 3.70 ns, a fill
  clipped away 2.28, a refused `read_into` 1.33, a `draw_pixel` outside the
  screen 0.60 -- the last of those *is* the floor, so the visible-bounds test
  costs nothing per pixel of a clipped glyph.
- **In `blit_argb_over` the bookkeeping, not the pixel, is the cost.** A 64x64
  cursor: 3,028 ns fully transparent (every pixel skipped on the alpha test),
  5,419 fully opaque, 11,379 half transparent. So 56% of an opaque composite is
  the loop and its four per-pixel branches, and the premultiplied blend adds
  about 1.5 ns per pixel on top. Half off the left edge is 6,046 -- half the
  pixels written and *dearer* than all of them on screen, which says the
  per-pixel clip costs as much as the store it skips. A fully opaque run could
  be a row copy; that is a change to the operator, not a measurement, so it is
  not in this batch.
- **The console scroll is cheaper than the clear it makes room for**: 396,001
  ns to move 1079 rows up by one, against 549,931 to paint the screen. On a
  real aperture that reverses, and badly: `copy_rect` *reads* the framebuffer.
- **The non-temporal rows do not carry across.** A 64x64 window is 13,117 ns
  on the write-combining path and 1,170 ns on the scalar one; a whole 1080p
  frame is 935,228 against 718,597. `MOVNTDQ` into write-back memory, which is
  all a heap `Vec` can be, goes to DRAM instead of staying in the cache the
  next reader would have found it in -- so the fake aperture charges the
  non-temporal path its full cost and pays it none of its benefit. **This pair
  is not an argument for a size threshold.** Which way round the two go on a
  real BAR1 is a hardware question.

## What the input capability bitmap said

`scheme::input::benches`, 12 rows. No hardware and no aperture, so unlike the
display rows these are exact.

- **Asking is free.** `contains` is 0.66 ns against a `bool` floor of 0.94,
  i.e. inlined. A code past the 1024-bit bitmap is 0.50 ns, so the `at` guard
  that keeps an out-of-range code from indexing off the end of a 16-word array
  -- a kernel panic taken from whatever code a driver was handed -- costs
  nothing at all. `contains_all` of nine codes is 6.92 ns, 1.29 when the first
  is missing.
- **`EVIOCGBIT` inbound is 1.7 us, and it is a probe cost, not an event
  cost.** `from_bitmap` walks the wire bit by bit: 757 ns for 1024 clear bits
  and 1,697 ns when every one is set, so 0.74 ns per bit walked and 0.92 more
  per bit set. It is paid once per `(device, event type)` pair when the device
  is probed and never per input event, which is why it is left alone rather
  than rewritten word-at-a-time.
- **The clamp does what it claims.** An 8192-byte bitmap -- 65536 bits, and
  `virtio/input.rs` sizes this from a byte the *device* writes -- is 1,732 ns,
  level with the 128-byte one. Without the clamp it would walk 64512 codes past
  the end and `warn!` about every one.
- **Outbound is free**: `to_le_bytes` is 2.81 ns for sixteen words.

## What the console's shadow framebuffer said

`utils::shadow_fb::tests::benches`, 23 rows over the console's whole CPU-side
present path: the glyph renderer's pixels going in through `put_pixels`, the
scroll through `copy_rect`, the dirty rectangle coming out through
`take_dirty`, the cursor through `cell_pixels`, and `present` /
`present_with_cursor` end to end. Everything here is cached RAM, so unlike the
aperture rows these figures are exact; what they leave out is the device blit
they hand off to, which the `scheme::display` rows price.

The device is a sink that discards. `tests::Recorder` copies every blit into a
fresh `Vec`, and for a full-screen present that is a 4 MiB allocation per
iteration: a row measured against it measures the recorder. Nested one level
inside the test module for the usual reason -- `take_dirty`, `cell_pixels`,
`wc_expand_x` and the `inner` lock are all private.

- **A full-screen present copies 8.3 MiB out of the shadow with interrupts
  off.** `take_a_whole_dirty_screen` is 679,412 ns and
  `present_a_whole_dirty_screen` is 680,227: the present *is* the snapshot
  copy, and that copy runs under the shadow lock, which is IRQ-disabling.
  `present` goes out of its way to release that lock before the device blit,
  and says why -- the snapshot it takes first is still 0.68 ms with interrupts
  off on a VT switch or a scroll. Not a change to make from a bench: the fix
  would be a second scratch buffer, which is a design question.
- **A scrolled line costs about 1.1 ms before a byte reaches the aperture**:
  424,109 ns to move every text row up by one in the shadow, then 680,227 to
  snapshot the screen it dirtied.
- **A clean present is free**: 14.75 ns for the timer tick that finds nothing
  dirty, which is what it finds most of the time. One dirty cell start to
  finish is 71.07.
- **The write-combining widening costs nothing.** `wc_expand_x` is 6.3-6.5 ns
  whether the span is aligned or lands mid-line, against a floor of 0.97, and
  it is called three times per present. Taking a widened cell out of the
  shadow is 62.73 ns against 62.94 for the unwidened one -- the extra seven
  columns are free, because sixteen rows of sixteen words and sixteen rows of
  nine cost the same.
- **The cursor blink is 538 ns, and 80% of it is bookkeeping.** Copying the
  cell out with its nine columns inverted is 110.54 ns and without inversion
  48.73; the rest of the 537.83 is the two `try_lock`s, the widening and the
  `prev_cursor` comparison. Moving one cell (an erase and a draw, two blits) is
  628.00.
- **Filling is already vectorised and clearing is already memory-bound**: a
  text row is 2,519 ns for 30,720 pixels (0.08 ns each), a whole-shadow clear
  465,492 ns for 2,073,600 (0.22 ns each, around 18 GB/s).
- **A negative result, recorded in the code.** A glyph is 268.88 ns for 144
  pixels and 64.99 of that is the iterator and the clip, so the store costs
  about 1.4 ns per pixel -- a lot for a bounds-checked word. `put_pixels`
  updates the dirty bounding box *per pixel*, through the lock guard, which
  looked like the answer. Folding the box in a local and committing it once
  measured 268.88 against 271.43: nothing, inside the noise of either row. So
  the simpler loop stays and the comment above it now says what was tried.
  What a glyph's 1.4 ns actually goes on is the indexed store itself -- the
  multiply, the bounds check and the word.

