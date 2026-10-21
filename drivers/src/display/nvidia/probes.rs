//! The GR hang probe and the channel self-test: diagnostics on the ring, out of `nvidia.rs` for its size ceiling.

use super::*;

impl NvidiaGpu {
    /// One-shot submission self-test, run on the first RM-backed
    /// CHANNEL_ALLOC of the boot while the GPFIFO ring is still empty.
    ///
    /// Why it exists: the hardware kept dying with RC error 31
    /// (`ROBUST_CHANNEL_FIFO_ERROR_MMU_ERR_FLT` -- the GPU touched an
    /// unmapped VA) on NVK's very first submission, with every uAPI call
    /// reporting success, and the RM's Xid narration (which would name the
    /// faulting address) never surfaces in this port. This test removes Mesa
    /// from the equation: the KERNEL authors a minimal, known-good push --
    /// one host-class semaphore RELEASE, the exact 6-dword stream the C
    /// fence builder uses -- places it in a GART GEM object bound through
    /// OUR OWN `VM_BIND` path at a VA in the kernel-reserved half of NVK's
    /// map (so it can never collide), and submits it through the same
    /// `exec_submit_signaled` EXEC uses.
    ///
    /// Three verdicts, each pinning the bug to a different layer:
    /// * **stage A passes** (fence lands AND the semaphore payload appears
    ///   in the GEM page): the whole path -- GART alloc, VM_BIND PTEs, PBDMA
    ///   fetch from a bound VA, method execution, fence -- works for a
    ///   kernel push. The MMU fault must then come from the CONTENT of
    ///   Mesa's pushes (a VA its methods reference), not from the plumbing.
    /// * **stage A fails but stage B passes** (same submit, but the push
    ///   fetched from the channel's own RM-mapped buffer): the channel
    ///   executes fine from RM-created mappings and the bug is precisely
    ///   that OUR VM_BIND PTEs are not GPU-visible.
    /// * **both fail**: the channel executes nothing at all -- the problem
    ///   is channel-level (step17: scheduling/runlist/USERD), and everything
    ///   VM_BIND/Mesa is a red herring.
    pub(super) fn nouveau_channel_selftest(&self, device_instance: u32, buf_gpu_va: u64) {
        use crate::display::nouveau_uapi as nv;
        use nvidia_rm_sys::rm_init as rm;
        // Kernel-reserved half of NVK's VA layout ([1<<39, 1<<40)): NVK's own
        // heaps never allocate here, so a fixed address is collision-free.
        const TEST_VA: u64 = 1u64 << 39;
        const TEST_SIZE: u64 = 0x2000; // page 0: pushbuffer; page 1: semaphore
        const SEM_OFF: u64 = 0x1000;
        const PAYLOAD: u32 = 0x5e1f_7e57;
        // ECLIPSE_PUSH_HDR(subch=0, NVC46F_SEM_ADDR_LO=0x5c, count=5):
        // SEC_OP=INC_METHOD (1<<29) | count<<16 | subch<<13 | mthd>>2.
        const HDR_SEM: u32 = (1 << 29) | (5 << 16) | (0x5c >> 2);

        let alloc = match rm::gem_alloc(device_instance, TEST_SIZE, true) {
            Ok(a) if a.alloc_status == 0 => a,
            other => {
                crate::klog_warn!(
                    "[nouveau-uapi] SELFTEST: GART alloc failed ({:?}) -- cannot run",
                    other.map(|a| a.alloc_status)
                );
                return;
            }
        };
        let pa = match rm::gem_map_cpu(device_instance, alloc.h_memory) {
            Ok(m) if m.lookup_status == 0 => m.phys_addr,
            _ => {
                crate::klog_warn!("[nouveau-uapi] SELFTEST: gem_map_cpu failed -- cannot run");
                rm::gem_free(device_instance, alloc.h_memory);
                return;
            }
        };
        let bind = match rm::vm_bind_map(
            device_instance,
            0,
            alloc.h_memory,
            TEST_SIZE,
            TEST_VA,
            0,
            0,
            false,
        ) {
            Ok(b) if b.map_status == 0 => b,
            _ => {
                crate::klog_warn!(
                    "[nouveau-uapi] SELFTEST: VM_BIND of the test page failed -- cannot run"
                );
                rm::gem_free(device_instance, alloc.h_memory);
                return;
            }
        };
        let base = crate::bus::phys_to_virt(pa as usize);
        let sem_va = TEST_VA + SEM_OFF;
        unsafe {
            core::ptr::write_volatile((base + SEM_OFF as usize) as *mut u32, 0);
            let p = base as *mut u32;
            core::ptr::write_volatile(p, HDR_SEM);
            core::ptr::write_volatile(p.add(1), sem_va as u32);
            core::ptr::write_volatile(p.add(2), ((sem_va >> 32) & 0xff) as u32);
            core::ptr::write_volatile(p.add(3), PAYLOAD);
            core::ptr::write_volatile(p.add(4), 0);
            // SEM_EXECUTE: OPERATION_RELEASE, 32-bit payload, no WFI, no
            // timestamp -- all other fields zero.
            core::ptr::write_volatile(p.add(5), 1);
        }
        let stage_a = rm::exec_submit_signaled(
            device_instance,
            0,
            TEST_VA,
            24,
            nv::next_fence_payload(),
            500,
        );
        let landed = unsafe { core::ptr::read_volatile((base + SEM_OFF as usize) as *const u32) };
        let a_ok = matches!(
            &stage_a,
            Ok(r) if r.submit_status == 0 && r.fence_submit_status == 0 && r.fence_wait_status == 0
        );
        if a_ok && landed == PAYLOAD {
            crate::klog_info!(
                "[nouveau-uapi] SELFTEST stage A PASS: kernel push executed from VM_BIND VA {:#x}, fence landed, semaphore={:#x} -- the submission plumbing works; an MMU fault after this points at the CONTENT of Mesa's pushes",
                TEST_VA,
                landed
            );
        } else {
            let (ss, fs, fw) = match &stage_a {
                Ok(r) => (r.submit_status, r.fence_submit_status, r.fence_wait_status),
                Err(e) => (*e, 0xffff_ffff, 0xffff_ffff),
            };
            crate::klog_warn!(
                "[nouveau-uapi] SELFTEST stage A FAIL: submit={:#x} fenceSubmit={:#x} fenceWait={:#x} semaphore={:#x} (expected {:#x}) -- kernel push from a VM_BIND VA did not execute",
                ss,
                fs,
                fw,
                landed,
                PAYLOAD
            );
            // Stage B: identical submit, but the caller push is the fence
            // method stream stage A just wrote into the CHANNEL's own
            // RM-mapped buffer (re-executing a stale semaphore release is
            // harmless). Distinguishes "our PTEs are invisible" from "the
            // channel executes nothing".
            let fence_pb_va = buf_gpu_va + 0x9000;
            let stage_b = rm::exec_submit_signaled(
                device_instance,
                0,
                fence_pb_va,
                24,
                nv::next_fence_payload(),
                500,
            );
            match stage_b {
                Ok(r)
                    if r.submit_status == 0
                        && r.fence_submit_status == 0
                        && r.fence_wait_status == 0 =>
                {
                    crate::klog_warn!(
                        "[nouveau-uapi] SELFTEST stage B PASS: the same push executed from the channel's RM-mapped buffer ({:#x}) -- the channel is fine and OUR VM_BIND PTEs are NOT GPU-visible (the bug is in vm_bind_map's Map)",
                        fence_pb_va
                    );
                }
                Ok(r) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] SELFTEST stage B FAIL too: submit={:#x} fenceSubmit={:#x} fenceWait={:#x} -- the channel executes NOTHING (kernel pushes from RM-mapped VAs included); the problem is channel-level (step17), not VM_BIND/Mesa",
                        r.submit_status,
                        r.fence_submit_status,
                        r.fence_wait_status
                    );
                }
                Err(e) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] SELFTEST stage B FAIL too (NV_STATUS={:#x}) -- channel-level problem",
                        e
                    );
                }
            }
        }
        let _ = rm::vm_bind_unmap(device_instance, bind.h_virt, TEST_SIZE, TEST_VA);
        rm::gem_free(device_instance, alloc.h_memory);
    }

    /// Whether THIS process owns the RM-backed channel.
    ///
    /// Submission is different from VA work: `EXEC` pushes into the GPFIFO
    /// ring that `step17` built, and this milestone has exactly one. Two
    /// clients pushing into the same ring corrupt each other, so submission
    /// stays restricted to the channel's owner even though the VAS is shared.
    /// GR-engine hang probe: at the moment a fence wait expired, snapshot the
    /// BAR0 registers that distinguish the four mutually-exclusive failure
    /// modes:
    ///   (a) MMU fault  — latched in 0xb83090 (valid=1)
    ///   (b) GR method stall — 0x400700 non-zero + 0x400704
    ///   (c) PBDMA never fetched the push — GP_GET != GP_PUT
    ///   (d) FECS/GPCCS ctx-switch hang — 0x409c00 / 0x41a000
    ///
    /// No RM calls: two prior attempts to involve the RM on this path crashed
    /// the machine. Pure BAR0 reads only. Stored in LAST_GR_HANG_PROBE for
    /// /proc/gpudbg; first timeout per boot wins (the ring stays wedged
    /// after). Shared by the RM per-submit path (inline poll timeout) and the
    /// direct-submit path (`syncobj` fence timeout / ring-full timeout).
    pub(super) fn gr_hang_probe(
        &self,
        ctx_idx: u32,
        owner_pid: u64,
        work_token: u32,
        runlist_id: u32,
        timeout_ms: u32,
    ) {
        use crate::display::nouveau_uapi as nv;
        let bar0 = self._bar0;
        let rd = |off: usize| unsafe { core::ptr::read_volatile((bar0 + off) as *const u32) };
        let wr =
            |off: usize, v: u32| unsafe { core::ptr::write_volatile((bar0 + off) as *mut u32, v) };

        // work_token encodes runlistId|chId (Turing: bits [19:16]
        // = runlistId, bits [11:0] = chId). Capture from exec
        // result so the probe names the EXACT channel that timed
        // out, not a generic PBDMA0 snapshot that may belong to
        // a different concurrent context.
        // Channel ID: low 12 bits of the work-submit token
        // (Turing/Ampere kernel_fifo_tu102.c: chId = token & 0xfff).
        let ch_id = work_token & 0x0fff;

        // NV_PGRAPH_STATUS: overall GR busy/idle.
        // 0 = idle; any bit set = a sub-engine is active/stalled.
        let gr_status = rd(0x0040_0700);
        // NV_PGRAPH_TRAPPED_ADDR: the method that stalled GR.
        //   bits [20:16] = subchannel, bits [12:0] = method
        let gr_trap_addr = rd(0x0040_0704);
        let gr_trap_lo = rd(0x0040_0708);
        let gr_trap_hi = rd(0x0040_070c);

        // The PBDMA that serves THIS channel's runlist, not PBDMA0.
        // `NV_PFIFO_PBDMA_MAP(i)` (0x2390 + i*4) is a bitmask of the
        // runlists PBDMA i serves, and the /proc/gpudbg dump below already
        // records that the CE's runlist came out on PBDMA9 on real
        // hardware. Reading 0x40000 for a channel on another runlist
        // reports a DIFFERENT engine's ring pointers, and `drained` is
        // what the hint -- and the doorbell re-ring -- then turn on.
        let maps: Vec<u16> = (0..nv::PBDMA_COUNT)
            .map(|i| (rd(0x0000_2390 + (i as usize) * 4) & 0xffff) as u16)
            .collect();
        let pbdma_idx = nv::pbdma_for_runlist(&maps, runlist_id);
        let pb = nv::PBDMA_BASE + (pbdma_idx.unwrap_or(0) as usize) * nv::PBDMA_STRIDE;
        // Ring pointers. GP_GET == GP_PUT means the PBDMA finished
        // fetching; GP_GET < GP_PUT means it never fetched the push.
        let pb_put = rd(pb);
        let pb_get = rd(pb + 0x14);
        // PBDMA execution state — decodes WHY GP_GET is stuck,
        // the one thing GP_PUT/GP_GET alone cannot tell apart:
        //   STATUS(+0x100): the PBDMA channel/exec state machine.
        //   GET(+0x18): pushbuffer-level get. If it advanced
        //     into the pending segment the host DID fetch the
        //     entry and is executing/blocked inside the push
        //     (a semaphore-acquire NVK baked in -> explicit-sync).
        //   INTR_0(+0x108): pending PBDMA interrupts. A blocked
        //     semaphore-acquire that exceeds its timeout raises
        //     one here; a channel that was simply never scheduled
        //     onto the runlist raises none. So INTR_0!=0 => the
        //     push was fetched and blocked (semaphore/PB error);
        //     INTR_0==0 with GP_GET frozen => never scheduled
        //     (runlist/doorbell), NOT a semaphore block.
        let pb_status = rd(pb + 0x100);
        let pb_pbget = rd(pb + 0x18);
        let pb_intr = rd(pb + 0x108);

        // NV_PFIFO_CHRAM_CHANNEL(ch_id): per-channel PCCSR register.
        // Stride is 8 bytes.  The FIRST word (+0x0) is the instance
        // pointer; the SECOND word (+0x4) carries channel state:
        //   bit 0       = ENABLE
        //   bits [27:24] = STATUS (0=IDLE, 1=PENDING, 2=CTX_RELOAD,
        //                          3=BUSY/ACTIVE, 4=PENDING_CTX_RELOAD,
        //                          5=PENDING_ACQ, 6=ENG_SEL_PENDING)
        //   bit 28      = BUSY
        // Reading at timeout tells us whether the channel was visible
        // to the host FIFO scheduler. Matches the existing decode at
        // drivers/src/display/nvidia.rs:3164 (PCCSR0(0x800004)).
        let pccsr_off = 0x0080_0004usize + (ch_id as usize) * 8;
        let pccsr_val = rd(pccsr_off);
        let pccsr_enable = pccsr_val & 1;
        let pccsr_busy = (pccsr_val >> 28) & 1;
        let pccsr_status = (pccsr_val >> 24) & 0xf;

        // HUB MMU fault latch (non-replayable, TU102 0xb83080..90).
        // valid = bit 31 of INFO1; reason = bits [4:0].
        let f_info1 = rd(0x00b8_3090);
        let f_addr_lo = rd(0x00b8_3080);
        let f_addr_hi = rd(0x00b8_3084);
        let f_info0 = rd(0x00b8_3088);

        // NV_PGRAPH_PRI_FECS_CTXSW_STATUS_FE_0: FECS state machine.
        // Non-zero when FECS is mid ctx-switch (save/restore).
        // 0x1 = SAVE_CONTEXT, 0x2 = RESTORE_CONTEXT (loading grctx).
        let fecs_status = rd(0x0040_9c00);
        // NV_PGRAPH_PRI_FECS_HOST_INT_STATUS: FECS interrupt flags.
        let fecs_intr = rd(0x0040_9c14);
        // NV_PGRAPH_PRI_FECS_CTXSW_MAILBOX(0/1): last FECS ucode
        // message and error detail. Ucode updates MAILBOX0 at each
        // step of the context-save/restore microcode; a non-zero
        // MAILBOX0 with fecs_status==0x2 (RESTORE) shows exactly
        // which grctx step the ucode reached before hanging. These
        // are the registers an rc_dump uses to diagnose FECS hangs.
        let fecs_mb0 = rd(0x0040_9800);
        let fecs_mb1 = rd(0x0040_9804);
        // NV_PGRAPH_PRI_FECS_CTXSW_PRIV_ERROR_FE_0: privilege
        // error flags set if FECS ucode tried to read an invalid
        // register (context-image addresses wrong / buffer too small).
        let fecs_priv_err = rd(0x0040_9c10);

        // NV_PGRAPH_PRI_GPC0_GPCCS_CTXSW_STATUS_GPC_0: GPCCS state.
        let gpccs_status = rd(0x0041_a000);

        // With no PBDMA for this runlist there are no ring pointers to
        // read, so `drained` would be `0 == 0` -- "the PBDMA drained the
        // push", the one conclusion the registers cannot support. Treat
        // the unknown as unknown and let the hint say so.
        let drained = pbdma_idx.is_some() && pb_get == pb_put;
        let mmu_valid = (f_info1 >> 31) & 1;
        let mmu_reason = f_info1 & 0x1f;
        let gr_subchan = (gr_trap_addr >> 16) & 0x1f;
        let gr_method = gr_trap_addr & 0x1fff;

        // Diagnosis: a short code that names the failure mode, and the
        // sentence that explains it. They are separate because
        // `klog_emit` formats into a 256-byte buffer and drops the rest:
        // four of the seven sentences are long enough that, glued into one
        // line with the registers and the pointer to /proc/gpudbg, the cut
        // landed BEFORE both -- so the line added to end this probe's
        // silence arrived without the two things worth reading.
        // `hang_verdict` has the table and the test that measures the line.
        let verdict = nv::hang_verdict(nv::HangSignals {
            mmu_fault_latched: mmu_valid != 0,
            gr_method_trapped: gr_status != 0 && gr_method != 0,
            pbdma_known: pbdma_idx.is_some(),
            ring_drained: drained,
            pbdma_intr_pending: pb_intr != 0,
            ctxsw_busy: fecs_status != 0 || gpccs_status != 0,
        });
        let hint = verdict.detail;

        // Until #1729 only two of the seven outcomes reached dmesg (the
        // doorbell re-ring and the FECS hang): an MMU fault, a GR stall or
        // a fence that never landed left the console silent, with the
        // whole record only in /proc/gpudbg -- which nobody reads unless
        // something already pointed them at it. Two lines per timeout, and
        // the ring stays wedged after the first: the facts first, so they
        // survive even if the sentence after them is cut.
        crate::klog_warn!(
            "[nouveau-uapi] EXEC: ctx={} fence TIMEOUT after {}ms -- {} (pbdma={} runlist={} ch={} GP_PUT={:#x} GP_GET={:#x} INTR_0={:#x}) -- full record in /proc/gpudbg",
            ctx_idx,
            timeout_ms,
            verdict.code,
            match pbdma_idx {
                Some(i) => alloc::format!("{}", i),
                None => alloc::string::String::from("none"),
            },
            runlist_id,
            ch_id,
            pb_put,
            pb_get,
            pb_intr
        );
        crate::klog_warn!("[nouveau-uapi] EXEC: {}: {}", verdict.code, verdict.detail);

        // Recovery attempt for the "never fetched, no PBDMA
        // interrupt" case: re-ring the doorbell with the exact
        // work-submit token the RM issued for THIS submit.
        // Rationale: the PBDMA may have missed the original
        // doorbell (GSP-to-host signalling is not guaranteed
        // lossless at high submission rates on Turing). Writing
        // the token to 0xbb0090 a second time is idempotent if
        // the channel is already running, and may unblock it if
        // the doorbell was lost. Pure BAR0 write; no RM call.
        // Only attempted when GP_GET is frozen AND INTR_0 == 0
        // (i.e., the push never made it to the PBDMA at all).
        if pbdma_idx.is_some() && !drained && pb_intr == 0 && work_token != 0 {
            wr(0x00bb_0090, work_token);
            crate::klog_warn!(
                "[nouveau-uapi] EXEC: ctx={} PBDMA-stall recovery: re-rang doorbell token={:#010x} runlist={} ch={}",
                ctx_idx, work_token, runlist_id, ch_id
            );
        }

        // FECS ctx-switch hang: log a targeted klog line so
        // dmesg immediately points at grctx/golden-context as
        // the root. CTXSW_STATUS==0x2 = RESTORE_CONTEXT: FECS
        // ucode is stuck trying to load the client channel's GR
        // context image. Most likely causes: ctx_prime timed out
        // (golden context was never loaded before NVK's first
        // push hit the cold load), or a global-ctx-buffer
        // (patch/attribute/pagepool) was not mapped for this ctx.
        // Two lines, like the verdict above: with the five registers in
        // front, the sentence ran past `klog_emit`'s 256 bytes and the cut
        // landed before the pointer to the PRIME line, which is the one
        // thing this line exists to give.
        if fecs_status != 0 || gpccs_status != 0 {
            crate::klog_warn!(
                "[nouveau-uapi] EXEC: ctx={} FECS/GPCCS ctx-switch hang \
                 (FECS_STATUS={:#010x} GPCCS_STATUS={:#010x} \
                 MAILBOX0={:#010x} MAILBOX1={:#010x} PRIV_ERR={:#010x})",
                ctx_idx,
                fecs_status,
                gpccs_status,
                fecs_mb0,
                fecs_mb1,
                fecs_priv_err
            );
            crate::klog_warn!(
                "[nouveau-uapi] EXEC: ctx={} GR context-image for this client channel is \
                 incomplete; check whether ctx_prime timed out at CHANNEL_ALLOC \
                 (see /proc/gpudbg last PRIME line for ctx={})",
                ctx_idx,
                ctx_idx
            );
        }

        // Whether THIS ctx was ever primed: the decisive
        // datum for a FECS RESTORE hang. "never recorded"
        // here means the EXEC ran on a ctx whose build/prime
        // path did not complete -- the exact anomaly a real
        // RTX repro exhibited (ctx=3 hung with no prime
        // line at all).
        let prime_state = nv::prime_line_for(ctx_idx).unwrap_or_else(|| {
            alloc::string::String::from(
                "NEVER RECORDED -- this ctx reached EXEC without its build/prime completing",
            )
        });
        nv::record_gr_hang_probe(alloc::format!(
            "ctx={ctx} pid={pid} fence TIMEOUT after {ms}ms\n\
             HINT: {hint}\n\
             prime-state: {prime_state}\n\
             submit: work_token={work_token:#010x} runlist_id={runlist_id} ch_id={ch_id}\n\
             GR_STATUS(0x400700)={gr_status:#010x} (0=idle)\n\
             TRAPPED_ADDR(0x400704)={gr_trap_addr:#010x} subchan={gr_subchan} method={gr_method:#06x}\n\
             TRAPPED_DATA lo={gr_trap_lo:#010x} hi={gr_trap_hi:#010x}\n\
             PBDMA{pbdma_name}(serves runl{runlist_id}, base={pb:#x}) GP_PUT={pb_put:#010x} GP_GET={pb_get:#010x} drained={drained}\n\
             PBDMA{pbdma_name} STATUS={pb_status:#010x} GET={pb_pbget:#010x} INTR_0={pb_intr:#010x}\n\
             PCCSR ch{ch_id}(0x{pccsr_off:x})={pccsr_val:#010x} enable={pccsr_enable} busy={pccsr_busy} status={pccsr_status} (0=IDLE,1=PENDING,3=ACTIVE)\n\
             MMU FAULT_INFO1(0xb83090)={f_info1:#010x} valid={mmu_valid} reason={mmu_reason:#04x}\n\
               FAULT_ADDR={f_addr_hi:#010x}_{f_addr_lo:#010x} engine_id={engine_id:#04x}\n\
             FECS CTXSW_STATUS(0x409c00)={fecs_status:#010x} (0=IDLE,1=SAVE,2=RESTORE) HOST_INT(0x409c14)={fecs_intr:#010x}\n\
             FECS MAILBOX0(0x409800)={fecs_mb0:#010x} MAILBOX1(0x409804)={fecs_mb1:#010x} PRIV_ERR(0x409c10)={fecs_priv_err:#010x}\n\
             GPCCS GPC0_STATUS(0x41a000)={gpccs_status:#010x}",
            ctx = ctx_idx,
            pid = owner_pid,
            ms = timeout_ms,
            hint = hint,
            work_token = work_token,
            runlist_id = runlist_id,
            ch_id = ch_id,
            gr_status = gr_status,
            gr_trap_addr = gr_trap_addr,
            gr_subchan = gr_subchan,
            gr_method = gr_method,
            gr_trap_lo = gr_trap_lo,
            gr_trap_hi = gr_trap_hi,
            pbdma_name = match pbdma_idx {
                Some(i) => alloc::format!("{}", i),
                None => alloc::string::String::from("?(none routes it)"),
            },
            pb = pb,
            pb_put = pb_put,
            pb_get = pb_get,
            pb_status = pb_status,
            pb_pbget = pb_pbget,
            pb_intr = pb_intr,
            drained = drained,
            pccsr_off = pccsr_off,
            pccsr_val = pccsr_val,
            pccsr_enable = pccsr_enable,
            pccsr_busy = pccsr_busy,
            pccsr_status = pccsr_status,
            f_info1 = f_info1,
            mmu_valid = mmu_valid,
            mmu_reason = mmu_reason,
            f_addr_hi = f_addr_hi,
            f_addr_lo = f_addr_lo,
            engine_id = f_info0 & 0xff,
            fecs_status = fecs_status,
            fecs_intr = fecs_intr,
            fecs_mb0 = fecs_mb0,
            fecs_mb1 = fecs_mb1,
            fecs_priv_err = fecs_priv_err,
            gpccs_status = gpccs_status,
        ));
    }
}
