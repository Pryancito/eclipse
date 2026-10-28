//! Which RM client a `GEM_NEW` object belongs to, and so which process
//! `nvidia-smi` bills it to, plus the `/proc/gpudbg` view of that billing.

use super::NvidiaGpu;
use alloc::string::String;
use alloc::vec::Vec;

impl NvidiaGpu {
    /// The context a new GEM object of `owner_pid` is allocated under: its
    /// ready context, or, for a uvmm client, the one built now. See the
    /// `GEM_NEW` arm. 0 is the shared kernel client, whose memory no process
    /// is billed for; the first such VRAM object of a pid says so.
    pub(super) fn gem_owner_ctx(&self, owner_pid: u64) -> u32 {
        let ready = self.ctx_idx_for_pid(owner_pid);
        if ready != 0 || !super::super::nouveau_uapi::gem_owner_early() {
            return ready;
        }
        if !self.nouveau_has_uvmm(owner_pid) {
            return 0;
        }
        let is_compositor = self.ctx0_is_owner(owner_pid);
        let (ctx_idx, _, _) = self.ensure_ctx_for_pid(owner_pid, is_compositor);
        if ctx_idx != 0 {
            crate::klog_info!(
                "[nouveau-uapi] GEM_NEW: pid={} had no context yet -> built CTX {} so its memory is billed to it",
                owner_pid,
                ctx_idx
            );
        } else {
            crate::klog_warn!(
                "[nouveau-uapi] GEM_NEW: pid={} has no context of its own -> object on the shared client, \
                 nvidia-smi bills it to no process",
                owner_pid
            );
        }
        ctx_idx
    }

    /// `/proc/gpudbg`: the live GEM objects of this GPU by process, VRAM and
    /// host memory apart. Only VRAM shows in `nvidia-smi` (as on Linux), and
    /// only an object with its own context (`ctx` not 0) shows under its
    /// process; `ctx0` is VRAM the total counts and no process does.
    pub(super) fn format_gem_by_pid(&self) -> String {
        use super::super::nouveau_uapi::NOUVEAU_GEM_DOMAIN_VRAM;
        use core::fmt::Write;
        // (pid, vram, sysmem, vram in ctx 0, objects)
        let mut rows: Vec<(u64, u64, u64, u64, u32)> = Vec::new();
        for o in self.nouveau_gem.lock().iter() {
            let i = match rows.iter().position(|r| r.0 == o.owner_pid) {
                Some(i) => i,
                None => {
                    rows.push((o.owner_pid, 0, 0, 0, 0));
                    rows.len() - 1
                }
            };
            let r = &mut rows[i];
            if o.domain & NOUVEAU_GEM_DOMAIN_VRAM != 0 {
                r.1 += o.size;
                if o.owner_ctx == 0 {
                    r.3 += o.size;
                }
            } else {
                r.2 += o.size;
            }
            r.4 += 1;
        }
        let kib = |b: u64| b / 1024;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "[gpudbg] --- live GEM by process (nvidia-smi bills VRAM only) ---"
        );
        if rows.is_empty() {
            let _ = writeln!(s, "[gpudbg]  (no live GEM objects on this GPU)");
        }
        for (pid, vram, sys, ctx0, n) in rows {
            let _ = writeln!(
                s,
                "[gpudbg]  pid={} ctx={} objects={} vram={} KiB (ctx0, unbilled: {} KiB) sysmem={} KiB",
                pid,
                self.ctx_idx_for_pid(pid),
                n,
                kib(vram),
                kib(ctx0),
                kib(sys)
            );
        }
        s
    }
}
