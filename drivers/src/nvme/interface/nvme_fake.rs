//! Un controlador NVMe falso en memoria del anfitrión.
//!
//! La ventana de registros es un `Vec<u64>` del montón cuya dirección hace de
//! BAR 0, porque bajo test `drivers_phys_to_virt` es la identidad: el driver
//! lee y escribe ahí directamente, y las direcciones físicas que programa en
//! ASQ, ACQ y los PRP se pueden seguir tal cual desde aquí.
//!
//! Lo único que esa memoria no puede dar es la **reacción** a una escritura, y
//! en NVMe la reacción es todo: `CC.EN` es lo que levanta `CSTS.RDY`, y el
//! timbre de la cola es lo que hace que el controlador lea el comando, lo
//! ejecute y conteste con su entrada de terminación. Esa es la única costura
//! `cfg(test)` del driver.
//!
//! Hay un falso por hilo, así que las vueltas en paralelo no se pisan la
//! ventana.

extern crate std;

use super::*;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::{read_volatile, write_volatile};

/// Una ventana de 8 KiB: los dos timbres de las dos colas caben de sobra.
pub(super) const BAR_LEN: usize = 0x2000;

/// Lo que contesta un comando que salió bien.
const SC_SUCCESS: u16 = 0;
/// Internal Error: un estado de error cualquiera, de los que el driver
/// reintenta.
pub(super) const SC_INTERNAL: u16 = 0x06;

std::thread_local! {
    static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Default)]
struct Cola {
    sq_pa: usize,
    cq_pa: usize,
    /// Entradas de la cola de terminación, para dar la vuelta con su fase.
    cq_len: usize,
    sq_len: usize,
    sq_head: usize,
    cq_tail: usize,
    /// La fase que el controlador escribe ahora en cada entrada nueva.
    phase: u16,
}

/// Las averías que se le pueden pedir al controlador. Cada una es un
/// controlador que existe.
#[derive(Default)]
pub(super) struct Averias {
    /// Nunca levanta `CSTS.RDY` al habilitarlo.
    pub never_ready: bool,
    /// No baja `CSTS.RDY` cuando le quitan `CC.EN`.
    pub never_resets: bool,
    /// Levanta `CSTS.CFS` en cuanto lo habilitan.
    pub fatal_on_enable: bool,
    /// No contesta ningún comando: el timbre no hace nada.
    pub deaf: bool,
    /// Contesta este estado en vez de éxito.
    pub status: u16,
    /// Contesta las primeras `stale` terminaciones con un identificador de
    /// comando que nadie espera.
    pub stale: u32,
    /// Lo que contesta el Identify Namespace.
    pub nsze: u64,
    pub lbads: u8,
    /// MDTS del Identify Controller.
    pub mdts: u8,
}

struct Fake {
    base: usize,
    enabled: bool,
    colas: Vec<Cola>,
    averias: Averias,
    /// Cada comando que el controlador llegó a leer, como `(qid, opcode)`.
    vistos: Vec<(usize, u8)>,
    /// Cada valor que el driver escribió en CC, en orden.
    cc_writes: Vec<u32>,
    /// Cada timbre de cola de terminación, como `(qid, cabeza)`.
    cq_timbres: Vec<(usize, usize)>,
    /// El medio, un sector de 512 bytes por LBA, sólo donde se escribió.
    medio: Vec<(u64, Vec<u8>)>,
}

fn peek32(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}

fn poke32(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}

impl Fake {
    fn csts(&self) -> u32 {
        peek32(self.base + NVME_REG_CSTS)
    }

    fn set_csts(&self, v: u32) {
        poke32(self.base + NVME_REG_CSTS, v);
    }
}

/// La ventana de registros de un controlador falso, viva mientras el test lo
/// esté.
pub(super) struct Hw {
    _mem: Vec<u64>,
    base: usize,
}

impl Hw {
    /// Un controlador sano: un namespace de `blocks` LBAs de 512 bytes.
    pub(super) fn new(blocks: u64) -> Self {
        Self::con(Averias {
            nsze: blocks,
            lbads: 9,
            mdts: 5,
            ..Default::default()
        })
    }

    /// Un controlador con las averías que se le pidan.
    pub(super) fn con(averias: Averias) -> Self {
        crate::nvme::nvme_queue::test_clock::set(0);
        // Las esperas del driver miran el reloj sin leer ningún registro, así
        // que sin esto un plazo que no se cumple no vence nunca.
        crate::nvme::nvme_queue::test_clock::set_auto_advance(1_000);
        let mut mem = vec![0u64; BAR_LEN / 8];
        let base = mem.as_mut_ptr() as usize;
        let hw = Self { _mem: mem, base };
        // CAP: MQES 31 (32 entradas), DSTRD 0 (timbres de 4 en 4), TO 2
        // (1 s), que es un controlador corriente.
        unsafe { write_volatile(base as *mut u64, 31 | (2u64 << 24)) };
        // El controlador llega corriendo, que es como lo deja la firmware
        // cuando ha arrancado desde el: `CC.EN` puesto y `CSTS.RDY` levantado.
        // Asi el reset del driver tiene algo que apagar y algo que esperar, en
        // vez de encontrarse los dos bits ya a cero.
        poke32(base + NVME_REG_CC, NVME_CC_ENABLE);
        poke32(base + NVME_REG_CSTS, NVME_CSTS_RDY);
        FAKE.with(|c| {
            *c.borrow_mut() = Some(Fake {
                base,
                enabled: false,
                colas: vec![Cola::default(); 2],
                averias,
                vistos: Vec::new(),
                cc_writes: Vec::new(),
                cq_timbres: Vec::new(),
                medio: Vec::new(),
            })
        });
        hw
    }

    pub(super) fn base(&self) -> usize {
        self.base
    }

    /// Escribe CAP a mano, para los controladores que no caben en la ventana
    /// o que dicen tener una cola de una entrada.
    pub(super) fn set_cap(&self, cap: u64) {
        unsafe { write_volatile(self.base as *mut u64, cap) };
    }

    pub(super) fn intms(&self) -> u32 {
        peek32(self.base + NVME_REG_INTMS)
    }

    pub(super) fn aqa(&self) -> u32 {
        peek32(self.base + NVME_REG_AQA)
    }

    /// Cada valor que el driver escribió en CC, en orden.
    pub(super) fn cc_writes(&self) -> Vec<u32> {
        FAKE.with(|c| {
            c.borrow()
                .as_ref()
                .map(|f| f.cc_writes.clone())
                .unwrap_or_default()
        })
    }

    /// Las cabezas que el driver confirmó en el timbre de la cola de
    /// terminación de `qid`, en orden.
    pub(super) fn cq_timbres(&self, qid: usize) -> Vec<usize> {
        FAKE.with(|c| {
            c.borrow()
                .as_ref()
                .map(|f| {
                    f.cq_timbres
                        .iter()
                        .filter(|(q, _)| *q == qid)
                        .map(|(_, h)| *h)
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// El tamaño con el que el driver declaró las dos colas de `qid`, en
    /// entradas: `(envío, terminación)`.
    pub(super) fn cola_len(&self, qid: usize) -> (usize, usize) {
        FAKE.with(|c| {
            c.borrow()
                .as_ref()
                .and_then(|f| f.colas.get(qid).map(|q| (q.sq_len, q.cq_len)))
                .unwrap_or_default()
        })
    }

    /// Los comandos que el controlador llegó a leer, como `(qid, opcode)`.
    pub(super) fn vistos(&self) -> Vec<(usize, u8)> {
        FAKE.with(|c| {
            c.borrow()
                .as_ref()
                .map(|f| f.vistos.clone())
                .unwrap_or_default()
        })
    }

    /// Lo que hay grabado en un LBA del medio.
    pub(super) fn sector(&self, lba: u64) -> Vec<u8> {
        FAKE.with(|c| {
            let b = c.borrow();
            let f = b.as_ref().expect("no hay controlador");
            f.medio
                .iter()
                .find(|(l, _)| *l == lba)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| vec![0u8; 512])
        })
    }

    /// Graba un LBA del medio antes de que nadie lo lea.
    pub(super) fn set_sector(&self, lba: u64, data: &[u8]) {
        FAKE.with(|c| {
            let mut b = c.borrow_mut();
            let f = b.as_mut().expect("no hay controlador");
            let mut s = vec![0u8; 512];
            let n = data.len().min(512);
            s[..n].copy_from_slice(&data[..n]);
            f.medio.retain(|(l, _)| *l != lba);
            f.medio.push((lba, s));
        });
    }

    /// Cambia una avería con el controlador ya arrancado.
    pub(super) fn averiar(&self, f: impl FnOnce(&mut Averias)) {
        FAKE.with(|c| {
            let mut b = c.borrow_mut();
            f(&mut b.as_mut().expect("no hay controlador").averias)
        });
    }
}

impl Drop for Hw {
    fn drop(&mut self) {
        FAKE.with(|c| *c.borrow_mut() = None);
    }
}

/// Lo que el controlador hace después de cada escritura del driver, con el
/// valor ya en memoria.
pub(super) fn on_write(base: usize, off: usize, v: u64) {
    FAKE.with(|c| {
        let mut slot = c.borrow_mut();
        let Some(f) = slot.as_mut() else { return };
        if f.base != base {
            return;
        }
        match off {
            NVME_REG_CC => {
                f.cc_writes.push(v as u32);
                reaccion_cc(f, v as u32)
            }
            NVME_REG_AQA => {
                f.colas[0].sq_len = ((v as u32 & 0xfff) + 1) as usize;
                f.colas[0].cq_len = (((v as u32 >> 16) & 0xfff) + 1) as usize;
            }
            NVME_REG_ASQ => f.colas[0].sq_pa = v as usize,
            NVME_REG_ACQ => f.colas[0].cq_pa = v as usize,
            o if o >= NVME_REG_DBS => {
                let idx = (o - NVME_REG_DBS) / 4;
                // Pares el timbre de la cola de envío, impares el de la de
                // terminación. El de terminación sólo dice por dónde va el
                // anfitrión: no hay nada que ejecutar, pero sí que apuntar,
                // porque un controlador real deja de reusar esas entradas.
                if idx.is_multiple_of(2) {
                    atender(f, idx / 2, v as usize);
                } else {
                    f.cq_timbres.push((idx / 2, v as usize));
                }
            }
            _ => {}
        }
    });
}

fn reaccion_cc(f: &mut Fake, cc: u32) {
    if (cc & NVME_CC_ENABLE) != 0 {
        f.enabled = true;
        f.colas[0].phase = 1;
        f.colas[0].cq_tail = 0;
        f.colas[0].sq_head = 0;
        if f.averias.fatal_on_enable {
            f.set_csts(NVME_CSTS_CFS);
            return;
        }
        if !f.averias.never_ready {
            f.set_csts(NVME_CSTS_RDY);
        }
        return;
    }
    // CC.EN a cero es un reset; CC.SHN es un apagado ordenado.
    if (cc & (3 << 14)) != 0 {
        f.set_csts(f.csts() | NVME_CSTS_SHST_CMPLT);
        return;
    }
    f.enabled = false;
    if !f.averias.never_resets {
        f.set_csts(0);
    }
}

/// El controlador lee los comandos que le acaban de dejar y contesta.
fn atender(f: &mut Fake, qid: usize, tail: usize) {
    if f.averias.deaf || qid >= f.colas.len() {
        return;
    }
    let cola = f.colas[qid];
    if cola.sq_pa == 0 || cola.cq_pa == 0 {
        return;
    }
    let mut head = cola.sq_head;
    while head != tail {
        let cmd = unsafe { read_volatile((cola.sq_pa + head * 64) as *const [u32; 16]) };
        head = (head + 1) % cola.sq_len.max(1);
        ejecutar(f, qid, cmd);
    }
    f.colas[qid].sq_head = head;
}

/// Mete una entrada de terminación en la cola de `qid`.
fn completar(f: &mut Fake, qid: usize, cid: u16, status: u16, result: u32) {
    let cola = f.colas[qid];
    let slot = cola.cq_pa + cola.cq_tail * 16;
    unsafe {
        write_volatile(slot as *mut u64, result as u64);
        write_volatile((slot + 8) as *mut u16, 0);
        write_volatile((slot + 10) as *mut u16, qid as u16);
        write_volatile((slot + 12) as *mut u16, cid);
        write_volatile((slot + 14) as *mut u16, (status << 1) | cola.phase);
    }
    let next = cola.cq_tail + 1;
    if next >= cola.cq_len.max(1) {
        f.colas[qid].cq_tail = 0;
        f.colas[qid].phase ^= 1;
    } else {
        f.colas[qid].cq_tail = next;
    }
}

fn ejecutar(f: &mut Fake, qid: usize, cmd: [u32; 16]) {
    let opcode = (cmd[0] & 0xff) as u8;
    let cid = (cmd[0] >> 16) as u16;
    let nsid = cmd[1];
    let prp1 = cmd[6] as u64 | ((cmd[7] as u64) << 32);
    let cdw10 = cmd[10];
    let cdw11 = cmd[11];
    f.vistos.push((qid, opcode));

    // Un identificador de comando que nadie espera: el driver tiene que
    // consumirlo y seguir esperando el suyo.
    if f.averias.stale > 0 {
        f.averias.stale -= 1;
        completar(f, qid, cid ^ 0x5a5a, SC_SUCCESS, 0);
    }

    let status = f.averias.status;
    if status != 0 {
        completar(f, qid, cid, status, 0);
        return;
    }

    if qid == 0 {
        match opcode {
            // Identify
            0x06 => {
                let cns = cdw10 & 0xff;
                let p = prp1 as usize;
                unsafe { core::ptr::write_bytes(p as *mut u8, 0, 4096) };
                if cns == 1 {
                    let modelo = b"ECLIPSE FAKE NVME                       ";
                    unsafe {
                        core::ptr::copy_nonoverlapping(modelo.as_ptr(), (p + 24) as *mut u8, 40)
                    };
                    unsafe { write_volatile((p + 77) as *mut u8, f.averias.mdts) };
                } else {
                    unsafe { write_volatile(p as *mut u64, f.averias.nsze) };
                    unsafe { write_volatile((p + 26) as *mut u8, 0) };
                    unsafe {
                        write_volatile((p + 128) as *mut u32, (f.averias.lbads as u32) << 16)
                    };
                }
                let _ = nsid;
                completar(f, qid, cid, SC_SUCCESS, 0);
            }
            // Set Features: una cola de envío y una de terminación, en base 0.
            0x09 => completar(f, qid, cid, SC_SUCCESS, 0),
            // Create CQ
            0x05 => {
                let q = (cdw10 & 0xffff) as usize;
                if q < f.colas.len() {
                    f.colas[q].cq_pa = prp1 as usize;
                    f.colas[q].cq_len = ((cdw10 >> 16) & 0xffff) as usize + 1;
                    f.colas[q].phase = 1;
                    f.colas[q].cq_tail = 0;
                }
                completar(f, qid, cid, SC_SUCCESS, 0);
            }
            // Create SQ
            0x01 => {
                let q = (cdw10 & 0xffff) as usize;
                if q < f.colas.len() {
                    f.colas[q].sq_pa = prp1 as usize;
                    f.colas[q].sq_len = ((cdw10 >> 16) & 0xffff) as usize + 1;
                    f.colas[q].sq_head = 0;
                }
                let _ = cdw11;
                completar(f, qid, cid, SC_SUCCESS, 0);
            }
            _ => completar(f, qid, cid, SC_SUCCESS, 0),
        }
        return;
    }

    // La cola de entrada y salida: leer, escribir y vaciar la caché.
    match opcode {
        0x02 | 0x01 => {
            let slba = cmd[10] as u64 | ((cmd[11] as u64) << 32);
            let nlb = (cmd[12] & 0xffff) as u64 + 1;
            let prp2 = cmd[8] as u64 | ((cmd[9] as u64) << 32);
            transferir(f, opcode == 0x01, slba, nlb, prp1 as usize, prp2 as usize);
            completar(f, qid, cid, SC_SUCCESS, 0);
        }
        _ => completar(f, qid, cid, SC_SUCCESS, 0),
    }
}

/// Mueve los sectores entre el medio y los PRP del comando. Una página cubre
/// ocho sectores de 512 bytes; la segunda sale de PRP2, que para más de dos
/// páginas es una lista.
fn transferir(f: &mut Fake, write: bool, slba: u64, nlb: u64, prp1: usize, prp2: usize) {
    let paginas = ((nlb * 512) + 4095) / 4096;
    let dir = |i: u64| -> usize {
        if i == 0 {
            prp1
        } else if paginas == 2 {
            prp2
        } else {
            unsafe { read_volatile((prp2 + (i as usize - 1) * 8) as *const u64) as usize }
        }
    };
    for i in 0..nlb {
        let off = (i * 512) as usize;
        let pagina = dir((off / 4096) as u64) + (off % 4096);
        if write {
            let mut s = vec![0u8; 512];
            unsafe { core::ptr::copy_nonoverlapping(pagina as *const u8, s.as_mut_ptr(), 512) };
            f.medio.retain(|(l, _)| *l != slba + i);
            f.medio.push((slba + i, s));
        } else {
            let s = f
                .medio
                .iter()
                .find(|(l, _)| *l == slba + i)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| vec![0u8; 512]);
            unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), pagina as *mut u8, 512) };
        }
    }
}
