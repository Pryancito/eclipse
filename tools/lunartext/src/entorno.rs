//! `/etc/eclipse/entorno`: the desktop settings that used to live only in
//! environment variables (`LUNAR_TEXT`, `LUNAR_FX`, `LUNARBAR_HEIGHT`,
//! `LUNARBG_STATIC`, `SILVERSURFER_ADBLOCK`; and `LUNAR_ACCENT`,
//! `LUNAR_CLOCK`, which never were variables), kept in a file so that the
//! control panel can change them.
//!
//! The file is `NAME=value` lines, `#` comments; `eclipse-entorno` (written
//! by xtask) is the only thing that writes it. An environment variable that
//! is set still wins, so `LUNAR_TEXT=bitmap lunarrun` keeps working for a
//! test; otherwise the program reads the file when it starts, and a setting
//! changed in the panel holds for every program opened after it.
//!
//! It lives in this crate because every Lunar client and SilverSurfer
//! already links it.

/// Where the settings are.
pub const FICHERO: &str = "/etc/eclipse/entorno";

/// `name` from the environment, else from [`FICHERO`].
pub fn var(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        return Some(v);
    }
    de_texto(&std::fs::read_to_string(FICHERO).ok()?, name)
}

/// `name`'s value in the text of an entorno file: its first line, as the
/// panel reads and writes the first one too. An empty value is no value.
pub fn de_texto(text: &str, name: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::de_texto;

    #[test]
    fn the_first_uncommented_line_of_the_name_is_its_value() {
        let t = "# LUNAR_FX=off\nLUNAR_FXX=1\n LUNAR_FX = off \nLUNAR_FX=on\n";
        assert_eq!(de_texto(t, "LUNAR_FX").as_deref(), Some("off"));
        assert_eq!(de_texto(t, "LUNAR_TEXT"), None);
        assert_eq!(de_texto("LUNAR_TEXT=\n", "LUNAR_TEXT"), None);
        assert_eq!(de_texto("", "LUNAR_TEXT"), None);
    }
}
