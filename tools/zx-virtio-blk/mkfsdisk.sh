#!/bin/bash
# usage: mkfsdisk.sh OUT.img
#
# A 32 MiB scratch disk for `zx-virtio-blk export` and `zx-virtio-blk fs`: an
# MBR whose first partition (16 MiB at 8 MiB) is a vfat holding pattern.bin,
# 1 MiB of the bytes fs_given() in zx-virtio-blk.c expects. Needs mkfs.vfat
# and mtools. Boot with `make ... UDISK=OUT.img`.
set -e
OUT=$1; T=$(mktemp -d)
python3 - "$T/pattern.bin" <<'PY'
import sys
open(sys.argv[1],'wb').write(bytes(((i*31 + (i>>12)) & 0xff) for i in range(1<<20)))
PY
truncate -s 16M $T/fat.img
mkfs.vfat -F 16 -n SERVED $T/fat.img >/dev/null
mcopy -i $T/fat.img $T/pattern.bin ::pattern.bin
truncate -s 0 $OUT; truncate -s 32M $OUT
dd if=$T/fat.img of=$OUT bs=1M seek=8 conv=notrunc status=none
python3 - "$OUT" <<'PY'
import sys, struct
f=open(sys.argv[1],'r+b')
mbr=bytearray(512)
start=8*2048; size=16*2048
mbr[446:462]=struct.pack('<B3sB3sII',0,b'\xfe\xff\xff',0x0e,b'\xfe\xff\xff',start,size)
mbr[510:512]=b'\x55\xaa'
f.write(mbr)
PY
