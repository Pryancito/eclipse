//! FFI extensions for Unix-like systems
use crate::env::{OsStr, OsString};

/// Unix-specific extensions to `OsString`.
pub trait OsStringExt {
    fn from_vec(vec: ::alloc::vec::Vec<u8>) -> Self;
    fn into_vec(self) -> ::alloc::vec::Vec<u8>;
}

/// Unix-specific extensions to `OsStr`.
pub trait OsStrExt {
    fn from_bytes(slice: &[u8]) -> &Self;
    fn as_bytes(&self) -> &[u8];
}

impl OsStringExt for OsString {
    fn from_vec(vec: ::alloc::vec::Vec<u8>) -> Self {
        unsafe { core::str::from_utf8_unchecked(&vec).to_string() }
    }
    fn into_vec(self) -> ::alloc::vec::Vec<u8> {
        self.into_bytes()
    }
}

impl OsStrExt for OsStr {
    fn from_bytes(slice: &[u8]) -> &Self {
        unsafe { core::mem::transmute(slice) }
    }
    fn as_bytes(&self) -> &[u8] {
        let s: &str = self;
        s.as_bytes()
    }
}
