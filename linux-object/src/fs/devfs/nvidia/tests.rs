use super::*;
use alloc::vec;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// `_IOWR('F', nr, size)`, the way libnvidia-ml builds its commands.
fn iowr(nr: u32, size: usize) -> u32 {
    0xC000_0000 | ((size as u32) << 16) | (u32::from(b'F') << 8) | nr
}

fn no_fds(_fd: i32) -> Option<Arc<dyn INode>> {
    None
}

fn no_gpus() -> u32 {
    0
}

fn no_pins(_va: usize, _count: usize, _write: bool) -> LxResult<nvctl::UserPages> {
    Err(LxError::EFAULT)
}

const NOBODY: NvCaller<'static> = NvCaller {
    admin: false,
    gpu_mask: &no_gpus,
    open_of_fd: &no_fds,
    pin_user_pages: &no_pins,
};

fn ctl() -> NvidiaOpen {
    NvidiaOpen::new(Node::Ctl, 1)
}

fn gpu(minor: u32) -> NvidiaOpen {
    NvidiaOpen::new(Node::Gpu(minor), 2)
}

fn card(minor: u32, bus: u8, instance: Option<u32>) -> nvctl::Card {
    nvctl::Card {
        minor,
        domain: 0,
        bus,
        slot: 0,
        device_id: 0x1f06,
        reg_address: 0xfb00_0000,
        reg_size: 16 << 20,
        fb_address: 0xd000_0000,
        fb_size: 256 << 20,
        instance,
    }
}

/// The real commands: libnvidia-ml's CHECK_VERSION_STR and XFER.
#[test]
fn the_command_is_read_for_its_nr_and_its_size() {
    assert_eq!(decode(0xC048_46D2), (esc::CHECK_VERSION_STR, 72));
    assert_eq!(decode(0xC010_46D3), (esc::IOCTL_XFER_CMD, 16));
    // Neither the type nor the direction is looked at, as in nv.c.
    assert_eq!(decode(0x0048_00D2), (esc::CHECK_VERSION_STR, 72));
}

#[test]
fn an_xfer_names_the_escape_its_size_and_its_pointer() {
    let mut b = [0u8; XFER_SIZE];
    put_u32(&mut b, 0, esc::CARD_INFO);
    put_u32(&mut b, 4, 32 * CARD_INFO_SIZE as u32);
    put_u64(&mut b, 8, 0x7fff_1234_5678);
    assert_eq!(
        parse_xfer(&b),
        (esc::CARD_INFO, 32 * CARD_INFO_SIZE, 0x7fff_1234_5678)
    );
}

/// Every field at the offset nv-ioctl.h gives it on x86_64.
#[test]
fn a_card_is_laid_out_as_nv_ioctl_card_info() {
    let mut p = vec![0xaau8; 2 * CARD_INFO_SIZE + 5];
    assert_eq!(
        card_info(&mut p, &[(card(1, 0x2d, Some(0)), 0x2d00)]),
        Ok(())
    );
    let e = &p[..CARD_INFO_SIZE];
    assert_eq!(e[0], 1, "valid");
    assert_eq!(u32_at(e, 4), 0, "domain");
    assert_eq!((e[8], e[9], e[10]), (0x2d, 0, 0), "bus, slot, function");
    assert_eq!(u16_at(e, 12), 0x10de);
    assert_eq!(u16_at(e, 14), 0x1f06);
    assert_eq!(u32_at(e, 16), 0x2d00, "gpu_id");
    assert_eq!(u16_at(e, 20), 0, "interrupt_line");
    assert_eq!(u64_at(e, 24), 0xfb00_0000);
    assert_eq!(u64_at(e, 32), 16 << 20);
    assert_eq!(u64_at(e, 40), 0xd000_0000);
    assert_eq!(u64_at(e, 48), 256 << 20);
    assert_eq!(u32_at(e, 56), 1, "minor_number");
    assert!(e[60..].iter().all(|&b| b == 0), "dev_name and padding");
    assert!(
        p[CARD_INFO_SIZE..2 * CARD_INFO_SIZE]
            .iter()
            .all(|&b| b == 0),
        "an entry with no GPU is cleared: valid = 0"
    );
    assert_eq!(
        &p[2 * CARD_INFO_SIZE..],
        &[0xaa; 5],
        "a partial entry is not touched"
    );
}

/// Fewer entries than GPUs is EINVAL, after the entries there are were
/// cleared; libnvidia-ml then asks again with room for `NV_MAX_DEVICES`.
#[test]
fn too_few_entries_for_the_gpus_is_einval() {
    let two = [(card(0, 1, Some(0)), 1), (card(1, 2, Some(1)), 2)];
    let mut p = vec![0xaau8; CARD_INFO_SIZE];
    assert_eq!(card_info(&mut p, &two), Err(LxError::EINVAL));
    assert!(p.iter().all(|&b| b == 0));
    let mut none: [u8; 0] = [];
    assert_eq!(card_info(&mut none, &[]), Ok(()), "no GPUs fit in nothing");
}

fn version_payload(cmd: u32, s: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; RM_API_VERSION_SIZE];
    put_u32(&mut p, 0, cmd);
    p[8..8 + s.len()].copy_from_slice(s);
    p
}

const RM: &[u8] = b"580.178.04";

#[test]
fn the_version_check_says_it_recognised_the_request() {
    let mut p = version_payload(0, RM);
    assert_eq!(check_version(&mut p, RM), Ok(()));
    assert_eq!(u32_at(&p, 4), nvctl::VERSION_REPLY_RECOGNIZED);
    assert_eq!(&p[8..8 + RM.len()], RM);
}

/// A mismatch is EINVAL and the client is told which version the kernel
/// has: that is how `nvidia-smi` gets to "Driver/library version mismatch".
#[test]
fn a_mismatch_is_einval_and_answers_with_the_rm_version() {
    let mut p = version_payload(0, b"575.64.05");
    assert_eq!(check_version(&mut p, RM), Err(LxError::EINVAL));
    assert_eq!(u32_at(&p, 4), nvctl::VERSION_REPLY_RECOGNIZED);
    assert_eq!(&p[8..8 + RM.len()], RM);
    assert!(p[8 + RM.len()..].iter().all(|&b| b == 0));
}

#[test]
fn a_query_and_a_relaxed_match_both_pass() {
    let mut p = version_payload(nvctl::VERSION_CMD_QUERY, b"");
    assert_eq!(check_version(&mut p, RM), Ok(()));
    assert_eq!(&p[8..8 + RM.len()], RM);
    let mut p = version_payload(u32::from(b'1'), b"580.95.05");
    assert_eq!(check_version(&mut p, RM), Ok(()));
}

#[test]
fn a_version_payload_of_another_size_is_einval() {
    let mut p = vec![0u8; RM_API_VERSION_SIZE - 1];
    assert_eq!(check_version(&mut p, RM), Err(LxError::EINVAL));
}

#[test]
fn the_memblock_size_is_set_once() {
    let cell = AtomicU64::new(0);
    let size = |v: u64| v.to_le_bytes();
    assert_eq!(sys_params(&size(1 << 27), &cell), Ok(()));
    assert_eq!(sys_params(&size(1 << 27), &cell), Ok(()), "the same again");
    assert_eq!(sys_params(&size(1 << 28), &cell), Err(LxError::EBUSY));
    assert_eq!(cell.load(Ordering::SeqCst), 1 << 27);
    assert_eq!(sys_params(&[0; 4], &cell), Err(LxError::EINVAL));
}

#[test]
fn the_status_code_is_the_adapter_status_at_that_location() {
    let cards = [card(0, 0x2d, Some(0)), card(1, 0x2e, None)];
    let ask = |bus: u8| {
        let mut p = vec![0xffu8; STATUS_CODE_SIZE];
        put_u32(&mut p, 0, 0);
        p[4] = bus;
        p[5] = 0;
        status_code(&mut p, &cards).map(|()| u32_at(&p, 8))
    };
    assert_eq!(ask(0x2d), Ok(NV_OK));
    assert_eq!(
        ask(0x2e),
        Ok(NV_ERR_NOT_READY),
        "probed, but the RM never brought it up"
    );
    assert_eq!(ask(0x2f), Err(LxError::EINVAL), "no GPU there");
}

/// The whole ioctl, through user copies (the host build's user half is the
/// whole address space).
fn ioctl_on(open: &NvidiaOpen, nr: u32, payload: &mut [u8]) -> LxResult<usize> {
    open.ioctl(
        iowr(nr, payload.len()),
        payload.as_mut_ptr() as usize,
        &NOBODY,
    )
}

#[test]
fn wait_open_complete_reports_an_open_that_succeeded() {
    let mut p = [0xffu8; WAIT_OPEN_COMPLETE_SIZE];
    assert_eq!(ioctl_on(&gpu(0), esc::WAIT_OPEN_COMPLETE, &mut p), Ok(0));
    assert_eq!(p, [0; WAIT_OPEN_COMPLETE_SIZE]);
    let mut p = [0u8; 4];
    assert_eq!(
        ioctl_on(&gpu(0), esc::WAIT_OPEN_COMPLETE, &mut p),
        Err(LxError::EINVAL)
    );
}

/// `NV_CTL_DEVICE_ONLY`: on `/dev/nvidia<N>` these are EINVAL, and the
/// payload goes back as it came.
#[test]
fn control_escapes_are_refused_on_a_gpu_node() {
    for nr in [
        esc::CARD_INFO,
        esc::CHECK_VERSION_STR,
        esc::SYS_PARAMS,
        esc::ATTACH_GPUS_TO_FD,
    ] {
        let mut p = vec![0x5au8; 72];
        assert_eq!(
            ioctl_on(&gpu(0), nr, &mut p),
            Err(LxError::EINVAL),
            "{:#x}",
            nr
        );
        assert!(p.iter().all(|&b| b == 0x5a));
    }
    let mut p = vec![0x5au8; STATUS_CODE_SIZE];
    assert_eq!(
        ioctl_on(&gpu(0), esc::STATUS_CODE, &mut p),
        Err(LxError::EINVAL)
    );
}

/// NV_ESC_NUMA_INFO, as Linux answers it for a GPU on PCIe: no node, NUMA
/// disabled, nothing onlined and no retired page, with the memblock size
/// SYS_PARAMS set. The 64 addresses and the padding go back as they came.
#[test]
fn numa_info_reports_a_gpu_without_numa_memory() {
    let mut p = vec![0x5au8; NUMA_INFO_SIZE];
    put_u32(&mut p, NUMA_INFO_OFFLINE_COUNT, 64);
    assert_eq!(numa_info(&mut p, 0x800_0000), Ok(()));
    assert_eq!(u32_at(&p, 0) as i32, -1, "nid");
    assert_eq!(u32_at(&p, 4), 0, "status: disabled");
    assert_eq!(u64_at(&p, 8), 0x800_0000, "memblock_size");
    assert_eq!(u64_at(&p, 16), 0, "numa_mem_addr");
    assert_eq!(u64_at(&p, 24), 0, "numa_mem_size");
    assert_eq!(p[32], 0, "use_auto_online");
    assert_eq!(u32_at(&p, NUMA_INFO_OFFLINE_COUNT), 0, "retired pages");
    assert!(p[33..NUMA_INFO_OFFLINE_COUNT].iter().all(|&b| b == 0x5a));
    assert!(p[NUMA_INFO_OFFLINE_COUNT + 4..].iter().all(|&b| b == 0x5a));
}

/// What nv.c refuses: room for more retired pages than the RM can list is
/// EBUSY, a payload of another size EINVAL, and so is the control device
/// (`NV_ACTUAL_DEVICE_ONLY`), which a GPU node is not.
#[test]
fn numa_info_is_refused_where_linux_refuses_it() {
    let mut p = vec![0x5au8; NUMA_INFO_SIZE];
    put_u32(&mut p, NUMA_INFO_OFFLINE_COUNT, 65);
    assert_eq!(numa_info(&mut p, 0), Err(LxError::EBUSY));
    assert_eq!(u32_at(&p, 0), 0x5a5a_5a5a, "nothing answered");
    let mut short = vec![0u8; NUMA_INFO_SIZE - 8];
    assert_eq!(numa_info(&mut short, 0), Err(LxError::EINVAL));

    let mut p = vec![0u8; NUMA_INFO_SIZE];
    assert_eq!(
        ioctl_on(&ctl(), esc::NUMA_INFO, &mut p),
        Err(LxError::EINVAL)
    );
    assert_eq!(u32_at(&p, 0), 0, "the control device answered nothing");
    assert_eq!(ioctl_on(&gpu(0), esc::NUMA_INFO, &mut p), Ok(0));
    assert_eq!(u32_at(&p, 0) as i32, -1);
    assert_eq!(u64_at(&p, 8), MEMBLOCK_SIZE.load(Ordering::SeqCst));
    assert!(!logged_not_served(esc::NUMA_INFO));
}

/// The payload is copied back even when the escape fails: the version check
/// that refuses a client still hands it the kernel's version.
#[test]
fn a_failed_escape_still_copies_its_payload_back() {
    let mut p = version_payload(0, b"575.64.05");
    // Hosted, the RM has no version at all, so nothing matches.
    assert_eq!(
        ioctl_on(&ctl(), esc::CHECK_VERSION_STR, &mut p),
        Err(LxError::EINVAL)
    );
    assert_eq!(u32_at(&p, 4), nvctl::VERSION_REPLY_RECOGNIZED);
    // `os_string_copy`, a strcpy: the RM's string and its NUL, which on the
    // host is the NUL alone.
    assert_eq!(p[8], 0, "replaced with the RM's string");
}

#[test]
fn an_xfer_carries_the_escape_to_its_own_payload() {
    let mut inner = version_payload(nvctl::VERSION_CMD_QUERY, b"x");
    let mut xfer = [0u8; XFER_SIZE];
    put_u32(&mut xfer, 0, esc::CHECK_VERSION_STR);
    put_u32(&mut xfer, 4, inner.len() as u32);
    put_u64(&mut xfer, 8, inner.as_mut_ptr() as u64);
    assert_eq!(ioctl_on(&ctl(), esc::IOCTL_XFER_CMD, &mut xfer), Ok(0));
    assert_eq!(u32_at(&inner, 4), nvctl::VERSION_REPLY_RECOGNIZED);
    assert_eq!(inner[8], 0, "the query was answered with the RM's string");
}

#[test]
fn an_xfer_of_the_wrong_shape_or_too_big_is_einval() {
    // A whole XFER naming an escape that would succeed, under a command that
    // does not say 16 bytes: refused before it is read, so the escape it
    // names never runs.
    let mut inner = version_payload(nvctl::VERSION_CMD_QUERY, b"x");
    let mut shaped = [0u8; 2 * XFER_SIZE];
    put_u32(&mut shaped, 0, esc::CHECK_VERSION_STR);
    put_u32(&mut shaped, 4, inner.len() as u32);
    put_u64(&mut shaped, 8, inner.as_mut_ptr() as u64);
    for size in [XFER_SIZE / 2, 2 * XFER_SIZE] {
        assert_eq!(
            ctl().ioctl(
                iowr(esc::IOCTL_XFER_CMD, size),
                shaped.as_mut_ptr() as usize,
                &NOBODY
            ),
            Err(LxError::EINVAL),
            "an XFER command of {size} bytes"
        );
    }
    assert_eq!(u32_at(&inner, 4), 0, "the escape it names never ran");
    let mut xfer = [0u8; XFER_SIZE];
    put_u32(&mut xfer, 0, esc::CARD_INFO);
    put_u32(&mut xfer, 4, (MAX_IOCTL_SIZE + 1) as u32);
    assert_eq!(
        ioctl_on(&ctl(), esc::IOCTL_XFER_CMD, &mut xfer),
        Err(LxError::EINVAL)
    );
}

#[test]
fn a_payload_nobody_can_read_is_efault() {
    let open = ctl();
    assert_eq!(
        open.ioctl(iowr(esc::CARD_INFO, CARD_INFO_SIZE), 0, &NOBODY),
        Err(LxError::EFAULT)
    );
}

/// What this kernel does not serve yet is EINVAL, never ENOTTY: libnvidia-ml
/// reads the failure, it does not take the fd for something else.
#[test]
fn escapes_not_served_are_einval() {
    for nr in [
        0x4A, /* RM_VID_HEAP_CONTROL */
        206,  /* ALLOC_OS_EVENT */
        213, 0x99,
    ] {
        let mut p = [0u8; 16];
        assert_eq!(
            ioctl_on(&ctl(), nr, &mut p),
            Err(LxError::EINVAL),
            "{:#x}",
            nr
        );
    }
    // Hosted there is no RM behind the RM escapes either, but they are
    // served: refused for want of an RM, not logged as escapes to write.
    for nr in [
        esc::RM_CONTROL,
        esc::RM_MAP_MEMORY,
        esc::RM_UNMAP_MEMORY,
        0x27, /* RM_ALLOC_MEMORY */
        0x5E, /* RM_UPDATE_DEVICE_MAPPING_INFO */
    ] {
        let mut p = [0u8; 56];
        assert_eq!(
            ioctl_on(&ctl(), nr, &mut p),
            Err(LxError::EINVAL),
            "{nr:#x}"
        );
        assert!(!logged_not_served(nr), "{:#x} is served", nr);
    }
    assert!(logged_not_served(0x4A));
}

fn logged_not_served(nr: u32) -> bool {
    let bit = (nr & 0xff) as usize;
    NOT_SERVED[bit / 64].load(Ordering::Relaxed) & (1 << (bit % 64)) != 0
}

#[test]
fn gpus_are_attached_once_and_only_known_ones() {
    let open = ctl();
    assert_eq!(open.attach_gpus(&[]), Err(LxError::EINVAL));
    assert_eq!(open.attach_gpus(&[0; 6]), Err(LxError::EINVAL));
    // No GPU is listed on the host, so any non-zero id is unknown, and the
    // refusal leaves room to try again.
    assert_eq!(open.attach_gpus(&7u32.to_le_bytes()), Err(LxError::EINVAL));
    assert!(open.state.lock().attached_gpus.is_none());
    // A zero id is skipped, as `nvidia_dev_get` is never asked about it.
    assert_eq!(open.attach_gpus(&[0; 8]), Ok(()));
    assert_eq!(open.attach_gpus(&[0; 4]), Err(LxError::EINVAL), "once");
}

fn caller_with_fd<'a>(fd_table: &'a dyn Fn(i32) -> Option<Arc<dyn INode>>) -> NvCaller<'a> {
    NvCaller {
        admin: false,
        gpu_mask: &no_gpus,
        open_of_fd: fd_table,
        pin_user_pages: &no_pins,
    }
}

#[test]
fn a_gpu_open_registers_its_control_fd_once() {
    let target: Arc<dyn INode> = Arc::new(ctl());
    let lookup = |fd: i32| (fd == 3).then(|| target.clone());
    let caller = caller_with_fd(&lookup);
    let open = gpu(0);
    let before = Arc::strong_count(&target);
    assert_eq!(open.register_fd(3, &caller), Ok(()));
    assert_eq!(
        Arc::strong_count(&target),
        before + 1,
        "the control open is held for as long as this one lives"
    );
    assert_eq!(open.register_fd(3, &caller), Err(LxError::EINVAL), "once");
    drop(open);
    assert_eq!(Arc::strong_count(&target), before);
}

/// `RmIoctl`'s `clientOSInfo`: once linked, a GPU open calls the RM as the
/// control open its clients were made through, and still as a GPU open.
#[test]
fn a_linked_open_calls_the_rm_as_its_control_open() {
    let target: Arc<dyn INode> = Arc::new(ctl());
    let ctl_os_info = target.downcast_ref::<NvidiaOpen>().unwrap().os_info();
    let lookup = |fd: i32| (fd == 3).then(|| target.clone());
    let caller = caller_with_fd(&lookup);
    let open = gpu(0);
    assert_eq!(open.rm_caller(&caller).os_info, open.os_info(), "unlinked");
    assert_eq!(open.register_fd(3, &caller), Ok(()));
    let rm = open.rm_caller(&caller);
    assert_eq!(rm.os_info, ctl_os_info);
    assert!(
        !rm.ctl,
        "the RM still refuses what only /dev/nvidiactl takes"
    );
    let ctl_open = target.downcast_ref::<NvidiaOpen>().unwrap();
    assert_eq!(ctl_open.rm_caller(&caller).os_info, ctl_os_info);
}

#[test]
fn register_fd_wants_an_unlinked_control_open_that_is_not_itself() {
    let gpu_node: Arc<dyn INode> = Arc::new(gpu(0));
    let a: Arc<dyn INode> = Arc::new(ctl());
    let b: Arc<dyn INode> = Arc::new(ctl());
    let lookup = |fd: i32| match fd {
        3 => Some(gpu_node.clone()),
        4 => Some(a.clone()),
        5 => Some(b.clone()),
        _ => None,
    };
    let caller = caller_with_fd(&lookup);
    let open = ctl();
    assert_eq!(
        open.register_fd(9, &caller),
        Err(LxError::EINVAL),
        "no such fd"
    );
    assert_eq!(
        open.register_fd(3, &caller),
        Err(LxError::EINVAL),
        "not a control open"
    );
    let a_open = a.downcast_ref::<NvidiaOpen>().unwrap();
    assert_eq!(
        a_open.register_fd(4, &caller),
        Err(LxError::EINVAL),
        "itself"
    );
    // A control open may register too; then nothing may link to it.
    assert_eq!(a_open.register_fd(5, &caller), Ok(()));
    assert_eq!(
        open.register_fd(4, &caller),
        Err(LxError::EINVAL),
        "a linked target"
    );
    let b_open = b.downcast_ref::<NvidiaOpen>().unwrap();
    assert_eq!(
        b_open.register_fd(4, &caller),
        Err(LxError::EINVAL),
        "no cycle"
    );
}

#[test]
fn register_fd_wants_exactly_an_int() {
    let open = gpu(0);
    let mut p = [0u8; 8];
    assert_eq!(
        ioctl_on(&open, esc::REGISTER_FD, &mut p),
        Err(LxError::EINVAL)
    );
}

#[test]
fn no_gpu_is_accessible_without_a_gpu_node_of_a_listed_gpu() {
    assert_eq!(gpu_mask(core::iter::empty()), 0);
    // On the host no GPU is probed, so even an open of minor 0 names none.
    let opens: [Arc<dyn INode>; 2] = [Arc::new(gpu(0)), Arc::new(ctl())];
    assert_eq!(gpu_mask(opens), 0);
}

#[test]
fn a_gpu_node_with_no_gpu_behind_it_is_enodev() {
    assert_eq!(
        NvidiaDev::new(Node::Gpu(0)).open_client().err(),
        Some(FsError::NoDevice)
    );
    assert!(NvidiaDev::new(Node::Ctl).open_client().is_ok());
}

#[test]
fn the_nodes_are_195_255_and_195_n() {
    let m = NvidiaDev::new(Node::Ctl).metadata().unwrap();
    assert_eq!(m.type_, FileType::CharDevice);
    assert_eq!(m.rdev, make_rdev(195, 255));
    assert_eq!(m.mode, 0o666);
    let m = gpu(1).metadata().unwrap();
    assert_eq!(m.rdev, make_rdev(195, 1));
}

/// The payloads laid out by hand here and the RM's own table agree; in the
/// kernel the table is in turn checked against the C `sizeof`s.
#[test]
fn the_payload_sizes_agree_with_the_rm() {
    assert_eq!(sizes_mismatch(), None);
}

fn mapping(memory: nvctl::MmapMemory, gpu_instance: u32) -> nvctl::MmapContext {
    nvctl::MmapContext {
        memory,
        gpu_instance,
        caching: 0,
        writable: true,
        start: 0x1234_5000,
        size: 0x4000,
    }
}

fn system() -> nvctl::MmapContext {
    mapping(nvctl::MmapMemory::System, u32::MAX)
}

fn registers(gpu_instance: u32) -> nvctl::MmapContext {
    mapping(nvctl::MmapMemory::Registers, gpu_instance)
}

/// GPU 0 is RM instance 1, GPU 1 is instance 0, and GPU 2 was never brought
/// up: no minor is its own instance, so a check that compared the two would
/// be caught.
fn instance_of(minor: u32) -> Option<u32> {
    match minor {
        0 => Some(1),
        1 => Some(0),
        _ => None,
    }
}

fn recorded(open: &NvidiaOpen) -> Option<nvctl::MmapContext> {
    open.state.lock().mapping
}

/// RM_MAP_MEMORY's mapping goes on the open the client named, and only on an
/// NVIDIA open of the node it is for.
#[test]
fn a_mapping_is_recorded_on_the_open_it_is_for() {
    let open = ctl();
    assert_eq!(record_mapping(Some(&open), system(), &instance_of), Ok(()));
    assert_eq!(recorded(&open), Some(system()));
    let open = gpu(0);
    assert_eq!(
        record_mapping(Some(&open), registers(1), &instance_of),
        Ok(())
    );
    assert_eq!(recorded(&open), Some(registers(1)));
    let fb = mapping(nvctl::MmapMemory::Framebuffer, 0);
    let open = gpu(1);
    assert_eq!(record_mapping(Some(&open), fb, &instance_of), Ok(()));
}

#[test]
fn a_mapping_for_another_node_is_not_recorded() {
    assert_eq!(
        record_mapping(None, system(), &instance_of),
        Err(NV_ERR_INVALID_ARGUMENT),
        "no such fd"
    );
    let not_an_open = NvidiaDev::new(Node::Ctl);
    assert_eq!(
        record_mapping(Some(&not_an_open), system(), &instance_of),
        Err(NV_ERR_INVALID_ARGUMENT),
        "the node itself, not an open of it"
    );
    let refusals = [
        (gpu(0), system(), "system memory on a GPU node"),
        (
            gpu(0),
            mapping(nvctl::MmapMemory::System, 1),
            "system memory on a GPU node, even one naming its instance",
        ),
        (ctl(), registers(1), "an aperture on the control node"),
        (gpu(0), registers(0), "another GPU's aperture"),
        (gpu(1), registers(1), "another GPU's aperture"),
        (gpu(2), registers(u32::MAX), "a GPU the RM never brought up"),
    ];
    for (open, m, what) in refusals {
        assert_eq!(
            record_mapping(Some(&open), m, &instance_of),
            Err(NV_ERR_INVALID_ARGUMENT),
            "{what}"
        );
        assert_eq!(recorded(&open), None, "{what}");
    }
}

/// One open, one mapping: a second is refused and the first one stays.
#[test]
fn an_open_holds_one_mapping() {
    let open = ctl();
    assert_eq!(record_mapping(Some(&open), system(), &instance_of), Ok(()));
    let mut other = system();
    other.start = 0x9999_0000;
    assert_eq!(
        record_mapping(Some(&open), other, &instance_of),
        Err(nvctl::NV_ERR_STATE_IN_USE)
    );
    assert_eq!(recorded(&open), Some(system()));
}

/// With no RM, RM_MAP_MEMORY is refused before the fd it names is looked up.
#[test]
fn a_map_with_no_rm_looks_nothing_up() {
    let asked = core::cell::Cell::new(0);
    let lookup = |_fd: i32| {
        asked.set(asked.get() + 1);
        None
    };
    let caller = caller_with_fd(&lookup);
    let mut p = [0u8; MAP_MEMORY_SIZE];
    assert_eq!(
        ctl().ioctl(
            iowr(esc::RM_MAP_MEMORY, MAP_MEMORY_SIZE),
            p.as_mut_ptr() as usize,
            &caller
        ),
        Err(LxError::EINVAL)
    );
    assert_eq!(asked.get(), 0);
}

#[test]
fn only_offset_zero_of_a_mapping_maps() {
    assert_eq!(mmap_window(&system(), 0x1000, 0x1000), Err(LxError::EINVAL));
    assert_eq!(
        mmap_window(&registers(0), 0x1000, 0x4000),
        Err(LxError::EINVAL)
    );
    assert_eq!(mmap_window(&system(), 0, 0), Err(LxError::EINVAL));
}

/// An aperture maps its window exactly (ENXIO otherwise); system memory any
/// length up to the end of the allocation (ERANGE past it).
#[test]
fn a_mapping_maps_its_own_length() {
    let regs = registers(0);
    assert_eq!(
        mmap_window(&regs, 0, 0x4000),
        Ok((0x1234_5000, 4, CachePolicy::UncachedDevice))
    );
    assert_eq!(
        mmap_window(&regs, 0, 0x3001),
        Ok((0x1234_5000, 4, CachePolicy::UncachedDevice)),
        "a length is whole pages, as a vma's is"
    );
    assert_eq!(mmap_window(&regs, 0, 0x3000), Err(LxError::ENXIO));
    assert_eq!(mmap_window(&regs, 0, 0x5000), Err(LxError::ENXIO));
    let sys = system();
    assert_eq!(
        mmap_window(&sys, 0, 0x1000),
        Ok((0x1234_5000, 1, CachePolicy::Cached))
    );
    assert_eq!(
        mmap_window(&sys, 0, 0x4000),
        Ok((0x1234_5000, 4, CachePolicy::Cached))
    );
    assert_eq!(
        mmap_window(&sys, 0, 0x3001),
        Ok((0x1234_5000, 4, CachePolicy::Cached))
    );
    assert_eq!(mmap_window(&sys, 0, 0x4001), Err(LxError::ERANGE));
    assert_eq!(mmap_window(&sys, 0, 0x5000), Err(LxError::ERANGE));
}

#[test]
fn a_mapping_maps_with_the_caching_its_memory_allows() {
    let mut fb = mapping(nvctl::MmapMemory::Framebuffer, 0);
    fb.caching = nvctl::caching::DEFAULT;
    assert_eq!(
        mmap_window(&fb, 0, 0x4000).map(|w| w.2),
        Ok(CachePolicy::WriteCombining)
    );
    fb.caching = nvctl::caching::UNCACHED;
    assert_eq!(
        mmap_window(&fb, 0, 0x4000).map(|w| w.2),
        Ok(CachePolicy::UncachedDevice)
    );
    let mut sys = system();
    sys.caching = nvctl::caching::UNCACHED;
    assert_eq!(
        mmap_window(&sys, 0, 0x1000).map(|w| w.2),
        Ok(CachePolicy::Uncached)
    );
}

/// An open no RM_MAP_MEMORY was recorded on maps nothing ("invalid mmap").
#[test]
fn an_open_with_no_mapping_maps_nothing() {
    assert_eq!(ctl().get_vmo(0, 0x1000).err(), Some(LxError::EINVAL));
    assert_eq!(gpu(0).get_vmo(0, 0x1000).err(), Some(LxError::EINVAL));
}

#[test]
fn a_recorded_mapping_is_one_physical_vmo_of_the_length_asked() {
    let open = ctl();
    assert_eq!(record_mapping(Some(&open), system(), &instance_of), Ok(()));
    let vmo = open.get_vmo(0, 0x2000).unwrap();
    assert_eq!(vmo.len(), 0x2000);
    assert_eq!(vmo.cache_policy(), CachePolicy::Cached);
    assert_eq!(open.get_vmo(0, 0x5000).err(), Some(LxError::ERANGE));
}

#[test]
fn only_a_read_only_grant_maps_read_only() {
    let open = ctl();
    assert!(!open.mmap_read_only(), "no mapping");
    assert_eq!(record_mapping(Some(&open), system(), &instance_of), Ok(()));
    assert!(!open.mmap_read_only(), "writable");
    let open = ctl();
    let mut ro = system();
    ro.writable = false;
    assert_eq!(record_mapping(Some(&open), ro, &instance_of), Ok(()));
    assert!(open.mmap_read_only());
}

// ---- OS events ----

/// A waker that counts its wakes.
struct Counter(core::sync::atomic::AtomicUsize);

impl alloc::task::Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker() -> (Arc<Counter>, Waker) {
    let counter = Arc::new(Counter(core::sync::atomic::AtomicUsize::new(0)));
    (counter.clone(), Waker::from(counter))
}

fn unix_event(h_object: u32) -> nvctl::UnixEvent {
    nvctl::UnixEvent {
        h_object,
        notify_index: 3,
        info32: 0x1234_5678,
        info16: 0x9abc,
    }
}

/// `nv_ioctl_alloc_os_event_t` / `nv_ioctl_free_os_event_t`, with a status
/// nothing answers.
fn os_event_payload(client: u32, fd: u32) -> [u8; ALLOC_OS_EVENT_SIZE] {
    let mut p = [0u8; ALLOC_OS_EVENT_SIZE];
    put_u32(&mut p, 0, client);
    put_u32(&mut p, 4, 0xd0d0_0001);
    put_u32(&mut p, 8, fd);
    put_u32(&mut p, 12, 0xffff_ffff);
    p
}

/// `NVOS41_PARAMETERS` aimed at `to`, with `MoreEvents` and `status` set to
/// what nothing answers.
fn get_event_payload(to: usize) -> [u8; GET_EVENT_DATA_SIZE] {
    let mut p = [0u8; GET_EVENT_DATA_SIZE];
    put_u64(&mut p, 0, to as u64);
    put_u32(&mut p, 8, 0xaaaa_aaaa);
    put_u32(&mut p, 12, 0xffff_ffff);
    p
}

/// RM_GET_EVENT_DATA on `events`: the status, `MoreEvents`, and what was
/// copied out.
fn read_event(events: &OpenEvents) -> (u32, u32, [u8; nvctl::UnixEvent::SIZE]) {
    let mut out = [0xeeu8; nvctl::UnixEvent::SIZE];
    let mut p = get_event_payload(out.as_mut_ptr() as usize);
    get_event_data(&mut p, events);
    (u32_at(&p, 12), u32_at(&p, 8), out)
}

fn readable(events: &OpenEvents) -> bool {
    events.bus.lock().events().contains(Event::READABLE)
}

/// Like the other RM escapes, the three need the RM core, and with none
/// they are refused as finding none rather than logged as not served.
#[test]
fn the_os_event_escapes_need_an_rm_core() {
    for nr in [
        esc::ALLOC_OS_EVENT,
        esc::FREE_OS_EVENT,
        esc::RM_GET_EVENT_DATA,
    ] {
        let mut p = [0u8; 16];
        assert_eq!(ioctl_on(&ctl(), nr, &mut p), Err(LxError::EINVAL));
        assert!(!logged_not_served(nr), "{:#x} is served", nr);
    }
}

#[test]
fn an_os_event_payload_of_another_size_is_einval() {
    let open = ctl();
    for nr in [
        esc::ALLOC_OS_EVENT,
        esc::FREE_OS_EVENT,
        esc::RM_GET_EVENT_DATA,
    ] {
        for wrong in [12, 20] {
            let mut p = vec![0x5au8; wrong];
            assert_eq!(open.os_event(nr, &mut p), Err(LxError::EINVAL));
            assert_eq!(p, vec![0x5au8; wrong], "{:#x} answers nothing", nr);
        }
    }
}

/// `allocate_os_event` and `free_os_event`: the answer is in the payload's
/// `Status`, and the ioctl itself succeeds.
#[test]
fn alloc_and_free_answer_in_the_payload() {
    const CLIENT: u32 = 0xc1e0_0001;
    for open in [ctl(), gpu(0)] {
        let fd = if open.is_ctl() { 7 } else { 8 };
        let mut p = os_event_payload(CLIENT, fd);
        assert_eq!(open.os_event(esc::ALLOC_OS_EVENT, &mut p), Ok(()));
        assert_eq!(u32_at(&p, 12), NV_OK);
        assert_eq!(&p[..12], &os_event_payload(CLIENT, fd)[..12]);
        let mut p = os_event_payload(CLIENT, fd);
        assert_eq!(open.os_event(esc::ALLOC_OS_EVENT, &mut p), Ok(()));
        assert_eq!(u32_at(&p, 12), 0x1A, "NV_ERR_INSUFFICIENT_RESOURCES");
        let mut p = os_event_payload(CLIENT, fd);
        assert_eq!(open.os_event(esc::FREE_OS_EVENT, &mut p), Ok(()));
        assert_eq!(u32_at(&p, 12), NV_OK);
        let mut p = os_event_payload(CLIENT, fd);
        assert_eq!(open.os_event(esc::FREE_OS_EVENT, &mut p), Ok(()));
        assert_eq!(u32_at(&p, 12), nvctl::NV_ERR_INVALID_EVENT);
    }
}

/// What the RM posts to an open that registered is what polls readable on
/// it and what RM_GET_EVENT_DATA hands back, byte for byte.
#[test]
fn an_event_posted_to_an_open_is_read_back_from_it() {
    const CLIENT: u32 = 0xc1e0_0002;
    let open = ctl();
    let mut p = os_event_payload(CLIENT, 7);
    assert_eq!(open.os_event(esc::ALLOC_OS_EVENT, &mut p), Ok(()));
    assert!(!open.poll().unwrap().read);

    post_event(open.os_info(), Some(unix_event(0xbeef_0001)));
    assert!(open.poll().unwrap().read);
    assert!(
        open.poll().unwrap().read,
        "still there: polling reads nothing"
    );

    let mut out = [0xeeu8; nvctl::UnixEvent::SIZE];
    let mut p = get_event_payload(out.as_mut_ptr() as usize);
    assert_eq!(open.os_event(esc::RM_GET_EVENT_DATA, &mut p), Ok(()));
    assert_eq!(u32_at(&p, 12), NV_OK);
    assert_eq!(u32_at(&p, 8), 0, "no more");
    assert_eq!(out, unix_event(0xbeef_0001).to_bytes());
    assert!(!open.poll().unwrap().read);
}

/// With two opens registered, what is posted to each is read back from it
/// and only from it.
#[test]
fn an_event_reaches_only_its_own_open() {
    const CLIENT: u32 = 0xc1e0_0004;
    let first = Arc::new(ctl());
    let second = Arc::new(ctl());
    for (open, fd) in [(&first, 7), (&second, 8)] {
        let mut p = os_event_payload(CLIENT, fd);
        assert_eq!(open.os_event(esc::ALLOC_OS_EVENT, &mut p), Ok(()));
    }
    post_event(first.os_info(), Some(unix_event(1)));
    post_event(second.os_info(), Some(unix_event(2)));
    assert_eq!(
        read_event(&first.events),
        (NV_OK, 0, unix_event(1).to_bytes())
    );
    assert_eq!(
        read_event(&second.events),
        (NV_OK, 0, unix_event(2).to_bytes())
    );
}

/// An open that never issued ALLOC_OS_EVENT is not where any event goes.
#[test]
fn an_open_that_registered_nothing_takes_no_event() {
    let open = gpu(0);
    post_event(open.os_info(), Some(unix_event(1)));
    post_event(open.os_info(), None);
    assert!(!open.poll().unwrap().read);
}

/// The close takes the open out of reach before the RM frees what it
/// registered. In an `Arc`, as every open is: the token is the open's
/// address, and `drop(open)` of a bare value would move it first.
#[test]
fn a_closed_open_is_out_of_reach() {
    const CLIENT: u32 = 0xc1e0_0003;
    let open = Arc::new(ctl());
    let mut p = os_event_payload(CLIENT, 7);
    assert_eq!(open.os_event(esc::ALLOC_OS_EVENT, &mut p), Ok(()));
    let events = open.events.clone();
    let reachable = || EVENT_OPENS.lock().values().any(|e| Arc::ptr_eq(e, &events));
    assert!(reachable());
    drop(open);
    assert!(!reachable());
}

#[test]
fn more_events_says_whether_another_follows() {
    let events = OpenEvents::default();
    events.post(Some(unix_event(1)));
    events.post(Some(unix_event(2)));
    let (status, more, out) = read_event(&events);
    assert_eq!((status, more), (NV_OK, 1));
    assert_eq!(out, unix_event(1).to_bytes(), "oldest first");
    let (status, more, out) = read_event(&events);
    assert_eq!((status, more), (NV_OK, 0));
    assert_eq!(out, unix_event(2).to_bytes());
    assert!(!readable(&events), "nothing left to report");
}

/// `nv_get_event` on an empty queue: `NV_ERR_OPERATING_SYSTEM`, nothing
/// copied out, `MoreEvents` as it came.
#[test]
fn an_empty_queue_answers_operating_system() {
    let events = OpenEvents::default();
    let (status, more, out) = read_event(&events);
    assert_eq!(status, NV_ERR_OPERATING_SYSTEM);
    assert_eq!(more, 0xaaaa_aaaa);
    assert_eq!(out, [0xee; nvctl::UnixEvent::SIZE]);
}

/// An event without data is only ever seen by `poll`, once.
#[test]
fn an_event_without_data_polls_readable_once() {
    let events = OpenEvents::default();
    events.post(None);
    assert!(readable(&events));
    let (status, _, _) = read_event(&events);
    assert_eq!(status, NV_ERR_OPERATING_SYSTEM, "it is not data");
    assert!(readable(&events), "and reading does not consume it");
    assert!(events.poll());
    assert!(!events.poll(), "polling does");
    assert!(!readable(&events));
}

/// Data and a dataless event at once: reporting the dataless one leaves the
/// data, and reading the data leaves the dataless one.
#[test]
fn data_and_a_dataless_event_are_told_apart() {
    let events = OpenEvents::default();
    events.post(Some(unix_event(1)));
    events.post(None);
    assert!(events.poll());
    assert!(events.poll(), "the data is still queued");
    let (status, more, _) = read_event(&events);
    assert_eq!((status, more), (NV_OK, 0));
    assert!(!events.poll());

    events.post(None);
    events.post(Some(unix_event(2)));
    let (status, more, _) = read_event(&events);
    assert_eq!(
        (status, more),
        (NV_OK, 0),
        "the dataless one is not more data"
    );
    assert!(readable(&events), "and it is still to be reported");
    assert!(events.poll());
    assert!(!readable(&events));
}

/// `get_os_event_data` takes the event off the queue before the copy, so
/// one that cannot be copied out is gone.
#[test]
fn an_event_that_cannot_be_copied_out_is_lost() {
    let events = OpenEvents::default();
    events.post(Some(unix_event(1)));
    events.post(Some(unix_event(2)));
    let mut p = get_event_payload(0);
    get_event_data(&mut p, &events);
    assert_eq!(u32_at(&p, 12), NV_ERR_INVALID_ADDRESS);
    assert_eq!(u32_at(&p, 8), 1, "MoreEvents is still told");
    let (status, _, out) = read_event(&events);
    assert_eq!(status, NV_OK);
    assert_eq!(out, unix_event(2).to_bytes());
}

#[test]
fn the_queue_stops_at_its_most() {
    let events = OpenEvents::default();
    for h in 0..MAX_QUEUED_EVENTS as u32 + 2 {
        events.post(Some(unix_event(h)));
    }
    assert_eq!(events.queue.lock().data.len(), MAX_QUEUED_EVENTS);
    let (_, _, out) = read_event(&events);
    assert_eq!(
        out,
        unix_event(0).to_bytes(),
        "the newest are the ones dropped"
    );
    events.post(Some(unix_event(0xffff)));
    assert_eq!(events.queue.lock().data.len(), MAX_QUEUED_EVENTS);
    assert_eq!(
        events.queue.lock().data.back(),
        Some(&unix_event(0xffff)),
        "room again once one is read"
    );
}

/// A poller parked on the open wakes when an event is posted, and one that
/// parks while an event is waiting does not sleep through it.
#[test]
fn a_parked_poller_wakes_on_a_post() {
    let open = gpu(0);
    let (count, waker) = counting_waker();
    let sub = open.subscribe_readiness(PollEvents::IN, &waker);
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
    open.events.post(Some(unix_event(1)));
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    drop(sub);

    let (count, waker) = counting_waker();
    let _sub = open.subscribe_readiness(PollEvents::IN, &waker);
    assert_eq!(count.0.load(Ordering::SeqCst), 1, "already readable");
}

/// Once there is nothing to report, a poller parks: the flag it parks on is
/// off.
#[test]
fn nothing_left_to_report_lets_a_poller_park() {
    let open = gpu(0);
    open.events.post(Some(unix_event(1)));
    let (status, _, _) = read_event(&open.events);
    assert_eq!(status, NV_OK);
    open.events.post(None);
    assert!(open.poll().unwrap().read);
    let (count, waker) = counting_waker();
    let _sub = open.subscribe_readiness(PollEvents::IN, &waker);
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
}

/// `poll(2)` reaches the open through its `File`: readable once an event is
/// queued, and a poller parks on the open's own flag instead of the short
/// re-poll a node with none gets.
#[test]
fn poll_through_the_file_reaches_the_open() {
    use crate::fs::{File, FileLike, OpenFlags};
    let open = Arc::new(gpu(0));
    let file = File::new(open.clone(), OpenFlags::RDWR, "/dev/nvidia0".into());
    let (count, waker) = counting_waker();
    let sub = file.subscribe_readiness(PollEvents::IN, &waker);
    assert!(sub.is_some());
    assert!(!file.poll(PollEvents::IN).unwrap().read);
    open.events.post(None);
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(file.poll(PollEvents::IN).unwrap().read);
}

/// An RM_ALLOC_MEMORY payload: `hClass`, `flags`, `pMemory` and `limit`.
fn alloc_memory(class: u32, flags: u32, address: u64, limit: u64) -> [u8; ALLOC_MEMORY_SIZE] {
    let mut p = [0u8; ALLOC_MEMORY_SIZE];
    put_u32(&mut p, ALLOC_MEMORY_CLASS, class);
    put_u32(&mut p, ALLOC_MEMORY_FLAGS, flags);
    put_u64(&mut p, ALLOC_MEMORY_PMEMORY, address);
    put_u64(&mut p, ALLOC_MEMORY_LIMIT, limit);
    p
}

#[test]
fn an_os_descriptor_has_the_pages_its_limit_covers_pinned() {
    // The limit is the last byte: 0x2000 bytes are two pages, one more three.
    for (limit, pages) in [(0, 1), (0xfff, 1), (0x1fff, 2), (0x2000, 3)] {
        let p = alloc_memory(OS_DESCRIPTOR_CLASS, 0, 0x7000_0000, limit);
        assert_eq!(
            os_descriptor_pages(&p, false),
            Some((0x7000_0000, pages, true)),
            "limit {limit:#x}"
        );
    }
    // NVOS02_FLAGS_ALLOC_USER_READ_ONLY_YES: pinned for reading only.
    let p = alloc_memory(OS_DESCRIPTOR_CLASS, 1 << 21, 0x7000_0000, 0xfff);
    assert_eq!(
        os_descriptor_pages(&p, false),
        Some((0x7000_0000, 1, false))
    );
    // The other flags are the RM's to check.
    let p = alloc_memory(OS_DESCRIPTOR_CLASS, !(1 << 21), 0x7000_0000, 0xfff);
    assert_eq!(os_descriptor_pages(&p, false), Some((0x7000_0000, 1, true)));
}

#[test]
fn nothing_is_pinned_for_a_call_the_rm_refuses_before_it_pins() {
    let good = alloc_memory(OS_DESCRIPTOR_CLASS, 0, 0x7000_0000, 0xfff);
    assert!(os_descriptor_pages(&good, false).is_some());
    assert_eq!(
        os_descriptor_pages(&good, true),
        None,
        "on the control device"
    );
    assert_eq!(
        os_descriptor_pages(&good[..48], false),
        None,
        "a payload of the wrong size"
    );
    for (p, why) in [
        (
            alloc_memory(0x3E, 0, 0x7000_0000, 0xfff),
            "NV01_MEMORY_SYSTEM",
        ),
        (
            alloc_memory(OS_DESCRIPTOR_CLASS, 0, 0x7000_0800, 0xfff),
            "an unaligned address",
        ),
        (
            alloc_memory(OS_DESCRIPTOR_CLASS, 0, 0x7000_0000, u64::MAX),
            "a limit that overflows",
        ),
    ] {
        assert_eq!(os_descriptor_pages(&p, false), None, "{why}");
    }
}

#[test]
fn an_os_descriptor_is_pinned_as_the_payload_asks() {
    let asked = core::cell::RefCell::new(Vec::new());
    let pin = |va: usize, count: usize, write: bool| {
        asked.borrow_mut().push((va, count, write));
        Ok(nvctl::UserPages {
            va,
            phys: vec![0x1000; count],
            writable: write,
            keep: alloc::boxed::Box::new(()),
        })
    };
    let caller = NvCaller {
        pin_user_pages: &pin,
        ..caller_with_fd(&no_fds)
    };
    let p = alloc_memory(OS_DESCRIPTOR_CLASS, 1 << 21, 0x7000_0000, 0x2fff);
    let pages = pin_os_descriptor(&p, false, &caller).expect("pinned");
    assert_eq!(
        (pages.va, pages.phys.len(), pages.writable),
        (0x7000_0000, 3, false)
    );
    assert_eq!(*asked.borrow(), [(0x7000_0000, 3, false)]);

    // Nothing is pinned for any other call, and a failure lends nothing.
    assert!(pin_os_descriptor(&p, true, &caller).is_none());
    assert_eq!(asked.borrow().len(), 1);
    let p = alloc_memory(OS_DESCRIPTOR_CLASS, 0, 0x7000_0000, 0xfff);
    assert!(pin_os_descriptor(&p, false, &caller_with_fd(&no_fds)).is_none());
}

#[test]
fn nothing_is_pinned_while_there_is_no_rm() {
    let asked = core::cell::Cell::new(false);
    let pin = |_: usize, _: usize, _: bool| {
        asked.set(true);
        Err(LxError::EFAULT)
    };
    let caller = NvCaller {
        pin_user_pages: &pin,
        ..caller_with_fd(&no_fds)
    };
    let mut p = alloc_memory(OS_DESCRIPTOR_CLASS, 0, 0x7000_0000, 0xfff);
    assert_eq!(
        gpu(0).ioctl(
            iowr(esc::RM_ALLOC_MEMORY, p.len()),
            p.as_mut_ptr() as usize,
            &caller
        ),
        Err(LxError::EINVAL)
    );
    assert!(!asked.get());
}
