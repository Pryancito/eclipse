//! What a walk asks of each directory on its way, as Linux 6.18 does
//! (`walk-survey`, measured there with glibc and musl).

use super::*;
use crate::error::LxError;
use crate::fs::{File, FileDesc, OpenFlags};
use crate::process::{CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH};
use alloc::string::String;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Job, Process};

/// `name` in `dir`, owned by `uid` and the group of the same number.
fn make(dir: &Arc<dyn INode>, name: &str, type_: FileType, mode: u16, uid: u32) -> Arc<dyn INode> {
    let inode = dir.create(name, type_, mode as u32).unwrap();
    own(&inode, mode, uid, uid);
    inode
}

fn own(inode: &Arc<dyn INode>, mode: u16, uid: u32, gid: u32) {
    let mut meta = inode.metadata().unwrap();
    meta.mode = mode;
    meta.uid = uid as usize;
    meta.gid = gid as usize;
    inode.set_metadata(&meta).unwrap();
}

/// A root process and its own root directory, with `shut` (0700, uid 2000)
/// holding `f`.
fn a_process() -> (LinuxProcess, Arc<dyn INode>, Arc<dyn INode>) {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let root = proc.root_inode().clone();
    let shut = make(&root, "shut", FileType::Dir, 0o700, 2000);
    make(&shut, "f", FileType::File, 0o644, 2000);
    (proc, root, shut)
}

fn become_user(proc: &LinuxProcess, uid: u32) {
    proc.set_resgid(uid, uid, uid).unwrap();
    proc.set_resuid(uid, uid, uid).unwrap();
}

fn walk(proc: &LinuxProcess, path: &str) -> Option<LxError> {
    proc.lookup_inode_at(FileDesc::CWD, path, true).err()
}

#[test]
fn a_user_does_not_pass_a_directory_it_may_not_search() {
    let (proc, root, _) = a_process();
    let only_x = make(&root, "only_x", FileType::Dir, 0o711, 2000);
    make(&only_x, "f", FileType::File, 0o644, 2000);
    become_user(&proc, 1000);
    // Before every name in it: a name that is not there, `.` and `..`.
    for path in [
        "/shut/f",
        "shut/f",
        "/shut/absent",
        "shut/.",
        "/shut/..",
        "/shut/f/",
    ] {
        assert_eq!(walk(&proc, path), Some(LxError::EACCES), "{}", path);
    }
    // What the last name names is not searched, and `x` alone lets a name
    // through that `r` would be needed to list.
    assert_eq!(walk(&proc, "/shut"), None);
    assert_eq!(walk(&proc, "shut/"), None);
    assert_eq!(walk(&proc, "/only_x/f"), None);
}

#[test]
fn the_owner_is_judged_by_the_owner_bits_and_a_member_by_the_group_bits() {
    let (proc, root, _) = a_process();
    let mine = make(&root, "mine", FileType::Dir, 0o611, 1000);
    make(&mine, "f", FileType::File, 0o644, 1000);
    let ours = make(&root, "ours", FileType::Dir, 0o661, 2000);
    own(&ours, 0o661, 2000, 1000);
    make(&ours, "f", FileType::File, 0o644, 2000);
    let theirs = make(&root, "theirs", FileType::Dir, 0o701, 2000);
    make(&theirs, "f", FileType::File, 0o644, 2000);
    become_user(&proc, 1000);
    assert_eq!(walk(&proc, "/mine/f"), Some(LxError::EACCES));
    assert_eq!(walk(&proc, "/ours/f"), Some(LxError::EACCES));
    assert_eq!(walk(&proc, "/theirs/f"), None);
}

#[test]
fn root_passes_with_either_dac_capability_and_not_without() {
    let (proc, _, _) = a_process();
    let set = |without: u64| {
        proc.change_capabilities(|caps| {
            caps.effective = crate::process::CAP_FULL_SET & !without;
            Ok(())
        })
        .unwrap()
    };
    let (read_search, overrides) = (1 << CAP_DAC_READ_SEARCH, 1 << CAP_DAC_OVERRIDE);
    set(read_search | overrides);
    assert_eq!(walk(&proc, "shut/f"), Some(LxError::EACCES));
    set(overrides);
    assert_eq!(walk(&proc, "shut/f"), None);
    set(read_search);
    assert_eq!(walk(&proc, "shut/f"), None);
}

#[test]
fn a_walk_from_the_working_directory_searches_it_and_not_above_it() {
    let (proc, root, _) = a_process();
    let d = make(&root, "d", FileType::Dir, 0o755, 1000);
    let sub = make(&d, "sub", FileType::Dir, 0o755, 1000);
    make(&sub, "f", FileType::File, 0o644, 1000);
    become_user(&proc, 1000);
    proc.change_directory("/d/sub");
    own(&d, 0o600, 1000, 1000);
    assert_eq!(walk(&proc, "f"), None, "the cwd is not walked to again");
    assert_eq!(walk(&proc, ".."), None);
    assert_eq!(walk(&proc, "../sub/f"), Some(LxError::EACCES));
    assert_eq!(walk(&proc, "/d/sub/f"), Some(LxError::EACCES));
    // The kernel finding the cwd again asks nothing of anyone.
    assert!(!proc.working_directory_removed());
    own(&d, 0o755, 1000, 1000);
    own(&sub, 0o600, 1000, 1000);
    assert_eq!(walk(&proc, "f"), Some(LxError::EACCES), "the cwd itself is");
    assert_eq!(walk(&proc, "."), Some(LxError::EACCES));
    assert!(!proc.working_directory_removed());
}

#[test]
fn the_directory_of_a_last_name_is_searched_too() {
    // What `mkdir`, `unlink`, `rename` and the rest look their last name up
    // in: a directory, and one the caller may search.
    let (proc, root, _) = a_process();
    make(&root, "file", FileType::File, 0o644, 1000);
    become_user(&proc, 1000);
    let parent = |path| proc.lookup_parent_at(FileDesc::CWD, path).err();
    assert_eq!(parent("/shut"), Some(LxError::EACCES));
    assert_eq!(parent("shut"), Some(LxError::EACCES));
    assert_eq!(parent("/"), None);
    assert_eq!(parent("."), None);
    assert_eq!(parent("/file"), Some(LxError::ENOTDIR));
    // And from the cache, which keeps what the walk to it met.
    assert_eq!(parent("/shut"), Some(LxError::EACCES));
}

#[test]
fn a_walk_from_a_descriptor_searches_its_directory() {
    let (proc, root, _) = a_process();
    let d = make(&root, "d", FileType::Dir, 0o755, 1000);
    let sub = make(&d, "sub", FileType::Dir, 0o755, 1000);
    make(&sub, "f", FileType::File, 0o644, 1000);
    become_user(&proc, 1000);
    let fd = proc
        .add_file(File::new(
            sub.clone(),
            OpenFlags::RDONLY,
            String::from("/d/sub"),
        ))
        .unwrap();
    own(&d, 0o600, 1000, 1000);
    assert!(proc.lookup_inode_at(fd, "f", true).is_ok());
    own(&sub, 0o600, 1000, 1000);
    assert_eq!(
        proc.lookup_inode_at(fd, "f", true).err(),
        Some(LxError::EACCES)
    );
}

#[test]
fn a_link_is_followed_as_whoever_walks() {
    let (proc, root, _) = a_process();
    let to = make(&root, "to", FileType::SymLink, 0o777, 2000);
    to.write_at(0, b"/shut/f").unwrap();
    let rel = make(&root, "rel", FileType::SymLink, 0o777, 2000);
    rel.write_at(0, b"shut/f").unwrap();
    become_user(&proc, 1000);
    assert_eq!(walk(&proc, "/to"), Some(LxError::EACCES));
    assert_eq!(walk(&proc, "rel"), Some(LxError::EACCES));
    // Left in place, a link leads nowhere.
    assert!(proc.lookup_inode_at(FileDesc::CWD, "/to", false).is_ok());
}

#[test]
fn access_walks_as_the_real_user_and_the_kernel_as_nobody() {
    // setresuid(1000, 0, 0): root by its effective uid, which keeps the
    // capabilities, and uid 1000 to access(2).
    let (proc, _, _) = a_process();
    proc.set_resuid(1000, 0, 0).unwrap();
    let by = |walker| {
        proc.lookup_inode_at_by(FileDesc::CWD, "/shut/f", true, walker)
            .err()
    };
    assert_eq!(by(Walker::Caller), None);
    assert_eq!(by(Walker::RealIds), Some(LxError::EACCES));
    proc.set_resuid(1000, 1000, 1000).unwrap();
    assert_eq!(by(Walker::Caller), Some(LxError::EACCES));
    assert_eq!(by(Walker::Kernel), None);
}

#[test]
fn the_path_cache_asks_its_gates_of_each_walker() {
    // Root walks `/shut/f` and the cache keeps it; the same root without its
    // DAC capabilities, and a user, are each asked about `shut` again.
    let (proc, _, _) = a_process();
    assert_eq!(walk(&proc, "/shut/f"), None);
    proc.change_capabilities(|caps| {
        caps.effective &= !(1 << CAP_DAC_READ_SEARCH | 1 << CAP_DAC_OVERRIDE);
        Ok(())
    })
    .unwrap();
    assert_eq!(walk(&proc, "/shut/f"), Some(LxError::EACCES));
    proc.change_capabilities(|caps| {
        caps.effective = caps.permitted;
        Ok(())
    })
    .unwrap();
    assert_eq!(walk(&proc, "/shut/f"), None);
    become_user(&proc, 2000);
    assert_eq!(walk(&proc, "/shut/f"), None, "its owner");
}

#[test]
fn a_new_mode_of_a_directory_is_asked_about_from_then_on() {
    // A directory everybody may search is no gate, so a cached walk through
    // it asks nothing: changing it has to empty the cache.
    let (proc, root, _) = a_process();
    let open = make(&root, "open", FileType::Dir, 0o755, 2000);
    make(&open, "f", FileType::File, 0o644, 2000);
    become_user(&proc, 1000);
    assert_eq!(walk(&proc, "/open/f"), None);
    let mut meta = open.metadata().unwrap();
    meta.mode = 0o700;
    set_mode(&open, &meta).unwrap();
    assert_eq!(walk(&proc, "/open/f"), Some(LxError::EACCES));
}

#[test]
fn proc_self_exe_leads_to_the_program_whatever_is_on_the_way_to_it() {
    // A magic link: no directory on the way to the program is searched.
    let (proc, _, shut) = a_process();
    make(&shut, "prog", FileType::File, 0o755, 2000);
    proc.set_execute_path("/shut/prog");
    become_user(&proc, 1000);
    assert_eq!(walk(&proc, "/shut/prog"), Some(LxError::EACCES));
    assert_eq!(walk(&proc, "/proc/self/exe"), None);
    // And the walk after it asks again.
    assert_eq!(walk(&proc, "/shut/prog"), Some(LxError::EACCES));
}

/// Whether two walks ask the one question.
fn same(a: &Walked, b: &Walked) -> bool {
    match (&a.search, &b.search) {
        (Some(a), Some(b)) => core::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
        _ => false,
    }
}

#[test]
fn a_walk_takes_the_question_made_for_the_same_credentials() {
    // Copying the credentials into a question of each walk's own was most of
    // what asking cost a user: about 2,300 instructions an `open`.
    let (proc, _, _) = a_process();
    let ask = |walker| proc.walked_by(walker);
    assert!(
        ask(Walker::Caller).search.is_none(),
        "root may search anything"
    );
    proc.set_resuid(1000, 0, 0).unwrap();
    assert!(ask(Walker::Caller).search.is_none());
    assert!(same(&ask(Walker::RealIds), &ask(Walker::RealIds)));
    let drop = |cap: u32| {
        proc.change_capabilities(|caps| {
            caps.effective &= !(1 << cap);
            Ok(())
        })
        .unwrap()
    };
    drop(CAP_DAC_READ_SEARCH);
    drop(CAP_DAC_OVERRIDE);
    let first = ask(Walker::Caller);
    assert!(same(&first, &ask(Walker::Caller)));
    assert!(
        !same(&first, &ask(Walker::RealIds)),
        "each kind of ids its own"
    );
    drop(crate::process::CAP_CHOWN);
    assert!(
        !same(&first, &ask(Walker::Caller)),
        "new credentials, a new one"
    );
}

#[test]
fn a_process_searches_its_own_fd_directory_whatever_its_owner() {
    // `proc_fd_permission`: a process that is not dumpable finds its `fd`
    // root's and 0500, and still walks through it to its descriptors.
    let process = Process::create_with_fixed_id_ext(
        &Job::root(),
        45_391,
        "search",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let dir = super::super::proc_self::ProcSelfFdDir {
        process: process.clone(),
    };
    let mut meta = dir.metadata().unwrap();
    meta.mode = 0o500;
    meta.uid = 0;
    meta.gid = 0;
    let user = {
        let proc = LinuxProcess::new(RamFS::new(), 0);
        become_user(&proc, 1000);
        proc.credentials()
    };
    assert_eq!(may_search(&user, true, process.id(), &dir, &meta), Ok(()));
    assert_eq!(
        may_search(&user, true, process.id() + 1, &dir, &meta),
        Err(FsError::NoPermission)
    );
    // Any other directory with that owner and mode is refused to it.
    let (_, _, shut) = a_process();
    assert_eq!(
        may_search(&user, true, process.id(), &*shut, &meta),
        Err(FsError::NoPermission)
    );
}
