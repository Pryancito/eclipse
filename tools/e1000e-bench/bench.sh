#!/bin/busybox sh
# bench.sh — e1000e diagnostic: runs apk update (APK signature/index integrity)
# and a bigfile wget (large-transfer integrity) against a local mirror, to
# isolate network/driver failures from Btrfs/block failures.
#
# Interpretation matrix (see tools/e1000e-bench/README.md):
#   BENCH_APK_CORRUPT > 0  → BAD signature / inconsistent index → e1000e/network
#   BENCH_APK_NOIDX   > 0  → connection failure → e1000e/network
#   BENCH_WGET_FAILS  > 0  → large-transfer loss/corruption → e1000e/network
#   All zeros          → network OK; any big-file failure is Btrfs/block
#
# Markers emitted (parsed by run-bench.sh on the host):
#   BENCH_APK_RUN i OK|CORRUPT|NOIDX
#   BENCH_APK_CORRUPT=n/N
#   BENCH_APK_NOIDX=n/N
#   BENCH_APK_FAILS=n/N          (corrupt + noidx combined)
#   BENCH_WGET i OK|FAIL
#   BENCH_BIGFILE_SHA=<sha256>   (last successful download; host compares)
#   BENCH_WGET_FAILS=n/N
PATH=/bin:/sbin:/usr/bin:/usr/sbin
export PATH
B=/bin/busybox
$B mount -t proc proc /proc 2>/dev/null
$B mount -t sysfs sysfs /sys 2>/dev/null
$B mount -t tmpfs tmpfs /tmp 2>/dev/null

echo "==== ECLIPSE E1000E BENCH START ===="

# --- Network setup ---
IF=""
for cand in $($B ls /sys/class/net 2>/dev/null); do
  [ "$cand" = "lo" ] && continue; IF="$cand"; break
done
[ -z "$IF" ] && IF=eth0
echo "BENCH_IFACE=$IF"
$B ip link set lo up 2>/dev/null
$B ip link set "$IF" up 2>/dev/null
$B ip addr add 10.0.2.15/24 dev "$IF" 2>/dev/null
$B ip route add default via 10.0.2.2 2>/dev/null

MIRROR="http://10.0.2.2:8080/repo"
AN=${BENCH_N:-8}

# Point apk at the local mirror (public key already in /etc/apk/keys via rootfs)
$B mkdir -p /etc/apk
echo "$MIRROR" > /etc/apk/repositories

# --- Step 1: apk update rounds (signature + index integrity via e1000e RX/TX) ---
# Detects: BAD signature, invalid or inconsistent index, UNTRUSTED key,
# NOIDX (connection failure).  These are unambiguous driver/network symptoms.
i=1; aok=0; acorrupt=0; anoidx=0
while [ "$i" -le "$AN" ]; do
  OUT=$( $B timeout 90 apk update --no-cache 2>&1 )
  RC=$?
  case "$OUT" in
    *"BAD signature"*|*"invalid or inconsistent"*|*"UNTRUSTED"*)
      acorrupt=$((acorrupt+1))
      echo "BENCH_APK_RUN $i CORRUPT rc=$RC"
      ;;
    *)
      if [ "$RC" -ne 0 ]; then
        anoidx=$((anoidx+1))
        echo "BENCH_APK_RUN $i NOIDX rc=$RC"
      else
        aok=$((aok+1))
        echo "BENCH_APK_RUN $i OK"
      fi
      ;;
  esac
  i=$((i+1))
done
AFAILS=$((acorrupt+anoidx))
echo "BENCH_APK_CORRUPT=${acorrupt}/${AN}"
echo "BENCH_APK_NOIDX=${anoidx}/${AN}"
echo "BENCH_APK_FAILS=${AFAILS}/${AN}"

# --- Step 2: bigfile wget (large TCP transfer integrity — exercises RX path) ---
# The host serves bigfile.bin (high-entropy random data) and its sha256.
# The guest downloads the file, computes sha256, and emits it for comparison.
wok=0; wfail=0
i=1
while [ "$i" -le "$AN" ]; do
  $B rm -f /tmp/bigfile.bin
  $B timeout 120 $B wget -q -O /tmp/bigfile.bin "$MIRROR/x86_64/bigfile.bin"
  WRC=$?
  if [ "$WRC" -eq 0 ] && [ -s /tmp/bigfile.bin ]; then
    SHA=$($B sha256sum /tmp/bigfile.bin 2>/dev/null | $B awk '{print $1}')
    wok=$((wok+1))
    echo "BENCH_WGET $i OK sha=$SHA"
    echo "BENCH_BIGFILE_SHA=$SHA"
  else
    wfail=$((wfail+1))
    echo "BENCH_WGET $i FAIL wrc=$WRC"
  fi
  i=$((i+1))
done
echo "BENCH_WGET_FAILS=${wfail}/${AN}"

echo "==== ECLIPSE E1000E BENCH END ===="
$B sync; $B sleep 1
$B poweroff -f 2>/dev/null
while true; do $B sleep 5; done
