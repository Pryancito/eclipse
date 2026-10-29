//! Lo único comprobable de una secuencia de arranque sin la placa: el **orden**
//! en que se tocan los registros y con qué valores. El chip no se queja cuando
//! el orden está mal —— el enlace no sube, o sube y las lecturas de
//! configuración abortan la CPU —— así que el orden es la parte que hay que
//! pinchar.

use super::super::bridge_fake::{self, primera, PUENTE, RESCAL, RESET};
use super::super::mdio::MdioBus;
use super::super::reset::{self, BrcmstbReset, PcieRescal, RESET_BRIDGE};
use super::*;
use alloc::boxed::Box;

/// La ventana del puente son `0x9310` bytes; la del controlador de resets,
/// `0x30`; la del recalibrado, `0x10`.
struct Trasto {
    pcie: BrcmPcie,
    /// La ventana del puente, que es tambien la del MDIO.
    base: usize,
    rst: BrcmstbReset,
    rescal: PcieRescal,
    /// La ventana del recalibrado, que es **otra**: su `reg` es
    /// `0x10_00119500`, no un offset dentro del puente.
    rescal_base: usize,
    mdio: MdioBus,
}

fn trasto() -> Trasto {
    let puente: &'static mut [u32] = Box::leak(alloc::vec![0u32; 0x9310 / 4].into_boxed_slice());
    let base = puente.as_mut_ptr() as usize;
    let rst: &'static mut [u32] = Box::leak(alloc::vec![0u32; 0x30 / 4].into_boxed_slice());
    let res: &'static mut [u32] = Box::leak(alloc::vec![0u32; 0x10 / 4].into_boxed_slice());
    let rescal_base = res.as_mut_ptr() as usize;
    Trasto {
        pcie: unsafe { BrcmPcie::new(base) },
        base,
        rst: unsafe { BrcmstbReset::new(rst.as_mut_ptr() as usize, 0x30) },
        rescal: unsafe { PcieRescal::new(rescal_base) },
        rescal_base,
        mdio: unsafe { MdioBus::new(base) },
    }
}

fn reg(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

fn poke(base: usize, off: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}

/// Los registros de dentro de la capacidad PCIe son de 16 bits y **no están
/// todos alineados a palabra**: el `LNKSTA` cae en `0xbe` y el `RTCAP` en
/// `0xca`. Tocarlos de 32 no es sólo pisar el de al lado: es un acceso
/// desalineado.
fn reg16(base: usize, off: usize) -> u16 {
    unsafe { core::ptr::read_volatile((base + off) as *const u16) }
}

fn poke16(base: usize, off: usize, val: u16) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u16, val) }
}

/// Un puente al que todo le sale bien: el recalibrado acaba, el enlace sube y
/// la capacidad PCIe contesta Gen2 por una línea.
fn todo_bien(base: usize) {
    poke(
        base,
        MISC_PCIE_STATUS,
        PCIE_STATUS_DL_ACTIVE | PCIE_STATUS_PHYLINKUP,
    );
    // CLS = 2, NLW = 1, en el `LNKSTA` de 16 bits.
    poke16(base, CAP_REGS + CAP_LNKSTA, 2 | (1 << 4));
}

fn cfg_pi5() -> BridgeConfig {
    BridgeConfig {
        // El `dma-ranges` del nodo `pcie2` de la Pi 5. La dirección y el
        // tamaño se dan distintos a propósito: con los dos iguales, un código
        // de tamaño calculado sobre la dirección da el mismo número y el error
        // no se ve.
        inbound_pci: 0x10_0000_0000,
        inbound_size: 0x4_0000_0000,
        outbound: [
            Some(OutboundWin {
                cpu: 0x1f_0000_0000,
                pcie: 0,
                size: 0x1_0000_0000,
            }),
            None,
            None,
            None,
        ],
        gen: Some(2),
        ssc: false,
    }
}

// --- el orden grande --------------------------------------------------------

/// El recalibrado del PHY pasa **antes** de tocar un solo registro del puente.
/// En Linux es el `reset_control_deassert(rescal)` del `probe`, y está ahí y no
/// dentro de `setup` por algo: el PHY se calibra con el puente todavía quieto.
#[test]
fn the_phy_is_recalibrated_before_a_single_bridge_register_is_touched() {
    let t = trasto();
    let (pcie, base, rst, rescal, mdio) = (&t.pcie, t.base, &t.rst, &t.rescal, &t.mdio);
    todo_bien(base);
    poke(
        t.rescal_base,
        reset::RESCAL_STATUS,
        reset::RESCAL_STATUS_BIT,
    );

    let (r, traza) = bridge_fake::trazando(|| pcie.bring_up(rst, rescal, mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    let i_rescal = primera(&traza, RESCAL, reset::RESCAL_START).expect("el recalibrado");
    let i_puente = traza
        .iter()
        .position(|(f, _, _)| *f == PUENTE)
        .expect("no se escribió nada en el puente");
    let i_reset = traza
        .iter()
        .position(|(f, _, _)| *f == RESET)
        .expect("no se tocó el reset del puente");
    assert!(
        i_rescal < i_reset && i_rescal < i_puente,
        "el PHY se calibró después de empezar con el puente, traza {:x?}",
        traza
    );
}

/// Si el recalibrado no sale, no se toca el puente. Levantar un puente cuyo PHY
/// no está calibrado es gastar 200 ms para que el enlace no suba.
#[test]
fn a_phy_that_does_not_recalibrate_stops_the_bring_up_before_it_starts() {
    let t = trasto();
    let (pcie, base, rst, rescal, mdio) = (&t.pcie, t.base, &t.rst, &t.rescal, &t.mdio);
    poke(
        base,
        MISC_PCIE_STATUS,
        PCIE_STATUS_DL_ACTIVE | PCIE_STATUS_PHYLINKUP,
    );
    // `RESCAL_STATUS` se queda a cero: el recalibrado no acaba nunca.

    let (r, traza) = bridge_fake::trazando(|| pcie.bring_up(rst, rescal, mdio, &cfg_pi5()));

    assert_eq!(
        r,
        Err(BringUpError::Rescal(reset::RescalError::SeAgotoElPlazo))
    );
    assert!(
        !traza.iter().any(|(f, _, _)| *f == PUENTE || *f == RESET),
        "se tocó el puente igualmente, traza {:x?}",
        traza
    );
}

/// **El puente tiene que estar fuera de reset antes de tocar el registro del
/// SerDes.** Lo dice el propio Linux en su camino de reanudación, y es la razón
/// de que el reset se suelte *antes* de encender el SerDes.
#[test]
fn the_bridge_comes_out_of_reset_before_the_serdes_register_is_touched() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    let (r, traza) = bridge_fake::trazando(|| pcie.setup(rst, mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    // El reset se suelta escribiendo en el `CLEAR` de su banco; apretarlo, en
    // el `SET`. Los dos tienen que estar antes de tocar el SerDes.
    let banco = reset::sw_init_bank(RESET_BRIDGE) * reset::SW_INIT_BANK_SIZE;
    let i_apretar = primera(&traza, RESET, banco + reset::SW_INIT_SET).expect("apretar");
    let i_soltar = primera(&traza, RESET, banco + reset::SW_INIT_CLEAR).expect("soltar");
    let i_serdes = primera(&traza, PUENTE, MISC_HARD_DEBUG_2712).expect("el SerDes");

    assert!(
        i_apretar < i_soltar,
        "el reset se soltó antes de apretarlo, traza {:x?}",
        traza
    );
    assert!(
        i_soltar < i_serdes,
        "se tocó el registro del SerDes con el puente todavía en reset, \
         traza {:x?}",
        traza
    );
    assert_eq!(
        rst.line_is_asserted(RESET_BRIDGE),
        Some(false),
        "el puente se quedó en reset"
    );
}

/// El SerDes se enciende **limpiando** su bit de reposo, no poniéndolo, y sin
/// llevarse por delante el resto del registro —— que es donde viven los bits
/// del `clkreq`.
#[test]
fn the_serdes_is_powered_up_by_clearing_its_idle_bit_not_by_setting_it() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);
    poke(
        base,
        MISC_HARD_DEBUG_2712,
        HARD_DEBUG_SERDES_IDDQ | HARD_DEBUG_CLKREQ_L1SS_ENABLE,
    );

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    let val = reg(base, MISC_HARD_DEBUG_2712);
    assert_eq!(val & HARD_DEBUG_SERDES_IDDQ, 0, "el SerDes sigue en reposo");
    assert_eq!(
        val & HARD_DEBUG_CLKREQ_L1SS_ENABLE,
        HARD_DEBUG_CLKREQ_L1SS_ENABLE,
        "se borró el bit del clkreq de al lado"
    );
}

/// El PLL se reprograma **con el SerDes ya encendido**: hacerlo antes no deja
/// nada puesto, porque el bloque está en reposo.
#[test]
fn the_pll_is_reprogrammed_only_once_the_serdes_is_awake() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    let (r, traza) = bridge_fake::trazando(|| pcie.setup(rst, mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    let i_serdes = primera(&traza, PUENTE, MISC_HARD_DEBUG_2712).expect("el SerDes");
    // El MDIO se ve en la traza por su registro de dirección, que es de la
    // ventana del puente; `SET_ADDR_OFFSET` es un número de registro del PHY,
    // no un offset de MMIO, y no sale en una traza de escrituras.
    let i_pll = primera(&traza, PUENTE, RC_DL_MDIO_ADDR).expect("no se tocó el MDIO del PHY");
    assert!(
        i_serdes < i_pll,
        "el PLL se reprogramó con el SerDes en reposo, traza {:x?}",
        traza
    );
}

/// Y el periodo del reloj de gestión de energía (la errata de L1SS) se pone
/// después del PLL, no antes, y sin tocar el resto de su registro.
#[test]
fn the_power_management_clock_period_is_set_after_the_pll() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);
    poke(base, RC_PL_PHY_CTL_15, 0xdead_0000 | 0xff);

    let (r, traza) = bridge_fake::trazando(|| pcie.setup(rst, mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    let i_pll = primera(&traza, PUENTE, RC_DL_MDIO_ADDR).expect("el PLL");
    let i_pm = primera(&traza, PUENTE, RC_PL_PHY_CTL_15).expect("el periodo del reloj");
    assert!(i_pll < i_pm, "traza {:x?}", traza);

    let val = reg(base, RC_PL_PHY_CTL_15);
    assert_eq!(val & PM_CLK_PERIOD, PM_CLK_PERIOD_2712, "los 18,52 ns");
    assert_eq!(val & 0xdead_0000, 0xdead_0000, "se tocó el resto");
}

// --- los valores que importan -----------------------------------------------

/// Que una lectura fallida vuelva con **todo unos** y que el UBUS **no**
/// responda con error son dos cosas que van juntas: sin el dato de relleno, la
/// lectura queda colgando; sin suprimir el error, un barrido de buses se lleva
/// la máquina por delante en el primer hueco.
#[test]
fn a_failed_read_is_made_to_return_all_ones_instead_of_aborting_the_cpu() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    let ubus = reg(base, MISC_UBUS_CTRL);
    assert_eq!(ubus & UBUS_CTRL_REPLY_ERR_DIS, UBUS_CTRL_REPLY_ERR_DIS);
    assert_eq!(
        ubus & UBUS_CTRL_REPLY_DECERR_DIS,
        UBUS_CTRL_REPLY_DECERR_DIS
    );
    assert_eq!(
        reg(base, MISC_AXI_READ_ERROR_DATA),
        0xffff_ffff,
        "el relleno tiene que ser TODO unos: con ceros, un dispositivo que no \
         está se lee como uno cuyo identificador es cero"
    );
}

/// Los dos plazos tienen que ser grandes **los dos**: el del UBUS corta también
/// los reintentos de CRS, así que una petición muere en cuanto expira el
/// primero de los dos. El de reintentos queda algo por debajo del del UBUS.
#[test]
fn both_timeouts_have_to_be_large_because_the_ubus_one_also_kills_crs_retries() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    assert_eq!(reg(base, MISC_UBUS_TIMEOUT), UBUS_TIMEOUT_2712);
    assert_eq!(
        reg(base, MISC_RC_CONFIG_RETRY_TIMEOUT),
        RC_RETRY_TIMEOUT_2712
    );
    const _: () = assert!(
        RC_RETRY_TIMEOUT_2712 < UBUS_TIMEOUT_2712,
        "el de reintentos tiene que caber dentro del del UBUS"
    );
}

/// **El código de clase es lo que hace que un barrido de buses mire detrás del
/// puente.** Por defecto el núcleo se anuncia como *endpoint*, y quien lea eso
/// no busca nada más: sin esto el RP1 no se descubre aunque el enlace esté
/// arriba.
#[test]
fn the_class_code_is_what_makes_a_bus_scan_look_behind_the_bridge() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);
    poke(base, RC_CFG_PRIV1_ID_VAL3, 0xff00_0000);

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    let val = reg(base, RC_CFG_PRIV1_ID_VAL3);
    assert_eq!(val & CLASS_CODE, CLASS_CODE_PCI_BRIDGE, "0x060400");
    assert_eq!(val & 0xff00_0000, 0xff00_0000, "y la revisión intacta");
}

/// Las dos ventanas de entrada que no se usan se **apagan**, y se apagan
/// poniendo su campo de tamaño a cero —— dejarlas como estaban deja al bus ver
/// trozos del SoC que no le tocan.
#[test]
fn the_two_unused_inbound_windows_are_turned_off_rather_than_left_as_they_were() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);
    poke(base, MISC_RC_BAR1_CONFIG_LO, 0xffff_ffff);
    poke(base, MISC_RC_BAR3_CONFIG_LO, 0xffff_ffff);

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    assert_eq!(reg(base, MISC_RC_BAR1_CONFIG_LO) & RC_BAR_CONFIG_LO_SIZE, 0);
    assert_eq!(reg(base, MISC_RC_BAR3_CONFIG_LO) & RC_BAR_CONFIG_LO_SIZE, 0);
}

/// La ventana de entrada que sí se usa lleva la dirección de PCIe entera en dos
/// mitades y el **código** de tamaño metido en los cinco bits de abajo de la
/// mitad baja: la dirección está alineada a su tamaño, así que esos bits están
/// libres.
#[test]
fn the_inbound_window_packs_its_size_code_into_the_low_bits_of_its_address() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    let lo = reg(base, MISC_RC_BAR2_CONFIG_LO);
    assert_eq!(
        lo & RC_BAR_CONFIG_LO_SIZE,
        19,
        "el código de 16 GiB, que sale del TAMAÑO y no de la dirección \
         (la de la Pi 5 daría 21)"
    );
    assert_eq!(lo & !RC_BAR_CONFIG_LO_SIZE, 0, "la mitad baja es cero");
    assert_eq!(
        reg(base, MISC_RC_BAR2_CONFIG_HI),
        0x10,
        "y la alta, los 0x10_00000000 del dma-ranges de la Pi 5"
    );
    assert_eq!(
        reg(base, MISC_UBUS_BAR2_CONFIG_REMAP) & UBUS_BAR_CONFIG_REMAP_ACCESS_ENABLE,
        UBUS_BAR_CONFIG_REMAP_ACCESS_ENABLE,
        "la ventana quedó sin habilitar en el UBUS"
    );
}

/// El tamaño de ráfaga va al **campo**, desplazado, y sin pisar los otros dos
/// bits que esa misma escritura tiene que poner.
#[test]
fn the_burst_size_and_the_two_access_bits_all_land_in_the_one_register() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    let val = reg(base, MISC_MISC_CTRL);
    assert_eq!(val & MISC_CTRL_SCB_ACCESS_EN, MISC_CTRL_SCB_ACCESS_EN);
    assert_eq!(val & MISC_CTRL_CFG_READ_UR_MODE, MISC_CTRL_CFG_READ_UR_MODE);
    assert_eq!(
        (val & MISC_CTRL_MAX_BURST_SIZE) >> MISC_CTRL_MAX_BURST_SIZE.trailing_zeros(),
        BURST_SIZE_2712
    );
}

// --- el QoS -----------------------------------------------------------------

/// El bit de arreglo del QoS **se relee**: si vuelve a cero, en este chip está
/// reservado —— un 2712C1 o un puente de una sola línea —— y el remedio es
/// otro. No es comprobar que la escritura llegó: es preguntarle al chip qué es.
#[test]
fn the_qos_fix_bit_is_read_back_to_ask_the_chip_which_one_it_is() {
    let t = trasto();
    let (pcie, base) = (&t.pcie, t.base);
    // La búsqueda de reenvío llega PUESTA del firmware; si no, limpiarla no se
    // distingue de no tocarla.
    poke(base, MISC_AXI_INTF_CTRL, AXI_REQFIFO_EN_QOS_PROPAGATION);

    // Un chip que acepta el arreglo: no hace falta el remedio.
    pcie.set_tc_qos();
    let val = reg(base, MISC_AXI_INTF_CTRL);
    assert_eq!(
        val & AXI_EN_QOS_UPDATE_TIMING_FIX,
        AXI_EN_QOS_UPDATE_TIMING_FIX
    );
    assert_eq!(
        val & AXI_MASTER_MAX_OUTSTANDING_REQUESTS,
        0,
        "se limitaron las peticiones en un chip que no lo necesita"
    );
    assert_eq!(
        val & AXI_REQFIFO_EN_QOS_PROPAGATION,
        0,
        "la búsqueda de reenvío de QoS, que está rota, quedó encendida"
    );
    assert_eq!(val & AXI_EN_RCLK_QOS_ARRAY_FIX, AXI_EN_RCLK_QOS_ARRAY_FIX);
    assert_eq!(
        val & AXI_DIS_QOS_GATING_IN_MASTER,
        AXI_DIS_QOS_GATING_IN_MASTER
    );
}

/// Y el remedio del 2712C1, donde ese bit **está reservado y lee cero**, es
/// limitar las peticiones en vuelo a quince. Es el único camino del driver que
/// depende de qué revisión del chip sea, y por eso el banco de pruebas sabe
/// declarar un bit reservado.
#[test]
fn a_chip_where_the_qos_fix_is_reserved_gets_its_requests_throttled_instead() {
    let t = trasto();
    let (pcie, base) = (&t.pcie, t.base);

    let (_, traza) = bridge_fake::trazando(|| {
        bridge_fake::reservar(MISC_AXI_INTF_CTRL, AXI_EN_QOS_UPDATE_TIMING_FIX);
        pcie.set_tc_qos();
    });

    assert_eq!(
        reg(base, MISC_AXI_INTF_CTRL) & AXI_MASTER_MAX_OUTSTANDING_REQUESTS,
        15,
        "el remedio son QUINCE peticiones en vuelo; un cero no es «sin límite», \
         es ninguna"
    );
    assert_eq!(
        traza
            .iter()
            .filter(|(f, o, _)| *f == bridge_fake::PUENTE && *o == MISC_AXI_INTF_CTRL)
            .count(),
        2,
        "el remedio es una segunda escritura, traza {:x?}",
        traza
    );
    const _: () = assert!(
        AXI_OUTSTANDING_FALLBACK & !AXI_MASTER_MAX_OUTSTANDING_REQUESTS == 0,
        "el quince tiene que caber en el campo de peticiones en vuelo"
    );
}

/// Y en un chip que **sí** tiene el arreglo, el remedio no se aplica: una sola
/// escritura en ese registro.
#[test]
fn a_chip_that_has_the_qos_fix_is_left_with_its_requests_unthrottled() {
    let t = trasto();
    let pcie = &t.pcie;

    let (_, traza) = bridge_fake::trazando(|| pcie.set_tc_qos());

    assert_eq!(
        traza
            .iter()
            .filter(|(f, o, _)| *f == bridge_fake::PUENTE && *o == MISC_AXI_INTF_CTRL)
            .count(),
        1,
        "se aplicó el remedio sin hacer falta, traza {:x?}",
        traza
    );
}

/// La recepción de VDM queda apagada, que es el mapa de QoS por defecto, y sin
/// tocar el resto de su registro.
#[test]
fn vdm_reception_is_left_off_which_is_the_default_qos_map() {
    let t = trasto();
    let (pcie, base) = (&t.pcie, t.base);
    poke(
        base,
        MISC_CTRL_1,
        MISC_CTRL_1_EN_VDM_QOS_CONTROL | 0x5555_0000,
    );

    pcie.set_tc_qos();

    let val = reg(base, MISC_CTRL_1);
    assert_eq!(val & MISC_CTRL_1_EN_VDM_QOS_CONTROL, 0);
    assert_eq!(val & 0x5555_0000, 0x5555_0000);
}

// --- el enlace --------------------------------------------------------------

/// Los 100 ms tras PERST# son lo que la norma CEM le concede al dispositivo, y
/// **no se pueden recortar** mirando si el enlace ya subió: el sondeo viene
/// después, no en su lugar.
#[test]
fn the_hundred_milliseconds_after_perst_are_spent_even_when_the_link_is_already_up() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);

    let (r, esperado) = reset::reset_fake::esperando(|| pcie.start_link(mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    assert!(
        esperado >= 100_000,
        "la norma CEM concede 100 ms enteros tras PERST#, y eso son 100_000 µs \
         (esperó {})",
        esperado
    );
}

/// Y el sondeo sí se corta en cuanto el enlace sube: con el enlace arriba desde
/// el principio se gastan los 100 ms de la norma y nada más.
#[test]
fn the_polling_stops_as_soon_as_the_link_is_up() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);

    let (_, esperado) = reset::reset_fake::esperando(|| pcie.start_link(mdio, &cfg_pi5()));

    assert_eq!(
        esperado, 100_000,
        "se siguió sondeando con el enlace ya arriba, o se recortó el plazo \
         de la norma"
    );
}

/// Con el enlace caído se gasta el peor caso entero, 200 ms, y se da por
/// perdido. Ni se sondea para siempre ni se abandona antes de tiempo.
#[test]
fn a_link_that_never_comes_up_costs_the_whole_two_hundred_milliseconds_and_no_more() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    let _ = base;
    // `MISC_PCIE_STATUS` se queda a cero: el enlace no sube.

    let (r, esperado) = reset::reset_fake::esperando(|| pcie.start_link(mdio, &cfg_pi5()));

    assert_eq!(r, Err(BringUpError::EnlaceCaido));
    assert_eq!(
        esperado, 200_000,
        "el peor caso son los 100 ms de la norma más los 100 del sondeo"
    );
}

/// PERST# se **suelta**, que en este chip es escribir un uno en el bit con
/// barra.
#[test]
fn starting_the_link_releases_perst_rather_than_asserting_it() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);

    assert!(pcie.start_link(mdio, &cfg_pi5()).is_ok());

    assert_eq!(
        reg(base, MISC_PCIE_CTRL) & PCIE_CTRL_PERSTB,
        PCIE_CTRL_PERSTB,
        "el reset se quedó apretado"
    );
}

/// La velocidad y la anchura salen del `LNKSTA`, cada una de su campo. El
/// `LNKSTA` es de **16 bits**: leerlo de 32 arrastraría el `LNKCTL` de al lado.
#[test]
fn the_speed_and_width_come_out_of_their_own_fields_of_a_sixteen_bit_register() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);
    // Gen3 por cuatro líneas, con basura en el registro de al lado.
    poke16(base, CAP_REGS + CAP_LNKSTA, 3 | (4 << 4));
    poke16(base, CAP_REGS + CAP_LNKSTA + 2, 0xffff);

    let info = pcie.start_link(mdio, &cfg_pi5()).expect("el enlace sube");

    assert_eq!(info.speed_code, 3, "la velocidad");
    assert_eq!(info.width, 4, "la anchura");
}

/// La visibilidad de CRS la borra PERST#, así que se vuelve a pedir —— pero
/// **sólo si el puente dice que la tiene**. Pedirla donde no está escribe en un
/// bit reservado.
#[test]
fn crs_visibility_is_only_re_requested_on_a_bridge_that_says_it_has_it() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);
    // El puente no la anuncia.
    poke16(base, CAP_REGS + CAP_RTCAP, 0);

    assert!(pcie.start_link(mdio, &cfg_pi5()).is_ok());
    assert_eq!(
        reg16(base, CAP_REGS + CAP_RTCTL) & RTCTL_CRSSVE,
        0,
        "se pidió en un puente que no la tiene"
    );

    // Y ahora uno que sí.
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);
    poke16(base, CAP_REGS + CAP_RTCAP, RTCAP_CRSVIS);

    assert!(pcie.start_link(mdio, &cfg_pi5()).is_ok());
    assert_eq!(
        reg16(base, CAP_REGS + CAP_RTCTL) & RTCTL_CRSSVE,
        RTCTL_CRSSVE
    );
}

/// Limitar la generación toca **dos** registros de anchuras distintas: el
/// `LNKCAP` de 32 y el `LNKCTL2` de 16.
#[test]
fn capping_the_generation_touches_both_the_link_capability_and_its_control() {
    let t = trasto();
    let (pcie, base) = (&t.pcie, t.base);
    poke(base, CAP_REGS + CAP_LNKCAP, 0xabcd_0003);
    poke16(base, CAP_REGS + CAP_LNKCTL2, 0x0003);

    pcie.set_gen(2);

    assert_eq!(
        reg(base, CAP_REGS + CAP_LNKCAP),
        0xabcd_0002,
        "el LNKCAP, con el resto intacto"
    );
    assert_eq!(
        reg16(base, CAP_REGS + CAP_LNKCTL2),
        0x0002,
        "y los cuatro bits de abajo del LNKCTL2"
    );
}

/// La Pi 5 declara `max-link-speed = <2>`, y eso se aplica **después** de que
/// el enlace suba, no antes: el tope cambia la renegociación, no el arranque.
#[test]
fn the_generation_cap_is_applied_after_the_link_is_up_and_not_before() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);
    poke(base, CAP_REGS + CAP_LNKCAP, 3);

    let (r, traza) = bridge_fake::trazando(|| pcie.start_link(mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    let i_perst = primera(&traza, PUENTE, MISC_PCIE_CTRL).expect("PERST");
    let i_gen = primera(&traza, PUENTE, CAP_REGS + CAP_LNKCAP).expect("el tope");
    assert!(i_perst < i_gen, "traza {:x?}", traza);
    assert_eq!(reg(base, CAP_REGS + CAP_LNKCAP) & LNKCAP_SLS, 2);
}

/// Un ensanchado que no cuaja **no tumba el arranque**: se sigue sin él, y se
/// dice en el resultado. Es un reloj mejor, no un requisito.
#[test]
fn a_spread_spectrum_clock_that_does_not_take_does_not_bring_down_the_bring_up() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);
    let mut cfg = cfg_pi5();
    cfg.ssc = true;

    let info = pcie.start_link(mdio, &cfg).expect("el enlace sube igual");

    assert!(!info.ssc, "dijo que el ensanchado cuajó");
}

/// Y con el ensanchado sin pedir, no se toca: la Pi 5 no declara
/// `brcm,enable-ssc`.
#[test]
fn a_bridge_that_does_not_ask_for_spread_spectrum_never_touches_the_phy_for_it() {
    let t = trasto();
    let (pcie, base, mdio) = (&t.pcie, t.base, &t.mdio);
    todo_bien(base);

    let (r, traza) = bridge_fake::trazando(|| pcie.start_link(mdio, &cfg_pi5()));

    assert!(!r.expect("el enlace sube").ssc);
    assert_eq!(
        primera(&traza, PUENTE, RC_DL_MDIO_ADDR),
        None,
        "se habló con el PHY sin que nadie lo pidiera"
    );
}

// --- las ventanas de salida -------------------------------------------------

/// Una ventana de salida que el puente no puede abrir **para el arranque**: sin
/// ella la CPU no llega al dispositivo, así que seguir es levantar un enlace
/// que no sirve para nada.
#[test]
fn an_outbound_window_the_bridge_cannot_open_stops_the_setup() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);
    let mut cfg = cfg_pi5();
    cfg.outbound[1] = Some(OutboundWin {
        cpu: 0x8000_0000,
        pcie: 0,
        size: 0,
    });

    let r = pcie.setup(rst, mdio, &cfg);

    assert_eq!(r, Err(BringUpError::VentanaDeSalida(1)));
}

/// La ventana de la Pi 5 queda donde tiene que quedar: `0x1f_00000000` por el
/// lado de la CPU, y la dirección cero por el del bus. Es la que hace que
/// `0x1f_00100000` sea el GEM del RP1.
#[test]
fn the_pi_fives_outbound_window_is_the_one_that_makes_the_gems_address_work() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    assert!(pcie.setup(rst, mdio, &cfg_pi5()).is_ok());

    assert_eq!(reg(base, mem_win_lo(0)), 0, "la dirección cero del bus");
    assert_eq!(reg(base, mem_win_hi(0)), 0);
    assert_eq!(
        reg(base, mem_win_base_hi(0)) & MEM_WIN_HI_ADDR,
        0x1f,
        "y los 0x1f_00000000 por el lado de la CPU"
    );
}

/// `bring_up` prepara el puente **antes** de intentar el enlace. Soltar PERST#
/// con el puente sin preparar gasta los 200 ms del peor caso para nada: ni
/// están las ventanas, ni el SerDes, ni el PLL del reloj de 54 MHz.
#[test]
fn the_bridge_is_prepared_before_the_link_is_attempted() {
    let t = trasto();
    let (pcie, base, rst, rescal, mdio) = (&t.pcie, t.base, &t.rst, &t.rescal, &t.mdio);
    todo_bien(base);
    poke(
        t.rescal_base,
        reset::RESCAL_STATUS,
        reset::RESCAL_STATUS_BIT,
    );

    let (r, traza) = bridge_fake::trazando(|| pcie.bring_up(rst, rescal, mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    // El SerDes es lo primero que `setup` escribe en el puente; PERST#, lo
    // primero que escribe `start_link`.
    let i_setup = primera(&traza, PUENTE, MISC_HARD_DEBUG_2712).expect("la preparación");
    let i_enlace = primera(&traza, PUENTE, MISC_PCIE_CTRL).expect("el enlace");
    assert!(
        i_setup < i_enlace,
        "se soltó PERST# con el puente sin preparar, traza {:x?}",
        traza
    );
}

/// Las cuatro esperas de la preparación están y son las que son: el puente
/// dentro de reset, el margen que el bloque necesita al salir, el SerDes
/// estabilizándose y el PLL asentándose. Tocar los registros del SerDes antes
/// de que se estabilice lee basura sin dar error.
#[test]
fn the_setup_spends_its_four_waits_and_no_others() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    let (r, esperado) = reset::reset_fake::esperando(|| pcie.setup(rst, mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    assert_eq!(
        esperado,
        100 + 100 + 100 + 100,
        "los 100 µs con el puente en reset, los 100 de margen al soltarlo, \
         los 100 del SerDes y los 100 del PLL"
    );
    assert_eq!(BRIDGE_RESET_US, 100);
    assert_eq!(SERDES_SETTLE_US, 100);
    assert_eq!(super::super::mdio::PLL_SETTLE_US, 100);
}

/// El orden de bytes de la ventana de entrada se pone **al final**, después de
/// que la ventana exista.
#[test]
fn the_endianness_of_the_inbound_window_is_set_last() {
    let t = trasto();
    let (pcie, base, rst, mdio) = (&t.pcie, t.base, &t.rst, &t.mdio);
    todo_bien(base);

    let (r, traza) = bridge_fake::trazando(|| pcie.setup(rst, mdio, &cfg_pi5()));

    assert!(r.is_ok(), "{:?}", r);
    let i_bar2 = primera(&traza, PUENTE, MISC_RC_BAR2_CONFIG_LO).expect("la ventana");
    let i_endian = primera(&traza, PUENTE, RC_CFG_VENDOR_SPECIFIC_REG1).expect("el orden de bytes");
    assert!(i_bar2 < i_endian, "traza {:x?}", traza);
    assert_eq!(
        reg(base, RC_CFG_VENDOR_SPECIFIC_REG1) & ENDIAN_MODE_BAR2,
        ENDIAN_MODE_LITTLE
    );
}
