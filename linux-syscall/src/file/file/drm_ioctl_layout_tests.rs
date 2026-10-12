//! The layout of the four ioctl structures this file reads out of, and writes
//! back into, user memory.
//!
//! All four used to be declared inside the method that uses them, which is
//! why none of them had a test: a `struct` local to a function body cannot be
//! named from anywhere else, so there was no way to write down what its
//! layout is. Lifting them to module level changes nothing a caller can see
//! and makes the statement writable.
//!
//! It is worth writing because here the order of the fields *is* the ABI.
//! `UserInOutPtr::read` reinterprets the caller's bytes as the struct, so
//! swapping two same-typed neighbours keeps the size, keeps the alignment,
//! compiles clean, and moves every value one slot along: `SYNC_IOC_MERGE`
//! would take the fence slot for the second fd and hand the caller back an fd
//! it already owns, and `SYNCOBJ_HANDLE_TO_FD` would take the flags word for
//! the handle and refuse every export with `EINVAL`. Measured over the
//! crate's whole suite, the six swaps the four structures allow all survived.
//!
//! Three of the four sizes are also written down somewhere else -- in the
//! ioctl number userspace sends, or in `drm_scheme`'s dispatch table -- and
//! the tests below tie them together rather than restating them.

use super::*;
use core::mem::{align_of, offset_of, size_of};

/// `struct sync_merge_data` as `<linux/sync_file.h>` fixes it: the 32-byte
/// name, the fd to merge with, the fd that comes back, and the two words that
/// must be zero.
#[test]
fn sync_merge_data_is_the_struct_sync_file_h_declares_field_by_field() {
    assert_eq!(size_of::<SyncMergeData>(), 48);
    assert_eq!(align_of::<SyncMergeData>(), 4);
    assert_eq!(offset_of!(SyncMergeData, name), 0);
    assert_eq!(offset_of!(SyncMergeData, fd2), 32);
    assert_eq!(offset_of!(SyncMergeData, fence), 36);
    assert_eq!(offset_of!(SyncMergeData, flags), 40);
    assert_eq!(offset_of!(SyncMergeData, pad), 44);
    // Which accounts for all 48 bytes: `pad` is the last word and ends at the
    // end of the struct.
    assert_eq!(offset_of!(SyncMergeData, pad) + 4, 48);
}

/// `SYNC_IOC_MERGE` is `_IOWR('>', 3, struct sync_merge_data)`, and the 14-bit
/// size field of that number is the one place the struct's size appears in
/// what userspace sends: libsync's `sync_merge()` builds `0xc030_3e03` from
/// its own copy of the header, at its own compile time.
///
/// [`is_sync_ioc_merge`] ignores the size on purpose -- a size-pinned constant
/// is how this tree has lost ioctls before -- so nothing in the kernel would
/// notice the day the two stopped agreeing. This is where they are held
/// together.
#[test]
fn the_number_libsync_sends_encodes_the_size_of_this_struct() {
    const SYNC_IOC_MERGE: u32 = 0xc030_3e03;
    assert!(is_sync_ioc_merge(SYNC_IOC_MERGE));
    assert_eq!(
        ((SYNC_IOC_MERGE >> 16) & 0x3fff) as usize,
        size_of::<SyncMergeData>()
    );
}

/// The fd that goes in and the fd that comes out are two different words.
///
/// `sys_sync_file_merge` reads `data.fd2`, installs the merged fence, and then
/// writes the new fd over `data.fence`. Swap the two and the caller is handed
/// back the fd it passed in -- a live sync_file it already owns, which it then
/// closes twice -- while the merged fence leaks.
#[test]
fn the_fd_that_goes_in_is_not_the_word_the_new_fd_comes_back_in() {
    assert_eq!(
        offset_of!(SyncMergeData, fd2) + 4,
        offset_of!(SyncMergeData, fence)
    );
}

/// `struct drm_mode_create_lease` as `drm.h` fixes it: the pointer to the
/// object ids, how many there are, the flags, and the two values the ioctl
/// fills in.
#[test]
fn drm_mode_create_lease_is_the_struct_drm_h_declares_field_by_field() {
    assert_eq!(size_of::<DrmModeCreateLease>(), 24);
    assert_eq!(align_of::<DrmModeCreateLease>(), 8);
    assert_eq!(offset_of!(DrmModeCreateLease, object_ids), 0);
    assert_eq!(offset_of!(DrmModeCreateLease, object_count), 8);
    assert_eq!(offset_of!(DrmModeCreateLease, flags), 12);
    assert_eq!(offset_of!(DrmModeCreateLease, lessee_id), 16);
    assert_eq!(offset_of!(DrmModeCreateLease, fd), 20);
    assert_eq!(offset_of!(DrmModeCreateLease, fd) + 4, 24);
}

/// `struct drm_syncobj_handle`'s frozen prefix, as `drm.h` fixes it.
///
/// This one is append-only in the UABI: newer kernels grew a tail, so the
/// ioctl number userspace sends carries a size that is 16 or more (Mesa sends
/// `0xC018_64C1`, which says 24). `sys_drm_syncobj_fd` therefore matches on
/// the nr and the type alone and only ever touches the frozen 16 bytes -- so
/// the thing to pin is that those 16 bytes are the four words the comment up
/// there names, in that order.
#[test]
fn drm_syncobj_handle_is_the_frozen_sixteen_byte_prefix() {
    assert_eq!(size_of::<DrmSyncobjHandle>(), 16);
    assert_eq!(align_of::<DrmSyncobjHandle>(), 4);
    assert_eq!(offset_of!(DrmSyncobjHandle, handle), 0);
    assert_eq!(offset_of!(DrmSyncobjHandle, flags), 4);
    assert_eq!(offset_of!(DrmSyncobjHandle, fd), 8);
    assert_eq!(offset_of!(DrmSyncobjHandle, pad), 12);
    // The size the oldest form of the ioctl number encodes, `0xC010_64C1`, is
    // exactly this prefix -- and every number Mesa sends encodes at least it.
    assert_eq!(((0xC010_64C1u32 >> 16) & 0x3fff) as usize, 16);
}

/// `struct drm_syncobj_eventfd` as `drm.h` fixes it: the syncobj, the flags,
/// the timeline point, the eventfd and a pad word.
#[test]
fn drm_syncobj_eventfd_is_the_struct_drm_h_declares_field_by_field() {
    assert_eq!(size_of::<DrmSyncobjEventfd>(), 24);
    assert_eq!(align_of::<DrmSyncobjEventfd>(), 8);
    assert_eq!(offset_of!(DrmSyncobjEventfd, handle), 0);
    assert_eq!(offset_of!(DrmSyncobjEventfd, flags), 4);
    assert_eq!(offset_of!(DrmSyncobjEventfd, point), 8);
    assert_eq!(offset_of!(DrmSyncobjEventfd, fd), 16);
    assert_eq!(offset_of!(DrmSyncobjEventfd, pad), 20);
    assert_eq!(offset_of!(DrmSyncobjEventfd, pad) + 4, 24);
}

/// The byte counts in `drm_scheme`'s dispatch table are the sizes of the
/// structures *this* file parses, and this is the only place that says so.
///
/// `sys_ioctl` decides whether a request reaches these handlers at all with
/// `is_drm_ioctl_nr(cmd, n, min)`, where `min` comes from that table; the
/// handler then parses as many bytes as its own struct is long. Those are two
/// different numbers in two different crates, and they have to be the same
/// one: a `min` below the struct means the handler reads past what the caller
/// was required to provide, and a `min` above it means a request the handler
/// could have served is refused before it arrives.
#[test]
fn the_dispatch_table_agrees_with_what_these_handlers_parse() {
    use linux_object::fs::devfs::drm_scheme::nr;
    assert_eq!(nr::SYNCOBJ_EVENTFD.1, size_of::<DrmSyncobjEventfd>());
    assert_eq!(nr::MODE_CREATE_LEASE.1, size_of::<DrmModeCreateLease>());
}
