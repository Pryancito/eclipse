//! Lo que se puede comprobar de MSI-X sin un dispositivo: qué se lee de la
//! capacidad, dónde cae cada entrada y **en qué orden** se arma un vector. Lo
//! último es lo que no recuerda ni un espacio de configuración falso ni un
//! trozo de memoria, así que va por traza.

use super::*;
use alloc::boxed::Box;
use lock::Mutex;

// --- un espacio de configuración de mentira --------------------------------

/// 256 bytes de espacio de configuración, como 64 palabras. Se direcciona por
/// el camino de memoria, que es el que pasa el offset tal cual.
struct FakeCfg {
    space: Mutex<[u32; 64]>,
}

impl FakeCfg {
    fn new() -> Self {
        Self {
            space: Mutex::new([0u32; 64]),
        }
    }
    fn pon(&self, off: u16, val: u32) {
        self.space.lock()[off as usize / 4] = val;
    }
    fn saca(&self, off: u16) -> u32 {
        self.space.lock()[off as usize / 4]
    }
}

impl PortOps for FakeCfg {
    unsafe fn read8(&self, port: u16) -> u8 {
        (self.saca(port & !0b11) >> ((port as usize & 0b11) * 8)) as u8
    }
    unsafe fn read16(&self, port: u16) -> u16 {
        (self.saca(port & !0b11) >> ((port as usize & 0b10) * 8)) as u16
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        self.saca((port & 0xff) as u16)
    }
    unsafe fn write8(&self, _port: u16, _val: u8) {
        unreachable!("el camino de memoria no llama a write8")
    }
    unsafe fn write16(&self, _port: u16, _val: u16) {
        unreachable!("el camino de memoria no llama a write16")
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        // En la MISMA traza que la tabla, con una marca que los distingue:
        // el orden que hay que comprobar es justo el de entre los dos.
        msix_fake::apunta(MARCA_CFG + (port & 0xff) as usize, val);
        self.pon((port & 0xff) as u16, val)
    }
}

/// Lo que se suma al offset de una escritura al espacio de configuración para
/// distinguirla de una a la tabla, dentro de la traza compartida.
const MARCA_CFG: usize = 0x1_0000;

const AM: CSpaceAccessMethod = CSpaceAccessMethod::MemoryMapped(core::ptr::null_mut());
const LOC: Location = Location {
    bus: 0,
    device: 0,
    function: 0,
};
/// Un `cap_ptr` **de los que delatan el bug de `write16` del crate**: con
/// `0x60`, `0x62 & 0x10` es cero y la media palabra se escribiría en la mitad
/// de abajo. Si algo de aquí usase `write16`, se vería con este puntero y no
/// con `0x70`.
const CAP: u16 = 0x60;

/// Monta una capacidad MSI-X en `CAP` y devuelve el espacio.
fn con_capacidad(qsize: u16, tabla: u32, pba: u32) -> FakeCfg {
    let cfg = FakeCfg::new();
    // El id de la capacidad en el byte 0, el puntero al siguiente en el 1
    // (cero: fin de la cadena) y el control en la mitad alta.
    cfg.pon(CAP, u32::from(CAP_ID_MSIX) | ((qsize as u32) << 16));
    cfg.pon(CAP + MSIX_TABLE, tabla);
    cfg.pon(CAP + MSIX_PBA, pba);
    cfg
}

fn cap(cfg: &FakeCfg) -> Option<MsixCap> {
    unsafe { MsixCap::read(cfg, AM, LOC, CAP) }
}

// --- una tabla de mentira --------------------------------------------------

fn tabla(entradas: usize) -> (MsixTable, usize) {
    let banco: &'static mut [u32] =
        Box::leak(alloc::vec![0u32; entradas * ENTRY_SIZE / 4].into_boxed_slice());
    // Tras un reset toda entrada está **tapada**: eso es lo que hay que
    // imitar, porque un banco a ceros diría lo contrario.
    for i in 0..entradas {
        banco[i * ENTRY_SIZE / 4 + ENTRY_VECTOR_CTRL / 4] = ENTRY_CTRL_MASKBIT;
    }
    let base = banco.as_mut_ptr() as usize;
    (unsafe { MsixTable::new(base, entradas) }, base)
}

fn palabra(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

fn escrituras(traza: &[msix_fake::Entrada]) -> alloc::vec::Vec<(usize, u32)> {
    traza
        .iter()
        .filter_map(|(off, val)| val.map(|v| (*off, v)))
        .collect()
}

// --- cuántos vectores hay --------------------------------------------------

/// `QSIZE` vale **uno menos** que los vectores. Un dispositivo con un solo
/// vector declara cero, así que leerlo tal cual da una tabla de cero entradas
/// y ningún vector que armar.
#[test]
fn the_table_size_is_one_more_than_the_field_says() {
    for (qsize, esperado) in [(0u16, 1usize), (1, 2), (31, 32), (0x7ff, 2048)] {
        let cfg = con_capacidad(qsize, 0, 0);
        assert_eq!(
            cap(&cfg).unwrap().table_size,
            esperado,
            "qsize {} son {} vectores",
            qsize,
            esperado
        );
    }
}

/// Y el campo son once bits, así que lo que haya por encima no es tamaño: son
/// `MASKALL` y `ENABLE`, y colarlos daría una tabla absurda.
#[test]
fn the_bits_above_the_size_field_are_maskall_and_enable_and_not_size() {
    let cfg = con_capacidad(MSIX_FLAGS_ENABLE | MSIX_FLAGS_MASKALL | 7, 0, 0);
    assert_eq!(cap(&cfg).unwrap().table_size, 8);
    assert_eq!(MAX_VECTORS, 2048);
    assert_eq!(MSIX_FLAGS_QSIZE, 0x07ff, "once bits");
}

// --- dónde está la tabla ---------------------------------------------------

/// El BIR y el offset **comparten palabra**: los tres bits de abajo son el
/// BIR. Un offset sin enmascarar sale hasta siete bytes corrido, o sea a
/// mitad de una entrada, y entonces la dirección de un mensaje se escribe
/// encima del control de la entrada anterior.
#[test]
fn the_offset_has_to_be_masked_because_the_bar_index_lives_in_its_low_bits() {
    // BAR 2, offset 0x3000.
    let cfg = con_capacidad(0, 0x3000 | 2, 0);
    let c = cap(&cfg).unwrap();
    assert_eq!(c.table_bir, 2);
    assert_eq!(c.table_offset, 0x3000, "sin los tres bits del BIR");
    assert_ne!(c.table_offset, 0x3002, "el BIR se coló en el offset");
    assert_eq!(MSIX_OFFSET, 0xffff_fff8);
    assert_eq!(MSIX_BIR, 0x7);
}

/// El del array de pendientes es otra palabra, y puede señalar **la misma
/// BAR** con otro offset. Si se leyesen de la misma palabra, uno de los dos
/// sería el otro.
#[test]
fn the_pending_array_has_its_own_word_and_may_share_the_bar() {
    let cfg = con_capacidad(0, 0x3000 | 2, 0x4000 | 2);
    let c = cap(&cfg).unwrap();
    assert_eq!((c.table_bir, c.table_offset), (2, 0x3000));
    assert_eq!((c.pba_bir, c.pba_offset), (2, 0x4000));
}

/// Los BIR 6 y 7 están **reservados**: no son BAR. Seguir con uno de ellos
/// sería mapear la BAR 6, que no existe, así que la capacidad se rechaza.
#[test]
fn a_reserved_bar_index_is_refused_instead_of_mapped() {
    for bir in [6u32, 7] {
        let cfg = con_capacidad(0, 0x3000 | bir, 0);
        assert!(cap(&cfg).is_none(), "BIR {} se aceptó", bir);
    }
    for bir in 0..=5u32 {
        let cfg = con_capacidad(0, 0x3000 | bir, 0);
        assert_eq!(cap(&cfg).unwrap().table_bir, bir);
    }
    assert_eq!(MAX_BIR, 5);
}

/// Una tabla que no cabe en su BAR no es un dispositivo raro: es un `QSIZE` o
/// un offset mal leídos. Programarla igual sería escribir fuera de la BAR.
#[test]
fn a_table_that_does_not_fit_in_its_bar_is_caught() {
    // 8 vectores (qsize 7) en el offset 0x3000: la tabla acaba en 0x3080.
    let cfg = con_capacidad(7, 0x3000 | 2, 0);
    let c = cap(&cfg).unwrap();
    assert_eq!(c.table_size * ENTRY_SIZE, 0x80);
    assert!(c.table_fits(0x3080), "justo cabe");
    assert!(!c.table_fits(0x307f), "un byte de menos no cabe");
    assert!(c.table_fits(0x4000));
}

// --- dónde cae cada entrada ------------------------------------------------

/// Una entrada son 16 bytes, y los cuatro campos están donde dice la
/// especificación. El control en `0xc` es el que importa: escribir el dato
/// ahí **taparía o destaparía** la entrada en vez de programarla.
#[test]
fn an_entry_is_sixteen_bytes_with_the_control_last() {
    assert_eq!(ENTRY_SIZE, 16);
    assert_eq!(ENTRY_LOWER_ADDR, 0x0);
    assert_eq!(ENTRY_UPPER_ADDR, 0x4);
    assert_eq!(ENTRY_DATA, 0x8);
    assert_eq!(ENTRY_VECTOR_CTRL, 0xc);
    assert_eq!(entry_offset(0), 0);
    assert_eq!(entry_offset(1), 16);
    assert_eq!(entry_offset(31), 496);
}

/// Un vector que no existe no tiene offset. Si lo tuviese, armarlo escribiría
/// pasado el final de la tabla, que es memoria de otro.
#[test]
fn a_vector_past_the_end_of_the_table_has_no_offset() {
    let cfg = con_capacidad(3, 0, 0);
    let c = cap(&cfg).unwrap();
    assert_eq!(c.table_size, 4);
    assert_eq!(c.entry_offset(3), Some(48));
    assert_eq!(c.entry_offset(4), None);
    assert_eq!(c.entry_offset(usize::MAX), None);
}

// --- programar una entrada -------------------------------------------------

/// La mitad alta de la dirección **se escribe siempre**, aunque sea cero. En
/// MSI hay un bit de capacidad que dice si el dispositivo es de 64 bits y hay
/// que mirarlo; en MSI-X no hay nada que mirar, las dos palabras existen, y
/// una sin escribir deja la dirección a medias con lo que hubiese.
#[test]
fn the_upper_half_of_the_address_is_always_written_even_when_it_is_zero() {
    let (t, base) = tabla(2);
    // Basura en la mitad alta, como la dejaría otro arranque.
    unsafe { core::ptr::write_volatile((base + ENTRY_UPPER_ADDR) as *mut u32, 0xdead_beef) };
    let (ok, traza) = msix_fake::trazando(|| t.program(0, 0xfee0_0000, 42));
    assert!(ok);
    assert_eq!(palabra(base, ENTRY_LOWER_ADDR), 0xfee0_0000);
    assert_eq!(palabra(base, ENTRY_UPPER_ADDR), 0, "la basura se quedó");
    assert_eq!(palabra(base, ENTRY_DATA), 42);
    assert!(
        escrituras(&traza)
            .iter()
            .any(|(off, _)| *off == ENTRY_UPPER_ADDR),
        "no se escribió la mitad alta: {:?}",
        traza
    );
}

/// Y una dirección de verdad de 64 bits se parte por la mitad, no se trunca.
#[test]
fn a_real_sixty_four_bit_address_is_split_and_not_truncated() {
    let (t, base) = tabla(1);
    t.program(0, 0x0000_0012_3456_7000, 7);
    assert_eq!(palabra(base, ENTRY_LOWER_ADDR), 0x3456_7000);
    assert_eq!(palabra(base, ENTRY_UPPER_ADDR), 0x12);
}

/// Programar **no destapa**. Son dos cosas, y separarlas es lo que permite
/// dejar la entrada tapada mientras se programa.
#[test]
fn programming_an_entry_does_not_unmask_it() {
    let (t, base) = tabla(1);
    assert_eq!(t.is_masked(0), Some(true), "arranca tapada");
    t.program(0, 0xfee0_0000, 1);
    assert_eq!(
        palabra(base, ENTRY_VECTOR_CTRL) & ENTRY_CTRL_MASKBIT,
        ENTRY_CTRL_MASKBIT
    );
    assert_eq!(t.is_masked(0), Some(true));
}

/// Cada entrada es la suya: programar el vector 0 no toca el 1. Con el offset
/// mal calculado, el segundo mensaje caería encima del primero.
#[test]
fn each_entry_is_sixteen_bytes_further_along() {
    let (t, base) = tabla(3);
    t.program(0, 0xaaaa_0000, 1);
    t.program(1, 0xbbbb_0000, 2);
    t.program(2, 0xcccc_0000, 3);
    assert_eq!(palabra(base, ENTRY_LOWER_ADDR), 0xaaaa_0000);
    assert_eq!(palabra(base, 16 + ENTRY_LOWER_ADDR), 0xbbbb_0000);
    assert_eq!(palabra(base, 32 + ENTRY_LOWER_ADDR), 0xcccc_0000);
    assert_eq!(palabra(base, ENTRY_DATA), 1);
    assert_eq!(palabra(base, 16 + ENTRY_DATA), 2);
    assert_eq!(palabra(base, 32 + ENTRY_DATA), 3);
}

/// Un vector que no existe no se programa, y no se escribe nada al
/// intentarlo.
#[test]
fn a_vector_the_table_does_not_have_is_refused_without_writing() {
    let (t, _) = tabla(2);
    let (_, traza) = msix_fake::trazando(|| {
        assert!(!t.program(2, 0xfee0_0000, 1));
        assert!(!t.mask(2));
        assert!(!t.unmask(2));
        assert_eq!(t.is_masked(2), None);
    });
    assert!(
        escrituras(&traza).is_empty(),
        "se escribió fuera de la tabla: {:?}",
        traza
    );
}

// --- tapar y destapar ------------------------------------------------------

/// El bit de tapado es **el 0**, contra el literal y no contra la constante:
/// una constante comparada consigo misma no dice cuál es su valor, y aquí el
/// valor es lo que decide si una entrada que arranca tapada se ve tapada.
#[test]
fn the_mask_bit_is_bit_zero_of_the_control_word() {
    assert_eq!(ENTRY_CTRL_MASKBIT, 1);
    let (t, base) = tabla(1);
    // Tal como lo deja un reset: sólo el bit 0 puesto.
    unsafe { core::ptr::write_volatile((base + ENTRY_VECTOR_CTRL) as *mut u32, 1) };
    assert_eq!(t.is_masked(0), Some(true));
    t.unmask(0);
    assert_eq!(palabra(base, ENTRY_VECTOR_CTRL), 0);
    t.mask(0);
    assert_eq!(palabra(base, ENTRY_VECTOR_CTRL), 1);
    // Y con sólo el bit 1 puesto la entrada NO está tapada.
    unsafe { core::ptr::write_volatile((base + ENTRY_VECTOR_CTRL) as *mut u32, 2) };
    assert_eq!(t.is_masked(0), Some(false), "el bit 1 no tapa");
}

/// Destapar y tapar tocan **sólo el bit 0** del control. Los demás bits de esa
/// palabra están reservados, y escribirla entera les metería ceros.
#[test]
fn masking_touches_only_the_low_bit_of_the_control_word() {
    let (t, base) = tabla(1);
    // Algo en los bits reservados, para ver si sobrevive.
    unsafe {
        core::ptr::write_volatile(
            (base + ENTRY_VECTOR_CTRL) as *mut u32,
            0x5555_0000 | ENTRY_CTRL_MASKBIT,
        )
    };
    assert!(t.unmask(0));
    assert_eq!(palabra(base, ENTRY_VECTOR_CTRL), 0x5555_0000);
    assert_eq!(t.is_masked(0), Some(false));
    assert!(t.mask(0));
    assert_eq!(
        palabra(base, ENTRY_VECTOR_CTRL),
        0x5555_0000 | ENTRY_CTRL_MASKBIT
    );
}

// --- armar un vector -------------------------------------------------------

/// El orden entero, que es lo que esto tiene de difícil:
///
/// 1. `ENABLE | MASKALL` — encendido pero todo tapado;
/// 2. el mensaje;
/// 3. la entrada destapada;
/// 4. `MASKALL` fuera.
///
/// Si `MASKALL` se quitase antes de programar, el dispositivo podría soltar un
/// mensaje con la dirección a medias. Y si no se desenmascarase la entrada,
/// quedaría un dispositivo que se encuentra, se reclama y no interrumpe nunca.
#[test]
fn arming_a_vector_follows_the_whole_order() {
    let cfg = con_capacidad(3, 0x3000 | 2, 0x4000 | 2);
    let c = cap(&cfg).unwrap();
    let (t, base) = tabla(4);

    let (ok, traza) =
        msix_fake::trazando(|| unsafe { arm(&cfg, AM, LOC, &c, &t, 1, 0xfee0_1000, 0x21) });
    assert!(ok);

    // El mensaje está puesto y la entrada destapada.
    assert_eq!(palabra(base, 16 + ENTRY_LOWER_ADDR), 0xfee0_1000);
    assert_eq!(palabra(base, 16 + ENTRY_UPPER_ADDR), 0);
    assert_eq!(palabra(base, 16 + ENTRY_DATA), 0x21);
    assert_eq!(t.is_masked(1), Some(false), "quedó tapada");

    // Y el control acabó encendido y sin MASKALL.
    let flags = flags_de_palabra(cfg.saca(CAP));
    assert_eq!(flags & MSIX_FLAGS_ENABLE, MSIX_FLAGS_ENABLE);
    assert_eq!(flags & MSIX_FLAGS_MASKALL, 0, "MASKALL se quedó puesto");

    // El orden dentro de la tabla: la dirección antes del destapado.
    let w = escrituras(&traza);
    let pos = |off: usize| w.iter().position(|(o, _)| *o == off);
    let dir = pos(16 + ENTRY_LOWER_ADDR).expect("no se escribió la dirección");
    let ctrl = pos(16 + ENTRY_VECTOR_CTRL).expect("no se destapó");
    assert!(dir < ctrl, "se destapó antes de programar: {:?}", traza);
}

/// Mientras se programa, `MASKALL` tiene que estar **puesto**, y eso sólo se
/// ve mirando el control y la tabla en la misma traza: el control se escribe
/// dos veces, la primera con `MASKALL` y la última sin él, y **todas** las
/// escrituras a la tabla caen entre las dos.
///
/// Si `MASKALL` se quitase antes de programar, el dispositivo podría soltar un
/// mensaje con la dirección a medias, y eso no deja rastro en ningún registro.
#[test]
fn maskall_is_on_for_the_whole_time_the_table_is_being_programmed() {
    let cfg = con_capacidad(0, 0x3000 | 2, 0);
    let c = cap(&cfg).unwrap();
    let (t, _) = tabla(1);

    let (_, traza) =
        msix_fake::trazando(|| unsafe { arm(&cfg, AM, LOC, &c, &t, 0, 0xfee0_0000, 1) });
    let w = escrituras(&traza);

    let al_control: alloc::vec::Vec<(usize, u16)> = w
        .iter()
        .enumerate()
        .filter(|(_, (off, _))| *off == MARCA_CFG + CAP as usize)
        .map(|(i, (_, val))| (i, flags_de_palabra(*val)))
        .collect();
    assert_eq!(
        al_control.len(),
        2,
        "el control se escribe dos veces, no {}: {:?}",
        al_control.len(),
        traza
    );
    let (i_primera, f_primera) = al_control[0];
    let (i_ultima, f_ultima) = al_control[1];
    assert_eq!(
        f_primera & (MSIX_FLAGS_ENABLE | MSIX_FLAGS_MASKALL),
        MSIX_FLAGS_ENABLE | MSIX_FLAGS_MASKALL,
        "la primera escritura no encendió con MASKALL"
    );
    assert_eq!(f_ultima & MSIX_FLAGS_ENABLE, MSIX_FLAGS_ENABLE);
    assert_eq!(f_ultima & MSIX_FLAGS_MASKALL, 0, "MASKALL se quedó puesto");

    let a_la_tabla: alloc::vec::Vec<usize> = w
        .iter()
        .enumerate()
        .filter(|(_, (off, _))| *off < MARCA_CFG)
        .map(|(i, _)| i)
        .collect();
    assert!(!a_la_tabla.is_empty(), "no se tocó la tabla: {:?}", traza);
    for i in a_la_tabla {
        assert!(
            i_primera < i && i < i_ultima,
            "una escritura a la tabla ({}) cayó fuera de la ventana de \
             MASKALL ({}..{}): {:?}",
            i,
            i_primera,
            i_ultima,
            traza
        );
    }
}

/// Armar **no pisa el id de la capacidad ni el puntero al siguiente**, que
/// viven en la mitad baja de la misma palabra. Pisarlos rompería el recorrido
/// de capacidades del siguiente que pase, y no se vería hasta entonces.
#[test]
fn arming_leaves_the_capability_id_and_next_pointer_alone() {
    let cfg = FakeCfg::new();
    cfg.pon(CAP, u32::from(CAP_ID_MSIX) | (0x80 << 8));
    cfg.pon(CAP + MSIX_TABLE, 0x3000 | 2);
    let c = cap(&cfg).unwrap();
    let (t, _) = tabla(1);
    unsafe { arm(&cfg, AM, LOC, &c, &t, 0, 0xfee0_0000, 1) };
    let dw = cfg.saca(CAP);
    assert_eq!(dw & 0xff, u32::from(CAP_ID_MSIX), "el id");
    assert_eq!((dw >> 8) & 0xff, 0x80, "el puntero al siguiente");
}

/// Un vector que no existe no arma, y **no deja la capacidad encendida**: si
/// la encendiese y luego fallase, quedaría un MSI-X activo con la tabla sin
/// programar y todas las entradas tapadas, que es un dispositivo mudo sin
/// nada que lo delate.
#[test]
fn a_vector_that_does_not_exist_does_not_leave_the_capability_enabled() {
    let cfg = con_capacidad(1, 0x3000 | 2, 0);
    let c = cap(&cfg).unwrap();
    let (t, _) = tabla(2);
    assert!(!unsafe { arm(&cfg, AM, LOC, &c, &t, 2, 0xfee0_0000, 1) });
    assert_eq!(
        flags_de_palabra(cfg.saca(CAP)) & MSIX_FLAGS_ENABLE,
        0,
        "se quedó encendido"
    );
}

/// Y si la tabla mapeada tiene menos entradas de las que dice la capacidad,
/// tampoco: el que manda para escribir es el mapeo, no lo que el dispositivo
/// declara.
#[test]
fn the_mapping_and_not_the_capability_decides_what_can_be_written() {
    let cfg = con_capacidad(7, 0x3000 | 2, 0);
    let c = cap(&cfg).unwrap();
    assert_eq!(c.table_size, 8);
    let (t, _) = tabla(2);
    assert_eq!(t.entries(), 2);
    assert!(!unsafe { arm(&cfg, AM, LOC, &c, &t, 5, 0xfee0_0000, 1) });
}

// --- MSI y MSI-X -----------------------------------------------------------

/// MSI y MSI-X **no pueden estar encendidos los dos**, así que apagar MSI es
/// parte de encender MSI-X. Y se apaga tocando sólo su bit 0, sin pisar el id
/// ni el puntero de su propia capacidad.
#[test]
fn turning_off_msi_clears_its_enable_bit_and_nothing_else() {
    let cfg = FakeCfg::new();
    const MSI: u16 = 0x50;
    // id 0x05, siguiente 0x60, control con el enable puesto y algo más.
    cfg.pon(MSI, 0x05 | (0x60 << 8) | ((0x0081u32) << 16));
    unsafe { disable_msi(&cfg, AM, LOC, MSI) };
    let dw = cfg.saca(MSI);
    assert_eq!(dw & 0xff, 0x05, "el id");
    assert_eq!((dw >> 8) & 0xff, 0x60, "el puntero al siguiente");
    assert_eq!(dw >> 16, 0x0080, "sólo se fue el bit 0 del control");
}

// --- la media palabra del crate --------------------------------------------

/// Nada de aquí usa `CSpaceAccessMethod::write16` ni `write8`, y el espacio
/// falso lo hace cumplir: sus `write16`/`write8` son `unreachable!`.
///
/// La razón es un paréntesis que falta en el crate `pci`: `write16` calcula
/// `offset as usize & 0b10 << 3`, y `<<` liga más que `&`, así que acaba
/// haciendo `offset & 0x10` en vez de `(offset & 0b10) << 3`. Este test fija
/// la cuenta para que se vea que con el `cap_ptr` de estos tests (`0x60`) sale
/// mal, y que por eso el control se toca de palabra entera.
#[test]
fn the_crates_half_word_write_would_land_in_the_wrong_half_here() {
    let off = (CAP + MSIX_FLAGS) as usize;
    let bueno = (off & 0b10) << 3;
    let del_crate = off & (0b10 << 3);
    assert_eq!(bueno, 16, "el control está en la mitad alta");
    assert_eq!(del_crate, 0, "el crate lo pondría en la baja");
    assert_ne!(bueno, del_crate);
    // Con un cap_ptr de 0x70 coincidirían, que es por lo que el bug no salta
    // siempre.
    let off70 = 0x70usize + MSIX_FLAGS as usize;
    assert_eq!((off70 & 0b10) << 3, off70 & (0b10 << 3));
}

/// El control es la mitad alta de la palabra que empieza en el puntero de la
/// capacidad, y la vuelta deja la mitad baja intacta.
#[test]
fn the_control_is_the_high_half_of_the_word_at_the_capability_pointer() {
    assert_eq!(MSIX_FLAGS, 2);
    assert_eq!(flags_de_palabra(0x8003_1105), 0x8003);
    assert_eq!(palabra_con_flags(0x8003_1105, 0x4000), 0x4000_1105);
    assert_eq!(
        palabra_con_flags(0xffff_1105, 0),
        0x0000_1105,
        "el id y el siguiente se quedan"
    );
}
