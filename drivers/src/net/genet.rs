//! El GENET v5 de Broadcom, el Ethernet gigabit integrado en el BCM2711 de la
//! Raspberry Pi 4.
//!
//! Esta tanda es la capa de abajo: la ventana de registros, el reset del UMAC,
//! la direccion MAC y el bus MDIO con el que se habla al PHY externo (un
//! BCM54213PE por RGMII). Los anillos DMA, el `NetScheme` y la sonda del
//! device tree van en tandas siguientes.
//!
//! ## De donde sale el mapa de registros
//!
//! Del BCM2711 **no hay hoja de datos publica** del GENET. Los offsets y los
//! bits de aqui son los que publican los dos drivers libres que ya hablan con
//! esta pieza -- `drivers/net/ethernet/broadcom/genet/bcmgenet.c` de Linux y
//! `drivers/net/bcmgenet.c` de U-Boot -- y son hechos sobre el silicio, no
//! codigo portado: la secuencia de arranque, los plazos y las decisiones de
//! este fichero estan escritos aqui y en dos sitios se separan a proposito de
//! lo que hace U-Boot (ver [`GenetMac::adjust_link`]).
//!
//! ## Por que compila en el anfitrion
//!
//! El driver no lleva ni una instruccion de ARM: todo lo que hace es leer y
//! escribir una ventana de registros. Asi que el `cfg` que lo encierra abre
//! tambien el build de test ([`super`] lo monta con `any(target_arch =
//! "aarch64", test)`), y bajo `cargo test` esa ventana es el MAC falso de
//! [`genet_fake`], un `Vec<u32>` del monton. Lo unico que un modelo en memoria
//! no da es la *reaccion* a una escritura -- un `MDIO_CMD` al que nadie baja
//! `START_BUSY` es un acceso al PHY que no termina nunca -- y eso es la unica
//! costura `cfg(test)` del fichero, en [`GenetMac::write`].

use super::mii::*;
use super::timer_now_as_micros;
use crate::{DeviceError, DeviceResult};
use core::ptr::{read_volatile, write_volatile};

// ---------------------------------------------------------------------------
// Mapa de registros (offsets absolutos desde la base del MAC)
// ---------------------------------------------------------------------------

/// Bloque SYS, en la base de la ventana.
const GENET_SYS_OFF: usize = 0x0000;
/// Revision del silicio: el nibble alto de `>> 24` dice la generacion.
pub const SYS_REV_CTRL: usize = GENET_SYS_OFF;
/// Modo del puerto: a que PHY esta cableado el MAC.
pub const SYS_PORT_CTRL: usize = GENET_SYS_OFF + 0x04;
pub const SYS_RBUF_FLUSH_CTRL: usize = GENET_SYS_OFF + 0x08;
pub const SYS_TBUF_FLUSH_CTRL: usize = GENET_SYS_OFF + 0x0C;

/// PHY externo con gigabit, que es como esta cableada la Raspberry Pi 4.
pub const PORT_MODE_EXT_GPHY: u32 = 3;

/// Bloque EXT.
const GENET_EXT_OFF: usize = 0x0080;
/// Control del "out of band" del RGMII: de aqui se le dice al MAC que el
/// enlace esta arriba cuando el PHY no se lo cuenta por el cable.
pub const EXT_RGMII_OOB_CTRL: usize = GENET_EXT_OFF + 0x0C;
pub const RGMII_LINK: u32 = 1 << 4;
pub const OOB_DISABLE: u32 = 1 << 5;
pub const RGMII_MODE_EN: u32 = 1 << 6;
/// Desactiva el retardo interno del RGMII: lo pone la placa, no el MAC.
pub const ID_MODE_DIS: u32 = 1 << 16;

/// Bloque RBUF (el buffer de recepcion).
const GENET_RBUF_OFF: usize = 0x0300;
pub const RBUF_CTRL: usize = GENET_RBUF_OFF;
/// Alinea la carga a 2 bytes, para que la cabecera IP caiga alineada a 4.
pub const RBUF_ALIGN_2B: u32 = 1 << 1;
pub const RBUF_TBUF_SIZE_CTRL: usize = GENET_RBUF_OFF + 0xB4;

/// Bloque UMAC (el MAC propiamente dicho).
const GENET_UMAC_OFF: usize = 0x0800;
pub const UMAC_CMD: usize = GENET_UMAC_OFF + 0x008;
/// Los cuatro bytes altos de la MAC, big endian.
pub const UMAC_MAC0: usize = GENET_UMAC_OFF + 0x00C;
/// Los dos bytes bajos de la MAC.
pub const UMAC_MAC1: usize = GENET_UMAC_OFF + 0x010;
pub const UMAC_MAX_FRAME_LEN: usize = GENET_UMAC_OFF + 0x014;
pub const UMAC_TX_FLUSH: usize = GENET_UMAC_OFF + 0x334;
pub const UMAC_MIB_CTRL: usize = GENET_UMAC_OFF + 0x580;
pub const UMAC_MDIO_CMD: usize = GENET_UMAC_OFF + 0x614;

// Bits de `UMAC_CMD`.
pub const CMD_TX_EN: u32 = 1 << 0;
pub const CMD_RX_EN: u32 = 1 << 1;
pub const CMD_SPEED_SHIFT: u32 = 2;
pub const CMD_SPEED_MASK: u32 = 3;
pub const CMD_PROMISC: u32 = 1 << 4;
/// Half duplex. Es un bit *positivo*: a cero el MAC va a full duplex.
pub const CMD_HD_EN: u32 = 1 << 10;
pub const CMD_SW_RESET: u32 = 1 << 13;
pub const CMD_LCL_LOOP_EN: u32 = 1 << 15;

/// Codificacion de la velocidad en `UMAC_CMD`.
pub const UMAC_SPEED_10: u32 = 0;
pub const UMAC_SPEED_100: u32 = 1;
pub const UMAC_SPEED_1000: u32 = 2;
pub const UMAC_SPEED_2500: u32 = 3;

// Bits de `UMAC_MIB_CTRL`: reset de los contadores.
pub const MIB_RESET_RX: u32 = 1 << 0;
pub const MIB_RESET_RUNT: u32 = 1 << 1;
pub const MIB_RESET_TX: u32 = 1 << 2;

// Bits de `UMAC_MDIO_CMD`.
/// El driver lo pone a uno para arrancar la transaccion; el MAC lo baja al
/// terminarla.
pub const MDIO_START_BUSY: u32 = 1 << 29;
/// El PHY no contesto (o contesto con error).
pub const MDIO_READ_FAIL: u32 = 1 << 28;
pub const MDIO_RD: u32 = 2 << 26;
pub const MDIO_WR: u32 = 1 << 26;
pub const MDIO_PMD_SHIFT: u32 = 21;
pub const MDIO_PMD_MASK: u32 = 0x1F;
pub const MDIO_REG_SHIFT: u32 = 16;
pub const MDIO_REG_MASK: u32 = 0x1F;

/// Trama maxima que el UMAC acepta: 1500 de carga + 14 de cabecera Ethernet
/// + 4 de etiqueta VLAN + 6 de etiqueta Broadcom + 4 de FCS + 8 de relleno,
/// que da 1536 y es multiplo de 256.
pub const ENET_MAX_MTU_SIZE: u32 = 1500 + 14 + 4 + 6 + 4 + 8;
const _: () = assert!(ENET_MAX_MTU_SIZE == 1536);

/// La generacion del GENET que este driver sabe manejar.
pub const GENET_VERSION_SUPPORTED: u8 = 5;

/// Plazo del bus MDIO, en microsegundos. El mismo que usa U-Boot: una
/// transaccion MDIO a 2,5 MHz son decenas de microsegundos, asi que 20 ms es
/// un PHY que no esta.
const MDIO_TIMEOUT_US: u64 = 20_000;

// ---------------------------------------------------------------------------
// Velocidad y duplex
// ---------------------------------------------------------------------------

/// La velocidad negociada, tal y como la entiende `UMAC_CMD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSpeed {
    Mbps10,
    Mbps100,
    Mbps1000,
}

impl LinkSpeed {
    /// El valor que va al campo `SPEED` de `UMAC_CMD`.
    pub fn umac_code(self) -> u32 {
        match self {
            LinkSpeed::Mbps10 => UMAC_SPEED_10,
            LinkSpeed::Mbps100 => UMAC_SPEED_100,
            LinkSpeed::Mbps1000 => UMAC_SPEED_1000,
        }
    }

    /// Megabits por segundo, para los logs y para `ethtool`.
    pub fn mbps(self) -> u32 {
        match self {
            LinkSpeed::Mbps10 => 10,
            LinkSpeed::Mbps100 => 100,
            LinkSpeed::Mbps1000 => 1000,
        }
    }
}

/// El estado del enlace leido del PHY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    /// Hay portadora y la autonegociacion termino.
    pub up: bool,
    pub speed: LinkSpeed,
    pub full_duplex: bool,
}

impl Link {
    /// El enlace caido. La velocidad no significa nada con `up` a false, pero
    /// un `Option` aqui obligaria a cada llamante a desenvolverlo para nada.
    pub const DOWN: Link = Link {
        up: false,
        speed: LinkSpeed::Mbps10,
        full_duplex: false,
    };
}

// ---------------------------------------------------------------------------
// La ventana de registros
// ---------------------------------------------------------------------------

/// El MAC del GENET: nada mas que la base de su ventana de registros.
///
/// La base es **virtual**: la trae ya mapeada quien sondea el device tree, que
/// es el unico que sabe traducir una direccion fisica en esta maquina. Bajo
/// test es la direccion del `Vec` del MAC falso.
pub struct GenetMac {
    base: usize,
}

impl GenetMac {
    /// # Safety
    ///
    /// `base` tiene que ser una ventana de al menos 0x10000 bytes mapeada sin
    /// cache sobre los registros del GENET, y nadie mas puede estar
    /// escribiendola.
    pub unsafe fn new(base: usize) -> Self {
        Self { base }
    }

    /// La base de la ventana, para el que tenga que mapear los anillos.
    pub fn base(&self) -> usize {
        self.base
    }

    #[inline]
    fn read(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    #[inline]
    fn write(&self, off: usize, val: u32) {
        unsafe { write_volatile((self.base + off) as *mut u32, val) };
        // El registro se escribe ANTES de que el MAC lo vea, que es el orden
        // que tiene el hardware de verdad. Bajo test el que lo ve es el MAC
        // falso, y esa reaccion es la unica costura que los tests necesitan.
        #[cfg(test)]
        genet_fake::on_write(self.base, off, val);
    }

    /// Lee, limpia unos bits, pone otros y vuelve a escribir.
    ///
    /// Es read-modify-write a proposito: ver [`Self::adjust_link`].
    fn modify(&self, off: usize, clear: u32, set: u32) {
        let v = self.read(off);
        self.write(off, (v & !clear) | set);
    }

    // -----------------------------------------------------------------------
    // Identificacion
    // -----------------------------------------------------------------------

    /// La generacion del GENET, ya traducida.
    ///
    /// El campo del registro **no es** el numero de generacion: el silicio
    /// numera 0, 5 y 6 lo que el driver llama v1, v4 y v5. La traduccion es la
    /// de Linux, y sin ella un BCM2711 (que pone un 6) se leeria como una
    /// generacion que no existe.
    pub fn hw_version(&self) -> u8 {
        let major = ((self.read(SYS_REV_CTRL) >> 24) & 0x0F) as u8;
        match major {
            6 => 5,
            5 => 4,
            0 => 1,
            other => other,
        }
    }

    /// `Ok` solo en el GENET v5, que es el de la Raspberry Pi 4.
    ///
    /// Las generaciones anteriores tienen los bloques en otros offsets y el
    /// `RBUF_TBUF_SIZE_CTRL` en otro bloque, asi que seguir adelante con ellas
    /// seria escribir registros al azar.
    pub fn check_supported(&self) -> DeviceResult {
        let v = self.hw_version();
        if v == GENET_VERSION_SUPPORTED {
            Ok(())
        } else {
            Err(DeviceError::NotSupported)
        }
    }

    // -----------------------------------------------------------------------
    // Arranque del UMAC
    // -----------------------------------------------------------------------

    /// Cablea el MAC a un PHY externo de gigabit, que es el RGMII de la Pi 4.
    pub fn set_port_mode_ext_gphy(&self) {
        self.write(SYS_PORT_CTRL, PORT_MODE_EXT_GPHY);
    }

    /// El reset del UMAC, en el orden en el que el silicio lo acepta.
    ///
    /// Los tres pasos del `SYS_RBUF_FLUSH_CTRL` (subir el bit 1, bajarlo,
    /// dejar el registro a cero) no son redundantes: el primero vacia el
    /// buffer de recepcion y el ultimo suelta el reset del propio RBUF. Y el
    /// `CMD_LCL_LOOP_EN` que acompaña al `CMD_SW_RESET` deja el MAC en bucle
    /// local mientras se resetea, para que no escupa nada al cable.
    pub fn reset_umac(&self) {
        let v = self.read(SYS_RBUF_FLUSH_CTRL);
        self.write(SYS_RBUF_FLUSH_CTRL, v | (1 << 1));
        udelay(10);
        self.write(SYS_RBUF_FLUSH_CTRL, v & !(1 << 1));
        udelay(10);
        self.write(SYS_RBUF_FLUSH_CTRL, 0);
        udelay(10);

        self.write(UMAC_CMD, 0);
        self.write(UMAC_CMD, CMD_SW_RESET | CMD_LCL_LOOP_EN);
        udelay(2);
        self.write(UMAC_CMD, 0);

        // Los contadores MIB arrancan con basura del arranque anterior.
        self.write(UMAC_MIB_CTRL, MIB_RESET_RX | MIB_RESET_TX | MIB_RESET_RUNT);
        self.write(UMAC_MIB_CTRL, 0);

        self.write(UMAC_MAX_FRAME_LEN, ENET_MAX_MTU_SIZE);

        self.modify(RBUF_CTRL, 0, RBUF_ALIGN_2B);
        self.write(RBUF_TBUF_SIZE_CTRL, 1);
    }

    /// Vacia la cola de transmision del UMAC.
    pub fn flush_tx(&self) {
        self.write(UMAC_TX_FLUSH, 1);
        udelay(10);
        self.write(UMAC_TX_FLUSH, 0);
    }

    // -----------------------------------------------------------------------
    // Direccion MAC
    // -----------------------------------------------------------------------

    /// Escribe la MAC en `UMAC_MAC0`/`UMAC_MAC1`.
    ///
    /// Big endian en los dos: el primer byte del cable es el mas
    /// significativo. En la Raspberry Pi la MAC real la deja la firmware en el
    /// nodo del device tree, no en una EEPROM del MAC.
    pub fn write_mac_addr(&self, mac: &[u8; 6]) {
        self.write(
            UMAC_MAC0,
            (mac[0] as u32) << 24 | (mac[1] as u32) << 16 | (mac[2] as u32) << 8 | mac[3] as u32,
        );
        self.write(UMAC_MAC1, (mac[4] as u32) << 8 | mac[5] as u32);
    }

    /// Relee la MAC de los registros.
    pub fn read_mac_addr(&self) -> [u8; 6] {
        let hi = self.read(UMAC_MAC0);
        let lo = self.read(UMAC_MAC1);
        [
            (hi >> 24) as u8,
            (hi >> 16) as u8,
            (hi >> 8) as u8,
            hi as u8,
            (lo >> 8) as u8,
            lo as u8,
        ]
    }

    // -----------------------------------------------------------------------
    // El bus MDIO
    // -----------------------------------------------------------------------

    /// Arranca la transaccion que ya esta cargada en `UMAC_MDIO_CMD` y espera
    /// a que el MAC baje `START_BUSY`.
    ///
    /// El `START_BUSY` se sube con read-modify-write y no reescribiendo el
    /// registro: la direccion y el registro del PHY ya estan ahi, y un segundo
    /// `write` completo tendria que repetirlos.
    fn mdio_start_and_wait(&self) -> DeviceResult<u32> {
        self.modify(UMAC_MDIO_CMD, 0, MDIO_START_BUSY);
        let t0 = timer_now_as_micros();
        loop {
            let cmd = self.read(UMAC_MDIO_CMD);
            if cmd & MDIO_START_BUSY == 0 {
                return Ok(cmd);
            }
            if timer_now_as_micros().wrapping_sub(t0) >= MDIO_TIMEOUT_US {
                return Err(DeviceError::NotReady);
            }
            core::hint::spin_loop();
        }
    }

    /// Lee un registro del PHY `pmd` por MDIO.
    ///
    /// `NotReady` es un MAC que no termina la transaccion; `IoError` es un PHY
    /// que no contesta (`MDIO_READ_FAIL`), que es lo que se ve cuando la
    /// direccion MDIO del device tree no es la del PHY de la placa.
    pub fn mdio_read(&self, pmd: u8, reg: u32) -> DeviceResult<u16> {
        let cmd = MDIO_RD
            | ((pmd as u32 & MDIO_PMD_MASK) << MDIO_PMD_SHIFT)
            | ((reg & MDIO_REG_MASK) << MDIO_REG_SHIFT);
        self.write(UMAC_MDIO_CMD, cmd);
        let done = self.mdio_start_and_wait()?;
        if done & MDIO_READ_FAIL != 0 {
            return Err(DeviceError::IoError);
        }
        Ok(done as u16)
    }

    /// Escribe un registro del PHY `pmd` por MDIO.
    pub fn mdio_write(&self, pmd: u8, reg: u32, val: u16) -> DeviceResult {
        let cmd = MDIO_WR
            | ((pmd as u32 & MDIO_PMD_MASK) << MDIO_PMD_SHIFT)
            | ((reg & MDIO_REG_MASK) << MDIO_REG_SHIFT)
            | val as u32;
        self.write(UMAC_MDIO_CMD, cmd);
        self.mdio_start_and_wait()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // El PHY
    // -----------------------------------------------------------------------

    /// El identificador de 32 bits del PHY (`PHYSID1` arriba, `PHYSID2`
    /// abajo).
    pub fn phy_id(&self, pmd: u8) -> DeviceResult<u32> {
        let hi = self.mdio_read(pmd, MII_PHYSID1)? as u32;
        let lo = self.mdio_read(pmd, MII_PHYSID2)? as u32;
        Ok(hi << 16 | lo)
    }

    /// Busca el PHY recorriendo las 32 direcciones del bus.
    ///
    /// La direccion la trae el device tree, y esta es la red de seguridad para
    /// cuando no la trae: un PHY ausente contesta 0x0000 o 0xffff en los dos
    /// registros de identificacion, asi que cualquier otra cosa es un PHY.
    pub fn phy_scan(&self) -> DeviceResult<u8> {
        for pmd in 0..32u8 {
            match self.phy_id(pmd) {
                Ok(id) if phy_id_is_present(id) => return Ok(pmd),
                _ => continue,
            }
        }
        Err(DeviceError::NotReady)
    }

    /// Reinicia la autonegociacion del PHY.
    pub fn restart_aneg(&self, pmd: u8) -> DeviceResult {
        let bmcr = self.mdio_read(pmd, MII_BMCR)?;
        self.mdio_write(
            pmd,
            MII_BMCR,
            bmcr | (BMCR_ANENABLE | BMCR_ANRESTART) as u16,
        )
    }

    /// Lee del PHY el estado del enlace, como `genphy_read_status`.
    ///
    /// Sin portadora (`BMSR_LSTATUS`) o sin autonegociacion terminada
    /// (`BMSR_ANEGCOMPLETE`) el enlace esta caido y la velocidad no se mira:
    /// lo que diga `STAT1000`/`LPA` mientras negocia es de la vuelta anterior.
    ///
    /// `BMSR` latcha `LSTATUS`: una lectura devuelve el estado desde la
    /// lectura anterior, asi que se lee **dos veces** y vale la segunda. Sin
    /// eso, un enlace que se cayo y volvio se leeria caido.
    pub fn read_link(&self, pmd: u8) -> DeviceResult<Link> {
        let _ = self.mdio_read(pmd, MII_BMSR)?;
        let bmsr = self.mdio_read(pmd, MII_BMSR)? as u32;
        if bmsr & BMSR_LSTATUS == 0 || bmsr & BMSR_ANEGCOMPLETE == 0 {
            return Ok(Link::DOWN);
        }

        // Lo negociado es la interseccion de lo que anuncia cada lado.
        let ctrl1000 = self.mdio_read(pmd, MII_CTRL1000)? as u32;
        let stat1000 = self.mdio_read(pmd, MII_STAT1000)? as u32;
        let advertise = self.mdio_read(pmd, MII_ADVERTISE)? as u32;
        let lpa = self.mdio_read(pmd, MII_LPA)? as u32;

        let (speed, full_duplex) =
            if ctrl1000 & ADVERTISE_1000FULL != 0 && stat1000 & LPA_1000FULL != 0 {
                (LinkSpeed::Mbps1000, true)
            } else if ctrl1000 & ADVERTISE_1000HALF != 0 && stat1000 & LPA_1000HALF != 0 {
                (LinkSpeed::Mbps1000, false)
            } else if advertise & ADVERTISE_100FULL != 0 && lpa & LPA_100FULL != 0 {
                (LinkSpeed::Mbps100, true)
            } else if advertise & ADVERTISE_100HALF != 0 && lpa & LPA_100HALF != 0 {
                (LinkSpeed::Mbps100, false)
            } else if advertise & ADVERTISE_10FULL != 0 && lpa & LPA_10FULL != 0 {
                (LinkSpeed::Mbps10, true)
            } else {
                (LinkSpeed::Mbps10, false)
            };

        Ok(Link {
            up: true,
            speed,
            full_duplex,
        })
    }

    // -----------------------------------------------------------------------
    // Enlace y habilitacion
    // -----------------------------------------------------------------------

    /// Le cuenta al MAC lo que el PHY negocio.
    ///
    /// **Dos divergencias deliberadas con el driver de U-Boot**, las dos por
    /// lo mismo: U-Boot ajusta el enlace una vez, antes de encender nada,
    /// mientras que un kernel lo reajusta con el MAC ya en marcha.
    ///
    /// 1. `UMAC_CMD` se modifica, no se reescribe. U-Boot escribe
    ///    `speed << CMD_SPEED_SHIFT` encima del registro entero, lo que borra
    ///    `CMD_TX_EN`/`CMD_RX_EN`: en U-Boot da igual porque enciende despues,
    ///    aqui apagaria la tarjeta en silencio en cada renegociacion.
    /// 2. Se pone el duplex (`CMD_HD_EN`). U-Boot no lo toca, asi que un
    ///    enlace a half duplex se queda con el MAC a full y el resultado son
    ///    colisiones que el contador de errores no explica.
    ///
    /// `id_mode_dis` es para el RGMII cuyo retardo lo pone la placa (el
    /// `rgmii-rxid` del device tree de la Pi 4), no el MAC.
    pub fn adjust_link(&self, link: &Link, id_mode_dis: bool) -> DeviceResult {
        if !link.up {
            return Err(DeviceError::NotReady);
        }

        self.modify(
            EXT_RGMII_OOB_CTRL,
            OOB_DISABLE,
            RGMII_LINK | RGMII_MODE_EN | if id_mode_dis { ID_MODE_DIS } else { 0 },
        );

        self.modify(
            UMAC_CMD,
            CMD_SPEED_MASK << CMD_SPEED_SHIFT | CMD_HD_EN,
            link.speed.umac_code() << CMD_SPEED_SHIFT
                | if link.full_duplex { 0 } else { CMD_HD_EN },
        );

        Ok(())
    }

    /// Enciende o apaga la recepcion y la transmision del MAC.
    pub fn set_rx_tx_enabled(&self, on: bool) {
        if on {
            self.modify(UMAC_CMD, 0, CMD_TX_EN | CMD_RX_EN);
        } else {
            self.modify(UMAC_CMD, CMD_TX_EN | CMD_RX_EN, 0);
        }
    }

    /// El modo promiscuo del UMAC.
    pub fn set_promiscuous(&self, on: bool) {
        if on {
            self.modify(UMAC_CMD, 0, CMD_PROMISC);
        } else {
            self.modify(UMAC_CMD, CMD_PROMISC, 0);
        }
    }
}

/// Si un identificador de PHY es de un PHY que esta de verdad.
///
/// El bus MDIO sin nadie en esa direccion devuelve todo unos o todo ceros, y
/// las dos cosas se leen como un identificador.
pub fn phy_id_is_present(id: u32) -> bool {
    id != 0 && id != 0xFFFF_FFFF && id != 0x0000_FFFF && id != 0xFFFF_0000
}

/// El OUI del BCM5421x de Broadcom, que es la familia del BCM54213PE de la
/// Raspberry Pi 4.
///
/// El identificador completo lleva la revision en el nibble bajo, y la
/// revision cambia entre tiradas de la misma placa: se compara con la mascara,
/// como hace Linux.
pub const BCM5421X_PHY_ID: u32 = 0x600D_84A0;
pub const BCM5421X_PHY_ID_MASK: u32 = 0xFFFF_FFF0;

/// Si este identificador es de la familia del PHY de la Raspberry Pi 4.
pub fn phy_is_bcm5421x(id: u32) -> bool {
    id & BCM5421X_PHY_ID_MASK == BCM5421X_PHY_ID
}

/// Espera activa de `us` microsegundos.
///
/// El tope de vueltas es la salida para un reloj que no avanza: sin el, un
/// `udelay` en el camino de arranque colgaria el nucleo entero en vez de
/// seguir con un retardo corto de mas.
fn udelay(us: u64) {
    if us == 0 {
        return;
    }
    const MAX_SPINS: u64 = 1_000_000;
    let t0 = timer_now_as_micros();
    let mut n = 0u64;
    while timer_now_as_micros().wrapping_sub(t0) < us {
        core::hint::spin_loop();
        n += 1;
        if n >= MAX_SPINS {
            break;
        }
    }
}

#[cfg(test)]
mod genet_fake;

#[cfg(test)]
mod umac_tests;

#[cfg(test)]
mod mdio_tests;
