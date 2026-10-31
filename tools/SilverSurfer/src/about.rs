//! Expande `about:*` a ficheros locales.

use url::Url;

use crate::{filter, home, passwords, privacy};

pub fn expand(url: Url) -> Url {
    if url.scheme() != "about" {
        return url;
    }
    match url.path().to_ascii_lowercase().as_str() {
        "blank" => url,
        "" | "home" | "newtab" | "start" => home::home_url(),
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
    fn blank_and_home() {
        assert_eq!(
            expand(Url::parse("about:blank").unwrap()).as_str(),
            "about:blank"
        );
        let home = expand(Url::parse("about:home").unwrap());
        assert_eq!(home.scheme(), "file");
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
