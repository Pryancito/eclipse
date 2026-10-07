# Claves de firma de Alpine, por arquitectura

**Alpine firma el `APKINDEX` de cada arquitectura con una clave distinta.** Un
`etc/apk/keys` con solo las de x86_64 deja la compilacion de aarch64 en

```
WARNING: updating and opening http://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/APKINDEX.tar.gz: UNTRUSTED signature
```

y apk no instala nada: el indice se lee, la resolucion falla y la tirada sigue,
porque el paso de paquetes es best-effort.

## Como se reparte

- `tools/apk/keys/<arco>/*.pub` — las claves de ESA arquitectura. Son las que
  `LinuxRootfs::install_apk_keys` copia a `<rootfs>/etc/apk/keys` segun el
  objetivo que se este compilando.
- `tools/apk/keys/*.pub` (sueltas, sin subdirectorio) — se copian para
  **cualquier** arquitectura. Es donde seguir dejando una clave propia o una
  que no cuelgue de un arco.

Es el mismo reparto que hace Alpine en su paquete `alpine-keys`:
`/usr/share/apk/keys/<arco>/` enlaza al monton comun y `/etc/apk/keys/` recibe
solo las del arco nativo.

## De donde salen estas

Del paquete `alpine-keys` de Alpine (`main/alpine-keys`, `pkgver=2.6`), cuyo
`APKBUILD` es el que dice que clave va con que arquitectura. Copiadas del
espejo oficial de aports en GitHub
(`raw.githubusercontent.com/alpinelinux/aports/master/main/alpine-keys/`), que
es lo unico alcanzable desde el entorno donde se trajeron.

El reparto del `APKBUILD`, para las tres que Eclipse soporta:

| arquitectura | claves                 |
| ------------ | ---------------------- |
| `x86_64`     | `4a6a0840` (compartida con x86), `5261cecb`, `6165ee59` |
| `aarch64`    | `58199dcc`, `616ae350` |
| `riscv64`    | `60ac2099`, `616db30d` |

Las siete son claves publicas RSA validas de 2048 o 4096 bits (`openssl rsa
-pubin -noout -text`). **Lo que no se ha podido comprobar aqui es que verifiquen
un APKINDEX de verdad**, porque en ese entorno dl-cdn.alpinelinux.org devuelve
403 y no hay espejo de Alpine.

## Para añadir otra arquitectura

Mirar el `_arch_keys` del `APKBUILD` de `alpine-keys`, no adivinar: los nombres
de fichero son lo que apk compara contra el nombre de clave que viene dentro de
la firma, asi que un nombre cambiado es una clave que no sirve.
