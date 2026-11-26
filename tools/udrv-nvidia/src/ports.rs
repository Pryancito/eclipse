//! The I/O ports the driver reaches for (`drivers_port_read/write`, which
//! `zcore-drivers` calls instead of `in`/`out` when built with its
//! `process` feature). A process may not touch a port, so this answers.
//!
//! What the driver reaches through ports is, almost all of it, the PCI
//! configuration mechanism: an address in 0xCF8, then the dword at
//! 0xCFC. Here 0xCF8 is a latch, and a dword of a function this process
//! holds (one per GPU) is read and written by the kernel for it ([`Space`]);
//! every other function reads as absent (all ones, as a slot with nothing
//! in it does) and its writes go nowhere. So a walk up to the GPU's root
//! port finds nothing to change, which is what the driver, unchanged,
//! already copes with on a machine whose bridges it cannot see.
//!
//! The BARs are the kernel's to write, so they are kept here, as a
//! hypervisor keeps them: a write of all ones reads back the size mask,
//! as on the bus, and the original value written back restores it. The
//! `pci` crate's probe and the driver's own BAR sizing work unchanged,
//! and nothing reaches the device.
//!
//! Any other port (the VGA registers, say) reads as all ones and takes
//! nothing; the first time each is touched it is logged.
use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock, RwLock};

/// Where the configuration mechanism's address goes, and its data.
const CONFIG_ADDRESS: u32 = 0xcf8;
const CONFIG_DATA: u32 = 0xcfc;
const ENABLE: u32 = 1 << 31;
/// The BARs, 0x10 to 0x27.
const BARS: core::ops::Range<u8> = 0x10..0x28;

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
}

/// The ports: the configuration mechanism's latch, and every function
/// this process holds behind it, one per GPU.
pub struct Ports {
    address: Mutex<u32>,
    functions: RwLock<Vec<Function>>,
    /// Ports and refused registers already logged.
    told: Mutex<BTreeSet<u32>>,
}

/// 0xCF8's bits 23:8 for `bus:device.function`.
fn place_bits((bus, device, function): (u8, u8, u8)) -> u32 {
    (bus as u32) << 16 | (device as u32 & 0x1f) << 11 | (function as u32 & 7) << 8
}

impl Ports {
    /// The ports, with the one function at `place` behind them.
    pub fn new(space: Box<dyn Space>, place: (u8, u8, u8), bar_len: [usize; 6]) -> Ports {
        let ports = Ports {
            address: Mutex::new(0),
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
        });
        true
    }

    /// The function the latch names, and the configuration register
    /// `port` reaches in it.
    fn ours(&self, port: u32) -> Option<(usize, u8)> {
        let address = *self.address.lock().unwrap();
        if address & ENABLE == 0 {
            return None;
        }
        let at = self
            .functions
            .read()
            .unwrap()
            .iter()
            .position(|f| f.place == address & 0x00ff_ff00)?;
        Some((at, (address & 0xfc) as u8 + (port - CONFIG_DATA) as u8))
    }

    pub fn read(&self, port: u32, width: u8) -> u32 {
        let ones = ones(width);
        if port == CONFIG_ADDRESS && width == 4 {
            return *self.address.lock().unwrap();
        }
        if !(CONFIG_DATA..CONFIG_DATA + 4).contains(&port) {
            self.tell(port, || format!("port {port:#x} read: not here"));
            return ones;
        }
        let Some((at, offset)) = self.ours(port) else {
            return ones;
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
            *self.address.lock().unwrap() = value;
            return;
        }
        if !(CONFIG_DATA..CONFIG_DATA + 4).contains(&port) {
            self.tell(port, || format!("port {port:#x} write: not here"));
            return;
        }
        let Some((at, offset)) = self.ours(port) else {
            return;
        };
        let functions = self.functions.read().unwrap();
        let function = &functions[at];
        if BARS.contains(&offset) {
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
}
