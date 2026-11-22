#!/usr/bin/env python3
"""Pasa la Open POSIX Test Suite dentro de Eclipse OS (QEMU x86_64) y la
compara con lo esperado.

    python3 scripts/open-posix/run.py            # compila, monta la imagen, arranca y mide
    python3 scripts/open-posix/run.py --no-build # solo arranca y mide (imagen ya hecha)
    python3 scripts/open-posix/run.py --only mq_open,sem_post

Lo que imprime: PASS por area, los tests que pasaban y ya no (regresiones,
y el script sale con 1), y los que estaban en expected-x86_64.txt y ahora
pasan (para quitarlos de ahi). El resultado de cada test queda en
target/open-posix/results.txt, con las primeras lineas de su salida cuando no
pasa.

Una pasada entera son unos 30 minutos con TCG y 3 CPUs. No es para cada PR:
es para lanzarla a mano, o cada noche, cuando se toca senales, hilos,
timers, memoria o IPC.

Los codigos son los de la suite (include/posixtest.h): 0 PASS, 1 FAIL,
2 UNRESOLVED, 4 UNSUPPORTED, 5 UNTESTED; opts-run anade 124 cuando el test
pasa de su tiempo y 128+N cuando muere de la senal N.
"""
import argparse
import collections
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
GUEST_DIR = "/libc-test/open-posix"
EXPECTED = HERE / "expected-x86_64.txt"
OUT_DIR = ROOT / "target" / "open-posix"
# Lo que tarda el test mas largo en Linux es 30 s justos (timer_settime/2-1
# duerme 2+4+6+8+10 s); con 30 s de tope salia como colgado.
PER_TEST_TIMEOUT = 60
PROMPT_TIMEOUT = 400
NAMES = {0: "PASS", 1: "FAIL", 2: "UNRESOLVED", 4: "UNSUPPORTED", 5: "UNTESTED", 124: "HUNG"}
RESULT = re.compile(r"^R (-?\d+) (\S+) (\S+)")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def area(test):
    """El area de un test, por el nombre de la interfaz que prueba."""
    name = test.split("/")[2]
    prefixes = [
        ("pthread_", "hilos"), ("sem_", "semaforos"), ("mq_", "colas de mensajes"),
        ("timer_", "timers y relojes"), ("clock", "timers y relojes"),
        ("nanosleep", "timers y relojes"), ("sched_", "planificacion"),
        ("aio_", "AIO"), ("lio_", "AIO"), ("shm_", "memoria"), ("mmap", "memoria"),
        ("munmap", "memoria"), ("mlock", "memoria"), ("munlock", "memoria"),
        ("mprotect", "memoria"), ("msync", "memoria"), ("posix_", "memoria"),
    ]
    for prefix, label in prefixes:
        if name.startswith(prefix):
            return label
    if name.startswith("sig") or name in ("kill", "killpg", "raise", "alarm", "pause", "abort"):
        return "senales"
    return "otros"


def status(code):
    if code in NAMES:
        return NAMES[code]
    return "SIGNAL" if code > 128 else f"EXIT({code})"


def load_expected():
    """`test ESTADO  # motivo` por linea. FLAKY: puede salir de las dos maneras."""
    expected = {}
    for line in EXPECTED.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            test, state = line.split()[:2]
            expected[test] = state
    return expected


def build():
    subprocess.run([str(HERE / "build.sh")], check=True)
    subprocess.run(["cargo", "image", "--arch", "x86_64", "--variant", "minimal"], cwd=ROOT, check=True)
    subprocess.run(
        ["make", "-C", "zCore", "build", "ARCH=x86_64", "LINUX=1", "MODE=release", "VARIANT=minimal"],
        cwd=ROOT,
        check=True,
    )


class Guest:
    """QEMU con la consola serie en stdin/stdout, leida en un hilo."""

    def __init__(self, smp):
        cmd = [
            "make", "-C", str(ROOT / "zCore"), "justrun", "ARCH=x86_64", "LINUX=1",
            "MODE=release", "VARIANT=minimal", f"SMP={smp}", "GRAPHIC=off",
        ]
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.lock = threading.Lock()
        self.text = ""
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for raw in iter(lambda: self.proc.stdout.read1(4096), b""):
            chunk = ANSI.sub("", raw.decode(errors="replace").replace("\r", ""))
            with self.lock:
                self.text += chunk

    def wait_for(self, pattern, timeout, start=0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                m = re.search(pattern, self.text[start:])
            if m:
                return True
            if self.proc.poll() is not None:
                return False
            time.sleep(0.2)
        return False

    def send(self, line):
        self.proc.stdin.write(line.encode() + b"\n")
        self.proc.stdin.flush()

    def stop(self):
        try:
            self.send("poweroff -f")
            time.sleep(2)
        except OSError:
            pass
        self.proc.kill()


def run(tests, smp, total_timeout):
    guest = Guest(smp)
    try:
        if not guest.wait_for(r"/ # ", PROMPT_TIMEOUT):
            raise SystemExit("el invitado no llego al prompt:\n" + guest.text[-4000:])
        guest.send("")
        guest.wait_for(r"entering supervision loop", 200)
        time.sleep(3)
        start = len(guest.text)
        guest.send("PS1='OPTS> '")
        guest.wait_for("OPTS> ", 60, start)
        start = len(guest.text)
        if tests is None:
            listing = f"{GUEST_DIR}/list.txt"
        else:
            listing = "/tmp/open-posix-list.txt"
            guest.send(f"grep -E '/({'|'.join(tests)})/' {GUEST_DIR}/list.txt > {listing}")
        guest.send(f"{GUEST_DIR}/opts-run {listing} {GUEST_DIR} {PER_TEST_TIMEOUT}; echo OPTS-EXIT$?")
        if not guest.wait_for(r"OPTS-EXIT\d+", total_timeout, start):
            print("la pasada no termino: el invitado se paro o murio; lo que hay se cuenta", file=sys.stderr)
        with guest.lock:
            return guest.text[start:]
    finally:
        guest.stop()


def parse(text):
    results, output, current = {}, collections.defaultdict(list), None
    for line in text.splitlines():
        m = RESULT.match(line)
        if m:
            current = m.group(3)
            results[current] = status(int(m.group(1)))
        elif line.startswith("O ") and current:
            output[current].append(line[2:])
    return results, output


def report(results, output, expected, listed):
    by_area = collections.defaultdict(collections.Counter)
    for test in listed:
        state = results.get(test, "NOT RUN")
        by_area[area(test)][state] += 1
        by_area["TOTAL"][state] += 1
    print(f"{'area':22} {'tests':>6} {'PASS':>6} {'%':>6}")
    for name in sorted(by_area, key=lambda a: (a == "TOTAL", a)):
        c = by_area[name]
        n = sum(c.values())
        print(f"{name:22} {n:6} {c['PASS']:6} {100 * c['PASS'] / n:6.1f}")
    regressions = [t for t in listed if results.get(t) != "PASS" and t not in expected]
    fixed = [t for t in listed if results.get(t) == "PASS" and expected.get(t) not in (None, "FLAKY")]
    if fixed:
        print(f"\nPasan y estan en {EXPECTED.name} ({len(fixed)}), para quitarlos de ahi:")
        for t in fixed:
            print("  " + t)
    if regressions:
        print(f"\nRegresiones ({len(regressions)}): pasaban y no pasan")
        for t in regressions:
            print(f"  {t}: {results.get(t, 'NOT RUN')}")
            for line in output.get(t, [])[:3]:
                print("      " + line)
    return 1 if regressions else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--no-build", action="store_true", help="usar la imagen que ya hay")
    parser.add_argument("--only", help="interfaces separadas por comas (los directorios de la suite)")
    parser.add_argument("--smp", type=int, default=max(1, min(3, (os.cpu_count() or 2) - 1)))
    args = parser.parse_args()
    if not args.no_build:
        build()
    list_file = ROOT / "rootfs/x86_64-minimal/libc-test/open-posix/list.txt"
    listed = [l.strip() for l in list_file.read_text().splitlines() if l.strip()]
    tests = args.only.split(",") if args.only else None
    if tests:
        listed = [t for t in listed if t.split("/")[2] in tests]
    text = run(tests, args.smp, total_timeout=len(listed) * PER_TEST_TIMEOUT + 600)
    results, output = parse(text)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    (OUT_DIR / "results.txt").write_text(text)
    sys.exit(report(results, output, load_expected(), listed))


if __name__ == "__main__":
    main()
