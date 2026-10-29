//! La secuencia de arranque del puente PCIe del BCM2712, y el único sitio
//! donde las tandas anteriores se juntan.
//!
//! Las tandas de antes pusieron las piezas: el mapa de registros, los dos
//! controladores de reset y el MDIO del PHY. Esta las usa **en orden**, que es
//! lo que de verdad importa aquí: casi ningún paso se puede adelantar ni
//! retrasar, y el chip no se queja cuando se hace mal —— simplemente el enlace
//! no sube, o sube y las lecturas de configuración abortan la CPU.
//!
//! # De dónde sale el orden
//!
//! De `brcm_pcie_probe`, `brcm_pcie_setup` y `brcm_pcie_start_link` de
//! `drivers/pci/controller/pcie-brcmstb.c` (`rpi-6.6.y`), quedándose en cada
//! bifurcación con la rama del `BCM2712`.
//!
//! # Lo que esta tanda deja fuera a propósito
//!
//! - **Las MSI** y el controlador `brcm,bcm2712-mip-intc` del que cuelgan.
//! - **Las ventanas de entrada de más** (`RC_BAR4`..`RC_BAR10`), que son siete
//!   y salen del `dma-ranges` del nodo.
//! - **El `clkreq`** y los estados de bajo consumo, que se configuran cuando
//!   ya hay un dispositivo enumerado detrás.
//! - **Leer la configuración del nodo del árbol**: aquí se recibe ya masticada
//!   en [`BridgeConfig`], para que la secuencia se pueda comprobar sin arrastrar
//!   el lector del árbol.
//!
//! # Qué NO está validado
//!
//! **Nada de esto ha corrido en una Pi 5.** Lo comprobable es el orden y los
//! valores: qué registro se toca, con qué bits, antes y después de qué. Eso es
//! lo que miden los tests, sobre la traza de escrituras del puente.

use super::mdio::MdioBus;
use super::reset::{udelay, BrcmstbReset, PcieRescal, RescalError, RESET_BRIDGE};
use super::*;

const MODULE: &str = "brcm-pcie";

// --- los plazos -------------------------------------------------------------

/// Lo que se espera con el puente en reset antes de sacarlo.
pub const BRIDGE_RESET_US: u64 = 100;
/// Y lo que se espera a que el SerDes se estabilice tras encenderlo.
pub const SERDES_SETTLE_US: u64 = 100;

/// Lo que hay que esperar tras soltar PERST#, por la especificación CEM de
/// PCIe (§2.2) y PCIe r5.0 §6.6.1. Son **100 ms**, y no es un margen de
/// cortesía: es lo que la norma le concede al dispositivo.
pub const PERST_WAIT_MS: u64 = 100;
/// Y después, otros 100 ms mirando si el enlace sube, de cinco en cinco.
pub const LINK_POLL_TOTAL_MS: u64 = 100;
/// El paso del sondeo del enlace.
pub const LINK_POLL_STEP_MS: u64 = 5;

/// `PCIE_MISC_UBUS_TIMEOUT` para el 2712: ~250 ms en relojes de 750 MHz.
///
/// **Los dos plazos de abajo tienen que ser grandes los dos.** El plazo del
/// UBUS corta también los reintentos de CRS, así que una petición muere en
/// cuanto expira el primero de los dos; poner uno corto hace inútil al otro.
pub const UBUS_TIMEOUT_2712: u32 = 0x0B2D_0000;
/// `PCIE_MISC_RC_CONFIG_RETRY_TIMEOUT` para el 2712: ~240 ms, algo por debajo
/// del del UBUS.
pub const RC_RETRY_TIMEOUT_2712: u32 = 0x0ABA_0000;

/// Lo que se devuelve en una lectura que falla: **todo unos**.
///
/// Es lo que convierte un dispositivo que no está en un `0xffff` en vez de en
/// un aborto de la CPU, y va junto con suprimir las respuestas de error del
/// UBUS. Sin las dos cosas, un barrido de buses se lleva la máquina por
/// delante en el primer hueco.
pub const AXI_READ_ERROR_DATA_ALL_ONES: u32 = 0xffff_ffff;

/// Con cuántas peticiones en vuelo se conforma el 2712C1, que no tiene el
/// arreglo del QoS.
pub const AXI_OUTSTANDING_FALLBACK: u32 = 15;

// --- la configuración -------------------------------------------------------

/// Una ventana de salida: lo que la CPU ve en `cpu` sale al bus como `pcie`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutboundWin {
    /// La dirección por el lado de la CPU.
    pub cpu: u64,
    /// La misma ventana vista desde el bus PCIe.
    pub pcie: u64,
    /// Su tamaño en bytes.
    pub size: u64,
}

/// Lo que la secuencia necesita saber y no está en ningún registro.
#[derive(Clone, Copy, Debug)]
pub struct BridgeConfig {
    /// Dónde empieza la memoria del sistema **en direcciones de PCIe**, del
    /// `dma-ranges` del nodo. En la Pi 5 es `0x10_00000000`.
    pub inbound_pci: u64,
    /// Y cuánta hay. Se redondea a la potencia de dos de abajo al codificarla,
    /// porque la ventana no puede valer otra cosa.
    pub inbound_size: u64,
    /// Las ventanas de salida, hasta [`NUM_OUT_WINS`].
    pub outbound: [Option<OutboundWin>; NUM_OUT_WINS],
    /// A qué generación limitar el enlace, del `max-link-speed` del nodo. La
    /// Pi 5 declara `<2>`.
    pub gen: Option<u8>,
    /// Si pedir el reloj de espectro ensanchado (`brcm,enable-ssc`). La Pi 5
    /// no lo declara.
    pub ssc: bool,
}

/// Por qué no se levantó el puente.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BringUpError {
    /// El recalibrado del PHY no salió.
    Rescal(RescalError),
    /// Una línea de reset que el controlador no tiene.
    LineaDeReset(u32),
    /// Una ventana de salida que el puente no pudo abrir.
    VentanaDeSalida(usize),
    /// El enlace no subió dentro del plazo.
    EnlaceCaido,
}

/// Cómo quedó el enlace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkInfo {
    /// El código de velocidad del `LNKSTA`, no la velocidad: 1 es Gen1, 2
    /// Gen2, 3 Gen3.
    pub speed_code: u8,
    /// Cuántas líneas negoció.
    pub width: u8,
    /// Si el ensanchado cuajó. `false` también cuando no se pidió.
    pub ssc: bool,
}

impl BrcmPcie {
    /// Levanta el puente, de principio a fin.
    ///
    /// El orden de los tres grandes pasos es el del `probe` de Linux y no se
    /// puede barajar:
    ///
    /// 1. **El recalibrado del PHY**, que en Linux es el
    ///    `reset_control_deassert(rescal)` del `probe` y pasa *antes* de tocar
    ///    un solo registro del puente.
    /// 2. **La preparación** ([`BrcmPcie::setup`]), con el puente dentro y
    ///    fuera de reset.
    /// 3. **El enlace** ([`BrcmPcie::start_link`]), que suelta PERST# y espera.
    pub fn bring_up(
        &self,
        rst: &BrcmstbReset,
        rescal: &PcieRescal,
        mdio: &MdioBus,
        cfg: &BridgeConfig,
    ) -> Result<LinkInfo, BringUpError> {
        rescal.start().map_err(BringUpError::Rescal)?;
        self.setup(rst, mdio, cfg)?;
        self.start_link(mdio, cfg)
    }

    /// `brcm_pcie_setup`, quedándose con la rama del 2712.
    ///
    /// Dos cosas del orden que no se ven leyendo el código suelto:
    ///
    /// - **El puente tiene que estar fuera de reset antes de tocar el registro
    ///   del SerDes.** Lo dice el propio Linux en su camino de reanudación
    ///   («Take bridge out of reset so we can access the SERDES reg»), y es la
    ///   razón de que el reset se suelte antes de encender el SerDes y no
    ///   después.
    /// - **El PLL se reprograma con el SerDes ya encendido y estable**, no
    ///   antes: reprogramarlo con el SerDes en reposo no deja nada puesto.
    pub fn setup(
        &self,
        rst: &BrcmstbReset,
        mdio: &MdioBus,
        cfg: &BridgeConfig,
    ) -> Result<(), BringUpError> {
        // 1. El puente, a reset. En el 2712 esto es una línea del controlador
        //    de resets, no un registro de esta ventana.
        if !rst.assert_line(RESET_BRIDGE) {
            return Err(BringUpError::LineaDeReset(RESET_BRIDGE));
        }
        udelay(BRIDGE_RESET_US);
        if !rst.deassert_line(RESET_BRIDGE) {
            return Err(BringUpError::LineaDeReset(RESET_BRIDGE));
        }

        // 2. El SerDes se enciende LIMPIANDO su bit de reposo, no poniéndolo.
        let tmp = self.read(MISC_HARD_DEBUG_2712);
        self.write(MISC_HARD_DEBUG_2712, tmp & !HARD_DEBUG_SERDES_IDDQ);
        udelay(SERDES_SETTLE_US);

        // 3. Con el SerDes ya en pie, el PLL para el reloj de 54 MHz, y el
        //    periodo del reloj de gestión de energía (la errata de L1SS).
        mdio.munge_pll();
        self.modify(RC_PL_PHY_CTL_15, PM_CLK_PERIOD, PM_CLK_PERIOD_2712);

        // 4. Los tres campos del MISC_CTRL, de una vez: dejarlos en tres
        //    escrituras sueltas es tres veces el mismo lee-modifica-escribe.
        let mut tmp = self.read(MISC_MISC_CTRL);
        tmp |= MISC_CTRL_SCB_ACCESS_EN | MISC_CTRL_CFG_READ_UR_MODE;
        tmp = (tmp & !MISC_CTRL_MAX_BURST_SIZE)
            | ((BURST_SIZE_2712 << MISC_CTRL_MAX_BURST_SIZE.trailing_zeros())
                & MISC_CTRL_MAX_BURST_SIZE);
        self.write(MISC_MISC_CTRL, tmp);

        self.set_tc_qos();

        // 5. La ventana de entrada: por dónde ve el bus la memoria del sistema.
        let tam = encode_ibar_size(cfg.inbound_size);
        let mut lo = cfg.inbound_pci as u32;
        lo = (lo & !RC_BAR_CONFIG_LO_SIZE) | (tam & RC_BAR_CONFIG_LO_SIZE);
        self.write(MISC_RC_BAR2_CONFIG_LO, lo);
        self.write(MISC_RC_BAR2_CONFIG_HI, (cfg.inbound_pci >> 32) as u32);
        self.modify(
            MISC_UBUS_BAR2_CONFIG_REMAP,
            UBUS_BAR_CONFIG_REMAP_ACCESS_ENABLE,
            1,
        );

        // 6. Que una lectura fallida vuelva con todo unos en vez de abortar la
        //    CPU. Las dos cosas van juntas: suprimir el error del UBUS sin dar
        //    un dato de relleno deja la lectura colgando.
        let mut tmp = self.read(MISC_UBUS_CTRL);
        tmp |= UBUS_CTRL_REPLY_ERR_DIS | UBUS_CTRL_REPLY_DECERR_DIS;
        self.write(MISC_UBUS_CTRL, tmp);
        self.write(MISC_AXI_READ_ERROR_DATA, AXI_READ_ERROR_DATA_ALL_ONES);

        self.write(MISC_UBUS_TIMEOUT, UBUS_TIMEOUT_2712);
        self.write(MISC_RC_CONFIG_RETRY_TIMEOUT, RC_RETRY_TIMEOUT_2712);

        // 7. Las dos ventanas de entrada que no se usan se APAGAN, y se apagan
        //    poniendo su campo de tamaño a cero.
        self.modify(MISC_RC_BAR1_CONFIG_LO, RC_BAR_CONFIG_LO_SIZE, 0);
        self.modify(MISC_RC_BAR3_CONFIG_LO, RC_BAR_CONFIG_LO_SIZE, 0);

        // 8. El código de clase. Por defecto el núcleo se anuncia como
        //    *endpoint*, y un barrido de buses que lee eso no busca nada
        //    detrás: sin esto el RP1 no se descubre aunque el enlace esté
        //    arriba.
        self.modify(RC_CFG_PRIV1_ID_VAL3, CLASS_CODE, CLASS_CODE_PCI_BRIDGE);

        // 9. Las ventanas de salida.
        for (i, win) in cfg.outbound.iter().enumerate() {
            if let Some(w) = win {
                if !self.set_outbound_win(i, w.cpu, w.pcie, w.size) {
                    return Err(BringUpError::VentanaDeSalida(i));
                }
            }
        }

        // 10. Y el orden de bytes de la ventana de entrada, al final.
        self.modify(
            RC_CFG_VENDOR_SPECIFIC_REG1,
            ENDIAN_MODE_BAR2,
            ENDIAN_MODE_LITTLE,
        );
        Ok(())
    }

    /// `brcm_pcie_set_tc_qos`, que en el 2712 hace tres cosas y una de ellas
    /// depende de lo que el chip conteste.
    ///
    /// El bit de arreglo del QoS **se relee**: si vuelve a cero es que en este
    /// chip está reservado, o sea un 2712C1 o un puente de una sola línea, y
    /// entonces el remedio es otro —— limitar las peticiones en vuelo. No es
    /// comprobar que la escritura llegó: es preguntarle al chip qué es.
    pub fn set_tc_qos(&self) {
        let mut reg = self.read(MISC_AXI_INTF_CTRL);
        reg &= !AXI_REQFIFO_EN_QOS_PROPAGATION;
        reg |=
            AXI_EN_RCLK_QOS_ARRAY_FIX | AXI_EN_QOS_UPDATE_TIMING_FIX | AXI_DIS_QOS_GATING_IN_MASTER;
        self.write(MISC_AXI_INTF_CTRL, reg);

        let reg = self.read(MISC_AXI_INTF_CTRL);
        if reg & AXI_EN_QOS_UPDATE_TIMING_FIX == 0 {
            let reg = (reg & !AXI_MASTER_MAX_OUTSTANDING_REQUESTS) | AXI_OUTSTANDING_FALLBACK;
            self.write(MISC_AXI_INTF_CTRL, reg);
        }

        // La recepción de VDM queda apagada, que es el mapa de QoS por defecto.
        let reg = self.read(MISC_CTRL_1);
        self.write(MISC_CTRL_1, reg & !MISC_CTRL_1_EN_VDM_QOS_CONTROL);
    }

    /// `brcm_pcie_start_link`.
    ///
    /// Los dos plazos son el peor caso del arranque, **200 ms**, y los dos
    /// hacen falta: los primeros 100 ms son lo que la norma CEM le concede al
    /// dispositivo tras PERST#, y no se pueden recortar mirando si el enlace ya
    /// subió; los otros 100 son el sondeo, y ésos sí se cortan en cuanto sube.
    pub fn start_link(&self, mdio: &MdioBus, cfg: &BridgeConfig) -> Result<LinkInfo, BringUpError> {
        self.perst_set(false);
        udelay(PERST_WAIT_MS * 1000);

        let mut vueltas = 0;
        while !self.link_up() && vueltas < LINK_POLL_TOTAL_MS / LINK_POLL_STEP_MS {
            udelay(LINK_POLL_STEP_MS * 1000);
            vueltas += 1;
        }
        if !self.link_up() {
            warn!("{}: el enlace no subio", MODULE);
            return Err(BringUpError::EnlaceCaido);
        }

        if let Some(gen) = cfg.gen {
            self.set_gen(gen);
        }

        let ssc = cfg.ssc && mdio.set_ssc().is_ok();
        if cfg.ssc && !ssc {
            warn!("{}: el ensanchado no cuajo; se sigue sin el", MODULE);
        }

        let lnksta = self.read16(CAP_REGS + CAP_LNKSTA);
        let info = LinkInfo {
            speed_code: ((lnksta & LNKSTA_CLS) >> LNKSTA_CLS.trailing_zeros()) as u8,
            width: ((lnksta & LNKSTA_NLW) >> LNKSTA_NLW.trailing_zeros()) as u8,
            ssc,
        };

        // La visibilidad de CRS la borra PERST#, así que se vuelve a pedir
        // aquí, y sólo si el puente dice que la tiene.
        if self.read16(CAP_REGS + CAP_RTCAP) & RTCAP_CRSVIS != 0 {
            self.modify16(CAP_REGS + CAP_RTCTL, RTCTL_CRSSVE, 1);
        }
        Ok(info)
    }

    /// `brcm_pcie_set_gen`: limita el enlace a una generación.
    ///
    /// Se tocan **dos** registros, y son de anchuras distintas: el `LNKCAP` es
    /// de 32 bits y el `LNKCTL2` de 16. Y en el `LNKCTL2` el campo son los
    /// cuatro bits de abajo sin nombre propio en la cabecera de Linux, que
    /// coinciden con los del `LNKCAP_SLS` por casualidad del formato.
    pub fn set_gen(&self, gen: u8) {
        let gen = u32::from(gen);
        let lnkcap = self.read(CAP_REGS + CAP_LNKCAP);
        self.write(CAP_REGS + CAP_LNKCAP, (lnkcap & !LNKCAP_SLS) | gen);

        let lnkctl2 = self.read16(CAP_REGS + CAP_LNKCTL2);
        self.write16(CAP_REGS + CAP_LNKCTL2, (lnkctl2 & !0xf) | gen as u16);
    }
}

#[cfg(test)]
mod bringup_tests;
