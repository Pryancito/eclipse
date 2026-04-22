/*
 * Eclipse OS libseat backend.
 *
 * Opens DRM/input devices directly (no privilege daemon needed — Eclipse's
 * kernel already grants access) and enables the seat immediately on the
 * first dispatch call.  A self-pipe is used as the event fd so that
 * wl_event_loop can poll on it without blocking.
 *
 * SPDX-License-Identifier: MIT
 */

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "backend.h"
#include "log.h"

struct backend_eclipse {
	struct libseat base;
	const struct libseat_seat_listener *seat_listener;
	void *seat_listener_data;

	/* Self-pipe: pipe_fds[0] = read end (given to event loop),
	 *            pipe_fds[1] = write end (used to wake up poll). */
	int pipe_fds[2];

	bool initial_setup;
};

extern const struct seat_impl eclipse_impl;

static struct backend_eclipse *
backend_eclipse_from_libseat(struct libseat *base)
{
	assert(base->impl == &eclipse_impl);
	return (struct backend_eclipse *)base;
}

static void
destroy(struct backend_eclipse *backend)
{
	if (backend->pipe_fds[0] >= 0) {
		close(backend->pipe_fds[0]);
	}
	if (backend->pipe_fds[1] >= 0) {
		close(backend->pipe_fds[1]);
	}
	free(backend);
}

static int
close_seat(struct libseat *base)
{
	struct backend_eclipse *backend = backend_eclipse_from_libseat(base);
	destroy(backend);
	return 0;
}

static int
disable_seat(struct libseat *base)
{
	(void)base;
	return 0;
}

static const char *
seat_name(struct libseat *base)
{
	(void)base;
	return "seat0";
}

static int
open_device(struct libseat *base, const char *path, int *fd)
{
	(void)base;

	int tmpfd = open(path, O_RDWR | O_CLOEXEC | O_NOCTTY | O_NONBLOCK);
	if (tmpfd < 0) {
		log_errorf("Eclipse: failed to open device %s: %s", path,
			   strerror(errno));
		return -1;
	}

	*fd = tmpfd;
	/* Return value is used as the device_id passed to close_device. */
	return tmpfd;
}

static int
close_device(struct libseat *base, int device_id)
{
	(void)base;
	if (device_id >= 0) {
		close(device_id);
	}
	return 0;
}

static int
switch_session(struct libseat *base, int s)
{
	(void)base;
	(void)s;
	log_errorf("Eclipse backend cannot switch to session %d", s);
	return -1;
}

static int
get_fd(struct libseat *base)
{
	struct backend_eclipse *backend = backend_eclipse_from_libseat(base);
	return backend->pipe_fds[0];
}

static int
dispatch(struct libseat *base, int timeout)
{
	struct backend_eclipse *backend = backend_eclipse_from_libseat(base);

	/* On the first dispatch, enable the seat immediately. */
	if (backend->initial_setup) {
		backend->initial_setup = false;
		backend->seat_listener->enable_seat(&backend->base,
						    backend->seat_listener_data);
		return 0;
	}

	/* Drain any bytes written to the pipe, then return. */
	struct pollfd pfd = {
		.fd = backend->pipe_fds[0],
		.events = POLLIN,
	};
	int ret = poll(&pfd, 1, timeout);
	if (ret < 0) {
		if (errno == EAGAIN || errno == EINTR) {
			return 0;
		}
		return -1;
	}
	if (ret > 0 && (pfd.revents & POLLIN)) {
		char buf[64];
		ssize_t n = read(backend->pipe_fds[0], buf, sizeof(buf));
		if (n < 0 && errno != EAGAIN && errno != EINTR) {
			log_errorf("Eclipse: pipe read error: %s", strerror(errno));
			return -1;
		}
	}
	return 0;
}

static struct libseat *
eclipse_open_seat(const struct libseat_seat_listener *listener, void *data)
{
	struct backend_eclipse *backend =
		calloc(1, sizeof(struct backend_eclipse));
	if (!backend) {
		return NULL;
	}

	backend->pipe_fds[0] = -1;
	backend->pipe_fds[1] = -1;

	if (pipe2(backend->pipe_fds, O_CLOEXEC | O_NONBLOCK) != 0) {
		log_errorf("Eclipse: pipe2() failed: %s", strerror(errno));
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
