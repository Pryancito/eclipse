//! Error trait and common implementations
use core::fmt::{Debug, Display};

/// Base Error trait
pub trait Error: Debug + Display {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

// pub use core::error::RequestRef;
