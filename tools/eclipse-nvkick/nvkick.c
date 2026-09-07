/*
 * libeclipse_nvkick.so — usermode NVIDIA kick for Eclipse OS.
 *
 * Intercepts DRM_NOUVEAU_EXEC and SYNCOBJ_WAIT/QUERY on the nouveau node
 * and performs GPPut + doorbell + fence poll from userspace, the way
 * nouveau on Linux kicks a channel without a kernel round-trip per frame.
 *
 * Disable: ECLIPSE_NV_USERMODE=0
 * Debug:   ECLIPSE_NV_USERMODE=debug  (one line to stderr every 1024 kicks)
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdarg.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#define DRM_COMMAND_BASE 0x40
#define NR_CHANNEL_ALLOC (DRM_COMMAND_BASE + 0x02)
#define NR_CHANNEL_FREE (DRM_COMMAND_BASE + 0x03)
#define NR_EXEC (DRM_COMMAND_BASE + 0x12)
#define NR_USERMODE (DRM_COMMAND_BASE + 0x51)

#define NR_SYNCOBJ_WAIT 0xC3
#define NR_SYNCOBJ_TIMELINE_WAIT 0xCA
#define NR_SYNCOBJ_QUERY 0xCB

#define SYNC_TIMELINE 0x1
#define WAIT_ALL (1u << 0)
#define WAIT_AVAILABLE (1u << 2)
#define QUERY_LAST_SUBMITTED (1u << 0)

#define ATTACH_PUT 0x40
#define ATTACH_GET 0x44
#define ATTACH_REC 0x80
#define ATTACH_CAP 32u

#define DRM_IOWR(nr, type) \
	((3u << 30) | ((unsigned)sizeof(type) << 16) | ((unsigned)'d' << 8) | (unsigned)(nr))

struct drm_eclipse_nv_usermode {
	uint32_t channel;
	uint32_t ctx_idx;
	uint32_t work_token;
	uint32_t entries;
	uint32_t gpget_off;
	uint32_t gpput_off;
	uint32_t doorbell_off;
	uint32_t slot_bytes;
	uint32_t next_payload;
	uint32_t userd_base_off;
	uint32_t fence_pb_off;
	uint32_t fence_sem_off;
	uint64_t buf_gpu_va;
	uint64_t fence_mmap;
	uint64_t gpfifo_mmap;
	uint64_t fence_pb_mmap;
	uint64_t userd_mmap;
	uint64_t doorbell_mmap;
};

struct drm_nouveau_exec {
	uint32_t channel;
	uint32_t push_count;
	uint32_t wait_count;
	uint32_t sig_count;
	uint64_t wait_ptr;
	uint64_t sig_ptr;
	uint64_t push_ptr;
};

struct drm_nouveau_exec_push {
	uint64_t va;
	uint32_t va_len;
	uint32_t flags;
};

struct drm_nouveau_sync {
	uint32_t flags;
	uint32_t handle;
	uint64_t timeline_value;
};

struct drm_nouveau_channel_alloc {
	uint32_t fb_ctxdma_handle;
	uint32_t tt_ctxdma_handle;
	int32_t channel;
	uint32_t pushbuf_domains;
	uint32_t notifier_handle;
	uint32_t subchan[16];
	uint32_t nr_subchan;
};

struct drm_nouveau_channel_free {
	int32_t channel;
};

struct drm_syncobj_wait {
	uint64_t handles;
	int64_t timeout_nsec;
	uint32_t count_handles;
	uint32_t flags;
	uint32_t first_signaled;
	uint32_t pad;
};

struct drm_syncobj_timeline_wait {
	uint64_t handles;
	uint64_t points;
	int64_t timeout_nsec;
	uint32_t count_handles;
	uint32_t flags;
	uint32_t first_signaled;
	uint32_t pad;
};

struct drm_syncobj_timeline_array {
	uint64_t handles;
	uint64_t points;
	uint32_t count_handles;
	uint32_t flags;
};

struct attach_rec {
	uint32_t handle;
	uint32_t payload;
	uint64_t point;
};

struct sig_last {
	uint32_t handle;
	uint32_t payload;
	uint64_t point;
};

struct chan {
	int fd;
	uint32_t channel;
	int mapped;
	struct drm_eclipse_nv_usermode u;
	volatile uint32_t *gpfifo;
	volatile uint32_t *fence_sem;
	volatile uint8_t *fence_pb;
	volatile uint8_t *userd;
	volatile uint32_t *doorbell;
	uint32_t next_payload;
	struct sig_last last[64];
	unsigned nlast;
};

static int (*real_ioctl)(int, int, ...);
static int enabled = -1;
static int debug;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct chan chans[8];
static unsigned long kicks;

static int env_enabled(void);

static void nv_init(void)
{
	for (int i = 0; i < 8; i++)
		chans[i].fd = -1;
	real_ioctl = (int (*)(int, int, ...))dlsym(RTLD_NEXT, "ioctl");
	enabled = env_enabled();
}

__attribute__((constructor))
static void nv_ctor(void)
{
	nv_init();
}

static int ioc_nr(int req)
{
	return req & 0xff;
}

static int ioc_type(int req)
{
	return (req >> 8) & 0xff;
}

static void store_fence(void)
{
#if defined(__x86_64__) || defined(__i386__)
	__asm__ __volatile__("sfence" ::: "memory");
#else
	atomic_thread_fence(memory_order_seq_cst);
#endif
}

static uint32_t gp_entry0(uint64_t va)
{
	return (uint32_t)va & ~3u;
}

static uint32_t gp_entry1(uint64_t va, uint32_t len_bytes)
{
	return ((uint32_t)(va >> 32) & 0xffu) | (((len_bytes / 4) & 0x1fffffu) << 10);
}

static void sem_stream(uint32_t *out, uint64_t sem_va, uint32_t payload)
{
	/* push_hdr(0, 0x5c, 5) | addr lo/hi | payload | 0 | RELEASE */
	out[0] = (1u << 29) | (5u << 16) | ((0x5c >> 2) & 0xfff);
	out[1] = (uint32_t)sem_va;
	out[2] = ((uint32_t)(sem_va >> 32)) & 0xffu;
	out[3] = payload;
	out[4] = 0;
	out[5] = 0x1;
}

static int fence_landed(volatile uint32_t *zone, uint32_t payload)
{
	uint32_t v = atomic_load_explicit((_Atomic uint32_t *)zone, memory_order_relaxed);
	return (int32_t)(v - payload) >= 0;
}

static int env_enabled(void)
{
	const char *e = getenv("ECLIPSE_NV_USERMODE");
	if (e && (e[0] == '0' && e[1] == 0))
		return 0;
	if (e && strcmp(e, "debug") == 0)
		debug = 1;
	return 1;
}

static void *map_off(int fd, uint64_t off)
{
	void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)off);
	return p == MAP_FAILED ? NULL : p;
}

static void unmap_chan(struct chan *c)
{
	if (c->gpfifo)
		munmap((void *)c->gpfifo, 4096);
	if (c->fence_sem)
		munmap((void *)c->fence_sem, 4096);
	if (c->fence_pb)
		munmap((void *)c->fence_pb, 4096);
	if (c->userd)
		munmap((void *)c->userd, 4096);
	if (c->doorbell)
		munmap((void *)((uintptr_t)c->doorbell & ~0xffful), 4096);
	memset(c, 0, sizeof(*c));
	c->fd = -1;
}

static struct chan *find_chan(int fd, uint32_t channel)
{
	for (int i = 0; i < 8; i++) {
		if (chans[i].fd == fd && chans[i].channel == channel)
			return &chans[i];
	}
	return NULL;
}

static struct chan *alloc_chan(int fd, uint32_t channel)
{
	struct chan *c = find_chan(fd, channel);
	if (c)
		return c;
	for (int i = 0; i < 8; i++) {
		if (chans[i].fd <= 0) {
			memset(&chans[i], 0, sizeof(chans[i]));
			chans[i].fd = fd;
			chans[i].channel = channel;
			return &chans[i];
		}
	}
	return NULL;
}

static int map_chan(struct chan *c)
{
	struct drm_eclipse_nv_usermode u;
	memset(&u, 0, sizeof(u));
	u.channel = c->channel;
	if (real_ioctl(c->fd, (int)DRM_IOWR(NR_USERMODE, struct drm_eclipse_nv_usermode), &u) != 0)
		return -1;
	if (!u.fence_mmap || !u.gpfifo_mmap || !u.userd_mmap || !u.doorbell_mmap || !u.fence_pb_mmap)
		return -1;
	c->u = u;
	c->gpfifo = map_off(c->fd, u.gpfifo_mmap);
	c->fence_sem = map_off(c->fd, u.fence_mmap);
	c->fence_pb = map_off(c->fd, u.fence_pb_mmap);
	c->userd = map_off(c->fd, u.userd_mmap);
	{
		void *db = map_off(c->fd, u.doorbell_mmap);
		c->doorbell = db ? (volatile uint32_t *)((uint8_t *)db + u.doorbell_off) : NULL;
	}
	if (!c->gpfifo || !c->fence_sem || !c->fence_pb || !c->userd || !c->doorbell) {
		unmap_chan(c);
		c->fd = -1;
		return -1;
	}
	c->next_payload = u.next_payload ? u.next_payload : 1;
	c->mapped = 1;
	if (debug)
		fprintf(stderr, "[nvkick] mapped fd=%d ch=%u ctx=%u token=%#x entries=%u\n",
			c->fd, c->channel, u.ctx_idx, u.work_token, u.entries);
	return 0;
}

static volatile uint32_t *userd_u32(struct chan *c, uint32_t off)
{
	return (volatile uint32_t *)(c->userd + c->u.userd_base_off + off);
}

static void remember_sig(struct chan *c, uint32_t handle, uint64_t point, uint32_t payload)
{
	for (unsigned i = 0; i < c->nlast; i++) {
		if (c->last[i].handle == handle) {
			c->last[i].point = point;
			c->last[i].payload = payload;
			return;
		}
	}
	if (c->nlast < 64) {
		c->last[c->nlast].handle = handle;
		c->last[c->nlast].point = point;
		c->last[c->nlast].payload = payload;
		c->nlast++;
	}
}

static int lookup_sig(struct chan *c, uint32_t handle, uint64_t need, uint32_t *payload)
{
	for (unsigned i = 0; i < c->nlast; i++) {
		if (c->last[i].handle == handle && c->last[i].point >= need) {
			if (payload)
				*payload = c->last[i].payload;
			return 1;
		}
	}
	return 0;
}

static int push_attach(struct chan *c, uint32_t handle, uint64_t point, uint32_t payload)
{
	volatile uint32_t *putp = (volatile uint32_t *)((uint8_t *)c->fence_sem + ATTACH_PUT);
	volatile uint32_t *getp = (volatile uint32_t *)((uint8_t *)c->fence_sem + ATTACH_GET);
	struct timespec t0;
	clock_gettime(CLOCK_MONOTONIC, &t0);
	for (;;) {
		uint32_t put = *putp;
		uint32_t get = *getp;
		if ((put - get) < ATTACH_CAP) {
			struct attach_rec rec = { .handle = handle, .payload = payload, .point = point };
			struct attach_rec *slot = (struct attach_rec *)((uint8_t *)c->fence_sem + ATTACH_REC
									+ (put % ATTACH_CAP) * sizeof(rec));
			*slot = rec;
			store_fence();
			*putp = put + 1;
			return 0;
		}
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		if ((now.tv_sec - t0.tv_sec) >= 1)
			return -1;
		__asm__ __volatile__("pause" ::: "memory");
	}
}

static int kick(struct chan *c, const struct drm_nouveau_exec *req)
{
	uint32_t needed = req->push_count + (req->sig_count > 0);
	uint32_t entries = c->u.entries;
	if (needed == 0 || needed >= entries || req->push_count > 64)
		return -EINVAL;

	if (req->wait_count && req->wait_ptr) {
		const struct drm_nouveau_sync *waits =
			(const struct drm_nouveau_sync *)(uintptr_t)req->wait_ptr;
		for (uint32_t i = 0; i < req->wait_count; i++) {
			uint64_t target = (waits[i].flags & 0xf) == SYNC_TIMELINE ? waits[i].timeline_value : 1;
			if (!lookup_sig(c, waits[i].handle, target, NULL)) {
				/* Foreign fence: kernel WAIT (and drain) without our lock. */
				struct drm_syncobj_wait w = {
					.handles = (uint64_t)(uintptr_t)&waits[i].handle,
					.timeout_nsec = 0x7fffffffffffffffLL,
					.count_handles = 1,
					.flags = WAIT_ALL,
				};
				int fd = c->fd;
				pthread_mutex_unlock(&lock);
				int wr = real_ioctl(fd, (int)DRM_IOWR(NR_SYNCOBJ_WAIT, struct drm_syncobj_wait), &w);
				int err = errno;
				pthread_mutex_lock(&lock);
				if (!c->mapped)
					return -ENODEV;
				if (wr != 0)
					return err ? -err : -EIO;
			}
		}
	}

	volatile uint32_t *gpput = userd_u32(c, c->u.gpput_off);
	volatile uint32_t *gpget = userd_u32(c, c->u.gpget_off);
	struct timespec t0;
	clock_gettime(CLOCK_MONOTONIC, &t0);
	uint32_t put, get, used;
	for (;;) {
		put = (*gpput) % entries;
		get = (*gpget) % entries;
		used = (put + entries - get) % entries;
		if (used + needed <= entries - 1)
			break;
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		if ((now.tv_sec - t0.tv_sec) >= 1)
			return -EIO;
		__asm__ __volatile__("pause" ::: "memory");
	}

	const struct drm_nouveau_exec_push *pushes =
		(const struct drm_nouveau_exec_push *)(uintptr_t)req->push_ptr;
	uint32_t slot = put;
	for (uint32_t i = 0; i < req->push_count; i++) {
		volatile uint32_t *gp = c->gpfifo + slot * 2;
		gp[0] = gp_entry0(pushes[i].va);
		gp[1] = gp_entry1(pushes[i].va, pushes[i].va_len);
		slot = (slot + 1) % entries;
	}

	uint32_t payload = 0;
	if (req->sig_count && req->sig_ptr) {
		payload = c->next_payload;
		c->next_payload++;
		if (c->next_payload == 0)
			c->next_payload = 1;
		uint32_t words[6];
		uint64_t sem_gpu = c->u.buf_gpu_va + c->u.fence_sem_off;
		uint64_t stream_gpu = c->u.buf_gpu_va + c->u.fence_pb_off
				      + (uint64_t)slot * c->u.slot_bytes;
		sem_stream(words, sem_gpu, payload);
		volatile uint32_t *st =
			(volatile uint32_t *)(c->fence_pb + slot * c->u.slot_bytes);
		for (int i = 0; i < 6; i++)
			st[i] = words[i];
		volatile uint32_t *gp = c->gpfifo + slot * 2;
		gp[0] = gp_entry0(stream_gpu);
		gp[1] = gp_entry1(stream_gpu, 24);
		slot = (slot + 1) % entries;
	}

	store_fence();
	*gpput = slot;
	store_fence();
	*c->doorbell = c->u.work_token;

	if (payload && req->sig_ptr) {
		const struct drm_nouveau_sync *sigs =
			(const struct drm_nouveau_sync *)(uintptr_t)req->sig_ptr;
		for (uint32_t i = 0; i < req->sig_count; i++) {
			uint64_t point = (sigs[i].flags & 0xf) == SYNC_TIMELINE ? sigs[i].timeline_value : 1;
			remember_sig(c, sigs[i].handle, point, payload);
			push_attach(c, sigs[i].handle, point, payload);
		}
	}

	kicks++;
	if (debug && (kicks & 1023ul) == 0)
		fprintf(stderr, "[nvkick] kicks=%lu fd=%d ch=%u\n", kicks, c->fd, c->channel);
	return 0;
}

static int usermode_exec(int fd, int req, void *arg)
{
	struct drm_nouveau_exec *e = arg;
	pthread_mutex_lock(&lock);
	struct chan *c = alloc_chan(fd, e->channel);
	int rc = -1;
	if (c) {
		if (!c->mapped)
			map_chan(c);
		if (c->mapped)
			rc = kick(c, e);
	}
	pthread_mutex_unlock(&lock);
	if (rc == 0)
		return 0;
	if (rc < 0 && rc != -1) {
		errno = -rc;
		return -1;
	}
	return real_ioctl(fd, req, arg);
}

static int wait_local(int fd, const uint32_t *handles, const uint64_t *points,
		      uint32_t n, uint32_t flags, int64_t deadline_ns, uint32_t *first)
{
	int all = flags & WAIT_ALL;
	int available = flags & WAIT_AVAILABLE;
	struct chan *hit = NULL;
	uint32_t payload = 0;
	pthread_mutex_lock(&lock);
	for (int i = 0; i < 8; i++) {
		if (chans[i].fd != fd || !chans[i].mapped)
			continue;
		int ok = 1;
		for (uint32_t h = 0; h < n; h++) {
			uint64_t need = points ? points[h] : 1;
			uint32_t p = 0;
			if (!lookup_sig(&chans[i], handles[h], need, &p)) {
				ok = 0;
				break;
			}
			payload = p;
		}
		if (ok) {
			uint32_t max_p = payload;
			for (uint32_t h = 0; h < n; h++) {
				uint64_t need = points ? points[h] : 1;
				uint32_t p = 0;
				if (lookup_sig(&chans[i], handles[h], need, &p)
				    && (int32_t)(p - max_p) > 0)
					max_p = p;
			}
			payload = max_p;
			hit = &chans[i];
			break;
		}
		if (!all) {
			for (uint32_t h = 0; h < n; h++) {
				uint64_t need = points ? points[h] : 1;
				if (lookup_sig(&chans[i], handles[h], need, &payload)) {
					hit = &chans[i];
					if (first)
						*first = h;
					goto found;
				}
			}
		}
	}
found:
	if (!hit || !hit->fence_sem) {
		pthread_mutex_unlock(&lock);
		return 1; /* not ours */
	}
	if (available) {
		pthread_mutex_unlock(&lock);
		if (first)
			*first = 0;
		return 0;
	}
	volatile uint32_t *zone = hit->fence_sem;
	pthread_mutex_unlock(&lock);

	for (;;) {
		if (fence_landed(zone, payload)) {
			if (first)
				*first = 0;
			return 0;
		}
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		int64_t ns = (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec;
		if (deadline_ns >= 0 && ns >= deadline_ns) {
			errno = ETIME;
			return -1;
		}
		__asm__ __volatile__("pause" ::: "memory");
	}
}

static int usermode_wait(int fd, int req, void *arg)
{
	int nr = ioc_nr(req);
	uint32_t buf[64];
	uint64_t pts[64];
	const uint32_t *handles;
	const uint64_t *points = NULL;
	uint32_t n, flags;
	int64_t deadline;
	uint32_t *first_out;
	if (nr == NR_SYNCOBJ_TIMELINE_WAIT) {
		struct drm_syncobj_timeline_wait *w = arg;
		n = w->count_handles;
		flags = w->flags;
		deadline = w->timeout_nsec;
		first_out = &w->first_signaled;
		handles = (const uint32_t *)(uintptr_t)w->handles;
		points = (const uint64_t *)(uintptr_t)w->points;
	} else {
		struct drm_syncobj_wait *w = arg;
		n = w->count_handles;
		flags = w->flags;
		deadline = w->timeout_nsec;
		first_out = &w->first_signaled;
		handles = (const uint32_t *)(uintptr_t)w->handles;
	}
	if (n == 0 || n > 64 || !handles)
		return real_ioctl(fd, req, arg);
	memcpy(buf, handles, n * sizeof(uint32_t));
	if (points)
		memcpy(pts, points, n * sizeof(uint64_t));
	uint32_t first = 0;
	int r = wait_local(fd, buf, points ? pts : NULL, n, flags, deadline, &first);
	if (r == 0) {
		*first_out = first;
		return 0;
	}
	if (r < 0)
		return -1;
	return real_ioctl(fd, req, arg);
}

static int usermode_query(int fd, int req, void *arg)
{
	struct drm_syncobj_timeline_array *q = arg;
	if (!(q->flags & QUERY_LAST_SUBMITTED) || q->count_handles == 0 || q->count_handles > 64
	    || !q->handles || !q->points)
		return real_ioctl(fd, req, arg);
	const uint32_t *handles = (const uint32_t *)(uintptr_t)q->handles;
	uint64_t *points = (uint64_t *)(uintptr_t)q->points;
	pthread_mutex_lock(&lock);
	int all_local = 1;
	for (uint32_t i = 0; i < q->count_handles; i++) {
		int found = 0;
		for (int c = 0; c < 8; c++) {
			if (chans[c].fd != fd || !chans[c].mapped)
				continue;
			for (unsigned s = 0; s < chans[c].nlast; s++) {
				if (chans[c].last[s].handle == handles[i]) {
					points[i] = chans[c].last[s].point;
					found = 1;
					break;
				}
			}
			if (found)
				break;
		}
		if (!found) {
			all_local = 0;
			break;
		}
	}
	pthread_mutex_unlock(&lock);
	if (all_local)
		return 0;
	return real_ioctl(fd, req, arg);
}

int ioctl(int fd, int request, ...)
{
	va_list ap;
	va_start(ap, request);
	void *arg = va_arg(ap, void *);
	va_end(ap);

	if (!real_ioctl)
		nv_init();
	if (!real_ioctl) {
		errno = ENOSYS;
		return -1;
	}
	if (enabled < 0)
		enabled = env_enabled();
	if (!enabled || ioc_type(request) != 'd')
		return real_ioctl(fd, request, arg);

	int nr = ioc_nr(request);
	if (nr == NR_EXEC)
		return usermode_exec(fd, request, arg);
	if (nr == NR_SYNCOBJ_WAIT || nr == NR_SYNCOBJ_TIMELINE_WAIT)
		return usermode_wait(fd, request, arg);
	if (nr == NR_SYNCOBJ_QUERY)
		return usermode_query(fd, request, arg);
	if (nr == NR_CHANNEL_FREE) {
		struct drm_nouveau_channel_free *f = arg;
		pthread_mutex_lock(&lock);
		struct chan *c = find_chan(fd, (uint32_t)f->channel);
		if (c)
			unmap_chan(c);
		pthread_mutex_unlock(&lock);
		return real_ioctl(fd, request, arg);
	}
	return real_ioctl(fd, request, arg);
}
