//! The groups of IPv6 a socket is a member of.
//!
//! As with IPv4's ([`super::groups`]): a program asks to hear what is
//! sent to a group's address (`ff00::/8`) with `IPV6_JOIN_GROUP`
//! (`IPV6_ADD_MEMBERSHIP` is the same option), and stops with
//! `IPV6_LEAVE_GROUP` or by closing the socket. A socket of datagrams of
//! `AF_INET6` may, and one that speaks ICMPv6, which is how a program
//! hears what the routers or the neighbours are told. Which sockets
//! asked is kept here, and the stack is told of the machine, about no
//! socket:
//!
//! ```text
//! JOIN6   (kernel to stack)  16  [u8; 16] group
//! LEAVE6  (kernel to stack)  16  [u8; 16] group
//! ```
//!
//! A `JOIN6` with the first socket that becomes a member of a group, a
//! `LEAVE6` with the last that is no more; a stack that comes is told of
//! each. The stack says it on the network (MLD) and takes what arrives
//! for the group. What arrives for a group the machine is no member of is
//! dropped there, as Linux drops it: before this a datagram for any group
//! reached the socket that had its port.
//!
//! What arrives for a group arrives as any datagram does, for the socket
//! that has the port, whether that socket asked for the group or another
//! did (`IPV6_MULTICAST_ALL`, on unless a program says otherwise).
//!
//! A datagram sent to a group needs no membership. When the machine is a
//! member of the group the socket here that has the port gets its copy,
//! from the card's address, unless the sender said not to
//! (`IPV6_MULTICAST_LOOP`): the card does not hear what it sends.
//!
//! The interface a membership names is the stack's card: by no index, or
//! by the index of an interface that is not the loopback. Anything else
//! is `ENODEV` to join on, and has no membership to leave
//! (`EADDRNOTAVAIL`). Not kept here: a membership by source
//! (`MCAST_JOIN_SOURCE_GROUP`), the same asked without saying the family
//! (`MCAST_JOIN_GROUP`), a group of the loopback's own, and the copy of
//! what a socket of ICMPv6 sends to a group.

use super::*;

/// The operations of a message that are about a group of IPv6.
pub const OP_JOIN6: u32 = 24;
pub const OP_LEAVE6: u32 = 25;

/// The groups of IPv6 one socket may be a member of. Linux has no number
/// for it: each membership is paid from the memory a socket may spend on
/// its options, and this many fit there.
pub const MEMBERSHIPS6_MAX: usize = 64;

use super::icmp6::{IPV6_JOIN_GROUP, IPV6_LEAVE_GROUP};
/// What a `struct ipv6_mreq` takes: the group and an index.
const MREQ_LEN: usize = 20;

/// The group of every machine on the link: each is a member without
/// asking.
const EVERYBODY: Ipv6Address = Ipv6Address::LINK_LOCAL_ALL_NODES;

/// `JOIN6`: the machine is a member of `group` from now on.
pub fn join6_message(group: [u8; 16]) -> Vec<u8> {
    let mut m = header(OP_JOIN6, 0, 16);
    m.extend_from_slice(&group);
    m
}

/// `LEAVE6`: the machine is a member of `group` no more.
pub fn leave6_message(group: [u8; 16]) -> Vec<u8> {
    let mut m = header(OP_LEAVE6, 0, 16);
    m.extend_from_slice(&group);
    m
}

/// What `IPV6_JOIN_GROUP` and `IPV6_LEAVE_GROUP` are given: the group and
/// the index of the interface to be a member on, or 0 for whichever goes
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Membership6 {
    pub group: Ipv6Address,
    pub index: u32,
}

impl Membership6 {
    /// Read a `struct ipv6_mreq`. Shorter is `EINVAL`
    /// (`do_ipv6_setsockopt`).
    pub fn parse(data: &[u8]) -> LxResult<Membership6> {
        let data = data.get(..MREQ_LEN).ok_or(LxError::EINVAL)?;
        let (group, index) = data.split_at(16);
        Ok(Membership6 {
            group: Ipv6Address::from_bytes(group),
            index: u32::from_ne_bytes(index.try_into().map_err(|_| LxError::EINVAL)?),
        })
    }
}

impl StackLink {
    /// Whether the interface with the index `index` is the stack's card:
    /// none said, or one that is not the loopback.
    fn is_the_card6(index: u32) -> bool {
        index == 0
            || crate::net::iface_by_linux_ifindex(index).is_ok_and(|iface| !iface.is_loopback())
    }

    /// A socket became a member of `group`: the stack is told when it is
    /// the first, and the next stack when it comes.
    fn join_group6(&self, group: Ipv6Address) {
        let _telling = self.telling.lock();
        let first = {
            let mut table = self.table.lock();
            let members = table.groups6.entry(group).or_insert(0);
            *members += 1;
            *members == 1
        };
        if first {
            let _ = self.tell(join6_message(group.0), true);
        }
    }

    /// A socket is a member of `group` no more: the stack is told when
    /// it was the last.
    fn leave_group6(&self, group: Ipv6Address) {
        let _telling = self.telling.lock();
        let last = {
            let mut table = self.table.lock();
            match table.groups6.get_mut(&group) {
                Some(members) if *members > 1 => {
                    *members -= 1;
                    false
                }
                Some(_) => {
                    table.groups6.remove(&group);
                    true
                }
                None => false,
            }
        };
        if last {
            let _ = self.tell(leave6_message(group.0), true);
        }
    }

    /// The groups of IPv6 the machine is a member of: what a stack that
    /// comes is told.
    pub(super) fn groups6(&self) -> Vec<Ipv6Address> {
        self.table.lock().groups6.keys().copied().collect()
    }

    /// Whether the machine is a member of `group`: some socket asked, or
    /// it is everybody's.
    fn is_member6(&self, group: &Ipv6Address) -> bool {
        *group == EVERYBODY || self.table.lock().groups6.contains_key(group)
    }

    /// `setsockopt` at IPv6's level of the socket `shared` is the queue
    /// of, when the option is `IPV6_JOIN_GROUP` or `IPV6_LEAVE_GROUP`,
    /// answered as `ipv6_sock_mc_join` and `ipv6_sock_mc_drop` answer.
    pub(super) fn membership6(
        &self,
        shared: &Shared,
        opt: usize,
        data: &[u8],
    ) -> Option<SysResult> {
        if !matches!(opt, IPV6_JOIN_GROUP | IPV6_LEAVE_GROUP) {
            return None;
        }
        let join = opt == IPV6_JOIN_GROUP;
        Some(self.be_member6(shared, join, data).map(|()| 0))
    }

    fn be_member6(&self, shared: &Shared, join: bool, data: &[u8]) -> LxResult<()> {
        let asked = Membership6::parse(data)?;
        if !asked.group.is_multicast() {
            return Err(LxError::EINVAL);
        }
        // The socket's lock is let go before the link's are taken.
        {
            let mut inner = shared.inner.lock();
            let at = inner.groups6.iter().position(|group| *group == asked.group);
            match (join, at) {
                (true, Some(_)) => return Err(LxError::EADDRINUSE),
                (true, None) if !Self::is_the_card6(asked.index) => return Err(LxError::ENODEV),
                (true, None) if inner.groups6.len() >= MEMBERSHIPS6_MAX => {
                    return Err(LxError::ENOMEM)
                }
                (true, None) => inner.groups6.push(asked.group),
                // Every membership here is on the card: one asked to be
                // left on another interface is none of them.
                (false, Some(at)) if Self::is_the_card6(asked.index) => {
                    inner.groups6.swap_remove(at);
                }
                (false, _) => return Err(LxError::EADDRNOTAVAIL),
            }
        }
        if join {
            self.join_group6(asked.group);
        } else {
            self.leave_group6(asked.group);
        }
        Ok(())
    }

    /// The socket `shared` is the queue of is closing: it is a member of
    /// nothing.
    pub(super) fn leave_every_group6(&self, shared: &Shared) {
        let groups = core::mem::take(&mut shared.inner.lock().groups6);
        for group in groups {
            self.leave_group6(group);
        }
    }

    /// The address of the card's a datagram for `group` leaves from: the
    /// link's own for a group of the link, and otherwise one that goes
    /// further when the card has one.
    fn source_for6(&self, group: &Ipv6Address) -> Option<Ipv6Address> {
        let addresses = self.sixes.addresses();
        let of_link = |cidr: &&Ipv6Cidr| cidr.address().is_link_local();
        let on_link = group.as_bytes()[1] & 0x0f <= 2;
        let fitting = addresses.iter().find(|cidr| of_link(cidr) == on_link);
        fitting.or(addresses.first()).map(Ipv6Cidr::address)
    }
}

impl ServedUdpSocket {
    /// A datagram of this socket's, from its port `port`, went to the
    /// port `to` of `group`: the socket of this machine that has that
    /// port hears it too, when the machine is a member and the sender did
    /// not say otherwise (`IPV6_MULTICAST_LOOP`).
    pub(super) fn loop_back6(&self, port: u16, group: Ipv6Address, to: u16, data: &[u8]) {
        if !self.six.lock().loops() || !self.link.is_member6(&group) {
            return;
        }
        if let Some(from) = self.link.source_for6(&group) {
            let from = IpEndpoint::new(IpAddress::Ipv6(from), port);
            self.link.send_here6(from, to, data);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::super::tests::{at6, ip, stack, stack_of, Stack};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    const GROUP: &str = "ff15::1234";
    const OTHER: &str = "ff15::4321";
    const ON_LINK: &str = "ff02::1:3";
    const EVERYBODY: &str = "ff02::1";
    const IPPROTO_IPV6: usize = 41;
    const IPV6_MULTICAST_LOOP: usize = 19;

    fn six(addr: &str) -> Ipv6Address {
        match at6(addr, 0).addr {
            IpAddress::Ipv6(addr) => addr,
            _ => unreachable!(),
        }
    }

    fn socket(link: &Arc<StackLink>) -> ServedUdpSocket {
        let s = ServedUdpSocket::new(link.clone(), true).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// A `struct ipv6_mreq`.
    fn mreq(group: &str, index: i32) -> Vec<u8> {
        let mut mreq = six(group).0.to_vec();
        mreq.extend_from_slice(&index.to_ne_bytes());
        mreq
    }

    fn add(s: &dyn Socket, group: &str) -> SysResult {
        s.setsockopt(IPPROTO_IPV6, IPV6_JOIN_GROUP, &mreq(group, 0))
    }

    fn drop_group(s: &dyn Socket, group: &str) -> SysResult {
        s.setsockopt(IPPROTO_IPV6, IPV6_LEAVE_GROUP, &mreq(group, 0))
    }

    fn join(group: &str) -> Vec<u8> {
        join6_message(six(group).0)
    }

    fn leave(group: &str) -> Vec<u8> {
        leave6_message(six(group).0)
    }

    fn to(addr: &str, port: u16) -> Option<Endpoint> {
        Some(Endpoint::Ip(at6(addr, port)))
    }

    fn recv(s: &ServedUdpSocket) -> (SysResult, IpEndpoint, Vec<u8>) {
        let mut buf = [0u8; 16];
        let (n, from) = async_std::task::block_on(Socket::read(s, &mut buf));
        let got = buf[..*n.as_ref().unwrap_or(&0)].to_vec();
        (n, ip(from), got)
    }

    fn port(s: &ServedUdpSocket) -> u16 {
        ip(Socket::endpoint(s).unwrap()).port
    }

    /// The card has an address of the link and one that goes further.
    fn addressed(stack: &Stack) {
        let cards = [
            Ipv6Cidr::new(six("fe80::7"), 64),
            Ipv6Cidr::new(six("fec0::7"), 64),
        ];
        stack.say(six_message(None, &cards));
    }

    #[test]
    fn the_messages_about_a_group_of_ipv6_are_laid_out_as_the_stack_reads_them() {
        // The same bytes `tools/eclipse-netstack/src/socks/mld.rs` reads,
        // written out.
        let group = six(GROUP).0;
        let mut m = std::vec![0, 0, 0, 0, 24, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        m.extend_from_slice(&[0xff, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x12, 0x34]);
        assert_eq!(join6_message(group), m);
        m[4] = 25;
        assert_eq!(leave6_message(group), m);
        assert_eq!(parse_header(&join(GROUP)), Some((OP_JOIN6, 0)));
        assert_eq!(parse_header(&leave(GROUP)), Some((OP_LEAVE6, 0)));
        // What a program hands over.
        let asked = Membership6 {
            group: six(GROUP),
            index: 3,
        };
        assert_eq!(Membership6::parse(&mreq(GROUP, 3)), Ok(asked));
        let mut long = mreq(GROUP, 3);
        long.push(9);
        assert_eq!(Membership6::parse(&long), Ok(asked));
        assert_eq!(
            Membership6::parse(&mreq(GROUP, 3)[..19]),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn the_first_member_of_a_group_of_ipv6_joins_it_and_the_last_leaves_it() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = socket(&stack.link);
        let b = socket(&stack.link);
        assert_eq!(add(&a, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(join(GROUP)));
        // The machine is a member already.
        assert_eq!(add(&b, GROUP), Ok(0));
        assert_eq!(stack.told(), None);
        assert_eq!(add(&b, OTHER), Ok(0));
        assert_eq!(stack.told(), Some(join(OTHER)));
        // Once for each socket.
        assert_eq!(add(&a, GROUP), Err(LxError::EADDRINUSE));
        // And still one while a socket is.
        assert_eq!(drop_group(&a, GROUP), Ok(0));
        assert_eq!(stack.told(), None);
        assert_eq!(drop_group(&a, GROUP), Err(LxError::EADDRNOTAVAIL));
        assert_eq!(stack.link.groups6().len(), 2);
        // A socket that closes is a member of nothing.
        let closed = b.sock;
        drop(b);
        let mut left = [stack.told().unwrap(), stack.told().unwrap()];
        left.sort();
        let mut both = [leave(GROUP), leave(OTHER)];
        both.sort();
        assert_eq!(left, both);
        assert_eq!(stack.told(), Some(close_message(closed)));
        assert_eq!(stack.told(), None);
        assert!(stack.link.groups6().is_empty());

        // A socket that speaks ICMPv6 is a member like one of datagrams:
        // it is how a program hears what the routers are told.
        let routers = "ff02::2";
        let raw = ServedIcmpSocket::new6(stack.link.clone(), true).unwrap();
        assert_eq!(stack.told(), Some(hear6_message(raw.sock)));
        assert_eq!(add(&raw, routers), Ok(0));
        assert_eq!(stack.told(), Some(join(routers)));
        assert_eq!(add(&a, routers), Ok(0));
        assert_eq!(add(&raw, routers), Err(LxError::EADDRINUSE));
        assert_eq!(drop_group(&raw, GROUP), Err(LxError::EADDRNOTAVAIL));
        assert_eq!(stack.told(), None);
        assert_eq!(drop_group(&a, routers), Ok(0));
        drop(raw);
        let mut told = std::vec::Vec::new();
        while let Some(m) = stack.told() {
            told.push(m);
        }
        assert!(told.contains(&leave(routers)), "{:?}", told);
        assert!(stack.link.groups6().is_empty());
    }

    #[test]
    fn a_membership_of_ipv6_asked_wrong_is_refused_as_linux_refuses_it() {
        let _g = LOCK.lock();
        let stack = stack();
        let s = socket(&stack.link);
        let ask = |opt: usize, data: &[u8]| Socket::setsockopt(&s, IPPROTO_IPV6, opt, data);
        // Too short to be a `struct ipv6_mreq`.
        let short = &mreq(GROUP, 0)[..19];
        assert_eq!(ask(IPV6_JOIN_GROUP, short), Err(LxError::EINVAL));
        assert_eq!(ask(IPV6_LEAVE_GROUP, short), Err(LxError::EINVAL));
        // Not a group.
        for not in ["fec0::1", "::", "::ffff:239.1.2.3"] {
            assert_eq!(add(&s, not), Err(LxError::EINVAL));
            assert_eq!(drop_group(&s, not), Err(LxError::EINVAL));
        }
        // An index that is no interface's: nothing to join on, and
        // nothing that was joined there.
        assert_eq!(
            ask(IPV6_JOIN_GROUP, &mreq(GROUP, 999)),
            Err(LxError::ENODEV)
        );
        assert_eq!(add(&s, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(join(GROUP)));
        assert_eq!(
            ask(IPV6_LEAVE_GROUP, &mreq(GROUP, 999)),
            Err(LxError::EADDRNOTAVAIL)
        );
        assert_eq!(drop_group(&s, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(leave(GROUP)));
        // IPv6's are no options of an `AF_INET` socket.
        let four = ServedUdpSocket::new(stack.link.clone(), false).unwrap();
        assert_eq!(add(&four, GROUP), Err(LxError::ENOPROTOOPT));
        // So many groups for one socket, and no more.
        for n in 0..MEMBERSHIPS6_MAX {
            assert_eq!(add(&s, &std::format!("ff15::1:{:x}", n)), Ok(0));
        }
        assert_eq!(add(&s, OTHER), Err(LxError::ENOMEM));
        assert_eq!(stack.link.groups6().len(), MEMBERSHIPS6_MAX);
        // The other options of the level are still the socket's.
        let looped = || Socket::getsockopt_own(&s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP);
        assert_eq!(looped(), Some(1i32.to_ne_bytes().to_vec()));
        assert_eq!(ask(IPV6_MULTICAST_LOOP, &0i32.to_ne_bytes()), Ok(0));
        assert_eq!(looped(), Some(0i32.to_ne_bytes().to_vec()));
        assert_eq!(ask(IPV6_MULTICAST_LOOP, &[1]), Err(LxError::EINVAL));
    }

    #[test]
    fn a_stack_that_comes_is_told_the_groups_of_ipv6_the_machine_is_a_member_of() {
        let _g = LOCK.lock();
        let link = StackLink::new();
        // Asked with no stack: the machine's all the same.
        let s = socket(&link);
        assert_eq!(add(&s, GROUP), Ok(0));
        assert_eq!(Socket::bind(&s, Endpoint::Ip(at6("::", 5000))), Ok(0));
        let stack = stack_of(&link);
        assert_eq!(stack.told(), Some(bind_message(s.sock, 5000, Takes::Both)));
        assert_eq!(stack.told(), Some(join(GROUP)));
        assert_eq!(stack.told(), None);
        // And the one after it.
        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert!(link.is_down());
        assert_eq!(drop_group(&s, GROUP), Ok(0));
        assert_eq!(add(&s, OTHER), Ok(0));
        let next = stack_of(&link);
        assert_eq!(next.told(), Some(bind_message(s.sock, 5000, Takes::Both)));
        assert_eq!(next.told(), Some(join(OTHER)));
        assert_eq!(next.told(), None);
    }

    #[test]
    fn a_datagram_for_a_group_of_ipv6_goes_out_and_to_the_member_in_this_machine() {
        let _g = LOCK.lock();
        let stack = stack();
        addressed(&stack);
        let member = socket(&stack.link);
        assert_eq!(Socket::bind(&member, Endpoint::Ip(at6("::", 5000))), Ok(0));
        assert!(stack.told().is_some());
        let sender = socket(&stack.link);
        assert_eq!(Socket::bind(&sender, Endpoint::Ip(at6("::", 6000))), Ok(0));
        assert!(stack.told().is_some());
        let out =
            |sock: u64, group: &str, data: &[u8]| send6_message(sock, six(group).0, 5000, 1, data);

        // Nobody here is a member: out, to stay on the link, and that
        // is all.
        assert_eq!(Socket::write(&sender, b"one", to(GROUP, 5000)), Ok(3));
        assert_eq!(stack.told(), Some(out(sender.sock, GROUP, b"one")));
        assert_eq!(recv(&member).0, Err(LxError::EAGAIN));

        // A member: out, and here, from the card's address that goes as
        // far as the group does.
        assert_eq!(add(&member, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(join(GROUP)));
        assert_eq!(Socket::write(&sender, b"two", to(GROUP, 5000)), Ok(3));
        assert_eq!(stack.told(), Some(out(sender.sock, GROUP, b"two")));
        assert_eq!(
            recv(&member),
            (Ok(3), at6("fec0::7", 6000), b"two".to_vec())
        );
        assert_eq!(add(&member, ON_LINK), Ok(0));
        assert_eq!(stack.told(), Some(join(ON_LINK)));
        assert_eq!(Socket::write(&sender, b"near", to(ON_LINK, 5000)), Ok(4));
        assert_eq!(stack.told(), Some(out(sender.sock, ON_LINK, b"near")));
        assert_eq!(
            recv(&member),
            (Ok(4), at6("fe80::7", 6000), b"near".to_vec())
        );
        // For the port, as what the card brings: another port's is not
        // this socket's, and another group is not the machine's.
        assert_eq!(Socket::write(&sender, b"x", to(GROUP, 5001)), Ok(1));
        assert_eq!(Socket::write(&sender, b"x", to(OTHER, 5000)), Ok(1));
        assert_eq!(recv(&member).0, Err(LxError::EAGAIN));
        assert!(stack.told().is_some() && stack.told().is_some());
        // Who sends hears itself when the port is its own.
        assert_eq!(Socket::write(&member, b"me", to(GROUP, 5000)), Ok(2));
        assert_eq!(stack.told(), Some(out(member.sock, GROUP, b"me")));
        assert_eq!(recv(&member), (Ok(2), at6("fec0::7", 5000), b"me".to_vec()));

        // Unless the sender says not here (`IPV6_MULTICAST_LOOP`).
        let quiet = 0i32.to_ne_bytes();
        assert_eq!(
            Socket::setsockopt(&sender, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &quiet),
            Ok(0)
        );
        assert_eq!(Socket::write(&sender, b"out", to(GROUP, 5000)), Ok(3));
        assert_eq!(stack.told(), Some(out(sender.sock, GROUP, b"out")));
        assert_eq!(recv(&member).0, Err(LxError::EAGAIN));

        // Everybody's group has every machine, this one too.
        let all = socket(&stack.link);
        assert_eq!(Socket::write(&all, b"all", to(EVERYBODY, 5000)), Ok(3));
        assert_eq!(
            recv(&member),
            (Ok(3), at6("fe80::7", port(&all)), b"all".to_vec())
        );
        // A socket of `AF_INET` with the port is not asked: what goes to
        // a group of IPv6 is not its own.
        drop(member);
        let four = ServedUdpSocket::new(stack.link.clone(), false).unwrap();
        FileLike::set_flags(&four, OpenFlags::NON_BLOCK).unwrap();
        let any4 = Endpoint::Ip(IpEndpoint::new(IpAddress::Ipv4(Ipv4Address([0; 4])), 5000));
        assert_eq!(Socket::bind(&four, any4), Ok(0));
        assert_eq!(Socket::write(&all, b"all", to(EVERYBODY, 5000)), Ok(3));
        let mut buf = [0u8; 4];
        let (n, _) = async_std::task::block_on(Socket::read(&four, &mut buf));
        assert_eq!(n, Err(LxError::EAGAIN));
    }
}
