use super::*;
use alloc::vec;
use alloc::vec::Vec;

fn uvm() -> Arc<UvmOpen> {
    Arc::new(UvmOpen::new(1))
}

fn no_fds(_fd: i32) -> Option<Arc<dyn INode>> {
    None
}

fn status_at(params: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([params[at], params[at + 1], params[at + 2], params[at + 3]])
}

fn initialize(open: &UvmOpen, flags: u64) -> u32 {
    let mut params = vec![0u8; INITIALIZE_SIZE];
    params[..8].copy_from_slice(&flags.to_le_bytes());
    open.call(cmd::INITIALIZE, &mut params, &no_fds);
    status_at(&params, INITIALIZE_STATUS)
}

fn mm_initialize_on(open: &UvmOpen, fd: i32, fds: &dyn Fn(i32) -> Option<Arc<dyn INode>>) -> u32 {
    let mut params = vec![0u8; MM_INITIALIZE_SIZE];
    params[..4].copy_from_slice(&fd.to_le_bytes());
    open.call(cmd::MM_INITIALIZE, &mut params, fds);
    status_at(&params, MM_INITIALIZE_STATUS)
}

/// The pageableMemAccess byte and the status, with the byte coming in as
/// `came`.
fn pageable_mem_access(open: &UvmOpen, came: u8) -> (u8, u32) {
    let mut params = vec![0u8; PAGEABLE_MEM_ACCESS_SIZE];
    params[0] = came;
    open.call(cmd::PAGEABLE_MEM_ACCESS, &mut params, &no_fds);
    (params[0], status_at(&params, PAGEABLE_MEM_ACCESS_STATUS))
}

/// What libcuda's cuInit does (traced against libcuda 580.178.04): open
/// twice, UVM_INITIALIZE with no flags on the first, UVM_MM_INITIALIZE on
/// the second naming the first, then UVM_PAGEABLE_MEM_ACCESS on the first.
/// Each answer is one cuInit accepts; any failure among them is its 999.
#[test]
fn cuinit_gets_the_answers_it_goes_on_with() {
    let va_space = uvm();
    let mm = uvm();
    let va_space_inode: Arc<dyn INode> = va_space.clone();
    let fds = move |fd: i32| (fd == 10).then(|| va_space_inode.clone());
    assert_eq!(initialize(&va_space, 0), NV_OK);
    assert_eq!(mm_initialize_on(&mm, 10, &fds), NV_WARN_NOTHING_TO_DO);
    assert_eq!(pageable_mem_access(&va_space, 0), (0, NV_OK));
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
    assert_eq!(open.ioctl(cmd::DEINITIALIZE, 0, &no_fds), Ok(0));
    let unserved: Vec<u32> = vec![37, 25, 27, 33, 68, 2047, 0x3000_0003];
    for cmd in unserved {
        assert_eq!(open.ioctl(cmd, 0, &no_fds), Err(LxError::ENOSYS), "{}", cmd);
    }
}

/// The names the log uses are the module's.
#[test]
fn the_log_names_the_command() {
    assert_eq!(name(37), "UVM_REGISTER_GPU");
    assert_eq!(name(25), "UVM_REGISTER_GPU_VASPACE");
    assert_eq!(name(33), "UVM_MAP_EXTERNAL_ALLOCATION");
    assert_eq!(name(68), "UVM_ALLOC_SEMAPHORE_POOL");
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
