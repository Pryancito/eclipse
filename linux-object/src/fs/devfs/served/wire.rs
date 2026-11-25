//! What passes between the kernel and a process that serves a device node:
//! the 32-byte message of the FIFO, and how the window is cut.
//! `tools/eclipse-udrivers/src/served.rs` has the same layout on the other
//! side.
//!
//! A request is one message, and what goes with it -- the bytes to write,
//! the argument of an `ioctl` -- is in the request's slot of the window,
//! where the server also puts what it answers with. The reply is a message
//! with the request's `reqid` and `status` filled in: a count, or a Linux
//! `errno`, negated.
//!
//! The server also says things nobody asked: that an open of its node can
//! now be read, or written, or has hung up ([`OP_READY`]). It says them
//! before it answers the request that changed them, and the kernel reads
//! the FIFO in order, so a reply never comes before the readiness it
//! leaves behind.

/// One FIFO element: `op, reqid, fh, len` (u32 each), `a` (u64), and `b`
/// (u64) in a request or `status` (i64) in a reply.
pub const MSG: usize = 32;

/// A slot of the window, one request in flight on each: what is written,
/// read or passed to an `ioctl`.
pub const SLOT_LEN: usize = 64 * 1024;
/// The most slots a window is cut into, whatever its length.
pub const SLOTS: usize = 16;

/// A new open of the node, with `a` the open's flags (`O_ACCMODE`,
/// `O_NONBLOCK`). The reply: in `fh` the server's name for the open, which
/// every request on it carries; in `a` its readiness ([`POLLIN`] and the
/// rest). A negative `status` refuses the open.
pub const OP_OPEN: u32 = 1;
/// The open `fh` is closed: nothing will be asked on it again.
pub const OP_RELEASE: u32 = 2;
/// Up to `len` bytes of the open `fh`, from byte `a`, into the slot. The
/// reply: `status` bytes, or `-EAGAIN` when there is nothing yet: a read
/// never waits in the server. A reader that may wait does so in the kernel,
/// until the server says the open is readable.
pub const OP_READ: u32 = 3;
/// The `len` bytes of the slot, written to the open `fh` at byte `a`. The
/// reply: `status` bytes taken, or `-EAGAIN` when it takes none now.
pub const OP_WRITE: u32 = 4;
/// `ioctl(fh, a, b)`: `a` is the command and `b` the argument as the
/// program passed it. When the command's number says it carries data in
/// (`_IOW`, `_IOWR`), its `len` bytes are in the slot; when it says it
/// carries data out (`_IOR`, `_IOWR`), the server puts it in the slot and
/// says how much in the reply's `len`, which is copied to the argument.
/// The reply's `status` is what the `ioctl` returns. [`IOCTL_WAITS`] in
/// `a` says the open waits.
pub const OP_IOCTL: u32 = 5;
/// Above the command in `a`: the open waits (it is not `O_NONBLOCK`), so an
/// answer of `EAGAIN` is asked again once the server says something of it,
/// and the call goes on until then. A server that would rather take all of
/// what it is given than some can tell the two apart by this.
pub const IOCTL_WAITS: u64 = 1 << 32;

/// An `ioctl` again, with what the server asked for of the program's
/// memory ([`IOCTL_FETCH`]): `a` and `b` as in the [`OP_IOCTL`] it
/// follows, and in the slot the [`Iov`] the server asked with, then the
/// bytes of each of its ranges to read, one after the other. The reply is
/// an [`OP_IOCTL`]'s, but what it puts in the slot goes to the ranges to
/// write, in order; or it asks again.
pub const OP_IOCTL_IOV: u32 = 6;

/// `mmap` of the open `fh`: `a` is the offset into the device and `b` the
/// length, both in bytes and whole pages; `len` says how it is mapped
/// ([`MMAP_SHARED`], [`MMAP_WRITE`]). The reply: in `a`, a handle in the
/// server's own table to a VMO that holds the window, and in the slot, when
/// the reply's `len` is 8, where the window starts in that VMO (at 0
/// otherwise). The handle stays the server's; the mapping holds the VMO
/// for as long as it lasts. The handle's rights are the mapping's ceiling:
/// it needs `MAP` and `READ`, and without `WRITE` the mapping is
/// read-only. A negative `status` refuses the `mmap`.
pub const OP_MMAP: u32 = 7;
/// In an [`OP_MMAP`]'s `len`: the mapping is `MAP_SHARED`.
pub const MMAP_SHARED: u32 = 1;
/// In an [`OP_MMAP`]'s `len`: the program asked for `PROT_WRITE`.
pub const MMAP_WRITE: u32 = 2;

/// From the server, unasked: the open `fh` is now as `a` says ([`POLLIN`]
/// and the rest). `reqid` is not looked at.
pub const OP_READY: u32 = 16;

/// In the reply to an [`OP_IOCTL`] or [`OP_IOCTL_IOV`], in `a`: the server
/// needs more of the program's memory than the command's number says,
/// because the argument points further (ALSA's `WRITEI_FRAMES`, whose
/// structure holds the address of the samples). The slot holds an [`Iov`]:
/// the ranges to read for it and those to write what it answers to. The
/// kernel asks again with [`OP_IOCTL_IOV`]; `status` is not looked at. It
/// is CUSE's retry of an unrestricted `ioctl`.
pub const IOCTL_FETCH: u64 = 1;
/// How many ranges an [`Iov`] may name, each way.
pub const IOV_MAX: usize = 8;
/// How many times one `ioctl` may be asked again: an argument that holds
/// the address of what holds an address takes two.
pub const FETCH_ROUNDS: usize = 4;

/// The ranges of the program's memory an `ioctl` reads and writes, as the
/// server asks for them: `ins` (`u32`), `outs` (`u32`), then an address
/// and a length (`u64` each) for every range, those to read first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Iov {
    pub ins: alloc::vec::Vec<(u64, u64)>,
    pub outs: alloc::vec::Vec<(u64, u64)>,
}

impl Iov {
    /// How long an [`Iov`] of `ins` and `outs` ranges is in the slot.
    pub fn len_of(ins: usize, outs: usize) -> usize {
        8 + 16 * (ins + outs)
    }

    pub fn encode(&self) -> alloc::vec::Vec<u8> {
        let mut b = alloc::vec::Vec::with_capacity(Self::len_of(self.ins.len(), self.outs.len()));
        b.extend_from_slice(&(self.ins.len() as u32).to_ne_bytes());
        b.extend_from_slice(&(self.outs.len() as u32).to_ne_bytes());
        for (addr, len) in self.ins.iter().chain(self.outs.iter()) {
            b.extend_from_slice(&addr.to_ne_bytes());
            b.extend_from_slice(&len.to_ne_bytes());
        }
        b
    }

    /// The [`Iov`] at the start of `b`, when it is one the slot can carry:
    /// at most [`IOV_MAX`] ranges each way, and what they read, after the
    /// ranges themselves, and what they write each fit a slot.
    pub fn decode(b: &[u8]) -> Option<Iov> {
        use core::convert::TryInto;
        let u32_at = |i: usize| -> Option<usize> {
            Some(u32::from_ne_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize)
        };
        let u64_at = |i: usize| -> Option<u64> {
            Some(u64::from_ne_bytes(b.get(i..i + 8)?.try_into().ok()?))
        };
        let (ins, outs) = (u32_at(0)?, u32_at(4)?);
        if ins > IOV_MAX || outs > IOV_MAX {
            return None;
        }
        let mut ranges = alloc::vec::Vec::with_capacity(ins + outs);
        for i in 0..ins + outs {
            ranges.push((u64_at(8 + 16 * i)?, u64_at(16 + 16 * i)?));
        }
        let outs_v = ranges.split_off(ins);
        let iov = Iov {
            ins: ranges,
            outs: outs_v,
        };
        let total = |r: &[(u64, u64)]| r.iter().try_fold(0u64, |t, (_, l)| t.checked_add(*l));
        let read = total(&iov.ins)?.checked_add(Self::len_of(ins, outs) as u64)?;
        if read > SLOT_LEN as u64 || total(&iov.outs)? > SLOT_LEN as u64 {
            return None;
        }
        Some(iov)
    }
}

/// Readiness, as `poll(2)` spells it.
pub const POLLIN: u64 = 0x1;
pub const POLLOUT: u64 = 0x4;
pub const POLLERR: u64 = 0x8;
pub const POLLHUP: u64 = 0x10;

/// A message, either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Msg {
    pub op: u32,
    pub reqid: u32,
    pub fh: u32,
    pub len: u32,
    pub a: u64,
    /// `b` in a request, `status` in a reply.
    pub b: u64,
}

impl Msg {
    pub fn encode(&self) -> [u8; MSG] {
        let mut m = [0u8; MSG];
        m[0..4].copy_from_slice(&self.op.to_ne_bytes());
        m[4..8].copy_from_slice(&self.reqid.to_ne_bytes());
        m[8..12].copy_from_slice(&self.fh.to_ne_bytes());
        m[12..16].copy_from_slice(&self.len.to_ne_bytes());
        m[16..24].copy_from_slice(&self.a.to_ne_bytes());
        m[24..32].copy_from_slice(&self.b.to_ne_bytes());
        m
    }

    pub fn decode(m: &[u8; MSG]) -> Self {
        let u32_at = |i: usize| u32::from_ne_bytes([m[i], m[i + 1], m[i + 2], m[i + 3]]);
        let u64_at = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&m[i..i + 8]);
            u64::from_ne_bytes(b)
        };
        Msg {
            op: u32_at(0),
            reqid: u32_at(4),
            fh: u32_at(8),
            len: u32_at(12),
            a: u64_at(16),
            b: u64_at(24),
        }
    }

    /// A reply's `status`.
    pub fn status(&self) -> i64 {
        self.b as i64
    }
}

/// How many slots a window of `len` bytes has: `None` when not even one
/// fits.
pub fn slots_of(len: usize) -> Option<usize> {
    match (len / SLOT_LEN).min(SLOTS) {
        0 => None,
        n => Some(n),
    }
}

/// What an `ioctl` command's number says it carries: the bytes that go in
/// to the server, and the most that come back out. The encoding is the
/// generic one (`asm-generic/ioctl.h`), which x86_64, aarch64 and riscv64
/// all use: direction in bits 30-31 (1 write, 2 read), size in bits 16-29.
/// A command with neither direction carries nothing, and its argument is
/// a number the server is given as it is.
pub fn ioctl_carries(cmd: u32) -> (usize, usize) {
    let size = ((cmd >> 16) & 0x3fff) as usize;
    let dir = cmd >> 30;
    let into = if dir & 1 != 0 { size } else { 0 };
    let out = if dir & 2 != 0 { size } else { 0 };
    (into, out)
}

/// The errno of a `status`, when it is one a device would answer;
/// anything else is the device failing (`EIO`).
pub fn errno_of(status: i64) -> usize {
    match -status {
        // 77 is `EBADFD`, a stream in the wrong state (ALSA's).
        e @ (1 | 2 | 5 | 6 | 11 | 12 | 13 | 14 | 16 | 19 | 22 | 25 | 28 | 32 | 38 | 75 | 77
        | 95 | 110) => e as usize,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_come_back_as_they_went_and_too_many_are_refused() {
        let iov = Iov {
            ins: alloc::vec![(0x1000, 24), (0x2000, 4096)],
            outs: alloc::vec![(0x1000, 24)],
        };
        let b = iov.encode();
        assert_eq!(b.len(), Iov::len_of(2, 1));
        assert_eq!(Iov::decode(&b), Some(iov));
        assert_eq!(Iov::decode(&b[..b.len() - 1]), None);
        let big = Iov {
            ins: alloc::vec![(0, SLOT_LEN as u64)],
            outs: alloc::vec![],
        };
        assert_eq!(Iov::decode(&big.encode()), None, "the ranges take room too");
        let many = Iov {
            ins: alloc::vec![(0, 1); IOV_MAX + 1],
            outs: alloc::vec![],
        };
        assert_eq!(Iov::decode(&many.encode()), None);
        let wrap = Iov {
            ins: alloc::vec![(0, u64::MAX), (0, 2)],
            outs: alloc::vec![],
        };
        assert_eq!(Iov::decode(&wrap.encode()), None);
    }

    #[test]
    fn a_message_comes_back_as_it_went() {
        let m = Msg {
            op: OP_IOCTL,
            reqid: 0x1234_5670,
            fh: 9,
            len: 24,
            a: 0x8018_4520,
            b: u64::MAX - 3,
        };
        assert_eq!(Msg::decode(&m.encode()), m);
        assert_eq!(m.status(), -4);
    }

    #[test]
    fn a_window_is_cut_in_whole_slots_up_to_sixteen() {
        assert_eq!(slots_of(SLOT_LEN - 1), None);
        assert_eq!(slots_of(SLOT_LEN), Some(1));
        assert_eq!(slots_of(3 * SLOT_LEN + 5), Some(3));
        assert_eq!(slots_of(100 * SLOT_LEN), Some(SLOTS));
    }

    #[test]
    fn an_ioctl_number_says_what_it_carries() {
        // EVIOCGBIT(EV_KEY, 96): _IOC(_IOC_READ, 'E', 0x21, 96).
        assert_eq!(ioctl_carries(0x8060_4521), (0, 96));
        // EVIOCGRAB: _IOW('E', 0x90, int).
        assert_eq!(ioctl_carries(0x4004_4590), (4, 0));
        // EVIOCSABS(0): _IOW('E', 0xc0, struct input_absinfo).
        assert_eq!(ioctl_carries(0x4018_45c0), (24, 0));
        // An _IOWR both ways.
        assert_eq!(ioctl_carries(0xc010_4100), (16, 16));
        // TCGETS, which says nothing: a number, carried as it is.
        assert_eq!(ioctl_carries(0x5401), (0, 0));
    }

    #[test]
    fn a_server_errno_is_one_a_device_answers_or_eio() {
        assert_eq!(errno_of(-11), 11);
        assert_eq!(errno_of(-25), 25);
        assert_eq!(errno_of(-77), 77);
        assert_eq!(errno_of(-4), 5);
        assert_eq!(errno_of(-100_000), 5);
    }
}
