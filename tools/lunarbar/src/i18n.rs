//! The panel's UI language, which is [`eclipse_i18n`]'s.
//!
//! The table itself lives in `tools/eclipse-i18n`, because lunarbar is not the
//! only client that draws Spanish: SilverSurfer and lunarbg are separate
//! crates and cannot link a module inside lunarbar. This module is the
//! re-export, so the seventy-odd `i18n::Lang` call sites in the panel did not
//! have to change when the strings moved.
//!
//! **New strings go through [`Key`]**, not through a new named accessor.

pub use eclipse_i18n::{Key, Lang};

#[cfg(test)]
mod tests {
    use super::*;

    /// The strings the panel shipped before the table existed, unchanged.
    ///
    /// The batch that moved them did it by hand, file by file; this is what
    /// says it moved them and did not reword any. It reads them through the
    /// named accessors on purpose, which also pins each accessor to its row:
    /// one wired to the wrong key would draw someone else's string.
    #[test]
    fn the_strings_the_panel_shipped_are_the_strings_it_still_shows() {
        #[allow(clippy::type_complexity)]
        let rows: [(&str, &str, &str, (&str, &str)); 14] = [
            (
                "apps_title",
                "aplicaciones",
                "applications",
                (Lang::Es.apps_title(), Lang::En.apps_title()),
            ),
            (
                "apps_search",
                "buscar aplicaciones",
                "search apps",
                (Lang::Es.apps_search(), Lang::En.apps_search()),
            ),
            (
                "run_title",
                "ejecutar",
                "run",
                (Lang::Es.run_title(), Lang::En.run_title()),
            ),
            (
                "run_key",
                "ALT+ESPACIO",
                "ALT+SPACE",
                (Lang::Es.run_key(), Lang::En.run_key()),
            ),
            (
                "enter_key",
                "INTRO",
                "ENTER",
                (Lang::Es.enter_key(), Lang::En.enter_key()),
            ),
            (
                "run_hint",
                "escribe para buscar o ejecutar",
                "type to search or run",
                (Lang::Es.run_hint(), Lang::En.run_hint()),
            ),
            (
                "run_command",
                "ejecutar orden",
                "run command",
                (Lang::Es.run_command(), Lang::En.run_command()),
            ),
            (
                "run_empty",
                "sin resultados",
                "no results",
                (Lang::Es.run_empty(), Lang::En.run_empty()),
            ),
            (
                "run_keys",
                "Intro ejecuta   Esc cierra",
                "Enter runs   Esc closes",
                (Lang::Es.run_keys(), Lang::En.run_keys()),
            ),
            (
                "run_reload",
                "recargar labwc",
                "reload labwc",
                (Lang::Es.run_reload(), Lang::En.run_reload()),
            ),
            (
                "power_lock",
                "bloquear",
                "lock",
                (Lang::Es.power_lock(), Lang::En.power_lock()),
            ),
            (
                "power_logout",
                "cerrar sesión",
                "log out",
                (Lang::Es.power_logout(), Lang::En.power_logout()),
            ),
            (
                "power_reboot",
                "reiniciar",
                "reboot",
                (Lang::Es.power_reboot(), Lang::En.power_reboot()),
            ),
            (
                "power_shutdown",
                "apagar",
                "shut down",
                (Lang::Es.power_shutdown(), Lang::En.power_shutdown()),
            ),
        ];
        for (name, es, en, got) in rows {
            assert_eq!(got.0, es, "{name} in Spanish");
            assert_eq!(got.1, en, "{name} in English");
        }
    }
}
