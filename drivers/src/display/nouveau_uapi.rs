//! Nouveau-compatible DRM driver-specific ioctl surface, for `NvidiaGpu`.
//!
//! # Why this exists
//!
//! Mesa's OpenGL/Vulkan acceleration for NVIDIA hardware (`nouveau_dri.so`,
//! NVK) is not something Eclipse can vendor: it is ordinary open-source
//! Linux userspace, already built for x86_64 by Alpine, and it already runs
//! unmodified under Eclipse's syscall layer -- same as Xorg/labwc/busybox.
//! What it needs from the KERNEL side is `nouveau.ko`'s ioctl protocol
//! (`include/uapi/drm/nouveau_drm.h`), which is a stable, public, documented
//! contract -- unlike NVIDIA's own closed userspace driver, which speaks an
//! undocumented private protocol to `nvidia.ko`. This module reimplements
//! that public contract against `NvidiaGpu`, the same way
//! `linux-object/src/fs/devfs/drm.rs` reimplements the generic DRM/KMS UAPI
//! (see `docs/README-drm.md`).
//!
//! # Scope of THIS milestone -- read before extending
//!
//! This was written in a sandbox with no NVIDIA GPU, no `/dev/kvm`, and no
//! QEMU at all -- nothing here has run against real hardware. To keep that
//! honest, every operation either:
//!   (a) reuses an already hardware-exercised entry point verbatim
//!       (`nvidia_rm_sys::rm_init::step16`/`step17`, the same calls
//!       `/proc/gpustep16`/`gpustep17` make), or
//!   (b) is pure bookkeeping with no hardware/register access at all
//!       (the GEM handle table, the VRAM bitmap allocator -- which already
//!       existed as dead code, see `NvidiaVramAllocator`), or
//!   (c) is explicitly refused with `EOPNOTSUPP`/`ENOSYS` and a log line,
//!       never silently faked.
//!
//! Concretely implemented: GETPARAM, CHANNEL_ALLOC/FREE (exactly one
//! channel, backed by the existing step16+step17 ladder), GEM_NEW/INFO/
//! CPU_PREP/CPU_FINI (VRAM domain only), VM_INIT. Deliberately refused with
//! `EOPNOTSUPP`: VM_BIND, EXEC -- submitting an arbitrary, Mesa-built
//! command buffer needs a new general-purpose submission path in
//! `nvidia-rm-sys` (today's `step18`/`step19` submit one hardcoded,
//! hand-authored kernel each, not arbitrary content) and a real GPU-VA
//! binding path. That is real, scoped follow-up work, not something to fake
//! here. See `docs/README-nouveau-uapi.md` for the full ioctl-by-ioctl
//! status table and what to test first on real hardware.
//!
//! Entirely opt-in via `nvidia.nouveau_uapi` on the kernel cmdline
//! (`set_enabled`, called from `zCore/src/main.rs`). Disabled by default:
//! `NvidiaGpu::ioctl` behaves exactly as before, byte for byte.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use lock::Mutex;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called from `zCore`'s cmdline parsing when `nvidia.nouveau_uapi` is
/// present. See the module doc for why this defaults to off.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Opt-in for on-demand console-GPU GSP bring-up (`ensure_console_gpu_brought_up`).
/// Default **off**: safer boot (manual `cat /proc/gpustep14`). Enable with
/// `nvidia.console_gsp` on the kernel cmdline.
static CONSOLE_GSP: AtomicBool = AtomicBool::new(false);

pub fn set_console_gsp_enabled(v: bool) {
    CONSOLE_GSP.store(v, Ordering::Relaxed);
}

pub fn console_gsp_enabled() -> bool {
    CONSOLE_GSP.load(Ordering::Relaxed)
}

/// Opt-in CE present via `page_flip` (`nvidia.hwflip`). Default **off**: keep
/// software scanout until the CE path is proven for KMS flips. Does NOT
/// claim hardware KMS (`has_hardware_kms` stays false).
static HWFLIP: AtomicBool = AtomicBool::new(false);

pub fn set_hwflip_enabled(v: bool) {
    HWFLIP.store(v, Ordering::Relaxed);
}

pub fn hwflip_enabled() -> bool {
    HWFLIP.load(Ordering::Relaxed)
}

/// Opt-in NVC57E window ISO surface flip (`nvidia.surfaceflip`). When the
/// ladder is READY, [`super::nvidia::NvidiaGpu::has_hardware_kms`] can claim
/// true so `present_now` prefers `page_flip` over the GOP blit.
static SURFACEFLIP: AtomicBool = AtomicBool::new(false);

pub fn set_surfaceflip_enabled(v: bool) {
    SURFACEFLIP.store(v, Ordering::Relaxed);
}

pub fn surfaceflip_enabled() -> bool {
    SURFACEFLIP.load(Ordering::Relaxed)
}

static FENCE_PAYLOAD_COUNTER: AtomicU32 = AtomicU32::new(1);

/// A fresh, never-zero value to write into `eclipse_rm_exec_submit_signaled`'s
/// kernel-owned fence semaphore each call, so a stale value left over from a
/// previous submission (or the zero the landing zone is cleared to) can never
/// be mistaken for this call's own completion.
pub(super) fn next_fence_payload() -> u32 {
    0x8000_0000 | (FENCE_PAYLOAD_COUNTER.fetch_add(1, Ordering::Relaxed) & 0x7FFF_FFFF)
}

/// Physical address of the RM channel's error notifier, cached at
/// CHANNEL_ALLOC time. The EXEC failure path reads the page through
/// `crate::bus::phys_to_virt` with NO RM calls and NO locks: acquiring the
/// RM's API lock from inside a failure storm is what wedged the machine on
/// real hardware (`DEADLOCK: spinlock(s) stuck >8s`), and before that, the
/// RM's own CPU-mapping of the same page took the kernel down with a page
/// fault. 0 = never captured.
static CHAN_NOTIFIER_PA: AtomicU64 = AtomicU64::new(0);

pub(super) fn set_chan_notifier_pa(pa: u64) {
    CHAN_NOTIFIER_PA.store(pa, Ordering::Relaxed);
}

pub(super) fn chan_notifier_pa_cached() -> Option<u64> {
    match CHAN_NOTIFIER_PA.load(Ordering::Relaxed) {
        0 => None,
        pa => Some(pa),
    }
}

/// NVIF subchannel objects that are backed by REAL RM class objects on a
/// process's RM channel, keyed by `(channel_token, nvif_object_token, rm_handle,
/// owner_pid)`. The channel token is the id CHANNEL_ALLOC handed back (and
/// mesa echoes as NVIF `token`); draining by channel on `CHANNEL_FREE` reaps
/// the throwaway-enumeration leak (NEW without DEL) without touching another
/// live channel's classes.
static CLASS_OBJECTS: lock::Mutex<alloc::vec::Vec<(u64, u64, u32, u64)>> =
    lock::Mutex::new(alloc::vec::Vec::new());

pub(super) fn class_object_insert(
    channel_token: u64,
    object_token: u64,
    rm_handle: u32,
    owner_pid: u64,
) {
    CLASS_OBJECTS
        .lock()
        .push((channel_token, object_token, rm_handle, owner_pid));
}

/// Removes and returns the RM handle for `object_token`, if it was RM-backed —
/// scoped to `owner_pid`. The token is a userspace cookie (mesa passes the
/// object's heap POINTER), and two processes running the same Mesa code get
/// deterministic-enough allocators that equal pointer values across processes
/// are a real possibility. An unscoped removal would let one client's NVIF
/// DEL free ANOTHER live client's (or the compositor's) engine-class object —
/// tearing its 3D class out from under a running channel, whose next method
/// of that class then MMU-faults. Match on (object_token, pid) so a DEL can
/// only ever free the caller's own object.
pub(super) fn class_object_remove(object_token: u64, owner_pid: u64) -> Option<u32> {
    let mut t = CLASS_OBJECTS.lock();
    t.iter()
        .position(|(_, obj, _, pid)| *obj == object_token && *pid == owner_pid)
        .map(|i| t.remove(i).2)
}

/// Drains class objects bound to `channel_token` for `owner_pid` (a
/// `CHANNEL_FREE` of that channel). Other channels' objects stay.
pub(super) fn class_objects_drain_channel(
    channel_token: u64,
    owner_pid: u64,
) -> alloc::vec::Vec<(u64, u32)> {
    let mut t = CLASS_OBJECTS.lock();
    let mut out = alloc::vec::Vec::new();
    let mut i = 0;
    while i < t.len() {
        if t[i].0 == channel_token && t[i].3 == owner_pid {
            let (_ch, token, h, _) = t.remove(i);
            out.push((token, h));
        } else {
            i += 1;
        }
    }
    out
}

/// Drains the class objects owned by `pid` (that process's exit / teardown),
/// returning `(token, rm_handle)` for each. Other processes' objects stay.
pub(super) fn class_objects_drain_pid(pid: u64) -> alloc::vec::Vec<(u64, u32)> {
    let mut t = CLASS_OBJECTS.lock();
    let mut out = alloc::vec::Vec::new();
    let mut i = 0;
    while i < t.len() {
        if t[i].3 == pid {
            let (_ch, token, h, _) = t.remove(i);
            out.push((token, h));
        } else {
            i += 1;
        }
    }
    out
}

/// Per-context "wedged" latch (bit `i` = context `i`). Set when a client
/// context's EXEC fence times out: its ring is jammed (GPGet frozen) and every
/// further submit would just re-poll a dead fence for the full timeout while
/// holding the RM gate -- which starves the compositor's own rendering and
/// froze the desktop/cursor when a hung GL client kept retrying. Once latched,
/// that context's submits fast-fail (no gate-holding poll) until the process
/// exits and its context is rebuilt fresh. Context 0 (the compositor) is never
/// latched. A lock-free u32 bitmask: MAX_CTX (32) fits it exactly (bits 0..31),
// which is why MAX_CTX must never exceed 32.
static CTX_WEDGED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub(super) fn ctx_is_wedged(ctx_idx: u32) -> bool {
    ctx_idx < 32
        && (CTX_WEDGED.load(core::sync::atomic::Ordering::Relaxed) & (1u32 << ctx_idx)) != 0
}

pub(super) fn ctx_set_wedged(ctx_idx: u32) {
    if ctx_idx < 32 {
        CTX_WEDGED.fetch_or(1u32 << ctx_idx, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Clear the latch for a context slot -- on process exit (before the slot is
/// reused) and when a fresh context is assigned, so a re-launched client gets a
/// clean channel, never the previous tenant's wedged verdict.
pub(super) fn ctx_clear_wedged(ctx_idx: u32) {
    if ctx_idx < 32 {
        CTX_WEDGED.fetch_and(!(1u32 << ctx_idx), core::sync::atomic::Ordering::Relaxed);
    }
}

/// De-dup latch for EXEC submission-failure klogs, keyed by the failure's
/// status signature. Userspace controls the retry rate (labwc respawns and
/// resubmits forever), `klog` writes synchronously to the UART with no
/// filter, and a wedged ring makes EVERY retry fail identically -- on real
/// hardware that storm (hundreds of ~180-byte lines per second, ~15 ms of
/// UART time each) monopolised the console path badly enough to starve other
/// spinlock holders past the 8 s deadlock watchdog. Log only when the
/// signature CHANGES; the first line of a new failure mode is the whole
/// diagnostic, every repeat after it is noise.
static LAST_EXEC_FAILURE_SIG: AtomicU64 = AtomicU64::new(u64::MAX);

pub(super) fn exec_failure_changed(sig: u64) -> bool {
    LAST_EXEC_FAILURE_SIG.swap(sig, Ordering::Relaxed) != sig
}

/// Order-sensitive fold of failure statuses into a signature for
/// [`exec_failure_changed`].
pub(super) fn exec_failure_sig(tag: u64, statuses: &[u32]) -> u64 {
    statuses
        .iter()
        .fold(tag ^ 0x9e37_79b9_7f4a_7c15, |acc, &x| {
            acc.rotate_left(13) ^ x as u64
        })
}

/// The last client (`ctx >= 1`) EXEC outcome, pre-formatted, for
/// `/proc/gpudbg`. The reason this exists: on this stack the ONLY thing that
/// exercises a real GPU 3D/compute draw is a GL/Vulkan client (the compositor
/// and lunarbar/foot paint via CPU wl_shm blits), and when a client's draw
/// fails NVK collapses every kernel errno into a single
/// `VK_ERROR_DEVICE_LOST`. The per-stage detail that says WHERE it failed is
/// klog'd, but klog means the serial console / dmesg, which is awkward to read
/// on the target. A GL client is always launched from a terminal, so mirror
/// the last outcome into a file that terminal can `cat` -- one read names the
/// failing stage (prime / lookup / map / token / submit / fence-wait) and the
/// ring pointers, which is exactly what picks between the mutually-exclusive
/// fixes each stage needs.
static LAST_CLIENT_EXEC: Mutex<Option<alloc::string::String>> = Mutex::new(None);

/// Record one client EXEC outcome (success or failure). `ok` true = the fence
/// landed and syncobjs signaled; false = it failed at `stage` with `detail`.
pub(super) fn record_client_exec(line: alloc::string::String) {
    *LAST_CLIENT_EXEC.lock() = Some(line);
}

/// Persistent per-context `ctx_prime` (golden-context load) outcomes for
/// `/proc/gpudbg`. Unlike [`record_client_exec`], this is NOT overwritten by the
/// client's later EXEC records: the prime verdict (OK / TIMEOUT) is the single
/// fork for the FECS ctx-switch hang -- golden context never loaded (prime
/// timed out) vs loaded-but-graphics-incomplete (prime OK yet the 3D draw still
/// hangs FECS in RESTORE). It must survive until next boot even after the client
/// has drawn and failed, so `cat /proc/gpudbg` always names it. One entry per
/// ctx (`1..8`); a later prime of the same ctx replaces its line.
static LAST_PRIME: Mutex<alloc::vec::Vec<(u32, alloc::string::String)>> =
    Mutex::new(alloc::vec::Vec::new());

/// Record the `ctx_prime` outcome for `ctx_idx`, replacing any prior entry for
/// that ctx. Persistent across the client's later draws (see [`LAST_PRIME`]).
pub(super) fn record_prime(ctx_idx: u32, line: alloc::string::String) {
    let mut v = LAST_PRIME.lock();
    if let Some(e) = v.iter_mut().find(|(c, _)| *c == ctx_idx) {
        e.1 = line;
    } else {
        v.push((ctx_idx, line));
    }
}

/// The recorded `ctx_prime` line for `ctx_idx`, if any. Used by the GR-hang
/// probe so a fence-timeout snapshot names, inline, whether the hanging ctx was
/// ever primed -- the decisive datum when the probe shows a FECS RESTORE hang.
pub(super) fn prime_line_for(ctx_idx: u32) -> Option<alloc::string::String> {
    LAST_PRIME
        .lock()
        .iter()
        .find(|(c, _)| *c == ctx_idx)
        .map(|(_, l)| l.clone())
}

/// `/proc/gpudbg` section: persistent `ctx_prime` golden-context outcomes. This
/// is the decisive fork for a FECS ctx-switch hang, and survives the client's
/// draws (which overwrite the last-EXEC slot).
fn format_prime() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = writeln!(
        s,
        "[gpudbg] --- ctx_prime golden-context outcomes (persistent; NOT overwritten by draws) ---"
    );
    let v = LAST_PRIME.lock();
    if v.is_empty() {
        let _ = writeln!(
            s,
            "[gpudbg]  (none this boot -- no client context has been primed yet)"
        );
    } else {
        for (_c, line) in v.iter() {
            let _ = writeln!(s, "[gpudbg]  {}", line);
        }
    }
    s
}

/// GR-engine hang probe snapshot captured at fence-wait timeout.
///
/// Populated once (first timeout) with a BAR0 register snapshot taken at the
/// exact moment the 1 s fence-wait expired.  The snapshot covers the four
/// mutually-exclusive failure modes:
///
/// * **MMU fault** — latched in `NV_PFB_PRI_MMU_FAULT_INFO1` (0xb83090);
///   `valid=1` + `reason` identify the access that triggered it.
/// * **GR engine stalled on a method** — `NV_PGRAPH_STATUS` (0x400700) non-zero
///   and `NV_PGRAPH_TRAPPED_ADDR` (0x400704) shows the subchannel/method.
/// * **PBDMA not drained** — `GP_GET != GP_PUT` means the PBDMA never fetched
///   the client's push; the GPU never even saw the draw commands.
/// * **FECS/GPCCS ctx-switch hang** — status registers at 0x409c00 / 0x41a000.
///
/// The snapshot is preserved until next boot so the terminal user can read it
/// with `cat /proc/gpudbg` after the client has already exited.
static LAST_GR_HANG_PROBE: Mutex<Option<alloc::string::String>> = Mutex::new(None);

/// Record the GR-hang BAR0 snapshot (captured at fence-wait timeout). Only the
/// first timeout per boot is stored; subsequent ones are suppressed because the
/// ring stays wedged and every later submit repeats identically.
pub(super) fn record_gr_hang_probe(snapshot: alloc::string::String) {
    let mut slot = LAST_GR_HANG_PROBE.lock();
    if slot.is_none() {
        *slot = Some(snapshot);
    }
}

/// `/proc/gpudbg` section: GR-engine hang probe snapshot, or a note that no
/// fence-wait timeout has occurred this boot.
fn format_gr_hang_probe() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = writeln!(
        s,
        "[gpudbg] --- GR-engine hang probe (captured at first fence-wait timeout) ---"
    );
    match &*LAST_GR_HANG_PROBE.lock() {
        Some(snap) => {
            for line in snap.lines() {
                let _ = writeln!(s, "[gpudbg]  {}", line);
            }
        }
        None => {
            let _ = writeln!(
                s,
                "[gpudbg]  (none this boot -- no fence-wait timeout has occurred)"
            );
        }
    }
    s
}

/// `/proc/gpudbg` section: the last client EXEC outcome, or a note that none
/// has run this boot (which itself is diagnostic -- it means no GL/Vulkan
/// client ever reached a real draw submit).
pub(super) fn format_last_exec() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = writeln!(s, "[gpudbg] --- last client (ctx>=1) EXEC draw submit ---");
    match &*LAST_CLIENT_EXEC.lock() {
        Some(line) => {
            let _ = writeln!(s, "[gpudbg]  {}", line);
        }
        None => {
            let _ = writeln!(
                s,
                "[gpudbg]  (none this boot -- no GL/Vulkan client reached a real draw submit yet)"
            );
        }
    }
    // The persistent prime verdict: for a FECS ctx-switch hang this is THE fork
    // (golden context never loaded vs loaded-but-graphics-incomplete). Kept
    // separate from the last-EXEC slot above precisely because the client's draw
    // overwrites that slot before the terminal can read it.
    s.push_str(&format_prime());
    // The GR-hang probe is the most actionable diagnostic when the draw never
    // completes: it picks between MMU fault / stalled method / PBDMA not
    // drained / FECS ctx-switch hang -- the four mutually-exclusive root causes.
    s.push_str(&format_gr_hang_probe());
    // The last ioctl error is the one that actually became DEVICE_LOST when the
    // EXEC itself succeeded. THIS is the line that matters when the EXEC above
    // says OK but the client still died.
    let _ = writeln!(
        s,
        "[gpudbg] --- last client nouveau-ioctl ERROR (any kind) ---"
    );
    match &*LAST_CLIENT_IOCTL_ERR.lock() {
        Some(line) => {
            let _ = writeln!(s, "[gpudbg]  {}", line);
        }
        None => {
            let _ = writeln!(
                s,
                "[gpudbg]  (none this boot -- no client nouveau ioctl has returned an error)"
            );
        }
    }
    s
}

/// The last nouveau ioctl (of ANY kind) that returned an error to a client.
static LAST_CLIENT_IOCTL_ERR: Mutex<Option<alloc::string::String>> = Mutex::new(None);

/// Record one client nouveau-ioctl error for `/proc/gpudbg`. Uses the
/// module's existing `nouveau_ioctl_name` (which expects the full nr byte and
/// subtracts `DRM_COMMAND_BASE` itself).
pub(super) fn record_ioctl_err(pid: u64, request: u32, errno: i32) {
    let nr = request & 0xff;
    *LAST_CLIENT_IOCTL_ERR.lock() = Some(alloc::format!(
        "pid={} ioctl {:#010x} (nr={:#04x} {}) -> errno={}",
        pid,
        request,
        nr,
        nouveau_ioctl_name(nr),
        errno
    ));
}

// --- Per-client GEM / VM_BIND memory summary for `/proc/gpudbg` ---
//
// A native Vulkan client (vkcube) can now run on NVK without crashing -- the
// submit completes and WSI presents -- yet its output is visually CORRUPTED
// (block-structured garbage). That is NOT a sync failure; it is a memory-layout
// one: the GPU reads/writes the surface with a swizzle or placement that
// disagrees with what NVK built. This summary discriminates the two suspects
// this driver's own shortcuts create, and survives the client's exit so the
// terminal user runs the app, then `cat /proc/gpudbg` reads the verdict:
//
//  * Compressed PTE kind in play. Sysmem-backed BOs here have no
//    comptag/compression support, so only pitch (0x00) and generic-uncompressed
//    (0x06) are byte-exact. GEM_NEW rejects a compressed kind that arrives as an
//    explicit `tile_flags` at allocation time; but a kind that reaches VM_BIND
//    is MAPPED UNCOMPRESSED (refusing it there hangs the channel -- see
//    `vm_bind_op`), which is byte-exact only if NVK did not actually compress.
//    `vmbind_non_generic > 0` (and the kind list) flags that this path was hit
//    and is the prime suspect when a surface renders as structured garbage.
//  * Tiled surface over host sysmem. GEM_NEW backs GART (and GART|VRAM) with
//    sysmem; a VRAM-only request is real LOCAL and is not CPU-mmapable.
//    `gem_tiled` shows how much tiled memory is in play.
//
// All state is bounded: a handful of atomics plus a 256-bit "kinds seen"
// bitset, reset only at boot.
static GEM_TOTAL: AtomicU32 = AtomicU32::new(0);
static GEM_REQ_VRAM: AtomicU32 = AtomicU32::new(0);
static GEM_REQ_GART: AtomicU32 = AtomicU32::new(0);
static GEM_CPU_MAPPABLE: AtomicU32 = AtomicU32::new(0);
static GEM_TILED: AtomicU32 = AtomicU32::new(0);
static GEM_VRAM_BACKED: AtomicU32 = AtomicU32::new(0);
static VMBIND_MAP: AtomicU32 = AtomicU32::new(0);
static VMBIND_UNMAP: AtomicU32 = AtomicU32::new(0);
static VMBIND_SPARSE: AtomicU32 = AtomicU32::new(0);
static VMBIND_NON_GENERIC: AtomicU32 = AtomicU32::new(0);
/// Bitset over PTE-kind values 0..=255, OR'd from GEM_NEW (`tile_flags >> 8`)
/// and VM_BIND (`flags & 0xff`). Word `k >> 6`, bit `k & 63`.
static KINDS_SEEN: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

fn note_kind_seen(kind: u32) {
    let k = (kind & 0xff) as usize;
    KINDS_SEEN[k >> 6].fetch_or(1u64 << (k & 63), Ordering::Relaxed);
}

/// Record one `GEM_NEW`. `req_domain` is the client's requested
/// NOUVEAU_GEM_DOMAIN_* mask (before we override it to the domain actually
/// used), `cpu_mappable` true when a host PA resolved, `vram_backed` true
/// when the object is `NV01_MEMORY_LOCAL_USER` (VRAM-only, no CPU map),
/// `tile_flags` the requested tiling (upper bits carry the PTE kind).
pub(super) fn record_gem_new(
    req_domain: u32,
    cpu_mappable: bool,
    tile_flags: u32,
    vram_backed: bool,
) {
    GEM_TOTAL.fetch_add(1, Ordering::Relaxed);
    if req_domain & NOUVEAU_GEM_DOMAIN_VRAM != 0 {
        GEM_REQ_VRAM.fetch_add(1, Ordering::Relaxed);
    }
    if req_domain & NOUVEAU_GEM_DOMAIN_GART != 0 {
        GEM_REQ_GART.fetch_add(1, Ordering::Relaxed);
    }
    if cpu_mappable {
        GEM_CPU_MAPPABLE.fetch_add(1, Ordering::Relaxed);
    }
    if vram_backed {
        GEM_VRAM_BACKED.fetch_add(1, Ordering::Relaxed);
    }
    if tile_flags != 0 {
        GEM_TILED.fetch_add(1, Ordering::Relaxed);
        note_kind_seen(tile_flags >> 8);
    }
}

/// Record one `VM_BIND` op. `is_map` true for MAP (vs UNMAP), `sparse` from the
/// SPARSE flag, `pte_kind` = `flags & 0xff`, `non_generic` true when the kind
/// was neither pitch (0x00) nor generic (0x06) and so was mapped uncompressed
/// (refusing it at VM_BIND hangs the channel -- see `vm_bind_op`).
pub(super) fn record_vm_bind(is_map: bool, sparse: bool, pte_kind: u32, non_generic: bool) {
    if is_map {
        VMBIND_MAP.fetch_add(1, Ordering::Relaxed);
    } else {
        VMBIND_UNMAP.fetch_add(1, Ordering::Relaxed);
    }
    if sparse {
        VMBIND_SPARSE.fetch_add(1, Ordering::Relaxed);
    }
    if non_generic {
        VMBIND_NON_GENERIC.fetch_add(1, Ordering::Relaxed);
    }
    note_kind_seen(pte_kind);
}

/// `/proc/gpudbg` section: per-client GEM / VM_BIND memory summary. See the
/// counters' module comment for how to read it.
pub(super) fn format_client_mem() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = writeln!(
        s,
        "[gpudbg] --- client GEM / VM_BIND memory summary (this boot) ---"
    );
    let gem_total = GEM_TOTAL.load(Ordering::Relaxed);
    if gem_total == 0 {
        let _ = writeln!(
            s,
            "[gpudbg]  (none this boot -- no GL/Vulkan client has allocated GEM memory yet)"
        );
        return s;
    }
    let _ = writeln!(
        s,
        "[gpudbg]  GEM_NEW: total={} req_vram={} req_gart={} cpu_mappable={} tiled={} vram_backed={}",
        gem_total,
        GEM_REQ_VRAM.load(Ordering::Relaxed),
        GEM_REQ_GART.load(Ordering::Relaxed),
        GEM_CPU_MAPPABLE.load(Ordering::Relaxed),
        GEM_TILED.load(Ordering::Relaxed),
        GEM_VRAM_BACKED.load(Ordering::Relaxed),
    );
    let non_generic = VMBIND_NON_GENERIC.load(Ordering::Relaxed);
    let _ = writeln!(
        s,
        "[gpudbg]  VM_BIND: map={} unmap={} sparse={} non_generic_kind={}{}",
        VMBIND_MAP.load(Ordering::Relaxed),
        VMBIND_UNMAP.load(Ordering::Relaxed),
        VMBIND_SPARSE.load(Ordering::Relaxed),
        non_generic,
        if non_generic > 0 {
            " <- compressed kind(s) mapped UNCOMPRESSED (byte-inexact for those few surfaces only; NOT the cause of whole-screen garbage when this count is tiny vs map= and the GR-hang probe shows a PBDMA stall with no MMU fault)"
        } else {
            ""
        },
    );
    // Distinct PTE kinds seen. 0x00 (pitch) and 0x06 (generic) are byte-exact
    // for this driver's sysmem-backed BOs; ANY other value needs comptags we do
    // not program -- rejected at GEM_NEW when requested via tile_flags, else
    // mapped uncompressed at VM_BIND (a hang-free but byte-inexact fallback).
    let mut kinds = alloc::string::String::new();
    for k in 0u32..256 {
        if KINDS_SEEN[(k >> 6) as usize].load(Ordering::Relaxed) & (1u64 << (k & 63)) != 0 {
            let _ = write!(kinds, " {:#04x}", k);
        }
    }
    if kinds.is_empty() {
        kinds.push_str(" (none -- all allocations linear)");
    }
    let _ = writeln!(s, "[gpudbg]  PTE kinds seen:{}", kinds);
    s
}

// --- Linux errno values used below (matches linux-object's translation) ---
pub(super) const ENOENT: i32 = 2;
pub(super) const EIO: i32 = 5;
pub(super) const ENOMEM: i32 = 12;
pub(super) const EBUSY: i32 = 16;
pub(super) const ENODEV: i32 = 19;
pub(super) const EINVAL: i32 = 22;
pub(super) const EFAULT: i32 = 14;

/// `access_ok()` for a raw user `(addr, bytes)` range a nouveau ioctl is about
/// to dereference: non-null when `bytes > 0`, and entirely below the canonical
/// user/kernel split (`0x0000_8000_0000_0000` on x86_64, which the Sv39/Sv48
/// and aarch64 user ranges also sit under). Mirrors
/// `kernel_hal::user::user_range_ok`; this crate has no `kernel-hal`
/// dependency, so the (one-line) check is repeated here rather than pulling
/// one in. The kernel is mapped into every address space, so a kernel address
/// passed through the (0666) render node used to resolve and turn the
/// driver's copy into an arbitrary kernel-memory read.
pub(super) fn user_range_ok(addr: usize, bytes: usize) -> bool {
    const USER_MAX: usize = 0x0000_8000_0000_0000;
    if bytes == 0 {
        return true;
    }
    addr != 0 && addr.checked_add(bytes).is_some_and(|end| end <= USER_MAX)
}
pub(super) const ENOSYS: i32 = 38;
pub(super) const EOPNOTSUPP: i32 = 95;

/// `DRM_COMMAND_BASE` (drm.h) -- start of the driver-private ioctl range.
const DRM_COMMAND_BASE: u32 = 0x40;

// --- DRM_NOUVEAU_* command offsets (nouveau_drm.h) ---
const DRM_NOUVEAU_GETPARAM: u32 = 0x00;
const DRM_NOUVEAU_CHANNEL_ALLOC: u32 = 0x02;
const DRM_NOUVEAU_CHANNEL_FREE: u32 = 0x03;
const DRM_NOUVEAU_NVIF: u32 = 0x07;
const DRM_NOUVEAU_VM_INIT: u32 = 0x10;
const DRM_NOUVEAU_VM_BIND: u32 = 0x11;
const DRM_NOUVEAU_EXEC: u32 = 0x12;
const DRM_NOUVEAU_GET_ZCULL_INFO: u32 = 0x13;
const DRM_NOUVEAU_GEM_NEW: u32 = 0x40;
const DRM_NOUVEAU_GEM_PUSHBUF: u32 = 0x41;
const DRM_NOUVEAU_GEM_CPU_PREP: u32 = 0x42;
const DRM_NOUVEAU_GEM_CPU_FINI: u32 = 0x43;
const DRM_NOUVEAU_GEM_INFO: u32 = 0x44;

// NOTE: the canonical `DRM_IOWR(...)`-encoded request numbers used to live
// here, and dispatch matched on them. They are gone on purpose: userspace does
// NOT always encode an ioctl the way the header does -- mesa issues VM_INIT
// through `drmCommandWrite` (_IOW) while the header defines it _IOWR, and NVIF
// multiplexes five different sizes/directions onto one NR. Linux itself
// dispatches driver-private ioctls by NR alone (`_IOC_NR(cmd) -
// DRM_COMMAND_BASE`) and takes the size from its own table, so this driver now
// does the same; see the `NR_*` constants at the end of this file.

// --- Diagnostic: decode + name every nouveau ioctl, handled or not ---
//
// A first-hardware boot's single most useful artifact is the exact list of
// ioctls Mesa issues -- above all *which submission path* it picks: the legacy
// `GEM_PUSHBUF` (classic nvc0 Gallium GL) or the new `EXEC` uAPI (NVK). The
// The dispatch in `nvidia.rs` keys on NR (the low 8 bits -- the stable
// identity of an ioctl, independent of the caller's struct size and
// direction), so this trace decodes the same way and can name what Mesa asked
// for even when the payload size differs from ours.

/// Split a full DRM ioctl request number into `(dir, nr, size)`.
pub(super) fn decode_ioc(request: u32) -> (u32, u32, u32) {
    let dir = (request >> 30) & 0x3;
    let nr = request & 0xff;
    let size = (request >> 16) & 0x3fff;
    (dir, nr, size)
}

/// Human-readable name for a driver-private ioctl NR, covering the whole
/// `nouveau_drm.h` vocabulary -- including ioctls this driver does not
/// implement -- so a trace names what Mesa wanted instead of a bare number.
/// NR is `DRM_COMMAND_BASE + DRM_NOUVEAU_*`; returns `"unknown"` for anything
/// outside nouveau's private range.
pub(super) fn nouveau_ioctl_name(nr: u32) -> &'static str {
    match nr.wrapping_sub(DRM_COMMAND_BASE) {
        DRM_NOUVEAU_GETPARAM => "GETPARAM",
        0x01 => "SETPARAM(deprecated)",
        DRM_NOUVEAU_CHANNEL_ALLOC => "CHANNEL_ALLOC",
        DRM_NOUVEAU_CHANNEL_FREE => "CHANNEL_FREE",
        0x04 => "GROBJ_ALLOC(deprecated)",
        0x05 => "NOTIFIEROBJ_ALLOC(deprecated)",
        0x06 => "GPUOBJ_FREE(deprecated)",
        0x07 => "NVIF",
        0x08 => "SVM_INIT",
        0x09 => "SVM_BIND",
        DRM_NOUVEAU_VM_INIT => "VM_INIT",
        DRM_NOUVEAU_VM_BIND => "VM_BIND",
        DRM_NOUVEAU_EXEC => "EXEC",
        DRM_NOUVEAU_GET_ZCULL_INFO => "GET_ZCULL_INFO",
        DRM_NOUVEAU_GEM_NEW => "GEM_NEW",
        DRM_NOUVEAU_GEM_PUSHBUF => "GEM_PUSHBUF",
        DRM_NOUVEAU_GEM_CPU_PREP => "GEM_CPU_PREP",
        DRM_NOUVEAU_GEM_CPU_FINI => "GEM_CPU_FINI",
        DRM_NOUVEAU_GEM_INFO => "GEM_INFO",
        _ => "unknown",
    }
}

/// First-sight trace de-dup: one bit per ioctl NR. The whole nouveau uAPI is
/// opt-in and only enabled for the NVIDIA GL experiment, so tracing each
/// *distinct* ioctl the first time Mesa issues it gives the full vocabulary
/// (and the submission path) in ~a dozen bounded lines, without flooding the
/// serial console on every per-draw EXEC.
static NR_TRACED: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

/// Log this ioctl by name the first time its NR is seen this boot.
pub(super) fn trace_first_sight(request: u32) {
    let (dir, nr, size) = decode_ioc(request);
    let idx = (nr & 0xff) as usize;
    if !NR_TRACED[idx].swap(true, Ordering::Relaxed) {
        // klog, not log::warn: real-hardware boots run at a log level that
        // drops warn, and this trace -- one bounded line per distinct ioctl --
        // IS the diagnostic. It is already de-duped per NR, so it cannot flood.
        crate::klog_warn!(
            "[nouveau-uapi] first {} : request={:#010x} dir={} nr={:#04x} size={}",
            nouveau_ioctl_name(nr),
            request,
            dir,
            nr,
            size
        );
    }
}

// --- NOUVEAU_GETPARAM_* selectors ---
pub(super) const NOUVEAU_GETPARAM_PCI_VENDOR: u64 = 3;
pub(super) const NOUVEAU_GETPARAM_PCI_DEVICE: u64 = 4;
pub(super) const NOUVEAU_GETPARAM_BUS_TYPE: u64 = 5;
pub(super) const NOUVEAU_GETPARAM_FB_SIZE: u64 = 8;
pub(super) const NOUVEAU_GETPARAM_AGP_SIZE: u64 = 9;
/// NOTE: 14, not 10. Verified against Linux `include/uapi/drm/nouveau_drm.h`
/// (`#define NOUVEAU_GETPARAM_PTIMER_TIME 14`). This was 10 for several
/// milestones, so Mesa's timestamp query hit the unknown-param EINVAL arm
/// while param 10 (unused upstream) answered with a timer value it does not
/// mean.
pub(super) const NOUVEAU_GETPARAM_PTIMER_TIME: u64 = 14;
pub(super) const NOUVEAU_GETPARAM_CHIPSET_ID: u64 = 11;
/// GPC/TPC topology, chip-specific. **Enumeration-fatal**: mesa's
/// `nouveau_ws_device_new` does `if (nouveau_ws_param(fd,
/// NOUVEAU_GETPARAM_GRAPH_UNITS, &value)) goto out_err;` and then unpacks
/// `gpc_count = value & 0xff; tpc_count = (value >> 8) & 0xffff`. Returning
/// EINVAL here silently kills the whole physical device (0 Vulkan GPUs).
/// Linux packs it in `gf100_gr_units()`: `cfg  = gr->gpc_nr;
/// cfg |= gr->tpc_total << 8; cfg |= (u64)gr->rop_nr << 32;`.
pub(super) const NOUVEAU_GETPARAM_GRAPH_UNITS: u64 = 13;
/// Max pushbuffers per EXEC ioctl (new submission uAPI). This driver caps EXEC
/// at 64 pushes, so it answers exactly that.
pub(super) const NOUVEAU_GETPARAM_EXEC_PUSH_MAX: u64 = 17;
pub(super) const NOUVEAU_GETPARAM_HAS_BO_USAGE: u64 = 15;
pub(super) const NOUVEAU_GETPARAM_HAS_PAGEFLIP: u64 = 16;
pub(super) const NOUVEAU_GETPARAM_VRAM_BAR_SIZE: u64 = 18;
pub(super) const NOUVEAU_GETPARAM_VRAM_USED: u64 = 19;
pub(super) const NOUVEAU_GETPARAM_HAS_VMA_TILEMODE: u64 = 20;

// --- NOUVEAU_GEM_DOMAIN_* flags (`nouveau_drm.h`) ---
pub(super) const NOUVEAU_GEM_DOMAIN_VRAM: u32 = 1 << 1;
/// Host system memory reachable by the GPU. NVK's
/// `nvkmd_nouveau_alloc_tiled_mem` picks exactly ONE domain per allocation
/// (`if GART ... else if VRAM ...`), so a GART request carries no VRAM bit --
/// GEM_NEW has to honour it on its own or every host-visible Vulkan
/// allocation fails.
pub(super) const NOUVEAU_GEM_DOMAIN_GART: u32 = 1 << 2;

// --- DRM_NOUVEAU_VM_BIND_OP_* ---
pub(super) const VM_BIND_OP_MAP: u32 = 0x0;
pub(super) const VM_BIND_OP_UNMAP: u32 = 0x1;
/// `DRM_NOUVEAU_VM_BIND_SPARSE` -- the op describes a SPARSE region: a VA
/// range with no GEM object behind it (`handle` is 0), whose pages read as
/// zero instead of faulting. NVK asks for these only for sparse Vulkan
/// resources (`nvk_image.c`/`nvk_buffer.c`), never during device creation.
pub(super) const VM_BIND_SPARSE: u32 = 1 << 8;
/// In a `VM_BIND` op, the low byte of `flags` is the PTE kind mesa wants the
/// mapping programmed with (`nouveau_ws_bo_bind` passes `pte_kind` straight
/// through as `flags`). 0 means plain linear.
pub(super) const VM_BIND_PTE_KIND_MASK: u32 = 0xff;
pub(super) const PTE_KIND_PITCH: u32 = 0x00;
pub(super) const PTE_KIND_GENERIC: u32 = 0x06;

#[inline]
pub(super) const fn vm_bind_pte_kind(flags: u32) -> u32 {
    flags & VM_BIND_PTE_KIND_MASK
}

#[inline]
pub(super) const fn pte_kind_is_supported(kind: u32) -> bool {
    // The Turing UNCOMPRESSED kind set (tu102 dev_mmu.h): PITCH (0x00), the
    // Z/S family Z16/S8/S8Z24/ZF32_X24S8/Z24S8 (0x01..0x05) and
    // GENERIC_MEMORY (0x06). All of these are programmed into the PTEs
    // verbatim -- none needs comptags. 0x07 is INVALID and 0x08..0x0F are the
    // COMPRESSIBLE kinds (handled separately: converted to their uncompressed
    // pair, since this driver has no comptag allocator).
    kind <= PTE_KIND_GENERIC
}

// --- DRM_NOUVEAU_SYNC_* (drm_nouveau_sync.flags) ---
pub(super) const SYNC_TIMELINE_SYNCOBJ: u32 = 0x1;
pub(super) const SYNC_TYPE_MASK: u32 = 0xf;

// --- Structs, field-for-field identical to nouveau_drm.h (natural C layout) ---

#[repr(C)]
pub(super) struct DrmNouveauGetparam {
    pub param: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct DrmNouveauChannelAllocSubchan {
    pub handle: u32,
    pub grclass: u32,
}

#[repr(C)]
pub(super) struct DrmNouveauChannelAlloc {
    pub fb_ctxdma_handle: u32,
    pub tt_ctxdma_handle: u32,
    pub channel: i32,
    pub pushbuf_domains: u32,
    pub notifier_handle: u32,
    pub subchan: [DrmNouveauChannelAllocSubchan; 8],
    pub nr_subchan: u32,
}

#[repr(C)]
pub(super) struct DrmNouveauChannelFree {
    pub channel: i32,
}

#[repr(C)]
pub(super) struct DrmNouveauGemInfo {
    pub handle: u32,
    pub domain: u32,
    pub size: u64,
    pub offset: u64,
    pub map_handle: u64,
    pub tile_mode: u32,
    pub tile_flags: u32,
}

#[repr(C)]
pub(super) struct DrmNouveauGemNew {
    pub info: DrmNouveauGemInfo,
    pub channel_hint: u32,
    pub align: u32,
}

#[repr(C)]
pub(super) struct DrmNouveauGemCpuPrep {
    pub handle: u32,
    pub flags: u32,
}

#[repr(C)]
pub(super) struct DrmNouveauGemCpuFini {
    pub handle: u32,
}

#[repr(C)]
pub(super) struct DrmNouveauVmInit {
    pub kernel_managed_addr: u64,
    pub kernel_managed_size: u64,
}

#[repr(C)]
#[allow(dead_code)] // fields read for validation only in this milestone (VM_BIND itself is EOPNOTSUPP)
pub(super) struct DrmNouveauVmBindOp {
    pub op: u32,
    pub flags: u32,
    pub handle: u32,
    pub pad: u32,
    pub addr: u64,
    pub bo_offset: u64,
    pub range: u64,
}

#[repr(C)]
#[allow(dead_code)]
pub(super) struct DrmNouveauVmBind {
    pub op_count: u32,
    pub flags: u32,
    pub wait_count: u32,
    pub sig_count: u32,
    pub wait_ptr: u64,
    pub sig_ptr: u64,
    pub op_ptr: u64,
}

#[repr(C)]
pub(super) struct DrmNouveauSync {
    pub flags: u32,
    pub handle: u32,
    pub timeline_value: u64,
}

#[repr(C)]
#[allow(dead_code)]
pub(super) struct DrmNouveauExecPush {
    pub va: u64,
    pub va_len: u32,
    pub flags: u32,
}

#[repr(C)]
#[allow(dead_code)]
pub(super) struct DrmNouveauExec {
    pub channel: u32,
    pub push_count: u32,
    pub wait_count: u32,
    pub sig_count: u32,
    pub wait_ptr: u64,
    pub sig_ptr: u64,
    pub push_ptr: u64,
}

// --- Legacy GEM_PUSHBUF submission ABI (nouveau_drm.h) --------------------------
//
// This is the path the classic **nvc0 Gallium** driver (Mesa's OpenGL for
// Turing) uses — NOT the new VM_BIND/EXEC uAPI. Field-for-field identical to
// `nouveau_drm.h` so the ioctl request number (which bakes in the struct size)
// matches what Mesa's libdrm issues. Only parsed+logged for now (see the
// dispatch arm in `nvidia.rs`): real submission needs GART-domain GEM, the 3D
// class bound to the channel, and relocation handling — hardware-validated
// follow-up, never faked.

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(super) struct DrmNouveauGemPushbufBoPresumed {
    pub valid: u32,
    pub domain: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(super) struct DrmNouveauGemPushbufBo {
    pub user_priv: u64,
    pub handle: u32,
    pub read_domains: u32,
    pub write_domains: u32,
    pub valid_domains: u32,
    pub presumed: DrmNouveauGemPushbufBoPresumed,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(super) struct DrmNouveauGemPushbufReloc {
    pub reloc_bo_index: u32,
    pub reloc_bo_offset: u32,
    pub bo_index: u32,
    pub flags: u32,
    pub data: u32,
    pub vor: u32,
    pub tor: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(super) struct DrmNouveauGemPushbufPush {
    pub bo_index: u32,
    pub pad: u32,
    pub offset: u64,
    pub length: u64,
}

#[repr(C)]
#[allow(dead_code)]
pub(super) struct DrmNouveauGemPushbuf {
    pub channel: u32,
    pub nr_buffers: u32,
    pub buffers: u64,
    pub nr_relocs: u32,
    pub nr_push: u32,
    pub relocs: u64,
    pub push: u64,
    pub suffix0: u32,
    pub suffix1: u32,
    pub vram_available: u64,
    pub gart_available: u64,
}

// --- Driver-side bookkeeping (Eclipse-internal, not part of the UAPI wire format) ---

/// The single channel this milestone supports, cached after `CHANNEL_ALLOC`
/// reuses the existing step16+step17 bring-up ladder. `h_vas` and
/// `notifier_handle` are real RM object handles from that ladder; nothing
/// here is invented.
pub(super) struct NouveauChannelState {
    /// Channel id handed back in `drm_nouveau_channel_alloc.channel`, and the
    /// value mesa echoes as the NVIF `token` when it enumerates classes or
    /// allocates subchannels on this channel. Signed to match the uAPI's
    /// `__s32 channel`.
    pub id: i32,
    pub h_vas: u32,
    pub notifier_handle: u32,
    /// Whether this channel is backed by a real RM GR channel (the
    /// `step16`+`step17` ladder) or is a *discovery* channel: NVK allocates a
    /// throwaway channel during `vkEnumeratePhysicalDevices` purely to ask
    /// `NVIF SCLASS` which engine classes exist, then frees it without ever
    /// submitting. Serving that from software lets the GPU enumerate on a
    /// kernel whose RM is not attached yet, while every path that genuinely
    /// needs hardware (GEM_NEW/VM_BIND/EXEC) still refuses with ENODEV.
    pub rm_backed: bool,
    /// Which per-process GPU context (independent VAS + GPFIFO channel) this
    /// channel belongs to. 0 = the compositor's singleton context (the
    /// `step16`+`step17` ladder), and also the value for discovery channels.
    /// >= 1 = a GL client's OWN context built by `nvidia_rm_sys::rm_init::
    /// ctx_alloc`, so a client's `VM_BIND`/`EXEC` route to its own VAS/channel
    /// instead of trampling the compositor's. `VM_BIND`/`EXEC` recover it from
    /// the calling pid via `ctx_idx_for_pid`.
    pub ctx_idx: u32,
    /// pid that issued `CHANNEL_ALLOC`, pushed down from `linux-object`'s
    /// ioctl dispatch (this crate can't learn it itself -- see
    /// `DrmScheme::ioctl_owned`'s doc). Used by `nouveau_release_process`
    /// to reclaim the channel (and everything bound in it) if this
    /// process exits without an explicit `CHANNEL_FREE`. 0 (no known
    /// caller, e.g. `ioctl` instead of `ioctl_owned`) never matches a
    /// real exiting pid, so such a channel is simply never auto-reclaimed.
    pub owner_pid: u64,
}

/// A GEM object allocated through `GEM_NEW`. Backed by a real RM memory
/// object (`nvidia_rm_sys::rm_init::gem_alloc`): `NV01_MEMORY_SYSTEM` for
/// GART / GART|VRAM, or `NV01_MEMORY_LOCAL_USER` for VRAM-only.
pub(super) struct NouveauGemObject {
    /// Handle returned to userspace (nouveau-uAPI wire format). Distinct
    /// from `h_memory`: this is Eclipse's own counter, not an RM handle.
    /// Drawn from this GPU's own slice of the high half of the `u32` range
    /// (`crate::scheme::gem_mmap::alloc_handle_slice`), so it can collide
    /// neither with `linux-object`'s own `DRM_STATE` handle ids -- both are
    /// decoded from the same fake-mmap-offset space by `DrmDev::get_vmo` --
    /// nor with another GPU's handles, which share the same global
    /// `gem_mmap` / `NOUVEAU_CPU_VMOS` tables.
    pub handle: u32,
    /// The real RM memory object handle backing this allocation.
    pub h_memory: u32,
    /// pid that allocated this object (`GEM_NEW`). Process-exit teardown frees
    /// ONLY the exiting process's objects -- freeing every object on any exit
    /// (the old single-process assumption) would pull the compositor's buffers
    /// out from under it the moment a GL client closed.
    pub owner_pid: u64,
    pub size: u64,
    /// BAR1-relative CPU physical address (`gem_map_cpu`'s `AT_CPU`
    /// offset), if resolving one succeeded at `GEM_NEW` time. `None` means
    /// this object is real (VM_BIND/EXEC both still work) but not
    /// CPU-mmap-able -- see the `GEM_NEW` gap note in
    /// docs/README-nouveau-uapi.md.
    pub phys_addr: Option<u64>,
    /// FBMEM (VRAM) offset from `gem_fbmem_offset` / `memdescGetPhysAddr
    /// AT_GPU`, when this is a VRAM-only object. Consumed by NVC57E
    /// surfaceflip / CE present bookkeeping.
    pub vram_offset: Option<u64>,
    /// Domain actually used (`NOUVEAU_GEM_DOMAIN_GART` or `_VRAM`), so
    /// `GEM_INFO` reports the backing rather than a hardcoded VRAM lie.
    pub domain: u32,
    /// Tiling the client asked for at `GEM_NEW`, kept so `GEM_INFO` returns
    /// what was requested. `tile_flags`'s upper bits carry the PTE kind
    /// (`pte_kind << 8`); 0/0 is plain linear.
    pub tile_mode: u32,
    pub tile_flags: u32,
}

/// A GPU-VA mapping created by `VM_BIND` (`MAP` op), tracked so `UNMAP` can
/// find the RM virtual-range handle to tear down.
pub(super) struct NouveauVmMapping {
    /// Which `NouveauGemObject::handle` this binds.
    pub gem_handle: u32,
    /// RM's virtual-range object handle (`vm_bind_map`'s `h_virt`) -- needed
    /// to `Unmap`+`Free` it later.
    pub h_virt: u32,
    /// pid that created this mapping. With per-process GPU contexts each client
    /// has its OWN VA space, so a bind is meaningful ONLY within its owner's
    /// context: process-exit teardown and MAP/UNMAP overlap-replace must scope
    /// to `owner_pid`, or a client's exit (or a same-VA bind in a DIFFERENT
    /// context) would tear down the compositor's or another client's mapping.
    pub owner_pid: u64,
    pub va: u64,
    pub size: u64,
    /// Offset into the GEM object this mapping starts at (`bo_offset` from
    /// the VM_BIND op) -- needed to translate a GPU VA inside this mapping
    /// back to a CPU-readable physical address (gem phys + bo_offset + delta).
    pub bo_offset: u64,
}

// ===================== NVIF (`DRM_NOUVEAU_NVIF`, nr 0x47) =====================
//
// NVIF is nouveau's generic object-model ioctl, and mesa's NVK winsys leans on
// it during *physical device enumeration* -- long before any rendering. It is
// therefore mandatory: `nouveau_ws_device_new()` fails, and NVK reports zero
// Vulkan GPUs, if any of these calls fails.
//
// Every NVIF call shares nr 0x47 but carries a DIFFERENT payload size and
// direction (72B W, 136B WR, 160B WR, 56B W, 24B W), so it can only be
// dispatched by NR -- never by full ioctl request number. See `nouveau_ioctl`.
//
// Layouts below are transcribed byte-for-byte from mesa's
// `src/nouveau/drm/nvif/{ioctl,cl0080}.h` (= Linux's `include/nvif/`).

/// `nvif_ioctl_v0.type` values (nvif/ioctl.h).
pub(super) const NVIF_IOCTL_V0_SCLASS: u8 = 0x01;
pub(super) const NVIF_IOCTL_V0_NEW: u8 = 0x02;
pub(super) const NVIF_IOCTL_V0_DEL: u8 = 0x03;
pub(super) const NVIF_IOCTL_V0_MTHD: u8 = 0x04;

/// `NV_DEVICE` class handle (nvif/class.h: `#define NV_DEVICE 0x00000080`).
pub(super) const NVIF_CLASS_NV_DEVICE: i32 = 0x0000_0080;
/// `NV_DEVICE_V0_INFO` method (nvif/cl0080.h).
pub(super) const NV_DEVICE_V0_INFO: u8 = 0x00;
/// `nv_device_info_v0.platform` = PCIE. Mesa maps PCI/AGP/PCIE/default to
/// `NV_DEVICE_TYPE_DIS` (discrete), which its conformance gate requires.
pub(super) const NV_DEVICE_INFO_V0_PCIE: u8 = 0x03;

/// Common 24-byte NVIF header (`struct nvif_ioctl_v0`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct NvifIoctlV0 {
    pub version: u8,
    pub type_: u8,
    pub pad02: [u8; 4],
    pub owner: u8,
    pub route: u8,
    pub token: u64,
    pub object: u64,
    // followed by a per-type body
}

/// `struct nvif_ioctl_new_v0` (32 bytes), body of a `NEW`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct NvifIoctlNewV0 {
    pub version: u8,
    pub pad01: [u8; 6],
    pub route: u8,
    pub token: u64,
    pub object: u64,
    pub handle: u32,
    pub oclass: i32,
    // followed by class data (e.g. NvDeviceV0)
}

/// `struct nvif_ioctl_mthd_v0` (8 bytes), body of an `MTHD`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct NvifIoctlMthdV0 {
    pub version: u8,
    pub method: u8,
    pub pad02: [u8; 6],
    // followed by method data (e.g. NvDeviceInfoV0)
}

/// `struct nvif_ioctl_sclass_v0` (8 bytes), body of an `SCLASS`, followed by
/// `count` entries of `NvifSclassOclassV0`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct NvifIoctlSclassV0 {
    pub version: u8,
    /// IN: how many entries the caller has room for (mesa passes 16).
    /// OUT: how many we filled.
    pub count: u8,
    pub pad02: [u8; 6],
}

/// `struct nvif_ioctl_sclass_oclass_v0` (8 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct NvifSclassOclassV0 {
    pub oclass: i32,
    pub minver: i16,
    pub maxver: i16,
}

/// `struct nv_device_v0` (16 bytes), class data for `NEW` of `NV_DEVICE`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct NvDeviceV0 {
    pub version: u8,
    pub pad01: [u8; 7],
    /// Device selector; mesa passes `~0` ("client default").
    pub device: u64,
}

/// `struct nv_device_info_v0` (104 bytes) -- the reply mesa reads for
/// chipset/VRAM/type. Mesa consumes `chipset`, `ram_user` (-> vram_size_B),
/// `platform` (-> device type) and copies `chip`/`name` as display strings.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct NvDeviceInfoV0 {
    pub version: u8,
    pub platform: u8,
    /// From NV_PMC_BOOT_0 -- the value mesa maps to an SM version.
    pub chipset: u16,
    pub revision: u8,
    pub family: u8,
    pub pad06: [u8; 2],
    pub ram_size: u64,
    pub ram_user: u64,
    pub chip: [u8; 16],
    pub name: [u8; 64],
}

// --- Compile-time ABI checks ------------------------------------------------
//
// These layouts were transcribed from mesa's `nvif/{ioctl,cl0080}.h` by hand
// and cannot be validated on hardware from here, so pin them at compile time:
// a wrong offset becomes a build failure instead of a GPU that silently
// refuses to enumerate. The totals are the exact payload sizes mesa sends
// (`sizeof` of its anonymous request structs).
const _: () = {
    use core::mem::size_of as sz;
    assert!(sz::<NvifIoctlV0>() == 24);
    assert!(sz::<NvifIoctlNewV0>() == 32);
    assert!(sz::<NvifIoctlMthdV0>() == 8);
    assert!(sz::<NvifIoctlSclassV0>() == 8);
    assert!(sz::<NvifSclassOclassV0>() == 8);
    assert!(sz::<NvDeviceV0>() == 16);
    assert!(sz::<NvDeviceInfoV0>() == 104);
    // nouveau_ws_device_alloc: ioctl + new + nv_device_v0
    assert!(sz::<NvifIoctlV0>() + sz::<NvifIoctlNewV0>() + sz::<NvDeviceV0>() == 72);
    // nouveau_ws_device_info: ioctl + mthd + nv_device_info_v0
    assert!(sz::<NvifIoctlV0>() + sz::<NvifIoctlMthdV0>() + sz::<NvDeviceInfoV0>() == 136);
    // nouveau_ws_context_query_classes: ioctl + sclass + 16 oclass slots
    assert!(
        sz::<NvifIoctlV0>() + sz::<NvifIoctlSclassV0>() + 16 * sz::<NvifSclassOclassV0>() == 160
    );
    // nouveau_ws_subchan_alloc: ioctl + new
    assert!(sz::<NvifIoctlV0>() + sz::<NvifIoctlNewV0>() == 56);
};

/// Ceiling on live nouveau channels per GPU. Bounds the channel Vec against a
/// runaway; the number of REAL RM-backed contexts is separately capped by
/// `MAX_CTX`, so the extra headroom here only ever holds cheap discovery/alias
/// entries. Raised from 16 after a real-RTX boot: NVK creates throwaway
/// enumeration contexts (a CHANNEL_ALLOC + 5 NVIF class NEWs each) and, when
/// wlroots' native-Vulkan renderer retry-loops on its external-semaphore
/// failure, a single process can allocate channels in a burst faster than it
/// frees them. At 16 that burst hit the cap -> CHANNEL_ALLOC EBUSY (errno 16)
/// -> NVK enumerate -3 -> "could not match drm and vulkan device", and the
/// desktop wedged. The default renderer is GLES2/zink now (which does not
/// churn), so this is belt-and-braces; 64 absorbs any enumeration burst.
pub(super) const MAX_CHANNELS: usize = 64;

/// Number of per-process GPU contexts (index 0 = the compositor's singleton;
/// 1.. = one per GL client). MUST match `ECLIPSE_MAX_CTX` in
/// `vendor/eclipse_rm_init.c`. Raised from 8 after a real-RTX boot exhausted
/// the slots: labwc's startup burst plus crash-loop respawn overlap filled all
/// 8, dropping later clients to the software path where their VM_BIND collided
/// at the shared top-of-heap VA (0x3fffff000, RM status 0x51). Contexts are
/// lazy (one per live client pid, freed on process exit), so this is a cap.
/// Must stay <= 32: the `CTX_WEDGED` latch is a u32 bitmask (ctx_idx < 32).
pub(super) const MAX_CTX: u32 = 32;

/// How many class slots mesa offers in an `SCLASS` call
/// (`NOUVEAU_WS_CONTEXT_MAX_CLASSES`).
pub(super) const NVIF_SCLASS_MAX: usize = 16;

// --- Engine classes advertised through SCLASS -------------------------------
//
// Mesa picks, per engine type, the HIGHEST advertised class whose LOW BYTE
// matches: 0xb5 copy, 0x2d 2d, 0x97 3d, 0x40 (else 0x39) m2mf, 0xc0 compute.
// A type with no match yields oclass 0 and mesa fails the device with EINVAL,
// so all five types must be present. Values from mesa's NVIDIA class headers.
/// `FERMI_TWOD_A` -- still the 2d class on every Turing+ chip.
pub(super) const CLASS_FERMI_TWOD_A: i32 = 0x902d;
/// `KEPLER_INLINE_TO_MEMORY_B` -- the m2mf (0x40) class on Turing+.
pub(super) const CLASS_KEPLER_INLINE_TO_MEMORY_B: i32 = 0xa140;

/// Per-architecture (3d, compute, dma-copy) class triple.
///
/// NVK's conformance gate additionally requires the 3d class to be in
/// `[KEPLER_A 0xa097 ..= ADA_A 0xc997]` or exactly `BLACKWELL_B 0xce97`;
/// anything else makes a release-built NVK skip the GPU silently.
pub(super) const CLASSES_TURING: (i32, i32, i32) = (0xc597, 0xc5c0, 0xc5b5);
pub(super) const CLASSES_AMPERE: (i32, i32, i32) = (0xc797, 0xc7c0, 0xc7b5);
/// Ada has NO copy class of its own -- RM's CE dispatch jumps straight from
/// `AMPERE_DMA_COPY_B` to `HOPPER_DMA_COPY_A`, so AD10x uses the Ampere-B one.
/// (0xc9b5 is `BLACKWELL_DMA_COPY_A`; advertising it here would make mesa emit
/// Blackwell CE methods on an Ada part and make our own GSP-RM reject the
/// NvRmAlloc.) Verified against this repo's vendored
/// `open-gpu-kernel-modules/.../g_allclasses.h`.
pub(super) const CLASSES_ADA: (i32, i32, i32) = (0xc997, 0xc9c0, 0xc7b5);
/// Hopper/Blackwell: UNVERIFIED against a real chip by this milestone. Note
/// `HOPPER_A` (0xcb97) and `BLACKWELL_A` (0xcd97) are NOT in NVK's conformant
/// range, so a release-built NVK skips such a GPU regardless of what we say;
/// `BLACKWELL_B` (0xce97) is the consumer GB20x class and IS accepted.
pub(super) const CLASSES_HOPPER: (i32, i32, i32) = (0xcb97, 0xcbc0, 0xc8b5);
/// Blackwell comes in two generations: `BLACKWELL_A`/`_COMPUTE_A`/`_DMA_COPY_A`
/// (0xcd97/0xcdc0/0xc9b5, GB100 datacenter) and `BLACKWELL_B`/`_COMPUTE_B`/
/// `_DMA_COPY_B` (0xce97/0xcec0/0xcab5, GB20x / RTX 50). We report the B set:
/// it matches the consumer parts this driver targets, and `BLACKWELL_B` is the
/// only Blackwell 3D class NVK accepts as conformant.
pub(super) const CLASSES_BLACKWELL: (i32, i32, i32) = (0xce97, 0xcec0, 0xcab5);

/// Minimum payload a caller must supply for a given driver-private NR.
///
/// The dispatch in `nvidia.rs` keys on NR alone (like Linux), which is what
/// lets it accept mesa's `_IOW`-encoded VM_INIT and NVIF's five different
/// shapes. The flip side is that the caller's declared size is no longer
/// implied by the request number, so it must be checked explicitly before any
/// arm casts the argument to a fixed struct and writes into it.
///
/// `None` means "no fixed minimum" -- only NVIF, which validates its own
/// header and per-type bodies internally because one NR carries five layouts.
pub(super) fn min_payload_for_nr(nr: u32) -> Option<usize> {
    use core::mem::size_of;
    Some(match nr {
        NR_GETPARAM => size_of::<DrmNouveauGetparam>(),
        NR_CHANNEL_ALLOC => size_of::<DrmNouveauChannelAlloc>(),
        NR_CHANNEL_FREE => size_of::<DrmNouveauChannelFree>(),
        NR_VM_INIT => size_of::<DrmNouveauVmInit>(),
        NR_VM_BIND => size_of::<DrmNouveauVmBind>(),
        NR_EXEC => size_of::<DrmNouveauExec>(),
        NR_GEM_NEW => size_of::<DrmNouveauGemNew>(),
        NR_GEM_PUSHBUF => size_of::<DrmNouveauGemPushbuf>(),
        NR_GEM_CPU_PREP => size_of::<DrmNouveauGemCpuPrep>(),
        NR_GEM_CPU_FINI => size_of::<DrmNouveauGemCpuFini>(),
        NR_GEM_INFO => size_of::<DrmNouveauGemInfo>(),
        _ => return None,
    })
}

// --- Driver-private ioctl NRs (dispatch keys) -------------------------------
//
// Linux dispatches driver-private ioctls by NR alone:
//   nouveau_drm.c: `switch (_IOC_NR(cmd) - DRM_COMMAND_BASE)`
// The caller's direction and size bits are advisory. Matching the FULL request
// number instead makes an ioctl unreachable whenever userspace encodes it
// differently -- which is exactly what happened with VM_INIT (mesa issues it
// via drmCommandWrite = _IOW, we only accepted _IOWR).
pub(super) const NR_GETPARAM: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_GETPARAM;
pub(super) const NR_CHANNEL_ALLOC: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_CHANNEL_ALLOC;
pub(super) const NR_CHANNEL_FREE: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_CHANNEL_FREE;
pub(super) const NR_NVIF: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_NVIF;
pub(super) const NR_VM_INIT: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_VM_INIT;
pub(super) const NR_VM_BIND: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_VM_BIND;
pub(super) const NR_EXEC: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_EXEC;
pub(super) const NR_GET_ZCULL_INFO: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_GET_ZCULL_INFO;
pub(super) const NR_GEM_NEW: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_GEM_NEW;
pub(super) const NR_GEM_PUSHBUF: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_GEM_PUSHBUF;
pub(super) const NR_GEM_CPU_PREP: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_GEM_CPU_PREP;
pub(super) const NR_GEM_CPU_FINI: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_GEM_CPU_FINI;
pub(super) const NR_GEM_INFO: u32 = DRM_COMMAND_BASE + DRM_NOUVEAU_GEM_INFO;

// --- Direct-submit fast path (EXEC without an RM entry per submission) ------
//
// See `eclipse_rm_exec_fast_prepare` (nvidia-rm-sys/vendor/eclipse_rm_init.c)
// for the RM half. This is the per-context state the Rust submit path needs,
// the method/GP-entry encoders (checked against the SDK's DRF macros at
// prepare time), and the counters `/proc/gpudbg` prints so a boot can say
// where a frame's time goes without a debugger.

/// Whether EXEC may take the direct-submit path (default on). Cleared by the
/// `nvidia.exec_rm` cmdline flag to A/B against the RM-per-submit path.
static EXEC_FAST_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_exec_fast_enabled(v: bool) {
    EXEC_FAST_ENABLED.store(v, Ordering::SeqCst);
}

pub fn exec_fast_enabled() -> bool {
    EXEC_FAST_ENABLED.load(Ordering::Relaxed)
}

/// Per-context constants for a direct submit, derived from
/// `nvidia_rm_sys::rm_init::ExecFast` with every physical page already
/// turned into a kernel virtual address.
pub(super) struct FastCtx {
    /// USERD `GPGet` (read-only, written back by the host) and `GPPut`.
    pub userd_gpget: usize,
    pub userd_gpput: usize,
    /// Ring page (128 x 8 B GP entries).
    pub gpfifo_va: usize,
    /// 128 x 32 B host SEM RELEASE method streams, one per ring slot.
    pub fence_pb_va: usize,
    /// The u32 landing zone the fence streams release into.
    pub fence_sem_va: usize,
    pub buf_gpu_va: u64,
    pub fence_pb_off: u32,
    pub fence_sem_off: u32,
    pub slot_bytes: u32,
    pub entries: u32,
    pub work_token: u32,
    pub runlist_id: u32,
    /// BAR0 kernel VA of the usermode doorbell register.
    pub doorbell_va: usize,
    /// Next fence payload (per-context monotonic, starts at 1; the landing
    /// zone starts at 0 so the wrapping `>=` in `syncobj` is never true early).
    pub next_payload: u32,
    pub submits: u64,
    pub fenced: u64,
}

/// One direct-submit failure mode, for the caller to turn into an errno and
/// a diagnostic.
pub(super) enum FastSubmitError {
    /// The context has no (or no longer has a) direct-submit state.
    Gone,
    /// The GPFIFO ring did not free `needed` slots within the bound: GPGet is
    /// frozen -- the channel is wedged (same signal the RM path reported as
    /// `NV_ERR_BUSY_RETRY`).
    RingFull { put: u32, get: u32, needed: u32 },
}

/// The GPFIFO ring's accounting: how the submit path turns the raw `GPPut` and
/// `GPGet` it reads back from the channel's USERD window into "is there room
/// for `needed` entries", and where the next entry goes.
///
/// Returns the wrapped `(put, get, room)`, or `None` for a ring of no entries.
/// All three inputs come from outside this kernel -- GPPut and GPGet are
/// written by the GPU and by the RM's own self-test submissions, and the size
/// is whatever `eclipse_rm_exec_fast_prepare` filled in -- so none of them may
/// be trusted to be sane: `% entries` on a zero-sized ring is a divide by zero,
/// which from `EXEC` means a kernel panic raised by any process with the device
/// open. The arithmetic runs in 64 bits for the same reason: `put + entries`
/// overflows a `u32` for a nonsense ring size, and an overflow there would
/// report a full ring as an empty one and overwrite entries the GPU has not
/// fetched yet.
pub(super) fn ring_state(
    put_raw: u32,
    get_raw: u32,
    entries: u32,
    needed: u32,
) -> Option<RingState> {
    if entries == 0 {
        return None;
    }
    let (put, get, entries64) = (
        (put_raw % entries) as u64,
        (get_raw % entries) as u64,
        entries as u64,
    );
    let used = (put + entries64 - get) % entries64;
    Some(RingState {
        put: put as u32,
        get: get as u32,
        // One entry stays unused on purpose: with all of them in play a full
        // ring and an empty one both have GPPut == GPGet, and the host would
        // read the full one as empty.
        room: used + (needed as u64) < entries64,
    })
}

/// What [`ring_state`] answers: the wrapped ring pointers and whether the
/// submission fits behind them.
pub(super) struct RingState {
    pub put: u32,
    pub get: u32,
    pub room: bool,
}

/// `NV906F_GP_ENTRY0`: GET = bits 31:2 of the push VA's low half,
/// NO_CONTEXT_SWITCH = FALSE.
#[inline]
pub(super) const fn gp_entry0(push_va: u64) -> u32 {
    (push_va as u32) & !0x3
}

/// `NV906F_GP_ENTRY1`: GET_HI (7:0) | LENGTH in dwords (30:10) | LEVEL_MAIN.
#[inline]
pub(super) const fn gp_entry1(push_va: u64, len_bytes: u32) -> u32 {
    (((push_va >> 32) as u32) & 0xff) | (((len_bytes / 4) & 0x1f_ffff) << 10)
}

/// `NV906F_DMA` header, INC_METHOD: SEC_OP (31:29) = 1, COUNT (28:16),
/// SUBCHANNEL (15:13), METHOD_ADDRESS (11:0) = mthd >> 2.
#[inline]
pub(super) const fn push_hdr(subch: u32, mthd: u32, count: u32) -> u32 {
    (1 << 29) | ((count & 0x1fff) << 16) | ((subch & 0x7) << 13) | ((mthd >> 2) & 0xfff)
}

pub(super) const NVC46F_SEM_ADDR_LO: u32 = 0x5c;
/// `NVC46F_SEM_EXECUTE`: OPERATION_RELEASE (2:0 = 1), **RELEASE_WFI_EN** (bit
/// 20 = 1), PAYLOAD_SIZE_32BIT (bit 24 = 0), RELEASE_TIMESTAMP_DIS (bit 25 = 0).
///
/// WFI_EN is what turns this from "the PBDMA fetched the caller's push" into
/// a real completion fence: the host waits for the channel's engines (GR/CE)
/// to go idle before writing the payload. With WFI_DIS the semaphore landed
/// as soon as the PBDMA *processed* the method, while the engine was still
/// executing the pushes in front of it -- and since every NVK syncobj resolves
/// against this payload, Mesa/wlroots reused staging buffers and sampled
/// textures the GPU had not finished writing: partial texture uploads, stale
/// tiles, garbage in freshly allocated surfaces (the lunarbar app-menu popup on
/// the dual-RTX box). Linux nouveau (`gv100_fence_emit32`) and NVK's own queue
/// fence both release with WFI_EN; so do we now. Must stay in step with
/// `chkSemExecute` in `eclipse_rm_exec_fast_prepare` or `check_encodings`
/// disables the direct-submit path.
pub(super) const NVC46F_SEM_EXECUTE_RELEASE: u32 = 0x1 | (1 << 20);

/// `NVC46F_SEM_EXECUTE`: OPERATION_ACQ_CIRC_GEQ (2:0 = 3). Circular GEQ
/// matches [`crate::scheme::syncobj`]'s wrapping `fence_landed` compare, so a
/// payload that wrapped past `u32::MAX` still unblocks the acquire. Strict
/// GEQ (`0x2`) would hang across wrap. PAYLOAD_SIZE_32BIT, no WFI/timestamp.
pub(super) const NVC46F_SEM_EXECUTE_ACQUIRE: u32 = 0x3;

/// Build the 6-dword host semaphore RELEASE stream (`sem_va` <- `payload`).
#[inline]
pub(super) fn sem_release_stream(sem_va: u64, payload: u32) -> [u32; 6] {
    [
        push_hdr(0, NVC46F_SEM_ADDR_LO, 5),
        sem_va as u32,
        ((sem_va >> 32) as u32) & 0xff,
        payload,
        0,
        NVC46F_SEM_EXECUTE_RELEASE,
    ]
}

/// Build the 6-dword host semaphore ACQUIRE stream (wait until `*sem_va`
/// circularly >= `payload`). Same layout as [`sem_release_stream`].
#[inline]
pub(super) fn sem_acquire_stream(sem_gpu_va: u64, payload: u32) -> [u32; 6] {
    [
        push_hdr(0, NVC46F_SEM_ADDR_LO, 5),
        sem_gpu_va as u32,
        ((sem_gpu_va >> 32) as u32) & 0xff,
        payload,
        0,
        NVC46F_SEM_EXECUTE_ACQUIRE,
    ]
}

/// Prove the encoders above agree with the values the C side computed with
/// the SDK's DRF macros for the same inputs. Returns the first mismatch as
/// `(what, ours, theirs)`.
pub(super) fn check_encodings(
    f: &nvidia_rm_sys::rm_init::ExecFast,
) -> Result<(), (&'static str, u32, u32)> {
    use nvidia_rm_sys::rm_init::{EXEC_FAST_CHK_LEN, EXEC_FAST_CHK_VA};
    let stream = sem_release_stream(EXEC_FAST_CHK_VA, 0);
    let checks = [
        ("GP_ENTRY0", gp_entry0(EXEC_FAST_CHK_VA), f.chk_gp_entry0),
        (
            "GP_ENTRY1",
            gp_entry1(EXEC_FAST_CHK_VA, EXEC_FAST_CHK_LEN),
            f.chk_gp_entry1,
        ),
        ("SEM header", stream[0], f.chk_sem_hdr),
        ("SEM_ADDR_HI", stream[2], f.chk_sem_addr_hi),
        ("SEM_EXECUTE", stream[5], f.chk_sem_execute),
        ("USERD GPGet offset", 0x88, f.userd_gpget_off),
        ("USERD GPPut offset", 0x8c, f.userd_gpput_off),
    ];
    for (what, ours, theirs) in checks {
        if ours != theirs {
            return Err((what, ours, theirs));
        }
    }
    Ok(())
}

// --- Profile counters for /proc/gpudbg -----------------------------------------

pub(super) struct IoctlStat {
    pub count: AtomicU64,
    pub total_us: AtomicU64,
    pub max_us: AtomicU64,
}

impl IoctlStat {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }
    fn add(&self, us: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }
}

const PROFILE_SLOTS: usize = 128;
static IOCTL_PROFILE: [IoctlStat; PROFILE_SLOTS] = [const { IoctlStat::new() }; PROFILE_SLOTS];

/// Account one driver ioctl (`nr` = full DRM NR, e.g. `NR_EXEC`) that took
/// `us` microseconds in the kernel.
pub(super) fn profile_ioctl(nr: u32, us: u64) {
    IOCTL_PROFILE[(nr as usize).wrapping_sub(DRM_COMMAND_BASE as usize) & (PROFILE_SLOTS - 1)]
        .add(us);
}

pub(super) static EXEC_FAST_SUBMITS: AtomicU64 = AtomicU64::new(0);
pub(super) static EXEC_FAST_FENCED: AtomicU64 = AtomicU64::new(0);
pub(super) static EXEC_LEGACY_SUBMITS: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent inside EXEC waiting for a ring slot (direct path).
pub(super) static EXEC_RING_WAIT_US: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent inside EXEC waiting for `wait` syncobjs on the CPU.
pub(super) static EXEC_WAIT_US: AtomicU64 = AtomicU64::new(0);
/// Microseconds the legacy path spent polling its fence inline.
pub(super) static EXEC_LEGACY_FENCE_US: AtomicU64 = AtomicU64::new(0);

pub(super) fn format_exec_profile() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = writeln!(
        s,
        "[gpudbg] --- nouveau ioctl profile (cumulative since boot; diff two reads) ---"
    );
    let _ = writeln!(
        s,
        "[gpudbg]  EXEC direct-submit: {} (path {}), submits={} fenced={} ring-wait={}us cpu-wait-on-syncobjs={}us | legacy(RM) submits={} inline-fence-poll={}us",
        if exec_fast_enabled() { "ENABLED" } else { "DISABLED (nvidia.exec_rm)" },
        if exec_fast_enabled() { "GP entries + GPPut + doorbell from the kernel, fence resolved lazily" } else { "RM lookup + BAR1 map + inline fence poll per submit" },
        EXEC_FAST_SUBMITS.load(Ordering::Relaxed),
        EXEC_FAST_FENCED.load(Ordering::Relaxed),
        EXEC_RING_WAIT_US.load(Ordering::Relaxed),
        EXEC_WAIT_US.load(Ordering::Relaxed),
        EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed),
        EXEC_LEGACY_FENCE_US.load(Ordering::Relaxed),
    );
    let _ = writeln!(s, "[gpudbg]  {}", crate::scheme::syncobj::stats_line());
    let _ = writeln!(
        s,
        "[gpudbg]  ioctl            count      total_us      avg_us      max_us"
    );
    for (i, st) in IOCTL_PROFILE.iter().enumerate() {
        let n = st.count.load(Ordering::Relaxed);
        if n == 0 {
            continue;
        }
        let nr = DRM_COMMAND_BASE + i as u32;
        let total = st.total_us.load(Ordering::Relaxed);
        let _ = writeln!(
            s,
            "[gpudbg]  {:<14} {:>8} {:>13} {:>11} {:>11}",
            nouveau_ioctl_name(nr),
            n,
            total,
            total / n,
            st.max_us.load(Ordering::Relaxed)
        );
    }
    s
}

/// What an OpenGL or Vulkan client makes the *NVIDIA* side of the kernel do.
///
/// `glxgears` and `glmark2` reach this file through Mesa: nvc0 (classic Gallium
/// GL) submits through `GEM_PUSHBUF`, NVK (Vulkan, which is what `zink` and
/// `wlr_gles2` end up on here) through `VM_INIT`/`VM_BIND`/`EXEC`. None of that
/// can run on a host --- the arms live in `nvidia.rs` behind MMIO and a GSP-RM
/// handshake. What CAN run here is the part that has actually broken, twice, and
/// both times took every GL client on the machine down with it: the *request
/// numbers and struct layouts*.
///
/// A DRM ioctl number encodes `sizeof(struct)` in bits 16..29. So a Rust struct
/// whose layout drifts by one field from the real uAPI does not produce a
/// slightly-wrong result --- it produces an ioctl number Mesa never sends, and
/// the client gets `ENOSYS` from a call it believes is mandatory. That is the
/// `deadline_nsec` saga in `README-nvk-hardware-status.md`: the field was first
/// removed (fixing QEMU's old libdrm, breaking Alpine's 2.4.134 on the real
/// card) and then restored, each time diagnosed only after a client died on
/// hardware. Nothing in the tree pinned the sizes, so nothing caught either.
///
/// The expected sizes below are the real ABI, taken from libdrm's own
/// `nouveau_drm.h` and `drm.h`. They are written as literals on purpose: a
/// struct edit here is supposed to fail this test and make its author go and
/// check the header, which is exactly the step that was skipped.
#[cfg(test)]
mod gl_client_abi_tests {
    use super::*;
    use core::mem::size_of;

    /// `struct drm_nouveau_*`, byte for byte, as libdrm lays them out on
    /// x86_64. Verified against the vendored `nouveau_drm.h` for every struct
    /// that header defines; the `VM_*`/`EXEC`/`sync` group is newer than it and
    /// comes from the upstream header the new submission uAPI was added with.
    #[test]
    fn every_request_struct_matches_the_real_uapi_layout() {
        // (what it is, ours, the ABI's)
        let table: &[(&str, usize, usize)] = &[
            ("drm_nouveau_getparam", size_of::<DrmNouveauGetparam>(), 16),
            (
                "drm_nouveau_channel_alloc",
                size_of::<DrmNouveauChannelAlloc>(),
                88,
            ),
            (
                "drm_nouveau_channel_free",
                size_of::<DrmNouveauChannelFree>(),
                4,
            ),
            ("drm_nouveau_gem_info", size_of::<DrmNouveauGemInfo>(), 40),
            ("drm_nouveau_gem_new", size_of::<DrmNouveauGemNew>(), 48),
            (
                "drm_nouveau_gem_cpu_prep",
                size_of::<DrmNouveauGemCpuPrep>(),
                8,
            ),
            (
                "drm_nouveau_gem_cpu_fini",
                size_of::<DrmNouveauGemCpuFini>(),
                4,
            ),
            // The legacy submission path, which is what the image's Mesa
            // (mesa-dri-gallium, i.e. nvc0) actually uses for OpenGL.
            (
                "drm_nouveau_gem_pushbuf_bo_presumed",
                size_of::<DrmNouveauGemPushbufBoPresumed>(),
                16,
            ),
            (
                "drm_nouveau_gem_pushbuf_bo",
                size_of::<DrmNouveauGemPushbufBo>(),
                40,
            ),
            (
                "drm_nouveau_gem_pushbuf_reloc",
                size_of::<DrmNouveauGemPushbufReloc>(),
                28,
            ),
            (
                "drm_nouveau_gem_pushbuf_push",
                size_of::<DrmNouveauGemPushbufPush>(),
                24,
            ),
            (
                "drm_nouveau_gem_pushbuf",
                size_of::<DrmNouveauGemPushbuf>(),
                64,
            ),
            // The new submission uAPI, which NVK uses.
            ("drm_nouveau_vm_init", size_of::<DrmNouveauVmInit>(), 16),
            (
                "drm_nouveau_vm_bind_op",
                size_of::<DrmNouveauVmBindOp>(),
                40,
            ),
            ("drm_nouveau_vm_bind", size_of::<DrmNouveauVmBind>(), 40),
            ("drm_nouveau_sync", size_of::<DrmNouveauSync>(), 16),
            ("drm_nouveau_exec_push", size_of::<DrmNouveauExecPush>(), 16),
            ("drm_nouveau_exec", size_of::<DrmNouveauExec>(), 40),
        ];
        for &(what, ours, abi) in table {
            assert_eq!(
                ours, abi,
                "{} is {} bytes here and {} in the ABI: Mesa's ioctl number for \
                 it will not be the one we answer",
                what, ours, abi,
            );
        }
    }

    /// The command numbers themselves, against `nouveau_drm.h`'s
    /// `DRM_NOUVEAU_*` offsets plus `DRM_COMMAND_BASE`. Getting one wrong routes
    /// a client's call to another arm, which is worse than not handling it.
    #[test]
    // The table below is `DRM_COMMAND_BASE + <offset>` spelled out, so the
    // 0x40 + 0x00 row is the same shape as its neighbours on purpose.
    #[allow(clippy::identity_op)]
    fn the_command_numbers_are_the_ones_nouveau_publishes() {
        assert_eq!(DRM_COMMAND_BASE, 0x40, "DRM_COMMAND_BASE is fixed at 0x40");
        let table: &[(&str, u32, u32)] = &[
            ("GETPARAM", NR_GETPARAM, 0x40 + 0x00),
            ("CHANNEL_ALLOC", NR_CHANNEL_ALLOC, 0x40 + 0x02),
            ("CHANNEL_FREE", NR_CHANNEL_FREE, 0x40 + 0x03),
            ("NVIF", NR_NVIF, 0x40 + 0x07),
            (
                "SVM_INIT is 0x08 and SVM_BIND 0x09: not ours",
                NR_VM_INIT,
                0x40 + 0x10,
            ),
            ("VM_BIND", NR_VM_BIND, 0x40 + 0x11),
            ("EXEC", NR_EXEC, 0x40 + 0x12),
            ("GET_ZCULL_INFO", NR_GET_ZCULL_INFO, 0x40 + 0x13),
            ("GEM_NEW", NR_GEM_NEW, 0x40 + 0x40),
            ("GEM_PUSHBUF", NR_GEM_PUSHBUF, 0x40 + 0x41),
            ("GEM_CPU_PREP", NR_GEM_CPU_PREP, 0x40 + 0x42),
            ("GEM_CPU_FINI", NR_GEM_CPU_FINI, 0x40 + 0x43),
            ("GEM_INFO", NR_GEM_INFO, 0x40 + 0x44),
        ];
        let mut seen = alloc::vec::Vec::new();
        for &(what, ours, abi) in table {
            assert_eq!(ours, abi, "{} is filed under the wrong NR", what);
            assert!(
                !seen.contains(&ours),
                "NR {:#04x} is claimed twice ({})",
                ours,
                what
            );
            seen.push(ours);
            // Every one has to fit the 8-bit NR field of an ioctl number.
            assert!(ours <= 0xff, "{} does not fit the NR field", what);
        }
    }

    /// The payload floor every arm leans on before it casts. Present for each
    /// NR that is dispatched, and equal to that NR's own struct --- a floor that
    /// is too small lets a short request through and the arm then writes past
    /// the end of the caller's buffer.
    #[test]
    fn each_dispatched_command_declares_its_own_struct_as_the_floor() {
        let table: &[(u32, usize)] = &[
            (NR_GETPARAM, size_of::<DrmNouveauGetparam>()),
            (NR_CHANNEL_ALLOC, size_of::<DrmNouveauChannelAlloc>()),
            (NR_CHANNEL_FREE, size_of::<DrmNouveauChannelFree>()),
            (NR_VM_INIT, size_of::<DrmNouveauVmInit>()),
            (NR_VM_BIND, size_of::<DrmNouveauVmBind>()),
            (NR_EXEC, size_of::<DrmNouveauExec>()),
            (NR_GEM_NEW, size_of::<DrmNouveauGemNew>()),
            (NR_GEM_PUSHBUF, size_of::<DrmNouveauGemPushbuf>()),
            (NR_GEM_CPU_PREP, size_of::<DrmNouveauGemCpuPrep>()),
            (NR_GEM_CPU_FINI, size_of::<DrmNouveauGemCpuFini>()),
            (NR_GEM_INFO, size_of::<DrmNouveauGemInfo>()),
        ];
        for &(nr, want) in table {
            assert_eq!(
                min_payload_for_nr(nr),
                Some(want),
                "{} has the wrong payload floor",
                nouveau_ioctl_name(nr),
            );
        }
        // NVIF is the one exception, and deliberately: five different request
        // layouts ride one NR, so it validates its own header instead. A floor
        // here would reject four of the five.
        assert_eq!(
            min_payload_for_nr(NR_NVIF),
            None,
            "NVIF must not carry a fixed floor",
        );
        // And a number that is not ours declares nothing.
        assert_eq!(min_payload_for_nr(DRM_COMMAND_BASE + 0x7f), None);
    }

    /// Dispatch is by NR alone, so neither the direction bits nor the size a
    /// client encoded can hide an arm. This is not a style preference: Mesa
    /// issues `VM_INIT` with `drmCommandWrite` (`_IOW`) although the header says
    /// `_IOWR`, and matching the full request number made it unreachable --- and
    /// with it the whole GPU, because NVK calls it while creating the physical
    /// device.
    #[test]
    fn a_command_is_found_by_its_nr_whatever_size_and_direction_it_carries() {
        /// `_IOC(dir, 'd', nr, size)`.
        fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
            (dir << 30) | (0x64 << 8) | (nr & 0xff) | (((size as u32) & 0x3fff) << 16)
        }
        const WRITE: u32 = 1;
        const READ_WRITE: u32 = 3;

        for &(dir, size) in &[
            (WRITE, size_of::<DrmNouveauVmInit>()), // what Mesa really sends
            (READ_WRITE, size_of::<DrmNouveauVmInit>()), // what the header says
            (READ_WRITE, size_of::<DrmNouveauVmInit>() + 8), // a grown struct
            (WRITE, size_of::<DrmNouveauVmInit>() + 64),
        ] {
            let (got_dir, nr, got_size) = decode_ioc(ioc(dir, NR_VM_INIT, size));
            assert_eq!(nr, NR_VM_INIT, "VM_INIT stopped being reachable");
            assert_eq!(got_dir, dir);
            assert_eq!(got_size as usize, size);
        }

        // And the five NVIF layouts, which is the case that forced this design.
        for size in [16usize, 24, 32, 40, 48] {
            let (_, nr, _) = decode_ioc(ioc(READ_WRITE, NR_NVIF, size));
            assert_eq!(nr, NR_NVIF, "NVIF at {} bytes lost its arm", size);
        }
    }

    /// The tile-kind boundary. Turing's UNCOMPRESSED kind set is `0x00..=0x06`
    /// (PITCH, the Z/S family Z16/S8/S8Z24/ZF32_X24S8/Z24S8, and
    /// GENERIC_MEMORY, per tu102's `dev_mmu.h`): all of those go into the PTEs
    /// verbatim and need no compression tags. `0x07` is INVALID and
    /// `0x08..=0x0f` are the COMPRESSIBLE kinds, which this driver cannot
    /// program because it has no comptag allocator.
    ///
    /// Where the line is drawn matters in both directions. Too strict and a
    /// depth surface NVK is entitled to allocate gets refused; too loose and a
    /// compressed kind reaches the page tables, where the bytes are
    /// byte-inexact. And it has to be enforced at `GEM_NEW`, while NVK can
    /// still choose another kind --- refusing it at `VM_BIND` instead left the
    /// surface unmapped and hung the channel on the first draw that used it,
    /// with no MMU fault to point at it (seen on the RTX with kind `0x01`).
    #[test]
    fn the_supported_tile_kinds_are_exactly_the_uncompressed_turing_set() {
        for kind in 0x00u32..=0x06 {
            assert!(
                pte_kind_is_supported(kind),
                "tile kind {:#04x} is uncompressed on Turing and must be accepted",
                kind,
            );
        }
        assert!(
            !pte_kind_is_supported(0x07),
            "0x07 is INVALID in the Turing kind set",
        );
        for kind in 0x08u32..=0xff {
            assert!(
                !pte_kind_is_supported(kind),
                "tile kind {:#04x} needs comptags, which this driver has none of",
                kind,
            );
        }

        // The kind is the low byte of a VM_BIND op's flags, so the op bits
        // above it must not leak into the decision.
        assert_eq!(vm_bind_pte_kind(PTE_KIND_GENERIC), PTE_KIND_GENERIC);
        assert_eq!(
            vm_bind_pte_kind(VM_BIND_SPARSE | PTE_KIND_GENERIC),
            PTE_KIND_GENERIC,
            "the SPARSE bit must not change the kind",
        );
        assert_eq!(vm_bind_pte_kind(0xABCD_EF00 | 0x06), PTE_KIND_GENERIC);
        assert_eq!(vm_bind_pte_kind(0x0000_0001), 0x01);
        assert_eq!(
            vm_bind_pte_kind(0xFFFF_FF00),
            PTE_KIND_PITCH,
            "no kind bits set is plain linear, whatever else is on",
        );
    }
}

/// The direct-submit path: the GPFIFO ring accounting and the four method
/// encoders that put work on the channel.
///
/// None of this runs in CI and none of it can: it only executes against a real
/// NVIDIA channel, and the emulated GPU in QEMU is virtio. On hardware the
/// encoders are cross-checked once at bring-up against the values the C side
/// computes with the SDK's own DRF macros (`check_encodings`), which is a good
/// check and a late one -- it needs the card. These pin the same values where
/// they can be read without one.
#[cfg(test)]
mod direct_submit_tests {
    use super::*;

    /// A ring of no entries is refused rather than divided by. `GPPut`,
    /// `GPGet` and the ring size all come from outside this kernel, and the
    /// wrap arithmetic used to take the size on faith: an `EXEC` on a channel
    /// the RM prepared without a GPFIFO divided by zero, which is a kernel
    /// panic reachable by any process holding the device open.
    #[test]
    fn a_ring_of_no_entries_is_refused_instead_of_divided_by() {
        assert!(ring_state(0, 0, 0, 1).is_none());
        assert!(ring_state(7, 3, 0, 0).is_none());
        // And every other size answers something.
        for entries in [1u32, 2, 8, 128, 4096, u32::MAX] {
            assert!(
                ring_state(0, 0, entries, 1).is_some(),
                "{} entries",
                entries
            );
        }
    }

    /// The pointers are taken modulo the ring, because the RM's own self-test
    /// submissions move `GPPut` on channel 0 behind this path's back and the
    /// value read back can be past the end.
    #[test]
    fn the_pointers_are_wrapped_into_the_ring() {
        let st = ring_state(4096 + 5, 4096 * 3 + 9, 4096, 1).expect("a real ring");
        assert_eq!(st.put, 5);
        assert_eq!(st.get, 9);
    }

    /// Free space, counted the way the hardware does: everything between
    /// `GPPut` and `GPGet` going forward is in flight, and the count wraps.
    #[test]
    fn the_ring_counts_the_entries_in_flight_across_the_wrap() {
        const N: u32 = 8;
        // Empty: put == get. Seven of the eight entries can be filled.
        assert!(ring_state(0, 0, N, 7).expect("empty").room);
        assert!(!ring_state(0, 0, N, 8).expect("empty").room);
        // Three in flight, so four more fit and five do not.
        assert!(ring_state(3, 0, N, 4).expect("three used").room);
        assert!(!ring_state(3, 0, N, 5).expect("three used").room);
        // The same three, with the pair straddling the end of the ring.
        assert!(ring_state(1, 6, N, 4).expect("wrapped").room);
        assert!(
            !ring_state(1, 6, N, 5).expect("wrapped").room,
            "the count did not wrap with the ring"
        );
    }

    /// One entry always stays unused. With every entry in play, a full ring
    /// and an empty one both read as `GPPut == GPGet`, and the host would take
    /// the full one for empty and let the CPU overwrite entries the GPU has
    /// not fetched.
    #[test]
    fn the_last_entry_stays_unused_so_full_is_not_empty() {
        for entries in [2u32, 8, 128, 4096] {
            let all = ring_state(0, 0, entries, entries).expect("a real ring");
            assert!(!all.room, "a ring of {} took all of its entries", entries);
            let all_but_one = ring_state(0, 0, entries, entries - 1).expect("a real ring");
            assert!(
                all_but_one.room,
                "a ring of {} lost a usable entry",
                entries
            );
        }
        // A one-entry ring can therefore never hold a submission, which is the
        // honest answer and not a panic.
        assert!(!ring_state(0, 0, 1, 1).expect("a one-entry ring").room);
    }

    /// A nonsense ring size cannot make a full ring look empty. `put +
    /// entries` overflows a `u32` well before the size stops being
    /// representable, and the overflow would answer "room" for a ring that has
    /// none.
    #[test]
    fn a_nonsense_ring_size_cannot_overflow_the_accounting() {
        let huge = ring_state(u32::MAX - 1, 0, u32::MAX, 4).expect("a huge ring");
        assert_eq!(huge.put, u32::MAX - 1);
        assert!(
            !huge.room,
            "an overflow reported room on a ring with one entry left"
        );
        // And a `needed` no submission could ever have is refused, not wrapped.
        assert!(!ring_state(0, 0, 4096, u32::MAX).expect("a real ring").room);
    }

    /// `NV906F_GP_ENTRY0` carries GET in bits 31:2 of the push VA's low half.
    /// The low two bits are not address: a push buffer is dword-aligned and
    /// they are the entry's own flags.
    #[test]
    fn the_first_gp_entry_word_is_the_low_half_of_the_address() {
        assert_eq!(gp_entry0(0x0000_0000_1234_5678), 0x1234_5678);
        assert_eq!(
            gp_entry0(0x0000_00ff_1234_5678),
            0x1234_5678,
            "the high byte leaked in"
        );
        assert_eq!(
            gp_entry0(0x1234_5679),
            0x1234_5678,
            "the low bits are not address"
        );
        assert_eq!(gp_entry0(0x1234_567f), 0x1234_567c);
    }

    /// `NV906F_GP_ENTRY1`: GET_HI in 7:0, LENGTH in dwords in 30:10.
    #[test]
    fn the_second_gp_entry_word_is_the_high_byte_and_the_length_in_dwords() {
        // Six dwords of semaphore stream at a 40-bit address.
        let w = gp_entry1(0x0000_00ab_1234_5678, 24);
        assert_eq!(w & 0xff, 0xab, "GET_HI is not the VA's byte above 32");
        assert_eq!((w >> 10) & 0x1f_ffff, 6, "LENGTH is in dwords, not bytes");
        assert_eq!(w & 0x300, 0, "bits 9:8 belong to LEVEL and must stay clear");
        // Only the low byte of the VA's high half is address: the whole word
        // is compared, because masking the answer the same way the encoder
        // does would hide a leak into LENGTH.
        assert_eq!(
            gp_entry1(0x0000_ff00_0000_0000, 4),
            1 << 10,
            "the bits above GET_HI leaked into the entry"
        );
        // A zero-length push encodes as zero length, not as a wrap.
        assert_eq!(gp_entry1(0, 0) >> 10, 0);
    }

    /// `NV906F_DMA` INC_METHOD header: SEC_OP in 31:29, COUNT in 28:16,
    /// SUBCHANNEL in 15:13, and the method's BYTE address shifted down to the
    /// dword address the host wants in 11:0.
    #[test]
    fn the_push_header_carries_the_method_the_count_and_the_subchannel() {
        let h = push_hdr(0, NVC46F_SEM_ADDR_LO, 5);
        assert_eq!(h >> 29, 1, "SEC_OP must be INC_METHOD");
        assert_eq!((h >> 16) & 0x1fff, 5, "the method count is wrong");
        assert_eq!((h >> 13) & 0x7, 0, "the subchannel is wrong");
        assert_eq!(
            h & 0xfff,
            NVC46F_SEM_ADDR_LO >> 2,
            "the method address is a dword index, not a byte offset"
        );
        // Each field stays inside its own bits.
        let full = push_hdr(0x7, 0x3ffc, 0x1fff);
        assert_eq!((full >> 13) & 0x7, 0x7);
        assert_eq!((full >> 16) & 0x1fff, 0x1fff);
        assert_eq!(full >> 29, 1, "a full count must not run into SEC_OP");
    }

    /// The host semaphore streams: five consecutive methods from `SEM_ADDR_LO`
    /// -- address low, address high, payload low, payload high, execute.
    #[test]
    fn the_semaphore_streams_write_the_address_the_payload_and_the_operation() {
        const VA: u64 = 0x0000_007f_dead_b000;
        let rel = sem_release_stream(VA, 0x4142_4344);
        assert_eq!(rel[0], push_hdr(0, NVC46F_SEM_ADDR_LO, 5));
        assert_eq!(rel[1], 0xdead_b000, "SEM_ADDR_LO");
        assert_eq!(rel[2], 0x7f, "SEM_ADDR_HI is one byte of address");
        assert_eq!(rel[3], 0x4142_4344, "SEM_PAYLOAD_LO");
        assert_eq!(rel[4], 0, "SEM_PAYLOAD_HI: the payload is 32 bits");
        assert_eq!(rel[5], NVC46F_SEM_EXECUTE_RELEASE);

        // SEM_ADDR_HI is one byte wide. A GPU VA is 49 bits here, so the bits
        // above the address's byte 4 are not address and must not ride along.
        assert_eq!(
            sem_release_stream(0x0000_abcd_1234_5000, 1)[2],
            0xcd,
            "the bits above SEM_ADDR_HI leaked into the method"
        );

        // A release is a completion fence, not a "the host fetched it" mark:
        // without RELEASE_WFI_EN the payload lands while the engines are still
        // running, and every NVK syncobj resolves against it.
        assert_eq!(rel[5] & 0x7, 1, "the operation must be RELEASE");
        assert_ne!(
            rel[5] & (1 << 20),
            0,
            "RELEASE_WFI_EN is what makes it a fence"
        );

        let acq = sem_acquire_stream(VA, 7);
        // Same layout, so the only difference is the operation.
        assert_eq!(acq[..5], [rel[0], rel[1], rel[2], 7, 0]);
        assert_eq!(acq[5] & 0x7, 3, "ACQUIRE must be the CIRCULAR GEQ flavour");
        // Strict GEQ (2) hangs once a payload wraps past u32::MAX, which the
        // syncobj layer's own compare tolerates.
        assert_ne!(acq[5] & 0x7, 2);
        assert_eq!(acq[5] & (1 << 20), 0, "an acquire must not wait for idle");
    }

    /// Every ioctl this driver dispatches gets its own profile slot, and the
    /// slot maps back to the name the report prints. The two sides are written
    /// apart -- one hashes the NR, the other adds the slot index back to
    /// `DRM_COMMAND_BASE` -- so a collision would silently add two ioctls'
    /// times together under one name.
    #[test]
    fn every_dispatched_ioctl_has_a_profile_slot_of_its_own() {
        let nrs = [
            NR_GETPARAM,
            NR_CHANNEL_ALLOC,
            NR_CHANNEL_FREE,
            NR_NVIF,
            NR_VM_INIT,
            NR_VM_BIND,
            NR_EXEC,
            NR_GET_ZCULL_INFO,
            NR_GEM_NEW,
            NR_GEM_PUSHBUF,
            NR_GEM_CPU_PREP,
            NR_GEM_CPU_FINI,
            NR_GEM_INFO,
        ];
        let slot =
            |nr: u32| (nr as usize).wrapping_sub(DRM_COMMAND_BASE as usize) & (PROFILE_SLOTS - 1);
        for (i, &a) in nrs.iter().enumerate() {
            for &b in &nrs[i + 1..] {
                assert_ne!(
                    slot(a),
                    slot(b),
                    "{} and {} share a profile slot",
                    nouveau_ioctl_name(a),
                    nouveau_ioctl_name(b)
                );
            }
            // And the report's own inverse lands back on the same command.
            let printed = DRM_COMMAND_BASE + slot(a) as u32;
            assert_eq!(
                nouveau_ioctl_name(printed),
                nouveau_ioctl_name(a),
                "slot {} prints as the wrong ioctl",
                slot(a)
            );
        }
    }
}
