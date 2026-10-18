# signal-probe — integrity probe for signal delivery and return in Eclipse OS.
#
# Statically linked against musl so the single binary drops straight into the
# rootfs and runs from the shell. Override CC if your musl cross-compiler is
# named or located differently:
#
#   make                              # uses x86_64-linux-musl-gcc
#   make CC=musl-gcc
#   make CC=/path/to/x86_64-linux-musl-gcc
#
# `make linux` builds the same source with the host compiler, which is how the
# probe is meant to be read: every check in it is one Linux holds, so a FAIL on
# Linux is a bug in the probe and a FAIL only on Eclipse is a bug in Eclipse.

CC     ?= x86_64-linux-musl-gcc
CFLAGS ?= -O2 -static -Wall -Wextra

signal-probe: signal-probe.c
	$(CC) $(CFLAGS) -o $@ $<

linux: signal-probe.c
	cc -O2 -Wall -Wextra -o signal-probe-linux $<

clean:
	rm -f signal-probe signal-probe-linux

.PHONY: clean linux
