//! The `/etc/fstab` the image ships, crossed against what
//! `install-eclipse` patches in it.
//!
//! Moved out of `mod.rs` by the size ratchet: the file was 14 lines over
//! its own ceiling.

use super::*;

/// The btrfs template is patched in place by `install-eclipse`, which
/// looks for each line exactly as its `FSTAB_TEMPLATE_LINE_*` says: a
/// column moved by one space and the installed system boots with a
/// placeholder for a device.
#[test]
fn the_btrfs_template_has_the_lines_the_installer_patches() {
    let installer =
        fs::read_to_string(PROJECT_DIR.join("tools/install-eclipse/install-eclipse.c")).unwrap();
    let template = LinuxRootfs::fstab_template(image::DiskFs::Btrfs);
    let mut found = 0;
    for line in installer.lines() {
        let Some(rest) = line.strip_prefix("#define FSTAB_TEMPLATE_LINE_") else {
            continue;
        };
        let quoted = rest.split('"').nth(1).unwrap();
        assert!(
            template.lines().any(|l| l == quoted),
            "the template has no line {quoted:?}"
        );
        found += 1;
    }
    assert_eq!(found, 4, "ROOT, EFI, HOME and SWAP");
}

/// With EclipseFS images, the disk lines say so (the installer rewrites
/// the file by mounting the partition, so only the type matters).
#[test]
fn the_eclipsefs_template_names_eclipsefs_for_root_and_home() {
    let template = LinuxRootfs::fstab_template(image::DiskFs::EclipseFs);
    let types: Vec<&str> = template
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split_whitespace().nth(2).unwrap())
        .collect();
    assert_eq!(types, ["eclipsefs", "vfat", "eclipsefs", "swap"]);
}
