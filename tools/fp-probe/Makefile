# fp-probe — do a thread's x87, SSE and AVX registers survive the kernel?
#
# Statically linked against musl so the single binary drops straight into the
# rootfs and runs from the shell. Override CC if your musl cross-compiler is
# named or located differently:
#
#   make                              # uses x86_64-linux-musl-gcc
#   make CC=musl-gcc
#
# `make linux` builds the same source with the host compiler: every check in
# it is one Linux holds, so a FAIL on Linux is a bug in the probe and a FAIL
# only on Eclipse is a bug in Eclipse.

CC     ?= x86_64-linux-musl-gcc
CFLAGS ?= -O2 -static -pthread -Wall -Wextra

fp-probe: fp-probe.c
	$(CC) $(CFLAGS) -o $@ $<

linux: fp-probe.c
	cc -O2 -pthread -Wall -Wextra -o fp-probe-linux $<

clean:
	rm -f fp-probe fp-probe-linux

.PHONY: clean linux
