//! Lo que el device tree dice del GENET, y nada mas.
//!
//! Son tres propiedades, y las tres estan en el nodo `ethernet@7d580000` del
//! device tree de la Raspberry Pi 4 (`bcm2711-rpi-4-b.dts`) o en los nodos
//! que cuelgan de el:
//!
//! - **la direccion MDIO del PHY**, que esta en un nieto
//!   (`mdio@e14/ethernet-phy@1`) y no en el nodo del MAC;
//! - **`phy-mode`**, que decide quien pone el retardo del RGMII;
//! - **`local-mac-address`**, que es la MAC de la placa, la que esta impresa
//!   en la caja.
//!
//! Esta en su fichero y son funciones puras sobre un `Node` a proposito: asi
//! se pueden probar sin levantar un GENET, que es lo unico que el arbol de
//! aqui puede hacer. La rama del `parse_ethernet` que las usa no se puede
//! probar: construye el driver de verdad sobre la ventana que el mapeador
//! devuelva.

use crate::utils::devicetree::Node;

/// La direccion MDIO del PHY, buscada donde el device tree la pone.
///
/// En la Pi 4 el PHY **no** esta en el nodo del MAC: cuelga de un nodo `mdio`
/// que cuelga del MAC, y su direccion es el `reg` de ese nieto.
///
/// ```text
/// ethernet@7d580000 {
///     compatible = "brcm,bcm2711-genet-v5";
///     phy-handle = <&phy1>;
///     mdio@e14 {
///         compatible = "brcm,genet-mdio-v5";
///         phy1: ethernet-phy@1 { reg = <0x1>; };
///     };
/// };
/// ```
///
/// Se busca por el arbol y no se resuelve el `phy-handle`: el phandle habria
/// que resolverlo contra el arbol entero, y el nodo al que apunta es
/// **siempre** ese nieto. `None` es «no lo dice el arbol», y entonces el
/// driver barre el bus, que es lo que hace U-Boot.
pub fn phy_addr(node: &Node) -> Option<u8> {
    for mdio in &node.children {
        if !is_mdio(mdio) {
            continue;
        }
        for phy in &mdio.children {
            if let Some(addr) = phy_reg(phy) {
                return Some(addr);
            }
        }
    }
    None
}

/// El nodo del bus MDIO, por lo que declara o, si no declara nada, por el
/// nombre. El device tree de la Pi lo llama `mdio@e14`, que es el offset del
/// `UMAC_MDIO_CMD` dentro de la ventana.
fn is_mdio(node: &Node) -> bool {
    if let Ok(comp) = node.prop_str_list("compatible") {
        if comp.iter().any(|c| c.contains("genet-mdio")) {
            return true;
        }
    }
    node.name == "mdio" || node.name.starts_with("mdio@")
}

/// La direccion de un nodo de PHY: su `reg`, que en este bus es la direccion
/// MDIO y va de 0 a 31.
fn phy_reg(node: &Node) -> Option<u8> {
    if !(node.name == "ethernet-phy" || node.name.starts_with("ethernet-phy@")) {
        return None;
    }
    let reg = node.prop_u32("reg").ok()?;
    // Mas de 31 no es una direccion de este bus: son cinco bits en el
    // `UMAC_MDIO_CMD`, y pasarlo adelante seria hablarle a otra direccion sin
    // decirlo.
    (reg < 32).then_some(reg as u8)
}

/// Si el MAC tiene que **dejar** de poner el retardo del RGMII porque lo pone
/// la placa o el PHY (el `ID_MODE_DIS` del `EXT_RGMII_OOB_CTRL`).
///
/// Las cuatro variantes de RGMII dicen quien retrasa cada reloj:
///
/// - `rgmii`: lo retrasa la placa (las pistas). El MAC no.
/// - `rgmii-rxid`: lo retrasa el PHY en recepcion. El MAC no. **Es lo que
///   declara la Pi 4.**
/// - `rgmii-id` y `rgmii-txid`: lo tiene que poner el MAC.
///
/// Es la misma tabla que `bcmgenet.c` de U-Boot, y equivocarla no da un error:
/// da un enlace que sube y pierde tramas a gigabit, porque los datos llegan
/// fuera de la ventana del reloj.
///
/// Sin `phy-mode` se devuelve `true`, que es lo que la Pi 4 necesita: este
/// driver solo arranca en un BCM2711, y ahi el modo es `rgmii-rxid`.
pub fn id_mode_dis(node: &Node) -> bool {
    match node.prop_str("phy-mode") {
        Ok("rgmii") | Ok("rgmii-rxid") => true,
        Ok("rgmii-id") | Ok("rgmii-txid") => false,
        Ok(otro) => {
            warn!(
                "genet: phy-mode {:?} no es RGMII; se trata como rgmii-rxid",
                otro
            );
            true
        }
        Err(_) => true,
    }
}

/// La MAC de la placa, si el device tree la trae.
///
/// En la Pi la pone el firmware en el arbol **y** en los registros del UMAC,
/// pero el arbol es la fuente de verdad: es de donde la saca Linux, y una
/// placa cuyo firmware deja los registros a cero sigue teniendola aqui.
///
/// `mac-address` es la que puede haber cambiado en caliente y
/// `local-mac-address` la de fabrica; el orden es el de Linux.
pub fn mac_addr(node: &Node) -> Option<[u8; 6]> {
    for nombre in ["mac-address", "local-mac-address"] {
        if let Some(raw) = node.prop_raw(nombre) {
            // Exactamente seis bytes. Una propiedad mas corta leida como seis
            // se lleva por delante lo que haya detras, y una mas larga no es
            // una MAC: en los dos casos es mejor no tener ninguna que tener
            // una inventada a medias.
            if raw.len() == 6 {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&raw[..6]);
                return Some(mac);
            }
            warn!(
                "genet: la propiedad {} mide {} bytes, no 6: se ignora",
                nombre,
                raw.len()
            );
        }
    }
    None
}
