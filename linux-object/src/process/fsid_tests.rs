//! `setfsuid(2)`/`setfsgid(2)`, and the id a file access is really
//! checked against.
//!
//! Linux keeps `fsuid` apart from `euid` on purpose;
//! `generic_permission()` says why in its own comment: "We use `fsuid`
//! for this, letting us set arbitrary permissions for filesystem access
//! without changing the 'normal' uids which are used for other things."
//! A file server takes an id from the wire, wears it while it touches the
//! file, and puts it back -- without giving up the privileges it needs
//! for its own sockets and its own log.
//!
//! This kernel had no `fsuid` at all. `setfsuid` took the argument,
//! ignored it and returned the caller's `euid`, which is **exactly what a
//! call that worked looks like**: the syscall cannot fail, so it returns
//! the previous id either way and there is no error for the caller to
//! check. The server went on reading the file as root, with nothing
//! anywhere saying so.
//!
//! Three parts, then: who may move the id, who drags it along (every
//! `set*id` path and `execve`, where a forgotten line is a privilege the
//! caller believes it put down), and what actually asks it.

use super::dup_fd_tests::a_process;
use super::*;
use rcore_fs::vfs::{FileType, FsError, PollStatus, Timespec};

const REAL: u32 = 1000;
const SAVED: u32 = 1500;
const OTHER: u32 = 2000;
const SPOOL: u32 = 4242;

/// The credentials of an ordinary, untainted process.
fn creds(ruid: u32, euid: u32, suid: u32) -> Credentials {
    Credentials {
        ruid,
        euid,
        suid,
        rgid: ruid,
        egid: euid,
        sgid: suid,
        fsuid: euid,
        fsgid: euid,
        groups: Vec::new(),
        umask: 0o022,
        caps: Capabilities::for_uids(ruid, euid, suid),
    }
}

/// A live process wearing those ids.
fn process_as(ruid: u32, euid: u32, suid: u32) -> LinuxProcess {
    let proc = a_process();
    proc.inner.lock().credentials = creds(ruid, euid, suid);
    proc
}

fn a_metadata(mode: u16, uid: u32, gid: u32) -> Metadata {
    Metadata {
        dev: 1,
        inode: 7,
        size: 0,
        blk_size: 4096,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::File,
        mode,
        nlinks: 1,
        uid: uid as _,
        gid: gid as _,
        rdev: 0,
    }
}

/// An inode that remembers who it ended up belonging to.
struct Owned(Mutex<Metadata>);

impl Owned {
    fn new() -> Arc<Self> {
        Arc::new(Owned(Mutex::new(a_metadata(0o644, NO_ID, NO_ID))))
    }
}

impl INode for Owned {
    fn read_at(&self, _: usize, _: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Err(FsError::NotSupported)
    }
    fn write_at(&self, _: usize, _: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Err(FsError::NotSupported)
    }
    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        Ok(self.0.lock().clone())
    }
    fn set_metadata(&self, metadata: &Metadata) -> rcore_fs::vfs::Result<()> {
        *self.0.lock() = metadata.clone();
        Ok(())
    }
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

// ---- who may move it ---------------------------------------------

#[test]
fn the_id_already_in_force_is_in_the_set_and_in_no_other_rule() {
    // The one thing that tells `setfsid_allowed` apart from the rule
    // every other `set*id` call uses. Without it a process whose
    // filesystem id sits somewhere the rest of its ids never were could
    // not name that id again -- not even to ask for it back.
    assert!(
        LinuxProcess::setfsid_allowed(REAL, REAL, REAL, SPOOL, SPOOL),
        "the acting id must be nameable"
    );
    assert!(
        !LinuxProcess::set_any_allowed(REAL, REAL, REAL, SPOOL),
        "and no other rule accepts it, which is why this one exists"
    );
}

#[test]
fn the_three_ordinary_ids_are_in_the_set_too() {
    for id in [REAL, OTHER, SAVED] {
        assert!(
            LinuxProcess::setfsid_allowed(REAL, OTHER, SAVED, REAL, id),
            "{} is one of the caller's own ids",
            id
        );
    }
}

#[test]
fn an_id_the_caller_never_held_is_refused() {
    assert!(!LinuxProcess::setfsid_allowed(
        REAL, OTHER, SAVED, REAL, SPOOL
    ));
}

#[test]
fn setfsuid_answers_with_the_id_that_was_in_force_not_the_new_one() {
    // `old_fsuid` is read before anything is decided and is what every
    // return path hands back; a program keeps it to put the id back.
    let proc = process_as(REAL, ROOT_UID, ROOT_UID);
    assert_eq!(proc.set_fsuid(REAL), ROOT_UID);
    assert_eq!(proc.fsuid(), REAL);
    assert_eq!(proc.set_fsuid(ROOT_UID), REAL, "and again on the way back");
    assert_eq!(proc.fsuid(), ROOT_UID);
}

#[test]
fn dropping_to_another_user_for_file_work_leaves_the_other_ids_alone() {
    // The whole point of the call: still root for everything that is not
    // a file.
    let proc = process_as(REAL, ROOT_UID, ROOT_UID);
    proc.set_fsuid(REAL);
    assert_eq!(proc.fsuid(), REAL);
    assert_eq!(proc.euid(), ROOT_UID);
    assert_eq!(proc.uid(), REAL);
    assert_eq!(proc.suid(), ROOT_UID);
}

#[test]
fn minus_one_is_the_query_the_man_page_tells_you_to_make() {
    // `if (!uid_valid(kuid)) return old_fsuid;`. Since the call cannot
    // report an error, `setfsuid(-1)` is how a careful program finds out
    // whether the previous one took.
    let proc = process_as(REAL, ROOT_UID, ROOT_UID);
    proc.set_fsuid(REAL);
    assert_eq!(proc.set_fsuid(NO_ID), REAL);
    assert_eq!(proc.fsuid(), REAL, "and it changed nothing");
}

#[test]
fn an_id_the_caller_never_held_leaves_the_filesystem_id_where_it_was() {
    let proc = process_as(REAL, REAL, REAL);
    assert_eq!(proc.set_fsuid(SPOOL), REAL);
    assert_eq!(
        proc.fsuid(),
        REAL,
        "refused, and the only sign of it is that the id did not move"
    );
}

#[test]
fn root_may_move_it_anywhere_because_of_cap_setuid() {
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    assert_eq!(proc.set_fsuid(SPOOL), ROOT_UID);
    assert_eq!(proc.fsuid(), SPOOL);
}

#[test]
fn the_group_half_runs_the_same_rule_with_cap_setgid() {
    let privileged = process_as(REAL, ROOT_UID, ROOT_UID);
    assert_eq!(privileged.set_fsgid(SPOOL), ROOT_UID);
    assert_eq!(privileged.fsgid(), SPOOL, "root, so the capability decides");

    let user = process_as(REAL, REAL, SAVED);
    assert_eq!(user.set_fsgid(SPOOL), REAL);
    assert_eq!(user.fsgid(), REAL, "no capability, and not one of its ids");
    assert_eq!(user.set_fsgid(SAVED), REAL);
    assert_eq!(user.fsgid(), SAVED, "the saved gid is in the set");
}

#[test]
fn a_move_taints_the_process_and_a_refusal_does_not() {
    // `commit_creds()` runs the same dumpability check on `fsuid` as on
    // the other ids, and `abort_creds()` never reaches it.
    let moved = process_as(REAL, ROOT_UID, ROOT_UID);
    moved.set_fsuid(REAL);
    assert!(moved.is_sugid());

    let refused = process_as(REAL, REAL, REAL);
    refused.set_fsuid(SPOOL);
    assert!(!refused.is_sugid());

    let noop = process_as(REAL, REAL, REAL);
    noop.set_fsuid(REAL);
    assert!(
        !noop.is_sugid(),
        "asking for the id already in force builds no credentials at all"
    );
}

// ---- who drags it along ------------------------------------------
//
// Every `set*id` path in `kernel/sys.c` ends `new->fsuid = new->euid;`.
// A path that forgets it leaves a process that dropped to `nobody` still
// reading files as root: the same hole from the other side.

#[test]
fn setuid_drags_the_filesystem_id_along() {
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsuid(SPOOL);
    proc.set_uid(REAL).unwrap();
    assert_eq!(proc.fsuid(), REAL);
}

#[test]
fn setreuid_drags_it_too() {
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsuid(SPOOL);
    proc.set_reuid(NO_ID, REAL).unwrap();
    assert_eq!(proc.fsuid(), REAL);
}

#[test]
fn setresuid_drags_it_too() {
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsuid(SPOOL);
    proc.set_resuid(NO_ID, REAL, NO_ID).unwrap();
    assert_eq!(proc.fsuid(), REAL);
}

#[test]
fn setgid_drags_the_filesystem_group_along() {
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsgid(SPOOL);
    proc.set_gid(REAL).unwrap();
    assert_eq!(proc.fsgid(), REAL);
}

#[test]
fn setregid_drags_it_too() {
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsgid(SPOOL);
    proc.set_regid(NO_ID, REAL).unwrap();
    assert_eq!(proc.fsgid(), REAL);
}

#[test]
fn setresgid_drags_it_too() {
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsgid(SPOOL);
    proc.set_resgid(NO_ID, REAL, NO_ID).unwrap();
    assert_eq!(proc.fsgid(), REAL);
}

#[test]
fn a_switch_that_names_no_effective_id_still_brings_the_pair_in_step() {
    // `setreuid(ruid, -1)` moves the REAL uid and nothing else, and
    // `__sys_setreuid` still ends `new->fsuid = new->euid;`. So the line
    // belongs at the end of the path and not beside the write to `euid`:
    // a filesystem id that had wandered comes home even though no
    // effective id was named.
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsuid(SPOOL);
    proc.set_reuid(REAL, NO_ID).unwrap();
    assert_eq!(proc.euid(), ROOT_UID, "the effective id did not move");
    assert_eq!(proc.fsuid(), ROOT_UID, "and the filesystem id came back");
}

#[test]
fn an_exec_puts_the_filesystem_id_back_on_the_effective_one() {
    // `cap_bprm_creds_from_file()`: `new->suid = new->fsuid = new->euid;`
    // `new->sgid = new->fsgid = new->egid;`. An image inherits the id its
    // accesses are checked against; it does not inherit a stray one the
    // caller happened to be wearing.
    let mut inner = LinuxProcessInner::default();
    inner.credentials = creds(REAL, REAL, REAL);
    inner.credentials.fsuid = SPOOL;
    inner.credentials.fsgid = SPOOL;
    inner.apply_exec_ids_from_a_normal_mount(0o755, OTHER, OTHER);
    assert_eq!(inner.credentials.fsuid, REAL);
    assert_eq!(inner.credentials.fsgid, REAL);
}

#[test]
fn a_setuid_image_hands_its_own_id_to_the_filesystem_too() {
    let mut inner = LinuxProcessInner::default();
    inner.credentials = creds(REAL, REAL, REAL);
    inner.apply_exec_ids_from_a_normal_mount(0o4755, ROOT_UID, OTHER);
    assert_eq!(inner.credentials.euid, ROOT_UID);
    assert_eq!(inner.credentials.fsuid, ROOT_UID);
}

// ---- what asks it ------------------------------------------------

#[test]
fn a_files_permission_bits_are_weighed_against_the_filesystem_id() {
    // Root that has dropped its filesystem id gets the OTHER bits of a
    // file it does not own, exactly like the user it stands in for.
    // Before this change the same process was still root here: a server
    // asked to read `/root/.ssh/id_rsa` on behalf of user 1000 read it.
    let mut root = creds(ROOT_UID, ROOT_UID, ROOT_UID);
    root.fsuid = REAL;
    root.fsgid = REAL;
    // And the filesystem capabilities with it, as `setfsuid` does.
    root.caps.follow_setfsuid(ROOT_UID, REAL);
    assert_eq!(
        LinuxProcess::access_verdict(&root, OTHER, OTHER, 0o600, false, ACCESS_WRITE, true),
        Err(LxError::EACCES)
    );
    assert_eq!(
        LinuxProcess::access_verdict(
            &creds(ROOT_UID, ROOT_UID, ROOT_UID),
            OTHER,
            OTHER,
            0o600,
            false,
            ACCESS_WRITE,
            true
        ),
        Ok(()),
        "and with the id left alone it is still root"
    );
}

#[test]
fn the_owner_bits_go_to_whoever_the_filesystem_id_names() {
    let mut c = creds(REAL, REAL, REAL);
    c.fsuid = OTHER;
    assert_eq!(
        LinuxProcess::access_verdict(&c, OTHER, SPOOL, 0o600, false, ACCESS_WRITE, true),
        Ok(()),
        "the file's owner is the id being acted as"
    );
}

#[test]
fn the_group_bits_follow_the_filesystem_group() {
    let mut c = creds(REAL, REAL, REAL);
    c.fsgid = SPOOL;
    assert_eq!(
        LinuxProcess::access_verdict(&c, OTHER, SPOOL, 0o060, false, ACCESS_WRITE, true),
        Ok(())
    );
    c.fsgid = REAL;
    assert_eq!(
        LinuxProcess::access_verdict(&c, OTHER, SPOOL, 0o060, false, ACCESS_WRITE, true),
        Err(LxError::EACCES)
    );
}

#[test]
fn access_2_still_asks_the_real_ids_and_not_the_filesystem_ones() {
    // `access(2)` is the one caller that deliberately asks as the real
    // user; a wandering filesystem id must not answer for it.
    let mut c = creds(REAL, ROOT_UID, ROOT_UID);
    c.fsuid = OTHER;
    c.fsgid = OTHER;
    assert_eq!(
        LinuxProcess::access_verdict(&c, OTHER, OTHER, 0o600, false, ACCESS_WRITE, false),
        Err(LxError::EACCES),
        "the real uid owns nothing here"
    );
}

#[test]
fn a_chmod_is_refused_to_a_filesystem_id_that_does_not_own_the_file() {
    // `inode_owner_or_capable()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
    let mut root = creds(ROOT_UID, ROOT_UID, ROOT_UID);
    root.fsuid = REAL;
    root.caps.follow_setfsuid(ROOT_UID, REAL);
    assert_eq!(
        LinuxProcess::chmod_bits(&root, OTHER, OTHER, 0o644, 0o600),
        Err(LxError::EPERM)
    );
    root.fsuid = OTHER;
    assert_eq!(
        LinuxProcess::chmod_bits(&root, OTHER, OTHER, 0o644, 0o600),
        Ok(0o600),
        "and allowed once the acting id is the owner"
    );
}

#[test]
fn a_new_file_belongs_to_the_id_its_creator_was_acting_as() {
    // `inode_init_owner()`: `inode_fsuid_set()` / `inode_fsgid_set()`. A
    // server that creates a file on a user's behalf must leave it owned
    // by the user, not by the server.
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsuid(REAL);
    proc.set_fsgid(SPOOL);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, None, 0o644, false)
        .unwrap();
    let meta = inode.metadata().unwrap();
    assert_eq!(meta.uid as u32, REAL);
    assert_eq!(meta.gid as u32, SPOOL);
}

#[test]
fn a_chown_is_weighed_against_the_filesystem_id() {
    // `chown_ok()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsuid(REAL);
    let mut meta = a_metadata(0o644, OTHER, OTHER);
    assert_eq!(
        proc.chown_metadata(&mut meta, NO_ID, SPOOL),
        Err(LxError::EPERM),
        "not root here, and not the owner either"
    );
}

#[test]
fn the_sticky_bit_asks_the_filesystem_id_too() {
    // `__check_sticky()` opens with `kuid_t fsuid = current_fsuid();`.
    let dir = a_metadata(0o1777, OTHER, OTHER);
    let victim = a_metadata(0o644, OTHER, OTHER);
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    proc.set_fsuid(REAL);
    assert_eq!(proc.check_sticky(&dir, &victim), Err(LxError::EPERM));
    proc.set_fsuid(ROOT_UID);
    assert_eq!(proc.check_sticky(&dir, &victim), Ok(()));
}

#[test]
fn the_pair_cannot_be_moved_one_at_a_time_by_hand() {
    // `Credentials::set_euid` is the only line in this kernel that writes
    // a filesystem id from an effective one, so a caller holding bare
    // credentials cannot separate them by accident.
    let mut c = creds(REAL, REAL, REAL);
    c.set_euid(OTHER);
    assert_eq!(c.fsuid, OTHER);
    c.set_egid(SPOOL);
    assert_eq!(c.fsgid, SPOOL);
}

// ---- the set-group-ID bit a new file may not ask for -------------

/// A set-group-ID directory, which is how a shared area says "everything
/// made in here belongs to this group".
fn a_shared_directory(gid: u32) -> Metadata {
    let mut meta = a_metadata(0o2775, OTHER, gid);
    meta.type_ = FileType::Dir;
    meta
}

/// A process acting as `fsgid`, in no supplementary group.
///
/// The id goes in directly rather than through `set_fsgid`, which would
/// refuse a group this process never held -- which is its job, and the
/// reason a file server gets the id from somewhere else entirely.
fn creator_acting_as(fsgid: u32) -> LinuxProcess {
    let proc = process_as(REAL, REAL, REAL);
    proc.inner.lock().credentials.fsgid = fsgid;
    proc
}

#[test]
fn a_set_group_id_executable_cannot_be_asked_for_in_a_group_the_creator_is_not_in() {
    // `mode_strip_sgid()`. The gid of a new file in a set-group-ID
    // directory comes from the DIRECTORY, so this one call asks for a
    // set-group-ID executable owned by a group its creator does not hold --
    // and `apply_exec_ids` honours `S_ISGID`, so whoever runs it holds the
    // group. `open(path, O_CREAT, 0o2755)` is all it takes.
    let proc = creator_acting_as(REAL);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o2755, false)
        .unwrap();
    let meta = inode.metadata().unwrap();
    assert_eq!(
        meta.gid as u32, SPOOL,
        "the group still comes from the directory"
    );
    assert_eq!(
        meta.mode & 0o7777,
        0o0755,
        "but the set-group-ID bit is not handed over: {:o}",
        meta.mode & 0o7777
    );
}

#[test]
fn a_member_of_the_group_keeps_the_set_group_id_bit() {
    // The other side of the same rule, and the reason it is a condition and
    // not a mask: a member of the group may make a set-group-ID program of
    // it, which is what the bit is for.
    let proc = creator_acting_as(SPOOL);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o2755, false)
        .unwrap();
    assert_eq!(inode.metadata().unwrap().mode & 0o7777, 0o2755);

    // And a supplementary group counts, which is what `in_group_p()` reads.
    let proc = creator_acting_as(REAL);
    proc.inner.lock().credentials.groups.push(SPOOL);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o2755, false)
        .unwrap();
    assert_eq!(inode.metadata().unwrap().mode & 0o7777, 0o2755);
}

#[test]
fn without_the_group_execute_bit_the_set_group_id_bit_is_left_alone() {
    // `(mode & (S_ISGID | S_IXGRP)) != (S_ISGID | S_IXGRP)` returns early.
    // On a file nobody can execute, `S_ISGID` is the System V mandatory
    // locking convention and gives away nothing, so stripping it would
    // break a program that works on Linux -- and break it silently, since
    // the create still succeeds.
    let proc = creator_acting_as(REAL);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o2644, false)
        .unwrap();
    assert_eq!(inode.metadata().unwrap().mode & 0o7777, 0o2644);
}

#[test]
fn a_new_directory_still_inherits_the_bit_from_its_parent() {
    // The exemption, and the rule the strip must not eat: on a directory
    // `S_ISGID` is `inode_init_owner`'s inheritance -- where the files made
    // inside it will land -- and not a privilege the caller asked for.
    // Linux returns early on `S_ISDIR` for exactly this reason.
    let proc = creator_acting_as(REAL);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o0775, true)
        .unwrap();
    let meta = inode.metadata().unwrap();
    assert_eq!(meta.gid as u32, SPOOL);
    assert_eq!(
        meta.mode & 0o7777,
        0o2775,
        "a subdirectory of a shared area is shared too"
    );
}

#[test]
fn a_new_directory_may_ask_for_the_bit_in_a_group_it_does_not_hold() {
    // The exemption again, this time with the caller ASKING for the bit
    // rather than inheriting it, which is the half the inheritance test
    // cannot see. `mkdir("/shared/sub", 02775)` is what `install -d -g` and
    // every "make the rest of this tree shared too" script do, and Linux
    // lets it through because `S_ISDIR(mode)` returns before any of the
    // ownership questions are asked.
    let proc = creator_acting_as(REAL);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o2775, true)
        .unwrap();
    assert_eq!(inode.metadata().unwrap().mode & 0o7777, 0o2775);
}

#[test]
fn root_may_still_ask_for_it() {
    // `in_group_or_capable()`: `CAP_FSETID`. An installer running as root
    // creates set-group-ID programs in group directories for a living.
    let proc = process_as(ROOT_UID, ROOT_UID, ROOT_UID);
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o2755, false)
        .unwrap();
    assert_eq!(inode.metadata().unwrap().mode & 0o7777, 0o2755);
    // It is the capability, not the uid: root that put CAP_FSETID down is
    // outside the group like anybody else.
    proc.change_capabilities(|caps| {
        caps.effective &= !(1 << CAP_FSETID);
        Ok(())
    })
    .unwrap();
    let inode: Arc<dyn INode> = Owned::new();
    proc.initialize_created_metadata(&inode, Some(&a_shared_directory(SPOOL)), 0o2755, false)
        .unwrap();
    assert_eq!(inode.metadata().unwrap().mode & 0o7777, 0o0755);
}

/// An inode whose stored mode already carries its file type, the way a real
/// filesystem's does. [`Owned::new`] stores a bare `0o644`, which is one of
/// the two reasons the masks below went unmeasured.
fn an_inode_with_mode(mode: u16) -> Arc<Owned> {
    Arc::new(Owned(Mutex::new(a_metadata(mode, NO_ID, NO_ID))))
}

#[test]
fn only_the_permission_bits_of_what_was_asked_for_reach_the_inode() {
    // Two masks, one on each side of the mode being assembled, and neither
    // had a test: the bits of the ARGUMENT above `S_IALLUGO` must not land
    // in the inode's mode -- a caller that reused a `st_mode` brings
    // `S_IFREG` or `S_IFDIR` along inside it -- and the bits the INODE
    // already has above it must survive, because that is where its own file
    // type lives.
    //
    // The two differ here on purpose, a regular file asked for with the
    // directory bit, because that is the only shape in which "the mask was
    // not applied" and "the mask ate what it was keeping" both look
    // different from the right answer. With the same type on both sides the
    // three agree and the test proves nothing.
    const S_IFREG: u16 = 0o100_000;
    const S_IFDIR: u16 = 0o040_000;
    let proc = creator_acting_as(REAL);
    let inode: Arc<dyn INode> = an_inode_with_mode(S_IFREG | 0o644);
    proc.initialize_created_metadata(&inode, None, S_IFDIR | 0o755, false)
        .unwrap();
    assert_eq!(
        inode.metadata().unwrap().mode,
        S_IFREG | 0o755,
        "the inode keeps its own type and takes only the permission bits"
    );
}
