/*
 * LD_PRELOAD shim for labwc on Eclipse OS.
 *
 * labwc's spawn_async_no_shell double-forks, then the parent calls
 * g_strfreev(argv) as soon as the intermediate child exits — while the
 * grandchild may still be about to execvp(argv[0], argv). That is safe on
 * Linux only because fork CoW keeps the grandchild's heap private after the
 * parent's free. If CoW/TLB shootdown ever lets the free land on a shared
 * frame, argv[0] becomes NULL and musl's execvpe SIGSEGVs reading it
 * (exactly the "unhandled page fault @ 0x0 … proc=labwc" crash at session
 * autostart).
 *
 * Mitigations here:
 *   1. Delay the real g_strfreev by a couple of seconds so the grandchild
 *      can exec before the heap is touched.
 *   2. Make execvp(NULL) return ENOENT instead of crashing in musl.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

typedef void (*g_strfreev_fn)(char **);
typedef int (*execvp_fn)(const char *, char *const *);

struct pending {
	char **argv;
	struct timespec when;
	struct pending *next;
};

static g_strfreev_fn real_g_strfreev;
static execvp_fn real_execvp;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct pending *head;
static int started;

static void *reaper_main(void *arg)
{
	(void)arg;
	for (;;) {
		sleep(1);
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		pthread_mutex_lock(&lock);
		struct pending **pp = &head;
		while (*pp) {
			struct pending *p = *pp;
			int ripe = now.tv_sec > p->when.tv_sec ||
				   (now.tv_sec == p->when.tv_sec &&
				    now.tv_nsec >= p->when.tv_nsec);
			if (!ripe) {
				pp = &p->next;
				continue;
			}
			*pp = p->next;
			pthread_mutex_unlock(&lock);
			if (real_g_strfreev)
				real_g_strfreev(p->argv);
			free(p);
			pthread_mutex_lock(&lock);
		}
		pthread_mutex_unlock(&lock);
	}
	return NULL;
}

static void ensure_started(void)
{
	if (started)
		return;
	pthread_mutex_lock(&lock);
	if (!started) {
		real_g_strfreev = (g_strfreev_fn)dlsym(RTLD_NEXT, "g_strfreev");
		real_execvp = (execvp_fn)dlsym(RTLD_NEXT, "execvp");
		if (real_g_strfreev) {
			pthread_t t;
			if (pthread_create(&t, NULL, reaper_main, NULL) == 0)
				pthread_detach(t);
		}
		started = 1;
	}
	pthread_mutex_unlock(&lock);
}

void g_strfreev(char **str_array)
{
	ensure_started();
	if (!real_g_strfreev) {
		/* No interposable symbol — nothing we can do. */
		return;
	}
	if (!str_array)
		return;

	struct pending *p = malloc(sizeof(*p));
	if (!p) {
		real_g_strfreev(str_array);
		return;
	}
	clock_gettime(CLOCK_MONOTONIC, &p->when);
	/* Grace window for the double-forked grandchild to reach execvp. */
	p->when.tv_sec += 2;
	p->argv = str_array;
	pthread_mutex_lock(&lock);
	p->next = head;
	head = p;
	pthread_mutex_unlock(&lock);
}

int execvp(const char *file, char *const argv[])
{
	ensure_started();
	if (!file) {
		errno = ENOENT;
		return -1;
	}
	if (!real_execvp)
		real_execvp = (execvp_fn)dlsym(RTLD_NEXT, "execvp");
	if (!real_execvp) {
		errno = ENOSYS;
		return -1;
	}
	return real_execvp(file, argv);
}
