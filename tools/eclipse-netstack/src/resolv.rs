//! `/etc/resolv.conf` from the lease and the link's routers.
//!
//! A DHCP server names the name servers along with the address, and on
//! this system whoever holds the lease writes them down: for a card of
//! the kernel's that is the script `udhcpc` runs, and for the card of this
//! stack it is this process, which is its own DHCP client. The rule is
//! the script's: the lease's servers replace what the file said, unless
//! they were fixed by hand (`lunarnet` leaves a mark while they are), and
//! a lease that names none, or one that is lost, changes nothing.
//!
//! A router of IPv6 names name servers too, in what it advertises (RFC
//! 8106), and each for a time. Those are written after the lease's,
//! which are asked first (the RFC's 5.3.1 has what DHCP says before
//! what the routers do), and the file is written again when one of them
//! ends. When none is left and the lease named none either, the file is
//! left as it was: with nobody to ask, what it says does no harm.
//!
//! Only while the card is the machine's, though: the kernel shows it
//! among its interfaces when every program's sockets are this stack's.
//! Where it serves the programs that asked and no more, the machine's
//! interfaces are the kernel's and the file is their leases'.

use std::io;
use std::path::Path;

use smoltcp::wire::{Ipv4Address, Ipv6Address};

/// Where the resolver of every program looks.
pub const RESOLV_CONF: &str = "/etc/resolv.conf";
/// There while the name servers are fixed by hand: `lunarnet`'s mark,
/// which the lease scripts of `udhcpc` look for too.
pub const FIXED: &str = "/run/lunarnet/dns";

/// Where the kernel lists the machine's interfaces.
pub const SYS_NET: &str = "/sys/class/net";

/// The name the card with this address has among the machine's
/// interfaces listed in `sys`, when it is one of them.
pub fn card_name(sys: &Path, mac: [u8; 6]) -> Option<String> {
    let ours = udrivers::status::mac_text(mac);
    let listed = std::fs::read_dir(sys).ok()?;
    let is_ours = |e: &std::fs::DirEntry| {
        std::fs::read_to_string(e.path().join("address"))
            .is_ok_and(|address| address.trim().eq_ignore_ascii_case(&ours))
    };
    let card = listed.flatten().find(is_ours)?;
    card.file_name().into_string().ok()
}

/// What became of the name servers of a lease and of the routers.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Applied {
    /// The file names them now.
    Written,
    /// It named them already.
    Unchanged,
    /// They are fixed by hand, and the file stays as it is.
    Fixed,
    /// Nobody named any.
    Nothing,
}

/// The name servers there are to write down.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Named {
    /// The lease's, in its order.
    pub lease: Vec<Ipv4Address>,
    /// The ones the link's routers name, the first heard first.
    pub routers: Vec<Ipv6Address>,
}

impl Named {
    /// Whether nobody named any.
    pub fn is_empty(&self) -> bool {
        self.lease.is_empty() && self.routers.is_empty()
    }

    /// Each as it is written, on the machine whose interface to the
    /// link is called `card`: an address of the link (`fe80::`) says
    /// nothing without the link it is of, so that one is written with
    /// it, as resolvers read it (`fe80::1%eth0`).
    pub fn written(&self, card: &str) -> Vec<String> {
        let lease = self.lease.iter().map(|d| d.to_string());
        let routers = self.routers.iter().map(|d| {
            if d.is_link_local() {
                format!("{d}%{card}")
            } else {
                d.to_string()
            }
        });
        lease.chain(routers).collect()
    }

    /// The file for these servers: a line each, the lease's first, as the
    /// lease scripts write it.
    pub fn text(&self, card: &str) -> String {
        let lines = self.written(card).into_iter();
        lines.map(|d| format!("nameserver {d}\n")).collect()
    }
}

/// Make `conf` name the servers `named`, on the machine whose interface
/// is called `card`, unless `fixed` is there.
pub fn apply(named: &Named, card: &str, conf: &Path, fixed: &Path) -> io::Result<Applied> {
    if named.is_empty() {
        return Ok(Applied::Nothing);
    }
    if fixed.exists() {
        return Ok(Applied::Fixed);
    }
    let text = named.text(card);
    if std::fs::read_to_string(conf).is_ok_and(|now| now == text) {
        return Ok(Applied::Unchanged);
    }
    // Whole or not at all: a program that opens it meanwhile reads the
    // one before.
    let mut tmp = conf.as_os_str().to_owned();
    tmp.push(".eclipse-netstack");
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, conf)?;
    Ok(Applied::Written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_leases_name_servers_are_written_unless_they_are_fixed() {
        let top = std::env::temp_dir().join(format!("netstack-resolv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let (conf, fixed) = (top.join("resolv.conf"), top.join("dns"));
        let read = || std::fs::read_to_string(&conf).unwrap();
        let named = |lease: &[Ipv4Address]| Named {
            lease: lease.to_vec(),
            routers: Vec::new(),
        };
        let apply = |lease: &[Ipv4Address], conf: &Path, fixed: &Path| {
            super::apply(&named(lease), "eth0", conf, fixed)
        };
        let lease = [Ipv4Address([10, 0, 9, 3]), Ipv4Address([9, 9, 9, 9])];
        // What the image was built with.
        std::fs::write(&conf, "nameserver 8.8.8.8\nnameserver 1.1.1.1\n").unwrap();

        // A lease that names no server leaves the file to who wrote it.
        assert_eq!(apply(&[], &conf, &fixed).unwrap(), Applied::Nothing);
        assert_eq!(read(), "nameserver 8.8.8.8\nnameserver 1.1.1.1\n");

        // One that names some replaces it, in the lease's order, and
        // leaves nothing else behind.
        assert_eq!(apply(&lease, &conf, &fixed).unwrap(), Applied::Written);
        assert_eq!(read(), "nameserver 10.0.9.3\nnameserver 9.9.9.9\n");
        let left: Vec<_> = std::fs::read_dir(&top).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
        // The same lease again writes nothing.
        assert_eq!(apply(&lease, &conf, &fixed).unwrap(), Applied::Unchanged);

        // Fixed by hand, the file is not the lease's to change.
        std::fs::write(&fixed, "").unwrap();
        std::fs::write(&conf, "# written by lunarnet\nnameserver 192.168.1.1\n").unwrap();
        assert_eq!(apply(&lease, &conf, &fixed).unwrap(), Applied::Fixed);
        assert_eq!(read(), "# written by lunarnet\nnameserver 192.168.1.1\n");
        // And it is again once the mark is gone.
        std::fs::remove_file(&fixed).unwrap();
        assert_eq!(apply(&lease[..1], &conf, &fixed).unwrap(), Applied::Written);
        assert_eq!(read(), "nameserver 10.0.9.3\n");

        // With no file there, it is made; where it cannot be, that is
        // said.
        std::fs::remove_file(&conf).unwrap();
        assert_eq!(apply(&lease[..1], &conf, &fixed).unwrap(), Applied::Written);
        assert!(apply(&lease, &top.join("no/such/resolv.conf"), &fixed).is_err());
        let _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn the_routers_name_servers_are_written_after_the_leases() {
        let top = std::env::temp_dir().join(format!("netstack-resolv6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let (conf, fixed) = (top.join("resolv.conf"), top.join("dns"));
        let read = || std::fs::read_to_string(&conf).unwrap();
        let far = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let near = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x99);
        let mut named = Named {
            lease: vec![Ipv4Address([10, 0, 9, 3])],
            routers: vec![far, near],
        };
        std::fs::write(&conf, "nameserver 8.8.8.8\n").unwrap();

        // The lease's first; the one on the link, with the link.
        let written = apply(&named, "eth0", &conf, &fixed).unwrap();
        assert_eq!(written, Applied::Written);
        assert_eq!(
            read(),
            "nameserver 10.0.9.3\nnameserver fec0::99\nnameserver fe80::99%eth0\n"
        );
        assert_eq!(
            apply(&named, "eth0", &conf, &fixed).unwrap(),
            Applied::Unchanged
        );
        // On another interface it is another line.
        assert_eq!(named.written("eth1")[2], "fe80::99%eth1");

        // One that ends is taken out; with no lease, the routers' alone.
        named.routers.remove(0);
        assert_eq!(
            apply(&named, "eth0", &conf, &fixed).unwrap(),
            Applied::Written
        );
        assert_eq!(read(), "nameserver 10.0.9.3\nnameserver fe80::99%eth0\n");
        named.lease.clear();
        assert_eq!(
            apply(&named, "eth0", &conf, &fixed).unwrap(),
            Applied::Written
        );
        assert_eq!(read(), "nameserver fe80::99%eth0\n");

        // With nobody left to name one, the file stays; and fixed by
        // hand it is not the routers' to change either.
        named.routers.clear();
        assert!(named.is_empty());
        assert_eq!(
            apply(&named, "eth0", &conf, &fixed).unwrap(),
            Applied::Nothing
        );
        assert_eq!(read(), "nameserver fe80::99%eth0\n");
        named.routers.push(far);
        std::fs::write(&fixed, "").unwrap();
        assert_eq!(
            apply(&named, "eth0", &conf, &fixed).unwrap(),
            Applied::Fixed
        );
        assert_eq!(read(), "nameserver fe80::99%eth0\n");
        let _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn the_card_is_the_machines_interface_that_has_its_address() {
        let top = std::env::temp_dir().join(format!("netstack-sysnet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        // No list at all, and one with other cards only.
        assert_eq!(card_name(&top, mac), None);
        for (name, address) in [("lo", "00:00:00:00:00:00"), ("eth1", "52:54:00:aa:bb:cc")] {
            std::fs::create_dir_all(top.join(name)).unwrap();
            std::fs::write(top.join(name).join("address"), format!("{address}\n")).unwrap();
        }
        std::fs::create_dir_all(top.join("eth0")).unwrap();
        assert_eq!(card_name(&top, mac), None);
        // As sysfs writes it, in either case.
        std::fs::write(top.join("eth0/address"), "52:54:00:12:34:56\n").unwrap();
        assert_eq!(card_name(&top, mac).as_deref(), Some("eth0"));
        std::fs::write(top.join("eth0/address"), "52:54:00:12:34:56".to_uppercase()).unwrap();
        assert_eq!(card_name(&top, mac).as_deref(), Some("eth0"));
        let _ = std::fs::remove_dir_all(&top);
    }
}
