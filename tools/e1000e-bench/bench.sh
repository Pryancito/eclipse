#!/bin/busybox sh
# bench.sh (DIAGNÓSTICO DEFINITIVO) — descarga el índice con wget bajo `timeout`
# externo (mata el wget atascado) y mide tamaño y sha256 vs lo esperado, para
# distinguir TRUNC (incompleto) / CORRUPT (completo pero sha != ) / OK.
PATH=/bin:/sbin:/usr/bin:/usr/sbin
export PATH
B=/bin/busybox
$B mount -t proc proc /proc 2>/dev/null
$B mount -t sysfs sysfs /sys 2>/dev/null
$B mount -t tmpfs tmpfs /tmp 2>/dev/null

echo "==== ECLIPSE E1000E BENCH START ===="
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

EXP_SZ=1556947
EXP_SHA=3db23ac97af118207397181393dc6728f41d3d250adddfb940bee08b0b84375b
URL=http://10.0.2.2:8080/repo/x86_64/APKINDEX.tar.gz

AN=${BENCH_N:-8}
i=1; ok=0; trunc=0; corrupt=0; hang=0
while [ "$i" -le "$AN" ]; do
  $B rm -f /tmp/idx
  $B timeout 90 $B wget -q -O /tmp/idx "$URL"
  wrc=$?
  SZ=$($B wc -c < /tmp/idx 2>/dev/null)
  [ -z "$SZ" ] && SZ=0
  SHA=$($B sha256sum /tmp/idx 2>/dev/null | $B awk '{print $1}')
  if [ "$wrc" = "143" ] || [ "$wrc" = "124" ]; then
    hang=$((hang+1)); echo "BENCH_DL $i HANG wrc=$wrc size=$SZ/$EXP_SZ"
  elif [ "$SZ" != "$EXP_SZ" ]; then
    trunc=$((trunc+1)); echo "BENCH_DL $i TRUNC wrc=$wrc size=$SZ/$EXP_SZ"
  elif [ "$SHA" != "$EXP_SHA" ]; then
    corrupt=$((corrupt+1)); echo "BENCH_DL $i CORRUPT wrc=$wrc size=$SZ sha=$SHA"
  else
    ok=$((ok+1)); echo "BENCH_DL $i OK size=$SZ"
  fi
  i=$((i+1))
done
echo "BENCH_DL_SUMMARY ok=$ok trunc=$trunc corrupt=$corrupt hang=$hang total=$AN"
echo "==== ECLIPSE E1000E BENCH END ===="
$B sync; $B sleep 1
$B poweroff -f 2>/dev/null
while true; do $B sleep 5; done
