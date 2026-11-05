# zCore en riscv64: placa T-HEAD C910 Light

**Español** · [English](README-C910.en.md)

> Notas heredadas del zCore original (rCore-OS), traducidas del chino. La
> placa se sigue compilando con `PLATFORM=c910light` en `zCore/Makefile`, pero
> no se ha vuelto a probar en Eclipse.

## Placa de validación T-HEAD C910 Light: cómo usarla

### Compilar la imagen del kernel zCore

Compilar el kernel zCore:
```
cd zCore
make build LINUX=1 MODE=release ARCH=riscv64 PLATFORM=c910light
```

Crear la imagen de sistema para u-boot:
```
mkimage -A riscv -O linux -C none -T kernel -a 0x200000 -e 0x200000 -n "zCore for c910" -d ../target/riscv64/release/zcore.bin uImageC910
```

### Compilar la imagen de opensbi
```
git clone https://github.com/elliott10/opensbi.git -b thead_light-c910

cd opensbi

make PLATFORM=generic CROSS_COMPILE=/path/to/toolchain/bin/riscv64-unknown-linux-gnu-
# genera el fw_dynamic.bin que hace falta
```
Nota: la cadena de herramientas original se compiló a partir del repositorio
oficial https://gitee.com/thead-yocto/xuantie-yocto.git. En teoría se puede
sustituir por otra.

### Arrancar desde u-boot

Pon la imagen de opensbi compilada, `fw_dynamic.bin`, y la imagen de sistema
`uImageC910` en el directorio de un servidor tftp ya configurado.<br>
En la línea de comandos de u-boot de la placa C910 Light, con la red
configurada, ejecuta:
```
ext4load mmc 0:2 $aon_ram_addr light_aon_fpga.bin; ext4load mmc 0:2 $dtb_addr ${fdt_file};

tftp $opensbi_addr fw_dynamic.bin;
tftp $kernel_addr uImageC910;

bootslave; run finduuid; run set_bootargs; bootm $kernel_addr - $dtb_addr;

```

## Placa de validación T-HEAD C910 Light: notas del port

### 1. Análisis inicial del sistema y creación de la imagen

<img src="img/c910-light.jpeg" alt="c910-light" style="zoom:25%;" />

La placa de desarrollo C910 Light es la de la imagen. Al recibirla, primero
conviene leer el manual de usuario oficial para conocer los componentes
básicos y cómo se conectan la alimentación, el puerto serie y demás.<br>
Manual de usuario: https://gitee.com/thead-yocto/documents/blob/master/en/user_guide/T-Head%20Yeying1520%20Yocto%20User%20Guide.pdf <br>

Con la alimentación conectada y el puerto serie unido al host, aparecen cuatro
puertos serie `/dev/ttyUSBX`; el de depuración es al que se conecta el host, y
pulsando cualquier tecla mientras arranca u-boot se entra en modo línea de
comandos.<br>
Desde la línea de comandos de u-boot se puede usar la red cableada y cargar
por tftp la imagen del sistema operativo que se va a arrancar.<br>

```
# minicom -b 115200 -D /dev/ttyUSB2

U-Boot 2020.01-ge0ddd4721a (Dec 14 2021 - 22:26:59 +0800)

CPU:   rv64imafdcvsu
Model: T-HEAD c910 light
DRAM:  1 GiB
C910 CPU FREQ: 1500MHz
AHB2_CPUSYS_HCLK FREQ: 250MHz
AHB3_CPUSYS_PCLK FREQ: 125MHz
PERISYS_AHB_HCLK FREQ: 250MHz
PERISYS_APB_PCLK FREQ: 62MHz
GMAC PLL POSTDIV FREQ: 1000MHZ
DPU0 PLL POSTDIV FREQ: 1188MHZ
DPU1 PLL POSTDIV FREQ: 1188MHZ
MMC:   sdhci@ffe7080000: 0, sd@ffe7090000: 1
Loading Environment from MMC... OK
In:    serial@ffe7014000
Out:   serial@ffe7014000
Err:   serial@ffe7014000
Net:   eth0: ethernet@ffe7070000
Hit any key to stop autoboot:  0 
C910 Light# 

# setenv ipaddr <IP>
# setenv serverip <Server IP>

```

Al principio se probó el mismo método que con la placa fu740 para crear la
imagen de zCore y arrancarla por tftp: en la línea de comandos de u-boot se
configuran la IP de la placa y la del servidor, se carga la imagen por tftp y
por último se ejecuta con bootm.<br>

Todo lo demás iba bien, pero al arrancar la imagen de zCore así aparecía un
error que decía que no reconocía la imagen.
```
Filename 'uImageC910'.
Load address: 0x200000
Loading: #################################################################
         #############################################
         5.1 MiB/s
done
Bytes transferred = 1609651 (188fb3 hex)
Wrong Image Format for bootm command
ERROR: can't get kernel image!
```
Al analizarlo se vio que las distintas versiones de u-boot reconocen formatos
de imagen distintos.<br>
La imagen de zCore de la placa fu740 es la nueva `FIT image`, basada en un
script `.its`;<br>
el u-boot de la placa C910 Light solo admite la `old legacy image`, que se
crea así:

```
mkimage -A riscv -O linux -C none -T kernel -a 0x200000 -e 0x200000 -n "zCore for c910" -d ../target/riscv64/release/zcore.bin uImageC910
```

### 2. Construir el arranque de zCore

A partir de la imagen de Linux que ya traía la placa se sabe que la dirección
de carga es `Load Address: 0x200000` y el punto de entrada
`Entry Point: 0x200000`.

Con la dirección de entrada `0x200000`, se cambió la variable `BASE_ADDRESS`
del script de enlazado `linker.ld` de zCore al valor correspondiente, se
compiló la imagen y, en u-boot, se cargó por red con `tftp` y se intentó
arrancar con `bootm`; tras saltar al kernel no pasaba nada.

Había dos posibilidades: o la salida por el puerto serie de zCore, que por
defecto llama al print de opensbi, no funcionaba, o algo fallaba al construir
la memoria virtual y saltar a ella. Primero se hizo que zCore arrancara con
memoria física, cambiando temporalmente las variables de desplazamiento del
arranque del directorio `zCore/src/platform/riscv/` a los valores que tocan
para una entrada en `0x200000`; con eso desaparecía el segundo problema y
quedaba el primero, la salida de opensbi.

Para comprobar si la salida de opensbi funcionaba, se probó la imagen de zCore
más reducida posible, llamando a `SBI_CONSOLE_PUTCHAR` de opensbi en el punto
más temprano del código Rust. Seguía sin salir nada, y aquí el problema se
atascó.

La C910 Light es una placa grande y cuadrada con muchísimas interfaces,
incluido un puerto JTAG para depurar la CPU. Se montó el entorno de depuración
siguiendo la documentación oficial de T-Head:
https://occ.t-head.cn/document?temp=linux&slug=t-head-debug-server-user-manual

<img src="img/c910-jtag.jpg" alt="jtag" style="zoom:25%;" />

Con gdb conectado al JTAG Debug Server ya se podía ejecutar paso a paso la CPU
C910, aunque a veces no se podía poner un breakpoint y había que cargar
primero la imagen de zCore en memoria y luego poner los breakpoints de
funciones.

Paso a paso por JTAG se vio que, tras ejecutar `ecall SBI_CONSOLE_PUTCHAR`,
no pasaba nada, cuando debería salir un carácter por el puerto serie. O no
había un opensbi válido, o el driver del puerto serie fallaba.

Arreglar la salida por el puerto serie era lo más directo, así que, tomando
como referencia el código serie de u-boot, se activó la funcionalidad básica
de salida:

```
// T-HEAD C910 light
pub fn uart_put(c: u8) {
    let ptr = BADDR as *mut u32;
    unsafe {
        //LSR bit:THRE
        while ptr.add(5).read_volatile() & (1 << 5) == 0 {}

        //transmisor vacío, los bits válidos de THR son 8
        ptr.add(0).write_volatile(c as u32);
    }
}
pub fn uart_get() -> Option<u8> {
    let ptr = BADDR as *mut u32;
    unsafe {
        //si el bit DR de LSR es 1 hay datos
        if ptr.add(5).read_volatile() & 0b1 == 0 {
            None
        } else {
            Some((ptr.add(0).read_volatile() & 0xff) as u8)
        }
    }
}
```

Con eso zCore ya podía arrancar y sacar texto.

Después apareció un fallo al parsear el DTB del device tree. Tras investigar
se vio que el kernel zCore pisaba la memoria donde se había cargado el DTB;
moviendo la dirección del DTB un poco más arriba se parseaba bien.

![c910 uart](img/c910-run-uart.png)

Más tarde, hablando con el soporte técnico de T-Head, se consiguieron la
documentación y el código de la placa C910 Light, con más información útil:
se pueden compilar a medida u-boot, el kernel Linux, el sistema de ficheros y
el resto de componentes que necesita la placa.

Analizando juntos la documentación y el código fuente de la placa quedó clara
su cadena de arranque:

```
U-Boot corre en modo M
	|
	V
OpenSBI lo carga U-Boot, y salta de modo M a modo S
	|
	V
Linux lo carga U-Boot, y corre en modo S
```

Así que, si U-Boot carga directamente un kernel zCore solo, no hay OpenSBI con
el que imprimir, y el efecto es que no sale nada. Localizado el problema, se
pasó a arreglarlo.

Al probar a ejecutar solo opensbi desde u-boot, no salía el típico logo grande
de «OPENSBI», así que se arregló el puerto serie de opensbi: al driver le
faltaba reconocer el modelo de la placa C910 Light, `snps,dw-apb-uart`, y
acababa usando el genérico `ns16550`. Se aplicó este parche:
https://github.com/elliott10/opensbi/commit/404951dd5b047873fa023545eafeb1fa2a9c5838

Compilado y ejecutado, ¡la salida de OpenSBI funciona!

### 3. El problema de la tabla de páginas de memoria virtual de zCore

zCore siguió avanzando y se topó con un problema nuevo: se quedaba parado
justo después de cambiar de tabla de páginas (`switch table`).

Era un problema muy raro: en el flujo normal aquí se mapean todas las
secciones del kernel zCore y el resto de espacios de direcciones en una tabla
de páginas nueva, con los mismos parámetros y el mismo proceso que en otras
placas como qemu o D1. En teoría no había motivo para un bug. Se paraba aquí:

```
[ 48.117274 INFO  0 0:0 kernel_hal::imp::arch::vm] initialized kernel page table @ 0x5c18000
[ 48.128044 DEBUG 0 0:0 kernel_hal::imp::arch::vm] cpu 0 switch table 2ec000 -> 5c18000
```

Se volvió al depurador JTAG para seguir el comportamiento de la CPU instrucción
a instrucción al llegar aquí.

Con un breakpoint antes del `switch table` y siguiendo paso a paso, se vio que
tras `csrw satp` el programa se descontrolaba y saltaba una excepción. La
sospecha recayó en la tabla de páginas que usaba satp.

El código de Linux es una referencia excelente para desarrollar zCore, así que
se comparó con el proceso de Linux al cambiar de tabla de páginas y escribir
satp, y se encontraron algunas pistas:

<img src="img/c910-linux-pg.png" alt="c910 pagetable" style="zoom:75%;" />

En el código de Linux para riscv, al construir las entradas de la tabla de
páginas de memoria virtual:

al espacio de direcciones de la imagen del kernel, `PAGE_KERNEL`, se le añaden
flags extra respecto a una página normal: `CACHE`, `SHARE`, `BUF`;

al espacio de direcciones de los dispositivos, `PAGE_IOREMAP`, se le añaden
los atributos `SHARE`, `SO`.

La definición de estos bits extra de las entradas se encuentra en el manual del
chip C910: los atributos de página extra están en los bits `63:59`, e incluyen
`SO`, `CACHE`, `BUF`, `SHARE` y `SEC`.

Según los manuales de otros chips, estos atributos extendidos de las CPU de
T-Head no existen solo en la C910: también están en la CPU C906.

![c910 pte flags](img/c910-pte-flags.png)

En zCore se probó primero a poner los bits `CACHE`, `SHARE` y `BUF` del kernel
al construir su tabla de páginas, pero el problema seguía: se paraba igual tras
el `switch table`, sin saber por qué.

Si son bits de una extensión propia sobre el estándar RISC-V, en teoría tenían
que poder desactivarse, así que se siguió buscando en el manual del chip y, en
efecto, en el registro de estado extendido `MXSTATUS` estaba el bit
correspondiente, `MAEE`.

![c910 mxstatus](img/c910-mxstatus.png)

El bit extendido MAEE de la CPU C910 decide si se activan los atributos de
dirección extendidos de la MMU, es decir, `SO`, `CACHE`, `BUF`, `SHARE` y
`SEC`.

Con `MAEE = 0`, la MMU no usa los atributos extendidos `SO`, `CACHE`, `BUF`,
`SHARE` y `SEC`;

con `MAEE = 1`, se activan los atributos extendidos `SO`, `CACHE`, `BUF`,
`SHARE` y `SEC` de las PTE de la MMU, y se pueden poner los bits
correspondientes en cada entrada.

![c910 mxstatus](img/c910-maee.jpeg)

Así que se desactivaron sin más los atributos extendidos de las entradas de la
tabla de páginas, con `MAEE = 0`, y efectivamente el problema de pararse en el
`switch table` desapareció. zCore pudo seguir y empezar a parsear los nodos
del device tree; a continuación se topó con direcciones de dispositivo
demasiado altas, que provocaban errores al procesar las direcciones de los
nodos...

### 4. Probar el ajuste MAEE de los atributos extendidos de las PTE en la placa CR1825

En una conversación anterior con DF sobre el bug que tenía entonces la CR1825,
al principio se sugirió que quizá el segmento `.BSS` del kernel no se estaba
limpiando del todo.

El año anterior, al portar a D1 el zCore recién extendido con soporte riscv,
también hubo un problema de interbloqueo con las instrucciones atómicas del
spinlock. Se le consultó entonces a JYK, que dio la pista: el segmento `.bss`
no se limpiaba bien, así que las variables atómicas sin inicializar tenían
valores aleatorios y el interbloqueo era permanente.

<img src="img/c906-clear-bss.jpeg" alt="c906 clear bsss" style="zoom:50%;" />



De todos modos, la limpieza del `.bss` se añadió poco después. Aunque luego se
reescribió la parte de arranque, DF confirmó que el `.bss` sí se limpiaba.

Mientras se depuraba la C910, al estudiar en el manual del chip los atributos
de página y los demás bits de registro relacionados, también se comparó con el
manual de la C906, y las dos CPU apenas se diferencian en esos atributos.

Tras arreglar el parón en el `switch table` de la C910 Light, se habló con DF
y se consiguió el código fuente de opensbi, u-boot, Linux, etc. de la CR1825,
sobre todo para compararlo con el código de arranque de la C910 Light.

El código de arranque de la CR1825, con CPU C906, resultó muy parecido al de
la placa Light con CPU C910, incluido el proceso de construir la memoria
virtual: también configuraba los bits extendidos `cache`, `share` y `buf` en
las entradas de la memoria virtual del kernel, mientras que el zCore original
nunca configuraba esos bits.

De ahí la sospecha de que el ajuste de los bits extendidos de las PTE tuviera
mucho que ver con el problema del bloqueo atómico de la CR1825: una operación
atómica tiene justamente que comprobar y reescribir un valor en memoria
virtual.

Para comprobarlo se creó una imagen de arranque `fip.bin` para la CR1825 que
desactiva directamente los atributos extendidos de las entradas de la tabla de
páginas, con `MAEE = 0`; el cambio es este:
https://github.com/elliott10/opensbi/commit/404951dd5b047873fa023545eafeb1fa2a9c5838

Se le pidió a DF que lo probara en remoto y, por suerte, el problema del
bloqueo atómico también se resolvió: ¡sí tenía que ver con las entradas
extendidas de T-Head!

<img src="img/c906-amo-1.jpg" alt="c906 amo 1" style="zoom:50%;" />

<img src="img/c906-amo-2.jpg" alt="c906 amo 1" style="zoom:50%;" />

Después, en el grupo, se siguió hablando con YDR y otros sobre cómo funcionan
los bits extendidos de T-Head, y un técnico de T-Head confirmó: «El bit
cacheable de la PTE indica que la página puede guardarse en la caché de la
906; si es 0 no entra en caché. El bit bufferable de la PTE controla si se
activa la señal bufferable en las transferencias del bus; a la CPU le da igual
en su comportamiento». Y: «Las instrucciones amo necesitan caché, es decir, no
se pueden usar en zonas sin caché».

Una vez resuelto el bloqueo de la CR1825, el resto avanzó bastante bien.

Con `maee` activado, hay que tener en cuenta que los atributos extra de las
`PTE` son distintos para la imagen del kernel y para el espacio de direcciones
de los dispositivos.

Según el código de Linux, al construir las entradas de la tabla de páginas de
memoria virtual:

al espacio de direcciones de la imagen del kernel, `PAGE_KERNEL`, se le añaden
flags extra respecto a una página normal: `CACHE`, `SHARE`, `BUF`;

al espacio de direcciones de los dispositivos, `PAGE_IOREMAP`, se le añaden
los atributos `SHARE`, `SO`.

### 5. Direcciones de dispositivo demasiado altas

Como se dijo antes, tras construir la memoria virtual, al parsear los nodos del
device tree zCore se topa con direcciones de dispositivo demasiado altas, que
provocan errores al procesar las direcciones de los nodos.

Por ejemplo, la dirección base del puerto serie que da el device tree es
`0xffe7014000`, más grande que una dirección de dispositivo típica, como la
base del puerto serie de Qemu, `0x10000000`. Varias direcciones base de
dispositivos de la placa C910 Light son así de grandes. En el espacio físico
estas direcciones no dan problemas, pero cuando zCore ya ha construido la
memoria virtual del kernel y les suma un `offset` común, como
`0xffffffe000200000`, se salen del espacio de direcciones virtuales del modo
SV39.

Para el desbordamiento basta un tratamiento sencillo: volver a poner a `1` los
bits reservados que se desbordaron:

```
paddr | (0x1ffffff << 39)
```

### 6. zCore en marcha en la placa C910 Light

zCore llega sin problemas a la shell de busybox:

![c910 zcore](img/c910-zcore-run.png)
