#include <stdio.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <string.h>
#include <linux/fb.h>
#include <linux/input.h>

int main(void) {
    printf("XTEST: start\n"); fflush(stdout);

    /* 1) framebuffer */
    int fb = open("/dev/fb0", O_RDWR);
    printf("XTEST: open /dev/fb0 = %d\n", fb); fflush(stdout);
    if (fb >= 0) {
        struct fb_var_screeninfo vi; memset(&vi,0,sizeof vi);
        int r = ioctl(fb, FBIOGET_VSCREENINFO, &vi);
        printf("XTEST: FBIOGET_VSCREENINFO r=%d %ux%u bpp=%u\n",
               r, vi.xres, vi.yres, vi.bits_per_pixel); fflush(stdout);
        size_t len = (size_t)vi.yres * vi.xres * (vi.bits_per_pixel? vi.bits_per_pixel/8:4);
        if (!len) len = 0x100000;
        void *p = mmap(0, len, PROT_READ|PROT_WRITE, MAP_SHARED, fb, 0);
        printf("XTEST: mmap fb = %p\n", p); fflush(stdout);
        if (p != MAP_FAILED) { memset(p, 0x3c, 4096); printf("XTEST: fb write ok\n"); fflush(stdout);}
    }

    /* 2) evdev */
    int ev = open("/dev/input/event0", O_RDONLY);
    printf("XTEST: open event0 = %d\n", ev); fflush(stdout);
    if (ev >= 0) {
        char name[64] = {0};
        int r = ioctl(ev, EVIOCGNAME(sizeof name), name);
        printf("XTEST: EVIOCGNAME r=%d name='%s'\n", r, name); fflush(stdout);
        unsigned long evbits = 0;
        r = ioctl(ev, EVIOCGBIT(0, sizeof evbits), &evbits);
        printf("XTEST: EVIOCGBIT(0) r=%d bits=0x%lx\n", r, evbits); fflush(stdout);
    }

    /* 3) TIOCGWINSZ on a pipe */
    int pfd[2]; if (pipe(pfd)==0) {
        struct winsize ws; memset(&ws,0,sizeof ws);
        int r = ioctl(pfd[1], TIOCGWINSZ, &ws);
        printf("XTEST: TIOCGWINSZ pipe r=%d %ux%u\n", r, ws.ws_col, ws.ws_row); fflush(stdout);
    }

    /* 4) AF_UNIX handshake, single process: connect enqueues, write buffers, accept pops */
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa; memset(&sa,0,sizeof sa);
    sa.sun_family = AF_UNIX; strcpy(sa.sun_path, "/tmp/xtest.sock");
    int br = bind(srv,(void*)&sa,sizeof sa);
    int lr = listen(srv,5);
    printf("XTEST: bind=%d listen=%d\n", br, lr); fflush(stdout);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    int cr = connect(c,(void*)&sa,sizeof sa);
    int wr = write(c, "PING", 4);
    printf("XTEST: connect=%d write=%d (before accept)\n", cr, wr); fflush(stdout);
    int cli = accept(srv, 0, 0);
    char b[8]={0}; int n = read(cli, b, 4);
    printf("XTEST: accept=%d server read n=%d '%s'\n", cli, n, b); fflush(stdout);

    printf("XTEST: done\n"); fflush(stdout);
    return 0;
}
