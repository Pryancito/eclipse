//! Paths on the EFI System Partition, as `rboot.conf` writes them and the
//! firmware reads them.
//!
//! rboot opens exactly three files off the disk: `rboot.conf` itself, the
//! kernel ELF and the initramfs. When one of them is missing, all the screen
//! can say is which path was asked for and what the directory that should
//! have held it actually contains -- so the piece of a path that names that
//! directory lives here, where it can be tested outside UEFI.

/// The directory part of an ESP path: `\EFI\zCore\zcore.elf` -> `\EFI\zCore`.
///
/// A path with no separator, or whose only separator is the leading root, is
/// a file in the volume root, which is `\`.
///
/// Backslash is the *only* separator UEFI knows: a `/` is an ordinary
/// character in a FAT name, so it is not treated as one here. A `kernel_path`
/// written with forward slashes is a path that does not exist, and printing
/// it whole ("cannot find `\EFI/zCore/zcore.elf`") is more useful than
/// guessing what it meant.
pub fn parent_dir(path: &str) -> &str {
    match path.rfind('\\') {
        // No separator, or only the root one: the file hangs off `\`.
        None | Some(0) => "\\",
        Some(i) => &path[..i],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_paths_rboot_opens() {
        assert_eq!(parent_dir("\\EFI\\Boot\\rboot.conf"), "\\EFI\\Boot");
        assert_eq!(parent_dir("\\EFI\\zCore\\zcore.elf"), "\\EFI\\zCore");
        assert_eq!(parent_dir("\\EFI\\zCore\\initramfs.img"), "\\EFI\\zCore");
    }

    #[test]
    fn the_root_in_its_three_shapes() {
        // A bare name, a name hanging off the root, and the root itself.
        assert_eq!(parent_dir("zcore.elf"), "\\");
        assert_eq!(parent_dir("\\zcore.elf"), "\\");
        assert_eq!(parent_dir(""), "\\");
        assert_eq!(parent_dir("\\"), "\\");
    }

    #[test]
    fn a_forward_slash_is_not_a_separator() {
        // The firmware does not treat it as one, so neither do we: the whole
        // name is what does not exist, and that is what has to be shown.
        assert_eq!(parent_dir("/EFI/zCore/zcore.elf"), "\\");
        assert_eq!(parent_dir("\\EFI/zCore/zcore.elf"), "\\");
    }

    #[test]
    fn a_trailing_separator_still_has_a_parent() {
        // A path that ends in a separator: the parent is what comes before
        // it, never the path itself -- listing the parent of a failure would
        // otherwise look at the very directory that failed.
        assert_eq!(parent_dir("\\EFI\\zCore\\"), "\\EFI\\zCore");
    }
}
