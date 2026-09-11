// Eclipse OS: a Firefox-shaped kernel probe.
//
// Firefox cannot be installed in every environment (its packages live on the
// Alpine CDN), but what it asks of the kernel can be reproduced exactly. Every
// check below mirrors a pattern taken from Firefox's own source rather than
// from a guess about what a browser "probably" does; the comment on each names
// the file it came from. Run it under Eclipse and the failures are the list of
// things standing between this kernel and a browser, in the order Firefox hits
// them.
//
// Build:  musl-gcc -O2 -static -o firefox-probe firefox-probe.c
// Run:    firefox-probe            (add -v for per-check detail)
//
// Exit status is the number of failed checks, capped at 125.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <poll.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// musl ships no <linux/futex.h>; these are the UAPI values.
#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif
#ifndef FUTEX_WAIT_BITSET
#define FUTEX_WAIT_BITSET 9
#endif
#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif
#ifndef MFD_ALLOW_SEALING
#define MFD_ALLOW_SEALING 0x0002U
#endif
#ifndef MFD_NOEXEC_SEAL
#define MFD_NOEXEC_SEAL 0x0008U
#endif
#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034
#endif
#ifndef F_SEAL_SEAL
#define F_SEAL_SEAL 0x0001
#define F_SEAL_SHRINK 0x0002
#define F_SEAL_GROW 0x0004
#define F_SEAL_WRITE 0x0008
#endif
#ifndef F_SEAL_FUTURE_WRITE
#define F_SEAL_FUTURE_WRITE 0x0010
#endif
#ifndef F_DUPFD_CLOEXEC
#define F_DUPFD_CLOEXEC 1030
#endif

static int g_verbose;
static int g_pass, g_fail, g_skip;
static const char *g_section = "";

static void section(const char *name) {
  g_section = name;
  printf("\n== %s\n", name);
}

// A check that Firefox's own code depends on. `why` names the source file the
// pattern came from, so a failure says what it breaks, not just what it is.
static void ok(const char *name, const char *why) {
  g_pass++;
  if (g_verbose) printf("  [ok]   %s  (%s)\n", name, why);
}

static void fail(const char *name, const char *why, int err) {
  g_fail++;
  printf("  [FAIL] %s: %s  (%s)\n", name, err ? strerror(err) : "unexpected result", why);
}

static void skip(const char *name, const char *why) {
  g_skip++;
  if (g_verbose) printf("  [skip] %s  (%s)\n", name, why);
}

static void check(int good, const char *name, const char *why, int err) {
  if (good) ok(name, why); else fail(name, why, err);
}

static int memfd(const char *name, unsigned flags) {
  return (int)syscall(SYS_memfd_create, name, flags);
}

// ── Shared memory ───────────────────────────────────────────────────────────
// ipc/glue/SharedMemoryPlatform_posix.cpp: every IPC buffer and every frame
// handed between the parent and a content process is a memfd. The flags and
// the fallback order below are that file's, including its retry without
// MFD_NOEXEC_SEAL on kernels that reject it.
static void test_shared_memory(void) {
  section("shared memory (content-process IPC, graphics buffers)");

  unsigned flags = MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL;
  int fd = memfd("mozilla-ipc-test", flags);
  int noexec_seal = 1;
  if (fd < 0) {
    noexec_seal = 0;
    flags &= ~MFD_NOEXEC_SEAL;
    fd = memfd("mozilla-ipc-test", flags);
  }
  if (fd < 0) {
    fail("memfd_create(MFD_CLOEXEC|MFD_ALLOW_SEALING)", "SharedMemoryPlatform_posix.cpp", errno);
    printf("         without memfd Firefox falls back to a file in /tmp or /dev/shm;\n"
           "         check that one of those is a writable tmpfs.\n");
    return;
  }
  ok("memfd_create(MFD_CLOEXEC|MFD_ALLOW_SEALING)", "SharedMemoryPlatform_posix.cpp");
  if (noexec_seal) ok("memfd_create(MFD_NOEXEC_SEAL)", "SharedMemoryPlatform_posix.cpp");
  else skip("memfd_create(MFD_NOEXEC_SEAL)", "rejected; Firefox retries without it, as we did");

  const size_t len = 64 * 1024;
  check(ftruncate(fd, (off_t)len) == 0, "ftruncate(memfd)", "SharedMemoryPlatform_posix.cpp Create", errno);

  // Firefox seals shrink+grow so a peer cannot resize the mapping under it.
  int seals = F_SEAL_SHRINK | F_SEAL_GROW;
  check(fcntl(fd, F_ADD_SEALS, seals) == 0, "fcntl(F_ADD_SEALS, SHRINK|GROW)",
        "SharedMemoryPlatform_posix.cpp Freeze", errno);
  int got = fcntl(fd, F_GET_SEALS);
  check(got >= 0 && (got & seals) == seals, "fcntl(F_GET_SEALS)",
        "SharedMemoryPlatform_posix.cpp", got < 0 ? errno : 0);

  // Growing a sealed memfd must be refused: this is what F_SEAL_GROW means,
  // and what Firefox's IsSafeToMap check below is asserting about the file.
  check(ftruncate(fd, (off_t)len * 2) < 0 && errno == EPERM, "sealed memfd refuses grow",
        "F_SEAL_GROW semantics", errno);
  check(ftruncate(fd, (off_t)len / 2) < 0 && errno == EPERM, "sealed memfd refuses shrink",
        "F_SEAL_SHRINK semantics", errno);

  // IsSafeToMap: before mapping ANY shared segment Firefox re-reads the seals
  // and refuses the mapping unless F_SEAL_SHRINK is present. A kernel that
  // accepts F_ADD_SEALS but reports no seals fails this on every IPC buffer
  // and every frame, so check it exactly as Firefox does.
  {
    int s = fcntl(fd, F_GET_SEALS);
    check(!(s == -1 || (~s & F_SEAL_SHRINK)), "IsSafeToMap seal re-check",
          "SharedMemoryPlatform_posix.cpp IsSafeToMap", s < 0 ? errno : 0);
    struct stat st;
    check(fstat(fd, &st) == 0 && (size_t)st.st_size >= len, "IsSafeToMap size check",
          "SharedMemoryPlatform_posix.cpp IsSafeToMap", errno);
  }

  // DupReadOnly: on Linux Firefox makes a read-only handle by re-opening the
  // descriptor through procfs -- NOT with F_DUPFD_CLOEXEC, which is its
  // FreeBSD path. This is how a content process receives a buffer it must not
  // write to, so /proc/self/fd/N has to open and yield a working mapping.
  {
    char path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    int rofd = open(path, O_RDONLY | O_CLOEXEC);
    check(rofd >= 0, "open(/proc/self/fd/N) for a read-only copy",
          "SharedMemoryPlatform_posix.cpp DupReadOnly", errno);
    if (rofd >= 0) {
      void *ro = mmap(NULL, len, PROT_READ, MAP_SHARED, rofd, 0);
      check(ro != MAP_FAILED, "map the read-only copy",
            "SharedMemoryPlatform_posix.cpp DupReadOnly", errno);
      if (ro != MAP_FAILED) munmap(ro, len);
      check(fcntl(rofd, F_GETFD) >= 0 && (fcntl(rofd, F_GETFD) & FD_CLOEXEC),
            "read-only copy is CLOEXEC", "SharedMemoryPlatform_posix.cpp", errno);
      close(rofd);
    }
  }

  void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  check(p != MAP_FAILED, "mmap(MAP_SHARED, memfd)", "SharedMemoryPlatform_posix.cpp Map", errno);
  if (p != MAP_FAILED) {
    memset(p, 0xa5, len);
    // A second independent mapping of the same memfd must see the same bytes:
    // this is the whole basis of cross-process buffer sharing.
    void *q = mmap(NULL, len, PROT_READ, MAP_SHARED, fd, 0);
    if (q == MAP_FAILED) {
      fail("second MAP_SHARED mapping", "SharedMemoryPlatform_posix.cpp", errno);
    } else {
      check(((unsigned char *)q)[len - 1] == 0xa5, "two mappings of one memfd share bytes",
            "cross-process shared memory", 0);
      munmap(q, len);
    }
    munmap(p, len);
  }
  close(fd);
}

// ── fd passing ──────────────────────────────────────────────────────────────
// ipc/chromium: every shared-memory handle and every child-process channel
// crosses as an SCM_RIGHTS control message on a AF_UNIX socketpair.
static void test_fd_passing(void) {
  section("fd passing over AF_UNIX (how shared memory reaches a child)");

  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, sv) != 0) {
    fail("socketpair(SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK)", "ipc/chromium channel", errno);
    return;
  }
  ok("socketpair(SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK)", "ipc/chromium channel");

  int fl = fcntl(sv[0], F_GETFL);
  check(fl >= 0 && (fl & O_NONBLOCK), "socketpair honours SOCK_NONBLOCK", "ipc/chromium", fl < 0 ? errno : 0);
  fl = fcntl(sv[0], F_GETFD);
  check(fl >= 0 && (fl & FD_CLOEXEC), "socketpair honours SOCK_CLOEXEC", "ipc/chromium", fl < 0 ? errno : 0);

  int payload = memfd("moz-scm-rights", MFD_CLOEXEC);
  if (payload < 0) payload = open("/dev/null", O_RDONLY);
  if (payload < 0) { skip("SCM_RIGHTS", "no fd to send"); close(sv[0]); close(sv[1]); return; }
  if (payload >= 0) { (void)ftruncate(payload, 4096); }

  char body[1] = { 'F' };
  struct iovec iov = { .iov_base = body, .iov_len = 1 };
  union { struct cmsghdr align; char buf[CMSG_SPACE(sizeof(int))]; } cmsg;
  memset(&cmsg, 0, sizeof cmsg);
  struct msghdr msg = { 0 };
  msg.msg_iov = &iov; msg.msg_iovlen = 1;
  msg.msg_control = cmsg.buf; msg.msg_controllen = sizeof cmsg.buf;
  struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
  c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(c), &payload, sizeof(int));

  if (sendmsg(sv[0], &msg, 0) != 1) {
    fail("sendmsg(SCM_RIGHTS)", "ipc/chromium: how a child receives shared memory", errno);
    close(payload); close(sv[0]); close(sv[1]);
    return;
  }
  ok("sendmsg(SCM_RIGHTS)", "ipc/chromium: how a child receives shared memory");

  char rbody[1] = { 0 };
  struct iovec riov = { .iov_base = rbody, .iov_len = 1 };
  union { struct cmsghdr align; char buf[CMSG_SPACE(sizeof(int))]; } rcmsg;
  memset(&rcmsg, 0, sizeof rcmsg);
  struct msghdr rmsg = { 0 };
  rmsg.msg_iov = &riov; rmsg.msg_iovlen = 1;
  rmsg.msg_control = rcmsg.buf; rmsg.msg_controllen = sizeof rcmsg.buf;
  ssize_t n = recvmsg(sv[1], &rmsg, MSG_CMSG_CLOEXEC);
  if (n != 1) {
    fail("recvmsg(SCM_RIGHTS)", "ipc/chromium", errno);
  } else {
    ok("recvmsg(SCM_RIGHTS)", "ipc/chromium");
    struct cmsghdr *rc = CMSG_FIRSTHDR(&rmsg);
    if (!rc || rc->cmsg_level != SOL_SOCKET || rc->cmsg_type != SCM_RIGHTS ||
        rc->cmsg_len != CMSG_LEN(sizeof(int))) {
      fail("received control message carries SCM_RIGHTS", "ipc/chromium", 0);
    } else {
      int rfd = -1;
      memcpy(&rfd, CMSG_DATA(rc), sizeof(int));
      check(rfd >= 0, "received a usable descriptor", "ipc/chromium", 0);
      if (rfd >= 0) {
        struct stat st;
        check(fstat(rfd, &st) == 0 && st.st_size == 4096, "passed fd refers to the same object",
              "ipc/chromium: the shared buffer must survive the trip", errno);
        int rfl = fcntl(rfd, F_GETFD);
        check(rfl >= 0 && (rfl & FD_CLOEXEC), "MSG_CMSG_CLOEXEC sets FD_CLOEXEC",
              "ipc/chromium", rfl < 0 ? errno : 0);
        close(rfd);
      }
    }
  }
  close(payload); close(sv[0]); close(sv[1]);
}

// ── Process launch ──────────────────────────────────────────────────────────
// security/sandbox/linux/launch/SandboxLaunch.cpp forks content processes with
// clone(SIGCHLD) (namespace flags only when the sandbox is on). ipc/glue then
// execs the child with the channel fd inherited across the exec.
static void test_process_launch(const char *self) {
  section("content-process launch");

  // The inherited-fd contract: the channel descriptor must NOT be CLOEXEC and
  // must still be open, and still be the same object, in the exec'd child.
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    fail("socketpair for child channel", "ipc/glue/GeckoChildProcessHost.cpp", errno);
    return;
  }
  // fstat on a socket, in this process: if this fails the child's check below
  // cannot be read as "the fd was lost".
  {
    struct stat st;
    check(fstat(sv[0], &st) == 0 && S_ISSOCK(st.st_mode), "fstat describes a socket",
          "ipc/chromium: fd bookkeeping", errno);
  }

  pid_t pid = fork();
  if (pid < 0) {
    fail("fork", "SandboxLaunch.cpp DoClone(SIGCHLD)", errno);
    close(sv[0]); close(sv[1]);
    return;
  }
  if (pid == 0) {
    close(sv[0]);
    // Move the channel to a fixed number the way Firefox does, then exec
    // ourselves in child mode.
    if (dup3(sv[1], 30, 0) < 0) _exit(70);
    char *argv[] = { (char *)self, (char *)"--child-fd", (char *)"30", NULL };
    execv(self, argv);
    // stderr is inherited, so say which path failed and why: exit codes alone
    // cannot tell "the kernel refused the exec" from "the path was wrong".
    fprintf(stderr, "         execv(\"%s\"): %s\n", self, strerror(errno));
    _exit(71);
  }
  close(sv[1]);
  ok("fork", "SandboxLaunch.cpp DoClone(SIGCHLD)");
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) {
    fail("waitpid", "ipc/glue: reaping a content process", errno);
  } else if (!WIFEXITED(status)) {
    fail("child exited normally", "ipc/glue", 0);
    printf("         child was killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : 0);
  } else if (WEXITSTATUS(status) != 0) {
    int rc = WEXITSTATUS(status);
    fail("exec'd child inherits its channel fd", "ipc/glue/GeckoChildProcessHost.cpp", 0);
    printf("         child exit code %d (70=dup3 failed, 71=execv failed,\n"
           "         72=fd did not survive execve, 73=not a socket,\n"
           "         74=fd survived but fstat failed on it)\n", rc);
  } else {
    ok("exec'd child inherits its channel fd", "ipc/glue/GeckoChildProcessHost.cpp");
  }
  close(sv[0]);

  // /proc/self/exe is how Firefox finds its own application directory to build
  // the child's argv[0] (toolkit/xre).
  char buf[PATH_MAX];
  ssize_t n = readlink("/proc/self/exe", buf, sizeof buf - 1);
  check(n > 0, "readlink(/proc/self/exe)", "toolkit/xre: locating the app directory", errno);
}

// ── Threading and the event loop ────────────────────────────────────────────
static void test_runtime(void) {
  section("threads, timers and the event loop");

  // mozglue and NSPR block on futexes for every condition variable and lock.
  int futex_word = 0;
  struct timespec ts = { .tv_sec = 0, .tv_nsec = 1000000 };
  long r = syscall(SYS_futex, &futex_word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 1, &ts, NULL,
                   (uint32_t)-1);
  // Value mismatch (word is 0, we waited on 1) must be reported as EAGAIN.
  check(r < 0 && errno == EAGAIN, "futex(FUTEX_WAIT_BITSET) value check",
        "mozglue: every lock and condition variable", errno);
  r = syscall(SYS_futex, &futex_word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1);
  check(r >= 0, "futex(FUTEX_WAKE)", "mozglue", errno);

  int efd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  check(efd >= 0, "eventfd(EFD_CLOEXEC|EFD_NONBLOCK)", "ipc/chromium message pump", errno);
  int ep = epoll_create1(EPOLL_CLOEXEC);
  check(ep >= 0, "epoll_create1(EPOLL_CLOEXEC)", "ipc/chromium message pump", errno);
  if (efd >= 0 && ep >= 0) {
    struct epoll_event ev = { .events = EPOLLIN, .data.fd = efd };
    check(epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev) == 0, "epoll_ctl(ADD)", "ipc/chromium", errno);
    uint64_t one = 1;
    check(write(efd, &one, sizeof one) == (ssize_t)sizeof one, "write(eventfd)", "ipc/chromium", errno);
    struct epoll_event out;
    int n = epoll_wait(ep, &out, 1, 1000);
    check(n == 1 && out.data.fd == efd, "epoll_wait sees the eventfd", "ipc/chromium message pump",
          n < 0 ? errno : 0);
  }
  if (efd >= 0) close(efd);
  if (ep >= 0) close(ep);

  int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
  check(tfd >= 0, "timerfd_create(CLOCK_MONOTONIC)", "ipc/chromium timers", errno);
  if (tfd >= 0) {
    struct itimerspec its = { .it_value = { .tv_sec = 0, .tv_nsec = 2000000 } };
    check(timerfd_settime(tfd, 0, &its, NULL) == 0, "timerfd_settime", "ipc/chromium timers", errno);
    close(tfd);
  }

  int pfd[2];
  check(pipe2(pfd, O_CLOEXEC | O_NONBLOCK) == 0, "pipe2(O_CLOEXEC|O_NONBLOCK)", "ipc/chromium", errno);
  if (pfd[0] >= 0) { close(pfd[0]); close(pfd[1]); }

  cpu_set_t set;
  CPU_ZERO(&set);
  check(sched_getaffinity(0, sizeof set, &set) == 0 && CPU_COUNT(&set) > 0,
        "sched_getaffinity", "how Firefox sizes its thread pools", errno);

  unsigned char rnd[16];
  check(syscall(SYS_getrandom, rnd, sizeof rnd, 0) == (long)sizeof rnd, "getrandom",
        "NSS / crypto seeding", errno);

  check(prctl(PR_SET_NAME, "MozProbe", 0, 0, 0) == 0, "prctl(PR_SET_NAME)",
        "every Gecko thread names itself", errno);

  struct timespec now;
  check(clock_gettime(CLOCK_MONOTONIC, &now) == 0, "clock_gettime(CLOCK_MONOTONIC)", "mozglue TimeStamp", errno);
}

// ── JIT ─────────────────────────────────────────────────────────────────────
// SpiderMonkey reserves a large address range up front and then commits pages
// out of it, flipping them between writable and executable.
static void test_jit(void) {
  section("SpiderMonkey JIT memory");

  const size_t reserve = 128u * 1024 * 1024;
  void *p = mmap(NULL, reserve, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
  if (p == MAP_FAILED) {
    fail("mmap 128 MiB PROT_NONE reservation", "js/src/gc: the JIT's address reserve", errno);
    return;
  }
  ok("mmap 128 MiB PROT_NONE reservation", "js/src/gc: the JIT's address reserve");

  const size_t page = 4096;
  check(mprotect(p, page * 16, PROT_READ | PROT_WRITE) == 0, "mprotect(RW) inside the reservation",
        "js/src/jit: committing code pages", errno);
  memset(p, 0x90, page * 16);
  check(mprotect(p, page * 16, PROT_READ | PROT_EXEC) == 0, "mprotect(RX) after writing code",
        "js/src/jit: W^X", errno);
  // Firefox returns pages with MADV_DONTNEED and requires them to read back as
  // zero; mozjemalloc's correctness depends on it.
  check(mprotect(p, page * 16, PROT_READ | PROT_WRITE) == 0, "mprotect back to RW", "js/src/jit", errno);
  check(madvise(p, page * 16, MADV_DONTNEED) == 0, "madvise(MADV_DONTNEED)", "mozjemalloc", errno);
  int zero = 1;
  for (size_t i = 0; i < page * 16; i++) if (((unsigned char *)p)[i]) { zero = 0; break; }
  check(zero, "MADV_DONTNEED zeroes the range", "mozjemalloc heap correctness", 0);
  munmap(p, reserve);
}

// ── WASM sandboxes (RLBox) ──────────────────────────────────────────────────
// Firefox compiles its font, image and media decoders to WASM and runs them
// under RLBox. wasm2c's fastest bounds check is "segue": the sandbox heap base
// goes in the GS segment base and every heap access carries a %gs prefix, one
// segment override instead of an add-and-mask. wasm-rt-impl.c installs it with
// arch_prctl(ARCH_SET_GS), and there is no fallback if that fails --
//
//   wasm_rt_syscall_set_segue_base error: Invalid argument
//   Redirecting call to abort() to mozalloc_abort
//
// which is the whole browser gone at startup on a kernel that answers only
// ARCH_SET_FS.
static void test_wasm_sandbox(void) {
  section("wasm sandbox (RLBox: font, image and media decoders)");

#if defined(__x86_64__)
  // <asm/prctl.h> is not in musl's headers; these are the UAPI values.
  const int set_gs = 0x1001, get_gs = 0x1004;
  const char *why = "wasm-rt-impl.c: wasm_rt_syscall_set_segue_base";

  const size_t len = 64 * 1024;
  unsigned char *heap = mmap(NULL, len, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (heap == MAP_FAILED) {
    fail("mmap the sandbox heap", why, errno);
    return;
  }

  if (syscall(SYS_arch_prctl, set_gs, (unsigned long)heap) != 0) {
    fail("arch_prctl(ARCH_SET_GS)", why, errno);
    munmap(heap, len);
    return;
  }
  ok("arch_prctl(ARCH_SET_GS)", why);

  // The base has to survive the trip back out to user mode, and every trap and
  // syscall after it: the generated code reads through %gs for the life of the
  // sandbox. Fault a page in and make a syscall before looking.
  heap[0x40] = 0xa5;
  (void)getpid();
  unsigned char seen = 0;
  __asm__ volatile("movb %%gs:0x40, %0" : "=r"(seen) : : "memory");
  check(seen == 0xa5, "%gs-relative load reaches the sandbox heap",
        "wasm2c segue codegen", 0);

  unsigned long got = ~0ul;
  if (syscall(SYS_arch_prctl, get_gs, &got) != 0) {
    fail("arch_prctl(ARCH_GET_GS)", why, errno);
  } else {
    check(got == (unsigned long)heap, "arch_prctl(ARCH_GET_GS) reports the base", why, 0);
  }

  // Only reached on a kernel that took ARCH_SET_GS above, so this cannot be
  // what kills an older one. A base the CPU would refuse must be rejected
  // here, in the syscall: the kernel writes it with `wrmsr` on the way back to
  // user mode, and a non-canonical value faults there -- in kernel mode, on
  // the exit path. Linux answers EPERM.
  errno = 0;
  check(syscall(SYS_arch_prctl, set_gs, ~0ul - 0xfff) != 0,
        "arch_prctl refuses a non-canonical base",
        "wrmsr would fault in the kernel, not in the sandbox", 0);

  syscall(SYS_arch_prctl, set_gs, 0ul);
  munmap(heap, len);
#else
  skip("arch_prctl(ARCH_SET_GS)", "wasm-rt-impl.c: segue is x86_64-only");
#endif
}

// ── Wayland proxy (how Firefox reaches the compositor) ──────────────────────
// widget/gtk/WaylandProxy.cpp. Firefox does not hand libwayland the
// compositor socket directly: it opens its own AF_UNIX listening socket,
// points WAYLAND_DISPLAY at it, accept()s its own connection and pumps bytes
// and fds between that and the real compositor. The pump is
// ProxiedConnection::TransferOrQueue(), which recvmsg()s in a loop with
// MSG_DONTWAIT and treats ANY result other than EAGAIN/EWOULDBLOCK as a dead
// socket:
//
//   Warning: ProxiedConnection::TransferOrQueue() broken source socket
//   Error: ProxiedConnection::Process(): Failed to read data from client!
//   Error: Failed to open Wayland display, fallback to X11.
//
// Those three lines are one failure: the proxy died, so GTK found no Wayland
// display. The checks below are that pump, in order. Note what they add over
// the socketpair checks above -- a NAMED socket, accept(), and a
// non-blocking recvmsg on the ACCEPTED fd, none of which a socketpair
// exercises.
static void test_wayland_proxy(void) {
  section("wayland proxy (how Firefox reaches the compositor)");

  const char *why = "widget/gtk/WaylandProxy.cpp";

  // The proxy puts its socket next to the compositor's, in XDG_RUNTIME_DIR.
  char path[96]; // < sizeof(sockaddr_un.sun_path), so the copies below provably fit
  const char *dirs[] = {getenv("XDG_RUNTIME_DIR"), "/tmp", "/run", "/"};
  int lfd = -1;
  size_t d = 0;
  for (; d < sizeof dirs / sizeof dirs[0]; d++) {
    if (!dirs[d] || !*dirs[d]) continue;
    snprintf(path, sizeof path, "%s/%seclipse-probe-wl-%d",
             dirs[d], strcmp(dirs[d], "/") ? "" : "", (int)getpid());
    unlink(path);
    lfd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (lfd < 0) break;
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);
    if (bind(lfd, (struct sockaddr *)&sa, sizeof sa) == 0) break;
    close(lfd);
    lfd = -1;
  }
  if (lfd < 0) {
    fail("bind a named AF_UNIX socket", why, errno);
    return;
  }
  ok("bind a named AF_UNIX socket", why);

  if (listen(lfd, 128) != 0) {
    fail("listen", why, errno);
    close(lfd);
    unlink(path);
    return;
  }
  ok("listen", why);

  int cfd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  struct sockaddr_un sa;
  memset(&sa, 0, sizeof sa);
  sa.sun_family = AF_UNIX;
  strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);
  if (cfd < 0 || connect(cfd, (struct sockaddr *)&sa, sizeof sa) != 0) {
    fail("connect to the named socket", why, errno);
    if (cfd >= 0) close(cfd);
    close(lfd);
    unlink(path);
    return;
  }
  ok("connect to the named socket", why);

  int afd = accept(lfd, NULL, NULL);
  if (afd < 0) {
    fail("accept", why, errno);
    close(cfd);
    close(lfd);
    unlink(path);
    return;
  }
  ok("accept", why);

  // THE check. TransferOrQueue() drains the source with MSG_DONTWAIT until it
  // sees EAGAIN; that is its ONLY loop exit. Any other errno is "broken source
  // socket" and the proxy tears itself down -- so an accepted socket with
  // nothing pending must answer EAGAIN and nothing else.
  char rbuf[64];
  struct iovec riov = {rbuf, sizeof rbuf};
  char cbuf[CMSG_SPACE(sizeof(int))];
  struct msghdr rmsg;
  memset(&rmsg, 0, sizeof rmsg);
  rmsg.msg_iov = &riov;
  rmsg.msg_iovlen = 1;
  rmsg.msg_control = cbuf;
  rmsg.msg_controllen = sizeof cbuf;
  errno = 0;
  ssize_t r = recvmsg(afd, &rmsg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
  if (r >= 0) {
    fail("recvmsg(MSG_DONTWAIT) on an idle socket reports EAGAIN", why, 0);
  } else {
    check(errno == EAGAIN || errno == EWOULDBLOCK,
          "recvmsg(MSG_DONTWAIT) on an idle socket reports EAGAIN",
          "TransferOrQueue() ends its drain loop on EAGAIN and ONLY on EAGAIN",
          errno);
  }

  // A byte plus a passed fd, client -> proxy, exactly as a Wayland client
  // sends a request carrying a buffer fd.
  int pfd = memfd("proxy-passed", MFD_CLOEXEC);
  if (pfd < 0) pfd = dup(0);
  {
    char sb[1] = {'w'};
    struct iovec siov = {sb, 1};
    char scbuf[CMSG_SPACE(sizeof(int))];
    memset(scbuf, 0, sizeof scbuf);
    struct msghdr smsg;
    memset(&smsg, 0, sizeof smsg);
    smsg.msg_iov = &siov;
    smsg.msg_iovlen = 1;
    smsg.msg_control = scbuf;
    smsg.msg_controllen = sizeof scbuf;
    struct cmsghdr *c = CMSG_FIRSTHDR(&smsg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &pfd, sizeof(int));
    check(sendmsg(cfd, &smsg, 0) == 1, "sendmsg a byte + SCM_RIGHTS to the proxy",
          why, errno);
  }

  // The proxy polls both ends rather than blocking on either.
  struct pollfd pf = {afd, POLLIN, 0};
  int pr = poll(&pf, 1, 2000);
  check(pr == 1 && (pf.revents & POLLIN), "poll() reports the proxy socket readable",
        "ProxiedConnection::Process() waits in poll()", pr < 0 ? errno : 0);

  memset(&rmsg, 0, sizeof rmsg);
  riov.iov_base = rbuf;
  riov.iov_len = sizeof rbuf;
  rmsg.msg_iov = &riov;
  rmsg.msg_iovlen = 1;
  rmsg.msg_control = cbuf;
  rmsg.msg_controllen = sizeof cbuf;
  r = recvmsg(afd, &rmsg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
  if (r != 1 || rbuf[0] != 'w') {
    fail("recvmsg(MSG_DONTWAIT) returns the queued byte", why, r < 0 ? errno : 0);
  } else {
    ok("recvmsg(MSG_DONTWAIT) returns the queued byte", why);
    struct cmsghdr *c = CMSG_FIRSTHDR(&rmsg);
    if (!c || c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS) {
      fail("the passed fd arrives with it", "a Wayland buffer fd rides its request", 0);
    } else {
      int got = -1;
      memcpy(&got, CMSG_DATA(c), sizeof(int));
      struct stat st;
      check(got >= 0 && fstat(got, &st) == 0, "the passed fd arrives with it",
            "a Wayland buffer fd rides its request", errno);
      if (got >= 0) close(got);
    }
  }

  // And back the other way: the proxy forwards the compositor's events to the
  // client over the same accepted socket.
  {
    char sb[1] = {'e'};
    check(send(afd, sb, 1, 0) == 1, "the accepted socket writes back to the client",
          "events flow proxy -> client", errno);
    char rb[1] = {0};
    check(recv(cfd, rb, 1, 0) == 1 && rb[0] == 'e', "the client reads them",
          "events flow proxy -> client", errno);
  }

  // FIONREAD on the socket. A proxy asks "how much is queued?" before it
  // reads, and on a socket that is an ordinary question with an ordinary
  // answer. Unanswered it used to reach the net ioctl table, miss, and come
  // back as ENOSYS -> ENOTTY -- "Not a tty" for a socket, which is exactly
  // the errno Firefox's proxy reported before it declared the socket broken.
  {
    char sb[3] = {'a', 'b', 'c'};
    if (send(cfd, sb, 3, 0) != 3) {
      fail("send bytes to measure", why, errno);
    } else {
      // Give the bytes a moment to land, then ask.
      struct pollfd wf = {afd, POLLIN, 0};
      poll(&wf, 1, 2000);
      int queued = -1;
      if (ioctl(afd, FIONREAD, &queued) != 0) {
        fail("ioctl(FIONREAD) on a socket", "a socket is not a tty, but it can be measured", errno);
      } else {
        check(queued == 3, "ioctl(FIONREAD) on a socket reports the queued bytes",
              "a proxy sizes the message before it reads it", 0);
      }
      char drain[8];
      (void)recv(afd, drain, sizeof drain, 0);
    }
  }

  // A closed client must read as EOF (0), not as an error: that is how
  // TransferOrQueue() learns the peer is gone instead of calling it broken.
  close(cfd);
  errno = 0;
  r = recv(afd, rbuf, sizeof rbuf, 0);
  check(r == 0, "a closed peer reads as EOF, not an error",
        "TransferOrQueue() distinguishes shutdown from failure", r < 0 ? errno : 0);

  close(pfd);
  close(afd);
  close(lfd);
  unlink(path);
}

// ── Shared memory that is actually shared ───────────────────────────────────
// Passing a memfd to a child proves the descriptor survives; it does not prove
// the MEMORY is shared. Firefox's IPC rings and its prefs map are polled in
// userspace: a parent and child that each map the same memfd but see private
// copies spin forever on a flag that never flips, in state R, making no
// syscalls at all -- no error, no log, nothing to attach a debugger to.
static void test_shared_across_processes(void) {
  section("shared memory across a fork (the IPC ring's real requirement)");

  int fd = memfd("moz-ipc-ring", MFD_CLOEXEC | MFD_ALLOW_SEALING);
  if (fd < 0) { skip("cross-process shared memory", "no memfd"); return; }
  const size_t len = 4096;
  if (ftruncate(fd, (off_t)len) != 0) { fail("ftruncate", "shared ring", errno); close(fd); return; }
  volatile unsigned *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (p == MAP_FAILED) { fail("mmap the ring", "shared ring", errno); close(fd); return; }
  p[0] = 0; p[1] = 0;

  pid_t pid = fork();
  if (pid < 0) { fail("fork", "shared ring", errno); munmap((void *)p, len); close(fd); return; }
  if (pid == 0) {
    // Child: map the SAME fd independently, then hand the parent a value and
    // wait for its reply, exactly as an IPC ring does.
    volatile unsigned *q = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (q == MAP_FAILED) _exit(80);
    q[0] = 0xc0ffee;
    for (int i = 0; i < 20000; i++) {
      if (q[1] == 0xbeef) _exit(0);
      struct timespec t = { .tv_sec = 0, .tv_nsec = 1000000 };
      nanosleep(&t, NULL);
    }
    _exit(81);  // parent's write never became visible here
  }

  int seen = 0;
  for (int i = 0; i < 20000 && !seen; i++) {
    if (p[0] == 0xc0ffee) seen = 1;
    else { struct timespec t = { .tv_sec = 0, .tv_nsec = 1000000 }; nanosleep(&t, NULL); }
  }
  check(seen, "a child's write is visible in the parent's mapping",
        "ipc/chromium: the shared ring both sides poll", 0);
  p[1] = 0xbeef;
  int status = 0;
  waitpid(pid, &status, 0);
  int rc = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  check(rc == 0, "a parent's write is visible in the child's mapping",
        "ipc/chromium: the shared ring both sides poll", 0);
  if (rc != 0)
    printf("         child rc=%d (80=mmap failed, 81=never saw the parent's write)\n", rc);
  munmap((void *)p, len);
  close(fd);
}

// ── Cross-process wakeups ───────────────────────────────────────────────────
// Firefox's parent and its children talk over an AF_UNIX socketpair and sleep
// in poll/epoll between messages. A kernel that delivers the bytes but loses
// the readiness edge deadlocks them both: nothing spins, nothing errors, and
// every process sits idle forever. That is invisible to a single-process test
// -- the write has to happen while the reader is ALREADY blocked, which needs
// two processes and a delay.
static void wakeup_case(const char *what, int use_epoll) {
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    fail(what, "ipc/chromium: parent<->child channel", errno);
    return;
  }
  pid_t pid = fork();
  if (pid < 0) {
    fail(what, "ipc/chromium", errno);
    close(sv[0]); close(sv[1]);
    return;
  }
  if (pid == 0) {
    // Child: block FIRST, then be woken by the parent's write.
    close(sv[0]);
    int rc = 1;
    if (use_epoll) {
      int ep = epoll_create1(EPOLL_CLOEXEC);
      struct epoll_event ev = { .events = EPOLLIN, .data.fd = sv[1] };
      if (ep >= 0 && epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &ev) == 0) {
        struct epoll_event out;
        rc = epoll_wait(ep, &out, 1, 10000) == 1 ? 0 : 2;
      }
    } else {
      struct pollfd p = { .fd = sv[1], .events = POLLIN };
      rc = poll(&p, 1, 10000) == 1 && (p.revents & POLLIN) ? 0 : 2;
    }
    char b;
    if (rc == 0 && read(sv[1], &b, 1) != 1) rc = 3;
    _exit(rc);
  }
  close(sv[1]);
  // Give the child time to reach the blocking call, so the write lands on a
  // sleeping reader -- the case a same-process test can never reach.
  struct timespec nap = { .tv_sec = 0, .tv_nsec = 300000000 };
  nanosleep(&nap, NULL);
  ssize_t w = write(sv[0], "x", 1);
  int status = 0;
  waitpid(pid, &status, 0);
  close(sv[0]);
  if (w != 1) { fail(what, "ipc/chromium: write to the channel", errno); return; }
  if (!WIFEXITED(status)) { fail(what, "ipc/chromium", 0); return; }
  int rc = WEXITSTATUS(status);
  if (rc == 0) { ok(what, "ipc/chromium: a blocked child must wake on a peer write"); return; }
  fail(what, "ipc/chromium: a blocked child must wake on a peer write", 0);
  printf("         child rc=%d (1=setup failed, 2=slept through the write, 3=read failed)\n", rc);
}

// The other order, and the one a handshake actually takes: the peer wrote
// BEFORE this side ever asked to watch the socket. `poll` and `epoll` are
// level-triggered by definition -- they report what is ready now, not what
// became ready while someone was looking. A kernel that only delivers the
// transition loses the message that was already sitting there, and both ends
// then wait to receive from each other forever.
static void ready_before_watch(const char *what, int use_epoll) {
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    fail(what, "ipc/chromium: a handshake already in the pipe", errno);
    return;
  }
  // Write FIRST. Only then does the reader come along and ask to watch.
  if (write(sv[0], "x", 1) != 1) {
    fail(what, "ipc/chromium", errno);
    close(sv[0]); close(sv[1]);
    return;
  }
  int ready;
  if (use_epoll) {
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = { .events = EPOLLIN, .data.fd = sv[1] };
    ready = ep >= 0 && epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &ev) == 0;
    if (ready) {
      struct epoll_event out;
      ready = epoll_wait(ep, &out, 1, 3000) == 1;
    }
    if (ep >= 0) close(ep);
  } else {
    struct pollfd p = { .fd = sv[1], .events = POLLIN };
    ready = poll(&p, 1, 3000) == 1 && (p.revents & POLLIN);
  }
  check(ready, what, "ipc/chromium: level-triggered means already-ready counts", 0);
  close(sv[0]);
  close(sv[1]);
}

static void test_wakeups(void) {
  section("cross-process wakeups (parent <-> child IPC)");
  wakeup_case("poll() wakes on a peer's write", 0);
  wakeup_case("epoll_wait() wakes on a peer's write", 1);
  ready_before_watch("poll() reports data that arrived before the call", 0);
  ready_before_watch("epoll_wait() reports data that arrived before EPOLL_CTL_ADD", 1);
}

// ── The glibc startup gate ──────────────────────────────────────────────────
// Not every Firefox is built against musl. A distribution .deb is glibc, and
// glibc parses `uname().release` in `_dl_discover_osversion` before main()
// runs, calling `__libc_fatal("FATAL: kernel too old")` when the leading
// version triple is below the minimum it was configured with -- 3.2.0 for
// Debian and Ubuntu builds. A kernel that reports its own product version
// there ("0.5.3") fails that test, and NO glibc-linked program can start at
// all, which is invisible from inside a musl userspace.
static void test_uname(void) {
  section("kernel version as glibc reads it");
  struct utsname u;
  if (uname(&u) != 0) {
    fail("uname", "glibc parses release before main() runs", errno);
    return;
  }
  ok("uname", "glibc parses release before main() runs");
  printf("         sysname=%s release=%s machine=%s\n", u.sysname, u.release, u.machine);

  // Exactly glibc's parse: up to three dot-separated numbers, stopping at the
  // first character that is neither.
  int v[3] = { 0, 0, 0 };
  const char *p = u.release;
  for (int i = 0; i < 3; i++) {
    if (*p < '0' || *p > '9') break;
    while (*p >= '0' && *p <= '9') v[i] = v[i] * 10 + (*p++ - '0');
    if (*p == '.') p++; else break;
  }
  long code = (long)v[0] * 65536 + v[1] * 256 + v[2];
  check(code >= 3 * 65536 + 2 * 256, "release parses to at least 3.2.0",
        "below this, glibc aborts every program with \"FATAL: kernel too old\"", 0);
  if (code < 3 * 65536 + 2 * 256)
    printf("         \"%s\" parses as %d.%d.%d\n", u.release, v[0], v[1], v[2]);
}

// ── /proc ───────────────────────────────────────────────────────────────────
static void test_proc(void) {
  section("/proc entries Firefox reads");
  static const struct { const char *path; const char *why; } paths[] = {
    { "/proc/self/maps", "mozglue: address-space bookkeeping and crash reports" },
    { "/proc/self/statm", "memory reporters" },
    { "/proc/self/status", "memory reporters" },
    { "/proc/cpuinfo", "thread-pool sizing and CPU feature detection" },
    { "/proc/meminfo", "how Firefox picks its cache sizes" },
    { "/proc/self/cmdline", "crash reporting" },
  };
  for (size_t i = 0; i < sizeof paths / sizeof paths[0]; i++) {
    int fd = open(paths[i].path, O_RDONLY);
    if (fd < 0) { fail(paths[i].path, paths[i].why, errno); continue; }
    char buf[256];
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    if (n > 0) ok(paths[i].path, paths[i].why);
    else fail(paths[i].path, paths[i].why, n < 0 ? errno : 0);
  }

  // musl's stat/fstat/lstat are statx underneath, so everything Firefox does
  // to a file goes through it. The buffer is opaque here (musl ships no
  // struct statx); 256 bytes covers the UAPI structure.
  unsigned char stx[256];
  int r = (int)syscall(SYS_statx, AT_FDCWD, "/", 0, 0x000007ffU /* STATX_BASIC_STATS */, stx);
  check(r == 0, "statx", "musl's stat family goes through statx", errno);

  struct rlimit rl;
  check(getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur >= 256, "RLIMIT_NOFILE >= 256",
        "Firefox opens hundreds of descriptors", errno);
}

// The child half of the process-launch check.
static int child_main(int fd) {
  // Separate "the descriptor did not survive execve" from "it survived but
  // fstat cannot describe a socket": both look like a dead channel to Firefox
  // but they are different kernel bugs.
  if (fcntl(fd, F_GETFD) < 0) return 72;
  struct stat st;
  if (fstat(fd, &st) != 0) return 74;
  if (!S_ISSOCK(st.st_mode)) return 73;
  return 0;
}

int main(int argc, char **argv) {
  // Firefox does not re-exec argv[0]: toolkit/xre resolves its own binary
  // through /proc/self/exe, because argv[0] is whatever word invoked us and
  // execv() does no PATH search -- launched as a bare name from PATH, argv[0]
  // resolves against the cwd and the exec fails. Mirror that, and keep argv[0]
  // as the fallback for a kernel with no /proc/self/exe.
  char exe[PATH_MAX];
  const char *self = argv[0];
  ssize_t exelen = readlink("/proc/self/exe", exe, sizeof exe - 1);
  if (exelen > 0) {
    exe[exelen] = '\0';
    self = exe;
  }
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "-v")) g_verbose = 1;
    else if (!strcmp(argv[i], "--child-fd") && i + 1 < argc) return child_main(atoi(argv[++i]));
    else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
      printf("usage: firefox-probe [-v]\n"
             "Checks the kernel interfaces Firefox depends on. Each check mirrors a\n"
             "pattern from Firefox's source; failures are what stands between this\n"
             "kernel and running the browser.\n");
      return 0;
    }
  }

  printf("Firefox-shaped kernel probe\n");
  printf("Each check mirrors a pattern from Firefox's own source.\n");

  test_shared_memory();
  test_fd_passing();
  test_process_launch(self);
  test_runtime();
  test_jit();
  test_wasm_sandbox();
  test_wayland_proxy();
  test_shared_across_processes();
  test_wakeups();
  test_uname();
  test_proc();

  printf("\n%d passed, %d failed, %d skipped\n", g_pass, g_fail, g_skip);
  if (g_fail == 0) printf("Nothing here stands in Firefox's way.\n");
  return g_fail > 125 ? 125 : g_fail;
}
