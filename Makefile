# Makefile for top level of zCore

HOST_ARCH := $(shell uname -m)
ifeq ($(HOST_ARCH),arm64)
  HOST_ARCH := aarch64
endif

ifneq ($(filter 1 libos,$(LIBOS) $(PLATFORM)),)
  ARCH ?= $(HOST_ARCH)
else
  ARCH ?= x86_64
endif
XTASK ?= 1
LOG ?= error
IFACE ?= eno1
GRAPHIC ?= on
ACCEL ?= 1
# Host audio for `make qemu`. on = pipewire/pa/alsa (or wav if this QEMU
# has none of those). off = guest still gets intel-hda, host is silent.
AUDIO ?= on
# Desktop session for `make qemu`: labwc — the session verified to reach a
# full desktop in QEMU (X still hangs in its /sys/class/drm platform probe;
# see the branch history). The kernel cmdline gets `desktop=$(DESKTOP)`, which
# eclipse-init reads to pick the session. Real hardware images carry no such
# argument and also default to labwc via /etc/eclipse/desktop. Override with
# `make qemu DESKTOP=xorg` to work on the X session. The default follows
# VARIANT, so it is set just below, once VARIANT is known.

# Which image variant to build: `desktop` (the Eclipse of always — labwc/Xorg
# plus the whole apk closure: Mesa, Firefox, XFCE) or `minimal` (console only:
# busybox, networking, audio and `install-eclipse`, with `/etc/eclipse/desktop`
# set to `none` so `eclipse-init` starts no compositor).
#
# Each variant has its own rootfs (`rootfs/$(ARCH)` / `rootfs/$(ARCH)-minimal`)
# and its own artifacts, so building one never disturbs the other and
# `make release` can publish both in one go. `desktop` carries NO suffix, which
# is what keeps every historical path (`rootfs/x86_64`, `zCore/x86_64.img`,
# `ignored/target/efi.img.gz`) exactly where it was.
VARIANT ?= desktop
ifeq ($(filter $(VARIANT),desktop minimal),)
  $(error VARIANT debe ser `desktop` o `minimal`, no `$(VARIANT)`)
endif
VARIANT_SUFFIX := $(patsubst desktop,,$(patsubst minimal,-minimal,$(VARIANT)))

# A minimal image carries no compositor, so the cmdline must not ask for one:
# `desktop=` on the cmdline WINS over /etc/eclipse/desktop (selected_desktop()
# in eclipse-init), so a hardcoded DESKTOP=labwc overrode the `none` the
# minimal rootfs had written and the guest booted hunting for labwc. Still
# overridable: `make qemu VARIANT=minimal DESKTOP=xorg`.
ifeq ($(VARIANT),minimal)
  DESKTOP ?= none
else
  DESKTOP ?= labwc
endif

# Extra kernel cmdline options passed straight through to the guest, e.g.
# `make qemu KOPTS=drm.present_probe` or several at once separated by colons:
# `make qemu KOPTS=drm.present_probe:drm.flip_fence=off`. See zCore/Makefile
# for why setting CMDLINE by hand is not a substitute.
KOPTS ?=

STRIP := $(ARCH)-linux-musl-strip
export PATH=$(shell printenv PATH):$(CURDIR)/ignored/target/$(ARCH)/$(ARCH)-linux-musl-cross/bin/

.PHONY: help zircon-init update rootfs libc-test other-test image check doc clean
.PHONY: iso qcow2 img release

# print top level help
help:
	cargo xtask

# download zircon binaries
zircon-init:
	cargo zircon-init

# update toolchain and dependencies
update:
	cargo update-all

# put rootfs for linux mode
rootfs:
ifeq ($(XTASK), 1)
	cargo rootfs --arch $(ARCH) --variant $(VARIANT)
else ifeq ($(ARCH), riscv64)
	@rm -rf rootfs/riscv && mkdir -p rootfs/riscv/bin
	@wget https://github.com/rcore-os/busybox-prebuilts/raw/master/busybox-1.30.1-riscv64/busybox -O rootfs/riscv/bin/busybox
	@ln -s busybox rootfs/riscv/bin/ls
endif

# put libc tests into rootfs
libc-test:
	cargo libc-test --arch $(ARCH)
	find rootfs/$(ARCH)/libc-test -type f \
	       -name "*so" -o -name "*exe" -exec $(STRIP) {} \; 

# put other tests into rootfs
other-test:
	cargo other-test --arch $(ARCH)

# build image from rootfs
image: rootfs
ifeq ($(XTASK), 1)
	cargo image --arch $(ARCH) --variant $(VARIANT)
else ifeq ($(ARCH), riscv64)
	@echo building riscv.img
	@rcore-fs-fuse zCore/riscv64.img rootfs/riscv zip
	@qemu-img resize -f raw zCore/riscv64.img +5M
endif

# check code style
check:
	cargo check-style

# build and open project document
doc:
	cargo doc --open

# clean targets
clean:
	cargo clean
	rm -f  *.asm
	rm -rf rootfs
	rm -rf zCore/disk
	find zCore -maxdepth 1 -name "*.img" -delete
	find zCore -maxdepth 1 -name "*.bin" -delete

# delete targets, including those that are large and compile slowly
cleanup: clean
	rm -rf ignored/target

# delete everything, including origin files that are downloaded directly
clean-everything: clean
	rm -rf ignored

# rt-test:
# 	cd rootfs/x86_64 && git clone https://kernel.googlesource.com/pub/scm/linux/kernel/git/clrkwllms/rt-tests --depth 1
# 	cd rootfs/x86_64/rt-tests && make
# 	echo x86 gcc build rt-test,now need manual modificy.
qemu: image
	$(MAKE) -C zCore run MODE=release LINUX=1 LOG=$(LOG) GRAPHIC=$(GRAPHIC) ACCEL=$(ACCEL) DESKTOP=$(DESKTOP) AUDIO=$(AUDIO) KOPTS=$(KOPTS) VARIANT=$(VARIANT)

vbox: image
	$(MAKE) -C zCore vbox MODE=release LINUX=1 LOG=$(LOG) GRAPHIC=$(GRAPHIC) ACCEL=$(ACCEL) DESKTOP=$(DESKTOP) IFACE=$(IFACE) KOPTS=$(KOPTS) VARIANT=$(VARIANT)

# Macvtap networking: VM gets its own MAC/IP on the physical LAN.
# Useful for testing the I219-V driver on bare metal.
# Usage: make qemu-macvtap [LOG=warn] [IFACE=eno1]
qemu-real: image
	IFACE=$(IFACE) LOG=$(LOG) ACCEL=$(ACCEL) GRAPHIC=$(GRAPHIC) AUDIO=$(AUDIO) \
		bash zCore/run-qemu-macvtap.sh

################ Distribution images ################
#
# - `make iso`   builds a UEFI-bootable ISO image.
#   The El Torito ESP carries the *installer* initramfs (busybox +
#   install-eclipse + gzipped payloads, no desktop). The desktop apk
#   stack lives only in `/boot/rootfs.btrfs.gz`, which the installer
#   writes to disk. Live ISO session: console + `install-eclipse`.
# - `make qcow2` builds a qcow2 disk image that contains the ESP filesystem.
# - `make release` builds every (variant, arch) ISO in one go — see RELEASE
#   below.
#
# Each of these is per VARIANT and per ARCH: one ISO per combination, named for
# both, and each one askable on its own:
#
#   make iso                                   # desktop, x86_64 (lo de siempre)
#   make iso VARIANT=minimal                   # minimal, x86_64
#   make iso ARCH=aarch64 VARIANT=minimal      # minimal, arm64
#
# Both targets rely on the existing ESP directory produced by `zCore/Makefile`
# when building for x86_64.

DIST_DIR := $(CURDIR)/dist
BUILD_DIR := $(CURDIR)/build
MODE ?= release
ESP_DIR := $(CURDIR)/target/$(ARCH)/$(MODE)/esp

# Staging paths carry variant AND arch: `make release` runs the combinations one
# after another through the SAME tree, and a shared `build/iso-root` meant the
# second ISO inherited whatever the first left in it — the minimal ISO would
# ship the desktop ESP and nobody would see a thing in the log.
ISO_STAGING := $(BUILD_DIR)/iso-root-$(VARIANT)-$(ARCH)
ESP_IMG := $(BUILD_DIR)/esp-$(VARIANT)-$(ARCH).img
DISK_IMG := $(BUILD_DIR)/disk-$(VARIANT)-$(ARCH).img
ESP_IMG_SIZE_MB ?= 1024
DISK_IMG_SIZE_MB ?= 1024
# The name says variant and arch, so a dist/ with four files in it needs no
# README to be read.
ISO_OUT := $(DIST_DIR)/eclipse-$(VARIANT)-$(ARCH).iso
QCOW2_OUT := $(DIST_DIR)/eclipse-$(VARIANT)-$(ARCH).qcow2
IMG_OUT := $(DIST_DIR)/eclipse-$(VARIANT)-$(ARCH).img
# The payloads `cargo image` wrote for THIS variant (see `LinuxRootfs::artifact`
# in xtask: desktop keeps the historical names, minimal gets `-minimal` before
# the extension).
ARTIFACTS := $(CURDIR)/ignored/target
ISO_INITRAMFS := $(ARTIFACTS)/iso-initramfs$(VARIANT_SUFFIX).img
EFI_GZ := $(ARTIFACTS)/efi$(VARIANT_SUFFIX).img.gz
ROOTFS_GZ := $(ARTIFACTS)/rootfs$(VARIANT_SUFFIX).btrfs.gz
HOME_GZ := $(ARTIFACTS)/home$(VARIANT_SUFFIX).btrfs.gz

# The ESP that every distribution image is cut from, built for THIS variant.
#
# It exists as its own target because `iso` built it and `qcow2`/`img` did not:
# they packaged `$(ESP_DIR)/EFI` as they found it. That directory has no variant
# in its path, so its initramfs.img was whatever the last zCore build happened
# to leave there — `make iso VARIANT=desktop && make qcow2 VARIANT=minimal`
# wrote a qcow2 with the DESKTOP installer inside it under a minimal filename.
# Nothing in the output said so.
#
# DESKTOP=none because this is the installer session for all three: console
# plus install-eclipse, with the desktop living in the payloads the installer
# writes to disk, not in the ESP.
.PHONY: esp-for-variant
esp-for-variant:
	@test -f "$(ISO_INITRAMFS)" || (echo "falta $(ISO_INITRAMFS). ¿Ha fallado cargo image?"; exit 1)
	@$(MAKE) -C zCore build MODE=release LINUX=1 LOG=$(LOG) GRAPHIC=on GL=$(GL) \
		DESKTOP=none VARIANT=$(VARIANT) INITRAMFS_IMG="$(ISO_INITRAMFS)"

iso: image
ifeq ($(ARCH), x86_64)
	@$(MAKE) --no-print-directory esp-for-variant ARCH=$(ARCH) VARIANT=$(VARIANT)
	@mkdir -p "$(DIST_DIR)" "$(BUILD_DIR)" "$(ISO_STAGING)"
	@test -d "$(ESP_DIR)/EFI" || (echo "ESP no encontrado en $(ESP_DIR). ¿Has compilado zCore para x86_64?"; exit 1)
	@rm -rf "$(ISO_STAGING)/EFI" && cp -a "$(ESP_DIR)/EFI" "$(ISO_STAGING)/"
	@command -v mkfs.vfat >/dev/null || (echo "falta mkfs.vfat (paquete: dosfstools)"; exit 1)
	@command -v mcopy >/dev/null || (echo "falta mcopy (paquete: mtools)"; exit 1)
	@command -v mmd >/dev/null || (echo "falta mmd (paquete: mtools)"; exit 1)
	@command -v xorriso >/dev/null || (echo "falta xorriso"; exit 1)
	@rm -f "$(ESP_DIR)/EFI/zCore/x86_64.img" "$(ESP_DIR)/EFI/zCore/aarch64.img" \
		"$(ESP_DIR)/EFI/zCore/riscv64.img"
	@rm -f "$(ESP_IMG)"
	@# Why apparent size and not blocks, and why the measurement lives in a
	@# script: scripts/esp-size-mb.sh says it, at length.
	@esp_mb=$$(sh "$(CURDIR)/scripts/esp-size-mb.sh" "$(ESP_DIR)/EFI" 96) || exit 1; \
		echo "ISO ESP: $$esp_mb MiB (sized to installer initramfs; installed efi.img.gz stays $(ESP_IMG_SIZE_MB) MiB)"; \
		dd if=/dev/zero of="$(ESP_IMG)" bs=1M count=$$esp_mb status=none
	@mkfs.vfat -F 32 "$(ESP_IMG)" >/dev/null
	@mmd -i "$(ESP_IMG)" ::/EFI ::/EFI/Boot ::/EFI/zCore >/dev/null
	@mcopy -i "$(ESP_IMG)" -s "$(ESP_DIR)/EFI" ::/ >/dev/null
	@mkdir -p "$(ISO_STAGING)/boot"
	@cp -f "$(ESP_IMG)" "$(ISO_STAGING)/boot/efi.img"
	@# Los payloads van con su NOMBRE DE SIEMPRE dentro de la ISO:
	@# install-eclipse busca /boot/rootfs.btrfs.gz, no el nombre de la variante.
	@cp -f "$(EFI_GZ)" "$(ISO_STAGING)/boot/efi.img.gz"
	@cp -f "$(ROOTFS_GZ)" "$(ISO_STAGING)/boot/rootfs.btrfs.gz"
	@cp -f "$(HOME_GZ)" "$(ISO_STAGING)/boot/home.btrfs.gz"
	@xorriso -as mkisofs \
		-R -J -V "ECLIPSE" \
		-publisher "Eclipse $(VARIANT) $(ARCH)" \
		-eltorito-alt-boot \
		-e "boot/efi.img" \
		-no-emul-boot \
		-isohybrid-gpt-basdat \
		-append_partition 2 0xef "$(ESP_IMG)" \
		-o "$(ISO_OUT)" \
		"$(ISO_STAGING)" >/dev/null
	@echo "ISO generado: $(ISO_OUT) ($(VARIANT), $(ARCH))"
else
	@$(MAKE) --no-print-directory iso-unsupported-arch
endif

# Why there is no ISO for an arch other than x86_64, said once and said
# precisely — a message that only says "no soportado" costs the next person the
# afternoon it cost to write this one.
#
# This is NOT about the ISO tooling: xorriso does not care about the
# architecture. It is the BOOT PATH. An Eclipse ISO is one single medium, so the
# kernel has to get its root filesystem from the bootloader (an initramfs loaded
# into RAM), and today only x86_64 does that:
#
#   * x86_64 boots `rboot` (ours, in `rboot/`), which reads `initramfs.img` off
#     the ESP and hands the kernel `initrd_start`/`initrd_size`
#     (zCore/src/platform/x86/entry.rs). `fs::rootfs()` then mounts that RAM
#     image, which is exactly what makes the live installer session possible.
#   * aarch64 boots a PREBUILT `rayboot` 2.0.0 binary downloaded from a 2022
#     GitHub release. Its `Boot.json` has no initramfs field at all, so
#     `init_ram_disk()` returns None and `fs::rootfs()` falls back to
#     `all_block().first_unwrap()` — the first block DEVICE. That is why
#     `make qemu ARCH=aarch64` hands QEMU two drives: a FAT one to boot from
#     and `aarch64.img` as the root. Two devices cannot be one ISO.
#   * riscv64 is booted by QEMU itself (`-kernel` + `-initrd`), with no UEFI
#     medium in the picture.
#
# So an arm64 ISO needs one of these two, and both are their own piece of work:
#   a) `rboot` ported to `aarch64-unknown-uefi` (it is x86-only today: Cr3, the
#      IDT, the `x86-interrupt` ABI, x86_64 page tables), or
#   b) rayboot replaced/extended so it loads an initramfs and passes it in
#      `platform/aarch64/entry.rs` the way the x86 one does.
#
# Everything ELSE for arm64 is already wired: `make rootfs`/`make image`
# ARCH=aarch64 build `rootfs/aarch64[-minimal]` and `zCore/aarch64[-minimal].img`
# for both variants, and `make release ARCHS="x86_64 aarch64"` walks the whole
# matrix — the day (a) or (b) lands, this target is the only thing to fill in.
.PHONY: iso-unsupported-arch
iso-unsupported-arch:
	@echo "iso: todavía no hay ISO para ARCH=$(ARCH) (solo x86_64)."
	@echo "  No es la ISO, es el arranque: una ISO es UN medio, así que el"
	@echo "  cargador tiene que pasarle al kernel un initramfs, y eso hoy solo"
	@echo "  lo hace rboot en x86_64. En aarch64 el cargador es un rayboot"
	@echo "  prebuilt de 2022 cuyo Boot.json no tiene campo de initramfs, así"
	@echo "  que el kernel coge su raíz del primer dispositivo de bloques"
	@echo "  (por eso 'make qemu ARCH=aarch64' usa DOS discos)."
	@echo "  Hace falta rboot portado a aarch64-unknown-uefi, o un rayboot que"
	@echo "  cargue el initramfs. Ver el comentario sobre este objetivo."
	@echo "  Mientras tanto SÍ se construyen, en las dos variantes:"
	@echo "    make image ARCH=$(ARCH) VARIANT=$(VARIANT)"
	@echo "    -> rootfs/$(ARCH)$(VARIANT_SUFFIX) y zCore/$(ARCH)$(VARIANT_SUFFIX).img"
	@exit 1

################ make release ################
#
# One target, every ISO. `make release` walks the (variant, arch) matrix and
# builds one ISO per combination, each named for both:
#
#   dist/eclipse-desktop-x86_64.iso
#   dist/eclipse-minimal-x86_64.iso
#   dist/eclipse-desktop-aarch64.iso     (see iso-unsupported-arch)
#   dist/eclipse-minimal-aarch64.iso     (see iso-unsupported-arch)
#
# Narrow it along either axis, and a single combination still goes through this
# same target -- there is no second code path for "just one":
#
#   make release VARIANTS=minimal               # both arches, minimal only
#   make release ARCHS=x86_64                   # both variants, x86_64 only
#   make release ARCHS=x86_64 VARIANTS=desktop  # exactly one
#
# A combination that cannot be built does NOT abort the rest: every one is
# attempted and the summary at the end says which ISOs this run produced and
# which it did not (today: arm64, see `iso-unsupported-arch`). A release run is
# half an hour per combination, so losing the x86_64 ISOs because arm64 has no
# bootloader yet would be the worst of both.
ARCHS ?= x86_64 aarch64
VARIANTS ?= desktop minimal

# What THIS run managed to build. The summary is driven off these two files and
# not off `test -f dist/...`: an ISO left in dist/ by an earlier run would
# otherwise be reported as OK for a combination that just failed, which is the
# one mistake a release summary must not make.
RELEASE_BUILT := $(BUILD_DIR)/release-built.txt
RELEASE_FAILED := $(BUILD_DIR)/release-failed.txt

release:
	@mkdir -p "$(DIST_DIR)" "$(BUILD_DIR)"
	@rm -f "$(RELEASE_BUILT)" "$(RELEASE_FAILED)"
	@echo "=== make release: arquitecturas [$(ARCHS)] x variantes [$(VARIANTS)] ==="
	@for a in $(ARCHS); do \
	  for v in $(VARIANTS); do \
	    echo ""; \
	    echo "=== release: $$v / $$a ==="; \
	    if $(MAKE) --no-print-directory iso ARCH=$$a VARIANT=$$v LOG=$(LOG) GL=$(GL); then \
	      echo "$$v $$a" >> "$(RELEASE_BUILT)"; \
	      echo "=== release: $$v / $$a OK ==="; \
	    else \
	      echo "$$v $$a" >> "$(RELEASE_FAILED)"; \
	      echo "=== release: $$v / $$a FALLÓ (sigo con el resto) ==="; \
	    fi; \
	  done; \
	done
	@echo ""
	@echo "=== make release: resumen ==="
	@for a in $(ARCHS); do \
	  for v in $(VARIANTS); do \
	    iso="$(DIST_DIR)/eclipse-$$v-$$a.iso"; \
	    if grep -qx "$$v $$a" "$(RELEASE_BUILT)" 2>/dev/null; then \
	      sz=$$({ du -h --apparent-size "$$iso" 2>/dev/null \
	              || du -h "$$iso" 2>/dev/null; } | cut -f1); \
	      [ -n "$$sz" ] || sz="tamaño desconocido"; \
	      echo "  OK      $$iso ($$sz)"; \
	    elif [ -f "$$iso" ]; then \
	      echo "  FALLÓ   $$v/$$a; el fichero que hay en dist/ es de otra tirada"; \
	    else \
	      echo "  FALLÓ   $$v/$$a; no hay ISO"; \
	    fi; \
	  done; \
	done
	@if [ -s "$(RELEASE_FAILED)" ]; then \
	  echo ""; \
	  echo "Sin construir: $$(tr '\n' ' ' < "$(RELEASE_FAILED)" | sed 's/ $$//')"; \
	  echo "(para arm64, el motivo exacto: make iso-unsupported-arch ARCH=aarch64)"; \
	  exit 1; \
	fi

qcow2: image
ifeq ($(ARCH), x86_64)
	@mkdir -p "$(DIST_DIR)" "$(BUILD_DIR)"
	@# Build the ESP for THIS variant instead of packaging whatever the last
	@# zCore build left in a path that has no variant in it.
	@$(MAKE) --no-print-directory esp-for-variant ARCH=$(ARCH) VARIANT=$(VARIANT)
	@test -d "$(ESP_DIR)/EFI" || (echo "ESP no encontrado en $(ESP_DIR). ¿Has compilado zCore para x86_64?"; exit 1)
	@command -v mkfs.vfat >/dev/null || (echo "falta mkfs.vfat (paquete: dosfstools)"; exit 1)
	@command -v mcopy >/dev/null || (echo "falta mcopy (paquete: mtools)"; exit 1)
	@command -v mmd >/dev/null || (echo "falta mmd (paquete: mtools)"; exit 1)
	@command -v qemu-img >/dev/null || (echo "falta qemu-img"; exit 1)
	@rm -f "$(ESP_DIR)/EFI/zCore/x86_64.img" "$(ESP_DIR)/EFI/zCore/aarch64.img" \
		"$(ESP_DIR)/EFI/zCore/riscv64.img"
	@rm -f "$(ESP_IMG)"
	@# Why apparent size and not blocks, and why the measurement lives in a
	@# script: scripts/esp-size-mb.sh says it, at length.
	@esp_mb=$$(sh "$(CURDIR)/scripts/esp-size-mb.sh" "$(ESP_DIR)/EFI" 96) || exit 1; \
		[ "$$esp_mb" -ge "$(ESP_IMG_SIZE_MB)" ] || esp_mb=$(ESP_IMG_SIZE_MB); \
		echo "ESP: $$esp_mb MiB"; \
		dd if=/dev/zero of="$(ESP_IMG)" bs=1M count=$$esp_mb status=none
	@mkfs.vfat -F 32 "$(ESP_IMG)" >/dev/null
	@mmd -i "$(ESP_IMG)" ::/EFI ::/EFI/Boot ::/EFI/zCore >/dev/null
	@mcopy -i "$(ESP_IMG)" -s "$(ESP_DIR)/EFI" ::/ >/dev/null
	@qemu-img convert -f raw "$(ESP_IMG)" -O qcow2 "$(QCOW2_OUT)" >/dev/null
	@echo "qcow2 generado: $(QCOW2_OUT)"
else
	@echo "qcow2: solo soportado para ARCH=x86_64 por ahora"
	@exit 1
endif

img: image
ifeq ($(ARCH), x86_64)
	@mkdir -p "$(DIST_DIR)" "$(BUILD_DIR)"
	@# Build the ESP for THIS variant instead of packaging whatever the last
	@# zCore build left in a path that has no variant in it.
	@$(MAKE) --no-print-directory esp-for-variant ARCH=$(ARCH) VARIANT=$(VARIANT)
	@test -d "$(ESP_DIR)/EFI" || (echo "ESP no encontrado en $(ESP_DIR). ¿Has compilado zCore para x86_64?"; exit 1)
	@command -v sgdisk >/dev/null || (echo "falta sgdisk (paquete: gdisk)"; exit 1)
	@command -v mformat >/dev/null || (echo "falta mformat (paquete: mtools)"; exit 1)
	@command -v mcopy >/dev/null || (echo "falta mcopy (paquete: mtools)"; exit 1)
	@command -v mmd >/dev/null || (echo "falta mmd (paquete: mtools)"; exit 1)
	@rm -f "$(ESP_DIR)/EFI/zCore/x86_64.img" "$(ESP_DIR)/EFI/zCore/aarch64.img" \
		"$(ESP_DIR)/EFI/zCore/riscv64.img"
	@rm -f "$(DISK_IMG)"
	@# Why apparent size and not blocks, and why the measurement lives in a
	@# script: scripts/esp-size-mb.sh says it, at length.
	@esp_mb=$$(sh "$(CURDIR)/scripts/esp-size-mb.sh" "$(ESP_DIR)/EFI" 96) || exit 1; \
		[ "$$esp_mb" -ge "$(ESP_IMG_SIZE_MB)" ] || esp_mb=$(ESP_IMG_SIZE_MB); \
		disk_mb=$$((esp_mb + 8)); \
		echo "disk: $$disk_mb MiB (ESP $$esp_mb MiB)"; \
		dd if=/dev/zero of="$(DISK_IMG)" bs=1M count=$$disk_mb status=none; \
		sgdisk -o "$(DISK_IMG)" >/dev/null; \
		sgdisk -n 1:2048:+$${esp_mb}M -t 1:ef00 -c 1:EFI "$(DISK_IMG)" >/dev/null
	@# FAT32 ESP starts at 2048 * 512 = 1048576 bytes (1MiB)
	@mformat -i "$(DISK_IMG)@@1048576" -F -v EFI :: >/dev/null
	@mmd -i "$(DISK_IMG)@@1048576" ::/EFI ::/EFI/Boot ::/EFI/zCore >/dev/null
	@mcopy -i "$(DISK_IMG)@@1048576" -s "$(ESP_DIR)/EFI" ::/ >/dev/null
	@cp -f "$(DISK_IMG)" "$(IMG_OUT)"
	@echo "img generado: $(IMG_OUT)"
else
	@echo "img: solo soportado para ARCH=x86_64 por ahora"
	@exit 1
endif
