/* stdio.h for Eclipse OS */
#pragma once
#ifndef _STDIO_H
#define _STDIO_H

#include <stddef.h>
#include <stdarg.h>

typedef struct _FILE FILE;

extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;

#define stdin  stdin
#define stdout stdout
#define stderr stderr

#define EOF (-1)
#define BUFSIZ 4096

/* Formatting */
int printf(const char *format, ...) __attribute__((format(printf, 1, 2)));
int fprintf(FILE *stream, const char *format, ...) __attribute__((format(printf, 2, 3)));
int sprintf(char *str, const char *format, ...) __attribute__((format(printf, 2, 3)));
int snprintf(char *str, size_t size, const char *format, ...) __attribute__((format(printf, 3, 4)));
int vprintf(const char *format, va_list ap);
int vfprintf(FILE *stream, const char *format, va_list ap);
int vsprintf(char *str, const char *format, va_list ap);
int vsnprintf(char *str, size_t size, const char *format, va_list ap);

/* I/O */
int   fputc(int c, FILE *stream);
int   fputs(const char *s, FILE *stream);
int   fgetc(FILE *stream);
char *fgets(char *s, int size, FILE *stream);
int   puts(const char *s);
int   putchar(int c);
int   getchar(void);
int   ungetc(int c, FILE *stream);

/* File operations */
FILE *fopen(const char *pathname, const char *mode);
FILE *fdopen(int fd, const char *mode);
FILE *freopen(const char *pathname, const char *mode, FILE *stream);
int   fclose(FILE *stream);
int   fflush(FILE *stream);
size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);
int   fseek(FILE *stream, long offset, int whence);
long  ftell(FILE *stream);
void  rewind(FILE *stream);
int   feof(FILE *stream);
int   ferror(FILE *stream);
void  clearerr(FILE *stream);
int   fileno(FILE *stream);

/* Error */
void perror(const char *s);

/* Temporary files */
FILE *tmpfile(void);
char *tmpnam(char *s);

/* scanf */
int scanf(const char *format, ...);
int fscanf(FILE *stream, const char *format, ...);
int sscanf(const char *str, const char *format, ...);

#endif /* _STDIO_H */
