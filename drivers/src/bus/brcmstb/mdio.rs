//! El MDIO del PHY de PCIe del BCM2712, y las dos cosas que se hacen por él.
//!
//! El puente lleva su propio bus MDIO, que no es el de una tarjeta de red: no
//! hay PHY externo al otro lado, sino los registros internos del SerDes del
//! propio puente. Por aquí se hacen dos cosas sin las que la Pi 5 no levanta
//! el enlace:
//!
//! - **`munge_pll`**: reprogramar el PLL para admitir un reloj de referencia de
//!   54 MHz (el `xosc` de la Pi 5). Es lo primero que hace la secuencia de
//!   arranque del 2712, y sin ello el enlace no sube.
//! - **`set_ssc`**: el reloj de espectro ensanchado, opcional.
//!
//! # De dónde salen los números
//!
//! De `drivers/pci/controller/pcie-brcmstb.c` de `rpi-6.6.y`
//! (`brcm_pcie_mdio_form_pkt`, `brcm_pcie_mdio_read`, `brcm_pcie_mdio_write`,
//! `brcm_pcie_set_ssc`, `brcm_pcie_munge_pll`), con el nombre de cada constante
//! de Linux al lado.
//!
//! # Qué NO está validado
//!
//! **Nada de esto ha corrido en una Pi 5.** Lo comprobable es el protocolo:
//! qué se escribe, en qué orden y qué se espera de vuelta. Eso es lo que miden
//! los tests, contra un PHY de mentira que imita el acuse del bus.

use super::{RC_DL_MDIO_ADDR, RC_DL_MDIO_RD_DATA, RC_DL_MDIO_WR_DATA};
use core::ptr::{read_volatile, write_volatile};

const MODULE: &str = "brcm-pcie-mdio";

/// `MDIO_PORT0`: el único puerto que se usa, el del SerDes del propio puente.
pub const PORT0: u8 = 0;

/// `MDIO_PORT_MASK`: el puerto va en los bits 16-19.
pub const PORT_MASK: u32 = 0x000f_0000;
/// `MDIO_REGAD_MASK`: el registro, en los 16 de abajo.
pub const REGAD_MASK: u32 = 0x0000_ffff;
/// `MDIO_CMD_MASK`: la orden, en los 12 de arriba.
pub const CMD_MASK: u32 = 0xfff0_0000;
/// `MDIO_CMD_READ`.
pub const CMD_READ: u32 = 0x1;
/// `MDIO_CMD_WRITE`.
pub const CMD_WRITE: u32 = 0x0;

/// `MDIO_DATA_MASK`: los datos son los 31 bits de abajo. El de arriba es el
/// acuse, y se quita al devolver una lectura.
pub const DATA_MASK: u32 = 0x7fff_ffff;
/// `MDIO_DATA_DONE_MASK`: el bit 31, el acuse del bus.
pub const DATA_DONE: u32 = 0x8000_0000;

/// `SET_ADDR_OFFSET`: el registro por el que se elige el **bloque** de
/// registros del PHY al que van los accesos siguientes.
///
/// El direccionamiento es de dos pasos: primero se escribe en este registro la
/// dirección del bloque, y luego se leen y escriben los registros *de dentro*
/// de ese bloque por su número. Un acceso a un registro sin haber puesto antes
/// su bloque va a otro sitio sin dar error.
pub const SET_ADDR_OFFSET: u8 = 0x1f;

/// `SSC_REGS_ADDR`: el bloque del reloj de espectro ensanchado.
///
/// Vale `0x1100`, que es **el mismo número** que el offset del registro
/// [`RC_DL_MDIO_ADDR`] en la ventana del puente. Son dos cosas sin ninguna
/// relación: una es un offset de MMIO y la otra una dirección de bloque dentro
/// del PHY.
pub const SSC_REGS_ADDR: u16 = 0x1100;
/// `SSC_CNTL_OFFSET`, dentro del bloque de SSC.
pub const SSC_CNTL_OFFSET: u8 = 0x2;
/// `SSC_CNTL_OVRD_EN_MASK`.
pub const SSC_CNTL_OVRD_EN: u32 = 0x8000;
/// `SSC_CNTL_OVRD_VAL_MASK`.
pub const SSC_CNTL_OVRD_VAL: u32 = 0x4000;
/// `SSC_STATUS_OFFSET`.
pub const SSC_STATUS_OFFSET: u8 = 0x1;
/// `SSC_STATUS_SSC_MASK`.
pub const SSC_STATUS_SSC: u32 = 0x400;
/// `SSC_STATUS_PLL_LOCK_MASK`.
pub const SSC_STATUS_PLL_LOCK: u32 = 0x800;

/// Cuántas veces se mira el acuse, y cada cuánto: Linux escribe
/// `readl_poll_timeout_atomic(..., 10, 100)`, o sea diez vueltas de 10 µs.
pub const POLL_TRIES: u32 = 10;
/// El paso entre vueltas.
pub const POLL_STEP_US: u64 = 10;

/// Lo que espera `set_ssc` entre pedir el ensanchado y preguntar si cuajó.
pub const SSC_SETTLE_US: u64 = 1000;
/// Lo que espera `munge_pll` tras reprogramar el PLL.
pub const PLL_SETTLE_US: u64 = 100;

/// `brcm_pcie_mdio_form_pkt`: el paquete que se deja en el registro de
/// dirección.
pub const fn form_pkt(port: u8, regad: u8, cmd: u32) -> u32 {
    let p = ((port as u32) << PORT_MASK.trailing_zeros()) & PORT_MASK;
    let r = ((regad as u32) << REGAD_MASK.trailing_zeros()) & REGAD_MASK;
    let c = (cmd << CMD_MASK.trailing_zeros()) & CMD_MASK;
    p | r | c
}

/// Por qué no salió un acceso al MDIO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdioError {
    /// El bus no acusó dentro del plazo.
    SinAcuse,
    /// El ensanchado se pidió y el PHY no lo dio, o el PLL no engancha.
    SinEnsanchado,
}

/// El bus MDIO del puente, visto por sus tres registros.
#[derive(Debug)]
pub struct MdioBus {
    base: usize,
}

impl MdioBus {
    /// # Safety
    /// `base` tiene que ser la misma base que la de [`super::BrcmPcie`]: estos
    /// tres registros viven en la ventana del puente.
    pub const unsafe fn new(base: usize) -> Self {
        Self { base }
    }

    /// `brcm_pcie_mdio_read`.
    ///
    /// Tras dejar el paquete en el registro de dirección se **relee ese mismo
    /// registro**, y no para mirar nada: es el vaciado de una escritura
    /// diferida. Sin esa lectura, el sondeo del acuse puede adelantarse a la
    /// escritura que lo provoca.
    ///
    /// El valor que se devuelve lleva el bit del acuse **quitado**: los datos
    /// son los 31 de abajo.
    pub fn read(&self, port: u8, regad: u8) -> Result<u32, MdioError> {
        self.write_reg(RC_DL_MDIO_ADDR, form_pkt(port, regad, CMD_READ));
        let _ = self.read_reg(RC_DL_MDIO_ADDR);

        for _ in 0..POLL_TRIES {
            let data = self.read_reg(RC_DL_MDIO_RD_DATA);
            if data & DATA_DONE != 0 {
                return Ok(data & DATA_MASK);
            }
            super::reset::udelay(POLL_STEP_US);
        }
        warn!(
            "{}: el bus no acuso la lectura del registro {:#x}",
            MODULE, regad
        );
        Err(MdioError::SinAcuse)
    }

    /// `brcm_pcie_mdio_write`.
    ///
    /// **El acuse de una escritura es el mismo bit que el de una lectura y con
    /// la polaridad al revés.** `MDIO_RD_DONE` dice que ha acabado cuando el
    /// bit 31 está **puesto**, y `MDIO_WT_DONE` cuando está **limpio**. El dato
    /// se escribe con ese bit a uno y es el silicio quien lo baja al terminar,
    /// así que esperar a verlo puesto aquí sería esperar para siempre.
    pub fn write(&self, port: u8, regad: u8, wrdata: u16) -> Result<(), MdioError> {
        self.write_reg(RC_DL_MDIO_ADDR, form_pkt(port, regad, CMD_WRITE));
        let _ = self.read_reg(RC_DL_MDIO_ADDR);
        self.write_reg(RC_DL_MDIO_WR_DATA, DATA_DONE | u32::from(wrdata));

        for _ in 0..POLL_TRIES {
            if self.read_reg(RC_DL_MDIO_WR_DATA) & DATA_DONE == 0 {
                return Ok(());
            }
            super::reset::udelay(POLL_STEP_US);
        }
        warn!(
            "{}: el bus no acuso la escritura del registro {:#x}",
            MODULE, regad
        );
        Err(MdioError::SinAcuse)
    }

    /// `brcm_pcie_set_ssc`: pide el reloj de espectro ensanchado.
    ///
    /// Hacen falta **los dos** bits del estado: el ensanchado puesto y el PLL
    /// enganchado. Con uno solo el reloj no sirve.
    pub fn set_ssc(&self) -> Result<(), MdioError> {
        self.write(PORT0, SET_ADDR_OFFSET, SSC_REGS_ADDR)?;

        let mut tmp = self.read(PORT0, SSC_CNTL_OFFSET)?;
        tmp |= SSC_CNTL_OVRD_EN | SSC_CNTL_OVRD_VAL;
        self.write(PORT0, SSC_CNTL_OFFSET, tmp as u16)?;

        super::reset::udelay(SSC_SETTLE_US);

        let tmp = self.read(PORT0, SSC_STATUS_OFFSET)?;
        if tmp & SSC_STATUS_SSC != 0 && tmp & SSC_STATUS_PLL_LOCK != 0 {
            Ok(())
        } else {
            warn!(
                "{}: el ensanchado no cuajo (estado {:#x})",
                MODULE, tmp as u16
            );
            Err(MdioError::SinEnsanchado)
        }
    }

    /// `brcm_pcie_munge_pll`: reprograma el PLL para un reloj de referencia de
    /// 54 MHz.
    ///
    /// Siete registros del bloque `0x1600` con valores fijos. Son los que son
    /// porque los dio el fabricante; no hay hoja que los explique, y el propio
    /// Linux los trae con el comentario de quien los dictó.
    ///
    /// **Ese comentario no coincide con el código, y el código es el que
    /// manda**: el comentario sólo nombra cuatro de los siete registros, y para
    /// el `0x17` da `0xbd1a` donde el código escribe `0xbda1`. Copiar el
    /// comentario deja el PLL con dos nibbles del revés en un registro y tres
    /// registros sin tocar.
    ///
    /// Un fallo de MDIO aquí **no aborta**: Linux ignora el resultado de cada
    /// acceso, y parar a mitad dejaría el PLL medio reprogramado, que es peor
    /// que no haberlo tocado. Lo que sí se hace es contarlo.
    pub fn munge_pll(&self) -> usize {
        /// El bloque del PLL.
        const PLL_BLOCK: u16 = 0x1600;
        /// Los registros y sus valores, del código de Linux y no de su
        /// comentario.
        const REGS: [u8; 7] = [0x16, 0x17, 0x18, 0x19, 0x1b, 0x1c, 0x1e];
        const DATA: [u16; 7] = [0x50b9, 0xbda1, 0x0094, 0x97b4, 0x5030, 0x5030, 0x0007];

        let mut fallos = 0usize;
        if self.write(PORT0, SET_ADDR_OFFSET, PLL_BLOCK).is_err() {
            fallos += 1;
        }
        for (reg, val) in REGS.iter().zip(DATA.iter()) {
            if self.write(PORT0, *reg, *val).is_err() {
                fallos += 1;
            }
        }
        super::reset::udelay(PLL_SETTLE_US);
        if fallos != 0 {
            warn!(
                "{}: {} accesos del PLL no acusaron; se sigue igual",
                MODULE, fallos
            );
        }
        fallos
    }

    fn read_reg(&self, off: usize) -> u32 {
        let val = unsafe { read_volatile((self.base + off) as *const u32) };
        #[cfg(test)]
        let val = mdio_fake::al_leer(self.base, off, val);
        val
    }

    fn write_reg(&self, off: usize, val: u32) {
        #[cfg(test)]
        let val = mdio_fake::al_escribir(self.base, off, val);
        // El MDIO escribe en la ventana DEL PUENTE, asi que sus escrituras van
        // a la misma traza: es la unica forma de comprobar que el PLL se
        // reprograma con el SerDes ya encendido.
        #[cfg(test)]
        super::bridge_fake::apunta(super::bridge_fake::PUENTE, off, val);
        unsafe { write_volatile((self.base + off) as *mut u32, val) }
    }
}

/// Un PHY de mentira: lo que un banco de registros no sabe hacer es **acusar**,
/// y el acuse es justo lo que hay que comprobar aquí.
#[cfg(test)]
pub(super) mod mdio_fake;

#[cfg(test)]
mod mdio_tests;
