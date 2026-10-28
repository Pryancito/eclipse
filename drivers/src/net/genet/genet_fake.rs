//! Un GENET falso en memoria del anfitrion, con su PHY detras del bus MDIO.
//!
//! La ventana de registros es un `Vec<u32>` del monton cuya direccion hace de
//! base del MAC: el driver lee y escribe ahi directamente, asi que no hay que
//! interceptar ninguna lectura. Lo unico que un modelo en memoria no puede dar
//! es la *reaccion* a una escritura -- un `MDIO_CMD` con `START_BUSY` que
//! nadie baja es un acceso al PHY que no termina nunca -- y eso es lo que hace
//! [`on_write`], la unica costura `cfg(test)` del driver.
//!
//! Un falso por hilo (`thread_local!`), para que las vueltas en paralelo no se
//! pisen la ventana.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
extern crate std;

use super::rings::*;
use super::*;
use crate::net::mii::*;

/// Tamaño de la ventana que se le da al falso, en bytes. Llega hasta los
/// anillos DMA (`0x4000` + 256 descriptores) con sitio de sobra, para que las
/// tandas siguientes no tengan que tocar esto.
const WINDOW_BYTES: usize = 0x10000;

/// Las averias que un test puede pedirle al MAC falso. Todas son cosas que el
/// hardware de verdad hace y que el driver tiene que sobrevivir.
#[derive(Default, Clone)]
pub(super) struct Faults {
    /// El MAC no baja nunca `START_BUSY`: una transaccion MDIO que se queda
    /// colgada.
    pub(super) mdio_never_ready: bool,
    /// Estas direcciones MDIO no tienen PHY: el MAC termina la transaccion
    /// pero con `READ_FAIL`.
    pub(super) mdio_mute: Vec<u8>,
    /// Toda lectura del PHY vuelve con `READ_FAIL`, haya PHY o no.
    pub(super) mdio_read_fail: bool,
}

struct State {
    regs: &'static mut [u32],
    base: usize,
    faults: Faults,
    /// Los registros del PHY, por (direccion MDIO, registro).
    phy: BTreeMap<(u8, u32), u16>,
    /// Lo que el driver ha escrito en el PHY, en orden: (direccion, registro,
    /// valor). Una escritura al PHY no deja rastro en la ventana del MAC, asi
    /// que sin esto no hay forma de comprobarla.
    phy_writes: Vec<(u8, u32, u16)>,
    /// Los valores con los que el driver ha pasado por `UMAC_CMD`, en orden.
    /// La secuencia del reset es justo eso: un orden, no un valor final.
    umac_cmd_writes: Vec<u32>,
    /// Los valores con los que ha pasado por `SYS_RBUF_FLUSH_CTRL`.
    rbuf_flush_writes: Vec<u32>,
    /// Cuantas transacciones MDIO se han arrancado.
    mdio_starts: u32,
    /// Toda escritura del driver, en orden: (offset, valor).
    ///
    /// No es decoracion. Lo unico que importa de completar un descriptor es
    /// **cuando** se escribe el estado respecto a la direccion, y un registro
    /// en memoria no deja rastro de eso: solo queda el ultimo valor. Esta es
    /// la unica forma de comprobar una ordenacion desde el anfitrion.
    writes: Vec<(usize, u32)>,
}

std::thread_local! {
    static FAKE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// La direccion MDIO en la que el falso pone su PHY por defecto, la misma que
/// el device tree de la Raspberry Pi 4 declara para el BCM54213PE.
pub(super) const DEFAULT_PHY_ADDR: u8 = 1;

/// El GENET falso de este hilo. Mientras vive, [`on_write`] lo atiende; al
/// caer, lo desengancha.
pub(super) struct Hw {
    base: usize,
}

impl Hw {
    /// Un GENET v5 con un BCM54213PE en la direccion 1, el enlace arriba a
    /// gigabit full duplex y la autonegociacion terminada: el estado desde el
    /// que arranca un driver que no se encuentra ninguna sorpresa.
    pub(super) fn new() -> Self {
        // Los `udelay` del driver y el plazo del MDIO miran el reloj, y bajo
        // test el reloj solo lo mueve quien lo pide: sin esto una espera se
        // come su tope de vueltas en cada test.
        crate::nvme::nvme_queue::test_clock::set_auto_advance(1);

        let regs: &'static mut [u32] = Box::leak(vec![0u32; WINDOW_BYTES / 4].into_boxed_slice());
        let base = regs.as_ptr() as usize;
        let mut st = State {
            regs,
            base,
            faults: Faults::default(),
            phy: BTreeMap::new(),
            phy_writes: Vec::new(),
            umac_cmd_writes: Vec::new(),
            rbuf_flush_writes: Vec::new(),
            mdio_starts: 0,
            writes: Vec::new(),
        };
        // Un BCM2711 pone un 6 en el nibble de la generacion, que el driver
        // traduce a v5.
        st.regs[SYS_REV_CTRL / 4] = 6 << 24;
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
    pub(super) fn mac(&self) -> GenetMac {
        unsafe { GenetMac::new(self.base) }
    }

    fn with<R>(&self, body: impl FnOnce(&mut State) -> R) -> R {
        FAKE.with(|f| body(f.borrow_mut().as_mut().expect("el GENET falso vive")))
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

    /// Un PHY de la familia del de la Raspberry Pi 4 en esa direccion, con
    /// gigabit anunciado y nada negociado todavia.
    pub(super) fn set_phy_present(&self, pmd: u8) {
        // 0x600d84a2 es el BCM54213PE; el driver compara con la mascara, asi
        // que la revision del nibble bajo no deberia importarle.
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

    /// Los valores por los que paso `UMAC_CMD`, en orden.
    pub(super) fn umac_cmd_writes(&self) -> Vec<u32> {
        self.with(|st| st.umac_cmd_writes.clone())
    }

    /// Los valores por los que paso `SYS_RBUF_FLUSH_CTRL`, en orden.
    pub(super) fn rbuf_flush_writes(&self) -> Vec<u32> {
        self.with(|st| st.rbuf_flush_writes.clone())
    }

    pub(super) fn mdio_starts(&self) -> u32 {
        self.with(|st| st.mdio_starts)
    }

    /// Toda escritura del driver, en orden.
    pub(super) fn writes(&self) -> Vec<(usize, u32)> {
        self.with(|st| st.writes.clone())
    }

    /// Los offsets escritos desde ahora, en orden, para medir una secuencia
    /// sin arrastrar la del arranque.
    pub(super) fn writes_since(&self, marca: usize) -> Vec<usize> {
        self.with(|st| st.writes[marca..].iter().map(|(off, _)| *off).collect())
    }

    /// Cuantas escrituras se han visto, para usarlo como marca.
    pub(super) fn write_mark(&self) -> usize {
        self.with(|st| st.writes.len())
    }
}

// ---------------------------------------------------------------------------
// El DMA del falso
// ---------------------------------------------------------------------------

impl Hw {
    /// Mete una trama en el anillo de recepcion como lo haria el MAC: la copia
    /// en el buffer al que apunta el descriptor, le pone la longitud (con los
    /// dos bytes de alineacion delante, que es como llega de verdad) y sube el
    /// indice del productor.
    ///
    /// `slot` es el descriptor, que el test tiene que llevar por su cuenta
    /// porque el driver no lo publica.
    pub(super) fn deliver_frame(&self, slot: usize, frame: &[u8]) {
        self.deliver_frame_with_status(slot, frame, 0);
    }

    /// Lo mismo, pero con los bits de estado que el MAC quiera poner: los de
    /// error (`DMA_RX_CRC_ERROR` y companIa) y los de clase de trama
    /// (`DMA_RX_BRDCAST`, `DMA_RX_MULT`).
    pub(super) fn deliver_frame_with_status(&self, slot: usize, frame: &[u8], status: u32) {
        let desc = GENET_RX_OFF + slot * DMA_DESC_SIZE;
        let lo = self.peek(desc + DMA_DESC_ADDRESS_LO) as usize;
        let hi = self.peek(desc + DMA_DESC_ADDRESS_HI) as usize;
        let paddr = lo | (hi << 32);
        assert_ne!(paddr, 0, "el descriptor {} no apunta a ningun buffer", slot);
        // `phys_to_virt` es la identidad en el anfitrion.
        unsafe {
            core::ptr::write_bytes((paddr + RX_BUF_OFFSET) as *mut u8, 0, frame.len());
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                (paddr + RX_BUF_OFFSET) as *mut u8,
                frame.len(),
            );
        }
        let declared = (frame.len() + RX_BUF_OFFSET) as u32;
        self.poke(
            desc + DMA_DESC_LENGTH_STATUS,
            (declared << DMA_BUFLENGTH_SHIFT) | DMA_SOP | DMA_EOP | status,
        );
        self.bump_rx_prod(1);
    }

    /// Sube el indice del productor de recepcion, sin tocar ningun
    /// descriptor: asi se llega a la vuelta de los 16 bits sin tener que
    /// entregar 65536 tramas.
    pub(super) fn bump_rx_prod(&self, n: u32) {
        let prod = self.peek(RDMA_PROD_INDEX).wrapping_add(n) & 0xFFFF;
        self.poke(RDMA_PROD_INDEX, prod);
    }

    /// Da por terminadas `n` transmisiones.
    pub(super) fn complete_tx(&self, n: u32) {
        let cons = self.peek(TDMA_CONS_INDEX).wrapping_add(n) & 0xFFFF;
        self.poke(TDMA_CONS_INDEX, cons);
    }

    /// La trama que el driver dejo en un descriptor de transmision, leida por
    /// donde el MAC la leeria.
    pub(super) fn sent_frame(&self, slot: usize) -> Vec<u8> {
        let desc = GENET_TX_OFF + slot * DMA_DESC_SIZE;
        let len_stat = self.peek(desc + DMA_DESC_LENGTH_STATUS);
        let len = ((len_stat >> DMA_BUFLENGTH_SHIFT) & DMA_BUFLENGTH_MASK) as usize;
        let lo = self.peek(desc + DMA_DESC_ADDRESS_LO) as usize;
        let hi = self.peek(desc + DMA_DESC_ADDRESS_HI) as usize;
        let paddr = lo | (hi << 32);
        if paddr == 0 || len == 0 {
            return Vec::new();
        }
        let mut out = vec![0u8; len];
        unsafe { core::ptr::copy_nonoverlapping(paddr as *const u8, out.as_mut_ptr(), len) };
        out
    }

    /// El estado y la longitud de un descriptor de transmision.
    pub(super) fn tx_len_stat(&self, slot: usize) -> u32 {
        self.peek(GENET_TX_OFF + slot * DMA_DESC_SIZE + DMA_DESC_LENGTH_STATUS)
    }

    /// El estado y la longitud de un descriptor de recepcion.
    pub(super) fn rx_len_stat(&self, slot: usize) -> u32 {
        self.peek(GENET_RX_OFF + slot * DMA_DESC_SIZE + DMA_DESC_LENGTH_STATUS)
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
/// Se llama desde `GenetMac::write` y solo bajo `cfg(test)`. Una escritura en
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
            UMAC_CMD => st.umac_cmd_writes.push(val),
            SYS_RBUF_FLUSH_CTRL => st.rbuf_flush_writes.push(val),
            UMAC_MDIO_CMD => st.mdio_cmd(val),
            _ => {}
        }
    });
}

impl State {
    /// Atiende una escritura de `UMAC_MDIO_CMD`.
    ///
    /// Sin `START_BUSY` el driver solo esta cargando la transaccion, y el
    /// hardware no hace nada hasta que la arranca: ese paso en dos tiempos es
    /// el que el driver hace con un read-modify-write, y respetarlo aqui es lo
    /// que hace que el test note si alguna vez deja de hacerlo.
    fn mdio_cmd(&mut self, val: u32) {
        if val & MDIO_START_BUSY == 0 {
            return;
        }
        self.mdio_starts += 1;

        if self.faults.mdio_never_ready {
            // `START_BUSY` se queda puesto: el driver tiene que irse por el
            // plazo.
            return;
        }

        let pmd = ((val >> MDIO_PMD_SHIFT) & MDIO_PMD_MASK) as u8;
        let reg = (val >> MDIO_REG_SHIFT) & MDIO_REG_MASK;
        let is_read = val & MDIO_RD == MDIO_RD;
        let muted = self.faults.mdio_mute.contains(&pmd);

        let mut done = val & !MDIO_START_BUSY;
        if is_read {
            if muted || self.faults.mdio_read_fail {
                // Un bus sin nadie en esa direccion deja las lineas arriba.
                done |= MDIO_READ_FAIL;
                done = (done & 0xFFFF_0000) | 0xFFFF;
            } else {
                let data = self.phy.get(&(pmd, reg)).copied().unwrap_or(0);
                done = (done & 0xFFFF_0000) | data as u32;
            }
        } else if !muted {
            let data = val as u16;
            self.phy.insert((pmd, reg), data);
            self.phy_writes.push((pmd, reg, data));
        }

        self.regs[UMAC_MDIO_CMD / 4] = done;
    }
}
