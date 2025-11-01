CC = gcc
CFLAGS = -static -nostdlib -ffreestanding -no-pie -fno-pic -m64
LDFLAGS = -static -nostdlib -Ttext=0x400000 -Wl,--build-id=none

all: wayland_compositor

wayland_compositor: wayland_compositor.c
	$(CC) $(CFLAGS) $(LDFLAGS) -o wayland_compositor wayland_compositor.c

clean:
	rm -f wayland_compositor

.PHONY: all clean
