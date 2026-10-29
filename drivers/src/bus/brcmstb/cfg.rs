//! El espacio de configuración del puente del BCM2712: **índice más ventana**,
//! que no es ECAM plano.
//!
//! En ECAM plano el `bus/devfn/registro` *es* una dirección: se compone el
//! offset y se lee ahí. Este puente no tiene esa ventana. Tiene dos registros:
//! en [`EXT_CFG_INDEX`] se deja el `bus/devfn` del dispositivo que se quiere
//! mirar, y por los 4 KiB de [`EXT_CFG_DATA`] se ve su espacio de
//! configuración. Es decir: **el camino tiene estado**, y eso cambia tres
//! cosas respecto del plano.
//!
//! 1. **Dos accesos a la vez se pisan.** Entre escribir el índice y leer por
//!    la ventana, otra CPU puede haber escrito *su* índice, y la lectura sale
//!    del dispositivo equivocado — sin error, con un valor perfectamente
//!    plausible. Linux no pone cerrojo en `brcm_pcie_map_bus` porque el núcleo
//!    de PCI ya sostiene `pci_lock` sobre `map_bus` **y** el acceso
//!    (`drivers/pci/access.c`). Aquí no hay tal cosa, así que el cerrojo lo
//!    pone este módulo, y cubre el par entero. Un acceso de menos de una
//!    palabra es lectura-modificación-escritura: también entero, dentro del
//!    mismo cerrojo.
//! 2. **El puente mismo no pasa por el índice.** Sus propios registros están
//!    en la ventana, en el offset tal cual, y sólo existe `devfn == 0`:
//!    cualquier otra función del bus raíz **no existe**. Es lo que decide
//!    [`BrcmPcie::cfg_access`].
//! 3. **Un dispositivo que no está no se lee: se inventa.** `cfg_access`
//!    devuelve `None` cuando no hay a quién preguntar (el enlace caído aborta
//!    la CPU, no devuelve unos), y la convención de PCI para «ahí no hay
//!    nadie» es **todo unos**: un `vendor id` de `0xffff`. Una escritura, en
//!    cambio, se **tira**. Y eso no es un detalle de estilo: el offset 0 de la
//!    ventana es el `vendor id` del puente, así que una escritura «perdida»
//!    que acabase en la ventana sin índice le estaría escribiendo al puente.
//!
//! # Esto no toca el camino plano
//!
//! [`crate::bus::pci`] sigue entrando por `CSpaceAccessMethod` con su
//! `PortOpsImpl` de siempre: puertos `0xCF8/0xCFC` en x86_64 y la ventana ECAM
//! en riscv64/aarch64. Eso es lo único que ha arrancado de verdad, y no cambia
//! ni una línea. Lo de aquí es **otra implementación del mismo trait**
//! ([`PortOps`]), que se usa sólo con un puente BCM2712 delante.
//!
//! # El `port` de `PortOps` y por qué se puede descomponer
//!
//! `CSpaceAccessMethod::MemoryMapped` le pasa al `PortOps` un offset ECAM ya
//! compuesto, `(bus << 20) | (device << 15) | (function << 12) | (off & 0xffc)`
//! — que es exactamente [`ecam_offset`], porque `devfn == (device << 3) |
//! function`. Los campos no se solapan, así que descomponerlo otra vez es
//! **exacto**, no una aproximación.
//!
//! # Lo que no se ha podido comprobar
//!
//! Nada de esto ha corrido contra el silicio: no hay Pi 5, y QEMU no emula
//! este puente. Los dos offsets están casados contra
//! `brcm_pcie_map_bus`.

use super::*;
use lock::Mutex;
use pci::PortOps;

/// El `reg` del nodo del árbol: `<0x10 0x00120000 0x0 0x9310>`.
pub const WIN_LEN_2712: usize = 0x9310;

/// Lo que lee un acceso a un dispositivo que no está: la convención de PCI
/// para un hueco, que es lo que hace que un escaneo de bus lo salte en vez de
/// quedarse con un `vendor id` de cero (que es un fabricante válido).
pub const CFG_NO_DEVICE: u32 = 0xffff_ffff;

// La ventana de datos y el registro de índice caben en la ventana del nodo, y
// **están pegados**: los 4 KiB de datos acaban en 0x8fff y el índice empieza
// en 0x9000. Por eso el offset se enmascara con `ECAM_REG_MASK` y no por
// prolijidad: un `where` de 0x1000 sin enmascarar no se saldría de la ventana,
// escribiría *el registro de índice*.
const _: () = assert!(EXT_CFG_DATA + ECAM_REG_MASK as usize == EXT_CFG_INDEX - 1);
const _: () = assert!(EXT_CFG_INDEX + 4 <= WIN_LEN_2712);
// Y de paso: el `0x9004` que la tabla de offsets del 2712 declara como
// `EXT_CFG_DATA` también cae dentro de la ventana. Por eso esa entrada
// equivocada nunca ha reventado nada — nadie la lee en este chip.
const _: () = assert!(0x9004 + 4 <= WIN_LEN_2712);

/// Descompone el offset ECAM que llega por `PortOps` en el `bus`, el `devfn` y
/// el registro que lo compusieron.
pub const fn decode_port(port: u32) -> (u8, u8, u32) {
    let bus = (port >> ECAM_BUS_SHIFT) as u8;
    let devfn = ((port >> ECAM_DEVFN_SHIFT) & 0xff) as u8;
    (bus, devfn, port & ECAM_REG_MASK)
}

/// El espacio de configuración de detrás de un puente BCM2712, como un
/// [`PortOps`].
pub struct BrcmCfgOps {
    pcie: BrcmPcie,
    /// El número del bus raíz, que es el del puente mismo.
    root_bus: u8,
    /// El par «escribe el índice, lee por la ventana» es indivisible. Ver la
    /// cabecera del módulo.
    puerta: Mutex<()>,
}

impl BrcmCfgOps {
    /// # Safety
    ///
    /// `base` tiene que ser la ventana de registros de un puente BCM2712 ya
    /// mapeada, y el puente tiene que estar levantado
    /// ([`BrcmPcie::bring_up`]): un acceso a configuración con el enlace
    /// caído aborta la CPU.
    pub const unsafe fn new(base: usize, root_bus: u8) -> Self {
        Self {
            pcie: unsafe { BrcmPcie::new(base) },
            root_bus,
            puerta: Mutex::new(()),
        }
    }

    /// El puente de debajo, para mirarle el enlace sin pasar por aquí.
    pub fn bridge(&self) -> &BrcmPcie {
        &self.pcie
    }

    /// Una palabra, **sin coger el cerrojo**: lo coge quien llama, porque un
    /// acceso de menos de una palabra necesita que la lectura y la escritura
    /// caigan dentro del mismo.
    fn leer_dword(&self, bus: u8, devfn: u8, reg: u32) -> u32 {
        match self.pcie.cfg_access(bus, devfn, reg, self.root_bus) {
            None => CFG_NO_DEVICE,
            Some(CfgAccess::Direct { off }) => self.pcie.read(off & !0b11),
            Some(CfgAccess::Indexed { index, off }) => {
                self.pcie.write(EXT_CFG_INDEX, index);
                self.pcie.read(off & !0b11)
            }
        }
    }

    /// Una palabra, sin coger el cerrojo. Devuelve si había dónde escribir:
    /// un `false` es una escritura **tirada**, no una perdida.
    fn escribir_dword(&self, bus: u8, devfn: u8, reg: u32, val: u32) -> bool {
        match self.pcie.cfg_access(bus, devfn, reg, self.root_bus) {
            None => false,
            Some(CfgAccess::Direct { off }) => {
                self.pcie.write(off & !0b11, val);
                true
            }
            Some(CfgAccess::Indexed { index, off }) => {
                self.pcie.write(EXT_CFG_INDEX, index);
                self.pcie.write(off & !0b11, val);
                true
            }
        }
    }

    /// Mete `val` en el trozo de la palabra que le toca, dejando el resto como
    /// estaba. `mask` es la máscara del trozo ya colocada.
    ///
    /// El índice se escribe **una sola vez** para el par: con el cerrojo
    /// cogido nadie lo ha podido mover entre la lectura y la escritura, y
    /// volver a escribirlo sería decir que sí ha podido.
    fn rmw(&self, port: u32, mask: u32, val: u32) {
        let (bus, devfn, reg) = decode_port(port);
        let _g = self.puerta.lock();
        match self.pcie.cfg_access(bus, devfn, reg, self.root_bus) {
            // Tirada, no perdida: ver la cabecera del módulo.
            None => (),
            Some(CfgAccess::Direct { off }) => {
                let off = off & !0b11;
                let viejo = self.pcie.read(off);
                self.pcie.write(off, (viejo & !mask) | (val & mask));
            }
            Some(CfgAccess::Indexed { index, off }) => {
                let off = off & !0b11;
                self.pcie.write(EXT_CFG_INDEX, index);
                let viejo = self.pcie.read(off);
                self.pcie.write(off, (viejo & !mask) | (val & mask));
            }
        }
    }
}

/// Dónde empieza, dentro de la palabra, el byte o la media palabra que pide
/// este offset.
const fn shift_de(port: u32, ancho: u32) -> u32 {
    let dentro = port & 0b11 & !((ancho / 8) - 1);
    dentro * 8
}

impl PortOps for BrcmCfgOps {
    unsafe fn read8(&self, port: u16) -> u8 {
        let port = u32::from(port);
        let (bus, devfn, reg) = decode_port(port);
        let _g = self.puerta.lock();
        let val = self.leer_dword(bus, devfn, reg);
        (val >> shift_de(port, 8)) as u8
    }

    unsafe fn read16(&self, port: u16) -> u16 {
        let port = u32::from(port);
        let (bus, devfn, reg) = decode_port(port);
        let _g = self.puerta.lock();
        let val = self.leer_dword(bus, devfn, reg);
        (val >> shift_de(port, 16)) as u16
    }

    unsafe fn read32(&self, port: u32) -> u32 {
        let (bus, devfn, reg) = decode_port(port);
        let _g = self.puerta.lock();
        self.leer_dword(bus, devfn, reg)
    }

    unsafe fn write8(&self, port: u16, val: u8) {
        let port = u32::from(port);
        let shift = shift_de(port, 8);
        self.rmw(port, 0xff << shift, u32::from(val) << shift);
    }

    unsafe fn write16(&self, port: u16, val: u16) {
        let port = u32::from(port);
        let shift = shift_de(port, 16);
        self.rmw(port, 0xffff << shift, u32::from(val) << shift);
    }

    unsafe fn write32(&self, port: u32, val: u32) {
        let (bus, devfn, reg) = decode_port(port);
        let _g = self.puerta.lock();
        self.escribir_dword(bus, devfn, reg, val);
    }
}

#[cfg(test)]
#[path = "cfg/cfg_tests.rs"]
mod cfg_tests;
