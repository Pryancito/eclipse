use crate::error::{LxError, LxResult};
use alloc::{boxed::Box, vec::Vec};
use lock::Mutex;
use smoltcp::wire::IpEndpoint;

const PORT_NUM: usize = 65536;

pub struct ListenTableEntry {
    /// Every endpoint listening on this port. More than one is legal: Linux
    /// only refuses a *conflicting* address (`inet_csk_bind_conflict`), so
    /// 127.0.0.1:80 and 192.168.1.5:80 are two independent listeners and a
    /// process may serve one interface without taking the port everywhere.
    pub listen_endpoints: Vec<IpEndpoint>,
}

pub struct ListenTable {
    tcp: Box<[Mutex<Option<Box<ListenTableEntry>>>]>,
}

impl Default for ListenTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ListenTable {
    pub fn new() -> Self {
        let mut vec = Vec::with_capacity(PORT_NUM);
        for _ in 0..PORT_NUM {
            vec.push(Mutex::new(None));
        }
        Self {
            tcp: vec.into_boxed_slice(),
        }
    }

    /// Whether `listen_endpoint` could start listening: the table is keyed by
    /// port but the decision is `endpoints_collide`, the same predicate `bind`
    /// already used. Keying the decision on the port alone made two
    /// address-specific listeners on one port collide, so `bind` let both
    /// sockets through and the second `listen` failed EADDRINUSE.
    pub fn can_listen(&self, listen_endpoint: IpEndpoint) -> bool {
        match &*self.tcp[listen_endpoint.port as usize].lock() {
            Some(entry) => !entry
                .listen_endpoints
                .iter()
                .any(|&held| endpoints_collide(held, listen_endpoint)),
            None => true,
        }
    }

    pub fn listen(&self, listen_endpoint: IpEndpoint) -> LxResult<()> {
        let port = listen_endpoint.port;
        if port == 0 {
            return Err(LxError::EINVAL);
        }
        let mut slot = self.tcp[port as usize].lock();
        let entry = slot.get_or_insert_with(|| {
            Box::new(ListenTableEntry {
                listen_endpoints: Vec::new(),
            })
        });
        if entry
            .listen_endpoints
            .iter()
            .any(|&held| endpoints_collide(held, listen_endpoint))
        {
            return Err(LxError::EADDRINUSE);
        }
        entry.listen_endpoints.push(listen_endpoint);
        Ok(())
    }

    /// Give back one listener's reservation. Idempotent, so it is safe to call
    /// from both `shutdown` and `Drop`.
    pub fn unlisten(&self, listen_endpoint: IpEndpoint) {
        let mut slot = self.tcp[listen_endpoint.port as usize].lock();
        if let Some(entry) = slot.as_mut() {
            if let Some(i) = entry
                .listen_endpoints
                .iter()
                .position(|&held| held == listen_endpoint)
            {
                entry.listen_endpoints.swap_remove(i);
            }
            if entry.listen_endpoints.is_empty() {
                *slot = None;
            }
        }
    }
}

/// Whether two endpoints claim the same port on overlapping addresses:
/// the wildcard overlaps everything, a specific address only itself
/// (`inet_rcv_saddr_equal`).
pub fn endpoints_collide(a: IpEndpoint, b: IpEndpoint) -> bool {
    a.port == b.port && (a.addr.is_unspecified() || b.addr.is_unspecified() || a.addr == b.addr)
}

/// A port a TCP socket took with `bind(2)`.
#[derive(Clone, Copy, Debug)]
pub struct BoundPort {
    pub endpoint: IpEndpoint,
    /// `SO_REUSEADDR` was set when the socket bound.
    pub reuse_addr: bool,
}

/// Linux's bind hash bucket, for the sockets smoltcp cannot see: a socket
/// that has bound but neither listens nor connects yet exists only in
/// `TcpInner`, so without this table a second `bind` on the same port
/// succeeded and the two servers found out at `listen`, or never (two
/// clients that bound the same source port).
pub struct BindTable {
    tcp: Mutex<Vec<BoundPort>>,
}

impl Default for BindTable {
    fn default() -> Self {
        Self::new()
    }
}

impl BindTable {
    pub fn new() -> Self {
        Self {
            tcp: Mutex::new(Vec::new()),
        }
    }

    pub fn insert(&self, endpoint: IpEndpoint, reuse_addr: bool) {
        self.tcp.lock().push(BoundPort {
            endpoint,
            reuse_addr,
        });
    }

    /// Give back one registration of `endpoint` (there is one per socket).
    pub fn release(&self, endpoint: IpEndpoint) {
        let mut tcp = self.tcp.lock();
        if let Some(i) = tcp.iter().position(|b| b.endpoint == endpoint) {
            tcp.swap_remove(i);
        }
    }

    pub fn snapshot(&self) -> Vec<BoundPort> {
        self.tcp.lock().clone()
    }
}

lazy_static::lazy_static! {
    pub static ref LISTEN_TABLE: ListenTable = ListenTable::new();
    pub static ref BIND_TABLE: BindTable = BindTable::new();
}

#[cfg(test)]
mod tests {
    //! `inet_csk_get_port` / `inet_csk_bind_conflict` for the two tables that
    //! hold a TCP port: the listeners and the sockets that only bound. Both
    //! answer with `endpoints_collide`, and the listen table used to answer
    //! with the port number alone.

    use super::*;
    use smoltcp::wire::{IpAddress, Ipv4Address};

    fn ep(addr: IpAddress, port: u16) -> IpEndpoint {
        IpEndpoint::new(addr, port)
    }

    fn any(port: u16) -> IpEndpoint {
        ep(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), port)
    }

    fn lo(port: u16) -> IpEndpoint {
        ep(IpAddress::v4(127, 0, 0, 1), port)
    }

    fn lan(port: u16) -> IpEndpoint {
        ep(IpAddress::v4(192, 168, 1, 5), port)
    }

    #[test]
    fn two_listeners_on_one_port_but_different_addresses_both_get_in() {
        let table = ListenTable::new();
        // This is a plain `nginx listening on 127.0.0.1:8080` plus
        // `listening on 192.168.1.5:8080`. `bind` allowed both (it asks
        // `endpoints_collide`, which says two specific addresses never
        // overlap) and then the second `listen` was refused, because the
        // table's only key was the port.
        assert!(table.can_listen(lo(8080)));
        assert_eq!(table.listen(lo(8080)), Ok(()));
        assert!(table.can_listen(lan(8080)), "a different address is free");
        assert_eq!(table.listen(lan(8080)), Ok(()));

        // Each of them still holds its own address.
        assert!(!table.can_listen(lo(8080)));
        assert!(!table.can_listen(lan(8080)));
        assert_eq!(table.listen(lo(8080)), Err(LxError::EADDRINUSE));
        assert_eq!(table.listen(lan(8080)), Err(LxError::EADDRINUSE));
        // ...and a third address on the same port is still free.
        assert!(table.can_listen(ep(IpAddress::v4(10, 0, 0, 1), 8080)));
    }

    #[test]
    fn the_wildcard_collides_with_every_address_on_its_port_both_ways() {
        let table = ListenTable::new();
        assert_eq!(table.listen(any(80)), Ok(()));
        assert!(!table.can_listen(lo(80)), "0.0.0.0 covers 127.0.0.1");
        assert_eq!(table.listen(lo(80)), Err(LxError::EADDRINUSE));
        assert!(table.can_listen(lo(81)), "another port is another matter");

        // And the other way round: a specific listener refuses the wildcard.
        let table = ListenTable::new();
        assert_eq!(table.listen(lo(80)), Ok(()));
        assert!(!table.can_listen(any(80)));
        assert_eq!(table.listen(any(80)), Err(LxError::EADDRINUSE));
    }

    #[test]
    fn unlisten_gives_back_one_reservation_and_only_that_one() {
        let table = ListenTable::new();
        assert_eq!(table.listen(lo(8080)), Ok(()));
        assert_eq!(table.listen(lan(8080)), Ok(()));

        table.unlisten(lo(8080));
        assert!(table.can_listen(lo(8080)), "released");
        assert!(!table.can_listen(lan(8080)), "the other one stays");
        // The wildcard is still refused while one address holds the port.
        assert!(!table.can_listen(any(8080)));

        table.unlisten(lan(8080));
        assert!(table.can_listen(any(8080)), "the port is free again");
    }

    #[test]
    fn unlisten_is_idempotent_because_shutdown_and_drop_both_call_it() {
        let table = ListenTable::new();
        assert_eq!(table.listen(lo(9000)), Ok(()));
        table.unlisten(lo(9000));
        table.unlisten(lo(9000));
        // An endpoint that never listened, and the port that never did.
        table.unlisten(lan(9000));
        table.unlisten(lo(0));
        assert!(table.can_listen(lo(9000)));
        assert!(table.can_listen(any(9000)));
    }

    #[test]
    fn port_zero_can_never_be_listened_on() {
        let table = ListenTable::new();
        // `inet_listen` never sees port 0: `bind`/`autobind` gave the socket a
        // real port first. A 0 here is a bug upstream, not a free port.
        assert_eq!(table.listen(any(0)), Err(LxError::EINVAL));
        assert_eq!(table.listen(lo(0)), Err(LxError::EINVAL));
    }

    #[test]
    fn endpoints_collide_is_inet_rcv_saddr_equal() {
        assert!(endpoints_collide(lo(80), lo(80)));
        assert!(endpoints_collide(any(80), lo(80)));
        assert!(endpoints_collide(lo(80), any(80)));
        assert!(endpoints_collide(any(80), any(80)));
        assert!(!endpoints_collide(lo(80), lan(80)));
        assert!(!endpoints_collide(lo(80), lo(81)));
        assert!(!endpoints_collide(any(80), any(81)));
        // Two families on one port do not overlap either.
        let v6 = ep(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 80);
        assert!(!endpoints_collide(lo(80), v6));
        assert!(endpoints_collide(v6, v6));
    }

    #[test]
    fn the_bind_table_hands_back_one_registration_per_socket() {
        let table = BindTable::new();
        assert!(table.snapshot().is_empty());
        // Two sockets that bound the same endpoint (`SO_REUSEADDR`, or a
        // listener plus the child that inherited it) each hold one.
        table.insert(lo(7000), true);
        table.insert(lo(7000), true);
        table.insert(lan(7000), false);
        assert_eq!(table.snapshot().len(), 3);

        table.release(lo(7000));
        assert_eq!(
            table
                .snapshot()
                .iter()
                .filter(|b| b.endpoint == lo(7000))
                .count(),
            1,
            "one release gives back one registration, not the pair"
        );
        // `reuse_addr` travels with the registration, which is what
        // `bind_conflict` reads to let a server restart.
        assert!(table
            .snapshot()
            .iter()
            .any(|b| b.endpoint == lo(7000) && b.reuse_addr));
        assert!(table
            .snapshot()
            .iter()
            .any(|b| b.endpoint == lan(7000) && !b.reuse_addr));

        // Releasing something never registered changes nothing.
        table.release(any(7000));
        assert_eq!(table.snapshot().len(), 2);
        table.release(lo(7000));
        table.release(lan(7000));
        assert!(table.snapshot().is_empty());
    }
}
