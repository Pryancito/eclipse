/* stdlib.h for Eclipse OS */
#pragma once
#ifndef _STDLIB_H
#define _STDLIB_H

#include <stddef.h>
#include <sys/types.h>

/* Minimal multibyte/locale surface area. */
#ifndef MB_CUR_MAX
#define MB_CUR_MAX 1
#endif

/* Memory allocation */
void *malloc(size_t size);
void  free(void *ptr);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);

/* Environment */
extern char **environ;
char *getenv(const char *name);
int   setenv(const char *name, const char *value, int overwrite);
int   putenv(char *string);
int   unsetenv(const char *name);
int   clearenv(void);

/* Program termination */
void exit(int status) __attribute__((noreturn));
void _Exit(int status) __attribute__((noreturn));
void abort(void) __attribute__((noreturn));

/* Conversions */
int        atoi(const char *s);
long       atol(const char *s);
long long  atoll(const char *s);
long       strtol(const char *s, char **endptr, int base);
long long  strtoll(const char *s, char **endptr, int base);
unsigned long       strtoul(const char *s, char **endptr, int base);
unsigned long long  strtoull(const char *s, char **endptr, int base);
double     strtod(const char *s, char **endptr);

/* Sorting and searching */
void qsort(void *base, size_t nmemb, size_t size,
           int (*compar)(const void *, const void *));
void *bsearch(const void *key, const void *base, size_t nmemb, size_t size,
              int (*compar)(const void *, const void *));

/* Misc */
int abs(int j);
long labs(long j);
int system(const char *command);
char *realpath(const char *path, char *resolved_path);
int mkstemp(char *tmpl);

/* Random numbers */
int rand(void);
void srand(unsigned int seed);
long random(void);
void srandom(unsigned int seed);

#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 2147483647

#endif /* _STDLIB_H */
