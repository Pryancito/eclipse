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
#include <pthread.h>
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
// A switch so the thread-creating section can be left out of a run: that is
// the controlled experiment for "does a fork after a thread break the child".
static int g_skip_js;
// Finer switches: the section does two new things (a big split reservation
// and a thread), and only one run per boot is possible, so each has to be
// removable on its own to say which one a later fork trips over.
static int g_no_jit;
static int g_no_jsthread;
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

// ── Mapping coherence ───────────────────────────────────────────────────────
// The parent process builds the shared font list (gfx/thebes/SharedFontList.cpp)
// in memfd blocks it maps MAP_SHARED and writes through the mapping; content
// processes map the same fds read-only later. SQLite maps its -shm files
// MAP_SHARED and grows them with ftruncate after mapping (os_unix.c unixShmMap).
// FreeType maps font files MAP_PRIVATE and reads tables at arbitrary offsets
// (ftsystem.c), while WebRender read()s the same files. All of that assumes one
// coherent view of a file across mmap, read/pread, pwrite, ftruncate and fork.
// Each check names the sequence; a failure prints the first page that differs.
static void cpattern(unsigned char *dst, size_t page, unsigned nonce) {
  for (size_t j = 0; j < 4096; j++) dst[j] = (unsigned char)(page * 7 + j + nonce);
}

// -1 if page `page` at `base` holds the pattern, else the first bad offset.
static long cverify(const unsigned char *base, size_t page, unsigned nonce) {
  const unsigned char *pg = base + page * 4096;
  for (size_t j = 0; j < 4096; j++)
    if (pg[j] != (unsigned char)(page * 7 + j + nonce)) return (long)j;
  return -1;
}

static void creport(const char *what, const unsigned char *base, size_t page, unsigned nonce) {
  long off = cverify(base, page, nonce);
  if (off < 0) return;
  const unsigned char *pg = base + page * 4096;
  printf("         %s: page %zu differs at +%ld: got %02x %02x %02x %02x, expected %02x %02x %02x %02x\n",
         what, page, off, pg[off], pg[off + 1], pg[off + 2], pg[off + 3],
         (unsigned char)(page * 7 + off + nonce), (unsigned char)(page * 7 + off + 1 + nonce),
         (unsigned char)(page * 7 + off + 2 + nonce), (unsigned char)(page * 7 + off + 3 + nonce));
}

// All pages in [from, to) hold the pattern? Reports the first that does not.
static int cverify_range(const char *what, const unsigned char *base, size_t from, size_t to, unsigned nonce) {
  for (size_t pg = from; pg < to; pg++)
    if (cverify(base, pg, nonce) >= 0) { creport(what, base, pg, nonce); return 0; }
  return 1;
}

static int cpread_range(const char *what, int fd, size_t from, size_t to, unsigned nonce) {
  unsigned char buf[4096];
  for (size_t pg = from; pg < to; pg++) {
    ssize_t n = pread(fd, buf, sizeof buf, (off_t)(pg * 4096));
    if (n != (ssize_t)sizeof buf) { printf("         %s: pread page %zu -> %zd\n", what, pg, n); return 0; }
    if (cverify(buf, 0, (unsigned)(pg * 7 + nonce)) >= 0) {
      // cverify with page 0 and nonce folded in == the pattern of page pg
      long off = cverify(buf, 0, (unsigned)(pg * 7 + nonce));
      printf("         %s: pread page %zu differs at +%ld: got %02x expected %02x\n", what, pg, off,
             buf[off], (unsigned char)(pg * 7 + off + nonce));
      return 0;
    }
  }
  return 1;
}

static void test_mapping_coherence(void) {
  section("mapping coherence (shared font list, SQLite -shm, font files)");
  const size_t MiB = 1024 * 1024, PAGES = 256;

  // 1. memfd: store through a mapping, read back every other way.
  {
    int fd = memfd("mozilla-fontlist-block", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, (off_t)MiB) != 0) {
      fail("memfd for the coherence checks", "SharedFontList.cpp", errno);
    } else {
      unsigned char *a = mmap(NULL, MiB, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
      if (a == MAP_FAILED) {
        fail("mmap(MAP_SHARED) of the block", "SharedFontList.cpp", errno);
      } else {
        for (size_t pg = 0; pg < PAGES; pg++) cpattern(a + pg * 4096, pg, 1);
        unsigned char *b = mmap(NULL, MiB, PROT_READ, MAP_SHARED, fd, 0);
        check(b != MAP_FAILED, "second mapping of the written block", "content process maps the fd", errno);
        if (b != MAP_FAILED) {
          check(cverify_range("mapping B", b, 0, PAGES, 1),
                "mapping created AFTER stores through another mapping sees them (all pages)",
                "SharedFontList.cpp: content maps blocks the parent already filled", 0);
          for (size_t pg = 0; pg < PAGES; pg++) cpattern(a + pg * 4096, pg, 2);
          check(cverify_range("mapping B", b, 0, PAGES, 2),
                "existing mapping sees LATER stores through the writer's mapping",
                "SharedFontList.cpp: the parent keeps appending to a block content already maps", 0);
        }
        check(cpread_range("pread", fd, 0, PAGES, 2), "pread() sees stores made through a mapping",
              "mmap and read must be one page cache (file.rs write/read go to the inode, mappings to a VMO)", 0);
        unsigned char pg5[4096];
        cpattern(pg5, 5, 3);
        check(pwrite(fd, pg5, sizeof pg5, 5 * 4096) == (ssize_t)sizeof pg5, "pwrite() into a page every mapping has faulted",
              "SQLite os_unix.c writes -shm through both paths", errno);
        check(cverify(a, 5, 3) < 0 && (b == MAP_FAILED || cverify(b, 5, 3) < 0),
              "mappings see a pwrite() to a page they had already faulted", "one page cache", 0);
        if (cverify(a, 5, 3) >= 0) creport("mapping A", a, 5, 3);
        if (b != MAP_FAILED && cverify(b, 5, 3) >= 0) creport("mapping B", b, 5, 3);
        if (b != MAP_FAILED) munmap(b, MiB);
        munmap(a, MiB);
      }
      close(fd);
    }
  }

  // 2. Grow a memfd after mapping it (SQLite -shm; any file that grows).
  {
    int fd = memfd("sqlite-shm", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, 64 * 1024) != 0) {
      fail("memfd for the growth checks", "os_unix.c unixShmMap", errno);
    } else {
      unsigned char *a = mmap(NULL, 64 * 1024, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
      if (a == MAP_FAILED) {
        fail("mmap the 64 KiB head", "os_unix.c", errno);
      } else {
        for (size_t pg = 0; pg < 16; pg++) cpattern(a + pg * 4096, pg, 4);
        check(ftruncate(fd, (off_t)MiB) == 0, "ftruncate(grow) with a mapping alive", "os_unix.c unixShmMap", errno);
        unsigned char *b = mmap(NULL, MiB, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        check(b != MAP_FAILED, "map the grown file", "os_unix.c", errno);
        if (b != MAP_FAILED) {
          check(cverify_range("post-growth mapping", b, 0, 16, 4),
                "mapping created after growth sees the head written before it",
                "file.rs: a window past the cache VMO must not become a private snapshot", 0);
          for (size_t pg = 16; pg < PAGES; pg++) cpattern(b + pg * 4096, pg, 5);
          unsigned char *c = mmap(NULL, MiB, PROT_READ, MAP_SHARED, fd, 0);
          if (c != MAP_FAILED) {
            check(cverify_range("third mapping", c, 16, PAGES, 5) && cverify_range("third mapping", c, 0, 16, 4),
                  "a later mapping sees writes made through the post-growth mapping", "os_unix.c", 0);
            munmap(c, MiB);
          }
          check(cpread_range("pread", fd, 16, PAGES, 5), "pread() sees the grown tail written through a mapping", "one page cache", 0);
          check(cverify_range("old head mapping", a, 0, 16, 4), "the pre-growth mapping still reads its bytes", "os_unix.c", 0);
          // Truncate to zero and regrow: nothing may come back from the dead.
          check(ftruncate(fd, 0) == 0 && ftruncate(fd, (off_t)MiB) == 0, "ftruncate(0) then regrow", "open(O_TRUNC) rewrite", errno);
          unsigned char *e = mmap(NULL, MiB, PROT_READ, MAP_SHARED, fd, 0);
          if (e != MAP_FAILED) {
            int zero = 1;
            for (size_t j = 0; j < 4096 && zero; j++) if (e[20 * 4096 + j]) zero = 0;
            check(zero, "truncate(0)+regrow reads as zeros through a new mapping (not resurrected data)",
                  "file.rs: resize must invalidate the cache VMO", 0);
            if (!zero) printf("         page 20 after truncate(0)+regrow: %02x %02x %02x %02x\n", e[20 * 4096], e[20 * 4096 + 1], e[20 * 4096 + 2], e[20 * 4096 + 3]);
            munmap(e, MiB);
          }
          munmap(b, MiB);
        }
        munmap(a, 64 * 1024);
      }
      close(fd);
    }
    // A mapping made BEFORE the file grew must see bytes written past the old EOF.
    int fd2 = memfd("grow-under-mapping", MFD_CLOEXEC);
    if (fd2 >= 0 && ftruncate(fd2, 4096) == 0) {
      unsigned char *d = mmap(NULL, 8192, PROT_READ, MAP_SHARED, fd2, 0);
      if (d != MAP_FAILED) {
        volatile unsigned char t = d[0]; (void)t;      // page 0 only: page 1 is past EOF
        check(ftruncate(fd2, 8192) == 0, "grow under a mapping that reaches past the old EOF", "os_unix.c", errno);
        unsigned char z = 'Z';
        check(pwrite(fd2, &z, 1, 4096) == 1, "pwrite past the old EOF", "os_unix.c", errno);
        check(d[4096] == 'Z', "mapping created before the growth sees the byte written past the old EOF",
              "paged.rs: a read past the creation-time size must not pin the zero page", 0);
        if (d[4096] != 'Z') printf("         got %02x, expected 'Z'\n", d[4096]);
        munmap(d, 8192);
      }
      close(fd2);
    }
  }

  // 3. A page READ before anyone wrote it must still see the later write.
  {
    volatile unsigned *p = mmap(NULL, 64 * 1024, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
      fail("mmap(MAP_SHARED|MAP_ANONYMOUS)", "ipc/chromium", errno);
    } else {
      unsigned x = p[0] + p[4096]; (void)x;             // read first: pages 0 and 4
      pid_t pid = fork();
      if (pid == 0) { p[0] = 0xc0ffee; p[4096] = 0xbeef; _exit(0); }
      if (pid > 0) {
        int st = 0; waitpid(pid, &st, 0);
        check(p[0] == 0xc0ffee && p[4096] == 0xbeef,
              "parent sees the child's writes to shared anonymous pages it had only READ before the fork",
              "vmar.rs: a PTE to the zero frame must be replaced when the page is first written", 0);
        if (p[0] != 0xc0ffee || p[4096] != 0xbeef) printf("         got %x %x, expected c0ffee beef\n", p[0], p[4096]);
      } else fail("fork", "process launch", errno);
      munmap((void *)p, 64 * 1024);
    }
    int fd = memfd("read-before-write", MFD_CLOEXEC);
    if (fd >= 0 && ftruncate(fd, 4096) == 0) {
      unsigned char *q = mmap(NULL, 8192, PROT_READ, MAP_SHARED, fd, 0);
      if (q != MAP_FAILED) {
        volatile unsigned char t = q[0]; (void)t;
        if (ftruncate(fd, 8192) == 0) {
          pid_t pid = fork();
          if (pid == 0) {
            unsigned char *w = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            if (w == MAP_FAILED) _exit(2);
            w[0] = 0x11; cpattern(w + 4096, 1, 6);
            _exit(0);
          }
          int st = 0; waitpid(pid, &st, 0);
          check(q[0] == 0x11 && cverify(q, 1, 6) < 0,
                "read-only mapping sees a child's writes, including to a page grown after the mapping",
                "SharedFontList.cpp: content maps read-only what the parent writes later", 0);
          if (q[0] != 0x11 || cverify(q, 1, 6) >= 0) { printf("         q[0]=%02x (11)\n", q[0]); creport("RO mapping", q, 1, 6); }
        }
        munmap(q, 8192);
      }
      close(fd);
    }
  }

  // 4. MADV_DONTNEED on a shared file mapping drops PTEs, never the file's bytes.
  {
    int fd = memfd("dontneed", MFD_CLOEXEC);
    if (fd >= 0 && ftruncate(fd, 4096) == 0) {
      unsigned char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
      if (p != MAP_FAILED) {
        p[0] = 1;
        madvise(p, 4096, MADV_DONTNEED);
        unsigned char *q = mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0);
        check(p[0] == 1 && (q == MAP_FAILED || q[0] == 1),
              "MADV_DONTNEED on a MAP_SHARED memfd keeps the shared bytes",
              "vmar.rs dontneed: zeroing must never reach a shared object", 0);
        if (p[0] != 1 || (q != MAP_FAILED && q[0] != 1)) printf("         p[0]=%d q[0]=%d (expected 1 1)\n", p[0], q == MAP_FAILED ? -1 : q[0]);
        if (q != MAP_FAILED) munmap(q, 4096);
        munmap(p, 4096);
      }
      close(fd);
    }
  }

  // 5. An O_TRUNC rewrite of a file that was once mapped must win over any
  //    stale cached page written back later.
  {
    const char *dirs[] = { "/tmp", "/root", "." };
    for (size_t di = 0; di < 3; di++) {
      char path[256];
      snprintf(path, sizeof path, "%s/ffprobe-wb-%d", dirs[di], (int)getpid());
      int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
      if (fd < 0) continue;
      unsigned char pg[4096];
      memset(pg, 'A', sizeof pg);
      if (write(fd, pg, sizeof pg) != (ssize_t)sizeof pg) { close(fd); unlink(path); continue; }
      unsigned char *m = mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0);
      if (m != MAP_FAILED) { volatile unsigned char t = m[0]; (void)t; munmap(m, 4096); }
      close(fd);
      fd = open(path, O_WRONLY | O_TRUNC);
      memset(pg, 'B', sizeof pg);
      if (fd >= 0) { (void)!write(fd, pg, sizeof pg); close(fd); }
      // Something else mapping another file is what evicts cache entries.
      int other = open("/proc/self/exe", O_RDONLY);
      if (other >= 0) {
        void *om = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, other, 0);
        if (om != MAP_FAILED) munmap(om, 4096);
        close(other);
      }
      fd = open(path, O_RDONLY);
      unsigned char first = 0;
      if (fd >= 0) { (void)!read(fd, &first, 1); close(fd); }
      char name[128];
      snprintf(name, sizeof name, "O_TRUNC rewrite of a once-mapped file survives (%s)", dirs[di]);
      check(first == 'B', name, "file.rs writeback_shared_vmo must not clobber a rewritten inode", 0);
      if (first != 'B') printf("         first byte after rewrite: %02x, expected 'B'\n", first);
      unlink(path);
    }
  }

  // 6. A file mapped MAP_PRIVATE, read in FreeType's order: every page, any
  //    order, equal to read()/pread(), in this process and in a child.
  {
    const size_t FPAGES = 768;                      // 3 MiB, like a font
    char path[256];
    snprintf(path, sizeof path, "/tmp/ffprobe-font-%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
      skip("synthetic font file", "/tmp not writable");
    } else {
      unsigned char pg[4096];
      int wrote = 1;
      for (size_t i = 0; i < FPAGES && wrote; i++) { cpattern(pg, i, 9); wrote = write(fd, pg, sizeof pg) == (ssize_t)sizeof pg; }
      close(fd);
      fd = open(path, O_RDONLY);
      if (!wrote || fd < 0) {
        fail("write the synthetic font file", "ftsystem.c", errno);
      } else {
        unsigned char *m = mmap(NULL, FPAGES * 4096, PROT_READ, MAP_PRIVATE, fd, 0);
        check(m != MAP_FAILED, "mmap(MAP_PRIVATE) of the file", "ftsystem.c FT_Stream_Open", errno);
        if (m != MAP_FAILED) {
          // FreeType's order: the table directory at 0 is read via the same
          // mapping as glyph data deep inside; touch the end first, then the
          // fault-around edges, then everything.
          static const size_t order[] = { 767, 17, 16, 15, 1, 0, 300, 299, 301, 100 };
          int good = 1;
          for (size_t k = 0; k < sizeof order / sizeof order[0] && good; k++)
            if (cverify(m, order[k], 9) >= 0) { creport("private mapping", m, order[k], 9); good = 0; }
          check(good, "MAP_PRIVATE mapping: pages read in non-sequential order match the file",
                "ftsystem.c: tables live at arbitrary offsets", 0);
          check(cverify_range("private mapping", m, 0, FPAGES, 9), "MAP_PRIVATE mapping: every page matches the file",
                "FileFrameFiller must never leave a page short or zero", 0);
          check(cpread_range("pread", fd, 0, FPAGES, 9), "pread() of every page matches",
                "WebRender read()s the font the parent maps", 0);
          unsigned char *m2 = mmap(NULL, FPAGES * 4096, PROT_READ, MAP_PRIVATE, fd, 0);
          if (m2 != MAP_FAILED) {
            check(cverify_range("second private mapping", m2, 0, FPAGES, 9), "a second MAP_PRIVATE mapping matches",
                  "two FT_Face instances of one file", 0);
            munmap(m2, FPAGES * 4096);
          }
          unsigned char *ms = mmap(NULL, FPAGES * 4096, PROT_READ, MAP_SHARED, fd, 0);
          if (ms != MAP_FAILED) {
            check(cverify_range("shared mapping", ms, 0, FPAGES, 9), "a MAP_SHARED read-only mapping matches",
                  "fontconfig maps its caches MAP_SHARED", 0);
            munmap(ms, FPAGES * 4096);
          }
          pid_t pid = fork();
          if (pid == 0) {
            unsigned char *mc = mmap(NULL, FPAGES * 4096, PROT_READ, MAP_PRIVATE, fd, 0);
            if (mc == MAP_FAILED) _exit(2);
            _exit(cverify_range("child private mapping", mc, 0, FPAGES, 9) ? 0 : 1);
          }
          if (pid > 0) {
            int st = 0; waitpid(pid, &st, 0);
            check(WIFEXITED(st) && WEXITSTATUS(st) == 0,
                  "a child's MAP_PRIVATE mapping made after the parent's matches (content-process order)",
                  "content processes map the fonts the parent already mapped", 0);
          }
          munmap(m, FPAGES * 4096);
        }
        close(fd);
      }
      unlink(path);
    }
    // The real fonts, when present: mmap view == read() view, byte for byte.
    const char *fonts[] = { "/usr/share/fonts/dejavu/DejaVuSans.ttf", "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf" };
    for (size_t fi = 0; fi < 2; fi++) {
      int ffd = open(fonts[fi], O_RDONLY);
      if (ffd < 0) { skip(fonts[fi], "not present"); continue; }
      struct stat st;
      if (fstat(ffd, &st) != 0 || st.st_size <= 0) { close(ffd); continue; }
      size_t size = (size_t)st.st_size;
      unsigned char *buf = malloc(size);
      size_t got = 0;
      while (buf && got < size) {
        ssize_t n = read(ffd, buf + got, size - got);
        if (n <= 0) break;
        got += (size_t)n;
      }
      unsigned char *m = mmap(NULL, size, PROT_READ, MAP_PRIVATE, ffd, 0);
      char name[160];
      snprintf(name, sizeof name, "%s: mmap view == read() view", fonts[fi]);
      if (buf && got == size && m != MAP_FAILED) {
        volatile unsigned char t = m[size - 1]; (void)t;   // end first, like the table directory walk
        size_t diff = size;
        for (size_t i = 0; i < size; i++) if (m[i] != buf[i]) { diff = i; break; }
        check(diff == size, name, "WebRender (read) and FreeType (mmap) must agree on the font", 0);
        if (diff != size) printf("         first difference at offset %zu (page %zu): mmap %02x read %02x\n", diff, diff / 4096, m[diff], buf[diff]);
        pid_t pid = fork();
        if (pid == 0) {
          unsigned char *mc = mmap(NULL, size, PROT_READ, MAP_PRIVATE, ffd, 0);
          if (mc == MAP_FAILED) _exit(2);
          _exit(memcmp(mc, buf, size) == 0 ? 0 : 1);
        }
        if (pid > 0) {
          int cst = 0; waitpid(pid, &cst, 0);
          snprintf(name, sizeof name, "%s: a child's mapping == read() view", fonts[fi]);
          check(WIFEXITED(cst) && WEXITSTATUS(cst) == 0, name, "content-process order", 0);
        }
      } else {
        fail(name, "read/mmap of the font failed", errno);
      }
      if (m != MAP_FAILED) munmap(m, size);
      free(buf);
      close(ffd);
    }
  }

  // 7. mremap growth keeps the bytes (mozjemalloc / SQLite grow mappings).
  {
    unsigned char *p = mmap(NULL, 64 * 1024, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p != MAP_FAILED) {
      for (size_t pg = 0; pg < 16; pg++) cpattern(p + pg * 4096, pg, 12);
      unsigned char *q = mremap(p, 64 * 1024, 256 * 1024, MREMAP_MAYMOVE);
      if (q == MAP_FAILED) {
        fail("mremap(MREMAP_MAYMOVE) grow of anonymous memory", "sys_mremap", errno);
        munmap(p, 64 * 1024);
      } else {
        check(cverify_range("mremap'd anon", q, 0, 16, 12), "mremap grow keeps anonymous content", "sys_mremap", 0);
        munmap(q, 256 * 1024);
      }
    }
    int fd = memfd("mremap-shm", MFD_CLOEXEC);
    if (fd >= 0 && ftruncate(fd, 256 * 1024) == 0) {
      unsigned char *p2 = mmap(NULL, 64 * 1024, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
      if (p2 != MAP_FAILED) {
        for (size_t pg = 0; pg < 16; pg++) cpattern(p2 + pg * 4096, pg, 13);
        unsigned char *q2 = mremap(p2, 64 * 1024, 256 * 1024, MREMAP_MAYMOVE);
        if (q2 == MAP_FAILED) {
          fail("mremap grow of a MAP_SHARED memfd mapping", "os_unix.c", errno);
          munmap(p2, 64 * 1024);
        } else {
          cpattern(q2 + 40 * 4096, 40, 13);
          unsigned char *r = mmap(NULL, 256 * 1024, PROT_READ, MAP_SHARED, fd, 0);
          check(cverify_range("mremap'd shared", q2, 0, 16, 13) &&
                    (r == MAP_FAILED || (cverify_range("other mapping", r, 0, 16, 13) && cverify_range("other mapping", r, 40, 41, 13))),
                "mremap grow of a shared mapping keeps the bytes and stays shared", "os_unix.c", 0);
          if (r != MAP_FAILED) munmap(r, 256 * 1024);
          munmap(q2, 256 * 1024);
        }
      }
      close(fd);
    }
  }
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

// FutexThread::initialize()'s shape: a thread that parks on a condition
// variable and is woken by another thread. Bounded by a deadline so a broken
// wake reports a failure instead of hanging the probe.
static pthread_mutex_t g_js_mtx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_js_cv = PTHREAD_COND_INITIALIZER;
static int g_js_flag = 0;
static int g_js_woke = 0;
static int g_js_timedout = 0;

static void *js_futex_worker(void *arg) {
  (void)arg;
  struct timespec deadline;
  clock_gettime(CLOCK_REALTIME, &deadline);
  deadline.tv_sec += 5;
  pthread_mutex_lock(&g_js_mtx);
  while (!g_js_flag) {
    if (pthread_cond_timedwait(&g_js_cv, &g_js_mtx, &deadline) == ETIMEDOUT) {
      g_js_timedout = 1;
      break;
    }
  }
  g_js_woke = g_js_flag;
  pthread_mutex_unlock(&g_js_mtx);
  return NULL;
}

static void test_js_futex(void) {
  pthread_t th;
  if (pthread_create(&th, NULL, js_futex_worker, NULL) != 0) {
    fail("pthread_create", "FutexThread::initialize needs a thread", errno);
    return;
  }
  ok("pthread_create", "FutexThread::initialize needs a thread");
  // Let the worker reach the wait, then wake it.
  struct timespec nap = {0, 100 * 1000 * 1000};
  nanosleep(&nap, NULL);
  pthread_mutex_lock(&g_js_mtx);
  g_js_flag = 1;
  pthread_cond_signal(&g_js_cv);
  pthread_mutex_unlock(&g_js_mtx);
  pthread_join(th, NULL);
  if (g_js_timedout) {
    fail("a condvar signal crosses threads", "Atomics.wait / the engine's parking", 0);
  } else {
    check(g_js_woke, "a condvar signal crosses threads",
          "Atomics.wait / the engine's parking", 0);
  }
}

// ── JavaScript engine startup (SpiderMonkey JS_Init) ───────────────────────
// js/src/vm/Initialization.cpp. Firefox's crash on this kernel is not a null
// dereference at all: it is MOZ_CRASH_UNSAFE(failure) on what JS_Init
// returned, using Firefox's deliberate `*(nullptr) = __LINE__` idiom. JS_Init
// hands back the name of the first check that failed, one of eight:
//
//   js::wasm::Init()                                  js::jit::InitializeJit()
//   js::InitDateTimeState()                           ICU4CLibrary::Initialize()
//   js::CreateHelperThreadsState()                    FutexThread::initialize()
//   js::SharedImmutableStringsCache::initSingleton()
//   js::frontend::WellKnownParserAtoms::initSingleton()
//
// That string lands in gMozCrashReason, which nothing prints, so the guest
// never says which. The checks below are instead what those eight need FROM
// THE KERNEL -- so a failure here names the one to fix, without needing
// Firefox to tell us.
static void test_js_init(void) {
  section("javascript engine startup (SpiderMonkey JS_Init)");

#if defined(__x86_64__)
  if (g_no_jit) {
    skip("reserve a 1 GiB PROT_NONE code pool", "--no-jit");
  } else {
  // js::jit::InitProcessExecutableMemory: reserve the process's whole code
  // budget PROT_NONE up front, trim the ends with munmap to align it, then
  // flip windows inside the reservation between RW and RX as code is emitted.
  // Note what this adds over the JIT checks above: those mmap a region and
  // mprotect the WHOLE of it. This splits a reservation with munmap and then
  // mprotects a window INSIDE what is left -- a mapping the kernel has to
  // carry per-page permissions for.
  const size_t pool = (size_t)1024 * 1024 * 1024;
  const size_t page = (size_t)sysconf(_SC_PAGESIZE);
  char *res = mmap(NULL, pool, PROT_NONE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
  if (res == MAP_FAILED) {
    fail("reserve a 1 GiB PROT_NONE code pool", "js::jit::InitProcessExecutableMemory", errno);
  } else {
    ok("reserve a 1 GiB PROT_NONE code pool", "js::jit::InitProcessExecutableMemory");

    // Trim a slice off each end, splitting the reservation in three.
    int trimmed = munmap(res, page) == 0 &&
                  munmap(res + pool - page, page) == 0;
    check(trimmed, "munmap trims the ends of a reservation",
          "how the JIT aligns its pool", errno);

    // A window inside the remainder: writable, then executable, then gone.
    char *code = res + (16 * page);
    if (mprotect(code, 64 * 1024, PROT_READ | PROT_WRITE) != 0) {
      fail("mprotect a window inside the reservation to RW", "the JIT emits code", errno);
    } else {
      ok("mprotect a window inside the reservation to RW", "the JIT emits code");
      // mov $42,%eax ; ret
      static const unsigned char stub[] = {0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3};
      memcpy(code, stub, sizeof stub);
      if (mprotect(code, 64 * 1024, PROT_READ | PROT_EXEC) != 0) {
        fail("mprotect that window to RX", "the JIT runs what it emitted", errno);
      } else {
        ok("mprotect that window to RX", "the JIT runs what it emitted");
        int (*fn)(void) = (int (*)(void))code;
        check(fn() == 42, "call into the reservation's code window",
              "a JIT that cannot run its output is a dead engine", 0);
      }
      check(mprotect(code, 64 * 1024, PROT_NONE) == 0,
            "hand the window back to PROT_NONE", "the JIT recycles its pool", errno);
    }
    munmap(res + page, pool - 2 * page);
  }
  }
#else
  skip("reserve a PROT_NONE code pool", "the stub below is x86_64 machine code");
#endif

  // js::InitDateTimeState / DateTimeInfo: the engine resolves the local time
  // zone at startup and every Date object depends on it.
  // A missing zone file is a packaging matter, not a kernel one (musl falls
  // back to UTC and the engine still starts), so say which it is.
  if (access("/etc/localtime", R_OK) != 0) {
    fail("/etc/localtime is readable",
         "js::InitDateTimeState resolves the local zone -- `apk add tzdata`", errno);
  } else {
    ok("/etc/localtime is readable", "js::InitDateTimeState resolves the local zone");
  }
  tzset();
  time_t now = time(NULL);
  struct tm lt;
  if (!localtime_r(&now, &lt)) {
    fail("localtime_r", "js::InitDateTimeState", errno);
  } else {
    // Any plausible wall clock. A 1970 date here means the engine would
    // compute every timestamp from a clock that never started.
    check(lt.tm_year + 1900 >= 2020, "localtime_r returns a plausible year",
          "js::InitDateTimeState", 0);
    if (g_verbose) {
      printf("         local: %04d-%02d-%02d %02d:%02d:%02d\n", lt.tm_year + 1900,
             lt.tm_mon + 1, lt.tm_mday, lt.tm_hour, lt.tm_min, lt.tm_sec);
    }
  }
  struct tm ut;
  check(gmtime_r(&now, &ut) != NULL, "gmtime_r", "js::InitDateTimeState", errno);

  // FutexThread::initialize: a thread plus the condvar it parks on. The futex
  // checks above are single-threaded; this is the wake crossing threads,
  // which is what the engine's Atomics.wait machinery is built on.
  if (g_no_jsthread) {
    skip("pthread_create", "--no-jsthread");
  } else {
    test_js_futex();
  }
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
    else if (!strcmp(argv[i], "--skip-js")) g_skip_js = 1;
    else if (!strcmp(argv[i], "--no-jit")) g_no_jit = 1;
    else if (!strcmp(argv[i], "--no-jsthread")) g_no_jsthread = 1;
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
  test_mapping_coherence();
  test_fd_passing();
  test_process_launch(self);
  test_runtime();
  test_jit();
  test_wasm_sandbox();
  test_wayland_proxy();
  if (!g_skip_js) test_js_init();
  test_shared_across_processes();
  test_wakeups();
  test_uname();
  test_proc();

  printf("\n%d passed, %d failed, %d skipped\n", g_pass, g_fail, g_skip);
  if (g_fail == 0) printf("Nothing here stands in Firefox's way.\n");
  return g_fail > 125 ? 125 : g_fail;
}
