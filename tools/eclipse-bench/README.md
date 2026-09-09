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

- `--only SECTION` — run one of `cpu mem syscall vm sched smp disk proc gfx`.
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
  change did anything.

Paste the output somewhere you can diff it; the labels and units are stable.
