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
use crate::ports::{self, Ports, Refused, Space};

const PAGE: usize = 4096;

/// The configuration space, as the kernel reads and writes it.
struct Kernel(PciFn);

impl Space for Kernel {
    fn read(&self, offset: u8, width: u8) -> Option<u32> {
        self.0.config_read(offset as usize, width as usize).ok()
    }

    fn write(&self, offset: u8, width: u8, value: u32) -> Result<(), Refused> {
        match self.0.config_write(offset as usize, width as usize, value) {
            Ok(()) => Ok(()),
            Err(Status::ACCESS_DENIED) => Err(Refused::Kernel),
            Err(_) => Err(Refused::Other),
        }
    }
}

/// Which GPU: an NVIDIA by its index, or, to check this process's side
/// on a machine without one, any PCI function by its identity.
#[derive(Debug, Clone, Copy)]
pub enum Which {
    Nvidia(usize),
    Pci { vendor: u16, device: u16 },
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
    let (claim, pci) = match which {
        Which::Nvidia(index) => (
            claim::claim(KIND_NVIDIA, index, 50),
            PciFn::of(KIND_NVIDIA, index),
        ),
        Which::Pci { vendor, device } => (
            claim::claim_pci(vendor, device, 0, 50),
            PciFn::of_pci(vendor, device, 0),
        ),
    };
    let claim = claim.map_err(|e| format!("claim {which:?}: {e}"))?;
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
    ports::install(Ports::new(Box::new(Kernel(pci)), place, lens))
        .map_err(|_| "the ports are already some other function's".to_string())?;
    let loc = Location {
        bus: place.0,
        device: place.1,
        function: place.2,
    };
    // SAFETY: every port the probe touches is answered by `ports`.
    let dev = unsafe { pci::probe_function(&PortOpsImpl, loc, PCI_ACCESS) }
        .ok_or_else(|| format!("nothing at {loc:?}"))?;
    Ok(Gpu {
        claim,
        pci,
        loc,
        bars,
        dev,
    })
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

    /// DMA memory from now on comes through this GPU's bus transaction
    /// initiator: contiguous, pinned, at the device address the GPU uses.
    /// What the driver frees stays with the process, pinned, until it
    /// exits (the claim's memory has no way back yet).
    pub fn install_dma(&self) -> Result<(), String> {
        let bti = self.claim.bti;
        // The pin of every buffer handed out, by its device address, so its
        // free can give it back. The free used to do nothing: every GEM
        // object the driver ever made stayed pinned, which a video (buffers
        // made and freed every frame) turns into the whole of RAM within
        // seconds, and then every contiguous allocation in the system fails.
        let pins: &'static Mutex<HashMap<usize, Handle>> =
            Box::leak(Box::new(Mutex::new(HashMap::new())));
        platform::install_dma(
            Box::new(move |pages| {
                let dma = match Dma::new(bti, pages * PAGE, None) {
                    Ok(dma) => dma,
                    Err(e) => {
                        dma_refused(pages, &e);
                        return None;
                    }
                };
                pins.lock().unwrap().insert(dma.daddr as usize, dma.pmt);
                Some(Range {
                    phys: dma.daddr as usize,
                    virt: dma.ptr as usize,
                    len: dma.len,
                })
            }),
            Box::new(move |range| {
                let Some(pmt) = pins.lock().unwrap().remove(&range.phys) else {
                    eprintln!("udrv-nvidia: DMA free of {:#x}, which was never handed out", range.phys);
                    return;
                };
                if let Err(e) = Dma::release(range.virt as *mut u8, range.len, pmt) {
                    eprintln!("udrv-nvidia: DMA free of {:#x}: pmt_unpin {e}", range.phys);
                }
            }),
        )
        .map_err(|_| "DMA memory already comes from somewhere".to_string())
    }
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
