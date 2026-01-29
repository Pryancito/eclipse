CC = gcc
CFLAGS = -static -nostdlib -ffreestanding -no-pie -fno-pic -m64 -fno-stack-protector -fcf-protection=none
LDFLAGS = -static -nostdlib -Ttext=0x400000 -Wl,--build-id=none -Wl,-z,norelro

all: wayland_compositor

wayland_compositor: wayland_compositor.c
	$(CC) $(CFLAGS) $(LDFLAGS) -o wayland_compositor wayland_compositor.c

clean:
	rm -f wayland_compositor

.PHONY: all clean
