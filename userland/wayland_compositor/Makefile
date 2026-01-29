# Wayland Compositor Makefile with wlroots integration
#
# This Makefile builds the Wayland compositor with optional wlroots support.
# It will automatically detect if wlroots is available and link against it.
# If wlroots is not available, it builds with the custom implementation.

CC = gcc
CFLAGS = -Wall -Wextra -std=c11

# Try to detect wlroots using pkg-config
WLROOTS_AVAILABLE := $(shell pkg-config --exists wlroots && echo yes || echo no)
WAYLAND_AVAILABLE := $(shell pkg-config --exists wayland-server && echo yes || echo no)

ifeq ($(WLROOTS_AVAILABLE),yes)
    $(info Found wlroots - building with wlroots support)
    WLROOTS_CFLAGS := $(shell pkg-config --cflags wlroots wayland-server)
    WLROOTS_LIBS := $(shell pkg-config --libs wlroots wayland-server)
    CFLAGS += $(WLROOTS_CFLAGS) -DHAVE_WLROOTS
    LDFLAGS += $(WLROOTS_LIBS)
    TARGET = wayland_compositor_wlroots
else ifeq ($(WAYLAND_AVAILABLE),yes)
    $(info Found wayland-server - building with libwayland support)
    WAYLAND_CFLAGS := $(shell pkg-config --cflags wayland-server)
    WAYLAND_LIBS := $(shell pkg-config --libs wayland-server)
    CFLAGS += $(WAYLAND_CFLAGS) -DHAVE_WAYLAND
    LDFLAGS += $(WAYLAND_LIBS)
    TARGET = wayland_compositor_wayland
else
    $(info Building with custom implementation - no system libraries)
    CFLAGS += -static -nostdlib -ffreestanding -no-pie -fno-pic -m64 -fno-stack-protector -fcf-protection=none
    LDFLAGS += -static -nostdlib -Ttext=0x400000 -Wl,--build-id=none -Wl,-z,norelro
    TARGET = wayland_compositor
endif

.PHONY: all clean help

all: $(TARGET)

# Build with wlroots
wayland_compositor_wlroots: wayland_compositor.c
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)
	@echo "Built compositor with wlroots support"

# Build with libwayland
wayland_compositor_wayland: wayland_compositor.c
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)
	@echo "Built compositor with libwayland support"

# Build with custom implementation
wayland_compositor: wayland_compositor.c
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<
	@echo "Built compositor with custom implementation"

clean:
	rm -f wayland_compositor wayland_compositor_wlroots wayland_compositor_wayland

help:
	@echo "Wayland Compositor Build System"
	@echo ""
	@echo "Targets:"
	@echo "  all    - Build compositor (auto-detects available libraries)"
	@echo "  clean  - Remove build artifacts"
	@echo "  help   - Show this help message"
	@echo ""
	@echo "The build system will automatically detect and use:"
	@echo "  1. wlroots (preferred)"
	@echo "  2. libwayland-server (fallback)"
	@echo "  3. Custom implementation (fallback)"
	@echo ""
	@echo "To install dependencies:"
	@echo "  sudo apt-get install libwayland-dev libwlroots-dev"
