//! Ventana privada (proceso aislado) y borrado de datos de sitio.
//!
//! Servo 0.7 no expone un flag público de private-browsing en `WebViewBuilder`.
//! La ventana `--private` es un proceso aparte que no restaura sesión y, al
//! cerrar, limpia cookies/almacenamiento/caché de esa instancia Servo.

use url::Url;

use crate::Key;

/// Página `about:privacy` (fichero temporal).
pub fn privacy_page_url() -> Url {
    let l = crate::lang();
    let s = |k| l.s(k);
    let head = s(Key::www_privacy);
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{head} — SilverSurfer</title>\
<style>body{{font:14px/1.5 system-ui,sans-serif;margin:2rem;background:#111;color:#eee;max-width:40rem}}\
code{{background:#222;padding:.1rem .35rem}}a{{color:#7ec8ff}}h1{{font-size:1.35rem}}</style>\
<h1>{head}</h1>\
<p>{window}</p>\
<p>{servo}</p>\
<ul>\
<li><code>Ctrl+Shift+Delete</code> — {now}</li>\
<li><a href=\"about:clear\">about:clear</a> — {same}</li>\
<li><a href=\"about:cookies\">about:cookies</a> — {list}</li>\
<li><a href=\"about:passwords\">about:passwords</a> — {logins}</li>\
</ul>",
        window = s(Key::www_privacy_window),
        servo = s(Key::www_privacy_servo),
        now = s(Key::www_privacy_clear_now),
        same = s(Key::www_privacy_same_via_page),
        list = s(Key::www_privacy_site_list),
        logins = s(Key::www_privacy_logins),
    );
    write_temp("silversurfer-privacy.html", &body)
}

/// Confirmación tras borrar datos.
pub fn cleared_page_url(summary: &str) -> Url {
    let l = crate::lang();
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{head} — SilverSurfer</title>\
<style>body{{font:14px/1.5 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}}\
a{{color:#7ec8ff}}</style>\
<h1>{head}</h1><p>{summary}</p>\
<p><a href=\"about:privacy\">{back}</a> · \
<a href=\"about:home\">{home}</a></p>",
        head = l.s(Key::www_cleared),
        summary = html_escape(summary),
        back = l.s(Key::www_back_to_privacy),
        home = l.s(Key::word_home),
    );
    write_temp("silversurfer-cleared.html", &body)
}

/// Lista de sitios (nombres eTLD+1) con datos.
pub fn cookies_page_html(sites: &[(String, String)]) -> String {
    let l = crate::lang();
    let mut body = format!(
        "<!doctype html><meta charset=utf-8><title>{tab} — SilverSurfer</title>\
<style>body{{font:14px/1.45 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}}\
.t{{color:#999;font-size:12px;margin-left:.5rem}}li{{margin:.35rem 0}}\
a{{color:#7ec8ff}}h1{{font-size:1.35rem}}</style>\
<h1>{head}</h1>",
        tab = l.s(Key::www_cookies_and_data),
        head = l.s(Key::www_cookies_title),
    );
    if sites.is_empty() {
        body.push_str(&format!("<p>{}</p>", l.s(Key::www_cookies_empty)));
    } else {
        body.push_str(&format!(
            "<p>{} {}</p><ul>",
            sites.len(),
            l.s(Key::www_sites)
        ));
        for (name, kinds) in sites {
            body.push_str(&format!(
                "<li>{}<span class=t>{}</span></li>",
                html_escape(name),
                html_escape(kinds)
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str(&format!(
        "<p><a href=\"about:clear\">{}</a> · \
<a href=\"about:privacy\">{}</a></p>",
        l.s(Key::www_clear_all),
        l.s(Key::www_privacy),
    ));
    body
}

pub fn cookies_page_url(sites: &[(String, String)]) -> Url {
    write_temp("silversurfer-cookies.html", &cookies_page_html(sites))
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
        // Against the ROW, not against its Spanish column: this container's
        // `/etc/eclipse/locale` is whatever the last measurement left in it.
        assert!(body.contains(crate::lang().s(Key::www_privacy_window)));
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
        assert!(html.contains(&format!("2 {}", crate::lang().s(Key::www_sites))));
    }

    #[test]
    fn cleared_page_escapes() {
        let url = cleared_page_url("Borrados: 3 sitios <test>");
        let body = std::fs::read_to_string(url.to_file_path().unwrap()).unwrap();
        assert!(body.contains("&lt;test&gt;"));
        assert!(!body.contains("<test>"));
    }
}
