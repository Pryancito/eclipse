//! MSI-X: la capacidad, la tabla y la secuencia con la que se arma un vector.
//!
//! Eclipse sabe de MSI, que es un vector por función y el mensaje metido en el
//! propio espacio de configuración ([`super::enable_with`]). MSI-X es otra
//! cosa, y la diferencia que importa no es «más vectores»: **el mensaje de
//! cada vector vive en una tabla en memoria de dispositivo**, dentro de una de
//! las BAR, no en el espacio de configuración. De ahí casi todo lo que sigue.
//!
//! # Lo que un dispositivo dice de su tabla
//!
//! Tres palabras en la capacidad, y cada una tiene su trampa:
//!
//! - **Cuántos vectores**: `QSIZE`, que son once bits y vale **uno menos** que
//!   los vectores que hay. Un dispositivo con un solo vector declara cero.
//!   Ver [`MsixCap::table_size`].
//! - **En qué BAR**: `BIR`, tres bits que comparten palabra con el offset.
//!   Sólo 0..=5 son BAR; 6 y 7 están reservados y hay que rechazarlos.
//! - **En qué offset de esa BAR**: los 29 bits de arriba de esa misma palabra,
//!   así que **el offset hay que enmascararlo** con [`MSIX_OFFSET`]. Sin
//!   enmascarar, el BIR se cuela como los tres bits bajos del offset y la
//!   tabla queda hasta siete bytes corrida: a mitad de una entrada.
//!
//! # Lo que hay que escribir en la tabla
//!
//! Una entrada son **16 bytes**: dirección baja, dirección alta, dato y
//! control. Y de ahí dos cosas que no se ven mirando sólo MSI:
//!
//! - **La dirección son siempre dos palabras.** En MSI hay un bit de
//!   capacidad que dice si el dispositivo es de 64 bits y, si lo es, hay que
//!   escribir la mitad alta aunque sea cero — un fallo que este kernel ya
//!   tuvo. En MSI-X no hay bit que consultar: las dos palabras existen
//!   siempre, así que la mitad alta **siempre** se escribe.
//! - **Las entradas arrancan enmascaradas.** El bit 0 del control vale uno
//!   tras un reset, así que programar la entrada y no desenmascararla deja un
//!   dispositivo que se encuentra, se reclama y **no interrumpe nunca**. Es
//!   el mismo par de puertas que el bloque del RP1
//!   ([`crate::irq::rp1`]): una del lado del dispositivo y otra del lado de
//!   quien reparte.
//!
//! # La secuencia
//!
//! [`MSIX_FLAGS_MASKALL`] tapa **todos** los vectores, pase lo que pase con el
//! control de cada uno. Así que se enciende la capacidad con `MASKALL`
//! puesto, se programa la tabla, y se quita `MASKALL` al final: entre medias
//! el dispositivo no puede soltar un mensaje a medio programar. Ver
//! [`arm`].
//!
//! Y **MSI y MSI-X no pueden estar encendidos los dos a la vez** (PCI 3.0
//! §6.8.2: el software no debe hacerlo). Ver [`disable_msi`].
//!
//! # Por qué el control se toca de palabra entera y no de media
//!
//! El registro de control está en `cap + 2`, que **no está alineado a
//! palabra**, y `CSpaceAccessMethod::write16` del crate `pci` calcula mal el
//! desplazamiento: escribe `offset as usize & 0b10 << 3`, y `<<` liga más que
//! `&`, así que lo que hace es `offset & 0x10` en vez de `(offset & 0b10) <<
//! 3`. Para un `cap_ptr` de `0x70` sale bien por casualidad (`0x72 & 0x10 ==
//! 16`) y para uno de `0x60` sale mal (`0x62 & 0x10 == 0`): la mitad alta se
//! escribe en la baja, y lo que se pisa son el id de la capacidad y el
//! puntero al siguiente. `write8` tiene el mismo paréntesis que falta.
//! (Los caminos propios que escribían menos de una palabra ya no pasan por
//! ahí: ver [`super::cfgwrite`].)
//!
//! Así que aquí no se usa ninguno de los dos. El puntero de una capacidad
//! está alineado a palabra por especificación, de modo que el control es
//! **la mitad alta de la palabra que empieza en `cap_ptr`**, y se toca con
//! `read32`/`write32` — que es, además, lo que ya hace el camino de MSI de
//! [`super::enable_with`]. Ver [`flags_de_palabra`].
//!
//! # Quién usa esto
//!
//! Esta capa son los registros y el orden. Lo que hace falta para llegar a un
//! dispositivo de verdad -- encontrar la capacidad, encontrar la BAR donde
//! vive la tabla y armar el primer vector -- está en [`bringup`], detrás de
//! la escotilla `pci.msix`.

use pci::{CSpaceAccessMethod, Location, PortOps};

/// Lo que hace falta para llegar a un dispositivo de verdad: encontrar la
/// capacidad, encontrar la BAR de la tabla y armar el primer vector, detrás de
/// la escotilla `pci.msix`.
pub mod bringup;

/// `PCI_CAP_ID_MSIX`.
pub const CAP_ID_MSIX: u8 = 0x11;

/// `PCI_MSIX_FLAGS`: el registro de control, 16 bits, en `cap + 2`.
pub const MSIX_FLAGS: u16 = 2;
/// `PCI_MSIX_FLAGS_QSIZE`: once bits, y vale **uno menos** que los vectores.
pub const MSIX_FLAGS_QSIZE: u16 = 0x07ff;
/// `PCI_MSIX_FLAGS_MASKALL`: tapa todos los vectores, pase lo que pase con el
/// control de cada entrada.
pub const MSIX_FLAGS_MASKALL: u16 = 0x4000;
/// `PCI_MSIX_FLAGS_ENABLE`.
pub const MSIX_FLAGS_ENABLE: u16 = 0x8000;

/// `PCI_MSIX_TABLE`: BIR y offset de la tabla, en una sola palabra.
pub const MSIX_TABLE: u16 = 4;
/// `PCI_MSIX_PBA`: lo mismo para el array de pendientes.
pub const MSIX_PBA: u16 = 8;
/// `PCI_MSIX_TABLE_BIR` (y `PCI_MSIX_PBA_BIR`, que es el mismo).
pub const MSIX_BIR: u32 = 0x0000_0007;
/// `PCI_MSIX_TABLE_OFFSET`: los 29 bits de arriba. **Hay que aplicarlo**: los
/// tres de abajo son el BIR, no parte del offset.
pub const MSIX_OFFSET: u32 = 0xffff_fff8;
/// `PCI_CAP_MSIX_SIZEOF`.
pub const CAP_MSIX_SIZEOF: u16 = 12;

/// `PCI_MSIX_ENTRY_SIZE`.
pub const ENTRY_SIZE: usize = 16;
/// `PCI_MSIX_ENTRY_LOWER_ADDR`.
pub const ENTRY_LOWER_ADDR: usize = 0x0;
/// `PCI_MSIX_ENTRY_UPPER_ADDR`.
pub const ENTRY_UPPER_ADDR: usize = 0x4;
/// `PCI_MSIX_ENTRY_DATA`.
pub const ENTRY_DATA: usize = 0x8;
/// `PCI_MSIX_ENTRY_VECTOR_CTRL`.
pub const ENTRY_VECTOR_CTRL: usize = 0xc;
/// `PCI_MSIX_ENTRY_CTRL_MASKBIT`: **uno tras un reset**, o sea enmascarada.
pub const ENTRY_CTRL_MASKBIT: u32 = 0x0000_0001;

/// Cuántos vectores caben en los once bits de `QSIZE`, contando el «uno
/// menos».
pub const MAX_VECTORS: usize = MSIX_FLAGS_QSIZE as usize + 1;

/// El BIR mayor que es una BAR de verdad. 6 y 7 están reservados.
pub const MAX_BIR: u32 = 5;

// El control es la mitad alta de la palabra que empieza en `cap_ptr`, y eso
// sólo se tiene en pie si `MSIX_FLAGS` es 2 y el puntero de la capacidad está
// alineado a palabra (lo está: la especificacion reserva sus dos bits bajos).
const _: () = assert!(MSIX_FLAGS == 2);

/// El control, sacado de la palabra que empieza en `cap_ptr`.
pub const fn flags_de_palabra(dw: u32) -> u16 {
    (dw >> 16) as u16
}

/// Y la palabra de vuelta, con el control cambiado y **el id y el puntero al
/// siguiente intactos**: eso es la mitad baja, y pisarla rompe el recorrido de
/// capacidades del siguiente que pase.
pub const fn palabra_con_flags(dw: u32, flags: u16) -> u32 {
    (dw & 0x0000_ffff) | ((flags as u32) << 16)
}

/// Lo que un dispositivo dice de su MSI-X.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixCap {
    /// Dónde empieza la capacidad en el espacio de configuración.
    pub cap_ptr: u16,
    /// **Cuántos vectores hay**, ya con el «uno más» aplicado: nunca cero.
    pub table_size: usize,
    /// En qué BAR está la tabla.
    pub table_bir: u32,
    /// En qué offset de esa BAR, ya enmascarado.
    pub table_offset: u32,
    /// En qué BAR está el array de pendientes, que puede ser la misma.
    pub pba_bir: u32,
    /// Y en qué offset.
    pub pba_offset: u32,
}

impl MsixCap {
    /// Lee la capacidad que empieza en `cap_ptr`.
    ///
    /// Devuelve `None` si el BIR de la tabla no es una BAR (6 y 7 están
    /// reservados): sin saber en qué BAR está, la tabla no se puede alcanzar,
    /// y seguir sería mapear la BAR 6, que no existe.
    ///
    /// # Safety
    ///
    /// `ops` tiene que direccionar el espacio de configuración de `loc`, y
    /// `cap_ptr` tiene que ser una capacidad de id [`CAP_ID_MSIX`].
    pub unsafe fn read<T: PortOps>(
        ops: &T,
        am: CSpaceAccessMethod,
        loc: Location,
        cap_ptr: u16,
    ) -> Option<Self> {
        let flags = flags_de_palabra(unsafe { am.read32(ops, loc, cap_ptr) });
        let tabla = unsafe { am.read32(ops, loc, cap_ptr + MSIX_TABLE) };
        let pba = unsafe { am.read32(ops, loc, cap_ptr + MSIX_PBA) };

        let table_bir = tabla & MSIX_BIR;
        if table_bir > MAX_BIR {
            return None;
        }
        Some(Self {
            cap_ptr,
            // El «uno más». `QSIZE` son once bits, así que esto no desborda.
            table_size: (flags & MSIX_FLAGS_QSIZE) as usize + 1,
            table_bir,
            table_offset: tabla & MSIX_OFFSET,
            pba_bir: pba & MSIX_BIR,
            pba_offset: pba & MSIX_OFFSET,
        })
    }

    /// Si la tabla cabe dentro de una BAR de `bar_len` bytes.
    ///
    /// Una tabla que no cabe no es un dispositivo raro: es un `QSIZE` o un
    /// offset mal leídos, y programarla igual sería escribir fuera de la BAR.
    pub fn table_fits(&self, bar_len: u64) -> bool {
        let fin = self.table_offset as u64 + (self.table_size * ENTRY_SIZE) as u64;
        fin <= bar_len
    }

    /// El offset de la entrada `vector` dentro de la tabla.
    pub fn entry_offset(&self, vector: usize) -> Option<usize> {
        if vector >= self.table_size {
            return None;
        }
        Some(vector * ENTRY_SIZE)
    }
}

/// El offset de una entrada dentro de la tabla, sin mirar cuántas hay.
pub const fn entry_offset(vector: usize) -> usize {
    vector * ENTRY_SIZE
}

/// La tabla de vectores, ya mapeada.
pub struct MsixTable {
    /// La dirección virtual de la entrada 0.
    base: usize,
    /// Cuántas entradas hay.
    entries: usize,
}

impl MsixTable {
    /// # Safety
    ///
    /// `base` tiene que ser la tabla ya mapeada (la BAR que dice
    /// [`MsixCap::table_bir`], más [`MsixCap::table_offset`]) y tiene que
    /// haber sitio para `entries` entradas de [`ENTRY_SIZE`] bytes.
    pub const unsafe fn new(base: usize, entries: usize) -> Self {
        Self { base, entries }
    }

    /// Cuántas entradas tiene.
    pub fn entries(&self) -> usize {
        self.entries
    }

    fn palabra(&self, vector: usize, campo: usize) -> Option<*mut u32> {
        if vector >= self.entries {
            return None;
        }
        Some((self.base + entry_offset(vector) + campo) as *mut u32)
    }

    fn lee(&self, vector: usize, campo: usize) -> Option<u32> {
        let p = self.palabra(vector, campo)?;
        let val = unsafe { core::ptr::read_volatile(p) };
        #[cfg(test)]
        msix_fake::apunta_lectura(entry_offset(vector) + campo);
        Some(val)
    }

    fn escribe(&self, vector: usize, campo: usize, val: u32) -> bool {
        match self.palabra(vector, campo) {
            None => false,
            Some(p) => {
                #[cfg(test)]
                msix_fake::apunta(entry_offset(vector) + campo, val);
                unsafe { core::ptr::write_volatile(p, val) };
                true
            }
        }
    }

    /// Mete el mensaje en una entrada, **sin desenmascararla**.
    ///
    /// La mitad alta de la dirección se escribe siempre, aunque sea cero: en
    /// MSI-X no hay bit de capacidad que diga si el dispositivo es de 64 bits,
    /// las dos palabras existen y una sin escribir deja una dirección a medias.
    pub fn program(&self, vector: usize, addr: u64, data: u32) -> bool {
        self.escribe(vector, ENTRY_LOWER_ADDR, addr as u32)
            && self.escribe(vector, ENTRY_UPPER_ADDR, (addr >> 32) as u32)
            && self.escribe(vector, ENTRY_DATA, data)
    }

    /// Tapa una entrada.
    pub fn mask(&self, vector: usize) -> bool {
        match self.lee(vector, ENTRY_VECTOR_CTRL) {
            None => false,
            Some(ctrl) => self.escribe(vector, ENTRY_VECTOR_CTRL, ctrl | ENTRY_CTRL_MASKBIT),
        }
    }

    /// La destapa. **Sin esto no interrumpe**: el bit vale uno tras un reset.
    pub fn unmask(&self, vector: usize) -> bool {
        match self.lee(vector, ENTRY_VECTOR_CTRL) {
            None => false,
            Some(ctrl) => self.escribe(vector, ENTRY_VECTOR_CTRL, ctrl & !ENTRY_CTRL_MASKBIT),
        }
    }

    /// Las cuatro palabras de una entrada, tal como el dispositivo las
    /// devuelve.
    ///
    /// Existe para poder **releer lo que se acaba de escribir**: es la única
    /// comprobación, sin esperar una interrupción, de que la tabla estaba
    /// donde la capacidad decía. Con el `BIR` mal leído o el offset sin
    /// enmascarar, lo escrito cae en otro sitio de la BAR y lo que vuelve de
    /// aquí no es lo que se puso.
    pub fn read_back(&self, vector: usize) -> Option<[u32; 4]> {
        Some([
            self.lee(vector, ENTRY_LOWER_ADDR)?,
            self.lee(vector, ENTRY_UPPER_ADDR)?,
            self.lee(vector, ENTRY_DATA)?,
            self.lee(vector, ENTRY_VECTOR_CTRL)?,
        ])
    }

    /// Si está tapada. `None` si ese vector no existe.
    pub fn is_masked(&self, vector: usize) -> Option<bool> {
        Some(self.lee(vector, ENTRY_VECTOR_CTRL)? & ENTRY_CTRL_MASKBIT != 0)
    }
}

/// Apaga MSI en la capacidad `msi_cap`, porque **MSI y MSI-X no pueden estar
/// encendidos los dos** (PCI 3.0 §6.8.2).
///
/// # Safety
///
/// `msi_cap` tiene que ser una capacidad MSI de `loc`.
pub unsafe fn disable_msi<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    msi_cap: u16,
) {
    let dw = unsafe { am.read32(ops, loc, msi_cap) };
    let ctrl = flags_de_palabra(dw);
    unsafe { am.write32(ops, loc, msi_cap, palabra_con_flags(dw, ctrl & !1)) };
}

/// Enciende MSI-X y arma un vector, en el orden que impone el silicio.
///
/// 1. `ENABLE | MASKALL`: la capacidad encendida pero todo tapado, para que
///    el dispositivo no suelte un mensaje a medio programar.
/// 2. El mensaje en la entrada.
/// 3. La entrada destapada (arrancaba tapada).
/// 4. `MASKALL` fuera.
///
/// Devuelve `false` si ese vector no existe, y entonces **no se queda
/// encendido**: deja la capacidad como estaba.
///
/// # Safety
///
/// `tabla` tiene que ser la tabla de `cap`, mapeada, y `ops` el espacio de
/// configuración de `loc`.
pub unsafe fn arm<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    cap: &MsixCap,
    tabla: &MsixTable,
    vector: usize,
    addr: u64,
    data: u32,
) -> bool {
    if vector >= cap.table_size || vector >= tabla.entries() {
        return false;
    }
    let dw = unsafe { am.read32(ops, loc, cap.cap_ptr) };
    let ctrl = flags_de_palabra(dw);
    let escribe_ctrl =
        |flags: u16| unsafe { am.write32(ops, loc, cap.cap_ptr, palabra_con_flags(dw, flags)) };
    escribe_ctrl(ctrl | MSIX_FLAGS_ENABLE | MSIX_FLAGS_MASKALL);
    tabla.program(vector, addr, data);
    tabla.unmask(vector);
    escribe_ctrl((ctrl | MSIX_FLAGS_ENABLE) & !MSIX_FLAGS_MASKALL);
    true
}

#[cfg(test)]
#[path = "msix/msix_tests.rs"]
mod msix_tests;

/// La traza de la tabla, que es lo único que un trozo de memoria no recuerda:
/// con una entrada que arranca tapada y un `MASKALL` por encima, lo que hay
/// que comprobar es **el orden**.
#[cfg(test)]
pub(super) mod msix_fake {
    use core::cell::RefCell;

    /// `(offset en la tabla, Some(valor))` una escritura; `None` una lectura.
    pub type Entrada = (usize, Option<u32>);

    std::thread_local! {
        pub static TRAZA: RefCell<alloc::vec::Vec<Entrada>> =
            const { RefCell::new(alloc::vec::Vec::new()) };
    }

    pub fn apunta(off: usize, val: u32) {
        TRAZA.with(|t| t.borrow_mut().push((off, Some(val))));
    }

    pub fn apunta_lectura(off: usize) {
        TRAZA.with(|t| t.borrow_mut().push((off, None)));
    }

    pub fn trazando<T>(f: impl FnOnce() -> T) -> (T, alloc::vec::Vec<Entrada>) {
        TRAZA.with(|t| t.borrow_mut().clear());
        let r = f();
        (r, TRAZA.with(|t| core::mem::take(&mut *t.borrow_mut())))
    }
}
