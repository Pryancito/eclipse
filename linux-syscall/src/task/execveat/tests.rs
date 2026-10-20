//! What `execveat` runs, and what an exec calls the program afterwards.
//!
//! Every test has a top directory of its own: a lookup from the root goes
//! through a path cache that every process in this binary shares, so two
//! tests naming the same path would read each other's files.

use super::*;
use linux_object::fs::{Inotify, OpenFlags};
use rcore_fs_ramfs::RamFS;

const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const AT_EMPTY_PATH: usize = 0x1000;

/// A process whose root holds, under `/<top>`: `bin/prog` and `bin/s.sh`
/// (files), `bin/sub/` (a directory), `bin/link` -> `prog`, `lib` -> `bin`
/// and `up` -> `/<top>/bin/link`.
fn a_process(top: &str) -> LinuxProcess {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let link = |dir: &Arc<dyn INode>, name: &str, to: &str| {
        let l = dir.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    let top_dir = proc.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    let bin = top_dir.create("bin", FileType::Dir, 0o755).unwrap();
    bin.create("prog", FileType::File, 0o755).unwrap();
    bin.create("s.sh", FileType::File, 0o755).unwrap();
    bin.create("sub", FileType::Dir, 0o755).unwrap();
    link(&bin, "link", "prog");
    link(&top_dir, "lib", "bin");
    link(&top_dir, "up", &format!("/{}/bin/link", top));
    proc
}

/// `open(path, O_RDONLY)`, recording `path` as the file's name as `openat`
/// does.
fn open(proc: &LinuxProcess, path: &str) -> FileDesc {
    let inode = proc.lookup_inode(path).unwrap();
    proc.add_file(File::new(inode, OpenFlags::RDONLY, String::from(path)))
        .unwrap()
}

fn ino(inode: &Arc<dyn INode>) -> usize {
    inode.metadata().unwrap().inode
}

fn ino_of(proc: &LinuxProcess, path: &str) -> usize {
    ino(&proc.lookup_inode(path).unwrap())
}

#[test]
fn a_flag_outside_nofollow_and_empty_path_is_einval() {
    let proc = a_process("xa_flags");
    for bad in [0x8000, 0x200, 0x400, AT_EMPTY_PATH | 0x800] {
        assert_eq!(
            exec_target(&proc, FileDesc::CWD, "/xa_flags/bin/prog", bad).err(),
            Some(LxError::EINVAL),
            "flags {:#x}",
            bad
        );
    }
    let both = AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH;
    let t = exec_target(&proc, FileDesc::CWD, "/xa_flags/bin/prog", both).unwrap();
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_flags/bin/prog"));
}

#[test]
fn an_empty_path_without_at_empty_path_is_enoent() {
    let proc = a_process("xa_empty");
    let fd = open(&proc, "/xa_empty/bin/prog");
    for flags in [0, AT_SYMLINK_NOFOLLOW] {
        assert_eq!(
            exec_target(&proc, fd, "", flags).err(),
            Some(LxError::ENOENT)
        );
    }
}

#[test]
fn at_empty_path_runs_the_file_the_descriptor_holds() {
    let proc = a_process("xa_fd");
    let fd = open(&proc, "/xa_fd/bin/prog");
    let t = exec_target(&proc, fd, "", AT_EMPTY_PATH).unwrap();
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_fd/bin/prog"));
    assert_eq!(t.name, "/xa_fd/bin/prog");
    assert_eq!(t.absolute, "/xa_fd/bin/prog");
    let (exe, comm) = t.identity(&proc, t.name.clone());
    assert_eq!(exe, "/xa_fd/bin/prog");
    assert_eq!(comm, "prog");

    // Opened through a link: the program is the file the link leads to, and
    // so is its name (Linux takes `comm` from the open file's dentry).
    let fd = open(&proc, "/xa_fd/bin/link");
    let t = exec_target(&proc, fd, "", AT_EMPTY_PATH).unwrap();
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_fd/bin/prog"));
    assert_eq!(t.absolute, "/xa_fd/bin/prog");
    let (exe, comm) = t.identity(&proc, t.name.clone());
    assert_eq!((exe.as_str(), comm.as_str()), ("/xa_fd/bin/prog", "prog"));
}

#[test]
fn at_empty_path_on_a_descriptor_that_is_not_a_file_is_eacces() {
    let proc = a_process("xa_sock");
    let fd = proc.add_file(Inotify::new(OpenFlags::RDONLY)).unwrap();
    assert_eq!(
        exec_target(&proc, fd, "", AT_EMPTY_PATH).err(),
        Some(LxError::EACCES)
    );
    // And on no descriptor at all, `EBADF`, with a path or without.
    let none = FileDesc::from(99usize);
    assert_eq!(
        exec_target(&proc, none, "", AT_EMPTY_PATH).err(),
        Some(LxError::EBADF)
    );
    assert_eq!(
        exec_target(&proc, none, "prog", 0).err(),
        Some(LxError::EBADF)
    );
}

#[test]
fn at_empty_path_from_at_fdcwd_is_the_working_directory() {
    let proc = a_process("xa_cwd");
    proc.change_directory("/xa_cwd/bin");
    let t = exec_target(&proc, FileDesc::CWD, "", AT_EMPTY_PATH).unwrap();
    assert_eq!(t.inode.metadata().unwrap().type_, FileType::Dir);
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_cwd/bin"));
    assert_eq!(t.absolute, "/xa_cwd/bin");
}

#[test]
fn a_relative_path_is_looked_up_from_the_directory_descriptor() {
    let proc = a_process("xa_rel");
    // The working directory is somewhere else, to tell the two apart.
    proc.change_directory("/xa_rel/bin/sub");
    let dir = open(&proc, "/xa_rel/bin");
    let t = exec_target(&proc, dir, "prog", 0).unwrap();
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_rel/bin/prog"));
    assert_eq!(t.name, "/xa_rel/bin/prog");
    assert_eq!(t.absolute, "/xa_rel/bin/prog");

    // Through a directory that is a link: the name is the way the caller
    // went, the program where it ended up.
    let lib = open(&proc, "/xa_rel/lib");
    let t = exec_target(&proc, lib, "prog", 0).unwrap();
    assert_eq!(t.name, "/xa_rel/lib/prog");
    assert_eq!(t.absolute, "/xa_rel/bin/prog");
    let (exe, comm) = t.identity(&proc, t.name.clone());
    assert_eq!((exe.as_str(), comm.as_str()), ("/xa_rel/bin/prog", "prog"));
}

#[test]
fn a_path_from_the_root_or_the_working_directory_keeps_the_callers_name() {
    let proc = a_process("xa_name");
    proc.change_directory("/xa_name/bin");
    let t = exec_target(&proc, FileDesc::CWD, "./prog", 0).unwrap();
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_name/bin/prog"));
    assert_eq!(t.name, "./prog");
    assert_eq!(t.absolute, "/xa_name/bin/prog");
    let (exe, comm) = t.identity(&proc, t.name.clone());
    assert_eq!((exe.as_str(), comm.as_str()), ("/xa_name/bin/prog", "prog"));

    // An absolute path ignores `dirfd`, even one that is not open.
    let dir = open(&proc, "/xa_name/bin/sub");
    for dirfd in [dir, FileDesc::from(99usize)] {
        let t = exec_target(&proc, dirfd, "/xa_name/lib/prog", 0).unwrap();
        assert_eq!(t.name, "/xa_name/lib/prog");
        assert_eq!(t.absolute, "/xa_name/bin/prog");
    }
}

#[test]
fn nofollow_refuses_a_link_and_without_it_the_program_is_where_it_leads() {
    let proc = a_process("xa_link");
    assert_eq!(
        exec_target(
            &proc,
            FileDesc::CWD,
            "/xa_link/bin/link",
            AT_SYMLINK_NOFOLLOW
        )
        .err(),
        Some(LxError::ELOOP)
    );
    let t = exec_target(&proc, FileDesc::CWD, "/xa_link/bin/link", 0).unwrap();
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_link/bin/prog"));
    assert_eq!(t.name, "/xa_link/bin/link");
    // `comm` is the link's name, as on Linux; `/proc/self/exe` the file.
    let (exe, comm) = t.identity(&proc, t.name.clone());
    assert_eq!((exe.as_str(), comm.as_str()), ("/xa_link/bin/prog", "link"));
    // Two links, the first one absolute.
    let t = exec_target(&proc, FileDesc::CWD, "/xa_link/up", 0).unwrap();
    assert_eq!(t.absolute, "/xa_link/bin/prog");
}

#[test]
fn a_regular_file_as_the_directory_is_enotdir() {
    let proc = a_process("xa_notdir");
    let fd = open(&proc, "/xa_notdir/bin/prog");
    assert_eq!(exec_target(&proc, fd, "x", 0).err(), Some(LxError::ENOTDIR));
}

#[test]
fn proc_self_fd_is_the_open_file_wherever_it_is() {
    let proc = a_process("xa_magic");
    let fd = open(&proc, "/xa_magic/lib/prog");
    let n = usize::from(fd);
    let magic = format!("/proc/self/fd/{}", n);
    let t = exec_target(&proc, FileDesc::CWD, &magic, 0).unwrap();
    assert_eq!(ino(&t.inode), ino_of(&proc, "/xa_magic/bin/prog"));
    assert_eq!(t.name, magic);
    // Not under `/proc`, which is `noexec`: where the file is.
    assert_eq!(t.absolute, "/xa_magic/bin/prog");
    let (exe, comm) = t.identity(&proc, t.name.clone());
    assert_eq!(exe, "/xa_magic/bin/prog");
    assert_eq!(comm, n.to_string());
    // Named from the root, it is the caller's name whatever `dirfd` is.
    let dir = open(&proc, "/xa_magic/bin");
    let t = exec_target(&proc, dir, &magic, 0).unwrap();
    assert_eq!(t.name, magic);
    assert_eq!(t.absolute, "/xa_magic/bin/prog");
}

#[test]
fn a_script_is_called_by_its_own_name_and_its_exe_is_the_interpreter() {
    let proc = a_process("xa_script");
    let t = exec_target(&proc, FileDesc::CWD, "/xa_script/bin/s.sh", 0).unwrap();
    // The loader ran the interpreter last, by the name in the `#!` line,
    // which it looks up from the root with or without a leading slash.
    for loaded in ["/xa_script/bin/link", "xa_script/lib/prog"] {
        let (exe, comm) = t.identity(&proc, String::from(loaded));
        assert_eq!(exe, "/xa_script/bin/prog", "{}", loaded);
        assert_eq!(comm, "s.sh", "{}", loaded);
    }
    // With no name, Linux names it after the file that runs.
    let fd = open(&proc, "/xa_script/bin/s.sh");
    let t = exec_target(&proc, fd, "", AT_EMPTY_PATH).unwrap();
    let (exe, comm) = t.identity(&proc, String::from("/xa_script/bin/prog"));
    assert_eq!(
        (exe.as_str(), comm.as_str()),
        ("/xa_script/bin/prog", "prog")
    );
}

#[test]
fn real_path_follows_each_link_and_folds_dot_and_dot_dot_after_it() {
    let proc = a_process("xa_real");
    let link = |name: &str, to: &str| {
        let top = proc.lookup_inode("/xa_real").unwrap();
        let l = top.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    link("far", "/xa_real/bin/sub");

    assert_eq!(real_path(&proc, "/xa_real/bin/prog"), "/xa_real/bin/prog");
    assert_eq!(
        real_path(&proc, "//xa_real/./bin//prog"),
        "/xa_real/bin/prog"
    );
    assert_eq!(
        real_path(&proc, "/xa_real/bin/sub/../prog"),
        "/xa_real/bin/prog"
    );
    // `..` after a link is the parent of where the link goes: folding the
    // text first would say `/xa_real/prog`.
    assert_eq!(
        real_path(&proc, "/xa_real/far/../prog"),
        "/xa_real/bin/prog"
    );
    // A link to a link, the first absolute, the second relative to its own
    // directory.
    assert_eq!(real_path(&proc, "/xa_real/up"), "/xa_real/bin/prog");
    assert_eq!(real_path(&proc, "/xa_real/lib/link"), "/xa_real/bin/prog");
    assert_eq!(real_path(&proc, "/"), "/");
    assert_eq!(real_path(&proc, ""), "/");
    assert_eq!(real_path(&proc, "/.."), "/");
}

/// The walk spends the lookup's whole budget of links and not one more: a
/// chain of `MAXSYMLINKS` resolves, and one link longer comes back as given,
/// which is where Linux answers `ELOOP`.
///
/// The budget is a number this shares with the lookup rather than a second
/// copy, so this also pins that they are the same: a path the exec can walk
/// is one `/proc/self/exe` can name.
#[test]
fn real_path_spends_the_lookups_whole_budget_of_links_and_no_more() {
    let proc = a_process("xa_chain");
    let top = proc.lookup_inode("/xa_chain").unwrap();
    let link = |name: &str, to: &str| {
        let l = top.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    // `l1` names the file, and each one after it names the one before, so
    // walking `lN` goes through N links.
    link("l1", "/xa_chain/bin/prog");
    for i in 2..=MAX_LINKS + 1 {
        link(&format!("l{}", i), &format!("/xa_chain/l{}", i - 1));
    }
    assert_eq!(
        real_path(&proc, &format!("/xa_chain/l{}", MAX_LINKS)),
        "/xa_chain/bin/prog",
        "{} links is the budget, not one over it",
        MAX_LINKS
    );
    let over = format!("/xa_chain/l{}", MAX_LINKS + 1);
    assert_eq!(real_path(&proc, &over), over);
}

/// Linux's `MAXSYMLINKS` is 40, written here as the number and not as the
/// constant: an assertion that reads the same constant it checks says only
/// that the file agrees with itself.
#[test]
fn the_budget_is_the_forty_links_linux_walks() {
    assert_eq!(MAX_LINKS, 40);
}

#[test]
fn real_path_keeps_the_path_it_cannot_walk() {
    let proc = a_process("xa_stuck");
    let top = proc.lookup_inode("/xa_stuck").unwrap();
    top.create("loop", FileType::SymLink, 0o777)
        .unwrap()
        .write_at(0, b"loop")
        .unwrap();
    top.create("hollow", FileType::SymLink, 0o777).unwrap();
    // Not there; a link to itself; a link with nothing in it. Each comes
    // back as given, folded, and not as the part walked so far.
    assert_eq!(real_path(&proc, "/xa_stuck/lib/gone"), "/xa_stuck/lib/gone");
    assert_eq!(real_path(&proc, "/xa_stuck/lib/../gone"), "/xa_stuck/gone");
    assert_eq!(real_path(&proc, "/xa_stuck/./gone"), "/xa_stuck/gone");
    assert_eq!(real_path(&proc, "/xa_stuck/loop"), "/xa_stuck/loop");
    assert_eq!(real_path(&proc, "/xa_stuck/hollow"), "/xa_stuck/hollow");
}
