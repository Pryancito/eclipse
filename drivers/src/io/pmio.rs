// 端口映射 I/O。
//! Port-mapped I/O.

use super::Io;
use core::{arch::asm, marker::PhantomData};

// 端口映射 I/O。
/// Port-mapped I/O.
#[derive(Copy, Clone)]
pub struct Pmio<T> {
    port: u16,
    _phantom: PhantomData<T>,
}

impl<T> Pmio<T> {
    // 映射指定端口进行外设访问。
    /// Maps a given port to assess device.
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            _phantom: PhantomData,
        }
    }

    /// The port this unit was built for.
    ///
    /// `#[cfg(test)]`, and it exists so a register map can be checked without a
    /// serial port: `read`/`write` below are `in`/`out`, which fault outside
    /// ring 0, so the ports themselves are the only part of a PMIO driver a host
    /// test can inspect -- and in a PMIO map the port number IS the register,
    /// exactly as the field offset is in an MMIO one.
    #[cfg(test)]
    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

// 逐字节端口映射读写。
/// Read/Write for byte PMIO.
impl Io for Pmio<u8> {
    type Value = u8;

    // 读。
    /// Read.
    #[inline(always)]
    fn read(&self) -> u8 {
        let value: u8;
        unsafe {
            asm!("in al, dx", out("al") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    // 写。
    /// Write.
    #[inline(always)]
    fn write(&mut self, value: u8) {
        unsafe {
            asm!("out dx, al", in("al") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        }
    }
}

// 逐字端口映射读写。
/// Read/Write for word PMIO.
impl Io for Pmio<u16> {
    type Value = u16;

    // 读。
    /// Read.
    #[inline(always)]
    fn read(&self) -> u16 {
        let value: u16;
        unsafe {
            asm!("in ax, dx", out("ax") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    // 写。
    /// Write.
    #[inline(always)]
    fn write(&mut self, value: u16) {
        unsafe {
            asm!("out dx, ax", in("ax") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        }
    }
}

// 逐双字端口映射读写。
/// Read/Write for double-word PMIO.
impl Io for Pmio<u32> {
    type Value = u32;

    // 读。
    /// Read.
    #[inline(always)]
    fn read(&self) -> u32 {
        let value: u32;
        unsafe {
            asm!("in eax, dx", out("eax") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    // 写。
    /// Write.
    #[inline(always)]
    fn write(&mut self, value: u32) {
        unsafe {
            asm!("out dx, eax", in("eax") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PMIO unit IS its port number -- `read`/`write` are `in`/`out`, which
    /// fault outside ring 0, so the port is the only thing about a PMIO
    /// register map a host test can check, and in such a map the port number
    /// is the register exactly as a field offset is in an MMIO one.
    #[test]
    fn a_unit_remembers_the_port_it_was_built_for() {
        assert_eq!(Pmio::<u8>::new(0x3f8).port(), 0x3f8);
        assert_eq!(Pmio::<u16>::new(0xcf8).port(), 0xcf8);
        assert_eq!(Pmio::<u32>::new(0xcfc).port(), 0xcfc);
    }

    /// The whole 16-bit port space is addressable, including the ends: a
    /// `u8`-wide port field, or a truncation to one, would lose the high half
    /// of the space and silently talk to the wrong device.
    #[test]
    fn the_whole_sixteen_bit_port_space_is_addressable() {
        assert_eq!(Pmio::<u8>::new(0).port(), 0);
        assert_eq!(Pmio::<u8>::new(u16::MAX).port(), u16::MAX);
        assert_eq!(Pmio::<u32>::new(0xffff).port(), 0xffff);
    }

    /// `new` is `const`, because a register map is a `const` table of these;
    /// and the unit is `Copy`, which is what lets a driver hand one out by
    /// value without giving up its own.
    #[test]
    fn a_unit_is_const_constructible_and_copies_by_value() {
        const COM1: Pmio<u8> = Pmio::new(0x3f8);
        let copy = COM1;
        assert_eq!(copy.port(), COM1.port());
        // A `PhantomData` type parameter costs nothing: the unit is its port.
        assert_eq!(
            core::mem::size_of::<Pmio<u32>>(),
            core::mem::size_of::<u16>()
        );
    }
}
