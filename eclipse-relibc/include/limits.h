/* limits.h for Eclipse OS */
#pragma once
#ifndef _LIMITS_H
#define _LIMITS_H

#ifndef CHAR_BIT
#define CHAR_BIT    8
#endif
#ifndef SCHAR_MIN
#define SCHAR_MIN   (-128)
#endif
#ifndef SCHAR_MAX
#define SCHAR_MAX   127
#endif
#ifndef UCHAR_MAX
#define UCHAR_MAX   255
#endif
#ifndef CHAR_MIN
#define CHAR_MIN    SCHAR_MIN
#endif
#ifndef CHAR_MAX
#define CHAR_MAX    SCHAR_MAX
#endif
#ifndef SHRT_MIN
#define SHRT_MIN    (-32768)
#endif
#ifndef SHRT_MAX
#define SHRT_MAX    32767
#endif
#ifndef USHRT_MAX
#define USHRT_MAX   65535
#endif
#ifndef INT_MIN
#define INT_MIN     (-2147483647 - 1)
#endif
#ifndef INT_MAX
#define INT_MAX     2147483647
#endif
#ifndef UINT_MAX
#define UINT_MAX    4294967295U
#endif
#ifndef LONG_MIN
#define LONG_MIN    (-9223372036854775807L - 1)
#endif
#ifndef LONG_MAX
#define LONG_MAX    9223372036854775807L
#endif
#ifndef ULONG_MAX
#define ULONG_MAX   (~0UL)
#endif
#ifndef LLONG_MIN
#define LLONG_MIN   (-9223372036854775807LL - 1)
#endif
#ifndef LLONG_MAX
#define LLONG_MAX   9223372036854775807LL
#endif
#ifndef ULLONG_MAX
#define ULLONG_MAX  (~0ULL)
#endif

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
