//! What the top bar shows: its claves in `/etc/eclipse/entorno`
//! (`LUNARBAR_CPU`, `LUNARBAR_TITULO`...), each `si` or `no`, written by the
//! control panel (`claves.d/64-barra.claves`) through `eclipse-entorno`,
//! which restarts the bar. Everything is on unless a clave says `no`, so a
//! machine with no file shows the bar it always did.
//!
//! The bar's layout already drops a module that does not fit; a module
//! turned off here is simply never offered to it, and what is left is laid
//! out by the same sums.

use std::sync::OnceLock;

/// One module of the bar that can be turned off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modulo {
    Escritorios,
    Titulo,
    Cpu,
    Memoria,
    Red,
    Temperatura,
    Disco,
    Bateria,
    EnMarcha,
    Carga,
}

/// Every module with the clave that holds it.
pub const MODULOS: [(Modulo, &str); 10] = [
    (Modulo::Escritorios, "LUNARBAR_ESCRITORIOS"),
    (Modulo::Titulo, "LUNARBAR_TITULO"),
    (Modulo::Cpu, "LUNARBAR_CPU"),
    (Modulo::Memoria, "LUNARBAR_MEMORIA"),
    (Modulo::Red, "LUNARBAR_RED"),
    (Modulo::Temperatura, "LUNARBAR_TEMPERATURA"),
    (Modulo::Disco, "LUNARBAR_DISCO"),
    (Modulo::Bateria, "LUNARBAR_BATERIA"),
    (Modulo::EnMarcha, "LUNARBAR_EN_MARCHA"),
    (Modulo::Carga, "LUNARBAR_CARGA"),
];

/// The modules turned off, from `get` (a clave's value by name). Only the
/// word `no` turns one off.
pub fn apagados_de(get: impl Fn(&str) -> Option<String>) -> Vec<Modulo> {
    MODULOS
        .iter()
        .filter(|(_, k)| get(k).is_some_and(|v| v.trim().eq_ignore_ascii_case("no")))
        .map(|(m, _)| *m)
        .collect()
}

/// Whether the bar shows `m`. Read once per process.
pub fn muestra(m: Modulo) -> bool {
    static OFF: OnceLock<Vec<Modulo>> = OnceLock::new();
    !OFF.get_or_init(|| apagados_de(lunartext::entorno::var))
        .contains(&m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_is_on_unless_its_clave_says_no() {
        assert!(apagados_de(|_| None).is_empty());
        let off = apagados_de(|k| match k {
            "LUNARBAR_CPU" => Some("no".into()),
            "LUNARBAR_TITULO" => Some(" No ".into()),
            "LUNARBAR_RED" => Some("si".into()),
            "LUNARBAR_DISCO" => Some("quizá".into()),
            _ => None,
        });
        assert_eq!(off, vec![Modulo::Titulo, Modulo::Cpu]);
    }

    #[test]
    fn every_module_has_its_own_clave() {
        let mut ks: Vec<&str> = MODULOS.iter().map(|(_, k)| *k).collect();
        ks.sort();
        ks.dedup();
        assert_eq!(ks.len(), MODULOS.len());
        assert!(ks.iter().all(|k| k.starts_with("LUNARBAR_")));
    }
}
