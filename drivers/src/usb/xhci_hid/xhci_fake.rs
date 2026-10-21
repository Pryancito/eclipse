//! Un controlador xHCI en memoria del host, para los tests de este módulo.
//!
//! Los accesos MMIO del driver leen y escriben memoria corriente en el
//! binario de test; este módulo es dueño de esa memoria y se entera de cada
//! acceso (`on_read`/`on_write`), que es lo que hace que los registros se
//! comporten como los de un controlador: HCRST se limpia solo, HCHalted sigue
//! a RS, los bits de cambio de PORTSC son write-1-to-clear, y un timbre en el
//! doorbell 0 consume el anillo de comandos y contesta cada TRB con su Command
//! Completion Event en el anillo de eventos que el driver haya programado.
//!
//! Detrás de un puerto puede haber además un **dispositivo** ([`FakeDev`])
//! que contesta por EP0: el fake lee el contexto de entrada del Address
//! Device para saber en qué anillo de transferencia escucha ese slot, y a
//! partir de ahí cada timbre en el doorbell del slot consume las etapas
//! Setup/Data/Status de una petición de control, escribe en el buffer del
//! driver lo que el dispositivo tenga que decir y contesta con su Transfer
//! Event. Eso es lo que convierte `setup_device` de una función que no se
//! podía llamar en una que enumera.
//!
//! Lo que un controlador hace *sin* que nadie se lo pida, el fake lo hace
//! cuando el test se lo pide: [`Hw::connect`] conecta un dispositivo a un
//! puerto, [`Hw::answer_commands_with`] cambia el código de terminación, y
//! [`Hw::go_deaf`] deja de contestar, que es lo que el driver ve cuando el
//! controlador se ha ido.
//!
//! El reloj es el reloj de test por hilo que hay detrás de
//! `timer_now_as_micros`, y lo mueve cada lectura de registro: así un bucle
//! que espera un bit que no va a cambiar llega a su plazo en vez de girar para
//! siempre. Un fake por hilo, así que los tests corren en paralelo.

use super::*;
use crate::nvme::nvme_queue::test_clock;
use core::cell::RefCell;

extern crate std;

/// Offsets dentro de la BAR que el fake y los tests comparten.
pub(super) const CAPLEN: usize = 0x20;
pub(super) const RTSOFF: usize = 0x1000;
pub(super) const DBOFF: usize = 0x1800;
pub(super) const BAR_SIZE: usize = 0x2000;

/// Registros, como offsets dentro de su ventana.
const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_CRCR: usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;
const OP_PORTSC: usize = 0x400;

const RT_IMAN: usize = 0x20;
const RT_ERSTSZ: usize = 0x28;
const RT_ERSTBA: usize = 0x30;
const RT_ERDP: usize = 0x38;

const USBCMD_RS: u32 = 1;
const USBCMD_HCRST: u32 = 1 << 1;
const USBSTS_HCH: u32 = 1;
const USBSTS_CNR: u32 = 1 << 11;

/// Stall Error: lo que un dispositivo contesta a lo que no entiende.
const TRB_CC_STALL: u32 = 6;
/// Parameter Error: el contexto que lleva el comando no vale.
const TRB_CC_PARAM: u32 = 17;

/// Tipos de TRB, ya desplazados a su campo.
const TY_NORMAL: u32 = 1;
const TY_SETUP: u32 = 2;
const TY_DATA: u32 = 3;
const TY_STATUS: u32 = 4;
const TY_LINK: u32 = TRB_LINK >> 10;

/// Un comando que el controlador consumió del anillo de comandos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SeenCmd {
    /// Tipo de TRB (bits 15:10 del dword de control).
    pub ty: u32,
    /// Dirección física del propio TRB, que es lo que el evento de
    /// terminación lleva en su campo de parámetro.
    pub phys: u64,
    /// El parámetro del TRB: para Address Device y Configure Endpoint, el
    /// contexto de entrada.
    pub param: u64,
    /// Slot al que iba dirigido (0 en Enable Slot).
    pub slot: u8,
    /// Endpoint (DCI) al que iba dirigido, cuando el comando lleva uno.
    pub dci: u8,
}

/// Un puerto raíz del controlador.
#[derive(Clone, Copy, Default)]
struct Port {
    connected: bool,
    /// Código de velocidad de PORTSC (xHCI §5.4.8); 0 si no hay nada.
    speed: u8,
    /// Lo que PORTSC lee ahora mismo. El fake se entera después de que el
    /// valor escrito ya esté en memoria, así que el valor *anterior* --que es
    /// contra el que se resuelven los bits write-1-to-clear-- tiene que estar
    /// aquí y no en la memoria.
    sc: u32,
}

/// Los ocho bytes de una petición de control, tal y como van en el parámetro
/// del TRB de Setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Setup {
    pub bmrt: u8,
    pub breq: u8,
    pub wvalue: u16,
    pub windex: u16,
    pub wlen: u16,
}

impl Setup {
    fn from_param(p: u64) -> Self {
        Self {
            bmrt: p as u8,
            breq: (p >> 8) as u8,
            wvalue: (p >> 16) as u16,
            windex: (p >> 32) as u16,
            wlen: (p >> 48) as u16,
        }
    }
}

/// Un puerto de un hub de los de abajo.
#[derive(Clone, Copy, Default)]
struct HubPort {
    connected: bool,
    /// Velocidad de PORTSC del dispositivo enchufado.
    speed: u8,
    /// `wPortStatus` tal y como lo devuelve `GET_STATUS`.
    status: u16,
    /// `wPortChange`, con sus bits de cambio.
    change: u16,
    /// Lecturas de estado que le quedan con el reset todavía puesto.
    reset_reads: u8,
}

/// Lo que un hub tiene además de ser un dispositivo.
pub(super) struct HubState {
    ports: Vec<HubPort>,
    /// `bPwrOn2PwrGood` del descriptor, en unidades de 2 ms.
    power_good: u8,
    /// Un hub cuyos puertos no se habilitan al resetearlos.
    never_enables: bool,
    /// Lecturas de estado durante las que un puerto reseteado sigue
    /// señalando el reset, habilitado o no.
    slow_reset: u8,
}

/// El `dci` con el que la unidad encola lo que sale por su endpoint bulk IN:
/// el fake no lo sabe hasta que el driver lo configura, y no hace falta, porque
/// los DCI impares son IN y los pares OUT (`dci = 2*ep + dirección`).
const DCI_BULK_IN: u8 = 0xff;

/// Las órdenes SCSI que la unidad falsa entiende.
const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;
const SCSI_WRITE_10: u8 = 0x2a;
const SCSI_SYNCHRONIZE_CACHE_10: u8 = 0x35;
const SCSI_SERVICE_ACTION_IN_16: u8 = 0x9e;

const BOT_CBW_SIGNATURE: u32 = 0x4342_5355;
const BOT_CSW_SIGNATURE: u32 = 0x5342_5355;

/// Una unidad de almacenamiento masivo sobre Bulk-Only Transport: su medio,
/// lo que contesta a cada orden y los moldes para que conteste mal.
///
/// El transporte es el de verdad (USB MSC BOT §5): por el endpoint OUT llega
/// un CBW de 31 bytes, por el que toque va la fase de datos, y por el IN sale
/// un CSW de 13 con el mismo `tag`. El fake lo lleva como máquina de estados
/// porque es justo ahí donde el driver se puede equivocar: creerse un CSW de
/// otro tag es leer el resultado de otra orden.
pub(super) struct DiskState {
    /// El medio, bloque a bloque.
    media: Vec<u8>,
    block_size: u32,
    blocks: u64,
    vendor: [u8; 8],
    product: [u8; 16],
    revision: [u8; 4],
    removable: bool,
    /// Unidades lógicas menos una, que es lo que contesta `GET_MAX_LUN`.
    max_lun: u8,
    /// `TEST UNIT READY` falla estas veces más antes de decir que sí.
    not_ready: u32,
    /// Esta orden se rechaza con CSW status 1.
    fail_op: Option<u8>,
    /// `READ CAPACITY(10)` contesta el último LBA saturado, que es como una
    /// unidad grande dice «pregúntame con la de 16».
    needs_16: bool,
    /// `READ CAPACITY(10)` contesta un tamaño de bloque de cero, que es lo
    /// que da una unidad con el medio a medio reconocer: la orden va bien y
    /// lo que contesta no se puede usar.
    bad_block_size: bool,
    /// El CSW lleva un tag que no es el del CBW.
    wrong_tag: bool,
    /// La fase de datos de una lectura entrega estos bytes de menos, con el
    /// CSW diciendo que todo fue bien: eso es un sector a medias.
    short_by: usize,
    /// Cada CDB que recibió, en orden.
    pub cdbs: Vec<Vec<u8>>,
    /// Una fase de datos OUT a medias: `(lba, bytes que faltan, tag, pedidos)`.
    pending_write: Option<(u64, usize, u32, u32)>,
}

impl DiskState {
    fn new(blocks: u64, block_size: u32) -> Self {
        Self {
            media: alloc::vec![0u8; (blocks * block_size as u64) as usize],
            block_size,
            blocks,
            vendor: *b"Eclipse ",
            product: *b"Pendrive Falso  ",
            revision: *b"0001",
            removable: true,
            max_lun: 0,
            not_ready: 0,
            fail_op: None,
            needs_16: false,
            bad_block_size: false,
            wrong_tag: false,
            short_by: 0,
            cdbs: Vec::new(),
            pending_write: None,
        }
    }

    /// Lo que hay escrito en un bloque, para comprobar una escritura.
    fn block(&self, lba: u64) -> Vec<u8> {
        let off = (lba * self.block_size as u64) as usize;
        let end = (off + self.block_size as usize).min(self.media.len());
        self.media.get(off..end).unwrap_or(&[]).to_vec()
    }

    fn put_block(&mut self, lba: u64, data: &[u8]) {
        let off = (lba * self.block_size as u64) as usize;
        for (i, b) in data.iter().enumerate() {
            if let Some(slot) = self.media.get_mut(off + i) {
                *slot = *b;
            }
        }
    }

    /// Los 36 bytes de `INQUIRY`.
    fn inquiry(&self) -> Vec<u8> {
        let mut d = alloc::vec![0u8; 36];
        d[0] = 0; // Direct access block device.
        d[1] = if self.removable { 0x80 } else { 0 };
        d[2] = 0x04; // SPC-2.
        d[3] = 0x02;
        d[4] = 31; // Additional length.
        d[8..16].copy_from_slice(&self.vendor);
        d[16..32].copy_from_slice(&self.product);
        d[32..36].copy_from_slice(&self.revision);
        d
    }

    /// Los 8 bytes de `READ CAPACITY(10)`: último LBA y tamaño de bloque, los
    /// dos en big endian, que es como habla SCSI.
    fn capacity10(&self) -> Vec<u8> {
        let last = if self.needs_16 {
            0xffff_ffffu32
        } else {
            self.blocks.saturating_sub(1).min(0xffff_fffe) as u32
        };
        let mut d = alloc::vec![0u8; 8];
        d[0..4].copy_from_slice(&last.to_be_bytes());
        let bs = if self.bad_block_size {
            0
        } else {
            self.block_size
        };
        d[4..8].copy_from_slice(&bs.to_be_bytes());
        d
    }

    /// Los 32 de `READ CAPACITY(16)`: el último LBA cabe en ocho bytes.
    fn capacity16(&self) -> Vec<u8> {
        let mut d = alloc::vec![0u8; 32];
        d[0..8].copy_from_slice(&self.blocks.saturating_sub(1).to_be_bytes());
        d[8..12].copy_from_slice(&self.block_size.to_be_bytes());
        d
    }

    /// Los 18 de `REQUEST SENSE`, con la condición que explica un «no lista».
    fn sense(&self) -> Vec<u8> {
        let mut d = alloc::vec![0u8; 18];
        d[0] = 0x70; // Current error, fixed format.
        d[2] = 0x02; // NOT READY.
        d[7] = 10; // Additional length.
        d[12] = 0x3a; // MEDIUM NOT PRESENT.
        d
    }
}

/// `(lba, bloques)` de un READ(10) o WRITE(10).
fn rw10_range(cdb: &[u8]) -> Option<(u64, u32)> {
    if cdb.len() < 10 {
        return None;
    }
    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
    let blocks = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
    Some((lba, blocks))
}

/// Los 13 bytes de un CSW. `wrong_tag` contesta con el tag de al lado, que es
/// el fallo que el driver tiene que cazar: un CSW de otra orden.
fn bot_csw(tag: u32, residue: u32, status: u8, wrong_tag: bool) -> Vec<u8> {
    let mut d = alloc::vec![0u8; 13];
    d[0..4].copy_from_slice(&BOT_CSW_SIGNATURE.to_le_bytes());
    d[4..8].copy_from_slice(&tag.wrapping_add(u32::from(wrong_tag)).to_le_bytes());
    d[8..12].copy_from_slice(&residue.to_le_bytes());
    d[12] = status;
    d
}

/// Un dispositivo contestando por EP0. Los descriptores los pone el test:
/// el fake no se inventa ninguno, sólo los entrega recortados a `wLength`
/// como lo haría el dispositivo.
pub(super) struct FakeDev {
    /// Descriptor de dispositivo (`GET_DESCRIPTOR` tipo 1).
    pub device: Vec<u8>,
    /// Configuración entera, cabecera incluida (tipo 2).
    pub config: Vec<u8>,
    /// Descriptor de informe HID (tipo 0x22, destinatario interfaz).
    pub report: Vec<u8>,
    /// Peticiones que este dispositivo rechaza, como `(bmRequestType, bRequest)`.
    pub stall: Vec<(u8, u8)>,
    /// Peticiones que deja sin contestar, igual.
    pub silent: Vec<(u8, u8)>,
    /// Un dispositivo que no contesta: ninguna petición genera evento.
    pub mute: bool,
    /// Peticiones que todavía va a dejar sin contestar, al principio.
    pub mute_first: u32,
    /// Todo lo que se le pidió, en orden.
    pub seen: Vec<Setup>,
    /// Si además es un hub, el estado de sus puertos.
    pub hub: Option<HubState>,
    /// Si además es una unidad de almacenamiento, su medio y su estado.
    pub disk: Option<DiskState>,
    /// Lo que todavía tiene por entregar por los endpoints que no son EP0,
    /// como `(dci, carga, código de terminación)`: el informe de un ratón o
    /// el mapa de bits de cambios de un hub.
    pub queued: Vec<(u8, Vec<u8>, u32)>,
}

impl FakeDev {
    /// Un ratón HID de los de siempre: interfaz única de clase 3, un
    /// endpoint de interrupción IN y su descriptor de informe.
    pub(super) fn mouse() -> Self {
        let report: Vec<u8> = alloc::vec![
            0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01,
            0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01,
            0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81,
            0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, 0xC0, 0xC0,
        ];
        let rlen = report.len() as u16;
        let mut config: Vec<u8> = alloc::vec![
            // Configuration: un total que se arregla abajo, 1 interfaz.
            0x09, 0x02, 0x00, 0x00, 0x01, 0x01, 0x00, 0xa0, 0x32,
            // Interface 0: HID (3), subclase boot (1), protocolo ratón (2).
            0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00,
            // HID: un descriptor de informe, con su longitud.
            0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x00, 0x00,
            // Endpoint 1 IN, interrupción, 8 bytes cada 10 ms.
            0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0a,
        ];
        let total = config.len() as u16;
        config[2..4].copy_from_slice(&total.to_le_bytes());
        config[25..27].copy_from_slice(&rlen.to_le_bytes());
        Self {
            device: alloc::vec![
                0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x6d, 0x04, 0x77, 0xc0, 0x00, 0x01,
                0x01, 0x02, 0x03, 0x01,
            ],
            config,
            report,
            stall: Vec::new(),
            silent: Vec::new(),
            mute: false,
            mute_first: 0,
            seen: Vec::new(),
            hub: None,
            disk: None,
            queued: Vec::new(),
        }
    }

    /// Un teclado de protocolo de arranque: la otra mitad del camino HID, y
    /// el único informe que no lleva longitud mínima, porque las teclas van
    /// en un array y no en bytes con signo.
    pub(super) fn keyboard() -> Self {
        let report: Vec<u8> = alloc::vec![
            0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00,
            0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
            0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x95, 0x05, 0x75, 0x01, 0x91, 0x02, 0x95, 0x01,
            0x75, 0x03, 0x91, 0x01, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x15, 0x00, 0x25, 0x65,
            0x75, 0x08, 0x95, 0x06, 0x81, 0x00, 0xC0,
        ];
        let rlen = report.len() as u16;
        let mut config: Vec<u8> = alloc::vec![
            // Configuration: un total que se arregla abajo, 1 interfaz.
            0x09, 0x02, 0x00, 0x00, 0x01, 0x01, 0x00, 0xa0, 0x32,
            // Interface 0: HID (3), subclase boot (1), protocolo teclado (1).
            0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00,
            // HID: un descriptor de informe, con su longitud.
            0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x00, 0x00,
            // Endpoint 1 IN, interrupción, 8 bytes cada 10 ms.
            0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0a,
        ];
        let total = config.len() as u16;
        config[2..4].copy_from_slice(&total.to_le_bytes());
        config[25..27].copy_from_slice(&rlen.to_le_bytes());
        Self {
            device: alloc::vec![
                0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x6d, 0x04, 0x2d, 0xc3, 0x00, 0x01,
                0x01, 0x02, 0x03, 0x01,
            ],
            config,
            report,
            stall: Vec::new(),
            silent: Vec::new(),
            mute: false,
            mute_first: 0,
            seen: Vec::new(),
            hub: None,
            disk: None,
            queued: Vec::new(),
        }
    }

    /// Una unidad de almacenamiento masivo con `blocks` bloques de
    /// `block_size`: clase 8, subclase 6 (SCSI transparente), protocolo 0x50
    /// (Bulk-Only) y la pareja de endpoints bulk que eso exige.
    pub(super) fn disk(blocks: u64, block_size: u32) -> Self {
        let mut config: Vec<u8> = alloc::vec![
            // Configuration: un total que se arregla abajo, 1 interfaz.
            0x09, 0x02, 0x00, 0x00, 0x01, 0x01, 0x00, 0x80, 0xfa,
            // Interface 0: almacenamiento masivo (8), SCSI (6), Bulk-Only.
            0x09, 0x04, 0x00, 0x00, 0x02, 0x08, 0x06, 0x50, 0x00,
            // Endpoint 2 IN, bulk, 512 bytes.
            0x07, 0x05, 0x82, 0x02, 0x00, 0x02, 0x00, // Endpoint 1 OUT, bulk, 512 bytes.
            0x07, 0x05, 0x01, 0x02, 0x00, 0x02, 0x00,
        ];
        let total = config.len() as u16;
        config[2..4].copy_from_slice(&total.to_le_bytes());
        Self {
            device: alloc::vec![
                0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x81, 0x07, 0x38, 0x62, 0x00, 0x01,
                0x01, 0x02, 0x03, 0x01,
            ],
            config,
            report: Vec::new(),
            stall: Vec::new(),
            silent: Vec::new(),
            mute: false,
            mute_first: 0,
            seen: Vec::new(),
            hub: None,
            disk: Some(DiskState::new(blocks, block_size)),
            queued: Vec::new(),
        }
    }

    /// Una unidad que dice «no lista» las `n` primeras veces, que es lo que
    /// hace un pendrive mientras arranca.
    pub(super) fn with_not_ready(mut self, n: u32) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.not_ready = n;
        }
        self
    }

    /// Una unidad que contesta su capacidad con bloques de cero bytes.
    pub(super) fn with_zero_block_size(mut self) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.bad_block_size = true;
        }
        self
    }

    /// Una unidad que rechaza esa orden con CSW status 1.
    pub(super) fn failing_scsi(mut self, op: u8) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.fail_op = Some(op);
        }
        self
    }

    /// Una unidad tan grande que `READ CAPACITY(10)` no la puede contar.
    pub(super) fn needing_capacity16(mut self) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.needs_16 = true;
        }
        self
    }

    /// Una unidad cuyo CSW contesta con un tag que no es el de la orden.
    pub(super) fn with_wrong_csw_tag(mut self) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.wrong_tag = true;
        }
        self
    }

    /// Una unidad que entrega menos datos de los que la orden pidió y aun así
    /// dice que todo fue bien.
    pub(super) fn with_short_data(mut self, bytes: usize) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.short_by = bytes;
        }
        self
    }

    /// `GET_MAX_LUN` contesta esto, que son las unidades lógicas menos una.
    pub(super) fn with_luns(mut self, max_lun: u8) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.max_lun = max_lun;
        }
        self
    }

    /// Un bloque del medio, para ver si una escritura llegó.
    pub(super) fn media_block(&self, lba: u64) -> Vec<u8> {
        self.disk.as_ref().map(|d| d.block(lba)).unwrap_or_default()
    }

    /// Deja un bloque escrito de antemano, para que una lectura traiga algo.
    pub(super) fn with_block(mut self, lba: u64, data: &[u8]) -> Self {
        if let Some(d) = self.disk.as_mut() {
            d.put_block(lba, data);
        }
        self
    }

    /// Lo que llegó por el endpoint bulk OUT: un CBW, o la fase de datos de
    /// una escritura que ya se había anunciado.
    ///
    /// Encola lo que la unidad conteste por su endpoint IN. Devuelve `false`
    /// cuando esto no es una unidad de almacenamiento, y entonces el
    /// controlador no tiene nada que contestar.
    fn bulk_out(&mut self, bytes: &[u8]) -> bool {
        let Some(disk) = self.disk.as_mut() else {
            return false;
        };
        // La fase de datos de una escritura anunciada: el medio se queda con
        // lo que llegó y después sale su CSW.
        if let Some((lba, want, tag, asked)) = disk.pending_write.take() {
            let n = bytes.len().min(want);
            disk.put_block(lba, &bytes[..n]);
            let status = u8::from(n < want);
            let csw = bot_csw(tag, asked.saturating_sub(n as u32), status, disk.wrong_tag);
            self.queued.push((DCI_BULK_IN, csw, TRB_CC_SUCCESS));
            return true;
        }
        if bytes.len() < 31
            || u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) != BOT_CBW_SIGNATURE
        {
            return true;
        }
        let tag = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let asked = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let cb_len = (bytes[14] & 0x1f) as usize;
        let cdb = bytes[15..(15 + cb_len).min(bytes.len())].to_vec();
        disk.cdbs.push(cdb.clone());
        let op = cdb.first().copied().unwrap_or(0xff);
        if disk.fail_op == Some(op) {
            let csw = bot_csw(tag, asked, 1, disk.wrong_tag);
            self.queued.push((DCI_BULK_IN, csw, TRB_CC_SUCCESS));
            return true;
        }
        let (data, status) = match op {
            SCSI_TEST_UNIT_READY => {
                let not_ready = disk.not_ready > 0;
                disk.not_ready = disk.not_ready.saturating_sub(1);
                (Vec::new(), u8::from(not_ready))
            }
            SCSI_REQUEST_SENSE => (disk.sense(), 0),
            SCSI_INQUIRY => (disk.inquiry(), 0),
            SCSI_READ_CAPACITY_10 => (disk.capacity10(), 0),
            SCSI_SERVICE_ACTION_IN_16 if cdb.get(1) == Some(&0x10) => (disk.capacity16(), 0),
            SCSI_SYNCHRONIZE_CACHE_10 => (Vec::new(), 0),
            SCSI_READ_10 => match rw10_range(&cdb) {
                Some((lba, blocks)) if lba + blocks as u64 <= disk.blocks => {
                    let off = (lba * disk.block_size as u64) as usize;
                    let len =
                        (blocks as usize * disk.block_size as usize).saturating_sub(disk.short_by);
                    (disk.media[off..off + len].to_vec(), 0)
                }
                // Fuera del medio es un fallo de la orden, no del transporte.
                _ => (Vec::new(), 1),
            },
            SCSI_WRITE_10 => match rw10_range(&cdb) {
                Some((lba, blocks)) if lba + blocks as u64 <= disk.blocks => {
                    // El CSW no sale hasta que llegue la fase de datos.
                    let len = blocks as usize * disk.block_size as usize;
                    disk.pending_write = Some((lba, len, tag, asked));
                    return true;
                }
                _ => (Vec::new(), 1),
            },
            // Una orden que la unidad no entiende.
            _ => (Vec::new(), 1),
        };
        let moved = data.len().min(asked as usize) as u32;
        let wrong_tag = disk.wrong_tag;
        if moved > 0 {
            self.queued
                .push((DCI_BULK_IN, data[..moved as usize].to_vec(), TRB_CC_SUCCESS));
        }
        let csw = bot_csw(tag, asked - moved, status, wrong_tag);
        self.queued.push((DCI_BULK_IN, csw, TRB_CC_SUCCESS));
        true
    }

    /// Un hub de `ports` puertos: clase 9 en el dispositivo y en su única
    /// interfaz, con el endpoint de interrupción por el que avisa de los
    /// cambios (USB 2.0 §11.12.1).
    pub(super) fn hub(ports: u8) -> Self {
        let mut config: Vec<u8> = alloc::vec![
            0x09, 0x02, 0x00, 0x00, 0x01, 0x01, 0x00, 0xe0, 0x32,
            // Interface 0: clase 9 (hub), subclase 0, protocolo 0 (TT único).
            0x09, 0x04, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x00,
            // Endpoint 1 IN, interrupción, un byte de mapa de cambios.
            0x07, 0x05, 0x81, 0x03, 0x01, 0x00, 0x0c,
        ];
        let total = config.len() as u16;
        config[2..4].copy_from_slice(&total.to_le_bytes());
        Self {
            device: alloc::vec![
                0x12, 0x01, 0x00, 0x02, 0x09, 0x00, 0x01, 0x40, 0x09, 0x1a, 0x01, 0x00, 0x00, 0x01,
                0x00, 0x00, 0x00, 0x01,
            ],
            config,
            report: Vec::new(),
            stall: Vec::new(),
            silent: Vec::new(),
            mute: false,
            mute_first: 0,
            seen: Vec::new(),
            hub: Some(HubState {
                ports: alloc::vec![HubPort::default(); ports as usize],
                // 50 ms, que es lo que dice casi cualquier hub de verdad.
                power_good: 25,
                never_enables: false,
                slow_reset: 0,
            }),
            disk: None,
            queued: Vec::new(),
        }
    }

    /// Un hub que señala el puerto habilitado mientras su reset sigue puesto,
    /// durante `reads` lecturas de estado. Hay hubs que lo hacen, y un puerto
    /// en reset no se puede usar todavía.
    pub(super) fn with_slow_reset(mut self, reads: u8) -> Self {
        if let Some(h) = self.hub.as_mut() {
            h.slow_reset = reads;
        }
        self
    }

    /// Un hub cuyos puertos no se habilitan por mucho que se reseteen.
    pub(super) fn with_dead_ports(mut self) -> Self {
        if let Some(h) = self.hub.as_mut() {
            h.never_enables = true;
        }
        self
    }

    /// `bMaxPacketSize0` que anuncia el descriptor de dispositivo.
    pub(super) fn with_mps0(mut self, mps: u8) -> Self {
        self.device[7] = mps;
        self
    }

    /// Esta petición se rechaza con STALL.
    pub(super) fn stalling(mut self, bmrt: u8, breq: u8) -> Self {
        self.stall.push((bmrt, breq));
        self
    }

    /// Esta petición se queda sin contestar: ni datos ni STALL, que es lo que
    /// hace un aparato al que esa petición le sienta mal. El driver sólo sale
    /// de ahí por su plazo.
    pub(super) fn muting(mut self, bmrt: u8, breq: u8) -> Self {
        self.silent.push((bmrt, breq));
        self
    }

    /// Un dispositivo que no contesta nada.
    pub(super) fn mute(mut self) -> Self {
        self.mute = true;
        self
    }

    /// Un dispositivo que se despierta tarde: las `n` primeras peticiones se
    /// quedan sin contestar y a partir de ahí va bien.
    pub(super) fn mute_for(mut self, n: u32) -> Self {
        self.mute_first = n;
        self
    }

    /// Este descriptor de dispositivo en vez del suyo.
    pub(super) fn with_device(mut self, device: Vec<u8>) -> Self {
        self.device = device;
        self
    }

    /// Esta configuración en vez de la suya.
    pub(super) fn with_config(mut self, config: Vec<u8>) -> Self {
        self.config = config;
        self
    }

    /// Lo que el dispositivo entrega para esta petición, y con qué código de
    /// terminación: `None` si no piensa contestar.
    fn answer(&mut self, s: Setup) -> Option<(Vec<u8>, u32)> {
        self.seen.push(s);
        if self.mute {
            return None;
        }
        if self.mute_first > 0 {
            self.mute_first -= 1;
            return None;
        }
        if self.silent.contains(&(s.bmrt, s.breq)) {
            return None;
        }
        if self.stall.contains(&(s.bmrt, s.breq)) {
            return Some((Vec::new(), TRB_CC_STALL));
        }
        // GET_DESCRIPTOR al dispositivo (0x80) o a una interfaz (0x81).
        let data = match (s.bmrt, s.breq, (s.wvalue >> 8) as u8) {
            (0x80, 0x06, 1) => self.device.clone(),
            (0x80, 0x06, 2) => self.config.clone(),
            (0x81, 0x06, 0x22) => self.report.clone(),
            // SET_CONFIGURATION, SET_INTERFACE y las dos opcionales de HID
            // (SET_IDLE, SET_PROTOCOL) no llevan datos.
            (0x00, 0x09, _) | (0x01, 0x0b, _) | (0x21, 0x0a, _) | (0x21, 0x0b, _) => Vec::new(),
            // Lo de clase, que sólo un hub contesta.
            // GET_MAX_LUN (MSC BOT §3.2): sin unidad es un STALL, que es
            // justo lo que el driver tiene que leer como «una sola unidad».
            (0xa1, 0xfe, _) => match self.disk.as_ref() {
                Some(d) => alloc::vec![d.max_lun],
                None => return Some((Vec::new(), TRB_CC_STALL)),
            },
            (0xa0, 0x06, USB_DESC_HUB) | (0xa3, 0x00, _) | (0x23, 0x03, _) | (0x23, 0x01, _) => {
                match self.hub.as_mut() {
                    Some(hub) => hub.class_request(s),
                    None => return Some((Vec::new(), TRB_CC_STALL)),
                }
            }
            // Cualquier otra cosa es lo que un dispositivo hace con lo que no
            // entiende: rechazarla.
            _ => return Some((Vec::new(), TRB_CC_STALL)),
        };
        let n = data.len().min(s.wlen as usize);
        let cc = if n < s.wlen as usize {
            TRB_CC_SHORT
        } else {
            TRB_CC_SUCCESS
        };
        Some((data[..n].to_vec(), cc))
    }
}

impl HubState {
    /// Las cuatro peticiones de clase que el driver le hace a un hub.
    fn class_request(&mut self, s: Setup) -> Vec<u8> {
        match (s.bmrt, s.breq) {
            // Descriptor de hub (USB 2.0 §11.23.2.1). Son nueve bytes y el
            // driver pide quince: la diferencia sale por Short Packet.
            (0xa0, 0x06) => alloc::vec![
                0x09,
                USB_DESC_HUB,
                self.ports.len() as u8,
                // wHubCharacteristics: alimentación por puertos, think time 0.
                0x09,
                0x00,
                self.power_good,
                0x64,
                0x00,
                0xff,
            ],
            // GET_STATUS de un puerto: wPortStatus y wPortChange.
            (0xa3, 0x00) => {
                if let Some(p) = self.port_mut(s.windex) {
                    // El reset se va solo, y hay hubs que tardan en irse.
                    if p.reset_reads > 0 {
                        p.reset_reads -= 1;
                    } else {
                        p.status &= !HUB_PORT_RESET;
                    }
                }
                let p = self.port(s.windex).unwrap_or_default();
                let mut out = Vec::new();
                out.extend_from_slice(&p.status.to_le_bytes());
                out.extend_from_slice(&p.change.to_le_bytes());
                out
            }
            // SET_FEATURE y CLEAR_FEATURE sobre un puerto.
            (0x23, breq) => {
                let set = breq == 0x03;
                let never_enables = self.never_enables;
                let slow_reset = self.slow_reset;
                if let Some(p) = self.port_mut(s.windex) {
                    if set {
                        hub_set_feature(p, s.wvalue, never_enables, slow_reset);
                    } else {
                        hub_clear_feature(p, s.wvalue);
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn port(&self, windex: u16) -> Option<HubPort> {
        self.ports.get((windex as usize).wrapping_sub(1)).copied()
    }

    fn port_mut(&mut self, windex: u16) -> Option<&mut HubPort> {
        self.ports.get_mut((windex as usize).wrapping_sub(1))
    }
}

/// `SET_FEATURE` sobre un puerto de hub, con la parte que el hardware hace
/// sola: encender un puerto hace aparecer lo que tenga enchufado, y
/// resetearlo lo habilita.
fn hub_set_feature(p: &mut HubPort, feature: u16, never_enables: bool, slow_reset: u8) {
    match feature {
        HUB_FEAT_PORT_POWER => {
            p.status |= 1 << 8;
            if p.connected {
                p.status |= HUB_PORT_CONNECTION;
                p.status |= hub_speed_bits(p.speed);
                p.change |= 1; // C_PORT_CONNECTION
            }
        }
        HUB_FEAT_PORT_RESET => {
            // El reset de un hub termina antes de que el driver vuelva a
            // preguntar: lo que ve es el puerto ya habilitado y el cambio
            // puesto. Un hub roto deja el puerto en reset para siempre.
            if never_enables {
                p.status |= HUB_PORT_RESET;
                return;
            }
            if slow_reset > 0 {
                p.status |= HUB_PORT_RESET;
                p.reset_reads = slow_reset;
            } else {
                p.status &= !HUB_PORT_RESET;
            }
            if p.connected {
                p.status |= HUB_PORT_ENABLE;
            }
            p.change |= 1 << 4; // C_PORT_RESET
        }
        _ => {}
    }
}

/// `CLEAR_FEATURE`: las cinco características de cambio de un hub USB 2.0 son
/// los cinco bits bajos de `wPortChange`, en el mismo orden.
fn hub_clear_feature(p: &mut HubPort, feature: u16) {
    if (HUB_FEAT_C_PORT_CONNECTION..=HUB_FEAT_C_PORT_RESET).contains(&feature) {
        p.change &= !(1 << (feature - HUB_FEAT_C_PORT_CONNECTION));
    }
}

/// Los bits de velocidad de `wPortStatus` para una velocidad de PORTSC.
fn hub_speed_bits(speed: u8) -> u16 {
    match speed {
        SPEED_LOW => HUB_PORT_LOW_SPEED,
        SPEED_HIGH => HUB_PORT_HIGH_SPEED,
        _ => 0,
    }
}

/// El anillo EP0 de un slot ya direccionado, por donde va el consumidor.
#[derive(Clone, Copy)]
struct Ep0 {
    /// Índice en `devs` del dispositivo al que habla este slot.
    node: usize,
    deq: u64,
    cycle: bool,
}

/// El anillo de un endpoint que no es EP0, por donde salen los informes.
#[derive(Clone, Copy)]
struct EpIn {
    deq: u64,
    cycle: bool,
}

/// Un dispositivo y por dónde se llega a él.
struct DevNode {
    /// Puerto del hub raíz (base 1).
    port: u8,
    /// Route string: un nibble por nivel, 0 en un puerto raíz.
    route: u32,
    dev: FakeDev,
}

struct Fake {
    base: usize,
    /// El controlador está corriendo (RS=1 y arrancado).
    running: bool,
    /// Un controlador que no sale del reset: HCRST se queda puesto.
    stuck_in_reset: bool,
    /// Un controlador que acepta RS pero nunca limpia HCHalted.
    never_runs: bool,
    /// Un controlador que no se para cuando le quitan RS: HCHalted no llega.
    never_halts: bool,
    /// CNR sigue puesto durante estas lecturas más de USBSTS.
    cnr_reads: u32,
    /// Microsegundos que avanza el reloj por lectura de registro.
    read_tick_us: u64,

    /// Lo que el driver programó.
    config_slots: u8,
    dcbaap: u64,
    crcr: u64,
    erstba: u64,
    erstsz: u32,
    erdp: u64,
    iman: u32,
    /// Cada valor que el driver escribió en ERDP, en orden.
    erdp_writes: Vec<u64>,
    /// Cada timbre, como `(slot, endpoint)`.
    doorbells: Vec<(u8, u8)>,
    /// Veces que el controlador se reseteó.
    resets: u32,

    /// Anillo de comandos: por dónde va el consumidor y con qué ciclo.
    cmd_deq: u64,
    cmd_cycle: bool,
    cmds: Vec<SeenCmd>,
    next_slot: u8,
    /// Código de terminación con el que se contestan los comandos.
    cmd_cc: u32,
    /// Un controlador que no contesta: el timbre no hace nada.
    deaf: bool,
    /// Los bits pegajosos de USBSTS, como los guarda el aparato.
    ///
    /// Hace falta una copia porque el driver escribe el registro *antes* de
    /// avisar al fake (es lo que hace el `write_volatile` de verdad), así que
    /// al llegar aquí el valor anterior ya no está en memoria y un registro
    /// de borrar-escribiendo-uno no se podría modelar: cualquier escritura
    /// borraría el registro entero y «qué bits se limpiaron» dejaría de ser
    /// observable.
    usbsts: u32,
    /// El controlador no contesta a MMIO: todo lectura sale a unos, que es lo
    /// que se lee de un aparato apagado (D3) o que se cayó del bus.
    dead: bool,

    /// Anillo de eventos tal y como lo publicó el driver.
    ev_seg: u64,
    ev_trbs: usize,
    ev_enq: usize,
    ev_cycle: bool,
    events_posted: u32,

    ports: Vec<Port>,
    /// Todo lo enchufado, cada cosa con el puerto raíz y el route string por
    /// los que se llega a ella: detrás de un hub hay más de un dispositivo en
    /// el mismo puerto raíz, y lo único que los distingue es el route.
    devs: Vec<DevNode>,
    /// Anillo EP0 de cada slot, indexado por slot id.
    ep0: Vec<Option<Ep0>>,
    /// El `Max Packet Size` de EP0 que llevaba cada Evaluate Context, en
    /// orden: es lo único que ese comando viene a cambiar.
    eval_mps: Vec<u32>,
    /// Anillos de los endpoints que no son EP0, como `((slot, dci), estado)`.
    /// Se descubren al primer timbre, leyendo el contexto de salida.
    eps: Vec<((u8, u8), EpIn)>,
}

std::thread_local! {
    static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
}

fn with_state<R>(base: usize, f: impl FnOnce(&mut Fake) -> R) -> Option<R> {
    FAKE.with(|cell| {
        let mut guard = cell.borrow_mut();
        match guard.as_mut() {
            Some(st) if st.base == base => Some(f(st)),
            _ => None,
        }
    })
}

fn poke(va: usize, v: u32) {
    unsafe { write_volatile(va as *mut u32, v) }
}

fn peek(va: usize) -> u32 {
    unsafe { read_volatile(va as *const u32) }
}

fn peek64(va: usize) -> u64 {
    peek(va) as u64 | ((peek(va + 4) as u64) << 32)
}

/// Mitad baja de un registro de 64 bits que el driver escribe en dos mitades.
fn join_lo(cur: u64, lo: u32) -> u64 {
    (cur & 0xffff_ffff_0000_0000) | lo as u64
}

fn join_hi(cur: u64, hi: u32) -> u64 {
    (cur & 0xffff_ffff) | ((hi as u64) << 32)
}

/// El valor que PORTSC debe leer: lo que la escritura deja, más lo que el
/// hardware es dueño de decir.
fn portsc_after_write(prev: u32, written: u32, port: Port) -> u32 {
    // Los bits de cambio son RW1CS: se quedan como estaban salvo donde la
    // escritura lleva un 1.
    let mut sc = (prev & PORTSC_CHANGE_BITS) & !(written & PORTSC_CHANGE_BITS);
    // PED (RW1C) igual, pero el reset de puerto lo vuelve a poner.
    if (prev & (1 << 1)) != 0 && (written & (1 << 1)) == 0 {
        sc |= 1 << 1;
    }
    // Lo que el driver manda de verdad.
    sc |= written & ((1 << 9) | (0xf << 5) | (1 << 16) | (1 << 25) | (1 << 26));
    if (written & (1 << 4)) != 0 && port.connected {
        // Un reset de puerto habilita el puerto y levanta PRC; PR se limpia
        // solo al terminar.
        sc |= (1 << 1) | (1 << 21);
    }
    // Y lo que es del hardware: presencia y velocidad.
    if port.connected {
        sc |= 1;
        sc |= (port.speed as u32 & 0xf) << 10;
    }
    sc
}

/// Lo que el fake hace antes de cada lectura del driver.
pub(super) fn on_read(base: usize, off: usize) {
    with_state(base, |st| {
        if st.read_tick_us != 0 {
            test_clock::advance(st.read_tick_us);
        }
        if st.dead {
            // Un aparato que no contesta deja el bus a unos, y eso es lo que
            // el driver lee en cada registro.
            poke(st.base + off, u32::MAX);
            return;
        }
        if off == CAPLEN + OP_USBSTS {
            let mut sts = st.usbsts & !(USBSTS_HCH | USBSTS_CNR);
            if !st.running {
                sts |= USBSTS_HCH;
            }
            if st.cnr_reads > 0 {
                st.cnr_reads -= 1;
                sts |= USBSTS_CNR;
            }
            poke(st.base + off, sts);
        }
    });
}

/// Lo que el fake hace después de cada escritura del driver, con el valor ya
/// en memoria.
pub(super) fn on_write(base: usize, off: usize, v: u32) {
    with_state(base, |st| {
        if off >= DBOFF {
            let slot = ((off - DBOFF) / 4) as u8;
            let dci = v as u8;
            st.doorbells.push((slot, dci));
            if st.deaf {
                return;
            }
            if slot == 0 {
                run_command_ring(st);
            } else if dci == 1 {
                run_ep0(st, slot);
            } else if dci.is_multiple_of(2) {
                run_ep_out(st, slot, dci);
            } else {
                run_ep_in(st, slot, dci);
            }
            return;
        }
        if off >= RTSOFF {
            match off - RTSOFF {
                RT_IMAN => st.iman = v,
                RT_ERSTSZ => st.erstsz = v,
                RT_ERSTBA => st.erstba = join_lo(st.erstba, v),
                o if o == RT_ERSTBA + 4 => {
                    st.erstba = join_hi(st.erstba, v);
                    // El ERST ya está entero: el segmento y su tamaño salen de
                    // su primera (y única) entrada, igual que los lee el
                    // controlador.
                    if st.erstba != 0 {
                        st.ev_seg = peek64(st.erstba as usize);
                        st.ev_trbs = peek(st.erstba as usize + 8) as usize;
                    }
                }
                RT_ERDP => st.erdp = join_lo(st.erdp, v),
                o if o == RT_ERDP + 4 => {
                    st.erdp = join_hi(st.erdp, v);
                    st.erdp_writes.push(st.erdp);
                }
                _ => {}
            }
            return;
        }
        match off - CAPLEN {
            OP_USBCMD => {
                if (v & USBCMD_HCRST) != 0 {
                    if !st.stuck_in_reset {
                        poke(st.base + off, v & !USBCMD_HCRST);
                        st.resets += 1;
                        st.running = false;
                        // Un reset le quita al controlador todo lo que sabía.
                        st.crcr = 0;
                        st.dcbaap = 0;
                        st.erstba = 0;
                        st.ev_seg = 0;
                        st.cmd_deq = 0;
                        st.cmd_cycle = true;
                        st.ev_enq = 0;
                        st.ev_cycle = true;
                        st.next_slot = 1;
                    }
                    return;
                }
                if (v & USBCMD_RS) == 0 && st.never_halts {
                    // Un controlador que no se para: RS=0 y sigue corriendo,
                    // así que HCHalted no llega nunca.
                    return;
                }
                st.running = (v & USBCMD_RS) != 0 && !st.never_runs;
            }
            OP_USBSTS => {
                // RW1C: los bits escritos a 1 se apagan. El valor anterior
                // sale de la copia del aparato, no de la memoria, que la
                // escritura del driver ya se llevó por delante.
                st.usbsts &= !v;
                poke(st.base + off, st.usbsts);
            }
            OP_CRCR => {
                st.crcr = join_lo(st.crcr, v);
                st.cmd_cycle = (v & 1) != 0;
            }
            o if o == OP_CRCR + 4 => {
                st.crcr = join_hi(st.crcr, v);
                st.cmd_deq = st.crcr & !0x3f;
            }
            OP_DCBAAP => st.dcbaap = join_lo(st.dcbaap, v),
            o if o == OP_DCBAAP + 4 => st.dcbaap = join_hi(st.dcbaap, v),
            OP_CONFIG => st.config_slots = (v & 0xff) as u8,
            o if o >= OP_PORTSC && (o - OP_PORTSC).is_multiple_of(0x10) => {
                let idx = (o - OP_PORTSC) / 0x10;
                let Some(port) = st.ports.get(idx).copied() else {
                    return;
                };
                let sc = portsc_after_write(port.sc, v, port);
                if let Some(p) = st.ports.get_mut(idx) {
                    p.sc = sc;
                }
                poke(st.base + off, sc);
            }
            _ => {}
        }
    });
}

/// Consume el anillo de comandos desde donde se quedó y contesta cada TRB.
fn run_command_ring(st: &mut Fake) {
    if st.cmd_deq == 0 {
        return;
    }
    // Un segmento de 64 TRBs con su Link al final, que es lo que `CmdRing`
    // construye: al llegar al Link se vuelve al principio y el ciclo cambia.
    loop {
        let addr = st.cmd_deq as usize;
        let ctrl = peek(addr + 12);
        if (ctrl & 1) != (st.cmd_cycle as u32) {
            break;
        }
        let ty = (ctrl >> 10) & 0x3f;
        if ty == (TRB_LINK >> 10) {
            st.cmd_deq = peek64(addr) & !0xf;
            if (ctrl & 2) != 0 {
                st.cmd_cycle = !st.cmd_cycle;
            }
            continue;
        }
        let cmd = SeenCmd {
            ty,
            phys: st.cmd_deq,
            param: peek64(addr),
            slot: ((ctrl >> 24) & 0xff) as u8,
            dci: ((ctrl >> 16) & 0x1f) as u8,
        };
        st.cmds.push(cmd);
        // Address Device (§6.4.3.4): su contexto de entrada dice el puerto
        // raíz del slot y la dirección del anillo de EP0, que es todo lo que
        // el controlador necesita para servirle peticiones de control. Un
        // contexto al que le falte algo de eso no se puede servir, y eso es un
        // Parameter Error, no un comando que sale bien.
        let mut cc = st.cmd_cc;
        // Address Device y Configure Endpoint publican el contexto que les
        // llega: el controlador lo copia del contexto de entrada al de salida
        // del dispositivo, que es de donde el driver lo vuelve a leer para el
        // siguiente comando. Sin esa copia, un Configure Endpoint que
        // arrastra el contexto anterior arrastraría ceros.
        if ty == 11 || ty == 12 || ty == 13 {
            // Evaluate Context no publica el Slot Context: sólo evalúa Max
            // Exit Latency e Interrupter Target (§4.6.7), así que lo que un
            // driver meta ahí esperando que cuaje no cuaja.
            copy_contexts(st, cmd.slot, cmd.param, ty != 13);
        }
        if ty == 11 && !bind_ep0(st, cmd.slot, cmd.param) {
            cc = TRB_CC_PARAM;
        }
        // Evaluate Context (§6.4.3.6): aquí sólo se mira lo que el driver lo
        // usa para cambiar, que es el tamaño de paquete de EP0.
        if ty == 13 {
            let csz = ctx_size(st);
            let ic = (cmd.param & !0x3f) as usize;
            if ic != 0 {
                st.eval_mps.push(peek(ic + 2 * csz + 4) >> 16);
            }
        }
        // El slot que contesta Enable Slot, que es el único comando que lo
        // reparte en vez de recibirlo.
        let slot = if ty == 9 {
            let s = st.next_slot.max(1);
            st.next_slot = s.saturating_add(1);
            s
        } else {
            cmd.slot
        };
        post_event(
            st,
            cmd.phys,
            cc << 24,
            TRB_EVT_CMD_COMP | ((slot as u32) << 24),
        );
        st.cmd_deq += 16;
    }
}

/// Apunta el anillo de EP0 de un slot, leyendo su contexto de entrada igual
/// que lo leería el controlador.
fn bind_ep0(st: &mut Fake, slot: u8, input_ctx: u64) -> bool {
    let csz = ctx_size(st);
    let ic = (input_ctx & !0x3f) as usize;
    if ic == 0 {
        return false;
    }
    // Slot Context DW1 bits 23:16: Root Hub Port Number. Sin él el
    // controlador no sabe por qué puerto hablarle al dispositivo.
    let port = ((peek(ic + csz + 4) >> 16) & 0xff) as u8;
    // Slot Context DW0 bits 19:0: Route String, que es lo que dice *cuál* de
    // los dispositivos de ese puerto es éste.
    let route = peek(ic + csz) & 0x000f_ffff;
    // EP0 Context DW1 bits 31:16: Max Packet Size. Un control de cero bytes
    // por paquete no es una transferencia posible.
    let mps = peek(ic + 2 * csz + 4) >> 16;
    // EP0 Context DW2/3: TR Dequeue Pointer, con DCS en el bit 0.
    let ring = peek64(ic + 2 * csz + 8);
    // Y el contexto de dispositivo, que no va en el comando: el controlador lo
    // busca en la entrada del slot dentro del DCBAA (§4.5.1), así que un slot
    // cuya entrada está a cero no se puede direccionar.
    let dev_ctx = if st.dcbaap == 0 {
        0
    } else {
        peek64(st.dcbaap as usize + slot as usize * 8)
    };
    if port == 0 || port as usize > st.ports.len() || mps == 0 || ring == 0 || dev_ctx == 0 {
        return false;
    }
    let Some(node) = st
        .devs
        .iter()
        .position(|n| n.port == port && n.route == route)
    else {
        // Un puerto sin nada detrás: el controlador no tiene a quién
        // direccionar, y eso es un Parameter Error como cualquier otro.
        return false;
    };
    // Y si el dispositivo está detrás de un hub, ese hub tiene que estar
    // declarado como hub en su contexto, con bastantes puertos para el nibble
    // que lo nombra: un slot sin el bit Hub no rutea nada aguas abajo
    // (§4.3.3), así que el controlador no llega hasta aquí.
    if route != 0 && !routes_through_a_hub(st, port, route) {
        return false;
    }
    if let Some(slot_ep0) = st.ep0.get_mut(slot as usize) {
        *slot_ep0 = Some(Ep0 {
            node,
            deq: ring & !0xf,
            cycle: (ring & 1) != 0,
        });
    }
    true
}

/// Copia del contexto de entrada al contexto de salida del dispositivo lo que
/// las banderas de Add Context digan, igual que lo hace el controlador: el
/// índice `n+1` del contexto de entrada es el `n` del de salida.
fn copy_contexts(st: &Fake, slot: u8, input_ctx: u64, with_slot_ctx: bool) {
    let csz = ctx_size(st);
    let ic = (input_ctx & !0x3f) as usize;
    let dev = device_context(st, slot);
    if ic == 0 || dev == 0 {
        return;
    }
    let add = peek(ic + 4);
    for e in 0..32usize {
        if (add & (1 << e)) == 0 || (e == 0 && !with_slot_ctx) {
            continue;
        }
        for i in 0..csz / 4 {
            poke(dev + e * csz + i * 4, peek(ic + (e + 1) * csz + i * 4));
        }
    }
}

/// El contexto de salida del slot, que el controlador busca en el DCBAA.
fn device_context(st: &Fake, slot: u8) -> usize {
    if st.dcbaap == 0 {
        return 0;
    }
    peek64(st.dcbaap as usize + slot as usize * 8) as usize
}

/// El tamaño de contexto que anuncia HCCPARAMS1 (bit 2, CSZ).
fn ctx_size(st: &Fake) -> usize {
    if (peek(st.base + 0x10) & 4) != 0 {
        64
    } else {
        32
    }
}

/// ¿Hay un hub declarado como tal, con puertos de sobra, por el que pase este
/// route string? Sólo se mira el primer nivel, que es hasta donde llegan los
/// tests.
fn routes_through_a_hub(st: &Fake, port: u8, route: u32) -> bool {
    let Some(parent) = st.devs.iter().position(|n| n.port == port && n.route == 0) else {
        return false;
    };
    // El slot del padre es aquel cuyo EP0 habla con ese dispositivo.
    let Some(parent_slot) = st
        .ep0
        .iter()
        .position(|e| e.is_some_and(|e| e.node == parent))
    else {
        return false;
    };
    let dev = device_context(st, parent_slot as u8);
    if dev == 0 {
        return false;
    }
    // Slot Context DW0 bit 26: Hub. DW1 bits 31:24: Number of Ports.
    let is_hub = (peek(dev) & (1 << 26)) != 0;
    let ports = peek(dev + 4) >> 24;
    is_hub && (route & 0xf) <= ports
}

/// Consume el anillo de EP0 de un slot: cada tanda Setup (+ Data) + Status es
/// una petición, y el dispositivo del puerto la contesta.
fn run_ep0(st: &mut Fake, slot: u8) {
    let Some(mut ep) = st.ep0.get(slot as usize).copied().flatten() else {
        return;
    };
    loop {
        let addr = ep.deq as usize;
        let ctrl = peek(addr + 12);
        if (ctrl & 1) != ep.cycle as u32 {
            break;
        }
        let ty = (ctrl >> 10) & 0x3f;
        if ty == TY_LINK {
            ep.deq = peek64(addr) & !0xf;
            if (ctrl & 2) != 0 {
                ep.cycle = !ep.cycle;
            }
            continue;
        }
        if ty != TY_SETUP {
            // Un TRB que no abre una petición de control: el controlador lo
            // ejecutaría igual, pero aquí no hay nada que contestar.
            ep.deq += 16;
            continue;
        }
        let setup = Setup::from_param(peek64(addr));
        // Las etapas de Data (opcional) y Status, que tienen que estar ya
        // publicadas: una tanda a medio escribir se deja para el próximo
        // timbre, que es lo que hace un controlador de verdad.
        let mut cur = ep;
        cur.deq += 16;
        let mut data_phys = 0u64;
        let Some((status_phys, after)) = ({
            let mut found = None;
            for _ in 0..2 {
                let a = cur.deq as usize;
                let c = peek(a + 12);
                if (c & 1) != cur.cycle as u32 {
                    break;
                }
                let t = (c >> 10) & 0x3f;
                cur.deq += 16;
                match t {
                    TY_DATA => data_phys = peek64(a),
                    TY_STATUS => {
                        found = Some((a as u64, cur));
                        break;
                    }
                    _ => break,
                }
            }
            found
        }) else {
            break;
        };
        ep = after;
        let answer = st.devs.get_mut(ep.node).and_then(|n| n.dev.answer(setup));
        let Some((data, cc)) = answer else {
            // Ni dispositivo ni respuesta: el driver se queda esperando su
            // plazo, que es lo que ve cuando el aparato no está.
            continue;
        };
        if !data.is_empty() && data_phys != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), data_phys as *mut u8, data.len());
            }
        }
        let residual = (setup.wlen as u32).saturating_sub(data.len() as u32);
        post_event(
            st,
            status_phys,
            (residual & 0x00ff_ffff) | (cc << 24),
            TRB_EVT_TRANSFER | ((slot as u32) << 24) | (1 << 16),
        );
    }
    if let Some(slot_ep0) = st.ep0.get_mut(slot as usize) {
        *slot_ep0 = Some(ep);
    }
}

/// Consume el anillo de un endpoint IN que no es EP0: cada Normal TRB se
/// llena con lo que el dispositivo tenga encolado para ese endpoint y se
/// contesta con su Transfer Event.
///
/// El anillo no lo dice el test: sale del contexto de salida del endpoint
/// (`TR Dequeue Pointer`, con el bit de ciclo del consumidor en el bit 0),
/// que es de donde lo lee un controlador de verdad. Así, un mutante que
/// publique el contexto mal deja el endpoint mudo.
fn run_ep_in(st: &mut Fake, slot: u8, dci: u8) {
    let Some(node) = st.ep0.get(slot as usize).copied().flatten().map(|e| e.node) else {
        return;
    };
    let mut ep = match st.eps.iter().find(|(k, _)| *k == (slot, dci)) {
        Some((_, e)) => *e,
        None => {
            let dev = device_context(st, slot);
            if dev == 0 {
                return;
            }
            let raw = peek64(dev + dci as usize * ctx_size(st) + 8);
            if (raw & !0xfu64) == 0 {
                return;
            }
            EpIn {
                deq: raw & !0xf,
                cycle: (raw & 1) != 0,
            }
        }
    };
    loop {
        let addr = ep.deq as usize;
        let ctrl = peek(addr + 12);
        if (ctrl & 1) != ep.cycle as u32 {
            break;
        }
        let ty = (ctrl >> 10) & 0x3f;
        if ty == TY_LINK {
            ep.deq = peek64(addr) & !0xf;
            if (ctrl & 2) != 0 {
                ep.cycle = !ep.cycle;
            }
            continue;
        }
        if ty != TY_NORMAL {
            ep.deq += 16;
            continue;
        }
        // Un endpoint IN armado sin nada que entregar se queda esperando. Una
        // unidad de almacenamiento encola con `DCI_BULK_IN` porque no sabe qué
        // DCI le tocó: cualquier endpoint IN impar es el suyo.
        let Some(pos) = st.devs.get(node).and_then(|n| {
            n.dev
                .queued
                .iter()
                .position(|(d, _, _)| *d == dci || *d == DCI_BULK_IN)
        }) else {
            break;
        };
        let (_, payload, cc) = st.devs[node].dev.queued.remove(pos);
        let buf = peek64(addr);
        let len = (peek(addr + 8) & 0x1_ffff) as usize;
        let n = if cc == TRB_CC_SUCCESS {
            payload.len().min(len)
        } else {
            0
        };
        if n > 0 && buf != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(payload.as_ptr(), buf as *mut u8, n);
            }
        }
        ep.deq += 16;
        let residual = (len - n) as u32;
        post_event(
            st,
            addr as u64,
            (residual & 0x00ff_ffff) | (cc << 24),
            TRB_EVT_TRANSFER | ((slot as u32) << 24) | ((dci as u32) << 16),
        );
    }
    match st.eps.iter_mut().find(|(k, _)| *k == (slot, dci)) {
        Some(e) => e.1 = ep,
        None => st.eps.push(((slot, dci), ep)),
    }
}

/// Consume el anillo de un endpoint bulk OUT: lo que el driver escribió en el
/// buffer de cada Normal TRB se le entrega al dispositivo, que contesta
/// encolando lo que salga por su endpoint IN.
///
/// Por aquí pasa el CBW de cada orden SCSI y la fase de datos de una
/// escritura, que es la única dirección en la que el host habla.
fn run_ep_out(st: &mut Fake, slot: u8, dci: u8) {
    let Some(node) = st.ep0.get(slot as usize).copied().flatten().map(|e| e.node) else {
        return;
    };
    let mut ep = match st.eps.iter().find(|(k, _)| *k == (slot, dci)) {
        Some((_, e)) => *e,
        None => {
            let dev = device_context(st, slot);
            if dev == 0 {
                return;
            }
            let raw = peek64(dev + dci as usize * ctx_size(st) + 8);
            if (raw & !0xfu64) == 0 {
                return;
            }
            EpIn {
                deq: raw & !0xf,
                cycle: (raw & 1) != 0,
            }
        }
    };
    loop {
        let addr = ep.deq as usize;
        let ctrl = peek(addr + 12);
        if (ctrl & 1) != ep.cycle as u32 {
            break;
        }
        let ty = (ctrl >> 10) & 0x3f;
        if ty == TY_LINK {
            ep.deq = peek64(addr) & !0xf;
            if (ctrl & 2) != 0 {
                ep.cycle = !ep.cycle;
            }
            continue;
        }
        if ty != TY_NORMAL {
            ep.deq += 16;
            continue;
        }
        let buf = peek64(addr);
        let len = (peek(addr + 8) & 0x1_ffff) as usize;
        let mut bytes = alloc::vec![0u8; len];
        if buf != 0 && len > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(buf as *const u8, bytes.as_mut_ptr(), len);
            }
        }
        let answered = st
            .devs
            .get_mut(node)
            .is_some_and(|n| n.dev.bulk_out(&bytes));
        ep.deq += 16;
        if !answered {
            // Nadie al otro lado: el driver se queda esperando su plazo.
            continue;
        }
        post_event(
            st,
            addr as u64,
            TRB_CC_SUCCESS << 24,
            TRB_EVT_TRANSFER | ((slot as u32) << 24) | ((dci as u32) << 16),
        );
    }
    match st.eps.iter_mut().find(|(k, _)| *k == (slot, dci)) {
        Some(e) => e.1 = ep,
        None => st.eps.push(((slot, dci), ep)),
    }
}

/// Escribe un evento en el anillo de eventos del driver, con el bit de ciclo
/// del productor.
fn post_event(st: &mut Fake, p: u64, status: u32, ctrl: u32) {
    if st.ev_seg == 0 || st.ev_trbs == 0 {
        return;
    }
    let off = st.ev_seg as usize + st.ev_enq * 16;
    unsafe {
        write_volatile(off as *mut u64, p);
    }
    poke(off + 8, status);
    poke(off + 12, ctrl | st.ev_cycle as u32);
    st.events_posted += 1;
    st.ev_enq += 1;
    if st.ev_enq >= st.ev_trbs {
        st.ev_enq = 0;
        st.ev_cycle = !st.ev_cycle;
    }
}

/// La BAR y el controlador que la contesta, vivos mientras el test los tenga.
pub(super) struct Hw {
    _mem: Vec<u64>,
    base: usize,
}

impl Hw {
    /// Un controlador con `ports` puertos raíz, `slots` slots y los registros
    /// de capacidad ya puestos.
    pub(super) fn new(slots: u8, ports: u8) -> Self {
        test_clock::set(0);
        // El reloj avanza también sin leer registros, porque el driver tiene
        // esperas que no leen ninguno (`xhci_spin_delay_us`, los 100 ms de
        // VBUS tras dar energía): con el reloj quieto esas esperas se van a su
        // guarda de 500 millones de vueltas y el test no termina nunca.
        test_clock::set_auto_advance(1);
        let mut mem = alloc::vec![0u64; BAR_SIZE / 8];
        let base = mem.as_mut_ptr() as usize;
        let hw = Self { _mem: mem, base };
        poke(base, CAPLEN as u32); // CAPLENGTH
        poke(base + 0x04, slots as u32 | ((ports as u32) << 24)); // HCSPARAMS1
        poke(base + 0x14, DBOFF as u32); // DBOFF
        poke(base + 0x18, RTSOFF as u32); // RTSOFF
        FAKE.with(|cell| {
            *cell.borrow_mut() = Some(Fake {
                base,
                running: false,
                stuck_in_reset: false,
                never_runs: false,
                never_halts: false,
                cnr_reads: 0,
                read_tick_us: 1,
                config_slots: 0,
                dcbaap: 0,
                crcr: 0,
                erstba: 0,
                erstsz: 0,
                erdp: 0,
                iman: 0,
                erdp_writes: Vec::new(),
                doorbells: Vec::new(),
                resets: 0,
                cmd_deq: 0,
                cmd_cycle: true,
                cmds: Vec::new(),
                next_slot: 1,
                cmd_cc: TRB_CC_SUCCESS,
                deaf: false,
                usbsts: 0,
                dead: false,
                ev_seg: 0,
                ev_trbs: 0,
                ev_enq: 0,
                ev_cycle: true,
                events_posted: 0,
                ports: alloc::vec![Port::default(); ports as usize],
                devs: Vec::new(),
                ep0: (0..=256).map(|_| None).collect(),
                eval_mps: Vec::new(),
                eps: Vec::new(),
            });
        });
        hw
    }

    /// El driver, con su MMIO ya validado sobre esta BAR.
    pub(super) fn inner(&self, slots: u8, ports: u8, msi_vector: usize) -> XhciInner {
        let mmio = XhciMmio::from_virt(self.base, BAR_SIZE).expect("una BAR válida");
        XhciInner::new(mmio, slots, ports, msi_vector).expect("el driver cabe en memoria")
    }

    /// Escribe un registro de capacidad (HCSPARAMS2, HCCPARAMS1, ...).
    pub(super) fn set_cap(&self, off: usize, v: u32) {
        poke(self.base + off, v);
    }

    pub(super) fn op(&self, off: usize) -> u32 {
        peek(self.base + CAPLEN + off)
    }

    pub(super) fn set_op(&self, off: usize, v: u32) {
        poke(self.base + CAPLEN + off, v);
    }

    /// Pone los bits pegajosos de USBSTS, como los dejaría un error del
    /// propio controlador.
    pub(super) fn set_usbsts(&self, v: u32) {
        self.with(|st| {
            st.usbsts = v;
            poke(st.base + CAPLEN + OP_USBSTS, v);
        });
    }

    /// Los bits pegajosos de USBSTS que el aparato sigue guardando.
    pub(super) fn usbsts(&self) -> u32 {
        self.with(|st| st.usbsts)
    }

    pub(super) fn op64(&self, off: usize) -> u64 {
        peek64(self.base + CAPLEN + off)
    }

    pub(super) fn portsc(&self, port: u8) -> u32 {
        self.op(OP_PORTSC + (port as usize - 1) * 0x10)
    }

    /// Pone PORTSC a `v` tal cual, como lo dejaría el hardware (no es una
    /// escritura del driver: el fake no le aplica semántica).
    pub(super) fn set_portsc(&self, port: u8, v: u32) {
        self.set_op(OP_PORTSC + (port as usize - 1) * 0x10, v);
        self.with(|st| {
            if let Some(p) = st.ports.get_mut(port as usize - 1) {
                p.sc = v;
            }
        });
    }

    fn with<R>(&self, f: impl FnOnce(&mut Fake) -> R) -> R {
        with_state(self.base, f).expect("el fake de este hilo")
    }

    /// Conecta un dispositivo al puerto `port` (1-indexado) a `speed`.
    pub(super) fn connect(&self, port: u8, speed: u8) {
        self.with(|st| {
            if let Some(p) = st.ports.get_mut(port as usize - 1) {
                p.connected = true;
                p.speed = speed;
            }
        });
        // CCS, CSC y la velocidad, que es lo que el driver encuentra al mirar.
        let sc = self.portsc(port) | 1 | (1 << 17) | ((speed as u32 & 0xf) << 10);
        self.set_portsc(port, sc);
    }

    /// Conecta un dispositivo al puerto `port` y le pone detrás un aparato
    /// que contesta por EP0.
    pub(super) fn plug(&self, port: u8, speed: u8, dev: FakeDev) {
        self.connect(port, speed);
        self.add_dev(port, 0, dev);
    }

    /// Desenchufa lo que haya en el puerto raíz `port`: pierde la conexión y
    /// señala el cambio, que es lo que el driver encuentra cuando le llega el
    /// Port Status Change Event.
    pub(super) fn unplug(&self, port: u8) {
        self.with(|st| {
            if let Some(p) = st.ports.get_mut(port as usize - 1) {
                p.connected = false;
                p.speed = 0;
            }
            st.devs.retain(|n| n.port != port);
        });
        // CCS a cero, CSC puesto, PED a cero y la velocidad fuera.
        let sc = (self.portsc(port) & !(1 | (1 << 1) | (0xf << 10))) | (1 << 17);
        self.set_portsc(port, sc);
    }

    /// Enchufa un dispositivo al puerto `hub_port` del hub que está en el
    /// puerto raíz `port`: le marca a ese hub el puerto como conectado y
    /// registra al hijo con el route string que le va a tocar (un nibble por
    /// nivel, el primero en los bits 3:0).
    pub(super) fn plug_behind(&self, port: u8, hub_port: u8, speed: u8, dev: FakeDev) {
        self.with(|st| {
            if let Some(hub) = st
                .devs
                .iter_mut()
                .find(|n| n.port == port && n.route == 0)
                .and_then(|n| n.dev.hub.as_mut())
            {
                if let Some(p) = hub.ports.get_mut(hub_port as usize - 1) {
                    p.connected = true;
                    p.speed = speed;
                    // Si el puerto ya está alimentado, el hub ve la conexión
                    // en el momento: eso es el hotplug. Si no, lo verá cuando
                    // le llegue su PORT_POWER.
                    if (p.status & (1 << 8)) != 0 {
                        p.status |= HUB_PORT_CONNECTION | hub_speed_bits(speed);
                        p.change |= 1; // C_PORT_CONNECTION
                    }
                }
            }
        });
        self.add_dev(port, hub_port as u32, dev);
    }

    /// Desenchufa lo que haya en el puerto `hub_port` de ese hub: el puerto
    /// pierde la conexión y señala el cambio, que es lo que el driver
    /// encuentra al barrer.
    pub(super) fn unplug_behind(&self, port: u8, hub_port: u8) {
        self.with(|st| {
            if let Some(hub) = st
                .devs
                .iter_mut()
                .find(|n| n.port == port && n.route == 0)
                .and_then(|n| n.dev.hub.as_mut())
            {
                if let Some(p) = hub.ports.get_mut(hub_port as usize - 1) {
                    p.connected = false;
                    p.speed = 0;
                    p.status &= !(HUB_PORT_CONNECTION | HUB_PORT_ENABLE);
                    p.change |= 1; // C_PORT_CONNECTION
                }
            }
            st.devs
                .retain(|n| !(n.port == port && n.route == hub_port as u32));
        });
    }

    fn add_dev(&self, port: u8, route: u32, dev: FakeDev) {
        self.with(|st| {
            st.devs.retain(|n| !(n.port == port && n.route == route));
            st.devs.push(DevNode { port, route, dev });
        });
    }

    /// Un informe que el aparato del puerto `port` entrega por el endpoint
    /// `dci`, como lo hace un ratón que se mueve: se escribe en el TRB que el
    /// driver dejó armado y se contesta con su Transfer Event.
    pub(super) fn send_in(&self, port: u8, dci: u8, data: &[u8]) {
        self.send_in_at(port, 0, dci, data);
    }

    /// Lo mismo, para un dispositivo que está detrás de un hub.
    pub(super) fn send_in_at(&self, port: u8, route: u32, dci: u8, data: &[u8]) {
        self.deliver(port, route, dci, data.to_vec(), TRB_CC_SUCCESS);
    }

    /// Una transferencia que falla en ese endpoint, con el código de
    /// terminación que diga el test: un endpoint con halt contesta así.
    pub(super) fn fail_in(&self, port: u8, dci: u8, cc: u32) {
        self.deliver(port, 0, dci, Vec::new(), cc);
    }

    /// El mapa de bits de cambios que el hub del puerto `port` manda por su
    /// endpoint de estado: el bit 0 es el hub y el bit `n` su puerto `n`,
    /// exactamente los que tienen cambios sin reconocer.
    pub(super) fn hub_notify(&self, port: u8, dci: u8) {
        let bitmap = self.with(|st| {
            let hub = st
                .devs
                .iter()
                .find(|n| n.port == port && n.route == 0)
                .and_then(|n| n.dev.hub.as_ref())?;
            let mut map = alloc::vec![0u8; hub.ports.len() / 8 + 1];
            for (i, p) in hub.ports.iter().enumerate() {
                if p.change != 0 {
                    let bit = i + 1;
                    map[bit / 8] |= 1 << (bit % 8);
                }
            }
            Some(map)
        });
        if let Some(bitmap) = bitmap {
            self.deliver(port, 0, dci, bitmap, TRB_CC_SUCCESS);
        }
    }

    /// Encola la carga y la entrega en el acto, que es el orden de verdad: el
    /// dato llega del cable y el controlador llena el TRB que ya estaba
    /// armado. Lo que no quepa en ningún TRB se queda a la espera del
    /// siguiente timbre.
    fn deliver(&self, port: u8, route: u32, dci: u8, data: Vec<u8>, cc: u32) {
        self.with(|st| {
            let Some(node) = st
                .devs
                .iter()
                .position(|n| n.port == port && n.route == route)
            else {
                return;
            };
            st.devs[node].dev.queued.push((dci, data, cc));
            let slot = st
                .ep0
                .iter()
                .position(|e| e.is_some_and(|e| e.node == node));
            if let Some(slot) = slot {
                run_ep_in(st, slot as u8, dci);
            }
        });
    }

    /// Lo que la unidad del puerto `port` tiene escrito en un bloque.
    pub(super) fn media_block(&self, port: u8, lba: u64) -> Vec<u8> {
        self.with(|st| {
            st.devs
                .iter()
                .find(|n| n.port == port && n.route == 0)
                .map(|n| n.dev.media_block(lba))
                .unwrap_or_default()
        })
    }

    /// Las órdenes SCSI que recibió la unidad del puerto `port`, en orden.
    pub(super) fn scsi_cdbs(&self, port: u8) -> Vec<Vec<u8>> {
        self.with(|st| {
            st.devs
                .iter()
                .find(|n| n.port == port && n.route == 0)
                .and_then(|n| n.dev.disk.as_ref())
                .map(|d| d.cdbs.clone())
                .unwrap_or_default()
        })
    }

    /// Lo que un dispositivo no ha podido entregar todavía porque su endpoint
    /// no tenía ningún TRB armado.
    pub(super) fn undelivered(&self, port: u8) -> usize {
        self.with(|st| {
            st.devs
                .iter()
                .find(|n| n.port == port && n.route == 0)
                .map(|n| n.dev.queued.len())
                .unwrap_or_default()
        })
    }

    /// Los tamaños de paquete de EP0 que llevaron los Evaluate Context.
    pub(super) fn eval_mps(&self) -> Vec<u32> {
        self.with(|st| st.eval_mps.clone())
    }

    /// El siguiente slot que reparte Enable Slot. Un controlador que
    /// contesta con un slot que no existe es lo que el driver tiene que
    /// rechazar antes de indexar sus tablas con él.
    pub(super) fn set_next_slot(&self, slot: u8) {
        self.with(|st| st.next_slot = slot);
    }

    /// Microsegundos que avanza el reloj por consulta, incluidas las de los
    /// bucles de espera que no leen ningún registro: subirlo hace que un
    /// plazo de segundos llegue en miles de vueltas y no en millones.
    pub(super) fn set_auto_advance(&self, us: u64) {
        test_clock::set_auto_advance(us);
    }

    /// `wPortChange` del puerto `hub_port` del hub que está en el puerto raíz
    /// `port`, para ver si el driver reconoció sus cambios.
    pub(super) fn hub_port_change(&self, port: u8, hub_port: u8) -> u16 {
        self.with(|st| {
            st.devs
                .iter()
                .find(|n| n.port == port && n.route == 0)
                .and_then(|n| n.dev.hub.as_ref())
                .and_then(|h| h.port(hub_port as u16))
                .map(|p| p.change)
                .unwrap_or_default()
        })
    }

    /// Las peticiones de control que recibió el aparato del puerto `port`.
    pub(super) fn requests(&self, port: u8) -> Vec<Setup> {
        self.requests_at(port, 0)
    }

    /// Lo mismo, para un dispositivo que está detrás de un hub.
    pub(super) fn requests_at(&self, port: u8, route: u32) -> Vec<Setup> {
        self.with(|st| {
            st.devs
                .iter()
                .find(|n| n.port == port && n.route == route)
                .map(|n| n.dev.seen.clone())
                .unwrap_or_default()
        })
    }

    /// Contesta los comandos con este código de terminación.
    pub(super) fn answer_commands_with(&self, cc: u32) {
        self.with(|st| st.cmd_cc = cc);
    }

    /// Deja de contestar: el timbre no hace nada.
    pub(super) fn go_deaf(&self) {
        self.with(|st| st.deaf = true);
    }

    /// El controlador no sale del reset.
    pub(super) fn stick_in_reset(&self) {
        self.with(|st| st.stuck_in_reset = true);
    }

    /// El controlador acepta RS pero nunca limpia HCHalted.
    pub(super) fn never_run(&self) {
        self.with(|st| st.never_runs = true);
    }

    /// El controlador se para solo, sin que nadie le quite RS: eso es lo que
    /// deja un error de bus (HSE), y lo que el driver encuentra como
    /// HCHalted sin haberlo pedido.
    pub(super) fn halt(&self) {
        self.with(|st| st.running = false);
    }

    /// Un halt que además deja RS caído: eso es lo que deja el firmware o la
    /// SMM cuando se lleva el controlador por delante, y entonces volver a
    /// poner RS es lo único que lo levanta.
    pub(super) fn halt_clearing_rs(&self) {
        self.with(|st| {
            st.running = false;
            let off = CAPLEN + OP_USBCMD;
            let prev = peek(st.base + off);
            poke(st.base + off, prev & !USBCMD_RS);
        });
    }

    /// El controlador deja de contestar a MMIO: todo sale a unos.
    pub(super) fn go_dead(&self) {
        self.with(|st| {
            st.dead = true;
            // Un aparato que se cayó del bus no corre: si alguien le escribe
            // RS y vuelve a «correr», es que le escribió a un aparato que no
            // está.
            st.running = false;
        });
    }

    /// Y vuelve a contestar, para ver que el que ya se dejó por muerto no se
    /// vuelve a mirar.
    pub(super) fn revive(&self) {
        self.with(|st| {
            st.dead = false;
            st.running = true;
        });
    }

    /// El controlador no se para cuando le quitan RS.
    pub(super) fn never_halt(&self) {
        self.with(|st| st.never_halts = true);
    }

    /// CNR sigue puesto durante `reads` lecturas más de USBSTS.
    pub(super) fn hold_cnr_for(&self, reads: u32) {
        self.with(|st| st.cnr_reads = reads);
    }

    /// Microsegundos que avanza el reloj por lectura de registro.
    pub(super) fn set_read_tick(&self, us: u64) {
        self.with(|st| st.read_tick_us = us);
    }

    pub(super) fn config_slots(&self) -> u8 {
        self.with(|st| st.config_slots)
    }

    pub(super) fn dcbaap(&self) -> u64 {
        self.with(|st| st.dcbaap)
    }

    pub(super) fn crcr(&self) -> u64 {
        self.with(|st| st.crcr)
    }

    pub(super) fn erstba(&self) -> u64 {
        self.with(|st| st.erstba)
    }

    pub(super) fn erstsz(&self) -> u32 {
        self.with(|st| st.erstsz)
    }

    pub(super) fn erdp_writes(&self) -> Vec<u64> {
        self.with(|st| st.erdp_writes.clone())
    }

    pub(super) fn doorbells(&self) -> Vec<(u8, u8)> {
        self.with(|st| st.doorbells.clone())
    }

    pub(super) fn iman(&self) -> u32 {
        self.with(|st| st.iman)
    }

    pub(super) fn resets(&self) -> u32 {
        self.with(|st| st.resets)
    }

    pub(super) fn running(&self) -> bool {
        self.with(|st| st.running)
    }

    pub(super) fn commands(&self) -> Vec<SeenCmd> {
        self.with(|st| st.cmds.clone())
    }

    pub(super) fn events_posted(&self) -> u32 {
        self.with(|st| st.events_posted)
    }

    /// El controlador ya corriendo, como lo encuentra un driver que arranca
    /// detrás de la BIOS.
    pub(super) fn already_running(&self) {
        self.set_op(OP_USBCMD, USBCMD_RS);
        self.with(|st| st.running = true);
    }
}

impl Drop for Hw {
    fn drop(&mut self) {
        test_clock::set_auto_advance(0);
        FAKE.with(|cell| {
            let mut guard = cell.borrow_mut();
            if guard.as_ref().is_some_and(|st| st.base == self.base) {
                *guard = None;
            }
        });
    }
}
