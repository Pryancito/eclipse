//! A working directory that has been removed is reported as gone.
//!
//! Every test has a top directory of its own: a lookup from the root goes
//! through a path cache that every process in this binary shares.

use super::*;
use rcore_fs_ramfs::RamFS;

fn a_process_in(top: &str) -> LinuxProcess {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let top_dir = proc.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    top_dir.create("d", FileType::Dir, 0o755).unwrap();
    top_dir.create("f", FileType::File, 0o644).unwrap();
    proc.change_directory(&format!("/{}/d", top));
    proc
}

#[test]
fn a_working_directory_that_is_there_is_its_own_link() {
    let proc = a_process_in("wdr_a");
    assert!(!proc.working_directory_removed());
    assert_eq!(proc.working_directory_link(), "/wdr_a/d");
    proc.change_directory("/");
    assert!(!proc.working_directory_removed(), "the root");
    assert_eq!(proc.working_directory_link(), "/");
}

#[test]
fn a_removed_working_directory_is_gone_and_its_link_says_deleted() {
    let proc = a_process_in("wdr_b");
    proc.root_inode()
        .lookup("wdr_b")
        .unwrap()
        .unlink("d")
        .unwrap();
    crate::fs::dcache_invalidate();
    assert!(proc.working_directory_removed());
    assert_eq!(proc.working_directory_link(), "/wdr_b/d (deleted)");
}

#[test]
fn a_working_directory_whose_name_is_now_a_file_is_gone() {
    let proc = a_process_in("wdr_c");
    let top = proc.root_inode().lookup("wdr_c").unwrap();
    top.unlink("d").unwrap();
    top.create("d", FileType::File, 0o644).unwrap();
    crate::fs::dcache_invalidate();
    assert!(proc.working_directory_removed());
}
