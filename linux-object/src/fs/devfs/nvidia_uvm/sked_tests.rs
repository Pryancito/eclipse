//! UVM_MAP_DYNAMIC_PARALLELISM_REGION: a SKED reflected range, video memory
//! the RM maps with the kind of a message to SKED.

use super::mapping_tests::*;
use super::tests::*;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// A region where libcuda's GPU VA space has nothing, and the 2 MiB page
/// that is the largest a Turing GPU maps.
const REGION: u64 = 0x2_0060_0000;
const PAGE_2M: u64 = 0x20_0000;

impl Process {
    /// UVM_MAP_DYNAMIC_PARALLELISM_REGION of `[base, base + length)` on
    /// `uuid`.
    fn map_sked(&self, base: u64, length: u64, uuid: &Uuid) -> u32 {
        let mut params = vec![0u8; GPU_RANGE_SIZE];
        params[..8].copy_from_slice(&base.to_le_bytes());
        params[8..16].copy_from_slice(&length.to_le_bytes());
        params[GPU_RANGE_GPU..][..16].copy_from_slice(uuid);
        self.call(cmd::MAP_DYNAMIC_PARALLELISM_REGION, &mut params)
    }

    /// The SKED reflected ranges the RM has mapped and not freed.
    fn skeds(&self) -> Vec<(By, nvctl::SkedRange)> {
        self.rm
            .state()
            .skeds
            .values()
            .map(|&(by, range, _)| (by, range))
            .collect()
    }

    /// Who freed what, in order, by handle.
    fn freed(&self) -> Vec<u32> {
        self.rm
            .state()
            .frees
            .iter()
            .map(|&(_, _, h_object)| h_object)
            .collect()
    }
}

/// A process with a GPU VA space on GPU0 only.
fn on_gpu0() -> Process {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    process
}

/// The range reaches the RM as the `/dev/nvidiactl` open's, in the GPU VA
/// space's client, device and VA space, of the GPU named and no other, and
/// is then a range UVM_VALIDATE_VA_RANGE knows.
#[test]
fn a_sked_range_is_mapped_on_the_gpu_named() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU1), NV_OK);
    let expected = nvctl::SkedRange {
        h_client: CLIENT,
        h_device: DEVICE1,
        h_va_space: VA_SPACE1,
        base: REGION,
        length: PAGE_2M,
    };
    assert_eq!(process.skeds(), [(process.by(), expected)]);
    assert!(process.rm.mapped().is_empty(), "nothing else is mapped");
    assert_eq!(process.validate(REGION, PAGE_2M), NV_OK);
    assert_eq!(
        process.validate(REGION, PAGE_2M - 0x1000),
        NV_ERR_INVALID_ADDRESS
    );
}

/// Each page size of a Turing GPU's MMU, in either canonical half.
#[test]
fn a_sked_range_is_one_page_of_a_size_the_gpu_maps() {
    let process = on_gpu0();
    for (base, length) in [
        (0x1000_0000, 0x1000),
        (0x1001_0000, 0x1_0000),
        (0x1020_0000, PAGE_2M),
        (0x7fff_ffe0_0000, PAGE_2M),
        (0xffff_8000_0000_0000, 0x1000),
    ] {
        assert_eq!(
            process.map_sked(base, length, &GPU0),
            NV_OK,
            "{:#x} +{:#x}",
            base,
            length
        );
    }
    assert_eq!(process.skeds().len(), 5);
}

/// `uvm_map_sked_reflected_range`'s refusals, in its order, and none of
/// them reaches the RM.
#[test]
fn a_sked_range_is_refused_as_the_module_refuses_it() {
    let process = on_gpu0();
    let unknown: Uuid = [0x77; 16];
    for (base, length, uuid, status) in [
        // Not whole 4 KiB pages: before the GPU is looked at.
        (0, PAGE_2M, GPU1, NV_ERR_INVALID_ADDRESS),
        (REGION + 0x800, 0x1000, GPU0, NV_ERR_INVALID_ADDRESS),
        (REGION, 0, GPU0, NV_ERR_INVALID_ADDRESS),
        (REGION, 0x1800, GPU0, NV_ERR_INVALID_ADDRESS),
        (u64::MAX - 0xfff, 0x2000, GPU0, NV_ERR_INVALID_ADDRESS),
        // A GPU with no GPU VA space here, or not registered at all, before
        // the range is looked at further.
        (REGION, 0x3000, GPU1, NV_ERR_INVALID_DEVICE),
        (REGION, PAGE_2M, unknown, NV_ERR_INVALID_DEVICE),
        // Outside both canonical halves, whatever its size.
        (0x7fff_ffff_f000, 0x2000, GPU0, NV_ERR_OUT_OF_RANGE),
        (0x8000_0000_0000, 0x3000, GPU0, NV_ERR_OUT_OF_RANGE),
        (0xffff_7fff_ffff_f000, 0x1000, GPU0, NV_ERR_OUT_OF_RANGE),
        // Not exactly one page of a size the MMU maps.
        (REGION, 0x3000, GPU0, NV_ERR_INVALID_ADDRESS),
        (REGION, 0x2000, GPU0, NV_ERR_INVALID_ADDRESS),
        (REGION, 0x40_0000, GPU0, NV_ERR_INVALID_ADDRESS),
        (0x2000_0000, 0x2000_0000, GPU0, NV_ERR_INVALID_ADDRESS),
        (REGION + 0x1000, 0x1_0000, GPU0, NV_ERR_INVALID_ADDRESS),
        (REGION + 0x1_0000, PAGE_2M, GPU0, NV_ERR_INVALID_ADDRESS),
    ] {
        assert_eq!(
            process.map_sked(base, length, &uuid),
            status,
            "{:#x} +{:#x}",
            base,
            length
        );
    }
    assert!(process.skeds().is_empty());
    assert_eq!(process.validate(REGION, PAGE_2M), NV_ERR_INVALID_ADDRESS);
}

/// No range overlaps another, of whatever kind, and an mmap over one is in
/// use.
#[test]
fn a_sked_range_overlaps_no_other_range() {
    let process = on_gpu0();
    assert_eq!(process.create_external_range(REGION, 0x1000), NV_OK);
    assert_eq!(
        process.map_sked(REGION, PAGE_2M, &GPU0),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(process.free(REGION, 0x1000), NV_OK);
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU0), NV_OK);
    assert_eq!(
        process.map_sked(REGION, PAGE_2M, &GPU0),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(
        process.create_external_range(REGION + PAGE_2M - 0x1000, 0x2000),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(
        process
            .uvm
            .get_vmo(REGION as usize + 0x1000, 0x1000)
            .map(|_| ()),
        Err(LxError::EADDRINUSE)
    );
    assert_eq!(process.skeds().len(), 1);
    // Right after it is free.
    assert_eq!(
        process.create_external_range(REGION + PAGE_2M, 0x1000),
        NV_OK
    );
}

/// UVM_FREE takes only the whole range, and gives back the mapping and then
/// the memory behind it.
#[test]
fn free_gives_back_a_sked_range_and_its_memory() {
    let process = on_gpu0();
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU0), NV_OK);
    let (&h_virtual, &(_, _, h_memory)) = process.rm.state().skeds.iter().next().unwrap();
    assert_eq!(
        process.free(REGION, PAGE_2M - 0x1000),
        NV_ERR_INVALID_ADDRESS
    );
    assert_eq!(process.skeds().len(), 1);
    assert_eq!(process.free(REGION, PAGE_2M), NV_OK);
    assert_eq!(process.freed(), [h_virtual, h_memory]);
    assert!(process.skeds().is_empty());
    assert_eq!(process.validate(REGION, PAGE_2M), NV_ERR_INVALID_ADDRESS);
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU0), NV_OK);
}

/// The GPU VA space takes the mapping with it and the range stays, mapped
/// nowhere, until UVM_FREE, which then frees nothing more.
#[test]
fn a_gpu_va_space_takes_the_mapping_and_leaves_the_range() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU0), NV_OK);
    assert_eq!(process.unregister_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.skeds().len(), 1, "another GPU's goes alone");
    assert_eq!(process.unregister_gpu_va_space(&GPU0), NV_OK);
    assert!(process.skeds().is_empty());
    assert_eq!(process.freed().len(), 2);
    assert_eq!(process.validate(REGION, PAGE_2M), NV_OK);
    assert_eq!(
        process.map_sked(REGION, PAGE_2M, &GPU1),
        NV_ERR_INVALID_DEVICE
    );
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(
        process.map_sked(REGION, PAGE_2M, &GPU0),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(process.free(REGION, PAGE_2M), NV_OK);
    assert_eq!(process.freed().len(), 2);
}

/// A range the RM refuses is not made: its status goes back, and the range
/// can be asked for again.
#[test]
fn a_sked_range_the_rm_refuses_is_not_made() {
    let process = on_gpu0();
    process.rm.state().refuse_sked.insert(VA_SPACE0, 0x1f);
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU0), 0x1f);
    assert_eq!(process.validate(REGION, PAGE_2M), NV_ERR_INVALID_ADDRESS);
    process.rm.state().refuse_sked.clear();
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU0), NV_OK);
}

/// Once the `/dev/nvidiactl` open is closed there is no client to map in.
#[test]
fn a_closed_ctl_open_has_no_client_for_a_sked_range() {
    let mut process = on_gpu0();
    process.ctl = None;
    assert_eq!(
        process.map_sked(REGION, PAGE_2M, &GPU0),
        nvctl::NV_ERR_INVALID_CLIENT
    );
    assert_eq!(process.validate(REGION, PAGE_2M), NV_ERR_INVALID_ADDRESS);
}

/// Through the ioctl, command 65 (`UVM_IOCTL_BASE(65)`) takes uvm_ioctl.h's
/// params: 40 bytes, the GPU's UUID at 16 and the status at 32, which is
/// all that is written back.
#[test]
fn the_ioctl_takes_the_modules_params() {
    assert_eq!(layout(65), Some((40, 32)));
    let process = on_gpu0();
    let mut params = vec![0xa5u8; 40];
    params[..8].copy_from_slice(&REGION.to_le_bytes());
    params[8..16].copy_from_slice(&PAGE_2M.to_le_bytes());
    params[16..32].copy_from_slice(&GPU0);
    let sent = params.clone();
    let at = params.as_mut_ptr() as usize;
    assert_eq!(process.uvm.ioctl(65, at, &plain()), Ok(0));
    assert_eq!(status_at(&params, 32), NV_OK);
    assert_eq!(params[..32], sent[..32]);
    assert_eq!(params[36..], sent[36..]);
    assert_eq!(process.skeds().len(), 1);
}

/// Closing the VA space unmaps the range, as the close.
#[test]
fn closing_the_va_space_unmaps_the_sked_range() {
    let process = on_gpu0();
    assert_eq!(process.map_sked(REGION, PAGE_2M, &GPU0), NV_OK);
    let os_info = process.os_info();
    let Process { rm, uvm, ctl, .. } = process;
    drop(uvm);
    assert!(rm.state().skeds.is_empty());
    let nobody = By {
        os_info,
        admin: false,
        gpu_mask: 0,
    };
    assert!(rm.state().frees.iter().all(|&(by, _, _)| by == nobody));
    assert_eq!(rm.state().frees.len(), 2);
    drop(ctl);
}
