//! Where a TCP connection of ours starts counting.
//!
//! smoltcp by itself starts every connection it makes at 42 and every one
//! it takes at the peer's number turned over, so anybody who knows the
//! addresses and ports of a connection knows which bytes it is about to
//! carry, and can write into it, or close it, without seeing a packet of
//! it. RFC 6528 says what to do instead, and it is what Linux does:
//!
//! ```text
//! start = clock + F(local address, local port, remote address, remote port, secret)
//! ```
//!
//! * The **clock** counts one every 4 µs, so a connection that follows
//!   another between the same two ends starts above where the other one
//!   could have got to, and what is left on the wire of the old one is not
//!   taken for the new.
//! * **F** is SipHash-2-4 with a key that nobody outside this process has:
//!   sixteen bytes the kernel hands out when the first connection asks.
//!   Whoever sees where one connection started learns nothing about where
//!   another, with other addresses or ports, will.
//!
//! The key is as good as the kernel's `getrandom`. On a machine without a
//! hardware source that is a generator seeded with the time of the boot.

use std::sync::OnceLock;

use smoltcp::wire::{IpAddress, IpEndpoint};

/// The key of this run of the stack.
static SECRET: OnceLock<[u64; 2]> = OnceLock::new();

/// How many nanoseconds one count of the clock is (RFC 6528, RFC 793).
const TICK_NS: u64 = 4_000;

/// The sequence number the connection between `local` and `remote` starts
/// at, now. It is what every TCP socket of the stack is given to
/// `set_initial_seq`.
pub fn initial_seq(local: IpEndpoint, remote: IpEndpoint) -> u32 {
    let secret = SECRET.get_or_init(secret);
    start(secret, udrivers::now_ns(), local, remote)
}

/// The sequence number the connection between `local` and `remote` starts
/// at when the clock says `now_ns`, with the key `secret`.
fn start(secret: &[u64; 2], now_ns: u64, local: IpEndpoint, remote: IpEndpoint) -> u32 {
    // Two addresses of sixteen bytes at most, a byte that says which kind
    // each is, and the two ports.
    let mut said = [0u8; 38];
    let mut len = 0;
    for end in [local, remote] {
        let addr: &[u8] = match &end.addr {
            IpAddress::Ipv4(addr) => addr.as_bytes(),
            IpAddress::Ipv6(addr) => addr.as_bytes(),
            _ => &[],
        };
        said[len] = addr.len() as u8;
        said[len + 1..][..addr.len()].copy_from_slice(addr);
        len += 1 + addr.len();
        said[len..][..2].copy_from_slice(&end.port.to_be_bytes());
        len += 2;
    }
    let hashed = siphash24(secret[0], secret[1], &said[..len]);
    (hashed as u32).wrapping_add((now_ns / TICK_NS) as u32)
}

/// Sixteen bytes from the kernel. One that has none to give leaves the
/// clock and the process number, which is still not a number everybody
/// knows beforehand.
fn secret() -> [u64; 2] {
    let mut bytes = [0u8; 16];
    // SAFETY: `bytes` is sixteen bytes to write.
    let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    let (low, high) = bytes.split_at(8);
    let mut key = [
        u64::from_le_bytes(low.try_into().unwrap()),
        u64::from_le_bytes(high.try_into().unwrap()),
    ];
    if got != bytes.len() as isize {
        key[0] ^= udrivers::now_ns();
        key[1] ^= u64::from(std::process::id());
    }
    key
}

/// SipHash-2-4 (Aumasson and Bernstein) of `data` with the key
/// `k0`, `k1`.
fn siphash24(k0: u64, k1: u64, data: &[u8]) -> u64 {
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    fn rounds(v: &mut [u64; 4], count: usize) {
        for _ in 0..count {
            v[0] = v[0].wrapping_add(v[1]);
            v[1] = v[1].rotate_left(13) ^ v[0];
            v[0] = v[0].rotate_left(32);
            v[2] = v[2].wrapping_add(v[3]);
            v[3] = v[3].rotate_left(16) ^ v[2];
            v[0] = v[0].wrapping_add(v[3]);
            v[3] = v[3].rotate_left(21) ^ v[0];
            v[2] = v[2].wrapping_add(v[1]);
            v[1] = v[1].rotate_left(17) ^ v[2];
            v[2] = v[2].rotate_left(32);
        }
    }
    let take = |v: &mut [u64; 4], word: u64| {
        v[3] ^= word;
        rounds(v, 2);
        v[0] ^= word;
    };
    let (words, left) = data.as_chunks::<8>();
    for word in words {
        take(&mut v, u64::from_le_bytes(*word));
    }
    // What is left, with the length in the last byte.
    let mut last = [0u8; 8];
    last[..left.len()].copy_from_slice(left);
    last[7] = data.len() as u8;
    take(&mut v, u64::from_le_bytes(last));
    v[2] ^= 0xff;
    rounds(&mut v, 4);
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::wire::{Ipv4Address, Ipv6Address};

    fn four(last: u8, port: u16) -> IpEndpoint {
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 9, last)), port)
    }

    /// The vectors of the paper and of the reference implementation: the
    /// key 00 01 .. 0f, and for what is hashed nothing, 00 .. 0e (the one
    /// the paper works through), a whole word and two.
    #[test]
    fn siphash_says_what_the_reference_says() {
        let (k0, k1) = (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let data: Vec<u8> = (0..16).collect();
        assert_eq!(siphash24(k0, k1, &[]), 0x726f_db47_dd0e_0e31);
        assert_eq!(siphash24(k0, k1, &data[..1]), 0x74f8_39c5_93dc_67fd);
        assert_eq!(siphash24(k0, k1, &data[..8]), 0x93f5_f579_9a93_2462);
        assert_eq!(siphash24(k0, k1, &data[..15]), 0xa129_ca61_49be_45e5);
        assert_eq!(siphash24(k0, k1, &data[..16]), 0x3f2a_cc7f_57c2_9bdb);
    }

    #[test]
    fn a_connection_starts_where_its_ends_and_the_key_say() {
        let secret = [0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321];
        let (us, them) = (four(15, 50000), four(1, 80));
        let at = start(&secret, 0, us, them);

        // The same ends, the same place; and nowhere near 42.
        assert_eq!(start(&secret, 0, us, them), at);
        assert_ne!(at, 42);

        // Any of the four that changes, another place; so does which end
        // is ours.
        let others = [
            start(&secret, 0, four(16, 50000), them),
            start(&secret, 0, four(15, 50001), them),
            start(&secret, 0, us, four(2, 80)),
            start(&secret, 0, us, four(1, 81)),
            start(&secret, 0, them, us),
        ];
        for (i, other) in others.iter().enumerate() {
            assert_ne!(*other, at, "{i}");
            assert!(!others[..i].contains(other), "{i}");
        }

        // An address of IPv6 is told from one of IPv4 with the same bytes
        // in front, and from none at all.
        let six = |port| {
            let addr = Ipv6Address::new(0x0a00, 0x090f, 0, 0, 0, 0, 0, 0);
            IpEndpoint::new(IpAddress::Ipv6(addr), port)
        };
        let none = IpEndpoint::new(IpAddress::Unspecified, 50000);
        assert_ne!(start(&secret, 0, six(50000), them), at);
        assert_ne!(start(&secret, 0, none, them), at);
        assert_ne!(
            start(&secret, 0, none, them),
            start(&secret, 0, six(50000), them)
        );

        // Another key, another place.
        let other = [secret[0] ^ 1, secret[1]];
        assert_ne!(start(&other, 0, us, them), at);
    }

    #[test]
    fn the_clock_moves_where_a_connection_starts() {
        let secret = [7, 9];
        let (us, them) = (four(15, 50000), four(1, 80));
        let at = start(&secret, 0, us, them);
        // One more every 4 µs, and round after 2^32 of them.
        assert_eq!(start(&secret, 3_999, us, them), at);
        assert_eq!(start(&secret, 4_000, us, them), at.wrapping_add(1));
        assert_eq!(
            start(&secret, 1_000_000_000, us, them),
            at.wrapping_add(250_000)
        );
        assert_eq!(start(&secret, 4_000 << 32, us, them), at);
    }

    #[test]
    fn the_key_is_not_the_same_twice() {
        assert_ne!(secret(), secret());
        // And the one of the run is taken once.
        let (us, them) = (four(15, 50000), four(1, 80));
        let key = *SECRET.get_or_init(secret);
        let before = udrivers::now_ns();
        let chosen = initial_seq(us, them);
        let after = udrivers::now_ns();
        let from = start(&key, before, us, them);
        let to = start(&key, after, us, them);
        assert!(chosen.wrapping_sub(from) <= to.wrapping_sub(from));
    }
}
