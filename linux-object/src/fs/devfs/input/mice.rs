use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::task::{Context, Poll};
use core::{any::Any, future::Future, pin::Pin};

use kernel_hal::sync::Mutex;

use kernel_hal::drivers::prelude::input::{Mouse, MouseFlags, MouseState, Ps2Mode};
use kernel_hal::drivers::scheme::{EventScheme, InputScheme};
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

const MAX_MOUSE_DEVICES: usize = 30;
const BUF_CAPACITY: usize = 32;

const MOUSE_DEV_MINOR_BASE: usize = 0x20;

/// The sample-rate sequence a client writes to ask for the IntelliMouse
/// protocol, and the one for IntelliMouse Explorer (`mousedev.c`).
const IMPS_SEQ: [u8; 6] = [0xf3, 200, 0xf3, 100, 0xf3, 80];
const IMEX_SEQ: [u8; 6] = [0xf3, 200, 0xf3, 200, 0xf3, 80];

/// Room for the longest thing a reader can be handed in one go: the ACK of a
/// `0xeb` poll followed by a four-byte packet.
const STAGE_LEN: usize = 5;

struct MiceDevInner {
    /// Which protocol this node is speaking. Every client shares it, which is
    /// the one place this is simpler than `mousedev.c`: there a mode belongs
    /// to an open file, here to the node.
    mode: Ps2Mode,
    /// How much of each sequence the bytes written so far match.
    imps_seq: usize,
    imex_seq: usize,
    /// The bytes a reader is being handed: either a reply to a command or one
    /// packet, never both. `mousedev.c` keeps exactly one such buffer, and
    /// keeping the bytes here rather than in the packet queue is what makes
    /// the queue safe to evict from: a packet whose first byte has already
    /// been read is no longer in it.
    stage: [u8; STAGE_LEN],
    stage_len: usize,
    stage_pos: usize,
    last_buttons: MouseFlags,
    buf: VecDeque<MouseState>,
}

/// mice device
pub struct MiceDev {
    id: usize,
    inode_id: usize,
    mice: Vec<Arc<Mouse>>,
    inner: Arc<Mutex<MiceDevInner>>,
}

impl MiceDevInner {
    /// Put `bytes` in front of the reader, replacing whatever was there.
    ///
    /// Replacing is what `mousedev_generate_response` does: a command answered
    /// mid-packet takes the buffer over, because a client that is talking to
    /// the mouse is not reading movement from it.
    fn stage(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(STAGE_LEN);
        self.stage[..n].copy_from_slice(&bytes[..n]);
        self.stage_len = n;
        self.stage_pos = 0;
    }

    /// Take one packet off the queue and put it in front of the reader.
    /// Returns whether there was one.
    fn stage_packet(&mut self) -> bool {
        let mode = self.mode;
        let (bytes, len, drained) = match self.buf.front_mut() {
            Some(front) => {
                let (bytes, len) = front.take_ps2_packet(mode);
                (bytes, len, front.is_drained())
            }
            None => return false,
        };
        // Only once there is nothing left of the movement: a step larger than
        // one packet leaves its remainder here for the next read.
        if drained {
            self.buf.pop_front();
        }
        self.stage(&bytes[..len]);
        true
    }

    fn read_at(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.stage_pos == self.stage_len && !self.stage_packet() {
            return Err(FsError::Again);
        }
        // One staged buffer per read, as `mousedev_read` does: a packet is a
        // frame, and handing back one and a half of them is how a protocol
        // with no framing loses its place.
        let n = buf.len().min(self.stage_len - self.stage_pos);
        buf[..n].copy_from_slice(&self.stage[self.stage_pos..self.stage_pos + n]);
        self.stage_pos += n;
        Ok(n)
    }

    /// One byte written to the node: advance both sequences, then answer it.
    fn write_byte(&mut self, c: u8) {
        // Both are tracked at once and independently, because they share a
        // prefix: `f3 200` is the start of either.
        self.imex_seq = if c == IMEX_SEQ[self.imex_seq] {
            self.imex_seq + 1
        } else {
            0
        };
        if self.imex_seq == IMEX_SEQ.len() {
            self.imex_seq = 0;
            self.mode = Ps2Mode::ImEx;
        }
        self.imps_seq = if c == IMPS_SEQ[self.imps_seq] {
            self.imps_seq + 1
        } else {
            0
        };
        if self.imps_seq == IMPS_SEQ.len() {
            self.imps_seq = 0;
            self.mode = Ps2Mode::ImPs2;
        }
        self.generate_response(c);
    }

    /// The reply a real mouse gives to a command byte (`mousedev.c`'s
    /// `mousedev_generate_response`). Every byte is acknowledged, which is
    /// what tells a client there is a mouse at the other end at all: without
    /// it no client ever gets past plain PS/2, so the wheel had nowhere to go.
    fn generate_response(&mut self, command: u8) {
        const ACK: u8 = 0xfa;
        match command {
            // Poll: the ACK and then a packet.
            0xeb => {
                let mut state = self.buf.front().copied().unwrap_or_default();
                let (bytes, len) = state.take_ps2_packet(self.mode);
                let mut out = [ACK, 0, 0, 0, 0];
                out[1..1 + len].copy_from_slice(&bytes[..len]);
                self.stage(&out[..1 + len]);
            }
            // Get ID: which protocol this mouse is speaking.
            0xf2 => {
                let id = match self.mode {
                    Ps2Mode::Ps2 => 0,
                    Ps2Mode::ImPs2 => 3,
                    Ps2Mode::ImEx => 4,
                };
                self.stage(&[ACK, id]);
            }
            // Get info: status byte, resolution, sample rate.
            0xe9 => self.stage(&[ACK, 0x60, 3, 200]),
            // Reset: back to plain PS/2, then the self-test pass and the id.
            0xff => {
                self.imps_seq = 0;
                self.imex_seq = 0;
                self.mode = Ps2Mode::Ps2;
                self.stage(&[ACK, 0xaa, 0x00]);
            }
            // Everything else is acknowledged and otherwise ignored, exactly
            // as a mouse with nothing to configure would.
            _ => self.stage(&[ACK]),
        }
    }

    fn handle_mouse_packet(&mut self, p: &MouseState) {
        // A report that moves nothing this protocol can carry, and changes no
        // button, is not a packet: in plain PS/2 that includes every scroll,
        // which used to queue three bytes saying nothing at all.
        let moves = p.dx != 0 || p.dy != 0 || (p.dz != 0 && self.mode != Ps2Mode::Ps2);
        if !moves && p.buttons == self.last_buttons {
            return;
        }

        self.last_buttons = p.buttons;
        while self.buf.len() >= BUF_CAPACITY {
            // The bytes a reader has already been handed live in `stage`, not
            // here, so dropping the oldest state cannot cut a packet in half.
            self.buf.pop_front();
        }
        self.buf.push_back(*p);
    }
}

impl MiceDev {
    /// Create a list of "mouseX" and "mice" INode from input devices.
    pub fn from_input_devices(inputs: &[Arc<dyn InputScheme>]) -> Vec<(Option<usize>, MiceDev)> {
        let mut mice = Vec::with_capacity(inputs.len());
        for i in inputs {
            if mice.len() < MAX_MOUSE_DEVICES && Mouse::compatible_with(i) {
                mice.push(Mouse::new(i.clone()));
            }
        }
        let mut ret = mice
            .iter()
            .enumerate()
            .map(|(i, m)| (Some(i), Self::new(m.clone(), i)))
            .collect::<Vec<_>>();
        if !mice.is_empty() {
            ret.push((None, Self::new_many(mice, MAX_MOUSE_DEVICES + 1)));
        }
        ret
    }

    /// Create a "mouseX" INode from one mouse device.
    pub fn new(mouse: Arc<Mouse>, id: usize) -> Self {
        Self::new_many(vec![mouse], id)
    }

    /// Create a "mice" INode from multiple mice.
    pub fn new_many(mice: Vec<Arc<Mouse>>, id: usize) -> Self {
        let inner = Arc::new(Mutex::new(MiceDevInner {
            mode: Ps2Mode::Ps2,
            imps_seq: 0,
            imex_seq: 0,
            stage: [0; STAGE_LEN],
            stage_len: 0,
            stage_pos: 0,
            last_buttons: MouseFlags::empty(),
            buf: VecDeque::with_capacity(BUF_CAPACITY),
        }));
        for m in &mice {
            let cloned = inner.clone();
            m.subscribe(
                Box::new(move |p| cloned.lock().handle_mouse_packet(p)),
                false,
            );
        }
        Self {
            id,
            mice,
            inner,
            inode_id: DevFS::new_inode_id(),
        }
    }

    fn can_read(&self) -> bool {
        let inner = self.inner.lock();
        inner.stage_pos < inner.stage_len || !inner.buf.is_empty()
    }
}

impl INode for MiceDev {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.inner.lock().read_at(buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        // A client talks to this node the way it would talk to a mouse:
        // set-sample-rate, get-device-id, reset, enable. Discarding the bytes
        // -- which is what this did -- makes the write succeed and leaves the
        // client with no answer, so it concludes there is nothing but a plain
        // three-byte mouse here and settles for one. That conclusion was
        // right, and it is what kept the wheel off this node entirely.
        let mut inner = self.inner.lock();
        for &c in buf {
            inner.write_byte(c);
        }
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.can_read(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        /// Parks on each mouse EventListener until readable. Must unsubscribe
        /// on Ready/`Drop` (same UAF class as EventDev / EventBus orphans).
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct MiceFuture<'a> {
            dev: &'a MiceDev,
            /// `(mouse index, subscription id)` pairs registered while Pending.
            subs: Vec<(usize, u64)>,
        }

        impl Drop for MiceFuture<'_> {
            fn drop(&mut self) {
                for (idx, id) in self.subs.drain(..) {
                    if let Some(m) = self.dev.mice.get(idx) {
                        m.unsubscribe(id);
                    }
                }
            }
        }

        impl<'a> Future for MiceFuture<'a> {
            type Output = Result<PollStatus>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                let this = self.as_mut().get_mut();
                let clear_subs = |fut: &mut MiceFuture<'_>| {
                    for (idx, id) in fut.subs.drain(..) {
                        if let Some(m) = fut.dev.mice.get(idx) {
                            m.unsubscribe(id);
                        }
                    }
                };
                if this.dev.can_read() {
                    clear_subs(this);
                    return Poll::Ready(this.dev.poll());
                }
                // Register the waker BEFORE the second can_read() check to close
                // the TOCTOU race: a packet that lands between the first check
                // and subscribe() would otherwise fire no waker, and the task
                // would sleep until the next packet. (Matches EventDev.)
                if this.subs.is_empty() {
                    for (idx, m) in this.dev.mice.iter().enumerate() {
                        let waker = cx.waker().clone();
                        if let Some(id) = m.subscribe(Box::new(move |_| waker.wake_by_ref()), true)
                        {
                            this.subs.push((idx, id));
                        }
                    }
                }
                if this.dev.can_read() {
                    clear_subs(this);
                    return Poll::Ready(this.dev.poll());
                }
                Poll::Pending
            }
        }

        Box::pin(MiceFuture {
            dev: self,
            subs: Vec::new(),
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o660,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(0xd, MOUSE_DEV_MINOR_BASE + self.id),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod mice_tests {
    //! `/dev/input/mice` is the node an X server opens when it wants a mouse
    //! and does not want evdev. Everything a client ever sees of it comes out
    //! of these four functions, and none of them needs a device: a
    //! `MouseState` is three numbers and a button mask.
    use super::*;

    fn inner() -> MiceDevInner {
        MiceDevInner {
            mode: Ps2Mode::Ps2,
            imps_seq: 0,
            imex_seq: 0,
            stage: [0; STAGE_LEN],
            stage_len: 0,
            stage_pos: 0,
            last_buttons: MouseFlags::empty(),
            buf: VecDeque::new(),
        }
    }

    fn moved(dx: i32, dy: i32, dz: i32) -> MouseState {
        MouseState {
            dx,
            dy,
            dz,
            buttons: MouseFlags::empty(),
        }
    }

    /// Read one whole staged buffer, the way a client's `read(fd, buf, 64)`
    /// does.
    fn read(dev: &mut MiceDevInner) -> Result<Vec<u8>> {
        let mut buf = [0u8; 64];
        let n = dev.read_at(&mut buf)?;
        Ok(buf[..n].to_vec())
    }

    fn write(dev: &mut MiceDevInner, bytes: &[u8]) {
        for &c in bytes {
            dev.write_byte(c);
        }
    }

    /// Ask for a protocol and read the answer, which is what a client does
    /// before it looks for movement again.
    fn negotiate(dev: &mut MiceDevInner, seq: &[u8]) {
        write(dev, seq);
        let _ = read(dev);
    }

    const ACK: u8 = 0xfa;
    const IMPS: [u8; 6] = [0xf3, 200, 0xf3, 100, 0xf3, 80];
    const IMEX: [u8; 6] = [0xf3, 200, 0xf3, 200, 0xf3, 80];

    // ---- the protocol a client ends up speaking -------------------------

    #[test]
    fn a_client_that_says_nothing_gets_three_byte_packets() {
        let mut d = inner();
        d.handle_mouse_packet(&moved(1, 1, 0));
        assert_eq!(read(&mut d).unwrap().len(), 3);
    }

    /// The point of the whole exercise: a client asks for the wheel by
    /// writing what it would write to a real mouse, and until it got an
    /// answer it never asked.
    #[test]
    fn the_intellimouse_sequence_switches_the_node_to_four_byte_packets() {
        let mut d = inner();
        negotiate(&mut d, &IMPS);
        assert_eq!(d.mode, Ps2Mode::ImPs2);
        d.handle_mouse_packet(&moved(1, 1, -2));
        let p = read(&mut d).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(p[3] as i8, -2, "the wheel is in the fourth byte");
    }

    /// The two sequences share their first two bytes, so a reader that
    /// tracked only one of them would answer the wrong protocol.
    #[test]
    fn the_explorer_sequence_is_told_apart_from_the_intellimouse_one() {
        let mut d = inner();
        write(&mut d, &IMEX);
        assert_eq!(d.mode, Ps2Mode::ImEx);
        let mut e = inner();
        write(&mut e, &IMPS);
        assert_eq!(e.mode, Ps2Mode::ImPs2);
    }

    #[test]
    fn a_sequence_interrupted_by_another_byte_starts_over() {
        let mut d = inner();
        write(&mut d, &IMPS[..4]);
        write(&mut d, &[0xf4]); // enable: not part of either sequence
        write(&mut d, &IMPS[4..]);
        assert_eq!(d.mode, Ps2Mode::Ps2, "half a sequence is not a sequence");
        // And the whole sequence afterwards still works.
        write(&mut d, &IMPS);
        assert_eq!(d.mode, Ps2Mode::ImPs2);
    }

    #[test]
    fn asking_for_the_wheel_twice_is_not_an_error() {
        let mut d = inner();
        write(&mut d, &IMPS);
        write(&mut d, &IMPS);
        assert_eq!(d.mode, Ps2Mode::ImPs2);
    }

    // ---- what a command is answered with ---------------------------------

    #[test]
    fn every_byte_written_is_acknowledged() {
        let mut d = inner();
        for c in [0xf3u8, 200, 0xf4, 0xf5, 0xe8, 0x03] {
            write(&mut d, &[c]);
            let r = read(&mut d).unwrap();
            assert_eq!(r[0], ACK, "command {:#x} went unanswered", c);
        }
    }

    #[test]
    fn get_id_names_the_protocol_the_node_is_speaking() {
        let mut d = inner();
        write(&mut d, &[0xf2]);
        assert_eq!(read(&mut d).unwrap(), vec![ACK, 0]);
        write(&mut d, &IMPS);
        write(&mut d, &[0xf2]);
        assert_eq!(read(&mut d).unwrap(), vec![ACK, 3]);
        write(&mut d, &IMEX);
        write(&mut d, &[0xf2]);
        assert_eq!(read(&mut d).unwrap(), vec![ACK, 4]);
    }

    #[test]
    fn get_info_answers_a_status_byte_a_resolution_and_a_sample_rate() {
        let mut d = inner();
        write(&mut d, &[0xe9]);
        assert_eq!(read(&mut d).unwrap(), vec![ACK, 0x60, 3, 200]);
    }

    /// A reset is how a client that has lost track starts again, so it has to
    /// put the protocol back as well as answer.
    #[test]
    fn a_reset_answers_the_self_test_and_goes_back_to_three_bytes() {
        let mut d = inner();
        negotiate(&mut d, &IMPS);
        assert_eq!(d.mode, Ps2Mode::ImPs2);
        write(&mut d, &[0xff]);
        assert_eq!(read(&mut d).unwrap(), vec![ACK, 0xaa, 0x00]);
        assert_eq!(d.mode, Ps2Mode::Ps2);
        d.handle_mouse_packet(&moved(1, 1, 0));
        assert_eq!(read(&mut d).unwrap().len(), 3);
    }

    #[test]
    fn a_poll_answers_with_an_ack_and_a_whole_packet() {
        let mut d = inner();
        negotiate(&mut d, &IMPS);
        d.handle_mouse_packet(&moved(2, 0, 1));
        write(&mut d, &[0xeb]);
        let r = read(&mut d).unwrap();
        assert_eq!(r.len(), 5, "an ACK and four bytes");
        assert_eq!(r[0], ACK);
        assert_eq!(r[2], 2, "the movement, not a fresh zero");
    }

    #[test]
    fn a_poll_with_nothing_to_report_still_answers_a_packet() {
        let mut d = inner();
        write(&mut d, &[0xeb]);
        let r = read(&mut d).unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0], ACK);
    }

    // ---- the byte stream -------------------------------------------------

    /// PS/2 has no framing: a client finds packet boundaries by counting, so
    /// one byte too many or too few and every packet after it is read wrong.
    /// The queue used to drop the very state a reader was in the middle of.
    #[test]
    fn the_packet_a_reader_is_in_the_middle_of_is_never_cut_in_half() {
        let mut d = inner();
        d.handle_mouse_packet(&moved(11, 22, 0));
        // Read the first byte only, as a client reading one byte at a time
        // does.
        let mut one = [0u8; 1];
        assert_eq!(d.read_at(&mut one).unwrap(), 1);
        let first = one[0];
        // Now flood the queue past its capacity.
        for i in 0..BUF_CAPACITY * 2 {
            d.handle_mouse_packet(&moved(i as i32 % 7 + 1, 1, 0));
        }
        // The rest of the packet that was being read is still the rest of
        // THAT packet.
        let rest = read(&mut d).unwrap();
        assert_eq!(rest.len(), 2);
        let flags = MouseFlags::from_bits_truncate(first);
        assert!(flags.contains(MouseFlags::ALWAYS_ONE));
        assert_eq!(rest[0], 11);
        assert_eq!(rest[1], 22);
    }

    #[test]
    fn a_read_smaller_than_a_packet_resumes_where_it_stopped() {
        let mut d = inner();
        d.handle_mouse_packet(&moved(9, 8, 0));
        let mut two = [0u8; 2];
        assert_eq!(d.read_at(&mut two).unwrap(), 2);
        assert_eq!(two[1], 9);
        let rest = read(&mut d).unwrap();
        assert_eq!(rest, vec![8]);
    }

    #[test]
    fn an_empty_queue_is_eagain_and_not_a_short_read() {
        let mut d = inner();
        assert_eq!(read(&mut d).err(), Some(FsError::Again));
        // A zero-length read of an empty node is the same answer.
        assert_eq!(d.read_at(&mut []).err(), Some(FsError::Again));
    }

    #[test]
    fn a_movement_too_large_for_one_packet_comes_out_as_two() {
        let mut d = inner();
        d.handle_mouse_packet(&moved(200, 0, 0));
        let first = read(&mut d).unwrap();
        assert_eq!(first[1], 127);
        let second = read(&mut d).unwrap();
        assert_eq!(second[1], 73, "200 - 127");
        assert_eq!(read(&mut d).err(), Some(FsError::Again));
    }

    // ---- what is worth queueing at all -----------------------------------

    /// A three-byte client cannot be told about a scroll, so a scroll on its
    /// own used to queue three bytes that said nothing had happened.
    #[test]
    fn a_scroll_alone_queues_nothing_a_three_byte_client_could_see() {
        let mut d = inner();
        d.handle_mouse_packet(&moved(0, 0, 3));
        assert_eq!(read(&mut d).err(), Some(FsError::Again));
    }

    #[test]
    fn a_scroll_alone_is_a_packet_once_the_client_has_asked_for_a_wheel() {
        let mut d = inner();
        negotiate(&mut d, &IMPS);
        d.handle_mouse_packet(&moved(0, 0, 3));
        let p = read(&mut d).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(p[3], 3);
    }

    /// A command is answered before any movement is: the reply takes the
    /// buffer over, because a client that is talking to the mouse is not
    /// reading from it. A client that read movement first would take an ACK
    /// for the first byte of a packet.
    #[test]
    fn the_answer_to_a_command_comes_before_the_movement() {
        let mut d = inner();
        d.handle_mouse_packet(&moved(5, 5, 0));
        write(&mut d, &[0xf2]);
        assert_eq!(read(&mut d).unwrap(), vec![ACK, 0]);
        let p = read(&mut d).unwrap();
        assert_eq!(p[1], 5, "and the movement is still there afterwards");
    }

    #[test]
    fn a_report_that_moves_nothing_and_changes_no_button_is_not_a_packet() {
        let mut d = inner();
        d.handle_mouse_packet(&moved(0, 0, 0));
        assert_eq!(read(&mut d).err(), Some(FsError::Again));
    }

    /// A button going down and coming back up are two different reports even
    /// though neither moves the pointer.
    #[test]
    fn a_button_that_changed_is_a_packet_even_with_no_movement() {
        let mut d = inner();
        let down = MouseState {
            dx: 0,
            dy: 0,
            dz: 0,
            buttons: MouseFlags::LEFT_BTN,
        };
        d.handle_mouse_packet(&down);
        let p = read(&mut d).unwrap();
        assert_eq!(p[0] & 0x07, MouseFlags::LEFT_BTN.bits());
        // The same state again says nothing new.
        d.handle_mouse_packet(&down);
        assert_eq!(read(&mut d).err(), Some(FsError::Again));
        // Letting go does.
        d.handle_mouse_packet(&moved(0, 0, 0));
        assert_eq!(read(&mut d).unwrap()[0] & 0x07, 0);
    }
}
