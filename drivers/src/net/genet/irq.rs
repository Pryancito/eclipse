//! El controlador de interrupciones del GENET: los dos bloques `INTRL2`.
//!
//! El GENET no tiene un registro de estado de interrupciones y una mascara,
//! sino **dos juegos de seis registros** (`INTRL2_0` para el MAC y el DMA de
//! la cola por defecto, `INTRL2_1` para las dieciseis colas de prioridad), y
//! cada uno se maneja por **set/clear**: no se escribe la mascara que uno
//! quiere, se escribe que bits poner y que bits quitar. Escribir la mascara
//! entera en `MASK_SET` desenmascara lo que no esta en ella, que es justo lo
//! contrario de lo que parece.
//!
//! Los offsets y los bits son los de `bcmgenet.h` de Linux; de esta pieza del
//! BCM2711 no hay hoja de datos publica. U-Boot no toca el `INTRL2` para nada
//! --sondea el anillo-- asi que aqui no hay divergencia que documentar: no hay
//! con que divergir.

use super::GenetMac;

/// Las interrupciones del MAC y de la cola por defecto.
pub const GENET_INTRL2_0_OFF: usize = 0x0200;
/// Las de las dieciseis colas de prioridad, que este driver no usa.
pub const GENET_INTRL2_1_OFF: usize = 0x0240;

/// Lo que esta pendiente, puesto por el hardware.
pub const INTRL2_CPU_STAT: usize = 0x00;
/// Levanta a mano una interrupcion (solo para probar el camino).
pub const INTRL2_CPU_SET: usize = 0x04;
/// Escribir un bit aqui es **reconocer** esa interrupcion.
pub const INTRL2_CPU_CLEAR: usize = 0x08;
/// Que esta enmascarado ahora mismo. **Uno es enmascarado**, no al contrario.
pub const INTRL2_CPU_MASK_STATUS: usize = 0x0C;
/// Escribir un bit aqui **enmascara** esa interrupcion.
pub const INTRL2_CPU_MASK_SET: usize = 0x10;
/// Escribir un bit aqui la **desenmascara**.
pub const INTRL2_CPU_MASK_CLEAR: usize = 0x14;

// Los seis registros de cada bloque tienen que caber antes del bloque
// siguiente: si el bloque 0 se solapara con el 1, una escritura de mascara del
// MAC caeria en el estado de las colas, y el bloque 1 pisaria el RBUF. Se
// comprueba al compilar porque un offset mal puesto no se nota al escribirlo.
const _: () = assert!(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_CLEAR + 4 <= GENET_INTRL2_1_OFF);
const _: () = assert!(GENET_INTRL2_1_OFF + INTRL2_CPU_MASK_CLEAR + 4 <= super::RBUF_CTRL);

// Los bits del `INTRL2_0`.
pub const UMAC_IRQ_SCB: u32 = 1 << 0;
pub const UMAC_IRQ_EPHY: u32 = 1 << 1;
pub const UMAC_IRQ_PHY_DET_R: u32 = 1 << 2;
pub const UMAC_IRQ_PHY_DET_F: u32 = 1 << 3;
pub const UMAC_IRQ_LINK_UP: u32 = 1 << 4;
pub const UMAC_IRQ_LINK_DOWN: u32 = 1 << 5;
pub const UMAC_IRQ_LINK_EVENT: u32 = UMAC_IRQ_LINK_UP | UMAC_IRQ_LINK_DOWN;
pub const UMAC_IRQ_UMAC: u32 = 1 << 6;
pub const UMAC_IRQ_UMAC_TSV: u32 = 1 << 7;
/// Se vacio el buffer de transmision del MAC con la trama a medias.
pub const UMAC_IRQ_TBUF_UNDERRUN: u32 = 1 << 8;
/// Se desbordo el de recepcion: tramas perdidas dentro del MAC.
pub const UMAC_IRQ_RBUF_OVERFLOW: u32 = 1 << 9;
pub const UMAC_IRQ_HFB_SM: u32 = 1 << 10;
pub const UMAC_IRQ_HFB_MM: u32 = 1 << 11;
pub const UMAC_IRQ_MPD_R: u32 = 1 << 12;
/// El DMA de recepcion ha terminado un lote de descriptores. **Esta es la de
/// recepcion**: las otras dos (`PDONE`, `BDONE`) son del modo por paquete y
/// por byte, que este driver no arma.
pub const UMAC_IRQ_RXDMA_MBDONE: u32 = 1 << 13;
pub const UMAC_IRQ_RXDMA_PDONE: u32 = 1 << 14;
pub const UMAC_IRQ_RXDMA_BDONE: u32 = 1 << 15;
pub const UMAC_IRQ_RXDMA_DONE: u32 = UMAC_IRQ_RXDMA_MBDONE;
pub const UMAC_IRQ_TXDMA_MBDONE: u32 = 1 << 16;
pub const UMAC_IRQ_TXDMA_PDONE: u32 = 1 << 17;
pub const UMAC_IRQ_TXDMA_BDONE: u32 = 1 << 18;
pub const UMAC_IRQ_TXDMA_DONE: u32 = UMAC_IRQ_TXDMA_MBDONE;
pub const UMAC_IRQ_MDIO_DONE: u32 = 1 << 23;
pub const UMAC_IRQ_MDIO_ERROR: u32 = 1 << 24;

/// Todas, para enmascarar o reconocer de golpe.
pub const UMAC_IRQ_ALL: u32 = 0xFFFF_FFFF;

/// Lo que este driver quiere que le despierte.
///
/// El `MBDONE` de cada DMA y los dos del enlace, que es lo mismo que arma
/// Linux. Los de error del MAC (`TBUF_UNDERRUN`, `RBUF_OVERFLOW`) quedan
/// enmascarados a proposito: no hay nada que hacer con ellos que no sea
/// contarlos, y el MAC ya los cuenta en los MIB; armarlos solo añadiria
/// interrupciones en el momento en que la tarjeta esta mas ocupada.
///
/// El `MDIO_DONE` tampoco: el bus MDIO se usa con un plazo en el camino de
/// arranque y del enlace, no por interrupcion.
pub const GENET_IRQ_WANTED: u32 = UMAC_IRQ_RXDMA_DONE | UMAC_IRQ_TXDMA_DONE | UMAC_IRQ_LINK_EVENT;

impl GenetMac {
    #[inline]
    fn intrl2_0(&self, reg: usize) -> usize {
        GENET_INTRL2_0_OFF + reg
    }

    #[inline]
    fn intrl2_1(&self, reg: usize) -> usize {
        GENET_INTRL2_1_OFF + reg
    }

    /// Enmascara y reconoce **todo**, en los dos bloques.
    ///
    /// Es lo primero que hay que hacer al coger la tarjeta y lo ultimo al
    /// dejarla: el GENET puede venir del cargador de arranque con
    /// interrupciones armadas y pendientes, y entonces la linea esta levantada
    /// antes de que el manejador exista. Enmascarar sin reconocer no basta: el
    /// bit pendiente sigue ahi y salta en cuanto se desenmascare algo.
    ///
    /// El `INTRL2_1` tambien, aunque este driver no use las colas de
    /// prioridad: comparten la misma linea del GIC, asi que un bit pendiente
    /// de una cola que nadie atiende cuelga la linea igual.
    pub fn irq_disable_all(&self) {
        self.write(self.intrl2_0(INTRL2_CPU_MASK_SET), UMAC_IRQ_ALL);
        self.write(self.intrl2_0(INTRL2_CPU_CLEAR), UMAC_IRQ_ALL);
        self.write(self.intrl2_1(INTRL2_CPU_MASK_SET), UMAC_IRQ_ALL);
        self.write(self.intrl2_1(INTRL2_CPU_CLEAR), UMAC_IRQ_ALL);
    }

    /// Desenmascara `bits` del `INTRL2_0`, y **solo** esos: los demas se
    /// quedan como estaban.
    pub fn irq_unmask(&self, bits: u32) {
        self.write(self.intrl2_0(INTRL2_CPU_MASK_CLEAR), bits);
    }

    /// Enmascara `bits` del `INTRL2_0`, y solo esos.
    pub fn irq_mask(&self, bits: u32) {
        self.write(self.intrl2_0(INTRL2_CPU_MASK_SET), bits);
    }

    /// Lo que esta pendiente **y no enmascarado**.
    ///
    /// El `CPU_STAT` tiene puesto lo que ha pasado, enmascarado o no, asi que
    /// leerlo a secas atribuye a esta interrupcion causas que nadie armo
    /// --y el manejador las reconoce y las da por atendidas--. La resta con el
    /// `MASK_STATUS` es la que hace Linux.
    pub fn irq_status(&self) -> u32 {
        let stat = self.read(self.intrl2_0(INTRL2_CPU_STAT));
        let masked = self.read(self.intrl2_0(INTRL2_CPU_MASK_STATUS));
        stat & !masked
    }

    /// Reconoce `bits`.
    ///
    /// Se reconoce **lo que se va a atender**, no todo: un bit reconocido y no
    /// atendido es un evento perdido, y en el caso del `RXDMA_DONE` es una
    /// trama que se queda en el anillo hasta que llegue la siguiente.
    pub fn irq_ack(&self, bits: u32) {
        if bits == 0 {
            return;
        }
        self.write(self.intrl2_0(INTRL2_CPU_CLEAR), bits);
    }

    /// Lo que el `INTRL2_1` tiene pendiente, para el unico sitio donde hace
    /// falta: reconocerlo si alguna vez aparece algo.
    pub fn irq1_status(&self) -> u32 {
        self.read(self.intrl2_1(INTRL2_CPU_STAT))
    }

    /// Reconoce `bits` del `INTRL2_1`.
    pub fn irq1_ack(&self, bits: u32) {
        if bits == 0 {
            return;
        }
        self.write(self.intrl2_1(INTRL2_CPU_CLEAR), bits);
    }
}
