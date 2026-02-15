pub use core::error::Error;
use alloc::boxed::Box;

pub type Result<T> = core::result::Result<T, Box<dyn Error>>;
