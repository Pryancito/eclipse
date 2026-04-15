/* ulimit.h for Eclipse OS */
#pragma once
#ifndef _ULIMIT_H
#define _ULIMIT_H

/* Commands for ulimit(2). */
#define UL_GETFSIZE 1
#define UL_SETFSIZE 2

long ulimit(int cmd, long newlim);

#endif /* _ULIMIT_H */

