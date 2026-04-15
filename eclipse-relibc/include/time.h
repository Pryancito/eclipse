/* time.h for Eclipse OS */
#pragma once
#ifndef _TIME_H
#define _TIME_H

#include <sys/types.h>

#define CLOCKS_PER_SEC 1000000L

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

clock_t clock(void);
time_t  time(time_t *tloc);
double  difftime(time_t time1, time_t time0);
struct tm *localtime(const time_t *timep);
struct tm *gmtime(const time_t *timep);
time_t  mktime(struct tm *tm);
size_t  strftime(char *s, size_t max, const char *format, const struct tm *tm);
char   *ctime(const time_t *timep);
int     nanosleep(const struct timespec *req, struct timespec *rem);
int     clock_gettime(int clockid, struct timespec *tp);
int     clock_settime(int clockid, const struct timespec *tp);

#define CLOCK_REALTIME  0
#define CLOCK_MONOTONIC 1

#endif /* _TIME_H */
