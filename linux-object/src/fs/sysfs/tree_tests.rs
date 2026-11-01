//! The shape of `/sys`, walked whole.
//!
//! Everything userspace knows about the machine it runs on comes from
//! walking this tree: udev enumerates it, libinput reads the input class
//! out of it, and libdrm and Mesa resolve a GPU through four of its
//! symlinks before they will open a device. All of that is `readdir`,
//! `open` and `readlink` over twenty-nine directories, and none of it was
//! reachable from a test until the bus became a seam.

use super::*;
use alloc::vec;

/// One node of the tree, with where it was found and who found it.
struct Node {
    path: String,
    inode: Arc<dyn INode>,
    parent_ino: usize,
    type_: FileType,
}

/// Every node reachable from `/sys`, without following a symlink --
/// following them would make the walk infinite, and they get a test of
/// their own.
fn whole_tree() -> Vec<Node> {
    let mut out: Vec<Node> = vec![];
    let root = SYS_ROOT.clone();
    let root_ino = root.metadata().unwrap().inode;
    let mut queue = vec![Node {
        path: String::from(""),
        inode: root,
        parent_ino: root_ino,
        type_: FileType::Dir,
    }];
    while let Some(node) = queue.pop() {
        if node.type_ == FileType::Dir {
            let ino = node.inode.metadata().unwrap().inode;
            for name in listing(&node.inode) {
                if name == "." || name == ".." {
                    continue;
                }
                let child = node
                    .inode
                    .find(&name)
                    .unwrap_or_else(|e| panic!("{}/{} listed but {:?}", node.path, name, e));
                let type_ = child.metadata().unwrap().type_;
                queue.push(Node {
                    path: format!("{}/{}", node.path, name),
                    inode: child,
                    parent_ino: ino,
                    type_,
                });
            }
        }
        out.push(node);
    }
    out
}

/// What `readdir` would return: `get_entry` from 0 until it stops.
fn listing(node: &Arc<dyn INode>) -> Vec<String> {
    let mut names = vec![];
    for id in 0.. {
        match node.get_entry(id) {
            Ok(name) => names.push(name),
            Err(_) => break,
        }
        assert!(
            id < 4096,
            "a listing that does not end is an `ls` that does not end"
        );
    }
    names
}

fn contents(node: &Arc<dyn INode>) -> Vec<u8> {
    let mut out = vec![];
    let mut buf = [0u8; 64];
    loop {
        let n = node.read_at(out.len(), &mut buf).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
        assert!(out.len() < 1 << 16, "a file that never ends");
    }
}

#[test]
fn every_directory_lists_itself_and_its_parent_first() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // `getdents` goes straight through `get_entry` and synthesises
    // nothing, so a directory numbering from its first real name is one
    // `readdir` never reports `.` or `..` for -- which every other
    // filesystem this kernel mounts does report.
    let _bus = test_bus::with(test_bus::a_real_machine());
    for node in whole_tree() {
        if node.type_ != FileType::Dir {
            continue;
        }
        let names = listing(&node.inode);
        assert_eq!(
            names.first().map(|s| s.as_str()),
            Some("."),
            "{}",
            node.path
        );
        assert_eq!(
            names.get(1).map(|s| s.as_str()),
            Some(".."),
            "{}",
            node.path
        );
        assert!(
            !names[2..].iter().any(|n| n == "." || n == ".."),
            "{} lists them twice",
            node.path
        );
    }
}

#[test]
fn an_empty_directory_still_has_the_two() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let _bus = test_bus::with(vec![]);
    let empty = SYS_ROOT.lookup_follow("class/power_supply", 4).unwrap();
    assert_eq!(listing(&empty), vec![String::from("."), String::from("..")]);
}

#[test]
fn dot_is_the_directory_and_dotdot_is_the_one_above_it() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let _bus = test_bus::with(test_bus::a_real_machine());
    for node in whole_tree() {
        if node.type_ != FileType::Dir {
            continue;
        }
        let ino = node.inode.metadata().unwrap().inode;
        let here = node.inode.find(".").unwrap().metadata().unwrap().inode;
        assert_eq!(here, ino, "{}/. is somewhere else", node.path);
        let up = node.inode.find("..").unwrap().metadata().unwrap().inode;
        assert_eq!(
            up, node.parent_ino,
            "{}/.. is not the directory above",
            node.path
        );
    }
}

#[test]
fn walking_up_from_a_power_supply_reaches_the_class_directory() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // Its `..` used to be itself, so anything walking up from a battery
    // -- which is how a power daemon finds the device one belongs to --
    // stayed in `/sys/class/power_supply` for ever.
    let _bus = test_bus::with(vec![]);
    let class = SYS_ROOT.lookup_follow("class", 4).unwrap();
    let up = SYS_ROOT.lookup_follow("class/power_supply/..", 4).unwrap();
    assert_eq!(
        up.metadata().unwrap().inode,
        class.metadata().unwrap().inode
    );
    let twice = SYS_ROOT
        .lookup_follow("class/power_supply/../..", 4)
        .unwrap();
    assert_eq!(
        twice.metadata().unwrap().inode,
        SYS_ROOT.metadata().unwrap().inode
    );
}

#[test]
fn every_name_a_directory_lists_can_be_opened() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // The walk panics on a name `find` refuses, so reaching the end is
    // the assertion; the count keeps it from passing on an empty tree.
    let _bus = test_bus::with(test_bus::a_real_machine());
    assert!(whole_tree().len() > 60);
}

#[test]
fn a_listing_ends_instead_of_repeating_itself_or_panicking() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let _bus = test_bus::with(test_bus::a_real_machine());
    for node in whole_tree() {
        if node.type_ != FileType::Dir {
            continue;
        }
        let n = listing(&node.inode).len();
        assert_eq!(
            node.inode.get_entry(n),
            Err(FsError::EntryNotFound),
            "{}",
            node.path
        );
        assert_eq!(
            node.inode.get_entry(usize::MAX),
            Err(FsError::EntryNotFound),
            "{} on the last id there is",
            node.path
        );
    }
}

#[test]
fn no_two_directories_of_the_tree_claim_the_same_inode() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // They were literals spaced ten apart with the per-device ones
    // written `40 + index` and `100 + index`, so the eleventh PCI
    // function was `/sys/devices/system` over again and the first disk
    // was `/sys/class/thermal`.
    let _bus = test_bus::with(test_bus::a_real_machine());
    let mut seen: Vec<(usize, String)> = vec![];
    for node in whole_tree() {
        if node.type_ != FileType::Dir {
            continue;
        }
        let ino = node.inode.metadata().unwrap().inode;
        if let Some((_, other)) = seen.iter().find(|(i, _)| *i == ino) {
            panic!("{} and {} are both inode {}", other, node.path, ino);
        }
        seen.push((ino, node.path.clone()));
    }
}

#[test]
fn a_bus_wider_than_the_old_numbering_allowed_still_numbers_apart() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // Twenty functions, which an ordinary desktop passes once the
    // chipset's own are counted.
    let devs: Vec<PciDevInfo> = (0..20)
        .map(|i| test_bus::dev(&format!("0000:00:{:02x}.0", i), 0x8086, 0x1234, 0x060000))
        .collect();
    let _bus = test_bus::with(devs);
    let mut seen: Vec<usize> = vec![];
    for node in whole_tree() {
        if node.type_ != FileType::Dir {
            continue;
        }
        let ino = node.inode.metadata().unwrap().inode;
        assert!(!seen.contains(&ino), "{} repeats inode {}", node.path, ino);
        seen.push(ino);
    }
}

#[test]
fn every_symlink_resolves_to_something_in_the_tree() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // Four of these are what libdrm walks to decide which device a card
    // belongs to, and the only thing that can be wrong with one is the
    // number of `..` in it, which reads as correct however long you look.
    let _bus = test_bus::with(test_bus::a_real_machine());
    let mut links = 0;
    for node in whole_tree() {
        if node.type_ != FileType::SymLink {
            continue;
        }
        let target = contents(&node.inode);
        let target = core::str::from_utf8(&target).unwrap();
        let path = node.path.trim_start_matches('/');
        let found = SYS_ROOT
            .lookup_follow(path, 8)
            .unwrap_or_else(|e| panic!("{} -> {} is {:?}", node.path, target, e));
        assert_eq!(
            found.metadata().unwrap().type_,
            FileType::Dir,
            "{} -> {} is not a directory",
            node.path,
            target
        );
        assert_eq!(
            node.inode.metadata().unwrap().size,
            target.len(),
            "{} reports the wrong length for its target",
            node.path
        );
        links += 1;
    }
    assert!(
        links >= 10,
        "only {} symlinks: the tree came up empty",
        links
    );
}

#[test]
fn a_file_read_from_the_middle_gives_the_rest_of_it() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // `cat` reads whatever its buffer holds and asks again from where it
    // stopped. A file answering from the beginning every time never ends.
    let _bus = test_bus::with(test_bus::a_real_machine());
    for node in whole_tree() {
        if node.type_ != FileType::File {
            continue;
        }
        let whole = contents(&node.inode);
        for offset in [0, 1, whole.len() / 2, whole.len()] {
            let offset = offset.min(whole.len());
            let mut buf = vec![0u8; whole.len() + 8];
            let n = node.inode.read_at(offset, &mut buf).unwrap();
            assert_eq!(&buf[..n], &whole[offset..], "{} at {}", node.path, offset);
        }
        assert_eq!(
            node.inode.read_at(whole.len() + 1, &mut [0u8; 8]).unwrap(),
            0,
            "{} past its end",
            node.path
        );
    }
}

#[test]
fn the_bus_the_tree_shows_is_the_bus_it_was_given() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let _bus = test_bus::with(test_bus::a_real_machine());
    let devices = SYS_ROOT.lookup_follow("bus/pci/devices", 4).unwrap();
    let names = listing(&devices);
    assert_eq!(&names[..2], &[String::from("."), String::from("..")]);
    assert_eq!(names[2], "0000:00:00.0");
    assert_eq!(names.len(), 8, "six functions and the two");

    // The card Moebius runs, read back the way udev reads it.
    let card = SYS_ROOT
        .lookup_follow("devices/pci0000:00/0000:02:00.0", 4)
        .unwrap();
    assert_eq!(contents(&card.find("vendor").unwrap()), b"0x10de\n");
    assert_eq!(contents(&card.find("device").unwrap()), b"0x1f06\n");
    assert_eq!(contents(&card.find("class").unwrap()), b"0x030000\n");
    // Upper case, because `modules.alias` is upper case and `kmod`
    // compares the two with `fnmatch`.
    assert_eq!(
        contents(&card.find("modalias").unwrap()),
        b"pci:v000010DEd00001F06sv00000000sd00000000bc03sc00i00\n"
    );
    assert_eq!(
        contents(&card.find("uevent").unwrap()),
        b"PCI_CLASS=30000\nPCI_ID=10DE:1F06\nPCI_SUBSYS_ID=0000:0000\n\
          PCI_SLOT_NAME=0000:02:00.0\n\
          MODALIAS=pci:v000010DEd00001F06sv00000000sd00000000bc03sc00i00\n"
            .as_slice()
    );

    // The 256 bytes `config` hands out are the header a driver expects to
    // find the device at: ids at 0, then revision, prog-if, subclass and
    // base class at 8 through 11.
    let cfg = contents(&card.find("config").unwrap());
    assert_eq!(cfg.len(), 256);
    assert_eq!(&cfg[0..4], &[0xde, 0x10, 0x06, 0x1f]);
    assert_eq!(&cfg[8..12], &[0x00, 0x00, 0x00, 0x03]);

    // Its HDMI audio function is a different device, not the card again.
    let audio = SYS_ROOT
        .lookup_follow("devices/pci0000:00/0000:02:00.1", 4)
        .unwrap();
    assert_eq!(contents(&audio.find("device").unwrap()), b"0x10f9\n");
    assert_eq!(contents(&audio.find("class").unwrap()), b"0x040300\n");
    assert_ne!(
        card.metadata().unwrap().inode,
        audio.metadata().unwrap().inode
    );
}

#[test]
fn a_class_with_a_hex_letter_in_it_keeps_its_case() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // An xHCI controller is class 0x0c0330, and the `0c` is the only
    // reason to look: every other number on a desk is digits. The old
    // modalias took the three bytes as text straight out of the string
    // the scan had formatted, which was lower case.
    let _bus = test_bus::with(vec![test_bus::dev(
        "0000:00:14.0",
        0x8086,
        0xa36d,
        0x0c0330,
    )]);
    let dev = SYS_ROOT
        .lookup_follow("bus/pci/devices/0000:00:14.0", 4)
        .unwrap();
    assert_eq!(
        contents(&dev.find("modalias").unwrap()),
        b"pci:v00008086d0000A36Dsv00000000sd00000000bc0Csc03i30\n"
    );
    let cfg = contents(&dev.find("config").unwrap());
    assert_eq!(&cfg[9..12], &[0x30, 0x03, 0x0c], "prog-if, subclass, class");
}

#[test]
fn a_device_with_nothing_readable_in_it_still_answers() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // `scan_pci_devices` writes these strings and they are always
    // well-formed, but the synthetic compute alias builds one by hand
    // with `0x0000`, and a parse that gave up used to leave three
    // different files disagreeing about what it had given up on.
    let ids = PciIds::parse("nonsense", "0x", "");
    assert_eq!(
        ids,
        PciIds {
            vendor: 0,
            device: 0,
            class: 0
        }
    );
    assert_eq!(
        ids.modalias(),
        "pci:v00000000d00000000sv00000000sd00000000bc00sc00i00\n"
    );
    assert!(ids.uevent("0000:00:00.0").ends_with(&ids.modalias()));
}

#[test]
fn a_disk_is_the_same_disk_from_both_places_that_show_it() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let _bus = test_bus::with(test_bus::a_real_machine());
    // Moebius's NVMe, plus the SATA disk he has said he does not have
    // here but the installer still has to name.
    let _disks = test_bus::disks(&_bus, &["nvme0", "ahci0"], 2048);

    let block = SYS_ROOT.lookup_follow("block", 4).unwrap();
    assert_eq!(
        listing(&block),
        vec![
            String::from("."),
            String::from(".."),
            String::from("nvme0n1"),
            String::from("sda"),
        ]
    );

    // `/sys/class/block` shows the same names, as links to those.
    let class_block = SYS_ROOT.lookup_follow("class/block", 4).unwrap();
    assert_eq!(listing(&class_block), listing(&block));
    let link = class_block.find("nvme0n1").unwrap();
    assert_eq!(link.metadata().unwrap().type_, FileType::SymLink);
    assert_eq!(contents(&link), b"../../block/nvme0n1");

    // And following it lands on the disk itself, not somewhere else
    // with the same name.
    let by_link = SYS_ROOT.lookup_follow("class/block/nvme0n1", 4).unwrap();
    let direct = SYS_ROOT.lookup_follow("block/nvme0n1", 4).unwrap();
    assert_eq!(
        by_link.metadata().unwrap().inode,
        direct.metadata().unwrap().inode
    );
    assert_eq!(contents(&direct.find("size").unwrap()), b"2048\n");
}

#[test]
fn walking_up_from_a_disk_goes_back_the_way_it_came() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let _bus = test_bus::with(vec![]);
    let _disks = test_bus::disks(&_bus, &["nvme0"], 512);

    // `/sys/block/<dev>/..` is `/sys/block`, whose own `..` is `/sys`.
    let disk = SYS_ROOT.lookup_follow("block/nvme0n1", 4).unwrap();
    let up = disk.find("..").unwrap();
    assert_eq!(
        up.metadata().unwrap().inode,
        SYS_ROOT
            .lookup_follow("block", 4)
            .unwrap()
            .metadata()
            .unwrap()
            .inode
    );
    assert_eq!(
        up.find("..").unwrap().metadata().unwrap().inode,
        SYS_ROOT.metadata().unwrap().inode
    );

    // `/sys/class/block`'s is `/sys/class`. Mounting one directory at
    // both places made this one `/sys`, so a walk up from a disk found
    // under `class/` skipped the subsystem it belongs to -- which is
    // precisely what libudev reads that walk for.
    let cb = SYS_ROOT.lookup_follow("class/block", 4).unwrap();
    assert_eq!(
        cb.find("..").unwrap().metadata().unwrap().inode,
        SYS_ROOT
            .lookup_follow("class", 4)
            .unwrap()
            .metadata()
            .unwrap()
            .inode
    );
    assert_ne!(
        cb.metadata().unwrap().inode,
        SYS_ROOT
            .lookup_follow("block", 4)
            .unwrap()
            .metadata()
            .unwrap()
            .inode
    );
}

#[test]
fn a_twenty_seventh_disk_does_not_take_the_first_one_s_name() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // `sda` through `sdz`, and then what? The naming wrapped with
    // `% 26` and started over, so two disks answered to `sda` and
    // `block_index_by_name` handed out the first of them for both --
    // the installer would have written a partition table to the wrong
    // one. Linux goes on to `sdaa`.
    let _bus = test_bus::with(vec![]);
    let names: Vec<String> = (0..27).map(|i| format!("ahci{}", i)).collect();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let _disks = test_bus::disks(&_bus, &refs, 64);

    let listed = list_block_devices();
    assert_eq!(listed.len(), 27);
    assert_eq!(listed[25], "sdz");
    assert_eq!(listed[26], "sdaa");

    let mut sorted = listed.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 27, "two disks share a name: {:?}", listed);

    // And every name still finds the disk it belongs to.
    for (i, name) in listed.iter().enumerate() {
        assert_eq!(block_index_by_name(name), Some(i), "{}", name);
    }

    // A name that is merely the start of one is not that disk. `sd`
    // resolving to `sda` would make `/sys/block/sd` a second way in.
    assert_eq!(block_index_by_name("sd"), None);
    assert_eq!(block_index_by_name("sda"), Some(0));
    assert_eq!(block_index_by_name("sdaa"), Some(26));
    assert_eq!(block_index_by_name("sdaaa"), None);
}

#[test]
fn el_arranque_por_uefi_se_anuncia_en_sys_firmware_efi() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // Lo que mira todo instalador para decidir la tabla de particiones.
    // Los dos cargadores del arbol son aplicaciones UEFI -- rboot en
    // x86_64 y rayboot en aarch64 -- asi que en las dos esto tiene que
    // estar; sin ello `install-eclipse` recomienda MBR/BIOS y escribe un
    // disco que el firmware no ofrece arrancar.
    let _bus = test_bus::with(vec![]);
    let firmware = SYS_ROOT.lookup_follow("firmware", 4).unwrap();
    let listed = listing(&firmware);

    if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        assert!(listed.contains(&String::from("efi")), "{:?}", listed);
        let efi = SYS_ROOT.lookup_follow("firmware/efi", 4).unwrap();
        assert_eq!(efi.metadata().unwrap().type_, FileType::Dir);
        let size = SYS_ROOT
            .lookup_follow("firmware/efi/fw_platform_size", 4)
            .unwrap();
        assert_eq!(contents(&size), b"64\n");
    } else {
        // riscv64 no arranca por UEFI (lo carga QEMU con `-kernel`):
        // el directorio existe vacio, que es la respuesta honesta, no un
        // `efi` de mentira.
        assert!(!listed.contains(&String::from("efi")), "{:?}", listed);
        assert!(SYS_ROOT.lookup_follow("firmware/efi", 4).is_err());
    }
}

#[test]
fn a_machine_with_no_bus_at_all_still_has_a_tree() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    // Not hypothetical: it is what every non-PCI machine looks like, and
    // the walk has to end rather than fault or run on.
    let _bus = test_bus::with(vec![]);
    let devices = SYS_ROOT.lookup_follow("bus/pci/devices", 4).unwrap();
    assert_eq!(
        listing(&devices),
        vec![String::from("."), String::from("..")]
    );
    assert!(whole_tree().len() > 30);
}

#[test]
fn sysfs_device_id_is_distinct_from_root_fs() {
    // DRM first, then the bus: `whole_tree()` walks `/sys` and a good
    // part of that tree comes out of the DRM node table, which another
    // thread rebuilds. Taking it here and not inside `whole_tree()` is
    // what keeps the order the same as everywhere else -- see
    // `sysfs::card0_identity_tests`, which already does DRM then bus.
    // The reverse order deadlocks: both are plain non-reentrant
    // `std::sync::Mutex`, and the DRM side of sysfs reads the bus table.
    let _drm_table = crate::fs::devfs::drm::test_globals::lock();
    let _bus = test_bus::with(test_bus::a_real_machine());
    let root_md = SYS_ROOT.metadata().unwrap();
    assert_eq!(root_md.dev, SYSFS_DEV);
    assert_ne!(
        (root_md.dev, root_md.inode),
        (0, 1),
        "/sys must not share (dev=0, ino=1) with / or systemd's chaseat/dir_fd_is_root mistakes /sys for /"
    );
    for node in whole_tree() {
        let md = node.inode.metadata().unwrap();
        assert_eq!(md.dev, SYSFS_DEV, "{} has dev={}", node.path, md.dev);
    }
}
