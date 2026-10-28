//! What a bind takes from the call that makes it: nothing. Linux 6.18
//! (measured there with glibc and musl) ignores the flags and the option
//! string of a plain bind, and gives the bind the flags and the line of the
//! mount it is taken from; a read-only bind is made with a remount after it.

use super::path_tests::{a_process, at, c, exists, options_at, umount, MNT_DETACH};
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use rcore_fs::vfs::FileType;

const MS_RDONLY: usize = 1;
const MS_NOEXEC: usize = 8;

/// `mount --bind source target` with `flags` and `data` asked for as well.
fn bind_asking(
    sys: &Syscall<'_>,
    source: &str,
    target: &str,
    flags: usize,
    data: &str,
) -> SysResult {
    let (source, target, data) = (Some(c(source)), Some(c(target)), Some(c(data)));
    sys.sys_mount(
        at(&source),
        at(&target),
        at(&None),
        MS_BIND | flags,
        at(&data),
    )
}

#[test]
fn a_plain_bind_takes_no_flags_and_no_options_from_the_call() {
    let proc = a_process(45_627, "mp_bo");
    let (got, bind, root, made, noexec, nosuid) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let got = bind_asking(
                sys,
                "/mp_bo/src/d",
                "/mp_bo/run/b",
                MS_RDONLY | MS_NOEXEC,
                "nosuid",
            );
            let made = sys
                .linux_process()
                .lookup_inode("/mp_bo/run/b")
                .and_then(|b| Ok(b.create("new", FileType::File, 0o644)?))
                .map(|_| exists(sys, "/mp_bo/src/d/new"));
            let noexec = linux_object::fs::path_is_noexec("/mp_bo/run/b/mark");
            let nosuid = linux_object::fs::path_is_nosuid("/mp_bo/run/b/mark");
            let lines = (options_at(sys, "/mp_bo/run/b"), options_at(sys, "/"));
            let _ = umount(sys, "/mp_bo/run/b", MNT_DETACH);
            (got, lines.0, lines.1, made, noexec, nosuid)
        })
    });
    assert_eq!(got, Ok(0));
    // The top `/` line: the mount `/mp_bo/src/d` is on.
    let root = root.last().expect("no line at /");
    assert_eq!(bind, core::slice::from_ref(root));
    assert_eq!(made, Ok(true), "a file made through the bind");
    assert!(!noexec, "MS_NOEXEC on a plain bind made it noexec");
    assert!(!nosuid, "nosuid in the option string made it nosuid");
}
