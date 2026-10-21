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
    /// Un dispositivo que no contesta: ninguna petición genera evento.
    pub mute: bool,
    /// Peticiones que todavía va a dejar sin contestar, al principio.
    pub mute_first: u32,
    /// Todo lo que se le pidió, en orden.
    pub seen: Vec<Setup>,
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
            mute: false,
            mute_first: 0,
            seen: Vec::new(),
        }
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

/// El anillo EP0 de un slot ya direccionado, por donde va el consumidor.
#[derive(Clone, Copy)]
struct Ep0 {
    /// Puerto raíz del slot, que es por donde se llega a su dispositivo.
    port: usize,
    deq: u64,
    cycle: bool,
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

    /// Anillo de eventos tal y como lo publicó el driver.
    ev_seg: u64,
    ev_trbs: usize,
    ev_enq: usize,
    ev_cycle: bool,
    events_posted: u32,

    ports: Vec<Port>,
    /// Lo que hay enchufado en cada puerto, en paralelo a `ports`.
    devs: Vec<Option<FakeDev>>,
    /// Anillo EP0 de cada slot, indexado por slot id.
    ep0: Vec<Option<Ep0>>,
    /// El `Max Packet Size` de EP0 que llevaba cada Evaluate Context, en
    /// orden: es lo único que ese comando viene a cambiar.
    eval_mps: Vec<u32>,
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
        if off == CAPLEN + OP_USBSTS {
            let mut sts = peek(st.base + off) & !(USBSTS_HCH | USBSTS_CNR);
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
                // RW1C: los bits escritos a 1 se apagan.
                let prev = peek(st.base + off);
                poke(st.base + off, prev & !v);
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
    let port = ((peek(ic + csz + 4) >> 16) & 0xff) as usize;
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
    if port == 0 || port > st.ports.len() || mps == 0 || ring == 0 || dev_ctx == 0 {
        return false;
    }
    if let Some(slot_ep0) = st.ep0.get_mut(slot as usize) {
        *slot_ep0 = Some(Ep0 {
            port,
            deq: ring & !0xf,
            cycle: (ring & 1) != 0,
        });
    }
    true
}

/// El tamaño de contexto que anuncia HCCPARAMS1 (bit 2, CSZ).
fn ctx_size(st: &Fake) -> usize {
    if (peek(st.base + 0x10) & 4) != 0 {
        64
    } else {
        32
    }
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
        let answer = st
            .devs
            .get_mut(port_index(ep.port))
            .and_then(|o| o.as_mut())
            .and_then(|dev| dev.answer(setup));
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

/// El índice en `devs` del puerto `port` (1-indexado); 0 no es un puerto, y
/// se queda fuera del vector a propósito.
fn port_index(port: usize) -> usize {
    port.wrapping_sub(1)
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
                ev_seg: 0,
                ev_trbs: 0,
                ev_enq: 0,
                ev_cycle: true,
                events_posted: 0,
                ports: alloc::vec![Port::default(); ports as usize],
                devs: (0..ports as usize).map(|_| None).collect(),
                ep0: (0..=256).map(|_| None).collect(),
                eval_mps: Vec::new(),
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
        self.with(|st| {
            if let Some(d) = st.devs.get_mut(port as usize - 1) {
                *d = Some(dev);
            }
        });
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

    /// Las peticiones de control que recibió el aparato del puerto `port`.
    pub(super) fn requests(&self, port: u8) -> Vec<Setup> {
        self.with(|st| {
            st.devs
                .get(port as usize - 1)
                .and_then(|o| o.as_ref())
                .map(|d| d.seen.clone())
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
