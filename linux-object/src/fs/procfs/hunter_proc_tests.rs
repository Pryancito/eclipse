//! `/proc/hunter` takes a control script from root, and from nobody else.

use super::hunter_proc::may_control_hunter;
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
