use super::*;
use alloc::collections::BTreeMap;
use lock::Mutex;

const CONFIG_ADDRESS: u32 = 0x0CF8;
const CONFIG_DATA: u32 = 0x0CFC;
const IO: CSpaceAccessMethod = CSpaceAccessMethod::IO;

/// A bus: the first 256 bytes of every function on it behind 0xCF8/0xCFC,
/// all ones where there is none, and every write that reached a function.
struct Bus {
    functions: Mutex<BTreeMap<(u8, u8, u8), [u32; 64]>>,
    latched: Mutex<u32>,
    writes: Mutex<alloc::vec::Vec<((u8, u8, u8), u8, u32)>>,
}

fn at(l: Location) -> (u8, u8, u8) {
    (l.bus, l.device, l.function)
}

fn loc(bus: u8, device: u8, function: u8) -> Location {
    Location {
        bus,
        device,
        function,
    }
}

impl Bus {
    fn new() -> Self {
        Bus {
            functions: Mutex::new(BTreeMap::new()),
            latched: Mutex::new(0),
            writes: Mutex::new(alloc::vec::Vec::new()),
        }
    }

    /// A PCI-PCI bridge at `l` with `secondary..=subordinate` behind it and
    /// `control` in its Bridge Control.
    fn bridge(&self, l: Location, secondary: u8, subordinate: u8, control: u16) {
        let mut s = [0u32; 64];
        s[0] = 0x2030_8086;
        s[0x0C / 4] = 0x0001_0000;
        s[0x18 / 4] = (subordinate as u32) << 16 | (secondary as u32) << 8 | l.bus as u32;
        s[0x3C / 4] = (control as u32) << 16 | 0x01ff;
        self.functions.lock().insert(at(l), s);
    }

    /// An endpoint at `l`: a GPU with a 32-bit BAR 0 and a 64-bit BAR 1,
    /// both cleared, as a bus reset leaves them.
    fn endpoint(&self, l: Location) {
        let mut s = [0u32; 64];
        s[0] = 0x1f06_10de;
        s[0x14 / 4] = 0x0000_000c;
        self.functions.lock().insert(at(l), s);
    }

    fn dword(&self, l: Location, off: usize) -> u32 {
        self.functions.lock()[&at(l)][off / 4]
    }

    fn control(&self, l: Location) -> u16 {
        (self.dword(l, 0x3C) >> 16) as u16
    }

    fn named(&self) -> ((u8, u8, u8), usize) {
        let a = *self.latched.lock();
        (
            ((a >> 16) as u8, (a >> 11 & 0x1f) as u8, (a >> 8 & 7) as u8),
            (a & 0xFC) as usize,
        )
    }
}

impl PortOps for Bus {
    unsafe fn read8(&self, _: u16) -> u8 {
        unreachable!()
    }
    unsafe fn read16(&self, _: u16) -> u16 {
        unreachable!()
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        assert_eq!(port, CONFIG_DATA);
        let (place, off) = self.named();
        match self.functions.lock().get(&place) {
            Some(s) => s[off / 4],
            None => u32::MAX,
        }
    }
    unsafe fn write8(&self, _: u16, _: u8) {
        unreachable!()
    }
    unsafe fn write16(&self, _: u16, _: u16) {
        unreachable!()
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        if port == CONFIG_ADDRESS {
            *self.latched.lock() = val;
            return;
        }
        let (place, off) = self.named();
        if let Some(s) = self.functions.lock().get_mut(&place) {
            // A memory BAR keeps its type bits whatever is written: the
            // endpoint's BAR 0 and the lower half of its BAR 1 (0x18 is
            // the upper half, every bit of it address).
            s[off / 4] = if off == 0x10 || off == 0x14 {
                (val & !0xf) | (s[off / 4] & 0xf)
            } else {
                val
            };
            self.writes.lock().push((place, off as u8, val));
        }
    }
}

const PORT: Location = Location {
    bus: 0x16,
    device: 0,
    function: 0,
};
const GPU: Location = Location {
    bus: 0x17,
    device: 0,
    function: 0,
};

/// The reference machine: a root port at 16:00.0 with only bus 0x17 behind
/// it, and on that bus the GPU with its audio function.
fn machine(control: u16) -> Bus {
    let bus = Bus::new();
    bus.bridge(PORT, 0x17, 0x17, control);
    bus.endpoint(GPU);
    bus.endpoint(loc(0x17, 0, 1));
    bus
}

#[test]
fn only_a_pci_pci_bridge_has_buses_behind_it() {
    let bus = machine(0);
    assert_eq!(unsafe { buses_behind(&bus, IO, PORT) }, Some((0x17, 0x17)));
    assert_eq!(
        unsafe { buses_behind(&bus, IO, GPU) },
        None,
        "an endpoint is not a bridge"
    );
    assert_eq!(
        unsafe { buses_behind(&bus, IO, loc(3, 0, 0)) },
        None,
        "nothing there"
    );
    // A multi-function bridge says so in the same register.
    bus.functions.lock().get_mut(&at(PORT)).unwrap()[0x0C / 4] = 0x0081_0000;
    assert_eq!(unsafe { buses_behind(&bus, IO, PORT) }, Some((0x17, 0x17)));
}

#[test]
fn vga_routing_is_cleared_and_put_back_and_nothing_else_changes() {
    // VGA Enable and VGA 16-bit decode set, as firmware leaves a console
    // card's port, with SERR forwarding.
    let bus = machine(0x001A);
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0x001A & !VGA_ENABLE) },
        Ok(0x0012)
    );
    assert_eq!(bus.control(PORT), 0x0012);
    assert_eq!(
        bus.dword(PORT, 0x3C) & 0xffff,
        0x01ff,
        "the interrupt line and pin beside it are as they were"
    );
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0x001A) },
        Ok(0x001A)
    );
    assert_eq!(bus.control(PORT), 0x001A);
}

#[test]
fn bits_a_driver_has_no_business_with_stay_as_the_bridge_has_them() {
    let bus = machine(0x0002);
    // Asked for everything: only the two bits are taken.
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0xFFFF) },
        Ok(0x0002 | VGA_ENABLE | BUS_RESET)
    );
    // Asked for nothing: the two bits go, the rest stays.
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0) },
        Ok(0x0002)
    );
}

#[test]
fn a_write_that_changes_nothing_does_not_reach_the_bridge() {
    let bus = machine(0x0010);
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0x0010) },
        Ok(0x0010)
    );
    assert!(bus.writes.lock().is_empty());
}

#[test]
fn only_a_bridge_above_the_function_is_written() {
    let bus = machine(VGA_ENABLE);
    // The other card's port: a bridge, but not above this GPU.
    let other = loc(0x64, 0, 0);
    bus.bridge(other, 0x65, 0x65, VGA_ENABLE);
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, other, GPU, 0) },
        Err(Refused::NotAbove)
    );
    assert_eq!(bus.control(other), VGA_ENABLE);
    // The GPU itself, its audio, and an empty slot: not bridges.
    for not_a_bridge in [GPU, loc(0x17, 0, 1), loc(0, 0x1f, 7)] {
        assert_eq!(
            unsafe { bridge_control_write(&bus, IO, not_a_bridge, GPU, 0) },
            Err(Refused::NotABridge)
        );
    }
    assert!(bus.writes.lock().is_empty());
}

#[test]
fn the_bus_is_reset_and_released_under_a_port_that_leads_only_to_the_device() {
    let bus = machine(0x0002);
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0x0002 | BUS_RESET) },
        Ok(0x0002 | BUS_RESET)
    );
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0x0002) },
        Ok(0x0002)
    );
    assert_eq!(bus.writes.lock().len(), 2);
}

#[test]
fn a_reset_asked_by_a_process_is_a_pulse_the_kernel_ends() {
    let bus = machine(0x0002);
    let held = core::cell::Cell::new(None);
    let taken = core::cell::Cell::new(0);
    let answer = unsafe {
        bridge_control_pulsed(
            &bus,
            IO,
            PORT,
            GPU,
            0x0002 | BUS_RESET,
            || taken.set(taken.get() + 1),
            || held.set(Some(bus.control(PORT))),
        )
    };
    assert_eq!(
        held.get(),
        Some(0x0002 | BUS_RESET),
        "in reset while it is held"
    );
    assert_eq!(answer, Ok(0x0002), "and out of it when the call returns");
    assert_eq!(bus.control(PORT), 0x0002);
    assert_eq!(taken.get(), 2, "one access at a time, not across the hold");
    // The write the driver then takes the bit off with changes nothing.
    let writes = bus.writes.lock().len();
    assert_eq!(
        unsafe { bridge_control_pulsed(&bus, IO, PORT, GPU, 0x0002, || (), || unreachable!()) },
        Ok(0x0002)
    );
    assert_eq!(bus.writes.lock().len(), writes);
    // A refused reset is not held, and VGA routing alone is no pulse.
    bus.endpoint(loc(0x17, 3, 0));
    assert_eq!(
        unsafe { bridge_control_pulsed(&bus, IO, PORT, GPU, BUS_RESET, || (), || unreachable!()) },
        Err(Refused::Shared)
    );
    assert_eq!(
        unsafe { bridge_control_pulsed(&bus, IO, PORT, GPU, VGA_ENABLE, || (), || unreachable!()) },
        Ok(0x0002 | VGA_ENABLE)
    );
}

#[test]
fn spare_bus_numbers_behind_a_port_do_not_stop_the_reset_but_a_device_on_them_does() {
    // Firmware gave the port four bus numbers; the GPU is on the first
    // and nothing answers on the others.
    let bus = Bus::new();
    bus.bridge(PORT, 0x17, 0x1A, 0);
    bus.endpoint(GPU);
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, BUS_RESET) },
        Ok(BUS_RESET)
    );
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, 0) },
        Ok(0)
    );
    // Somebody else's device on one of them: theirs would be reset too.
    bus.endpoint(loc(0x19, 0, 0));
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, PORT, GPU, BUS_RESET) },
        Err(Refused::Shared)
    );
    // A port whose first bus is not the device's: something is between.
    let far = Bus::new();
    far.bridge(PORT, 0x16, 0x17, 0);
    far.endpoint(GPU);
    assert_eq!(
        unsafe { bridge_control_write(&far, IO, PORT, GPU, BUS_RESET) },
        Err(Refused::Shared)
    );
}

#[test]
fn a_bridge_shared_with_other_devices_is_not_reset() {
    // A switch: its upstream port leads to several buses.
    let bus = machine(0);
    let upstream = loc(0x10, 0, 0);
    bus.bridge(upstream, 0x11, 0x20, 0);
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, upstream, GPU, BUS_RESET) },
        Err(Refused::Shared)
    );
    // VGA routing on it is still the driver's to change.
    assert_eq!(
        unsafe { bridge_control_write(&bus, IO, upstream, GPU, VGA_ENABLE) },
        Ok(VGA_ENABLE)
    );
    // One bus behind the port, but another device on it beside the GPU.
    let crowded = machine(0);
    crowded.endpoint(loc(0x17, 3, 0));
    assert_eq!(
        unsafe { bridge_control_write(&crowded, IO, PORT, GPU, BUS_RESET) },
        Err(Refused::Shared)
    );
    assert_eq!(crowded.control(PORT), 0);
    // A reset already asserted may always be released.
    let stuck = machine(BUS_RESET);
    stuck.endpoint(loc(0x17, 3, 0));
    assert_eq!(
        unsafe { bridge_control_write(&stuck, IO, PORT, GPU, BUS_RESET) },
        Ok(BUS_RESET),
        "no change asked of the reset bit"
    );
}

#[test]
fn the_bars_a_bus_reset_cleared_are_written_again() {
    let bus = machine(0);
    let mut bars = [0usize; 6];
    bars[0] = 0xb400_0000;
    bars[1] = 0x38_2000_0000;
    unsafe { restore_bars(&bus, IO, GPU, &bars) };
    assert_eq!(bus.dword(GPU, 0x10), 0xb400_0000);
    assert_eq!(
        bus.dword(GPU, 0x14),
        0x2000_000c,
        "the low half, with the type bits the device keeps"
    );
    assert_eq!(
        bus.dword(GPU, 0x18),
        0x38,
        "the upper half of the 64-bit BAR"
    );
    assert_eq!(
        bus.dword(GPU, 0x1C),
        0,
        "a BAR the kernel recorded nothing for"
    );
    // Nothing but the GPU's own BARs was written.
    assert!(bus
        .writes
        .lock()
        .iter()
        .all(|&(place, off, _)| place == at(GPU) && (0x10..0x1C).contains(&off)));
}
