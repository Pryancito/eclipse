# Eclipse boot: how it is measured

[Español](README-boot.md) · **English**

"Can it boot faster?" is not answered by reading the code. The expensive parts
of boot are not the ones that compute, they are the ones that **wait**, and
until now there was no way to see them: the kernel timestamps every `dmesg`
line, which says *when it spoke*, not where the time went between two lines;
and `eclipse-init` printed its lines **with no time at all**, so a boot log
pasted into a chat showed the *order* of the boot and hid its *shape* — a
ten-second gate and a ten-millisecond one read the same.

This document describes the two measurements that already exist. It does not
propose cuts: a cut is justified with one of these tables in front of you, not
the other way round.

## 1. Both halves of the bar, in one table

The boot progress bar already ran end to end: **rboot paints 0 % to 51 %**
(`rboot::progress::bar`) and **the kernel 52 % to 100 %**
(`kernel_hal::console::early_progress_bar`), with marks at the end of each
expensive stretch. Those marks were drawn on the framebuffer and forgotten; now
they are also **timestamped**, and both halves come out in the same table.
There is not a single new call: the cost is one relaxed `store` per mark, and a
whole boot has fewer than thirty.

The two halves cannot measure the same way, because they do not share a clock:

- **rboot stores raw `rdtsc`** (`rboot/src/marks.rs`), without calibrating
  anything. The only calibrated clock the firmware offers is
  `BootServices::stall`, and calling it **would spend** 10-20 ms of boot in order
  to measure it: measuring cannot cost what you are trying to cut. The counters
  travel to the kernel in `BootInfo::loader_marks`, which grows at the tail of
  the structure (the ABI convention of this `BootInfo`), and the kernel converts
  them to nanoseconds with `kernel_hal::cpu::tsc_hz()`.
- **The kernel timestamps with `timer_now()`**, already in nanoseconds.

The table is printed on the console at the end of boot, once `init(1)` is
running, and is also kept at the end of `/proc/perf/kernel`, which is where one
asks about the boot on a machine that is already up:

```
boot timeline — 26 marks, 8127.226ms to the last one
  gap = time spent in the stretch ENDING at that mark;
  marks below 81% are timed with the provisional TSC frequency.
  before 0%: about 2777.288ms in the firmware, from processor reset to rboot
  (the TSC counts from reset -- a warm reset or a hypervisor need not start it at 0).
   mark          at         gap  stretch
     0%     0.000ms     0.000ms  rboot: config read, GOP mode set, splash drawn
    15%   190.897ms   190.041ms  rboot: kernel ELF read off the ESP
    45%  3748.113ms  3557.216ms  rboot: initramfs read off the ESP
    46%  7191.059ms  3442.945ms  rboot: memory map walked, kernel ELF and stack mapped
    47%  7199.858ms     8.799ms  rboot: all of physical memory mapped
    49%  7299.951ms    99.793ms  rboot: ExitBootServices
    52%  7300.396ms     0.000ms  entered the kernel (arch entry)
    87%  7838.000ms   454.012ms  PCI scan
   100%  8127.226ms   154.744ms  init(1) running
```

That `before 0%` line is the **firmware before rboot**: the TSC at the 0 % mark
counts from processor reset, so its value is already how long the UEFI took to
reach our loader. It is the only part of boot that is not ours, and it is worth
knowing what it is before celebrating a cut.

The column to read is **`gap`**: what the stretch *ending* at that mark cost.
`at` is only there to line the row up with a `dmesg` time.

Two warnings the table itself prints, because whoever does not know them will
draw the wrong conclusion:

- **Marks below 81 % are timed with the *provisional* TSC frequency.** The real
  one is measured against the ACPI PM timer when device probing starts
  (`recalibrate_tsc_hz`, the 81 % mark); on a machine whose provisional estimate
  was wrong, those early times are wrong by the same factor — and the `[tsc]`
  line in `dmesg` says by how much.
- **A `gap` is wall-clock time, not work.** A stretch waiting on firmware or a
  device looks exactly like one that computes.

Adding a new mark is one call to `early_progress_bar` (or to `step` in
`rboot/src/main.rs`) and one line in `boot_marks::label`. An unnamed mark comes
out as its number, not as a panic: nobody has to go through that file before
instrumenting something. Rows are sorted **by time, not by percentage**,
because a mark painted twice or out of order is precisely what you want to see.

## 2. The init half: `eclipse-init`'s `boot timeline`

Two changes in `tools/eclipse-init`:

1. **Every line carries its time** since the first instruction of PID 1:
   `[eclipse-init] [   3.214s] respawn: labwc (starting)`. It is the cheap half
   of making boot measurable, and it is enough to read a pasted log.
2. **Every wait is timed** and, when the boot loop finishes, the table is
   printed, worst to best, together with what the rows do **not** explain. It is
   also kept in `/run/eclipse-boot-timeline`.

```
boot timeline: 6.204s to the supervision loop, 4.930s of it waiting
     2.011s   32%  at    3.918s  labwc: wait for /dev/input to settle
     1.902s   30%  at    1.004s  gtk-caches: run the oneshot
     ...
     1.274s   20%            everything not timed above (forks, execs, short steps)
```

What is timed are **leaves**, never a wrapper around several: every
`wait_socket =` gate, every `wait_path =`, a directory's settle window separately
from its appearance (they are different costs with different cures: one is the
kernel enumerating devices, the other a fixed window this init chooses), each
`oneshot` run, the pseudo-filesystem mounts and the four `apply_*`. That way the
rows add up to **less** than the total and the difference is real, unexplained
time.

That last row, `everything not timed above`, is the figure that tells whether
the instrumentation is still short: if `rest` is most of the boot, the waits are
not where the time goes and the next measurement has to go somewhere else.

- Rows under 5 ms are not listed (a boot has a long tail of microsecond steps,
  and a table that lists them is one nobody reads to the end), but they **are
  counted**, otherwise `rest` would stop meaning "time nothing explains".
- Nothing is recorded once the table is printed. `start_service` serves boot
  and every later restart alike, so a `respawn` that crashes an hour later goes
  through the same gates; recording those waits would grow the list for as long
  as the machine lives and rewrite the history of a boot that is already over.

## 3. What the measurements justified

### The init: all three settings at once

The init's three `apply_*` — keyboard, language, time zone — were 1.13 s out of
1.46 s, and they are independent of each other: they now run concurrently. The
arithmetic, the lock that makes it safe and the escape hatch
(`init.serial_setup`) are in `docs/README-init.md`.

### rboot: the physical map in 2 MiB pages

The first complete table gave a result that changes the question: **of the
11.8 s of the measured boot, 10.9 s were rboot's**, and everything measured
before — kernel and init together — was 15 % of the boot. Inside rboot, the
stretch ending at the 47 % mark cost **6.98 s**, but that mark measured two
things at once, so the first step was to **split it**: 46 % closes with the
kernel ELF and stack mapped, 47 % with physical memory. Split, the share comes
out **half and half**: about 3.4 s for the ELF and about 3 s for the physical
map.

Those 3 s were a single function: `page_table::map_physical_memory` mapped
**every 4 KiB frame** of physical memory, one by one. Twelve gigabytes are
3,145,728 calls, each walking four table levels and asking the firmware for new
frames. The cost is **linear in the machine's RAM** and the floor is 4 GB no
matter what (`max` over `0x1_0000_0000` in `main.rs`), so a big machine boots
slower for having it.

The same twelve gigabytes are **6,144 mappings with 2 MiB pages**, and that is
what it does now: the 47 % mark went from **2800.8 ms to 8.8 ms**, 318 times
less. Those two figures are the same binary booted twice, with and without the
hatch below, which is the only comparison that counts: under TCG the same
stretch varies 30 % from boot to boot, so a figure from today against one from
yesterday proves nothing. What is not free, and is the reason the signature has
a new argument:

- `map_physical_memory(offset, max_addr, fine, m)` receives in `fine` the
  **physical ranges that must keep 4 KiB leaves**. There is one and it is not
  optional: the kernel walks the boot framebuffer's entries to set them to
  *write-combining* after the PCI scan (`pat::enable_framebuffer_wc`), and that
  **can only be done on a 4 KiB leaf** — a large leaf is skipped, correctly, but
  leaves the framebuffer *write-back*, which on real hardware measured 300 FPS
  versus 2000 in `glxgears`. Any 2 MiB block overlapping a `fine` range is mapped
  the old way.
- A block for which the firmware (or an earlier pass) already has an entry also
  falls back to 4 KiB, where `map_page` tolerates an entry that already points to
  the same frame. That is how the low megabytes the UEFI mapped itself survive.
- `Machine::map_huge` **has no default implementation on purpose**. A silent
  fallback would let the host tests be happy with a loader that had gone back to
  taking seven seconds.
- The hatch is **`PHYSMAP4K`** on the command line, which passes a `fine` that
  covers everything: exactly the previous behaviour. Since `justrun` rewrites the
  `cmdline` of the ESP's `rboot.conf` without recompiling, `make justrun
  KOPTS=PHYSMAP4K` gives the A/B with a single binary.

### What is left untouched

The two big stretches left in rboot, in order:

- **Reading the initramfs**, the 45 % mark: 3.49 s — and this row is a trap that
  is worth reading in full before trying to cut it.

  What `make qemu` puts in `\EFI\zCore\initramfs.img` **is not the 75 MiB boot
  initramfs**, it is the variant's live image, **396 MiB**. An installed machine
  reads the 75 MiB one. And the firmware's FAT driver **is not slow**: 396 MiB in
  3486 ms is **119 MB/s**, and reading the kernel ELF at the 15 % mark gives
  **122 MB/s** separately — two independent measurements that agree. At that
  rate the 75 MiB of an installed machine take about **660 ms**.

  So there is nothing to cut here until there is a table from a real machine:
  what decides this row is **which file is read**, not how fast, and QEMU reads
  one five times bigger than the one really booted. Of the 75 MiB installed,
  about 23 are NVIDIA's GSP firmware blob, which is there on purpose because the
  GPU needs it before the pivot to the btrfs root (see
  `xtask/src/linux/image.rs`).
- **Mapping the kernel ELF**, the 46 % mark: 3.44 s. It is not `.text`: the
  kernel's last `PT_LOAD` has `FileSiz` 0 and **`MemSiz` 548 MiB**, and 512 MiB
  of them are a single object, `zcore::memory::init::HEAP` — the kernel heap,
  reserved in `.bss`. rboot maps it frame by frame (133,936 frames, one firmware
  `allocate_pages` each) and zeroes all of it, on every boot. There are two
  independent cuts there: asking the firmware for large chunks instead of one
  frame per call, and mapping that segment with 2 MiB pages as is already done
  for the physical map. Whether the heap should be 512 static MiB is a separate
  decision, and not ours.

  **The first of the two is already done**: `rboot/src/frames.rs` asks the
  firmware for memory four mebibytes at a time and hands out frames by bumping a
  pointer, so the ~134,000 calls to `allocate_pages` are now 137. The 46 % mark
  went from **3442.9 ms to 2238.6 ms**. What is left in there are two things no
  mark separates yet: the ~134,000 4 KiB `map_to` calls and the 548 MiB
  `write_bytes`. Both are expensive **mostly under TCG**, so a real machine's
  split may make them not worth touching.
- **The kernel's PCI scan**: 441 ms out of 882 ms in QEMU, but that figure is
  for *emulated* PCI. A table from a real machine is needed before touching it.

And the two warnings that apply to all these figures:

- **They are from QEMU with TCG**, without acceleration. A pass that *maps* (the
  physical map) is MMU work and is cut the same on hardware; one that *copies or
  zeroes* a lot of memory is punished much more by emulation than it will be on
  a real machine.
- **It is not the same image.** `make qemu` boots the variant's live image; an
  installed machine boots the 75 MiB initramfs and pivots to btrfs. The 45 % row
  is the one that changes most because of this, but the image size also moves
  `max_phys_addr` and, with it, the physical map.

That is why a real machine's split may be different, and it is the table that
is missing.

## 4. What could already be measured, and is not this

- `BOOTTRACE=<comm>` on the kernel command line records **every file a process
  opens** (`linux-object/src/boot_trace.rs`) and publishes it in
  `/proc/bootprofile`, with a deduplicated preload list. It is the tool for "why
  does labwc take so long to produce its first frame?", not for "where does the
  boot go?".
- `/proc/perf/kernel` (see `docs/README-performance.md`) measures the machine
  **while running**: occupancy, IRQs, scheduler.
