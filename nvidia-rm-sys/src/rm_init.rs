//! Safe Rust entry points for `vendor/eclipse_rm_init.c` -- Eclipse's own
//! equivalent of what NVIDIA's real Linux platform layer does in
//! arch/nvalloc/unix/src/osinit.c (osRmInitRm / osInitNvMapping /
//! RmInitAdapter) to bring up the real, portable RM core: construct the
//! OBJSYS singleton and resource server, then attach a GPU by real PCI
//! location and BAR info. See that file's own header comment for the
//! full real-vs-ours breakdown and the one known gap (REGISTER_ALL_HALS).
use crate::types::*;

extern "C" {
    fn eclipse_rm_init_core() -> NV_STATUS;

    fn eclipse_rm_attach_gpu(
        domain: NvU32,
        bus: NvU8,
        device: NvU8,
        bar0_phys: NvU64,
        bar0_virt: *mut c_void,
        bar0_len: NvU64,
        bar1_phys: NvU64,
        bar1_len: NvU64,
        bar2_phys: NvU64,
        bar2_len: NvU64,
        out_device_instance: *mut NvU32,
    ) -> NV_STATUS;

    fn eclipse_rm_init_gsp(device_instance: NvU32, buf: *const c_void, size: NvU32) -> NV_STATUS;
}

/// Constructs the real OBJSYS singleton and the RM resource server.
/// Call exactly once, before the first `attach_gpu`.
pub fn init_core() -> NV_STATUS {
    unsafe { eclipse_rm_init_core() }
}

/// Attaches a GPU to RM by its real PCI location and BAR0/BAR1/BAR2
/// physical/virtual addresses, mirroring what NVIDIA's own
/// `osInitNvMapping` packages into a `GPUATTACHARG`. `bar0_virt` must
/// already be mapped (Eclipse maps BAR0 during PCI probe); BAR1 (FB) and
/// BAR2 (IMEM) are passed as physical addresses only, same as the real
/// driver (`fbBaseAddr`/`instBaseAddr = NULL // not mapped`). BAR2 becomes
/// `GPUATTACHARG.instPhysAddr`/`instLength`, required by the BAR2 MMU
/// self-test in `gpuStateInit` (osinit.c:708).
///
/// Returns the real RM device instance on success.
#[allow(clippy::too_many_arguments)]
pub fn attach_gpu(
    domain: u32,
    bus: u8,
    device: u8,
    bar0_phys: u64,
    bar0_virt: *mut c_void,
    bar0_len: u64,
    bar1_phys: u64,
    bar1_len: u64,
    bar2_phys: u64,
    bar2_len: u64,
) -> Result<u32, NV_STATUS> {
    let mut device_instance: NvU32 = 0;
    let status = unsafe {
        eclipse_rm_attach_gpu(
            domain,
            bus,
            device,
            bar0_phys,
            bar0_virt,
            bar0_len,
            bar1_phys,
            bar1_len,
            bar2_phys,
            bar2_len,
            &mut device_instance,
        )
    };
    if status == NV_OK {
        Ok(device_instance)
    } else {
        Err(status)
    }
}

/// Boots GSP-RM on an already-attached GPU via the real, vendored
/// `kgspInitRm`, given the raw bytes of NVIDIA's `gsp.bin` (the one
/// firmware blob genuinely external to the open-sourced RM core --
/// everything else `kgspInitRm` needs, it self-derives from `buf` or
/// from bindata already compiled into this crate). `device_instance` is
/// the value returned by a prior successful `attach_gpu`.
pub fn init_gsp(device_instance: u32, buf: &[u8]) -> Result<(), NV_STATUS> {
    let status = unsafe {
        eclipse_rm_init_gsp(
            device_instance,
            buf.as_ptr() as *const c_void,
            buf.len() as NvU32,
        )
    };
    if status == NV_OK {
        Ok(())
    } else {
        Err(status)
    }
}

/// Fixed-layout mirror of `EclipseGspInfo` (vendor/eclipse_rm_init.c): the
/// subset of `GspStaticConfigInfo` that the live GSP-RM returned during
/// `kgspInitRm`'s GET_GSP_STATIC_INFO RPC. All-zero fields mean the RPC has
/// not run yet (gpustep6 not completed on this GPU).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GspInfo {
    pub gpu_name: [u8; 64],
    pub gpu_short_name: [u8; 64],
    pub fb_length: NvU64,
    pub fb_bus_width: NvU32,
    pub fb_ram_type: NvU32,
    pub l2_cache_size: NvU32,
    pub vbios_valid: u8,
    pub vbios_sub_vendor: NvU32,
    pub vbios_sub_device: NvU32,
}

extern "C" {
    fn eclipse_rm_get_gsp_info(device_instance: NvU32, info: *mut GspInfo) -> NV_STATUS;
}

/// Reads back the firmware-provided static config for an attached GPU.
pub fn get_gsp_info(device_instance: u32) -> Result<GspInfo, NV_STATUS> {
    let mut info = GspInfo {
        gpu_name: [0; 64],
        gpu_short_name: [0; 64],
        fb_length: 0,
        fb_bus_width: 0,
        fb_ram_type: 0,
        l2_cache_size: 0,
        vbios_valid: 0,
        vbios_sub_vendor: 0,
        vbios_sub_device: 0,
    };
    let status = unsafe { eclipse_rm_get_gsp_info(device_instance, &mut info) };
    if status == NV_OK {
        Ok(info)
    } else {
        Err(status)
    }
}

/// Fixed-layout mirror of `EclipseRmApiDemo` (vendor/eclipse_rm_init.c):
/// results of three read-only RM API controls executed by the live GSP-RM's
/// own resource server via `rpcRmApiControl_GSP` (the GSP_RM_CONTROL RPC).
/// Each control carries its own NV_STATUS so partial success is visible.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RmApiDemo {
    pub name_status: NV_STATUS,
    pub name: [u8; 64],
    pub gid_status: NV_STATUS,
    pub gid_length: NvU32,
    pub gid: [u8; 136],
    pub fb_status: NV_STATUS,
    pub heap_size_kb: NvU32,
    pub heap_free_kb: NvU32,
    pub bus_width: NvU32,
}

extern "C" {
    fn eclipse_rm_step8(device_instance: NvU32, out: *mut RmApiDemo) -> NV_STATUS;
}

/// Runs the step-8 RM-API-control demo against the live GSP.
pub fn rm_api_demo(device_instance: u32) -> Result<RmApiDemo, NV_STATUS> {
    let mut out = RmApiDemo {
        name_status: 0,
        name: [0; 64],
        gid_status: 0,
        gid_length: 0,
        gid: [0; 136],
        fb_status: 0,
        heap_size_kb: 0,
        heap_free_kb: 0,
        bus_width: 0,
    };
    let status = unsafe { eclipse_rm_step8(device_instance, &mut out) };
    if status == NV_OK {
        Ok(out)
    } else {
        Err(status)
    }
}

/// Fixed-layout mirror of `EclipseGrProbe` (vendor/eclipse_rm_init.c): the
/// graphics/compute (GR) engine's shader config as reported by the live GSP-RM
/// via the GR_GET_GPC_MASK / GR_GET_TPC_MASK controls. Turing packs TWO SMs
/// per TPC (Volta+ layout), so the usable SM count is `2 * total_tpc`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GrProbe {
    pub gpc_mask_status: NV_STATUS,
    pub gpc_mask: NvU32,
    pub num_gpc: NvU32,
    pub tpc_mask_status: NV_STATUS,
    pub total_tpc: NvU32,
    pub per_gpc_tpc: [NvU32; 8],
}

extern "C" {
    fn eclipse_rm_step15(device_instance: NvU32, out: *mut GrProbe) -> NV_STATUS;
}

/// Probes the GR (graphics/compute) engine's GPC/TPC/SM config on a
/// state-loaded GPU, over the live GSP resource server.
pub fn step15(device_instance: u32) -> Result<GrProbe, NV_STATUS> {
    let mut out = GrProbe {
        gpc_mask_status: 0,
        gpc_mask: 0,
        num_gpc: 0,
        tpc_mask_status: 0,
        total_tpc: 0,
        per_gpc_tpc: [0; 8],
    };
    let status = unsafe { eclipse_rm_step15(device_instance, &mut out) };
    if status == NV_OK {
        Ok(out)
    } else {
        Err(status)
    }
}

/// One row of the GSP-reported interrupt kernel table (mirror of
/// `EclipseIntrTableEntry`): which engine (MC_ENGINE_IDX_*) owns which
/// stall/nonstall vector in the Turing+ CPU_INTR tree, plus its legacy PMC
/// mask. `0xFFFFFFFF` vectors mean not-applicable.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntrTableEntry {
    pub engine_idx: NvU32,
    pub pmc_intr_mask: NvU32,
    pub vector_stall: NvU32,
    pub vector_non_stall: NvU32,
}

/// Mirror of `EclipseIntrTable`: the live GSP-RM's authoritative
/// vector->engine interrupt map (NV2080_CTRL_CMD_INTERNAL_INTR_GET_KERNEL_TABLE,
/// the same control kernel RM uses to build its own interrupt table).
#[repr(C)]
pub struct IntrTable {
    pub ctrl_status: NV_STATUS,
    pub table_len: NvU32,
    pub entries: [IntrTableEntry; 128],
}

extern "C" {
    fn eclipse_rm_intr_table(device_instance: NvU32, out: *mut IntrTable) -> NV_STATUS;
}

/// Mirror of `EclipseGrAlloc` (vendor/eclipse_rm_init.c): per-stage NV_STATUS
/// (`0xFFFFFFFF` = not reached) and the allocated handles of the GR
/// allocation ladder: client -> device -> subdevice -> VA space -> TSG
/// (GRAPHICS engine) -> context share. Handles stay alive C-side for step17.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GrAlloc {
    pub client_status: NvU32,
    pub device_status: NvU32,
    pub subdev_status: NvU32,
    pub vas_status: NvU32,
    pub tsg_status: NvU32,
    pub ctxshare_status: NvU32,
    pub h_client: NvU32,
    pub h_device: NvU32,
    pub h_subdevice: NvU32,
    pub h_vas: NvU32,
    pub h_tsg: NvU32,
    pub h_ctxshare: NvU32,
}

extern "C" {
    fn eclipse_rm_step16(device_instance: NvU32, out: *mut GrAlloc) -> NV_STATUS;
}

/// Runs the step-16 GR allocation ladder on a state-loaded GPU (idempotent:
/// repeat calls return the cached, still-alive allocation).
pub fn step16(device_instance: u32) -> Result<GrAlloc, NV_STATUS> {
    let mut out = GrAlloc {
        client_status: 0xFFFF_FFFF,
        device_status: 0xFFFF_FFFF,
        subdev_status: 0xFFFF_FFFF,
        vas_status: 0xFFFF_FFFF,
        tsg_status: 0xFFFF_FFFF,
        ctxshare_status: 0xFFFF_FFFF,
        h_client: 0,
        h_device: 0,
        h_subdevice: 0,
        h_vas: 0,
        h_tsg: 0,
        h_ctxshare: 0,
    };
    let status = unsafe { eclipse_rm_step16(device_instance, &mut out) };
    if status == NV_OK {
        Ok(out)
    } else {
        Err(status)
    }
}

/// Mirror of `EclipseGrChannel` (vendor/eclipse_rm_init.c): per-stage
/// NV_STATUS (`0xFFFFFFFF` = not reached) and handles for the step-17
/// compute-channel bring-up on the step-16 ladder.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GrChannel {
    pub userd_status: NvU32,
    pub buf_status: NvU32,
    pub virt_status: NvU32,
    pub map_status: NvU32,
    pub notif_status: NvU32,
    pub chan_status: NvU32,
    pub compute_status: NvU32,
    pub sched_status: NvU32,
    pub h_userd: NvU32,
    pub h_phys_buf: NvU32,
    pub h_virt_buf: NvU32,
    pub h_notifier: NvU32,
    pub h_channel: NvU32,
    pub h_compute: NvU32,
    pub channel_class: NvU32,
    pub userd_size: NvU32,
    pub buf_gpu_va: u64,
}

extern "C" {
    fn eclipse_rm_step17(device_instance: NvU32, out: *mut GrChannel) -> NV_STATUS;
}

/// Runs step-17 (USERD + buffers + GPFIFO channel + TURING_COMPUTE_A +
/// schedule) on the cached step-16 ladder. Idempotent.
pub fn step17(device_instance: u32) -> Result<GrChannel, NV_STATUS> {
    let mut out = GrChannel {
        userd_status: 0xFFFF_FFFF,
        buf_status: 0xFFFF_FFFF,
        virt_status: 0xFFFF_FFFF,
        map_status: 0xFFFF_FFFF,
        notif_status: 0xFFFF_FFFF,
        chan_status: 0xFFFF_FFFF,
        compute_status: 0xFFFF_FFFF,
        sched_status: 0xFFFF_FFFF,
        h_userd: 0,
        h_phys_buf: 0,
        h_virt_buf: 0,
        h_notifier: 0,
        h_channel: 0,
        h_compute: 0,
        channel_class: 0,
        userd_size: 0,
        buf_gpu_va: 0,
    };
    let status = unsafe { eclipse_rm_step17(device_instance, &mut out) };
    if status == NV_OK {
        Ok(out)
    } else {
        Err(status)
    }
}

/// Fetches the GSP-reported interrupt kernel table (boxed: ~2 KiB).
pub fn intr_table(device_instance: u32) -> Result<alloc::boxed::Box<IntrTable>, NV_STATUS> {
    let mut out = alloc::boxed::Box::new(IntrTable {
        ctrl_status: 0,
        table_len: 0,
        entries: [IntrTableEntry {
            engine_idx: 0,
            pmc_intr_mask: 0,
            vector_stall: 0,
            vector_non_stall: 0,
        }; 128],
    });
    let status = unsafe { eclipse_rm_intr_table(device_instance, &mut *out) };
    if status == NV_OK {
        Ok(out)
    } else {
        Err(status)
    }
}

/// Mirror of `EclipseStateInitResult` (vendor/eclipse_rm_init.c): per-phase
/// NV_STATUS of the real RmInitAdapter device bring-up. `0xFFFFFFFF` means
/// the phase was not reached (an earlier phase failed).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StateInitResult {
    pub pre_init_status: NvU32,
    pub init_status: NvU32,
    pub load_status: NvU32,
}

extern "C" {
    fn eclipse_rm_state_init(device_instance: NvU32, out: *mut StateInitResult) -> NV_STATUS;
}

/// Runs gpumgrStatePreInitGpu / StateInitGpu / StateLoadGpu on an attached,
/// GSP-booted GPU -- the rest of the real RmInitAdapter sequence.
pub fn state_init(device_instance: u32) -> Result<StateInitResult, NV_STATUS> {
    let mut out = StateInitResult {
        pre_init_status: 0xFFFF_FFFF,
        init_status: 0xFFFF_FFFF,
        load_status: 0xFFFF_FFFF,
    };
    let status = unsafe { eclipse_rm_state_init(device_instance, &mut out) };
    if status == NV_OK {
        Ok(out)
    } else {
        Err(status)
    }
}

/// Mirror of `EclipseStep10Result` (vendor/eclipse_rm_init.c): per-phase
/// NV_STATUS of the first real copy-engine data movement (CE memset A,
/// CE memset B=poison, CE copy A->B, CPU readback verify of B through BAR2)
/// on the state-loaded GPU. `0xFFFFFFFF` = phase not reached.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Step10Result {
    pub ce_utils_status: NvU32,
    pub alloc_a_status: NvU32,
    pub alloc_b_status: NvU32,
    pub poison_status: NvU32,
    pub memset_status: NvU32,
    pub copy_status: NvU32,
    pub verify_status: NvU32,
    pub buffer_size: NvU64,
    pub pa_a: NvU64,
    pub pa_b: NvU64,
    pub pattern: NvU32,
    pub poison: NvU32,
    pub dwords_checked: NvU32,
    pub mismatch_count: NvU32,
    pub first_mismatch_idx: NvU32,
    pub first_mismatch_val: NvU32,
}

extern "C" {
    fn eclipse_rm_step10(device_instance: NvU32, out: *mut Step10Result) -> NV_STATUS;

    fn eclipse_rm_mark_console_gpu(
        device_instance: NvU32,
        console_size: NvU64,
        console_at_bar1_base: u8,
    ) -> NV_STATUS;
}

/// Declares a GPU as the primary/console device to RM, NVIDIA's own way
/// (PDB_PROP_GPU_PRIMARY_DEVICE + BAR1-console preservation + reserved
/// console display memory -- what Linux's RmDeterminePrimaryDevice /
/// RmSetConsolePreservationParams do right before kgspInitRm). Must be
/// called BEFORE `init_gsp` so the SET_GUEST_SYSTEM_INFO RPC reports
/// `bIsPrimary = true` to the GSP.
pub fn mark_console_gpu(
    device_instance: u32,
    console_size: u64,
    console_at_bar1_base: bool,
) -> Result<(), NV_STATUS> {
    let status = unsafe {
        eclipse_rm_mark_console_gpu(
            device_instance,
            console_size,
            console_at_bar1_base as u8,
        )
    };
    if status == NV_OK {
        Ok(())
    } else {
        Err(status)
    }
}

/// Runs the step-10 CE memset/copy + readback-verify test against the
/// state-loaded GPU (requires a successful `state_init` first).
pub fn step10(device_instance: u32) -> Result<Step10Result, NV_STATUS> {
    let mut out = Step10Result {
        ce_utils_status: 0xFFFF_FFFF,
        alloc_a_status: 0xFFFF_FFFF,
        alloc_b_status: 0xFFFF_FFFF,
        poison_status: 0xFFFF_FFFF,
        memset_status: 0xFFFF_FFFF,
        copy_status: 0xFFFF_FFFF,
        verify_status: 0xFFFF_FFFF,
        buffer_size: 0,
        pa_a: 0,
        pa_b: 0,
        pattern: 0,
        poison: 0,
        dwords_checked: 0,
        mismatch_count: 0,
        first_mismatch_idx: 0,
        first_mismatch_val: 0,
    };
    let status = unsafe { eclipse_rm_step10(device_instance, &mut out) };
    if status == NV_OK {
        Ok(out)
    } else {
        Err(status)
    }
}
