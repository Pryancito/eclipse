use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// The two GPUs of the machine under test, as the RM names them.
pub(super) const GPU0: Uuid = [
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
pub(super) const GPU1: Uuid = [
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
];

/// Who a call came from, as the RM was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct By {
    pub os_info: usize,
    pub admin: bool,
    pub gpu_mask: u32,
}

impl By {
    fn of(caller: &nvctl::Caller) -> By {
        By {
            os_info: caller.os_info,
            admin: caller.admin,
            gpu_mask: caller.gpu_mask,
        }
    }
}

/// The RM of the machine under test: [`GPU0`] and [`GPU1`] as RM instances
/// 0 and 1, the VA spaces it is told of, and every call UVM made on it.
#[derive(Default)]
pub(super) struct FakeRm {
    state: spin::Mutex<FakeState>,
}

#[derive(Default)]
pub(super) struct FakeState {
    /// The VA spaces it knows, as (os_info, hClient, hVaSpace, instance),
    /// with the device each hangs from.
    pub va_spaces: BTreeMap<(usize, u32, u32, u32), u32>,
    /// What is mapped, by the handle of its NV50_MEMORY_VIRTUAL.
    pub maps: BTreeMap<u32, (By, nvctl::ExternalMapping)>,
    pub last_virtual: u32,
    /// The status a map in a VA space answers with, for those that fail.
    pub refuse: BTreeMap<u32, u32>,
    /// Every free, as (by, hClient, hObject).
    pub frees: Vec<(By, u32, u32)>,
    /// The pages lent to it and not freed, by the handle of their memory.
    pub lent: BTreeMap<u32, LentPages>,
    pub last_memory: u32,
    /// The status lending pages under a device answers with, for those that
    /// fail.
    pub refuse_pages: BTreeMap<u32, u32>,
    /// The SKED reflected ranges made, by the handle of their
    /// NV50_MEMORY_VIRTUAL, with the handle of their memory.
    pub skeds: BTreeMap<u32, (By, nvctl::SkedRange, u32)>,
    /// The status a SKED reflected range in a VA space answers with, for
    /// those that fail.
    pub refuse_sked: BTreeMap<u32, u32>,
}

/// Pages made RM memory, with the hold the RM keeps on them.
pub(super) struct LentPages {
    pub by: By,
    pub h_client: u32,
    pub h_device: u32,
    pub phys: u64,
    pub size: u64,
    pub gpu_cached: bool,
    pub hold: Arc<dyn Any + Send + Sync>,
}

impl FakeRm {
    pub fn new() -> Arc<FakeRm> {
        Arc::new(FakeRm::default())
    }

    pub fn state(&self) -> spin::MutexGuard<'_, FakeState> {
        self.state.lock()
    }

    /// The mappings made and not freed, in the order they were made.
    pub fn mapped(&self) -> Vec<nvctl::ExternalMapping> {
        self.state().maps.values().map(|&(_, m)| m).collect()
    }
}

impl Rm for FakeRm {
    fn gpus(&self) -> Vec<(Uuid, u32)> {
        vec![(GPU0, 0), (GPU1, 1)]
    }

    fn va_space_device(
        &self,
        by: &nvctl::Caller,
        h_client: u32,
        h_va_space: u32,
        instance: u32,
    ) -> RmResult {
        let key = (by.os_info, h_client, h_va_space, instance);
        self.state().va_spaces.get(&key).copied().ok_or(0x31)
    }

    fn map(&self, by: &nvctl::Caller, m: &nvctl::ExternalMapping) -> RmResult {
        let mut state = self.state();
        if let Some(&status) = state.refuse.get(&m.h_va_space) {
            return Err(status);
        }
        state.last_virtual += 1;
        let h_virtual = 0xcaf0_0000 + state.last_virtual;
        state.maps.insert(h_virtual, (By::of(by), *m));
        Ok(h_virtual)
    }

    fn pages(
        &self,
        by: &nvctl::Caller,
        h_client: u32,
        h_device: u32,
        pages: &nvctl::Pages,
        gpu_cached: bool,
    ) -> RmResult {
        let mut state = self.state();
        if let Some(&status) = state.refuse_pages.get(&h_device) {
            return Err(status);
        }
        state.last_memory += 1;
        let h_memory = 0xbee0_0000 + state.last_memory;
        let lent = LentPages {
            by: By::of(by),
            h_client,
            h_device,
            phys: pages.phys,
            size: pages.size,
            gpu_cached,
            hold: pages.keep.clone(),
        };
        state.lent.insert(h_memory, lent);
        Ok(h_memory)
    }

    fn sked(
        &self,
        by: &nvctl::Caller,
        r: &nvctl::SkedRange,
    ) -> core::result::Result<(u32, u32), u32> {
        let mut state = self.state();
        if let Some(&status) = state.refuse_sked.get(&r.h_va_space) {
            return Err(status);
        }
        state.last_memory += 1;
        let h_memory = 0xbee0_0000 + state.last_memory;
        state.last_virtual += 1;
        let h_virtual = 0xcaf0_0000 + state.last_virtual;
        state.skeds.insert(h_virtual, (By::of(by), *r, h_memory));
        Ok((h_memory, h_virtual))
    }

    fn free(&self, by: &nvctl::Caller, h_client: u32, h_object: u32) {
        let mut state = self.state();
        state.maps.remove(&h_object);
        state.skeds.remove(&h_object);
        let hold = state.lent.remove(&h_object);
        state.frees.push((By::of(by), h_client, h_object));
        drop(state);
        drop(hold);
    }
}

/// An open of `/dev/nvidia-uvm` on its own [`FakeRm`].
fn uvm() -> Arc<UvmOpen> {
    uvm_on(&FakeRm::new())
}

pub(super) fn uvm_on(rm: &Arc<FakeRm>) -> Arc<UvmOpen> {
    let rm: Arc<dyn Rm> = rm.clone();
    Arc::new(UvmOpen::with_rm(1, rm))
}

fn no_fds(_fd: i32) -> Option<Arc<dyn INode>> {
    None
}

fn no_pins(_va: usize, _count: usize, _write: bool) -> LxResult<nvctl::UserPages> {
    Err(LxError::EFAULT)
}

/// Both GPUs, as RM instances 0 and 1.
pub(super) fn both_gpus() -> u32 {
    0b11
}

/// A caller with the fds `fds`, not root, with both GPUs open.
pub(super) fn with_fds(fds: &dyn Fn(i32) -> Option<Arc<dyn INode>>) -> NvCaller<'_> {
    NvCaller {
        admin: false,
        gpu_mask: &both_gpus,
        open_of_fd: fds,
        pin_user_pages: &no_pins,
    }
}

/// [`with_fds`] with no fd at all.
pub(super) fn plain() -> NvCaller<'static> {
    with_fds(&no_fds)
}

pub(super) fn status_at(params: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([params[at], params[at + 1], params[at + 2], params[at + 3]])
}

pub(super) fn initialize(open: &UvmOpen, flags: u64) -> u32 {
    let mut params = vec![0u8; INITIALIZE_SIZE];
    params[..8].copy_from_slice(&flags.to_le_bytes());
    open.call(cmd::INITIALIZE, &mut params, &plain());
    status_at(&params, INITIALIZE_STATUS)
}

fn mm_initialize_on(open: &UvmOpen, fd: i32, fds: &dyn Fn(i32) -> Option<Arc<dyn INode>>) -> u32 {
    let mut params = vec![0u8; MM_INITIALIZE_SIZE];
    params[..4].copy_from_slice(&fd.to_le_bytes());
    open.call(cmd::MM_INITIALIZE, &mut params, &with_fds(fds));
    status_at(&params, MM_INITIALIZE_STATUS)
}

/// The pageableMemAccess byte and the status, with the byte coming in as
/// `came`.
fn pageable_mem_access(open: &UvmOpen, came: u8) -> (u8, u32) {
    let mut params = vec![0u8; PAGEABLE_MEM_ACCESS_SIZE];
    params[0] = came;
    open.call(cmd::PAGEABLE_MEM_ACCESS, &mut params, &plain());
    (params[0], status_at(&params, PAGEABLE_MEM_ACCESS_STATUS))
}

/// UVM_REGISTER_GPU as libcuda asks it (rmCtrlFd -1, no client, no
/// partition), with the NUMA outputs coming in as 0xAA: the params after
/// the call.
fn register_gpu_params(open: &UvmOpen, uuid: &Uuid) -> Vec<u8> {
    let mut params = vec![0u8; REGISTER_GPU_SIZE];
    params[..16].copy_from_slice(uuid);
    params[REGISTER_GPU_NUMA_ENABLED..REGISTER_GPU_NUMA_NODE_ID + 4].fill(0xAA);
    params[24..28].copy_from_slice(&(-1i32).to_le_bytes());
    open.call(cmd::REGISTER_GPU, &mut params, &plain());
    params
}

pub(super) fn register_gpu(open: &UvmOpen, uuid: &Uuid) -> u32 {
    status_at(&register_gpu_params(open, uuid), REGISTER_GPU_STATUS)
}

pub(super) fn unregister_gpu(open: &UvmOpen, uuid: &Uuid) -> u32 {
    let mut params = vec![0u8; UNREGISTER_GPU_SIZE];
    params[..16].copy_from_slice(uuid);
    open.call(cmd::UNREGISTER_GPU, &mut params, &plain());
    status_at(&params, UNREGISTER_GPU_STATUS)
}

/// The id and the status.
fn create_range_group(open: &UvmOpen) -> (u64, u32) {
    let mut params = vec![0u8; RANGE_GROUP_SIZE];
    open.call(cmd::CREATE_RANGE_GROUP, &mut params, &plain());
    (u64_at(&params, 0), status_at(&params, RANGE_GROUP_STATUS))
}

fn destroy_range_group(open: &UvmOpen, id: u64) -> u32 {
    let mut params = vec![0u8; RANGE_GROUP_SIZE];
    params[..8].copy_from_slice(&id.to_le_bytes());
    open.call(cmd::DESTROY_RANGE_GROUP, &mut params, &plain());
    status_at(&params, RANGE_GROUP_STATUS)
}

/// What libcuda's cuInit does (traced against libcuda 580.178.04): open
/// twice, UVM_INITIALIZE with no flags on the first, UVM_MM_INITIALIZE on
/// the second naming the first, then UVM_PAGEABLE_MEM_ACCESS on the first;
/// a failure among them is its 999. Then, with the devices, UVM_REGISTER_GPU
/// for each and one UVM_CREATE_RANGE_GROUP, on the first; a failure there is
/// its 3. Each answer is one cuInit accepts.
#[test]
fn cuinit_gets_the_answers_it_goes_on_with() {
    let va_space = uvm();
    let mm = uvm();
    let va_space_inode: Arc<dyn INode> = va_space.clone();
    let fds = move |fd: i32| (fd == 10).then(|| va_space_inode.clone());
    assert_eq!(initialize(&va_space, 0), NV_OK);
    assert_eq!(mm_initialize_on(&mm, 10, &fds), NV_WARN_NOTHING_TO_DO);
    assert_eq!(pageable_mem_access(&va_space, 0), (0, NV_OK));
    assert_eq!(register_gpu(&va_space, &GPU0), NV_OK);
    assert_eq!(register_gpu(&va_space, &GPU1), NV_OK);
    assert_eq!(create_range_group(&va_space), (1, NV_OK));
}

/// The UUID goes back as it came, and the GPU is no NUMA node: numaEnabled
/// false and numaNodeId -1, whatever came in.
#[test]
fn a_registered_gpu_is_not_a_numa_node() {
    let open = uvm();
    assert_eq!(initialize(&open, 0), NV_OK);
    let params = register_gpu_params(&open, &GPU1);
    assert_eq!(status_at(&params, REGISTER_GPU_STATUS), NV_OK);
    assert_eq!(params[..16], GPU1);
    assert_eq!(params[REGISTER_GPU_NUMA_ENABLED], 0);
    assert_eq!(i32_at(&params, REGISTER_GPU_NUMA_NODE_ID), -1);
}

/// `uvm_gpu_retain_by_uuid` finds no GPU the RM did not bring up, and the
/// outputs go back as they came.
#[test]
fn a_gpu_the_rm_does_not_know_is_not_found() {
    let open = uvm();
    assert_eq!(initialize(&open, 0), NV_OK);
    let mut stranger = GPU0;
    stranger[15] ^= 1;
    let params = register_gpu_params(&open, &stranger);
    assert_eq!(
        status_at(&params, REGISTER_GPU_STATUS),
        NV_ERR_GPU_UUID_NOT_FOUND
    );
    assert_eq!(params[..16], stranger);
    assert_eq!(params[REGISTER_GPU_NUMA_ENABLED], 0xAA);
    assert_eq!(
        i32_at(&params, REGISTER_GPU_NUMA_NODE_ID),
        0xAAAA_AAAAu32 as i32
    );
    // And it was not registered: there is nothing to unregister.
    assert_eq!(unregister_gpu(&open, &stranger), NV_ERR_INVALID_DEVICE);
}

/// Once per VA space, and the other GPU is not taken with it.
#[test]
fn a_gpu_is_registered_once_per_va_space() {
    let a = uvm();
    let b = uvm();
    assert_eq!(initialize(&a, 0), NV_OK);
    assert_eq!(initialize(&b, 0), NV_OK);
    assert_eq!(register_gpu(&a, &GPU0), NV_OK);
    assert_eq!(register_gpu(&a, &GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(register_gpu(&a, &GPU1), NV_OK);
    assert_eq!(register_gpu(&b, &GPU0), NV_OK);
}

/// `uvm_va_space_unregister_gpu` only knows the GPUs registered in its own
/// VA space; once unregistered, a GPU can be registered again.
#[test]
fn unregister_takes_back_only_a_gpu_registered_here() {
    let a = uvm();
    let b = uvm();
    assert_eq!(initialize(&a, 0), NV_OK);
    assert_eq!(initialize(&b, 0), NV_OK);
    assert_eq!(unregister_gpu(&a, &GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(register_gpu(&a, &GPU0), NV_OK);
    assert_eq!(register_gpu(&a, &GPU1), NV_OK);
    assert_eq!(unregister_gpu(&b, &GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(unregister_gpu(&a, &GPU0), NV_OK);
    assert_eq!(unregister_gpu(&a, &GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(register_gpu(&a, &GPU1), NV_ERR_INVALID_DEVICE);
    assert_eq!(register_gpu(&a, &GPU0), NV_OK);
}

/// Ids count up from 1 in each VA space and are never handed out twice,
/// even after their group is destroyed; a group is destroyed once, and only
/// in its own VA space.
#[test]
fn range_groups_count_up_and_are_destroyed_once() {
    let a = uvm();
    let b = uvm();
    assert_eq!(initialize(&a, 0), NV_OK);
    assert_eq!(initialize(&b, 0), NV_OK);
    assert_eq!(create_range_group(&a), (1, NV_OK));
    assert_eq!(create_range_group(&a), (2, NV_OK));
    assert_eq!(create_range_group(&b), (1, NV_OK));
    assert_eq!(destroy_range_group(&b, 2), NV_ERR_OBJECT_NOT_FOUND);
    assert_eq!(destroy_range_group(&a, 2), NV_OK);
    assert_eq!(destroy_range_group(&a, 2), NV_ERR_OBJECT_NOT_FOUND);
    assert_eq!(destroy_range_group(&a, 0), NV_ERR_OBJECT_NOT_FOUND);
    assert_eq!(create_range_group(&a), (3, NV_OK));
    assert_eq!(destroy_range_group(&a, 1), NV_OK);
    assert_eq!(destroy_range_group(&b, 1), NV_OK);
}

/// The INIT_CHECK of the module's route: before UVM_INITIALIZE all four are
/// NV_ERR_ILLEGAL_ACTION, nothing is registered or handed out, and the
/// params go back untouched.
#[test]
fn gpus_and_range_groups_need_a_va_space() {
    let open = uvm();
    let params = register_gpu_params(&open, &GPU0);
    assert_eq!(
        status_at(&params, REGISTER_GPU_STATUS),
        NV_ERR_ILLEGAL_ACTION
    );
    assert_eq!(params[REGISTER_GPU_NUMA_ENABLED], 0xAA);
    assert_eq!(unregister_gpu(&open, &GPU0), NV_ERR_ILLEGAL_ACTION);
    let mut params = vec![0x55u8; RANGE_GROUP_SIZE];
    open.call(cmd::CREATE_RANGE_GROUP, &mut params, &plain());
    assert_eq!(
        status_at(&params, RANGE_GROUP_STATUS),
        NV_ERR_ILLEGAL_ACTION
    );
    assert_eq!(u64_at(&params, 0), 0x5555_5555_5555_5555);
    assert_eq!(destroy_range_group(&open, 1), NV_ERR_ILLEGAL_ACTION);
    // The open is still uninitialised and empty: it can become a VA space
    // and then get both.
    assert_eq!(initialize(&open, 0), NV_OK);
    assert_eq!(register_gpu(&open, &GPU0), NV_OK);
    assert_eq!(create_range_group(&open), (1, NV_OK));
}

/// The UUID in the log is the one `nvidia-smi -L` prints.
#[test]
fn the_log_writes_a_uuid_as_nvidia_smi_does() {
    assert_eq!(
        alloc::format!("{}", ShowUuid(&GPU0)),
        "GPU-10111213-1415-1617-1819-1a1b1c1d1e1f"
    );
}

#[test]
fn initializing_again_with_the_same_flags_changes_nothing() {
    let open = uvm();
    assert_eq!(initialize(&open, 0x1), NV_OK);
    assert_eq!(initialize(&open, 0x1), NV_OK);
    assert_eq!(*open.fd_type.lock(), FdType::VaSpace { flags: 0x1 });
}

#[test]
fn initializing_again_with_other_flags_is_refused_and_keeps_the_first() {
    let open = uvm();
    assert_eq!(initialize(&open, 0x2), NV_OK);
    assert_eq!(initialize(&open, 0x0), NV_ERR_INVALID_ARGUMENT);
    assert_eq!(*open.fd_type.lock(), FdType::VaSpace { flags: 0x2 });
}

/// `uvm_va_space_create` refuses a flag outside UVM_INIT_FLAGS_MASK, and the
/// open is left as it was, so a later good UVM_INITIALIZE still works.
#[test]
fn an_unknown_flag_is_refused_and_leaves_the_open_uninitialized() {
    let open = uvm();
    assert_eq!(initialize(&open, 0x4), NV_ERR_INVALID_ARGUMENT);
    assert_eq!(*open.fd_type.lock(), FdType::Uninitialized);
    assert_eq!(initialize(&open, 0x3), NV_OK);
}

/// The INIT_CHECK of the module's route: before UVM_INITIALIZE it is
/// NV_ERR_ILLEGAL_ACTION and the params go back untouched.
#[test]
fn pageable_mem_access_needs_a_va_space() {
    let open = uvm();
    assert_eq!(pageable_mem_access(&open, 1), (1, NV_ERR_ILLEGAL_ACTION));
}

/// No HMM and no ATS: false, whatever came in.
#[test]
fn pageable_mem_access_is_false() {
    let open = uvm();
    assert_eq!(initialize(&open, 0), NV_OK);
    assert_eq!(pageable_mem_access(&open, 1), (0, NV_OK));
}

#[test]
fn mm_initialize_wants_an_initialized_open_of_this_file() {
    let mm = uvm();
    let uninitialized: Arc<dyn INode> = uvm();
    let other: Arc<dyn INode> = Arc::new(UvmDev::new());
    let fds = move |fd: i32| match fd {
        3 => Some(uninitialized.clone()),
        4 => Some(other.clone()),
        _ => None,
    };
    assert_eq!(mm_initialize_on(&mm, 3, &fds), NV_ERR_INVALID_ARGUMENT);
    assert_eq!(mm_initialize_on(&mm, 4, &fds), NV_ERR_INVALID_ARGUMENT);
    assert_eq!(mm_initialize_on(&mm, 5, &fds), NV_ERR_INVALID_ARGUMENT);
    assert_eq!(mm_initialize_on(&mm, -1, &fds), NV_ERR_INVALID_ARGUMENT);
}

/// The open UVM_MM_INITIALIZE is issued on stays what it was: it can still
/// become a VA space of its own.
#[test]
fn mm_initialize_leaves_its_own_open_alone() {
    let va_space = uvm();
    let mm = uvm();
    let va_space_inode: Arc<dyn INode> = va_space.clone();
    let fds = move |_fd: i32| Some(va_space_inode.clone());
    assert_eq!(initialize(&va_space, 0), NV_OK);
    assert_eq!(mm_initialize_on(&mm, 7, &fds), NV_WARN_NOTHING_TO_DO);
    assert_eq!(*mm.fd_type.lock(), FdType::Uninitialized);
    assert_eq!(initialize(&mm, 0), NV_OK);
}

/// Neither touches user memory: UVM_DEINITIALIZE is a no-op, and a command
/// not served is ENOSYS before anything is copied.
#[test]
fn deinitialize_succeeds_and_the_rest_is_enosys() {
    let open = uvm();
    assert_eq!(open.ioctl(cmd::DEINITIALIZE, 0, &plain()), Ok(0));
    let unserved: Vec<u32> = vec![31, 35, 69, 71, 74, 2047, 0x3000_0003];
    for cmd in unserved {
        assert_eq!(
            open.ioctl(cmd, 0, &plain()),
            Err(LxError::ENOSYS),
            "{}",
            cmd
        );
    }
}

/// The names the log uses are the module's.
#[test]
fn the_log_names_the_command() {
    assert_eq!(name(37), "UVM_REGISTER_GPU");
    assert_eq!(name(25), "UVM_REGISTER_GPU_VASPACE");
    assert_eq!(name(33), "UVM_MAP_EXTERNAL_ALLOCATION");
    assert_eq!(name(68), "UVM_ALLOC_SEMAPHORE_POOL");
    assert_eq!(name(cmd::INITIALIZE), "UVM_INITIALIZE");
    assert_eq!(name(cmd::MM_INITIALIZE), "UVM_MM_INITIALIZE");
    assert_eq!(name(2047), "unknown");
}

#[test]
fn the_node_is_a_character_device_anyone_can_open() {
    let dev = UvmDev::new();
    let meta = dev.metadata().unwrap();
    assert_eq!(meta.type_, FileType::CharDevice);
    assert_eq!(meta.mode, 0o666);
    assert_eq!(meta.rdev, make_rdev(MAJOR, 0));
    // Every open is its own.
    let a = dev.open_client();
    let b = dev.open_client();
    assert!(!Arc::ptr_eq(&a, &b));
    assert!(a.downcast_ref::<UvmOpen>().is_some());
}
