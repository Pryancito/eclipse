# Binarios precompilados del runtime de Zircon

[English](README.md) · **Español**

Descarga los artefactos del runtime con `make zircon-init` (o `cargo
zircon-init`). El instalador usa la GitHub Release fijada
[`prebuilt-260903`](https://github.com/rcore-os/zCore/releases/tag/prebuilt-260903),
verifica el SHA-256 del archivo y el `SHA256SUMS` de este directorio, y luego
instala los ficheros en el directorio ignorado `prebuilt/zircon/`. Necesita
`wget`, `tar` con soporte xz y `shasum` (disponible en macOS y Ubuntu).

La release contiene la instantánea de main de Fuchsia compilada el 2026-09-03
para x64, arm64 y riscv64: userboot/test-userboot de upstream, vDSO e imágenes
de bringup y core-test, más vDSO de LibOS separados para x64 y arm64. Los
binarios de usuario de arm64 y la imagen de core-test usan alineación de
segmentos ELF de 16 KiB para hosts Apple Silicon. Solo se parchea el vDSO de
LibOS; ver `scripts/gen-prebuilt.sh`.

Los artefactos binarios van en GitHub Releases, no en Git. Al publicar una
actualización, usa una etiqueta de release o un nombre de asset nuevos,
actualiza la URL, el nombre del fichero de caché y el resumen del archivo en
`xtask/src/main.rs`, y actualiza `prebuilt/SHA256SUMS`. No sustituyas el asset
fijado en su sitio. La CI instala la misma release con `make zircon-init`.
