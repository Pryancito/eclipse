//! What hunter's network monitor (`hunter::netmon`) is told about: every inet
//! socket a process holds, with the process.
//!
//! `/proc/net/tcp` walks the smoltcp socket set, which knows ports and states
//! and nothing about owners. Ownership lives in the descriptor tables, so this
//! walks those instead: one [`hunter::netmon::Sock`] per (process, socket),
//! the way `netstat -p` pairs them on Linux. A socket inherited across `fork`
//! shows under each holder.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use hunter::netmon::{Proto, Sock, State};
use zircon_object::object::KernelObject;
use zircon_object::task::Process;

use super::{Endpoint, Socket};
use crate::process::ProcessExt;

const IPPROTO_UDP: u32 = 17;

/// The program a process runs, as hunter's other domains name it: its
/// `execve` path, or its name before it has one.
fn program_of(proc: &Process) -> String {
    match proc.try_linux().map(|lp| lp.execute_path()) {
        Some(p) if !p.is_empty() => p,
        _ => proc.name(),
    }
}

fn ip_of(ep: Option<Endpoint>) -> Option<(String, u16, bool)> {
    match ep? {
        Endpoint::Ip(ip) => Some((
            ip.addr.to_string(),
            ip.port,
            matches!(ip.addr, smoltcp::wire::IpAddress::Ipv6(_)),
        )),
        _ => None,
    }
}

/// `socket` as hunter sees it, held by `pid` running `program`. `None` for
/// anything that is not TCP or UDP over IP: unix, netlink, raw and packet
/// sockets carry no port a service could be reached on.
pub fn describe(pid: u64, program: String, socket: &dyn Socket) -> Option<Sock> {
    if !socket.is_inet() {
        return None;
    }
    let proto = if socket.is_tcp() {
        Proto::Tcp
    } else if socket.so_protocol() == Some(IPPROTO_UDP) {
        Proto::Udp
    } else {
        return None;
    };
    let (local_addr, local_port, v6) = ip_of(socket.endpoint())?;
    let listening = proto == Proto::Tcp && socket.is_listening();
    // A listener's smoltcp socket is "open" with an unspecified peer: that is
    // no peer. Asked only when not listening, which also spares the lock.
    let remote = if listening {
        None
    } else {
        ip_of(socket.remote_endpoint())
            .filter(|(_, port, _)| *port != 0)
            .map(|(a, p, _)| (a, p))
    };
    let state = match (proto, listening, &remote) {
        (_, true, _) => State::Listen,
        (_, false, Some(_)) => State::Established,
        (Proto::Udp, false, None) if local_port != 0 => State::Unconnected,
        _ => State::Closed,
    };
    Some(Sock {
        pid,
        program,
        proto,
        v6,
        state,
        local_addr,
        local_port,
        remote,
    })
}

/// Every inet socket of every live process.
///
/// The descriptor table is copied (`get_files`) and its lock dropped before a
/// socket is asked anything: a socket's accessors take its own lock and then
/// the global socket set's, the order every hot path takes them in, and none
/// of that may happen under a process lock.
pub fn snapshot() -> Vec<Sock> {
    let mut out = Vec::new();
    for proc in crate::process::live_processes() {
        let files = match proc.try_linux().and_then(|lp| lp.get_files().ok()) {
            Some(f) => f,
            None => continue,
        };
        let program = program_of(&proc);
        let mut fds: Vec<_> = files.into_iter().collect();
        fds.sort_by_key(|(fd, _)| i32::from(*fd));
        for (_, file) in fds {
            if let Ok(socket) = file.as_socket() {
                if let Some(s) = describe(proc.id(), program.clone(), socket) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Takes a [`snapshot`] and hands it to hunter, which reports the listeners
/// nobody expected. `/proc/hunter_net` calls this on every read, so whoever
/// watches the table also keeps the alerts current.
pub fn refresh() {
    hunter::netmon::observe(snapshot());
}

/// The `listen(2)` hook: tells hunter about the listener the moment it opens,
/// instead of at the next read of the table.
pub fn on_listen(proc: &Arc<Process>, socket: &dyn Socket) {
    if let Some(s) = describe(proc.id(), program_of(proc), socket) {
        hunter::netmon::on_listen(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{IpAddress, IpEndpoint, TcpSocketState, UdpSocketState, NET_TEST_LOCK};

    fn any(port: u16) -> Endpoint {
        Endpoint::Ip(IpEndpoint::new(IpAddress::v4(0, 0, 0, 0), port))
    }

    #[test]
    fn a_bound_udp_socket_is_an_unconnected_listener() {
        let _g = NET_TEST_LOCK.lock();
        let s = UdpSocketState::new(false).unwrap();
        assert_eq!(Socket::bind(&s, any(40053)), Ok(0));
        let d = describe(9, String::from("/sbin/x"), &s).unwrap();
        assert_eq!(d.proto, Proto::Udp);
        assert_eq!(d.state, State::Unconnected);
        assert_eq!((d.local_addr.as_str(), d.local_port), ("0.0.0.0", 40053));
        assert!(d.is_listener());
        assert_eq!(d.remote, None);
    }

    #[test]
    fn a_listening_tcp_socket_is_a_listener_without_a_peer() {
        let _g = NET_TEST_LOCK.lock();
        let s = TcpSocketState::new(false).unwrap();
        assert_eq!(Socket::bind(&s, any(40080)), Ok(0));
        assert_eq!(Socket::listen(&s), Ok(0));
        let d = describe(7, String::from("/usr/bin/nc"), &s).unwrap();
        assert_eq!(d.proto, Proto::Tcp);
        assert_eq!(d.state, State::Listen);
        assert_eq!(d.local_port, 40080);
        assert_eq!(d.remote, None);
        assert!(d.is_listener());
    }

    #[test]
    fn a_fresh_tcp_socket_is_closed_and_not_a_listener() {
        let _g = NET_TEST_LOCK.lock();
        let s = TcpSocketState::new(false).unwrap();
        let d = describe(7, String::from("/x"), &s).unwrap();
        assert_eq!(d.state, State::Closed);
        assert!(!d.is_listener());
    }
}
