//! Port packet data structure definition.

use super::*;
use core::fmt::{Debug, Formatter};

// C struct: for storing

/// A packet sent through a port.
///
/// This is the wire struct, and the two scalars are plain integers on purpose.
/// `sys_port_queue` hands the caller's own buffer to
/// `UserInPtr::<PortPacket>::read`, so every byte here is whatever the process
/// wrote, and turning an arbitrary `u32` into a `#[repr(u32)]` enum is
/// undefined behaviour. Neither of the two enums that used to sit here is
/// dense: `PacketType` has no variant 8, and `ZxError` is a sparse `-76..=0`.
#[repr(C)]
pub struct PortPacket {
    pub key: u64,
    /// One of [`PacketType`], raw. Read it with [`PortPacket::packet_type`].
    pub type_: u32,
    /// A `zx_status_t`. The kernel carries a user packet's status through
    /// verbatim, so it is never interpreted here.
    pub status: i32,
    pub data: Payload,
}

impl PortPacket {
    /// The packet's type, or `INVALID_ARGS` if the caller wrote something that
    /// is not one.
    pub fn packet_type(&self) -> ZxResult<PacketType> {
        PacketType::from_raw(self.type_)
    }

    /// The packet's payload, read as its type says.
    pub fn decode(&self) -> ZxResult<PortPacketRepr> {
        Ok(PortPacketRepr {
            key: self.key,
            status: self.status,
            data: PayloadRepr::decode(self.packet_type()?, &self.data)?,
        })
    }
}

// reference: zircon/system/public/zircon/syscalls/port.h ZX_PKT_TYPE_*
/// The type of a packet.
#[repr(u32)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PacketType {
    User = 0,
    SignalOne = 1,
    SignalRep = 2,
    GuestBell = 3,
    GuestMem = 4,
    GuestIo = 5,
    GuestVcpu = 6,
    Interrupt = 7,
    PageRequest = 9,
}

impl PacketType {
    /// Convert the raw `zx_packet_type_t` a process wrote.
    ///
    /// Written out rather than range-checked: there is no type 8, and adding a
    /// variant without listing it here should be a compile error, not a
    /// silently rejected packet.
    pub fn from_raw(raw: u32) -> ZxResult<Self> {
        Ok(match raw {
            0 => Self::User,
            1 => Self::SignalOne,
            2 => Self::SignalRep,
            3 => Self::GuestBell,
            4 => Self::GuestMem,
            5 => Self::GuestIo,
            6 => Self::GuestVcpu,
            7 => Self::Interrupt,
            9 => Self::PageRequest,
            _ => return Err(ZxError::INVALID_ARGS),
        })
    }
}

#[repr(C)]
/// The data carried by a packet
pub union Payload {
    user: PacketUser,
    signal: PacketSignal,
    guest_bell: PacketGuestBell,
    guest_mem: PacketGuestMem,
    guest_io: PacketGuestIo,
    guest_vcpu: PacketGuestVcpu,
    interrupt: PacketInterrupt,
    // TODO: PacketPageRequest
}

pub type PacketUser = [u8; 32];

#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketSignal {
    pub trigger: Signal,
    pub observed: Signal,
    pub count: u64,
    pub timestamp: u64,
    pub _reserved1: u64,
}

#[repr(C)]
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketGuestBell {
    pub addr: u64,
    pub _reserved0: u64,
    pub _reserved1: u64,
    pub _reserved2: u64,
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketGuestMem {
    pub addr: u64,
    pub inst_len: u8,
    pub inst_buf: [u8; 15],
    pub default_operand_size: u8,
    pub _reserved: [u8; 7],
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketGuestMem {
    pub addr: u64,
    pub access_size: u8,
    pub sign_extend: bool,
    pub xt: u8,
    pub read: bool,
    pub _padding1: [u8; 4],
    pub data: u64,
    pub _reserved: u64,
}

#[cfg(target_arch = "riscv64")]
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketGuestMem {
    pub addr: u64,
    //保持32字节的空间
    pub _reserved: u64,
    pub _reserved1: u64,
    pub _reserved2: u64,
}

#[repr(C)]
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketGuestIo {
    pub port: u16,
    pub access_size: u8,
    pub input: bool,
    pub data: [u8; 4],
    pub _reserved0: u64,
    pub _reserved1: u64,
    pub _reserved2: u64,
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PacketGuestVcpuType {
    VcpuInterrupt = 0,
    VcpuStartup = 1,
}

impl PacketGuestVcpuType {
    /// Convert the raw type byte a process wrote.
    pub fn from_raw(raw: u8) -> ZxResult<Self> {
        Ok(match raw {
            0 => Self::VcpuInterrupt,
            1 => Self::VcpuStartup,
            _ => return Err(ZxError::INVALID_ARGS),
        })
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union PacketGuestVcpuData {
    interrupt: PacketGuestVcpuInterrupt,
    startup: PacketGuestVcpuStartup,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketGuestVcpuInterrupt {
    mask: u64,
    vector: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketGuestVcpuStartup {
    id: u64,
    entry: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct PacketGuestVcpu {
    pub data: PacketGuestVcpuData,
    /// One of [`PacketGuestVcpuType`], raw: this struct is a member of
    /// `Payload`, so the byte is the caller's as well.
    pub type_: u8,
    pub _padding1: [u8; 7],
    pub _reserved: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct PacketInterrupt {
    pub timestamp: i64,
    pub _reserved0: u64,
    pub _reserved1: u64,
    pub _reserved2: u64,
}

// Rust struct: for internal constructing and debugging

/// A high-level representation of a packet sent through a port.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PortPacketRepr {
    pub key: u64,
    /// A `zx_status_t`, as it goes on the wire.
    pub status: i32,
    pub data: PayloadRepr,
}

/// A high-level representation of a packet payload.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PayloadRepr {
    User(PacketUser),
    Signal(PacketSignal),
    GuestBell(PacketGuestBell),
    GuestMem(PacketGuestMem),
    GuestIo(PacketGuestIo),
    GuestVcpu(PacketGuestVcpu),
    Interrupt(PacketInterrupt),
}

impl PayloadRepr {
    fn type_(&self) -> PacketType {
        match self {
            PayloadRepr::User(_) => PacketType::User,
            PayloadRepr::Signal(_) => PacketType::SignalOne,
            PayloadRepr::GuestBell(_) => PacketType::GuestBell,
            PayloadRepr::GuestMem(_) => PacketType::GuestMem,
            PayloadRepr::GuestIo(_) => PacketType::GuestIo,
            PayloadRepr::GuestVcpu(_) => PacketType::GuestVcpu,
            PayloadRepr::Interrupt(_) => PacketType::Interrupt,
        }
    }
    fn encode(&self) -> Payload {
        match *self {
            PayloadRepr::User(user) => Payload { user },
            PayloadRepr::Signal(signal) => Payload { signal },
            PayloadRepr::GuestBell(guest_bell) => Payload { guest_bell },
            PayloadRepr::GuestMem(guest_mem) => Payload { guest_mem },
            PayloadRepr::GuestIo(guest_io) => Payload { guest_io },
            PayloadRepr::GuestVcpu(guest_vcpu) => Payload { guest_vcpu },
            PayloadRepr::Interrupt(interrupt) => Payload { interrupt },
        }
    }
    #[allow(unsafe_code)]
    fn decode(type_: PacketType, data: &Payload) -> ZxResult<Self> {
        Ok(unsafe {
            match type_ {
                PacketType::User => PayloadRepr::User(data.user),
                PacketType::SignalOne => PayloadRepr::Signal(data.signal),
                PacketType::SignalRep => PayloadRepr::Signal(data.signal),
                PacketType::GuestBell => PayloadRepr::GuestBell(data.guest_bell),
                PacketType::GuestMem => PayloadRepr::GuestMem(data.guest_mem),
                PacketType::GuestIo => PayloadRepr::GuestIo(data.guest_io),
                PacketType::GuestVcpu => PayloadRepr::GuestVcpu(data.guest_vcpu),
                PacketType::Interrupt => PayloadRepr::Interrupt(data.interrupt),
                // `ZX_PKT_TYPE_PAGE_REQUEST` is a type a process may write and
                // `Payload` has no member for it. This used to be a catch-all
                // `unimplemented!()`, so queueing one panicked the kernel. The
                // arm is written out so that adding a type stops compiling
                // here instead of falling into it.
                PacketType::PageRequest => return Err(ZxError::NOT_SUPPORTED),
            }
        })
    }
}

impl From<PortPacketRepr> for PortPacket {
    fn from(r: PortPacketRepr) -> Self {
        PortPacket {
            key: r.key,
            type_: r.data.type_() as u32,
            status: r.status,
            data: r.data.encode(),
        }
    }
}

impl PartialEq for PacketGuestVcpu {
    #[allow(unsafe_code)]
    fn eq(&self, other: &Self) -> bool {
        if !self.type_.eq(&other.type_)
            || !self._padding1.eq(&other._padding1)
            || !self._reserved.eq(&other._reserved)
        {
            return false;
        }
        unsafe {
            match PacketGuestVcpuType::from_raw(self.type_) {
                Ok(PacketGuestVcpuType::VcpuInterrupt) => {
                    self.data.interrupt.eq(&other.data.interrupt)
                }
                Ok(PacketGuestVcpuType::VcpuStartup) => self.data.startup.eq(&other.data.startup),
                // A type nobody recognises names no member of the union, so
                // there is nothing left to compare: the two packets agree on
                // everything the kernel can read. Comparing the union's bytes
                // instead would read the padding of whichever member was
                // written, which is uninitialised.
                Err(_) => true,
            }
        }
    }
}

impl Eq for PacketGuestVcpu {}

impl Debug for PacketGuestVcpu {
    #[allow(unsafe_code)]
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        let mut out = f.debug_struct("PacketGuestVcpu");
        unsafe {
            match PacketGuestVcpuType::from_raw(self.type_) {
                Ok(PacketGuestVcpuType::VcpuInterrupt) => out.field("data", &self.data.interrupt),
                Ok(PacketGuestVcpuType::VcpuStartup) => out.field("data", &self.data.startup),
                Err(_) => out.field("data", &"<unknown vcpu packet type>"),
            };
        }
        out.field("type_", &self.type_)
            .field("_padding1", &self._padding1)
            .field("_reserved", &self._reserved)
            .finish()
    }
}

impl Debug for PortPacket {
    /// Never fails, whatever the caller wrote: `sys_port_queue` logs the packet
    /// it was handed before anything validates it.
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        let mut out = f.debug_struct("PortPacket");
        out.field("key", &self.key);
        match self.packet_type() {
            Ok(type_) => {
                out.field("type_", &type_);
            }
            Err(_) => {
                out.field("type_", &self.type_);
            }
        }
        out.field("status", &self.status);
        match self.decode() {
            Ok(repr) => {
                out.field("data", &repr.data);
            }
            Err(_) => {
                out.field("data", &"<no payload for this type>");
            }
        }
        out.finish()
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// Build the forty-eight bytes a process would hand `zx_port_queue` and
    /// read them back as the kernel does, so that the layout is part of what
    /// these tests check and not an assumption.
    #[allow(unsafe_code)]
    fn packet_from_bytes(key: u64, type_: u32, status: i32, payload: [u8; 32]) -> PortPacket {
        let mut bytes = [0u8; 48];
        bytes[0..8].copy_from_slice(&key.to_le_bytes());
        bytes[8..12].copy_from_slice(&type_.to_le_bytes());
        bytes[12..16].copy_from_slice(&status.to_le_bytes());
        bytes[16..48].copy_from_slice(&payload);
        unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const PortPacket) }
    }

    /// `sys_port_wait` asserts this at run time on every wait, and the byte
    /// builder above depends on it too.
    #[test]
    fn the_wire_packet_is_forty_eight_bytes() {
        assert_eq!(core::mem::size_of::<PortPacket>(), 48);
        let packet = packet_from_bytes(0x1122_3344_5566_7788, 7, -3, [0xab; 32]);
        assert_eq!(packet.key, 0x1122_3344_5566_7788);
        assert_eq!(packet.type_, 7);
        assert_eq!(packet.status, -3);
    }

    /// `sys_port_queue` hands the caller's buffer to `UserInPtr::read`, so
    /// `type_` is whatever the process wrote. There is no type 8, and nothing
    /// above 9.
    #[test]
    fn a_packet_type_the_caller_invented_is_rejected() {
        for (raw, expected) in [
            (0, PacketType::User),
            (1, PacketType::SignalOne),
            (2, PacketType::SignalRep),
            (3, PacketType::GuestBell),
            (4, PacketType::GuestMem),
            (5, PacketType::GuestIo),
            (6, PacketType::GuestVcpu),
            (7, PacketType::Interrupt),
            (9, PacketType::PageRequest),
        ] {
            assert_eq!(PacketType::from_raw(raw), Ok(expected));
            assert_eq!(expected as u32, raw);
        }
        // 8 sits inside the range and is still not a type.
        for raw in [8, 10, 11, 0x8000, u32::MAX] {
            assert_eq!(PacketType::from_raw(raw), Err(ZxError::INVALID_ARGS));
        }
    }

    /// `ZX_PKT_TYPE_PAGE_REQUEST` is a type a process may write and `Payload`
    /// has no member for it, so decoding it used to be `unimplemented!()`.
    #[test]
    fn a_page_request_packet_is_refused_rather_than_panicking() {
        let packet = packet_from_bytes(1, PacketType::PageRequest as u32, 0, [0; 32]);
        assert_eq!(packet.packet_type(), Ok(PacketType::PageRequest));
        assert_eq!(packet.decode().err(), Some(ZxError::NOT_SUPPORTED));
    }

    /// `sys_port_queue` logs the packet it was handed before anything looks at
    /// it, and the log ran the whole decode. Every value a process can write
    /// has to come out as text.
    #[test]
    fn debugging_a_packet_the_caller_wrote_never_panics() {
        for type_ in [0u32, 1, 6, 7, 8, 9, 10, 0x8000, u32::MAX] {
            for payload in [[0u8; 32], [0xff; 32]] {
                let packet = packet_from_bytes(1, type_, -99, payload);
                let text = format!("{:?}", packet);
                assert!(text.contains("status: -99"), "{}", text);
            }
        }
        let unknown = format!("{:?}", packet_from_bytes(1, 8, 0, [0; 32]));
        assert!(unknown.contains("type_: 8"), "{}", unknown);
        assert!(
            unknown.contains("<no payload for this type>"),
            "{}",
            unknown
        );
    }

    /// A vcpu packet keeps its own discriminant inside `Payload`, so that byte
    /// is the caller's as well, and the two places that read it matched on it
    /// with no other arm.
    #[test]
    fn a_vcpu_packet_type_the_caller_invented_is_rejected() {
        assert_eq!(
            PacketGuestVcpuType::from_raw(0),
            Ok(PacketGuestVcpuType::VcpuInterrupt)
        );
        assert_eq!(
            PacketGuestVcpuType::from_raw(1),
            Ok(PacketGuestVcpuType::VcpuStartup)
        );
        for raw in [2u8, 3, 0x80, u8::MAX] {
            assert_eq!(
                PacketGuestVcpuType::from_raw(raw),
                Err(ZxError::INVALID_ARGS)
            );
        }

        let mut payload = [0u8; 32];
        payload[16] = 2;
        let packet = packet_from_bytes(1, PacketType::GuestVcpu as u32, 0, payload);
        let vcpu = match packet.decode().unwrap().data {
            PayloadRepr::GuestVcpu(vcpu) => vcpu,
            other => panic!("expected a vcpu packet, got {:?}", other),
        };
        assert_eq!(vcpu.type_, 2);
        // Eq must stay reflexive whatever the byte says.
        assert_eq!(vcpu, vcpu);
        assert!(format!("{:?}", vcpu).contains("<unknown vcpu packet type>"));
    }

    /// The status is a `zx_status_t`, not one of the forty-six `ZxError`
    /// variants: the kernel carries a user packet's through verbatim.
    #[test]
    fn a_status_that_is_not_a_zx_error_survives_the_round_trip() {
        for status in [0i32, 5, -3, -12345, i32::MIN, i32::MAX] {
            let packet = packet_from_bytes(7, PacketType::User as u32, status, [1; 32]);
            let repr = packet.decode().unwrap();
            assert_eq!(repr.status, status);
            assert_eq!(repr.key, 7);
            assert_eq!(PortPacket::from(repr).status, status);
        }
    }

    #[test]
    fn port_packet_size() {
        use core::mem::size_of;
        assert_eq!(size_of::<PacketUser>(), 32);
        assert_eq!(size_of::<PacketSignal>(), 32);
        assert_eq!(size_of::<PacketGuestBell>(), 32);
        assert_eq!(size_of::<PacketGuestMem>(), 32);
        assert_eq!(size_of::<PacketGuestIo>(), 32);
        assert_eq!(size_of::<PacketGuestVcpu>(), 32);
        assert_eq!(size_of::<PacketInterrupt>(), 32);
    }

    fn test_encdec(data: PayloadRepr) {
        let repr = PortPacketRepr {
            key: 0,
            status: ZxError::OK as i32,
            data: data.clone(),
        };
        let packet = PortPacket {
            key: 0,
            type_: data.type_() as u32,
            status: ZxError::OK as i32,
            data: data.encode(),
        };
        assert_eq!(repr, packet.decode().unwrap());
        assert_eq!(repr.clone(), PortPacket::from(repr).decode().unwrap());
    }

    #[test]
    fn user() {
        let user = PacketUser::default();
        test_encdec(PayloadRepr::User(user));
    }

    #[test]
    fn signal() {
        let data = PacketSignal {
            trigger: Signal::READABLE,
            observed: Signal::WRITABLE,
            count: 1,
            timestamp: 0,
            _reserved1: 0,
        };
        test_encdec(PayloadRepr::Signal(data));
        let packet = PortPacket {
            key: 1,
            type_: PacketType::SignalOne as u32,
            status: ZxError::OK as i32,
            data: Payload { signal: data },
        };
        let packet_ = PortPacket {
            key: 1,
            type_: PacketType::SignalRep as u32,
            status: ZxError::OK as i32,
            data: Payload { signal: data },
        };
        assert_eq!(packet.decode().unwrap(), packet_.decode().unwrap());
    }

    #[test]
    fn guest_bell() {
        let guest_bell = PacketGuestBell::default();
        assert_eq!(guest_bell.addr, 0);
        test_encdec(PayloadRepr::GuestBell(guest_bell));
    }

    #[test]
    fn guest_mem() {
        let guest_mem = PacketGuestMem::default();
        assert_eq!(guest_mem.addr, 0);
        test_encdec(PayloadRepr::GuestMem(guest_mem));
    }

    #[test]
    fn guest_io() {
        let guest_io = PacketGuestIo::default();
        assert_eq!(guest_io.port, 0);
        assert_eq!(guest_io.input, false);
        test_encdec(PayloadRepr::GuestIo(guest_io));
    }

    #[test]
    fn guest_vcpu() {
        let interrupt = PacketGuestVcpuInterrupt { mask: 0, vector: 0 };
        let guest_vcpu1 = PacketGuestVcpu {
            data: PacketGuestVcpuData { interrupt },
            type_: PacketGuestVcpuType::VcpuInterrupt as u8,
            _padding1: Default::default(),
            _reserved: 0,
        };
        let startup = PacketGuestVcpuStartup { id: 0, entry: 0 };
        let guest_vcpu2 = PacketGuestVcpu {
            data: PacketGuestVcpuData { startup },
            type_: PacketGuestVcpuType::VcpuStartup as u8,
            _padding1: Default::default(),
            _reserved: 0,
        };
        test_encdec(PayloadRepr::GuestVcpu(guest_vcpu1));
        test_encdec(PayloadRepr::GuestVcpu(guest_vcpu2));

        let packet = PortPacket {
            key: 1,
            type_: PacketType::GuestVcpu as u32,
            status: ZxError::OK as i32,
            data: Payload {
                guest_vcpu: guest_vcpu2,
            },
        };
        assert_eq!(
            format!("{:?}", packet),
            "PortPacket { key: 1, type_: GuestVcpu, status: 0, data: GuestVcpu(PacketGuestVcpu { data: PacketGuestVcpuStartup { id: 0, entry: 0 }, type_: 1, _padding1: [0, 0, 0, 0, 0, 0, 0], _reserved: 0 }) }"
        );

        assert!(!guest_vcpu1.eq(&guest_vcpu2));
        let guest_vcpu3 = PacketGuestVcpu {
            data: PacketGuestVcpuData {
                startup: PacketGuestVcpuStartup { id: 0, entry: 1 },
            },
            type_: PacketGuestVcpuType::VcpuStartup as u8,
            _padding1: Default::default(),
            _reserved: 0,
        };
        assert!(!guest_vcpu2.eq(&guest_vcpu3));
    }

    #[test]
    fn interrupt() {
        let interrupt = PacketInterrupt {
            timestamp: 12345,
            _reserved0: 0,
            _reserved1: 0,
            _reserved2: 0,
        };
        test_encdec(PayloadRepr::Interrupt(interrupt));
    }

    /// `ZX_PKT_TYPE_PAGE_REQUEST` is a type a process may write, and `Payload`
    /// has no member for it. It used to reach an `unimplemented!()`.
    #[test]
    fn page_request() {
        let data: PacketUser = [0u8; 32];
        assert_eq!(
            PayloadRepr::decode(PacketType::PageRequest, &Payload { user: data }).err(),
            Some(ZxError::NOT_SUPPORTED)
        );
    }
}
