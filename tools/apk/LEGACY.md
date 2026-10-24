# Legado: apk-tools / Alpine

El guest de producto usa **apt** con Ubuntu 26.10 (`xtask/src/linux/apt_rootfs.rs`).

Este árbol (`tools/apk/`, estáticos Chimera, claves Alpine) se conserva para:

- `ECLIPSE_PKG=apk` (escape hatch en builds)
- el banco `tools/e1000e-bench` (mirror apk firmado)

No es el gestor por defecto en x86_64/aarch64.
