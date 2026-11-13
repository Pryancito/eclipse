//! The network configuration: what `/etc/eclipse/network` says, whether it
//! makes sense, what it takes to apply it, and what the interfaces have now.
//!
//! The window (`lunarnet`) edits it and the network service
//! (`lunarnet --service`) applies it at boot and when the window says so. Everything here is pure or
//! touches one file, so the parts that are easy to get wrong (a prefix of 33, a
//! gateway outside its network, a command line built from what was typed) are
//! tested without a network.
//!
//! The file is one block per interface, `key=value`, a blank line or a new
//! `iface=` starting the next:
//!
//! ```text
//! iface=eth0
//! ipv4=static
//! address4=192.168.1.20/24
//! gateway4=192.168.1.1
//! ipv6=auto
//! dns=1.1.1.1 2606:4700:4700::1111
//! ```
//!
//! An interface the file does not name gets DHCPv4 and automatic IPv6, which
//! is what the image did before this file existed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::i18n::Key;
use std::path::Path;

pub const CONF: &str = "/etc/eclipse/network";

/// How an interface gets its IPv4 address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum V4 {
    #[default]
    Dhcp,
    Static,
    Off,
}

/// How an interface gets its IPv6 address. `Auto` is SLAAC from the router's
/// advertisements, which the kernel does by itself; `Dhcp` adds DHCPv6 on top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum V6 {
    #[default]
    Auto,
    Dhcp,
    Static,
}

impl V4 {
    pub fn as_str(self) -> &'static str {
        match self {
            V4::Dhcp => "dhcp",
            V4::Static => "static",
            V4::Off => "off",
        }
    }
    pub fn parse(s: &str) -> Option<V4> {
        match s {
            "dhcp" => Some(V4::Dhcp),
            "static" | "manual" => Some(V4::Static),
            "off" => Some(V4::Off),
            _ => None,
        }
    }
    pub const ALL: [V4; 3] = [V4::Dhcp, V4::Static, V4::Off];
}

impl V6 {
    pub fn as_str(self) -> &'static str {
        match self {
            V6::Auto => "auto",
            V6::Dhcp => "dhcp",
            V6::Static => "static",
        }
    }
    pub fn parse(s: &str) -> Option<V6> {
        match s {
            "auto" | "slaac" => Some(V6::Auto),
            "dhcp" => Some(V6::Dhcp),
            "static" | "manual" => Some(V6::Static),
            _ => None,
        }
    }
    pub const ALL: [V6; 3] = [V6::Auto, V6::Dhcp, V6::Static];
}

/// One interface's settings, as typed: the fields stay text so the window can
/// hold a half-typed address and say what is wrong with it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub v4: V4,
    pub address4: String,
    pub gateway4: String,
    pub v6: V6,
    pub address6: String,
    pub gateway6: String,
    /// Space-separated, IPv4 and IPv6 mixed. Empty: whatever DHCP gives.
    pub dns: String,
}

impl Iface {
    pub fn new(name: &str) -> Self {
        Iface { name: name.to_string(), ..Iface::default() }
    }
}

/// A field that does not make sense, and why.
///
/// `why` is a KEY, not a string: the check runs the same whatever language the
/// session is in, and the window renders the reason with `lang.s(p.why)`. When
/// this held a `&'static str` the validation carried Spanish, which is how the
/// English session ended up with Spanish error text under English labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub field: Field,
    pub why: Key,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    Address4,
    Gateway4,
    Address6,
    Gateway6,
    Dns,
}

/// `a.b.c.d/n`, with `n` defaulting to 24 when left out.
pub fn cidr4(s: &str) -> Option<(Ipv4Addr, u8)> {
    let s = s.trim();
    let (a, p) = s.split_once('/').unwrap_or((s, "24"));
    let addr: Ipv4Addr = a.trim().parse().ok()?;
    let p: u8 = p.trim().parse().ok()?;
    (p <= 32).then_some((addr, p))
}

/// `addr/n`, with `n` defaulting to 64 when left out.
pub fn cidr6(s: &str) -> Option<(Ipv6Addr, u8)> {
    let s = s.trim();
    let (a, p) = s.split_once('/').unwrap_or((s, "64"));
    let addr: Ipv6Addr = a.trim().parse().ok()?;
    let p: u8 = p.trim().parse().ok()?;
    (p <= 128).then_some((addr, p))
}

fn mask4(p: u8) -> u32 {
    if p == 0 {
        0
    } else {
        u32::MAX << (32 - p)
    }
}

/// Whether `a` and `b` are on the same `/p`.
pub fn same_net4(a: Ipv4Addr, b: Ipv4Addr, p: u8) -> bool {
    u32::from(a) & mask4(p) == u32::from(b) & mask4(p)
}

/// The DNS servers of a `dns=` line, or the first one that is not an address.
pub fn dns_list(s: &str) -> Result<Vec<IpAddr>, String> {
    s.split([' ', ','])
        .filter(|w| !w.is_empty())
        .map(|w| w.parse::<IpAddr>().map_err(|_| w.to_string()))
        .collect()
}

/// What is wrong with `i`, field by field; empty when it can be applied.
pub fn check(i: &Iface) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut bad = |field, why| out.push(Problem { field, why });
    if i.v4 == V4::Static {
        match cidr4(&i.address4) {
            None => bad(Field::Address4, Key::net_bad_v4_cidr),
            Some((a, p)) => {
                if a.is_unspecified() || a.is_broadcast() || a.is_multicast() || a.is_loopback() {
                    bad(Field::Address4, Key::net_bad_addr_reserved);
                } else if p == 0 || p > 31 && p != 32 {
                    bad(Field::Address4, Key::net_bad_prefix4);
                }
                if !i.gateway4.trim().is_empty() {
                    match i.gateway4.trim().parse::<Ipv4Addr>() {
                        Err(_) => bad(Field::Gateway4, Key::net_bad_gw4),
                        Ok(g) if g == a => bad(Field::Gateway4, Key::net_bad_gw_is_self),
                        Ok(g) if p < 32 && !same_net4(a, g, p) => {
                            bad(Field::Gateway4, Key::net_bad_gw_offnet)
                        }
                        Ok(_) => {}
                    }
                }
            }
        }
    }
    if i.v6 == V6::Static {
        match cidr6(&i.address6) {
            None => bad(Field::Address6, Key::net_bad_v6_cidr),
            Some((a, p)) => {
                if a.is_unspecified() || a.is_loopback() || a.is_multicast() {
                    bad(Field::Address6, Key::net_bad_addr_reserved);
                } else if p == 0 {
                    bad(Field::Address6, Key::net_bad_prefix6);
                }
            }
        }
        if !i.gateway6.trim().is_empty() && i.gateway6.trim().parse::<Ipv6Addr>().is_err() {
            bad(Field::Gateway6, Key::net_bad_gw6);
        }
    }
    if let Err(w) = dns_list(&i.dns) {
        // `bad` borrows `out` mutably; the message carries no copy of `w`
        // because a Problem's text is fixed, and the field says where.
        let _ = w;
        bad(Field::Dns, Key::net_bad_dns);
    }
    out
}

// ── the file ─────────────────────────────────────────────────────────────────

/// One value of the file: no newline (that would start a new key).
fn one_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

/// Whether `name` can be an interface name: what the kernel allows, and
/// nothing a shell would read as more than a word.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 16
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

pub fn parse(text: &str) -> Vec<Iface> {
    let mut out: Vec<Iface> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        if k.trim() == "iface" {
            if valid_name(v) && !out.iter().any(|i| i.name == v) {
                out.push(Iface::new(v));
            }
            continue;
        }
        let Some(i) = out.last_mut() else { continue };
        match k.trim() {
            "ipv4" => i.v4 = V4::parse(v).unwrap_or_default(),
            "address4" => i.address4 = v.to_string(),
            "gateway4" => i.gateway4 = v.to_string(),
            "ipv6" => i.v6 = V6::parse(v).unwrap_or_default(),
            "address6" => i.address6 = v.to_string(),
            "gateway6" => i.gateway6 = v.to_string(),
            "dns" => i.dns = v.to_string(),
            _ => {}
        }
    }
    out
}

pub fn render(ifaces: &[Iface]) -> String {
    let mut s = String::from(
        "# Eclipse OS network settings, written by lunarnet and applied by\n\
         # lunarnet --service. ipv4 = dhcp | static | off; ipv6 = auto | dhcp | static.\n",
    );
    for i in ifaces {
        if !valid_name(&i.name) {
            continue;
        }
        s += &format!("\niface={}\nipv4={}\n", i.name, i.v4.as_str());
        if i.v4 == V4::Static {
            s += &format!("address4={}\n", one_line(&i.address4));
            if !i.gateway4.trim().is_empty() {
                s += &format!("gateway4={}\n", one_line(&i.gateway4));
            }
        }
        s += &format!("ipv6={}\n", i.v6.as_str());
        if i.v6 == V6::Static {
            s += &format!("address6={}\n", one_line(&i.address6));
            if !i.gateway6.trim().is_empty() {
                s += &format!("gateway6={}\n", one_line(&i.gateway6));
            }
        }
        if !i.dns.trim().is_empty() {
            s += &format!("dns={}\n", one_line(&i.dns));
        }
    }
    s
}

pub fn load(path: &Path) -> Vec<Iface> {
    std::fs::read_to_string(path).map(|t| parse(&t)).unwrap_or_default()
}

/// Write the file through a temporary beside it, so the service never reads
/// half of it.
pub fn save(path: &Path, ifaces: &[Iface]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, render(ifaces))?;
    std::fs::rename(&tmp, path)
}

/// The settings for `name`: the file's, or the default (DHCPv4, automatic
/// IPv6) for an interface the file does not mention.
pub fn settings_for(ifaces: &[Iface], name: &str) -> Iface {
    ifaces.iter().find(|i| i.name == name).cloned().unwrap_or_else(|| Iface::new(name))
}

// ── applying it ──────────────────────────────────────────────────────────────

/// What applying `i` takes: the `ip` commands to run now, in order, and which
/// DHCP clients to keep running. Each command is its arguments, never a shell
/// line, so nothing typed into a field reaches a shell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub commands: Vec<Vec<String>>,
    pub dhcp4: bool,
    pub dhcp6: bool,
    /// The servers `resolv.conf` gets; empty leaves it to DHCP.
    pub dns: Vec<IpAddr>,
}

/// The plan for `i`. A setting with a problem ([`check`]) is not applied:
/// the plan for an invalid static address flushes and adds nothing, rather
/// than adding half of it.
pub fn plan(i: &Iface) -> Plan {
    let problems = check(i);
    let has = |f| problems.iter().any(|p| p.field == f);
    let ip = |args: &[&str]| -> Vec<String> {
        std::iter::once("ip").chain(args.iter().copied()).map(str::to_string).collect()
    };
    let dev = i.name.as_str();
    let mut p = Plan::default();
    p.commands.push(ip(&["link", "set", "dev", dev, "up"]));
    match i.v4 {
        V4::Dhcp => p.dhcp4 = true,
        V4::Off => {
            p.commands.push(ip(&["-4", "addr", "flush", "dev", dev]));
        }
        V4::Static => {
            p.commands.push(ip(&["-4", "addr", "flush", "dev", dev]));
            if let (false, Some((a, n))) = (has(Field::Address4), cidr4(&i.address4)) {
                p.commands.push(ip(&["-4", "addr", "add", &format!("{a}/{n}"), "dev", dev]));
                if !has(Field::Gateway4) && !i.gateway4.trim().is_empty() {
                    p.commands.push(ip(&["-4", "route", "del", "default"]));
                    p.commands.push(ip(&["-4", "route", "add", "default", "via", i.gateway4.trim(), "dev", dev]));
                }
            }
        }
    }
    match i.v6 {
        V6::Auto => {}
        V6::Dhcp => p.dhcp6 = true,
        V6::Static => {
            if let (false, Some((a, n))) = (has(Field::Address6), cidr6(&i.address6)) {
                p.commands.push(ip(&["-6", "addr", "add", &format!("{a}/{n}"), "dev", dev]));
                if !has(Field::Gateway6) && !i.gateway6.trim().is_empty() {
                    p.commands.push(ip(&["-6", "route", "del", "default"]));
                    p.commands.push(ip(&["-6", "route", "add", "default", "via", i.gateway6.trim(), "dev", dev]));
                }
            }
        }
    }
    if !has(Field::Dns) {
        p.dns = dns_list(&i.dns).unwrap_or_default();
    }
    p
}

/// `resolv.conf` for these servers.
pub fn resolv_conf(dns: &[IpAddr]) -> String {
    let mut s = String::from("# written by lunarnet from /etc/eclipse/network\n");
    for d in dns {
        s += &format!("nameserver {d}\n");
    }
    s
}

// ── what the interfaces have now ─────────────────────────────────────────────

/// One interface as it stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Live {
    pub name: String,
    pub mac: String,
    /// `up`, `down`, `unknown`: `/sys/class/net/<if>/operstate`.
    pub state: String,
    pub v4: Vec<String>,
    pub v6: Vec<String>,
    pub gateway4: Option<String>,
    pub gateway6: Option<String>,
}

/// The addresses in `ip -o addr` output, by interface: `(name, family, cidr)`.
///
/// ```text
/// 2: eth0    inet 10.0.2.15/24 brd 10.0.2.255 scope global eth0\       valid_lft forever ...
/// 2: eth0    inet6 fe80::5054:ff:fe12:3456/64 scope link \       valid_lft forever ...
/// ```
pub fn parse_ip_addr(text: &str) -> Vec<(String, bool, String)> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let _index = w.next()?;
            let name = w.next()?.trim_end_matches(':').split('@').next()?.to_string();
            let fam = w.next()?;
            let cidr = w.next()?.to_string();
            match fam {
                "inet" => Some((name, false, cidr)),
                "inet6" => Some((name, true, cidr)),
                _ => None,
            }
        })
        .collect()
}

/// The default gateway of each interface in `ip route` / `ip -6 route` output.
pub fn parse_default_routes(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            if w.first() != Some(&"default") {
                return None;
            }
            let via = w.iter().position(|x| *x == "via").and_then(|i| w.get(i + 1))?;
            let dev = w.iter().position(|x| *x == "dev").and_then(|i| w.get(i + 1))?;
            Some((dev.to_string(), via.to_string()))
        })
        .collect()
}

/// The interfaces of `/sys/class/net`, loopback left out, in name order.
pub fn interfaces(sys: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(sys)
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    v.retain(|n| n != "lo" && valid_name(n));
    v.sort();
    v
}

/// Where the servers that run as processes say what they are, one file
/// each (`eclipse-udrivers`' `status`).
pub const SERVERS: &str = "/run/eclipse-servers";

/// The MACs of the cards that have a network stack running as a process:
/// the `mac=` of each `stack.*` file of `servers`, as sysfs writes one.
pub fn stack_cards(servers: &Path) -> Vec<String> {
    let files = std::fs::read_dir(servers).map(|rd| rd.flatten().collect()).unwrap_or_else(|_| Vec::new());
    let mut macs: Vec<String> = files
        .iter()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("stack.") && !n.ends_with(".tmp")))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| text.lines().find_map(|l| l.strip_prefix("mac=").map(|m| m.trim().to_ascii_lowercase())))
        .collect();
    macs.sort();
    macs.dedup();
    macs
}

/// The interfaces this service is to configure, and whether a stack that
/// runs as a process has a card. The card of such a stack is that stack's
/// to configure, with the address it was started with or its own DHCP
/// client: the kernel shows it to be looked at, and an `ip addr add` or a
/// `udhcpc` on it would do nothing. So it is left out here, and with one
/// around, an empty list is no reason to try `eth0` on the off chance.
pub fn to_configure(sys: &Path, servers: &Path) -> (Vec<String>, bool) {
    let cards = stack_cards(servers);
    let mac = |n: &str| std::fs::read_to_string(sys.join(n).join("address")).map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    let mut names = interfaces(sys);
    names.retain(|n| !cards.contains(&mac(n)));
    (names, !cards.is_empty())
}

/// The interfaces that are the card of a stack that runs as a process:
/// the ones [`to_configure`] leaves out.
pub fn of_stacks(sys: &Path, servers: &Path) -> Vec<String> {
    let ours = to_configure(sys, servers).0;
    interfaces(sys).into_iter().filter(|n| !ours.contains(n)).collect()
}

/// The name servers the stacks that run as processes were leased: the
/// `dns=` of each `stack.*` file of `servers` (`none` when its lease named
/// none, or it has no lease), each once. Of the stacks whose card is one
/// of the machine's interfaces in `sys`: one that serves only the
/// programs that asked for it does not name the machine's servers.
pub fn stack_dns(sys: &Path, servers: &Path) -> Vec<IpAddr> {
    let shown: Vec<String> = interfaces(sys)
        .iter()
        .filter_map(|n| std::fs::read_to_string(sys.join(n).join("address")).ok())
        .map(|a| a.trim().to_ascii_lowercase())
        .collect();
    let files = std::fs::read_dir(servers).map(|rd| rd.flatten().collect()).unwrap_or_else(|_| Vec::new());
    let mut names: Vec<_> = files
        .iter()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("stack.") && !n.ends_with(".tmp")))
        .map(|e| e.path())
        .collect();
    names.sort();
    let of_shown = |text: &String| {
        let mac = text.lines().find_map(|l| l.strip_prefix("mac=")).unwrap_or("");
        shown.contains(&mac.trim().to_ascii_lowercase())
    };
    let mut dns: Vec<IpAddr> = Vec::new();
    for text in names.iter().filter_map(|p| std::fs::read_to_string(p).ok()).filter(of_shown) {
        let said = text.lines().find_map(|l| l.strip_prefix("dns=")).unwrap_or("");
        for d in said.split(',').filter_map(|d| d.trim().parse().ok()) {
            if !dns.contains(&d) {
                dns.push(d);
            }
        }
    }
    dns
}

/// `resolv.conf` for the servers of a lease, as whoever holds one writes
/// it: the lease script of `udhcpc`, and a stack that runs as a process.
pub fn lease_resolv_conf(dns: &[IpAddr]) -> String {
    dns.iter().map(|d| format!("nameserver {d}\n")).collect()
}

/// Put it together: sysfs for the link, `ip` output for the addresses.
pub fn live(sys: &Path, ip_addr: &str, route4: &str, route6: &str) -> Vec<Live> {
    let addrs = parse_ip_addr(ip_addr);
    let gw4 = parse_default_routes(route4);
    let gw6 = parse_default_routes(route6);
    let read = |n: &str, f: &str| std::fs::read_to_string(sys.join(n).join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut names = interfaces(sys);
    for (n, _, _) in &addrs {
        if n != "lo" && !names.contains(n) && valid_name(n) {
            names.push(n.clone());
        }
    }
    names
        .into_iter()
        .map(|name| Live {
            mac: read(&name, "address"),
            state: read(&name, "operstate"),
            v4: addrs.iter().filter(|(n, v6, _)| *n == name && !v6).map(|(_, _, c)| c.clone()).collect(),
            v6: addrs.iter().filter(|(n, v6, _)| *n == name && *v6).map(|(_, _, c)| c.clone()).collect(),
            gateway4: gw4.iter().find(|(d, _)| *d == name).map(|(_, g)| g.clone()),
            gateway6: gw6.iter().find(|(d, _)| *d == name).map(|(_, g)| g.clone()),
            name,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(addr: &str, gw: &str) -> Iface {
        Iface { name: "eth0".into(), v4: V4::Static, address4: addr.into(), gateway4: gw.into(), ..Iface::default() }
    }

    #[test]
    fn the_file_round_trips_and_an_unnamed_interface_gets_the_old_default() {
        let a = vec![
            Iface {
                name: "eth0".into(),
                v4: V4::Static,
                address4: "192.168.1.20/24".into(),
                gateway4: "192.168.1.1".into(),
                v6: V6::Static,
                address6: "2001:db8::20/64".into(),
                gateway6: "fe80::1".into(),
                dns: "1.1.1.1 2606:4700:4700::1111".into(),
            },
            Iface { name: "eth1".into(), v4: V4::Off, v6: V6::Dhcp, ..Iface::default() },
        ];
        assert_eq!(parse(&render(&a)), a);
        assert_eq!(settings_for(&a, "wlan0"), Iface::new("wlan0"));
        assert_eq!(Iface::new("x").v4, V4::Dhcp);
        assert_eq!(Iface::new("x").v6, V6::Auto);
    }

    #[test]
    fn a_value_cannot_start_a_second_key_and_a_bad_name_is_dropped() {
        let a = vec![Iface { name: "eth0".into(), dns: "1.1.1.1\nipv4=off".into(), ..Iface::default() }];
        let back = parse(&render(&a));
        assert_eq!(back[0].v4, V4::Dhcp);
        assert_eq!(parse("iface=eth0; rm -rf /\nipv4=off\n"), vec![]);
        assert!(!valid_name("a b") && !valid_name("") && !valid_name("x".repeat(16).as_str()));
        assert!(valid_name("enp0s3") && valid_name("eth0.10"));
    }

    #[test]
    fn a_static_ipv4_is_checked_like_a_person_would() {
        assert!(check(&st("192.168.1.20/24", "192.168.1.1")).is_empty());
        assert!(check(&st("192.168.1.20", "")).is_empty(), "the /24 is implied");
        let f = |a, g| check(&st(a, g)).into_iter().map(|p| p.field).collect::<Vec<_>>();
        assert_eq!(f("192.168.1.300/24", ""), [Field::Address4]);
        assert_eq!(f("192.168.1.20/33", ""), [Field::Address4]);
        assert_eq!(f("0.0.0.0/24", ""), [Field::Address4]);
        assert_eq!(f("192.168.1.20/24", "10.0.0.1"), [Field::Gateway4]);
        assert_eq!(f("192.168.1.20/24", "192.168.1.20"), [Field::Gateway4]);
        assert_eq!(f("192.168.1.20/24", "router"), [Field::Gateway4]);
        // DHCP is not checked: nothing typed is used.
        let mut d = st("basura", "basura");
        d.v4 = V4::Dhcp;
        assert!(check(&d).is_empty());
    }

    #[test]
    fn a_static_ipv6_and_the_dns_are_checked() {
        let mut i = Iface { name: "eth0".into(), v6: V6::Static, address6: "2001:db8::20/64".into(), gateway6: "fe80::1".into(), ..Iface::default() };
        assert!(check(&i).is_empty());
        i.address6 = "2001:db8::20".into();
        assert!(check(&i).is_empty(), "the /64 is implied");
        i.address6 = "2001:db8::g/64".into();
        assert_eq!(check(&i)[0].field, Field::Address6);
        i.address6 = "::1/128".into();
        assert_eq!(check(&i)[0].field, Field::Address6);
        i.address6 = "2001:db8::20/129".into();
        assert_eq!(check(&i)[0].field, Field::Address6);
        let d = Iface { name: "eth0".into(), dns: "1.1.1.1, 2606:4700:4700::1111".into(), ..Iface::default() };
        assert!(check(&d).is_empty());
        assert_eq!(dns_list(&d.dns).unwrap().len(), 2);
        let d = Iface { dns: "1.1.1.1 dns.google".into(), ..d };
        assert_eq!(check(&d)[0].field, Field::Dns);
    }

    #[test]
    fn the_plan_is_ip_arguments_and_never_a_shell_line() {
        let mut i = st("192.168.1.20/24", "192.168.1.1");
        i.v6 = V6::Static;
        i.address6 = "2001:db8::20/64".into();
        i.dns = "9.9.9.9".into();
        let p = plan(&i);
        let lines: Vec<String> = p.commands.iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            lines,
            [
                "ip link set dev eth0 up",
                "ip -4 addr flush dev eth0",
                "ip -4 addr add 192.168.1.20/24 dev eth0",
                "ip -4 route del default",
                "ip -4 route add default via 192.168.1.1 dev eth0",
                "ip -6 addr add 2001:db8::20/64 dev eth0",
            ]
        );
        assert!(!p.dhcp4 && !p.dhcp6);
        assert_eq!(resolv_conf(&p.dns), "# written by lunarnet from /etc/eclipse/network\nnameserver 9.9.9.9\n");
        // The default is DHCP for v4 and the kernel's own SLAAC for v6.
        let d = plan(&Iface::new("eth0"));
        assert!(d.dhcp4 && !d.dhcp6);
        assert_eq!(d.commands.len(), 1);
    }

    #[test]
    fn an_invalid_address_is_flushed_and_not_half_applied() {
        let p = plan(&st("192.168.1.20/24", "10.0.0.1"));
        let lines: Vec<String> = p.commands.iter().map(|c| c.join(" ")).collect();
        assert!(lines.contains(&"ip -4 addr add 192.168.1.20/24 dev eth0".to_string()));
        assert!(!lines.iter().any(|l| l.contains("route add")), "a gateway off the network is not routed to");
        let p = plan(&st("nada", ""));
        assert!(!p.commands.iter().any(|c| c.contains(&"add".to_string())));
    }

    #[test]
    fn the_live_state_reads_busybox_ip_output() {
        let addr = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
                    2: eth0    inet 10.0.2.15/24 brd 10.0.2.255 scope global eth0\\       valid_lft forever\n\
                    2: eth0    inet6 fe80::5054:ff:fe12:3456/64 scope link \\       valid_lft forever\n\
                    2: eth0    inet6 2001:db8::5054:ff:fe12:3456/64 scope global dynamic\n";
        let got = parse_ip_addr(addr);
        assert_eq!(got.len(), 4);
        assert_eq!(got[1], ("eth0".into(), false, "10.0.2.15/24".into()));
        assert_eq!(got[3].2, "2001:db8::5054:ff:fe12:3456/64");
        let r4 = "default via 10.0.2.2 dev eth0\n10.0.2.0/24 dev eth0 scope link  src 10.0.2.15\n";
        let r6 = "2001:db8::/64 dev eth0 metric 256\ndefault via fe80::2 dev eth0 metric 1024\n";
        assert_eq!(parse_default_routes(r4), [("eth0".to_string(), "10.0.2.2".to_string())]);
        assert_eq!(parse_default_routes(r6), [("eth0".to_string(), "fe80::2".to_string())]);
        let sys = std::env::temp_dir().join(format!("lunarnet-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sys);
        for (n, mac, state) in [("eth0", "52:54:00:12:34:56", "up"), ("lo", "00:00:00:00:00:00", "unknown")] {
            std::fs::create_dir_all(sys.join(n)).unwrap();
            std::fs::write(sys.join(n).join("address"), format!("{mac}\n")).unwrap();
            std::fs::write(sys.join(n).join("operstate"), format!("{state}\n")).unwrap();
        }
        let l = live(&sys, addr, r4, r6);
        assert_eq!(l.len(), 1, "no loopback");
        assert_eq!(l[0].mac, "52:54:00:12:34:56");
        assert_eq!(l[0].state, "up");
        assert_eq!(l[0].v4, ["10.0.2.15/24"]);
        assert_eq!(l[0].v6.len(), 2);
        assert_eq!(l[0].gateway6.as_deref(), Some("fe80::2"));
        let _ = std::fs::remove_dir_all(&sys);
    }

    #[test]
    fn the_card_of_a_stack_process_is_not_this_services_to_configure() {
        let top = std::env::temp_dir().join(format!("lunarnet-stack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let (sys, servers) = (top.join("sys"), top.join("servers"));
        for (n, mac) in [("eth0", "52:54:00:12:34:56"), ("eth1", "52:54:00:aa:bb:cc"), ("lo", "00:00:00:00:00:00")] {
            std::fs::create_dir_all(sys.join(n)).unwrap();
            std::fs::write(sys.join(n).join("address"), format!("{mac}\n")).unwrap();
        }
        // No servers at all: every card is the kernel's.
        assert_eq!(to_configure(&sys, &servers), (vec!["eth0".to_string(), "eth1".to_string()], false));

        // A driver's file names a card too, and is not a stack's; a file
        // half written is not read.
        std::fs::create_dir_all(&servers).unwrap();
        std::fs::write(servers.join("driver.virtio-net@0"), "pid=7\nmac=52:54:00:12:34:56\n").unwrap();
        std::fs::write(servers.join("stack.virtio-net@1.tmp"), "pid=9\nmac=52:54:00:aa:bb:cc\n").unwrap();
        assert!(stack_cards(&servers).is_empty());
        assert_eq!(to_configure(&sys, &servers).0.len(), 2);

        // The stack's card is left out, however its MAC is written, and
        // the other one stays.
        std::fs::write(servers.join("stack.virtio-net@0"), "pid=8\nprogram=eclipse-netstack\nmac=52:54:00:12:34:56\naddr=10.0.9.15/24\n").unwrap();
        assert_eq!(stack_cards(&servers), ["52:54:00:12:34:56"]);
        assert_eq!(to_configure(&sys, &servers), (vec!["eth1".to_string()], true));
        std::fs::write(sys.join("eth0").join("address"), "52:54:00:12:34:56".to_uppercase()).unwrap();
        assert_eq!(to_configure(&sys, &servers), (vec!["eth1".to_string()], true));

        // With the only card a stack's, there is nothing to configure and
        // it says why.
        std::fs::remove_dir_all(sys.join("eth1")).unwrap();
        assert_eq!(to_configure(&sys, &servers), (Vec::new(), true));
        // What still shows it is the list for the panel.
        assert_eq!(interfaces(&sys), ["eth0"]);
        let _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn the_name_servers_of_a_stacks_lease_are_read_from_what_it_publishes() {
        let top = std::env::temp_dir().join(format!("lunarnet-stackdns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let (sys, servers) = (top.join("sys"), top.join("servers"));
        for (n, mac) in [("eth0", "52:54:00:12:34:56"), ("eth1", "52:54:00:aa:bb:cc")] {
            std::fs::create_dir_all(sys.join(n)).unwrap();
            std::fs::write(sys.join(n).join("address"), format!("{mac}\n")).unwrap();
        }
        // No stack: no card of one, and no servers.
        assert!(of_stacks(&sys, &servers).is_empty());
        assert!(stack_dns(&sys, &servers).is_empty());
        std::fs::create_dir_all(&servers).unwrap();
        let stack = |name: &str, mac: &str, dns: &str| {
            std::fs::write(servers.join(name), format!("pid=8\nmac={mac}\naddr=10.0.9.15/24\ndns={dns}\n")).unwrap();
        };
        // A stack with no lease yet, or whose lease named none, says so.
        stack("stack.virtio-net@0", "52:54:00:12:34:56", "none");
        assert_eq!(of_stacks(&sys, &servers), ["eth0"]);
        assert!(stack_dns(&sys, &servers).is_empty());
        // What it was leased, in its order; a driver's file and one half
        // written say nothing, and a second stack adds what is new.
        stack("stack.virtio-net@0", "52:54:00:12:34:56", "10.0.9.3,9.9.9.9");
        stack("driver.virtio-net@0", "52:54:00:12:34:56", "1.1.1.1");
        stack("stack.virtio-net@1.tmp", "52:54:00:aa:bb:cc", "2.2.2.2");
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(stack_dns(&sys, &servers), [ip("10.0.9.3"), ip("9.9.9.9")]);
        stack("stack.virtio-net@1", "52:54:00:aa:bb:cc", "9.9.9.9, 10.0.9.4,nonsense");
        assert_eq!(stack_dns(&sys, &servers), [ip("10.0.9.3"), ip("9.9.9.9"), ip("10.0.9.4")]);
        assert_eq!(of_stacks(&sys, &servers), ["eth0", "eth1"]);
        // A stack whose card the machine does not show serves the
        // programs that asked for it, and its servers are not the
        // machine's.
        stack("stack.virtio-net@2", "52:54:00:00:00:02", "10.0.9.5");
        assert_eq!(stack_dns(&sys, &servers).len(), 3);
        // Written as a lease script writes them: no line of ours on top.
        assert_eq!(lease_resolv_conf(&stack_dns(&sys, &servers)[..2]), "nameserver 10.0.9.3\nnameserver 9.9.9.9\n");
        let _ = std::fs::remove_dir_all(&top);
    }
}
