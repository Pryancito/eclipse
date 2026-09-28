use {
    crate::object::*,
    alloc::collections::VecDeque,
    alloc::sync::{Arc, Weak},
    kernel_hal::sync::Mutex,
};

/// First-In First-Out inter-process queue.
///
/// # SYNOPSIS
///
/// FIFOs are intended to be the control plane for shared memory transports.
/// Their read and write operations are more efficient than [`sockets`] or [`channels`],
/// but there are severe restrictions on the size of elements and buffers.
///
/// [`sockets`]: ../socket/struct.Socket.html
/// [`channels`]: ../channel/struct.Channel.html
pub struct Fifo {
    base: KObjectBase,
    peer: Weak<Fifo>,
    elem_count: usize,
    elem_size: usize,
    recv_queue: Mutex<VecDeque<u8>>,
}

impl_kobject!(Fifo
    fn peer(&self) -> ZxResult<Arc<dyn KernelObject>> {
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        Ok(peer)
    }
    fn related_koid(&self) -> KoID {
        self.peer.upgrade().map(|p| p.id()).unwrap_or(0)
    }
);

impl Fifo {
    /// Create a FIFO.
    #[allow(unsafe_code)]
    pub fn create(elem_count: usize, elem_size: usize) -> (Arc<Self>, Arc<Self>) {
        let end0 = Arc::new(Fifo {
            base: KObjectBase::with_signal(Signal::WRITABLE),
            peer: Weak::default(),
            elem_count,
            elem_size,
            recv_queue: Mutex::new(VecDeque::with_capacity(elem_count * elem_size)),
        });
        let end1 = Arc::new(Fifo {
            base: KObjectBase::with_signal(Signal::WRITABLE),
            peer: Arc::downgrade(&end0),
            elem_count,
            elem_size,
            recv_queue: Mutex::new(VecDeque::with_capacity(elem_count * elem_size)),
        });
        // no other reference of `end0`
        unsafe { &mut *(Arc::as_ptr(&end0) as *mut Fifo) }.peer = Arc::downgrade(&end1);
        (end0, end1)
    }

    /// Write data to the FIFO.
    ///
    /// This attempts to write up to `count` elements (`count * elem_size` bytes)
    /// from `data` to the fifo.
    ///
    /// Fewer elements may be written than requested if there is insufficient room
    /// in the fifo to contain all of them.
    ///
    /// The number of elements actually written is returned.
    ///
    /// `count` must be nonzero.
    pub fn write(&self, elem_size: usize, data: &[u8], count: usize) -> ZxResult<usize> {
        if elem_size != self.elem_size || count == 0 {
            return Err(ZxError::OUT_OF_RANGE);
        }
        let count_size = count * elem_size;
        assert_eq!(data.len(), count_size);

        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        let mut recv_queue = peer.recv_queue.lock();
        let rest_capacity = self.capacity() - recv_queue.len();
        if rest_capacity == 0 {
            return Err(ZxError::SHOULD_WAIT);
        }
        if recv_queue.is_empty() {
            peer.base.signal_set(Signal::READABLE);
        }
        let write_len = count_size.min(rest_capacity);
        recv_queue.extend(&data[..write_len]);
        if recv_queue.len() == self.capacity() {
            self.base.signal_clear(Signal::WRITABLE);
        }
        Ok(write_len / elem_size)
    }

    /// Read data from the FIFO.
    ///
    /// This attempts to read up to `count` elements from the fifo into `data`.
    ///
    /// Fewer elements may be read than requested if there are insufficient elements
    /// in the fifo to fulfill the entire request.
    /// The number of elements actually read is returned.
    ///
    /// The `elem_size` must match the element size that was passed into `Fifo::create()`.
    ///
    /// `data` must have a size of `count * elem_size` bytes.
    ///
    /// `count` must be nonzero.
    pub fn read(&self, elem_size: usize, data: &mut [u8], count: usize) -> ZxResult<usize> {
        if elem_size != self.elem_size || count == 0 {
            return Err(ZxError::OUT_OF_RANGE);
        }
        let count_size = count * elem_size;
        assert_eq!(data.len(), count_size);

        let peer = self.peer.upgrade();
        let mut recv_queue = self.recv_queue.lock();
        if recv_queue.is_empty() {
            if peer.is_none() {
                return Err(ZxError::PEER_CLOSED);
            }
            return Err(ZxError::SHOULD_WAIT);
        }
        let read_size = count_size.min(recv_queue.len());
        if recv_queue.len() == self.capacity() {
            if let Some(peer) = peer {
                peer.base.signal_set(Signal::WRITABLE);
            }
        }
        for (i, x) in recv_queue.drain(..read_size).enumerate() {
            data[i] = x;
        }
        if recv_queue.is_empty() {
            self.base.signal_clear(Signal::READABLE);
        }
        Ok(read_size / elem_size)
    }

    /// The number of elements a `read` of `count` elements could ever return,
    /// or `OUT_OF_RANGE` for the two requests a read refuses outright.
    ///
    /// A `count` of zero is one of them, and answers `OUT_OF_RANGE` rather than
    /// `Ok(0)`: `zx_fifo_read` requires a nonzero count, [`read`] refuses it,
    /// and `sys_fifo_read` has already refused it before this is reached. The
    /// number this would naturally be is zero, so the error is the contract
    /// speaking and not the arithmetic.
    ///
    /// A fifo holds at most `elem_count` elements, so a caller asking for more
    /// is asking for bytes that cannot exist. The syscall layer sizes its
    /// buffer with this rather than with `count`, because both `count` and
    /// `elem_size` arrive raw from userspace and `vec![0; count * elem_size]`
    /// cannot fail: without the bound it is a kernel panic a caller can ask
    /// for, by naming a buffer of its own and a count to match.
    ///
    /// The mismatched `elem_size` is refused here and not only in [`read`],
    /// because otherwise a single element of a gigabyte would be allocated
    /// before `read` ever looked at it.
    ///
    /// [`read`]: Fifo::read
    pub fn read_buffer_elems(&self, elem_size: usize, count: usize) -> ZxResult<usize> {
        if elem_size != self.elem_size || count == 0 {
            return Err(ZxError::OUT_OF_RANGE);
        }
        Ok(count.min(self.elem_count))
    }

    /// Get capacity in bytes.
    fn capacity(&self) -> usize {
        self.elem_size * self.elem_count
    }
}

impl Drop for Fifo {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.upgrade() {
            peer.base
                .signal_change(Signal::WRITABLE, Signal::PEER_CLOSED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn test_basics() {
        let (end0, end1) = Fifo::create(10, 5);
        assert!(Arc::ptr_eq(
            &end0.peer().unwrap().downcast_arc().unwrap(),
            &end1
        ));
        assert_eq!(end0.related_koid(), end1.id());

        drop(end1);
        assert_eq!(end0.peer().unwrap_err(), ZxError::PEER_CLOSED);
        assert_eq!(end0.related_koid(), 0);
    }

    /// The buffer the syscall layer allocates for a read is bounded by the fifo
    /// and not by what the caller asked for. The bug: `sys_fifo_read` sized it
    /// with `count * elem_size` straight from userspace, an allocation that
    /// cannot fail, so a caller with a large mapping of its own could ask the
    /// kernel for that much and panic it.
    #[test]
    fn a_read_buffer_is_never_larger_than_the_fifo_itself() {
        let (end0, _end1) = Fifo::create(10, 5);
        assert_eq!(end0.read_buffer_elems(5, 1), Ok(1));
        assert_eq!(end0.read_buffer_elems(5, 10), Ok(10));
        for count in [11usize, 4096, 1 << 20, usize::MAX / 5] {
            assert_eq!(
                end0.read_buffer_elems(5, count),
                Ok(10),
                "a count of {:#x} was not bounded by the fifo",
                count
            );
        }
    }

    /// An element size that is not the fifo's is refused before anything is
    /// sized by it: one element of a gigabyte is the same panic as a billion
    /// one-byte ones.
    #[test]
    fn a_read_buffer_of_the_wrong_element_size_is_refused_not_allocated() {
        let (end0, _end1) = Fifo::create(10, 5);
        for elem_size in [1usize, 4, 6, 1 << 30, usize::MAX] {
            assert_eq!(
                end0.read_buffer_elems(elem_size, 1),
                Err(ZxError::OUT_OF_RANGE),
                "an element size of {:#x} was accepted",
                elem_size
            );
        }
        assert_eq!(end0.read_buffer_elems(5, 0), Err(ZxError::OUT_OF_RANGE));
    }

    #[test]
    fn read_write() {
        let (end0, end1) = Fifo::create(2, 5);

        assert_eq!(
            end0.write(4, &[0; 9], 1).unwrap_err(),
            ZxError::OUT_OF_RANGE
        );
        assert_eq!(
            end0.write(5, &[0; 0], 0).unwrap_err(),
            ZxError::OUT_OF_RANGE
        );
        let data = (0..15).collect::<Vec<u8>>();
        assert_eq!(end0.write(5, data.as_slice(), 3).unwrap(), 2);
        assert_eq!(
            end0.write(5, data.as_slice(), 3).unwrap_err(),
            ZxError::SHOULD_WAIT
        );

        let mut buf = [0; 15];
        assert_eq!(
            end1.read(4, &mut [0; 4], 1).unwrap_err(),
            ZxError::OUT_OF_RANGE
        );
        assert_eq!(end1.read(5, &mut [], 0).unwrap_err(), ZxError::OUT_OF_RANGE);
        assert_eq!(end1.read(5, &mut buf, 3).unwrap(), 2);
        let mut data = (0..10).collect::<Vec<u8>>();
        data.append(&mut vec![0; 5]);
        assert_eq!(buf, data.as_slice());
        assert_eq!(end1.read(5, &mut buf, 3).unwrap_err(), ZxError::SHOULD_WAIT);

        drop(end1);
        assert_eq!(
            end0.write(5, data.as_slice(), 3).unwrap_err(),
            ZxError::PEER_CLOSED
        );
        assert_eq!(end0.read(5, &mut buf, 3).unwrap_err(), ZxError::PEER_CLOSED);
    }

    // ---------- The signals a fifo publishes ----------
    //
    // READABLE, WRITABLE and PEER_CLOSED are the whole of what a fifo tells
    // anyone waiting on it: `zx_object_wait_one`, a port subscription and
    // every poll on a fifo handle read nothing else. Not one of the six
    // places that move them had a test, so swapping any two of them --
    // asserting READABLE where WRITABLE belongs, clearing the reader's signal
    // instead of the writer's -- passed green.

    /// Which signals does a fifo carry right now?
    fn signals_of(fifo: &Fifo) -> (bool, bool, bool) {
        let s = fifo.base.signal();
        (
            s.contains(Signal::READABLE),
            s.contains(Signal::WRITABLE),
            s.contains(Signal::PEER_CLOSED),
        )
    }

    /// A new fifo is writable at both ends and readable at neither.
    #[test]
    fn a_new_fifo_is_writable_at_both_ends_and_readable_at_neither() {
        let (end0, end1) = Fifo::create(2, 5);
        assert_eq!(signals_of(&end0), (false, true, false));
        assert_eq!(signals_of(&end1), (false, true, false));
    }

    /// A write makes the FAR end readable — the data is queued on the peer,
    /// so the peer is the one with something to read. Asserting it on the
    /// writer instead wakes the wrong side of the pipe: the reader sleeps on
    /// a fifo that has data in it.
    #[test]
    fn a_write_makes_the_far_end_readable_and_not_the_near_one() {
        let (end0, end1) = Fifo::create(2, 5);
        assert_eq!(end0.write(5, &[1u8; 5], 1), Ok(1));

        assert_eq!(
            signals_of(&end1),
            (true, true, false),
            "the end holding the data is the one that became readable"
        );
        assert_eq!(
            signals_of(&end0),
            (false, true, false),
            "the writer became readable for data it sent"
        );
    }

    /// Filling the fifo takes WRITABLE off the WRITER, and the first read at
    /// the other end gives it back. The writer is the side that must stop, and
    /// it is the side whose next `write` would answer `SHOULD_WAIT`.
    #[test]
    fn a_full_fifo_stops_being_writable_until_something_is_read() {
        let (end0, end1) = Fifo::create(2, 5);
        assert_eq!(end0.write(5, &[7u8; 10], 2), Ok(2));

        assert_eq!(
            signals_of(&end0),
            (false, false, false),
            "a writer with a full fifo in front of it is still writable"
        );
        assert_eq!(end0.write(5, &[7u8; 5], 1), Err(ZxError::SHOULD_WAIT));
        assert_eq!(signals_of(&end1), (true, true, false));

        let mut buf = [0u8; 5];
        assert_eq!(end1.read(5, &mut buf, 1), Ok(1));
        assert_eq!(
            signals_of(&end0),
            (false, true, false),
            "room was made and the writer was not told"
        );
        assert_eq!(end0.write(5, &[9u8; 5], 1), Ok(1));
    }

    /// And emptying it takes READABLE off the READER, which is the side whose
    /// next `read` would answer `SHOULD_WAIT`.
    #[test]
    fn an_empty_fifo_stops_being_readable() {
        let (end0, end1) = Fifo::create(2, 5);
        end0.write(5, &[3u8; 5], 1).unwrap();
        assert_eq!(signals_of(&end1), (true, true, false));

        let mut buf = [0u8; 5];
        assert_eq!(end1.read(5, &mut buf, 1), Ok(1));
        assert_eq!(
            signals_of(&end1),
            (false, true, false),
            "an empty fifo was left looking readable"
        );
        assert_eq!(end1.read(5, &mut buf, 1), Err(ZxError::SHOULD_WAIT));
        assert_eq!(
            signals_of(&end0),
            (false, true, false),
            "the writer lost a signal it never had reason to lose"
        );
    }

    /// A read takes what was asked for and leaves the rest queued, even when
    /// the fifo holds more. Draining the whole queue for a one-element request
    /// both loses the caller's data and writes past the buffer it was given.
    #[test]
    fn a_read_takes_what_was_asked_for_and_leaves_the_rest() {
        let (end0, end1) = Fifo::create(4, 2);
        end0.write(2, &[1, 2, 3, 4, 5, 6], 3).unwrap();

        let mut one = [0u8; 2];
        assert_eq!(end1.read(2, &mut one, 1), Ok(1));
        assert_eq!(one, [1, 2]);
        assert_eq!(
            signals_of(&end1),
            (true, true, false),
            "two elements are still queued and the fifo says it is empty"
        );

        let mut rest = [0u8; 4];
        assert_eq!(end1.read(2, &mut rest, 2), Ok(2));
        assert_eq!(rest, [3, 4, 5, 6], "the rest was dropped by the first read");
        assert_eq!(signals_of(&end1), (false, true, false));
    }

    /// Closing one end tells the other: PEER_CLOSED goes up and WRITABLE comes
    /// down, in that order and on the end that is still alive. The two the
    /// other way round is a writer that goes on writing into a fifo whose
    /// reader is gone.
    #[test]
    fn closing_one_end_leaves_the_other_closed_and_not_writable() {
        let (end0, end1) = Fifo::create(2, 5);
        assert_eq!(signals_of(&end0), (false, true, false));

        drop(end1);
        assert_eq!(
            signals_of(&end0),
            (false, false, true),
            "the surviving end was not told its peer had gone"
        );
        assert_eq!(end0.write(5, &[1u8; 5], 1), Err(ZxError::PEER_CLOSED));
    }

    /// A reader whose peer is gone gets PEER_CLOSED and not SHOULD_WAIT once
    /// the queue runs dry: SHOULD_WAIT is a promise that waiting will help.
    #[test]
    fn a_reader_with_no_peer_is_told_so_rather_than_told_to_wait() {
        let (end0, end1) = Fifo::create(2, 5);
        end0.write(5, &[4u8; 5], 1).unwrap();
        drop(end0);

        let mut buf = [0u8; 5];
        assert_eq!(end1.read(5, &mut buf, 1), Ok(1), "queued data survives");
        assert_eq!(end1.read(5, &mut buf, 1), Err(ZxError::PEER_CLOSED));
    }
}
