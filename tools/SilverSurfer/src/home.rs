//! The start page, written next to the process so Servo can load it as a file.
//!
//! Welcome is a blank canvas with the Eclipse OS distributor logo. The older
//! JS / WebGL / media smokes live in [`smoke_url`] (`smoke.html`) so they are
//! still available without crowding the home page.

use crate::look::Look;
use url::Url;

const HTML_TEMPLATE: &str = include_str!("home.html");
const LOGO_SVG: &str = include_str!("eclipse-logo.svg");
const SMOKE_HTML: &str = include_str!("smoke.html");

fn hex(rgb: crate::look::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)
}

fn render_home(look: Look) -> String {
    let bg = hex(look.palette().panel);
    HTML_TEMPLATE
        .replace("{{BG}}", &bg)
        .replace("{{LOGO}}", LOGO_SVG.trim())
}

/// Blank welcome page with the Eclipse OS logo, tinted to the current look.
pub fn home_url() -> Url {
    let path = std::env::temp_dir().join("silversurfer-home.html");
    let html = render_home(Look::current());
    let _ = std::fs::write(&path, html);
    Url::from_file_path(&path).expect("el directorio temporal es absoluto")
}

/// Local / network capability smokes that used to live on the home page.
pub fn smoke_url() -> Url {
    let path = std::env::temp_dir().join("silversurfer-smoke.html");
    let _ = std::fs::write(&path, SMOKE_HTML);
    Url::from_file_path(&path).expect("el directorio temporal es absoluto")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_page_is_a_blank_canvas_with_the_eclipse_logo() {
        let html = render_home(Look::Eclipse);
        assert!(!html.contains("{{BG}}"));
        assert!(!html.contains("{{LOGO}}"));
        assert!(html.contains("#0b1220"), "eclipse panel colour: {html}");
        assert!(html.contains("aria-label=\"Eclipse OS\"") || html.contains("<title>Eclipse OS</title>"));
        assert!(html.contains("viewBox=\"0 0 48 48\""));
        assert!(html.contains("<circle"));
        assert!(html.contains("#aa00aa"));
        assert!(!html.contains("<video"));
        assert!(!html.contains("webgl"));
        assert!(!html.contains("Smokes locales"));
        let url = home_url();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-home.html"));
        let body = std::fs::read_to_string(url.to_file_path().unwrap()).unwrap();
        assert!(body.contains("Eclipse OS"));
        assert!(body.contains("<svg"));
    }

    #[test]
    fn the_start_page_follows_the_active_look_palette() {
        let kde = render_home(Look::Kde);
        assert!(kde.contains("#2a2e32"), "{kde}");
        let win = render_home(Look::Win11);
        assert!(win.contains("#2b2b2b"), "{win}");
    }

    #[test]
    fn smoke_page_still_covers_script_media_and_hls() {
        assert!(SMOKE_HTML.contains("<script"));
        assert!(SMOKE_HTML.contains("<video"));
        assert!(SMOKE_HTML.contains("<audio"));
        assert!(SMOKE_HTML.contains(".m3u8"));
        assert!(SMOKE_HTML.contains("webgl"));
        assert!(SMOKE_HTML.contains("gpu"));
        assert!(SMOKE_HTML.contains("AudioContext") || SMOKE_HTML.contains("webkitAudioContext"));
        assert!(SMOKE_HTML.contains("Smokes locales"));
        let url = smoke_url();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-smoke.html"));
        let body = std::fs::read_to_string(url.to_file_path().unwrap()).unwrap();
        assert!(body.contains("<video"));
        assert!(body.contains(".m3u8"));
        assert!(body.contains("webaudio") || body.contains("AudioContext"));
    }
}
