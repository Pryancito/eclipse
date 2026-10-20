use super::*;

fn caller(os_info: usize) -> Caller {
    Caller {
        os_info,
        ctl: true,
        admin: false,
        gpu_mask: 0b11,
    }
}

const MAPPING: ExternalMapping = ExternalMapping {
    h_client: 0xc1d0_0002,
    h_device: 0x5c00_0002,
    h_va_space: 0x5c00_0007,
    h_memory: 0x5c00_0014,
    offset: 0,
    base: 0x2_0000_0000,
    length: 0x20_0000,
    read_only: false,
};

/// With no open to act through, the RM is not entered at all.
#[test]
fn nothing_reaches_the_rm_without_an_open() {
    let _t = super::super::tests::turnstile();
    assert_eq!(
        va_space_device(&caller(0), 0xc1d0_0002, 0x5c00_0007, 0),
        Err(NV_ERR_INVALID_ARGUMENT)
    );
    assert_eq!(
        map_external(&caller(0), &MAPPING),
        Err(NV_ERR_INVALID_ARGUMENT)
    );
    assert_eq!(
        free(&caller(0), 0xc1d0_0002, 0xcaf0_0000),
        NV_ERR_INVALID_ARGUMENT
    );
}

/// A hosted build has no RM, so no client: each call is refused as for a
/// client that does not exist, and none leaves the front end in a call.
#[test]
fn a_hosted_build_knows_no_client() {
    let _t = super::super::tests::turnstile();
    let open = caller(0x1000);
    assert_eq!(
        va_space_device(&open, 0xc1d0_0002, 0x5c00_0007, 0),
        Err(NV_ERR_INVALID_CLIENT)
    );
    assert!(!super::super::IN_CALL.load(core::sync::atomic::Ordering::SeqCst));
    assert_eq!(map_external(&open, &MAPPING), Err(NV_ERR_INVALID_CLIENT));
    assert!(!super::super::IN_CALL.load(core::sync::atomic::Ordering::SeqCst));
    assert_eq!(free(&open, 0xc1d0_0002, 0xcaf0_0000), NV_ERR_INVALID_CLIENT);
    assert!(!super::super::IN_CALL.load(core::sync::atomic::Ordering::SeqCst));
}
