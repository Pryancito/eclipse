//! Escrituras de menos de una palabra al espacio de configuración, que es lo
//! que el crate `pci` hace mal.
//!
//! # El bug
//!
//! `CSpaceAccessMethod::write16` y `write8` del crate `pci`
//! (`elliott10/pci-rs`, `rev 8f33774b`) calculan el desplazamiento dentro de
//! la palabra así:
//!
//! ```text
//! let dest = offset as usize & 0b10 << 3;   // write16
//! let dest = offset as usize & 0b11 << 3;   // write8
//! ```
//!
//! `<<` liga más que `&`, así que lo que hacen no es `(offset & 0b10) << 3`
//! sino **`offset & 0x10`** (y `offset & 0x18`). O sea que el desplazamiento
//! lo deciden los bits **4 y 3** del offset en vez de los **1 y 0**, y sale
//! bien sólo cuando esos bits coinciden. Las **lecturas** del crate sí tienen
//! los paréntesis; son las dos escrituras las que están mal.
//!
//! La palabra a la que van es la correcta — `read32` y `write32` enmascaran el
//! offset — así que lo que se estropea no es una dirección: es **el campo
//! vecino dentro de la misma palabra**. El valor se escribe en el trozo que no
//! toca y el que se quería cambiar se queda como estaba, sin error en ninguna
//! parte.
//!
//! Y eso, en una capacidad, significa pisar **el id de la capacidad y el
//! puntero al siguiente**, que viven en la mitad baja de la palabra que
//! empieza en `cap_ptr`: el recorrido de capacidades se rompe para el
//! siguiente que pase, no para quien escribió.
//!
//! # Qué coincide por casualidad y qué no
//!
//! | offset | lo correcto | el crate | |
//! |---|---|---|---|
//! | `0x04` (comando) | 0 | 0 | coincide |
//! | `0x3e` (control del puente) | 16 | 16 | coincide |
//! | `0x62` (`cap_ptr = 0x60` + 2) | 16 | **0** | **no** |
//! | `0x4c` (byte) | 0 | **8** | **no** |
//! | `0x4e` (byte) | 16 | **8** | **no** |
//!
//! Por eso no ha saltado nunca: los dos offsets que este kernel usa a todas
//! horas, `0x04` y `0x3e`, son justo de los que coinciden.
//!
//! # El arreglo de aquí
//!
//! No se toca el crate, que es una dependencia de git con `rev` fijo. Lo que
//! se hace es lo que ya hacía el camino de MSI de [`super::enable_with`] y lo
//! que hace [`super::msix`]: **leer la palabra entera, cambiar el trozo y
//! escribir la palabra entera**, con el desplazamiento bien. Ver
//! [`shift_de_16`] y [`shift_de_8`].
//!
//! Una escritura de 16 bits a un offset **impar** no es legal en PCI; aquí se
//! redondea a la media palabra alineada, que es lo mismo que hace el camino de
//! lectura del crate, en vez de inventar un comportamiento nuevo.

use pci::{CSpaceAccessMethod, Location, PortOps};

/// Dónde empieza, dentro de la palabra, la media palabra de este offset.
///
/// **Los bits 1 y 0**, no los 4 y 3: ahí está el bug del crate.
pub const fn shift_de_16(offset: u16) -> u32 {
    ((offset as u32) & 0b10) * 8
}

/// Lo mismo para un byte.
pub const fn shift_de_8(offset: u16) -> u32 {
    ((offset as u32) & 0b11) * 8
}

/// La palabra que contiene este offset.
pub const fn palabra_de(offset: u16) -> u16 {
    offset & !0b11
}

/// Mete `val` en el trozo `mask` (ya colocada) de `dw`.
pub const fn con_campo(dw: u32, mask: u32, val: u32) -> u32 {
    (dw & !mask) | (val & mask)
}

/// Escribe media palabra en el espacio de configuración, por la palabra que la
/// contiene.
///
/// # Safety
///
/// `ops` tiene que direccionar el espacio de configuración de `loc`.
pub unsafe fn cfg_write16<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    offset: u16,
    val: u16,
) {
    let palabra = palabra_de(offset);
    let shift = shift_de_16(offset);
    let mask = 0x0000_ffffu32 << shift;
    let dw = unsafe { am.read32(ops, loc, palabra) };
    unsafe {
        am.write32(
            ops,
            loc,
            palabra,
            con_campo(dw, mask, u32::from(val) << shift),
        )
    };
}

/// Y un byte.
///
/// # Safety
///
/// Igual que [`cfg_write16`].
pub unsafe fn cfg_write8<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    offset: u16,
    val: u8,
) {
    let palabra = palabra_de(offset);
    let shift = shift_de_8(offset);
    let mask = 0x0000_00ffu32 << shift;
    let dw = unsafe { am.read32(ops, loc, palabra) };
    unsafe {
        am.write32(
            ops,
            loc,
            palabra,
            con_campo(dw, mask, u32::from(val) << shift),
        )
    };
}

#[cfg(test)]
#[path = "cfgwrite/cfgwrite_tests.rs"]
mod cfgwrite_tests;
