//! First-party UI language for lunarbar (`es` / `en`).
//!
//! GTK/Firefox use gettext via `$LANG`. This crate does not: no libintl, no
//! `setlocale`. Strings are a static table keyed by [`Lang`], resolved from
//! `/etc/eclipse/locale` then `$LANG`, default `es`. Keyboard layout (`kbd=`)
//! is a different preference.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Es,
    En,
}

impl Lang {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "es" | "ES" | "es_ES" => Some(Lang::Es),
            "en" | "EN" | "en_US" => Some(Lang::En),
            _ => None,
        }
    }

    pub fn from_posix(lang: &str) -> Self {
        let tag = lang.split(['.', '@', ':']).next().unwrap_or(lang);
        if tag.eq_ignore_ascii_case("en")
            || tag.eq_ignore_ascii_case("en_US")
            || tag.eq_ignore_ascii_case("en_GB")
        {
            Lang::En
        } else {
            Lang::Es
        }
    }

    pub fn current() -> Self {
        if let Some(l) = file_lang() {
            return l;
        }
        if let Ok(lang) = std::env::var("LANG") {
            return Self::from_posix(&lang);
        }
        Lang::Es
    }

    pub fn apps_title(self) -> &'static str {
        match self {
            Lang::Es => "aplicaciones",
            Lang::En => "applications",
        }
    }

    pub fn apps_search(self) -> &'static str {
        match self {
            Lang::Es => "buscar aplicaciones",
            Lang::En => "search apps",
        }
    }

    /// lunarrun's own strings.
    pub fn run_title(self) -> &'static str {
        match self {
            Lang::Es => "ejecutar",
            Lang::En => "run",
        }
    }

    pub fn run_hint(self) -> &'static str {
        match self {
            Lang::Es => "escribe para buscar o ejecutar",
            Lang::En => "type to search or run",
        }
    }

    pub fn run_command(self) -> &'static str {
        match self {
            Lang::Es => "ejecutar orden",
            Lang::En => "run command",
        }
    }

    pub fn run_empty(self) -> &'static str {
        match self {
            Lang::Es => "sin resultados",
            Lang::En => "no results",
        }
    }

    pub fn run_keys(self) -> &'static str {
        match self {
            Lang::Es => "Intro ejecuta   Esc cierra",
            Lang::En => "Enter runs   Esc closes",
        }
    }

    pub fn run_reload(self) -> &'static str {
        match self {
            Lang::Es => "recargar labwc",
            Lang::En => "reload labwc",
        }
    }

    pub fn power_lock(self) -> &'static str {
        match self {
            Lang::Es => "bloquear",
            Lang::En => "lock",
        }
    }

    pub fn power_logout(self) -> &'static str {
        match self {
            Lang::Es => "cerrar sesión",
            Lang::En => "log out",
        }
    }

    pub fn power_reboot(self) -> &'static str {
        match self {
            Lang::Es => "reiniciar",
            Lang::En => "reboot",
        }
    }

    pub fn power_shutdown(self) -> &'static str {
        match self {
            Lang::Es => "apagar",
            Lang::En => "shut down",
        }
    }

    /// Sunday-first, for the date pill (`dom 21 jul` / `sun 21 jul`).
    pub fn weekday_sun_first(self) -> [&'static str; 7] {
        match self {
            Lang::Es => ["dom", "lun", "mar", "mié", "jue", "vie", "sáb"],
            Lang::En => ["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
        }
    }

    /// Monday-first, two-letter calendar header.
    pub fn weekday_mon_first(self) -> [&'static str; 7] {
        match self {
            Lang::Es => ["lu", "ma", "mi", "ju", "vi", "sá", "do"],
            Lang::En => ["mo", "tu", "we", "th", "fr", "sa", "su"],
        }
    }

    pub fn month_short(self) -> [&'static str; 12] {
        match self {
            Lang::Es => [
                "ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic",
            ],
            Lang::En => [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ],
        }
    }

    pub fn month_full(self) -> [&'static str; 12] {
        match self {
            Lang::Es => [
                "enero",
                "febrero",
                "marzo",
                "abril",
                "mayo",
                "junio",
                "julio",
                "agosto",
                "septiembre",
                "octubre",
                "noviembre",
                "diciembre",
            ],
            Lang::En => [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ],
        }
    }
}

fn file_lang() -> Option<Lang> {
    parse_locale(&std::fs::read_to_string("/etc/eclipse/locale").ok()?)
}

/// The language `/etc/eclipse/locale` names, in either shape the file can
/// have: a `lang=` line (what `eclipse-look` writes) or a bare language name
/// (what a hand-edited file looks like). Comments and blanks are skipped, and
/// any other `key=` line belongs to another preference -- `kbd=` lives here
/// too. Split from the read so the shapes can be exercised without a file.
fn parse_locale(text: &str) -> Option<Lang> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("lang=") {
            return Lang::from_name(v);
        }
        if !line.contains('=') {
            return Lang::from_name(line);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_en_and_default_es() {
        assert_eq!(Lang::from_posix("en_US.UTF-8"), Lang::En);
        assert_eq!(Lang::from_posix("es_ES.UTF-8"), Lang::Es);
        assert_eq!(Lang::from_posix("C.UTF-8"), Lang::Es);
        assert_eq!(Lang::from_name("en"), Some(Lang::En));
        assert_eq!(Lang::from_name("de"), None);
    }

    #[test]
    fn tables_differ() {
        assert_ne!(Lang::Es.apps_title(), Lang::En.apps_title());
        assert_eq!(Lang::Es.weekday_sun_first()[0], "dom");
        assert_eq!(Lang::En.weekday_sun_first()[0], "sun");
        assert_eq!(Lang::Es.month_full()[0], "enero");
        assert_eq!(Lang::En.month_full()[0], "January");
    }

    /// Every string the panel shows exists in both languages and is a
    /// different string in each. A row copied from the other column is
    /// invisible until someone runs the panel in that language, and the whole
    /// point of this table is that nobody does both.
    #[test]
    fn every_string_the_panel_shows_has_its_own_pair() {
        type Get = fn(Lang) -> &'static str;
        const TABLE: [(&str, Get, &str, &str); 12] = [
            (
                "apps_title",
                Lang::apps_title,
                "aplicaciones",
                "applications",
            ),
            (
                "apps_search",
                Lang::apps_search,
                "buscar aplicaciones",
                "search apps",
            ),
            ("run_title", Lang::run_title, "ejecutar", "run"),
            (
                "run_hint",
                Lang::run_hint,
                "escribe para buscar o ejecutar",
                "type to search or run",
            ),
            (
                "run_command",
                Lang::run_command,
                "ejecutar orden",
                "run command",
            ),
            ("run_empty", Lang::run_empty, "sin resultados", "no results"),
            (
                "run_keys",
                Lang::run_keys,
                "Intro ejecuta   Esc cierra",
                "Enter runs   Esc closes",
            ),
            (
                "run_reload",
                Lang::run_reload,
                "recargar labwc",
                "reload labwc",
            ),
            ("power_lock", Lang::power_lock, "bloquear", "lock"),
            (
                "power_logout",
                Lang::power_logout,
                "cerrar sesión",
                "log out",
            ),
            ("power_reboot", Lang::power_reboot, "reiniciar", "reboot"),
            (
                "power_shutdown",
                Lang::power_shutdown,
                "apagar",
                "shut down",
            ),
        ];
        for (name, get, es, en) in TABLE {
            assert_eq!(get(Lang::Es), es, "{name} in Spanish");
            assert_eq!(get(Lang::En), en, "{name} in English");
        }
    }

    /// The two weekday tables differ only in where the week starts, and that
    /// is the whole reason there are two: the date pill counts from Sunday,
    /// the calendar header from Monday. Swapping them silently relabels every
    /// column of the calendar.
    #[test]
    fn each_weekday_table_starts_on_the_day_its_name_says() {
        assert_eq!(
            Lang::Es.weekday_sun_first(),
            ["dom", "lun", "mar", "mié", "jue", "vie", "sáb"]
        );
        assert_eq!(
            Lang::En.weekday_sun_first(),
            ["sun", "mon", "tue", "wed", "thu", "fri", "sat"]
        );
        assert_eq!(
            Lang::Es.weekday_mon_first(),
            ["lu", "ma", "mi", "ju", "vi", "sá", "do"]
        );
        assert_eq!(
            Lang::En.weekday_mon_first(),
            ["mo", "tu", "we", "th", "fr", "sa", "su"]
        );
        // The Monday-first table is the Sunday-first one rotated by one, in
        // both languages: same seven days, a different first column.
        for lang in [Lang::Es, Lang::En] {
            let sun = lang.weekday_sun_first();
            let mon = lang.weekday_mon_first();
            for i in 0..7 {
                let rotated = sun[(i + 1) % 7];
                assert!(
                    rotated.starts_with(mon[i]),
                    "{i}: {} against {rotated}",
                    mon[i]
                );
            }
        }
    }

    /// Both month tables run January to December. The calendar indexes them by
    /// a 0-based month straight out of `localtime`, so a pair out of order is
    /// a month drawn under its neighbour's name.
    #[test]
    fn both_month_tables_run_january_to_december() {
        assert_eq!(
            Lang::Es.month_short(),
            ["ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic"]
        );
        assert_eq!(
            Lang::En.month_short(),
            ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        );
        assert_eq!(
            Lang::Es.month_full(),
            [
                "enero",
                "febrero",
                "marzo",
                "abril",
                "mayo",
                "junio",
                "julio",
                "agosto",
                "septiembre",
                "octubre",
                "noviembre",
                "diciembre"
            ]
        );
        assert_eq!(
            Lang::En.month_full(),
            [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December"
            ]
        );
        // The short form is the long one's first three letters, which is what
        // keeps the two tables in step with each other.
        for lang in [Lang::Es, Lang::En] {
            for (short, full) in lang.month_short().iter().zip(lang.month_full()) {
                let head: String = full.to_lowercase().chars().take(3).collect();
                assert_eq!(*short, head, "{full}");
            }
        }
    }

    /// Every spelling `eclipse-look` and the locale file can write.
    #[test]
    fn every_spelling_of_a_language_name_is_accepted() {
        for n in ["es", "ES", "es_ES"] {
            assert_eq!(Lang::from_name(n), Some(Lang::Es), "{n}");
        }
        for n in ["en", "EN", "en_US"] {
            assert_eq!(Lang::from_name(n), Some(Lang::En), "{n}");
        }
        // The file is read line by line, so the newline comes with it.
        assert_eq!(Lang::from_name("  es\n"), Some(Lang::Es));
        assert_eq!(Lang::from_name("\ten_US "), Some(Lang::En));
        for n in ["", "de", "es-ES", "english", "e"] {
            assert_eq!(Lang::from_name(n), None, "{n}");
        }
    }

    /// `$LANG` is English only for the three tags that name English, and the
    /// tag is what is left after the charset, the modifier and the separator
    /// the variable can carry.
    #[test]
    fn a_posix_locale_is_english_only_where_it_names_english() {
        for l in [
            "en",
            "EN",
            "en_US",
            "en_US.UTF-8",
            "en_GB.UTF-8",
            "en_gb",
            "en_US@euro",
            "en:en_GB",
        ] {
            assert_eq!(Lang::from_posix(l), Lang::En, "{l}");
        }
        // Everything else falls back to Spanish, this image's own language.
        for l in [
            "es_ES.UTF-8",
            "C",
            "C.UTF-8",
            "POSIX",
            "",
            "english",
            "en_CA",
        ] {
            assert_eq!(Lang::from_posix(l), Lang::Es, "{l}");
        }
    }

    /// The locale file's shape: a bare language name or a `lang=` line, with
    /// comments and blanks skipped. `eclipse-look` writes the `lang=` form;
    /// the bare one is what a hand-edited file looks like.
    #[test]
    fn the_locale_file_is_read_in_either_of_the_two_shapes_it_can_have() {
        assert_eq!(parse_locale("en\n"), Some(Lang::En));
        assert_eq!(parse_locale("lang=en\n"), Some(Lang::En));
        assert_eq!(parse_locale("  lang=es  \n"), Some(Lang::Es));
        // A comment and a blank line are not the language. The comment has
        // to be skipped as a comment and not merely fall through the `lang=`
        // and the `key=` tests: one without an '=' in it would otherwise be
        // read as a bare language name, and answer None for the whole file.
        assert_eq!(parse_locale("# lang=en\n\nes\n"), Some(Lang::Es));
        assert_eq!(parse_locale("# a note\nes\n"), Some(Lang::Es));
        // A key that is not `lang=` is skipped, not read as a bare name.
        assert_eq!(parse_locale("kbd=us\nlang=en\n"), Some(Lang::En));
        assert_eq!(parse_locale(""), None);
        assert_eq!(parse_locale("# nothing but a comment\n"), None);
        // A `lang=` line naming a language nobody knows answers None rather
        // than reading on: the file said what it wanted.
        assert_eq!(parse_locale("lang=de\nes\n"), None);
    }
}
