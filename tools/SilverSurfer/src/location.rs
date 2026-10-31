//! What the address bar turns into a URL.

use url::Url;

/// Parse what the user typed.
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
}
