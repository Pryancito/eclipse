//! Un HBA de AHCI falso en memoria del anfitrión.
//!
//! Lo que falta de este fichero bajo test no es aritmética —de eso ya se
//! encargan los tests puros de `command_tests`— sino el **motor**: programar
//! el puerto, someter un comando y esperarlo, leer el IDENTIFY, mover un
//! bloque. Todo eso habla con registros por MMIO y espera que el controlador
//! conteste, así que sin un controlador no se puede llamar.
//!
//! El truco es el mismo que el del xHCI: la traducción de física a virtual es
//! la identidad bajo test (`utils/host_hooks.rs`), así que una ventana de
//! registros en el montón del anfitrión vale tal cual como ventana de BAR, y
//! las direcciones que el driver mete en el PRDT se pueden seguir desde aquí
//! sin traducir nada. Los registros los lee el driver de esa memoria
//! directamente; lo único que necesita una costura es que **alguien reaccione
//! a una escritura**, porque un `PxCI` que nadie baja es un comando que nunca
//! acaba. Esa costura es [`on_write`], y vive por debajo de
//! `AhciPort::write_reg`.

extern crate std;

use super::*;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Tamaño de la ventana de registros: la cabecera del HBA más los 32 puertos.
const WINDOW: usize = 0x100 + 32 * 0x80;

/// Lo que contesta el disco de un puerto.
pub(super) struct Disk {
    /// El contenido, en bytes. Su longitud decide la capacidad.
    pub bytes: Vec<u8>,
    /// Bytes por sector lógico del aparato (512, o 4096 en un 4Kn).
    pub sector_bytes: usize,
    /// Si el IDENTIFY dice que soporta LBA de 48 bits (palabra 83, bit 10).
    pub lba48: bool,
    /// Palabra 106 del IDENTIFY, tal cual. `None` la deja a cero, que es lo
    /// que contesta un disco de 512 bytes de toda la vida.
    pub word106: Option<u16>,
    /// Palabras 117-118 del IDENTIFY (el tamaño de sector en palabras).
    pub words117_118: Option<u32>,
    /// Cuenta de sectores que declara el IDENTIFY en las palabras 60-61
    /// (LBA de 28 bits). `None` la deriva del tamaño.
    pub lba28_sectors: Option<u32>,
    /// Ídem para las palabras 100-103 (LBA de 48 bits).
    pub lba48_sectors: Option<u64>,
}

impl Disk {
    /// Un disco de `sectors` sectores de 512 bytes, con LBA de 48 bits.
    pub(super) fn of_512(sectors: usize) -> Self {
        Disk {
            bytes: vec![0u8; sectors * 512],
            sector_bytes: 512,
            lba48: true,
            word106: None,
            words117_118: None,
            lba28_sectors: None,
            lba48_sectors: None,
        }
    }

    /// Un disco 4Kn: `sectors` sectores de 4096 bytes, y el IDENTIFY lo dice.
    pub(super) fn of_4kn(sectors: usize) -> Self {
        Disk {
            bytes: vec![0u8; sectors * 4096],
            sector_bytes: 4096,
            lba48: true,
            // Bit 14 «válido», bit 12 «el sector pasa de 256 palabras».
            word106: Some((1 << 14) | (1 << 12)),
            words117_118: Some(2048),
            lba28_sectors: None,
            lba48_sectors: None,
        }
    }

    fn dev_sectors(&self) -> u64 {
        (self.bytes.len() / self.sector_bytes) as u64
    }

    /// Los 512 bytes de la respuesta al IDENTIFY.
    fn identify_page(&self) -> [u8; 512] {
        let mut words = [0u16; 256];
        let lba28 = self
            .lba28_sectors
            .unwrap_or_else(|| self.dev_sectors().min(u32::from(u16::MAX) as u64 * 2) as u32);
        words[60] = lba28 as u16;
        words[61] = (lba28 >> 16) as u16;
        if self.lba48 {
            words[83] = 1 << 10;
        }
        let lba48 = self.lba48_sectors.unwrap_or_else(|| self.dev_sectors());
        for (i, w) in [100usize, 101, 102, 103].iter().enumerate() {
            words[*w] = (lba48 >> (16 * i)) as u16;
        }
        if let Some(w) = self.word106 {
            words[106] = w;
        }
        if let Some(v) = self.words117_118 {
            words[117] = v as u16;
            words[118] = (v >> 16) as u16;
        }
        let mut out = [0u8; 512];
        for (i, w) in words.iter().enumerate() {
            out[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}

/// Lo que el controlador falso hace mal a propósito.
#[derive(Default, Clone, Copy)]
pub(super) struct Faults {
    /// Los siguientes `n` comandos no bajan `PxCI`: el driver se come su
    /// plazo entero y acaba en `reset_port`.
    pub hang_commands: u32,
    /// Los siguientes `n` comandos levantan TFES en `PxIS`.
    pub tfes_commands: u32,
    /// Los siguientes `n` comandos dejan ERR en `PxTFD` **sin** latchar nada
    /// en `PxIS`, que es el caso que el driver tuvo que aprender a ver.
    pub tfd_err_commands: u32,
    /// `PxTFD` empieza con BUSY puesto y nadie lo baja.
    pub tfd_stuck_busy: bool,
    /// Los siguientes `n` comandos levantan TFES **y además** dejan `PxCI`
    /// puesto: un aparato que informa del error y no suelta el hueco. Es el
    /// único caso en que mirar los bits de error durante la espera cambia
    /// algo, porque con el hueco ya libre la espera acaba igual.
    pub tfes_and_hang: u32,
}

struct State {
    regs: Vec<u32>,
    base: usize,
    disk: Option<Disk>,
    faults: Faults,
    /// `PxIS` y `PxSERR` son **write-1-to-clear**: una escritura no pone el
    /// valor, apaga los bits que trae. `write_reg` ya ha dejado el valor
    /// crudo en la ventana cuando llegamos aquí —igual que un
    /// `write_volatile` de verdad—, así que el valor anterior hay que
    /// guardarlo nosotros o no hay con qué calcular el nuevo. Sin esto, el
    /// `PxIS = 0xffff_ffff` con el que el driver limpia antes de cada comando
    /// deja los cuatro bits de error puestos y **todo comando falla**.
    is_prev: [u32; 32],
    serr_prev: [u32; 32],
    /// Comandos sometidos, contando los que fallan.
    commands: u32,
    /// El opcode de cada comando sometido, en orden.
    opcodes: Vec<u8>,
}

std::thread_local! {
    static FAKE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// La costura: la llama `AhciPort::write_reg` después de escribir el registro,
/// igual que un controlador de verdad ve la escritura ya hecha.
pub(super) fn on_write(base: usize, offset: usize, val: u32) {
    FAKE.with(|f| {
        let mut slot = f.borrow_mut();
        let Some(st) = slot.as_mut() else { return };
        if base < st.base + 0x100 {
            return;
        }
        let port = (base - st.base - 0x100) / 0x80;
        if offset == PORT_IS {
            let nuevo = st.is_prev[port] & !val;
            st.is_prev[port] = nuevo;
            *st.reg(base + PORT_IS) = nuevo;
        }
        if offset == PORT_SERR {
            let nuevo = st.serr_prev[port] & !val;
            st.serr_prev[port] = nuevo;
            *st.reg(base + PORT_SERR) = nuevo;
        }
        if offset == PORT_CI && val != 0 {
            st.run_command(base, val);
        }
    });
}

impl State {
    fn reg(&mut self, addr: usize) -> &mut u32 {
        let idx = (addr - self.base) / 4;
        &mut self.regs[idx]
    }

    /// Ejecuta el comando del hueco que `val` enciende.
    fn run_command(&mut self, pbase: usize, val: u32) {
        let slot = val.trailing_zeros() as usize;
        self.commands += 1;

        // La cabeza del comando y su tabla, por donde el driver las dejó: el
        // controlador las encuentra igual, leyendo PxCLB.
        let clb =
            (*self.reg(pbase + PORT_CLB) as u64) | ((*self.reg(pbase + PORT_CLBU) as u64) << 32);
        // Un PxCLB con la mitad baja a cero es un PxCLB que el driver no ha
        // programado: la mitad alta de una dirección del montón del anfitrión
        // no es cero, así que un driver que sólo escribiera `PxCLBU` dejaría
        // aquí un puntero a ninguna parte. Un HBA de verdad haría DMA ahí y se
        // llevaría por delante lo que hubiera; seguirlo desde aquí tumba el
        // proceso del test en vez de dar un veredicto, así que el controlador
        // falso se niega y deja el comando sin contestar.
        if clb & 0xffff_ffff == 0 {
            return;
        }
        let header = (clb as usize + slot * 32) as *mut u8;
        let (ctba, prdtl) = unsafe {
            let prdtl = u16::from_le_bytes([*header.add(2), *header.add(3)]) as usize;
            let lo = u32::from_le_bytes([
                *header.add(8),
                *header.add(9),
                *header.add(10),
                *header.add(11),
            ]) as u64;
            let hi = u32::from_le_bytes([
                *header.add(12),
                *header.add(13),
                *header.add(14),
                *header.add(15),
            ]) as u64;
            (lo | (hi << 32), prdtl)
        };
        let cfis = ctba as usize as *const u8;
        let opcode = unsafe { *cfis.add(2) };
        self.opcodes.push(opcode);

        if self.faults.tfes_and_hang > 0 {
            self.faults.tfes_and_hang -= 1;
            let port = (pbase - self.base - 0x100) / 0x80;
            self.is_prev[port] |= PXIS_TFES;
            *self.reg(pbase + PORT_IS) |= PXIS_TFES;
            return; // El error está latchado y el hueco sigue ocupado.
        }
        if self.faults.hang_commands > 0 {
            self.faults.hang_commands -= 1;
            return; // PxCI se queda puesto: el driver espera su plazo entero.
        }

        let moved = self.transfer(opcode, cfis, ctba as usize, prdtl);
        unsafe {
            let prdbc = moved as u32;
            core::ptr::copy_nonoverlapping(prdbc.to_le_bytes().as_ptr(), header.add(4), 4);
        }

        if self.faults.tfes_commands > 0 {
            self.faults.tfes_commands -= 1;
            let port = (pbase - self.base - 0x100) / 0x80;
            self.is_prev[port] |= PXIS_TFES;
            *self.reg(pbase + PORT_IS) |= PXIS_TFES;
        }
        if self.faults.tfd_err_commands > 0 {
            self.faults.tfd_err_commands -= 1;
            *self.reg(pbase + PORT_TFD) |= ATA_DEV_ERR as u32;
        }
        *self.reg(pbase + PORT_CI) &= !val;
    }

    /// Mueve los datos del comando entre el disco falso y lo que el PRDT
    /// describe. Devuelve los bytes movidos.
    fn transfer(&mut self, opcode: u8, cfis: *const u8, ctba: usize, prdtl: usize) -> usize {
        let prds: Vec<(usize, usize)> = (0..prdtl)
            .map(|i| unsafe {
                let e = (ctba + 0x80 + i * 16) as *const u8;
                let rd = |off: usize| {
                    u32::from_le_bytes([
                        *e.add(off),
                        *e.add(off + 1),
                        *e.add(off + 2),
                        *e.add(off + 3),
                    ])
                };
                let addr = (rd(0) as usize) | ((rd(4) as usize) << 32);
                let len = ((rd(12) & 0x003f_ffff) + 1) as usize;
                (addr, len)
            })
            .collect();

        if opcode == ATA_CMD_IDENTIFY {
            let page = match &self.disk {
                Some(d) => d.identify_page(),
                None => [0u8; 512],
            };
            let n = prds.first().map(|p| p.1.min(512)).unwrap_or(0);
            if let Some((addr, _)) = prds.first() {
                unsafe { core::ptr::copy_nonoverlapping(page.as_ptr(), *addr as *mut u8, n) };
            }
            return n;
        }

        let write = opcode == ATA_CMD_WRITE_DMA_EXT || opcode == ATA_CMD_WRITE_DMA;
        let read = opcode == ATA_CMD_READ_DMA_EXT || opcode == ATA_CMD_READ_DMA;
        if !read && !write {
            return 0; // FLUSH y compañía no mueven nada.
        }

        let lba = unsafe { lba_of_fis(cfis, opcode) };
        let Some(disk) = self.disk.as_mut() else {
            return 0;
        };
        let mut off = lba as usize * disk.sector_bytes;
        let mut moved = 0;
        for (addr, len) in prds {
            let len = len.min(disk.bytes.len().saturating_sub(off));
            if len == 0 {
                break;
            }
            unsafe {
                if write {
                    core::ptr::copy_nonoverlapping(
                        addr as *const u8,
                        disk.bytes[off..].as_mut_ptr(),
                        len,
                    );
                } else {
                    core::ptr::copy_nonoverlapping(
                        disk.bytes[off..].as_ptr(),
                        addr as *mut u8,
                        len,
                    );
                }
            }
            off += len;
            moved += len;
        }
        moved
    }
}

/// El LBA que lleva el FIS, en los dos modos de direccionamiento.
unsafe fn lba_of_fis(cfis: *const u8, opcode: u8) -> u64 {
    let b = |i: usize| unsafe { *cfis.add(i) } as u64;
    if opcode == ATA_CMD_READ_DMA_EXT || opcode == ATA_CMD_WRITE_DMA_EXT {
        b(4) | (b(5) << 8) | (b(6) << 16) | (b(8) << 24) | (b(9) << 32) | (b(10) << 40)
    } else {
        b(4) | (b(5) << 8) | (b(6) << 16) | ((b(7) & 0x0f) << 24)
    }
}

/// El HBA falso. Vive mientras el test lo tenga, y al soltarlo deja el hilo
/// como estaba.
pub(super) struct Hw;

impl Hw {
    /// Monta un HBA con esos puertos implementados y ese disco en el puerto 0.
    ///
    /// `s64a` es CAP.S64A: con `false` el driver tiene que rechazar cualquier
    /// DMA que pase de los 4 GiB en vez de dejar que el controlador lo truncue.
    pub(super) fn new(ports_implemented: u32, disk: Option<Disk>, s64a: bool) -> Self {
        let mut regs = vec![0u32; WINDOW / 4];
        let base = regs.as_ptr() as usize;
        let put = |regs: &mut Vec<u32>, addr: usize, val: u32| {
            regs[(addr - base) / 4] = val;
        };
        // CAP: 32 huecos de comando, y S64A según lo pedido.
        let cap = (31u32 << 8) | if s64a { 1 << 31 } else { 0 };
        put(&mut regs, base + HBA_CAP, cap);
        put(&mut regs, base + HBA_PI, ports_implemented);
        // Los 32 puertos quedan sembrados con un aparato presente, no sólo
        // los implementados: así `PI` es lo ÚNICO que decide a quién se mira,
        // y un driver que recorriera los 32 a ciegas enumeraría 32 discos en
        // vez de pararse en un puerto que no contesta.
        for i in 0..32u32 {
            let pbase = base + 0x100 + i as usize * 0x80;
            // DET=3 (aparato presente y comunicando), IPM=1 (activo).
            put(&mut regs, pbase + PORT_SSTS, 0x0000_0133);
            put(&mut regs, pbase + PORT_SIG, HBA_SIG_ATA);
        }
        let st = State {
            regs,
            base,
            disk,
            faults: Faults::default(),
            is_prev: [0; 32],
            serr_prev: [0; 32],
            commands: 0,
            opcodes: Vec::new(),
        };
        FAKE.with(|f| *f.borrow_mut() = Some(st));
        Hw
    }

    /// La dirección de la ventana de registros, que vale como BAR.
    pub(super) fn base(&self) -> usize {
        FAKE.with(|f| f.borrow().as_ref().unwrap().base)
    }

    /// Lo que el controlador hace mal.
    pub(super) fn faults(&self, faults: Faults) {
        FAKE.with(|f| f.borrow_mut().as_mut().unwrap().faults = faults);
        if faults.tfd_stuck_busy {
            for i in 0..32u32 {
                self.poke_port(i, PORT_TFD, ATA_DEV_BUSY as u32);
            }
        }
    }

    /// Escribe un registro de un puerto a mano.
    pub(super) fn poke_port(&self, port: u32, offset: usize, val: u32) {
        FAKE.with(|f| {
            let mut s = f.borrow_mut();
            let st = s.as_mut().unwrap();
            let addr = st.base + 0x100 + port as usize * 0x80 + offset;
            *st.reg(addr) = val;
        });
    }

    /// Lee un registro de un puerto.
    pub(super) fn peek_port(&self, port: u32, offset: usize) -> u32 {
        FAKE.with(|f| {
            let mut s = f.borrow_mut();
            let st = s.as_mut().unwrap();
            let addr = st.base + 0x100 + port as usize * 0x80 + offset;
            *st.reg(addr)
        })
    }

    /// Comandos sometidos.
    pub(super) fn commands(&self) -> u32 {
        FAKE.with(|f| f.borrow().as_ref().unwrap().commands)
    }

    /// Los opcodes sometidos, en orden.
    pub(super) fn opcodes(&self) -> Vec<u8> {
        FAKE.with(|f| f.borrow().as_ref().unwrap().opcodes.clone())
    }

    /// Lo que hay en el disco falso, para comprobar una escritura.
    pub(super) fn disk_bytes(&self, off: usize, len: usize) -> Vec<u8> {
        FAKE.with(|f| {
            f.borrow().as_ref().unwrap().disk.as_ref().unwrap().bytes[off..off + len].to_vec()
        })
    }

    /// Siembra el disco falso, para comprobar una lectura.
    pub(super) fn fill_disk(&self, off: usize, bytes: &[u8]) {
        FAKE.with(|f| {
            let mut s = f.borrow_mut();
            let d = s.as_mut().unwrap().disk.as_mut().unwrap();
            d.bytes[off..off + bytes.len()].copy_from_slice(bytes);
        });
    }
}

impl Drop for Hw {
    fn drop(&mut self) {
        FAKE.with(|f| *f.borrow_mut() = None);
    }
}
