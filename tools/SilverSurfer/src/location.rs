//! What the address bar turns into a URL — and when it becomes a search.

use url::Url;

/// Default DuckDuckGo HTML search. `{query}` is replaced with a percent-encoded
/// query. Override with `$SILVERSURFER_SEARCH`.
pub const DEFAULT_SEARCH_TEMPLATE: &str = "https://duckduckgo.com/?q={query}";

/// Parse what the user typed as a location only (no search fallback).
///
/// A path that starts with `/` is a local file. Anything that already has a
/// scheme (`https://`, `file:`, `about:`, `data:`) is used as written. A bare
/// host gets `https://`.
pub fn parse_location(input: &str) -> Result<Url, ()> {
    let text = input.trim();
    if text.is_empty() {
        return Err(());
    }
    // A scheme-relative URL (`//host/path`) is https. It has to be decided
    // before the file-path rule: `//host` also starts with `/`. Three slashes
    // (`///etc/passwd`) are still a local file.
    if let Some(rest) = text.strip_prefix("//") {
        if rest.starts_with('/') {
            return Url::from_file_path(rest).map_err(|_| ());
        }
        return Url::parse(&format!("https://{rest}")).map_err(|_| ());
    }
    if text.starts_with('/') {
        return Url::from_file_path(text).map_err(|_| ());
    }
    if let Some((scheme, rest)) = text.split_once(':') {
        let scheme_ok = !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic());
        let known = matches!(scheme.to_ascii_lowercase().as_str(), "about" | "data" | "mailto");
        // `file:/tmp/a`, `http:/host` and `About:blank`. `localhost:8080` is
        // not a scheme: the part after the colon does not start a path.
        if scheme_ok && (known || rest.starts_with('/') || rest.starts_with("//")) {
            return Url::parse(text).map_err(|_| ());
        }
    }
    Url::parse(&format!("https://{text}")).map_err(|_| ())
}

/// Plantilla de búsqueda: `$SILVERSURFER_SEARCH` o [`DEFAULT_SEARCH_TEMPLATE`].
pub fn search_template() -> String {
    std::env::var("SILVERSURFER_SEARCH").unwrap_or_else(|_| DEFAULT_SEARCH_TEMPLATE.to_string())
}

/// Decide si el texto se navega o se busca (comportamiento tipo omnibox).
///
/// - Esquema, `about:`, `data:`, ruta absoluta o `//host` → ubicación.
/// - Espacios → búsqueda.
/// - Una sola etiqueta sin `.` ni `:` (p. ej. `hola`) → búsqueda.
/// - Host con punto, puerto o `localhost` → `https://…`.
pub fn resolve_omnibox(input: &str, search_template: &str) -> Result<Url, ()> {
    let text = input.trim();
    if text.is_empty() {
        return Err(());
    }
    if looks_like_location(text) {
        parse_location(text)
    } else {
        build_search(text, search_template)
    }
}

fn looks_like_location(text: &str) -> bool {
    if text.starts_with('/') || text.starts_with("//") {
        return true;
    }
    if text.contains("://") || text.starts_with("about:") || text.starts_with("data:") {
        return true;
    }
    if text.chars().any(char::is_whitespace) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if lower == "localhost"
        || lower.starts_with("localhost:")
        || lower.starts_with("localhost/")
    {
        return true;
    }
    text.contains('.') || text.contains(':')
}

fn build_search(query: &str, template: &str) -> Result<Url, ()> {
    let encoded = percent_encode_query(query);
    let url = if template.contains("{query}") {
        template.replace("{query}", &encoded)
    } else if template.contains("{}") {
        template.replace("{}", &encoded)
    } else {
        format!("{template}{encoded}")
    };
    Url::parse(&url).map_err(|_| ())
}

fn percent_encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for &b in text.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Esquema que muestra el cromo junto a la barra.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemeHint {
    Https,
    Http,
    File,
    Other,
}

impl SchemeHint {
    pub fn from_url_str(url: &str) -> Self {
        match Url::parse(url).ok().as_ref().map(Url::scheme) {
            Some("https") => Self::Https,
            Some("http") => Self::Http,
            Some("file") => Self::File,
            _ => Self::Other,
        }
    }

    /// Etiqueta ASCII corta para FONT_6X10.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Https => "s",
            Self::Http => "!",
            Self::File => "/",
            Self::Other => "·",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_host_becomes_https() {
        let url = parse_location("  servo.org/demo ").unwrap();
        assert_eq!(url.as_str(), "https://servo.org/demo");
    }

    #[test]
    fn an_explicit_scheme_is_kept() {
        let url = parse_location("http://example.com").unwrap();
        assert_eq!(url.scheme(), "http");
    }

    #[test]
    fn an_absolute_path_is_a_file() {
        let url = parse_location("/etc/eclipse/look").unwrap();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("/etc/eclipse/look"));
    }

    #[test]
    fn empty_is_rejected() {
        assert!(parse_location("   ").is_err());
        assert!(parse_location("https://").is_err());
        assert!(parse_location("not a url").is_err());
    }

    #[test]
    fn about_and_data_are_kept() {
        assert_eq!(parse_location("about:blank").unwrap().as_str(), "about:blank");
        let data = parse_location("data:text/plain,hola").unwrap();
        assert_eq!(data.scheme(), "data");
    }

    #[test]
    fn a_scheme_relative_url_is_https() {
        let url = parse_location("//example.com/a").unwrap();
        assert_eq!(url.as_str(), "https://example.com/a");
    }

    #[test]
    fn a_file_url_is_kept() {
        let url = parse_location("file:///tmp/pagina.html").unwrap();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("pagina.html"));
    }

    #[test]
    fn a_scheme_with_one_slash_is_not_https() {
        let file = parse_location("file:/tmp/a.html").unwrap();
        assert_eq!(file.scheme(), "file");
        assert!(file.path().ends_with("/tmp/a.html"));
        let http = parse_location("http:/ejemplo.com").unwrap();
        assert_eq!(http.scheme(), "http");
        assert_eq!(http.host_str(), Some("ejemplo.com"));
        assert_eq!(parse_location("About:blank").unwrap().as_str(), "about:blank");
    }

    #[test]
    fn three_slashes_are_a_local_file() {
        let url = parse_location("///etc/passwd").unwrap();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("/etc/passwd"));
    }

    #[test]
    fn a_port_on_a_bare_host_stays_https() {
        let url = parse_location("localhost:8080").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.port(), Some(8080));
    }

    #[test]
    fn omnibox_searches_words_and_navigates_hosts() {
        let tpl = DEFAULT_SEARCH_TEMPLATE;
        let search = resolve_omnibox("hola mundo", tpl).unwrap();
        assert_eq!(search.scheme(), "https");
        assert!(search.as_str().contains("duckduckgo.com"));
        assert!(search.as_str().contains("hola%20mundo"));

        let single = resolve_omnibox("firefox", tpl).unwrap();
        assert!(single.as_str().contains("duckduckgo.com"));
        assert!(single.as_str().contains("firefox"));

        let host = resolve_omnibox("servo.org/demo", tpl).unwrap();
        assert_eq!(host.as_str(), "https://servo.org/demo");

        let https = resolve_omnibox("https://example.com", tpl).unwrap();
        assert_eq!(https.as_str(), "https://example.com/");

        let path = resolve_omnibox("/tmp/x.html", tpl).unwrap();
        assert_eq!(path.scheme(), "file");

        let local = resolve_omnibox("localhost:8080", tpl).unwrap();
        assert_eq!(local.as_str(), "https://localhost:8080/");
    }

    #[test]
    fn omnibox_honours_custom_template_placeholder() {
        let url = resolve_omnibox("a b", "https://example.test/search?q={query}").unwrap();
        assert_eq!(url.as_str(), "https://example.test/search?q=a%20b");
        let url = resolve_omnibox("x", "https://example.test/?q={}").unwrap();
        assert_eq!(url.as_str(), "https://example.test/?q=x");
    }

    #[test]
    fn scheme_hint_reads_the_url() {
        assert_eq!(
            SchemeHint::from_url_str("https://a"),
            SchemeHint::Https
        );
        assert_eq!(SchemeHint::from_url_str("http://a"), SchemeHint::Http);
        assert_eq!(
            SchemeHint::from_url_str("file:///tmp/a"),
            SchemeHint::File
        );
        assert_eq!(SchemeHint::from_url_str("about:blank"), SchemeHint::Other);
        assert_eq!(SchemeHint::Https.badge(), "s");
        assert_eq!(SchemeHint::Http.badge(), "!");
    }

    #[test]
    fn empty_omnibox_is_rejected() {
        assert!(resolve_omnibox("  ", DEFAULT_SEARCH_TEMPLATE).is_err());
    }
}
