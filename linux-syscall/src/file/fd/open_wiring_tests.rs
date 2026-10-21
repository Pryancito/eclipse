//! `open` and `openat`, through the syscall: the flag word, the mode and the
//! directory a relative name starts from, each one followed to the place that
//! reads it.
//!
//! `open_flag_tests` next door covers `open_flags`, `path_open_flags`,
//! `open_resolved_type`, `o_path_holds_the_link`, `nodev_applies_to` and
//! `truncates_on_open`, which between them are every verdict this open makes
//! -- and all of it is reached by calling those functions. What a pure test
//! of a verdict cannot see is the three things around it:
//!
//! **That the call asks the question userspace asked.** `sys_openat` does not
//! call `open_flags`: it calls `openat_flags`, which adds the `O_LARGEFILE`
//! that every 64-bit kernel forces on (`force_o_largefile`), so `F_GETFL` of
//! a file opened `O_RDONLY` reports `0100000` as Linux 6.18 does. The two
//! functions are one word apart and no pure test tells them apart.
//!
//! **That a refusal arrives before anything is changed.** An unnamed flag
//! bit, an `O_EXCL` over a name that exists, a directory the caller may not
//! write in, an `O_TRUNC` of a file they may not write: each has to answer
//! before the file is created, emptied, or a descriptor spent.
//!
//! **That what the verdict decided reaches the file.** The mode an `O_CREAT`
//! open asks for arrives at the inode through `S_IALLUGO` and the umask; the
//! `O_TRUNC` it allows actually resizes; the `O_CLOEXEC` goes to the
//! descriptor and not to the description; and an `O_CREAT` over a symbolic
//! link writes into what the link points AT, which is the one case where
//! `find` hands back the wrong inode to open.
//!
//! Fixed pids live in 47_40x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `open(2)` flags, as `<asm-generic/fcntl.h>` gives them. `O_RDONLY` is the
/// absence of the other two and is written as a plain 0 below.
const O_WRONLY: usize = 0o1;
const O_RDWR: usize = 0o2;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_TRUNC: usize = 0o1000;
/// The bit the kernel adds to every open of its own accord, and the one
/// `O_PATH` takes away again with the rest.
const O_LARGEFILE: usize = 0o100000;
const O_DIRECTORY: usize = 0o200000;
const O_NOFOLLOW: usize = 0o400000;
const O_NOATIME: usize = 0o1000000;
const O_CLOEXEC: usize = 0o2000000;
const O_PATH: usize = 0o10000000;
/// `O_TMPFILE`, which this kernel does not name: it changes what the call
/// CREATES, and nothing here creates that. It is the bit that must come back
/// `EINVAL` rather than as an ordinary descriptor.
const O_TMPFILE: usize = 0o20000000;

/// `fcntl(2)` commands: the flags of the description, and the one flag of the
/// descriptor.
const F_GETFD: usize = 1;
const F_GETFL: usize = 3;
/// `FD_CLOEXEC`, which is 1 and not `O_CLOEXEC`: different words.
const FD_CLOEXEC: usize = 1;

/// The mode a test passes when the mode is not what it is about.
const A_PLAIN_MODE: usize = 0o644;
/// Somebody who is not root, because root is allowed anyway.
const A_USER: u32 = 1000;
/// Somebody else again, who owns the files `A_USER` is refused.
const ANOTHER_USER: u32 = 999;
/// A descriptor nobody opened.
const A_FD_OF_NOBODY: usize = 90;

/// A process with a `/<top>` of its own, because a lookup from the root goes
/// through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A path as userspace writes one: NUL-terminated.
fn c(s: &str) -> Vec<u8> {
    let mut bytes = Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

/// Who the process is from here on. Only ever called once per process: this
/// kernel refuses a change of real uid from anyone but root, as Linux does.
fn acting_as(sys: &Syscall<'_>, uid: u32) {
    let lp = sys.linux_process();
    lp.set_groups(Vec::new());
    lp.set_resgid(uid, uid, uid).unwrap();
    lp.set_resuid(uid, uid, uid).unwrap();
}

/// A file of `dir_path` with exactly this owner, mode and contents.
fn a_file_with(lp: &LinuxProcess, dir_path: &str, name: &str, uid: u32, mode: u16, bytes: &[u8]) {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    inode.write_at(0, bytes).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = uid as _;
    metadata.mode = mode;
    inode.set_metadata(&metadata).unwrap();
}

/// A symbolic link of `dir_path` pointing at `to`, which is read relative to
/// the directory the link is in.
fn a_link_named(lp: &LinuxProcess, dir_path: &str, name: &str, to: &str) {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::SymLink, 0o777).unwrap();
    inode.write_at(0, to.as_bytes()).unwrap();
}

/// The owner and mode of a directory that is already there, so a test can ask
/// what an open is allowed to do in it.
fn owned_with_mode(lp: &LinuxProcess, path: &str, uid: u32, mode: u16) {
    let inode = lp.lookup_inode(path).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = uid as _;
    metadata.mode = mode;
    inode.set_metadata(&metadata).unwrap();
}

/// The permission bits of what `path` names, which is where the mode an
/// `O_CREAT` open asked for has to end up.
fn mode_of(lp: &LinuxProcess, path: &str) -> u16 {
    lp.lookup_inode(path).unwrap().metadata().unwrap().mode
}

/// What `path` names, read straight off the inode rather than through a
/// descriptor, and without following a link on the last component.
fn type_of(lp: &LinuxProcess, path: &str) -> FileType {
    lp.lookup_inode_at(FileDesc::CWD, path, false)
        .unwrap()
        .metadata()
        .unwrap()
        .type_
}

/// The first `want` bytes of a file, by name: what an `O_TRUNC` empties and
/// what a write through a symbolic link lands in.
fn content_of(lp: &LinuxProcess, path: &str, want: usize) -> Vec<u8> {
    let inode = lp.lookup_inode(path).unwrap();
    let mut buf = alloc::vec![0u8; want];
    let got = inode.read_at(0, &mut buf).unwrap();
    buf.truncate(got);
    buf
}

/// Whether anything is there at all, which is how a refusal is told from a
/// creation that happened and was then undone.
fn exists(lp: &LinuxProcess, path: &str) -> bool {
    lp.lookup_inode(path).is_ok()
}

/// The same `Syscall` with a mutable borrow, which the dispatch table needs.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A syscall made the way userspace makes it: by number, through the table.
async fn by_number(sys: &Syscall<'_>, which: crate::Sys, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

/// `openat(2)` with every argument spelled out.
fn openat(
    sys: &Syscall<'_>,
    dir_fd: FileDesc,
    path: &[u8],
    flags: usize,
    mode: usize,
) -> SysResult {
    sys.sys_openat(dir_fd, UserInPtr::from(path.as_ptr() as usize), flags, mode)
}

/// `openat(2)` relative to the working directory, which is what `open(2)` is.
fn open(sys: &Syscall<'_>, path: &[u8], flags: usize) -> SysResult {
    openat(sys, FileDesc::CWD, path, flags, A_PLAIN_MODE)
}

/// The status flags of the open file description, asked the way a program
/// asks: `fcntl(F_GETFL)`.
async fn status_flags(sys: &Syscall<'_>, fd: usize) -> SysResult {
    sys.sys_fcntl(FileDesc::from(fd), F_GETFL, 0).await
}

/// Whether this DESCRIPTOR is close-on-exec, which is not a status flag.
async fn is_cloexec(sys: &Syscall<'_>, fd: usize) -> Result<bool, LxError> {
    Ok(sys.sys_fcntl(FileDesc::from(fd), F_GETFD, 0).await? & FD_CLOEXEC != 0)
}

async fn write_to(sys: &Syscall<'_>, fd: usize, bytes: &[u8]) -> SysResult {
    sys.sys_write(
        FileDesc::from(fd),
        UserInPtr::from(bytes.as_ptr() as usize),
        bytes.len(),
    )
    .await
}

async fn read_from(sys: &Syscall<'_>, fd: usize, len: usize) -> Result<Vec<u8>, LxError> {
    let mut buf = alloc::vec![0u8; len];
    let got = sys
        .sys_read(
            FileDesc::from(fd),
            UserOutPtr::from(buf.as_mut_ptr() as usize),
            len,
        )
        .await?;
    buf.truncate(got);
    Ok(buf)
}

#[test]
fn every_open_carries_o_largefile_and_an_o_path_open_carries_nothing_else() {
    // `sys_openat` calls `openat_flags`, not `open_flags`: the difference is
    // the `O_LARGEFILE` that `build_open_how` forces on for every 64-bit
    // kernel. musl passes the bit on every open and glibc does not, so
    // without this a glibc program's `F_GETFL` said `0` where Linux 6.18
    // says `0100000` -- and a pure test of either function cannot tell which
    // of the two the syscall called.
    let proc = a_process(47_400, "open-largefile", "open_largefile");
    let dir = String::from("/open_largefile");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = c(&alloc::format!("{dir}/f"));
            let made = open(sys, &path, O_CREAT | O_RDWR).unwrap();
            assert_eq!(sys.sys_close(FileDesc::from(made)), Ok(0));

            let read_only = open(sys, &path, 0).unwrap();
            assert_eq!(
                status_flags(sys, read_only).await,
                Ok(O_LARGEFILE),
                "an O_RDONLY open reports O_LARGEFILE and nothing else"
            );

            // The creation flags are not status flags (`status()` drops
            // them), the access mode is, and `O_CLOEXEC` belongs to the
            // descriptor.
            let written = open(sys, &path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC).unwrap();
            assert_eq!(
                status_flags(sys, written).await,
                Ok(O_LARGEFILE | O_RDWR),
                "O_CREAT, O_TRUNC and O_CLOEXEC are not status flags"
            );
            assert_eq!(
                is_cloexec(sys, written).await,
                Ok(true),
                "the O_CLOEXEC of an open reaches the descriptor"
            );

            // `path_open_flags` keeps three bits of an `O_PATH` open and
            // drops every other, the access mode and the `O_LARGEFILE` just
            // added among them. So the bit has to go in before that runs:
            // added afterwards it would show up here.
            let as_a_path = open(sys, &c(&dir), O_PATH | O_DIRECTORY | O_RDWR).unwrap();
            assert_eq!(
                status_flags(sys, as_a_path).await,
                Ok(O_PATH | O_DIRECTORY),
                "a path descriptor keeps O_DIRECTORY and nothing else"
            );
        })
    });
}

#[test]
fn a_flag_this_kernel_does_not_name_is_refused_before_the_path_is_walked() {
    // `O_TMPFILE` asks for an unnamed file, which nothing here makes.
    // `from_bits_truncate` alone would drop the bit in silence and hand back
    // an ordinary descriptor to a NAMED file -- the silent lie. And the
    // refusal has to come out of the flag word alone, before the walk, or a
    // caller probing an old kernel gets `ENOENT` for a path that is fine.
    let proc = a_process(47_401, "open-unnamed-flag", "open_unnamed_flag");
    let dir = String::from("/open_unnamed_flag");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            assert_eq!(
                open(sys, &c("/nowhere/at/all"), O_TMPFILE),
                Err(LxError::EINVAL),
                "the flag word is judged before the path is walked"
            );

            let new_path = alloc::format!("{dir}/new");
            let new = c(&new_path);
            let free = lp.get_free_fd();
            assert_eq!(
                open(sys, &new, O_CREAT | O_WRONLY | O_TMPFILE),
                Err(LxError::EINVAL),
                "a refused flag word must not reach the creation"
            );
            assert!(
                !exists(&lp, &new_path),
                "the file was created by a call that was refused"
            );
            assert_eq!(
                lp.get_free_fd(),
                free,
                "a refused open spent a descriptor anyway"
            );

            // The same call without that one bit, so what was refused is the
            // bit and not the path.
            assert!(
                open(sys, &new, O_CREAT | O_WRONLY).is_ok(),
                "the same open without the unnamed flag must work"
            );
            assert!(exists(&lp, &new_path));
        })
    });
}

#[test]
fn o_creat_judges_the_name_before_it_makes_anything() {
    // `open_last_lookups`: the root, `.`, `..` and any name with a slash
    // after it are directories, `EISDIR`, whether or not they exist -- and
    // the empty path is `ENOENT` (`getname_flags`), not "this directory".
    // `open("new/", O_CREAT)` used to create `new`.
    let proc = a_process(47_402, "open-creat-name", "open_creat_name");
    let dir = String::from("/open_creat_name");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            assert_eq!(
                open(sys, &c(""), O_CREAT | O_WRONLY),
                Err(LxError::ENOENT),
                "the empty path names nothing, and creates nothing"
            );
            assert_eq!(
                open(sys, &c("/"), O_CREAT | O_WRONLY),
                Err(LxError::EISDIR),
                "the root is a directory"
            );
            assert_eq!(
                open(sys, &c(&alloc::format!("{dir}/.")), O_CREAT | O_WRONLY),
                Err(LxError::EISDIR),
                "`.` is a directory"
            );
            assert_eq!(
                open(sys, &c(&alloc::format!("{dir}/..")), O_CREAT | O_WRONLY),
                Err(LxError::EISDIR),
                "`..` is a directory"
            );

            // A trailing slash is a promise that what is named is a
            // directory, and `open` never makes one.
            let promised_dir = alloc::format!("{dir}/new");
            assert_eq!(
                open(
                    sys,
                    &c(&alloc::format!("{promised_dir}/")),
                    O_CREAT | O_WRONLY
                ),
                Err(LxError::EISDIR),
                "a name with a slash after it can only be a directory"
            );
            assert!(
                !exists(&lp, &promised_dir),
                "`open(\"new/\", O_CREAT)` created `new`"
            );

            // Without `O_CREAT` the empty path is still nothing, which is
            // the half of it that is the walk's rather than this call's.
            assert_eq!(open(sys, &c(""), 0), Err(LxError::ENOENT));
        })
    });
}

#[test]
fn o_excl_over_a_name_that_is_already_there_leaves_it_alone() {
    // `O_EXCL` is the whole of the "create it if nobody else did" idiom, so
    // the `EEXIST` has to arrive before the `O_TRUNC` in the same word gets
    // its turn: a lock file opened `O_CREAT|O_EXCL|O_TRUNC` that comes back
    // EEXIST with the contents gone has already lost what it was protecting.
    let proc = a_process(47_403, "open-excl", "open_excl");
    let dir = String::from("/open_excl");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let path = alloc::format!("{dir}/lock");
            a_file_with(&lp, &dir, "lock", 0, 0o644, b"keep me");
            let name = c(&path);
            let free = lp.get_free_fd();

            assert_eq!(
                open(sys, &name, O_CREAT | O_EXCL | O_WRONLY | O_TRUNC),
                Err(LxError::EEXIST),
                "O_EXCL over a name that exists"
            );
            assert_eq!(
                content_of(&lp, &path, 7),
                Vec::from(&b"keep me"[..]),
                "the refusal came after the truncation"
            );
            assert_eq!(
                lp.get_free_fd(),
                free,
                "a refused open spent a descriptor anyway"
            );

            // The same open without `O_EXCL` does truncate it, so what
            // refused above was that flag and not the write.
            assert!(open(sys, &name, O_CREAT | O_WRONLY | O_TRUNC).is_ok());
            assert!(
                content_of(&lp, &path, 7).is_empty(),
                "O_TRUNC left the contents in place"
            );
        })
    });
}

#[test]
fn the_mode_an_o_creat_open_asks_for_arrives_through_the_umask() {
    // `build_open_flags`: `op->mode = mode & S_IALLUGO`, and then the umask.
    // Without the mask, `open(..., O_CREAT, 0o100644)` -- which is what a
    // program passing `S_IFREG|0644` sends -- left the file-type bits in the
    // inode's mode. Nothing downstream rejects that: the file is simply a
    // file whose mode is a number no `ls` can print.
    let proc = a_process(47_404, "open-creat-mode", "open_creat_mode");
    let dir = String::from("/open_creat_mode");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            assert_eq!(lp.set_umask(0o022), 0o022, "the umask starts at 0o022");

            let with_type_bits = c(&alloc::format!("{dir}/a"));
            assert!(openat(
                sys,
                FileDesc::CWD,
                &with_type_bits,
                O_CREAT | O_WRONLY,
                0o100_644
            )
            .is_ok());
            assert_eq!(
                mode_of(&lp, &alloc::format!("{dir}/a")),
                0o644,
                "S_IFREG in the mode argument reached the inode"
            );
            assert_eq!(
                type_of(&lp, &alloc::format!("{dir}/a")),
                FileType::File,
                "what O_CREAT makes is a regular file"
            );

            // The umask is subtracted from what was asked for, and it is the
            // caller's own rather than a constant.
            lp.set_umask(0o027);
            let narrowed = c(&alloc::format!("{dir}/b"));
            assert!(openat(sys, FileDesc::CWD, &narrowed, O_CREAT | O_WRONLY, 0o666).is_ok());
            assert_eq!(
                mode_of(&lp, &alloc::format!("{dir}/b")),
                0o640,
                "the umask did not reach the mode of the created file"
            );

            lp.set_umask(0);
            let wide = c(&alloc::format!("{dir}/c"));
            assert!(openat(sys, FileDesc::CWD, &wide, O_CREAT | O_WRONLY, 0o666).is_ok());
            assert_eq!(
                mode_of(&lp, &alloc::format!("{dir}/c")),
                0o666,
                "an empty umask takes nothing away"
            );
        })
    });
}

#[test]
fn o_creat_over_a_symbolic_link_opens_what_it_points_at() {
    // `find` hands back the LINK's own inode, so an `O_CREAT` open that
    // takes it writes into the link and not into the file: `>> log` through
    // `/var/log/syslog -> /mnt/log/syslog` appended to the link. Only
    // `O_EXCL` (EEXIST) and `O_NOFOLLOW` (ELOOP) stop the open going
    // through, and neither of them is this.
    let proc = a_process(47_405, "open-creat-link", "open_creat_link");
    let dir = String::from("/open_creat_link");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_with(&lp, &dir, "target", 0, 0o644, b"old");
            a_link_named(&lp, &dir, "l", "target");
            let link = c(&alloc::format!("{dir}/l"));

            let fd = open(sys, &link, O_CREAT | O_WRONLY)
                .expect("O_CREAT over a link opens what the link points at");
            assert_eq!(write_to(sys, fd, b"new").await, Ok(3));
            assert_eq!(
                content_of(&lp, &alloc::format!("{dir}/target"), 3),
                Vec::from(&b"new"[..]),
                "the write went into the link instead of into the file"
            );
            assert_eq!(
                type_of(&lp, &alloc::format!("{dir}/l")),
                FileType::SymLink,
                "the link itself was replaced by a file"
            );

            assert_eq!(
                open(sys, &link, O_CREAT | O_WRONLY | O_NOFOLLOW),
                Err(LxError::ELOOP),
                "O_NOFOLLOW is the flag that refuses a link as the last component"
            );
            assert_eq!(
                open(sys, &link, O_CREAT | O_EXCL | O_WRONLY),
                Err(LxError::EEXIST),
                "O_EXCL sees the name that is there, link or not"
            );
        })
    });
}

#[test]
fn o_creat_wants_write_and_search_on_the_directory_it_creates_in() {
    // `check_access(&dir_metadata, 0o3, true)`: both bits, which is what
    // Linux asks of the parent before `filename_create`. One of the two is
    // not enough, and getting the pair wrong is a directory that either
    // refuses the writes it should take or takes the writes it should
    // refuse.
    let proc = a_process(47_406, "open-creat-dir-perm", "open_creat_dir_perm");
    let dir = String::from("/open_creat_dir_perm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let new_path = alloc::format!("{dir}/new");
            let new = c(&new_path);
            // Asked as somebody who is not root, of a directory that is
            // their own, so what decides is the mode and nothing else.
            owned_with_mode(&lp, &dir, A_USER, 0o555);
            acting_as(sys, A_USER);

            let free = lp.get_free_fd();
            assert_eq!(
                open(sys, &new, O_CREAT | O_WRONLY),
                Err(LxError::EACCES),
                "a directory with no write permission took a creation"
            );
            assert!(!exists(&lp, &new_path), "the file was created anyway");
            assert_eq!(lp.get_free_fd(), free, "a refused open spent a descriptor");

            owned_with_mode(&lp, &dir, A_USER, 0o666);
            assert_eq!(
                open(sys, &new, O_CREAT | O_WRONLY),
                Err(LxError::EACCES),
                "a directory with no search permission took a creation"
            );
            assert!(!exists(&lp, &new_path));

            owned_with_mode(&lp, &dir, A_USER, 0o333);
            assert!(
                open(sys, &new, O_CREAT | O_WRONLY).is_ok(),
                "write and search are the two it wants, and it had both"
            );
            assert!(exists(&lp, &new_path));
        })
    });
}

#[test]
fn o_trunc_empties_what_it_opens_and_leaves_a_file_it_just_created_alone() {
    // Two halves of one flag. On a file that was already there it has to
    // resize to zero -- `> log` from a shell is an `O_WRONLY|O_CREAT|O_TRUNC`
    // open and nothing else, so a dropped `O_TRUNC` is a log that only ever
    // grows. On a file this very call created, `do_open` clears the flag and
    // asks for no write permission, which is what makes `> file` work from a
    // `umask 222` shell.
    let proc = a_process(47_407, "open-trunc", "open_trunc");
    let dir = String::from("/open_trunc");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_with(&lp, &dir, "full", A_USER, 0o644, b"0123456789");
            owned_with_mode(&lp, &dir, A_USER, 0o755);
            acting_as(sys, A_USER);

            let full = alloc::format!("{dir}/full");
            assert!(open(sys, &c(&full), O_WRONLY | O_TRUNC).is_ok());
            assert!(
                content_of(&lp, &full, 10).is_empty(),
                "O_TRUNC opened the file without emptying it"
            );

            // A file created by this very open, with a mode that allows
            // nobody to write it. Asking for write permission here is the
            // `EACCES` a shell's `> file` used to get from its own umask.
            let fresh = alloc::format!("{dir}/fresh");
            assert!(
                openat(
                    sys,
                    FileDesc::CWD,
                    &c(&fresh),
                    O_CREAT | O_WRONLY | O_TRUNC,
                    0o444
                )
                .is_ok(),
                "a file this open created was asked for write permission"
            );
            assert_eq!(mode_of(&lp, &fresh), 0o444);
        })
    });
}

#[test]
fn o_trunc_of_a_file_the_caller_cannot_write_refuses_before_emptying_it() {
    // `may_open` asks for `MAY_WRITE` on the strength of `O_TRUNC` alone,
    // whatever the access mode is, and the answer has to come before
    // `inode.resize(0)`. A refusal that arrives afterwards is the worst of
    // both: the caller is told no and the file is empty.
    let proc = a_process(47_408, "open-trunc-perm", "open_trunc_perm");
    let dir = String::from("/open_trunc_perm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let theirs = alloc::format!("{dir}/theirs");
            a_file_with(&lp, &dir, "theirs", ANOTHER_USER, 0o444, b"not yours");
            acting_as(sys, A_USER);

            let free = lp.get_free_fd();
            assert_eq!(
                open(sys, &c(&theirs), O_TRUNC),
                Err(LxError::EACCES),
                "a read-only O_TRUNC of a file this user cannot write"
            );
            assert_eq!(
                content_of(&lp, &theirs, 9),
                Vec::from(&b"not yours"[..]),
                "the file was emptied by a call that was refused"
            );
            assert_eq!(lp.get_free_fd(), free, "a refused open spent a descriptor");

            // The same file opens read-only, so what was refused is the
            // truncation and not the read.
            assert!(
                open(sys, &c(&theirs), 0).is_ok(),
                "a world-readable file did not open read-only"
            );
        })
    });
}

#[test]
fn o_noatime_is_for_the_owner_or_for_root() {
    // `may_open`: "O_NOATIME can only be set by the owner or superuser", and
    // after the permission checks -- so a file the caller may not read at
    // all answers `EACCES` and not `EPERM`, which is the difference between
    // "you are not allowed in" and "you are in, but not like that".
    let proc = a_process(47_409, "open-noatime", "open_noatime");
    let dir = String::from("/open_noatime");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_with(&lp, &dir, "mine", A_USER, 0o666, b"x");
            a_file_with(&lp, &dir, "theirs", ANOTHER_USER, 0o666, b"x");
            a_file_with(&lp, &dir, "closed", ANOTHER_USER, 0o600, b"x");
            let mine = c(&alloc::format!("{dir}/mine"));
            let theirs = c(&alloc::format!("{dir}/theirs"));
            let closed = c(&alloc::format!("{dir}/closed"));

            // Root owns none of the three and is allowed anyway, which is
            // the "or superuser" half.
            assert!(
                open(sys, &closed, O_NOATIME).is_ok(),
                "root was refused O_NOATIME on a file it does not own"
            );

            acting_as(sys, A_USER);
            assert!(
                open(sys, &mine, O_NOATIME).is_ok(),
                "the owner was refused O_NOATIME on their own file"
            );
            assert_eq!(
                open(sys, &theirs, O_NOATIME),
                Err(LxError::EPERM),
                "O_NOATIME on somebody else's file"
            );
            assert_eq!(
                open(sys, &closed, O_NOATIME),
                Err(LxError::EACCES),
                "the permission check answers before the owner check"
            );

            // And the flag is the only thing being refused: the same file
            // opens without it.
            assert!(open(sys, &theirs, 0).is_ok());
        })
    });
}

#[test]
fn each_kind_of_file_is_refused_for_what_it_is() {
    // The type's own refusals, through the call: `open_resolved_type` is
    // asked three times in `sys_openat` (once in each branch and once after
    // them), and a pure test of it cannot say whether the branch a given
    // open takes asks at all.
    let proc = a_process(47_410, "open-type", "open_type");
    let dir = String::from("/open_type");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_with(&lp, &dir, "f", 0, 0o666, b"x");
            a_link_named(&lp, &dir, "l", "f");
            let here = c(&dir);
            let file = c(&alloc::format!("{dir}/f"));
            let link = c(&alloc::format!("{dir}/l"));

            assert_eq!(
                open(sys, &here, O_RDWR),
                Err(LxError::EISDIR),
                "a directory opened for writing"
            );
            assert_eq!(
                open(sys, &here, O_TRUNC),
                Err(LxError::EISDIR),
                "a read-only O_TRUNC of a directory counts as a write"
            );
            assert!(
                open(sys, &here, 0).is_ok(),
                "a directory opens read-only, which is how `ls` reads one"
            );
            assert_eq!(
                open(sys, &file, O_DIRECTORY),
                Err(LxError::ENOTDIR),
                "a regular file asked for as a directory"
            );
            assert_eq!(
                open(sys, &link, O_NOFOLLOW),
                Err(LxError::ELOOP),
                "a link as the last component of an O_NOFOLLOW open"
            );
            assert_eq!(
                open(sys, &link, O_PATH | O_NOFOLLOW | O_DIRECTORY),
                Err(LxError::ENOTDIR),
                "a link is not a directory whatever it points at"
            );

            // The one pair that is a request rather than a refusal, and the
            // only way to hold a symbolic link by descriptor.
            let held = open(sys, &link, O_PATH | O_NOFOLLOW)
                .expect("O_PATH|O_NOFOLLOW is how a caller takes hold of a link");
            // `O_NOFOLLOW` is a status flag and stays on the description, as
            // Linux reports it; what `path_open_flags` dropped is the access
            // mode and the `O_LARGEFILE`.
            assert_eq!(status_flags(sys, held).await, Ok(O_PATH | O_NOFOLLOW));

            // And the same link opened the ordinary way is the file.
            let through = open(sys, &link, 0).unwrap();
            assert_eq!(
                read_from(sys, through, 1).await,
                Ok(Vec::from(&b"x"[..])),
                "an open through a link reads the file it points at"
            );
        })
    });
}

#[test]
fn the_one_path_this_kernel_rewrites_is_dev_shm_testshm() {
    // `sys_openat` has one hard-coded path: `/dev/shm/testshm` is opened as
    // `/testshm`. It is not a flag or a mount, it is a string comparison in
    // the syscall, so nothing but a call through the syscall sees it -- and
    // a test that opens the name and the file it becomes is the only way to
    // notice it going away.
    let proc = a_process(47_411, "open-testshm", "open_testshm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = open(sys, &c("/dev/shm/testshm"), O_CREAT | O_RDWR)
                .expect("the one path the syscall rewrites");
            assert_eq!(write_to(sys, fd, b"shm").await, Ok(3));
            // What was written is at the rewritten name, in this process's
            // own root -- which is the whole of the rewrite: `/dev/shm` is
            // no directory of this filesystem, so an open that had stopped
            // rewriting would have had nowhere to put it.
            assert_eq!(
                content_of(&lp, "/testshm", 3),
                Vec::from(&b"shm"[..]),
                "the rewritten open did not land on /testshm"
            );
            assert!(
                open(sys, &c("/testshm"), 0).is_ok(),
                "the rewritten name is a file of this process's root"
            );

            // The comparison is of the whole path and not of a prefix, asked
            // without `O_CREAT` so that the question itself makes nothing.
            assert!(
                open(sys, &c("/dev/shm/testshm-and-more"), 0).is_err(),
                "a neighbouring name was rewritten too"
            );
        })
    });
}

#[test]
fn where_a_relative_open_starts_is_the_descriptor_it_was_given() {
    // `sys_open` is `sys_openat_waiting(AT_FDCWD, ...)` and nothing else,
    // so the whole of `open(2)` is that one argument being right. And a
    // relative name under a `dir_fd` has to start at that directory:
    // `openat` is how every `find`, `rsync` and `install` walks a tree
    // without racing a rename above it.
    let proc = a_process(47_412, "open-relative", "open_relative");
    let dir = String::from("/open_relative");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            lp.lookup_inode(&dir)
                .unwrap()
                .create("sub", FileType::Dir, 0o755)
                .unwrap();
            lp.change_directory(&dir);

            // `open(2)`: relative to the working directory.
            let here = c("here");
            assert!(sys
                .sys_open(
                    UserInPtr::from(here.as_ptr() as usize),
                    O_CREAT | O_WRONLY,
                    A_PLAIN_MODE
                )
                .await
                .is_ok());
            assert!(
                exists(&lp, &alloc::format!("{dir}/here")),
                "open(2) did not start at the working directory"
            );

            // `openat(2)`: relative to the descriptor, and not to the
            // working directory the call also has.
            let sub = open(sys, &c(&alloc::format!("{dir}/sub")), O_DIRECTORY).unwrap();
            let there = c("there");
            assert!(openat(
                sys,
                FileDesc::from(sub),
                &there,
                O_CREAT | O_WRONLY,
                A_PLAIN_MODE
            )
            .is_ok());
            assert!(
                exists(&lp, &alloc::format!("{dir}/sub/there")),
                "openat did not start at the directory it was given"
            );
            assert!(
                !exists(&lp, &alloc::format!("{dir}/there")),
                "openat started at the working directory instead"
            );

            // An absolute path pays the descriptor no attention.
            let absolute = c(&alloc::format!("{dir}/abs"));
            assert!(openat(
                sys,
                FileDesc::from(sub),
                &absolute,
                O_CREAT | O_WRONLY,
                A_PLAIN_MODE
            )
            .is_ok());
            assert!(exists(&lp, &alloc::format!("{dir}/abs")));

            // A descriptor that is not a directory, and one that is nothing
            // at all.
            let not_a_dir = open(sys, &c("here"), 0).unwrap();
            assert_eq!(
                openat(
                    sys,
                    FileDesc::from(not_a_dir),
                    &there,
                    O_CREAT | O_WRONLY,
                    A_PLAIN_MODE
                ),
                Err(LxError::ENOTDIR),
                "a regular file was taken for a directory to start from"
            );
            assert_eq!(
                openat(
                    sys,
                    FileDesc::from(A_FD_OF_NOBODY),
                    &there,
                    O_CREAT | O_WRONLY,
                    A_PLAIN_MODE
                ),
                Err(LxError::EBADF),
                "a descriptor nobody opened was taken for a directory"
            );
        })
    });
}

#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    // Through the table, the way userspace calls them: four arguments for
    // `openat` and three for `open`, and a pair swapped anywhere here is a
    // mode read as a flag word.
    let proc = a_process(47_413, "open-by-number", "open_by_number");
    let dir = String::from("/open_by_number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            lp.lookup_inode(&dir)
                .unwrap()
                .create("sub", FileType::Dir, 0o755)
                .unwrap();
            let sub = open(sys, &c(&alloc::format!("{dir}/sub")), O_DIRECTORY).unwrap();

            let name = c("by-number");
            let fd = by_number(
                sys,
                crate::Sys::OPENAT,
                [
                    sub,
                    name.as_ptr() as usize,
                    O_CREAT | O_WRONLY | O_CLOEXEC,
                    0o640,
                    0,
                    0,
                ],
            )
            .await;
            assert!(fd >= 0, "openat by number answered {}", fd);
            assert!(
                exists(&lp, &alloc::format!("{dir}/sub/by-number")),
                "the first argument is the directory to start from"
            );
            assert_eq!(
                mode_of(&lp, &alloc::format!("{dir}/sub/by-number")),
                0o640,
                "the fourth argument is the mode"
            );
            assert_eq!(
                is_cloexec(sys, fd as usize).await,
                Ok(true),
                "the third argument is the flag word"
            );

            // `open(2)` keeps its own number on x86_64 and drops the first
            // argument, so its three are one place to the left.
            #[cfg(target_arch = "x86_64")]
            {
                lp.change_directory(&dir);
                let plain = c("plain");
                let fd = by_number(
                    sys,
                    crate::Sys::OPEN,
                    [plain.as_ptr() as usize, O_CREAT | O_WRONLY, 0o604, 0, 0, 0],
                )
                .await;
                assert!(fd >= 0, "open by number answered {}", fd);
                assert_eq!(
                    mode_of(&lp, &alloc::format!("{dir}/plain")),
                    0o604,
                    "the third argument of open(2) is the mode"
                );
            }
        })
    });
}
