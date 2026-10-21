//! Un MAC e1000e falso en memoria del anfitrion, para los caminos que hablan
//! con el PHY, el semaforo SW/FW, el Kumeran, la NVM y los anillos.
//!
//! La ventana de registros es un `Vec<u32>` del monton cuya direccion hace de
//! BAR0: bajo test `phys_to_virt` y `virt_to_phys` son la identidad, asi que el
//! driver lee y escribe ahi directamente y no hace falta interceptar ninguna
//! lectura. Lo unico que un modelo en memoria no puede dar es la *reaccion* a
//! una escritura -- un `MDIC` sin su bit READY es un acceso al PHY que nunca
//! acaba, un `CTRL.RST` que nadie baja es un reset que nunca termina -- y eso
//! es lo que hace [`on_write`], la unica costura `cfg(test)` del driver.
//!
//! Un falso por hilo (`thread_local!`), para que las vueltas en paralelo no se
//! pisen la ventana.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
extern crate std;

use super::*;

/// Las averias que un test puede pedirle al MAC falso. Todas son cosas que el
/// hardware de verdad hace y que el driver tiene que sobrevivir.
#[derive(Default, Clone)]
pub(super) struct Faults {
    /// El semaforo SW/FW se queda cogido por la firmware: `EXTCNF_CTRL.SWFLAG`
    /// vuelve a uno en cuanto el driver lo baja.
    pub(super) swflag_busy: bool,
    /// El bit del semaforo no latcha: el driver lo escribe y el registro
    /// vuelve a leerse sin el, porque la ME se lo ha quitado en medio.
    pub(super) swflag_no_latch: bool,
    /// El PHY de estas direcciones MDIO no contesta (ningun READY).
    pub(super) mdic_mute: Vec<u8>,
    /// El PHY contesta, pero con el bit de error.
    pub(super) mdic_error: bool,
    /// `STATUS.GIO_MASTER_ENABLE` nunca se baja: queda DMA en vuelo.
    pub(super) gio_master_stuck: bool,
    /// `CTRL_EXT.LPCD` nunca se enciende tras el baile de LANPHYPC.
    pub(super) lpcd_never: bool,
    /// La ME no termina de deshacer el ULP: `FWSM.ULP_CFG_DONE` no se baja.
    pub(super) ulp_cfg_stuck: bool,
    /// `RXDCTL`/`TXDCTL.QUEUE_ENABLE` no latchean.
    pub(super) queue_enable_stuck: bool,
    /// La EEPROM no contesta a `EERD` con ningun bit de DONE.
    pub(super) eerd_dead: bool,
}

struct State {
    regs: &'static mut [u32],
    base: usize,
    faults: Faults,
    /// Registros del PHY por (direccion MDIO, seleccion de pagina, registro).
    /// La clave de pagina es el valor que fue por el cable, no el numero de
    /// pagina: la pagina 768 se selecciona escribiendo un cero, asi que
    /// guardarla por su numero seria guardarla donde el driver no la busca.
    phy: BTreeMap<(u8, u32, u32), u16>,
    /// Lo ultimo que se escribio en `PHY_PAGE_SELECT`.
    page: u32,
    /// Registros Kumeran por offset.
    kmrn: BTreeMap<u32, u16>,
    /// Palabras de la EEPROM, indexadas por offset.
    eeprom: BTreeMap<u16, u16>,
    /// El desplazamiento con el que esta EEPROM codifica el offset en `EERD`.
    eerd_shift: u32,
    /// Lo que el driver ha escrito en el PHY, en orden: (direccion, pagina,
    /// registro, valor).
    phy_writes: Vec<(u8, u32, u32, u16)>,
    /// Cuantas veces se ha cogido el semaforo SW/FW.
    swflag_takes: u32,
    /// El driver tiene LANPHYPC forzado ahora mismo.
    lanphypc_forced: bool,
}

std::thread_local! {
    static FAKE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// El MAC falso de este hilo. Mientras vive, [`on_write`] lo atiende; al caer,
/// lo desengancha.
pub(super) struct Hw {
    base: usize,
}

impl Hw {
    /// Un MAC falso con el enlace arriba a gigabit full duplex, el semaforo
    /// libre y sin DMA en vuelo: el estado desde el que arranca un driver que
    /// no se encuentra ninguna sorpresa.
    pub(super) fn new() -> Self {
        let regs: &'static mut [u32] = Box::leak(vec![0u32; 0x4000].into_boxed_slice());
        let base = regs.as_ptr() as usize;
        let mut st = State {
            regs,
            base,
            faults: Faults::default(),
            phy: BTreeMap::new(),
            page: 0,
            kmrn: BTreeMap::new(),
            eeprom: BTreeMap::new(),
            eerd_shift: 2,
            phy_writes: Vec::new(),
            swflag_takes: 0,
            lanphypc_forced: false,
        };
        st.regs[E1000E_STATUS] = STATUS_LU | STATUS_FD | (2 << STATUS_SPEED_SHIFT);
        FAKE.with(|f| *f.borrow_mut() = Some(st));
        Self { base }
    }

    pub(super) fn base(&self) -> usize {
        self.base
    }

    fn with<R>(&self, body: impl FnOnce(&mut State) -> R) -> R {
        FAKE.with(|f| body(f.borrow_mut().as_mut().expect("el MAC falso vive")))
    }

    /// Cambia las averias del MAC falso.
    pub(super) fn faults(&self, body: impl FnOnce(&mut Faults)) {
        self.with(|st| body(&mut st.faults));
    }

    pub(super) fn poke(&self, reg: usize, val: u32) {
        self.with(|st| st.regs[reg] = val);
    }

    pub(super) fn peek(&self, reg: usize) -> u32 {
        self.with(|st| st.regs[reg])
    }

    /// Pone un registro IEEE del PHY (0x00..=0x0F), que es el mismo en todas
    /// las paginas.
    pub(super) fn set_mii(&self, addr: u8, reg: u32, val: u16) {
        self.with(|st| {
            st.phy.insert((addr, 0, reg), val);
        });
    }

    /// Pone un registro paginado del PHY, en la pagina HV `page`.
    pub(super) fn set_phy_hv(&self, page: u32, reg: u32, val: u16) {
        let key = hv_page_select(page) as u32;
        self.with(|st| {
            st.phy.insert((HV_PHY_ADDR, key, reg), val);
        });
    }

    /// Pone un registro del PHY por la seleccion de pagina que tiene que ir
    /// por el cable, sin pasar por `hv_page_select`: un test que sembrara con
    /// la misma funcion que prueba estaria probando la funcion contra si misma.
    pub(super) fn set_phy_raw(&self, addr: u8, page_key: u32, reg: u32, val: u16) {
        self.with(|st| {
            st.phy.insert((addr, page_key, reg), val);
        });
    }

    pub(super) fn phy_hv(&self, page: u32, reg: u32) -> Option<u16> {
        let key = hv_page_select(page) as u32;
        self.with(|st| st.phy.get(&(HV_PHY_ADDR, key, reg)).copied())
    }

    pub(super) fn mii(&self, addr: u8, reg: u32) -> Option<u16> {
        self.with(|st| st.phy.get(&(addr, 0, reg)).copied())
    }

    pub(super) fn phy_writes(&self) -> Vec<(u8, u32, u32, u16)> {
        self.with(|st| st.phy_writes.clone())
    }

    pub(super) fn set_kmrn(&self, offset: u32, val: u16) {
        self.with(|st| {
            st.kmrn.insert(offset, val);
        });
    }

    pub(super) fn kmrn(&self, offset: u32) -> Option<u16> {
        self.with(|st| st.kmrn.get(&offset).copied())
    }

    /// Carga la EEPROM con las palabras del MAC y dice con que desplazamiento
    /// codifica el offset (los e1000e discretos usan 2; algunos, 3).
    pub(super) fn set_eeprom(&self, shift: u32, words: &[(u16, u16)]) {
        self.with(|st| {
            st.eerd_shift = shift;
            for (off, w) in words {
                st.eeprom.insert(*off, *w);
            }
        });
    }

    pub(super) fn swflag_takes(&self) -> u32 {
        self.with(|st| st.swflag_takes)
    }
}

impl Drop for Hw {
    fn drop(&mut self) {
        FAKE.with(|f| *f.borrow_mut() = None);
    }
}

/// La costura: el MAC falso reacciona a una escritura que el driver acaba de
/// dejar en la ventana. `off` son bytes desde BAR0.
pub(super) fn on_write(base: usize, off: usize, val: u32) {
    FAKE.with(|f| {
        let mut slot = f.borrow_mut();
        let Some(st) = slot.as_mut() else { return };
        if st.base != base {
            // La ventana de otro test (los anillos tienen la suya, sin falso
            // detras): ni mirarla.
            return;
        }
        st.react(off / 4, val);
    });
}

impl State {
    fn react(&mut self, reg: usize, val: u32) {
        match reg {
            E1000E_MDIC => self.mdic(val),
            E1000E_EXTCNF_CTRL => self.extcnf_ctrl(val),
            E1000E_KMRNCTRLSTA => self.kmrnctrlsta(val),
            E1000E_EERD => self.eerd(val),
            E1000E_CTRL => self.ctrl(val),
            E1000E_RXDCTL | E1000E_TXDCTL => {
                if self.faults.queue_enable_stuck {
                    self.regs[reg] = val & !(RXDCTL_QUEUE_ENABLE | TXDCTL_QUEUE_ENABLE);
                }
            }
            E1000E_H2ME => self.h2me(val),
            _ => {}
        }
    }

    /// El PHY contesta un acceso MDIO: siempre deja READY, porque un acceso sin
    /// READY es lo que el driver interpreta como «no hay PHY».
    fn mdic(&mut self, val: u32) {
        let addr = ((val >> MDIC_PHYADD_SHIFT) & 0x1F) as u8;
        let reg = (val >> MDIC_REG_SHIFT) & 0x1F;
        if self.faults.mdic_mute.contains(&addr) {
            self.regs[E1000E_MDIC] = val & !MDIC_READY;
            return;
        }
        if self.faults.mdic_error {
            self.regs[E1000E_MDIC] = val | MDIC_READY | MDIC_ERROR;
            return;
        }
        if val & MDIC_OP_WRITE != 0 {
            let data = (val & 0xFFFF) as u16;
            if reg == PHY_PAGE_SELECT_REG {
                // La seleccion de pagina no es un registro del PHY: es el
                // estado con el que se leen los siguientes.
                self.page = data as u32;
            } else {
                let page = self.page_of(reg);
                self.phy.insert((addr, page, reg), data);
                self.phy_writes.push((addr, page, reg, data));
            }
            self.regs[E1000E_MDIC] = val | MDIC_READY;
            return;
        }
        let page = self.page_of(reg);
        let data = self.phy.get(&(addr, page, reg)).copied().unwrap_or(0xFFFF);
        self.regs[E1000E_MDIC] = (val & !0xFFFF) | MDIC_READY | data as u32;
    }

    /// Los registros IEEE (0x00..=0x0F) son los mismos en todas las paginas,
    /// que es justo lo que dice `MAX_PHY_MULTI_PAGE_REG`.
    fn page_of(&self, reg: u32) -> u32 {
        if reg > MAX_PHY_MULTI_PAGE_REG {
            self.page
        } else {
            0
        }
    }

    fn extcnf_ctrl(&mut self, val: u32) {
        if self.faults.swflag_busy {
            self.regs[E1000E_EXTCNF_CTRL] = val | EXTCNF_CTRL_SWFLAG;
            return;
        }
        if self.faults.swflag_no_latch {
            self.regs[E1000E_EXTCNF_CTRL] = val & !EXTCNF_CTRL_SWFLAG;
            return;
        }
        if val & EXTCNF_CTRL_SWFLAG != 0 {
            self.swflag_takes += 1;
        }
    }

    fn kmrnctrlsta(&mut self, val: u32) {
        let offset = (val & KMRNCTRLSTA_OFFSET) >> KMRNCTRLSTA_OFFSET_SHIFT;
        if val & KMRNCTRLSTA_REN != 0 {
            let data = self.kmrn.get(&offset).copied().unwrap_or(0);
            self.regs[E1000E_KMRNCTRLSTA] = (val & !0xFFFF) | data as u32;
        } else {
            self.kmrn.insert(offset, (val & 0xFFFF) as u16);
        }
    }

    fn eerd(&mut self, val: u32) {
        if self.faults.eerd_dead || val & EERD_START == 0 {
            return;
        }
        let addr = val & !EERD_START;
        let offset = (addr >> self.eerd_shift) as u16;
        if (offset as u32) << self.eerd_shift != addr {
            // Un offset que no cabe en la codificacion de esta EEPROM: el
            // ciclo no acaba, que es lo que ve el driver cuando prueba el
            // desplazamiento equivocado.
            return;
        }
        let Some(w) = self.eeprom.get(&offset).copied() else {
            return;
        };
        self.regs[E1000E_EERD] = val | EERD_DONE_BIT4 | ((w as u32) << EERD_DATA_SHIFT);
    }

    fn ctrl(&mut self, val: u32) {
        // Un reset se autobaja; mientras el driver lo espera, el hardware ya
        // ha vuelto.
        if val & CTRL_RST != 0 {
            self.regs[E1000E_CTRL] = val & !CTRL_RST;
        }
        if val & CTRL_GIO_MASTER_DISABLE != 0 && !self.faults.gio_master_stuck {
            self.regs[E1000E_STATUS] &= !STATUS_GIO_MASTER_ENABLE;
        }
        if val & CTRL_GIO_MASTER_DISABLE != 0 && self.faults.gio_master_stuck {
            self.regs[E1000E_STATUS] |= STATUS_GIO_MASTER_ENABLE;
        }
        // El PHY acaba su secuencia de arranque cuando el driver suelta
        // LANPHYPC, no en cualquier escritura de CTRL.
        if val & CTRL_LANPHYPC_OVERRIDE != 0 {
            self.lanphypc_forced = true;
        } else if self.lanphypc_forced {
            self.lanphypc_forced = false;
            if !self.faults.lpcd_never {
                self.regs[E1000E_CTRL_EXT] |= CTRL_EXT_LPCD;
            }
        }
    }

    fn h2me(&mut self, val: u32) {
        if val & H2ME_ENFORCE_SETTINGS != 0 && !self.faults.ulp_cfg_stuck {
            self.regs[E1000E_FWSM] &= !FWSM_ULP_CFG_DONE;
        }
    }
}
