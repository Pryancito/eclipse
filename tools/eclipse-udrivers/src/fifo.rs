//! A FIFO of fixed-size messages between a driver and the kernel.

use core::marker::PhantomData;
use core::mem::size_of;

use crate::zx::{self, nr, Handle, Status};

/// A message that can go through a FIFO as its bytes.
///
/// # Safety
/// The type is `#[repr(C)]`, has no padding, and every bit pattern is a
/// value of it: the kernel fills messages in from the other end's bytes.
pub unsafe trait Message: Copy {}

/// One end of a FIFO of `M`s.
pub struct Fifo<M: Message> {
    handle: Handle,
    _messages: PhantomData<M>,
}

impl<M: Message> Fifo<M> {
    /// A new FIFO holding up to `depth` messages each way: this end, and
    /// the handle of the other, to give away.
    pub fn create(depth: usize) -> Result<(Fifo<M>, Handle), Status> {
        let mut ends: [Handle; 2] = [0; 2];
        // SAFETY: the kernel writes a handle into each of `ends`.
        unsafe {
            zx::call(
                nr::FIFO_CREATE,
                [
                    depth,
                    size_of::<M>(),
                    0,
                    &mut ends[0] as *mut Handle as usize,
                    &mut ends[1] as *mut Handle as usize,
                    0,
                ],
            )?;
        }
        Ok((
            Fifo {
                handle: ends[0],
                _messages: PhantomData,
            },
            ends[1],
        ))
    }

    /// Read what is waiting, up to `buf.len()` messages, without waiting:
    /// `SHOULD_WAIT` when there is nothing. How many were read.
    pub fn read(&self, buf: &mut [M]) -> Result<usize, Status> {
        let mut got: usize = 0;
        // SAFETY: the kernel writes at most `buf.len()` messages into `buf`
        // (any bytes are an `M`) and the count into `got`.
        unsafe {
            zx::call(
                nr::FIFO_READ,
                [
                    self.handle as usize,
                    size_of::<M>(),
                    buf.as_mut_ptr() as usize,
                    buf.len(),
                    &mut got as *mut usize as usize,
                    0,
                ],
            )?;
        }
        Ok(got)
    }

    /// Write `msgs`, as many as fit. How many went.
    pub fn write(&self, msgs: &[M]) -> Result<usize, Status> {
        let mut put: usize = 0;
        // SAFETY: the kernel reads `msgs` and writes the count into `put`.
        unsafe {
            zx::call(
                nr::FIFO_WRITE,
                [
                    self.handle as usize,
                    size_of::<M>(),
                    msgs.as_ptr() as usize,
                    msgs.len(),
                    &mut put as *mut usize as usize,
                    0,
                ],
            )?;
        }
        Ok(put)
    }

    /// A whole turn in one call (`eclipse_fifo_reply_wait`): the first
    /// `replies` messages of `buf` go out, then the call sleeps until there
    /// is something to read and reads up to `buf.len()` messages into
    /// `buf`. How many were read. With the Zircon calls alone that is a
    /// write, a read that finds nothing, a wait and a read.
    pub fn reply_wait(&self, buf: &mut [M], mut replies: usize) -> Result<usize, Status> {
        assert!(replies <= buf.len());
        loop {
            let mut got: usize = 0;
            // SAFETY: the kernel reads `replies` messages of `buf`, then
            // writes at most `buf.len()` into it and the count into `got`.
            let r = unsafe {
                zx::call(
                    nr::ECLIPSE_FIFO_REPLY_WAIT,
                    [
                        self.handle as usize,
                        size_of::<M>(),
                        buf.as_mut_ptr() as usize,
                        replies,
                        buf.len(),
                        &mut got as *mut usize as usize,
                    ],
                )
            };
            match r {
                Ok(_) => return Ok(got),
                // A signal cut the wait short. The replies went before it.
                Err(Status::INTERNAL_INTR_RETRY) => replies = 0,
                Err(e) => return Err(e),
            }
        }
    }
}
