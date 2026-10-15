#!/usr/bin/env bash
#
# eclipsefs-linux-bench.sh — EclipseFS against the filesystems of the Linux
# kernel (ext4, xfs, btrfs), with the same tool and the same workloads.
#
# Every filesystem gets a fresh image of the same size, in the same directory
# (same disk), attached to a loop device; the Linux ones are mounted by the
# kernel, EclipseFS by tools/eclipsefs-fuse (the same driver the Eclipse kernel
# uses). fio and tar then run the same workloads on each mount point, with the
# page cache dropped before every cold read. Each workload runs RUNS times on
# a fresh filesystem and the median is reported.
#
# EclipseFS goes through FUSE, which costs a trip to user space per request
# that the kernel filesystems do not pay. The `ext4-fuse` row measures that
# cost: the same ext4 seen through a FUSE passthrough (bindfs). Compare
# EclipseFS with it to judge the design, and with ext4/xfs/btrfs for what a
# user of Linux would see today.
#
# Needs root, fio, mkfs.ext4/mkfs.xfs/mkfs.btrfs, bindfs, fuse3 and a kernel
# that can mount each filesystem; a filesystem the kernel cannot mount is
# reported as such and skipped.
#
# Usage:
#   sudo scripts/eclipsefs-linux-bench.sh
#   sudo FS_LIST="ext4 eclipsefs" RUNS=5 scripts/eclipsefs-linux-bench.sh
#
# Environment:
#   FS_LIST   filesystems to test (default: ext4 xfs btrfs eclipsefs ext4-fuse)
#   IMG_SIZE  image size (default: 4G)
#   RUNS      runs per workload, median reported (default: 3)
#   WORKDIR   where images go (default: /var/tmp/eclipsefs-linux-bench)
#   OUT       markdown table output (default: $WORKDIR/results.md)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FS_LIST="${FS_LIST:-ext4 xfs btrfs eclipsefs ext4-fuse}"
IMG_SIZE="${IMG_SIZE:-4G}"
RUNS="${RUNS:-3}"
WORKDIR="${WORKDIR:-/var/tmp/eclipsefs-linux-bench}"
OUT="${OUT:-$WORKDIR/results.md}"
FUSE_BIN="$ROOT/tools/eclipsefs-fuse/target/release/eclipsefs-fuse"

IMG="$WORKDIR/fs.img"
MNT="$WORKDIR/mnt"
LOWER="$WORKDIR/lower"
RAW="$WORKDIR/raw.tsv"
TARBALL="$WORKDIR/tree.tar"

die() { echo "error: $*" >&2; exit 1; }
log() { echo "== $*" >&2; }

[ "$(id -u)" = 0 ] || die "needs root (loop devices, mount, drop_caches)"
for t in fio losetup mount python3; do
    command -v "$t" >/dev/null || die "$t not found"
done

mkdir -p "$WORKDIR" "$MNT" "$LOWER"
: > "$RAW"

if [[ " $FS_LIST " == *" eclipsefs "* ]]; then
    log "building tools/eclipsefs-fuse"
    (cd "$ROOT/tools/eclipsefs-fuse" && cargo build --release --quiet)
fi

# The source tree the `tar` workloads unpack: this repository at HEAD.
git -C "$ROOT" archive --format=tar -o "$TARBALL" HEAD
TREE_FILES=$(tar -tf "$TARBALL" | grep -vc '/$')
TREE_MIB=$(( $(stat -c %s "$TARBALL") / 1048576 ))

LOOP=""
FUSE_PID=""

drop_caches() {
    sync
    echo 3 > /proc/sys/vm/drop_caches
}

now() { date +%s.%N; }

# Mount a fresh $1 on $MNT.
setup() {
    local fs=$1
    rm -f "$IMG"
    truncate -s "$IMG_SIZE" "$IMG"
    LOOP=$(losetup --find --show "$IMG")
    case "$fs" in
    ext4 | ext4-fuse) mkfs.ext4 -q -F "$LOOP" ;;
    xfs) mkfs.xfs -q -f "$LOOP" ;;
    btrfs) mkfs.btrfs -q -f "$LOOP" ;;
    esac
    case "$fs" in
    ext4 | xfs | btrfs)
        mount -t "$fs" "$LOOP" "$MNT" 2>/dev/null || return 1
        ;;
    ext4-fuse)
        mount -t ext4 "$LOOP" "$LOWER" || return 1
        bindfs --multithreaded "$LOWER" "$MNT" || return 1
        ;;
    eclipsefs)
        "$FUSE_BIN" --mkfs "$LOOP" "$MNT" &
        FUSE_PID=$!
        for _ in $(seq 100); do
            mountpoint -q "$MNT" && break
            kill -0 "$FUSE_PID" 2>/dev/null || return 1
            sleep 0.05
        done
        mountpoint -q "$MNT" || return 1
        ;;
    esac
}

teardown() {
    sync
    if mountpoint -q "$MNT"; then umount "$MNT"; fi
    if [ -n "$FUSE_PID" ]; then
        wait "$FUSE_PID" 2>/dev/null || true
        FUSE_PID=""
    fi
    if mountpoint -q "$LOWER"; then umount "$LOWER"; fi
    if [ -n "$LOOP" ]; then
        losetup -d "$LOOP"
        LOOP=""
    fi
    rm -f "$IMG"
}
trap teardown EXIT

# record FS WORKLOAD VALUE UNIT
record() { printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >> "$RAW"; }

# fio_run FS WORKLOAD METRIC UNIT fio-args... — METRIC is a python expression
# over `r`/`w` (the job's read/write sections) and `j` (the job).
fio_run() {
    local fs=$1 name=$2 metric=$3 unit=$4
    shift 4
    local v
    v=$(fio --name="$name" --directory="$MNT" --output-format=json "$@" |
        python3 -c "import json,sys
j=json.load(sys.stdin)['jobs'][0]; r=j['read']; w=j['write']
print($metric)")
    record "$fs" "$name" "$v" "$unit"
}

MIB_S="(r['bw_bytes']+w['bw_bytes'])/1048576"
IOPS="r['iops']+w['iops']"

workloads() {
    local fs=$1 t0 t1

    # Sequential: 1 GiB in 1 MiB requests, counted until it is on disk.
    fio_run "$fs" seq-write "$MIB_S" MiB/s --filename=seq \
        --rw=write --bs=1M --size=1G --end_fsync=1
    drop_caches
    fio_run "$fs" seq-read "$MIB_S" MiB/s --filename=seq \
        --rw=read --bs=1M --size=1G
    rm -f "$MNT/seq"

    # Random 4 KiB over a 512 MiB file (laid out first, not timed).
    fio_run "$fs" rand-write "$IOPS" IOPS --filename=rand \
        --rw=randwrite --bs=4k --size=512M --io_size=128M --end_fsync=1
    drop_caches
    fio_run "$fs" rand-read "$IOPS" IOPS --filename=rand \
        --rw=randread --bs=4k --size=512M --io_size=32M
    rm -f "$MNT/rand"

    # A database log: every 4 KiB write is fsync'ed.
    fio_run "$fs" fsync-write "$IOPS" IOPS --filename=log \
        --rw=write --bs=4k --size=8M --fsync=1
    rm -f "$MNT/log"

    # Metadata: create, stat and delete 10000 empty files in one directory.
    mkdir "$MNT/meta"
    local meta=(--directory="$MNT/meta" --nrfiles=10000 --filesize=4k --openfiles=1
        --filename_format='f.$filenum')
    fio_run "$fs" create "$IOPS" files/s "${meta[@]}" --ioengine=filecreate
    drop_caches
    fio_run "$fs" stat "$IOPS" files/s "${meta[@]}" --ioengine=filestat
    fio_run "$fs" delete "$IOPS" files/s "${meta[@]}" --ioengine=filedelete
    rmdir "$MNT/meta"

    # A real tree: unpack this repository and get it to disk, then remove it.
    drop_caches
    t0=$(now)
    tar -xf "$TARBALL" -C "$MNT"
    sync -f "$MNT"
    t1=$(now)
    record "$fs" untar "$(python3 -c "print($t1-$t0)")" s
    drop_caches
    t0=$(now)
    find "$MNT" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
    sync -f "$MNT"
    t1=$(now)
    record "$fs" rm-tree "$(python3 -c "print($t1-$t0)")" s
}

for fs in $FS_LIST; do
    case "$fs" in
    ext4 | xfs | btrfs | eclipsefs | ext4-fuse) ;;
    *) die "unknown filesystem: $fs" ;;
    esac
    for run in $(seq "$RUNS"); do
        log "$fs: run $run/$RUNS"
        if ! setup "$fs"; then
            log "$fs: cannot be mounted here, skipped"
            record "$fs" unavailable 0 -
            teardown
            continue 2
        fi
        workloads "$fs"
        teardown
    done
done

python3 - "$RAW" "$FS_LIST" "$IMG_SIZE" "$RUNS" "$TREE_FILES" "$TREE_MIB" > "$OUT" <<'EOF'
import statistics, sys
raw, fs_list, img, runs, tree_files, tree_mib = sys.argv[1:]
fs_list = fs_list.split()
vals, units, missing = {}, {}, set()
for line in open(raw):
    fs, wl, v, unit = line.rstrip("\n").split("\t")
    if wl == "unavailable":
        missing.add(fs)
        continue
    vals.setdefault((wl, fs), []).append(float(v))
    units[wl] = unit
cols = [f for f in fs_list if f not in missing]
names = {
    "seq-write": "Escritura secuencial 1 GiB (1 MiB)",
    "seq-read": "Lectura secuencial en frío 1 GiB (1 MiB)",
    "rand-write": "Escritura aleatoria 4 KiB",
    "rand-read": "Lectura aleatoria 4 KiB en frío",
    "fsync-write": "Escritura 4 KiB + fsync cada una",
    "create": "Crear 10000 archivos",
    "stat": "stat de 10000 archivos",
    "delete": "Borrar 10000 archivos",
    "untar": f"Descomprimir el repo ({tree_files} archivos, {tree_mib} MiB) + sync",
    "rm-tree": "Borrar ese árbol + sync",
}
print(f"Imagen de {img} por sistema de archivos en un dispositivo loop; mediana de {runs} ejecuciones.\n")
print("| Carga | Unidad | " + " | ".join(cols) + " |")
print("|---|---|" + "---|" * len(cols))
for wl, label in names.items():
    if not any((wl, f) in vals for f in cols):
        continue
    unit = units[wl]
    lower_better = unit == "s"
    med = {f: statistics.median(vals[(wl, f)]) for f in cols if (wl, f) in vals}
    best = (min if lower_better else max)(med.values())
    cells = []
    for f in cols:
        if f not in med:
            cells.append("—")
            continue
        v = med[f]
        s = f"{v:.2f}" if v < 100 else f"{v:,.0f}"
        cells.append(f"**{s}**" if v == best else s)
    arrow = "↓" if lower_better else "↑"
    print(f"| {label} | {unit} {arrow} | " + " | ".join(cells) + " |")
if missing:
    print(f"\nNo se pudieron montar en este kernel: {', '.join(sorted(missing))}.")
EOF

cat "$OUT"
