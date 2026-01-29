CC = gcc
CFLAGS = -static -nostdlib -ffreestanding -no-pie -fno-pic -m64 -fno-stack-protector -fcf-protection=none
LDFLAGS = -static -nostdlib -Ttext=0x400000 -Wl,--build-id=none -Wl,-z,norelro

all: cosmic_desktop

cosmic_desktop: cosmic_desktop.c
	$(CC) $(CFLAGS) $(LDFLAGS) -o cosmic_desktop cosmic_desktop.c

clean:
	rm -f cosmic_desktop

.PHONY: all clean
