#!/bin/busybox sh
# bench.sh — arranca como PID 1 (ROOTPROC). Configura red estática (SLIRP) y
# ejercita el driver e1000e: (1) integridad de transferencia TCP por wget de un
# fichero grande con sha256 repetido N veces, (2) apk update repetido. Imprime
# marcadores BENCH_* por serie para evaluación automática.
PATH=/bin:/sbin:/usr/bin:/usr/sbin
export PATH
B=/bin/busybox

$B mount -t proc proc /proc 2>/dev/null
$B mount -t sysfs sysfs /sys 2>/dev/null
$B mount -t tmpfs tmpfs /tmp 2>/dev/null

echo "==== ECLIPSE E1000E BENCH START ===="

IF=""
for cand in $($B ls /sys/class/net 2>/dev/null); do
  [ "$cand" = "lo" ] && continue
  IF="$cand"; break
done
[ -z "$IF" ] && IF=eth0
echo "BENCH_IFACE=$IF"

$B ip link set lo up 2>/dev/null
$B ip link set "$IF" up 2>/dev/null
$B ip addr add 10.0.2.15/24 dev "$IF" 2>/dev/null
$B ip route add default via 10.0.2.2 2>/dev/null

URL=http://10.0.2.2:8080/repo/x86_64
EXP=$($B wget -q -O - "$URL/bigfile.sha256" 2>/dev/null | $B awk '{print $1}')
echo "BENCH_EXP_SHA=$EXP"

# ---- (1) Integridad de transferencia (driver RX) ----
WN=${BENCH_WGET_N:-5}
i=1; wfail=0
while [ "$i" -le "$WN" ]; do
  $B rm -f /tmp/bigfile.bin
  T0=$($B cat /proc/uptime 2>/dev/null | $B awk '{print $1}')
  $B wget -q -O /tmp/bigfile.bin "$URL/bigfile.bin"
  wrc=$?
  SZ=$($B wc -c < /tmp/bigfile.bin 2>/dev/null)
  GOT=$($B sha256sum /tmp/bigfile.bin 2>/dev/null | $B awk '{print $1}')
  if [ "$wrc" = "0" ] && [ -n "$GOT" ] && [ "$GOT" = "$EXP" ]; then
    echo "BENCH_WGET $i OK size=$SZ"
  else
    echo "BENCH_WGET $i FAIL rc=$wrc size=$SZ sha=$GOT"
    wfail=$((wfail+1))
  fi
  i=$((i+1))
done
echo "BENCH_WGET_FAILS=$wfail/$WN"

# ---- (2) apk update --no-cache (aísla la red; cuenta corrupción del driver) ----
$B rm -rf /var/cache/apk/* 2>/dev/null
AN=${BENCH_APK_N:-30}
i=1; afail=0; aerr=0
while [ "$i" -le "$AN" ]; do
  $B rm -rf /var/cache/apk/* 2>/dev/null
  apk update --no-cache >/tmp/apk.out 2>&1
  arc=$?
  # corrupción del driver -> firma mala / índice inválido / inconsistente
  corr=$($B sed 's/\x1b\[[0-9;]*m//g' /tmp/apk.out | $B grep -ciE "BAD signature|invalid or inconsistent|format is invalid|UNTRUSTED")
  ok=$($B grep -c "distinct packages" /tmp/apk.out)
  if [ "$corr" != "0" ]; then
    aerr=$((aerr+1))
    echo "BENCH_APK $i CORRUPT rc=$arc"
    $B sed 's/\x1b\[[0-9;]*m//g' /tmp/apk.out | $B grep -iE "BAD|invalid|UNTRUSTED|inconsistent" | head -1
  elif [ "$ok" = "0" ]; then
    afail=$((afail+1))
    echo "BENCH_APK $i NOIDX rc=$arc"
  else
    echo "BENCH_APK $i OK rc=$arc"
  fi
  i=$((i+1))
done
echo "BENCH_APK_CORRUPT=$aerr/$AN"
echo "BENCH_APK_NOIDX=$afail/$AN"

echo "==== ECLIPSE E1000E BENCH END wget_fails=$wfail apk_corrupt=$aerr apk_noidx=$afail ===="
$B sync
$B sleep 1
$B poweroff -f 2>/dev/null
while true; do $B sleep 5; done
