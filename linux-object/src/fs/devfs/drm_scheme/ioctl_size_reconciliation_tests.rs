use super::*;

/// Every canonical command must be reachable from its own NR. A typo in
/// the table (two NRs mapping to one command, or a command filed under the
/// wrong number) silently reroutes a client's ioctl to another handler,
/// which is worse than not handling it at all.
#[test]
fn every_canonical_command_round_trips_through_its_nr() {
    const ALL: &[u32] = &[
        DRM_IOCTL_VERSION,
        DRM_IOCTL_GET_UNIQUE,
        DRM_IOCTL_GET_MAGIC,
        DRM_IOCTL_SET_VERSION,
        DRM_IOCTL_GEM_CLOSE,
        DRM_IOCTL_GET_CLIENT,
        DRM_IOCTL_GET_CAP,
        DRM_IOCTL_SET_CLIENT_CAP,
        DRM_IOCTL_AUTH_MAGIC,
        DRM_IOCTL_SET_MASTER,
        DRM_IOCTL_DROP_MASTER,
        DRM_IOCTL_WAIT_VBLANK,
        DRM_IOCTL_MODE_GETRESOURCES,
        DRM_IOCTL_MODE_GETCRTC,
        DRM_IOCTL_MODE_SETCRTC,
        DRM_IOCTL_MODE_CURSOR,
        DRM_IOCTL_MODE_GETGAMMA,
        DRM_IOCTL_MODE_SETGAMMA,
        DRM_IOCTL_MODE_GETENCODER,
        DRM_IOCTL_MODE_GETCONNECTOR,
        DRM_IOCTL_MODE_GETPROPERTY,
        DRM_IOCTL_MODE_SETPROPERTY,
        DRM_IOCTL_MODE_GETPROPBLOB,
        DRM_IOCTL_MODE_GETFB,
        DRM_IOCTL_MODE_ADDFB,
        DRM_IOCTL_MODE_RMFB,
        DRM_IOCTL_MODE_PAGE_FLIP,
        DRM_IOCTL_MODE_DIRTYFB,
        DRM_IOCTL_MODE_CREATE_DUMB,
        DRM_IOCTL_MODE_MAP_DUMB,
        DRM_IOCTL_MODE_DESTROY_DUMB,
        DRM_IOCTL_MODE_GETPLANERESOURCES,
        DRM_IOCTL_MODE_GETPLANE,
        DRM_IOCTL_MODE_SETPLANE,
        DRM_IOCTL_MODE_ADDFB2,
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
        DRM_IOCTL_MODE_OBJ_SETPROPERTY,
        DRM_IOCTL_MODE_CURSOR2,
        DRM_IOCTL_MODE_ATOMIC,
        DRM_IOCTL_MODE_CREATEPROPBLOB,
        DRM_IOCTL_MODE_DESTROYPROPBLOB,
        DRM_IOCTL_SYNCOBJ_CREATE,
        DRM_IOCTL_SYNCOBJ_DESTROY,
        DRM_IOCTL_SYNCOBJ_WAIT,
        DRM_IOCTL_SYNCOBJ_RESET,
        DRM_IOCTL_SYNCOBJ_SIGNAL,
        DRM_IOCTL_MODE_LIST_LESSEES,
        DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
        DRM_IOCTL_SYNCOBJ_QUERY,
        DRM_IOCTL_SYNCOBJ_TRANSFER,
        DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
        DRM_IOCTL_MODE_GETFB2,
        DRM_IOCTL_MODE_CLOSEFB,
    ];
    let mut seen = alloc::vec::Vec::new();
    for &cmd in ALL {
        let nr = cmd & 0xff;
        assert_eq!(
            canonical_drm_ioctl(nr),
            Some(cmd),
            "nr {:#04x} does not map back to {:#010x}",
            nr,
            cmd
        );
        assert!(
            !seen.contains(&nr),
            "nr {:#04x} is claimed by two commands",
            nr
        );
        seen.push(nr);
        // Every DRM ioctl's type byte is 'd'.
        assert_eq!(
            (cmd >> 8) & 0xff,
            0x64,
            "{:#010x} is not a DRM command",
            cmd
        );
    }
}

/// The regression that motivated the whole layer: the 2023 fence-deadline
/// feature appended a `__u64 deadline_nsec` to both wait structs, so a
/// current libdrm encodes 40/48 bytes where this tree parses 32/40. Linux
/// dispatches both to the same handler; we must too, without the pair of
/// hand-written `*_DEADLINE` constants that used to be the only reason the
/// larger encoding worked.
#[test]
fn a_grown_struct_reaches_the_same_handler() {
    for (canon, grown) in [
        (DRM_IOCTL_SYNCOBJ_WAIT, DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE),
        (
            DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
            DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE,
        ),
    ] {
        assert_ne!(canon, grown, "the two encodings must actually differ");
        assert_eq!(canonical_drm_ioctl(grown & 0xff), Some(canon));
        // And the kernel-side buffer is the LARGER of the two, so the
        // trailing bytes the client sent survive the round trip.
        let sizes = reconcile_sizes(grown, canon);
        assert_eq!(sizes.ksize, ioc_size(grown));
        assert_eq!(sizes.in_size, ioc_size(grown));
        assert_eq!(sizes.out_size, ioc_size(grown));
    }
}

/// A client older than us encodes FEWER bytes. Linux copies only those in,
/// zeroes the rest of its struct and copies only those back -- it never
/// writes past the end of the caller's buffer.
#[test]
fn a_short_struct_is_zero_padded_and_never_overwritten() {
    let canon = DRM_IOCTL_MODE_GETFB2; // 104 bytes
    let short = (canon & !(0x3fff << 16)) | (64u32 << 16);
    let sizes = reconcile_sizes(short, canon);
    assert_eq!(sizes.in_size, 64);
    assert_eq!(sizes.out_size, 64, "only the caller's 64 bytes go back");
    assert_eq!(sizes.ksize, 104, "but we parse our own full struct");
}

/// Direction is intersected with the handler's, so a caller cannot encode
/// `_IOC_READ` on a write-only ioctl to have a struct copied back.
#[test]
fn direction_is_intersected_with_the_handler() {
    // GEM_CLOSE is _IOW: write-only.
    let canon = DRM_IOCTL_GEM_CLOSE;
    assert_eq!(canon & IOC_READ_DIR, 0);
    let forged = canon | IOC_READ_DIR;
    let sizes = reconcile_sizes(forged, canon);
    assert_eq!(sizes.out_size, 0, "nothing may be copied back");
    assert_eq!(sizes.in_size, ioc_size(canon));
}

/// Driver-private numbers keep passing through untouched: Linux consults
/// the driver's own ioctl table for `DRM_COMMAND_BASE..DRM_COMMAND_END`,
/// and nouveau's uAPI lives there.
#[test]
fn driver_private_numbers_are_left_alone() {
    for nr in DRM_COMMAND_BASE..DRM_COMMAND_END {
        assert_eq!(canonical_drm_ioctl(nr), None, "nr {:#04x}", nr);
        assert!(!is_core_drm_nr(nr));
    }
    assert!(is_core_drm_nr(0x00));
    assert!(is_core_drm_nr(0x3A));
    assert!(is_core_drm_nr(0xA0));
    assert!(is_core_drm_nr(0xCF));
}

/// The size-mismatched path must actually WORK, not merely be reachable.
/// It hands the dispatcher a KERNEL bounce buffer, so the argument's
/// `access_ok()` has to live out here, over the range the client gave --
/// leaving it inside the dispatcher made every reconciled ioctl EFAULT,
/// i.e. exactly the calls this layer exists to rescue.
#[test]
fn the_reconciled_path_copies_in_zero_fills_and_copies_back() {
    // A client whose `drm_mode_fb_cmd2` is 40 bytes shorter than ours.
    let canon = DRM_IOCTL_MODE_GETFB2;
    let short_size = ioc_size(canon) - 40;
    let short = (canon & !(0x3fff << 16)) | ((short_size as u32) << 16);
    let mut user = alloc::vec![0xAAu8; short_size];
    user[0] = 7; // fb_id
    let user_addr = user.as_ptr() as usize;
    let ret = drm_ioctl_reconciled(short, user_addr, |cmd, kdata| {
        // Dispatched on the canonical command, never the client's.
        assert_eq!(cmd, canon);
        assert_ne!(kdata, user_addr, "the arms must see the bounce buffer");
        // SAFETY: the wrapper owns `ksize` bytes at `kdata`.
        let buf = unsafe { core::slice::from_raw_parts_mut(kdata as *mut u8, ioc_size(canon)) };
        assert_eq!(buf[0], 7, "the client's bytes arrived");
        assert!(
            buf[short_size..].iter().all(|&b| b == 0),
            "the fields the client did not send must read as zero"
        );
        buf[1] = 0x5A; // the handler's reply
        Ok(0)
    });
    assert_eq!(ret, Ok(0));
    assert_eq!(user[1], 0x5A, "the reply reached the client");
    assert_eq!(
        user.len(),
        short_size,
        "and nothing was written past its buffer"
    );
}

/// `sys_ioctl`'s pre-dispatch helpers parse the request struct themselves,
/// so they match on the NR but still need a size floor: a command encoding
/// fewer bytes than they read must fall through to the padded path instead
/// of over-reading the caller's buffer.
#[test]
fn nr_matching_keeps_a_size_floor() {
    let (n, min) = nr::PRIME_HANDLE_TO_FD;
    assert!(is_drm_ioctl_nr(0xC00C_642D, n, min), "the frozen encoding");
    assert!(is_drm_ioctl_nr(0xC018_642D, n, min), "a grown one");
    assert!(!is_drm_ioctl_nr(0xC008_642D, n, min), "a short one");
    assert!(!is_drm_ioctl_nr(0xC00C_652D, n, min), "not a DRM type byte");
    assert!(!is_drm_ioctl_nr(0xC00C_642E, n, min), "a different NR");
}

/// The two PRIME numbers as `drm.h` files them: 0x2d exports, 0x2e
/// imports. They spent a long time the other way round here, and the
/// export/import arm compensated by reading the operation off the
/// struct (`fd < 0`); with the numbers right, the number decides.
#[test]
fn prime_handle_to_fd_is_0x2d_and_fd_to_handle_is_0x2e() {
    assert_eq!(
        nr::PRIME_HANDLE_TO_FD,
        (0x2D, 12),
        "DRM_IOWR(0x2d, drm_prime_handle)"
    );
    assert_eq!(
        nr::PRIME_FD_TO_HANDLE,
        (0x2E, 12),
        "DRM_IOWR(0x2e, drm_prime_handle)"
    );
}

/// An export is an export because of the ioctl number, whatever the
/// caller left in the OUTPUT field `fd`: libdrm presets it to -1, a
/// caller that zeroes the struct leaves 0, and both are exporting. Read
/// off the struct, the zeroed one became "import stdin".
#[test]
fn a_prime_export_is_told_by_its_number_not_by_what_the_fd_field_holds() {
    const HANDLE_TO_FD: u32 = 0xC00C_642D;
    const FD_TO_HANDLE: u32 = 0xC00C_642E;
    let libdrm = DrmPrimeHandle {
        handle: 7,
        flags: DRM_CLOEXEC | DRM_RDWR,
        fd: -1,
    };
    let zeroed = DrmPrimeHandle {
        handle: 7,
        flags: DRM_CLOEXEC,
        fd: 0,
    };
    for args in [libdrm, zeroed] {
        assert_eq!(
            prime_request(HANDLE_TO_FD, args),
            Ok(PrimeRequest::Export {
                handle: 7,
                flags: args.flags
            }),
            "fd={} is not read on an export",
            args.fd
        );
    }
    // A grown struct (a newer drm.h) keeps the number.
    assert_eq!(
        prime_request(0xC018_642D, zeroed),
        Ok(PrimeRequest::Export {
            handle: 7,
            flags: DRM_CLOEXEC
        })
    );
    // The import reads `fd` and nothing else: the `handle` field is its
    // output, whatever it holds.
    let import = DrmPrimeHandle {
        handle: 0xDEAD,
        flags: 0,
        fd: 5,
    };
    assert_eq!(
        prime_request(FD_TO_HANDLE, import),
        Ok(PrimeRequest::Import { fd: 5 })
    );
    // Linux's own checks on the fields each one reads.
    assert_eq!(
        prime_request(
            HANDLE_TO_FD,
            DrmPrimeHandle {
                handle: 7,
                flags: 0x4,
                fd: -1
            }
        ),
        Err(LxError::EINVAL),
        "a flag other than DRM_CLOEXEC | DRM_RDWR"
    );
    assert_eq!(
        prime_request(
            FD_TO_HANDLE,
            DrmPrimeHandle {
                handle: 0,
                flags: 0,
                fd: -1
            }
        ),
        Err(LxError::EBADF),
        "dma_buf_get(-1)"
    );
    assert_eq!(
        prime_request(0xC00C_642F, import),
        Err(LxError::ENOTTY),
        "not a PRIME number"
    );
}
