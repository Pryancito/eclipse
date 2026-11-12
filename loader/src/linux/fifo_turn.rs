//! [`ECLIPSE_FIFO_REPLY_WAIT`](super::ECLIPSE_FIFO_REPLY_WAIT): a driver's
//! whole turn on its FIFO in one syscall.
//!
//! A process that serves a device to the kernel (`block_serve`,
//! `net_serve`) lives in a loop on one FIFO: answer what it was asked, find
//! out what is asked next. With the Zircon calls alone that is a write, a
//! read that finds nothing, a wait and a read -- four trips into the kernel
//! for each request, and on the path of every block and every frame. This is
//! the same turn in one trip, the FIFO's twin of
//! [`ECLIPSE_CHANNEL_REPLY_WAIT`](super::ECLIPSE_CHANNEL_REPLY_WAIT).
//!
//! The two halves are separate functions because only the second waits: the
//! first ends the call at once when the handle, the sizes or the buffer are
//! wrong, and the second is what a signal may cut short.

use alloc::sync::Arc;
use alloc::vec::Vec;
use kernel_hal::user::{UserInPtr, UserOutPtr};
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::{ZxError, ZxResult};

/// A turn whose replies are on their way: what is left is the wait.
pub(super) struct Turn {
    fifo: Arc<Fifo>,
    elem_size: usize,
    /// The caller's buffer, and how many elements of it may be filled.
    buf: usize,
    room: usize,
    /// Where the count of what was read goes.
    actual: usize,
}

/// Put `count` elements on `fifo`, all of them: `Err` with how many went
/// when the other side's queue has no room for the rest.
pub(super) fn put_all(
    fifo: &Fifo,
    elem_size: usize,
    bytes: &[u8],
    count: usize,
) -> Result<(), (ZxError, usize)> {
    match fifo.write(elem_size, bytes, count) {
        Ok(put) if put == count => Ok(()),
        Ok(put) => Err((ZxError::SHOULD_WAIT, put)),
        Err(e) => Err((e, 0)),
    }
}

/// The elements waiting on `fifo`, up to `room` of them into `data`, asleep
/// until there is one. `PEER_CLOSED` once the queue is empty and nobody is
/// left to fill it.
pub(super) async fn take_next(
    fifo: &Arc<Fifo>,
    elem_size: usize,
    data: &mut [u8],
    room: usize,
) -> ZxResult<usize> {
    loop {
        match fifo.read(elem_size, data, room) {
            Err(ZxError::SHOULD_WAIT) => {
                let object: Arc<dyn KernelObject> = fifo.clone();
                object
                    .wait_signal(Signal::READABLE | Signal::PEER_CLOSED)
                    .await;
            }
            answer => return answer,
        }
    }
}

/// The call up to the wait: `args` checked and the replies sent. What it
/// answers is the error that ends the call before the wait.
pub(super) fn reply_then(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult<Turn> {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE)?;
    let (elem_size, buf, replies, room, actual) = (args[1], args[2], args[3], args[4], args[5]);
    // The element size is the FIFO's and there is room to read into, or
    // `OUT_OF_RANGE`; and no more is ever read than the FIFO holds.
    let room = fifo.read_buffer_elems(elem_size, room)?;
    // Look at `actual` before sending anything: a bad pointer must not cost
    // replies that then cannot be followed by a read.
    let mut count = UserOutPtr::<usize>::from(actual);
    count.write(0).map_err(|_| ZxError::INVALID_ARGS)?;
    if replies != 0 {
        // More than the FIFO holds can never all go.
        if fifo.read_buffer_elems(elem_size, replies)? != replies {
            return Err(ZxError::OUT_OF_RANGE);
        }
        let len = replies
            .checked_mul(elem_size)
            .ok_or(ZxError::INVALID_ARGS)?;
        let bytes: Vec<u8> = UserInPtr::<u8>::from(buf)
            .read_array(len)
            .map_err(|_| ZxError::INVALID_ARGS)?;
        if let Err((e, put)) = put_all(&fifo, elem_size, &bytes, replies) {
            let _ = count.write(put);
            return Err(e);
        }
    }
    Ok(Turn {
        fifo,
        elem_size,
        buf,
        room,
        actual,
    })
}

impl Turn {
    /// The call from the wait on: the next elements into the caller's
    /// buffer, and how many into `*actual`.
    pub(super) async fn wait_then_read(self) -> ZxResult {
        let mut data = alloc::vec![0u8; self.room * self.elem_size];
        let got = take_next(&self.fifo, self.elem_size, &mut data, self.room).await?;
        // As `zx_fifo_read`: the elements are taken, and a buffer that
        // faults loses them.
        UserOutPtr::<u8>::from(self.buf)
            .write_array(&data[..got * self.elem_size])
            .map_err(|_| ZxError::INVALID_ARGS)?;
        UserOutPtr::<usize>::from(self.actual)
            .write(got)
            .map_err(|_| ZxError::INVALID_ARGS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    const ELEM: usize = 4;
    const DEPTH: usize = 4;

    fn elems(first: u8, count: usize) -> Vec<u8> {
        (0..count * ELEM).map(|i| first + i as u8).collect()
    }

    #[test]
    fn replies_go_out_whole_and_the_peer_reads_them() {
        let (ours, theirs) = Fifo::create(DEPTH, ELEM);
        let out = elems(10, 3);
        assert_eq!(put_all(&ours, ELEM, &out, 3), Ok(()));
        let mut back = [0u8; DEPTH * ELEM];
        assert_eq!(theirs.read(ELEM, &mut back, DEPTH), Ok(3));
        assert_eq!(&back[..3 * ELEM], &out[..]);
    }

    #[test]
    fn replies_that_do_not_all_fit_say_how_many_went() {
        let (ours, theirs) = Fifo::create(DEPTH, ELEM);
        assert_eq!(put_all(&ours, ELEM, &elems(0, 3), 3), Ok(()));
        // One place left for two elements: one goes, and the caller is told.
        assert_eq!(
            put_all(&ours, ELEM, &elems(50, 2), 2),
            Err((ZxError::SHOULD_WAIT, 1))
        );
        // None left at all.
        assert_eq!(
            put_all(&ours, ELEM, &elems(60, 1), 1),
            Err((ZxError::SHOULD_WAIT, 0))
        );
        // What went is what the peer finds, in order.
        let mut back = [0u8; DEPTH * ELEM];
        assert_eq!(theirs.read(ELEM, &mut back, DEPTH), Ok(DEPTH));
        assert_eq!(&back[3 * ELEM..], &elems(50, 1)[..]);
    }

    #[test]
    fn replies_to_a_peer_that_is_gone_or_of_another_size_are_refused() {
        let (ours, theirs) = Fifo::create(DEPTH, ELEM);
        assert_eq!(
            put_all(&ours, ELEM + 1, &[0u8; ELEM + 1], 1),
            Err((ZxError::OUT_OF_RANGE, 0))
        );
        drop(theirs);
        assert_eq!(
            put_all(&ours, ELEM, &elems(0, 1), 1),
            Err((ZxError::PEER_CLOSED, 0))
        );
    }

    #[async_std::test]
    async fn what_is_already_waiting_is_read_without_a_wait() {
        let (ours, theirs) = Fifo::create(DEPTH, ELEM);
        theirs.write(ELEM, &elems(7, 2), 2).unwrap();
        let mut data = [0u8; DEPTH * ELEM];
        assert_eq!(take_next(&ours, ELEM, &mut data, DEPTH).await, Ok(2));
        assert_eq!(&data[..2 * ELEM], &elems(7, 2)[..]);
    }

    #[async_std::test]
    async fn no_more_is_read_than_there_is_room_for() {
        let (ours, theirs) = Fifo::create(DEPTH, ELEM);
        theirs.write(ELEM, &elems(1, 3), 3).unwrap();
        let mut data = [0u8; ELEM];
        assert_eq!(take_next(&ours, ELEM, &mut data, 1).await, Ok(1));
        assert_eq!(&data[..], &elems(1, 1)[..]);
        // The rest is still there for the next turn.
        let mut rest = [0u8; DEPTH * ELEM];
        assert_eq!(take_next(&ours, ELEM, &mut rest, DEPTH).await, Ok(2));
    }

    #[async_std::test]
    async fn an_empty_fifo_is_waited_on_until_the_peer_writes() {
        let (ours, theirs) = Fifo::create(DEPTH, ELEM);
        let writer = async_std::task::spawn(async move {
            async_std::task::sleep(Duration::from_millis(20)).await;
            theirs.write(ELEM, &elems(90, 1), 1).unwrap();
            theirs
        });
        let mut data = [0u8; DEPTH * ELEM];
        assert_eq!(take_next(&ours, ELEM, &mut data, DEPTH).await, Ok(1));
        assert_eq!(&data[..ELEM], &elems(90, 1)[..]);
        drop(writer.await);
    }

    #[async_std::test]
    async fn the_wait_ends_when_the_peer_goes_but_what_it_left_is_read_first() {
        let (ours, theirs) = Fifo::create(DEPTH, ELEM);
        theirs.write(ELEM, &elems(3, 1), 1).unwrap();
        let closer = async_std::task::spawn(async move {
            async_std::task::sleep(Duration::from_millis(20)).await;
            drop(theirs);
        });
        let mut data = [0u8; DEPTH * ELEM];
        assert_eq!(take_next(&ours, ELEM, &mut data, DEPTH).await, Ok(1));
        // Nothing left and, in a moment, nobody to write: the wait is woken
        // by the close and answers it.
        assert_eq!(
            take_next(&ours, ELEM, &mut data, DEPTH).await,
            Err(ZxError::PEER_CLOSED)
        );
        closer.await;
    }
}
