//! The paths `mount(2)` and `umount2(2)` act on, as Linux 6.18 finds them
//! (measured there with glibc and musl): from the working directory,
//! through symbolic links, with `..` read where the walk is and not as
//! text; and in what order each call looks at what it was given.
//!
//! Every test has a top directory and a pid of its own: the mount table and
//! the path cache are shared by every process in this binary.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const MS_RDONLY: usize = 1;
const MNT_DETACH: usize = 2;
const AN_UNKNOWN_UMOUNT_FLAG: usize = 0x8000;

/// A root process whose root holds, under `/<top>`: `src/d/mark`, `run/b`,
/// `run/c`, `run/link` -> `/<top>/run/b`, `run/rel` -> `b`, `run/srclink`
/// -> `/<top>/src/d` and `run/dangling` -> `/<top>/run/nowhere`.
fn a_process(pid: KoID, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let link = |dir: &Arc<dyn INode>, name: &str, to: &str| {
        let l = dir.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    let src = top_dir.create("src", FileType::Dir, 0o755).unwrap();
    let d = src.create("d", FileType::Dir, 0o755).unwrap();
    d.create("mark", FileType::File, 0o644).unwrap();
    let run = top_dir.create("run", FileType::Dir, 0o755).unwrap();
    run.create("b", FileType::Dir, 0o755).unwrap();
    run.create("c", FileType::Dir, 0o755).unwrap();
    link(&run, "link", &format!("/{}/run/b", top));
    link(&run, "rel", "b");
    link(&run, "srclink", &format!("/{}/src/d", top));
    link(&run, "dangling", &format!("/{}/run/nowhere", top));
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, top, lp).unwrap()
}

/// A NUL-terminated copy, and a pointer to it, or NULL for no string.
fn c(s: &str) -> String {
    format!("{}\0", s)
}

fn at(s: &Option<String>) -> UserInPtr<u8> {
    UserInPtr::from(s.as_ref().map_or(0, |s| s.as_ptr() as usize))
}

fn mount(
    sys: &Syscall<'_>,
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: usize,
) -> SysResult {
    let (source, target, fstype) = (source.map(c), Some(c(target)), fstype.map(c));
    sys.sys_mount(
        at(&source),
        at(&target),
        at(&fstype),
        flags,
        UserInPtr::from(0),
    )
}

fn bind(sys: &Syscall<'_>, source: &str, target: &str) -> SysResult {
    mount(sys, Some(source), target, None, MS_BIND)
}

fn umount(sys: &Syscall<'_>, target: &str, flags: usize) -> SysResult {
    sys.sys_umount2(at(&Some(c(target))), flags)
}

fn chdir(sys: &Syscall<'_>, path: &str) {
    sys.sys_chdir(at(&Some(c(path)))).unwrap();
}

fn exists(sys: &Syscall<'_>, path: &str) -> bool {
    sys.linux_process().lookup_inode(path).is_ok()
}

/// The roots of the lines `/proc/self/mountinfo` has at `point`, in order.
fn roots_at(sys: &Syscall<'_>, point: &str) -> Vec<String> {
    field_at(sys, point, 3)
}

/// The options of those lines.
fn options_at(sys: &Syscall<'_>, point: &str) -> Vec<String> {
    field_at(sys, point, 5)
}

/// Field `field` of the lines `/proc/self/mountinfo` has at `point`.
fn field_at(sys: &Syscall<'_>, point: &str, field: usize) -> Vec<String> {
    let inode = sys
        .linux_process()
        .lookup_inode("/proc/self/mountinfo")
        .unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let len = inode.read_at(0, &mut buf).unwrap();
    String::from_utf8_lossy(&buf[..len])
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            (fields.get(4) == Some(&point)).then(|| String::from(fields[field]))
        })
        .collect()
}

/// What a bind of `/<top>/src/d` onto `/<top>/run/b`, however it was
/// spelled, leaves: the mark through `run/b`, one line there rooted at the
/// directory, and no line under any other spelling of it.
fn bound_at_b(sys: &Syscall<'_>, top: &str) -> (bool, Vec<String>, Vec<String>) {
    let run = format!("/{}/run", top);
    let seen = exists(sys, &format!("{}/b/mark", run));
    let roots = roots_at(sys, &format!("{}/b", run));
    let mut stray = Vec::new();
    for name in [
        String::from("b"),
        String::from("/b"),
        String::from("/d"),
        format!("{}/link", run),
        format!("{}/rel", run),
        format!("{}/c/../b", run),
    ] {
        stray.extend(
            roots_at(sys, &name)
                .into_iter()
                .map(|r| name.clone() + " " + &r),
        );
    }
    let _ = umount(sys, &format!("{}/b", run), MNT_DETACH);
    (seen, roots, stray)
}

fn assert_bound_at_b(got: (SysResult, (bool, Vec<String>, Vec<String>)), top: &str, how: &str) {
    let (result, (seen, roots, stray)) = got;
    assert_eq!(result, Ok(0), "{}: the bind failed", how);
    assert!(seen, "{}: run/b does not show the directory", how);
    assert_eq!(
        roots,
        [format!("/{}/src/d", top)],
        "{}: the line at run/b",
        how
    );
    assert!(
        stray.is_empty(),
        "{}: lines under other names: {:?}",
        how,
        stray
    );
}

#[test]
fn a_relative_target_is_taken_from_the_working_directory() {
    let proc = a_process(45_610, "mp_rt");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            chdir(sys, "/mp_rt/run");
            (bind(sys, "/mp_rt/src/d", "b"), bound_at_b(sys, "mp_rt"))
        })
    });
    assert_bound_at_b(got, "mp_rt", "a relative target");
}

#[test]
fn a_relative_source_is_taken_from_the_working_directory() {
    let proc = a_process(45_611, "mp_rs");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            chdir(sys, "/mp_rs/src");
            (bind(sys, "d", "/mp_rs/run/b"), bound_at_b(sys, "mp_rs"))
        })
    });
    assert_bound_at_b(got, "mp_rs", "a relative source");
}

#[test]
fn a_target_through_a_symbolic_link_is_where_the_link_leads() {
    let proc = a_process(45_612, "mp_lt");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                bind(sys, "/mp_lt/src/d", "/mp_lt/run/link"),
                bound_at_b(sys, "mp_lt"),
            )
        })
    });
    assert_bound_at_b(got, "mp_lt", "a target through a link");
}

/// A relative link is read from the directory it is in, wherever the
/// caller is.
#[test]
fn a_relative_link_in_the_target_is_read_from_its_own_directory() {
    let proc = a_process(45_613, "mp_rl");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            chdir(sys, "/mp_rl/src");
            (bind(sys, "d", "/mp_rl/run/rel"), bound_at_b(sys, "mp_rl"))
        })
    });
    assert_bound_at_b(got, "mp_rl", "a target through a relative link");
}

#[test]
fn a_source_through_a_symbolic_link_is_where_the_link_leads() {
    let proc = a_process(45_614, "mp_ls");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                bind(sys, "/mp_ls/run/srclink", "/mp_ls/run/b"),
                bound_at_b(sys, "mp_ls"),
            )
        })
    });
    assert_bound_at_b(got, "mp_ls", "a source through a link");
}

#[test]
fn a_target_with_dot_dot_in_it_is_the_directory_it_names() {
    let proc = a_process(45_615, "mp_dd");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                bind(sys, "/mp_dd/src/d", "/mp_dd/run/c/../b"),
                bound_at_b(sys, "mp_dd"),
            )
        })
    });
    assert_bound_at_b(got, "mp_dd", "a target with ..");
}

/// `srclink/..` is the directory the link leads into, `/<top>/src`. Folded
/// as text first it is `/<top>/run`, where there is no `d`.
#[test]
fn a_dot_dot_after_a_link_climbs_out_of_where_the_link_leads() {
    let proc = a_process(45_616, "mp_ld");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                bind(sys, "/mp_ld/run/srclink/../d", "/mp_ld/run/b"),
                bound_at_b(sys, "mp_ld"),
            )
        })
    });
    assert_bound_at_b(got, "mp_ld", "a source with .. after a link");
}

#[test]
fn umount_of_a_relative_path() {
    let proc = a_process(45_617, "mp_ur");
    let (bound, got, left, seen) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let bound = bind(sys, "/mp_ur/src/d", "/mp_ur/run/b");
            chdir(sys, "/mp_ur/run");
            let got = umount(sys, "b", 0);
            (
                bound,
                got,
                roots_at(sys, "/mp_ur/run/b"),
                exists(sys, "/mp_ur/run/b/mark"),
            )
        })
    });
    assert_eq!(bound, Ok(0));
    assert_eq!(got, Ok(0), "umount b from /mp_ur/run");
    assert!(left.is_empty(), "lines left at run/b: {:?}", left);
    assert!(!seen, "run/b still shows the directory");
}

#[test]
fn umount_through_a_symbolic_link_follows_it() {
    let proc = a_process(45_618, "mp_ul");
    let (bound, got, left) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let bound = bind(sys, "/mp_ul/src/d", "/mp_ul/run/b");
            let got = umount(sys, "/mp_ul/run/link", 0);
            (bound, got, roots_at(sys, "/mp_ul/run/b"))
        })
    });
    assert_eq!(bound, Ok(0));
    assert_eq!(got, Ok(0), "umount through the link");
    assert!(left.is_empty(), "lines left at run/b: {:?}", left);
}

/// The link itself is no mount point. With a slash after it, the link is
/// followed all the same, as every lookup follows one there.
#[test]
fn umount_nofollow_of_a_link_is_einval_and_a_trailing_slash_follows_it() {
    let proc = a_process(45_619, "mp_un");
    let (bound, kept, left, slashed, gone) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let bound = bind(sys, "/mp_un/src/d", "/mp_un/run/b");
            let kept = umount(sys, "/mp_un/run/link", UMOUNT_NOFOLLOW);
            let left = roots_at(sys, "/mp_un/run/b");
            let slashed = umount(sys, "/mp_un/run/link/", UMOUNT_NOFOLLOW);
            (bound, kept, left, slashed, roots_at(sys, "/mp_un/run/b"))
        })
    });
    assert_eq!(bound, Ok(0));
    assert_eq!(kept, Err(LxError::EINVAL), "UMOUNT_NOFOLLOW of the link");
    assert_eq!(left.len(), 1, "UMOUNT_NOFOLLOW took the mount: {:?}", left);
    assert_eq!(slashed, Ok(0), "UMOUNT_NOFOLLOW of link/");
    assert!(gone.is_empty(), "lines left at run/b: {:?}", gone);
}

#[test]
fn a_move_between_relative_paths() {
    let proc = a_process(45_620, "mp_mv");
    let (bound, got, at_c, at_b, named, seen) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let bound = bind(sys, "/mp_mv/src/d", "/mp_mv/run/b");
            chdir(sys, "/mp_mv/run");
            let got = mount(sys, Some("b"), "c", None, MS_MOVE);
            let seen = exists(sys, "/mp_mv/run/c/mark");
            let lines = (
                roots_at(sys, "/mp_mv/run/c"),
                roots_at(sys, "/mp_mv/run/b"),
                roots_at(sys, "c"),
            );
            let _ = umount(sys, "/mp_mv/run/c", MNT_DETACH);
            (bound, got, lines.0, lines.1, lines.2, seen)
        })
    });
    assert_eq!(bound, Ok(0));
    assert_eq!(got, Ok(0), "move b c from /mp_mv/run");
    assert!(seen, "run/c does not show the directory");
    assert_eq!(at_c, ["/mp_mv/src/d"], "the line at run/c");
    assert!(at_b.is_empty(), "lines left at run/b: {:?}", at_b);
    assert!(named.is_empty(), "a line named c: {:?}", named);
}

#[test]
fn a_tmpfs_on_a_relative_path_is_listed_and_unmounted_by_its_full_path() {
    let proc = a_process(45_621, "mp_tf");
    let (got, at_c, named, gone, left) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            chdir(sys, "/mp_tf/run");
            let got = mount(sys, Some("t58"), "c", Some("tmpfs"), 0);
            let at_c = roots_at(sys, "/mp_tf/run/c");
            let named = roots_at(sys, "c");
            let gone = umount(sys, "c", 0);
            (got, at_c, named, gone, roots_at(sys, "/mp_tf/run/c"))
        })
    });
    assert_eq!(got, Ok(0), "mount -t tmpfs t58 c from /mp_tf/run");
    assert_eq!(at_c.len(), 1, "lines at run/c: {:?}", at_c);
    assert!(named.is_empty(), "a line named c: {:?}", named);
    assert_eq!(gone, Ok(0), "umount c from /mp_tf/run");
    assert!(left.is_empty(), "lines left at run/c: {:?}", left);
}

#[test]
fn a_target_that_is_a_dangling_link_is_enoent() {
    let proc = a_process(45_622, "mp_dl");
    let (got, named) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let got = bind(sys, "/mp_dl/src/d", "/mp_dl/run/dangling");
            (got, roots_at(sys, "/mp_dl/run/dangling"))
        })
    });
    assert_eq!(got, Err(LxError::ENOENT));
    assert!(named.is_empty(), "a line at the link: {:?}", named);
}

/// `do_loopback` and `do_move_mount_old`: `if (!old_name || !*old_name)
/// return -EINVAL`. Taken as a path from the root, "" bound `/` itself.
#[test]
fn a_bind_or_a_move_of_an_empty_name_is_einval() {
    let proc = a_process(45_623, "mp_es");
    let (bound, moved, left) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let bound = bind(sys, "", "/mp_es/run/b");
            let moved = mount(sys, Some(""), "/mp_es/run/b", None, MS_MOVE);
            let left = roots_at(sys, "/mp_es/run/b");
            let _ = umount(sys, "/mp_es/run/b", MNT_DETACH);
            (bound, moved, left)
        })
    });
    assert_eq!(bound, Err(LxError::EINVAL), "a bind of \"\"");
    assert_eq!(moved, Err(LxError::EINVAL), "a move of \"\"");
    assert!(left.is_empty(), "lines at run/b: {:?}", left);
}

/// `do_mount` looks the target up first, and only then the type
/// (`ENODEV`) and the source (`EINVAL` for an empty one).
#[test]
fn the_target_is_looked_up_before_the_type_and_the_source() {
    let proc = a_process(45_624, "mp_tg");
    let (typed, bound) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                mount(sys, Some("t58"), "/mp_tg/run/nowhere", Some("nosuchfs"), 0),
                bind(sys, "", "/mp_tg/run/nowhere"),
            )
        })
    });
    assert_eq!(
        typed,
        Err(LxError::ENOENT),
        "an unknown type onto a missing target"
    );
    assert_eq!(
        bound,
        Err(LxError::ENOENT),
        "a bind of \"\" onto a missing target"
    );
}

/// `ksys_umount` reads the word before the path.
#[test]
fn umount2_reads_its_flags_before_it_looks_the_path_up() {
    let proc = a_process(45_625, "mp_uf");
    let (flagged, plain) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                umount(sys, "/mp_uf/run/nowhere", AN_UNKNOWN_UMOUNT_FLAG),
                umount(sys, "/mp_uf/run/nowhere", 0),
            )
        })
    });
    assert_eq!(
        flagged,
        Err(LxError::EINVAL),
        "an unknown flag on a missing path"
    );
    assert_eq!(plain, Err(LxError::ENOENT), "umount of a missing path");
}

/// A remount reads neither the source nor the type, so a source that is no
/// path changes nothing: the bind is read-only after it all the same.
#[test]
fn a_remount_does_not_look_its_source_up() {
    let proc = a_process(45_626, "mp_rm");
    let (bound, got, opts) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let bound = bind(sys, "/mp_rm/src/d", "/mp_rm/run/b");
            let got = mount(
                sys,
                Some("/mp_rm/no-such-source"),
                "/mp_rm/run/b",
                None,
                MS_REMOUNT | MS_BIND | MS_RDONLY,
            );
            let opts = options_at(sys, "/mp_rm/run/b");
            let _ = umount(sys, "/mp_rm/run/b", MNT_DETACH);
            (bound, got, opts)
        })
    });
    assert_eq!(bound, Ok(0));
    assert_eq!(got, Ok(0), "a remount with a source that is not there");
    assert!(
        opts.iter().any(|o| o.starts_with("ro")),
        "the options at run/b: {:?}",
        opts
    );
}
