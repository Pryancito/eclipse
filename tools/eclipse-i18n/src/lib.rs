//! The UI strings of Eclipse's own clients, in Spanish and English.
//!
//! One table, linked by every client that draws text: the panel and its
//! binaries (lunarbar, lunarrun, lunarfile, lunarclip, lunarnet, lunardesk,
//! lunarlock, lunarshot, lunarnotes, lunarhunter), the wallpaper client
//! (lunarbg) and the browser (SilverSurfer). GTK and Firefox use gettext via
//! `$LANG`; these do not -- no libintl, no `setlocale`, nothing to install in
//! the rootfs.
//!
//! The language comes from `/etc/eclipse/locale`, then `$LANG`, then Spanish,
//! which is this image's own language. `eclipse-locale es|us` is what writes
//! that file; the keyboard layout (`kbd=`) is a different preference and lives
//! in `/etc/eclipse/keyboard`.
//!
//! # Why a table and not a method per string
//!
//! lunarbar's own `i18n.rs`, which this replaces, had one method per string.
//! That is fine for a dozen and unreadable at two hundred: the file was 486
//! lines for twelve strings plus the date tables. Here each string is one row
//! of [`strings!`], which also means the set of keys is a value the tests can
//! walk -- and [`Key::ALL`] is what makes
//! `every_key_has_both_languages_and_they_differ` possible at all.

#![forbid(unsafe_code)]

mod table;
pub use table::Key;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Es,
    En,
}

impl Lang {
    /// The language a name spells, or `None` if it is not one of the two.
    ///
    /// `us` is a spelling of English because `eclipse-locale` speaks `es|us`,
    /// the same two country names as `eclipse-kbd`. The command itself always
    /// writes the tag (`lang=en`); this arm is for the file somebody edited
    /// with the command's own vocabulary.
    ///
    /// An unknown name is NOT a language: it answers `None` so the caller
    /// falls through to the next source, rather than a stale `lang=fr`
    /// pinning the desktop to a language with no translation.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "es" | "ES" | "es_ES" => Some(Lang::Es),
            "en" | "EN" | "en_US" | "us" | "US" => Some(Lang::En),
            _ => None,
        }
    }

    /// The language a POSIX locale names, Spanish for anything else.
    ///
    /// Unlike [`Lang::from_name`] this always answers, because `$LANG` is the
    /// last source before the default: `C`, `POSIX` and an empty value all
    /// mean "no preference", which on this image is Spanish.
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

    /// `/etc/eclipse/locale`, then `$LANG`, then Spanish.
    pub fn current() -> Self {
        if let Some(l) = file_lang() {
            return l;
        }
        if let Ok(lang) = std::env::var("LANG") {
            return Self::from_posix(&lang);
        }
        Lang::Es
    }

    /// This language's string for `key`.
    pub fn s(self, key: Key) -> &'static str {
        let (es, en) = key.pair();
        match self {
            Lang::Es => es,
            Lang::En => en,
        }
    }

    /// The character a fraction is written with: `1,5 K` in Spanish, `1.5 K`
    /// in English.
    ///
    /// Not a row of the table because it is not a string anybody translated:
    /// it is how the language writes numbers, and the only number in the whole
    /// desktop with a fraction in it is lunarfile's size column.
    pub fn decimal_point(self) -> char {
        match self {
            Lang::Es => ',',
            Lang::En => '.',
        }
    }

    // ── the named accessors the panel already called ─────────────────────
    //
    // Before the table existed, lunarbar's `i18n.rs` had one method per
    // string, and about seventy call sites use them. They are kept as one
    // lookup each so that the batch which moved the strings into this table
    // did not also have to rewrite every call site -- and, more to the point,
    // so there is exactly ONE copy of each string, covered by
    // `every_pair_differs_in_the_two_languages` like every other row.
    //
    // They are inherent methods and not a trait because a trait would need a
    // `use` in all thirteen files that call them, which is the churn this was
    // meant to avoid. NEW STRINGS GO THROUGH `Key`: this list does not grow.

    pub fn apps_title(self) -> &'static str {
        self.s(Key::apps_title)
    }

    pub fn apps_search(self) -> &'static str {
        self.s(Key::apps_search)
    }

    pub fn run_title(self) -> &'static str {
        self.s(Key::run_title)
    }

    pub fn run_key(self) -> &'static str {
        self.s(Key::run_key)
    }

    pub fn enter_key(self) -> &'static str {
        self.s(Key::enter_key)
    }

    pub fn run_hint(self) -> &'static str {
        self.s(Key::run_hint)
    }

    pub fn run_command(self) -> &'static str {
        self.s(Key::run_command)
    }

    pub fn run_empty(self) -> &'static str {
        self.s(Key::run_empty)
    }

    pub fn run_keys(self) -> &'static str {
        self.s(Key::run_keys)
    }

    pub fn run_reload(self) -> &'static str {
        self.s(Key::run_reload)
    }

    pub fn power_lock(self) -> &'static str {
        self.s(Key::power_lock)
    }

    pub fn power_logout(self) -> &'static str {
        self.s(Key::power_logout)
    }

    pub fn power_reboot(self) -> &'static str {
        self.s(Key::power_reboot)
    }

    pub fn power_shutdown(self) -> &'static str {
        self.s(Key::power_shutdown)
    }

    /// Sunday-first, for the date pill (`dom 21 jul` / `sun 21 jul`).
    pub fn weekday_sun_first(self) -> [&'static str; 7] {
        match self {
            Lang::Es => ["dom", "lun", "mar", "mié", "jue", "vie", "sáb"],
            Lang::En => ["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
        }
    }

    /// Sunday-first, written out whole: the date line of the lock screen
    /// (`jueves 5 de noviembre de 2026` / `Thursday 5 November 2026`).
    ///
    /// Sunday-first because that is how a `tm_wday` comes out of the clock,
    /// and whole because the lock screen has the width of the display.
    pub fn weekday_full(self) -> [&'static str; 7] {
        match self {
            Lang::Es => [
                "domingo",
                "lunes",
                "martes",
                "miércoles",
                "jueves",
                "viernes",
                "sábado",
            ],
            Lang::En => [
                "Sunday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
            ],
        }
    }

    /// Monday-first, written out: what a scheduled scan says it runs on
    /// (`cada lunes` / `every Monday`).
    ///
    /// `miérc.` and `Wednes.` are cut on purpose: the card they go on is the
    /// width of a scan job and the longest day name does not fit it in either
    /// language.
    pub fn weekday_long(self) -> [&'static str; 7] {
        match self {
            Lang::Es => [
                "lunes", "martes", "miérc.", "jueves", "viernes", "sábado", "domingo",
            ],
            Lang::En => [
                "Monday", "Tuesday", "Wednes.", "Thursday", "Friday", "Saturday", "Sunday",
            ],
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
/// have: a `lang=` line (what `eclipse-locale` writes) or a bare language name
/// (what a hand-edited file looks like). Comments and blanks are skipped, and
/// any other `key=` line belongs to another preference. Split from the read so
/// the shapes can be exercised without a file.
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
mod tests;
