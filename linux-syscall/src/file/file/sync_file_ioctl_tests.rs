//! The sync_file ioctls `sys_ioctl` has to route itself: `SYNC_IOC_MERGE` and
//! the two dma-buf sync_file ioctls, recognised by number.

use super::{dma_buf_sync_file_ioctl, is_sync_ioc_merge};

/// The number libsync's `sync_merge()` sends.
const SYNC_IOC_MERGE: u32 = 0xc030_3e03;

#[test]
fn the_merge_ioctl_is_recognised() {
    assert!(is_sync_ioc_merge(SYNC_IOC_MERGE));
    // Same type, other nrs (`SYNC_IOC_FILE_INFO` is 4) are not the merge.
    assert!(!is_sync_ioc_merge(0xc038_3e04));
    // The DRM syncobj fd ioctls share the "match on type and nr" idea but
    // are another family.
    assert!(!is_sync_ioc_merge(0xc018_64c1));
    assert!(!is_sync_ioc_merge(0xc018_64c2));
}

/// The two dma-buf sync_file ioctls, by the numbers `<linux/dma-buf.h>`
/// encodes (`_IOWR('b', 2, ...)` and `_IOW('b', 3, ...)`, 8-byte
/// structs), and nothing else of the family: `SYNC` and `SET_NAME` go
/// to the dma-buf's own `ioctl`.
#[test]
fn the_dma_buf_sync_file_ioctls_are_recognised() {
    assert_eq!(dma_buf_sync_file_ioctl(0xc008_6202), Some(true), "EXPORT");
    assert_eq!(dma_buf_sync_file_ioctl(0x4008_6203), Some(false), "IMPORT");
    assert_eq!(dma_buf_sync_file_ioctl(0x4008_6200), None, "SYNC");
    assert_eq!(dma_buf_sync_file_ioctl(0x4008_6201), None, "SET_NAME");
    assert_eq!(
        dma_buf_sync_file_ioctl(0x4008_6202),
        None,
        "wrong direction"
    );
    assert_eq!(dma_buf_sync_file_ioctl(SYNC_IOC_MERGE), None);
}
