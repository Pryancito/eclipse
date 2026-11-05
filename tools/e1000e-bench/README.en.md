# e1000e driver test bench (apk update under QEMU)

[Español](README.md) · **English**

**Alpine legacy:** this bench still uses a signed **apk** mirror as a
reproducible network load. The product guest uses apt (Ubuntu 26.10); running
the bench needs `ECLIPSE_PKG=apk` or a rootfs that still carries `/bin/apk`.
Moving the bench to `apt-get update` is still pending.

It reproduces, self-contained and without Internet access, the scenario that
stresses the `drivers/src/net/e1000e.rs` driver: downloading the package index
with `apk update` and TCP transfers of several MB.

Instead of the real Alpine mirror (`dl-cdn.alpinelinux.org`, usually
unreachable from CI/restricted environments) a **signed local mirror** is
served over HTTP on the host. QEMU-SLIRP routes `10.0.2.2` → host, so the
guest runs `apk update` against that mirror, exercising the driver's RX/TX
path. If the driver drops or corrupts packets, `apk` reports `BAD signature` /
`invalid or inconsistent`; if it is correct, it gives 0 failures.

## Usage

```bash
# 1. Rootfs + image + ESP (once; needs QEMU, mtools, dosfstools)
cargo rootfs --arch x86_64
cp tools/e1000e-bench/bench.sh rootfs/x86_64/bench.sh   # the benchmark's init
cargo image --arch x86_64
make -C zCore build MODE=release LINUX=1 LOG=warn GRAPHIC=off \
  CMDLINE='LOG=warn:console.shell=true:virtcon.disable=true:ROOT=/dev/vda:ROOTPROC=/bin/busybox?sh?/bench.sh'

# 2. Generate the signed local mirror (large index + bigfile)
NUM_RECORDS=40000 BIGFILE_MB=16 bash tools/e1000e-bench/build-mirror.sh

# 3. Launch QEMU + mirror and evaluate (timeout in seconds)
bash tools/e1000e-bench/run-bench.sh 720
```

The guest pins `/etc/apk/repositories` to `http://10.0.2.2:8080/repo` and
trusts the mirror's key through `/etc/apk/keys` (the public key is copied to
`prebuilt/alpine-apk-keys/` during `build-mirror.sh` and `cargo rootfs` puts
it in the rootfs).

## Output markers (serial)

- `BENCH_WGET i OK|FAIL`     integrity via wget+sha256 of `bigfile.bin`
- `BENCH_WGET_FAILS=n/N`
- `BENCH_APK i OK|CORRUPT|NOIDX`  `apk update --no-cache`
- `BENCH_APK_CORRUPT=n/N`    runs with a bad signature / inconsistent index (driver corruption)
- `BENCH_APK_NOIDX=n/N`      runs with no index (connection failure)

Goal: `BENCH_APK_CORRUPT=0/N` and `BENCH_WGET_FAILS=0/N`.

## Notes

- Without `/dev/kvm` TCG (emulation) is used, slower but valid for integrity.
- The private key and the generated repo live under `local-mirror/`
  (gitignored).
- In QEMU, smoltcp verifies TCP/IP checksums in software, so corruption is
  detected and retransmitted; silent corruption failures are usually specific
  to real hardware (I219-V / DMA coherence / PCH).
