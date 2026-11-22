//! Why a driver service for a device the kernel left to userspace did not
//! start, said so that a person reading the boot can tell "this machine
//! has no such device" from "the kernel drives it itself" from "the
//! machine has one and nobody took it".
//!
//! The image carries a service for each of several devices of a kind
//! (`unvme`, `unvme2`, four `ublk`, four `uinput`...), and on a given
//! machine most of them have nothing to drive. One line per service, each
//! saying "the kernel left no ... to userspace", read on a real PC as if
//! the disks, the network card and USB had not been found. What is on the
//! bus is in `/sys/bus/pci/devices`, from the same scan the kernel's
//! drivers were matched against, so the answer is there.

use std::fs;

/// One PCI function, as `/sys/bus/pci/devices/<bdf>` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pci {
    pub vendor: u16,
    pub device: u16,
    /// Base class, subclass and programming interface: `0x010802`.
    pub class: u32,
}

/// Every PCI function the kernel saw, or none where `/sys` has no bus.
pub fn bus() -> Vec<Pci> {
    let Ok(dir) = fs::read_dir("/sys/bus/pci/devices") else {
        return Vec::new();
    };
    let num = |path: std::path::PathBuf| -> Option<u32> {
        let text = fs::read_to_string(path).ok()?;
        u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
    };
    let mut bus: Vec<Pci> = dir
        .flatten()
        .filter_map(|entry| {
            let at = entry.path();
            Some(Pci {
                vendor: num(at.join("vendor"))? as u16,
                device: num(at.join("device"))? as u16,
                class: num(at.join("class"))?,
            })
        })
        .collect();
    bus.sort_by_key(|p| (p.class, p.vendor, p.device));
    bus
}

/// Which PCI functions are one of a kind.
type Is = fn(&Pci) -> bool;

/// The kinds of `/proc/udrivers`, with what each is called on the screen
/// and which PCI functions are one.
fn kind_of(kind: &str) -> Option<(&'static str, Is)> {
    Some(match kind {
        "nvme" => ("NVMe controller", |p| p.class >> 8 == 0x0108),
        // What the kernel hands over (`drivers/src/pci_disks.rs`): the AHCI
        // programming interface, as a SATA, IDE or RAID controller.
        "ahci" => ("AHCI (SATA) controller", |p| {
            p.class & 0xff == 0x01 && matches!(p.class >> 8, 0x0106 | 0x0101 | 0x0104)
        }),
        "xhci" => ("xHCI (USB 3) controller", |p| p.class == 0x0c0330),
        "e1000e" => ("Intel Ethernet card", |p| {
            p.vendor == 0x8086 && p.class >> 8 == 0x0200
        }),
        "virtio-blk" => ("virtio disk", |p| {
            p.vendor == 0x1af4 && matches!(p.device, 0x1001 | 0x1042)
        }),
        "virtio-net" => ("virtio network card", |p| {
            p.vendor == 0x1af4 && matches!(p.device, 0x1000 | 0x1041)
        }),
        "virtio-input" => ("virtio input device", |p| {
            p.vendor == 0x1af4 && p.device == 0x1052
        }),
        _ => return None,
    })
}

/// A device a service names (`nvme/2`, `e1000e@0`, `xhci`): its kind and
/// which of that kind it is, counted from 1.
fn split(device: &str) -> (&str, usize) {
    if let Some((kind, nth)) = device.split_once('/') {
        return (kind, nth.parse().unwrap_or(1));
    }
    if let Some((kind, index)) = device.split_once('@') {
        return (kind, index.parse::<usize>().map_or(1, |i| i + 1));
    }
    (device, 1)
}

/// What became of a service's device.
#[derive(Debug, PartialEq, Eq)]
pub enum Why {
    /// The machine has fewer than the service's number of them.
    Absent,
    /// The machine has it and the kernel drives it itself: the text says
    /// why and how to hand it over.
    KernelDrives(String),
    /// The machine has it and the kernel left it to nobody.
    Missed(String),
}

fn ids(of: &[&Pci]) -> String {
    of.iter()
        .map(|p| format!("{:04x}:{:04x}", p.vendor, p.device))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why the service for `device` was skipped, on a machine whose bus is
/// `bus`.
pub fn why(device: &str, bus: &[Pci]) -> Why {
    // `virtio-blk|virtio-blk/1`: any of them, and the first names the kind.
    let device = device.split('|').next().unwrap_or(device);
    let (kind, nth) = split(device);
    // No PCI bus to look at (aarch64's devices are on MMIO), or a kind
    // this does not know: what the kernel's list says, and no guess.
    let Some((what, is)) = kind_of(kind).filter(|_| !bus.is_empty()) else {
        return Why::Missed(format!("the kernel left no '{device}' to userspace"));
    };
    let found: Vec<&Pci> = bus.iter().filter(|p| is(p)).collect();
    if found.len() < nth {
        // A SATA controller in RAID mode (Intel RST) has no AHCI
        // programming interface, and nothing here drives it.
        let raid: Vec<&Pci> = bus
            .iter()
            .filter(|p| p.class >> 8 == 0x0104 && p.class & 0xff != 0x01)
            .collect();
        if kind == "ahci" && !raid.is_empty() {
            return Why::Missed(format!(
                "the SATA controller ({}) is in RAID mode; set SATA to AHCI in the firmware setup",
                ids(&raid)
            ));
        }
        return Why::Absent;
    }
    match kind {
        "e1000e" => Why::KernelDrives(format!(
            "the kernel drives the network card itself ({}): the userspace e1000e driver does not take it, or the boot said 'unetcard=kernel'",
            ids(&found)
        )),
        "xhci" => Why::KernelDrives(format!(
            "the kernel drives USB itself ({}); 'udriver=xhci@0' on its cmdline hands it to userspace",
            ids(&found)
        )),
        _ => Why::Missed(format!(
            "the machine has {} {what}(s) ({}) but the kernel left no '{device}' to userspace",
            found.len(),
            ids(&found)
        )),
    }
}

/// The lines the boot log gets for the services skipped, each `(name,
/// device)`: one for every service whose device is there, and one for all
/// those with nothing to drive.
pub fn lines(skipped: &[(String, String)], bus: &[Pci]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut absent = Vec::new();
    for (name, device) in skipped {
        match why(device, bus) {
            Why::Absent => absent.push(name.as_str()),
            Why::KernelDrives(text) => lines.push(format!("{name}: not started: {text}")),
            Why::Missed(text) => lines.push(format!(
                "{name}: skipped: {text}; 'udriver={device}' on its cmdline asks for it"
            )),
        }
    }
    if !absent.is_empty() {
        lines.push(format!(
            "not started, this machine has no device for them: {}",
            absent.join(" ")
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pci(vendor: u16, device: u16, class: u32) -> Pci {
        Pci {
            vendor,
            device,
            class,
        }
    }

    /// A desktop like the one this was written for: one NVMe, one SATA
    /// controller, an I219-V, an xHCI, no virtio.
    fn pc() -> Vec<Pci> {
        vec![
            pci(0x144d, 0xa808, 0x010802),
            pci(0x8086, 0xa352, 0x010601),
            pci(0x8086, 0x15bc, 0x020000),
            pci(0x8086, 0xa36d, 0x0c0330),
        ]
    }

    #[test]
    fn the_second_of_a_kind_on_a_machine_with_one_is_absent() {
        assert_eq!(why("nvme/2", &pc()), Why::Absent);
        assert_eq!(why("ahci/2", &pc()), Why::Absent);
        assert_eq!(why("virtio-blk|virtio-blk/1", &pc()), Why::Absent);
        assert_eq!(why("virtio-net@0", &pc()), Why::Absent);
    }

    #[test]
    fn a_card_the_kernel_keeps_is_named_with_its_ids() {
        let Why::KernelDrives(text) = why("e1000e@0", &pc()) else {
            panic!("not the kernel's");
        };
        assert!(text.contains("8086:15bc"), "{text}");
        assert!(matches!(why("xhci", &pc()), Why::KernelDrives(_)));
    }

    #[test]
    fn a_device_present_and_left_to_nobody_says_so() {
        let mut bus = pc();
        bus.push(pci(0x15b7, 0x5011, 0x010802));
        let Why::Missed(text) = why("nvme/2", &bus) else {
            panic!("not reported");
        };
        assert!(
            text.contains("2 NVMe") && text.contains("15b7:5011"),
            "{text}"
        );
    }

    #[test]
    fn a_sata_controller_in_raid_mode_is_named() {
        let bus = vec![pci(0x8086, 0x2822, 0x010400)];
        let Why::Missed(text) = why("ahci", &bus) else {
            panic!("not reported");
        };
        assert!(text.contains("RAID"), "{text}");
    }

    #[test]
    fn the_absent_ones_share_one_line() {
        let skipped: Vec<(String, String)> = [
            ("enet", "e1000e@0"),
            ("uahci2", "ahci/2"),
            ("ublk", "virtio-blk"),
            ("unvme2", "nvme/2"),
        ]
        .iter()
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .collect();
        let lines = lines(&skipped, &pc());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("enet: not started: the kernel drives"));
        assert_eq!(
            lines[1],
            "not started, this machine has no device for them: uahci2 ublk unvme2"
        );
    }

    #[test]
    fn with_no_pci_bus_nothing_is_guessed() {
        assert!(matches!(why("virtio-blk/2", &[]), Why::Missed(_)));
    }

    #[test]
    fn indexes_and_numbers_count_from_one() {
        assert_eq!(split("nvme/2"), ("nvme", 2));
        assert_eq!(split("e1000e@0"), ("e1000e", 1));
        assert_eq!(split("xhci"), ("xhci", 1));
    }
}
