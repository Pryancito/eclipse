//! A network card the kernel drives, borrowed by this process.
//!
//! The kernel keeps the card's driver and gives this process its frames
//! (`eclipse_net_borrow`, `eclipse_net_lent_turn`:
//! `loader/src/linux/net_lend.rs` is the other end). The messages are
//! [`Msg`]s over a window of [`BUF_SIZE`]-byte slots, receive slots first,
//! as with a card a driver serves ([`crate::net`]); the window is this
//! process's own memory, which the kernel copies frames into and out of
//! during a turn, and there is no FIFO: a [`turn`](Lent::turn) is one
//! call that gives the kernel this side's messages and takes the
//! kernel's.

use std::ptr;

use crate::net::{Msg, NoCard, BUF_SIZE};
use crate::zx::{self, nr, Handle, Status};

/// [`Lent::turn`]: sleep until the kernel has something to say.
const WAIT: usize = 1;
/// [`Lent::turn`]: take the frames that arrived too.
const FRAMES: usize = 2;

/// A borrowed card. The loan ends when this is dropped, or with the
/// process.
pub struct Lent {
    handle: Handle,
    window: *mut u8,
    len: usize,
    /// The card's address.
    pub mac: [u8; 6],
    pub rx_slots: usize,
    pub tx_slots: usize,
}

// SAFETY: the window is this value's alone; what points into it (a port)
// is kept by whoever keeps this.
unsafe impl Send for Lent {}
unsafe impl Sync for Lent {}

impl Lent {
    /// Borrow the kernel's card `index` (the loopback is not counted),
    /// over a window of this many slots.
    pub fn borrow(index: usize, rx_slots: usize, tx_slots: usize) -> Result<Lent, NoCard> {
        let len = (rx_slots + tx_slots) * BUF_SIZE;
        // SAFETY: a fresh anonymous mapping.
        let window = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if window == libc::MAP_FAILED {
            return Err(NoCard::Other(format!(
                "mmap: {}",
                std::io::Error::last_os_error()
            )));
        }
        // Every page there before the kernel writes a frame to it.
        // SAFETY: `len` bytes just mapped.
        unsafe { ptr::write_bytes(window.cast::<u8>(), 0, len) };
        let mut out = [0u64; 2];
        // SAFETY: the kernel writes two words to `out`, and keeps the
        // window's address for the turns to come: the mapping lasts as
        // long as the handle does.
        let r = unsafe {
            zx::call(
                nr::ECLIPSE_NET_BORROW,
                [
                    index,
                    window as usize,
                    rx_slots,
                    tx_slots,
                    out.as_mut_ptr() as usize,
                    0,
                ],
            )
        };
        if let Err(e) = r {
            // SAFETY: the mapping made above, which nothing else has seen.
            unsafe { libc::munmap(window, len) };
            return Err(match e {
                Status::NO_GATEWAY => NoCard::NoGateway,
                Status::NOT_FOUND => NoCard::Other(format!("the kernel drives no card {index}")),
                Status::NOT_SUPPORTED => NoCard::Other(format!(
                    "the driver of the kernel's card {index} cannot lend it"
                )),
                Status::ALREADY_BOUND => {
                    NoCard::Other(format!("the kernel's card {index} is lent already"))
                }
                e => NoCard::Other(format!("net_borrow: {e}")),
            });
        }
        Ok(Lent {
            handle: out[0] as Handle,
            window: window.cast(),
            len,
            mac: core::array::from_fn(|i| (out[1] >> (8 * i)) as u8),
            rx_slots,
            tx_slots,
        })
    }

    /// The window: `(rx_slots + tx_slots) * BUF_SIZE` bytes, for as long
    /// as this lives.
    pub fn window(&self) -> *mut u8 {
        self.window
    }

    /// A turn: the first `give` messages of `msgs` go to the kernel -- the
    /// receive slots that were read, the frames to send -- and what the
    /// kernel has to say comes back in `msgs`: the transmit slots whose
    /// frame the card has taken and, with `frames`, the frames that
    /// arrived, each at the start of its receive slot. With `wait` the
    /// call sleeps until there is something. How many came back.
    pub fn turn(
        &self,
        msgs: &mut [Msg],
        mut give: usize,
        wait: bool,
        frames: bool,
    ) -> Result<usize, Status> {
        assert!(give <= msgs.len());
        let room = msgs.len().min(self.rx_slots + self.tx_slots);
        let flags = if wait { WAIT } else { 0 } | if frames { FRAMES } else { 0 };
        loop {
            let mut got: usize = 0;
            // SAFETY: the kernel reads `give` messages of `msgs`, writes at
            // most `room` into it and the count into `got`, and reads and
            // writes the window, which is this value's.
            let r = unsafe {
                zx::call(
                    nr::ECLIPSE_NET_LENT_TURN,
                    [
                        self.handle as usize,
                        msgs.as_mut_ptr() as usize,
                        give,
                        room,
                        flags,
                        &mut got as *mut usize as usize,
                    ],
                )
            };
            match r {
                Ok(_) => return Ok(got),
                // A signal cut the wait short. The messages went before it.
                Err(Status::INTERNAL_INTR_RETRY) => give = 0,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Lent {
    /// The card is the kernel's again.
    fn drop(&mut self) {
        zx::close(self.handle);
        // SAFETY: the mapping `borrow` made; the kernel has let go of it
        // with the handle, and whoever dropped this has let go of what
        // pointed into it.
        unsafe { libc::munmap(self.window.cast(), self.len) };
    }
}
