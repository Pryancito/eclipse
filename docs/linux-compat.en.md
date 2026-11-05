# Linux compatibility guided by `Documentation/`

[Español](linux-compat.md) · **English**

Eclipse OS implements its Linux layer (`linux-syscall` + `linux-object`) using
the kernel's documentation tree as the reference:
<https://github.com/torvalds/linux/tree/master/Documentation> (together with the
man7.org man pages, which document the stable UAPI). This page maps the
compatibility subsystems to the documents that guide them and records the
places where our kernel deliberately differs.

## Process identity and job control

| Interface | Reference | Status |
|---|---|---|
| `getpid`/`getppid`/`gettid`, leader TID == PID | proc.rst | Complete |
| Process groups: `setpgid`/`getpgid`/`getpgrp` | credentials.rst, proc.rst | Complete (permissive model, no cross-session validation) |
| Sessions: `setsid`/`getsid` | credentials.rst | Real: per-process sid, inherited across `fork`; `setsid` fails with `EPERM` for group leaders. Detaching from the controlling terminal is not modelled (each process is tied to its VT) |
| `prctl(PR_SET_NAME/PR_GET_NAME)` | prctl(2); `TASK_COMM_LEN` in sched.h | Real per-thread `comm`, visible in `/proc/<pid>/comm`; `execve` resets it to the binary's basename |
| `prctl(PR_SET_PDEATHSIG)` | prctl(2) | Really delivered when the parent dies; cleared in the child after `fork`, as in Linux |
| `prctl(PR_SET_CHILD_SUBREAPER)` | prctl(2) | Orphans are reparented to the nearest living subreaper instead of init (as tmux / session managers use it) |
| `prctl(PR_SET_NO_NEW_PRIVS)` | **Documentation/userspace-api/no_new_privs.rst** | One-way flag, inherited across `fork`/`execve`; while set, `execve` does not honour setuid/setgid bits |
| `prctl` dumpable / timerslack / THP / capbset / tid_address | prctl(2) | Real per-process/per-thread state; unknown options → `EINVAL` (previously *all* of them returned 0, claiming e.g. that seccomp was active) |
| `personality` | personality(2) | Per-process persona (PER_LINUX + modifiers such as `ADDR_NO_RANDOMIZE`), inherited; queryable with 0xffffffff |
| `execve` / `execveat` (`AT_EMPTY_PATH`, `fexecve`) | execve(2), execveat(2) | Implemented |
| `process_vm_readv` / `process_vm_writev` | process_vm_readv(2) | Implemented |

## Signals

| Interface | Reference | Status |
|---|---|---|
| `rt_sigaction`/`rt_sigprocmask`/`rt_sigreturn`/`sigaltstack` | signal.rst (core-api) | Complete |
| `rt_sigpending` | sigpending(2) | The thread's pending-and-blocked set |
| `rt_sigqueueinfo` / `rt_tgsigqueueinfo` | rt_sigqueueinfo(2) | Real delivery with `si_code` validation (`EPERM` when trying to forge kernel codes towards another process). The pending set is a bitmask: the `si_value` payload is not kept |
| `sigsuspend`/`sigtimedwait`/`signalfd` | signalfd.rst | Complete (minimal siginfo) |
| pdeathsig when the parent dies | prctl(2) | See above |

## System V IPC (sysvipc(7), proc.rst → `/proc/sysvipc`)

| Mechanism | Status |
|---|---|
| Semaphores (`semget`/`semop`/`semctl`) | Pre-existing; per-process ids, global rendezvous by key |
| Shared memory (`shmget`/`shmat`/`shmdt`/`shmctl`) | Pre-existing |
| **Message queues** (`msgget`/`msgsnd`/`msgrcv`/`msgctl`) | New: global ids, persistent until `IPC_RMID` (sysvipc(7)), blocking and interruptible send/receive, `MSG_NOERROR`/`MSG_EXCEPT`/`IPC_NOWAIT`, `MSGMAX`/`MSGMNB` limits from Documentation/admin-guide/sysctl/kernel.rst, and a table in `/proc/sysvipc/msg` for `ipcs -q` |

## File system

| Interface | Reference | Status |
|---|---|---|
| `renameat2` | renameat2(2), Documentation/filesystems/porting.rst | Real `RENAME_NOREPLACE` (fails with `EEXIST`); `RENAME_EXCHANGE`/`WHITEOUT` → `EINVAL`, the documented answer of a file system without support |
| `syncfs` | syncfs(2) | Syncs the descriptor's file system |
| `/proc` (proc.rst) | **Documentation/filesystems/proc.rst** | per pid: stat (with real pgrp/session/tty_nr/tpgid), status, maps, fd, comm, environ, statm, exe, cmdline; global: meminfo, stat, uptime, loadavg, mounts, net/*, sys/*, sysvipc/{msg,sem,shm} |
| xattrs (`get/set/list/removexattr` and their `l*`/`f*` forms) | xattr.rst, `fs/xattr.c` | EclipseFS stores extended attributes; every other file system answers as one without them (`ENODATA`/`EOPNOTSUPP`). Name, flags and file are validated first, as in Linux (`ENOENT`, `EBADF`, `ERANGE`, `E2BIG`) |

## Memory

| Interface | Reference | Status |
|---|---|---|
| `mlock`/`mlock2`/`munlock`/`mlockall`/`munlockall` | **Documentation/mm/unevictable-lru.rst**, mlock(2) | Range/flag validation as in Linux; without swap, mapped pages are permanently resident, so success is the truthful answer (what gpg/ssh-agent need) |
| `mmap`/`mprotect`/`munmap`/`mremap`/`madvise`/`mincore` | Documentation/mm/ | Pre-existing (anonymous with demand paging) |
| overcommit / max_map_count in `/proc/sys/vm` | Documentation/admin-guide/sysctl/vm.rst | Linux defaults |

## Deliberate decisions

- **`clone3`**: translated into `clone` (`sys_clone3` in `linux-syscall/src/task.rs`).
  A `clone_args` longer than version 0 with non-zero fields answers `E2BIG`,
  like `copy_struct_from_user`.
- **`rseq` → silent `ENOSYS`**: glibc probes it and falls back gracefully.
- **seccomp**: `prctl(PR_GET/SET_SECCOMP)` answers `EINVAL` like a kernel built
  without `CONFIG_SECCOMP`; the `seccomp` syscall stays `ENOSYS`.
- **Message queues**: `msgrcv` wakes up by short polling (5 ms) instead of with
  a waiter queue; enough for the workloads that use them, and the path stays
  interruptible by signals (`EINTR`) as msgsnd(2) requires.

## Pending (next candidates, with their guiding document)

- `openat2` with `RESOLVE_*` (Documentation/filesystems/path-lookup.rst).
- POSIX `mq_*` queues (mq_overview(7)).
