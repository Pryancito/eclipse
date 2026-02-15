//! Error handling
pub const EINVAL: i32 = 22;
pub const ENOMEM: i32 = 12;
pub const EBADF: i32 = 9;

#[derive(Debug, Copy, Clone)]
pub struct Error {
    pub errno: i32,
}

impl Error {
    pub const fn new(errno: i32) -> Self {
        Self { errno }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

pub fn cvt(ret: usize) -> Result<usize> {
    if ret >= (usize::MAX - 4096) {
        let errno = (usize::MAX - ret + 1) as i32;
        Err(Error::new(if errno > 0 { errno } else { EINVAL }))
    } else {
        Ok(ret)
    }
}

pub fn cvt_unit(ret: usize) -> Result<()> {
    cvt(ret).map(|_| ())
}
