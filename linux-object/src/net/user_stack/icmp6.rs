//! The sockets that speak ICMPv6, whose stack is a process: what
//! `ping -6` opens. The same sockets as IPv4's ([`super::icmp`]), with
//! what is different between the two said here.
//!
//! There are two, as in Linux. The raw one (`SOCK_RAW`) writes ICMPv6
//! messages as the program made them and reads every one the machine
//! gets that its filter lets by (`ICMP6_FILTER`). The one that pings
//! (`SOCK_DGRAM`) writes echoes only, which leave with the socket's
//! identifier in place of the program's, and reads the answers to them.
//!
//! Neither reads the IPv6 header, as neither does in Linux. How many
//! hops a message had left is told beside it, to a `recvmsg` that asked
//! (`IPV6_RECVHOPLIMIT`, or `IPV6_2292HOPLIMIT` as busybox's `ping`
//! does).
//!
//! The sum of a message is never the program's: ICMPv6's covers the
//! addresses, and the address a message leaves from is chosen when it
//! leaves. The stack writes it for what goes by the card, and the kernel
//! for what stays in the machine.
//!
//! What ICMPv6 says of something that was not delivered is a message
//! like any other to the raw socket, which reads it when its filter lets
//! it by. What it means for the socket that sent what was not delivered
//! is said here too ([`undelivered_error`]).
//!
//! Not here: a socket bound to one address sending from it or hearing
//! only what is for it, the socket that pings being told of the errors
//! that come back for its echoes (in Linux one that asked with
//! `IPV6_RECVERR` is). A raw socket also hears what the card brings for
//! a group the machine is no member of.

use super::icmp::{checksum, ServedIcmpSocket};
use super::*;
use alloc::borrow::Cow;

const ECHO_REQUEST: u8 = 128;
const ECHO_REPLY: u8 = 129;
/// What an echo starts with: type, code, sum, identifier, sequence.
const ECHO_HEADER: usize = 8;
/// What every message starts with: type, code, sum, which is at 2.
const SUM_END: usize = 4;
const SUM_AT: i32 = 2;
/// The IPv6 header a message travels behind, and what it calls ICMPv6.
const IP_HEADER: usize = 40;
const ICMPV6: u8 = 58;
/// The hop limit of what this machine says to itself.
const HOPS_HERE: u8 = 64;
/// As far as a message goes when the program has not said: a group's
/// stays on the link.
const HOPS: u8 = 64;
const GROUP_HOPS: u8 = 1;

/// The levels and options of `setsockopt` that are these sockets'.
const IPPROTO_IP: usize = 0;
pub(super) const IPPROTO_IPV6: usize = 41;
const IPPROTO_ICMPV6: usize = 58;
const SOL_RAW: usize = 255;
const IPV6_CHECKSUM: usize = 7;
const IPV6_2292HOPLIMIT: usize = 8;
const IPV6_UNICAST_HOPS: usize = 16;
const IPV6_MULTICAST_HOPS: usize = 18;
const IPV6_V6ONLY: usize = 26;
const IPV6_RECVHOPLIMIT: usize = 51;
const IPV6_HOPLIMIT: usize = 52;
const ICMP6_FILTER: usize = 1;

/// ICMPv6's errors: destination unreachable, packet too big, time
/// exceeded and parameter problem.
const UNREACHABLE: u8 = 1;
const TOO_BIG: u8 = 2;
const TIME_EXCEEDED: u8 = 3;
const PARAMETER: u8 = 4;

/// What a program is told when an ICMPv6 message of this type and code
/// says that something its socket sent was not delivered, and whether
/// that is an error that does not mend by itself: Linux's table
/// (`icmpv6_err_convert`) and what `__udp6_lib_err` adds to it, which is
/// that a datagram too big for the way does not mend. `None` for a
/// message that is not about that.
///
/// As for IPv4 ([`super::icmp::undelivered_error`]), a connect under way
/// ends with the error, mending or not (`tcp_v6_err`), and a datagram
/// socket's program hears only of the ones that do not mend, and only on
/// a connected socket.
pub(super) fn undelivered_error(kind: u8, code: u8) -> Option<(LxError, bool)> {
    use LxError::*;
    match (kind, code) {
        (UNREACHABLE, code) => Some(match code {
            // No way there: a router's word, which may change.
            0 => (ENETUNREACH, false),
            // Forbidden on the way, by whoever runs it.
            1 | 5 | 6 => (EACCES, true),
            // Nobody answers at the address: a neighbour may be back.
            2 | 3 => (EHOSTUNREACH, false),
            // Nobody at the port: the host's own word.
            4 => (ECONNREFUSED, true),
            _ => (EPROTO, true),
        }),
        (TOO_BIG, _) => Some((EMSGSIZE, true)),
        (TIME_EXCEEDED, _) => Some((EHOSTUNREACH, false)),
        (PARAMETER, _) => Some((EPROTO, true)),
        _ => None,
    }
}

/// The error of a connect that a `FAILED` says ended because of an
/// ICMPv6 message, whose type and code are its `bytes`: `None` when it
/// carries none, or none that means anything.
pub(super) fn connect_error(bytes: &[u8]) -> Option<LxError> {
    let &[kind, code] = bytes else {
        return None;
    };
    undelivered_error(kind, code).map(|(error, _)| error)
}

/// The ICMPv6 message of an IPv6 packet that carries one right behind
/// its header, and the hops the packet had left.
pub(super) fn message_of(packet: &[u8]) -> Option<(u8, &[u8])> {
    if packet.len() < IP_HEADER || packet[0] >> 4 != 6 || packet[6] != ICMPV6 {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let message = packet.get(IP_HEADER..IP_HEADER + len)?;
    Some((packet[7], message))
}

/// The identifier of `message` when it is the answer to an echo.
pub(super) fn echo_reply_ident(message: &[u8]) -> Option<u16> {
    (message.len() >= ECHO_HEADER && message[0] == ECHO_REPLY && message[1] == 0)
        .then(|| u16::from_be_bytes([message[4], message[5]]))
}

/// Write into `message` the sum it has when it goes from `from` to `to`.
fn seal(from: Ipv6Address, to: Ipv6Address, message: &mut [u8]) {
    message[2..4].copy_from_slice(&[0, 0]);
    // The sum is over the addresses, the length and the protocol too.
    let mut summed = Vec::with_capacity(IP_HEADER + message.len());
    summed.extend_from_slice(&from.0);
    summed.extend_from_slice(&to.0);
    summed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    summed.extend_from_slice(&[0, 0, 0, ICMPV6]);
    summed.extend_from_slice(message);
    let sum = checksum(&summed);
    message[2..4].copy_from_slice(&sum.to_be_bytes());
}

/// Where a program means when it says `addr`: nowhere is this machine
/// (`ip6_datagram_connect`, `rawv6_sendmsg`). An IPv4 address written as
/// IPv6 is nowhere ICMPv6 goes.
fn destination(addr: IpAddress) -> LxResult<Ipv6Address> {
    match addr {
        IpAddress::Unspecified => Ok(Ipv6Address::LOOPBACK),
        IpAddress::Ipv6(addr) if addr.is_unspecified() => Ok(Ipv6Address::LOOPBACK),
        IpAddress::Ipv6(addr) if addr.as_ipv4().is_some() => Err(LxError::ENETUNREACH),
        IpAddress::Ipv6(addr) => Ok(addr),
        _ => Err(LxError::EAFNOSUPPORT),
    }
}

/// An `int` a program set an option to (`do_ipv6_setsockopt`: nothing
/// shorter is one).
fn int(data: &[u8]) -> LxResult<i32> {
    let bytes: [u8; 4] = data
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(LxError::EINVAL)?;
    Ok(i32::from_ne_bytes(bytes))
}

/// As many hops as an option says: -1 is as many as when it was not
/// said, and more than a byte holds is `EINVAL`.
fn hops(data: &[u8]) -> LxResult<Option<u8>> {
    match int(data)? {
        -1 => Ok(None),
        hops => hops.try_into().map(Some).map_err(|_| LxError::EINVAL),
    }
}

/// Whether `opt` is one of the two that ask `recvmsg` to say how many
/// hops what it read had left.
pub(super) fn tells_hops(opt: usize) -> bool {
    matches!(opt, IPV6_RECVHOPLIMIT | IPV6_2292HOPLIMIT)
}

/// The options of a socket that are IPv6's and not IPv4's: all of them
/// for a socket of ICMPv6, the hops for one of datagrams.
#[derive(Default)]
pub(super) struct Opts {
    /// `IPV6_UNICAST_HOPS` and `IPV6_MULTICAST_HOPS`, when said.
    hops: Option<u8>,
    group_hops: Option<u8>,
    /// `IPV6_RECVHOPLIMIT` and `IPV6_2292HOPLIMIT`: `recvmsg` says how
    /// many hops what it read had left.
    tell_hops: bool,
    tell_hops_2292: bool,
}

impl Opts {
    /// `setsockopt` of how many hops what the socket sends may travel
    /// (`IPV6_UNICAST_HOPS`, `IPV6_MULTICAST_HOPS`): `None` for any
    /// other option.
    pub(super) fn set_hops(&mut self, opt: usize, data: &[u8]) -> Option<LxResult<()>> {
        let kept = match opt {
            IPV6_UNICAST_HOPS => &mut self.hops,
            IPV6_MULTICAST_HOPS => &mut self.group_hops,
            _ => return None,
        };
        Some(hops(data).map(|hops| *kept = hops))
    }

    /// How many hops what goes to `to` may travel, when the socket was
    /// told: `None` when it was not, which is 64, and 1 to a group
    /// ([`hops_to`](Self::hops_to)). Never 0: the stack sends nothing
    /// that is to go nowhere.
    pub(super) fn hops_said(&self, to: &Ipv6Address) -> Option<u8> {
        let said = if to.is_multicast() {
            self.group_hops
        } else {
            self.hops
        };
        said.map(|hops| hops.max(1))
    }

    /// How many hops what goes to `to` may travel.
    fn hops_to(&self, to: &Ipv6Address) -> u8 {
        let unsaid = if to.is_multicast() { GROUP_HOPS } else { HOPS };
        self.hops_said(to).unwrap_or(unsaid)
    }

    /// What `getsockopt` says an option of IPv6's level is, for the ones
    /// kept here (`do_ipv6_getsockopt`: hops that were not said are the
    /// ones a packet gets). `IPV6_V6ONLY` is no: a socket served by the
    /// stack takes both families, or is of ICMPv6 and has one.
    pub(super) fn get(&self, opt: usize) -> Option<i32> {
        match opt {
            IPV6_UNICAST_HOPS => Some(i32::from(self.hops.unwrap_or(HOPS))),
            IPV6_MULTICAST_HOPS => Some(i32::from(self.group_hops.unwrap_or(GROUP_HOPS))),
            IPV6_RECVHOPLIMIT => Some(i32::from(self.tell_hops)),
            IPV6_2292HOPLIMIT => Some(i32::from(self.tell_hops_2292)),
            IPV6_V6ONLY => Some(0),
            _ => None,
        }
    }
}

impl StackLink {
    /// Whether `addr` is this machine: `::1`, or an address of the
    /// card's.
    fn is_here6(&self, addr: &Ipv6Address) -> bool {
        addr.is_loopback() || self.sixes.has(addr)
    }

    /// An ICMPv6 message a program of this machine sent to this machine
    /// at `to`. Whoever hears ICMPv6 hears it, and an echo is answered,
    /// as the stack of a machine answers one: neither leaves it.
    fn icmp_here6(&self, to: Ipv6Address, message: &[u8]) {
        let mut said = message.to_vec();
        seal(to, to, &mut said);
        self.heard6(to, HOPS_HERE, &said);
        if said.len() >= ECHO_HEADER && said[0] == ECHO_REQUEST && said[1] == 0 {
            said[0] = ECHO_REPLY;
            seal(to, to, &mut said);
            self.heard6(to, HOPS_HERE, &said);
        }
    }
}

impl ServedIcmpSocket {
    /// What leaves when a socket that pings writes `data`: an echo, with
    /// the socket's identifier (`ping_common_sendmsg`: anything else is
    /// `EINVAL`).
    fn echo6(&self, data: &[u8]) -> LxResult<Vec<u8>> {
        if data.len() < ECHO_HEADER || data[0] != ECHO_REQUEST || data[1] != 0 {
            return Err(LxError::EINVAL);
        }
        let ident = self.autobind()?;
        let mut message = data.to_vec();
        message[4..6].copy_from_slice(&ident.to_be_bytes());
        Ok(message)
    }

    /// `write`, on a socket of ICMPv6.
    pub(super) fn write6(&self, data: &[u8], to: Option<Endpoint>) -> SysResult {
        let to = match to {
            Some(Endpoint::Ip(to)) => to,
            Some(_) => return Err(LxError::EAFNOSUPPORT),
            None => self.shared.inner.lock().remote.ok_or(LxError::ENOTCONN)?,
        };
        let to = destination(to.addr)?;
        // The length of what an IPv6 packet carries is 16 bits.
        if data.len() > usize::from(u16::MAX) {
            return Err(LxError::EMSGSIZE);
        }
        let message = if self.is_raw() {
            // The sum has to have where to be written
            // (`rawv6_push_pending_frames`).
            if data.len() < SUM_END {
                return Err(LxError::EINVAL);
            }
            Cow::Borrowed(data)
        } else {
            Cow::Owned(self.echo6(data)?)
        };
        if self.link.is_here6(&to) {
            self.link.icmp_here6(to, &message);
            return Ok(data.len());
        }
        // Nothing leaves the machine while no stack serves, nor for
        // where the card has no way to.
        if self.link.is_down() {
            return Err(LxError::ENETDOWN);
        }
        if !self.link.sixes.reaches(&to) {
            return Err(LxError::ENETUNREACH);
        }
        // What one frame carries behind an IPv6 header: a UDP datagram's
        // header is as long as an echo's, and the stack does not
        // fragment.
        let longest = self.link.max_datagram.load(Ordering::Acquire) + ECHO_HEADER;
        if message.len() + IPV6_MORE > longest {
            return Err(LxError::EMSGSIZE);
        }
        let hop_limit = self.six.lock().hops_to(&to);
        let said = icmp6_message(self.sock, to.0, hop_limit, &message);
        self.link.tell(said, false)?;
        Ok(data.len())
    }

    /// `connect`, on a socket of ICMPv6.
    pub(super) fn connect6(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(peer) = endpoint else {
            return Err(LxError::EAFNOSUPPORT);
        };
        let peer = destination(peer.addr)?;
        if !self.is_raw() {
            self.autobind()?;
        }
        // Who answers has no port to answer from.
        self.shared.inner.lock().remote = Some(IpEndpoint::new(IpAddress::Ipv6(peer), 0));
        Ok(0)
    }

    /// `bind`, on a socket of ICMPv6.
    pub(super) fn bind6(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(local) = endpoint else {
            return Err(LxError::EINVAL);
        };
        match local.addr {
            IpAddress::Unspecified => {}
            IpAddress::Ipv6(addr) if addr.is_unspecified() || self.link.is_here6(&addr) => {}
            IpAddress::Ipv6(_) => return Err(LxError::EADDRNOTAVAIL),
            _ => return Err(LxError::EAFNOSUPPORT),
        }
        // A raw socket has no port, and a `sockaddr`'s is not looked at.
        if self.is_raw() {
            return Ok(0);
        }
        // `ping_bind`: a socket that has an identifier is `EINVAL`.
        if self.shared.inner.lock().port.is_some() {
            return Err(LxError::EINVAL);
        }
        let ident = self.link.ident(local.port)?;
        self.shared.inner.lock().port = Some(ident);
        Ok(0)
    }

    /// `setsockopt`, on a socket of ICMPv6: what it answers for an
    /// option that is one of these sockets', `None` for any other.
    pub(super) fn setsockopt6(&self, level: usize, opt: usize, data: &[u8]) -> Option<SysResult> {
        let done = |set: LxResult<()>| Some(set.map(|()| 0));
        match (level, opt) {
            // IPv4's are no options of a socket of this family.
            (IPPROTO_IP, _) => Some(Err(LxError::ENOPROTOOPT)),
            (IPPROTO_IPV6, IPV6_UNICAST_HOPS | IPV6_MULTICAST_HOPS) => {
                done(self.six.lock().set_hops(opt, data)?)
            }
            (IPPROTO_IPV6, IPV6_RECVHOPLIMIT) => done(int(data).map(|on| {
                self.six.lock().tell_hops = on != 0;
            })),
            (IPPROTO_IPV6, IPV6_2292HOPLIMIT) => done(int(data).map(|on| {
                self.six.lock().tell_hops_2292 = on != 0;
            })),
            // Where the sum is written, which a program may say when it
            // says where it is anyway. At this level only
            // (`do_rawv6_setsockopt`: at IPv6's it is `EINVAL` for a
            // socket of ICMPv6).
            (SOL_RAW, IPV6_CHECKSUM) if self.is_raw() => done(int(data).and_then(|at| {
                if at == SUM_AT {
                    Ok(())
                } else {
                    Err(LxError::EINVAL)
                }
            })),
            (IPPROTO_IPV6, IPV6_CHECKSUM) if self.is_raw() => Some(Err(LxError::EINVAL)),
            // As much of the filter as the program gave, the rest as it
            // was (`rawv6_seticmpfilter`).
            (IPPROTO_ICMPV6, ICMP6_FILTER) if self.is_raw() => {
                let mut inner = self.shared.inner.lock();
                let given = data.len().min(inner.blocked.len());
                inner.blocked[..given].copy_from_slice(&data[..given]);
                Some(Ok(0))
            }
            (IPPROTO_ICMPV6, _) => Some(Err(LxError::ENOPROTOOPT)),
            _ => None,
        }
    }

    /// `getsockopt`, on a socket of ICMPv6, of the options that are
    /// these sockets': IPv6's, and the raw one's filter.
    pub(super) fn getsockopt6(&self, level: usize, opt: usize) -> Option<Vec<u8>> {
        match (level, opt) {
            (IPPROTO_IPV6, _) => {
                let said = self.six.lock().get(opt)?;
                Some(said.to_ne_bytes().to_vec())
            }
            (IPPROTO_ICMPV6, ICMP6_FILTER) if self.is_raw() => {
                Some(self.shared.inner.lock().blocked.to_vec())
            }
            _ => None,
        }
    }

    /// What `recvmsg` says beside what it last read: how many hops it
    /// had left, to a program that asked, as each way of asking is
    /// answered (`ip6_datagram_recv_specific_ctl`).
    pub(super) fn recv_cmsgs6(&self) -> Vec<(i32, i32, i32)> {
        let six = self.six.lock();
        if !six.tell_hops && !six.tell_hops_2292 {
            return Vec::new();
        }
        let hops = i32::from(self.shared.inner.lock().last_hops);
        let mut said = Vec::new();
        if six.tell_hops {
            said.push((IPPROTO_IPV6 as i32, IPV6_HOPLIMIT as i32, hops));
        }
        if six.tell_hops_2292 {
            said.push((IPPROTO_IPV6 as i32, IPV6_2292HOPLIMIT as i32, hops));
        }
        said
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{any, at, at6, data6, ip, stack, stack_of, Stack, MAX_FOR_TESTS};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    /// The card's addresses, a neighbour on its network and on its link,
    /// and somewhere past a router.
    const LINK: &str = "fe80::7";
    const GIVEN: &str = "fec0::7";
    const NEAR: &str = "fec0::99";
    const FAR: &str = "2001:db8::1";
    const HERE: &str = "::1";

    fn six(addr: &str) -> Ipv6Address {
        match at6(addr, 0).addr {
            IpAddress::Ipv6(addr) => addr,
            _ => unreachable!(),
        }
    }

    /// A stack whose card has LINK and GIVEN, and no router.
    fn stack6() -> Stack {
        let stack = stack();
        let cards = [Ipv6Cidr::new(six(LINK), 64), Ipv6Cidr::new(six(GIVEN), 64)];
        stack.say(six_message(None, &cards));
        stack
    }

    fn socket(link: &Arc<StackLink>, raw: bool) -> ServedIcmpSocket {
        let s = ServedIcmpSocket::new6(link.clone(), raw).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// A destination, as `sendto` gives it: no port.
    fn to(addr: &str) -> Option<Endpoint> {
        Some(Endpoint::Ip(at6(addr, 0)))
    }

    /// What `recv` reads next, and who sent it.
    fn next(s: &ServedIcmpSocket) -> Option<(Vec<u8>, IpEndpoint)> {
        let mut buf = [0u8; 2048];
        match async_std::task::block_on(Socket::read(s, &mut buf)) {
            (Ok(n), from) => Some((buf[..n].to_vec(), ip(from))),
            (Err(LxError::EAGAIN), _) => None,
            (Err(e), _) => panic!("{:?}", e),
        }
    }

    /// An ICMPv6 message of this type with this identifier and sequence
    /// 1, as a program writes it: the sum is not its to write.
    fn icmp(kind: u8, ident: u16, data: &[u8]) -> Vec<u8> {
        let mut m = vec![kind, 0, 0xde, 0xad];
        m.extend_from_slice(&ident.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        m.extend_from_slice(data);
        m
    }

    /// `message` as it travels from `from` to `to`: its sum right.
    fn sealed(from: &str, to: &str, message: &[u8]) -> Vec<u8> {
        let mut message = message.to_vec();
        seal(six(from), six(to), &mut message);
        message
    }

    /// Whether `message` has the sum of one that went from `from` to `to`.
    fn sum_right(from: &str, to: &str, message: &[u8]) -> bool {
        sealed(from, to, message) == message
    }

    /// The IPv6 packet that carries `message` from `from` to `to` with
    /// `hops` left.
    fn packet(from: &str, to: &str, hops: u8, message: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x60, 0, 0, 0];
        packet.extend_from_slice(&(message.len() as u16).to_be_bytes());
        packet.extend_from_slice(&[ICMPV6, hops]);
        packet.extend_from_slice(&six(from).0);
        packet.extend_from_slice(&six(to).0);
        packet.extend_from_slice(&sealed(from, to, message));
        packet
    }

    /// The stack heard `message` from `from` for GIVEN, with 57 hops
    /// left.
    fn hears(stack: &Stack, from: &str, message: &[u8]) {
        stack.say(data6(HEARD, from, 0, &packet(from, GIVEN, 57, message)));
    }

    #[test]
    fn what_icmpv6_says_means_what_it_means_on_linux() {
        use LxError::*;
        // Destination unreachable, code by code (`tab_unreach`), and a
        // code past the table.
        let unreachable = [
            (ENETUNREACH, false),
            (EACCES, true),
            (EHOSTUNREACH, false),
            (EHOSTUNREACH, false),
            (ECONNREFUSED, true),
            (EACCES, true),
            (EACCES, true),
            (EPROTO, true),
        ];
        for (code, means) in unreachable.iter().enumerate() {
            assert_eq!(undelivered_error(1, code as u8), Some(*means), "{}", code);
        }
        assert_eq!(undelivered_error(1, 200), Some((EPROTO, true)));
        // Too big for the way, out of hops, a header nobody understood.
        assert_eq!(undelivered_error(2, 0), Some((EMSGSIZE, true)));
        assert_eq!(undelivered_error(3, 0), Some((EHOSTUNREACH, false)));
        assert_eq!(undelivered_error(3, 1), Some((EHOSTUNREACH, false)));
        assert_eq!(undelivered_error(4, 2), Some((EPROTO, true)));
        // What is no error of something sent.
        for kind in [0, 5, 100, 127, 128, 129, 133, 136, 255] {
            assert_eq!(undelivered_error(kind, 0), None, "type {}", kind);
        }
        assert_eq!(connect_error(&[1, 3]), Some(EHOSTUNREACH));
        assert_eq!(connect_error(&[1, 4]), Some(ECONNREFUSED));
        assert_eq!(connect_error(&[]), None);
        assert_eq!(connect_error(&[1, 3, 0]), None);
        assert_eq!(connect_error(&[128, 0]), None);
    }

    #[test]
    fn the_messages_are_laid_out_as_the_stack_reads_them() {
        let m = icmp6_message(0x0102, six(NEAR).0, 9, &[128, 0, 0, 0]);
        let mut said = vec![0, 0, 0, 0, 21, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0];
        said.extend_from_slice(&[0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x99]);
        said.extend_from_slice(&[9, 0, 0, 0, 128, 0, 0, 0]);
        assert_eq!(m, said);
        assert_eq!(
            hear6_message(0x0102),
            [0, 0, 0, 0, 22, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0]
        );
        // The sum is over the addresses too: an echo from GIVEN to NEAR.
        let echo = sealed(GIVEN, NEAR, &icmp(ECHO_REQUEST, 0x4242, b"hi!"));
        assert_eq!(echo[2..4], [0xb5, 0xeb]);
        assert!(sum_right(GIVEN, NEAR, &echo));
        assert!(!sum_right(LINK, NEAR, &echo));
        // A packet's message is what its header says is behind it, and
        // the hops are the header's.
        let p = packet(NEAR, GIVEN, 57, &echo);
        assert_eq!(p.len(), IP_HEADER + echo.len());
        assert_eq!(message_of(&p), Some((57, &echo[..])));
        let mut longer = p.clone();
        longer.extend_from_slice(b"padding");
        assert_eq!(message_of(&longer), Some((57, &echo[..])));
        assert_eq!(message_of(&p[..p.len() - 1]), None);
        assert_eq!(message_of(&p[..IP_HEADER - 1]), None);
        assert_eq!(message_of(&[]), None);
        let mut udp = p.clone();
        udp[6] = 17;
        assert_eq!(message_of(&udp), None);
        let mut four = p;
        four[0] = 0x45;
        assert_eq!(message_of(&four), None);
        assert_eq!(
            echo_reply_ident(&icmp(ECHO_REPLY, 0x4242, b"")),
            Some(0x4242)
        );
        assert_eq!(echo_reply_ident(&icmp(ECHO_REQUEST, 0x4242, b"")), None);
        assert_eq!(echo_reply_ident(&[ECHO_REPLY, 0, 0, 0, 0x42]), None);
    }

    #[test]
    fn a_socket_says_what_its_options_are() {
        let _g = LOCK.lock();
        let stack = stack6();
        let raw = socket(&stack.link, true);
        let pings = socket(&stack.link, false);
        let get = |s: &ServedIcmpSocket, opt: usize| {
            let said = Socket::getsockopt_own(s, 41, opt)?;
            Some(i32::from_ne_bytes(said.try_into().unwrap()))
        };
        let set = |s: &ServedIcmpSocket, opt: usize, value: i32| {
            Socket::setsockopt(s, 41, opt, &value.to_ne_bytes())
        };
        for s in [&raw, &pings] {
            // As they are before anybody says: 64 hops, 1 to a group,
            // nothing said of what arrives, and not IPv6 only.
            let before = [(16, 64), (18, 1), (51, 0), (8, 0), (26, 0)];
            for (opt, value) in before {
                assert_eq!(get(s, opt), Some(value), "option {}", opt);
            }
            // And as they were set.
            for (opt, value) in [(16, 7), (18, 3), (51, 1), (8, 5)] {
                assert_eq!(set(s, opt, value), Ok(0));
            }
            let after = [(16, 7), (18, 3), (51, 1), (8, 1), (26, 0)];
            for (opt, value) in after {
                assert_eq!(get(s, opt), Some(value), "option {}", opt);
            }
            // -1 hops is as many as before.
            assert_eq!(set(s, 16, -1), Ok(0));
            assert_eq!(get(s, 16), Some(64));
            // Not one of these.
            assert_eq!(get(s, 7), None, "IPV6_CHECKSUM");
            assert_eq!(get(s, 1000), None);
            assert_eq!(Socket::getsockopt_own(s, 0, 2), None, "IP_TTL");
            assert_eq!(Socket::getsockopt_own(s, 58, 2), None);
        }
        // The filter is the raw socket's: all let by, then as it was set.
        assert_eq!(Socket::getsockopt_own(&raw, 58, 1), Some(vec![0; 32]));
        let mut filter = [0xffu8; 32];
        filter[16] = !0b10;
        assert_eq!(Socket::setsockopt(&raw, 58, 1, &filter), Ok(0));
        assert_eq!(Socket::getsockopt_own(&raw, 58, 1), Some(filter.to_vec()));
        assert_eq!(Socket::getsockopt_own(&pings, 58, 1), None);

        // A socket of IPv4's ICMP has none of them.
        let four = ServedIcmpSocket::new(stack.link.clone(), true).unwrap();
        assert_eq!(Socket::getsockopt_own(&four, 41, 16), None);
        assert_eq!(Socket::getsockopt_own(&four, 58, 1), None);
    }

    #[test]
    fn a_raw_socket_sends_its_messages_and_reads_what_its_filter_lets_by() {
        let _g = LOCK.lock();
        let stack = stack6();
        let s = socket(&stack.link, true);
        assert_eq!(Socket::socket_type(&s), Some(SocketType::SOCK_RAW));
        assert_eq!(Socket::so_domain(&s), Some(Domain::AF_INET6 as u32));
        assert_eq!(Socket::so_protocol(&s), Some(58));
        assert!(!Socket::is_raw_ipv4(&s));
        // The stack is told the socket hears before it says anything.
        assert_eq!(stack.told(), Some(hear6_message(s.sock)));
        assert_eq!(Socket::write(&s, b"xxxx", None), Err(LxError::ENOTCONN));
        // The message is the program's, whatever its type; the sum is
        // the stack's to write, so what is in its place goes as it is.
        let ping = icmp(ECHO_REQUEST, 0x4242, b"anybody?");
        assert_eq!(Socket::write(&s, &ping, to(NEAR)), Ok(ping.len()));
        let near = six(NEAR).0;
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, near, 64, &ping)));
        let odd = [200u8, 7, 0, 0];
        assert_eq!(Socket::write(&s, &odd, to("fe80::99")), Ok(4));
        let link = six("fe80::99").0;
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, link, 64, &odd)));
        // One with no place for the sum is not one.
        assert_eq!(Socket::write(&s, &odd[..3], to(NEAR)), Err(LxError::EINVAL));

        // `IPV6_UNICAST_HOPS` is how far it goes, and to a group
        // `IPV6_MULTICAST_HOPS`: the link, unless the program says
        // further. -1 is as when nothing was said.
        let set = |opt: usize, value: i32| Socket::setsockopt(&s, 41, opt, &value.to_ne_bytes());
        assert_eq!(set(16, 3), Ok(0));
        assert_eq!(Socket::write(&s, &ping, to(NEAR)), Ok(ping.len()));
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, near, 3, &ping)));
        let all = six("ff02::1").0;
        assert_eq!(Socket::write(&s, &ping, to("ff02::1")), Ok(ping.len()));
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, all, 1, &ping)));
        assert_eq!(set(18, 9), Ok(0));
        assert_eq!(Socket::write(&s, &ping, to("ff02::1")), Ok(ping.len()));
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, all, 9, &ping)));
        assert_eq!(set(16, -1), Ok(0));
        assert_eq!(set(18, -1), Ok(0));
        assert_eq!(Socket::write(&s, &ping, to(NEAR)), Ok(ping.len()));
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, near, 64, &ping)));
        assert_eq!(Socket::write(&s, &ping, to("ff02::1")), Ok(ping.len()));
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, all, 1, &ping)));
        for opt in [16, 18] {
            assert_eq!(set(opt, 256), Err(LxError::EINVAL));
            assert_eq!(set(opt, -2), Err(LxError::EINVAL));
            assert_eq!(Socket::setsockopt(&s, 41, opt, &[1]), Err(LxError::EINVAL));
        }

        // One frame's worth and no more, and nowhere the card has no way
        // to: past its networks with no router, and IPv4 at all.
        let long = vec![0u8; MAX_FOR_TESTS + ECHO_HEADER - IPV6_MORE + 1];
        assert_eq!(Socket::write(&s, &long, to(NEAR)), Err(LxError::EMSGSIZE));
        assert_eq!(
            Socket::write(&s, &long[..long.len() - 1], to(NEAR)),
            Ok(long.len() - 1)
        );
        assert!(stack.told().is_some());
        assert_eq!(Socket::write(&s, &ping, to(FAR)), Err(LxError::ENETUNREACH));
        assert_eq!(
            Socket::write(&s, &ping, to("::ffff:10.0.9.1")),
            Err(LxError::ENETUNREACH)
        );
        let four = Some(Endpoint::Ip(at([10, 0, 9, 1], 0)));
        assert_eq!(Socket::write(&s, &ping, four), Err(LxError::EAFNOSUPPORT));
        assert_eq!(stack.told(), None);

        // What the stack hears is read without its header, whoever it is
        // for and whatever it is, from who sent it.
        assert_eq!(Socket::poll(&s, PollEvents::IN), (false, true, false));
        let pong = icmp(ECHO_REPLY, 0x4242, b"anybody?");
        let asks = icmp(135, 0, b"where is fec0::7?");
        hears(&stack, NEAR, &pong);
        hears(&stack, NEAR, &asks);
        assert_eq!(Socket::poll(&s, PollEvents::IN), (true, true, false));
        assert_eq!(FileLike::readable_bytes(&s), Some(pong.len()));
        assert_eq!(next(&s), Some((sealed(NEAR, GIVEN, &pong), at6(NEAR, 0))));
        // How many hops it had left is said to who asked, each way of
        // asking with its answer.
        assert_eq!(Socket::recv_cmsgs(&s), []);
        assert_eq!(set(8, 1), Ok(0));
        assert_eq!(next(&s), Some((sealed(NEAR, GIVEN, &asks), at6(NEAR, 0))));
        assert_eq!(Socket::recv_cmsgs(&s), [(41, 8, 57)]);
        assert_eq!(set(51, 1), Ok(0));
        assert_eq!(Socket::recv_cmsgs(&s), [(41, 52, 57), (41, 8, 57)]);
        assert_eq!(set(8, 0), Ok(0));
        assert_eq!(Socket::recv_cmsgs(&s), [(41, 52, 57)]);
        assert_eq!(next(&s), None);

        // `ICMP6_FILTER`: a bit for each type, set for one that is not
        // wanted. What busybox's `ping` says: only the answers to echoes.
        let mut filter = [0xffu8; 32];
        filter[usize::from(ECHO_REPLY >> 3)] &= !(1 << (ECHO_REPLY & 7));
        assert_eq!(Socket::setsockopt(&s, 58, 1, &filter), Ok(0));
        hears(&stack, NEAR, &asks);
        hears(&stack, NEAR, &icmp(ECHO_REQUEST, 1, b"you?"));
        hears(&stack, NEAR, &pong);
        assert_eq!(next(&s), Some((sealed(NEAR, GIVEN, &pong), at6(NEAR, 0))));
        assert_eq!(next(&s), None);
        // As much of it as was given: the first word lets everything of
        // its types by again, which are the errors.
        assert_eq!(Socket::setsockopt(&s, 58, 1, &[0; 4]), Ok(0));
        hears(&stack, NEAR, &icmp(1, 0, b"no way"));
        hears(&stack, NEAR, &asks);
        assert_eq!(next(&s).map(|(m, _)| m[0]), Some(1));
        assert_eq!(next(&s), None);
        assert_eq!(
            Socket::setsockopt(&s, 58, 2, &filter),
            Err(LxError::ENOPROTOOPT)
        );

        // The sum is where it is, which a program may say; and IPv4's
        // options are none of this socket's.
        let at = |level: usize, at: i32| Socket::setsockopt(&s, level, 7, &at.to_ne_bytes());
        assert_eq!(at(255, 2), Ok(0));
        assert_eq!(at(255, 4), Err(LxError::EINVAL));
        assert_eq!(at(255, -1), Err(LxError::EINVAL));
        assert_eq!(at(41, 2), Err(LxError::EINVAL));
        assert_eq!(
            Socket::setsockopt(&s, 0, 2, &3u32.to_ne_bytes()),
            Err(LxError::ENOPROTOOPT)
        );

        // A socket of ICMP hears nothing of this, nor this one of ICMP.
        let four = ServedIcmpSocket::new(stack.link.clone(), true).unwrap();
        FileLike::set_flags(&four, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(stack.told(), Some(hear_message(four.sock)));
        assert_eq!(Socket::setsockopt(&s, 58, 1, &[0; 32]), Ok(0));
        hears(&stack, NEAR, &pong);
        let mut buf = [0u8; 64];
        let read = async_std::task::block_on(Socket::read(&four, &mut buf));
        assert_eq!(read.0, Err(LxError::EAGAIN));
        let mut v4 = vec![0x45, 0, 0, 28, 0, 0, 0x40, 0, 64, 1, 0, 0];
        v4.extend_from_slice(&[10, 0, 9, 1, 10, 0, 9, 15, 0, 0, 0, 0, 0, 0, 0, 0]);
        stack.arrives(HEARD, [10, 0, 9, 1], 0, &v4);
        assert_eq!(next(&s).map(|(m, _)| m[0]), Some(ECHO_REPLY));
        assert_eq!(next(&s), None);
        assert_eq!(
            async_std::task::block_on(Socket::read(&four, &mut buf)).0,
            Ok(28)
        );

        let closed = s.sock;
        drop(s);
        assert_eq!(stack.told(), Some(close_message(closed)));
        // And what is heard after that is nobody's.
        hears(&stack, NEAR, &pong);
    }

    #[test]
    fn a_socket_that_pings_sends_echoes_as_its_own_and_reads_their_answers() {
        let _g = LOCK.lock();
        let stack = stack6();
        let s = socket(&stack.link, false);
        let other = socket(&stack.link, false);
        assert_eq!(Socket::socket_type(&s), Some(SocketType::SOCK_DGRAM));
        assert_eq!(stack.told(), Some(hear6_message(s.sock)));
        assert_eq!(stack.told(), Some(hear6_message(other.sock)));
        // An echo and nothing else (`ping_common_sendmsg`).
        assert_eq!(
            Socket::write(&s, &[128, 0, 0], to(NEAR)),
            Err(LxError::EINVAL)
        );
        let reply = icmp(ECHO_REPLY, 1, b"");
        assert_eq!(Socket::write(&s, &reply, to(NEAR)), Err(LxError::EINVAL));
        // IPv4's echo is none of ICMPv6's.
        let old = icmp(8, 1, b"");
        assert_eq!(Socket::write(&s, &old, to(NEAR)), Err(LxError::EINVAL));
        assert_eq!(stack.told(), None);
        assert_eq!(ip(Socket::endpoint(&s).unwrap()), at6("::", 0));

        // It leaves with the socket's identifier, which is its port.
        let ping = icmp(ECHO_REQUEST, 0x4242, b"anybody?");
        assert_eq!(Socket::write(&s, &ping, to(NEAR)), Ok(ping.len()));
        let ident = ip(Socket::endpoint(&s).unwrap()).port;
        assert_ne!(ident, 0);
        assert_eq!(ip(Socket::endpoint(&s).unwrap()), at6("::", ident));
        let sent = icmp(ECHO_REQUEST, ident, b"anybody?");
        let near = six(NEAR).0;
        assert_eq!(stack.told(), Some(icmp6_message(s.sock, near, 64, &sent)));
        // Two sockets never ping as one, whichever kind of echo each
        // sends.
        assert_eq!(Socket::write(&other, &ping, to(NEAR)), Ok(ping.len()));
        let theirs = ip(Socket::endpoint(&other).unwrap()).port;
        assert_ne!(theirs, ident);
        assert!(stack.told().is_some());
        let four = ServedIcmpSocket::new(stack.link.clone(), false).unwrap();
        assert_eq!(Socket::bind(&four, any(ident)), Err(LxError::EADDRINUSE));
        drop(four);
        while stack.told().is_some() {}

        // The answer to its echo is its own, without the header; the
        // answer to another's is not, nor is anything that is no answer.
        hears(&stack, NEAR, &icmp(ECHO_REPLY, theirs, b"for the other"));
        hears(&stack, NEAR, &icmp(ECHO_REQUEST, ident, b"an echo"));
        hears(&stack, NEAR, &icmp(1, ident, b"an error"));
        let pong = icmp(ECHO_REPLY, ident, b"anybody?");
        hears(&stack, NEAR, &pong);
        assert_eq!(next(&s), Some((sealed(NEAR, GIVEN, &pong), at6(NEAR, 0))));
        assert_eq!(next(&s), None);
        assert_eq!(next(&other).map(|(m, _)| m.len()), Some(8 + 13));
        assert_eq!(Socket::setsockopt(&s, 41, 51, &1u32.to_ne_bytes()), Ok(0));
        assert_eq!(Socket::recv_cmsgs(&s), [(41, 52, 57)]);

        // Bound, it pings as what it was bound to, once: to any address
        // or one of the machine's.
        let bound = socket(&stack.link, false);
        let bind = |s: &ServedIcmpSocket, addr: &str, ident: u16| {
            Socket::bind(s, Endpoint::Ip(at6(addr, ident)))
        };
        assert_eq!(bind(&bound, "::", theirs), Err(LxError::EADDRINUSE));
        assert_eq!(bind(&bound, NEAR, 777), Err(LxError::EADDRNOTAVAIL));
        assert_eq!(Socket::bind(&bound, any(777)), Err(LxError::EAFNOSUPPORT));
        assert_eq!(bind(&bound, GIVEN, 777), Ok(0));
        assert_eq!(bind(&bound, "::", 778), Err(LxError::EINVAL));
        assert_eq!(bind(&socket(&stack.link, false), HERE, 779), Ok(0));
        while stack.told().is_some() {}
        assert_eq!(Socket::write(&bound, &ping, to(NEAR)), Ok(ping.len()));
        let sent = icmp(ECHO_REQUEST, 777, b"anybody?");
        assert_eq!(
            stack.told(),
            Some(icmp6_message(bound.sock, near, 64, &sent))
        );
        // The filter is a raw socket's.
        assert_eq!(
            Socket::setsockopt(&s, 58, 1, &[0; 32]),
            Err(LxError::ENOPROTOOPT)
        );

        // Connected, it pings its peer and hears nobody else.
        let c = socket(&stack.link, false);
        let connect = |to| async_std::task::block_on(Socket::connect(&c, Endpoint::Ip(to)));
        assert_eq!(connect(at6(NEAR, 0)), Ok(0));
        assert_eq!(Socket::remote_endpoint(&c).map(ip), Some(at6(NEAR, 0)));
        let ident = ip(Socket::endpoint(&c).unwrap()).port;
        while stack.told().is_some() {}
        assert_eq!(FileLike::write(&c, &ping), Ok(ping.len()));
        let sent = icmp(ECHO_REQUEST, ident, b"anybody?");
        assert_eq!(stack.told(), Some(icmp6_message(c.sock, near, 64, &sent)));
        let pong = icmp(ECHO_REPLY, ident, b"anybody?");
        hears(&stack, "fec0::77", &pong);
        hears(&stack, NEAR, &pong);
        assert_eq!(next(&c), Some((sealed(NEAR, GIVEN, &pong), at6(NEAR, 0))));
        assert_eq!(next(&c), None);
    }

    #[test]
    fn an_echo_to_this_machine_is_answered_here_with_or_without_a_stack() {
        let _g = LOCK.lock();
        let link = StackLink::new();
        let raw = socket(&link, true);
        let pings = socket(&link, false);
        let ping = icmp(ECHO_REQUEST, 0x4242, b"me?");
        // No stack: what would leave the machine has nowhere to go.
        assert_eq!(Socket::write(&raw, &ping, to(NEAR)), Err(LxError::ENETDOWN));
        // What stays in it is heard and answered. A raw socket hears the
        // echo and its answer, each with the sum it would have on a wire.
        assert_eq!(Socket::write(&raw, &ping, to(HERE)), Ok(ping.len()));
        let (asked, from) = next(&raw).unwrap();
        assert_eq!(from, at6(HERE, 0));
        assert_eq!(asked, sealed(HERE, HERE, &ping));
        let (answered, from) = next(&raw).unwrap();
        assert_eq!(from, at6(HERE, 0));
        let pong = icmp(ECHO_REPLY, 0x4242, b"me?");
        assert_eq!(answered, sealed(HERE, HERE, &pong));
        assert_eq!(next(&raw), None);
        // The socket that pings heard neither: they were not its own.
        assert_eq!(next(&pings), None);
        // Nowhere is here too, and the socket that pings reads its
        // answer, which went as far as nothing goes in one machine.
        assert_eq!(
            Socket::setsockopt(&pings, 41, 8, &1u32.to_ne_bytes()),
            Ok(0)
        );
        assert_eq!(Socket::write(&pings, &ping, to("::")), Ok(ping.len()));
        let ident = ip(Socket::endpoint(&pings).unwrap()).port;
        let pong = icmp(ECHO_REPLY, ident, b"me?");
        assert_eq!(
            next(&pings),
            Some((sealed(HERE, HERE, &pong), at6(HERE, 0)))
        );
        assert_eq!(Socket::recv_cmsgs(&pings), [(41, 8, 64)]);
        assert_eq!(next(&raw).map(|(m, _)| m[0]), Some(ECHO_REQUEST));
        assert_eq!(next(&raw).map(|(m, _)| m[0]), Some(ECHO_REPLY));
        // What is no echo has no answer.
        assert_eq!(Socket::write(&raw, &[200, 0, 0, 0], to(HERE)), Ok(4));
        assert_eq!(next(&raw).map(|(m, _)| m.len()), Some(4));
        assert_eq!(next(&raw), None);

        // The stack that comes is told of both, each as what it is, and
        // of a socket of ICMP as that; and the card's addresses are this
        // machine once it says them.
        let four = ServedIcmpSocket::new(link.clone(), true).unwrap();
        let stack = stack_of(&link);
        assert_eq!(stack.told(), Some(hear6_message(raw.sock)));
        assert_eq!(stack.told(), Some(hear6_message(pings.sock)));
        assert_eq!(stack.told(), Some(hear_message(four.sock)));
        assert_eq!(stack.told(), None);
        assert_eq!(
            Socket::write(&pings, &ping, to(GIVEN)),
            Err(LxError::ENETUNREACH)
        );
        let cards = [Ipv6Cidr::new(six(GIVEN), 64)];
        stack.say(six_message(None, &cards));
        assert_eq!(Socket::write(&pings, &ping, to(GIVEN)), Ok(ping.len()));
        assert_eq!(stack.told(), None);
        assert_eq!(
            next(&pings),
            Some((sealed(GIVEN, GIVEN, &pong), at6(GIVEN, 0)))
        );
        assert_eq!(Socket::write(&pings, &ping, to(NEAR)), Ok(ping.len()));
        assert!(stack.told().is_some());
    }
}
