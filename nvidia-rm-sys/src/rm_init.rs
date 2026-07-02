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
        out_device_instance: *mut NvU32,
    ) -> NV_STATUS;
}

/// Constructs the real OBJSYS singleton and the RM resource server.
/// Call exactly once, before the first `attach_gpu`.
pub fn init_core() -> NV_STATUS {
    unsafe { eclipse_rm_init_core() }
}

/// Attaches a GPU to RM by its real PCI location and BAR0/BAR1
/// physical/virtual addresses, mirroring what NVIDIA's own
/// `osInitNvMapping` packages into a `GPUATTACHARG`. `bar0_virt` must
/// already be mapped (Eclipse maps BAR0 during PCI probe); BAR1 is
/// passed as a physical address only, same as the real driver
/// (`fbBaseAddr = NULL // not mapped`).
///
/// Returns the real RM device instance on success.
pub fn attach_gpu(
    domain: u32,
    bus: u8,
    device: u8,
    bar0_phys: u64,
    bar0_virt: *mut c_void,
    bar0_len: u64,
    bar1_phys: u64,
    bar1_len: u64,
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
            &mut device_instance,
        )
    };
    if status == NV_OK {
        Ok(device_instance)
    } else {
        Err(status)
    }
}
