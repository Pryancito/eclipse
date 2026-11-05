# zCore en riscv64: placa SiFive fu740

**Español** · [English](README-fu740.en.md)

> Notas heredadas del zCore original (rCore-OS), traducidas del chino. La
> placa se sigue compilando con `PLATFORM=fu740` en `zCore/Makefile`, pero no
> se ha vuelto a probar en Eclipse.

### Compilar la imagen de zCore para fu740

Primero, en la raíz del código, compila el sistema de ficheros de riscv64.

Después entra en el subdirectorio zCore y compila el kernel; se genera la
imagen de sistema `zcore-fu740.itb`.

La imagen incluye el device tree de la placa fu740. El dtb se puede sacar del
directorio `/boot` del Linux que trae la fu740, o de la imagen oficial de
SiFive:
https://github.com/sifive/freedom-u-sdk/releases/download/2022.04.00/demo-coreip-cli-unmatched-2022.04.00.rootfs.wic.xz

```sh
make rootfs ARCH=riscv64 && make image ARCH=riscv64
cd zCore
make build MODE=release LINUX=1 ARCH=riscv64 PLATFORM=fu740
```

### Arrancar la imagen desde U-Boot

Primero monta un servidor tftp; por ejemplo, instala `tftpd-hpa` en un
servidor Linux; el directorio del servicio tftp suele ser `/srv/tftp/`.

Después copia la imagen compilada para fu740, `zcore-fu740.itb`, al directorio
del servicio tftp.

Enciende la placa fu740 y entra en la línea de comandos de U-Boot:

```
# Configura la IP de la placa y la del servidor
setenv ipaddr <IP>
setenv serverip <Server IP>

# Carga la imagen por tftp
tftp 0xa0000000 zcore-fu740.itb

# Ejecuta
bootm 0xa0000000
```

### Documentación de la fu740

La documentación recopilada de la placa Unmatched fu740 está en:
https://github.com/oscomp/DocUnmatchedU740/blob/main/unmatched.md
