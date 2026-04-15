/* sys/types.h for Eclipse OS */
#pragma once
#ifndef _SYS_TYPES_H
#define _SYS_TYPES_H

#include <stddef.h>
#include <stdint.h>

typedef int             pid_t;
typedef unsigned int    uid_t;
typedef unsigned int    gid_t;
typedef unsigned long   ino_t;
typedef unsigned long   dev_t;
typedef long            off_t;
typedef unsigned int    mode_t;
typedef unsigned int    nlink_t;
typedef long            ssize_t;
typedef long            time_t;
typedef long            clock_t;
typedef long            suseconds_t;
typedef unsigned long   blksize_t;
typedef long            blkcnt_t;
typedef unsigned int    id_t;
typedef unsigned long   fsblkcnt_t;
typedef unsigned long   fsfilcnt_t;

struct timeval {
    time_t      tv_sec;
    suseconds_t tv_usec;
};

struct timespec {
    time_t tv_sec;
    long   tv_nsec;
};

#endif /* _SYS_TYPES_H */
