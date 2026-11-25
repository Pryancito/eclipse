//! What every node of `/sys` says of itself, against what Linux's sysfs
//! (`fs/sysfs`, `fs/kernfs`) says: one filesystem, `SYSFS_MAGIC`, whose
//! attributes are lines of text and whose read-only ones refuse a write.

use super::tree_tests::{contents, listing, whole_tree};
use super::*;
use crate::net::test_card::{plug, Card};
use alloc::vec;
use zcore_drivers::wire::{IpAddress, IpCidr};

/// A machine with something in every class this kernel fills from its
/// devices: the bus, two disks, and two network cards.
fn with_a_machine<R>(f: impl FnOnce() -> R) -> R {
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let bus = test_bus::with(test_bus::a_real_machine());
    let _disks = test_bus::disks(&bus, &["nvme0", "virtio-blk"], 2048);
    let _cards = plug(vec![
        Card::ether("eth0", [0x52, 0x54, 0, 0x12, 0x34, 0x56])
            .with_address(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))
            .with_mtu(9000),
        Card::loopback(),
    ]);
    f()
}

/// `statfs("/sys/...")` of any node is sysfs: `f_type` is `SYSFS_MAGIC`,
/// which is how systemd, udev and `mountpoint` tell a sysfs from a
/// directory that is not one.
#[test]
fn every_node_is_of_one_sysfs() {
    with_a_machine(|| {
        let tree = whole_tree();
        assert!(tree.len() > 100, "{} nodes", tree.len());
        for node in &tree {
            let info = node.inode.fs().info();
            assert_eq!(info.ftype, rcore_fs::vfs::SYSFS_MAGIC, "{}", node.path);
            assert_eq!((info.bsize, info.namemax), (4096, 255), "{}", node.path);
            assert_eq!(
                node.inode.metadata().unwrap().dev,
                SYSFS_DEV,
                "{}",
                node.path
            );
        }
        assert!(SysFS::new().sync().is_ok());
        assert!(Arc::ptr_eq(&SysFS.root_inode(), &SYS_ROOT));
    });
}

/// A directory of sysfs is a directory: two links at least, readable,
/// owned by root, and nothing can be written into it as if it were a file.
#[test]
fn every_directory_is_a_directory_nobody_writes_into() {
    with_a_machine(|| {
        for node in whole_tree() {
            if node.type_ != FileType::Dir {
                continue;
            }
            let md = node.inode.metadata().unwrap();
            assert!(md.nlinks >= 2, "{}", node.path);
            assert_eq!((md.uid, md.gid), (0, 0), "{}", node.path);
            assert_ne!(md.mode & 0o555, 0, "{} can be listed", node.path);
            assert!(node.inode.write_at(0, b"x").is_err(), "{}", node.path);
            let poll = node.inode.poll().unwrap();
            assert!(poll.read && !poll.error, "{}", node.path);
            assert_eq!(
                node.inode.read_at(0, &mut [0u8; 8]).unwrap_or(0),
                0,
                "{}",
                node.path
            );
            // `.` is the directory itself, by inode.
            let dot = node.inode.find(".").unwrap();
            assert_eq!(dot.metadata().unwrap().inode, md.inode, "{}/.", node.path);
            assert!(node.inode.as_any_ref().type_id() != core::any::TypeId::of::<()>());
        }
    });
}

/// A name that is not in a directory is not found in it, and the error is
/// `ENOENT` (`EntryNotFound`), not something else.
#[test]
fn a_name_that_is_not_there_is_not_found_anywhere() {
    with_a_machine(|| {
        for node in whole_tree() {
            if node.type_ != FileType::Dir {
                continue;
            }
            assert_eq!(
                node.inode.find("no-such-entry").err(),
                Some(FsError::EntryNotFound),
                "{}",
                node.path
            );
        }
    });
}

/// An attribute with no write bit refuses a write, as kernfs refuses one
/// to a file with no `store` (`EACCES` before it, `EIO` in it), and the
/// file reads the same afterwards.
#[test]
fn a_read_only_attribute_refuses_a_write() {
    with_a_machine(|| {
        let mut checked = 0;
        for node in whole_tree() {
            if node.type_ != FileType::File {
                continue;
            }
            let md = node.inode.metadata().unwrap();
            if md.mode & 0o222 != 0 {
                continue;
            }
            let before = contents(&node.inode);
            assert!(node.inode.write_at(0, b"1\n").is_err(), "{}", node.path);
            assert_eq!(contents(&node.inode), before, "{}", node.path);
            checked += 1;
        }
        assert!(checked > 50, "only {} read-only attributes", checked);
    });
}

/// The attributes `sysfs_emit` writes are lines: what is read ends with a
/// newline. The binary ones (`config`, `descriptors`, `edid`) are not
/// text and are left out.
#[test]
fn a_text_attribute_is_a_line() {
    with_a_machine(|| {
        let binary = ["config", "descriptors", "edid", "rom"];
        for node in whole_tree() {
            if node.type_ != FileType::File {
                continue;
            }
            let name = node.path.rsplit('/').next().unwrap();
            if binary.contains(&name) {
                continue;
            }
            let text = contents(&node.inode);
            if text.is_empty() {
                continue;
            }
            assert_eq!(
                text.last(),
                Some(&b'\n'),
                "{} = {:?}",
                node.path,
                String::from_utf8_lossy(&text)
            );
            assert!(
                core::str::from_utf8(&text).is_ok(),
                "{} is not text",
                node.path
            );
        }
    });
}

/// `/sys/class/net/<if>`: the card's hardware address as `ip link` prints
/// it, and the loopback's of zeros.
#[test]
fn a_network_card_says_its_hardware_address() {
    with_a_machine(|| {
        let net = SYS_ROOT.lookup_follow("class/net", 4).unwrap();
        let names = listing(&net);
        assert!(names.contains(&String::from("eth0")) && names.contains(&String::from("lo")));
        let read = |card: &str, attr: &str| {
            let node = net.find(card).unwrap().find(attr).unwrap();
            String::from_utf8(contents(&node)).unwrap()
        };
        assert_eq!(read("eth0", "address"), "52:54:00:12:34:56\n");
        assert_eq!(read("eth0", "operstate"), "up\n");
        assert_eq!(read("lo", "address"), "00:00:00:00:00:00\n");
        assert_eq!(read("lo", "operstate"), "unknown\n");
        // A card that is listed is a directory of its own, numbered apart
        // from the other.
        let a = net.find("eth0").unwrap().metadata().unwrap();
        let b = net.find("lo").unwrap().metadata().unwrap();
        assert_eq!(a.type_, FileType::Dir);
        assert_ne!(a.inode, b.inode);
        assert_eq!(net.find("wlan9").err(), Some(FsError::EntryNotFound));
    });
}

/// `/sys/block/<disk>/size` is in 512-byte sectors, and a disk on a bus
/// that does not unplug is not `removable`.
#[test]
fn a_disk_says_its_size_in_sectors() {
    with_a_machine(|| {
        let block = SYS_ROOT.lookup_follow("block", 4).unwrap();
        let disks: Vec<String> = listing(&block).into_iter().skip(2).collect();
        assert_eq!(disks.len(), 2, "{:?}", disks);
        for disk in &disks {
            let d = block.find(disk).unwrap();
            let read = |attr: &str| String::from_utf8(contents(&d.find(attr).unwrap())).unwrap();
            assert_eq!(read("size"), "2048\n", "{}", disk);
            assert_eq!(read("removable"), "0\n", "{}", disk);
        }
    });
}
