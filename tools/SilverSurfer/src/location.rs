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
    // before the file-path rule: `//host` also starts with `/`.
    if let Some(rest) = text.strip_prefix("//") {
        return Url::parse(&format!("https://{rest}")).map_err(|_| ());
    }
    if text.starts_with('/') {
        return Url::from_file_path(text).map_err(|_| ());
    }
    if text.contains("://") || text.starts_with("about:") || text.starts_with("data:") {
        return Url::parse(text).map_err(|_| ());
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
}
