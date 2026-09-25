//! IPv6 Router Advertisement (ICMPv6 type 134) processing.
//!
//! smoltcp never acts on Router Advertisements, and DHCPv6 (IA_NA) only conveys
//! a /128 host address — neither installs a default route nor learns the on-link
//! prefix. Without that, every off-link IPv6 destination (and every reply to an
//! off-link peer) has no next-hop, so global IPv6 is effectively unroutable in
//! both directions even though the address is configured.
//!
//! This module is fed every RX frame from `push_packet` (the same path as
//! `ndp_cache` / `icmp_rx`). For a valid RA it:
//!   * installs a default IPv6 route (`::/0`) via the router's link-local source,
//!     which gives every IPv6 destination a reachable, NDP-resolvable next-hop;
//!   * performs SLAAC (RFC 4862) for an autonomous on-link /64 prefix, forming
//!     `prefix || EUI-64(MAC)` and assigning it as a /64 so the prefix is on-link.
//!
//! The RA is parsed by hand rather than via `NdiscRepr::parse`, because that
//! parser rejects the whole advertisement when it carries an option it does not
//! model (RDNSS, Route Information, …) — options real routers routinely send.

use kernel_hal::net::get_net_device;
use lazy_static::lazy_static;
use lock::Mutex;
use log::*;
use smoltcp::wire::{
    Icmpv6Packet, IpAddress, IpCidr, IpProtocol, Ipv6Address, Ipv6Cidr, Ipv6Packet,
};

const ICMPV6_ROUTER_ADVERT: u8 = 134;
/// Prefix Information option (RFC 4861 §4.6.2).
const OPT_PREFIX_INFORMATION: u8 = 3;
const PIO_FLAG_ONLINK: u8 = 0x80;
const PIO_FLAG_AUTONOMOUS: u8 = 0x40;

/// Bound how many default routes / SLAAC addresses RA processing may install, so
/// an on-link attacker flooding RAs with varied source link-locals or prefixes
/// cannot grow the routing table / interface address list without limit.
const MAX_RA_ROUTES: usize = 8;
const MAX_RA_SLAAC: usize = 8;

/// Last applied configuration, to keep periodic re-advertisements idempotent.
struct RaState {
    gateway: Option<Ipv6Address>,
    slaac: Option<Ipv6Address>,
    /// Count of routes/addresses this module has installed (bounded above).
    installed_routes: usize,
    installed_slaac: usize,
}

lazy_static! {
    static ref STATE: Mutex<RaState> = Mutex::new(RaState {
        gateway: None,
        slaac: None,
        installed_routes: 0,
        installed_slaac: 0,
    });
}

/// (prefix bytes, prefix length, PIO flags, valid lifetime seconds).
type PrefixInfo = ([u8; 16], u8, u8, u32);

/// Inspect a received Ethernet frame and act on an IPv6 Router Advertisement.
pub fn process_from_frame(frame: &[u8]) {
    if let Some((src, router_lifetime, prefix)) = parse_ra(frame) {
        apply(src, router_lifetime, prefix);
    }
}

/// The validating half of [`process_from_frame`], with no interface state: the
/// router's link-local source, the router lifetime, and the first usable
/// Prefix Information option, or `None` when the frame is not an RA we accept.
fn parse_ra(frame: &[u8]) -> Option<(Ipv6Address, u16, Option<PrefixInfo>)> {
    // Ethernet header, optionally one 802.1Q VLAN tag, then IPv6.
    let (l2, et) = crate::net::packet::eth_l2_header_len(frame)?;
    if et != 0x86dd {
        return None;
    }

    let ipv6 = match Ipv6Packet::new_checked(&frame[l2..]) {
        Ok(p) => p,
        Err(_) => return None,
    };
    if ipv6.next_header() != IpProtocol::Icmpv6 {
        return None;
    }
    // RFC 4861 §6.1.2: a received RA MUST have IPv6 Hop Limit 255. An off-link
    // attacker cannot forge this (any intermediate router decrements it), so
    // this rejects off-link rogue-RA spoofing.
    if ipv6.hop_limit() != 255 {
        return None;
    }
    let src = ipv6.src_addr();
    // RFC 4861 §6.1.2: a valid RA is always sourced from a link-local address.
    if !src.is_link_local() {
        return None;
    }
    let dst = ipv6.dst_addr();

    let payload = ipv6.payload();
    if payload.len() < 16 || payload[0] != ICMPV6_ROUTER_ADVERT {
        return None;
    }
    // Validate the ICMPv6 checksum before trusting any field.
    match Icmpv6Packet::new_checked(payload) {
        Ok(icmp) => {
            if !icmp.verify_checksum(&IpAddress::Ipv6(src), &IpAddress::Ipv6(dst)) {
                return None;
            }
        }
        Err(_) => return None,
    }

    // RA header: [4]=cur hop limit, [5]=flags, [6..8]=router lifetime (s),
    // [8..12]=reachable time, [12..16]=retrans timer, [16..]=options.
    let router_lifetime = u16::from_be_bytes([payload[6], payload[7]]);

    let mut prefix: Option<PrefixInfo> = None;
    let mut off = 16usize;
    while off + 2 <= payload.len() {
        let opt_type = payload[off];
        let units = payload[off + 1] as usize;
        if units == 0 {
            break; // malformed: every option length is >= 1 unit (8 bytes)
        }
        let opt_len = units * 8;
        if off + opt_len > payload.len() {
            break;
        }
        if opt_type == OPT_PREFIX_INFORMATION && opt_len >= 32 {
            let plen = payload[off + 2];
            let flags = payload[off + 3];
            let valid = u32::from_be_bytes([
                payload[off + 4],
                payload[off + 5],
                payload[off + 6],
                payload[off + 7],
            ]);
            let mut pfx = [0u8; 16];
            pfx.copy_from_slice(&payload[off + 16..off + 32]);
            prefix = Some((pfx, plen, flags, valid));
            break; // single-prefix model is enough for the common case
        }
        off += opt_len;
    }

    Some((src, router_lifetime, prefix))
}

/// What an RA asks us to do with the default route, as a value, so the rule can
/// be read and tested without an interface.
#[derive(Debug, PartialEq, Eq)]
enum RouteAction {
    /// Nothing to do: this router is already the gateway, or was never it.
    Keep,
    /// Drop the default route via the current gateway (lifetime reached 0).
    Remove,
    /// Install a default route via this router.
    Install,
    /// Install this router's route and drop the previous gateway's.
    ///
    /// Only adding was done before, and the old route stayed in the table
    /// pointing at a router that is no longer advertising. A pair of routers
    /// taking turns, or one that changes its link-local, filled the table up to
    /// `MAX_RA_ROUTES` with dead next hops, and the kernel picks among them.
    Replace(Ipv6Address),
}

fn route_action(
    gateway: Option<Ipv6Address>,
    router_ll: Ipv6Address,
    router_lifetime: u16,
    installed_routes: usize,
) -> RouteAction {
    if router_lifetime == 0 {
        // A lifetime of 0 means "not a default router". Only our own gateway
        // saying it has anything to withdraw.
        return if gateway == Some(router_ll) {
            RouteAction::Remove
        } else {
            RouteAction::Keep
        };
    }
    match gateway {
        Some(current) if current == router_ll => RouteAction::Keep,
        Some(old) => RouteAction::Replace(old),
        // The bound is only for a first install: a replacement swaps one route
        // for another and cannot grow the table.
        None if installed_routes < MAX_RA_ROUTES => RouteAction::Install,
        None => RouteAction::Keep,
    }
}

/// Whether a Prefix Information option may be used for SLAAC.
///
/// RFC 4862 §5.5.3: the prefix must be autonomous, its valid lifetime nonzero,
/// and **the link-local prefix must be silently ignored**. Neither the
/// link-local nor the unspecified nor a multicast prefix was refused before, so
/// one RA from anybody on the link assigned this host an `fe80::/64` -- or a
/// `ff00::/64` -- address of its own, alongside the real link-local it already
/// derived from the same MAC.
fn prefix_is_usable(pfx: &[u8; 16], plen: u8, flags: u8, valid: u32) -> bool {
    if (flags & PIO_FLAG_AUTONOMOUS) == 0 || (flags & PIO_FLAG_ONLINK) == 0 {
        return false;
    }
    if valid == 0 || plen != 64 {
        return false;
    }
    // Only the prefix half forms the address, so only the prefix half is
    // judged: normalise the option's low 64 bits to zero first. Reading all
    // 128 let a rogue RA walk past the `::` check by putting anything it liked
    // in a half nobody uses -- `::/64` with a dirty low half looked like a
    // perfectly ordinary global prefix, and the address handed out was
    // `::<interface id>`. `fe80::/64` and a multicast prefix were caught
    // anyway, since what decides those lives in the high half.
    let mut high = [0u8; 16];
    high[..8].copy_from_slice(&pfx[..8]);
    let prefix = Ipv6Address::from_bytes(&high);
    // `::1/64` normalises to `::`, so loopback needs no clause of its own.
    !(prefix.is_link_local() || prefix.is_unspecified() || prefix.is_multicast())
}

/// `prefix || EUI-64(MAC)`: the interface identifier is the low 64 bits of the
/// RFC 4862 link-local address derived from the same MAC.
fn slaac_address(pfx: &[u8; 16], link_local: Ipv6Address) -> Ipv6Address {
    let iid = link_local.as_bytes();
    let mut addr = [0u8; 16];
    addr[..8].copy_from_slice(&pfx[..8]);
    addr[8..].copy_from_slice(&iid[8..]);
    Ipv6Address::from_bytes(&addr)
}

fn apply(router_ll: Ipv6Address, router_lifetime: u16, prefix: Option<PrefixInfo>) {
    // Apply to the primary Ethernet interface. The RX dispatch path carries no
    // ingress-interface information (frames are processed globally, as with
    // `ndp_cache`/`icmp_rx`), so on a multi-NIC host the RA is attributed to the
    // first Ethernet device — correct for the common single-NIC case.
    let iface = match get_net_device()
        .into_iter()
        .find(|d| d.get_ifname() != "loopback")
    {
        Some(d) => d,
        None => return,
    };

    // --- Default IPv6 route via the router's link-local source ---
    let default_cidr = IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0));
    let mut st = STATE.lock();
    match route_action(st.gateway, router_ll, router_lifetime, st.installed_routes) {
        RouteAction::Keep => {}
        RouteAction::Remove => {
            let _ = iface.del_route(default_cidr, Some(IpAddress::Ipv6(router_ll)));
            st.gateway = None;
            st.installed_routes = st.installed_routes.saturating_sub(1);
            info!(
                "[ra] removed default IPv6 route via {} on {}",
                router_ll,
                iface.get_ifname()
            );
        }
        RouteAction::Install => {
            if iface
                .add_route(default_cidr, Some(IpAddress::Ipv6(router_ll)))
                .is_ok()
            {
                st.gateway = Some(router_ll);
                st.installed_routes += 1;
                info!(
                    "[ra] default IPv6 route via {} on {}",
                    router_ll,
                    iface.get_ifname()
                );
            }
        }
        RouteAction::Replace(old) => {
            if iface
                .add_route(default_cidr, Some(IpAddress::Ipv6(router_ll)))
                .is_ok()
            {
                let _ = iface.del_route(default_cidr, Some(IpAddress::Ipv6(old)));
                st.gateway = Some(router_ll);
                info!(
                    "[ra] default IPv6 route moved from {} to {} on {}",
                    old,
                    router_ll,
                    iface.get_ifname()
                );
            }
        }
    }
    drop(st);

    // --- SLAAC for an autonomous on-link /64 prefix ---
    if let Some((pfx, plen, flags, valid)) = prefix {
        if prefix_is_usable(&pfx, plen, flags, valid) {
            let ll = crate::net::ipv6_link_local_from_mac(&iface.get_mac());
            let global = slaac_address(&pfx, ll);
            let cidr = IpCidr::Ipv6(Ipv6Cidr::new(global, 64));

            let mut st = STATE.lock();
            let already = iface.get_ip_address().contains(&cidr);
            if !already && st.installed_slaac < MAX_RA_SLAAC && iface.add_ip_address(cidr).is_ok() {
                st.slaac = Some(global);
                st.installed_slaac += 1;
                info!("[ra] SLAAC {}/64 on {}", global, iface.get_ifname());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Router Advertisement processing split in two: `parse_ra`, which decides
    //! whether a frame is an RA we accept, and the three rules `apply` follows
    //! once it is. The interface half needs a real NIC and stays out.

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use smoltcp::wire::{EthernetAddress, Ipv6Repr};

    const ROUTER_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    fn router() -> Ipv6Address {
        Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)
    }

    fn all_nodes() -> Ipv6Address {
        Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 1)
    }

    fn global_prefix() -> [u8; 16] {
        let mut p = [0u8; 16];
        p[..4].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8]);
        p
    }

    fn prefix_of(addr: Ipv6Address) -> [u8; 16] {
        let mut p = [0u8; 16];
        p.copy_from_slice(addr.as_bytes());
        p
    }

    /// An Ethernet frame carrying `ipv6`, optionally 802.1Q tagged.
    fn eth(vlan: bool, ipv6: &[u8]) -> Vec<u8> {
        let mut f = vec![0x33u8, 0x33, 0, 0, 0, 1];
        f.extend_from_slice(&ROUTER_MAC);
        if vlan {
            f.extend_from_slice(&0x8100u16.to_be_bytes());
            f.extend_from_slice(&0x0064u16.to_be_bytes());
        }
        f.extend_from_slice(&0x86ddu16.to_be_bytes());
        f.extend_from_slice(ipv6);
        f
    }

    /// A Router Advertisement built by hand: smoltcp's own emitter refuses the
    /// options real routers send, which is why `parse_ra` reads it by hand.
    /// `opts` is appended verbatim after the 16-byte RA header.
    fn ra_frame(
        src: Ipv6Address,
        dst: Ipv6Address,
        hop_limit: u8,
        msg_type: u8,
        router_lifetime: u16,
        opts: &[u8],
        vlan: bool,
    ) -> Vec<u8> {
        let mut icmp = vec![0u8; 16];
        icmp[0] = msg_type;
        icmp[6..8].copy_from_slice(&router_lifetime.to_be_bytes());
        icmp.extend_from_slice(opts);
        let ip = Ipv6Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Icmpv6,
            payload_len: icmp.len(),
            hop_limit,
        };
        let mut buf = vec![0u8; ip.buffer_len() + icmp.len()];
        let mut pkt = Ipv6Packet::new_unchecked(&mut buf);
        ip.emit(&mut pkt);
        pkt.payload_mut().copy_from_slice(&icmp);
        // Checksum last, over the finished pseudo-header.
        let mut icmp_pkt = Icmpv6Packet::new_unchecked(pkt.payload_mut());
        icmp_pkt.fill_checksum(&IpAddress::Ipv6(src), &IpAddress::Ipv6(dst));
        eth(vlan, &buf)
    }

    /// A Prefix Information option (RFC 4861 §4.6.2), 32 bytes.
    fn pio(pfx: &[u8; 16], plen: u8, flags: u8, valid: u32) -> Vec<u8> {
        let mut o = vec![0u8; 32];
        o[0] = OPT_PREFIX_INFORMATION;
        o[1] = 4; // 4 units of 8 bytes
        o[2] = plen;
        o[3] = flags;
        o[4..8].copy_from_slice(&valid.to_be_bytes());
        o[16..32].copy_from_slice(pfx);
        o
    }

    const AUTO_ONLINK: u8 = PIO_FLAG_AUTONOMOUS | PIO_FLAG_ONLINK;

    #[test]
    fn a_tagged_and_an_untagged_advertisement_parse_the_same() {
        let opts = pio(&global_prefix(), 64, AUTO_ONLINK, 86400);
        for vlan in [false, true] {
            let f = ra_frame(
                router(),
                all_nodes(),
                255,
                ICMPV6_ROUTER_ADVERT,
                1800,
                &opts,
                vlan,
            );
            let (src, lifetime, prefix) = parse_ra(&f).expect("vlan={vlan}");
            assert_eq!(src, router());
            assert_eq!(lifetime, 1800);
            let (pfx, plen, flags, valid) = prefix.expect("prefix");
            assert_eq!(pfx, global_prefix());
            assert_eq!((plen, flags, valid), (64, AUTO_ONLINK, 86400));
        }
    }

    #[test]
    fn an_advertisement_that_breaks_any_of_its_four_rules_is_refused() {
        let opts = pio(&global_prefix(), 64, AUTO_ONLINK, 86400);
        let ok = ra_frame(
            router(),
            all_nodes(),
            255,
            ICMPV6_ROUTER_ADVERT,
            1800,
            &opts,
            false,
        );
        assert!(parse_ra(&ok).is_some(), "the control case parses");

        // RFC 4861 §6.1.2: hop limit 255, which an off-link attacker cannot
        // forge because every router on the way decrements it.
        let f = ra_frame(
            router(),
            all_nodes(),
            254,
            ICMPV6_ROUTER_ADVERT,
            1800,
            &opts,
            false,
        );
        assert!(parse_ra(&f).is_none(), "hop limit 254");
        // ...and a link-local source.
        let global = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let f = ra_frame(
            global,
            all_nodes(),
            255,
            ICMPV6_ROUTER_ADVERT,
            1800,
            &opts,
            false,
        );
        assert!(parse_ra(&f).is_none(), "global source");
        // A Neighbor Advertisement is not a Router Advertisement.
        let f = ra_frame(router(), all_nodes(), 255, 136, 1800, &opts, false);
        assert!(parse_ra(&f).is_none(), "type 136");
        // A checksum that does not add up.
        let mut f = ok.clone();
        let last = f.len() - 1;
        f[last] ^= 0xff;
        assert!(parse_ra(&f).is_none(), "broken checksum");
    }

    #[test]
    fn a_frame_that_is_not_an_ipv6_icmp_datagram_is_refused_without_panicking() {
        let opts = pio(&global_prefix(), 64, AUTO_ONLINK, 86400);
        let ok = ra_frame(
            router(),
            all_nodes(),
            255,
            ICMPV6_ROUTER_ADVERT,
            1800,
            &opts,
            false,
        );
        // An IPv4 EtherType, and an unknown one.
        for et in [0x0800u16, 0x88f7] {
            let mut f = ok.clone();
            f[12..14].copy_from_slice(&et.to_be_bytes());
            assert!(parse_ra(&f).is_none());
        }
        // A tag with nothing behind it, every runt length, and a header cut in
        // the middle. An RA header shorter than its 16 fixed bytes too.
        assert!(parse_ra(&eth(true, &[])).is_none());
        for n in 0..24 {
            assert!(parse_ra(&vec![0u8; n]).is_none(), "len {}", n);
        }
        assert!(parse_ra(&ok[..30]).is_none(), "truncated IPv6 header");
        let short = ra_frame(
            router(),
            all_nodes(),
            255,
            ICMPV6_ROUTER_ADVERT,
            1800,
            &[],
            false,
        );
        assert!(parse_ra(&short).is_some(), "16 bytes is the whole header");
        assert!(
            parse_ra(&short[..short.len() - 4]).is_none(),
            "15 bytes is not"
        );
    }

    #[test]
    fn the_option_walk_survives_a_prefix_behind_options_it_does_not_model() {
        // RDNSS (25) and Route Information (24) are what made the module parse
        // the RA by hand: `NdiscRepr::parse` rejects the whole advertisement
        // over an option it does not model, and real routers send these.
        let mut opts = vec![0u8; 8];
        opts[0] = 1; // Source Link-Layer Address
        opts[1] = 1;
        let mut rdnss = vec![0u8; 24];
        rdnss[0] = 25;
        rdnss[1] = 3;
        opts.extend_from_slice(&rdnss);
        opts.extend_from_slice(&pio(&global_prefix(), 64, AUTO_ONLINK, 7200));
        let f = ra_frame(
            router(),
            all_nodes(),
            255,
            ICMPV6_ROUTER_ADVERT,
            1800,
            &opts,
            false,
        );
        let (_, _, prefix) = parse_ra(&f).expect("parses");
        assert_eq!(
            prefix.expect("prefix behind two options").0,
            global_prefix()
        );

        // An option claiming zero units would loop forever; one claiming more
        // than the payload holds would read past it. Both stop the walk, and
        // the advertisement itself still counts (its lifetime is the point).
        for units in [0u8, 40] {
            let mut bad = vec![0u8; 8];
            bad[0] = 1;
            bad[1] = units;
            bad.extend_from_slice(&pio(&global_prefix(), 64, AUTO_ONLINK, 7200));
            let f = ra_frame(
                router(),
                all_nodes(),
                255,
                ICMPV6_ROUTER_ADVERT,
                600,
                &bad,
                false,
            );
            let (_, lifetime, prefix) = parse_ra(&f).expect("still an RA");
            assert_eq!(lifetime, 600);
            assert_eq!(prefix, None, "units={}", units);
        }
        // A Prefix Information option too short to hold a prefix is skipped.
        let mut runt = pio(&global_prefix(), 64, AUTO_ONLINK, 7200);
        runt.truncate(24);
        runt[1] = 3;
        let f = ra_frame(
            router(),
            all_nodes(),
            255,
            ICMPV6_ROUTER_ADVERT,
            600,
            &runt,
            false,
        );
        assert_eq!(parse_ra(&f).expect("still an RA").2, None);
    }

    #[test]
    fn slaac_refuses_the_link_local_prefix_and_everything_else_that_is_not_ours() {
        let good = global_prefix();
        assert!(prefix_is_usable(&good, 64, AUTO_ONLINK, 86400));

        // RFC 4862 §5.5.3: the link-local prefix is silently ignored. Without
        // this, one advertisement from anybody on the link gave this host a
        // second fe80::/64 address of its own, beside the real link-local it
        // derives from the same MAC.
        assert!(!prefix_is_usable(
            &prefix_of(router()),
            64,
            AUTO_ONLINK,
            86400
        ));
        // And neither `::`, nor a multicast prefix, nor loopback.
        assert!(!prefix_is_usable(&[0u8; 16], 64, AUTO_ONLINK, 86400));
        assert!(!prefix_is_usable(
            &prefix_of(all_nodes()),
            64,
            AUTO_ONLINK,
            86400
        ));
        assert!(!prefix_is_usable(
            &prefix_of(Ipv6Address::LOOPBACK),
            64,
            AUTO_ONLINK,
            86400
        ));

        // The flags and the lifetime that were already checked.
        assert!(
            !prefix_is_usable(&good, 64, PIO_FLAG_ONLINK, 86400),
            "not autonomous"
        );
        assert!(
            !prefix_is_usable(&good, 64, PIO_FLAG_AUTONOMOUS, 86400),
            "not on-link"
        );
        assert!(!prefix_is_usable(&good, 64, AUTO_ONLINK, 0), "deprecated");
        assert!(!prefix_is_usable(&good, 56, AUTO_ONLINK, 86400), "only /64");
        assert!(
            !prefix_is_usable(&good, 128, AUTO_ONLINK, 86400),
            "only /64"
        );
    }

    #[test]
    fn the_slaac_address_is_the_prefix_over_the_link_locals_interface_id() {
        let ll = crate::net::ipv6_link_local_from_mac(&EthernetAddress(ROUTER_MAC));
        let addr = slaac_address(&global_prefix(), ll);
        // High half from the advertisement, low half from the MAC -- byte for
        // byte the same identifier the link-local carries.
        assert_eq!(&addr.as_bytes()[..8], &global_prefix()[..8]);
        assert_eq!(&addr.as_bytes()[8..], &ll.as_bytes()[8..]);
        // Only the prefix half of the option is read: the rest is the router's
        // to fill however it likes and must not leak into our address.
        let mut dirty = global_prefix();
        dirty[8..].copy_from_slice(&[0xde; 8]);
        assert_eq!(slaac_address(&dirty, ll), addr);
    }

    #[test]
    fn a_second_router_replaces_the_first_instead_of_piling_up_routes() {
        let a = router();
        let b = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);

        // Nothing installed yet.
        assert_eq!(route_action(None, a, 1800, 0), RouteAction::Install);
        // The same router re-advertising is idempotent.
        assert_eq!(route_action(Some(a), a, 1800, 1), RouteAction::Keep);
        // A different router used to be pure addition, so the route via `a`
        // stayed in the table pointing at a router that no longer advertises.
        assert_eq!(route_action(Some(a), b, 1800, 1), RouteAction::Replace(a));
        // A replacement swaps one route for another, so the cap does not
        // apply to it -- otherwise a full table froze the gateway forever.
        assert_eq!(
            route_action(Some(a), b, 1800, MAX_RA_ROUTES),
            RouteAction::Replace(a)
        );
    }

    #[test]
    fn a_zero_lifetime_only_withdraws_our_own_gateway() {
        let a = router();
        let b = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
        assert_eq!(route_action(Some(a), a, 0, 1), RouteAction::Remove);
        // Somebody else announcing it is not a router says nothing about ours.
        assert_eq!(route_action(Some(a), b, 0, 1), RouteAction::Keep);
        assert_eq!(route_action(None, a, 0, 0), RouteAction::Keep);
    }

    #[test]
    fn the_route_table_is_bounded_against_a_flood_of_link_local_sources() {
        // An on-link attacker advertising from a fresh link-local each time
        // installs at most `MAX_RA_ROUTES` -- and only while there is no
        // gateway, because once there is one every RA is a Replace.
        assert_eq!(
            route_action(None, router(), 1800, MAX_RA_ROUTES),
            RouteAction::Keep
        );
        assert_eq!(
            route_action(None, router(), 1800, MAX_RA_ROUTES - 1),
            RouteAction::Install
        );
    }

    #[test]
    fn slaac_judges_only_the_half_of_the_prefix_it_actually_uses() {
        // The option's low 64 bits never reach the address -- the interface
        // identifier replaces them -- so a router may put anything there, and
        // a rogue one will. Judging all 128 bits let `::/64` walk past the
        // unspecified check with a dirty low half, and the address handed out
        // was `::<interface id>`.
        let mut zero_prefix = [0u8; 16];
        zero_prefix[8..].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 1]);
        assert!(!prefix_is_usable(&zero_prefix, 64, AUTO_ONLINK, 86400));

        // Same for the other two, which the high half decided already and must
        // keep deciding once the low half is noise.
        let mut ll = prefix_of(router());
        ll[8..].copy_from_slice(&[0xff; 8]);
        assert!(!prefix_is_usable(&ll, 64, AUTO_ONLINK, 86400), "fe80::/64");
        let mut mc = prefix_of(all_nodes());
        mc[8..].copy_from_slice(&[0xff; 8]);
        assert!(!prefix_is_usable(&mc, 64, AUTO_ONLINK, 86400), "ff02::/64");

        // And a real prefix is still usable with a dirty low half, because
        // that half is exactly what `slaac_address` throws away.
        let mut good = global_prefix();
        good[8..].copy_from_slice(&[0x11; 8]);
        assert!(prefix_is_usable(&good, 64, AUTO_ONLINK, 86400));
    }
}
