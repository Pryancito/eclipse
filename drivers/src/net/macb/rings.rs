//! Los anillos de descriptores del Cadence GEM y el camino de la trama.
//!
//! ## Aqui los descriptores SI viven en RAM
//!
//! Es la diferencia grande con el GENET, donde los 256 descriptores estan
//! dentro de la ventana MMIO y escribir uno es escribir un registro. Aqui son
//! memoria normal que el MAC lee por DMA, como en un e1000: hacen falta
//! regiones coherentes para los anillos **y** para los buffers, y el orden en
//! el que se escriben las dos palabras de un descriptor importa, porque el MAC
//! puede estar mirando la primera mientras se escribe la segunda.
//!
//! ## Las tres trampas de este formato
//!
//! 1. **El bit de propiedad de recepcion vive DENTRO de la palabra de la
//!    direccion** (`MACB_RX_USED`, bit 0), y el de la vuelta del anillo
//!    (`MACB_RX_WRAP`) en el bit 1. Asi que la direccion del buffer son los
//!    **30 bits de arriba**: un buffer que no este alineado a cuatro se
//!    convierte en un descriptor propiedad del MAC con la direccion
//!    truncada. En transmision no es asi: la direccion ocupa la palabra
//!    entera y la propiedad (`MACB_TX_USED`, bit 31) esta en la de control.
//! 2. **Las dos colas tienen convenciones opuestas de donde esta cada cosa**,
//!    y la de transmision necesita un descriptor **centinela**: el MAC
//!    transmite hasta que encuentra uno con `TX_USED` puesto, asi que detras
//!    de la ultima trama tiene que haber uno marcado o seguiria tirando del
//!    anillo entero.
//! 3. **Dar un hueco de recepcion al MAC es escribir su direccion**, porque
//!    eso baja el `RX_USED`. De ahi que el control se ponga a cero **antes**:
//!    al contrario, el MAC puede escribir la trama y leer un control que
//!    todavia es el de la trama anterior.
//!
//! El mapa y los ordenes son los de `macb_main.c` de Linux (`gem_rx_refill`,
//! `gem_rx`, `macb_tx_map`), con el comentario de cada valla apuntado. Como
//! todo lo de este driver: **aqui no hay Pi 5 y QEMU no emula este GEM**, asi
//! que esto corre contra el modelo de [`macb_fake`] y no contra silicio.

use super::*;
use crate::utils::dma::DmaRegion;

// ---------------------------------------------------------------------------
// El descriptor
// ---------------------------------------------------------------------------

/// Las palabras de un descriptor, por su indice.
///
/// `struct macb_dma_desc { u32 addr; u32 ctrl; }`, y en modo de 64 bits detras
/// va `struct macb_dma_desc_64 { u32 addrh; u32 resvd; }`.
pub const DESC_ADDR: usize = 0;
pub const DESC_CTRL: usize = 1;
pub const DESC_ADDRH: usize = 2;

/// Lo que ocupa un descriptor con direcciones de 32 bits, y con las de 64.
pub const DESC_SIZE_32: usize = 8;
pub const DESC_SIZE_64: usize = 16;

// Bits de la palabra de direccion de un descriptor de RECEPCION.
/// `MACB_RX_USED`: el MAC ha escrito aqui una trama. A cero el hueco es suyo.
pub const RX_USED: u32 = 1 << 0;
/// `MACB_RX_WRAP`: detras de este descriptor se vuelve al primero.
pub const RX_WRAP: u32 = 1 << 1;
/// `MACB_RX_WADDR`: los 30 bits de arriba son la direccion del buffer.
pub const RX_ADDR_MASK: u32 = !(RX_USED | RX_WRAP);

// Bits de la palabra de control de un descriptor de RECEPCION.
/// `MACB_RX_FRMLEN`: 12 bits de longitud (14 con jumbo, que no se pide).
pub const RX_FRMLEN_MASK: u32 = 0x0FFF;
/// `MACB_RX_SOF` y `MACB_RX_EOF`: principio y fin de trama.
pub const RX_SOF: u32 = 1 << 14;
pub const RX_EOF: u32 = 1 << 15;

// Bits de la palabra de control de un descriptor de TRANSMISION.
/// `GEM_TX_FRMLEN`: 14 bits en el GEM (11 en el MACB viejo).
pub const TX_FRMLEN_MASK: u32 = 0x3FFF;
/// `MACB_TX_LAST`: ultimo buffer de la trama.
pub const TX_LAST: u32 = 1 << 15;
/// `MACB_TX_NOCRC`: no añadir la FCS. **No se pone**: la trama que llega de
/// arriba no la lleva, asi que el MAC tiene que ponerla.
pub const TX_NOCRC: u32 = 1 << 16;
/// `MACB_TX_BUF_EXHAUSTED`, `MACB_TX_UNDERRUN` y `MACB_TX_ERROR`: lo que el
/// MAC marca de una trama que no llego a salir.
pub const TX_BUF_EXHAUSTED: u32 = 1 << 27;
pub const TX_UNDERRUN: u32 = 1 << 28;
pub const TX_ERROR: u32 = 1 << 29;
pub const TX_ERRORS: u32 = TX_BUF_EXHAUSTED | TX_UNDERRUN | TX_ERROR;
/// `MACB_TX_WRAP`: detras de este descriptor se vuelve al primero.
pub const TX_WRAP: u32 = 1 << 30;
/// `MACB_TX_USED`: el MAC ya ha terminado con este descriptor. Es tambien
/// como se le dice donde acaba la cola.
pub const TX_USED: u32 = 1 << 31;

// Campos de `GEM_DMACFG`.
/// `GEM_FBLDO`: rafaga fija de DMA.
pub const DMACFG_FBLDO_SHIFT: u32 = 0;
pub const DMACFG_FBLDO_MASK: u32 = 0x1F;
/// `GEM_ENDIA_DESC` y `GEM_ENDIA_PKT`: cambiar el orden de los bytes. Los dos
/// van a cero, porque este nucleo es little endian como el bus.
pub const DMACFG_ENDIA_DESC: u32 = 1 << 6;
pub const DMACFG_ENDIA_PKT: u32 = 1 << 7;
/// `GEM_RXBMS`: cuanta memoria del MAC se le da al buffer de recepcion. Todo.
pub const DMACFG_RXBMS_SHIFT: u32 = 8;
pub const DMACFG_RXBMS_MASK: u32 = 0x3;
/// `GEM_TXPBMS`: toda la memoria del MAC para el de transmision.
pub const DMACFG_TXPBMS: u32 = 1 << 10;
/// `GEM_TXCOEN`: calcular la suma de comprobacion en transmision. No se pide.
pub const DMACFG_TXCOEN: u32 = 1 << 11;
/// `GEM_RXBS`: el tamaño del buffer de recepcion, **en multiplos de 64**.
pub const DMACFG_RXBS_SHIFT: u32 = 16;
pub const DMACFG_RXBS_MASK: u32 = 0xFF;
/// `GEM_ADDR64`: las direcciones de los descriptores son de 64 bits, y cada
/// descriptor mide el doble.
pub const DMACFG_ADDR64: u32 = 1 << 30;

/// `RX_BUFFER_MULTIPLE` de Linux: el tamaño del buffer de recepcion se le dice
/// al MAC en multiplos de esto.
pub const RX_BUFFER_MULTIPLE: usize = 64;

/// La rafaga de DMA que pide Linux cuando el arbol no dice otra cosa.
pub const DMA_BURST_LENGTH: u32 = 16;

/// Huecos de recepcion y de transmision. **Tienen que ser potencias de dos**:
/// el indice del anillo se saca con una mascara, como `macb_rx_ring_wrap`.
///
/// Linux usa 512 de cada; aqui 64, que con buffers de 2 KiB son 128 KiB por
/// anillo y es lo que usa el GENET de al lado. Subirlo es cambiar este numero.
pub const RX_RING_SIZE: usize = 64;
pub const TX_RING_SIZE: usize = 64;
const _: () = assert!(RX_RING_SIZE.is_power_of_two());
const _: () = assert!(TX_RING_SIZE.is_power_of_two());

/// Lo que cabe en un buffer de recepcion. Multiplo de 64 porque el campo
/// `RXBS` cuenta en multiplos de 64, y una trama no cabe partida: este driver
/// no junta descriptores, asi que el buffer tiene que dar para la trama mas
/// larga que el MAC acepte.
pub const RX_BUF_LENGTH: usize = 2048;
const _: () = assert!(RX_BUF_LENGTH.is_multiple_of(RX_BUFFER_MULTIPLE));
const _: () = assert!(RX_BUF_LENGTH >= ENET_MAX_FRAME_LEN as usize);

/// Trama Ethernet mas larga que se acepta transmitir, sin la FCS, que la pone
/// el MAC.
pub const MAX_TX_FRAME: usize = 1514;

/// Si un bloque entero cae donde un GEM que direcciona a 32 bits lo alcanza.
///
/// Con `ADDR64` apagado, la palabra de la direccion de un descriptor son 32
/// bits y no hay ninguna otra: una region que empiece o acabe por encima de
/// los 4 GiB **se trunca sin avisar**, y el MAC escribe las tramas en la
/// direccion equivocada. En Eclipse no hay asignador de DMA que garantice
/// estar por debajo de los 4 GiB, asi que lo unico honesto es comprobarlo y
/// **rechazar el arranque** en vez de truncar, que es lo mismo que hace el
/// driver del PCnet.
pub fn reachable_by_32bit_dma(paddr: usize, len: usize) -> bool {
    match paddr.checked_add(len) {
        Some(end) => end <= 1usize << 32,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Un anillo
// ---------------------------------------------------------------------------

/// Cual de los dos anillos, para que el modelo del test sepa de quien es una
/// escritura de descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingKind {
    Rx,
    Tx,
}

/// Un anillo de descriptores en memoria coherente.
struct Ring {
    region: DmaRegion,
    /// Lo que ocupa un descriptor: 8 bytes, o 16 con direcciones de 64 bits.
    stride: usize,
    len: usize,
    kind: RingKind,
}

impl Ring {
    fn alloc(kind: RingKind, len: usize, stride: usize) -> DeviceResult<Self> {
        let region = DmaRegion::alloc_coherent(len * stride).ok_or(DeviceError::DmaError)?;
        Ok(Self {
            region,
            stride,
            len,
            kind,
        })
    }

    fn paddr(&self) -> usize {
        self.region.paddr()
    }

    fn word(&self, slot: usize, word: usize) -> usize {
        debug_assert!(slot < self.len);
        self.region.vaddr() + slot * self.stride + word * 4
    }

    fn read(&self, slot: usize, word: usize) -> u32 {
        unsafe { core::ptr::read_volatile(self.word(slot, word) as *const u32) }
    }

    /// Escribe una palabra de un descriptor.
    ///
    /// Un descriptor es memoria, no un registro, asi que una escritura no deja
    /// rastro: solo queda el ultimo valor. Y de este formato lo que importa es
    /// justo el **orden** --- el control antes de la direccion en recepcion, la
    /// direccion antes del control en transmision ---, asi que esta es la unica
    /// costura `cfg(test)` del fichero, igual que `on_write` lo es del MAC.
    fn write(&self, slot: usize, word: usize, val: u32) {
        unsafe { core::ptr::write_volatile(self.word(slot, word) as *mut u32, val) };
        #[cfg(test)]
        macb_fake::on_desc_write(self.kind, slot, word, val);
    }
}

// ---------------------------------------------------------------------------
// Los dos anillos
// ---------------------------------------------------------------------------

/// Los dos anillos del GEM, sus buffers y por donde va el driver en cada uno.
pub struct MacbRings {
    mac: MacbMac,
    rx: Ring,
    tx: Ring,
    /// Los buffers de recepcion, seguidos.
    rx_buf: DmaRegion,
    /// Los de transmision. El GEM transmite desde donde se le diga, pero una
    /// trama que llega de arriba esta en memoria del nucleo que puede no ser
    /// coherente ni estar donde el MAC alcanza, asi que se copia. Es lo que
    /// hacen el e1000, el GMAC del D1 y el GENET.
    tx_buf: DmaRegion,
    /// Si las direcciones de los descriptores son de 64 bits.
    addr64: bool,
    /// El hueco de recepcion por el que va el driver (`rx_tail` de Linux).
    rx_tail: usize,
    /// El hueco de transmision donde va la siguiente trama, y el primero que
    /// el MAC todavia no ha devuelto.
    tx_head: usize,
    tx_tail: usize,
    /// Tramas descartadas por venir marcadas como malas, partidas o con una
    /// longitud imposible. Lo que `ethtool` llama `rx_errors`.
    rx_errors: u64,
    /// Transmisiones que el MAC marco como fallidas al devolver el descriptor.
    tx_errors: u64,
}

impl MacbRings {
    /// Pide los anillos y los buffers, los deja listos y apunta el MAC a
    /// ellos.
    ///
    /// El MAC tiene que estar ya reseteado y configurado
    /// ([`MacbMac::reset_hw`] y [`MacbMac::init_config`]): de ahi sale el
    /// relleno de dos bytes que el MAC mete delante de la trama, que esta
    /// **dentro** de la longitud que declara el descriptor.
    pub fn new(mac: MacbMac) -> DeviceResult<Self> {
        let addr64 = mac.has_64bit_dma();
        let stride = if addr64 { DESC_SIZE_64 } else { DESC_SIZE_32 };

        let rx = Ring::alloc(RingKind::Rx, RX_RING_SIZE, stride)?;
        let tx = Ring::alloc(RingKind::Tx, TX_RING_SIZE, stride)?;
        let rx_buf =
            DmaRegion::alloc_coherent(RX_RING_SIZE * RX_BUF_LENGTH).ok_or(DeviceError::DmaError)?;
        let tx_buf =
            DmaRegion::alloc_coherent(TX_RING_SIZE * RX_BUF_LENGTH).ok_or(DeviceError::DmaError)?;

        // Con direcciones de 32 bits, una region por encima de los 4 GiB se
        // trunca en silencio: mejor no arrancar.
        if !addr64 {
            for (que, paddr, len) in [
                ("el anillo de recepcion", rx.paddr(), RX_RING_SIZE * stride),
                (
                    "el anillo de transmision",
                    tx.paddr(),
                    TX_RING_SIZE * stride,
                ),
                (
                    "los buffers de recepcion",
                    rx_buf.paddr(),
                    RX_RING_SIZE * RX_BUF_LENGTH,
                ),
                (
                    "los buffers de transmision",
                    tx_buf.paddr(),
                    TX_RING_SIZE * RX_BUF_LENGTH,
                ),
            ] {
                if !reachable_by_32bit_dma(paddr, len) {
                    warn!(
                        "macb: {} esta en {:#x}, donde un DMA de 32 bits no llega",
                        que, paddr
                    );
                    return Err(DeviceError::DmaError);
                }
            }
        }

        let mut rings = Self {
            mac,
            rx,
            tx,
            rx_buf,
            tx_buf,
            addr64,
            rx_tail: 0,
            tx_head: 0,
            tx_tail: 0,
            rx_errors: 0,
            tx_errors: 0,
        };
        rings.configure_dma();
        rings.rx_descs_init();
        rings.tx_descs_init();
        rings.publish_rings();
        Ok(rings)
    }

    /// El MAC de debajo, para ajustar el enlace sin soltar los anillos.
    pub fn mac(&self) -> &MacbMac {
        &self.mac
    }

    pub fn rx_errors(&self) -> u64 {
        self.rx_errors
    }

    pub fn tx_errors(&self) -> u64 {
        self.tx_errors
    }

    // -----------------------------------------------------------------------
    // Arranque
    // -----------------------------------------------------------------------

    /// Escribe `GEM_DMACFG`, que es `macb_configure_dma` de Linux.
    ///
    /// Se **modifica**, no se reescribe: los bits que este driver no nombra
    /// (los de las marcas de tiempo del 1588, por ejemplo) son del silicio y
    /// no de nadie de aqui.
    ///
    /// Los dos bits de orden de bytes se bajan a proposito. Son para un
    /// anfitrion big endian; con uno puesto, el MAC lee la direccion de un
    /// descriptor con los bytes del reves y escribe la trama donde no debe.
    pub fn configure_dma(&self) {
        let buffer_size = (RX_BUF_LENGTH / RX_BUFFER_MULTIPLE) as u32;
        let mut set = ((buffer_size & DMACFG_RXBS_MASK) << DMACFG_RXBS_SHIFT)
            | ((DMA_BURST_LENGTH & DMACFG_FBLDO_MASK) << DMACFG_FBLDO_SHIFT)
            | DMACFG_TXPBMS
            | (DMACFG_RXBMS_MASK << DMACFG_RXBMS_SHIFT);
        if self.addr64 {
            set |= DMACFG_ADDR64;
        }
        let clear = (DMACFG_RXBS_MASK << DMACFG_RXBS_SHIFT)
            | (DMACFG_FBLDO_MASK << DMACFG_FBLDO_SHIFT)
            | DMACFG_ENDIA_DESC
            | DMACFG_ENDIA_PKT
            | DMACFG_TXCOEN
            | DMACFG_ADDR64;
        self.mac.modify(GEM_DMACFG, clear, set);
    }

    /// Le dice al MAC donde estan los dos anillos.
    ///
    /// La mitad alta va **antes** que la baja, por lo mismo que en un
    /// descriptor: lo que el MAC toma como señal es la palabra baja.
    fn publish_rings(&self) {
        if self.addr64 {
            self.mac.write(RBQPH, (self.rx.paddr() >> 32) as u32);
            self.mac.write(TBQPH, (self.tx.paddr() >> 32) as u32);
        }
        self.mac.write(RBQP, self.rx.paddr() as u32);
        self.mac.write(TBQP, self.tx.paddr() as u32);
    }

    /// La direccion virtual y fisica del buffer de un hueco.
    fn rx_buf_at(&self, slot: usize) -> (usize, usize) {
        let off = slot * RX_BUF_LENGTH;
        (self.rx_buf.vaddr() + off, self.rx_buf.paddr() + off)
    }

    fn tx_buf_at(&self, slot: usize) -> (usize, usize) {
        let off = slot * RX_BUF_LENGTH;
        (self.tx_buf.vaddr() + off, self.tx_buf.paddr() + off)
    }

    /// Deja los huecos de recepcion apuntando a su buffer y en manos del MAC.
    fn rx_descs_init(&self) {
        for slot in 0..RX_RING_SIZE {
            self.rx_give_back(slot);
        }
    }

    /// Devuelve un hueco al MAC.
    ///
    /// **El orden es el que importa**: primero el control a cero (y la mitad
    /// alta de la direccion, si la hay) y solo despues la palabra baja de la
    /// direccion, porque esa escritura es la que baja el `RX_USED` y abre el
    /// hueco. Al contrario, el MAC puede escribir una trama y dejar en el
    /// control una longitud que el driver va a pisar, o leer una direccion
    /// alta que todavia no esta.
    fn rx_give_back(&self, slot: usize) {
        let (_, paddr) = self.rx_buf_at(slot);
        debug_assert_eq!(
            paddr % 4,
            0,
            "un buffer sin alinear a cuatro pierde la direccion"
        );
        self.rx.write(slot, DESC_CTRL, 0);
        if self.addr64 {
            self.rx.write(slot, DESC_ADDRH, (paddr >> 32) as u32);
        }
        let mut addr = (paddr as u32) & RX_ADDR_MASK;
        if slot == RX_RING_SIZE - 1 {
            addr |= RX_WRAP;
        }
        self.rx.write(slot, DESC_ADDR, addr);
    }

    /// Deja la cola de transmision vacia: todos los descriptores en manos del
    /// driver (`TX_USED`) y el ultimo con la marca de la vuelta.
    ///
    /// Que `TX_USED` signifique "del driver" aqui y "del MAC" en recepcion no
    /// es un descuido del silicio: en los dos casos el bit puesto quiere decir
    /// *el MAC ha terminado con este descriptor*. Lo que cambia es **en que
    /// palabra esta** --- la de control en transmision, la de la direccion en
    /// recepcion ---, y por eso las dos colas se escriben al reves la una de la
    /// otra.
    fn tx_descs_init(&self) {
        for slot in 0..TX_RING_SIZE {
            self.tx.write(slot, DESC_ADDR, 0);
            if self.addr64 {
                self.tx.write(slot, DESC_ADDRH, 0);
            }
            let mut ctrl = TX_USED;
            if slot == TX_RING_SIZE - 1 {
                ctrl |= TX_WRAP;
            }
            self.tx.write(slot, DESC_CTRL, ctrl);
        }
    }

    // -----------------------------------------------------------------------
    // Recepcion
    // -----------------------------------------------------------------------

    /// Si el MAC ha dejado una trama en el hueco por el que va el driver.
    ///
    /// No hay ningun indice que preguntar: lo unico que dice que hay trama es
    /// el `RX_USED` del descriptor, que esta en la palabra de la direccion.
    pub fn can_recv(&self) -> bool {
        self.rx.read(self.rx_tail, DESC_ADDR) & RX_USED != 0
    }

    /// Saca la siguiente trama recibida.
    ///
    /// `NotReady` es que no hay ninguna; `BufferTooSmall` es que `buf` no da
    /// para la trama, y en ese caso **la trama no se descarta**: sigue ahi
    /// para el que venga con sitio.
    pub fn recv(&mut self, buf: &mut [u8]) -> DeviceResult<usize> {
        if !self.can_recv() {
            return Err(DeviceError::NotReady);
        }
        let slot = self.rx_tail;
        let ctrl = self.rx.read(slot, DESC_CTRL);

        // Una trama que no empieza y acaba en el mismo descriptor es una trama
        // partida en varios, y este driver no las junta: el buffer de 2 KiB da
        // para cualquier trama que el MAC acepte, asi que si llega partida es
        // que algo no cuadra. Se descarta y se cuenta, que es lo que hace
        // Linux (`not whole frame pointed by descriptor`).
        //
        // Va **antes** de mirar el tamaño del buffer del llamante a proposito:
        // si no, una trama partida con sitio de sobra se entregaria entera y
        // una sin sitio se quedaria atascada para siempre en el anillo.
        let entera = ctrl & RX_SOF != 0 && ctrl & RX_EOF != 0;
        let len = (ctrl & RX_FRMLEN_MASK) as usize;
        if !entera || len <= RX_IP_ALIGN as usize || len > RX_BUF_LENGTH {
            warn!(
                "macb: trama descartada en el hueco {}: ctrl={:#x}, len={}",
                slot, ctrl, len
            );
            self.rx_errors += 1;
            self.rx_advance(slot);
            return Err(DeviceError::IoError);
        }

        // El MAC deja `RX_IP_ALIGN` bytes delante de la trama para que la
        // cabecera IP caiga alineada a cuatro, y esos bytes **estan dentro** de
        // la longitud que declara el descriptor.
        let len = len - RX_IP_ALIGN as usize;
        if buf.len() < len {
            return Err(DeviceError::BufferTooSmall);
        }
        let (vaddr, _) = self.rx_buf_at(slot);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (vaddr + RX_IP_ALIGN as usize) as *const u8,
                buf.as_mut_ptr(),
                len,
            );
        }
        self.rx_advance(slot);
        Ok(len)
    }

    /// Devuelve el hueco al MAC y pasa al siguiente.
    fn rx_advance(&mut self, slot: usize) {
        self.rx_give_back(slot);
        self.rx_tail = (self.rx_tail + 1) & (RX_RING_SIZE - 1);
    }

    // -----------------------------------------------------------------------
    // Transmision
    // -----------------------------------------------------------------------

    /// Huecos de transmision que el MAC todavia no ha devuelto.
    fn tx_in_flight(&self) -> usize {
        self.tx_head.wrapping_sub(self.tx_tail) & (TX_RING_SIZE - 1)
    }

    /// Si cabe otra trama.
    ///
    /// Se deja **un hueco libre** siempre: el que va detras de la ultima trama
    /// es el centinela con `TX_USED` donde el MAC para, y sin el no habria
    /// forma de distinguir un anillo lleno de uno vacio.
    pub fn can_send(&self) -> bool {
        self.tx_in_flight() < TX_RING_SIZE - 1
    }

    /// Pone una trama en la cola de transmision y toca el timbre.
    ///
    /// No espera a que salga: el que la escribio no tiene por que quedarse
    /// mirando el cable. `NotReady` es que el anillo esta lleno.
    pub fn send(&mut self, frame: &[u8]) -> DeviceResult<usize> {
        if frame.is_empty() || frame.len() > MAX_TX_FRAME {
            return Err(DeviceError::InvalidParam);
        }
        self.reclaim_tx();
        if !self.can_send() {
            return Err(DeviceError::NotReady);
        }

        let slot = self.tx_head;
        let (vaddr, paddr) = self.tx_buf_at(slot);
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), vaddr as *mut u8, frame.len());
        }

        // El centinela primero: el MAC transmite hasta que encuentra un
        // descriptor con `TX_USED`, asi que el de detras tiene que estar
        // marcado **antes** de soltar este, o seguiria tirando del anillo
        // entero. Es el orden inverso de `macb_tx_map`, que escribe los
        // descriptores de atras adelante por lo mismo.
        let siguiente = (slot + 1) & (TX_RING_SIZE - 1);
        let mut centinela = TX_USED;
        if siguiente == TX_RING_SIZE - 1 {
            centinela |= TX_WRAP;
        }
        self.tx.write(siguiente, DESC_CTRL, centinela);

        // Y en este, la direccion antes del control: escribir el control es lo
        // que baja el `TX_USED` y suelta el descriptor, asi que la direccion
        // tiene que estar visible ya.
        if self.addr64 {
            self.tx.write(slot, DESC_ADDRH, (paddr >> 32) as u32);
        }
        self.tx.write(slot, DESC_ADDR, paddr as u32);
        let mut ctrl = (frame.len() as u32 & TX_FRMLEN_MASK) | TX_LAST;
        if slot == TX_RING_SIZE - 1 {
            ctrl |= TX_WRAP;
        }
        self.tx.write(slot, DESC_CTRL, ctrl);

        self.tx_head = siguiente;
        self.start_tx();
        Ok(frame.len())
    }

    /// Toca el timbre: `TSTART` de `NCR`.
    ///
    /// Es read-modify-write porque `NCR` lleva tambien las dos habilitaciones
    /// y el puerto de mantenimiento, y un `write` pelado apagaria la tarjeta
    /// con una trama recien puesta en la cola.
    fn start_tx(&self) {
        self.mac.modify(NCR, 0, NCR_TSTART);
    }

    /// Recoge los descriptores que el MAC ya ha devuelto.
    ///
    /// Devuelve cuantos. El MAC pone `TX_USED` al terminar con uno, y el
    /// centinela lleva ese bit puesto desde antes: por eso se para en
    /// `tx_head` y no en el primer descriptor marcado.
    pub fn reclaim_tx(&mut self) -> usize {
        let mut n = 0;
        while self.tx_tail != self.tx_head {
            let ctrl = self.tx.read(self.tx_tail, DESC_CTRL);
            if ctrl & TX_USED == 0 {
                break;
            }
            if ctrl & TX_ERRORS != 0 {
                warn!(
                    "macb: la transmision del hueco {} fallo: ctrl={:#x}",
                    self.tx_tail, ctrl
                );
                self.tx_errors += 1;
            }
            self.tx_tail = (self.tx_tail + 1) & (TX_RING_SIZE - 1);
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod rings_tests;
