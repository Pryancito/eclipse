//! The address this stack is to have, when the machine's network
//! settings say one.
//!
//! The command line gives the stack its address, or has it ask for one.
//! That is the word of whoever built the boot. The person who uses the
//! machine sets its network in `/etc/eclipse/network`, by interface name,
//! and this process cannot look itself up there: its card has no name
//! until the kernel shows it, which is after it has an address. So the
//! service that applies those settings (`lunarnet`) leaves what they say
//! for this card where the stack can find it by the one thing it knows
//! of itself, the card's address: a file named after it, holding a word
//! as the command line's (`dhcp`, `ADDR/PREFIX`, `ADDR/PREFIX,GATEWAY`).
//! While that file is there, its word is the stack's addressing and the
//! command line's is not; it is read once, when the stack starts, and
//! the service starts the stack again when the word is to be another.

use std::path::{Path, PathBuf};

/// Where the words are, one file per card.
pub const DIR: &str = "/etc/eclipse/netstack";

/// The file for the card with this address.
pub fn path(dir: &Path, mac: [u8; 6]) -> PathBuf {
    dir.join(udrivers::status::mac_text(mac))
}

/// What the settings say for the card with this address: the first line
/// of its file that says anything, or nothing when there is no file or
/// it is empty.
pub fn word(dir: &Path, mac: [u8; 6]) -> Option<String> {
    let text = std::fs::read_to_string(path(dir, mac)).ok()?;
    let said = text.lines().map(str::trim);
    let mut said = said.filter(|line| !line.is_empty() && !line.starts_with('#'));
    said.next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_word_is_read_from_the_file_named_after_the_card() {
        let top = std::env::temp_dir().join(format!("netstack-governed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        // No directory, no file: the command line's word stands.
        assert_eq!(word(&top, mac), None);
        std::fs::create_dir_all(&top).unwrap();
        assert_eq!(word(&top, mac), None);
        assert_eq!(path(&top, mac), top.join("52:54:00:12:34:56"));
        // Another card's file is not this one's.
        std::fs::write(top.join("52:54:00:aa:bb:cc"), "dhcp\n").unwrap();
        assert_eq!(word(&top, mac), None);

        std::fs::write(path(&top, mac), "10.0.9.20/24,10.0.9.1\n").unwrap();
        assert_eq!(word(&top, mac).as_deref(), Some("10.0.9.20/24,10.0.9.1"));
        // A comment and blank lines say nothing, and neither does a file
        // with only those.
        std::fs::write(path(&top, mac), "# by lunarnet\n\n  dhcp  \nmore\n").unwrap();
        assert_eq!(word(&top, mac).as_deref(), Some("dhcp"));
        std::fs::write(path(&top, mac), "# by lunarnet\n\n").unwrap();
        assert_eq!(word(&top, mac), None);
        let _ = std::fs::remove_dir_all(&top);
    }
}
