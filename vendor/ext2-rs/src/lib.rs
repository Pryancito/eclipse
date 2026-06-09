#![feature(step_trait)]
#![feature(const_fn_trait_bound)]
#![cfg_attr(all(not(test), feature = "no_std"), no_std)]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate bitflags;
extern crate genfs;
extern crate spin;

#[cfg(any(test, not(feature = "no_std")))]
extern crate core;

pub mod error;
pub mod fs;
pub mod sector;
pub mod sys;
pub mod volume;

#[cfg(test)]
mod tests {
    use sys::block_group::*;
    use sys::inode::*;
    use sys::superblock::*;

    #[test]
    fn sizes() {
        use std::mem::size_of;
        assert_eq!(size_of::<Superblock>(), 1024);
        assert_eq!(size_of::<BlockGroupDescriptor>(), 32);
        assert_eq!(size_of::<Inode>(), 128);
    }
}
