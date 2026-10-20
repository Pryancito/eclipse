//! What `cuCtxCreate` asks of UVM once its channels are registered: a
//! semaphore pool, kernel pages that every GPU VA space and the CPU map.

use super::mapping_tests::*;
use super::tests::*;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

const POOL: u64 = 0x2_0400_0000;
const LENGTH: u64 = 0x1_0000;
/// UvmGpuCachingType: ForceCached.
const FORCE_CACHED: u32 = 2;
/// A GPU the RM knows that is not registered in the VA space.
const GPU2: Uuid = [0x30; 16];

/// UVM_ALLOC_SEMAPHORE_POOL of `length` bytes at `base`, with `count`
/// entries of `perGpuAttributes`, the first of them `attrs` as (GPU, mapping
/// type, caching type).
fn alloc_with(
    process: &Process,
    base: u64,
    length: u64,
    attrs: &[(Uuid, u32, u32)],
    count: u64,
) -> u32 {
    let mut params = vec![0u8; POOL_SIZE];
    params[..8].copy_from_slice(&base.to_le_bytes());
    params[8..16].copy_from_slice(&length.to_le_bytes());
    for (i, (uuid, mapping, caching)) in attrs.iter().enumerate() {
        let at = POOL_ATTRIBUTES + i * ATTRIBUTES_SIZE;
        params[at..at + 16].copy_from_slice(uuid);
        params[at + 16..at + 20].copy_from_slice(&mapping.to_le_bytes());
        params[at + 20..at + 24].copy_from_slice(&caching.to_le_bytes());
    }
    params[POOL_COUNT..][..8].copy_from_slice(&count.to_le_bytes());
    process.call(cmd::ALLOC_SEMAPHORE_POOL, &mut params)
}

/// The pool at POOL, with `attrs`.
fn alloc(process: &Process, attrs: &[(Uuid, u32, u32)]) -> u32 {
    alloc_with(process, POOL, LENGTH, attrs, attrs.len() as u64)
}

/// A [`Process`] with a GPU VA space on both GPUs.
fn with_both() -> Process {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    process
}

/// The devices pages are lent under and not freed, each with whether the
/// GPU caches them.
fn lent(process: &Process) -> Vec<(u32, bool)> {
    let mut lent: Vec<(u32, bool)> = process
        .rm
        .state()
        .lent
        .values()
        .map(|l| (l.h_device, l.gpu_cached))
        .collect();
    lent.sort_unstable();
    lent
}

/// The CPU's mapping of the pool at POOL.
fn cpu_map(process: &Process) -> LxResult<Arc<VmObject>> {
    process.uvm.get_vmo(POOL as usize, LENGTH as usize)
}

/// On each GPU the pool's pages become memory of the GPU VA space's
/// client, under its device, as the `/dev/nvidiactl` open's, and that
/// memory is mapped whole at the pool's address, read-write and uncached.
/// The pages are one block of zeroes, the same on both GPUs and for the
/// CPU.
#[test]
fn a_pool_is_mapped_at_its_address_on_every_gpu_va_space() {
    let process = with_both();
    assert_eq!(alloc(&process, &[]), NV_OK);
    let vmo = cpu_map(&process).unwrap();
    let phys = vmo.commit_page(0, MMUFlags::READ).unwrap() as u64;
    let mut bytes = vec![0xAAu8; LENGTH as usize];
    vmo.read(0, &mut bytes).unwrap();
    assert!(bytes.iter().all(|&b| b == 0));
    let state = process.rm.state();
    assert_eq!(state.lent.len(), 2);
    for (&h_memory, lent) in &state.lent {
        assert_eq!(lent.by, process.by());
        assert_eq!(lent.h_client, CLIENT);
        assert_eq!((lent.phys, lent.size), (phys, LENGTH));
        assert!(!lent.gpu_cached);
        let (va_space, device) = if lent.h_device == DEVICE0 {
            (VA_SPACE0, DEVICE0)
        } else {
            (VA_SPACE1, DEVICE1)
        };
        let (by, mapping) = state
            .maps
            .values()
            .find(|(_, m)| m.h_memory == h_memory)
            .copied()
            .expect("mapped");
        assert_eq!(by, process.by());
        assert_eq!(
            mapping,
            nvctl::ExternalMapping {
                h_client: CLIENT,
                h_device: device,
                h_va_space: va_space,
                h_memory,
                offset: 0,
                base: POOL,
                length: LENGTH,
                read_only: false,
            }
        );
    }
}

/// A GPU `perGpuAttributes` names maps the pool as they say; one it does
/// not name maps it read-write and uncached.
#[test]
fn the_attributes_say_how_each_gpu_maps_the_pool() {
    let process = with_both();
    assert_eq!(alloc(&process, &[(GPU0, READ_ONLY, FORCE_CACHED)]), NV_OK);
    assert_eq!(lent(&process), vec![(DEVICE0, true), (DEVICE1, false)]);
    let read_only: Vec<(u32, bool)> = process
        .rm
        .mapped()
        .iter()
        .map(|m| (m.h_va_space, m.read_only))
        .collect();
    assert!(read_only.contains(&(VA_SPACE0, true)));
    assert!(read_only.contains(&(VA_SPACE1, false)));
}

/// What `uvm_api_alloc_semaphore_pool` refuses, it refuses before any page
/// is lent: a range that is not whole pages, more GPUs than there can be,
/// a mapping or caching type it does not know, a GPU not registered, and a
/// second GPU caching it.
#[test]
fn a_pool_is_refused_as_the_module_refuses_it() {
    let process = with_both();
    let refused = [
        (
            alloc_with(&process, POOL + 1, LENGTH, &[], 0),
            NV_ERR_INVALID_ADDRESS,
        ),
        (
            alloc_with(&process, POOL, 0, &[], 0),
            NV_ERR_INVALID_ADDRESS,
        ),
        (
            alloc_with(&process, 0, LENGTH, &[], 0),
            NV_ERR_INVALID_ADDRESS,
        ),
        (
            alloc_with(&process, POOL, LENGTH, &[], MAX_GPUS + 1),
            NV_ERR_INVALID_ARGUMENT,
        ),
        (alloc(&process, &[(GPU0, 4, 0)]), NV_ERR_INVALID_ARGUMENT),
        (alloc(&process, &[(GPU0, 0, 3)]), NV_ERR_INVALID_ARGUMENT),
        (alloc(&process, &[(GPU2, 0, 0)]), NV_ERR_INVALID_DEVICE),
        (
            alloc(
                &process,
                &[(GPU0, 0, FORCE_CACHED), (GPU1, 0, FORCE_CACHED)],
            ),
            NV_ERR_INVALID_ARGUMENT,
        ),
    ];
    for (i, &(got, want)) in refused.iter().enumerate() {
        assert_eq!(got, want, "{}", i);
    }
    assert!(process.rm.state().lent.is_empty());
    assert!(process.rm.mapped().is_empty());
    assert_eq!(cpu_map(&process).map(|_| ()), Err(LxError::ENODEV));
    // Each GPU named once, one of them cached, is a pool.
    assert_eq!(
        alloc(&process, &[(GPU0, 0, FORCE_CACHED), (GPU1, 0, 1)]),
        NV_OK
    );
}

/// No range of either kind overlaps another: a pool over an external range
/// or a pool is in use, and so is an external range over a pool. Ranges
/// that only touch are not.
#[test]
fn a_pool_overlaps_no_other_range() {
    let process = with_both();
    assert_eq!(alloc(&process, &[]), NV_OK);
    assert_eq!(
        alloc_with(&process, POOL + LENGTH - 0x1000, 0x2000, &[], 0),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(
        process.create_external_range(POOL - 0x1000, 0x2000),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(process.create_external_range(POOL + LENGTH, 0x1000), NV_OK);
    assert_eq!(
        alloc_with(&process, POOL + LENGTH, 0x1000, &[], 0),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(alloc_with(&process, POOL - 0x1000, 0x1000, &[], 0), NV_OK);
    assert_eq!(process.rm.state().lent.len(), 4);
}

/// A GPU VA space registered after the pool was made gets it mapped too,
/// as its attributes said.
#[test]
fn a_later_gpu_va_space_gets_the_pools_already_made() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(alloc(&process, &[(GPU1, READ_ONLY, 0)]), NV_OK);
    assert_eq!(process.mapped_on(VA_SPACE1), Vec::<u64>::new());
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.mapped_on(VA_SPACE0), vec![POOL]);
    assert_eq!(process.mapped_on(VA_SPACE1), vec![POOL]);
    let on1 = process
        .rm
        .mapped()
        .into_iter()
        .find(|m| m.h_va_space == VA_SPACE1)
        .expect("mapped");
    assert!(on1.read_only);
}

/// A pool that cannot be mapped on a GPU VA space being registered fails
/// the registration: the memory made of its pages there is freed, the GPU
/// is left with no GPU VA space, and it can be registered again.
#[test]
fn a_pool_that_cannot_be_mapped_fails_the_registration() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(alloc(&process, &[]), NV_OK);
    process.rm.state().refuse.insert(VA_SPACE1, 0x1f);
    assert_eq!(process.register_gpu_va_space(&GPU1), 0x1f);
    assert_eq!(lent(&process), vec![(DEVICE0, false)]);
    assert_eq!(
        process.unregister_gpu_va_space(&GPU1),
        NV_ERR_INVALID_DEVICE
    );
    process.rm.state().refuse.clear();
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(lent(&process), vec![(DEVICE0, false), (DEVICE1, false)]);
}

/// When one GPU fails, the pool is not made: what the others mapped is
/// unmapped, every page lent is freed, and the range is free again.
#[test]
fn a_gpu_that_fails_takes_the_pool_with_it() {
    let process = with_both();
    process.rm.state().refuse_pages.insert(DEVICE1, 0x51);
    assert_eq!(alloc(&process, &[]), NV_ERR_NO_MEMORY);
    assert!(process.rm.state().lent.is_empty());
    assert!(process.rm.mapped().is_empty());
    assert_eq!(process.free(POOL, LENGTH), NV_ERR_INVALID_ADDRESS);
    process.rm.state().refuse_pages.clear();
    process.rm.state().refuse.insert(VA_SPACE1, 0x1f);
    assert_eq!(alloc(&process, &[]), 0x1f);
    assert!(process.rm.state().lent.is_empty());
    assert!(process.rm.mapped().is_empty());
    process.rm.state().refuse.clear();
    assert_eq!(alloc(&process, &[]), NV_OK);
}

/// UVM_FREE of the whole pool unmaps it on every GPU, the virtual object
/// before the memory, and once nothing else holds them its pages go back.
#[test]
fn free_gives_back_the_pool_and_its_pages() {
    let process = with_both();
    assert_eq!(alloc(&process, &[]), NV_OK);
    let pages = Arc::downgrade(&cpu_map(&process).unwrap());
    assert_eq!(process.free(POOL, LENGTH - 0x1000), NV_ERR_INVALID_ADDRESS);
    assert_eq!(process.free(POOL, LENGTH), NV_OK);
    assert!(process.rm.state().lent.is_empty());
    assert!(process.rm.mapped().is_empty());
    let frees: Vec<u32> = process.rm.state().frees.iter().map(|f| f.2).collect();
    assert_eq!(frees.len(), 4);
    for pair in frees.chunks(2) {
        assert_eq!(pair[0] >> 16, 0xcaf0, "the virtual object first");
        assert_eq!(pair[1] >> 16, 0xbee0, "then the memory");
    }
    assert!(pages.upgrade().is_none());
    assert_eq!(process.free(POOL, LENGTH), NV_ERR_INVALID_ADDRESS);
}

/// A pool the CPU still maps is not freed (`uvm_mem_mapped_on_cpu_user`);
/// once it is unmapped, it is.
#[test]
fn a_pool_mapped_on_the_cpu_is_not_freed() {
    let process = with_both();
    assert_eq!(alloc(&process, &[]), NV_OK);
    let mapping = cpu_map(&process).unwrap();
    assert_eq!(process.free(POOL, LENGTH), NV_ERR_INVALID_ARGUMENT);
    assert_eq!(process.rm.state().lent.len(), 2);
    drop(mapping);
    assert_eq!(process.free(POOL, LENGTH), NV_OK);
}

/// An mmap maps exactly a pool, as its own pages; a range that only
/// overlaps one, or an external range, is in use; any other would be
/// managed memory; and an open that is not a VA space maps nothing.
#[test]
fn mmap_maps_exactly_a_pool() {
    let process = with_both();
    assert_eq!(alloc(&process, &[]), NV_OK);
    assert_eq!(process.create_external_range(0x10_0000, 0x4000), NV_OK);
    let pool = process.uvm.va_space.lock().pools[&POOL].memory.clone();
    assert!(Arc::ptr_eq(&cpu_map(&process).unwrap(), &pool));
    let uvm = &process.uvm;
    let got = |offset: u64, len: u64| uvm.get_vmo(offset as usize, len as usize).map(|_| ());
    assert_eq!(got(POOL, LENGTH - 0x1000), Err(LxError::EADDRINUSE));
    assert_eq!(got(POOL + 0x1000, LENGTH), Err(LxError::EADDRINUSE));
    assert_eq!(got(0x10_0000, 0x4000), Err(LxError::EADDRINUSE));
    assert_eq!(got(0x40_0000, 0x1000), Err(LxError::ENODEV));
    assert_eq!(
        uvm_on(&FakeRm::new())
            .get_vmo(POOL as usize, LENGTH as usize)
            .map(|_| ()),
        Err(LxError::EBADFD)
    );
}

/// Unregistering a GPU VA space unmaps the pool there and frees what was
/// made of its pages, and the pool stays on the other GPU.
#[test]
fn a_gpu_va_space_takes_its_pool_mappings_with_it() {
    let process = with_both();
    assert_eq!(alloc(&process, &[]), NV_OK);
    assert_eq!(process.unregister_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(lent(&process), vec![(DEVICE0, false)]);
    assert_eq!(process.mapped_on(VA_SPACE1), Vec::<u64>::new());
    assert_eq!(process.mapped_on(VA_SPACE0), vec![POOL]);
    assert_eq!(process.free(POOL, LENGTH), NV_OK);
    assert!(process.rm.state().lent.is_empty());
}

/// The close unmaps every pool as nobody, and the pages stay for as long
/// as the RM still holds them.
#[test]
fn closing_the_va_space_unmaps_the_pools_and_the_rm_keeps_its_pages() {
    let process = with_both();
    assert_eq!(alloc(&process, &[]), NV_OK);
    let pages = Arc::downgrade(&cpu_map(&process).unwrap());
    let held = process
        .rm
        .state()
        .lent
        .values()
        .next()
        .unwrap()
        .hold
        .clone();
    let os_info = process.os_info();
    let Process { rm, uvm, ctl, .. } = process;
    drop(uvm);
    assert!(rm.state().lent.is_empty());
    assert!(rm.mapped().is_empty());
    let nobody = By {
        os_info,
        admin: false,
        gpu_mask: 0,
    };
    assert!(rm.state().frees.iter().all(|&(by, _, _)| by == nobody));
    assert_eq!(rm.state().frees.len(), 4);
    assert!(pages.upgrade().is_some());
    drop(held);
    assert!(pages.upgrade().is_none());
    drop(ctl);
}

/// With the `/dev/nvidiactl` open closed there is no client to make memory
/// in, and no pool is made.
#[test]
fn a_closed_ctl_open_makes_no_pool() {
    let mut process = with_both();
    process.ctl = None;
    assert_eq!(alloc(&process, &[]), nvctl::NV_ERR_INVALID_CLIENT);
    assert!(process.rm.state().lent.is_empty());
    assert_eq!(cpu_map(&process).map(|_| ()), Err(LxError::ENODEV));
}
