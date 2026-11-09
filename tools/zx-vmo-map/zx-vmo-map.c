// zx-vmo-map: mapping a Zircon VMO from Linux, through an fd.
//
// A VMO is made through the gateway, moved into an fd (eclipse_handle_to_fd)
// and mapped with mmap(MAP_SHARED). It checks that the mapping is the VMO
// itself (stores through the mapping show in zx_vmo_read and the other way
// round, a forked child's stores show in the parent), that the window must
// lie inside the VMO, that MAP_PRIVATE is refused, and that the handle's
// rights are the ceiling: a handle without WRITE maps read-only, and
// mprotect cannot make it writable afterwards. This is what a userspace
// driver maps its registers and its DMA memory with.
//
//   zx-vmo-map
//
// Run it on Eclipse booted with `zircon.gateway=1`. Exit status: 0 on
// success, 2 when the gateway is off (ENOSYS), 1 on error.

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define ZX_GATEWAY_BASE 0x5a000000L

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_handle_duplicate 47
#define ZX_SYS_handle_replace 48
#define ZX_SYS_vmo_create 191
#define ZX_SYS_vmo_read 198
#define ZX_SYS_vmo_write 204

#define ECLIPSE_SYS_handle_to_fd 0x4000

// zircon-object/src/object/rights.rs
#define ZX_RIGHT_TRANSFER (1u << 1)
#define ZX_RIGHT_READ (1u << 2)
#define ZX_RIGHT_MAP (1u << 5)
#define ZX_RIGHT_SAME_RIGHTS (1u << 31)

#define PAGE 4096
#define PAGES 4

typedef uint32_t zx_handle_t;

static long zx(long n, long a, long b, long c, long d) {
    return syscall(ZX_GATEWAY_BASE + n, a, b, c, d);
}

// musl's syscall() turns a negative return into errno for the -4095..-1
// range, which is where every zx_status_t lives: undo that.
static long zx_status(long r) { return r == -1 ? -(long)errno : r; }

static int failures;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("zx-vmo-map: FAIL: %s (errno %d)\n", what, errno);
        failures++;
    }
}

static void must(long status, const char *what) {
    if (status != 0) {
        printf("zx-vmo-map: %s: status %ld\n", what, status);
        _exit(1);
    }
}

// A fresh VMO of PAGES pages: one handle stays in the table (*kept), the
// other, narrowed to `rights` (0: all of them), goes into the fd returned.
static int vmo_fd(uint32_t rights, zx_handle_t *kept) {
    zx_handle_t vmo, dup;
    long r = zx_status(zx(ZX_SYS_vmo_create, PAGES * PAGE, 0, (long)&vmo, 0));
    if (r == -ENOSYS) {
        printf("zx-vmo-map: the gateway is off (boot with zircon.gateway=1)\n");
        _exit(2);
    }
    must(r, "vmo_create");
    must(zx_status(zx(ZX_SYS_handle_duplicate, vmo,
                      rights ? rights : ZX_RIGHT_SAME_RIGHTS, (long)&dup, 0)),
         "handle_duplicate");
    *kept = vmo;
    int fd = (int)zx_status(zx(ECLIPSE_SYS_handle_to_fd, dup, 0, 0, 0));
    if (fd < 0) {
        printf("zx-vmo-map: handle_to_fd: %d\n", fd);
        _exit(1);
    }
    return fd;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    zx_handle_t vmo;
    int fd = vmo_fd(0, &vmo);

    // The mapping is the VMO: both ways.
    char *map = mmap(NULL, PAGES * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(map != MAP_FAILED, "mmap(MAP_SHARED) of a VMO fd");
    if (map == MAP_FAILED)
        return 1;
    static const char a[] = "written through the mapping";
    static const char b[] = "written with zx_vmo_write";
    memcpy(map + PAGE + 8, a, sizeof a);
    char buf[64] = {0};
    must(zx_status(zx(ZX_SYS_vmo_read, vmo, (long)buf, PAGE + 8, sizeof a)), "vmo_read");
    check(memcmp(buf, a, sizeof a) == 0, "a store through the mapping is in the VMO");
    must(zx_status(zx(ZX_SYS_vmo_write, vmo, (long)b, 3 * PAGE, sizeof b)), "vmo_write");
    check(memcmp(map + 3 * PAGE, b, sizeof b) == 0, "a vmo_write shows in the mapping");

    // An offset maps from there on.
    char *second = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, PAGE);
    check(second != MAP_FAILED && memcmp(second + 8, a, sizeof a) == 0,
          "a mapping at an offset starts there");

    // A forked child's stores reach the parent: it is one VMO, not a copy.
    pid_t child = fork();
    if (child == 0) {
        map[2 * PAGE] = 'k';
        _exit(0);
    }
    int st;
    waitpid(child, &st, 0);
    check(WIFEXITED(st) && map[2 * PAGE] == 'k', "a forked child shares the mapping");

    // The window has to lie inside the VMO.
    errno = 0;
    check(mmap(NULL, 2 * PAGE, PROT_READ, MAP_SHARED, fd, (PAGES - 1) * PAGE) == MAP_FAILED &&
              errno == ENXIO,
          "a window past the end is ENXIO");
    errno = 0;
    check(mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE, fd, 0) == MAP_FAILED && errno == EINVAL,
          "MAP_PRIVATE is EINVAL");

    // A read-only handle: maps read-only, never writable.
    zx_handle_t ro_kept;
    int ro = vmo_fd(ZX_RIGHT_READ | ZX_RIGHT_MAP | ZX_RIGHT_TRANSFER, &ro_kept);
    errno = 0;
    check(mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, ro, 0) == MAP_FAILED &&
              errno == EACCES,
          "a writable mapping of a read-only handle is EACCES");
    char *rmap = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, ro, 0);
    check(rmap != MAP_FAILED, "a read-only mapping of a read-only handle");
    if (rmap != MAP_FAILED) {
        errno = 0;
        check(mprotect(rmap, PAGE, PROT_READ | PROT_WRITE) != 0 && errno == EACCES,
              "mprotect cannot raise it past the handle");
    }

    // A handle without MAP does not map at all.
    zx_handle_t nomap_kept;
    int nomap = vmo_fd(ZX_RIGHT_READ | ZX_RIGHT_TRANSFER, &nomap_kept);
    errno = 0;
    check(mmap(NULL, PAGE, PROT_READ, MAP_SHARED, nomap, 0) == MAP_FAILED && errno == EACCES,
          "a handle without MAP is EACCES");

    if (failures == 0)
        printf("zx-vmo-map: all checks passed\n");
    else
        printf("zx-vmo-map: %d checks failed\n", failures);
    return failures != 0;
}
