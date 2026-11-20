#!/usr/bin/env python3
"""Instala la ISO en un disco NVMe de QEMU, arranca de ese disco y comprueba
que `/` es la particion EclipseFS del disco.

Por que existe: los discos los sirve ahora un driver en espacio de usuario
(NVMe, AHCI, virtio-blk), arrancado por init desde la initramfs de la ESP. El
nucleo elige `/` antes de que haya ningun proceso, no veia el disco, y un
sistema recien instalado arrancaba con la initramfs como `/` y solo
`/boot/efi` montado (visto en una maquina real con `/dev/nvme0n1p2`). El humo
de la ISO (`iso-smoke.sh`) no lo podia ver: el sistema live no tiene raiz en
disco. Esto hace lo que hace una persona: instalar y reiniciar.

  scripts/install-smoke.py [--iso ISO] [--disk-size GIB] [--timeout S] [--out LOG]

Sin KVM a proposito, como `iso-smoke.sh`: en un runner de CI no lo hay. Por
eso los plazos son largos.
"""

import argparse
import os
import re
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class Guest:
    """QEMU with the serial console on its stdin/stdout, all of it logged."""

    def __init__(self, args, log_path):
        self.log = open(log_path, "ab")
        self.buf = b""
        self.lock = threading.Lock()
        self.proc = subprocess.Popen(
            args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
        )
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            chunk = self.proc.stdout.read1(4096)
            if not chunk:
                return
            self.log.write(chunk)
            self.log.flush()
            with self.lock:
                self.buf += chunk

    def text(self):
        with self.lock:
            return self.buf.decode("utf-8", "replace")

    def mark(self):
        with self.lock:
            return len(self.buf)

    def wait(self, pattern, timeout, since=0):
        """Wait for regex `pattern` in the output after byte `since`."""
        deadline = time.time() + timeout
        rx = re.compile(pattern, re.M)
        while time.time() < deadline:
            with self.lock:
                text = self.buf[since:].decode("utf-8", "replace")
            m = rx.search(text)
            if m:
                return m
            if self.proc.poll() is not None:
                raise SystemExit(f"QEMU se fue esperando {pattern!r}")
            time.sleep(1)
        raise SystemExit(f"{timeout} s sin ver {pattern!r} en la consola")

    def send(self, line):
        self.proc.stdin.write(line.encode() + b"\n")
        self.proc.stdin.flush()

    def run(self, command, timeout, tag):
        """Run `command` in the guest shell; answer its output and status.

        The marker is computed by the shell (`$((...))`), so the echo of the
        typed line never matches it.
        """
        since = self.mark()
        self.send(f"{command}; echo {tag}-FIN-$((40+2))-$?")
        m = self.wait(rf"{tag}-FIN-42-(\d+)", timeout, since)
        with self.lock:
            out = self.buf[since:].decode("utf-8", "replace")
        return int(m.group(1)), out

    def stop(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()


def qemu(ovmf, mem, extra):
    return [
        "qemu-system-x86_64",
        "-smp", "2",
        "-machine", "q35",
        "-cpu", "Haswell,+smap,-check,-fsgsbase,+invtsc",
        "-m", mem,
        "-serial", "stdio",
        "-drive", f"format=raw,if=pflash,readonly=on,file={ovmf}",
        "-nic", "none",
        "-display", "none",
        "-no-reboot",
    ] + extra


def boot_to_shell(guest, timeout):
    guest.wait(r"\[eclipse-init\].*entering supervision loop", timeout)
    # The shell on the console comes a moment after init's loop.
    deadline = time.time() + 120
    while True:
        try:
            status, _ = guest.run("true", 10, "LISTO")
            return status
        except SystemExit:
            if time.time() > deadline:
                raise


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iso", default=os.path.join(ROOT, "dist/eclipse-minimal-x86_64.iso"))
    ap.add_argument("--disk-size", type=int, default=12, help="GiB del disco NVMe")
    ap.add_argument("--timeout", type=int, default=900, help="segundos por arranque")
    ap.add_argument("--install-timeout", type=int, default=3600)
    ap.add_argument("--mem", default="4G")
    ap.add_argument("--out", default=os.path.join(ROOT, "build/install-smoke.log"))
    a = ap.parse_args()

    ovmf = os.path.join(ROOT, "rboot/OVMF.fd")
    for path, what in [(a.iso, "la ISO (make release ARCHS=x86_64 VARIANTS=minimal)"), (ovmf, "OVMF")]:
        if not os.path.isfile(path):
            sys.exit(f"falta {what}: {path}")
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    open(a.out, "wb").close()
    disk = os.path.join(os.path.dirname(a.out), "install-smoke-nvme.img")
    with open(disk, "wb") as f:
        f.truncate(a.disk_size << 30)
    nvme = ["-drive", f"if=none,id=nv0,format=raw,file={disk}",
            "-device", "nvme,drive=nv0,serial=eclipse0"]
    # A `bootindex` on any device makes OVMF ignore `-boot order`: only the
    # second boot, from the disk alone, names one.
    nvme_boot = ["-drive", f"if=none,id=nv0,format=raw,file={disk}",
                 "-device", "nvme,drive=nv0,serial=eclipse0,bootindex=0"]

    print(f"=== 1/2: arranco {os.path.basename(a.iso)} e instalo en NVMe ({a.disk_size} GiB) ===")
    g = Guest(qemu(ovmf, a.mem, ["-drive", f"file={a.iso},media=cdrom", "-boot", "order=d"] + nvme), a.out)
    try:
        boot_to_shell(g, a.timeout)
        status, out = g.run(
            "install-eclipse --disk /dev/nvme0n1 --mode new --simple --gpt --yes",
            a.install_timeout,
            "INSTALA",
        )
        if status != 0:
            print(out[-4000:])
            sys.exit(f"install-eclipse salio con {status}")
        g.run("sync", 300, "SYNC")
    finally:
        g.stop()

    print("=== 2/2: arranco del disco NVMe ===")
    with open(a.out, "ab") as f:
        f.write(b"\n=== arranque del disco instalado ===\n")
    g = Guest(qemu(ovmf, a.mem, nvme_boot), a.out)
    try:
        boot_to_shell(g, a.timeout)
        _, out = g.run("cat /proc/mounts", 60, "MONTAJES")
        lines = [l.split() for l in out.splitlines()]
        root = [l for l in lines if len(l) >= 3 and l[1] == "/"]
        efi = [l for l in lines if len(l) >= 3 and l[1] == "/boot/efi"]
        print("/ :", " ".join(root[-1]) if root else "(nada)")
        print("/boot/efi :", " ".join(efi[-1]) if efi else "(nada)")
        ok = (
            root
            and root[-1][0] == "/dev/nvme0n1p2"
            and root[-1][2] in ("eclipsefs", "btrfs")
            and efi
            and efi[-1][0] == "/dev/nvme0n1p1"
        )
        if not ok:
            print(out[-4000:])
            sys.exit("/ no es la particion instalada del disco NVMe")
        # Writable, and it is the installed system that answers.
        status, out = g.run("touch /root/.install-smoke && rm /root/.install-smoke", 60, "ESCRIBE")
        if status != 0:
            print(out[-2000:])
            sys.exit("/ no se deja escribir")
    finally:
        g.stop()
    print("=== OK: instalado en NVMe, arranca y / es /dev/nvme0n1p2 ===")


if __name__ == "__main__":
    main()
