//! El Cadence GEM (`cdns,macb`), que es el Ethernet gigabit de la Raspberry
//! Pi 5: el RP1 lo lleva dentro y lo publica por PCIe.
//!
//! Esta tanda es la capa de abajo, la misma que la primera del GENET: la
//! ventana de registros, la identificacion, el reset, la configuracion de red,
//! la direccion MAC y el bus MDIO con el que se habla al PHY externo. Los
//! anillos de descriptores, las interrupciones y el `NetScheme` van en tandas
//! siguientes.
//!
//! ## De donde sale el mapa de registros
//!
//! De `drivers/net/ethernet/cadence/macb.h` y `macb_main.c` de Linux (v6.12) y
//! de `drivers/net/macb.c` de U-Boot. Cada offset y cada bit de aqui lleva
//! apuntado **el nombre de la constante de la que sale**, y
//! [`regmap_tests`](super::macb::regmap_tests) los clava uno a uno: no prueban
//! el silicio, prueban que no se copiaron mal, que es el error realista.
//!
//! El nodo del device tree de la Pi 5 es `ethernet@100000` de `rp1.dtsi`:
//!
//! ```text
//! compatible = "raspberrypi,rp1-gem", "cdns,macb";
//! reg = <0xc0 0x40100000  0x0 0x4000>;
//! phy-mode = "rgmii-id";
//! ```
//!
//! ## Dos cosas que este driver NO puede alcanzar todavia
//!
//! 1. **El RP1 esta detras de un PCIe que Eclipse no sabe levantar**: la
//!    direccion de ahi arriba (`0xc0_4010_0000`) es una ventana del puente
//!    raiz del BCM2712, y en el arbol no hay driver de controlador PCIe de
//!    Broadcom STB, solo el `bus/pci.rs` de x86. Asi que en una Pi 5 de verdad
//!    nadie llega a esta ventana aun.
//! 2. **El nodo puede no estar en el DTB base**: en el arbol de Raspberry Pi,
//!    el RP1 se descubre por PCIe y su driver **aplica un overlay en caliente**
//!    con los nodos de dentro, asi que un lector de DTB no tiene por que
//!    encontrar un `cdns,macb` en el arbol que le dio el firmware.
//!
//! Y sigue en pie lo de siempre: **aqui no hay Raspberry Pi y QEMU no emula ni
//! el GENET ni este GEM**, asi que nada de esto se valida en placa. Todo corre
//! contra el MAC falso de [`macb_fake`], un `Vec<u32>` del monton.

use super::mii::*;
use super::timer_now_as_micros;
use crate::{DeviceError, DeviceResult};
use core::ptr::{read_volatile, write_volatile};

// ---------------------------------------------------------------------------
// Mapa de registros
// ---------------------------------------------------------------------------
//
// Los nombres son los de Linux: `MACB_*` los que estan en el mismo sitio en
// todas las generaciones, `GEM_*` los que el GEM movio.

/// `MACB_NCR`: control de red.
pub const NCR: usize = 0x000;
/// `MACB_NCFGR` / `GEM_NCFGR`: configuracion de red.
pub const NCFGR: usize = 0x004;
/// `MACB_NSR`: estado de red. Aqui vive el `IDLE` del bus MDIO.
pub const NSR: usize = 0x008;
/// `GEM_USRIO`: entradas y salidas de usuario. **En el GEM esta en 0x0c y en
/// el MACB de Atmel en 0xc0**, que es una de las razones por las que este
/// driver se niega a hablar con un MACB que no sea GEM.
pub const GEM_USRIO: usize = 0x00C;
/// `GEM_DMACFG`: configuracion del DMA. La escribe la tanda de los anillos.
pub const GEM_DMACFG: usize = 0x010;
/// `MACB_TSR`: estado de transmision.
pub const TSR: usize = 0x014;
/// `MACB_RBQP`: base de la cola de recepcion.
pub const RBQP: usize = 0x018;
/// `MACB_TBQP`: base de la cola de transmision.
pub const TBQP: usize = 0x01C;
/// `MACB_RSR`: estado de recepcion.
pub const RSR: usize = 0x020;
/// `MACB_ISR`: estado de las interrupciones.
pub const ISR: usize = 0x024;
/// `MACB_IER`: habilitar interrupciones (solo escritura).
pub const IER: usize = 0x028;
/// `MACB_IDR`: deshabilitar interrupciones (solo escritura).
pub const IDR: usize = 0x02C;
/// `MACB_IMR`: la mascara, que es lo que se lee.
pub const IMR: usize = 0x030;
/// `MACB_MAN`: mantenimiento del PHY, o sea el bus MDIO.
pub const MAN: usize = 0x034;
/// `GEM_PBUFRXCUT`: "partial store and forward" de recepcion.
pub const GEM_PBUFRXCUT: usize = 0x044;
/// `GEM_JML`: trama jumbo maxima.
pub const GEM_JML: usize = 0x048;
/// `GEM_HRB` / `GEM_HRT`: el hash de multicast.
pub const GEM_HRB: usize = 0x080;
pub const GEM_HRT: usize = 0x084;
/// `GEM_SA1B`: la parte baja de la primera direccion especifica, que es la MAC
/// de la tarjeta. **En el MACB de Atmel esto esta en 0x98.**
pub const GEM_SA1B: usize = 0x088;
/// `GEM_SA1T`: la parte alta.
pub const GEM_SA1T: usize = 0x08C;
/// `MACB_SA1B`: el mismo registro en un MACB que no es GEM. No se usa (este
/// driver no habla con esos), esta para que el test pueda comprobar que los
/// dos juegos son distintos de verdad y la negativa tiene sentido.
pub const MACB_SA1B: usize = 0x098;
/// Cuanto hay de un juego de direccion especifica al siguiente: cuatro juegos
/// de dos palabras, `SA1B SA1T SA2B SA2T ...`.
pub const SA_STRIDE: usize = 8;
/// Cuantos juegos de direccion especifica tiene el GEM.
pub const SA_SETS: usize = 4;
/// `MACB_MID`: identificacion del modulo.
pub const MID: usize = 0x0FC;
/// `GEM_DCFG1`: configuracion de diseño 1. De aqui sale el ancho del bus.
pub const GEM_DCFG1: usize = 0x280;
/// `GEM_DCFG2`: configuracion de diseño 2.
pub const GEM_DCFG2: usize = 0x284;
/// `GEM_DCFG6`: configuracion de diseño 6. De aqui sale si el DMA direcciona
/// a 64 bits, que es lo que la tanda de los anillos necesita saber.
pub const GEM_DCFG6: usize = 0x294;
/// `MACB_TBQPH` / `MACB_RBQPH`: la mitad alta de las bases de las colas,
/// cuando el DMA direcciona a 64 bits.
pub const TBQPH: usize = 0x4C8;
pub const RBQPH: usize = 0x4D4;

/// La ventana entera, como la declara el `reg` del nodo del RP1.
pub const MACB_WINDOW_BYTES: usize = 0x4000;

// Bits de `NCR` (`MACB_*_OFFSET` de macb.h).
/// `LLB`, bucle local. Es el bit con el que Linux comprueba si hay algo en la
/// ventana antes de creerse nada.
pub const NCR_LLB: u32 = 1 << 1;
/// `RE`, recepcion encendida.
pub const NCR_RE: u32 = 1 << 2;
/// `TE`, transmision encendida.
pub const NCR_TE: u32 = 1 << 3;
/// `MPE`, puerto de mantenimiento encendido: **sin esto el bus MDIO no
/// funciona**, y una lectura del PHY devuelve basura sin dar ningun error.
pub const NCR_MPE: u32 = 1 << 4;
/// `CLRSTAT`, pone a cero los contadores de estadisticas. Es una orden: se
/// escribe a uno y el silicio no lo deja puesto.
pub const NCR_CLRSTAT: u32 = 1 << 5;
/// `INCSTAT`, incrementa los contadores (solo para probarlos).
pub const NCR_INCSTAT: u32 = 1 << 6;
/// `WESTAT`, permite escribir los contadores.
pub const NCR_WESTAT: u32 = 1 << 7;
/// `TSTART`, arranca una transmision.
pub const NCR_TSTART: u32 = 1 << 9;
/// `THALT`, para la transmision.
pub const NCR_THALT: u32 = 1 << 10;

// Bits y campos de `NCFGR`.
/// `SPD`: a uno son 100 megabits, a cero 10. El gigabit **no esta aqui**, esta
/// en `GBE`.
pub const NCFGR_SPD: u32 = 1 << 0;
/// `FD`: full duplex.
pub const NCFGR_FD: u32 = 1 << 1;
/// `JFRAME`: tramas jumbo.
pub const NCFGR_JFRAME: u32 = 1 << 3;
/// `CAF`: copia todas las tramas, o sea el modo promiscuo.
pub const NCFGR_CAF: u32 = 1 << 4;
/// `NBC`: nada de broadcast.
pub const NCFGR_NBC: u32 = 1 << 5;
/// `MTI`: hash de multicast encendido.
pub const NCFGR_MTI: u32 = 1 << 6;
/// `UNI`: hash de unicast encendido.
pub const NCFGR_UNI: u32 = 1 << 7;
/// `BIG`: acepta tramas de 1536 bytes.
pub const NCFGR_BIG: u32 = 1 << 8;
/// `GEM_GBE`: gigabit. **Ocupa el bit 10, que en un MACB que no es GEM es el
/// bit bajo del divisor del MDC** (`MACB_CLK_OFFSET` 10, tamaño 2): poner
/// gigabit en el sitio equivocado cambia el reloj del bus MDIO.
pub const NCFGR_GBE: u32 = 1 << 10;
/// `GEM_PCSSEL`: el PCS interno, para SGMII y 10GBASE-R. La Pi 5 es RGMII.
pub const NCFGR_PCSSEL: u32 = 1 << 11;
/// `PAE`: control de flujo por tramas de pausa.
pub const NCFGR_PAE: u32 = 1 << 13;
/// `RBOF`: cuantos bytes de relleno mete el MAC delante de la trama recibida.
pub const NCFGR_RBOF_SHIFT: u32 = 14;
pub const NCFGR_RBOF_MASK: u32 = 0x3;
/// `DRFCS`: quita el FCS de la trama recibida.
pub const NCFGR_DRFCS: u32 = 1 << 17;
/// `GEM_CLK`: el divisor del reloj del MDC. **En el GEM esta en el bit 18 y
/// mide tres bits**, no en el 10 y dos como en el MACB.
pub const NCFGR_CLK_SHIFT: u32 = 18;
pub const NCFGR_CLK_MASK: u32 = 0x7;
/// `GEM_DBW`: el ancho del bus de datos.
pub const NCFGR_DBW_SHIFT: u32 = 21;
pub const NCFGR_DBW_MASK: u32 = 0x3;
/// `GEM_RXCOEN`: descarga de la suma de comprobacion en recepcion.
pub const NCFGR_RXCOEN: u32 = 1 << 24;
/// `GEM_SGMIIEN`: SGMII.
pub const NCFGR_SGMIIEN: u32 = 1 << 27;

/// Trama mas larga que el MAC acepta con `BIG` puesto: los 1518 de una trama
/// Ethernet con su FCS, redondeados a 1536 como hace el silicio.
pub const ENET_MAX_FRAME_LEN: u32 = 1536;

/// Los dos bytes de relleno que el MAC mete delante de la trama para que la
/// cabecera IP caiga alineada a cuatro (el `NET_IP_ALIGN` de Linux).
pub const RX_IP_ALIGN: u32 = 2;

/// Codificacion de `GEM_DBW`: 32, 64 y 128 bits de bus AMBA.
pub const GEM_DBW32: u32 = 0;
pub const GEM_DBW64: u32 = 1;
pub const GEM_DBW128: u32 = 2;

/// `GEM_DBWDEF`, en `DCFG1`: el ancho que el silicio dice tener.
pub const DCFG1_DBWDEF_SHIFT: u32 = 25;
pub const DCFG1_DBWDEF_MASK: u32 = 0x7;
/// `GEM_IRQCOR`, en `DCFG1`: a cero, el `ISR` se limpia escribiendolo.
pub const DCFG1_IRQCOR: u32 = 1 << 23;
/// `GEM_DAW64`, en `DCFG6`: el DMA direcciona a 64 bits.
pub const DCFG6_DAW64: u32 = 1 << 23;

/// Codificacion del divisor del MDC en el GEM (`GEM_CLK_DIV*`).
pub const GEM_CLK_DIV8: u32 = 0;
pub const GEM_CLK_DIV16: u32 = 1;
pub const GEM_CLK_DIV32: u32 = 2;
pub const GEM_CLK_DIV48: u32 = 3;
pub const GEM_CLK_DIV64: u32 = 4;
pub const GEM_CLK_DIV96: u32 = 5;
pub const GEM_CLK_DIV128: u32 = 6;
pub const GEM_CLK_DIV224: u32 = 7;

// Bits y campos de `NSR`.
/// `MACB_IDLE`: la logica de gestion del PHY esta quieta.
pub const NSR_IDLE: u32 = 1 << 2;

// Campos de `MAN`, el bus MDIO de clausula 22.
pub const MAN_DATA_SHIFT: u32 = 0;
pub const MAN_DATA_MASK: u32 = 0xFFFF;
pub const MAN_CODE_SHIFT: u32 = 16;
pub const MAN_CODE_MASK: u32 = 0x3;
pub const MAN_REGA_SHIFT: u32 = 18;
pub const MAN_REGA_MASK: u32 = 0x1F;
pub const MAN_PHYA_SHIFT: u32 = 23;
pub const MAN_PHYA_MASK: u32 = 0x1F;
pub const MAN_RW_SHIFT: u32 = 28;
pub const MAN_RW_MASK: u32 = 0x3;
pub const MAN_SOF_SHIFT: u32 = 30;
pub const MAN_SOF_MASK: u32 = 0x3;

/// Los valores fijos de una transaccion de clausula 22 (`MACB_MAN_C22_*`).
pub const MAN_C22_SOF: u32 = 1;
pub const MAN_C22_CODE: u32 = 2;
pub const MAN_C22_WRITE: u32 = 1;
pub const MAN_C22_READ: u32 = 2;

// Campos de `MID`.
/// `MACB_IDNUM`: que modulo es. El GEM dice 2 o mas.
pub const MID_IDNUM_SHIFT: u32 = 16;
pub const MID_IDNUM_MASK: u32 = 0xFFF;
/// `MACB_REV`: la revision.
pub const MID_REV_SHIFT: u32 = 0;
pub const MID_REV_MASK: u32 = 0xFFFF;

/// El `IDNUM` desde el que un Cadence es un GEM, y no el MACB viejo de Atmel
/// (`hw_is_gem` de Linux: `MACB_BFEXT(IDNUM, id) >= 0x2`).
pub const IDNUM_GEM_MIN: u32 = 2;

/// Plazo del bus MDIO, en microsegundos. El de Linux (`MACB_MDIO_TIMEOUT`) es
/// un segundo entero, que es un plazo de un driver que puede dormir; aqui se
/// espera dando vueltas en el camino de arranque, asi que se usa el mismo que
/// el del GENET: una transaccion a 2,5 MHz son decenas de microsegundos, y 20
/// ms es un PHY que no esta.
const MDIO_TIMEOUT_US: u64 = 20_000;

// ---------------------------------------------------------------------------
// Lo que hay que saber de la placa
// ---------------------------------------------------------------------------

/// Lo que el arranque le tiene que contar al driver y no esta en ningun
/// registro.
#[derive(Debug, Clone, Copy)]
pub struct MacbConfig {
    /// El `pclk` en hercios, que es lo unico que decide el divisor del MDC.
    /// En la Pi 5 es `RP1_CLK_SYS`, que el device tree declara como el reloj
    /// `pclk` del nodo.
    pub pclk_hz: u32,
    /// Modo promiscuo desde el arranque.
    pub promiscuous: bool,
}

impl Default for MacbConfig {
    /// El `clk_sys` del RP1 son 200 MHz, que es el reloj con el que arranca la
    /// Pi 5. No es un dato del silicio del GEM sino de la placa, asi que el
    /// que lea el device tree debe sobreescribirlo con lo que diga el arbol.
    fn default() -> Self {
        Self {
            pclk_hz: 200_000_000,
            promiscuous: false,
        }
    }
}

/// El divisor del MDC que le toca a un `pclk`, codificado para `NCFGR`.
///
/// Es la tabla de `gem_mdc_clk_div` de Linux tal cual. El MDC no puede pasar
/// de 2,5 MHz, que es el techo del 802.3 para clausula 22, y el divisor es lo
/// unico que lo baja: pasarse no da ningun error, da un PHY que contesta
/// basura.
///
/// Los tramos son **inclusivos por arriba**, como los `<=` de Linux: un
/// `pclk` de exactamente 20 MHz va a dividir por 8, no por 16.
pub fn mdc_clk_div(pclk_hz: u32) -> u32 {
    let div = if pclk_hz <= 20_000_000 {
        GEM_CLK_DIV8
    } else if pclk_hz <= 40_000_000 {
        GEM_CLK_DIV16
    } else if pclk_hz <= 80_000_000 {
        GEM_CLK_DIV32
    } else if pclk_hz <= 120_000_000 {
        GEM_CLK_DIV48
    } else if pclk_hz <= 160_000_000 {
        GEM_CLK_DIV64
    } else if pclk_hz <= 240_000_000 {
        GEM_CLK_DIV96
    } else if pclk_hz <= 320_000_000 {
        GEM_CLK_DIV128
    } else {
        GEM_CLK_DIV224
    };
    (div & NCFGR_CLK_MASK) << NCFGR_CLK_SHIFT
}

/// El campo `DBW` de `NCFGR` que le toca a lo que `DCFG1` declara.
///
/// Es `macb_dbw` de Linux: el silicio dice su ancho en `DBWDEF` con una
/// codificacion (1, 2, 4) que **no es la del campo `DBW`** (0, 1, 2), y
/// cualquier otra cosa se trata como 32 bits, que es la unica suposicion que
/// no puede partir una transferencia a la mitad.
pub fn dbw_field(dcfg1: u32) -> u32 {
    let declarado = (dcfg1 >> DCFG1_DBWDEF_SHIFT) & DCFG1_DBWDEF_MASK;
    let dbw = match declarado {
        4 => GEM_DBW128,
        2 => GEM_DBW64,
        _ => GEM_DBW32,
    };
    (dbw & NCFGR_DBW_MASK) << NCFGR_DBW_SHIFT
}

// ---------------------------------------------------------------------------
// La ventana de registros
// ---------------------------------------------------------------------------

/// El GEM: nada mas que la base de su ventana de registros.
///
/// La base es **virtual**: la trae ya mapeada quien sondea el bus donde esta
/// la tarjeta. Bajo test es la direccion del `Vec` del MAC falso.
///
/// Es `Copy` por lo mismo que [`super::GenetMac`]: la ventana se mira desde el
/// manejador de la interrupcion y desde el trabajo diferido a la vez, y lo que
/// hay que serializar es el estado de los anillos, no una escritura de una
/// palabra.
#[derive(Clone, Copy)]
pub struct MacbMac {
    base: usize,
}

impl MacbMac {
    /// # Safety
    ///
    /// `base` tiene que ser una ventana de al menos [`MACB_WINDOW_BYTES`]
    /// mapeada sin cache sobre los registros del GEM, y nadie mas puede estar
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
        let val = unsafe { read_volatile((self.base + off) as *const u32) };
        // La otra mitad de la costura del MAC falso: hay un registro que
        // **se limpia al leerlo** (el `ISR`, con `GEM_IRQCOR` puesto), y un
        // modelo en memoria que no lo haga deja pasar un driver que lo lee
        // dos veces --- que en ese silicio pierde causas y en el otro no.
        #[cfg(test)]
        let val = macb_fake::on_read(self.base, off, val);
        val
    }

    #[inline]
    fn write(&self, off: usize, val: u32) {
        unsafe { write_volatile((self.base + off) as *mut u32, val) };
        // La reaccion del MAC falso, que es la unica costura `cfg(test)` del
        // fichero: un `MAN` escrito tiene que dejar contestado el PHY, y un
        // `IDR` tiene que quitar bits de `IMR` en vez de guardarse.
        #[cfg(test)]
        macb_fake::on_write(self.base, off, val);
    }

    fn modify(&self, off: usize, clear: u32, set: u32) {
        let v = self.read(off);
        self.write(off, (v & !clear) | set);
    }

    // -----------------------------------------------------------------------
    // Identificacion
    // -----------------------------------------------------------------------

    /// El `IDNUM` y la revision de `MID`.
    pub fn hw_id(&self) -> (u32, u32) {
        let id = self.read(MID);
        (
            (id >> MID_IDNUM_SHIFT) & MID_IDNUM_MASK,
            (id >> MID_REV_SHIFT) & MID_REV_MASK,
        )
    }

    /// Si lo que hay en la ventana es un GEM y no el MACB viejo.
    pub fn is_gem(&self) -> bool {
        self.hw_id().0 >= IDNUM_GEM_MIN
    }

    /// Si hay algo en la ventana.
    ///
    /// Es `macb_probe_queues` de Linux: se escribe el bit del bucle local en
    /// `NCR`, se relee y se deja el registro a cero. Una ventana sin tarjeta
    /// detras no devuelve lo escrito, y eso es lo unico que distingue un MAC
    /// de una direccion mal mapeada **antes** de creerse un `MID`: un `MID`
    /// leido de la nada puede dar cualquier `IDNUM`, incluido uno valido.
    ///
    /// **Deja `NCR` a cero**, que es lo que hace Linux. Por eso esto es una
    /// prueba de sondeo y no se puede llamar con el MAC en marcha: borraria el
    /// `MPE` y las dos habilitaciones.
    pub fn responds(&self) -> bool {
        self.write(NCR, NCR_LLB);
        let leido = self.read(NCR);
        self.write(NCR, 0);
        leido == NCR_LLB
    }

    /// `Ok` solo si hay un GEM delante.
    ///
    /// Un MACB que no es GEM se rechaza en vez de intentarlo: tiene la MAC y
    /// el hash en otros offsets ([`MACB_SA1B`] contra [`GEM_SA1B`]), el
    /// divisor del MDC en otro campo y ningun gigabit, asi que seguir adelante
    /// seria escribir registros al azar.
    pub fn check_supported(&self) -> DeviceResult {
        if !self.responds() {
            return Err(DeviceError::NotReady);
        }
        if !self.is_gem() {
            return Err(DeviceError::NotSupported);
        }
        Ok(())
    }

    /// Si el DMA de este GEM direcciona a 64 bits (`GEM_DAW64` de `DCFG6`).
    pub fn has_64bit_dma(&self) -> bool {
        self.read(GEM_DCFG6) & DCFG6_DAW64 != 0
    }

    /// Si el `ISR` se limpia escribiendolo (`GEM_IRQCOR` a cero en `DCFG1`).
    ///
    /// Es la unica forma de saberlo: en el silicio donde no es asi, escribir
    /// el `ISR` no hace nada y el manejador se quedaria dando vueltas sobre la
    /// misma interrupcion.
    pub fn isr_clear_on_write(&self) -> bool {
        self.read(GEM_DCFG1) & DCFG1_IRQCOR == 0
    }

    // -----------------------------------------------------------------------
    // Reset y configuracion
    // -----------------------------------------------------------------------

    /// Deja el MAC parado y callado, como `macb_reset_hw` de Linux.
    ///
    /// `NCR` se **modifica**, no se reescribe: en el camino de arranque da
    /// igual, pero esto mismo se llama al bajar la interfaz y ahi un `write`
    /// completo borraria el `MPE`, dejando el bus MDIO muerto sin decir nada.
    ///
    /// El orden importa en un sitio: las interrupciones se apagan (`IDR`)
    /// **antes** de leer el `ISR` para limpiarlo, porque al contrario el
    /// silicio puede poner un bit entre la lectura y el apagado y ese bit se
    /// queda puesto con la linea levantada.
    pub fn reset_hw(&self) {
        self.modify(NCR, NCR_RE | NCR_TE, NCR_CLRSTAT);

        // `TSR` y `RSR` se limpian escribiendo unos donde hay que limpiar.
        self.write(TSR, !0);
        self.write(RSR, !0);

        self.write(GEM_PBUFRXCUT, 0);

        self.write(IDR, !0);
        let pendientes = self.read(ISR);
        if self.isr_clear_on_write() {
            self.write(ISR, pendientes);
        }
    }

    /// Escribe `NCFGR` entero, que es lo que hace `macb_init_hw`.
    ///
    /// Aqui si se reescribe el registro: es el arranque, y el valor es
    /// completo a proposito --- lo que `NCFGR` tenga de un arranque anterior o
    /// de la firmware no es un punto de partida, es basura.
    ///
    /// `BIG` (tramas de 1536) va puesto y `JFRAME` (jumbo) no: el GEM del RP1
    /// no declara jumbo en el device tree, y pedir jumbo sin que el silicio lo
    /// tenga cambia el significado del campo de longitud del descriptor.
    pub fn init_config(&self, cfg: &MacbConfig) {
        let mut ncfgr = mdc_clk_div(cfg.pclk_hz)
            | dbw_field(self.read(GEM_DCFG1))
            | ((RX_IP_ALIGN & NCFGR_RBOF_MASK) << NCFGR_RBOF_SHIFT)
            | NCFGR_DRFCS
            | NCFGR_BIG;
        if cfg.promiscuous {
            ncfgr |= NCFGR_CAF;
        }
        self.write(NCFGR, ncfgr);
    }

    /// Enciende o apaga el puerto de mantenimiento, o sea el bus MDIO.
    pub fn set_mdio_enabled(&self, on: bool) {
        if on {
            self.modify(NCR, 0, NCR_MPE);
        } else {
            self.modify(NCR, NCR_MPE, 0);
        }
    }

    // -----------------------------------------------------------------------
    // Direccion MAC
    // -----------------------------------------------------------------------

    /// Escribe la MAC en el primer juego de direccion especifica y deja los
    /// otros tres a cero.
    ///
    /// **El orden de los bytes es el contrario al del GENET**: aqui el primer
    /// byte del cable es el **menos** significativo de `SA1B`
    /// (`macb_set_hwaddr` de Linux lo hace con un `cpu_to_le32` de los cuatro
    /// primeros bytes). Equivocarlo da una tarjeta que no se reconoce en
    /// ninguna trama que le llegue, sin un solo error en los contadores.
    ///
    /// Los otros tres juegos se ponen a cero porque un juego con basura filtra
    /// tramas ajenas hacia dentro.
    pub fn write_mac_addr(&self, mac: &[u8; 6]) {
        let bottom = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
        let top = u16::from_le_bytes([mac[4], mac[5]]) as u32;
        self.write(GEM_SA1B, bottom);
        self.write(GEM_SA1T, top);
        for set in 1..SA_SETS {
            self.write(GEM_SA1B + set * SA_STRIDE, 0);
            self.write(GEM_SA1T + set * SA_STRIDE, 0);
        }
    }

    /// Relee una de las cuatro direcciones especificas.
    pub fn read_mac_addr_set(&self, set: usize) -> [u8; 6] {
        let bottom = self.read(GEM_SA1B + set * SA_STRIDE);
        let top = self.read(GEM_SA1T + set * SA_STRIDE);
        [
            bottom as u8,
            (bottom >> 8) as u8,
            (bottom >> 16) as u8,
            (bottom >> 24) as u8,
            top as u8,
            (top >> 8) as u8,
        ]
    }

    /// La primera MAC que la firmware dejo en los registros, si dejo alguna.
    ///
    /// Es `macb_get_hwaddr` de Linux: recorre los cuatro juegos y se queda con
    /// el primero que valga. En la Pi 5 el `local-mac-address` del device tree
    /// viene **a ceros** en `rp1.dtsi` y lo rellena la firmware, asi que esta
    /// es la fuente de verdad cuando el arbol no trae nada.
    pub fn hw_mac_addr(&self) -> Option<[u8; 6]> {
        (0..SA_SETS)
            .map(|set| self.read_mac_addr_set(set))
            .find(mac_addr_is_valid)
    }

    // -----------------------------------------------------------------------
    // El bus MDIO
    // -----------------------------------------------------------------------

    /// Espera a que la logica de gestion del PHY quede quieta.
    ///
    /// Se espera **antes y despues** de cada transaccion, como Linux: antes
    /// porque `MAN` escrito mientras hay una en marcha la pisa, y despues
    /// porque hasta que acaba el dato que hay en `MAN` es el de la anterior.
    fn mdio_wait_idle(&self) -> DeviceResult {
        let t0 = timer_now_as_micros();
        loop {
            if self.read(NSR) & NSR_IDLE != 0 {
                return Ok(());
            }
            if timer_now_as_micros().wrapping_sub(t0) >= MDIO_TIMEOUT_US {
                return Err(DeviceError::NotReady);
            }
            core::hint::spin_loop();
        }
    }

    /// La palabra de `MAN` de una transaccion de clausula 22.
    fn man_word(op: u32, pmd: u8, reg: u32, data: u16) -> u32 {
        ((MAN_C22_SOF & MAN_SOF_MASK) << MAN_SOF_SHIFT)
            | ((op & MAN_RW_MASK) << MAN_RW_SHIFT)
            | ((pmd as u32 & MAN_PHYA_MASK) << MAN_PHYA_SHIFT)
            | ((reg & MAN_REGA_MASK) << MAN_REGA_SHIFT)
            | ((MAN_C22_CODE & MAN_CODE_MASK) << MAN_CODE_SHIFT)
            | ((data as u32 & MAN_DATA_MASK) << MAN_DATA_SHIFT)
    }

    /// Lee un registro del PHY `pmd` por MDIO.
    ///
    /// **No hay bit de error**: el GEM no tiene nada como el `READ_FAIL` del
    /// GENET, asi que una direccion sin PHY no se distingue aqui de un PHY que
    /// contesta todo unos. Quien busque un PHY tiene que mirar el dato
    /// ([`phy_id_is_present`]), no el resultado.
    pub fn mdio_read(&self, pmd: u8, reg: u32) -> DeviceResult<u16> {
        self.mdio_wait_idle()?;
        self.write(MAN, Self::man_word(MAN_C22_READ, pmd, reg, 0));
        self.mdio_wait_idle()?;
        Ok((self.read(MAN) >> MAN_DATA_SHIFT) as u16)
    }

    /// Escribe un registro del PHY `pmd` por MDIO.
    pub fn mdio_write(&self, pmd: u8, reg: u32, val: u16) -> DeviceResult {
        self.mdio_wait_idle()?;
        self.write(MAN, Self::man_word(MAN_C22_WRITE, pmd, reg, val));
        self.mdio_wait_idle()
    }

    // -----------------------------------------------------------------------
    // El PHY
    // -----------------------------------------------------------------------

    /// El identificador de 32 bits del PHY.
    pub fn phy_id(&self, pmd: u8) -> DeviceResult<u32> {
        let hi = self.mdio_read(pmd, MII_PHYSID1)? as u32;
        let lo = self.mdio_read(pmd, MII_PHYSID2)? as u32;
        Ok(hi << 16 | lo)
    }

    /// Busca el PHY recorriendo las 32 direcciones del bus.
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

    /// Lee del PHY el estado del enlace, por el bus MDIO del GEM.
    pub fn read_link(&self, pmd: u8) -> DeviceResult<Link> {
        read_link_with(|reg| self.mdio_read(pmd, reg))
    }

    // -----------------------------------------------------------------------
    // Enlace y habilitacion
    // -----------------------------------------------------------------------

    /// Le cuenta al MAC lo que el PHY negocio.
    ///
    /// **La velocidad son dos campos, no uno**: `SPD` dice 10 o 100 y `GBE`
    /// dice gigabit, asi que gigabit es `GBE` puesto con `SPD` a cero, y
    /// dejarse `GBE` puesto al bajar a 100 es una tarjeta que no transmite
    /// nada mientras el enlace parece estar arriba.
    ///
    /// Se modifica y no se reescribe: esto se llama con el MAC en marcha cada
    /// vez que el enlace renegocia, y el resto de `NCFGR` (el divisor del MDC,
    /// el ancho del bus, el relleno de recepcion) es del arranque.
    pub fn adjust_link(&self, link: &Link) -> DeviceResult {
        if !link.up {
            return Err(DeviceError::NotReady);
        }
        let mut set = 0;
        match link.speed {
            LinkSpeed::Mbps1000 => set |= NCFGR_GBE,
            LinkSpeed::Mbps100 => set |= NCFGR_SPD,
            LinkSpeed::Mbps10 => {}
        }
        if link.full_duplex {
            set |= NCFGR_FD;
        }
        self.modify(NCFGR, NCFGR_SPD | NCFGR_FD | NCFGR_GBE, set);
        Ok(())
    }

    /// Enciende o apaga la recepcion y la transmision.
    pub fn set_rx_tx_enabled(&self, on: bool) {
        if on {
            self.modify(NCR, 0, NCR_RE | NCR_TE);
        } else {
            self.modify(NCR, NCR_RE | NCR_TE, 0);
        }
    }

    /// El modo promiscuo (`CAF`, copia todas las tramas).
    pub fn set_promiscuous(&self, on: bool) {
        if on {
            self.modify(NCFGR, 0, NCFGR_CAF);
        } else {
            self.modify(NCFGR, NCFGR_CAF, 0);
        }
    }
}

/// Si una MAC leida de los registros es una MAC de verdad.
///
/// Es `is_valid_ether_addr` de Linux: ni todo ceros (lo que deja un registro
/// que nadie relleno) ni multicast (el bit bajo del primer byte), porque una
/// tarjeta no puede tener por suya una direccion de grupo.
pub fn mac_addr_is_valid(mac: &[u8; 6]) -> bool {
    mac.iter().any(|b| *b != 0) && mac[0] & 1 == 0
}

/// Los anillos de descriptores y el camino de la trama.
mod rings;
pub use rings::{MacbRings, RingKind, MAX_TX_FRAME, RX_BUF_LENGTH, RX_RING_SIZE, TX_RING_SIZE};

/// Las cuatro ventanas de la interrupcion y los grupos de bits.
mod irq;
pub use irq::*;

#[cfg(test)]
mod macb_fake;

#[cfg(test)]
mod regmap_tests;

#[cfg(test)]
mod gem_tests;

#[cfg(test)]
mod mdio_tests;
