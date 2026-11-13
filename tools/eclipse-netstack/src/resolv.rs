//! `/etc/resolv.conf` from the lease.
//!
//! A DHCP server names the name servers along with the address, and on
//! this system whoever holds the lease writes them down: for a card of
//! the kernel's that is the script `udhcpc` runs, and for the card of this
//! stack it is this process, which is its own DHCP client. The rule is
//! the script's: the lease's servers replace what the file said, unless
//! they were fixed by hand (`lunarnet` leaves a mark while they are), and
//! a lease that names none, or one that is lost, changes nothing.
//!
//! Only while the card is the machine's, though: the kernel shows it
//! among its interfaces when every program's sockets are this stack's.
//! Where it serves the programs that asked and no more, the machine's
//! interfaces are the kernel's and the file is their leases'.

use std::io;
use std::path::Path;

use smoltcp::wire::Ipv4Address;

/// Where the resolver of every program looks.
pub const RESOLV_CONF: &str = "/etc/resolv.conf";
/// There while the name servers are fixed by hand: `lunarnet`'s mark,
/// which the lease scripts of `udhcpc` look for too.
pub const FIXED: &str = "/run/lunarnet/dns";

/// Where the kernel lists the machine's interfaces.
pub const SYS_NET: &str = "/sys/class/net";

/// Whether the card with this address is one of the machine's interfaces
/// listed in `sys`.
pub fn is_the_machines(sys: &Path, mac: [u8; 6]) -> bool {
    let ours = udrivers::status::mac_text(mac);
    let Ok(listed) = std::fs::read_dir(sys) else {
        return false;
    };
    listed
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("address")).ok())
        .any(|address| address.trim().eq_ignore_ascii_case(&ours))
}

/// What became of a lease's name servers.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Applied {
    /// The file names them now.
    Written,
    /// It named them already.
    Unchanged,
    /// They are fixed by hand, and the file stays as it is.
    Fixed,
    /// The lease named none.
    Nothing,
}

/// The file for these servers: a line each, in the lease's order, as the
/// lease scripts write it.
pub fn text(dns: &[Ipv4Address]) -> String {
    dns.iter().map(|d| format!("nameserver {d}\n")).collect()
}

/// Make `conf` name the servers of a lease, unless `fixed` is there.
pub fn apply(dns: &[Ipv4Address], conf: &Path, fixed: &Path) -> io::Result<Applied> {
    if dns.is_empty() {
        return Ok(Applied::Nothing);
    }
    if fixed.exists() {
        return Ok(Applied::Fixed);
    }
    let text = text(dns);
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
    fn the_card_is_the_machines_when_an_interface_has_its_address() {
        let top = std::env::temp_dir().join(format!("netstack-sysnet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        // No list at all, and one with other cards only.
        assert!(!is_the_machines(&top, mac));
        for (name, address) in [("lo", "00:00:00:00:00:00"), ("eth1", "52:54:00:aa:bb:cc")] {
            std::fs::create_dir_all(top.join(name)).unwrap();
            std::fs::write(top.join(name).join("address"), format!("{address}\n")).unwrap();
        }
        std::fs::create_dir_all(top.join("eth0")).unwrap();
        assert!(!is_the_machines(&top, mac));
        // As sysfs writes it, in either case.
        std::fs::write(top.join("eth0/address"), "52:54:00:12:34:56\n").unwrap();
        assert!(is_the_machines(&top, mac));
        std::fs::write(top.join("eth0/address"), "52:54:00:12:34:56".to_uppercase()).unwrap();
        assert!(is_the_machines(&top, mac));
        let _ = std::fs::remove_dir_all(&top);
    }
}
