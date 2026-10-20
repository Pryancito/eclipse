//! The staging buffer every write syscall copies its bytes through. Its
//! whole job is that the slice the filesystem sees is KERNEL memory: a
//! filesystem reads that slice with its own lock held, and a fault on a
//! user page there re-enters the filesystem on the cpu that holds the
//! lock (see `Staging`). So the two things that can silently break it are
//! a buffer of the wrong length -- which would truncate or panic the
//! per-chunk `copy_from_slice` -- and a huge request aborting the kernel
//! instead of answering ENOMEM.
use super::*;

#[test]
fn a_small_chunk_is_served_without_touching_the_heap() {
    let mut s = Staging::new(64).unwrap();
    assert_eq!(s.buf().len(), 64);
    assert!(s.heap.is_empty(), "a 64-byte chunk must not allocate");
}

#[test]
fn the_stack_buffer_is_five_hundred_and_twelve_bytes() {
    // Pinned on its own, because every other test here feeds
    // `STAGING_STACK_BUF` back in: move the constant and the tests move
    // with it, so the boundary they claim to guard goes untested and
    // halving it to 256 passes the whole suite green.
    //
    // The number is a kernel stack-frame budget. This array sits in the
    // frame of every write syscall, so growing it eats the kernel stack
    // on a path that is already several frames deep, and shrinking it
    // sends ordinary small writes -- a shell echoing a line, a log
    // record -- through the heap allocator instead.
    assert_eq!(STAGING_STACK_BUF, 512, "the staging stack budget moved");
}

#[test]
fn a_chunk_of_exactly_the_stack_ceiling_still_fits_on_the_stack() {
    // The boundary, because `>` and `>=` here disagree only at this one
    // value and both look right: at `>=` the heap buffer is never made
    // yet `buf()` would index it. The size is written out rather than
    // read from the constant -- see
    // `the_stack_buffer_is_five_hundred_and_twelve_bytes`.
    let mut s = Staging::new(512).unwrap();
    assert_eq!(s.buf().len(), 512);
    assert!(s.heap.is_empty());
}

#[test]
fn a_chunk_past_the_ceiling_moves_to_the_heap_at_its_full_length() {
    let mut s = Staging::new(513).unwrap();
    assert_eq!(s.buf().len(), 513);
    let n = s.buf().len();
    assert_eq!(s.heap.len(), n);
}

#[test]
fn a_chunk_the_heap_cannot_hold_is_enomem_and_not_a_kernel_abort() {
    assert_eq!(Staging::new(usize::MAX).err(), Some(LxError::ENOMEM));
}

#[test]
fn the_staged_slice_carries_the_bytes_and_is_not_the_source() {
    // The property the deadlock fix rests on: what the file is handed is a
    // copy, at a different address from the caller's buffer.
    let src = alloc::vec![7u8; 300];
    let mut s = Staging::new(300).unwrap();
    let dst = &mut s.buf()[..300];
    dst.copy_from_slice(&src);
    assert_eq!(dst, &src[..]);
    assert_ne!(dst.as_ptr(), src.as_ptr());
}

#[test]
fn a_chunk_shorter_than_the_buffer_stages_only_its_own_bytes() {
    // The loop stages `n <= chunk_size` bytes into the front of the
    // buffer; the tail is left alone and must not reach the file.
    let mut s = Staging::new(STAGING_STACK_BUF + 8).unwrap();
    s.buf().fill(0xAA);
    let dst = &mut s.buf()[..3];
    dst.copy_from_slice(&[1, 2, 3]);
    assert_eq!(&s.buf()[..5], &[1, 2, 3, 0xAA, 0xAA]);
}
