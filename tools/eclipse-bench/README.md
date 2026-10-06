# eclipse-bench

A small, dependency-free benchmark for Eclipse OS. One static musl binary, so it
drops straight into the rootfs and runs from the shell.

Every micro-benchmark is *time-bounded* (it runs for a short budget and counts
work done), so the whole suite finishes in well under a minute even on a slow
USB stick, and adapts to fast (QEMU) vs slow (real disk) machines.

## Why the numbers changed

The previous version of this tool reported CPU, memory, disk and `fork` figures
and, read literally, said Eclipse was level with Linux. Using the system did not
agree, and the tool was right about what it measured — it just did not measure
the things that decide how fast a system feels.

Three problems, all now fixed:

1. **Most of it never entered the kernel.** The CPU and memory sections are
   userspace ALU loops and `memcpy` over already-faulted pages. The kernel
   contributes nothing to them, so they *cannot* differ between Eclipse and
   Linux on the same hardware. Passing them is not evidence of anything; every
   such line is now tagged `[user]` and the ones that do exercise the kernel are
   tagged `[kernel]`.
2. **Everything ran alone.** Each measurement had the machine to itself. That is
   exactly the condition under which a scheduler cannot be caught being slow —
   there is never a second runnable task to be delayed behind. Real use is a
   shell, a compositor, daemons and your program all wanting CPU at once. The
   `SCHEDULER / IPC` section now measures with every CPU deliberately saturated.
3. **The costs real programs pay were missing.** Page faults, `mmap`,
   copy-on-write after `fork`, path lookup, context switches, `clock_gettime`,
   SMP scaling — none of them appeared, and between them they dominate the
   runtime of almost any real workload.

## Build

```sh
make                       # x86_64-linux-musl-gcc -O2 -static -pthread
# or: make CC=musl-gcc
```

Copy the resulting `eclipse-bench` into your rootfs image (e.g. `/root`), or add
it to the rootfs build the same way the other `tools/` binaries are added.

## Run

```sh
./eclipse-bench [--only SECTION] [--quick] [DIR] [DISK_MB] [MEM_MB]
```

- `--only SECTION` — run one of `cpu mem syscall vm sched psched net fs heap sig
  futex smp disk proc gfx`.
  Useful for before/after on a single change.
- `--quick` — shorter budgets, roughly 3x faster, noisier.
- `--drm PATH` — DRM device for the `gfx` section (default `/dev/dri/card0`).
- `DIR` — directory for the disk tests. **It must be on the filesystem you want
  to measure (the btrfs/ext2 root), not a tmpfs** like `/tmp` or `/run`, or the
  "disk" numbers will just measure RAM. Default: current directory.
- `DISK_MB` — size of the disk test file (default 32).
- `MEM_MB` — memory working-set size (default 32).

Example on a slow USB boot:

```sh
cd /root            # on the btrfs root, NOT /tmp
./eclipse-bench . 16 16
```

**The comparison that actually means something** is running this same binary on
Linux on the same machine and diffing the two outputs. The `linux: ~N` hints
printed next to each kernel line are order-of-magnitude orientation for a modern
x86_64 box, not targets — a VM or an emulated CPU will miss them by a lot for
reasons that have nothing to do with Eclipse.

## What each section means

**CPU** `[user]` — dependent-operation chains, so their rate is ~proportional to
the effective core clock. Useful only as a frequency/P-state check and as the
clock the `RATIOS` section measures kernel costs against.

**MEMORY** `[user]` — sequential bandwidth and cache/DRAM miss latency over
pre-faulted buffers. A property of the memory system, not the OS.

**SYSCALL** `[kernel]` — `getpid()` is the floor: trap in, trap out. Every other
row adds one subsystem on top, so the *difference* from the `getpid` row
localises the cost. Watch `clock_gettime`: Linux serves it from the vDSO without
entering the kernel at all, so a figure here in `getpid` territory means Eclipse
is taking a real trap for one of the most frequently issued operations there is.

**VM / PAGE FAULTS** `[kernel]` — `mmap`/`mprotect`, demand-zero minor faults and
copy-on-write faults after `fork`. Every program pays these on startup and on
every allocation it touches, and none of them show up in a `memcpy` benchmark.

**SCHEDULER / IPC** `[kernel]` — **the section to watch.** A pipe round trip is
two context switches, so it measures how long anything waits to be handed a CPU.
The `sleep 1ms late` rows are measured twice: on an idle machine, then with one
CPU-bound process per CPU. The gap between those two is the interactive latency
you feel. The `(worst)` rows matter more than the means — one 20 ms stall in
forty prompt wakes is experienced as stuttering and averages away to nothing.

**PREEMPTIVE SCHEDULER INTERNALS** `[kernel]` — the section above says how
long a woken task waited; this one says which mechanism made it wait.

Eclipse's scheduler is not a Linux runqueue. `vendor/PreemptiveScheduler` is a
per-CPU async executor, and the policy in `zircon-object`'s thread code decides
only the *length* of a timeslice, never which task runs next. That shape has
mechanisms of its own, and each can be wrong on its own while every row in
`SCHEDULER / IPC` still looks reasonable. One probe per mechanism:

- **`yield hand-off`** — two threads on one CPU handing the CPU straight back.
  No timer, no descriptor, no wake-up: the executor picking the next task and
  switching to it, and nothing else. It is the floor under every other figure
  here, so `pipe RT / yield hand-off` says how much of a round trip is *not*
  dispatch.
- **the timeslice floor** — a spinner and a thread waking every 200 µs, pinned
  to the *same* CPU. Without a floor the waker preempts the spinner on every
  wake and `throughput retained` collapses; with one, the first stretch of a
  slice belongs to whoever is running. The waker's own latency is printed
  directly underneath because the floor *buys* that throughput with it: high
  retention together with low waker latency is the only unambiguously good
  outcome, and a run showing only the retention would be advertising half of a
  trade.
- **the slice remainder** — a plain spinner against one that spins for most of a
  slice and then parks briefly, both on one CPU. A thread handed a whole new
  slice on every resumption is never preempted and starves its neighbour; one
  that comes back with the remainder it already had is preempted on schedule.
  `plain spinner fair share` is normalised so 1.00 is an even split, so it has
  no hardware in it at all.
- **work stealing** — every worker created by a parent confined to CPU 0, none
  pinned, all CPU-bound. The right outcome is one worker per CPU, whether the
  kernel placed them there at creation or other CPUs noticed the backlog and
  took work; `time to occupy all CPUs` treats both as the success they are and
  `CPUs occupied` is the row that catches work never spreading at all.
- **the idle steal scan** — one second of a deliberately idle machine, read
  from the kernel's own counters. An idle CPU leaves halt on every interrupt,
  and walking every peer's runtime to conclude there is nothing to steal costs
  a lock per peer per wake-up on lines those peers own. `skipped share` is how
  often that question was answered without taking one.
- **affinity** — `sched_setaffinity` is a scheduler operation, not a store:
  narrowing a mask has to kick a CPU in the new mask when the task is runnable
  outside it, and that kick walks the other CPUs' runtimes.
- **cross-CPU wake coalescing** — one waker, N sleepers confined to one *other*
  CPU, woken in a burst. Only the issuing is timed; getting the sleepers parked
  again happens outside the clock, because N targets sharing one CPU
  necessarily serialize there and timing that would report the CPU's width as
  the cost of a wake. Eclipse folds the reschedule request per CPU and Linux
  does not fold distinct futex wakes, so `burst / single` is the two kernels
  against each other rather than a score.
- **per-operation kernel work** — `timer rearms per sleep` should be about one,
  and says the timer is being reprogrammed by something other than the sleep
  that needed it when it is not. `task polls` and `weak execs created per RT`
  are the executor's own accounting: a task that yields in the middle of a poll
  leaves a weak executor holding a 32 KiB stack behind it, and that churn is
  invisible from userspace.

The kernel-side rows read `/proc/perf/kernel`, which Linux does not have, so
there they are `n/a` and the userspace rows are the whole comparison. Every
userspace row runs on both. The section also echoes the boot's `sched mode:`
line, so a captured report cannot be compared against one whose policy
switches differed — `scripts/qemu-bench.sh -c 'WAKEPREEMPT=0'` and
`-c 'TIMERDEADLINE=0'` turn those off on one build, which is the only honest
way to A/B them (rebuilding between A and B changes the binary and its layout,
and TCG run-to-run variance is large enough to hide the effect either way).

**SOCKETS / IPC** `[kernel]` — everything above the bare socketpair the
`SCHEDULER` section already measures. That row is the shortest possible path
through the stack: no address, no listener, no protocol. These are the paths
real programs use.

- `socket()+close()` is the floor for the family, the way `getpid` is the floor
  for the syscall section: subtract it from any other row here.
- `TCP loopback round trip` is the same one-byte exchange as the pipe row with
  the whole IP stack in it, so the gap against the socketpair row is protocol
  processing rather than the wake-up.
- `TCP connect+accept+close` is what a server pays per connection. A kernel can
  answer requests quickly and accept them slowly; that is a common shape and
  one row cannot show both.
- `UNIX stream RT (bound)` reaches the socket by **name** — bind, listen,
  connect through the filesystem — which is what every desktop bus and
  compositor does. Against the socketpair row, the difference is the name and
  the listener.
- `sendmsg / send` is the iovec and control-message plumbing, same byte and
  same socket, so the ratio is that plumbing and nothing else.
- `SCM_RIGHTS fd pass` is passing a descriptor over a UNIX socket and getting a
  byte back: what a browser's zygote and every sandboxed helper live on. An
  `n/a` here is a correctness result, not a slow one — the descriptor did not
  arrive.
- `recv on empty (EAGAIN)` is the non-blocking read every event loop issues per
  spurious readiness notification: kernel entry plus a queue check, with no
  wake-up in it.
- The **readiness rows** are an event loop's floor. One fd is ready and it is
  the *last* one in the array, which is the worst case for an implementation
  that walks the set and the best case for one that keeps a ready list — so the
  `per extra fd` slope between the 1-fd and 64-fd rows is the answer, not either
  row alone. A hundred-connection event loop pays that slope on every wake-up.
  `epoll / poll` above 1 means `epoll` is walking the same set `poll` does and
  buys nothing.

**FILESYSTEM / VFS** `[kernel]` — deliberately *not* the `DISK` section. This
one touches almost no data and measures the layer above the device: resolving a
path, opening a descriptor, answering `stat`, reading a directory, serving a
warm page out of the cache, and formatting the procfs files every tool reads.
All of it runs against files written moments earlier, so the data is in cache
and the device is out of the picture. A kernel can be fast at streaming and slow
at every one of these — and these are what a shell, a build and `ps` spend their
time in.

- `open+close, 1 component` and `open+close, 9 components` differ in exactly one
  thing, how many components the kernel had to resolve, so their difference is
  the `path cost per component`. No single row can show that.
- `openat(dirfd, name)` is what build tools and `find` do; the gap against the
  full-path row is what the resolution they skip was costing.
- `open+close (O_PATH)` gets a row of its own rather than being assumed equal to
  a normal open: `ps` uses it, and Eclipse has had it wrong twice.
- `(lseek+read) / pread` is what maintaining the file position costs — `pread`
  carries the offset, the `read` row pays an explicit `lseek` first, and the
  `lseek` row gives the other half of the arithmetic.
- `mmap+touch 1 MiB file` touches one byte per page rather than copying the
  file, so it is the fault path and not memory bandwidth. Whether faulting
  beats copying is a property of the kernel, not a given.
- `getdents` is charged **per entry**, because that is what scales with a big
  directory while a per-call figure hides it. `entries per getdents call` is the
  batch size: a kernel that returns one entry per syscall costs a directory walk
  a syscall per file, and the per-entry row alone would just read "slow".
- The **procfs rows** are reports the kernel formats on demand. `ps aux` reads
  several of them per process on the machine, which is why a slow one is felt
  and not merely measured. `/proc/self/task listing` reporting `n/a` is the
  directory that used to be missing altogether, which is what killed chromium's
  zygote.

**HEAP / ANONYMOUS MEMORY** — how a program gets memory, from the allocator call
down to the mapping. The split between `[user]` and `[kernel]` here is not fixed
by the row: it is decided by the libc's mmap threshold, and that is the trap this
section exists to expose.

- The `malloc+free` family at 64 B, 4 KiB and 256 KiB is almost always `[user]`:
  the allocator hands back a block it already owns and the kernel is never
  entered. **glibc raises its mmap threshold whenever a large mmap'd block is
  freed**, up to 32 MiB, so a loop that allocates and frees 256 KiB teaches it to
  keep that size on the heap after the first iteration — which is exactly what
  the loop does, and why that row reads tens of nanoseconds rather than the
  thousands an `mmap`/`munmap` pair costs. Its `mmap calls/op` row is the proof:
  a value near 0 is the allocator, a value near 1 is the kernel. The
  `malloc+free 64 MiB` row sits above the threshold's ceiling, so it is a real
  mapping pair on glibc and on musl alike. musl has no adaptive threshold at
  all, which is one more reason a figure from one libc says nothing about the
  other.
- `malloc churn, 512 live` replaces one of 512 live blocks of mixed sizes at a
  scattered index. The plain rows measure the allocator's happiest case, where
  the block it just freed is the one it hands back; a long-running program is
  never in that case.
- `mmap+touch+munmap` and its `MAP_POPULATE` variant are the demand-fault path
  against the up-front one. **A `MAP_POPULATE` figure equal to the plain row
  means the flag was accepted and ignored**, so the faults were still taken one
  page at a time — silently, since the call succeeded.
- `MADV_DONTNEED+refault` is what a heap that shrinks and grows again pays, and
  what every garbage collector and arena allocator does continuously.
- The two `munmap N MiB resident` rows are the teardown a process pays for its
  whole address space on exit. **A per-MiB rate that falls as the mapping grows
  is a teardown that walks something per page rather than per range** — that
  cost is inside the `PROCESS` section's `fork + exit` row, where it cannot be
  separated out.

**SIGNALS** `[kernel]` — delivery, faults, and two correctness verdicts printed
as sentences rather than numbers, because a figure for a mechanism that does not
work is worse than no figure.

- `block+pending+unblock` requires the handler **not** to have run until the mask
  is lifted. A kernel that delivers a blocked signal anyway reports `n/a` here
  instead of a plausible number.
- `SIGSEGV fault+handler+retry` installs a handler that makes the faulting page
  writable and lets the store retry. That is how a JIT, a copy-on-write arena
  and every stack guard work, so it is a path, not a pathology.
- **`SA_RESTART`** is the verdict that matters most. musl's `__synccall`, every
  shell `read` and most library code assume a handler can interrupt a blocking
  syscall without the caller seeing `EINTR`. The `interrupted read, over 2x20ms`
  row is the excess over the two 20 ms timer periods the probe schedules itself;
  the raw elapsed time would be ~40 ms on any kernel and would say nothing. It
  is **one sample** — the probe cannot be repeated inside one interrupted read —
  so it carries the timer's own granularity and swings by a factor of two
  between runs.
- **`sigaltstack`** says whether a handler really ran on the alternate stack. If
  it did not, a fault caused by running out of stack has nowhere to be delivered
  and the process dies instead of handling it.
- `sigsuspend wake, over 1ms` is the signal path's wake-up latency: the process
  really is put to sleep and woken, and the 1 ms timer period is subtracted so
  what is left is the kernel's part.

**FUTEX / LOCKS UNDER CONTENTION** — a lock costs nothing until it is contended,
and the `SCHEDULER` section only measures one uncontended round trip. This
section measures the regime that matters.

- `mutex lock+unlock, alone` is `[user]`: an uncontended `pthread_mutex` is two
  atomic operations and never enters the kernel. The `contended / alone` ratio
  against the two-thread row is what a lock costs when it is actually a lock.
- `FUTEX_WAKE, nobody waiting` is issued by every unlock of a mutex that *might*
  have waiters, so on a mostly-uncontended lock it is the whole kernel cost.
- The wake families come in two flavours at 1, 16 and 64 parked waiters, and
  they answer different questions. **`FUTEX_WAKE issue` times the syscall
  alone**: the kernel picks a waiter off the queue, makes it runnable and
  returns. That is the row that says whether the pick *scans* the queue, because
  nothing in it depends on the machine having a spare CPU. **`wake+observe` adds
  the woken thread reporting back**, which is what a program feels — but on a
  box with fewer CPUs than waiters most of it is that thread queueing for a CPU.
  A flat issue ratio with a growing round-trip ratio means the futex code is
  O(1) and what grows is the wait for a CPU; a growing *issue* ratio is the
  queue being scanned, and that one is the kernel's to fix.
- The `condvar signal` / `broadcast` pair is the thundering herd every
  `notify_all` creates: everyone wakes, and then they all contend for one mutex.
- `FUTEX_WAIT 200us timeout` should read a little over 200 us. Far more is a
  timer that fires late; **far less is a wait that did not wait**, and every
  bounded queue built on it then spins instead of sleeping.

Every row in this section that wakes a thread also has to get a CPU running
again. Under a hypervisor that is a VM exit and an IPI — tens of microseconds on
a vCPU that had halted — so the **absolute** figures belong to the machine and
only the ratios travel. Measured on a 4-vCPU microVM, `FUTEX_WAKE issue` reads
~16 us against the ~1.5 us of bare metal, and the `wake+observe` rows swing by a
factor of two between runs on the same binary. Compare against a Linux control
under the *same* hypervisor or not at all.

**SMP SCALING** `[kernel]` — N threads running the same pure-userspace ALU loop
that one thread ran. The work has no kernel component, so anything short of
linear scaling is the kernel: placement, lock contention, or CPUs that never
came online.

**DISK** `[kernel]` — streaming throughput, random IOPS and latency, `fsync`
commit cost, and the `meta` lines (create / stat / unlink many small files) that
stress exactly the path that makes `exec`, path lookup and boot slow.

**GRAPHICS / DRM-KMS** `[kernel]` — what a compositor pays per frame, on the
raw DRM path: no libdrm, no Mesa, no Wayland, so the same binary runs on Eclipse
and on any Linux without installing anything.

It deliberately does *not* measure a frame rate. Under QEMU both kernels drive
the **same emulated GPU**, so "how fast is the GPU" has no kernel content at
all. What differs between two kernels on one virtual GPU is the cost of the
calls a compositor makes every frame, and that is what these rows are:

- `DRM ioctl floor (GET_CAP)` is the graphics equivalent of the `getpid` row —
  a DRM ioctl that does essentially nothing. Subtract it from any other row to
  separate that operation's real work from generic ioctl dispatch.
- `MODE_GETRESOURCES` / `GETCONNECTOR` / `GETCRTC` are the queries a compositor
  issues at startup and on every hotplug.
- `CREATE+DESTROY_DUMB`, `ADDFB2+RMFB`, `MAP_DUMB+mmap+munmap` are the buffer
  lifecycle: paid on every window resize and every swapchain rebuild.
- **`mapped fb write`** is the one to watch. It is a plain store loop, but the
  number is decided by *the cache policy the kernel chose for the mapping*.
  Write-combining runs at DRAM speed; uncached is 10-50x slower and turns a
  full-screen repaint from a millisecond into tens of them. Nothing else in the
  suite can catch that, and it is invisible in a `memcpy` benchmark over
  ordinary memory — which is why the `fb write / memcpy` ratio exists.
- `page flip -> event` submits a flip and waits for its completion event. The
  wait is the point: the event is the frame actually being on screen. A figure
  near the refresh period means frames are paced; far below it means they are
  not (tearing, and a compositor that spins).
- `vblank interval` / `jitter` is the refresh clock as the kernel delivers it.
  An interval far shorter than any display period means `WAIT_VBLANK` is not
  waiting at all — it answers with the current sequence and returns. The tool
  says so explicitly rather than printing what looks like a very fast number:
  every client that paces itself on it (X's Present, SDL, older toolkits) then
  spins at full CPU instead of sleeping.

The flip, cursor and atomic rows change what is on the display, so they run only
if the process can become DRM master. With a compositor running, `SET_MASTER`
fails and they are reported `n/a` rather than fighting the desktop — stop the
session, or run from a text console, to measure them.

**PROCESS CREATION** `[kernel]` — `fork + exit` is raw process creation;
`fork + exec(self)` adds address-space replacement and a static ELF load;
`fork + exec(sh -c :)` adds path lookup and the dynamic linker, i.e. the cost a
shell or an init system actually pays per command.

`tools/drmbench` is the deeper standalone probe of the same path (atomic
commits, cursor throughput, plane properties) and emits `key=value` lines for
mechanical diffing. The `gfx` section here is the integrated version: fewer
knobs, but it shares the suite's `[user]`/`[kernel]` tagging and its ratios, so
graphics costs land next to the syscall and scheduler costs they compete with.

**RATIOS** — each is a kernel cost divided by something measured on the same
machine in the same run, so hardware speed cancels out. These are the numbers to
quote when someone objects that you are running in a VM. `wake late loaded/idle`
is the headline: near 1 means a woken task gets a CPU straight away even when
the machine is busy; a large value means it waits for someone else's timeslice
to run out, and the system will feel sluggish regardless of how good the
`[user]` numbers look.

## Kernel-side counters

`/proc/perf/kernel` carries the matching kernel view, including:

```
wakeup preempt: N requests (R/s), M honoured (P%)
```

A *request* is raised when a task becomes runnable on a CPU that is busy with a
different task; it is *honoured* when that CPU cuts the running thread's
timeslice short in response. That percentage is the kernel-side twin of the
`wake late loaded/idle` ratio above.

The `psched` section reads this file before and after each probe and prints the
*delta* divided by the operations that caused it, which is the form that
compares across machines. The lines it uses are `sched:` (task polls,
weak-executor yields), `sched steal:` (scans, probes, affinity-empty, skipped),
`sched weak:` (executors created, peak live, soft-cap hits, stack-pool
overflow), `sched stack:` (high-water), `timer rearms:` and `wakeup preempt:`.
They are located by the label that introduces the line and then by position
within it, so a line that is renamed or reordered makes those rows read `n/a`
rather than report a number from a neighbouring field.

### Per-process syscall accounting

The `net` and `fs` sections pair their rows with `/proc/self/perf` instead,
Eclipse's **per-process** syscall table: one row per syscall the process has
issued, with the call count and the time the kernel spent inside it. For these
sections that is the better pairing, for two reasons.

It is *our* process, so an idle shell or a daemon waking up cannot move the
numbers — the system-wide table cannot promise that. And it splits a measured
figure into two different bugs: `calls/op` is how many syscalls libc really
issued (a value that is not the expected integer means the row is not measuring
what its label says — a retry loop, a short read), and `in-kernel` is what share
of the measured wall clock was spent inside them. A low in-kernel share with a
high ns/op puts the cost in entry/exit or in being rescheduled, not in the
subsystem. A single ns/op figure cannot tell those apart.

The reader's own `openat`/`read`/`close` land in the very rows an open or read
probe wants to read back, so its cost is **measured** once — two back-to-back
snapshots with no work between them — and subtracted, rather than assumed.
Linux has no such file, so these rows read `n/a` there, the same convention the
`/proc/perf/kernel` rows follow.

## Suggested comparisons

- **Eclipse vs Linux, same machine** — the only comparison that settles an
  argument. Same binary, same `DIR`, diff the output.
- **Eclipse vs Linux under QEMU** — `scripts/qemu-bench.sh` boots Eclipse and
  `scripts/qemu-linux-bench.sh` boots a stock Linux kernel under the *same*
  QEMU machine, and both run this same binary. For the `gfx` section the Linux
  control also needs a DRM driver bound to the emulated GPU, or it will report
  "no DRM device" and there is nothing to compare — pass `-g` to that script and
  it stages the driver stack into the initramfs. Both harnesses already give the
  guest the same emulated GPU (QEMU's default std VGA on q35). Under TCG (no KVM) absolute figures describe QEMU's emulator, so read
  the RATIOS section rather than the nanoseconds.
- **QEMU vs real hardware** — a large gap on the `[user]` CPU lines points at
  frequency scaling; a gap mostly on `DISK` points at I/O.
- **Before vs after a kernel change** — capture the output, rebuild, capture
  again. `--only sched` is usually the fastest way to see whether a scheduler
  change did anything, and `--only psched` the fastest way to see *which*
  mechanism it moved.
- **After touching sockets, the VFS or procfs** — `--only net` and `--only fs`
  are the matching fast paths. Both sections are userspace-only probes, so they
  run unchanged on the Linux control and the whole output is comparable.
- **After touching the allocator, signal delivery or the futex code** —
  `--only heap`, `--only sig` and `--only futex`. These are userspace-only too,
  so the Linux control runs them unchanged. For `heap`, remember that the
  control's libc decides which rows are `[user]`, so compare the `mmap calls/op`
  rows before comparing the nanoseconds. For `futex`, compare the ratios: the
  absolute wake figures are a property of the host, not of the kernel under
  test.

Paste the output somewhere you can diff it; the labels and units are stable.
