//! Lo que esta capa decide, con un espacio de configuración falso y una tabla
//! en memoria del host.
//!
//! Lo que **no** se prueba aquí: que un dispositivo real entregue el mensaje.
//! Eso no lo contesta ningún buffer.

use super::*;
use alloc::vec;
use alloc::vec::Vec;
use lock::Mutex;

const AM: CSpaceAccessMethod = CSpaceAccessMethod::MemoryMapped(core::ptr::null_mut());
const LOC: Location = Location {
    bus: 0,
    device: 0,
    function: 0,
};

/// 256 bytes de espacio de configuración, como 64 palabras. Se direcciona por
/// el camino de memoria, que es el que pasa el offset tal cual; `read8` del
/// crate se la sintetiza de `read32`, así que no hace falta nada más.
struct FakeCfg {
    space: Mutex<[u32; 64]>,
}

impl FakeCfg {
    fn nueva() -> Self {
        Self {
            space: Mutex::new([0u32; 64]),
        }
    }
    fn pon32(&self, off: usize, v: u32) {
        self.space.lock()[off / 4] = v;
    }
    fn pon8(&self, off: usize, v: u8) {
        let palabra = off & !0b11;
        let shift = (off & 0b11) * 8;
        let viejo = self.lee32(palabra);
        self.pon32(
            palabra,
            (viejo & !(0xffu32 << shift)) | ((v as u32) << shift),
        );
    }
    fn lee32(&self, off: usize) -> u32 {
        self.space.lock()[off / 4]
    }
}

impl PortOps for FakeCfg {
    unsafe fn read8(&self, _port: u16) -> u8 {
        unreachable!("el camino de memoria lo saca de read32")
    }
    unsafe fn read16(&self, _port: u16) -> u16 {
        unreachable!("el camino de memoria lo saca de read32")
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        self.lee32((port & 0xffc) as usize)
    }
    unsafe fn write8(&self, _port: u16, _val: u8) {
        unreachable!("nada escribe de a byte")
    }
    unsafe fn write16(&self, _port: u16, _val: u16) {
        unreachable!("nada escribe de a media palabra: ése es el bug del crate")
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        self.pon32((port & 0xffc) as usize, val)
    }
}

/// Una máquina de pega: una BAR con dirección y tamaño, y un mapeo que
/// devuelve un trozo de memoria del host.
struct FakeBar {
    /// Qué registro de BAR tiene algo.
    reg: u16,
    pa: u64,
    len: u64,
    /// La memoria que hace de BAR.
    mem: Vec<u32>,
    /// Qué se pidió mapear, para poder comprobarlo.
    mapeos: Vec<(u64, usize)>,
}

impl FakeBar {
    fn nueva(reg: u16, pa: u64, len: u64) -> Self {
        Self {
            reg,
            pa,
            len,
            mem: vec![0u32; 1024],
            mapeos: Vec::new(),
        }
    }
}

impl BarInfo for FakeBar {
    fn addr(&mut self, bar_reg: u16) -> u64 {
        if bar_reg == self.reg {
            self.pa
        } else {
            0
        }
    }
    fn len(&mut self, bar_reg: u16) -> u64 {
        if bar_reg == self.reg {
            self.len
        } else {
            0
        }
    }
    fn map(&mut self, pa: u64, len: usize) -> usize {
        self.mapeos.push((pa, len));
        self.mem.as_ptr() as usize
    }
}

const CAP_MSI: u16 = 0x50;
const CAP_MSIX: u16 = 0x60;

/// Amuebla una función con MSI en `0x50` y MSI-X en `0x60`, la tabla en la
/// BAR que diga `bir` y en el offset que diga `off`, con `vectores` vectores.
fn trasto(bir: u32, off: u32, vectores: u16) -> FakeCfg {
    let cfg = FakeCfg::nueva();
    cfg.pon8(0x34, CAP_MSI as u8);
    // MSI: id 0x05, siguiente 0x60, control con el bit de enable puesto.
    cfg.pon32(CAP_MSI as usize, 0x0001_0000 | (0x60 << 8) | 0x05);
    // MSI-X: id 0x11, `QSIZE` = vectores - 1, y el puntero al siguiente a
    // cero -- que es fin de cadena, y por eso no se suma: `| (0 << 8)` no
    // hace nada y clippy lo dice.
    cfg.pon32(CAP_MSIX as usize, ((vectores as u32 - 1) << 16) | 0x11);
    cfg.pon32(CAP_MSIX as usize + 4, off | bir);
    cfg.pon32(CAP_MSIX as usize + 8, off | bir);
    cfg
}

fn con_escotilla<T>(f: impl FnOnce() -> T) -> T {
    set_enabled(true);
    let r = f();
    set_enabled(false);
    r
}

// --- lo que se averigua del dispositivo ------------------------------------

#[test]
fn both_capabilities_are_found_in_one_walk() {
    let cfg = trasto(0, 0x1000, 4);
    let caps = unsafe { find_caps(&cfg, AM, LOC) };
    assert_eq!(caps.msi, Some(CAP_MSI));
    assert_eq!(caps.msix, Some(CAP_MSIX));
}

#[test]
fn a_chain_that_points_at_itself_does_not_walk_for_ever() {
    let cfg = FakeCfg::nueva();
    cfg.pon8(0x34, 0x50);
    // Id cualquiera, y el siguiente es él mismo.
    cfg.pon32(0x50, (0x50 << 8) | 0x09);
    let caps = unsafe { find_caps(&cfg, AM, LOC) };
    assert_eq!(caps, Caps::default());
}

#[test]
fn a_chain_that_goes_round_in_a_ring_does_not_walk_for_ever_either() {
    // El corte de «el puntero no avanza» sólo pilla un ciclo de UNO. Un ciclo
    // de dos lo pasa: 0x50 apunta a 0x60 y 0x60 vuelve a 0x50, y cada paso
    // avanza de verdad. Lo que corta eso es el número de pasos, y lo que este
    // test comprueba no es el valor que devuelve: es que **devuelva**.
    let cfg = FakeCfg::nueva();
    cfg.pon8(0x34, 0x50);
    cfg.pon32(0x50, (0x60 << 8) | 0x09);
    cfg.pon32(0x60, (0x50 << 8) | 0x09);
    let caps = unsafe { find_caps(&cfg, AM, LOC) };
    assert_eq!(caps, Caps::default());
}

#[test]
fn a_capability_pointer_inside_the_header_is_a_broken_chain() {
    let cfg = FakeCfg::nueva();
    // 0x30 está por debajo de 0x40: es cabecera, no capacidad. Y para que
    // esto compruebe el corte y no la casualidad de que ahí haya ceros, en
    // 0x30 hay algo que SE LEE como una capacidad MSI-X: en 0x30 vive la
    // dirección de la ROM de expansión, y una ROM en `0x…11` deja el byte
    // bajo a 0x11, que es justo el id de MSI-X.
    cfg.pon8(0x34, 0x30);
    cfg.pon32(0x30, 0x0003_0011);
    let caps = unsafe { find_caps(&cfg, AM, LOC) };
    assert_eq!(
        caps.msix, None,
        "una capacidad dentro de la cabecera no es una capacidad"
    );
}

#[test]
fn the_bir_names_a_register_and_not_a_bar() {
    // Esto es lo que separa un índice de registro de un número de BAR: con una
    // BAR0 de 64 bits, el registro 1 es su mitad alta y `BIR = 2` es la
    // SEGUNDA BAR, en 0x18 -- no la tercera.
    assert_eq!(bar_reg_de_bir(0), 0x10);
    assert_eq!(bar_reg_de_bir(1), 0x14);
    assert_eq!(bar_reg_de_bir(2), 0x18);
    assert_eq!(bar_reg_de_bir(5), 0x24);
}

#[test]
fn the_message_address_is_the_lapic_window_with_the_destination_in_it() {
    assert_eq!(lapic_msg_addr(0), 0xfee0_0000);
    assert_eq!(lapic_msg_addr(1), 0xfee0_1000);
    // El destino va en los bits 19:12, así que el id 0xff llega entero.
    assert_eq!(lapic_msg_addr(0xff), 0xfeef_f000);
}

// --- la escotilla ----------------------------------------------------------

#[test]
fn with_the_hatch_shut_not_even_the_capability_is_read() {
    let cfg = trasto(0, 0x1000, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    let r = unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar) };
    assert_eq!(r, Err(NoHubo::Escotilla));
    // Y MSI sigue encendido: no se ha tocado nada.
    assert_eq!(cfg.lee32(CAP_MSI as usize) >> 16 & 1, 1);
    assert!(bar.mapeos.is_empty());
}

#[test]
fn the_hatch_is_shut_unless_someone_opens_it() {
    assert!(!enabled());
}

// --- por qué puede no armarse ----------------------------------------------

#[test]
fn a_device_without_the_capability_keeps_the_msi_it_had() {
    let cfg = FakeCfg::nueva();
    cfg.pon8(0x34, CAP_MSI as u8);
    cfg.pon32(CAP_MSI as usize, 0x0001_0000 | 0x05);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    let r = con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar) });
    assert_eq!(r, Err(NoHubo::SinCapacidad));
    assert_eq!(cfg.lee32(CAP_MSI as usize) >> 16 & 1, 1, "MSI sigue puesto");
}

#[test]
fn a_reserved_bir_is_refused_before_any_bar_is_touched() {
    // 6 y 7 están reservados.
    let cfg = trasto(6, 0x1000, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    let r = con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar) });
    assert_eq!(r, Err(NoHubo::BirReservado));
    assert!(bar.mapeos.is_empty());
}

#[test]
fn a_bar_that_is_not_programmed_is_not_mapped() {
    // La tabla dice BAR 2 y la máquina sólo tiene la 0.
    let cfg = trasto(2, 0x1000, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    let r = con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar) });
    assert_eq!(r, Err(NoHubo::BarSinDireccion));
    assert!(bar.mapeos.is_empty());
}

#[test]
fn a_table_that_does_not_fit_in_its_bar_is_not_programmed() {
    // Cuatro vectores en el offset 0x1000 son 0x1040 bytes, y la BAR tiene
    // 0x1000: la última entrada caería fuera, encima de lo que haya detrás.
    let cfg = trasto(0, 0x1000, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x1000);
    let r = con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar) });
    assert_eq!(r, Err(NoHubo::TablaNoCabe));
    assert!(bar.mapeos.is_empty());
}

#[test]
fn msi_is_still_on_after_every_refusal() {
    // El orden de la cabecera: apagar MSI sólo cuando ya se sabe que MSI-X se
    // va a poder armar. Al revés, un dispositivo con la tabla fuera de la BAR
    // se quedaría sin ninguna de las dos.
    for cfg in [trasto(6, 0x1000, 4), trasto(2, 0x1000, 4)] {
        let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x1000);
        let _ = con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar) });
        assert_eq!(cfg.lee32(CAP_MSI as usize) >> 16 & 1, 1);
    }
}

// --- y cuando sí ------------------------------------------------------------

#[test]
fn arming_turns_msi_off_and_msix_on() {
    let cfg = trasto(0, 0x100, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    let r = con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar) });
    let armado = r.expect("se tenía que armar");
    assert_eq!(armado.cap.table_size, 4);
    assert_eq!(armado.cap.table_offset, 0x100);
    assert_eq!(
        cfg.lee32(CAP_MSI as usize) >> 16 & 1,
        0,
        "MSI se tenía que apagar"
    );
    let ctrl = (cfg.lee32(CAP_MSIX as usize) >> 16) as u16;
    assert_eq!(ctrl & super::super::MSIX_FLAGS_ENABLE, 0x8000);
    assert_eq!(ctrl & super::super::MSIX_FLAGS_MASKALL, 0, "MASKALL fuera");
    // Y el `QSIZE` no se ha pisado al escribir el control.
    assert_eq!(ctrl & super::super::MSIX_FLAGS_QSIZE, 3);
}

#[test]
fn the_capability_id_and_next_pointer_survive_the_control_write() {
    // Ésta es la razón de que el control se escriba de palabra entera: en
    // `0x62` el crate `pci` pondría la media palabra en la mitad baja, que es
    // justo donde viven el id y el puntero al siguiente.
    let cfg = trasto(0, 0x100, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar).unwrap() });
    assert_eq!(cfg.lee32(CAP_MSIX as usize) & 0xffff, 0x11);
}

#[test]
fn the_mapping_asked_for_covers_the_offset_and_the_whole_table() {
    // El offset es parte de lo que hay que mapear: la tabla no empieza en el
    // principio de la BAR.
    let cfg = trasto(0, 0x100, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar).unwrap() });
    assert_eq!(bar.mapeos, vec![(0x8000_0000, 0x100 + 4 * 16)]);
}

#[test]
fn the_message_that_lands_in_the_table_is_the_lapic_one_and_the_vector_is_offset_by_thirty_two() {
    let cfg = trasto(0, 0x100, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 7, 5, &mut bar).unwrap() });
    let palabra = |i: usize| bar.mem[0x100 / 4 + i];
    assert_eq!(palabra(0), 0xfee0_7000, "dirección baja");
    assert_eq!(palabra(1), 0, "la mitad alta se escribe siempre");
    assert_eq!(palabra(2), 5 + 32, "el dato es el irq + 32");
    assert_eq!(palabra(3) & 1, 0, "y la entrada queda destapada");
}

#[test]
fn only_the_first_vector_is_armed_and_the_rest_stay_masked() {
    // La tabla de pega arranca a ceros, que es lo contrario de lo que hace el
    // silicio, así que esto no comprueba el bit de reset: comprueba que no se
    // ha escrito nada en las otras entradas.
    let cfg = trasto(0, 0x100, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar).unwrap() });
    for v in 1..4 {
        for i in 0..4 {
            assert_eq!(
                bar.mem[0x100 / 4 + v * 4 + i],
                0,
                "vector {}, palabra {}",
                v,
                i
            );
        }
    }
}

#[test]
fn a_device_with_one_single_vector_declares_zero_and_still_arms() {
    // `QSIZE` vale uno menos que los vectores que hay: un dispositivo con un
    // vector declara cero, y es el caso que más fácil se lee como «ninguno».
    let cfg = trasto(0, 0x100, 1);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    let armado =
        con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar).unwrap() });
    assert_eq!(armado.cap.table_size, 1);
    assert_eq!(bar.mem[0x100 / 4 + 2], 1 + 32);
}

#[test]
fn the_table_offset_comes_out_without_the_bir_in_it() {
    // El `BIR` comparte palabra con el offset. Sin enmascarar, una tabla en
    // BAR 2 saldría en el offset 0x102 en vez de 0x100: a mitad de entrada.
    let cfg = trasto(2, 0x100, 4);
    let mut bar = FakeBar::nueva(0x18, 0x9000_0000, 0x10000);
    let armado =
        con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 0, 1, &mut bar).unwrap() });
    assert_eq!(armado.cap.table_offset, 0x100);
    assert_eq!(bar.mapeos, vec![(0x9000_0000, 0x100 + 4 * 16)]);
}

#[test]
fn the_entry_reads_back_exactly_what_was_programmed_into_it() {
    // Esto es lo único que dice, sin esperar una interrupción, que la tabla
    // estaba donde la capacidad decía: con el `BIR` o el offset mal, lo
    // escrito cae en otro sitio de la BAR y esto devuelve otra cosa.
    let cfg = trasto(0, 0x100, 4);
    let mut bar = FakeBar::nueva(0x10, 0x8000_0000, 0x10000);
    let armado =
        con_escotilla(|| unsafe { arm_first_vector(&cfg, AM, LOC, 7, 5, &mut bar).unwrap() });
    assert_eq!(armado.entrada, [0xfee0_7000, 0, 5 + 32, 0]);
}
