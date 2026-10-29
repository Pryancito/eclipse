//! El controlador PCIe raíz de Broadcom STB, en su variante **BCM2712** (la
//! del SoC de la Raspberry Pi 5).
//!
//! Esta es la pieza que falta para que la dirección que [`crate::utils::devicetree`]
//! ya sabe traducir conteste: el Ethernet de la Pi 5 vive dentro del RP1, y el
//! RP1 cuelga de este puente. Sin levantar el puente, `0x1f_00100000` es una
//! ventana que no responde.
//!
//! El nodo del árbol es
//!
//! ```text
//! compatible = "brcm,bcm2712-pcie";
//! reg        = <0x10 0x00120000 0x0 0x9310>;
//! max-link-speed = <2>;
//! interrupts = <GIC_SPI 233 ...>, <GIC_SPI 234 ...>;   /* "pcie", "msi" */
//! resets     = <&bcm_reset 32>, <&bcm_reset 44>, <&pcie_rescal>;
//! ```
//!
//! # De dónde salen los números
//!
//! Todos los offsets y máscaras de aquí están casados **uno a uno** contra el
//! nombre de la constante publicada en `drivers/pci/controller/pcie-brcmstb.c`
//! del árbol de Raspberry Pi (rama `rpi-6.6.y`), y el nombre de Linux va escrito
//! al lado de cada uno. Del BCM2712 no hay hoja de datos pública, así que esa
//! fuente es la única que hay.
//!
//! # Qué NO está validado
//!
//! **Nada de esto ha corrido en una Pi 5.** Aquí no hay placa, y QEMU no emula
//! este controlador, así que lo único que se puede comprobar es la aritmética:
//! qué registro se toca, con qué bits y en qué orden. Eso es lo que miden los
//! tests, contra un banco de registros falso. La secuencia de arranque en sí
//! (tanda 3) y el controlador de interrupciones del RP1 (tanda 4) vienen aparte.
//!
//! # Las trampas del BCM2712
//!
//! El driver de Linux es genérico para toda la familia STB y el 2712 se desvía
//! de la variante genérica en seis sitios. Cada uno está documentado en el
//! elemento al que afecta; en resumen:
//!
//! 1. La ventana de datos de configuración es **0x8000**, aunque la tabla de
//!    offsets del 2712 diga `[EXT_CFG_DATA] = 0x9004`: esa entrada sólo la lee
//!    `brcm7425_pcie_map_bus`, y el 2712 usa `brcm_pcie_map_bus`, que escribe
//!    los literales. Ver [`EXT_CFG_DATA`].
//! 2. `PCIE_HARD_DEBUG` es **0x4304** en el 2712, no el offset genérico.
//! 3. `RGR1_SW_INIT_1` **no existe** en la tabla del 2712: el reset del puente
//!    es un *reset controller* del árbol, no un registro de esta ventana.
//! 4. PERST# vive en [`MISC_PCIE_CTRL`] bit 2 y es **PERST*B*** — polaridad
//!    invertida. Ver [`BrcmPcie::perst_set`].
//! 5. El bit de «modo RC» del 2712 (`0x40`) es **el mismo bit** que
//!    `PCIE_LINK_IN_L23`, y Linux remata su lectura con `| 1`, así que la
//!    comprobación está muerta. Ver [`BrcmPcie::rc_mode`].
//! 6. El tamaño de ráfaga del 2712 es `1`, que en su codificación son 128 bytes
//!    (en la genérica, `1` son 256). Ver [`BURST_SIZE_2712`].

use core::ptr::{read_volatile, write_volatile};

const MODULE: &str = "brcm-pcie";

// --- el mapa de registros, contra `pcie-brcmstb.c` --------------------------

/// `BRCM_PCIE_CAP_REGS`: la base de las capacidades PCIe obligatorias dentro
/// del espacio de configuración del puente.
pub const CAP_REGS: usize = 0x00ac;

// Los offsets de dentro de la capacidad PCIe, contra
// `include/uapi/linux/pci_regs.h`. Son relativos a [`CAP_REGS`], no a la
// ventana, y se acceden **de 16 bits** salvo `LNKCAP`.

/// `PCI_EXP_LNKCAP` (32 bits).
pub const CAP_LNKCAP: usize = 0x0c;
/// `PCI_EXP_LNKCAP_SLS`: las velocidades que el puente anuncia soportar.
pub const LNKCAP_SLS: u32 = 0xf;
/// `PCI_EXP_LNKSTA` (16 bits).
pub const CAP_LNKSTA: usize = 0x12;
/// `PCI_EXP_LNKSTA_CLS`: la velocidad a la que quedo el enlace.
pub const LNKSTA_CLS: u16 = 0x000f;
/// `PCI_EXP_LNKSTA_NLW`: y con cuantas lineas.
pub const LNKSTA_NLW: u16 = 0x03f0;
/// `PCI_EXP_RTCTL` (16 bits).
pub const CAP_RTCTL: usize = 0x1c;
/// `PCI_EXP_RTCTL_CRSSVE`.
pub const RTCTL_CRSSVE: u16 = 0x0010;
/// `PCI_EXP_RTCAP` (16 bits).
pub const CAP_RTCAP: usize = 0x1e;
/// `PCI_EXP_RTCAP_CRSVIS`.
pub const RTCAP_CRSVIS: u16 = 0x0001;
/// `PCI_EXP_LNKCTL2` (16 bits).
pub const CAP_LNKCTL2: usize = 0x30;

/// `PCIE_RC_CFG_VENDOR_VENDOR_SPECIFIC_REG1`.
pub const RC_CFG_VENDOR_SPECIFIC_REG1: usize = 0x0188;
/// `..._ENDIAN_MODE_BAR2_MASK`.
pub const ENDIAN_MODE_BAR2: u32 = 0xc;
/// `PCIE_RC_CFG_VENDOR_SPCIFIC_REG1_LITTLE_ENDIAN` (el nombre está mal escrito
/// en Linux; se deja el valor, no la falta).
pub const ENDIAN_MODE_LITTLE: u32 = 0x0;

/// `PCIE_RC_CFG_PRIV1_ID_VAL3`: el código de clase que el puente anuncia.
pub const RC_CFG_PRIV1_ID_VAL3: usize = 0x043c;
/// `..._CLASS_CODE_MASK`.
pub const CLASS_CODE: u32 = 0x00ff_ffff;
/// El código de clase de un puente PCIe-a-PCIe. Por defecto el núcleo se
/// anuncia como *endpoint*, que es lo que hace que un barrido de buses no vea
/// nada detrás.
pub const CLASS_CODE_PCI_BRIDGE: u32 = 0x0006_0400;

/// `PCIE_RC_CFG_PRIV1_LINK_CAPABILITY`.
pub const RC_CFG_PRIV1_LINK_CAPABILITY: usize = 0x04dc;
/// `..._ASPM_SUPPORT_MASK`.
pub const ASPM_SUPPORT: u32 = 0xc00;

/// `PCIE_RC_PL_PHY_CTL_15`.
pub const RC_PL_PHY_CTL_15: usize = 0x184c;
/// `..._PM_CLK_PERIOD_MASK`.
pub const PM_CLK_PERIOD: u32 = 0xff;
/// El periodo del reloj de gestión de energía del 2712 son 18,52 ns, que
/// redondeado hacia abajo es `0x12`. Es el arreglo de la errata de L1SS.
pub const PM_CLK_PERIOD_2712: u32 = 0x12;

/// `PCIE_RC_DL_MDIO_ADDR`: el registro de direccion del bus MDIO del PHY del
/// puente. Ver [`mdio`].
pub const RC_DL_MDIO_ADDR: usize = 0x1100;
/// `PCIE_RC_DL_MDIO_WR_DATA`.
pub const RC_DL_MDIO_WR_DATA: usize = 0x1104;
/// `PCIE_RC_DL_MDIO_RD_DATA`.
pub const RC_DL_MDIO_RD_DATA: usize = 0x1108;

/// `PCIE_MISC_MISC_CTRL`.
pub const MISC_MISC_CTRL: usize = 0x4008;
/// `..._PCIE_RCB_MPS_MODE_MASK`.
pub const MISC_CTRL_RCB_MPS_MODE: u32 = 0x400;
/// `..._SCB_ACCESS_EN_MASK`.
pub const MISC_CTRL_SCB_ACCESS_EN: u32 = 0x1000;
/// `..._CFG_READ_UR_MODE_MASK`.
pub const MISC_CTRL_CFG_READ_UR_MODE: u32 = 0x2000;
/// `..._MAX_BURST_SIZE_MASK`.
pub const MISC_CTRL_MAX_BURST_SIZE: u32 = 0x0030_0000;

/// El tamaño de ráfaga del BCM2712.
///
/// La codificación de este campo **cambia con el chip**: en la variante
/// genérica `0=128, 1=256, 2=512`, y en el BCM7278 `1=128, 2=256, 3=512`. El
/// 2712 pide `1`, que en su codificación son **128 bytes** — el mismo tamaño
/// que el 2711 pide con un `0`. O sea: ni el valor ni el significado se pueden
/// copiar de otra variante.
pub const BURST_SIZE_2712: u32 = 0x1;

/// `PCIE_MISC_RC_BAR1_CONFIG_LO`: la ventana PCIe→GISB, que se apaga.
pub const MISC_RC_BAR1_CONFIG_LO: usize = 0x402c;
/// `PCIE_MISC_RC_BAR2_CONFIG_LO`: la ventana de entrada a la memoria del
/// sistema.
pub const MISC_RC_BAR2_CONFIG_LO: usize = 0x4034;
/// `PCIE_MISC_RC_BAR2_CONFIG_HI`.
pub const MISC_RC_BAR2_CONFIG_HI: usize = 0x4038;
/// `PCIE_MISC_RC_BAR3_CONFIG_LO`: la ventana PCIe→SCB, que se apaga.
pub const MISC_RC_BAR3_CONFIG_LO: usize = 0x403c;
/// `PCIE_MISC_RC_BAR_CONFIG_LO_SIZE_MASK`: el campo de tamaño que comparten
/// todos los `RC_BARn_CONFIG_LO`, y que a cero deja la ventana **apagada**.
pub const RC_BAR_CONFIG_LO_SIZE: u32 = 0x1f;

/// `PCIE_MISC_RC_CONFIG_RETRY_TIMEOUT`.
pub const MISC_RC_CONFIG_RETRY_TIMEOUT: usize = 0x405c;

/// `PCIE_MISC_PCIE_CTRL`.
pub const MISC_PCIE_CTRL: usize = 0x4064;
/// `..._PCIE_L23_REQUEST_MASK`.
pub const PCIE_CTRL_L23_REQUEST: u32 = 0x1;
/// `PCIE_MISC_PCIE_CTRL_PCIE_PERSTB_MASK`.
pub const PCIE_CTRL_PERSTB: u32 = 0x4;

/// `PCIE_MISC_PCIE_STATUS`.
pub const MISC_PCIE_STATUS: usize = 0x4068;
/// `..._PCIE_PORT_MASK_2712`. Ojo: es **el mismo bit** que
/// [`PCIE_STATUS_LINK_IN_L23`].
pub const PCIE_STATUS_PORT_2712: u32 = 0x40;
/// `..._PCIE_DL_ACTIVE_MASK`.
pub const PCIE_STATUS_DL_ACTIVE: u32 = 0x20;
/// `..._PCIE_PHYLINKUP_MASK`.
pub const PCIE_STATUS_PHYLINKUP: u32 = 0x10;
/// `..._PCIE_LINK_IN_L23_MASK`.
pub const PCIE_STATUS_LINK_IN_L23: u32 = 0x40;

/// `PCIE_MISC_REVISION`.
pub const MISC_REVISION: usize = 0x406c;

/// `PCIE_MISC_HARD_PCIE_HARD_DEBUG`, en la tabla del **2712**.
///
/// En la variante genérica este registro está en otro sitio; el 2712 lo trae a
/// `0x4304` por su tabla `pcie_offsets_bcm2712`. Un driver que use el offset
/// genérico aquí escribe en un registro que no es.
pub const MISC_HARD_DEBUG_2712: usize = 0x4304;
/// `..._SERDES_IDDQ_MASK`: el SerDes en reposo. Se **limpia** para encenderlo.
pub const HARD_DEBUG_SERDES_IDDQ: u32 = 0x0800_0000;
/// `..._CLKREQ_DEBUG_ENABLE_MASK`.
pub const HARD_DEBUG_CLKREQ_DEBUG_ENABLE: u32 = 0x2;
/// `..._PERST_ASSERT_MASK`.
pub const HARD_DEBUG_PERST_ASSERT: u32 = 0x8;
/// `..._CLKREQ_L1SS_ENABLE_MASK`.
pub const HARD_DEBUG_CLKREQ_L1SS_ENABLE: u32 = 0x0020_0000;

/// `PCIE_MISC_CTRL_1`.
pub const MISC_CTRL_1: usize = 0x40a0;
/// `..._EN_VDM_QOS_CONTROL_MASK`.
pub const MISC_CTRL_1_EN_VDM_QOS_CONTROL: u32 = 1 << 5;

/// `PCIE_MISC_UBUS_CTRL`.
pub const MISC_UBUS_CTRL: usize = 0x40a4;
/// `..._UBUS_PCIE_REPLY_ERR_DIS_MASK`.
pub const UBUS_CTRL_REPLY_ERR_DIS: u32 = 1 << 13;
/// `..._UBUS_PCIE_REPLY_DECERR_DIS_MASK`.
pub const UBUS_CTRL_REPLY_DECERR_DIS: u32 = 1 << 19;

/// `PCIE_MISC_UBUS_TIMEOUT`.
pub const MISC_UBUS_TIMEOUT: usize = 0x40a8;
/// `PCIE_MISC_UBUS_BAR2_CONFIG_REMAP`.
pub const MISC_UBUS_BAR2_CONFIG_REMAP: usize = 0x40b4;
/// `..._ACCESS_ENABLE_MASK`.
pub const UBUS_BAR_CONFIG_REMAP_ACCESS_ENABLE: u32 = 1 << 0;

/// `PCIE_MISC_AXI_INTF_CTRL`.
pub const MISC_AXI_INTF_CTRL: usize = 0x416c;
/// `AXI_EN_RCLK_QOS_ARRAY_FIX`.
pub const AXI_EN_RCLK_QOS_ARRAY_FIX: u32 = 1 << 13;
/// `AXI_EN_QOS_UPDATE_TIMING_FIX`.
pub const AXI_EN_QOS_UPDATE_TIMING_FIX: u32 = 1 << 12;
/// `AXI_DIS_QOS_GATING_IN_MASTER`.
pub const AXI_DIS_QOS_GATING_IN_MASTER: u32 = 1 << 11;
/// `AXI_REQFIFO_EN_QOS_PROPAGATION`.
pub const AXI_REQFIFO_EN_QOS_PROPAGATION: u32 = 1 << 7;
/// `AXI_MASTER_MAX_OUTSTANDING_REQUESTS_MASK`.
pub const AXI_MASTER_MAX_OUTSTANDING_REQUESTS: u32 = 0x3f;

/// `PCIE_MISC_AXI_READ_ERROR_DATA`.
pub const MISC_AXI_READ_ERROR_DATA: usize = 0x4170;

/// `PCIE_EXT_CFG_INDEX`: dónde se deja el `bus/devfn` del dispositivo cuyo
/// espacio de configuración se quiere leer.
pub const EXT_CFG_INDEX: usize = 0x9000;

/// `PCIE_EXT_CFG_DATA`: la ventana por la que se ve ese espacio de
/// configuración.
///
/// **La tabla de offsets del 2712 dice `0x9004` y aun así el valor correcto
/// aquí es `0x8000`.** `pcie_offsets_bcm2712[EXT_CFG_DATA] = 0x9004` sólo lo
/// consume `brcm7425_pcie_map_bus`, que es la ruta del BCM7425; el 2712 usa
/// `brcm_pcie_map_bus`, y ésa escribe los literales `PCIE_EXT_CFG_INDEX`
/// (0x9000) y `PCIE_EXT_CFG_DATA` (0x8000) sin pasar por la tabla. Es decir:
/// la entrada de la tabla está muerta en este chip, y quien la copie se lleva
/// la ventana equivocada.
pub const EXT_CFG_DATA: usize = 0x8000;

/// `BRCM_NUM_PCIE_OUT_WINS`: cuántas ventanas de salida tiene el puente.
pub const NUM_OUT_WINS: usize = 4;

/// `PCIE_MEM_WIN0_LO(win)`: la base, en direcciones de **PCIe**, de la ventana
/// de salida `win`.
pub const fn mem_win_lo(win: usize) -> usize {
    0x400c + win * 8
}
/// `PCIE_MEM_WIN0_HI(win)`.
pub const fn mem_win_hi(win: usize) -> usize {
    0x4010 + win * 8
}
/// `PCIE_MEM_WIN0_BASE_LIMIT(win)`: base y límite, en direcciones de **CPU**,
/// empaquetados en un solo registro **y en unidades de MiB**.
pub const fn mem_win_base_limit(win: usize) -> usize {
    0x4070 + win * 4
}
/// `PCIE_MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT_BASE_MASK`.
pub const MEM_WIN_BASE: u32 = 0xfff0;
/// `PCIE_MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT_LIMIT_MASK`.
pub const MEM_WIN_LIMIT: u32 = 0xfff0_0000;
/// `PCIE_MEM_WIN0_BASE_HI(win)`.
pub const fn mem_win_base_hi(win: usize) -> usize {
    0x4080 + win * 8
}
/// `PCIE_MEM_WIN0_LIMIT_HI(win)`. Se intercala con [`mem_win_base_hi`]: los
/// dos van de ocho en ocho, uno en `0x4080` y el otro en `0x4084`.
pub const fn mem_win_limit_hi(win: usize) -> usize {
    0x4084 + win * 8
}
/// `..._BASE_HI_BASE_MASK` y `..._LIMIT_HI_LIMIT_MASK`, que son el mismo campo.
pub const MEM_WIN_HI_ADDR: u32 = 0xff;

/// Cuánto se desplaza la parte alta de una dirección de ventana de salida.
///
/// Linux lo calcula como `HWEIGHT32(BASE_MASK)`, el **número de bits puestos**
/// en la máscara de la parte baja, no la posición del campo. Con
/// `BASE_MASK = 0xfff0` son doce bits, así que el desplazamiento es **12**, que
/// no es ni dónde empieza el campo (4) ni dónde acaba (16). Y lo que se
/// desplaza son los MiB ya divididos, no la dirección.
pub const MEM_WIN_HI_SHIFT: u32 = MEM_WIN_BASE.count_ones();

// --- el espacio de configuración (ECAM), contra `include/linux/pci-ecam.h` --

/// `PCIE_ECAM_BUS_SHIFT`.
pub const ECAM_BUS_SHIFT: u32 = 20;
/// `PCIE_ECAM_DEVFN_SHIFT`.
pub const ECAM_DEVFN_SHIFT: u32 = 12;
/// `PCIE_ECAM_REG_MASK`: el offset dentro del espacio de configuración se
/// limita a 4 KiB.
pub const ECAM_REG_MASK: u32 = 0xfff;

/// `PCIE_ECAM_OFFSET(bus, devfn, where)`.
pub const fn ecam_offset(bus: u8, devfn: u8, reg: u32) -> u32 {
    ((bus as u32) << ECAM_BUS_SHIFT) | ((devfn as u32) << ECAM_DEVFN_SHIFT) | (reg & ECAM_REG_MASK)
}

/// A dónde hay que ir para leer un registro de configuración.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfgAccess {
    /// El puente mismo: sus registros están en la ventana, sin índice de por
    /// medio.
    Direct {
        /// Offset dentro de la ventana de registros del puente.
        off: usize,
    },
    /// Un dispositivo de detrás: primero se escribe el índice y luego se lee
    /// por la ventana de datos.
    Indexed {
        /// Lo que va a [`EXT_CFG_INDEX`].
        index: u32,
        /// Offset dentro de la ventana de datos.
        off: usize,
    },
}

// --- el tamaño de una ventana de entrada -----------------------------------

/// `brcm_pcie_encode_ibar_size`: el tamaño de una ventana de entrada, en la
/// codificación **no lineal** de `RC_BARn_CONFIG_LO.SIZE`.
///
/// Son dos tramos que no se tocan y un agujero entre ellos:
///
/// - de 4 KiB a 32 KiB: `log2 - 12 + 0x1c`, o sea `0x1c..=0x1f`;
/// - de 64 KiB a 64 GiB: `log2 - 15`, o sea `1..=21`;
/// - cualquier otra cosa: **0**, que no es «cero bytes» sino *ventana
///   apagada*.
///
/// Un tamaño que no sea potencia de dos se trunca al `log2` de abajo, igual que
/// `ilog2` en Linux, porque la ventana sólo puede valer una potencia de dos.
pub fn encode_ibar_size(size: u64) -> u32 {
    if size == 0 {
        return 0;
    }
    let log2_in = size.ilog2();
    if (12..=15).contains(&log2_in) {
        (log2_in - 12) + 0x1c
    } else if (16..=36).contains(&log2_in) {
        log2_in - 15
    } else {
        0
    }
}

// --- el puente ---------------------------------------------------------------

/// El puente PCIe del BCM2712, visto por su ventana de registros.
#[derive(Debug)]
pub struct BrcmPcie {
    base: usize,
}

impl BrcmPcie {
    /// Toma la ventana de registros ya mapeada. `base` es la dirección virtual
    /// del `reg` del nodo, que en la Pi 5 son `0x9310` bytes a partir de
    /// `0x10_00120000` físicos.
    ///
    /// # Safety
    /// `base` tiene que apuntar a la ventana de registros del puente, mapeada
    /// como dispositivo y de al menos `0x9310` bytes.
    pub const unsafe fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, off: usize) -> u32 {
        let val = unsafe { read_volatile((self.base + off) as *const u32) };
        #[cfg(test)]
        let val = bridge_fake::al_leer(off, val);
        val
    }

    fn write(&self, off: usize, val: u32) {
        #[cfg(test)]
        bridge_fake::apunta(bridge_fake::PUENTE, off, val);
        unsafe { write_volatile((self.base + off) as *mut u32, val) }
    }

    /// Los registros de dentro de la capacidad PCIe son de 16 bits, y la
    /// anchura es parte del acceso: una escritura de 32 se llevaria por delante
    /// el registro de al lado, que en este bloque es otro registro de verdad.
    fn read16(&self, off: usize) -> u16 {
        unsafe { read_volatile((self.base + off) as *const u16) }
    }

    fn write16(&self, off: usize, val: u16) {
        #[cfg(test)]
        bridge_fake::apunta(bridge_fake::PUENTE, off, u32::from(val));
        unsafe { write_volatile((self.base + off) as *mut u16, val) }
    }

    /// Cambia solo los bits de `mask` de un registro de 16.
    fn modify16(&self, off: usize, mask: u16, val: u16) {
        let shift = mask.trailing_zeros();
        let tmp = (self.read16(off) & !mask) | ((val << shift) & mask);
        self.write16(off, tmp);
    }

    /// Cambia sólo los bits de `mask`, dejando el resto como estaban.
    fn modify(&self, off: usize, mask: u32, val: u32) {
        let shift = mask.trailing_zeros();
        let tmp = (self.read(off) & !mask) | ((val << shift) & mask);
        self.write(off, tmp);
    }

    /// `brcm_pcie_link_up`: el enlace está arriba cuando el nivel de enlace
    /// está activo **y** la capa física ha subido. Los dos, no uno.
    pub fn link_up(&self) -> bool {
        let val = self.read(MISC_PCIE_STATUS);
        (val & PCIE_STATUS_DL_ACTIVE != 0) && (val & PCIE_STATUS_PHYLINKUP != 0)
    }

    /// `brcm_pcie_rc_mode_2712`: si el núcleo está de raíz y no de *endpoint*.
    ///
    /// En el 2712 esto **siempre es cierto**, y a propósito. El bit que lee
    /// Linux (`0x40`) es el mismo que `PCIE_LINK_IN_L23`, así que su valor no
    /// dice nada del papel del núcleo, y el propio Linux lo remata con `| 1`
    /// sobre un `//XXX`. Se deja aquí con la forma de Linux para que el sitio
    /// quede señalado, pero **nada debe condicionarse a esta respuesta**: en el
    /// 2712 no hay forma de distinguir los dos papeles por este registro.
    pub fn rc_mode(&self) -> bool {
        let _ = self.read(MISC_PCIE_STATUS) & PCIE_STATUS_PORT_2712;
        true
    }

    /// `brcm_pcie_perst_set_2712`: suelta o aprieta PERST#.
    ///
    /// El bit se llama **PERST*B***, con la barra: está a uno cuando el reset
    /// está *suelto*. Así que `assert = true` escribe un **cero**. El driver
    /// genérico escribe el valor tal cual en otro registro distinto; copiarlo
    /// aquí deja la tarjeta en reset justo cuando se la quería soltar.
    pub fn perst_set(&self, assert: bool) {
        self.modify(MISC_PCIE_CTRL, PCIE_CTRL_PERSTB, u32::from(!assert));
    }

    /// `brcm_pcie_set_outbound_win`: abre la ventana de salida `win`.
    ///
    /// `cpu_addr` y `pcie_addr` son las dos caras de la misma ventana, y pueden
    /// no coincidir: lo que la CPU ve en `cpu_addr` sale al bus PCIe como
    /// `pcie_addr`. La base y el límite del lado de la CPU van en **MiB**, y el
    /// límite es la **última** dirección dentro de la ventana (`cpu + size - 1`),
    /// no la primera de fuera; por eso una ventana más pequeña que 1 MiB acaba
    /// con base y límite iguales, que es lo correcto: cubre ese MiB.
    ///
    /// Devuelve `false` y no toca nada si `win` no existe o la ventana está
    /// vacía.
    pub fn set_outbound_win(&self, win: usize, cpu_addr: u64, pcie_addr: u64, size: u64) -> bool {
        if win >= NUM_OUT_WINS {
            warn!(
                "{}: la ventana de salida {} no existe (hay {})",
                MODULE, win, NUM_OUT_WINS
            );
            return false;
        }
        if size == 0 {
            warn!("{}: ventana de salida {} de tamaño cero", MODULE, win);
            return false;
        }

        // La base del lado de PCIe, entera.
        self.write(mem_win_lo(win), pcie_addr as u32);
        self.write(mem_win_hi(win), (pcie_addr >> 32) as u32);

        const MIB: u64 = 1 << 20;
        let cpu_mb = cpu_addr / MIB;
        let limit_mb = (cpu_addr + size - 1) / MIB;

        self.modify(mem_win_base_limit(win), MEM_WIN_BASE, cpu_mb as u32);
        self.modify(mem_win_base_limit(win), MEM_WIN_LIMIT, limit_mb as u32);

        self.modify(
            mem_win_base_hi(win),
            MEM_WIN_HI_ADDR,
            (cpu_mb >> MEM_WIN_HI_SHIFT) as u32,
        );
        self.modify(
            mem_win_limit_hi(win),
            MEM_WIN_HI_ADDR,
            (limit_mb >> MEM_WIN_HI_SHIFT) as u32,
        );
        true
    }

    /// `brcm_pcie_map_bus`: por dónde se llega al registro `reg` del `devfn`
    /// del bus `bus`.
    ///
    /// Tres reglas, y las tres importan:
    ///
    /// - el puente mismo (bus raíz, `devfn == 0`) se lee **directo**, sin
    ///   índice;
    /// - cualquier otra función del bus raíz **no existe**: el puente es la
    ///   única;
    /// - un dispositivo de detrás necesita el enlace arriba. Un acceso a
    ///   configuración sin enlace **aborta la CPU**, no devuelve unos, así que
    ///   esto no se puede intentar «por ver».
    ///
    /// El índice se compone con `where = 0`: el offset dentro de los 4 KiB se
    /// aplica a la **ventana de datos**, no al índice.
    pub fn cfg_access(&self, bus: u8, devfn: u8, reg: u32, root_bus: u8) -> Option<CfgAccess> {
        if bus == root_bus {
            if devfn != 0 {
                return None;
            }
            return Some(CfgAccess::Direct {
                off: (reg & ECAM_REG_MASK) as usize,
            });
        }
        if !self.link_up() {
            return None;
        }
        Some(CfgAccess::Indexed {
            index: ecam_offset(bus, devfn, 0),
            off: EXT_CFG_DATA + (reg & ECAM_REG_MASK) as usize,
        })
    }
}

pub mod bringup;
pub mod mdio;
pub mod reset;

/// El orden en que se tocan los registros, que es la mitad de lo que hay que
/// comprobar en una secuencia de arranque y lo unico que un banco de registros
/// no recuerda.
///
/// La traza es **una sola para los cuatro bloques** (el puente, el controlador
/// de resets, el recalibrado y el MDIO del PHY), porque lo que hay que
/// comprobar es justo el orden ENTRE ellos: que el puente sale de reset antes
/// de que se toque el SerDes, que el PLL se reprograma con el SerDes ya
/// encendido.
#[cfg(test)]
pub(super) mod bridge_fake {
    use core::cell::RefCell;

    /// De que bloque es una entrada de la traza.
    pub const PUENTE: &str = "puente";
    /// El controlador de resets de estilo SW_INIT.
    pub const RESET: &str = "reset";
    /// El recalibrado del PHY.
    pub const RESCAL: &str = "rescal";

    /// Una entrada: de que bloque, en que offset y con que valor.
    pub type Entrada = (&'static str, usize, u32);

    std::thread_local! {
        /// Cada escritura, en orden.
        pub static TRAZA: RefCell<alloc::vec::Vec<Entrada>> =
            const { RefCell::new(alloc::vec::Vec::new()) };

        /// Bits del puente que **leen cero** aunque se les escriba un uno, que
        /// es como se porta un bit reservado en una revision del chip. Sin
        /// esto no hay forma de recorrer el camino del 2712C1.
        pub static RESERVADOS: RefCell<alloc::vec::Vec<(usize, u32)>> =
            const { RefCell::new(alloc::vec::Vec::new()) };
    }

    pub fn apunta(fuente: &'static str, off: usize, val: u32) {
        TRAZA.with(|t| t.borrow_mut().push((fuente, off, val)));
    }

    /// Lo que de verdad se lee del puente, con los bits reservados a cero.
    pub fn al_leer(off: usize, val: u32) -> u32 {
        RESERVADOS.with(|r| {
            let mut out = val;
            for (o, mask) in r.borrow().iter() {
                if *o == off {
                    out &= !*mask;
                }
            }
            out
        })
    }

    /// Declara que `mask` del registro `off` es un bit reservado en este chip.
    pub fn reservar(off: usize, mask: u32) {
        RESERVADOS.with(|r| r.borrow_mut().push((off, mask)));
    }

    /// Corre `f` con la traza y la lista de reservados limpias, y devuelve lo
    /// que se haya apuntado.
    pub fn trazando<T>(f: impl FnOnce() -> T) -> (T, alloc::vec::Vec<Entrada>) {
        TRAZA.with(|t| t.borrow_mut().clear());
        RESERVADOS.with(|r| r.borrow_mut().clear());
        let r = f();
        (r, TRAZA.with(|t| core::mem::take(&mut *t.borrow_mut())))
    }

    /// En que posicion de la traza se escribio por primera vez `off` en
    /// `fuente`, si se escribio.
    pub fn primera(traza: &[Entrada], fuente: &str, off: usize) -> Option<usize> {
        traza.iter().position(|(f, o, _)| *f == fuente && *o == off)
    }
}

#[cfg(test)]
mod brcmstb_tests;
