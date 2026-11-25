//! Kani proofs: what this crate reads off a disk, for **every** content of
//! the bytes up to a size, not a sample of them.
//!
//! Run from this directory with `cargo kani --harness <name>` (install once
//! with `cargo install --locked kani-verifier && cargo kani setup`). One
//! harness at a time: the solver takes gigabytes for the block-sized ones.
//!
//! A btrfs image is untrusted input -- anyone can hand the machine a USB stick
//! -- so a parse that panics, or reads past a block, on crafted bytes is a
//! kernel crash at mount time. "VERIFICATION:- SUCCESSFUL" means no byte
//! pattern of the given length makes the harness panic, overflow or index out
//! of bounds.

use crate::structs::*;

/// Symbolic bytes: `N` of them, of which the first `len` (any `len <= N`)
/// are the input.
fn any_input<const N: usize>() -> ([u8; N], usize) {
    let bytes: [u8; N] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= N);
    (bytes, len)
}

/// The contract every tree walk relies on: a block `check_block_bounds`
/// accepts can be read at every slot it claims to hold, key and data for a
/// leaf, key and pointer for a node, without one index leaving the block.
/// Up to three items in a block as small as the format allows past them.
#[kani::proof]
#[kani::unwind(5)]
fn a_block_that_passes_the_bounds_check_reads_safely_at_every_slot() {
    const N: usize = HEADER_SIZE + 3 * KEY_PTR_SIZE + 8;
    let b: [u8; N] = kani::any();
    let nodesize: usize = kani::any();
    kani::assume(nodesize <= N);
    // Few slots so the loops stay small; the arithmetic is the same for
    // every count.
    kani::assume(header::nritems(&b) <= 3);
    if check_block_bounds(&b, nodesize).is_ok() {
        let n = header::nritems(&b) as usize;
        let slot: usize = kani::any();
        kani::assume(slot < n);
        if header::level(&b) == 0 {
            let _ = leaf::key(&b, slot);
            let d = leaf::data(&b, slot);
            assert!(d.len() <= nodesize);
            let _ = leaf::free_space(&b, nodesize);
        } else {
            let _ = node::key(&b, slot);
            let _ = node::blockptr(&b, slot);
            let _ = node::generation(&b, slot);
        }
    }
}

#[kani::proof]
#[kani::unwind(8)]
fn dir_entries_of_any_item_never_panic_and_stay_inside_it() {
    let (bytes, len) = any_input::<40>();
    let b = &bytes[..len];
    for (range, e) in parse_dir_entries(b) {
        assert!(range.start < range.end && range.end <= len);
        assert!(!e.name.is_empty() && e.name.len() <= len);
    }
}

#[kani::proof]
#[kani::unwind(10)]
fn inode_refs_of_any_item_never_panic_and_stay_inside_it() {
    let (bytes, len) = any_input::<24>();
    let b = &bytes[..len];
    let mut prev = 0;
    for (range, _, name) in parse_inode_refs(b) {
        assert!(range.start == prev && range.end <= len);
        assert_eq!(name.len(), range.end - range.start - 10);
        prev = range.end;
    }
}

#[kani::proof]
#[kani::unwind(4)]
fn chunk_item_of_any_bytes_never_panics() {
    let (bytes, len) = any_input::<{ CHUNK_ITEM_HDR_LEN + 2 * STRIPE_LEN + 4 }>();
    // Two stripes at most fit; a bigger count must be refused, not read.
    if let Some(c) = ChunkItem::parse(&bytes[..len]) {
        assert!(!c.stripes.is_empty() && c.stripes.len() <= 2);
    }
}

#[kani::proof]
fn fixed_items_of_any_bytes_never_panic() {
    let (bytes, len) = any_input::<{ ROOT_ITEM_LEN + 1 }>();
    let b = &bytes[..len];
    let _ = InodeItem::parse(b);
    let _ = RootItem::parse(b);
    let _ = FileExtent::parse(b);
    let _ = DevItem::parse(b);
    let _ = DevExtent::parse(b);
    let _ = BlockGroupItem::parse(b);
}
