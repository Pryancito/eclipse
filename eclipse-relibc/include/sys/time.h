/* sys/time.h for Eclipse OS */
#pragma once
#ifndef _SYS_TIME_H
#define _SYS_TIME_H

#include <sys/types.h>

struct timezone {
  int tz_minuteswest;
  int tz_dsttime;
};

/* gettimeofday uses struct timeval (from sys/types.h). */
int gettimeofday(struct timeval *restrict tv, void *restrict tz);

#endif /* _SYS_TIME_H */

