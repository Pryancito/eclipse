#!/bin/bash
SYSROOT=$(pwd)/eclipse-os-build/sysroot
INCLUDE=$SYSROOT/usr/include

mkdir -p $INCLUDE/sys

# sys/types.h
cat > $INCLUDE/sys/types.h <<EOF
#ifndef _SYS_TYPES_H
#define _SYS_TYPES_H

#include <stddef.h>

typedef long off_t;
typedef long ssize_t;
typedef long fd_mask;
typedef struct { unsigned long sig[1]; } sigset_t;
typedef int pid_t;
typedef unsigned int uid_t;
typedef unsigned int gid_t;
typedef int key_t;
typedef unsigned int mode_t;
typedef char *caddr_t;
typedef unsigned long dev_t;
typedef unsigned long ino_t;
typedef unsigned int useconds_t;
typedef long suseconds_t;
typedef long time_t;
typedef long clock_t;
typedef int clockid_t;

typedef struct {
    unsigned long fds_bits[1024 / (8 * sizeof(long))];
} fd_set;

#define FD_SET(fd, set)   (((fd_set*)(set))->fds_bits[(fd) / (8 * sizeof(long))] |= (1UL << ((fd) % (8 * sizeof(long)))))
#define FD_CLR(fd, set)   (((fd_set*)(set))->fds_bits[(fd) / (8 * sizeof(long))] &= ~(1UL << ((fd) % (8 * sizeof(long)))))
#define FD_ISSET(fd, set) (((fd_set*)(set))->fds_bits[(fd) / (8 * sizeof(long))] & (1UL << ((fd) % (8 * sizeof(long)))))
#define FD_ZERO(set) memset((set), 0, sizeof(fd_set))

#endif
EOF

# sys/time.h
cat > $SYSROOT/usr/include/sys/time.h <<EOF
#ifndef _SYS_TIME_H
#define _SYS_TIME_H

#include <sys/types.h>

#ifndef _STRUCT_TIMEVAL
#define _STRUCT_TIMEVAL
struct timeval {
    time_t      tv_sec;
    suseconds_t tv_usec;
};
#endif

struct timezone {
    int tz_minuteswest;
    int tz_dsttime;
};

#define ITIMER_REAL    0
#define ITIMER_VIRTUAL 1
#define ITIMER_PROF    2

struct itimerval {
    struct timeval it_interval;
    struct timeval it_value;
};

int gettimeofday(struct timeval *tv, void *tz);
int setitimer(int which, const struct itimerval *new_value, struct itimerval *old_value);
int getitimer(int which, struct itimerval *curr_value);

#endif
EOF

# sys/param.h
cat > $SYSROOT/usr/include/sys/param.h <<EOF
#ifndef _SYS_PARAM_H
#define _SYS_PARAM_H

#define MAXPATHLEN 4096
#define PATH_MAX 4096

#ifndef MIN
#define MIN(a,b) (((a)<(b))?(a):(b))
#endif
#ifndef MAX
#define MAX(a,b) (((a)>(b))?(a):(b))
#endif

#endif
EOF

# sys/mman.h
cat > $INCLUDE/sys/mman.h <<EOF
#ifndef _SYS_MMAN_H
#define _SYS_MMAN_H

#include <sys/types.h>

#define PROT_READ 0x1
#define PROT_WRITE 0x2
#define PROT_EXEC 0x4
#define PROT_NONE 0x0

#define MAP_SHARED 0x01
#define MAP_PRIVATE 0x02
#define MAP_FIXED 0x10
#define MAP_ANONYMOUS 0x20

#define MAP_FAILED ((void *) -1)

void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset);
int munmap(void *addr, size_t length);

#endif
EOF

# stdint.h
cat > $INCLUDE/stdint.h <<EOF
#ifndef _STDINT_H
#define _STDINT_H

typedef signed char int8_t;
typedef short int16_t;
typedef int int32_t;
typedef long int64_t;

typedef unsigned char uint8_t;
typedef unsigned short uint16_t;
typedef unsigned int uint32_t;
typedef unsigned long uint64_t;

typedef long intptr_t;
typedef unsigned long uintptr_t;

typedef long intmax_t;
typedef unsigned long uintmax_t;

#define INT8_MIN   (-128)
#define INT8_MAX   127
#define UINT8_MAX  0xff

#define INT16_MIN  (-32768)
#define INT16_MAX  32767
#define UINT16_MAX 0xffff

#define INT32_MIN  (-2147483648)
#define INT32_MAX  2147483647
#define UINT32_MAX 0xffffffffU

#define INT64_MIN  (-9223372036854775808L)
#define INT64_MAX  9223372036854775807L
#define UINT64_MAX 0xffffffffffffffffUL

#define SIZE_MAX   UINT64_MAX

#endif
EOF

# math.h
cat > $INCLUDE/math.h <<EOF
#ifndef _MATH_H
#define _MATH_H

#define HUGE_VAL (__builtin_huge_val())

#define M_E        2.7182818284590452354
#define M_LOG2E    1.4426950408889634074
#define M_LOG10E   0.43429448190325182765
#define M_LN2      0.69314718055994530942
#define M_LN10     2.30258509299404568402
#define M_PI       3.14159265358979323846
#define M_PI_2     1.57079632679489661923
#define M_PI_4     0.78539816339744830962
#define M_1_PI     0.31830988618379067154
#define M_2_PI     0.63661977236758134308
#define M_2_SQRTPI 1.12837916709551257390
#define M_SQRT2    1.41421356237309504880
#define M_SQRT1_2  0.70710678118654752440

double hypot(double x, double y);
double sqrt(double x);
double sin(double x);
double cos(double x);
double tan(double x);
double pow(double x, double y);
double fabs(double x);
double floor(double x);
double ceil(double x);
double atan2(double y, double x);
double exp(double x);
double log(double x);
double log10(double x);
double fmod(double x, double y);

#endif
EOF

# endian.h
cat > $INCLUDE/endian.h <<EOF
#ifndef _ENDIAN_H
#define _ENDIAN_H

#define __LITTLE_ENDIAN 1234
#define __BIG_ENDIAN    4321
#define __PDP_ENDIAN    3412

#define __BYTE_ORDER __LITTLE_ENDIAN

#define LITTLE_ENDIAN __LITTLE_ENDIAN
#define BIG_ENDIAN    __BIG_ENDIAN
#define PDP_ENDIAN    __PDP_ENDIAN
#define BYTE_ORDER    __BYTE_ORDER

#endif
EOF

# sys/endian.h (alias)
mkdir -p $INCLUDE/sys
cp $INCLUDE/endian.h $INCLUDE/sys/endian.h

# locale.h
cat > $INCLUDE/locale.h <<EOF
#ifndef _LOCALE_H
#define _LOCALE_H

#define LC_CTYPE    0
#define LC_NUMERIC  1
#define LC_TIME     2
#define LC_COLLATE  3
#define LC_MONETARY 4
#define LC_MESSAGES 5
#define LC_ALL      6

struct lconv {
    char *decimal_point;
    char *thousands_sep;
    char *grouping;
    char *int_curr_symbol;
    char *currency_symbol;
    char *mon_decimal_point;
    char *mon_thousands_sep;
    char *mon_grouping;
    char *positive_sign;
    char *negative_sign;
    char int_frac_digits;
    char frac_digits;
    char p_cs_precedes;
    char p_sep_by_space;
    char n_cs_precedes;
    char n_sep_by_space;
    char p_sign_posn;
    char n_sign_posn;
};

char *setlocale(int category, const char *locale);
struct lconv *localeconv(void);

#endif
EOF

# netinet/in.h
mkdir -p $INCLUDE/netinet
cat > $INCLUDE/netinet/in.h <<INNER
#ifndef _NETINET_IN_H
#define _NETINET_IN_H

#include <stdint.h>
#include <sys/types.h>

typedef uint32_t in_addr_t;
typedef uint16_t in_port_t;

struct in_addr {
    in_addr_t s_addr;
};

struct sockaddr_in {
    unsigned short sin_family;
    in_port_t      sin_port;
    struct in_addr sin_addr;
    char           sin_zero[8];
};

#define INADDR_ANY  ((in_addr_t) 0x00000000)
#define INADDR_NONE ((in_addr_t) 0xffffffff)

#endif
INNER

# arpa/inet.h
mkdir -p $INCLUDE/arpa
cat > $INCLUDE/arpa/inet.h <<EOF
#ifndef _ARPA_INET_H
#define _ARPA_INET_H

#include <netinet/in.h>

char *inet_ntoa(struct in_addr in);

#endif
EOF

# time.h
cat > $INCLUDE/time.h <<INNER
#ifndef _TIME_H
#define _TIME_H

#include <sys/types.h>

struct timespec {
    time_t tv_sec;
    long   tv_nsec;
};

struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
};

time_t time(time_t *tloc);
char *ctime(const time_t *timep);
double difftime(time_t time1, time_t time0);
time_t mktime(struct tm *tm);
size_t strftime(char *s, size_t max, const char *format, const struct tm *tm);

#define CLOCK_REALTIME           0
#define CLOCK_MONOTONIC          1
#define CLOCK_PROCESS_CPUTIME_ID 2
#define CLOCK_THREAD_CPUTIME_ID  3

int clock_gettime(clockid_t clk_id, struct timespec *tp);

#endif
INNER

# netinet/tcp.h
cat > $INCLUDE/netinet/tcp.h <<EOF
#ifndef _NETINET_TCP_H
#define _NETINET_TCP_H
#endif
EOF

# signal.h
cat > $INCLUDE/signal.h <<INNER
#ifndef _SIGNAL_H
#define _SIGNAL_H

#include <sys/types.h>

typedef void (*sighandler_t)(int);

#define SIG_DFL ((sighandler_t)0)
#define SIG_IGN ((sighandler_t)1)
#define SIG_ERR ((sighandler_t)-1)

struct sigaction {
    sighandler_t sa_handler;
    sigset_t     sa_mask;
    int          sa_flags;
    void       (*sa_restorer)(void);
};

#define SIGHUP  1
#define SIGINT  2
#define SIGQUIT 3
#define SIGILL  4
#define SIGTRAP 5
#define SIGABRT 6
#define SIGBUS  7
#define SIGFPE  8
#define SIGKILL 9
#define SIGUSR1 10
#define SIGSEGV 11
#define SIGUSR2 12
#define SIGPIPE 13
#define SIGALRM 14
#define SIGTERM 15
#define SIGCHLD 17
#define SIGCONT 18
#define SIGSTOP 19
#define SIGTSTP 20
#define SIGTTIN 21
#define SIGTTOU 22
#define SIGIO   29
#define SIGVTALRM 26

#define SIG_BLOCK   0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2

int kill(pid_t pid, int sig);
sighandler_t signal(int sig, sighandler_t handler);
int sigprocmask(int how, const sigset_t *set, sigset_t *oldset);
int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact);
int sigemptyset(sigset_t *set);
int sigfillset(sigset_t *set);
int sigaddset(sigset_t *set, int signum);
int sigdelset(sigset_t *set, int signum);
int sigismember(const sigset_t *set, int signum);

#endif
INNER

# limits.h
cat > $INCLUDE/limits.h <<INNER
#ifndef _LIMITS_H
#define _LIMITS_H

#define OPEN_MAX 256
#define NOFILES_MAX 256
#define NAME_MAX 255
#define PATH_MAX 4096

#endif
INNER

# sys/select.h
mkdir -p $INCLUDE/sys
cat > $INCLUDE/sys/select.h <<INNER
#ifndef _SYS_SELECT_H
#define _SYS_SELECT_H

#include <sys/types.h>
#include <string.h>

int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds, struct timeval *timeout);

#endif
INNER

# more X11 extensions
cat > $INCLUDE/X11/extensions/XTest.h <<INNER
#ifndef _XTEST_H_
#define _XTEST_H_
#endif
INNER

cat > $INCLUDE/X11/extensions/Xinerama.h <<INNER
#ifndef _XINERAMA_H_
#define _XINERAMA_H_
#endif
INNER

# termios.h
cat > $INCLUDE/termios.h <<INNER
#ifndef _TERMIOS_H
#define _TERMIOS_H
#include <sys/types.h>
typedef unsigned int tcflag_t;
typedef unsigned char cc_t;
typedef unsigned int speed_t;
struct termios {
    tcflag_t c_iflag;
    tcflag_t c_oflag;
    tcflag_t c_cflag;
    tcflag_t c_lflag;
    cc_t c_line;
    cc_t c_cc[32];
    speed_t c_ispeed;
    speed_t c_ospeed;
};

#define VTIME   5
#define VMIN    6

#define IGNBRK  0000001
#define BRKINT  0000002
#define IGNPAR  0000004
#define PARMRK  0000010
#define INPCK   0000020
#define ISTRIP  0000040
#define INLCR   0000100
#define IGNCR   0000200
#define ICRNL   0000400
#define IXON    0002000
#define IXANY   0004000
#define IXOFF   0010000

#define CS7     0000040
#define CS8     0000060
#define CSTOPB  0000100

#define CREAD   0000200
#define HUPCL   0002000
#define CLOCAL  0004000

#define ISIG    0000001
#define ICANON  0000002
#define ECHO    0000010
#define ECHOE   0000020
#define ECHOK   0000040
#define ECHONL  0000100
#define NOFLSH  0000200
#define TOSTOP  0000400
#define IEXTEN  0100000

#define B1200   0000011
#define B9600   0000015

int tcgetattr(int fd, struct termios *termios_p);
int tcsetattr(int fd, int optional_actions, const struct termios *termios_p);
int cfsetispeed(struct termios *termios_p, speed_t speed);
int cfsetospeed(struct termios *termios_p, speed_t speed);

#define TCSANOW 0
#endif
INNER

# linux/vt.h
mkdir -p $INCLUDE/linux
cat > $INCLUDE/linux/vt.h <<INNER
#ifndef _LINUX_VT_H
#define _LINUX_VT_H
struct vt_stat {
    unsigned short v_active;
    unsigned short v_signal;
    unsigned short v_state;
};
struct vt_mode {
    char mode;
    char waitv;
    short relsig;
    short acqsig;
    short frsig;
};
#define VT_GETSTATE 0x5603
#define VT_OPENQRY  0x5600
#define VT_ACTIVATE 0x5606
#define VT_WAITACTIVE 0x5607
#define VT_GETMODE  0x5601
#define VT_SETMODE  0x5602
#define VT_AUTO     0x00
#define VT_DISALLOCATE 0x5608
#define VT_RELDISP  0x5604
#define VT_ACKACQ   0x01
#define VT_PROCESS  0x01
#endif
INNER

# linux/apm_bios.h
cat > $INCLUDE/linux/apm_bios.h <<INNER
#ifndef _LINUX_APM_BIOS_H
#define _LINUX_APM_BIOS_H
typedef unsigned short apm_event_t;
#define APM_IOC_STANDBY 0x01
#define APM_IOC_SUSPEND 0x02
#define APM_STANDBY_RESUME 0x0001
#define APM_USER_SUSPEND 0x0002
#define APM_USER_STANDBY 0x0003
#define APM_NORMAL_RESUME 0x0004
#define APM_CRITICAL_RESUME 0x0005
#define APM_CRITICAL_SUSPEND 0x0006
#define APM_SYS_SUSPEND 0x0007
#define APM_SYS_STANDBY 0x0008
#endif
INNER

# linux/keyboard.h
cat > $INCLUDE/linux/keyboard.h <<INNER
#ifndef _LINUX_KEYBOARD_H
#define _LINUX_KEYBOARD_H
struct kbentry {
    unsigned char kb_table;
    unsigned char kb_index;
    unsigned short kb_value;
};
#define KDGKBENT 0x4B46
#define KDGKBTYPE 0x4B33
#define KB_101    0x0002
#define NR_KEYS   128
#define KTYP(x)   ((x) >> 8)
#define KVAL(x)   ((x) & 0xFF)

#define KT_LATIN  0x00
#define KT_LETTER 0x0b
#define KT_FN     0x01
#define KT_SPEC   0x03
#define KT_PAD    0x04
#define KT_DEAD   0x05
#define KT_SHIFT  0x06
#define KT_META   0x07
#define KT_ASCII  0x0f
#define KT_LOCK   0x02
#define KT_CUR    0x0a
#define KT_X      0x0c
#define KT_XF     0x0d

#define K_SHIFT   0x00
#define K_SHIFTL  0x0b
#define K_SHIFTR  0x0c
#define K_SHIFTLOCK 0x0d
#define K_CAPS    0x0e
#define K_ALT     0x02
#define K_ALTGR   0x01
#define K_CTRL    0x03
#define K_CTRLL   0x04
#define K_CTRLR   0x05
#define K_UP      0x01
#define K_DOWN    0x02
#define K_LEFT    0x03
#define K_RIGHT   0x04
#define K_FIND    0x01
#define K_INSERT  0x02
#define K_REMOVE  0x03
#define K_SELECT  0x04
#define K_PGUP    0x05
#define K_PGDN    0x06
#define K_HELP    0x07
#define K_DO      0x08
#define K_PAUSE   0x09
#define K_MACRO   0x0a
#define K_ENTER   0x0b
#define K_BREAK   0x0c
#define K_NUM     0x48

#define KG_SHIFT  0
#define KG_ALTGR  1

/* Mocked constants for X11 mapping in keyboard.c */
#define K_PUBLISHING 0x1e
#define K_Agrave 0x20
#define K_Aacute 0x21
#define K_Acircumflex 0x22
#define K_Atilde 0x23
#define K_Adiaeresis 0x24
#define K_Aring 0x25
#define K_AE 0x26
#define K_Ccedilla 0x27
#define K_Egrave 0x28
#define K_Eacute 0x29
#define K_Ecircumflex 0x2a
#define K_Ediaeresis 0x2b
#define K_Igrave 0x2c
#define K_Iacute 0x2d
#define K_Icircumflex 0x2e
#define K_Idiaeresis 0x2f
#define K_ETH 0x30
#define K_Ntilde 0x31
#define K_Ograve 0x32
#define K_Oacute 0x33
#define K_Ocircumflex 0x34
#define K_Otilde 0x35
#define K_Odiaeresis 0x36
#define K_XMENU 0x37
#define K_XTELEPHONE 0x38
#define K_HOLD 0x39
#define K_COMPOSE 0x3a
#define K_DACUTE 0x3b
#define K_DCIRCM 0x3c
#define K_DDIERE 0x3d
#define K_DGRAVE 0x3e
#define K_DTILDE 0x3f

#define K_PSTAR 0x40
#define K_PSLASH 0x41
#define K_PENTER 0x42
#define K_PCOMMA 0x43
#define K_PDOT 0x44
#define K_PPLUSMINUS 0x45
#define K_PMINUS 0x46
#define K_PPLUS 0x47

#endif
INNER

# linux/kd.h (needed by linux backend)
cat > $INCLUDE/linux/kd.h <<INNER
#ifndef _LINUX_KD_H
#define _LINUX_KD_H
#define KDSETMODE 0x4B3A
#define KD_TEXT   0x00
#define KD_GRAPHICS 0x01
#define KDGKBMODE 0x4B44
#define KDSKBMODE 0x4B45
#define K_RAW     0x00
#define K_MEDIUMRAW 0x02
#define KDSETLED  0x4B32
#define KDMKTONE  0x4B30
#endif
INNER

# more X11 extension headers
cat > $INCLUDE/X11/extensions/XTest.h <<INNER
#ifndef _XTEST_H_
#define _XTEST_H_
#endif
INNER

cat > $INCLUDE/X11/extensions/Xinerama.h <<INNER
#ifndef _XINERAMA_H_
#define _XINERAMA_H_
#endif
INNER

# unistd.h
cat > $INCLUDE/unistd.h <<INNER
#ifndef _UNISTD_H
#define _UNISTD_H

#include <stddef.h>
#include <sys/types.h>

#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

#define R_OK 4
#define W_OK 2
#define X_OK 1
#define F_OK 0

#define _SC_ARG_MAX        0
#define _SC_CHILD_MAX      1
#define _SC_CLK_TCK        2
#define _SC_NGROUPS_MAX    3
#define _SC_OPEN_MAX       4
#define _SC_STREAM_MAX     5
#define _SC_TZNAME_MAX     6
#define _SC_JOB_CONTROL    7
#define _SC_SAVED_IDS      8
#define _SC_REALTIME_SIGNALS 9
#define _SC_PRIORITY_SCHEDULING 10
#define _SC_TIMERS         11
#define _SC_ASYNCHRONOUS_IO 12
#define _SC_PRIORITIZED_IO 13
#define _SC_SYNCHRONIZED_IO 14
#define _SC_FSYNC          15
#define _SC_MAPPED_FILES   16
#define _SC_MEMLOCK        17
#define _SC_MEMLOCK_RANGE  18
#define _SC_MEMORY_PROTECTION 19
#define _SC_MESSAGE_PASSING 20
#define _SC_SEMAPHORES     21
#define _SC_SHARED_MEMORY_OBJECTS 22
#define _SC_AIO_LISTIO_MAX 23
#define _SC_AIO_MAX        24
#define _SC_AIO_PRIO_DELTA_MAX 25
#define _SC_DELAYTIMER_MAX 26
#define _SC_MQ_OPEN_MAX    27
#define _SC_MQ_PRIO_MAX    28
#define _SC_VERSION        29
#define _SC_PAGESIZE       30
#define _SC_PAGE_SIZE      _SC_PAGESIZE

ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
int close(int fd);
off_t lseek(int fd, off_t offset, int whence);

int unlink(const char *pathname);
int rmdir(const char *pathname);
int chdir(const char *path);
char *getcwd(char *buf, size_t size);
int access(const char *pathname, int mode);

pid_t getpid(void);
pid_t getppid(void);
uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);
gid_t getegid(void);
int setuid(uid_t uid);
int seteuid(uid_t euid);
int setgid(gid_t gid);
int setegid(gid_t egid);

unsigned int sleep(unsigned int seconds);
int usleep(useconds_t usec);
int getdtablesize(void);
long sysconf(int name);
int open(const char *pathname, int flags, ...);
int link(const char *oldpath, const char *newpath);
int gethostname(char *name, size_t len);
void _exit(int status);

int isatty(int fd);
int chown(const char *path, uid_t owner, gid_t group);
int fchown(int fd, uid_t owner, gid_t group);
int pipe(int pipefd[2]);
int dup(int oldfd);
int dup2(int oldfd, int newfd);

int fork(void);
int execv(const char *path, char *const argv[]);
int execve(const char *pathname, char *const argv[], char *const envp[]);
int execvp(const char *file, char *const argv[]);
int execl(const char *path, const char *arg, ...);

pid_t getpgrp(void);
int setpgid(pid_t pid, pid_t pgid);

#endif
INNER

# linux/fb.h
cat > $INCLUDE/linux/fb.h <<INNER
#ifndef _LINUX_FB_H
#define _LINUX_FB_H

typedef unsigned short __u16;
typedef unsigned int __u32;

struct fb_bitfield {
    __u32 offset;
    __u32 length;
    __u32 msb_right;
};

struct fb_var_screeninfo {
    __u32 xres;
    __u32 yres;
    __u32 xres_virtual;
    __u32 yres_virtual;
    __u32 xoffset;
    __u32 yoffset;
    __u32 bits_per_pixel;
    __u32 grayscale;
    struct fb_bitfield red;
    struct fb_bitfield green;
    struct fb_bitfield blue;
    struct fb_bitfield transp;
    __u32 nonstd;
    __u32 activate;
    __u32 height;
    __u32 width;
    __u32 accel_flags;
    __u32 pixclock;
    __u32 left_margin;
    __u32 right_margin;
    __u32 upper_margin;
    __u32 lower_margin;
    __u32 hsync_len;
    __u32 vsync_len;
    __u32 sync;
    __u32 vmode;
    __u32 rotate;
    __u32 reserved[5];
};

struct fb_fix_screeninfo {
    char id[16];
    unsigned long smem_start;
    __u32 smem_len;
    __u32 type;
    __u32 type_aux;
    __u32 visual;
    unsigned short xpanstep;
    unsigned short ypanstep;
    unsigned short ywrapstep;
    __u32 line_length;
    unsigned long mmio_start;
    __u32 mmio_len;
    __u32 accel;
    unsigned short reserved[3];
};

struct fb_cmap {
    __u32 start;
    __u32 len;
    __u16 *red;
    __u16 *green;
    __u16 *blue;
    __u16 *transp;
};

#define FBIOGET_VSCREENINFO 0x4600
#define FBIOPUT_VSCREENINFO 0x4601
#define FBIOGET_FSCREENINFO 0x4602
#define FBIOPUTCMAP         0x4604
#define FBIOGETCMAP         0x4605

#define FB_TYPE_PACKED_PIXELS 0

#define FB_VISUAL_TRUECOLOR 2
#define FB_VISUAL_PSEUDOCOLOR 3
#define FB_VISUAL_STATIC_PSEUDOCOLOR 4
#define FB_VISUAL_DIRECTCOLOR 5

#define FB_ACTIVATE_NOW 0
#define FB_CHANGE_CMAP_VBL 1

#define FB_SYNC_HOR_HIGH_ACT 1
#define FB_SYNC_VERT_HIGH_ACT 2

#endif
INNER

# more X11 extensions
cat > $INCLUDE/X11/extensions/dpms.h <<INNER
#ifndef _DPMS_H_
#define _DPMS_H_
#define DPMSModeOn      0
#define DPMSModeStandby 1
#define DPMSModeSuspend 2
#define DPMSModeOff     3
#endif
INNER

cat > $INCLUDE/X11/extensions/Xinerama.h <<INNER
#ifndef _XINERAMA_H_
#define _XINERAMA_H_
#endif
INNER

# stdio.h
cat > $INCLUDE/stdio.h <<INNER
#ifndef _STDIO_H
#define _STDIO_H

#include <sys/types.h>
#include <stdarg.h>
#include <stddef.h>

#define NULL ((void*)0)
#define EOF (-1)

typedef struct FILE FILE;

extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;

int printf(const char *format, ...);
int fprintf(FILE *stream, const char *format, ...);
int sprintf(char *str, const char *format, ...);
int snprintf(char *str, size_t size, const char *format, ...);
int vsnprintf(char *str, size_t size, const char *format, va_list ap);
int vfprintf(FILE *stream, const char *format, va_list ap);

FILE *fopen(const char *pathname, const char *mode);
int fclose(FILE *stream);
size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);
int fseek(FILE *stream, long offset, int whence);
long ftell(FILE *stream);
void rewind(FILE *stream);
int vfscanf(FILE *stream, const char *format, va_list ap);
int vscanf(const char *format, va_list ap);
int vsscanf(const char *str, const char *format, va_list ap);
int fscanf(FILE *stream, const char *format, ...);
int scanf(const char *format, ...);
int sscanf(const char *str, const char *format, ...);

int fputc(int c, FILE *stream);
int fputs(const char *s, FILE *stream);
int putc(int c, FILE *stream);
int putchar(int c);
int puts(const char *s);
int ungetc(int c, FILE *stream);
int fileno(FILE *stream);
FILE *fdopen(int fd, const char *mode);
int fgetc(FILE *stream);
char *fgets(char *s, int size, FILE *stream);
int getc(FILE *stream);
int getchar(void);
int fflush(FILE *stream);
int remove(const char *pathname);
int rename(const char *oldpath, const char *newpath);
void perror(const char *s);

#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2

#endif
INNER

# string.h
cat > $INCLUDE/string.h <<INNER
#ifndef _STRING_H
#define _STRING_H

#include <stddef.h>

void *memcpy(void *dest, const void *src, size_t n);
void *memmove(void *dest, const void *src, size_t n);
void *memset(void *s, int c, size_t n);
int memcmp(const void *s1, const void *s2, size_t n);
void *memchr(const void *s, int c, size_t n);

size_t strlen(const char *s);
size_t strnlen(const char *s, size_t maxlen);
char *strcpy(char *dest, const char *src);
char *strncpy(char *dest, const char *src, size_t n);
char *strcat(char *dest, const char *src);
char *strncat(char *dest, const char *src, size_t n);
int strcmp(const char *s1, const char *s2);
int strncmp(const char *s1, const char *s2, size_t n);
char *strchr(const char *s, int c);
char *strrchr(const char *s, int c);
char *strstr(const char *haystack, const char *needle);
char *strdup(const char *s);
char *strerror(int errnum);
size_t strcspn(const char *s, const char *reject);
size_t strspn(const char *s, const char *accept);
char *strpbrk(const char *s, const char *accept);
char *strtok(char *str, const char *delim);
int strcasecmp(const char *s1, const char *s2);
int strncasecmp(const char *s1, const char *s2, size_t n);
#define bzero(s, n) memset(s, 0, n)

#endif
INNER

# strings.h
cat > $INCLUDE/strings.h <<INNER
#ifndef _STRINGS_H
#define _STRINGS_H
#include <string.h>
#endif
INNER

# errno.h
cat > $INCLUDE/errno.h <<INNER
#ifndef _ERRNO_H
#define _ERRNO_H

extern int *__errno_location(void);
#define errno (*__errno_location())

#define EPERM 1
#define ENOENT 2
#define ESRCH 3
#define EINTR 4
#define EIO 5
#define ENXIO 6
#define E2BIG 7
#define ENOEXEC 8
#define EBADF 9
#define ECHILD 10
#define EAGAIN 11
#define ENOMEM 12
#define EACCES 13
#define EFAULT 14
#define ENOTBLK 15
#define EBUSY 16
#define EEXIST 17
#define EXDEV 18
#define ENODEV 19
#define ENOTDIR 20
#define EISDIR 21
#define EINVAL 22
#define ENFILE 23
#define EMFILE 24
#define ENOTTY 25
#define ETXTBSY 26
#define EFBIG 27
#define ENOSPC 28
#define ESPIPE 29
#define EROFS 30
#define EMLINK 31
#define EPIPE 32
#define EDOM 33
#define ERANGE 34

#define EAFNOSUPPORT 97
#define EADDRINUSE 98
#define EADDRNOTAVAIL 99
#define EISCONN 106
#define ETIMEDOUT 110
#define ECONNREFUSED 111
#define ENETUNREACH 101
#define EHOSTUNREACH 113
#define ENOPROTOOPT 92
#define EINPROGRESS 115
#define EPROTOTYPE 91

#define EWOULDBLOCK EAGAIN

#endif
INNER

# sys/socket.h
cat > $INCLUDE/sys/socket.h <<INNER
#ifndef _SYS_SOCKET_H
#define _SYS_SOCKET_H

#include <sys/types.h>

#define AF_UNSPEC   0
#define AF_UNIX     1
#define AF_INET     2
#define PF_UNIX     AF_UNIX
#define PF_INET     AF_INET
#define SOCK_STREAM 1
#define SOCK_DGRAM  2

#define SOL_SOCKET  1
#define SO_KEEPALIVE 9

struct sockaddr {
    unsigned short sa_family;
    char           sa_data[14];
};

typedef unsigned int socklen_t;

int socket(int domain, int type, int protocol);
int bind(int sockfd, const struct sockaddr *addr, socklen_t addrlen);
int listen(int sockfd, int backlog);
int accept(int sockfd, struct sockaddr *addr, socklen_t *addrlen);
int connect(int sockfd, const struct sockaddr *addr, socklen_t addrlen);
int shutdown(int sockfd, int how);
int send(int sockfd, const void *buf, size_t len, int flags);
int recv(int sockfd, void *buf, size_t len, int flags);
int getsockname(int sockfd, struct sockaddr *addr, socklen_t *addrlen);
int getpeername(int sockfd, struct sockaddr *addr, socklen_t *addrlen);
int setsockopt(int sockfd, int level, int optname, const void *optval, socklen_t optlen);
int getsockopt(int sockfd, int level, int optname, void *optval, socklen_t *optlen);

#endif
INNER

# stdlib.h
cat > $INCLUDE/stdlib.h <<INNER
#ifndef _STDLIB_H
#define _STDLIB_H

#include <stddef.h>
#define RAND_MAX 2147483647
#include <sys/types.h>

#define NULL ((void*)0)

void *malloc(size_t size);
void free(void *ptr);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);

void exit(int status);
void abort(void);

int atoi(const char *nptr);
long atol(const char *nptr);
long long atoll(const char *nptr);
long strtol(const char *nptr, char **endptr, int base);
unsigned long strtoul(const char *nptr, char **endptr, int base);

char *getenv(const char *name);
int setenv(const char *name, const char *value, int overwrite);
unsigned long long strtoull(const char *nptr, char **endptr, int base);
double strtod(const char *nptr, char **endptr);
float strtof(const char *nptr, char **endptr);
int unsetenv(const char *name);

int abs(int j);
long labs(long j);
long long llabs(long long j);

int rand(void);
void srand(unsigned int seed);
double atof(const char *nptr);
void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));
char *realpath(const char *path, char *resolved_path);
int system(const char *command);

#endif
INNER

# ctype.h
cat > $INCLUDE/ctype.h <<INNER
#ifndef _CTYPE_H
#define _CTYPE_H

int isalnum(int c);
int isalpha(int c);
int iscntrl(int c);
int isdigit(int c);
int isgraph(int c);
int islower(int c);
int isprint(int c);
int ispunct(int c);
int isspace(int c);
int isupper(int c);
int isxdigit(int c);
int tolower(int c);
int toupper(int c);

#endif
INNER

# fcntl.h
cat > $INCLUDE/fcntl.h <<INNER
#ifndef _FCNTL_H
#define _FCNTL_H

#include <sys/types.h>

#define O_RDONLY    0x0000
#define O_WRONLY    0x0001
#define O_RDWR      0x0002
#define O_CREAT     0x0040
#define O_EXCL      0x0080
#define O_NOCTTY    0x0100
#define O_TRUNC     0x0200
#define O_APPEND    0x0400
#define O_NONBLOCK  0x0800
#define O_CLOEXEC   0x80000

#define F_DUPFD     0
#define F_GETFD     1
#define F_SETFD     2
#define F_GETFL     3
#define F_SETFL     4

#define FD_CLOEXEC  1

int fcntl(int fd, int cmd, ...);
int open(const char *pathname, int flags, ...);
int creat(const char *pathname, mode_t mode);

#endif
INNER

# fcntl.h update
cat > $INCLUDE/fcntl.h <<INNER
#ifndef _FCNTL_H
#define _FCNTL_H

#include <sys/types.h>

#define O_RDONLY    0x0000
#define O_WRONLY    0x0001
#define O_RDWR      0x0002
#define O_CREAT     0x0040
#define O_EXCL      0x0080
#define O_NOCTTY    0x0100
#define O_TRUNC     0x0200
#define O_APPEND    0x0400
#define O_NONBLOCK  0x0800
#define O_CLOEXEC   0x80000
#define O_NOFOLLOW  0x20000
#define O_DIRECTORY 0x10000

#define F_DUPFD     0
#define F_GETFD     1
#define F_SETFD     2
#define F_GETFL     3
#define F_SETFL     4

#define FD_CLOEXEC  1

int fcntl(int fd, int cmd, ...);
int open(const char *pathname, int flags, ...);
int creat(const char *pathname, mode_t mode);

#endif
INNER

# arpa/inet.h
cat > $INCLUDE/arpa/inet.h <<INNER
#ifndef _ARPA_INET_H
#define _ARPA_INET_H

#include <netinet/in.h>

uint32_t htonl(uint32_t hostlong);
uint16_t htons(uint16_t hostshort);
uint32_t ntohl(uint32_t netlong);
uint16_t ntohs(uint16_t netshort);

in_addr_t inet_addr(const char *cp);
char *inet_ntoa(struct in_addr in);

#endif
INNER



# ifaddrs.h
cat > $INCLUDE/ifaddrs.h <<INNER
#ifndef _IFADDRS_H
#define _IFADDRS_H

#include <sys/socket.h>

struct ifaddrs {
    struct ifaddrs  *ifa_next;
    char            *ifa_name;
    unsigned int     ifa_flags;
    struct sockaddr *ifa_addr;
    struct sockaddr *ifa_netmask;
    union {
        struct sockaddr *ifu_broadaddr;
        struct sockaddr *ifu_dstaddr;
    } ifa_ifu;
#define ifa_broadaddr ifa_ifu.ifu_broadaddr
#define ifa_dstaddr   ifa_ifu.ifu_dstaddr
    void            *ifa_data;
};

int getifaddrs(struct ifaddrs **ifap);
void freeifaddrs(struct ifaddrs *ifa);

#endif
INNER

# net/if.h cleanup and IFF flags
cat > $INCLUDE/net/if.h <<INNER
#ifndef _NET_IF_H
#define _NET_IF_H

#include <sys/socket.h>

#define IFF_UP          0x1
#define IFF_BROADCAST   0x2
#define IFF_DEBUG       0x4
#define IFF_LOOPBACK    0x8
#define IFF_POINTOPOINT 0x10
#define IFF_NOTRAILERS  0x20
#define IFF_RUNNING     0x40
#define IFF_NOARP       0x80
#define IFF_PROMISC     0x100

struct ifreq {
    char ifr_name[16];
    union {
        struct sockaddr ifru_addr;
        struct sockaddr ifru_dstaddr;
        struct sockaddr ifru_broadaddr;
        struct sockaddr ifru_netmask;
        struct sockaddr ifru_hwaddr;
        short           ifru_flags;
        int             ifru_ivalue;
        int             ifru_mtu;
    } ifr_ifru;
};

#define ifr_addr      ifr_ifru.ifru_addr
#define ifr_dstaddr   ifr_ifru.ifru_dstaddr
#define ifr_broadaddr ifr_ifru.ifru_broadaddr
#define ifr_netmask   ifr_ifru.ifru_netmask
#define ifr_hwaddr    ifr_ifru.ifru_hwaddr
#define ifr_flags     ifr_ifru.ifru_flags
#define ifr_ifindex   ifr_ifru.ifru_ivalue
#define ifr_metric    ifr_ifru.ifru_ivalue
#define ifr_mtu       ifr_ifru.ifru_mtu

struct ifconf {
    int ifc_len;
    union {
        char *ifcu_buf;
        struct ifreq *ifcu_req;
    } ifc_ifcu;
};

#define ifc_buf ifc_ifcu.ifcu_buf
#define ifc_req ifc_ifcu.ifcu_req

#endif
INNER

# fcntl.h constants update
cat >> $INCLUDE/fcntl.h <<INNER
#define FASYNC      0x2000
#define FNDELAY     0x0800
#define F_SETOWN    8
INNER

# stdio.h update for asprintf
cat >> $INCLUDE/stdio.h <<INNER
int asprintf(char **strp, const char *fmt, ...);
int vasprintf(char **strp, const char *fmt, va_list ap);
INNER

# fcntl.h update for O_NDELAY
cat >> $INCLUDE/fcntl.h <<INNER
#define O_NDELAY    0x0800
INNER


# math.h
cat > $INCLUDE/math.h <<INNER
#ifndef _MATH_H
#define _MATH_H

double cos(double x);
double sin(double x);
double tan(double x);
double acos(double x);
double asin(double x);
double atan(double x);
double atan2(double y, double x);
double exp(double x);
double log(double x);
double log10(double x);
double pow(double x, double y);
double sqrt(double x);
double ceil(double x);
double fabs(double x);
double floor(double x);
double fmod(double x, double y);
double hypot(double x, double y);
void sincos(double x, double *sinp, double *cosp);

float cosf(float x);
float sinf(float x);
float tanf(float x);
float acosf(float x);
float asinf(float x);
float atanf(float x);
float atan2f(float y, float x);
float expf(float x);
float logf(float x);
float log10f(float x);
float powf(float x, float y);
float sqrtf(float x);
float ceilf(float x);
float fabsf(float x);
float floorf(float x);
float fmodf(float x, float y);
float hypotf(float x, float y);
void sincosf(float x, float *sinp, float *cosp);

#define HUGE_VAL    (__builtin_huge_val())
#define HUGE_VALF   (__builtin_huge_valf())
#define HUGE_VALL   (__builtin_huge_vall())
#define INFINITY    (__builtin_inff())
#define NAN         (__builtin_nanf(""))

#define M_E         2.7182818284590452354
#define M_LOG2E     1.4426950408889634074
#define M_LOG10E    0.43429448190325182765
#define M_LN2       0.69314718055994530942
#define M_LN10      2.30258509299404568402
#define M_PI        3.14159265358979323846
#define M_PI_2      1.57079632679489661923
#define M_PI_4      0.78539816339744830962
#define M_1_PI      0.31830988618379067154
#define M_2_PI      0.63661977236758134308
#define M_2_SQRTPI  1.12837916709551257390
#define M_SQRT2     1.41421356237309504880
#define M_SQRT1_2   0.70710678118654752440

#endif
INNER




# more stdio.h functions
cat >> $INCLUDE/stdio.h <<INNER
void setlinebuf(FILE *stream);
INNER

# more stat.h functions
cat >> $INCLUDE/sys/stat.h <<INNER
mode_t umask(mode_t mask);
INNER


# sys/mman.h
cat > $SYSROOT/usr/include/sys/mman.h <<INNER
#ifndef _SYS_MMAN_H
#define _SYS_MMAN_H

#include <sys/types.h>

#define PROT_READ  0x1
#define PROT_WRITE 0x2
#define PROT_EXEC  0x4
#define PROT_NONE  0x0

#define MAP_SHARED  0x01
#define MAP_PRIVATE 0x02
#define MAP_FIXED   0x10
#define MAP_ANONYMOUS 0x20

void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset);
int munmap(void *addr, size_t length);
int getpagesize(void);

#endif
INNER


# alloca.h
cat > $INCLUDE/alloca.h <<INNER
#ifndef _ALLOCA_H
#define _ALLOCA_H

#include <stddef.h>

#define alloca(size) __builtin_alloca(size)

#endif
INNER
