//! Los anillos DMA del GENET v5: la cola por defecto de recepcion y la de
//! transmision, y el camino por el que una trama entra y sale.
//!
//! ## Los descriptores viven en la ventana de registros
//!
//! Esa es la rareza de esta pieza. En un e1000 o en el GMAC del D1 los
//! descriptores son memoria normal que el MAC lee por DMA; aqui son **256
//! descriptores de 12 bytes dentro del propio bloque MMIO** (`0x2000` los de
//! recepcion, `0x4000` los de transmision). Lo que viaja por DMA son solo los
//! buffers a los que apuntan. Asi que escribir un descriptor es escribir un
//! registro: no hace falta vallar nada entre el descriptor y el timbre, y un
//! `DmaRegion` solo hace falta para los buffers.
//!
//! El silicio sabe repartir los 256 descriptores entre diecisiete colas (16 de
//! prioridad y una por defecto). Aqui van **los 256 a la cola por defecto, la
//! 16**, que es el reparto que hace U-Boot y el unico que tiene sentido
//! mientras no haya varias colas de verdad arriba.

use super::*;
use crate::utils::dma::DmaRegion;

// ---------------------------------------------------------------------------
// Mapa de registros de los anillos
// ---------------------------------------------------------------------------

/// Los descriptores de recepcion.
pub const GENET_RX_OFF: usize = 0x2000;
/// Los descriptores de transmision.
pub const GENET_TX_OFF: usize = 0x4000;

/// Descriptores en total, los mismos para rx y para tx.
pub const TOTAL_DESCS: usize = 256;
/// Un descriptor: estado y longitud, direccion baja, direccion alta.
pub const DMA_DESC_SIZE: usize = 12;
pub const DMA_DESC_LENGTH_STATUS: usize = 0x00;
pub const DMA_DESC_ADDRESS_LO: usize = 0x04;
pub const DMA_DESC_ADDRESS_HI: usize = 0x08;

/// La cola por defecto, la unica que este driver usa.
pub const DEFAULT_Q: usize = 0x10;
/// Lo que ocupan los registros de una cola.
pub const DMA_RING_SIZE: usize = 0x40;
/// Lo que ocupan las diecisiete.
pub const DMA_RINGS_SIZE: usize = DMA_RING_SIZE * (DEFAULT_Q + 1);

/// Los registros de las colas van **detras** de los descriptores.
pub const GENET_RDMA_REG_OFF: usize = GENET_RX_OFF + TOTAL_DESCS * DMA_DESC_SIZE;
pub const GENET_TDMA_REG_OFF: usize = GENET_TX_OFF + TOTAL_DESCS * DMA_DESC_SIZE;

/// Los registros de la cola por defecto.
pub const RDMA_RING_REG_BASE: usize = GENET_RDMA_REG_OFF + DEFAULT_Q * DMA_RING_SIZE;
pub const TDMA_RING_REG_BASE: usize = GENET_TDMA_REG_OFF + DEFAULT_Q * DMA_RING_SIZE;

/// Y los del DMA entero, detras de las diecisiete colas.
pub const RDMA_REG_BASE: usize = GENET_RDMA_REG_OFF + DMA_RINGS_SIZE;
pub const TDMA_REG_BASE: usize = GENET_TDMA_REG_OFF + DMA_RINGS_SIZE;

// Los registros de la cola por defecto caen DENTRO de las diecisiete, o sea
// antes de los del DMA: si la cuenta de colas se quedara corta, escribir un
// indice de la cola pisaria el `DMA_CTRL`. Comprobado aqui y no en un test
// porque ninguno de los dos cambia en ejecucion.
const _: () = assert!(RDMA_RING_REG_BASE < RDMA_REG_BASE);
const _: () = assert!(TDMA_RING_REG_BASE < TDMA_REG_BASE);
// Y la ventana que el device tree mapea tiene que dar para el mas alto.
const _: () = assert!(TDMA_REG_BASE + DMA_SCB_BURST_SIZE < 0x1_0000);

// Dentro de los registros de una cola. **El productor y el consumidor estan
// cambiados de sitio entre rx y tx**, porque quien produce no es el mismo: en
// recepcion produce el MAC y consume el driver, en transmision al reves.
pub const DMA_RING_WRITE_PTR: usize = 0x00;
pub const RDMA_PROD_INDEX: usize = RDMA_RING_REG_BASE + 0x08;
pub const RDMA_CONS_INDEX: usize = RDMA_RING_REG_BASE + 0x0C;
pub const TDMA_CONS_INDEX: usize = TDMA_RING_REG_BASE + 0x08;
pub const TDMA_PROD_INDEX: usize = TDMA_RING_REG_BASE + 0x0C;
pub const DMA_RING_BUF_SIZE: usize = 0x10;
pub const DMA_START_ADDR: usize = 0x14;
pub const DMA_END_ADDR: usize = 0x1C;
pub const DMA_MBUF_DONE_THRESH: usize = 0x24;
pub const RDMA_XON_XOFF_THRESH: usize = RDMA_RING_REG_BASE + 0x28;
pub const TDMA_FLOW_PERIOD: usize = TDMA_RING_REG_BASE + 0x28;
pub const DMA_RING_READ_PTR: usize = 0x2C;

// Dentro de los registros del DMA.
pub const DMA_RING_CFG: usize = 0x00;
pub const DMA_CTRL: usize = 0x04;
pub const DMA_SCB_BURST_SIZE: usize = 0x0C;

/// Enciende el DMA.
pub const DMA_EN: u32 = 1 << 0;
/// El primer bit de la mascara de colas encendidas.
pub const DMA_RING_BUF_EN_SHIFT: u32 = 1;
pub const DMA_MAX_BURST_LENGTH: u32 = 0x8;

// Bits del campo de estado de un descriptor.
pub const DMA_BUFLENGTH_SHIFT: u32 = 16;
pub const DMA_BUFLENGTH_MASK: u32 = 0x0FFF;
pub const DMA_RING_SIZE_SHIFT: u32 = 16;
/// El descriptor es del MAC: en recepcion es un hueco listo para una trama.
pub const DMA_OWN: u32 = 0x8000;
pub const DMA_EOP: u32 = 0x4000;
pub const DMA_SOP: u32 = 0x2000;
pub const DMA_TX_APPEND_CRC: u32 = 0x0040;

// Bits del estado de un descriptor de RECEPCION. El MAC marca aqui lo que le
// paso a la trama, y **no mirarlos es entregar arriba una trama que el
// hardware ya ha dicho que esta mal**: ni U-Boot ni esta tanda los miraban.
/// La trama era mas larga de lo que el UMAC acepta.
pub const DMA_RX_LG: u32 = 0x0010;
/// Nibble impar: la trama no acabo en un byte entero.
pub const DMA_RX_NO: u32 = 0x0008;
/// El PHY levanto RX_ER en medio de la trama.
pub const DMA_RX_RXER: u32 = 0x0004;
pub const DMA_RX_CRC_ERROR: u32 = 0x0002;
/// Se desbordo el buffer de recepcion del MAC.
pub const DMA_RX_OV: u32 = 0x0001;
/// Todo lo que hace de una trama una trama que no se entrega.
pub const DMA_RX_ERRORS: u32 = DMA_RX_LG | DMA_RX_NO | DMA_RX_RXER | DMA_RX_CRC_ERROR | DMA_RX_OV;

// Y dos bits que NO son errores, sino de que clase era la trama. Meterlos en
// `DMA_RX_ERRORS` tirarIa **todo el broadcast y todo el multicast**, o sea
// todo el ARP y todo el DHCP: la tarjeta pareceria viva y no hablaria con
// nadie.
pub const DMA_RX_BRDCAST: u32 = 0x0040;
pub const DMA_RX_MULT: u32 = 0x0020;
const _: () = assert!(DMA_RX_ERRORS & (DMA_RX_BRDCAST | DMA_RX_MULT) == 0);
pub const DMA_TX_QTAG_SHIFT: u32 = 7;
/// La etiqueta de cola que el UMAC espera de una trama que no lleva ninguna.
pub const DMA_TX_QTAG_ALL: u32 = 0x3F;

/// Lo que cabe en un buffer de recepcion.
pub const RX_BUF_LENGTH: usize = 2048;
/// El `RBUF_ALIGN_2B` del reset hace que el MAC deje dos bytes delante de la
/// trama, para que la cabecera IP caiga alineada a 4. **Esos dos bytes van
/// dentro de la longitud que el descriptor declara.**
pub const RX_BUF_OFFSET: usize = 2;

/// Los umbrales de control de flujo de la cola de recepcion: XOFF cuando
/// quedan `RX_DESCS >> 4` huecos, XON cuando hay 5 libres.
const DMA_FC_THRESH_HI: u32 = (TOTAL_DESCS >> 4) as u32;
const DMA_FC_THRESH_LO: u32 = 5;
const DMA_FC_THRESH_VALUE: u32 = (DMA_FC_THRESH_LO << 16) | DMA_FC_THRESH_HI;

/// Los indices del hardware son contadores libres de **16 bits**: cuentan
/// tramas, no huecos, y dan la vuelta en `0x10000`, que no es multiplo de 256.
/// Y la mitad ALTA del `RDMA_PROD_INDEX` no es parte del indice: es el
/// contador de tramas que el MAC descarto por no tener hueco
/// (`DMA_P_INDEX_DISCARD_CNT`). Sin la mascara, una sola trama descartada
/// haria que el driver creyera que tiene 65536 esperando y leyera el anillo
/// entero de descriptores que el MAC no ha tocado.
const INDEX_MASK: u32 = 0xFFFF;

/// Trama Ethernet mas larga que se acepta transmitir, sin la FCS, que la pone
/// el MAC (`DMA_TX_APPEND_CRC`).
pub const MAX_TX_FRAME: usize = 1514;

// ---------------------------------------------------------------------------
// Los anillos
// ---------------------------------------------------------------------------

/// Los dos anillos del GENET y los buffers a los que apuntan.
pub struct GenetRings {
    mac: GenetMac,
    /// Los 256 buffers de recepcion, seguidos.
    rx_buf: DmaRegion,
    /// Los 256 buffers de transmision. El GENET transmite por DMA desde donde
    /// se le diga, pero una trama que llega de arriba esta en memoria del
    /// nucleo que puede no ser coherente ni estar donde el MAC alcanza, asi
    /// que se copia a un buffer propio. Es lo que hacen el e1000 y el GMAC del
    /// D1; U-Boot transmite desde el buffer del llamante porque alli solo hay
    /// un llamante.
    tx_buf: DmaRegion,
    /// El descriptor de recepcion por el que va el driver, de 0 a 255.
    rx_slot: usize,
    /// El indice de 16 bits que el driver le ha confirmado al MAC.
    rx_cons: u32,
    tx_slot: usize,
    /// El indice de 16 bits que el driver le ha anunciado al MAC.
    tx_prod: u32,
    /// Tramas que llegaron marcadas como malas por el MAC, o con una longitud
    /// imposible. Lo que `ethtool` llama `rx_errors`.
    rx_errors: u64,
}

impl GenetRings {
    /// Pide los buffers y deja los dos anillos listos y el DMA encendido.
    ///
    /// El MAC tiene que estar ya reseteado ([`GenetMac::reset_umac`]): el reset
    /// vuelve a poner el `RBUF_ALIGN_2B` del que depende [`RX_BUF_OFFSET`].
    pub fn new(mac: GenetMac) -> DeviceResult<Self> {
        let bytes = TOTAL_DESCS * RX_BUF_LENGTH;
        let rx_buf = DmaRegion::alloc_coherent(bytes).ok_or(DeviceError::DmaError)?;
        let tx_buf = DmaRegion::alloc_coherent(bytes).ok_or(DeviceError::DmaError)?;

        let mut rings = Self {
            mac,
            rx_buf,
            tx_buf,
            rx_slot: 0,
            rx_cons: 0,
            tx_slot: 0,
            tx_prod: 0,
            rx_errors: 0,
        };
        rings.disable_dma();
        rings.rx_ring_init();
        rings.rx_descs_init();
        rings.tx_ring_init();
        rings.enable_dma();
        Ok(rings)
    }

    /// El MAC de debajo, para ajustar el enlace sin soltar los anillos.
    pub fn mac(&self) -> &GenetMac {
        &self.mac
    }

    /// Tramas descartadas por venir marcadas como malas o con una longitud
    /// imposible.
    pub fn rx_errors(&self) -> u64 {
        self.rx_errors
    }

    /// Tramas que el MAC tiro por no tener hueco donde ponerlas, que es el
    /// contador que lleva la mitad alta del `RDMA_PROD_INDEX`. Es un contador
    /// del hardware: cuenta desde el arranque y da la vuelta a los 16 bits.
    pub fn rx_discarded(&self) -> u32 {
        (self.mac.read(RDMA_PROD_INDEX) >> 16) & 0xFFFF
    }

    // -----------------------------------------------------------------------
    // Encendido y apagado del DMA
    // -----------------------------------------------------------------------

    /// Apaga los dos DMA y vacia la cola de transmision del UMAC.
    pub fn disable_dma(&self) {
        self.mac.modify(TDMA_REG_BASE + DMA_CTRL, DMA_EN, 0);
        self.mac.modify(RDMA_REG_BASE + DMA_CTRL, DMA_EN, 0);
        self.mac.flush_tx();
    }

    /// Enciende los dos DMA, con la cola por defecto como unica cola activa.
    pub fn enable_dma(&self) {
        let ctrl = (1 << (DEFAULT_Q as u32 + DMA_RING_BUF_EN_SHIFT)) | DMA_EN;
        self.mac.write(TDMA_REG_BASE + DMA_CTRL, ctrl);
        self.mac.modify(RDMA_REG_BASE + DMA_CTRL, 0, ctrl);
    }

    // -----------------------------------------------------------------------
    // Los descriptores
    // -----------------------------------------------------------------------

    fn rx_desc(slot: usize) -> usize {
        GENET_RX_OFF + slot * DMA_DESC_SIZE
    }

    fn tx_desc(slot: usize) -> usize {
        GENET_TX_OFF + slot * DMA_DESC_SIZE
    }

    /// La direccion fisica y virtual del buffer de recepcion de un hueco.
    fn rx_buf_at(&self, slot: usize) -> (usize, usize) {
        let off = slot * RX_BUF_LENGTH;
        (self.rx_buf.vaddr() + off, self.rx_buf.paddr() + off)
    }

    fn tx_buf_at(&self, slot: usize) -> (usize, usize) {
        let off = slot * RX_BUF_LENGTH;
        (self.tx_buf.vaddr() + off, self.tx_buf.paddr() + off)
    }

    /// Deja los 256 descriptores de recepcion apuntando a su buffer y en manos
    /// del MAC.
    fn rx_descs_init(&self) {
        let len_stat = ((RX_BUF_LENGTH as u32) << DMA_BUFLENGTH_SHIFT) | DMA_OWN;
        for slot in 0..TOTAL_DESCS {
            let (_, paddr) = self.rx_buf_at(slot);
            let desc = Self::rx_desc(slot);
            self.mac
                .write(desc + DMA_DESC_ADDRESS_LO, (paddr & 0xFFFF_FFFF) as u32);
            self.mac
                .write(desc + DMA_DESC_ADDRESS_HI, (paddr >> 32) as u32);
            self.mac.write(desc + DMA_DESC_LENGTH_STATUS, len_stat);
        }
    }

    /// Los registros de la cola de recepcion.
    ///
    /// **El indice del productor no se puede poner a cero**: es del MAC y de
    /// solo lectura. Asi que lo que se alinea es el consumidor, sobre el valor
    /// que el productor tenga ya, y el hueco por el que empieza el driver es
    /// el de ese mismo indice. Arrancar en el cero con un productor heredado
    /// de un arranque anterior seria leer como tramas los 256 descriptores que
    /// el MAC todavia no ha tocado.
    fn rx_ring_init(&mut self) {
        self.mac
            .write(RDMA_REG_BASE + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_LENGTH);

        self.mac.write(RDMA_RING_REG_BASE + DMA_START_ADDR, 0);
        self.mac.write(RDMA_RING_REG_BASE + DMA_RING_READ_PTR, 0);
        self.mac.write(RDMA_RING_REG_BASE + DMA_RING_WRITE_PTR, 0);
        // En palabras de 32 bits, y el ultimo indice, no la cuenta.
        self.mac.write(
            RDMA_RING_REG_BASE + DMA_END_ADDR,
            (TOTAL_DESCS * DMA_DESC_SIZE / 4 - 1) as u32,
        );

        let prod = self.mac.read(RDMA_PROD_INDEX) & INDEX_MASK;
        self.mac.write(RDMA_CONS_INDEX, prod);
        self.rx_cons = prod;
        self.rx_slot = (prod as usize) % TOTAL_DESCS;

        self.mac.write(
            RDMA_RING_REG_BASE + DMA_RING_BUF_SIZE,
            ((TOTAL_DESCS as u32) << DMA_RING_SIZE_SHIFT) | RX_BUF_LENGTH as u32,
        );
        self.mac.write(RDMA_XON_XOFF_THRESH, DMA_FC_THRESH_VALUE);
        self.mac.write(RDMA_REG_BASE + DMA_RING_CFG, 1 << DEFAULT_Q);
    }

    /// Los registros de la cola de transmision, con el mismo baile al reves:
    /// aqui el que no se puede poner a cero es el consumidor.
    fn tx_ring_init(&mut self) {
        self.mac
            .write(TDMA_REG_BASE + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_LENGTH);

        self.mac.write(TDMA_RING_REG_BASE + DMA_START_ADDR, 0);
        self.mac.write(TDMA_RING_REG_BASE + DMA_RING_READ_PTR, 0);
        self.mac.write(TDMA_RING_REG_BASE + DMA_RING_WRITE_PTR, 0);
        self.mac.write(
            TDMA_RING_REG_BASE + DMA_END_ADDR,
            (TOTAL_DESCS * DMA_DESC_SIZE / 4 - 1) as u32,
        );

        let cons = self.mac.read(TDMA_CONS_INDEX) & INDEX_MASK;
        self.mac.write(TDMA_PROD_INDEX, cons);
        self.tx_prod = cons;
        self.tx_slot = (cons as usize) % TOTAL_DESCS;

        // Que avise en cuanto termine una trama, no cuando se junten varias:
        // un umbral mas alto deja la ultima trama de una rafaga sin aviso.
        self.mac.write(TDMA_RING_REG_BASE + DMA_MBUF_DONE_THRESH, 1);
        self.mac.write(TDMA_FLOW_PERIOD, 0);
        self.mac.write(
            TDMA_RING_REG_BASE + DMA_RING_BUF_SIZE,
            ((TOTAL_DESCS as u32) << DMA_RING_SIZE_SHIFT) | RX_BUF_LENGTH as u32,
        );
        self.mac.write(TDMA_REG_BASE + DMA_RING_CFG, 1 << DEFAULT_Q);
    }

    // -----------------------------------------------------------------------
    // Recepcion
    // -----------------------------------------------------------------------

    /// Cuantas tramas hay recibidas y sin recoger.
    ///
    /// La resta es de **16 bits**, no de `usize`. Los dos indices son
    /// contadores libres que dan la vuelta en `0x10000`: una resta normal da
    /// un numero enorme justo cuando el productor acaba de dar la vuelta y el
    /// consumidor no, y un driver que comparase `cons < prod` --que es lo que
    /// hace U-Boot-- dejaria de ver tramas en esa vuelta.
    pub fn rx_pending(&self) -> usize {
        let prod = self.mac.read(RDMA_PROD_INDEX) & INDEX_MASK;
        (prod.wrapping_sub(self.rx_cons) & INDEX_MASK) as usize
    }

    pub fn can_recv(&self) -> bool {
        self.rx_pending() != 0
    }

    /// Saca la siguiente trama recibida.
    ///
    /// `NotReady` es que no hay ninguna; `BufferTooSmall` es que `buf` no da
    /// para la trama, y en ese caso **la trama no se descarta**: sigue ahi
    /// para el que venga con sitio.
    pub fn recv(&mut self, buf: &mut [u8]) -> DeviceResult<usize> {
        if self.rx_pending() == 0 {
            return Err(DeviceError::NotReady);
        }

        let desc = Self::rx_desc(self.rx_slot);
        let len_stat = self.mac.read(desc + DMA_DESC_LENGTH_STATUS);

        // Lo primero, los bits de error. Entregar arriba una trama que el MAC
        // ya ha marcado como mala --CRC roto, desbordamiento, RX_ER, nibble
        // impar-- es darle basura a la pila IP, y con un cable marginal eso
        // pasa a diario.
        //
        // Lo que de verdad obliga a mirarlos **aqui** y no mas abajo es el
        // tamaño del buffer del llamante: una trama mala con sitio de sobra da
        // igual donde se mire, pero una trama mala que no cabe se quedaria
        // atascada en el anillo como `BufferTooSmall` para siempre. Respecto a
        // la validacion de la longitud, en cambio, el orden no se nota desde
        // fuera: las dos acaban en `IoError`, y ponerlo antes es solo para que
        // la causa que se cuenta sea la que el hardware dio.
        if len_stat & DMA_RX_ERRORS != 0 {
            self.rx_errors += 1;
            self.rx_release();
            return Err(DeviceError::IoError);
        }

        let declared = ((len_stat >> DMA_BUFLENGTH_SHIFT) & DMA_BUFLENGTH_MASK) as usize;

        // La longitud declarada incluye los dos bytes que el MAC deja delante.
        // Una trama mas corta que eso no existe, y restar sin mirar daria una
        // longitud por debajo de cero convertida en un numero gigante.
        if declared <= RX_BUF_OFFSET || declared > RX_BUF_LENGTH {
            // Un descriptor con una longitud imposible no es una trama: se
            // suelta el hueco y se sigue, que es lo contrario de colgarse en
            // el.
            self.rx_errors += 1;
            self.rx_release();
            return Err(DeviceError::IoError);
        }
        let len = declared - RX_BUF_OFFSET;
        if buf.len() < len {
            return Err(DeviceError::BufferTooSmall);
        }

        let (vaddr, _) = self.rx_buf_at(self.rx_slot);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (vaddr + RX_BUF_OFFSET) as *const u8,
                buf.as_mut_ptr(),
                len,
            );
        }
        self.rx_release();
        Ok(len)
    }

    /// Devuelve el hueco al MAC y avanza.
    ///
    /// El descriptor vuelve a declarar su buffer entero: dejarlo con la
    /// longitud de la trama que acaba de llegar lo convertiria en un hueco de
    /// ese tamaño para siempre.
    fn rx_release(&mut self) {
        let desc = Self::rx_desc(self.rx_slot);
        let len_stat = ((RX_BUF_LENGTH as u32) << DMA_BUFLENGTH_SHIFT) | DMA_OWN;
        self.mac.write(desc + DMA_DESC_LENGTH_STATUS, len_stat);

        self.rx_cons = self.rx_cons.wrapping_add(1) & INDEX_MASK;
        self.mac.write(RDMA_CONS_INDEX, self.rx_cons);
        self.rx_slot = (self.rx_slot + 1) % TOTAL_DESCS;
    }

    // -----------------------------------------------------------------------
    // Transmision
    // -----------------------------------------------------------------------

    /// Cuantas tramas ha aceptado el MAC y todavia no ha terminado.
    pub fn tx_in_flight(&self) -> usize {
        let cons = self.mac.read(TDMA_CONS_INDEX) & INDEX_MASK;
        (self.tx_prod.wrapping_sub(cons) & INDEX_MASK) as usize
    }

    pub fn can_send(&self) -> bool {
        self.tx_in_flight() < TOTAL_DESCS
    }

    /// Pone una trama en la cola de transmision y toca el timbre.
    ///
    /// No espera a que salga: el que la escribio no tiene por que quedarse
    /// mirando el cable. U-Boot si espera, porque alli no hay nadie mas a
    /// quien devolver el control. `NotReady` es que el anillo esta lleno.
    pub fn send(&mut self, frame: &[u8]) -> DeviceResult<usize> {
        if frame.is_empty() || frame.len() > MAX_TX_FRAME {
            return Err(DeviceError::InvalidParam);
        }
        if !self.can_send() {
            return Err(DeviceError::NotReady);
        }

        let (vaddr, paddr) = self.tx_buf_at(self.tx_slot);
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), vaddr as *mut u8, frame.len());
        }

        let len_stat = ((frame.len() as u32) << DMA_BUFLENGTH_SHIFT)
            | (DMA_TX_QTAG_ALL << DMA_TX_QTAG_SHIFT)
            | DMA_TX_APPEND_CRC
            | DMA_SOP
            | DMA_EOP;

        let desc = Self::tx_desc(self.tx_slot);
        self.mac
            .write(desc + DMA_DESC_ADDRESS_LO, (paddr & 0xFFFF_FFFF) as u32);
        self.mac
            .write(desc + DMA_DESC_ADDRESS_HI, (paddr >> 32) as u32);
        // El estado y la longitud, LO ULTIMO: es lo que le dice al MAC que el
        // descriptor esta completo, y escribirlo antes de la direccion le
        // ofreceria una trama que apunta a donde apuntaba la anterior.
        self.mac.write(desc + DMA_DESC_LENGTH_STATUS, len_stat);

        self.tx_slot = (self.tx_slot + 1) % TOTAL_DESCS;
        self.tx_prod = self.tx_prod.wrapping_add(1) & INDEX_MASK;
        self.mac.write(TDMA_PROD_INDEX, self.tx_prod);

        Ok(frame.len())
    }
}

#[cfg(test)]
mod rings_tests;
