/* wchar.h for Eclipse OS (minimal) */
#pragma once
#ifndef _WCHAR_H
#define _WCHAR_H

#include <stddef.h>
#include <stdint.h>

typedef uint32_t wint_t;

#ifndef __MBSTATE_T_DEFINED
#define __MBSTATE_T_DEFINED 1
typedef struct {
  unsigned int __opaque;
} mbstate_t;
#endif

#ifndef WEOF
#define WEOF ((wint_t)-1)
#endif

#endif /* _WCHAR_H */

