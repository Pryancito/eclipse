# zCore en la placa StarFive VisionFive

**Español** · [English](README-visionfive.en.md)

> Notas heredadas del zCore original (rCore-OS), traducidas del chino. No se
> han vuelto a probar en Eclipse.

## 1. Grabar la placa VisionFive

> **NOTICE** Las operaciones del equipo host que describe este documento se han probado en Ubuntu 20.04.

### 1. Montar los servicios necesarios en el PC industrial

```bash
# Montar dhcpd
# 1. Instalar dhcp3-server
sudo apt install isc-dhcp-server

# 2. Elegir la interfaz del servicio dhcp: la interfaz de red física real
vim /etc/default/isc-dhcp-server
------------------------------------------------
INTERFACESv4="enp2s0" # el puerto dhcpv4 es enp2s0
INTERFACESv6=""
------------------------------------------------

# 3. Dar una dirección IP a esa interfaz y levantarla
vim /etc/netplan/01-network-manager-all.yaml
-----------------------------------------------------------
# Let NetworkManager manage all devices on this system
network:
  version: 2
  renderer: NetworkManager
  ethernets:
    eno1:
      addresses: []
      dhcp4: true
      optional: true
    enp2s0:
      addresses: [192.168.10.10/24]
      gateway4: 192.168.10.1
      nameservers:
        addresses: [114.114.114.114,8.8.8.8]
      dhcp4: no
------------------------------------------------------------
sudo netplan appply

# 4. Configurar dhcp
vim /etc/dhcp/dhcpd.conf
----------------------------------------------------
subnet 192.168.10.0 netmask 255.255.255.0 {
  range 192.168.10.120 192.168.10.128;
  option routers 192.168.10.10;
  option subnet-mask 255.255.255.0;
  option broadcast-address 192.168.10.255;
  option domain-name-servers 192.168.10.10;
}
-----------------------------------------------------

# 5. Arrancar el servicio dhcpd
systemctl restart isc-dhcp-server

# 6. Comprobar el estado del servicio dhcpd
systemctl status isc-dhcp-server

# Montar tftpd
# 1. Instalar tftpd-hpa
sudo apt install tftpd-hpa

# 2. Cambiar la configuración
--------------------------------------------------------------
TFTP_USERNAME="tftp"
TFTP_DIRECTORY="/root/work/tftpboot" # ruta de descarga de ficheros de tftp
TFTP_ADDRESS=":69"
TFTP_OPTIONS="--secure"
--------------------------------------------------------------

# 3. Arrancar el servicio tftpd
systemctl restart tftpd-hpa

# 4. Comprobar el estado del servicio tftpd
systemctl status tftpd-hpa
```

### 2. Conectar el PC industrial y la placa VisionFive

![avatar](visionfive.jpg)

```bash
# El cable negro de punta redonda es la alimentación del PC industrial
# El cable de red amarillo es el que usa la placa para obtener dirección por dhcp
# El cable de red azul es el que usa el PC industrial para descargar de la red pública; se puede ignorar
# El conector USB blanco alimenta la placa, 5 V/3 A
# El conector USB negro es la consola serie
```

Abre el asistente de puerto serie (minicom) en el PC industrial; tras encender la placa, a los pocos segundos entra sola en la línea de comandos de u-boot

```bash
Welcome to minicom 2.7.1

OPTIONS: I18n
Compiled on Dec 23 2019, 02:06:26.
Port /dev/ttyUSB0, 12:54:20

Press CTRL-A Z for help on special keys


VisionFive #
VisionFive #
VisionFive #

# Aquí puedes usar `help` para ver qué comandos hay
# y `printenv` para ver qué variables de entorno hay configuradas

# 1. Escribe el comando `dhcp` para que la placa obtenga una dirección IP
# Ojo: puede que empiece a descargar sola la imagen del servidor; interrúmpelo con Ctrl+C
dhcp

# 2. Dirección del cliente tftp (esta máquina)
setenv ipaddr 192.168.10.121 # la dirección que dio dhcp

# 3. Dirección del servidor tftp (el PC industrial)
setenv serverip 192.168.10.10 # tiene que estar en la misma subred que tu IP

# 4. Comprobar que la red funciona
ping 192.168.10.10 # prueba a hacer ping al host (opcional)

# 5. Guardar la configuración
saveenv # la guarda en la flash; ya no hace falta volver a configurarla
```

### 3. Crear la imagen en formato itb

```bash
# 1. Descarga `starfive_fb.h`, `starfive_vic7100_clk.dtsi` y `starfive_vic7100_evb.dts` del repositorio oficial de VisionFive

# 2. Compila el dtb con estos comandos
cpp -nostdinc -I include -undef -x assembler-with-cpp starfive_vic7100_evb.dts starfive.dts.0

dtc -o starfive.dtb starfive.dts.0	# hay un starfive.dtb precompilado en el directorio prebuilt

# 3. Comprime la imagen del kernel en `z.bin.gz`
gzip -9 -cvf z.bin > z.bin.gz

# 4. Crea el fichero itb
mkimage -f zcore-starfive.its z.itb	# zcore-starfive.its es el starfive_fdt.its del directorio prebuilt, así que mkimage -f prebuilt/firmware/riscv/starfive_fdt.its z.itb hace lo mismo

Este es el contenido de zcore-starfive.its
--------------------------------------------------------------------
/*
 * U-Boot uImage source file for "zCore-visionfive"
 */

/dts-v1/;

/ {
    description = "U-Boot uImage source file for zCore-visionfive";
    #address-cells = <1>;

    images {
        kernel {
            description = "Linux kernel for zCore-visionfive";
            data = /incbin/("./z.bin.gz");
            type = "kernel";
            arch = "riscv";
            os = "linux";
            compression = "gzip";
            load = <0x80200000>;
            entry = <0x80200000>;
        };
        fdt {
            description = "Flattened Device Tree blob for zCore-visionfive";
            data = /incbin/("./starfive.dtb");
            type = "flat_dt";
            arch = "riscv";
            compression = "none";
        };
    };

    configurations {
        default = "conf";
        conf {
            description = "Boot Linux kernel with FDT blob";
            kernel = "kernel";
            fdt = "fdt";
        };
    };
};
--------------------------------------------------------------------

# 5. Pon `z.itb` en la ruta de descarga del servidor tftp
cp z.itb /path/to/tftpboot
```

### 4. Subir el fichero itb a la memoria de la placa y arrancar desde esa dirección

```bash
VisionFive #tftpboot 0xc0000000 z.itb
Speed: 1000, full duplex
Using dwmac.10020000 device
TFTP from server 192.168.10.10; our IP address is 192.168.10.121
Filename 'z.itb'.
Load address: 0xc0000000
Loading: ##################################################  1.5 MiB
         11.4 MiB/s
done
Bytes transferred = 1584003 (182b83 hex)
VisionFive #bootm 0xc0000000
## Loading kernel from FIT Image at c0000000 ...
   Using 'conf' configuration
   Trying 'kernel' kernel subimage
     Description:  Linux kernel for zCore-FU740
     Type:         Kernel Image
     Compression:  gzip compressed
     Data Start:   0xc00000e8
     Data Size:    1564993 Bytes = 1.5 MiB
     Architecture: RISC-V
     OS:           Linux
     Load Address: 0x80200000
     Entry Point:  0x80200000
   Verifying Hash Integrity ... OK
## Loading fdt from FIT Image at c0000000 ...
   Using 'conf' configuration
   Trying 'fdt' fdt subimage
     Description:  Flattened Device Tree blob for zCoreFU740
     Type:         Flat Device Tree
     Compression:  uncompressed
     Data Start:   0xc017e2ec
     Data Size:    17270 Bytes = 16.9 KiB
     Architecture: RISC-V
   Verifying Hash Integrity ... OK
   Booting using the fdt blob at 0xc017e2ec
   Uncompressing Kernel Image
   Loading Device Tree to 00000000ff6e1000, end 00000000ff6e8375 ... OK

Starting kernel ...


boot page table launched, sstatus = 0x8000000200046000
kernel (physical): 0000000080200000..0000000085ce70b8
kernel (remapped): ffffffc080200000..ffffffc085ce70b8
device tree:       00000000ff6e1000..00000000ff6e6000

hart0 is booting...
hart1 is the primary hart.

[ 28.007868 WARN  1 0:0 zcore_drivers::builder::devicetree] device-tree: failed to parsing node "gpio@11910000": InvalidParam
[ 28.022102 ERROR 1 0:0 zcore_drivers::irq::riscv_intc] invalid SCAUSE value 0xb!
[ 28.032369 ERROR 1 0:0 zcore_drivers::irq::riscv_intc] invalid SCAUSE value 0xb!
[ 28.074321 WARN  1 0:0 linux_syscall] brk: unimplemented
[ 28.082355 WARN  1 0:0 linux_syscall] brk: unimplemented
[ 28.090609 WARN  1 0:0 linux_syscall::vm] mprotect: unimplemented
[ 28.100315 WARN  1 0:0 linux_syscall::vm] mprotect: unimplemented
[ 28.109179 WARN  1 0:0 linux_syscall] getuid: unimplemented
[ 28.117922 WARN  1 0:0 linux_syscall] getpgid: unimplemented
[ 28.126330 WARN  1 0:0 linux_syscall] setpgid: unimplemented
[ 28.134857 WARN  1 0:0 linux_object::fs::stdio] stdout TCGETS | TIOCSPGRP, pretend to be tty.
[ 28.146419 WARN  1 0:0 linux_object::fs::stdio] stdin TCGETS | TIOCSPGRP, pretend to be tty.
[ 28.157926 WARN  1 0:0 linux_syscall] geteuid: unimplemented
[ 28.166389 WARN  1 0:0 linux_syscall] geteuid: unimplemented
/ #


## 2. Problemas del port a VisionFive

### 1. Grabación

Se graba con el comando `tftpboot 0xc0000000 z.itb`; la dirección `0xc0000000` tiene que dejar espacio de sobra para el kernel descomprimido.

### 2. Mapa de memoria

El u-boot de la placa StarFive tiene que fijar dónde se cargan el kernel y el device tree, y luego le pasa al kernel la posición del device tree en el registro `a1`. En la práctica el device tree queda por encima de 1 GiB, y zCore solo admitía 1 GiB, así que daba un error de memoria ilegal. Hay que cambiar la disposición del mapa de memoria; los cambios están en estos dos commits:

[commit1](https://github.com/rcore-os/zCore/commit/227956d5df401c8f8f2fa746f8aa911d3530637f)

[commit2](https://github.com/rcore-os/zCore/commit/16772b9363d02945863008c6a4639ad1cb37eed4)

### 3. Había un bug en el manejo de interrupciones: cada interrupción tiene que atenderla el núcleo actual. El arreglo está en

[commit](https://github.com/rcore-os/zCore/commit/55b3145442f0f70c01527c20e87988d26c01a39b)

### 4. Driver del puerto serie

La placa StarFive admite el driver `16550`; basta con adaptar el código. El cambio está en

[commit](https://github.com/rcore-os/zCore/commit/55b3145442f0f70c01527c20e87988d26c01a39b)

### 5. Interrupción sin registrar

Vimos que la placa StarFive, al arrancar desde el núcleo `1`, lanza la interrupción `131`, que no está en el device tree y por tanto no se registra en el kernel, así que da error. Al manejar las interrupciones le bajamos la prioridad para enmascararla, ver:

[commit](https://github.com/rcore-os/zCore/commit/55b3145442f0f70c01527c20e87988d26c01a39b)
