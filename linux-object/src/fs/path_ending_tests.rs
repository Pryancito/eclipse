use super::*;
use crate::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;

fn a_process() -> LinuxProcess {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    proc.root_inode()
        .create("pe_d", FileType::Dir, 0o755)
        .unwrap();
    proc
}

#[test]
fn an_empty_path_is_enoent_from_the_cwd_and_from_a_directory_fd() {
    let proc = a_process();
    assert_eq!(
        proc.lookup_inode_at(FileDesc::CWD, "", true).err(),
        Some(LxError::ENOENT)
    );
    assert_eq!(
        proc.lookup_inode_at(FileDesc::CWD, "", false).err(),
        Some(LxError::ENOENT)
    );
    let dir = proc.lookup_inode_at(FileDesc::CWD, "pe_d", true).unwrap();
    let fd = proc
        .add_file(File::new(dir, OpenFlags::RDONLY, String::from("/pe_d")))
        .unwrap();
    assert_eq!(
        proc.lookup_inode_at(fd, "", true).err(),
        Some(LxError::ENOENT)
    );
}

#[test]
fn a_trailing_slash_wants_a_directory_and_follows_a_link_to_one() {
    // `LOOKUP_DIRECTORY | LOOKUP_FOLLOW`: `stat("pe_f/")` on a regular file
    // is `ENOTDIR`, and `lstat("pe_l/")` on a link to a directory is the
    // directory, not the link.
    let proc = a_process();
    let root = proc.root_inode();
    root.create("pe_f", FileType::File, 0o644).unwrap();
    let to_dir = root.create("pe_l", FileType::SymLink, 0o777).unwrap();
    to_dir.write_at(0, b"pe_d").unwrap();
    let to_file = root.create("pe_m", FileType::SymLink, 0o777).unwrap();
    to_file.write_at(0, b"pe_f").unwrap();
    let d = proc
        .lookup_inode_at(FileDesc::CWD, "pe_d", true)
        .unwrap()
        .metadata()
        .unwrap()
        .inode;

    assert_eq!(
        proc.lookup_inode_at(FileDesc::CWD, "pe_f/", true).err(),
        Some(LxError::ENOTDIR)
    );
    assert_eq!(
        proc.lookup_inode_at(FileDesc::CWD, "pe_f/", false).err(),
        Some(LxError::ENOTDIR)
    );
    let kind = |path: &str, follow: bool| {
        proc.lookup_inode_at(FileDesc::CWD, path, follow)
            .map(|i| i.metadata().unwrap())
            .map(|m| (m.type_, m.inode))
    };
    assert_eq!(kind("pe_d/", true), Ok((FileType::Dir, d)));
    assert_eq!(kind("pe_d/", false), Ok((FileType::Dir, d)));
    // Without the slash `lstat` gets the link; with it, the directory.
    assert_eq!(kind("pe_l", false).map(|(t, _)| t), Ok(FileType::SymLink));
    assert_eq!(kind("pe_l/", false), Ok((FileType::Dir, d)));
    // A link to a file is followed too, and what it reaches is not a
    // directory.
    assert_eq!(kind("pe_m/", false).err(), Some(LxError::ENOTDIR));
    // The root is a directory that is nothing but a slash.
    assert_eq!(kind("/", false).map(|(t, _)| t), Ok(FileType::Dir));
}

#[test]
fn a_dot_still_names_the_directory_itself() {
    // The guard is on the empty string alone: "." is a real component.
    let proc = a_process();
    let root = proc.root_inode().metadata().unwrap().inode;
    assert_eq!(
        proc.lookup_inode_at(FileDesc::CWD, ".", true)
            .unwrap()
            .metadata()
            .unwrap()
            .inode,
        root
    );
}
