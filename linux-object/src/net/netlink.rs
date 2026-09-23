// udpsocket

use super::socket_address::*;
use crate::fs::{OpenFlags, PollEvents, PollStatus};
use crate::{
    error::{LxError, LxResult},
    fs::FileLike,
    net::{
        AddressFamily, Endpoint, Socket, SysResult, ARPHRD_ETHER, ARPHRD_LOOPBACK, IFF_BROADCAST,
        IFF_CHANGE_ALL, IFF_LOOPBACK, IFF_LOWER_UP, IFF_MULTICAST, IFF_NOARP, IFF_RUNNING, IFF_UP,
    },
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use async_trait::async_trait;
use bitflags::bitflags;
use core::{mem::size_of, slice};
use kernel_hal::net::get_net_device;
use kernel_hal::thread;
use lock::Mutex;
use smoltcp::wire::IpCidr;
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};
use zcore_drivers::scheme::RouteInfo;

/// Bound queued netlink replies (unread sockets must not grow without limit).
const NETLINK_RX_QUEUE_MAX: usize = 64;

fn push_netlink_rx(queue: &mut Vec<Vec<u8>>, msg: Vec<u8>) {
    if queue.len() >= NETLINK_RX_QUEUE_MAX {
        queue.remove(0);
    }
    queue.push(msg);
}

// Needed by `impl_kobject!`
#[allow(unused_imports)]
use zircon_object::object::*;

pub struct NetlinkSocketState {
    base: zircon_object::object::KObjectBase,
    data: Arc<Mutex<Vec<Vec<u8>>>>,
    local_endpoint: Arc<Mutex<Option<NetlinkEndpoint>>>,
    flags: Arc<Mutex<OpenFlags>>,
}

impl Default for NetlinkSocketState {
    fn default() -> Self {
        Self {
            base: zircon_object::object::KObjectBase::new(),
            data: Arc::new(Mutex::new(Vec::new())),
            local_endpoint: Arc::new(Mutex::new(None)),
            flags: Arc::new(Mutex::new(OpenFlags::RDWR)),
        }
    }
}
impl NetlinkSocketState {
    fn auto_port_id(&self) -> u32 {
        let reduced = self.base.id % u32::MAX as u64;
        (reduced as u32).max(1)
    }

    fn local_port_id(&self) -> u32 {
        self.local_endpoint
            .lock()
            .as_ref()
            .map(|e| e.port_id)
            .filter(|&p| p != 0)
            .unwrap_or_else(|| self.auto_port_id())
    }
}

/// `nlmsg_pid` echoed in dump replies must match what userland put in the request
/// (fastfetch/musl filter on this field; falls back to process pid if getsockname fails).
fn reply_nl_pid(req: &NetlinkMessageHeader, local_port: u32) -> u32 {
    if req.nlmsg_pid != 0 {
        req.nlmsg_pid
    } else {
        local_port
    }
}

#[async_trait]
impl Socket for NetlinkSocketState {
    /// missing documentation
    async fn read(&self, data: &mut [u8]) -> (LxResult<usize>, Endpoint) {
        let endpoint = Endpoint::Netlink(NetlinkEndpoint::new(0, 0));
        let non_block = self.flags.lock().contains(OpenFlags::NON_BLOCK);

        loop {
            let maybe_msg = {
                let mut buffer = self.data.lock();
                if buffer.is_empty() {
                    None
                } else {
                    let msg = buffer.remove(0);
                    // Guard the header index: a short/partial buffer would panic
                    // on `msg[4]`/`msg[5]`.
                    let msg_type = if msg.len() >= 6 {
                        u16::from_le_bytes([msg[4], msg[5]])
                    } else {
                        0
                    };
                    info!("[netlink] read: type={}, len={}", msg_type, msg.len());
                    Some(msg)
                }
            };

            match maybe_msg {
                Some(msg) => {
                    let n = core::cmp::min(msg.len(), data.len());
                    if n != 0 {
                        data[..n].copy_from_slice(&msg[..n]);
                    }
                    // Netlink is a datagram protocol: a message larger than the
                    // caller's buffer is truncated and its remainder DISCARDED
                    // (Linux would set MSG_TRUNC). Do NOT re-queue `msg[n..]` as a
                    // new message — a headerless fragment corrupts netlink framing
                    // for the next reader and can be indexed out of bounds.
                    info!("[netlink] read hex: {:?}", &msg[..n]);
                    return (Ok(n), endpoint);
                }
                None if non_block => return (Err(LxError::EAGAIN), endpoint),
                None => {
                    kernel_hal::deferred_job::drain_deferred_jobs();
                    thread::sleep_until(
                        kernel_hal::timer::timer_now() + core::time::Duration::from_millis(5),
                    )
                    .await;
                }
            }
        }
    }

    /// One `write` can carry SEVERAL netlink messages back to back — that is how
    /// `ip addr flush` deletes every address on an interface: it batches one
    /// RTM_DELADDR per address into a single send. Handling only the first left
    /// the rest in place, which is half of why a stale `0.0.0.0/0` survived a
    /// flush. Walk the batch, advancing by the NLMSG-aligned length.
    fn write(&self, data: &[u8], _sendto_endpoint: Option<Endpoint>) -> SysResult {
        if data.len() < size_of::<NetlinkMessageHeader>() {
            return Err(LxError::EINVAL);
        }
        // Cleared once per `write`, so every message in the batch appends to the
        // same reply stream — as Linux does.
        self.data.lock().clear();
        let whole = data;
        let mut offset = 0usize;
        let mut handled = 0usize;
        // `saturating_sub`, NOT `-`: NLMSG_ALIGN rounds the offset UP, so a
        // final message whose length is not a multiple of 4 leaves
        // `offset > whole.len()`. Plain subtraction wraps around in `usize` and
        // the loop spins forever in the kernel — which is exactly what a
        // `ip addr flush` did before this line was written this way.
        while whole.len().saturating_sub(offset) >= size_of::<NetlinkMessageHeader>() {
            // `read_unaligned`: the batch advances by NLMSG_ALIGN(len), so a
            // later message need not share the buffer's alignment.
            #[allow(unsafe_code)]
            let Some(probe) = read_struct::<NetlinkMessageHeader>(whole, offset) else {
                break;
            };
            let nlmsg_len = probe.nlmsg_len as usize;
            // Safe: the loop condition above guarantees `offset + 16 <= len`.
            let remaining = whole.len() - offset;
            if nlmsg_len < size_of::<NetlinkMessageHeader>() || nlmsg_len > remaining {
                // A malformed trailer does not invalidate messages already
                // applied; only a bad FIRST message is an error.
                if handled == 0 {
                    return Err(LxError::EINVAL);
                }
                break;
            }
            // Shadow `data` so the per-message body below reads exactly one
            // message, unchanged from when it handled the whole buffer.
            let data = &whole[offset..offset + nlmsg_len];
            handled += 1;
            offset = offset.saturating_add((nlmsg_len + 3) & !3);

            #[allow(unsafe_code)]
            let Some(header) = read_struct::<NetlinkMessageHeader>(data, 0) else {
                break;
            };
            let header = &header;
            let message_type = NetlinkMessageType::from(header.nlmsg_type);
            info!(
                "Netlink write: message_type={:?}, len={}, seq={}, hex: {:?}",
                message_type, header.nlmsg_len, header.nlmsg_seq, data
            );
            let local_port = self.local_port_id();
            let reply_pid = reply_nl_pid(header, local_port);
            let mut buffer = self.data.lock();
            match message_type {
                NetlinkMessageType::GetLink => {
                    let ifaces = get_net_device();
                    info!("Netlink GetLink: found {} interfaces", ifaces.len());
                    for (i, iface) in ifaces.iter().enumerate() {
                        let mut msg = Vec::new();
                        let new_header = NetlinkMessageHeader {
                            nlmsg_len: 0, // to be determined later
                            nlmsg_type: NetlinkMessageType::NewLink.into(),
                            nlmsg_flags: NetlinkMessageFlags::MULTI,
                            nlmsg_seq: header.nlmsg_seq,
                            nlmsg_pid: reply_pid,
                        };
                        msg.push_ext(new_header);

                        let is_loopback = iface.get_ifname() == "loopback";
                        let ifi_type = if is_loopback {
                            ARPHRD_LOOPBACK
                        } else {
                            ARPHRD_ETHER
                        };
                        let ifi_flags = if is_loopback {
                            IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_NOARP | IFF_LOWER_UP
                        } else {
                            // Always report RUNNING like v0.5.0. Gating on
                            // STATUS.LU made `ip link` show NO-CARRIER while
                            // the PHY was still coming up, so udhcpc/`ip`
                            // skipped the NIC that DHCP needs.
                            IFF_UP | IFF_BROADCAST | IFF_MULTICAST | IFF_RUNNING | IFF_LOWER_UP
                        };

                        let if_info = IfaceInfoMsg {
                            ifi_family: (u16::from(AddressFamily::Unspecified)) as u8,
                            ifi_pad: 0,
                            ifi_type,
                            ifi_index: (i as i32) + 1, // Linux interface indices start at 1
                            ifi_flags,
                            ifi_change: IFF_CHANGE_ALL, // all flags changeable (kernel convention)
                        };
                        msg.align4();
                        msg.push_ext(if_info);

                        let mut attrs = Vec::new();

                        let mac_addr = iface.get_mac();
                        push_rtattr_bytes(
                            &mut attrs,
                            RouteAttrTypes::Address.into(),
                            if is_loopback {
                                &[0; 6]
                            } else {
                                mac_addr.as_bytes()
                            },
                        );

                        if !is_loopback {
                            // Broadcast MAC for Ethernet.
                            push_rtattr_bytes(
                                &mut attrs,
                                RouteAttrTypes::Broadcast.into(),
                                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                            );
                        }

                        // MTU (best-effort default; drivers can expose real value later).
                        push_rtattr_u32(&mut attrs, RouteAttrTypes::MTU.into(), 1500);

                        // ifOperStatus: 6 == IF_OPER_UP.
                        push_rtattr_bytes(&mut attrs, RouteAttrTypes::OperState.into(), &[6u8]);

                        // IFLA_LINK: for plain Ethernet, point to self ifindex.
                        push_rtattr_u32(&mut attrs, RouteAttrTypes::Link.into(), (i as u32) + 1);

                        let ifname = iface.get_ifname();
                        // IFLA_IFNAME includes a null terminator (Linux kernel convention)
                        let mut ifname_bytes = Vec::from(ifname.as_bytes());
                        ifname_bytes.push(0u8);
                        push_rtattr_bytes(&mut attrs, RouteAttrTypes::Ifname.into(), &ifname_bytes);

                        msg.align4();
                        msg.append(&mut attrs);

                        msg.align4();
                        msg.set_ext(0, msg.len() as u32);

                        push_netlink_rx(&mut buffer, msg);
                    }
                }
                NetlinkMessageType::GetAddr => {
                    let ifaces = get_net_device();
                    for iface in &ifaces {
                        crate::net::ensure_ipv6_link_local(iface.as_ref());
                    }
                    // Byte pattern of the pre-DHCP placeholder IPv4 address.
                    let placeholder_v4: [u8; 4] = [240, 0, 0, 0];
                    // RTM_GETADDR carries the requested address family in the first
                    // byte after the netlink header (rtgenmsg.rtgen_family /
                    // ifaddrmsg.ifa_family). `ip -4 addr flush` / `ip -6 addr flush`
                    // dump with AF_INET / AF_INET6 and rely on the kernel to filter,
                    // then turn every returned address into an RTM_DELADDR. If we
                    // ignore the filter and dump every family, `ip -4 addr flush`
                    // also wipes IPv6 addresses (and vice-versa) — e.g. running
                    // `udhcpc` after `udhcpc6` silently drops the DHCPv6 global.
                    let req_family: u8 = data
                        .get(size_of::<NetlinkMessageHeader>())
                        .copied()
                        .unwrap_or(0);
                    let af_inet: u8 = {
                        let f: u16 = AddressFamily::Internet.into();
                        f as u8
                    };
                    let af_inet6: u8 = {
                        let f: u16 = AddressFamily::Internet6.into();
                        f as u8
                    };
                    for (i, iface) in ifaces.iter().enumerate() {
                        let ip_addrs = iface.get_ip_address();
                        for ip in &ip_addrs {
                            let ip_addr = ip.address();
                            let ip_bytes = ip_addr.as_bytes();

                            // Skip placeholder IPv4 240.0.0.0 entries (assigned before DHCP).
                            if ip_bytes == placeholder_v4 {
                                continue;
                            }

                            // Skip FREE SLOTS. The drivers hold a fixed pool of
                            // address slots and mark an empty one as
                            // `0.0.0.0/0`: `add_ip_address` fills the first such
                            // slot, `remove_ip_address` resets one back to it.
                            // They are bookkeeping, not addresses, and reporting
                            // them had two visible costs:
                            //
                            //  * `ip addr show eth0` listed three phantom
                            //    `inet 0.0.0.0/0` entries beside the real one;
                            //  * `ip addr flush` never terminated. busybox loops
                            //    `for(;;)` { dump; delete everything returned; }
                            //    and stops only when a dump comes back empty. It
                            //    kept being handed slots that deleting cannot
                            //    remove, so it dumped and deleted forever.
                            //
                            // (Before the ACK fix below, that loop exited early
                            // for the wrong reason: the flush "failed", so it
                            // never spun. Fixing one exposed the other.)
                            if ip_addr.is_unspecified() && ip.prefix_len() == 0 {
                                continue;
                            }

                            // Derive address family from byte width.
                            let ifa_family: u8 = if ip_bytes.len() == 4 {
                                af_inet
                            } else {
                                af_inet6
                            };

                            // Honor the requested family filter (AF_UNSPEC = dump all).
                            if req_family != 0 && req_family != ifa_family {
                                continue;
                            }

                            // Compute scope per RFC 2473 / rt_scope_t:
                            //   RT_SCOPE_HOST=254  (loopback), RT_SCOPE_LINK=253 (link-local), 0 (global)
                            let ifa_scope: u8 = if ip_bytes.len() == 16 {
                                // IPv6 link-local: fe80::/10 (first byte 0xfe, second byte top-2-bits == 10).
                                if ip_bytes[0] == 0xfe && (ip_bytes[1] & 0xc0) == 0x80 {
                                    253 // RT_SCOPE_LINK
                                } else if ip_bytes
                                    == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
                                {
                                    254 // RT_SCOPE_HOST (::1)
                                } else {
                                    0 // RT_SCOPE_UNIVERSE
                                }
                            } else {
                                // IPv4 loopback 127.x.x.x
                                if ip_bytes[0] == 127 {
                                    254 // RT_SCOPE_HOST
                                } else {
                                    0 // RT_SCOPE_UNIVERSE
                                }
                            };

                            let mut msg = Vec::new();
                            let new_header = NetlinkMessageHeader {
                                nlmsg_len: 0, // to be determined later
                                nlmsg_type: NetlinkMessageType::NewAddr.into(),
                                nlmsg_flags: NetlinkMessageFlags::MULTI,
                                nlmsg_seq: header.nlmsg_seq,
                                nlmsg_pid: reply_pid,
                            };
                            msg.push_ext(new_header);

                            let if_addr = IfaceAddrMsg {
                                ifa_family,
                                ifa_prefixlen: ip.prefix_len(),
                                ifa_flags: 0,
                                ifa_scope,
                                ifa_index: (i + 1) as u32, // must match GetLink ifi_index (1-based)
                            };
                            msg.align4();
                            msg.push_ext(if_addr);

                            let mut attrs = Vec::new();

                            // IFA_LOCAL and IFA_ADDRESS are both used by userland.
                            push_rtattr_bytes(&mut attrs, IfAddrAttrTypes::Local.into(), ip_bytes);
                            push_rtattr_bytes(
                                &mut attrs,
                                IfAddrAttrTypes::Address.into(),
                                ip_bytes,
                            );

                            // Label (interface name) with NUL terminator.
                            let ifname = iface.get_ifname();
                            let mut ifname_bytes = Vec::from(ifname.as_bytes());
                            ifname_bytes.push(0u8);
                            push_rtattr_bytes(
                                &mut attrs,
                                IfAddrAttrTypes::Label.into(),
                                &ifname_bytes,
                            );

                            // IFA_FLAGS (musl getifaddrs / udhcpc6 expect this on IPv6 addrs).
                            if ip_bytes.len() == 16 {
                                let flags: u32 =
                                    if ip_bytes[0] == 0xfe && (ip_bytes[1] & 0xc0) == 0x80 {
                                        0x82 // IFA_F_NODAD | IFA_F_PERMANENT
                                    } else {
                                        0x80 // IFA_F_PERMANENT
                                    };
                                push_rtattr_u32(&mut attrs, IfAddrAttrTypes::Flags.into(), flags);
                            }

                            // IPv4 broadcast if applicable.
                            if ip_bytes.len() == 4 {
                                let bcast = ipv4_broadcast(
                                    smoltcp::wire::Ipv4Address::from_bytes(ip_bytes),
                                    ip.prefix_len(),
                                );
                                push_rtattr_bytes(
                                    &mut attrs,
                                    IfAddrAttrTypes::Broadcast.into(),
                                    bcast.as_bytes(),
                                );
                            }

                            msg.align4();
                            msg.append(&mut attrs);

                            msg.align4();
                            msg.set_ext(0, msg.len() as u32);

                            push_netlink_rx(&mut buffer, msg);
                        }
                    }
                }
                NetlinkMessageType::NewAddr => {
                    if let Some((ifindex, cidr)) = parse_ifaddr_cidr(data) {
                        if let Ok(iface) = crate::net::iface_by_linux_ifindex(ifindex) {
                            let _ = iface.add_ip_address(cidr);
                            if let IpCidr::Ipv4(v4) = cidr {
                                let _ = iface.set_ipv4_address(v4);
                                crate::net::prepare_ipv4_stack();
                            }
                            log::debug!(
                                "[netlink] NewAddr {} on {} ifindex={}",
                                cidr,
                                iface.get_ifname(),
                                ifindex
                            );
                        } else {
                            log::warn!("[netlink] NewAddr: unknown ifindex {}", ifindex);
                        }
                    }
                    push_ack(&mut buffer, header, reply_pid);
                }
                NetlinkMessageType::NewRoute => {
                    if let Some((rtm, dst_cidr, gw_ip, oif)) = parse_route_request(data) {
                        if oif != 0 {
                            if let Ok(iface) = crate::net::iface_by_linux_ifindex(oif) {
                                let _ = iface.add_route(dst_cidr, gw_ip);
                                info!(
                                    "[netlink] NewRoute: {:?} gw={:?} via {} (oif {})",
                                    dst_cidr,
                                    gw_ip,
                                    iface.get_ifname(),
                                    oif
                                );
                            }
                        } else if let Some(gw) = gw_ip {
                            // Gateway without RTA_OIF: pick first matching family iface.
                            let ifaces = get_net_device();
                            let iface = ifaces.iter().find(|i| {
                                i.get_ip_address().iter().any(|a| {
                                    matches!(
                                        (a, &gw),
                                        (IpCidr::Ipv4(_), smoltcp::wire::IpAddress::Ipv4(_))
                                            | (IpCidr::Ipv6(_), smoltcp::wire::IpAddress::Ipv6(_))
                                    )
                                })
                            });
                            if let Some(iface) = iface {
                                let _ = iface.add_route(dst_cidr, Some(gw));
                            }
                        }
                        if matches!(dst_cidr, IpCidr::Ipv4(_)) {
                            crate::net::prepare_ipv4_stack();
                        }
                        let _ = rtm;
                    }
                    push_ack(&mut buffer, header, reply_pid);
                }
                NetlinkMessageType::GetRoute => {
                    let ifaces = get_net_device();
                    for (i, iface) in ifaces.iter().enumerate() {
                        let ifindex = (i + 1) as u32;
                        for route in iface.get_routes() {
                            push_route_dump_entry(
                                &mut buffer,
                                header.nlmsg_seq,
                                reply_pid,
                                ifindex,
                                &route,
                            );
                        }
                    }
                    info!("[netlink] GetRoute: dumped routes");
                }
                NetlinkMessageType::DelAddr => {
                    if let Some((ifindex, cidr)) = parse_ifaddr_cidr(data) {
                        if let Ok(iface) = crate::net::iface_by_linux_ifindex(ifindex) {
                            let skip = matches!(
                                cidr,
                                smoltcp::wire::IpCidr::Ipv6(v6) if v6.address().is_link_local()
                            );
                            if !skip {
                                let _ = iface.remove_ip_address(cidr);
                            }
                            info!(
                                "[netlink] DelAddr: removed {} from {} (ifindex {})",
                                cidr,
                                iface.get_ifname(),
                                ifindex
                            );
                        }
                    }
                    push_ack(&mut buffer, header, reply_pid);
                }
                NetlinkMessageType::DelRoute => {
                    if let Some((_rtm, dst_cidr, gw_ip, oif)) = parse_route_request(data) {
                        let iface = if oif != 0 {
                            crate::net::iface_by_linux_ifindex(oif).ok()
                        } else {
                            None
                        };
                        if let Some(iface) = iface {
                            let _ = iface.del_route(dst_cidr, gw_ip);
                            info!(
                                "[netlink] DelRoute: removed {:?} gw={:?} from {}",
                                dst_cidr,
                                gw_ip,
                                iface.get_ifname()
                            );
                        }
                    }
                    push_ack(&mut buffer, header, reply_pid);
                }
                _ => {
                    // Unknown/unimplemented request: return NLMSG_ERROR with -EOPNOTSUPP.
                    // This is better than a silent NLMSG_DONE which confuses userland.
                    const EOPNOTSUPP: i32 = 95;
                    #[repr(C)]
                    #[derive(Copy, Clone)]
                    struct NetlinkError {
                        error: i32,
                        msg: NetlinkMessageHeader,
                    }
                    const _: () = {
                        assert!(size_of::<NetlinkError>() == 20);
                    };
                    let err = NetlinkError {
                        error: -EOPNOTSUPP,
                        msg: *header,
                    };
                    let mut msg = Vec::new();
                    let new_header = NetlinkMessageHeader {
                        nlmsg_len: 0,
                        nlmsg_type: NetlinkMessageType::Error.into(),
                        nlmsg_flags: NetlinkMessageFlags::MULTI,
                        nlmsg_seq: header.nlmsg_seq,
                        nlmsg_pid: reply_pid,
                    };
                    msg.push_ext(new_header);
                    msg.align4();
                    msg.push_ext(err);
                    msg.align4();
                    msg.set_ext(0, msg.len() as u32);
                    push_netlink_rx(&mut buffer, msg);
                }
            }
            let is_dump = matches!(
                message_type,
                NetlinkMessageType::GetLink
                    | NetlinkMessageType::GetAddr
                    | NetlinkMessageType::GetRoute
            );
            if is_dump {
                let mut msg = Vec::new();
                let new_header = NetlinkMessageHeader {
                    nlmsg_len: 0, // to be determined later
                    nlmsg_type: NetlinkMessageType::Done.into(),
                    nlmsg_flags: NetlinkMessageFlags::MULTI,
                    nlmsg_seq: header.nlmsg_seq,
                    nlmsg_pid: reply_pid,
                };
                msg.push_ext(new_header);
                msg.align4();
                msg.push_ext(0i32);
                msg.align4();
                msg.set_ext(0, msg.len() as u32);
                push_netlink_rx(&mut buffer, msg);
                info!(
                    "[netlink] write: pushed DONE, buffer len now {}",
                    buffer.len()
                );
            }
        }
        self.base.signal_set(Signal::READABLE);
        Ok(whole.len())
    }

    /// connect (netlink sockets do not support connect)
    async fn connect(&self, _endpoint: Endpoint) -> SysResult {
        // Netlink sockets do not support connect(2). Returning ENOTSUP is
        // correct for SOCK_RAW/SOCK_DGRAM netlink; prevents panic on `ip`
        // tool usage inside udhcpc default.script.
        warn!("[netlink] connect: not supported on netlink sockets");
        Err(LxError::EINVAL)
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        if let Endpoint::Netlink(mut netlink) = endpoint {
            if netlink.port_id == 0 {
                netlink.port_id = self.auto_port_id();
            }
            *self.local_endpoint.lock() = Some(netlink);
            Ok(0)
        } else {
            Err(LxError::EINVAL)
        }
    }

    fn listen(&self) -> SysResult {
        warn!("[netlink] listen: not supported on netlink sockets");
        Err(LxError::EINVAL)
    }

    fn shutdown(&self, _howto: usize) -> SysResult {
        // Accept shutdown silently — some userland code calls shutdown() before
        // close() even on netlink sockets. Return success to avoid EINVAL noise.
        Ok(0)
    }

    async fn accept(&self) -> LxResult<(Arc<dyn FileLike>, Endpoint)> {
        warn!("[netlink] accept: not supported on netlink sockets");
        Err(LxError::EINVAL)
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let groups = self
            .local_endpoint
            .lock()
            .as_ref()
            .map(|e| e.multicast_groups_mask)
            .unwrap_or(0);
        Some(Endpoint::Netlink(NetlinkEndpoint::new(
            self.local_port_id(),
            groups,
        )))
    }

    fn remote_endpoint(&self) -> Option<Endpoint> {
        // Netlink sockets are connectionless; no remote endpoint.
        None
    }

    fn setsockopt(&self, _level: usize, _opt: usize, _data: &[u8]) -> SysResult {
        Ok(0)
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> SysResult {
        crate::net::handle_net_ioctl(request, arg1, arg2, arg3, false)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let readable = !self.data.lock().is_empty();
        (readable, true, false)
    }
}

zircon_object::impl_kobject!(NetlinkSocketState);

#[async_trait]
impl FileLike for NetlinkSocketState {
    /// A socket reports `S_IFSOCK` to `fstat(2)`, as on Linux.
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        Ok(crate::fs::anon_metadata(
            rcore_fs::vfs::FileType::Socket,
            self.id() as usize,
        ))
    }

    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        let flags = &mut *self.flags.lock();
        flags.set(OpenFlags::APPEND, f.contains(OpenFlags::APPEND));
        flags.set(OpenFlags::NON_BLOCK, f.contains(OpenFlags::NON_BLOCK));
        flags.set(OpenFlags::CLOEXEC, f.contains(OpenFlags::CLOEXEC));
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        Socket::read(self, buf).await.0
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        // Sockets do not support positioned reads.
        Err(LxError::ESPIPE)
    }

    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        Socket::write(self, buf, None)
    }

    fn poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus {
            read,
            write,
            error,
            hangup: false,
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus {
            read,
            write,
            error,
            hangup: false,
        })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        Socket::ioctl(self, request, arg1, arg2, arg3)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

/// Common structure:
/// | nlmsghdr | ifinfomsg/ifaddrmsg | rtattr | rtattr | rtattr | ... | rtattr
/// All aligned to 4 bytes boundary
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct NetlinkMessageHeader {
    nlmsg_len: u32,                   // length of message including header
    nlmsg_type: u16,                  // message content
    nlmsg_flags: NetlinkMessageFlags, // additional flags
    nlmsg_seq: u32,                   // sequence number
    nlmsg_pid: u32,                   // sending process port id
}

const _: () = {
    // Linux rtnetlink ABI sanity checks (x86_64): nlmsghdr is 16 bytes.
    assert!(size_of::<NetlinkMessageHeader>() == 16);
};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct IfaceInfoMsg {
    // Matches Linux `struct ifinfomsg` layout.
    ifi_family: u8,
    ifi_pad: u8,
    ifi_type: u16,
    ifi_index: i32,
    ifi_flags: u32,
    ifi_change: u32,
}

const _: () = {
    // Linux `struct ifinfomsg` is 16 bytes.
    assert!(size_of::<IfaceInfoMsg>() == 16);
};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct IfaceAddrMsg {
    ifa_family: u8,
    ifa_prefixlen: u8,
    ifa_flags: u8,
    ifa_scope: u8,
    ifa_index: u32,
}

const _: () = {
    // Linux `struct ifaddrmsg` is 8 bytes.
    assert!(size_of::<IfaceAddrMsg>() == 8);
};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct RouteAttr {
    rta_len: u16,
    rta_type: u16,
}

const _: () = {
    // Linux `struct rtattr` is 4 bytes.
    assert!(size_of::<RouteAttr>() == 4);
};

bitflags! {
    struct NetlinkMessageFlags : u16 {
        const REQUEST = 0x01;
        const MULTI = 0x02;
        const ACK = 0x04;
        const ECHO = 0x08;
        const DUMP_INTR = 0x10;
        const DUMP_FILTERED = 0x20;
        // GET request
        const ROOT = 0x100;
        const MATCH = 0x200;
        const ATOMIC = 0x400;
        const DUMP = 0x100 | 0x200;
        // NEW request
        const REPLACE = 0x100;
        const EXCL = 0x200;
        const CREATE = 0x400;
        const APPEND = 0x800;
        // DELETE request
        const NONREC = 0x100;
        // ACK message
        const CAPPED = 0x100;
        const ACK_TLVS = 0x200;
    }
}

enum_with_unknown! {
    /// Netlink message types
    pub doc enum NetlinkMessageType(u16) {
        /// Nothing
        Noop = 1,
        /// Error
        Error = 2,
        /// End of a dump
        Done = 3,
        /// Data lost
        Overrun = 4,
        /// New link
        NewLink = 16,
        /// Delete link
        DelLink = 17,
        /// Get link
        GetLink = 18,
        /// Set link
        SetLink = 19,
        /// New addr
        NewAddr = 20,
        /// Delete addr
        DelAddr = 21,
        /// Get addr
        GetAddr = 22,
        /// New route
        NewRoute = 24,
        /// Delete route
        DelRoute = 25,
        /// Get route
        GetRoute = 26,
    }
}

enum_with_unknown! {
    /// Route Attr Types
    pub doc enum RouteAttrTypes(u16) {
        /// Unspecified
        Unspecified = 0,
        /// MAC Address
        Address = 1,
        /// Broadcast
        Broadcast = 2,
        /// Interface name
        Ifname = 3,
        /// MTU
        MTU = 4,
        /// Link
        Link = 5,
        /// Operational state (IF_OPER_*)
        OperState = 16,
    }
}

enum_with_unknown! {
    /// ifaddrmsg attribute types (IFA_*)
    pub doc enum IfAddrAttrTypes(u16) {
        /// Unspecified
        Unspecified = 0,
        /// IFA_ADDRESS
        Address = 1,
        /// IFA_LOCAL
        Local = 2,
        /// IFA_LABEL
        Label = 3,
        /// IFA_BROADCAST
        Broadcast = 4,
        /// IFA_FLAGS
        Flags = 8,
    }
}

fn push_rtattr_bytes(dst: &mut Vec<u8>, rta_type: u16, payload: &[u8]) {
    let attr = RouteAttr {
        rta_len: (payload.len() + size_of::<RouteAttr>()) as u16,
        rta_type,
    };
    dst.align4();
    dst.push_ext(attr);
    dst.extend_from_slice(payload);
}

fn push_rtattr_u32(dst: &mut Vec<u8>, rta_type: u16, v: u32) {
    push_rtattr_bytes(dst, rta_type, &v.to_ne_bytes());
}

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;

#[repr(C)]
#[derive(Copy, Clone)]
struct RouteMsg {
    rtm_family: u8,
    rtm_dst_len: u8,
    rtm_src_len: u8,
    rtm_tos: u8,
    rtm_table: u8,
    rtm_protocol: u8,
    rtm_scope: u8,
    rtm_type: u8,
    rtm_flags: u32,
}

const _: () = {
    assert!(size_of::<RouteMsg>() == 12);
};

/// Build a CIDR from an address and the prefix length a netlink message
/// carried, or `None` when the two do not go together.
///
/// The prefix length arrives straight from a `sendmsg` on an `AF_NETLINK`
/// socket — `ifa_prefixlen` of `struct ifaddrmsg`, `rtm_dst_len` of
/// `struct rtmsg` — and smoltcp's `Ipv4Cidr::new`/`Ipv6Cidr::new` **panic**
/// when it is out of range (`["Prefix length should be <= 32"][..]` and
/// `assert!(prefix_len <= 128)`). Six call sites handed it over unchecked, so
/// one `RTM_NEWADDR` with `ifa_prefixlen = 33` — twelve bytes of attacker
/// data, no privilege needed to build the socket — took the kernel down.
///
/// Linux checks the same thing and answers `EINVAL` (`inet_rtm_newaddr`:
/// `ifa_prefixlen > 32`; `rtm_to_fib_config`: `fc_dst_len > 32`), which is
/// what the `None` here turns into.
/// Copy a fixed-size netlink struct out of a message at `off`, or `None` when
/// the message is not long enough to hold one.
///
/// Every one of these reads is from a `Vec<u8>` a process handed to
/// `sendmsg`, at an NLMSG_ALIGN(4) offset inside a buffer the allocator only
/// aligned to 1, so a `&NlStruct` pointed at it may be misaligned for its own
/// `u32` fields: undefined behaviour, and a fault on the architectures that
/// check alignment. The same read was written five times over, `read_unaligned`
/// in one of them and a plain reference in the other four — with the bound
/// check somewhere else, or missing. Here it is written once, and the bound
/// sits against the read it protects.
fn read_struct<T: Copy>(data: &[u8], off: usize) -> Option<T> {
    if data.len().checked_sub(off)? < size_of::<T>() {
        return None;
    }
    #[allow(unsafe_code)]
    Some(unsafe { core::ptr::read_unaligned(data[off..].as_ptr() as *const T) })
}

/// Walk the `rtattr` chain that follows the fixed body of a netlink message,
/// handing each attribute's type and payload to `f`.
///
/// Both request parsers carried their own copy of this loop, down to the same
/// comment — and it is four lines of arithmetic on numbers a process chose:
/// `rta_len` below the header names a payload of negative length, and past the
/// end names bytes the message does not have. Either one is a slice out of
/// bounds, which in a kernel is not an exception. Linux's `RTA_OK` checks the
/// same two bounds; this is the one place that does.
fn for_each_rtattr(data: &[u8], mut off: usize, mut f: impl FnMut(u16, &[u8])) {
    while let Some(rta) = read_struct::<RouteAttr>(data, off) {
        let rta_len = rta.rta_len as usize;
        // `read_struct` succeeded, so `off + 4 <= data.len()`.
        if rta_len < size_of::<RouteAttr>() || rta_len > data.len() - off {
            break;
        }
        f(
            rta.rta_type,
            &data[off + size_of::<RouteAttr>()..off + rta_len],
        );
        // The NEXT attribute starts at RTA_ALIGN(rta_len); the padding is
        // between attributes, never counted inside one.
        off += (rta_len + 3) & !3;
    }
}

fn cidr_from_parts(addr: IpAddress, prefix_len: u8) -> Option<IpCidr> {
    match addr {
        IpAddress::Ipv4(v4) if prefix_len <= 32 => {
            Some(IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(v4, prefix_len)))
        }
        IpAddress::Ipv6(v6) if prefix_len <= 128 => {
            Some(IpCidr::Ipv6(smoltcp::wire::Ipv6Cidr::new(v6, prefix_len)))
        }
        _ => None,
    }
}

fn parse_ifaddr_cidr(data: &[u8]) -> Option<(u32, IpCidr)> {
    let ifa_off = size_of::<NetlinkMessageHeader>();
    let ifa: IfaceAddrMsg = read_struct(data, ifa_off)?;
    let mut ip_bytes: Option<Vec<u8>> = None;
    for_each_rtattr(
        data,
        ifa_off + size_of::<IfaceAddrMsg>(),
        |rta_type, payload| {
            let t = IfAddrAttrTypes::from(rta_type);
            if matches!(t, IfAddrAttrTypes::Local | IfAddrAttrTypes::Address)
                && (payload.len() == 4 || payload.len() == 16)
            {
                ip_bytes = Some(payload.to_vec());
            }
        },
    );
    let bytes = ip_bytes?;
    let (addr, whole) = if bytes.len() == 4 {
        let mut arr = [0u8; 4];
        arr.copy_from_slice(&bytes);
        (IpAddress::Ipv4(Ipv4Address::from_bytes(&arr)), 32)
    } else {
        let mut arr = [0u8; 16];
        arr.copy_from_slice(&bytes);
        (IpAddress::Ipv6(Ipv6Address::from_bytes(&arr)), 128)
    };
    let prefix = if ifa.ifa_prefixlen != 0 {
        ifa.ifa_prefixlen
    } else {
        whole
    };
    let cidr = cidr_from_parts(addr, prefix)?;
    Some((ifa.ifa_index, cidr))
}

fn parse_route_request(data: &[u8]) -> Option<(RouteMsg, IpCidr, Option<IpAddress>, u32)> {
    let rtm_off = size_of::<NetlinkMessageHeader>();
    let rtm: RouteMsg = read_struct(data, rtm_off)?;
    let mut dst_bytes: Option<Vec<u8>> = None;
    let mut gw_ip: Option<IpAddress> = None;
    let mut oif: u32 = 0;
    for_each_rtattr(
        data,
        rtm_off + size_of::<RouteMsg>(),
        |rta_type, payload| match rta_type {
            RTA_DST if payload.len() == 4 || payload.len() == 16 => {
                dst_bytes = Some(payload.to_vec());
            }
            RTA_GATEWAY if payload.len() == 4 => {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(payload);
                gw_ip = Some(IpAddress::Ipv4(Ipv4Address::from_bytes(&arr)));
            }
            RTA_GATEWAY if payload.len() == 16 => {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(payload);
                gw_ip = Some(IpAddress::Ipv6(Ipv6Address::from_bytes(&arr)));
            }
            RTA_OIF if payload.len() == 4 => {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(payload);
                oif = u32::from_ne_bytes(arr);
            }
            _ => {}
        },
    );

    let dst_cidr = if let Some(bytes) = dst_bytes {
        let addr = if bytes.len() == 4 {
            let mut arr = [0u8; 4];
            arr.copy_from_slice(&bytes);
            IpAddress::Ipv4(Ipv4Address::from_bytes(&arr))
        } else {
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&bytes);
            IpAddress::Ipv6(Ipv6Address::from_bytes(&arr))
        };
        cidr_from_parts(addr, rtm.rtm_dst_len)?
    } else if rtm.rtm_family as u16 == AddressFamily::Internet.into() {
        cidr_from_parts(IpAddress::v4(0, 0, 0, 0), rtm.rtm_dst_len)?
    } else if rtm.rtm_family as u16 == AddressFamily::Internet6.into() {
        cidr_from_parts(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0), rtm.rtm_dst_len)?
    } else {
        let gw = gw_ip?;
        match gw {
            IpAddress::Ipv4(_) => IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            IpAddress::Ipv6(_) => IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0), 0),
            _ => return None,
        }
    };

    Some((rtm, dst_cidr, gw_ip, oif))
}

fn push_route_dump_entry(
    buffer: &mut Vec<Vec<u8>>,
    seq: u32,
    pid: u32,
    ifindex: u32,
    route: &RouteInfo,
) {
    let (family, dst_len, dst_bytes, scope) = match route.dst {
        IpCidr::Ipv4(cidr) => {
            let f: u16 = AddressFamily::Internet.into();
            (
                f as u8,
                cidr.prefix_len(),
                cidr.address().as_bytes().to_vec(),
                if route.gateway.is_some() { 0u8 } else { 253u8 },
            )
        }
        IpCidr::Ipv6(cidr) => {
            let f: u16 = AddressFamily::Internet6.into();
            (
                f as u8,
                cidr.prefix_len(),
                cidr.address().as_bytes().to_vec(),
                if route.gateway.is_some() { 0u8 } else { 253u8 },
            )
        }
        _ => return,
    };

    let mut msg = Vec::new();
    let new_header = NetlinkMessageHeader {
        nlmsg_len: 0,
        nlmsg_type: NetlinkMessageType::NewRoute.into(),
        nlmsg_flags: NetlinkMessageFlags::MULTI,
        nlmsg_seq: seq,
        nlmsg_pid: pid,
    };
    msg.push_ext(new_header);

    let rtm = RouteMsg {
        rtm_family: family,
        rtm_dst_len: dst_len,
        rtm_src_len: 0,
        rtm_tos: 0,
        rtm_table: 254,  // RT_TABLE_MAIN
        rtm_protocol: 4, // RTPROT_STATIC
        rtm_scope: scope,
        // RTN_UNICAST is 1, not 2 (2 is RTN_LOCAL). With 2, iproute2 rendered
        // every route as `local ...` and a default route as `local 0.0.0.0/0`
        // instead of `default`, so `grep default` and every "is there a default
        // route?" check (the udhcpc script, the desktop's route probe) missed a
        // route that was actually installed and working.
        rtm_type: 1, // RTN_UNICAST
        // No flags. The low bits of rtm_flags are the single-path nexthop flags
        // RTNH_F_DEAD (0x01) and RTNH_F_PERVASIVE (0x02); setting them made
        // iproute2 print "dead pervasive" on every route, marking a live route
        // dead. A normal installed route carries no flags here.
        rtm_flags: 0,
    };
    msg.align4();
    msg.push_ext(rtm);

    let mut attrs = Vec::new();
    // A default route (prefix 0) carries NO RTA_DST: iproute2 prints the literal
    // word "default" only when the destination attribute is absent and dst_len
    // is 0. Emitting RTA_DST=0.0.0.0 instead made it render "0.0.0.0/0", so
    // `ip route | grep default` (the udhcpc default-route probe, the desktop's
    // connectivity check) found nothing even though the route was installed.
    if dst_len != 0 {
        push_rtattr_bytes(&mut attrs, RTA_DST, &dst_bytes);
    }
    if let Some(gw) = route.gateway {
        push_rtattr_bytes(&mut attrs, RTA_GATEWAY, gw.as_bytes());
    }
    push_rtattr_u32(&mut attrs, RTA_OIF, ifindex);

    msg.align4();
    msg.append(&mut attrs);
    msg.align4();
    msg.set_ext(0, msg.len() as u32);
    push_netlink_rx(buffer, msg);
}

/// Build a success ACK (NLMSG_ERROR with error=0) and push it onto `buffer` —
/// but ONLY when the request asked for one with `NLM_F_ACK`, which is what
/// Linux does.
///
/// Acking unconditionally is what broke `ip addr flush`. busybox's
/// `rtnl_send_check` writes the batch of RTM_DELADDR messages, peeks the
/// socket, and treats ANY `NLMSG_ERROR` it finds as a failure — it never
/// inspects `error`, so a success ACK is indistinguishable from a real one:
///
/// ```text
/// if (h->nlmsg_type == NLMSG_ERROR) { errno = -err->error; return -1; }
/// ```
///
/// Its flush messages carry NLM_F_REQUEST only, so on Linux nothing comes back,
/// the peek returns EAGAIN, and the flush succeeds. Here every one of them got
/// an ACK, the peek found NLMSG_ERROR, and `ip` printed
/// "can't send flush request" — leaving a stale 0.0.0.0/0 beside each real
/// address.
///
/// `ip addr add` and `ip route add|del` go through `rtnl_talk`, which DOES set
/// NLM_F_ACK and waits for the reply, so they keep their ACK and keep working.
fn push_ack(buffer: &mut Vec<Vec<u8>>, req: &NetlinkMessageHeader, nl_pid: u32) {
    if !req.nlmsg_flags.contains(NetlinkMessageFlags::ACK) {
        info!(
            "[netlink] no ACK requested (flags={:#x}, seq={}); staying silent like Linux",
            req.nlmsg_flags.bits(),
            req.nlmsg_seq
        );
        return;
    }
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct NetlinkError {
        error: i32,
        msg: NetlinkMessageHeader,
    }
    const _: () = {
        assert!(size_of::<NetlinkError>() == 20);
    };
    let err = NetlinkError {
        error: 0,
        msg: *req,
    };
    let mut msg = Vec::new();
    let ack = NetlinkMessageHeader {
        nlmsg_len: 0,
        nlmsg_type: NetlinkMessageType::Error.into(),
        nlmsg_flags: NetlinkMessageFlags::empty(),
        nlmsg_seq: req.nlmsg_seq,
        nlmsg_pid: nl_pid,
    };
    msg.push_ext(ack);
    msg.push_ext(err);
    msg.align4();
    msg.set_ext(0, msg.len() as u32);
    info!(
        "[netlink] push_ack: seq={}, len={}",
        req.nlmsg_seq,
        msg.len()
    );
    push_netlink_rx(buffer, msg);
}

fn ipv4_broadcast(addr: smoltcp::wire::Ipv4Address, prefix_len: u8) -> smoltcp::wire::Ipv4Address {
    let ip = u32::from_be_bytes(addr.0);
    // Both ends are shifts a `u32` cannot take: `<< 32` for a /0, and a shift
    // count that wraps below zero for anything past /32. The first is the
    // `prefix_len == 0` arm; the clamp is the second. `/32` is not a special
    // case -- it shifts by 0 and its broadcast is the address itself.
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len.min(32) as u32)
    };
    let bcast = ip | (!mask);
    smoltcp::wire::Ipv4Address::from_bytes(&bcast.to_be_bytes())
}

trait VecExt {
    fn align4(&mut self);
    fn push_ext<T: Sized>(&mut self, data: T);
    fn set_ext<T: Sized>(&mut self, offset: usize, data: T);
}

impl VecExt for Vec<u8> {
    fn align4(&mut self) {
        let len = (self.len() + 3) & !3;
        if len > self.len() {
            self.resize(len, 0);
        }
    }

    fn push_ext<T: Sized>(&mut self, data: T) {
        #[allow(unsafe_code)]
        let bytes =
            unsafe { slice::from_raw_parts(&data as *const T as *const u8, size_of::<T>()) };
        for byte in bytes {
            self.push(*byte);
        }
    }

    fn set_ext<T: Sized>(&mut self, offset: usize, data: T) {
        if self.len() < offset + size_of::<T>() {
            self.resize(offset + size_of::<T>(), 0);
        }
        #[allow(unsafe_code)]
        let bytes =
            unsafe { slice::from_raw_parts(&data as *const T as *const u8, size_of::<T>()) };
        self[offset..(bytes.len() + offset)].copy_from_slice(bytes);
    }
}

#[cfg(test)]
mod netlink_tests {
    //! Host tests for what a process is allowed to send down an `AF_NETLINK`
    //! socket, and for what comes back.
    //!
    //! This is `ip`, `udhcpc`, `getifaddrs()` and every "am I online?" probe
    //! on the box, and the whole surface is length fields and prefix lengths
    //! copied out of a buffer userspace wrote. None of it needs an interface
    //! or the network stack: the parsers take a `&[u8]`, and the builders
    //! hand back a `Vec<u8>`, so a request can be built here and walked back
    //! the way `iproute2` walks it.

    use super::*;

    const NLMSG_HDR: usize = size_of::<NetlinkMessageHeader>();
    const IFA_LOCAL: u16 = 2;
    const IFA_ADDRESS: u16 = 1;

    /// A `nlmsghdr` with its length filled in by the caller's buffer, the way
    /// `libnetlink` writes one.
    fn header(msg_type: u16) -> NetlinkMessageHeader {
        NetlinkMessageHeader {
            nlmsg_len: 0,
            nlmsg_type: msg_type,
            nlmsg_flags: NetlinkMessageFlags::REQUEST,
            nlmsg_seq: 7,
            nlmsg_pid: 0,
        }
    }

    /// `ip addr add <addr>/<prefix> dev <ifindex>`, as it reaches the kernel.
    fn newaddr_request(ifindex: u32, prefixlen: u8, attr_type: u16, addr: &[u8]) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.push_ext(header(NetlinkMessageType::NewAddr.into()));
        msg.push_ext(IfaceAddrMsg {
            ifa_family: if addr.len() == 4 { 2 } else { 10 },
            ifa_prefixlen: prefixlen,
            ifa_flags: 0,
            ifa_scope: 0,
            ifa_index: ifindex,
        });
        let mut attrs = Vec::new();
        push_rtattr_bytes(&mut attrs, attr_type, addr);
        msg.append(&mut attrs);
        msg.align4();
        msg.set_ext(0, msg.len() as u32);
        msg
    }

    /// `ip route add <dst>/<len> via <gw> dev <oif>`.
    fn newroute_request(dst_len: u8, dst: Option<&[u8]>, gw: Option<&[u8]>, oif: u32) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.push_ext(header(NetlinkMessageType::NewRoute.into()));
        msg.push_ext(RouteMsg {
            rtm_family: if dst.map(|d| d.len()) == Some(16) {
                10
            } else {
                2
            },
            rtm_dst_len: dst_len,
            rtm_src_len: 0,
            rtm_tos: 0,
            rtm_table: 254,
            rtm_protocol: 4,
            rtm_scope: 0,
            rtm_type: 1,
            rtm_flags: 0,
        });
        let mut attrs = Vec::new();
        if let Some(d) = dst {
            push_rtattr_bytes(&mut attrs, RTA_DST, d);
        }
        if let Some(g) = gw {
            push_rtattr_bytes(&mut attrs, RTA_GATEWAY, g);
        }
        if oif != 0 {
            push_rtattr_u32(&mut attrs, RTA_OIF, oif);
        }
        msg.align4();
        msg.append(&mut attrs);
        msg.align4();
        msg.set_ext(0, msg.len() as u32);
        msg
    }

    // ---- the prefix length that took the kernel down ---------------------

    /// The panic. `ifa_prefixlen`/`rtm_dst_len` are one byte of the buffer a
    /// process hands to `sendmsg`, and smoltcp's `Ipv4Cidr::new` does not
    /// return an error for an impossible prefix — it indexes a one-element
    /// array and brings the machine down. Linux answers `EINVAL`.
    #[test]
    fn a_prefix_longer_than_the_address_is_refused_not_fatal() {
        for bad in [33u8, 64, 128, 255] {
            assert_eq!(cidr_from_parts(IpAddress::v4(10, 0, 0, 1), bad), None);
        }
        for bad in [129u8, 200, 255] {
            assert_eq!(
                cidr_from_parts(IpAddress::v6(0xfe80, 0, 0, 0, 0, 0, 0, 1), bad),
                None
            );
        }
    }

    #[test]
    fn every_prefix_an_address_can_carry_is_accepted() {
        for good in [0u8, 1, 8, 24, 31, 32] {
            assert!(cidr_from_parts(IpAddress::v4(10, 0, 0, 1), good).is_some());
        }
        for good in [0u8, 64, 127, 128] {
            assert!(cidr_from_parts(IpAddress::v6(0xfe80, 0, 0, 0, 0, 0, 0, 1), good).is_some());
        }
    }

    /// The bound belongs to the address family, not to the message: a v4
    /// address with a /64 is the v6 bound leaking onto a v4 address.
    #[test]
    fn the_bound_is_the_one_of_the_address_family() {
        assert!(cidr_from_parts(IpAddress::v4(10, 0, 0, 1), 64).is_none());
        assert!(cidr_from_parts(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 64).is_some());
    }

    /// The same byte reaches the kernel through `RTM_NEWADDR` and through
    /// `RTM_NEWROUTE`; both used to hand it over unchecked.
    #[test]
    fn an_address_message_with_an_impossible_prefix_is_dropped() {
        let msg = newaddr_request(2, 33, IFA_LOCAL, &[10, 0, 0, 5]);
        assert_eq!(parse_ifaddr_cidr(&msg), None);
    }

    #[test]
    fn a_route_message_with_an_impossible_prefix_is_dropped() {
        let msg = newroute_request(33, Some(&[10, 0, 0, 0]), None, 2);
        assert!(parse_route_request(&msg).is_none());
        // Also on the branch that carries no destination at all.
        let msg = newroute_request(99, None, Some(&[192, 168, 1, 1]), 2);
        assert!(parse_route_request(&msg).is_none());
    }

    // ---- what a well-formed request parses to ----------------------------

    #[test]
    fn an_address_request_round_trips_through_the_attribute_walk() {
        let msg = newaddr_request(3, 24, IFA_LOCAL, &[192, 168, 1, 50]);
        let (ifindex, cidr) = parse_ifaddr_cidr(&msg).unwrap();
        assert_eq!(ifindex, 3);
        assert_eq!(
            cidr,
            IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                Ipv4Address::new(192, 168, 1, 50),
                24
            ))
        );
    }

    /// `IFA_ADDRESS` and `IFA_LOCAL` both name the address; `ip` sends one or
    /// the other depending on the interface.
    #[test]
    fn either_address_attribute_is_accepted() {
        let a = parse_ifaddr_cidr(&newaddr_request(1, 24, IFA_ADDRESS, &[10, 1, 2, 3]));
        let b = parse_ifaddr_cidr(&newaddr_request(1, 24, IFA_LOCAL, &[10, 1, 2, 3]));
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn an_ipv6_address_request_parses_as_ipv6() {
        let addr = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1u8];
        let (ifindex, cidr) = parse_ifaddr_cidr(&newaddr_request(4, 64, IFA_LOCAL, &addr)).unwrap();
        assert_eq!(ifindex, 4);
        assert!(matches!(cidr, IpCidr::Ipv6(v6) if v6.prefix_len() == 64));
    }

    /// A message with no address attribute names no address, so there is
    /// nothing to add or remove.
    #[test]
    fn an_address_request_with_no_address_is_dropped() {
        let mut msg = Vec::new();
        msg.push_ext(header(NetlinkMessageType::NewAddr.into()));
        msg.push_ext(IfaceAddrMsg {
            ifa_family: 2,
            ifa_prefixlen: 24,
            ifa_flags: 0,
            ifa_scope: 0,
            ifa_index: 1,
        });
        msg.set_ext(0, msg.len() as u32);
        assert_eq!(parse_ifaddr_cidr(&msg), None);
    }

    /// Truncated before the `ifaddrmsg` even starts.
    #[test]
    fn a_message_too_short_to_hold_its_own_body_is_dropped() {
        let short = alloc::vec![0u8; NLMSG_HDR + 4];
        assert_eq!(parse_ifaddr_cidr(&short), None);
        assert!(parse_route_request(&short).is_none());
    }

    #[test]
    fn a_route_request_carries_its_destination_gateway_and_interface() {
        let msg = newroute_request(24, Some(&[10, 9, 0, 0]), Some(&[10, 9, 0, 1]), 7);
        let (rtm, dst, gw, oif) = parse_route_request(&msg).unwrap();
        assert_eq!(rtm.rtm_dst_len, 24);
        assert_eq!(
            dst,
            IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                Ipv4Address::new(10, 9, 0, 0),
                24
            ))
        );
        assert_eq!(gw, Some(IpAddress::Ipv4(Ipv4Address::new(10, 9, 0, 1))));
        assert_eq!(oif, 7);
    }

    /// The default route: no `RTA_DST` and a zero prefix. This is the shape
    /// `udhcpc` installs, and the one whose rendering has already gone wrong
    /// twice.
    #[test]
    fn a_default_route_has_no_destination_attribute() {
        let msg = newroute_request(0, None, Some(&[192, 168, 1, 1]), 2);
        let (_rtm, dst, gw, oif) = parse_route_request(&msg).unwrap();
        assert_eq!(dst.prefix_len(), 0);
        assert_eq!(gw, Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))));
        assert_eq!(oif, 2);
    }

    // ---- the attribute walk itself ---------------------------------------

    /// `rta_len` is attacker-controlled. Below the header it names a negative
    /// payload, and past the end it names bytes the message does not have;
    /// either one used to be a slice out of bounds, which in a kernel is not
    /// an exception.
    #[test]
    fn an_attribute_length_out_of_range_stops_the_walk() {
        for bad in [0u16, 1, 3, 0xffff] {
            let mut msg = newaddr_request(1, 24, IFA_LOCAL, &[10, 0, 0, 1]);
            // Overwrite the attribute header's length, which sits right after
            // the fixed part of the message.
            let at = NLMSG_HDR + size_of::<IfaceAddrMsg>();
            msg[at..at + 2].copy_from_slice(&bad.to_ne_bytes());
            assert_eq!(parse_ifaddr_cidr(&msg), None, "rta_len {}", bad);
        }
    }

    /// An attribute whose payload is not an address is not an address. A
    /// 5-byte `IFA_LOCAL` would otherwise be copied into a 4- or 16-byte
    /// array.
    #[test]
    fn an_address_attribute_of_the_wrong_width_is_ignored() {
        for width in [0usize, 1, 3, 5, 8, 15, 17] {
            let payload = alloc::vec![1u8; width];
            let msg = newaddr_request(1, 24, IFA_LOCAL, &payload);
            assert_eq!(parse_ifaddr_cidr(&msg), None, "payload of {} bytes", width);
        }
    }

    /// The walk advances by `RTA_ALIGN(rta_len)`, so an attribute with an odd
    /// payload does not push the next one off its alignment. A route message
    /// carries three attributes in a row, and the gateway is behind the
    /// destination.
    #[test]
    fn an_attribute_is_found_behind_one_of_odd_length() {
        let mut msg = Vec::new();
        msg.push_ext(header(NetlinkMessageType::NewRoute.into()));
        msg.push_ext(RouteMsg {
            rtm_family: 2,
            rtm_dst_len: 24,
            rtm_src_len: 0,
            rtm_tos: 0,
            rtm_table: 254,
            rtm_protocol: 4,
            rtm_scope: 0,
            rtm_type: 1,
            rtm_flags: 0,
        });
        let mut attrs = Vec::new();
        // RTA_PRIORITY-ish filler of 3 bytes: not a multiple of 4.
        push_rtattr_bytes(&mut attrs, 6, &[1, 2, 3]);
        push_rtattr_bytes(&mut attrs, RTA_DST, &[10, 9, 0, 0]);
        push_rtattr_u32(&mut attrs, RTA_OIF, 9);
        msg.append(&mut attrs);
        msg.align4();
        msg.set_ext(0, msg.len() as u32);
        let (_rtm, dst, _gw, oif) = parse_route_request(&msg).unwrap();
        assert_eq!(
            dst.address(),
            IpAddress::Ipv4(Ipv4Address::new(10, 9, 0, 0))
        );
        assert_eq!(oif, 9);
    }

    /// The message sits at an NLMSG_ALIGN(4) offset inside a byte buffer the
    /// allocator only aligned to 1, so its `u32` fields can land anywhere.
    /// Taking a `&struct` to that was undefined behaviour and a fault on the
    /// architectures that check.
    #[test]
    fn a_message_at_an_odd_address_parses_the_same() {
        let msg = newaddr_request(3, 24, IFA_LOCAL, &[192, 168, 1, 50]);
        let want = parse_ifaddr_cidr(&msg);
        assert!(want.is_some());
        for skew in 1..4usize {
            let mut skewed = alloc::vec![0u8; skew];
            skewed.extend_from_slice(&msg);
            assert_eq!(parse_ifaddr_cidr(&skewed[skew..]), want, "skew {}", skew);
        }
    }

    // ---- what the builders write -----------------------------------------

    /// `rta_len` counts the header and the payload and is NOT rounded up: the
    /// padding is between attributes, not inside one. Written by
    /// `push_rtattr_bytes` and read by the two walks above — nothing else
    /// keeps the two in step, so the round trip is the test that matters.
    #[test]
    fn an_attribute_declares_its_header_plus_its_payload() {
        let mut attrs = Vec::new();
        push_rtattr_bytes(&mut attrs, RTA_DST, &[1, 2, 3, 4]);
        assert_eq!(u16::from_ne_bytes([attrs[0], attrs[1]]), 8);
        assert_eq!(u16::from_ne_bytes([attrs[2], attrs[3]]), RTA_DST);
        assert_eq!(&attrs[4..8], &[1, 2, 3, 4]);

        let mut odd = Vec::new();
        push_rtattr_bytes(&mut odd, 6, &[9, 9, 9]);
        assert_eq!(u16::from_ne_bytes([odd[0], odd[1]]), 7);
        // No padding of its own: the next attribute aligns before writing.
        assert_eq!(odd.len(), 7);
        push_rtattr_bytes(&mut odd, RTA_OIF, &[0, 0, 0, 0]);
        assert_eq!(odd.len(), 8 + 8);
    }

    #[test]
    fn a_built_message_declares_its_whole_length() {
        let msg = newaddr_request(3, 24, IFA_LOCAL, &[192, 168, 1, 50]);
        let declared = u32::from_ne_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
        assert_eq!(declared, msg.len());
        assert_eq!(msg.len() % 4, 0);
    }

    #[test]
    fn aligning_pads_with_zeroes_and_leaves_an_aligned_buffer_alone() {
        let mut v = alloc::vec![7u8; 5];
        v.align4();
        assert_eq!(v, alloc::vec![7, 7, 7, 7, 7, 0, 0, 0]);
        let before = v.clone();
        v.align4();
        assert_eq!(v, before);
    }

    /// The bound of [`read_struct`] is the one thing between a short message
    /// and a read off the end of the buffer, and from outside a parser it
    /// cannot be seen: a truncated message answers `None` either way, once
    /// because the struct did not fit and once because the garbage it read
    /// named no address. So it is asked here directly.
    #[test]
    fn a_struct_that_does_not_fit_is_not_read() {
        assert!(read_struct::<RouteAttr>(&[1, 2, 3], 0).is_none());
        assert!(read_struct::<RouteAttr>(&[1, 2, 3, 4], 0).is_some());
        // The offset counts too: four bytes are not four bytes from byte two.
        assert!(read_struct::<RouteAttr>(&[1, 2, 3, 4], 2).is_none());
        // An offset past the end must not wrap round to a huge length.
        assert!(read_struct::<RouteAttr>(&[1, 2, 3, 4], 99).is_none());
        assert!(read_struct::<NetlinkMessageHeader>(&[0u8; 15], 0).is_none());
        assert!(read_struct::<NetlinkMessageHeader>(&[0u8; 16], 0).is_some());
    }

    /// A zeroed `ifa_prefixlen` means the whole address here: `ip addr add
    /// 10.0.0.5 dev eth0`, with no prefix written, is how an address is most
    /// often added, and the field arrives at 0. (Linux takes a literal /0;
    /// nothing configures one, and reading it as /32 is what makes the
    /// prefix-less form work.)
    #[test]
    fn a_zero_prefix_means_the_whole_address() {
        let (_, cidr) =
            parse_ifaddr_cidr(&newaddr_request(1, 0, IFA_LOCAL, &[10, 0, 0, 5])).unwrap();
        assert_eq!(cidr.prefix_len(), 32);
        let addr = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1u8];
        let (_, cidr) = parse_ifaddr_cidr(&newaddr_request(1, 0, IFA_LOCAL, &addr)).unwrap();
        assert_eq!(cidr.prefix_len(), 128);
    }

    /// `set_ext` past the end grows the buffer rather than panicking, which
    /// is what lets a builder reserve the length field before it knows it.
    #[test]
    fn writing_past_the_end_grows_the_buffer() {
        let mut v: Vec<u8> = Vec::new();
        v.set_ext(0, 0x1122_3344u32);
        assert_eq!(v.len(), 4);
        assert_eq!(u32::from_ne_bytes([v[0], v[1], v[2], v[3]]), 0x1122_3344);
    }

    // ---- the rest ---------------------------------------------------------

    /// A broadcast address is the host part set to ones. Both ends of the
    /// range are shifts a `u32` cannot take.
    #[test]
    fn the_broadcast_address_covers_both_ends_of_the_prefix() {
        let a = Ipv4Address::new(192, 168, 1, 50);
        assert_eq!(ipv4_broadcast(a, 24), Ipv4Address::new(192, 168, 1, 255));
        assert_eq!(ipv4_broadcast(a, 16), Ipv4Address::new(192, 168, 255, 255));
        // A /32 is a single host: it is its own broadcast.
        assert_eq!(ipv4_broadcast(a, 32), a);
        // A /0 is everything.
        assert_eq!(ipv4_broadcast(a, 0), Ipv4Address::new(255, 255, 255, 255));
        // Out of range must not shift by a negative amount.
        assert_eq!(ipv4_broadcast(a, 33), a);
    }

    /// The reply must echo the port id the request carried, because that is
    /// what musl and fastfetch filter on; a request that left it at 0 (it has
    /// not called `bind`) gets the socket's own.
    #[test]
    fn the_reply_echoes_the_port_id_of_the_request() {
        let mut req = header(NetlinkMessageType::GetLink.into());
        req.nlmsg_pid = 4321;
        assert_eq!(reply_nl_pid(&req, 99), 4321);
        req.nlmsg_pid = 0;
        assert_eq!(reply_nl_pid(&req, 99), 99);
    }

    /// The reply queue is bounded, so a socket nobody reads cannot grow until
    /// the kernel heap is gone. What it drops is the oldest message.
    #[test]
    fn the_reply_queue_is_bounded() {
        let mut q: Vec<Vec<u8>> = Vec::new();
        for i in 0..NETLINK_RX_QUEUE_MAX + 10 {
            push_netlink_rx(&mut q, alloc::vec![i as u8]);
        }
        assert_eq!(q.len(), NETLINK_RX_QUEUE_MAX);
        assert_eq!(q.last().unwrap()[0], (NETLINK_RX_QUEUE_MAX + 9) as u8);
        assert_eq!(q[0][0], 10u8);
    }
}
