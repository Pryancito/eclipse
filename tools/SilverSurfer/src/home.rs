//! The start page, written next to the process so Servo can load it as a file.

use url::Url;

const HTML: &str = include_str!("home.html");

pub fn home_url() -> Url {
    let path = std::env::temp_dir().join("silversurfer-home.html");
    let _ = std::fs::write(&path, HTML);
    Url::from_file_path(&path).expect("el directorio temporal es absoluto")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_page_covers_script_media_and_hls() {
        assert!(HTML.contains("<script"));
        assert!(HTML.contains("<video"));
        assert!(HTML.contains("<audio"));
        assert!(HTML.contains(".m3u8"));
        assert!(HTML.contains("webgl"));
        assert!(HTML.contains("gpu"));
        let url = home_url();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-home.html"));
        let body = std::fs::read_to_string(url.to_file_path().unwrap()).unwrap();
        assert!(body.contains("<video"));
        assert!(body.contains(".m3u8"));
    }
}
