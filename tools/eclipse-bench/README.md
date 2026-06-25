# eclipse-bench

A small, dependency-free benchmark for Eclipse OS that measures **CPU**,
**memory**, **disk**, and **process creation** and prints comparable numbers.
It is one static musl binary, so it drops straight into the rootfs and runs from
the shell.

Every micro-benchmark is *time-bounded* (it runs for a short budget and counts
work done), so the whole suite finishes in well under a minute even on a slow
USB stick, and adapts to fast (QEMU) vs slow (real disk) machines.

## Build

```sh
make                       # x86_64-linux-musl-gcc -O2 -static
# or: make CC=musl-gcc
```

Copy the resulting `eclipse-bench` into your rootfs image (e.g. `/root`), or
add it to the rootfs build the same way the other `tools/` binaries are added.

## Run

```sh
./eclipse-bench [DIR] [DISK_MB] [MEM_MB]
```

- `DIR` — directory for the disk tests. **It must be on the filesystem you want
  to measure (the btrfs/ext2 root), not a tmpfs** like `/tmp` or `/run`, or the
  "disk" numbers will just measure RAM. Default: current directory.
- `DISK_MB` — size of the disk test file (default 32). Use a smaller value on a
  very slow/small medium.
- `MEM_MB` — memory working-set size (default 32).

Example on a slow USB boot:

```sh
cd /root            # on the btrfs root, NOT /tmp
./eclipse-bench . 16 16
```

## What each line means

**CPU** — the two `latency` lines are dependent op chains, so their rate is
~proportional to the *effective core clock you are actually getting*. This is
the cleanest signal for the P-state / frequency-scaling behaviour: if the CPU is
being throttled to a low P-state, these numbers drop several-fold. `throughput`
adds instruction-level parallelism; `getpid()` is raw syscall (trap) overhead.

**MEMORY** — `memcpy`/`memset` are sequential bandwidth; `random access latency`
is a pointer-chase through a working set larger than cache, i.e. the cache/DRAM
miss latency.

**DISK** — `seq write (+fsync)` and `seq read` are streaming throughput;
`rand 4K read` is random IOPS + average latency (sensitive to per-command
latency on USB/SATA and to the kernel block cache + read-ahead); `fsync latency`
is the commit cost; the `meta` lines (create / stat / unlink many small files)
stress filesystem metadata — exactly the path that makes `exec`, path lookup and
boot slow.

**PROCESS** — `fork + exit` is process-creation cost; `fork + exec(self)` adds a
full address-space replacement + ELF load, i.e. the cost a shell or OpenRC pays
for every command it launches.

## Suggested comparisons

- **QEMU vs USB** — run the same command in both. A large gap on the CPU
  `latency` lines points at frequency scaling on real hardware; a gap that is
  mostly on the DISK lines points at I/O.
- **Before vs after a kernel change** — capture the output, rebuild, capture
  again. The CPU section tracks the P-state policy; the DISK `meta` / `rand 4K`
  lines and `PROCESS` track the block cache, read-ahead and exec path.

Paste the output somewhere you can diff it; the labels and units are stable.
