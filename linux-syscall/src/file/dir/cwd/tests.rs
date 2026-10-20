//! A working directory reached through a symbolic link is where the link
//! leads, as on Linux.
//!
//! Every test has a top directory of its own: a lookup from the root goes
//! through a path cache that every process in this binary shares.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::fs::{File, OpenFlags};
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// A process whose root holds, under `/<top>`: `real/` with `real/f` and
/// `real/sub/`, and `link` -> `real`, `deep` -> `/<top>/real/sub`.
fn a_process(top: &str) -> LinuxProcess {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let link = |dir: &Arc<dyn INode>, name: &str, to: &str| {
        let l = dir.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    let top_dir = proc.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    let real = top_dir.create("real", FileType::Dir, 0o755).unwrap();
    real.create("f", FileType::File, 0o644).unwrap();
    real.create("sub", FileType::Dir, 0o755).unwrap();
    link(&top_dir, "link", "real");
    link(&top_dir, "deep", &format!("/{}/real/sub", top));
    proc
}

#[test]
fn a_directory_reached_through_a_link_is_where_the_link_leads() {
    let proc = a_process("cwd_a");
    assert_eq!(physical_dir(&proc, "/cwd_a/link"), "/cwd_a/real");
    assert_eq!(physical_dir(&proc, "/cwd_a/deep/.."), "/cwd_a/real");
    proc.change_directory("/cwd_a");
    assert_eq!(physical_dir(&proc, "link/sub"), "/cwd_a/real/sub");
    assert_eq!(
        physical_dir(&proc, "deep/../.."),
        "/cwd_a",
        "`..` goes up from where the link led"
    );
}

#[test]
fn a_relative_lookup_works_in_a_directory_reached_through_a_link() {
    let proc = a_process("cwd_b");
    proc.change_directory(&physical_dir(&proc, "/cwd_b/link"));
    assert_eq!(proc.current_working_directory(), "/cwd_b/real");
    assert!(proc.lookup_inode("f").is_ok());
    assert!(proc.lookup_inode("sub").is_ok());
}

#[test]
fn chdir_through_a_link_leaves_the_process_where_it_leads() {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, 45_411, "cwd", a_process("cwd_c")).unwrap();
    let (cwd, f) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            sys.sys_chdir(UserInPtr::from(b"/cwd_c/link\0".as_ptr() as usize))
                .unwrap();
            let mut buf = [0u8; 64];
            let len = sys
                .sys_getcwd(UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len())
                .unwrap();
            let cwd = String::from_utf8(buf[..len - 1].to_vec()).unwrap();
            (cwd, sys.linux_process().lookup_inode("f").is_ok())
        })
    });
    assert_eq!(cwd, "/cwd_c/real");
    assert!(f, "a relative lookup from there");
}

#[test]
fn fchdir_to_a_directory_opened_through_a_link_and_getcwd_once_it_has_gone() {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, 45_412, "cwd", a_process("cwd_d")).unwrap();
    let (through, gone) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let proc = sys.linux_process();
            let deep = proc.lookup_inode("/cwd_d/deep").unwrap();
            let fd = proc
                .add_file(File::new(
                    deep,
                    OpenFlags::RDONLY,
                    String::from("/cwd_d/deep"),
                ))
                .unwrap();
            sys.sys_fchdir(fd).unwrap();
            let through = proc.current_working_directory();
            proc.lookup_inode("/cwd_d/real")
                .unwrap()
                .unlink("sub")
                .unwrap();
            linux_object::fs::dcache_invalidate();
            let mut buf = [0u8; 64];
            let gone = sys.sys_getcwd(UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len());
            (through, gone)
        })
    });
    assert_eq!(through, "/cwd_d/real/sub");
    assert_eq!(
        gone,
        Err(LxError::ENOENT),
        "the working directory was removed"
    );
}
