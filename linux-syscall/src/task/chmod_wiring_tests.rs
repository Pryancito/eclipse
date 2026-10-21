//! `chmod(2)` by path, asked through the syscall.
//!
//! The gate it leans on, `chmod_bits`, is pure and well covered: `dac_tests`
//! and `fsid_tests` in `linux-object` already fix the setuid bit the owner
//! keeps, the setgid bit a caller outside the file's group loses, root's
//! exemption from that rule, and the filesystem id being the one that
//! decides. What none of them can see is the **wiring** of `sys_chmod`:
//! whether it asks the gate about the file userspace named, whether its
//! refusal arrives before the mode is written, and whether the mode the gate
//! decided is the one that reaches the inode.
//!
//! `fchmod` and `fchmodat` got that treatment in
//! `file/file/permission_tests.rs`, but `sys_chmod` lives here and is its own
//! copy of those four lines: it walks a path instead of taking a descriptor,
//! and no test in the crate named it.
//!
//! What cannot be asked here: that it follows a symbolic link to the end
//! (ramfs has no links).
//!
//! Fixed pids live in 47_16x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// The set-user-ID and set-group-ID bits, as `chmod(1)` writes them.
const SETUID: u16 = 0o4000;
const SETGID: u16 = 0o2000;

/// `S_IFDIR` and `S_IFREG`: the file-type bits, which live above the
/// permission mask and are never a caller's to set.
const IS_DIR: u16 = 0o40000;
const IS_REG: u16 = 0o100000;

/// Somebody who is not root, and the group they are in.
const A_USER: u32 = 1000;
const A_GROUP: u32 = 1000;
/// A group `A_USER` is not in, which is what strips the setgid bit.
const ANOTHER_GROUP: u32 = 2000;
/// Somebody who is neither root nor `A_USER`.
const A_STRANGER: u32 = 3000;

/// A path nothing is at.
const A_MISSING_PATH: &str = "/chmod-no-such-file";

/// A NUL-terminated path, which is what `as_c_str` reads.
fn c(s: &str) -> alloc::vec::Vec<u8> {
    let mut bytes = alloc::vec::Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A file of `dir_path` named `name`, with exactly this owner and mode.
///
/// Owner and mode are written through `set_metadata` rather than left to
/// whatever `create` puts there, because every answer below is read off
/// them.
fn a_file_named(lp: &LinuxProcess, dir_path: &str, name: &str, uid: u32, gid: u32, mode: u16) {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = gid as _;
    metadata.mode = mode;
    inode.set_metadata(&metadata).unwrap();
}

/// [`a_file_named`] for the one file a test has, which is called `f`.
fn a_file_owned_by(lp: &LinuxProcess, dir_path: &str, uid: u32, gid: u32, mode: u16) {
    a_file_named(lp, dir_path, "f", uid, gid, mode)
}

/// The mode the file really has, asked of the filesystem and not of the
/// syscall under test.
fn mode_of(lp: &LinuxProcess, path: &str) -> u16 {
    lp.lookup_inode(path).unwrap().metadata().unwrap().mode
}

#[test]
fn the_mode_that_lands_is_the_one_the_gate_decided_and_not_the_one_asked_for() {
    // The whole point of a gate that answers a MODE instead of a yes or a
    // no. `setattr_prepare` strips `S_ISGID` from a chmod by someone outside
    // the file's group, "so that a chmod cannot give away group-execute
    // privilege it does not hold", and keeps `S_ISUID`, which is how an
    // unprivileged user makes a setuid binary of a file they own.
    //
    // So the call has to succeed AND land a different mode from the one it
    // was handed. A syscall that wrote the argument instead of the verdict
    // would pass a test that only looked at the return value: it returns 0
    // either way.
    //
    // And it is asked from BOTH sides of the group, because one side alone
    // cannot tell "strips setgid from a caller outside the group" from
    // "strips setgid from everyone who is not root" -- which is what this
    // used to do, silently, so `chmod 2755` left `0755` behind and a program
    // that read back the mode it had just set saw a different one. The two
    // files differ by their group and by nothing else.
    let proc = a_process(47_160, "chmod-verdict", "chmod_verdict");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let dir = "/chmod_verdict";
            a_file_named(&lp, dir, "mine", A_USER, A_GROUP, IS_REG | 0o644);
            a_file_named(&lp, dir, "theirs", A_USER, ANOTHER_GROUP, IS_REG | 0o644);
            // The groups go first: once the user id is down, moving the
            // group ids is `EPERM`, which is the whole point of them.
            lp.set_groups(alloc::vec::Vec::new());
            lp.set_resgid(A_GROUP, A_GROUP, A_GROUP).unwrap();
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            for (name, landed, why) in [
                (
                    "mine",
                    IS_REG | SETUID | SETGID | 0o755,
                    "setgid was stripped from a caller who is in the file group",
                ),
                (
                    "theirs",
                    IS_REG | SETUID | 0o755,
                    "setgid survived a caller outside the file group, or setuid did not",
                ),
            ] {
                let path = alloc::format!("{}/{}", dir, name);
                let at = c(&path);
                assert_eq!(
                    sys.sys_chmod(
                        UserInPtr::from(at.as_ptr() as usize),
                        (SETUID | SETGID | 0o755) as usize
                    ),
                    Ok(0),
                    "the owner was refused the mode of their own file"
                );
                assert_eq!(mode_of(&lp, &path), landed, "{}", why);
            }
        })
    });
}

#[test]
fn the_gate_is_asked_about_the_file_and_judged_against_the_acting_id() {
    // Two things at once, because they are the same mistake read from two
    // sides: the owner the gate is told about has to be the FILE's, and the
    // id it is judged against has to be the CALLER's filesystem id.
    //
    // A file owned by uid 1000 and group 2000 separates them. Handing the
    // gate the file's gid where its uid goes would refuse the owner; handing
    // it the caller's id where the file's goes would let a stranger through.
    // And the acting id is the filesystem one, not the effective one, which
    // is what `setfsuid` is for: a file server that wants the walk and the
    // verdict done as its client without becoming its client everywhere.
    //
    // So the caller keeps the owner's id in its SAVED slot -- which is what
    // makes `setfsuid(A_USER)` a move it is allowed -- and the effective id
    // stays the stranger's from the first call to the last. Nothing between
    // the two calls below moves except the filesystem id.
    let proc = a_process(47_161, "chmod-who", "chmod_who");
    let path = String::from("/chmod_who/f");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_owned_by(&lp, "/chmod_who", A_USER, ANOTHER_GROUP, IS_REG | 0o644);
            let at = c(&path);
            let at = || UserInPtr::from(at.as_ptr() as usize);

            // A stranger, whichever way round the ids are read.
            lp.set_resuid(A_STRANGER, A_STRANGER, A_USER).unwrap();
            assert_eq!(
                sys.sys_chmod(at(), 0o600),
                Err(LxError::EPERM),
                "somebody who owns nothing here set the mode"
            );

            // The owner, by filesystem id alone.
            assert_eq!(lp.set_fsuid(A_USER), A_STRANGER, "setfsuid was declined");
            assert_eq!(
                sys.sys_chmod(at(), 0o600),
                Ok(0),
                "the filesystem id of the owner was not the one that decided"
            );
            assert_eq!(mode_of(&lp, &path), IS_REG | 0o600);
            assert_eq!(
                lp.credentials().euid,
                A_STRANGER,
                "the effective id moved, so the filesystem id is not what decided"
            );
        })
    });
}

#[test]
fn a_chmod_the_gate_refuses_leaves_the_mode_exactly_as_it_was() {
    // The order half of the lens. A refusal that arrives after the write is
    // worse than no gate at all: the caller is told `EPERM` and the file
    // changed anyway, so nothing upstream knows to put it back.
    //
    // The file is left world-writable on purpose. Write access is not what
    // `chmod` asks for -- the mode is what grants access in the first place,
    // so a file left 0666 by mistake must not be one `chmod 777` away from
    // being world-anything-else.
    let proc = a_process(47_162, "chmod-order", "chmod_order");
    let path = String::from("/chmod_order/f");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_owned_by(&lp, "/chmod_order", 0, 0, IS_REG | 0o666);
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();
            let at = c(&path);

            assert_eq!(
                sys.sys_chmod(UserInPtr::from(at.as_ptr() as usize), 0o777),
                Err(LxError::EPERM)
            );
            assert_eq!(
                mode_of(&lp, &path),
                IS_REG | 0o666,
                "a refused chmod changed the mode anyway"
            );
        })
    });
}

#[test]
fn a_directory_is_chmoddable_and_keeps_the_bits_above_the_permission_mask() {
    // `chmod 755 somedir` is the ordinary case and no test in the crate
    // asked it: every `chmod`, `fchmod` and `fchmodat` test names a regular
    // file. The walk ends on a directory inode here instead of a file one,
    // and the gate keeps `cur_mode & !MODE_PERM_MASK`, so the syscall has to
    // write back the whole word the gate answered and not just the low bits
    // it cares about -- lose `S_IFDIR` out of a mode and `stat` stops
    // calling the thing a directory.
    //
    // The type bits have to be seeded here: ramfs keeps the type in
    // `metadata.type_` and leaves `mode` as the bare permissions, so a
    // directory it made reads `0o755` and not `0o40755`.
    //
    // Root does the asking, so the setgid rule cannot account for a
    // difference.
    let proc = a_process(47_163, "chmod-dir", "chmod_dir");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let inode = lp.lookup_inode("/chmod_dir").unwrap();
            let mut metadata = inode.metadata().unwrap();
            metadata.mode = IS_DIR | 0o755;
            inode.set_metadata(&metadata).unwrap();
            let at = c("/chmod_dir");

            assert_eq!(
                sys.sys_chmod(
                    UserInPtr::from(at.as_ptr() as usize),
                    (SETGID | 0o711) as usize
                ),
                Ok(0),
                "a directory could not be chmodded at all"
            );
            assert_eq!(
                mode_of(&lp, "/chmod_dir"),
                IS_DIR | SETGID | 0o711,
                "the bits above the permission mask did not come back"
            );
        })
    });
}

#[test]
fn a_relative_path_is_walked_from_the_working_directory_and_not_from_the_root() {
    // What makes `chmod` a different call from `fchmod` and not a wrapper
    // over it: it takes a path, and a path with no leading slash is the
    // working directory's. Walking it from the root instead would reach a
    // different file, or none -- and reaching NONE is the kind one notices,
    // while reaching a different file of the same name is not.
    //
    // So there are two files called `f` here, one in the working directory
    // and one at the root, and only the first may move.
    let proc = a_process(47_164, "chmod-cwd", "chmod_cwd");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_owned_by(&lp, "/chmod_cwd", 0, 0, IS_REG | 0o644);
            a_file_owned_by(&lp, "/", 0, 0, IS_REG | 0o644);
            lp.change_directory("/chmod_cwd");
            let at = c("f");

            assert_eq!(
                sys.sys_chmod(UserInPtr::from(at.as_ptr() as usize), 0o600),
                Ok(0)
            );
            assert_eq!(
                mode_of(&lp, "/chmod_cwd/f"),
                IS_REG | 0o600,
                "the relative path did not name the file of the working directory"
            );
            assert_eq!(
                mode_of(&lp, "/f"),
                IS_REG | 0o644,
                "the relative path was walked from the root"
            );
        })
    });
}

#[test]
fn a_path_that_names_nothing_is_enoent_and_a_null_one_is_efault() {
    // The two refusals that come before the gate, and in this order: the
    // path is read out of user memory first (`as_c_str`), then walked. A
    // null pointer is not an empty path and never reaches the walk.
    let proc = a_process(47_165, "chmod-nopath", "chmod_nopath");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let missing = c(A_MISSING_PATH);
            assert_eq!(
                sys.sys_chmod(UserInPtr::from(missing.as_ptr() as usize), 0o600),
                Err(LxError::ENOENT),
                "a path with nothing at it named something"
            );
            assert_eq!(
                sys.sys_chmod(UserInPtr::from(0usize), 0o600),
                Err(LxError::EFAULT),
                "a null path was read as a path"
            );
        })
    });
}

#[test]
fn only_the_low_sixteen_bits_of_the_mode_word_are_a_mode() {
    // `mode` arrives as a register and the kernel's own type for it is
    // `umode_t`, sixteen bits wide, so everything above them is dropped at
    // the boundary -- before the permission mask gets a look. It matters
    // because the two are not the same cut: `0o200644` keeps its `0o644`
    // through the truncation, while `0o200000` becomes a mode of zero and
    // takes every permission off the file.
    //
    // Root asks, so nothing here is the setgid rule.
    let proc = a_process(47_166, "chmod-wide", "chmod_wide");
    let path = String::from("/chmod_wide/f");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_owned_by(&lp, "/chmod_wide", 0, 0, IS_REG | 0o777);
            let at = c(&path);
            let at = || UserInPtr::from(at.as_ptr() as usize);

            assert_eq!(sys.sys_chmod(at(), 0o200_644), Ok(0));
            assert_eq!(
                mode_of(&lp, &path),
                IS_REG | 0o644,
                "the word above the mode did not fall off"
            );

            assert_eq!(sys.sys_chmod(at(), 0o200_000), Ok(0));
            assert_eq!(
                mode_of(&lp, &path),
                IS_REG,
                "a mode of nothing but high bits left permissions behind"
            );
        })
    });
}
