use super::*;

fn creds(ruid: u32, euid: u32) -> Credentials {
    Credentials {
        ruid,
        euid,
        suid: euid,
        rgid: ruid,
        egid: euid,
        sgid: euid,
        fsuid: euid,
        fsgid: euid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

#[test]
fn the_targets_real_uid_against_either_of_the_callers() {
    let alice = creds(1000, 1000);
    let bob = creds(2000, 2000);
    assert!(LinuxProcess::may_set_ioprio_of(&alice, &alice));
    assert!(!LinuxProcess::may_set_ioprio_of(&alice, &bob));
    // A set-uid-root task Alice started is still hers.
    let alices_setuid = creds(1000, ROOT_UID);
    assert!(LinuxProcess::may_set_ioprio_of(&alice, &alices_setuid));
    // A caller running set-uid as Bob reaches Bob's tasks through its
    // effective id and its own through its real one.
    let alice_as_bob = creds(1000, 2000);
    assert!(LinuxProcess::may_set_ioprio_of(&alice_as_bob, &bob));
    assert!(LinuxProcess::may_set_ioprio_of(&alice_as_bob, &alice));
    // But the target's EFFECTIVE id is not what is compared: Bob's task
    // running set-uid as Alice is not Alice's to touch.
    let bobs_setuid_alice = creds(2000, 1000);
    assert!(!LinuxProcess::may_set_ioprio_of(&alice, &bobs_setuid_alice));
}

#[test]
fn cap_sys_nice_reaches_everyone() {
    let root = creds(ROOT_UID, ROOT_UID);
    assert!(LinuxProcess::may_set_ioprio_of(&root, &creds(2000, 2000)));
}
