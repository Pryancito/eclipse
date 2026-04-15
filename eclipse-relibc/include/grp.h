/* grp.h for Eclipse OS */
#pragma once
#ifndef _GRP_H
#define _GRP_H

#include <sys/types.h>

struct group {
    char   *gr_name;
    char   *gr_passwd;
    gid_t   gr_gid;
    char  **gr_mem;
};

struct group *getgrgid(gid_t gid);
struct group *getgrnam(const char *name);

#endif /* _GRP_H */
