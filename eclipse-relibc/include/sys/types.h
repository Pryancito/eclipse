/* sys/types.h for Eclipse OS */
#pragma once
#ifndef _SYS_TYPES_H
#define _SYS_TYPES_H

#include <stddef.h>
#include <stdint.h>

/* Make POSIX feature-test visible to consumers that don't include unistd.h. */
#ifndef _POSIX_VERSION
#define _POSIX_VERSION 200809L
#endif

/* Access modes for access()/eaccess(), commonly pulled in early. */
#ifndef F_OK
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4
#endif

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

/* Common struct guards used by multiple headers. */
#ifndef _STRUCT_TIMEVAL
#define _STRUCT_TIMEVAL 1
struct timeval {
    time_t      tv_sec;
    suseconds_t tv_usec;
};
#endif

#ifndef _STRUCT_TIMESPEC
#define _STRUCT_TIMESPEC 1
struct timespec {
    time_t tv_sec;
    long   tv_nsec;
};
#endif

#endif /* _SYS_TYPES_H */
