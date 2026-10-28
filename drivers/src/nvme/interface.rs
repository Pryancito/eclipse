use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicBool, Ordering};

use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::scheme::{BlockScheme, Scheme};
use crate::{Device, DeviceError, DeviceResult};
use pci::{PCIDevice, BAR};

use lock::Mutex;

use super::nvme_queue::*;

const SECTOR_SIZE: usize = 512;

pub struct NvmeInterface {
    name: String,

    admin_queue: Arc<Mutex<NvmeQueue<ProviderImpl>>>,

    io_queues: Vec<Arc<Mutex<NvmeQueue<ProviderImpl>>>>,

    bar: usize,

    /// Doorbell stride in bytes (4 << CAP.DSTRD).
    stride: usize,

    irq: usize,

    /// Capacity in 512-byte sectors.
    capacity: usize,

    /// log2 of the namespace LBA size (9 = 512B, 12 = 4KiB).
    lba_shift: u8,

    /// Per-command transfer cap in bytes: the controller's advertised MDTS
    /// (Identify Controller byte 77, in units of 2^n minimum-page-size pages;
    /// 0 = unlimited), further clamped by callers to the bounce-buffer size.
    /// Starts at one page — the always-legal minimum — until Identify runs.
    max_transfer: usize,

    /// Set when a command timed out and never completed, so the controller may
    /// still DMA into the queue's shared bounce buffer at any moment. Every
    /// subsequent request is refused rather than risk handing back data the
    /// controller overwrote. See `submit_sync`.
    poisoned: AtomicBool,
}

/// Outcome of waiting for one specific completion.
enum CqWait {
    /// The awaited completion arrived with a success status; carries CQE dword 0.
    Done(u32),
    /// The awaited completion arrived reporting an error status. The command is
    /// finished, so its data buffer is no longer under controller ownership and
    /// re-issuing it is safe.
    Failed,
    /// Nothing arrived before the deadline. The command is STILL owned by the
    /// controller and its PRPs still point at the queue's bounce buffer, so that
    /// buffer must not be reused until the completion is seen.
    Timeout,
    /// CSTS.CFS: the controller reported a fatal status. It will not complete
    /// anything, so waiting longer is pointless — but its DMA engine is in an
    /// undefined state, so the buffer is no safer than after a plain timeout.
    Fatal,
}

#[cfg(test)]
mod nvme_fake;

/// El arranque del controlador: el reset, la cola de administracion, lo que
/// dice el Identify y las colas de entrada y salida.
#[cfg(test)]
mod bringup_tests;

/// Escribe un registro de 32 bits de la ventana del controlador.
///
/// Lo unico que una ventana de registros en memoria no puede dar es la
/// REACCION a la escritura, y aqui esa reaccion es todo: CC.EN es lo que
/// levanta CSTS.RDY, y el timbre de la cola es lo que hace que el controlador
/// lea el comando y conteste. Esa es la costura `cfg(test)`, la misma que
/// tienen el xHCI y el e1000e.
#[inline]
fn reg_write32(bar: usize, off: usize, v: u32) {
    unsafe { write_volatile((bar + off) as *mut u32, v) };
    #[cfg(test)]
    nvme_fake::on_write(bar, off, v as u64);
}

/// [`reg_write32`] para los registros de 64 bits (ASQ y ACQ).
#[inline]
fn reg_write64(bar: usize, off: usize, v: u64) {
    unsafe { write_volatile((bar + off) as *mut u64, v) };
    #[cfg(test)]
    nvme_fake::on_write(bar, off, v);
}

impl NvmeInterface {
    const ADMIN_TIMEOUT_US: u64 = 5_000_000;
    const IO_TIMEOUT_US: u64 = 5_000_000;
    /// Extra grace given to a timed-out command purely to observe its
    /// completion, so we know the controller has stopped writing into the
    /// bounce buffer. See `submit_sync`.
    const DRAIN_TIMEOUT_US: u64 = 20_000_000;
    /// Floor for the hard spin cap. The cap is a safety net for a stopped
    /// timer, NOT a second timeout: the old flat 50 M spins cost roughly 2-3 s
    /// at 3 GHz (each spin does a clflush + mfence + volatile CQE read), i.e. it
    /// fired *before* the nominal 5 s timeout on a fast machine and after it on
    /// a slow one, making the effective timeout depend on CPU frequency. Scaled
    /// with the requested timeout and logged distinctly, mirroring the AHCI
    /// `wait_until` helper.
    const CQ_WAIT_MIN_SPINS: u64 = 500_000_000;
    /// Retries for an I/O command that came back with an *error completion*.
    /// Matching the AHCI driver, where a single transient hiccup used to abort
    /// a whole `apk` extraction with EIO. Only error completions are retried;
    /// a timed-out command is never re-issued (see `submit_sync`).
    const IO_RETRIES: u32 = 4;
    /// Settle time between I/O retries.
    const IO_RETRY_SETTLE_US: u64 = 50_000;

    /// Bring up the controller whose registers start at `bar`, a window
    /// `bar_len` bytes long.
    pub fn new(bar: usize, bar_len: usize, irq: usize) -> DeviceResult<NvmeInterface> {
        // Controller Capabilities: doorbell stride, max queue entries, ready timeout
        let cap = unsafe { read_volatile(bar as *const u64) };
        let window = controller_window(cap, bar_len, IO_QUEUES)?;
        warn!(
            "[nvme] CAP: {:#x}, stride {}B, admin/io queue entries: {}/{}, TO: {}us",
            cap, window.stride, window.admin_q_size, window.io_q_size, window.ready_timeout_us
        );

        let stride = window.stride;
        let ready_timeout_us = window.ready_timeout_us;
        let admin_queue = Arc::new(Mutex::new(NvmeQueue::new(0, window.admin_q_size)?));
        let io_queues = vec![Arc::new(Mutex::new(NvmeQueue::<ProviderImpl>::new(
            1,
            window.io_q_size,
        )?))];

        let mut interface = NvmeInterface {
            name: String::from("nvme"),
            admin_queue,
            io_queues,
            bar,
            stride,
            irq,
            capacity: 0,
            lba_shift: 9,
            max_transfer: PAGE_SIZE,
            poisoned: AtomicBool::new(false),
        };

        interface.nvme_configure_admin_queue(ready_timeout_us)?;
        interface.nvme_alloc_io_queue()?;

        Ok(interface)
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
    // Non-x86 cache maintenance for non-coherent DMA is a TODO; rely on coherent
    // mappings for now.
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (vaddr, len);
}

impl NvmeInterface {
    /// Poll `queue`'s completion queue until the CQE for `cid` is consumed or
    /// `timeout_us` elapses.
    ///
    /// Handles phase tracking and the CQ head doorbell, which is updated for
    /// every consumed entry — including error and stale completions — so the
    /// queue never gets out of sync with the controller.
    fn wait_cq(
        bar: usize,
        cq_db: usize,
        queue: &mut NvmeQueue<ProviderImpl>,
        cid: u16,
        timeout_us: u64,
        context: &str,
    ) -> CqWait {
        let start = timer_now_as_micros();
        let mut spins = 0_u64;
        // Safety net for a timer that does not advance, not a second timeout:
        // large enough that the timer-based check below always wins on a
        // healthy machine. See `CQ_WAIT_MIN_SPINS`.
        let max_spins = Self::CQ_WAIT_MIN_SPINS.max(timeout_us.saturating_mul(500));

        loop {
            let head = queue.cq_head;
            clflush_range(&queue.cq[head] as *const _ as usize, 16);
            let entry = queue.cq[head].read();

            if (entry.status & 1) as usize == queue.cq_phase {
                queue.cq_head += 1;
                if queue.cq_head >= queue.cq.len() {
                    queue.cq_head = 0;
                    queue.cq_phase ^= 1;
                }
                reg_write32(bar, cq_db - bar, queue.cq_head as u32);

                if entry.command_id != cid {
                    // A completion for a *different* CID than the one we're
                    // awaiting: the command we want may still be in flight, so
                    // consume this stale CQE (head/phase/doorbell already
                    // advanced) and keep polling instead of abandoning ours —
                    // returning here desynced and could wedge the queue. Still
                    // bounded by the timeout / CFS checks below.
                    warn!(
                        "[nvme] stale completion cid {:#x} (wanted {:#x}) for {}; continuing",
                        entry.command_id, cid, context
                    );
                    continue;
                }

                let sc = (entry.status >> 1) & 0xff;
                let sct = (entry.status >> 9) & 0x7;
                if sc != 0 || sct != 0 {
                    warn!(
                        "[nvme] completion error: status={:#x} (sct={}, sc={}) for {}",
                        entry.status, sct, sc, context
                    );
                    return CqWait::Failed;
                }
                return CqWait::Done(entry.result as u32);
            }

            core::hint::spin_loop();
            spins = spins.saturating_add(1);

            if spins.is_multiple_of(256) {
                let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
                if csts & NVME_CSTS_CFS != 0 {
                    warn!(
                        "[nvme] controller fatal status while waiting for {}",
                        context
                    );
                    return CqWait::Fatal;
                }
            }

            if spins >= max_spins {
                warn!(
                    "[nvme] CQ{} wait fallback: timer stuck, giving up after {} spins for {}",
                    queue.qid, spins, context
                );
                return CqWait::Timeout;
            }

            if timer_now_as_micros().wrapping_sub(start) >= timeout_us {
                warn!(
                    "[nvme] timeout waiting CQ{} completion (head={}, phase={}) for {}",
                    queue.qid, head, queue.cq_phase, context
                );
                return CqWait::Timeout;
            }
        }
    }

    /// Submit one command on `queue` and poll its completion queue until done.
    ///
    /// Returns CQE dword 0 (command result) on success.
    ///
    /// On a timeout the command is still owned by the controller, and its PRPs
    /// still point at `queue`'s bounce buffer — the single buffer every read and
    /// write on that queue shares. Returning straight away (what the old code
    /// did) left the next transfer's data exposed: if the abandoned command
    /// landed later, its DMA overwrote the new data and the read returned
    /// another sector's contents with no error anywhere. So we first spend
    /// `DRAIN_TIMEOUT_US` doing nothing but waiting for that completion to
    /// appear, which is what tells us the controller has stopped writing. If it
    /// never appears, the queue's buffer can be clobbered at any moment, so the
    /// device is marked poisoned and every later request is refused instead of
    /// silently returning corrupt data. Recovering properly from that state
    /// needs a controller reset (CC.EN=0, re-enable, recreate the I/O queues),
    /// which this driver does not implement yet.
    fn submit_sync(
        bar: usize,
        stride: usize,
        queue: &mut NvmeQueue<ProviderImpl>,
        mut cmd: NvmeCommonCommand,
        timeout_us: u64,
        context: &str,
        poisoned: &AtomicBool,
    ) -> DeviceResult<u32> {
        if poisoned.load(Ordering::Relaxed) {
            return Err(DeviceError::IoError);
        }

        let sq_db = bar + NVME_REG_DBS + 2 * queue.qid * stride;
        let cq_db = sq_db + stride;

        let cid = queue.next_cid();
        cmd.command_id = cid;

        let tail = queue.sq_tail;
        queue.sq[tail].write(cmd);
        queue.sq_tail = if tail + 1 >= queue.sq.len() {
            0
        } else {
            tail + 1
        };

        // Make the SQ entry visible to the device before ringing the doorbell.
        clflush_range(&queue.sq[tail] as *const _ as usize, 64);
        fence(Ordering::SeqCst);
        reg_write32(bar, sq_db - bar, queue.sq_tail as u32);

        let outcome = match Self::wait_cq(bar, cq_db, queue, cid, timeout_us, context) {
            CqWait::Done(result) => return Ok(result),
            CqWait::Failed => return Err(DeviceError::IoError),
            // A fatal controller status will never produce a completion, so
            // draining would only burn the whole grace period before reaching
            // the same conclusion.
            CqWait::Fatal => CqWait::Fatal,
            CqWait::Timeout => {
                warn!(
                    "[nvme] draining timed-out command cid {:#x} ({}) before reusing its buffer",
                    cid, context
                );
                Self::wait_cq(bar, cq_db, queue, cid, Self::DRAIN_TIMEOUT_US, context)
            }
        };

        match outcome {
            // The command finished after all: the controller is no longer
            // touching the bounce buffer, so the queue stays usable.
            CqWait::Done(_) | CqWait::Failed => Err(DeviceError::IoError),
            CqWait::Timeout | CqWait::Fatal => {
                poisoned.store(true, Ordering::Relaxed);
                error!(
                    "[nvme] command cid {:#x} ({}) never completed; the controller may still DMA \
                     into the shared transfer buffer. Disabling this device rather than return \
                     data it can overwrite — a reboot is needed to recover it.",
                    cid, context
                );
                Err(DeviceError::IoError)
            }
        }
    }

    pub fn nvme_configure_admin_queue(&mut self, ready_timeout_us: u64) -> DeviceResult {
        let bar = self.bar;
        let stride = self.stride;
        let admin = self.admin_queue.clone();
        let mut admin_queue = admin.lock();

        // Reset controller first
        warn!("[nvme] Resetting controller...");
        unsafe {
            let cc = read_volatile((bar + NVME_REG_CC) as *const u32);
            reg_write32(bar, NVME_REG_CC, cc & !NVME_CC_ENABLE);
        }

        // Wait for CSTS.RDY to become 0
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
            if (csts & NVME_CSTS_RDY) == 0 {
                break;
            }
            if timer_now_as_micros().wrapping_sub(start) > ready_timeout_us {
                warn!("[nvme] timeout waiting for controller reset");
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }

        // Admin queue attributes: 0-based sizes for SQ (bits 0-11) and CQ (bits 16-27)
        let aqa_entries = (admin_queue.sq.len() - 1) as u32;
        let aqa = (aqa_entries << 16) | aqa_entries;

        reg_write32(bar, NVME_REG_AQA, aqa);
        reg_write64(bar, NVME_REG_ASQ, admin_queue.sq_pa as u64);
        reg_write64(bar, NVME_REG_ACQ, admin_queue.cq_pa as u64);

        // enable ctrl
        let mut ctrl_config = NVME_CC_ENABLE | NVME_CC_CSS_NVM;
        ctrl_config |= 0 << NVME_CC_MPS_SHIFT;
        ctrl_config |= NVME_CC_ARB_RR | NVME_CC_SHN_NONE;
        ctrl_config |= NVME_CC_IOSQES | NVME_CC_IOCQES;

        reg_write32(bar, NVME_REG_CC, ctrl_config);

        // Wait for CSTS.RDY to become 1
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
            if (csts & NVME_CSTS_RDY) != 0 {
                break;
            }
            if csts & NVME_CSTS_CFS != 0 {
                warn!("[nvme] controller fatal status during enable");
                return Err(DeviceError::IoError);
            }
            if timer_now_as_micros().wrapping_sub(start) > ready_timeout_us {
                warn!("[nvme] timeout waiting for controller ready");
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }
        warn!("[nvme] Controller ready!");

        // We poll for completions; mask all controller interrupts.
        reg_write32(bar, NVME_REG_INTMS, 0xffff_ffff);

        let data_va = admin_queue.data_va;
        let data_pa = admin_queue.data_pa;

        // Identify Controller (CNS = 1)
        clflush_range(data_va, 4096);
        let mut cmd = NvmeIdentify::new();
        cmd.prp1 = data_pa as u64;
        cmd.nsid = 0;
        cmd.cns = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "identify controller",
            &self.poisoned,
        )?;
        clflush_range(data_va, 4096);

        // Model number: bytes 24..63 of the Identify Controller data (ASCII)
        let model = unsafe { core::slice::from_raw_parts((data_va + 24) as *const u8, 40) };
        if let Ok(model) = core::str::from_utf8(model) {
            warn!("[nvme] model: {}", model.trim());
        }

        // MDTS (byte 77): max data transfer size as 2^n minimum-page-size
        // pages, 0 = unlimited. CAP.MPSMIN is virtually always 4 KiB; using
        // that floor as the unit keeps our computed cap <= the controller's
        // real limit even when MPSMIN is larger. `io_rw` callers clamp every
        // command to this, so the 128 KiB bounce never exceeds what the
        // controller accepts.
        let mdts = unsafe { read_volatile((data_va + 77) as *const u8) };
        self.max_transfer = max_transfer_from_mdts(mdts);
        warn!(
            "[nvme] MDTS: {} ({} per command)",
            mdts,
            if self.max_transfer == usize::MAX {
                String::from("unlimited")
            } else {
                alloc::format!("{} KiB", self.max_transfer / 1024)
            }
        );

        // Identify Namespace 1 (CNS = 0)
        clflush_range(data_va, 4096);
        let mut cmd = NvmeIdentify::new();
        cmd.cns = 0;
        cmd.prp1 = data_pa as u64;
        cmd.nsid = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "identify namespace",
            &self.poisoned,
        )?;
        clflush_range(data_va, 4096);

        // Namespace size (LBAs) and current LBA format
        let nsze = unsafe { read_volatile(data_va as *const u64) };
        let flbas = unsafe { read_volatile((data_va + 26) as *const u8) };
        let lbaf_index = (flbas & 0xF) as usize;
        let lbaf_offset = 128 + lbaf_index * 4;
        let lbaf = unsafe { read_volatile((data_va + lbaf_offset) as *const u32) };
        let lbads = ((lbaf >> 16) & 0xFF) as u8;

        drop(admin_queue);

        if nsze == 0 {
            warn!("[nvme] namespace 1 has zero size, not usable");
            return Err(DeviceError::NoResources);
        }
        // Cap LBA size at 8 KiB: the single-LBA (partial-update) path assumes
        // one LBA always fits the bounce comfortably, and no real consumer
        // formats beyond 4 KiB anyway.
        if !lba_shift_supported(lbads) {
            warn!("[nvme] unsupported LBA size 2^{} bytes", lbads);
            return Err(DeviceError::NotSupported);
        }

        self.lba_shift = lbads;
        self.capacity = capacity_in_sectors(nsze, lbads);
        warn!(
            "[nvme] namespace 1: {} LBAs of {}B, capacity {} sectors (512B)",
            nsze,
            1u32 << lbads,
            self.capacity
        );

        Ok(())
    }

    pub fn nvme_alloc_io_queue(&mut self) -> DeviceResult {
        let bar = self.bar;
        let stride = self.stride;
        let mut admin_queue = self.admin_queue.lock();
        let io_queue = self.io_queues[0].lock();

        // Set Features: Number of Queues (request 1 IO SQ + 1 IO CQ, 0-based)
        let mut cmd = NvmeCommonCommand::new();
        cmd.opcode = 0x09;
        cmd.cdw10 = NVME_FEAT_NUM_QUEUES;
        cmd.cdw11 = 0;
        let result = Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            cmd,
            Self::ADMIN_TIMEOUT_US,
            "set queue count",
            &self.poisoned,
        )?;
        trace!(
            "[nvme] controller allocated {} IO SQs / {} IO CQs",
            (result & 0xffff) + 1,
            (result >> 16) + 1
        );

        // Create IO Completion Queue (qid 1). We poll, so no interrupts.
        let mut cmd = NvmeCreateCq::new();
        cmd.prp1 = io_queue.cq_pa as u64;
        cmd.cqid = 1;
        cmd.qsize = (io_queue.cq.len() - 1) as u16;
        cmd.cq_flags = NVME_QUEUE_PHYS_CONTIG;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "create io completion queue",
            &self.poisoned,
        )?;

        // Create IO Submission Queue (qid 1, bound to CQ 1)
        let mut cmd = NvmeCreateSq::new();
        cmd.prp1 = io_queue.sq_pa as u64;
        cmd.sqid = 1;
        cmd.qsize = (io_queue.sq.len() - 1) as u16;
        cmd.sq_flags = NVME_QUEUE_PHYS_CONTIG;
        cmd.cqid = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "create io submission queue",
            &self.poisoned,
        )?;

        Ok(())
    }

    /// One read/write command on the IO queue, transferring `len` bytes
    /// through the queue's bounce buffer. PRP1 covers page 0; a two-page
    /// transfer points PRP2 at page 1 directly; anything larger points PRP2
    /// at the queue's PRP list holding the remaining page addresses (the
    /// standard NVMe mechanism — one list page covers 512 entries, far above
    /// the 31 the 128 KiB bounce needs).
    fn check_request(&self, block_id: usize, len: usize) -> DeviceResult {
        check_request(block_id, len, self.capacity)
    }

    fn io_rw(
        &self,
        queue: &mut NvmeQueue<ProviderImpl>,
        write: bool,
        slba: u64,
        nlb_minus_1: u16,
        len: usize,
    ) -> DeviceResult {
        let mut cmd = if write {
            NvmeRWCommand::new_write_command()
        } else {
            NvmeRWCommand::new_read_command()
        };
        cmd.nsid = 1;
        cmd.prp1 = queue.data_pa as u64;
        match prp2_for(queue.data_pa, len) {
            Prp2::Unused => {}
            Prp2::SecondPage(pa) => cmd.prp2 = pa,
            Prp2::List(entries) => {
                // The bounce is physically contiguous, so the list is a
                // simple arithmetic fill.
                for i in 0..entries {
                    unsafe {
                        write_volatile(
                            (queue.prp_list_va as *mut u64).add(i),
                            prp_list_entry(queue.data_pa, i),
                        );
                    }
                }
                clflush_range(queue.prp_list_va, entries * 8);
                cmd.prp2 = queue.prp_list_pa as u64;
            }
        }
        cmd.slba = slba;
        cmd.length = nlb_minus_1;

        let common_cmd: NvmeCommonCommand = unsafe { core::mem::transmute(cmd) };
        let context = if write { "write block" } else { "read block" };

        // Retry an error completion, as the AHCI driver does: a single transient
        // hiccup should not fail the caller's whole operation. Safe because an
        // error completion means the command is finished, so the bounce buffer
        // is ours again and re-issuing the identical command is idempotent —
        // its contents (for a write) and its PRPs are untouched. A *timed-out*
        // command is never retried: `submit_sync` either drains it or poisons
        // the device, and a poisoned device short-circuits below.
        let mut attempt = 1u32;
        loop {
            match Self::submit_sync(
                self.bar,
                self.stride,
                queue,
                common_cmd,
                Self::IO_TIMEOUT_US,
                context,
                &self.poisoned,
            ) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if attempt >= Self::IO_RETRIES || self.poisoned.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                    attempt += 1;
                    warn!(
                        "[nvme] retrying {} at lba {:#x} (attempt {}/{})",
                        context,
                        slba,
                        attempt,
                        Self::IO_RETRIES
                    );
                    udelay(Self::IO_RETRY_SETTLE_US);
                }
            }
        }
    }
}

/// Where PRP2 points for a transfer of `len` bytes out of a physically
/// contiguous, page-aligned buffer.
///
/// NVMe describes a data buffer with two fields and, past two pages, a list.
/// PRP1 always covers the first page. What PRP2 means then depends on the
/// size, and the three cases are not interchangeable: point it at a page
/// when the controller expects a list and it reads eight bytes of data as
/// addresses, scattering the transfer across whatever those bytes happen to
/// spell.
#[derive(Debug, PartialEq, Eq)]
enum Prp2 {
    /// One page or less. The field is not read at all.
    Unused,
    /// Exactly two pages. PRP2 is the second page itself.
    SecondPage(u64),
    /// More than two. PRP2 points at a list holding this many entries, one
    /// per page after the first.
    List(usize),
}

fn prp2_for(data_pa: usize, len: usize) -> Prp2 {
    if len > PAGE_SIZE * 2 {
        // Page 0 rides in PRP1; the list covers the rest.
        Prp2::List(len.div_ceil(PAGE_SIZE) - 1)
    } else if len > PAGE_SIZE {
        Prp2::SecondPage((data_pa + PAGE_SIZE) as u64)
    } else {
        Prp2::Unused
    }
}

/// Entry `i` of the PRP list covers page `i + 1` of the buffer, because page
/// 0 is already in PRP1. Off by one here and every transfer larger than two
/// pages reads or writes one page too early.
fn prp_list_entry(data_pa: usize, i: usize) -> u64 {
    (data_pa + (i + 1) * PAGE_SIZE) as u64
}

/// How much of the caller's request the next command moves.
struct Chunk {
    /// Bytes the command transfers, always whole LBAs.
    io_len: usize,
    /// Bytes of it the caller actually gets, which is less when the request
    /// starts or ends inside an LBA.
    take: usize,
    /// The command's NLB field, which is **zero-based**: one LBA is spelled
    /// 0. Writing the count itself transfers one LBA too many, over the top
    /// of whatever follows.
    nlb: u16,
}

/// Split the next command out of a request, given how much is left, how far
/// into an LBA it starts, and the per-command cap.
///
/// `io_len` is never zero: the transfer loops advance by `take` and would
/// otherwise spin for ever, and `nlb` would be computed from a count of 0.
/// Today that is guaranteed by `lba_shift_supported` capping an LBA at 8 KiB
/// while `max_transfer_from_mdts` never returns less than that -- a pairing
/// that is easy to break from either side.
fn io_chunk(remaining: usize, off: usize, lba_bytes: usize, chunk: usize) -> Chunk {
    if off == 0 && remaining >= lba_bytes {
        let n = (remaining / lba_bytes).min(chunk / lba_bytes).max(1);
        Chunk {
            io_len: n * lba_bytes,
            take: n * lba_bytes,
            nlb: (n - 1) as u16,
        }
    } else {
        Chunk {
            io_len: lba_bytes,
            take: remaining.min(lba_bytes - off),
            nlb: 0,
        }
    }
}

/// Per-command transfer cap from Identify Controller byte 77.
///
/// MDTS is a power of two in minimum-page-size units, and 0 means no limit
/// at all -- not "no transfers". CAP.MPSMIN is virtually always 4 KiB; using
/// that floor as the unit keeps the computed cap at or below the
/// controller's real limit even when MPSMIN is larger.
///
/// `checked_shl` alone is not enough here: it checks the *shift amount*
/// against the word width, not whether the value survives. `PAGE_SIZE` is
/// 2^12, so any MDTS from 52 up shifts every bit off the top and comes back
/// as `Some(0)` -- a cap of zero bytes, which stalls the transfer loop and
/// underflows the block count. A shift that large is the controller saying
/// it has no practical limit, so it saturates like the 64-and-over case
/// already did.
fn max_transfer_from_mdts(mdts: u8) -> usize {
    if mdts == 0 {
        return usize::MAX;
    }
    match PAGE_SIZE.checked_shl(mdts as u32) {
        Some(n) if n >= PAGE_SIZE => n,
        _ => usize::MAX,
    }
}

/// LBA sizes this driver will work with, as the exponent in LBAF.LBADS.
///
/// The partial-update path reads one whole LBA into the bounce buffer, so an
/// LBA has to fit there comfortably; 8 KiB is the cap and no real consumer
/// formats beyond 4 KiB anyway. Below 512 bytes the 512-byte block API above
/// could not address a whole LBA.
fn lba_shift_supported(lbads: u8) -> bool {
    (9..=13).contains(&lbads)
}

/// Namespace size in the 512-byte units the `BlockScheme` API speaks. The
/// drive counts in its own LBAs, which are larger on anything but a 512-byte
/// format, so without the scaling the upper part of the disk disappears.
fn capacity_in_sectors(nsze: u64, lbads: u8) -> usize {
    (nsze as usize) << (lbads - 9)
}

/// Validate a request in 512-byte sectors against the namespace capacity,
/// mirroring the AHCI driver's `check_request`. Without this an out-of-range
/// request reached the controller and came back as a command error, which is
/// both slower and -- since the timeout path is the expensive one -- a
/// needless way to stress recovery. It also backstops a corrupt partition
/// table, whose `PartitionBlock` bounds are derived from on-disk data that
/// nothing else validates.
fn check_request(block_id: usize, len: usize, capacity: usize) -> DeviceResult {
    if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
        return Err(DeviceError::InvalidParam);
    }
    match block_id.checked_add(len / SECTOR_SIZE) {
        Some(end) if end <= capacity => Ok(()),
        _ => Err(DeviceError::InvalidParam),
    }
}

/// Busy-wait for `us` microseconds, with a hard spin cap so a stopped timer
/// cannot turn this into an infinite loop.
fn udelay(us: u64) {
    let t0 = timer_now_as_micros();
    const MAX_SPINS: u64 = 10_000_000;
    let mut spins = 0u64;
    while timer_now_as_micros().wrapping_sub(t0) < us {
        core::hint::spin_loop();
        spins = spins.wrapping_add(1);
        if spins >= MAX_SPINS {
            break;
        }
    }
}

impl BlockScheme for NvmeInterface {
    // `block_id` indexes 512-byte sectors (same convention as the AHCI
    // driver); `buf.len()` may be any multiple of 512.
    fn read_block(&self, block_id: usize, read_buf: &mut [u8]) -> DeviceResult {
        self.check_request(block_id, read_buf.len())?;
        let lba_bytes = 1usize << self.lba_shift;
        let mut queue = self.io_queues[0].lock();
        let queue = &mut *queue;

        let mut byte_addr = block_id * SECTOR_SIZE;
        let mut done = 0usize;
        while done < read_buf.len() {
            let remaining = read_buf.len() - done;
            let lba = (byte_addr / lba_bytes) as u64;
            let off = byte_addr % lba_bytes;

            // Whole-LBA transfers go in chunks of up to the bounce buffer size
            // (clamped to the controller's MDTS); a sector range inside a
            // bigger LBA reads the full LBA and copies out.
            let chunk = queue.data_len.min(self.max_transfer);
            let Chunk { io_len, take, nlb } = io_chunk(remaining, off, lba_bytes, chunk);

            clflush_range(queue.data_va, io_len);
            self.io_rw(queue, false, lba, nlb, io_len)?;
            clflush_range(queue.data_va, io_len);

            let src =
                unsafe { core::slice::from_raw_parts((queue.data_va + off) as *const u8, take) };
            read_buf[done..done + take].copy_from_slice(src);

            done += take;
            byte_addr += take;
        }
        Ok(())
    }

    fn write_block(&self, block_id: usize, write_buf: &[u8]) -> DeviceResult {
        self.check_request(block_id, write_buf.len())?;
        let lba_bytes = 1usize << self.lba_shift;
        let mut queue = self.io_queues[0].lock();
        let queue = &mut *queue;

        let mut byte_addr = block_id * SECTOR_SIZE;
        let mut done = 0usize;
        while done < write_buf.len() {
            let remaining = write_buf.len() - done;
            let lba = (byte_addr / lba_bytes) as u64;
            let off = byte_addr % lba_bytes;

            let take;
            if off == 0 && remaining >= lba_bytes {
                let chunk = queue.data_len.min(self.max_transfer);
                let Chunk { io_len, nlb, .. } = io_chunk(remaining, off, lba_bytes, chunk);
                take = io_len;

                let dst =
                    unsafe { core::slice::from_raw_parts_mut(queue.data_va as *mut u8, io_len) };
                dst.copy_from_slice(&write_buf[done..done + io_len]);
                clflush_range(queue.data_va, io_len);
                self.io_rw(queue, true, lba, nlb, io_len)?;
            } else {
                // Partial LBA update: read-modify-write through the bounce buffer.
                take = remaining.min(lba_bytes - off);

                clflush_range(queue.data_va, lba_bytes);
                self.io_rw(queue, false, lba, 0, lba_bytes)?;
                clflush_range(queue.data_va, lba_bytes);

                let dst = unsafe {
                    core::slice::from_raw_parts_mut((queue.data_va + off) as *mut u8, take)
                };
                dst.copy_from_slice(&write_buf[done..done + take]);
                clflush_range(queue.data_va, lba_bytes);
                self.io_rw(queue, true, lba, 0, lba_bytes)?;
            }

            done += take;
            byte_addr += take;
        }
        Ok(())
    }

    fn flush(&self) -> DeviceResult {
        let mut queue = self.io_queues[0].lock();
        let mut cmd = NvmeCommonCommand::new();
        cmd.opcode = 0x00; // Flush
        cmd.nsid = 1;
        // Retried like a data command: `apk` fsync()s after every extracted
        // file, and a flush is the command most likely to hit a transient
        // error because it can sit behind a large device write cache. Flush is
        // idempotent and carries no data buffer.
        let mut attempt = 1u32;
        loop {
            match Self::submit_sync(
                self.bar,
                self.stride,
                &mut queue,
                cmd,
                Self::IO_TIMEOUT_US,
                "flush",
                &self.poisoned,
            ) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if attempt >= Self::IO_RETRIES || self.poisoned.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                    attempt += 1;
                    warn!(
                        "[nvme] retrying flush (attempt {}/{})",
                        attempt,
                        Self::IO_RETRIES
                    );
                    udelay(Self::IO_RETRY_SETTLE_US);
                }
            }
        }
    }

    fn block_count(&self) -> usize {
        self.capacity
    }

    // The namespace's real LBA size, as chosen by its current LBA format
    // (Identify Namespace LBAF[FLBAS].LBADS). Reads and writes above translate
    // 512-byte `block_id`s onto it, but the partition tables on the disk are
    // laid out in *these* units, so the scanner needs the true value.
    fn logical_block_size(&self) -> usize {
        1usize << self.lba_shift
    }

    /// Flush the write cache, then perform the NVMe orderly shutdown (CC.SHN =
    /// normal) and wait — bounded — for CSTS.SHST to report completion. The
    /// spec expects hosts to do this before any reset; on a DRAM-less
    /// controller (e.g. SM2269XT) this is the moment the FTL mapping tables
    /// are persisted to NAND instead of being rebuilt on next power-up.
    fn quiesce_for_reboot(&self) {
        let _ = self.flush();
        unsafe {
            let cc = read_volatile((self.bar + NVME_REG_CC) as *const u32);
            reg_write32(
                self.bar,
                NVME_REG_CC,
                (cc & !(3 << 14)) | NVME_CC_SHN_NORMAL,
            );
        }
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((self.bar + NVME_REG_CSTS) as *const u32) };
            if csts & (3 << 2) == NVME_CSTS_SHST_CMPLT {
                break;
            }
            // RTD3 entry latency is typically well under a second; do not hold
            // the reboot hostage to a wedged controller.
            if timer_now_as_micros().wrapping_sub(start) > 1_000_000 {
                warn!("[nvme] shutdown handshake timed out; proceeding with reset");
                break;
            }
            core::hint::spin_loop();
        }
    }
}

impl Scheme for NvmeInterface {
    fn name(&self) -> &str {
        "nvme"
    }

    fn handle_irq(&self, irq: usize) {
        // Completions are polled; interrupts are masked via INTMS.
        trace!("nvme device irq {}", irq);
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
            command_id: 0,
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
            ..Default::default()
        }
    }
    pub fn new_read_command() -> Self {
        Self {
            opcode: 0x02,
            ..Default::default()
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct NvmeCompletion {
    pub result: u64,
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

/// How many I/O queues this driver creates, besides the admin queue.
pub const IO_QUEUES: usize = 1;

/// What CAP and the size of BAR 0 decide between them about a controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerWindow {
    /// Doorbell stride in bytes, `4 << CAP.DSTRD`.
    pub stride: usize,
    /// Entries in the admin queue.
    pub admin_q_size: usize,
    /// Entries in each I/O queue.
    pub io_q_size: usize,
    /// How long CSTS.RDY is given, in microseconds.
    pub ready_timeout_us: u64,
}

/// Read CAP for a controller whose BAR 0 is `bar_len` bytes long.
///
/// Every number in here comes out of the controller, and two of them address
/// something: `MQES` sizes the queues and `DSTRD` places the doorbells. Neither
/// was checked.
pub fn controller_window(
    cap: u64,
    bar_len: usize,
    io_queues: usize,
) -> DeviceResult<ControllerWindow> {
    let stride = 4usize << ((cap >> 32) & 0xf);

    // MQES is the maximum queue entries *minus one*, so the smallest value a
    // controller may report is 1, meaning two entries. Zero used to give a
    // one-entry queue, and a queue with one entry is full as soon as anything
    // is in it (head == tail is empty, so `size - 1` commands fit): not one
    // command would ever be submitted, and the driver would sit waiting for a
    // completion for a command the controller never saw.
    let mqes = (cap & 0xffff) as usize + 1;
    if mqes < 2 {
        warn!(
            "[nvme] CAP.MQES says {} queue entry; a queue needs at least two",
            mqes
        );
        return Err(DeviceError::NotSupported);
    }

    // The doorbells start at 0x1000, two per queue, `stride` apart. DSTRD is
    // four bits, so a controller can ask for a stride of 128 KiB, and the last
    // doorbell has to be inside BAR 0: the driver writes it on every single
    // command. Nothing checked, and BAR 0's length was not even passed in ---
    // a fixed 32 KiB was mapped instead --- so a controller reporting a large
    // stride had its doorbells written outside its own window.
    let doorbells = NVME_REG_DBS
        .checked_add(2 * (io_queues + 1) * stride)
        .ok_or(DeviceError::InvalidParam)?;
    if doorbells > bar_len {
        warn!(
            "[nvme] {} queues at a doorbell stride of {:#x} need {:#x} bytes and BAR 0 has {:#x}",
            io_queues + 1,
            stride,
            doorbells,
            bar_len
        );
        return Err(DeviceError::NotSupported);
    }

    Ok(ControllerWindow {
        stride,
        admin_q_size: mqes.min(32),
        io_q_size: mqes.min(128),
        // CAP.TO is in 500 ms units; keep at least 1 s as a floor.
        ready_timeout_us: (((cap >> 24) & 0xff) * 500_000).max(1_000_000),
    })
}

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

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[0] {
            // The whole BAR, through the base the mapper returns, and its
            // length goes to the controller bring-up: see
            // `bus::resolve_window` and `controller_window`.
            let len = len as usize;
            let vaddr = crate::bus::resolve_window(mapper, addr as usize, len, 0);
            let vector = irq.map(|idx| idx + 32).unwrap_or(33);
            let blk = Arc::new(NvmeInterface::new(vaddr, len, vector)?);
            Ok(Device::Block(blk))
        } else {
            Err(crate::DeviceError::NotSupported)
        }
    }
}

#[cfg(test)]
mod command_tests {
    //! Host tests for the arithmetic behind an NVMe command.
    //!
    //! Like the AHCI driver next door, none of this runs in CI: eclipse boots
    //! in QEMU from an image in RAM and never issues a disk command, so the
    //! whole file is dead code there and alive on a real machine.
    //!
    //! What is checked is what the controller reads out of the submission
    //! queue entry. The two fields that decide where the data goes are PRP1
    //! and PRP2, and PRP2 means three different things depending on the
    //! size; the field that decides how much data moves is NLB, which is
    //! zero-based. Each of those is a way to read or write the wrong memory
    //! and have the command complete with no error.

    use super::*;

    // --- where the data goes ---

    #[test]
    fn a_transfer_of_one_page_or_less_does_not_use_the_second_pointer() {
        // PRP1 covers it all. Filling PRP2 anyway would be harmless on a
        // forgiving controller and a spec violation on a strict one.
        assert_eq!(prp2_for(0x20_0000, 512), Prp2::Unused);
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE), Prp2::Unused);
    }

    #[test]
    fn a_two_page_transfer_points_the_second_pointer_at_the_second_page() {
        // Not at a list: with only one page left there is nothing to list,
        // and the controller would read the data itself as addresses.
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE + 1),
            Prp2::SecondPage((0x20_0000 + PAGE_SIZE) as u64)
        );
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE * 2),
            Prp2::SecondPage((0x20_0000 + PAGE_SIZE) as u64)
        );
    }

    #[test]
    fn a_longer_transfer_switches_to_a_list_one_byte_later() {
        // The boundary is exactly two pages. One byte past it the meaning of
        // PRP2 changes completely, so this is where an off-by-one hides.
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE * 2 + 1), Prp2::List(2));
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE * 3), Prp2::List(2));
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE * 3 + 1), Prp2::List(3));
    }

    #[test]
    fn the_list_holds_one_entry_per_page_after_the_first() {
        // The whole 128 KiB bounce is 32 pages, so 31 entries. Counting the
        // first page twice would leave the last page of every large transfer
        // untransferred, silently.
        let pages = 32;
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE * pages),
            Prp2::List(pages - 1)
        );
        // A partial last page still needs its own entry.
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE * pages + 7),
            Prp2::List(pages)
        );
    }

    #[test]
    fn the_first_list_entry_is_the_second_page_not_the_first() {
        // Page 0 is already in PRP1. Starting the list at page 0 makes every
        // transfer over two pages read or write one page too early, which is
        // the classic way to scribble on the page before the buffer.
        let base = 0x20_0000;
        assert_eq!(prp_list_entry(base, 0), (base + PAGE_SIZE) as u64);
        assert_eq!(prp_list_entry(base, 1), (base + 2 * PAGE_SIZE) as u64);
        assert_eq!(prp_list_entry(base, 30), (base + 31 * PAGE_SIZE) as u64);
    }

    #[test]
    fn the_list_and_prp1_together_cover_the_whole_buffer_exactly_once() {
        // Walk the descriptors the way the controller does and check the
        // pages come out contiguous, in order, and with none repeated or
        // skipped.
        let base = 0x20_0000;
        for pages in 3..=32usize {
            let len = PAGE_SIZE * pages;
            let entries = match prp2_for(base, len) {
                Prp2::List(n) => n,
                other => panic!("{} pages should need a list, got {:?}", pages, other),
            };
            let mut seen = alloc::vec![base as u64];
            for i in 0..entries {
                seen.push(prp_list_entry(base, i));
            }
            assert_eq!(seen.len(), pages, "{} pages", pages);
            for (i, pa) in seen.iter().enumerate() {
                assert_eq!(*pa, (base + i * PAGE_SIZE) as u64, "{} pages", pages);
            }
        }
    }

    // --- how much data moves ---

    #[test]
    fn the_block_count_is_zero_based() {
        // NLB of 0 means one LBA. Sending the count itself transfers one LBA
        // too many, over the top of whatever follows the buffer.
        let c = io_chunk(512, 0, 512, 128 * 1024);
        assert_eq!((c.io_len, c.nlb), (512, 0));
        let c = io_chunk(4096, 0, 512, 128 * 1024);
        assert_eq!((c.io_len, c.nlb), (4096, 7));
    }

    #[test]
    fn a_whole_lba_transfer_hands_back_everything_it_moved() {
        let c = io_chunk(8192, 0, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take, c.nlb), (8192, 8192, 1));
    }

    #[test]
    fn the_command_is_capped_by_the_controllers_limit() {
        // MDTS is what the controller will accept; exceeding it is a command
        // error on every real device and accepted by most emulators.
        let c = io_chunk(1024 * 1024, 0, 512, 128 * 1024);
        assert_eq!(c.io_len, 128 * 1024);
        assert_eq!(c.nlb, (128 * 1024 / 512 - 1) as u16);
        let c = io_chunk(1024 * 1024, 0, 512, 8192);
        assert_eq!(c.io_len, 8192);
    }

    #[test]
    fn a_request_starting_inside_an_lba_reads_the_whole_lba_and_keeps_a_slice() {
        // A 4 KiB-formatted drive cannot address anything smaller. The
        // command has to be a whole LBA even though the caller wants 512
        // bytes out of the middle of it.
        let c = io_chunk(512, 1536, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take, c.nlb), (4096, 512, 0));
        // ...and never more of the LBA than is actually left in it.
        let c = io_chunk(8192, 3584, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take), (4096, 512));
    }

    #[test]
    fn a_request_ending_inside_an_lba_is_also_a_whole_lba_command() {
        let c = io_chunk(512, 0, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take, c.nlb), (4096, 512, 0));
    }

    #[test]
    fn a_command_always_moves_at_least_one_lba() {
        // io_len of zero would make the transfer loop spin for ever, and NLB
        // would be computed from a count of 0 -- an underflow that reads back
        // as 65536 blocks. Today the two bounds that prevent it are a cap on
        // the LBA size and a floor under MDTS, and at 8 KiB they meet exactly
        // with nothing to spare.
        for &lba_bytes in &[512usize, 1024, 2048, 4096, 8192] {
            for &chunk in &[512usize, 4096, 8192, 128 * 1024] {
                let c = io_chunk(1024 * 1024, 0, lba_bytes, chunk);
                assert_ne!(c.io_len, 0, "lba {} chunk {}", lba_bytes, chunk);
                assert_ne!(c.take, 0, "lba {} chunk {}", lba_bytes, chunk);
                assert_eq!(c.io_len % lba_bytes, 0, "lba {} chunk {}", lba_bytes, chunk);
                assert_eq!(c.io_len / lba_bytes, c.nlb as usize + 1);
            }
        }
    }

    #[test]
    fn every_command_is_a_whole_number_of_lbas() {
        for &lba_bytes in &[512usize, 4096] {
            for &remaining in &[1usize, 511, 512, 513, 4096, 4097, 100_000] {
                for &off in &[0usize, 1, 511, 512] {
                    if off >= lba_bytes {
                        continue;
                    }
                    let c = io_chunk(remaining, off, lba_bytes, 128 * 1024);
                    assert_eq!(c.io_len % lba_bytes, 0);
                    assert!(c.take <= c.io_len);
                    assert!(c.take <= remaining, "handed back more than was asked");
                    assert_ne!(c.take, 0, "the loop would not advance");
                }
            }
        }
    }

    // --- what the controller told us about itself ---

    #[test]
    fn an_mdts_of_zero_means_no_limit_not_no_transfers() {
        // Reading it as a size would cap every command at one byte and never
        // finish a read.
        assert_eq!(max_transfer_from_mdts(0), usize::MAX);
    }

    #[test]
    fn mdts_counts_pages_as_a_power_of_two() {
        assert_eq!(max_transfer_from_mdts(1), PAGE_SIZE * 2);
        assert_eq!(max_transfer_from_mdts(5), PAGE_SIZE * 32);
    }

    #[test]
    fn an_absurd_mdts_saturates_instead_of_wrapping_to_nothing() {
        // `checked_shl` guards the shift amount, not the value. A page is
        // 2^12, so from MDTS 52 upwards every bit falls off the top of a
        // 64-bit word and the "checked" answer is Some(0) -- a cap of zero
        // bytes. That stalls the transfer loop on the first read and
        // underflows the block count into 65536 blocks. Only MDTS 64 and
        // over hit the guard that was there.
        assert_eq!(max_transfer_from_mdts(255), usize::MAX);
        assert_eq!(max_transfer_from_mdts(64), usize::MAX);
        for mdts in 52..=63u8 {
            assert_eq!(
                max_transfer_from_mdts(mdts),
                usize::MAX,
                "MDTS {} came back as a cap of {} bytes",
                mdts,
                max_transfer_from_mdts(mdts)
            );
        }
        // Everything below the wrap is still its real value.
        assert_eq!(max_transfer_from_mdts(51), PAGE_SIZE << 51);
    }

    #[test]
    fn the_transfer_cap_never_comes_out_below_one_page() {
        // Whatever the controller reports, for every one of the 256 values
        // it can report. A cap under one page cannot describe even the
        // smallest command.
        for mdts in 0..=255u8 {
            assert!(
                max_transfer_from_mdts(mdts) >= PAGE_SIZE,
                "MDTS {} gave {}",
                mdts,
                max_transfer_from_mdts(mdts)
            );
        }
    }

    #[test]
    fn the_transfer_cap_is_never_smaller_than_the_largest_lba_we_accept() {
        // This is the pairing `io_chunk` depends on. If either side moves,
        // `chunk / lba_bytes` comes out zero and the driver hangs on the
        // first read of the root filesystem.
        let largest_lba = 1usize << 13;
        for mdts in 0..=8u8 {
            assert!(
                max_transfer_from_mdts(mdts) >= largest_lba,
                "mdts {} allows less than one LBA",
                mdts
            );
        }
    }

    #[test]
    fn only_lba_sizes_the_driver_can_actually_handle_are_accepted() {
        // Below 512 the 512-byte block API above cannot address a whole LBA;
        // above 8 KiB one LBA no longer fits the bounce buffer comfortably,
        // and the partial-update path reads a whole one.
        assert!(!lba_shift_supported(8));
        assert!(lba_shift_supported(9));
        assert!(lba_shift_supported(12));
        assert!(lba_shift_supported(13));
        assert!(!lba_shift_supported(14));
        assert!(!lba_shift_supported(0));
    }

    #[test]
    fn the_namespace_size_is_scaled_into_512_byte_sectors() {
        // The drive counts in its own LBAs. Taking the number at face value
        // on a 4 KiB drive makes seven eighths of the disk unreachable.
        assert_eq!(capacity_in_sectors(1000, 9), 1000);
        assert_eq!(capacity_in_sectors(1000, 12), 8000);
        assert_eq!(capacity_in_sectors(1000, 13), 16000);
    }

    // --- the request the filesystem hands us ---

    #[test]
    fn a_request_past_the_end_of_the_namespace_is_refused() {
        assert!(check_request(0, 512, 1000).is_ok());
        assert!(check_request(999, 512, 1000).is_ok());
        assert!(check_request(1000, 512, 1000).is_err());
        assert!(check_request(999, 1024, 1000).is_err());
    }

    #[test]
    fn a_request_that_is_not_a_whole_number_of_blocks_is_refused() {
        assert!(check_request(0, 0, 1000).is_err());
        assert!(check_request(0, 511, 1000).is_err());
        assert!(check_request(0, 513, 1000).is_err());
    }

    #[test]
    fn a_request_that_would_wrap_round_is_refused_rather_than_accepted() {
        // The sum overflowing would come out small and pass a naive bounds
        // check, aiming the transfer at the start of the namespace.
        assert!(check_request(usize::MAX, 512, usize::MAX).is_err());
        assert!(check_request(usize::MAX - 1, 4096, usize::MAX).is_err());
    }
}

#[cfg(test)]
mod cap_tests {
    //! What the controller says about itself, and what the driver does with it.
    //!
    //! CAP is one 64-bit register read out of the device's own BAR, and two of
    //! its fields address memory: MQES sizes the queues and DSTRD places the
    //! doorbells the driver writes on every command. Neither was checked, and
    //! the length of BAR 0 was not even passed in.

    use super::*;

    /// CAP as a controller reports it: `mqes` is the raw field (entries minus
    /// one), `dstrd` the raw stride exponent, `to` the raw 500 ms units.
    fn cap(mqes: u64, dstrd: u64, to: u64) -> u64 {
        (dstrd << 32) | (to << 24) | mqes
    }

    /// Room for the doorbells of every queue at `stride`, which is what
    /// [`controller_window`] demands of BAR 0.
    fn room_for(stride: usize) -> usize {
        NVME_REG_DBS + 2 * (IO_QUEUES + 1) * stride
    }

    #[test]
    fn what_a_normal_controller_reports() {
        // 2048 entries, the usual 4-byte stride, 15 s to become ready.
        let w = controller_window(cap(0x7ff, 0, 30), 0x4000, IO_QUEUES).expect("a normal NVMe");
        assert_eq!(
            w,
            ControllerWindow {
                stride: 4,
                admin_q_size: 32,
                io_q_size: 128,
                ready_timeout_us: 15_000_000,
            }
        );
    }

    #[test]
    fn a_controller_that_says_one_queue_entry_is_refused() {
        // A queue of one entry is full the moment anything is in it, so not one
        // command would ever be submitted and the driver would wait for a
        // completion the controller never got asked for.
        assert_eq!(
            controller_window(cap(0, 0, 30), 0x4000, IO_QUEUES).err(),
            Some(DeviceError::NotSupported)
        );
    }

    #[test]
    fn two_entries_is_the_smallest_it_accepts() {
        let w = controller_window(cap(1, 0, 30), 0x4000, IO_QUEUES).expect("two entries");
        assert_eq!((w.admin_q_size, w.io_q_size), (2, 2));
    }

    #[test]
    fn the_queue_sizes_are_clamped_to_what_the_driver_uses() {
        let w = controller_window(cap(0xffff, 0, 30), 0x4000, IO_QUEUES).expect("65536 entries");
        assert_eq!((w.admin_q_size, w.io_q_size), (32, 128));
    }

    #[test]
    fn a_stride_that_puts_the_doorbells_outside_the_bar_is_refused() {
        // DSTRD is four bits, so a controller can ask for 4 << 15 = 128 KiB
        // between doorbells. The driver writes the I/O queue's doorbell at
        // 0x1000 + 2 * 128 KiB, well past a 16 KiB BAR.
        assert_eq!(
            controller_window(cap(0x7ff, 0xf, 30), 0x4000, IO_QUEUES).err(),
            Some(DeviceError::NotSupported)
        );
        // The same controller behind a BAR that does hold them is fine: it is
        // the window that decides, not the stride on its own.
        let w = controller_window(cap(0x7ff, 0xf, 30), room_for(4 << 0xf), IO_QUEUES)
            .expect("a BAR big enough");
        assert_eq!(w.stride, 4 << 0xf);
    }

    #[test]
    fn the_guard_covers_the_doorbell_the_driver_actually_writes() {
        // `nvme_write_doorbell` puts the submission doorbell at
        // `0x1000 + 2 * qid * stride` and the completion one a stride further.
        // For the highest queue id, that last dword has to be inside the room
        // the guard asks for, and one byte less has to be refused: this is what
        // keeps the guard and the formula from drifting apart.
        for dstrd in 0..=3u64 {
            let stride = 4usize << dstrd;
            let room = room_for(stride);
            let w = controller_window(cap(0x7ff, dstrd, 30), room, IO_QUEUES)
                .expect("exactly enough room");
            assert_eq!(w.stride, stride);
            let last_doorbell = NVME_REG_DBS + 2 * IO_QUEUES * w.stride + w.stride;
            assert!(
                last_doorbell + 4 <= room,
                "the last doorbell at {:#x} does not fit in {:#x}",
                last_doorbell,
                room
            );
            assert!(
                controller_window(cap(0x7ff, dstrd, 30), room - 1, IO_QUEUES).is_err(),
                "one byte short of the doorbells was accepted at stride {:#x}",
                stride
            );
        }
    }

    #[test]
    fn more_io_queues_need_more_room() {
        let stride = 4;
        assert!(controller_window(cap(0x7ff, 0, 30), room_for(stride), 1).is_ok());
        assert!(controller_window(cap(0x7ff, 0, 30), room_for(stride), 8).is_err());
    }

    #[test]
    fn the_ready_timeout_is_five_hundred_milliseconds_a_unit_with_a_floor() {
        let w = controller_window(cap(0x7ff, 0, 4), 0x4000, IO_QUEUES).unwrap();
        assert_eq!(w.ready_timeout_us, 2_000_000);
        // A controller that reports no timeout at all still gets a second:
        // CSTS.RDY takes as long as it takes.
        let w = controller_window(cap(0x7ff, 0, 0), 0x4000, IO_QUEUES).unwrap();
        assert_eq!(w.ready_timeout_us, 1_000_000);
        // ...and one unit is below the floor too.
        let w = controller_window(cap(0x7ff, 0, 1), 0x4000, IO_QUEUES).unwrap();
        assert_eq!(w.ready_timeout_us, 1_000_000);
    }

    #[test]
    fn nothing_above_the_four_bits_of_dstrd_reaches_the_stride() {
        // The bits above DSTRD in CAP are other fields (CSS, MPSMIN, ...): a
        // stride read too wide would be enormous and the doorbells would land
        // outside any BAR.
        let with_rubbish_above = cap(0x7ff, 0, 30) | (0xffff_ffffu64 << 36);
        let w = controller_window(with_rubbish_above, 0x4000, IO_QUEUES).expect("stride of 4");
        assert_eq!(w.stride, 4);
    }
}

#[cfg(test)]
mod command_layout_tests {
    //! The commands themselves, as bytes the controller reads back out of
    //! DMA memory. `command_tests` above covers the arithmetic that fills
    //! them in; this covers what they say.
    //!
    //! Nothing in here is computed: these are the numbers NVM Express gives
    //! each command, and the driver fills a struct with them and hands the
    //! controller the address. So the failure mode is not a wrong answer, it
    //! is a different command carried out in full -- a read issued as a
    //! write overwrites the sector it was asked to fetch, and the two
    //! opcodes are one digit apart. Nothing else in this file can tell:
    //! every path that builds one of these ends at a doorbell.

    use super::*;

    #[test]
    fn the_admin_opcodes_are_the_ones_the_specification_assigns() {
        // NVM Express 1.4, figure 139 (admin command set).
        assert_eq!(NvmeIdentify::new().opcode, 0x06, "Identify is 06h");
        assert_eq!(
            NvmeCreateCq::new().opcode,
            0x05,
            "Create I/O Completion Queue is 05h"
        );
        assert_eq!(
            NvmeCreateSq::new().opcode,
            0x01,
            "Create I/O Submission Queue is 01h"
        );
        // The two queue commands share the admin opcode space with the NVM
        // read and write below, which is why each is pinned here by name.
        assert_ne!(NvmeCreateCq::new().opcode, NvmeCreateSq::new().opcode);
    }

    #[test]
    fn a_read_command_is_not_a_write_command() {
        // NVM Express 1.4, figure 348 (NVM command set).
        assert_eq!(
            NvmeRWCommand::new_read_command().opcode,
            0x02,
            "Read is 02h"
        );
        assert_eq!(
            NvmeRWCommand::new_write_command().opcode,
            0x01,
            "Write is 01h"
        );
    }

    #[test]
    fn every_command_fills_the_slot_it_is_copied_into() {
        // A submission queue entry is 64 bytes and a completion 16, and the
        // driver copies these structs straight into the ring. One byte of
        // padding anywhere and every field past it lands in the controller's
        // next field -- and every entry after this one starts in the wrong
        // place.
        assert_eq!(core::mem::size_of::<NvmeCommonCommand>(), 64);
        assert_eq!(core::mem::size_of::<NvmeIdentify>(), 64);
        assert_eq!(core::mem::size_of::<NvmeCreateCq>(), 64);
        assert_eq!(core::mem::size_of::<NvmeCreateSq>(), 64);
        assert_eq!(core::mem::size_of::<NvmeRWCommand>(), 64);
        assert_eq!(core::mem::size_of::<NvmeCompletion>(), 16);
    }

    #[test]
    fn the_opcode_is_the_first_byte_of_every_command() {
        // The controller reads the opcode at offset 0 of the entry, whatever
        // the struct behind it looks like.
        assert_eq!(core::mem::offset_of!(NvmeCommonCommand, opcode), 0);
        assert_eq!(core::mem::offset_of!(NvmeIdentify, opcode), 0);
        assert_eq!(core::mem::offset_of!(NvmeCreateCq, opcode), 0);
        assert_eq!(core::mem::offset_of!(NvmeCreateSq, opcode), 0);
        assert_eq!(core::mem::offset_of!(NvmeRWCommand, opcode), 0);
        // And the data pointers sit at dwords 6 and 8, where the read and
        // write path puts the buffer's physical address.
        assert_eq!(core::mem::offset_of!(NvmeRWCommand, prp1), 24);
        assert_eq!(core::mem::offset_of!(NvmeRWCommand, prp2), 32);
        assert_eq!(core::mem::offset_of!(NvmeRWCommand, slba), 40);
    }
}

#[cfg(test)]
mod completion_layout_tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    /// The completion queue entry the controller writes by DMA.
    ///
    /// The driver does not build this: the device does, at the offsets the
    /// NVMe spec fixes, and the driver reads it out of a queue the device
    /// owns. So nothing inside the driver can disagree with itself about the
    /// order -- every read goes through the field name -- and the three
    /// two-byte fields in the tail are all the same width. The size was
    /// checked; sixteen bytes stay sixteen bytes however they are arranged.
    ///
    /// Getting it wrong is quiet in the worst way: `command_id` is how the
    /// driver matches a completion to the request that is waiting for it, so a
    /// swap hands each waiter somebody else's answer rather than failing.
    #[test]
    fn the_completion_entry_is_the_sixteen_bytes_of_the_nvme_spec() {
        assert_eq!(size_of::<NvmeCompletion>(), 16);
        assert_eq!(align_of::<NvmeCompletion>(), 8);
        assert_eq!(offset_of!(NvmeCompletion, result), 0);
        assert_eq!(offset_of!(NvmeCompletion, sq_head), 8);
        assert_eq!(offset_of!(NvmeCompletion, sq_id), 10);
        assert_eq!(offset_of!(NvmeCompletion, command_id), 12);
        assert_eq!(offset_of!(NvmeCompletion, status), 14);
    }

    /// Dwords two and three of the entry: the queue bookkeeping and then the
    /// identity and the verdict, four two-byte fields in eight bytes.
    #[test]
    fn the_last_two_dwords_are_four_halves_with_no_padding() {
        assert_eq!(
            size_of::<NvmeCompletion>() - offset_of!(NvmeCompletion, sq_head),
            8
        );
        assert_eq!(offset_of!(NvmeCompletion, status) + 2, 16);
    }
}

#[cfg(test)]
mod controller_tests {
    //! Host tests for the bodies: bring-up, the completion wait, and the
    //! block API that runs on top of them.
    //!
    //! `command_tests` above covers every pure helper in this file and not one
    //! of the functions that call them, because reaching those needs a
    //! controller. There is one here, and two properties of the driver build
    //! it. The BAR is only ever read and written volatile at `bar + offset`,
    //! so a leaked window of words is a register file; and the completion
    //! queue is *host* memory the controller DMAs into, so a test can post the
    //! completion itself -- which is the whole of what `wait_cq` waits for.
    //!
    //! The clock is the other half. `test_clock` is per-thread, so a frozen
    //! clock means no deadline can fire and a function that returns did so
    //! because the controller answered, while a galloping clock means the
    //! deadline fires on the first look, which is how the timeouts are
    //! reached without waiting for them.

    use super::*;
    use crate::nvme::nvme_queue::test_clock;
    use crate::utils::host_hooks::alone_with_the_allocator;

    /// The fake BAR, long enough for the doorbells of the admin queue and one
    /// I/O queue at the minimum stride.
    const BAR_BYTES: usize = 0x2000;
    /// Doorbell stride, `4 << CAP.DSTRD` with DSTRD zero.
    const STRIDE: usize = 4;
    /// Entries in each queue.
    const ENTRIES: usize = 32;
    /// Namespace size in the 512-byte sectors the block API speaks.
    const SECTORS: usize = 4096;

    /// A completion whose status says "mine, and it went well". Bit 0 is the
    /// phase, which a fresh queue expects to be 1; bits 1..9 are the status
    /// code and 9..12 its type, and zero in both is success.
    const OK: u16 = 1;
    /// The same, reporting an error. The command is over either way, which is
    /// what makes it safe to re-issue.
    const FAILED: u16 = 1 | (0x04 << 1);

    /// The address of a leaked register window, which is all a BAR ever is to
    /// this driver. Only the address comes back, so nothing here holds a
    /// reference to the memory the driver reads and writes volatile; and it is
    /// a block of `u64`, because CAP, ASQ and ACQ are read and written eight
    /// bytes at a time.
    fn a_window() -> usize {
        let window: &'static mut [u64] = alloc::vec![0u64; BAR_BYTES / 8].leak();
        window.as_mut_ptr() as usize
    }

    fn reg(base: usize, at: usize) -> u32 {
        // SAFETY: `base` came from `a_window` and `at` is inside it.
        unsafe { read_volatile((base + at) as *const u32) }
    }

    fn set_reg(base: usize, at: usize, value: u32) {
        // SAFETY: as `reg`.
        unsafe { write_volatile((base + at) as *mut u32, value) }
    }

    fn reg64(base: usize, at: usize) -> u64 {
        // SAFETY: as `reg`, and the window is eight-byte aligned.
        unsafe { read_volatile((base + at) as *const u64) }
    }

    /// Where the driver writes the new submission tail of queue `qid`.
    fn sq_doorbell(base: usize, qid: usize) -> u32 {
        reg(base, NVME_REG_DBS + 2 * qid * STRIDE)
    }

    /// Where it writes the consumed completion head.
    fn cq_doorbell(base: usize, qid: usize) -> u32 {
        reg(base, NVME_REG_DBS + 2 * qid * STRIDE + STRIDE)
    }

    type Queue = Arc<Mutex<NvmeQueue<ProviderImpl>>>;

    fn a_queue(qid: usize) -> Queue {
        Arc::new(Mutex::new(
            NvmeQueue::new(qid, ENTRIES).expect("the shim serves four regions"),
        ))
    }

    /// The driver as `new` leaves it just before `nvme_configure_admin_queue`:
    /// queues built, nothing asked of the controller yet.
    fn a_drive_not_yet_brought_up(base: usize) -> NvmeInterface {
        NvmeInterface {
            name: String::from("nvme"),
            admin_queue: a_queue(0),
            io_queues: alloc::vec![a_queue(1)],
            bar: base,
            stride: STRIDE,
            irq: 11,
            capacity: 0,
            lba_shift: 9,
            max_transfer: PAGE_SIZE,
            poisoned: AtomicBool::new(false),
        }
    }

    /// The driver as bring-up leaves it: a namespace of 512-byte blocks and a
    /// per-command cap as big as the bounce buffer.
    fn a_drive(base: usize) -> NvmeInterface {
        let mut drive = a_drive_not_yet_brought_up(base);
        drive.capacity = SECTORS;
        drive.max_transfer = PAGE_SIZE * 32;
        drive
    }

    /// Post one completion into `queue`'s completion queue, which is what the
    /// controller's DMA engine would have done.
    fn a_completion_at(queue: &Queue, head: usize, cid: u16, status: u16) {
        let mut q = queue.lock();
        q.cq[head].write(NvmeCompletion {
            command_id: cid,
            status,
            ..Default::default()
        });
    }

    /// Post one completion per status, in order, for the command ids
    /// `next_cid` is about to hand out.
    fn the_controller_answers(queue: &Queue, statuses: &[u16]) {
        let (head, first_cid) = {
            let q = queue.lock();
            (q.cq_head, q.cid_counter.wrapping_add(1))
        };
        for (i, status) in statuses.iter().enumerate() {
            a_completion_at(queue, head + i, first_cid.wrapping_add(i as u16), *status);
        }
    }

    /// Fill the queue's bounce buffer the way a read command's DMA would.
    fn the_controller_leaves(queue: &Queue, at: usize, bytes: &[u8]) {
        let q = queue.lock();
        // SAFETY: the bounce is 32 pages and `at + len` stays inside it.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), (q.data_va + at) as *mut u8, bytes.len())
        }
    }

    /// What is in the bounce buffer now, which for a write is what the
    /// controller would have read out of it.
    fn the_bounce(queue: &Queue, at: usize, len: usize) -> Vec<u8> {
        let q = queue.lock();
        // SAFETY: as `the_controller_leaves`.
        unsafe { core::slice::from_raw_parts((q.data_va + at) as *const u8, len) }.to_vec()
    }

    fn sq_entry(queue: &Queue, at: usize) -> NvmeCommonCommand {
        let q = queue.lock();
        q.sq[at].read()
    }

    /// How many commands have been submitted on this queue.
    fn submitted(queue: &Queue) -> usize {
        queue.lock().sq_tail
    }

    /// The same sixty-four bytes read back as the command they are. `io_rw`
    /// and the two bring-up steps build their entries by transmuting the
    /// other way, so this is that transmute undone.
    fn as_rw(cmd: NvmeCommonCommand) -> NvmeRWCommand {
        // SAFETY: both are `#[repr(C)]` and sixty-four bytes wide.
        unsafe { core::mem::transmute(cmd) }
    }

    fn as_identify(cmd: NvmeCommonCommand) -> NvmeIdentify {
        // SAFETY: as `as_rw`.
        unsafe { core::mem::transmute(cmd) }
    }

    fn as_create_cq(cmd: NvmeCommonCommand) -> NvmeCreateCq {
        // SAFETY: as `as_rw`.
        unsafe { core::mem::transmute(cmd) }
    }

    fn as_create_sq(cmd: NvmeCommonCommand) -> NvmeCreateSq {
        // SAFETY: as `as_rw`.
        unsafe { core::mem::transmute(cmd) }
    }

    /// The Identify payload the driver reads back, written into the one bounce
    /// buffer both Identify commands share. That sharing is why the model
    /// string and the namespace's format byte overlap: byte 26 of the buffer
    /// is the third character of the model *and* FLBAS, so the character has
    /// to be one whose low nibble is a format index that exists.
    fn an_identified_namespace(queue: &Queue, nsze: u64, lbads: u8, mdts: u8) {
        let q = queue.lock();
        // SAFETY: the bounce is 32 pages, far more than the 4 KiB Identify
        // returns.
        let data = unsafe { core::slice::from_raw_parts_mut(q.data_va as *mut u8, 4096) };
        data[..8].copy_from_slice(&nsze.to_le_bytes());
        let model = b"EC0IPSE FAKE NVME";
        data[24..24 + model.len()].copy_from_slice(model);
        for byte in data[24 + model.len()..64].iter_mut() {
            *byte = b' ';
        }
        data[77] = mdts;
        data[128..132].copy_from_slice(&((lbads as u32) << 16).to_le_bytes());
    }

    /// The one thing a window of memory cannot do: answer. A real controller
    /// raises CSTS.RDY *because* the host wrote CC.EN, so the driver's two
    /// waits are one handshake and a passive window can only ever satisfy one
    /// of them. This watches for the enable and answers it.
    ///
    /// It gives up after a bounded wait and answers anyway, so a test that
    /// never writes CC fails on the flag it returns instead of hanging; and
    /// the driver's own clock is left galloping, so even then the wait ends.
    fn a_controller_that_answers(base: usize) -> std::thread::JoinHandle<bool> {
        std::thread::spawn(move || {
            let mut saw_the_enable = false;
            for _ in 0..1_000_000u32 {
                if reg(base, NVME_REG_CC) & NVME_CC_ENABLE != 0 {
                    saw_the_enable = true;
                    break;
                }
                std::thread::yield_now();
            }
            set_reg(base, NVME_REG_CSTS, NVME_CSTS_RDY);
            saw_the_enable
        })
    }

    /// A clock that moves a microsecond per look, with a five second deadline
    /// on top: more than enough room for the answering thread, and still
    /// bounded if it never answers.
    fn a_patient_clock() -> u64 {
        test_clock::set(0);
        test_clock::set_auto_advance(1);
        5_000_000
    }

    /// A clock that jumps ten seconds per look, so the first deadline check
    /// fails and every timeout is reached at once.
    fn a_galloping_clock() {
        test_clock::set(0);
        test_clock::set_auto_advance(10_000_000);
    }

    /// A clock that moves a second per look. Every wait below is satisfied by
    /// a completion that is already there, so the clock does not matter to
    /// what is being tested -- but it means a test that breaks ends on a
    /// deadline in a handful of looks instead of spinning out the hard cap.
    fn a_quick_clock() {
        test_clock::set(0);
        test_clock::set_auto_advance(1_000_000);
    }

    /// Microseconds the driver's waits have consumed, read without moving the
    /// clock any further.
    fn clock_spent() -> u64 {
        test_clock::set_auto_advance(0);
        test_clock::now()
    }

    // --- bring-up ---

    #[test]
    fn a_bar_too_small_for_the_doorbells_is_refused() {
        alone_with_the_allocator(|| {
            let base = a_window();
            // CAP: MQES 31 (32 entries), DSTRD 0 (a 4-byte stride), TO 2.
            // SAFETY: the window is eight-byte aligned.
            unsafe { write_volatile(base as *mut u64, 0x0000_0002_0000_001f) };

            // Two queues at a four-byte stride put the last doorbell at
            // 0x1010, which does not fit in sixteen bytes of BAR. Writing it
            // there would land outside the controller's own window, on every
            // single command.
            assert!(matches!(
                NvmeInterface::new(base, 0x10, 11),
                Err(DeviceError::NotSupported)
            ));
            // And it was refused before taking a single DMA region, which is
            // the point of reading CAP first: a probe that fails should not
            // have to give anything back.
            assert_eq!(
                crate::utils::host_hooks::ALLOC_CALLS.load(Ordering::SeqCst),
                0
            );
        })
    }

    #[test]
    fn a_ready_bit_that_never_clears_stops_the_bring_up_before_the_queues_are_handed_over() {
        alone_with_the_allocator(|| {
            let base = a_window();
            // A controller wedged with RDY set, and a CC that already carries
            // a setting the reset must not disturb.
            set_reg(base, NVME_REG_CSTS, NVME_CSTS_RDY);
            set_reg(base, NVME_REG_CC, NVME_CC_ENABLE | NVME_CC_IOSQES);
            a_galloping_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            assert_eq!(
                drive.nvme_configure_admin_queue(1_000),
                Err(DeviceError::IoError)
            );

            // The enable bit was taken away and nothing else was,
            assert_eq!(reg(base, NVME_REG_CC), NVME_CC_IOSQES);
            // and the queues were never handed over, which is the point of
            // giving up here: a controller that has not let go of its old
            // configuration must not be told where the new rings are.
            assert_eq!(reg(base, NVME_REG_AQA), 0);
            assert_eq!(reg64(base, NVME_REG_ASQ), 0);
            assert_eq!(reg64(base, NVME_REG_ACQ), 0);
            assert_eq!(reg(base, NVME_REG_INTMS), 0);
        })
    }

    #[test]
    fn the_admin_queue_is_handed_over_and_the_controller_told_how_to_read_it() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_galloping_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            let (sq_pa, cq_pa, entries) = {
                let q = drive.admin_queue.lock();
                (q.sq_pa as u64, q.cq_pa as u64, q.sq.len())
            };

            // RDY never rises, so this ends in the enable timeout -- after
            // the programming below, which is what is being read back.
            assert_eq!(
                drive.nvme_configure_admin_queue(1_000),
                Err(DeviceError::IoError)
            );

            // Both halves of AQA are the queue depth minus one, because the
            // field is zero-based: the depth itself asks for one entry more
            // than exists.
            let aqa = reg(base, NVME_REG_AQA);
            assert_eq!(aqa & 0xfff, (entries - 1) as u32);
            assert_eq!((aqa >> 16) & 0xfff, (entries - 1) as u32);

            // The two rings sit at their own physical addresses, not at one
            // address twice.
            assert_eq!(reg64(base, NVME_REG_ASQ), sq_pa);
            assert_eq!(reg64(base, NVME_REG_ACQ), cq_pa);
            assert_ne!(sq_pa, cq_pa);

            // CC, field by field. The two entry-size fields are log2 of the
            // size, so they are checked against the structs the driver
            // actually puts in the rings.
            let cc = reg(base, NVME_REG_CC);
            assert_eq!(cc & 1, 1, "enabled");
            assert_eq!((cc >> 4) & 0x7, 0, "the NVM command set");
            assert_eq!((cc >> 7) & 0xf, 0, "4 KiB pages");
            assert_eq!((cc >> 11) & 0x7, 0, "round robin arbitration");
            assert_eq!((cc >> 14) & 0x3, 0, "no shutdown asked for");
            assert_eq!(
                1usize << ((cc >> 16) & 0xf),
                core::mem::size_of::<NvmeCommonCommand>(),
                "the submission entry size the driver writes"
            );
            assert_eq!(
                1usize << ((cc >> 20) & 0xf),
                core::mem::size_of::<NvmeCompletion>(),
                "the completion entry size the driver reads"
            );

            // And nothing was asked of the controller: the interrupt mask
            // comes after it reports ready.
            assert_eq!(reg(base, NVME_REG_INTMS), 0);
            assert_eq!(submitted(&drive.admin_queue), 0);
        })
    }

    #[test]
    fn a_controller_that_reports_a_fatal_status_while_enabling_is_given_up_on_at_once() {
        alone_with_the_allocator(|| {
            let base = a_window();
            // RDY clear so the reset passes, CFS set so the enable cannot.
            set_reg(base, NVME_REG_CSTS, NVME_CSTS_CFS);
            // A clock that crawls, against a deadline five million microseconds
            // away: reaching the deadline would take five million looks, so a
            // clock that barely moved is the proof this returned on the fatal
            // status and not on the timeout.
            let deadline = a_patient_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            assert_eq!(
                drive.nvme_configure_admin_queue(deadline),
                Err(DeviceError::IoError)
            );
            assert!(
                clock_spent() < 1_000,
                "gave up on the deadline, not on the fatal status: {} us",
                clock_spent()
            );
        })
    }

    #[test]
    fn a_controller_that_answers_the_enable_gets_its_interrupts_masked_and_is_identified() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let deadline = a_patient_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK]);
            an_identified_namespace(&drive.admin_queue, 1024, 9, 5);
            let answered = a_controller_that_answers(base);

            assert_eq!(drive.nvme_configure_admin_queue(deadline), Ok(()));
            assert!(
                answered.join().expect("the controller thread lived"),
                "the fake controller answered before the enable was written"
            );

            // We poll for completions, so every controller interrupt is
            // masked -- and only once the controller is ready to be written.
            assert_eq!(reg(base, NVME_REG_INTMS), 0xffff_ffff);

            // Two Identify commands went out: the controller, then namespace
            // one. Both read into the queue's bounce buffer.
            let data_pa = drive.admin_queue.lock().data_pa as u64;
            assert_eq!(submitted(&drive.admin_queue), 2);
            let controller = as_identify(sq_entry(&drive.admin_queue, 0));
            assert_eq!(controller.opcode, 0x06);
            assert_eq!(controller.cns, 1, "identify the controller");
            assert_eq!(controller.nsid, 0);
            assert_eq!(controller.prp1, data_pa);
            let namespace = as_identify(sq_entry(&drive.admin_queue, 1));
            assert_eq!(namespace.cns, 0, "identify a namespace");
            assert_eq!(namespace.nsid, 1);
            assert_eq!(namespace.prp1, data_pa);

            // Both completions were consumed and the controller told so.
            assert_eq!(cq_doorbell(base, 0), 2);
        })
    }

    #[test]
    fn the_namespace_size_and_block_size_come_out_of_identify() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let deadline = a_patient_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK]);
            // 1024 blocks of 4 KiB, and a transfer cap of 2^5 pages.
            an_identified_namespace(&drive.admin_queue, 1024, 12, 5);
            let answered = a_controller_that_answers(base);

            assert_eq!(drive.nvme_configure_admin_queue(deadline), Ok(()));
            answered.join().expect("the controller thread lived");

            // The block API counts in 512-byte sectors and the drive counts
            // in its own 4 KiB blocks, so the capacity is eight times the
            // block count -- without the scaling seven eighths of the disk
            // would be unreachable.
            assert_eq!(drive.logical_block_size(), 4096);
            assert_eq!(drive.block_count(), 1024 * 8);
            // MDTS 5 means 2^5 pages of the controller's minimum page size.
            assert_eq!(drive.max_transfer, 32 * 4096);
        })
    }

    #[test]
    fn a_controller_with_no_transfer_limit_is_not_given_one() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let deadline = a_patient_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK]);
            an_identified_namespace(&drive.admin_queue, 1024, 9, 0);
            let answered = a_controller_that_answers(base);

            assert_eq!(drive.nvme_configure_admin_queue(deadline), Ok(()));
            answered.join().expect("the controller thread lived");

            // MDTS zero means no limit at all, not "no transfers". Read as a
            // limit it would be a cap of zero bytes, which stalls the
            // transfer loop and underflows the block count.
            assert_eq!(drive.max_transfer, usize::MAX);
        })
    }

    #[test]
    fn a_namespace_of_zero_size_is_refused() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let deadline = a_patient_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK]);
            an_identified_namespace(&drive.admin_queue, 0, 9, 5);
            let answered = a_controller_that_answers(base);

            assert_eq!(
                drive.nvme_configure_admin_queue(deadline),
                Err(DeviceError::NoResources)
            );
            answered.join().expect("the controller thread lived");
            assert_eq!(drive.block_count(), 0);
        })
    }

    #[test]
    fn a_block_size_this_driver_cannot_work_with_is_refused() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let deadline = a_patient_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK]);
            // 2^14 is 16 KiB, twice what the read-modify-write path will take.
            an_identified_namespace(&drive.admin_queue, 1024, 14, 5);
            let answered = a_controller_that_answers(base);

            assert_eq!(
                drive.nvme_configure_admin_queue(deadline),
                Err(DeviceError::NotSupported)
            );
            answered.join().expect("the controller thread lived");
            // The refusal left the defaults alone rather than half-applying
            // a format it cannot serve.
            assert_eq!(drive.logical_block_size(), 512);
        })
    }

    #[test]
    fn the_io_queue_is_asked_for_and_then_created_in_the_right_order() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK, OK]);

            assert_eq!(drive.nvme_alloc_io_queue(), Ok(()));

            assert_eq!(submitted(&drive.admin_queue), 3);
            // Set Features first: a queue cannot be created before the
            // controller has agreed to have one.
            let features = sq_entry(&drive.admin_queue, 0);
            assert_eq!(features.opcode, 0x09);
            assert_eq!(features.cdw10, 0x07, "feature 7, the number of queues");
            // Then the completion queue, then the submission queue that
            // reports into it -- the other order names a queue that does not
            // exist yet.
            assert_eq!(as_create_cq(sq_entry(&drive.admin_queue, 1)).opcode, 0x05);
            assert_eq!(as_create_sq(sq_entry(&drive.admin_queue, 2)).opcode, 0x01);
            assert_eq!(cq_doorbell(base, 0), 3);
        })
    }

    #[test]
    fn the_io_queue_is_created_over_the_memory_the_driver_allocated_for_it() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK, OK]);
            let (sq_pa, cq_pa, entries) = {
                let q = drive.io_queues[0].lock();
                (q.sq_pa as u64, q.cq_pa as u64, q.sq.len())
            };

            assert_eq!(drive.nvme_alloc_io_queue(), Ok(()));

            let cq = as_create_cq(sq_entry(&drive.admin_queue, 1));
            assert_eq!(cq.prp1, cq_pa);
            assert_eq!(cq.cqid, 1);
            assert_eq!(cq.qsize, (entries - 1) as u16, "zero-based, like AQA");
            assert_eq!(cq.cq_flags & 1, 1, "physically contiguous");
            // Interrupts stay off: the driver polls.
            assert_eq!(cq.cq_flags & 2, 0, "no interrupt vector");

            let sq = as_create_sq(sq_entry(&drive.admin_queue, 2));
            assert_eq!(sq.prp1, sq_pa);
            assert_eq!(sq.sqid, 1);
            assert_eq!(sq.cqid, 1, "reports into the queue just created");
            assert_eq!(sq.qsize, (entries - 1) as u16);
            assert_eq!(sq.sq_flags & 1, 1, "physically contiguous");
            assert_ne!(sq_pa, cq_pa);
        })
    }

    // --- waiting for a completion ---

    #[test]
    fn a_completion_for_another_command_is_consumed_and_the_wait_goes_on() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();

            // A completion for a command nobody is waiting for sits in front
            // of ours. Abandoning the wait there would leave the head and the
            // phase one behind the controller for ever, so it is consumed and
            // the wait carries on.
            a_completion_at(&io, 0, 99, OK);
            a_completion_at(&io, 1, 1, OK);

            assert_eq!(drive.flush(), Ok(()));
            // Both were consumed, and the controller was told so -- that is
            // what keeps the queue in step.
            assert_eq!(io.lock().cq_head, 2);
            assert_eq!(cq_doorbell(base, 1), 2);
            assert_eq!(submitted(&io), 1, "and ours was not sent twice");
        })
    }

    #[test]
    fn an_error_completion_is_retried() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[FAILED, OK]);

            // An error completion means the command is over, so the buffer is
            // ours again and the identical command can go out once more.
            assert_eq!(drive.flush(), Ok(()));
            assert_eq!(submitted(&io), 2);
            assert_eq!(sq_doorbell(base, 1), 2);
            assert!(!drive.poisoned.load(Ordering::Relaxed));
        })
    }

    #[test]
    fn an_error_completion_on_every_attempt_gives_up_without_poisoning() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[FAILED, FAILED, FAILED, FAILED, OK]);

            assert_eq!(drive.flush(), Err(DeviceError::IoError));
            // Four attempts and no more: the fifth completion is still
            // sitting there untouched.
            assert_eq!(submitted(&io), 4);
            // And the device is still usable, because every one of those
            // commands finished. This is the whole difference from a timeout.
            assert!(!drive.poisoned.load(Ordering::Relaxed));
            assert_eq!(drive.flush(), Ok(()));
        })
    }

    #[test]
    fn a_command_that_never_completes_poisons_the_device_and_is_not_retried() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();

            // Nothing answers. The command is still the controller's, and so
            // is the buffer its pointers name.
            assert_eq!(drive.flush(), Err(DeviceError::IoError));
            assert!(drive.poisoned.load(Ordering::Relaxed));
            // Exactly one command went out. Re-issuing a command the
            // controller may still be working on is what the retry loop must
            // not do, and the poison is what stops it.
            assert_eq!(submitted(&io), 1);
        })
    }

    #[test]
    fn a_fatal_controller_status_poisons_the_device() {
        alone_with_the_allocator(|| {
            let base = a_window();
            set_reg(base, NVME_REG_CSTS, NVME_CSTS_CFS);
            // A crawling clock, so the five-second deadline is five million
            // looks away and the fatal status -- read every 256 looks -- is
            // what ends the wait.
            let _ = a_patient_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();

            assert_eq!(drive.flush(), Err(DeviceError::IoError));
            assert!(drive.poisoned.load(Ordering::Relaxed));
            assert_eq!(submitted(&io), 1);
            assert!(
                clock_spent() < 1_000_000,
                "ended on the deadline, not on the fatal status: {} us",
                clock_spent()
            );
        })
    }

    #[test]
    fn a_poisoned_device_refuses_every_request_without_touching_the_controller() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            // Completions are waiting, so nothing here is refused for want of
            // an answer.
            the_controller_answers(&io, &[OK, OK, OK]);
            drive.poisoned.store(true, Ordering::Relaxed);

            let mut buf = alloc::vec![0u8; 512];
            assert_eq!(drive.read_block(0, &mut buf), Err(DeviceError::IoError));
            assert_eq!(drive.write_block(0, &buf), Err(DeviceError::IoError));
            assert_eq!(drive.flush(), Err(DeviceError::IoError));

            // Not one command was submitted and no doorbell was rung: the
            // refusal happens before the controller is involved, because the
            // controller is exactly what cannot be trusted any more.
            assert_eq!(submitted(&io), 0);
            assert_eq!(sq_doorbell(base, 1), 0);
        })
    }

    #[test]
    fn the_completion_queue_wraps_and_the_phase_bit_turns_over_with_it() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            let entries = io.lock().cq.len();

            // One completion per entry, all with the phase a fresh queue
            // expects.
            the_controller_answers(&io, &alloc::vec![OK; entries]);
            for _ in 0..entries {
                assert_eq!(drive.flush(), Ok(()));
            }

            // Round the end the head is back at zero and the phase has turned
            // over, which is the only thing that stops the entries still
            // lying there from reading as a second set of completions.
            {
                let q = io.lock();
                assert_eq!(q.cq_head, 0);
                assert_eq!(q.cq_phase, 0);
            }

            // So the next completion carries a zero in bit 0, and is believed.
            a_completion_at(&io, 0, (entries + 1) as u16, 0);
            assert_eq!(drive.flush(), Ok(()));
            assert_eq!(cq_doorbell(base, 1), 1, "the head went round and on");
        })
    }

    // --- reads and writes ---

    #[test]
    fn a_read_of_one_sector_asks_for_it_and_hands_back_what_arrived() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);
            let arrived: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
            the_controller_leaves(&io, 0, &arrived);

            let mut buf = alloc::vec![0u8; 512];
            assert_eq!(drive.read_block(8, &mut buf), Ok(()));
            assert_eq!(buf, arrived, "what the controller left is what came back");

            let data_pa = io.lock().data_pa as u64;
            let cmd = as_rw(sq_entry(&io, 0));
            assert_eq!(cmd.opcode, 0x02, "a read");
            assert_eq!(cmd.nsid, 1);
            assert_eq!(cmd.slba, 8);
            assert_eq!(cmd.length, 0, "one block, and the field is zero-based");
            assert_eq!(cmd.prp1, data_pa);
            assert_eq!(cmd.prp2, 0, "one page needs no second pointer");
            assert_eq!(sq_doorbell(base, 1), 1);
        })
    }

    #[test]
    fn a_read_of_several_sectors_goes_in_one_command() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);
            let arrived: Vec<u8> = (0..4096u32).map(|i| (i % 253) as u8).collect();
            the_controller_leaves(&io, 0, &arrived);

            let mut buf = alloc::vec![0u8; 4096];
            assert_eq!(drive.read_block(0, &mut buf), Ok(()));
            assert_eq!(buf, arrived);

            // Eight blocks in one command, not eight commands.
            assert_eq!(submitted(&io), 1);
            assert_eq!(as_rw(sq_entry(&io, 0)).length, 7);
        })
    }

    #[test]
    fn a_read_inside_a_bigger_block_reads_the_whole_block_and_copies_out_the_part_asked_for() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let mut drive = a_drive(base);
            // A namespace of 4 KiB blocks, addressed in 512-byte sectors: a
            // one-sector read lands in the middle of a block and the drive
            // cannot transfer less than one.
            drive.lba_shift = 12;
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);
            let arrived: Vec<u8> = (0..4096u32).map(|i| (i % 247) as u8).collect();
            the_controller_leaves(&io, 0, &arrived);

            let mut buf = alloc::vec![0u8; 512];
            assert_eq!(drive.read_block(1, &mut buf), Ok(()));
            // The second sector of the block, not the first.
            assert_eq!(buf, arrived[512..1024].to_vec());

            let cmd = as_rw(sq_entry(&io, 0));
            assert_eq!(cmd.slba, 0, "the block the sector lives in");
            assert_eq!(cmd.length, 0, "and the whole of it");
        })
    }

    #[test]
    fn a_read_of_exactly_two_pages_points_the_second_pointer_at_the_second_page() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let mut drive = a_drive(base);
            drive.lba_shift = 12;
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);
            let (data_pa, prp_list_va) = {
                let q = io.lock();
                (q.data_pa, q.prp_list_va)
            };

            let mut buf = alloc::vec![0u8; 2 * PAGE_SIZE];
            assert_eq!(drive.read_block(0, &mut buf), Ok(()));

            // With one page left over there is nothing to list, and a
            // controller told to read a list would read the data itself as
            // addresses.
            let cmd = as_rw(sq_entry(&io, 0));
            assert_eq!(cmd.prp2, (data_pa + PAGE_SIZE) as u64);
            // SAFETY: the list is a whole page.
            let list = unsafe { core::ptr::read_volatile(prp_list_va as *const u64) };
            assert_eq!(list, 0, "and the list was left alone");
        })
    }

    #[test]
    fn a_read_bigger_than_two_pages_hands_the_controller_a_list_of_them() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let mut drive = a_drive(base);
            drive.lba_shift = 12;
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);
            let (data_pa, prp_list_pa, prp_list_va) = {
                let q = io.lock();
                (q.data_pa, q.prp_list_pa, q.prp_list_va)
            };

            let mut buf = alloc::vec![0u8; 4 * PAGE_SIZE];
            assert_eq!(drive.read_block(0, &mut buf), Ok(()));

            let cmd = as_rw(sq_entry(&io, 0));
            assert_eq!(
                cmd.prp1, data_pa as u64,
                "page zero rides in the first pointer"
            );
            assert_eq!(cmd.prp2, prp_list_pa as u64, "and the rest in a list");
            assert_eq!(cmd.length, 3, "four blocks");

            // SAFETY: the list is a whole page, far more than four entries.
            let list = unsafe { core::slice::from_raw_parts(prp_list_va as *const u64, 4) };
            assert_eq!(
                list[0],
                (data_pa + PAGE_SIZE) as u64,
                "the list starts at the SECOND page, because the first is already in prp1"
            );
            assert_eq!(list[1], (data_pa + 2 * PAGE_SIZE) as u64);
            assert_eq!(list[2], (data_pa + 3 * PAGE_SIZE) as u64);
            assert_eq!(list[3], 0, "and stops there");
        })
    }

    #[test]
    fn a_read_longer_than_the_controller_accepts_is_cut_into_commands() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let mut drive = a_drive(base);
            // A controller that takes one page per command, which is what
            // MDTS says on plenty of real ones.
            drive.max_transfer = PAGE_SIZE;
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK, OK]);
            let arrived: Vec<u8> = (0..PAGE_SIZE as u32).map(|i| (i % 241) as u8).collect();
            the_controller_leaves(&io, 0, &arrived);

            let mut buf = alloc::vec![0u8; 2 * PAGE_SIZE];
            assert_eq!(drive.read_block(0, &mut buf), Ok(()));

            // Two commands of one page, where the bounce buffer alone would
            // have allowed thirty-two.
            assert_eq!(submitted(&io), 2);
            let first = as_rw(sq_entry(&io, 0));
            let second = as_rw(sq_entry(&io, 1));
            assert_eq!((first.slba, first.length), (0, 7));
            assert_eq!(
                (second.slba, second.length),
                (8, 7),
                "carrying on where it stopped"
            );
            // Both halves came out of the bounce, so neither was skipped.
            assert_eq!(buf[..PAGE_SIZE].to_vec(), arrived);
            assert_eq!(buf[PAGE_SIZE..].to_vec(), arrived);
        })
    }

    #[test]
    fn an_error_completion_on_a_read_is_retried_after_a_settle() {
        alone_with_the_allocator(|| {
            let base = a_window();
            // Ten milliseconds a look, so the settle between attempts really
            // has to go round rather than being over before it starts.
            test_clock::set(0);
            test_clock::set_auto_advance(10_000);
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[FAILED, OK]);
            let arrived: Vec<u8> = (0..512u32).map(|i| (i % 229) as u8).collect();
            the_controller_leaves(&io, 0, &arrived);

            let mut buf = alloc::vec![0u8; 512];
            assert_eq!(drive.read_block(0, &mut buf), Ok(()));
            // One transient hiccup should not fail the caller's read, which
            // is why the error completion is retried at all.
            assert_eq!(submitted(&io), 2);
            assert_eq!(buf, arrived);
            assert!(
                clock_spent() >= 50_000,
                "did not settle between the attempts: {} us",
                clock_spent()
            );
        })
    }

    #[test]
    fn a_write_of_whole_blocks_goes_through_the_bounce_buffer() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);
            let payload: Vec<u8> = (0..1024u32).map(|i| (i % 239) as u8).collect();

            assert_eq!(drive.write_block(4, &payload), Ok(()));

            // The controller reads its data out of the bounce, so the bounce
            // is where the caller's bytes have to be before the doorbell.
            assert_eq!(the_bounce(&io, 0, 1024), payload);
            let cmd = as_rw(sq_entry(&io, 0));
            assert_eq!(cmd.opcode, 0x01, "a write");
            assert_eq!(cmd.slba, 4);
            assert_eq!(cmd.length, 1, "two blocks");
            assert_eq!(sq_doorbell(base, 1), 1);
        })
    }

    #[test]
    fn a_write_inside_a_bigger_block_reads_it_first_and_writes_it_back_changed() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let mut drive = a_drive(base);
            drive.lba_shift = 12;
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK, OK]);
            // What is on the drive now, as the read would bring it back.
            let on_the_drive: Vec<u8> = (0..4096u32).map(|i| (i % 233) as u8).collect();
            the_controller_leaves(&io, 0, &on_the_drive);

            assert_eq!(drive.write_block(1, &[0xAB; 512]), Ok(()));

            // Two commands: the block is read, changed in place, written back.
            // Writing only the sector would have handed the controller a whole
            // block with three quarters of it whatever the bounce held.
            assert_eq!(submitted(&io), 2);
            let read = as_rw(sq_entry(&io, 0));
            let write = as_rw(sq_entry(&io, 1));
            assert_eq!((read.opcode, read.slba, read.length), (0x02, 0, 0));
            assert_eq!((write.opcode, write.slba, write.length), (0x01, 0, 0));

            // And the block that went back has the one sector changed and the
            // rest exactly as it came.
            assert_eq!(the_bounce(&io, 0, 512), on_the_drive[..512].to_vec());
            assert_eq!(the_bounce(&io, 512, 512), alloc::vec![0xABu8; 512]);
            assert_eq!(the_bounce(&io, 1024, 3072), on_the_drive[1024..].to_vec());
        })
    }

    #[test]
    fn a_request_the_namespace_cannot_hold_is_refused_before_any_command() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            // Answers are waiting, so nothing below is refused for want of one.
            the_controller_answers(&io, &[OK, OK, OK]);

            let mut two_sectors = alloc::vec![0u8; 1024];
            // One sector of this straddles the end of the namespace.
            assert_eq!(
                drive.read_block(SECTORS - 1, &mut two_sectors),
                Err(DeviceError::InvalidParam)
            );
            assert_eq!(
                drive.write_block(SECTORS, &two_sectors),
                Err(DeviceError::InvalidParam)
            );
            // And half a sector is not a request a block device can serve.
            let mut half = alloc::vec![0u8; 256];
            assert_eq!(
                drive.read_block(0, &mut half),
                Err(DeviceError::InvalidParam)
            );

            assert_eq!(submitted(&io), 0);
            assert_eq!(sq_doorbell(base, 1), 0);
        })
    }

    #[test]
    fn the_last_sector_of_the_namespace_can_be_read_and_the_one_after_it_cannot() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);

            assert_eq!(drive.block_count(), SECTORS);
            let mut buf = alloc::vec![0u8; 512];
            // The bound is the sector after the last, not the last.
            assert_eq!(drive.read_block(SECTORS - 1, &mut buf), Ok(()));
            assert_eq!(
                drive.read_block(SECTORS, &mut buf),
                Err(DeviceError::InvalidParam)
            );
            assert_eq!(submitted(&io), 1);
        })
    }

    #[test]
    fn the_driver_answers_to_the_name_the_registry_matches_on() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let drive = a_drive(base);

            assert_eq!(Scheme::name(&drive), "nvme");
            assert_eq!(drive.get_name_irq(), (String::from("nvme"), 11));

            // Completions are polled and the controller's interrupts are
            // masked, so an interrupt must not move the queue on.
            drive.handle_irq(11);
            assert_eq!(sq_doorbell(base, 1), 0);
            assert_eq!(cq_doorbell(base, 1), 0);
            assert_eq!(submitted(&drive.io_queues[0]), 0);
        })
    }

    // --- shutting down ---

    #[test]
    fn a_reboot_asks_for_an_orderly_shutdown_and_keeps_the_rest_of_the_configuration() {
        alone_with_the_allocator(|| {
            let base = a_window();
            // A controller running, with an abrupt shutdown left in the
            // shutdown field by whatever ran before.
            set_reg(
                base,
                NVME_REG_CC,
                NVME_CC_ENABLE | NVME_CC_IOSQES | NVME_CC_SHN_ABRUPT,
            );
            set_reg(base, NVME_REG_CSTS, NVME_CSTS_SHST_CMPLT);
            // The handshake is answered already, so the clock is beside the
            // point here -- but it has to move, or a wait that stops looking
            // at CSTS has nothing to end it.
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            the_controller_answers(&io, &[OK]);

            drive.quiesce_for_reboot();

            // The write cache is flushed before the shutdown is asked for. On
            // a controller without DRAM that flush is the moment the mapping
            // tables reach the NAND instead of being rebuilt on next
            // power-up, so skipping it is not a missing optimisation.
            assert_eq!(submitted(&io), 1);
            assert_eq!(sq_entry(&io, 0).nsid, 1, "a flush of the namespace");
            assert_eq!(sq_doorbell(base, 1), 1);

            // The shutdown field was replaced, not or-ed into: asking for a
            // normal shutdown on top of an abrupt one spells neither.
            let cc = reg(base, NVME_REG_CC);
            assert_eq!((cc >> 14) & 3, 1, "a normal shutdown, not an abrupt one");
            assert_eq!(cc & 1, 1, "still enabled, which the shutdown needs");
            assert_eq!((cc >> 16) & 0xf, 6, "and the entry size untouched");
        })
    }

    #[test]
    fn a_controller_that_never_reports_its_shutdown_does_not_hold_the_reboot() {
        alone_with_the_allocator(|| {
            let base = a_window();
            set_reg(base, NVME_REG_CC, NVME_CC_ENABLE);
            // CSTS.SHST stays at "no shutdown occurring", which is what a
            // wedged controller reports for ever.
            test_clock::set(0);
            test_clock::set_auto_advance(2_000_000);
            let drive = a_drive(base);
            drive.poisoned.store(true, Ordering::Relaxed);

            // Returning at all is the behaviour: a reboot cannot wait on a
            // drive that has stopped answering.
            drive.quiesce_for_reboot();

            assert_eq!((reg(base, NVME_REG_CC) >> 14) & 3, 1, "it did ask");
            assert!(
                clock_spent() > 1_000_000,
                "left before the second it gives the drive: {} us",
                clock_spent()
            );
            assert!(
                clock_spent() < 10_000_000,
                "waited far past that second: {} us",
                clock_spent()
            );
        })
    }

    // --- the verdict a completion carries ---

    /// The sixteen bits a controller writes as its verdict, laid out as NVM
    /// Express 1.4 figure 126 has them: the phase at bit 0, then the status
    /// code, then its type, then three modifiers above both.
    ///
    /// The driver reads the code and the type and fails the command if either
    /// is not zero, so the two tests below are about where each of those two
    /// fields ends.
    fn a_verdict(sc: u16, sct: u16, above: u16) -> u16 {
        1 | (sc << 1) | (sct << 9) | (above << 12)
    }

    #[test]
    fn a_completion_whose_type_is_set_but_whose_code_is_zero_is_a_failure() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            // Status code type 4 with a status code of zero. The type is the
            // top three of the twelve bits the two fields share, so this is
            // the verdict that says where the type starts: read one bit
            // lower and the only bit set falls outside it, leaving code and
            // type both zero, which is exactly what a success looks like.
            the_controller_answers(&io, &[a_verdict(0, 4, 0), OK]);

            // The command is over either way, so the retry is what tells the
            // two readings apart. Taken for a success, the command would be
            // reported done without the controller having carried it out.
            assert_eq!(drive.flush(), Ok(()));
            assert_eq!(
                submitted(&io),
                2,
                "the failure was read as a success and never retried"
            );
        })
    }

    #[test]
    fn the_bits_above_the_status_type_are_not_part_of_it() {
        alone_with_the_allocator(|| {
            let base = a_window();
            a_quick_clock();
            let drive = a_drive(base);
            let io = drive.io_queues[0].clone();
            // Above the type sit the command retry delay and two flags. They
            // qualify a failure rather than being one, so a verdict whose
            // code and type are both zero is a success however they are set.
            // Read the type one bit too wide and this completion turns into
            // an error, and a command that did succeed goes out again.
            the_controller_answers(&io, &[a_verdict(0, 0, 0b11)]);

            assert_eq!(drive.flush(), Ok(()));
            assert_eq!(
                submitted(&io),
                1,
                "a success was read as a failure and submitted twice"
            );
        })
    }

    // --- the format a namespace is in ---

    /// An Identify Namespace payload whose format in use is not the first
    /// one: every LBA format descriptor says `others`, and the one `index`
    /// points at says `lbads`.
    fn an_identified_namespace_in_format(
        queue: &Queue,
        nsze: u64,
        index: usize,
        lbads: u8,
        others: u8,
        mdts: u8,
    ) {
        an_identified_namespace(queue, nsze, others, mdts);
        let q = queue.lock();
        // SAFETY: as `an_identified_namespace`.
        let data = unsafe { core::slice::from_raw_parts_mut(q.data_va as *mut u8, 4096) };
        // FLBAS, byte 26, whose low four bits are the index of the format in
        // use. It is the third character of the model string as well, and the
        // model is only ever logged.
        data[26] = index as u8;
        // Sixteen descriptors of four bytes each from byte 128, so a wrong
        // base or a wrong stride lands on a different descriptor instead of
        // outside the payload.
        for slot in 0..16 {
            let shift = if slot == index { lbads } else { others };
            let at = 128 + slot * 4;
            data[at..at + 4].copy_from_slice(&((shift as u32) << 16).to_le_bytes());
        }
    }

    #[test]
    fn the_namespace_is_read_in_the_lba_format_its_flbas_points_at() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let deadline = a_patient_clock();

            let mut drive = a_drive_not_yet_brought_up(base);
            the_controller_answers(&drive.admin_queue, &[OK, OK]);
            // 4 KiB blocks in the third descriptor, 512 bytes in every other
            // one. A namespace in a format past the first is ordinary -- it
            // is what formatting a disk to another block size leaves behind
            // -- and every test here so far has used the first.
            an_identified_namespace_in_format(&drive.admin_queue, 1024, 2, 12, 9, 5);
            let answered = a_controller_that_answers(base);

            assert_eq!(drive.nvme_configure_admin_queue(deadline), Ok(()));
            answered.join().expect("the controller thread lived");

            // Land on another descriptor and the driver finds 512 bytes
            // here, and from then on every address it computes is eight
            // times too small: a read of the last sector would come back
            // with the first eighth of the disk, and no error anywhere.
            assert_eq!(drive.logical_block_size(), 4096);
            assert_eq!(drive.block_count(), 1024 * 8);
        })
    }
}
