//! DRM (Direct Rendering Manager) Scheme for drivers
//!
//! This trait allows drivers to implement DRM/KMS functionality.

use super::Scheme;
use alloc::string::String;
use alloc::vec::Vec;

/// Result of [`DrmScheme::compute_launch`]: a structured stand-in for the
/// `/proc/gpustep23` / `/proc/gpubench` text so userspace can drive compute
/// through a DRM ioctl instead of `cat`.
#[derive(Clone)]
pub struct ComputeLaunchResult {
    /// 0 = success; positive = NV_STATUS; negative = negated errno.
    pub status: i32,
    /// Wall time of the GPU work, when the op reports one (bench).
    pub elapsed_ns: u64,
    /// Threads in the dispatched grid (32 for SAXPY, chip-scale for bench).
    pub grid_threads: u32,
    /// Human-readable report (same narration as the corresponding `/proc` node).
    pub report: String,
}

impl ComputeLaunchResult {
    pub fn unsupported() -> Self {
        Self {
            status: -38, // -ENOSYS
            elapsed_ns: 0,
            grid_threads: 0,
            report: String::from("compute launch not supported on this driver"),
        }
    }
}

/// [`DrmScheme::compute_launch`] op: GPU identity + RM readiness (no dispatch).
pub const COMPUTE_OP_INFO: u32 = 0;
/// Integer SAXPY (`gpustep23`).
pub const COMPUTE_OP_SAXPY: u32 = 1;
/// Integer-ALU GIOPS benchmark (`gpubench`).
pub const COMPUTE_OP_BENCH: u32 = 2;

/// DRM Device capabilities
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmCaps {
    pub has_3d: bool,
    pub has_cursor: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// GEM (Graphics Execution Manager) handle
#[derive(Debug, Clone, Copy)]
pub struct GemHandle {
    pub id: u32,
    pub size: usize,
    pub phys_addr: u64,
}

/// Where a GEM object's pages live, from the point of view of a caller
/// deciding whether a CPU present can read them.
///
/// `GEM_NEW` allocates either sysmem/GART or VRAM, and the difference is not
/// cosmetic: a VRAM-only object has no host physical address at all. The RM
/// refuses `gem_map_cpu` for it, because `memdescGetPhysAddr(AT_CPU)` of an
/// `ADDR_FBMEM` object is a VRAM offset and not a BAR1 address, so there is
/// nothing for the CPU present path to read. Only the display engine, which
/// scans that offset out itself, can serve such a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GemAperture {
    /// Host RAM (GART). Has a host physical address; every present path works.
    Sysmem,
    /// Device memory. No host physical address; only a hardware flip serves it.
    Vram,
    /// The driver does not know this handle, or does not track apertures.
    Unknown,
}

/// DRM Connector (output)
#[derive(Debug, Clone, Copy)]
pub struct DrmConnector {
    pub id: u32,
    pub connected: bool,
    pub mm_width: u32,
    pub mm_height: u32,
    /// DRM_MODE_CONNECTOR_* (0 = Unknown). Synthetic/fallback connectors
    /// keep the historical hardcoded 11.
    pub connector_type: u32,
}

/// DRM CRTC (display controller)
#[derive(Debug, Clone, Copy)]
pub struct DrmCrtc {
    pub id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
}

/// DRM Plane (Overlay, Primary, or Cursor)
#[derive(Debug, Clone, Copy)]
pub struct DrmPlane {
    pub id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub plane_type: u32, // 1=Primary, 2=Cursor, 0=Overlay
}

/// Abstract trait for DRM Driver implementations
pub trait DrmScheme: Scheme {
    fn get_caps(&self) -> DrmCaps;

    /// This device's PCI location as `(domain, bus, device, function)`.
    ///
    /// `linux-object` needs it to make the sysfs identity of `/dev/dri/card0`
    /// and `renderD128` describe the SAME GPU that actually serves their
    /// ioctls. Matching must be done on these NUMBERS -- a driver's display
    /// name renders the bus in decimal while sysfs paths use hex, so string
    /// comparison silently mismatches. Default: unknown (not PCI, or the
    /// driver does not track it).
    fn pci_bdf(&self) -> Option<(u32, u8, u8, u8)> {
        None
    }

    /// Whether this driver actually implements the nouveau-compatible
    /// driver-private ioctl surface (`GETPARAM`, `NVIF`, `CHANNEL_ALLOC`,
    /// `GEM_NEW`, `VM_INIT`/`VM_BIND`, `EXEC`).
    ///
    /// The `nvidia.nouveau_uapi` cmdline flag is a *request*; this is the
    /// *capability*. Honouring the request with no capable driver registered
    /// would make `/dev/dri/*` answer `DRM_IOCTL_VERSION` with the name
    /// `"nouveau"` on a machine that has no NVIDIA GPU at all -- Mesa would
    /// then load NVK and aim the whole nouveau uAPI at whatever DRM driver
    /// happens to be primary (virtio-gpu under QEMU), and `DRM_CAP_SYNCOBJ`
    /// would start advertising a syncobj implementation on that node too.
    /// Default: false, so only a driver that opts in can turn the flag live.
    fn nouveau_uapi_capable(&self) -> bool {
        false
    }

    /// Whether this GPU can serve the nouveau uAPI **right now**:
    /// [`DrmScheme::nouveau_uapi_capable`] is the code, this is the hardware
    /// behind it. On the NVIDIA driver it is "the RM is attached", which is
    /// what `CHANNEL_ALLOC` and `EXEC` need; without it they answer `ENODEV`
    /// and NVK reports a lost device.
    ///
    /// This is what decides whether a SECOND card is offered to Mesa as a
    /// nouveau device at all (see `linux-object`'s `node_driver_id`). The
    /// console GPU boots cold on purpose -- its GSP resume can wedge the bus
    /// while the console renders through its BAR1 -- so until
    /// `nvidia.console_gpu` brings it up there is nothing to offer, and a node
    /// that enumerated as nouveau and then died on the first `EXEC` would be
    /// worse than no node. Default: false, so a driver has to opt in.
    fn nouveau_uapi_ready(&self) -> bool {
        false
    }

    /// Whether this GPU is scanning out the boot console. Such a GPU is
    /// deliberately excluded from the automatic RM bring-up on the BOOT path
    /// (its GSP resume can wedge the bus while the console renders through its
    /// BAR1), so by default it cannot serve the RM-backed nouveau paths. It
    /// can be brought up later, once the desktop is up and scanout is paused,
    /// via `nvidia.console_gpu` — see `deferred_console_bringup`. Default:
    /// false, which is also the right answer for non-NVIDIA drivers.
    fn is_console_gpu(&self) -> bool {
        false
    }

    /// Whether this GPU is a compute device: GSP-RM is (or will be) auto-booted
    /// and it can serve SAXPY / NVK / CE-present. Default: not a compute GPU.
    /// On dual NVIDIA boxes this is every GPU that does **not** drive GOP.
    fn is_compute_gpu(&self) -> bool {
        false
    }

    /// One-line role dump for `/proc/gpuroles`. Empty for non-NVIDIA drivers.
    fn gpu_role_line(&self) -> String {
        String::new()
    }

    /// Userspace compute launch (DRM ioctl `ECLIPSE_COMPUTE`, also `/proc`
    /// gpustep23/gpubench). `op` is [`COMPUTE_OP_INFO`] / [`COMPUTE_OP_SAXPY`] /
    /// [`COMPUTE_OP_BENCH`]. Default: not supported.
    fn compute_launch(&self, _op: u32) -> ComputeLaunchResult {
        ComputeLaunchResult::unsupported()
    }

    /// Whether this driver can own legacy-KMS scanout/presentation for dumb
    /// framebuffers instead of falling back to software KMS blits.
    fn has_hardware_kms(&self) -> bool {
        false
    }

    /// Whether this driver delivers real hardware vblank interrupts.
    /// Stub default: false — software-KMS pacing stays on the synthetic
    /// timer. NVIDIA (and other HW KMS paths) are not wired yet.
    fn has_hw_vblank(&self) -> bool {
        false
    }

    /// Close a GEM handle this driver allocated OUTSIDE the generic
    /// `CREATE_DUMB`/PRIME handle table -- `linux-object`'s own
    /// `drm::gem_close` only knows about that table. A driver with its own
    /// GEM_NEW-equivalent (currently: `NvidiaGpu`'s nouveau-uAPI, see
    /// `drivers/src/display/nouveau_uapi.rs`) keeps that bookkeeping to
    /// itself, so the generic `DRM_IOCTL_GEM_CLOSE` dispatcher falls back
    /// to this when `drm::gem_close` reports the handle unknown. Returns
    /// whether `handle` was actually one of this driver's own. Default:
    /// nothing to close (most drivers only ever use the generic table).
    /// `owner_pid` is the closing process (0 = no current thread): a driver
    /// that tracks holders per pid refuses to close a buffer the caller does
    /// not hold.
    fn nouveau_gem_close(&self, _handle: u32, _owner_pid: u64) -> bool {
        false
    }

    /// The GPU fence that says every submission which may still be writing
    /// into `gem_handle` has landed, as `(kernel VA of the fence landing
    /// zone, payload)`, for a caller that is about to scan that buffer out.
    ///
    /// This is the implicit-sync side of a present. An atomic commit can hand
    /// the plane an `IN_FENCE_FD` and the sync arm waits on it, but the legacy
    /// path -- `SETCRTC` and `PAGE_FLIP`, which is what wlroots drives while
    /// `drm.atomic` is off, and so what this kernel actually runs -- carries
    /// no fence at all. Linux covers that case from the buffer's reservation
    /// object: `drm_atomic_helper_prepare_planes` picks up the fences on the
    /// fb's BO and the commit waits for them. There is no reservation object
    /// here, and nouveau's `EXEC` carries no buffer list, so nothing says
    /// *which* submissions touched this buffer; what a driver can answer is
    /// which channel its owner submits on, and whether that channel has
    /// drained. That is the same question `GEM_CPU_PREP` answers, and it is
    /// conservative in the right direction: it can wait for work that never
    /// touched the buffer, never miss work that did.
    ///
    /// `owner_pid` is the pid that created the framebuffer, used only when the
    /// driver does not know `gem_handle` -- a dumb buffer from the generic
    /// table, or a handle from another GPU's slice. Default: no fence to wait
    /// for, which leaves the present exactly as unsynchronised as before.
    fn render_fence_for_scanout(&self, _gem_handle: u32, _owner_pid: u64) -> Option<(usize, u32)> {
        None
    }

    /// The GPU fences a BLOCKING `GEM_CPU_PREP` on `gem_handle` would park
    /// on, each as `(kernel VA of the fence landing zone, payload)`, so the
    /// async syscall path can sleep on them before the synchronous ioctl arm
    /// runs.
    ///
    /// The sync arm cannot sleep -- `INode::io_control` is synchronous -- so
    /// it busy-waits, which pegs a core for however long the GPU takes and
    /// starves every other coroutine on that CPU. Mesa calls this ioctl
    /// whenever it recycles or reads back a buffer, so that is a core held
    /// per frame. Answering here lets the caller wait properly; the sync arm
    /// then finds the work already done and returns without spinning.
    ///
    /// The answer must be the SAME set the blocking arm waits for, or the
    /// sleep ends early and the spin comes back: every process whose VM maps
    /// the buffer, plus the caller's own. It is conservative in the same
    /// direction as [`Self::render_fence_for_scanout`] -- it may name work
    /// that never touched the buffer, never miss work that did -- and an
    /// empty answer simply leaves the ioctl behaving exactly as before.
    ///
    /// Never called for a `NOWAIT` prep: that one answers EBUSY instead of
    /// blocking, so nothing may sleep ahead of it.
    fn cpu_prep_fences(&self, _gem_handle: u32, _owner_pid: u64) -> Vec<(usize, u32)> {
        Vec::new()
    }

    /// The GPU work that may still be reading or writing `gem_handle`, as
    /// hardware fences a `sync_file` can carry: what
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` hands a client that is about to use
    /// the buffer and has to wait for its other users first (an X server
    /// about to copy a window, Mesa's WSI before it renders into a
    /// swapchain image again). Linux reads them off the buffer's
    /// reservation object; the nouveau `EXEC` carries no buffer list, so the
    /// same conservative answer as [`Self::render_fence_for_scanout`] and
    /// `GEM_CPU_PREP`: the idle probe of every ring whose owner holds or
    /// maps the buffer. Empty when the driver does not know the handle or
    /// nothing is queued. Default: none.
    fn queued_fences(&self, _gem_handle: u32) -> alloc::vec::Vec<crate::scheme::syncobj::HwFence> {
        alloc::vec::Vec::new()
    }

    /// Retrieve the raw 128-byte EDID for a connector, if available.
    fn get_connector_edid(&self, _id: u32) -> Option<[u8; 128]> {
        None
    }

    /// Read-only register/state dump for GPU bring-up debugging, surfaced at
    /// `/proc/gpudbg`. Default: nothing. Hardware drivers override it to read
    /// (never write) device registers post-boot — early BAR0 access can hang
    /// some GPUs, so this is only ever invoked on demand from userspace. With
    /// multiple GPUs every DRM device is dumped, each labelled by its own name.
    fn debug_dump(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Hand the driver a firmware blob read from the mounted rootfs (e.g.
    /// GSP-RM's `gsp.bin`). Drivers can't read files themselves --
    /// `drivers` sits below `linux-object`/`kernel-hal` in the dependency
    /// graph, so the blob must be pushed down from boot code that already
    /// has filesystem access, once the rootfs is mounted. Default: ignore
    /// (most drivers need no firmware).
    fn set_gsp_firmware(&self, _bytes: alloc::vec::Vec<u8>) {}

    /// Record a human-readable outcome of the boot-time firmware-load attempt
    /// (see `set_gsp_firmware`). Pushed down from boot code even when the load
    /// FAILED, so a driver whose `set_gsp_firmware` was never called can still
    /// report *why* (file not found, short read, etc.) in its bring-up output
    /// instead of a bare "no firmware". Default: ignore.
    fn set_gsp_firmware_status(&self, _status: alloc::string::String) {}

    /// Bring-up **Step 5** (`/proc/gpustep5`): the real vendored RM core's
    /// own attach path (`nvidia_rm_sys::rm_init::attach_gpu`) -- real HAL
    /// bind/attach work, not a safe register read. Originally wired into
    /// `debug_dump`, moved out after it hung real hardware on a plain
    /// `cat /proc/gpudbg`. Default: nothing.
    fn bringup_step5(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 6** (`/proc/gpustep6`): the real vendored `kgspInitRm`
    /// GSP-RM boot (VBIOS/FWSEC extraction, Booter secure boot, WPR2 setup).
    /// The deepest and riskiest step yet -- requires `bringup_step5` to have
    /// succeeded first. Default: nothing.
    fn bringup_step6(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 7** (`/proc/gpustep7`): read back the
    /// `GspStaticConfigInfo` the live GSP-RM firmware returned during
    /// `bringup_step6` (GPU name, VRAM geometry, VBIOS IDs) -- proves the
    /// CPU<->GSP RPC channel end-to-end beyond boot. Default: nothing.
    fn bringup_step7(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 8** (`/proc/gpustep8`): read-only RM API controls
    /// (GPU name, UUID, FB heap total/free) executed by the live GSP-RM's
    /// own resource server over the GSP_RM_CONTROL RPC. Default: nothing.
    fn bringup_step8(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 9** (`/proc/gpustep9`): the rest of the real
    /// RmInitAdapter sequence -- gpuStatePreInit/StateInit/StateLoad for all
    /// kernel-side engines against the live GSP. Destructive/one-shot per
    /// boot (result cached). Default: nothing.
    fn bringup_step9(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 10** (`/proc/gpustep10`): first real copy-engine data
    /// movement on the state-loaded GPU -- CE memset + copy between two VRAM
    /// buffers, verified by CPU readback through BAR2. Cached per boot.
    /// Default: nothing.
    fn bringup_step10(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 14** (`/proc/gpustep14`): the CONSOLE GPU's full
    /// bring-up chained in one shot -- attach, GSP-RM boot (with the permanent
    /// console SEC2 drain), RM controls, gpuStatePreInit/Init/Load, and the
    /// copy-engine data-movement test -- bringing the primary to the same
    /// state-loaded, CE-verified state as the secondary. Default: nothing.
    fn bringup_step14(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// CE-offloaded present: copy the compositor's dumb buffer (contiguous
    /// sysmem at `src_sysmem_pa`, `size` bytes) into this GPU's scanout
    /// framebuffer via the persistent CeUtils channel, replacing the CPU
    /// `memcpy`-over-PCIe. Returns true if the CE copy was performed (console
    /// GPU, state-loaded); false to fall back to the CPU blit. Default: false.
    fn ce_present(&self, _src_sysmem_pa: u64, _size: u64) -> bool {
        false
    }

    /// Pitched 2D CE-offloaded present: copy `line_count` rows of `row_bytes`
    /// from `src_sysmem_pa + r * src_pitch` into the console GPU's scanout FB
    /// at `dst_byte_offset + r * dst_pitch`, using a GPU copy engine over PCIe
    /// P2P — without any CPU staging repack.  Returns true if the CE 2D copy
    /// was performed; false to fall back to the repack+flat path.
    ///
    /// `dst_byte_offset` is what lets this serve a damage rectangle: the
    /// caller biases `src_sysmem_pa` into the source buffer and passes the
    /// matching offset into the scanout framebuffer, so the engine writes only
    /// the region that changed.  0 is a full-frame present.  Default: false.
    fn ce_present_2d_pitched(
        &self,
        _src_sysmem_pa: u64,
        _src_pitch: u32,
        _dst_byte_offset: u64,
        _dst_pitch: u32,
        _row_bytes: u32,
        _line_count: u32,
    ) -> bool {
        false
    }

    /// Whether this driver can offload scanout via the copy engine. True for
    /// any NVIDIA GPU that is state-loaded: a compute GPU copies into the
    /// console framebuffer over PCIe P2P, and a console GPU (brought up by
    /// `nvidia.console_gpu` or a manual `/proc/gpustep14`) copies into its own
    /// framebuffer, which needs no P2P at all. Used to auto-enable the CE
    /// present path when manual `nvidia.cepresent` is absent.
    ///
    /// This is re-read after the deferred console bring-up, not only at boot,
    /// so a console GPU that comes up late still switches present off the CPU.
    fn ce_present_ready(&self) -> bool {
        false
    }

    /// Automatic boot-time compute-GPU bring-up. Runs the proven state-load
    /// chain (attach -> GSP-RM boot -> RM API client -> gpuStateLoad, i.e. the
    /// `/proc/gpustep5;6;8;9` sequence) on every GPU that does NOT drive the
    /// boot display, so the copy-engine present path (`ce_present` over PCIe
    /// P2P) is ready before the compositor starts — no manual `cat` needed.
    /// The GPU that drives the console is SKIPPED: its GSP boot wedges at the
    /// SEC2 STARTCPU store (~1/3 of the time, hanging the machine), so it is
    /// never auto-booted. Called once, synchronously, from the kernel main
    /// after the GSP firmware is loaded and before any userspace runs, so
    /// there is no concurrent RM access. Systems with a single (console) GPU
    /// bring up nothing here and fall back to the CPU blit. General over 1, 2,
    /// 3+ GPUs. Returns a SINGLE clean status line per NVIDIA GPU -- for a
    /// compute GPU what its bring-up achieved, for the console GPU the reason
    /// it was left cold and the hatch that lifts it (empty only for
    /// non-NVIDIA drivers) -- the verbose per-step
    /// narration is discarded (still captured into `/proc/gpustep*`), and the
    /// caller suppresses the driver's own log output around this call, so the
    /// desktop console stays clean. Default: nothing.
    fn auto_bringup_compute(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Deferred console-GPU bring-up (the gpustep14 chain). Called from a
    /// background kernel task AFTER the desktop is up and scanout has been
    /// paused — never from the boot progress path, where a wedged SEC2
    /// STARTCPU store would take the machine down before anything is on
    /// screen. Returns one status line; empty for non-console / non-NVIDIA
    /// drivers, so the caller can simply loop over every registered driver.
    ///
    /// Two cmdline flags reach here: `nvidia.hwcursor`, which wants the
    /// console GPU up only so `MODE_CURSOR` can take the hardware plane, and
    /// `nvidia.console_gpu`, which wants it up so the console GPU can present
    /// its OWN framebuffer with its own copy engine. The latter is the only
    /// path to any GPU acceleration at all on a single-GPU box: with no second
    /// card there is nothing to P2P from, so a cold console GPU means the CPU
    /// blit and nothing else.
    fn deferred_console_bringup(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Build the console GPU's GR allocation ladder (step16 + step17), called
    /// right AFTER `deferred_console_bringup` and after scanout has been
    /// UNPAUSED. The ladder is what a client riding context 0 binds into, and
    /// `auto_bringup_compute` builds it for every other card; the console GPU
    /// skips that function for the SEC2 store, so it needs this.
    ///
    /// It is a separate call, outside the paused window, on purpose: the
    /// ladder drives the compute engine's cold golden-context load on the very
    /// card that scans out the screen, and doing that with the scanout parked
    /// is how a boot lost the monitor. By here the card is state-loaded, the
    /// screen is moving again, and a client that cannot bind yet simply
    /// retries -- `ensure_ctx_for_pid` drops its reservation on failure, so
    /// the next ioctl tries afresh. Returns one status line, empty when there
    /// is nothing to say (no console GPU, no RM, ladder already built).
    fn build_console_gr_ladder(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// `nvidia.nvkms`: load nvidia-modeset and give it this card's display,
    /// as `nvidia-drm.modeset=1` does in Linux. Called once per card, after
    /// the deferred console bring-up, with no DRM lock held. Returns the lines
    /// to log; empty for a card that is not NVIDIA's or is already nvkms's.
    fn nvkms_load(&self) -> alloc::vec::Vec<alloc::string::String> {
        alloc::vec::Vec::new()
    }

    /// Run nvidia-modeset's due timers (Linux runs them from a kthread).
    /// Microseconds until the next one, `u64::MAX` for none queued; `None`
    /// for a card nvkms does not hold.
    fn nvkms_service(&self) -> Option<u64> {
        None
    }

    /// Read-only dump of this card's display heads at a named point, for the
    /// one stretch of the deferred bring-up that has no narration at all.
    ///
    /// The bring-up already dumps head liveness before the GSP boot and twice
    /// inside it, and on Moebius's box every one of those reads says
    /// `ADVANCING`. The monitor dies AFTER them: his screen's last line is
    /// `bring-up terminado ... el scanout vuelve`, and nothing is logged
    /// between there and the next boot. So the three reads that would say
    /// whether the raster survived the unpause, the console redraw and the GR
    /// ladder are exactly the three that do not exist.
    ///
    /// Empty from a driver with no console GPU, so a caller can ask every
    /// registered driver and print whatever comes back. Must have no side
    /// effects: it runs on a card that is scanning out at the time.
    fn scanout_liveness(&self, _phase: &str) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Read-only: can a GL/Vulkan client use this driver's console GPU yet?
    ///
    /// `None` from a driver with no console GPU of its own, so a caller can
    /// loop over every registered driver and treat "nobody answered" as
    /// "nothing to wait for". Otherwise `(ready, one status line)`.
    ///
    /// This exists because the console GPU now comes up in the MIDDLE of a
    /// live session -- a deferred task, several seconds after userspace
    /// starts -- and nothing told userspace when. A compositor that asks for
    /// a GPU renderer gets two tries at its EGL stack and then falls back to
    /// pixman for the rest of the boot; on real hardware its second and last
    /// try failed less than a second before the GR ladder was ready, so the
    /// desktop spent the whole session on the CPU with a working card beside
    /// it. With this, the session can simply wait.
    ///
    /// It must have NO side effects: unlike `/proc/gpustep*`, which performs
    /// its step when read, this is polled in a loop while a boot waits on it.
    fn console_gpu_ready(&self) -> Option<(bool, alloc::string::String)> {
        None
    }

    /// Leave this GPU in a clean (cold) state before a system reboot. A GPU we
    /// brought up carries a live GSP-RM: WPR2 is locked in its VRAM and its
    /// SEC2/GSP falcons are running. Eclipse's `reboot` is a WARM reset -- it
    /// restarts the CPU but does NOT power-cycle the GPU -- so without this the
    /// GPU crosses the reboot still dirty, and the firmware POST that runs
    /// BEFORE the OS on the next boot chokes on it (long POST / hang / recovery).
    /// Implementations issue a PCIe Function Level Reset (or similar) to return
    /// the GPU close to cold state. Only GPUs actually brought up act; the
    /// console GPU and non-NVIDIA drivers are no-ops. Called from the reset
    /// path just before the CPU reset. Returns a short log line. Default: none.
    fn quiesce_for_reboot(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// CE-offload visual test (`/proc/gpucefill`): CE-memset the console GPU's
    /// scanout framebuffer to a solid colour via the persistent CeUtils channel,
    /// to confirm the BAR1->VRAM offset is correct before wiring the full
    /// per-frame CE present blit. Requires the console GPU to be state-loaded
    /// (`/proc/gpustep14`). Default: nothing.
    fn bringup_ce_fill_fb(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// P2P CE-offload visual test (`/proc/gpucefillp2p`): from the COMPUTE GPU,
    /// CE-memset the CONSOLE GPU's scanout framebuffer white over PCIe
    /// peer-to-peer, to confirm P2P works before relying on it for the present
    /// path — the via-A route that avoids the flaky console-GPU bring-up.
    fn bringup_ce_fill_fb_p2p(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Read back (and clear) the CMOS survival breadcrumb the previous
    /// console-GPU boot attempt left, surfaced at `/proc/gpusurvive`. On a box
    /// with no serial, this is how a SEC2-window wedge is diagnosed: the CPU
    /// hangs and nothing else survives, but the CMOS NVRAM keeps the last
    /// milestone + RM narration count across the reboot. Default: nothing.
    fn survival_report(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 15** (`/proc/gpustep15`): probe the GR (graphics/compute)
    /// engine's GPC/TPC/SM shader config on a state-loaded GPU via the live
    /// GSP-RM (GR_GET_GPC_MASK / GR_GET_TPC_MASK controls). Read-only,
    /// repeatable; groundwork toward a real compute launch. Default: nothing.
    fn bringup_step15(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 16** (`/proc/gpustep16`): GR allocation ladder on a
    /// state-loaded GPU (client/device/subdevice/VAS/TSG(GR)/ctxshare) via
    /// the vendored resource server -- front half of a compute launch.
    /// Idempotent. Default: nothing.
    fn bringup_step16(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 17** (`/proc/gpustep17`): compute channel on the
    /// step-16 ladder (USERD + GPFIFO memory + channel-in-TSG +
    /// TURING_COMPUTE_A + schedule). Idempotent. Default: nothing.
    fn bringup_step17(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// `/proc/gpuedid`: real display query via NV04_DISPLAY_COMMON — which
    /// outputs exist, which are connected, and the EDID of the first
    /// connected one. Read-only (never programs the display engine).
    /// Requires step16. Default: nothing.
    fn bringup_edid(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 18** (`/proc/gpustep18`): first Eclipse-authored
    /// submission on the step-17 channel — semaphore-release method stream,
    /// GP entry, GPPut, usermode doorbell, CPU-polled verification.
    /// Idempotent once fully successful. Default: nothing.
    fn bringup_step18(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 19** (`/proc/gpustep19`): first real compute launch —
    /// a Turing QMD pointing at a minimal SM75 kernel, submitted via
    /// SEND_PCAS on the step-17/18 channel, verified by the QMD RELEASE0
    /// semaphore. Idempotent once the semaphore lands. Default: nothing.
    fn bringup_step19(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 20** (`/proc/gpustep20`): kernel that stores a chosen
    /// value to a chosen VA (patched immediates + STG.E.SYS + EXIT), triple
    /// verified (fence, RELEASE0, CPU readback of the stored dword).
    /// Idempotent once release+store verify. Default: nothing.
    fn bringup_step20(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 21** (`/proc/gpustep21`): 32-thread kernel
    /// (out[tid] = tid*3+7) with per-thread CPU verification of all 32
    /// slots. Idempotent once release+verify pass. Default: nothing.
    fn bringup_step21(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 22** (`/proc/gpustep22`): chip-scale grid — 68 CTAs
    /// x 32 threads across all 34 SMs, out[gid] = gid*3+7, all 2176 slots
    /// CPU-verified. Idempotent once release+verify pass. Default: nothing.
    fn bringup_step22(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Bring-up **Step 23** (`/proc/gpustep23`): integer SAXPY
    /// (y[i] = a*x[i] + y[i]) with real global loads (LDG) -- the
    /// load-compute-store canon, per-element verified. Default: nothing.
    fn bringup_step23(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// GIOPS benchmark (`/proc/gpubench`): a big grid of dependent-IMAD
    /// chains timed with the GPU PTIMER. Default: nothing.
    fn bringup_bench(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Read-only hardware-state dump (`/proc/gpudump`): the discriminating
    /// registers (display head liveness, VGA workspace, PMC, BSI, sysmem
    /// flush) for this GPU, with NO GSP boot -- zero wedge risk. Lets us
    /// diff primary vs secondary and pre-decide the display experiments
    /// without spending a boot. Default: nothing.
    fn hw_dump(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// Import a buffer allocated by the kernel (DRM core)
    fn import_buffer(&self, _handle: GemHandle) -> bool {
        true
    }

    /// Free a buffer
    fn free_buffer(&self, _handle: GemHandle) {}

    /// Where `handle_id`'s pages live, for a caller that has already failed
    /// to resolve a host physical address for it.
    ///
    /// This exists so that failure can be told apart from a handle that does
    /// not exist. The two produce the same `None` out of the resolver and
    /// want opposite things from whoever is debugging: an unknown handle is a
    /// lifetime or ownership bug, a VRAM handle is a buffer the CPU present
    /// simply cannot read. Default: nothing tracked.
    ///
    /// `pid` is who asks. Handles are per file in Linux
    /// (`drm_gem_object_lookup(file_priv, handle)` fails for another file's),
    /// so an object the process may not touch is `Unknown` to it, the same
    /// as no object at all; 0 is the kernel, which may touch every object.
    fn gem_aperture(&self, _handle_id: u32, _pid: u64) -> GemAperture {
        GemAperture::Unknown
    }

    /// Create a framebuffer from a GEM handle
    fn create_fb(&self, handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32>;

    /// Page flip: atomically switch to a new framebuffer
    fn page_flip(&self, fb_id: u32) -> bool;

    /// Set hardware cursor position and/or image
    fn set_cursor(&self, crtc_id: u32, x: i32, y: i32, handle: u32, flags: u32) -> bool;

    /// REAL display-engine cursor plane: upload a premultiplied-ARGB image
    /// (w, h <= 64) and enable the plane. `true` = the hardware owns the
    /// cursor from here on; the kernel's software compositing must stand
    /// down. Default: not supported.
    fn hw_cursor_set(&self, _argb: &[u32], _w: u32, _h: u32) -> bool {
        false
    }

    /// Move the display-engine cursor plane (top-left position). Only
    /// meaningful after a successful [`Self::hw_cursor_set`]. Must be cheap:
    /// it is called on every pointer motion.
    fn hw_cursor_move(&self, _x: i32, _y: i32) -> bool {
        false
    }

    /// Hide the display-engine cursor plane. Default: nothing to hide.
    fn hw_cursor_hide(&self) -> bool {
        false
    }

    /// Wait for vertical blank on a CRTC
    fn wait_vblank(&self, crtc_id: u32) -> bool;

    /// Get driver resources (fb_ids, crtc_ids, connector_ids)
    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>);

    /// Get connector info
    fn get_connector(&self, id: u32) -> Option<DrmConnector>;

    /// Get crtc info
    fn get_crtc(&self, id: u32) -> Option<DrmCrtc>;

    /// Get plane info
    fn get_plane(&self, id: u32) -> Option<DrmPlane>;

    /// Get all planes supported by the driver
    fn get_planes(&self) -> Vec<u32>;

    /// Set plane properties
    fn set_plane(
        &self,
        plane_id: u32,
        crtc_id: u32,
        fb_id: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
    ) -> bool;

    /// Driver-specific IOCTLs
    fn ioctl(&self, _request: u32, _arg: usize) -> Result<usize, i32> {
        Err(38) // ENOSYS
    }

    /// Same as [`ioctl`](DrmScheme::ioctl), but also given the pid of the
    /// calling process. `drivers` cannot learn this itself: it has no
    /// dependency on `kernel-hal`/`zircon-object` (the process/thread
    /// types live above this crate in the dependency graph), so
    /// `linux-object`'s ioctl dispatch -- which already resolves the
    /// caller's pid for its own generic GEM-handle-table cleanup on
    /// process exit -- pushes it down here instead. Default: ignore it
    /// and delegate to `ioctl`, so every driver except one that actually
    /// needs per-process ownership (currently: `NvidiaGpu`'s nouveau-uAPI
    /// channel, for `nouveau_release_process`) is unaffected.
    fn ioctl_owned(&self, request: u32, arg: usize, _owner_pid: u64) -> Result<usize, i32> {
        self.ioctl(request, arg)
    }

    /// Same as [`ioctl_owned`](DrmScheme::ioctl_owned), but also given the
    /// DRM file the ioctl came on (the address of its `DrmFileState`, alive
    /// as long as the open): Linux keeps a nouveau VM per file, and a driver
    /// that does the same needs to tell two files of one process apart.
    /// Default: ignore it.
    fn ioctl_file(
        &self,
        request: u32,
        arg: usize,
        owner_pid: u64,
        _file: usize,
    ) -> Result<usize, i32> {
        self.ioctl_owned(request, arg, owner_pid)
    }

    /// The DRM file at `file` (as [`ioctl_file`](DrmScheme::ioctl_file)
    /// names it) closed. Default: nothing kept per file.
    fn nouveau_file_closed(&self, _file: usize) {}

    /// Release every driver-private resource (currently: the nouveau-uAPI
    /// channel and everything bound in it) owned by `pid`, e.g. because
    /// the process exited without an explicit `CHANNEL_FREE`/`GEM_CLOSE`.
    /// Counterpart to `linux-object`'s own `drm::release_process` for the
    /// generic CREATE_DUMB/PRIME table, called from the same process-exit
    /// hook. Default: nothing (most drivers have no driver-private,
    /// per-process state to release).
    fn nouveau_release_process(&self, _pid: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Mutex;
    use alloc::vec;

    /// A DRM driver that implements only the methods the trait requires, so
    /// whatever else a test reads back from it is the trait's own default.
    ///
    /// The defaults in this trait are not cosmetic: several of them decide
    /// whether a GPU is offered to Mesa as a nouveau device at all, and a
    /// `false` that turned into a `true` would aim the whole nouveau uAPI at
    /// whatever DRM driver happens to be primary (virtio-gpu under QEMU) on a
    /// machine with no NVIDIA GPU in it.
    struct BareDrm {
        /// Every `(request, arg)` the default `ioctl_owned` forwarded.
        ioctls: Mutex<Vec<(u32, usize)>>,
    }

    impl BareDrm {
        fn new() -> Self {
            Self {
                ioctls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Scheme for BareDrm {
        fn name(&self) -> &str {
            "bare-drm"
        }
    }

    impl DrmScheme for BareDrm {
        fn get_caps(&self) -> DrmCaps {
            DrmCaps {
                has_3d: false,
                has_cursor: false,
                max_width: 1920,
                max_height: 1080,
            }
        }
        fn create_fb(&self, _handle_id: u32, _w: u32, _h: u32, _pitch: u32) -> Option<u32> {
            None
        }
        fn page_flip(&self, _fb_id: u32) -> bool {
            false
        }
        fn set_cursor(&self, _crtc: u32, _x: i32, _y: i32, _handle: u32, _flags: u32) -> bool {
            false
        }
        fn wait_vblank(&self, _crtc_id: u32) -> bool {
            false
        }
        fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
            (Vec::new(), Vec::new(), Vec::new())
        }
        fn get_connector(&self, _id: u32) -> Option<DrmConnector> {
            None
        }
        fn get_crtc(&self, _id: u32) -> Option<DrmCrtc> {
            None
        }
        fn get_plane(&self, _id: u32) -> Option<DrmPlane> {
            None
        }
        fn get_planes(&self) -> Vec<u32> {
            Vec::new()
        }
        #[allow(clippy::too_many_arguments)]
        fn set_plane(
            &self,
            _plane_id: u32,
            _crtc_id: u32,
            _fb_id: u32,
            _x: i32,
            _y: i32,
            _w: u32,
            _h: u32,
            _src_x: u32,
            _src_y: u32,
            _src_w: u32,
            _src_h: u32,
        ) -> bool {
            false
        }
        fn ioctl(&self, request: u32, arg: usize) -> Result<usize, i32> {
            self.ioctls.lock().push((request, arg));
            Ok(request as usize)
        }
    }

    /// A driver that does NOT override `ioctl` either, so the trait's own
    /// `ENOSYS` is what answers.
    struct NoIoctlDrm;

    impl Scheme for NoIoctlDrm {
        fn name(&self) -> &str {
            "no-ioctl-drm"
        }
    }

    impl DrmScheme for NoIoctlDrm {
        fn get_caps(&self) -> DrmCaps {
            DrmCaps {
                has_3d: false,
                has_cursor: false,
                max_width: 0,
                max_height: 0,
            }
        }
        fn create_fb(&self, _handle_id: u32, _w: u32, _h: u32, _pitch: u32) -> Option<u32> {
            None
        }
        fn page_flip(&self, _fb_id: u32) -> bool {
            false
        }
        fn set_cursor(&self, _crtc: u32, _x: i32, _y: i32, _handle: u32, _flags: u32) -> bool {
            false
        }
        fn wait_vblank(&self, _crtc_id: u32) -> bool {
            false
        }
        fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
            (Vec::new(), Vec::new(), Vec::new())
        }
        fn get_connector(&self, _id: u32) -> Option<DrmConnector> {
            None
        }
        fn get_crtc(&self, _id: u32) -> Option<DrmCrtc> {
            None
        }
        fn get_plane(&self, _id: u32) -> Option<DrmPlane> {
            None
        }
        fn get_planes(&self) -> Vec<u32> {
            Vec::new()
        }
        #[allow(clippy::too_many_arguments)]
        fn set_plane(
            &self,
            _plane_id: u32,
            _crtc_id: u32,
            _fb_id: u32,
            _x: i32,
            _y: i32,
            _w: u32,
            _h: u32,
            _src_x: u32,
            _src_y: u32,
            _src_w: u32,
            _src_h: u32,
        ) -> bool {
            false
        }
    }

    /// `unsupported()` is what every driver that cannot launch compute work
    /// answers, and userspace reads the three fields: `-ENOSYS` as a NEGATED
    /// errno (the field is positive for NV_STATUS), nothing dispatched, and a
    /// report that says why rather than an empty string.
    #[test]
    fn an_unsupported_compute_launch_reports_negated_enosys() {
        let r = ComputeLaunchResult::unsupported();
        assert_eq!(r.status, -38);
        assert!(r.status < 0, "a negated errno must stay negative");
        assert_eq!(r.elapsed_ns, 0);
        assert_eq!(r.grid_threads, 0);
        assert!(!r.report.is_empty());
    }

    /// A driver that does not override `compute_launch` must answer the same
    /// thing for every op, including an op number that does not exist.
    #[test]
    fn a_driver_without_compute_refuses_every_op() {
        let dev = BareDrm::new();
        for op in [COMPUTE_OP_INFO, COMPUTE_OP_SAXPY, COMPUTE_OP_BENCH, 99] {
            let r = dev.compute_launch(op);
            assert_eq!(r.status, -38, "op {} was not refused", op);
            assert_eq!(r.grid_threads, 0);
        }
    }

    /// The op numbers cross the ioctl boundary, so they are ABI: they must
    /// stay at their own values and stay distinct from each other.
    #[test]
    fn the_compute_op_numbers_are_distinct_and_pinned() {
        assert_eq!(COMPUTE_OP_INFO, 0);
        assert_eq!(COMPUTE_OP_SAXPY, 1);
        assert_eq!(COMPUTE_OP_BENCH, 2);
    }

    /// Every capability predicate defaults to false. This is the whole
    /// safety model of the trait: the `nvidia.nouveau_uapi` cmdline flag is a
    /// *request*, these are the *capability*, and a driver has to opt in.
    #[test]
    fn every_capability_predicate_is_false_by_default() {
        let dev = BareDrm::new();
        assert!(!dev.nouveau_uapi_capable());
        assert!(!dev.nouveau_uapi_ready());
        assert!(!dev.is_console_gpu());
        assert!(!dev.is_compute_gpu());
        assert!(!dev.has_hardware_kms());
        assert!(!dev.has_hw_vblank());
        assert!(!dev.ce_present(0x1000, 4096));
        assert!(!dev.ce_present_2d_pitched(0x1000, 4096, 0, 4096, 4096, 1080));
        assert!(!dev.ce_present_ready());
        assert!(!dev.hw_cursor_set(&[0; 4], 2, 2));
        assert!(!dev.hw_cursor_move(10, 10));
        assert!(!dev.hw_cursor_hide());
    }

    /// A driver that does not track PCI must say it does not know, not make
    /// up a location: `linux-object` builds the sysfs identity of
    /// `/dev/dri/card0` from these NUMBERS, and a fabricated 0:0.0 would
    /// describe a different device than the one serving the ioctls.
    #[test]
    fn a_driver_that_does_not_track_pci_answers_unknown() {
        assert!(BareDrm::new().pci_bdf().is_none());
    }

    /// `gem_aperture` exists precisely to tell an unknown handle apart from a
    /// VRAM one, so a driver that tracks nothing must answer `Unknown` and
    /// never `Vram` -- the two want opposite things from whoever is reading.
    #[test]
    fn an_untracked_handle_has_an_unknown_aperture() {
        assert_eq!(BareDrm::new().gem_aperture(7, 0), GemAperture::Unknown);
    }

    /// And the three apertures must stay distinct, since the present path
    /// branches on them.
    #[test]
    fn the_three_apertures_are_distinct() {
        assert_ne!(GemAperture::Sysmem, GemAperture::Vram);
        assert_ne!(GemAperture::Sysmem, GemAperture::Unknown);
        assert_ne!(GemAperture::Vram, GemAperture::Unknown);
    }

    /// With no fence to wait for, a present is left exactly as
    /// unsynchronised as it was before -- `None` and an empty list, never a
    /// zero landing zone, which a caller would then poll at address 0.
    #[test]
    fn a_driver_with_no_fences_names_none() {
        let dev = BareDrm::new();
        assert!(dev.render_fence_for_scanout(1, 100).is_none());
        assert!(dev.cpu_prep_fences(1, 100).is_empty());
        assert!(dev.queued_fences(1).is_empty());
    }

    /// A handle this driver never allocated is not its to close: answering
    /// true would make the generic dispatcher believe a buffer it still owns
    /// was released.
    #[test]
    fn a_foreign_gem_handle_is_not_closed() {
        assert!(!BareDrm::new().nouveau_gem_close(3, 100));
    }

    /// The generic DRM core allocates the buffer and only tells the driver;
    /// a driver with nothing to do about it must ACCEPT the import (true),
    /// because a refusal fails `CREATE_DUMB` on a perfectly good buffer.
    /// Freeing it again must be a no-op and not panic.
    #[test]
    fn importing_a_core_allocated_buffer_is_accepted_and_freeing_it_is_a_no_op() {
        let dev = BareDrm::new();
        let handle = GemHandle {
            id: 1,
            size: 4096,
            phys_addr: 0x1000,
        };
        assert!(dev.import_buffer(handle));
        dev.free_buffer(handle);
    }

    /// Every text node is empty by default. A placeholder line would read as
    /// a diagnosis in `/proc/gpudbg`, and `gpu_role_line` is concatenated
    /// into `/proc/gpuroles`, where a non-NVIDIA driver must contribute
    /// nothing at all rather than a blank row.
    #[test]
    fn every_text_node_is_empty_by_default() {
        let dev = BareDrm::new();
        for (name, text) in [
            ("gpu_role_line", dev.gpu_role_line()),
            ("debug_dump", dev.debug_dump()),
            ("hw_dump", dev.hw_dump()),
            ("auto_bringup_compute", dev.auto_bringup_compute()),
            ("deferred_console_bringup", dev.deferred_console_bringup()),
            ("quiesce_for_reboot", dev.quiesce_for_reboot()),
            ("survival_report", dev.survival_report()),
            ("bringup_ce_fill_fb", dev.bringup_ce_fill_fb()),
            ("bringup_ce_fill_fb_p2p", dev.bringup_ce_fill_fb_p2p()),
            ("bringup_edid", dev.bringup_edid()),
            ("bringup_bench", dev.bringup_bench()),
        ] {
            assert!(text.is_empty(), "{} is not empty: {:?}", name, text);
        }
    }

    /// Same for the whole bring-up ladder. Each step is its own `/proc` node
    /// and each one of them is a hardware operation, so a driver that has
    /// none must say nothing for all of them -- a step left reporting
    /// something would be read as a step that ran.
    #[test]
    fn the_whole_bringup_ladder_is_silent_by_default() {
        let dev = BareDrm::new();
        let steps: Vec<(u32, String)> = vec![
            (5, dev.bringup_step5()),
            (6, dev.bringup_step6()),
            (7, dev.bringup_step7()),
            (8, dev.bringup_step8()),
            (9, dev.bringup_step9()),
            (10, dev.bringup_step10()),
            (14, dev.bringup_step14()),
            (15, dev.bringup_step15()),
            (16, dev.bringup_step16()),
            (17, dev.bringup_step17()),
            (18, dev.bringup_step18()),
            (19, dev.bringup_step19()),
            (20, dev.bringup_step20()),
            (21, dev.bringup_step21()),
            (22, dev.bringup_step22()),
            (23, dev.bringup_step23()),
        ];
        // Every rung that still has a node (5 to 10, 14 to 23) is present and
        // silent. A rung missing from this list is a rung nobody checked.
        assert_eq!(steps.len(), 16);
        for (n, text) in steps {
            assert!(text.is_empty(), "step{} is not silent: {:?}", n, text);
        }
    }

    /// Pushing firmware (or the reason it could not be loaded) down to a
    /// driver that needs none must be ignored silently: boot code pushes it
    /// to EVERY driver, non-NVIDIA ones included.
    #[test]
    fn pushing_firmware_at_a_driver_that_needs_none_is_ignored() {
        let dev = BareDrm::new();
        dev.set_gsp_firmware(vec![0u8; 16]);
        dev.set_gsp_firmware_status(String::from("gsp.bin: not found"));
    }

    /// A driver with no private ioctls answers `ENOSYS` as a POSITIVE errno
    /// (this surface returns `Result<usize, i32>`, not a negated errno), so
    /// Mesa sees an unimplemented ioctl rather than a driver error.
    #[test]
    fn a_driver_without_private_ioctls_answers_enosys() {
        assert_eq!(NoIoctlDrm.ioctl(0x40, 0), Err(38));
        assert_eq!(NoIoctlDrm.ioctl_owned(0x40, 0, 100), Err(38));
    }

    /// The default `ioctl_owned` is a pass-through: it must forward BOTH the
    /// request and the argument unchanged and drop only the pid. A swapped
    /// pair here would hand a driver another ioctl's argument.
    #[test]
    fn the_owned_ioctl_forwards_request_and_argument_unchanged() {
        let dev = BareDrm::new();
        assert_eq!(dev.ioctl_owned(0xc0, 0xdead_beef, 4242), Ok(0xc0));
        assert_eq!(&*dev.ioctls.lock(), &[(0xc0, 0xdead_beef)]);
    }

    /// Process exit calls this on every DRM driver, so a driver with no
    /// per-process state must tolerate it -- including for a pid it never saw.
    #[test]
    fn releasing_a_process_a_driver_never_saw_is_a_no_op() {
        BareDrm::new().nouveau_release_process(9999);
    }

    /// `get_connector_edid` is `None` and not a zeroed array. The difference
    /// matters because the caller's only question is "is there EDID data":
    /// `None` says there is none, while a zeroed block says there is, and
    /// `display::edid::block_valid` then has to be the thing that rejects it
    /// (it does, on the mandatory `00 ff ff ff ff ff ff 00` header -- the
    /// checksum alone would NOT catch it, since 128 zero bytes sum to zero).
    /// A driver that answered zeros would be relying on that check instead of
    /// saying what it knows.
    #[test]
    fn a_connector_with_no_edid_answers_none_rather_than_zeros() {
        assert!(BareDrm::new().get_connector_edid(0).is_none());
    }

    /// The descriptor structs cross the ioctl boundary by value; they must
    /// stay `Copy` and keep their fields through a copy.
    ///
    /// The assignments below would compile as *moves* if `Copy` were dropped,
    /// so each type is named to a function that demands `Copy`: that is what
    /// actually fails the build if the derive goes away.
    #[test]
    fn the_descriptor_structs_survive_a_copy() {
        fn requires_copy<T: Copy>() {}
        requires_copy::<DrmCaps>();
        requires_copy::<DrmConnector>();
        requires_copy::<DrmCrtc>();
        requires_copy::<DrmPlane>();

        let caps = BareDrm::new().get_caps();
        let copy = caps;
        // And the original is still usable, which a move would forbid.
        assert_eq!(caps.max_width, copy.max_width);
        assert_eq!((copy.max_width, copy.max_height), (1920, 1080));
        assert!(!copy.has_3d && !copy.has_cursor);

        let conn = DrmConnector {
            id: 1,
            connected: true,
            mm_width: 530,
            mm_height: 300,
            connector_type: 11,
        };
        let conn_copy = conn;
        assert!(conn_copy.connected);
        assert_eq!(conn_copy.connector_type, 11);

        let crtc = DrmCrtc {
            id: 2,
            fb_id: 3,
            x: 0,
            y: 0,
        };
        assert_eq!((crtc.id, crtc.fb_id), (2, 3));

        let plane = DrmPlane {
            id: 4,
            crtc_id: 2,
            fb_id: 3,
            possible_crtcs: 0b11,
            plane_type: 1,
        };
        assert_eq!(plane.plane_type, 1, "1 is the primary plane");
        assert_eq!(plane.possible_crtcs.count_ones(), 2);
    }
}
