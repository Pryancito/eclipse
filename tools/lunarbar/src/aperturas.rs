//! The "Aperturas" page of the control panel: which program opens each kind
//! of file. The built-in table ([`crate::open::BUILTIN`]) is what comes with
//! the image; `/etc/eclipse/openers` changes it, one `ext=command` line per
//! kind, and lunarfile reads both ([`crate::open::handler_for`]).
//!
//! The page offers only the programs its block names (`valores`), and of
//! those only the ones the image has. A kind whose program is none of them
//! (`.drv`, the driver installer) is shown and left as it is. Choosing the
//! built-in program again takes the kind's line out of the file.

use std::path::Path;

use crate::open;

/// One kind of file.
#[derive(Clone, Debug, PartialEq)]
pub struct Apertura {
    /// The extension, lower case, no dot.
    pub ext: String,
    /// What the image opens it with.
    pub de_serie: Option<String>,
    /// What `/etc/eclipse/openers` says, when it says something.
    pub tuyo: Option<String>,
}

impl Apertura {
    /// The command in force.
    pub fn orden(&self) -> Option<&str> {
        self.tuyo.as_deref().or(self.de_serie.as_deref())
    }

    /// The program in force: the command's first word.
    pub fn programa(&self) -> Option<&str> {
        self.orden().and_then(open::program_of)
    }

    /// Whether the file changes what the image does.
    pub fn cambiada(&self) -> bool {
        self.tuyo.is_some() && self.tuyo != self.de_serie
    }
}

/// Every kind the image or the file names, in the image's order, then the
/// file's own.
pub fn lee(openers: &[(String, String)]) -> Vec<Apertura> {
    let mut out: Vec<Apertura> = open::BUILTIN
        .iter()
        .map(|(e, c)| Apertura {
            ext: e.to_string(),
            de_serie: Some(c.to_string()),
            tuyo: None,
        })
        .collect();
    for (ext, cmd) in openers {
        match out.iter_mut().find(|a| a.ext == *ext) {
            Some(a) => a.tuyo = Some(cmd.clone()),
            None => out.push(Apertura {
                ext: ext.clone(),
                de_serie: None,
                tuyo: Some(cmd.clone()),
            }),
        }
    }
    out
}

/// The command that opens a file with `programa`.
pub fn orden_de(programa: &str) -> String {
    format!("{programa} %f")
}

/// The text of `/etc/eclipse/openers` with `ext` opened by `orden`, or by
/// the image's program again when `orden` is `None`: every other line,
/// comments included, stays as it was, and the kind's line goes to the end.
pub fn con(text: &str, ext: &str, orden: Option<&str>) -> String {
    let mine = |l: &str| {
        let l = l.trim();
        !l.starts_with('#')
            && l.split_once('=')
                .is_some_and(|(e, _)| e.trim().trim_start_matches('.').eq_ignore_ascii_case(ext))
    };
    let mut out: String = text
        .lines()
        .filter(|l| !mine(l))
        .map(|l| format!("{l}\n"))
        .collect();
    if let Some(o) = orden {
        out.push_str(&format!("{ext}={o}\n"));
    }
    out
}

/// Whether `ext` can be written into the file: a plain extension.
pub fn ext_ok(ext: &str) -> bool {
    !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Opens `a` with `programa` (one of the page's), or as the image does when
/// that is the image's own program or `programa` is `None`, in the file at
/// `fichero`.
pub fn cambia(fichero: &Path, a: &Apertura, programa: Option<&str>) -> Result<(), String> {
    let bad = programa.is_some_and(|p| !crate::claves::palabra_ok(p));
    if !ext_ok(&a.ext) || bad {
        return Err(format!(
            ".{} → {}: no admitido",
            a.ext,
            programa.unwrap_or("")
        ));
    }
    let orden = programa.map(orden_de);
    let tuyo = orden
        .as_deref()
        .filter(|o| a.de_serie.as_deref() != Some(*o));
    let old = std::fs::read_to_string(fichero).unwrap_or_default();
    let new = con(&old, &a.ext, tuyo);
    if new == old {
        return Ok(());
    }
    if let Some(d) = fichero.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = fichero.with_extension("tmp");
    std::fs::write(&tmp, new)
        .and_then(|_| std::fs::rename(&tmp, fichero))
        .map_err(|e| format!("{}: {e}", fichero.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_changes_the_image_kind_by_kind() {
        let file = open::parse_openers("svg=eclipse-view %f\nfoo=eclipse-edit %f\n");
        let all = lee(&file);
        let svg = all.iter().find(|a| a.ext == "svg").unwrap();
        assert_eq!(svg.de_serie.as_deref(), Some("eclipse-firefox %f"));
        assert_eq!(svg.programa(), Some("eclipse-view"));
        assert!(svg.cambiada());
        let foo = all.last().unwrap();
        assert_eq!((foo.ext.as_str(), foo.de_serie.as_ref()), ("foo", None));
        let txt = all.iter().find(|a| a.ext == "txt").unwrap();
        assert!(!txt.cambiada());
        assert_eq!(all.iter().filter(|a| a.ext == "png").count(), 1);
    }

    #[test]
    fn a_line_is_set_or_taken_out_and_the_rest_stays() {
        let text = "# mine\nsvg=eclipse-view %f\n.PNG = eclipse-firefox %f\n";
        assert_eq!(
            con(text, "png", Some("eclipse-view %f")),
            "# mine\nsvg=eclipse-view %f\npng=eclipse-view %f\n"
        );
        assert_eq!(
            con(text, "svg", None),
            "# mine\n.PNG = eclipse-firefox %f\n"
        );
        assert_eq!(
            con("", "md", Some("eclipse-edit %f")),
            "md=eclipse-edit %f\n"
        );
    }

    #[test]
    fn choosing_the_image_program_again_empties_the_file_of_it() {
        let dir = std::env::temp_dir().join(format!("aperturas-{}", std::process::id()));
        let f = dir.join("openers");
        let svg = Apertura {
            ext: "svg".into(),
            de_serie: Some("eclipse-firefox %f".into()),
            tuyo: None,
        };
        cambia(&f, &svg, Some("eclipse-view")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "svg=eclipse-view %f\n"
        );
        cambia(&f, &svg, Some("eclipse-firefox")).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "");
        cambia(&f, &svg, Some("eclipse-view")).unwrap();
        cambia(&f, &svg, None).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "");
        let bad = Apertura {
            ext: "s;vg".into(),
            ..svg.clone()
        };
        assert!(cambia(&f, &bad, Some("eclipse-view")).is_err());
        assert!(cambia(&f, &svg, Some("rm -rf")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
