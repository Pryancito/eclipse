//! Path Module - Basic path handling for Eclipse OS

use ::alloc::string::String;
use ::alloc::vec::Vec;
use ::alloc::borrow::ToOwned;
use core::fmt;
use core::ops::Deref;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PathBuf {
    inner: String,
}

impl PathBuf {
    pub fn new() -> Self {
        PathBuf { inner: String::new() }
    }
    
    pub fn to_str(&self) -> Option<&str> {
        Some(&self.inner)
    }

    pub fn push<P: AsRef<str>>(&mut self, path: P) {
        if !self.inner.is_empty() && !self.inner.ends_with('/') {
            self.inner.push('/');
        }
        self.inner.push_str(path.as_ref());
    }
    
    pub fn display(&self) -> Display<'_> {
        Display { inner: &self.inner }
    }
}

impl From<String> for PathBuf {
    fn from(inner: String) -> Self {
        PathBuf { inner }
    }
}

impl AsRef<str> for PathBuf {
    fn as_ref(&self) -> &str {
        &self.inner
    }
}

impl Deref for PathBuf {
    type Target = Path;
    fn deref(&self) -> &Path {
        Path::new(&self.inner)
    }
}

impl core::borrow::Borrow<Path> for PathBuf {
    fn borrow(&self) -> &Path {
        self.deref()
    }
}

#[repr(transparent)]
pub struct Path(str);

impl Path {
    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> &Path {
        unsafe { &*(s.as_ref() as *const str as *const Path) }
    }
    
    pub fn display(&self) -> Display<'_> {
        Display { inner: &self.0 }
    }
    
    pub fn to_str(&self) -> Option<&str> {
        Some(&self.0)
    }
    
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf { inner: String::from(&self.0) }
    }
}

pub struct Display<'a> {
    inner: &'a str,
}

impl fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.inner, f)
    }
}

impl AsRef<str> for Path {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl ToOwned for Path {
    type Owned = PathBuf;
    fn to_owned(&self) -> PathBuf {
        self.to_path_buf()
    }
}

impl<'a> From<&'a str> for &'a Path {
    fn from(s: &'a str) -> &'a Path {
        Path::new(s)
    }
}
