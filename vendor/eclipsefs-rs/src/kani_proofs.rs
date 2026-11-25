//! Kani proofs: the records this crate reads off a disk, for **every**
//! content of the bytes up to a size, not a sample of them.
//!
//! Run from this directory with `cargo kani --harness <name>` (install once
//! with `cargo install --locked kani-verifier && cargo kani setup`), one
//! harness at a time. An EclipseFS image is untrusted input -- a disk can be
//! anybody's -- so a record that panics or reads past its item on crafted
//! bytes is a kernel crash at lookup time. "VERIFICATION:- SUCCESSFUL" means
//! no byte pattern of the given length does.

use crate::cowfmt::*;
use crate::format::*;

/// Symbolic bytes: `N` of them, of which the first `len` (any `len <= N`)
/// are the input.
fn any_input<const N: usize>() -> ([u8; N], usize) {
    let bytes: [u8; N] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= N);
    (bytes, len)
}

/// Every record the classic directory format can pack into 48 bytes: none
/// panics, every name lies inside the block, and the records tile it.
#[kani::proof]
#[kani::unwind(6)]
fn classic_dirents_of_any_block_never_panic() {
    let (bytes, len) = any_input::<48>();
    let b = &bytes[..len];
    if let Ok(list) = parse_dirents(b) {
        let mut at = 0u64;
        for d in &list {
            assert_eq!(d.off, at);
            assert!(d.name.len() < d.rec_len);
            at += d.rec_len as u64;
        }
        assert_eq!(at as usize, len);
    }
}

#[kani::proof]
#[kani::unwind(6)]
fn cow_dir_items_of_any_value_never_panic() {
    let (bytes, len) = any_input::<48>();
    let b = &bytes[..len];
    if let Ok(list) = decode_dir_item(b) {
        let total: usize = list.iter().map(|e| 19 + e.name.len()).sum();
        assert_eq!(total, len);
    }
    let _ = decode_pair(b);
}

#[kani::proof]
#[kani::unwind(10)]
fn xattrs_of_any_value_never_panic() {
    let (bytes, len) = any_input::<32>();
    if let Ok(list) = decode_xattrs(&bytes[..len]) {
        let total: usize = list.iter().map(|(n, v)| 4 + n.len() + v.len()).sum();
        assert_eq!(total, len);
    }
}

macro_rules! never_panics {
    ($name:ident, $n:expr, $decode:expr) => {
        #[kani::proof]
        fn $name() {
            let (bytes, len) = any_input::<{ $n }>();
            let _ = $decode(&bytes[..len]);
        }
    };
}

never_panics!(ref_item_never_panics, 16, decode_ref);
never_panics!(
    cow_inode_item_never_panics,
    INODE_ITEM_SIZE + 1,
    InodeItem::decode
);
never_panics!(cow_file_extent_never_panics, 64, FileExtent::decode);
never_panics!(
    cow_root_item_never_panics,
    ROOT_ITEM_SIZE + 1,
    RootItem::decode
);
never_panics!(classic_inode_never_panics, INODE_SIZE + 1, Inode::decode);

#[kani::proof]
fn extent_of_its_disk_size_never_panics() {
    let b: [u8; crate::extent::EXTENT_DISK_SIZE] = kani::any();
    let _ = crate::extent::Extent::decode(&b);
}
