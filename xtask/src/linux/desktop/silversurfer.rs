//! SilverSurfer: wrapper, `.desktop` and default http(s) handler.
//!
//! The binary itself is optional at image-build time (Servo + GStreamer are
//! heavy and dynamically linked). The wrapper falls back to `eclipse-firefox`
//! when `silversurfer` is missing, so the menu row is never dead.

use std::{fs, path::Path};

/// `/usr/local/bin/eclipse-browser`: prefer SilverSurfer, else Firefox.
pub(crate) fn write_silversurfer_wrapper(rootfs: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let wrapper = localbin.join("eclipse-browser");
    fs::write(
        &wrapper,
        b"#!/bin/sh\n\
          # Eclipse OS: native browser when present, Firefox otherwise.\n\
          export HOME=\"${HOME:-/root}\"\n\
          if command -v silversurfer >/dev/null 2>&1; then\n\
          \x20 exec silversurfer \"$@\"\n\
          fi\n\
          exec /usr/local/bin/eclipse-firefox \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Launcher entry + MIME so labwc / lunarrun / xdg-open find SilverSurfer.
pub(crate) fn write_silversurfer_desktop_entry(rootfs: &Path) {
    let dir = rootfs.join("root/.local/share/applications");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("silversurfer.desktop"),
        "[Desktop Entry]\n\
         Version=1.0\n\
         Type=Application\n\
         Name=SilverSurfer\n\
         GenericName=Web Browser\n\
         Comment=Navegador de Eclipse OS (Servo)\n\
         Exec=/usr/local/bin/eclipse-browser %u\n\
         TryExec=/usr/local/bin/eclipse-browser\n\
         Terminal=false\n\
         Icon=web-browser\n\
         Categories=Network;WebBrowser;\n\
         MimeType=text/html;text/xml;application/xhtml+xml;x-scheme-handler/http;x-scheme-handler/https;\n\
         StartupNotify=false\n\
         StartupWMClass=silversurfer\n",
    )
    .unwrap();

    // Default handler for the session user. Firefox may still be installed;
    // SilverSurfer wins when the user opens http(s) links.
    let mime = rootfs.join("root/.config/mimeapps.list");
    if let Some(parent) = mime.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let block = "[Default Applications]\n\
                 text/html=silversurfer.desktop\n\
                 x-scheme-handler/http=silversurfer.desktop\n\
                 x-scheme-handler/https=silversurfer.desktop\n\
                 application/xhtml+xml=silversurfer.desktop\n";
    match fs::read_to_string(&mime) {
        Ok(existing) if existing.contains("silversurfer.desktop") => {}
        Ok(existing) => {
            let mut out = existing;
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
            fs::write(&mime, out).unwrap();
        }
        Err(_) => fs::write(&mime, block).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn wrapper_and_desktop_land_in_the_rootfs() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("eclipse-ss-desktop-{nanos}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_silversurfer_wrapper(&root);
        write_silversurfer_desktop_entry(&root);
        let wrapper = root.join("usr/local/bin/eclipse-browser");
        let desk = root.join("root/.local/share/applications/silversurfer.desktop");
        let mime = root.join("root/.config/mimeapps.list");
        let w = fs::read_to_string(&wrapper).unwrap();
        assert!(w.contains("silversurfer"));
        assert!(w.contains("eclipse-firefox"));
        let d = fs::read_to_string(&desk).unwrap();
        assert!(d.contains("x-scheme-handler/https"));
        assert!(d.contains("eclipse-browser"));
        let m = fs::read_to_string(&mime).unwrap();
        assert!(m.contains("silversurfer.desktop"));
        let _ = fs::remove_dir_all(&root);
    }
}
