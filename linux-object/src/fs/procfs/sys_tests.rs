//! `/proc/sys`, `/proc/sysvipc` and the fixed files of `/proc`, walked, and
//! each value read against the one Linux 6.x gives with its defaults.

use super::*;

fn root() -> Arc<dyn INode> {
    PROC_ROOT.clone()
}

fn at(path: &str) -> Arc<dyn INode> {
    root()
        .lookup(path)
        .unwrap_or_else(|e| panic!("/proc/{}: {:?}", path, e))
}

fn read_all(node: &Arc<dyn INode>) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = node.read_at(out.len(), &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        assert!(out.len() < 1 << 20);
    }
    String::from_utf8(out).unwrap()
}

fn read(path: &str) -> String {
    read_all(&at(path))
}

/// What `readdir` gives: `get_entry` from 0 until it ends.
fn listing(node: &Arc<dyn INode>) -> Vec<String> {
    let mut names = Vec::new();
    while let Ok(name) = node.get_entry(names.len()) {
        names.push(name);
        assert!(names.len() < 4096);
    }
    names
}

/// The directories under `/proc/sys` and `/proc/sysvipc`, and every file in
/// them, with the path each was found at.
fn sys_tree() -> Vec<(String, Arc<dyn INode>)> {
    let mut out = Vec::new();
    let mut queue = vec![
        (String::from("sys"), at("sys")),
        (String::from("sysvipc"), at("sysvipc")),
    ];
    while let Some((path, node)) = queue.pop() {
        if node.metadata().unwrap().type_ == FileType::Dir {
            for name in listing(&node) {
                if name == "." || name == ".." {
                    continue;
                }
                let child = node
                    .find(&name)
                    .unwrap_or_else(|e| panic!("{}/{} listed but {:?}", path, name, e));
                queue.push((format!("{}/{}", path, name), child));
            }
        }
        out.push((path, node));
    }
    out
}

/// Every directory lists `.` and `..` first, each name it lists can be
/// opened, `.` is itself, and `..` is the one above it.
#[test]
fn every_directory_of_proc_sys_lists_itself_its_parent_and_what_is_in_it() {
    let tree = sys_tree();
    assert!(tree.len() > 25, "{} nodes", tree.len());
    for (path, node) in &tree {
        let md = node.metadata().unwrap();
        if md.type_ != FileType::Dir {
            continue;
        }
        let names = listing(node);
        assert_eq!(&names[..2], [".", ".."], "{}", path);
        assert_eq!(
            node.find(".").unwrap().metadata().unwrap().inode,
            md.inode,
            "{}",
            path
        );
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        let above = if parent.is_empty() {
            root()
        } else {
            at(parent)
        };
        assert_eq!(
            node.find("..").unwrap().metadata().unwrap().inode,
            above.metadata().unwrap().inode,
            "{}/..",
            path
        );
        assert!(md.nlinks >= 2, "{}", path);
        assert!(node.write_at(0, b"x").is_err(), "{}", path);
        assert!(node.poll().unwrap().read, "{}", path);
        assert_eq!(
            node.find("nope").err(),
            Some(FsError::EntryNotFound),
            "{}",
            path
        );
    }
}

/// Every node is of procfs (`PROC_SUPER_MAGIC`, what systemd's
/// `proc_mounted()` asks), and no two of them have one inode.
#[test]
fn every_node_is_of_procfs_and_has_an_inode_of_its_own() {
    let mut seen = alloc::collections::BTreeMap::new();
    for (path, node) in sys_tree() {
        assert_eq!(
            node.fs().info().ftype,
            rcore_fs::vfs::PROC_SUPER_MAGIC,
            "{}",
            path
        );
        let md = node.metadata().unwrap();
        assert_eq!((md.uid, md.gid), (0, 0), "{}", path);
        if let Some(other) = seen.insert(md.inode, path.clone()) {
            panic!("{} and {} are both inode {}", other, path, md.inode);
        }
    }
    assert_eq!(ProcFS::new().info().namemax, 255);
    assert!(ProcFS.sync().is_ok());
}

/// Every value of `/proc/sys` is one line.
#[test]
fn every_value_of_proc_sys_is_a_line() {
    for (path, node) in sys_tree() {
        if node.metadata().unwrap().type_ != FileType::File || path.starts_with("sysvipc") {
            continue;
        }
        let text = read_all(&node);
        assert!(text.ends_with('\n'), "{} = {:?}", path, text);
        assert_eq!(text.matches('\n').count(), 1, "{} = {:?}", path, text);
    }
}

/// The values Linux has, with its defaults: what `sysconf`, glibc, bwrap,
/// JVMs and container runtimes read and size things from.
#[test]
fn the_values_linux_gives_by_default() {
    for (path, want) in [
        ("sys/kernel/pid_max", "4194304\n"),
        ("sys/kernel/ngroups_max", "65536\n"),
        ("sys/kernel/overflowuid", "65534\n"),
        ("sys/kernel/overflowgid", "65534\n"),
        ("sys/kernel/kptr_restrict", "0\n"),
        ("sys/vm/overcommit_memory", "0\n"),
        ("sys/vm/max_map_count", "65530\n"),
        ("sys/vm/drop_caches", "0\n"),
        ("sys/fs/nr_open", "1048576\n"),
        ("sys/fs/pipe-max-size", "1048576\n"),
    ] {
        assert_eq!(read(path), want, "/proc/{}", path);
    }
    // What `uname -s`, `-r` and `-v` say (the names are this kernel's own).
    assert_eq!(
        read("sys/kernel/ostype"),
        format!("{}\n", crate::uname::OS_TYPE)
    );
    assert_eq!(
        read("sys/kernel/osrelease"),
        format!("{}\n", crate::uname::OS_RELEASE)
    );
    assert_eq!(
        read("sys/kernel/version"),
        format!("{}\n", crate::uname::os_version())
    );
    let threads: u64 = read("sys/kernel/threads-max").trim().parse().unwrap();
    assert!(threads > 0);
    for f in [
        "max_user_watches",
        "max_user_instances",
        "max_queued_events",
    ] {
        let v: u64 = read(&format!("sys/fs/inotify/{}", f))
            .trim()
            .parse()
            .unwrap();
        assert!(v > 0, "{}", f);
    }
    let paranoid: i32 = read("sys/kernel/perf_event_paranoid")
        .trim()
        .parse()
        .unwrap();
    assert!((-1..=4).contains(&paranoid));
}

fn is_uuid_v4(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or("");
    let groups: Vec<&str> = s.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && s.chars()
            .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        && groups[2].starts_with('4')
        && matches!(groups[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b')
}

/// `random/boot_id` is a version-4 UUID that is the same for the whole
/// boot; `random/uuid` is a new one each read.
#[test]
fn boot_id_stays_and_uuid_is_new_each_read() {
    let boot = read("sys/kernel/random/boot_id");
    assert!(is_uuid_v4(&boot), "{:?}", boot);
    assert_eq!(read("sys/kernel/random/boot_id"), boot);
    let (a, b) = (
        read("sys/kernel/random/uuid"),
        read("sys/kernel/random/uuid"),
    );
    assert!(is_uuid_v4(&a) && is_uuid_v4(&b), "{:?} {:?}", a, b);
    assert_ne!(a, b);
    assert!(!is_uuid_v4("0123456-89ab-4def-8123-456789abcdef\n"));
    assert!(
        !is_uuid_v4("01234567-89ab-1def-8123-456789abcdef\n"),
        "version 1"
    );
}

/// `echo name > /proc/sys/kernel/hostname` is `sethostname`: the name is
/// what `uname -n` says, the newline is not part of it, and a name longer
/// than `HOST_NAME_MAX` (64) is `EINVAL` and changes nothing.
#[test]
fn the_hostname_is_written_as_sethostname_writes_it() {
    let was = crate::uname::hostname();
    let node = at("sys/kernel/hostname");
    assert_eq!(node.metadata().unwrap().mode & 0o777, 0o644);
    assert_eq!(node.write_at(0, b"eclipse-test\n"), Ok(13));
    assert_eq!(read("sys/kernel/hostname"), "eclipse-test\n");
    assert_eq!(crate::uname::hostname(), "eclipse-test");
    let sixty_four = "h".repeat(64);
    assert_eq!(node.write_at(0, sixty_four.as_bytes()), Ok(64), "64 fits");
    assert!(node.write_at(0, "h".repeat(65).as_bytes()).is_err());
    assert_eq!(crate::uname::hostname(), sixty_four);
    assert!(node.write_at(0, &[0xff]).is_err(), "not text");
    assert!(node.resize(0).is_ok(), "O_TRUNC is taken and ignored");
    crate::uname::set_hostname(&was);

    let was = crate::uname::domainname();
    let dom = at("sys/kernel/domainname");
    assert_eq!(dom.write_at(0, b"lan\n"), Ok(4));
    assert_eq!(read("sys/kernel/domainname"), "lan\n");
    crate::uname::set_domainname(&was);
}

/// `drop_caches` takes 1, 2 or 3 and reads back 0; anything else is
/// `EINVAL`. A value that is only read (`ostype`) refuses a write.
#[test]
fn drop_caches_takes_one_two_or_three() {
    let node = at("sys/vm/drop_caches");
    for v in ["1\n", "2", "3\n"] {
        assert_eq!(node.write_at(0, v.as_bytes()), Ok(v.len()), "{:?}", v);
        assert_eq!(read("sys/vm/drop_caches"), "0\n");
    }
    for v in ["0", "4", "x", ""] {
        assert!(node.write_at(0, v.as_bytes()).is_err(), "{:?}", v);
    }
    let ostype = read("sys/kernel/ostype");
    assert!(at("sys/kernel/ostype").write_at(0, b"BSD\n").is_err());
    assert_eq!(read("sys/kernel/ostype"), ostype);
}

/// `/proc/sysvipc/{msg,sem,shm}` open with the header line `ipcs` parses,
/// whatever is in them.
#[test]
fn the_sysvipc_tables_open_with_their_headers() {
    for (table, first) in [("msg", "key"), ("sem", "key"), ("shm", "key")] {
        let text = read(&format!("sysvipc/{}", table));
        let header = text.lines().next().unwrap_or("");
        assert_eq!(
            header.split_whitespace().next(),
            Some(first),
            "{}: {:?}",
            table,
            header
        );
    }
}

/// `/proc` itself: every fixed name it lists can be opened, and a number
/// that is no process is not there.
#[test]
fn every_fixed_name_proc_lists_can_be_opened() {
    let r = root();
    let names = listing(&r);
    assert_eq!(&names[..2], [".", ".."]);
    for name in PROC_ROOT_STATIC {
        assert!(names.iter().any(|n| n == name), "{} not listed", name);
        assert!(r.find(name).is_ok(), "{} listed but not there", name);
    }
    assert!(r.find("999999999").is_err());
    assert!(r.find("-1").is_err());
    assert_eq!(r.fs().info().ftype, rcore_fs::vfs::PROC_SUPER_MAGIC);
    assert!(r.write_at(0, b"x").is_err());
}

/// `/proc/version` is `uname -s`, `-r` and `-v`, laid out as Linux lays
/// it out: `<sysname> version <release> (...)`.
#[test]
fn proc_version_is_the_systems_name_version_and_release() {
    let v = read("version");
    let opens = format!(
        "{} version {} ",
        crate::uname::OS_TYPE,
        crate::uname::OS_RELEASE
    );
    assert!(v.starts_with(&opens), "{:?}", v);
    assert!(v.ends_with('\n'));
    assert!(read("cmdline").ends_with('\n'));
}
