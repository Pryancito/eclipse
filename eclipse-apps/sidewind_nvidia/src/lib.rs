#![no_std]

//! sidewind_nvidia - NVIDIA GPU abstraction (Nova-aligned)
//!
//! Hardware abstraction layer for GSP-based NVIDIA GPUs, aligned with the
//! **Nova** driver (Linux kernel: nova-core / nova-drm).
//!
//! Provides protocol definitions for GSP (GPU System Processor) RPC and
//! BAR0 register layouts.

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
    //! Common NVIDIA Register Offsets (BAR0)
    //! Aligned with Nova / open-gpu-kernel-modules.
    
    /// PMC (Power Management Controller)
    pub const NV_PMC_BOOT_0: u32 = 0x00000000;
    pub const NV_PMC_INTR_0: u32 = 0x00000100;
    pub const NV_PMC_INTR_EN_0: u32 = 0x00000140;

    /// GSP Mailbox registers (architecture specific)
    /// These are common on Turing/Ampere
    pub const NV_GSP_MAILBOX_IN: u32 = 0x00118000;
    pub const NV_GSP_MAILBOX_OUT: u32 = 0x00118004;
    pub const NV_GSP_DOORBELL: u32 = 0x00118008;

    /// GSP Boot Controls
    pub const NV_GSP_FW_PHYS_ADDR_LO: u32 = 0x00118010;
    pub const NV_GSP_FW_PHYS_ADDR_HI: u32 = 0x00118014;
    pub const NV_GSP_FW_SIZE: u32 = 0x00118018;
    pub const NV_GSP_CTRL: u32 = 0x0011801C;
    
    pub const NV_GSP_CTRL_RUN: u32 = 1 << 0;
    pub const NV_GSP_CTRL_RESET: u32 = 1 << 1;
}
