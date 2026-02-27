#![no_std]
extern crate alloc;

/// sidewind_nvidia - NVIDIA GPU abstraction (Nova-aligned)
///
/// Hardware abstraction layer for GSP-based NVIDIA GPUs, aligned with the
/// **Nova** driver (Linux kernel: nova-core / nova-drm).
///
/// Provides protocol definitions for GSP (GPU System Processor) RPC and
/// BAR0 register layouts.

pub mod gsp {
    /// GSP Status Codes
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u32)]
    pub enum GspStatus {
        Ok = 0,
        Error = 1,
        Busy = 2,
        Timeout = 3,
        InvalidArg = 4,
        NotSupported = 5,
        NoMemory = 6,
        InvalidState = 7,
        HardwareError = 8,
    }

    /// RPC Opcode for GSP messages (Nova-aligned)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u32)]
    pub enum GspOpcode {
        SystemGetBuildInfo = 0x01,
        ControlGetCaps = 0x02,
        ControlGetGpuInfo = 0x03,
        
        // GPU Lifecycle
        GspLoadGpuGroup = 0x08,
        GspUnloadGpuGroup = 0x09,
        
        // Resource Management
        MemoryAllocate = 0x10,
        MemoryFree = 0x11,
        MemoryMap = 0x12,
        MemoryUnmap = 0x13,
        
        // Engine Initialization
        DisplaySetup = 0x20,
        GraphicsInit = 0x30,
        /// OpenGL context creation (software GL handshake with GSP)
        OpenGLContextCreate = 0x31,
        /// Map a VRAM surface for an OpenGL render target
        OpenGLSurfaceMap = 0x32,
        ComputeInit = 0x40,
        VideoInit = 0x50,
        
        // Debug/Diagnostic
        InternalPoll = 0xFFFF,
    }

    /// Common GSP RPC Header
    #[derive(Debug, Clone, Copy)]
    #[repr(C)]
    pub struct GspHeader {
        pub opcode: u32,
        pub seq_num: u32,
        pub status: u32,
        pub payload_len: u32,
    }

    /// Generic GSP RPC Message
    #[derive(Clone)]
    #[repr(C)]
    pub struct GspMessage<const P: usize> {
        pub header: GspHeader,
        pub payload: [u8; P],
    }

    /// Payload for ControlGetCaps
    #[repr(C)]
    pub struct GspCapsPayload {
        pub count: u32,
        pub caps: [u32; 32],
    }

    /// Payload for MemoryAllocate
    #[repr(C)]
    pub struct GspMemAllocPayload {
        pub size: u64,
        pub alignment: u64,
        pub type_flags: u32,
        pub unused: u32,
    }

    /// Payload for OpenGL context creation (OpenGLContextCreate RPC).
    /// Sent to the GSP to announce that a software-GL context is active
    /// and to request a VRAM surface mapping.
    #[derive(Debug, Clone, Copy)]
    #[repr(C)]
    pub struct GspOpenGLPayload {
        /// Desired render-target width in pixels.
        pub width: u32,
        /// Desired render-target height in pixels.
        pub height: u32,
        /// Pixel format: 0 = BGRA8, 1 = RGBA8.
        pub format: u32,
        /// Reserved / padding.
        pub _pad: u32,
    }

    /// GSP Mailbox layout (usually in BAR0)
    pub struct GspMailbox {
        pub in_ptr: *mut u32,
        pub out_ptr: *const u32,
        pub doorbell: *mut u32,
    }

    /// GSP RPC Queue Control (Shared Memory)
    #[derive(Debug, Clone, Copy)]
    #[repr(C)]
    pub struct GspRpcControl {
        pub put: u32,
        pub get: u32,
        pub capacity: u32,
        pub unused: u32,
    }

    /// Maximum entries in a GSP RPC queue
    pub const GSP_RPC_QUEUE_SIZE: usize = 128;
    pub const GSP_RPC_PAYLOAD_SIZE: usize = 256;

    /// A functional GSP RPC Queue
    #[repr(C)]
    pub struct GspRpcQueue {
        pub control: GspRpcControl,
        pub messages: [GspMessage<GSP_RPC_PAYLOAD_SIZE>; GSP_RPC_QUEUE_SIZE],
    }

    impl GspRpcQueue {
        /// Initialize a new RPC queue in-place
        pub unsafe fn init_at(ptr: *mut Self) {
            let q = &mut *ptr;
            q.control.put = 0;
            q.control.get = 0;
            q.control.capacity = GSP_RPC_QUEUE_SIZE as u32;
        }

        /// Check if the queue is full
        pub fn is_full(&self) -> bool {
            ((self.control.put + 1) % self.control.capacity) == self.control.get
        }

        /// Check if the queue is empty
        pub fn is_empty(&self) -> bool {
            self.control.put == self.control.get
        }

        /// Push a command to the queue
        pub fn push(&mut self, header: GspHeader, payload: &[u8]) -> Result<(), GspStatus> {
            if self.is_full() {
                return Err(GspStatus::Busy);
            }

            let idx = self.control.put as usize;
            self.messages[idx].header = header;
            
            // Fixed-size copy to payload buffer
            let len = core::cmp::min(payload.len(), GSP_RPC_PAYLOAD_SIZE);
            self.messages[idx].payload[..len].copy_from_slice(&payload[..len]);
            
            self.control.put = (self.control.put + 1) % self.control.capacity;
            Ok(())
        }

        /// Pop a response from the queue
        pub fn pop(&mut self) -> Option<GspMessage<GSP_RPC_PAYLOAD_SIZE>> {
            if self.is_empty() {
                return None;
            }

            let idx = self.control.get as usize;
            let msg = self.messages[idx].clone();
            
            self.control.get = (self.control.get + 1) % self.control.capacity;
            Some(msg)
        }
    }
}

pub mod registers {
    //! NVIDIA BAR0 register offsets aligned with open-gpu-kernel-modules.
    //!
    //! Sources (NVIDIA open-gpu-kernel-modules):
    //!   src/nvidia/arch/nvalloc/chips/tu102/inc/published/tu102/dev_pmc.h
    //!   src/nvidia/arch/nvalloc/chips/tu102/inc/published/tu102/dev_falcon_v4.h
    //!   src/nvidia/arch/nvalloc/chips/tu102/inc/published/tu102/dev_fb.h
    //!   src/nvidia/arch/nvalloc/chips/tu102/inc/published/tu102/dev_therm.h

    // -----------------------------------------------------------------------
    // PMC (Power Management Controller)  — dev_pmc.h
    // -----------------------------------------------------------------------

    /// GPU identification (chip architecture, implementation, revision).
    /// Bits [31:20] = chip_id (12 bits).  Bits [27:24] = architecture family:
    ///   0x6 = Turing (TU1xx), 0x7 = Ampere (GA1xx/GA2xx),
    ///   0x9 = Ada Lovelace (AD1xx), 0xB = Hopper (GH1xx).
    ///   Blackwell (GB2xx): chip_id[31:20] >= 0x200.
    pub const NV_PMC_BOOT_0: u32 = 0x0000_0000;

    /// PMC engine enable/disable bitmask.
    /// Set the corresponding engine bit to 1 to enable it before GSP boot.
    /// Key bits (from dev_pmc.h / open-gpu-kernel-modules):
    ///   bit  0 = PMASTER
    ///   bit  4 = PTIMER
    ///   bit 12 = PFIFO (channel dispatch)
    ///   bit 13 = PGRAPH (3D/compute)
    ///   bit 20 = PMC_ENABLE_BIT_PDISP (display)
    ///   bit 28 = PFB (framebuffer)
    pub const NV_PMC_ENABLE: u32 = 0x0000_0200;
    /// Enable all standard engines on Turing/Ampere/Ada hardware.
    /// From open-gpu-kernel-modules `nv_pmc_enable_mask` defaults.
    /// Bit layout of 0x2FFF_FFFF:
    ///   bit 0  = PMASTER (master enable), bit 4  = PTIMER (timer),
    ///   bit 12 = PFIFO (channel dispatch), bit 13 = PGRAPH (3D/compute),
    ///   bit 20 = PDISP (display), bit 28 = PFB (framebuffer controller).
    ///   All remaining set bits enable sub-engines as per dev_pmc.h.
    pub const NV_PMC_ENABLE_DEFAULT: u32 = 0x2FFF_FFFF;

    pub const NV_PMC_INTR_0: u32 = 0x0000_0100;
    pub const NV_PMC_INTR_EN_0: u32 = 0x0000_0140;

    // PMC_BOOT_0 architecture discriminant helpers
    // chip_id = (PMC_BOOT_0 >> 20) & 0xFFF
    // arch_major = chip_id >> 4  (0x16=Turing, 0x17=Ampere, 0x19=Ada, 0x1B=Hopper)
    pub const PMC_BOOT0_CHIP_ID_SHIFT: u32 = 20;
    pub const PMC_BOOT0_CHIP_ID_MASK: u32 = 0xFFF;
    /// chip_id range for Turing (TU102..TU116): 0x160..=0x16F
    pub const PMC_BOOT0_CHIPID_TURING_MIN: u32 = 0x160;
    pub const PMC_BOOT0_CHIPID_TURING_MAX: u32 = 0x16F;
    /// chip_id range for Ampere (GA102..GA107): 0x170..=0x17F
    pub const PMC_BOOT0_CHIPID_AMPERE_MIN: u32 = 0x170;
    pub const PMC_BOOT0_CHIPID_AMPERE_MAX: u32 = 0x17F;
    /// chip_id range for Ada Lovelace (AD102..AD107): 0x190..=0x19F
    pub const PMC_BOOT0_CHIPID_ADA_MIN: u32 = 0x190;
    pub const PMC_BOOT0_CHIPID_ADA_MAX: u32 = 0x19F;
    /// chip_id range for Hopper (GH100): 0x1B0..=0x1BF
    pub const PMC_BOOT0_CHIPID_HOPPER_MIN: u32 = 0x1B0;
    pub const PMC_BOOT0_CHIPID_HOPPER_MAX: u32 = 0x1BF;
    /// chip_id range for Blackwell (GB202+): 0x200..=0x2FF
    pub const PMC_BOOT0_CHIPID_BLACKWELL_MIN: u32 = 0x200;

    // -----------------------------------------------------------------------
    // PFB (Physical Frame Buffer) — dev_fb.h
    // Provides VRAM size in units of 1 MB.
    // -----------------------------------------------------------------------

    /// PFB Controller Status register.  Bits [14:0] = VRAM size in MB.
    /// NV_PFB_CSTATUS, from dev_fb.h (GA/TU architectures).
    pub const NV_PFB_CSTATUS: u32 = 0x0010_020C;
    pub const NV_PFB_CSTATUS_MEM_SIZE_MASK: u32 = 0x7FFF; // bits [14:0]

    // -----------------------------------------------------------------------
    // THERM (Thermal Sensor Engine) — dev_therm.h (Turing / Ampere)
    // -----------------------------------------------------------------------

    /// Raw thermal sensor reading.
    /// Bits [8:0] = temperature in Celsius (two's complement for negative).
    /// NV_THERM_TEMP, from dev_therm.h (TU102 and later).
    pub const NV_THERM_TEMP: u32 = 0x0002_0400;
    pub const NV_THERM_TEMP_VALUE_MASK: u32 = 0x1FF; // bits [8:0] (signed 9-bit)
    pub const NV_THERM_TEMP_VALUE_SIGN_BIT: u32 = 0x100;

    /// Thermal interrupt status register.
    pub const NV_THERM_INT_STATUS: u32 = 0x0002_0004;

    // -----------------------------------------------------------------------
    // GSP / Falcon processor — dev_falcon_v4.h
    //
    // On Turing/Ampere/Ada, the GSP Falcon lives at BAR0 + 0x00118000.
    // All offsets below are RELATIVE to this GSP Falcon base.
    // -----------------------------------------------------------------------

    /// Base offset of the GSP Falcon within BAR0 (Turing/Ampere/Ada/Hopper).
    pub const NV_GSP_FALCON_BASE: u32 = 0x0011_8000;

    // Falcon mailboxes used for GSP↔driver handshake.
    // From dev_falcon_v4.h:  NV_PFALCON_FALCON_MAILBOX0 = 0x40, MAILBOX1 = 0x44.
    pub const NV_PFALCON_FALCON_MAILBOX0: u32 = 0x40;
    pub const NV_PFALCON_FALCON_MAILBOX1: u32 = 0x44;

    // Computed absolute BAR0 offsets for the GSP Falcon mailboxes.
    pub const NV_GSP_MAILBOX0: u32 = NV_GSP_FALCON_BASE + NV_PFALCON_FALCON_MAILBOX0; // 0x118040
    pub const NV_GSP_MAILBOX1: u32 = NV_GSP_FALCON_BASE + NV_PFALCON_FALCON_MAILBOX1; // 0x118044

    // GSP CPU control register (used to release GSP Falcon from reset and start it).
    // NV_PFALCON_FALCON_CPUCTL = 0x100.  Bit 2 = STARTCPU, bit 1 = HRESET.
    pub const NV_PFALCON_FALCON_CPUCTL: u32 = 0x100;
    pub const NV_GSP_CPUCTL: u32 = NV_GSP_FALCON_BASE + NV_PFALCON_FALCON_CPUCTL; // 0x118100
    pub const NV_PFALCON_FALCON_CPUCTL_STARTCPU: u32 = 1 << 2;
    pub const NV_PFALCON_FALCON_CPUCTL_HRESET: u32 = 1 << 1;

    // GSP Falcon DMA transfer registers (for loading firmware into Falcon IMEM/DMEM).
    // NV_PFALCON_FALCON_DMATRFBASE  = 0x110 — Physical base address (>> 8).
    // NV_PFALCON_FALCON_DMATRFMOFFS = 0x114 — Falcon memory offset.
    // NV_PFALCON_FALCON_DMATRFCMD   = 0x118 — DMA command/control word.
    // NV_PFALCON_FALCON_DMATRFFBOFFS= 0x11C — FB (DRAM) offset.
    pub const NV_PFALCON_FALCON_DMATRFBASE: u32 = 0x110;
    pub const NV_PFALCON_FALCON_DMATRFMOFFS: u32 = 0x114;
    pub const NV_PFALCON_FALCON_DMATRFCMD: u32 = 0x118;
    pub const NV_PFALCON_FALCON_DMATRFFBOFFS: u32 = 0x11C;
    pub const NV_GSP_DMATRFBASE: u32 = NV_GSP_FALCON_BASE + NV_PFALCON_FALCON_DMATRFBASE;
    pub const NV_GSP_DMATRFMOFFS: u32 = NV_GSP_FALCON_BASE + NV_PFALCON_FALCON_DMATRFMOFFS;
    pub const NV_GSP_DMATRFCMD: u32 = NV_GSP_FALCON_BASE + NV_PFALCON_FALCON_DMATRFCMD;
    pub const NV_GSP_DMATRFFBOFFS: u32 = NV_GSP_FALCON_BASE + NV_PFALCON_FALCON_DMATRFFBOFFS;
    /// NV_PFALCON_FALCON_DMATRFCMD: set this bit to initiate IMEM/DMEM DMA transfer.
    pub const NV_PFALCON_DMATRFCMD_IMEM: u32 = 1 << 4;
    pub const NV_PFALCON_DMATRFCMD_WRITE: u32 = 0; // write to Falcon IMEM/DMEM
    pub const NV_PFALCON_DMATRFCMD_BUSY: u32 = 1 << 1; // set while DMA is in progress

    /// Legacy aliases (kept for compatibility with existing nvidia.rs code)
    pub const NV_GSP_MAILBOX_IN: u32 = NV_GSP_MAILBOX0;
    pub const NV_GSP_MAILBOX_OUT: u32 = NV_GSP_MAILBOX1;
    /// Falcon doorbell register (NV_PFALCON_FALCON_DMACTL + 8 offset).
    /// Writing to this register signals the GSP Falcon that a new message is available
    /// in the shared RPC queue (used by open-gpu-kernel-modules kgspDoorbell_TU102).
    pub const NV_GSP_DOORBELL: u32 = NV_GSP_FALCON_BASE + 0x8;

    /// Legacy GSP boot control aliases (not used in Falcon flow but kept for fallback).
    pub const NV_GSP_FW_PHYS_ADDR_LO: u32 = NV_GSP_FALCON_BASE + 0x10;
    pub const NV_GSP_FW_PHYS_ADDR_HI: u32 = NV_GSP_FALCON_BASE + 0x14;
    pub const NV_GSP_FW_SIZE: u32 = NV_GSP_FALCON_BASE + 0x18;
    pub const NV_GSP_CTRL: u32 = NV_GSP_FALCON_BASE + 0x1C;
    pub const NV_GSP_CTRL_RUN: u32 = 1 << 0;
    pub const NV_GSP_CTRL_RESET: u32 = 1 << 1;

    // -----------------------------------------------------------------------
    // GSP-RM handshake magic values (from open-gpu-kernel-modules)
    // -----------------------------------------------------------------------

    /// GSP firmware signals "ready" by writing one of these to MAILBOX0.
    /// Observed values across driver versions; see kernel_gsp.h.
    pub const GSP_MAILBOX0_READY_MAGIC_1: u32 = 0x5245_4144; // "READ"
    pub const GSP_MAILBOX0_READY_MAGIC_2: u32 = 0x4753_5052; // "GSPR"
    pub const GSP_MAILBOX0_READY_MAGIC_3: u32 = 0xFFFF_2222; // driver ver >= 515
}

pub mod features;