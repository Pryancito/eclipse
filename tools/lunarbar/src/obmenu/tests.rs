//! What labwc is handed. The whole point of these is that nothing here can be
//! checked on real hardware by looking at it: a document labwc's XML parser
//! rejects shows up as an EMPTY right-click menu, with no error anywhere.

use super::*;

fn app(name: &str, exec: &str, cats: &[&str]) -> AppEntry {
    with_icon(name, exec, cats, None)
}

fn with_icon(name: &str, exec: &str, cats: &[&str], icon: Option<&str>) -> AppEntry {
    AppEntry {
        name: name.into(),
        exec: exec.into(),
        icon: icon.map(str::to_string),
        categories: cats.iter().map(|c| (*c).to_string()).collect(),
    }
}

#[test]
fn the_document_is_wrapped_the_way_labwc_demands() {
    let out = pipemenu(&[app("Firefox", "firefox", &["Network"])], Lang::Es, false);
    assert!(
        out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<openbox_pipe_menu>"),
        "{out}"
    );
    assert!(out.trim_end().ends_with("</openbox_pipe_menu>"), "{out}");
}

#[test]
fn an_entry_becomes_an_execute_action_with_its_exec_line() {
    let out = pipemenu(
        &[app("Firefox", "/usr/bin/firefox", &["Network"])],
        Lang::Es,
        true,
    );
    assert!(
        out.contains(
            "<item label=\"Firefox\"><action name=\"Execute\"><command>/usr/bin/firefox</command></action></item>"
        ),
        "{out}"
    );
}

#[test]
fn the_icon_is_emitted_as_a_name_for_labwc_to_theme() {
    let a = with_icon("Firefox", "firefox", &["Network"], Some("firefox"));
    let out = pipemenu(&[a], Lang::Es, true);
    assert!(
        out.contains("<item label=\"Firefox\" icon=\"firefox\">"),
        "{out}"
    );
    // No icon at all means no attribute, NOT icon="".
    let out = pipemenu(&[app("X", "x", &[])], Lang::Es, true);
    assert!(!out.contains("icon="), "{out}");
}

#[test]
fn categories_become_submenus_in_schema_order_not_file_order() {
    let apps = [
        app("Juego", "game", &["Network", "Game"]),
        app("Navegador", "web", &["Network"]),
        app("Suelto", "x", &[]),
    ];
    let groups = grouped(&apps, Lang::Es);
    let labels: Vec<&str> = groups.iter().map(|(l, _)| *l).collect();
    // Game precedes Network in SCHEMA, so the entry claiming both is a game.
    assert_eq!(labels, vec!["Juegos", "Internet", "Otros"]);
    assert_eq!(groups[0].1.len(), 1);
    assert_eq!(groups[0].1[0].name, "Juego");
    assert_eq!(groups[2].1[0].name, "Suelto");
}

#[test]
fn an_empty_category_is_not_an_empty_submenu() {
    let out = pipemenu(&[app("Navegador", "web", &["Network"])], Lang::Es, false);
    assert!(out.contains("label=\"Internet\""), "{out}");
    // Exactly one submenu: the ten unused schema rows are not in the document.
    assert_eq!(out.matches("<menu ").count(), 1, "{out}");
}

#[test]
fn flat_mode_has_no_submenus_at_all() {
    let apps = [app("A", "a", &["Game"]), app("B", "b", &["Network"])];
    let out = pipemenu(&apps, Lang::Es, true);
    assert!(!out.contains("<menu "), "{out}");
    assert_eq!(out.matches("<item ").count(), 2, "{out}");
}

#[test]
fn nothing_installed_says_so_instead_of_handing_labwc_an_empty_menu() {
    for (lang, text) in [
        (Lang::Es, "Sin aplicaciones"),
        (Lang::En, "No applications"),
    ] {
        let out = pipemenu(&[], lang, false);
        assert!(
            out.contains(&format!("<separator label=\"{text}\"/>")),
            "{out}"
        );
        assert!(!out.contains("<item"), "{out}");
    }
}

#[test]
fn the_labels_follow_the_session_language() {
    let apps = [app("A", "a", &["Utility"])];
    assert!(pipemenu(&apps, Lang::Es, false).contains("label=\"Accesorios\""));
    assert!(pipemenu(&apps, Lang::En, false).contains("label=\"Accessories\""));
}

#[test]
fn an_ampersand_in_an_exec_line_is_escaped_not_pasted_in() {
    // `Exec=sh -c 'a && b'` with a `<` in the name: unescaped, labwc's parser
    // rejects the DOCUMENT, so this one .desktop file would empty the menu.
    let out = pipemenu(&[app("A & B <x>", "sh -c 'a && b'", &[])], Lang::Es, true);
    assert!(out.contains("label=\"A &amp; B &lt;x&gt;\""), "{out}");
    assert!(
        out.contains("<command>sh -c &apos;a &amp;&amp; b&apos;</command>"),
        "{out}"
    );
    assert!(!out.contains("&& b</command>"), "{out}");
}

#[test]
fn a_control_character_is_dropped_because_xml_cannot_carry_it_even_escaped() {
    let out = pipemenu(&[app("A\u{1}B", "x", &[])], Lang::Es, true);
    assert!(out.contains("label=\"AB\""), "{out}");
    assert!(!out.contains('\u{1}'), "{out}");
}

#[test]
fn submenu_ids_are_unique_and_xml_safe() {
    let apps = [app("A", "a", &["Game"]), app("B", "b", &["AudioVideo"])];
    let out = pipemenu(&apps, Lang::Es, false);
    let ids: Vec<&str> = out
        .lines()
        .filter_map(|l| l.split("id=\"").nth(1))
        .map(|r| r.split('"').next().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{out}");
    assert_ne!(ids[0], ids[1]);
    for id in ids {
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{id}"
        );
    }
}
