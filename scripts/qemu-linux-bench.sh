#!/usr/bin/env bash
#
# qemu-linux-bench.sh — the Linux control for scripts/qemu-bench.sh.
#
# Boots a stock Linux kernel under *the same* QEMU machine (same -cpu, -smp,
# -m, same TCG emulation, no KVM) with a minimal busybox initramfs, and runs the
# same commands on the same kind of serial console.
#
# This is the only comparison that settles anything. Running eclipse-bench on
# the host and on an emulated Eclipse compares an emulated CPU against a real
# one, which tells you nothing about the two kernels. Running both kernels under
# identical emulation cancels the hardware out.
#
# Usage:
#   scripts/qemu-linux-bench.sh [-o OUTFILE] [-t TIMEOUT] [-s SMP] [-m MEM]
#                               [-k KERNEL] CMD [CMD...]
#
#   -k KERNEL   bzImage to boot (default: /boot/vmlinuz, or $LINUX_KERNEL)
#   -g          stage the DRM driver stack so eclipse-bench's `gfx` section has
#               a /dev/dri/card0 to measure. Both harnesses give the guest the
#               SAME emulated GPU (QEMU's default std VGA on q35 — neither
#               script passes -vga or -nodefaults), but Eclipse binds it from
#               its own driver while a distro kernel keeps bochs-drm modular,
#               so without this the Linux side reports "no DRM device" and
#               there is nothing to compare.
#
# The initramfs is built from the busybox that `cargo rootfs` already compiled
# (ignored/target/x86_64/busybox/busybox) plus the eclipse-bench binary from
# rootfs/x86_64/bin, so guest userland is byte-identical to Eclipse's.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUSYBOX="$ROOT/ignored/target/x86_64/busybox/busybox"
BENCH="$ROOT/rootfs/x86_64/bin/eclipse-bench"

OUTFILE=""
TIMEOUT=1800
SMP=4
MEM=4G
KERNEL="${LINUX_KERNEL:-/boot/vmlinuz}"

GRAPHICS=0

while getopts "o:t:s:m:k:d:g" opt; do
    case "$opt" in
        o) OUTFILE="$OPTARG" ;;
        t) TIMEOUT="$OPTARG" ;;
        s) SMP="$OPTARG" ;;
        m) MEM="$OPTARG" ;;
        k) KERNEL="$OPTARG" ;;
        d) DISK_IMG="$OPTARG" ;;
        g) GRAPHICS=1 ;;
        *) echo "usage: $0 [-o OUT] [-t TIMEOUT] [-s SMP] [-m MEM] [-k KERNEL] [-d DISK] [-g] CMD..." >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -gt 0 ] || { echo "$0: no commands given" >&2; exit 2; }
[ -f "$KERNEL" ] || { echo "$0: no kernel at $KERNEL (set -k or \$LINUX_KERNEL)" >&2; exit 1; }
[ -x "$BUSYBOX" ] || { echo "$0: no busybox at $BUSYBOX — run: cargo rootfs --arch x86_64" >&2; exit 1; }
[ -x "$BENCH" ] || { echo "$0: no eclipse-bench at $BENCH — run: cargo rootfs --arch x86_64" >&2; exit 1; }

WORK="$(mktemp -d)"
[ -n "$OUTFILE" ] || OUTFILE="$WORK/console.log"
FIFO="$WORK/stdin.fifo"
mkfifo "$FIFO"

QEMU_PID=""
cleanup() {
    [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null
    exec 3>&- 2>/dev/null
    rm -rf "$WORK" 2>/dev/null
}
trap cleanup EXIT INT TERM

# ── initramfs ────────────────────────────────────────────────────────────────
IRD="$WORK/initramfs"
mkdir -p "$IRD"/{bin,dev,proc,sys,tmp,root,run}
cp "$BUSYBOX" "$IRD/bin/busybox"
cp "$BENCH" "$IRD/bin/eclipse-bench"
( cd "$IRD/bin" && for a in sh ls cat mount umount echo sleep nproc uname poweroff dmesg grep mkdir sync insmod; do
      ln -sf busybox "$a"
  done )

# When a disk is attached, stage the btrfs module stack (the distro kernel has
# it modular, not builtin) so the guest can `insmod` it before mounting.
# Decompressed to plain .ko because busybox insmod does not grok .ko.zst.
MODLINES=""
if [ -n "${DISK_IMG:-}" ]; then
    KREL="$(ls /lib/modules | head -1)"
    MODDIR="/lib/modules/$KREL/kernel"
    mkdir -p "$IRD/lib/modules"
    # Storage first (libahci, ahci — the disk controller is modular in this
    # kernel), then the btrfs filesystem stack. sd_mod/libata are builtin.
    for m in drivers/ata/libahci drivers/ata/ahci \
             lib/libcrc32c lib/raid6/raid6_pq crypto/xor crypto/blake2b_generic fs/btrfs/btrfs; do
        src="$MODDIR/$m.ko.zst"
        [ -f "$src" ] || src="$MODDIR/$m.ko"
        base="$(basename "$m")"
        if [ -f "$src" ]; then
            if echo "$src" | grep -q '\.zst$'; then
                zstd -d -q -c "$src" > "$IRD/lib/modules/$base.ko"
            else
                cp "$src" "$IRD/lib/modules/$base.ko"
            fi
            MODLINES="${MODLINES}/bin/insmod /lib/modules/$base.ko 2>/dev/null
"
        fi
    done
fi

# Same idea for the graphics comparison (-g): the DRM driver for QEMU's std VGA
# is modular in a distro kernel, so without it the guest has no /dev/dri at all
# and eclipse-bench's `gfx` section has nothing to measure. `modprobe
# --show-depends` resolves the dependency ORDER for us, which matters because
# busybox has insmod but not modprobe.
if [ "$GRAPHICS" = 1 ]; then
    KREL="${KREL:-$(ls /lib/modules | head -1)}"
    mkdir -p "$IRD/lib/modules"
    stage_mod() {
        # $1: absolute .ko / .ko.zst path
        base="$(basename "$1")"; base="${base%.zst}"; base="${base%.ko}"
        [ -f "$IRD/lib/modules/$base.ko" ] && return 0
        case "$1" in
            *.zst) zstd -d -q -c "$1" > "$IRD/lib/modules/$base.ko" || return 1 ;;
            *)     cp "$1" "$IRD/lib/modules/$base.ko" || return 1 ;;
        esac
        MODLINES="${MODLINES}/bin/insmod /lib/modules/$base.ko 2>/dev/null
"
    }
    staged=0
    for drv in bochs bochs-drm virtio-gpu vmwgfx; do
        deps="$(modprobe -S "$KREL" -n --show-depends "$drv" 2>/dev/null                 | awk '$1=="insmod"{print $2}')"
        [ -n "$deps" ] || continue
        for ko in $deps; do stage_mod "$ko"; done
        staged=1
        break
    done
    if [ "$staged" = 0 ]; then
        echo "$0: warning: no DRM driver found for kernel $KREL — the guest will" >&2
        echo "$0: have no /dev/dri and the gfx section cannot be compared." >&2
    fi
fi

cat > "$IRD/init" <<'EOF'
#!/bin/sh
/bin/busybox mount -t proc  proc  /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs dev /dev 2>/dev/null
/bin/busybox mount -t tmpfs tmpfs /tmp
export PATH=/bin
__MODLINES__
/bin/busybox sleep 1
# Match the prompt the Eclipse harness waits for, so both logs parse the same.
exec /bin/busybox sh -i
EOF
# Splice the accumulated insmod lines in place of the literal marker (the init
# heredoc is quoted, so the marker was written verbatim).
awk -v ml="$MODLINES" '{ if ($0 == "__MODLINES__") printf "%s", ml; else print }' \
    "$IRD/init" > "$IRD/init.tmp" && mv "$IRD/init.tmp" "$IRD/init"
chmod +x "$IRD/init"

( cd "$IRD" && find . | cpio -o -H newc --quiet | gzip -9 ) > "$WORK/initramfs.cpio.gz"

# ── boot ─────────────────────────────────────────────────────────────────────
# Same machine, CPU, core count and memory as scripts/qemu-bench.sh, and equally
# without KVM. `quiet loglevel=0` keeps kernel chatter out of the captured log.
# `+invtsc` (CPUID.80000007H:EDX[8]) is on BOTH harnesses deliberately. Eclipse
# only lets userspace read the TSC directly when the counter advertises itself
# as invariant -- constant-rate and reset-synchronized across cores -- because
# without that guarantee a thread that migrates can see time run backwards, and
# userspace cannot participate in the kernel's monotonic floor. QEMU's stock
# Haswell model does not advertise it, so the vDSO would sit disabled here while
# staying active on every real post-2008 x86. Linux is looser (it infers a
# constant rate from the CPU model) and was already serving clock_gettime from
# its vDSO without the flag, so adding it changes Eclipse's path and not Linux's
# -- but it goes on both, because the two must run on the same machine.
# Dedicated AHCI disk (`-d IMG`) mirroring scripts/qemu-bench.sh: same
# controller, same raw image, so the disk under test is identical on both
# kernels. The Linux guest mounts it in the commands the caller passes.
DISK_ARGS=""
if [ -n "${DISK_IMG:-}" ]; then
    [ -f "$DISK_IMG" ] || { echo "$0: no disk image at $DISK_IMG" >&2; exit 1; }
    DISK_ARGS="-device ich9-ahci,id=ahcibench -drive id=benchdisk,if=none,format=raw,file=$DISK_IMG -device ide-hd,drive=benchdisk,bus=ahcibench.0"
fi

qemu-system-x86_64 \
    -smp "$SMP" \
    -machine q35 \
    -cpu Haswell,+smap,-check,-fsgsbase,+invtsc \
    -m "$MEM" \
    -serial mon:stdio \
    -kernel "$KERNEL" \
    -initrd "$WORK/initramfs.cpio.gz" \
    -append "console=ttyS0 quiet loglevel=0 rdinit=/init" \
    -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 \
    $DISK_ARGS \
    -nic none \
    -display none \
    -no-reboot \
    < "$FIFO" > "$OUTFILE" 2>&1 &
QEMU_PID=$!

exec 3>"$FIFO"

deadline=$(( $(date +%s) + TIMEOUT ))

wait_for() {
    local pattern="$1" count="$2"
    while :; do
        if [ "$(grep -c -- "$pattern" "$OUTFILE" 2>/dev/null)" -ge "$count" ]; then
            return 0
        fi
        if ! kill -0 "$QEMU_PID" 2>/dev/null; then
            echo "$0: QEMU exited before '$pattern' appeared" >&2
            return 1
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            # Re-check before giving up: the poll below sleeps a second, so a
            # marker that lands inside that window would otherwise be reported
            # as a timeout on a run that actually completed.
            if [ "$(grep -c -- "$pattern" "$OUTFILE" 2>/dev/null)" -ge "$count" ]; then
                return 0
            fi
            echo "$0: timed out waiting for '$pattern'" >&2
            return 1
        fi
        sleep 1
    done
}

wait_for '#' 1 || exit 1

# The marker is split by a quote in the command we *type* ("__ECLIPSE""_BENCH..")
# so the line the shell echoes back can never match it — only the shell's own
# `echo` produces the joined string. Counting occurrences instead would be
# fragile: whether the console echoes typed input at all varies with how the
# guest set the terminal up, and getting that wrong reports a completed run as
# a timeout.
n=0
for cmd in "$@"; do
    n=$((n + 1))
    marker="__ECLIPSE_BENCH_DONE_${n}__"
    printf '%s; echo "__ECLIPSE""_BENCH_DONE_%d__"\n' "$cmd" "$n" >&3
    wait_for "$marker" 1 || exit 1
done

printf 'poweroff -f\n' >&3
sleep 3
kill "$QEMU_PID" 2>/dev/null
wait "$QEMU_PID" 2>/dev/null

echo "console log: $OUTFILE" >&2
