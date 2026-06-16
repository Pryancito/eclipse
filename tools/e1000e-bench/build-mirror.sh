#!/usr/bin/env bash
# build-mirror.sh — Genera un mirror Alpine local (firmado) para el benchmark del
# driver e1000e bajo QEMU. No requiere acceso a dl-cdn.alpinelinux.org.
#
#   repo/x86_64/APKINDEX.tar.gz   índice v2 firmado (apk update lo descarga)
#   repo/x86_64/bigfile.bin       fichero grande de alta entropía (test wget/integridad)
#   repo/x86_64/bigfile.sha256    checksum esperado de bigfile.bin
#
# El tamaño del índice y del bigfile se controlan por entorno para ejercitar el
# camino RX/TX del driver con transferencias TCP de varios MB.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$REPO_ROOT/local-mirror"
mkdir -p "$HERE"
ROOT="$REPO_ROOT"
APK="$ROOT/tools/apk/apk-x86_64.static"
KEY="$HERE/keys/eclipse-bench@local-1.rsa"
PUB="eclipse-bench@local-1.rsa.pub"

# Generar clave de firma si no existe (privada gitignored bajo local-mirror/)
if [ ! -f "$KEY" ]; then
  echo "[mirror] generando clave de firma..."
  mkdir -p "$HERE/keys"
  openssl genrsa -out "$KEY" 2048 2>/dev/null
  openssl rsa -in "$KEY" -pubout -out "$HERE/keys/$PUB" 2>/dev/null
fi
# La pública debe estar en /etc/apk/keys del guest: el rootfs la toma de prebuilt/
mkdir -p "$ROOT/prebuilt/alpine-apk-keys"
cp -f "$HERE/keys/$PUB" "$ROOT/prebuilt/alpine-apk-keys/$PUB"

NUM_RECORDS="${NUM_RECORDS:-60000}"   # nº de paquetes ficticios en el índice
BIGFILE_MB="${BIGFILE_MB:-16}"        # tamaño del fichero de integridad

REPO="$HERE/repo/x86_64"
rm -rf "$HERE/repo"
mkdir -p "$REPO"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "[mirror] generando APKINDEX con $NUM_RECORDS registros..."
# Cabecera DESCRIPTION + cuerpo APKINDEX (registros de alta entropía -> índice grande)
echo "Eclipse OS benchmark mirror $(date -u +%Y%m%d%H%M%S)" > "$WORK/DESCRIPTION"
python3 - "$NUM_RECORDS" > "$WORK/APKINDEX" <<'PY'
import sys, os, base64, hashlib
n = int(sys.argv[1])
out = sys.stdout
for i in range(n):
    # checksum Q1<base64(sha1)> de datos aleatorios -> entropía real en el índice
    h = hashlib.sha1(os.urandom(20)).digest()
    csum = "Q1" + base64.b64encode(h).decode()
    name = f"benchpkg{i:06d}"
    out.write(f"C:{csum}\n")
    out.write(f"P:{name}\n")
    out.write("V:1.0.0-r0\n")
    out.write("A:x86_64\n")
    out.write(f"S:{1000+i%5000}\n")
    out.write(f"I:{4096+i%8192}\n")
    out.write(f"T:Benchmark dummy package {i}\n")
    out.write("U:https://eclipse.local/\n")
    out.write("L:MIT\n")
    out.write(f"o:{name}\n")
    out.write("m:Eclipse Bench <bench@eclipse.local>\n")
    out.write("t:1700000000\n")
    out.write("\n")
PY

# Empaquetar índice v2 sin firmar (DESCRIPTION + APKINDEX), formato ustar
UNS="$WORK/APKINDEX.unsigned.tar.gz"
tar -c --format=ustar -C "$WORK" -f - DESCRIPTION APKINDEX | gzip -9 -n > "$UNS"

# Firmar (RSA-SHA256), miembro .SIGN.RSA256.<pub>, gzip propio, concatenado.
# CLAVE: el tar de la firma NO debe llevar los bloques EOF (ceros) — equivale a
# `abuild-tar --cut`. Si se incluyen, apk reporta "invalid or inconsistent".
SIGDIR="$WORK/sig"; mkdir -p "$SIGDIR"
openssl dgst -sha256 -sign "$KEY" -out "$SIGDIR/.SIGN.RSA256.$PUB" "$UNS"
tar -c --format=ustar -C "$SIGDIR" -f "$WORK/sig.tar" ".SIGN.RSA256.$PUB"
python3 - "$WORK/sig.tar" "$WORK/sig.tar.cut" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
# Quitar todos los bloques de 512 bytes a cero del final (EOF de tar)
while len(data) >= 512 and data[-512:] == b"\x00" * 512:
    data = data[:-512]
open(sys.argv[2], "wb").write(data)
PY
gzip -9 -n -c "$WORK/sig.tar.cut" > "$WORK/sig.tar.gz"
cat "$WORK/sig.tar.gz" "$UNS" > "$REPO/APKINDEX.tar.gz"
echo "[mirror] APKINDEX.tar.gz: $(stat -c%s "$REPO/APKINDEX.tar.gz") bytes"

# Fichero grande de alta entropía para test de integridad por wget
echo "[mirror] generando bigfile.bin (${BIGFILE_MB} MiB)..."
head -c "$((BIGFILE_MB*1024*1024))" /dev/urandom > "$REPO/bigfile.bin"
( cd "$REPO" && sha256sum bigfile.bin > bigfile.sha256 )
echo "[mirror] bigfile.sha256: $(cat "$REPO/bigfile.sha256")"
echo "[mirror] listo en $REPO"
