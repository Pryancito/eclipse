//! Openbox-3.6 menu XML for labwc, built from the same `.desktop` scan the
//! panel's launcher uses ([`crate::apps`]).
//!
//! labwc's root menu is hand-written XML in the image (`menu.xml`), so until
//! now an installed program only appeared in lunarbar's launcher, never in the
//! desktop's right-click menu. labwc can fill a submenu from a program's
//! stdout instead -- a *pipe menu*: `<menu id=".." label=".." execute=".."/>`,
//! whose output is wrapped in `<openbox_pipe_menu>`. This module is that
//! output, so `lunarbar --labwc-menu` is to labwc what
//! `labwc-menu-generator -p` is upstream, minus the C program and its
//! `glib-2.0` runtime: the scanner, the Desktop Entry parsing, the localized
//! `Name[xx]` keys, the `TryExec` pruning and the `OnlyShowIn` gating are
//! already here and already tested.
//!
//! Icons are emitted as the raw `Icon=` *name*, not a path: labwc resolves it
//! against the icon theme itself, exactly as it does for its own menu.

use crate::apps::AppEntry;
use crate::i18n::Lang;

/// The freedesktop main categories entries are filed under, in menu order,
/// with their Spanish and English labels. This is the "built-in directory
/// schema" an Openbox menu would otherwise need `.directory` files for.
///
/// An entry lands in the FIRST row whose category it declares, so the order
/// here decides where a program claiming several lands (`Network;Game` is a
/// game, because Game comes first): the answer must not depend on the order
/// the `.desktop` file happened to list them in.
pub const SCHEMA: &[(&str, &str, &str)] = &[
    ("Game", "Juegos", "Games"),
    ("AudioVideo", "Multimedia", "Multimedia"),
    ("Graphics", "Graficos", "Graphics"),
    ("Office", "Oficina", "Office"),
    ("Development", "Desarrollo", "Development"),
    ("Education", "Educacion", "Education"),
    ("Science", "Ciencia", "Science"),
    ("Network", "Internet", "Internet"),
    ("Settings", "Preferencias", "Settings"),
    ("System", "Sistema", "System"),
    ("Utility", "Accesorios", "Accessories"),
];

/// Label of the catch-all submenu for entries with no main category.
fn other_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "Otros",
        Lang::En => "Other",
    }
}

/// What a menu with nothing in it says. A `<separator label>` rather than an
/// item, so there is nothing to click: an empty `<menu>` is what labwc would
/// otherwise be handed, and a menu with no entries cannot be told apart from a
/// generator that crashed.
fn empty_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "Sin aplicaciones",
        Lang::En => "No applications",
    }
}

/// Group `apps` into the schema, in menu order: `(label, entries)` per
/// non-empty submenu, with the catch-all last. Entries keep the order they
/// came in, which [`crate::apps::scan_apps`] has already sorted by name.
pub fn grouped(apps: &[AppEntry], lang: Lang) -> Vec<(&'static str, Vec<&AppEntry>)> {
    let mut buckets: Vec<Vec<&AppEntry>> = vec![Vec::new(); SCHEMA.len() + 1];
    for app in apps {
        let slot = SCHEMA
            .iter()
            .position(|(fd, _, _)| app.categories.iter().any(|c| c == fd))
            .unwrap_or(SCHEMA.len());
        buckets[slot].push(app);
    }
    let mut out = Vec::new();
    for (i, bucket) in buckets.into_iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        let label = match SCHEMA.get(i) {
            Some((_, es, en)) => match lang {
                Lang::Es => *es,
                Lang::En => *en,
            },
            None => other_label(lang),
        };
        out.push((label, bucket));
    }
    out
}

/// The `<openbox_pipe_menu>` document labwc reads from our stdout.
///
/// `flat` drops the per-category submenus and lists every entry at the top
/// level: one escape hatch for a labwc build that will not nest a menu inside
/// pipe-menu output, so a broken submenu is one flag away from a working menu
/// rather than a rebuild.
pub fn pipemenu(apps: &[AppEntry], lang: Lang, flat: bool) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<openbox_pipe_menu>\n");
    if apps.is_empty() {
        s.push_str(&format!(
            "  <separator label=\"{}\"/>\n",
            esc(empty_label(lang))
        ));
        s.push_str("</openbox_pipe_menu>\n");
        return s;
    }
    if flat {
        for app in apps {
            s.push_str(&item(app, "  "));
        }
    } else {
        for (label, bucket) in grouped(apps, lang) {
            s.push_str(&format!(
                "  <menu id=\"{}\" label=\"{}\">\n",
                esc(&menu_id(label)),
                esc(label)
            ));
            for app in bucket {
                s.push_str(&item(app, "    "));
            }
            s.push_str("  </menu>\n");
        }
    }
    s.push_str("</openbox_pipe_menu>\n");
    s
}

/// Submenu id: stable, unique per submenu and valid in XML. Derived from the
/// label, so the two languages give different ids -- that is fine, the id is
/// never referenced from outside this document.
fn menu_id(label: &str) -> String {
    let mut id = String::from("lunar-apps-");
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else {
            id.push('-');
        }
    }
    id
}

/// One `<item>`: label, optional icon NAME (labwc themes it), and the Execute
/// action carrying the `Exec=` line with its field codes already stripped.
fn item(app: &AppEntry, indent: &str) -> String {
    let icon = match &app.icon {
        Some(i) if !i.is_empty() => format!(" icon=\"{}\"", esc(i)),
        _ => String::new(),
    };
    format!(
        "{indent}<item label=\"{}\"{icon}><action name=\"Execute\"><command>{}</command></action></item>\n",
        esc(&app.name),
        esc(&app.exec)
    )
}

/// XML escaping for attribute values and text. `Exec=` lines carry `&` and
/// shell quotes often enough that an unescaped document is not a corner case:
/// labwc's parser rejects the whole menu, so one bad `.desktop` file would
/// otherwise empty the menu.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // A control character is not valid XML 1.0 at all, not even
            // escaped; drop it rather than hand labwc a document it rejects.
            c if (c as u32) < 0x20 && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests;
