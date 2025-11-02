# Makefile para coreutils de Eclipse OS

CC = gcc
CFLAGS = -static -nostdlib -ffreestanding -no-pie -fno-pic -m64 -Os
LDFLAGS = -static -nostdlib -Ttext=0x400000 -Wl,--build-id=none

BINDIR = bin

all: $(BINDIR)/echo $(BINDIR)/ls $(BINDIR)/cat

$(BINDIR)/echo: echo.c
	@mkdir -p $(BINDIR)
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<
	@echo "✓ echo compilado"

$(BINDIR)/ls: ls.c
	@mkdir -p $(BINDIR)
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<
	@echo "✓ ls compilado"

$(BINDIR)/cat: cat.c
	@mkdir -p $(BINDIR)
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<
	@echo "✓ cat compilado"

clean:
	rm -rf $(BINDIR)

install: all
	@echo "Instalando binarios en /boot de Eclipse OS..."
	@# TODO: Copiar a la imagen de disco de Eclipse OS

.PHONY: all clean install

