#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "backend.h"
#include "log.h"

/*
 * Eclipse OS libseat backend.
 *
 * On Eclipse OS there is no seat daemon or logind.  The kernel exposes DRM
 * and input devices as regular files that any privileged process can open
 * directly.  This backend simply opens the requested device path with
 * open(2) and returns the resulting file descriptor as the device handle,
 * mirroring the "noop" backend but clearly identified so that callers that
 * set LIBSEAT_BACKEND=eclipse get a named, discoverable backend.
 *
 * Dispatch uses a socketpair: the write-end is never written to in normal
 * operation, but the read-end is returned by get_fd() so that poll()-based
 * event loops can include it without blocking indefinitely.  The
 * enable_seat callback is fired exactly once on the first dispatch() call,
 * matching what wlroots / labwc expects from libseat.
 */

struct backend_eclipse {
	struct libseat base;
	const struct libseat_seat_listener *seat_listener;
	void *seat_listener_data;

	bool initial_setup;
	int sockets[2]; /* [0]=read/event end, [1]=write end */
};

extern const struct seat_impl eclipse_impl;

static struct backend_eclipse *backend_eclipse_from_libseat(struct libseat *base) {
	assert(base->impl == &eclipse_impl);
	return (struct backend_eclipse *)base;
}

static void destroy(struct backend_eclipse *backend) {
	close(backend->sockets[0]);
	close(backend->sockets[1]);
	free(backend);
}

static int close_seat(struct libseat *base) {
	struct backend_eclipse *backend = backend_eclipse_from_libseat(base);
	destroy(backend);
	return 0;
}

static int disable_seat(struct libseat *base) {
	(void)base;
	return 0;
}

static const char *seat_name(struct libseat *base) {
	(void)base;
	return "seat0";
}

static int open_device(struct libseat *base, const char *path, int *fd) {
	(void)base;

	int tmpfd = open(path, O_RDWR | O_NOCTTY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK);
	if (tmpfd < 0) {
		log_errorf("Eclipse backend: failed to open device '%s': %s", path,
			   strerror(errno));
		return -1;
	}

	*fd = tmpfd;
	return tmpfd; /* device_id == fd for this backend */
}

static int close_device(struct libseat *base, int device_id) {
	(void)base;
	if (device_id >= 0) {
		close(device_id);
	}
	return 0;
}

static int switch_session(struct libseat *base, int s) {
	(void)base;
	(void)s;
	log_errorf("Eclipse backend cannot switch to session %d", s);
	return -1;
}

static int get_fd(struct libseat *base) {
	struct backend_eclipse *backend = backend_eclipse_from_libseat(base);
	return backend->sockets[0];
}

static int dispatch(struct libseat *base, int timeout) {
	struct backend_eclipse *backend = backend_eclipse_from_libseat(base);

	if (backend->initial_setup) {
		backend->initial_setup = false;
		if (backend->seat_listener != NULL &&
		    backend->seat_listener->enable_seat != NULL) {
			backend->seat_listener->enable_seat(&backend->base,
							    backend->seat_listener_data);
		}
	}

	struct pollfd pfd = {
		.fd = backend->sockets[0],
		.events = POLLIN,
	};
	if (poll(&pfd, 1, timeout) < 0) {
		if (errno == EAGAIN || errno == EINTR) {
			return 0;
		}
		return -1;
	}

	return 0;
}

static struct libseat *eclipse_open_seat(const struct libseat_seat_listener *listener,
					 void *data) {
	struct backend_eclipse *backend = calloc(1, sizeof(struct backend_eclipse));
	if (backend == NULL) {
		return NULL;
	}

	if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, backend->sockets) != 0) {
		log_errorf("Eclipse backend: socketpair() failed: %s", strerror(errno));
		free(backend);
		return NULL;
	}

	backend->initial_setup = true;
	backend->seat_listener = listener;
	backend->seat_listener_data = data;
	backend->base.impl = &eclipse_impl;

	return &backend->base;
}

const struct seat_impl eclipse_impl = {
	.open_seat = eclipse_open_seat,
	.disable_seat = disable_seat,
	.close_seat = close_seat,
	.seat_name = seat_name,
	.open_device = open_device,
	.close_device = close_device,
	.switch_session = switch_session,
	.get_fd = get_fd,
	.dispatch = dispatch,
};
