//! Every file watcher reads `/proc/sys/fs/inotify/max_user_watches`
//! before it decides how to watch. It was not there, so chromium logged
//! "Failed to read /proc/sys/fs/inotify/max_user_watches" once per
//! process.

use super::*;
use alloc::string::ToString;

fn read_all(inode: &Arc<dyn INode>) -> String {
    let mut buf = [0u8; 64];
    let n = inode.read_at(0, &mut buf).unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

#[test]
fn sys_fs_lists_and_resolves_inotify() {
    let fs = PROC_SYS_DIR.find("fs").unwrap();
    let mut listed = Vec::new();
    let mut id = 0;
    while let Ok(name) = fs.get_entry(id) {
        listed.push(name);
        id += 1;
    }
    assert!(listed.contains(&"inotify".to_string()), "{:?}", listed);
    let dir = fs.find("inotify").unwrap();
    assert_eq!(dir.metadata().unwrap().type_, FileType::Dir);
    // A directory that holds one subdirectory more than `fs` reported
    // stops `find`/`du` descending into it (`2 + subdirs`).
    assert_eq!(fs.metadata().unwrap().nlinks, 3);
}

#[test]
fn the_three_limits_are_there_and_parse_as_numbers() {
    let dir = PROC_SYS_DIR.find("fs").unwrap().find("inotify").unwrap();
    let mut listed = Vec::new();
    let mut id = 0;
    while let Ok(name) = dir.get_entry(id) {
        listed.push(name);
        id += 1;
    }
    assert_eq!(
        listed,
        alloc::vec![
            ".",
            "..",
            "max_user_watches",
            "max_user_instances",
            "max_queued_events"
        ]
    );
    for name in [
        "max_user_watches",
        "max_user_instances",
        "max_queued_events",
    ] {
        let text = read_all(&dir.find(name).unwrap());
        // One number and a newline is the whole sysctl format; a reader
        // that `atoi`s it must not find anything else.
        assert!(text.ends_with('\n'), "{}: {:?}", name, text);
        let value: u64 = text.trim_end().parse().unwrap();
        assert!(value > 0, "{} vale {}", name, value);
    }
    assert_eq!(read_all(&dir.find("max_user_watches").unwrap()), "65536\n");
    // And `..` climbs back to /proc/sys/fs.
    assert_eq!(
        dir.find("..").unwrap().metadata().unwrap().inode,
        PROC_SYS_FS_DIR.metadata().unwrap().inode
    );
}
