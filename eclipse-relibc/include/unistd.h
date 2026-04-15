/* unistd.h for Eclipse OS */
#pragma once
#ifndef _UNISTD_H
#define _UNISTD_H

#include <sys/types.h>
#include <stddef.h>

/* Access modes for access() */
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4

/* lseek whence values */
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2

/* Standard file descriptors */
#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

/* POSIX version */
#define _POSIX_VERSION 200809L
#define _XOPEN_VERSION 700

extern char **environ;

/* Process functions */
pid_t fork(void);
pid_t getpid(void);
pid_t getppid(void);
void  _exit(int status) __attribute__((noreturn));

/* File descriptor functions */
int   close(int fd);
ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
int   dup(int oldfd);
int   dup2(int oldfd, int newfd);
int   dup3(int oldfd, int newfd, int flags);
int   pipe(int pipefd[2]);
int   pipe2(int pipefd[2], int flags);
off_t lseek(int fd, off_t offset, int whence);
int   ftruncate(int fd, off_t length);
int   truncate(const char *path, off_t length);
int   fsync(int fd);
int   fdatasync(int fd);
int   isatty(int fd);

/* Process execution */
int execve(const char *pathname, char *const argv[], char *const envp[]);
int execv(const char *pathname, char *const argv[]);
int execvp(const char *file, char *const argv[]);
int execvpe(const char *file, char *const argv[], char *const envp[]);

/* Working directory */
int   chdir(const char *path);
int   fchdir(int fd);
char *getcwd(char *buf, size_t size);

/* File operations */
int   access(const char *pathname, int mode);
int   unlink(const char *pathname);
int   rmdir(const char *pathname);
int   link(const char *oldpath, const char *newpath);
int   symlink(const char *target, const char *linkpath);
ssize_t readlink(const char *pathname, char *buf, size_t bufsiz);
int   rename(const char *oldpath, const char *newpath);

/* User / group functions */
uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);
gid_t getegid(void);
int   setuid(uid_t uid);
int   seteuid(uid_t euid);
int   setgid(gid_t gid);
int   setegid(gid_t egid);
int   getgroups(int size, gid_t list[]);
int   setgroups(size_t size, const gid_t *list);
int   getlogin_r(char *buf, size_t bufsize);

/* Process group / session */
pid_t getpgrp(void);
pid_t getpgid(pid_t pid);
int   setpgid(pid_t pid, pid_t pgid);
int   setpgrp(void);
pid_t setsid(void);
pid_t tcgetpgrp(int fd);
int   tcsetpgrp(int fd, pid_t pgrp);

/* Miscellaneous */
unsigned int sleep(unsigned int seconds);
int usleep(unsigned int usec);
long sysconf(int name);
size_t confstr(int name, char *buf, size_t len);

#endif /* _UNISTD_H */
