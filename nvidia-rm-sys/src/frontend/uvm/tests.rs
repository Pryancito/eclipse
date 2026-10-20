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

const SKED: SkedRange = SkedRange {
    h_client: 0xc1d0_0002,
    h_device: 0x5c00_0002,
    h_va_space: 0x5c00_0007,
    base: 0x3_0000_0000,
    length: 0x20_0000,
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
        sked_reflected(&caller(0), &SKED),
        Err(NV_ERR_INVALID_ARGUMENT)
    );
    assert_eq!(
        free(&caller(0), 0xc1d0_0002, 0xcaf0_0000),
        NV_ERR_INVALID_ARGUMENT
    );
    let keep: Arc<dyn Any + Send + Sync> = Arc::new(());
    let lent = Pages {
        phys: PHYS,
        size: SIZE,
        keep: &keep,
    };
    assert_eq!(
        pages(&caller(0), 0xc1d0_0002, 0x5c00_0002, &lent, false),
        Err(NV_ERR_INVALID_ARGUMENT)
    );
    assert_eq!(Arc::strong_count(&keep), 1);
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
    assert_eq!(sked_reflected(&open, &SKED), Err(NV_ERR_INVALID_CLIENT));
    assert!(!super::super::IN_CALL.load(core::sync::atomic::Ordering::SeqCst));
    assert_eq!(free(&open, 0xc1d0_0002, 0xcaf0_0000), NV_ERR_INVALID_CLIENT);
    assert!(!super::super::IN_CALL.load(core::sync::atomic::Ordering::SeqCst));
    let keep: Arc<dyn Any + Send + Sync> = Arc::new(());
    let lent = Pages {
        phys: PHYS,
        size: SIZE,
        keep: &keep,
    };
    assert_eq!(
        pages(&open, 0xc1d0_0002, 0x5c00_0002, &lent, true),
        Err(NV_ERR_INVALID_CLIENT)
    );
    assert!(!super::super::IN_CALL.load(core::sync::atomic::Ordering::SeqCst));
    // The loan ended with the call.
    assert!(eclipse_uvm_pages_hold(PHYS, SIZE).is_null());
    assert_eq!(Arc::strong_count(&keep), 1);
}

/// A semaphore pool's pages, as the tests lend them.
const PHYS: u64 = 0x8000_0000;
const SIZE: u64 = 0x1_0000;

/// The RM gets a hold only on exactly the pages lent to it, and only while
/// they are; the hold keeps them past the loan, until the RM gives it back.
#[test]
fn the_rm_holds_only_the_pages_lent_to_it() {
    let _t = super::super::tests::turnstile();
    let keep: Arc<dyn Any + Send + Sync> = Arc::new(());
    let pages = Pages {
        phys: PHYS,
        size: SIZE,
        keep: &keep,
    };
    assert!(eclipse_uvm_pages_hold(PHYS, SIZE).is_null());
    let lent = Lent::new(&pages);
    assert_eq!(Arc::strong_count(&keep), 2);
    assert!(eclipse_uvm_pages_hold(PHYS, SIZE - 0x1000).is_null());
    assert!(eclipse_uvm_pages_hold(PHYS + 0x1000, SIZE - 0x1000).is_null());
    let hold = eclipse_uvm_pages_hold(PHYS, SIZE);
    assert!(!hold.is_null());
    assert_eq!(Arc::strong_count(&keep), 3);
    drop(lent);
    assert_eq!(Arc::strong_count(&keep), 2);
    assert!(eclipse_uvm_pages_hold(PHYS, SIZE).is_null());
    eclipse_uvm_pages_release(hold);
    assert_eq!(Arc::strong_count(&keep), 1);
    eclipse_uvm_pages_release(core::ptr::null_mut());
}

/// Two loans of one block are two: the end of one leaves the other.
#[test]
fn each_loan_ends_on_its_own() {
    let _t = super::super::tests::turnstile();
    let keep: Arc<dyn Any + Send + Sync> = Arc::new(());
    let pages = Pages {
        phys: PHYS,
        size: SIZE,
        keep: &keep,
    };
    let first = Lent::new(&pages);
    let second = Lent::new(&pages);
    drop(first);
    let hold = eclipse_uvm_pages_hold(PHYS, SIZE);
    assert!(!hold.is_null());
    eclipse_uvm_pages_release(hold);
    drop(second);
    assert!(eclipse_uvm_pages_hold(PHYS, SIZE).is_null());
    assert_eq!(Arc::strong_count(&keep), 1);
}
