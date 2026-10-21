//! Los caminos del e1000e que hablan con el PHY, el semaforo SW/FW, el
//! Kumeran, la EEPROM y los anillos, contra el MAC falso de
//! [`super::e1000e_fake`].
//!
//! Lo que estos tests ejercitan es el driver de verdad: `disable_ulp`,
//! `configure_k1`, `toggle_lanphypc`, `oem_bits_config`, `restart_autoneg`,
//! `init_tx`, `init_rx` y el watchdog, con una tarjeta falsa detras que
//! contesta a los accesos al PHY y puede negarse a contestarlos. Eso es lo que
//! no se puede probar en hardware desde aqui y lo que mas caro sale equivocar:
//! un bit de LPLU que no se baja son 10 Mb/s en un switch de gigabit, y una
//! cola que no latchea es una tarjeta muda que parece sana.

use super::e1000e_fake::Hw;
use super::rx_ring_tests::make_hw_at;
use super::*;
extern crate std;

/// I219 sobre Sunrise Point o posterior: el camino con ULP, LPCD y colas que
/// hay que habilitar a mano.
const I219_SPT: u16 = 0x1570;
/// I217: PCH, pero antes de Sunrise Point.
const I217: u16 = 0x153a;
/// 82574L: discreta, el que hay en QEMU.
const DISCRETA: u16 = 0x10d3;

/// El reloj virtual avanza solo: los `udelay` del driver son esperas de
/// milisegundos y sin esto una espera que no se satisface no acaba nunca.
fn reloj() {
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1_000);
}

/// Una tarjeta falsa y el driver encima.
fn tarjeta(device_id: u16) -> (Hw, E1000eHw) {
    reloj();
    let hw = Hw::new();
    let dev = make_hw_at(hw.base(), device_id);
    (hw, dev)
}

/// Un PHY que contesta como uno real recien negociado: enlace arriba,
/// autonegociacion completa, gigabit full duplex y PAUSE simetrico por las dos
/// partes.
fn phy_sano(hw: &Hw, addr: u8) {
    hw.set_mii(
        addr,
        MII_BMSR,
        BMSR_LSTATUS | BMSR_ANEGCOMPLETE | BMSR_ESTATEN,
    );
    hw.set_mii(addr, MII_BMCR, MII_CR_AUTO_NEG_EN);
    hw.set_mii(
        addr,
        MII_ADVERTISE,
        ADVERTISE_ALL_10_100 | ADVERTISE_PAUSE_CAP | ADVERTISE_PAUSE_ASYM,
    );
    hw.set_mii(addr, MII_LPA, LPA_100FULL | LPA_PAUSE_CAP | LPA_PAUSE_ASYM);
    hw.set_mii(addr, MII_CTRL1000, ADVERTISE_1000FULL);
    hw.set_mii(addr, MII_STAT1000, LPA_1000FULL);
    hw.set_mii(addr, MII_ESTATUS, ESTATUS_1000_TFULL);
}

// ---------------------------------------------------------------------------
// MDIO y el acceso paginado al PHY
// ---------------------------------------------------------------------------

#[test]
fn un_phy_que_no_contesta_no_devuelve_valor() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.set_mii(1, MII_BMSR, 0x1234);
    hw.faults(|f| f.mdic_mute.push(1));
    assert!(
        unsafe { dev.mdic_read(1, MII_BMSR) }.is_none(),
        "se ha creido un MDIC sin su bit de listo"
    );
}

#[test]
fn un_phy_que_contesta_con_error_no_devuelve_valor() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.set_mii(1, MII_BMSR, 0x1234);
    hw.faults(|f| f.mdic_error = true);
    assert!(unsafe { dev.mdic_read(1, MII_BMSR) }.is_none());
    assert!(
        !unsafe { dev.mdic_write(1, MII_BMCR, 0x8000) },
        "una escritura con error se ha dado por buena"
    );
}

#[test]
fn una_lectura_paginada_selecciona_la_pagina_antes_del_registro() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.set_phy_hv(CV_SMB_CTRL_PAGE, CV_SMB_CTRL_REG, 0x0003);
    hw.set_phy_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG, 0x0070);
    assert_eq!(
        unsafe { dev.phy_read_hv(CV_SMB_CTRL_PAGE, CV_SMB_CTRL_REG) },
        Some(0x0003)
    );
    // La segunda lectura tiene que volver a seleccionar: si la pagina se
    // quedara pegada, aqui saldria el registro de la pagina anterior.
    assert_eq!(
        unsafe { dev.phy_read_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG) },
        Some(0x0070)
    );
}

#[test]
fn la_pagina_768_se_direcciona_escribiendo_un_cero() {
    let (hw, dev) = tarjeta(I219_SPT);
    // `hv_page_select(768)` vale 0, y el registro se siembra con ese cero a
    // mano: sembrarlo con la misma funcion que se esta probando seria probarla
    // contra si misma, y un driver que la pagina 768 la pidiera por su numero
    // pasaria igual.
    hw.set_phy_raw(HV_PHY_ADDR, 0, HV_OEM_BITS_REG, HV_OEM_BITS_LPLU);
    assert_eq!(
        unsafe { dev.phy_read_hv(HV_OEM_BITS_PAGE, HV_OEM_BITS_REG) },
        Some(HV_OEM_BITS_LPLU)
    );
}

#[test]
fn un_registro_ieee_es_el_mismo_en_cualquier_pagina() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.set_mii(HV_PHY_ADDR, MII_BMSR, BMSR_LSTATUS);
    // Deja la seleccion de pagina en otra parte y pide un registro IEEE: los
    // de 0x00 a 0x0F no estan paginados.
    hw.set_phy_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG, 0);
    let _ = unsafe { dev.phy_read_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG) };
    assert_eq!(
        unsafe { dev.phy_read_hv(ULP_CONFIG1_PAGE, MII_BMSR) },
        Some(BMSR_LSTATUS)
    );
}

// ---------------------------------------------------------------------------
// El semaforo SW/FW
// ---------------------------------------------------------------------------

#[test]
fn el_semaforo_se_coge_y_se_suelta() {
    let (hw, dev) = tarjeta(I219_SPT);
    assert!(unsafe { dev.acquire_swflag() });
    assert!(
        hw.peek(E1000E_EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG != 0,
        "el bit del semaforo no quedo puesto"
    );
    unsafe { dev.release_swflag() };
    assert_eq!(hw.peek(E1000E_EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG, 0);
    assert_eq!(hw.swflag_takes(), 1);
}

#[test]
fn un_semaforo_que_la_firmware_tiene_cogido_no_se_coge() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.faults(|f| f.swflag_busy = true);
    hw.poke(E1000E_EXTCNF_CTRL, EXTCNF_CTRL_SWFLAG);
    assert!(
        !unsafe { dev.acquire_swflag() },
        "se ha dado por cogido un semaforo ocupado"
    );
}

#[test]
fn un_semaforo_que_no_latcha_no_se_da_por_cogido() {
    let (hw, dev) = tarjeta(I219_SPT);
    // El registro se lee libre, el driver escribe el bit y la ME se lo quita:
    // darlo por cogido es hablar con el PHY mientras la firmware tambien lo
    // hace.
    hw.faults(|f| f.swflag_no_latch = true);
    assert!(
        !unsafe { dev.acquire_swflag() },
        "se dio por cogido un semaforo que no latcho"
    );
}

// ---------------------------------------------------------------------------
// ULP
// ---------------------------------------------------------------------------

#[test]
fn el_ulp_solo_se_deshace_en_sunrise_point_o_posterior() {
    for id in [DISCRETA, I217] {
        let (hw, dev) = tarjeta(id);
        hw.poke(E1000E_H2ME, H2ME_ULP);
        hw.poke(E1000E_FEXTNVM7, FEXTNVM7_DISABLE_SMB_PERST);
        hw.set_phy_hv(CV_SMB_CTRL_PAGE, CV_SMB_CTRL_REG, CV_SMB_CTRL_FORCE_SMBUS);
        unsafe { dev.disable_ulp(true) };
        assert_eq!(
            hw.peek(E1000E_H2ME),
            H2ME_ULP,
            "ha tocado el H2ME de una tarjeta que no tiene ULP ({id:#06x})"
        );
        // Y tampoco el camino de software: en una tarjeta sin ULP, bajarle el
        // SMBus forzado al PHY es deshacer algo que nadie configuro.
        assert!(
            hw.phy_writes().is_empty(),
            "ha hablado con el PHY de una tarjeta que no tiene ULP ({id:#06x}): {:?}",
            hw.phy_writes()
        );
        assert_ne!(hw.peek(E1000E_FEXTNVM7) & FEXTNVM7_DISABLE_SMB_PERST, 0);
    }
}

#[test]
fn con_firmware_viva_el_ulp_se_le_pide_a_la_me() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_FWSM, ICH_FWSM_FW_VALID | FWSM_ULP_CFG_DONE);
    hw.poke(E1000E_H2ME, H2ME_ULP);
    unsafe { dev.disable_ulp(true) };
    assert_eq!(
        hw.peek(E1000E_FWSM) & FWSM_ULP_CFG_DONE,
        0,
        "la ME no llego a deshacer el ULP"
    );
    let h2me = hw.peek(E1000E_H2ME);
    assert_eq!(h2me & H2ME_ULP, 0, "la peticion de ULP sigue puesta");
    assert_eq!(
        h2me & H2ME_ENFORCE_SETTINGS,
        0,
        "el bit de forzar ajustes se queda pegado y la ME no vuelve a mirar el resto"
    );
    assert!(
        hw.phy_writes().is_empty(),
        "con firmware viva no se toca el PHY: {:?}",
        hw.phy_writes()
    );
}

#[test]
fn si_la_me_no_termina_el_ulp_se_sigue_adelante() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.faults(|f| f.ulp_cfg_stuck = true);
    hw.poke(E1000E_FWSM, ICH_FWSM_FW_VALID | FWSM_ULP_CFG_DONE);
    unsafe { dev.disable_ulp(true) };
    assert_ne!(
        hw.peek(E1000E_FWSM) & FWSM_ULP_CFG_DONE,
        0,
        "el falso tenia que dejar cfg_done puesto"
    );
    assert_eq!(
        hw.peek(E1000E_H2ME) & H2ME_ENFORCE_SETTINGS,
        0,
        "se quedo esperando sin soltar el bit de forzar ajustes"
    );
}

#[test]
fn sin_firmware_el_ulp_se_deshace_por_el_phy() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_FWSM, 0);
    hw.poke(E1000E_CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
    hw.poke(E1000E_FEXTNVM7, FEXTNVM7_DISABLE_SMB_PERST);
    hw.set_phy_hv(CV_SMB_CTRL_PAGE, CV_SMB_CTRL_REG, CV_SMB_CTRL_FORCE_SMBUS);
    hw.set_phy_hv(HV_PM_CTRL_PAGE, HV_PM_CTRL_REG, 0);
    hw.set_phy_hv(
        ULP_CONFIG1_PAGE,
        ULP_CONFIG1_REG,
        ULP_CONFIG1_IND | ULP_CONFIG1_STICKY_ULP | ULP_CONFIG1_WOL_HOST,
    );

    unsafe { dev.disable_ulp(true) };

    assert_eq!(
        hw.phy_hv(CV_SMB_CTRL_PAGE, CV_SMB_CTRL_REG),
        Some(0),
        "el SMBus forzado sigue puesto en el PHY"
    );
    assert_eq!(
        hw.phy_hv(HV_PM_CTRL_PAGE, HV_PM_CTRL_REG),
        Some(HV_PM_CTRL_K1_ENABLE),
        "K1 no se volvio a habilitar"
    );
    assert_eq!(
        hw.phy_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG),
        Some(ULP_CONFIG1_START),
        "la configuracion de ULP no quedo limpia y confirmada"
    );
    // El START va en una escritura APARTE y DESPUES de limpiar: en la misma
    // escritura el PHY confirmaria una configuracion que todavia tiene los
    // bits viejos.
    let ulp: alloc::vec::Vec<u16> = hw
        .phy_writes()
        .into_iter()
        .filter(|(_, _, reg, _)| *reg == ULP_CONFIG1_REG)
        .map(|(_, _, _, val)| val)
        .collect();
    assert_eq!(ulp, alloc::vec![0, ULP_CONFIG1_START], "{ulp:?}");
    assert_eq!(hw.peek(E1000E_CTRL_EXT) & CTRL_EXT_FORCE_SMBUS, 0);
    assert_eq!(hw.peek(E1000E_FEXTNVM7) & FEXTNVM7_DISABLE_SMB_PERST, 0);
    assert_eq!(
        hw.peek(E1000E_EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG,
        0,
        "se dejo el semaforo cogido"
    );
}

#[test]
fn sin_firmware_y_con_el_semaforo_ocupado_el_ulp_no_toca_el_phy() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_FWSM, 0);
    hw.faults(|f| f.swflag_busy = true);
    hw.poke(E1000E_EXTCNF_CTRL, EXTCNF_CTRL_SWFLAG);
    hw.poke(E1000E_FEXTNVM7, FEXTNVM7_DISABLE_SMB_PERST);
    unsafe { dev.disable_ulp(true) };
    assert!(
        hw.phy_writes().is_empty(),
        "ha hablado con el PHY sin el semaforo"
    );
    assert_ne!(
        hw.peek(E1000E_FEXTNVM7) & FEXTNVM7_DISABLE_SMB_PERST,
        0,
        "ha seguido con la secuencia sin el semaforo"
    );
}

// ---------------------------------------------------------------------------
// GIO master
// ---------------------------------------------------------------------------

#[test]
fn el_dma_en_vuelo_se_quiesce_antes_del_reset() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_STATUS, STATUS_LU | STATUS_GIO_MASTER_ENABLE);
    assert!(unsafe { dev.disable_pcie_master() });
    assert_ne!(
        hw.peek(E1000E_CTRL) & CTRL_GIO_MASTER_DISABLE,
        0,
        "no pidio deshabilitar el maestro"
    );
}

#[test]
fn un_maestro_que_no_se_para_se_reporta() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.faults(|f| f.gio_master_stuck = true);
    hw.poke(E1000E_STATUS, STATUS_LU | STATUS_GIO_MASTER_ENABLE);
    assert!(
        !unsafe { dev.disable_pcie_master() },
        "dio por quiescido un maestro que sigue pidiendo"
    );
}

// ---------------------------------------------------------------------------
// Kumeran y K1
// ---------------------------------------------------------------------------

#[test]
fn el_kumeran_se_lee_y_se_escribe_por_la_misma_ventana() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.set_kmrn(KMRNCTRLSTA_K1_CONFIG, 0x0005);
    assert_eq!(unsafe { dev.kmrn_read(KMRNCTRLSTA_K1_CONFIG) }, 0x0005);
    unsafe { dev.kmrn_write(KMRNCTRLSTA_K1_CONFIG, 0x0007) };
    assert_eq!(hw.kmrn(KMRNCTRLSTA_K1_CONFIG), Some(0x0007));
    // Y cada offset es el suyo: una escritura no se lleva por delante al vecino.
    assert_eq!(hw.kmrn(KMRNCTRLSTA_TIMEOUTS), None);
}

#[test]
fn k1_se_habilita_sin_perder_el_ctrl_que_habia() {
    let (hw, dev) = tarjeta(I219_SPT);
    let ctrl = CTRL_SLU | CTRL_ASDE;
    hw.poke(E1000E_CTRL, ctrl);
    hw.poke(E1000E_CTRL_EXT, CTRL_EXT_DRV_LOAD);
    hw.set_kmrn(KMRNCTRLSTA_K1_CONFIG, 0);

    unsafe { dev.configure_k1(true) };

    assert_eq!(
        hw.kmrn(KMRNCTRLSTA_K1_CONFIG),
        Some(KMRNCTRLSTA_K1_ENABLE),
        "K1 no quedo habilitado"
    );
    // El baile de FRCSPD/SPD_BYPS es temporal: si CTRL se quedara con los bits
    // forzados, el enlace quedaria clavado a la velocidad de ese momento.
    assert_eq!(
        hw.peek(E1000E_CTRL),
        ctrl,
        "no restauro CTRL tras forzar la velocidad"
    );
    assert_eq!(
        hw.peek(E1000E_CTRL_EXT),
        CTRL_EXT_DRV_LOAD,
        "no restauro CTRL_EXT"
    );
    assert_eq!(hw.swflag_takes(), 1, "no paso por el semaforo");
}

#[test]
fn k1_se_deshabilita_cuando_se_pide() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.set_kmrn(KMRNCTRLSTA_K1_CONFIG, KMRNCTRLSTA_K1_ENABLE | 0x0001);
    unsafe { dev.configure_k1(false) };
    assert_eq!(
        hw.kmrn(KMRNCTRLSTA_K1_CONFIG),
        Some(0x0001),
        "se llevo por delante el resto del registro"
    );
}

#[test]
fn k1_no_existe_en_una_discreta() {
    let (hw, dev) = tarjeta(DISCRETA);
    unsafe { dev.configure_k1(true) };
    assert_eq!(hw.kmrn(KMRNCTRLSTA_K1_CONFIG), None);
    assert_eq!(hw.swflag_takes(), 0);
}

#[test]
fn k1_con_el_semaforo_ocupado_no_toca_el_kumeran() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.faults(|f| f.swflag_busy = true);
    hw.poke(E1000E_EXTCNF_CTRL, EXTCNF_CTRL_SWFLAG);
    unsafe { dev.configure_k1(true) };
    assert_eq!(hw.kmrn(KMRNCTRLSTA_K1_CONFIG), None);
}

#[test]
fn los_tiempos_de_cobre_del_kumeran_se_programan_sin_borrar_lo_demas() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.set_kmrn(KMRNCTRLSTA_INBAND_PARAM, 0x0100);
    unsafe { dev.setup_kmrn_copper_timeouts() };
    assert_eq!(hw.kmrn(KMRNCTRLSTA_TIMEOUTS), Some(0xFFFF));
    assert_eq!(
        hw.kmrn(KMRNCTRLSTA_INBAND_PARAM),
        Some(0x013F),
        "el suelo del parametro in-band borro lo que ya habia"
    );
}

// ---------------------------------------------------------------------------
// LANPHYPC
// ---------------------------------------------------------------------------

#[test]
fn el_lanphypc_se_fuerza_y_se_suelta() {
    let (hw, dev) = tarjeta(I217);
    unsafe { dev.toggle_lanphypc() };
    let ctrl = hw.peek(E1000E_CTRL);
    assert_eq!(
        ctrl & CTRL_LANPHYPC_OVERRIDE,
        0,
        "dejo el LANPHYPC forzado: el PHY se queda sin su secuencia de arranque"
    );
    assert_eq!(ctrl & CTRL_LANPHYPC_VALUE, 0);
    assert_ne!(
        hw.peek(E1000E_CTRL_EXT) & CTRL_EXT_LPCD,
        0,
        "el falso tenia que haber dado la señal de config del PHY"
    );
}

#[test]
fn en_sunrise_point_se_espera_la_senal_del_phy_y_no_se_cuelga_sin_ella() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.faults(|f| f.lpcd_never = true);
    unsafe { dev.toggle_lanphypc() };
    assert_eq!(
        hw.peek(E1000E_CTRL_EXT) & CTRL_EXT_LPCD,
        0,
        "el falso no tenia que dar la señal"
    );
    assert_eq!(hw.peek(E1000E_CTRL) & CTRL_LANPHYPC_OVERRIDE, 0);
}

#[test]
fn el_lanphypc_no_se_toca_en_una_discreta() {
    let (hw, dev) = tarjeta(DISCRETA);
    hw.poke(E1000E_CTRL, CTRL_SLU);
    unsafe { dev.toggle_lanphypc() };
    assert_eq!(hw.peek(E1000E_CTRL), CTRL_SLU);
    // El baile deja CTRL igual que estaba, asi que lo que lo delata es el
    // PHY: una discreta no tiene secuencia de arranque que relanzar.
    assert_eq!(
        hw.peek(E1000E_CTRL_EXT),
        0,
        "forzo y solto el LANPHYPC de una tarjeta que no lo tiene"
    );
}

// ---------------------------------------------------------------------------
// HV_OEM_BITS (LPLU)
// ---------------------------------------------------------------------------

#[test]
fn el_lplu_del_phy_se_baja_con_la_puerta_del_nvm_abierta() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_FEXTNVM, FEXTNVM_SW_CONFIG_ICH8M);
    hw.poke(E1000E_PHY_CTRL, 0);
    hw.set_phy_hv(
        HV_OEM_BITS_PAGE,
        HV_OEM_BITS_REG,
        HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS | 0x0002,
    );

    unsafe { dev.oem_bits_config(true) };

    let oem = hw.phy_hv(HV_OEM_BITS_PAGE, HV_OEM_BITS_REG).unwrap();
    assert_eq!(oem & HV_OEM_BITS_LPLU, 0, "LPLU sigue puesto: 10 Mb/s");
    assert_eq!(
        oem & HV_OEM_BITS_GBE_DIS,
        0,
        "el gigabit sigue deshabilitado"
    );
    assert_ne!(
        oem & HV_OEM_BITS_RESTART_AN,
        0,
        "no pidio reiniciar la autonegociacion, asi que el cambio no llega al cable"
    );
    assert_eq!(oem & 0x0002, 0x0002, "se llevo por delante otro bit");
}

#[test]
fn con_la_puerta_del_nvm_cerrada_solo_se_baja_el_lplu() {
    let (hw, dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_FEXTNVM, 0);
    // GBE_DIS puesto en el PHY y la puerta cerrada: el camino estrecho de
    // Linux solo toca LPLU, asi que el gigabit se queda como estaba.
    hw.set_phy_hv(
        HV_OEM_BITS_PAGE,
        HV_OEM_BITS_REG,
        HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS,
    );
    unsafe { dev.oem_bits_config(true) };
    let oem = hw.phy_hv(HV_OEM_BITS_PAGE, HV_OEM_BITS_REG).unwrap();
    assert_eq!(oem & HV_OEM_BITS_LPLU, 0, "LPLU sigue puesto: 10 Mb/s");
    assert_ne!(oem & HV_OEM_BITS_GBE_DIS, 0);
    assert_ne!(oem & HV_OEM_BITS_RESTART_AN, 0);
}

#[test]
fn un_hv_oem_bits_que_no_se_puede_leer_no_se_escribe() {
    let (hw, dev) = tarjeta(I219_SPT);
    // 0xFFFF es lo que contesta un bus MDIO sin nadie al otro lado: escribir
    // ahi la version "arreglada" de ese valor seria inventarse un PHY.
    hw.set_phy_hv(HV_OEM_BITS_PAGE, HV_OEM_BITS_REG, 0xFFFF);
    unsafe { dev.oem_bits_config(true) };
    assert!(
        hw.phy_writes()
            .iter()
            .all(|(_, _, reg, _)| *reg != HV_OEM_BITS_REG),
        "escribio en un registro que no se pudo leer: {:?}",
        hw.phy_writes()
    );
}

#[test]
fn el_lplu_no_se_toca_en_una_discreta() {
    let (hw, dev) = tarjeta(DISCRETA);
    // Con el bit puesto en el PHY: si el driver mirara aqui, escribiria.
    hw.set_phy_hv(HV_OEM_BITS_PAGE, HV_OEM_BITS_REG, HV_OEM_BITS_LPLU);
    hw.poke(E1000E_FEXTNVM, FEXTNVM_SW_CONFIG_ICH8M);
    unsafe { dev.oem_bits_config(true) };
    assert!(
        hw.phy_writes().is_empty(),
        "una discreta no tiene HV_OEM_BITS: {:?}",
        hw.phy_writes()
    );
}

// ---------------------------------------------------------------------------
// Autonegociacion
// ---------------------------------------------------------------------------

#[test]
fn en_un_pch_la_autonegociacion_se_reinicia_en_la_direccion_dos() {
    let (hw, dev) = tarjeta(I219_SPT);
    // En un PCH los registros IEEE viven en la direccion 2; la 1 es el bloque
    // de wakeup, y "contesta" igual. Si el driver la prueba primero, el
    // reinicio no llega nunca al PHY de verdad.
    phy_sano(&hw, 2);
    hw.set_mii(1, MII_BMCR, 0);
    unsafe { dev.restart_autoneg() };
    let bmcr = hw.mii(2, MII_BMCR).unwrap();
    assert_ne!(bmcr & MII_CR_AUTO_NEG_EN, 0);
    assert_ne!(
        bmcr & MII_CR_RESTART_AUTO_NEG,
        0,
        "no reinicio la autonegociacion en la direccion 2"
    );
    assert_eq!(
        hw.mii(1, MII_BMCR),
        Some(0),
        "toco el bloque de wakeup de la direccion 1"
    );
}

#[test]
fn en_una_discreta_la_autonegociacion_se_reinicia_en_la_direccion_uno() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    unsafe { dev.restart_autoneg() };
    assert_ne!(
        hw.mii(1, MII_BMCR).unwrap() & MII_CR_RESTART_AUTO_NEG,
        0,
        "no reinicio la autonegociacion en la direccion 1"
    );
}

#[test]
fn una_direccion_que_contesta_todo_unos_no_es_un_phy() {
    let (hw, dev) = tarjeta(DISCRETA);
    // Sin sembrar nada, el falso contesta 0xFFFF: hay bus pero no hay PHY.
    hw.set_mii(1, MII_BMCR, 0xFFFF);
    hw.set_mii(2, MII_BMCR, 0xFFFF);
    unsafe { dev.restart_autoneg() };
    assert!(
        hw.phy_writes().is_empty(),
        "escribio en un PHY que no existe: {:?}",
        hw.phy_writes()
    );
}

#[test]
fn la_advertencia_se_ensancha_con_los_dos_bits_de_pause() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    hw.set_mii(1, MII_ADVERTISE, ADVERTISE_100FULL);
    unsafe { dev.widen_autoneg_advertisement(1) };
    let adv = hw.mii(1, MII_ADVERTISE).unwrap();
    assert_eq!(
        adv & (ADVERTISE_PAUSE_CAP | ADVERTISE_PAUSE_ASYM),
        ADVERTISE_PAUSE_CAP | ADVERTISE_PAUSE_ASYM,
        "sin los bits de PAUSE las marcas de agua son decoracion"
    );
    assert_eq!(adv & ADVERTISE_ALL_10_100, ADVERTISE_ALL_10_100);
}

#[test]
fn el_gigabit_se_advierte_cuando_el_estatus_extendido_lo_soporta() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    hw.set_mii(1, MII_CTRL1000, 0);
    unsafe { dev.widen_autoneg_advertisement(1) };
    assert_eq!(
        hw.mii(1, MII_CTRL1000),
        Some(ADVERTISE_1000FULL),
        "no advirtio el gigabit que el PHY dice soportar"
    );
}

#[test]
fn sin_estatus_extendido_el_gigabit_no_se_advierte() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    // BMSR sin ESTATEN: el registro extendido no existe, asi que CTRL1000
    // tampoco se toca.
    hw.set_mii(1, MII_BMSR, BMSR_LSTATUS | BMSR_ANEGCOMPLETE);
    hw.set_mii(1, MII_CTRL1000, 0);
    unsafe { dev.widen_autoneg_advertisement(1) };
    assert_eq!(hw.mii(1, MII_CTRL1000), Some(0));
}

#[test]
fn un_phy_que_solo_soporta_gigabit_a_medio_duplex_no_lo_advierte() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    hw.set_mii(1, MII_ESTATUS, ESTATUS_1000_THALF);
    hw.set_mii(1, MII_CTRL1000, 0);
    unsafe { dev.widen_autoneg_advertisement(1) };
    assert_eq!(
        hw.mii(1, MII_CTRL1000),
        Some(0),
        "advirtio gigabit a medio duplex, que solo puede resolver un enlace peor"
    );
}

// ---------------------------------------------------------------------------
// Lo que el PHY dice del enlace, y el control de flujo
// ---------------------------------------------------------------------------

#[test]
fn el_enlace_del_phy_se_lee_con_el_semaforo_y_se_suelta() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    let link = unsafe { dev.read_phy_link() }.expect("el PHY contesta");
    assert_eq!(link.negotiated, Some((1000, true)));
    assert_ne!(link.lpa & LPA_PAUSE_CAP, 0);
    assert_eq!(
        hw.peek(E1000E_EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG,
        0,
        "dejo el semaforo cogido"
    );
}

#[test]
fn sin_phy_al_otro_lado_no_hay_lectura_de_enlace() {
    let (hw, dev) = tarjeta(DISCRETA);
    hw.faults(|f| f.mdic_mute.extend([1, 2]));
    assert!(unsafe { dev.read_phy_link() }.is_none());
}

#[test]
fn un_bus_mdio_vacio_tampoco_da_una_lectura_de_enlace() {
    let (hw, dev) = tarjeta(DISCRETA);
    // El bus contesta, pero con todo unos: hay MDIO y no hay PHY. Creerselo
    // seria leer un enlace negociado de un registro que no existe.
    hw.set_mii(1, MII_BMSR, 0xFFFF);
    hw.set_mii(2, MII_BMSR, 0xFFFF);
    assert!(unsafe { dev.read_phy_link() }.is_none());
}

#[test]
fn una_eeprom_con_otra_codificacion_se_lee_al_segundo_intento() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    // Algunas discretas codifican el offset de EERD desplazado 3, no 2. La
    // palabra 1 es la que lo delata: desplazada 2 cae en una direccion que esa
    // EEPROM no contesta, asi que solo sale si el driver reintenta con el otro
    // desplazamiento.
    hw.set_eeprom(3, &[(1, 0x1200)]);
    dev.mac = [0; 6];
    unsafe { dev.read_mac_from_hw() };
    assert_eq!(
        &dev.mac[2..4],
        &[0x00, 0x12],
        "sin el segundo desplazamiento la palabra 1 no se lee: {:02x?}",
        dev.mac
    );
}

#[test]
fn el_control_de_flujo_sale_de_lo_que_las_dos_partes_advirtieron() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    hw.poke(E1000E_CTRL, CTRL_SLU);
    unsafe { dev.on_link_up(STATUS_LU | STATUS_FD | (2 << STATUS_SPEED_SHIFT)) };
    let ctrl = hw.peek(E1000E_CTRL);
    assert_eq!(
        ctrl & (CTRL_RFCE | CTRL_TFCE),
        CTRL_RFCE | CTRL_TFCE,
        "las dos partes advirtieron PAUSE simetrico y no se habilito"
    );
    assert_ne!(ctrl & CTRL_SLU, 0, "se llevo por delante el resto de CTRL");
}

#[test]
fn si_el_phy_no_contesta_el_control_de_flujo_se_deja_como_estaba() {
    let (hw, dev) = tarjeta(DISCRETA);
    hw.faults(|f| f.mdic_mute.extend([1, 2]));
    hw.poke(E1000E_CTRL, CTRL_SLU);
    unsafe { dev.on_link_up(STATUS_LU | STATUS_FD | (2 << STATUS_SPEED_SHIFT)) };
    assert_eq!(
        hw.peek(E1000E_CTRL),
        CTRL_SLU,
        "habilito PAUSE contra un socio que nunca lo acepto"
    );
}

#[test]
fn a_medio_duplex_no_hay_control_de_flujo() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    hw.poke(E1000E_CTRL, CTRL_RFCE | CTRL_TFCE);
    unsafe { dev.on_link_up(STATUS_LU | (2 << STATUS_SPEED_SHIFT)) };
    assert_eq!(
        hw.peek(E1000E_CTRL) & (CTRL_RFCE | CTRL_TFCE),
        0,
        "dejo PAUSE habilitado en un enlace a medio duplex"
    );
}

#[test]
fn el_gap_entre_paquetes_se_retoca_en_cada_cambio_de_enlace() {
    let (hw, dev) = tarjeta(I217);
    phy_sano(&hw, 2);
    hw.poke(E1000E_TIPG, 8 | (8 << 10) | (6 << 20));
    hw.poke(E1000E_FEXTNVM4, 0x0000_0001);
    // 10 Mb/s medio duplex: el gap tiene que ensancharse.
    unsafe { dev.on_link_up(STATUS_LU) };
    assert_eq!(
        hw.peek(E1000E_TIPG),
        tipg_for_link(8 | (8 << 10) | (6 << 20), false, 10, false)
    );
    assert_eq!(
        hw.peek(E1000E_FEXTNVM4) & FEXTNVM4_BEACON_DURATION_MASK,
        FEXTNVM4_BEACON_DURATION_8USEC,
        "no puso la duracion de beacon que pide la errata del I217"
    );
}

#[test]
fn el_gap_entre_paquetes_no_se_toca_en_una_discreta() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    hw.poke(E1000E_TIPG, 0x1234);
    unsafe { dev.on_link_up(STATUS_LU) };
    assert_eq!(hw.peek(E1000E_TIPG), 0x1234);
}

#[test]
fn las_marcas_de_agua_se_programan_y_el_pause_llega_al_phy_del_pch() {
    let (hw, dev) = tarjeta(I219_SPT);
    phy_sano(&hw, 2);
    unsafe { dev.setup_flow_control() };
    let fc = fc_settings(true, MAX_FRAME_SIZE);
    assert_eq!(hw.peek(E1000E_PBA), fc.pba_kb);
    assert_eq!(hw.peek(E1000E_FCTTV), fc.pause_time);
    assert_eq!(hw.peek(E1000E_FCRTH), fc.high_water);
    assert_eq!(hw.peek(E1000E_FCRTL), fcrtl_value(fc.low_water, true));
    assert_eq!(hw.peek(E1000E_FCRTV_PCH), fc.refresh_time);
    assert_eq!(
        hw.phy_hv(BM_PORT_CTRL_PAGE, BM_PORT_PAUSE_TIME_REG),
        Some(fc.pause_time as u16),
        "el PHY del PCH se quedo sin su copia del tiempo de pausa"
    );
}

#[test]
fn una_discreta_no_tiene_ni_refresco_ni_pause_en_el_phy() {
    let (hw, dev) = tarjeta(DISCRETA);
    phy_sano(&hw, 1);
    // Un centinela: el refresco que le toca a una discreta es 0, asi que sin
    // esto escribir el registro y no escribirlo se ven igual.
    hw.poke(E1000E_FCRTV_PCH, 0xDEAD);
    unsafe { dev.setup_flow_control() };
    assert_eq!(
        hw.peek(E1000E_FCRTV_PCH),
        0xDEAD,
        "escribio un registro que esa tarjeta no tiene"
    );
    assert!(hw.phy_writes().is_empty());
}

// ---------------------------------------------------------------------------
// La direccion MAC
// ---------------------------------------------------------------------------

#[test]
fn el_mac_sale_de_ral_y_rah_cuando_el_hardware_los_trae() {
    let (hw, mut dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_RAL0, 0x5644_3322);
    hw.poke(E1000E_RAH0, 0x8000_0088);
    unsafe { dev.read_mac_from_hw() };
    assert_eq!(dev.mac, [0x22, 0x33, 0x44, 0x56, 0x88, 0x00]);
}

#[test]
fn sin_ral_ni_rah_una_discreta_lo_lee_de_la_eeprom() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    hw.set_eeprom(2, &[(0, 0x5452), (1, 0x1200), (2, 0x3456)]);
    unsafe { dev.read_mac_from_hw() };
    assert_eq!(dev.mac, [0x52, 0x54, 0x00, 0x12, 0x56, 0x34]);
    assert!(dev.is_valid_mac());
}

#[test]
fn una_eeprom_que_no_contesta_deja_el_mac_sin_valor() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    hw.faults(|f| f.eerd_dead = true);
    dev.mac = [0; 6];
    unsafe { dev.read_mac_from_hw() };
    assert_eq!(dev.mac, [0; 6]);
    assert!(
        !dev.is_valid_mac(),
        "un MAC a ceros no es un MAC: el llamante tiene que poner el de reserva"
    );
}

#[test]
fn un_pch_no_prueba_la_eeprom_que_no_tiene() {
    let (hw, mut dev) = tarjeta(I217);
    // En silicio ICH/PCH el offset de EERD es FEXTNVM5, un registro de
    // workaround: "leer la EEPROM" ahi es escribir basura en el.
    hw.set_eeprom(2, &[(0, 0xBEEF), (1, 0xBEEF), (2, 0xBEEF)]);
    dev.mac = [0; 6];
    unsafe { dev.read_mac_from_hw() };
    assert_eq!(dev.mac, [0; 6], "se creyo una EEPROM que no existe");
}

// ---------------------------------------------------------------------------
// Los anillos
// ---------------------------------------------------------------------------

#[test]
fn init_tx_programa_el_anillo_y_enciende_la_transmision() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    unsafe { dev.init_tx() };
    let pa = dev.tx_ring.paddr();
    assert_eq!(hw.peek(E1000E_TDBAL), pa as u32);
    assert_eq!(hw.peek(E1000E_TDBAH), (pa >> 32) as u32);
    assert_eq!(
        hw.peek(E1000E_TDLEN),
        (NUM_TX * core::mem::size_of::<TxDesc>()) as u32
    );
    assert_eq!(hw.peek(E1000E_TDH), 0);
    assert_eq!(hw.peek(E1000E_TDT), 0);
    assert_ne!(
        hw.peek(E1000E_TCTL) & TCTL_EN,
        0,
        "la transmision sigue apagada"
    );
    assert_ne!(
        hw.peek(E1000E_TARC0) & 1,
        0,
        "sin el bit de arbitraje el 82574 transmite mal"
    );
    // Cada hueco empieza libre: un descriptor sin DD parece propiedad del
    // hardware para siempre y la cola nace muerta.
    let ring = dev.tx_ring.as_ptr::<TxDesc>();
    for i in 0..NUM_TX {
        let d = unsafe { &*ring.add(i) };
        assert_eq!(d.status & TX_STAT_DD, TX_STAT_DD, "hueco {i} sin DD");
        assert_eq!(d.addr, dev.tx_buf_paddr(i));
    }
}

#[test]
fn en_sunrise_point_la_cola_de_transmision_se_habilita_y_se_espeja() {
    let (hw, mut dev) = tarjeta(I219_SPT);
    unsafe { dev.init_tx() };
    assert_ne!(
        hw.peek(E1000E_TXDCTL) & TXDCTL_QUEUE_ENABLE,
        0,
        "la cola no quedo habilitada"
    );
    assert_eq!(
        hw.peek(E1000E_TXDCTL1),
        hw.peek(E1000E_TXDCTL),
        "la cola 1 no se espejo"
    );
    assert_ne!(
        hw.peek(E1000E_IOSFPC) & 0x0001_0000,
        0,
        "sin el bit de IOSF el PCIe no cumple"
    );
}

#[test]
fn una_cola_de_transmision_que_no_latchea_no_cuelga_el_arranque() {
    let (hw, mut dev) = tarjeta(I219_SPT);
    hw.faults(|f| f.queue_enable_stuck = true);
    unsafe { dev.init_tx() };
    assert_eq!(hw.peek(E1000E_TXDCTL) & TXDCTL_QUEUE_ENABLE, 0);
    // Y aun asi acaba de configurar: el aviso es lo unico que distingue una
    // cola muerta de una sana pero parada.
    assert_ne!(hw.peek(E1000E_TCTL) & TCTL_EN, 0);
}

#[test]
fn una_discreta_no_habilita_colas_sino_que_pide_escritura_completa() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    unsafe { dev.init_tx() };
    assert_eq!(
        hw.peek(E1000E_TXDCTL),
        TXDCTL_DMA_BURST | TXDCTL_FULL_TX_DESC_WB
    );
    assert_eq!(hw.peek(E1000E_TXDCTL1), 0, "espejo una cola que no existe");
}

#[test]
fn init_rx_programa_el_anillo_y_da_los_descriptores_al_hardware() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    unsafe { dev.init_rx() };
    let pa = dev.rx_ring.paddr();
    assert_eq!(hw.peek(E1000E_RDBAL), pa as u32);
    assert_eq!(
        hw.peek(E1000E_RDLEN),
        (NUM_RX * core::mem::size_of::<RxDesc>()) as u32
    );
    assert_eq!(hw.peek(E1000E_RDH), 0);
    assert_eq!(
        hw.peek(E1000E_RDT),
        (NUM_RX - 1) as u32,
        "no dio los descriptores al hardware: el anillo nace vacio"
    );
    assert_eq!(hw.peek(E1000E_ITR), E1000E_ITR_BALANCED);
    let rctl = hw.peek(E1000E_RCTL);
    assert_eq!(
        rctl & (RCTL_EN | RCTL_BAM | RCTL_SECRC),
        RCTL_EN | RCTL_BAM | RCTL_SECRC
    );
    // El anillo arranca en promiscuo + allmulti para que el DHCP del arranque
    // y los puentes funcionen.
    assert_eq!(rctl & (RCTL_UPE | RCTL_MPE), RCTL_UPE | RCTL_MPE);
    let rfctl = hw.peek(E1000E_RFCTL);
    assert_eq!(
        rfctl & RFCTL_EXTEN,
        0,
        "pidio escritura extendida, que el camino de recepcion no sabe leer"
    );
    assert_eq!(
        rfctl & (RFCTL_NFSW_DIS | RFCTL_NFSR_DIS),
        RFCTL_NFSW_DIS | RFCTL_NFSR_DIS
    );
    let ring = dev.rx_ring.as_ptr::<RxDesc>();
    for i in 0..NUM_RX {
        let d = unsafe { &*ring.add(i) };
        assert_eq!(d.addr, dev.rx_buf_paddr(i), "hueco {i} sin su buffer");
        assert_eq!(d.status, 0);
    }
}

#[test]
fn el_rxcsum_que_dejo_la_firmware_se_limpia() {
    let (hw, mut dev) = tarjeta(I219_SPT);
    hw.poke(E1000E_RXCSUM, 0x0000_0300);
    unsafe { dev.init_rx() };
    assert_eq!(
        hw.peek(E1000E_RXCSUM),
        0,
        "un offload heredado da veredictos que el verificador de software no comparte"
    );
}

#[test]
fn sin_promiscuo_ni_allmulti_el_rctl_no_los_pide() {
    let (_hw, mut dev) = tarjeta(DISCRETA);
    dev.rx_promisc = false;
    dev.rx_allmulti = false;
    unsafe { dev.init_rx() };
    let rctl = unsafe { mmio_read(dev.base, E1000E_RCTL) };
    assert_eq!(rctl & (RCTL_UPE | RCTL_MPE), 0);
    assert_ne!(rctl & RCTL_EN, 0);
}

#[test]
fn en_un_pch_el_srrctl_pide_dos_kb_y_descarta_cuando_se_llena() {
    let (hw, mut dev) = tarjeta(I219_SPT);
    unsafe { dev.init_rx() };
    assert_eq!(hw.peek(E1000E_SRRCTL), 2 | (1 << 31));
    assert_ne!(
        hw.peek(E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE,
        0,
        "la cola de recepcion no quedo habilitada"
    );
}

#[test]
fn una_discreta_no_tiene_srrctl() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    unsafe { dev.init_rx() };
    assert_eq!(hw.peek(E1000E_SRRCTL), 0);
    assert_eq!(hw.peek(E1000E_MRQC), 0);
}

// ---------------------------------------------------------------------------
// El watchdog
// ---------------------------------------------------------------------------

#[test]
fn el_watchdog_ve_subir_el_enlace_y_hace_la_transicion() {
    let (hw, mut dev) = tarjeta(I217);
    phy_sano(&hw, 2);
    dev.link_up = false;
    hw.poke(E1000E_TIPG, 8 | (8 << 10) | (6 << 20));
    hw.poke(
        E1000E_STATUS,
        STATUS_LU | STATUS_FD | (1 << STATUS_SPEED_SHIFT),
    );

    assert!(unsafe { dev.watchdog_tick() }, "no vio el cambio");
    assert!(dev.link_up);
    assert_eq!(
        hw.peek(E1000E_CTRL) & (CTRL_RFCE | CTRL_TFCE),
        CTRL_RFCE | CTRL_TFCE,
        "la subida no paso por on_link_up: el control de flujo quedo sin negociar"
    );
}

#[test]
fn el_watchdog_ve_bajar_el_enlace() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    dev.link_up = true;
    hw.poke(E1000E_STATUS, 0);
    assert!(unsafe { dev.watchdog_tick() });
    assert!(!dev.link_up);
}

#[test]
fn sin_cambio_de_enlace_el_watchdog_no_dice_que_hubo_uno() {
    let (hw, mut dev) = tarjeta(DISCRETA);
    dev.link_up = true;
    hw.poke(E1000E_STATUS, STATUS_LU | STATUS_FD);
    assert!(
        !unsafe { dev.watchdog_tick() },
        "un enlace que sigue igual no es un cambio"
    );
}

#[test]
fn el_camino_de_recepcion_arma_el_enlace_que_el_watchdog_todavia_no_vio() {
    let (hw, mut dev) = tarjeta(I217);
    phy_sano(&hw, 2);
    dev.link_up = false;
    hw.poke(
        E1000E_STATUS,
        STATUS_LU | STATUS_FD | (2 << STATUS_SPEED_SHIFT),
    );
    assert!(dev.link_up_refreshed());
    assert_eq!(
        hw.peek(E1000E_CTRL) & (CTRL_RFCE | CTRL_TFCE),
        CTRL_RFCE | CTRL_TFCE,
        "gano la carrera al watchdog y no hizo la transicion, que es como quedo muerto el codigo de la subida"
    );
}

#[test]
fn con_el_enlace_ya_arriba_no_se_repite_la_transicion() {
    let (hw, mut dev) = tarjeta(I217);
    phy_sano(&hw, 2);
    dev.link_up = true;
    hw.poke(E1000E_CTRL, CTRL_SLU);
    hw.poke(
        E1000E_STATUS,
        STATUS_LU | STATUS_FD | (2 << STATUS_SPEED_SHIFT),
    );
    assert!(dev.link_up_refreshed());
    // Esto corre en cada poll y en cada transmision: con el enlace ya arriba
    // no hay nada que hacer, asi que ni se mira el STATUS ni se rehace la
    // negociacion del control de flujo.
    assert_eq!(
        hw.peek(E1000E_CTRL),
        CTRL_SLU,
        "rehizo la transicion de subida con el enlace ya arriba"
    );
}
