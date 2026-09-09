# firefox-probe

Firefox is a demanding test of a Linux personality, and a slow one to debug:
it is a hundred megabytes of browser that either paints a window or does not.
This tool asks the same questions Firefox asks, one at a time, and says which
answers are wrong.

Every check mirrors a pattern taken from **Firefox's own source**, not from a
guess about what a browser probably does, and each names the file it came
from. That matters: the first version of this probe tested a read-only
descriptor with `F_DUPFD_CLOEXEC`, which is Firefox's *FreeBSD* path — on
Linux it re-opens `/proc/self/fd/N`, an entirely different thing to get right.

## Using it

```sh
make                      # x86_64-linux-musl-gcc, static
./firefox-probe           # summary only
./firefox-probe -v        # one line per check
```

Run it on a Linux box first. It must report **0 failures** there; a failure on
Linux means the probe is wrong, not the kernel. Then run the same binary on
Eclipse. The exit status is the number of failures.

`cargo xtask image` builds it into the rootfs at `/bin/firefox-probe`.

## What it covers, and why each part matters

| Section | What breaks if it fails |
|---|---|
| **Shared memory** | `memfd_create` with `MFD_ALLOW_SEALING`, size sealing, and the `F_GET_SEALS` re-check. Firefox's `Platform::IsSafeToMap` refuses to map *any* shared segment whose seals do not report `F_SEAL_SHRINK` — so a kernel that accepts `F_ADD_SEALS` and then reports "no seals" loses every IPC buffer and every rendered frame. Also `/proc/self/fd/N`, which is how Firefox makes the read-only copy it hands to a content process. |
| **fd passing** | `SCM_RIGHTS` over a `SOCK_CLOEXEC\|SOCK_NONBLOCK` socketpair. This is how a shared-memory handle reaches a child at all. |
| **Content-process launch** | `fork`, `execve` with an inherited channel descriptor, `fstat` on that descriptor, `/proc/self/exe`. Firefox cannot be run single-process on a distribution build (see below), so this path is not optional. |
| **Event loop** | `futex`, `eventfd`, `epoll`, `timerfd`, `pipe2`, `sched_getaffinity`, `getrandom`, `prctl(PR_SET_NAME)`. |
| **JIT memory** | A large `PROT_NONE` reservation, `mprotect` W^X flips, and `MADV_DONTNEED` returning zeroed pages — mozjemalloc's correctness depends on that last one. |
| **/proc** | `maps`, `statm`, `status`, `cpuinfo`, `meminfo`, `cmdline`, plus `statx` and `RLIMIT_NOFILE`. |

## Single-process Firefox is not available

`/usr/local/bin/eclipse-firefox` sets `MOZ_FORCE_DISABLE_E10S=1`, on the
reasoning that a single-process browser exercises no cross-process IPC. That
reasoning does not hold on a distribution build. In `BrowserTabsRemoteAutostart`
(`toolkit/xre/nsAppRunner.cpp`) the variable is honoured only when
`allowDisablingE10s` is true, and in a `MOZILLA_OFFICIAL` build that requires
`xpc::AreNonLocalConnectionsDisabled()` — which is false for any browser
actually able to reach the network. Alpine sets `MOZILLA_OFFICIAL=1` for both
`firefox` and `firefox-esr`, so content processes will spawn regardless, and
the IPC, shared-memory and process-launch sections above are all on the
critical path.
