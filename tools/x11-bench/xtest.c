/*
 * x11-bench init: exercises, in one real zCore boot, the kernel surface an X
 * server (startx) relies on. Each check prints "XTEST: <name> ..."; the final
 * line reports how many passed. Run via tools/x11-bench/run.sh.
 */
#include <stdio.h>
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <pthread.h>
#include <time.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/select.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/time.h>
#include <linux/fb.h>
#include <linux/input.h>

static int passed = 0, total = 0;
static void check(const char *name, int ok, const char *detail) {
    total++; if (ok) passed++;
    printf("XTEST: [%s] %s %s\n", ok ? "PASS" : "FAIL", name, detail ? detail : "");
    fflush(stdout);
}

static volatile int alarm_fired = 0;
static void on_alarm(int s) { (void)s; alarm_fired = 1; }
static volatile int thread_ran = 0;
static void *thread_fn(void *a) { (void)a; thread_ran = 1; return 0; }

/* Bind/connect helper for both filesystem and abstract (leading-NUL) names. */
static socklen_t fill_addr(struct sockaddr_un *sa, const char *name, int abstract) {
    memset(sa, 0, sizeof *sa);
    sa->sun_family = AF_UNIX;
    if (abstract) {
        sa->sun_path[0] = 0;
        strcpy(sa->sun_path + 1, name);
        return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
    }
    strcpy(sa->sun_path, name);
    return sizeof *sa;
}

static void test_unix(const char *label, const char *name, int abstract) {
    struct sockaddr_un sa; socklen_t al = fill_addr(&sa, name, abstract);
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    int ok = bind(srv, (void *)&sa, al) == 0 && listen(srv, 5) == 0;
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    ok = ok && connect(c, (void *)&sa, al) == 0;
    ok = ok && write(c, "ABCD", 4) == 4;                 /* write before accept */
    struct pollfd p = { .fd = srv, .events = POLLIN };
    ok = ok && poll(&p, 1, 2000) == 1;                   /* X's accept loop wakes */
    int a = accept(srv, 0, 0);
    char b[8] = {0};
    ok = ok && a >= 0 && read(a, b, 4) == 4 && memcmp(b, "ABCD", 4) == 0;
    close(a); close(c); close(srv);
    check(label, ok, "");
}

int main(void) {
    printf("XTEST: start\n"); fflush(stdout);

    /* Display: framebuffer open + screeninfo + mmap + draw. */
    int fb = open("/dev/fb0", O_RDWR);
    if (fb >= 0) {
        struct fb_var_screeninfo vi; memset(&vi, 0, sizeof vi);
        int si = ioctl(fb, FBIOGET_VSCREENINFO, &vi) == 0 && vi.xres && vi.bits_per_pixel;
        check("fb screeninfo", si, "");
        size_t len = (size_t)vi.yres * vi.xres * (vi.bits_per_pixel ? vi.bits_per_pixel / 8 : 4);
        if (!len) len = 0x100000;
        void *m = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fb, 0);
        int mok = m != MAP_FAILED;
        if (mok) memset(m, 0x3c, 4096);
        check("fb mmap+draw", mok, "");
    } else check("fb open", 0, "/dev/fb0");

    /* Input: evdev probe (name + capability bitmap). */
    int ev = open("/dev/input/event0", O_RDONLY);
    if (ev >= 0) {
        char name[64] = {0};
        check("evdev EVIOCGNAME", ioctl(ev, EVIOCGNAME(sizeof name), name) >= 0, name);
        unsigned long bits = 0;
        check("evdev EVIOCGBIT", ioctl(ev, EVIOCGBIT(0, sizeof bits), &bits) >= 0 && bits != 0, "");
    } else check("evdev open", 0, "/dev/input/event0");

    /* TTY: TIOCGWINSZ on a pipe must succeed (X probes non-tty fds). */
    int pf[2];
    if (pipe(pf) == 0) {
        struct winsize ws; memset(&ws, 0, sizeof ws);
        check("TIOCGWINSZ on pipe", ioctl(pf[1], TIOCGWINSZ, &ws) == 0, "");
    }

    /* AF_UNIX: filesystem and abstract transports (Xlib uses abstract first). */
    test_unix("unix fs socket", "/tmp/xtest.sock", 0);
    test_unix("unix abstract socket", "/tmp/.X11-unix/X99", 1);

    /* Event loop: select / epoll / eventfd on a ready socket. */
    {
        struct sockaddr_un sa; socklen_t al = fill_addr(&sa, "/tmp/sel.sock", 0);
        int srv = socket(AF_UNIX, SOCK_STREAM, 0); bind(srv,(void*)&sa,al); listen(srv,5);
        int c = socket(AF_UNIX, SOCK_STREAM, 0); connect(c,(void*)&sa,al);
        fd_set r; FD_ZERO(&r); FD_SET(srv,&r);
        struct timeval tv = { 2, 0 };
        check("select", select(srv+1,&r,0,0,&tv) == 1 && FD_ISSET(srv,&r), "");
        int ep = epoll_create1(0);
        struct epoll_event eev = { .events = EPOLLIN, .data.fd = srv }, out[2];
        epoll_ctl(ep, EPOLL_CTL_ADD, srv, &eev);
        check("epoll", epoll_wait(ep, out, 2, 2000) == 1, "");
        close(ep); close(c); close(srv);
    }
    {
        int efd = eventfd(0, 0);
        uint64_t one = 1, got = 0;
        write(efd, &one, 8);
        struct pollfd p = { .fd = efd, .events = POLLIN };
        int ok = poll(&p, 1, 1000) == 1 && read(efd, &got, 8) == 8 && got == 1;
        check("eventfd", ok, ""); close(efd);
    }

    /* Scheduler: SIGALRM via setitimer. */
    {
        signal(SIGALRM, on_alarm);
        struct itimerval it; memset(&it, 0, sizeof it);
        it.it_value.tv_usec = 50000;
        setitimer(ITIMER_REAL, &it, 0);
        for (int i = 0; i < 400 && !alarm_fired; i++) { struct timespec t = {0, 1000000}; nanosleep(&t, 0); }
        check("SIGALRM/setitimer", alarm_fired, "");
    }

    /* Input thread: pthread_create + join. */
    {
        pthread_t th;
        int ok = pthread_create(&th, 0, thread_fn, 0) == 0 && pthread_join(th, 0) == 0 && thread_ran;
        check("pthread", ok, "");
    }

    /* socketpair (internal server pipes). */
    {
        int sp[2] = {-1,-1}; char b[4] = {0};
        int ok = socketpair(AF_UNIX, SOCK_STREAM, 0, sp) == 0 &&
                 write(sp[0], "AB", 2) == 2 && read(sp[1], b, 2) == 2 && memcmp(b,"AB",2)==0;
        check("socketpair", ok, "");
    }

    /* xauth: write temp then atomic rename onto the auth file. */
    {
        unlink("/root/.Xauth"); unlink("/root/.Xauth-n");
        int fd = open("/root/.Xauth-n", O_CREAT | O_WRONLY, 0600);
        int ok = fd >= 0;
        if (ok) { write(fd, "cookie", 6); close(fd); }
        ok = ok && rename("/root/.Xauth-n", "/root/.Xauth") == 0;
        check("xauth write+rename", ok, "");
    }

    printf("XTEST: %d/%d passed\n", passed, total); fflush(stdout);
    printf("XTEST: done\n"); fflush(stdout);
    return passed == total ? 0 : 1;
}
