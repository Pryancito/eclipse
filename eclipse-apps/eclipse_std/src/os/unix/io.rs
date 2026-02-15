//! Unix-specific I/O extensions
pub type RawFd = i32;

pub trait AsRawFd {
    fn as_raw_fd(&self) -> RawFd;
}

pub trait FromRawFd {
    unsafe fn from_raw_fd(fd: RawFd) -> Self;
}

pub trait IntoRawFd {
    fn into_raw_fd(self) -> RawFd;
}

pub struct BorrowedFd<'a> {
    fd: RawFd,
    _marker: core::marker::PhantomData<&'a RawFd>,
}

impl<'a> BorrowedFd<'a> {
    pub unsafe fn borrow_raw(fd: RawFd) -> Self {
        Self { fd, _marker: core::marker::PhantomData }
    }
}

impl AsRawFd for BorrowedFd<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

pub trait AsFd {
    fn as_fd(&self) -> BorrowedFd<'_>;
}

pub struct OwnedFd {
    fd: RawFd,
}

impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl AsFd for OwnedFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.fd) }
    }
}

impl FromRawFd for OwnedFd {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }
}

impl IntoRawFd for OwnedFd {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.fd;
        core::mem::forget(self);
        fd
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}
