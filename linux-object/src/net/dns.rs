//! Hostname resolver: `/etc/hosts` first, then DNS (A / AAAA) via smoltcp UDP
//! and `/etc/resolv.conf`.

use crate::error::{LxError, LxResult};
use crate::net::{drain_net_poll, local_udp_endpoint_for, UDP_METADATA_BUF};
use alloc::{sync::Arc, vec, vec::Vec};
use core::time::Duration;
use rcore_fs::vfs::INode;
use smoltcp::socket::{UdpPacketMetadata, UdpSocket, UdpSocketBuffer};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address};
use zcore_drivers::net::get_sockets;

const DNS_PORT: u16 = 53;
const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const QCLASS_IN: u16 = 1;

/// One address returned by [`resolve`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DnsResultEntry {
    pub family: u16,
    pub _pad: u16,
    pub addr: [u8; 16],
}

impl DnsResultEntry {
    pub fn from_ip(ip: IpAddress) -> Self {
        let mut addr = [0u8; 16];
        let family = match ip {
            IpAddress::Ipv4(v4) => {
                addr[..4].copy_from_slice(&v4.0);
                2u16
            }
            IpAddress::Ipv6(v6) => {
                addr.copy_from_slice(&v6.0);
                10u16
            }
            IpAddress::Unspecified | _ => 0,
        };
        Self {
            family,
            _pad: 0,
            addr,
        }
    }
}

/// Address family filter for [`resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsFamily {
    Unspec,
    V4,
    V6,
}

impl DnsFamily {
    pub fn from_usize(v: usize) -> Self {
        match v {
            2 => DnsFamily::V4,
            10 => DnsFamily::V6,
            _ => DnsFamily::Unspec,
        }
    }
}

/// Resolve `hostname` using `/etc/hosts`, then nameservers from `/etc/resolv.conf`.
pub fn resolve(
    root: &Arc<dyn INode>,
    hostname: &str,
    family: DnsFamily,
) -> LxResult<Vec<IpAddress>> {
    if hostname.is_empty() || hostname.len() > 253 {
        return Err(LxError::EINVAL);
    }

    let hosts = lookup_hosts(root, hostname, family);
    if !hosts.is_empty() {
        return Ok(hosts);
    }

    let servers = read_nameservers(root);
    if servers.is_empty() {
        return Err(LxError::ENOENT);
    }

    let want_v4 = matches!(family, DnsFamily::Unspec | DnsFamily::V4);
    let want_v6 = matches!(family, DnsFamily::Unspec | DnsFamily::V6);
    let mut out = Vec::new();

    for server in servers {
        if want_v4 {
            if let Ok(addrs) = query_at(server, hostname, QTYPE_A) {
                out.extend(addrs);
            }
        }
        if want_v6 {
            if let Ok(addrs) = query_at(server, hostname, QTYPE_AAAA) {
                out.extend(addrs);
            }
        }
        if !out.is_empty() {
            break;
        }
    }

    if out.is_empty() {
        Err(LxError::ENOENT)
    } else {
        Ok(out)
    }
}

fn lookup_hosts(root: &Arc<dyn INode>, hostname: &str, family: DnsFamily) -> Vec<IpAddress> {
    let Ok(inode) = root.lookup("/etc/hosts") else {
        return Vec::new();
    };
    let Ok(meta) = inode.metadata() else {
        return Vec::new();
    };
    let size = meta.size;
    if size == 0 || size > 65536 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size];
    if inode.read_at(0, &mut buf).unwrap_or(0) == 0 {
        return Vec::new();
    }
    let text = core::str::from_utf8(&buf).unwrap_or("");
    let want_v4 = matches!(family, DnsFamily::Unspec | DnsFamily::V4);
    let want_v6 = matches!(family, DnsFamily::Unspec | DnsFamily::V6);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(ip_raw) = parts.next() else { continue };
        let ip = if let Some(v4) = parse_ipv4(ip_raw) {
            if !want_v4 {
                continue;
            }
            IpAddress::Ipv4(v4)
        } else if let Some(v6) = parse_ipv6(ip_raw) {
            if !want_v6 {
                continue;
            }
            IpAddress::Ipv6(v6)
        } else {
            continue;
        };
        for alias in parts {
            if alias.eq_ignore_ascii_case(hostname) {
                if !out.contains(&ip) {
                    out.push(ip);
                }
                break;
            }
        }
    }
    out
}

fn read_nameservers(root: &Arc<dyn INode>) -> Vec<IpAddress> {
    let Ok(inode) = root.lookup("/etc/resolv.conf") else {
        return fallback_nameservers();
    };
    let Ok(meta) = inode.metadata() else {
        return fallback_nameservers();
    };
    let size = meta.size;
    if size == 0 || size > 8192 {
        return fallback_nameservers();
    }
    let mut buf = vec![0u8; size];
    if inode.read_at(0, &mut buf).unwrap_or(0) == 0 {
        return fallback_nameservers();
    }
    let text = core::str::from_utf8(&buf).unwrap_or("");
    let mut servers = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if !line.starts_with("nameserver") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let _ = parts.next();
        let Some(raw) = parts.next() else { continue };
        let scoped = raw.split('%').next().unwrap_or(raw);
        if let Some(v4) = parse_ipv4(scoped) {
            servers.push(IpAddress::Ipv4(v4));
        } else if let Some(v6) = parse_ipv6(scoped) {
            if v6.is_link_local() && !raw.contains('%') {
                continue;
            }
            servers.push(IpAddress::Ipv6(v6));
        }
    }
    if servers.is_empty() {
        return fallback_nameservers();
    }
    for fb in fallback_nameservers() {
        if !servers.contains(&fb) {
            servers.push(fb);
        }
    }
    servers
}

fn fallback_nameservers() -> Vec<IpAddress> {
    vec![
        IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)),
        IpAddress::Ipv4(Ipv4Address::new(1, 1, 1, 1)),
    ]
}

fn parse_ipv4(s: &str) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut parts = s.split('.');
    for o in &mut octets {
        *o = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Ipv4Address::from_bytes(&octets))
}

fn parse_ipv6(s: &str) -> Option<Ipv6Address> {
    // Minimal parser: hex groups separated by ':', optional '::' compression.
    // `str::split(':')` yields consecutive empties for leading/trailing `::`
    // (`::1` → ["", "", "1"], `a::` → ["a", "", ""], `::` → ["", "", ""]).
    let s = s.split('%').next().unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let mut groups = [0u16; 8];
    let mut count = 0usize;
    let mut double_off = None;
    let mut empty_run = 0usize;
    for part in s.split(':') {
        if part.is_empty() {
            empty_run += 1;
            if empty_run == 1 {
                // First empty starts `::` compression (at most one double-colon).
                if double_off.is_some() {
                    return None;
                }
                double_off = Some(count);
            } else if empty_run == 2 {
                // Second consecutive empty: leading or trailing `::` artifact.
            } else if empty_run == 3 && count == 0 && double_off == Some(0) {
                // Bare `::` yields three empties.
            } else {
                return None;
            }
            continue;
        }
        if empty_run == 3 {
            return None; // e.g. `:::1`
        }
        // Lone leading ':' (`:1`): one empty then a group without a second empty.
        if empty_run == 1 && count == 0 && double_off == Some(0) {
            return None;
        }
        empty_run = 0;
        let value = u16::from_str_radix(part, 16).ok()?;
        if count >= 8 {
            return None;
        }
        groups[count] = value;
        count += 1;
    }
    // Lone trailing ':' (`1:`) is a single empty at end — not valid `::`.
    if empty_run == 1 && count > 0 {
        return None;
    }
    let tail = if let Some(at) = double_off {
        let head = at;
        let rest = count - at;
        let zeros = 8usize.checked_sub(rest + head)?;
        if zeros == 0 {
            return None;
        }
        let mut out = [0u16; 8];
        out[..head].copy_from_slice(&groups[..head]);
        let tail_start = 8 - rest;
        out[tail_start..].copy_from_slice(&groups[at..count]);
        out
    } else if count == 8 {
        groups
    } else {
        return None;
    };
    let mut bytes = [0u8; 16];
    for (i, g) in tail.iter().enumerate() {
        bytes[i * 2] = (g >> 8) as u8;
        bytes[i * 2 + 1] = (g & 0xff) as u8;
    }
    Some(Ipv6Address::from_bytes(&bytes))
}

fn query_at(server: IpAddress, name: &str, qtype: u16) -> LxResult<Vec<IpAddress>> {
    let id = (super::rand() & 0xffff) as u16;
    let query = build_query(name, id, qtype)?;
    let (local, remote) = match server {
        IpAddress::Ipv4(v4) => (
            local_udp_endpoint_for(IpAddress::Ipv4(v4)),
            IpEndpoint::new(IpAddress::Ipv4(v4), DNS_PORT),
        ),
        IpAddress::Ipv6(v6) => (
            local_udp_endpoint_for(IpAddress::Ipv6(v6)),
            IpEndpoint::new(IpAddress::Ipv6(v6), DNS_PORT),
        ),
        IpAddress::Unspecified | _ => return Err(LxError::EINVAL),
    };
    let reply = udp_exchange(local, remote, &query, id)?;
    parse_addresses(&reply, qtype)
}

fn build_query(name: &str, id: u16, qtype: u16) -> LxResult<Vec<u8>> {
    let mut qname = Vec::new();
    encode_qname(name, &mut qname)?;
    let mut out = Vec::with_capacity(12 + qname.len() + 4);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x00]); // RD=1
    out.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // AN/NS/AR
    out.extend_from_slice(&qname);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&QCLASS_IN.to_be_bytes());
    Ok(out)
}

fn encode_qname(name: &str, out: &mut Vec<u8>) -> LxResult {
    if name.is_empty() {
        out.push(0);
        return Ok(());
    }
    // A fully-qualified name ends in a dot (`example.com.`), which `split`
    // turns into a trailing empty label. That is legal syntax -- it appears in
    // `resolv.conf` search lists and in anything a user typed with the root
    // spelled out -- and rejecting it made the whole lookup fail with EINVAL
    // rather than resolve. The dot IS the root label the encoding already adds
    // at the end, so there is nothing else to do with it.
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() {
        out.push(0);
        return Ok(());
    }
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(LxError::EINVAL);
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(())
}

fn spin_ms(ms: u64) {
    let deadline = kernel_hal::timer::timer_now() + Duration::from_millis(ms);
    while kernel_hal::timer::timer_now() < deadline {
        core::hint::spin_loop();
    }
}

fn udp_exchange(
    local: IpEndpoint,
    remote: IpEndpoint,
    query: &[u8],
    expect_id: u16,
) -> LxResult<Vec<u8>> {
    let rx_buffer = UdpSocketBuffer::new(
        vec![UdpPacketMetadata::EMPTY; UDP_METADATA_BUF],
        vec![0u8; 512],
    );
    let tx_buffer = UdpSocketBuffer::new(
        vec![UdpPacketMetadata::EMPTY; UDP_METADATA_BUF],
        vec![0u8; 512],
    );
    let socket = UdpSocket::new(rx_buffer, tx_buffer);
    let sockets = get_sockets();
    let mut set = sockets.lock();
    if super::smoltcp_socket_count(&set) >= super::MAX_SMOLTCIP_SOCKETS {
        return Err(LxError::ENOMEM);
    }
    let handle = set.add(socket);
    drop(set);
    {
        let mut set = sockets.lock();
        let mut sock = set.get::<UdpSocket>(handle);
        sock.bind(local).map_err(|_| LxError::EINVAL)?;
        sock.send_slice(query, remote).map_err(|_| LxError::EIO)?;
    }

    let mut buf = [0u8; 512];
    for round in 0..32 {
        drain_net_poll(8);
        if round == 0 {
            kernel_hal::deferred_job::drain_deferred_jobs();
        }
        let mut set = sockets.lock();
        let mut sock = set.get::<UdpSocket>(handle);
        if sock.can_recv() {
            if let Ok((n, _)) = sock.recv_slice(&mut buf) {
                if n >= 2 {
                    let id = u16::from_be_bytes([buf[0], buf[1]]);
                    if id == expect_id {
                        drop(sock);
                        drop(set);
                        sockets.lock().remove(handle);
                        return Ok(buf[..n].to_vec());
                    }
                }
            }
        }
        drop(sock);
        drop(set);
        spin_ms(50);
    }
    sockets.lock().remove(handle);
    Err(LxError::ETIMEDOUT)
}

fn skip_name(data: &[u8], mut off: usize) -> Option<usize> {
    let mut jumps = 0;
    loop {
        if off >= data.len() {
            return None;
        }
        let len = data[off];
        if len == 0 {
            return Some(off + 1);
        }
        if len & 0xc0 == 0xc0 {
            if off + 1 >= data.len() {
                return None;
            }
            return Some(off + 2);
        }
        off += 1 + len as usize;
        jumps += 1;
        if jumps > 128 {
            return None;
        }
    }
}

fn parse_addresses(data: &[u8], qtype: u16) -> LxResult<Vec<IpAddress>> {
    if data.len() < 12 {
        return Err(LxError::EINVAL);
    }
    let rcode = data[3] & 0x0f;
    if rcode != 0 {
        return Err(LxError::EIO);
    }
    let qd = u16::from_be_bytes([data[4], data[5]]) as usize;
    let an = u16::from_be_bytes([data[6], data[7]]) as usize;
    let mut off = 12usize;
    for _ in 0..qd {
        off = skip_name(data, off).ok_or(LxError::EINVAL)?;
        off += 4;
        if off > data.len() {
            return Err(LxError::EINVAL);
        }
    }
    let mut addrs = Vec::new();
    for _ in 0..an {
        // `an` is the sender's word for how many records follow, and a packet
        // that overstates it -- by accident or on purpose -- runs this loop
        // past the end. The two bounds checks below already treat that as
        // "the answers stop here" and keep what was parsed; this one used to
        // throw the whole packet away with EINVAL instead, so the SAME lie
        // failed the lookup or not depending on exactly which byte the packet
        // ended on. The records already read were well formed and matched the
        // question, so they are worth as much as any: stop, do not discard.
        off = match skip_name(data, off) {
            Some(next) => next,
            None => break,
        };
        if off + 10 > data.len() {
            break;
        }
        let rtype = u16::from_be_bytes([data[off], data[off + 1]]);
        let rdlen = u16::from_be_bytes([data[off + 8], data[off + 9]]) as usize;
        off += 10;
        if off + rdlen > data.len() {
            break;
        }
        let rdata = &data[off..off + rdlen];
        if rtype == qtype {
            match qtype {
                QTYPE_A if rdlen == 4 => {
                    addrs.push(IpAddress::Ipv4(Ipv4Address::from_bytes(rdata)));
                }
                QTYPE_AAAA if rdlen == 16 => {
                    addrs.push(IpAddress::Ipv6(Ipv6Address::from_bytes(rdata)));
                }
                _ => {}
            }
        }
        off += rdlen;
    }
    if addrs.is_empty() {
        Err(LxError::ENOENT)
    } else {
        Ok(addrs)
    }
}

#[cfg(test)]
mod dns_tests {
    //! Everything here reads bytes that came off the network or out of a
    //! config file, so it is the one place in the resolver that a hostile or
    //! merely broken answer reaches first. The failure modes are quiet:
    //! a length read past the end of the packet, a `::` expanded to the wrong
    //! number of zero groups, an address taken from the wrong record.

    use super::*;

    /// A DNS response: header, one question, then the answers as given.
    fn response(rcode: u8, qname: &[u8], answers: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0x12, 0x34]); // id
        v.push(0x81); // QR=1, RD=1
        v.push(0x80 | rcode); // RA + rcode
        v.extend_from_slice(&[0x00, 0x01]); // QDCOUNT
        v.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // NS/AR
        v.extend_from_slice(qname);
        v.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // QTYPE=A QCLASS=IN
        for a in answers {
            v.extend_from_slice(a);
        }
        v
    }

    /// One resource record, with the name as a compression pointer to offset
    /// 12 -- which is what a real server sends.
    fn record(rtype: u16, rdata: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0xC0, 0x0C]); // pointer to the question's name
        v.extend_from_slice(&rtype.to_be_bytes());
        v.extend_from_slice(&QCLASS_IN.to_be_bytes());
        v.extend_from_slice(&[0, 0, 0, 60]); // ttl
        v.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        v.extend_from_slice(rdata);
        v
    }

    fn qname(name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        encode_qname(name, &mut v).unwrap();
        v
    }

    // ── the address literals in resolv.conf and /etc/hosts ──────────────────

    #[test]
    fn an_ipv4_literal_needs_exactly_four_octets() {
        assert_eq!(
            parse_ipv4("192.168.1.1"),
            Some(Ipv4Address::new(192, 168, 1, 1))
        );
        assert_eq!(parse_ipv4("0.0.0.0"), Some(Ipv4Address::new(0, 0, 0, 0)));
        assert_eq!(
            parse_ipv4("255.255.255.255"),
            Some(Ipv4Address::new(255, 255, 255, 255))
        );
        assert_eq!(parse_ipv4("1.2.3"), None, "three octets is not an address");
        assert_eq!(parse_ipv4("1.2.3.4.5"), None, "five is not either");
        assert_eq!(parse_ipv4("1.2.3.256"), None, "256 does not fit an octet");
        assert_eq!(parse_ipv4("1.2.3.x"), None);
        assert_eq!(parse_ipv4(""), None);
    }

    #[test]
    fn an_ipv6_literal_without_compression() {
        let a = parse_ipv6("2001:0db8:0000:0000:0000:0000:0000:0001").unwrap();
        assert_eq!(a, Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        assert_eq!(
            parse_ipv6("1:2:3:4:5:6:7"),
            None,
            "seven groups is not enough"
        );
        assert_eq!(parse_ipv6("1:2:3:4:5:6:7:8:9"), None, "nine is too many");
    }

    #[test]
    fn the_double_colon_expands_to_the_zeros_it_stands_for() {
        // This is the whole of the compressed form, and getting the count
        // wrong puts every group after it in the wrong place -- an address
        // that parses cleanly and points somewhere else entirely.
        assert_eq!(
            parse_ipv6("2001:db8::1"),
            Some(Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))
        );
        assert_eq!(
            parse_ipv6("::1"),
            Some(Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 1)),
            "the loopback address"
        );
        assert_eq!(
            parse_ipv6("fe80::"),
            Some(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0))
        );
        assert_eq!(
            parse_ipv6("::"),
            Some(Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 0)),
            "the unspecified address"
        );
        assert_eq!(
            parse_ipv6("1:2:3::7:8"),
            Some(Ipv6Address::new(1, 2, 3, 0, 0, 0, 7, 8)),
            "the zeros go where the :: was, not at the end"
        );
    }

    #[test]
    fn a_double_colon_that_stands_for_nothing_is_refused() {
        // `::` must cover at least one group. `1:2:3:4:5:6:7::8` names nine.
        assert_eq!(parse_ipv6("1:2:3:4:5:6:7:8::"), None);
        assert_eq!(parse_ipv6("1:2:3:4:5:6:7::8"), None);
    }

    #[test]
    fn a_malformed_ipv6_literal_is_refused_rather_than_half_parsed() {
        assert_eq!(parse_ipv6(":::1"), None, "three colons");
        assert_eq!(parse_ipv6(":1"), None, "a lone leading colon");
        assert_eq!(parse_ipv6("1:"), None, "a lone trailing colon");
        assert_eq!(
            parse_ipv6("1::2::3"),
            None,
            "two compressions are ambiguous"
        );
        assert_eq!(parse_ipv6(""), None);
        assert_eq!(parse_ipv6("gggg::1"), None, "not hexadecimal");
    }

    #[test]
    fn a_scope_suffix_is_dropped_rather_than_refused() {
        // Link-local addresses come with `%iface` attached, and a nameserver
        // line that carries one has to resolve to the address, not to nothing.
        assert_eq!(
            parse_ipv6("fe80::1%eth0"),
            Some(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1))
        );
    }

    // ── the query that goes out ─────────────────────────────────────────────

    #[test]
    fn a_name_is_encoded_as_length_prefixed_labels() {
        assert_eq!(qname("www.example.com"), b"\x03www\x07example\x03com\x00");
        assert_eq!(qname(""), b"\x00", "the empty name is the root");
    }

    #[test]
    fn a_fully_qualified_name_keeps_its_meaning_and_loses_its_dot() {
        // `example.com.` is legal and ordinary -- it is how you spell a name
        // that must not have a search domain appended. `split('.')` turns the
        // trailing dot into an empty label, which used to be an outright
        // EINVAL: the lookup failed instead of resolving.
        assert_eq!(qname("example.com."), qname("example.com"));
        assert_eq!(qname("."), b"\x00", "the root, spelled out");
    }

    #[test]
    fn an_empty_or_oversized_label_is_refused() {
        // An empty label in the middle (`a..b`) has no encoding, and a label
        // over 63 bytes does not fit its length byte -- the top two bits are
        // the compression marker, so a longer one would read as a pointer.
        let mut out = Vec::new();
        assert!(matches!(
            encode_qname("a..b", &mut out),
            Err(LxError::EINVAL)
        ));
        let long = "x".repeat(64);
        assert!(matches!(
            encode_qname(&long, &mut out),
            Err(LxError::EINVAL)
        ));
        let just_fits = "x".repeat(63);
        let mut ok = Vec::new();
        assert!(
            encode_qname(&just_fits, &mut ok).is_ok(),
            "63 bytes must fit"
        );
        assert_eq!(ok[0], 63);
    }

    #[test]
    fn the_query_header_asks_for_recursion_and_one_question() {
        let q = build_query("example.com", 0xBEEF, QTYPE_A).unwrap();
        assert_eq!(&q[0..2], &[0xBE, 0xEF], "the id must go out as given");
        assert_eq!(
            q[2] & 0x01,
            0x01,
            "RD must be set or the server will not recurse"
        );
        assert_eq!(&q[4..6], &[0x00, 0x01], "exactly one question");
        assert_eq!(&q[6..12], &[0, 0, 0, 0, 0, 0], "no answers in a query");
        assert_eq!(&q[12..q.len() - 4], qname("example.com").as_slice());
        assert_eq!(&q[q.len() - 4..], &[0x00, 0x01, 0x00, 0x01], "A, IN");
        let q6 = build_query("example.com", 1, QTYPE_AAAA).unwrap();
        assert_eq!(&q6[q6.len() - 4..], &[0x00, 0x1C, 0x00, 0x01], "AAAA, IN");
    }

    // ── the answer that comes back ──────────────────────────────────────────

    #[test]
    fn a_name_is_skipped_label_by_label_to_the_root() {
        let data = b"\x03www\x07example\x03com\x00rest";
        assert_eq!(skip_name(data, 0), Some(17), "past the terminating zero");
        assert_eq!(skip_name(b"\x00", 0), Some(1), "the root is one byte");
    }

    #[test]
    fn a_compression_pointer_is_two_bytes_and_is_not_followed() {
        // Following pointers is how a resolver is made to loop for ever on a
        // packet that points at itself; this one only has to step over them.
        let data = [0xC0, 0x0C, 0xFF];
        assert_eq!(skip_name(&data, 0), Some(2));
        // A pointer whose second byte is off the end of the packet.
        assert_eq!(skip_name(&[0xC0], 0), None);
    }

    #[test]
    fn a_name_that_runs_off_the_end_of_the_packet_is_refused() {
        // A truncated packet claiming a 200-byte label: the parser must stop,
        // not read past the buffer.
        // 63 is the longest a label may be, and the top two bits of the
        // length byte are the compression marker -- so a genuine length that
        // overruns is at most 0x3F, and anything above 0xC0 is a pointer.
        assert_eq!(skip_name(b"\x3Fabc", 0), None, "a 63-byte label in 4 bytes");
        assert_eq!(skip_name(b"\x03ww", 0), None, "the label is cut short");
        assert_eq!(skip_name(b"", 0), None);
        assert_eq!(skip_name(b"\x03www", 5), None, "an offset past the end");
    }

    #[test]
    fn an_a_record_yields_its_address() {
        let pkt = response(
            0,
            &qname("example.com"),
            &[&record(QTYPE_A, &[93, 184, 216, 34])],
        );
        let got = parse_addresses(&pkt, QTYPE_A).unwrap();
        assert_eq!(
            got,
            alloc::vec![IpAddress::Ipv4(Ipv4Address::new(93, 184, 216, 34))]
        );
    }

    #[test]
    fn several_answers_all_come_back_in_order() {
        // A load-balanced name answers with every address it has, and a
        // resolver that stops at the first one never fails over.
        let pkt = response(
            0,
            &qname("example.com"),
            &[
                &record(QTYPE_A, &[1, 1, 1, 1]),
                &record(QTYPE_A, &[8, 8, 8, 8]),
            ],
        );
        let got = parse_addresses(&pkt, QTYPE_A).unwrap();
        assert_eq!(
            got,
            alloc::vec![
                IpAddress::Ipv4(Ipv4Address::new(1, 1, 1, 1)),
                IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8))
            ]
        );
    }

    #[test]
    fn a_cname_in_front_of_the_answer_is_stepped_over() {
        // The commonest real answer shape: the name is an alias, so the
        // packet carries a CNAME and then the A record it points at. Taking
        // the first record's rdata as an address would read four bytes of a
        // hostname as an IPv4 address.
        let cname = record(5, b"\x03www\x07example\x03com\x00");
        let a = record(QTYPE_A, &[203, 0, 113, 7]);
        let pkt = response(0, &qname("example.com"), &[&cname, &a]);
        let got = parse_addresses(&pkt, QTYPE_A).unwrap();
        assert_eq!(
            got,
            alloc::vec![IpAddress::Ipv4(Ipv4Address::new(203, 0, 113, 7))]
        );
    }

    #[test]
    fn an_aaaa_answer_is_sixteen_bytes_and_an_a_answer_is_four() {
        // The length is what tells them apart, and a record whose rdlen does
        // not match its type is malformed: taking it anyway would build an
        // address out of whatever followed.
        let v6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let pkt = response(0, &qname("example.com"), &[&record(QTYPE_AAAA, &v6)]);
        let got = parse_addresses(&pkt, QTYPE_AAAA).unwrap();
        assert_eq!(
            got,
            alloc::vec![IpAddress::Ipv6(Ipv6Address::new(
                0x2001, 0xdb8, 0, 0, 0, 0, 0, 1
            ))]
        );

        // Too short and too long are both malformed, and the long one is the
        // dangerous shape: an address built from the first four bytes of a
        // six-byte record silently drops what the rest of it said.
        for rdata in [&[1u8, 2, 3][..], &[1, 2, 3, 4, 5, 6][..]] {
            let wrong = response(0, &qname("example.com"), &[&record(QTYPE_A, rdata)]);
            assert!(
                matches!(parse_addresses(&wrong, QTYPE_A), Err(LxError::ENOENT)),
                "an A record of {} bytes was taken as an address",
                rdata.len()
            );
        }
        let short6 = response(0, &qname("example.com"), &[&record(QTYPE_AAAA, &[0u8; 15])]);
        assert!(matches!(
            parse_addresses(&short6, QTYPE_AAAA),
            Err(LxError::ENOENT)
        ));
    }

    #[test]
    fn a_server_error_is_reported_rather_than_read_past() {
        // NXDOMAIN and SERVFAIL carry no answers, and the counts in the header
        // are not to be trusted on one.
        for rcode in [2u8, 3, 5] {
            let pkt = response(rcode, &qname("nope.invalid"), &[]);
            assert!(
                matches!(parse_addresses(&pkt, QTYPE_A), Err(LxError::EIO)),
                "rcode {} was not reported as an error",
                rcode
            );
        }
    }

    #[test]
    fn an_answer_with_nothing_of_the_right_type_is_not_found() {
        let pkt = response(0, &qname("example.com"), &[&record(16, b"some text")]);
        assert!(matches!(
            parse_addresses(&pkt, QTYPE_A),
            Err(LxError::ENOENT)
        ));

        // The type has to be checked, not guessed from the length. A TXT
        // record carrying exactly four bytes is the same size as an A record,
        // and a parser that only looked at `rdlen` would hand back 3.97.98.99
        // -- a real-looking address made out of the string "abc".
        let same_size = response(0, &qname("example.com"), &[&record(16, b"\x03abc")]);
        assert!(
            matches!(parse_addresses(&same_size, QTYPE_A), Err(LxError::ENOENT)),
            "a four-byte TXT record was read as an address"
        );
        let empty = response(0, &qname("example.com"), &[]);
        assert!(matches!(
            parse_addresses(&empty, QTYPE_A),
            Err(LxError::ENOENT)
        ));
    }

    #[test]
    fn a_packet_too_short_to_hold_a_header_is_refused() {
        for n in 0..12usize {
            assert!(
                matches!(
                    parse_addresses(&alloc::vec![0u8; n], QTYPE_A),
                    Err(LxError::EINVAL)
                ),
                "a {}-byte packet was accepted",
                n
            );
        }
    }

    #[test]
    fn a_record_whose_rdata_runs_past_the_packet_is_dropped() {
        // The length inside the packet is the attacker's to choose. A record
        // claiming 200 bytes of rdata in a packet with 4 left must yield
        // nothing, not 200 bytes of whatever is next in memory.
        let mut pkt = response(0, &qname("example.com"), &[&record(QTYPE_A, &[1, 2, 3, 4])]);
        let len = pkt.len();
        pkt[len - 6] = 0x00;
        pkt[len - 5] = 0xC8; // rdlen = 200, with 4 bytes actually there
        assert!(matches!(
            parse_addresses(&pkt, QTYPE_A),
            Err(LxError::ENOENT)
        ));
    }

    #[test]
    fn a_header_claiming_more_answers_than_the_packet_holds_is_refused() {
        // AN=8 with one record in the packet: the loop must run out of data
        // and stop, not keep reading.
        let mut pkt = response(0, &qname("example.com"), &[&record(QTYPE_A, &[5, 6, 7, 8])]);
        pkt[7] = 8;
        // The one real answer is still found, and the seven that are not there
        // do not take the parser past the end.
        let got = parse_addresses(&pkt, QTYPE_A).unwrap();
        assert_eq!(
            got,
            alloc::vec![IpAddress::Ipv4(Ipv4Address::new(5, 6, 7, 8))]
        );
    }

    #[test]
    fn a_question_count_that_eats_the_answers_is_refused() {
        let mut pkt = response(0, &qname("example.com"), &[&record(QTYPE_A, &[1, 2, 3, 4])]);
        pkt[5] = 9; // QDCOUNT=9, so skipping the questions runs off the end
        assert!(matches!(
            parse_addresses(&pkt, QTYPE_A),
            Err(LxError::EINVAL)
        ));
    }
}
