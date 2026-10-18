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

const NOBODY: NvCaller<'static> = NvCaller {
    admin: false,
    gpu_mask: &no_gpus,
    open_of_fd: &no_fds,
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
        0x4E, /* RM_MAP_MEMORY */
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
    // Hosted there is no RM behind the RM escapes either.
    let mut p = [0u8; 32];
    assert_eq!(
        ioctl_on(&ctl(), esc::RM_CONTROL, &mut p),
        Err(LxError::EINVAL)
    );
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
