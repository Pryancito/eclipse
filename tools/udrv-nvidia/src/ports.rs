//! The I/O ports the driver reaches for (`drivers_port_read/write`, which
//! `zcore-drivers` calls instead of `in`/`out` when built with its
//! `process` feature). A process may not touch a port, so this answers.
//!
//! What the driver reaches through ports is, almost all of it, the PCI
//! configuration mechanism: an address in 0xCF8, then the dword at
//! 0xCFC. Here 0xCF8 is a latch, one for each thread (two threads of the
//! driver must not finish each other's accesses), and a dword of a
//! function this process holds (one per GPU) is read and written by the
//! kernel for it ([`Space`]).
//!
//! Every other function is read by the kernel too ([`Space::read_at`]),
//! so the driver's walk up to its GPU's root port finds the port, as it
//! did in the kernel. It used to find nothing (every other function read
//! as absent), and what the driver does there was skipped without a
//! word: matching the GPU's Max Payload Size to the port's, the fix for
//! the console card's firmware boot wedging the link; taking VGA routing
//! off the bridges for that boot; resetting the bus when it wedges
//! anyway. Of writes to a function not held, only Bridge Control of a
//! bridge above a GPU reaches the kernel, which takes two bits of it
//! ([`Space::bridge_control`]); the rest go nowhere, as before.
//!
//! The BARs are the kernel's to write, so they are kept here, as a
//! hypervisor keeps them: a write of all ones reads back the size mask,
//! as on the bus, and the original value written back restores it. The
//! `pci` crate's probe and the driver's own BAR sizing work unchanged,
//! and nothing reaches the device.
//!
//! Any other port (the VGA registers, say) reads as all ones and takes
//! nothing; the first time each is touched it is logged.
use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// Where the configuration mechanism's address goes, and its data.
const CONFIG_ADDRESS: u32 = 0xcf8;
const CONFIG_DATA: u32 = 0xcfc;
const ENABLE: u32 = 1 << 31;
/// The BARs, 0x10 to 0x27.
const BARS: core::ops::Range<u8> = 0x10..0x28;
/// Bridge Control, in a PCI-PCI bridge's header, and its Secondary Bus
/// Reset bit.
const BRIDGE_CONTROL: u8 = 0x3e;
const BUS_RESET: u16 = 1 << 6;

thread_local! {
    /// 0xCF8 as this thread last wrote it. The real port is one for the
    /// whole machine, and the kernel's driver had it to itself for the
    /// length of an access; here another thread of the driver (the other
    /// card's, an interrupt's) may run between the address and the data.
    static ADDRESS: Cell<u32> = const { Cell::new(0) };
}

/// Why a write did not reach the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// A register only the kernel sets.
    Kernel,
    /// Anything else (outside the space, a kernel that cannot).
    Other,
}

/// The configuration space of the function this process holds, as the
/// kernel reads and writes it for it.
pub trait Space: Send + Sync {
    /// `width` bytes at `offset`, aligned to them.
    fn read(&self, offset: u8, width: u8) -> Option<u32>;
    /// The low `width` bytes of `value` at `offset`.
    fn write(&self, offset: u8, width: u8, value: u32) -> Result<(), Refused>;
    /// `width` bytes at `offset` of the function at `place`
    /// (`bus, device, function`), one this process does not hold, which
    /// the kernel reads for the holder of this one. `None` when it does
    /// not: that function then reads as absent.
    fn read_at(&self, _place: (u8, u8, u8), _offset: u8, _width: u8) -> Option<u32> {
        None
    }
    /// Write `value` to Bridge Control of the bridge at `place`, for a
    /// bridge above this function; the kernel takes VGA Enable and, of a
    /// bridge that leads only here, Secondary Bus Reset. What the register
    /// holds afterwards.
    fn bridge_control(&self, _place: (u8, u8, u8), _value: u16) -> Result<u16, Refused> {
        Err(Refused::Other)
    }
    /// Have the kernel write this function's BARs again, which a bus
    /// reset cleared: they are the kernel's to write.
    fn restore_bars(&self) -> Result<(), Refused> {
        Ok(())
    }
}

/// One BAR register as the function's driver sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Bar {
    /// What it reads now.
    value: u32,
    /// The bits a write changes: the address bits above the BAR's size.
    /// 0 for one that is not a memory BAR this process knows the length
    /// of, which then keeps its value whatever is written.
    mask: u32,
}

/// The six BAR registers, from what each reads and each memory BAR's
/// length (0 for none, an I/O BAR, the upper half of a 64-bit one).
fn bars(raw: [u32; 6], len: [usize; 6]) -> [Bar; 6] {
    let mut bars = [Bar::default(); 6];
    let mut i = 0;
    while i < 6 {
        bars[i].value = raw[i];
        let memory = raw[i] & 1 == 0;
        let wide = memory && raw[i] & 0b110 == 0b100 && i < 5;
        if memory && len[i] != 0 {
            let size_mask = !(len[i] as u64 - 1);
            bars[i].mask = size_mask as u32 & !0xf;
            if wide {
                bars[i + 1] = Bar {
                    value: raw[i + 1],
                    mask: (size_mask >> 32) as u32,
                };
            }
        }
        i += if wide { 2 } else { 1 };
    }
    bars
}

/// One function this process holds, at `bus:device.function`.
struct Function {
    space: Box<dyn Space>,
    /// The function's address as 0xCF8 carries it (bits 23:8).
    place: u32,
    bars: Mutex<[Bar; 6]>,
    /// The bus above it was reset through here and its BARs have not been
    /// written since: the device holds none, whatever `bars` says.
    reset: AtomicBool,
}

/// The ports: the configuration mechanism's latch, and every function
/// this process holds behind it, one per GPU.
pub struct Ports {
    functions: RwLock<Vec<Function>>,
    /// Ports and refused registers already logged.
    told: Mutex<BTreeSet<u32>>,
}

/// 0xCF8's bits 23:8 for `bus:device.function`.
fn place_bits((bus, device, function): (u8, u8, u8)) -> u32 {
    (bus as u32) << 16 | (device as u32 & 0x1f) << 11 | (function as u32 & 7) << 8
}

/// `bus, device, function` of an address in 0xCF8.
fn place_of(address: u32) -> (u8, u8, u8) {
    (
        (address >> 16) as u8,
        (address >> 11 & 0x1f) as u8,
        (address >> 8 & 7) as u8,
    )
}

impl Ports {
    /// The ports, with the one function at `place` behind them.
    pub fn new(space: Box<dyn Space>, place: (u8, u8, u8), bar_len: [usize; 6]) -> Ports {
        let ports = Ports {
            functions: RwLock::new(Vec::new()),
            told: Mutex::new(BTreeSet::new()),
        };
        ports.add(space, place, bar_len);
        ports
    }

    /// Another function behind the same ports, at `place`. Refused when
    /// one is already there.
    pub fn add(&self, space: Box<dyn Space>, place: (u8, u8, u8), bar_len: [usize; 6]) -> bool {
        let place = place_bits(place);
        let mut functions = self.functions.write().unwrap();
        if functions.iter().any(|f| f.place == place) {
            return false;
        }
        let mut raw = [0u32; 6];
        for (i, r) in raw.iter_mut().enumerate() {
            *r = space.read(0x10 + 4 * i as u8, 4).unwrap_or(0);
        }
        functions.push(Function {
            space,
            place,
            bars: Mutex::new(bars(raw, bar_len)),
            reset: AtomicBool::new(false),
        });
        true
    }

    /// The function the latch names, and the configuration register
    /// `port` reaches in it.
    fn ours(&self, port: u32) -> Option<(usize, u8)> {
        let (address, offset) = named(port)?;
        let at = self
            .functions
            .read()
            .unwrap()
            .iter()
            .position(|f| f.place == address & 0x00ff_ff00)?;
        Some((at, offset))
    }

    /// A read of a function this process does not hold: by the kernel,
    /// for the holder of any function here. All ones when it reads none.
    fn read_beyond(&self, port: u32, width: u8) -> u32 {
        let Some((address, offset)) = named(port) else {
            return ones(width);
        };
        self.functions
            .read()
            .unwrap()
            .first()
            .and_then(|f| f.space.read_at(place_of(address), offset, width))
            .unwrap_or(ones(width))
    }

    /// A write to a function this process does not hold. One that covers
    /// Bridge Control is offered to the kernel as each function's holder,
    /// which takes it for a bridge above that function; anything else
    /// goes nowhere.
    fn write_beyond(&self, port: u32, width: u8, value: u32) {
        let Some((address, offset)) = named(port) else {
            return;
        };
        let covers = offset <= BRIDGE_CONTROL && BRIDGE_CONTROL + 2 <= offset + width;
        if !covers {
            return;
        }
        let control = (value >> (8 * (BRIDGE_CONTROL - offset))) as u16;
        let reset = control & BUS_RESET != 0;
        let place = place_of(address);
        let functions = self.functions.read().unwrap();
        for function in functions.iter() {
            if function.space.bridge_control(place, control).is_ok() {
                // The kernel held the reset and let it go before it
                // answered: the bus was reset, and the bit is off again.
                if reset {
                    function.reset.store(true, Ordering::SeqCst);
                }
                return;
            }
        }
        let (bus, device, function) = place;
        if reset {
            // Each time: the recovery that asked for it goes on as if the
            // bus had been reset, and what it then reports is not a reset
            // that failed but one that was never done.
            eprintln!(
                "udrv-nvidia: bus reset at {bus:02x}:{device:02x}.{function} refused: \
                 the bridge leads to more than the GPU, or is not above it"
            );
            return;
        }
        self.tell(0x2_0000 | address >> 8 & 0xffff, || {
            format!("Bridge Control write at {bus:02x}:{device:02x}.{function} refused")
        });
    }

    pub fn read(&self, port: u32, width: u8) -> u32 {
        let ones = ones(width);
        if port == CONFIG_ADDRESS && width == 4 {
            return ADDRESS.get();
        }
        if !(CONFIG_DATA..CONFIG_DATA + 4).contains(&port) {
            self.tell(port, || format!("port {port:#x} read: not here"));
            return ones;
        }
        let Some((at, offset)) = self.ours(port) else {
            return self.read_beyond(port, width);
        };
        let functions = self.functions.read().unwrap();
        let function = &functions[at];
        if BARS.contains(&offset) {
            let i = (offset - BARS.start) as usize / 4;
            let shift = (offset % 4) * 8;
            return function.bars.lock().unwrap()[i].value >> shift & ones;
        }
        function.space.read(offset, width).unwrap_or(ones)
    }

    pub fn write(&self, port: u32, width: u8, value: u32) {
        if port == CONFIG_ADDRESS && width == 4 {
            ADDRESS.set(value);
            return;
        }
        if !(CONFIG_DATA..CONFIG_DATA + 4).contains(&port) {
            self.tell(port, || format!("port {port:#x} write: not here"));
            return;
        }
        let Some((at, offset)) = self.ours(port) else {
            self.write_beyond(port, width, value);
            return;
        };
        let functions = self.functions.read().unwrap();
        let function = &functions[at];
        if BARS.contains(&offset) {
            // The driver puts its BARs back after the bus reset it asked
            // for. They are the kernel's to write, so its first write of
            // one has the kernel write them all, to what it recorded.
            if function.reset.swap(false, Ordering::SeqCst)
                && function.space.restore_bars().is_err()
            {
                self.refused(offset);
            }
            let i = (offset - BARS.start) as usize / 4;
            let shift = (offset % 4) * 8;
            let lanes = ones(width) << shift;
            let bar = &mut function.bars.lock().unwrap()[i];
            let written = (bar.value & !lanes) | (value << shift & lanes);
            bar.value = (written & bar.mask) | (bar.value & !bar.mask);
            return;
        }
        let space = &function.space;
        match space.write(offset, width, value) {
            Ok(()) => {}
            // A wide write over a register the kernel keeps (a dword at
            // 0x0C covers the header type): the bytes that change go one
            // at a time, the kernel's own among them refused.
            Err(Refused::Kernel) if width > 1 => {
                for k in 0..width {
                    let byte = value >> (8 * k) & 0xff;
                    let at = offset + k;
                    if space.read(at, 1) != Some(byte) && space.write(at, 1, byte).is_err() {
                        self.refused(at);
                    }
                }
            }
            Err(_) => self.refused(offset),
        }
    }

    fn refused(&self, offset: u8) {
        self.tell(0x1_0000 | offset as u32, || {
            format!("config write at {offset:#x} refused")
        });
    }

    fn tell(&self, key: u32, what: impl FnOnce() -> String) {
        if self.told.lock().unwrap().insert(key) {
            eprintln!("udrv-nvidia: {}", what());
        }
    }
}

/// The address this thread latched, when it enables the mechanism, and
/// the configuration register `port` (0xCFC to 0xCFF) reaches at it.
fn named(port: u32) -> Option<(u32, u8)> {
    let address = ADDRESS.get();
    (address & ENABLE != 0)
        .then_some((address, (address & 0xfc) as u8 + (port - CONFIG_DATA) as u8))
}

fn ones(width: u8) -> u32 {
    match width {
        1 => 0xff,
        2 => 0xffff,
        _ => 0xffff_ffff,
    }
}

static PORTS: OnceLock<Ports> = OnceLock::new();

/// Answer the driver's ports for the function at `place` with `space`
/// from now on, beside those already there. Refused for a place already
/// taken.
pub fn install(space: Box<dyn Space>, place: (u8, u8, u8), bar_len: [usize; 6]) -> Result<(), ()> {
    let mut space = Some(space);
    let ports = PORTS.get_or_init(|| Ports::new(space.take().unwrap(), place, bar_len));
    match space {
        None => Ok(()),
        Some(space) => ports.add(space, place, bar_len).then_some(()).ok_or(()),
    }
}

#[no_mangle]
extern "C" fn drivers_port_read(port: u32, width: u8) -> u32 {
    match PORTS.get() {
        Some(ports) => ports.read(port, width),
        None => ones(width),
    }
}

#[no_mangle]
extern "C" fn drivers_port_write(port: u32, width: u8, value: u32) {
    if let Some(ports) = PORTS.get() {
        ports.write(port, width, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pci::{CSpaceAccessMethod, Location, PortOps, BAR};
    use std::sync::Arc;

    /// A function's space as the kernel keeps it: 256 bytes, with the
    /// identity, BARs and ROM refused to writes.
    struct Fake {
        bytes: Arc<Mutex<[u8; 256]>>,
    }

    impl Space for Fake {
        fn read(&self, offset: u8, width: u8) -> Option<u32> {
            let b = self.bytes.lock().unwrap();
            let mut v = 0u32;
            for k in 0..width {
                v |= (b[(offset + k) as usize] as u32) << (8 * k);
            }
            Some(v)
        }
        fn write(&self, offset: u8, width: u8, value: u32) -> Result<(), Refused> {
            let kernel =
                |o: u8| o < 4 || (8..12).contains(&o) || o == 0x0e || (0x10..0x3c).contains(&o);
            if (offset..offset + width).any(kernel) {
                return Err(Refused::Kernel);
            }
            let mut b = self.bytes.lock().unwrap();
            for k in 0..width {
                b[(offset + k) as usize] = (value >> (8 * k)) as u8;
            }
            Ok(())
        }
    }

    /// An NVIDIA GPU's space at 01:00.0: a 16 MiB register BAR 0, a
    /// 256 MiB 64-bit prefetchable BAR 1 above 4 GiB, a 32 MiB 64-bit
    /// BAR 3, an I/O BAR 5.
    fn gpu() -> (Ports, Arc<Mutex<[u8; 256]>>) {
        let mut b = [0u8; 256];
        let mut put = |at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(0x00, 0x1e89_10de);
        put(0x04, 0x0010_0007);
        put(0x08, 0x0300_00a1);
        put(0x0c, 0x0080_0010);
        put(0x10, 0xf600_0000);
        put(0x14, 0x0000_000c);
        put(0x18, 0x0000_0040);
        put(0x1c, 0xf800_000c);
        put(0x20, 0x0000_0000);
        put(0x24, 0x0000_e001);
        put(0x3c, 0x0000_01ff);
        let bytes = Arc::new(Mutex::new(b));
        let ports = Ports::new(
            Box::new(Fake {
                bytes: bytes.clone(),
            }),
            (1, 0, 0),
            [16 << 20, 256 << 20, 0, 32 << 20, 0, 0],
        );
        (ports, bytes)
    }

    struct Through<'a>(&'a Ports);
    impl PortOps for Through<'_> {
        unsafe fn read8(&self, p: u16) -> u8 {
            self.0.read(p as u32, 1) as u8
        }
        unsafe fn read16(&self, p: u16) -> u16 {
            self.0.read(p as u32, 2) as u16
        }
        unsafe fn read32(&self, p: u32) -> u32 {
            self.0.read(p, 4)
        }
        unsafe fn write8(&self, p: u16, v: u8) {
            self.0.write(p as u32, 1, v as u32)
        }
        unsafe fn write16(&self, p: u16, v: u16) {
            self.0.write(p as u32, 2, v as u32)
        }
        unsafe fn write32(&self, p: u32, v: u32) {
            self.0.write(p, 4, v)
        }
    }

    const GPU: Location = Location {
        bus: 1,
        device: 0,
        function: 0,
    };

    #[test]
    fn the_pci_probe_finds_the_gpu_with_the_bars_the_kernel_sized() {
        let (ports, bytes) = gpu();
        let before = *bytes.lock().unwrap();
        // SAFETY: every port goes to `ports`.
        let dev = unsafe { pci::probe_function(&Through(&ports), GPU, CSpaceAccessMethod::IO) }
            .expect("the GPU is there");
        assert_eq!((dev.id.vendor_id, dev.id.device_id), (0x10de, 0x1e89));
        assert_eq!((dev.id.class, dev.id.revision_id), (3, 0xa1));
        // The lengths are what the same probe finds in the kernel, which
        // the `pci` crate computes 2 short (`zcore-drivers`' `bar_len`).
        assert!(
            matches!(dev.bars[0], Some(BAR::Memory(0xf600_0000, l, _, _)) if l == (16 << 20) - 2)
        );
        assert!(
            matches!(dev.bars[1], Some(BAR::Memory(0x40_0000_0000, l, _, _)) if l == (256 << 20) - 2)
        );
        assert!(dev.bars[2].is_none(), "the upper half of BAR 1");
        assert!(
            matches!(dev.bars[3], Some(BAR::Memory(0xf800_0000, l, _, _)) if l == (32 << 20) - 2)
        );
        // The sizing never reached the device: its space is as it was.
        assert_eq!(*bytes.lock().unwrap(), before);
    }

    #[test]
    fn the_driver_s_own_bar_sizing_reads_the_size_and_restores_the_bar() {
        let (ports, _) = gpu();
        let ops = Through(&ports);
        // SAFETY: as above.
        unsafe {
            let am = CSpaceAccessMethod::IO;
            am.write32(&ops, GPU, 0x10, !0);
            assert_eq!(am.read32(&ops, GPU, 0x10), 0xff00_0000);
            am.write32(&ops, GPU, 0x14, !0);
            am.write32(&ops, GPU, 0x18, !0);
            assert_eq!(am.read32(&ops, GPU, 0x14), 0xf000_000c);
            assert_eq!(am.read32(&ops, GPU, 0x18), 0xffff_ffff);
            am.write32(&ops, GPU, 0x10, 0xf600_0000);
            am.write32(&ops, GPU, 0x14, 0x0000_000c);
            am.write32(&ops, GPU, 0x18, 0x0000_0040);
            assert_eq!(am.read32(&ops, GPU, 0x10), 0xf600_0000);
            assert_eq!(am.read32(&ops, GPU, 0x18), 0x40);
            // The I/O BAR, whose length nobody gave: it keeps its value.
            am.write32(&ops, GPU, 0x24, !0);
            assert_eq!(am.read32(&ops, GPU, 0x24), 0xe001);
        }
    }

    #[test]
    fn writes_reach_the_gpu_and_only_the_gpu() {
        let (ports, bytes) = gpu();
        let ops = Through(&ports);
        let am = CSpaceAccessMethod::IO;
        // SAFETY: as above.
        unsafe {
            // The command register.
            am.write32(&ops, GPU, 0x04, 0x0010_0006);
            assert_eq!(bytes.lock().unwrap()[4], 6);
            // A dword over the header type: the latency timer still lands.
            am.write32(&ops, GPU, 0x0c, 0x0080_4010);
            assert_eq!(bytes.lock().unwrap()[0x0d], 0x40);
            assert_eq!(bytes.lock().unwrap()[0x0e], 0x80);
            // Another function: absent, and untouched.
            let root = Location {
                bus: 0,
                device: 1,
                function: 0,
            };
            assert_eq!(am.read32(&ops, root, 0), 0xffff_ffff);
            am.write32(&ops, root, 0x04, 0);
            assert_eq!(bytes.lock().unwrap()[4], 6);
        }
        // A port that is not the configuration mechanism.
        assert_eq!(ports.read(0x3c2, 1), 0xff);
        ports.write(0x3c2, 1, 0);
        // A partial read of the data port: the byte that address names.
        ports.write(CONFIG_ADDRESS, 4, ENABLE | 1 << 16 | 0x08);
        assert_eq!(ports.read(CONFIG_DATA + 3, 1), 0x03);
        assert_eq!(ports.read(CONFIG_DATA + 2, 2), 0x0300);
        // The latch disabled: nothing.
        ports.write(CONFIG_ADDRESS, 4, 1 << 16);
        assert_eq!(ports.read(CONFIG_DATA, 4), 0xffff_ffff);
    }

    #[test]
    fn two_gpus_behind_the_same_ports_each_answer_for_itself() {
        let (ports, one) = gpu();
        let other = Arc::new(Mutex::new(*one.lock().unwrap()));
        other.lock().unwrap()[2] = 0x8a; // device 1e8a
        let fake = |bytes: &Arc<Mutex<[u8; 256]>>| {
            Box::new(Fake {
                bytes: bytes.clone(),
            })
        };
        let lens = [16 << 20, 256 << 20, 0, 32 << 20, 0, 0];
        assert!(ports.add(fake(&other), (2, 0, 0), lens));
        assert!(
            !ports.add(fake(&other), (1, 0, 0), lens),
            "01:00.0 is taken"
        );
        let second = Location {
            bus: 2,
            device: 0,
            function: 0,
        };
        let ops = Through(&ports);
        let am = CSpaceAccessMethod::IO;
        // SAFETY: every port goes to `ports`.
        unsafe {
            assert_eq!(am.read32(&ops, GPU, 0) >> 16, 0x1e89);
            assert_eq!(am.read32(&ops, second, 0) >> 16, 0x1e8a);
            // Sizing one GPU's BAR leaves the other's as it was.
            am.write32(&ops, second, 0x10, !0);
            assert_eq!(am.read32(&ops, second, 0x10), 0xff00_0000);
            assert_eq!(am.read32(&ops, GPU, 0x10), 0xf600_0000);
            am.write32(&ops, second, 0x04, 0x0010_0002);
        }
        assert_eq!(other.lock().unwrap()[4], 2);
        assert_eq!(one.lock().unwrap()[4], 7);
    }

    /// `bus, device, function`.
    type Place = (u8, u8, u8);

    /// The machine around the GPUs, as the kernel answers for it: the
    /// functions this process does not hold, the Bridge Control writes it
    /// took, and how many times it wrote a GPU's BARs again.
    #[derive(Default)]
    struct Around {
        functions: Mutex<std::collections::HashMap<Place, [u8; 256]>>,
        control: Mutex<Vec<(Place, u16)>>,
        restored: std::sync::atomic::AtomicUsize,
    }

    impl Around {
        /// A PCI-PCI bridge at `place` with only `bus` behind it.
        fn bridge(&self, place: (u8, u8, u8), bus: u8, control: u16) {
            let mut b = [0u8; 256];
            b[0..4].copy_from_slice(&0x2030_8086u32.to_le_bytes());
            b[0x0e] = 0x01;
            b[0x18..0x1c].copy_from_slice(&[place.0, bus, bus, 0]);
            b[0x3c..0x40].copy_from_slice(&((control as u32) << 16 | 0x01ff).to_le_bytes());
            self.functions.lock().unwrap().insert(place, b);
        }

        fn control_of(&self, place: (u8, u8, u8)) -> u16 {
            let f = self.functions.lock().unwrap();
            u16::from_le_bytes([f[&place][0x3e], f[&place][0x3f]])
        }
    }

    /// A GPU's space with the kernel's answers for the rest of the machine:
    /// reads of any function, and Bridge Control of the bridge `above`.
    struct Held {
        own: Fake,
        around: Arc<Around>,
        above: (u8, u8, u8),
    }

    impl Space for Held {
        fn read(&self, offset: u8, width: u8) -> Option<u32> {
            self.own.read(offset, width)
        }
        fn write(&self, offset: u8, width: u8, value: u32) -> Result<(), Refused> {
            self.own.write(offset, width, value)
        }
        fn read_at(&self, place: (u8, u8, u8), offset: u8, width: u8) -> Option<u32> {
            let f = self.around.functions.lock().unwrap();
            let Some(b) = f.get(&place) else {
                return Some(ones(width));
            };
            let mut v = 0u32;
            for k in 0..width {
                v |= (b[(offset + k) as usize] as u32) << (8 * k);
            }
            Some(v)
        }
        fn bridge_control(&self, place: (u8, u8, u8), value: u16) -> Result<u16, Refused> {
            if place != self.above {
                return Err(Refused::Kernel);
            }
            let old = self.around.control_of(place);
            // As the kernel: VGA Enable is taken, and a bus reset is a
            // pulse it has ended by the time it answers.
            let new = (old & !0x48) | (value & 0x08);
            let mut f = self.around.functions.lock().unwrap();
            f.get_mut(&place).unwrap()[0x3e..0x40].copy_from_slice(&new.to_le_bytes());
            self.around.control.lock().unwrap().push((place, value));
            Ok(new)
        }
        fn restore_bars(&self) -> Result<(), Refused> {
            self.around
                .restored
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    const PORT: (u8, u8, u8) = (0, 1, 0);
    const OTHER_PORT: (u8, u8, u8) = (0, 2, 0);

    /// The GPU at 01:00.0 under a root port at 00:01.0, a second GPU at
    /// 02:00.0 under 00:02.0, and the kernel answering for the ports.
    fn machine() -> (Ports, Arc<Around>, Arc<Mutex<[u8; 256]>>) {
        let (_, bytes) = gpu();
        let around = Arc::new(Around::default());
        around.bridge(PORT, 1, 0x001a);
        around.bridge(OTHER_PORT, 2, 0x0002);
        let lens = [16 << 20, 256 << 20, 0, 32 << 20, 0, 0];
        let held = |bytes: &Arc<Mutex<[u8; 256]>>, above| {
            Box::new(Held {
                own: Fake {
                    bytes: bytes.clone(),
                },
                around: around.clone(),
                above,
            })
        };
        let ports = Ports::new(held(&bytes, PORT), (1, 0, 0), lens);
        let second = Arc::new(Mutex::new(*bytes.lock().unwrap()));
        assert!(ports.add(held(&second, OTHER_PORT), (2, 0, 0), lens));
        (ports, around, bytes)
    }

    fn loc((bus, device, function): (u8, u8, u8)) -> Location {
        Location {
            bus,
            device,
            function,
        }
    }

    /// The driver's own walk (`NvidiaGpu::find_parent_bridge`): the first
    /// PCI-PCI bridge whose secondary bus is the GPU's.
    fn parent_bridge(ops: &Through, bus: u8) -> Option<(u8, u8, u8)> {
        let am = CSpaceAccessMethod::IO;
        for b in 0..=bus {
            for device in 0..32u8 {
                for function in 0..8u8 {
                    let at = loc((b, device, function));
                    // SAFETY: every port goes to the `Ports` behind `ops`.
                    unsafe {
                        if am.read16(ops, at, 0x00) == 0xffff
                            || am.read8(ops, at, 0x0e) & 0x7f != 0x01
                        {
                            continue;
                        }
                        if am.read8(ops, at, 0x19) == bus {
                            return Some((b, device, function));
                        }
                    }
                }
            }
        }
        None
    }

    #[test]
    fn the_walk_to_the_root_port_finds_it_through_the_kernel() {
        let (ports, _, _) = machine();
        let ops = Through(&ports);
        assert_eq!(parent_bridge(&ops, 1), Some(PORT));
        assert_eq!(parent_bridge(&ops, 2), Some(OTHER_PORT));
        // With a kernel that reads nothing for it, as before: no port.
        let (alone, _) = gpu();
        assert_eq!(parent_bridge(&Through(&alone), 1), None);
        // The port's registers are the port's, BARs and bus numbers too:
        // nothing of them is kept here.
        // SAFETY: as above.
        unsafe {
            let am = CSpaceAccessMethod::IO;
            assert_eq!(am.read32(&ops, loc(PORT), 0x18), 0x0001_0100);
            assert_eq!(am.read16(&ops, loc(PORT), 0x3e), 0x001a);
        }
    }

    #[test]
    fn vga_routing_of_the_port_above_is_the_kernel_s_to_write_and_nothing_else_is() {
        use zcore_drivers::bus::pci::cfgwrite::cfg_write16;
        let (ports, around, _) = machine();
        let ops = Through(&ports);
        let am = CSpaceAccessMethod::IO;
        // SAFETY: every port goes to `ports`.
        unsafe {
            // As `set_path_vga_routing` clears VGA Enable and puts it back.
            cfg_write16(&ops, am, loc(PORT), 0x3e, 0x001a & !0x0008);
            assert_eq!(am.read16(&ops, loc(PORT), 0x3e), 0x0012);
            cfg_write16(&ops, am, loc(PORT), 0x3e, 0x001a);
            assert_eq!(am.read16(&ops, loc(PORT), 0x3e), 0x001a);
            // The second GPU's port is written as that GPU's holder.
            cfg_write16(&ops, am, loc(OTHER_PORT), 0x3e, 0x000a);
            assert_eq!(around.control_of(OTHER_PORT), 0x000a);
            // Any other register of the port: nowhere.
            let command = am.read32(&ops, loc(PORT), 0x04);
            am.write32(&ops, loc(PORT), 0x04, !command);
            am.write32(&ops, loc(PORT), 0x18, 0);
            assert_eq!(am.read32(&ops, loc(PORT), 0x04), command);
            assert_eq!(am.read32(&ops, loc(PORT), 0x18), 0x0001_0100);
            // A bridge above no GPU of this process: refused, and as it was.
            around.bridge((0, 3, 0), 9, 0x0008);
            cfg_write16(&ops, am, loc((0, 3, 0)), 0x3e, 0);
            assert_eq!(around.control_of((0, 3, 0)), 0x0008);
        }
        assert_eq!(
            *around.control.lock().unwrap(),
            [(PORT, 0x0012), (PORT, 0x001a), (OTHER_PORT, 0x000a)]
        );
    }

    #[test]
    fn after_a_bus_reset_the_first_bar_write_has_the_kernel_write_the_bars() {
        use std::sync::atomic::Ordering;
        use zcore_drivers::bus::pci::cfgwrite::cfg_write16;
        let (ports, around, _) = machine();
        let ops = Through(&ports);
        let am = CSpaceAccessMethod::IO;
        // SAFETY: every port goes to `ports`.
        unsafe {
            // A BAR written with no reset before: kept here, as always.
            am.write32(&ops, GPU, 0x10, 0xf600_0000);
            assert_eq!(around.restored.load(Ordering::SeqCst), 0);
            // As `sbr_recover` pulses Secondary Bus Reset on the port:
            // the kernel does the whole pulse at the first write, and the
            // second finds the bit off...
            cfg_write16(&ops, am, loc(PORT), 0x3e, 0x001a | 0x0040);
            assert_eq!(around.control_of(PORT), 0x001a);
            cfg_write16(&ops, am, loc(PORT), 0x3e, 0x001a);
            assert_eq!(around.control_of(PORT), 0x001a);
            assert_eq!(
                *around.control.lock().unwrap(),
                [(PORT, 0x005a), (PORT, 0x001a)]
            );
            assert_eq!(around.restored.load(Ordering::SeqCst), 0);
            // ...and writes the GPU's BARs back: the kernel does, once.
            am.write32(&ops, GPU, 0x10, 0xf600_0000);
            assert_eq!(around.restored.load(Ordering::SeqCst), 1);
            am.write32(&ops, GPU, 0x14, 0x0000_000c);
            am.write32(&ops, GPU, 0x18, 0x0000_0040);
            assert_eq!(around.restored.load(Ordering::SeqCst), 1);
            // The other GPU was not under that port: its BARs are not
            // written by anybody.
            am.write32(&ops, loc((2, 0, 0)), 0x10, 0xf600_0000);
            assert_eq!(around.restored.load(Ordering::SeqCst), 1);
            // A reset the kernel refuses (a bridge above no GPU of this
            // process) reset nothing: no BARs to write again after it.
            around.bridge((0, 3, 0), 9, 0x0002);
            cfg_write16(&ops, am, loc((0, 3, 0)), 0x3e, 0x0002 | 0x0040);
            assert_eq!(around.control_of((0, 3, 0)), 0x0002);
            am.write32(&ops, GPU, 0x10, 0xf600_0000);
            am.write32(&ops, loc((2, 0, 0)), 0x10, 0xf600_0000);
            assert_eq!(around.restored.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn each_thread_latches_its_own_address() {
        let (ports, _, _) = machine();
        let ports = Arc::new(ports);
        // This thread names the GPU's identity...
        ports.write(CONFIG_ADDRESS, 4, ENABLE | 1 << 16);
        // ...another names the port's, and reads it, in between...
        let theirs = ports.clone();
        let seen = std::thread::spawn(move || {
            theirs.write(CONFIG_ADDRESS, 4, ENABLE | 1 << 11);
            theirs.read(CONFIG_DATA, 4)
        })
        .join()
        .unwrap();
        assert_eq!(seen, 0x2030_8086);
        // ...and this one still reads what it named.
        assert_eq!(ports.read(CONFIG_DATA, 4), 0x1e89_10de);
        assert_eq!(ports.read(CONFIG_ADDRESS, 4), ENABLE | 1 << 16);
    }
}
