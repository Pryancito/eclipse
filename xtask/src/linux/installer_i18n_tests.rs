//! La tabla de cadenas del instalador, que es un fichero de C y por tanto no
//! la mira ni el compilador de Rust ni los tests de `eclipse-i18n`.
//!
//! `tools/install-eclipse/i18n.h` tiene la misma forma que la tabla del crate:
//! una fila por cadena, con la columna castellana y la inglesa. Lo que aquí se
//! comprueba es lo que en C no da ni un aviso y se nota en mitad de una
//! instalación:
//!
//! - Una columna con un `%s` de menos (o de más) que la otra. `log` es
//!   variádico: el idioma decide cuántos argumentos se leen de la pila, así que
//!   una fila descuadrada es una lectura fuera de sitio **solo en uno de los
//!   dos idiomas**, y el otro pasa la prueba.
//! - Una fila sin traducir, con las dos columnas iguales. Para eso está `XB`,
//!   que es la forma de decir «es el mismo texto en los dos» a propósito.
//! - Una frase en castellano que se quedó en el código en vez de pasar por la
//!   tabla.

use crate::PROJECT_DIR;
use std::fs;

fn header() -> String {
    fs::read_to_string(PROJECT_DIR.join("tools/install-eclipse/i18n.h")).unwrap()
}

fn source() -> String {
    fs::read_to_string(PROJECT_DIR.join("tools/install-eclipse/install-eclipse.c")).unwrap()
}

/// Los argumentos de una llamada de macro que empieza en `at` (el índice de su
/// paréntesis de apertura), cortados por las comas de primer nivel. Las comas
/// y los paréntesis que van dentro de una cadena no cuentan.
fn macro_args(text: &str, at: usize) -> Vec<String> {
    let bytes: Vec<char> = text[at + 1..].chars().collect();
    let mut args = vec![String::new()];
    let mut depth = 0usize;
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == '\\' {
                args.last_mut().unwrap().push(c);
                i += 1;
                if i < bytes.len() {
                    args.last_mut().unwrap().push(bytes[i]);
                    i += 1;
                }
                continue;
            }
            if c == '"' {
                in_str = false;
            }
        } else {
            match c {
                '"' => in_str = true,
                '(' => depth += 1,
                ')' if depth == 0 => return args,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    args.push(String::new());
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        args.last_mut().unwrap().push(c);
        i += 1;
    }
    panic!("llamada de macro sin cerrar en {at}");
}

/// Lo que diría el texto ya concatenado: el contenido de cada literal de la
/// expresión, uno detrás de otro. `PRIu64` se cuenta como su expansión para
/// que `%" PRIu64 "` se mida igual que un `%llu` escrito a mano.
fn joined(expr: &str) -> String {
    let expr = expr.replace("PRIu64", "\"llu\"");
    let chars: Vec<char> = expr.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        i += 1;
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' && i + 1 < chars.len() {
                out.push(chars[i]);
                i += 1;
            }
            out.push(chars[i]);
            i += 1;
        }
        i += 1;
    }
    out
}

/// Las conversiones de `printf` del texto, en orden: `%s`, `%d`, `%llu`...
/// Un `%%` no es una conversión y no lee nada de la pila.
fn conversions(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            i += 1;
            continue;
        }
        i += 1;
        if i < chars.len() && chars[i] == '%' {
            i += 1;
            continue;
        }
        let mut spec = String::from("%");
        while i < chars.len() && !chars[i].is_ascii_alphabetic() {
            spec.push(chars[i]);
            i += 1;
        }
        while i < chars.len() && "hlLzjt".contains(chars[i]) {
            spec.push(chars[i]);
            i += 1;
        }
        if i < chars.len() {
            spec.push(chars[i]);
            i += 1;
        }
        out.push(spec);
    }
    out
}

/// Cada fila de la tabla: la clave, el castellano y el inglés. Una fila `XB`
/// devuelve el mismo texto en las dos columnas, que es lo que significa.
fn rows() -> Vec<(String, String, String)> {
    let header = header();
    let mut out = Vec::new();
    for (name, args) in [("\n    X(", 3), ("\n    XB(", 2)] {
        let mut from = 0;
        while let Some(found) = header[from..].find(name) {
            let at = from + found + name.len() - 1;
            let parts = macro_args(&header, at);
            assert_eq!(
                parts.len(),
                args,
                "la fila de {name} en {at} tiene {} argumentos",
                parts.len()
            );
            let key = parts[0].trim().to_string();
            let es = joined(&parts[1]);
            let en = joined(parts.last().unwrap());
            out.push((key, es, en));
            from = at;
        }
    }
    assert!(out.len() > 150, "solo {} filas", out.len());
    out
}

/// Una fila `X` con las dos columnas iguales es una fila sin traducir: el
/// texto se escribió una vez y se copió en la otra columna para que
/// compilara. Cuando la igualdad es a propósito, la fila se escribe con `XB`,
/// que es una palabra menos y además se ve en el diff.
#[test]
fn every_translated_row_of_the_installer_says_something_different() {
    let header = header();
    let mut checked = 0;
    let mut from = 0;
    while let Some(found) = header[from..].find("\n    X(") {
        let at = from + found + "\n    X(".len() - 1;
        let parts = macro_args(&header, at);
        let key = parts[0].trim();
        let es = joined(&parts[1]);
        let en = joined(&parts[2]);
        assert!(
            !es.is_empty() && !en.is_empty(),
            "{key} tiene una columna vacía"
        );
        assert_ne!(
            es, en,
            "{key} dice lo mismo en los dos idiomas; si es a propósito, va con XB"
        );
        checked += 1;
        from = at;
    }
    assert!(checked > 150, "solo {checked} filas traducidas");
}

/// `log` es variádico y el idioma elige la columna, así que la columna decide
/// cuántos argumentos se leen de la pila. Una columna con un `%s` de menos lee
/// un argumento que no se pasó, y solo en uno de los dos idiomas: el otro pasa
/// la instalación entera sin enterarse.
#[test]
fn both_columns_of_a_row_read_the_same_arguments() {
    for (key, es, en) in rows() {
        assert_eq!(
            conversions(&es),
            conversions(&en),
            "{key} no lee los mismos argumentos en los dos idiomas:\n  es: {es}\n  en: {en}"
        );
    }
}

/// Dos filas con el mismo castellano son una fila muerta: la segunda la
/// escribió alguien que no vio la primera, y a partir de ahí hay dos sitios
/// donde cambiar la misma frase.
#[test]
fn no_two_rows_of_the_installer_say_the_same_thing() {
    let rows = rows();
    for (i, (key, es, _)) in rows.iter().enumerate() {
        for (other, other_es, _) in &rows[i + 1..] {
            assert_ne!(es, other_es, "{key} y {other} son la misma frase: {es:?}");
        }
    }
}

/// Lo que la tabla no puede comprobar de sí misma: una frase que se quedó en
/// el código. Se miran las cadenas del `.c` (sin comentarios, que van en
/// castellano a propósito) y se busca una letra que solo sale en castellano.
#[test]
fn the_installer_has_no_spanish_sentence_left_outside_the_table() {
    // Lo que no es texto de pantalla: la cabecera de /etc/fstab, que es un
    // fichero del sistema instalado y se lee igual en cualquier idioma.
    const PERMITIDAS: &[&str] = &["# /etc/fstab - generado por install-eclipse"];

    let source = strip_comments(&source());
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    let mut left = Vec::new();
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        i += 1;
        let mut lit = String::new();
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' && i + 1 < chars.len() {
                i += 1;
                lit.push(chars[i]);
                i += 1;
                continue;
            }
            lit.push(chars[i]);
            i += 1;
        }
        i += 1;
        if lit.chars().any(|c| "áéíóúüñ¿¡ÁÉÍÓÚÑ".contains(c))
            && !PERMITIDAS.iter().any(|ok| lit.contains(ok))
        {
            left.push(lit);
        }
    }
    assert!(
        left.is_empty(),
        "estas frases no pasan por la tabla: {left:#?}"
    );
}

/// Los comentarios del instalador van en castellano a propósito, así que hay
/// que quitarlos antes de buscar castellano en las cadenas.
fn strip_comments(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            out.push(chars[i]);
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    out.push(chars[i]);
                    i += 1;
                }
                out.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }
        if chars[i] == '\'' {
            out.push(chars[i]);
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    out.push(chars[i]);
                    i += 1;
                }
                out.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Una traducción cambiada en `i18n.h` deja el binario «al día» y la imagen
/// con el texto viejo, sin una línea en ningún registro: se lee como que el
/// cambio nunca ocurrió. Por eso el rebuild mira también la cabecera.
#[test]
fn the_installer_rebuilds_when_only_its_strings_change() {
    let mod_rs = fs::read_to_string(PROJECT_DIR.join("xtask/src/linux/mod.rs")).unwrap();
    let at = mod_rs.find("fn install_eclipse(").unwrap();
    let body = &mod_rs[at..at + 1200];
    assert!(
        body.contains("i18n.h"),
        "install_eclipse no mira i18n.h para decidir si recompila"
    );
}
