//! Las interrupciones del GEM: `ISR`, `IER`, `IDR` e `IMR`.
//!
//! El GEM no se maneja como el GENET de la Pi 4, que lleva dos bloques
//! `INTRL2` con sus seis registros cada uno, ni como una tarjeta de PCI
//! clasica, donde la mascara es un registro que se lee y se escribe. Aqui hay
//! **cuatro** registros y tres de ellos no son lo que parecen:
//!
//! * `IER` e `IDR` son **de solo escritura**, y lo que se les escribe son
//!   ordenes: un bit a uno en `IER` **arma** esa interrupcion, un bit a uno en
//!   `IDR` la **desarma**. Los bits a cero no dicen nada. Escribir la mascara
//!   que uno quiere, como se haria en un registro normal, no desarma nada.
//! * `IMR` es el unico que se puede leer, y **este driver no lo lee**. Se
//!   llama *mask* y sus companeros se llaman *enable* y *disable*, asi que un
//!   uno tiene que querer decir enmascarado --un registro de mascara donde el
//!   uno significara armado seria el `IER` otra vez--, y el reset deja la
//!   tarjeta con todo callado, que con esa polaridad es el registro a unos.
//!   Pero **ningun driver publicado lee el `IMR` para decidir nada**: ni
//!   Linux, ni el `emacps` de Xilinx, ni el `cgem` de FreeBSD. Linux filtra el
//!   `ISR` contra **la constante que acaba de armar**, y eso es lo que se hace
//!   aqui: depender de una polaridad que nadie ha puesto a prueba, en un
//!   silicio que aqui no se puede probar, seria adivinar.
//! * `ISR` **puede no limpiarse escribiendolo**. Depende de `GEM_IRQCOR` en
//!   `DCFG1`, que la tanda 1 ya lee en
//!   [`isr_clear_on_write`](super::MacbMac::isr_clear_on_write): con el bit
//!   puesto, el `ISR` se limpia **al leerlo** y escribirlo no hace nada.
//!
//! Los offsets, los bits y los tres grupos de abajo son los de `macb.h` y
//! `macb_main.c` de Linux. U-Boot no arma ninguna interrupcion --sondea el
//! anillo-- asi que de esta pieza no hay divergencia que documentar: no hay
//! con que divergir.
//!
//! **Nada de esto se ha probado contra el silicio**: aqui no hay Pi 5, y
//! ademas el RP1 vive detras de un PCIe del BCM2712 que Eclipse todavia no
//! sabe levantar. Lo unico que se puede garantizar es que cada bit lleva en su
//! comentario el nombre de la constante de la que sale.

use super::{MacbMac, IDR, IER, ISR, NCR, NCR_RE};

// Los bits del `ISR`/`IER`/`IDR`/`IMR`. Son los mismos cuatro registros con
// los mismos bits, que es lo que hace que una mascara valga para los cuatro.

/// `MACB_MFD`: se fue la trama de gestion del MDIO.
pub const ISR_MFD: u32 = 1 << 0;
/// `MACB_RCOMP`: recepcion completa. **Esta es la de recepcion.**
pub const ISR_RCOMP: u32 = 1 << 1;
/// `MACB_RXUBR`: el DMA de recepcion leyo un descriptor **que seguia siendo
/// del driver**, es decir, se quedo sin huecos. Ver
/// [`MacbMac::rx_restart`].
pub const ISR_RXUBR: u32 = 1 << 2;
/// `MACB_TXUBR`: lo mismo del lado de la transmision. No es un error: es el
/// centinela que la tanda 2 planta detras de cada trama, y el MAC avisa de
/// haberlo leido.
pub const ISR_TXUBR: u32 = 1 << 3;
/// `MACB_ISR_TUND`: se vacio la FIFO de transmision con la trama a medias.
pub const ISR_TUND: u32 = 1 << 4;
/// `MACB_ISR_RLE`: se agotaron los reintentos, o colision tardia.
pub const ISR_RLE: u32 = 1 << 5;
/// `MACB_TXERR`: el MAC dio la trama por corrupta.
pub const ISR_TXERR: u32 = 1 << 6;
/// `MACB_TCOMP`: transmision completa. **Esta es la de transmision.**
pub const ISR_TCOMP: u32 = 1 << 7;
/// `MACB_ISR_LINK`: cambio del enlace. **El GEM del RP1 no lo levanta**: el
/// enlace lo lleva el PHY por MDIO, no el MAC. Se nombra para no armarlo por
/// descuido creyendo que hace lo que hace el `UMAC_IRQ_LINK_EVENT` del GENET.
pub const ISR_LINK: u32 = 1 << 9;
/// `MACB_ISR_ROVR`: se desbordo la FIFO de recepcion. Tramas perdidas dentro
/// del MAC, antes de que ningun descriptor las viera.
pub const ISR_ROVR: u32 = 1 << 10;
/// `MACB_HRESP`: el bus le contesto que no al DMA. Esto **no** es un problema
/// de red: es el DMA escribiendo o leyendo donde no debe.
pub const ISR_HRESP: u32 = 1 << 11;
/// `MACB_PFR`: llego una trama de pausa con cantidad.
pub const ISR_PFR: u32 = 1 << 12;
/// `MACB_PTZ`: el contador de la pausa llego a cero.
pub const ISR_PTZ: u32 = 1 << 13;

/// `MACB_RX_INT_FLAGS`, de `macb_main.c`.
pub const MACB_RX_INT_FLAGS: u32 = ISR_RCOMP | ISR_ROVR;

/// `MACB_TX_ERR_FLAGS`, de `macb_main.c`.
pub const MACB_TX_ERR_FLAGS: u32 = ISR_TUND | ISR_RLE | ISR_TXERR;

/// `MACB_TX_INT_FLAGS`, de `macb_main.c`. Lleva dentro los tres de error,
/// porque los tres acaban en lo mismo: hay descriptores que recoger y una
/// cuenta que subir.
pub const MACB_TX_INT_FLAGS: u32 = MACB_TX_ERR_FLAGS | ISR_TCOMP | ISR_TXUBR;

/// Lo que este driver quiere que le despierte.
///
/// Las de recepcion, las de transmision y el `HRESP`, que es lo mismo que arma
/// Linux en `macb_open`. Dos ausencias a proposito:
///
/// * `ISR_RXUBR`, que Linux arma **solo en dos versiones de silicio** por una
///   errata (la seccion 16.7.4 de su hoja). El GEM del RP1 no es ninguna de
///   las dos, y armarlo en una tarjeta que no lo necesita es una interrupcion
///   por cada vez que la recepcion se queda corta, o sea justo cuando la
///   tarjeta esta mas ocupada. El arreglo esta escrito ([`MacbMac::rx_restart`])
///   para quien lo necesite, pero el bit no se arma aqui.
/// * `ISR_MFD`, porque el bus MDIO se usa con un plazo en el camino del
///   arranque y del enlace, no por interrupcion.
///
/// Y `ISR_LINK` tampoco, que en este MAC no se levanta.
pub const MACB_IRQ_WANTED: u32 = MACB_RX_INT_FLAGS | MACB_TX_INT_FLAGS | ISR_HRESP;

/// Todas, para desarmar o reconocer de golpe.
pub const MACB_IRQ_ALL: u32 = 0xFFFF_FFFF;

// Un bit armado tiene que estar entre los que el registro define, o se esta
// escribiendo en un bit reservado. Se comprueba al compilar porque un bit mal
// puesto en una mascara no se nota al escribirlo.
const _: () = assert!(MACB_IRQ_WANTED & !0x3FFF == 0);
// Y las de recepcion y las de transmision no pueden compartir ningun bit: el
// manejador decide que hacer por grupos, y un bit en los dos haria que recoger
// descriptores de transmision contase como trama recibida.
const _: () = assert!(MACB_RX_INT_FLAGS & MACB_TX_INT_FLAGS == 0);

impl MacbMac {
    /// Desarma y reconoce **todo**.
    ///
    /// Es lo primero al coger la tarjeta y lo ultimo al dejarla. El orden no
    /// se puede cambiar: desarmar y luego limpiar deja la linea en paz, y al
    /// contrario un flanco que llegue entre las dos escrituras queda pendiente
    /// con todo armado todavia.
    ///
    /// Esto repite a proposito lo que [`MacbMac::reset_hw`] ya hace: el reset
    /// es del camino de arranque, y esto es tambien del de parada.
    pub fn irq_disable_all(&self) {
        self.write(IDR, MACB_IRQ_ALL);
        let pendientes = self.read(ISR);
        if self.isr_clear_on_write() {
            self.write(ISR, pendientes);
        }
    }

    /// Arma `bits`, y **solo** esos: los demas se quedan como estaban.
    ///
    /// El `IER` es de solo escritura y lo que se le escribe son ordenes, asi
    /// que no hay nada que leer antes ni ningun bit que haya que repetir.
    pub fn irq_enable(&self, bits: u32) {
        self.write(IER, bits);
    }

    /// Desarma `bits`, y solo esos.
    pub fn irq_disable(&self, bits: u32) {
        self.write(IDR, bits);
    }

    /// Lo que esta pendiente **de lo que este driver armo**.
    ///
    /// El `ISR` tiene puesto lo que ha pasado, armado o no, asi que leerlo a
    /// secas atribuye a esta interrupcion causas que nadie armo --y el
    /// manejador las reconoce y las da por atendidas--. El filtro es contra
    /// [`MACB_IRQ_WANTED`], que es lo que el arranque armo y lo unico que se
    /// desarma y se vuelve a armar: es el filtro de `macb_interrupt`, y no
    /// depende de la polaridad del `IMR`, que aqui nadie puede comprobar.
    ///
    /// **Leer el `ISR` puede ser lo que lo limpie** (con `GEM_IRQCOR`
    /// puesto), asi que esto se llama **una vez** por interrupcion y lo que
    /// devuelve es lo unico que se va a saber de ella. Llamarlo dos veces
    /// pierde causas en ese silicio y no en el otro, que es la peor clase de
    /// fallo.
    pub fn irq_status(&self) -> u32 {
        self.read(ISR) & MACB_IRQ_WANTED
    }

    /// Reconoce `bits`, si este silicio se limpia escribiendo.
    ///
    /// Se reconoce **lo que se va a atender**, no todo: un bit reconocido y no
    /// atendido es un evento perdido, y en el caso del `RCOMP` es una trama
    /// que se queda en el anillo hasta que llegue la siguiente.
    ///
    /// Con `GEM_IRQCOR` puesto esto no escribe nada, y no hace falta: la
    /// lectura de [`irq_status`](Self::irq_status) ya limpio el registro.
    pub fn irq_ack(&self, bits: u32) {
        if bits == 0 || !self.isr_clear_on_write() {
            return;
        }
        self.write(ISR, bits);
    }

    /// El arreglo del `RXUBR`: apagar la recepcion y volverla a encender.
    ///
    /// En las dos versiones de silicio con la errata, el DMA de recepcion que
    /// lee un descriptor que sigue siendo del driver **se queda parado** y no
    /// vuelve a mirar el anillo aunque se le devuelvan huecos. Lo unico que lo
    /// saca de ahi es bajar `NCR_RE` y subirlo.
    ///
    /// Se lee el `NCR` y se vuelve a escribir entero **con lo que tenia**, en
    /// vez de usar `modify` dos veces: entre las dos escrituras no puede
    /// cambiar ningun otro bit del registro, y leerlo una sola vez es lo que
    /// lo garantiza. Es la forma de `macb_main.c`.
    ///
    /// **Esto deja la recepcion encendida**, tambien si estaba apagada. Es la
    /// forma de `macb_main.c` y no es un descuido suyo: alli solo se llama
    /// desde el camino del `RXUBR`, y un `RXUBR` solo lo levanta un MAC que
    /// estaba recibiendo. Pero quiere decir que esto **no** es «reiniciar la
    /// recepcion si estaba en marcha»: llamarlo desde otro sitio enciende el
    /// MAC, y encenderlo sin huecos en el anillo es pedirle que desborde su
    /// FIFO.
    ///
    /// El `RXUBR` no esta en [`MACB_IRQ_WANTED`], asi que en el GEM del RP1
    /// esto no lo llama nadie desde el manejador. Esta escrito porque el
    /// arreglo es del MAC y no de la placa, y porque la alternativa --no
    /// escribirlo-- deja la trampa sin documentar para el siguiente que ponga
    /// este driver en otro silicio.
    pub fn rx_restart(&self) {
        let ctrl = self.read(NCR);
        self.write(NCR, ctrl & !NCR_RE);
        self.write(NCR, ctrl | NCR_RE);
    }
}

#[cfg(test)]
mod irq_tests;
