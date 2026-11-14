//! The groups a datagram socket is a member of: multicast.
//!
//! A program asks to hear what is sent to a group's address
//! (224.0.0.0/4) with `IP_ADD_MEMBERSHIP`, and stops with
//! `IP_DROP_MEMBERSHIP` or by closing the socket. Which sockets asked is
//! kept here, as every option of a socket is. What the stack has to know
//! is whether the machine is a member, to say so on the network and to
//! take what arrives for the group, and it is told that, about no socket
//! (the header's socket is zero):
//!
//! ```text
//! JOIN    (kernel to stack)  16  [u8; 4] group
//! LEAVE   (kernel to stack)  16  [u8; 4] group
//! ```
//!
//! A `JOIN` with the first socket that becomes a member of a group, a
//! `LEAVE` with the last that is no more. The groups are the machine's,
//! like the ports: asked with no stack they wait, and a stack that comes
//! is told of each.
//!
//! What arrives for a group arrives as any datagram does, for the socket
//! that has the port it was sent to: whether that socket asked for the
//! group or another did is not looked at, as Linux does not look by
//! itself (`IP_MULTICAST_ALL`, on unless a program says otherwise).
//!
//! A datagram sent to a group needs no membership. It goes to the stack,
//! to travel `IP_MULTICAST_TTL` hops (1 unless said), and when the
//! machine is a member of the group the socket here that has the port
//! gets its copy, from the stack's address, unless the sender said not to
//! (`IP_MULTICAST_LOOP`): the card does not hear what it sends.
//!
//! The interface a membership names is the stack's card, which is the
//! only one these sockets have beyond the machine: by no address and no
//! index, by the stack's address, or by the index of an interface that is
//! not the loopback. Anything else is `ENODEV` to join on, and has no
//! membership to drop (`EADDRNOTAVAIL`). Not kept here: a
//! membership by source (`IP_ADD_SOURCE_MEMBERSHIP`), the same asked
//! without saying the family (`MCAST_JOIN_GROUP`), and a group of the
//! loopback's own.

use super::*;

/// The operations of a message that are about a group.
pub const OP_JOIN: u32 = 14;
pub const OP_LEAVE: u32 = 15;

/// The groups one socket may be a member of: Linux's
/// `igmp_max_memberships`.
pub const MEMBERSHIPS_MAX: usize = 20;

const IPPROTO_IP: usize = 0;
const IP_ADD_MEMBERSHIP: usize = 35;
const IP_DROP_MEMBERSHIP: usize = 36;

/// The group of every machine on the network: each is a member without
/// asking.
const EVERYBODY: Ipv4Address = Ipv4Address::MULTICAST_ALL_SYSTEMS;

/// `JOIN`: the machine is a member of `group` from now on.
pub fn join_message(group: [u8; 4]) -> Vec<u8> {
    let mut m = header(OP_JOIN, 0, 4);
    m.extend_from_slice(&group);
    m
}

/// `LEAVE`: the machine is a member of `group` no more.
pub fn leave_message(group: [u8; 4]) -> Vec<u8> {
    let mut m = header(OP_LEAVE, 0, 4);
    m.extend_from_slice(&group);
    m
}

/// What `IP_ADD_MEMBERSHIP` and `IP_DROP_MEMBERSHIP` are given: the group
/// and the interface to be a member on, by its address or, in the longer
/// form, by its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Membership {
    pub group: Ipv4Address,
    pub interface: Ipv4Address,
    pub index: u32,
}

impl Membership {
    /// Read a `struct ip_mreq` (two addresses) or a `struct ip_mreqn`
    /// (and an index), as `do_ip_setsockopt` tells them apart: by the
    /// length. Shorter than the first is `EINVAL`.
    pub fn parse(data: &[u8]) -> LxResult<Membership> {
        let addr = |at: usize| -> Option<Ipv4Address> {
            let bytes: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
            Some(Ipv4Address(bytes))
        };
        let group = addr(0).ok_or(LxError::EINVAL)?;
        let interface = addr(4).ok_or(LxError::EINVAL)?;
        let index = data.get(8..12).and_then(|index| index.try_into().ok());
        Ok(Membership {
            group,
            interface,
            index: index.map_or(0, u32::from_ne_bytes),
        })
    }
}

impl StackLink {
    /// Whether the interface `asked` names is the stack's card.
    fn is_the_card(&self, asked: &Membership) -> bool {
        if asked.index != 0 {
            // `ip_mc_find_dev`: an index is looked at before an address.
            return crate::net::iface_by_linux_ifindex(asked.index)
                .is_ok_and(|iface| !iface.is_loopback());
        }
        asked.interface.is_unspecified() || asked.interface == self.address()
    }

    /// A socket became a member of `group`: the stack is told when it is
    /// the first, and the next stack when it comes.
    fn join_group(&self, group: Ipv4Address) {
        let _telling = self.telling.lock();
        let first = {
            let mut table = self.table.lock();
            let members = table.groups.entry(group).or_insert(0);
            *members += 1;
            *members == 1
        };
        if first {
            let _ = self.tell(join_message(group.0), true);
        }
    }

    /// A socket is a member of `group` no more: the stack is told when
    /// it was the last.
    fn leave_group(&self, group: Ipv4Address) {
        let _telling = self.telling.lock();
        let last = {
            let mut table = self.table.lock();
            match table.groups.get_mut(&group) {
                Some(members) if *members > 1 => {
                    *members -= 1;
                    false
                }
                Some(_) => {
                    table.groups.remove(&group);
                    true
                }
                None => false,
            }
        };
        if last {
            let _ = self.tell(leave_message(group.0), true);
        }
    }

    /// The groups the machine is a member of: what a stack that comes
    /// is told.
    pub(super) fn groups(&self) -> Vec<Ipv4Address> {
        self.table.lock().groups.keys().copied().collect()
    }

    /// Whether the machine is a member of `group`: some socket asked, or
    /// it is everybody's.
    fn is_member(&self, group: Ipv4Address) -> bool {
        group == EVERYBODY || self.table.lock().groups.contains_key(&group)
    }
}

impl ServedUdpSocket {
    /// `setsockopt`, when the option is `IP_ADD_MEMBERSHIP` or
    /// `IP_DROP_MEMBERSHIP`, answered as `ip_mc_join_group` and
    /// `ip_mc_leave_group` answer.
    pub(super) fn membership(&self, level: usize, opt: usize, data: &[u8]) -> Option<SysResult> {
        if level != IPPROTO_IP || !matches!(opt, IP_ADD_MEMBERSHIP | IP_DROP_MEMBERSHIP) {
            return None;
        }
        let join = opt == IP_ADD_MEMBERSHIP;
        Some(self.be_member(join, data).map(|()| 0))
    }

    fn be_member(&self, join: bool, data: &[u8]) -> LxResult<()> {
        let asked = Membership::parse(data)?;
        if join && !asked.group.is_multicast() {
            return Err(LxError::EINVAL);
        }
        if !self.link.is_the_card(&asked) {
            // Nothing to join on, and nothing that was joined there.
            return Err(if join {
                LxError::ENODEV
            } else {
                LxError::EADDRNOTAVAIL
            });
        }
        // The socket's lock is let go before the link's are taken.
        {
            let mut inner = self.shared.inner.lock();
            let at = inner.groups.iter().position(|group| *group == asked.group);
            match (join, at) {
                (true, Some(_)) => return Err(LxError::EADDRINUSE),
                (true, None) if inner.groups.len() >= MEMBERSHIPS_MAX => {
                    return Err(LxError::ENOBUFS)
                }
                (true, None) => inner.groups.push(asked.group),
                (false, Some(at)) => {
                    inner.groups.swap_remove(at);
                }
                (false, None) => return Err(LxError::EADDRNOTAVAIL),
            }
        }
        if join {
            self.link.join_group(asked.group);
        } else {
            self.link.leave_group(asked.group);
        }
        Ok(())
    }

    /// The socket is closing: it is a member of nothing.
    pub(super) fn leave_every_group(&self) {
        let groups = core::mem::take(&mut self.shared.inner.lock().groups);
        for group in groups {
            self.link.leave_group(group);
        }
    }

    /// A datagram of this socket's, from its port `port`, went to the
    /// group `to`: the socket of this machine that has the port it was
    /// for hears it too, when the machine is a member and the sender did
    /// not say otherwise (`IP_MULTICAST_LOOP`).
    pub(super) fn loop_back(&self, port: u16, to: IpEndpoint, data: &[u8]) {
        let IpAddress::Ipv4(group) = to.addr else {
            return;
        };
        if !self.opts.lock().mcast_loop || !self.link.is_member(group) {
            return;
        }
        let from = IpEndpoint::new(IpAddress::Ipv4(self.link.address()), port);
        self.link.send_here(from, group, to.port, data, false);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::super::tests::{any, at, ip, stack, stack_of, Stack, HOST, STACK_ADDR};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    const GROUP: [u8; 4] = [239, 1, 2, 3];
    const OTHER: [u8; 4] = [239, 9, 9, 9];
    const EVERYBODY: [u8; 4] = [224, 0, 0, 1];

    fn socket(link: &Arc<StackLink>) -> ServedUdpSocket {
        let s = ServedUdpSocket::new(link.clone(), false).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// A `struct ip_mreq`.
    fn mreq(group: [u8; 4], interface: [u8; 4]) -> Vec<u8> {
        let mut mreq = group.to_vec();
        mreq.extend_from_slice(&interface);
        mreq
    }

    fn add(s: &ServedUdpSocket, group: [u8; 4]) -> SysResult {
        Socket::setsockopt(s, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mreq(group, [0; 4]))
    }

    fn drop_group(s: &ServedUdpSocket, group: [u8; 4]) -> SysResult {
        Socket::setsockopt(s, IPPROTO_IP, IP_DROP_MEMBERSHIP, &mreq(group, [0; 4]))
    }

    fn to(addr: [u8; 4], port: u16) -> Option<Endpoint> {
        Some(Endpoint::Ip(at(addr, port)))
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

    #[test]
    fn the_messages_about_a_group_are_laid_out_as_the_stack_reads_them() {
        // The same bytes `tools/eclipse-netstack/src/socks/groups.rs`
        // reads, written out.
        assert_eq!(
            join_message(GROUP),
            [0, 0, 0, 0, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 239, 1, 2, 3]
        );
        assert_eq!(
            leave_message(GROUP),
            [0, 0, 0, 0, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 239, 1, 2, 3]
        );
        assert_eq!(parse_header(&join_message(GROUP)), Some((OP_JOIN, 0)));
        assert_eq!(parse_header(&leave_message(GROUP)), Some((OP_LEAVE, 0)));
        // What a program hands over, in either form.
        assert_eq!(
            Membership::parse(&mreq(GROUP, STACK_ADDR)),
            Ok(Membership {
                group: Ipv4Address(GROUP),
                interface: Ipv4Address(STACK_ADDR),
                index: 0,
            })
        );
        let mut mreqn = mreq(GROUP, [0; 4]);
        mreqn.extend_from_slice(&3i32.to_ne_bytes());
        assert_eq!(Membership::parse(&mreqn).map(|asked| asked.index), Ok(3));
        // A few bytes more than the short form are the short form.
        assert_eq!(
            Membership::parse(&mreqn[..11]).map(|asked| asked.index),
            Ok(0)
        );
        assert_eq!(Membership::parse(&mreqn[..7]), Err(LxError::EINVAL));
    }

    #[test]
    fn the_first_member_of_a_group_joins_it_and_the_last_leaves_it() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = socket(&stack.link);
        let b = socket(&stack.link);
        assert_eq!(add(&a, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(join_message(GROUP)));
        // The machine is a member already.
        assert_eq!(add(&b, GROUP), Ok(0));
        assert_eq!(stack.told(), None);
        assert_eq!(add(&b, OTHER), Ok(0));
        assert_eq!(stack.told(), Some(join_message(OTHER)));
        // Once for each socket.
        assert_eq!(add(&a, GROUP), Err(LxError::EADDRINUSE));
        // And still one while a socket is.
        assert_eq!(drop_group(&a, GROUP), Ok(0));
        assert_eq!(stack.told(), None);
        assert_eq!(drop_group(&a, GROUP), Err(LxError::EADDRNOTAVAIL));
        assert_eq!(stack.link.groups().len(), 2);
        // A socket that closes is a member of nothing.
        let closed = b.sock;
        drop(b);
        let mut left = [stack.told().unwrap(), stack.told().unwrap()];
        left.sort();
        let mut both = [leave_message(GROUP), leave_message(OTHER)];
        both.sort();
        assert_eq!(left, both);
        assert_eq!(stack.told(), Some(close_message(closed)));
        assert_eq!(stack.told(), None);
        assert!(stack.link.groups().is_empty());
        // And can be one again.
        assert_eq!(add(&a, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(join_message(GROUP)));
    }

    #[test]
    fn a_membership_asked_wrong_is_refused_as_linux_refuses_it() {
        let _g = LOCK.lock();
        let stack = stack();
        let s = socket(&stack.link);
        let ask = |opt: usize, data: &[u8]| Socket::setsockopt(&s, IPPROTO_IP, opt, data);
        // Too short to be a `struct ip_mreq`.
        let short = &mreq(GROUP, [0; 4])[..7];
        assert_eq!(ask(IP_ADD_MEMBERSHIP, short), Err(LxError::EINVAL));
        assert_eq!(ask(IP_DROP_MEMBERSHIP, short), Err(LxError::EINVAL));
        // Not a group.
        for not in [HOST, [0; 4], [255; 4]] {
            assert_eq!(add(&s, not), Err(LxError::EINVAL));
            assert_eq!(drop_group(&s, not), Err(LxError::EADDRNOTAVAIL));
        }
        // On the stack's card, by its address or by none; no other
        // interface is these sockets'.
        assert_eq!(ask(IP_ADD_MEMBERSHIP, &mreq(GROUP, STACK_ADDR)), Ok(0));
        assert_eq!(stack.told(), Some(join_message(GROUP)));
        for elsewhere in [HOST, [127, 0, 0, 1]] {
            assert_eq!(
                ask(IP_ADD_MEMBERSHIP, &mreq(OTHER, elsewhere)),
                Err(LxError::ENODEV)
            );
            assert_eq!(
                ask(IP_DROP_MEMBERSHIP, &mreq(GROUP, elsewhere)),
                Err(LxError::EADDRNOTAVAIL)
            );
        }
        // An index that is no interface's.
        let mut mreqn = mreq(OTHER, [0; 4]);
        mreqn.extend_from_slice(&999i32.to_ne_bytes());
        assert_eq!(ask(IP_ADD_MEMBERSHIP, &mreqn), Err(LxError::ENODEV));
        assert_eq!(drop_group(&s, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(leave_message(GROUP)));
        // So many groups for one socket, and no more.
        for n in 0..MEMBERSHIPS_MAX as u8 {
            assert_eq!(add(&s, [239, 0, 0, n]), Ok(0));
        }
        assert_eq!(add(&s, OTHER), Err(LxError::ENOBUFS));
        assert_eq!(stack.link.groups().len(), MEMBERSHIPS_MAX);
        // The other options of the level are still the socket's.
        assert_eq!(ask(33, &[5]), Ok(0));
        assert_eq!(Socket::ip_multicast_ttl(&s), 5);
    }

    #[test]
    fn a_stack_that_comes_is_told_the_groups_the_machine_is_a_member_of() {
        let _g = LOCK.lock();
        let link = StackLink::new();
        // Asked with no stack: the machine's all the same.
        let s = socket(&link);
        assert_eq!(add(&s, GROUP), Ok(0));
        assert_eq!(Socket::bind(&s, any(5000)), Ok(0));
        let stack = stack_of(&link);
        assert_eq!(stack.told(), Some(bind_message(s.sock, 5000, Takes::Four)));
        assert_eq!(stack.told(), Some(join_message(GROUP)));
        assert_eq!(stack.told(), None);
        // And the one after it.
        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert!(link.is_down());
        assert_eq!(drop_group(&s, GROUP), Ok(0));
        assert_eq!(add(&s, OTHER), Ok(0));
        let next = stack_of(&link);
        assert_eq!(next.told(), Some(bind_message(s.sock, 5000, Takes::Four)));
        assert_eq!(next.told(), Some(join_message(OTHER)));
        assert_eq!(next.told(), None);
        // What arrives for the group arrives for the port.
        next.arrives(s.sock, HOST, 4000, b"hello");
        assert_eq!(recv(&s), (Ok(5), at(HOST, 4000), b"hello".to_vec()));
    }

    #[test]
    fn a_datagram_for_a_group_goes_out_and_to_the_member_in_this_machine() {
        let _g = LOCK.lock();
        let stack = stack();
        let member = socket(&stack.link);
        assert_eq!(Socket::bind(&member, any(5000)), Ok(0));
        assert_eq!(
            stack.told(),
            Some(bind_message(member.sock, 5000, Takes::Four))
        );
        let sender = socket(&stack.link);
        assert_eq!(Socket::bind(&sender, any(6000)), Ok(0));
        assert_eq!(
            stack.told(),
            Some(bind_message(sender.sock, 6000, Takes::Four))
        );

        // Nobody here is a member: out, one hop, and that is all.
        assert_eq!(Socket::write(&sender, b"one", to(GROUP, 5000)), Ok(3));
        let out = |data: &[u8]| send_message(sender.sock, GROUP, 5000, 1, data);
        assert_eq!(stack.told(), Some(out(b"one")));
        assert_eq!(recv(&member).0, Err(LxError::EAGAIN));

        // A member: out, and here, from the stack's address.
        assert_eq!(add(&member, GROUP), Ok(0));
        assert_eq!(stack.told(), Some(join_message(GROUP)));
        assert_eq!(Socket::write(&sender, b"two", to(GROUP, 5000)), Ok(3));
        assert_eq!(stack.told(), Some(out(b"two")));
        assert_eq!(
            recv(&member),
            (Ok(3), at(STACK_ADDR, 6000), b"two".to_vec())
        );
        // For the port, as what the card brings: another port's is not
        // this socket's, and another group is not the machine's.
        assert_eq!(Socket::write(&sender, b"x", to(GROUP, 5001)), Ok(1));
        assert_eq!(Socket::write(&sender, b"x", to(OTHER, 5000)), Ok(1));
        assert_eq!(recv(&member).0, Err(LxError::EAGAIN));
        assert!(stack.told().is_some() && stack.told().is_some());
        // Who sends hears itself when the port is its own.
        assert_eq!(Socket::write(&member, b"me", to(GROUP, 5000)), Ok(2));
        assert_eq!(
            stack.told(),
            Some(send_message(member.sock, GROUP, 5000, 1, b"me"))
        );
        assert_eq!(recv(&member), (Ok(2), at(STACK_ADDR, 5000), b"me".to_vec()));

        // Unless the sender says not here (`IP_MULTICAST_LOOP`), and as
        // far as it says (`IP_MULTICAST_TTL`).
        assert_eq!(Socket::setsockopt(&sender, IPPROTO_IP, 34, &[0]), Ok(0));
        assert_eq!(Socket::setsockopt(&sender, IPPROTO_IP, 33, &[7]), Ok(0));
        assert_eq!(Socket::write(&sender, b"out", to(GROUP, 5000)), Ok(3));
        assert_eq!(
            stack.told(),
            Some(send_message(sender.sock, GROUP, 5000, 7, b"out"))
        );
        assert_eq!(recv(&member).0, Err(LxError::EAGAIN));

        // Everybody's group has every machine, this one too.
        let all = socket(&stack.link);
        assert_eq!(Socket::write(&all, b"all", to(EVERYBODY, 5000)), Ok(3));
        assert_eq!(
            recv(&member),
            (Ok(3), at(STACK_ADDR, port(&all)), b"all".to_vec())
        );

        // With no stack nothing goes to a group, here either.
        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert_eq!(
            Socket::write(&sender, b"x", to(GROUP, 5000)),
            Err(LxError::ENETDOWN)
        );
        assert_eq!(Socket::setsockopt(&sender, IPPROTO_IP, 34, &[1]), Ok(0));
        assert_eq!(
            Socket::write(&sender, b"x", to(GROUP, 5000)),
            Err(LxError::ENETDOWN)
        );
        assert_eq!(recv(&member).0, Err(LxError::EAGAIN));
    }
}
