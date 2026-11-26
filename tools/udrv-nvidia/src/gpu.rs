//! The GPU this process holds: claimed from the kernel
//! (`ECLIPSE_DEVICE_CLAIM`, kind NVIDIA), with the rest of the function
//! reached through `ECLIPSE_DEVICE_PCI_FN`. What the driver needs of it
//! is put where `zcore-drivers` looks: its configuration space behind the
//! emulated ports, its BARs behind the [`BarMapper`], and DMA memory
//! through the claim's bus transaction initiator.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pci::{Location, PCIDevice};
use udrivers::claim::{self, Claim, Dma, PciFn, KIND_NVIDIA};
use udrivers::zx::{self, Handle, Status};
use zcore_drivers::bus::pci::{PortOpsImpl, PCI_ACCESS};

use crate::mapper::{Bar, BarMapper};
use crate::platform::{self, Range};
use crate::ports::{self, Refused, Space};

const PAGE: usize = 4096;

/// The configuration space, as the kernel reads and writes it.
struct Kernel(PciFn);

impl Space for Kernel {
    fn read(&self, offset: u8, width: u8) -> Option<u32> {
        self.0.config_read(offset as usize, width as usize).ok()
    }

    fn write(&self, offset: u8, width: u8, value: u32) -> Result<(), Refused> {
        self.0
            .config_write(offset as usize, width as usize, value)
            .map_err(refused)
    }

    fn read_at(&self, place: (u8, u8, u8), offset: u8, width: u8) -> Option<u32> {
        self.0
            .config_read_at(place, offset as usize, width as usize)
            .ok()
    }

    fn bridge_control(&self, place: (u8, u8, u8), value: u16) -> Result<u16, Refused> {
        self.0.bridge_control_write(place, value).map_err(refused)
    }

    fn restore_bars(&self) -> Result<(), Refused> {
        self.0.restore_bars().map_err(refused)
    }
}

fn refused(status: Status) -> Refused {
    match status {
        Status::ACCESS_DENIED => Refused::Kernel,
        _ => Refused::Other,
    }
}

/// Which GPU: an NVIDIA by its index, or, to check this process's side
/// on a machine without one, any PCI function by its identity.
#[derive(Debug, Clone, Copy)]
pub enum Which {
    Nvidia(usize),
    Pci {
        vendor: u16,
        device: u16,
        index: usize,
    },
}

pub struct Gpu {
    pub claim: Claim,
    pub pci: PciFn,
    pub loc: Location,
    pub bars: Vec<Bar>,
    /// The function as the `pci` crate's probe reads it through the ports,
    /// as the kernel's bus scan did.
    pub dev: PCIDevice,
}

/// Claim the GPU and put its configuration space behind the ports.
pub fn claim(which: Which) -> Result<Gpu, String> {
    claim_one(which)?.ok_or_else(|| format!("claim {which:?}: no such device"))
}

/// Claim every GPU `which` numbers, from 0 up to the first the kernel
/// does not have, each behind the ports: the GPUs of one RM. An error for
/// one the kernel has and refuses, or for none at all.
pub fn claim_all(which: impl Fn(usize) -> Which) -> Result<Vec<Gpu>, String> {
    let mut gpus = Vec::new();
    while let Some(gpu) = claim_one(which(gpus.len()))? {
        gpus.push(gpu);
    }
    if gpus.is_empty() {
        return Err(format!("claim {:?}: no such device", which(0)));
    }
    Ok(gpus)
}

/// [`claim`], `None` for a GPU the kernel does not have.
fn claim_one(which: Which) -> Result<Option<Gpu>, String> {
    let (claim, pci) = match which {
        Which::Nvidia(index) => (
            claim::claim(KIND_NVIDIA, index, 50),
            PciFn::of(KIND_NVIDIA, index),
        ),
        Which::Pci {
            vendor,
            device,
            index,
        } => (
            claim::claim_pci(vendor, device, index, 50),
            PciFn::of_pci(vendor, device, index),
        ),
    };
    let claim = match claim {
        Ok(claim) => claim,
        Err(Status::NOT_FOUND) => return Ok(None),
        Err(e) => return Err(format!("claim {which:?}: {e}")),
    };
    let place = pci.place().map_err(|e| format!("place: {e}"))?;
    let mut lens = [0usize; 6];
    for (i, len) in lens.iter_mut().enumerate() {
        *len = pci.bar_len(i).map_err(|e| format!("bar {i} length: {e}"))?;
    }
    let mut bars = Vec::new();
    for (i, &len) in lens.iter().enumerate() {
        if len == 0 {
            continue;
        }
        let lo = pci
            .config_read(0x10 + 4 * i, 4)
            .map_err(|e| format!("bar {i}: {e}"))?;
        let hi = if lo & 0b110 == 0b100 && i < 5 {
            pci.config_read(0x14 + 4 * i, 4).unwrap_or(0)
        } else {
            0
        };
        bars.push(Bar {
            index: i,
            phys: (lo & !0xf) as usize | (hi as usize) << 32,
            len,
        });
    }
    ports::install(Box::new(Kernel(pci)), place, lens)
        .map_err(|_| format!("{place:x?} is behind the ports already"))?;
    let loc = Location {
        bus: place.0,
        device: place.1,
        function: place.2,
    };
    // SAFETY: every port the probe touches is answered by `ports`.
    let dev = unsafe { pci::probe_function(&PortOpsImpl, loc, PCI_ACCESS) }
        .ok_or_else(|| format!("nothing at {loc:?}"))?;
    Ok(Some(Gpu {
        claim,
        pci,
        loc,
        bars,
        dev,
    }))
}

impl Gpu {
    /// The driver's `IoMapper`: the BARs, mapped into this process.
    pub fn mapper(&self) -> Arc<BarMapper> {
        let pci = self.pci;
        BarMapper::new(
            self.bars.clone(),
            Box::new(move |index, len, combine| {
                let vmo = pci.bar(index, combine).ok()?;
                zx::map(vmo, len.div_ceil(PAGE) * PAGE)
                    .ok()
                    .map(|p| p as usize)
            }),
        )
    }
}

/// DMA memory from now on comes through the bus transaction initiators
/// of `gpus`, every GPU of this process: contiguous, pinned in each, so
/// any of them reaches it, at the device address each GPU uses (the same
/// for all, as one RM drives them all and hands any buffer to any of them).
pub fn install_dma(gpus: &[Gpu]) -> Result<(), String> {
    let btis: Vec<Handle> = gpus.iter().map(|g| g.claim.bti).collect();
    if btis.is_empty() {
        return Err("no GPU to give DMA memory to".into());
    }
    // The pins of every buffer handed out, by its device address, so its
    // free can give it back. The free used to do nothing: every GEM object
    // the driver ever made stayed pinned, which a video (buffers made and
    // freed every frame) turns into the whole of RAM within seconds, and
    // then every contiguous allocation in the system fails.
    type Pins = Mutex<HashMap<usize, (Handle, Vec<Handle>)>>;
    let pins: &'static Pins = Box::leak(Box::new(Mutex::new(HashMap::new())));
    platform::install_dma(
        Box::new(move |pages| {
            let dma = match Dma::new_on(&btis, pages * PAGE, None) {
                Ok(dma) => dma,
                Err(e) => {
                    dma_refused(pages, &e);
                    return None;
                }
            };
            pins.lock()
                .unwrap()
                .insert(dma.daddr as usize, (dma.pmt, dma.also));
            Some(Range {
                phys: dma.daddr as usize,
                virt: dma.ptr as usize,
                len: dma.len,
            })
        }),
        Box::new(move |range| {
            let Some((pmt, also)) = pins.lock().unwrap().remove(&range.phys) else {
                eprintln!(
                    "udrv-nvidia: DMA free of {:#x}, which was never handed out",
                    range.phys
                );
                return;
            };
            if let Err(e) = Dma::release_on(range.virt as *mut u8, range.len, pmt, also) {
                eprintln!("udrv-nvidia: DMA free of {:#x}: pmt_unpin {e}", range.phys);
            }
        }),
    )
    .map_err(|_| "DMA memory already comes from somewhere".to_string())
}

/// A DMA allocation the kernel refused: the driver answers `ENOMEM` for it.
/// The first one and then one in 64 reach the log.
fn dma_refused(pages: usize, why: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(64) {
        eprintln!(
            "udrv-nvidia: no {} KiB of contiguous DMA memory ({why}); ENOMEM for it ({n} refused)",
            pages * 4
        );
    }
}
