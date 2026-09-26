//! VirtIO guest drivers.

#![cfg_attr(not(test), no_std)]
#![deny(unused_must_use, missing_docs)]
#![allow(clippy::identity_op)]
#![allow(dead_code)]

// #[macro_use]
extern crate log;

extern crate alloc;

mod blk;
mod console;
mod gpu;
mod hal;
mod header;
mod input;
mod net;
mod queue;
#[cfg(test)]
mod test_dev;

pub use self::blk::VirtIOBlk;
pub use self::console::VirtIOConsole;
pub use self::gpu::VirtIOGpu;
pub use self::header::*;
pub use self::input::{InputConfigSelect, InputEvent, VirtIOInput};
pub use self::net::VirtIONet;
use self::queue::VirtQueue;
use core::mem::size_of;
use hal::*;

const PAGE_SIZE: usize = 0x1000;

/// The type returned by driver methods.
pub type Result<T = ()> = core::result::Result<T, Error>;

// pub struct Error {
//     kind: ErrorKind,
//     reason: &'static str,
// }

/// The error type of VirtIO drivers.
#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    /// The buffer is too small.
    BufferTooSmall,
    /// The device is not ready.
    NotReady,
    /// The queue is already in use.
    AlreadyUsed,
    /// Invalid parameter.
    InvalidParam,
    /// Failed to alloc DMA memory.
    DmaError,
    /// I/O Error
    IoError,
}

/// Align `size` up to a page.
///
/// This used to be `(size + PAGE_SIZE) & !(PAGE_SIZE - 1)`, which is a page too
/// much for a size that is already aligned -- `align_up(4096)` answered 8192.
/// No queue size reaches that case today (`18 * n + 6` and `8 * n + 6` are
/// never multiples of 4096 for a power-of-two `n`), so it cost a page of DMA
/// and nothing else; it is the next caller that would have paid.
fn align_up(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Pages of `size`.
fn pages(size: usize) -> usize {
    (size + PAGE_SIZE - 1) / PAGE_SIZE
}

/// Convert a struct into buffer.
unsafe trait AsBuf: Sized {
    fn as_buf(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const _ as _, size_of::<Self>()) }
    }
    fn as_buf_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self as *mut _ as _, size_of::<Self>()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_leaves_a_whole_page_alone() {
        // It used to add a page to a size that was already aligned:
        // `align_up(4096)` answered 8192.
        assert_eq!(align_up(PAGE_SIZE), PAGE_SIZE);
        assert_eq!(align_up(4 * PAGE_SIZE), 4 * PAGE_SIZE);
    }

    #[test]
    fn align_up_rounds_a_partial_page_up() {
        assert_eq!(align_up(1), PAGE_SIZE);
        assert_eq!(align_up(PAGE_SIZE - 1), PAGE_SIZE);
        assert_eq!(align_up(PAGE_SIZE + 1), 2 * PAGE_SIZE);
    }

    #[test]
    fn align_up_of_nothing_is_nothing() {
        assert_eq!(align_up(0), 0);
    }

    #[test]
    fn pages_counts_the_page_a_partial_one_still_needs() {
        assert_eq!(pages(0), 0);
        assert_eq!(pages(1), 1);
        assert_eq!(pages(PAGE_SIZE), 1);
        assert_eq!(pages(PAGE_SIZE + 1), 2);
    }

    #[test]
    fn a_size_rounded_up_covers_the_pages_counted() {
        for size in [
            0usize,
            1,
            17,
            PAGE_SIZE - 1,
            PAGE_SIZE,
            PAGE_SIZE + 1,
            100_000,
        ] {
            assert_eq!(align_up(size), pages(size) * PAGE_SIZE, "size {}", size);
        }
    }

    #[test]
    fn every_device_error_has_a_name() {
        // `zcore-drivers` maps this enum onto its own one exhaustively, so a
        // variant added here without a home there stops that crate compiling
        // -- which is the point. This pins the set as it stands.
        let all = [
            Error::BufferTooSmall,
            Error::NotReady,
            Error::AlreadyUsed,
            Error::InvalidParam,
            Error::DmaError,
            Error::IoError,
        ];
        assert_eq!(all.len(), 6);
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert!((a == b) == (i == j), "{:?} and {:?} compare wrongly", a, b);
            }
        }
    }
}
