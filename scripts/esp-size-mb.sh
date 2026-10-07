#!/bin/sh
# Prints the size a FAT32 ESP needs for a directory tree, in MiB: the tree's
# APPARENT size plus the headroom given as the second argument.
#
#   esp-size-mb.sh <dir> <headroom-MiB>
#
# APPARENT size, not allocated blocks: the SFS initramfs is written SPARSE
# (`set_len` and nothing else), so a plain `du -sm` counts it at about half,
# the FAT32 comes out short, and mcopy says "Disk full" exactly when it copies
# BootX64.efi -- which goes last. The result is an image with nothing to boot
# from, and on the guest console it reads like a hung kernel.
#
# This lives in one file because the same calculation had been copied into
# zCore/Makefile and three recipes of the root Makefile, and the copies drifted:
# zCore's was right and the other three were measuring blocks.
#
# The pipeline is the other half of the lesson. This was the shape everywhere:
#
#     esp_mb=$(du -sm --apparent-size "$d" 2>/dev/null | cut -f1 \
#              || du -sm "$d" | cut -f1)
#
# `||` binds to the whole PIPELINE, and a pipeline's status is its LAST
# command. On a `du` without `--apparent-size` (macOS, busybox) the first `du`
# fails, its stderr is swallowed, `cut` reads EOF and exits 0 -- so the
# pipeline SUCCEEDS with empty output and the fallback never runs. `esp_mb` is
# then empty, `$((esp_mb + 96))` is 96 under POSIX arithmetic, and the ESP
# comes out at 96 MiB: the exact failure the --apparent-size fix existed to
# prevent, reintroduced by the fallback meant to be portable.
#
# So the alternation is grouped BEFORE the pipe, and an unmeasurable tree is a
# hard error rather than a silent 0.
#
# And the fallback computes the apparent size ITSELF rather than falling back
# to block counts. `du` without `--apparent-size` undercounts a sparse tree by
# construction -- on this very tree it says 1 MiB where the apparent size is
# 200 -- so a fallback to it is a fallback to the wrong answer, just a louder
# one. Summing the sizes `ls -ln` reports (field 5, apparent bytes) is correct
# on both BSD and GNU, which is what the fallback is for.
set -eu

dir=${1:?usage: esp-size-mb.sh <dir> <headroom-MiB>}
headroom=${2:?usage: esp-size-mb.sh <dir> <headroom-MiB>}

[ -d "$dir" ] || { echo "esp-size-mb.sh: no existe el directorio '$dir'" >&2; exit 1; }

# Round UP: a tree of 0.4 MiB needs 1, not 0.
apparent_mb_portable() {
  find "$1" -type f -exec ls -ln {} + 2>/dev/null \
    | awk '{ s += $5 } END { print int((s + 1048575) / 1048576) }'
}

size=$(du -sm --apparent-size "$dir" 2>/dev/null | cut -f1) || size=''
case $size in
  '' | *[!0-9]*) size=$(apparent_mb_portable "$dir") ;;
esac

# Both paths gone, or output that is not a number: say so instead of handing
# back a size that boots nothing.
case $size in
  '' | *[!0-9]*)
    echo "esp-size-mb.sh: no se ha podido medir '$dir' (dio '$size')" >&2
    exit 1
    ;;
esac

echo $((size + headroom))
