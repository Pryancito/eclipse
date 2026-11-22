//! Managed memory (`cudaMallocManaged`, `cuMemAllocManaged`): one range of
//! virtual addresses that the CPU and every GPU of the VA space read and
//! write at the same address.
//!
//! libcuda makes one with an mmap of `/dev/nvidia-uvm` at the address it
//! wants (`uvm_mmap`, a `uvm_va_range_managed_t`). On Linux the pages are
//! made on first touch, and UVM moves them to whoever touches them: a CPU
//! fault brings them to system memory, a GPU's replayable fault (Pascal and
//! later) to that GPU's video memory, and UVM writes the PTEs on both sides
//! as they move.
//!
//! This port has no GPU fault handling and no UVM page tables (the RM keeps
//! those, see the module's own doc), so a managed range lives where every
//! processor can reach it all the time: in system memory. Its pages are
//! made, zeroed and pinned at the mmap, the CPU maps them as that mmap, and
//! each GPU with a GPU VA space maps them at the same address, now or when
//! its GPU VA space comes later, as RM memory of the process's own pages
//! (an OS descriptor of their page array, the way `cuMemHostRegister`
//! memory is made). The GPU reaches them across PCIe, snooped, so what one
//! processor writes the other reads with no copy and no fault: the memory
//! model of managed memory with `concurrentManagedAccess`, at the speed of
//! system memory from the GPU's side.
//!
//! The policy calls (`cudaMemAdvise`: preferred location, accessed-by, read
//! duplication; `cudaStreamAttachMemAsync`: range groups) are checked as
//! uvm.c checks them and recorded; with the memory already mapped
//! everywhere there is nothing for them to move. `cudaMemPrefetchAsync`
//! (UVM_MIGRATE) is checked the same way and completes at once, its
//! semaphore released before it returns.
//!
//! The range goes when the CPU no longer maps any of it: libcuda frees
//! managed memory with munmap, which UVM sees as the VMA's close. There is
//! no such hook here, so each call on the VA space first looks for ranges
//! nothing maps any more and frees them, with their GPU mappings
//! ([`UvmOpen::reap_managed`]).

use super::*;
use alloc::boxed::Box;
use core::convert::TryFrom;

/// `UVM_MIGRATE_PARAMS`: `NvU64 base`, `NvU64 length`, `NvProcessorUuid
/// destinationUuid`, `NvU32 flags`, `NvU64 semaphoreAddress`, `NvU32
/// semaphorePayload`, `NvS32 cpuNumaNode`, `NvU64 userSpaceStart`, `NvU64
/// userSpaceLength`, `NV_STATUS rmStatus`.
pub(super) const MIGRATE_SIZE: usize = 80;
pub(super) const MIGRATE_STATUS: usize = 72;
const MIGRATE_DESTINATION: usize = 16;
const MIGRATE_FLAGS: usize = 32;
const MIGRATE_SEMAPHORE: usize = 40;
const MIGRATE_PAYLOAD: usize = 48;
const MIGRATE_USER_START: usize = 56;
const MIGRATE_USER_LENGTH: usize = 64;
/// `UVM_MIGRATE_FLAG_ASYNC`, and every flag uvm.c takes
/// (`UVM_MIGRATE_FLAGS_ALL`).
const MIGRATE_FLAG_ASYNC: u32 = 0x1;
const MIGRATE_FLAGS_ALL: u32 = 0x7;
/// `UVM_SET_PREFERRED_LOCATION_PARAMS`: `NvU64 requestedBase`, `NvU64
/// length`, `NvProcessorUuid preferredLocation`, `NvS32
/// preferredCpuNumaNode`, `NV_STATUS rmStatus`.
pub(super) const PREFERRED_SIZE: usize = 40;
pub(super) const PREFERRED_STATUS: usize = 36;
/// `UVM_SET_RANGE_GROUP_PARAMS`: `NvU64 rangeGroupId`, `NvU64
/// requestedBase`, `NvU64 length`, `NV_STATUS rmStatus`.
pub(super) const SET_RANGE_GROUP_SIZE: usize = 32;
pub(super) const SET_RANGE_GROUP_STATUS: usize = 24;
/// `UVM_PREVENT_MIGRATION_RANGE_GROUPS_PARAMS` and its ALLOW twin: `NvU64
/// rangeGroupIds[UVM_MAX_RANGE_GROUPS_PER_IOCTL_CALL]`, `NvU64
/// numGroupIds`, `NV_STATUS rmStatus`.
pub(super) const MIGRATION_GROUPS_SIZE: usize = 272;
pub(super) const MIGRATION_GROUPS_STATUS: usize = 264;
const MIGRATION_GROUPS_COUNT: usize = 256;
const MAX_RANGE_GROUPS_PER_CALL: u64 = 32;
/// `UVM_MIGRATE_RANGE_GROUP_PARAMS`: `NvU64 rangeGroupId`,
/// `NvProcessorUuid destinationUuid`, `NV_STATUS rmStatus`.
pub(super) const MIGRATE_RANGE_GROUP_SIZE: usize = 32;
pub(super) const MIGRATE_RANGE_GROUP_STATUS: usize = 24;
/// `UVM_ENABLE_SYSTEM_WIDE_ATOMICS_PARAMS` and its DISABLE twin:
/// `NvProcessorUuid gpu_uuid`, `NV_STATUS rmStatus`.
pub(super) const GPU_ONLY_SIZE: usize = 20;
pub(super) const GPU_ONLY_STATUS: usize = 16;

/// `UVM_RANGE_GROUP_ID_NONE`.
const RANGE_GROUP_NONE: u64 = 0;

/// The CPU's `NvProcessorUuid` (`NV_PROCESSOR_UUID_CPU_DEFAULT`): all zeros.
const CPU: Uuid = [0; nvctl::GPU_UUID_LEN];

/// A range of managed memory (`uvm_va_range_managed_t`).
pub(super) struct ManagedRange {
    /// One past its last byte.
    pub(super) end: u64,
    /// Its pages, which the CPU maps.
    memory: Arc<VmObject>,
    /// What keeps them pinned, which every GPU's RM memory of them holds a
    /// clone of.
    pinned: Arc<Pinned>,
    /// Each page's physical address, in order.
    phys: Vec<u64>,
    /// Its mapping on each GPU with a GPU VA space.
    maps: BTreeMap<Uuid, MemoryMapping>,
    /// `uvm_va_policy_t`, recorded.
    policy: Policy,
}

/// What `cudaMemAdvise` and range groups set on a managed range.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(super) struct Policy {
    /// The preferred location: the CPU's UUID or a GPU's, `None` when unset.
    pub(super) preferred: Option<Uuid>,
    pub(super) accessed_by: BTreeSet<Uuid>,
    pub(super) read_duplication: bool,
    /// The range group, [`RANGE_GROUP_NONE`] for none.
    pub(super) range_group: u64,
}

/// The pin of a managed range's pages, undone when the last holder (the
/// range, or the RM memory made of them on some GPU) lets go.
struct Pinned {
    memory: Arc<VmObject>,
    len: usize,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let _ = self.memory.unpin(0, self.len);
    }
}

impl ManagedRange {
    /// `[base, end)` of zeroed, pinned system memory. ENOMEM when there is
    /// not that much memory to pin.
    fn new(base: u64, end: u64) -> LxResult<ManagedRange> {
        let len = usize::try_from(end - base).map_err(|_| LxError::ENOMEM)?;
        let pages = len / PAGE_4K as usize;
        let memory = VmObject::new_paged(pages);
        memory.commit(0, len).map_err(|_| LxError::ENOMEM)?;
        memory.pin(0, len).map_err(|_| LxError::ENOMEM)?;
        let pinned = Arc::new(Pinned {
            memory: memory.clone(),
            len,
        });
        let phys = (0..pages)
            .map(|i| memory.committed_paddr(i).map(|p| p as u64))
            .collect::<Option<Vec<u64>>>()
            .ok_or(LxError::ENOMEM)?;
        Ok(ManagedRange {
            end,
            memory,
            pinned,
            phys,
            maps: BTreeMap::new(),
            policy: Policy::default(),
        })
    }

    /// Whether the CPU still maps any of it: anything holds its pages but
    /// the range and its pin.
    fn mapped_on_cpu(&self) -> bool {
        Arc::strong_count(&self.memory) > 2
    }

    #[cfg(test)]
    pub(super) fn policy(&self) -> &Policy {
        &self.policy
    }

    #[cfg(test)]
    pub(super) fn gpus_mapped(&self) -> Vec<Uuid> {
        self.maps.keys().copied().collect()
    }
}

impl VaSpace {
    /// The managed ranges that together cover `[base, end)` with no gap, by
    /// first byte: `uvm_for_each_va_range_managed_in_contig`. `None` when
    /// some of it is not managed memory.
    fn managed_covering(&self, base: u64, end: u64) -> Option<Vec<u64>> {
        let mut at = base;
        let mut found = Vec::new();
        while at < end {
            let (&first, range) = self.managed.range(..=at).next_back()?;
            if range.end <= at {
                return None;
            }
            found.push(first);
            at = range.end;
        }
        Some(found)
    }

    /// `uvm_api_range_invalid` and then every byte managed: the check of
    /// every policy call.
    fn managed_in(&self, base: u64, length: u64) -> core::result::Result<Vec<u64>, u32> {
        let end = range_end(base, length).ok_or(NV_ERR_INVALID_ADDRESS)?;
        self.managed_covering(base, end)
            .ok_or(NV_ERR_INVALID_ADDRESS)
    }

    /// A processor policy can name: the CPU, or a GPU registered here.
    fn processor(&self, uuid: &Uuid) -> core::result::Result<(), u32> {
        if *uuid == CPU || self.gpus.contains(uuid) {
            Ok(())
        } else {
            Err(NV_ERR_INVALID_DEVICE)
        }
    }

    /// Set something on the policy of every managed range in `[base, base +
    /// length)`.
    fn set_policy(&mut self, base: u64, length: u64, set: impl Fn(&mut Policy)) -> u32 {
        match self.managed_in(base, length) {
            Ok(ranges) => {
                for first in ranges {
                    if let Some(range) = self.managed.get_mut(&first) {
                        set(&mut range.policy);
                    }
                }
                NV_OK
            }
            Err(status) => status,
        }
    }
}

impl UvmOpen {
    /// `uvm_mmap` of a range that is no other here: a new managed range,
    /// mapped on every GPU with a GPU VA space, whose pages the mmap maps
    /// for the CPU. One that cannot be mapped on some GPU is not made, and
    /// the mmap fails with ENOMEM, as an mmap the kernel has no memory for.
    pub(super) fn mmap_managed(
        &self,
        va_space: &mut VaSpace,
        base: u64,
        end: u64,
    ) -> LxResult<Arc<VmObject>> {
        if range_end(base, end - base).is_none() || !gpu_can_address(base, end) {
            return Err(LxError::EINVAL);
        }
        let mut range = ManagedRange::new(base, end)?;
        let who = va_space.who.unwrap_or(Who::NOBODY);
        for (uuid, gpu_va_space) in &va_space.gpu_va_spaces {
            if let Err(status) = self.map_managed(gpu_va_space, uuid, base, &mut range, who) {
                managed_refused(base, end, status);
                self.unmap_managed_everywhere(&va_space.gpu_va_spaces, range, who);
                return Err(LxError::ENOMEM);
            }
        }
        let memory = range.memory.clone();
        va_space.managed.insert(base, range);
        first_time(
            &FIRST_MANAGED,
            "mmap: the first range of managed memory (cudaMallocManaged); its pages are system memory, pinned, mapped on the CPU and on every GPU at the same address",
        );
        Ok(memory)
    }

    /// `va_range_add_gpu_va_space_managed`, done at once instead of at the
    /// GPU's first fault: the range at `base` mapped at `base` on the GPU VA
    /// space of `uuid`, as the user, as RM memory of the process's pages.
    pub(super) fn map_managed(
        &self,
        gpu_va_space: &GpuVaSpace,
        uuid: &Uuid,
        base: u64,
        range: &mut ManagedRange,
        who: Who,
    ) -> core::result::Result<(), u32> {
        let Some((_open, by)) = gpu_va_space.ctl.caller(who) else {
            return Err(nvctl::NV_ERR_INVALID_CLIENT);
        };
        let pages = nvctl::UserPages {
            va: base as usize,
            phys: range.phys.clone(),
            writable: true,
            keep: Box::new(range.pinned.clone()),
        };
        let h_client = gpu_va_space.h_client;
        let h_memory = self
            .rm
            .user_pages(&by, h_client, gpu_va_space.h_device, pages)?;
        let mapping = nvctl::ExternalMapping {
            h_client,
            h_device: gpu_va_space.h_device,
            h_va_space: gpu_va_space.h_va_space,
            h_memory,
            offset: 0,
            base,
            length: range.end - base,
            read_only: false,
        };
        match self.rm.map(&by, &mapping) {
            Ok(h_virtual) => {
                range.maps.insert(
                    *uuid,
                    MemoryMapping {
                        h_memory,
                        h_virtual,
                    },
                );
                Ok(())
            }
            Err(status) => {
                self.rm.free(&by, h_client, h_memory);
                Err(status)
            }
        }
    }

    /// Every managed range here mapped on the GPU VA space of `uuid`, which
    /// has none of them yet. The first that fails stops it, with its status.
    pub(super) fn map_managed_on(
        &self,
        va_space: &mut VaSpace,
        uuid: &Uuid,
        who: Who,
    ) -> core::result::Result<(), u32> {
        let gpu_va_space = &va_space.gpu_va_spaces[uuid];
        for (&base, range) in va_space.managed.iter_mut() {
            self.map_managed(gpu_va_space, uuid, base, range, who)?;
        }
        Ok(())
    }

    /// The mapping of every managed range on the GPU VA space going away.
    pub(super) fn unmap_managed_from(
        &self,
        va_space: &mut VaSpace,
        uuid: &Uuid,
        gpu: &GpuVaSpace,
        who: Who,
    ) {
        for range in va_space.managed.values_mut() {
            if let Some(map) = range.maps.remove(uuid) {
                self.unmap_memory(gpu, map, who);
            }
        }
    }

    /// Every mapping of `range`, on every GPU. Its pages are unpinned and
    /// freed once neither the CPU nor the RM has them.
    pub(super) fn unmap_managed_everywhere(
        &self,
        gpu_va_spaces: &BTreeMap<Uuid, GpuVaSpace>,
        range: ManagedRange,
        who: Who,
    ) {
        for (uuid, map) in range.maps {
            if let Some(gpu_va_space) = gpu_va_spaces.get(&uuid) {
                self.unmap_memory(gpu_va_space, map, who);
            }
        }
    }

    /// The managed ranges the CPU no longer maps, freed: what the VMA's
    /// close does on Linux, looked for here before each call on the VA
    /// space and each mmap of it.
    pub(super) fn reap_managed(&self, va_space: &mut VaSpace, who: Who) {
        let gone: Vec<u64> = va_space
            .managed
            .iter()
            .filter(|(_, range)| !range.mapped_on_cpu())
            .map(|(&base, _)| base)
            .collect();
        for base in gone {
            if let Some(range) = va_space.managed.remove(&base) {
                self.unmap_managed_everywhere(&va_space.gpu_va_spaces, range, who);
            }
        }
    }

    /// `uvm_api_set_preferred_location`.
    pub(super) fn set_preferred_location(&self, params: &[u8]) -> u32 {
        let uuid = uuid_at(params, 16);
        let mut va_space = self.va_space.lock();
        if let Err(status) = va_space.processor(&uuid) {
            return status;
        }
        va_space.set_policy(u64_at(params, 0), u64_at(params, 8), |p| {
            p.preferred = Some(uuid)
        })
    }

    /// `uvm_api_unset_preferred_location`.
    pub(super) fn unset_preferred_location(&self, base: u64, length: u64) -> u32 {
        self.va_space
            .lock()
            .set_policy(base, length, |p| p.preferred = None)
    }

    /// `uvm_api_set_accessed_by` and its UNSET twin.
    pub(super) fn set_accessed_by(&self, params: &[u8], on: bool) -> u32 {
        let uuid = uuid_at(params, 16);
        let mut va_space = self.va_space.lock();
        if let Err(status) = va_space.processor(&uuid) {
            return status;
        }
        va_space.set_policy(u64_at(params, 0), u64_at(params, 8), |p| {
            if on {
                p.accessed_by.insert(uuid);
            } else {
                p.accessed_by.remove(&uuid);
            }
        })
    }

    /// `uvm_api_enable_read_duplication` and its DISABLE twin.
    pub(super) fn set_read_duplication(&self, base: u64, length: u64, on: bool) -> u32 {
        self.va_space
            .lock()
            .set_policy(base, length, |p| p.read_duplication = on)
    }

    /// `uvm_api_set_range_group`: the group must be one of this VA space's,
    /// or none.
    pub(super) fn set_range_group(&self, params: &[u8]) -> u32 {
        let group = u64_at(params, 0);
        let mut va_space = self.va_space.lock();
        if group != RANGE_GROUP_NONE && !va_space.range_groups.contains(&group) {
            return NV_ERR_OBJECT_NOT_FOUND;
        }
        va_space.set_policy(u64_at(params, 8), u64_at(params, 16), |p| {
            p.range_group = group
        })
    }

    /// `uvm_api_prevent_migration_range_groups` and its ALLOW twin: each
    /// group named must be one here. Nothing ever migrates, so preventing
    /// it changes nothing else.
    pub(super) fn migration_range_groups(&self, params: &[u8]) -> u32 {
        let count = u64_at(params, MIGRATION_GROUPS_COUNT);
        if count == 0 || count > MAX_RANGE_GROUPS_PER_CALL {
            return NV_ERR_INVALID_ARGUMENT;
        }
        let va_space = self.va_space.lock();
        let all_known = (0..count as usize)
            .map(|i| u64_at(params, i * 8))
            .all(|group| va_space.range_groups.contains(&group));
        if all_known {
            NV_OK
        } else {
            NV_ERR_OBJECT_NOT_FOUND
        }
    }

    /// `uvm_api_migrate_range_group`: a group here, to a processor here.
    pub(super) fn migrate_range_group(&self, params: &[u8]) -> u32 {
        let va_space = self.va_space.lock();
        if !va_space.range_groups.contains(&u64_at(params, 0)) {
            return NV_ERR_OBJECT_NOT_FOUND;
        }
        va_space
            .processor(&uuid_at(params, 8))
            .map_or_else(|status| status, |()| NV_OK)
    }

    /// `uvm_api_enable_system_wide_atomics` and its DISABLE twin: a GPU
    /// registered here. The memory is system memory, which the GPU's
    /// atomics reach across PCIe as they are, so there is nothing to turn on.
    pub(super) fn system_wide_atomics(&self, uuid: &Uuid) -> u32 {
        if self.va_space.lock().gpus.contains(uuid) {
            NV_OK
        } else {
            NV_ERR_INVALID_DEVICE
        }
    }

    /// `uvm_api_migrate` (`cudaMemPrefetchAsync`): managed memory only, to
    /// the CPU or a GPU with a GPU VA space here. It is where every
    /// processor reaches it already, so the migration is done when it is
    /// asked for: an asynchronous one releases its semaphore (writes the
    /// payload at `semaphoreAddress`, which the caller maps) before it
    /// returns, and a synchronous one must not name a semaphore.
    pub(super) fn migrate(&self, params: &mut [u8], write_u32: &dyn Fn(u64, u32) -> bool) -> u32 {
        let flags = u32_at(params, MIGRATE_FLAGS);
        let semaphore = u64_at(params, MIGRATE_SEMAPHORE);
        if flags & !MIGRATE_FLAGS_ALL != 0 {
            return NV_ERR_INVALID_ARGUMENT;
        }
        if flags & MIGRATE_FLAG_ASYNC == 0 && semaphore != 0 {
            return NV_ERR_INVALID_ARGUMENT;
        }
        let destination = uuid_at(params, MIGRATE_DESTINATION);
        {
            let va_space = self.va_space.lock();
            if destination != CPU && !va_space.gpu_va_spaces.contains_key(&destination) {
                return NV_ERR_INVALID_DEVICE;
            }
            if let Err(status) = va_space.managed_in(u64_at(params, 0), u64_at(params, 8)) {
                return status;
            }
        }
        params[MIGRATE_USER_START..MIGRATE_USER_START + 8].copy_from_slice(&0u64.to_le_bytes());
        params[MIGRATE_USER_LENGTH..MIGRATE_USER_LENGTH + 8].copy_from_slice(&0u64.to_le_bytes());
        if semaphore != 0 && !write_u32(semaphore, u32_at(params, MIGRATE_PAYLOAD)) {
            return NV_ERR_INVALID_ADDRESS;
        }
        first_time(
            &FIRST_MIGRATE,
            "UVM_MIGRATE: the first prefetch of managed memory; it stays in system memory, where every processor reaches it",
        );
        NV_OK
    }
}

static FIRST_MIGRATE: AtomicBool = AtomicBool::new(false);

/// One log line for each managed range a GPU refused to map: the mmap then
/// fails with ENOMEM, and `cudaMallocManaged` with it.
fn managed_refused(base: u64, end: u64, status: u32) {
    zcore_drivers::klog_warn!(
        "[nvidia-uvm] mmap: managed range {:#x}..{:#x} not mapped on a GPU: RM status {:#x}; ENOMEM",
        base,
        end,
        status
    );
}
