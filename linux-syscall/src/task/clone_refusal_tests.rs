//! The two pure halves of `clone`'s refusals: which namespace flags were
//! asked for, and whether a sharing flag came without `CLONE_THREAD`.
//!
//! Moved out of `task.rs` whole, with nothing added or removed: the file
//! was at 3978 of the 4000 the size ratchet allows, and this is the fix
//! the ratchet prescribes.

use super::*;

/// Every namespace flag is refused, each with the bit Linux gives it.
///
/// The bit numbers are the contract: `bwrap` asks for `CLONE_NEWUSER` by
/// value, and what it does with the answer is decided by which answer it
/// gets. A flag missing from the mask is not a refusal that goes astray
/// but a `clone` that **succeeds** and then lies: the child runs in the
/// parent's namespace while the caller believes it was isolated.
#[test]
fn every_namespace_flag_is_refused_with_its_linux_value() {
    let named: [(usize, &str); 7] = [
        (1 << 17, "CLONE_NEWNS"),
        (1 << 25, "CLONE_NEWCGROUP"),
        (1 << 26, "CLONE_NEWUTS"),
        (1 << 27, "CLONE_NEWIPC"),
        (1 << 28, "CLONE_NEWUSER"),
        (1 << 29, "CLONE_NEWPID"),
        (1 << 30, "CLONE_NEWNET"),
    ];
    for (bits, name) in named.iter() {
        let f = CloneFlags::from_bits_truncate(*bits);
        assert_eq!(f.bits(), *bits, "{} is not the bit this kernel names", name);
        assert_eq!(
            clone_namespaces_refused(f),
            f,
            "{} has to be refused, not quietly granted",
            name
        );
    }
}

/// The mask is those seven bits and nothing else.
///
/// Written out as one number because that is the shape a drift shows up
/// in: a flag dropped from the list leaves six assertions above still
/// passing.
#[test]
fn the_namespace_mask_is_only_those_seven_bits() {
    assert_eq!(
        UNSUPPORTED_CLONE_NS.bits(),
        0x7E02_0000,
        "NEWNS and the six from NEWCGROUP up"
    );
    assert_eq!(UNSUPPORTED_CLONE_NS.bits().count_ones(), 7);
}

/// A `clone` that asks for no namespace is left alone.
///
/// Which is nearly every `clone` there is: a plain `fork`, a
/// `pthread_create`, a `vfork`. The refusal has to be the exception.
#[test]
fn the_ordinary_clones_ask_for_no_namespace() {
    assert!(clone_namespaces_refused(CloneFlags::empty()).is_empty());
    // What glibc's `pthread_create` passes.
    let pthread = CloneFlags::VM
        | CloneFlags::FS
        | CloneFlags::FILES
        | CloneFlags::SIGHAND
        | CloneFlags::THREAD
        | CloneFlags::SYSVSEM
        | CloneFlags::SETTLS
        | CloneFlags::PARENT_SETTID
        | CloneFlags::CHILD_CLEARTID;
    assert!(clone_namespaces_refused(pthread).is_empty());
    assert!(!clone_shares_without_thread(pthread));
    // And a `vfork`.
    let vfork = CloneFlags::VM | CloneFlags::VFORK;
    assert!(clone_namespaces_refused(vfork).is_empty());
    assert!(!clone_shares_without_thread(vfork));
}

/// Only the flags actually asked for come back refused.
///
/// The refusal is logged by value, so a mask that answered with more than
/// the caller asked for would name flags nobody passed.
#[test]
fn the_refusal_names_what_was_asked_for_and_no_more() {
    let asked = CloneFlags::NEWUSER | CloneFlags::NEWPID | CloneFlags::VM | CloneFlags::VFORK;
    assert_eq!(
        clone_namespaces_refused(asked),
        CloneFlags::NEWUSER | CloneFlags::NEWPID
    );
}

/// Sharing the descriptor table, the filesystem information, the signal
/// handlers or the `SEM_UNDO` list is only implemented together with
/// `CLONE_THREAD`.
///
/// Accepting any of them as a plain fork answered success and then did a
/// fork: the caller held a descriptor table it believed was shared, so an
/// `open` in the child was a descriptor the parent never saw.
#[test]
fn sharing_state_without_clone_thread_is_refused() {
    let named: [(usize, &str); 4] = [
        (1 << 9, "CLONE_FS"),
        (1 << 10, "CLONE_FILES"),
        (1 << 11, "CLONE_SIGHAND"),
        (1 << 18, "CLONE_SYSVSEM"),
    ];
    for (bits, name) in named.iter() {
        let f = CloneFlags::from_bits_truncate(*bits);
        assert_eq!(f.bits(), *bits, "{} is not the bit this kernel names", name);
        assert!(
            clone_shares_without_thread(f),
            "{} without CLONE_THREAD has to be refused",
            name
        );
        assert!(
            !clone_shares_without_thread(f | CloneFlags::THREAD),
            "{} is implemented for a thread",
            name
        );
    }
    assert_eq!(
        UNSUPPORTED_CLONE_SHARE.bits(),
        0x4_0E00,
        "FS, FILES, SIGHAND and SYSVSEM"
    );
    assert_eq!(UNSUPPORTED_CLONE_SHARE.bits().count_ones(), 4);
}

/// `CLONE_VM` is the exception, and it needs the other half of the rule.
///
/// On its own it is a child sharing the parent's address space with
/// neither of them suspended, which this kernel does not implement.
/// Together with `CLONE_VFORK` it is an ordinary `vfork`, which it does,
/// and `posix_spawn` is built on exactly that pair.
#[test]
fn clone_vm_needs_vfork_or_a_thread() {
    assert!(clone_shares_without_thread(CloneFlags::VM));
    assert!(!clone_shares_without_thread(
        CloneFlags::VM | CloneFlags::VFORK
    ));
    assert!(!clone_shares_without_thread(
        CloneFlags::VM | CloneFlags::THREAD
    ));
    // And neither one alone asks for anything unimplemented.
    assert!(!clone_shares_without_thread(CloneFlags::VFORK));
    assert!(!clone_shares_without_thread(CloneFlags::empty()));
    assert!(!clone_shares_without_thread(CloneFlags::THREAD));
}

/// `CLONE_THREAD` does not excuse a namespace.
///
/// The two decisions are separate, and the namespace one comes first: a
/// thread asking for a new network namespace is still refused.
#[test]
fn a_thread_still_cannot_ask_for_a_namespace() {
    let f = CloneFlags::THREAD | CloneFlags::VM | CloneFlags::NEWNET;
    assert_eq!(clone_namespaces_refused(f), CloneFlags::NEWNET);
    assert!(!clone_shares_without_thread(f));
}

/// `CLONE_PARENT` is a fork of its own now (see `ChildOf`), from anybody
/// but init, which has no parent to give the child to.
#[test]
fn clone_parent_is_a_fork_and_init_cannot_ask_for_it() {
    let parent = CloneFlags::from_bits_truncate(1 << 15);
    assert_eq!(parent, CloneFlags::PARENT);
    assert!(!clone_shares_without_thread(parent));
    assert!(clone_parent_refused(parent, 1), "init made a sibling");
    assert!(!clone_parent_refused(parent, 2));
    assert!(
        !clone_parent_refused(CloneFlags::empty(), 1),
        "init cannot fork"
    );
    assert!(!clone_parent_refused(CloneFlags::VFORK | CloneFlags::VM, 1));
}
