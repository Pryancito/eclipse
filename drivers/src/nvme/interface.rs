use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
// use core::mem::size_of;
use alloc::sync::Arc;
use core::ptr::{read_volatile, write_volatile};

use crate::scheme::{BlockScheme, Scheme};
use crate::{Device, DeviceError, DeviceResult};
use crate::bus::pci_drivers::PciDriver;
use crate::builder::IoMapper;
use pci::{PCIDevice, BAR};

use lock::Mutex;

use super::nvme_queue::*;

pub struct NvmeInterface {
    name: String,

    admin_queue: Arc<Mutex<NvmeQueue<ProviderImpl>>>,

    io_queues: Vec<Arc<Mutex<NvmeQueue<ProviderImpl>>>>,

    bar: usize,

    irq: usize,

    capacity: usize,
}

impl NvmeInterface {
    const CQ_WAIT_TIMEOUT_US: u64 = 1_000_000;
    const CQ_WAIT_MAX_SPINS: u64 = 50_000_000;

    pub fn new(bar: usize, irq: usize) -> DeviceResult<NvmeInterface> {
        // Read CAP register to determine doorbell stride (DSTRD)
        let cap = unsafe { read_volatile(bar as *const u64) };
        let dstrd = ((cap >> 32) & 0xf) as u32;
        let stride = 4 << dstrd;
        warn!("[nvme] CAP: {:#x}, DSTRD: {}, stride: {} bytes", cap, dstrd, stride);

        // SQ y tail doorbell offset is 2 * y * stride
        let admin_db_offset = 0;
        let io_db_offset = 2 * stride as usize;

        let admin_queue = Arc::new(Mutex::new(NvmeQueue::new(0, admin_db_offset)));
        let io_queues = vec![Arc::new(Mutex::new(NvmeQueue::<ProviderImpl>::new(1, io_db_offset)))];

        let mut interface = NvmeInterface {
            name: String::from("nvme"),
            admin_queue,
            io_queues,
            bar,
            irq,
            capacity: 0,
        };

        interface.init(stride as usize)?;

        Ok(interface)
    }

    pub fn init(&mut self, stride: usize) -> DeviceResult {
        self.nvme_configure_admin_queue(stride)?;
        self.nvme_alloc_io_queue(stride)?;
        Ok(())
    }

    pub fn get_name_irq(&self) -> (String, usize) {
        (self.name.clone(), self.irq)
    }
}

fn clflush_range(vaddr: usize, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{_mm_clflush, _mm_mfence};
        let line_size = 64;
        let start = vaddr & !(line_size - 1);
        let end = (vaddr + len + line_size - 1) & !(line_size - 1);
        unsafe {
            _mm_mfence();
            for addr in (start..end).step_by(line_size) {
                _mm_clflush(addr as *const u8);
            }
            _mm_mfence();
        }
    }
}

impl NvmeInterface {
    fn wait_cq_complete(
        queue: &mut NvmeQueue<ProviderImpl>,
        context: &str,
    ) -> DeviceResult {
        let start = timer_now_as_micros();
        let mut spins = 0_u64;
        let head = queue.cq_head;
        let expected_phase = queue.cq_phase;

        loop {
            let status = queue.cq[head].read();
            let phase = (status.status & 1) as usize;
            if phase == expected_phase {
                let sc = (status.status >> 1) & 0xff;
                let sct = (status.status >> 9) & 0x7;
                if sc != 0 || sct != 0 {
                    warn!(
                        "[nvme] completion error: status={:#x} (sct={}, sc={}) for {}",
                        status.status, sct, sc, context
                    );
                    return Err(DeviceError::IoError);
                }

                // Advance cq_head
                queue.cq_head += 1;
                if queue.cq_head >= queue.cq.len() {
                    queue.cq_head = 0;
                    queue.cq_phase = 1 - queue.cq_phase;
                }
                return Ok(());
            }

            core::hint::spin_loop();
            spins = spins.saturating_add(1);
            if timer_now_as_micros().wrapping_sub(start) >= Self::CQ_WAIT_TIMEOUT_US
                || spins >= Self::CQ_WAIT_MAX_SPINS
            {
                warn!(
                    "[nvme] timeout waiting CQ{} completion (head={}, phase={}) for {}",
                    head, head, expected_phase, context
                );
                return Err(DeviceError::IoError);
            }
        }
    }

    pub fn nvme_configure_admin_queue(&mut self, stride: usize) -> DeviceResult {
        let mut admin_queue = self.admin_queue.lock();

        let bar = self.bar;
        let dbs = bar + NVME_REG_DBS;

        let sq_dma_pa = admin_queue.sq_pa as u64;
        let cq_dma_pa = admin_queue.cq_pa as u64;
        let data_dma_pa = admin_queue.data_pa as u64;

        // Reset controller first
        warn!("[nvme] Resetting controller...");
        unsafe {
            let cc = read_volatile((bar + NVME_REG_CC) as *const u32);
            write_volatile((bar + NVME_REG_CC) as *mut u32, cc & !NVME_CC_ENABLE);
        }

        // Wait for CSTS.RDY to become 0
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
            if (csts & NVME_CSTS_RDY) == 0 {
                break;
            }
            if timer_now_as_micros().wrapping_sub(start) > 2_000_000 {
                warn!("[nvme] timeout waiting for controller reset");
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }

        let aqa_low_16 = 31_u16; // 32 entries (0-based)
        let aqa_high_16 = 31_u16; // 32 entries (0-based)
        let aqa = (aqa_high_16 as u32) << 16 | aqa_low_16 as u32;
        let aqa_address = bar + NVME_REG_AQA;

        unsafe {
            write_volatile(aqa_address as *mut u32, aqa);
            write_volatile((bar + NVME_REG_ASQ) as *mut u64, sq_dma_pa);
            write_volatile((bar + NVME_REG_ACQ) as *mut u64, cq_dma_pa);
        }

        // enable ctrl
        let mut ctrl_config = NVME_CC_ENABLE | NVME_CC_CSS_NVM;
        ctrl_config |= 0 << NVME_CC_MPS_SHIFT;
        ctrl_config |= NVME_CC_ARB_RR | NVME_CC_SHN_NONE;
        ctrl_config |= NVME_CC_IOSQES | NVME_CC_IOCQES;

        unsafe { write_volatile((bar + NVME_REG_CC) as *mut u32, ctrl_config) }

        // Wait for CSTS.RDY to become 1
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
            if (csts & NVME_CSTS_RDY) != 0 {
                break;
            }
            if timer_now_as_micros().wrapping_sub(start) > 2_000_000 {
                warn!("[nvme] timeout waiting for controller ready");
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }
        warn!("[nvme] Controller ready!");

        let data_va = phys_to_virt(data_dma_pa as usize);

        // Invalidate cache before command so no dirty lines are written back
        clflush_range(data_va, 4096);

        // config identify (CNS = 1: Identify Controller)
        let mut cmd = NvmeIdentify::new();
        cmd.prp1 = data_dma_pa;
        cmd.command_id = 0x1018;
        cmd.nsid = 0;
        cmd.cns = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };

        let tail = admin_queue.sq_tail;
        admin_queue.sq[tail].write(common_cmd);
        admin_queue.sq_tail += 1;
        if admin_queue.sq_tail >= admin_queue.sq.len() {
            admin_queue.sq_tail = 0;
        }

        let admin_q_db = dbs + admin_queue.db_offset;
        unsafe { write_volatile(admin_q_db as *mut u32, (tail + 1) as u32) }

        Self::wait_cq_complete(&mut admin_queue, "identify admin queue")?;
        unsafe { write_volatile((admin_q_db + stride) as *mut u32, admin_queue.cq_head as u32) }

        // Invalidate cache after command so CPU reads from RAM
        clflush_range(data_va, 4096);

        // config identify (CNS = 0: Identify Namespace)
        let mut cmd = NvmeIdentify::new();
        cmd.cns = 0;
        cmd.prp1 = data_dma_pa;
        cmd.command_id = 0x1019;
        cmd.nsid = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };

        let tail = admin_queue.sq_tail;
        admin_queue.sq[tail].write(common_cmd);
        admin_queue.sq_tail += 1;
        if admin_queue.sq_tail >= admin_queue.sq.len() {
            admin_queue.sq_tail = 0;
        }

        unsafe { write_volatile(admin_q_db as *mut u32, (tail + 1) as u32) }

        Self::wait_cq_complete(&mut admin_queue, "identify namespace")?;
        unsafe { write_volatile((admin_q_db + stride) as *mut u32, admin_queue.cq_head as u32) }

        // Invalidate cache after command so CPU reads from RAM
        clflush_range(data_va, 4096);

        // read namespace size and check LBA size format
        let nsze = unsafe { core::ptr::read_volatile(data_va as *const u64) };
        let flbas = unsafe { core::ptr::read_volatile((data_va + 26) as *const u8) };
        let lbaf_index = (flbas & 0xF) as usize;
        let lbaf_offset = 128 + lbaf_index * 4;
        let lbaf = unsafe { core::ptr::read_volatile((data_va + lbaf_offset) as *const u32) };
        let lbads = ((lbaf >> 16) & 0xFF) as u8;
        let block_size = if lbads >= 9 && lbads <= 16 {
            1 << lbads
        } else {
            512
        };

        self.capacity = nsze as usize * (block_size / 512);
        warn!(
            "[nvme] Identified namespace size: {} blocks (block_size: {}B), capacity: {} sectors (512B)",
            nsze, block_size, self.capacity
        );

        Ok(())
    }

    pub fn nvme_alloc_io_queue(&mut self, stride: usize) -> DeviceResult {
        let mut admin_queue = self.admin_queue.lock();
        let io_queue = self.io_queues[0].lock();

        let bar = self.bar;
        let dbs = bar + NVME_REG_DBS;
        let admin_q_db = dbs;

        // Set Features: Number of Queues
        let mut cmd = NvmeCommonCommand::new();
        cmd.opcode = 0x09;
        cmd.command_id = 0x2;
        cmd.nsid = 0;
        cmd.cdw10 = 0x7;
        cmd.cdw11 = 0x0007_0007; // Request 8 SQs and 8 CQs

        let tail = admin_queue.sq_tail;
        admin_queue.sq[tail].write(cmd);
        admin_queue.sq_tail += 1;
        if admin_queue.sq_tail >= admin_queue.sq.len() {
            admin_queue.sq_tail = 0;
        }

        unsafe { write_volatile(admin_q_db as *mut u32, (tail + 1) as u32) }

        Self::wait_cq_complete(&mut admin_queue, "set queue count")?;
        unsafe { write_volatile((admin_q_db + stride) as *mut u32, admin_queue.cq_head as u32) }

        // Create IO Completion Queue
        let mut cmd = NvmeCreateCq::new();
        cmd.opcode = 0x05;
        cmd.command_id = 0x3;
        cmd.prp1 = io_queue.cq_pa as u64;
        cmd.cqid = 1;
        cmd.qsize = (io_queue.cq.len() - 1) as u16; // 0-based size (e.g. 127 for 128 entries)
        cmd.cq_flags = NVME_QUEUE_PHYS_CONTIG | NVME_CQ_IRQ_ENABLED;

        let common_cmd = unsafe { core::mem::transmute(cmd) };

        let tail = admin_queue.sq_tail;
        admin_queue.sq[tail].write(common_cmd);
        admin_queue.sq_tail += 1;
        if admin_queue.sq_tail >= admin_queue.sq.len() {
            admin_queue.sq_tail = 0;
        }
        unsafe { write_volatile(admin_q_db as *mut u32, (tail + 1) as u32) }
        Self::wait_cq_complete(&mut admin_queue, "create io completion queue")?;
        unsafe { write_volatile((admin_q_db + stride) as *mut u32, admin_queue.cq_head as u32) }

        // Create IO Submission Queue
        let mut cmd = NvmeCreateSq::new();
        cmd.opcode = 0x01;
        cmd.command_id = 0x4;
        cmd.prp1 = io_queue.sq_pa as u64;
        cmd.sqid = 1;
        cmd.qsize = (io_queue.sq.len() - 1) as u16; // 0-based size
        cmd.sq_flags = 0x1;
        cmd.cqid = 1;

        let common_cmd = unsafe { core::mem::transmute(cmd) };

        // write command to sq
        let tail = admin_queue.sq_tail;
        admin_queue.sq[tail].write(common_cmd);
        admin_queue.sq_tail += 1;
        if admin_queue.sq_tail >= admin_queue.sq.len() {
            admin_queue.sq_tail = 0;
        }

        // write doorbell register
        unsafe { write_volatile(admin_q_db as *mut u32, (tail + 1) as u32) }

        // wait for command complete
        Self::wait_cq_complete(&mut admin_queue, "create io submission queue")?;
        unsafe { write_volatile((admin_q_db + stride) as *mut u32, admin_queue.cq_head as u32) }
        Ok(())
    }
}

impl BlockScheme for NvmeInterface {
    fn read_block(&self, block_id: usize, read_buf: &mut [u8]) -> DeviceResult {
        let mut io_queue = self.io_queues[0].lock();
        let db_offset = io_queue.db_offset;
        let bar = self.bar;
        let dbs = bar + NVME_REG_DBS;

        let ptr = read_buf.as_mut_ptr();
        let addr = virt_to_phys(ptr as usize);
        let buf_len = read_buf.len();

        // Invalidate cache before command so no dirty lines are written back
        clflush_range(ptr as usize, buf_len);

        // build nvme read command
        let mut cmd = NvmeRWCommand::new_read_command();
        cmd.nsid = 1;
        cmd.prp1 = addr as u64;
        cmd.command_id = 101;
        cmd.length = 0; // 0 means 1 block
        cmd.slba = block_id as u64;

        let common_cmd = unsafe { core::mem::transmute(cmd) };

        let tail = io_queue.sq_tail;
        io_queue.sq[tail].write(common_cmd);
        io_queue.sq_tail += 1;
        if io_queue.sq_tail >= io_queue.sq.len() {
            io_queue.sq_tail = 0;
        }

        // write SQ tail doorbell
        unsafe { write_volatile((dbs + db_offset) as *mut u32, (tail + 1) as u32) }

        // wait for command complete on io_queue
        Self::wait_cq_complete(&mut io_queue, "read block")?;

        // Invalidate cache after command so CPU reads from RAM
        clflush_range(ptr as usize, buf_len);

        // write CQ head doorbell
        let stride = db_offset / 2;
        unsafe { write_volatile((dbs + db_offset + stride) as *mut u32, io_queue.cq_head as u32) }

        Ok(())
    }

    fn write_block(&self, block_id: usize, write_buf: &[u8]) -> DeviceResult {
        let mut io_queue = self.io_queues[0].lock();
        let db_offset = io_queue.db_offset;
        let bar = self.bar;
        let dbs = bar + NVME_REG_DBS;

        let ptr = write_buf.as_ptr();
        let addr = virt_to_phys(ptr as usize);
        let buf_len = write_buf.len();

        // Flush cache before command so device reads fresh data from RAM
        clflush_range(ptr as usize, buf_len);

        // build nvme write command
        let mut cmd = NvmeRWCommand::new_write_command();
        cmd.nsid = 1;
        cmd.prp1 = addr as u64;
        cmd.length = 0; // 0 means 1 block
        cmd.command_id = 100;
        cmd.slba = block_id as u64;

        let common_cmd = unsafe { core::mem::transmute(cmd) };

        let tail = io_queue.sq_tail;
        io_queue.sq[tail].write(common_cmd);
        io_queue.sq_tail += 1;
        if io_queue.sq_tail >= io_queue.sq.len() {
            io_queue.sq_tail = 0;
        }

        // write SQ tail doorbell
        unsafe { write_volatile((dbs + db_offset) as *mut u32, (tail + 1) as u32) }

        // wait for command complete on io_queue
        Self::wait_cq_complete(&mut io_queue, "write block")?;

        // write CQ head doorbell
        let stride = db_offset / 2;
        unsafe { write_volatile((dbs + db_offset + stride) as *mut u32, io_queue.cq_head as u32) }

        Ok(())
    }

    fn flush(&self) -> DeviceResult {
        Ok(())
    }

    fn block_count(&self) -> usize {
        self.capacity
    }
}

impl Scheme for NvmeInterface {
    fn name(&self) -> &str {
        "nvme"
    }

    fn handle_irq(&self, irq: usize) {
        warn!("nvme device irq {}", irq);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
//64B
pub struct NvmeCommonCommand {
    opcode: u8,
    flags: u8,
    command_id: u16,
    nsid: u32,
    cdw2: [u32; 2],
    metadata: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

impl NvmeCommonCommand {
    pub fn new() -> Self {
        Self {
            opcode: 0,
            flags: 0,
            command_id: 0,
            nsid: 0,
            cdw2: [0; 2],
            metadata: 0,
            prp1: 0,
            prp2: 0,
            cdw10: 0,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeIdentify {
    opcode: u8,
    flags: u8,
    command_id: u16,
    nsid: u32,
    rsvd2: [u64; 2],
    prp1: u64,
    prp2: u64,
    cns: u8,
    rsvd3: u8,
    ctrlid: u16,
    rsvd11: [u8; 3],
    csi: u8,
    rsvd12: [u32; 4],
}

impl NvmeIdentify {
    pub fn new() -> Self {
        Self {
            opcode: 0x06,
            flags: 0,
            command_id: 0x1,
            nsid: 1,
            rsvd2: [0; 2],
            prp1: 0,
            prp2: 0,
            cns: 1,
            rsvd3: 0,
            ctrlid: 0,
            rsvd11: [0; 3],
            csi: 0,
            rsvd12: [0; 4],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeCreateCq {
    pub opcode: u8,
    pub flags: u8,
    pub command_id: u16,
    pub nsid: u32,
    pub rsvd1: [u32; 4],
    pub prp1: u64,
    pub rsvd8: u64,
    pub cqid: u16,
    pub qsize: u16,
    pub cq_flags: u16,
    pub irq_vector: u16,
    pub rsvd12: [u32; 4],
}

impl NvmeCreateCq {
    fn new() -> Self {
        Self {
            opcode: 0x05,
            flags: 0,
            command_id: 0,
            nsid: 0,
            rsvd1: [0; 4],
            prp1: 0,
            rsvd8: 0,
            cqid: 0,
            qsize: 0,
            cq_flags: 0,
            irq_vector: 0,
            rsvd12: [0; 4],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeCreateSq {
    pub opcode: u8,
    pub flags: u8,
    pub command_id: u16,
    pub nsid: u32,
    pub rsvd1: [u32; 4],
    pub prp1: u64,
    pub rsvd8: u64,
    pub sqid: u16,
    pub qsize: u16,
    pub sq_flags: u16,
    pub cqid: u16,
    pub rsvd12: [u32; 4],
}

impl NvmeCreateSq {
    fn new() -> Self {
        Self {
            opcode: 0x01,
            flags: 0,
            command_id: 0,
            nsid: 0,
            rsvd1: [0; 4],
            prp1: 0,
            rsvd8: 0,
            sqid: 0,
            qsize: 0,
            sq_flags: 0,
            cqid: 0,
            rsvd12: [0; 4],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct NvmeRWCommand {
    pub opcode: u8,
    pub flags: u8,
    pub command_id: u16,
    pub nsid: u32,
    pub rsvd2: u64,
    pub metadata: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub slba: u64,
    pub length: u16,
    pub control: u16,
    pub dsmgmt: u32,
    pub reftag: u32,
    pub apptag: u16,
    pub appmask: u16,
}

impl NvmeRWCommand {
    pub fn new_write_command() -> Self {
        Self {
            opcode: 0x01,
            flags: 0,
            command_id: 0,
            nsid: 0,
            rsvd2: 0,
            metadata: 0,
            prp1: 0,
            prp2: 0,
            slba: 0,
            length: 0,
            control: 0,
            dsmgmt: 0,
            reftag: 0,
            apptag: 0,
            appmask: 0,
        }
    }
    pub fn new_read_command() -> Self {
        Self {
            opcode: 0x02,
            flags: 0,
            command_id: 0,
            nsid: 0,
            rsvd2: 0,
            metadata: 0,
            prp1: 0,
            prp2: 0,
            slba: 0,
            length: 0,
            control: 0,
            dsmgmt: 0,
            reftag: 0,
            apptag: 0,
            appmask: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct NvmeCompletion {
    pub result: u64,
    // pub rsvd: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub command_id: u16,
    pub status: u16,
}

// NvmeRegister
pub const NVME_REG_CAP: usize = 0x0000; /* Controller Capabilities */
pub const NVME_REG_VS: usize = 0x0008; /* Version */
pub const NVME_REG_INTMS: usize = 0x000c; /* Interrupt Mask Set */
pub const NVME_REG_INTMC: usize = 0x0010; /* Interrupt Mask Clear */
pub const NVME_REG_CC: usize = 0x0014; /* Controller Configuration */
pub const NVME_REG_CSTS: usize = 0x001c; /* Controller Status */
pub const NVME_REG_NSSR: usize = 0x0020; /* NVM Subsystem Reset */
pub const NVME_REG_AQA: usize = 0x0024; /* Admin Queue Attributes */
pub const NVME_REG_ASQ: usize = 0x0028; /* Admin SQ Base Address */
pub const NVME_REG_ACQ: usize = 0x0030; /* Admin CQ Base Address */
pub const NVME_REG_CMBLOC: usize = 0x0038; /* Controller Memory Buffer Location */
pub const NVME_REG_CMBSZ: usize = 0x003c; /* Controller Memory Buffer Size */
pub const NVME_REG_BPINFO: usize = 0x0040; /* Boot Partition Information */
pub const NVME_REG_BPRSEL: usize = 0x0044; /* Boot Partition Read Select */
pub const NVME_REG_BPMBL: usize = 0x0048; /* Boot Partition Memory Buffer
                                           * Location
                                           */
pub const NVME_REG_CMBMSC: usize = 0x0050; /* Controller Memory Buffer Memory
                                            * Space Control
                                            */
pub const NVME_REG_CRTO: usize = 0x0068; /* Controller Ready Timeouts */
pub const NVME_REG_PMRCAP: usize = 0x0e00; /* Persistent Memory Capabilities */
pub const NVME_REG_PMRCTL: usize = 0x0e04; /* Persistent Memory Region Control */
pub const NVME_REG_PMRSTS: usize = 0x0e08; /* Persistent Memory Region Status */
pub const NVME_REG_PMREBS: usize = 0x0e0c; /* Persistent Memory Region Elasticity
                                            * Buffer Size
                                            */
pub const NVME_REG_PMRSWTP: usize = 0x0e10; /* Persistent Memory Region Sustained
                                             * Write Throughput
                                             */
pub const NVME_REG_DBS: usize = 0x1000; /* SQ 0 Tail Doorbell */

// NVME CONST
pub const NVME_CC_ENABLE: u32 = 1 << 0;
pub const NVME_CC_CSS_NVM: u32 = 0 << 4;
pub const NVME_CC_MPS_SHIFT: u32 = 7;
pub const NVME_CC_ARB_RR: u32 = 0 << 11;
pub const NVME_CC_ARB_WRRU: u32 = 1 << 11;
pub const NVME_CC_ARB_VS: u32 = 7 << 11;
pub const NVME_CC_SHN_NONE: u32 = 0 << 14;
pub const NVME_CC_SHN_NORMAL: u32 = 1 << 14;
pub const NVME_CC_SHN_ABRUPT: u32 = 2 << 14;
pub const NVME_CC_IOSQES: u32 = 6 << 16;
pub const NVME_CC_IOCQES: u32 = 4 << 20;
pub const NVME_CSTS_RDY: u32 = 1 << 0;
pub const NVME_CSTS_CFS: u32 = 1 << 1;
pub const NVME_CSTS_SHST_NORMAL: u32 = 0 << 2;
pub const NVME_CSTS_SHST_OCCUR: u32 = 1 << 2;
pub const NVME_CSTS_SHST_CMPLT: u32 = 2 << 2;

pub const NVME_QUEUE_PHYS_CONTIG: u16 = 1 << 0;
pub const NVME_CQ_IRQ_ENABLED: u16 = 1 << 1;
pub const NVME_SQ_PRIO_URGENT: u16 = 0 << 1;
pub const NVME_SQ_PRIO_HIGH: u16 = 1 << 1;
pub const NVME_SQ_PRIO_MEDIUM: u16 = 2 << 1;
pub const NVME_SQ_PRIO_LOW: u16 = 3 << 1;

pub const NVME_FEAT_ARBITRATION: u32 = 0x01;
pub const NVME_FEAT_POWER_MGMT: u32 = 0x02;
pub const NVME_FEAT_LBA_RANGE: u32 = 0x03;
pub const NVME_FEAT_TEMP_THRESH: u32 = 0x04;
pub const NVME_FEAT_ERR_RECOVERY: u32 = 0x05;
pub const NVME_FEAT_VOLATILE_WC: u32 = 0x06;
pub const NVME_FEAT_NUM_QUEUES: u32 = 0x07;
pub const NVME_FEAT_IRQ_COALESCE: u32 = 0x08;
pub const NVME_FEAT_IRQ_CONFIG: u32 = 0x09;
pub const NVME_FEAT_WRITE_ATOMIC: u32 = 0x0a;
pub const NVME_FEAT_ASYNC_EVENT: u32 = 0x0b;
pub const NVME_FEAT_SW_PROGRESS: u32 = 0x0c;

pub struct NvmeDriverPci;

impl PciDriver for NvmeDriverPci {
    fn name(&self) -> &str {
        "nvme"
    }

    fn matched(&self, _vendor_id: u16, _device_id: u16) -> bool {
        false
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        dev.id.class == 0x01 && dev.id.subclass == 0x08
    }

    fn init(&self, dev: &PCIDevice, mapper: &Option<Arc<dyn IoMapper>>, irq: Option<usize>) -> DeviceResult<Device> {
        if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[0] {
            if let Some(m) = mapper {
                m.query_or_map(addr as usize, 4096 * 8);
            }
            let vaddr = crate::bus::phys_to_virt(addr as usize);
            let vector = irq.map(|idx| idx + 32).unwrap_or(33);
            let blk = Arc::new(NvmeInterface::new(vaddr, vector)?);
            Ok(Device::Block(blk))
        } else {
            Err(crate::DeviceError::NotSupported)
        }
    }
}
