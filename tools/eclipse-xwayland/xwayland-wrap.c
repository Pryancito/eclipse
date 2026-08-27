/*
 * Tiny WLR_XWAYLAND target: log argv, then exec the real Xwayland.
 *
 * Must NOT be a shell script — labwc/wlroots fork+exec this path, and ash
 * has SIGSEGV'd on this kernel in other spawn paths. Keep it a plain C
 * binary linked against musl.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static void log_line(int fd, const char *s) {
    if (fd < 0 || !s)
        return;
    (void)write(fd, s, strlen(s));
    (void)write(fd, "\n", 1);
}

int main(int argc, char **argv, char **envp) {
    int fd = open("/tmp/xwayland.log", O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (fd >= 0) {
        time_t now = time(NULL);
        char buf[256];
        int n = snprintf(buf, sizeof buf, "==== %ld pid=%d ====", (long)now,
                         (int)getpid());
        if (n > 0)
            log_line(fd, buf);
        log_line(fd, "exec /usr/bin/Xwayland");
        for (int i = 0; i < argc; i++) {
            n = snprintf(buf, sizeof buf, "  argv[%d]=%s", i,
                         argv[i] ? argv[i] : "(null)");
            if (n > 0)
                log_line(fd, buf);
        }
        close(fd);
    }

    /* Ensure socket dir exists (init may have wiped /tmp). */
    (void)mkdir("/tmp/.X11-unix", 01777);

    argv[0] = (char *)"/usr/bin/Xwayland";
    execve("/usr/bin/Xwayland", argv, envp);

    fd = open("/tmp/xwayland.log", O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (fd >= 0) {
        char buf[128];
        snprintf(buf, sizeof buf, "execve failed errno=%d", errno);
        log_line(fd, buf);
        close(fd);
    }
    return 127;
}
