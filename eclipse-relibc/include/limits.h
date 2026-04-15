/* limits.h for Eclipse OS */
#pragma once
#ifndef _LIMITS_H
#define _LIMITS_H

#define CHAR_BIT    8
#define SCHAR_MIN   (-128)
#define SCHAR_MAX   127
#define UCHAR_MAX   255
#define CHAR_MIN    SCHAR_MIN
#define CHAR_MAX    SCHAR_MAX
#define SHRT_MIN    (-32768)
#define SHRT_MAX    32767
#define USHRT_MAX   65535
#define INT_MIN     (-2147483648)
#define INT_MAX     2147483647
#define UINT_MAX    4294967295U
#define LONG_MIN    (-9223372036854775807L - 1)
#define LONG_MAX    9223372036854775807L
#define ULONG_MAX   (~0UL)
#define LLONG_MIN   (-9223372036854775807LL - 1)
#define LLONG_MAX   9223372036854775807LL
#define ULLONG_MAX  (~0ULL)

#define NAME_MAX    255
#define PATH_MAX    4096
#define OPEN_MAX    256
#define ARG_MAX     131072
#define CHILD_MAX   256
#define PIPE_BUF    4096
#define NGROUPS_MAX 32
#define _POSIX_OPEN_MAX 20
#define _POSIX_PATH_MAX 256
#define IOV_MAX     1024

#endif /* _LIMITS_H */
