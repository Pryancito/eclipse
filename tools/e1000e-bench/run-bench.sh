#!/usr/bin/env bash
# run-bench.sh — Levanta el mirror HTTP local y arranca Eclipse OS en QEMU
# (headless, TCG) con el driver e1000e + user-net, capturando la salida de
# serie. Imprime PASS/FAIL según los marcadores BENCH_* del guest.
#
# Uso: bash local-mirror/run-bench.sh [TIMEOUT_SEG]
set -u
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$REPO_ROOT/local-mirror"
ROOT="$REPO_ROOT"
ESP="$ROOT/target/x86_64/release/esp"
OVMF="$ROOT/rboot/OVMF.fd"
TIMEOUT="${1:-240}"
PORT="${PORT:-8080}"
SERIAL="/tmp/eclipse-serial.log"
PCAP="/tmp/eclipse.pcap"

rm -f "$SERIAL" "$PCAP"

# 1. Mirror HTTP en 0.0.0.0:PORT (alcanzable desde el guest como 10.0.2.2:PORT)
pkill -f "http.server $PORT" 2>/dev/null
( cd "$HERE" && python3 -m http.server "$PORT" --bind 0.0.0.0 >/tmp/mirror-httpd.log 2>&1 & echo $! > /tmp/mirror-httpd.pid )
sleep 1
echo "[bench] mirror servido en 0.0.0.0:$PORT (pid $(cat /tmp/mirror-httpd.pid))"

# 2. QEMU headless. e1000e + user net (SLIRP). Sin KVM (TCG).
echo "[bench] arrancando QEMU (timeout ${TIMEOUT}s, TCG)..."
timeout "${TIMEOUT}" qemu-system-x86_64 \
    -smp 4 \
    -machine q35 \
    -cpu Haswell,+smap,-check,-fsgsbase \
    -m 1G \
    -display none \
    -serial "file:$SERIAL" \
    -drive "format=raw,if=pflash,readonly=on,file=$OVMF" \
    -drive "format=raw,file=fat:rw:$ESP" \
    -nic none \
    -device qemu-xhci,id=xhci \
    -netdev user,id=net1 \
    -device e1000e,netdev=net1 \
    -object "filter-dump,id=f0,netdev=net1,file=$PCAP" \
    -no-reboot
QEMU_RC=$?
echo "[bench] QEMU terminó rc=$QEMU_RC"

# 3. Parar mirror
[ -f /tmp/mirror-httpd.pid ] && kill "$(cat /tmp/mirror-httpd.pid)" 2>/dev/null

# 4. Resumen
echo "=================== RESUMEN BENCH ==================="
EXPECTED=$(cat "$HERE/repo/x86_64/bigfile.sha256" 2>/dev/null | awk '{print $1}')
grep -aE "BENCH_APK_RUN|BENCH_APK_FAILS|BENCH_BIGFILE_SHA|BENCH:|link UP|link DOWN|RX overrun|smoltcp poll" "$SERIAL" 2>/dev/null
echo "----------------------------------------------------"
GOTSHA=$(grep -a "BENCH_BIGFILE_SHA=" "$SERIAL" 2>/dev/null | tail -1 | sed 's/.*BENCH_BIGFILE_SHA=//')
FAILS=$(grep -a "BENCH_APK_FAILS=" "$SERIAL" 2>/dev/null | tail -1 | sed 's/.*BENCH_APK_FAILS=//')
echo "apk update fails : ${FAILS:-<sin marcador>}"
echo "bigfile sha got  : ${GOTSHA:-<sin marcador>}"
echo "bigfile sha exp  : ${EXPECTED:-<desconocido>}"
if [ -n "$GOTSHA" ] && [ "$GOTSHA" = "$EXPECTED" ]; then
  echo "bigfile integrity: OK"
else
  echo "bigfile integrity: MISMATCH/UNKNOWN"
fi
echo "serial log       : $SERIAL"
echo "pcap             : $PCAP"
