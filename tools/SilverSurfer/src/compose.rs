//! Teclas muertas y secuencias de composición (´ + a → á) con xkb compose.
//!
//! La tabla sale de la locale (`LC_ALL`, `LC_CTYPE`, `LANG`), como en el
//! resto del escritorio. Si la imagen no trae `/usr/share/X11/locale`, se usa
//! una tabla mínima con las teclas muertas de los teclados latinos: tildes,
//! diéresis, circunflejo, grave, virgulilla y cedilla.

use std::ffi::OsString;

use xkbcommon::xkb::{self, compose};

/// Lo que hace una pulsación al pasar por el compositor de secuencias.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No forma parte de ninguna secuencia: la tecla sigue su camino normal.
    Pass,
    /// Tecla muerta o secuencia a medias (o anulada): no escribe nada.
    Swallow,
    /// La secuencia terminó y produce este texto.
    Text(String),
}

pub struct Composer {
    state: compose::State,
}

impl Composer {
    /// Tabla de la locale; si no existe, la tabla mínima incluida.
    pub fn new() -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let table = compose::Table::new_from_locale(&context, &locale(), compose::COMPILE_NO_FLAGS)
            .or_else(|()| fallback_table(&context))
            .ok()?;
        Some(Self {
            state: compose::State::new(&table, compose::STATE_NO_FLAGS),
        })
    }

    /// Tabla mínima, sin mirar la locale (pruebas y respaldo).
    pub fn fallback() -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let table = fallback_table(&context).ok()?;
        Some(Self {
            state: compose::State::new(&table, compose::STATE_NO_FLAGS),
        })
    }

    pub fn feed(&mut self, keysym: xkb::Keysym) -> Outcome {
        if self.state.feed(keysym) == compose::FeedResult::Ignored {
            // Modificadores y similares: no cortan una secuencia en curso.
            return Outcome::Pass;
        }
        match self.state.status() {
            compose::Status::Nothing => Outcome::Pass,
            compose::Status::Composing => Outcome::Swallow,
            compose::Status::Cancelled => {
                self.state.reset();
                Outcome::Swallow
            }
            compose::Status::Composed => {
                let text = self.state.utf8().unwrap_or_default();
                self.state.reset();
                if text.is_empty() {
                    Outcome::Swallow
                } else {
                    Outcome::Text(text)
                }
            }
        }
    }

    pub fn reset(&mut self) {
        self.state.reset();
    }
}

fn locale() -> OsString {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .unwrap_or_else(|| "C".into())
}

fn fallback_table(context: &xkb::Context) -> Result<compose::Table, ()> {
    compose::Table::new_from_buffer(
        context,
        fallback_source(),
        "C",
        compose::FORMAT_TEXT_V1,
        compose::COMPILE_NO_FLAGS,
    )
}

/// Secuencias de la tabla mínima en formato Compose de X11.
pub fn fallback_source() -> String {
    // (tecla muerta, carácter suelto, pares base → resultado)
    const DEAD: &[(&str, &str, &str)] = &[
        (
            "dead_acute",
            "´",
            "aá eé ií oó uú yý AÁ EÉ IÍ OÓ UÚ YÝ cć CĆ nń NŃ sś SŚ zź ZŹ",
        ),
        ("dead_grave", "`", "aà eè iì oò uù AÀ EÈ IÌ OÒ UÙ"),
        ("dead_diaeresis", "¨", "aä eë iï oö uü yÿ AÄ EË IÏ OÖ UÜ"),
        ("dead_circumflex", "^", "aâ eê iî oô uû AÂ EÊ IÎ OÔ UÛ"),
        ("dead_tilde", "~", "aã oõ nñ AÃ OÕ NÑ"),
        ("dead_cedilla", "¸", "cç CÇ"),
    ];
    let mut out = String::new();
    for (dead, alone, pairs) in DEAD {
        for pair in pairs.split_whitespace() {
            let mut chars = pair.chars();
            let (Some(base), Some(result)) = (chars.next(), chars.next()) else {
                continue;
            };
            out.push_str(&format!("<{dead}> <{base}> : \"{result}\"\n"));
        }
        out.push_str(&format!("<{dead}> <space> : \"{alone}\"\n"));
        out.push_str(&format!("<{dead}> <{dead}> : \"{alone}\"\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str) -> xkb::Keysym {
        xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS)
    }

    #[test]
    fn dead_acute_then_vowel_gives_accented_vowel() {
        let mut c = Composer::fallback().expect("tabla mínima");
        assert_eq!(c.feed(sym("dead_acute")), Outcome::Swallow);
        assert_eq!(c.feed(sym("a")), Outcome::Text("á".into()));
        assert_eq!(c.feed(sym("a")), Outcome::Pass);
    }

    #[test]
    fn dead_key_then_space_gives_the_accent() {
        let mut c = Composer::fallback().expect("tabla mínima");
        c.feed(sym("dead_diaeresis"));
        assert_eq!(c.feed(sym("space")), Outcome::Text("¨".into()));
        c.feed(sym("dead_tilde"));
        assert_eq!(c.feed(sym("N")), Outcome::Text("Ñ".into()));
    }

    #[test]
    fn shift_does_not_break_a_sequence() {
        let mut c = Composer::fallback().expect("tabla mínima");
        c.feed(sym("dead_acute"));
        assert_eq!(c.feed(sym("Shift_L")), Outcome::Pass);
        assert_eq!(c.feed(sym("E")), Outcome::Text("É".into()));
    }

    #[test]
    fn unknown_pair_cancels_without_text() {
        let mut c = Composer::fallback().expect("tabla mínima");
        c.feed(sym("dead_acute"));
        assert_eq!(c.feed(sym("x")), Outcome::Swallow);
        assert_eq!(c.feed(sym("x")), Outcome::Pass);
    }
}
