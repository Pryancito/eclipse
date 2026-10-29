//! Lo que el device tree dice del GEM del RP1, y --sobre todo-- lo que **no**
//! dice.
//!
//! El nodo es este, de `rp1.dtsi` del arbol de Raspberry Pi:
//!
//! ```text
//! rp1_eth: ethernet@100000 {
//!     reg = <0xc0 0x40100000  0x0 0x4000>;
//!     compatible = "raspberrypi,rp1-gem", "cdns,macb";
//!     interrupts = <RP1_INT_ETH IRQ_TYPE_LEVEL_HIGH>;
//!     clocks = <&rp1_clocks RP1_CLK_SYS &rp1_clocks RP1_CLK_SYS
//!               &rp1_clocks RP1_CLK_ETH_TSU &rp1_clocks RP1_CLK_ETH>;
//!     clock-names = "pclk", "hclk", "tsu_clk", "tx_clk";
//!     phy-mode = "rgmii-id";
//!     local-mac-address = [00 00 00 00 00 00];
//!     status = "disabled";
//! };
//! ```
//!
//! Comparado con el `ethernet@7d580000` de la Pi 4, que es de donde sale
//! [`genet::dt`](crate::net::genet), lo que llama la atencion son las
//! ausencias:
//!
//! * **No hay `phy-handle` ni nodo `mdio` hijo, asi que no hay direccion de
//!   PHY en ningun sitio.** En la Pi 4 el PHY esta en un nieto
//!   (`mdio@e14/ethernet-phy@1`) con su `reg`. Aqui no esta, y no es que el
//!   nodo este incompleto: Linux lo descubre barriendo el bus
//!   (`macb_mii_probe` sin `phy-handle` llama a `phylink_of_phy_connect` y
//!   acaba en un barrido). Eso quiere decir que **el barrido no es el camino
//!   de respaldo, es el unico camino**, y la direccion del PHY no se puede
//!   fijar en un test de arbol.
//! * **La `local-mac-address` son seis ceros.** La pone el firmware al
//!   arrancar, no el arbol, asi que leerla y creersela deja la tarjeta con la
//!   MAC `00:00:00:00:00:00`. Se rechaza por
//!   [`mac_addr_is_valid`](super::mac_addr_is_valid), que es la razon por la
//!   que esa funcion existe.
//! * **El `pclk` es un phandle a un proveedor de relojes, no una frecuencia.**
//!   El divisor del MDC sale de los hercios del `pclk`, y en el arbol no hay
//!   hercios: hay `<&rp1_clocks RP1_CLK_SYS>`, y Eclipse no tiene driver del
//!   proveedor de relojes del RP1. Se queda el valor por defecto de
//!   [`MacbConfig`](super::MacbConfig), que son los 200 MHz con los que
//!   arranca la placa, y se acepta un `clock-frequency` si alguna vez hay uno.
//!
//! Y una presencia que engaña: **`phy-mode = "rgmii-id"` no se programa en
//! ningun sitio**. En el GENET el modo del RGMII decide un bit del MAC
//! (`ID_MODE_DIS` del `EXT_RGMII_OOB_CTRL`); el GEM **no tiene ese registro**,
//! y en Linux el `phy-mode` del macb va al PHY por `phylink`, no al MAC. Lo
//! unico que se hace con el aqui es comprobar que es una variante de RGMII, y
//! avisar si no: un modo distinto (SGMII, MII) seria otra placa, y el driver
//! estaria programando el MAC como si fuera la Pi 5.

use super::MacbConfig;
use crate::utils::devicetree::Node;

/// Los cuatro modos de RGMII, que es lo que el GEM del RP1 lleva detras.
///
/// El GEM no tiene con que distinguirlos --- no hay bit del retardo en el
/// MAC --- asi que esto no elige nada: dice si el nodo describe la placa que
/// este driver sabe manejar.
const MODOS_RGMII: [&str; 4] = ["rgmii", "rgmii-id", "rgmii-rxid", "rgmii-txid"];

/// Si el `phy-mode` del nodo es uno que este driver puede manejar.
///
/// Devuelve `true` tambien cuando el nodo no dice nada: el `phy-mode` es
/// opcional y no cambia ningun registro, asi que su ausencia no es motivo para
/// dejar una placa sin red.
pub fn phy_mode_is_supported(node: &Node) -> bool {
    match node.prop_str("phy-mode") {
        Ok(modo) if MODOS_RGMII.contains(&modo) => true,
        Ok(otro) => {
            warn!(
                "macb: phy-mode {:?} no es RGMII; este driver solo sabe del GEM del RP1",
                otro
            );
            false
        }
        Err(_) => true,
    }
}

/// La MAC que trae el nodo, si trae una **que sirva**.
///
/// En la Pi 5 la `local-mac-address` del arbol son seis ceros, que es lo que
/// `rp1.dtsi` escribe: la de verdad la pone el firmware en los registros del
/// GEM. Asi que aqui lo normal es devolver `None`, y la MAC sale de
/// [`hw_mac_addr`](super::MacbMac::hw_mac_addr).
///
/// `mac-address` es la que puede haber cambiado en caliente y
/// `local-mac-address` la de fabrica; el orden es el de Linux.
pub fn mac_addr(node: &Node) -> Option<[u8; 6]> {
    for nombre in ["mac-address", "local-mac-address"] {
        let Some(raw) = node.prop_raw(nombre) else {
            continue;
        };
        // Exactamente seis bytes. Una propiedad mas corta leida como seis se
        // lleva por delante lo que haya detras, y una mas larga no es una MAC:
        // en los dos casos es mejor no tener ninguna que tener una inventada a
        // medias.
        if raw.len() != 6 {
            warn!(
                "macb: la propiedad {} mide {} bytes, no 6: se ignora",
                nombre,
                raw.len()
            );
            continue;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&raw[..6]);
        if super::mac_addr_is_valid(&mac) {
            return Some(mac);
        }
        // **Esto es el caso normal en la Pi 5**, no un arbol roto: los seis
        // ceros son el hueco que el firmware rellena. No se avisa, porque un
        // aviso en cada arranque de una placa que esta bien es ruido.
        return None;
    }
    None
}

/// La configuracion del MAC que se puede sacar del nodo.
///
/// Es poco: el divisor del MDC sale de los hercios del `pclk`, y el arbol da
/// un phandle a un proveedor de relojes que Eclipse no sabe leer. Se acepta un
/// `clock-frequency` --- que el nodo del RP1 **no** trae, pero que es la
/// propiedad estandar para decir una frecuencia --- y si no, se queda el valor
/// por defecto.
///
/// Un `clock-frequency` de cero o absurdamente bajo se descarta: el divisor se
/// elige por tramos y un `pclk` que no llegue al primero daria el divisor mas
/// pequeño, o sea un MDC por encima de los 2,5 MHz que el bus admite.
pub fn config(node: &Node) -> MacbConfig {
    let mut cfg = MacbConfig::default();
    if let Ok(hz) = node.prop_u32("clock-frequency") {
        if hz >= 1_000_000 {
            cfg.pclk_hz = hz;
        } else {
            warn!(
                "macb: clock-frequency {} Hz no es un pclk creible: se usa el valor por defecto",
                hz
            );
        }
    }
    cfg
}

#[cfg(test)]
mod dt_tests;
