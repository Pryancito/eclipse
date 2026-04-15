# Makefile for building bash-5.3 for Eclipse OS
#
# Prerequisites:
#   - x86_64-linux-gnu cross-toolchain (gcc-x86-64-linux-gnu)
#   - eclipse-relibc built and installed in $(SYSROOT)
#   - bash 5.3 source tarball downloaded to $(BASH_SRC_DIR)
#
# Usage:
#   make download   -- download and extract bash 5.3
#   make configure  -- configure bash for Eclipse OS
#   make build      -- build bash binary
#   make install    -- copy binary to $(OUTDIR)/bin/bash
#   make all        -- download + configure + build + install

BASH_VERSION  := 5.3
BASH_TARBALL  := bash-$(BASH_VERSION).tar.gz
BASH_URL      := https://ftp.gnu.org/gnu/bash/$(BASH_TARBALL)
BASH_SRC_DIR  := bash-$(BASH_VERSION)

# Output directory for the compiled binary
OUTDIR        := bin

# Path to the eclipse-relibc build artefact (static library)
# After building eclipse-relibc with:
#   cargo +nightly build -p eclipse-libc --release \
#       --target x86_64-unknown-linux-gnu \
#       -Z build-std=core,alloc
# the library ends up in target/x86_64-unknown-linux-gnu/release/liblibc.a
RELIBC_DIR    := $(abspath ../../eclipse-relibc)
RELIBC_LIB    := $(RELIBC_DIR)/target/x86_64-unknown-linux-gnu/release/liblibc.a

# Cross-compiler settings.  We use a plain x86_64-linux-gnu gcc and override
# the sysroot / libc so that bash links against eclipse-relibc instead of
# the host glibc.
CC            := musl-gcc
AR            := ar
CFLAGS        := -O2 -static -nostdlib -nostdinc \
                 -I$(RELIBC_DIR)/include \
                 -D_GNU_SOURCE \
                 -DECLIPSE_OS \
                 -ffreestanding \
                 -fno-pie -no-pie \
                 -m64

LDFLAGS       := -static -nostdlib -no-pie \
                 -Wl,--allow-multiple-definition \
                 $(RELIBC_LIB) \
                 -lgcc

CONFIGURE_ENV := \
    CC="$(CC)" \
    AR="$(AR)" \
    CFLAGS="$(CFLAGS)" \
    LDFLAGS="$(LDFLAGS)" \
    ac_cv_func_getpwent=no \
    ac_cv_func_getpwnam=no \
    ac_cv_func_getpwuid=no \
    ac_cv_func_getgrent=no \
    ac_cv_func_getgroups=yes \
    ac_cv_func_sysconf=no \
    ac_cv_func_pathconf=no \
    ac_cv_func_fpathconf=no \
    ac_cv_func_getrlimit=yes \
    ac_cv_func_setrlimit=yes \
    ac_cv_func_mmap=yes \
    ac_cv_func_munmap=yes \
    ac_cv_func_waitpid=yes \
    ac_cv_func_wait3=yes \
    ac_cv_func_wait4=yes \
    ac_cv_func_execve=yes \
    ac_cv_func_execvp=yes \
    ac_cv_func_pipe=yes \
    ac_cv_func_dup2=yes \
    ac_cv_func_fcntl=yes \
    ac_cv_func_opendir=yes \
    ac_cv_func_readdir=yes \
    ac_cv_func_chdir=yes \
    ac_cv_func_getcwd=yes \
    ac_cv_func_access=yes \
    ac_cv_func_tcgetpgrp=yes \
    ac_cv_func_tcsetpgrp=yes \
    ac_cv_func_setpgid=yes \
    ac_cv_func_setsid=yes \
    ac_cv_func_sigaction=yes \
    ac_cv_func_sigprocmask=yes \
    ac_cv_func_select=yes \
    ac_cv_func_poll=yes \
    ac_cv_func_stat=yes \
    ac_cv_func_lstat=yes \
    ac_cv_func_mkdir=yes \
    ac_cv_func_unlink=yes \
    ac_cv_func_rename=yes \
    ac_cv_func_strdup=yes \
    ac_cv_func_strndup=yes \
    ac_cv_func_strerror=yes \
    ac_cv_func_strsignal=yes \
    ac_cv_func_strcasecmp=yes \
    ac_cv_func_strncasecmp=yes \
    ac_cv_func_getenv=yes \
    ac_cv_func_setenv=yes \
    ac_cv_func_putenv=yes \
    ac_cv_func_unsetenv=yes \
    ac_cv_func_isatty=yes \
    ac_cv_func_ttyname=no \
    ac_cv_func_getlogin=no \
    ac_cv_func_getlogin_r=yes \
    ac_cv_func_getpgrp=yes \
    ac_cv_func_getpgid=yes \
    ac_cv_func_fork=yes \
    ac_cv_func_vfork=no \
    ac_cv_sys_restartable_syscalls=yes \
    bash_cv_getcwd_malloc=yes \
    bash_cv_job_control_missing=present \
    bash_cv_func_sigsetjmp=present \
    bash_cv_func_snprintf=yes \
    bash_cv_func_vsnprintf=yes

CONFIGURE_FLAGS := \
    --host=x86_64-unknown-linux-musl \
    --build=x86_64-pc-linux-gnu \
    --without-bash-malloc \
    --disable-nls \
    --disable-rpath \
    --enable-static-link \
    --without-curses

.PHONY: all download configure build install clean

all: download configure build install

download:
	@echo "==> Downloading bash $(BASH_VERSION)..."
	test -f $(BASH_TARBALL) || wget -q $(BASH_URL) -O $(BASH_TARBALL)
	test -d $(BASH_SRC_DIR) || tar xzf $(BASH_TARBALL)
	@echo "==> Done: $(BASH_SRC_DIR)"

configure: download
	@echo "==> Configuring bash for Eclipse OS..."
	cd $(BASH_SRC_DIR) && $(CONFIGURE_ENV) ./configure $(CONFIGURE_FLAGS)
	@echo "==> Configuration done"

build: configure
	@echo "==> Building bash..."
	cd $(BASH_SRC_DIR) && $(MAKE)
	@echo "==> Build done: $(BASH_SRC_DIR)/bash"

install: build
	@echo "==> Installing bash to $(OUTDIR)/..."
	mkdir -p $(OUTDIR)
	cp $(BASH_SRC_DIR)/bash $(OUTDIR)/bash
	@echo "==> Installed: $(OUTDIR)/bash"

clean:
	rm -rf $(BASH_SRC_DIR) $(BASH_TARBALL) $(OUTDIR)

# Build eclipse-relibc first (prerequisite for bash build).
relibc:
	@echo "==> Building eclipse-relibc static library..."
	cd $(RELIBC_DIR) && \
	    cargo +nightly build --release \
	        --target x86_64-unknown-linux-gnu \
	        -Z build-std=core,alloc
	@echo "==> eclipse-relibc built: $(RELIBC_LIB)"
