//! Lo único que se puede comprobar aquí del puente PCIe del BCM2712: qué
//! registro se toca, con qué bits y en qué orden. Aquí no hay Pi 5, y QEMU no
//! emula este controlador, así que el banco de registros es de mentira — una
//! tira de palabras que se queda con lo que se le escribe.

use super::*;
use alloc::boxed::Box;

/// La ventana del nodo son `0x9310` bytes.
const VENTANA: usize = 0x9310 / 4;

/// Un puente sobre un banco de registros falso. Devuelve también la base, para
/// poder mirar las palabras una a una.
fn puente() -> (BrcmPcie, usize) {
    let banco: &'static mut [u32] = Box::leak(alloc::vec![0u32; VENTANA].into_boxed_slice());
    let base = banco.as_mut_ptr() as usize;
    (unsafe { BrcmPcie::new(base) }, base)
}

fn reg(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

fn poke(base: usize, off: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}

// --- el tamaño de una ventana de entrada -----------------------------------

/// Los dos extremos del tramo de abajo. `0x1c` no es un tamaño: es el código
/// con el que empieza el tramo de los kilobytes.
#[test]
fn the_small_window_sizes_use_the_high_codes() {
    assert_eq!(encode_ibar_size(4 * 1024), 0x1c, "4 KiB");
    assert_eq!(encode_ibar_size(8 * 1024), 0x1d);
    assert_eq!(encode_ibar_size(16 * 1024), 0x1e);
    assert_eq!(encode_ibar_size(32 * 1024), 0x1f, "32 KiB");
}

/// Y los del tramo de arriba, que empieza en 1 y no en 0 — el 0 está reservado
/// para decir «apagada».
#[test]
fn the_large_window_sizes_count_from_one() {
    assert_eq!(encode_ibar_size(64 * 1024), 1, "64 KiB");
    assert_eq!(encode_ibar_size(1 << 30), 15, "1 GiB");
    assert_eq!(encode_ibar_size(4u64 << 30), 17, "4 GiB");
    assert_eq!(encode_ibar_size(64u64 << 30), 21, "64 GiB");
}

/// Fuera de los dos tramos la respuesta es cero, y cero **no es un tamaño**:
/// es la ventana apagada. Un driver que tratase el cero como «lo más pequeño
/// que hay» dejaría la memoria del sistema invisible para el bus.
#[test]
fn a_size_outside_both_ranges_disables_the_window_rather_than_shrinking_it() {
    assert_eq!(encode_ibar_size(0), 0, "sin tamaño");
    assert_eq!(encode_ibar_size(2 * 1024), 0, "2 KiB, por debajo del tramo");
    assert_eq!(encode_ibar_size(1), 0);
    assert_eq!(encode_ibar_size(128u64 << 30), 0, "128 GiB, por encima");
}

/// Entre los dos tramos no hay agujero: 32 KiB es el último del de abajo y
/// 64 KiB el primero del de arriba, y entre ambos no cabe ninguna potencia de
/// dos. Lo que sí hay es un salto de código, de `0x1f` a `1`.
#[test]
fn the_two_ranges_meet_without_a_gap_in_sizes_but_with_one_in_codes() {
    assert_eq!(encode_ibar_size(32 * 1024), 0x1f);
    assert_eq!(encode_ibar_size(64 * 1024), 1);
}

/// Un tamaño que no sea potencia de dos se trunca **hacia abajo**, como el
/// `ilog2` de Linux. La ventana no puede valer otra cosa, y redondear hacia
/// arriba abriría una ventana mayor que la memoria que la respalda.
#[test]
fn a_size_that_is_not_a_power_of_two_is_truncated_downwards() {
    assert_eq!(
        encode_ibar_size((1 << 30) + 1),
        15,
        "1 GiB + 1 byte sigue siendo el código de 1 GiB"
    );
    assert_eq!(
        encode_ibar_size((2u64 << 30) - 1),
        15,
        "2 GiB - 1 byte se queda en 1 GiB, no sube a 2"
    );
}

// --- el estado del enlace ---------------------------------------------------

/// Hacen falta los dos bits. Con uno solo el enlace no está, y un acceso a
/// configuración basado en esa respuesta aborta la CPU.
#[test]
fn the_link_is_up_only_when_both_the_data_link_and_the_phy_say_so() {
    let (pcie, base) = puente();

    assert!(!pcie.link_up(), "un registro a cero no es un enlace arriba");

    poke(base, MISC_PCIE_STATUS, PCIE_STATUS_DL_ACTIVE);
    assert!(!pcie.link_up(), "sólo el nivel de enlace");

    poke(base, MISC_PCIE_STATUS, PCIE_STATUS_PHYLINKUP);
    assert!(!pcie.link_up(), "sólo la capa física");

    poke(
        base,
        MISC_PCIE_STATUS,
        PCIE_STATUS_DL_ACTIVE | PCIE_STATUS_PHYLINKUP,
    );
    assert!(pcie.link_up());
}

/// El bit de «modo RC» del 2712 es el mismo que `PCIE_LINK_IN_L23`, así que no
/// dice nada del papel del núcleo. Esta es la prueba de que nada se condiciona
/// a él: con el bit a cero la respuesta sigue siendo que sí.
#[test]
fn the_root_complex_check_is_dead_on_this_chip_and_always_answers_yes() {
    let (pcie, base) = puente();

    assert_eq!(
        PCIE_STATUS_PORT_2712, PCIE_STATUS_LINK_IN_L23,
        "si estos dos dejan de ser el mismo bit, la comprobación vuelve a tener sentido"
    );

    poke(base, MISC_PCIE_STATUS, 0);
    assert!(pcie.rc_mode(), "con el bit a cero");

    poke(base, MISC_PCIE_STATUS, PCIE_STATUS_PORT_2712);
    assert!(pcie.rc_mode(), "y con el bit a uno");
}

// --- PERST# -----------------------------------------------------------------

/// `PERSTB` lleva barra: a uno el reset está **suelto**. Apretar el reset
/// escribe un cero, y soltarlo un uno. Al revés deja la tarjeta en reset justo
/// cuando se la quería arrancar.
#[test]
fn asserting_perst_writes_a_zero_because_the_bit_is_inverted() {
    let (pcie, base) = puente();

    pcie.perst_set(false);
    assert_eq!(
        reg(base, MISC_PCIE_CTRL) & PCIE_CTRL_PERSTB,
        PCIE_CTRL_PERSTB,
        "soltar el reset pone el bit"
    );

    pcie.perst_set(true);
    assert_eq!(
        reg(base, MISC_PCIE_CTRL) & PCIE_CTRL_PERSTB,
        0,
        "apretarlo lo limpia"
    );
}

/// Y no se lleva por delante lo que haya en el resto del registro, que es
/// donde vive la petición de L23.
#[test]
fn touching_perst_leaves_the_rest_of_its_register_alone() {
    let (pcie, base) = puente();
    poke(base, MISC_PCIE_CTRL, PCIE_CTRL_L23_REQUEST | 0x1234_0000);

    pcie.perst_set(false);

    let val = reg(base, MISC_PCIE_CTRL);
    assert_eq!(val & PCIE_CTRL_L23_REQUEST, PCIE_CTRL_L23_REQUEST);
    assert_eq!(val & 0x1234_0000, 0x1234_0000);
}

// --- las ventanas de salida -------------------------------------------------

/// La base del lado de PCIe se escribe **entera**, en dos mitades de 32 bits,
/// sin pasar por MiB ni por máscaras: es la dirección tal cual.
#[test]
fn the_pcie_side_of_a_window_is_written_whole_in_two_halves() {
    let (pcie, base) = puente();

    assert!(pcie.set_outbound_win(0, 0x1f_0000_0000, 0x4_8765_0000, 1 << 20));

    assert_eq!(reg(base, mem_win_lo(0)), 0x8765_0000, "la mitad baja");
    assert_eq!(reg(base, mem_win_hi(0)), 0x4, "la mitad alta");
}

/// El lado de la CPU va en **MiB**, y el límite es la última dirección
/// **dentro** de la ventana. Una ventana de 16 MiB desde 0x1000_0000 va del MiB
/// 256 al 271, no al 272.
#[test]
fn the_cpu_side_goes_in_megabytes_and_the_limit_is_the_last_one_inside() {
    let (pcie, base) = puente();

    assert!(pcie.set_outbound_win(0, 0x1000_0000, 0, 16 << 20));

    let val = reg(base, mem_win_base_limit(0));
    assert_eq!(
        (val & MEM_WIN_BASE) >> MEM_WIN_BASE.trailing_zeros(),
        256,
        "0x1000_0000 es el MiB 256"
    );
    assert_eq!(
        (val & MEM_WIN_LIMIT) >> MEM_WIN_LIMIT.trailing_zeros(),
        271,
        "16 MiB desde el 256 acaban en el 271, no en el 272"
    );
}

/// Una ventana más pequeña que un MiB acaba con base y límite iguales, y eso es
/// lo correcto: el registro no sabe hablar de menos de un MiB, así que cubre
/// ese MiB entero. Un límite calculado como `cpu + size` en vez de
/// `cpu + size - 1` abriría un MiB de más.
#[test]
fn a_window_smaller_than_a_megabyte_has_the_same_base_and_limit() {
    let (pcie, base) = puente();

    assert!(pcie.set_outbound_win(1, 0x4000_0000, 0, 0x1000));

    let val = reg(base, mem_win_base_limit(1));
    let b = (val & MEM_WIN_BASE) >> MEM_WIN_BASE.trailing_zeros();
    let l = (val & MEM_WIN_LIMIT) >> MEM_WIN_LIMIT.trailing_zeros();
    assert_eq!(b, 1024, "0x4000_0000 es el MiB 1024");
    assert_eq!(l, b, "la ventana cabe en un solo MiB");
}

/// La parte alta se desplaza doce bits, que es el número de bits puestos en la
/// máscara de la baja — ni los 4 donde empieza el campo ni los 16 donde acaba.
/// Y lo que se desplaza son los MiB, no la dirección: con los 20 bits del MiB
/// ya quitados, un desplazamiento de 20 dejaría la parte alta a cero y la
/// ventana caería dentro de los primeros 4 GiB.
#[test]
fn the_high_half_of_a_window_is_the_megabyte_count_shifted_by_twelve() {
    let (pcie, base) = puente();

    assert_eq!(MEM_WIN_HI_SHIFT, 12, "popcount(0xfff0)");

    // La ventana del RP1 en la Pi 5: 0x1f_0000_0000, 16 MiB.
    assert!(pcie.set_outbound_win(0, 0x1f_0000_0000, 0, 16 << 20));

    let mib = 0x1f_0000_0000u64 >> 20; // 0x1f000
    assert_eq!(
        reg(base, mem_win_base_hi(0)) & MEM_WIN_HI_ADDR,
        (mib >> 12) as u32,
        "0x1f, la parte alta de la base"
    );
    assert_eq!(
        reg(base, mem_win_base_hi(0)) & MEM_WIN_HI_ADDR,
        0x1f,
        "y ese 0x1f es el que pone la ventana por encima de los 4 GiB"
    );
    let limite_mib = (0x1f_0000_0000u64 + (16 << 20) - 1) >> 20;
    assert_eq!(
        reg(base, mem_win_limit_hi(0)) & MEM_WIN_HI_ADDR,
        (limite_mib >> 12) as u32
    );
}

/// Los registros de las cuatro ventanas se intercalan de dos maneras
/// distintas: `BASE_LIMIT` va de cuatro en cuatro y los dos `_HI` de ocho en
/// ocho desde direcciones contiguas. Abrir una ventana no puede tocar los
/// registros de otra.
#[test]
fn opening_one_window_does_not_touch_the_registers_of_another() {
    let (pcie, base) = puente();

    assert!(pcie.set_outbound_win(2, 0x8000_0000, 0x1_0000_0000, 32 << 20));

    for win in 0..NUM_OUT_WINS {
        if win == 2 {
            continue;
        }
        assert_eq!(reg(base, mem_win_lo(win)), 0, "lo de la ventana {}", win);
        assert_eq!(reg(base, mem_win_hi(win)), 0, "hi de la ventana {}", win);
        assert_eq!(
            reg(base, mem_win_base_limit(win)),
            0,
            "base/limit de la ventana {}",
            win
        );
        assert_eq!(
            reg(base, mem_win_base_hi(win)) & MEM_WIN_HI_ADDR,
            0,
            "base_hi de la ventana {}",
            win
        );
        assert_eq!(
            reg(base, mem_win_limit_hi(win)) & MEM_WIN_HI_ADDR,
            0,
            "limit_hi de la ventana {}",
            win
        );
    }
    assert_ne!(reg(base, mem_win_base_limit(2)), 0, "la 2 sí se escribió");
}

/// `mem_win_base_hi` y `mem_win_limit_hi` comparten el paso de ocho bytes
/// desde `0x4080` y `0x4084`: el límite de una ventana cae donde la base de la
/// siguiente **no** está. Si se calculasen con el mismo paso que `BASE_LIMIT`,
/// la ventana 1 pisaría el límite de la 0.
#[test]
fn the_window_register_offsets_interleave_the_way_the_chip_lays_them_out() {
    assert_eq!(mem_win_lo(0), 0x400c);
    assert_eq!(mem_win_hi(0), 0x4010);
    assert_eq!(mem_win_lo(1), 0x4014, "lo y hi se alternan");

    assert_eq!(mem_win_base_limit(0), 0x4070);
    assert_eq!(mem_win_base_limit(1), 0x4074, "de cuatro en cuatro");

    assert_eq!(mem_win_base_hi(0), 0x4080);
    assert_eq!(mem_win_limit_hi(0), 0x4084);
    assert_eq!(mem_win_base_hi(1), 0x4088, "de ocho en ocho");
    assert_eq!(mem_win_limit_hi(1), 0x408c);
}

/// Sólo hay cuatro ventanas. La quinta no está en el chip, y escribirla caería
/// encima de `PCIE_MISC_HARD_PCIE_HARD_DEBUG` y compañía.
#[test]
fn a_window_the_chip_does_not_have_is_refused_rather_than_written() {
    let (pcie, base) = puente();

    assert!(!pcie.set_outbound_win(NUM_OUT_WINS, 0x8000_0000, 0, 1 << 20));

    for off in (0x4000..0x4100).step_by(4) {
        assert_eq!(reg(base, off), 0, "se escribió en {:#x}", off);
    }
}

/// Y una ventana vacía tampoco se abre: con `size == 0` el límite saldría
/// `cpu - 1`, o sea por debajo de la base, que es una ventana al revés.
#[test]
fn an_empty_window_is_refused_rather_than_turned_inside_out() {
    let (pcie, base) = puente();

    assert!(!pcie.set_outbound_win(0, 0x4000_0000, 0, 0));

    assert_eq!(reg(base, mem_win_base_limit(0)), 0);
    assert_eq!(reg(base, mem_win_lo(0)), 0);
}

// --- el espacio de configuración --------------------------------------------

/// El puente mismo se lee directo, por la ventana de registros, sin índice.
#[test]
fn the_bridge_itself_is_read_straight_out_of_its_register_window() {
    let (pcie, _) = puente();

    assert_eq!(
        pcie.cfg_access(0, 0, 0x10, 0),
        Some(CfgAccess::Direct { off: 0x10 })
    );
}

/// En el bus raíz el puente es la única función. Las demás no existen, y
/// pedirlas no es un acceso sin enlace: es un acceso a nada.
#[test]
fn the_root_bus_has_no_function_but_the_bridge() {
    let (pcie, _) = puente();

    assert_eq!(pcie.cfg_access(0, 1, 0, 0), None, "devfn 1 del bus raíz");
    assert_eq!(pcie.cfg_access(0, 8, 0, 0), None, "ni el primer slot");
}

/// Un dispositivo de detrás necesita el enlace arriba. Sin enlace el acceso
/// **aborta la CPU**: no devuelve unos, no falla suave. Por eso esto se
/// comprueba antes y no se intenta «por ver».
#[test]
fn a_device_behind_the_bridge_cannot_be_reached_without_a_link() {
    let (pcie, base) = puente();

    assert_eq!(
        pcie.cfg_access(1, 0, 0, 0),
        None,
        "sin enlace no se toca configuración"
    );

    poke(
        base,
        MISC_PCIE_STATUS,
        PCIE_STATUS_DL_ACTIVE | PCIE_STATUS_PHYLINKUP,
    );
    assert!(pcie.cfg_access(1, 0, 0, 0).is_some(), "con enlace sí");
}

/// El índice lleva bus y devfn pero **no** el offset: el `where` del índice es
/// cero y el offset se aplica a la ventana de datos. Metiéndolo en los dos
/// sitios se leería un registro a `2 * offset`.
#[test]
fn the_index_carries_the_bus_and_function_but_not_the_register_offset() {
    let (pcie, base) = puente();
    poke(
        base,
        MISC_PCIE_STATUS,
        PCIE_STATUS_DL_ACTIVE | PCIE_STATUS_PHYLINKUP,
    );

    let acceso = pcie.cfg_access(1, 0, 0x10, 0).expect("con enlace");

    assert_eq!(
        acceso,
        CfgAccess::Indexed {
            index: 1 << ECAM_BUS_SHIFT,
            off: EXT_CFG_DATA + 0x10,
        }
    );
}

/// Y el offset se recorta a los 4 KiB del espacio de configuración, los dos
/// campos del índice van en sus sitios.
#[test]
fn the_ecam_index_puts_each_field_where_the_chip_expects_it() {
    assert_eq!(ecam_offset(1, 0, 0), 0x0010_0000, "el bus a 20 bits");
    assert_eq!(ecam_offset(0, 8, 0), 0x0000_8000, "el devfn a 12");
    assert_eq!(ecam_offset(0, 0, 0x3f), 0x3f, "el registro, abajo");
    assert_eq!(
        ecam_offset(0, 0, 0x1234),
        0x234,
        "un offset de más de 4 KiB se recorta"
    );
    assert_eq!(ecam_offset(2, 9, 0x40), 0x0020_9040, "los tres a la vez");
}

// --- el mapa de registros ---------------------------------------------------

/// La trampa de la tabla de offsets: el 2712 declara
/// `[EXT_CFG_DATA] = 0x9004` y aun así la ventana de datos que usa es
/// `0x8000`, porque su ruta (`brcm_pcie_map_bus`) escribe los literales y no
/// mira la tabla. Quien copie la tabla lee por la ventana equivocada.
#[test]
fn the_config_data_window_is_the_literal_and_not_the_entry_in_the_offsets_table() {
    assert_eq!(EXT_CFG_DATA, 0x8000);
    assert_ne!(
        EXT_CFG_DATA, 0x9004,
        "0x9004 es lo que dice pcie_offsets_bcm2712, y en este chip no se usa"
    );
    assert_eq!(EXT_CFG_INDEX, 0x9000);
}

/// El `PCIE_HARD_DEBUG` del 2712 está en `0x4304`, fuera del bloque `0x40xx`
/// donde caen los demás `MISC`. Con el offset genérico se apagaría el SerDes
/// escribiendo en otra cosa.
#[test]
fn the_hard_debug_register_of_this_chip_sits_outside_the_misc_block() {
    assert_eq!(MISC_HARD_DEBUG_2712, 0x4304);
    const _: () = assert!(
        MISC_HARD_DEBUG_2712 > MISC_AXI_READ_ERROR_DATA,
        "está por encima de todo el bloque MISC"
    );
}

/// Todos los offsets del mapa tienen que caber en la ventana que el nodo
/// declara (`0x9310` bytes) y estar alineados a palabra.
#[test]
fn every_register_in_the_map_fits_inside_the_window_the_node_declares() {
    let todos = alloc::vec![
        CAP_REGS,
        RC_CFG_VENDOR_SPECIFIC_REG1,
        RC_CFG_PRIV1_ID_VAL3,
        RC_CFG_PRIV1_LINK_CAPABILITY,
        RC_PL_PHY_CTL_15,
        MISC_MISC_CTRL,
        MISC_RC_BAR1_CONFIG_LO,
        MISC_RC_BAR2_CONFIG_LO,
        MISC_RC_BAR2_CONFIG_HI,
        MISC_RC_BAR3_CONFIG_LO,
        MISC_RC_CONFIG_RETRY_TIMEOUT,
        MISC_PCIE_CTRL,
        MISC_PCIE_STATUS,
        MISC_REVISION,
        MISC_HARD_DEBUG_2712,
        MISC_CTRL_1,
        MISC_UBUS_CTRL,
        MISC_UBUS_TIMEOUT,
        MISC_UBUS_BAR2_CONFIG_REMAP,
        MISC_AXI_INTF_CTRL,
        MISC_AXI_READ_ERROR_DATA,
        EXT_CFG_DATA,
        EXT_CFG_INDEX,
        mem_win_lo(NUM_OUT_WINS - 1),
        mem_win_hi(NUM_OUT_WINS - 1),
        mem_win_base_limit(NUM_OUT_WINS - 1),
        mem_win_base_hi(NUM_OUT_WINS - 1),
        mem_win_limit_hi(NUM_OUT_WINS - 1),
    ];

    for off in todos {
        assert!(off + 4 <= 0x9310, "{:#x} se sale de la ventana", off);
        assert!(off.is_multiple_of(4), "{:#x} no está alineado", off);
    }
}

/// El tamaño de ráfaga del 2712 es `1`, que en **su** codificación son 128
/// bytes. El mismo `1` en la codificación genérica son 256, y el mismo 128 en
/// la genérica se pide con un `0`. Ni el número ni el significado viajan entre
/// variantes.
#[test]
fn the_burst_size_of_this_chip_is_a_code_and_not_a_number_of_bytes() {
    assert_eq!(BURST_SIZE_2712, 1);

    let (pcie, base) = puente();
    pcie.modify(MISC_MISC_CTRL, MISC_CTRL_MAX_BURST_SIZE, BURST_SIZE_2712);

    assert_eq!(
        reg(base, MISC_MISC_CTRL),
        1 << MISC_CTRL_MAX_BURST_SIZE.trailing_zeros(),
        "el código va desplazado al campo, no escrito en crudo"
    );
}

/// El campo de tamaño de un `RC_BARn_CONFIG_LO` a cero **apaga** la ventana, y
/// es así como se cierran las de GISB y SCB. Compartir la máscara entre los
/// tres BAR es a propósito.
#[test]
fn clearing_the_size_field_is_how_an_inbound_window_is_turned_off() {
    let (pcie, base) = puente();
    poke(
        base,
        MISC_RC_BAR1_CONFIG_LO,
        0xdead_0000 | RC_BAR_CONFIG_LO_SIZE,
    );

    pcie.modify(MISC_RC_BAR1_CONFIG_LO, RC_BAR_CONFIG_LO_SIZE, 0);

    let val = reg(base, MISC_RC_BAR1_CONFIG_LO);
    assert_eq!(val & RC_BAR_CONFIG_LO_SIZE, 0, "la ventana queda apagada");
    assert_eq!(val & 0xdead_0000, 0xdead_0000, "y la dirección intacta");
}

/// `modify` toca sólo su campo. Es el cimiento de todo lo de arriba: media
/// docena de campos de este chip viven en registros compartidos con bits que
/// el firmware ya dejó puestos.
#[test]
fn modifying_a_field_leaves_every_other_bit_of_the_register_alone() {
    let (pcie, base) = puente();
    poke(base, MISC_MISC_CTRL, 0xffff_ffff);

    pcie.modify(MISC_MISC_CTRL, MISC_CTRL_MAX_BURST_SIZE, 0);

    assert_eq!(reg(base, MISC_MISC_CTRL), !MISC_CTRL_MAX_BURST_SIZE);
}
