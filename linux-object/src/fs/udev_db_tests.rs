//! `/run/udev/data`, as libudev reads it: with no udevd on the machine the
//! kernel writes the records udevd would, and libinput takes a device only
//! if its record says it is initialized (`I:`), on a seat (`G:seat`) and
//! what kind of input it is (`E:ID_INPUT_*`, udev's `input_id` builtin).
//!
//! The records of devices a process serves come and go with the server;
//! these tests drive that path, with `/run` a ramfs of their own.

use super::*;
use crate::fs::devfs::served::input::Caps;
use rcore_fs::vfs::{FileType, FsError};
use rcore_fs_ramfs::RamFS;

static ALONE: lock::Mutex<()> = lock::Mutex::new(());

/// `/run` with udev's directories, made the way the boot makes them, and
/// put back as it was when the test is done.
struct Run {
    root: Arc<dyn INode>,
    was: Option<UdevInputDirs>,
    _alone: lock::MutexGuard<'static, ()>,
}

impl Drop for Run {
    fn drop(&mut self) {
        *UDEV_INPUT_DIRS.lock() = self.was.take();
    }
}

fn a_run() -> Run {
    let alone = ALONE.lock();
    let was = UDEV_INPUT_DIRS.lock().take();
    let ramfs = RamFS::new();
    fill_udev_db(&ramfs);
    Run {
        root: ramfs.root_inode(),
        was,
        _alone: alone,
    }
}

impl Run {
    fn at(&self, path: &str) -> Result<Arc<dyn INode>> {
        let mut node = self.root.clone();
        for c in path.split('/') {
            node = node.find(c)?;
        }
        Ok(node)
    }

    fn record(&self, name: &str) -> String {
        let f = self.at(&alloc::format!("udev/data/{}", name)).unwrap();
        String::from_utf8(f.read_as_vec().unwrap()).unwrap()
    }
}

fn bits(codes: &[u16], bytes: usize) -> Vec<u8> {
    let mut map = alloc::vec![0u8; bytes];
    for &c in codes {
        map[c as usize / 8] |= 1 << (c % 8);
    }
    map
}

fn caps(key: &[u16], rel: &[u16], abs: &[u16]) -> Caps {
    Caps {
        key: bits(key, 0x300 / 8),
        rel: bits(rel, 2),
        abs: bits(abs, 8),
    }
}

const KEY_ESC: u16 = 1;
const KEY_A: u16 = 30;
const BTN_LEFT: u16 = 0x110;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOUCH: u16 = 0x14a;

/// The boot makes `udev/data` and the `seat` and `seat0` tag directories
/// whether or not there is an input device yet, mode 0755 like udevd's.
#[test]
fn the_boot_makes_udevs_directories_with_no_device_in_them() {
    let run = a_run();
    for dir in [
        "udev",
        "udev/data",
        "udev/tags",
        "udev/tags/seat",
        "udev/tags/seat0",
    ] {
        let md = run.at(dir).unwrap().metadata().unwrap();
        assert_eq!(md.type_, FileType::Dir, "{}", dir);
        assert_eq!(md.mode & 0o777, 0o755, "{}", dir);
    }
}

/// A keyboard served as `event<N>` is the record `c13:<64+N>` (char major
/// 13, the evdev minors from 64), tagged on both seats, and carries what
/// udev's `input_id` says of a keyboard; its server going takes all three
/// files away.
#[test]
fn a_served_keyboard_gets_udevs_record_and_loses_it_with_its_server() {
    let run = a_run();
    udev_input_record(210, &caps(&[KEY_ESC, KEY_A], &[], &[]));
    assert_eq!(
        run.record("c13:274"),
        "I:1\nE:ID_INPUT=1\nE:ID_SEAT=seat0\nE:ID_INPUT_KEYBOARD=1\nE:ID_INPUT_KEY=1\n\
         G:seat\nQ:seat\nV:1\n"
    );
    for tag in ["udev/tags/seat/c13:274", "udev/tags/seat0/c13:274"] {
        assert_eq!(
            run.at(tag).unwrap().metadata().unwrap().type_,
            FileType::File,
            "{}",
            tag
        );
    }
    udev_input_forget(210);
    for gone in [
        "udev/data/c13:274",
        "udev/tags/seat/c13:274",
        "udev/tags/seat0/c13:274",
    ] {
        assert_eq!(run.at(gone).err(), Some(FsError::EntryNotFound), "{}", gone);
    }
}

/// What kind of pointer a device is, as `input_id` decides it: relative
/// axes are a mouse; absolute ones with a left button and no pen are a
/// mouse too (QEMU's `usb-tablet`); a pen makes a tablet and a touch
/// button a touchscreen.
#[test]
fn each_pointer_is_the_kind_udev_says_it_is() {
    let run = a_run();
    let kinds = [
        (211, caps(&[BTN_LEFT], &[0, 1], &[]), "ID_INPUT_MOUSE"),
        (212, caps(&[BTN_LEFT], &[], &[0, 1]), "ID_INPUT_MOUSE"),
        (
            213,
            caps(&[BTN_LEFT, BTN_TOOL_PEN], &[], &[0, 1]),
            "ID_INPUT_TABLET",
        ),
        (
            214,
            caps(&[BTN_TOUCH], &[], &[0, 1]),
            "ID_INPUT_TOUCHSCREEN",
        ),
    ];
    for (id, c, kind) in &kinds {
        udev_input_record(*id, c);
        let rec = run.record(&alloc::format!("c13:{}", 64 + id));
        let props: Vec<&str> = rec
            .lines()
            .filter_map(|l| l.strip_prefix("E:"))
            .filter(|p| p.starts_with("ID_INPUT_"))
            .collect();
        assert_eq!(props, [alloc::format!("{}=1", kind)], "{}: {}", id, rec);
        assert!(rec.starts_with("I:1\nE:ID_INPUT=1\n"), "{}", rec);
        assert!(rec.ends_with("G:seat\nQ:seat\nV:1\n"), "{}", rec);
        udev_input_forget(*id);
    }
}

/// With no `/run` yet (before the boot made it), a served device's record
/// is skipped rather than written somewhere else, and forgetting it is
/// harmless.
#[test]
fn before_the_boot_made_run_there_is_nothing_to_record_into() {
    let _alone = ALONE.lock();
    let was = UDEV_INPUT_DIRS.lock().take();
    udev_input_record(215, &caps(&[KEY_ESC], &[], &[]));
    udev_input_forget(215);
    assert!(UDEV_INPUT_DIRS.lock().is_none());
    *UDEV_INPUT_DIRS.lock() = was;
}
