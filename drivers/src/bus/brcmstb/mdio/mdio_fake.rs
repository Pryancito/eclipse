//! Un PHY de mentira al otro lado del bus MDIO del puente.
//!
//! Un banco de registros se queda con lo que se le escribe, y eso no basta
//! aquí: lo que hay que comprobar es el **acuse**, que es cosa del silicio y
//! con una polaridad distinta en cada sentido. Así que este módulo imita al
//! PHY: se queda con los valores por bloque y registro, pone el bit del acuse
//! en una lectura y lo baja en una escritura, y deja que un test le diga que no
//! acuse.

use super::*;
use alloc::collections::BTreeMap;
use core::cell::RefCell;

/// Lo que el PHY recuerda, por hilo, para que el banco de pruebas pueda correr
/// en paralelo.
#[derive(Default)]
pub struct Phy {
    /// El bloque elegido por el último `SET_ADDR_OFFSET`.
    pub bloque: u16,
    /// `(bloque, registro) -> valor`.
    pub regs: BTreeMap<(u16, u8), u16>,
    /// Qué paquete está esperando en el registro de dirección.
    pendiente: Option<(u8, u32)>,
    /// Con esto puesto el PHY **no acusa**, en ninguno de los dos sentidos.
    pub mudo: bool,
    /// La lista de accesos, en orden, para poder mirar el protocolo y no sólo
    /// el resultado. Cada entrada es `(offset, valor)` y una escritura va con
    /// el offset tal cual; una lectura, con el offset más `LEIDO`.
    pub traza: alloc::vec::Vec<(usize, u32)>,
}

/// La marca que distingue una lectura de una escritura en la traza.
pub const LEIDO: usize = 0x1_0000;

std::thread_local! {
    pub static PHY: RefCell<Phy> = RefCell::new(Phy::default());
}

/// Empieza un PHY limpio y devuelve lo que quede al acabar.
pub fn con_un_phy<T>(f: impl FnOnce() -> T) -> (T, Phy) {
    PHY.with(|p| *p.borrow_mut() = Phy::default());
    let r = f();
    let phy = PHY.with(|p| core::mem::take(&mut *p.borrow_mut()));
    (r, phy)
}

/// Deja el PHY mudo mientras corre `f`.
pub fn mudo<T>(f: impl FnOnce() -> T) -> (T, Phy) {
    PHY.with(|p| {
        let mut p = p.borrow_mut();
        *p = Phy::default();
        p.mudo = true;
    });
    let r = f();
    let phy = PHY.with(|p| core::mem::take(&mut *p.borrow_mut()));
    (r, phy)
}

/// Lo que de verdad llega al registro. Devuelve el valor a almacenar.
pub(super) fn al_escribir(_base: usize, off: usize, val: u32) -> u32 {
    PHY.with(|p| {
        let mut p = p.borrow_mut();
        p.traza.push((off, val));
        match off {
            RC_DL_MDIO_ADDR => {
                let regad = ((val & REGAD_MASK) >> REGAD_MASK.trailing_zeros()) as u8;
                let cmd = (val & CMD_MASK) >> CMD_MASK.trailing_zeros();
                p.pendiente = Some((regad, cmd));
                val
            }
            RC_DL_MDIO_WR_DATA => {
                if let Some((regad, CMD_WRITE)) = p.pendiente {
                    let dato = (val & 0xffff) as u16;
                    if regad == SET_ADDR_OFFSET {
                        p.bloque = dato;
                    } else {
                        let b = p.bloque;
                        p.regs.insert((b, regad), dato);
                    }
                }
                // El silicio baja el acuse al terminar; mudo, se queda puesto.
                if p.mudo {
                    val
                } else {
                    val & !DATA_DONE
                }
            }
            _ => val,
        }
    })
}

/// Lo que el registro contesta.
pub(super) fn al_leer(_base: usize, off: usize, val: u32) -> u32 {
    PHY.with(|p| {
        let mut p = p.borrow_mut();
        let salida = match off {
            RC_DL_MDIO_RD_DATA => match p.pendiente {
                Some((regad, CMD_READ)) if !p.mudo => {
                    let b = p.bloque;
                    let dato = if regad == SET_ADDR_OFFSET {
                        b
                    } else {
                        p.regs.get(&(b, regad)).copied().unwrap_or(0)
                    };
                    DATA_DONE | u32::from(dato)
                }
                // Sin lectura pedida, o mudo: el acuse no llega.
                _ => 0,
            },
            _ => val,
        };
        p.traza.push((off + LEIDO, salida));
        salida
    })
}
