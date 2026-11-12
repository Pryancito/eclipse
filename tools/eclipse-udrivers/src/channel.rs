//! A channel: messages of bytes and handles between two processes.
//!
//! This is how a driver gives its device to another process instead of to
//! the kernel: whoever starts the driver keeps one end, the driver inherits
//! the other in a file descriptor, and what the kernel would have been
//! handed -- the FIFO's far end, the window -- goes down it.

use crate::zx::{self, nr, Handle, Status};

/// One end of a channel.
pub struct Channel(Handle);

impl Channel {
    /// A new channel: both ends.
    pub fn create() -> Result<(Channel, Channel), Status> {
        let mut ends: [Handle; 2] = [0; 2];
        // SAFETY: the kernel writes a handle into each of `ends`.
        unsafe {
            zx::call(
                nr::CHANNEL_CREATE,
                [
                    0,
                    &mut ends[0] as *mut Handle as usize,
                    &mut ends[1] as *mut Handle as usize,
                    0,
                    0,
                    0,
                ],
            )?;
        }
        Ok((Channel(ends[0]), Channel(ends[1])))
    }

    /// Move this end into a file descriptor a child process inherits (it is
    /// not close-on-exec). The child takes it out with [`Channel::from_fd`];
    /// the parent closes its copy of the descriptor once the child exists.
    pub fn into_fd(self) -> Result<libc::c_int, Status> {
        let handle = self.0;
        core::mem::forget(self);
        // SAFETY: no pointers. The handle is the descriptor's from here on,
        // whatever the answer.
        let fd = unsafe { zx::call(nr::ECLIPSE_HANDLE_TO_FD, [handle as usize, 0, 0, 0, 0, 0]) }?;
        Ok(fd as libc::c_int)
    }

    /// The end that descriptor `fd` holds, taken out of it, and the
    /// descriptor closed.
    pub fn from_fd(fd: libc::c_int) -> Result<Channel, Status> {
        const TAKE: usize = 1;
        let mut handle: Handle = 0;
        // SAFETY: the kernel writes a handle into `handle`.
        let taken = unsafe {
            zx::call(
                nr::ECLIPSE_FD_TO_HANDLE,
                [
                    fd as usize,
                    TAKE,
                    &mut handle as *mut Handle as usize,
                    0,
                    0,
                    0,
                ],
            )
        };
        // SAFETY: a descriptor this process was started with for this.
        unsafe { libc::close(fd) };
        taken.map(|_| Channel(handle))
    }

    /// Send `bytes` and `handles` as one message. The handles are the other
    /// end's from here on, whatever the answer.
    pub fn write(&self, bytes: &[u8], handles: &[Handle]) -> Result<(), Status> {
        // SAFETY: the kernel reads `bytes` and `handles`.
        unsafe {
            zx::call(
                nr::CHANNEL_WRITE,
                [
                    self.0 as usize,
                    0,
                    bytes.as_ptr() as usize,
                    bytes.len(),
                    handles.as_ptr() as usize,
                    handles.len(),
                ],
            )?;
        }
        Ok(())
    }

    /// Wait for the next message and read it: how many bytes and how many
    /// handles it had. `PEER_CLOSED` when the other end went and left
    /// nothing. A message that does not fit `bytes` and `handles` is an
    /// error and stays queued.
    pub fn read(&self, bytes: &mut [u8], handles: &mut [Handle]) -> Result<(usize, usize), Status> {
        let observed = zx::wait_one(self.0, zx::READABLE | zx::PEER_CLOSED)?;
        if observed & zx::READABLE == 0 {
            return Err(Status::PEER_CLOSED);
        }
        let (mut nbytes, mut nhandles): (u32, u32) = (0, 0);
        // SAFETY: the kernel writes at most `bytes.len()` bytes and
        // `handles.len()` handles, and the two counts.
        unsafe {
            zx::call8(
                nr::CHANNEL_READ,
                [
                    self.0 as usize,
                    0,
                    bytes.as_mut_ptr() as usize,
                    handles.as_mut_ptr() as usize,
                    bytes.len(),
                    handles.len(),
                    &mut nbytes as *mut u32 as usize,
                    &mut nhandles as *mut u32 as usize,
                ],
            )?;
        }
        Ok((nbytes as usize, nhandles as usize))
    }

    /// Wait for the next message of bytes alone and read it into `buf`, in
    /// one call (`eclipse_channel_reply_wait`, with no reply): how many
    /// bytes it had. `PEER_CLOSED` when the other end went and left
    /// nothing. A message that does not fit `buf`, or that carries
    /// handles, is `BUFFER_TOO_SMALL` and stays queued.
    pub fn wait_read(&self, buf: &mut [u8]) -> Result<usize, Status> {
        loop {
            let mut actual: u32 = 0;
            // SAFETY: the kernel writes at most `buf.len()` bytes into
            // `buf` and the length into `actual`.
            let r = unsafe {
                zx::call(
                    nr::ECLIPSE_CHANNEL_REPLY_WAIT,
                    [
                        self.0 as usize,
                        0,
                        0,
                        buf.as_mut_ptr() as usize,
                        buf.len(),
                        &mut actual as *mut u32 as usize,
                    ],
                )
            };
            match r {
                Ok(_) => return Ok(actual as usize),
                // A signal cut the wait short.
                Err(Status::INTERNAL_INTR_RETRY) => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Read the next message of bytes alone into `buf`, if there is one,
    /// without waiting: how many bytes it had, or `None` when nothing is
    /// queued. `PEER_CLOSED` when the other end went and left nothing.
    pub fn try_read(&self, buf: &mut [u8]) -> Result<Option<usize>, Status> {
        let (mut nbytes, mut nhandles): (u32, u32) = (0, 0);
        // SAFETY: the kernel writes at most `buf.len()` bytes and the two
        // counts; it is given room for no handles.
        let read = unsafe {
            zx::call8(
                nr::CHANNEL_READ,
                [
                    self.0 as usize,
                    0,
                    buf.as_mut_ptr() as usize,
                    0,
                    buf.len(),
                    0,
                    &mut nbytes as *mut u32 as usize,
                    &mut nhandles as *mut u32 as usize,
                ],
            )
        };
        match read {
            Ok(_) => Ok(Some(nbytes as usize)),
            Err(Status::SHOULD_WAIT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// This end as its handle, to give to a call that takes it: the
    /// caller's to close if the call does not.
    pub fn into_handle(self) -> Handle {
        let handle = self.0;
        core::mem::forget(self);
        handle
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        zx::close(self.0);
    }
}
