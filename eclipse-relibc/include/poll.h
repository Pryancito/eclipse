/* poll.h for Eclipse OS */
#pragma once
#ifndef _POLL_H
#define _POLL_H

#define POLLIN   0x001
#define POLLPRI  0x002
#define POLLOUT  0x004
#define POLLERR  0x008
#define POLLHUP  0x010
#define POLLNVAL 0x020
#define POLLRDHUP 0x2000

struct pollfd {
    int   fd;
    short events;
    short revents;
};

int poll(struct pollfd *fds, unsigned long nfds, int timeout);
int ppoll(struct pollfd *fds, unsigned long nfds, const struct timespec *tmo, const sigset_t *sigmask);

#endif /* _POLL_H */
