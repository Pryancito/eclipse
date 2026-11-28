//! An input device that a process serves (`/dev/input/eventN`, char
//! 13:64+N), as the rest of the system knows the kernel's own:
//!
//! - its keys reach the text consoles, as a kernel keyboard's do: the
//!   kernel opens the node itself, a reader like any other, and gives what
//!   it reads to the console's key handler ([`attach`]);
//! - it is in `/sys/class/input` and in udev's database under `/run/udev`,
//!   with the `ID_INPUT_*` properties its capabilities give, which is how
//!   libinput finds an input device and tells what it is ([`caps`],
//!   [`ids`]).
//!
//! Both last as long as the node: when its server goes the kernel's read
//! ends, and the device leaves `/sys` and the database.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use lock::Mutex;
use rcore_fs::vfs::FsError;
use zcore_drivers::prelude::{InputEvent, InputEventType};

use super::{ServedNode, ServedOpen};
use crate::sync::{wait_for_event, Event};

/// evdev's major, and the minor of `event0`.
const EVDEV_MAJOR: usize = 13;
const EVENT_MINOR_BASE: usize = 64;
/// `O_RDONLY | O_NONBLOCK`.
const OPEN_FLAGS: usize = 0o4000;
/// `struct input_event` on a 64-bit machine: a `timeval`, then the type,
/// the code and the value.
const EVENT_LEN: usize = 24;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
/// How many bytes of each bitmap are asked for: `KEY_MAX`, `REL_MAX`,
/// `ABS_MAX` as bits.
const KEY_BYTES: usize = 0x300 / 8;
const REL_BYTES: usize = 0x10 / 8;
const ABS_BYTES: usize = 0x40 / 8;

/// What a served input device can report: its evdev bitmaps.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Caps {
    pub key: Vec<u8>,
    pub rel: Vec<u8>,
    pub abs: Vec<u8>,
}

fn bit(map: &[u8], code: u16) -> bool {
    map.get(code as usize / 8)
        .is_some_and(|b| b & (1 << (code % 8)) != 0)
}

impl Caps {
    pub fn has_key(&self, code: u16) -> bool {
        bit(&self.key, code)
    }
    pub fn has_rel(&self, code: u16) -> bool {
        bit(&self.rel, code)
    }
    pub fn has_abs(&self, code: u16) -> bool {
        bit(&self.abs, code)
    }
}

lazy_static::lazy_static! {
    /// The served input devices, by their `N` of `eventN`.
    static ref SERVED: Mutex<BTreeMap<usize, Caps>> = Mutex::new(BTreeMap::new());
}

/// The `N` of each served `/dev/input/eventN`.
pub fn ids() -> Vec<usize> {
    SERVED.lock().keys().copied().collect()
}

/// What served `eventN` can report, when it is one.
pub fn caps(id: usize) -> Option<Caps> {
    SERVED.lock().get(&id).cloned()
}

/// The `N` of `eventN` for a node at `name` with device number `rdev`, when
/// it is an evdev node.
pub(super) fn event_id(name: &str, rdev: usize) -> Option<usize> {
    let n: usize = name.strip_prefix("input/event")?.parse().ok()?;
    let (major, minor) = ((rdev >> 8) & 0xfff, rdev & 0xff);
    (major == EVDEV_MAJOR && minor == EVENT_MINOR_BASE + n).then_some(n)
}

/// `EVIOCGBIT(ev, len)`.
fn eviocgbit(ev: u16, len: usize) -> u32 {
    (2 << 30) | ((len as u32) << 16) | ((b'E' as u32) << 8) | (0x20 + ev as u32)
}

/// The events in `bytes`, whole ones, as the console's handler takes them.
fn events(bytes: &[u8]) -> impl Iterator<Item = InputEvent> + '_ {
    bytes.as_chunks::<EVENT_LEN>().0.iter().filter_map(|e| {
        let kind = u16::from_ne_bytes([e[16], e[17]]);
        let code = u16::from_ne_bytes([e[18], e[19]]);
        let value = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
        let event_type = match kind {
            EV_SYN => InputEventType::Syn,
            EV_KEY => InputEventType::Key,
            _ => return None,
        };
        Some(InputEvent {
            event_type,
            code,
            value,
        })
    })
}

/// A node a process put in `/dev`: when it is an evdev node, the kernel
/// opens it, records what it can report (in `/sys` and udev's database)
/// and gives its keys to the consoles until the server goes. Anything else
/// is left alone.
pub fn attach(node: &Arc<ServedNode>) {
    let Some(id) = event_id(node.name(), node.0.rdev) else {
        return;
    };
    let node = node.clone();
    // Not here: the server is still in the call that put the node there,
    // and answers nothing until it is back.
    kernel_hal::thread::spawn(async move {
        let Ok(open) = node.open_client(OPEN_FLAGS) else {
            warn!("served input: /dev/{} would not open", node.name());
            return;
        };
        let Some(served) = open.as_any_ref().downcast_ref::<ServedOpen>() else {
            return;
        };
        let ask = |ev: u16, len: usize| {
            let mut map = alloc::vec![0u8; len];
            let n = served.ioctl_read(eviocgbit(ev, len), &mut map).unwrap_or(0);
            map.truncate(n);
            map
        };
        let caps = Caps {
            key: ask(EV_KEY, KEY_BYTES),
            rel: ask(EV_REL, REL_BYTES),
            abs: ask(EV_ABS, ABS_BYTES),
        };
        crate::fs::udev_input_record(id, &caps);
        SERVED.lock().insert(id, caps);
        info!("served input: /dev/{} reaches the consoles", node.name());
        let bus = served.bus();
        let mut buf = alloc::vec![0u8; EVENT_LEN * 64];
        loop {
            match open.read_at(0, &mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    for event in events(&buf[..n]) {
                        crate::fs::stdio::console_key(&event);
                    }
                    continue;
                }
                Err(FsError::Again) => {}
                Err(_) => break,
            }
            let mask = Event::READABLE | Event::ERROR | Event::CLOSED;
            match wait_for_event(bus.clone(), mask).await {
                Ok(seen) if !seen.intersects(Event::ERROR | Event::CLOSED) => {}
                _ => break,
            }
        }
        SERVED.lock().remove(&id);
        crate::fs::udev_input_forget(id);
        info!("served input: /dev/{} is gone", node.name());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_evdev_node_is_one_named_and_numbered_as_evdev() {
        let rdev = |major: usize, minor: usize| rcore_fs::vfs::make_rdev(major, minor);
        assert_eq!(event_id("input/event3", rdev(13, 67)), Some(3));
        assert_eq!(event_id("input/event3", rdev(13, 64)), None);
        assert_eq!(event_id("dsp", rdev(14, 3)), None);
        assert_eq!(event_id("input/mice", rdev(13, 63)), None);
    }

    #[test]
    fn events_come_whole_and_only_keys_and_syncs() {
        let mut bytes = Vec::new();
        for (kind, code, value) in [(1u16, 30u16, 1i32), (2, 0, 5), (0, 0, 0)] {
            bytes.extend_from_slice(&[0u8; 16]);
            bytes.extend_from_slice(&kind.to_ne_bytes());
            bytes.extend_from_slice(&code.to_ne_bytes());
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        bytes.extend_from_slice(&[1, 2, 3]);
        let got: Vec<_> = events(&bytes).map(|e| (e.code, e.value)).collect();
        assert_eq!(got, [(30, 1), (0, 0)]);
    }

    #[test]
    fn a_bitmap_says_each_code() {
        let caps = Caps {
            key: alloc::vec![0b10, 0, 0, 0],
            rel: alloc::vec![0b11],
            abs: Vec::new(),
        };
        assert!(caps.has_key(1) && !caps.has_key(0) && !caps.has_key(200));
        assert!(caps.has_rel(0) && caps.has_rel(1) && !caps.has_abs(0));
    }

    #[test]
    fn eviocgbit_is_the_read_command_linux_numbers() {
        assert_eq!(eviocgbit(EV_KEY, 96), 0x8060_4521);
        assert_eq!(eviocgbit(0, 4), 0x8004_4520);
    }
}
