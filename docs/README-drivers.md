# Escribir un driver para Eclipse OS

En Eclipse OS un driver es un **programa normal** que corre como proceso,
no un módulo del núcleo. El núcleo le cede un dispositivo PCI entero: sus
registros, memoria para DMA y su interrupción. Si el driver se cae, el resto
del sistema sigue en pie. Esta guía va de cero a un fichero `.drv` que
cualquiera instala desde lunarfile o desde el panel de control.

El driver de ejemplo completo está en
[`tools/eclipse-udrivers/src/bin/udrv-ejemplo.rs`](../tools/eclipse-udrivers/src/bin/udrv-ejemplo.rs).
Compila y es el punto de partida recomendado.

## Cómo funciona

1. El usuario instala `mi-driver-14c3-0616.drv`. El panel copia el fichero,
   crea su servicio de eclipse-init y añade `pci-14c3-0616@0` a `udriver=` en
   `rboot.conf`.
2. Al arrancar, el núcleo ve `udriver=pci-14c3-0616@0` y no toca la primera
   función PCI con fabricante `14c3` y dispositivo `0616`. Tampoco la toca su
   propio driver, si lo tiene. Guarda su primer BAR de memoria y su MSI, y la
   lista en `/proc/udrivers`.
3. eclipse-init ve el dispositivo en `/proc/udrivers` y arranca el servicio:
   `/usr/lib/eclipse/udrivers/udrv-14c3-0616 pci-14c3-0616@0`.
4. El driver **reclama** el dispositivo (`claim_pci`), mapea sus registros,
   pide memoria DMA y espera interrupciones. Lo que ofrece al resto del
   sistema lo ofrece como un nodo en `/dev` (o como disco o tarjeta de red,
   como hacen los drivers de serie).
5. Si el proceso muere, eclipse-init lo vuelve a arrancar (`type = respawn`).

## Lo que necesitas

- Rust con el target musl: `rustup target add x86_64-unknown-linux-musl`.
- El crate `eclipse-udrivers` de este repositorio. Trae la llamada para
  reclamar el dispositivo, la memoria DMA, las interrupciones y los nodos de
  `/dev`.

```toml
# Cargo.toml de tu driver
[package]
name = "mi-driver"
version = "0.1.0"
edition = "2021"

[dependencies]
udrivers = { package = "eclipse-udrivers", git = "https://github.com/Pryancito/eclipse" }
```

## El driver, paso a paso

El argumento que recibe es el dispositivo, tal como lo nombra el núcleo:
`pci-VVVV-DDDD@N`.

### 1. Reclamar el dispositivo

```rust
use udrivers::claim;

let claim = claim::claim_pci(0x14c3, 0x0616, 0, 100)?;
```

Solo un proceso puede tener el dispositivo a la vez. El último número es
cuántas veces se reintenta (cada 10 ms) si otro proceso aún lo tiene, por
ejemplo el driver anterior, que acaba de morir. Devuelve un `Claim`:

| Campo | Qué es |
|---|---|
| `size` | Tamaño de la ventana de registros (el primer BAR de memoria). |
| `pci_id` | `vendor << 16 \| device`. |
| `irq` | La línea, solo informativa (`u32::MAX` si no hay). |
| `interrupt` | El objeto de interrupción para esperar, o `None`. |
| `bti` | Para pedir memoria DMA. |

### 2. Los registros

```rust
let regs: *mut u8 = claim.map_regs()?;
let status = unsafe { (regs.add(0x08) as *const u32).read_volatile() };
```

La ventana está mapeada sin caché. Lee y escribe **siempre** con
`read_volatile` y `write_volatile`, y solo dentro de `claim.size` bytes.

### 3. Memoria DMA

```rust
use udrivers::claim::Dma;

let ring = Dma::new(claim.bti, 4096, None)?;
// ring.ptr: donde la ves tú. ring.daddr: la dirección que le das al dispositivo.
```

Es memoria contigua, fijada y puesta a cero, en páginas enteras. Es tuya
mientras viva el proceso.

### 4. Interrupciones

```rust
if let Some(irq) = claim.interrupt {
    std::thread::spawn(move || {
        while claim::interrupt_wait(irq).is_ok() {
            // lee el estado del dispositivo y atiende lo que haya
        }
    });
}
```

La interrupción es un MSI y no se enmascara: el driver tiene que dejar
quieto el dispositivo en cada vuelta, como pida su hoja de datos.

### 5. Ofrecerlo al sistema

La forma más general es un nodo en `/dev` que sirve el propio proceso:

```rust
use udrivers::served::{Device, Node};

let node = Node::serve("mi-driver", 240, 0, 0o666)?;
node.run(&std::sync::Mutex::new(MiDispositivo { /* ... */ }));
```

`MiDispositivo` implementa el trait `served::Device`: `open`, `read`,
`write`, `release` e `ioctl`, cada uno devolviendo un `errno` si falla. El
major 240 es el rango local. Elige un minor que no use otro driver.

Para un disco o una tarjeta de red, mira cómo lo hacen
`udrv-nvme` / `udrv-ahci` (`blk.rs`) y `udrv-virtio-net` (`net.rs`).

Añade `panic = "abort"` a `[profile.release]`: un driver que entra en
pánico no tiene a dónde volver.

## Compilar y empaquetar

Los binarios de Eclipse OS son estáticos y no PIE:

```sh
RUSTFLAGS="-C relocation-model=static -C target-feature=+crt-static" \
  cargo build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/mi-driver mi-driver-14c3-0616.drv
```

El nombre del `.drv` **tiene que llevar los ids** del dispositivo
(`VVVV-DDDD`, en hex de cuatro cifras, al final del nombre). Así el panel
sabe para qué dispositivo es. Los ids salen en la página «Dispositivos» del
panel (COPIAR IDS) o con `lspci -n`.

En aarch64 y riscv64 el target es `aarch64-unknown-linux-musl` o
`riscv64gc-unknown-linux-musl`.

## Instalar y probar

- **Desde lunarfile:** doble clic en el `.drv`. Se abre el panel con el
  diálogo de instalación ya apuntando al dispositivo.
- **Desde el panel:** Dispositivos, elige el dispositivo y pulsa INSTALAR
  DRIVER…. Busca los `.drv` en `~`, `~/Descargas` y `~/Downloads`.
- **A mano:**
  `eclipse-udriver instalar 14c3 0616 mi-driver-14c3-0616.drv` y luego añade
  `pci-14c3-0616@0` a `udriver=` en la línea `cmdline=` de
  `/boot/efi/rboot.conf`.

Después reinicia y comprueba:

| Qué | Dónde |
|---|---|
| El núcleo cedió el dispositivo | `cat /proc/udrivers` |
| El servicio corre | `/run/eclipse-init/services`, o la página del panel («en uso · usuario») |
| La salida del driver | `/tmp/udrv-14c3-0616.log` |

Para desarrollar sin reinstalar cada vez, para el servicio
(`: > /run/eclipse-init/ctl/stop.udrv-14c3-0616; kill -HUP 1`) y lanza tu
binario a mano con el mismo argumento.

Para quitarlo, usa QUITAR DRIVER en el panel o
`eclipse-udriver quitar 14c3 0616`, y quita el `pci-…` de `udriver=`.

## Límites que debes conocer

- **IOMMU solo en x86_64, y solo si la máquina la tiene.** Las direcciones
  DMA son físicas. En un PC con VT-d o AMD-Vi, el núcleo la enciende por
  defecto y tu dispositivo solo alcanza lo que fijaste con `Dma::new`; una
  escritura fuera de eso falla y aparece en `dmesg` como `iommu: DMA
  fault`. Sus interrupciones pasan por ella también: tu dispositivo solo
  puede lanzar la suya (`iommu=noir` deja solo esa parte apagada). Sin
  IOMMU (otras arquitecturas, máquinas sin ella, o arrancando
  con `iommu=off`), un driver con fallos puede escribir en cualquier parte
  de la memoria. El panel lo avisa al instalar. Prueba en una máquina que
  puedas reiniciar.
- **Si el driver muere, el dispositivo espera al reinicio.** El núcleo no
  sabe parar un dispositivo que no conoce, así que lo deja fuera de uso con
  su memoria fijada. Sal limpiamente: deja el dispositivo parado antes de
  terminar.
- **Un dispositivo por identidad.** El instalador cede la primera función
  con esos ids (`@0`). Para una segunda tarjeta igual, añade `@1` a mano.
- **Solo el primer BAR de memoria.** No hay acceso al espacio de
  configuración PCI ni a los BAR de puertos.
- **Para x86_64 la interrupción es MSI.** En las otras arquitecturas es una
  línea por nivel.

## Referencia

- `tools/eclipse-udrivers/src/claim.rs`: `claim_pci`, `Dma`, `interrupt_wait`.
- `tools/eclipse-udrivers/src/served.rs`: nodos de `/dev`.
- `drivers/src/user.rs`: qué cede el núcleo y cómo (`udriver=`).
- `loader/src/linux.rs`, `ECLIPSE_DEVICE_CLAIM`: la llamada al sistema.
- `xtask/src/linux/panel.rs`, `write_udriver`: lo que hace la instalación.
- Drivers de serie para leer: `udrv-nvme`, `udrv-ahci`, `udrv-hda`,
  `udrv-xhci`, `udrv-virtio-net`.
