//! Addresses of the network, as the kernel names them: the card's, IPv4's
//! and IPv6's, a network with its prefix, an address with its port.
//!
//! The kernel has no network stack of its own: the stack runs as a process
//! and the two exchange bytes. What is left here is what the kernel needs
//! to show an interface, keep a route and say whose a socket is. The types
//! are the ones of smoltcp's `wire` module (0BSD), which the kernel used
//! until it stopped building that crate: same names, same methods, same
//! text when printed.

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn write_u16(bytes: &mut [u8], value: u16) {
    bytes[..2].copy_from_slice(&value.to_be_bytes());
}

/// Why an address or a network could not be made of what was given.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Error {
    /// What was given does not say one.
    Illegal,
}

/// The result of making one.
pub type Result<T> = core::result::Result<T, Error>;

pub use self::ethernet::Address as EthernetAddress;
pub use self::ip::{Address as IpAddress, Cidr as IpCidr, Endpoint as IpEndpoint};
pub use self::ipv4::{Address as Ipv4Address, Cidr as Ipv4Cidr};
pub use self::ipv6::{Address as Ipv6Address, Cidr as Ipv6Cidr};

mod ethernet {
    use core::fmt;

    /// A six-octet Ethernet II address.
    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
    pub struct Address(pub [u8; 6]);

    impl Address {
        /// The broadcast address.
        pub const BROADCAST: Address = Address([0xff; 6]);

        /// Construct an Ethernet address from a sequence of octets, in big-endian.
        ///
        /// # Panics
        /// The function panics if `data` is not six octets long.
        pub fn from_bytes(data: &[u8]) -> Address {
            let mut bytes = [0; 6];
            bytes.copy_from_slice(data);
            Address(bytes)
        }

        /// Return an Ethernet address as a sequence of octets, in big-endian.
        pub fn as_bytes(&self) -> &[u8] {
            &self.0
        }

        /// Query whether the address is an unicast address.
        pub fn is_unicast(&self) -> bool {
            !(self.is_broadcast() || self.is_multicast())
        }

        /// Query whether this address is the broadcast address.
        pub fn is_broadcast(&self) -> bool {
            *self == Self::BROADCAST
        }

        /// Query whether the "multicast" bit in the OUI is set.
        pub fn is_multicast(&self) -> bool {
            self.0[0] & 0x01 != 0
        }

        /// Query whether the "locally administered" bit in the OUI is set.
        pub fn is_local(&self) -> bool {
            self.0[0] & 0x02 != 0
        }
    }

    impl fmt::Display for Address {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let bytes = self.0;
            write!(
                f,
                "{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}",
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
            )
        }
    }
}

mod ipv4 {
    use super::{read_u32, Error, Result};
    use core::fmt;

    /// A four-octet IPv4 address.
    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
    pub struct Address(pub [u8; 4]);

    impl Address {
        /// An unspecified address.
        pub const UNSPECIFIED: Address = Address([0x00; 4]);

        /// The broadcast address.
        pub const BROADCAST: Address = Address([0xff; 4]);

        /// All multicast-capable nodes
        pub const MULTICAST_ALL_SYSTEMS: Address = Address([224, 0, 0, 1]);

        /// All multicast-capable routers
        pub const MULTICAST_ALL_ROUTERS: Address = Address([224, 0, 0, 2]);

        /// Construct an IPv4 address from parts.
        pub const fn new(a0: u8, a1: u8, a2: u8, a3: u8) -> Address {
            Address([a0, a1, a2, a3])
        }

        /// Construct an IPv4 address from a sequence of octets, in big-endian.
        ///
        /// # Panics
        /// The function panics if `data` is not four octets long.
        pub fn from_bytes(data: &[u8]) -> Address {
            let mut bytes = [0; 4];
            bytes.copy_from_slice(data);
            Address(bytes)
        }

        /// Return an IPv4 address as a sequence of octets, in big-endian.
        pub fn as_bytes(&self) -> &[u8] {
            &self.0
        }

        /// Query whether the address is an unicast address.
        pub fn is_unicast(&self) -> bool {
            !(self.is_broadcast() || self.is_multicast() || self.is_unspecified())
        }

        /// Query whether the address is the broadcast address.
        pub fn is_broadcast(&self) -> bool {
            self.0[0..4] == [255; 4]
        }

        /// Query whether the address is a multicast address.
        pub const fn is_multicast(&self) -> bool {
            self.0[0] & 0xf0 == 224
        }

        /// Query whether the address falls into the "unspecified" range.
        pub const fn is_unspecified(&self) -> bool {
            self.0[0] == 0
        }

        /// Query whether the address falls into the "link-local" range.
        pub fn is_link_local(&self) -> bool {
            self.0[0..2] == [169, 254]
        }

        /// Query whether the address falls into the "loopback" range.
        pub const fn is_loopback(&self) -> bool {
            self.0[0] == 127
        }
    }

    impl fmt::Display for Address {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let bytes = self.0;
            write!(f, "{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
        }
    }

    /// A specification of an IPv4 CIDR block, containing an address and a variable-length
    /// subnet masking prefix length.
    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
    pub struct Cidr {
        address: Address,
        prefix_len: u8,
    }

    impl Cidr {
        /// Create an IPv4 CIDR block from the given address and prefix length.
        ///
        /// # Panics
        /// This function panics if the prefix length is larger than 32.
        #[allow(clippy::no_effect)]
        pub const fn new(address: Address, prefix_len: u8) -> Cidr {
            // Replace with const panic (or assert) when stabilized
            // see: https://github.com/rust-lang/rust/issues/51999
            ["Prefix length should be <= 32"][(prefix_len > 32) as usize];
            Cidr {
                address,
                prefix_len,
            }
        }

        /// Create an IPv4 CIDR block from the given address and network mask.
        pub fn from_netmask(addr: Address, netmask: Address) -> Result<Cidr> {
            let netmask = read_u32(&netmask.0[..]);
            if netmask.leading_zeros() == 0 && netmask.trailing_zeros() == netmask.count_zeros() {
                Ok(Cidr {
                    address: addr,
                    prefix_len: netmask.count_ones() as u8,
                })
            } else {
                Err(Error::Illegal)
            }
        }

        /// Return the address of this IPv4 CIDR block.
        pub const fn address(&self) -> Address {
            self.address
        }

        /// Return the prefix length of this IPv4 CIDR block.
        pub const fn prefix_len(&self) -> u8 {
            self.prefix_len
        }

        /// Return the network mask of this IPv4 CIDR.
        pub fn netmask(&self) -> Address {
            if self.prefix_len == 0 {
                return Address([0, 0, 0, 0]);
            }

            let number = 0xffffffffu32 << (32 - self.prefix_len);
            let data = [
                ((number >> 24) & 0xff) as u8,
                ((number >> 16) & 0xff) as u8,
                ((number >> 8) & 0xff) as u8,
                (number & 0xff) as u8,
            ];

            Address(data)
        }

        /// Return the broadcast address of this IPv4 CIDR.
        pub fn broadcast(&self) -> Option<Address> {
            let network = self.network();

            if network.prefix_len == 31 || network.prefix_len == 32 {
                return None;
            }

            let network_number = read_u32(&network.address.0[..]);
            let number = network_number | 0xffffffffu32 >> network.prefix_len;
            let data = [
                ((number >> 24) & 0xff) as u8,
                ((number >> 16) & 0xff) as u8,
                ((number >> 8) & 0xff) as u8,
                (number & 0xff) as u8,
            ];

            Some(Address(data))
        }

        /// Return the network block of this IPv4 CIDR.
        pub fn network(&self) -> Cidr {
            let mask = self.netmask().0;
            let network = [
                self.address.0[0] & mask[0],
                self.address.0[1] & mask[1],
                self.address.0[2] & mask[2],
                self.address.0[3] & mask[3],
            ];
            Cidr {
                address: Address(network),
                prefix_len: self.prefix_len,
            }
        }

        /// Query whether the subnetwork described by this IPv4 CIDR block contains
        /// the given address.
        pub fn contains_addr(&self, addr: &Address) -> bool {
            // right shift by 32 is not legal
            if self.prefix_len == 0 {
                return true;
            }

            let shift = 32 - self.prefix_len;
            let self_prefix = read_u32(self.address.as_bytes()) >> shift;
            let addr_prefix = read_u32(addr.as_bytes()) >> shift;
            self_prefix == addr_prefix
        }

        /// Query whether the subnetwork described by this IPv4 CIDR block contains
        /// the subnetwork described by the given IPv4 CIDR block.
        pub fn contains_subnet(&self, subnet: &Cidr) -> bool {
            self.prefix_len <= subnet.prefix_len && self.contains_addr(&subnet.address)
        }
    }

    impl fmt::Display for Cidr {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{}/{}", self.address, self.prefix_len)
        }
    }
}

mod ipv6 {
    use super::ipv4;
    use super::{read_u16, write_u16};
    use core::fmt;

    /// A sixteen-octet IPv6 address.
    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
    pub struct Address(pub [u8; 16]);

    impl Address {
        /// The [unspecified address].
        ///
        /// [unspecified address]: https://tools.ietf.org/html/rfc4291#section-2.5.2
        pub const UNSPECIFIED: Address = Address([0x00; 16]);

        /// The link-local [all routers multicast address].
        ///
        /// [all routers multicast address]: https://tools.ietf.org/html/rfc4291#section-2.7.1
        pub const LINK_LOCAL_ALL_NODES: Address = Address([
            0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ]);

        /// The link-local [all nodes multicast address].
        ///
        /// [all nodes multicast address]: https://tools.ietf.org/html/rfc4291#section-2.7.1
        pub const LINK_LOCAL_ALL_ROUTERS: Address = Address([
            0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02,
        ]);

        /// The [loopback address].
        ///
        /// [loopback address]: https://tools.ietf.org/html/rfc4291#section-2.5.3
        pub const LOOPBACK: Address = Address([
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ]);

        /// Construct an IPv6 address from parts.
        #[allow(clippy::too_many_arguments)]
        pub fn new(
            a0: u16,
            a1: u16,
            a2: u16,
            a3: u16,
            a4: u16,
            a5: u16,
            a6: u16,
            a7: u16,
        ) -> Address {
            let mut addr = [0u8; 16];
            write_u16(&mut addr[0..2], a0);
            write_u16(&mut addr[2..4], a1);
            write_u16(&mut addr[4..6], a2);
            write_u16(&mut addr[6..8], a3);
            write_u16(&mut addr[8..10], a4);
            write_u16(&mut addr[10..12], a5);
            write_u16(&mut addr[12..14], a6);
            write_u16(&mut addr[14..16], a7);
            Address(addr)
        }

        /// Construct an IPv6 address from a sequence of octets, in big-endian.
        ///
        /// # Panics
        /// The function panics if `data` is not sixteen octets long.
        pub fn from_bytes(data: &[u8]) -> Address {
            let mut bytes = [0; 16];
            bytes.copy_from_slice(data);
            Address(bytes)
        }

        /// Construct an IPv6 address from a sequence of words, in big-endian.
        ///
        /// # Panics
        /// The function panics if `data` is not 8 words long.
        pub fn from_parts(data: &[u16]) -> Address {
            assert!(data.len() >= 8);
            let mut bytes = [0; 16];
            for (word_idx, chunk) in bytes.chunks_mut(2).enumerate() {
                write_u16(chunk, data[word_idx]);
            }
            Address(bytes)
        }

        /// Write a IPv6 address to the given slice.
        ///
        /// # Panics
        /// The function panics if `data` is not 8 words long.
        pub fn write_parts(&self, data: &mut [u16]) {
            assert!(data.len() >= 8);
            for (i, chunk) in self.0.chunks(2).enumerate() {
                data[i] = read_u16(chunk);
            }
        }

        /// Return an IPv6 address as a sequence of octets, in big-endian.
        pub fn as_bytes(&self) -> &[u8] {
            &self.0
        }

        /// Query whether the IPv6 address is an [unicast address].
        ///
        /// [unicast address]: https://tools.ietf.org/html/rfc4291#section-2.5
        pub fn is_unicast(&self) -> bool {
            !(self.is_multicast() || self.is_unspecified())
        }

        /// Query whether the IPv6 address is a [multicast address].
        ///
        /// [multicast address]: https://tools.ietf.org/html/rfc4291#section-2.7
        pub fn is_multicast(&self) -> bool {
            self.0[0] == 0xff
        }

        /// Query whether the IPv6 address is the [unspecified address].
        ///
        /// [unspecified address]: https://tools.ietf.org/html/rfc4291#section-2.5.2
        pub fn is_unspecified(&self) -> bool {
            self.0 == [0x00; 16]
        }

        /// Query whether the IPv6 address is in the [link-local] scope.
        ///
        /// [link-local]: https://tools.ietf.org/html/rfc4291#section-2.5.6
        pub fn is_link_local(&self) -> bool {
            self.0[0..8] == [0xfe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        }

        /// Query whether the IPv6 address is the [loopback address].
        ///
        /// [loopback address]: https://tools.ietf.org/html/rfc4291#section-2.5.3
        pub fn is_loopback(&self) -> bool {
            *self == Self::LOOPBACK
        }

        /// Query whether the IPv6 address is an [IPv4 mapped IPv6 address].
        ///
        /// [IPv4 mapped IPv6 address]: https://tools.ietf.org/html/rfc4291#section-2.5.5.2
        pub fn is_ipv4_mapped(&self) -> bool {
            self.0[0..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]
        }

        /// Convert an IPv4 mapped IPv6 address to an IPv4 address.
        pub fn as_ipv4(&self) -> Option<ipv4::Address> {
            if self.is_ipv4_mapped() {
                Some(ipv4::Address::new(
                    self.0[12], self.0[13], self.0[14], self.0[15],
                ))
            } else {
                None
            }
        }

        /// Helper function used to mask an addres given a prefix.
        ///
        /// # Panics
        /// This function panics if `mask` is greater than 128.
        pub(super) fn mask(&self, mask: u8) -> [u8; 16] {
            assert!(mask <= 128);
            let mut bytes = [0u8; 16];
            let idx = (mask as usize) / 8;
            let modulus = (mask as usize) % 8;
            let (first, second) = self.0.split_at(idx);
            bytes[0..idx].copy_from_slice(first);
            if idx < 16 {
                let part = second[0];
                bytes[idx] = part & (!(0xff >> modulus) as u8);
            }
            bytes
        }

        /// The solicited node for the given unicast address.
        ///
        /// # Panics
        /// This function panics if the given address is not
        /// unicast.
        pub fn solicited_node(&self) -> Address {
            assert!(self.is_unicast());
            // FF02:0:0:0:0:1:FF00::/104 and the address's last 24 bits
            // (RFC 4291, 2.7.1).
            let mut bytes = Cidr::SOLICITED_NODE_PREFIX.address.0;
            bytes[13..].copy_from_slice(&self.0[13..]);
            Address(bytes)
        }
    }

    impl fmt::Display for Address {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            if self.is_ipv4_mapped() {
                return write!(
                    f,
                    "::ffff:{}.{}.{}.{}",
                    self.0[12], self.0[13], self.0[14], self.0[15]
                );
            }

            // The string representation of an IPv6 address should
            // collapse a series of 16 bit sections that evaluate
            // to 0 to "::"
            //
            // See https://tools.ietf.org/html/rfc4291#section-2.2
            // for details.
            enum State {
                Head,
                HeadBody,
                Tail,
                TailBody,
            }
            let mut words = [0u16; 8];
            self.write_parts(&mut words);
            let mut state = State::Head;
            for word in words.iter() {
                state = match (*word, &state) {
                    // Once a u16 equal to zero write a double colon and
                    // skip to the next non-zero u16.
                    (0, &State::Head) | (0, &State::HeadBody) => {
                        write!(f, "::")?;
                        State::Tail
                    }
                    // Continue iterating without writing any characters until
                    // we hit anothing non-zero value.
                    (0, &State::Tail) => State::Tail,
                    // When the state is Head or Tail write a u16 in hexadecimal
                    // without the leading colon if the value is not 0.
                    (_, &State::Head) => {
                        write!(f, "{:x}", word)?;
                        State::HeadBody
                    }
                    (_, &State::Tail) => {
                        write!(f, "{:x}", word)?;
                        State::TailBody
                    }
                    // Write the u16 with a leading colon when parsing a value
                    // that isn't the first in a section
                    (_, &State::HeadBody) | (_, &State::TailBody) => {
                        write!(f, ":{:x}", word)?;
                        state
                    }
                }
            }
            Ok(())
        }
    }

    /// Convert the given IPv4 address into a IPv4-mapped IPv6 address
    impl From<ipv4::Address> for Address {
        fn from(address: ipv4::Address) -> Self {
            let octets = address.0;
            Address([
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, octets[0], octets[1], octets[2],
                octets[3],
            ])
        }
    }

    /// A specification of an IPv6 CIDR block, containing an address and a variable-length
    /// subnet masking prefix length.

    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
    pub struct Cidr {
        address: Address,
        prefix_len: u8,
    }

    impl Cidr {
        /// The [solicited node prefix].
        ///
        /// [solicited node prefix]: https://tools.ietf.org/html/rfc4291#section-2.7.1
        pub const SOLICITED_NODE_PREFIX: Cidr = Cidr {
            address: Address([
                0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xff, 0x00,
                0x00, 0x00,
            ]),
            prefix_len: 104,
        };

        /// Create an IPv6 CIDR block from the given address and prefix length.
        ///
        /// # Panics
        /// This function panics if the prefix length is larger than 128.
        pub fn new(address: Address, prefix_len: u8) -> Cidr {
            assert!(prefix_len <= 128);
            Cidr {
                address,
                prefix_len,
            }
        }

        /// Return the address of this IPv6 CIDR block.
        pub fn address(&self) -> Address {
            self.address
        }

        /// Return the prefix length of this IPv6 CIDR block.
        pub fn prefix_len(&self) -> u8 {
            self.prefix_len
        }

        /// Query whether the subnetwork described by this IPv6 CIDR block contains
        /// the given address.
        pub fn contains_addr(&self, addr: &Address) -> bool {
            // The first `prefix_len` bits are the network's: `mask` keeps
            // that many from the front.
            self.address.mask(self.prefix_len) == addr.mask(self.prefix_len)
        }

        /// Query whether the subnetwork described by this IPV6 CIDR block contains
        /// the subnetwork described by the given IPv6 CIDR block.
        pub fn contains_subnet(&self, subnet: &Cidr) -> bool {
            self.prefix_len <= subnet.prefix_len && self.contains_addr(&subnet.address)
        }

        /// Return the network block of this IPv6 CIDR.
        pub fn network(&self) -> Cidr {
            Cidr {
                address: Address(self.address.mask(self.prefix_len)),
                prefix_len: self.prefix_len,
            }
        }
    }

    impl fmt::Display for Cidr {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            // https://tools.ietf.org/html/rfc4291#section-2.3
            write!(f, "{}/{}", self.address, self.prefix_len)
        }
    }
}

mod ip {
    use super::{Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};
    use core::fmt;

    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
    #[non_exhaustive]
    pub enum Address {
        /// An unspecified address.
        /// May be used as a placeholder for storage where the address is not assigned yet.
        #[default]
        Unspecified,
        /// An IPv4 address.
        Ipv4(Ipv4Address),
        /// An IPv6 address.
        Ipv6(Ipv6Address),
    }

    impl Address {
        /// Create an address wrapping an IPv4 address with the given octets.
        pub fn v4(a0: u8, a1: u8, a2: u8, a3: u8) -> Address {
            Address::Ipv4(Ipv4Address::new(a0, a1, a2, a3))
        }

        /// Create an address wrapping an IPv6 address with the given octets.
        #[allow(clippy::too_many_arguments)]
        pub fn v6(
            a0: u16,
            a1: u16,
            a2: u16,
            a3: u16,
            a4: u16,
            a5: u16,
            a6: u16,
            a7: u16,
        ) -> Address {
            Address::Ipv6(Ipv6Address::new(a0, a1, a2, a3, a4, a5, a6, a7))
        }

        /// Return an address as a sequence of octets, in big-endian.
        pub fn as_bytes(&self) -> &[u8] {
            match *self {
                Address::Unspecified => &[],
                Address::Ipv4(ref addr) => addr.as_bytes(),
                Address::Ipv6(ref addr) => addr.as_bytes(),
            }
        }

        /// Query whether the address is a valid unicast address.
        pub fn is_unicast(&self) -> bool {
            match *self {
                Address::Unspecified => false,
                Address::Ipv4(addr) => addr.is_unicast(),
                Address::Ipv6(addr) => addr.is_unicast(),
            }
        }

        /// Query whether the address is a valid multicast address.
        pub fn is_multicast(&self) -> bool {
            match *self {
                Address::Unspecified => false,
                Address::Ipv4(addr) => addr.is_multicast(),
                Address::Ipv6(addr) => addr.is_multicast(),
            }
        }

        /// Query whether the address is the broadcast address.
        pub fn is_broadcast(&self) -> bool {
            match *self {
                Address::Unspecified => false,
                Address::Ipv4(addr) => addr.is_broadcast(),
                Address::Ipv6(_) => false,
            }
        }

        /// Query whether the address names the local host (127.0.0.0/8, or `::1`).
        ///
        /// RFC 1122 section 3.2.1.3 calls this the "internal host loopback
        /// address": a datagram addressed there must never leave the host, and a
        /// host must never forward one it receives on a real link.
        pub fn is_loopback(&self) -> bool {
            match *self {
                Address::Unspecified => false,
                Address::Ipv4(addr) => addr.is_loopback(),
                Address::Ipv6(addr) => addr.is_loopback(),
            }
        }

        /// Query whether the address falls into the "unspecified" range.
        pub fn is_unspecified(&self) -> bool {
            match *self {
                Address::Unspecified => true,
                Address::Ipv4(addr) => addr.is_unspecified(),
                Address::Ipv6(addr) => addr.is_unspecified(),
            }
        }

        /// Return an unspecified address that has the same IP version as `self`.
        pub fn as_unspecified(&self) -> Address {
            match *self {
                Address::Unspecified => Address::Unspecified,
                Address::Ipv4(_) => Address::Ipv4(Ipv4Address::UNSPECIFIED),
                Address::Ipv6(_) => Address::Ipv6(Ipv6Address::UNSPECIFIED),
            }
        }

        /// If `self` is a CIDR-compatible subnet mask, return `Some(prefix_len)`,
        /// where `prefix_len` is the number of leading zeroes. Return `None` otherwise.
        pub fn prefix_len(&self) -> Option<u8> {
            let mut ones = true;
            let mut prefix_len = 0;
            for byte in self.as_bytes() {
                let mut mask = 0x80;
                for _ in 0..8 {
                    let one = *byte & mask != 0;
                    if ones {
                        // Expect 1s until first 0
                        if one {
                            prefix_len += 1;
                        } else {
                            ones = false;
                        }
                    } else if one {
                        // 1 where 0 was expected
                        return None;
                    }
                    mask >>= 1;
                }
            }
            Some(prefix_len)
        }
    }

    impl From<Ipv4Address> for Address {
        fn from(addr: Ipv4Address) -> Self {
            Address::Ipv4(addr)
        }
    }

    impl From<Ipv6Address> for Address {
        fn from(addr: Ipv6Address) -> Self {
            Address::Ipv6(addr)
        }
    }

    impl fmt::Display for Address {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match *self {
                Address::Unspecified => write!(f, "*"),
                Address::Ipv4(addr) => write!(f, "{}", addr),
                Address::Ipv6(addr) => write!(f, "{}", addr),
            }
        }
    }

    /// subnet masking prefix length.
    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
    #[non_exhaustive]
    pub enum Cidr {
        Ipv4(Ipv4Cidr),
        Ipv6(Ipv6Cidr),
    }

    impl Cidr {
        /// Create a CIDR block from the given address and prefix length.
        ///
        /// # Panics
        /// This function panics if the given address is unspecified, or
        /// the given prefix length is invalid for the given address.
        pub fn new(addr: Address, prefix_len: u8) -> Cidr {
            match addr {
                Address::Ipv4(addr) => Cidr::Ipv4(Ipv4Cidr::new(addr, prefix_len)),
                Address::Ipv6(addr) => Cidr::Ipv6(Ipv6Cidr::new(addr, prefix_len)),
                Address::Unspecified => {
                    panic!("a CIDR block cannot be based on an unspecified address")
                }
            }
        }

        /// Return the IP address of this CIDR block.
        pub fn address(&self) -> Address {
            match *self {
                Cidr::Ipv4(cidr) => Address::Ipv4(cidr.address()),
                Cidr::Ipv6(cidr) => Address::Ipv6(cidr.address()),
            }
        }

        /// Return the prefix length of this CIDR block.
        pub fn prefix_len(&self) -> u8 {
            match *self {
                Cidr::Ipv4(cidr) => cidr.prefix_len(),
                Cidr::Ipv6(cidr) => cidr.prefix_len(),
            }
        }

        /// Query whether the subnetwork described by this CIDR block contains
        /// the given address.
        pub fn contains_addr(&self, addr: &Address) -> bool {
            match (self, addr) {
                (Cidr::Ipv4(cidr), Address::Ipv4(addr)) => cidr.contains_addr(addr),
                (Cidr::Ipv6(cidr), Address::Ipv6(addr)) => cidr.contains_addr(addr),
                (Cidr::Ipv4(_), Address::Ipv6(_)) | (Cidr::Ipv6(_), Address::Ipv4(_)) => false,
                (_, Address::Unspecified) =>
                // a fully unspecified address covers both IPv4 and IPv6,
                // and no CIDR block can do that.
                {
                    false
                }
            }
        }

        /// Query whether the subnetwork described by this CIDR block contains
        /// the subnetwork described by the given CIDR block.
        pub fn contains_subnet(&self, subnet: &Cidr) -> bool {
            match (self, subnet) {
                (Cidr::Ipv4(cidr), Cidr::Ipv4(other)) => cidr.contains_subnet(other),
                (Cidr::Ipv6(cidr), Cidr::Ipv6(other)) => cidr.contains_subnet(other),
                (Cidr::Ipv4(_), Cidr::Ipv6(_)) | (Cidr::Ipv6(_), Cidr::Ipv4(_)) => false,
            }
        }
    }

    impl From<Ipv4Cidr> for Cidr {
        fn from(addr: Ipv4Cidr) -> Self {
            Cidr::Ipv4(addr)
        }
    }

    impl From<Ipv6Cidr> for Cidr {
        fn from(addr: Ipv6Cidr) -> Self {
            Cidr::Ipv6(addr)
        }
    }

    impl fmt::Display for Cidr {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match *self {
                Cidr::Ipv4(cidr) => write!(f, "{}", cidr),
                Cidr::Ipv6(cidr) => write!(f, "{}", cidr),
            }
        }
    }

    /// An internet endpoint address.
    ///
    /// An endpoint can be constructed from a port, in which case the address is unspecified.
    #[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
    pub struct Endpoint {
        pub addr: Address,
        pub port: u16,
    }

    impl Endpoint {
        /// An endpoint with unspecified address and port.
        pub const UNSPECIFIED: Endpoint = Endpoint {
            addr: Address::Unspecified,
            port: 0,
        };

        /// Create an endpoint address from given address and port.
        pub fn new(addr: Address, port: u16) -> Endpoint {
            Endpoint { addr, port }
        }

        /// Query whether the endpoint has a specified address and port.
        pub fn is_specified(&self) -> bool {
            !self.addr.is_unspecified() && self.port != 0
        }
    }

    impl fmt::Display for Endpoint {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{}:{}", self.addr, self.port)
        }
    }

    impl From<u16> for Endpoint {
        fn from(port: u16) -> Endpoint {
            Endpoint {
                addr: Address::Unspecified,
                port,
            }
        }
    }

    impl<T: Into<Address>> From<(T, u16)> for Endpoint {
        fn from((addr, port): (T, u16)) -> Endpoint {
            Endpoint {
                addr: addr.into(),
                port,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::format;

    /// The text of an address is what `/proc`, `ip` and the logs show, and
    /// what the tests of the rest of the kernel compare against.
    #[test]
    fn an_address_prints_as_it_always_did() {
        assert_eq!(
            format!("{}", EthernetAddress([0x52, 0x54, 0, 0x12, 0x34, 0x56])),
            "52-54-00-12-34-56"
        );
        assert_eq!(format!("{}", Ipv4Address::new(10, 0, 2, 15)), "10.0.2.15");
        assert_eq!(format!("{}", Ipv6Address::LOOPBACK), "::1");
        assert_eq!(
            format!(
                "{}",
                Ipv6Address::new(0xfe80, 0, 0, 0, 0x5054, 0xff, 0xfe12, 0x3456)
            ),
            "fe80::5054:ff:fe12:3456"
        );
        assert_eq!(
            format!("{}", IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)),
            "10.0.2.15/24"
        );
        assert_eq!(
            format!("{}", IpEndpoint::new(IpAddress::v4(10, 0, 2, 2), 80)),
            "10.0.2.2:80"
        );
    }

    #[test]
    fn a_network_holds_its_own_addresses_and_no_other() {
        let net = Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24);
        assert_eq!(net.network().address(), Ipv4Address::new(10, 0, 2, 0));
        assert_eq!(net.netmask(), Ipv4Address::new(255, 255, 255, 0));
        assert_eq!(net.broadcast(), Some(Ipv4Address::new(10, 0, 2, 255)));
        assert!(net.contains_addr(&Ipv4Address::new(10, 0, 2, 2)));
        assert!(!net.contains_addr(&Ipv4Address::new(10, 0, 3, 2)));
        // Every address is in the network of no bits, which is what the
        // default route is.
        let all = Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0);
        assert!(all.contains_addr(&Ipv4Address::new(192, 168, 1, 1)));

        let net6 = Ipv6Cidr::new(Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 1), 64);
        assert!(net6.contains_addr(&Ipv6Address::new(0xfec0, 0, 0, 0, 1, 2, 3, 4)));
        assert!(!net6.contains_addr(&Ipv6Address::new(0xfec1, 0, 0, 0, 1, 2, 3, 4)));
    }

    #[test]
    fn a_netmask_with_a_hole_is_no_network() {
        let addr = Ipv4Address::new(10, 0, 2, 15);
        assert_eq!(
            Ipv4Cidr::from_netmask(addr, Ipv4Address::new(255, 255, 255, 0)),
            Ok(Ipv4Cidr::new(addr, 24))
        );
        assert_eq!(
            Ipv4Cidr::from_netmask(addr, Ipv4Address::new(255, 0, 255, 0)),
            Err(Error::Illegal)
        );
    }

    #[test]
    fn the_kinds_of_address_are_told_apart() {
        assert!(Ipv4Address::new(10, 0, 2, 15).is_unicast());
        assert!(!Ipv4Address::UNSPECIFIED.is_unicast());
        assert!(Ipv4Address::BROADCAST.is_broadcast());
        assert!(Ipv4Address::new(224, 0, 0, 1).is_multicast());
        assert!(Ipv4Address::new(127, 0, 0, 1).is_loopback());
        assert!(Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 1).is_multicast());
        assert!(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).is_link_local());
        assert!(EthernetAddress([0x52, 0x54, 0, 0, 0, 1]).is_unicast());
        assert!(EthernetAddress::BROADCAST.is_broadcast());
        assert!(IpAddress::Unspecified.is_unspecified());
        assert_eq!(IpAddress::default(), IpAddress::Unspecified);
        // An IPv4 address written as IPv6 is the mapped one.
        let mapped: Ipv6Address = Ipv4Address::new(10, 0, 2, 2).into();
        assert_eq!(format!("{}", mapped), "::ffff:10.0.2.2");
    }
}
