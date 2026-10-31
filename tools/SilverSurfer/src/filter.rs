//! Bloqueo de anuncios/trackers ligero (sin WebExtensions).
//!
//! - Lista de hosts/subcadenas embebida (EasyList-lite casera).
//! - El motor cancela la carga vía `WebResourceLoad::intercept(...).cancel()`.
//! - Opcional: UserScript que inyecta CSS `display:none` en selectores comunes.
//!
//! Desactivar con `SILVERSURFER_ADBLOCK=0`.

use url::Url;

/// Fragmentos de host o path que se bloquean (minúsculas).
const BLOCK_NEEDLES: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "googletagservices.com",
    "googletagmanager.com",
    "google-analytics.com",
    "adservice.google.",
    "pagead2.googlesyndication",
    "adsystem.com",
    "amazon-adsystem.com",
    "adsrvr.org",
    "adnxs.com",
    "ads-twitter.com",
    "ads.linkedin.com",
    "facebook.net/tr",
    "connect.facebook.net",
    "scorecardresearch.com",
    "quantserve.com",
    "hotjar.com",
    "taboola.com",
    "outbrain.com",
    "criteo.com",
    "criteo.net",
    "pubmatic.com",
    "openx.net",
    "rubiconproject.com",
    "moatads.com",
    "advertising.com",
    "adform.net",
    "adsafeprotected.com",
    "/ads?",
    "/ads/",
    "/ad/",
    "/ad-",
    "ads.js",
    "ads.min.js",
    "advertisement",
    "pagead/js",
    "gtag/js",
    "analytics.js",
];

/// CSS inyectado para ocultar contenedores típicos de anuncios.
pub const HIDE_ADS_CSS: &str = "\
iframe[id*=\"google_ads\"],iframe[src*=\"doubleclick\"],iframe[src*=\"googlesyndication\"],\
div[id^=\"div-gpt-ad\"],div[class*=\"ad-container\"],div[class*=\"ad_container\"],\
div[class*=\"ad-banner\"],div[class*=\"adsbox\"],aside[class*=\"advert\"],\
ins.adsbygoogle,[data-ad-client],[aria-label=\"Ads\"]{display:none!important;max-height:0!important;overflow:hidden!important;}";

pub fn enabled() -> bool {
    match std::env::var("SILVERSURFER_ADBLOCK") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "off" || v == "no")
        }
        Err(_) => true,
    }
}

/// `true` si la URL debe cancelarse.
pub fn should_block(url: &Url) -> bool {
    if !enabled() {
        return false;
    }
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let path = url.path().to_ascii_lowercase();
    let full = format!("{host}{path}");
    let query = url.query().unwrap_or("").to_ascii_lowercase();
    let hay = if query.is_empty() {
        full
    } else {
        format!("{full}?{query}")
    };
    BLOCK_NEEDLES.iter().any(|n| hay.contains(n))
}

/// Script que inserta [`HIDE_ADS_CSS`] una sola vez.
pub fn hide_ads_userscript() -> String {
    format!(
        "(function(){{try{{if(document.getElementById('ss-adblock'))return;\
var s=document.createElement('style');s.id='ss-adblock';\
s.textContent={css};(document.documentElement||document.head).appendChild(s);\
}}catch(e){{}}}})();",
        css = js_string_literal(HIDE_ADS_CSS)
    )
}

fn js_string_literal(s: &str) -> String {
    let mut out = String::from('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn status_page_url(blocked_hint: usize) -> Url {
    let on = if enabled() { "activo" } else { "desactivado (SILVERSURFER_ADBLOCK=0)" };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Filtro — SilverSurfer</title>\
<style>body{{font:14px/1.5 system-ui,sans-serif;margin:2rem;background:#111;color:#eee;max-width:40rem}}\
code{{background:#222;padding:.1rem .35rem}}h1{{font-size:1.35rem}}</style>\
<h1>Bloqueo de anuncios (lite)</h1>\
<p>Estado: <strong>{on}</strong>. Reglas embebidas: {n}. \
Cancelaciones de red en esta sesión (aprox.): {blocked}.</p>\
<p>No es uBlock: solo hosts/paths frecuentes + CSS hide. Sin WebExtensions.</p>",
        on = on,
        n = BLOCK_NEEDLES.len(),
        blocked = blocked_hint
    );
    let path = std::env::temp_dir().join("silversurfer-filters.html");
    let _ = std::fs::write(&path, body);
    Url::from_file_path(&path).expect("temp absoluto")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_known_ad_hosts() {
        let u = Url::parse("https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js").unwrap();
        assert!(should_block(&u));
        let ok = Url::parse("https://example.com/docs/index.html").unwrap();
        assert!(!should_block(&ok));
        let tracker = Url::parse("https://www.google-analytics.com/analytics.js").unwrap();
        assert!(should_block(&tracker));
    }

    #[test]
    fn userscript_embeds_css() {
        let s = hide_ads_userscript();
        assert!(s.contains("ss-adblock"));
        assert!(s.contains("adsbygoogle"));
    }

    #[test]
    fn status_page_is_file() {
        let url = status_page_url(3);
        assert_eq!(url.scheme(), "file");
    }
}
