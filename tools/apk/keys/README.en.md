# Alpine signing keys, per architecture

[Español](README.md) · **English**

> Only for the legacy apk path (`ECLIPSE_PKG=apk`). The default image uses apt
> with Ubuntu and does not need these keys.

**Alpine signs each architecture's `APKINDEX` with a different key.** An
`etc/apk/keys` with only the x86_64 ones leaves the aarch64 build at

```
WARNING: updating and opening http://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/APKINDEX.tar.gz: UNTRUSTED signature
```

and apk installs nothing: the index is read, resolution fails and the run
carries on, because the package step is best-effort.

## How they are split

- `tools/apk/keys/<arch>/*.pub` — the keys for THAT architecture. These are
  the ones `LinuxRootfs::install_apk_keys` copies to `<rootfs>/etc/apk/keys`
  for the target being built.
- `tools/apk/keys/*.pub` (loose, no subdirectory) — copied for **any**
  architecture. That is where to keep dropping a key of your own or one that
  does not belong to an architecture.

It is the same split Alpine does in its `alpine-keys` package:
`/usr/share/apk/keys/<arch>/` links into the common pile and `/etc/apk/keys/`
gets only the native architecture's.

## Where these come from

From Alpine's `alpine-keys` package (`main/alpine-keys`, `pkgver=2.6`), whose
`APKBUILD` is what says which key goes with which architecture. Copied from
the official aports mirror on GitHub
(`raw.githubusercontent.com/alpinelinux/aports/master/main/alpine-keys/`),
which was the only thing reachable from the environment they were fetched in.

The `APKBUILD`'s split, for the three Eclipse supports:

| architecture | keys                   |
| ------------ | ---------------------- |
| `x86_64`     | `4a6a0840` (shared with x86), `5261cecb`, `6165ee59` |
| `aarch64`    | `58199dcc`, `616ae350` |
| `riscv64`    | `60ac2099`, `616db30d` |

All seven are valid 2048- or 4096-bit RSA public keys (`openssl rsa -pubin
-noout -text`). **What could not be checked here is that they verify a real
APKINDEX**, because in that environment dl-cdn.alpinelinux.org returns 403 and
there is no Alpine mirror.

## Adding another architecture

Look at the `_arch_keys` in `alpine-keys`'s `APKBUILD`, do not guess: the file
names are what apk compares against the key name inside the signature, so a
renamed file is a key that does not work.
