//! Ventana privada (proceso aislado) y borrado de datos de sitio.
//!
//! Servo 0.7 no expone un flag público de private-browsing en `WebViewBuilder`.
//! La ventana `--private` es un proceso aparte que no restaura sesión y, al
//! cerrar, limpia cookies/almacenamiento/caché de esa instancia Servo.

use url::Url;

/// Página `about:privacy` (fichero temporal).
pub fn privacy_page_url() -> Url {
    let body = "\
<!doctype html><meta charset=utf-8><title>Privacidad — SilverSurfer</title>\
<style>body{font:14px/1.5 system-ui,sans-serif;margin:2rem;background:#111;color:#eee;max-width:40rem}\
code{background:#222;padding:.1rem .35rem}a{color:#7ec8ff}h1{font-size:1.35rem}</style>\
<h1>Privacidad</h1>\
<p>La <strong>ventana privada</strong> (<code>--private</code> o Ctrl+Shift+P) no\
 restaura pestañas ni escribe <code>session.json</code>. Al cerrarla se borran\
 cookies, almacenamiento web y caché HTTP de esa instancia.</p>\
<p>Servo 0.7 no aísla aún el almacenamiento privado por WebView; el proceso\
 dedicado es el aislamiento práctico en Eclipse.</p>\
<ul>\
<li><code>Ctrl+Shift+Delete</code> — borrar cookies y datos de sitio ahora</li>\
<li><a href=\"about:clear\">about:clear</a> — misma acción vía página</li>\
<li><a href=\"about:cookies\">about:cookies</a> — lista de sitios con datos</li>\
<li><a href=\"about:passwords\">about:passwords</a> — logins guardados (no en privado)</li>\
</ul>";
    write_temp("silversurfer-privacy.html", body)
}

/// Confirmación tras borrar datos.
pub fn cleared_page_url(summary: &str) -> Url {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Datos borrados — SilverSurfer</title>\
<style>body{{font:14px/1.5 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}}\
a{{color:#7ec8ff}}</style>\
<h1>Datos borrados</h1><p>{}</p>\
<p><a href=\"about:privacy\">Volver a privacidad</a> · \
<a href=\"about:home\">Inicio</a></p>",
        html_escape(summary)
    );
    write_temp("silversurfer-cleared.html", &body)
}

/// Lista de sitios (nombres eTLD+1) con datos.
pub fn cookies_page_html(sites: &[(String, String)]) -> String {
    let mut body = String::from(
        "<!doctype html><meta charset=utf-8><title>Cookies y datos — SilverSurfer</title>\
<style>body{font:14px/1.45 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}\
.t{color:#999;font-size:12px;margin-left:.5rem}li{margin:.35rem 0}\
a{color:#7ec8ff}h1{font-size:1.35rem}</style>\
<h1>Cookies y datos de sitio</h1>",
    );
    if sites.is_empty() {
        body.push_str("<p>No hay sitios con cookies ni almacenamiento local.</p>");
    } else {
        body.push_str(&format!("<p>{} sitios:</p><ul>", sites.len()));
        for (name, kinds) in sites {
            body.push_str(&format!(
                "<li>{}<span class=t>{}</span></li>",
                html_escape(name),
                html_escape(kinds)
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str(
        "<p><a href=\"about:clear\">Borrar todo</a> · \
<a href=\"about:privacy\">Privacidad</a></p>",
    );
    body
}

pub fn cookies_page_url(sites: &[(String, String)]) -> Url {
    write_temp(
        "silversurfer-cookies.html",
        &cookies_page_html(sites),
    )
}

fn write_temp(name: &str, body: &str) -> Url {
    let path = std::env::temp_dir().join(name);
    let _ = std::fs::write(&path, body);
    Url::from_file_path(&path).expect("el directorio temporal es absoluto")
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_page_is_file() {
        let url = privacy_page_url();
        assert_eq!(url.scheme(), "file");
        let body = std::fs::read_to_string(url.to_file_path().unwrap()).unwrap();
        assert!(body.contains("ventana privada"));
        assert!(body.contains("Ctrl+Shift+Delete"));
    }

    #[test]
    fn cookies_page_lists_sites() {
        let html = cookies_page_html(&[
            ("example.com".into(), "cookies".into()),
            ("servo.org".into(), "local".into()),
        ]);
        assert!(html.contains("example.com"));
        assert!(html.contains("servo.org"));
        assert!(html.contains("2 sitios"));
    }

    #[test]
    fn cleared_page_escapes() {
        let url = cleared_page_url("Borrados: 3 sitios <test>");
        let body = std::fs::read_to_string(url.to_file_path().unwrap()).unwrap();
        assert!(body.contains("&lt;test&gt;"));
        assert!(!body.contains("<test>"));
    }
}
