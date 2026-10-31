//! Expande `about:*` a ficheros locales que Servo sí puede cargar.

use url::Url;

use crate::{bookmarks, downloads, filter, history, home, passwords, privacy};

/// Resuelve páginas internas. Lo demás se deja igual.
pub fn expand(url: Url) -> Url {
    if url.scheme() != "about" {
        return url;
    }
    match url.path().to_ascii_lowercase().as_str() {
        "blank" => url,
        "" | "home" | "newtab" | "start" => home::home_url(),
        "history" => history::page_url(),
        "bookmarks" | "favorites" => bookmarks::page_url(),
        "downloads" => downloads::page_url(),
        "privacy" | "private" => privacy::privacy_page_url(),
        "filters" | "adblock" => filter::status_page_url(0),
        "passwords" | "logins" => passwords::page_url(),
        // clear / cookies se resuelven en el motor (necesitan SiteDataManager).
        _ => url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_stays_about() {
        let url = expand(Url::parse("about:blank").unwrap());
        assert_eq!(url.as_str(), "about:blank");
    }

    #[test]
    fn history_and_bookmarks_become_files() {
        let h = expand(Url::parse("about:history").unwrap());
        assert_eq!(h.scheme(), "file");
        assert!(h.path().ends_with("silversurfer-history.html"));
        let b = expand(Url::parse("about:bookmarks").unwrap());
        assert_eq!(b.scheme(), "file");
        assert!(b.path().ends_with("silversurfer-bookmarks.html"));
    }

    #[test]
    fn home_aliases() {
        let url = expand(Url::parse("about:home").unwrap());
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-home.html"));
    }

    #[test]
    fn downloads_becomes_file() {
        let url = expand(Url::parse("about:downloads").unwrap());
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-downloads.html"));
    }

    #[test]
    fn privacy_becomes_file() {
        let url = expand(Url::parse("about:privacy").unwrap());
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-privacy.html"));
    }

    #[test]
    fn passwords_becomes_file() {
        let url = expand(Url::parse("about:passwords").unwrap());
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-passwords.html"));
    }
}
