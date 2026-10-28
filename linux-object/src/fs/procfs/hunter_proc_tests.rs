//! `/proc/hunter` takes a control script from root, and from nobody else.

use super::hunter_proc::{may_control_hunter, scan_inode};
use super::*;

#[test]
fn only_root_or_the_kernel_may_drive_hunter() {
    assert!(may_control_hunter(None), "the kernel itself");
    assert!(may_control_hunter(Some((100, 0, 0))), "euid 0");
    assert!(
        !may_control_hunter(Some((100, 1000, 1000))),
        "an ordinary user"
    );
    // The EFFECTIVE uid decides, as for every other permission check: a
    // setuid-root program is root here, and root that dropped to 1000 is not.
    assert!(
        !may_control_hunter(Some((100, 1000, 0))),
        "egid 0 is not enough"
    );
}

#[test]
fn proc_hunter_is_readable_by_all_and_writable() {
    let node = ProcHunterINode;
    assert_eq!(node.metadata().unwrap().mode, 0o644);
    assert!(node.poll().unwrap().write);
    // `echo ... > /proc/hunter` opens with O_TRUNC first.
    node.resize(0).unwrap();
    let mut buf = [0u8; 64];
    let n = node.read_at(0, &mut buf).unwrap();
    assert!(core::str::from_utf8(&buf[..n])
        .unwrap()
        .starts_with("hunter security subsystem"));
}

#[test]
fn a_script_that_does_not_parse_is_einval() {
    // No user thread on a host test, so this is the kernel writing: allowed,
    // and still refused for the typo.
    let node = ProcHunterINode;
    assert!(matches!(
        node.write_at(0, b"mode exec enforcing\n"),
        Err(FsError::InvalidParam)
    ));
    assert!(matches!(
        node.write_at(0, b"\xff\n"),
        Err(FsError::InvalidParam)
    ));
}

#[test]
fn a_valid_script_is_taken_whole() {
    // Tightening only, and to a domain no other test in this crate reads, so
    // the global stance it leaves behind cannot move anyone else's answer.
    let node = ProcHunterINode;
    let script = b"deny /hunter-tests/denied\n";
    assert_eq!(node.write_at(0, script).unwrap(), script.len());
    assert!(hunter::policy::is_exec_blacklisted("/hunter-tests/denied"));
}

/// The EICAR test file: harmless, and what every antivirus is tested with.
const EICAR: &[u8] = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";

#[test]
fn the_on_demand_scanner_finds_eicar_in_a_file_and_says_clean_of_the_rest() {
    use rcore_fs::vfs::FileType;
    // A byte signature of its own, so it cannot collide with another test's.
    hunter::av::add_signature(
        hunter::av::parse_signature(
            "bytes:45494341522d5354414e444152442d414e544956495255532d544553542d46494c45:Test.EICAR.procfs",
        )
        .unwrap()
        .unwrap(),
    );
    let fs = rcore_fs_ramfs::RamFS::new();
    let root = rcore_fs::vfs::FileSystem::root_inode(&*fs);
    let bad = root.create("eicar.com", FileType::File, 0o644).unwrap();
    bad.write_at(0, EICAR).unwrap();
    let good = root.create("clean", FileType::File, 0o644).unwrap();
    good.write_at(0, b"nothing to see").unwrap();
    let dir = root.create("dir", FileType::Dir, 0o755).unwrap();

    hunter::av::begin_scan_batch();
    scan_inode("/eicar.com", Some(&*bad));
    scan_inode("/clean", Some(&*good));
    scan_inode("/dir", Some(&*dir));
    scan_inode("/missing", None);
    let node = ProcHunterScanINode;
    let mut buf = [0u8; 512];
    let n = node.read_at(0, &mut buf).unwrap();
    assert_eq!(
        core::str::from_utf8(&buf[..n]).unwrap(),
        "/eicar.com: Test.EICAR.procfs FOUND\n/clean: OK\n/dir: ERROR cannot open\n/missing: ERROR cannot open\n"
    );
    assert_eq!(node.metadata().unwrap().mode, 0o644);
}
