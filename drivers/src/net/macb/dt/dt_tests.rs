//! Lo que el nodo del RP1 dice y lo que no, sobre nodos armados a mano como
//! los de `rp1.dtsi`.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::super::dt;
use crate::utils::devicetree::Node;

fn cells(v: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in v {
        out.extend_from_slice(&c.to_be_bytes());
    }
    out
}

fn strs(v: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in v {
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    }
    out
}

fn prop(name: &str, value: Vec<u8>) -> (String, Vec<u8>) {
    (name.to_string(), value)
}

fn nodo(name: &str, props: Vec<(String, Vec<u8>)>) -> Node {
    Node {
        name: name.to_string(),
        props,
        children: vec![],
    }
}

/// El nodo del GEM de la Pi 5, copiado de `rp1.dtsi`.
///
/// Las dos propiedades que importan son las que engañan: la
/// `local-mac-address` de seis ceros y el `phy-mode` que no se programa.
fn gem_del_rp1() -> Node {
    nodo(
        "ethernet@100000",
        vec![
            prop("compatible", strs(&["raspberrypi,rp1-gem", "cdns,macb"])),
            // reg = <0xc0 0x40100000  0x0 0x4000>, en el padre de dos celdas
            // de direccion y dos de tamaño.
            prop("reg", cells(&[0xc0, 0x4010_0000, 0x0, 0x4000])),
            prop("phy-mode", strs(&["rgmii-id"])),
            prop("local-mac-address", vec![0, 0, 0, 0, 0, 0]),
            prop("clock-names", strs(&["pclk", "hclk", "tsu_clk", "tx_clk"])),
            prop("status", strs(&["disabled"])),
        ],
    )
}

// ---------------------------------------------------------------------------
// La MAC
// ---------------------------------------------------------------------------

/// **El caso normal de la Pi 5**: el arbol trae la propiedad y son seis ceros,
/// porque la MAC de verdad la pone el firmware en los registros.
///
/// Creerse esos ceros deja la tarjeta con la MAC `00:00:00:00:00:00`, y una
/// trama con esa MAC de origen la tira el primer conmutador.
#[test]
fn the_six_zeroes_the_rp1_tree_carries_are_not_a_mac() {
    assert_eq!(dt::mac_addr(&gem_del_rp1()), None);
}

/// Una MAC de verdad en el arbol si se coge: es lo que pasa en una placa cuyo
/// firmware escriba el arbol en vez de los registros.
#[test]
fn a_real_address_in_the_tree_is_taken() {
    let mut node = gem_del_rp1();
    node.props.retain(|(name, _)| name != "local-mac-address");
    node.props.push(prop(
        "local-mac-address",
        vec![0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56],
    ));
    assert_eq!(
        dt::mac_addr(&node),
        Some([0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56])
    );
}

/// `mac-address` manda sobre `local-mac-address`, que es el orden de Linux: la
/// primera es la que pudo cambiar en caliente, la segunda la de fabrica.
#[test]
fn the_hot_address_wins_over_the_factory_one() {
    let mut node = gem_del_rp1();
    node.props.insert(
        0,
        prop("mac-address", vec![0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
    );
    node.props.push(prop(
        "local-mac-address",
        vec![0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56],
    ));
    assert_eq!(
        dt::mac_addr(&node),
        Some([0x02, 0x11, 0x22, 0x33, 0x44, 0x55])
    );
}

/// Una propiedad que no mide seis bytes **no es una MAC**, y se pasa a la
/// siguiente en vez de leer seis bytes de donde solo hay cuatro.
#[test]
fn a_property_that_is_not_six_bytes_long_is_not_a_mac() {
    let mut node = gem_del_rp1();
    node.props.retain(|(name, _)| name != "local-mac-address");
    node.props
        .push(prop("mac-address", vec![0x2c, 0xcf, 0x67, 0x12]));
    node.props.push(prop(
        "local-mac-address",
        vec![0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56],
    ));
    assert_eq!(
        dt::mac_addr(&node),
        Some([0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56]),
        "una propiedad corta tapo la buena"
    );
}

/// Una MAC de grupo tampoco sirve como direccion propia de la tarjeta: el bit
/// de multicast puesto en una direccion de origen es invalido.
#[test]
fn a_group_address_is_not_the_cards_own() {
    let mut node = gem_del_rp1();
    node.props.retain(|(name, _)| name != "local-mac-address");
    node.props.push(prop(
        "local-mac-address",
        vec![0x01, 0x00, 0x5e, 0x00, 0x00, 0x01],
    ));
    assert_eq!(dt::mac_addr(&node), None);
}

/// Y un nodo sin ninguna de las dos propiedades no inventa nada.
#[test]
fn a_node_with_no_address_property_says_so() {
    let mut node = gem_del_rp1();
    node.props.retain(|(name, _)| name != "local-mac-address");
    assert_eq!(dt::mac_addr(&node), None);
}

// ---------------------------------------------------------------------------
// El phy-mode
// ---------------------------------------------------------------------------

/// El `rgmii-id` del nodo del RP1 se acepta. **No se programa en ningun
/// sitio**: el GEM no tiene el bit del retardo que tiene el GENET, y en Linux
/// el `phy-mode` del macb va al PHY. Lo unico que dice es que la placa es la
/// que este driver sabe manejar.
#[test]
fn the_rgmii_mode_the_rp1_declares_is_accepted() {
    assert!(dt::phy_mode_is_supported(&gem_del_rp1()));
}

/// Las cuatro variantes de RGMII valen igual, porque ninguna cambia nada aqui.
#[test]
fn all_four_rgmii_variants_are_the_same_to_this_driver() {
    for modo in ["rgmii", "rgmii-id", "rgmii-rxid", "rgmii-txid"] {
        let mut node = gem_del_rp1();
        node.props.retain(|(name, _)| name != "phy-mode");
        node.props.push(prop("phy-mode", strs(&[modo])));
        assert!(dt::phy_mode_is_supported(&node), "{} se rechazo", modo);
    }
}

/// Un modo que no es RGMII **si** se rechaza: seria otra placa, y el driver
/// estaria programando el MAC como si fuera el GEM del RP1.
#[test]
fn a_mode_that_is_not_rgmii_is_refused_rather_than_driven_as_if_it_were() {
    for modo in ["sgmii", "mii", "rmii", "10gbase-r"] {
        let mut node = gem_del_rp1();
        node.props.retain(|(name, _)| name != "phy-mode");
        node.props.push(prop("phy-mode", strs(&[modo])));
        assert!(!dt::phy_mode_is_supported(&node), "{} se acepto", modo);
    }
}

/// Y un nodo sin `phy-mode` se acepta: la propiedad es opcional y no cambia
/// ningun registro, asi que su ausencia no es motivo para dejar una placa sin
/// red.
#[test]
fn a_node_with_no_phy_mode_is_not_refused_for_it() {
    let mut node = gem_del_rp1();
    node.props.retain(|(name, _)| name != "phy-mode");
    assert!(dt::phy_mode_is_supported(&node));
}

// ---------------------------------------------------------------------------
// El reloj
// ---------------------------------------------------------------------------

/// El nodo del RP1 **no trae la frecuencia del `pclk`**: trae un phandle a un
/// proveedor de relojes que Eclipse no sabe leer. Asi que se queda el valor
/// por defecto, que son los 200 MHz del `clk_sys` con los que arranca la
/// placa.
#[test]
fn the_rp1_node_carries_no_clock_rate_so_the_default_stands() {
    let cfg = dt::config(&gem_del_rp1());
    assert_eq!(cfg.pclk_hz, 200_000_000);
    assert!(!cfg.promiscuous);
}

/// Si alguna vez hay un `clock-frequency`, se usa: es la propiedad estandar
/// para decir una frecuencia, y el divisor del MDC depende de ella.
#[test]
fn a_clock_frequency_in_the_node_is_used() {
    let mut node = gem_del_rp1();
    node.props
        .push(prop("clock-frequency", cells(&[125_000_000])));
    assert_eq!(dt::config(&node).pclk_hz, 125_000_000);
}

/// Pero un `clock-frequency` absurdo se descarta en vez de pasarse adelante.
///
/// El divisor del MDC se elige por tramos: un `pclk` que no llegue al primero
/// coge el divisor mas pequeño, que a esas frecuencias sigue dando un MDC por
/// encima de los 2,5 MHz que el bus admite. Y un cero daria una division por
/// un reloj que no existe.
#[test]
fn a_clock_rate_that_cannot_be_a_pclk_is_discarded() {
    for hz in [0u32, 1, 999_999] {
        let mut node = gem_del_rp1();
        node.props.push(prop("clock-frequency", cells(&[hz])));
        assert_eq!(
            dt::config(&node).pclk_hz,
            200_000_000,
            "{} Hz paso por un pclk",
            hz
        );
    }
}

// ---------------------------------------------------------------------------
// Las ausencias
// ---------------------------------------------------------------------------

/// **El nodo no dice donde esta el PHY**, y eso no es un arbol incompleto.
///
/// En la Pi 4 el PHY esta en un nieto (`mdio@e14/ethernet-phy@1`) con su
/// `reg`, y el driver lo lee. Aqui no hay ni `phy-handle` ni nodo `mdio`: el
/// barrido del bus MDIO **no es el camino de respaldo, es el unico camino**.
/// Se fija aqui porque es la clase de cosa que alguien "arregla" añadiendo una
/// lectura de `reg` del nodo del MAC --- que en este nodo es `0xc0`, una
/// direccion de bus valida, y entonces el driver le habla a la direccion 0 sin
/// decirlo.
#[test]
fn the_node_says_nothing_about_where_the_phy_is() {
    let node = gem_del_rp1();
    assert!(
        !node.props.iter().any(|(name, _)| name == "phy-handle"),
        "el nodo del RP1 no trae phy-handle"
    );
    assert!(
        node.children.is_empty(),
        "el nodo del RP1 no tiene hijos, y menos un bus mdio"
    );
}
