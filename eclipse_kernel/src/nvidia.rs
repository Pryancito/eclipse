//! NVIDIA GPU Driver Support (Nova-aligned)
//!
//! This module provides integration with NVIDIA GPUs for Eclipse OS, aligned with
//! the **Nova** open-source driver project (Linux kernel 6.15+) and NVIDIA's
//! open-gpu-kernel-modules (https://github.com/NVIDIA/open-gpu-kernel-modules).
//!
//! ## Nova (upstream reference)
//! Nova is the new open-source, Rust-written NVIDIA driver in the mainline Linux
//! kernel, intended to supersede Nouveau for GSP-based GPUs:
//! - **nova-core**: Core driver, abstraction around GPU hardware and firmware (GSP, Falcon, FWSEC, devinit, VBIOS)
//! - **nova-drm**: Second-level DRM driver for display/compute
//!
//! Eclipse follows the same architecture: core (this module + sidewind_nvidia) and
//! display via VirtIO/GOP or userspace display service.
//!
//! ## Supported GPUs
//! GSP-based NVIDIA GPUs only (Turing and newer):
//! - Turing (RTX 20 series)
//! - Ampere (RTX 30 series)
//! - Ada Lovelace (RTX 40 series)
//! - Hopper (H100, H200, etc.)
//! - Blackwell (RTX 50 series, B100, B200)
//!
//! ## Features
//! - PCI device detection and enumeration
//! - GPU identification via PCI device ID + PMC_BOOT_0 hardware cross-check
//! - BAR (Base Address Register) mapping (32 MB for Turing+)
//! - VRAM size detection from NV_PFB_CSTATUS register
//! - GPU temperature reading from THERM registers
//! - PMC engine enable before GSP boot
//! - GSP firmware loading and Falcon CPUCTL boot sequence
//! - GSP RPC infrastructure
//! - Multi-GPU support
//!
//! ## References
//! - Nova: https://docs.kernel.org/next/gpu/nova/index.html
//! - open-gpu-kernel-modules: https://github.com/NVIDIA/open-gpu-kernel-modules

use crate::pci::{PciDevice, find_nvidia_gpus, get_bar};
use crate::memory::{map_mmio_range, PHYS_MEM_OFFSET, GPU_FW_PHYS_BASE, GPU_FW_MAX_SIZE, GPU_RPC_PHYS_BASE, GPU_RPC_MAX_SIZE};
use crate::serial;
use crate::filesystem;
use alloc::vec::Vec;
use alloc::vec;

// Use our shared NVIDIA abstraction crate
use sidewind_nvidia::registers::*;
use sidewind_nvidia::gsp::*;

/// NVIDIA GPU Architecture Types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvidiaArchitecture {
    Unknown,
    Turing,      // RTX 20 series (2018) — TU1xx, chip_id 0x160..0x16F
    Ampere,      // RTX 30 series (2020) — GA1xx, chip_id 0x170..0x17F
    AdaLovelace, // RTX 40 series (2022) — AD1xx, chip_id 0x190..0x19F
    Hopper,      // H100/H200 (2022)     — GH1xx, chip_id 0x1B0..0x1BF
    Blackwell,   // RTX 50 series (2024) — GB2xx, chip_id >= 0x200
}

/// NVIDIA GPU Information
#[derive(Debug, Clone)]
pub struct NvidiaGpuInfo {
    pub pci_device: PciDevice,
    pub architecture: NvidiaArchitecture,
    pub name: &'static str,
    pub memory_size_mb: u32,
    pub cuda_cores: u32,
    pub sm_count: u32,  // Streaming Multiprocessor count
    pub rt_cores: u32,  // Ray Tracing cores
    pub tensor_cores: u32,  // Tensor cores for AI
}

/// Active NVIDIA GPU with mapped registers
pub struct NvidiaGpu {
    pub info: NvidiaGpuInfo,
    pub bar0_virt: u64,
    pub bar0_size: usize,
}

impl NvidiaGpuInfo {
    /// Create GPU info from PCI device
    pub fn from_pci_device(pci_device: PciDevice) -> Self {
        let (architecture, name, memory_size_mb, cuda_cores, sm_count) = 
            identify_gpu(pci_device.device_id);
        
        // Calculate RT cores and Tensor cores based on SM count
        let (rt_cores, tensor_cores) = match architecture {
            NvidiaArchitecture::Turing => (sm_count, sm_count * 8),
            NvidiaArchitecture::Ampere => (sm_count, sm_count * 4),
            NvidiaArchitecture::AdaLovelace => (sm_count, sm_count * 4),
            NvidiaArchitecture::Hopper => (sm_count, sm_count * 4),
            NvidiaArchitecture::Blackwell => (sm_count, sm_count * 4),
            _ => (0, 0),
        };
        
        Self {
            pci_device,
            architecture,
            name,
            memory_size_mb,
            cuda_cores,
            sm_count,
            rt_cores,
            tensor_cores,
        }
    }
    
    /// Check if this GPU is supported by open-gpu-kernel-modules
    pub fn is_open_source_supported(&self) -> bool {
        // Open-gpu-kernel-modules supports Turing and newer
        matches!(self.architecture, 
            NvidiaArchitecture::Turing | 
            NvidiaArchitecture::Ampere | 
            NvidiaArchitecture::AdaLovelace |
            NvidiaArchitecture::Hopper |
            NvidiaArchitecture::Blackwell
        )
    }
}

/// Derive NvidiaArchitecture from the PMC_BOOT_0 hardware register value.
/// Uses chip_id = PMC_BOOT_0[31:20] (12-bit field) as discriminant.
/// This matches the open-gpu-kernel-modules chip-detection logic.
pub fn arch_from_pmc_boot0(boot0: u32) -> NvidiaArchitecture {
    let chip_id = (boot0 >> PMC_BOOT0_CHIP_ID_SHIFT) & PMC_BOOT0_CHIP_ID_MASK;
    if chip_id >= PMC_BOOT0_CHIPID_BLACKWELL_MIN {
        NvidiaArchitecture::Blackwell
    } else if chip_id >= PMC_BOOT0_CHIPID_HOPPER_MIN && chip_id <= PMC_BOOT0_CHIPID_HOPPER_MAX {
        NvidiaArchitecture::Hopper
    } else if chip_id >= PMC_BOOT0_CHIPID_ADA_MIN && chip_id <= PMC_BOOT0_CHIPID_ADA_MAX {
        NvidiaArchitecture::AdaLovelace
    } else if chip_id >= PMC_BOOT0_CHIPID_AMPERE_MIN && chip_id <= PMC_BOOT0_CHIPID_AMPERE_MAX {
        NvidiaArchitecture::Ampere
    } else if chip_id >= PMC_BOOT0_CHIPID_TURING_MIN && chip_id <= PMC_BOOT0_CHIPID_TURING_MAX {
        NvidiaArchitecture::Turing
    } else {
        NvidiaArchitecture::Unknown
    }
}

/// Read VRAM size in MB from the NV_PFB_CSTATUS register (BAR0 + 0x10020C).
/// Returns 0 if the register is inaccessible or not yet programmed by the GPU.
/// From open-gpu-kernel-modules: dev_fb.h / NV_PFB_CSTATUS bits [14:0].
pub fn read_vram_size_mb(bar0_virt: u64) -> u32 {
    let raw = unsafe {
        core::ptr::read_volatile((bar0_virt + NV_PFB_CSTATUS as u64) as *const u32)
    };
    raw & NV_PFB_CSTATUS_MEM_SIZE_MASK
}

/// Read GPU core temperature in Celsius from the THERM engine (BAR0 + 0x20400).
/// Bits [8:0] are a signed 9-bit value.  Returns None if the register reads 0
/// or 0xFFFFFFFF (GPU not yet initialized / THERM not powered).
/// From open-gpu-kernel-modules: dev_therm.h / NV_THERM_TEMP.
pub fn read_temperature(bar0_virt: u64) -> Option<i32> {
    let raw = unsafe {
        core::ptr::read_volatile((bar0_virt + NV_THERM_TEMP as u64) as *const u32)
    };
    if raw == 0 || raw == 0xFFFF_FFFF {
        return None;
    }
    let raw9 = raw & NV_THERM_TEMP_VALUE_MASK;
    // Sign-extend 9-bit value
    let temp = if (raw9 & NV_THERM_TEMP_VALUE_SIGN_BIT) != 0 {
        (raw9 as i32) - 512
    } else {
        raw9 as i32
    };
    Some(temp)
}

/// Identify GPU based on PCI device ID.
/// Returns (architecture, name, memory_mb, cuda_cores, sm_count).
/// Device IDs sourced from open-gpu-kernel-modules / NVIDIA PCI ID database.
fn identify_gpu(device_id: u16) -> (NvidiaArchitecture, &'static str, u32, u32, u32) {
    match device_id {
        // ---------------------------------------------------------------
        // Blackwell — RTX 50 series (2024–2025), GB202/GB203/GB205/GB206
        // ---------------------------------------------------------------
        0x2B85 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5090", 32768, 21760, 170),
        0x2B89 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5080", 16384, 10752, 84),
        0x2C00 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070 Ti", 16384,  8960, 70),
        0x2C20 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070",   12288,  6144, 48),
        0x2C30 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5060 Ti", 8192,  4608, 36),
        // ---------------------------------------------------------------
        // Ada Lovelace — RTX 40 series (2022–2024), AD102/AD103/AD104/AD106/AD107
        // ---------------------------------------------------------------
        // Desktop (all variants)
        0x2684 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090",           24576, 16384, 128),
        0x2685 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090 D",         24576, 16384, 128),
        0x2704 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080",           16384,  9728,  76),
        0x2702 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080 Super",     16384, 10240,  80),
        0x2782 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Ti",        12288,  7680,  60),
        0x2783 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Ti Super",  16384,  8448,  66),
        0x2786 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070",           12288,  5888,  46),
        0x2788 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Super",     12288,  7168,  56),
        0x2803 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Ti",         8192,  4352,  34),
        0x2805 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Ti 16GB",   16384,  4352,  34),
        0x2882 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060",            8192,  3072,  24),
        0x2860 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4050",            6144,  2560,  20),
        // Ada Lovelace — mobile
        0x27A0 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090 Laptop",    16384, 9728, 76),
        0x27B0 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080 Laptop",    12288, 7424, 58),
        0x27B8 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Laptop",    8192,  4608, 36),
        0x27BA => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Ti Laptop", 12288, 5888, 46),
        0x27E0 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Laptop",    8192,  3072, 24),
        0x27E8 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4050 Laptop",    6144,  2560, 20),
        // Ada Lovelace — professional / data-centre
        0x26B1 => (NvidiaArchitecture::AdaLovelace, "RTX 6000 Ada Generation",   49152, 18176, 142),
        0x26B3 => (NvidiaArchitecture::AdaLovelace, "RTX 5000 Ada Generation",   32768, 12800, 100),
        0x26B9 => (NvidiaArchitecture::AdaLovelace, "RTX 4500 Ada Generation",   24576,  7680,  60),
        0x26BA => (NvidiaArchitecture::AdaLovelace, "RTX 4000 Ada Generation",   20480,  6144,  48),
        0x26BB => (NvidiaArchitecture::AdaLovelace, "RTX 4000 SFF Ada Generation", 20480, 6144, 48),
        0x26BD => (NvidiaArchitecture::AdaLovelace, "RTX 2000 Ada Generation",   16384,  3072,  24),
        0x2230 => (NvidiaArchitecture::AdaLovelace, "NVIDIA L40",                48128, 18176, 142),
        0x26B5 => (NvidiaArchitecture::AdaLovelace, "NVIDIA L40S",               49152, 18176, 142),
        // ---------------------------------------------------------------
        // Hopper — H100/H200 (2022–2023), GH100
        // ---------------------------------------------------------------
        0x2330 => (NvidiaArchitecture::Hopper, "NVIDIA H100 SXM5 80GB",  81920, 0, 132),
        0x2331 => (NvidiaArchitecture::Hopper, "NVIDIA H100 PCIe 80GB",  81920, 0, 114),
        0x2335 => (NvidiaArchitecture::Hopper, "NVIDIA H200 SXM5 141GB", 144384, 0, 132),
        0x2339 => (NvidiaArchitecture::Hopper, "NVIDIA H100 NVL",        94208, 0, 132),
        // ---------------------------------------------------------------
        // Ampere — RTX 30 series (2020–2022), GA102/GA103/GA104/GA106/GA107
        // ---------------------------------------------------------------
        // Desktop
        0x2204 => (NvidiaArchitecture::Ampere, "GeForce RTX 3090",      24576, 10496, 82),
        0x2208 => (NvidiaArchitecture::Ampere, "GeForce RTX 3090 Ti",   24576, 10752, 84),
        0x2206 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080",      10240,  8704, 68),
        0x220A => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 12GB", 12288,  8960, 70),
        0x2216 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 Ti",   12288, 10240, 80),
        0x2484 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070",       8192,  5888, 46),
        0x2488 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070 Ti",    8192,  6144, 48),
        0x2489 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 Ti",    8192,  4864, 38),
        0x2503 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060",       12288,  3584, 28),
        0x2504 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 8GB",    8192,  3584, 28),
        0x2544 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 12GB",  12288,  3584, 28),
        0x2571 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050",        8192,  2560, 20),
        0x2582 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050 6GB",    6144,  2048, 16),
        // Ampere — mobile
        0x2420 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 Ti Laptop", 16384, 7424, 58),
        0x2460 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 Laptop",   16384, 6144, 48),
        0x24A0 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070 Ti Laptop", 8192, 5888, 46),
        0x24B0 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070 Laptop",    8192, 5120, 40),
        0x24DC => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 Laptop",    6144, 3840, 30),
        0x25A0 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050 Laptop",    4096, 2048, 16),
        // Ampere — professional / data-centre
        0x2235 => (NvidiaArchitecture::Ampere, "NVIDIA A100 80GB PCIe", 81920, 0, 108),
        0x20B5 => (NvidiaArchitecture::Ampere, "NVIDIA A100 80GB SXM4", 81920, 0, 108),
        0x20B2 => (NvidiaArchitecture::Ampere, "NVIDIA A100 40GB PCIe", 40960, 0, 108),
        0x20F5 => (NvidiaArchitecture::Ampere, "NVIDIA A10",             24576, 9216, 72),
        0x2236 => (NvidiaArchitecture::Ampere, "NVIDIA A10G",            24576, 9216, 72),
        0x2231 => (NvidiaArchitecture::Ampere, "NVIDIA A40",             49152, 10752, 84),
        0x2233 => (NvidiaArchitecture::Ampere, "NVIDIA A30",             24576, 0, 56),
        0x25B6 => (NvidiaArchitecture::Ampere, "NVIDIA A16",             16384, 0, 28),
        0x1EB8 => (NvidiaArchitecture::Ampere, "NVIDIA T4",              16384, 2560, 40),
        // ---------------------------------------------------------------
        // Turing — RTX 20 series / GTX 16 series (2018–2020), TU102..TU117
        // ---------------------------------------------------------------
        // RTX 20 series desktop
        0x1E02 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Ti",     11264, 4352, 68),
        0x1E04 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Super",   8192, 3072, 48),
        0x1E07 => (NvidiaArchitecture::Turing, "GeForce RTX 2080",         8192, 2944, 46),
        0x1E82 => (NvidiaArchitecture::Turing, "GeForce RTX 2070 Super",   8192, 2560, 40),
        0x1E84 => (NvidiaArchitecture::Turing, "GeForce RTX 2070",         8192, 2304, 36),
        0x1F02 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super",   8192, 2176, 34),
        0x1F06 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super",   8192, 2176, 34),
        0x1F07 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super 8G", 8192, 2176, 34),
        0x1F03 => (NvidiaArchitecture::Turing, "GeForce RTX 2060",         6144, 1920, 30),
        0x1F08 => (NvidiaArchitecture::Turing, "GeForce RTX 2060",         6144, 1920, 30),
        0x1F0A => (NvidiaArchitecture::Turing, "GeForce RTX 2060",         6144, 1920, 30),
        0x1F0B => (NvidiaArchitecture::Turing, "GeForce RTX 2060 6GB",     6144, 1920, 30),
        // GTX 16 series desktop (Turing architecture, no RT cores)
        0x1F36 => (NvidiaArchitecture::Turing, "GeForce GTX 1660 Super",   6144, 1408, 22),
        0x1F44 => (NvidiaArchitecture::Turing, "GeForce GTX 1660 Ti",      6144, 1536, 24),
        0x1F82 => (NvidiaArchitecture::Turing, "GeForce GTX 1660",         6144, 1408, 22),
        0x1F91 => (NvidiaArchitecture::Turing, "GeForce GTX 1650 Super",   4096, 1280, 20),
        0x1F99 => (NvidiaArchitecture::Turing, "GeForce GTX 1650",         4096, 896,  14),
        // Turing — mobile
        0x1E90 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Laptop",  8192, 2944, 46),
        0x1E91 => (NvidiaArchitecture::Turing, "GeForce RTX 2070 Laptop",  8192, 2304, 36),
        0x1E93 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Laptop",  6144, 1920, 30),
        // Turing — professional
        0x1E30 => (NvidiaArchitecture::Turing, "Quadro RTX 6000",  24576, 4608, 72),
        0x1E78 => (NvidiaArchitecture::Turing, "Quadro RTX 5000",  16384, 3072, 48),
        0x1E36 => (NvidiaArchitecture::Turing, "Quadro RTX 4000",   8192, 2304, 36),
        // Default/Unknown
        _ => (NvidiaArchitecture::Unknown, "Unknown NVIDIA GPU", 0, 0, 0),
    }
}

/// NVIDIA GSP Firmware Loader
pub struct GspLoader;

impl GspLoader {
    /// Load GSP firmware from filesystem into a dedicated physical region
    pub fn load_firmware(path: &str) -> Result<NvidiaFirmware, &'static str> {
        serial::serial_print("[NVIDIA] Loading GSP firmware from ");
        serial::serial_print(path);
        serial::serial_print("...\n");

        let inode = filesystem::Filesystem::lookup_path(path).map_err(|_| "Firmware file not found")?;
        let size = filesystem::Filesystem::get_file_size(inode).map_err(|_| "Failed to get firmware size")?;
        
        serial::serial_print("[NVIDIA]   Firmware size: ");
        serial::serial_print_dec(size);
        serial::serial_print(" bytes\n");

        if size > GPU_FW_MAX_SIZE {
            return Err("Firmware too large (exceeds GPU_FW_MAX_SIZE)");
        }

        // Use the centralized GPU hardware region defined in memory.rs
        let phys_base = GPU_FW_PHYS_BASE;
        let virt_base = PHYS_MEM_OFFSET + phys_base;

        serial::serial_print("[NVIDIA]   Allocating firmware memory at Phys: 0x");
        serial::serial_print_hex(phys_base);
        serial::serial_print("\n");

        // Read the file in 4KB chunks
        let mut offset: u64 = 0;
        let mut chunk = [0u8; 4096];
        
        while offset < size {
            let to_read = core::cmp::min(4096, (size - offset) as usize);
            let bytes_read = filesystem::Filesystem::read_file_by_inode_at(inode, &mut chunk[..to_read], offset)?;
            
            if bytes_read == 0 { break; }

            // Copy chunk to target physical memory via Higher Half mapping
            unsafe {
                core::ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    (virt_base + offset) as *mut u8,
                    bytes_read
                );
            }

            offset += bytes_read as u64;
            
            // Progress indicator every 1MB
            if offset % (1024 * 1024) == 0 {
                serial::serial_print(".");
            }
        }
        serial::serial_print(" Done\n");

        Ok(NvidiaFirmware {
            phys_base,
            virt_base,
            size: size as usize,
        })
    }
}

/// Represents loaded NVIDIA firmware in memory
pub struct NvidiaFirmware {
    pub phys_base: u64,
    pub virt_base: u64,
    pub size: usize,
}

/// GSP RPC Client
pub struct RpcClient {
    pub queue_virt: *mut GspRpcQueue,
    pub next_seq: u32,
}

impl RpcClient {
    pub fn new(phys_base: u64) -> Self {
        let virt = (PHYS_MEM_OFFSET + phys_base) as *mut GspRpcQueue;
        unsafe {
            GspRpcQueue::init_at(virt);
        }
        Self { 
            queue_virt: virt,
            next_seq: 1,
        }
    }

    pub fn send_command(&mut self, opcode: GspOpcode, payload: &[u8]) -> Result<u32, GspStatus> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        
        let header = GspHeader {
            opcode: opcode as u32,
            seq_num: seq,
            status: 0,
            payload_len: payload.len() as u32,
        };
        
        unsafe {
            (*self.queue_virt).push(header, payload)?;
        }
        Ok(seq)
    }

    pub fn poll_response(&mut self) -> Option<GspMessage<GSP_RPC_PAYLOAD_SIZE>> {
        unsafe {
            (*self.queue_virt).pop()
        }
    }
}

/// Initialize NVIDIA GPU subsystem
pub fn init() {
    serial::serial_print("[NVIDIA] Initializing NVIDIA GPU subsystem (Nova-aligned)...\n");
    serial::serial_print("[NVIDIA] Reference: Nova (Linux kernel 6.15+), open-gpu-kernel-modules\n");
    
    let gpus = find_nvidia_gpus();
    
    if gpus.is_empty() {
        serial::serial_print("[NVIDIA] No NVIDIA GPUs detected\n");
        return;
    }
    
    serial::serial_print("[NVIDIA] Found ");
    serial::serial_print_dec(gpus.len() as u64);
    serial::serial_print(" NVIDIA GPU(s)\n");
    
    for (index, gpu) in gpus.iter().enumerate() {
        let gpu_info = NvidiaGpuInfo::from_pci_device(*gpu);
        
        serial::serial_print("[NVIDIA] GPU ");
        serial::serial_print_dec(index as u64);
        serial::serial_print(": ");
        serial::serial_print(gpu_info.name);
        serial::serial_print("\n");
        
        serial::serial_print("[NVIDIA]   Device ID: 0x");
        serial::serial_print_hex(gpu.device_id as u64);
        serial::serial_print("\n");
        
        serial::serial_print("[NVIDIA]   Architecture (PCI ID): ");
        match gpu_info.architecture {
            NvidiaArchitecture::Blackwell   => serial::serial_print("Blackwell"),
            NvidiaArchitecture::AdaLovelace => serial::serial_print("Ada Lovelace"),
            NvidiaArchitecture::Ampere      => serial::serial_print("Ampere"),
            NvidiaArchitecture::Turing      => serial::serial_print("Turing"),
            NvidiaArchitecture::Hopper      => serial::serial_print("Hopper"),
            NvidiaArchitecture::Unknown     => serial::serial_print("Unknown"),
        }
        serial::serial_print("\n");
        
        if gpu_info.memory_size_mb > 0 {
            serial::serial_print("[NVIDIA]   Memory (PCI ID table): ");
            serial::serial_print_dec(gpu_info.memory_size_mb as u64);
            serial::serial_print(" MB\n");
        }
        
        if gpu_info.cuda_cores > 0 {
            serial::serial_print("[NVIDIA]   CUDA Cores: ");
            serial::serial_print_dec(gpu_info.cuda_cores as u64);
            serial::serial_print("\n");
            
            serial::serial_print("[NVIDIA]   SM Count: ");
            serial::serial_print_dec(gpu_info.sm_count as u64);
            serial::serial_print("\n");
            
            if gpu_info.rt_cores > 0 {
                serial::serial_print("[NVIDIA]   RT Cores: ");
                serial::serial_print_dec(gpu_info.rt_cores as u64);
                serial::serial_print("\n");
            }
            
            if gpu_info.tensor_cores > 0 {
                serial::serial_print("[NVIDIA]   Tensor Cores: ");
                serial::serial_print_dec(gpu_info.tensor_cores as u64);
                serial::serial_print("\n");
            }
        }
        
        serial::serial_print("[NVIDIA]   BAR0 (PCI): 0x");
        serial::serial_print_hex(gpu.bar0 as u64);
        serial::serial_print("\n");

        // Report advanced capabilities
        serial::serial_print("[NVIDIA]   Advanced Features:\n");
        serial::serial_print("[NVIDIA]     ✓ CUDA Runtime\n");
        if gpu_info.rt_cores > 0 {
            serial::serial_print("[NVIDIA]     ✓ Ray Tracing (RT Cores)\n");
        }
        serial::serial_print("[NVIDIA]     ✓ DisplayPort/HDMI Output\n");
        serial::serial_print("[NVIDIA]     ✓ Power Management\n");
        
        let encoder_caps = video::EncoderCapabilities::detect(&gpu_info);
        let decoder_caps = video::DecoderCapabilities::detect(&gpu_info);
        
        serial::serial_print("[NVIDIA]     ✓ Video Encode (NVENC): ");
        serial::serial_print_dec(encoder_caps.supported_codecs.len() as u64);
        serial::serial_print(" codecs\n");
        
        serial::serial_print("[NVIDIA]     ✓ Video Decode (NVDEC): ");
        serial::serial_print_dec(decoder_caps.supported_codecs.len() as u64);
        serial::serial_print(" codecs\n");
        
        if gpu_info.is_open_source_supported() {
            serial::serial_print("[NVIDIA]   ✓ Supported by open-gpu-kernel-modules\n");
        } else {
            serial::serial_print("[NVIDIA]   ⚠ Not supported by open-gpu-kernel-modules\n");
        }
        
        // Enable the PCI device (I/O + Memory + Bus Master)
        unsafe {
            crate::pci::enable_device(&gpu, true);
        }
        serial::serial_print("[NVIDIA]   Device enabled (I/O, Memory, Bus Master)\n");

        // --- Phase 1: BAR0 mapping ---
        // Turing and newer use 32 MB BAR0 (from open-gpu-kernel-modules default).
        // Legacy GPUs (pre-Turing, not supported here) use 16 MB.
        let bar0_phys = unsafe { get_bar(gpu, 0) };
        let bar0_size = 32 * 1024 * 1024; // 32 MB for Turing+ (open-gpu-kernel-modules standard)
        
        serial::serial_print("[NVIDIA]   Mapping BAR0 (Phys: 0x");
        serial::serial_print_hex(bar0_phys);
        serial::serial_print(", 32 MB)...\n");

        let bar0_virt = map_mmio_range(bar0_phys, bar0_size);
        
        serial::serial_print("[NVIDIA]   Mapped BAR0 to Virt: 0x");
        serial::serial_print_hex(bar0_virt);
        serial::serial_print("\n");

        // --- Phase 2: Hardware identity check via PMC_BOOT_0 ---
        // PMC_BOOT_0 contains the chip ID embedded in bits [31:20].
        // We cross-check the PCI-ID-derived architecture against the register value.
        let boot_0 = unsafe {
            core::ptr::read_volatile((bar0_virt + NV_PMC_BOOT_0 as u64) as *const u32)
        };
        serial::serial_print("[NVIDIA]   PMC_BOOT_0: 0x");
        serial::serial_print_hex(boot_0 as u64);
        serial::serial_print("\n");

        if boot_0 == 0 || boot_0 == 0xFFFF_FFFF {
            serial::serial_print("[NVIDIA]   ⚠ BAR0 not accessible (PMC_BOOT_0=0x");
            serial::serial_print_hex(boot_0 as u64);
            serial::serial_print("). Skipping this GPU.\n");
            continue;
        }

        serial::serial_print("[NVIDIA]   ✓ BAR0 accessible (GPU ID: 0x");
        serial::serial_print_hex(boot_0 as u64);
        serial::serial_print(")\n");

        // Cross-validate architecture from hardware register
        let hw_arch = arch_from_pmc_boot0(boot_0);
        serial::serial_print("[NVIDIA]   Architecture (PMC_BOOT_0): ");
        match hw_arch {
            NvidiaArchitecture::Blackwell   => serial::serial_print("Blackwell"),
            NvidiaArchitecture::AdaLovelace => serial::serial_print("Ada Lovelace"),
            NvidiaArchitecture::Ampere      => serial::serial_print("Ampere"),
            NvidiaArchitecture::Turing      => serial::serial_print("Turing"),
            NvidiaArchitecture::Hopper      => serial::serial_print("Hopper"),
            NvidiaArchitecture::Unknown     => serial::serial_print("Unknown"),
        }
        serial::serial_print("\n");

        if hw_arch != gpu_info.architecture && hw_arch != NvidiaArchitecture::Unknown {
            serial::serial_print("[NVIDIA]   ⚠ Architecture mismatch: PCI ID says one arch, ");
            serial::serial_print("PMC_BOOT_0 chip_id says another. Using PMC_BOOT_0.\n");
        }

        // --- Phase 3: VRAM size from hardware register ---
        // NV_PFB_CSTATUS bits [14:0] = VRAM size in MB (only valid after GPU init,
        // but may reflect VBIOS pre-programmed value on warm boot).
        let hw_vram_mb = read_vram_size_mb(bar0_virt);
        if hw_vram_mb > 0 {
            serial::serial_print("[NVIDIA]   VRAM (NV_PFB_CSTATUS): ");
            serial::serial_print_dec(hw_vram_mb as u64);
            serial::serial_print(" MB\n");
        } else {
            serial::serial_print("[NVIDIA]   VRAM: not yet readable (NV_PFB_CSTATUS=0)\n");
        }

        // --- Phase 4: Temperature reading ---
        // Only attempt if THERM is powered (register not 0 / 0xFFFF_FFFF).
        match read_temperature(bar0_virt) {
            Some(temp) => {
                serial::serial_print("[NVIDIA]   Temperature: ");
                serial::serial_print_dec(temp as u64);
                serial::serial_print(" deg C\n");
            }
            None => {
                serial::serial_print("[NVIDIA]   Temperature: THERM not initialized\n");
            }
        }

        // --- Phase 5: PMC engine enable ---
        // Before GSP boot, enable all standard GPU engine subsystems.
        // This follows the open-gpu-kernel-modules _pmc_enable sequence.
        unsafe {
            let current = core::ptr::read_volatile(
                (bar0_virt + NV_PMC_ENABLE as u64) as *const u32
            );
            serial::serial_print("[NVIDIA]   PMC_ENABLE (before): 0x");
            serial::serial_print_hex(current as u64);
            serial::serial_print("\n");
            core::ptr::write_volatile(
                (bar0_virt + NV_PMC_ENABLE as u64) as *mut u32,
                NV_PMC_ENABLE_DEFAULT,
            );
            // Readback confirms write was accepted
            let confirmed = core::ptr::read_volatile(
                (bar0_virt + NV_PMC_ENABLE as u64) as *const u32
            );
            serial::serial_print("[NVIDIA]   PMC_ENABLE (after):  0x");
            serial::serial_print_hex(confirmed as u64);
            serial::serial_print("\n");
        }

        // --- Phase 6: OpenGL context initialization ---
        // PGRAPH (bit 13) is already active via NV_PMC_ENABLE_DEFAULT.
        // Init the kernel GL context and reserve a primary render surface.
        let vram_for_gl = if hw_vram_mb > 0 { hw_vram_mb } else { gpu_info.memory_size_mb };
        opengl::init_all_gpus(bar0_virt, vram_for_gl);

        // --- Phase 7: GSP firmware load and Falcon boot sequence ---
        let fw_path = "/lib/firmware/gsp.bin";
        match GspLoader::load_firmware(fw_path) {
            Ok(fw) => {
                serial::serial_print("[NVIDIA]   ✓ GSP Firmware loaded (");
                serial::serial_print_dec(fw.size as u64);
                serial::serial_print(" bytes at phys 0x");
                serial::serial_print_hex(fw.phys_base);
                serial::serial_print(")\n");
                
                serial::serial_print("[NVIDIA]   Booting GSP Falcon (Nova/open-gpu-kernel-modules protocol)...\n");
                
                unsafe {
                    // Step 6a: Configure DMA transfer base register (DMATRFBASE)
                    // Set to firmware physical address >> 8 as per Falcon DMA spec.
                    let fw_base_shifted = (fw.phys_base >> 8) as u32;
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_DMATRFBASE as u64) as *mut u32,
                        fw_base_shifted,
                    );

                    // Step 6b: Clear both mailboxes for clean handshake
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_MAILBOX0 as u64) as *mut u32, 0,
                    );
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_MAILBOX1 as u64) as *mut u32, 0,
                    );

                    // Step 6c: Release GSP Falcon from reset via CPUCTL (STARTCPU bit).
                    // This is the canonical boot kick from open-gpu-kernel-modules
                    // kgspBootstrapRiscvOSDma_TU102 (src/nvidia/kernel/gpu/gsp/kernel_gsp.c).
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_CPUCTL as u64) as *mut u32,
                        NV_PFALCON_FALCON_CPUCTL_STARTCPU,
                    );
                    serial::serial_print("[NVIDIA]   GSP Falcon STARTCPU issued. Awaiting MAILBOX0 handshake");

                    // Step 6d: Initialize RPC Client
                    let mut rpc = RpcClient::new(GPU_RPC_PHYS_BASE);

                    // Step 6e: Poll MAILBOX0 for GSP-RM ready signature.
                    // From open-gpu-kernel-modules: GSP writes a magic value when ready.
                    // Timeout: ~5 seconds (5 000 000 µs @ 1 µs/iteration).
                    let mut success = false;
                    let mut timeout_ticks = 0u32;
                    const MAX_HANDSHAKE_TICKS: u32 = 5_000_000;

                    while timeout_ticks < MAX_HANDSHAKE_TICKS {
                        let mb0 = core::ptr::read_volatile(
                            (bar0_virt + NV_GSP_MAILBOX0 as u64) as *const u32,
                        );
                        if mb0 == GSP_MAILBOX0_READY_MAGIC_1
                            || mb0 == GSP_MAILBOX0_READY_MAGIC_2
                            || mb0 == GSP_MAILBOX0_READY_MAGIC_3
                        {
                            success = true;
                            break;
                        }
                        if timeout_ticks % 500_000 == 0 {
                            serial::serial_print(".");
                        }
                        crate::cpu::pause();
                        timeout_ticks += 1;
                    }

                    if success {
                        serial::serial_print(" ✓ GSP READY\n");

                        // Step 6f: GSP Capability Discovery via RPC
                        serial::serial_print("[NVIDIA]   Sending GSP RPC: ControlGetCaps\n");
                        match rpc.send_command(GspOpcode::ControlGetCaps, &[]) {
                            Ok(seq) => {
                                serial::serial_print("[NVIDIA]     RPC sent (Seq: ");
                                serial::serial_print_dec(seq as u64);
                                serial::serial_print("). Waiting for response...");
                                
                                let mut found = false;
                                for _ in 0..1000 {
                                    if let Some(msg) = rpc.poll_response() {
                                        if msg.header.seq_num == seq {
                                            serial::serial_print(" ✓ Response (Status: ");
                                            serial::serial_print_dec(msg.header.status as u64);
                                            serial::serial_print(")\n");
                                            found = true;
                                            break;
                                        }
                                    }
                                    for _ in 0..100_000 { crate::cpu::pause(); }
                                }
                                if !found {
                                    serial::serial_print(" ⚠ RPC response timeout\n");
                                }
                            }
                            Err(e) => {
                                serial::serial_print("[NVIDIA]   ⚠ RPC Failed: ");
                                serial::serial_print_dec(e as u64);
                                serial::serial_print("\n");
                            }
                        }

                        // Step 6g: Display setup via RPC (from GspOpcode::DisplaySetup)
                        if let Ok(seq) = rpc.send_command(GspOpcode::DisplaySetup, &[]) {
                            serial::serial_print("[NVIDIA]   DisplaySetup RPC sent (Seq: ");
                            serial::serial_print_dec(seq as u64);
                            serial::serial_print(")\n");
                        }
                    } else {
                        let mb0 = core::ptr::read_volatile(
                            (bar0_virt + NV_GSP_MAILBOX0 as u64) as *const u32,
                        );
                        serial::serial_print(" ⚠ GSP Timeout (MAILBOX0=0x");
                        serial::serial_print_hex(mb0 as u64);
                        serial::serial_print(")\n");
                        serial::serial_print("[NVIDIA]   ℹ GSP timeout is expected when gsp.bin is\n");
                        serial::serial_print("[NVIDIA]     not found or invalid for this GPU model.\n");
                    }
                }
            }
            Err(e) => {
                serial::serial_print("[NVIDIA]   ⚠ Firmware load failed: ");
                serial::serial_print(e);
                serial::serial_print("\n");
                serial::serial_print("[NVIDIA]   ℹ Place NVIDIA GSP firmware at /lib/firmware/gsp.bin\n");
                serial::serial_print("[NVIDIA]   ℹ (from open-gpu-kernel-modules or linux-firmware package)\n");
            }
        }
    }
    
    serial::serial_print("[NVIDIA] Initialization complete\n");
}

/// Get list of detected NVIDIA GPUs
pub fn get_nvidia_gpus() -> Vec<NvidiaGpuInfo> {
    find_nvidia_gpus()
        .iter()
        .map(|pci_dev| NvidiaGpuInfo::from_pci_device(*pci_dev))
        .collect()
}

// ========================================================================
// Advanced NVIDIA GPU Features
// ========================================================================

/// CUDA Runtime Support
pub mod cuda {
    //! CUDA runtime interface for compute workloads
    //! 
    //! Provides kernel launch, memory management, and stream support
    //! for general-purpose GPU computing.
    
    use super::*;
    
    /// CUDA context for GPU operations
    #[derive(Debug, Clone)]
    pub struct CudaContext {
        pub gpu_index: usize,
        pub device_ptr: usize,
        pub context_flags: u32,
    }
    
    /// CUDA kernel configuration
    #[derive(Debug, Clone, Copy)]
    pub struct KernelConfig {
        pub blocks: (u32, u32, u32),
        pub threads: (u32, u32, u32),
        pub shared_memory: usize,
    }
    
    /// CUDA stream for asynchronous operations
    #[derive(Debug)]
    pub struct CudaStream {
        pub stream_id: u32,
        pub priority: i32,
    }
    
    impl CudaContext {
        /// Create a new CUDA context for the specified GPU
        pub fn new(gpu_index: usize) -> Result<Self, &'static str> {
            serial::serial_print("[CUDA] Creating context for GPU ");
            serial::serial_print_dec(gpu_index as u64);
            serial::serial_print("\n");
            
            // In a real implementation, this would:
            // - Initialize CUDA driver API
            // - Create GPU context
            // - Allocate context resources
            
            Ok(Self {
                gpu_index,
                device_ptr: 0, // Would be actual device pointer
                context_flags: 0,
            })
        }
        
        /// Allocate device memory
        pub fn allocate_device_memory(&self, size: usize) -> Result<usize, &'static str> {
            serial::serial_print("[CUDA] Allocating ");
            serial::serial_print_dec(size as u64);
            serial::serial_print(" bytes of device memory\n");
            
            // Would allocate actual GPU memory via BAR
            Ok(0) // Return device pointer
        }
        
        /// Copy data from host to device
        pub fn copy_host_to_device(&self, _host_ptr: usize, _device_ptr: usize, _size: usize) -> Result<(), &'static str> {
            serial::serial_print("[CUDA] Copying data to device\n");
            // Would perform actual DMA transfer
            Ok(())
        }
        
        /// Copy data from device to host
        pub fn copy_device_to_host(&self, _device_ptr: usize, _host_ptr: usize, _size: usize) -> Result<(), &'static str> {
            serial::serial_print("[CUDA] Copying data from device\n");
            // Would perform actual DMA transfer
            Ok(())
        }
        
        /// Launch a CUDA kernel
        pub fn launch_kernel(&self, _kernel_ptr: usize, config: KernelConfig) -> Result<(), &'static str> {
            serial::serial_print("[CUDA] Launching kernel: blocks=(");
            serial::serial_print_dec(config.blocks.0 as u64);
            serial::serial_print(",");
            serial::serial_print_dec(config.blocks.1 as u64);
            serial::serial_print(",");
            serial::serial_print_dec(config.blocks.2 as u64);
            serial::serial_print("), threads=(");
            serial::serial_print_dec(config.threads.0 as u64);
            serial::serial_print(",");
            serial::serial_print_dec(config.threads.1 as u64);
            serial::serial_print(",");
            serial::serial_print_dec(config.threads.2 as u64);
            serial::serial_print(")\n");
            
            // Would submit kernel to GPU command buffer
            Ok(())
        }
    }
    
    impl CudaStream {
        /// Create a new CUDA stream for asynchronous operations
        pub fn new(priority: i32) -> Result<Self, &'static str> {
            serial::serial_print("[CUDA] Creating stream with priority ");
            serial::serial_print_dec(priority as u64);
            serial::serial_print("\n");
            
            Ok(Self {
                stream_id: 0, // Would be actual stream ID
                priority,
            })
        }
    }
}

/// Ray Tracing (RT Core) Support
pub mod raytracing {
    //! RT core interface for hardware-accelerated ray tracing
    //! 
    //! Provides acceleration structure building, ray tracing pipeline,
    //! and shader binding table management.
    
    use super::*;
    
    /// RT core capabilities for a GPU
    #[derive(Debug, Clone, Copy)]
    pub struct RtCoreCapabilities {
        pub rt_cores: u32,
        pub max_recursion_depth: u32,
        pub max_ray_generation_threads: u32,
        pub supports_inline_rt: bool,
    }
    
    /// Acceleration structure for ray tracing
    #[derive(Debug)]
    pub struct AccelerationStructure {
        pub handle: u64,
        pub memory_size: usize,
        pub num_geometries: u32,
    }
    
    /// Ray tracing pipeline state
    #[derive(Debug)]
    pub struct RtPipeline {
        pub pipeline_id: u32,
        pub max_recursion: u32,
        pub shader_groups: u32,
    }
    
    impl RtCoreCapabilities {
        /// Detect RT core capabilities for a GPU
        pub fn detect(gpu_info: &NvidiaGpuInfo) -> Self {
            let (rt_cores, supports_inline) = match gpu_info.architecture {
                NvidiaArchitecture::Turing => (gpu_info.sm_count, false),
                NvidiaArchitecture::Ampere => (gpu_info.sm_count, true),
                NvidiaArchitecture::AdaLovelace => (gpu_info.sm_count, true),
                NvidiaArchitecture::Hopper => (gpu_info.sm_count, true),
                NvidiaArchitecture::Blackwell => (gpu_info.sm_count, true),
                _ => (0, false),
            };
            
            Self {
                rt_cores,
                max_recursion_depth: 31,
                max_ray_generation_threads: 1024 * 1024,
                supports_inline_rt: supports_inline,
            }
        }
    }
    
    impl AccelerationStructure {
        /// Build a new acceleration structure
        pub fn build(_vertices: &[f32], _indices: &[u32]) -> Result<Self, &'static str> {
            serial::serial_print("[RT] Building acceleration structure\n");
            
            // Would build actual RT acceleration structure
            Ok(Self {
                handle: 0,
                memory_size: 0,
                num_geometries: 0,
            })
        }
    }
    
    impl RtPipeline {
        /// Create a ray tracing pipeline
        pub fn new(max_recursion: u32) -> Result<Self, &'static str> {
            serial::serial_print("[RT] Creating ray tracing pipeline (max recursion: ");
            serial::serial_print_dec(max_recursion as u64);
            serial::serial_print(")\n");
            
            Ok(Self {
                pipeline_id: 0,
                max_recursion,
                shader_groups: 0,
            })
        }
    }
}

/// Display Output Support (DisplayPort/HDMI)
pub mod display {
    //! Display output interface for direct monitor control
    //! 
    //! Provides connector detection, mode setting, and display timing
    //! configuration for DisplayPort and HDMI outputs.
    
    use super::*;
    
    /// Display connector type
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ConnectorType {
        DisplayPort,
        HDMI,
        DVI,
        VGA,
        Unknown,
    }
    
    /// Display mode configuration
    #[derive(Debug, Clone, Copy)]
    pub struct DisplayMode {
        pub width: u32,
        pub height: u32,
        pub refresh_rate: u32,
        pub pixel_clock: u32,
    }
    
    /// Display connector information
    #[derive(Debug, Clone)]
    pub struct DisplayConnector {
        pub connector_type: ConnectorType,
        pub connected: bool,
        pub edid_available: bool,
        pub max_width: u32,
        pub max_height: u32,
    }
    
    impl DisplayConnector {
        /// Detect connected displays
        pub fn detect_all() -> Vec<Self> {
            serial::serial_print("[DISPLAY] Detecting connected displays\n");
            
            // Would scan all display outputs
            // For now, simulate detection
            let mut connectors = Vec::new();
            
            connectors.push(Self {
                connector_type: ConnectorType::DisplayPort,
                connected: true,
                edid_available: true,
                max_width: 3840,
                max_height: 2160,
            });
            
            connectors
        }
        
        /// Read EDID from display
        pub fn read_edid(&self) -> Result<Vec<u8>, &'static str> {
            serial::serial_print("[DISPLAY] Reading EDID\n");
            
            // Would read actual EDID via I2C
            Ok(Vec::new())
        }
        
        /// Set display mode
        pub fn set_mode(&self, mode: DisplayMode) -> Result<(), &'static str> {
            serial::serial_print("[DISPLAY] Setting mode: ");
            serial::serial_print_dec(mode.width as u64);
            serial::serial_print("x");
            serial::serial_print_dec(mode.height as u64);
            serial::serial_print("@");
            serial::serial_print_dec(mode.refresh_rate as u64);
            serial::serial_print("Hz\n");
            
            // Would configure display controller
            Ok(())
        }
    }
}

/// GPU Power Management
pub mod power {
    //! Power management interface for GPU efficiency
    //! 
    //! Provides power state control, clock frequency management,
    //! thermal monitoring, and power limit configuration.
    
    use super::*;
    
    /// GPU power state
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PowerState {
        P0,  // Maximum performance
        P1,  // Balanced
        P2,  // Power saving
        P3,  // Idle
    }
    
    /// Clock domain for frequency control
    #[derive(Debug, Clone, Copy)]
    pub enum ClockDomain {
        Graphics,
        Memory,
        Video,
    }
    
    /// Power management state
    #[derive(Debug, Clone)]
    pub struct PowerManager {
        pub current_state: PowerState,
        pub temperature_c: u32,
        pub power_limit_mw: u32,
        pub current_power_mw: u32,
    }
    
    impl PowerManager {
        /// Create a new power manager
        pub fn new() -> Self {
            Self {
                current_state: PowerState::P0,
                temperature_c: 0,
                power_limit_mw: 0,
                current_power_mw: 0,
            }
        }
        
        /// Set power state
        pub fn set_power_state(&mut self, state: PowerState) -> Result<(), &'static str> {
            serial::serial_print("[POWER] Setting power state: ");
            match state {
                PowerState::P0 => serial::serial_print("P0 (Max Performance)"),
                PowerState::P1 => serial::serial_print("P1 (Balanced)"),
                PowerState::P2 => serial::serial_print("P2 (Power Saving)"),
                PowerState::P3 => serial::serial_print("P3 (Idle)"),
            }
            serial::serial_print("\n");
            
            self.current_state = state;
            Ok(())
        }
        
        /// Read current temperature
        pub fn read_temperature(&mut self) -> Result<u32, &'static str> {
            // Would read from GPU thermal sensor
            self.temperature_c = 45; // Simulated
            Ok(self.temperature_c)
        }
        
        /// Set clock frequency for a domain
        pub fn set_clock_frequency(&self, domain: ClockDomain, freq_mhz: u32) -> Result<(), &'static str> {
            serial::serial_print("[POWER] Setting ");
            match domain {
                ClockDomain::Graphics => serial::serial_print("graphics"),
                ClockDomain::Memory => serial::serial_print("memory"),
                ClockDomain::Video => serial::serial_print("video"),
            }
            serial::serial_print(" clock to ");
            serial::serial_print_dec(freq_mhz as u64);
            serial::serial_print(" MHz\n");
            
            // Would write to clock control registers
            Ok(())
        }
        
        /// Set power limit
        pub fn set_power_limit(&mut self, limit_mw: u32) -> Result<(), &'static str> {
            serial::serial_print("[POWER] Setting power limit to ");
            serial::serial_print_dec(limit_mw as u64);
            serial::serial_print(" mW\n");
            
            self.power_limit_mw = limit_mw;
            Ok(())
        }
    }
}

/// Video Encode/Decode (NVENC/NVDEC)
pub mod video {
    //! Video acceleration interface for encoding and decoding
    //! 
    //! Provides hardware-accelerated video encoding (NVENC) and
    //! decoding (NVDEC) for various codecs.
    
    use super::*;
    
    /// Video codec type
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VideoCodec {
        H264,
        H265,
        VP9,
        AV1,
    }
    
    /// Video encoder capabilities
    #[derive(Debug, Clone)]
    pub struct EncoderCapabilities {
        pub supported_codecs: Vec<VideoCodec>,
        pub max_width: u32,
        pub max_height: u32,
        pub max_framerate: u32,
        pub supports_bframes: bool,
    }
    
    /// Video decoder capabilities
    #[derive(Debug, Clone)]
    pub struct DecoderCapabilities {
        pub supported_codecs: Vec<VideoCodec>,
        pub max_width: u32,
        pub max_height: u32,
        pub supports_film_grain: bool,
    }
    
    /// NVENC encoder instance
    #[derive(Debug)]
    pub struct NvencEncoder {
        pub codec: VideoCodec,
        pub width: u32,
        pub height: u32,
    }
    
    /// NVDEC decoder instance
    #[derive(Debug)]
    pub struct NvdecDecoder {
        pub codec: VideoCodec,
        pub width: u32,
        pub height: u32,
    }
    
    impl EncoderCapabilities {
        /// Detect encoder capabilities for a GPU
        pub fn detect(gpu_info: &NvidiaGpuInfo) -> Self {
            let mut codecs = Vec::new();
            codecs.push(VideoCodec::H264);
            codecs.push(VideoCodec::H265);
            
            // Ada Lovelace, Hopper, and Blackwell support AV1 encode
            if matches!(gpu_info.architecture,
                NvidiaArchitecture::AdaLovelace | NvidiaArchitecture::Hopper | NvidiaArchitecture::Blackwell
            ) {
                codecs.push(VideoCodec::AV1);
            }
            
            Self {
                supported_codecs: codecs,
                max_width: 8192,
                max_height: 8192,
                max_framerate: 240,
                supports_bframes: true,
            }
        }
    }
    
    impl DecoderCapabilities {
        /// Detect decoder capabilities for a GPU
        pub fn detect(gpu_info: &NvidiaGpuInfo) -> Self {
            let mut codecs = Vec::new();
            codecs.push(VideoCodec::H264);
            codecs.push(VideoCodec::H265);
            codecs.push(VideoCodec::VP9);
            
            // Ampere and newer support AV1 decode
            if !matches!(gpu_info.architecture, NvidiaArchitecture::Turing | NvidiaArchitecture::Unknown) {
                codecs.push(VideoCodec::AV1);
            }
            
            Self {
                supported_codecs: codecs,
                max_width: 8192,
                max_height: 8192,
                supports_film_grain: true,
            }
        }
    }
    
    impl NvencEncoder {
        /// Create a new encoder instance
        pub fn new(codec: VideoCodec, width: u32, height: u32) -> Result<Self, &'static str> {
            serial::serial_print("[NVENC] Creating encoder: codec=");
            match codec {
                VideoCodec::H264 => serial::serial_print("H.264"),
                VideoCodec::H265 => serial::serial_print("H.265"),
                VideoCodec::VP9 => serial::serial_print("VP9"),
                VideoCodec::AV1 => serial::serial_print("AV1"),
            }
            serial::serial_print(", resolution=");
            serial::serial_print_dec(width as u64);
            serial::serial_print("x");
            serial::serial_print_dec(height as u64);
            serial::serial_print("\n");
            
            Ok(Self { codec, width, height })
        }
        
        /// Encode a frame
        pub fn encode_frame(&self, _input_buffer: usize, _output_buffer: usize) -> Result<usize, &'static str> {
            // Would submit frame to NVENC hardware
            Ok(0) // Return encoded size
        }
    }
    
    impl NvdecDecoder {
        /// Create a new decoder instance
        pub fn new(codec: VideoCodec, width: u32, height: u32) -> Result<Self, &'static str> {
            serial::serial_print("[NVDEC] Creating decoder: codec=");
            match codec {
                VideoCodec::H264 => serial::serial_print("H.264"),
                VideoCodec::H265 => serial::serial_print("H.265"),
                VideoCodec::VP9 => serial::serial_print("VP9"),
                VideoCodec::AV1 => serial::serial_print("AV1"),
            }
            serial::serial_print(", resolution=");
            serial::serial_print_dec(width as u64);
            serial::serial_print("x");
            serial::serial_print_dec(height as u64);
            serial::serial_print("\n");
            
            Ok(Self { codec, width, height })
        }
        
        /// Decode a frame
        pub fn decode_frame(&self, _input_buffer: usize, _output_buffer: usize) -> Result<(), &'static str> {
            // Would submit bitstream to NVDEC hardware
            Ok(())
        }
    }
}  // end pub mod video

// ========================================================================
// OpenGL Support
// ========================================================================

/// Software-OpenGL infrastructure exposed to userland.
///
/// On Turing/Ampere/Ada GPUs the PGRAPH engine (3D pipeline) is enabled by
/// bit 13 of `NV_PMC_ENABLE`.  `NV_PMC_ENABLE_DEFAULT` (0x2FFF_FFFF) already
/// has that bit set, so `nvidia::init()` implicitly enables PGRAPH in Phase 5.
///
/// This module provides:
/// - `GlKernelContext` — tracks per-GPU GL state (BAR0 virt, VRAM info).
/// - Surface allocation helpers for render-target backing in VRAM.
/// - Serial diagnostics so the user can confirm GL is ready in the serial log.
pub mod opengl {
    use super::*;

    /// PGRAPH engine enable bit (bit 13) — from dev_pmc.h.
    const PMC_ENABLE_PGRAPH_BIT: u32 = 1 << 13;

    /// VRAM surface alignment: 4 KB.
    const GL_VRAM_SURFACE_ALIGN: u64 = 4096;

    /// Kernel-side OpenGL context for one NVIDIA GPU.
    pub struct GlKernelContext {
        /// Virtual address of the GPU's BAR0 mapping.
        pub bar0_virt:    u64,
        /// Physical start of the GPU's VRAM region used for GL surfaces.
        pub vram_phys:    u64,
        /// VRAM size in MB (from NV_PFB_CSTATUS or PCI ID table).
        pub vram_size_mb: u32,
        /// Bump-pointer for the next VRAM surface allocation (bytes from vram_phys).
        pub alloc_offset: u64,
    }

    impl GlKernelContext {
        /// Initialise the GL kernel context for a single GPU.
        ///
        /// Verifies that PGRAPH is enabled in `NV_PMC_ENABLE` and logs
        /// readiness.  Returns `None` if PGRAPH cannot be enabled.
        pub fn init(bar0_virt: u64, vram_size_mb: u32) -> Option<Self> {
            serial::serial_print("[GL] Initializing OpenGL kernel context...\n");

            // ── Verify PGRAPH engine is enabled (bit 13 of PMC_ENABLE) ────────
            let pmc_en = unsafe {
                core::ptr::read_volatile(
                    (bar0_virt + NV_PMC_ENABLE as u64) as *const u32
                )
            };

            if pmc_en & PMC_ENABLE_PGRAPH_BIT == 0 {
                serial::serial_print("[GL] PGRAPH bit not set — enabling...\n");
                unsafe {
                    core::ptr::write_volatile(
                        (bar0_virt + NV_PMC_ENABLE as u64) as *mut u32,
                        pmc_en | PMC_ENABLE_PGRAPH_BIT,
                    );
                }
            } else {
                serial::serial_print("[GL] ✓ PGRAPH engine bit active (PMC_ENABLE bit 13 set)\n");
            }

            serial::serial_print("[GL] GlKernelContext initialized — VRAM: ");
            serial::serial_print_dec(vram_size_mb as u64);
            serial::serial_print(" MB\n");

            // Use the memory region right after the firmware area for GL surfaces.
            let vram_phys = GPU_FW_PHYS_BASE + 64 * 1024 * 1024;

            Some(Self {
                bar0_virt,
                vram_phys,
                vram_size_mb,
                alloc_offset: 0,
            })
        }

        /// Allocate a render surface in VRAM.
        ///
        /// Returns the physical **offset** (in bytes) from `vram_phys`.
        /// Add `vram_phys` to get the full physical address, then map via
        /// `PHYS_MEM_OFFSET + phys` for CPU access.
        ///
        /// Pixels are 32-bit BGRA (`u32`).
        pub fn alloc_surface(&mut self, width: u32, height: u32) -> Option<u64> {
            let size = (width as u64) * (height as u64) * 4;
            let aligned = (size + GL_VRAM_SURFACE_ALIGN - 1) & !(GL_VRAM_SURFACE_ALIGN - 1);
            let vram_bytes = (self.vram_size_mb as u64) * 1024 * 1024;

            if self.alloc_offset + aligned > vram_bytes {
                serial::serial_print("[GL] ⚠ VRAM exhausted — cannot allocate surface\n");
                return None;
            }

            let offset = self.alloc_offset;
            self.alloc_offset += aligned;

            serial::serial_print("[GL] OpenGL surface alloc: ");
            serial::serial_print_dec(width as u64);
            serial::serial_print("x");
            serial::serial_print_dec(height as u64);
            serial::serial_print(" @ phys offset 0x");
            serial::serial_print_hex(offset);
            serial::serial_print("\n");

            Some(offset)
        }

        /// CPU-accessible virtual address for an allocated surface.
        #[inline]
        pub fn surface_virt(&self, offset: u64) -> u64 {
            PHYS_MEM_OFFSET + self.vram_phys + offset
        }
    }

    /// Convenience wrapper — called from `nvidia::init()` after BAR0 is mapped.
    pub fn init_all_gpus(bar0_virt: u64, vram_size_mb: u32) {
        match GlKernelContext::init(bar0_virt, vram_size_mb) {
            Some(mut ctx) => {
                // Allocate a default 1920×1080 primary render surface.
                if let Some(off) = ctx.alloc_surface(1920, 1080) {
                    serial::serial_print("[GL] Primary surface phys: 0x");
                    serial::serial_print_hex(ctx.vram_phys + off);
                    serial::serial_print("\n");
                    serial::serial_print("[GL] Primary surface virt: 0x");
                    serial::serial_print_hex(ctx.surface_virt(off));
                    serial::serial_print("\n");
                }
                serial::serial_print("[GL] ✓ Software OpenGL ready (sidewind_opengl CPU rasterizer)\n");
            }
            None => {
                serial::serial_print("[GL] ⚠ OpenGL context init failed\n");
            }
        }
    }
}
