use {
    crate::object::*,
    alloc::collections::VecDeque,
    alloc::sync::{Arc, Weak},
    bitflags::bitflags,
    kernel_hal::sync::Mutex,
};

/// Bidirectional streaming IPC transport.
///
/// # SYNOPSIS
///
/// Sockets are a bidirectional stream transport.
/// Unlike channels, sockets only move data (not handles).
pub struct Socket {
    base: KObjectBase,
    peer: Weak<Socket>,
    flags: SocketFlags, // constant value
    inner: Mutex<SocketInner>,
}

#[derive(Default)]
struct SocketInner {
    data: VecDeque<u8>,
    datagram_len: VecDeque<usize>,
    read_threshold: usize,
    write_threshold: usize,
    read_disabled: bool,
}

const SOCKET_SIZE: usize = 128 * 2048;

impl_kobject!(Socket
    fn peer(&self) -> ZxResult<Arc<dyn KernelObject>> {
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        Ok(peer)
    }
    fn related_koid(&self) -> KoID {
        self.peer.upgrade().map(|p| p.id()).unwrap_or(0)
    }
);

bitflags! {
    /// Signals that waitable kernel objects expose to applications.
    #[derive(Default)]
    pub struct SocketFlags: u32 {
        #[allow(clippy::identity_op)]
        // These options can be passed to socket_shutdown().
        /// Via this option to `socket_shutdown()`, one end of the socket can be closed for writing.
        const SHUTDOWN_WRITE                = 1;
        /// Via this option to `socket_shutdown()`, one end of the socket can be closed for reading.
        const SHUTDOWN_READ                 = 1 << 1;
        /// Valid flags of `socket_shutdown()`.
        const SHUTDOWN_MASK                 = Self::SHUTDOWN_WRITE.bits | Self::SHUTDOWN_READ.bits;

        // These can be passed to socket_create().
        // const STREAM                     = 0; // Don't use contains
        /// Create a datagram socket. See [`read`] and [`write`] for details.
        ///
        /// [`read`]: struct.Socket.html#method.read
        /// [`write`]: struct.Socket.html#method.write
        const DATAGRAM                      = 1;
        /// Valid flags of `socket_create()`.
        const CREATE_MASK                   = Self::DATAGRAM.bits;

        // These can be passed to socket_read().
        /// Leave the message in the socket.
        const SOCKET_PEEK                   = 1 << 3;
    }
}

impl Socket {
    /// Create a socket.
    #[allow(unsafe_code)]
    pub fn create(flags: u32) -> ZxResult<(Arc<Self>, Arc<Self>)> {
        let flags = SocketFlags::from_bits(flags).ok_or(ZxError::INVALID_ARGS)?;
        if !(flags - SocketFlags::CREATE_MASK).is_empty() {
            return Err(ZxError::INVALID_ARGS);
        }
        let starting_signals: Signal = Signal::WRITABLE;
        let end0 = Arc::new(Socket {
            base: KObjectBase::with_signal(starting_signals),
            peer: Weak::default(),
            flags,
            inner: Default::default(),
        });
        let end1 = Arc::new(Socket {
            base: KObjectBase::with_signal(starting_signals),
            peer: Arc::downgrade(&end0),
            flags,
            inner: Default::default(),
        });
        // no other reference of `end0`
        unsafe { &mut *(Arc::as_ptr(&end0) as *mut Socket) }.peer = Arc::downgrade(&end1);
        Ok((end0, end1))
    }

    /// Write data to the socket. If successful, the number of bytes actually written are returned.
    ///
    /// A **SOCKET_STREAM**(default) socket write can be short if the socket does not have
    /// enough space for all of *data*.
    /// Otherwise, if the socket was already full, the call returns **ZxError::SHOULD_WAIT**.
    ///
    ///
    /// A **SOCKET_DATAGRAM** socket write is never short. If the socket has
    /// insufficient space for *data*, it writes nothing and returns
    /// **ZxError::SHOULD_WAIT**. Attempting to write a packet larger than the datagram socket's
    /// capacity will fail with **ZxError::OUT_OF_RANGE**.
    pub fn write(&self, data: &[u8]) -> ZxResult<usize> {
        if self.base.signal().contains(Signal::SOCKET_WRITE_DISABLED) {
            return Err(ZxError::BAD_STATE);
        }
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        let actual_count = peer.write_data(data)?;
        if actual_count > 0 {
            // One lock at a time, and the same goes for `read` and `get_info`.
            // This used to hold the PEER's `inner` and then take its own, while
            // `read` took its own and then the peer's: an endpoint being
            // written by one thread and read by another (or the two ends used
            // at the same time) left each thread holding the lock the other was
            // waiting for. These are spin locks taken with interrupts off, so
            // that is not a slow path, it is a CPU that never comes back.
            let peer_rest_size = SOCKET_SIZE - peer.inner.lock().data.len();
            let write_threshold = self.inner.lock().write_threshold;
            let mut clear = Signal::empty();
            if peer_rest_size == 0 {
                clear |= Signal::WRITABLE;
            }
            if write_threshold > 0 && peer_rest_size < write_threshold {
                clear |= Signal::SOCKET_WRITE_THRESHOLD;
            }
            self.base.signal_clear(clear);
        }
        Ok(actual_count)
    }

    /// Return the number of bytes a write would consume from the user buffer.
    pub fn write_size(&self, count: usize) -> ZxResult<usize> {
        if count > u32::MAX as usize {
            return Err(ZxError::INVALID_ARGS);
        }
        if self.base.signal().contains(Signal::SOCKET_WRITE_DISABLED) {
            return Err(ZxError::BAD_STATE);
        }
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        let rest_size = SOCKET_SIZE - peer.inner.lock().data.len();
        if rest_size == 0 {
            return Err(ZxError::SHOULD_WAIT);
        }
        if self.flags.contains(SocketFlags::DATAGRAM) {
            if count > SOCKET_SIZE {
                return Err(ZxError::OUT_OF_RANGE);
            }
            if count > rest_size {
                return Err(ZxError::SHOULD_WAIT);
            }
            Ok(count)
        } else {
            Ok(count.min(rest_size))
        }
    }

    /// Measure the free room and fill it under **one** hold of the lock.
    ///
    /// These were two: the room was measured, the lock let go, and the push
    /// took it again. Two writers on the same endpoint both measured the same
    /// room and both wrote it, so `data` went past `SOCKET_SIZE` -- and from
    /// then on every `SOCKET_SIZE - data.len()` in `write`, `read` and
    /// `write_size` underflows, so the socket reports room for the rest of the
    /// address space and grows until the kernel heap is gone.
    fn write_data(&self, data: &[u8]) -> ZxResult<usize> {
        let (was_empty, actual_count) = {
            let mut inner = self.inner.lock();
            let curr_size = inner.data.len();
            let was_empty = curr_size == 0;
            let rest_size = SOCKET_SIZE - curr_size;
            if rest_size == 0 {
                return Err(ZxError::SHOULD_WAIT);
            }
            let write_size = data.len().min(rest_size);
            let actual_count = if self.flags.contains(SocketFlags::DATAGRAM) {
                if data.len() > SOCKET_SIZE {
                    return Err(ZxError::OUT_OF_RANGE);
                }
                if data.len() > rest_size {
                    return Err(ZxError::SHOULD_WAIT);
                }
                Self::write_datagram(&mut inner, &data[..write_size])?
            } else {
                Self::write_stream(&mut inner, &data[..write_size])
            };
            (was_empty, actual_count)
        };
        if actual_count > 0 {
            let mut set = Signal::empty();
            if was_empty {
                set |= Signal::READABLE;
            }
            // Read the two values and let go: `signal_change` runs the
            // observers' callbacks with its own lock held, and holding one of
            // ours across that is how a re-entrant acquire happens.
            let (size, read_threshold) = {
                let inner = self.inner.lock();
                (inner.data.len(), inner.read_threshold)
            };
            if read_threshold > 0 && size >= read_threshold {
                set |= Signal::SOCKET_READ_THRESHOLD;
            }
            self.base.signal_set(set);
        }
        Ok(actual_count)
    }

    /// Takes the guard its caller already holds: the measure and the push are
    /// one step, see [`Socket::write_data`].
    fn write_datagram(inner: &mut SocketInner, data: &[u8]) -> ZxResult<usize> {
        if data.is_empty() {
            return Err(ZxError::INVALID_ARGS);
        }
        let actual_count = data.len();
        inner.data.extend(data);
        inner.datagram_len.push_back(actual_count);
        Ok(actual_count)
    }

    /// Takes the guard its caller already holds; see [`Socket::write_data`].
    fn write_stream(inner: &mut SocketInner, data: &[u8]) -> usize {
        inner.data.extend(data);
        data.len()
    }

    /// The buffer a read of `count` bytes needs, which is never more than the
    /// socket can hold.
    ///
    /// A socket holds at most `SOCKET_SIZE` bytes and a datagram is bounded
    /// by the same size, so bytes past it can never be filled. The syscall
    /// layer sizes its buffer with this rather than with `count`, because
    /// `count` arrives raw from userspace and `vec![0; count]` cannot fail:
    /// without the bound a caller that maps a gigabyte of its own can ask the
    /// kernel for a gigabyte too, and panic it.
    ///
    /// Clamping cannot change what a read returns, which is why it is a clamp
    /// and not an error.
    pub const fn read_buffer_len(count: usize) -> usize {
        if count > SOCKET_SIZE {
            SOCKET_SIZE
        } else {
            count
        }
    }

    /// Read data from the socket. If successful, the number of bytes actually read are returned.
    ///
    /// If the socket was created with **SOCKET_DATAGRAM**, this method reads
    /// only the first available datagram in the socket (if one is present).
    /// If *data* is too small for the datagram, then the read will be
    /// truncated, and any remaining bytes in the datagram will be discarded.
    ///
    /// If `peek` is true, leave the message in the socket. Otherwise consume the message.
    pub fn read(&self, peek: bool, data: &mut [u8]) -> ZxResult<usize> {
        // The look and the take are one step, under one hold of the lock.
        // They used to be two: this decided the socket was not empty, let go,
        // and the take below took the lock again. Two threads reading the same
        // endpoint -- two threads of one process, or two processes sharing the
        // handle -- both got past the check, and on a datagram socket the
        // second one reached `datagram_len.pop_front().unwrap()` with the
        // queue already drained, **which panics the kernel**. On a stream
        // socket it was quieter and still wrong: the read of an empty buffer
        // answered `Ok(0)` where the contract says `SHOULD_WAIT`, and a
        // userspace loop reads that as the end of the stream.
        let (was_full, actual_count) = {
            let mut inner = self.inner.lock();
            if inner.data.is_empty() {
                // `PEER_CLOSED` first, as before: a peer that is gone is the
                // reason the buffer is never going to fill.
                if self.peer.upgrade().is_none() {
                    return Err(ZxError::PEER_CLOSED);
                }
                if inner.read_disabled {
                    return Err(ZxError::BAD_STATE);
                }
                return Err(ZxError::SHOULD_WAIT);
            }
            let was_full = inner.data.len() == SOCKET_SIZE;
            let actual_count = if self.flags.contains(SocketFlags::DATAGRAM) {
                Self::read_datagram(&mut inner, data, peek)
            } else {
                Self::read_stream(&mut inner, data, peek)
            };
            (was_full, actual_count)
        };
        if !peek && actual_count > 0 {
            // One lock at a time: see the note in `write`.
            let (self_size, read_threshold) = {
                let inner = self.inner.lock();
                (inner.data.len(), inner.read_threshold)
            };
            let mut clear = Signal::empty();
            if read_threshold > 0 && self_size < read_threshold {
                clear |= Signal::SOCKET_READ_THRESHOLD;
            }
            if self_size == 0 {
                clear |= Signal::READABLE;
            }
            self.base.signal_clear(clear);
            if let Some(peer) = self.peer.upgrade() {
                let peer_write_threshold = peer.inner.lock().write_threshold;
                let mut set = Signal::empty();
                if peer_write_threshold > 0 && SOCKET_SIZE - self_size >= peer_write_threshold {
                    set |= Signal::SOCKET_WRITE_THRESHOLD;
                }
                // Draining a full socket makes room, but room is not
                // permission: once writing on that end is shut down,
                // `WRITABLE` never comes back. This is the only place it is
                // ever re-asserted, and it used to re-assert it regardless, so
                // a peer parked on `WRITABLE` was woken to a `write` that can
                // only answer `BAD_STATE`, over and over.
                if was_full && !peer.base.signal().contains(Signal::SOCKET_WRITE_DISABLED) {
                    set |= Signal::WRITABLE;
                }
                peer.base.signal_set(set);
            }
        }
        Ok(actual_count)
    }

    /// Takes the guard its caller already holds, which is what makes the two
    /// `datagram_len` reads below safe; see [`Socket::read`].
    fn read_datagram(inner: &mut SocketInner, data: &mut [u8], peek: bool) -> usize {
        if data.is_empty() {
            return 0;
        }
        // The caller has established, under this same guard, that `data` is
        // not empty, and every byte in it belongs to a datagram whose length
        // is in this queue.
        let datagram_len = if peek {
            match inner.datagram_len.front() {
                Some(len) => *len,
                None => return 0,
            }
        } else {
            match inner.datagram_len.pop_front() {
                Some(len) => len,
                None => return 0,
            }
        };
        let read_size = data.len().min(datagram_len);
        if peek {
            for (i, x) in inner.data.iter().take(read_size).enumerate() {
                data[i] = *x;
            }
        } else {
            for (i, x) in inner.data.drain(..datagram_len).take(read_size).enumerate() {
                data[i] = x;
            }
        };
        read_size
    }

    /// Takes the guard its caller already holds; see [`Socket::read`].
    fn read_stream(inner: &mut SocketInner, data: &mut [u8], peek: bool) -> usize {
        let read_size = data.len().min(inner.data.len());
        if peek {
            for (i, x) in inner.data.iter().take(read_size).enumerate() {
                data[i] = *x;
            }
        } else {
            for (i, x) in inner.data.drain(..read_size).enumerate() {
                data[i] = x;
            }
        };
        read_size
    }

    /// Get information of the socket.
    pub fn get_info(&self) -> SocketInfo {
        // One lock at a time: see the note in `write`.
        let (self_size, rx_buf_available) = {
            let inner = self.inner.lock();
            let self_size = inner.data.len();
            let available = if self.flags.contains(SocketFlags::DATAGRAM) {
                *inner.datagram_len.front().unwrap_or(&0)
            } else {
                self_size
            };
            (self_size, available)
        };
        let mut info = SocketInfo {
            options: self.flags.bits() as _,
            padding1: 0,
            rx_buf_max: SOCKET_SIZE as _,
            rx_buf_size: self_size as _,
            rx_buf_available: rx_buf_available as _,
            tx_buf_max: 0,
            tx_buf_size: 0,
        };
        if let Some(peer) = self.peer.upgrade() {
            info.tx_buf_size = peer.inner.lock().data.len() as u64;
            info.tx_buf_max = SOCKET_SIZE as u64;
        };
        info
    }

    /// Prevent reading or writing.
    ///
    /// The two directions **cross** at the peer: nothing more can arrive here
    /// once this end stops reading, so the peer stops writing, and the peer
    /// stops reading once this end stops writing. Negating them instead --
    /// which is what this used to do -- happens to look right whenever exactly
    /// one direction is named, and is wrong in both the other cases:
    ///
    /// - `shutdown(read, write)` with both named told the peer **nothing**, so
    ///   a peer blocked writing was never woken and never saw
    ///   `SOCKET_WRITE_DISABLED` or `SOCKET_PEER_WRITE_DISABLED`; and
    /// - `shutdown` with neither named -- which `zx_socket_shutdown(s, 0)`
    ///   reaches -- shut the peer down in **both** directions, so a call that
    ///   asks for nothing killed the other end.
    pub fn shutdown(&self, read: bool, write: bool) -> ZxResult {
        self.shutdown_self(read, write)?;
        if let Some(peer) = self.peer.upgrade() {
            peer.shutdown_self(write, read)?;
        }
        Ok(())
    }

    /// Change the write disposition of this endpoint and/or its peer.
    /// `Some(true)` disables writes, `Some(false)` enables them, and `None`
    /// leaves that endpoint unchanged.
    pub fn set_disposition(
        &self,
        disposition: Option<bool>,
        peer_disposition: Option<bool>,
    ) -> ZxResult {
        let peer = self.peer.upgrade();

        // Validate both changes before applying either one.
        if disposition == Some(false) {
            if let Some(peer) = &peer {
                if peer
                    .base
                    .signal()
                    .contains(Signal::SOCKET_PEER_WRITE_DISABLED)
                    && !peer.inner.lock().data.is_empty()
                {
                    return Err(ZxError::BAD_STATE);
                }
            }
        }
        if peer_disposition == Some(false)
            && self
                .base
                .signal()
                .contains(Signal::SOCKET_PEER_WRITE_DISABLED)
            && !self.inner.lock().data.is_empty()
        {
            return Err(ZxError::BAD_STATE);
        }

        if let Some(disabled) = disposition {
            if disabled {
                self.base
                    .signal_change(Signal::WRITABLE, Signal::SOCKET_WRITE_DISABLED);
            } else {
                self.base
                    .signal_change(Signal::SOCKET_WRITE_DISABLED, Signal::WRITABLE);
            }
            if let Some(peer) = &peer {
                peer.inner.lock().read_disabled = disabled;
                if disabled {
                    peer.base.signal_set(Signal::SOCKET_PEER_WRITE_DISABLED);
                } else {
                    peer.base.signal_clear(Signal::SOCKET_PEER_WRITE_DISABLED);
                }
            }
        }

        if let Some(disabled) = peer_disposition {
            self.inner.lock().read_disabled = disabled;
            if let Some(peer) = &peer {
                if disabled {
                    peer.base
                        .signal_change(Signal::WRITABLE, Signal::SOCKET_WRITE_DISABLED);
                    self.base.signal_set(Signal::SOCKET_PEER_WRITE_DISABLED);
                } else {
                    peer.base
                        .signal_change(Signal::SOCKET_WRITE_DISABLED, Signal::WRITABLE);
                    self.base.signal_clear(Signal::SOCKET_PEER_WRITE_DISABLED);
                }
            }
        }
        Ok(())
    }

    fn shutdown_self(&self, read: bool, write: bool) -> ZxResult {
        let mut set = Signal::empty();
        let mut clear = Signal::empty();
        {
            let mut inner = self.inner.lock();
            if read {
                inner.read_disabled = true;
                set |= Signal::SOCKET_PEER_WRITE_DISABLED;
            }
        }
        if write {
            clear |= Signal::WRITABLE;
            set |= Signal::SOCKET_WRITE_DISABLED;
        }
        // Outside the lock, as in `read` and `write_data`.
        self.base.signal_change(clear, set);
        Ok(())
    }

    /// Set the read threshold of the socket.
    ///
    /// When the bytes queued on the socket (available for reading) is equal to
    /// or greater than this value, the **SOCKET_READ_THRESHOLD** signal is asserted.
    /// Read threshold signalling is disabled by default (and when set, writing
    /// a value of 0 for this property disables it).
    pub fn set_read_threshold(&self, threshold: usize) -> ZxResult {
        if threshold > SOCKET_SIZE {
            return Err(ZxError::INVALID_ARGS);
        }
        let queued = {
            let mut inner = self.inner.lock();
            inner.read_threshold = threshold;
            inner.data.len()
        };
        // Outside the lock, as in `read` and `write_data`.
        if threshold > 0 && queued >= threshold {
            self.base.signal_set(Signal::SOCKET_READ_THRESHOLD);
        } else {
            self.base.signal_clear(Signal::SOCKET_READ_THRESHOLD);
        }
        Ok(())
    }

    /// Set the write threshold of the socket.
    ///
    /// When the space available for writing on the socket is equal to or
    /// greater than this value, the **SOCKET_WRITE_THRESHOLD** signal is asserted.
    /// Write threshold signalling is disabled by default (and when set, writing a
    /// value of 0 for this property disables it).
    pub fn set_write_threshold(&self, threshold: usize) -> ZxResult {
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        if threshold > SOCKET_SIZE {
            return Err(ZxError::INVALID_ARGS);
        }
        self.inner.lock().write_threshold = threshold;
        if threshold == 0 {
            self.base.signal_clear(Signal::SOCKET_WRITE_THRESHOLD);
        } else if SOCKET_SIZE - peer.inner.lock().data.len() >= threshold {
            self.base.signal_set(Signal::SOCKET_WRITE_THRESHOLD);
        } else {
            self.base.signal_clear(Signal::SOCKET_WRITE_THRESHOLD);
        }
        Ok(())
    }

    /// Get the read and write thresholds of the socket.
    pub fn get_rx_tx_threshold(&self) -> (usize, usize) {
        let inner = self.inner.lock();
        (inner.read_threshold, inner.write_threshold)
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.upgrade() {
            peer.signal_change(Signal::WRITABLE, Signal::PEER_CLOSED);
        }
    }
}

/// The information of a socket
#[repr(C)]
#[derive(Debug, Eq, PartialEq)]
pub struct SocketInfo {
    options: u32,
    padding1: u32,
    rx_buf_max: u64,
    rx_buf_size: u64,
    rx_buf_available: u64,
    tx_buf_max: u64,
    tx_buf_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, Ordering};

    /// Both ends of a socket used at once must not wedge. A socket is
    /// bidirectional, so each side writes its own endpoint and reads it: that
    /// is the ordinary duplex pattern, one thread per end.
    ///
    /// `write` held the PEER's `inner` and then took its own, while `read` and
    /// `get_info` took their own and then the peer's. So `end0.write` wanted
    /// (end1, end0) and `end1.write` wanted (end0, end1) -- the same two spin
    /// locks in opposite orders, and neither thread ever let go. They are
    /// taken with interrupts off, so it is not a slow path, it is two CPUs
    /// that never come back.
    ///
    /// The watchdog is the point: with the orders crossed this test **hangs**,
    /// and a hang says nothing. The timeout turns it into a failure with a
    /// name on it.
    #[test]
    fn both_ends_of_a_socket_used_at_once_do_not_wedge() {
        use std::sync::mpsc;
        use std::time::Duration;

        const ROUNDS: usize = 50_000;
        let (end0, end1) = Socket::create(0).unwrap();
        // Thresholds on both ends, so every round takes the branches that
        // used to want the other endpoint's lock.
        for end in [&end0, &end1] {
            end.set_read_threshold(1).unwrap();
            end.set_write_threshold(1).unwrap();
        }

        let (done, finished) = mpsc::channel();
        let ends = [end0, end1].map(|end| {
            let done = done.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8];
                for _ in 0..ROUNDS {
                    let _ = end.write(b"xyz");
                    let _ = end.read(false, &mut buf);
                    let _ = end.get_info();
                }
                let _ = done.send(());
            })
        });

        for _ in 0..2 {
            finished
                .recv_timeout(Duration::from_secs(30))
                .expect("the two ends of one socket deadlocked");
        }
        for end in ends {
            end.join().unwrap();
        }
    }

    /// The buffer the syscall layer allocates for a read is bounded by the
    /// socket and not by what the caller asked for. The bug: `sys_socket_read`
    /// sized it with `count` straight from userspace, an allocation that cannot
    /// fail, so a caller with a large mapping of its own could ask the kernel
    /// for that much and panic it.
    #[test]
    fn a_read_buffer_is_never_larger_than_the_socket_itself() {
        assert_eq!(Socket::read_buffer_len(0), 0);
        assert_eq!(Socket::read_buffer_len(1), 1);
        assert_eq!(Socket::read_buffer_len(SOCKET_SIZE), SOCKET_SIZE);
        for count in [SOCKET_SIZE + 1, 1 << 30, usize::MAX] {
            assert_eq!(
                Socket::read_buffer_len(count),
                SOCKET_SIZE,
                "a count of {:#x} was not bounded by the socket",
                count
            );
        }
    }

    /// The clamp is sound only because a read can never hand back more than the
    /// socket holds: a write of twice the capacity keeps exactly the capacity,
    /// so nothing past it was ever there to read.
    #[test]
    fn the_socket_never_holds_more_than_the_clamp_allows() {
        let (end0, end1) = Socket::create(0).unwrap();
        assert_eq!(end0.write(&[7u8; SOCKET_SIZE * 2]).unwrap(), SOCKET_SIZE);
        let mut buf = vec![0u8; Socket::read_buffer_len(usize::MAX)];
        assert_eq!(end1.read(false, &mut buf).unwrap(), SOCKET_SIZE);
        assert_eq!(
            end1.read(false, &mut buf).unwrap_err(),
            ZxError::SHOULD_WAIT
        );
    }

    #[test]
    fn test_basics() {
        assert_eq!(Socket::create(1 << 10).unwrap_err(), ZxError::INVALID_ARGS);
        assert_eq!(
            Socket::create(SocketFlags::SOCKET_PEEK.bits).unwrap_err(),
            ZxError::INVALID_ARGS
        );
        let (end0, end1) = Socket::create(1).unwrap();
        assert!(Arc::ptr_eq(
            &end0.peer().unwrap().downcast_arc().unwrap(),
            &end1
        ));
        assert_eq!(end0.related_koid(), end1.id());

        drop(end1);
        assert_eq!(end0.peer().unwrap_err(), ZxError::PEER_CLOSED);
        assert_eq!(end0.related_koid(), 0);
    }

    #[test]
    fn test_stream() {
        let (end0, end1) = Socket::create(0).unwrap();

        // empty read & write
        assert_eq!(
            end0.read(false, &mut [0; 10]).unwrap_err(),
            ZxError::SHOULD_WAIT
        );
        assert_eq!(end0.write(&[]).unwrap(), 0);

        assert_eq!(end0.write(&[1, 2, 3]), Ok(3));
        let mut buf = [0u8; 4];
        assert_eq!(end1.read(true, &mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3, 0]);
        buf = [0; 4];
        // can read again
        assert_eq!(end1.read(true, &mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3, 0]);
        assert_eq!(
            end0.get_info(),
            SocketInfo {
                options: 0,
                padding1: 0,
                rx_buf_max: SOCKET_SIZE as _,
                rx_buf_size: 0,
                rx_buf_available: 0,
                tx_buf_max: SOCKET_SIZE as _,
                tx_buf_size: 3,
            }
        );

        // use a small buffer now
        let mut buf = [0u8; 2];
        assert_eq!(end1.read(true, &mut buf).unwrap(), 2);
        assert_eq!(buf, [1, 2]);
        // consume
        assert_eq!(end1.read(false, &mut buf).unwrap(), 2);
        assert_eq!(buf, [1, 2]);
        assert_eq!(end1.read(false, &mut buf).unwrap(), 1);
        assert_eq!(buf, [3, 2]);

        end1.write(&[111, 233, 222]).unwrap();
        assert_eq!(
            end0.get_info(),
            SocketInfo {
                options: 0,
                padding1: 0,
                rx_buf_max: SOCKET_SIZE as _,
                rx_buf_size: 3,
                rx_buf_available: 3,
                tx_buf_max: SOCKET_SIZE as _,
                tx_buf_size: 0,
            }
        );

        // write much data
        assert_eq!(end0.write(&[0; SOCKET_SIZE * 2]).unwrap(), SOCKET_SIZE);
        assert_eq!(end0.write(&[0; 1]).unwrap_err(), ZxError::SHOULD_WAIT);
        assert!(!end0.signal().contains(Signal::WRITABLE));
        end1.read(false, &mut [0; 1]).unwrap();
        assert!(end0.signal().contains(Signal::WRITABLE));
    }

    #[test]
    fn test_datagram() {
        let (end0, end1) = Socket::create(1).unwrap();

        // empty read & write
        assert_eq!(
            end0.read(false, &mut [0; 10]).unwrap_err(),
            ZxError::SHOULD_WAIT
        );
        assert_eq!(end0.write(&[]).unwrap_err(), ZxError::INVALID_ARGS);

        assert_eq!(end0.write(&[1, 2, 3]), Ok(3));
        assert_eq!(end0.write(&[4, 5, 6, 7]), Ok(4));

        let mut buf = [0u8; 4];
        assert_eq!(end1.read(true, &mut []).unwrap(), 0);
        assert_eq!(end1.read(true, &mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3, 0]);
        buf = [0; 4];
        // can read again
        assert_eq!(end1.read(true, &mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3, 0]);
        assert_eq!(
            end0.get_info(),
            SocketInfo {
                options: SocketFlags::DATAGRAM.bits,
                padding1: 0,
                rx_buf_max: SOCKET_SIZE as _,
                rx_buf_size: 0,
                rx_buf_available: 0,
                tx_buf_max: SOCKET_SIZE as _,
                tx_buf_size: 7,
            }
        );
        assert_eq!(
            end1.get_info(),
            SocketInfo {
                options: SocketFlags::DATAGRAM.bits,
                padding1: 0,
                rx_buf_max: SOCKET_SIZE as _,
                rx_buf_size: 7,
                rx_buf_available: 3,
                tx_buf_max: SOCKET_SIZE as _,
                tx_buf_size: 0,
            }
        );

        // use a small buffer now
        let mut buf = [0u8; 2];
        assert_eq!(end1.read(true, &mut buf).unwrap(), 2);
        assert_eq!(buf, [1, 2]);
        // consume
        assert_eq!(end1.read(false, &mut buf).unwrap(), 2);
        assert_eq!(buf, [1, 2]);
        assert_eq!(end1.read(false, &mut buf).unwrap(), 2);
        assert_eq!(buf, [4, 5]);

        // write much data
        let (end0, end1) = Socket::create(1).unwrap();
        assert_eq!(
            end0.write(&[0; SOCKET_SIZE * 2]).unwrap_err(),
            ZxError::OUT_OF_RANGE
        );
        assert_eq!(end0.write(&[0; SOCKET_SIZE]).unwrap(), SOCKET_SIZE);
        assert!(!end0.signal().contains(Signal::WRITABLE));
        end1.read(false, &mut [0; 1]).unwrap();
        assert!(end0.signal().contains(Signal::WRITABLE));
    }

    #[test]
    fn test_threshold() {
        let (end0, end1) = Socket::create(0).unwrap();
        assert_eq!(end0.get_rx_tx_threshold(), (0, 0));

        // write
        assert_eq!(
            end0.set_write_threshold(SOCKET_SIZE * 2).unwrap_err(),
            ZxError::INVALID_ARGS
        );
        // have space when setting threshold
        assert!(end0.set_write_threshold(10).is_ok());
        assert!(end0.signal().contains(Signal::SOCKET_WRITE_THRESHOLD));
        assert_eq!(end0.get_rx_tx_threshold(), (0, 10));
        end0.write(&[0; SOCKET_SIZE - 9]).unwrap();
        assert!(!end0.signal().contains(Signal::SOCKET_WRITE_THRESHOLD));
        // no space when setting threshold
        assert!(end0.set_write_threshold(20).is_ok());
        assert!(!end0.signal().contains(Signal::SOCKET_WRITE_THRESHOLD));
        end1.read(false, &mut [0; 10]).unwrap();
        assert!(!end0.signal().contains(Signal::SOCKET_WRITE_THRESHOLD));
        end1.read(false, &mut [0; 1]).unwrap();
        assert!(end0.signal().contains(Signal::SOCKET_WRITE_THRESHOLD));
        // disable threshold
        assert!(end0.set_write_threshold(0).is_ok());
        assert!(!end0.signal().contains(Signal::SOCKET_WRITE_THRESHOLD));

        // read
        assert_eq!(
            end0.set_read_threshold(SOCKET_SIZE * 2).unwrap_err(),
            ZxError::INVALID_ARGS
        );
        // have data when setting threshold
        end1.write(&[0; 10]).unwrap();
        assert!(end0.set_read_threshold(10).is_ok());
        assert!(end0.signal().contains(Signal::SOCKET_READ_THRESHOLD));
        assert_eq!(end0.get_rx_tx_threshold(), (10, 0));
        end0.read(false, &mut [0; 1]).unwrap();
        assert!(!end0.signal().contains(Signal::SOCKET_READ_THRESHOLD));
        // no data when setting threshold
        end0.read(false, &mut [0; 10]).unwrap();
        assert!(end0.set_read_threshold(20).is_ok());
        assert!(!end0.signal().contains(Signal::SOCKET_WRITE_THRESHOLD));
        end1.write(&[0; 10]).unwrap();
        assert!(!end0.signal().contains(Signal::SOCKET_READ_THRESHOLD));
        end1.write(&[0; 10]).unwrap();
        assert!(end0.signal().contains(Signal::SOCKET_READ_THRESHOLD));
        // disable threshold
        assert!(end0.set_read_threshold(0).is_ok());
        assert!(!end0.signal().contains(Signal::SOCKET_READ_THRESHOLD));
    }

    #[test]
    fn test_shutdown() {
        let (end0, end1) = Socket::create(0).unwrap();
        end0.write(&[0; 10]).unwrap();

        assert!(end1.shutdown(true, false).is_ok());
        assert_eq!(end0.write(&[0; 1]).unwrap_err(), ZxError::BAD_STATE);
        assert!(!end0.signal().contains(Signal::WRITABLE));
        // buffered data can be read
        assert!(end1.signal().contains(Signal::READABLE));
        assert_eq!(end1.read(false, &mut [0; 20]).unwrap(), 10);
        // no more data
        assert!(!end1.signal().contains(Signal::READABLE));
        assert_eq!(
            end1.read(false, &mut [0; 20]).unwrap_err(),
            ZxError::BAD_STATE
        );
        // the opposite direction is still okay
        assert_eq!(end1.write(&[0; 1]).unwrap(), 1);
        assert_eq!(end0.read(false, &mut [0; 10]).unwrap(), 1);
    }

    #[test]
    /// Shutting both directions down has to reach the peer, and it used to
    /// reach nothing: the two flags were negated instead of crossed, so
    /// `(true, true)` became `(false, false)` at the peer. A peer parked in a
    /// write then had nothing to wake it and no signal to tell it why.
    fn shutting_down_both_directions_tells_the_peer_too() {
        let (end0, end1) = Socket::create(0).unwrap();
        end1.write(&[0; 10]).unwrap();

        end0.shutdown(true, true).unwrap();

        assert!(!end0.signal().contains(Signal::WRITABLE));
        assert!(end0.signal().contains(Signal::SOCKET_WRITE_DISABLED));
        assert!(end0.signal().contains(Signal::SOCKET_PEER_WRITE_DISABLED));

        assert!(
            !end1.signal().contains(Signal::WRITABLE),
            "the peer must not still look writable",
        );
        assert!(
            end1.signal().contains(Signal::SOCKET_WRITE_DISABLED),
            "nothing this end writes can be read any more",
        );
        assert!(
            end1.signal().contains(Signal::SOCKET_PEER_WRITE_DISABLED),
            "and nothing more is coming from the other one",
        );
        assert_eq!(end1.write(&[0; 1]).unwrap_err(), ZxError::BAD_STATE);

        // What was already buffered is still readable, as after a one-sided
        // shutdown, and the end of it is `BAD_STATE` rather than a wait.
        assert_eq!(end0.read(false, &mut [0; 20]).unwrap(), 10);
        assert_eq!(
            end0.read(false, &mut [0; 20]).unwrap_err(),
            ZxError::BAD_STATE,
        );
    }

    #[test]
    /// And the other end of the same mistake: asking for neither direction
    /// negated to "both" at the peer, so `zx_socket_shutdown(s, 0)` -- which
    /// the syscall let through -- shut the other end down completely.
    fn a_shutdown_of_neither_direction_shuts_nothing_down() {
        let (end0, end1) = Socket::create(0).unwrap();
        end0.shutdown(false, false).unwrap();

        for (name, end) in [("end0", &end0), ("end1", &end1)] {
            assert!(end.signal().contains(Signal::WRITABLE), "{}", name);
            assert!(
                !end.signal().contains(Signal::SOCKET_WRITE_DISABLED),
                "{}",
                name,
            );
            assert!(
                !end.signal().contains(Signal::SOCKET_PEER_WRITE_DISABLED),
                "{}",
                name,
            );
        }
        assert_eq!(end0.write(&[0; 4]).unwrap(), 4);
        assert_eq!(end1.read(false, &mut [0; 4]).unwrap(), 4);
        assert_eq!(end1.write(&[0; 4]).unwrap(), 4);
        assert_eq!(end0.read(false, &mut [0; 4]).unwrap(), 4);
    }

    #[test]
    /// Draining a full socket makes room, and room is not permission: the
    /// only place `WRITABLE` is ever re-asserted is the read that empties a
    /// full socket, and it did so even for an end whose writes had been shut
    /// down. A peer parked on `WRITABLE` was then woken to a `write` that can
    /// only answer `BAD_STATE`.
    fn a_read_that_makes_room_does_not_make_a_shut_down_end_writable() {
        let (end0, end1) = Socket::create(0).unwrap();

        // Fill it from end0, so end1 holds a full buffer.
        let full = vec![0u8; SOCKET_SIZE];
        assert_eq!(end0.write(&full).unwrap(), SOCKET_SIZE);
        assert!(!end0.signal().contains(Signal::WRITABLE), "no room left");

        // Then shut writing down on end0, and drain from end1.
        end0.shutdown(false, true).unwrap();
        assert!(end0.signal().contains(Signal::SOCKET_WRITE_DISABLED));
        assert_eq!(
            end1.read(false, &mut vec![0u8; SOCKET_SIZE]).unwrap(),
            SOCKET_SIZE
        );

        assert!(
            !end0.signal().contains(Signal::WRITABLE),
            "there is room again, but this end may not write",
        );
        assert_eq!(end0.write(&[0; 1]).unwrap_err(), ZxError::BAD_STATE);
    }

    /// Runs `body` on its own thread and turns a wedge or a panicked
    /// participant into a named failure: a test that hangs says nothing, and
    /// these put two threads inside one endpoint on purpose.
    fn with_watchdog(what: &str, body: impl FnOnce() + Send + 'static) {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            body();
            let _ = done.send(());
        });
        if finished
            .recv_timeout(core::time::Duration::from_secs(30))
            .is_err()
        {
            panic!("{}", what);
        }
    }

    #[test]
    /// Two threads reading one endpoint, kept on the boundary: the writer only
    /// sends the next datagram once the socket is empty again, so the readers
    /// are nearly always racing over the last one. The look and the take used
    /// to be two separate holds of the lock, so both got past "the socket is
    /// not empty" and the loser popped a `datagram_len` queue the winner had
    /// already drained -- an `unwrap` on `None`, which **panics the kernel**.
    fn two_threads_reading_one_datagram_socket_do_not_panic() {
        with_watchdog("a reader panicked the kernel or wedged", || {
            const ROUNDS: usize = 100_000;
            let (writer, reader) = Socket::create(SocketFlags::DATAGRAM.bits()).unwrap();
            let stop = Arc::new(AtomicBool::new(false));

            let readers: Vec<_> = (0..2)
                .map(|_| {
                    let reader = reader.clone();
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 4];
                        while !stop.load(Ordering::Relaxed) {
                            let _ = reader.read(false, &mut buf);
                        }
                    })
                })
                .collect();

            // No throttle: the readers drain faster than one writer fills,
            // so the queue sits at nought or one datagram and every read is at
            // the boundary.
            for _ in 0..ROUNDS {
                let _ = writer.write(&[1, 2, 3, 4]);
            }
            stop.store(true, Ordering::Relaxed);
            for (i, r) in readers.into_iter().enumerate() {
                assert!(r.join().is_ok(), "reader {} panicked the kernel", i);
            }
        });
    }

    #[test]
    /// And the same two steps on the way in, on the same boundary: the socket
    /// is held with room for about one chunk while two writers write one each.
    /// Both used to measure the free room, let go, and write it, so the buffer
    /// went past `SOCKET_SIZE` -- after which every `SOCKET_SIZE -
    /// data.len()` underflows and the socket reports room for the rest of the
    /// address space.
    fn two_threads_writing_one_socket_do_not_overrun_it() {
        with_watchdog("a writer overran the socket or wedged", || {
            const ROUNDS: usize = 200_000;
            const CHUNK: usize = 1024;
            let (writer, reader) = Socket::create(0).unwrap();
            let stop = Arc::new(AtomicBool::new(false));

            // Filled so that only about one chunk of room is ever free.
            assert_eq!(
                writer.write(&vec![0u8; SOCKET_SIZE - CHUNK]).unwrap(),
                SOCKET_SIZE - CHUNK,
            );

            let writers: Vec<_> = (0..2)
                .map(|_| {
                    let writer = writer.clone();
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        let chunk = vec![0u8; CHUNK];
                        while !stop.load(Ordering::Relaxed) {
                            let _ = writer.write(&chunk);
                        }
                    })
                })
                .collect();

            let mut drain = vec![0u8; CHUNK];
            for _ in 0..ROUNDS {
                let held = reader.get_info().rx_buf_size as usize;
                assert!(
                    held <= SOCKET_SIZE,
                    "{} bytes in a socket that holds {}",
                    held,
                    SOCKET_SIZE,
                );
                if held > SOCKET_SIZE - CHUNK {
                    let _ = reader.read(false, &mut drain);
                }
            }
            stop.store(true, Ordering::Relaxed);
            for (i, w) in writers.into_iter().enumerate() {
                assert!(w.join().is_ok(), "writer {} overran the socket", i);
            }
        });
    }

    #[test]
    fn test_drop() {
        let (end0, end1) = Socket::create(0).unwrap();
        end0.write(&[0; 10]).unwrap();

        drop(end0);
        assert!(!end1.signal().contains(Signal::WRITABLE));
        assert!(end1.signal().contains(Signal::PEER_CLOSED));
        assert_eq!(end1.write(&[0; 1]).unwrap_err(), ZxError::PEER_CLOSED);
        // buffered data can be read
        assert!(end1.signal().contains(Signal::READABLE));
        assert_eq!(end1.read(false, &mut [0; 20]).unwrap(), 10);
        // no more data
        assert!(!end1.signal().contains(Signal::READABLE));
        assert_eq!(
            end1.read(false, &mut [0; 20]).unwrap_err(),
            ZxError::PEER_CLOSED
        );
    }
}
