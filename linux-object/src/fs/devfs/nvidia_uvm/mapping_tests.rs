//! What `cuCtxCreate` asks of UVM: a GPU VA space, external ranges mapped
//! through the RM, and channels.

use super::tests::*;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// The client libcuda makes, and on each GPU the device and the
/// FERMI_VASPACE_A under it it hands UVM.
const CLIENT: u32 = 0xc1d0_0002;
const DEVICE0: u32 = 0x5c00_0002;
const VA_SPACE0: u32 = 0x5c00_0007;
const DEVICE1: u32 = 0x5c00_1002;
const VA_SPACE1: u32 = 0x5c00_1007;
/// The fds of the `/dev/nvidiactl` open and of a `/dev/nvidia0` open.
const CTL_FD: i32 = 5;
const GPU_NODE_FD: i32 = 6;
const MEMORY: u32 = 0x5c00_0014;

/// UvmGpuMappingType: ReadWriteAtomic and ReadOnly.
const READ_WRITE_ATOMIC: u32 = 1;
const READ_ONLY: u32 = 3;

/// A process with a `/dev/nvidiactl` open, a VA space of `/dev/nvidia-uvm`
/// with both GPUs registered, and an RM that knows the VA space of each GPU
/// as one of that open's.
struct Process {
    rm: Arc<FakeRm>,
    uvm: Arc<UvmOpen>,
    ctl: Option<Arc<dyn INode>>,
    gpu_node: Arc<dyn INode>,
}

impl Process {
    fn new() -> Process {
        let rm = FakeRm::new();
        let uvm = uvm_on(&rm);
        let ctl: Arc<dyn INode> = Arc::new(NvidiaOpen::for_uvm_test(true));
        let gpu_node: Arc<dyn INode> = Arc::new(NvidiaOpen::for_uvm_test(false));
        let process = Process {
            rm,
            uvm,
            ctl: Some(ctl),
            gpu_node,
        };
        let os_info = process.os_info();
        let mut state = process.rm.state();
        state
            .va_spaces
            .insert((os_info, CLIENT, VA_SPACE0, 0), DEVICE0);
        state
            .va_spaces
            .insert((os_info, CLIENT, VA_SPACE1, 1), DEVICE1);
        drop(state);
        assert_eq!(initialize(&process.uvm, 0), NV_OK);
        assert_eq!(register_gpu(&process.uvm, &GPU0), NV_OK);
        assert_eq!(register_gpu(&process.uvm, &GPU1), NV_OK);
        process
    }

    /// The `os_info` of the `/dev/nvidiactl` open.
    fn os_info(&self) -> usize {
        let ctl = self.ctl.as_ref().expect("open");
        ctl.downcast_ref::<NvidiaOpen>()
            .and_then(NvidiaOpen::uvm_os_info)
            .expect("an open of /dev/nvidiactl")
    }

    /// Who the RM is told calls, as this process.
    fn by(&self) -> By {
        By {
            os_info: self.os_info(),
            admin: false,
            gpu_mask: both_gpus(),
        }
    }

    /// `cmd` on `params`, from this process: the status.
    fn call(&self, cmd: u32, params: &mut [u8]) -> u32 {
        let ctl = self.ctl.clone();
        let gpu_node = self.gpu_node.clone();
        let fds = move |fd: i32| match fd {
            CTL_FD => ctl.clone(),
            GPU_NODE_FD => Some(gpu_node.clone()),
            _ => None,
        };
        self.uvm.call(cmd, params, &with_fds(&fds));
        let (_, at) = layout(cmd).expect("served");
        status_at(params, at)
    }

    fn register_gpu_va_space_with(&self, uuid: &Uuid, fd: i32, h_va_space: u32) -> u32 {
        let mut params = vec![0u8; REGISTER_GPU_VASPACE_SIZE];
        params[..16].copy_from_slice(uuid);
        params[REGISTER_GPU_VASPACE_CTRL_FD..][..4].copy_from_slice(&fd.to_le_bytes());
        params[REGISTER_GPU_VASPACE_CLIENT..][..4].copy_from_slice(&CLIENT.to_le_bytes());
        params[REGISTER_GPU_VASPACE_VA_SPACE..][..4].copy_from_slice(&h_va_space.to_le_bytes());
        self.call(cmd::REGISTER_GPU_VASPACE, &mut params)
    }

    /// The GPU VA space libcuda made on `uuid`.
    fn register_gpu_va_space(&self, uuid: &Uuid) -> u32 {
        let h_va_space = if *uuid == GPU0 { VA_SPACE0 } else { VA_SPACE1 };
        self.register_gpu_va_space_with(uuid, CTL_FD, h_va_space)
    }

    fn unregister_gpu_va_space(&self, uuid: &Uuid) -> u32 {
        let mut params = vec![0u8; UNREGISTER_GPU_VASPACE_SIZE];
        params[..16].copy_from_slice(uuid);
        self.call(cmd::UNREGISTER_GPU_VASPACE, &mut params)
    }

    fn range_call(&self, cmd: u32, base: u64, length: u64) -> u32 {
        let mut params = vec![0u8; RANGE_SIZE];
        params[..8].copy_from_slice(&base.to_le_bytes());
        params[8..16].copy_from_slice(&length.to_le_bytes());
        self.call(cmd, &mut params)
    }

    fn create_external_range(&self, base: u64, length: u64) -> u32 {
        self.range_call(cmd::CREATE_EXTERNAL_RANGE, base, length)
    }

    fn free(&self, base: u64, length: u64) -> u32 {
        self.range_call(cmd::FREE, base, length)
    }

    /// UVM_MAP_EXTERNAL_ALLOCATION of MEMORY of CLIENT from `offset`, on
    /// each of `gpus` with its mapping type and default kind.
    fn map(&self, base: u64, length: u64, offset: u64, gpus: &[(Uuid, u32)]) -> u32 {
        self.map_of(CLIENT, base, length, offset, gpus, gpus.len() as u64)
    }

    fn map_of(
        &self,
        h_client: u32,
        base: u64,
        length: u64,
        offset: u64,
        gpus: &[(Uuid, u32)],
        count: u64,
    ) -> u32 {
        let mut params = vec![0u8; MAP_SIZE];
        params[..8].copy_from_slice(&base.to_le_bytes());
        params[8..16].copy_from_slice(&length.to_le_bytes());
        params[MAP_OFFSET..][..8].copy_from_slice(&offset.to_le_bytes());
        for (i, (uuid, mapping)) in gpus.iter().enumerate() {
            let at = MAP_ATTRIBUTES + i * ATTRIBUTES_SIZE;
            params[at..at + 16].copy_from_slice(uuid);
            params[at + 16..at + 20].copy_from_slice(&mapping.to_le_bytes());
        }
        params[MAP_COUNT..][..8].copy_from_slice(&count.to_le_bytes());
        params[MAP_CLIENT..][..4].copy_from_slice(&h_client.to_le_bytes());
        params[MAP_MEMORY..][..4].copy_from_slice(&MEMORY.to_le_bytes());
        self.call(cmd::MAP_EXTERNAL_ALLOCATION, &mut params)
    }

    fn unmap(&self, base: u64, length: u64, uuid: &Uuid) -> u32 {
        let mut params = vec![0u8; UNMAP_EXTERNAL_SIZE];
        params[..8].copy_from_slice(&base.to_le_bytes());
        params[8..16].copy_from_slice(&length.to_le_bytes());
        params[UNMAP_EXTERNAL_GPU..][..16].copy_from_slice(uuid);
        self.call(cmd::UNMAP_EXTERNAL, &mut params)
    }

    fn register_channel(&self, uuid: &Uuid, h_channel: u32) -> u32 {
        let mut params = vec![0u8; REGISTER_CHANNEL_SIZE];
        params[..16].copy_from_slice(uuid);
        params[16..20].copy_from_slice(&CTL_FD.to_le_bytes());
        params[REGISTER_CHANNEL_CLIENT..][..4].copy_from_slice(&CLIENT.to_le_bytes());
        params[REGISTER_CHANNEL_CHANNEL..][..4].copy_from_slice(&h_channel.to_le_bytes());
        self.call(cmd::REGISTER_CHANNEL, &mut params)
    }

    fn unregister_channel(&self, uuid: &Uuid, h_channel: u32) -> u32 {
        let mut params = vec![0u8; UNREGISTER_CHANNEL_SIZE];
        params[..16].copy_from_slice(uuid);
        params[UNREGISTER_CHANNEL_CLIENT..][..4].copy_from_slice(&CLIENT.to_le_bytes());
        params[UNREGISTER_CHANNEL_CHANNEL..][..4].copy_from_slice(&h_channel.to_le_bytes());
        self.call(cmd::UNREGISTER_CHANNEL, &mut params)
    }

    /// The bases of what the RM has mapped, on the VA space `h_va_space`.
    fn mapped_on(&self, h_va_space: u32) -> Vec<u64> {
        let mut bases: Vec<u64> = self
            .rm
            .mapped()
            .iter()
            .filter(|m| m.h_va_space == h_va_space)
            .map(|m| m.base)
            .collect();
        bases.sort_unstable();
        bases
    }
}

/// What libcuda's cuCtxCreate does on one GPU (traced against libcuda
/// 580.178.04 on the fake driver): UVM_REGISTER_GPU_VASPACE, then seven
/// times an external range with memory mapped in all of it, then a channel
/// with no buffers. A refusal of the first is its 801. Each mapping reaches
/// the RM as the `/dev/nvidiactl` open's, in the VA space's client and
/// device, at the address UVM was given.
#[test]
fn cuctxcreate_gets_the_answers_it_goes_on_with() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    let ranges: [(u64, u64); 7] = [
        (0x2_0000_0000, 0x20_0000),
        (0x2_0020_0000, 0x20_0000),
        (0x2_0040_0000, 0x200_0000),
        (0x2_0240_0000, 0x20_0000),
        (0x100_0000_0000, 0x200_0000),
        (0x100_0200_0000, 0x20_0000),
        (0x7f58_9a00_0000, 0x20_0000),
    ];
    for (i, &(base, length)) in ranges.iter().enumerate() {
        assert_eq!(process.create_external_range(base, length), NV_OK);
        let offset = i as u64 * 0x1000;
        assert_eq!(
            process.map(base, length, offset, &[(GPU0, READ_WRITE_ATOMIC)]),
            NV_OK
        );
    }
    assert_eq!(process.register_channel(&GPU0, 0x5c00_0030), NV_OK);
    let state = process.rm.state();
    assert_eq!(state.maps.len(), 7);
    for (&(by, m), (i, &(base, length))) in state.maps.values().zip(ranges.iter().enumerate()) {
        assert_eq!(by, process.by());
        assert_eq!(
            m,
            nvctl::ExternalMapping {
                h_client: CLIENT,
                h_device: DEVICE0,
                h_va_space: VA_SPACE0,
                h_memory: MEMORY,
                offset: i as u64 * 0x1000,
                base,
                length,
                read_only: false,
            }
        );
    }
    assert!(state.frees.is_empty());
}

/// The GPU must be registered and have no GPU VA space yet, the fd be the
/// `/dev/nvidiactl` open, and the RM find the VA space; a refusal leaves
/// nothing behind, so a good call after it still works.
#[test]
fn a_gpu_va_space_is_one_the_rm_finds_through_the_ctl_open() {
    let process = Process::new();
    assert_eq!(
        process.register_gpu_va_space_with(&GPU0, GPU_NODE_FD, VA_SPACE0),
        NV_ERR_INVALID_ARGUMENT
    );
    assert_eq!(
        process.register_gpu_va_space_with(&GPU0, 99, VA_SPACE0),
        NV_ERR_INVALID_ARGUMENT
    );
    // The RM's own answer: VA_SPACE1 is no VA space on GPU0.
    assert_eq!(
        process.register_gpu_va_space_with(&GPU0, CTL_FD, VA_SPACE1),
        0x31
    );
    let mut stranger = GPU0;
    stranger[0] ^= 1;
    assert_eq!(
        process.register_gpu_va_space_with(&stranger, CTL_FD, VA_SPACE0),
        NV_ERR_INVALID_DEVICE
    );
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    // Unregistered, it can be registered again.
    assert_eq!(process.unregister_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(
        process.unregister_gpu_va_space(&GPU0),
        NV_ERR_INVALID_DEVICE
    );
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
}

#[test]
fn a_gpu_va_space_needs_a_registered_gpu() {
    let process = Process::new();
    assert_eq!(unregister_gpu(&process.uvm, &GPU1), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_ERR_INVALID_DEVICE);
}

/// Whole 4 KiB pages, not at 0, not empty, not wrapping, and no overlap
/// with a range already here, on either side; a range just next to one is
/// fine.
#[test]
fn external_ranges_are_whole_pages_that_never_overlap() {
    let process = Process::new();
    for (base, length) in [
        (0, 0x1000),
        (0x1000, 0),
        (0x1800, 0x1000),
        (0x1000, 0x1800),
        (u64::MAX & !0xfff, 0x2000),
    ] {
        assert_eq!(
            process.create_external_range(base, length),
            NV_ERR_INVALID_ADDRESS,
            "{:#x}+{:#x}",
            base,
            length
        );
    }
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    for (base, length) in [
        (0x10_0000, 0x4000),
        (0xf_f000, 0x2000),
        (0x10_3000, 0x2000),
        (0x10_1000, 0x1000),
        (0xf_0000, 0x20000),
    ] {
        assert_eq!(
            process.create_external_range(base, length),
            NV_ERR_UVM_ADDRESS_IN_USE,
            "{:#x}+{:#x}",
            base,
            length
        );
    }
    assert_eq!(process.create_external_range(0xf_c000, 0x4000), NV_OK);
    assert_eq!(process.create_external_range(0x10_4000, 0x4000), NV_OK);
}

/// A mapping must lie in one external range, name between 1 and
/// UVM_MAX_GPUS GPUs, each with a GPU VA space, and memory of that VA
/// space's client; a refused one reaches no RM.
#[test]
fn a_mapping_is_refused_before_the_rm_when_uvm_would_refuse_it() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    assert_eq!(process.create_external_range(0x10_4000, 0x4000), NV_OK);
    let rw = [(GPU0, READ_WRITE_ATOMIC)];
    // Outside every range, and across two.
    assert_eq!(
        process.map(0x20_0000, 0x1000, 0, &rw),
        NV_ERR_INVALID_ADDRESS
    );
    assert_eq!(
        process.map(0x10_3000, 0x2000, 0, &rw),
        NV_ERR_INVALID_ADDRESS
    );
    assert_eq!(
        process.map(0x10_0800, 0x1000, 0, &rw),
        NV_ERR_INVALID_ADDRESS
    );
    // No GPU, too many, one with no GPU VA space.
    assert_eq!(
        process.map_of(CLIENT, 0x10_0000, 0x1000, 0, &rw, 0),
        NV_ERR_INVALID_ARGUMENT
    );
    assert_eq!(
        process.map_of(CLIENT, 0x10_0000, 0x1000, 0, &rw, MAX_GPUS + 1),
        NV_ERR_INVALID_ARGUMENT
    );
    assert_eq!(
        process.map(0x10_0000, 0x1000, 0, &[(GPU1, READ_WRITE_ATOMIC)]),
        NV_ERR_INVALID_DEVICE
    );
    // A mapping type the module does not know.
    assert_eq!(
        process.map(0x10_0000, 0x1000, 0, &[(GPU0, 4)]),
        NV_ERR_INVALID_ARGUMENT
    );
    // Memory of another client.
    assert_eq!(
        process.map_of(CLIENT + 1, 0x10_0000, 0x1000, 0, &rw, 1),
        NV_ERR_NOT_SUPPORTED
    );
    assert!(process.rm.mapped().is_empty());
    // The whole of one range, then a page at the very end of the other.
    assert_eq!(process.map(0x10_0000, 0x4000, 0, &rw), NV_OK);
    assert_eq!(process.map(0x10_7000, 0x1000, 0, &rw), NV_OK);
    assert_eq!(process.mapped_on(VA_SPACE0), vec![0x10_0000, 0x10_7000]);
}

/// UvmGpuMappingTypeReadOnly is the only one mapped read-only.
#[test]
fn a_read_only_mapping_is_mapped_read_only() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    for (mapping, read_only) in [(0, false), (1, false), (2, false), (READ_ONLY, true)] {
        assert_eq!(process.map(0x10_0000, 0x1000, 0, &[(GPU0, mapping)]), NV_OK);
        let mapped = process.rm.mapped();
        assert_eq!(mapped.len(), 1, "the one before was unmapped first");
        assert_eq!(mapped[0].read_only, read_only, "type {}", mapping);
    }
}

/// `uvm_map_external_allocation` unmaps what overlaps on the GPU first, and
/// only there.
#[test]
fn mapping_again_unmaps_what_was_there_on_that_gpu() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x8000), NV_OK);
    let both = [(GPU0, READ_WRITE_ATOMIC), (GPU1, READ_WRITE_ATOMIC)];
    assert_eq!(process.map(0x10_0000, 0x2000, 0, &both), NV_OK);
    assert_eq!(process.map(0x10_4000, 0x2000, 0, &both), NV_OK);
    // Overlapping the first on GPU0 only.
    assert_eq!(
        process.map(0x10_1000, 0x2000, 0, &[(GPU0, READ_WRITE_ATOMIC)]),
        NV_OK
    );
    assert_eq!(process.mapped_on(VA_SPACE0), vec![0x10_1000, 0x10_4000]);
    assert_eq!(process.mapped_on(VA_SPACE1), vec![0x10_0000, 0x10_4000]);
    let state = process.rm.state();
    assert_eq!(state.frees.len(), 1);
    assert_eq!(state.frees[0].0, process.by());
    assert_eq!(state.frees[0].1, CLIENT);
}

/// When the RM refuses one GPU, what the call mapped on the others is
/// unmapped again, and the RM's status is the answer.
#[test]
fn a_mapping_the_rm_refuses_on_one_gpu_is_undone_on_the_others() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    process.rm.state().refuse.insert(VA_SPACE1, 0x51);
    let both = [(GPU0, READ_WRITE_ATOMIC), (GPU1, READ_WRITE_ATOMIC)];
    assert_eq!(process.map(0x10_0000, 0x4000, 0, &both), 0x51);
    assert!(process.rm.mapped().is_empty());
    assert_eq!(process.rm.state().frees.len(), 1);
    process.rm.state().refuse.clear();
    assert_eq!(process.map(0x10_0000, 0x4000, 0, &both), NV_OK);
    assert_eq!(process.rm.mapped().len(), 2);
}

/// UVM_UNMAP_EXTERNAL unmaps what overlaps on the GPU named, and nothing on
/// the other; the range must be one here and the GPU registered.
#[test]
fn unmap_external_unmaps_one_gpu() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x8000), NV_OK);
    let both = [(GPU0, READ_WRITE_ATOMIC), (GPU1, READ_WRITE_ATOMIC)];
    assert_eq!(process.map(0x10_0000, 0x2000, 0, &both), NV_OK);
    assert_eq!(process.map(0x10_4000, 0x2000, 0, &both), NV_OK);
    assert_eq!(process.unmap(0x10_1000, 0x1000, &GPU1), NV_OK);
    assert_eq!(process.mapped_on(VA_SPACE0), vec![0x10_0000, 0x10_4000]);
    assert_eq!(process.mapped_on(VA_SPACE1), vec![0x10_4000]);
    // Nothing mapped there is fine, and the mappings just next to it stay;
    // outside every range is not, nor a GPU not registered.
    assert_eq!(process.unmap(0x10_2000, 0x2000, &GPU0), NV_OK);
    assert_eq!(process.mapped_on(VA_SPACE0), vec![0x10_0000, 0x10_4000]);
    assert_eq!(
        process.unmap(0x20_0000, 0x1000, &GPU1),
        NV_ERR_INVALID_ADDRESS
    );
    let mut stranger = GPU0;
    stranger[0] ^= 1;
    assert_eq!(
        process.unmap(0x10_0000, 0x1000, &stranger),
        NV_ERR_INVALID_DEVICE
    );
    assert_eq!(process.rm.state().frees.len(), 1);
}

/// UVM_FREE takes only a whole external range, with every mapping in it on
/// every GPU; the addresses can be reserved again after.
#[test]
fn free_gives_back_a_whole_range_with_its_mappings() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    assert_eq!(process.create_external_range(0x20_0000, 0x4000), NV_OK);
    let both = [(GPU0, READ_WRITE_ATOMIC), (GPU1, READ_WRITE_ATOMIC)];
    assert_eq!(process.map(0x10_0000, 0x2000, 0, &both), NV_OK);
    assert_eq!(process.map(0x10_2000, 0x2000, 0, &both), NV_OK);
    assert_eq!(process.map(0x20_0000, 0x4000, 0, &both), NV_OK);
    assert_eq!(process.free(0x10_0000, 0x2000), NV_ERR_INVALID_ADDRESS);
    assert_eq!(process.free(0x10_1000, 0x3000), NV_ERR_INVALID_ADDRESS);
    assert_eq!(process.free(0x10_0000, 0x4000), NV_OK);
    assert_eq!(process.free(0x10_0000, 0x4000), NV_ERR_INVALID_ADDRESS);
    assert_eq!(process.mapped_on(VA_SPACE0), vec![0x20_0000]);
    assert_eq!(process.mapped_on(VA_SPACE1), vec![0x20_0000]);
    assert_eq!(process.rm.state().frees.len(), 4);
    assert_eq!(process.create_external_range(0x10_0000, 0x8000), NV_OK);
}

/// Unregistering a GPU VA space, or its GPU, unmaps everything on that GPU
/// and nothing on the other; its range stays, and can be mapped once the
/// GPU VA space is back.
#[test]
fn a_gpu_va_space_takes_its_mappings_with_it() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    let both = [(GPU0, READ_WRITE_ATOMIC), (GPU1, READ_WRITE_ATOMIC)];
    assert_eq!(process.map(0x10_0000, 0x4000, 0, &both), NV_OK);
    assert_eq!(process.unregister_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.mapped_on(VA_SPACE0), Vec::<u64>::new());
    assert_eq!(process.mapped_on(VA_SPACE1), vec![0x10_0000]);
    assert_eq!(unregister_gpu(&process.uvm, &GPU1), NV_OK);
    assert!(process.rm.mapped().is_empty());
    assert_eq!(process.rm.state().frees.len(), 2);
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(
        process.map(0x10_0000, 0x4000, 0, &[(GPU0, READ_WRITE_ATOMIC)]),
        NV_OK
    );
}

/// A channel needs a GPU VA space on its GPU and is registered once; it is
/// unregistered whatever GPU is named, once, and goes with its GPU VA space.
#[test]
fn channels_are_listed_once_on_a_gpu_va_space() {
    let process = Process::new();
    assert_eq!(process.register_channel(&GPU0, 0x30), NV_ERR_INVALID_DEVICE);
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.register_channel(&GPU0, 0x30), NV_OK);
    assert_eq!(
        process.register_channel(&GPU0, 0x30),
        NV_ERR_INVALID_CHANNEL
    );
    assert_eq!(
        process.register_channel(&GPU1, 0x30),
        NV_ERR_INVALID_CHANNEL
    );
    assert_eq!(process.register_channel(&GPU1, 0x31), NV_OK);
    assert_eq!(process.unregister_channel(&GPU1, 0x30), NV_OK);
    assert_eq!(
        process.unregister_channel(&GPU0, 0x30),
        NV_ERR_INVALID_CHANNEL
    );
    assert_eq!(process.unregister_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(
        process.unregister_channel(&GPU1, 0x31),
        NV_ERR_INVALID_CHANNEL
    );
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.register_channel(&GPU1, 0x31), NV_OK);
    // Nothing of it reached the RM.
    assert!(process.rm.state().frees.is_empty());
}

/// Closing the VA space unmaps what is still mapped, as the close: nobody
/// is root and no GPU is open.
#[test]
fn closing_the_va_space_unmaps_everything() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    let both = [(GPU0, READ_WRITE_ATOMIC), (GPU1, READ_WRITE_ATOMIC)];
    assert_eq!(process.map(0x10_0000, 0x4000, 0, &both), NV_OK);
    let os_info = process.os_info();
    let Process { rm, uvm, ctl, .. } = process;
    drop(uvm);
    assert!(rm.mapped().is_empty());
    let nobody = By {
        os_info,
        admin: false,
        gpu_mask: 0,
    };
    assert!(rm.state().frees.iter().all(|&(by, _, _)| by == nobody));
    assert_eq!(rm.state().frees.len(), 2);
    drop(ctl);
}

/// Once the `/dev/nvidiactl` open is closed its clients are gone with
/// everything in them: nothing is freed in the RM, and nothing more is
/// mapped.
#[test]
fn a_closed_ctl_open_leaves_nothing_to_free() {
    let mut process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    let rw = [(GPU0, READ_WRITE_ATOMIC)];
    assert_eq!(process.map(0x10_0000, 0x2000, 0, &rw), NV_OK);
    process.ctl = None;
    assert_eq!(process.unmap(0x10_0000, 0x2000, &GPU0), NV_OK);
    assert_eq!(
        process.map(0x10_2000, 0x2000, 0, &rw),
        nvctl::NV_ERR_INVALID_CLIENT
    );
    assert_eq!(process.free(0x10_0000, 0x4000), NV_OK);
    assert!(process.rm.state().frees.is_empty());
}

/// The INIT_CHECK of the module's route: none of them before UVM_INITIALIZE.
#[test]
fn cuctxcreate_calls_need_a_va_space() {
    let rm = FakeRm::new();
    let open = uvm_on(&rm);
    for cmd in [
        cmd::REGISTER_GPU_VASPACE,
        cmd::UNREGISTER_GPU_VASPACE,
        cmd::CREATE_EXTERNAL_RANGE,
        cmd::MAP_EXTERNAL_ALLOCATION,
        cmd::UNMAP_EXTERNAL,
        cmd::FREE,
        cmd::REGISTER_CHANNEL,
        cmd::UNREGISTER_CHANNEL,
    ] {
        let (size, at) = layout(cmd).expect("served");
        let mut params = vec![0u8; size];
        open.call(cmd, &mut params, &plain());
        assert_eq!(
            status_at(&params, at),
            NV_ERR_ILLEGAL_ACTION,
            "{}",
            name(cmd)
        );
    }
}

/// An NVOS21 (or NVOS64) RM_ALLOC payload of `class`, whose alloc params
/// are at `params`.
fn alloc_payload(len: usize, class: u32, params: usize) -> Vec<u8> {
    let mut payload = vec![0u8; len];
    payload[ALLOC_CLASS..][..4].copy_from_slice(&class.to_le_bytes());
    payload[ALLOC_PARAMS..][..8].copy_from_slice(&(params as u64).to_le_bytes());
    payload
}

/// NV_VASPACE_ALLOCATION_PARAMETERS with `flags`.
fn va_space_params(flags: u32) -> Vec<u8> {
    let mut params = vec![0u8; 40];
    params[VASPACE_FLAGS..][..4].copy_from_slice(&flags.to_le_bytes());
    params
}

/// libcuda's VA space for UVM (IS_EXTERNALLY_OWNED | ENABLE_PAGE_FAULTING)
/// reaches the RM with neither, the rest of its flags kept, and the caller
/// gets its own flags back.
#[test]
fn a_va_space_for_uvm_goes_to_the_rm_as_the_rms() {
    for len in [32, 48] {
        let mut params = va_space_params(0x48 | 0x1);
        let payload = alloc_payload(len, FERMI_VASPACE_A, params.as_mut_ptr() as usize);
        let flags = strip_va_space_flags(&payload).expect("one for UVM");
        assert_eq!(u32_at(&params, VASPACE_FLAGS), 0x1);
        flags.restore();
        assert_eq!(u32_at(&params, VASPACE_FLAGS), 0x49);
    }
}

/// Any other allocation, and a VA space that is not for UVM, is left alone.
#[test]
fn other_allocations_are_left_alone() {
    let mut params = va_space_params(0x48);
    let at = params.as_mut_ptr() as usize;
    assert!(strip_va_space_flags(&alloc_payload(32, 0x50a0, at)).is_none());
    assert!(strip_va_space_flags(&alloc_payload(32, FERMI_VASPACE_A, 0)).is_none());
    assert!(strip_va_space_flags(&alloc_payload(32, FERMI_VASPACE_A, at)[..20]).is_none());
    assert_eq!(u32_at(&params, VASPACE_FLAGS), 0x48);
    let mut faulting_only = va_space_params(0x40);
    let at = faulting_only.as_mut_ptr() as usize;
    assert!(strip_va_space_flags(&alloc_payload(32, FERMI_VASPACE_A, at)).is_none());
    assert_eq!(u32_at(&faulting_only, VASPACE_FLAGS), 0x40);
}

/// Without `nvidia.uvm` nothing is changed: there is no UVM to own it.
#[test]
fn without_the_hatch_a_va_space_is_left_alone() {
    assert!(!nvctl::uvm_enabled());
    let mut params = va_space_params(0x48);
    let payload = alloc_payload(32, FERMI_VASPACE_A, params.as_mut_ptr() as usize);
    assert!(rm_owned_va_space(&payload).is_none());
    assert_eq!(u32_at(&params, VASPACE_FLAGS), 0x48);
}
