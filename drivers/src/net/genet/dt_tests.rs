//! Las tres propiedades del device tree, sobre nodos armados a mano como los
//! del `bcm2711-rpi-4-b.dts`.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::dt;
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

fn nodo(name: &str, props: Vec<(String, Vec<u8>)>, children: Vec<Node>) -> Node {
    Node {
        name: name.to_string(),
        props,
        children,
    }
}

/// El nodo del GENET de la Pi 4, con su bus MDIO y su PHY colgando.
fn genet_de_la_pi(addr: u32) -> Node {
    nodo(
        "ethernet@7d580000",
        vec![
            prop("compatible", strs(&["brcm,bcm2711-genet-v5"])),
            // La ventana de registros, como la trae el arbol de la Pi: dos
            // celdas. Esta aqui a proposito, porque es la propiedad que un
            // driver que buscara el `reg` en el nodo del MAC se llevaria como
            // direccion MDIO --y truncada a un byte, la 0x7d580000 es la
            // direccion 0, que es una direccion valida del bus.
            prop("reg", cells(&[0x7d58_0000, 0x1_0000])),
            prop("phy-mode", strs(&["rgmii-rxid"])),
            prop(
                "local-mac-address",
                vec![0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33],
            ),
        ],
        vec![nodo(
            "mdio@e14",
            vec![prop("compatible", strs(&["brcm,genet-mdio-v5"]))],
            vec![nodo(
                "ethernet-phy@1",
                vec![prop("reg", cells(&[addr]))],
                vec![],
            )],
        )],
    )
}

// ---------------------------------------------------------------------------
// La direccion del PHY
// ---------------------------------------------------------------------------

#[test]
fn the_phy_address_comes_from_the_grandchild_not_from_the_mac_node() {
    // El PHY no esta en el nodo del MAC: cuelga de un nodo `mdio` que cuelga
    // del MAC. Un driver que buscara un `reg` en el nodo del MAC se llevaria
    // la direccion de la ventana de registros como direccion MDIO.
    assert_eq!(dt::phy_addr(&genet_de_la_pi(1)), Some(1));
    assert_eq!(dt::phy_addr(&genet_de_la_pi(7)), Some(7));
}

#[test]
fn a_tree_that_declares_no_phy_leaves_the_bus_to_be_scanned() {
    // `None` es «el arbol no lo dice», y entonces el driver barre el bus, que
    // es lo que hace U-Boot. Inventar la direccion 0 seria hablarle a una
    // direccion que puede tener otro PHY.
    let solo = nodo("ethernet@7d580000", vec![], vec![]);
    assert_eq!(dt::phy_addr(&solo), None);

    // Un bus MDIO sin PHY tampoco da direccion.
    let vacio = nodo(
        "ethernet@7d580000",
        vec![],
        vec![nodo("mdio@e14", vec![], vec![])],
    );
    assert_eq!(dt::phy_addr(&vacio), None);
}

#[test]
fn an_address_that_does_not_fit_the_bus_is_not_an_address() {
    // La direccion son CINCO bits en el `UMAC_MDIO_CMD`. Pasar un 32 adelante
    // es hablarle a la direccion 0 sin decirlo, y ahi puede haber otro PHY.
    let malo = nodo(
        "ethernet@7d580000",
        vec![],
        vec![nodo(
            "mdio@e14",
            vec![],
            vec![nodo(
                "ethernet-phy@20",
                vec![prop("reg", cells(&[32]))],
                vec![],
            )],
        )],
    );
    assert_eq!(dt::phy_addr(&malo), None);
    // Y la ultima que si cabe se acepta.
    assert_eq!(dt::phy_addr(&genet_de_la_pi(31)), Some(31));
}

#[test]
fn a_child_that_is_not_a_phy_is_not_read_as_one() {
    // Del nodo `mdio` pueden colgar otras cosas; solo un `ethernet-phy` lleva
    // una direccion de este bus.
    let otro = nodo(
        "ethernet@7d580000",
        vec![],
        vec![nodo(
            "mdio@e14",
            vec![],
            vec![
                nodo("fixed-link", vec![prop("reg", cells(&[3]))], vec![]),
                nodo("ethernet-phy@1", vec![prop("reg", cells(&[1]))], vec![]),
            ],
        )],
    );
    assert_eq!(dt::phy_addr(&otro), Some(1));
}

#[test]
fn the_mdio_node_is_recognised_by_what_it_declares_and_by_its_name() {
    // El arbol de la Pi lo llama `mdio@e14` y declara `brcm,genet-mdio-v5`.
    // Con cualquiera de las dos cosas tiene que valer: otros arboles de la
    // familia le ponen otro offset al nombre.
    let por_nombre = nodo(
        "ethernet@7d580000",
        vec![],
        vec![nodo(
            "mdio@1234",
            vec![],
            vec![nodo(
                "ethernet-phy@1",
                vec![prop("reg", cells(&[1]))],
                vec![],
            )],
        )],
    );
    assert_eq!(dt::phy_addr(&por_nombre), Some(1));

    let por_compatible = nodo(
        "ethernet@7d580000",
        vec![],
        vec![nodo(
            "otra-cosa",
            vec![prop("compatible", strs(&["brcm,genet-mdio-v5"]))],
            vec![nodo(
                "ethernet-phy@1",
                vec![prop("reg", cells(&[9]))],
                vec![],
            )],
        )],
    );
    assert_eq!(dt::phy_addr(&por_compatible), Some(9));
}

// ---------------------------------------------------------------------------
// El retardo del RGMII
// ---------------------------------------------------------------------------

#[test]
fn the_rgmii_delay_table_is_the_published_one() {
    // Equivocar esto no da un error: da un enlace que sube y pierde tramas a
    // gigabit, porque los datos llegan fuera de la ventana del reloj.
    for (modo, espera) in [
        ("rgmii", true),
        ("rgmii-rxid", true),
        ("rgmii-id", false),
        ("rgmii-txid", false),
    ] {
        let n = nodo(
            "ethernet@7d580000",
            vec![prop("phy-mode", strs(&[modo]))],
            vec![],
        );
        assert_eq!(dt::id_mode_dis(&n), espera, "phy-mode {}", modo);
    }
}

#[test]
fn a_tree_with_no_phy_mode_gets_what_the_pi_four_needs() {
    // Este driver solo arranca en un BCM2711, y ahi el modo es `rgmii-rxid`.
    let n = nodo("ethernet@7d580000", vec![], vec![]);
    assert!(dt::id_mode_dis(&n));
    // Y un modo que no es RGMII tampoco deja el MAC poniendo un retardo que
    // la placa ya pone: el GENET de la Pi es RGMII y nada mas.
    let raro = nodo(
        "ethernet@7d580000",
        vec![prop("phy-mode", strs(&["sgmii"]))],
        vec![],
    );
    assert!(dt::id_mode_dis(&raro));
}

// ---------------------------------------------------------------------------
// La MAC de la placa
// ---------------------------------------------------------------------------

#[test]
fn the_board_mac_comes_out_of_the_tree() {
    assert_eq!(
        dt::mac_addr(&genet_de_la_pi(1)),
        Some([0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33])
    );
}

#[test]
fn a_hot_changed_mac_wins_over_the_factory_one() {
    // El orden es el de Linux: `mac-address` es la de ahora y
    // `local-mac-address` la de fabrica.
    let n = nodo(
        "ethernet@7d580000",
        vec![
            prop("local-mac-address", vec![0xDC, 0xA6, 0x32, 0, 0, 1]),
            prop("mac-address", vec![0xDC, 0xA6, 0x32, 0, 0, 2]),
        ],
        vec![],
    );
    assert_eq!(dt::mac_addr(&n), Some([0xDC, 0xA6, 0x32, 0, 0, 2]));
}

#[test]
fn a_property_that_is_not_six_bytes_is_not_a_mac() {
    // Una mas corta leida como seis se lleva por delante lo que haya detras, y
    // una mas larga no es una MAC: en los dos casos es mejor no tener ninguna
    // que tener media inventada, porque entonces el driver se inventa una
    // administrada localmente y lo dice.
    for raw in [vec![0xDC, 0xA6, 0x32], vec![0u8; 8], Vec::new()] {
        let n = nodo(
            "ethernet@7d580000",
            vec![prop("local-mac-address", raw.clone())],
            vec![],
        );
        assert_eq!(dt::mac_addr(&n), None, "con {} bytes", raw.len());
    }
}

#[test]
fn a_tree_with_no_mac_at_all_says_so() {
    let n = nodo("ethernet@7d580000", vec![], vec![]);
    assert_eq!(dt::mac_addr(&n), None);
}
