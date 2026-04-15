/* sys/select.h for Eclipse OS */
#pragma once
#ifndef _SYS_SELECT_H
#define _SYS_SELECT_H

#include <sys/types.h>
#include <signal.h>
#include <time.h>

#define FD_SETSIZE 1024

typedef struct {
    unsigned long fds_bits[FD_SETSIZE / (8 * sizeof(unsigned long))];
} fd_set;

#define FD_ZERO(set)   do { unsigned long *_p = (set)->fds_bits; \
    for (int _i = 0; _i < (int)(FD_SETSIZE / (8 * sizeof(unsigned long))); _i++) _p[_i] = 0; } while(0)
#define FD_SET(fd, set)   ((set)->fds_bits[(fd) / (8 * sizeof(unsigned long))] |= (1UL << ((fd) % (8 * sizeof(unsigned long)))))
#define FD_CLR(fd, set)   ((set)->fds_bits[(fd) / (8 * sizeof(unsigned long))] &= ~(1UL << ((fd) % (8 * sizeof(unsigned long)))))
#define FD_ISSET(fd, set) (((set)->fds_bits[(fd) / (8 * sizeof(unsigned long))] >> ((fd) % (8 * sizeof(unsigned long)))) & 1)

int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds,
           struct timeval *timeout);
int pselect(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds,
            const struct timespec *timeout, const sigset_t *sigmask);

#endif /* _SYS_SELECT_H */
