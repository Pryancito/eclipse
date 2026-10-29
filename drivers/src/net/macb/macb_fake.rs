//! Un Cadence GEM falso en memoria del anfitrion, con su PHY detras del bus
//! MDIO.
//!
//! Igual que el del GENET: la ventana de registros es un `Vec<u32>` del monton
//! cuya direccion hace de base, asi que el driver lee y escribe ahi
//! directamente y no hay que interceptar ninguna lectura. Lo que un modelo en
//! memoria no da es la *reaccion* a una escritura, y aqui hay tres que
//! importan:
//!
//! - **`MAN`**: escribirlo arranca una transaccion MDIO, y hasta que acaba el
//!   `IDLE` de `NSR` esta a cero. Un falso que guardara `MAN` y nada mas
//!   dejaria pasar un driver que no espera.
//! - **`IDR`/`IER`**: no son registros, son ordenes que quitan y ponen bits de
//!   `IMR`.
//! - **`TSR`/`RSR`/`ISR`**: se limpian escribiendo unos, no se guardan.
//!
//! Un falso por hilo (`thread_local!`), para que las vueltas en paralelo no se
//! pisen la ventana.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
extern crate std;

use super::*;
use crate::net::mii::*;

/// Tamaño de la ventana que se le da al falso: la entera que declara el `reg`
/// del nodo del RP1, donde cabe todo lo que el driver toca.
const WINDOW_BYTES: usize = MACB_WINDOW_BYTES;

/// El `IDNUM` que el GEM del RP1 declara. Linux solo exige que sea 2 o mas.
pub(super) const GEM_IDNUM: u32 = 2;

/// Las averias que un test puede pedirle al MAC falso. Todas son cosas que el
/// hardware de verdad hace y que el driver tiene que sobrevivir.
#[derive(Default, Clone)]
pub(super) struct Faults {
    /// La logica de gestion no vuelve nunca a reposo: una transaccion MDIO
    /// que se queda colgada.
    pub(super) mdio_never_idle: bool,
    /// Estas direcciones MDIO no tienen PHY. El GEM no tiene bit de error, asi
    /// que el bus devuelve todo unos, que es lo que hace un bus en reposo.
    pub(super) mdio_mute: Vec<u8>,
    /// La ventana no contesta: una direccion mal mapeada.
    pub(super) dead_window: bool,
}

struct State {
    regs: &'static mut [u32],
    base: usize,
    faults: Faults,
    /// Los registros del PHY, por (direccion MDIO, registro).
    phy: BTreeMap<(u8, u32), u16>,
    /// Lo que el driver ha escrito en el PHY, en orden.
    phy_writes: Vec<(u8, u32, u16)>,
    /// Cuantas transacciones MDIO se han arrancado.
    mdio_starts: u32,
    /// Toda escritura del driver, en orden: (offset, valor). Es la unica
    /// forma de comprobar una ordenacion desde el anfitrion.
    writes: Vec<(usize, u32)>,
}

std::thread_local! {
    static FAKE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// La direccion MDIO en la que el falso pone su PHY por defecto.
pub(super) const DEFAULT_PHY_ADDR: u8 = 1;

/// El GEM falso de este hilo. Mientras vive, [`on_write`] lo atiende; al caer,
/// lo desengancha.
pub(super) struct Hw {
    base: usize,
}

impl Hw {
    /// Un GEM con un PHY de gigabit en la direccion 1, el enlace arriba a
    /// gigabit full duplex y la autonegociacion terminada: el estado desde el
    /// que arranca un driver que no se encuentra ninguna sorpresa.
    pub(super) fn new() -> Self {
        // Los plazos del driver miran el reloj, y bajo test el reloj solo lo
        // mueve quien lo pide: sin esto una espera se come su tope de vueltas
        // en cada test.
        crate::nvme::nvme_queue::test_clock::set_auto_advance(1);

        let regs: &'static mut [u32] = Box::leak(vec![0u32; WINDOW_BYTES / 4].into_boxed_slice());
        let base = regs.as_ptr() as usize;
        let mut st = State {
            regs,
            base,
            faults: Faults::default(),
            phy: BTreeMap::new(),
            phy_writes: Vec::new(),
            mdio_starts: 0,
            writes: Vec::new(),
        };
        st.regs[MID / 4] = GEM_IDNUM << MID_IDNUM_SHIFT | 0x0107;
        // Un bus de 64 bits, que es lo que `DBWDEF` codifica con un 2.
        st.regs[GEM_DCFG1 / 4] = 2 << DCFG1_DBWDEF_SHIFT;
        // La logica de gestion del PHY, en reposo desde el principio.
        st.regs[NSR / 4] = NSR_IDLE;
        FAKE.with(|f| *f.borrow_mut() = Some(st));

        let hw = Self { base };
        hw.set_phy_present(DEFAULT_PHY_ADDR);
        hw.set_link_1000_full(DEFAULT_PHY_ADDR);
        hw
    }

    pub(super) fn base(&self) -> usize {
        self.base
    }

    /// El driver que mira esta ventana.
    pub(super) fn mac(&self) -> MacbMac {
        unsafe { MacbMac::new(self.base) }
    }

    fn with<R>(&self, body: impl FnOnce(&mut State) -> R) -> R {
        FAKE.with(|f| body(f.borrow_mut().as_mut().expect("el GEM falso vive")))
    }

    /// Cambia las averias del MAC falso.
    pub(super) fn faults(&self, body: impl FnOnce(&mut Faults)) {
        self.with(|st| body(&mut st.faults));
    }

    pub(super) fn poke(&self, off: usize, val: u32) {
        self.with(|st| st.regs[off / 4] = val);
    }

    pub(super) fn peek(&self, off: usize) -> u32 {
        self.with(|st| st.regs[off / 4])
    }

    /// Pone un registro del PHY.
    pub(super) fn set_phy(&self, pmd: u8, reg: u32, val: u16) {
        self.with(|st| {
            st.phy.insert((pmd, reg), val);
        });
    }

    pub(super) fn phy(&self, pmd: u8, reg: u32) -> Option<u16> {
        self.with(|st| st.phy.get(&(pmd, reg)).copied())
    }

    /// Un PHY de gigabit en esa direccion, con gigabit anunciado y nada
    /// negociado todavia.
    ///
    /// El identificador es el del BCM54213PE, que es el PHY que la Raspberry
    /// Pi lleva en las dos placas: el de la Pi 5 cuelga del RGMII del RP1 en
    /// vez del del BCM2711, pero es la misma familia.
    pub(super) fn set_phy_present(&self, pmd: u8) {
        self.set_phy(pmd, MII_PHYSID1, 0x600D);
        self.set_phy(pmd, MII_PHYSID2, 0x84A2);
        self.set_phy(pmd, MII_BMCR, BMCR_ANENABLE as u16);
        self.set_phy(pmd, MII_ADVERTISE, (ADVERTISE_ALL | ADVERTISE_CSMA) as u16);
        self.set_phy(pmd, MII_CTRL1000, ADVERTISE_1000FULL as u16);
    }

    /// El enlace arriba a gigabit full duplex.
    pub(super) fn set_link_1000_full(&self, pmd: u8) {
        self.set_phy(
            pmd,
            MII_BMSR,
            (BMSR_LSTATUS | BMSR_ANEGCOMPLETE | BMSR_ANEGCAPABLE | BMSR_ESTATEN) as u16,
        );
        self.set_phy(pmd, MII_CTRL1000, ADVERTISE_1000FULL as u16);
        self.set_phy(pmd, MII_STAT1000, LPA_1000FULL as u16);
        self.set_phy(pmd, MII_LPA, 0);
    }

    /// El enlace arriba a 100 megabits, el duplex a eleccion.
    pub(super) fn set_link_100(&self, pmd: u8, full_duplex: bool) {
        self.set_phy(
            pmd,
            MII_BMSR,
            (BMSR_LSTATUS | BMSR_ANEGCOMPLETE | BMSR_ANEGCAPABLE) as u16,
        );
        self.set_phy(pmd, MII_CTRL1000, 0);
        self.set_phy(pmd, MII_STAT1000, 0);
        self.set_phy(
            pmd,
            MII_LPA,
            if full_duplex {
                LPA_100FULL as u16
            } else {
                LPA_100HALF as u16
            },
        );
    }

    /// El enlace caido: sin portadora.
    pub(super) fn set_link_down(&self, pmd: u8) {
        self.set_phy(pmd, MII_BMSR, BMSR_ANEGCAPABLE as u16);
    }

    /// Lo que el driver escribio en el PHY, en orden.
    pub(super) fn phy_writes(&self) -> Vec<(u8, u32, u16)> {
        self.with(|st| st.phy_writes.clone())
    }

    pub(super) fn mdio_starts(&self) -> u32 {
        self.with(|st| st.mdio_starts)
    }

    /// Toda escritura del driver, en orden.
    pub(super) fn writes(&self) -> Vec<(usize, u32)> {
        self.with(|st| st.writes.clone())
    }

    /// Los offsets escritos desde una marca, en orden, para medir una
    /// secuencia sin arrastrar la del arranque.
    pub(super) fn writes_since(&self, marca: usize) -> Vec<usize> {
        self.with(|st| st.writes[marca..].iter().map(|(off, _)| *off).collect())
    }

    /// Cuantas escrituras se han visto, para usarlo como marca.
    pub(super) fn write_mark(&self) -> usize {
        self.with(|st| st.writes.len())
    }

    /// Levanta interrupciones como las levantaria el hardware: pone los bits
    /// en el `ISR`, sin tocar la mascara.
    pub(super) fn raise_irq(&self, bits: u32) {
        self.poke(ISR, self.peek(ISR) | bits);
    }
}

impl Drop for Hw {
    fn drop(&mut self) {
        crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
        FAKE.with(|f| *f.borrow_mut() = None);
    }
}

/// La reaccion del MAC falso a una escritura del driver.
///
/// Se llama desde `MacbMac::write` y solo bajo `cfg(test)`. Una escritura en
/// una ventana que no es la del falso de este hilo no es de este falso: se
/// ignora, porque dos tests en paralelo tienen cada uno la suya.
pub(super) fn on_write(base: usize, off: usize, val: u32) {
    FAKE.with(|f| {
        let mut slot = f.borrow_mut();
        let Some(st) = slot.as_mut() else { return };
        if st.base != base {
            return;
        }
        st.writes.push((off, val));
        match off {
            NCR => st.ncr_cmd(val),
            MAN => st.man_cmd(val),
            IER => {
                st.regs[IMR / 4] |= val;
                st.regs[IER / 4] = 0;
            }
            IDR => {
                st.regs[IMR / 4] &= !val;
                st.regs[IDR / 4] = 0;
            }
            TSR | RSR | ISR => {
                // Se limpia lo que se escribe, no se guarda lo escrito.
                st.regs[off / 4] &= !val;
            }
            _ => {}
        }
    });
}

impl State {
    /// Atiende una escritura de `NCR`.
    ///
    /// Dos cosas que no son memoria: `CLRSTAT` es una **orden** y el silicio
    /// no la deja puesta, y una ventana muerta no devuelve lo que se le
    /// escribe, que es de lo que vive `responds`.
    fn ncr_cmd(&mut self, val: u32) {
        if self.faults.dead_window {
            self.regs[NCR / 4] = 0;
            return;
        }
        self.regs[NCR / 4] = val & !NCR_CLRSTAT;
    }

    /// Atiende una escritura de `MAN`: la transaccion MDIO entera.
    ///
    /// El `IDLE` de `NSR` se baja al arrancar y se sube al acabar, que es el
    /// unico sitio donde el driver puede notar si deja de esperar.
    fn man_cmd(&mut self, val: u32) {
        self.mdio_starts += 1;
        self.regs[NSR / 4] &= !NSR_IDLE;

        if self.faults.mdio_never_idle {
            return;
        }

        let pmd = ((val >> MAN_PHYA_SHIFT) & MAN_PHYA_MASK) as u8;
        let reg = (val >> MAN_REGA_SHIFT) & MAN_REGA_MASK;
        let op = (val >> MAN_RW_SHIFT) & MAN_RW_MASK;
        let muted = self.faults.mdio_mute.contains(&pmd);

        if op == MAN_C22_READ {
            let data = if muted {
                // Un bus sin nadie en esa direccion deja las lineas arriba.
                0xFFFF
            } else {
                self.phy.get(&(pmd, reg)).copied().unwrap_or(0)
            };
            self.regs[MAN / 4] = (val & !MAN_DATA_MASK) | data as u32;
        } else if op == MAN_C22_WRITE && !muted {
            let data = val as u16;
            self.phy.insert((pmd, reg), data);
            self.phy_writes.push((pmd, reg, data));
        }

        self.regs[NSR / 4] |= NSR_IDLE;
    }
}
