//! `/dev/pts` as Linux 6.18 has it (measured there with glibc and musl, as
//! root and as `nobody`): a slave is listed there by its number from the open
//! of `/dev/ptmx` until its master is closed, `stat` of its name is the inode
//! a descriptor of it is, any name of it opens it, it belongs to whoever
//! opened the master, a `chmod` of either is a `chmod` of both, and
//! `/dev/pts` is a devpts of its own; and anybody may open `/dev/ptmx` by any
//! name of it. `/dev/pts` was an empty directory and only the exact string
//! `/dev/pts/N` opened a slave, so `ttyname` of a pty failed and anybody could
//! open anybody's terminal.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::convert::TryInto;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_NOCTTY: usize = 0o400;
const O_DIRECTORY: usize = 0o200_000;
/// `TIOCGPTN`, which `ptsname` asks the master.
const TIOCGPTN: usize = 0x8004_5430;
const DEVPTS_SUPER_MAGIC: i64 = 0x1cd1;
const DT_CHR: u8 = 2;
const S_IFCHR: u32 = 0o020_000;
const A_USER: u32 = 1000;
const A_STRANGER: u32 = 1001;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

fn c(s: &str) -> String {
    format!("{}\0", s)
}

fn at(s: &str) -> UserInPtr<u8> {
    UserInPtr::from(s.as_ptr() as usize)
}

fn fd(n: usize) -> FileDesc {
    (n as i32).into()
}

fn blank() -> Stat {
    use linux_object::fs::vfs::{FileType, Timespec};
    let zero = Timespec { sec: 0, nsec: 0 };
    Metadata {
        dev: 0,
        inode: 0,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: zero,
        mtime: zero,
        ctime: zero,
        type_: FileType::File,
        mode: 0,
        nlinks: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
    }
    .into()
}

fn stat(sys: &Syscall<'_>, path: &str) -> LxResult<Stat> {
    let path = c(path);
    let mut st = blank();
    sys.sys_stat(at(&path), UserOutPtr::from(&mut st as *mut Stat as usize))?;
    Ok(st)
}

fn fstat(sys: &Syscall<'_>, fd: FileDesc) -> LxResult<Stat> {
    let mut st = blank();
    sys.sys_fstat(fd, UserOutPtr::from(&mut st as *mut Stat as usize))?;
    Ok(st)
}

fn open_at(sys: &Syscall<'_>, dir: FileDesc, path: &str, flags: usize) -> LxResult<FileDesc> {
    let path = c(path);
    sys.sys_openat(dir, at(&path), flags, 0).map(fd)
}

fn open(sys: &Syscall<'_>, path: &str) -> LxResult<FileDesc> {
    open_at(sys, FileDesc::CWD, path, O_RDWR | O_NOCTTY)
}

/// A new pair, as `posix_openpt` and `ptsname` make it: the master, and the
/// name of its slave.
async fn a_pair(sys: &Syscall<'_>) -> (FileDesc, String) {
    let master = open(sys, "/dev/ptmx").unwrap();
    let mut n = 0u32;
    sys.sys_ioctl(master, TIOCGPTN, &mut n as *mut u32 as usize, 0, 0)
        .await
        .unwrap();
    (master, format!("/dev/pts/{}", n))
}

fn number(name: &str) -> u64 {
    name.trim_start_matches("/dev/pts/").parse().unwrap()
}

/// `(d_ino, d_type, d_name)` of every entry `getdents64` gives of `dir`.
fn entries(sys: &Syscall<'_>, dir: &str) -> Vec<(u64, u8, String)> {
    let dir = open_at(sys, FileDesc::CWD, dir, O_RDONLY | O_DIRECTORY).unwrap();
    let mut buf = alloc::vec![0u8; 4096];
    let mut all = Vec::new();
    loop {
        let n = sys
            .sys_getdents64(dir, UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len())
            .unwrap();
        if n == 0 {
            break;
        }
        let mut at = 0;
        while at < n {
            let ino = u64::from_ne_bytes(buf[at..at + 8].try_into().unwrap());
            let reclen = u16::from_ne_bytes(buf[at + 16..at + 18].try_into().unwrap()) as usize;
            let type_ = buf[at + 18];
            let name = &buf[at + 19..at + reclen];
            let name = &name[..name.iter().position(|&b| b == 0).unwrap()];
            all.push((ino, type_, String::from_utf8(name.to_vec()).unwrap()));
            at += reclen;
        }
    }
    sys.sys_close(dir).unwrap();
    all
}

/// `mode`, `uid` and `gid` of `st`, the mode without its type.
fn owner(st: &Stat) -> (u32, u32, u32) {
    (st.mode.bits() & 0o7777, st.uid, st.gid)
}

#[test]
fn a_new_pair_is_listed_in_dev_pts_by_its_number() {
    let proc = a_process(45_660, "pts_ls");
    let (name, listed) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_master, name) = a_pair(sys).await;
            let listed = entries(sys, "/dev/pts");
            (name, listed)
        })
    });
    let n = number(&name);
    let names: Vec<&str> = listed.iter().map(|(_, _, name)| name.as_str()).collect();
    assert!(names.contains(&"."), "{:?}", names);
    assert!(names.contains(&".."), "{:?}", names);
    let (ino, type_, _) = listed
        .iter()
        .find(|(_, _, it)| *it == n.to_string())
        .unwrap_or_else(|| panic!("{} is not among {:?}", name, names));
    // `glibc`'s `ttyname` matches the inode before it stats the name.
    assert_eq!((*ino, *type_), (n + 3, DT_CHR), "{}", name);
}

#[test]
fn stat_of_dev_pts_n_is_the_slave_a_descriptor_of_it_is() {
    let proc = a_process(45_661, "pts_stat");
    let (name, named, opened, pts, dev) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_master, name) = a_pair(sys).await;
            let slave = open(sys, &name).unwrap();
            (
                name.clone(),
                stat(sys, &name).unwrap(),
                fstat(sys, slave).unwrap(),
                stat(sys, "/dev/pts").unwrap(),
                stat(sys, "/dev").unwrap(),
            )
        })
    });
    let n = number(&name);
    assert_eq!(
        named.mode.bits() & !0o7777,
        S_IFCHR,
        "{} is no character device",
        name
    );
    for (what, got, want) in [
        ("st_dev", named.dev, opened.dev),
        ("st_ino", named.ino, opened.ino),
        ("st_rdev", named.rdev, opened.rdev),
        (
            "st_mode",
            named.mode.bits() as u64,
            opened.mode.bits() as u64,
        ),
    ] {
        assert_eq!(got, want, "{} of {} and of the slave", what, name);
    }
    assert_eq!(named.ino, n + 3, "devpts numbers a slave N + 3");
    // `MKDEV(UNIX98_PTY_SLAVE_MAJOR, index)`, packed as glibc's `makedev`.
    let rdev = (136 << 8) | (n & 0xff) | ((n & !0xff) << 12);
    assert_eq!(named.rdev, rdev, "the slave is device 136:{}", n);
    assert_eq!(named.dev, pts.dev, "the slave is not on /dev/pts's device");
    assert_ne!(pts.dev, dev.dev, "/dev/pts is on /dev's device");
}

#[test]
fn the_slave_opens_by_any_name_of_it_and_is_named_by_its_path() {
    let proc = a_process(45_662, "pts_open");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (master, name) = a_pair(sys).await;
            let n = name.trim_start_matches("/dev/pts/");
            let pts = open_at(sys, FileDesc::CWD, "/dev/pts", O_RDONLY | O_DIRECTORY).unwrap();
            let by_dirfd = open_at(sys, pts, n, O_RDWR | O_NOCTTY);
            let dotted = open(sys, &format!("/dev/pts/../pts/./{}", n));
            let cwd = c("/dev/pts");
            sys.sys_chdir(at(&cwd)).unwrap();
            let relative = open(sys, n);
            let mut each = Vec::new();
            for (how, slave) in [
                ("by dirfd", by_dirfd),
                ("through ..", dotted),
                ("from /dev/pts", relative),
            ] {
                let slave = slave.unwrap_or_else(|e| panic!("{}: {:?}", how, e));
                // What goes in at the master comes out of this slave.
                let line = format!("{}\n", how);
                sys.sys_write(master, at(&line), line.len()).await.unwrap();
                let mut buf = [0u8; 64];
                let len = sys
                    .sys_read(
                        slave,
                        UserOutPtr::from(buf.as_mut_ptr() as usize),
                        buf.len(),
                    )
                    .await
                    .unwrap();
                let link = super::pipe_tests::readlink(sys, &super::pipe_tests::fd_link(slave));
                each.push((
                    how,
                    line,
                    String::from_utf8(buf[..len].to_vec()).unwrap(),
                    link,
                ));
            }
            (name, each)
        })
    });
    let (name, each) = got;
    for (how, sent, read, link) in each {
        assert_eq!(read, sent, "the slave opened {} is not the pair's", how);
        assert_eq!(
            link,
            Ok(name.clone()),
            "/proc/self/fd of the slave opened {}",
            how
        );
    }
}

#[test]
fn dev_pts_n_is_gone_once_its_master_is_closed() {
    let proc = a_process(45_663, "pts_gone");
    let (name, before, after, listed, reopened, still) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (master, name) = a_pair(sys).await;
            let slave = open(sys, &name).unwrap();
            let before = stat(sys, &name).map(|_| ());
            sys.sys_close(master).unwrap();
            (
                name.clone(),
                before,
                stat(sys, &name).map(|_| ()),
                entries(sys, "/dev/pts"),
                open(sys, &name).map(|_| ()),
                fstat(sys, slave).map(|_| ()),
            )
        })
    });
    assert_eq!(before, Ok(()), "{} before its master was closed", name);
    assert_eq!(
        after,
        Err(LxError::ENOENT),
        "stat of {} with its master closed",
        name
    );
    assert_eq!(
        reopened,
        Err(LxError::ENOENT),
        "open of {} with its master closed",
        name
    );
    let n = number(&name).to_string();
    assert!(
        listed.iter().all(|(_, _, it)| *it != n),
        "{} is still listed: {:?}",
        name,
        listed
    );
    assert_eq!(still, Ok(()), "the open slave is still a descriptor");
}

#[test]
fn a_slave_reached_through_its_name_once_its_master_is_closed_is_eio() {
    const O_PATH: usize = 0o10_000_000;
    let proc = a_process(45_672, "pts_dead");
    let (alive, dead, as_path) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (master, name) = a_pair(sys).await;
            let held = open_at(sys, FileDesc::CWD, &name, O_PATH).unwrap();
            let link = super::pipe_tests::fd_link(held);
            let alive = open(sys, &link).map(|_| ());
            let _slave = open(sys, &name).unwrap();
            sys.sys_close(master).unwrap();
            (
                alive,
                open(sys, &link).map(|_| ()),
                open_at(sys, FileDesc::CWD, &link, O_PATH).map(|_| ()),
            )
        })
    });
    assert_eq!(alive, Ok(()), "the name held while its master is open");
    assert_eq!(
        dead,
        Err(LxError::EIO),
        "the name held once its master is closed"
    );
    assert_eq!(as_path, Ok(()), "an O_PATH open of it opens nothing");
}

#[test]
fn a_name_that_is_no_live_pair_is_enoent() {
    let proc = a_process(45_664, "pts_none");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            ["/dev/pts/4294967296", "/dev/pts/987654", "/dev/pts/x"]
                .map(|name| (name, stat(sys, name).map(|_| ())))
        })
    });
    for (name, got) in got {
        assert_eq!(got, Err(LxError::ENOENT), "{}", name);
    }
}

#[test]
fn the_slave_belongs_to_whoever_opened_the_master_and_to_the_tty_group() {
    let proc = a_process(45_665, "pts_own");
    proc.linux().set_fsuid(A_USER);
    let (named, opened) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_master, name) = a_pair(sys).await;
            let slave = open(sys, &name).unwrap();
            (stat(sys, &name).unwrap(), fstat(sys, slave).unwrap())
        })
    });
    // `gid=5,mode=620`, as `/dev/pts` is mounted everywhere it is used.
    assert_eq!(owner(&named), (0o620, A_USER, 5));
    assert_eq!(owner(&opened), (0o620, A_USER, 5));
}

#[test]
fn nobody_else_may_open_somebody_s_terminal() {
    let mine = a_process(45_666, "pts_mine");
    let theirs = a_process(45_667, "pts_theirs");
    let root = a_process(45_668, "pts_root");
    let (name, by_me) = on_a_thread(&mine, |sys| {
        Box::pin(async move {
            sys.linux_process()
                .set_resuid(A_USER, A_USER, A_USER)
                .unwrap();
            let (_master, name) = a_pair(sys).await;
            let by_me = open(sys, &name).map(|_| ());
            (name, by_me)
        })
    });
    let them = name.clone();
    let by_them = on_a_thread(&theirs, move |sys| {
        Box::pin(async move {
            sys.linux_process()
                .set_resuid(A_STRANGER, A_STRANGER, A_STRANGER)
                .unwrap();
            open(sys, &them).map(|_| ())
        })
    });
    let as_root = name.clone();
    let by_root = on_a_thread(&root, move |sys| {
        Box::pin(async move { open(sys, &as_root).map(|_| ()) })
    });
    assert_eq!(by_me, Ok(()), "its owner opening {}", name);
    assert_eq!(by_them, Err(LxError::EACCES), "a stranger opening {}", name);
    assert_eq!(by_root, Ok(()), "root opening {}", name);
}

#[test]
fn a_chmod_or_a_chown_of_the_name_and_of_the_slave_are_one() {
    let proc = a_process(45_669, "pts_chmod");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_master, name) = a_pair(sys).await;
            let slave = open(sys, &name).unwrap();
            let path = c(&name);
            sys.sys_fchmod(slave, 0o600).unwrap();
            let after_fchmod = stat(sys, &name).unwrap();
            sys.sys_fchmodat(FileDesc::CWD, at(&path), 0o620, 0)
                .unwrap();
            let after_chmod = fstat(sys, slave).unwrap();
            sys.sys_fchownat(FileDesc::CWD, at(&path), A_USER as usize, 100, 0)
                .unwrap();
            let after_chown = fstat(sys, slave).unwrap();
            (after_fchmod, after_chmod, after_chown)
        })
    });
    let (after_fchmod, after_chmod, after_chown) = got;
    assert_eq!(
        owner(&after_fchmod).0,
        0o600,
        "stat of the name after fchmod of the slave"
    );
    assert_eq!(
        owner(&after_chmod).0,
        0o620,
        "fstat of the slave after chmod of the name"
    );
    assert_eq!(
        owner(&after_chown),
        (0o620, A_USER, 100),
        "after chown of the name"
    );
}

#[test]
fn dev_pts_is_a_devpts_and_says_so_in_mountinfo() {
    let proc = a_process(45_670, "pts_fs");
    let (of_dir, of_slave, pts, lines) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_master, name) = a_pair(sys).await;
            let slave = open(sys, &name).unwrap();
            let dir = c("/dev/pts");
            let mut of_dir = [0i64; 15];
            sys.sys_statfs(at(&dir), UserOutPtr::from(of_dir.as_mut_ptr() as usize))
                .unwrap();
            let mut of_slave = [0i64; 15];
            sys.sys_fstatfs(slave, UserOutPtr::from(of_slave.as_mut_ptr() as usize))
                .unwrap();
            let info = sys
                .linux_process()
                .lookup_inode("/proc/self/mountinfo")
                .unwrap();
            let mut buf = alloc::vec![0u8; 1 << 16];
            let len = info.read_at(0, &mut buf).unwrap();
            let lines: Vec<String> = String::from_utf8_lossy(&buf[..len])
                .lines()
                .filter(|line| line.split(' ').nth(4) == Some("/dev/pts"))
                .map(String::from)
                .collect();
            (
                of_dir[0],
                of_slave[0],
                stat(sys, "/dev/pts").unwrap(),
                lines,
            )
        })
    });
    assert_eq!(of_dir, DEVPTS_SUPER_MAGIC, "f_type of /dev/pts");
    assert_eq!(of_slave, DEVPTS_SUPER_MAGIC, "f_type of a slave");
    assert_eq!(lines.len(), 1, "{:?}", lines);
    let fields: Vec<&str> = lines[0].split(' ').collect();
    let (major, minor) = (
        ((pts.dev >> 8) & 0xfff) | ((pts.dev >> 32) & !0xfff),
        (pts.dev & 0xff) | ((pts.dev >> 12) & !0xff),
    );
    assert_eq!(fields[2], format!("{}:{}", major, minor), "{}", lines[0]);
    assert!(lines[0].contains(" - devpts devpts "), "{}", lines[0]);
}

#[test]
fn anybody_may_open_ptmx_by_any_name_of_it_and_owns_the_pair() {
    let proc = a_process(45_671, "pts_ptmx");
    let (ptmx, master, slave) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            sys.linux_process()
                .set_resuid(A_USER, A_USER, A_USER)
                .unwrap();
            let ptmx = stat(sys, "/dev/ptmx").unwrap();
            let dev = c("/dev");
            sys.sys_chdir(at(&dev)).unwrap();
            let master = open(sys, "ptmx").unwrap();
            let mut n = 0u32;
            sys.sys_ioctl(master, TIOCGPTN, &mut n as *mut u32 as usize, 0, 0)
                .await
                .unwrap();
            (
                ptmx,
                fstat(sys, master).unwrap(),
                stat(sys, &format!("/dev/pts/{}", n)).unwrap(),
            )
        })
    });
    // `crw-rw-rw-` of root's, as devtmpfs makes it with no udev to regroup it.
    assert_eq!(owner(&ptmx), (0o666, 0, 0), "/dev/ptmx");
    assert_eq!(ptmx.mode.bits() & !0o7777, S_IFCHR, "/dev/ptmx");
    assert_eq!(ptmx.rdev, 5 << 8 | 2, "/dev/ptmx is device 5:2");
    for (what, got, want) in [
        ("st_dev", master.dev, ptmx.dev),
        ("st_rdev", master.rdev, ptmx.rdev),
        (
            "st_mode",
            master.mode.bits() as u64,
            ptmx.mode.bits() as u64,
        ),
    ] {
        assert_eq!(got, want, "{} of the master and of /dev/ptmx", what);
    }
    assert_eq!(owner(&slave), (0o620, A_USER, 5), "the slave of the pair");
}
