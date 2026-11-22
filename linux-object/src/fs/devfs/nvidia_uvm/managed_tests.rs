//! What `cudaMallocManaged` asks of UVM: an mmap of the VA space that is
//! managed memory, mapped on every GPU VA space at the same address, and the
//! policy and prefetch calls on it.

use super::managed::*;
use super::mapping_tests::*;
use super::tests::*;
use super::*;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

const BASE: u64 = 0x7f00_0000_0000;
const LENGTH: u64 = 0x4000;

/// A [`Process`] with a GPU VA space on both GPUs.
fn with_both() -> Process {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    process
}

/// The CPU's mmap of `[base, base + length)`.
fn mmap(process: &Process, base: u64, length: u64) -> LxResult<Arc<VmObject>> {
    process.uvm.get_vmo(base as usize, length as usize)
}

/// The devices of the managed pages the RM holds, sorted.
fn devices(process: &Process) -> Vec<u32> {
    let mut devices: Vec<u32> = process
        .rm
        .state()
        .user_pages
        .values()
        .map(|(_, _, device, _)| *device)
        .collect();
    devices.sort_unstable();
    devices
}

fn policy(process: &Process, base: u64) -> Policy {
    process.uvm.va_space.lock().managed[&base].policy().clone()
}

fn range_call(process: &Process, cmd: u32, base: u64, length: u64) -> u32 {
    let mut params = vec![0u8; RANGE_SIZE];
    params[..8].copy_from_slice(&base.to_le_bytes());
    params[8..16].copy_from_slice(&length.to_le_bytes());
    process.call(cmd, &mut params)
}

fn gpu_range_call(process: &Process, cmd: u32, base: u64, length: u64, uuid: &Uuid) -> u32 {
    let mut params = vec![0u8; GPU_RANGE_SIZE];
    params[..8].copy_from_slice(&base.to_le_bytes());
    params[8..16].copy_from_slice(&length.to_le_bytes());
    params[GPU_RANGE_GPU..][..16].copy_from_slice(uuid);
    process.call(cmd, &mut params)
}

fn set_preferred(process: &Process, base: u64, length: u64, uuid: &Uuid) -> u32 {
    let mut params = vec![0u8; PREFERRED_SIZE];
    params[..8].copy_from_slice(&base.to_le_bytes());
    params[8..16].copy_from_slice(&length.to_le_bytes());
    params[16..32].copy_from_slice(uuid);
    process.call(cmd::SET_PREFERRED_LOCATION, &mut params)
}

/// UVM_MIGRATE of `[base, base + length)` to `uuid`: the status and the
/// params as they came back.
fn migrate(
    process: &Process,
    base: u64,
    length: u64,
    uuid: &Uuid,
    flags: u32,
    semaphore: u64,
    payload: u32,
) -> (u32, Vec<u8>) {
    let mut params = vec![0u8; MIGRATE_SIZE];
    params[..8].copy_from_slice(&base.to_le_bytes());
    params[8..16].copy_from_slice(&length.to_le_bytes());
    params[16..32].copy_from_slice(uuid);
    params[32..36].copy_from_slice(&flags.to_le_bytes());
    params[40..48].copy_from_slice(&semaphore.to_le_bytes());
    params[48..52].copy_from_slice(&payload.to_le_bytes());
    params[56..64].copy_from_slice(&0x1234u64.to_le_bytes());
    params[64..72].copy_from_slice(&0x5678u64.to_le_bytes());
    let status = process.call(cmd::MIGRATE, &mut params);
    (status, params)
}

/// The mmap is its own pages, zeroed; each GPU's RM memory of them is
/// under that GPU's device, starts at the range, holds every page in order,
/// and is mapped whole at the range's address, read-write.
#[test]
fn an_mmap_is_managed_memory_mapped_on_every_gpu() {
    let process = with_both();
    let vmo = mmap(&process, BASE, LENGTH).unwrap();
    let mut bytes = vec![0xAAu8; LENGTH as usize];
    vmo.read(0, &mut bytes).unwrap();
    assert!(bytes.iter().all(|&b| b == 0));
    let phys: Vec<u64> = (0..(LENGTH / PAGE_4K) as usize)
        .map(|i| vmo.committed_paddr(i).unwrap() as u64)
        .collect();
    assert_eq!(devices(&process), vec![DEVICE0, DEVICE1]);
    let state = process.rm.state();
    for (&h_memory, (by, h_client, h_device, pages)) in &state.user_pages {
        assert_eq!(*by, process.by());
        assert_eq!(*h_client, CLIENT);
        assert_eq!(pages.va, BASE as usize);
        assert_eq!(pages.phys, phys);
        assert!(pages.writable);
        let va_space = if *h_device == DEVICE0 {
            VA_SPACE0
        } else {
            VA_SPACE1
        };
        let (_, mapping) = state
            .maps
            .values()
            .find(|(_, m)| m.h_memory == h_memory)
            .copied()
            .expect("mapped");
        assert_eq!(
            mapping,
            nvctl::ExternalMapping {
                h_client: CLIENT,
                h_device: *h_device,
                h_va_space: va_space,
                h_memory,
                offset: 0,
                base: BASE,
                length: LENGTH,
                read_only: false,
            }
        );
    }
    drop(state);
    // `uvm_api_validate_va_range` takes a range of any kind.
    assert_eq!(process.validate(BASE, LENGTH), NV_OK);
}

/// A GPU VA space registered after the mmap maps the range too, and one
/// unregistered takes its mapping with it.
#[test]
fn a_later_gpu_va_space_maps_the_range_too() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    let _vmo = mmap(&process, BASE, LENGTH).unwrap();
    assert_eq!(process.mapped_on(VA_SPACE0), vec![BASE]);
    assert!(process.mapped_on(VA_SPACE1).is_empty());
    assert_eq!(process.register_gpu_va_space(&GPU1), NV_OK);
    assert_eq!(process.mapped_on(VA_SPACE1), vec![BASE]);
    assert_eq!(devices(&process), vec![DEVICE0, DEVICE1]);
    assert_eq!(
        process.uvm.va_space.lock().managed[&BASE].gpus_mapped(),
        vec![GPU0, GPU1]
    );
    assert_eq!(process.unregister_gpu_va_space(&GPU1), NV_OK);
    assert!(process.mapped_on(VA_SPACE1).is_empty());
    assert_eq!(devices(&process), vec![DEVICE0]);
}

/// A GPU VA space that cannot map a managed range is not registered, and
/// leaves nothing behind.
#[test]
fn a_gpu_va_space_that_cannot_map_the_range_is_not_registered() {
    let process = Process::new();
    assert_eq!(process.register_gpu_va_space(&GPU0), NV_OK);
    let _vmo = mmap(&process, BASE, LENGTH).unwrap();
    process.rm.state().refuse.insert(VA_SPACE1, 0x51);
    assert_eq!(process.register_gpu_va_space(&GPU1), 0x51);
    assert!(!process
        .uvm
        .va_space
        .lock()
        .gpu_va_spaces
        .contains_key(&GPU1));
    assert_eq!(devices(&process), vec![DEVICE0]);
}

/// A range some GPU cannot map is not made: the mmap is ENOMEM and nothing
/// stays in the RM.
#[test]
fn a_range_a_gpu_refuses_is_enomem() {
    let process = with_both();
    process.rm.state().refuse.insert(VA_SPACE1, 0x51);
    assert_eq!(
        mmap(&process, BASE, LENGTH).map(|_| ()),
        Err(LxError::ENOMEM)
    );
    assert!(process.rm.state().user_pages.is_empty());
    assert!(process.rm.mapped().is_empty());
    assert!(process.uvm.va_space.lock().managed.is_empty());
    process.rm.state().refuse_pages.insert(DEVICE0, 0x51);
    assert_eq!(
        mmap(&process, BASE, LENGTH).map(|_| ()),
        Err(LxError::ENOMEM)
    );
    assert!(process.rm.state().user_pages.is_empty());
}

/// Once the CPU unmaps the range, the next call frees it on every GPU, and
/// the address can be mapped again.
#[test]
fn munmap_frees_the_range_at_the_next_call() {
    let process = with_both();
    let vmo = mmap(&process, BASE, LENGTH).unwrap();
    assert_eq!(
        range_call(&process, cmd::ENABLE_READ_DUPLICATION, BASE, LENGTH),
        NV_OK
    );
    assert_eq!(devices(&process).len(), 2);
    drop(vmo);
    assert_eq!(
        range_call(&process, cmd::ENABLE_READ_DUPLICATION, BASE, LENGTH),
        NV_ERR_INVALID_ADDRESS
    );
    assert!(process.rm.state().user_pages.is_empty());
    assert!(process.rm.mapped().is_empty());
    assert!(process.uvm.va_space.lock().managed.is_empty());
    assert!(mmap(&process, BASE, LENGTH).is_ok());
}

/// Managed memory overlaps no other range, of any kind; and it must be
/// whole pages the GPU can address.
#[test]
fn managed_memory_overlaps_nothing() {
    let process = with_both();
    let _vmo = mmap(&process, BASE, LENGTH).unwrap();
    let in_use = Err(LxError::EADDRINUSE);
    assert_eq!(mmap(&process, BASE, LENGTH).map(|_| ()), in_use);
    assert_eq!(mmap(&process, BASE + 0x1000, LENGTH).map(|_| ()), in_use);
    assert_eq!(
        process.create_external_range(BASE + 0x1000, 0x1000),
        NV_ERR_UVM_ADDRESS_IN_USE
    );
    assert_eq!(
        mmap(&process, 0x8000_0000_0000, 0x1000).map(|_| ()),
        Err(LxError::EINVAL)
    );
    // One that only touches it is not in use.
    assert!(mmap(&process, BASE + LENGTH, 0x1000).is_ok());
}

/// `cudaMemAdvise` and `cudaStreamAttachMemAsync`: checked, and recorded on
/// every range they cover.
#[test]
fn the_policy_calls_are_checked_and_recorded() {
    let process = with_both();
    let _a = mmap(&process, BASE, LENGTH).unwrap();
    let _b = mmap(&process, BASE + LENGTH, LENGTH).unwrap();
    let both = 2 * LENGTH;
    assert_eq!(set_preferred(&process, BASE, both, &GPU1), NV_OK);
    assert_eq!(
        gpu_range_call(&process, cmd::SET_ACCESSED_BY, BASE, both, &GPU0),
        NV_OK
    );
    assert_eq!(
        range_call(&process, cmd::ENABLE_READ_DUPLICATION, BASE, both),
        NV_OK
    );
    let (group, status) = create_range_group(&process.uvm);
    assert_eq!(status, NV_OK);
    let mut params = vec![0u8; SET_RANGE_GROUP_SIZE];
    params[..8].copy_from_slice(&group.to_le_bytes());
    params[8..16].copy_from_slice(&BASE.to_le_bytes());
    params[16..24].copy_from_slice(&LENGTH.to_le_bytes());
    assert_eq!(process.call(cmd::SET_RANGE_GROUP, &mut params), NV_OK);
    for base in [BASE, BASE + LENGTH] {
        let got = policy(&process, base);
        assert_eq!(got.preferred, Some(GPU1));
        assert!(got.accessed_by.contains(&GPU0));
        assert!(got.read_duplication);
    }
    assert_eq!(policy(&process, BASE).range_group, group);
    assert_eq!(policy(&process, BASE + LENGTH).range_group, 0);

    assert_eq!(
        range_call(&process, cmd::UNSET_PREFERRED_LOCATION, BASE, LENGTH),
        NV_OK
    );
    assert_eq!(
        gpu_range_call(&process, cmd::UNSET_ACCESSED_BY, BASE, LENGTH, &GPU0),
        NV_OK
    );
    assert_eq!(
        range_call(&process, cmd::DISABLE_READ_DUPLICATION, BASE, LENGTH),
        NV_OK
    );
    let got = policy(&process, BASE);
    assert_eq!(got.preferred, None);
    assert!(got.accessed_by.is_empty());
    assert!(!got.read_duplication);
    assert_eq!(policy(&process, BASE + LENGTH).preferred, Some(GPU1));

    // The CPU is a processor too; a GPU not registered is not.
    assert_eq!(set_preferred(&process, BASE, LENGTH, &[0; 16]), NV_OK);
    assert_eq!(
        set_preferred(&process, BASE, LENGTH, &[0x30; 16]),
        NV_ERR_INVALID_DEVICE
    );
    // Not managed memory, or not all of it, or not whole pages.
    assert_eq!(
        set_preferred(&process, BASE, 3 * LENGTH, &GPU0),
        NV_ERR_INVALID_ADDRESS
    );
    assert_eq!(
        set_preferred(&process, 0x10_0000, 0x1000, &GPU0),
        NV_ERR_INVALID_ADDRESS
    );
    assert_eq!(
        set_preferred(&process, BASE, 0x800, &GPU0),
        NV_ERR_INVALID_ADDRESS
    );
    // A range group that is not one here.
    params[..8].copy_from_slice(&(group + 7).to_le_bytes());
    assert_eq!(
        process.call(cmd::SET_RANGE_GROUP, &mut params),
        NV_ERR_OBJECT_NOT_FOUND
    );
}

/// Range groups are checked by id, and the GPUs of system-wide atomics must
/// be registered.
#[test]
fn range_group_migration_and_atomics_are_checked() {
    let process = with_both();
    let (group, _) = create_range_group(&process.uvm);
    let mut params = vec![0u8; MIGRATION_GROUPS_SIZE];
    params[..8].copy_from_slice(&group.to_le_bytes());
    params[256..264].copy_from_slice(&1u64.to_le_bytes());
    assert_eq!(
        process.call(cmd::PREVENT_MIGRATION_RANGE_GROUPS, &mut params),
        NV_OK
    );
    assert_eq!(
        process.call(cmd::ALLOW_MIGRATION_RANGE_GROUPS, &mut params),
        NV_OK
    );
    params[..8].copy_from_slice(&(group + 1).to_le_bytes());
    assert_eq!(
        process.call(cmd::ALLOW_MIGRATION_RANGE_GROUPS, &mut params),
        NV_ERR_OBJECT_NOT_FOUND
    );
    params[256..264].copy_from_slice(&0u64.to_le_bytes());
    assert_eq!(
        process.call(cmd::ALLOW_MIGRATION_RANGE_GROUPS, &mut params),
        NV_ERR_INVALID_ARGUMENT
    );

    let mut params = vec![0u8; MIGRATE_RANGE_GROUP_SIZE];
    params[..8].copy_from_slice(&group.to_le_bytes());
    params[8..24].copy_from_slice(&GPU1);
    assert_eq!(process.call(cmd::MIGRATE_RANGE_GROUP, &mut params), NV_OK);
    params[8..24].copy_from_slice(&[0x30; 16]);
    assert_eq!(
        process.call(cmd::MIGRATE_RANGE_GROUP, &mut params),
        NV_ERR_INVALID_DEVICE
    );

    let mut params = vec![0u8; GPU_ONLY_SIZE];
    params[..16].copy_from_slice(&GPU0);
    assert_eq!(
        process.call(cmd::ENABLE_SYSTEM_WIDE_ATOMICS, &mut params),
        NV_OK
    );
    assert_eq!(
        process.call(cmd::DISABLE_SYSTEM_WIDE_ATOMICS, &mut params),
        NV_OK
    );
    params[..16].copy_from_slice(&[0x30; 16]);
    assert_eq!(
        process.call(cmd::ENABLE_SYSTEM_WIDE_ATOMICS, &mut params),
        NV_ERR_INVALID_DEVICE
    );
}

/// `cudaMemPrefetchAsync`: done at once, its semaphore released, and
/// nothing left for user space to move.
#[test]
fn a_prefetch_completes_at_once_and_releases_its_semaphore() {
    let process = with_both();
    let _vmo = mmap(&process, BASE, LENGTH).unwrap();
    let mut semaphore = Box::new(0u32);
    let at = &mut *semaphore as *mut u32 as u64;
    let (status, params) = migrate(&process, BASE, LENGTH, &GPU1, 1, at, 0x77);
    assert_eq!(status, NV_OK);
    assert_eq!(*semaphore, 0x77);
    assert_eq!(u64_at(&params, 56), 0);
    assert_eq!(u64_at(&params, 64), 0);
    assert_eq!(migrate(&process, BASE, LENGTH, &[0; 16], 0, 0, 0).0, NV_OK);
    // A semaphore with a synchronous migration, an unknown flag.
    assert_eq!(
        migrate(&process, BASE, LENGTH, &GPU0, 0, at, 1).0,
        NV_ERR_INVALID_ARGUMENT
    );
    assert_eq!(
        migrate(&process, BASE, LENGTH, &GPU0, 0x8, 0, 0).0,
        NV_ERR_INVALID_ARGUMENT
    );
    // Not to a GPU with a GPU VA space; not managed memory.
    assert_eq!(
        migrate(&process, BASE, LENGTH, &[0x30; 16], 0, 0, 0).0,
        NV_ERR_INVALID_DEVICE
    );
    assert_eq!(
        migrate(&process, 0x10_0000, 0x1000, &GPU0, 0, 0, 0).0,
        NV_ERR_INVALID_ADDRESS
    );
    assert_eq!(*semaphore, 0x77);
}
