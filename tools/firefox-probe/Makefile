# firefox-probe — does this kernel give Firefox what it asks for?
#
# Statically linked against musl so the single binary drops straight into the
# rootfs and runs from the shell. Override CC if your musl cross-compiler is
# named or located differently:
#
#   make                              # uses x86_64-linux-musl-gcc
#   make CC=musl-gcc
#
# Run it on Linux first: it must report 0 failures there, since every check is
# a pattern taken from Firefox's own source. Then run it on Eclipse; what
# fails is the list of things standing between this kernel and the browser.

CC     ?= x86_64-linux-musl-gcc
CFLAGS ?= -O2 -static -Wall -Wextra

firefox-probe: firefox-probe.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f firefox-probe

.PHONY: clean
