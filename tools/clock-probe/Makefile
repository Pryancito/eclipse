# clock-probe — integrity probe for the clocks and timers of Eclipse OS.
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
# probe is meant to be read: every invariant in it is one Linux holds, so a FAIL
# on Linux is a bug in the probe and a FAIL only on Eclipse is a bug in Eclipse.

CC     ?= x86_64-linux-musl-gcc
# -pthread: the watchdog that keeps a timer which never fires from hanging the
# probe is a thread, and so is the per-thread CPU-clock check.
CFLAGS ?= -O2 -static -pthread -Wall -Wextra

clock-probe: clock-probe.c
	$(CC) $(CFLAGS) -o $@ $<

linux: clock-probe.c
	cc -O2 -pthread -Wall -Wextra -o clock-probe-linux $<

clean:
	rm -f clock-probe clock-probe-linux

.PHONY: clean linux
