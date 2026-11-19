//! Against a real NFS server: ganesha on this machine, exporting
//! `/srv/ganesha` over NFSv3 with `no_root_squash`, with `fichero.txt`
//! ("hola desde nfs"), a symbolic link `enlace` to it and a directory
//! `sub` in it. Ignored by default, as the kernel's own suite against it
//! is (`linux-object/src/fs/nfs/real_server_tests.rs`):
//!
//! ```text
//! rpcbind -w; ganesha.nfsd -F -f /etc/ganesha/ganesha.conf &
//! cargo test --manifest-path tools/eclipse-nfs/Cargo.toml -- --ignored --test-threads=1
//! ```

use super::*;
use crate::net::{Dial, Transport};
use fatserve::wire::DATA_AREA;
use nfs3::connect::Connector;
use nfs3::greeting;
use nfs3::source::{NfsOptions, NfsSource};

fn export() -> Export<Transport> {
    let source = NfsSource::parse("127.0.0.1:/srv/ganesha").unwrap();
    let opts = NfsOptions::default();
    let mut dial = Dial::new("127.0.0.1".parse().unwrap());
    let greeted = greeting::greet(&mut dial, &source, &opts).expect("the greeting");
    let transport = dial.connect(greeted.nfs_port, 5_000).expect("nfsd");
    let mut session = NfsSession::new(transport, dial.first_xid(), greeted.root, &opts);
    session.negotiate().expect("FSINFO");
    Export::new(session, false)
}

/// The name and mode of each record in `out[..used]`.
fn records(out: &[u8]) -> Vec<(String, u32, u64)> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < out.len() {
        let len = u16::from_ne_bytes(out[at..at + 2].try_into().unwrap()) as usize;
        let name_len = u16::from_ne_bytes(out[at + 2..at + 4].try_into().unwrap()) as usize;
        let mode = u32::from_ne_bytes(out[at + 4..at + 8].try_into().unwrap());
        let size = u64::from_ne_bytes(out[at + 8..at + 16].try_into().unwrap());
        let name = &out[at + RECORD_HEAD..at + RECORD_HEAD + name_len];
        found.push((String::from_utf8(name.to_vec()).unwrap(), mode, size));
        at += len;
    }
    found
}

fn listing(e: &mut Export<Transport>, path: &str) -> Vec<(String, u32, u64)> {
    let mut out = vec![0u8; DATA_AREA];
    let (_, used, next) = e.list(path, 0, &mut out).expect("the listing");
    assert_eq!(next, LIST_END);
    records(&out[..used])
}

/// A name of its own for each test, so they do not meet on the server.
fn scratch(e: &mut Export<Transport>, name: &str) -> String {
    let path = format!("prueba-{name}");
    let _ = e.remove(&format!("{path}/dentro"));
    let _ = e.remove(&path);
    path
}

#[test]
#[ignore]
fn the_top_lists_what_the_export_holds() {
    let mut e = export();
    let found = listing(&mut e, "");
    let names: Vec<&str> = found.iter().map(|f| f.0.as_str()).collect();
    assert!(names.contains(&"fichero.txt"), "{names:?}");
    assert!(!names.contains(&"."), "no . in a listing: {names:?}");
    let link = found.iter().find(|f| f.0 == "enlace").expect("the link");
    assert_eq!(link.1 & S_IFMT, S_IFLNK);
    let dir = found.iter().find(|f| f.0 == "sub").expect("the directory");
    assert_eq!(dir.1 & S_IFMT, S_IFDIR);
}

#[test]
#[ignore]
fn a_file_and_a_link_read_as_they_are() {
    let mut e = export();
    let mut out = [0u8; 64];
    let n = e.read("fichero.txt", 0, &mut out).unwrap();
    assert_eq!(&out[..n], b"hola desde nfs");
    let n = e.read("fichero.txt", 5, &mut out).unwrap();
    assert_eq!(&out[..n], b"desde nfs");
    let n = e.read("enlace", 0, &mut out).unwrap();
    assert_eq!(&out[..n], b"fichero.txt");
    assert_eq!(e.read("no-esta", 0, &mut out), Err(libc::ENOENT));
    assert_eq!(e.read("sub", 0, &mut out), Err(libc::EISDIR));
}

#[test]
#[ignore]
fn a_file_is_made_written_renamed_and_removed() {
    let mut e = export();
    let path = scratch(&mut e, "fichero");
    let to = format!("{path}-otro");
    let _ = e.remove(&to);
    let mut out = vec![0u8; 4096];
    let n = e.create(&path, S_IFREG | 0o640, &mut out).unwrap();
    let made = records(&out[..n]);
    assert_eq!(made[0].0, path);
    assert_eq!(made[0].1, S_IFREG | 0o640);
    assert_eq!(
        e.create(&path, S_IFREG | 0o640, &mut out),
        Err(libc::EEXIST)
    );

    let data: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
    assert_eq!(e.write(&path, 0, &data[..65536]).unwrap(), 65536);
    assert_eq!(
        e.write(&path, 65536, &data[65536..]).unwrap(),
        data.len() - 65536
    );
    assert!(e.is_dirty());
    e.commit().unwrap();
    assert!(!e.is_dirty());
    let mut back = vec![0u8; data.len() + 10];
    assert_eq!(e.read(&path, 0, &mut back[..65536]).unwrap(), 65536);
    assert_eq!(
        e.read(&path, 65536, &mut back[65536..]).unwrap(),
        data.len() - 65536
    );
    assert_eq!(&back[..data.len()], &data[..]);

    e.truncate(&path, 10).unwrap();
    e.set_time(&path, 1_000_000_000).unwrap();
    e.set_attr(&path, 0, 0, 0o600).unwrap();
    let n = e.stat(&path, &mut out).unwrap();
    let st = &out[..n];
    assert_eq!(records(st)[0].1, S_IFREG | 0o600);
    assert_eq!(records(st)[0].2, 10);
    assert_eq!(
        i64::from_ne_bytes(st[16..24].try_into().unwrap()),
        1_000_000_000
    );

    let n = e.rename(&path, &to, &mut out).unwrap();
    assert_eq!(records(&out[..n])[0].0, to);
    assert_eq!(e.stat(&path, &mut out), Err(libc::ENOENT));
    e.remove(&to).unwrap();
    assert_eq!(e.stat(&to, &mut out), Err(libc::ENOENT));
}

#[test]
#[ignore]
fn a_directory_holds_what_is_made_in_it() {
    let mut e = export();
    let path = scratch(&mut e, "dir");
    let mut out = vec![0u8; 4096];
    e.create(&path, S_IFDIR | 0o755, &mut out).unwrap();
    let inside = format!("{path}/dentro");
    e.create(&inside, S_IFREG | 0o644, &mut out).unwrap();
    let found = listing(&mut e, &path);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "dentro");
    assert_eq!(e.remove(&path), Err(libc::ENOTEMPTY));
    e.remove(&inside).unwrap();
    e.remove(&path).unwrap();
}

#[test]
#[ignore]
fn a_link_is_made_when_its_target_is_written() {
    let mut e = export();
    let path = scratch(&mut e, "enlace");
    let mut out = vec![0u8; 4096];
    let n = e.create(&path, S_IFLNK | 0o777, &mut out).unwrap();
    assert_eq!(records(&out[..n])[0].1 & S_IFMT, S_IFLNK);
    assert!(
        listing(&mut e, "").iter().any(|f| f.0 == path),
        "listed while held"
    );
    e.write(&path, 0, b"fichero.txt").unwrap();
    let mut target = [0u8; 64];
    let n = e.read(&path, 0, &mut target).unwrap();
    assert_eq!(&target[..n], b"fichero.txt");
    // On the server, as a new export finds it.
    let mut fresh = export();
    let n = fresh.read(&path, 0, &mut target).unwrap();
    assert_eq!(&target[..n], b"fichero.txt");
    // A target written again makes it again, with the bytes written where
    // they were written, as in a file.
    e.write(&path, 0, b"sub").unwrap();
    let n = fresh.read(&path, 0, &mut target).unwrap();
    assert_eq!(&target[..n], b"subhero.txt");
    e.remove(&path).unwrap();
}

#[test]
#[ignore]
fn statfs_says_nfs() {
    let mut e = export();
    let mut out = [0u8; 40];
    assert_eq!(e.statfs(&mut out).unwrap(), 40);
    let n = |i: usize| u64::from_ne_bytes(out[i * 8..i * 8 + 8].try_into().unwrap());
    assert_eq!(n(0), BLOCK);
    assert!(n(1) > 0 && n(2) <= n(1));
    assert_eq!(n(4), NFS_MAGIC);
}

#[test]
#[ignore]
fn a_read_only_export_changes_nothing() {
    let mut e = export();
    e.read_only = true;
    let mut out = vec![0u8; 4096];
    assert_eq!(
        e.create("nada", S_IFREG | 0o644, &mut out),
        Err(libc::EROFS)
    );
    assert_eq!(e.write("fichero.txt", 0, b"x"), Err(libc::EROFS));
}

#[test]
fn a_record_is_what_the_kernel_reads() {
    let attr = Fattr3 {
        rdev: (8, 1),
        size: 42,
        ..held_link(b"", 0o644)
    };
    let mut out = [0u8; 80];
    put_record("abc", &attr, &mut out);
    assert_eq!(
        u16::from_ne_bytes([out[0], out[1]]) as usize,
        record_len("abc")
    );
    assert_eq!(record_len("abc"), 72);
    assert_eq!(
        u32::from_ne_bytes(out[4..8].try_into().unwrap()),
        S_IFLNK | 0o644
    );
    assert_eq!(u64::from_ne_bytes(out[8..16].try_into().unwrap()), 42);
    assert_eq!(u64::from_ne_bytes(out[56..64].try_into().unwrap()), 0x801);
    assert_eq!(&out[64..67], b"abc");
}

#[test]
fn paths_split_and_nest() {
    assert_eq!(split("a/b/c"), ("a/b", "c"));
    assert_eq!(split("c"), ("", "c"));
    assert!(under("a/b", "a"));
    assert!(!under("ab", "a"));
    assert!(under("x", ""));
}
