//! `udrv-nvidia bridge-check VVVV DDDD`: what the driver does above its
//! GPU, on any PCI function behind a bridge, through the ports as the
//! driver reaches them. On QEMU: an `e1000e` on a `pcie-root-port`, with
//! `udriver=pci-8086-10d3@0`.
//!
//! The walk to the root port (`NvidiaGpu::find_parent_bridge`), the two
//! Device Controls it compares and the one it writes (`normalize_mps`),
//! VGA routing off and back (`set_path_vga_routing`), a bus reset and the
//! BARs after it (`sbr_recover`). The driver itself is not run.
//!
//! A second machine for the reset the kernel refuses: the function on a
//! `pci-bridge` with another device beside it.
use std::process::exit;
use std::thread::sleep;
use std::time::Duration;

use pci::Location;
use zcore_drivers::builder::IoMapper;
use zcore_drivers::bus::pci::cfgwrite::cfg_write16;
use zcore_drivers::bus::pci::{PortOpsImpl, PCI_ACCESS};

use crate::gpu::{self, Gpu};
use crate::{fail, NAME};

/// A PCI-PCI bridge's bus numbers and its Bridge Control, with the bits
/// of it this touches.
const BUSES: u16 = 0x18;
const BRIDGE_CONTROL: u16 = 0x3e;
const SERR_ENABLE: u16 = 1 << 1;
const VGA_ENABLE: u16 = 1 << 3;
const BUS_RESET: u16 = 1 << 6;
/// The command register, the Interrupt Line (with the Interrupt Pin above
/// it), and Device Control in a PCI Express capability.
const COMMAND: u16 = 0x04;
const INTERRUPT_LINE: u16 = 0x3c;
const DEVICE_CONTROL: u16 = 0x08;

// SAFETY, for the five below: every port they reach is answered by `ports`.
fn read8(loc: Location, at: u16) -> u8 {
    unsafe { PCI_ACCESS.read8(&PortOpsImpl, loc, at) }
}

fn read16(loc: Location, at: u16) -> u16 {
    unsafe { PCI_ACCESS.read16(&PortOpsImpl, loc, at) }
}

fn read32(loc: Location, at: u16) -> u32 {
    unsafe { PCI_ACCESS.read32(&PortOpsImpl, loc, at) }
}

/// A word written as the driver writes one: the dword read, and written
/// back with the word in it.
fn write16(loc: Location, at: u16, value: u16) {
    unsafe { cfg_write16(&PortOpsImpl, PCI_ACCESS, loc, at, value) }
}

fn write32(loc: Location, at: u16, value: u32) {
    unsafe { PCI_ACCESS.write32(&PortOpsImpl, loc, at, value) }
}

fn name(loc: Location) -> String {
    format!("{:02x}:{:02x}.{}", loc.bus, loc.device, loc.function)
}

fn place(loc: Location) -> (u8, u8, u8) {
    (loc.bus, loc.device, loc.function)
}

/// Every PCI-PCI bridge on `bus`.
fn bridges(bus: u8) -> Vec<Location> {
    let mut found = Vec::new();
    for device in 0..32 {
        for function in 0..8 {
            let loc = Location {
                bus,
                device,
                function,
            };
            if read16(loc, 0x00) != 0xffff && read8(loc, 0x0e) & 0x7f == 0x01 {
                found.push(loc);
            }
        }
    }
    found
}

/// The bridge whose secondary bus is `of`'s, by the driver's walk: every
/// function of every bus up to its own. None for a function on bus 0.
fn parent_bridge(of: Location) -> Option<Location> {
    if of.bus == 0 {
        return None;
    }
    (0..=of.bus)
        .flat_map(bridges)
        .find(|&bridge| read8(bridge, BUSES + 1) == of.bus)
}

/// Where the PCI Express capability of `loc` is.
fn pcie_capability(loc: Location) -> Option<u16> {
    if read16(loc, 0x06) & (1 << 4) == 0 {
        return None;
    }
    let mut at = read8(loc, 0x34) as u16;
    for _ in 0..48 {
        if at == 0 {
            break;
        }
        if read8(loc, at) == 0x10 {
            return Some(at);
        }
        at = read8(loc, at + 1) as u16;
    }
    None
}

pub fn bridge_check(vendor: u16, device: u16) {
    let ok = |what: &str, ok: bool| {
        println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            exit(1);
        }
    };
    let gpus = gpu::claim_all(|index| gpu::Which::Pci {
        vendor,
        device,
        index,
    })
    .unwrap_or_else(|e| fail(e));
    println!("{NAME}: {} function(s) claimed", gpus.len());
    let host = Location {
        bus: 0,
        device: 0,
        function: 0,
    };
    let id = read32(host, 0x00);
    println!("{NAME}: {} is {id:#010x}", name(host));
    ok(
        "a function this process does not hold is read, by the kernel",
        id != u32::MAX && gpus[0].pci.config_read_at(place(host), 0, 4) == Ok(id),
    );
    let mut behind = 0;
    for gpu in &gpus {
        match parent_bridge(gpu.loc) {
            Some(bridge) => {
                behind += 1;
                above_one(gpu, bridge, &ok);
            }
            None => println!("{NAME}: {} has no bridge above it", name(gpu.loc)),
        }
    }
    let above_any = |bridge: Location| {
        let (secondary, subordinate) = (read8(bridge, BUSES + 1), read8(bridge, BUSES + 2));
        gpus.iter()
            .any(|g| secondary <= g.loc.bus && g.loc.bus <= subordinate)
    };
    match bridges(0).into_iter().find(|&bridge| !above_any(bridge)) {
        Some(other) => {
            let control = read16(other, BRIDGE_CONTROL);
            write16(other, BRIDGE_CONTROL, control ^ VGA_ENABLE);
            println!("{NAME}: {} is a bridge above none of them", name(other));
            ok(
                "a bridge that is not above the function is not written",
                read16(other, BRIDGE_CONTROL) == control,
            );
        }
        None => println!("{NAME}: no bridge but those above: a write to another not checked"),
    }
    if behind == 0 {
        fail("bridge-check FAILED: no function behind a bridge (QEMU: -device pcie-root-port,id=rp1,chassis=1 -device e1000e,bus=rp1)".into());
    }
    println!("{NAME}: bridge-check: ok");
}

/// What the driver does at `bridge`, the one above `gpu`.
fn above_one(gpu: &Gpu, bridge: Location, ok: &dyn Fn(&str, bool)) {
    let here = gpu.loc;
    let (secondary, subordinate) = (read8(bridge, BUSES + 1), read8(bridge, BUSES + 2));
    println!(
        "{NAME}: {} is behind the bridge at {} (buses {secondary:02x} to {subordinate:02x})",
        name(here),
        name(bridge)
    );
    ok(
        "the walk finds the bridge above, as the kernel reads it",
        gpu.pci.config_read_at(place(bridge), BUSES as usize, 4) == Ok(read32(bridge, BUSES)),
    );

    // The two Max Payload Sizes the driver compares, and a write of the
    // function's Device Control, where it clamps its own.
    match (pcie_capability(here), pcie_capability(bridge)) {
        (Some(mine), Some(ports)) => {
            let control = read16(here, mine + DEVICE_CONTROL);
            let above = read16(bridge, ports + DEVICE_CONTROL);
            println!(
                "{NAME}: Max Payload Size {} here, {} at the port",
                128 << (control >> 5 & 7),
                128 << (above >> 5 & 7)
            );
            // The error reporting enables, which real hardware takes.
            // QEMU's e1000e keeps the whole register read only.
            write16(here, mine + DEVICE_CONTROL, control ^ 0xf);
            let changed = read16(here, mine + DEVICE_CONTROL);
            write16(here, mine + DEVICE_CONTROL, control);
            if changed == control {
                println!("{NAME}: this function's Device Control takes no write: not checked");
            } else {
                ok(
                    "the function's Device Control is written through the ports",
                    read16(here, mine + DEVICE_CONTROL) == control,
                );
            }
        }
        _ => println!("{NAME}: not PCI Express on both ends: Device Control not checked"),
    }
    // The same path with a register every function takes: the Interrupt
    // Line, a byte the hardware keeps and never reads.
    let line = read16(here, INTERRUPT_LINE);
    write16(here, INTERRUPT_LINE, line ^ 0x00ff);
    let changed = read16(here, INTERRUPT_LINE);
    write16(here, INTERRUPT_LINE, line);
    ok(
        "a register of the function is written through the ports, and put back",
        changed == line ^ 0x00ff && read16(here, INTERRUPT_LINE) == line,
    );

    let control = read16(bridge, BRIDGE_CONTROL);
    write16(bridge, BRIDGE_CONTROL, control ^ VGA_ENABLE);
    ok(
        "VGA routing of the bridge above is switched",
        read16(bridge, BRIDGE_CONTROL) == control ^ VGA_ENABLE,
    );
    write16(bridge, BRIDGE_CONTROL, control);
    ok("...and put back", read16(bridge, BRIDGE_CONTROL) == control);
    write16(bridge, BRIDGE_CONTROL, control ^ SERR_ENABLE);
    ok(
        "no other bit of its Bridge Control is the driver's to write",
        read16(bridge, BRIDGE_CONTROL) == control,
    );
    let buses = read32(bridge, BUSES);
    write32(bridge, BUSES, buses ^ 0x00ff_0000);
    ok(
        "nor any other register of the bridge",
        read32(bridge, BUSES) == buses,
    );

    // The bus reset, and what the driver does after one: its BARs, then
    // its command register. The bridge leads to this device alone when
    // nothing else answers on any bus behind it (a port may have spare bus
    // numbers, all empty).
    let alone = (secondary..=subordinate).all(|bus| {
        (0..32)
            .filter(|&device| (bus, device) != (here.bus, here.device))
            .all(|device| {
                let loc = Location {
                    bus,
                    device,
                    function: 0,
                };
                read16(loc, 0x00) == 0xffff
            })
    });
    // The BARs as the device holds them (the ports keep their own).
    let held = || -> Vec<u32> {
        (0..6)
            .map(|i| gpu.pci.config_read(0x10 + 4 * i, 4).unwrap_or(u32::MAX))
            .collect()
    };
    let before = held();
    // The registers the kernel keeps: each memory BAR, both halves of a
    // 64-bit one.
    let mut kernels = [false; 6];
    for bar in &gpu.bars {
        kernels[bar.index] = true;
        if before[bar.index] & 0b110 == 0b100 && bar.index < 5 {
            kernels[bar.index + 1] = true;
        }
    }
    let memory =
        |all: &[u32]| -> Vec<u32> { (0..6).filter(|&i| kernels[i]).map(|i| all[i]).collect() };
    let command = read16(here, COMMAND);
    let first = gpu
        .bars
        .first()
        .copied()
        .unwrap_or_else(|| fail("no BAR".into()));
    let mapped = gpu
        .mapper()
        .query_or_map(first.phys, 0x1000)
        .unwrap_or_else(|| fail("BAR not mapped".into()));
    // SAFETY: inside the first BAR, mapped above.
    let peek = || unsafe { ((mapped + 8) as *const u32).read_volatile() };
    let was = peek();
    // As `sbr_recover`: the bit on, 5 ms, the bit off, 250 ms. The kernel
    // does the whole pulse at the first write, so a driver that dies
    // before its second leaves no bus in reset.
    write16(bridge, BRIDGE_CONTROL, control | BUS_RESET);
    ok(
        "the bus is out of reset when the write that asked for one returns",
        read16(bridge, BRIDGE_CONTROL) == control,
    );
    sleep(Duration::from_millis(5));
    write16(bridge, BRIDGE_CONTROL, control);
    ok(
        "...and the driver's own write to let it go changes nothing",
        read16(bridge, BRIDGE_CONTROL) == control,
    );
    sleep(Duration::from_millis(250));
    let cleared = held();
    println!("{NAME}: BARs {before:x?} before the reset, {cleared:x?} after it");
    // A reset that reached the device cleared its BARs.
    let reset = memory(&cleared) != memory(&before);
    ok(
        "the bus is reset under a bridge that leads to this device alone, and no other",
        reset == alone,
    );
    if !reset {
        println!(
            "{NAME}: {} leads to more than this device: not reset",
            name(bridge)
        );
        return;
    }
    for (i, &bar) in before.iter().enumerate() {
        write32(here, 0x10 + 4 * i as u16, bar);
    }
    write16(here, COMMAND, command);
    ok(
        "the kernel wrote them again, at the driver's first BAR write",
        memory(&held()) == memory(&before),
    );
    ok(
        "...and the command register is the driver's to put back",
        read16(here, COMMAND) == command,
    );
    let is = peek();
    println!("{NAME}: the function's memory read {was:#x} before, {is:#x} now");
    ok("its memory answers again", is == was || is != u32::MAX);
}
