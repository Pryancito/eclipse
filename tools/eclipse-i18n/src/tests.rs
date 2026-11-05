use super::*;

/// Every row has both languages and, unless it says otherwise, they are
/// different strings.
///
/// This is the whole reason the table exists in one place. A row whose English
/// column was copied from the Spanish one is invisible until somebody runs the
/// client in English, and nobody runs both; an empty column is a label that
/// draws as nothing, which looks like a layout bug rather than a missing
/// translation. A row that really is one string in both languages says `both:`
/// and is exempt by construction, so there is no allow-list to keep in step.
#[test]
fn every_pair_differs_in_the_two_languages() {
    for key in Key::ALL {
        let (es, en) = key.pair();
        let name = key.name();
        assert!(!es.trim().is_empty(), "{name}: the Spanish column is empty");
        assert!(!en.trim().is_empty(), "{name}: the English column is empty");
        if key.is_shared() {
            assert_eq!(es, en, "{name}: declared `both:` but the columns differ");
        } else {
            assert_ne!(
                es, en,
                "{name}: the two columns are the same string. If that is on \
                 purpose, write the row as `both:`"
            );
        }
    }
}

/// No key is declared twice, and no two keys hold the same Spanish string.
///
/// A duplicated name would not compile (two arms of the same enum), so what
/// this really catches is the other half: two keys with the same text, which
/// means one of them is dead and the next person to change that string will
/// change only one of the two places it is drawn.
#[test]
fn no_two_keys_hold_the_same_spanish_string() {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for key in Key::ALL {
        let (es, _) = key.pair();
        if let Some((other, _)) = seen.iter().find(|(_, s)| *s == es) {
            panic!("{} and {} both say {es:?}", key.name(), other);
        }
        seen.push((key.name(), es));
    }
}

/// `Lang::s` hands back the column its language names, for every row.
#[test]
fn each_language_gets_its_own_column() {
    for key in Key::ALL {
        let (es, en) = key.pair();
        assert_eq!(Lang::Es.s(*key), es, "{}", key.name());
        assert_eq!(Lang::En.s(*key), en, "{}", key.name());
    }
}

/// Every spelling `eclipse-locale` and the locale file can write.
#[test]
fn every_spelling_of_a_language_name_is_accepted() {
    for n in ["es", "ES", "es_ES"] {
        assert_eq!(Lang::from_name(n), Some(Lang::Es), "{n}");
    }
    // `us` and `US` are in here because that is the word the command the user
    // types is spelt with: `eclipse-locale es|us`, the same two country names
    // as `eclipse-kbd`.
    for n in ["en", "EN", "en_US", "us", "US"] {
        assert_eq!(Lang::from_name(n), Some(Lang::En), "{n}");
    }
    // The file is read line by line, so the newline comes with it.
    assert_eq!(Lang::from_name("  es\n"), Some(Lang::Es));
    assert_eq!(Lang::from_name("\ten_US "), Some(Lang::En));
    for n in ["", "de", "es-ES", "english", "e"] {
        assert_eq!(Lang::from_name(n), None, "{n}");
    }
}

/// `$LANG` is English only for the three tags that name English, and the tag
/// is what is left after the charset, the modifier and the separator the
/// variable can carry.
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
/// comments and blanks skipped. `eclipse-locale` writes the `lang=` form; the
/// bare one is what a hand-edited file looks like.
#[test]
fn the_locale_file_is_read_in_either_of_the_two_shapes_it_can_have() {
    assert_eq!(parse_locale("en\n"), Some(Lang::En));
    assert_eq!(parse_locale("lang=en\n"), Some(Lang::En));
    assert_eq!(parse_locale("  lang=es  \n"), Some(Lang::Es));
    // `eclipse-locale` writes the tag, but a file edited with the command's
    // own vocabulary says `us`, and that has to mean English here too.
    assert_eq!(parse_locale("lang=us\n"), Some(Lang::En));
    // A comment and a blank line are not the language. The comment has to be
    // skipped as a comment and not merely fall through the `lang=` and the
    // `key=` tests: one without an '=' in it would otherwise be read as a bare
    // language name, and answer None for the whole file.
    assert_eq!(parse_locale("# lang=en\n\nes\n"), Some(Lang::Es));
    assert_eq!(parse_locale("# a note\nes\n"), Some(Lang::Es));
    // A key that is not `lang=` is skipped, not read as a bare name.
    assert_eq!(parse_locale("kbd=us\nlang=en\n"), Some(Lang::En));
    assert_eq!(parse_locale(""), None);
    assert_eq!(parse_locale("# nothing but a comment\n"), None);
    // A `lang=` line naming a language nobody knows answers None rather than
    // reading on: the file said what it wanted.
    assert_eq!(parse_locale("lang=de\nes\n"), None);
}

/// The two weekday tables differ only in where the week starts, and that is
/// the whole reason there are two: the date pill counts from Sunday, the
/// calendar header from Monday. Swapping them silently relabels every column
/// of the calendar.
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
    // The Monday-first table is the Sunday-first one rotated by one, in both
    // languages: same seven days, a different first column.
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

/// Both month tables run January to December. The calendar indexes them by a
/// 0-based month straight out of `localtime`, so a pair out of order is a
/// month drawn under its neighbour's name.
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
    assert_eq!(Lang::Es.month_full()[0], "enero");
    assert_eq!(Lang::Es.month_full()[11], "diciembre");
    assert_eq!(Lang::En.month_full()[0], "January");
    assert_eq!(Lang::En.month_full()[11], "December");
    // The short form is the long one's first three letters, which is what
    // keeps the two tables in step with each other.
    for lang in [Lang::Es, Lang::En] {
        for (short, full) in lang.month_short().iter().zip(lang.month_full()) {
            let head: String = full.to_lowercase().chars().take(3).collect();
            assert_eq!(*short, head, "{full}");
        }
    }
}

/// A row's two columns take the same number of `{}` holes.
///
/// A translation that dropped the hole draws the sentence without the filename
/// in it, and one that gained a hole prints a literal `{}` on the panel. Both
/// are the kind of thing nobody sees until the desktop is running in the other
/// language, which is exactly what this file is for.
#[test]
fn every_row_fills_the_same_holes_in_both_languages() {
    for key in Key::ALL {
        let (es, en) = key.pair();
        assert_eq!(
            es.matches("{}").count(),
            en.matches("{}").count(),
            "{}: the two columns do not take the same number of `{{}}` holes",
            key.name()
        );
    }
}

/// The two languages do not write a fraction the same way.
#[test]
fn each_language_has_its_own_decimal_point() {
    assert_eq!(Lang::Es.decimal_point(), ',');
    assert_eq!(Lang::En.decimal_point(), '.');
}

/// `decimals` writes a whole number the way the language writes it, and leaves
/// the English one alone: every panel that prints a fraction goes through it,
/// so a second separator creeping in here would show up in all of them.
#[test]
fn decimals_rewrites_only_the_spanish_separator() {
    assert_eq!(Lang::Es.decimals("2.1 GiB".to_string()), "2,1 GiB");
    assert_eq!(Lang::En.decimals("2.1 GiB".to_string()), "2.1 GiB");
    // La media de carga son tres números en la misma cadena.
    assert_eq!(
        Lang::Es.decimals("0.84 0.61 0.52".to_string()),
        "0,84 0,61 0,52"
    );
    assert_eq!(
        Lang::En.decimals("0.84 0.61 0.52".to_string()),
        "0.84 0.61 0.52"
    );
    // Sin punto no cambia nada en ninguno de los dos.
    assert_eq!(Lang::Es.decimals("612 MiB".to_string()), "612 MiB");
}
