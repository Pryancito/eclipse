//! Lo que hay que comprobar de un espacio de configuración con estado no es
//! qué valor sale, sino **qué se tocó y en qué orden**: el índice antes del
//! dato, el índice una sola vez por par, y nada en absoluto cuando no hay
//! dispositivo. Un banco de registros no recuerda nada de eso, así que se mira
//! la traza.

use super::*;
use crate::bus::brcmstb::bridge_fake;
use alloc::boxed::Box;

const VENTANA: usize = WIN_LEN_2712 / 4 + 1;

/// El bus raíz que se usa en todos los tests. **No es 0 a propósito**: un
/// `root_bus` que se colase como constante se vería.
const RAIZ: u8 = 1;

struct Trasto {
    ops: BrcmCfgOps,
    base: usize,
}

/// Un puente con el enlace **arriba**, que es lo único con lo que un acceso a
/// un dispositivo de detrás está permitido.
fn trasto() -> Trasto {
    let banco: &'static mut [u32] = Box::leak(alloc::vec![0u32; VENTANA].into_boxed_slice());
    let base = banco.as_mut_ptr() as usize;
    poke(
        base,
        MISC_PCIE_STATUS,
        PCIE_STATUS_DL_ACTIVE | PCIE_STATUS_PHYLINKUP,
    );
    Trasto {
        ops: unsafe { BrcmCfgOps::new(base, RAIZ) },
        base,
    }
}

/// El mismo, con el enlace **caído**.
fn trasto_sin_enlace() -> Trasto {
    let banco: &'static mut [u32] = Box::leak(alloc::vec![0u32; VENTANA].into_boxed_slice());
    let base = banco.as_mut_ptr() as usize;
    Trasto {
        ops: unsafe { BrcmCfgOps::new(base, RAIZ) },
        base,
    }
}

/// El mismo, con el puente en el **bus 0**. Hace falta para los accesos de
/// menos de una palabra: su `port` es un `u16`, y ahí no cabe un número de bus
/// (empieza en el bit 20). Ver
/// [`a_bus_number_does_not_fit_in_a_sixteen_bit_port`].
fn trasto_raiz0() -> Trasto {
    let banco: &'static mut [u32] = Box::leak(alloc::vec![0u32; VENTANA].into_boxed_slice());
    let base = banco.as_mut_ptr() as usize;
    poke(
        base,
        MISC_PCIE_STATUS,
        PCIE_STATUS_DL_ACTIVE | PCIE_STATUS_PHYLINKUP,
    );
    Trasto {
        ops: unsafe { BrcmCfgOps::new(base, 0) },
        base,
    }
}

/// Un `port` de los que caben en 16 bits: bus 0 y un `devfn` de cuatro bits.
fn puerto16(devfn: u8, reg: u32) -> u16 {
    assert!(devfn <= 0xf, "un devfn mayor no cabe en un port de 16 bits");
    ecam_offset(0, devfn, reg) as u16
}

fn reg(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

fn poke(base: usize, off: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}

/// Las escrituras de la traza, en orden, sólo las del puente.
fn escrituras(traza: &[bridge_fake::Entrada]) -> alloc::vec::Vec<(usize, u32)> {
    traza
        .iter()
        .filter(|(f, _, _)| *f == bridge_fake::PUENTE)
        .map(|(_, off, val)| (*off, *val))
        .collect()
}

// --- descomponer el offset -------------------------------------------------

/// El offset que llega por `PortOps` es el que compuso `ecam_offset`, y los
/// campos no se solapan, así que la vuelta es exacta. Si no lo fuese, un
/// dispositivo se leería como otro.
#[test]
fn taking_the_ecam_offset_apart_gives_back_exactly_what_built_it() {
    for &(bus, devfn, reg) in &[
        (0u8, 0u8, 0u32),
        (1, 0, 0x04),
        (0xff, 0xff, 0xffc),
        (0x2a, 0x08, 0x40),
        (1, 0x18, 0x34),
    ] {
        let port = ecam_offset(bus, devfn, reg);
        assert_eq!(
            decode_port(port),
            (bus, devfn, reg),
            "bus {:#x} devfn {:#x} reg {:#x}",
            bus,
            devfn,
            reg
        );
    }
}

/// El `devfn` del ECAM es **ocho bits**, el dispositivo y la función juntos, y
/// es la misma composición que usa `CSpaceAccessMethod::MemoryMapped`
/// (`device << 15 | function << 12`). Si fueran campos distintos, la función 7
/// del dispositivo 3 se leería como otra cosa.
#[test]
fn the_device_and_the_function_are_one_eight_bit_field_and_not_two() {
    let (device, function) = (3u32, 7u32);
    let del_crate = (device << 15) | (function << 12);
    let devfn = ((device << 3) | function) as u8;
    assert_eq!(del_crate, ecam_offset(0, devfn, 0));
    assert_eq!(decode_port(del_crate), (0, devfn, 0));
}

/// El registro se queda en 12 bits. Lo que sobra **no se sale de la ventana**:
/// se mete justo en el registro de índice, porque los 4 KiB de datos acaban
/// pegados a él. Por eso el enmascarado no es prolijidad.
#[test]
fn an_offset_past_the_four_kilobytes_would_land_on_the_index_register_itself() {
    assert_eq!(EXT_CFG_DATA + 0x1000, EXT_CFG_INDEX);
    let (_, _, reg) = decode_port(0x1000);
    assert_eq!(reg, 0, "el 0x1000 se queda fuera del registro");
}

// --- el puente mismo -------------------------------------------------------

/// Los registros del puente están en la ventana tal cual: **sin índice de por
/// medio**. Un índice escrito aquí sería mirar a otro sitio.
#[test]
fn the_bridge_reads_its_own_registers_without_touching_the_index() {
    let t = trasto();
    poke(t.base, 0x04, 0xdead_beef);
    let (val, traza) =
        bridge_fake::trazando(|| unsafe { t.ops.read32(ecam_offset(RAIZ, 0, 0x04)) });
    assert_eq!(val, 0xdead_beef);
    assert!(
        bridge_fake::primera(&traza, bridge_fake::PUENTE, EXT_CFG_INDEX).is_none(),
        "se escribió el índice para un registro del puente: {:?}",
        traza
    );
}

/// En el bus raíz **sólo existe el puente**. Cualquier otra función de ese bus
/// no es un dispositivo que no contesta: no existe, y se lee como un hueco.
#[test]
fn the_root_bus_has_exactly_one_function_and_the_rest_are_holes() {
    let t = trasto();
    poke(t.base, 0x00, 0x1234_5678);
    for devfn in [1u8, 7, 8, 0xff] {
        let val = unsafe { t.ops.read32(ecam_offset(RAIZ, devfn, 0)) };
        // Contra el literal y no contra `CFG_NO_DEVICE`: una constante
        // comparada consigo misma no comprueba cuál es su valor, y aquí el
        // valor es lo único que hace que un escaneo salte el hueco.
        assert_eq!(
            val, 0xffff_ffff,
            "devfn {:#x} del bus raíz contestó algo",
            devfn
        );
    }
    assert_eq!(
        unsafe { t.ops.read32(ecam_offset(RAIZ, 0, 0)) },
        0x1234_5678
    );
}

/// El bus raíz es el que se le dice, no el cero. Con el puente en el bus 1, el
/// bus 0 es «de detrás» y pasa por el índice.
#[test]
fn the_root_bus_is_the_one_it_was_told_and_not_bus_zero() {
    let t = trasto();
    let (_, traza) = bridge_fake::trazando(|| unsafe { t.ops.read32(ecam_offset(0, 0, 0)) });
    assert!(
        bridge_fake::primera(&traza, bridge_fake::PUENTE, EXT_CFG_INDEX).is_some(),
        "el bus 0 se trató como raíz aunque la raíz es {}: {:?}",
        RAIZ,
        traza
    );
}

// --- un dispositivo de detrás ----------------------------------------------

/// El orden: primero el índice con el `bus/devfn`, después la ventana de
/// datos. Al revés se leería el dispositivo que estuviese puesto de antes —
/// sin error, con un valor perfectamente creíble, que es lo que lo hace malo.
///
/// Se mira en la traza, que apunta las lecturas con fuente propia justo para
/// poder comparar un orden entre una escritura y una lectura.
#[test]
fn the_index_goes_down_before_the_window_is_read_and_not_after() {
    let t = trasto();
    poke(t.base, EXT_CFG_DATA + 0x10, 0xcafe_0000);
    let (val, traza) =
        bridge_fake::trazando(|| unsafe { t.ops.read32(ecam_offset(2, 0x18, 0x10)) });
    assert_eq!(val, 0xcafe_0000);
    let i = bridge_fake::primera(&traza, bridge_fake::PUENTE, EXT_CFG_INDEX)
        .expect("no se escribió el índice");
    let d = bridge_fake::primera(&traza, bridge_fake::LECTURA, EXT_CFG_DATA + 0x10)
        .expect("no se leyó por la ventana de datos");
    assert!(i < d, "el dato se leyó antes del índice: {:?}", traza);
    assert_eq!(reg(t.base, EXT_CFG_INDEX), ecam_offset(2, 0x18, 0));
}

/// En el índice va el `bus/devfn` **con el registro a cero**: el offset dentro
/// de los 4 KiB se aplica a la ventana de datos, no al índice. Es justo lo
/// contrario de lo que hace la variante del 7425, que mete el `where` en el
/// índice y lee el dato sin offset.
#[test]
fn the_index_carries_the_device_and_not_the_register() {
    let t = trasto();
    unsafe { t.ops.read32(ecam_offset(2, 0x18, 0x34)) };
    assert_eq!(
        reg(t.base, EXT_CFG_INDEX),
        ecam_offset(2, 0x18, 0),
        "el registro se colo en el indice"
    );
    assert_ne!(reg(t.base, EXT_CFG_INDEX), ecam_offset(2, 0x18, 0x34));
}

/// Dos dispositivos distintos tienen que dejar índices distintos. Si el índice
/// no cambiase, el segundo leería lo del primero — sin error y con un valor
/// creíble.
#[test]
fn two_different_devices_leave_two_different_indices() {
    let t = trasto();
    unsafe { t.ops.read32(ecam_offset(2, 0x00, 0)) };
    let primero = reg(t.base, EXT_CFG_INDEX);
    unsafe { t.ops.read32(ecam_offset(2, 0x08, 0)) };
    let segundo = reg(t.base, EXT_CFG_INDEX);
    assert_ne!(primero, segundo);
    assert_eq!(primero, ecam_offset(2, 0x00, 0));
    assert_eq!(segundo, ecam_offset(2, 0x08, 0));
}

// --- sin enlace ------------------------------------------------------------

/// Con el enlace caído un acceso a configuración **aborta la CPU**, no
/// devuelve unos. Así que no se intenta: se contesta el hueco sin tocar nada.
#[test]
fn with_the_link_down_nothing_is_touched_at_all() {
    let t = trasto_sin_enlace();
    let (val, traza) = bridge_fake::trazando(|| unsafe { t.ops.read32(ecam_offset(2, 0, 0)) });
    assert_eq!(val, 0xffff_ffff, "un hueco no se lee como todo unos");
    assert!(
        escrituras(&traza).is_empty(),
        "con el enlace caído se escribió algo: {:?}",
        traza
    );
}

/// Y el puente mismo sí se lee con el enlace caído: sus registros son suyos,
/// no hay nada al otro lado del enlace que haga falta. Es lo que permite
/// mirarle el estado para saber *por qué* no hay enlace.
#[test]
fn the_bridge_itself_is_still_readable_with_the_link_down() {
    let t = trasto_sin_enlace();
    poke(t.base, 0x08, 0x0006_0400);
    assert_eq!(
        unsafe { t.ops.read32(ecam_offset(RAIZ, 0, 0x08)) },
        0x0006_0400
    );
}

// --- escrituras ------------------------------------------------------------

/// Una escritura a un hueco se **tira**. El offset 0 de la ventana es el
/// `vendor id` del puente, así que una escritura «perdida» que acabase en la
/// ventana sin índice le estaría escribiendo al puente.
#[test]
fn a_write_to_a_hole_is_dropped_and_does_not_fall_through_onto_the_bridge() {
    let t = trasto_sin_enlace();
    poke(t.base, 0x00, 0x1234_5678);
    let (_, traza) =
        bridge_fake::trazando(|| unsafe { t.ops.write32(ecam_offset(2, 0, 0x00), 0xffff_ffff) });
    assert!(
        escrituras(&traza).is_empty(),
        "la escritura al hueco tocó algo: {:?}",
        traza
    );
    assert_eq!(
        reg(t.base, 0x00),
        0x1234_5678,
        "se le escribió al vendor id del puente"
    );
}

/// Lo mismo con una función que no existe del bus raíz, que es el otro camino
/// que devuelve hueco.
#[test]
fn a_write_to_a_nonexistent_root_bus_function_is_dropped_too() {
    let t = trasto();
    poke(t.base, 0x00, 0x1234_5678);
    unsafe { t.ops.write32(ecam_offset(RAIZ, 3, 0x00), 0xffff_ffff) };
    assert_eq!(reg(t.base, 0x00), 0x1234_5678);
}

/// Una escritura a un dispositivo de detrás también pasa por el índice.
#[test]
fn writing_to_a_device_behind_the_bridge_goes_through_the_index_as_well() {
    let t = trasto();
    unsafe { t.ops.write32(ecam_offset(3, 0x10, 0x04), 0x0000_0007) };
    assert_eq!(reg(t.base, EXT_CFG_INDEX), ecam_offset(3, 0x10, 0));
    assert_eq!(reg(t.base, EXT_CFG_DATA + 0x04), 0x0000_0007);
}

// --- menos de una palabra --------------------------------------------------

/// Una lectura de media palabra sale del trozo que le toca de la palabra, no
/// de la palabra entera.
#[test]
fn a_half_word_read_comes_out_of_its_own_half() {
    let t = trasto_raiz0();
    poke(t.base, 0x04, 0xbbbb_aaaa);
    let bajo = puerto16(0, 0x04);
    assert_eq!(unsafe { t.ops.read16(bajo) }, 0xaaaa, "la mitad de abajo");
    assert_eq!(unsafe { t.ops.read16(bajo + 2) }, 0xbbbb, "la de arriba");
}

/// Un acceso de media palabra a un offset **impar** no es legal en PCI, y lo
/// que hace este camino es lo mismo que el plano: se queda con la mitad
/// alineada, sin cruzar el límite. El bit 0 se ignora; si en vez de eso se
/// desplazara por los dos bits de abajo, un `read16(0x05)` devolvería dos
/// bytes a caballo de las dos mitades.
#[test]
fn a_misaligned_half_word_access_still_comes_out_of_its_aligned_half() {
    let t = trasto_raiz0();
    poke(t.base, 0x04, 0xbbbb_aaaa);
    let bajo = puerto16(0, 0x04);
    assert_eq!(unsafe { t.ops.read16(bajo + 1) }, 0xaaaa, "0x05");
    assert_eq!(unsafe { t.ops.read16(bajo + 3) }, 0xbbbb, "0x07");
}

/// Y una de un byte, de su byte.
#[test]
fn a_byte_read_comes_out_of_its_own_byte() {
    let t = trasto_raiz0();
    poke(t.base, 0x08, 0x4433_2211);
    let bajo = puerto16(0, 0x08);
    assert_eq!(unsafe { t.ops.read8(bajo) }, 0x11);
    assert_eq!(unsafe { t.ops.read8(bajo + 1) }, 0x22);
    assert_eq!(unsafe { t.ops.read8(bajo + 2) }, 0x33);
    assert_eq!(unsafe { t.ops.read8(bajo + 3) }, 0x44);
}

/// Una escritura de menos de una palabra es lectura-modificación-escritura, y
/// **deja el resto de la palabra como estaba**. Si no, escribir el registro de
/// comando de un byte borraría el de estado.
#[test]
fn a_byte_write_leaves_the_rest_of_the_word_alone() {
    let t = trasto_raiz0();
    poke(t.base, 0x04, 0xffff_ffff);
    let bajo = puerto16(0, 0x04);
    unsafe { t.ops.write8(bajo + 1, 0x00) };
    assert_eq!(reg(t.base, 0x04), 0xffff_00ff);
}

/// El par lectura-escritura de un acceso a medias **comparte el índice**: se
/// escribe una sola vez. Escribirlo dos veces no está mal por el valor, está
/// mal por lo que dice — que entre la lectura y la escritura alguien pudo
/// moverlo, y con el cerrojo cogido no pudo.
#[test]
fn the_read_modify_write_pair_writes_the_index_once_and_not_twice() {
    let t = trasto();
    // Bus 0, que con la raíz en 1 es «de detrás» y sí pasa por el índice.
    let bajo = puerto16(2, 0x04);
    let (_, traza) = bridge_fake::trazando(|| unsafe { t.ops.write8(bajo + 1, 0x5a) });
    let veces = escrituras(&traza)
        .iter()
        .filter(|(off, _)| *off == EXT_CFG_INDEX)
        .count();
    assert_eq!(
        veces, 1,
        "el índice se escribió {} veces: {:?}",
        veces, traza
    );
}

/// Y una escritura a medias a un hueco también se tira, sin leer nada.
#[test]
fn a_byte_write_to_a_hole_reads_nothing_and_writes_nothing() {
    let t = trasto_sin_enlace();
    let bajo = puerto16(0, 0x04);
    let (_, traza) = bridge_fake::trazando(|| unsafe { t.ops.write8(bajo, 0x5a) });
    assert!(
        escrituras(&traza).is_empty(),
        "la escritura a medias al hueco tocó algo: {:?}",
        traza
    );
}

/// Un número de bus **no cabe** en el `port` de 16 bits que piden `read8` y
/// `read16`: empieza en el bit 20. Esto no es un descuido de aquí sino del
/// trait, y no se nota porque `CSpaceAccessMethod` nunca los llama — compone
/// los accesos de menos de una palabra a partir de `read32`, que sí recibe 32
/// bits. Quien los llame a mano por encima del bus 0 estará leyendo otro
/// dispositivo, y este test está para que eso se vea escrito.
#[test]
fn a_bus_number_does_not_fit_in_a_sixteen_bit_port() {
    let port = ecam_offset(2, 0x18, 0x04);
    assert!(port > u32::from(u16::MAX), "{:#x}", port);
    assert_eq!(decode_port(u32::from(port as u16)).0, 0, "el bus se perdió");
    // Lo que sí cabe: bus 0 y un devfn de cuatro bits.
    assert_eq!(
        decode_port(u32::from(puerto16(0xf, 0xffc))),
        (0, 0xf, 0xffc)
    );
}

// --- la ventana del nodo ---------------------------------------------------

// Los dos registros que hacen falta caben en los `0x9310` del nodo, y el
// `0x9004` que la tabla de offsets del 2712 declara como `EXT_CFG_DATA`
// también — por eso esa entrada equivocada nunca ha reventado nada: nadie la
// lee en este chip. Van como asertos de compilación, no como test, porque
// comparan constantes: un `assert!` en tiempo de ejecución sobre constantes es
// un error de clippy, y además esto tiene que fallar al compilar, no al
// correr. Los mismos asertos están en el módulo.
const _: () = assert!(EXT_CFG_DATA + (ECAM_REG_MASK as usize) < WIN_LEN_2712);
const _: () = assert!(EXT_CFG_INDEX + 4 <= WIN_LEN_2712);
const _: () = assert!(0x9004 + 4 <= WIN_LEN_2712, "la entrada que nadie lee");

/// Y el camino plano sigue siendo otra cosa: este `PortOps` no es
/// `PortOpsImpl`, y nada de aquí lo toca.
#[test]
fn this_is_a_second_implementation_and_not_a_branch_inside_the_flat_one() {
    fn es_portops<T: PortOps>() {}
    es_portops::<BrcmCfgOps>();
    es_portops::<crate::bus::pci::PortOpsImpl>();
}
