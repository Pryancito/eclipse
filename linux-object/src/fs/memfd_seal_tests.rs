//! `fcntl(F_ADD_SEALS)` and `F_GET_SEALS`. Nothing in this crate looked at
//! the seals, and they used to be answered with a blanket 0. That is not a
//! harmless simplification: Firefox seals every shared-memory segment
//! against shrinking and then refuses to map any segment whose
//! `F_GET_SEALS` does not report the seal back, so a kernel that accepts a
//! seal and then denies having it fails on every IPC buffer and frame.

use super::*;

/// A memfd that accepts seals. The inode is what the seals hang off, and
/// holding it is what keeps the registry entry alive.
fn sealable() -> Arc<dyn INode> {
    new_memfd("sealable", MFD_ALLOW_SEALING).unwrap().inode()
}

/// An inode that is not a memfd at all: the backing filesystem's own root.
fn not_a_memfd() -> Arc<dyn INode> {
    MEMFD_FS.root_inode()
}

#[test]
fn the_seals_have_their_linux_values_and_are_one_bit_each() {
    assert_eq!(F_SEAL_SEAL, 0x0001);
    assert_eq!(F_SEAL_SHRINK, 0x0002);
    assert_eq!(F_SEAL_GROW, 0x0004);
    assert_eq!(F_SEAL_WRITE, 0x0008);
    assert_eq!(F_SEAL_FUTURE_WRITE, 0x0010);
    // `F_ADD_SEALS` refuses every bit outside this set, so it has to hold
    // all five of them and nothing else.
    assert_eq!(F_SEAL_ALL, 0x001F, "the set F_ADD_SEALS accepts moved");
    let each = [
        F_SEAL_SEAL,
        F_SEAL_SHRINK,
        F_SEAL_GROW,
        F_SEAL_WRITE,
        F_SEAL_FUTURE_WRITE,
    ];
    for (i, a) in each.iter().enumerate() {
        assert_eq!(a.count_ones(), 1, "seal {i} is not a single bit");
        assert_ne!(a & F_SEAL_ALL, 0, "seal {i} is not in F_SEAL_ALL");
        for b in &each[i + 1..] {
            assert_eq!(a & b, 0, "two seals share a bit: {a:#x} and {b:#x}");
        }
    }
}

#[test]
fn a_seal_added_is_a_seal_reported_back() {
    let inode = sealable();
    assert_eq!(
        memfd_seals(&inode),
        Some(0),
        "a memfd made with MFD_ALLOW_SEALING starts unsealed"
    );
    memfd_add_seals(&inode, F_SEAL_SHRINK).unwrap();
    assert_eq!(memfd_seals(&inode), Some(F_SEAL_SHRINK));
    // The second request adds to the first instead of replacing it. This
    // is the sequence Firefox makes -- shrink first, then write -- and
    // dropping the shrink seal on the way is exactly what makes it refuse
    // the mapping it just sealed.
    memfd_add_seals(&inode, F_SEAL_GROW | F_SEAL_WRITE).unwrap();
    assert_eq!(
        memfd_seals(&inode),
        Some(F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE)
    );
}

#[test]
fn a_seal_belongs_to_one_file_and_not_to_the_next() {
    let a = sealable();
    let b = sealable();
    memfd_add_seals(&a, F_SEAL_WRITE).unwrap();
    assert_eq!(memfd_seals(&a), Some(F_SEAL_WRITE));
    assert_eq!(memfd_seals(&b), Some(0), "the seal landed on another file");
    // And an inode that is not a memfd has no seals at all, however many
    // memfds are live beside it -- `a` and `b` are still held here.
    assert_eq!(memfd_seals(&not_a_memfd()), None);
}

#[test]
fn only_the_five_seals_that_exist_can_be_added() {
    let inode = sealable();
    for bad in [0x0020u32, 0x0040, 0x8000, u32::MAX] {
        assert_eq!(
            memfd_add_seals(&inode, bad),
            Err(LxError::EINVAL),
            "{bad:#x}"
        );
    }
    assert_eq!(
        memfd_seals(&inode),
        Some(0),
        "a refused request sealed something anyway"
    );
    // An empty request is not an unknown bit; Linux takes it.
    assert_eq!(memfd_add_seals(&inode, 0), Ok(()));
}

#[test]
fn a_file_that_cannot_be_sealed_says_so_with_eperm() {
    // Without MFD_ALLOW_SEALING a memfd is born carrying F_SEAL_SEAL, and
    // F_SEAL_SEAL is the one that refuses every later request.
    let shut = new_memfd("shut", 0).unwrap().inode();
    assert_eq!(memfd_seals(&shut), Some(F_SEAL_SEAL));
    assert_eq!(memfd_add_seals(&shut, F_SEAL_WRITE), Err(LxError::EPERM));
    assert_eq!(memfd_seals(&shut), Some(F_SEAL_SEAL));
    // A sealable one can be shut the same way afterwards.
    let inode = sealable();
    memfd_add_seals(&inode, F_SEAL_SEAL).unwrap();
    assert_eq!(memfd_add_seals(&inode, F_SEAL_GROW), Err(LxError::EPERM));
    // EPERM is "this file refuses", which is a different answer from the
    // EINVAL of an unknown bit or of a file that is not a memfd.
    assert_eq!(
        memfd_add_seals(&not_a_memfd(), F_SEAL_GROW),
        Err(LxError::EINVAL)
    );
}

#[test]
fn shrinking_and_growing_are_refused_by_their_own_seal() {
    let inode = sealable();
    inode.resize(4096).unwrap();
    memfd_add_seals(&inode, F_SEAL_SHRINK).unwrap();
    assert!(
        !memfd_resize_allowed(&inode, 0),
        "a shrink to nothing got through F_SEAL_SHRINK"
    );
    assert!(!memfd_resize_allowed(&inode, 4095));
    assert!(
        memfd_resize_allowed(&inode, 4096),
        "staying the same size is not a shrink"
    );
    assert!(
        memfd_resize_allowed(&inode, 8192),
        "F_SEAL_SHRINK refused a grow"
    );

    let other = sealable();
    other.resize(4096).unwrap();
    memfd_add_seals(&other, F_SEAL_GROW).unwrap();
    assert!(
        !memfd_resize_allowed(&other, 4097),
        "a grow got through F_SEAL_GROW"
    );
    assert!(memfd_resize_allowed(&other, 4096));
    assert!(
        memfd_resize_allowed(&other, 0),
        "F_SEAL_GROW refused a shrink"
    );

    // Anything that is not a memfd carries no seals and gets no say.
    assert!(memfd_resize_allowed(&not_a_memfd(), 0));
}

#[test]
fn a_write_seal_and_a_future_write_seal_both_stop_a_write() {
    let resized = sealable();
    assert!(memfd_write_allowed(&resized));
    memfd_add_seals(&resized, F_SEAL_SHRINK | F_SEAL_GROW).unwrap();
    assert!(
        memfd_write_allowed(&resized),
        "a resize seal stopped a write"
    );

    let write = sealable();
    memfd_add_seals(&write, F_SEAL_WRITE).unwrap();
    assert!(!memfd_write_allowed(&write));

    // F_SEAL_FUTURE_WRITE is the one a caller adds to keep the mapping it
    // already holds writable while nobody else may write. From this side
    // it stops a write just like F_SEAL_WRITE, and looking only at
    // F_SEAL_WRITE let every such write through.
    let future = sealable();
    memfd_add_seals(&future, F_SEAL_FUTURE_WRITE).unwrap();
    assert!(!memfd_write_allowed(&future));

    assert!(memfd_write_allowed(&not_a_memfd()));
}
