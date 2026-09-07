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
    let text = std::fs::read_to_string("/etc/eclipse/locale").ok()?;
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
}
