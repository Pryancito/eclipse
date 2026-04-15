/* fcntl.h for Eclipse OS */
#pragma once
#ifndef _FCNTL_H
#define _FCNTL_H

#include <sys/types.h>

/* open() flags */
#define O_RDONLY    0x0000
#define O_WRONLY    0x0001
#define O_RDWR      0x0002
#define O_CREAT     0x0040
#define O_EXCL      0x0080
#define O_NOCTTY    0x0100
#define O_TRUNC     0x0200
#define O_APPEND    0x0400
#define O_NONBLOCK  0x0800
#define O_DSYNC     0x1000
#define O_SYNC      0x101000
#define O_CLOEXEC   0x80000
#define O_NOFOLLOW  0x20000
#define O_DIRECTORY 0x10000
#define O_PATH      0x200000

/* fcntl() commands */
#define F_DUPFD     0
#define F_GETFD     1
#define F_SETFD     2
#define F_GETFL     3
#define F_SETFL     4
#define F_GETLK     5
#define F_SETLK     6
#define F_SETLKW    7
#define F_DUPFD_CLOEXEC 1030

/* File descriptor flags */
#define FD_CLOEXEC  1

int open(const char *pathname, int flags, ...);
int creat(const char *pathname, int mode);
int fcntl(int fd, int cmd, ...);

#endif /* _FCNTL_H */
