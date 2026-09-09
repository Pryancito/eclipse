# `WAIT_VBLANK` blocks (resolved) — and the sync-`ioctl` constraint behind it

This started as a handoff for a refactor. The bug is fixed; what remains
worth reading is *how*, because the constraint that shaped the fix is still
there for the next ioctl that needs to wait.

## The bug

`DRM_IOCTL_WAIT_VBLANK`'s blocking form returned immediately instead of
waiting. Measured with `eclipse-bench --only gfx` on Eclipse under QEMU:

```
vblank interval            0.04 ms      <-- should be 16.7 at 60 Hz
vblank jitter (stddev)     0.02 ms
```

The event form (`_DRM_VBLANK_EVENT`) was always correct — it defers through
the timer queue — and page flips were already paced properly at ~60 Hz. Only
the blocking branch was wrong, so a client pacing frames with `drmWaitVBlank`
(X's Present, SDL, older toolkits) spun at full CPU instead of sleeping.

## Why it could not just sleep

`INode::io_control` is synchronous, and there is no synchronous yield in this
kernel — a sync frame cannot give the CPU back. An earlier implementation
"blocked" by busy-spinning the 16.7 ms, which starved every other coroutine on
that CPU and made the machine look frozen. **Do not reintroduce a spin.**

## What was done

Not the broad refactor this document used to recommend (`async fn ioctl` on
`FileLike`, propagated through every implementor). The wait was put where an
async context already exists:

- `sys_ioctl` became `async fn` — the syscall dispatcher was already async, so
  this is one `.await` at the call site and no change to `FileLike` at all.
- Before dispatching, it calls `DrmDev::wait_vblank_sleep`, which resolves the
  target sequence exactly as the sync arm does and sleeps to that vblank's
  deadline. The synthetic counter is a pure function of the clock
  (`drm::vblank_deadline_for_seq`), so there is an exact deadline to sleep to
  and no polling at all.
- The sync arm then reports the sequence that has genuinely completed, because
  the sleep preceded it. Its logic did not change.
- The interception is gated on the fd really being a `DrmDev`, not on the
  request number alone: sleeping is a side effect and must not be inflictable
  on an unrelated fd handed that number.

Two details that turned out to matter:

- **Sleep in a bounded loop, not once.** A wake-up landing a nanosecond before
  the lattice boundary leaves the counter one short; the sync arm then reports
  `target - 1`, the caller's next relative request resolves to a vblank that has
  just passed and returns instantly, and the frame after it waits a full period.
  That alternation shows up as jitter — measured 4.69 ms stddev with a single
  sleep, 1.40 ms with the re-check loop.
- **Cap the wait at 3 s**, matching Linux's `DRM_WAIT_ON(..., 3 * HZ, ...)`, so
  an absolute target far in the future cannot park a thread forever.

## Result

```
                        before      after
vblank interval         0.04 ms     16.5 ms     (60 Hz = 16.7)
vblank jitter (stddev)  0.02 ms      1.40 ms
flip period / vblank    n/a          1.00 x     <-- paced to the refresh
flip rate               61.4 /s      60.8 /s    <-- flip path undisturbed
```

The residual 1.4 ms of jitter is measured under TCG, where timer wake-ups are
imprecise; it has not been re-measured under KVM.

## If another ioctl needs to block

The general refactor is still available and still the cleaner answer if a
second one appears: `async fn ioctl` on `FileLike` with a default forwarding to
the sync version, then override per device. It was not done for one ioctl
because the risk is breadth — `FileLike` is implemented by every file type.

## The other findings from the same run, settled

* `CREATE+DESTROY_DUMB` was suspected of doing synchronous zeroing. It is, and
  that is correct (Linux zeroes dumb buffers too). The measurement now says so:
  1.246 ms for an 800x600x4 buffer, against 1.875 MiB at the measured
  1471 MiB/s framebuffer write speed = 1.27 ms. It is the zeroing, and it is
  the same 54x-the-ioctl-floor that Linux pays (57x).
* Every other row in the section costs the same as Linux or less, relative to
  this machine's own ioctl floor — so `WAIT_VBLANK` was the one real defect.
* `SET_CLIENT_CAP(ATOMIC)` returning `EOPNOTSUPP` is not a bug: the atomic uAPI
  is opt-in behind the `drm.atomic` cmdline flag, deliberately mirroring a Linux
  driver without `DRIVER_ATOMIC`.
